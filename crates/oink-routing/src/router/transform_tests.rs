use super::*;
use oink_foundation::input::{CapturedEvent, LocalRewrite};
use oink_foundation::input_transform::{InputTransform, KeyMapping, KeyPreset, KeyboardMapping, TargetOverride};

const ME: DeviceId = DeviceId([1; 32]);
const SOURCE: DeviceId = DeviceId([2; 32]);
fn router(os: Os) -> Router {
    Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: ME,
                nonce: [ME.0[0]; 16],
            },
            "local",
            os,
        ),
        Arc::new(crate::SimpleLayout::new()),
    )
}
fn custom(from: KeyCode, to: KeyCode) -> KeyboardMapping {
    KeyboardMapping {
        saved_custom: None,
        preset: KeyPreset::Custom,
        caps_tap_hold: None,
        table: vec![KeyMapping { from, to }],
    }
}
fn keys(r: &mut Router) -> Vec<(KeyCode, bool)> {
    r.drain_outputs()
        .into_iter()
        .filter_map(|o| {
            if let Output::Inject(InjectEvent::Key { code, pressed }) = o {
                Some((code, pressed))
            } else {
                None
            }
        })
        .collect()
}
fn physical(r: &mut Router, code: KeyCode, pressed: bool) -> Disposition {
    // Pure FakeOS input, never posted to an OS or a native diagnostics recorder.
    r.on_local_event(
        &CapturedEvent {
            time_us: 1,
            physical: true,
            source: SourceKind::Keyboard,
            cursor: None,
            event: InputEvent::Key { code, pressed },
        },
        1,
    )
}

#[test]
fn peer_without_an_input_item_uses_source_os_defaults() {
    for target_os in [Os::MacOS, Os::Windows] {
        for source_os in [Os::MacOS, Os::Windows] {
            let mut r = router(target_os);
            r.test_connect(SOURCE, 0);
            r.peers.get_mut(&SOURCE).unwrap().os = source_os;
            // A synced group may contain members without explicit source items.
            r.set_input_transforms(HashMap::new());
            assert!(!r.resolved_transform(SOURCE).reverse_wheel);
            r.target_key(SOURCE, KeyCode::RIGHT_GUI, true);
            r.target_key(SOURCE, KeyCode::RIGHT_GUI, false);
            let output = if source_os == target_os {
                KeyCode::RIGHT_GUI
            } else {
                KeyCode::RIGHT_CTRL
            };
            assert_eq!(keys(&mut r), [(output, true), (output, false)]);
        }
    }
}

#[test]
fn per_source_self_override_and_down_mapping_cover_repeat_release_and_config_edit() {
    let mut r = router(Os::Windows);
    let a = KeyCode::keyboard(4);
    let b = KeyCode::keyboard(5);
    let mut s = InputTransform::initial("macos");
    s.overrides.push(TargetOverride {
        target: ME,
        reverse_wheel: None,
        keyboard: Some(custom(a, b)),
    });
    r.set_input_transforms([(SOURCE, s)].into());
    r.target_key(SOURCE, a, true);
    assert_eq!(keys(&mut r), [(b, true)]);
    r.set_input_transforms([(SOURCE, InputTransform::default())].into());
    r.remote_repeat(SOURCE, 0, a, 1, InputMsg::KeyRepeat { code: a, n: 1 });
    r.target_key(SOURCE, a, false);
    assert_eq!(keys(&mut r), [(b, true), (b, false)]);
}

