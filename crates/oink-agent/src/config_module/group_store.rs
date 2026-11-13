//! The sole durable group envelope. Signed roster authorization belongs to the
//! network owner; this store preserves exact retained protocol objects and
//! commits a candidate before the runtime publishes it.
use crate::config::store::{self, StoreError};
use base64::Engine;
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v2::{
    self, RawMap, RawValue, dto, retained,
    schema::{WireDto, WireValue},
};
use std::{io::Read, path::Path};

mod legacy_layout;

pub const STATE_FILE: &str = "group-state.json";
const MAX_ENVELOPE_BYTES: usize = v2::limits::CONFIG_STATE * 2;

/// JSON schema1: device is {algorithm:"ed25519",public_key:<64hex>};
/// state_cbor is base64 retained GroupState. Other envelope fields are retained.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredGroupState {
    pub device: DeviceId,
    pub state: dto::GroupState,
    envelope: serde_json::Value,
}
impl StoredGroupState {
    pub fn new(device: DeviceId, state: dto::GroupState) -> Result<Self, StoreError> {
        validate(&state)?;
        Ok(Self {
            device,
            state,
            envelope: serde_json::json!({
                "schema":1,"device":{"algorithm":"ed25519","public_key":device.to_hex()}
            }),
        })
    }
    pub fn group(&self) -> GroupId {
        GroupId(self.state.group().expect("validated group").0)
    }
    pub fn to_wire(&self) -> Result<dto::GroupState, StoreError> {
        let roster = self.state.roster()?;
        let registers = self.state.registers()?.0;
        if !roster.raw().contains_key(&2)
            && !registers
                .iter()
                .any(|register| register.raw().contains_key(&6) || register.raw().contains_key(&7))
        {
            return Ok(self.state.clone());
        }
        let anchor = oink_network::roster::Anchor {
            group: self.group(),
            root: oink_network::roster::root_id(&roster)?,
        };
        let roster = oink_network::roster::Roster::verify(roster, anchor)?;
        let registers = registers
            .into_iter()
            .map(legacy_layout::project)
            .map(|result| result.map(|register| register.write()))
            .collect::<v2::Result<Vec<_>>>()?;
        let mut raw = self.state.raw().clone();
        raw.insert(2, RawValue::Array(registers));
        raw.insert(3, roster.wire().write());
        Ok(dto::GroupState::from_raw(raw)?)
    }
    pub fn has_legacy_layout(&self) -> Result<bool, StoreError> {
        let raw = self.raw_layout_register()?;
        Ok(raw.raw().contains_key(&6) || raw.raw().contains_key(&7))
    }
    /// Replace the v2 runtime projection while retaining fields owned by the
    /// pre-v2 local format.  These fields are intentionally absent from v2
    /// network state, but dropping them during a normal state commit would
    /// make the same directory unreadable by a 0.3.x process.
    pub fn replace_runtime_state(&mut self, candidate: dto::GroupState) -> Result<(), StoreError> {
        let mut raw = candidate.raw().clone();
        if self.state.group()? == candidate.group()? {
            preserve_legacy_roster(&self.state, &mut raw)?;
            preserve_legacy_registers(&self.state, &mut raw)?;
        }
        self.state = dto::GroupState::from_raw(raw)?;
        validate(&self.state)
    }
    pub fn layout_register(&self) -> Result<dto::Register, StoreError> {
        Ok(legacy_layout::project(self.raw_layout_register()?)?)
    }
    fn raw_layout_register(&self) -> Result<dto::Register, StoreError> {
        self.state
            .registers()?
            .0
            .into_iter()
            .find(|r| r.kind() == Ok(2))
            .ok_or(StoreError::Invalid)
    }
    /// The caller supplies an authorized narrow edit over the latest retained
    /// layout. Failure leaves the complete state untouched.
    pub fn edit_layout(
        &mut self,
        expected: &dto::ConfigVersion,
        patch: impl FnOnce(&mut RawMap) -> v2::Result<()>,
    ) -> Result<bool, StoreError> {
        let current = self.layout_register()?;
        let edited = v2::layout::edit_register(&current, expected, store::wire_device(self.device), patch)?;
        if edited.value()? == current.value()? {
            return Ok(false);
        }
        self.replace_layout(edited)
    }
    pub fn adopt_layout(&mut self, incoming: &dto::Register) -> Result<bool, StoreError> {
        if incoming.kind()? != 2 {
            return Err(StoreError::Invalid);
        }
        let merged = retained::merge_register(&self.layout_register()?, incoming)?;
        self.replace_layout(merged)
    }
    fn replace_layout(&mut self, layout: dto::Register) -> Result<bool, StoreError> {
        if self.layout_register()? == layout {
            return Ok(false);
        }
        let mut raw = self.state.raw().clone();
        let RawValue::Array(registers) = raw.get_mut(&2).ok_or(StoreError::Invalid)? else {
            return Err(StoreError::Invalid);
        };
        let slot = registers
            .iter_mut()
            .find(|r| dto::Register::read(r).is_ok_and(|r| r.kind() == Ok(2)))
            .ok_or(StoreError::Invalid)?;
        *slot = layout.write();
        let candidate = dto::GroupState::from_raw(raw)?;
        validate(&candidate)?;
        self.state = candidate;
        Ok(true)
    }
}

