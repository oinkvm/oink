use super::test_support::volume_route;
use super::*;
const A: DeviceId = DeviceId([1; 32]);
const B: DeviceId = DeviceId([2; 32]);
const E: DeviceId = DeviceId([3; 32]);
const UP: KeyCode = KeyCode::new(0x0c, 0xe9);

#[derive(Default)]
struct VolumeEvidence(std::sync::Mutex<Vec<VolumeDecision>>);
impl RoutingDiagnostics for VolumeEvidence {
    fn enabled(&self) -> bool {
        true
    }
    fn volume(&self, record: VolumeDecision) {
        self.0.lock().unwrap().push(record);
    }
    fn decision(&self, _: DecisionKind, _: DecisionReason, _: u64, _: DeviceId, _: Option<&ScreenPos>) {}
}

#[test]
fn volume_transition_records_only_the_final_choice_with_source_provenance() {
    use oink_foundation::input::RecentAudioNotifications;
    for focus in [A, B, E] {
        for physical in [true, false] {
            for keep_local in [true, false] {
                for meta in [false, true] {
                    for code in [UP, KeyCode::new(12, 0xea), KeyCode::new(12, 0xe2)] {
                        let evidence = Arc::new(VolumeEvidence::default());
                        let exit = (focus != E).then_some(E);
                        let mut source = router(A, focus, exit);
                        source.set_diagnostics(evidence.clone());
                        source.set_volume_keep_local_probe(Arc::new(move || keep_local));
                        source.set_volume_notifications_probe(Arc::new(|| {
                            Some(RecentAudioNotifications {
                                default_endpoint_age_ms: Some(37),
                                arrival_removal_age_ms: Some(12),
                            })
                        }));
                        let mut target = router(focus, focus, exit);
                        target.set_diagnostics(evidence.clone());
                        target.set_volume_keep_local_probe(Arc::new(move || !keep_local));
                        if meta {
                            source.local_key(KeyCode::LEFT_GUI, true, 0);
                            for (_, msg) in messages(&mut source) {
                                target.on_input(A, A, 0, msg, 0);
                            }
                        }
                        let event = CapturedEvent {
                            event: InputEvent::Key { code, pressed: true },
                            physical,
                            source: if physical {
                                SourceKind::Keyboard
                            } else {
                                SourceKind::Unknown
                            },
                            time_us: 1,
                            cursor: None,
                        };
                        for _ in 0..2 {
                            source.on_local_event(&event, 1);
                            for (to, msg) in messages(&mut source) {
                                if to == focus && focus != A {
                                    target.on_input(A, A, 0, msg, 1);
                                }
                            }
                        }
                        let records = evidence.0.lock().unwrap();
                        assert_eq!(records.len(), 1, "one decision, no repeat record");
                        let record = records[0];
                        assert_eq!(record.source, A);
                        let is_gain = meta && exit.is_some();
                        assert_eq!(
                            record.action,
                            match (code, is_gain) {
                                (UP, false) => VolumeAction::VolumeUp,
                                (UP, true) => VolumeAction::GainUp,
                                (c, false) if c == KeyCode::new(12, 0xea) => VolumeAction::VolumeDown,
                                (c, true) if c == KeyCode::new(12, 0xea) => VolumeAction::GainDown,
                                (_, false) => VolumeAction::Mute,
                                (_, true) => VolumeAction::ForwardMute,
                            }
                        );
                        assert_eq!(record.focus, focus);
                        assert_eq!(
                            record.origin_kind,
                            if physical {
                                VolumeOriginKind::Physical
                            } else {
                                VolumeOriginKind::Synthetic
                            }
                        );
                        assert_eq!(record.source_keep_local_playback, Some(keep_local));
                        assert_eq!(
                            record.focus_keep_local_playback,
                            Some(if focus == A { keep_local } else { !keep_local })
                        );
                        assert_eq!(record.windows_notifications.unwrap().default_endpoint_age_ms, Some(37));
                        assert_eq!(record.focus_forwards, exit.is_some());
                        assert_eq!(
                            record.target,
                            if exit.is_none() {
                                if focus == A {
                                    VolumeDestination::Source
                                } else {
                                    VolumeDestination::Focus
                                }
                            } else if meta {
                                VolumeDestination::Gain
                            } else {
                                VolumeDestination::Exit
                            }
                        );
                        drop(records);
                        let mut up = event;
                        up.event = InputEvent::Key { code, pressed: false };
                        source.on_local_event(&up, 2);
                        for (to, msg) in messages(&mut source) {
                            if to == focus && focus != A {
                                target.on_input(A, A, 0, msg, 2);
                            }
                        }
                        source.local_key(KeyCode::keyboard(4), true, 3);
                        for (_, msg) in messages(&mut source) {
                            assert!(
                                !matches!(msg, InputMsg::VolumeKeyOrigin { .. }),
                                "other keys have no extra context"
                            );
                            target.on_input(A, A, 0, msg, 3);
                        }
                        assert_eq!(evidence.0.lock().unwrap().len(), 1, "no release or ordinary-key record");
                    }
                }
            }
        }
    }
}

