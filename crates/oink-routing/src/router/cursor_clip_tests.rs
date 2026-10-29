//! Pure routing cases; no native cursor movement or ClipCursor calls.
use super::*;
const A: DeviceId = DeviceId([1; 32]);
const B: DeviceId = DeviceId([2; 32]);
fn display(key: &str, bounds: Rect) -> DisplayInfo {
    DisplayInfo {
        key: key.into(),
        name: key.into(),
        bounds,
        pixel_width: bounds.w as u32,
        pixel_height: bounds.h as u32,
        reference_extent: None,
        rotation_deg: 0,
        is_primary: key == "main",
        is_builtin: false,
        is_virtual: false,
    }
}
fn target(multiple: bool) -> Router {
    let mut layout = crate::SimpleLayout::new();
    let mut displays = vec![display("main", Rect::new(0., 0., 1000., 800.))];
    if multiple {
        displays.push(display("left", Rect::new(-1000., 0., 1000., 800.)));
    }
    for d in &displays {
        layout.add_screen(ScreenId::new(B, &d.key), d.bounds, d.bounds);
    }
    layout.add_screen(
        ScreenId::new(A, "a"),
        Rect::new(0., 0., 1000., 800.),
        Rect::new(if multiple { -2000. } else { -1000. }, 0., 1000., 800.),
    );
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: B,
                nonce: [2; 16],
            },
            "target",
            Os::Windows,
        ),
        Arc::new(layout),
    );
    r.set_local_displays(displays);
    r.test_connect(A, 0);
    r.peers.get_mut(&A).unwrap().focus_ready = true;
    r.set_game_mode(GameMode::Off);
    r.set_cursor(Point::new(500., 400.));
    r.drain_outputs();
    r
}
fn clip() -> Rect {
    Rect::new(200., 100., 400., 300.)
}
#[test]
fn clip_clamps_virtual_pointer_and_absolute_injection() {
    let mut r = target(false);
    r.set_system_cursor_clip(Some(clip()));
    assert_eq!(r.local_pointer(), Some(Point::new(500., 399.)));
    r.remote_motion(A, 10000., -10000., 1);
    assert_eq!(r.local_pointer(), Some(Point::new(599., 100.)));
    assert!(r.drain_outputs().iter().any(|o| matches!(o,
        Output::Inject(InjectEvent::MoveToWithDelta { pos, .. }) if *pos == Point::new(599., 100.))));
}
#[test]
fn clip_suppresses_crossing_then_release_restores_edges() {
    let mut r = target(false);
    r.set_system_cursor_clip(Some(clip()));
    r.remote_motion(A, -10000., 0., 1);
    assert_eq!(r.holder, B);
    assert_eq!(r.local_pointer(), Some(Point::new(200., 399.)));
    assert_eq!(r.system_cursor_clip_status().1, 1);
    r.set_system_cursor_clip(None);
    r.remote_motion(A, -10000., 0., 2);
    assert_eq!(r.holder, A);
}
#[test]
fn clip_spans_system_monitors_and_uses_negative_pixels() {
    let mut r = target(true);
    r.set_cursor(Point::new(100., 200.));
    r.set_system_cursor_clip(Some(Rect::new(-200., 100., 600., 300.)));
    r.remote_motion(A, -250., 0., 1);
    assert_eq!(r.local_pointer(), Some(Point::new(-150., 200.)));
    assert_eq!(r.pointer.as_ref().unwrap().screen.display, "left");
    r.remote_motion(A, -10000., 0., 2);
    assert_eq!(r.local_pointer(), Some(Point::new(-200., 200.)));
    assert_eq!(r.holder, B);
}
#[test]
fn clip_game_mode_preserves_relative_stream() {
    let mut r = target(false);
    r.set_game_mode(GameMode::On);
    r.drain_outputs();
    r.set_system_cursor_clip(Some(clip()));
    assert_eq!(r.local_pointer(), Some(Point::new(500., 399.)));
    let Message::Control(ControlMsg::GameMode { epoch, generation, .. }) = r.input_game_state() else {
        panic!()
    };
    r.receive_relative(A, epoch, generation, 1, 3., -7.);
    let outputs = r.drain_outputs();
    assert!(
        outputs
            .iter()
            .any(|o| matches!(o, Output::Inject(InjectEvent::MoveRelative { dx: 3., dy: -7. })))
    );
    assert!(!outputs.iter().any(|o| matches!(
        o,
        Output::Inject(InjectEvent::MoveTo { .. } | InjectEvent::MoveToWithDelta { .. })
    )));
}
#[test]
fn known_clip_constrains_mac_but_does_not_pin_a_remote_holder() {
    let mut mac = target(false);
    mac.cfg.os = Os::MacOS;
    mac.set_system_cursor_clip(Some(clip()));
    assert_eq!(mac.system_cursor_clip_status().0, Some(clip()));
    mac.remote_motion(A, -10000., 0., 1);
    assert_eq!(mac.holder, B);
    assert_eq!(mac.local_pointer(), Some(Point::new(200., 399.)));
    mac.set_system_cursor_clip(None);
    mac.remote_motion(A, -10000., 0., 2);
    assert_eq!(mac.holder, A);
    let mut source = target(false);
    source.become_source(1, A);
    source.set_system_cursor_clip(Some(clip()));
    assert!(!source.leave_blocked());
}
fn key(r: &mut Router, code: KeyCode, now: u64) {
    r.on_local_event(
        &CapturedEvent {
            time_us: now,
            physical: true,
            source: SourceKind::Keyboard,
            cursor: None,
            event: InputEvent::Key { code, pressed: true },
        },
        now,
    );
}
#[test]
fn clip_keeps_emergency_and_recall_hotkeys_available() {
    let mut r = target(false);
    r.set_system_cursor_clip(Some(clip()));
    for (i, code) in [
        KeyCode::LEFT_CTRL,
        KeyCode::LEFT_ALT,
        KeyCode::LEFT_SHIFT,
        KeyCode::ESCAPE,
    ]
    .into_iter()
    .enumerate()
    {
        key(&mut r, code, i as u64 + 1);
    }
    assert!(r.paused());
    let mut source = target(false);
    source.become_source(1, A);
    source.set_system_cursor_clip(Some(clip()));
    source.set_hotkeys(vec![HotkeyBinding {
        chord: crate::Chord::parse(&["Ctrl", "Alt", "H"]).unwrap(),
        action: "return_pointer_home".into(),
    }]);
    for (i, code) in [KeyCode::LEFT_CTRL, KeyCode::LEFT_ALT, KeyCode::keyboard(0x0b)]
        .into_iter()
        .enumerate()
    {
        key(&mut source, code, i as u64 + 1);
    }
    assert_eq!(source.holder, B);
}

