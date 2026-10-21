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