#[test]
fn focus_targets_choose_once_and_release_their_own_side_specific_mapping() {
    let mut first = router(Os::MacOS);
    let mut cfg = RouterConfig::new(
        oink_foundation::session::Boot {
            device_id: DeviceId([3; 32]),
            nonce: [DeviceId([3; 32]).0[0]; 16],
        },
        "target",
        Os::Windows,
    );
    cfg.os = Os::Windows;
    let mut second = Router::new(cfg, Arc::new(crate::SimpleLayout::new()));
    let mut s = InputTransform::initial("windows");
    s.macos.keyboard = custom(KeyCode::RIGHT_CTRL, KeyCode::RIGHT_ALT);
    s.windows.keyboard = custom(KeyCode::RIGHT_CTRL, KeyCode::RIGHT_GUI);
    first.set_input_transforms([(SOURCE, s.clone())].into());
    second.set_input_transforms([(SOURCE, s)].into());
    // Focus snapshots and forwarding keep the physical identity on both paths.
    first.target_key(SOURCE, KeyCode::RIGHT_CTRL, true);
    second.target_key(SOURCE, KeyCode::RIGHT_CTRL, true);
    first.target_key(SOURCE, KeyCode::RIGHT_CTRL, false);
    second.target_key(SOURCE, KeyCode::RIGHT_CTRL, false);
    assert_eq!(
        keys(&mut first),
        [(KeyCode::RIGHT_ALT, true), (KeyCode::RIGHT_ALT, false)]
    );
    assert_eq!(
        keys(&mut second),
        [(KeyCode::RIGHT_GUI, true), (KeyCode::RIGHT_GUI, false)]
    );
}

#[test]
fn windows_local_sequence_swallow_includes_identity_keys_and_releases_after_focus_moves() {
    let mut r = router(Os::Windows);
    let mut s = InputTransform::initial("windows");
    s.windows.keyboard = custom(KeyCode::LEFT_CTRL, KeyCode::LEFT_ALT);
    r.set_input_transforms([(ME, s)].into());
    let a = KeyCode::keyboard(4);
    for (code, pressed) in [(KeyCode::LEFT_CTRL, true), (a, true), (a, true)] {
        assert_eq!(physical(&mut r, code, pressed), Disposition::Swallow);
    }
    assert_eq!(keys(&mut r), [(KeyCode::LEFT_ALT, true), (a, true), (a, true)]);
    r.holder = SOURCE;
    r.test_connect(SOURCE, 0);
    r.retarget_modifiers();
    r.drain_outputs();
    assert_eq!(physical(&mut r, a, false), Disposition::Swallow);
    assert_eq!(physical(&mut r, KeyCode::LEFT_CTRL, false), Disposition::Swallow);
    assert_eq!(keys(&mut r), [(a, false), (KeyCode::LEFT_ALT, false)]);
    assert!(r.key_refs.is_empty());
}

#[test]
fn mac_local_rewrite_pairs_keys_and_keeps_shortcuts_physical() {
    let mut r = router(Os::MacOS);
    let mut s = InputTransform::initial("macos");
    s.macos.keyboard = custom(KeyCode::LEFT_CTRL, KeyCode::LEFT_GUI);
    r.set_input_transforms([(ME, s)].into());
    assert_eq!(physical(&mut r, KeyCode::LEFT_CTRL, true), Disposition::Pass);
    assert_eq!(
        r.local_rewrite(),
        Some(LocalRewrite::Key {
            code: KeyCode::LEFT_GUI,
            pressed: true
        })
    );
    assert!(r.keys.contains_key(&KeyCode::LEFT_CTRL));
    r.holder = SOURCE;
    r.test_connect(SOURCE, 0);
    r.retarget_modifiers();
    let sent = r.drain_outputs();
    assert!(sent.iter().any(|o| matches!(
        o,
        Output::Send {
            msg: Message::Input(InputMsg::Key {
                code: KeyCode::LEFT_CTRL,
                pressed: true
            }),
            ..
        }
    )));
    assert_eq!(physical(&mut r, KeyCode::LEFT_CTRL, false), Disposition::Pass);
    assert_eq!(
        r.local_rewrite(),
        Some(LocalRewrite::Key {
            code: KeyCode::LEFT_GUI,
            pressed: false
        })
    );
    assert!(r.local_mapped.is_empty());
}

