//! Pure routing reproductions; no native cursor or platform behavior is simulated as evidence.
use super::*;
const ME: DeviceId = DeviceId([1; 32]);
const SOURCE: DeviceId = DeviceId([2; 32]);

fn fixture(os: Os) -> Router {
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: ME,
                nonce: [1; 16],
            },
            "target",
            os,
        ),
        super::connection_tests::session_layout(ME, SOURCE),
    );
    r.set_local_displays(vec![DisplayInfo {
        key: "screen".into(),
        name: "screen".into(),
        bounds: Rect::new(0., 0., 1000., 600.),
        pixel_width: 1000,
        pixel_height: 600,
        reference_extent: Some(oink_foundation::Size::new(250., 150.)),
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }]);
    r.set_cursor(Point::new(100., 100.));
    r.out.clear();
    r
}
fn step(r: &mut Router) -> Point {
    r.remote_motion(SOURCE, 2.5, 0., 1); // 0.25 reference units per native unit => +10.
    r.out
        .iter()
        .rev()
        .find_map(|o| match o {
            Output::Inject(InjectEvent::MoveTo { pos } | InjectEvent::MoveToWithDelta { pos, .. }) => Some(*pos),
            _ => None,
        })
        .expect("absolute target output")
}
#[test]
fn normal_motion_uses_virtual_position_on_both_targets() {
    for os in [Os::MacOS, Os::Windows] {
        let mut r = fixture(os);
        // No observation is delivered for a silent external warp: router cannot see it.
        assert_eq!(step(&mut r), Point::new(110., 100.));
        // A reconciled observation *before routing* changes the absolute target.
        r.set_cursor(Point::new(400., 100.));
        assert_eq!(step(&mut r), Point::new(410., 100.));
    }
}
#[test]
fn game_delta_does_not_use_or_advance_virtual_position_on_both_targets() {
    for os in [Os::MacOS, Os::Windows] {
        let mut r = fixture(os);
        r.set_game_mode(GameMode::On);
        r.out.clear();
        let Message::Control(ControlMsg::GameMode { generation, .. }) = r.input_game_state() else {
            panic!("game state")
        };
        r.receive_relative(SOURCE, r.epoch, generation, 1, 10., 0.);
        assert!(
            r.out
                .iter()
                .any(|o| matches!(o, Output::Inject(InjectEvent::MoveRelative { dx: 10., dy: 0. })))
        );
        assert_eq!(r.pointer.as_ref().unwrap().local, Point::new(100., 100.));
    }
}
#[test]
fn delayed_own_sample_must_not_erase_already_routed_motion() {
    let mut r = fixture(Os::Windows);
    assert_eq!(step(&mut r), Point::new(110., 100.));
    // Observer sampled 100 before the +10 commit; its message arrives afterwards.
    // This is a counterexample to adding unconditional 50ms set_cursor, NOT current normal-mode behavior.
    // The sequenced reconciler rejects that old observation, so the Router
    // receives no replacement base. The classifier itself is tested in input.
    r.reconcile_target_snapshot(None, None);
    assert_eq!(step(&mut r), Point::new(120., 100.));
}

#[test]
fn target_cursor_external_base_clears_stale_edge_without_emitting_a_warp() {
    for os in [Os::MacOS, Os::Windows] {
        let mut r = fixture(os);
        r.set_cursor(Point::new(999., 100.));
        r.out.clear();
        assert!(r.reconcile_target_cursor(Point::new(400., 100.)));
        assert!(r.out.is_empty());
        assert_eq!(step(&mut r), Point::new(410., 100.));
        assert_eq!(r.focus().holder, ME);
    }
}
#[test]
fn target_cursor_invalid_desktop_points_and_game_reads_do_not_replace_prediction() {
    let mut r = fixture(Os::Windows);
    for p in [Point::new(-1., 10.), Point::new(1001., 10.), Point::new(f64::NAN, 10.)] {
        assert!(!r.reconcile_target_cursor(p));
        assert_eq!(r.local_pointer(), Some(Point::new(100., 100.)));
    }
    r.set_game_mode(GameMode::On);
    assert!(!r.reconcile_target_cursor(Point::new(400., 100.)));
    assert_eq!(r.local_pointer(), Some(Point::new(100., 100.)));
}
#[test]
fn target_cursor_fresh_clip_and_actual_replace_obsolete_prediction_together() {
    let mut r = fixture(Os::Windows);
    r.set_cursor(Point::new(900., 100.));
    r.out.clear();
    r.reconcile_target_snapshot(
        Some(Point::new(300., 100.)),
        Some(Some(Rect::new(200., 50., 200., 200.))),
    );
    assert!(r.out.is_empty(), "do not first clamp the stale 900 to 399");
    assert_eq!(step(&mut r), Point::new(310., 100.));
    r.out.clear();
    r.reconcile_target_snapshot(Some(Point::new(600., 100.)), Some(None));
    assert_eq!(step(&mut r), Point::new(610., 100.));
}

