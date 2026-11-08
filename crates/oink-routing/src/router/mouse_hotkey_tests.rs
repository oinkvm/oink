use super::*;
use crate::Packet;

fn router(remote: bool) -> Router {
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: DeviceId([1; 32]),
                nonce: [1; 16],
            },
            "source",
            Os::Other,
        ),
        Arc::new(crate::SimpleLayout::new()),
    );
    if remote {
        let peer = DeviceId([2; 32]);
        r.test_connect(peer, 0);
        r.holder = peer;
        r.claim = peer;
        r.fresh_join = false;
    }
    r.drain_outputs();
    r
}
fn event(r: &mut Router, event: InputEvent) -> Disposition {
    r.on_local_event(
        &CapturedEvent {
            event,
            physical: true,
            source: SourceKind::Mouse,
            time_us: 1,
            cursor: None,
        },
        1,
    )
}
fn button(r: &mut Router, button: MouseButton, pressed: bool) -> Disposition {
    event(r, InputEvent::Button { button, pressed })
}
#[test]
fn mouse_hotkeys_swallow_both_edges_across_modifiers_focus_and_binding_changes() {
    for remote in [false, true] {
        for (name, b) in [
            ("Mouse3", MouseButton::Middle),
            ("Mouse4", MouseButton::Back),
            ("Mouse5", MouseButton::Forward),
        ] {
            for ctrl in [false, true] {
                let mut r = router(remote);
                let names = if ctrl { vec!["Ctrl", name] } else { vec![name] };
                r.set_hotkeys(vec![HotkeyBinding {
                    chord: crate::Chord::parse(&names).unwrap(),
                    action: "open_window".into(),
                }]);
                if ctrl {
                    event(
                        &mut r,
                        InputEvent::Key {
                            code: KeyCode::LEFT_CTRL,
                            pressed: true,
                        },
                    );
                }
                r.drain_outputs();
                assert_eq!(button(&mut r, b, true), Disposition::Swallow);
                assert_eq!(
                    r.drain_outputs()
                        .iter()
                        .filter(
                            |o| matches!(o, Output::Status(StatusEvent::Hotkey { action }) if action == "open_window")
                        )
                        .count(),
                    1
                );
                // Release must follow the consumed DOWN, even after changing the binding,
                // releasing modifiers, pausing, or moving focus back home.
                r.set_hotkeys(vec![]);
                if ctrl {
                    event(
                        &mut r,
                        InputEvent::Key {
                            code: KeyCode::LEFT_CTRL,
                            pressed: false,
                        },
                    );
                }
                r.holder = r.me();
                r.fully_paused = true;
                r.drain_outputs();
                assert_eq!(button(&mut r, b, false), Disposition::Swallow);
                assert!(!r.drain_outputs().iter().any(|o| matches!(
                    o,
                    Output::Send {
                        msg: Message::Input(InputMsg::Button { .. }),
                        ..
                    } | Output::Inject(InjectEvent::Button { .. })
                )));
                r.fully_paused = false;
                assert_eq!(button(&mut r, b, true), Disposition::Pass);
                assert_eq!(button(&mut r, b, false), Disposition::Pass);
            }
        }
    }
}
#[test]
fn mouse_hotkeys_do_not_take_unmatched_or_already_delivered_releases() {
    let mut r = router(false);
    r.set_hotkeys(vec![HotkeyBinding {
        chord: crate::Chord::parse(&["Ctrl", "Mouse4"]).unwrap(),
        action: "open_window".into(),
    }]);
    assert_eq!(button(&mut r, MouseButton::Back, true), Disposition::Pass);
    event(
        &mut r,
        InputEvent::Key {
            code: KeyCode::LEFT_CTRL,
            pressed: true,
        },
    );
    assert_eq!(button(&mut r, MouseButton::Back, false), Disposition::Pass);
    for b in [MouseButton::Left, MouseButton::Right] {
        assert_eq!(button(&mut r, b, true), Disposition::Pass);
        assert_eq!(button(&mut r, b, false), Disposition::Pass);
    }
}

