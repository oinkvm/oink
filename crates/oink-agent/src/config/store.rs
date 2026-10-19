//! Common encoding for local retained stores. No importer or alternate schema.
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v1::{
    RawMap, RawValue, dto,
    schema::{WireDto, WireValue},
};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("local store I/O failed")]
    Io(#[from] std::io::Error),
    #[error("unsupported local schema")]
    UnsupportedSchema,
    #[error("invalid local store")]
    Invalid,
    #[error("local state conflict")]
    StateConflict,
    #[error("unsupported local value")]
    UnsupportedValue,
    #[error("local store limit exceeded")]
    LimitExceeded,
    #[error("local store identity mismatch")]
    IdentityMismatch,
}
impl From<oink_protocol::v1::Error> for StoreError {
    fn from(error: oink_protocol::v1::Error) -> Self {
        use oink_protocol::v1::ErrorCode;
        match error.code {
            ErrorCode::StateConflict => Self::StateConflict,
            ErrorCode::UnsupportedValue => Self::UnsupportedValue,
            ErrorCode::LimitExceeded => Self::LimitExceeded,
            _ => Self::Invalid,
        }
    }
}

pub(crate) fn read(path: &Path, maximum: usize) -> Result<Option<RawMap>, StoreError> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(StoreError::LimitExceeded);
    }
    let map = oink_protocol::v1::cbor::decode_map(&bytes, maximum)?;
    schema(&map)?;
    Ok(Some(map))
}

pub(crate) fn schema(raw: &RawMap) -> Result<(), StoreError> {
    if raw.get(&1) != Some(&RawValue::Uint(1)) {
        return Err(StoreError::UnsupportedSchema);
    }
    Ok(())
}

pub(crate) fn save(path: &Path, raw: &RawMap, maximum: usize) -> Result<(), StoreError> {
    schema(raw)?;
    let bytes = oink_protocol::v1::cbor::encode_map(raw, maximum)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    oink_foundation::storage::atomic_write(path, &bytes)?;
    Ok(())
}

pub(crate) fn empty() -> RawMap {
    [(1, RawValue::Uint(1)), (2, RawValue::Array(Vec::new()))].into()
}

pub(crate) fn entries(raw: &RawMap) -> Result<&[RawValue], StoreError> {
    match raw.get(&2) {
        Some(RawValue::Array(items)) => Ok(items),
        _ => Err(StoreError::Invalid),
    }
}

pub fn wire_device(device: DeviceId) -> dto::DeviceId {
    dto::DeviceId::from_raw(
        [
            (1, RawValue::Text("ed25519".into())),
            (2, RawValue::Bytes(device.0.to_vec())),
        ]
        .into(),
    )
    .expect("Ed25519 identity")
}

pub fn device(value: &RawValue) -> Result<DeviceId, StoreError> {
    let id = dto::DeviceId::read(value)?;
    if id.algorithm()?.0 != "ed25519" {
        return Err(StoreError::IdentityMismatch);
    }
    Ok(DeviceId(
        id.public_key()?.0.try_into().map_err(|_| StoreError::Invalid)?,
    ))
}

pub fn group(value: &RawValue) -> Result<GroupId, StoreError> {
    match value {
        RawValue::Bytes(bytes) => Ok(GroupId(bytes.as_slice().try_into().map_err(|_| StoreError::Invalid)?)),
        _ => Err(StoreError::Invalid),
    }
}

pub(crate) fn required(raw: &RawMap, key: u32) -> Result<&RawValue, StoreError> {
    raw.get(&key).ok_or(StoreError::Invalid)
}
