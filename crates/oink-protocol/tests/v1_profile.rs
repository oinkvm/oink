use oink_protocol::v1::{
    self, ErrorCode, RawMap, RawValue as V, cbor,
    dto::*,
    schema::{Open, WireDto, WireValue},
};
use oink_test_support::protocol as support;
#[test]
fn rejects_nonprofile_values_including_unknown_fields() {
    for hex in [
        "a1011817",
        "a1013817",
        "a1015800",
        "a1017800",
        "a1019800",
        "a101b800",
        "a1019fff",
        "bf01f5ff",
        "a201010102",
        "a200010102",
        "a202010101",
        "a11affffffff01",
        "a101c000",
        "a101f7",
        "a101f800",
        "a101f90000",
        "a101fa80000000",
        "a101fb8000000000000000",
        "a101fa7f800000",
        "a101fb7ff8000000000000",
        "a10161ff",
        "a10100ff",
        "a1015bffffffffffffffff",
        "a1019bffffffffffffffff",
        "a101a2",
        "a1011c",
    ] {
        assert!(cbor::decode_map(&support::unhex(hex), 1024).is_err(), "{hex}");
    }
}
#[test]
fn integer_float_width_and_endianness_boundaries() {
    let values = [
        V::Uint(0),
        V::Uint(23),
        V::Uint(24),
        V::Uint(255),
        V::Uint(256),
        V::Uint(65535),
        V::Uint(65536),
        V::Uint(u32::MAX as u64),
        V::Uint(u64::MAX),
        V::Negative(0),
        V::Negative(u64::MAX),
        V::F32(1.5),
        V::F64(1.5),
        V::Null,
    ];
    for value in values {
        let bytes = value.encode(64).unwrap();
        assert_eq!(cbor::decode(&bytes, 64).unwrap(), value);
    }
    assert_eq!(V::F64(8.0).encode(9).unwrap(), support::unhex("fb4020000000000000"));
    assert_eq!(V::F32(-0.0).encode(5).unwrap(), support::unhex("fa00000000"));
    assert!(V::F64(f64::NAN).encode(9).is_err());
    assert!(f32::read(&V::F64(1.0)).is_err());
    assert!(f64::read(&V::F32(1.0)).is_err());
}
#[test]
fn budgets_and_depth_precede_container_allocations() {
    assert_eq!(cbor::decode(&[0x80], 0).unwrap_err().code, ErrorCode::LimitExceeded);
    let mut bytes = vec![0x81; 32];
    bytes.push(0);
    assert_eq!(cbor::decode(&bytes, 100).unwrap_err().code, ErrorCode::LimitExceeded);
    let mut raw = V::Uint(0);
    for _ in 0..32 {
        raw = V::Array(vec![raw]);
    }
    assert!(raw.encode(100).is_err());
    let bytes = support::unhex("ba00010000");
    assert_eq!(cbor::decode(&bytes, 100).unwrap_err().code, ErrorCode::LimitExceeded);
}
#[test]
fn defaults_omitted_and_null_remain_distinct_in_retained_tree() {
    let settings = TopologySettings::from_raw(RawMap::new()).unwrap();
    assert_eq!(settings.endpoint_reach().unwrap(), 0.0);
    assert_eq!(settings.minimum_portal().unwrap(), 10.0 / 686.0);
    assert_eq!(settings.encode(100).unwrap(), vec![0xa0]);
    let receipt = PairReceipt::from_raw(support::sample_named("PairReceipt")).unwrap();
    assert!(!receipt.reply().unwrap());
    assert!(!receipt.raw().contains_key(&4));
    let no_report = ResourceFacts::from_raw(RawMap::new()).unwrap();
    assert_eq!(no_report.mouse_mode().unwrap(), None);
    let null = ResourceFacts::from_raw([(2, V::Null)].into()).unwrap();
    assert_eq!(null.mouse_mode().unwrap(), None);
    assert_eq!(null.encode(20).unwrap(), support::unhex("a102f6"));
    let explicit = ResourceFacts::from_raw([(2, V::Uint(0))].into()).unwrap();
    assert_eq!(explicit.mouse_mode().unwrap(), Some(SharingMode::Shared));
}
#[test]
fn open_enums_preserve_unknown_body_and_never_fabricate_success() {
    assert_eq!(SharingMode::read(&V::Uint(400)).unwrap(), SharingMode::Unknown(400));
    assert_eq!(SourceKind::read(&V::Uint(3)).unwrap(), SourceKind::Indeterminate);
    assert_eq!(SourceKind::read(&V::Uint(400)).unwrap(), SourceKind::Unknown(400));
    let body = [(55, support::synthetic_unknown())].into();
    let raw: RawMap = [(1, V::Uint(42)), (2, V::Map(body)), (500, V::Text("future".into()))].into();
    let result = TypedResult::from_raw(raw.clone()).unwrap();
    assert!(!result.is_success());
    assert!(matches!(result.variant().unwrap(), Open::Unknown { id: 42, .. }));
    assert_eq!(result.into_raw(), raw);
    let decision = Decision::from_raw([(1, V::Uint(555))].into()).unwrap();
    assert!(matches!(decision.variant().unwrap(), Open::Unknown { id: 555, .. }));
    assert!(!v1::ErrorCode::from_number(0x1_0000).is_success());
    assert_eq!(v1::ErrorCode::from_number(0x1_0000).number(), 0x1_0000);
}
#[test]
fn unknown_crypto_is_data_but_cannot_be_used_as_ed25519() {
    let mut raw = support::sample_named("DeviceId");
    raw.insert(1, V::from("future-key/2"));
    raw.insert(2, V::Bytes(vec![4; 71]));
    let id = DeviceId::from_raw(raw.clone()).unwrap();
    assert_eq!(id.require_supported().unwrap_err().code, ErrorCode::UnsupportedValue);
    assert_eq!(id.into_raw(), raw);
    let mut raw = support::sample_named("DeviceId");
    raw.insert(2, V::Bytes(vec![0; 31]));
    assert!(DeviceId::from_raw(raw).is_err());
}
#[test]
fn device_card_bounds_and_legacy_f32_allocation_are_enforced() {
    assert!(SpeakerObservation::from_raw([(3, V::Uint(201))].into()).is_err());
    let speaker = SpeakerObservation::from_raw([(2, V::Uint(0)), (3, V::Uint(0)), (4, V::Bool(false))].into()).unwrap();
    assert_eq!(speaker.forwarding_volume_percent().unwrap(), Some(0));
    assert_eq!(speaker.forwarding_muted().unwrap(), Some(false));
    // Retired display text is retained as opaque unknown data, not rendered or
    // subjected to its removed text schema. New presentation uses typed codes.
    assert!(ResourceFault::from_raw([(1, V::Uint(0)), (2, V::Text("x".repeat(257)))].into()).is_ok());
    assert!(ResourceFault::from_raw([(1, V::Uint(0)), (3, V::Text("synthetic".into()))].into()).is_err());
    assert!(ResourceFacts::from_raw([(900, V::Bytes(vec![0; 8192]))].into()).is_err());
    let mut options = support::sample_named("LocalOptionsSummary");
    options.insert(3, V::F64(1.0));
    assert!(LocalOptionsSummary::from_raw(options).is_err());
    let mut facts = support::sample_named("DeviceFacts");
    facts.insert(
        16,
        V::Map([(5, speaker.write()), (900, support::synthetic_unknown())].into()),
    );
    let mut facts = DeviceFacts::from_raw(facts).unwrap();
    let before = facts.raw()[&16].clone();
    facts
        .edit(|raw| {
            raw.insert(6, V::from("edited synthetic name"));
        })
        .unwrap();
    assert_eq!(facts.raw()[&16], before);
}
