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
    // Every target, including clone followers with independent source modes.
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
            tracing::warn!(target: "oink_arrangement", operation = "buffer_sizes",
                win32_error = result.0, display_paths = np, display_modes = nm,
                query_flags = flags.0, "native arrangement read failed");
            return Err(DisplayError::Platform(format!(
                "GetDisplayConfigBufferSizes: {result:?}"
            )));
        }
        if np > 256 || nm > 1024 {
            tracing::warn!(target: "oink_arrangement", operation = "buffer_bounds",
                display_paths = np, display_modes = nm, "native arrangement read failed");
            return Err(ArrangementError::Invalid.into());
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        // SAFETY: initialized buffers have exactly the declared element counts.
        let result =
            unsafe { QueryDisplayConfig(flags, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None) };
        if result == ERROR_INSUFFICIENT_BUFFER {
            tracing::warn!(target: "oink_arrangement", operation = "query",
                win32_error = result.0, display_paths = np, display_modes = nm,
                query_flags = flags.0, "native arrangement read failed");
            continue;
        }
        if result != ERROR_SUCCESS {
            tracing::warn!(target: "oink_arrangement", operation = "query",
                win32_error = result.0, display_paths = np, display_modes = nm,
                query_flags = flags.0, "native arrangement read failed");
            return Err(DisplayError::Platform(format!("QueryDisplayConfig: {result:?}")));
        }
        paths.truncate(np as usize);
        modes.truncate(nm as usize);
        return Ok((paths, modes));
    }
    Err(ArrangementError::Changed.into())
}

fn target_name(path: &DISPLAYCONFIG_PATH_INFO, display_paths: usize) -> Result<String, DisplayError> {
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
        tracing::warn!(target: "oink_arrangement", operation = "target_name",
            win32_error = result, display_paths, "native arrangement read failed");
        return Err(DisplayError::Platform(format!("DisplayConfigGetDeviceInfo: {result}")));
    }
    let end = target
        .monitorDevicePath
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(target.monitorDevicePath.len());
    let key = String::from_utf16_lossy(&target.monitorDevicePath[..end]);
    if key.is_empty() {
        tracing::warn!(target: "oink_arrangement", operation = "target_key",
            display_paths, "native arrangement read failed");
        return Err(ArrangementError::Invalid.into());
    }
    Ok(key)
}

fn read() -> Result<Snapshot, DisplayError> {
    let (paths, modes) = query()?;
    let visible = super::enumerate::displays().inspect_err(|error| {
        tracing::warn!(target: "oink_arrangement", operation = "enumerate",
            display_paths = paths.len(), display_modes = modes.len(), %error,
            "native arrangement read failed");
    })?;
    let names = paths
        .iter()
        .map(|path| target_name(path, paths.len()))
        .collect::<Result<Vec<_>, _>>()?;
    from_parts(paths, modes, visible, names)
}