#[test]
fn wheel_both_axes_reverse_but_continuous_input_never_does() {
    for os in [Os::MacOS, Os::Windows] {
        let mut r = router(os);
        let mut s = InputTransform::initial("macos");
        s.macos.reverse_wheel = true;
        s.windows.reverse_wheel = true;
        r.set_input_transforms([(SOURCE, s.clone()), (ME, s)].into());
        for (source, phase, sign) in [
            (SourceKind::Mouse, ScrollPhase::None, -1.),
            (SourceKind::Touchpad, ScrollPhase::None, 1.),
            (SourceKind::Mouse, ScrollPhase::Changed, 1.),
        ] {
            r.inject_scroll(SOURCE, source, 2., 3., ScrollAmount::Lines, phase);
            assert!(r.drain_outputs().iter().any(|o| matches!(o, Output::Inject(InjectEvent::Scroll { dx, dy, .. }) if dx.signum() == sign && dy.signum() == sign)));
        }
        for source in [SourceKind::Mouse, SourceKind::Touchpad] {
            let event = CapturedEvent {
                time_us: 1,
                physical: true,
                source,
                cursor: None,
                event: InputEvent::Scroll {
                    dx: 2.,
                    dy: 3.,
                    unit: ScrollUnit::Line,
                    phase: ScrollPhase::None,
                },
            };
            let inverted = source == SourceKind::Mouse;
            assert_eq!(
                r.on_local_event(&event, 1),
                if inverted && os == Os::Windows {
                    Disposition::Swallow
                } else {
                    Disposition::Pass
                }
            );
            assert_eq!(
                r.local_rewrite(),
                (inverted && os == Os::MacOS).then_some(LocalRewrite::ReverseWheel)
            );
            let output = r.drain_outputs();
            assert_eq!(
                output.iter().any(
                    |o| matches!(o, Output::Inject(InjectEvent::Scroll { dx, dy, .. }) if *dx == -2. && *dy == -3.)
                ),
                inverted && os == Os::Windows
            );
        }
    }
}

#[test]
fn mac_identity_key_and_custom_key_share_one_native_press() {
    let mut r = router(Os::MacOS);
    let a = KeyCode::keyboard(4);
    let b = KeyCode::keyboard(5);
    let mut s = InputTransform::initial("macos");
    s.macos.keyboard = custom(a, b);
    r.set_input_transforms([(ME, s)].into());
    assert_eq!(physical(&mut r, b, true), Disposition::Pass);
    assert_eq!(physical(&mut r, a, true), Disposition::Swallow);
    assert_eq!(physical(&mut r, b, false), Disposition::Swallow);
    assert_eq!(physical(&mut r, a, false), Disposition::Pass);
    assert_eq!(
        r.local_rewrite(),
        Some(LocalRewrite::Key {
            code: b,
            pressed: false
        })
    );
}

#[test]
fn media_keys_use_the_same_table_and_windows_local_injection_sequence() {
    let a = KeyCode::keyboard(4);
    let play = KeyCode::new(KeyCode::PAGE_CONSUMER, 0xcd);
    let mut r = router(Os::Windows);
    let mut s = InputTransform::initial("windows");
    s.windows.keyboard = custom(play, a);
    r.set_input_transforms([(ME, s)].into());
    assert_eq!(physical(&mut r, play, true), Disposition::Swallow);
    assert_eq!(physical(&mut r, play, false), Disposition::Swallow);
    assert_eq!(keys(&mut r), [(a, true), (a, false)]);
    let mut s = InputTransform::initial("macos");
    s.windows.keyboard = custom(a, play);
    r.set_input_transforms([(SOURCE, s)].into());
    r.target_key(SOURCE, a, true);
    r.target_key(SOURCE, a, false);
    assert_eq!(keys(&mut r), [(play, true), (play, false)]);
}

#[test]
fn return_to_mac_injected_modifier_retains_its_physical_to_logical_flag_mapping() {
    let mut r = router(Os::MacOS);
    let mut s = InputTransform::initial("macos");
    s.macos.keyboard = custom(KeyCode::RIGHT_CTRL, KeyCode::RIGHT_ALT);
    r.set_input_transforms([(ME, s)].into());
    r.target_key(ME, KeyCode::RIGHT_CTRL, true);
    let (mapping, injected) = r.local_modifier_mapping();
    assert!(mapping.contains(&(KeyCode::RIGHT_CTRL, KeyCode::RIGHT_ALT)));
    assert!(injected.contains(&KeyCode::RIGHT_ALT));
    r.target_key(ME, KeyCode::RIGHT_CTRL, false);
    assert_eq!(keys(&mut r), [(KeyCode::RIGHT_ALT, true), (KeyCode::RIGHT_ALT, false)]);
}

