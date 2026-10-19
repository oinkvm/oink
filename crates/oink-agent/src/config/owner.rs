//! Device-authored preferences. Only this store advances local revisions; peer
//! facts are never merged into its items. Audio ordering records are group-local.
use super::store::{self, StoreError};
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v1::{
    RawMap, RawValue,
    dto::{self, ControlFailure, DevicePreference, LocalOption},
    schema::{WireDto, WireValue},
};
use std::collections::BTreeSet;

pub const MAX_OWNER_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct ClaimRecord {
    pub owner: DeviceId,
    pub revision: u64,
    pub claim: Option<dto::AudioExitClaim>,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroupClaims {
    pub seen_counter: u64,
    pub records: Vec<ClaimRecord>,
    pub watermark: Option<dto::AudioExitClaim>,
}
impl GroupClaims {
    pub fn watermark(&self) -> Option<dto::AudioExitClaim> {
        self.records
            .iter()
            .filter_map(|record| record.claim.as_ref())
            .chain(self.watermark.iter())
            .max_by_key(|claim| oink_audio::claims::claim_order(claim).expect("validated ordering witness"))
            .cloned()
    }
}

/// CBOR map: 1 schema, 2 ordered DevicePreference list, 3 group records,
/// 4 active group (optional). Each group: 1 group ID, 2 seen counter,
/// 3 records [{1 owner, 2 revision, 3 claim/null}], 4 ordering witness/null.
/// All unknown keys are retained.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnerStore {
    raw: RawMap,
}
impl OwnerStore {
    pub fn new(defaults: impl IntoIterator<Item = (LocalOption, RawValue)>) -> Result<Self, StoreError> {
        let mut items: Vec<_> = defaults
            .into_iter()
            .map(|(option, value)| {
                DevicePreference::from_raw([(1, option.write()), (2, 1u64.write()), (3, value)].into())
            })
            .collect::<Result<_, _>>()?;
        items.sort_by_key(|item| item.option().expect("valid option").number());
        Self::from_raw(
            [
                (1, 1u64.write()),
                (2, RawValue::Array(items.iter().map(WireValue::write).collect())),
                (3, RawValue::Array(Vec::new())),
            ]
            .into(),
        )
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
        Self::from_raw(oink_protocol::v1::cbor::decode_map(bytes, MAX_OWNER_BYTES)?)
    }
    pub fn encode(&self) -> Result<Vec<u8>, StoreError> {
        Ok(oink_protocol::v1::cbor::encode_map(&self.raw, MAX_OWNER_BYTES)?)
    }
    fn from_raw(raw: RawMap) -> Result<Self, StoreError> {
        store::schema(&raw)?;
        let items = store::entries(&raw)?;
        if items.len() > 32 {
            return Err(StoreError::LimitExceeded);
        }
        let mut previous = 0;
        for value in items {
            let item = DevicePreference::read(value)?;
            let option = item.option()?.number();
            if option <= previous || matches!(option, 9 | 17) {
                return Err(StoreError::Invalid);
            }
            previous = option;
        }
        let groups = array(store::required(&raw, 3)?)?;
        let mut seen = BTreeSet::new();
        for group in groups {
            let map = group.as_map()?;
            if !seen.insert(store::group(store::required(map, 1)?)?) {
                return Err(StoreError::Invalid);
            }
            parse_claims(map)?;
        }
        if let Some(group) = raw.get(&4) {
            store::group(group)?;
        }
        Ok(Self { raw })
    }
    pub fn preferences(&self) -> Vec<DevicePreference> {
        store::entries(&self.raw)
            .expect("validated items")
            .iter()
            .map(|item| DevicePreference::read(item).expect("validated preference"))
            .collect()
    }
    pub fn item(&self, option: LocalOption) -> Option<DevicePreference> {
        self.preferences().into_iter().find(|item| item.option() == Ok(option))
    }
    pub fn revision(&self, option: LocalOption) -> Option<u64> {
        self.item(option).and_then(|item| item.revision().ok())
    }
    pub fn value(&self, option: LocalOption) -> Option<RawValue> {
        self.item(option).and_then(|item| item.value().ok())
    }

    /// Composite command values are narrow patches. Keyed-list changes carry
    /// {1: upserts, 2: removals}; omission never erases another known or future path.
    /// Audio option20 is command-derived and uses commit_claims instead.
    pub fn edit(
        &mut self,
        option: LocalOption,
        expected: u64,
        patch: RawValue,
    ) -> Result<DevicePreference, ControlFailure> {
        if option == LocalOption::AudioExitClaim {
            return Err(ControlFailure::UnsupportedOption);
        }
        let current = self.item(option).ok_or(ControlFailure::UnsupportedOption)?;
        if current.revision().map_err(invalid)? != expected {
            return Err(ControlFailure::StaleRevision);
        }
        let mut value = current.value().map_err(invalid)?;
        if matches!(
            option,
            LocalOption::ScreenPreferences
                | LocalOption::InputTransform
                | LocalOption::SourceInputPolicy
                | LocalOption::ForwardingControls
        ) {
            apply_patch(option, &mut value, &patch)?;
        } else {
            value = patch;
        }
        validate_value(option, &value)?;
        self.replace_value(option, expected, value)
    }

