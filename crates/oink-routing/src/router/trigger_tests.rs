//! Production topology/router scenarios. Native desktop acceptance is separate.
use super::*;
use crate::display_layout::TopologyLayout;
use oink_display::layout::{Layout, OwnerLayoutPreferences, ScreenPlacement, SizeSource, SpeedMode};
const A: DeviceId = DeviceId([1; 32]);
const B: DeviceId = DeviceId([2; 32]);
const SOURCE: DeviceId = DeviceId([3; 32]);
fn placement(device: DeviceId, rect: Rect) -> ScreenPlacement {
    ScreenPlacement {
        id: oink_display::layout::ScreenId::new(device, "screen"),
        name: "screen".into(),
        local: Rect::new(0., 0., 1000., 1000.),
        rect: Rect::new(rect.x / 686., rect.y / 686., rect.w / 686., rect.h / 686.),
        size_source: SizeSource::Manual,
        primary: true,
        builtin: false,
    }
}
fn owners(mode: SpeedMode) -> std::collections::BTreeMap<DeviceId, OwnerLayoutPreferences> {
    [(A, 2.), (B, 3.)]
        .into_iter()
        .map(|(id, device_gain)| {
            (
                id,
                OwnerLayoutPreferences {
                    device_gain,
                    speed_mode: mode,
                    ..Default::default()
                },
            )
        })
        .collect()
}
fn setup(reverse: bool, gap: bool, mode: SpeedMode) -> (Router, Layout) {
    let mut l = Layout {
        screens: vec![
            placement(A, Rect::new(0., 0., 200., 200.)),
            placement(B, Rect::new(220., 50., 200., 200.)),
        ],
        ..Default::default()
    };
    // Trigger zones are off by default; these tests exercise them at the former 8/686 reference units reach.
    l.settings.endpoint_reach = 8. / 686.;
    l.settings.gap_travel = gap;
    let owners = owners(mode);
    let me = if reverse { B } else { A };
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: me,
                nonce: [me.0[0]; 16],
            },
            "local",
            Os::Other,
        ),
        Arc::new(TopologyLayout::from_layout(&l, &owners)),
    );
    r.set_local_displays(vec![DisplayInfo {
        key: "screen".into(),
        name: "screen".into(),
        bounds: Rect::new(0., 0., 1000., 1000.),
        pixel_width: 1000,
        pixel_height: 1000,
        reference_extent: None,
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }]);
    for peer in [A, B, SOURCE].into_iter().filter(|d| *d != me) {
        r.test_connect(peer, 0);
        r.peers.get_mut(&peer).unwrap().focus_ready = true;
    }
    r.set_cursor(if reverse {
        Point::new(0.5, 775.)
    } else {
        Point::new(999., 225.)
    });
    r.drain_outputs();
    (r, l)
}
fn landing(r: &mut Router) -> Point {
    r.drain_outputs()
        .into_iter()
        .find_map(|o| match o {
            Output::Send {
                msg: Message::Control(ControlMsg::FocusEnter { pos, .. }),
                ..
            } => Some(pos),
            _ => None,
        })
        .expect("no focus enter")
}
fn near(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-3, "{a} != {b}");
}
#[test]
fn zones_cross_both_directions_with_local_or_remote_input_and_scaling() {
    for reverse in [false, true] {
        for remote in [false, true] {
            let (mut r, l) = setup(reverse, false, SpeedMode::Physical);
            let owners = owners(SpeedMode::Physical);
            let sign = if reverse { -1. } else { 1. };
            let from = r.pointer.clone().unwrap();
            let scale = r.layout.reference_per_native_unit(&from.screen).unwrap();
            let hand = 2. / 686.;
            if remote {
                r.remote_motion(SOURCE, sign * hand, 0., 1);
            } else {
                r.local_motion(sign * hand / scale, 0., Some(from.local), 1);
            }
            assert_eq!(r.focus().holder, if reverse { A } else { B });
            let p = landing(&mut r);
            let source = l.screens.iter().find(|s| s.id.device == from.screen.device).unwrap();
            let remaining = hand - (if reverse { 0.1 } else { 0.2 }) / (686. * owners[&source.id.device].device_gain);
            let target = if reverse { &l.screens[0] } else { &l.screens[1] };
            near(
                p.x,
                if reverse {
                    1000. - remaining * owners[&target.id.device].device_gain * (5. * 686.)
                } else {
                    remaining * owners[&target.id.device].device_gain * (5. * 686.)
                },
            );
            near(p.y, if reverse { 1000. } else { 0. });
        }
    }
}
#[test]
fn zones_pay_the_full_gap_at_the_leaving_scale_and_land_at_the_endpoint() {
    for reverse in [false, true] {
        for mode in [SpeedMode::Physical, SpeedMode::ScreenProportional] {
            let (mut r, l) = setup(reverse, true, mode);
            let sign = if reverse { -1. } else { 1. };
            let gain = l
                .topology(&owners(mode))
                .plane_gain(&oink_display::layout::ScreenId::new(r.me(), "screen"))
                .unwrap();
            r.remote_motion(SOURCE, (sign * 10. / gain) / 686., 0., 1);
            assert!(r.pointer_in_gap());
            assert_eq!(r.focus().holder, r.me());
            r.remote_motion(SOURCE, (sign * 10. / gain) / 686., 0., 2);
            assert!(
                r.pointer_in_gap(),
                "only 19.8/686 reference units of the 20/686 reference units gap travelled"
            );
            r.remote_motion(SOURCE, (sign * 1. / gain) / 686., 0., 3);
            assert_eq!(r.focus().holder, if reverse { A } else { B });
            let p = landing(&mut r);
            near(p.x, if reverse { 1000. } else { 0. });
            near(p.y, if reverse { 1000. } else { 0. });
        }
    }
}
#[test]
fn zone_native_held_drag_uses_the_channel_probe_and_hold_path() {
    use crate::dnd::DND_SEQ_BIT;
    use oink_foundation::content::ClipId;
    for reverse in [false, true] {
        let (mut r, _) = setup(reverse, true, SpeedMode::Physical);
        r.set_dnd_available(true);
        let peer = if reverse { A } else { B };
        r.test_message(peer, Message::Dnd(crate::dnd::DndMsg::Caps { enabled: true }), 0);
        r.local_button(MouseButton::Left, true);
        let pos = r.pointer.clone().unwrap().local;
        let dx = if reverse { -100. } else { 100. };
        r.local_motion(dx, 0., Some(pos), 1);
        assert_eq!(r.dnd_state_name(), "probing");
        assert!(!r.pointer_in_gap());
        let id = ClipId {
            origin: r.me(),
            seq: DND_SEQ_BIT | 1,
        };
        r.on_dnd_event(DndEvent::Detected {
            id,
            items: vec![oink_foundation::clipboard::ClipData::Text("drag".into())],
        });
        assert_eq!(r.dnd_state_name(), "leaving");
        assert!(
            r.drain_outputs()
                .iter()
                .any(|o| matches!(o,Output::Dnd(DndCmd::HoldSource {id:found,..}) if *found==id))
        );
    }
}
#[test]
fn zones_respect_lock_pause_disabled_targets_and_immediate_layout_edits() {
    for kind in 0..5 {
        let (mut r, mut l) = setup(false, false, SpeedMode::Physical);
        let mut owners = owners(SpeedMode::Physical);
        match kind {
            0 => r.set_locked(true),
            1 => r.set_paused(true),
            2 => {
                owners
                    .get_mut(&B)
                    .unwrap()
                    .screens
                    .entry("screen".into())
                    .or_default()
                    .enabled = false;
                r.set_layout(Arc::new(TopologyLayout::from_layout(&l, &owners)));
            }
            3 => {
                owners.get_mut(&B).unwrap().screen_shared = false;
                r.set_layout(Arc::new(TopologyLayout::from_layout(&l, &owners)));
            }
            _ => {
                l.settings.endpoint_reach = 0.;
                r.set_layout(Arc::new(TopologyLayout::from_layout(&l, &owners)));
            }
        }
        r.remote_motion(SOURCE, (10.) / 686., 0., 1);
        assert_eq!(r.focus().holder, A);
    }
}