#[test]
fn device_notification_context_and_key_origin_leave_volume_actions_unchanged() {
    use oink_foundation::input::RecentAudioNotifications;
    for focus in [A, B, E] {
        for forwarding in [true, false] {
            for keep_local in [true, false] {
                for meta in [true, false] {
                    for code in [UP, KeyCode::new(12, 0xea), KeyCode::new(12, 0xe2)] {
                        let press = |physical, notifications| {
                            let exit = (forwarding && focus != E).then_some(E);
                            let mut source = router(A, focus, Some(E));
                            source.cfg.os = Os::Windows;
                            source.set_volume_keep_local_probe(Arc::new(move || keep_local));
                            source.set_volume_notifications_probe(Arc::new(move || notifications));
                            let mut target = router(focus, focus, exit);
                            target.set_volume_keep_local_probe(Arc::new(move || keep_local));
                            target.set_input_transforms(std::collections::HashMap::from([(
                                A,
                                oink_foundation::input_transform::InputTransform::initial("windows"),
                            )]));
                            if focus == A {
                                source.set_volume_exit_probe(Arc::new(move || exit.map(|exit| volume_route(A, exit))));
                            }
                            if meta {
                                source.local_key(KeyCode::LEFT_GUI, true, 0);
                                for (_, msg) in messages(&mut source) {
                                    target.on_input(A, A, 0, msg, 0);
                                }
                            }
                            source.out.clear();
                            target.out.clear();
                            let disposition = source.on_local_event(
                                &CapturedEvent {
                                    event: InputEvent::Key { code, pressed: true },
                                    physical,
                                    source: SourceKind::Keyboard,
                                    time_us: 1,
                                    cursor: None,
                                },
                                1,
                            );
                            let outputs = if focus == A {
                                std::mem::take(&mut source.out)
                            } else {
                                for (to, msg) in messages(&mut source) {
                                    assert_eq!(to, focus);
                                    target.on_input(A, A, 0, msg, 1);
                                }
                                std::mem::take(&mut target.out)
                            };
                            let mut sent = Vec::new();
                            let mut injected = Vec::new();
                            let mut gain = Vec::new();
                            for output in outputs {
                                match output {
                                    Output::Send {
                                        to,
                                        msg: Message::Input(msg),
                                        ..
                                    } => sent.push((to, msg)),
                                    Output::Inject(event) => injected.push(event),
                                    Output::Status(StatusEvent::AudioVolumeKey { code }) => gain.push(code),
                                    _ => {}
                                }
                            }
                            (disposition, sent, injected, gain)
                        };
                        let expected = press(true, None);
                        if forwarding && focus != E {
                            assert_eq!(expected.0, Disposition::Swallow);
                            assert!(!expected.2.iter().any(|event| matches!(
                                event,
                                InjectEvent::Key { code: injected, .. } if *injected == code
                            )));
                            if meta {
                                assert!(expected.1.is_empty());
                                assert_eq!(expected.3, vec![code]);
                            } else {
                                assert_eq!(
                                    expected.1,
                                    vec![(
                                        E,
                                        InputMsg::Volume {
                                            route: volume_route(focus, E),
                                            press_id: 1,
                                            code,
                                            pressed: true,
                                            repeat: false,
                                            modifiers: 0
                                        }
                                    )]
                                );
                                assert!(expected.3.is_empty());
                            }
                        } else {
                            assert!(expected.1.is_empty());
                            assert!(expected.3.is_empty());
                            if focus == A {
                                assert_eq!(expected.0, Disposition::Pass);
                            } else {
                                assert_eq!(expected.2, vec![InjectEvent::Key { code, pressed: true }]);
                            }
                        }
                        // Compare actual routing outputs, not just the diagnostic
                        // target: metadata must neither suppress nor reroute keys.
                        for physical in [true, false] {
                            for age in [None, Some(0), Some(5000)] {
                                let context = age.map(|age| RecentAudioNotifications {
                                    default_endpoint_age_ms: Some(age),
                                    arrival_removal_age_ms: Some(age),
                                });
                                assert_eq!(press(physical, context), expected);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn three_device_focus_table_uses_the_focus_source_even_with_windows_meta_remapping() {
    for (keyboard, focus, forwarding, meta) in [
        (A, B, true, false),
        (A, B, false, false),
        (A, E, false, false),
        (E, B, true, false),
        (A, B, true, true),
        (A, B, false, true),
        (E, B, true, true),
        (A, E, false, true),
    ] {
        let mut source = router(keyboard, focus, Some(E));
        source.cfg.os = Os::Windows;
        let mut target = router(focus, focus, forwarding.then_some(E));
        target.input_transforms.insert(
            keyboard,
            oink_foundation::input_transform::InputTransform::initial("windows"),
        );
        if meta {
            source.local_key(KeyCode::LEFT_GUI, true, 0);
            for (to, msg) in messages(&mut source) {
                assert_eq!(to, focus);
                target.on_input(keyboard, keyboard, 0, msg, 0);
            }
        }
        target.out.clear();
        source.local_key(UP, true, 1);
        for (to, msg) in messages(&mut source) {
            assert_eq!(to, focus);
            target.on_input(keyboard, keyboard, 0, msg, 1);
        }
        if forwarding && focus != E {
            if meta {
                assert!(
                    target
                        .out
                        .iter()
                        .any(|o| matches!(o, Output::Status(StatusEvent::AudioVolumeKey { code: UP })))
                );
                assert!(messages(&mut target).is_empty());
            } else {
                assert!(matches!(
                    messages(&mut target).as_slice(),
                    [(
                        E,
                        InputMsg::Volume {
                            code: UP,
                            pressed: true,
                            ..
                        }
                    )]
                ));
            }
            assert!(
                !target
                    .out
                    .iter()
                    .any(|o| matches!(o, Output::Inject(InjectEvent::Key { code: UP, .. })))
            );
        } else {
            assert!(target.out.iter().any(|o| matches!(
                o,
                Output::Inject(InjectEvent::Key {
                    code: UP,
                    pressed: true
                })
            )));
            assert!(messages(&mut target).is_empty());
        }
        source.local_key(UP, false, 2);
        if meta {
            source.local_key(KeyCode::LEFT_GUI, false, 2);
        }
        for (_, msg) in messages(&mut source) {
            target.on_input(keyboard, keyboard, 0, msg, 2);
        }
        assert!(target.held.get(&keyboard).is_none_or(|held| held.keys.is_empty()));
        assert!(target.volume_targets.is_empty());
    }
}

#[test]
fn cmd_volume_action_is_pinned_across_repeat_and_mute_does_not_repeat_toggle() {
    for code in [UP, KeyCode::new(12, 0xea), KeyCode::new(12, 0xe2)] {
        let mut r = router(A, A, Some(E));
        r.cfg.os = Os::Windows;
        r.local_key(KeyCode::LEFT_GUI, true, 0);
        r.out.clear();
        assert_eq!(r.local_key(code, true, 1), Disposition::Swallow);
        assert!(
            r.out
                .iter()
                .any(|o| matches!(o, Output::Status(StatusEvent::AudioVolumeKey { code: c }) if *c == code))
        );
        r.out.clear();
        r.set_volume_exit_probe(Arc::new(|| None));
        assert_eq!(r.local_key(code, true, 2), Disposition::Swallow);
        let actions = r
            .out
            .iter()
            .filter(|o| matches!(o, Output::Status(StatusEvent::AudioVolumeKey { .. })))
            .count();
        assert_eq!(actions, usize::from(code != KeyCode::new(12, 0xe2)));
        assert!(messages(&mut r).is_empty());
        assert_eq!(r.local_key(code, false, 3), Disposition::Swallow);
        assert_eq!(r.local_key(KeyCode::LEFT_GUI, false, 3), Disposition::Pass);
        assert!(r.volume_targets.is_empty());
        assert!(r.keys.is_empty());
    }
}

#[test]
fn volume_press_after_focus_handoff_uses_the_new_focus_and_repeats_keep_the_original_exit() {
    let mut source = router(A, B, None);
    let mut b = router(B, B, Some(E));
    source.local_key(UP, true, 0);
    for (_, msg) in messages(&mut source) {
        b.on_input(A, A, 0, msg, 0);
    }
    messages(&mut b);
    b.set_volume_exit_probe(Arc::new(|| None));
    source.local_key(UP, true, 1);
    for (_, msg) in messages(&mut source) {
        b.on_input(A, A, 0, msg, 1);
    }
    assert!(matches!(
        messages(&mut b).as_slice(),
        [(
            E,
            InputMsg::Volume {
                pressed: true,
                repeat: true,
                ..
            }
        )]
    ));
    source.local_key(UP, false, 2);
    for (_, msg) in messages(&mut source) {
        b.on_input(A, A, 0, msg, 2);
    }
    assert!(matches!(
        messages(&mut b).as_slice(),
        [(E, InputMsg::Volume { pressed: false, .. })]
    ));
    source.holder = E;
    source.claim = E;
    let mut e = router(E, E, None);
    source.local_key(UP, true, 3);
    for (to, msg) in messages(&mut source) {
        assert_eq!(to, E);
        e.on_input(A, A, 0, msg, 3);
    }
    assert!(e.out.iter().any(|o| matches!(
        o,
        Output::Inject(InjectEvent::Key {
            code: UP,
            pressed: true
        })
    )));
    assert!(messages(&mut e).is_empty());
}

fn router(me: DeviceId, focus: DeviceId, exit: Option<DeviceId>) -> Router {
    let mut cfg = RouterConfig::new(
        oink_foundation::session::Boot {
            device_id: me,
            nonce: [me.0[0]; 16],
        },
        "device",
        Os::MacOS,
    );
    cfg.capture_reports_key_repeat = true;
    let mut r = Router::new(cfg, Arc::new(crate::SimpleLayout::default()));
    for peer in [A, B, E].into_iter().filter(|p| *p != me) {
        r.test_connect(peer, 0);
    }
    r.holder = focus;
    r.claim = focus;
    r.epoch = 1;
    r.focus_joined = true;
    r.focus_scope = Some(r.new_focus_scope());
    r.fresh_join = false;
    r.set_volume_exit_probe(Arc::new(move || exit.map(|exit| volume_route(me, exit))));
    r.out.clear();
    r
}
fn messages(r: &mut Router) -> Vec<(DeviceId, InputMsg)> {
    std::mem::take(&mut r.out)
        .into_iter()
        .filter_map(|o| match o {
            Output::Send {
                to,
                msg: Message::Input(msg),
                ..
            } => Some((to, msg)),
            _ => None,
        })
        .collect()
}

#[test]
fn local_volume_is_swallowed_and_sent_to_nonfocus_exit_with_modifiers_and_repeat() {
    for code in [
        UP,
        KeyCode::new(12, 0xea),
        KeyCode::new(12, 0xe2),
        KeyCode::keyboard(0x80),
    ] {
        let mut r = router(A, A, Some(E));
        r.local_key(KeyCode::LEFT_ALT, true, 0);
        r.local_key(KeyCode::LEFT_SHIFT, true, 0);
        for (pressed, repeat) in [(true, false), (true, true), (false, false)] {
            assert_eq!(r.local_key(code, pressed, 1), Disposition::Swallow);
            assert_eq!(
                messages(&mut r),
                vec![(
                    E,
                    InputMsg::Volume {
                        route: volume_route(A, E),
                        press_id: 1,
                        code,
                        pressed,
                        repeat,
                        modifiers: 6
                    }
                )]
            );
        }
    }
}

#[test]
fn remote_focus_decides_using_its_own_audio_state() {
    // A's audio route is irrelevant when the original target is B.
    let mut a = router(A, B, Some(E));
    let mut b = router(B, B, Some(E));
    for code in [KeyCode::LEFT_ALT, KeyCode::LEFT_SHIFT] {
        a.local_key(code, true, 0);
        for (_, msg) in messages(&mut a) {
            b.on_input(A, A, 0, msg, 0);
        }
    }
    b.out.clear();
    for (pressed, repeat) in [(true, false), (true, true), (false, false)] {
        assert_eq!(a.local_key(UP, pressed, 1), Disposition::Swallow);
        for (to, msg) in messages(&mut a) {
            assert_eq!(to, B);
            b.on_input(A, A, 0, msg, 1);
        }
        assert!(!b.out.iter().any(|o| matches!(o, Output::Inject(_))));
        assert_eq!(
            messages(&mut b),
            vec![(
                E,
                InputMsg::Volume {
                    route: volume_route(B, E),
                    press_id: 1,
                    code: UP,
                    pressed,
                    repeat,
                    modifiers: 6
                }
            )]
        );
    }
}

#[test]
fn inactive_or_self_exit_preserves_local_and_remote_behavior_and_playback_keys() {
    for exit in [None, Some(A)] {
        let mut a = router(A, A, exit);
        assert_eq!(a.local_key(UP, true, 0), Disposition::Pass);
        assert_eq!(a.local_key(UP, false, 1), Disposition::Pass);
        assert!(messages(&mut a).is_empty());
    }
    for exit in [None, Some(B)] {
        let mut b = router(B, B, exit);
        b.on_input(
            A,
            A,
            0,
            InputMsg::Key {
                code: UP,
                pressed: true,
            },
            0,
        );
        assert!(b.out.iter().any(|o| matches!(
            o,
            Output::Inject(InjectEvent::Key {
                code: UP,
                pressed: true
            })
        )));
        assert!(messages(&mut b).is_empty());
    }
    let mut a = router(A, A, Some(E));
    for usage in [0xcd, 0xb5, 0xb6] {
        assert_eq!(a.local_key(KeyCode::new(12, usage), true, 0), Disposition::Pass);
    }
    a.test_disconnect(E, 1);
    assert_eq!(a.local_key(UP, true, 2), Disposition::Pass);
}

#[test]
fn exit_delivers_without_focus_or_second_redirect_and_rejects_other_keys() {
    let mut e = router(E, B, Some(A));
    e.epoch = 0;
    for (pressed, repeat) in [(true, false), (true, true), (false, false)] {
        e.on_input(
            B,
            B,
            0,
            InputMsg::Volume {
                route: volume_route(B, E),
                press_id: 1,
                code: UP,
                pressed,
                repeat,
                modifiers: 6,
            },
            0,
        );
        assert!(
            matches!(e.out.pop(), Some(Output::ExitVolumeKey { code: UP, pressed: p, repeat: r, modifiers: 6, .. }) if p == pressed && r == repeat)
        );
        assert!(messages(&mut e).is_empty());
    }
    e.on_input(
        B,
        B,
        0,
        InputMsg::Volume {
            route: volume_route(B, E),
            press_id: 1,
            code: KeyCode::keyboard(4),
            pressed: true,
            repeat: false,
            modifiers: 0,
        },
        0,
    );
    assert!(e.out.is_empty());
    e.paused = true;
    e.on_input(
        B,
        B,
        0,
        InputMsg::Volume {
            route: volume_route(B, E),
            press_id: 1,
            code: UP,
            pressed: true,
            repeat: false,
            modifiers: 0,
        },
        0,
    );
    assert!(e.out.is_empty());
}

#[test]
fn release_and_held_sync_follow_original_exit_after_audio_stops() {
    let mut b = router(B, B, Some(E));
    b.on_input(
        A,
        A,
        0,
        InputMsg::Key {
            code: UP,
            pressed: true,
        },
        0,
    );
    messages(&mut b);
    b.set_volume_exit_probe(Arc::new(|| None));
    b.held_sync(A, A, &[], &[]);
    assert_eq!(
        messages(&mut b),
        vec![(
            E,
            InputMsg::Volume {
                route: volume_route(B, E),
                press_id: 1,
                code: UP,
                pressed: false,
                repeat: false,
                modifiers: 0
            }
        )]
    );
    assert!(b.volume_targets.is_empty());
}

#[test]
fn volume_repeat_and_release_keep_initial_claim_context_after_rebinding() {
    use oink_protocol::v1::schema::{WireDto, WireValue};
    let mut source = router(A, A, Some(E));
    source.local_key(UP, true, 1);
    let first = messages(&mut source).pop().unwrap().1;
    let InputMsg::Volume {
        route: initial,
        press_id,
        ..
    } = first
    else {
        panic!("directed volume")
    };
    let mut replacement = volume_route(A, E).session.raw().clone();
    replacement.insert(2, 2u64.write());
    let replacement = crate::input::VolumeRoute::from_session(Arc::new(
        oink_protocol::v1::dto::AudioSessionContext::from_raw(replacement).unwrap(),
    ))
    .unwrap();
    source.set_volume_exit_probe(Arc::new(move || Some(replacement.clone())));
    for pressed in [true, false] {
        source.local_key(UP, pressed, 2);
        let message = messages(&mut source).pop().unwrap().1;
        assert!(matches!(message,InputMsg::Volume {route,press_id:found,..} if route==initial && found==press_id));
    }
    source.local_key(UP, true, 3);
    assert!(matches!(messages(&mut source).pop().unwrap().1,InputMsg::Volume {press_id:next,..} if next>press_id));
}

#[test]
fn stale_volume_target_boot_never_reaches_the_native_audio_owner() {
    let mut target = router(E, E, None);
    let mut route = volume_route(B, E);
    route.target.nonce = [99; 16];
    target.on_input(
        B,
        B,
        0,
        InputMsg::Volume {
            route,
            press_id: 1,
            code: UP,
            pressed: true,
            repeat: false,
            modifiers: 0,
        },
        1,
    );
    assert!(!target.out.iter().any(|output| matches!(
        output,
        Output::ExitVolumeKey { .. } | Output::Inject(InjectEvent::Volume { .. })
    )));
}