fn from_parts(
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
    visible: Vec<oink_foundation::display::DisplayInfo>,
    names: Vec<String>,
) -> Result<Snapshot, DisplayError> {
    let changed = |operation: &'static str| {
        tracing::warn!(target: "oink_arrangement", operation, display_paths = paths.len(),
            display_modes = modes.len(), visible_displays = visible.len(),
            "native arrangement read failed: configuration changed");
        ArrangementError::Changed
    };
    let read_source = |path: &DISPLAYCONFIG_PATH_INFO| -> Result<_, DisplayError> {
        let index = source_index(path) as usize;
        let mode = modes.get(index).ok_or_else(|| changed("source_index"))?;
        if mode.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
            || mode.id != path.sourceInfo.id
            || mode.adapterId.HighPart != path.sourceInfo.adapterId.HighPart
            || mode.adapterId.LowPart != path.sourceInfo.adapterId.LowPart
        {
            return Err(changed("source_identity").into());
        }
        // SAFETY: infoType was checked before reading the tagged union.
        let source = unsafe { mode.Anonymous.sourceMode };
        Ok((index, mode, source))
    };
    // CCD virtual clones can have distinct source IDs and mode-array indices.
    // Equal source rectangles on the same adapter describe one cloned desktop;
    // grouping by index would leave its non-GDI-visible followers without a leader.
    // Keep each path's source index below so origin writes move all members.
    // This independent-index form was observed on Windows 11 build 26200.
    // https://learn.microsoft.com/en-us/windows-hardware/drivers/display/ccd-example-code
    let clone_group = |path: &DISPLAYCONFIG_PATH_INFO, source: &DISPLAYCONFIG_SOURCE_MODE| {
        (
            (path.sourceInfo.adapterId.HighPart, path.sourceInfo.adapterId.LowPart),
            (source.position.x, source.position.y),
            (source.width, source.height),
        )
    };
    let mut leaders = BTreeMap::new();
    for (path, name) in paths.iter().zip(&names) {
        let (_, _, source) = read_source(path)?;
        if visible.iter().any(|d| d.key == *name) {
            leaders.insert(clone_group(path, &source), name.clone());
        }
    }
    let mut configuration = NativeConfiguration { displays: Vec::new() };
    let mut sources = BTreeMap::new();
    for (path, name) in paths.iter().zip(names) {
        let (index, mode, source) = read_source(path)?;
        let leader = leaders
            .get(&clone_group(path, &source))
            .ok_or_else(|| changed("visible_source"))?;
        let info = visible
            .iter()
            .find(|d| d.key == *leader)
            .ok_or_else(|| changed("visible_source"))?;
        let bounds = Rect::new(
            source.position.x.into(),
            source.position.y.into(),
            source.width.into(),
            source.height.into(),
        );
        if info.bounds != bounds {
            tracing::warn!(target: "oink_arrangement", operation = "bounds",
                display_paths = paths.len(), display_modes = modes.len(),
                source_index = index, gdi_bounds = ?info.bounds, source_bounds = ?bounds,
                "native arrangement read failed: desktop bounds differ");
            return Err(DisplayError::Unsupported);
        }
        let is_leader = name == *leader;
        sources.insert(name.clone(), index);
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
    // A virtual clone follower can reference another source mode. The native
    // plan gives every follower its leader's origin; update them all while
    // preserving the complete original paths and target/desktop modes.
    for display in &target.displays {
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
    use oink_foundation::{Point, display::DisplayInfo};
    use windows::Win32::Foundation::LUID;

    fn source_mode(id: u32, x: i32, y: i32) -> DISPLAYCONFIG_MODE_INFO {
        DISPLAYCONFIG_MODE_INFO {
            infoType: DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
            id,
            adapterId: LUID {
                LowPart: 7,
                HighPart: 0,
            },
            Anonymous: DISPLAYCONFIG_MODE_INFO_0 {
                sourceMode: DISPLAYCONFIG_SOURCE_MODE {
                    width: 3840,
                    height: 2160,
                    pixelFormat: DISPLAYCONFIG_PIXELFORMAT_32BPP,
                    position: windows::Win32::Foundation::POINTL { x, y },
                },
            },
        }
    }

    fn virtual_path(source: u32, index: u32, target: u32, target_index: u32) -> DISPLAYCONFIG_PATH_INFO {
        DISPLAYCONFIG_PATH_INFO {
            flags: DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE | 1,
            sourceInfo: DISPLAYCONFIG_PATH_SOURCE_INFO {
                adapterId: LUID {
                    LowPart: 7,
                    HighPart: 0,
                },
                id: source,
                Anonymous: DISPLAYCONFIG_PATH_SOURCE_INFO_0 {
                    modeInfoIdx: (index << 16) | 0xffff,
                },
                ..Default::default()
            },
            targetInfo: DISPLAYCONFIG_PATH_TARGET_INFO {
                adapterId: LUID {
                    LowPart: 7,
                    HighPart: 0,
                },
                id: target,
                Anonymous: DISPLAYCONFIG_PATH_TARGET_INFO_0 {
                    modeInfoIdx: (target_index << 16) | (target_index + 2),
                },
                targetAvailable: windows::core::BOOL(1),
                rotation: DISPLAYCONFIG_ROTATION_IDENTITY,
                scaling: DISPLAYCONFIG_SCALING_IDENTITY,
                ..Default::default()
            },
        }
    }

    fn visible(key: &str, x: f64, primary: bool) -> DisplayInfo {
        DisplayInfo {
            key: key.into(),
            name: key.into(),
            bounds: Rect::new(x, 0.0, 3840.0, 2160.0),
            pixel_width: 3840,
            pixel_height: 2160,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: primary,
            is_builtin: false,
            is_virtual: false,
        }
    }

    fn virtual_clone_fixture() -> (Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>) {
        let paths = vec![virtual_path(0, 1, 101, 0), virtual_path(1, 4, 102, 3)];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); 6];
        for (target_index, target_id) in [(0, 101), (3, 102)] {
            modes[target_index].infoType = DISPLAYCONFIG_MODE_INFO_TYPE_TARGET;
            modes[target_index].id = target_id;
            modes[target_index].adapterId = LUID {
                LowPart: 7,
                HighPart: 0,
            };
            modes[target_index + 2].infoType = DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE;
            modes[target_index + 2].id = target_id;
            modes[target_index + 2].adapterId = LUID {
                LowPart: 7,
                HighPart: 0,
            };
        }
        modes[1] = source_mode(0, 0, 0);
        modes[4] = source_mode(1, 0, 0);
        (paths, modes)
    }

    #[test]
    fn virtual_clone_paths_with_distinct_source_modes_are_one_screen() {
        // Virtual-aware CCD has two active paths (flags 9), source IDs 0/1,
        // indices 1/4, equal desktop rectangles, and one GDI-visible monitor.
        let (paths, modes) = virtual_clone_fixture();
        let snapshot = from_parts(
            paths,
            modes,
            vec![visible("leader", 0.0, true)],
            vec!["leader".into(), "mirror".into()],
        )
        .unwrap();
        assert_eq!(snapshot.paths.len(), 2);
        assert_eq!(snapshot.configuration.displays.len(), 2);
        assert_eq!(snapshot.configuration.sources().count(), 1);
        let mirror = snapshot
            .configuration
            .displays
            .iter()
            .find(|d| d.key == "mirror")
            .unwrap();
        assert_eq!(mirror.mirror_of.as_deref(), Some("leader"));
        assert!(!mirror.primary);
        assert_eq!(snapshot.sources.get("leader"), Some(&1));
        assert_eq!(snapshot.sources.get("mirror"), Some(&4));
    }

    #[test]
    fn virtual_clone_source_patch_moves_every_member_and_preserves_modes() {
        let (mut paths, mut modes) = virtual_clone_fixture();
        paths.push(virtual_path(2, 7, 103, 6));
        modes[1] = source_mode(0, 3840, 0);
        modes[4] = source_mode(1, 3840, 0);
        modes.resize(9, DISPLAYCONFIG_MODE_INFO::default());
        modes[7] = source_mode(2, 0, 0);
        for index in [0, 3, 6] {
            modes[index].infoType = DISPLAYCONFIG_MODE_INFO_TYPE_TARGET;
            modes[index].id = 101 + (index / 3) as u32;
            modes[index].adapterId = LUID {
                LowPart: 7,
                HighPart: 0,
            };
            modes[index].Anonymous.targetMode.targetVideoSignalInfo.pixelRate = 100_000_000 + index as u64;
        }
        for index in [2, 5, 8] {
            modes[index].infoType = DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE;
            modes[index].id = 101 + (index / 3) as u32;
            modes[index].adapterId = LUID {
                LowPart: 7,
                HighPart: 0,
            };
            modes[index].Anonymous.desktopImageInfo.PathSourceSize.x = 3840;
            modes[index].Anonymous.desktopImageInfo.PathSourceSize.y = 2160;
            modes[index].Anonymous.desktopImageInfo.DesktopImageClip.right = 3840;
            modes[index].Anonymous.desktopImageInfo.DesktopImageClip.bottom = 2160;
        }
        let native = NativeConfiguration {
            displays: vec![
                NativeDisplay {
                    key: "leader".into(),
                    bounds: Rect::new(3840.0, 0.0, 3840.0, 2160.0),
                    primary: false,
                    mirror_of: None,
                    mode: "leader-mode".into(),
                },
                NativeDisplay {
                    key: "mirror".into(),
                    bounds: Rect::new(3840.0, 0.0, 3840.0, 2160.0),
                    primary: false,
                    mirror_of: Some("leader".into()),
                    mode: "mirror-mode".into(),
                },
                NativeDisplay {
                    key: "primary".into(),
                    bounds: Rect::new(0.0, 0.0, 3840.0, 2160.0),
                    primary: true,
                    mirror_of: None,
                    mode: "primary-mode".into(),
                },
            ],
        };
        let target = native
            .with_origins(&[
                NativeOrigin {
                    key: "leader".into(),
                    origin: Point::new(0.0, 2160.0),
                },
                NativeOrigin {
                    key: "primary".into(),
                    origin: Point::default(),
                },
            ])
            .unwrap();
        let sources = BTreeMap::from([("leader".into(), 1), ("mirror".into(), 4), ("primary".into(), 7)]);
        patch_sources(&mut modes, &sources, &target).unwrap();
        for index in [1, 4] {
            // SAFETY: the fixture initializes source modes at these indices.
            let source = unsafe { modes[index].Anonymous.sourceMode };
            assert_eq!((source.position.x, source.position.y), (0, 2160), "source {index}");
            assert_eq!(
                (source.width, source.height, source.pixelFormat),
                (3840, 2160, DISPLAYCONFIG_PIXELFORMAT_32BPP)
            );
        }
        // SAFETY: the fixture initializes the primary source at index 7.
        let primary = unsafe { modes[7].Anonymous.sourceMode };
        assert_eq!((primary.position.x, primary.position.y), (0, 0));
        for index in [0, 3, 6] {
            assert_eq!(
                // SAFETY: these are the target-mode entries initialized above.
                unsafe { modes[index].Anonymous.targetMode.targetVideoSignalInfo.pixelRate },
                100_000_000 + index as u64
            );
        }
        for index in [2, 5, 8] {
            // SAFETY: these are the desktop-image entries initialized above.
            let image = unsafe { modes[index].Anonymous.desktopImageInfo };
            assert_eq!((image.PathSourceSize.x, image.PathSourceSize.y), (3840, 2160));
            assert_eq!(
                (image.DesktopImageClip.right, image.DesktopImageClip.bottom),
                (3840, 2160)
            );
        }
        assert_eq!(paths.len(), 3);
        assert_eq!((source_index(&paths[0]), source_index(&paths[1])), (1, 4));
        assert_eq!((modes[1].id, modes[4].id), (0, 1));
        assert_eq!((paths[0].flags, paths[1].flags), (9, 9));
        assert_eq!(
            target
                .displays
                .iter()
                .find(|d| d.key == "mirror")
                .unwrap()
                .mirror_of
                .as_deref(),
            Some("leader")
        );
    }

    #[test]
    fn equal_rectangles_on_different_adapters_are_separate_sources() {
        let (mut paths, mut modes) = virtual_clone_fixture();
        paths[1].sourceInfo.adapterId.LowPart = 8;
        paths[1].targetInfo.adapterId.LowPart = 8;
        for mode in &mut modes[3..] {
            mode.adapterId.LowPart = 8;
        }
        let snapshot = from_parts(
            paths,
            modes,
            vec![visible("leader", 0.0, true), visible("other", 0.0, false)],
            vec!["leader".into(), "other".into()],
        )
        .unwrap();
        assert_eq!(snapshot.configuration.sources().count(), 2);
    }

    #[test]
    fn unmatched_source_with_different_geometry_still_fails() {
        for different_size in [false, true] {
            let (paths, mut modes) = virtual_clone_fixture();
            if different_size {
                modes[4].Anonymous.sourceMode.width = 1920;
            } else {
                modes[4].Anonymous.sourceMode.position.x = 1;
            }
            assert!(matches!(
                from_parts(
                    paths,
                    modes,
                    vec![visible("leader", 0.0, true)],
                    vec!["leader".into(), "other".into()]
                ),
                Err(DisplayError::Arrangement(ArrangementError::Changed))
            ));
        }
    }

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
