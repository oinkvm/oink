//! Windows capture -> source router -> Mac router -> recording injector. No OS input.
use super::*;
use oink_input::{InjectEvent, InputBackend, InputEvent, KeyCode, MouseButton, SourceKind};
use oink_routing::Output;
use oink_routing::input::InputMsg;

fn event(event: InputEvent) -> CapturedEvent {
    CapturedEvent {
        event,
        physical: true,
        source: SourceKind::Keyboard,
        time_us: 1,
        cursor: None,
    }
}
pub(super) fn pair() -> (Router, Router) {
    pair_target(Os::MacOS)
}
pub(super) fn pair_target(target_os: Os) -> (Router, Router) {
    let layout = Arc::new(oink_routing::TopologyLayout::from_layout(
        &fixture_layout(DeviceId([1; 32])),
        &Default::default(),
    ));
    let mut source = Router::new(
        RouterConfig::new(fixture_boot(DeviceId([1; 32])), "Windows", Os::Windows),
        layout.clone(),
    );
    let mut target = Router::new(RouterConfig::new(fixture_boot(peer()), "target", target_os), layout);
    source.attach_input(
        fixture_boot(peer()),
        target.routing_session(),
        true,
        false,
        Default::default(),
        0,
    );
    target.attach_input(
        fixture_boot(source.device()),
        source.routing_session(),
        true,
        false,
        Default::default(),
        0,
    );
    target.set_peer_os(source.device(), Os::Windows, 0);
    source.set_peer_os(peer(), target_os, 0);
    let scope = oink_routing::control::FocusScope {
        members: vec![
            (source.device(), source.routing_session()),
            (target.device(), target.routing_session()),
        ],
    };
    target.input_message(
        source.device(),
        Message::Control(ControlMsg::FocusState {
            lock_state: None,
            epoch: 100,
            holder: peer(),
            scope: Some(scope.clone()),
            session: Some(source.routing_session()),
        }),
        1,
    );
    source.input_message(
        peer(),
        Message::Control(ControlMsg::FocusState {
            lock_state: None,
            epoch: 100,
            holder: peer(),
            scope: Some(scope),
            session: Some(target.routing_session()),
        }),
        1,
    );
    assert_eq!(source.focus().holder, peer());
    source.drain_outputs();
    target.drain_outputs();
    (source, target)
}
fn deliver(
    source: &mut Router,
    target: &mut Router,
    fake: &oink_test_support::input::FakeInput,
    drop_releases: bool,
) -> Vec<Message> {
    let mut sent = Vec::new();
    for output in source.drain_outputs() {
        if let Output::Send { to, msg, .. } = output {
            if to != target.device() {
                continue;
            }
            if drop_releases
                && matches!(
                    msg,
                    Message::Input(InputMsg::Key { pressed: false, .. } | InputMsg::Button { pressed: false, .. })
                )
            {
                continue;
            }
            target.input_message(source.device(), msg.clone(), 2);
            sent.push(msg);
        }
    }
    for output in target.drain_outputs() {
        if let Output::Inject(event) = output {
            fake.inject(&event).unwrap();
        }
    }
    sent
}
#[test]
fn windows_capture_loss_releases_mapped_control_and_held_sync_heals_a_lost_release() {
    for drop_release in [false, true] {
        let (mut source, mut target) = pair();
        let fake = oink_test_support::input::FakeInput::new();
        let mut capture = oink_test_support::input::WindowsCapture::default();
        let down = event(InputEvent::Key {
            code: KeyCode::LEFT_GUI,
            pressed: true,
        });
        capture.observe(&down.event);
        source.on_local_event(&down, 10);
        deliver(&mut source, &mut target, &fake, false);
        assert!(fake.take_injected().iter().any(|e| matches!(
            e,
            InjectEvent::Key {
                code: KeyCode::LEFT_CTRL,
                pressed: true,
                ..
            }
        )));
        // Desktop becomes unavailable. No physical UP reaches the hook.
        for release in capture.transition(false, |_| panic!("must not query inaccessible desktop"), 20) {
            source.on_local_event(&release, 20);
        }
        deliver(&mut source, &mut target, &fake, drop_release);
        if !drop_release {
            assert!(
                fake.injected().iter().any(|e| matches!(
                    e,
                    InjectEvent::Key {
                        code: KeyCode::LEFT_CTRL,
                        pressed: false,
                        ..
                    }
                )),
                "capture loss must release before unlocking"
            );
        }
        // Real keyboard says it was released while locked. Resume must not resurrect it.
        for resumed in capture.transition(true, |_| false, 30) {
            source.on_local_event(&resumed, 30);
        }
        let mut sent = Vec::new();
        for now in [250_000, 500_000, 750_000, 1_000_000] {
            source.tick(now);
            sent.extend(deliver(&mut source, &mut target, &fake, false));
        }
        assert!(sent.iter().any(
            |m| matches!(m, Message::Input(InputMsg::HeldSync {keys, buttons}) if keys.is_empty() && buttons.is_empty())
        ));
        source.on_local_event(
            &event(InputEvent::Button {
                button: MouseButton::Left,
                pressed: true,
            }),
            1_100_001,
        );
        deliver(&mut source, &mut target, &fake, false);
        let mut held = true;
        let mut clicked = false;
        for injected in fake.take_injected() {
            match injected {
                InjectEvent::Key {
                    code: KeyCode::LEFT_CTRL,
                    pressed,
                    ..
                } => held = pressed,
                InjectEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                } => {
                    assert!(!held, "click must carry no stale Control");
                    clicked = true;
                }
                _ => {}
            }
        }
        assert!(clicked);
        assert_eq!(
            source.focus().holder,
            peer(),
            "capture loss does not implement focus takeback"
        );
    }
}

