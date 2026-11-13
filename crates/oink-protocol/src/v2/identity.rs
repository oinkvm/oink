//! Explicit conversion between local identities and their wire DTOs.
use super::{
    self as v2, RawValue, dto,
    schema::{WireDto, WireValue},
};
use oink_foundation::{DeviceId, session::Boot};

pub fn device(value: DeviceId) -> dto::DeviceId {
    dto::DeviceId::from_raw(
        [
            (1, RawValue::from(dto::ED25519)),
            (2, RawValue::Bytes(value.0.to_vec())),
        ]
        .into(),
    )
    .expect("fixed Ed25519 identity")
}
pub fn read_device(value: &dto::DeviceId) -> v2::Result<DeviceId> {
    value.require_supported()?;
    Ok(DeviceId(
        value
            .public_key()?
            .0
            .try_into()
            .map_err(|_| v2::ErrorCode::MalformedFrame)?,
    ))
}
pub fn boot(value: Boot) -> dto::Boot {
    dto::Boot::from_raw(
        [
            (1, device(value.device_id).write()),
            (2, RawValue::Bytes(value.nonce.to_vec())),
        ]
        .into(),
    )
    .expect("fixed boot identity")
}
pub fn read_boot(value: &dto::Boot) -> v2::Result<Boot> {
    Ok(Boot {
        device_id: read_device(&value.device_id()?)?,
        nonce: value.nonce()?.0,
    })
}