#[derive(Default)]
struct Crossings(std::sync::Mutex<Vec<bool>>);
impl RoutingDiagnostics for Crossings {
    fn enabled(&self) -> bool {
        true
    }
    #[allow(clippy::too_many_arguments)]
    fn crossing_with_zone(
        &self,
        _layout: Arc<dyn LayoutQuery>,
        _epoch: u64,
        _from: &ScreenPos,
        _delta: Point,
        _local: bool,
        _gap: bool,
        zone: bool,
        _to: &ScreenPos,
        _departure: DepartureEvidence<'_>,
    ) {
        self.0.lock().unwrap().push(zone);
    }
    fn decision(
        &self,
        _kind: DecisionKind,
        _reason: DecisionReason,
        _epoch: u64,
        _holder: DeviceId,
        _target: Option<&ScreenPos>,
    ) {
    }
}

#[test]
fn crossing_evidence_distinguishes_zones_channels_and_native_held_drags() {
    for gap in [false, true] {
        for zone in [false, true] {
            let (mut r, _) = setup(false, gap, SpeedMode::Physical);
            if !zone {
                r.set_cursor(Point::new(999., 400.));
            }
            let trace = Arc::new(Crossings::default());
            r.diagnostics = Some(trace.clone());
            r.remote_motion(SOURCE, (15.) / 686., 0., 1);
            assert_eq!(*trace.0.lock().unwrap(), vec![zone]);
        }
    }
    let (mut r, _) = setup(false, true, SpeedMode::Physical);
    let trace = Arc::new(Crossings::default());
    r.diagnostics = Some(trace.clone());
    r.set_dnd_available(true);
    r.local_button(MouseButton::Left, true);
    let p = r.pointer.clone().unwrap().local;
    r.local_motion(100., 0., Some(p), 1);
    let id = oink_foundation::content::ClipId {
        origin: A,
        seq: crate::dnd::DND_SEQ_BIT | 1,
    };
    r.on_dnd_event(DndEvent::Detected {
        id,
        items: vec![oink_foundation::clipboard::ClipData::Text("drag".into())],
    });
    r.on_dnd_event(DndEvent::SourceHeld {
        id,
        epoch: r.epoch,
        ok: true,
    });
    assert_eq!(*trace.0.lock().unwrap(), vec![true]);
}