#[test]
fn mac_local_unsupported_entries_pass_unchanged_while_other_entries_and_remote_targets_work() {
    let mut r = router(Os::MacOS);
    let a = KeyCode::keyboard(4);
    let b = KeyCode::keyboard(5);
    let play = KeyCode::new(KeyCode::PAGE_CONSUMER, 0xcd);
    let function = KeyCode::new(0xff, 3);
    let mut s = InputTransform::initial("macos");
    s.macos.keyboard = KeyboardMapping {
        saved_custom: None,
        preset: KeyPreset::Custom,
        caps_tap_hold: None,
        table: vec![
            KeyMapping {
                from: KeyCode::CAPS_LOCK,
                to: b,
            },
            KeyMapping { from: play, to: a },
            KeyMapping {
                from: function,
                to: KeyCode::RIGHT_SHIFT,
            },
            KeyMapping { from: a, to: b },
        ],
    };
    r.set_input_transforms([(ME, s.clone()), (SOURCE, s)].into());
    for code in [KeyCode::CAPS_LOCK, play, function] {
        assert_eq!(physical(&mut r, code, true), Disposition::Pass);
        assert_eq!(r.local_rewrite(), None);
        assert_eq!(physical(&mut r, code, false), Disposition::Pass);
        assert_eq!(r.local_rewrite(), None);
    }
    assert_eq!(physical(&mut r, a, true), Disposition::Pass);
    assert_eq!(r.local_rewrite(), Some(LocalRewrite::Key { code: b, pressed: true }));
    physical(&mut r, a, false);
    assert_eq!(
        r.local_rewrite(),
        Some(LocalRewrite::Key {
            code: b,
            pressed: false
        })
    );
    r.drain_outputs();
    for code in [KeyCode::CAPS_LOCK, play, function] {
        r.target_key(SOURCE, code, true);
        r.target_key(SOURCE, code, false);
    }
    assert_eq!(
        keys(&mut r),
        [
            (b, true),
            (b, false),
            (a, true),
            (a, false),
            (KeyCode::RIGHT_SHIFT, true),
            (KeyCode::RIGHT_SHIFT, false)
        ]
    );
}

#[test]
fn mac_entry_with_an_unsupported_local_output_keeps_its_source_physical_key() {
    let mut r = router(Os::MacOS);
    let a = KeyCode::keyboard(4);
    for output in [
        KeyCode::CAPS_LOCK,
        KeyCode::new(KeyCode::PAGE_CONSUMER, 0xe9),
        KeyCode::new(0xff, 3),
    ] {
        let mut s = InputTransform::initial("macos");
        s.macos.keyboard = custom(a, output);
        r.set_input_transforms([(ME, s)].into());
        assert_eq!(physical(&mut r, a, true), Disposition::Pass);
        assert_eq!(r.local_rewrite(), None);
        assert_eq!(physical(&mut r, a, false), Disposition::Pass);
        assert_eq!(r.local_rewrite(), None);
    }
}

#[test]
fn none_drops_down_repeat_up_and_remembers_the_press_mapping() {
    let mut r = router(Os::Windows);
    let a = KeyCode::keyboard(4);
    let mut s = InputTransform::initial("macos");
    s.windows.keyboard = custom(a, KeyCode(0));
    r.set_input_transforms([(SOURCE, s)].into());
    r.target_key(SOURCE, a, true);
    r.set_input_transforms(HashMap::new());
    r.remote_repeat(SOURCE, 0, a, 1, InputMsg::KeyRepeat { code: a, n: 1 });
    r.target_key(SOURCE, a, false);
    assert!(keys(&mut r).is_empty());
    assert!(r.key_refs.is_empty());
}

