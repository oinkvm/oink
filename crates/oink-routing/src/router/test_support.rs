use super::*;
impl Router {
    pub(super) fn test_connect(&mut self, peer: DeviceId, now: Micros) {
        if self.peers.get(&peer).is_some_and(|p| p.connected) {
            return;
        }
        assert!(self.attach_input(
            oink_foundation::session::Boot {
                device_id: peer,
                nonce: [peer.0[0]; 16]
            },
            [peer.0[0]; 16],
            true,
            true,
            FeelProfile::default(),
            now
        ));
    }
    pub(super) fn test_disconnect(&mut self, peer: DeviceId, _now: Micros) {
        self.detach_input(peer);
    }
    pub(super) fn test_message(&mut self, peer: DeviceId, message: Message, now: Micros) {
        self.test_connect(peer, now);
        self.input_message(peer, message, now);
    }
}

pub(super) fn volume_route(source: DeviceId, target: DeviceId) -> crate::input::VolumeRoute {
    use oink_protocol::v1::{
        RawValue, dto,
        schema::{WireDto, WireValue},
    };
    let boot = |id: DeviceId| {
        dto::Boot::from_raw(
            [
                (
                    1,
                    RawValue::Map(
                        [
                            (1, RawValue::Text(dto::ED25519.into())),
                            (2, RawValue::Bytes(id.0.to_vec())),
                        ]
                        .into(),
                    ),
                ),
                (2, RawValue::Bytes(vec![id.0[0]; 16])),
            ]
            .into(),
        )
        .unwrap()
    };
    let receiver = dto::ReceiverOutputBinding::from_raw(
        [
            (1, boot(target).write()),
            (2, 1u64.write()),
            (3, RawValue::Text("output".into())),
        ]
        .into(),
    )
    .unwrap();
    let claim =
        dto::ExitClaimStamp::from_raw([(1, 1u64.write()), (2, boot(target).device_id().unwrap().write())].into())
            .unwrap();
    let context = dto::AudioSessionContext::from_raw(
        [
            (1, boot(source).write()),
            (2, 1u64.write()),
            (3, receiver.write()),
            (4, claim.write()),
        ]
        .into(),
    )
    .unwrap();
    crate::input::VolumeRoute::from_session(Arc::new(context)).unwrap()
}