fn preserve_legacy_roster(previous: &dto::GroupState, candidate: &mut RawMap) -> Result<(), StoreError> {
    let Some(RawValue::Map(previous_roster)) = previous.raw().get(&3) else {
        return Err(StoreError::Invalid);
    };
    let Some(RawValue::Map(candidate_roster)) = candidate.get_mut(&3) else {
        return Err(StoreError::Invalid);
    };
    if let Some(root) = previous_roster.get(&2) {
        let stored_roster = dto::Roster::from_raw(previous_roster.clone())?;
        let anchor = oink_network::roster::Anchor {
            group: GroupId(stored_roster.group()?.0),
            root: oink_network::roster::root_id(&stored_roster)?,
        };
        let projected = oink_network::roster::Roster::verify(stored_roster, anchor)?;
        if projected.wire().raw() == candidate_roster {
            *candidate_roster = previous_roster.clone();
            return Ok(());
        }
        candidate_roster.entry(2).or_insert_with(|| root.clone());
    }
    let Some(RawValue::Array(previous_records)) = previous_roster.get(&3) else {
        return Err(StoreError::Invalid);
    };
    let Some(RawValue::Array(candidate_records)) = candidate_roster.get_mut(&3) else {
        return Err(StoreError::Invalid);
    };
    for candidate_record in candidate_records {
        let Some(previous_record) = previous_records
            .iter()
            .find(|previous_record| same_v2_record(previous_record, candidate_record))
        else {
            continue;
        };
        let (RawValue::Map(previous_record), RawValue::Map(candidate_record)) = (previous_record, candidate_record)
        else {
            return Err(StoreError::Invalid);
        };
        if let Some(value) = previous_record.get(&2) {
            candidate_record.entry(2).or_insert_with(|| value.clone());
        }
        let (Some(RawValue::Map(previous_body)), Some(RawValue::Map(candidate_body))) =
            (previous_record.get(&1), candidate_record.get_mut(&1))
        else {
            return Err(StoreError::Invalid);
        };
        if let Some(value) = previous_body.get(&6) {
            candidate_body.entry(6).or_insert_with(|| value.clone());
        }
    }
    Ok(())
}

fn same_v2_record(previous: &RawValue, candidate: &RawValue) -> bool {
    let (RawValue::Map(previous), RawValue::Map(candidate)) = (previous, candidate) else {
        return false;
    };
    let (Some(RawValue::Map(previous_body)), Some(RawValue::Map(candidate_body))) =
        (previous.get(&1), candidate.get(&1))
    else {
        return false;
    };
    let mut previous_body = previous_body.clone();
    let mut candidate_body = candidate_body.clone();
    previous_body.remove(&6);
    candidate_body.remove(&6);
    previous_body == candidate_body
}

fn preserve_legacy_registers(previous: &dto::GroupState, candidate: &mut RawMap) -> Result<(), StoreError> {
    let Some(RawValue::Array(previous_registers)) = previous.raw().get(&2) else {
        return Err(StoreError::Invalid);
    };
    let Some(RawValue::Array(candidate_registers)) = candidate.get_mut(&2) else {
        return Err(StoreError::Invalid);
    };
    for candidate_register in candidate_registers {
        let RawValue::Map(candidate_register) = candidate_register else {
            return Err(StoreError::Invalid);
        };
        let Some(kind) = candidate_register.get(&1) else {
            return Err(StoreError::Invalid);
        };
        let Some(RawValue::Map(previous_register)) = previous_registers
            .iter()
            .find(|value| matches!(value, RawValue::Map(register) if register.get(&1) == Some(kind)))
        else {
            continue;
        };
        let previous = dto::Register::from_raw(previous_register.clone())?;
        legacy_layout::retain(&previous, candidate_register)?;
    }
    Ok(())
}
fn validate(state: &dto::GroupState) -> Result<(), StoreError> {
    state.encode(v2::limits::CONFIG_STATE)?;
    let mut layout = false;
    for register in state.registers()?.0 {
        retained::verify_register(&register)?;
        if register.kind()? == 2 {
            layout = true;
        }
    }
    if !layout {
        return Err(StoreError::Invalid);
    }
    Ok(())
}
/// Missing is fresh-start; damaged/foreign files are never treated as missing.
pub fn load(path: &Path, device: DeviceId) -> Result<Option<StoredGroupState>, StoreError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_ENVELOPE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(StoreError::LimitExceeded);
    }
    let mut envelope: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| StoreError::Invalid)?;
    if envelope.get("schema").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(StoreError::UnsupportedSchema);
    }
    let identity = envelope.get("device").ok_or(StoreError::Invalid)?;
    if identity.get("algorithm").and_then(serde_json::Value::as_str) != Some("ed25519")
        || identity.get("public_key").and_then(serde_json::Value::as_str) != Some(device.to_hex().as_str())
    {
        return Err(StoreError::IdentityMismatch);
    }
    let encoded = envelope
        .get("state_cbor")
        .and_then(serde_json::Value::as_str)
        .ok_or(StoreError::Invalid)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| StoreError::Invalid)?;
    // v2 changes the network projection only. Keep unknown retained fields
    // from 0.3.x in the raw DTO so older readers can still read the file.
    let state = dto::GroupState::from_raw(v2::cbor::decode_map(&bytes, v2::limits::CONFIG_STATE)?)?;
    validate(&state)?;
    envelope
        .as_object_mut()
        .ok_or(StoreError::Invalid)?
        .remove("state_cbor");
    Ok(Some(StoredGroupState {
        device,
        state,
        envelope,
    }))
}