#[test]
fn mouse_pause_actions_wait_for_consumed_release_but_escape_remains_immediate() {
    for action in ["toggle_sharing", "pause_all"] {
        let mut r = router(false);
        r.set_hotkeys(vec![HotkeyBinding {
            chord: crate::Chord::parse(&["Mouse4"]).unwrap(),
            action: action.into(),
        }]);
        assert_eq!(button(&mut r, MouseButton::Back, true), Disposition::Swallow);
        assert!(!r.drain_outputs().iter().any(|o| matches!(
            o,
            Output::Status(StatusEvent::Hotkey { .. } | StatusEvent::EmergencyStop { .. })
        )));
        assert!(!r.fully_paused());
        assert_eq!(button(&mut r, MouseButton::Back, false), Disposition::Swallow);
        let outputs = r.drain_outputs();
        assert!(outputs.iter().any(|o| match o {
            Output::Status(StatusEvent::Hotkey { action }) => action == "toggle_sharing",
            Output::Status(StatusEvent::EmergencyStop { .. }) => true,
            _ => false,
        }));
    }
    let mut r = router(false);
    r.cfg.settings.emergency_combo = true;
    for code in [KeyCode::LEFT_CTRL, KeyCode::LEFT_ALT, KeyCode::LEFT_SHIFT] {
        event(&mut r, InputEvent::Key { code, pressed: true });
    }
    assert_eq!(
        event(
            &mut r,
            InputEvent::Key {
                code: KeyCode::ESCAPE,
                pressed: true
            }
        ),
        Disposition::Swallow
    );
    assert!(
        r.drain_outputs()
            .iter()
            .any(|o| matches!(o, Output::Status(StatusEvent::EmergencyStop { .. })))
    );
}

#[test]
fn native_recorder_takes_precedence_and_pairs_release_after_cancel_or_lease_expiry() {
    for remote in [false, true] {
        let mut r = router(remote);
        r.set_hotkeys(vec![HotkeyBinding {
            chord: crate::Chord::parse(&["Ctrl", "Mouse5"]).unwrap(),
            action: "open_window".into(),
        }]);
        event(
            &mut r,
            InputEvent::Key {
                code: KeyCode::LEFT_CTRL,
                pressed: true,
            },
        );
        r.record_mouse_shortcut("old".into(), true, 0);
        r.record_mouse_shortcut("current".into(), true, 0);
        r.record_mouse_shortcut("old".into(), false, 0);
        r.drain_outputs();
        assert_eq!(button(&mut r, MouseButton::Forward, true), Disposition::Swallow);
        let outputs = r.drain_outputs();
        assert!(outputs.iter().any(|o| matches!(o, Output::Status(StatusEvent::MouseShortcutRecorded {token,keys}) if token=="current" && keys==&["Ctrl", "Mouse5"])));
        assert!(!outputs.iter().any(|o| matches!(
            o,
            Output::Status(StatusEvent::Hotkey { .. })
                | Output::Send {
                    msg: Message::Input(InputMsg::Button { .. }),
                    ..
                }
                | Output::Inject(InjectEvent::Button { .. })
        )));
        r.record_mouse_shortcut("current".into(), false, 0);
        assert_eq!(button(&mut r, MouseButton::Forward, false), Disposition::Swallow);
        r.record_mouse_shortcut("expires".into(), true, 0);
        let ev = CapturedEvent {
            event: InputEvent::Button {
                button: MouseButton::Middle,
                pressed: true,
            },
            physical: true,
            source: SourceKind::Mouse,
            time_us: 60_000_001,
            cursor: None,
        };
        assert_eq!(r.on_local_event(&ev, 60_000_001), disposition_for(!remote));
        assert!(
            !r.drain_outputs()
                .iter()
                .any(|o| matches!(o, Output::Status(StatusEvent::MouseShortcutRecorded { .. })))
        );
    }
}

#[test]
fn mouse_shortcut_exact_modifiers_and_passed_release_keep_their_original_remote_target() {
    let mut r = router(true);
    r.set_hotkeys(vec![HotkeyBinding {
        chord: crate::Chord::parse(&["Ctrl", "Mouse4"]).unwrap(),
        action: "open_window".into(),
    }]);
    // With no Control this is normal remote input; a binding edit cannot consume its UP.
    assert_eq!(button(&mut r, MouseButton::Back, true), Disposition::Swallow);
    event(
        &mut r,
        InputEvent::Key {
            code: KeyCode::LEFT_CTRL,
            pressed: true,
        },
    );
    r.drain_outputs();
    assert_eq!(button(&mut r, MouseButton::Back, false), Disposition::Swallow);
    assert!(r.drain_outputs().iter().any(|o| matches!(
        o,
        Output::Send {
            msg: Message::Input(InputMsg::Button {
                button: MouseButton::Back,
                pressed: false
            }),
            ..
        }
    )));
    event(
        &mut r,
        InputEvent::Key {
            code: KeyCode::LEFT_SHIFT,
            pressed: true,
        },
    );
    r.drain_outputs();
    button(&mut r, MouseButton::Back, true);
    assert!(
        !r.drain_outputs()
            .iter()
            .any(|o| matches!(o, Output::Status(StatusEvent::Hotkey { .. })))
    );
}