#[test]
fn clip_target_accepts_source_recall_and_long_escape() {
    let mut b = target(false);
    let mut a = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: A,
                nonce: [1; 16],
            },
            "source",
            Os::Windows,
        ),
        b.layout.clone(),
    );
    a.set_local_displays(vec![display("a", Rect::new(0., 0., 1000., 800.))]);
    b.test_disconnect(A, 0);
    a.attach_input(
        oink_foundation::session::Boot {
            device_id: B,
            nonce: b.session,
        },
        b.session,
        true,
        true,
        FeelProfile::default(),
        0,
    );
    b.attach_input(
        oink_foundation::session::Boot {
            device_id: A,
            nonce: a.session,
        },
        a.session,
        true,
        true,
        FeelProfile::default(),
        0,
    );
    a.peers.get_mut(&B).unwrap().focus_ready = true;
    b.peers.get_mut(&A).unwrap().focus_ready = true;
    b.reclaim_focus(None);
    a.become_source(b.epoch, B);
    b.set_system_cursor_clip(Some(clip()));
    b.drain_outputs();
    a.drain_outputs();
    a.set_hotkeys(vec![HotkeyBinding {
        chord: crate::Chord::parse(&["Ctrl", "Alt", "H"]).unwrap(),
        action: "return_pointer_home".into(),
    }]);
    for (i, code) in [KeyCode::LEFT_CTRL, KeyCode::LEFT_ALT, KeyCode::keyboard(0x0b)]
        .into_iter()
        .enumerate()
    {
        key(&mut a, code, i as u64 + 1);
    }
    for output in a.drain_outputs() {
        if let Output::Send { to: B, msg, .. } = output {
            b.input_message(A, msg, 10);
        }
    }
    assert_eq!(a.holder, A);
    assert_eq!(b.holder, A, "application clip must not override explicit recall");
    let mut b = target(false);
    b.cfg.settings.emergency_hold_ms = 500;
    b.set_system_cursor_clip(Some(clip()));
    key(&mut b, KeyCode::ESCAPE, 1);
    b.tick(500_001);
    assert!(b.paused());
}

