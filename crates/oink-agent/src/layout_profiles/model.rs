//! Pure catalog, comparison and restoration decisions. No I/O or OS calls.
use super::types::*;
use crate::api::{DeviceKey, GroupKey, ScreenKey};
use oink_display::arrangement;
use oink_foundation::{Point, Rect};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use unicode_casefold::UnicodeCaseFold;

pub const MAX_PROFILES: usize = 16;
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
const EPS: f64 = 1e-10;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Version {
    pub counter: u64,
    pub writer: DeviceKey,
}
impl Version {
    pub fn token(&self) -> String {
        format!("{}:{}", self.counter, self.writer.0.to_hex())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Recency {
    pub logical_ms: u64,
    pub writer: DeviceKey,
    pub sequence: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceState {
    pub online_devices: BTreeSet<DeviceKey>,
    pub connected_screens: BTreeMap<DeviceKey, Option<BTreeSet<String>>>,
}
impl DeviceState {
    pub fn exact(&self, other: &Self) -> bool {
        self.connected_screens.values().all(Option::is_some)
            && other.connected_screens.values().all(Option::is_some)
            && self == other
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<u8>,
    pub id: String,
    pub name: ProfileName,
    pub content_revision: Version,
    pub last_used: Recency,
    pub content: ArrangementSnapshot,
    pub match_state: DeviceState,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<u8>,
    pub schema: u32,
    pub group: GroupKey,
    pub version: Version,
    pub active_profile_id: String,
    pub profiles: Vec<Profile>,
    pub imported_merges: BTreeSet<String>,
}

pub fn hash(value: &impl Serialize) -> String {
    // All local structs serialize deterministically; maps are BTreeMaps and
    // callers canonicalize identity-keyed lists before hashing.
    let bytes = serde_json::to_vec(value).expect("finite validated profile data");
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}
pub fn new_id() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| ProfileError::new(ErrorCode::PersistenceFailed, "Cannot create profile ID"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub fn valid_name(value: &str) -> Result<ProfileName> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 32 || value.chars().any(char::is_control) {
        return Err(ProfileError::new(
            ErrorCode::InvalidName,
            "Profile name must contain 1–32 Unicode characters without control characters",
        ));
    }
    Ok(ProfileName::Custom { value: value.into() })
}

impl Catalog {
    pub fn initial(
        group: GroupKey,
        writer: DeviceKey,
        snapshot: &ArrangementSnapshot,
        state: DeviceState,
        now: u64,
    ) -> Result<Self> {
        let mut snapshot = snapshot.clone();
        canonicalize(&mut snapshot);
        snapshot.active_profile_id.clear();
        for n in &mut snapshot.native {
            n.token.clear();
        }
        validate_snapshot(&snapshot)?;
        let id = hash(&("oink.default-layout/1", group))[..32].to_owned();
        let version = Version { counter: 1, writer };
        let profile = Profile {
            extensions: Vec::new(),
            id: id.clone(),
            name: ProfileName::Default,
            content_revision: version.clone(),
            last_used: Recency {
                logical_ms: now,
                writer,
                sequence: 1,
            },
            content: snapshot,
            match_state: state,
        };
        Ok(Self {
            extensions: Vec::new(),
            schema: 1,
            group,
            version,
            active_profile_id: id,
            profiles: vec![profile],
            imported_merges: BTreeSet::new(),
        })
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != 1
            || self.version.counter == 0
            || self.profiles.is_empty()
            || self.profiles.len() > MAX_PROFILES
            || self.imported_merges.len() > 256
            || !self.profiles.iter().any(|p| p.id == self.active_profile_id)
        {
            return Err(ProfileError::new(ErrorCode::InvalidGeometry, "Invalid profile catalog"));
        }
        let mut ids = BTreeSet::new();
        let mut names = BTreeSet::new();
        for profile in &self.profiles {
            if profile.id.len() != 32
                || !profile.id.bytes().all(|b| b.is_ascii_hexdigit())
                || !ids.insert(&profile.id)
                || profile.content_revision.counter == 0
                || profile.match_state.connected_screens.len() > 16
            {
                return Err(ProfileError::new(
                    ErrorCode::InvalidGeometry,
                    "Invalid profile identity or device state",
                ));
            }
            if !names.insert(name_key(&profile.name)) {
                return Err(ProfileError::new(ErrorCode::DuplicateName, "Duplicate profile name"));
            }
            if let ProfileName::Custom { value } = &profile.name {
                valid_name(value)?;
            }
            validate_snapshot(&profile.content)?;
        }
        if serde_json::to_vec(self)
            .map_err(|_| ProfileError::new(ErrorCode::InvalidGeometry, "Cannot encode catalog"))?
            .len()
            > MAX_BYTES
        {
            return Err(ProfileError::new(
                ErrorCode::SizeLimit,
                "Profile catalog exceeds its size limit",
            ));
        }
        Ok(())
    }
    pub fn profile(&self, id: &str) -> Result<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| ProfileError::new(ErrorCode::ProfileNotFound, "Profile no longer exists"))
    }
    pub fn active(&self) -> &Profile {
        self.profile(&self.active_profile_id).expect("validated active profile")
    }
    fn advance(&mut self, writer: DeviceKey) -> Result<()> {
        self.version = Version {
            counter: self
                .version
                .counter
                .checked_add(1)
                .ok_or_else(|| ProfileError::new(ErrorCode::SizeLimit, "Catalog revision exhausted"))?,
            writer,
        };
        Ok(())
    }
    fn recency(&self, now: u64) -> Result<Recency> {
        let previous = self.profiles.iter().map(|p| p.last_used.logical_ms).max().unwrap_or(0);
        Ok(Recency {
            logical_ms: now.max(
                previous
                    .checked_add(1)
                    .ok_or_else(|| ProfileError::new(ErrorCode::SizeLimit, "Profile clock exhausted"))?,
            ),
            writer: self.version.writer,
            sequence: self.version.counter,
        })
    }
    /// Return a candidate; the caller persists it before replacing live state.
    pub fn save(
        &self,
        writer: DeviceKey,
        snapshot: &ArrangementSnapshot,
        state: DeviceState,
        name: Option<&str>,
        now: u64,
    ) -> Result<Self> {
        let mut next = self.clone();
        if name.is_some() && next.profiles.len() == MAX_PROFILES {
            return Err(ProfileError::new(
                ErrorCode::ProfileLimit,
                "At most sixteen layout profiles",
            ));
        }
        let custom_name = name.map(valid_name).transpose()?;
        if let Some(name) = &custom_name {
            self.check_name(name, None)?;
        }
        let mut content = snapshot.clone();
        canonicalize(&mut content);
        content.active_profile_id.clear();
        for n in &mut content.native {
            n.token.clear();
        }
        validate_snapshot(&content)?;
        next.advance(writer)?;
        let used = next.recency(now)?;
        if let Some(name) = custom_name {
            let id = new_id()?;
            next.profiles.push(Profile {
                extensions: Vec::new(),
                id: id.clone(),
                name,
                content_revision: next.version.clone(),
                last_used: used,
                content,
                match_state: state,
            });
            next.active_profile_id = id;
        } else {
            let active = next
                .profiles
                .iter_mut()
                .find(|p| p.id == next.active_profile_id)
                .expect("active");
            active.content = content;
            active.match_state = state;
            active.content_revision = next.version.clone();
            active.last_used = used;
        }
        next.validate()?;
        Ok(next)
    }
    pub fn activate(&self, writer: DeviceKey, id: &str, touch: bool, now: u64) -> Result<Self> {
        self.profile(id)?;
        let mut next = self.clone();
        next.advance(writer)?;
        next.active_profile_id = id.into();
        if touch {
            let recency = next.recency(now)?;
            next.profiles
                .iter_mut()
                .find(|p| p.id == id)
                .expect("profile")
                .last_used = recency;
        }
        Ok(next)
    }
    pub fn rename(&self, writer: DeviceKey, id: &str, name: &str) -> Result<Self> {
        self.profile(id)?;
        let name = valid_name(name)?;
        self.check_name(&name, Some(id))?;
        let mut next = self.clone();
        next.advance(writer)?;
        next.profiles.iter_mut().find(|p| p.id == id).expect("profile").name = name;
        Ok(next)
    }
    pub fn delete(
        &self,
        writer: DeviceKey,
        id: &str,
        _snapshot: &ArrangementSnapshot,
        _state: DeviceState,
        _now: u64,
    ) -> Result<Self> {
        self.profile(id)?;
        if self.profiles.len() == 1 {
            return Err(ProfileError::new(
                ErrorCode::LastProfile,
                "The last profile cannot be deleted",
            ));
        }
        let mut next = self.clone();
        next.advance(writer)?;
        next.profiles.retain(|p| p.id != id);
        if next.active_profile_id == id {
            next.profiles
                .sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.id.cmp(&b.id)));
            next.active_profile_id = next.profiles[0].id.clone();
        }
        next.validate()?;
        Ok(next)
    }
    fn check_name(&self, name: &ProfileName, except: Option<&str>) -> Result<()> {
        if self
            .profiles
            .iter()
            .any(|p| Some(p.id.as_str()) != except && name_key(&p.name) == name_key(name))
        {
            return Err(ProfileError::new(
                ErrorCode::DuplicateName,
                "A profile with this name already exists",
            ));
        }
        Ok(())
    }
    fn disambiguate_names(&mut self) {
        // Merge order cannot choose a winner: stable ID order gives all replicas
        // the same names. Rehash equal prefixes until the bounded set is unique.
        let mut order = (0..self.profiles.len()).collect::<Vec<_>>();
        order.sort_by(|a, b| self.profiles[*a].id.cmp(&self.profiles[*b].id));
        let mut used = BTreeSet::new();
        for index in order {
            let p = &mut self.profiles[index];
            if !used.insert(name_key(&p.name)) {
                let base = match &p.name {
                    ProfileName::Default => "Default layout",
                    ProfileName::Custom { value } => value,
                };
                let prefix = base.chars().take(23).collect::<String>();
                let mut suffix = p.id[..8].to_owned();
                let mut name = format!("{prefix} {suffix}");
                while !used.insert(name.case_fold().collect::<String>()) {
                    suffix = hash(&(p.id.as_str(), suffix.as_str()))[..8].to_owned();
                    name = format!("{prefix} {suffix}");
                }
                p.name = ProfileName::Custom { value: name };
            }
        }
    }
    pub fn summaries(&self, current: &DeviceState) -> Vec<ProfileSummary> {
        let mut profiles = self.profiles.iter().collect::<Vec<_>>();
        profiles.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.id.cmp(&b.id)));
        profiles
            .into_iter()
            .map(|p| ProfileSummary {
                id: p.id.clone(),
                name: p.name.clone(),
                content_revision: p.content_revision.token(),
                last_used_ms: p.last_used.logical_ms.to_string(),
                exact_match: p.match_state.exact(current),
                active: p.id == self.active_profile_id,
            })
            .collect()
    }
    pub fn receive(&self, incoming: &Self, writer: DeviceKey) -> Result<Self> {
        incoming.validate()?;
        if incoming.group != self.group {
            return Err(ProfileError::new(
                ErrorCode::NotMember,
                "Catalog belongs to another group",
            ));
        }
        if let Some(merge_id) = incoming.imported_merges.difference(&self.imported_merges).next() {
            // A source can import its archive before receiving the target's
            // catalog. Its higher version then contains only part of the union.
            // Complete each newly observed import against our retained catalog
            // before LWW. Known imports stay on LWW so replay cannot undo deletes.
            return self.merge_group(incoming, merge_id, writer);
        }
        match incoming.version.cmp(&self.version) {
            std::cmp::Ordering::Greater => Ok(incoming.clone()),
            std::cmp::Ordering::Less => Ok(self.clone()),
            std::cmp::Ordering::Equal if incoming == self => Ok(self.clone()),
            _ => Err(ProfileError::new(
                ErrorCode::StaleState,
                "Same catalog version has different content",
            )),
        }
    }
    /// The caller verifies the existing certified merge proof before importing.
    pub fn merge_group(&self, source: &Self, merge_id: &str, writer: DeviceKey) -> Result<Self> {
        source.validate()?;
        if self.imported_merges.contains(merge_id) {
            return Ok(self.clone());
        }
        let mut next = self.clone();
        next.version.counter = next.version.counter.max(source.version.counter);
        next.advance(writer)?;
        for incoming in &source.profiles {
            if let Some(p) = next.profiles.iter_mut().find(|p| p.id == incoming.id) {
                let recency = p.last_used.clone().max(incoming.last_used.clone());
                if incoming.content_revision > p.content_revision {
                    *p = incoming.clone();
                }
                p.last_used = recency;
            } else {
                next.profiles.push(incoming.clone());
            }
        }
        next.profiles
            .sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.id.cmp(&b.id)));
        next.profiles.truncate(MAX_PROFILES);
        if !next.profiles.iter().any(|p| p.id == next.active_profile_id) {
            next.active_profile_id = if next.profiles.iter().any(|p| p.id == source.active_profile_id) {
                source.active_profile_id.clone()
            } else {
                next.profiles[0].id.clone()
            };
        }
        next.imported_merges.extend(source.imported_merges.iter().cloned());
        next.imported_merges.insert(merge_id.into());
        next.disambiguate_names();
        next.validate()?;
        Ok(next)
    }
}

