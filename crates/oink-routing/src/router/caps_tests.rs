use super::*;
use oink_foundation::input_transform::InputTransform;
const ME: DeviceId = DeviceId([1; 32]);
const PC: DeviceId = DeviceId([2; 32]);
const LETTER: KeyCode = KeyCode::keyboard(4);
fn pair() -> (Router, Router) {
    let mut source = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: ME,
                nonce: [ME.0[0]; 16],
            },
            "source",
            Os::MacOS,
        ),
        Arc::new(crate::SimpleLayout::new()),
    );
    let mut target = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: PC,
                nonce: [PC.0[0]; 16],
            },
            "target",
            Os::Windows,
        ),
        Arc::new(crate::SimpleLayout::new()),
    );
    source.test_connect(PC, 0);
    target.test_connect(ME, 0);
    source.peers.get_mut(&PC).unwrap().os = Os::Windows;
    target.peers.get_mut(&ME).unwrap().os = Os::MacOS;
    source.focus_joined = true;
    target.focus_joined = true;
    source.fresh_join = false;
    target.fresh_join = false;
    source.peers.get_mut(&PC).unwrap().focus_ready = true;
    target.peers.get_mut(&ME).unwrap().focus_ready = true;
    source.focus_scope = Some(source.new_focus_scope());
    target.focus_scope = Some(target.new_focus_scope());
    source.epoch = 1;
    source.holder = PC;
    source.claim = PC;
    target.epoch = 1;
    target.claim = PC;
    let config: InputTransform = serde_json::from_value(serde_json::json!({
        "windows": {"keyboard": {"preset": "custom", "table": [],
            "caps_tap_hold": {"tap": KeyCode::LEFT_SHIFT, "hold": KeyCode::CAPS_LOCK}}}
    }))
    .unwrap();
    source.set_input_transforms([(ME, config.clone())].into());
    target.set_input_transforms([(ME, config)].into());
    source.drain_outputs();
    target.drain_outputs();
    (source, target)
}
fn key(r: &mut Router, code: KeyCode, pressed: bool, now: Micros) -> Disposition {
    r.on_local_event(
        &CapturedEvent {
            time_us: now,
            physical: true,
            source: SourceKind::Keyboard,
            cursor: None,
            event: InputEvent::Key { code, pressed },
        },
        now,
    )
}
fn deliver(source: &mut Router, target: &mut Router) -> Vec<(KeyCode, bool)> {
    for output in source.drain_outputs() {
        if let Output::Send { to, msg, .. } = output
            && to == PC
        {
            target.input_message(ME, msg, 1);
        }
    }
    target
        .drain_outputs()
        .into_iter()
        .filter_map(|output| match output {
            Output::Inject(InjectEvent::Key { code, pressed }) => Some((code, pressed)),
            _ => None,
        })
        .collect()
}
#[test]
fn caps_tap_waits_for_release_and_never_repeats() {
    let (mut s, mut t) = pair();
    assert_eq!(key(&mut s, KeyCode::CAPS_LOCK, true, 10), Disposition::Swallow);
    assert!(deliver(&mut s, &mut t).is_empty());
    key(&mut s, KeyCode::CAPS_LOCK, true, 20);
    assert!(deliver(&mut s, &mut t).is_empty());
    key(&mut s, KeyCode::CAPS_LOCK, false, 100_000);
    assert_eq!(
        deliver(&mut s, &mut t),
        [(KeyCode::LEFT_SHIFT, true), (KeyCode::LEFT_SHIFT, false)]
    );
}
#[test]
fn caps_hold_fires_at_threshold_before_release() {
    let (mut s, mut t) = pair();
    key(&mut s, KeyCode::CAPS_LOCK, true, 10);
    s.tick(500_009);
    assert!(deliver(&mut s, &mut t).is_empty());
    s.tick(500_010);
    assert_eq!(deliver(&mut s, &mut t), [(KeyCode::CAPS_LOCK, true)]);
    key(&mut s, KeyCode::CAPS_LOCK, false, 700_000);
    assert_eq!(deliver(&mut s, &mut t), [(KeyCode::CAPS_LOCK, false)]);
}
#[test]
fn caps_chord_orders_modifier_before_letter_and_stays_short() {
    let (mut s, mut t) = pair();
    key(&mut s, KeyCode::CAPS_LOCK, true, 10);
    key(&mut s, LETTER, true, 20);
    assert_eq!(deliver(&mut s, &mut t), [(KeyCode::LEFT_SHIFT, true), (LETTER, true)]);
    s.tick(600_000);
    assert!(deliver(&mut s, &mut t).is_empty());
    key(&mut s, LETTER, false, 600_010);
    key(&mut s, KeyCode::CAPS_LOCK, false, 600_020);
    assert_eq!(deliver(&mut s, &mut t), [(LETTER, false), (KeyCode::LEFT_SHIFT, false)]);
}
#[test]
fn caps_focus_change_retires_pending_and_pressed_outputs() {
    for committed in [false, true] {
        let (mut s, mut t) = pair();
        key(&mut s, KeyCode::CAPS_LOCK, true, 10);
        if committed {
            key(&mut s, LETTER, true, 20);
        }
        deliver(&mut s, &mut t);
        s.become_holder(2, None);
        let expected = if committed {
            vec![(KeyCode::LEFT_SHIFT, false)]
        } else {
            vec![]
        };
        assert_eq!(deliver(&mut s, &mut t), expected);
        s.tick(600_000);
        key(&mut s, KeyCode::CAPS_LOCK, false, 700_000);
        assert!(deliver(&mut s, &mut t).is_empty());
    }
}
#[test]
fn caps_pause_releases_and_does_not_emit_a_tap() {
    for committed in [false, true] {
        let (mut s, mut t) = pair();
        key(&mut s, KeyCode::CAPS_LOCK, true, 10);
        if committed {
            s.tick(500_010);
        }
        deliver(&mut s, &mut t);
        s.set_paused(true);
        let expected = if committed {
            vec![(KeyCode::CAPS_LOCK, false)]
        } else {
            vec![]
        };
        assert_eq!(deliver(&mut s, &mut t), expected);
        key(&mut s, KeyCode::CAPS_LOCK, false, 700_000);
        assert!(deliver(&mut s, &mut t).is_empty());
    }
}