#[test]
fn clip_intersection_avoids_holes_in_a_multimonitor_desktop() {
    let mut r = target(true);
    r.displays[1].bounds = Rect::new(-1000., 400., 800., 800.);
    r.set_system_cursor_clip(Some(Rect::new(-300., 200., 600., 400.)));
    r.set_cursor(Point::new(-100., 300.));
    // (-100,300) is in the clip but in neither physical display.
    let at = r.local_pointer().unwrap();
    assert!(r.displays.iter().any(|d| d.bounds.contains(at)));
    assert!(Rect::new(-300., 200., 600., 400.).contains(at));
}

#[test]
fn clip_local_edge_waits_for_fresh_snapshot_outside_callback() {
    let mut r = target(false);
    r.enable_system_clip_recheck(true);
    r.set_cursor(Point::new(0., 400.));
    r.local_motion(-3., 0., Some(Point::new(0., 400.)), 1);
    assert_eq!(r.holder, B, "native callback cannot decide against a stale snapshot");
    assert!(r.system_clip_recheck_pending());
    r.set_system_cursor_clip(Some(Rect::new(0., 0., 500., 800.)));
    r.resume_system_clip_motion();
    assert_eq!(r.holder, B);
    r.local_motion(-3., 0., Some(Point::new(0., 400.)), 2);
    assert!(r.system_clip_recheck_pending());
    r.set_system_cursor_clip(None);
    r.resume_system_clip_motion();
    assert_eq!(r.holder, A, "release is checked before resolving this very edge");
}
#[test]
fn clip_deferred_edge_cannot_survive_emergency() {
    let mut r = target(false);
    r.enable_system_clip_recheck(true);
    r.set_cursor(Point::new(0., 400.));
    r.local_motion(-3., 0., Some(Point::new(0., 400.)), 1);
    assert!(r.system_clip_recheck_pending());
    r.trigger_emergency();
    assert!(
        !r.system_clip_recheck_pending(),
        "pause must disarm the immediate native-loop wake"
    );
    r.set_system_cursor_clip(None);
    r.resume_system_clip_motion();
    assert_eq!(r.holder, B);
    assert!(r.paused());
    assert!(!r.system_clip_recheck_pending());
}

fn escape_binding(r: &mut Router) {
    r.set_hotkeys(vec![HotkeyBinding {
        chord: crate::Chord {
            mods: Mods::NONE,
            key: KeyCode::LEFT_ALT,
        },
        action: "lock_escape".into(),
    }]);
}

#[test]
fn passive_escape_crosses_observed_clip_edge_only_while_held() {
    for remote in [false, true] {
        for held in [false, true] {
            let mut r = target(false);
            escape_binding(&mut r);
            r.set_system_cursor_clip(Some(Rect::new(1., 1., 998., 798.)));
            r.set_cursor(Point::new(1., 400.));
            if held {
                if remote {
                    r.input_message(A, Message::Control(ControlMsg::LockEscape { held: true }), 1);
                } else {
                    key(&mut r, KeyCode::LEFT_ALT, 1);
                }
            }
            if remote {
                r.remote_motion(A, -0.1, 0., 2);
            } else {
                r.local_motion(-0.1, 0., Some(Point::new(1., 400.)), 2);
            }
            assert_eq!(r.holder, if held { A } else { B }, "remote={remote}, held={held}");
        }
    }
}