#[test]
fn detached_capture_retires_consumed_mouse_generation_and_deferred_pause() {
    let mut r = router(false);
    r.set_hotkeys(vec![HotkeyBinding {
        chord: crate::Chord::parse(&["Mouse4"]).unwrap(),
        action: "toggle_sharing".into(),
    }]);
    assert_eq!(button(&mut r, MouseButton::Back, true), Disposition::Swallow);
    // Emergency/permission withdrawal cannot wait for the physical release.
    // A fresh capture generation must not inherit its pending pause or button.
    r.set_capture_available(false);
    r.set_capture_available(true);
    r.set_hotkeys(vec![HotkeyBinding {
        chord: crate::Chord::parse(&["Mouse4"]).unwrap(),
        action: "open_window".into(),
    }]);
    r.drain_outputs();
    assert_eq!(button(&mut r, MouseButton::Back, true), Disposition::Swallow);
    assert!(
        r.drain_outputs()
            .iter()
            .any(|o| matches!(o,Output::Status(StatusEvent::Hotkey {action}) if action=="open_window"))
    );
    assert_eq!(button(&mut r, MouseButton::Back, false), Disposition::Swallow);
    assert!(
        !r.drain_outputs()
            .iter()
            .any(|o| matches!(o,Output::Status(StatusEvent::Hotkey {action}) if action=="toggle_sharing"))
    );
}

#[test]
fn mouse_lock_escape_is_a_hold_and_consumes_both_edges() {
    let mut r = router(true);
    let peer = DeviceId([2; 32]);
    assert!(r.input_message(
        peer,
        Packet::Control(ControlMsg::FocusState {
            epoch: 1,
            holder: peer,
            lock_state: None,
            scope: Some(FocusScope {
                members: vec![(r.me(), r.routing_session()), (peer, [2; 16])],
            }),
            session: Some([2; 16]),
        }),
        1,
    ));
    r.set_hotkeys(vec![HotkeyBinding {
        chord: crate::Chord::parse(&["Mouse4"]).unwrap(),
        action: "lock_escape".into(),
    }]);
    r.drain_outputs();

    assert_eq!(button(&mut r, MouseButton::Back, true), Disposition::Swallow);
    assert!(r.drain_outputs().iter().any(|output| matches!(
        output,
        Output::Send {
            to,
            msg: Message::Control(ControlMsg::LockEscape { held: true }),
            ..
        } if *to == peer
    )));
    assert_eq!(button(&mut r, MouseButton::Back, false), Disposition::Swallow);
    assert!(r.drain_outputs().iter().any(|output| matches!(
        output,
        Output::Send {
            to,
            msg: Message::Control(ControlMsg::LockEscape { held: false }),
            ..
        } if *to == peer
    )));
}

#[test]
fn mouse_lock_escape_uses_mouse_source_when_keyboard_is_local() {
    let mut r = router(true);
    let peer = DeviceId([2; 32]);
    assert!(r.input_message(
        peer,
        Packet::Control(ControlMsg::FocusState {
            epoch: 1,
            holder: peer,
            lock_state: None,
            scope: Some(FocusScope {
                members: vec![(r.me(), r.routing_session()), (peer, [2; 16])],
            }),
            session: Some([2; 16]),
        }),
        1,
    ));
    let mut settings = r.cfg.settings.clone();
    settings.keyboard_mode = InputMode::Local;
    r.set_settings(settings);
    r.set_hotkeys(vec![HotkeyBinding {
        chord: crate::Chord::parse(&["Mouse4"]).unwrap(),
        action: "lock_escape".into(),
    }]);
    r.drain_outputs();

    assert_eq!(button(&mut r, MouseButton::Back, true), Disposition::Swallow);
    assert!(r.drain_outputs().iter().any(|output| matches!(
        output,
        Output::Send {
            to,
            msg: Message::Control(ControlMsg::LockEscape { held: true }),
            ..
        } if *to == peer
    )));
    assert_eq!(button(&mut r, MouseButton::Back, false), Disposition::Swallow);
    assert!(r.drain_outputs().iter().any(|output| matches!(
        output,
        Output::Send {
            to,
            msg: Message::Control(ControlMsg::LockEscape { held: false }),
            ..
        } if *to == peer
    )));
}
