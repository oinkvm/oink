use oink_protocol::v1::{
    self, ErrorCode, RawMap, RawValue as V,
    dto::*,
    schema::{WireDto, WireValue},
};
use oink_test_support::protocol as support;

#[test]
fn source_resolved_keys_keep_physical_identity_in_owned_and_borrowed_views() {
    use oink_foundation::input::KeyCode;
    use v1::{Message, frame, hot};
    let mut raw = support::sample_named("InputKey");
    assert_eq!(InputKey::from_raw(raw.clone()).unwrap().physical_code().unwrap(), None);
    raw.insert(1, V::Uint(0x0007_00e1)); // HID keyboard left Shift.
    raw.insert(3, V::Uint(u64::from(KeyCode::CAPS_LOCK.0)));
    let key = InputKey::from_raw(raw.clone()).unwrap();
    assert_eq!(key.code().unwrap(), 0x0007_00e1);
    assert_eq!(key.physical_code().unwrap(), Some(KeyCode::CAPS_LOCK.0));
    let bytes = frame::encode(&Message::InputKey(key), 16384).unwrap();
    let borrowed = hot::View::<InputKey>::parse(&bytes[6..], 16384).unwrap();
    assert_eq!(borrowed.code().unwrap(), 0x0007_00e1);
    assert_eq!(borrowed.physical_code().unwrap(), Some(KeyCode::CAPS_LOCK.0));
    raw.insert(3, V::from("invalid physical identity"));
    assert!(InputKey::from_raw(raw).is_err());
}

#[test]
fn audio_reports_distinguish_unknown_latency_and_preserve_nested_extensions() {
    let mut latency = support::sample_named("OutputLatency");
    assert_eq!(
        OutputLatency::from_raw(latency.clone())
            .unwrap()
            .reported_latency_ms()
            .unwrap(),
        None
    );
    latency.insert(2, V::Null);
    assert!(OutputLatency::from_raw(latency.clone()).unwrap().raw().contains_key(&2));
    latency.insert(2, V::F64(0.0));
    latency.insert(900, support::synthetic_unknown());
    let output = OutputLatency::from_raw(latency.clone()).unwrap();
    assert_eq!(output.reported_latency_ms().unwrap(), Some(0.0));
    let mut receiver = support::sample_named("ReceiverReport");
    receiver.insert(900, support::synthetic_unknown());
    let mut state = support::sample_named("AudioState");
    state.insert(8, V::Uint(200));
    state.insert(12, V::Array(vec![output.write()]));
    state.insert(13, V::Array(vec![V::Map(receiver.clone())]));
    let state = AudioState::from_raw(state).unwrap();
    let saved = state.encode(16384).unwrap();
    let decoded = AudioState::from_raw(v1::cbor::decode_map(&saved, 16384).unwrap()).unwrap();
    assert_eq!(decoded.outputs().unwrap().0[0].raw(), &latency);
    assert_eq!(decoded.receivers().unwrap().0[0].raw(), &receiver);
    let mut invalid = decoded.raw().clone();
    invalid.insert(8, V::Uint(201));
    assert_eq!(
        AudioState::from_raw(invalid).unwrap_err().code,
        ErrorCode::UnsupportedValue
    );
    for bad in [-0.5, f64::INFINITY, f64::NAN] {
        let mut invalid = latency.clone();
        invalid.insert(2, V::F64(bad));
        assert!(OutputLatency::from_raw(invalid).is_err());
        let mut invalid = receiver.clone();
        invalid.insert(5, V::F64(bad));
        assert!(ReceiverReport::from_raw(invalid).is_err());
    }
    let mut invalid = state.raw().clone();
    invalid.insert(12, V::Array(vec![output.write(); 33]));
    assert!(AudioState::from_raw(invalid).is_err());
}

#[test]
fn protocol_violation_exports_aliases_and_open_codes_only() {
    let mut body = support::sample_named("DiagnosticProtocolViolation");
    body.insert(1, V::Uint(400));
    body.insert(2, V::Uint(400));
    body.insert(10, V::Uint(400));
    body.insert(900, support::synthetic_unknown());
    let dto = DiagnosticProtocolViolation::from_raw(body.clone()).unwrap();
    assert_eq!(dto.scope().unwrap(), ErrorScope::Unknown(400));
    assert_eq!(dto.isolation().unwrap(), Isolation::Unknown(400));
    let event =
        DiagnosticEvent::from_raw(RawMap::from([(1, V::from("protocol-violation/1")), (2, dto.write())])).unwrap();
    assert_eq!(event.fields().unwrap(), body);
    for key in [3, 4, 5] {
        let mut invalid = body.clone();
        invalid.insert(key, V::Bytes(vec![42; 32]));
        assert!(DiagnosticProtocolViolation::from_raw(invalid).is_err());
    }
    assert!(v1::registry::DIAGNOSTIC_EVENTS.contains(&"protocol-violation/1"));
}
