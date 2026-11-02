use super::*;

const ME: DeviceId = DeviceId([1; 32]);
const MAC: DeviceId = DeviceId([2; 32]);
const OTHER: DeviceId = DeviceId([3; 32]);
const CONTROLS: [KeyCode; 5] = [
    KeyCode::new(KeyCode::PAGE_CONSUMER, 0x6f),
    KeyCode::new(KeyCode::PAGE_CONSUMER, 0x70),
    KeyCode::new(KeyCode::PAGE_CONSUMER, 0x79),
    KeyCode::new(KeyCode::PAGE_CONSUMER, 0x7a),
    KeyCode::new(KeyCode::PAGE_CONSUMER, 0x7c),
];

fn router() -> Router {
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: ME,
                nonce: [1; 16],
            },
            "source",
            Os::MacOS,
        ),
        Arc::new(crate::SimpleLayout::new()),
    );
    for peer in [MAC, OTHER] {
        r.test_connect(peer, 0);
        r.peers.get_mut(&peer).unwrap().os = Os::MacOS;
    }
    r.holder = MAC;
    r.fresh_join = false;
    r.drain_outputs();
    r
}

fn key(r: &mut Router, code: KeyCode, pressed: bool) -> Disposition {
    r.on_local_event(
        &CapturedEvent {
            physical: true,
            source: SourceKind::Keyboard,
            time_us: 1,
            cursor: None,
            event: InputEvent::Key { code, pressed },
        },
        1,
    )
}