#[test]
fn fn_maps_to_every_windows_modifier_and_none() {
    for output in (0xe0..=0xe7).map(KeyCode::keyboard).chain([KeyCode(0)]) {
        let mut r = router(Os::Windows);
        let mut s = InputTransform::initial("macos");
        s.windows.keyboard = custom(KeyCode::new(0xff, 3), output);
        assert!(s.validate().is_ok());
        r.set_input_transforms([(SOURCE, s)].into());
        r.target_key(SOURCE, KeyCode::new(0xff, 3), true);
        r.target_key(SOURCE, KeyCode::new(0xff, 3), false);
        let expected = if output == KeyCode(0) {
            vec![]
        } else {
            vec![(output, true), (output, false)]
        };
        assert_eq!(keys(&mut r), expected);
        assert!(r.key_refs.is_empty());
    }
}

#[test]
fn enabled_fn_follows_focus_and_releases_previous_target_on_departure() {
    let mut r = router(Os::MacOS);
    r.test_connect(SOURCE, 0);
    r.peers.get_mut(&SOURCE).unwrap().os = Os::Windows;
    let mut s = InputTransform::initial("macos");
    s.windows.keyboard = custom(KeyCode::new(0xff, 3), KeyCode::RIGHT_CTRL);
    r.set_input_transforms([(ME, s)].into());
    physical(&mut r, KeyCode::new(0xff, 3), true);
    r.drain_outputs();
    r.holder = SOURCE;
    r.retarget_modifiers();
    assert!(r.drain_outputs().iter().any(|o| matches!(o, Output::Send {to, msg: Message::Input(InputMsg::Key {code,pressed:true}),..} if *to == SOURCE && *code == KeyCode::new(0xff,3))));
    r.holder = ME;
    r.retarget_modifiers();
    assert!(r.drain_outputs().iter().any(|o| matches!(o, Output::Send {to, msg: Message::Input(InputMsg::Key {code,pressed:false}),..} if *to == SOURCE && *code == KeyCode::new(0xff,3))));
    physical(&mut r, KeyCode::new(0xff, 3), false);
    assert!(r.key_refs.is_empty());
}

#[test]
fn disabled_fn_keeps_its_existing_capture_and_focus_behavior() {
    let mut r = router(Os::MacOS);
    r.test_connect(SOURCE, 0);
    assert_eq!(physical(&mut r, KeyCode::new(0xff, 3), true), Disposition::Pass);
    r.drain_outputs();
    r.holder = SOURCE;
    r.retarget_modifiers();
    assert!(!r.drain_outputs().iter().any(|o| matches!(o, Output::Send { msg: Message::Input(InputMsg::Key {code,..}),..} if *code == KeyCode::new(0xff,3))));
}

#[test]
fn none_is_dropped_at_source_but_source_shortcuts_and_escape_hold_still_work() {
    use crate::hotkey::{Chord, Mods};
    let mut r = router(Os::Windows);
    r.test_connect(SOURCE, 0);
    r.peers.get_mut(&SOURCE).unwrap().os = Os::Windows;
    r.holder = SOURCE;
    let a = KeyCode::keyboard(4);
    let mut s = InputTransform::initial("windows");
    s.windows.keyboard = KeyboardMapping {
        saved_custom: None,
        preset: KeyPreset::Custom,
        caps_tap_hold: None,
        table: [KeyCode::LEFT_CTRL, a, KeyCode::ESCAPE]
            .map(|from| KeyMapping {
                from,
                to: KeyCode::NONE,
            })
            .to_vec(),
    };
    r.set_input_transforms([(ME, s)].into());
    r.set_hotkeys(vec![HotkeyBinding {
        chord: Chord {
            mods: Mods::CTRL,
            key: a,
        },
        action: "home".into(),
    }]);
    assert_eq!(physical(&mut r, KeyCode::LEFT_CTRL, true), Disposition::Swallow);
    assert_eq!(physical(&mut r, a, true), Disposition::Swallow);
    let out = r.drain_outputs();
    assert!(
        out.iter()
            .any(|o| matches!(o,Output::Status(StatusEvent::Hotkey {action}) if action=="home"))
    );
    assert!(!out.iter().any(|o| matches!(
        o,
        Output::Send {
            msg: Message::Input(InputMsg::Key { .. }),
            ..
        }
    )));
    physical(&mut r, a, false);
    physical(&mut r, KeyCode::LEFT_CTRL, false);
    r.drain_outputs();
    physical(&mut r, KeyCode::ESCAPE, true);
    r.tick(1 + r.emergency_hold_us());
    assert_eq!(r.emergency(), Some(ME));
}

