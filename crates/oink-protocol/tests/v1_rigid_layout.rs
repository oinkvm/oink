use oink_protocol::v1::{self, RawValue as V, dto::*, schema::WireDto};
use oink_test_support::protocol as support;
fn layout() -> LayoutValue {
    let mut group = support::sample_named("DevicePlacement");
    group.insert(1, V::Map(support::sample_named("DeviceId")));
    group.insert(2, V::Map([(1, V::F64(-1.0)), (2, V::F64(2.0))].into()));
    group.insert(3, V::F64(0.001));
    group.insert(4, V::Array(vec![V::Map(support::sample_named("ScreenPlacement"))]));
    let mut value = support::sample_named("LayoutValue");
    value.insert(1, V::Array(vec![V::Map(group)]));
    LayoutValue::from_raw(value).unwrap()
}
#[test]
fn group_geometry_is_bounded_unique_and_has_no_independent_screen_placement() {
    let value = layout();
    v1::layout::require_supported_geometry(&value).unwrap();
    for key in [3, 4] {
        let mut raw = value.raw().clone();
        let V::Array(groups) = raw.get_mut(&1).unwrap() else {
            panic!()
        };
        if key == 3 {
            groups.push(groups[0].clone());
        } else {
            let V::Map(group) = &mut groups[0] else { panic!() };
            let V::Array(screens) = group.get_mut(&4).unwrap() else {
                panic!()
            };
            let V::Map(screen) = &mut screens[0] else { panic!() };
            screen.insert(4, V::Map(support::sample_named("Rect")));
        }
        let invalid = LayoutValue::from_raw(raw).unwrap();
        assert!(v1::layout::require_supported_geometry(&invalid).is_err());
    }
    for scale in [0.0, -1.0, f64::MAX] {
        let mut raw = value.raw().clone();
        let V::Array(groups) = raw.get_mut(&1).unwrap() else {
            panic!()
        };
        let V::Map(group) = &mut groups[0] else { panic!() };
        group.insert(3, V::F64(scale));
        // Finite values whose expanded geometry overflows cannot execute either.
        if scale == f64::MAX {
            let V::Array(screens) = group.get_mut(&4).unwrap() else {
                panic!()
            };
            let V::Map(screen) = &mut screens[0] else { panic!() };
            screen.insert(
                3,
                V::Map([(1, V::F64(2.0)), (2, V::F64(2.0)), (3, V::F64(2.0)), (4, V::F64(2.0))].into()),
            );
        }
        assert!(v1::layout::require_supported_geometry(&LayoutValue::from_raw(raw).unwrap()).is_err());
    }
}