pub fn canonicalize(snapshot: &mut ArrangementSnapshot) {
    snapshot.screen_shared.sort_by_key(|d| d.device);
    snapshot.placements.sort_by_key(|p| p.device);
    snapshot.disabled.sort_by(|a, b| a.screen.cmp(&b.screen));
    snapshot.native.sort_by_key(|n| n.device);
    for n in &mut snapshot.native {
        n.screens.sort_by(|a, b| a.screen.cmp(&b.screen));
    }
}
pub fn validate_snapshot(snapshot: &ArrangementSnapshot) -> Result<()> {
    let invalid = || ProfileError::new(ErrorCode::InvalidGeometry, "Invalid profile geometry");
    if snapshot.placements.len() > 16
        || snapshot.native.len() != snapshot.placements.len()
        || snapshot.native.iter().map(|n| n.screens.len()).sum::<usize>() > 256
    {
        return Err(invalid());
    }
    let mut devices = BTreeSet::new();
    for p in &snapshot.placements {
        if !devices.insert(p.device)
            || ![p.origin.x, p.origin.y, p.scale].into_iter().all(f64::is_finite)
            || p.scale <= 0.0
            || p.scale > 1e9
            || p.origin.x.abs() > 1e12
            || p.origin.y.abs() > 1e12
        {
            return Err(invalid());
        }
    }
    let mut sharing = BTreeSet::new();
    if snapshot
        .screen_shared
        .iter()
        .any(|d| !devices.contains(&d.device) || !sharing.insert(d.device))
    {
        return Err(invalid());
    }
    let mut natives = BTreeSet::new();
    let mut screens = BTreeSet::new();
    for n in &snapshot.native {
        if !natives.insert(n.device) || !devices.contains(&n.device) || n.primary.device != n.device {
            return Err(invalid());
        }
        if !n.screens.is_empty() && !n.screens.iter().any(|s| s.screen == n.primary) {
            return Err(invalid());
        }
        for s in &n.screens {
            if s.screen.device != n.device
                || s.screen.display.is_empty()
                || s.screen.display.len() > 512
                || !screens.insert(s.screen.clone())
                || ![
                    s.origin.x,
                    s.origin.y,
                    s.size.w,
                    s.size.h,
                    s.origin.x + s.size.w,
                    s.origin.y + s.size.h,
                ]
                .into_iter()
                .all(arrangement::valid_integer)
                || s.size.w <= 0.0
                || s.size.h <= 0.0
            {
                return Err(invalid());
            }
        }
    }
    let bits = snapshot
        .disabled
        .iter()
        .map(|d| d.screen.clone())
        .collect::<BTreeSet<_>>();
    if bits.len() != snapshot.disabled.len() || bits != screens {
        return Err(invalid());
    }
    let s = &snapshot.topology;
    if ![s.overlap_tolerance, s.minimum_portal, s.portal_snap, s.endpoint_reach]
        .into_iter()
        .all(|v| v.is_finite() && v >= 0.0)
    {
        return Err(invalid());
    }
    Ok(())
}
fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= EPS * a.abs().max(b.abs()).max(1.0)
}
fn same_point(a: Point, b: Point) -> bool {
    close(a.x, b.x) && close(a.y, b.y)
}
pub fn same_native(a: &NativeState, b: &NativeState) -> bool {
    a.device == b.device
        && a.primary == b.primary
        && a.screens.len() == b.screens.len()
        && a.screens.iter().all(|s| {
            b.screens.iter().any(|t| {
                s.screen == t.screen
                    && same_point(s.origin, t.origin)
                    && close(s.size.w, t.size.w)
                    && close(s.size.h, t.size.h)
            })
        })
}
pub fn screen_shared(snapshot: &ArrangementSnapshot, device: DeviceKey) -> bool {
    snapshot
        .screen_shared
        .iter()
        .find(|d| d.device == device)
        .is_none_or(|d| d.shared)
}
pub fn set_screen_shared(snapshot: &mut ArrangementSnapshot, device: DeviceKey, shared: bool) {
    snapshot.screen_shared.retain(|d| d.device != device);
    snapshot.screen_shared.push(DeviceSharing { device, shared });
}
pub fn same_content(a: &ArrangementSnapshot, b: &ArrangementSnapshot) -> bool {
    a.placements.len() == b.placements.len()
        && a.native.len() == b.native.len()
        && a.disabled.len() == b.disabled.len()
        && a.topology == b.topology
        && a.placements
            .iter()
            .all(|p| screen_shared(a, p.device) == screen_shared(b, p.device))
        && a.placements.iter().all(|p| {
            b.placements
                .iter()
                .any(|q| p.device == q.device && same_point(p.origin, q.origin) && close(p.scale, q.scale))
        })
        && a.native.iter().all(|n| b.native.iter().any(|m| same_native(n, m)))
        && a.disabled.iter().all(|d| b.disabled.contains(d))
}
pub fn history_changed(a: &ArrangementSnapshot, b: &ArrangementSnapshot) -> bool {
    !same_content(a, b) || a.active_profile_id != b.active_profile_id
}

