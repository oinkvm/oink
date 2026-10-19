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
