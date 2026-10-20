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

fn with_remote_gains(r: &mut Router, layout: &Layout, a: f64, b: f64) {
    let mut preferences = owners(SpeedMode::Physical);
    for (device, remote_gain) in [(A, a), (B, b)] {
        preferences.get_mut(&device).unwrap().screens.insert(
            "screen".into(),
            oink_display::layout::ScreenPreference {
                remote_gain,
                ..Default::default()
            },
        );
    }
    r.layout = Arc::new(TopologyLayout::from_layout(layout, &preferences));
}

#[test]
fn remote_screen_gain_scales_injection_exactly_and_preserves_local_motion() {
    for os in [Os::Other, Os::Windows] {
        let (mut r, layout) = setup(false, false, SpeedMode::Physical);
        with_remote_gains(&mut r, &layout, 2., 0.5);
        r.cfg.os = os;
        r.set_cursor(Point::new(500., 500.));
        r.remote_motion(SOURCE, 1. / 686., -0.5 / 686., 1);
        near(r.local_pointer().unwrap().x, 520.);
        near(r.local_pointer().unwrap().y, 490.);
        let outputs = r.drain_outputs();
        assert!(outputs.iter().any(|output| matches!(output,
            Output::Inject(InjectEvent::MoveTo { pos }) if (pos.x - 520.).abs() < 1e-9 && (pos.y - 490.).abs() < 1e-9) || matches!(output,
            Output::Inject(InjectEvent::MoveToWithDelta { pos, dx, dy })
                if (pos.x - 520.).abs() < 1e-9 && (pos.y - 490.).abs() < 1e-9 && (*dx - 20.).abs() < 1e-9 && (*dy + 10.).abs() < 1e-9)));
        let disposition = r.on_local_event(
            &CapturedEvent {
                event: InputEvent::PointerDelta { dx: 10., dy: 0. },
                source: SourceKind::Mouse,
                physical: true,
                time_us: 2,
                cursor: Some(Point::new(530., 490.)),
            },
            2,
        );
        assert_eq!(disposition, Disposition::Pass);
        assert_eq!(r.local_pointer(), Some(Point::new(530., 490.)));
        assert!(!r.drain_outputs().iter().any(|o| matches!(o, Output::Inject(_))));
    }
}

#[test]
fn remote_screen_gain_splits_remaining_motion_at_each_screen_and_excludes_its_source() {
    let (mut r, mut layout) = setup(false, false, SpeedMode::Physical);
    layout.screens[1].rect.y = 0.;
    with_remote_gains(&mut r, &layout, 2., 0.5);
    let from = ScreenPos::new(ScreenId::new(A, "screen"), Point::new(990., 500.));
    // 0.5 mm of hand motion reaches A's edge (gain 4); the remaining
    // 4.5 mm uses B's gain 1.5, yielding 33.75 native units.
    let to = r.layout.step_from_source(&from, (5. / 686., 0.), SOURCE).to;
    assert_eq!(to.screen.device, B);
    assert!((to.local.x - 33.75).abs() < 1e-3, "{to:?}");
    // A's native mouse ignores A's remote gain, including its source scale.
    let native = r.layout.exit_through_edge(&from, (20., 0.)).unwrap();
    assert_eq!(native.screen.device, B);
    assert!((native.local.x - 7.5).abs() < 1e-3, "{native:?}");
    let scale = r.layout.source_reference_per_native_unit(&from.screen).unwrap();
    assert!((scale - 0.1 / 686.).abs() < 1e-12);
    let back = r
        .layout
        .step_from_source(
            &ScreenPos::new(ScreenId::new(B, "screen"), Point::new(1., 500.)),
            (-1. / 686., 0.),
            A,
        )
        .to;
    assert_eq!(back.screen.device, A);
    // Returning to the input owner's screen uses its existing gain 2, not 4.
    assert!(
        (back.local.x - (1000. - (1. - 0.2 / 1.5) * 10.)).abs() < 1e-3,
        "{back:?}"
    );
}

#[test]
fn remote_screen_gain_drives_gap_travel_and_wall_delta_once() {
    let (mut r, layout) = setup(false, true, SpeedMode::Physical);
    with_remote_gains(&mut r, &layout, 2., 0.5);
    r.cfg.settings.damping.platform_profile = crate::damping::DampingProfile::DepthOnly { depth: 0. };
    r.set_cursor(Point::new(990., 500.));
    r.remote_motion(SOURCE, 2. / 686., 0., 1);
    assert!(r.pointer_in_gap());
    let head = r.trail_pointer().unwrap();
    assert!((head.x - 1030.).abs() < 1e-9, "{head:?}");
    // Once outside the screen, use the same leaving-screen motion factor.
    r.remote_motion(SOURCE, 1. / 686., 0., 2);
    assert!((r.trail_pointer().unwrap().x - 1050.).abs() < 1e-9);
}