    /// Owner actor's already validated full value (used to commit a local typed
    /// edit). The caller must preserve unknown subtrees when building `value`.
    pub(crate) fn replace_value(
        &mut self,
        option: LocalOption,
        expected: u64,
        value: RawValue,
    ) -> Result<DevicePreference, ControlFailure> {
        let current = self.item(option).ok_or(ControlFailure::UnsupportedOption)?;
        if current.revision().map_err(invalid)? != expected {
            return Err(ControlFailure::StaleRevision);
        }
        if current.value().map_err(invalid)? == value {
            return Ok(current);
        }
        let revision = expected.checked_add(1).ok_or(ControlFailure::StateConflict)?;
        let mut raw = current.into_raw();
        raw.insert(2, revision.write());
        raw.insert(3, value);
        let item = DevicePreference::from_raw(raw).map_err(invalid)?;
        self.set_item(item.clone());
        Ok(item)
    }
    fn set_item(&mut self, item: DevicePreference) {
        let RawValue::Array(items) = self.raw.get_mut(&2).expect("validated items") else {
            unreachable!()
        };
        let index = items
            .iter()
            .position(|v| DevicePreference::read(v).expect("validated item").option() == item.option())
            .expect("known option");
        items[index] = item.write();
    }
    /// Startup safety release is a new publication even when the persisted
    /// keyboard was already enabled. Invoke once per backend Boot, inside the
    /// same durable owner transaction used for ordinary preference changes.
    pub fn clear_keyboard_on_start(&mut self) -> Result<DevicePreference, StoreError> {
        let mut item = self
            .item(LocalOption::KeyboardDisabled)
            .ok_or(StoreError::Invalid)?
            .into_raw();
        let revision = item
            .get(&2)
            .ok_or(StoreError::Invalid)?
            .as_u64()?
            .checked_add(1)
            .ok_or(StoreError::LimitExceeded)?;
        item.insert(2, revision.write());
        item.insert(3, false.write());
        let item = DevicePreference::from_raw(item)?;
        self.set_item(item.clone());
        Ok(item)
    }
    pub fn active_group(&self) -> Option<GroupId> {
        self.raw.get(&4).and_then(|value| store::group(value).ok())
    }
    pub fn claims(&self, group: GroupId) -> GroupClaims {
        self.group_raw(group)
            .map(|raw| parse_claims(raw).expect("validated group"))
            .unwrap_or_default()
    }
    fn group_raw(&self, group: GroupId) -> Option<&RawMap> {
        array(self.raw.get(&3).expect("validated groups"))
            .expect("validated groups")
            .iter()
            .map(|v| v.as_map().expect("validated group"))
            .find(|raw| store::group(raw.get(&1).expect("group ID")).ok() == Some(group))
    }
    /// Switch the option20 projection without moving source-group claims. The
    /// global option revision advances even when both projections are null.
    pub fn switch_group(&mut self, group: GroupId, owner: DeviceId) -> Result<(), StoreError> {
        if self.active_group() == Some(group) {
            return Ok(());
        }
        let mut candidate = self.clone();
        let item = candidate.item(LocalOption::AudioExitClaim).ok_or(StoreError::Invalid)?;
        let revision = item.revision()?.checked_add(1).ok_or(StoreError::LimitExceeded)?;
        let mut claims = candidate.claims(group);
        let claim = claims
            .records
            .iter()
            .find(|r| r.owner == owner)
            .and_then(|r| r.claim.clone());
        let record = ClaimRecord {
            owner,
            revision,
            claim: claim.clone(),
        };
        if let Some(existing) = claims.records.iter_mut().find(|r| r.owner == owner) {
            *existing = record;
        } else {
            claims.records.push(record);
        }
        let mut raw = item.into_raw();
        raw.insert(2, revision.write());
        raw.insert(3, claim.write());
        candidate.set_item(DevicePreference::from_raw(raw)?);
        candidate.raw.insert(4, RawValue::Bytes(group.0.to_vec()));
        candidate.commit_claims(group, owner, claims)?;
        *self = candidate;
        Ok(())
    }
    /// Persist the pure audio owner's proposed authenticated records together
    /// with option20. Removed-member records and group history are never pruned.
    pub fn commit_claims(&mut self, group: GroupId, owner: DeviceId, claims: GroupClaims) -> Result<(), StoreError> {
        let previous = self.claims(group);
        if claims.seen_counter < previous.seen_counter {
            return Err(StoreError::Invalid);
        }
        let order =
            |claim: &dto::AudioExitClaim| oink_audio::claims::claim_order(claim).map_err(|_| StoreError::Invalid);
        if claims.watermark.as_ref().map(order).transpose()? < previous.watermark.as_ref().map(order).transpose()? {
            return Err(StoreError::Invalid);
        }
        if previous
            .records
            .iter()
            .any(|old| !claims.records.iter().any(|new| new.owner == old.owner))
        {
            return Err(StoreError::Invalid);
        }
        let mut map = self.group_raw(group).cloned().unwrap_or_default();
        let prior = map.get(&3).map(array).transpose()?.unwrap_or_default();
        let mut records = Vec::new();
        for record in &claims.records {
            if let Some(old) = previous.records.iter().find(|old| old.owner == record.owner)
                && (record.revision < old.revision || (record.revision == old.revision && record != old))
            {
                return Err(StoreError::Invalid);
            }
            let mut raw = prior
                .iter()
                .map(|v| v.as_map().expect("validated claim"))
                .find(|raw| store::device(raw.get(&1).expect("owner")).ok() == Some(record.owner))
                .cloned()
                .unwrap_or_default();
            raw.entry(1).or_insert_with(|| store::wire_device(record.owner).write());
            raw.insert(2, record.revision.write());
            raw.insert(3, record.claim.write());
            records.push(RawValue::Map(raw));
        }
        map.insert(1, RawValue::Bytes(group.0.to_vec()));
        map.insert(2, claims.seen_counter.write());
        map.insert(3, RawValue::Array(records));
        map.insert(4, claims.watermark.write());
        parse_claims(&map)?;
        let mut candidate = self.clone();
        let RawValue::Array(groups) = candidate.raw.get_mut(&3).expect("groups") else {
            unreachable!()
        };
        if let Some(index) = groups
            .iter()
            .position(|v| store::group(v.as_map().expect("group").get(&1).expect("ID")).ok() == Some(group))
        {
            groups[index] = RawValue::Map(map);
        } else {
            groups.push(RawValue::Map(map));
        }
        if candidate.active_group() == Some(group)
            && let Some(local) = claims.records.iter().find(|record| record.owner == owner)
        {
            let mut item = candidate
                .item(LocalOption::AudioExitClaim)
                .ok_or(StoreError::Invalid)?
                .into_raw();
            let current_revision = item.get(&2).ok_or(StoreError::Invalid)?.as_u64()?;
            if local.revision < current_revision {
                return Err(StoreError::Invalid);
            }
            if local.revision == current_revision && item.get(&3) != Some(&local.claim.write()) {
                return Err(StoreError::Invalid);
            }
            item.insert(2, local.revision.write());
            item.insert(3, local.claim.write());
            candidate.set_item(DevicePreference::from_raw(item)?);
        }
        candidate.encode()?;
        *self = candidate;
        Ok(())
    }
}
fn invalid(_: oink_protocol::v1::Error) -> ControlFailure {
    ControlFailure::InvalidValue
}
fn array(value: &RawValue) -> Result<&[RawValue], StoreError> {
    match value {
        RawValue::Array(v) => Ok(v),
        _ => Err(StoreError::Invalid),
    }
}
fn parse_claims(raw: &RawMap) -> Result<GroupClaims, StoreError> {
    let seen_counter = store::required(raw, 2)?.as_u64()?;
    let mut seen = BTreeSet::new();
    let mut records = Vec::new();
    for value in array(store::required(raw, 3)?)? {
        let raw = value.as_map()?;
        let owner = store::device(store::required(raw, 1)?)?;
        let revision = store::required(raw, 2)?.as_u64()?;
        if revision == 0 || !seen.insert(owner) {
            return Err(StoreError::Invalid);
        }
        let claim = Option::<dto::AudioExitClaim>::read(store::required(raw, 3)?)?;
        if let Some(claim) = &claim {
            let stamp = claim.stamp()?;
            if store::device(&stamp.owner()?.write())? != owner || stamp.counter()? > seen_counter {
                return Err(StoreError::Invalid);
            }
        }
        records.push(ClaimRecord { owner, revision, claim });
    }
    let watermark = raw
        .get(&4)
        .map(Option::<dto::AudioExitClaim>::read)
        .transpose()?
        .flatten();
    if let Some(claim) = &watermark
        && oink_audio::claims::claim_order(claim)
            .map_err(|_| StoreError::Invalid)?
            .0
            .counter
            > seen_counter
    {
        return Err(StoreError::Invalid);
    }
    Ok(GroupClaims {
        seen_counter,
        records,
        watermark,
    })
}