#[test]
fn passive_escape_does_not_cross_from_interior_or_release_or_ordinary_clip() {
    for (rect, position, release) in [
        (Rect::new(1., 1., 998., 798.), Point::new(100., 400.), false),
        (Rect::new(1., 1., 998., 798.), Point::new(1., 400.), true),
        (Rect::new(3., 3., 994., 794.), Point::new(3., 400.), false),
    ] {
        let mut r = target(false);
        r.set_system_cursor_clip(Some(rect));
        r.set_cursor(position);
        r.input_message(A, Message::Control(ControlMsg::LockEscape { held: true }), 1);
        if release {
            r.input_message(A, Message::Control(ControlMsg::LockEscape { held: false }), 2);
        }
        r.remote_motion(A, -0.1, 0., 3);
        assert_eq!(r.holder, B);
    }
}

#[test]
fn passive_game_mode_without_fresh_readback_keeps_relative_stream_and_focus() {
    let mut r = target(false);
    r.set_system_cursor_clip(Some(Rect::new(1., 1., 998., 798.)));
    r.set_cursor(Point::new(1., 400.));
    r.set_game_mode(GameMode::On);
    r.input_message(A, Message::Control(ControlMsg::LockEscape { held: true }), 1);
    r.drain_outputs();
    let Message::Control(ControlMsg::GameMode { epoch, generation, .. }) = r.input_game_state() else {
        panic!()
    };
    r.receive_relative(A, epoch, generation, 1, -10000., 0.);
    assert_eq!(r.holder, B);
    assert!(
        r.drain_outputs()
            .iter()
            .any(|o| matches!(o, Output::Inject(InjectEvent::MoveRelative { dx: -10000., dy: 0. })))
    );
}

#[test]
fn passive_classification_physical_monitor_boundaries() {
    use super::cursor_clip::passive_clip;
    let bounds = Rect::new(-1000., -800., 1000., 800.);
    for left in [0., 1., 2., 3.] {
        for top in [0., 1., 2., 3.] {
            for right in [0., 1., 2., 3.] {
                for bottom in [0., 1., 2., 3.] {
                    let gaps = [left, top, right, bottom];
                    let clip = Rect::new(
                        bounds.x + left,
                        bounds.y + top,
                        bounds.w - left - right,
                        bounds.h - top - bottom,
                    );
                    assert_eq!(
                        passive_clip(clip, bounds),
                        gaps.iter().all(|g| *g <= 2.) && gaps.iter().any(|g| *g > 0.),
                        "{gaps:?}"
                    );
                }
            }
        }
    }
    let spanning = Rect::new(-999., -799., 1998., 798.);
    assert!(!passive_clip(spanning, bounds));
    assert!(!passive_clip(spanning, Rect::new(0., -800., 1000., 800.)));
    assert!(!passive_clip(Rect::new(-1001., -799., 1000., 798.), bounds));
}

#[test]
fn passive_entry_damping_is_directional_and_clears_on_owner_state_change() {
    use crate::control::EntryProtection;
    for state in [
        EntryProtection {
            locked: true,
            ..Default::default()
        },
        EntryProtection {
            game: true,
            ..Default::default()
        },
        EntryProtection {
            passive_displays: vec!["a".into()],
            ..Default::default()
        },
    ] {
        let mut r = target(false);
        r.cfg.settings.damping.platform_profile = crate::damping::DampingProfile::DepthOnly { depth: 0.05 };
        r.input_message(A, Message::Control(ControlMsg::EntryProtection { state }), 1);
        r.set_cursor(Point::new(0., 400.));
        r.local_motion(-0.1, 0., Some(Point::new(0., 400.)), 2);
        assert_eq!(r.holder, B, "protected destination must damp entry on Windows too");
        assert!(r.resistance.is_some());
        r.input_message(
            A,
            Message::Control(ControlMsg::EntryProtection {
                state: EntryProtection::default(),
            }),
            3,
        );
        assert!(
            r.resistance.is_none(),
            "owner state change retires accumulated pressure"
        );
        r.local_motion(-0.1, 0., Some(Point::new(0., 400.)), 4);
        assert_eq!(r.holder, A);
    }
    // A protection fact for a different destination monitor cannot damp this portal.
    let mut r = target(false);
    r.input_message(
        A,
        Message::Control(ControlMsg::EntryProtection {
            state: EntryProtection {
                passive_displays: vec!["other".into()],
                ..Default::default()
            },
        }),
        1,
    );
    r.set_cursor(Point::new(0., 400.));
    r.local_motion(-0.1, 0., Some(Point::new(0., 400.)), 2);
    assert_eq!(r.holder, A);
}