#[test]
fn game_relative_motion_bypasses_device_screen_mode_and_remote_gains() {
    let (mut target, layout) = setup(true, false, SpeedMode::ScreenProportional);
    let (mut source, _) = setup(false, false, SpeedMode::ScreenProportional);
    let mut preferences = owners(SpeedMode::ScreenProportional);
    for owner in preferences.values_mut() {
        owner.screens.insert(
            "screen".into(),
            oink_display::layout::ScreenPreference {
                gain: 3.,
                remote_gain: 4.,
                ..Default::default()
            },
        );
    }
    let topology = Arc::new(TopologyLayout::from_layout(&layout, &preferences));
    target.layout = topology.clone();
    source.layout = topology;
    target.test_message(
        A,
        Message::Control(ControlMsg::FocusEnter {
            epoch: 1,
            display: "screen".into(),
            pos: Point::new(500., 500.),
            scope: Some(target.new_focus_scope()),
            pointer_source: Some(A),
        }),
        1,
    );
    target.set_game_mode(GameMode::On);
    assert!(target.game_mode().active);
    let generation = target
        .drain_outputs()
        .into_iter()
        .find_map(|output| match output {
            Output::Send {
                msg:
                    Message::Control(ControlMsg::GameMode {
                        generation,
                        active: true,
                        ..
                    }),
                ..
            } => Some(generation),
            _ => None,
        })
        .expect("game holder publication");
    source.become_source(1, B);
    source.test_message(
        B,
        Message::Control(ControlMsg::GameMode {
            epoch: 1,
            generation,
            mode: GameMode::On,
            detected: false,
            active: true,
            cursor_hiding_cause_available: None,
        }),
        2,
    );
    assert!(source.game_mode().active);
    source.drain_outputs();
    target.drain_outputs();
    source.raw_motion(17., -9.);
    let relative = source
        .drain_outputs()
        .into_iter()
        .find_map(|output| match output {
            Output::Send {
                msg: Message::Input(msg @ InputMsg::RelativeMotion { .. }),
                ..
            } => Some(msg),
            _ => None,
        })
        .expect("relative source packet");
    assert!(matches!(relative, InputMsg::RelativeMotion { x: 17., y: -9., .. }));
    target.test_message(A, Message::Input(relative), 3);
    assert!(
        target
            .drain_outputs()
            .iter()
            .any(|output| matches!(output, Output::Inject(InjectEvent::MoveRelative { dx: 17., dy: -9. })))
    );
    assert_eq!(target.local_pointer(), Some(Point::new(500., 500.)));
}

#[test]
fn separate_screens_on_one_device_use_their_own_remote_gains() {
    let (mut r, mut layout) = setup(false, false, SpeedMode::Physical);
    layout.screens[1].id = oink_display::layout::ScreenId::new(A, "second");
    layout.screens[1].local.x = 1000.;
    layout.screens[1].rect.y = 0.;
    let mut preferences = owners(SpeedMode::Physical);
    preferences.get_mut(&A).unwrap().screens = [
        (
            "screen".into(),
            oink_display::layout::ScreenPreference {
                remote_gain: 2.,
                ..Default::default()
            },
        ),
        (
            "second".into(),
            oink_display::layout::ScreenPreference {
                remote_gain: 0.5,
                ..Default::default()
            },
        ),
    ]
    .into();
    r.layout = Arc::new(TopologyLayout::from_layout(&layout, &preferences));
    r.test_message(
        SOURCE,
        Message::Control(ControlMsg::FocusEnter {
            epoch: 1,
            display: "screen".into(),
            pos: Point::new(990., 500.),
            scope: Some(r.new_focus_scope()),
            pointer_source: Some(SOURCE),
        }),
        0,
    );
    r.remote_motion(SOURCE, 5. / 686., 0., 1);
    let pos = r.pointer.as_ref().unwrap();
    assert_eq!(pos.screen, ScreenId::new(A, "second"));
    assert!((pos.local.x - 1022.5).abs() < 1e-3, "{pos:?}");
    assert_eq!(r.focus().holder, A);
    r.drain_outputs();
    // A packet relayed by B retains SOURCE as its physical origin and scales once.
    r.test_message(
        B,
        Message::Input(InputMsg::Forwarded {
            origin: r.admitted_boots[&SOURCE],
            hops: 1,
            msg: Box::new(InputMsg::Motion {
                seq: 1,
                dx: 1. / 686.,
                dy: 0.,
            }),
        }),
        2,
    );
    assert!((r.pointer.as_ref().unwrap().local.x - 1027.5).abs() < 1e-3);
}