pub fn save(path: &Path, state: &StoredGroupState) -> Result<(), StoreError> {
    validate(&state.state)?;
    let mut envelope = state.envelope.clone();
    envelope["state_cbor"] = base64::engine::general_purpose::STANDARD
        .encode(state.state.encode(v2::limits::CONFIG_STATE)?)
        .into();
    let bytes = serde_json::to_vec_pretty(&envelope).map_err(|_| StoreError::Invalid)?;
    if bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(StoreError::LimitExceeded);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    oink_foundation::storage::atomic_write(path, &bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn initial() -> StoredGroupState {
        let device = DeviceId([7; 32]);
        let register: RawMap = [
            (1, 2u32.write()),
            (
                3,
                RawValue::Map([(1, 1u64.write()), (3, store::wire_device(device).write())].into()),
            ),
            (
                4,
                RawValue::Map(
                    [
                        (1, RawValue::Array(vec![])),
                        (2, dto::LayoutSource::Default.write()),
                        (4, RawValue::Map(RawMap::new())),
                    ]
                    .into(),
                ),
            ),
            (90, RawValue::Map([(91, RawValue::Text("future".into()))].into())),
        ]
        .into();
        let group = RawValue::Bytes(vec![9; 16]);
        let state = dto::GroupState::from_raw(
            [
                (1, group.clone()),
                (2, RawValue::Array(vec![RawValue::Map(register)])),
                (3, RawValue::Map([(1, group), (3, RawValue::Array(vec![]))].into())),
                (99, RawValue::Bytes(vec![1, 2, 3])),
            ]
            .into(),
        )
        .unwrap();
        StoredGroupState::new(device, state).unwrap()
    }

    #[test]
    fn legacy_030_033_104_105_state_roundtrips_without_rewrite() {
        let state = initial();
        let device = state.device;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        let releases = ["0.3.0", "0.3.3", "104", "105"];

        for release in releases {
            let mut raw = state.state.raw().clone();
            let RawValue::Map(roster) = raw.get_mut(&3).unwrap() else {
                panic!("roster")
            };
            roster.insert(2, RawValue::Bytes(vec![4; 32]));
            let RawValue::Array(records) = roster.get_mut(&3).unwrap() else {
                panic!("records")
            };
            records.push(RawValue::Map(
                [
                    (
                        1,
                        RawValue::Map(
                            [
                                (1, dto::RecordKind::Create.write()),
                                (2, RawValue::Bytes(vec![9; 16])),
                                (3, store::wire_device(device).write()),
                                (4, 0u64.write()),
                                (5, store::wire_device(device).write()),
                                (6, RawValue::Bytes(vec![6; 32])),
                                (9, 1u64.write()),
                            ]
                            .into(),
                        ),
                    ),
                    (
                        2,
                        RawValue::Map(
                            [(1, RawValue::Text("ed25519".into())), (2, RawValue::Bytes(vec![7; 64]))].into(),
                        ),
                    ),
                ]
                .into(),
            ));
            let RawValue::Array(registers) = raw.get_mut(&2).unwrap() else {
                panic!("registers")
            };
            let RawValue::Map(register) = registers.first_mut().unwrap() else {
                panic!("register")
            };
            register.insert(6, RawValue::Bytes(vec![8; 32]));
            register.insert(
                7,
                RawValue::Map(
                    [
                        (1, RawValue::Array(Vec::new())),
                        (2, RawValue::Array(Vec::new())),
                        (3, RawValue::Array(Vec::new())),
                        (4, RawValue::Array(Vec::new())),
                    ]
                    .into(),
                ),
            );
            let original = dto::GroupState::from_raw(raw).unwrap();
            validate(&original).unwrap();
            let envelope = serde_json::json!({
                "schema": 1,
                "device": {"algorithm": "ed25519", "public_key": device.to_hex()},
                "legacy_release": release,
                "state_cbor": base64::engine::general_purpose::STANDARD.encode(original.encode(v2::limits::CONFIG_STATE).unwrap()),
            });
            std::fs::write(&path, serde_json::to_vec_pretty(&envelope).unwrap()).unwrap();
            let before = std::fs::read(&path).unwrap();
            let loaded = load(&path, device).unwrap().unwrap();
            assert_eq!(loaded.state.raw(), original.raw(), "{release} state changed");
            assert_eq!(std::fs::read(&path).unwrap(), before, "{release} was rewritten on load");
            let projected = loaded.to_wire().unwrap();
            let wire_register = &projected.registers().unwrap().0[0];
            assert!(!wire_register.raw().contains_key(&6));
            assert!(!wire_register.raw().contains_key(&7));
            let wire_roster = projected.roster().unwrap();
            assert!(!wire_roster.raw().contains_key(&2));
            assert!(!wire_roster.records().unwrap().0[0].raw().contains_key(&2));
            let mut merged = loaded.clone();
            merged.replace_runtime_state(projected).unwrap();
            assert_eq!(merged, loaded, "{release} state changed by its v2 projection");
            save(&path, &loaded).unwrap();
            let after_save = load(&path, device).unwrap().unwrap();
            assert_eq!(
                after_save.state.raw(),
                original.raw(),
                "{release} state changed after save"
            );
        }
    }
    #[test]
    fn retired_register_survives_save_and_layout_edit() {
        let mut state = initial();
        let version = state.layout_register().unwrap().version().unwrap();
        let raw: RawMap = [
            (1, 8u32.write()),
            (3, version.write()),
            (
                4,
                RawValue::Map([(1, true.write()), (90, RawValue::from("opaque"))].into()),
            ),
        ]
        .into();
        let register = dto::Register::from_raw(raw).unwrap();
        let mut raw = state.state.raw().clone();
        let RawValue::Array(registers) = raw.get_mut(&2).unwrap() else {
            panic!("registers")
        };
        registers.push(register.write());
        state = StoredGroupState::new(state.device, dto::GroupState::from_raw(raw).unwrap()).unwrap();
        state
            .edit_layout(&version, |raw| {
                let RawValue::Map(topology) = raw.get_mut(&4).unwrap() else {
                    panic!("layout topology")
                };
                topology.insert(12, (8.0 / 686.0).write());
                Ok(())
            })
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        save(&path, &state).unwrap();
        let loaded = load(&path, state.device).unwrap().unwrap();
        assert_eq!(loaded, state);
        assert_eq!(
            loaded
                .state
                .registers()
                .unwrap()
                .0
                .into_iter()
                .find(|r| r.kind() == Ok(8))
                .unwrap(),
            register
        );
    }
    #[test]
    fn retained_roundtrip_and_fenced_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        let mut state = initial();
        state.envelope["future"] = serde_json::json!({"keep":true});
        let version = state.layout_register().unwrap().version().unwrap();
        assert!(
            state
                .edit_layout(&version, |raw| {
                    let RawValue::Map(topology) = raw.get_mut(&4).unwrap() else {
                        panic!("layout topology")
                    };
                    topology.insert(12, (8.0 / 686.0).write());
                    Ok(())
                })
                .unwrap()
        );
        let before = state.clone();
        assert!(matches!(
            state.edit_layout(&version, |_| Ok(())),
            Err(StoreError::StateConflict)
        ));
        assert_eq!(state, before);
        save(&path, &state).unwrap();
        assert_eq!(load(&path, state.device).unwrap(), Some(state.clone()));
        assert!(state.layout_register().unwrap().raw().contains_key(&90));
        assert!(state.state.raw().contains_key(&99));
        let bytes = std::fs::read(&path).unwrap();
        assert!(load(&path, DeviceId([8; 32])).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn unknown_schema_and_malformed_register_fail_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        let state = initial();
        save(&path, &state).unwrap();
        let mut envelope: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        envelope["schema"] = 2.into();
        let bytes = serde_json::to_vec(&envelope).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(load(&path, state.device), Err(StoreError::UnsupportedSchema)));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let mut raw = state.layout_register().unwrap().into_raw();
        raw.remove(&3);
        assert!(dto::Register::from_raw(raw).is_err());
        let mut restored = envelope;
        restored["schema"] = 1.into();
        std::fs::write(&path, serde_json::to_vec(&restored).unwrap()).unwrap();
        assert_eq!(load(&path, state.device).unwrap(), Some(state));
    }
}