#[test]
fn zone_damping_and_gap_return_keep_normal_edge_pressure_and_cancel_rules() {
    use crate::damping::{DampingConfig, DampingProfile, DampingRule, DampingSegment};
    let (mut r, _) = setup(false, false, SpeedMode::Physical);
    r.cfg.settings.damping = DampingConfig {
        rules: vec![DampingRule {
            segment: DampingSegment {
                source: None,
                target: None,
                display: None,
                edge: oink_foundation::input::ScreenEdge::Right,
                start: 0.,
                end: 1.,
            },
            profile: DampingProfile::default(),
        }],
        ..Default::default()
    };
    r.remote_motion(SOURCE, (0.2) / 686., 0., 1);
    assert_eq!(r.focus().holder, A);
    let (mut r, _) = setup(false, true, SpeedMode::Physical);
    r.remote_motion(SOURCE, (5.) / 686., 0., 1);
    assert!(r.pointer_in_gap());
    r.remote_motion(SOURCE, (-6.) / 686., 0., 2);
    assert!(!r.pointer_in_gap());
    assert_eq!(r.focus().holder, A);
    near(r.local_pointer().unwrap().y, 225.);
}

fn native_geometry(reach: f64) -> Layout {
    let mut layout = Layout {
        screens: vec![
            placement(A, Rect::new(0., 0., 200., 200.)),
            placement(B, Rect::new(220., 50., 200., 200.)),
        ],
        ..Default::default()
    };
    layout.screens[0].local = Rect::new(0., 0., 1728., 1117.);
    layout.screens[1].local = Rect::new(0., 0., 3840., 2160.);
    layout.settings.portal_snap = 0.;
    layout.settings.endpoint_reach = reach / 686.;
    layout
}

