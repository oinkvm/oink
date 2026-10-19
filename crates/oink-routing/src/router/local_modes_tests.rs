//! Source policy tests. Native cursor behaviour is verified separately on desktops.
use super::*;
use oink_foundation::input::InputMode;
const ME: DeviceId = DeviceId([1; 32]);
const PEER: DeviceId = DeviceId([2; 32]);
const A: KeyCode = KeyCode::keyboard(4);
fn router(remote: bool) -> Router {
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: ME,
                nonce: [ME.0[0]; 16],
            },
            "source",
            Os::Windows,
        ),
        connection_tests::session_layout(ME, PEER),
    );
    r.set_local_displays(vec![DisplayInfo {
        key: "screen".into(),
        name: "screen".into(),
        bounds: Rect::new(0., 0., 1000., 600.),
        pixel_width: 1000,
        pixel_height: 600,
        reference_extent: None,
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }]);
    r.set_cursor(Point::new(123., 234.));
    r.test_connect(PEER, 0);
    if remote {
        r.claim = PEER;
        r.become_source(1, PEER);
    }
    r.out.clear();
    r
}
fn event(r: &mut Router, e: InputEvent) -> Disposition {
    r.on_local_event(
        &CapturedEvent {
            event: e,
            source: SourceKind::Mouse,
            physical: true,
            time_us: 100,
            cursor: Some(Point::new(999., 234.)),
        },
        100,
    )
}
fn modes(r: &mut Router, mouse: InputMode, keyboard: InputMode) {
    let mut settings = r.config().settings.clone();
    settings.mouse_mode = mouse;
    settings.keyboard_mode = keyboard;
    r.set_settings(settings);
}
fn inputs(r: &Router) -> Vec<&InputMsg> {
    r.out
        .iter()
        .filter_map(|o| {
            if let Output::Send {
                msg: Message::Input(i), ..
            } = o
            {
                Some(i)
            } else {
                None
            }
        })
        .collect()
}
#[test]
fn mouse_local_restores_last_local_cursor_and_never_claims_focus() {
    let mut r = router(true);
    let claim = r.focus_claim();
    modes(&mut r, InputMode::Local, InputMode::Shared);
    assert!(r.out.contains(&Output::RestoreLocal {
        at: Point::new(123., 234.)
    }));
    r.out.clear();
    for e in [
        InputEvent::PointerDelta { dx: 20., dy: 0. },
        InputEvent::RawPointerDelta { dx: 20., dy: 0. },
        InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        },
        InputEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        },
        InputEvent::Scroll {
            dx: 0.,
            dy: 1.,
            unit: ScrollUnit::Line,
            phase: ScrollPhase::None,
        },
        InputEvent::Gesture(Gesture::SmartMagnify),
    ] {
        assert_eq!(event(&mut r, e), Disposition::Pass);
    }
    assert!(inputs(&r).is_empty());
    assert_eq!(r.focus_claim(), claim);
    assert_eq!(r.holder, PEER);
}
#[test]
fn local_mouse_edges_and_held_drag_never_handoff_but_remote_motion_can() {
    let mut r = router(false);
    let mut layout = crate::SimpleLayout::new();
    for (device, x) in [(ME, 0.), (PEER, 250.)] {
        layout.add_screen(
            ScreenId::new(device, "screen"),
            Rect::new(0., 0., 1000., 600.),
            Rect::new(x, 0., 250., 150.),
        );
    }
    r.set_layout(Arc::new(layout));
    // Complete the peer's opening focus record before testing target admission.
    r.input_message(
        PEER,
        Message::Control(ControlMsg::FocusState {
            epoch: 0,
            holder: ME,
            scope: None,
            session: Some([2; 16]),
            lock_state: None,
        }),
        0,
    );

    modes(&mut r, InputMode::Local, InputMode::Shared);
    r.out.clear();
    event(
        &mut r,
        InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        },
    );
    for _ in 0..30 {
        event(&mut r, InputEvent::PointerDelta { dx: 1000., dy: 0. });
    }
    assert_eq!(r.holder, ME);
    assert!(matches!(r.dnd.state, dnd::State::Idle));
    assert!(
        !r.out
            .iter()
            .any(|o| matches!(o, Output::SetMode(CaptureMode::Remote { .. }) | Output::Dnd(_)))
    );
    event(
        &mut r,
        InputEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        },
    );
    for step in 0..30 {
        r.remote_motion(PEER, 10., 0., step * 1000);
        if r.holder == PEER {
            break;
        }
    }
    assert_eq!(r.holder, PEER, "an incoming shared source can still cross the portal");
    assert!(
        r.out
            .iter()
            .any(|o| matches!(o, Output::SetMode(CaptureMode::Remote { .. }))),
        "a shared source leaving this screen must hide its cursor even with mouse-local enabled"
    );
    let claim = r.focus_claim();
    r.out.clear();
    assert_eq!(
        r.on_local_event(
            &CapturedEvent {
                physical: true,
                event: InputEvent::PointerDelta { dx: -10., dy: 5. },
                source: SourceKind::Mouse,
                time_us: 40_000,
                cursor: None,
            },
            40_000,
        ),
        Disposition::Pass
    );
    assert!(
        r.out.iter().any(|o| matches!(o, Output::RestoreLocal { .. })),
        "the device's own mouse must reveal a usable local cursor on first motion"
    );
    assert!(inputs(&r).is_empty());
    assert_eq!(r.focus_claim(), claim);
    assert_eq!(r.holder, PEER);
}
#[test]
fn keyboard_local_keeps_modifiers_repeats_media_and_hotkeys_local() {
    let mut r = router(true);
    modes(&mut r, InputMode::Shared, InputMode::Local);
    r.out.clear();
    r.set_volume_exit_probe(Arc::new(|| Some(super::test_support::volume_route(ME, PEER))));
    for code in [A, KeyCode::LEFT_SHIFT, KeyCode::new(12, 0xe9)] {
        for pressed in [true, true, false] {
            assert_eq!(event(&mut r, InputEvent::Key { code, pressed }), Disposition::Pass);
        }
    }
    assert!(inputs(&r).is_empty());
    event(
        &mut r,
        InputEvent::Key {
            code: KeyCode::LEFT_SHIFT,
            pressed: true,
        },
    );
    r.become_holder(2, None);
    r.become_source(3, PEER);
    assert!(inputs(&r).is_empty());
}
#[test]
fn mode_switch_releases_old_targets_and_suppresses_held_tails() {
    let mut r = router(true);
    event(&mut r, InputEvent::Key { code: A, pressed: true });
    event(
        &mut r,
        InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        },
    );
    r.out.clear();
    modes(&mut r, InputMode::Local, InputMode::Local);
    assert!(inputs(&r).contains(&&InputMsg::Key {
        code: A,
        pressed: false
    }));
    assert!(inputs(&r).contains(&&InputMsg::Button {
        button: MouseButton::Left,
        pressed: false
    }));
    r.out.clear();
    assert_eq!(
        event(&mut r, InputEvent::Key { code: A, pressed: true }),
        Disposition::Swallow
    );
    assert_eq!(
        event(
            &mut r,
            InputEvent::Key {
                code: A,
                pressed: false
            }
        ),
        Disposition::Swallow
    );
    assert_eq!(
        event(
            &mut r,
            InputEvent::Button {
                button: MouseButton::Left,
                pressed: false
            }
        ),
        Disposition::Swallow
    );
    assert_eq!(
        event(&mut r, InputEvent::Key { code: A, pressed: true }),
        Disposition::Pass
    );
    assert!(inputs(&r).is_empty());
}
#[test]
fn local_modes_do_not_claim_fresh_join_or_override_disabled_guard() {
    let mut r = router(false);
    r.fresh_join = true;
    modes(&mut r, InputMode::Local, InputMode::Local);
    let claim = r.focus_claim();
    event(&mut r, InputEvent::PointerDelta { dx: 10., dy: 0. });
    event(&mut r, InputEvent::Key { code: A, pressed: true });
    assert!(r.fresh_join);
    assert_eq!(r.focus_claim(), claim);
    r.set_keyboard_disabled(true);
    assert_eq!(
        event(&mut r, InputEvent::Key { code: A, pressed: true }),
        Disposition::Swallow
    );
}
#[test]
fn raw_game_sync_never_replays_local_mouse_motion() {
    let mut r = router(true);
    r.receive_game_state(PEER, 1, 1, GameMode::On, false, true, None);
    event(&mut r, InputEvent::RawPointerDelta { dx: 20., dy: 1. });
    modes(&mut r, InputMode::Local, InputMode::Shared);
    r.out.clear();
    event(&mut r, InputEvent::RawPointerDelta { dx: 40., dy: 2. });
    r.send_state_sync();
    assert!(
        !inputs(&r)
            .iter()
            .any(|i| matches!(i, InputMsg::RelativeMotion { .. } | InputMsg::Motion { .. }))
    );
}