pub fn validate_value(option: LocalOption, value: &RawValue) -> Result<(), ControlFailure> {
    use LocalOption::*;
    let bad = ControlFailure::InvalidValue;
    match option {
        MouseMode | KeyboardMode | SpeedMode => {
            if value.as_u64().map_err(invalid)? > 1 {
                return Err(bad);
            }
        }
        GameModePreference => {
            if value.as_u64().map_err(invalid)? > 2 {
                return Err(bad);
            }
        }
        ClipboardEnabled | ForwardingEnabled | KeyboardDisabled | ScreenShared | CursorTrail | AcceptContacts => {
            bool::read(value).map_err(invalid)?;
        }
        CursorTrailDuration => {
            if !(100..=60_000).contains(&value.as_u64().map_err(invalid)?) {
                return Err(bad);
            }
        }
        RemoteScrollSpeed => {
            if !(0.1..=10.0).contains(&f64::read(value).map_err(invalid)?) {
                return Err(bad);
            }
        }
        DeviceName => {
            let RawValue::Text(name) = value else { return Err(bad) };
            if name.is_empty() || name.len() > 256 {
                return Err(bad);
            }
        }
        ScreenPreferences => {
            let preferences = dto::ScreenPreferencesValue::read(value).map_err(invalid)?;
            let mut seen = BTreeSet::new();
            for screen in preferences.screens().map_err(invalid)?.0 {
                if !seen.insert(screen.display().map_err(invalid)?.0) {
                    return Err(bad);
                }
            }
        }
        InputTransform => {
            let transform = dto::InputTransformValue::read(value).map_err(invalid)?;
            for target in [
                transform.macos().map_err(invalid)?,
                transform.windows().map_err(invalid)?,
            ] {
                validate_keyboard(&target.keyboard().map_err(invalid)?)?;
            }
            let mut seen = BTreeSet::new();
            for target in transform.overrides().map_err(invalid)?.0 {
                if !seen.insert(target.target().map_err(invalid)?.ordering_key().map_err(invalid)?) {
                    return Err(bad);
                }
                if let Some(keyboard) = target.keyboard().map_err(invalid)? {
                    validate_keyboard(&keyboard)?;
                }
            }
        }
        SourceInputPolicy => {
            let policy = dto::SourceInputPolicyValue::read(value).map_err(invalid)?;
            if !matches!(
                policy.button_handoff().map_err(invalid)?,
                dto::ButtonHandoff::Block | dto::ButtonHandoff::Allow
            ) {
                return Err(bad);
            }
        }
        ForwardingControls => {
            dto::ForwardingControlsValue::read(value).map_err(invalid)?;
        }
        AudioExitClaim => {
            Option::<dto::AudioExitClaim>::read(value).map_err(invalid)?;
        }
        _ => return Err(ControlFailure::UnsupportedOption),
    }
    Ok(())
}
fn validate_keyboard(keyboard: &dto::KeyboardMapping) -> Result<(), ControlFailure> {
    if !matches!(
        keyboard.preset().map_err(invalid)?,
        dto::KeyPreset::WinFeel | dto::KeyPreset::MacFeel | dto::KeyPreset::Custom | dto::KeyPreset::None
    ) {
        return Err(ControlFailure::InvalidValue);
    }
    let supported = oink_foundation::input_transform::candidate_keys();
    let mut seen = BTreeSet::new();
    for key in keyboard.table().map_err(invalid)?.0 {
        let from = oink_foundation::input::KeyCode(key.from().map_err(invalid)?);
        let to = oink_foundation::input::KeyCode(key.to().map_err(invalid)?);
        if !supported.contains(&from) || !supported.contains(&to) || !seen.insert(from) {
            return Err(ControlFailure::InvalidValue);
        }
    }
    if let Some(caps) = keyboard.caps_tap_hold().map_err(invalid)?
        && (!supported.contains(&oink_foundation::input::KeyCode(caps.tap().map_err(invalid)?))
            || !supported.contains(&oink_foundation::input::KeyCode(caps.hold().map_err(invalid)?)))
    {
        return Err(ControlFailure::InvalidValue);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Shape {
    ScreenPreferences,
    Screen,
    Transform,
    Target,
    Override,
    Keyboard,
    Key,
    Caps,
    Policy,
    Forwarding,
}
fn apply_patch(option: LocalOption, current: &mut RawValue, patch: &RawValue) -> Result<(), ControlFailure> {
    let shape = match option {
        LocalOption::ScreenPreferences => Shape::ScreenPreferences,
        LocalOption::InputTransform => Shape::Transform,
        LocalOption::SourceInputPolicy => Shape::Policy,
        LocalOption::ForwardingControls => Shape::Forwarding,
        _ => return Err(ControlFailure::UnsupportedOption),
    };
    patch_map(current, patch, shape)
}
fn patch_map(current: &mut RawValue, patch: &RawValue, shape: Shape) -> Result<(), ControlFailure> {
    let RawValue::Map(current) = current else {
        return Err(ControlFailure::InvalidValue);
    };
    let patch = patch.as_map().map_err(invalid)?;
    for (&key, value) in patch {
        use Shape::*;
        let child = match (shape, key) {
            (ScreenPreferences, 1) => Some((Screen, true)),
            (Transform, 1 | 2) => Some((Target, false)),
            (Transform, 3) => Some((Override, true)),
            (Target, 2) | (Override, 3) => Some((Keyboard, false)),
            (Keyboard, 2) => Some((Key, true)),
            (Keyboard, 3) => Some((Caps, false)),
            _ => None,
        };
        let valid = match shape {
            ScreenPreferences | Target | Key | Caps | Forwarding => key <= 2,
            Screen | Transform | Override | Keyboard => key <= 3,
            Policy => key <= 6,
        };
        if key == 0 || !valid {
            return Err(ControlFailure::UnsupportedOption);
        }
        let optional = matches!((shape, key), (Policy, 1) | (Override, 2 | 3) | (Keyboard, 3));
        if optional && matches!(value, RawValue::Null) {
            if current.get(&key).is_some_and(|value| !matches!(value, RawValue::Null)) {
                current.remove(&key);
            }
            continue;
        }
        if let Some((shape, list)) = child {
            let entry = current.entry(key).or_insert_with(|| {
                if list {
                    RawValue::Array(Vec::new())
                } else {
                    RawValue::Map(RawMap::new())
                }
            });
            if list {
                patch_list(entry, value, shape)?;
            } else {
                patch_map(entry, value, shape)?;
            }
        } else {
            current.insert(key, value.clone());
        }
    }
    Ok(())
}
fn same_key(shape: Shape, a: &RawValue, b: &RawValue) -> bool {
    if matches!(shape, Shape::Override) {
        match (
            dto::DeviceId::read(a).and_then(|v| v.ordering_key()),
            dto::DeviceId::read(b).and_then(|v| v.ordering_key()),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    } else {
        a == b
    }
}
fn patch_list(current: &mut RawValue, patch: &RawValue, shape: Shape) -> Result<(), ControlFailure> {
    let RawValue::Array(items) = current else {
        return Err(ControlFailure::InvalidValue);
    };
    let patch = patch.as_map().map_err(invalid)?;
    if patch.keys().any(|key| !matches!(key, 1 | 2)) {
        return Err(ControlFailure::UnsupportedOption);
    }
    let removals = match patch.get(&2) {
        Some(RawValue::Array(removals)) => removals.as_slice(),
        None => &[],
        _ => return Err(ControlFailure::InvalidValue),
    };
    for (index, remove) in removals.iter().enumerate() {
        if removals[..index].iter().any(|old| same_key(shape, old, remove)) {
            return Err(ControlFailure::InvalidValue);
        }
        items.retain(|item| {
            !item
                .as_map()
                .ok()
                .and_then(|map| map.get(&1))
                .is_some_and(|key| same_key(shape, key, remove))
        });
    }
    if let Some(upserts) = patch.get(&1) {
        let RawValue::Array(upserts) = upserts else {
            return Err(ControlFailure::InvalidValue);
        };
        let mut keys = Vec::new();
        for upsert in upserts {
            let key = upsert
                .as_map()
                .map_err(invalid)?
                .get(&1)
                .ok_or(ControlFailure::InvalidValue)?;
            if keys.iter().chain(removals).any(|old| same_key(shape, old, key)) {
                return Err(ControlFailure::InvalidValue);
            }
            keys.push(key.clone());
            if let Some(existing) = items.iter_mut().find(|item| {
                item.as_map()
                    .ok()
                    .and_then(|map| map.get(&1))
                    .is_some_and(|old| same_key(shape, old, key))
            }) {
                // A keyed edit does not replace its identity object: future
                // identity metadata belongs to that same retained object.
                let mut fields = upsert.as_map().map_err(invalid)?.clone();
                fields.remove(&1);
                patch_map(existing, &RawValue::Map(fields), shape)?;
            } else {
                let mut new = RawValue::Map(RawMap::new());
                patch_map(&mut new, upsert, shape)?;
                items.push(new);
            }
        }
    }
    Ok(())
}

/// Convert an explicitly changed typed projection into the same narrow command
/// patch used by remote controls. The latest raw tree remains authoritative.
pub(crate) fn typed_patch(option: LocalOption, before: &RawValue, after: &RawValue) -> RawValue {
    let shape = match option {
        LocalOption::ScreenPreferences => Shape::ScreenPreferences,
        LocalOption::InputTransform => Shape::Transform,
        LocalOption::SourceInputPolicy => Shape::Policy,
        LocalOption::ForwardingControls => Shape::Forwarding,
        _ => return after.clone(),
    };
    diff_map(before, after, shape)
}
fn diff_map(before: &RawValue, after: &RawValue, shape: Shape) -> RawValue {
    let empty = RawMap::new();
    let before = before.as_map().unwrap_or(&empty);
    let after = after.as_map().expect("typed map");
    let mut patch = RawMap::new();
    for (&key, value) in after {
        if before.get(&key) == Some(value) {
            continue;
        }
        use Shape::*;
        let child = match (shape, key) {
            (ScreenPreferences, 1) => Some((Screen, true)),
            (Transform, 1 | 2) => Some((Target, false)),
            (Transform, 3) => Some((Override, true)),
            (Target, 2) | (Override, 3) => Some((Keyboard, false)),
            (Keyboard, 2) => Some((Key, true)),
            (Keyboard, 3) => Some((Caps, false)),
            _ => None,
        };
        let value = if let Some((shape, list)) = child {
            let old = before.get(&key).unwrap_or(&RawValue::Null);
            if list {
                diff_list(old, value, shape)
            } else {
                diff_map(old, value, shape)
            }
        } else {
            value.clone()
        };
        patch.insert(key, value);
    }
    for &key in before.keys() {
        if !after.contains_key(&key) {
            patch.insert(key, RawValue::Null);
        }
    }
    RawValue::Map(patch)
}
fn diff_list(before: &RawValue, after: &RawValue, shape: Shape) -> RawValue {
    let before = if let RawValue::Array(items) = before {
        items.as_slice()
    } else {
        &[]
    };
    let RawValue::Array(after) = after else { unreachable!() };
    let key = |value: &RawValue| {
        value
            .as_map()
            .expect("typed entry")
            .get(&1)
            .expect("stable key")
            .clone()
    };
    let removals = before
        .iter()
        .filter(|old| !after.iter().any(|new| key(old) == key(new)))
        .map(key)
        .collect();
    let upserts = after
        .iter()
        .filter_map(|new| {
            let old = before.iter().find(|old| key(old) == key(new));
            if old == Some(new) {
                return None;
            }
            let RawValue::Map(mut diff) = diff_map(old.unwrap_or(&RawValue::Null), new, shape) else {
                unreachable!()
            };
            diff.insert(1, key(new));
            Some(RawValue::Map(diff))
        })
        .collect();
    RawValue::Map([(1, RawValue::Array(upserts)), (2, RawValue::Array(removals))].into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fresh() -> OwnerStore {
        OwnerStore::new(super::super::projection::values(&super::super::Settings::default())).unwrap()
    }
    fn map(entries: impl IntoIterator<Item = (u32, RawValue)>) -> RawValue {
        RawValue::Map(entries.into_iter().collect())
    }
    fn claim(owner: DeviceId, counter: u64, claimed: bool) -> dto::AudioExitClaim {
        dto::AudioExitClaim::from_raw(
            [
                (1, map([(1, counter.write()), (2, store::wire_device(owner).write())])),
                (2, claimed.write()),
            ]
            .into(),
        )
        .unwrap()
    }
    #[test]
    fn composite_commands_preserve_unknown_paths_and_remove_only_explicit_keys() {
        let mut owner = fresh();
        let option = LocalOption::ScreenPreferences;
        let mut item = owner.item(option).unwrap().into_raw();
        item.insert(100, RawValue::Text("envelope extension".into()));
        item.insert(
            3,
            map([
                (
                    1,
                    RawValue::Array(vec![map([
                        (1, RawValue::Text("screen".into())),
                        (2, 1.0f64.write()),
                        (3, false.write()),
                        (100, u64::MAX.write()),
                    ])]),
                ),
                (2, 1.0f64.write()),
                (100, map([(1, RawValue::Bytes(vec![1, 2, 3]))])),
            ]),
        );
        owner.set_item(DevicePreference::from_raw(item).unwrap());
        let patch = map([(
            1,
            map([(
                1,
                RawValue::Array(vec![map([(1, RawValue::Text("screen".into())), (2, 2.0f64.write())])]),
            )]),
        )]);
        let edited = owner.edit(option, 1, patch).unwrap();
        assert_eq!(edited.revision().unwrap(), 2);
        assert!(edited.raw().contains_key(&100));
        let value = edited.value().unwrap();
        let value = value.as_map().unwrap();
        assert!(value.contains_key(&100));
        let RawValue::Array(screens) = &value[&1] else {
            unreachable!()
        };
        let screen = screens[0].as_map().unwrap();
        assert_eq!(screen[&2], 2.0f64.write());
        assert_eq!(screen[&3], false.write());
        assert_eq!(screen[&100], u64::MAX.write());
        let before = owner.clone();
        assert_eq!(
            owner.edit(option, 2, map([(2, 6.0f64.write())])),
            Err(ControlFailure::InvalidValue)
        );
        assert_eq!(owner, before);
        owner
            .edit(
                option,
                2,
                map([(1, map([(2, RawValue::Array(vec![RawValue::Text("screen".into())]))]))]),
            )
            .unwrap();
        assert_eq!(
            dto::ScreenPreferencesValue::read(&owner.value(option).unwrap())
                .unwrap()
                .screens()
                .unwrap()
                .0
                .len(),
            0
        );
    }
    #[test]
    fn keyboard_startup_release_advances_revision_even_if_already_enabled() {
        let mut owner = fresh();
        let released = owner.clear_keyboard_on_start().unwrap();
        assert_eq!(released.revision().unwrap(), 2);
        assert_eq!(released.value().unwrap(), false.write());
        owner.edit(LocalOption::KeyboardDisabled, 2, true.write()).unwrap();
        assert_eq!(owner.clear_keyboard_on_start().unwrap().revision().unwrap(), 4);
        assert_eq!(owner.value(LocalOption::KeyboardDisabled), Some(false.write()));
    }
    #[test]
    fn keyed_identity_extensions_survive_and_conflicting_operations_fail() {
        let mut owner = fresh();
        let option = LocalOption::InputTransform;
        let mut value = owner.value(option).unwrap().as_map().unwrap().clone();
        let mut identity = store::wire_device(DeviceId([8; 32])).into_raw();
        identity.insert(99, RawValue::Text("future identity".into()));
        value.insert(
            3,
            RawValue::Array(vec![map([(1, RawValue::Map(identity.clone())), (2, false.write())])]),
        );
        owner.replace_value(option, 1, RawValue::Map(value)).unwrap();
        let key = store::wire_device(DeviceId([8; 32])).write();
        let edited = owner
            .edit(
                option,
                2,
                map([(
                    3,
                    map([(1, RawValue::Array(vec![map([(1, key.clone()), (2, true.write())])]))]),
                )]),
            )
            .unwrap();
        let transform = dto::InputTransformValue::read(&edited.value().unwrap()).unwrap();
        assert_eq!(transform.overrides().unwrap().0[0].target().unwrap().raw(), &identity);
        let before = owner.clone();
        assert_eq!(
            owner.edit(
                option,
                3,
                map([(
                    3,
                    map([
                        (1, RawValue::Array(vec![map([(1, key.clone()), (2, false.write())])])),
                        (2, RawValue::Array(vec![key]))
                    ])
                )])
            ),
            Err(ControlFailure::InvalidValue)
        );
        assert_eq!(owner, before);
        let policy = LocalOption::SourceInputPolicy;
        assert_eq!(
            owner
                .edit(policy, 1, map([(1, RawValue::Null)]))
                .unwrap()
                .revision()
                .unwrap(),
            1
        );
    }
    #[test]
    fn unknown_options_and_full_width_revisions_round_trip() {
        let mut owner = fresh();
        owner.raw.insert(100, RawValue::Uint(u64::MAX));
        let RawValue::Array(items) = owner.raw.get_mut(&2).unwrap() else {
            unreachable!()
        };
        items.push(map([
            (1, 99u32.write()),
            (2, u64::MAX.write()),
            (3, map([(91, RawValue::Bytes(vec![4, 5, 6]))])),
        ]));
        let bytes = owner.encode().unwrap();
        let mut loaded = OwnerStore::decode(&bytes).unwrap();
        assert_eq!(loaded.encode().unwrap(), bytes);
        assert_eq!(
            loaded.edit(LocalOption::Unknown(99), u64::MAX, RawValue::Bool(false)),
            Err(ControlFailure::UnsupportedOption)
        );
        let mut item = loaded.item(LocalOption::ClipboardEnabled).unwrap().into_raw();
        item.insert(2, u64::MAX.write());
        loaded.set_item(DevicePreference::from_raw(item).unwrap());
        let before = loaded.clone();
        assert_eq!(
            loaded.edit(LocalOption::ClipboardEnabled, u64::MAX, false.write()),
            Err(ControlFailure::StateConflict)
        );
        assert_eq!(loaded, before);
    }
    #[test]
    fn group_switch_and_claim_records_commit_one_projection_without_losing_history() {
        let mut owner = fresh();
        let me = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let a = GroupId([3; 16]);
        let b = GroupId([4; 16]);
        owner.switch_group(a, me).unwrap();
        assert_eq!(owner.revision(LocalOption::AudioExitClaim), Some(2));
        assert_eq!(
            owner.claims(a).records[0],
            ClaimRecord {
                owner: me,
                revision: 2,
                claim: None
            }
        );
        let claims = GroupClaims {
            watermark: None,
            seen_counter: u64::MAX,
            records: vec![
                ClaimRecord {
                    owner: me,
                    revision: 3,
                    claim: Some(claim(me, 4, true)),
                },
                ClaimRecord {
                    owner: peer,
                    revision: 8,
                    claim: Some(claim(peer, u64::MAX, false)),
                },
            ],
        };
        owner.commit_claims(a, me, claims.clone()).unwrap();
        assert_eq!(owner.revision(LocalOption::AudioExitClaim), Some(3));
        assert_eq!(
            owner.value(LocalOption::AudioExitClaim),
            claims.records[0].claim.as_ref().map(WireValue::write)
        );
        owner.switch_group(b, me).unwrap();
        assert_eq!(owner.value(LocalOption::AudioExitClaim), Some(RawValue::Null));
        assert_eq!(owner.claims(a), claims);
        assert_eq!(owner.claims(b).seen_counter, 0);
        owner.switch_group(a, me).unwrap();
        assert_eq!(owner.revision(LocalOption::AudioExitClaim), Some(5));
        let restored = OwnerStore::decode(&owner.encode().unwrap()).unwrap();
        assert_eq!(restored, owner);
        assert_eq!(restored.claims(a).seen_counter, u64::MAX);
        assert_eq!(restored.claims(a).records[0].revision, 5);
        let returned = restored.claims(a).records[0].claim.clone().unwrap();
        assert!(returned.claimed().unwrap());
        assert_eq!(returned.stamp().unwrap().counter().unwrap(), 4);
        let mut invalid = restored.claims(a);
        invalid.records.pop();
        assert!(owner.commit_claims(a, me, invalid).is_err());
        assert_eq!(owner, restored);
    }
    #[test]
    fn relayed_ordering_witness_is_durable_group_local_and_cannot_regress() {
        let mut owner = fresh();
        let me = DeviceId([1; 32]);
        let removed = DeviceId([9; 32]);
        let a = GroupId([3; 16]);
        let b = GroupId([4; 16]);
        owner.switch_group(a, me).unwrap();
        let mut records = owner.claims(a);
        records.seen_counter = 8;
        records.watermark = Some(claim(removed, 8, false));
        owner.commit_claims(a, me, records.clone()).unwrap();
        let mut restored = OwnerStore::decode(&owner.encode().unwrap()).unwrap();
        assert_eq!(restored.claims(a), records);
        assert!(restored.claims(a).records.iter().all(|record| record.owner != removed));
        restored.switch_group(b, me).unwrap();
        assert_eq!(restored.claims(b).watermark(), None);
        assert_eq!(restored.claims(b).seen_counter, 0);
        restored.switch_group(a, me).unwrap();
        assert_eq!(restored.claims(a).watermark(), records.watermark);
        let mut regressed = restored.claims(a);
        regressed.watermark = Some(claim(removed, 8, true));
        assert!(restored.commit_claims(a, me, regressed).is_err());
    }
    #[test]
    fn group_switch_restores_saved_withdrawal_without_importing_source_claim() {
        let mut owner = fresh();
        let me = DeviceId([1; 32]);
        let a = GroupId([3; 16]);
        let b = GroupId([4; 16]);
        owner.switch_group(a, me).unwrap();
        let withdrawn = claim(me, 9, false);
        owner
            .commit_claims(
                a,
                me,
                GroupClaims {
                    watermark: None,
                    seen_counter: 9,
                    records: vec![ClaimRecord {
                        owner: me,
                        revision: 3,
                        claim: Some(withdrawn.clone()),
                    }],
                },
            )
            .unwrap();
        owner.switch_group(b, me).unwrap();
        owner
            .commit_claims(
                b,
                me,
                GroupClaims {
                    watermark: None,
                    seen_counter: 20,
                    records: vec![ClaimRecord {
                        owner: me,
                        revision: 5,
                        claim: Some(claim(me, 20, true)),
                    }],
                },
            )
            .unwrap();
        let source = owner.claims(b);
        owner.switch_group(a, me).unwrap();
        assert_eq!(owner.value(LocalOption::AudioExitClaim), Some(withdrawn.write()));
        assert_eq!(owner.revision(LocalOption::AudioExitClaim), Some(6));
        assert_eq!(owner.claims(a).seen_counter, 9);
        assert_eq!(owner.claims(b), source);
        let restored = OwnerStore::decode(&owner.encode().unwrap()).unwrap();
        assert_eq!(restored, owner);
    }
    #[test]
    fn group_record_updates_keep_retained_owner_identity_extensions() {
        let mut owner = fresh();
        let me = DeviceId([1; 32]);
        let group = GroupId([3; 16]);
        owner.switch_group(group, me).unwrap();
        let RawValue::Array(groups) = owner.raw.get_mut(&3).unwrap() else {
            unreachable!()
        };
        let RawValue::Map(group_raw) = &mut groups[0] else {
            unreachable!()
        };
        let RawValue::Array(records) = group_raw.get_mut(&3).unwrap() else {
            unreachable!()
        };
        let RawValue::Map(record) = &mut records[0] else {
            unreachable!()
        };
        let RawValue::Map(id) = record.get_mut(&1).unwrap() else {
            unreachable!()
        };
        id.insert(99, RawValue::Bytes(vec![7, 8]));
        let mut records = owner.claims(group);
        records.seen_counter = 1;
        records.records[0].revision += 1;
        records.records[0].claim = Some(claim(me, 1, true));
        owner.commit_claims(group, me, records).unwrap();
        let restored = OwnerStore::decode(&owner.encode().unwrap()).unwrap();
        let record = &array(restored.group_raw(group).unwrap().get(&3).unwrap()).unwrap()[0];
        assert_eq!(
            record.as_map().unwrap()[&1].as_map().unwrap()[&99],
            RawValue::Bytes(vec![7, 8])
        );
    }
    #[test]
    fn local_typed_composite_changes_produce_narrow_keyed_patches() {
        let mut owner = fresh();
        let mut before = super::super::Settings::default();
        before.screen_preferences.screens.push(super::super::ScreenPreference {
            display: "screen".into(),
            gain: 1.0,
            enabled: false,
        });
        let value = super::super::projection::values(&before)
            .into_iter()
            .find(|(option, _)| *option == LocalOption::ScreenPreferences)
            .unwrap()
            .1;
        owner
            .replace_value(LocalOption::ScreenPreferences, 1, value.clone())
            .unwrap();
        let mut after = before.clone();
        after.screen_preferences.screens[0].gain = 3.0;
        let changed = super::super::projection::values(&after)
            .into_iter()
            .find(|(option, _)| *option == LocalOption::ScreenPreferences)
            .unwrap()
            .1;
        let patch = typed_patch(LocalOption::ScreenPreferences, &value, &changed);
        assert!(patch.as_map().unwrap().get(&2).is_none());
        owner.edit(LocalOption::ScreenPreferences, 2, patch).unwrap();
        assert_eq!(owner.value(LocalOption::ScreenPreferences), Some(changed));
    }
}