fn native_router(layout: &Layout, source: DeviceId, target_gain: f64) -> Router {
    let screen = layout.screens.iter().find(|s| s.id.device == source).unwrap();
    let os = if source == B { Os::Windows } else { Os::MacOS };
    let mut router = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: source,
                nonce: [source.0[0]; 16],
            },
            "source",
            os,
        ),
        Arc::new(TopologyLayout::from_layout(
            layout,
            &std::collections::BTreeMap::from([(
                A,
                OwnerLayoutPreferences {
                    device_gain: target_gain,
                    ..Default::default()
                },
            )]),
        )),
    );
    router.set_local_displays(vec![DisplayInfo {
        key: "screen".into(),
        name: "screen".into(),
        bounds: screen.local,
        pixel_width: if source == B { 3840 } else { 3456 },
        pixel_height: if source == B { 2160 } else { 2234 },
        reference_extent: None,
        rotation_deg: 0,
        is_primary: true,
        is_builtin: source == A,
        is_virtual: false,
    }]);
    let peer = if source == B { A } else { B };
    router.test_connect(peer, 0);
    router.peers.get_mut(&peer).unwrap().focus_ready = true;
    router
}

#[test]
fn in_channel_edge_arrival_does_not_replay_completed_tangential_motion() {
    // Installed-product geometry, with anonymous IDs and native units retained:
    // the target is 1728x1117 points, the source 3840x2160 physical pixels.
    // Both plane rectangles are 200x200/686 reference units with a 20/686 reference units gap and a 50/686 reference units offset.
    // The failing frame arrived from the source's centre at (0,540). Its -540
    // tangent already moved the native cursor; normal pressure is unchanged.
    for reach in [8., 0.] {
        let layout = native_geometry(reach);
        let topology = TopologyLayout::from_layout(&layout, &Default::default());
        let edge = ScreenPos::new(ScreenId::new(B, "screen"), Point::new(0., 540.));
        let portal = topology.first_portal(&edge, (-1., 0.)).unwrap();
        assert!(
            !portal.trigger_zone,
            "y=100/686 reference units is inside the [50,200] channel"
        );
        near(portal.to.local.y, 558.5);

        let mut r = native_router(&layout, B, 1.);
        let trace = Arc::new(Crossings::default());
        r.diagnostics = Some(trace.clone());
        r.set_cursor(Point::new(1920., 1080.));
        r.drain_outputs();

        r.local_motion(-1920., -540., Some(edge.local), 1);
        assert_eq!(r.focus().holder, A);
        let to = landing(&mut r);
        near(to.x, 863.99999136);
        near(to.y, 558.5);
        assert_eq!(*trace.0.lock().unwrap(), vec![false]);
        let after_push = topology.step(
            &ScreenPos::new(ScreenId::new(A, "screen"), to),
            (-0.2777777777777778 / 686., 0.),
        );
        near(after_push.to.local.x, 861.59999136);
        near(after_push.to.local.y, 558.5);

        // An actual outward push from an established edge position still keeps
        // its full remainder, as in the recorded 0/686 reference units baseline.
        let mut r = native_router(&layout, B, 1.);
        r.set_cursor(edge.local);
        r.drain_outputs();
        r.local_motion(-3., 0., Some(edge.local), 2);
        assert_eq!(r.focus().holder, A);
        let to = landing(&mut r);
        near(to.x, 1726.64999136);
        near(to.y, 558.5);
    }
}