#[test]
fn local_keyboard_hotkey_still_controls_group_lock_and_pause_keeps_modes() {
    let mut r = router(true);
    modes(&mut r, InputMode::Local, InputMode::Local);
    r.set_hotkeys(vec![HotkeyBinding {
        chord: crate::Chord::parse(&["Ctrl", "L"]).unwrap(),
        action: "lock_focus".into(),
    }]);
    event(
        &mut r,
        InputEvent::Key {
            code: KeyCode::LEFT_CTRL,
            pressed: true,
        },
    );
    assert_eq!(
        event(
            &mut r,
            InputEvent::Key {
                code: KeyCode::keyboard(0xf),
                pressed: true
            }
        ),
        Disposition::Swallow
    );
    assert!(
        r.out
            .iter()
            .any(|o| matches!(o,Output::Send{to,msg:Message::Control(ControlMsg::SetFocusLock{..}),..} if *to==PEER))
    );
    assert!(
        r.out
            .iter()
            .any(|o| matches!(o,Output::Status(StatusEvent::Hotkey{action}) if action=="lock_focus"))
    );
    let claim = r.focus_claim();
    event(&mut r, InputEvent::PointerDelta { dx: 500., dy: 0. });
    assert_eq!(r.focus_claim(), claim);
    r.set_fully_paused(true);
    modes(&mut r, InputMode::Shared, InputMode::Shared);
    // A key passed during pause is also retired on a subsequent mode switch.
    assert_eq!(
        event(&mut r, InputEvent::Key { code: A, pressed: true }),
        Disposition::Pass
    );
    modes(&mut r, InputMode::Local, InputMode::Local);
    assert!(r.out.contains(&Output::Inject(InjectEvent::Key {
        code: A,
        pressed: false
    })));
    r.set_fully_paused(false);
    assert_eq!(
        event(&mut r, InputEvent::Key { code: A, pressed: true }),
        Disposition::Swallow
    );
    assert_eq!(
        event(
            &mut r,
            InputEvent::Key {
                code: A,
                pressed: false
            }
        ),
        Disposition::Swallow
    );
}
#[test]
fn local_mode_transforms_stay_local_and_shared_input_can_still_arrive() {
    use oink_foundation::input_transform::{InputTransform, KeyMapping};
    for os in [Os::Windows, Os::MacOS] {
        let mut r = router(true);
        r.cfg.os = os;
        modes(&mut r, InputMode::Local, InputMode::Local);
        let mut transform = InputTransform::default();
        let t = if os == Os::MacOS {
            &mut transform.macos
        } else {
            &mut transform.windows
        };
        t.reverse_wheel = true;
        t.keyboard.table = vec![KeyMapping {
            from: A,
            to: KeyCode::keyboard(5),
        }];
        r.set_input_transforms(HashMap::from([(ME, transform)]));
        r.out.clear();
        assert_eq!(
            event(&mut r, InputEvent::Key { code: A, pressed: true }),
            if os == Os::MacOS {
                Disposition::Pass
            } else {
                Disposition::Swallow
            }
        );
        event(
            &mut r,
            InputEvent::Scroll {
                dx: 0.,
                dy: 1.,
                unit: ScrollUnit::Line,
                phase: ScrollPhase::None,
            },
        );
        assert!(inputs(&r).is_empty());
        r.become_holder(
            2,
            Some(ScreenPos::new(ScreenId::new(ME, "screen"), Point::new(500., 234.))),
        );
        r.out.clear();
        r.target_key(PEER, A, true);
        r.target_button(PEER, MouseButton::Left, true);
        assert!(
            r.out
                .contains(&Output::Inject(InjectEvent::Key { code: A, pressed: true }))
        );
        modes(&mut r, InputMode::Shared, InputMode::Shared);
        assert!(r.held[&PEER].keys.contains_key(&A));
        assert!(r.held[&PEER].buttons.contains(&MouseButton::Left));
    }
}
#[test]
fn local_contacts_and_gestures_never_forward_and_switch_cancels_old_sequences() {
    use oink_foundation::contacts::{Sample, Surface};
    let mut r = router(true);
    r.set_peer_touchpad_contacts(PEER, Some(5));
    r.local_scroll(SourceKind::Touchpad, 0., 1., ScrollUnit::Pixel, ScrollPhase::Began);
    r.local_gesture(
        SourceKind::Touchpad,
        Gesture::Magnify {
            phase: GesturePhase::Began,
            amount: 0.1,
        },
    );
    r.out.clear();
    modes(&mut r, InputMode::Local, InputMode::Shared);
    assert!(inputs(&r).iter().any(|i| matches!(
        i,
        InputMsg::Scroll {
            phase: ScrollPhase::Cancelled,
            ..
        }
    )));
    assert!(inputs(&r).iter().any(|i| matches!(
        i,
        InputMsg::Gesture {
            gesture: Gesture::Magnify {
                phase: GesturePhase::Cancelled,
                ..
            },
            ..
        }
    )));
    r.out.clear();
    event(
        &mut r,
        InputEvent::Gesture(Gesture::Magnify {
            phase: GesturePhase::Changed,
            amount: 0.2,
        }),
    );
    let sample = Sample {
        device: 1,
        time_us: 100,
        surface: Surface {
            width: 15600,
            height: 9600,
        },
        ..Default::default()
    };
    event(&mut r, InputEvent::TouchpadSample(sample));
    assert!(inputs(&r).is_empty());
    assert_eq!(r.contacts_path(), ContactsPath::Local);
}