#[test]
fn passive_all_zero_monitor_clip_does_not_block_external_edge() {
    let mut r = target(false);
    r.set_system_cursor_clip(Some(Rect::new(0., 0., 1000., 800.)));
    r.set_cursor(Point::new(0., 400.));
    r.local_motion(-1., 0., Some(Point::new(0., 400.)), 1);
    assert_eq!(r.holder, A);
}

#[test]
fn passive_entry_damping_accumulates_but_disconnect_retires_it() {
    use crate::control::EntryProtection;
    let mut r = target(false);
    r.cfg.settings.damping.platform_profile = crate::damping::DampingProfile::DepthOnly { depth: 0.05 };
    r.input_message(
        A,
        Message::Control(ControlMsg::EntryProtection {
            state: EntryProtection {
                locked: true,
                ..Default::default()
            },
        }),
        1,
    );
    r.set_cursor(Point::new(0., 400.));
    r.local_motion(-1., 0., Some(Point::new(0., 400.)), 2);
    assert_eq!(r.holder, B);
    r.local_motion(-100., 0., Some(Point::new(0., 400.)), 3);
    assert_eq!(r.holder, A, "existing Dock depth threshold must be usable");

    let mut r = target(false);
    r.input_message(
        A,
        Message::Control(ControlMsg::EntryProtection {
            state: EntryProtection {
                game: true,
                ..Default::default()
            },
        }),
        1,
    );
    r.set_cursor(Point::new(0., 400.));
    r.local_motion(-0.1, 0., Some(Point::new(0., 400.)), 2);
    assert!(r.resistance.is_some());
    r.test_disconnect(A, 3);
    r.test_connect(A, 4);
    r.peers.get_mut(&A).unwrap().focus_ready = true;
    r.set_cursor(Point::new(0., 400.));
    r.local_motion(-0.1, 0., Some(Point::new(0., 400.)), 5);
    assert_eq!(r.holder, A, "no protection state from retired stream");
}

#[test]
fn passive_game_escape_uses_fresh_edge_readback_without_extra_pressure() {
    for clip in [None, Some(Rect::new(1., 1., 998., 798.))] {
        for held in [false, true] {
            for at_edge in [false, true] {
                let mut r = target(false);
                r.set_game_mode(GameMode::On);
                r.set_system_cursor_clip(clip);
                r.input_message(A, Message::Control(ControlMsg::LockEscape { held }), 1);
                // Readback must be associated with the moving physical source.
                r.last_pointer_source = Some(A);
                let x = if at_edge {
                    if clip.is_some() { 1. } else { 0. }
                } else {
                    500.
                };
                r.reconcile_target_snapshot(Some(Point::new(x, 400.)), Some(clip));
                let Message::Control(ControlMsg::GameMode { epoch, generation, .. }) = r.input_game_state() else {
                    panic!()
                };
                r.receive_relative(A, epoch, generation, 1, -0.1, 0.);
                assert_eq!(
                    r.holder,
                    if held && at_edge { A } else { B },
                    "clip={clip:?}, held={held}, at_edge={at_edge}"
                );
            }
        }
    }
}

