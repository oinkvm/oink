//! Explicit conversion at the authenticated identity boundary.
use oink_foundation::{
    DeviceId,
    session::{Boot, BootPair, ConnectionId, PairId, PathId},
};
use oink_protocol::v1::{
    self, RawValue, dto,
    schema::{WireDto, WireValue},
};

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
pub fn read_device(value: &dto::DeviceId) -> v1::Result<DeviceId> {
    value.require_supported()?;
    Ok(DeviceId(
        value
            .public_key()?
            .0
            .try_into()
            .map_err(|_| v1::ErrorCode::MalformedFrame)?,
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
pub fn read_boot(value: &dto::Boot) -> v1::Result<Boot> {
    Ok(Boot {
        device_id: read_device(&value.device_id()?)?,
        nonce: value.nonce()?.0,
    })
}
pub fn boot_pair(value: BootPair) -> dto::BootPair {
    dto::BootPair::from_raw(
        [
            (1, boot(value.coordinator()).write()),
            (2, boot(value.follower()).write()),
        ]
        .into(),
    )
    .expect("canonical boot pair")
}
pub fn read_boot_pair(value: &dto::BootPair) -> v1::Result<BootPair> {
    BootPair::new(read_boot(&value.coordinator()?)?, read_boot(&value.follower()?)?)
        .ok_or(v1::ErrorCode::MalformedFrame.into())
}
pub fn connection(value: ConnectionId) -> dto::ConnectionId {
    dto::ConnectionId::from_raw(
        [
            (1, boot(value.initiator_boot).write()),
            (2, RawValue::Bytes(value.random.to_vec())),
        ]
        .into(),
    )
    .expect("fixed connection identity")
}
pub fn read_connection(value: &dto::ConnectionId) -> v1::Result<ConnectionId> {
    Ok(ConnectionId {
        initiator_boot: read_boot(&value.initiator_boot()?)?,
        random: value.random()?.0,
    })
}
pub fn pair(value: PairId) -> dto::PairId {
    dto::PairId::from_raw(
        [
            (1, boot(value.initiator_boot).write()),
            (2, RawValue::Bytes(value.random.to_vec())),
        ]
        .into(),
    )
    .expect("fixed pairing identity")
}
pub fn read_pair(value: &dto::PairId) -> v1::Result<PairId> {
    Ok(PairId {
        initiator_boot: read_boot(&value.initiator_boot()?)?,
        random: value.random()?.0,
    })
}
pub fn path(value: PathId) -> dto::PathId {
    dto::PathId::from_raw(
        [
            (1, boot_pair(value.boot_pair).write()),
            (2, value.coordinator_counter.write()),
        ]
        .into(),
    )
    .expect("fixed path identity")
}
pub fn read_path(value: &dto::PathId) -> v1::Result<PathId> {
    let path = PathId {
        boot_pair: read_boot_pair(&value.boot_pair()?)?,
        coordinator_counter: value.coordinator_counter()?,
    };
    if !path.valid() {
        return Err(v1::ErrorCode::MalformedFrame.into());
    }
    Ok(path)
}