#[test]
fn diagnostic_decisions_report_policy_and_actual_local_forwarded_or_consumed_path() {
    use std::sync::Mutex;
    #[derive(Default)]
    struct Sink(Mutex<Vec<SourceDecision>>);
    impl RoutingDiagnostics for Sink {
        fn enabled(&self) -> bool {
            true
        }
        fn decision(&self, _: DecisionKind, _: DecisionReason, _: u64, _: DeviceId, _: Option<&ScreenPos>) {}
        fn source_decision(&self, record: SourceDecision) {
            self.0.lock().unwrap().push(record);
        }
    }
    let mut r = router(true);
    let sink = Arc::new(Sink::default());
    r.set_diagnostics(sink.clone());
    event(&mut r, InputEvent::Key { code: A, pressed: true });
    modes(&mut r, InputMode::Local, InputMode::Local);
    event(
        &mut r,
        InputEvent::Key {
            code: A,
            pressed: false,
        },
    );
    event(&mut r, InputEvent::Key { code: A, pressed: true });
    let decisions = sink.0.lock().unwrap();
    assert_eq!(
        decisions.iter().map(|v| v.destination).collect::<Vec<_>>(),
        vec![
            SourceDestination::Forwarded,
            SourceDestination::Consumed,
            SourceDestination::Local
        ]
    );
    assert_eq!(decisions[2].keyboard_mode, InputMode::Local);
    assert_eq!(decisions[2].mouse_mode, InputMode::Local);
}

