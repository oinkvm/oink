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
    /// New optional device settings acquire an owner-authored baseline on
    /// upgrade. Existing (including unknown) owner values are never replaced.
    pub(crate) fn ensure_option(&mut self, option: LocalOption, value: RawValue) -> Result<bool, StoreError> {
        if self.item(option).is_some() {
            return Ok(false);
        }
        validate_value(option, &value).map_err(|_| StoreError::Invalid)?;
        let mut items = self.stored_preferences();
        items.push(DevicePreference::from_raw(
            [(1, option.write()), (2, 1u64.write()), (3, value)].into(),
        )?);
        items.sort_by_key(|item| item.option().expect("validated option").number());
        self.raw
            .insert(2, RawValue::Array(items.iter().map(WireValue::write).collect()));
        Ok(true)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
        Self::from_raw(oink_protocol::v1::cbor::decode_map(bytes, MAX_OWNER_BYTES)?)
    }
    pub fn encode(&self) -> Result<Vec<u8>, StoreError> {
        let mut raw = self.raw.clone();
        raw.insert(
            2,
            RawValue::Array(self.stored_preferences().iter().map(WireValue::write).collect()),
        );
        Ok(oink_protocol::v1::cbor::encode_map(&raw, MAX_OWNER_BYTES)?)
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
            if let Some(microphone) = map.get(&5) {
                parse_claims(microphone.as_map()?)?;
            }
        }
        if let Some(group) = raw.get(&4) {
            store::group(group)?;
        }
        Ok(Self { raw })
    }
    fn stored_preferences(&self) -> Vec<DevicePreference> {
        store::entries(&self.raw)
            .expect("validated items")
            .iter()
            .map(|item| DevicePreference::read(item).expect("validated preference"))
            // Option 5 is retired locally. A stale saved guard is never authority.
            .filter(|item| item.option() != Ok(LocalOption::KeyboardDisabled))
            .collect()
    }
    /// Frozen v1 projection: Disabled is represented by the original bool option,
    /// never by sending the local-only KeyboardMode value 2 to an old peer.
    pub fn preferences(&self) -> Vec<DevicePreference> {
        let mut items: Vec<_> = self
            .stored_preferences()
            .into_iter()
            .map(|item| self.wire_item(item))
            .collect();
        if let Some(item) = self.item(LocalOption::KeyboardDisabled) {
            items.push(item);
        }
        items.sort_by_key(|item| item.option().expect("validated option").number());
        items
    }
    fn wire_item(&self, item: DevicePreference) -> DevicePreference {
        if item.option() == Ok(LocalOption::KeyboardMode) && item.value() == Ok(2u32.write()) {
            let mut raw = item.into_raw();
            raw.insert(3, 0u32.write());
            DevicePreference::from_raw(raw).expect("valid shared keyboard projection")
        } else {
            item
        }
    }
    pub fn item(&self, option: LocalOption) -> Option<DevicePreference> {
        if option == LocalOption::KeyboardDisabled {
            let mode = self.item(LocalOption::KeyboardMode)?;
            return DevicePreference::from_raw(
                [
                    (1, option.write()),
                    (2, mode.revision().ok()?.write()),
                    (3, (mode.value().ok()? == 2u32.write()).write()),
                ]
                .into(),
            )
            .ok();
        }
        self.stored_preferences()
            .into_iter()
            .find(|item| item.option() == Ok(option))
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
        // Compatibility command only: no separate local guard or guard revision.
        if option == LocalOption::KeyboardDisabled {
            let disabled = bool::read(&patch).map_err(invalid)?;
            let value = if disabled {
                2u32.write()
            } else if self.value(LocalOption::KeyboardMode) == Some(2u32.write()) {
                0u32.write()
            } else {
                self.value(LocalOption::KeyboardMode)
                    .ok_or(ControlFailure::UnsupportedOption)?
            };
            self.edit(LocalOption::KeyboardMode, expected, value)?;
            return self.item(option).ok_or(ControlFailure::UnsupportedOption);
        }
        if matches!(option, LocalOption::AudioExitClaim | LocalOption::MicrophoneShareClaim) {
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
                | LocalOption::MicrophoneControls
        ) {
            apply_patch(option, &mut value, &patch)?;
        } else {
            value = patch;
        }
        validate_value(option, &value)?;
        let item = self.replace_value(option, expected, value)?;
        if matches!(option, LocalOption::TouchpadInjection | LocalOption::NativeMultifinger)
            && let Some(revision) = self.revision(LocalOption::AcceptContacts)
        {
            // New local and remote edits project to the same released fallback.
            // An old Mac source still selects today's raw-or-mapped path, never
            // the new plain-wheel or scroll-only classifiers.
            let enabled = [LocalOption::TouchpadInjection, LocalOption::NativeMultifinger]
                .into_iter()
                .all(|option| self.value(option) == Some(RawValue::Bool(true)));
            self.replace_value(LocalOption::AcceptContacts, revision, RawValue::Bool(enabled))?;
        }
        Ok(item)
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
    /// Both source modes use the same durable Disabled -> Shared release before
    /// startup, on pause, and on the physical release chord. Local stays Local.
    pub fn clear_input_disables_on_start(&mut self) -> Result<(), StoreError> {
        self.release_input_disables()
    }
    pub fn release_input_disables(&mut self) -> Result<(), StoreError> {
        for option in [LocalOption::KeyboardMode, LocalOption::MouseMode] {
            let item = self.item(option).ok_or(StoreError::Invalid)?;
            if item.value()? == 2u32.write() {
                self.edit(option, item.revision()?, 0u32.write())
                    .map_err(|_| StoreError::Invalid)?;
            }
        }
        Ok(())
    }
    pub fn active_group(&self) -> Option<GroupId> {
        self.raw.get(&4).and_then(|value| store::group(value).ok())
    }
    pub fn claims(&self, group: GroupId) -> GroupClaims {
        self.claims_for(group, LocalOption::AudioExitClaim)
    }
    pub fn microphone_claims(&self, group: GroupId) -> GroupClaims {
        self.claims_for(group, LocalOption::MicrophoneShareClaim)
    }
    fn claims_for(&self, group: GroupId, option: LocalOption) -> GroupClaims {
        self.group_raw(group)
            .and_then(|raw| {
                if option == LocalOption::MicrophoneShareClaim {
                    raw.get(&5).and_then(|v| v.as_map().ok())
                } else {
                    Some(raw)
                }
            })
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
        candidate.raw.insert(4, RawValue::Bytes(group.0.to_vec()));
        for option in [LocalOption::AudioExitClaim, LocalOption::MicrophoneShareClaim] {
            let Some(item) = candidate.item(option) else { continue };
            let revision = item.revision()?.checked_add(1).ok_or(StoreError::LimitExceeded)?;
            let mut claims = candidate.claims_for(group, option);
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
            candidate.commit_claims_for(group, owner, claims, option)?;
        }
        *self = candidate;
        Ok(())
    }
    /// A verified group merge carries active local roles into the new group.
    /// Ordinary group switches still restore that group's independent history.
    /// Each carried claim gets a fresh ordered stamp, so concurrent merge
    /// participants converge and the losing local owner durably yields.
    pub fn merge_group(&mut self, group: GroupId, owner: DeviceId) -> Result<(), StoreError> {
        if self.active_group() == Some(group) {
            return Ok(());
        }
        let mut candidate = self.clone();
        candidate.switch_group(group, owner)?;
        for option in [LocalOption::AudioExitClaim, LocalOption::MicrophoneShareClaim] {
            let Some(item) = self.item(option) else { continue };
            let claim = Option::<dto::AudioExitClaim>::read(&item.value()?)?;
            if !claim.is_some_and(|c| c.claimed() == Ok(true)) {
                continue;
            }
            let previous = self
                .active_group()
                .map(|g| self.claims_for(g, option))
                .unwrap_or_default();
            let mut claims = candidate.claims_for(group, option);
            let counter = claims
                .seen_counter
                .max(previous.seen_counter)
                .checked_add(1)
                .ok_or(StoreError::LimitExceeded)?;
            let stamp = oink_audio::claims::Stamp { counter, owner };
            let claim = dto::AudioExitClaim::from_raw([(1, stamp.to_wire().write()), (2, true.write())].into())?;
            let revision = candidate.revision(option).ok_or(StoreError::Invalid)?;
            let local = claims
                .records
                .iter_mut()
                .find(|r| r.owner == owner)
                .ok_or(StoreError::Invalid)?;
            local.claim = Some(claim.clone());
            // switch_group already advances the local projection once; this is
            // one atomic transition, never an intermediate published baseline.
            local.revision = revision.checked_add(1).ok_or(StoreError::LimitExceeded)?;
            claims.seen_counter = counter;
            claims.watermark = Some(claim);
            candidate.commit_claims_for(group, owner, claims, option)?;
        }
        *self = candidate;
        Ok(())
    }
    /// Persist the pure audio owner's proposed authenticated records together
    /// with option20. Removed-member records and group history are never pruned.
    pub fn commit_claims(&mut self, group: GroupId, owner: DeviceId, claims: GroupClaims) -> Result<(), StoreError> {
        self.commit_claims_for(group, owner, claims, LocalOption::AudioExitClaim)
    }
    pub fn commit_microphone_claims(
        &mut self,
        group: GroupId,
        owner: DeviceId,
        claims: GroupClaims,
    ) -> Result<(), StoreError> {
        self.commit_claims_for(group, owner, claims, LocalOption::MicrophoneShareClaim)
    }
    /// Release and receiver unmute share one retained-store transaction.
    pub fn release_microphone_claim(
        &mut self,
        group: GroupId,
        owner: DeviceId,
        claims: GroupClaims,
    ) -> Result<(), StoreError> {
        let mut candidate = self.clone();
        candidate.commit_microphone_claims(group, owner, claims)?;
        let option = LocalOption::MicrophoneReceiveMuted;
        let revision = candidate.revision(option).ok_or(StoreError::Invalid)?;
        candidate
            .edit(option, revision, false.write())
            .map_err(|_| StoreError::Invalid)?;
        *self = candidate;
        Ok(())
    }
    fn commit_claims_for(
        &mut self,
        group: GroupId,
        owner: DeviceId,
        claims: GroupClaims,
        option: LocalOption,
    ) -> Result<(), StoreError> {
        let previous = self.claims_for(group, option);
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
        let mut outer = self.group_raw(group).cloned().unwrap_or_default();
        let mut map = if option == LocalOption::MicrophoneShareClaim {
            outer
                .get(&5)
                .map(|v| v.as_map().cloned())
                .transpose()?
                .unwrap_or_default()
        } else {
            outer.clone()
        };
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
        if option == LocalOption::MicrophoneShareClaim {
            outer.entry(1).or_insert_with(|| RawValue::Bytes(group.0.to_vec()));
            outer.entry(2).or_insert_with(|| 0u64.write());
            outer.entry(3).or_insert_with(|| RawValue::Array(Vec::new()));
            outer.entry(4).or_insert(RawValue::Null);
            outer.insert(5, RawValue::Map(map));
            map = outer;
        }
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
            let mut item = candidate.item(option).ok_or(StoreError::Invalid)?.into_raw();
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
        MouseMode | KeyboardMode => {
            if value.as_u64().map_err(invalid)? > 2 {
                return Err(bad);
            }
        }
        SpeedMode => {
            if value.as_u64().map_err(invalid)? > 1 {
                return Err(bad);
            }
        }
        GameModePreference => {
            if value.as_u64().map_err(invalid)? > 2 {
                return Err(bad);
            }
        }
        ClipboardEnabled
        | ForwardingEnabled
        | KeyboardDisabled
        | ScreenShared
        | CursorTrail
        | AcceptContacts
        | TouchpadInjection
        | NativeMultifinger
        | MacScrollFeel
        | SyncSystemArrangement
        | MicrophoneReceiveMuted => {
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
        ForwardingControls | MicrophoneControls => {
            dto::ForwardingControlsValue::read(value).map_err(invalid)?;
        }
        MicrophoneInput => {
            let input = Option::<oink_protocol::v1::schema::Text<512>>::read(value).map_err(invalid)?;
            oink_audio::validate_receive_output(input.as_ref().map(|v| v.0.as_str())).map_err(|_| bad)?;
        }
        AudioExitClaim | MicrophoneShareClaim => {
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
        if !supported.contains(&from)
            || (to != oink_foundation::input::KeyCode::NONE && !supported.contains(&to))
            || !seen.insert(from)
        {
            return Err(ControlFailure::InvalidValue);
        }
    }
    if let Some(caps) = keyboard.caps_tap_hold().map_err(invalid)?
        && [caps.tap().map_err(invalid)?, caps.hold().map_err(invalid)?]
            .iter()
            .any(|code| *code != 0 && !supported.contains(&oink_foundation::input::KeyCode(*code)))
    {
        return Err(ControlFailure::InvalidValue);
    }
    if let Some(saved) = keyboard.saved_custom().map_err(invalid)? {
        let mut sources = BTreeSet::new();
        for key in saved.table().map_err(invalid)?.0 {
            let from = oink_foundation::input::KeyCode(key.from().map_err(invalid)?);
            let to = oink_foundation::input::KeyCode(key.to().map_err(invalid)?);
            if !supported.contains(&from) || (to.0 != 0 && !supported.contains(&to)) || !sources.insert(from) {
                return Err(ControlFailure::InvalidValue);
            }
        }
        let hold = saved.caps_hold().map_err(invalid)?;
        if hold.is_some_and(|code| code != 0 && !supported.contains(&oink_foundation::input::KeyCode(code)))
            || (saved.caps_split().map_err(invalid)? && hold.is_none())
            || (saved.allow_fn().map_err(invalid)? && !sources.contains(&oink_foundation::input::KeyCode::APPLE_FN))
        {
            return Err(ControlFailure::InvalidValue);
        }
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
    SavedCustom,
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
        LocalOption::ForwardingControls | LocalOption::MicrophoneControls => Shape::Forwarding,
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
            (Keyboard, 4) => Some((SavedCustom, false)),
            (SavedCustom, 1) => Some((Key, true)),
            _ => None,
        };
        let valid = match shape {
            ScreenPreferences | Target | Key | Caps | Forwarding => key <= 2,
            Screen | Keyboard | SavedCustom => key <= 4,
            Transform | Override => key <= 3,
            Policy => key <= 6,
        };
        if key == 0 || !valid {
            return Err(ControlFailure::UnsupportedOption);
        }
        let optional = matches!(
            (shape, key),
            (Policy, 1) | (Override, 2 | 3) | (Keyboard, 3 | 4) | (SavedCustom, 2)
        );
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
        LocalOption::ForwardingControls | LocalOption::MicrophoneControls => Shape::Forwarding,
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
            (Keyboard, 4) => Some((SavedCustom, false)),
            (SavedCustom, 1) => Some((Key, true)),
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
    fn keyboard_and_mouse_modes_share_runtime_and_release_lifecycle() {
        for option in [LocalOption::KeyboardMode, LocalOption::MouseMode] {
            for initial in [0u32, 1u32] {
                let mut owner = fresh();
                owner.edit(option, 1, initial.write()).unwrap();
                let revision = owner.revision(option).unwrap();
                owner.edit(option, revision, 2u32.write()).unwrap();
                assert_eq!(owner.value(option), Some(2u32.write()));
                let saved = owner.encode().unwrap();
                let mut restored = OwnerStore::decode(&saved).unwrap();
                assert_eq!(restored.value(option), Some(2u32.write()));
                assert!(
                    restored
                        .stored_preferences()
                        .iter()
                        .all(|item| item.option() != Ok(LocalOption::KeyboardDisabled))
                );
                restored.release_input_disables().unwrap();
                assert_eq!(restored.value(option), Some(0u32.write()));
                let revision = restored.revision(option).unwrap();
                restored.edit(option, revision, 2u32.write()).unwrap();
                restored.edit(option, revision + 1, 0u32.write()).unwrap();
                assert_eq!(restored.value(option), Some(0u32.write()));
            }
        }
    }
    #[test]
    fn frozen_keyboard_guard_commands_and_facts_are_boundary_projections() {
        let mut owner = fresh();
        owner.edit(LocalOption::KeyboardMode, 1, 1u32.write()).unwrap();
        let revision = owner.revision(LocalOption::KeyboardMode).unwrap();
        let disabled = owner
            .edit(LocalOption::KeyboardDisabled, revision, true.write())
            .unwrap();
        assert_eq!(owner.value(LocalOption::KeyboardMode), Some(2u32.write()));
        assert_eq!(disabled.value().unwrap(), true.write());
        let preferences = owner.preferences();
        assert_eq!(
            preferences
                .iter()
                .find(|p| p.option() == Ok(LocalOption::KeyboardMode))
                .unwrap()
                .value()
                .unwrap(),
            0u32.write()
        );
        assert_eq!(
            preferences
                .iter()
                .find(|p| p.option() == Ok(LocalOption::KeyboardDisabled))
                .unwrap(),
            &disabled
        );
        assert_eq!(
            owner.edit(LocalOption::KeyboardDisabled, revision, false.write()),
            Err(ControlFailure::StaleRevision)
        );
        owner
            .edit(LocalOption::KeyboardDisabled, revision + 1, false.write())
            .unwrap();
        assert_eq!(owner.value(LocalOption::KeyboardMode), Some(0u32.write()));
        assert_eq!(owner.value(LocalOption::KeyboardDisabled), Some(false.write()));
    }
    #[test]
    fn stale_saved_keyboard_guard_is_ignored_and_is_not_saved_again() {
        let mut owner = fresh();
        owner.edit(LocalOption::KeyboardMode, 1, 1u32.write()).unwrap();
        let RawValue::Array(items) = owner.raw.get_mut(&2).unwrap() else {
            unreachable!()
        };
        items.push(
            DevicePreference::from_raw(
                [
                    (1, LocalOption::KeyboardDisabled.write()),
                    (2, 42u64.write()),
                    (3, true.write()),
                ]
                .into(),
            )
            .unwrap()
            .write(),
        );
        items.sort_by_key(|item| DevicePreference::read(item).unwrap().option().unwrap().number());
        let bytes = oink_protocol::v1::cbor::encode_map(&owner.raw, MAX_OWNER_BYTES).unwrap();
        let restored = OwnerStore::decode(&bytes).unwrap();
        assert_eq!(restored.value(LocalOption::KeyboardMode), Some(1u32.write()));
        assert_eq!(restored.value(LocalOption::KeyboardDisabled), Some(false.write()));
        let raw = oink_protocol::v1::cbor::decode_map(&restored.encode().unwrap(), MAX_OWNER_BYTES).unwrap();
        assert!(
            store::entries(&raw)
                .unwrap()
                .iter()
                .all(|item| DevicePreference::read(item).unwrap().option() != Ok(LocalOption::KeyboardDisabled))
        );
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
        let patch = map([(
            1,
            map([(
                1,
                RawValue::Array(vec![map([(1, RawValue::Text("screen".into())), (4, 2.5f64.write())])]),
            )]),
        )]);
        let edited = owner.edit(option, 2, patch).unwrap();
        let value = dto::ScreenPreferencesValue::read(&edited.value().unwrap()).unwrap();
        let screen = &value.screens().unwrap().0[0];
        assert_eq!(screen.remote_gain().unwrap(), 2.5);
        assert_eq!(screen.gain().unwrap(), 2.0);
        assert_eq!(screen.raw()[&100], u64::MAX.write());
        let before = owner.clone();
        assert_eq!(
            owner.edit(option, 3, map([(2, 6.0f64.write())])),
            Err(ControlFailure::InvalidValue)
        );
        assert_eq!(owner, before);
        owner
            .edit(
                option,
                3,
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
    fn microphone_release_unmutes_receiver_in_same_retained_projection() {
        let mut owner = fresh();
        let me = DeviceId([1; 32]);
        let group = GroupId([3; 16]);
        owner.switch_group(group, me).unwrap();
        let option = LocalOption::MicrophoneReceiveMuted;
        owner
            .edit(option, owner.revision(option).unwrap(), true.write())
            .unwrap();
        let muted_revision = owner.revision(option).unwrap();
        let mut claims = owner.microphone_claims(group);
        claims.seen_counter = 1;
        claims.records = vec![ClaimRecord {
            owner: me,
            revision: owner.revision(LocalOption::MicrophoneShareClaim).unwrap() + 1,
            claim: Some(claim(me, 1, true)),
        }];
        owner.commit_microphone_claims(group, me, claims.clone()).unwrap();
        claims.seen_counter = 2;
        claims.records[0].revision += 1;
        claims.records[0].claim = Some(claim(me, 2, false));
        owner.release_microphone_claim(group, me, claims.clone()).unwrap();
        let loaded = OwnerStore::decode(&owner.encode().unwrap()).unwrap();
        assert_eq!(loaded.value(option), Some(false.write()));
        assert_eq!(loaded.revision(option), Some(muted_revision + 1));
        assert_eq!(loaded.microphone_claims(group), claims);
        // Releasing an already unmuted receiver never adds an extra mute revision.
        owner.release_microphone_claim(group, me, claims).unwrap();
        assert_eq!(owner.revision(option), Some(muted_revision + 1));
    }
    #[test]
    fn group_merge_carries_both_roles_without_crossing_their_claim_namespaces() {
        let mut owner = fresh();
        let me = DeviceId([1; 32]);
        let a = GroupId([3; 16]);
        let b = GroupId([4; 16]);
        owner.switch_group(a, me).unwrap();
        for option in [LocalOption::AudioExitClaim, LocalOption::MicrophoneShareClaim] {
            let mut claims = owner.claims_for(a, option);
            let revision = owner.revision(option).unwrap() + 1;
            claims.seen_counter = 7;
            claims.records = vec![ClaimRecord {
                owner: me,
                revision,
                claim: Some(claim(me, 7, true)),
            }];
            owner.commit_claims_for(a, me, claims, option).unwrap();
        }
        let original = owner.clone();
        owner.merge_group(b, me).unwrap();
        for option in [LocalOption::AudioExitClaim, LocalOption::MicrophoneShareClaim] {
            assert_eq!(owner.claims_for(a, option), original.claims_for(a, option));
            let records = owner.claims_for(b, option);
            assert_eq!(records.seen_counter, 8);
            assert!(records.records[0].claim.as_ref().unwrap().claimed().unwrap());
        }
        let microphone = owner.microphone_claims(b);
        let mut speaker = owner.claims(b);
        speaker.seen_counter = 9;
        speaker.records[0].revision += 1;
        speaker.records[0].claim = Some(claim(me, 9, false));
        owner.commit_claims(b, me, speaker).unwrap();
        assert_eq!(owner.microphone_claims(b), microphone);
        assert_eq!(
            OwnerStore::decode(&owner.encode().unwrap())
                .unwrap()
                .microphone_claims(b),
            microphone
        );
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
            remote_gain: 1.0,
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
    #[test]
    fn owner_accepts_empty_destinations_but_never_empty_sources() {
        let mapping = |from: u64, to: u64| {
            dto::KeyboardMapping::from_raw(
                [
                    (1, RawValue::Uint(2)),
                    (
                        2,
                        RawValue::Array(vec![map([(1, RawValue::Uint(from)), (2, RawValue::Uint(to))])]),
                    ),
                    (3, map([(1, RawValue::Uint(0)), (2, RawValue::Uint(0))])),
                ]
                .into(),
            )
            .unwrap()
        };
        assert!(validate_keyboard(&mapping(0xff0003, 0)).is_ok());
        assert!(validate_keyboard(&mapping(0, 0x700e0)).is_err());
    }
}