#[test]
fn local_windows_key_released_while_locked_is_not_retargeted_to_mac() {
    let (mut source, mut target) = pair();
    let fake = oink_test_support::input::FakeInput::new();
    let mut capture = oink_test_support::input::WindowsCapture::default();
    source.return_pointer_home();
    source.drain_outputs();
    let down = event(InputEvent::Key {
        code: KeyCode::LEFT_GUI,
        pressed: true,
    });
    capture.observe(&down.event);
    assert_eq!(source.on_local_event(&down, 10), Disposition::Pass);
    for released in capture.transition(false, |_| panic!("secure desktop"), 20) {
        source.on_local_event(&released, 20);
    }
    for resumed in capture.transition(true, |_| false, 30) {
        source.on_local_event(&resumed, 30);
    }
    let scope = oink_routing::control::FocusScope {
        members: vec![
            (source.device(), source.routing_session()),
            (target.device(), target.routing_session()),
        ],
    };
    source.input_message(
        target.device(),
        Message::Control(ControlMsg::FocusState {
            lock_state: None,
            epoch: 200,
            holder: target.device(),
            scope: Some(scope),
            session: Some(target.routing_session()),
        }),
        40,
    );
    let sent = deliver(&mut source, &mut target, &fake, false);
    assert!(!sent.iter().any(|m| matches!(
        m,
        Message::Input(InputMsg::Key {
            code: KeyCode::LEFT_GUI,
            pressed: true
        })
    )));
    assert_eq!(source.diagnostic_key_state(KeyCode::LEFT_GUI, false).1, 0);
}

#[test]
fn secure_desktop_reclaims_own_source_releases_and_gates_until_default() {
    let (mut source, mut target) = pair();
    let fake = oink_test_support::input::FakeInput::new();
    source.on_local_event(
        &event(InputEvent::Key {
            code: KeyCode::LEFT_GUI,
            pressed: true,
        }),
        10,
    );
    source.on_local_event(
        &event(InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        }),
        11,
    );
    deliver(&mut source, &mut target, &fake, false);
    fake.take_injected();
    source.set_capture_available(false);
    assert_eq!(source.focus().holder, source.device());
    let epoch = source.focus().epoch;
    deliver(&mut source, &mut target, &fake, false);
    assert!(fake.injected().iter().any(|e| matches!(
        e,
        InjectEvent::Key {
            code: KeyCode::LEFT_CTRL,
            pressed: false,
            ..
        }
    )));
    assert!(fake.injected().iter().any(|e| matches!(
        e,
        InjectEvent::Button {
            button: MouseButton::Left,
            pressed: false,
            ..
        }
    )));
    for input in [
        InputEvent::PointerDelta { dx: 10000., dy: 0. },
        InputEvent::Key {
            code: KeyCode(0x070004),
            pressed: true,
        },
    ] {
        assert_eq!(source.on_local_event(&event(input), 20), Disposition::Pass);
    }
    source.set_capture_available(false);
    assert_eq!(source.focus().epoch, epoch);
    assert!(source.drain_outputs().is_empty());
    source.set_capture_available(true);
    assert_eq!(source.focus().holder, source.device());
    assert_eq!(
        source.focus().epoch,
        epoch,
        "Default recovery must not restore the old remote focus"
    );
}