#[test]
fn passive_game_escape_cannot_use_release_or_old_readback_or_ordinary_confinement() {
    for reason in ["released", "missing", "ordinary", "recentered"] {
        let mut r = target(false);
        r.set_game_mode(GameMode::On);
        r.input_message(A, Message::Control(ControlMsg::LockEscape { held: true }), 1);
        r.last_pointer_source = Some(A);
        let clip = (reason == "ordinary").then_some(Rect::new(3., 3., 994., 794.));
        r.set_system_cursor_clip(clip);
        r.set_cursor(Point::new(0., 400.)); // stale cached edge is not native evidence
        if reason != "missing" {
            r.reconcile_target_snapshot(
                Some(Point::new(if reason == "ordinary" { 3. } else { 0. }, 400.)),
                Some(clip),
            );
        }
        if reason == "released" {
            r.input_message(A, Message::Control(ControlMsg::LockEscape { held: false }), 2);
        }
        if reason == "recentered" {
            r.reconcile_target_snapshot(Some(Point::new(500., 400.)), Some(clip));
        }
        let Message::Control(ControlMsg::GameMode { epoch, generation, .. }) = r.input_game_state() else {
            panic!()
        };
        r.receive_relative(A, epoch, generation, 1, -1000., 0.);
        assert_eq!(r.holder, B, "{reason}");
    }
}

#[test]
fn passive_escape_bypasses_all_entry_and_manual_damping() {
    use crate::{
        control::EntryProtection,
        damping::{DampingConfig, DampingProfile, DampingRule, DampingSegment},
    };
    for held in [false, true] {
        let mut r = target(false);
        escape_binding(&mut r);
        r.cfg.settings.damping = DampingConfig {
            platform_profile: DampingProfile::DepthOnly { depth: 1000. },
            rules: vec![DampingRule {
                segment: DampingSegment {
                    source: None,
                    target: None,
                    display: None,
                    edge: oink_foundation::input::ScreenEdge::Left,
                    start: 0.,
                    end: 1.,
                },
                profile: DampingProfile::DepthOnly { depth: 1000. },
            }],
        };
        r.input_message(
            A,
            Message::Control(ControlMsg::EntryProtection {
                state: EntryProtection {
                    locked: true,
                    game: true,
                    passive_displays: vec!["a".into()],
                },
            }),
            1,
        );
        r.set_cursor(Point::new(0., 400.));
        if held {
            key(&mut r, KeyCode::LEFT_ALT, 2);
        }
        r.local_motion(-0.1, 0., Some(Point::new(0., 400.)), 3);
        assert_eq!(r.holder, if held { A } else { B });
        if held {
            assert!(r.resistance.is_none());
        }
    }
}

fn protected_trio() -> Vec<Router> {
    const C: DeviceId = DeviceId([3; 32]);
    let mut layout = crate::SimpleLayout::new();
    let bounds = Rect::new(0., 0., 1000., 800.);
    for (i, (id, name)) in [(A, "a"), (B, "b"), (C, "c")].into_iter().enumerate() {
        layout.add_screen(
            ScreenId::new(id, name),
            bounds,
            Rect::new(i as f64 * 1000., 0., 1000., 800.),
        );
    }
    let layout = Arc::new(layout);
    [(A, "a"), (B, "b"), (C, "c")]
        .into_iter()
        .map(|(id, name)| {
            let mut r = Router::new(
                RouterConfig::new(
                    oink_foundation::session::Boot {
                        device_id: id,
                        nonce: [id.0[0]; 16],
                    },
                    name,
                    Os::Windows,
                ),
                layout.clone(),
            );
            r.set_local_displays(vec![display(name, bounds)]);
            r.set_cursor(Point::new(500., 400.));
            r.set_game_mode(GameMode::Off);
            for other in [A, B, C] {
                if other != id {
                    r.attach_input(
                        oink_foundation::session::Boot {
                            device_id: other,
                            nonce: [other.0[0]; 16],
                        },
                        [other.0[0]; 16],
                        true,
                        true,
                        FeelProfile::default(),
                        0,
                    );
                    r.peers.get_mut(&other).unwrap().focus_ready = true;
                }
            }
            r
        })
        .collect()
}
fn settle_trio(routers: &mut [Router]) {
    for now in 1..30 {
        let mut messages = Vec::new();
        for r in routers.iter_mut() {
            let from = r.me();
            for output in r.drain_outputs() {
                if let Output::Send { to, msg, .. } = output {
                    messages.push((from, to, msg));
                }
            }
        }
        if messages.is_empty() {
            return;
        }
        for (from, to, msg) in messages {
            routers
                .iter_mut()
                .find(|r| r.me() == to)
                .unwrap()
                .input_message(from, msg, now);
        }
    }
    panic!("owner messages must converge");
}