#[test]
fn none_mac_local_edges_are_swallowed_without_a_native_rewrite() {
    let mut r = router(Os::MacOS);
    let mut s = InputTransform::initial("macos");
    s.macos.keyboard = custom(KeyCode::LEFT_CTRL, KeyCode::NONE);
    r.set_input_transforms([(ME, s)].into());
    for pressed in [true, false] {
        assert_eq!(physical(&mut r, KeyCode::LEFT_CTRL, pressed), Disposition::Swallow);
        assert_eq!(r.local_rewrite(), None);
    }
    assert!(keys(&mut r).is_empty());
}

#[test]
fn changing_fn_toggle_during_a_press_still_releases_the_departed_target() {
    let mut r = router(Os::MacOS);
    r.test_connect(SOURCE, 0);
    r.peers.get_mut(&SOURCE).unwrap().os = Os::Windows;
    r.holder = SOURCE;
    let mut mapping = InputTransform::initial("macos");
    mapping.windows.keyboard = custom(KeyCode::APPLE_FN, KeyCode::LEFT_CTRL);
    r.set_input_transforms([(ME, mapping)].into());
    physical(&mut r, KeyCode::APPLE_FN, true);
    r.drain_outputs();
    r.set_input_transforms([(ME, InputTransform::initial("macos"))].into());
    r.holder = ME;
    r.retarget_modifiers();
    assert!(r.drain_outputs().iter().any(|o|matches!(o,Output::Send{to,msg:Message::Input(InputMsg::Key{code:KeyCode::APPLE_FN,pressed:false}),..} if *to==SOURCE)));
    physical(&mut r, KeyCode::APPLE_FN, false);
    assert!(r.key_refs.is_empty());
}

fn fn_row_pair(output: KeyCode) -> (Router, Router) {
    let mut source = router(Os::MacOS);
    source.cfg.capture_reports_key_repeat = true;
    source.test_connect(SOURCE, 0);
    source.peers.get_mut(&SOURCE).unwrap().os = Os::Windows;
    source.holder = SOURCE;
    let mut target = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: SOURCE,
                nonce: [2; 16],
            },
            "target",
            Os::Windows,
        ),
        Arc::new(crate::SimpleLayout::new()),
    );
    target.test_connect(ME, 0);
    target.peers.get_mut(&ME).unwrap().os = Os::MacOS;
    target.epoch = 1;
    target.focus_scope = Some(target.new_focus_scope());
    target.focus_joined = true;
    target.fresh_join = false;
    source.epoch = 1;
    source.claim = SOURCE;
    source.focus_scope = Some(source.new_focus_scope());
    source.focus_joined = true;
    source.fresh_join = false;
    let mut mapping = InputTransform::initial("macos");
    mapping.windows.keyboard = custom(KeyCode::APPLE_FN, output);
    source.set_input_transforms([(ME, mapping.clone())].into());
    target.set_input_transforms([(ME, mapping)].into());
    source.drain_outputs();
    target.drain_outputs();
    (source, target)
}

fn relay_keys(source: &mut Router, target: &mut Router) -> Vec<(KeyCode, bool)> {
    for output in source.drain_outputs() {
        if let Output::Send {
            to: SOURCE,
            msg: Message::Input(input),
            ..
        } = output
        {
            target.receive_message(ME, Message::Input(input), 1);
        }
    }
    keys(target)
}

fn row_codes() -> Vec<KeyCode> {
    (0x3a..=0x45)
        .map(KeyCode::keyboard)
        .chain((0x7f..=0x81).map(KeyCode::keyboard))
        .chain(
            [
                0x6f, 0x70, 0x79, 0x7a, 0x7c, 0xe2, 0xe9, 0xea, 0xcd, 0xb5, 0xb6, 0xb3, 0xb4,
            ]
            .map(|u| KeyCode::new(KeyCode::PAGE_CONSUMER, u)),
        )
        .collect()
}