#[test]
fn native_edge_band_keeps_the_existing_normal_pressure() {
    for reach in [0., 8.] {
        let mut router = native_router(&native_geometry(reach), B, 1.);
        router.set_cursor(Point::new(1.5, 540.));
        router.drain_outputs();
        router.local_motion(-1., 0., Some(Point::new(0.5, 540.)), 1);
        assert_eq!(router.focus().holder, A);
        let to = landing(&mut router);
        near(to.x, 1727.77499136);
        near(to.y, 558.5);
    }
}

#[test]
fn in_channel_right_edge_keeps_the_existing_normal_pressure() {
    for reach in [0., 8.] {
        let mut router = native_router(&native_geometry(reach), A, 1.);
        router.set_cursor(Point::new(864., 558.5));
        router.drain_outputs();
        router.local_motion(864., 0., Some(Point::new(1727.999999, 558.5)), 1);
        assert_eq!(router.focus().holder, B);
        let to = landing(&mut router);
        near(to.x, 1919.9999808);
        near(to.y, 540.);
    }
}

#[test]
fn native_edge_pressure_after_an_independent_cursor_change_uses_the_current_position() {
    let mut router = native_router(&native_geometry(8.), A, 1.);
    router.set_cursor(Point::new(864., 558.5));
    router.drain_outputs();
    // A display/OS placement changed the cursor independently of this small
    // outward frame. The cached centre cannot be its previous native position.
    router.local_motion(2., 0., Some(Point::new(1727.999, 500.)), 1);
    assert_eq!(router.focus().holder, B);
    let to = landing(&mut router);
    near(to.y, 500. * 2160. / 1117. - 540.);
}

#[test]
fn native_approach_keeps_normal_pressure_and_target_gain_without_replaying_tangent() {
    for reach in [0., 8.] {
        for (end_y, dy, zone) in [(540., -540., false), (1674., 594., true)] {
            let mut router = native_router(&native_geometry(reach), B, 2.);
            let trace = Arc::new(Crossings::default());
            router.diagnostics = Some(trace.clone());
            router.set_cursor(Point::new(1920., 1080.));
            router.drain_outputs();
            router.local_motion(-2000., dy, Some(Point::new(0., end_y)), 1);
            if zone && reach == 0. {
                assert_eq!(router.focus().holder, B);
                assert!(trace.0.lock().unwrap().is_empty());
                continue;
            }
            assert_eq!(router.focus().holder, A);
            let to = landing(&mut router);
            // The normal pressure still receives target gain 2 and reaches
            // its far wall. Tangent movement was already applied by the OS.
            near(to.x, 0.);
            near(to.y, if zone { 1117. } else { 558.5 });
            assert_eq!(*trace.0.lock().unwrap(), vec![zone]);
        }
    }
}

#[test]
fn mouse_local_receiver_hides_in_gap_and_reveals_without_stealing_the_travelling_pointer() {
    use oink_foundation::input::InputMode;
    for reveal in [false, true] {
        let (mut r, l) = setup(false, true, SpeedMode::Physical);
        let mut settings = r.config().settings.clone();
        settings.mouse_mode = InputMode::Local;
        r.set_settings(settings);
        r.drain_outputs();
        let gain = l
            .topology(&owners(SpeedMode::Physical))
            .plane_gain(&oink_display::layout::ScreenId::new(r.me(), "screen"))
            .unwrap();
        r.remote_motion(SOURCE, 10. / (gain * 686.), 0., 1);
        assert!(r.pointer_in_gap());
        assert!(
            r.out
                .iter()
                .any(|o| matches!(o, Output::SetMode(CaptureMode::Remote { .. })))
        );
        r.out.clear();
        if reveal {
            let pointer = r.pointer.clone();
            let claim = r.focus_claim();
            r.on_local_event(
                &CapturedEvent {
                    physical: true,
                    event: InputEvent::PointerDelta { dx: -10., dy: 2. },
                    source: SourceKind::Mouse,
                    time_us: 2,
                    cursor: None,
                },
                2,
            );
            assert!(r.out.iter().any(|o| matches!(o, Output::RestoreLocal { .. })));
            assert_eq!(r.pointer, pointer);
            assert_eq!(r.focus_claim(), claim);
            assert!(r.pointer_in_gap());
            r.out.clear();
        }
        r.remote_motion(SOURCE, 20. / (gain * 686.), 0., 3);
        assert_eq!(r.holder, B);
        assert_eq!(
            r.out
                .iter()
                .any(|o| matches!(o, Output::SetMode(CaptureMode::Remote { .. }))),
            !reveal,
            "an independent cursor already revealed in the gap belongs to the local mouse"
        );
    }
}

