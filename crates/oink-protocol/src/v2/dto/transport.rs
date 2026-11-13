//! Retained identities for routing and Low operations. A network owner must
//! authenticate the current connection and path before acting on these values.
use super::*;
use crate::v2::schema::{OpenEnvelope, enum_body, no_validation, open_enum, required, wire_map};
use crate::v2::{ErrorCode, RawMap, Result};

wire_map!(PathRequestId,no_validation,{
    1=>source_boot:Boot=required;2=>random:Id16=required;
});
wire_map!(NextHopBinding,no_validation,{
    1=>peer:DeviceId=required;2=>connection_id:ConnectionId=required;3=>route_tag:u64=required;
});
wire_map!(RelayReservation,no_validation,{
    1=>relay:Boot=required;2=>random:Id16=required;
    3=>source_connection:ConnectionId=required;4=>target_connection:ConnectionId=required;
});
wire_map!(MigrationId,no_validation,{
    1=>path_id:PathId=required;2=>to_revision:u64=required;
});
open_enum!(PathReferenceKind {Request=0,Installed=1});
wire_map!(RequestedPath,no_validation,{1=>request:PathRequestId=required;});
wire_map!(InstalledPath,no_validation,{1=>path:PathId=required;});
enum_body!(PathReferenceKind {Request=>RequestedPath,Installed=>InstalledPath});
pub type PathReference = OpenEnvelope<PathReferenceKind>;

wire_map!(LowBinding,no_validation,{
    1=>path_id:PathId=required;2=>route_revision:u64=required;
    3=>source:Boot=required;4=>target:Boot=required;5=>hop:NextHopBinding=required;
});
wire_map!(LowOptions,validate_low_options,{
    1=>send_max:u64=required;2=>receive_max:u64=required;3=>operation_max:u64=required;
});
wire_map!(LowRequest,no_validation,{
    1=>request:Id16=required;2=>connection:Id16=required;3=>options:Option<LowOptions>=optional;
    4=>extra:bool=required;5=>keep_alive:bool=required;6=>purpose:LowTcpPurpose=required;
    7=>paced:bool=required;8=>binding:LowBinding=required;9=>route_tag:u64=required;10=>kind:u16=required;
});

fn validate_low_options(raw: &RawMap) -> Result<()> {
    let total = required::<u64>(raw, 1)?.checked_add(required::<u64>(raw, 2)?);
    let limit = required::<u64>(raw, 3)?;
    if total.is_none_or(|total| total > limit) {
        return Err(ErrorCode::LimitExceeded.into());
    }
    Ok(())
}

// Route selection between direct connections of one pair (oink.core/2).
// `RouteAddress` has the same shape as EndpointFact keys 1 and 2.
wire_map!(RouteAddress,validate_route_address,{
    1=>family:AddressFamily=required;2=>address:Bytes<16>=required;
});
wire_map!(RoutePin,no_validation,{1=>dialer:DeviceId=required;2=>target:RouteAddress=required;});
open_enum!(RouteActionKind {Use=0,Remember=1,Propose=2});
wire_map!(RouteUse,no_validation,{1=>connection_id:ConnectionId=required;2=>pinned_ip:Option<RouteAddress>=optional;});
wire_map!(RouteRemember,no_validation,{1=>pin:Option<RoutePin>=optional;});
wire_map!(RoutePropose,no_validation,{1=>connection_id:ConnectionId=required;2=>median_us:u32=required;});
enum_body!(RouteActionKind {Use=>RouteUse,Remember=>RouteRemember,Propose=>RoutePropose});
pub type RouteAction = OpenEnvelope<RouteActionKind>;
wire_map!(LinkRouteRequest,validate_route_request,{
    1=>path_id:PathId=required;2=>request_id:u64=required;3=>action:RouteAction=required;
});
wire_map!(LinkRouteState,no_validation,{
    1=>path_id:PathId=required;2=>route_revision:u64=required;3=>active:ConnectionId=required;
    4=>pin:Option<RoutePin>=optional;5=>set_by:Option<DeviceId>=optional;
    6=>answers:Option<u64>=optional;7=>refused:Option<u32>=optional;
    8=>pin_connection:Option<ConnectionId>=optional;
});

