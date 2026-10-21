use crate::{
    DisplayError,
    arrangement::{ArrangementError, NativeConfiguration, NativeDisplay, NativeOrigin},
};
use oink_foundation::Rect;
use std::collections::BTreeMap;
use windows::Win32::{
    Devices::Display::*,
    Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS},
    Graphics::Gdi::DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE,
};

struct Snapshot {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
    configuration: NativeConfiguration,
    sources: BTreeMap<String, usize>,
}

fn source_index(path: &DISPLAYCONFIG_PATH_INFO) -> u32 {
    // SAFETY: both union representations contain the same initialized u32. With
    // virtual awareness the high 16 bits are sourceModeInfoIdx, not the full word.
    let value = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
    if path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0 {
        value >> 16
    } else {
        value
    }
}

fn query() -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), DisplayError> {
    let flags = QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE;
    for _ in 0..4 {
        let (mut np, mut nm) = (0, 0);
        // SAFETY: output pointers refer to live local counts.
        let result = unsafe { GetDisplayConfigBufferSizes(flags, &mut np, &mut nm) };
        if result != ERROR_SUCCESS {
            return Err(DisplayError::Platform(format!(
                "GetDisplayConfigBufferSizes: {result:?}"
            )));
        }
        if np > 256 || nm > 1024 {
            return Err(ArrangementError::Invalid.into());
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        // SAFETY: initialized buffers have exactly the declared element counts.
        let result =
            unsafe { QueryDisplayConfig(flags, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None) };
        if result == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if result != ERROR_SUCCESS {
            return Err(DisplayError::Platform(format!("QueryDisplayConfig: {result:?}")));
        }
        paths.truncate(np as usize);
        modes.truncate(nm as usize);
        return Ok((paths, modes));
    }
    Err(ArrangementError::Changed.into())
}

fn target_name(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String, DisplayError> {
    let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: header size and type describe the initialized enclosing structure.
    let result = unsafe { DisplayConfigGetDeviceInfo(&mut target.header) };
    if result != 0 {
        return Err(DisplayError::Platform(format!("DisplayConfigGetDeviceInfo: {result}")));
    }
    let end = target
        .monitorDevicePath
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(target.monitorDevicePath.len());
    let key = String::from_utf16_lossy(&target.monitorDevicePath[..end]);
    if key.is_empty() {
        return Err(ArrangementError::Invalid.into());
    }
    Ok(key)
}

fn read() -> Result<Snapshot, DisplayError> {
    let (paths, modes) = query()?;
    let visible = super::enumerate::displays()?;
    let names = paths.iter().map(target_name).collect::<Result<Vec<_>, _>>()?;
    let mut leaders = BTreeMap::new();
    for (path, name) in paths.iter().zip(&names) {
        let index = source_index(path) as usize;
        if visible.iter().any(|d| d.key == *name) {
            leaders.insert(index, name.clone());
        }
    }
    let mut configuration = NativeConfiguration { displays: Vec::new() };
    let mut sources = BTreeMap::new();
    for (path, name) in paths.iter().zip(names) {
        let index = source_index(path) as usize;
        let mode = modes.get(index).ok_or(ArrangementError::Changed)?;
        if mode.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
            || mode.id != path.sourceInfo.id
            || mode.adapterId.HighPart != path.sourceInfo.adapterId.HighPart
            || mode.adapterId.LowPart != path.sourceInfo.adapterId.LowPart
        {
            return Err(ArrangementError::Changed.into());
        }
        // SAFETY: infoType was checked before reading the tagged union.
        let source = unsafe { mode.Anonymous.sourceMode };
        let leader = leaders.get(&index).ok_or(ArrangementError::Changed)?;
        let info = visible
            .iter()
            .find(|d| d.key == *leader)
            .ok_or(ArrangementError::Changed)?;
        let bounds = Rect::new(
            source.position.x.into(),
            source.position.y.into(),
            source.width.into(),
            source.height.into(),
        );
        if info.bounds != bounds {
            return Err(DisplayError::Unsupported);
        }
        let is_leader = name == *leader;
        if is_leader {
            sources.insert(name.clone(), index);
        }
        configuration.displays.push(NativeDisplay {
            key: name,
            bounds,
            primary: is_leader && info.is_primary,
            mirror_of: (!is_leader).then(|| leader.clone()),
            mode: format!(
                "{}:{}:{}:{}:{}:{}:{}:{}:{}",
                mode.adapterId.HighPart,
                mode.adapterId.LowPart,
                mode.id,
                path.targetInfo.id,
                source.pixelFormat.0,
                path.targetInfo.rotation.0,
                path.targetInfo.scaling.0,
                path.targetInfo.refreshRate.Numerator,
                path.targetInfo.refreshRate.Denominator
            ),
        });
    }
    configuration.canonicalize();
    Ok(Snapshot {
        paths,
        modes,
        configuration,
        sources,
    })
}

pub(super) fn configuration() -> Result<NativeConfiguration, DisplayError> {
    read().map(|s| s.configuration)
}

fn patch_sources(
    modes: &mut [DISPLAYCONFIG_MODE_INFO],
    sources: &BTreeMap<String, usize>,
    target: &NativeConfiguration,
) -> Result<(), DisplayError> {
    for display in target.sources() {
        let index = *sources.get(&display.key).ok_or(ArrangementError::Changed)?;
        let mode = modes.get_mut(index).ok_or(ArrangementError::Changed)?;
        if mode.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            return Err(ArrangementError::Changed.into());
        }
        // Only integer origin fields change; sizes/pixel formats and every
        // target/desktop mode remain intact. Writing a union field is safe.
        mode.Anonymous.sourceMode.position.x = display.bounds.x as i32;
        mode.Anonymous.sourceMode.position.y = display.bounds.y as i32;
    }
    Ok(())
}

const VALIDATE: SET_DISPLAY_CONFIG_FLAGS =
    SET_DISPLAY_CONFIG_FLAGS(SDC_VALIDATE.0 | SDC_USE_SUPPLIED_DISPLAY_CONFIG.0 | SDC_VIRTUAL_MODE_AWARE.0);
const APPLY: SET_DISPLAY_CONFIG_FLAGS =
    SET_DISPLAY_CONFIG_FLAGS(SDC_APPLY.0 | SDC_USE_SUPPLIED_DISPLAY_CONFIG.0 | SDC_VIRTUAL_MODE_AWARE.0);

pub(super) fn apply(
    expected: &NativeConfiguration,
    origins: &[NativeOrigin],
) -> Result<NativeConfiguration, DisplayError> {
    let mut current = read()?;
    if &current.configuration != expected {
        return Err(ArrangementError::Changed.into());
    }
    let target = expected.with_origins(origins)?;
    if target == *expected {
        return Ok(target);
    }
    patch_sources(&mut current.modes, &current.sources, &target)?;
    // Why this shape: complete supplied paths preserve clone membership and
    // modes. ALLOW_CHANGES could silently change resolution; SAVE_TO_DATABASE
    // would change persistence scope. Query/apply use the same virtual indices.
    for flags in [VALIDATE, APPLY] {
        // SAFETY: initialized path indices and modes come from the same query;
        // patch_sources validates source indices before writing origins.
        let result = unsafe { SetDisplayConfig(Some(&current.paths), Some(&current.modes), flags) };
        if result != 0 {
            return Err(DisplayError::Platform(format!("SetDisplayConfig: {result}")));
        }
    }
    let observed = configuration()?;
    if observed.displays.len() != expected.displays.len()
        || observed
            .displays
            .iter()
            .zip(&expected.displays)
            .any(|(a, b)| a.key != b.key || a.mode != b.mode || a.primary != b.primary || a.mirror_of != b.mirror_of)
    {
        return Err(ArrangementError::Changed.into());
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_patch_preserves_size_and_indices_and_is_session_scoped() {
        let mut mode = DISPLAYCONFIG_MODE_INFO {
            infoType: DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
            id: 7,
            ..Default::default()
        };
        mode.Anonymous.sourceMode = DISPLAYCONFIG_SOURCE_MODE {
            width: 1000,
            height: 700,
            ..Default::default()
        };
        let mut modes = vec![mode];
        let target = NativeConfiguration {
            displays: vec![NativeDisplay {
                key: "source".into(),
                bounds: Rect::new(-1000.0, 20.0, 1000.0, 700.0),
                primary: false,
                mirror_of: None,
                mode: "untouched".into(),
            }],
        };
        patch_sources(&mut modes, &BTreeMap::from([("source".into(), 0)]), &target).unwrap();
        // SAFETY: the fixture explicitly initializes a source-mode tagged union.
        let source = unsafe { modes[0].Anonymous.sourceMode };
        assert_eq!(
            (source.position.x, source.position.y, source.width, source.height),
            (-1000, 20, 1000, 700)
        );
        assert_eq!(modes[0].id, 7);
        assert_eq!(APPLY.0 & (SDC_ALLOW_CHANGES.0 | SDC_SAVE_TO_DATABASE.0), 0);
        assert_eq!(VALIDATE.0 & SDC_APPLY.0, 0);
        let path = DISPLAYCONFIG_PATH_INFO {
            flags: DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE,
            sourceInfo: DISPLAYCONFIG_PATH_SOURCE_INFO {
                Anonymous: DISPLAYCONFIG_PATH_SOURCE_INFO_0 {
                    modeInfoIdx: (23 << 16) | 5,
                },
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(source_index(&path), 23);
    }
}
