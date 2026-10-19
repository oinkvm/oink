use oink_protocol::v1::{self, ErrorCode, RawValue as V, dto::*, schema::WireDto};
use oink_test_support::protocol as support;

#[test]
fn merge_geometry_is_typed_bounded_and_retains_reserved_extensions() {
    let mut raw = support::sample_named("GeometryPatch");
    let patch = GeometryPatch::from_raw(raw.clone()).unwrap();
    assert_eq!(patch.source().unwrap(), LayoutSource::Manual);
    assert!(!patch.raw().contains_key(&2));
    assert!(!patch.raw().contains_key(&4));
    raw.insert(2, support::synthetic_unknown());
    raw.insert(900, support::synthetic_unknown());
    let patch = GeometryPatch::from_raw(raw.clone()).unwrap();
    assert_eq!(patch.raw(), &raw);
    assert_eq!(
        GeometryPatch::from_raw(v1::cbor::decode_map(&patch.encode(8192).unwrap(), 8192).unwrap()).unwrap(),
        patch
    );
    raw.insert(1, V::Array(vec![V::Map(support::sample_named("DevicePlacement")); 17]));
    let bytes = v1::cbor::encode_map(&raw, v1::limits::PAIR_OBJECT).unwrap();
    assert_eq!(
        GeometryPatch::preflight(&mut minicbor::Decoder::new(&bytes), 0)
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    raw.insert(1, V::Array(vec![V::Map([(1, V::Uint(3))].into())]));
    assert!(GeometryPatch::from_raw(raw).is_err());
}