#[test]
fn secure_desktop_does_not_steal_another_devices_focus() {
    let (mut source, _target) = pair();
    let before = source.focus();
    source.set_capture_available(false);
    assert_eq!(source.focus().holder, before.holder);
    assert_eq!(source.focus().epoch, before.epoch);
    assert!(
        !source.paused(),
        "secure source desktop must not disable remote injection"
    );
    source.set_capture_available(true);
    assert_eq!(source.focus().holder, before.holder);
    assert_eq!(
        source.on_local_event(
            &event(InputEvent::Key {
                code: KeyCode(0x070004),
                pressed: true
            }),
            30
        ),
        Disposition::Swallow
    );
    assert!(source.drain_outputs().iter().any(|o| matches!(
        o,
        Output::Send {
            msg: Message::Input(InputMsg::Key { pressed: true, .. }),
            ..
        }
    )));
}

#[test]
fn native_contacts_keep_desktop_and_start_gestures_out_of_the_shortcut_mapper() {
    use oink_foundation::input::{Gesture, GesturePhase, SwipeAxis};
    let (mut source, mut target) = pair_target(Os::Windows);
    source.set_peer_touchpad_contacts(target.device(), Some(5));
    target.set_touchpad_contacts(Some(5));
    target.set_peer_os(source.device(), Os::MacOS, 0);
    target.set_accept_contacts(true);
    for axis in [SwipeAxis::Vertical, SwipeAxis::Pinch] {
        for progress in [-0.2, 0.2] {
            for phase in [GesturePhase::Began, GesturePhase::Ended] {
                source.on_local_event(
                    &event(InputEvent::Gesture(Gesture::DockSwipe {
                        axis,
                        phase,
                        progress,
                        velocity: 0.,
                    })),
                    0,
                );
                for out in source.drain_outputs() {
                    if let Output::Send { msg, .. } = out {
                        target.input_message(source.device(), msg, 0);
                    }
                }
                assert!(
                    !target
                        .drain_outputs()
                        .iter()
                        .any(|out| matches!(out, Output::Inject(InjectEvent::Gesture(_))))
                );
            }
        }
    }
}

#[test]
fn target_contacts_off_reaches_windows_shortcut_mapper() {
    use oink_foundation::input::{Gesture, GesturePhase, SwipeAxis};
    use oink_input::win_actions::{GestureMapper, WinAction};
    let (mut source, mut target) = pair_target(Os::Windows);
    source.set_peer_touchpad_contacts(target.device(), Some(5));
    target.set_accept_contacts(false);
    let mut mapper = GestureMapper::default();
    for (axis, progress, shortcut) in [
        (
            SwipeAxis::Vertical,
            -0.2,
            vec![KeyCode::LEFT_GUI, KeyCode::keyboard(0x2b)],
        ),
        (SwipeAxis::Vertical, 0.2, vec![KeyCode::keyboard(0x29)]),
        (
            SwipeAxis::Horizontal,
            0.2,
            vec![KeyCode::LEFT_CTRL, KeyCode::LEFT_GUI, KeyCode::keyboard(0x4f)],
        ),
    ] {
        let mut actions = Vec::new();
        for phase in [GesturePhase::Began, GesturePhase::Changed, GesturePhase::Ended] {
            source.on_local_event(
                &event(InputEvent::Gesture(Gesture::DockSwipe {
                    axis,
                    phase,
                    progress,
                    velocity: 1.,
                })),
                0,
            );
            for out in source.drain_outputs() {
                if let Output::Send { msg, .. } = out {
                    target.input_message(source.device(), msg, 0);
                }
            }
            for out in target.drain_outputs() {
                if let Output::Inject(InjectEvent::Gesture(g)) = out {
                    actions.extend(mapper.map_with_foreground_query(&g, |_| {
                        oink_input::win_actions::ForegroundState {
                            task_view_showing: true,
                            ..Default::default()
                        }
                    }));
                }
            }
        }
        assert_eq!(actions, vec![WinAction::Shortcut(shortcut)]);
    }
    source.on_local_event(
        &event(InputEvent::Gesture(Gesture::Magnify {
            phase: GesturePhase::Began,
            amount: 0.1,
        })),
        0,
    );
    let mut actions = Vec::new();
    for out in source.drain_outputs() {
        if let Output::Send { msg, .. } = out {
            target.input_message(source.device(), msg, 0);
        }
    }
    for out in target.drain_outputs() {
        if let Output::Inject(InjectEvent::Gesture(g)) = out {
            actions.extend(mapper.map(&g));
        }
    }
    assert_eq!(actions, vec![WinAction::CtrlWheel(1)]);
}