#[test]
fn delayed_top_edge_detection_uses_current_tangent_mapping() {
    for os in [Os::MacOS, Os::Windows] {
        for remote in [false, true] {
            for before_detection in [false, true] {
                let layout = Layout {
                    screens: vec![
                        placement(A, Rect::new(100., 220., 200., 200.)),
                        placement(B, Rect::new(0., 0., 400., 200.)),
                    ],
                    ..Default::default()
                };
                let mut r = native_router(&layout, A, 1.);
                r.cfg.os = os;
                r.cfg.settings.dnd_push_fraction = 0.02;
                r.test_connect(SOURCE, 0);
                r.peers.get_mut(&SOURCE).unwrap().focus_ready = true;
                for peer in [B, SOURCE] {
                    r.test_message(peer, Message::Dnd(crate::dnd::DndMsg::Caps { enabled: true }), 0);
                }
                r.set_dnd_available(true);
                r.set_cursor(Point::new(600., 0.));
                if remote {
                    r.held.entry(SOURCE).or_default().buttons.insert(MouseButton::Left);
                    r.button_refs.insert(MouseButton::Left, 1);
                    let scale = r
                        .layout
                        .reference_per_native_unit(&r.pointer.as_ref().unwrap().screen)
                        .unwrap();
                    r.remote_motion(SOURCE, 0., -40. * scale, 1);
                    if before_detection {
                        r.remote_motion(SOURCE, -100. * scale, 0., 2);
                    }
                } else {
                    r.local_button(MouseButton::Left, true);
                    r.local_motion(0., -40., Some(Point::new(600., 0.)), 1);
                    if before_detection {
                        r.local_motion(-100., 0., Some(Point::new(500., 0.)), 2);
                    }
                }
                assert_eq!(r.dnd_state_name(), "probing");
                r.on_dnd_event(DndEvent::Detected {
                    id: oink_foundation::content::ClipId {
                        origin: A,
                        seq: crate::dnd::DND_SEQ_BIT | 1,
                    },
                    items: vec![oink_foundation::clipboard::ClipData::Text("drag".into())],
                });
                if !before_detection {
                    if remote {
                        let scale = r
                            .layout
                            .reference_per_native_unit(&r.pointer.as_ref().unwrap().screen)
                            .unwrap();
                        r.remote_motion(SOURCE, -100. * scale, 0., 3);
                    } else {
                        r.local_motion(-100., 0., Some(Point::new(500., 0.)), 3);
                    }
                }
                let expected = r
                    .layout
                    .exit_through_edge(r.pointer.as_ref().unwrap(), (0., -2.))
                    .unwrap();
                let dnd::State::Leaving { id, epoch, .. } = &r.dnd.state else {
                    panic!("expected native hold request")
                };
                let (id, epoch) = (*id, *epoch);
                r.on_dnd_event(DndEvent::SourceHeld { id, epoch, ok: true });
                near(landing(&mut r).x, expected.local.x);
            }
        }
    }
}

