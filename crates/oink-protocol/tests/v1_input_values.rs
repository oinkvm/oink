use oink_protocol::v1::{self, ErrorCode, RawMap, RawValue as V, dto::*, hot, schema::WireDto};
use oink_test_support::protocol as support;

fn bytes(raw: &RawMap) -> Vec<u8> {
    v1::cbor::encode_map(raw, v1::limits::RELIABLE_FRAME).unwrap()
}
fn contacts() -> RawMap {
    let mut raw = support::sample_named("InputTouchpadContacts");
    raw.insert(3, V::Uint(0));
    let mut change = support::sample_named("ContactChange");
    change.insert(2, V::Uint(0));
    raw.insert(6, V::Array(vec![V::Map(change)]));
    raw
}
#[test]
fn contact_quanta_bounds_duplicates_and_cancellation_match_borrowed_decoder() {
    let good = contacts();
    assert!(InputTouchpadContacts::from_raw(good.clone()).is_ok());
    assert!(hot::View::<InputTouchpadContacts>::parse(&bytes(&good), v1::limits::RELIABLE_FRAME).is_ok());
    for case in 0..4 {
        let mut raw = good.clone();
        match case {
            0 => {
                raw.insert(4, V::Map([(1, V::Uint(999)), (2, V::Uint(1000))].into()));
            }
            1 => {
                raw.insert(1, V::Uint(0));
            }
            2 => {
                let V::Array(changes) = raw.get_mut(&6).unwrap() else {
                    panic!()
                };
                changes.push(changes[0].clone());
            }
            _ => {
                raw.insert(6, V::Array(vec![]));
            }
        }
        assert_eq!(
            InputTouchpadContacts::from_raw(raw.clone()).unwrap_err().code,
            ErrorCode::UnsupportedValue
        );
        assert_eq!(
            hot::View::<InputTouchpadContacts>::parse(&bytes(&raw), v1::limits::RELIABLE_FRAME)
                .unwrap_err()
                .code,
            ErrorCode::UnsupportedValue
        );
    }
    let mut cancel = good;
    cancel.insert(3, V::Uint(3));
    cancel.insert(4, V::Map([(1, V::Uint(0)), (2, V::Uint(0))].into()));
    cancel.insert(6, V::Array(vec![]));
    InputTouchpadContacts::from_raw(cancel.clone()).unwrap();
    hot::View::<InputTouchpadContacts>::parse(&bytes(&cancel), v1::limits::RELIABLE_FRAME).unwrap();
    let mut oversized = contacts();
    oversized.insert(6, V::Array(vec![V::Map(support::sample_named("ContactChange")); 11]));
    assert_eq!(
        v1::Message::preflight(0x020b, &bytes(&oversized)).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
}
#[test]
fn button_and_gesture_payloads_are_typed_and_unknown_variants_are_retained() {
    let raw = RawMap::from([(1, V::Uint(5)), (2, V::Map([(1, V::Uint(255))].into()))]);
    MouseButton::from_raw(raw.clone()).unwrap();
    let encoded = bytes(&raw);
    let button = hot::View::<MouseButton>::parse(&encoded, 8192).unwrap();
    assert_eq!(button.kind().unwrap(), MouseButtonKind::Other);
    assert_eq!(
        hot::View::<OtherMouseButton>::from_map(button.body().unwrap())
            .unwrap()
            .number()
            .unwrap(),
        255
    );
    let bad = RawMap::from([(1, V::Uint(5))]);
    assert!(MouseButton::from_raw(bad.clone()).is_err());
    assert!(hot::View::<MouseButton>::parse(&bytes(&bad), 8192).is_err());
    let future = RawMap::from([
        (1, V::Uint(100)),
        (2, support::synthetic_unknown()),
        (900, support::synthetic_unknown()),
    ]);
    assert_eq!(Gesture::from_raw(future.clone()).unwrap().into_raw(), future);
    let encoded = bytes(&future);
    let future = hot::View::<Gesture>::parse(&encoded, 8192).unwrap();
    assert_eq!(future.kind().unwrap(), GestureKind::Unknown(100));
    assert_eq!(future.bytes(), encoded);
    let missing = RawMap::from([(1, V::Uint(0)), (2, V::Map(RawMap::new()))]);
    assert!(Gesture::from_raw(missing.clone()).is_err());
    assert!(hot::View::<Gesture>::parse(&bytes(&missing), 8192).is_err());
}
#[test]
fn trail_order_and_notification_age_are_bounded() {
    let mut trail = support::sample_named("TrailContinuation");
    let point = |age| V::Map([(1, V::F64(age)), (2, V::Map(support::sample_named("Point")))].into());
    trail.insert(6, V::Array(vec![point(62.0), point(1.0)]));
    TrailContinuation::from_raw(trail.clone()).unwrap();
    for points in [vec![point(1.0), point(2.0)], vec![point(62.01)], vec![point(1.0); 65]] {
        trail.insert(6, V::Array(points));
        assert!(TrailContinuation::from_raw(trail.clone()).is_err());
    }
    let raw = [(1, V::Uint(5001))].into();
    assert_eq!(
        RecentAudioNotifications::from_raw(raw).unwrap_err().code,
        ErrorCode::UnsupportedValue
    );
    let raw = [(2, V::Uint(5001))].into();
    assert_eq!(
        hot::View::<RecentAudioNotifications>::parse(&bytes(&raw), 8192)
            .unwrap_err()
            .code,
        ErrorCode::UnsupportedValue
    );
}