#[test]
fn caps_release_or_chord_at_threshold_handles_a_late_timer() {
    for chord in [false, true] {
        let (mut s, mut t) = pair();
        key(&mut s, KeyCode::CAPS_LOCK, true, 10);
        s.next_sync = None;
        assert_eq!(s.next_deadline(), Some(500_010));
        if chord {
            key(&mut s, LETTER, true, 500_010);
            assert_eq!(deliver(&mut s, &mut t), [(KeyCode::CAPS_LOCK, true), (LETTER, true)]);
        }
        key(&mut s, KeyCode::CAPS_LOCK, false, 500_010);
        let expected = if chord {
            vec![(KeyCode::CAPS_LOCK, false)]
        } else {
            vec![(KeyCode::CAPS_LOCK, true), (KeyCode::CAPS_LOCK, false)]
        };
        assert_eq!(deliver(&mut s, &mut t), expected);
    }
}

#[test]
fn caps_can_release_before_letter_and_shares_physical_shift_refcounts() {
    for shift in [false, true] {
        let (mut s, mut t) = pair();
        if shift {
            key(&mut s, KeyCode::LEFT_SHIFT, true, 1);
        }
        deliver(&mut s, &mut t);
        key(&mut s, KeyCode::CAPS_LOCK, true, 10);
        key(&mut s, LETTER, true, 20);
        let expected = if shift {
            vec![(LETTER, true)]
        } else {
            vec![(KeyCode::LEFT_SHIFT, true), (LETTER, true)]
        };
        assert_eq!(deliver(&mut s, &mut t), expected);
        key(&mut s, KeyCode::CAPS_LOCK, false, 30);
        let expected = if shift {
            vec![]
        } else {
            vec![(KeyCode::LEFT_SHIFT, false)]
        };
        assert_eq!(deliver(&mut s, &mut t), expected);
        if shift {
            key(&mut s, KeyCode::LEFT_SHIFT, false, 40);
        }
        key(&mut s, LETTER, false, 50);
        let expected = if shift {
            vec![(KeyCode::LEFT_SHIFT, false), (LETTER, false)]
        } else {
            vec![(LETTER, false)]
        };
        assert_eq!(deliver(&mut s, &mut t), expected);
        assert!(t.key_refs.is_empty());
    }
}