#[test]
fn touchpad_options_matrix_reaches_windows_mapper_for_non_scroll_gestures() {
    use oink_foundation::input::{Gesture, GesturePhase, SwipeAxis};
    use oink_input::win_actions::{GestureMapper, WinAction};
    for parent in [false, true] {
        for native in [false, true] {
            for mac in [false, true] {
                for gesture in [
                    Gesture::Magnify {
                        phase: GesturePhase::Began,
                        amount: 0.2,
                    },
                    Gesture::Rotate {
                        phase: GesturePhase::Began,
                        degrees: 30.,
                    },
                    Gesture::SmartMagnify,
                    Gesture::DockSwipe {
                        axis: SwipeAxis::Horizontal,
                        phase: GesturePhase::Ended,
                        progress: 0.2,
                        velocity: 0.,
                    },
                ] {
                    let (source, mut target) = pair_target(Os::Windows);
                    target.set_touchpad_contacts(Some(5));
                    target.set_peer_os(source.device(), Os::MacOS, 0);
                    target.set_peer_touchpad_options(source.device(), true);
                    let mut settings = target.config().settings.clone();
                    settings.touchpad_injection = parent;
                    settings.native_multifinger = native;
                    settings.mac_scroll_feel = mac;
                    target.set_settings(settings);
                    target.drain_outputs();
                    // A committed Dock swipe is not a start; send its Began first.
                    if let Gesture::DockSwipe { axis, .. } = gesture {
                        target.input_message(
                            source.device(),
                            Message::Input(oink_routing::input::InputMsg::Gesture {
                                source: SourceKind::Touchpad,
                                gesture: Gesture::DockSwipe {
                                    axis,
                                    phase: GesturePhase::Began,
                                    progress: 0.,
                                    velocity: 0.,
                                },
                            }),
                            0,
                        );
                        target.drain_outputs();
                    }
                    target.input_message(
                        source.device(),
                        Message::Input(oink_routing::input::InputMsg::Gesture {
                            source: SourceKind::Touchpad,
                            gesture,
                        }),
                        0,
                    );
                    let mut mapper = GestureMapper::default();
                    let mut mapped = false;
                    let mut actions = Vec::new();
                    for out in target.drain_outputs() {
                        if let Output::Inject(InjectEvent::Gesture(g)) = out {
                            mapped = true;
                            actions.extend(mapper.map(&g));
                        }
                    }
                    assert_eq!(mapped, !parent || !native, "{parent} {native} {mac} {gesture:?}");
                    if !parent || !native {
                        match gesture {
                            Gesture::Magnify { .. } => assert_eq!(actions, [WinAction::CtrlWheel(2)]),
                            Gesture::DockSwipe { .. } => {
                                assert!(matches!(actions.as_slice(), [WinAction::Shortcut(_)]))
                            }
                            _ => assert!(actions.is_empty()),
                        }
                    }
                }
            }
        }
    }
}
