//! Explicit conversion at the authenticated identity boundary.
use oink_foundation::session::{BootPair, ConnectionId, PairId, PathId};
use oink_protocol::v2::{
    self, RawValue, dto,
    schema::{WireDto, WireValue},
};

/// Local correlation hash for authenticated group objects. v2 deliberately
/// has no generic wire `Digest` DTO.
pub fn object_digest(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

pub use oink_protocol::v2::identity::{boot, device, read_boot, read_device};
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
pub fn read_boot_pair(value: &dto::BootPair) -> v2::Result<BootPair> {
    BootPair::new(read_boot(&value.coordinator()?)?, read_boot(&value.follower()?)?)
        .ok_or(v2::ErrorCode::MalformedFrame.into())
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
pub fn read_connection(value: &dto::ConnectionId) -> v2::Result<ConnectionId> {
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
pub fn read_pair(value: &dto::PairId) -> v2::Result<PairId> {
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
pub fn read_path(value: &dto::PathId) -> v2::Result<PathId> {
    let path = PathId {
        boot_pair: read_boot_pair(&value.boot_pair()?)?,
        coordinator_counter: value.coordinator_counter()?,
    };
    if !path.valid() {
        return Err(v2::ErrorCode::MalformedFrame.into());
    }
    Ok(path)
}