#[test]
fn caps_cleanup_boundaries_cancel_pending_and_release_committed_keys() {
    for boundary in ["disabled", "local", "emergency", "capture", "disconnect"] {
        for committed in [false, true] {
            let (mut s, mut t) = pair();
            key(&mut s, KeyCode::CAPS_LOCK, true, 10);
            if committed {
                key(&mut s, LETTER, true, 20);
            }
            deliver(&mut s, &mut t);
            match boundary {
                "disabled" => s.set_keyboard_disabled(true),
                "local" => {
                    let mut settings = s.cfg.settings.clone();
                    settings.keyboard_mode = InputMode::Local;
                    s.set_settings(settings);
                }
                "emergency" => s.trigger_emergency(),
                "capture" => s.set_capture_available(false),
                "disconnect" => {
                    s.test_disconnect(PC, 30);
                    t.test_disconnect(ME, 30);
                }
                _ => unreachable!(),
            }
            deliver(&mut s, &mut t);
            assert!(!t.key_refs.contains_key(&KeyCode::LEFT_SHIFT), "{boundary}");
            s.tick(600_000);
            assert_eq!(
                key(&mut s, KeyCode::CAPS_LOCK, false, 700_000),
                Disposition::Swallow,
                "{boundary}"
            );
            assert!(deliver(&mut s, &mut t).is_empty(), "{boundary}");
            assert!(s.caps_press.is_none());
        }
    }
}

#[test]
fn caps_reconciled_up_cancels_without_a_tap() {
    let (mut s, mut t) = pair();
    key(&mut s, KeyCode::CAPS_LOCK, true, 10);
    s.on_local_event(
        &CapturedEvent {
            time_us: 20,
            physical: false,
            source: SourceKind::Unknown,
            cursor: None,
            event: InputEvent::Key {
                code: KeyCode::CAPS_LOCK,
                pressed: false,
            },
        },
        20,
    );
    s.tick(600_000);
    assert!(deliver(&mut s, &mut t).is_empty());
    assert!(s.caps_press.is_none());
}

#[test]
fn caps_held_sync_preserves_committed_output_and_heals_a_lost_release() {
    let (mut s, mut t) = pair();
    key(&mut s, KeyCode::CAPS_LOCK, true, 10);
    key(&mut s, LETTER, true, 20);
    deliver(&mut s, &mut t);
    s.send_state_sync();
    assert!(deliver(&mut s, &mut t).is_empty());
    assert!(t.key_refs.contains_key(&KeyCode::LEFT_SHIFT));
    key(&mut s, KeyCode::CAPS_LOCK, false, 30);
    s.drain_outputs(); // Drop the ordered release to exercise source-state healing.
    s.send_state_sync();
    assert_eq!(deliver(&mut s, &mut t), [(KeyCode::LEFT_SHIFT, false)]);
}

