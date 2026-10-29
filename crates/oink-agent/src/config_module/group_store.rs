//! The sole durable group envelope. Signed roster authorization belongs to the
//! network owner; this store preserves exact retained protocol objects and
//! commits a candidate before the runtime publishes it.
use crate::config::store::{self, StoreError};
use base64::Engine;
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v1::{
    self, RawMap, RawValue, dto, retained,
    schema::{WireDto, WireValue},
};
use std::{io::Read, path::Path};

pub const STATE_FILE: &str = "group-state.json";
const MAX_ENVELOPE_BYTES: usize = v1::limits::CONFIG_STATE * 2;

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
    pub fn to_wire(&self) -> dto::GroupState {
        self.state.clone()
    }
    pub fn application_focus_enabled(&self) -> Result<bool, StoreError> {
        for register in self.state.registers()?.0 {
            if register.kind()? == 8 && !register.deleted()? {
                return Ok(dto::ApplicationFocusSettings::from_raw(register.value()?)?.enabled()?);
            }
        }
        Ok(false)
    }
    /// Independent group register: geometry edits and local owner preferences
    /// cannot overwrite this choice. Retained LWW ordering resolves concurrent edits.
    pub fn set_application_focus_enabled(&mut self, enabled: bool) -> Result<bool, StoreError> {
        if self.application_focus_enabled()? == enabled {
            return Ok(false);
        }
        let mut registers = self.state.registers()?.0;
        let existing = registers.iter().position(|r| r.kind() == Ok(8));
        let register = if let Some(index) = existing {
            let current = &registers[index];
            retained::edit_register(current, &current.version()?, store::wire_device(self.device), |value| {
                value.insert(1, enabled.write());
                Ok(())
            })?
        } else {
            let version =
                dto::ConfigVersion::from_raw([(1, 1u64.write()), (3, store::wire_device(self.device).write())].into())?;
            let mut raw = RawMap::from([
                (1, 8u32.write()),
                (3, version.write()),
                (4, RawValue::Map([(1, enabled.write())].into())),
            ]);
            raw.insert(6, retained::register_digest(&raw)?.write());
            dto::Register::from_raw(raw)?
        };
        if let Some(index) = existing {
            registers[index] = register;
        } else {
            registers.push(register);
            registers.sort_by_key(|r| r.kind().expect("validated register"));
        }
        let mut raw = self.state.raw().clone();
        raw.insert(2, RawValue::Array(registers.iter().map(WireValue::write).collect()));
        let state = dto::GroupState::from_raw(raw)?;
        validate(&state)?;
        self.state = state;
        Ok(true)
    }
    pub fn layout_register(&self) -> Result<dto::Register, StoreError> {
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
        patch: impl FnOnce(&mut RawMap) -> v1::Result<()>,
    ) -> Result<bool, StoreError> {
        let current = self.layout_register()?;
        let edited = retained::edit_register(&current, expected, store::wire_device(self.device), patch)?;
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
fn validate(state: &dto::GroupState) -> Result<(), StoreError> {
    state.encode(v1::limits::CONFIG_STATE)?;
    let mut layout = false;
    for register in state.registers()?.0 {
        retained::verify_register(&register)?;
        if register.kind()? == 8 && !register.deleted()? {
            dto::ApplicationFocusSettings::from_raw(register.value()?)?;
        }
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
    let state = dto::GroupState::from_raw(v1::cbor::decode_map(&bytes, v1::limits::CONFIG_STATE)?)?;
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
        .encode(state.state.encode(v1::limits::CONFIG_STATE)?)
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
        let mut register: RawMap = [
            (1, 2u32.write()),
            (
                3,
                RawValue::Map([(1, 1u64.write()), (3, store::wire_device(device).write())].into()),
            ),
            (
                4,
                RawValue::Map([(90, RawValue::Map([(91, RawValue::Text("future".into()))].into()))].into()),
            ),
        ]
        .into();
        register.insert(6, retained::register_digest(&register).unwrap().write());
        let group = RawValue::Bytes(vec![9; 16]);
        let state = dto::GroupState::from_raw(
            [
                (1, group.clone()),
                (2, RawValue::Array(vec![RawValue::Map(register)])),
                (
                    3,
                    RawValue::Map(
                        [
                            (1, group),
                            (2, RawValue::Bytes(vec![3; 32])),
                            (3, RawValue::Array(vec![])),
                        ]
                        .into(),
                    ),
                ),
                (99, RawValue::Bytes(vec![1, 2, 3])),
            ]
            .into(),
        )
        .unwrap();
        StoredGroupState::new(device, state).unwrap()
    }
    #[test]
    fn application_focus_is_one_independent_durable_group_register() {
        let mut state = initial();
        let layout = state.layout_register().unwrap();
        assert!(!state.application_focus_enabled().unwrap());
        assert!(!state.set_application_focus_enabled(false).unwrap());
        assert!(state.set_application_focus_enabled(true).unwrap());
        assert_eq!(state.layout_register().unwrap(), layout);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        save(&path, &state).unwrap();
        let mut loaded = load(&path, state.device).unwrap().unwrap();
        assert!(loaded.application_focus_enabled().unwrap());
        // A different member edits the same register; no per-owner preference exists.
        loaded.device = DeviceId([8; 32]);
        assert!(loaded.set_application_focus_enabled(false).unwrap());
        let register = loaded
            .state
            .registers()
            .unwrap()
            .0
            .into_iter()
            .find(|r| r.kind() == Ok(8))
            .unwrap();
        assert_eq!(register.version().unwrap().counter().unwrap(), 2);
        assert_eq!(
            register.version().unwrap().writer().unwrap(),
            store::wire_device(loaded.device)
        );
        assert!(!loaded.application_focus_enabled().unwrap());
        assert_eq!(loaded.layout_register().unwrap(), layout);
        assert!(
            !initial().application_focus_enabled().unwrap(),
            "a fresh group never inherits another group's choice"
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
                    raw.insert(12, true.write());
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
        assert!(state.layout_register().unwrap().value().unwrap().contains_key(&90));
        assert!(state.state.raw().contains_key(&99));
        let bytes = std::fs::read(&path).unwrap();
        assert!(load(&path, DeviceId([8; 32])).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn unknown_schema_and_bad_digest_fail_without_writing() {
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
        raw.insert(6, RawValue::Bytes(vec![0; 32]));
        let mut candidate = state.clone();
        assert!(candidate.adopt_layout(&dto::Register::from_raw(raw).unwrap()).is_err());
        assert_eq!(candidate, state);
    }
}
