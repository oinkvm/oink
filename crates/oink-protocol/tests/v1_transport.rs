use oink_protocol::v1::{
    self, ErrorCode, RawValue as V,
    dto::*,
    schema::{Open, WireDto, WireValue},
};
use oink_test_support::protocol as support;

#[test]
fn path_and_low_nested_identities_retain_future_values() {
    let mut binding = LowBinding::from_raw(support::sample_named("LowBinding")).unwrap();
    binding
        .edit(|raw| {
            raw.insert(900, support::synthetic_unknown());
        })
        .unwrap();
    let mut raw = support::sample_named("LowBind");
    let V::Map(request) = raw.get_mut(&1).unwrap() else {
        panic!()
    };
    request.insert(8, binding.write());
    raw.insert(100, binding.write());
    let value = LowBind::from_raw(raw).unwrap();
    assert_eq!(value.request().unwrap().binding().unwrap(), binding);
    let bytes = value.encode(128 * 1024).unwrap();
    assert_eq!(
        LowBind::from_raw(v1::cbor::decode_map(&bytes, 128 * 1024).unwrap()).unwrap(),
        value
    );
    let reference = PathReference::from_raw([(1, V::Uint(500)), (2, support::synthetic_unknown())].into()).unwrap();
    assert!(matches!(reference.variant().unwrap(), Open::Unknown { id: 500, .. }));
}

#[test]
fn low_budgets_reject_overflow_and_insufficient_operation_limit() {
    for (send, receive, operation, valid) in [(1, 1, 2, true), (1, 1, 1, false), (u64::MAX, 1, u64::MAX, false)] {
        let result = LowOptions::from_raw([(1, V::Uint(send)), (2, V::Uint(receive)), (3, V::Uint(operation))].into());
        if valid {
            assert!(result.is_ok());
        } else {
            assert_eq!(result.unwrap_err().code, ErrorCode::LimitExceeded);
        }
    }
}

#[test]
fn path_reference_requires_its_known_nested_body() {
    let malformed = [(1, V::Uint(0)), (2, V::Map(Default::default()))].into();
    assert!(PathReference::from_raw(malformed).is_err());
    let valid = PathReference::from_raw(support::sample_type("PathReference").as_map().unwrap().clone()).unwrap();
    assert_eq!(valid.variant().unwrap(), Open::Known(PathReferenceKind::Request));
}