#[test]
fn caps_mapping_is_chosen_at_down_and_outputs_are_not_mapped_again() {
    use oink_foundation::input_transform::{CapsTapHold, KeyMapping, KeyPreset, KeyboardMapping, TargetOverride};
    let (mut s, mut t) = pair();
    let mut settings = InputTransform::default();
    settings.windows.keyboard = KeyboardMapping {
        preset: KeyPreset::Custom,
        table: vec![KeyMapping {
            from: KeyCode::RIGHT_SHIFT,
            to: KeyCode::LEFT_CTRL,
        }],
        caps_tap_hold: Some(CapsTapHold {
            tap: KeyCode::LEFT_ALT,
            hold: KeyCode::CAPS_LOCK,
        }),
    };
    settings.overrides.push(TargetOverride {
        target: PC,
        reverse_wheel: None,
        keyboard: Some(KeyboardMapping {
            preset: KeyPreset::Custom,
            table: settings.windows.keyboard.table.clone(),
            caps_tap_hold: Some(CapsTapHold {
                tap: KeyCode::RIGHT_SHIFT,
                hold: KeyCode::ESCAPE,
            }),
        }),
    });
    s.set_input_transforms([(ME, settings.clone())].into());
    t.set_input_transforms([(ME, settings)].into());
    key(&mut s, KeyCode::CAPS_LOCK, true, 10);
    s.set_input_transforms(HashMap::new());
    key(&mut s, KeyCode::CAPS_LOCK, false, 20);
    assert_eq!(
        deliver(&mut s, &mut t),
        [(KeyCode::RIGHT_SHIFT, true), (KeyCode::RIGHT_SHIFT, false)]
    );
}

#[test]
fn caps_mac_feel_rechecks_setting_on_each_press_but_preserves_inflight_choice() {
    let (mut s, mut t) = pair();
    s.set_input_transforms(HashMap::new());
    t.set_input_transforms(HashMap::new());
    for enabled in [false, true, false] {
        s.set_mac_caps_switch(enabled);
        key(&mut s, KeyCode::CAPS_LOCK, true, 10);
        s.set_mac_caps_switch(!enabled);
        key(&mut s, KeyCode::CAPS_LOCK, false, 20);
        let code = if enabled {
            KeyCode::LEFT_SHIFT
        } else {
            KeyCode::CAPS_LOCK
        };
        assert_eq!(deliver(&mut s, &mut t), [(code, true), (code, false)]);
    }
    s.become_holder(2, None);
    s.set_mac_caps_switch(true);
    assert_eq!(key(&mut s, KeyCode::CAPS_LOCK, true, 30), Disposition::Pass);
    assert_eq!(key(&mut s, KeyCode::CAPS_LOCK, false, 40), Disposition::Pass);
    assert!(s.caps_press.is_none());
}

#[test]
fn caps_keyboard_local_and_game_mode_keep_their_existing_route() {
    let (mut s, mut t) = pair();
    let mut settings = s.cfg.settings.clone();
    settings.keyboard_mode = InputMode::Local;
    s.set_settings(settings.clone());
    assert_eq!(key(&mut s, KeyCode::CAPS_LOCK, true, 10), Disposition::Pass);
    assert_eq!(key(&mut s, KeyCode::CAPS_LOCK, false, 20), Disposition::Pass);
    assert!(deliver(&mut s, &mut t).is_empty());
    settings.keyboard_mode = InputMode::Shared;
    s.set_settings(settings);
    t.set_game_mode(GameMode::On);
    assert!(t.game_mode().active);
    key(&mut s, KeyCode::CAPS_LOCK, true, 30);
    key(&mut s, LETTER, true, 40);
    key(&mut s, KeyCode::CAPS_LOCK, false, 50);
    assert_eq!(
        deliver(&mut s, &mut t),
        [
            (KeyCode::LEFT_SHIFT, true),
            (LETTER, true),
            (KeyCode::LEFT_SHIFT, false)
        ]
    );
}