#[test]
fn local_mouse_does_not_leak_shared_keyboard_modifiers_into_native_click_flags() {
    let mut r = router(true);
    modes(&mut r, InputMode::Local, InputMode::Shared);
    event(
        &mut r,
        InputEvent::Key {
            code: KeyCode::LEFT_SHIFT,
            pressed: true,
        },
    );
    assert!(r.keyboard_pointer_modifiers().contains(&KeyCode::LEFT_SHIFT));
    r.target_key(PEER, KeyCode::LEFT_SHIFT, true);
    assert!(!r.keyboard_pointer_modifiers().contains(&KeyCode::LEFT_SHIFT));
    r.target_key(PEER, KeyCode::LEFT_SHIFT, false);
    event(
        &mut r,
        InputEvent::Key {
            code: KeyCode::LEFT_SHIFT,
            pressed: false,
        },
    );
    modes(&mut r, InputMode::Local, InputMode::Local);
    event(
        &mut r,
        InputEvent::Key {
            code: KeyCode::LEFT_SHIFT,
            pressed: true,
        },
    );
    assert!(!r.keyboard_pointer_modifiers().contains(&KeyCode::LEFT_SHIFT));
}

#[test]
fn local_mouse_departure_and_reveal_are_independent_of_group_focus_on_both_platforms() {
    for os in [Os::MacOS, Os::Windows] {
        let mut r = router(false);
        r.cfg.os = os;
        modes(&mut r, InputMode::Local, InputMode::Shared);
        for epoch in [1, 3] {
            r.out.clear();
            let at = r.pointer.clone().unwrap();
            r.become_source(epoch, PEER);
            assert!(
                r.out
                    .iter()
                    .any(|o| matches!(o, Output::SetMode(CaptureMode::Remote { .. })))
            );
            let claim = r.focus_claim();
            r.out.clear();
            for (physical, e) in [
                (true, InputEvent::PointerDelta { dx: 0., dy: 0. }),
                (true, InputEvent::RawPointerDelta { dx: 1., dy: 2. }),
                (false, InputEvent::PointerDelta { dx: 2., dy: 3. }),
                (true, InputEvent::Key { code: A, pressed: true }),
                (
                    true,
                    InputEvent::Key {
                        code: A,
                        pressed: false,
                    },
                ),
            ] {
                r.on_local_event(
                    &CapturedEvent {
                        physical,
                        event: e,
                        source: SourceKind::Mouse,
                        cursor: None,
                        time_us: 100,
                    },
                    100,
                );
            }
            assert!(!r.out.iter().any(|o| matches!(o, Output::RestoreLocal { .. })));
            r.out.clear();
            let e = CapturedEvent {
                physical: true,
                event: InputEvent::PointerDelta { dx: 2., dy: 3. },
                source: SourceKind::Mouse,
                cursor: Some(Point::new(500., 300.)),
                time_us: 200,
            };
            assert_eq!(r.on_local_event(&e, 200), Disposition::Pass);
            assert!(r.out.contains(&Output::RestoreLocal {
                at: Point::new(at.local.x + 2., at.local.y + 3.)
            }));
            assert_eq!(
                r.local_cursor.as_ref().unwrap().local,
                Point::new(at.local.x + 2., at.local.y + 3.)
            );
            assert!(inputs(&r).is_empty());
            assert_eq!(r.focus_claim(), claim);
            assert_eq!(r.holder, PEER);
            r.out.clear();
            r.on_local_event(
                &CapturedEvent {
                    cursor: Some(Point::new(150., 250.)),
                    ..e
                },
                300,
            );
            assert!(!r.out.iter().any(|o| matches!(o, Output::RestoreLocal { .. })));
            r.become_holder(epoch + 1, Some(at));
        }
    }
}