/// Above-primary native geometry with a short touching bottom segment. The
/// physical layout sends the exposed segment to a peer below the external.
fn multidisplay_bottom_setup(backing_scale: u32) -> Router {
    multidisplay_setup(backing_scale, false)
}

fn multidisplay_setup(backing_scale: u32, gap_travel: bool) -> Router {
    use oink_foundation::input::{EdgeResistanceRegion, ScreenEdge};
    let primary = Rect::new(0., 0., 1728., 1117.);
    let external = Rect::new(1560., -1080., 1920., 1080.);
    let mut screens = Vec::new();
    for (device, key, local, physical) in [
        (A, "primary", primary, primary),
        (A, "external", external, external),
        (
            B,
            "screen",
            Rect::new(-1920., -1080., 1920., 1080.),
            Rect::new(1728., 0., 1920., 1080.),
        ),
    ] {
        let mut screen = placement(device, physical);
        screen.id.display = key.into();
        screen.local = local;
        screens.push(screen);
    }
    let mut layout = Layout {
        screens,
        ..Default::default()
    };
    layout.settings.gap_travel = gap_travel;
    layout.settings.endpoint_reach = 0.;
    layout.settings.portal_snap = 0.;
    let mut r = native_router(&layout, A, 1.);
    r.set_local_displays(
        [("primary", primary, 2), ("external", external, backing_scale)]
            .into_iter()
            .map(|(key, bounds, scale)| DisplayInfo {
                key: key.into(),
                name: key.into(),
                bounds,
                pixel_width: bounds.w as u32 * scale,
                pixel_height: bounds.h as u32 * scale,
                reference_extent: None,
                rotation_deg: 0,
                is_primary: key == "primary",
                is_builtin: key == "primary",
                is_virtual: false,
            })
            .collect(),
    );
    r.set_edge_resistance_regions(
        ["primary", "external"]
            .into_iter()
            .map(|display| EdgeResistanceRegion {
                display: display.into(),
                edges: vec![ScreenEdge::Bottom],
                corners: vec![],
            })
            .collect(),
    );
    r.cfg.settings.damping.platform_profile = crate::damping::DampingProfile::DepthOnly { depth: 0.5 };
    r.cfg.settings.dnd_push_fraction = 0.001;
    for peer in [B, SOURCE] {
        r.test_connect(peer, 0);
        r.peers.get_mut(&peer).unwrap().focus_ready = true;
        r.test_message(peer, Message::Dnd(crate::dnd::DndMsg::Caps { enabled: true }), 0);
    }
    r.set_dnd_available(true);
    r.set_cursor(Point::new(2600., -0.02));
    r.drain_outputs();
    r
}

#[test]
fn multidisplay_bottom_drag_recognition_precedes_damping_for_both_input_sources() {
    for backing_scale in [1, 2] {
        for remote in [false, true] {
            let mut r = multidisplay_bottom_setup(backing_scale);
            // Establish resistance before the press: it must not survive into
            // the independent native drag-recognition lifecycle.
            r.local_motion(0., 2., Some(Point::new(2600., -0.02)), 1);
            assert!(r.resistance.is_some());
            if remote {
                r.held.entry(SOURCE).or_default().buttons.insert(MouseButton::Left);
                r.button_refs.insert(MouseButton::Left, 1);
                let scale = r
                    .layout
                    .reference_per_native_unit(&r.pointer.as_ref().unwrap().screen)
                    .unwrap();
                r.remote_motion(SOURCE, 0., 10. * scale, 2);
            } else {
                r.local_button(MouseButton::Left, true);
                r.local_motion(0., 10., Some(Point::new(2600., -0.02)), 2);
            }
            assert!(r.resistance.is_none());
            assert_eq!(r.dnd_state_name(), "probing");
            assert!(
                r.out
                    .iter()
                    .any(|o| matches!(o, Output::Dnd(DndCmd::Probe { at }) if at.y < 0.))
            );
            r.on_dnd_event(DndEvent::Detected {
                id: oink_foundation::content::ClipId {
                    origin: A,
                    seq: crate::dnd::DND_SEQ_BIT | 1,
                },
                items: vec![oink_foundation::clipboard::ClipData::Text("drag".into())],
            });
            assert_eq!(r.dnd_state_name(), "leaving", "scale={backing_scale} remote={remote}");
        }
    }
}