// Keep these review regressions on the production topology, whose wall sliding
// and rigid same-computer seams are intentionally richer than SimpleLayout.
fn review_drag_corner_setup(os: Os, remote: bool, hosted: bool, left_portal: bool, detect: bool, depth: f64) -> Router {
    let mut l = Layout {
        screens: vec![
            placement(A, Rect::new(0., 0., 200., 200.)),
            placement(B, Rect::new(0., 220., 200., 200.)),
        ],
        ..Default::default()
    };
    if left_portal {
        let mut left = placement(B, Rect::new(-220., 0., 200., 200.));
        left.id.display = "left".into();
        // Both peer screens use one rigid native-to-canvas transform.
        left.local = Rect::new(-1100., -1100., 1000., 1000.);
        l.screens.push(left);
    }
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: A,
                nonce: [1; 16],
            },
            "local",
            os,
        ),
        Arc::new(TopologyLayout::from_layout(&l, &Default::default())),
    );
    r.cfg.settings.dnd_push_fraction = 0.02;
    r.set_local_displays(vec![DisplayInfo {
        key: "screen".into(),
        name: "screen".into(),
        bounds: Rect::new(0., 0., 1000., 1000.),
        pixel_width: 1000,
        pixel_height: 1000,
        reference_extent: None,
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }]);
    for peer in [B, SOURCE] {
        r.test_connect(peer, 0);
        r.peers.get_mut(&peer).unwrap().focus_ready = true;
        r.test_message(peer, Message::Dnd(crate::dnd::DndMsg::Caps { enabled: true }), 0);
    }
    r.set_dnd_available(true);
    r.set_cursor(Point::new(0., 999.));
    let input = if remote { SOURCE } else { A };
    if remote {
        r.held.entry(input).or_default().buttons.insert(MouseButton::Left);
        r.button_refs.insert(MouseButton::Left, 1);
    } else {
        r.local_button(MouseButton::Left, true);
    }
    if hosted {
        r.dnd.state = dnd::State::Hosting {
            id: oink_foundation::content::ClipId {
                origin: B,
                seq: crate::dnd::DND_SEQ_BIT | 1,
            },
            input,
            items: vec![oink_foundation::clipboard::ClipData::Text("drag".into())],
            push: None,
        };
        r.held.entry(input).or_default().buttons.insert(MouseButton::Left);
        r.button_refs.insert(MouseButton::Left, 1);
        if !remote {
            r.buttons.insert(MouseButton::Left, Target::LocalInjected);
        }
    }
    review_drag_motion(&mut r, remote, 0., depth, 1);
    if !hosted && detect {
        r.on_dnd_event(DndEvent::Detected {
            id: oink_foundation::content::ClipId {
                origin: A,
                seq: crate::dnd::DND_SEQ_BIT | 1,
            },
            items: vec![oink_foundation::clipboard::ClipData::Text("drag".into())],
        });
        assert_eq!(r.dnd_state_name(), "ready");
    } else if hosted {
        assert!(matches!(r.dnd.state, dnd::State::Hosting { push: Some(_), .. }));
    }
    r.drain_outputs();
    r
}
fn review_drag_motion(r: &mut Router, remote: bool, dx: f64, dy: f64, now: Micros) {
    if remote {
        let scale = r
            .layout
            .reference_per_native_unit(&r.pointer.as_ref().unwrap().screen)
            .unwrap();
        r.remote_motion(SOURCE, dx * scale, dy * scale, now);
    } else {
        let p = r.pointer.as_ref().unwrap().local;
        let c = Rect::new(0., 0., 1000., 1000.).clamp(Point::new(p.x + dx, p.y + dy));
        r.local_motion(dx, dy, Some(c), now);
    }
}

fn review_drag_is_pending(r: &Router, hosted: bool) -> bool {
    if hosted {
        matches!(r.dnd.state, dnd::State::Hosting { push: Some(_), .. })
    } else {
        matches!(r.dnd.state, dnd::State::Probing { content: Some(_), .. })
    }
}

