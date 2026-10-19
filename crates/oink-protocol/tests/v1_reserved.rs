use oink_protocol::v1::{
    self, ErrorCode, Message, RawValue as V,
    admission::{Agreement, Context, Phase},
    dto::*,
    frame::{FrameHeader, OpaqueFrame},
    registry::{self, Transport},
    schema::WireDto,
};
use oink_test_support::protocol as support;

#[test]
fn allocated_operations_are_unknown_to_endpoints_and_opaque_to_relays() {
    assert!(!registry::CAPABILITIES.contains(&"oink.calibration/1"));
    assert!(Agreement::new(["oink.calibration/1".into()]).is_err());
    assert!(registry::stream(6).is_none());
    assert!(registry::RESERVED_STREAMS.contains(&(6, "K0")));
    let agreement = Agreement::new(registry::CAPABILITIES.iter().map(|s| (*s).into())).unwrap();
    let context = Context {
        kind: 6,
        transport: Transport::Reliable,
        phase: Phase::Member,
        agreement: &agreement,
        direct: true,
        old_member_lane: false,
    };
    for &(id, _) in registry::RESERVED_MESSAGES {
        assert!(registry::message(id).is_none());
        let header = FrameHeader {
            type_id: id,
            payload_length: 1,
        };
        let violation = context.admit(header).unwrap_err();
        assert_eq!(violation.code, ErrorCode::UnknownMessage);
        assert_eq!(violation.isolation, Isolation::Stream);
        assert!(violation.cleanup && violation.suppress_reopen);
        let mut bytes = Vec::from(id.to_le_bytes());
        bytes.extend(1_u32.to_le_bytes());
        bytes.push(0xa0);
        let frame = OpaqueFrame::parse_exact(&bytes, 16384).unwrap();
        assert_eq!(frame.decode().unwrap_err().code, ErrorCode::UnknownMessage);
        // A relay can retain these exact bytes without constructing a DTO.
        assert_eq!(frame.header.type_id, id);
        assert_eq!(
            Message::preflight(id, &[0xa0]).unwrap_err().code,
            ErrorCode::UnknownMessage
        );
    }
}

#[test]
fn reserved_layout_positions_are_absent_on_creation_and_survive_narrow_edits() {
    let mut layout = support::sample_named("LayoutValue");
    let mut screen = support::sample_named("ScreenPlacement");
    assert!(!layout.contains_key(&3));
    assert!(!screen.contains_key(&8));
    assert!(!support::sample_named("CompleteLayout").contains_key(&13));
    // Unknown extensions remain data, including nested keys and explicit null.
    let extension = support::synthetic_unknown();
    screen.insert(8, extension.clone());
    screen.insert(5, V::Uint(2));
    let mut group = support::sample_named("DevicePlacement");
    group.insert(4, V::Array(vec![V::Map(screen)]));
    layout.insert(1, V::Array(vec![V::Map(group)]));
    layout.insert(2, V::Uint(2));
    layout.insert(3, V::Array(vec![extension.clone()]));
    let mut decoded = LayoutValue::from_raw(layout).unwrap();
    assert_eq!(decoded.source().unwrap(), LayoutSource::Unknown(2));
    assert_eq!(
        decoded.screens().unwrap().0[0].size_source().unwrap(),
        SizeSource::Unknown(2)
    );
    let mut raw = decoded.raw().clone();
    // Changing a non-geometric preference does not reconstruct the layout.
    v1::retained::patch_path(&mut raw, &[4, 10], Some(V::Bool(false))).unwrap();
    decoded = LayoutValue::from_raw(raw).unwrap();
    let encoded = decoded.encode(v1::limits::CONFIG_STATE).unwrap();
    let restored = LayoutValue::from_raw(v1::cbor::decode_map(&encoded, v1::limits::CONFIG_STATE).unwrap()).unwrap();
    assert_eq!(restored.raw()[&3], V::Array(vec![extension.clone()]));
    assert_eq!(restored.screens().unwrap().0[0].raw()[&8], extension);
    assert_eq!(ChangeKind::from_number(3), ChangeKind::Unknown(3));
    assert_eq!(PairLimitKind::from_number(6), PairLimitKind::Unknown(6));
    assert_eq!(PairCode::from_number(29), PairCode::Unknown(29));
    assert_eq!(SizeSource::Manual.number(), 3);
}