#[test]
fn fn_row_fn_first_releases_each_modifier_before_the_row_and_stays_released() {
    for modifier in (0xe0..=0xe7).map(KeyCode::keyboard) {
        for row in row_codes() {
            let (mut source, mut target) = fn_row_pair(modifier);
            physical(&mut source, KeyCode::APPLE_FN, true);
            assert_eq!(relay_keys(&mut source, &mut target), [(modifier, true)]);
            physical(&mut source, row, true);
            let mut expected = vec![(modifier, false)];
            if !row.is_brightness() {
                expected.push((row, true));
            }
            assert_eq!(relay_keys(&mut source, &mut target), expected, "{row:?}");
            for code in [row, KeyCode::APPLE_FN, row, KeyCode::APPLE_FN] {
                physical(&mut source, code, true);
                assert!(!relay_keys(&mut source, &mut target).iter().any(|(k, _)| *k == modifier));
            }
            physical(&mut source, row, false);
            relay_keys(&mut source, &mut target);
            physical(&mut source, KeyCode::APPLE_FN, true);
            assert!(relay_keys(&mut source, &mut target).is_empty());
            physical(&mut source, KeyCode::APPLE_FN, false);
            assert!(relay_keys(&mut source, &mut target).is_empty());
            physical(&mut source, KeyCode::APPLE_FN, true);
            physical(&mut source, KeyCode::APPLE_FN, false);
            assert_eq!(
                relay_keys(&mut source, &mut target),
                [(modifier, true), (modifier, false)]
            );
            assert!(target.key_refs.is_empty());
        }
    }
}

#[test]
fn fn_row_row_first_and_both_release_orders_do_not_rearm_held_fn() {
    for row in row_codes() {
        for fn_released_first in [false, true] {
            let modifier = KeyCode::RIGHT_ALT;
            let (mut source, mut target) = fn_row_pair(modifier);
            physical(&mut source, row, true);
            relay_keys(&mut source, &mut target);
            physical(&mut source, KeyCode::APPLE_FN, true);
            assert!(relay_keys(&mut source, &mut target).is_empty(), "{row:?}");
            let release_order = if fn_released_first {
                [KeyCode::APPLE_FN, row]
            } else {
                [row, KeyCode::APPLE_FN]
            };
            physical(&mut source, release_order[0], false);
            relay_keys(&mut source, &mut target);
            // Repeated Fn or a new Fn press while the row is still held cannot rearm it.
            physical(&mut source, KeyCode::APPLE_FN, true);
            assert!(relay_keys(&mut source, &mut target).is_empty());
            physical(&mut source, release_order[1], false);
            relay_keys(&mut source, &mut target);
            physical(&mut source, KeyCode::APPLE_FN, false);
            assert!(relay_keys(&mut source, &mut target).is_empty());
            physical(&mut source, KeyCode::APPLE_FN, true);
            physical(&mut source, KeyCode::APPLE_FN, false);
            assert_eq!(
                relay_keys(&mut source, &mut target),
                [(modifier, true), (modifier, false)]
            );
        }
    }
}

#[test]
fn fn_row_cancellation_survives_crossings_without_releasing_another_device() {
    let modifier = KeyCode::RIGHT_CTRL;
    let (mut source, mut target) = fn_row_pair(modifier);
    let other = DeviceId([3; 32]);
    target.test_connect(other, 0);
    target.peers.get_mut(&other).unwrap().os = Os::MacOS;
    target
        .input_transforms
        .insert(other, target.input_transforms[&ME].clone());
    target.target_key(other, KeyCode::APPLE_FN, true);
    assert_eq!(keys(&mut target), [(modifier, true)]);
    physical(&mut source, KeyCode::APPLE_FN, true);
    assert!(relay_keys(&mut source, &mut target).is_empty());
    physical(&mut source, KeyCode::keyboard(0x3a), true);
    assert_eq!(relay_keys(&mut source, &mut target), [(KeyCode::keyboard(0x3a), true)]);
    source.holder = ME;
    source.retarget_modifiers();
    source.holder = SOURCE;
    source.retarget_modifiers();
    assert!(relay_keys(&mut source, &mut target).is_empty());
    target.target_key(other, KeyCode::APPLE_FN, false);
    assert_eq!(keys(&mut target), [(modifier, false)]);
    physical(&mut source, KeyCode::APPLE_FN, false);
    physical(&mut source, KeyCode::keyboard(0x3a), false);
    assert_eq!(relay_keys(&mut source, &mut target), [(KeyCode::keyboard(0x3a), false)]);
    assert!(target.key_refs.is_empty());
}