#[test]
fn multidisplay_bottom_resistance_only_exempts_the_touching_native_segment() {
    for backing_scale in [1, 2] {
        for (x, seam) in [
            (1560., true),
            (1727.999, true),
            (1728., false),
            (1728.02, false),
            (2600., false),
        ] {
            let mut r = multidisplay_bottom_setup(backing_scale);
            let at = ScreenPos::new(ScreenId::new(A, "external"), Point::new(x, -0.02));
            let dst = ScreenPos::new(ScreenId::new(B, "screen"), Point::new(-1000., -1079.));
            assert_eq!(r.push_resistance(A, &at, Some(&at), &dst, (0., 1.), 1), seam, "x={x}");
            assert_eq!(r.resistance.is_none(), seam);
        }
    }
}

#[test]
fn multidisplay_hotplug_retires_damping_and_remaps_removed_negative_screen() {
    let mut r = multidisplay_bottom_setup(1);
    r.local_motion(0., 2., Some(Point::new(2600., -0.02)), 1);
    assert!(r.resistance.is_some());
    let remaining = r.displays.iter().filter(|d| d.is_primary).cloned().collect();
    r.set_local_displays(remaining);
    assert!(r.resistance.is_none());
    let p = r.pointer.as_ref().unwrap();
    assert_eq!(p.screen.display, "primary");
    assert!(r.displays[0].bounds.contains(p.local));
}

#[test]
fn multidisplay_small_native_gap_is_not_exempt_from_bottom_damping() {
    for gap in [0.25, 1., 1.5, 2.] {
        let mut r = multidisplay_bottom_setup(1);
        r.displays.iter_mut().find(|d| d.is_primary).unwrap().bounds.y = gap;
        let at = ScreenPos::new(ScreenId::new(A, "external"), Point::new(1600., -0.02));
        let dst = ScreenPos::new(ScreenId::new(B, "screen"), Point::new(-1000., -1079.));
        assert!(!r.push_resistance(A, &at, Some(&at), &dst, (0., 1.), 1), "gap={gap}");
        assert!(r.resistance.is_some());
    }
}

fn stillness_setup(corner: bool) -> Router {
    use oink_foundation::input::{EdgeResistanceRegion, ScreenCorner};
    let mut r = multidisplay_bottom_setup(1);
    if corner {
        r.set_edge_resistance_regions(vec![EdgeResistanceRegion {
            display: "external".into(),
            edges: vec![],
            corners: vec![ScreenCorner::BottomRight],
        }]);
        r.set_cursor(Point::new(3479.98, -0.02));
    }
    r
}

fn stillness_motion(r: &mut Router, remote: bool, delta: (f64, f64), now: Micros) {
    let from = r.pointer.as_ref().unwrap().clone();
    if remote {
        let k = r.layout.reference_per_native_unit(&from.screen).unwrap();
        r.remote_motion(SOURCE, delta.0 * k, delta.1 * k, now);
    } else {
        let bounds = r.displays.iter().find(|d| d.key == from.screen.display).unwrap().bounds;
        let pos = bounds.clamp(Point::new(from.local.x + delta.0, from.local.y + delta.1));
        r.local_motion(delta.0, delta.1, Some(pos), now);
    }
}

#[test]
fn edge_stillness_requires_100ms_for_local_remote_and_hot_corner_segments() {
    for remote in [false, true] {
        for corner in [false, true] {
            for wait in [99_000, 100_000, 150_000] {
                let mut r = stillness_setup(corner);
                // Arrival with no outward pressure must start an edge visit too.
                stillness_motion(&mut r, remote, (0., 0.), 0);
                for now in [10_000, 50_000, 90_000] {
                    stillness_motion(&mut r, remote, (0., 0.), now);
                }
                stillness_motion(&mut r, remote, (0., 2.), wait);
                assert_eq!(
                    r.holder == B,
                    wait >= 100_000,
                    "remote={remote},corner={corner},wait={wait}"
                );
                if wait >= 100_000 {
                    assert!(
                        r.resting_edges.iter().all(Option::is_none),
                        "handoff ends the edge visit"
                    );
                }
            }
        }
    }
}