#[test]
fn review_corner_wall_motion_must_not_count_as_portal_normal_pressure() {
    let mut failures = Vec::new();
    for os in [Os::MacOS, Os::Windows] {
        for remote in [false, true] {
            for hosted in [false, true] {
                let mut r = review_drag_corner_setup(os, remote, hosted, false, true, 10.);
                let mut control = review_drag_corner_setup(os, remote, hosted, false, true, 10.);
                review_drag_motion(&mut control, remote, 0., 2., 2);
                assert!(
                    review_drag_is_pending(&control, hosted),
                    "normal-only control must stay on source"
                );
                // Left is a wall; bottom is the only portal. Of this frame only
                // two units press toward the portal; 100 units hit the wall.
                review_drag_motion(&mut r, remote, -100., 2., 2);
                if !review_drag_is_pending(&r, hosted) {
                    failures.push(format!(
                        "{os:?} remote={remote} hosted={hosted}: left-wall distance changed the pending bottom drag to {}", r.dnd_state_name()
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn review_corner_new_edge_to_same_peer_requires_its_own_pressure() {
    let mut failures = Vec::new();
    for os in [Os::MacOS, Os::Windows] {
        for remote in [false, true] {
            for hosted in [false, true] {
                let mut r = review_drag_corner_setup(os, remote, hosted, true, true, 10.);
                let mut control = review_drag_corner_setup(os, remote, hosted, true, true, 2.);
                review_drag_motion(&mut control, remote, -20., 0., 2);
                assert!(
                    review_drag_is_pending(&control, hosted),
                    "20 units alone must not cross"
                );
                // The new left portal belongs to the same peer, but its 20-unit
                // push is below the 28.28-unit threshold. Prior bottom pressure
                // must not become left-edge pressure.
                review_drag_motion(&mut r, remote, -20., 0., 2);
                if !review_drag_is_pending(&r, hosted) {
                    failures.push(format!(
                        "{os:?} remote={remote} hosted={hosted}: bottom pressure leaked into the left portal; state={}",
                        r.dnd_state_name()
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn review_delayed_detection_lands_at_current_tangent_position() {
    let mut failures = Vec::new();
    for os in [Os::MacOS, Os::Windows] {
        for remote in [false, true] {
            // The normal push has crossed the threshold, but native metadata
            // has not arrived yet. A tangent frame precedes that callback.
            let mut r = review_drag_corner_setup(os, remote, false, false, false, 40.);
            assert_eq!(r.dnd_state_name(), "probing");
            review_drag_motion(&mut r, remote, 100., 0., 2);
            assert_eq!(r.dnd_state_name(), "probing");
            let current = r.pointer.as_ref().unwrap();
            let expected = r.layout.exit_through_edge(current, (0., 2.)).unwrap();
            r.on_dnd_event(DndEvent::Detected {
                id: oink_foundation::content::ClipId {
                    origin: A,
                    seq: crate::dnd::DND_SEQ_BIT | 1,
                },
                items: vec![oink_foundation::clipboard::ClipData::Text("drag".into())],
            });
            let dnd::State::Leaving { gesture, .. } = &r.dnd.state else {
                panic!("expected native hold request")
            };
            let dst = &gesture.landing;
            if (dst.local.x - expected.local.x).abs() > 1e-3 {
                failures.push(format!(
                    "{os:?} remote={remote}: landing x={} but current tangent maps to {}",
                    dst.local.x, expected.local.x
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn review_corner_plain_pointer_honors_actual_portal_damping() {
    use oink_foundation::input::{EdgeResistanceRegion, ScreenEdge};
    let mut r = review_drag_corner_setup(Os::MacOS, false, false, false, false, 10.);
    r.local_button(MouseButton::Left, false);
    assert_eq!(r.dnd_state_name(), "idle");
    r.set_edge_resistance_regions(vec![EdgeResistanceRegion {
        display: "screen".into(),
        edges: vec![ScreenEdge::Bottom],
        corners: vec![],
    }]);
    // Two units of normal motion must not bypass the protected bottom edge
    // just because the first geometric collision is the unprotected left wall.
    review_drag_motion(&mut r, false, -100., 2., 2);
    assert_eq!(r.focus().holder, A, "left wall normal bypassed bottom-edge damping");
}