#[derive(Clone, Debug)]
pub struct RestorePlan {
    pub snapshot: ArrangementSnapshot,
    pub adjustments: Vec<Adjustment>,
    pub native_devices: Vec<DeviceKey>,
}
fn adjustment(code: AdjustmentCode, device: DeviceKey, screen: Option<ScreenKey>) -> Adjustment {
    Adjustment { code, device, screen }
}
pub fn native_rects(n: &NativeState) -> Vec<Rect> {
    n.screens
        .iter()
        .map(|s| Rect::new(s.origin.x, s.origin.y, s.size.w, s.size.h))
        .collect()
}
pub fn snap_native(n: &NativeState, screen: &ScreenKey, desired: Point) -> Result<NativeState> {
    let i = n
        .screens
        .iter()
        .position(|s| s.screen == *screen)
        .ok_or_else(|| ProfileError::new(ErrorCode::InvalidGeometry, "Unknown screen"))?;
    let p = arrangement::snap(&native_rects(n), i, desired)
        .map_err(|_| ProfileError::new(ErrorCode::Disconnected, "No connected native position").device(n.device))?;
    let mut next = n.clone();
    next.screens[i].origin = p;
    Ok(next)
}

pub fn restore(
    saved: &ArrangementSnapshot,
    current: &ArrangementSnapshot,
    devices: &[DeviceArrangementState],
) -> Result<RestorePlan> {
    validate_snapshot(saved)?;
    validate_snapshot(current)?;
    let mut next = current.clone();
    let mut adjustments = Vec::new();
    let mut native_devices = Vec::new();
    let mut changed = BTreeSet::new();
    next.topology = saved.topology.clone();
    for p in &saved.placements {
        if !devices.iter().any(|d| d.device == p.device) {
            adjustments.push(adjustment(AdjustmentCode::DeviceRemoved, p.device, None));
        }
    }
    for device in devices {
        let Some(index) = next.native.iter().position(|n| n.device == device.device) else {
            continue;
        };
        let old = next.native[index].clone();
        let Some(stored) = saved.native.iter().find(|n| n.device == device.device) else {
            adjustments.push(adjustment(AdjustmentCode::DeviceAdded, device.device, None));
            continue;
        };
        let Some(saved_p) = saved.placements.iter().find(|p| p.device == device.device) else {
            continue;
        };
        let p = next
            .placements
            .iter_mut()
            .find(|p| p.device == device.device)
            .expect("validated placement");
        *p = saved_p.clone();
        if device.availability == Availability::Offline {
            // Retained offline identities can use saved drawing geometry, but
            // a removed screen has no current owner visibility to resurrect.
            for screen in &mut next.native[index].screens {
                if let Some(saved) = stored.screens.iter().find(|s| s.screen == screen.screen) {
                    screen.origin = saved.origin;
                    screen.size = saved.size;
                }
                adjustments.push(adjustment(
                    AdjustmentCode::OwnerOffline,
                    device.device,
                    Some(screen.screen.clone()),
                ));
            }
            continue;
        }
        for s in &stored.screens {
            if !old.screens.iter().any(|c| c.screen == s.screen) {
                adjustments.push(adjustment(
                    AdjustmentCode::ScreenRemoved,
                    device.device,
                    Some(s.screen.clone()),
                ));
            }
        }
        for s in &old.screens {
            if !stored.screens.iter().any(|c| c.screen == s.screen) {
                adjustments.push(adjustment(
                    AdjustmentCode::ScreenAdded,
                    device.device,
                    Some(s.screen.clone()),
                ));
            }
        }
        if !same_native(&old, stored) {
            changed.insert(device.device);
            adjustments.push(adjustment(AdjustmentCode::NativeChanged, device.device, None));
        }
        if let Some(main) = old.screens.iter().find(|s| s.screen == old.primary) {
            let saved_main = stored
                .screens
                .iter()
                .find(|s| s.screen == main.screen)
                .or_else(|| stored.screens.iter().find(|s| s.screen == stored.primary));
            if let Some(saved_main) = saved_main {
                p.origin = Point::new(
                    saved_p.origin.x + (saved_main.origin.x - main.origin.x) * p.scale,
                    saved_p.origin.y + (saved_main.origin.y - main.origin.y) * p.scale,
                );
                if old.primary != stored.primary {
                    adjustments.push(adjustment(AdjustmentCode::AnchorChanged, device.device, None));
                }
                if device.sync_system_arrangement && device.availability == Availability::Ready {
                    let mut exact = old.clone();
                    for screen in &mut exact.screens {
                        screen.origin = if screen.screen == old.primary {
                            Point::default()
                        } else {
                            stored
                                .screens
                                .iter()
                                .find(|s| s.screen == screen.screen)
                                .map(|s| Point::new(s.origin.x - saved_main.origin.x, s.origin.y - saved_main.origin.y))
                                .unwrap_or(screen.origin)
                        };
                    }
                    let exact_valid = arrangement::validate_rectangles(&native_rects(&exact)).is_ok();
                    let mut assembled = old.clone();
                    let mut anchor = main.clone();
                    anchor.origin = Point::default();
                    assembled.screens = vec![anchor];
                    let mut others = old
                        .screens
                        .iter()
                        .filter(|s| s.screen != old.primary)
                        .collect::<Vec<_>>();
                    others.sort_by(|a, b| a.screen.cmp(&b.screen));
                    for s in others.into_iter().filter(|_| !exact_valid) {
                        let desired = stored
                            .screens
                            .iter()
                            .find(|t| t.screen == s.screen)
                            .map(|t| Point::new(t.origin.x - saved_main.origin.x, t.origin.y - saved_main.origin.y))
                            .unwrap_or(s.origin);
                        assembled.screens.push(s.clone());
                        assembled = snap_native(&assembled, &s.screen, desired)?;
                        if assembled.screens.last().is_some_and(|t| t.origin != desired) {
                            adjustments.push(adjustment(
                                AdjustmentCode::NativeSnapped,
                                device.device,
                                Some(s.screen.clone()),
                            ));
                        }
                    }
                    if exact_valid {
                        assembled = exact;
                    }
                    p.origin = Point::new(
                        saved_p.origin.x + saved_main.origin.x * p.scale,
                        saved_p.origin.y + saved_main.origin.y * p.scale,
                    );
                    if !same_native(&old, &assembled) {
                        native_devices.push(device.device);
                    }
                    next.native[index] = assembled;
                }
            }
        }
        set_screen_shared(&mut next, device.device, screen_shared(saved, device.device));
        for bit in next.disabled.iter_mut().filter(|d| d.screen.device == device.device) {
            if let Some(saved) = saved.disabled.iter().find(|s| s.screen == bit.screen) {
                bit.disabled = saved.disabled;
            }
        }
    }
    let rank = |d: DeviceKey| {
        if changed.contains(&d) {
            0
        } else if saved.placements.iter().any(|p| p.device == d) {
            1
        } else {
            2
        }
    };
    let mut order = devices.iter().map(|d| d.device).collect::<Vec<_>>();
    order.sort_by_key(|d| (rank(*d), *d));
    repair_collisions(&mut next, &order, &mut adjustments)?;
    canonicalize(&mut next);
    validate_snapshot(&next)?;
    Ok(RestorePlan {
        snapshot: next,
        adjustments,
        native_devices,
    })
}
/// Replay only affected retained fields. Membership churn reconciles geometry;
/// native history is strict and must never silently turn into a new Load plan.
pub fn replay(
    from: &ArrangementSnapshot,
    target: &ArrangementSnapshot,
    current: &ArrangementSnapshot,
    devices: &[DeviceArrangementState],
    native_devices: &[DeviceKey],
) -> Result<RestorePlan> {
    validate_snapshot(from)?;
    validate_snapshot(target)?;
    let mut expected = from.clone();
    let mut actual = current.clone();
    expected
        .placements
        .retain(|p| current.placements.iter().any(|c| c.device == p.device));
    actual
        .placements
        .retain(|p| from.placements.iter().any(|c| c.device == p.device));
    expected
        .disabled
        .retain(|b| current.disabled.iter().any(|c| c.screen == b.screen));
    actual
        .disabled
        .retain(|b| from.disabled.iter().any(|c| c.screen == b.screen));
    expected.native.clear();
    actual.native.clear();
    if !same_content(&expected, &actual) {
        return Err(ProfileError::new(
            ErrorCode::StaleState,
            "History conflicts with current geometry or visibility",
        ));
    }
    let mut owners = devices.to_vec();
    for device in &mut owners {
        let restore_native = native_devices.contains(&device.device);
        if restore_native {
            if !device.sync_system_arrangement || device.availability != Availability::Ready {
                return Err(ProfileError::new(
                    ErrorCode::SettingDisabled,
                    "History needs an available native owner with arrangement sync on",
                )
                .device(device.device));
            }
            let changed = || {
                ProfileError::new(
                    ErrorCode::NativeChanged,
                    "Native history identities, dimensions or origins changed",
                )
                .device(device.device)
            };
            let live = current
                .native
                .iter()
                .find(|n| n.device == device.device)
                .ok_or_else(changed)?;
            let source = from
                .native
                .iter()
                .find(|n| n.device == device.device)
                .ok_or_else(changed)?;
            let saved = target
                .native
                .iter()
                .find(|n| n.device == device.device)
                .ok_or_else(changed)?;
            if !same_native(live, source)
                || live.primary != saved.primary
                || live.screens.len() != saved.screens.len()
                || saved
                    .screens
                    .iter()
                    .any(|s| !live.screens.iter().any(|c| c.screen == s.screen && c.size == s.size))
            {
                return Err(changed());
            }
            arrangement::validate_rectangles(&native_rects(saved)).map_err(|_| changed())?;
        }
        device.sync_system_arrangement &= restore_native;
    }
    let mut plan = restore(target, current, &owners)?;
    let added = current
        .placements
        .iter()
        .filter(|p| !target.placements.iter().any(|t| t.device == p.device))
        .collect::<Vec<_>>();
    let mut order = Vec::new();
    for p in added {
        *plan
            .snapshot
            .placements
            .iter_mut()
            .find(|t| t.device == p.device)
            .expect("retained placement") = p.clone();
        order.push(p.device);
    }
    order.extend(target.placements.iter().map(|p| p.device));
    repair_collisions(&mut plan.snapshot, &order, &mut plan.adjustments)?;
    Ok(plan)
}
fn visible_rects(s: &ArrangementSnapshot, device: DeviceKey) -> Vec<Rect> {
    if !screen_shared(s, device) {
        return Vec::new();
    }
    let Some(p) = s.placements.iter().find(|p| p.device == device) else {
        return Vec::new();
    };
    s.native
        .iter()
        .filter(|n| n.device == device)
        .flat_map(|n| &n.screens)
        .filter(|n| !s.disabled.iter().any(|d| d.screen == n.screen && d.disabled))
        .map(|n| {
            Rect::new(
                p.origin.x + n.origin.x * p.scale,
                p.origin.y + n.origin.y * p.scale,
                n.size.w * p.scale,
                n.size.h * p.scale,
            )
        })
        .collect()
}
fn overlap(a: Rect, b: Rect) -> bool {
    let eps = EPS * a.w.max(a.h).max(b.w).max(b.h);
    (a.x + a.w).min(b.x + b.w) - a.x.max(b.x) > eps && (a.y + a.h).min(b.y + b.h) - a.y.max(b.y) > eps
}
pub fn repair_collisions(
    s: &mut ArrangementSnapshot,
    order: &[DeviceKey],
    adjustments: &mut Vec<Adjustment>,
) -> Result<()> {
    let mut obstacles: Vec<Rect> = Vec::new();
    for device in order {
        let moving = visible_rects(s, *device);
        if moving.iter().any(|a| obstacles.iter().any(|b| overlap(*a, *b))) {
            let mut xs = vec![0.0];
            let mut ys = vec![0.0];
            for a in &moving {
                for b in &obstacles {
                    xs.extend([b.x + b.w - a.x, b.x - a.x - a.w]);
                    ys.extend([b.y + b.h - a.y, b.y - a.y - a.h]);
                }
            }
            xs.sort_by(f64::total_cmp);
            xs.dedup();
            ys.sort_by(f64::total_cmp);
            ys.dedup();
            let mut best: Option<(f64, Point)> = None;
            for x in xs {
                for y in &ys {
                    let cost = x * x + y * y;
                    if best.as_ref().is_some_and(|(c, _)| cost >= *c) {
                        continue;
                    }
                    if moving.iter().all(|a| {
                        obstacles
                            .iter()
                            .all(|b| !overlap(Rect::new(a.x + x, a.y + y, a.w, a.h), *b))
                    }) {
                        best = Some((cost, Point::new(x, *y)));
                    }
                }
            }
            let shift = best
                .ok_or_else(|| ProfileError::new(ErrorCode::NoValidPlacement, "No free device position"))?
                .1;
            let p = s
                .placements
                .iter_mut()
                .find(|p| p.device == *device)
                .expect("placement");
            p.origin.x += shift.x;
            p.origin.y += shift.y;
            adjustments.push(adjustment(AdjustmentCode::OverlapRepaired, *device, None));
        }
        obstacles.extend(visible_rects(s, *device));
    }
    Ok(())
}

fn name_key(name: &ProfileName) -> String {
    match name {
        ProfileName::Default => "default layout".into(),
        ProfileName::Custom { value } => value.trim().case_fold().collect(),
    }
}