#[test]
fn clamped_outward_motion_is_not_stillness_then_silence_releases_damping() {
    for remote in [false, true] {
        let mut r = stillness_setup(false);
        stillness_motion(&mut r, remote, (0., 2.), 0);
        let at = r.pointer.as_ref().unwrap().local;
        for now in (10_000..=500_000).step_by(10_000) {
            stillness_motion(&mut r, remote, (0., 0.1), now);
            assert_eq!(r.holder, A);
            assert_eq!(r.pointer.as_ref().unwrap().local, at, "native cursor remains clamped");
        }
        // No events at all between the last push and this one.
        stillness_motion(&mut r, remote, (0., 2.), 600_000);
        assert_eq!(r.holder, B);
    }
}

#[test]
fn raw_jitter_and_tangential_sliding_restart_the_edge_stillness_clock() {
    for remote in [false, true] {
        for delta in [(0., 0.000001), (1., 0.)] {
            let mut r = stillness_setup(false);
            stillness_motion(&mut r, remote, (0., 2.), 0);
            stillness_motion(&mut r, remote, delta, 90_000);
            stillness_motion(&mut r, remote, (0., 2.), 100_000);
            assert_eq!(r.holder, A, "remote={remote},delta={delta:?}");
            stillness_motion(&mut r, remote, (0., 2.), 200_000);
            assert_eq!(r.holder, B);
        }
    }
    // The separate raw-capture event must count even before a position sample.
    let mut r = stillness_setup(false);
    stillness_motion(&mut r, false, (0., 2.), 0);
    r.on_local_event(
        &CapturedEvent {
            event: InputEvent::RawPointerDelta { dx: 0., dy: 1. },
            source: SourceKind::Mouse,
            physical: true,
            time_us: 90_000,
            cursor: None,
        },
        90_000,
    );
    stillness_motion(&mut r, false, (0., 2.), 100_000);
    assert_eq!(r.holder, A);
}

#[test]
fn released_damping_lasts_only_for_the_same_boundary_visit() {
    for remote in [false, true] {
        for reset in 0..6 {
            let mut r = stillness_setup(false);
            stillness_motion(&mut r, remote, (0., 2.), 0);
            stillness_motion(&mut r, remote, (0., 0.), 100_000);
            match reset {
                0 => {
                    // Moving within the boundary preserves an acquired bypass.
                    stillness_motion(&mut r, remote, (1., 0.), 100_001);
                }
                1 => {
                    stillness_motion(&mut r, remote, (0., -3.), 100_001);
                    stillness_motion(&mut r, remote, (0., 3.), 100_002);
                }
                2 => r.set_local_displays(r.displays.clone()),
                3 => r.set_layout(r.layout.clone()),
                4 => {
                    r.set_locked(true);
                    r.set_locked(false);
                }
                _ => {
                    r.set_projection_gesture(true);
                    r.set_projection_gesture(false);
                }
            }
            stillness_motion(&mut r, remote, (0., 2.), 100_003);
            assert_eq!(r.holder == B, reset == 0, "remote={remote},reset={reset}");
        }
        let mut r = stillness_setup(true);
        stillness_motion(&mut r, remote, (0., 2.), 0);
        stillness_motion(&mut r, remote, (0., 0.), 100_000);
        stillness_motion(&mut r, remote, (-50., 0.), 100_001); // outside hot corner
        stillness_motion(&mut r, remote, (50., 0.), 100_002);
        stillness_motion(&mut r, remote, (0., 2.), 100_003);
        assert_eq!(r.holder, A, "leaving the protected corner resets its bypass");
    }
}

#[test]
fn stillness_release_applies_to_gap_travel_and_manual_windows_damping() {
    for remote in [false, true] {
        for gap in [false, true] {
            for os in [Os::MacOS, Os::Windows] {
                // Windows platform regions are empty; explicit manual rules
                // opt it into the same damping and stillness policy.
                let mut r = multidisplay_setup(1, gap);
                r.cfg.os = os;
                if os == Os::Windows {
                    r.cfg.settings.damping.rules = vec![crate::damping::DampingRule {
                        segment: crate::damping::DampingSegment {
                            source: Some(A),
                            target: Some(B),
                            display: Some("external".into()),
                            edge: oink_foundation::input::ScreenEdge::Bottom,
                            start: 0.,
                            end: 1.,
                        },
                        profile: crate::damping::DampingProfile::DepthOnly { depth: 0.5 },
                    }];
                    // Physical-gap system-edge damping is Mac-only by design.
                    if gap {
                        continue;
                    }
                }
                stillness_motion(&mut r, remote, (0., 2.), 0);
                assert_eq!(r.holder, A);
                stillness_motion(&mut r, remote, (0., 2.), 100_000);
                assert_eq!(r.holder, B, "remote={remote},gap={gap},os={os:?}");
            }
        }
    }
}
