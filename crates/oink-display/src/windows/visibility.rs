//! Session-only target removal and exact-mode reconnection.
use super::arrangement::*;
use crate::{
    DisplayError, DisplayInfo,
    arrangement::{NativeConfiguration, NativeOrigin, enabled_targets},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};
use windows::Win32::{
    Devices::Display::*,
    Foundation::{POINTL, RECTL},
};

// Stable values only: mode-array indices belong to one QueryDisplayConfig result.
#[derive(Clone, Serialize, Deserialize)]
struct Packet {
    virtual_mode: bool,
    // width, height, pixel format, x, y.
    source: [i64; 5],
    // Pixel rate; H/V rational pairs; active/total sizes; video standard; scanline.
    signal: [u64; 11],
    // PathSourceSize, DesktopImageRegion, DesktopImageClip, in Win32 field order.
    desktop: Option<[i32; 10]>,
    desktop_source: bool,
    rotation: i32,
    scaling: i32,
    refresh: [u32; 2],
    scanline: i32,
}
fn target_index(p: &DISPLAYCONFIG_PATH_INFO) -> u32 {
    // SAFETY: both views of this initialized union are u32.
    let v = unsafe { p.targetInfo.Anonymous.modeInfoIdx };
    if p.flags & windows::Win32::Graphics::Gdi::DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0 {
        v >> 16
    } else {
        v
    }
}
fn desktop_index(p: &DISPLAYCONFIG_PATH_INFO) -> Option<u32> {
    // SAFETY: both views of this initialized union are u32.
    let v = unsafe { p.targetInfo.Anonymous.modeInfoIdx };
    (p.flags & windows::Win32::Graphics::Gdi::DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0 && v & 0xffff != 0xffff)
        .then_some(v & 0xffff)
}
fn invalid() -> DisplayError {
    crate::arrangement::ArrangementError::Changed.into()
}
impl Packet {
    fn capture(p: &DISPLAYCONFIG_PATH_INFO, modes: &[DISPLAYCONFIG_MODE_INFO]) -> Result<Self, DisplayError> {
        let source = modes.get(source_index(p) as usize).ok_or_else(invalid)?;
        let target = modes.get(target_index(p) as usize).ok_or_else(invalid)?;
        if source.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
            || target.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_TARGET
        {
            return Err(invalid());
        }
        // SAFETY: tags checked before union access; signal's anonymous member is a u32 bitfield.
        let (s, t) = unsafe {
            (
                source.Anonymous.sourceMode,
                target.Anonymous.targetMode.targetVideoSignalInfo,
            )
        };
        let mut desktop_source = false;
        let desktop = desktop_index(p)
            .map(|idx| {
                let m = modes.get(idx as usize).ok_or_else(invalid)?;
                if m.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE {
                    return Err(invalid());
                }
                desktop_source = m.id == p.sourceInfo.id && m.adapterId == p.sourceInfo.adapterId;
                if !desktop_source && (m.id != p.targetInfo.id || m.adapterId != p.targetInfo.adapterId) {
                    return Err(invalid());
                }
                // SAFETY: desktop-image tag was checked.
                let d = unsafe { m.Anonymous.desktopImageInfo };
                Ok([
                    d.PathSourceSize.x,
                    d.PathSourceSize.y,
                    d.DesktopImageRegion.left,
                    d.DesktopImageRegion.top,
                    d.DesktopImageRegion.right,
                    d.DesktopImageRegion.bottom,
                    d.DesktopImageClip.left,
                    d.DesktopImageClip.top,
                    d.DesktopImageClip.right,
                    d.DesktopImageClip.bottom,
                ])
            })
            .transpose()?;
        // SAFETY: both signal-union representations are initialized u32 bitfields.
        let standard = unsafe { t.Anonymous.videoStandard };
        Ok(Self {
            virtual_mode: p.flags & windows::Win32::Graphics::Gdi::DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0,
            source: [
                s.width.into(),
                s.height.into(),
                s.pixelFormat.0.into(),
                s.position.x.into(),
                s.position.y.into(),
            ],
            signal: [
                t.pixelRate,
                t.hSyncFreq.Numerator.into(),
                t.hSyncFreq.Denominator.into(),
                t.vSyncFreq.Numerator.into(),
                t.vSyncFreq.Denominator.into(),
                t.activeSize.cx.into(),
                t.activeSize.cy.into(),
                t.totalSize.cx.into(),
                t.totalSize.cy.into(),
                standard.into(),
                t.scanLineOrdering.0 as u64,
            ],
            desktop,
            desktop_source,
            rotation: p.targetInfo.rotation.0,
            scaling: p.targetInfo.scaling.0,
            refresh: [p.targetInfo.refreshRate.Numerator, p.targetInfo.refreshRate.Denominator],
            scanline: p.targetInfo.scanLineOrdering.0,
        })
    }
    fn append(
        &self,
        mut p: DISPLAYCONFIG_PATH_INFO,
        modes: &mut Vec<DISPLAYCONFIG_MODE_INFO>,
    ) -> Result<DISPLAYCONFIG_PATH_INFO, DisplayError> {
        let start = modes.len() as u32;
        if start > 1020 {
            return Err(invalid());
        }
        let mut src = DISPLAYCONFIG_MODE_INFO {
            infoType: DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
            id: p.sourceInfo.id,
            adapterId: p.sourceInfo.adapterId,
            ..Default::default()
        };
        src.Anonymous.sourceMode = DISPLAYCONFIG_SOURCE_MODE {
            width: self.source[0] as u32,
            height: self.source[1] as u32,
            pixelFormat: DISPLAYCONFIG_PIXELFORMAT(self.source[2] as i32),
            position: POINTL {
                x: self.source[3] as i32,
                y: self.source[4] as i32,
            },
        };
        let t = &self.signal;
        let mut target = DISPLAYCONFIG_MODE_INFO {
            infoType: DISPLAYCONFIG_MODE_INFO_TYPE_TARGET,
            id: p.targetInfo.id,
            adapterId: p.targetInfo.adapterId,
            ..Default::default()
        };
        target.Anonymous.targetMode = DISPLAYCONFIG_TARGET_MODE {
            targetVideoSignalInfo: DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
                pixelRate: t[0],
                hSyncFreq: DISPLAYCONFIG_RATIONAL {
                    Numerator: t[1] as u32,
                    Denominator: t[2] as u32,
                },
                vSyncFreq: DISPLAYCONFIG_RATIONAL {
                    Numerator: t[3] as u32,
                    Denominator: t[4] as u32,
                },
                activeSize: DISPLAYCONFIG_2DREGION {
                    cx: t[5] as u32,
                    cy: t[6] as u32,
                },
                totalSize: DISPLAYCONFIG_2DREGION {
                    cx: t[7] as u32,
                    cy: t[8] as u32,
                },
                Anonymous: DISPLAYCONFIG_VIDEO_SIGNAL_INFO_0 {
                    videoStandard: t[9] as u32,
                },
                scanLineOrdering: DISPLAYCONFIG_SCANLINE_ORDERING(t[10] as i32),
            },
        };
        let source = if let Some(index) = modes.iter().position(|m| {
            m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE && m.id == src.id && m.adapterId == src.adapterId
        }) {
            // SAFETY: both entries have checked SOURCE tags.
            let (a, b) = unsafe { (modes[index].Anonymous.sourceMode, src.Anonymous.sourceMode) };
            if a.width != b.width || a.height != b.height || a.pixelFormat != b.pixelFormat || a.position != b.position
            {
                return Err(invalid());
            }
            index as u32
        } else {
            modes.push(src);
            start
        };
        let target_index = modes.len() as u32;
        modes.push(target);
        let desktop = if let Some(d) = self.desktop {
            let mut m = DISPLAYCONFIG_MODE_INFO {
                infoType: DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE,
                id: if self.desktop_source {
                    p.sourceInfo.id
                } else {
                    p.targetInfo.id
                },
                adapterId: if self.desktop_source {
                    p.sourceInfo.adapterId
                } else {
                    p.targetInfo.adapterId
                },
                ..Default::default()
            };
            m.Anonymous.desktopImageInfo = DISPLAYCONFIG_DESKTOP_IMAGE_INFO {
                PathSourceSize: POINTL { x: d[0], y: d[1] },
                DesktopImageRegion: RECTL {
                    left: d[2],
                    top: d[3],
                    right: d[4],
                    bottom: d[5],
                },
                DesktopImageClip: RECTL {
                    left: d[6],
                    top: d[7],
                    right: d[8],
                    bottom: d[9],
                },
            };
            let index = modes.len() as u32;
            modes.push(m);
            index
        } else {
            0xffff
        };
        p.flags &= !windows::Win32::Graphics::Gdi::DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE;
        if self.virtual_mode {
            p.flags |= windows::Win32::Graphics::Gdi::DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE;
            p.sourceInfo.Anonymous.modeInfoIdx = (source << 16) | 0xffff;
            p.targetInfo.Anonymous.modeInfoIdx = (target_index << 16) | desktop;
        } else {
            if self.desktop.is_some() {
                return Err(DisplayError::Unsupported);
            }
            p.sourceInfo.Anonymous.modeInfoIdx = source;
            p.targetInfo.Anonymous.modeInfoIdx = target_index;
        }
        p.flags |= 1;
        p.targetInfo.rotation = DISPLAYCONFIG_ROTATION(self.rotation);
        p.targetInfo.scaling = DISPLAYCONFIG_SCALING(self.scaling);
        p.targetInfo.refreshRate = DISPLAYCONFIG_RATIONAL {
            Numerator: self.refresh[0],
            Denominator: self.refresh[1],
        };
        p.targetInfo.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING(self.scanline);
        Ok(p)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Saved {
    released: bool,
    native: crate::arrangement::NativeDisplay,
    info: Option<DisplayInfo>,
    source_adapter: (i32, u32),
    source_id: u32,
    packet: Packet,
}
type Store = BTreeMap<String, Saved>;
fn path(root: &Path) -> std::path::PathBuf {
    root.join("windows-disabled-displays.json")
}
fn error(e: impl std::fmt::Display) -> DisplayError {
    DisplayError::Platform(format!("Windows disabled displays: {e}"))
}
fn load(root: &Path) -> Result<Store, DisplayError> {
    use std::io::Read;
    let file = match std::fs::File::open(path(root)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::new()),
        Err(e) => return Err(error(e)),
    };
    let mut bytes = Vec::new();
    file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(error)?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(error("file too large"));
    }
    let store: Store = serde_json::from_slice(&bytes).map_err(error)?;
    if store.len() > 256 {
        return Err(error("too many targets"));
    }
    Ok(store)
}
fn save(root: &Path, store: &Store) -> Result<(), DisplayError> {
    if store.len() > 256 {
        return Err(error("too many targets"));
    }
    std::fs::create_dir_all(root).map_err(error)?;
    oink_foundation::storage::atomic_write(&path(root), &serde_json::to_vec(store).map_err(error)?).map_err(error)
}
fn returned_keys<T>(store: &BTreeMap<String, T>, active: &[String]) -> Vec<String> {
    store.keys().filter(|k| active.contains(k)).cloned().collect()
}
// A known successful release ends Oink's ownership, even if another actor has
// since disabled that target again. Never reclaim an externally disabled path.
fn reconcile_releases(store: &mut Store) -> bool {
    let count = store.len();
    store.retain(|_, saved| !saved.released);
    count != store.len()
}
pub(super) fn returned(root: &Path) -> Result<Vec<String>, DisplayError> {
    let mut store = load(root)?;
    if store.is_empty() {
        return Ok(Vec::new());
    }
    if reconcile_releases(&mut store) {
        save(root, &store)?;
    }
    if store.is_empty() {
        return Ok(Vec::new());
    }
    let active = read()?
        .configuration
        .displays
        .into_iter()
        .map(|d| d.key)
        .collect::<Vec<_>>();
    let mut keys = returned_keys(&store, &active);
    for key in keys.clone() {
        if let Some(leader) = store[&key].native.mirror_of.as_ref() {
            keys.push(leader.clone());
        }
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}
pub(super) fn acknowledge(root: &Path, keys: &[String]) -> Result<(), DisplayError> {
    let mut store = load(root)?;
    store.retain(|key, saved| {
        !keys.contains(key)
            && !saved
                .native
                .mirror_of
                .as_ref()
                .is_some_and(|leader| keys.contains(leader))
    });
    save(root, &store)
}
fn available() -> Result<Vec<DISPLAYCONFIG_PATH_INFO>, DisplayError> {
    Ok(query_with_flags(QDC_ALL_PATHS | QDC_VIRTUAL_MODE_AWARE)?
        .0
        .into_iter()
        .filter(|p| p.targetInfo.targetAvailable.as_bool())
        .collect())
}
type Source = (i32, u32, u32);
fn source_id(p: &DISPLAYCONFIG_PATH_INFO) -> Source {
    (
        p.sourceInfo.adapterId.HighPart,
        p.sourceInfo.adapterId.LowPart,
        p.sourceInfo.id,
    )
}
fn choose_path(
    key: &str,
    preferred: Source,
    assigned: Option<Source>,
    reserved: &std::collections::BTreeSet<Source>,
    candidates: &[(String, DISPLAYCONFIG_PATH_INFO)],
) -> Result<Option<DISPLAYCONFIG_PATH_INFO>, DisplayError> {
    let matching = candidates.iter().filter(|(name, _)| name == key).collect::<Vec<_>>();
    if matching.is_empty() {
        return Ok(None);
    }
    matching
        .into_iter()
        .filter(|(_, p)| assigned.map_or_else(|| !reserved.contains(&source_id(p)), |source| source == source_id(p)))
        .min_by_key(|(_, p)| source_id(p) != preferred)
        .map(|(_, p)| Some(*p))
        .ok_or(DisplayError::Unsupported)
}
fn merge(snapshot: &mut Snapshot, store: &Store, available: &[DISPLAYCONFIG_PATH_INFO]) -> Result<(), DisplayError> {
    let candidates = available
        .iter()
        .filter_map(|p| target_name(p, available.len()).ok().map(|key| (key, *p)))
        .collect::<Vec<_>>();
    let mut reserved = snapshot
        .paths
        .iter()
        .map(source_id)
        .collect::<std::collections::BTreeSet<_>>();
    let mut assigned = BTreeMap::new();
    let mut retained = store.iter().collect::<Vec<_>>();
    retained.sort_by_key(|(key, saved)| (saved.native.mirror_of.is_some(), *key));
    for (key, saved) in retained {
        if saved.released || snapshot.sources.contains_key(key) {
            continue;
        }
        if saved
            .native
            .mirror_of
            .as_ref()
            .is_some_and(|leader| !snapshot.sources.contains_key(leader))
        {
            continue;
        }
        // Source IDs are only a clone-group hint, never a durable identity.
        // Windows may recycle a removed source for another monitor. Choose an
        // unused source from this query's paths for the stable target key, and
        // reuse that choice only for followers that originally shared a source.
        let preferred = (saved.source_adapter.0, saved.source_adapter.1, saved.source_id);
        let Some(path) = choose_path(
            key,
            preferred,
            assigned.get(&preferred).copied(),
            &reserved,
            &candidates,
        )?
        else {
            continue;
        };
        let rebuilt = saved.packet.append(path, &mut snapshot.modes)?;
        let source = source_id(&rebuilt);
        assigned.insert(preferred, source);
        reserved.insert(source);
        snapshot.sources.insert(key.clone(), source_index(&rebuilt) as usize);
        let mut native = saved.native.clone();
        native.primary = false;
        native.mode = format!(
            "{}:{}:{}:{}:{}:{}:{}:{}:{}",
            source.0,
            source.1,
            source.2,
            rebuilt.targetInfo.id,
            saved.packet.source[2],
            saved.packet.rotation,
            saved.packet.scaling,
            saved.packet.refresh[0],
            saved.packet.refresh[1]
        );
        snapshot.paths.push(rebuilt);
        snapshot.configuration.displays.push(native);
    }
    snapshot.configuration.canonicalize();
    Ok(())
}
pub(super) fn configuration(root: &Path) -> Result<NativeConfiguration, DisplayError> {
    let mut snapshot = read()?;
    let store = load(root)?;
    if !store.is_empty() {
        merge(&mut snapshot, &store, &available()?)?;
    }
    Ok(snapshot.configuration)
}
pub(super) fn displays(root: &Path) -> Result<Vec<DisplayInfo>, DisplayError> {
    let mut visible = super::enumerate::displays()?;
    let store = load(root)?;
    if store.is_empty() {
        return Ok(visible);
    }
    let native = configuration(root)?;
    for (key, saved) in store {
        if visible.iter().any(|d| d.key == key) || !native.displays.iter().any(|d| d.key == key) {
            continue;
        }
        if let Some(mut info) = saved.info {
            info.bounds = saved.native.bounds;
            info.is_primary = false;
            visible.push(info);
        }
    }
    Ok(visible)
}

fn visibility_plan(
    configuration: &NativeConfiguration,
    active: &[String],
    disabled: &[(String, bool)],
) -> (Vec<String>, bool) {
    let enabled = enabled_targets(active, disabled);
    let keys = configuration
        .displays
        .iter()
        .filter(|d| enabled.contains(d.mirror_of.as_ref().unwrap_or(&d.key)))
        .map(|d| d.key.clone())
        .collect();
    let changed = active != enabled.as_slice();
    (keys, changed)
}
pub(super) fn apply(
    root: &Path,
    expected: &NativeConfiguration,
    origins: &[NativeOrigin],
    disabled: &[(String, bool)],
) -> Result<NativeConfiguration, DisplayError> {
    let mut current = read()?;
    let active_keys = current
        .configuration
        .sources()
        .map(|d| d.key.clone())
        .collect::<Vec<_>>();
    let mut store = load(root)?;
    let all = if store.is_empty() { Vec::new() } else { available()? };
    merge(&mut current, &store, &all)?;
    if &current.configuration != expected {
        return Err(invalid());
    }
    if disabled
        .iter()
        .any(|(key, _)| !expected.sources().any(|d| d.key == *key))
    {
        return Err(invalid());
    }
    let mut target = if origins.is_empty() {
        expected.clone()
    } else {
        expected.with_origins(origins)?
    };
    let enabled = enabled_targets(&active_keys, disabled);
    let (retained, membership_changed) = visibility_plan(&target, &active_keys, disabled);
    let mut keep = Vec::new();
    for (i, p) in current.paths.iter().enumerate() {
        let key = target_name(p, current.paths.len())?;
        if retained.contains(&key) {
            keep.push(i);
        }
    }
    if keep.is_empty() {
        return Err(invalid());
    }
    // Removing the primary requires a surviving source at the system origin.
    // Translate the whole logical desktop (including retained hidden geometry),
    // so later unhide uses the same layout coordinate system.
    let anchor = target
        .sources()
        .find(|d| d.primary && enabled.contains(&d.key))
        .or_else(|| target.sources().find(|d| enabled.contains(&d.key)))
        .ok_or_else(invalid)?
        .bounds;
    for d in &mut target.displays {
        d.bounds.x -= anchor.x;
        d.bounds.y -= anchor.y;
    }
    let position_changed = current
        .configuration
        .displays
        .iter()
        .any(|d| target.displays.iter().any(|t| t.key == d.key && t.bounds != d.bounds));
    if !position_changed && !membership_changed {
        return Ok(current.configuration);
    }
    patch_sources(&mut current.modes, &current.sources, &target)?;
    let visible = super::enumerate::displays()?;
    let mut next = store.clone();
    for (i, p) in current.paths.iter().enumerate() {
        let key = target_name(p, current.paths.len())?;
        if keep.contains(&i) {
            // A reconnect record stays until the agent has durably enabled the
            // owner preference and acknowledged the active observation. Deleting
            // before APPLY loses the only mode on a crash; deleting immediately
            // after APPLY could make startup re-disable before preference commit.
            continue;
        }
        let mut native = target
            .displays
            .iter()
            .find(|d| d.key == key)
            .ok_or_else(invalid)?
            .clone();
        native.primary = false;
        let info = visible
            .iter()
            .find(|d| d.key == key)
            .cloned()
            .or_else(|| store.get(&key).and_then(|s| s.info.clone()));
        next.insert(
            key,
            Saved {
                released: false,
                native,
                info,
                source_adapter: (p.sourceInfo.adapterId.HighPart, p.sourceInfo.adapterId.LowPart),
                source_id: p.sourceInfo.id,
                packet: Packet::capture(p, &current.modes)?,
            },
        );
    }
    let paths = keep.into_iter().map(|i| current.paths[i]).collect::<Vec<_>>();
    // Why this shape: CCD removal is omission from a complete supplied active
    // path set, not an inactive path with stale/invalid mode indices. Reconnect
    // uses freshly enumerated available target/source identities and rebased,
    // saved exact source/target/desktop-image modes; QDC_ALL_PATHS alone contains
    // invalid mode indices for inactive targets. No mode negotiation or database
    // save is requested. Exit/switch-off release adds owned targets without
    // restoring old active origins. VALIDATE precedes durable intent,
    // then APPLY; a crash is reconciled by observing actual active targets.
    // Windows 11 build 26200 exposes distinct virtual indices for copied targets.
    // The 2026-11-07 zero-active-path probe returned ERROR_INVALID_PARAMETER
    // (87) for both empty and ACTIVE-cleared packets, VALIDATE and APPLY. Our
    // final-source guard returns before either call, preserving every clone path.
    // Live acceptance on that build (2026-11-07): the copied final source stayed
    // intact; two extended sources passed hide/unhide, outside reactivation,
    // startup and exit/switch-off release, retaining mode and layout position.
    // Only packet tests cover removing a clone group beside another source.
    set(&paths, &current.modes, VALIDATE)?;
    save(root, &next)?;
    set(&paths, &current.modes, APPLY)?;
    store = next;
    let mut observed = read()?;
    let wanted = paths
        .iter()
        .map(|p| target_name(p, paths.len()))
        .collect::<Result<Vec<_>, _>>()?;
    if observed.configuration.displays.len() != wanted.len()
        || observed.configuration.displays.iter().any(|actual| {
            !wanted.contains(&actual.key)
                || !target.displays.iter().any(|desired| {
                    desired.key == actual.key && desired.mode == actual.mode && desired.bounds == actual.bounds
                })
        })
    {
        return Err(invalid());
    }
    if !store.is_empty() {
        merge(&mut observed, &store, &available()?)?;
    }
    Ok(observed.configuration)
}
pub(super) fn release(root: &Path) -> Result<(), DisplayError> {
    let store = load(root)?;
    if store.is_empty() {
        return Ok(());
    }
    let active = read()?
        .configuration
        .displays
        .into_iter()
        .map(|d| d.key)
        .collect::<Vec<_>>();
    let current = configuration(root)?;
    let leaders = current
        .sources()
        .filter(|d| store.contains_key(&d.key) && !active.contains(&d.key))
        .map(|d| d.key.clone())
        .collect::<Vec<_>>();
    // Enabling our paths is not restoring an old desktop snapshot: retain every
    // current active path and origin, and add only the saved disabled targets.
    // The observation gate keeps our successful release out of external-unhide
    // handling. A crash with an unknown outcome follows actual OS activity.
    apply(
        root,
        &current,
        &[],
        &leaders.iter().cloned().map(|key| (key, false)).collect::<Vec<_>>(),
    )?;
    let active = read()?
        .configuration
        .displays
        .into_iter()
        .map(|d| d.key)
        .collect::<Vec<_>>();
    let mut store = load(root)?;
    for (key, saved) in &mut store {
        let leader = saved.native.mirror_of.as_ref().unwrap_or(key);
        if leaders.contains(leader) && active.contains(key) {
            saved.released = true;
        }
    }
    // Persist acknowledgement only for a known completed apply. Never preclaim
    // an outside activation while an accepted release's native outcome is unknown.
    save(root, &store)?;
    returned(root)?;
    Ok(())
}
fn set(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
    flags: SET_DISPLAY_CONFIG_FLAGS,
) -> Result<(), DisplayError> {
    // SAFETY: complete initialized paths and mode arrays from a fresh query,
    // with captured modes appended and every new index rebuilt in this packet.
    let code = unsafe { SetDisplayConfig(Some(paths), Some(modes), flags) };
    if code != 0 {
        return Err(DisplayError::Platform(format!("SetDisplayConfig: {code}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_packet_roundtrip_rebases_indices_and_preserves_mode() {
        let path = test_path();
        let modes = test_modes();
        let packet = Packet::capture(&path, &modes).unwrap();
        let bytes = serde_json::to_vec(&packet).unwrap();
        let packet: Packet = serde_json::from_slice(&bytes).unwrap();
        let mut output = vec![DISPLAYCONFIG_MODE_INFO::default(); 9];
        let rebuilt = packet.append(path, &mut output).unwrap();
        assert_eq!(source_index(&rebuilt), 9);
        assert_eq!(target_index(&rebuilt), 10);
        assert_eq!(desktop_index(&rebuilt), Some(11));
        // SAFETY: append emits source/target/desktop tagged entries, in that order.
        unsafe {
            assert_eq!(output[9].Anonymous.sourceMode.width, 1920);
            assert_eq!(
                output[10].Anonymous.targetMode.targetVideoSignalInfo.pixelRate,
                148500000
            );
            assert_eq!(output[11].Anonymous.desktopImageInfo.PathSourceSize.x, 1920);
        }
        assert_eq!(rebuilt.targetInfo.rotation, DISPLAYCONFIG_ROTATION_ROTATE90);
        assert_eq!(rebuilt.targetInfo.refreshRate.Numerator, 60000);
        assert_eq!(APPLY.0 & (SDC_ALLOW_CHANGES.0 | SDC_SAVE_TO_DATABASE.0), 0);
    }
    #[test]
    fn packet_rejects_invalid_modes_and_clones_reuse_their_source() {
        let p = test_path();
        let mut modes = test_modes();
        let packet = Packet::capture(&p, &modes).unwrap();
        let mut output = Vec::new();
        let a = packet.append(p, &mut output).unwrap();
        let mut follower = p;
        follower.targetInfo.id = 7;
        let b = packet.append(follower, &mut output).unwrap();
        assert_eq!(source_index(&a), source_index(&b));
        assert_ne!(target_index(&a), target_index(&b));
        modes[0].infoType = DISPLAYCONFIG_MODE_INFO_TYPE_TARGET;
        assert!(Packet::capture(&p, &modes).is_err());
    }
    #[test]
    fn disabled_record_survives_recreation_and_only_ack_clears_it() {
        let root = std::env::temp_dir().join(format!("oink-display-visibility-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let native = crate::arrangement::NativeDisplay {
            key: "hidden".into(),
            bounds: oink_foundation::Rect::new(1920.0, 0.0, 1920.0, 1080.0),
            primary: false,
            mirror_of: None,
            mode: "exact-mode".into(),
        };
        let store = Store::from([(
            "hidden".into(),
            Saved {
                native,
                info: None,
                source_adapter: (0, 0),
                source_id: 4,
                packet: Packet::capture(&test_path(), &test_modes()).unwrap(),
                released: false,
            },
        )]);
        save(&root, &store).unwrap();
        drop(store);
        let reloaded = load(&root).unwrap();
        assert_eq!(reloaded["hidden"].native.mode, "exact-mode");
        assert_eq!(reloaded["hidden"].packet.refresh, [60000, 1001]);
        acknowledge(&root, &["other".into()]).unwrap();
        assert_eq!(load(&root).unwrap().len(), 1);
        acknowledge(&root, &["hidden".into()]).unwrap();
        assert!(load(&root).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn desktop_image_identity_tracks_its_original_source_or_target_role() {
        for source_role in [false, true] {
            let path = test_path();
            let mut modes = test_modes();
            modes[2].id = if source_role { 4 } else { 6 };
            let packet = Packet::capture(&path, &modes).unwrap();
            let mut fresh = path;
            fresh.sourceInfo.id = 8;
            fresh.targetInfo.id = 9;
            let mut rebuilt = Vec::new();
            let fresh = packet.append(fresh, &mut rebuilt).unwrap();
            assert_eq!(
                rebuilt[desktop_index(&fresh).unwrap() as usize].id,
                if source_role { 8 } else { 9 }
            );
        }
    }
    #[test]
    fn copied_desktop_is_never_split_and_last_source_is_a_noop() {
        use crate::arrangement::NativeDisplay;
        let main = NativeDisplay {
            key: "main".into(),
            bounds: oink_foundation::Rect::new(0.0, 0.0, 1920.0, 1080.0),
            primary: true,
            mirror_of: None,
            mode: "mode".into(),
        };
        let mut copy = main.clone();
        copy.key = "copy".into();
        copy.primary = false;
        copy.mirror_of = Some("main".into());
        let mut config = NativeConfiguration {
            displays: vec![main.clone(), copy],
        };
        let (keys, changed) = visibility_plan(&config, &["main".into()], &[("main".into(), true)]);
        assert_eq!(keys, vec!["main", "copy"]);
        assert!(!changed, "last cloned desktop must return before VALIDATE/APPLY");
        let mut other = main;
        other.key = "other".into();
        other.primary = false;
        other.bounds.x = 1920.0;
        config.displays.push(other);
        let (keys, changed) = visibility_plan(&config, &["main".into(), "other".into()], &[("main".into(), true)]);
        assert!(changed);
        assert_eq!(keys, vec!["other"]);
        let (keys, _) = visibility_plan(&config, &["main".into(), "other".into()], &[("other".into(), true)]);
        assert_eq!(keys, vec!["main", "copy"]);
    }
    #[test]
    fn reconnect_uses_a_free_fresh_source_and_preserves_shared_source_groups() {
        let mut original = test_path();
        original.sourceInfo.id = 4;
        let mut alternative = original;
        alternative.sourceInfo.id = 5;
        let candidates = vec![("target".into(), original), ("target".into(), alternative)];
        let reserved = std::collections::BTreeSet::from([source_id(&original)]);
        let picked = choose_path("target", source_id(&original), None, &reserved, &candidates)
            .unwrap()
            .unwrap();
        assert_eq!(picked.sourceInfo.id, 5);
        let reserved = std::collections::BTreeSet::from([source_id(&original), source_id(&alternative)]);
        let follower = choose_path(
            "target",
            source_id(&original),
            Some(source_id(&alternative)),
            &reserved,
            &candidates,
        )
        .unwrap()
        .unwrap();
        assert_eq!(follower.sourceInfo.id, 5);
        assert!(choose_path("target", source_id(&original), None, &reserved, &candidates).is_err());
        assert!(
            choose_path("unplugged", source_id(&original), None, &reserved, &candidates)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn oink_release_ack_keeps_hidden_preference_and_never_reclaims_external_disable() {
        let native = crate::arrangement::NativeDisplay {
            key: "hidden".into(),
            bounds: oink_foundation::Rect::new(1920.0, 0.0, 1920.0, 1080.0),
            primary: false,
            mirror_of: None,
            mode: "mode".into(),
        };
        let saved = Saved {
            native,
            info: None,
            source_adapter: (0, 0),
            source_id: 4,
            packet: Packet::capture(&test_path(), &test_modes()).unwrap(),
            released: true,
        };
        let mut store = Store::from([("hidden".into(), saved.clone())]);
        assert!(reconcile_releases(&mut store));
        assert!(store.is_empty(), "our successful release needs no owner unhide");
        let mut store = Store::from([("hidden".into(), saved.clone())]);
        assert!(reconcile_releases(&mut store));
        assert!(
            store.is_empty(),
            "completed release must not reclaim a later external disable"
        );
        let mut owned = saved;
        owned.released = false;
        let mut store = Store::from([("hidden".into(), owned)]);
        assert!(!reconcile_releases(&mut store));
        assert_eq!(returned_keys(&store, &["main".into(), "hidden".into()]), vec!["hidden"]);
    }
    fn test_path() -> DISPLAYCONFIG_PATH_INFO {
        DISPLAYCONFIG_PATH_INFO {
            flags: windows::Win32::Graphics::Gdi::DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE | 1,
            sourceInfo: DISPLAYCONFIG_PATH_SOURCE_INFO {
                id: 4,
                Anonymous: DISPLAYCONFIG_PATH_SOURCE_INFO_0 { modeInfoIdx: 0xffff },
                ..Default::default()
            },
            targetInfo: DISPLAYCONFIG_PATH_TARGET_INFO {
                id: 6,
                Anonymous: DISPLAYCONFIG_PATH_TARGET_INFO_0 {
                    modeInfoIdx: (1 << 16) | 2,
                },
                rotation: DISPLAYCONFIG_ROTATION_ROTATE90,
                refreshRate: DISPLAYCONFIG_RATIONAL {
                    Numerator: 60000,
                    Denominator: 1001,
                },
                ..Default::default()
            },
        }
    }
    fn test_modes() -> Vec<DISPLAYCONFIG_MODE_INFO> {
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); 3];
        modes[0].id = 4;
        modes[0].infoType = DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE;
        modes[0].Anonymous.sourceMode.width = 1920;
        modes[0].Anonymous.sourceMode.height = 1080;
        modes[1].id = 6;
        modes[1].infoType = DISPLAYCONFIG_MODE_INFO_TYPE_TARGET;
        modes[1].Anonymous.targetMode.targetVideoSignalInfo.pixelRate = 148500000;
        modes[2].id = 4;
        modes[2].infoType = DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE;
        modes[2].Anonymous.desktopImageInfo.PathSourceSize.x = 1920;
        modes
    }
}

#[cfg(test)]
mod follow_tests {
    use super::*;
    #[test]
    fn system_reactivation_is_unhide_but_an_unplug_is_not() {
        let store = BTreeMap::from([("hidden".to_owned(), ())]);
        assert_eq!(returned_keys(&store, &["main".into(), "hidden".into()]), vec!["hidden"]);
        assert!(returned_keys(&store, &["main".into()]).is_empty());
    }
}