#[test]
fn target_cursor_warp_back_to_saved_restore_point_is_not_an_own_match() {
    let mut r = fixture(Os::Windows);
    assert_eq!(step(&mut r), Point::new(110., 100.));
    // local_cursor still holds 100, but current prediction is 110.
    assert!(r.reconcile_target_cursor(Point::new(100., 100.)));
    assert_eq!(step(&mut r), Point::new(110., 100.));
}

// Sanitized current geometry from the failing three-device layout. S is the
// authoritative remote target, C is the source and destination below S2.
fn current_s2_bottom() -> Router {
    use oink_display::layout::{Layout, OwnerLayoutPreferences, ScreenPlacement, ScreenPreference, SizeSource};
    use std::collections::BTreeMap;
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/current_s2_bottom.json")).unwrap();
    let rect = |v: &serde_json::Value| {
        Rect::new(
            v["x"].as_f64().unwrap(),
            v["y"].as_f64().unwrap(),
            v["w"].as_f64().unwrap(),
            v["h"].as_f64().unwrap(),
        )
    };
    let mut layout = Layout {
        screens: vec![],
        settings: serde_json::from_value(v["settings"].clone()).unwrap(),
    };
    let mut owners = BTreeMap::<DeviceId, OwnerLayoutPreferences>::new();
    let mut displays = vec![];
    for s in v["screens"].as_array().unwrap() {
        let device = match s["device"].as_str().unwrap() {
            "S" => ME,
            "C" => SOURCE,
            _ => DeviceId([3; 32]),
        };
        let code = s["code"].as_str().unwrap();
        let local = rect(&s["local"]);
        layout.screens.push(ScreenPlacement {
            id: oink_display::layout::ScreenId::new(device, code),
            name: code.into(),
            local,
            rect: rect(&s["rect"]),
            size_source: SizeSource::Manual,
            primary: code.ends_with('1'),
            builtin: device != DeviceId([3; 32]) && code.ends_with('1'),
            device_hidden: false,
            screen_hidden: false,
            device_visibility_known: false,
            screen_visibility_known: false,
        });
        let owner = owners.entry(device).or_default();
        owner.screen_shared = !s["disabled"].as_bool().unwrap();
        owner.screens.insert(
            code.into(),
            ScreenPreference {
                enabled: !s["hidden"].as_bool().unwrap(),
                gain: s["gain"].as_f64().unwrap(),
                remote_gain: s["remote_gain"].as_f64().unwrap(),
            },
        );
        if device == ME {
            displays.push(DisplayInfo {
                key: code.into(),
                name: code.into(),
                bounds: local,
                pixel_width: (local.w * 2.) as u32,
                pixel_height: (local.h * 2.) as u32,
                reference_extent: None,
                rotation_deg: 0,
                is_primary: code == "S1",
                is_builtin: code == "S1",
                is_virtual: false,
            });
        }
    }
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: ME,
                nonce: [1; 16],
            },
            "target",
            Os::MacOS,
        ),
        Arc::new(crate::TopologyLayout::from_layout(&layout, &owners)),
    );
    r.set_local_displays(displays);
    assert!(r.attach_input(
        oink_foundation::session::Boot {
            device_id: SOURCE,
            nonce: [2; 16]
        },
        [2; 16],
        true,
        false,
        FeelProfile::default(),
        0
    ));
    r.input_message(
        SOURCE,
        Message::Control(ControlMsg::FocusState {
            lock_state: None,
            epoch: 0,
            holder: ME,
            scope: None,
            session: Some([2; 16]),
        }),
        1,
    );
    assert!(r.input_ready(SOURCE));
    r.set_edge_resistance_regions(vec![oink_foundation::input::EdgeResistanceRegion {
        display: "S2".into(),
        edges: vec![oink_foundation::input::ScreenEdge::Bottom],
        corners: vec![],
    }]);
    r.set_cursor(Point::new(2500., -0.1));
    r.out.clear();
    r
}

#[test]
fn edge_clamp_exception_requires_a_portal_and_a_real_outer_edge() {
    let mut r = current_s2_bottom();
    r.set_cursor(Point::new(2500., -0.000005));
    assert!(r.target_readback_constrained_point(Point::new(2500., -1.)).is_some());
    r.set_cursor(Point::new(2500., -0.5));
    assert!(
        !r.target_readback_constrained_point(Point::new(2500., -1.)).is_some(),
        "an ordinary near-edge prediction is not a parked edge"
    );
    r.set_cursor(Point::new(2500., -0.000005));
    r.cfg.os = Os::Windows;
    assert!(
        !r.target_readback_constrained_point(Point::new(2500., -1.)).is_some(),
        "Windows exact receipts handle their own clamp"
    );
    r.cfg.os = Os::MacOS;
    r.displays.push(DisplayInfo {
        key: "native-neighbour".into(),
        name: "native-neighbour".into(),
        bounds: Rect::new(2000., 0., 1000., 600.),
        ..r.displays[0].clone()
    });
    assert!(
        !r.target_readback_constrained_point(Point::new(2500., -1.)).is_some(),
        "a native seam is not OS confinement"
    );
    let mut wall = fixture(Os::MacOS);
    wall.set_cursor(Point::new(500., 599.999999));
    assert!(
        !wall.target_readback_constrained_point(Point::new(500., 599.)).is_some(),
        "no bottom portal"
    );
}