#[test]
fn passive_three_owners_keep_game_lock_and_clip_across_pointer_hotkey_visits() {
    const C: DeviceId = DeviceId([3; 32]);
    let mut routers = protected_trio();
    routers[0].reclaim_focus(None);
    settle_trio(&mut routers);
    routers[0].set_game_mode(GameMode::On);
    settle_trio(&mut routers);
    // The device-card menu and settings both edit the owner preference even
    // while another device holds focus. No group-wide game-mode exclusivity.
    let mut settings = routers[1].config().settings.clone();
    settings.game_mode_preference = GameMode::On;
    routers[1].set_settings(settings.clone());
    settle_trio(&mut routers);
    assert!(routers[0].owner_game_active() && routers[1].owner_game_active());
    settings.game_mode_preference = GameMode::Off;
    routers[1].set_settings(settings);
    routers[2].set_system_cursor_clip(Some(Rect::new(1., 1., 998., 798.)));
    routers[0].lock_focus_to(B).unwrap();
    settle_trio(&mut routers);
    assert!(routers[1].owner_pointer_locked);
    for destination in [C, A, B, C, B, A] {
        let holder = routers[0].holder;
        // This is the router command used by per-device pointer shortcuts.
        routers
            .iter_mut()
            .find(|r| r.me() == holder)
            .unwrap()
            .move_pointer_to(destination)
            .unwrap();
        settle_trio(&mut routers);
        assert!(routers[0].owner_game_active());
        assert!(routers[1].owner_pointer_locked);
        assert_eq!(routers[2].local_entry_protection().passive_displays, ["c"]);
        for r in &routers {
            assert_eq!(r.holder, destination);
            assert_eq!(r.game_mode().active, destination == A, "game badge follows holder");
            assert_eq!(
                r.locked_device(),
                (destination == B).then_some(B),
                "lock badge follows holder"
            );
            for (id, name) in [(A, "a"), (B, "b"), (C, "c")] {
                assert!(r.protected_entry(&ScreenId::new(id, name)));
            }
        }
    }
}

#[test]
fn passive_escape_all_four_physical_edges_and_local_game_mode() {
    let bounds = Rect::new(0., 0., 1000., 800.);
    for (neighbor, point, delta) in [
        (Rect::new(-1000., 0., 1000., 800.), Point::new(1., 400.), (-0.1, 0.)),
        (Rect::new(1000., 0., 1000., 800.), Point::new(998., 400.), (0.1, 0.)),
        (Rect::new(0., -800., 1000., 800.), Point::new(500., 1.), (0., -0.1)),
        (Rect::new(0., 800., 1000., 800.), Point::new(500., 798.), (0., 0.1)),
    ] {
        for game in [false, true] {
            for key_code in [KeyCode::LEFT_ALT, KeyCode::RIGHT_ALT] {
                let mut r = target(false);
                let mut layout = crate::SimpleLayout::new();
                layout.add_screen(ScreenId::new(B, "main"), bounds, bounds);
                layout.add_screen(ScreenId::new(A, "a"), bounds, neighbor);
                r.set_layout(Arc::new(layout));
                escape_binding(&mut r);
                r.set_system_cursor_clip(Some(Rect::new(1., 1., 998., 798.)));
                r.set_cursor(point);
                if game {
                    r.set_game_mode(GameMode::On);
                }
                key(&mut r, key_code, 1);
                r.local_motion(delta.0, delta.1, Some(point), 2);
                assert_eq!(r.holder, A, "point={point:?}, game={game}, key={key_code:?}");
            }
        }
    }
}