#[test]
fn caps_windows_local_custom_mapping_orders_letter_with_modifier() {
    let (mut s, _) = pair();
    s.cfg.os = Os::Windows;
    s.become_holder(2, None);
    s.drain_outputs();
    for (code, pressed, now) in [
        (KeyCode::CAPS_LOCK, true, 10),
        (LETTER, true, 20),
        (KeyCode::CAPS_LOCK, false, 30),
        (LETTER, false, 40),
    ] {
        assert_eq!(key(&mut s, code, pressed, now), Disposition::Swallow);
    }
    let output: Vec<_> = s
        .drain_outputs()
        .into_iter()
        .filter_map(|o| match o {
            Output::Inject(InjectEvent::Key { code, pressed }) => Some((code, pressed)),
            _ => None,
        })
        .collect();
    assert_eq!(
        output,
        [
            (KeyCode::LEFT_SHIFT, true),
            (LETTER, true),
            (KeyCode::LEFT_SHIFT, false),
            (LETTER, false)
        ]
    );
    assert!(s.key_refs.is_empty());
}

#[test]
fn caps_windows_local_inflight_mapping_survives_settings_edit_without_letter_overtaking() {
    let (mut s, _) = pair();
    s.cfg.os = Os::Windows;
    s.become_holder(2, None);
    s.drain_outputs();
    key(&mut s, KeyCode::CAPS_LOCK, true, 10);
    s.set_input_transforms([(ME, InputTransform::default())].into());
    assert_eq!(key(&mut s, LETTER, true, 20), Disposition::Swallow);
    let output: Vec<_> = s
        .drain_outputs()
        .into_iter()
        .filter_map(|o| match o {
            Output::Inject(InjectEvent::Key { code, pressed }) => Some((code, pressed)),
            _ => None,
        })
        .collect();
    assert_eq!(output, [(KeyCode::LEFT_SHIFT, true), (LETTER, true)]);
}

#[test]
fn caps_custom_remote_mapping_works_in_all_os_directions() {
    for source_os in [Os::MacOS, Os::Windows] {
        for target_os in [Os::MacOS, Os::Windows] {
            let (mut s, mut t) = pair();
            s.cfg.os = source_os;
            t.cfg.os = target_os;
            s.peers.get_mut(&PC).unwrap().os = target_os;
            let settings = s.input_transforms.get_mut(&ME).unwrap();
            settings.macos = settings.windows.clone();
            key(&mut s, KeyCode::CAPS_LOCK, true, 10);
            key(&mut s, KeyCode::CAPS_LOCK, false, 20);
            assert_eq!(
                deliver(&mut s, &mut t),
                [(KeyCode::LEFT_SHIFT, true), (KeyCode::LEFT_SHIFT, false)]
            );
        }
    }
}

#[test]
fn caps_focus_handoff_between_remote_targets_releases_only_the_original_target() {
    let other = DeviceId([3; 32]);
    let (mut s, mut t) = pair();
    s.test_connect(other, 0);
    key(&mut s, KeyCode::CAPS_LOCK, true, 10);
    s.tick(500_010);
    assert_eq!(deliver(&mut s, &mut t), [(KeyCode::CAPS_LOCK, true)]);
    t.holder = other;
    s.become_source(2, other);
    let outputs = s.drain_outputs();
    for output in outputs {
        if let Output::Send {
            to,
            msg: Message::Input(input),
            ..
        } = output
        {
            assert_eq!(to, PC);
            t.on_input(ME, ME, 0, input, 600_000);
        }
    }
    let outputs = t.drain_outputs();
    assert!(outputs.iter().any(|o| matches!(
        o,
        Output::Inject(InjectEvent::Key {
            code: KeyCode::CAPS_LOCK,
            pressed: false
        })
    )));
    assert!(!outputs.iter().any(|o| matches!(
        o,
        Output::Send {
            msg: Message::Input(_),
            ..
        }
    )));
    key(&mut s, KeyCode::CAPS_LOCK, false, 700_000);
    assert!(s.caps_press.is_none());
}
