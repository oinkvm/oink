//! Device-authored preferences. Only this store advances local revisions; peer
//! facts are never merged into its items. This device's own audio
//! configuration (exit / sender / forward / receive) is stored device-level.
use super::store::{self, StoreError};
use oink_audio::route::{Flag, OwnConfig};
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v2::{
    RawMap, RawValue,
    dto::{self, ControlFailure, DevicePreference, LocalOption},
    schema::{WireDto, WireValue},
};
use std::collections::BTreeSet;

pub const MAX_OWNER_BYTES: usize = 4 * 1024 * 1024;
const RETIRED_EXIT_CLAIM: u32 = 20;
const RETIRED_MICROPHONE_CLAIM: u32 = 22;
/// Retired LocalOption 4 (ForwardingEnabled) and 24 (MicrophoneReceiveMuted):
/// kept as saved, never published or edited; read once as the initial
/// forward / receive switches when key 5 does not exist yet.
const RETIRED_FORWARDING: u32 = 4;
const RETIRED_RECEIVE_MUTED: u32 = 24;
const AUDIO_KEY: u32 = 5;

/// CBOR map: 1 schema, 2 ordered DevicePreference list, 3 group records,
/// 4 active group (optional), 5 this device's own audio configuration
/// (optional): {1 speaker exit: bool, 2 microphone sender: bool, 3 forward:
/// AudioFlag, 4 receive: AudioFlag, 5 HLC last: u64}. Each group: 1 group ID.
/// Group keys 2-5 held the per-owner exit claims of earlier builds and key 6
/// the per-group route setting of early 0.4.0 builds; like all unknown keys
/// they are retained and never read.
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
        Self::from_raw(oink_protocol::v2::cbor::decode_map(bytes, MAX_OWNER_BYTES)?)
    }
    pub fn encode(&self) -> Result<Vec<u8>, StoreError> {
        let mut raw = self.raw.clone();
        raw.insert(
            2,
            RawValue::Array(self.stored_preferences().iter().map(WireValue::write).collect()),
        );
        Ok(oink_protocol::v2::cbor::encode_map(&raw, MAX_OWNER_BYTES)?)
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
        }
        if let Some(group) = raw.get(&4) {
            store::group(group)?;
        }
        if let Some(audio) = raw.get(&AUDIO_KEY) {
            parse_audio(audio)?;
        }
        Ok(Self { raw })
    }
    fn stored_preferences(&self) -> Vec<DevicePreference> {
        store::entries(&self.raw)
            .expect("validated items")
            .iter()
            .map(|item| DevicePreference::read(item).expect("validated preference"))
            // Option 5 is retired locally. A stale saved guard is never authority.
            // Options 20/22 were per-device exit/microphone claims, replaced by
            // the group route setting; an earlier build's saved item is dropped.
            .filter(|item| {
                !matches!(
                    item.option().map(LocalOption::number),
                    Ok(5 | RETIRED_EXIT_CLAIM | RETIRED_MICROPHONE_CLAIM)
                )
            })
            .collect()
    }
    /// Frozen v1 projection: Disabled is represented by the original bool option,
    /// never by sending the local-only KeyboardMode value 2 to an old peer.
    /// The retired forward / receive options are never published.
    pub fn preferences(&self) -> Vec<DevicePreference> {
        let mut items: Vec<_> = self
            .stored_preferences()
            .into_iter()
            .filter(|item| !retired_audio_option(item))
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
            .filter(|item| !retired_audio_option(item))
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
    /// Group switches keep no per-group audio state: the audio owner merges
    /// with the new group as after a start (global audio configuration).
    pub fn switch_group(&mut self, group: GroupId) -> Result<(), StoreError> {
        self.raw.insert(4, RawValue::Bytes(group.0.to_vec()));
        Ok(())
    }
    /// This device's own audio configuration and the HLC's last stamp. Before
    /// this build wrote key 5 once, the retired options 4 / 24 are the
    /// initial forward / receive switches (0.3.x and early 0.4.0 settings);
    /// exit and sender are never taken from older claims or route records
    /// (choose the exit once after upgrading).
    pub fn audio(&self, me: DeviceId) -> (OwnConfig, u64) {
        if let Some(audio) = self.raw.get(&AUDIO_KEY) {
            return parse_audio(audio).expect("validated audio configuration");
        }
        let mut own = OwnConfig::local(me);
        own.forward.on = self.retired_bool(RETIRED_FORWARDING).unwrap_or(false);
        own.receive.on = !self.retired_bool(RETIRED_RECEIVE_MUTED).unwrap_or(true);
        (own, 0)
    }
    pub fn commit_audio(&mut self, own: OwnConfig, hlc_last: u64) -> Result<(), StoreError> {
        let mut candidate = self.clone();
        candidate.raw.insert(
            AUDIO_KEY,
            RawValue::Map(
                [
                    (1, own.speaker_exit.write()),
                    (2, own.microphone_send.write()),
                    (3, own.forward.to_wire().write()),
                    (4, own.receive.to_wire().write()),
                    (5, hlc_last.write()),
                ]
                .into(),
            ),
        );
        candidate.encode()?;
        *self = candidate;
        Ok(())
    }
    fn retired_bool(&self, option: u32) -> Option<bool> {
        self.stored_preferences()
            .into_iter()
            .find(|item| item.option().map(LocalOption::number) == Ok(option))
            .and_then(|item| bool::read(&item.value().ok()?).ok())
    }
}
fn retired_audio_option(item: &DevicePreference) -> bool {
    matches!(
        item.option().map(LocalOption::number),
        Ok(RETIRED_FORWARDING | RETIRED_RECEIVE_MUTED)
    )
}
fn parse_audio(value: &RawValue) -> Result<(OwnConfig, u64), StoreError> {
    let raw = value.as_map()?;
    let flag = |key| -> Result<Flag, StoreError> {
        Flag::from_wire(&dto::AudioFlag::read(store::required(raw, key)?)?).map_err(|_| StoreError::Invalid)
    };
    let own = OwnConfig {
        speaker_exit: bool::read(store::required(raw, 1)?)?,
        microphone_send: bool::read(store::required(raw, 2)?)?,
        forward: flag(3)?,
        receive: flag(4)?,
    };
    Ok((own, u64::read(store::required(raw, 5)?)?))
}
fn invalid(_: oink_protocol::v2::Error) -> ControlFailure {
    ControlFailure::InvalidValue
}
fn array(value: &RawValue) -> Result<&[RawValue], StoreError> {
    match value {
        RawValue::Array(v) => Ok(v),
        _ => Err(StoreError::Invalid),
    }
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
        | KeyboardDisabled
        | ScreenShared
        | CursorTrail
        | AcceptContacts
        | TouchpadInjection
        | NativeMultifinger
        | MacScrollFeel
        | SyncSystemArrangement
        | DisableHiddenDisplays => {
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
            ]
            .into_iter()
            .chain(transform.android().map_err(invalid)?)
            {
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
        AudioQuality => {
            if oink_audio::negotiation::Quality::from_number(u32::read(value).map_err(invalid)?).is_none() {
                return Err(bad);
            }
        }
        ForwardingControls | MicrophoneControls => {
            dto::ForwardingControlsValue::read(value).map_err(invalid)?;
        }
        MicrophoneInput => {
            let input = Option::<oink_protocol::v2::schema::Text<512>>::read(value).map_err(invalid)?;
            oink_audio::validate_receive_output(input.as_ref().map(|v| v.0.as_str())).map_err(|_| bad)?;
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
            (Transform, 1 | 2 | 4) => Some((Target, false)),
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
            Transform => key <= 4,
            Override => key <= 3,
            Policy => key <= 6,
        };
        if key == 0 || !valid {
            return Err(ControlFailure::UnsupportedOption);
        }
        let optional = matches!(
            (shape, key),
            (Transform, 4) | (Policy, 1) | (Override, 2 | 3) | (Keyboard, 3 | 4) | (SavedCustom, 2)
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
            (Transform, 1 | 2 | 4) => Some((Target, false)),
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
    use oink_audio::route::Version;
    fn fresh() -> OwnerStore {
        OwnerStore::new(super::super::projection::values(&super::super::Settings::default())).unwrap()
    }
    fn map(entries: impl IntoIterator<Item = (u32, RawValue)>) -> RawValue {
        RawValue::Map(entries.into_iter().collect())
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
        let bytes = oink_protocol::v2::cbor::encode_map(&owner.raw, MAX_OWNER_BYTES).unwrap();
        let restored = OwnerStore::decode(&bytes).unwrap();
        assert_eq!(restored.value(LocalOption::KeyboardMode), Some(1u32.write()));
        assert_eq!(restored.value(LocalOption::KeyboardDisabled), Some(false.write()));
        let raw = oink_protocol::v2::cbor::decode_map(&restored.encode().unwrap(), MAX_OWNER_BYTES).unwrap();
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
    fn own_audio_configuration_persists_device_level_with_the_clock() {
        let me = DeviceId([1; 32]);
        let mut owner = fresh();
        let (initial, last) = owner.audio(me);
        assert_eq!(
            (initial, last),
            (OwnConfig::local(me), 0),
            "a fresh install is all local"
        );
        let flag = |on, stamp| Flag {
            on,
            version: Version { stamp, author: me },
        };
        let own = OwnConfig {
            speaker_exit: true,
            microphone_send: false,
            forward: flag(true, 7 << 16),
            receive: flag(false, 8 << 16),
        };
        owner.commit_audio(own, 9 << 16).unwrap();
        // Not per group: a switch keeps it.
        owner.switch_group(GroupId([3; 16])).unwrap();
        let restored = OwnerStore::decode(&owner.encode().unwrap()).unwrap();
        assert_eq!(restored.audio(me), (own, 9 << 16));
        assert!(
            restored
                .preferences()
                .iter()
                .all(|item| !matches!(item.option().unwrap().number(), 4 | 24))
        );
    }
    #[test]
    fn retired_forward_and_receive_options_seed_the_switches_once_and_are_kept() {
        let me = DeviceId([1; 32]);
        let mut owner = fresh();
        let group = GroupId([3; 16]);
        // An earlier build: options 4 / 24, a group record with claims (keys
        // 2-5) and an early 0.4.0 route setting (key 6).
        let legacy_group = map([
            (1, RawValue::Bytes(group.0.to_vec())),
            (2, 7u64.write()),
            (3, RawValue::Array(vec![])),
            (4, RawValue::Null),
            (
                6,
                map([(1, map([(1, 3u64.write()), (2, store::wire_device(me).write())]))]),
            ),
        ]);
        owner.raw.insert(3, RawValue::Array(vec![legacy_group.clone()]));
        let RawValue::Array(items) = owner.raw.get_mut(&2).unwrap() else {
            unreachable!()
        };
        for (option, value) in [
            (RETIRED_FORWARDING, true.write()),
            (RETIRED_EXIT_CLAIM, RawValue::Null),
            (RETIRED_MICROPHONE_CLAIM, RawValue::Null),
            (RETIRED_RECEIVE_MUTED, false.write()),
        ] {
            items.push(map([(1, option.write()), (2, 3u64.write()), (3, value)]));
        }
        items.sort_by_key(|item| item.as_map().unwrap()[&1].as_u64().unwrap());
        let loaded =
            OwnerStore::decode(&oink_protocol::v2::cbor::encode_map(&owner.raw, MAX_OWNER_BYTES).unwrap()).unwrap();
        let (own, _) = loaded.audio(me);
        assert!(own.forward.on && own.receive.on, "seeded from options 4 / 24");
        assert!(!own.speaker_exit && !own.microphone_send, "no exit from older claims");
        assert!(
            loaded
                .preferences()
                .iter()
                .all(|item| !matches!(item.option().unwrap().number(), 4 | 20 | 22 | 24))
        );
        assert_eq!(loaded.item(LocalOption::from_number(4)), None, "never edited");
        let mut updated = loaded.clone();
        updated.commit_audio(OwnConfig::local(me), 1).unwrap();
        let reloaded = OwnerStore::decode(&updated.encode().unwrap()).unwrap();
        assert_eq!(reloaded.audio(me), (OwnConfig::local(me), 1), "key 5 wins once written");
        assert_eq!(
            reloaded.retired_bool(RETIRED_FORWARDING),
            Some(true),
            "the old option is kept"
        );
        assert_eq!(
            reloaded.raw[&3],
            RawValue::Array(vec![legacy_group]),
            "group records are kept"
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