#[test]
fn held_escape_from_either_keyboard_skips_damping_for_both_mice() {
    use oink_foundation::input::{EdgeResistanceRegion, ScreenEdge};
    for remote in [false, true] {
        let mut r = target(false);
        r.cfg.os = Os::MacOS;
        escape_binding(&mut r);
        r.cfg.settings.damping.platform_profile = crate::damping::DampingProfile::DepthOnly { depth: 1000. };
        r.set_edge_resistance_regions(vec![EdgeResistanceRegion {
            display: "main".into(),
            edges: vec![ScreenEdge::Left],
            corners: vec![],
        }]);
        r.set_cursor(Point::new(0., 400.));
        let at = r.pointer.clone().unwrap();
        let dst = ScreenPos::new(ScreenId::new(A, "a"), Point::new(999., 400.));
        let origin = if remote { A } else { B };
        assert!(!r.push_resistance(origin, &at, None, &dst, (-1., 0.), 1));
        assert!(r.resistance.is_some());
        if remote {
            r.input_message(A, Message::Control(ControlMsg::LockEscape { held: true }), 2);
        } else {
            key(&mut r, KeyCode::RIGHT_ALT, 2);
        }
        assert!(r.resistance.is_none());
        assert!(r.push_resistance(origin, &at, None, &dst, (-0.1, 0.), 3));
        assert!(r.push_gap_resistance(origin, &at, &at, (-0.1, 0.), 3));
        assert!(r.resistance.is_none());
        let other = if remote { B } else { A };
        assert!(r.push_resistance(other, &at, None, &dst, (-0.1, 0.), 4));
        assert!(r.push_gap_resistance(other, &at, &at, (-0.1, 0.), 4));
        assert!(
            r.resistance.is_none(),
            "the shared keyboard gate applies to either mouse"
        );
    }
}

#[test]
fn passive_game_sample_is_consumed_once_even_for_rejected_relative_packet() {
    let mut r = target(false);
    r.set_game_mode(GameMode::On);
    r.input_message(A, Message::Control(ControlMsg::LockEscape { held: true }), 1);
    r.reconcile_game_escape_snapshot(A, Some(Point::new(0., 400.)), Some(None));
    let Message::Control(ControlMsg::GameMode { epoch, generation, .. }) = r.input_game_state() else {
        panic!()
    };
    r.receive_relative(A, epoch, generation + 1, 1, -1., 0.);
    r.receive_relative(A, epoch, generation, 1, -1., 0.);
    assert_eq!(r.holder, B, "old edge sample cannot survive a rejected packet");
    r.reconcile_game_escape_snapshot(A, Some(Point::new(0., 400.)), Some(None));
    r.receive_relative(A, epoch, generation, 2, -1.1, 0.);
    assert_eq!(r.holder, A);
}

#[test]
fn remote_keyboard_escape_retires_local_mouse_pressure() {
    let mut r = target(false);
    escape_binding(&mut r);
    r.input_message(
        A,
        Message::Control(ControlMsg::EntryProtection {
            state: crate::control::EntryProtection {
                locked: true,
                ..Default::default()
            },
        }),
        1,
    );
    r.set_cursor(Point::new(0., 400.));
    r.local_motion(-0.1, 0., Some(Point::new(0., 400.)), 2);
    assert!(r.resistance.is_some());
    r.input_message(A, Message::Control(ControlMsg::LockEscape { held: true }), 3);
    assert!(r.resistance.is_none(), "remote keyboard clears local mouse damping");
    key(&mut r, KeyCode::LEFT_ALT, 4);
    assert!(r.resistance.is_none());
}

#[test]
fn remote_keyboard_release_invalidates_local_mouse_game_readback() {
    let mut r = target(false);
    r.set_game_mode(GameMode::On);
    assert!(!r.game_escape_readback(B));
    r.input_message(A, Message::Control(ControlMsg::LockEscape { held: true }), 1);
    assert!(r.game_escape_readback(B));
    r.reconcile_game_escape_snapshot(B, Some(Point::new(0., 400.)), Some(None));
    assert!(r.game.escape_cursor.is_some());
    r.input_message(A, Message::Control(ControlMsg::LockEscape { held: false }), 2);
    assert!(!r.game_escape_readback(B));
    assert!(r.game.escape_cursor.is_none());
}