#[test]
fn first_local_button_after_departure_is_restored_then_injected_with_a_paired_release() {
    let mut r = router(false);
    modes(&mut r, InputMode::Local, InputMode::Shared);
    r.become_source(1, PEER);
    r.out.clear();
    let claim = r.focus_claim();
    let e = CapturedEvent {
        physical: true,
        event: InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        },
        source: SourceKind::Mouse,
        cursor: None,
        time_us: 100,
    };
    assert_eq!(r.on_local_event(&e, 100), Disposition::Swallow);
    let restore = r
        .out
        .iter()
        .position(|o| matches!(o, Output::RestoreLocal { .. }))
        .unwrap();
    let down = Output::Inject(InjectEvent::Button {
        button: MouseButton::Left,
        pressed: true,
    });
    assert!(r.out.iter().position(|o| o == &down).unwrap() > restore);
    r.out.clear();
    assert_eq!(
        r.on_local_event(
            &CapturedEvent {
                event: InputEvent::Button {
                    button: MouseButton::Left,
                    pressed: false
                },
                ..e
            },
            200
        ),
        Disposition::Swallow
    );
    assert_eq!(
        r.out
            .iter()
            .filter(|o| matches!(
                o,
                Output::Inject(InjectEvent::Button {
                    button: MouseButton::Left,
                    pressed: false
                })
            ))
            .count(),
        1
    );
    assert!(inputs(&r).is_empty());
    assert_eq!(r.focus_claim(), claim);
    assert_eq!(r.holder, PEER);
}

#[test]
fn first_local_wheel_after_departure_keeps_local_transform_and_never_forwards() {
    for os in [Os::MacOS, Os::Windows] {
        let mut r = router(false);
        r.cfg.os = os;
        let mut transform = oink_foundation::input_transform::InputTransform::initial(os.as_str());
        transform.macos.reverse_wheel = true;
        transform.windows.reverse_wheel = true;
        r.set_local_input_transform(transform);
        modes(&mut r, InputMode::Local, InputMode::Shared);
        r.become_source(1, PEER);
        r.out.clear();
        assert_eq!(
            r.on_local_event(
                &CapturedEvent {
                    physical: true,
                    source: SourceKind::Mouse,
                    event: InputEvent::Scroll {
                        dx: 1.,
                        dy: 2.,
                        unit: ScrollUnit::Line,
                        phase: ScrollPhase::None
                    },
                    cursor: None,
                    time_us: 100
                },
                100
            ),
            Disposition::Swallow
        );
        assert!(r.out.contains(&Output::Inject(InjectEvent::Scroll {
            dx: -1.,
            dy: -2.,
            unit: ScrollUnit::Line,
            phase: ScrollPhase::None
        })));
        assert!(r.out.iter().any(|o| matches!(o, Output::RestoreLocal { .. })));
        assert!(inputs(&r).is_empty());
        assert_eq!(r.local_rewrite(), None);
    }
}