#[test]
fn fn_row_disabled_mapping_keeps_existing_fn_edges() {
    let (mut source, _) = fn_row_pair(KeyCode::LEFT_CTRL);
    source.set_input_transforms([(ME, InputTransform::initial("macos"))].into());
    let fn_edges = |r: &mut Router| {
        r.drain_outputs()
            .into_iter()
            .filter_map(|o| match o {
                Output::Send {
                    msg:
                        Message::Input(InputMsg::Key {
                            code: KeyCode::APPLE_FN,
                            pressed,
                        }),
                    ..
                } => Some(pressed),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    physical(&mut source, KeyCode::APPLE_FN, true);
    assert_eq!(fn_edges(&mut source), [true]);
    physical(&mut source, KeyCode::keyboard(0x3a), true);
    physical(&mut source, KeyCode::keyboard(0x3a), false);
    assert!(fn_edges(&mut source).is_empty());
    physical(&mut source, KeyCode::APPLE_FN, false);
    assert_eq!(fn_edges(&mut source), [false]);
}

#[test]
fn fn_row_unrelated_keys_remote_input_and_non_mac_sources_do_not_cancel() {
    let modifier = KeyCode::LEFT_CTRL;
    for code in [KeyCode::keyboard(4), KeyCode::keyboard(0x68), KeyCode::new(12, 0x123)] {
        let (mut source, mut target) = fn_row_pair(modifier);
        physical(&mut source, KeyCode::APPLE_FN, true);
        relay_keys(&mut source, &mut target);
        physical(&mut source, code, true);
        assert!(!relay_keys(&mut source, &mut target).contains(&(modifier, false)));
    }
    let (mut source, mut target) = fn_row_pair(modifier);
    physical(&mut source, KeyCode::APPLE_FN, true);
    relay_keys(&mut source, &mut target);
    // Remote injection never enters the physical source chord tracker.
    source.target_key(SOURCE, KeyCode::keyboard(0x3a), true);
    assert!(!relay_keys(&mut source, &mut target).contains(&(modifier, false)));
    source.cfg.os = Os::Windows;
    physical(&mut source, KeyCode::keyboard(0x3b), true);
    assert!(!relay_keys(&mut source, &mut target).contains(&(modifier, false)));
}

#[test]
fn fn_row_brightness_observation_keeps_every_transition_native() {
    let (mut source, mut target) = fn_row_pair(KeyCode::LEFT_CTRL);
    for code in row_codes().into_iter().filter(|code| code.is_brightness()) {
        for pressed in [true, true, false] {
            assert_eq!(physical(&mut source, code, pressed), Disposition::Pass);
            assert!(relay_keys(&mut source, &mut target).is_empty());
            assert!(!source.keys.contains_key(&code));
        }
    }
}

#[test]
fn fn_row_native_local_fn_edges_stay_native_without_rearming_on_crossing() {
    let (mut source, mut target) = fn_row_pair(KeyCode::LEFT_CTRL);
    source.holder = ME;
    assert_eq!(physical(&mut source, KeyCode::APPLE_FN, true), Disposition::Pass);
    assert_eq!(physical(&mut source, KeyCode::new(12, 0x6f), true), Disposition::Pass);
    assert!(source.local_rewrite().is_none());
    assert!(!source.drain_outputs().iter().any(|o| matches!(o, Output::Inject(_))));
    source.holder = SOURCE;
    source.retarget_modifiers();
    assert!(relay_keys(&mut source, &mut target).is_empty());
    assert_eq!(physical(&mut source, KeyCode::APPLE_FN, false), Disposition::Pass);
    assert!(relay_keys(&mut source, &mut target).is_empty());
}
