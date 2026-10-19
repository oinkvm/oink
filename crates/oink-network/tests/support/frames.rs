use super::oink_network_profile;
use oink_protocol::v1::{
    self, Message, RawValue, dto,
    schema::{WireDto, WireValue},
};

pub fn profile() -> oink_network_profile::OfferProfile {
    let mut profile = oink_network_profile::OfferProfile::default();
    profile.capabilities.extend(
        [
            "oink.diagnostics.collect/1",
            "oink.input.relative/1",
            "oink.input.contacts/1",
            "oink.input.gestures/1",
        ]
        .into_iter()
        .map(str::to_owned),
    );
    profile
}

pub fn control(size: usize) -> Vec<u8> {
    encode(&Message::ProtocolError(
        dto::ProtocolError::from_raw(
            [
                (1, 2u16.write()),
                (2, dto::ErrorScope::Message.write()),
                (42, RawValue::Bytes(vec![7; size])),
            ]
            .into(),
        )
        .unwrap(),
    ))
}
pub fn low(byte: u8, size: usize) -> Message {
    Message::DiagnosticsChunk(
        dto::DiagnosticsChunk::from_raw(
            [
                (1, RawValue::Bytes(vec![byte; size])),
                (2, RawValue::Bytes(vec![1; 16])),
                (3, 1u64.write()),
                (4, 0u32.write()),
            ]
            .into(),
        )
        .unwrap(),
    )
}
pub fn encode(message: &Message) -> Vec<u8> {
    v1::frame::encode(message, v1::limits::CONFIG_FRAME).unwrap()
}