fn validate_route_address(raw: &RawMap) -> Result<()> {
    let length = required::<Bytes<16>>(raw, 2)?.0.len();
    match required::<AddressFamily>(raw, 1)? {
        AddressFamily::Ipv4 if length == 4 => Ok(()),
        AddressFamily::Ipv6 if length == 16 => Ok(()),
        AddressFamily::Ipv4 | AddressFamily::Ipv6 => Err(ErrorCode::MalformedFrame.into()),
        AddressFamily::Unknown(_) => Err(ErrorCode::UnsupportedValue.into()),
    }
}
fn validate_route_request(raw: &RawMap) -> Result<()> {
    if required::<u64>(raw, 2)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    // The envelope keeps unknown kinds structurally; this request has no
    // forward-compatible meaning for them, so they are refused here.
    if matches!(
        required::<RouteAction>(raw, 3)?.variant()?,
        crate::v2::schema::Open::Unknown { .. }
    ) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::v2::{Message, RawValue as V, frame, registry, schema::WireDto};

    fn device(byte: u8) -> V {
        V::Map([(1, V::from(ED25519)), (2, V::Bytes(vec![byte; 32]))].into())
    }
    fn boot(byte: u8) -> V {
        V::Map([(1, device(byte)), (2, V::Bytes(vec![byte; 16]))].into())
    }
    fn connection(byte: u8) -> V {
        V::Map([(1, boot(byte)), (2, V::Bytes(vec![0x40 | byte; 16]))].into())
    }
    fn path() -> V {
        V::Map([(1, V::Map([(1, boot(1)), (2, boot(2))].into())), (2, V::Uint(7))].into())
    }
    fn address(octets: [u8; 4]) -> V {
        V::Map([(1, V::Uint(0)), (2, V::Bytes(octets.to_vec()))].into())
    }
    fn action(kind: u64, body: crate::v2::RawMap) -> V {
        V::Map([(1, V::Uint(kind)), (2, V::Map(body))].into())
    }
    fn request(action: V) -> crate::v2::RawMap {
        [(1, path()), (2, V::Uint(9)), (3, action)].into()
    }
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
    fn golden(message: Message) -> String {
        hex(&frame::encode(&message, crate::v2::limits::RELIABLE_FRAME).unwrap())
    }

    fn use_min() -> Message {
        Message::LinkRouteRequest(LinkRouteRequest::from_raw(request(action(0, [(1, connection(2))].into()))).unwrap())
    }
    fn use_max() -> Message {
        Message::LinkRouteRequest(
            LinkRouteRequest::from_raw(request(action(
                0,
                [(1, connection(2)), (2, address([10, 77, 0, 1]))].into(),
            )))
            .unwrap(),
        )
    }
    fn remember() -> Message {
        Message::LinkRouteRequest(LinkRouteRequest::from_raw(request(action(1, Default::default()))).unwrap())
    }
    fn propose() -> Message {
        Message::LinkRouteRequest(
            LinkRouteRequest::from_raw(request(action(2, [(1, connection(2)), (2, V::Uint(400))].into()))).unwrap(),
        )
    }
    fn state_min() -> Message {
        Message::LinkRouteState(
            LinkRouteState::from_raw([(1, path()), (2, V::Uint(3)), (3, connection(1))].into()).unwrap(),
        )
    }
    fn state_max() -> Message {
        Message::LinkRouteState(
            LinkRouteState::from_raw(
                [
                    (1, path()),
                    (2, V::Uint(3)),
                    (3, connection(2)),
                    (4, V::Map([(1, device(2)), (2, address([10, 77, 0, 1]))].into())),
                    (5, device(2)),
                    (6, V::Uint(9)),
                    (7, V::Uint(3)),
                ]
                .into(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn route_messages_match_golden_vectors_and_round_trip() {
        for (message, expected) in [
            (use_min(), include_str!("../../../tests/fixtures/v2/0c13-use-min.hex")),
            (use_max(), include_str!("../../../tests/fixtures/v2/0c13-use-max.hex")),
            (
                remember(),
                include_str!("../../../tests/fixtures/v2/0c13-remember-clear.hex"),
            ),
            (propose(), include_str!("../../../tests/fixtures/v2/0c13-propose.hex")),
            (state_min(), include_str!("../../../tests/fixtures/v2/0c14-min.hex")),
            (state_max(), include_str!("../../../tests/fixtures/v2/0c14-max.hex")),
        ] {
            let name = registry::message(message.type_id()).unwrap().name;
            assert_eq!(golden(message.clone()), expected.trim(), "{name}");
            let bytes = frame::encode(&message, crate::v2::limits::RELIABLE_FRAME).unwrap();
            let decoded = frame::OpaqueFrame::parse_exact(&bytes, crate::v2::limits::RELIABLE_FRAME)
                .unwrap()
                .decode()
                .unwrap();
            assert_eq!(decoded, message, "{name}");
        }
        let Message::LinkRouteRequest(request) = use_max() else {
            unreachable!()
        };
        assert!(matches!(
            request.action().unwrap().variant().unwrap(),
            crate::v2::schema::Open::Known(RouteActionKind::Use)
        ));
        let body = RouteUse::from_raw(request.action().unwrap().body().unwrap()).unwrap();
        assert_eq!(
            body.pinned_ip().unwrap().unwrap().address().unwrap().0,
            vec![10, 77, 0, 1]
        );
    }

    #[test]
    fn unknown_action_kind_is_unsupported_and_missing_keys_are_malformed() {
        let error = LinkRouteRequest::from_raw(request(action(9, Default::default()))).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedValue);
        let mut missing = request(action(1, Default::default()));
        missing.remove(&2);
        assert_eq!(
            LinkRouteRequest::from_raw(missing).unwrap_err().code,
            ErrorCode::MalformedFrame
        );
        let mut zero = request(action(1, Default::default()));
        zero.insert(2, V::Uint(0));
        assert_eq!(
            LinkRouteRequest::from_raw(zero).unwrap_err().code,
            ErrorCode::MalformedFrame
        );
        assert!(LinkRouteRequest::from_raw(request(action(0, Default::default()))).is_err());
        assert!(LinkRouteRequest::from_raw(request(action(2, [(1, connection(2))].into()))).is_err());
        assert!(LinkRouteState::from_raw([(1, path()), (2, V::Uint(3))].into()).is_err());
        let bad = V::Map([(1, V::Uint(0)), (2, V::Bytes(vec![1; 16]))].into());
        assert!(LinkRouteRequest::from_raw(request(action(0, [(1, connection(2)), (2, bad)].into()))).is_err());
        // Unknown refusal numbers stay decodable; the receiver treats them as refused.
        let mut refused = state_min().raw().clone();
        refused.insert(7, V::Uint(77));
        assert_eq!(LinkRouteState::from_raw(refused).unwrap().refused().unwrap(), Some(77));
    }

    #[test]
    fn route_messages_are_core_n0_only_and_0c12_stays_retired() {
        for id in [0x0c13, 0x0c14] {
            let entry = registry::message(id).unwrap();
            assert_eq!(entry.capabilities, ["oink.core/2"]);
            assert_eq!(entry.routes.len(), 1);
            assert_eq!(entry.routes[0].kind, 0);
            assert_eq!(entry.routes[0].transport, registry::Transport::Reliable);
            assert!(entry.routes[0].extra_capabilities.is_empty());
        }
        assert!(registry::TOMBSTONES.contains(&0x0c12));
        assert!(registry::message(0x0c12).is_none());
        assert!(!registry::CAPABILITIES.iter().any(|cap| cap.starts_with("oink.route")));
    }

    #[test]
    #[ignore = "prints the route golden vectors"]
    fn print_route_vectors() {
        for message in [use_min(), use_max(), remember(), propose(), state_min(), state_max()] {
            println!("VECTOR {}", golden(message));
        }
    }
    #[test]
    fn remember_target_and_clear_use_the_single_message_codec() {
        let pin = V::Map([(1, device(2)), (2, address([192, 0, 2, 2]))].into());
        for body in [RawMap::new(), [(1, pin)].into()] {
            let message = Message::LinkRouteRequest(LinkRouteRequest::from_raw(request(action(1, body))).unwrap());
            let encoded = frame::encode(&message, crate::v2::limits::RELIABLE_FRAME).unwrap();
            let decoded = frame::OpaqueFrame::parse_exact(&encoded, crate::v2::limits::RELIABLE_FRAME)
                .unwrap()
                .decode()
                .unwrap();
            assert_eq!(decoded, message);
        }
    }
    #[test]
    fn pin_connection_binding_uses_the_same_route_state_message() {
        let Message::LinkRouteState(state) = state_max() else {
            unreachable!()
        };
        let mut raw = state.into_raw();
        raw.insert(8, connection(2));
        let message = Message::LinkRouteState(LinkRouteState::from_raw(raw).unwrap());
        let bytes = frame::encode(&message, crate::v2::limits::RELIABLE_FRAME).unwrap();
        assert_eq!(
            frame::OpaqueFrame::parse_exact(&bytes, crate::v2::limits::RELIABLE_FRAME)
                .unwrap()
                .decode()
                .unwrap(),
            message
        );
    }
}
