//! Frozen execution boundary from public 0.3.3 build 100 (3c0f57552) and
//! in-house 101 (f403d7dbc). The files were byte-identical at those revisions.
use oink_protocol::v1::{
    self, ErrorCode, RawMap, RawValue, Result, dto, labels, limits, schema,
    schema::{WireDto, WireValue},
};
#[path = "fixtures/compat/layout_100_101.rs"]
mod legacy_layout;
#[allow(dead_code)]
#[path = "fixtures/compat/retained_100_101.rs"]
mod legacy_retained;
use oink_agent::config_module::{group_store::StoredGroupState, layout_wire};
use oink_display::layout::{Layout, ScreenId, ScreenPlacement, SizeSource};
use oink_foundation::Rect;
use oink_network::{admission, group_state::State, identity::Identity};

#[test]
fn c0_100_101_admit_current_layout_with_visibility_and_preserve_it_on_edit() {
    let a = Identity::from_seed([1; 32]);
    let b = Identity::from_seed([2; 32]);
    let fresh = State::create(&a, "new", "test", 1).unwrap();
    let base = fresh
        .with_roster(&fresh.roster().add(&a, b.id(), "old", "test", 2).unwrap())
        .unwrap();
    for hidden in [false, true] {
        let mut layout = Layout::default();
        layout.screens.push(ScreenPlacement {
            id: ScreenId::new(a.id(), "screen"),
            name: "screen".into(),
            local: Rect::new(0., 0., 1., 1.),
            rect: Rect::new(0., 0., 1., 1.),
            size_source: SizeSource::Reported,
            primary: true,
            builtin: false,
            device_hidden: hidden,
            screen_hidden: hidden,
            device_visibility_known: true,
            screen_visibility_known: true,
        });
        let value = layout_wire::to_wire(&layout, dto::LayoutSource::Manual).unwrap();
        let mut stored = StoredGroupState::new(a.id(), base.wire().clone()).unwrap();
        stored
            .edit_layout(&base.layout().unwrap().version().unwrap(), |raw| {
                *raw = value.into_raw();
                Ok(())
            })
            .unwrap();
        let unchanged = stored.layout_register().unwrap();
        assert!(!stored.edit_layout(&unchanged.version().unwrap(), |_| Ok(())).unwrap());
        assert_eq!(stored.layout_register().unwrap(), unchanged);
        let projected = v1::layout::register_value(&unchanged).unwrap();
        assert_eq!(
            projected,
            layout_wire::to_wire(&layout_wire::from_wire(&projected).unwrap(), dto::LayoutSource::Manual).unwrap()
        );
        let new = State::verify(stored.state, base.roster().anchor()).unwrap();
        let v1::Message::ConfigRequest(request) = admission::request(&new, a.id(), [7; 32], 0).unwrap() else {
            panic!()
        };
        let bytes = v1::frame::encode(&v1::Message::ConfigRequest(request), v1::limits::CONFIG_FRAME).unwrap();
        let v1::Message::ConfigRequest(request) = v1::frame::OpaqueFrame::parse_exact(&bytes, v1::limits::CONFIG_FRAME)
            .unwrap()
            .decode()
            .unwrap()
        else {
            panic!()
        };
        let old = admission::receive_request(&base, &request, b.id(), a.id(), [7; 32]).unwrap();
        let reg = old.layout().unwrap();
        legacy_retained::verify_register(&reg).unwrap();
        legacy_layout::require_supported_geometry(&dto::LayoutValue::from_raw(reg.value().unwrap()).unwrap())
            .expect("100/101 must execute the admitted geometry");
        let v1::Message::ConfigReply(reply) = admission::reply(&old, b.id(), [7; 32], 0).unwrap() else {
            panic!()
        };
        let accepted = admission::receive_reply(&new, &new, &reply, a.id(), b.id(), [7; 32]).unwrap();
        let v1::Message::ConfigDone(done) = admission::done(&accepted.acknowledged, [7; 32]).unwrap() else {
            panic!()
        };
        admission::receive_done(&old, &done, [7; 32]).unwrap();
        // The old editor preserves unknown register-envelope fields, hashes them,
        // and increments the same version. The new reader must recover visibility.
        let edited = legacy_retained::edit_register(
            &reg,
            &reg.version().unwrap(),
            oink_network::wire_identity::device(b.id()),
            |raw| {
                raw.insert(2, dto::LayoutSource::Default.write());
                Ok(())
            },
        )
        .unwrap();
        let mut state_raw = old.wire().raw().clone();
        let v1::RawValue::Array(registers) = state_raw.get_mut(&2).unwrap() else {
            panic!()
        };
        *registers
            .iter_mut()
            .find(|v| dto::Register::read(v).unwrap().kind().unwrap() == 2)
            .unwrap() = edited.write();
        let mut restored = StoredGroupState::new(a.id(), dto::GroupState::from_raw(state_raw).unwrap()).unwrap();
        // Exercise a native refresh through the same edit entry point.
        restored.edit_layout(&edited.version().unwrap(), |_| Ok(())).unwrap();
        let projected = layout_wire::from_register(&restored.layout_register().unwrap()).unwrap();
        assert_eq!(projected.screens[0].device_hidden, hidden);
    }
}