fn sent(r: &mut Router, code: KeyCode) -> Vec<(DeviceId, InputMsg)> {
    r.drain_outputs()
        .into_iter()
        .filter_map(|o| match o {
            Output::Send {
                to,
                msg: Message::Input(msg),
                ..
            } if matches!(msg, InputMsg::Key { code: c, .. } | InputMsg::KeyRepeat { code: c, .. } if c == code) => {
                Some((to, msg))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn command_controls_route_identically_to_mac_and_windows() {
    for code in CONTROLS {
        for (cmd, os) in [KeyCode::LEFT_GUI, KeyCode::RIGHT_GUI]
            .into_iter()
            .flat_map(|cmd| [Os::MacOS, Os::Windows].map(|os| (cmd, os)))
        {
            let mut r = router();
            r.peers.get_mut(&MAC).unwrap().os = os;
            key(&mut r, cmd, true);
            r.drain_outputs();
            for pressed in [true, true, true, false] {
                assert_eq!(key(&mut r, code, pressed), Disposition::Swallow);
            }
            assert_eq!(
                sent(&mut r, code),
                [
                    (MAC, InputMsg::Key { code, pressed: true }),
                    (MAC, InputMsg::KeyRepeat { code, n: 1 }),
                    (MAC, InputMsg::KeyRepeat { code, n: 2 }),
                    (MAC, InputMsg::Key { code, pressed: false }),
                ]
            );
            assert!(!r.keys.contains_key(&code));
        }
    }
}

#[test]
fn brightness_and_backlight_without_command_or_remote_focus_stay_local() {
    for code in CONTROLS {
        for scenario in 0..4 {
            let mut r = router();
            match scenario {
                0 => {} // No Command, remote Mac.
                1 => r.holder = ME,
                2 => r.cfg.settings.keyboard_mode = oink_foundation::input::InputMode::Local,
                3 => r.cfg.os = Os::Windows,
                _ => unreachable!(),
            }
            if scenario != 0 {
                key(&mut r, KeyCode::LEFT_GUI, true);
            }
            if scenario == 3 {
                // The new gesture is a Mac-source policy, not a generic GUI shortcut.
                assert_eq!(r.brightness_event(&InputEvent::Key { code, pressed: true }), None);
                continue;
            }
            r.drain_outputs();
            for pressed in [true, true, false] {
                assert_eq!(key(&mut r, code, pressed), Disposition::Pass);
                assert!(sent(&mut r, code).is_empty());
                assert!(r.local_rewrite().is_none());
            }
        }
    }
}

#[test]
fn forwarded_controls_pair_and_repeat_after_command_release_and_focus_changes() {
    for code in CONTROLS {
        for (focus, release_command) in [MAC, ME, OTHER]
            .into_iter()
            .flat_map(|focus| [false, true].map(|release| (focus, release)))
        {
            let mut r = router();
            key(&mut r, KeyCode::RIGHT_GUI, true);
            key(&mut r, code, true);
            sent(&mut r, code);
            if release_command {
                key(&mut r, KeyCode::RIGHT_GUI, false);
            }
            r.holder = focus;
            r.retarget_modifiers();
            assert_eq!(key(&mut r, code, true), Disposition::Swallow);
            assert_eq!(key(&mut r, code, false), Disposition::Swallow);
            assert_eq!(
                sent(&mut r, code),
                [
                    (MAC, InputMsg::KeyRepeat { code, n: 1 }),
                    (MAC, InputMsg::Key { code, pressed: false }),
                ]
            );
        }
    }
}

#[test]
fn native_controls_stay_local_when_command_or_focus_changes_after_down() {
    for code in CONTROLS {
        let mut r = router();
        r.holder = ME;
        assert_eq!(key(&mut r, code, true), Disposition::Pass);
        key(&mut r, KeyCode::LEFT_GUI, true);
        r.holder = MAC;
        r.retarget_modifiers();
        assert_eq!(key(&mut r, code, true), Disposition::Pass);
        assert_eq!(key(&mut r, code, false), Disposition::Pass);
        assert!(sent(&mut r, code).is_empty());
        assert_eq!(key(&mut r, code, true), Disposition::Swallow);
        assert_eq!(key(&mut r, code, false), Disposition::Swallow);
        assert_eq!(
            sent(&mut r, code),
            [
                (MAC, InputMsg::Key { code, pressed: true }),
                (MAC, InputMsg::Key { code, pressed: false }),
            ]
        );
    }
}

#[test]
fn mac_target_injects_control_repeats_and_up_while_command_remains_held() {
    for code in CONTROLS {
        let mut r = router();
        r.holder = ME;
        r.target_key(MAC, KeyCode::LEFT_GUI, true);
        r.drain_outputs();
        r.target_key(MAC, code, true);
        r.remote_repeat(MAC, 0, code, 1, InputMsg::KeyRepeat { code, n: 1 });
        r.remote_repeat(MAC, 0, code, 1, InputMsg::KeyRepeat { code, n: 1 });
        r.target_key(MAC, code, false);
        let injected: Vec<_> = r
            .drain_outputs()
            .into_iter()
            .filter_map(|o| match o {
                Output::Inject(event) => Some(event),
                _ => None,
            })
            .collect();
        assert_eq!(
            injected,
            [
                InjectEvent::Key { code, pressed: true },
                InjectEvent::Key { code, pressed: true },
                InjectEvent::Key { code, pressed: false },
            ]
        );
        assert!(r.key_refs.contains_key(&KeyCode::LEFT_GUI));
    }
}

#[test]
fn windows_target_drops_control_down_repeat_and_up_without_injection_or_held_state() {
    for code in CONTROLS {
        let mut r = router();
        r.cfg.os = Os::Windows;
        r.holder = ME;
        r.target_key(MAC, KeyCode::LEFT_GUI, true);
        r.drain_outputs();
        r.test_message(MAC, Message::Input(InputMsg::Key { code, pressed: true }), 1);
        r.test_message(MAC, Message::Input(InputMsg::KeyRepeat { code, n: 1 }), 1);
        r.test_message(MAC, Message::Input(InputMsg::Key { code, pressed: false }), 1);
        assert!(r.drain_outputs().is_empty());
        assert!(!r.held.get(&MAC).is_some_and(|h| h.keys.contains_key(&code)));
        assert!(!r.key_refs.contains_key(&code));
    }
}
