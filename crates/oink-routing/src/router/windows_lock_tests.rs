use super::*;

const ME: DeviceId = DeviceId([1; 32]);
const PEER: DeviceId = DeviceId([2; 32]);
const L: KeyCode = KeyCode::keyboard(0x0f);

fn router(source: Os, target: Os, remote: bool) -> Router {
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: ME,
                nonce: [1; 16],
            },
            "source",
            source,
        ),
        Arc::new(crate::SimpleLayout::new()),
    );
    r.test_connect(PEER, 0);
    r.peers.get_mut(&PEER).unwrap().os = target;
    r.holder = if remote { PEER } else { ME };
    r.fresh_join = false;
    r.set_hotkeys(vec![HotkeyBinding {
        chord: crate::Chord::parse(&["Meta", "Shift", "L"]).unwrap(),
        action: "lock_remote_windows".into(),
    }]);
    r.drain_outputs();
    r
}

fn key(r: &mut Router, code: KeyCode, pressed: bool) -> Disposition {
    r.on_local_event(
        &CapturedEvent {
            event: InputEvent::Key { code, pressed },
            physical: true,
            source: SourceKind::Keyboard,
            time_us: 1,
            cursor: None,
        },
        1,
    )
}

#[test]
fn remote_windows_lock_action_does_not_consume_local_mac_or_old_peer_chords() {
    for (source, target, remote, capable) in [
        (Os::Windows, Os::Windows, false, true),
        (Os::Windows, Os::MacOS, true, true),
        (Os::MacOS, Os::Windows, true, true),
        // An otherwise ready old peer still keeps ordinary key routing.
        (Os::Windows, Os::Windows, true, false),
    ] {
        let mut r = router(source, target, remote);
        ready(&mut r);
        r.set_peer_windows_lock(PEER, capable);
        key(&mut r, KeyCode::LEFT_GUI, true);
        key(&mut r, KeyCode::LEFT_SHIFT, true);
        r.drain_outputs();
        assert_eq!(key(&mut r, L, true), disposition_for(!remote));
        assert!(!r.out.iter().any(|o| matches!(o,
            Output::Status(StatusEvent::Hotkey { action }) if action == "lock_remote_windows")));
        if remote {
            assert!(r.out.iter().any(|o| matches!(o,
                Output::Send { to, msg: Message::Input(InputMsg::Key { code, pressed: true }), .. }
                    if *to == PEER && *code == L)));
        }
    }
}

#[test]
fn windows_origin_target_key_carries_lock_bypass_independent_of_pointer_focus() {
    let mut r = router(Os::Windows, Os::Windows, true);
    // The receiver's origin is PEER, even when its pointer focus disagrees.
    r.target_key(PEER, KeyCode::LEFT_GUI, true);
    r.drain_outputs();
    r.target_key(PEER, L, true);
    assert!(r.out.iter().any(|o| matches!(o,
        Output::Inject(InjectEvent::KeyWithoutLock {code,pressed:true}) if *code==L)));
}

fn ready(r: &mut Router) {
    r.session = [1; 16];
    r.epoch = 7;
    r.claim = r.holder;
    r.focus_joined = true;
    r.focus_scope = Some(FocusScope {
        members: vec![(ME, [1; 16]), (PEER, [2; 16])],
    });
    r.peers.get_mut(&PEER).unwrap().focus_ready = true;
    r.set_peer_windows_lock(PEER, true);
    r.drain_outputs();
}

#[test]
fn remote_windows_lock_is_once_per_press_and_releases_only_its_own_chord_first() {
    for win in [KeyCode::LEFT_GUI, KeyCode::RIGHT_GUI] {
        for shift in [KeyCode::LEFT_SHIFT, KeyCode::RIGHT_SHIFT] {
            let mut r = router(Os::Windows, Os::Windows, true);
            ready(&mut r);
            key(&mut r, win, true);
            key(&mut r, shift, true);
            r.drain_outputs();
            key(&mut r, L, true);
            let requests: Vec<_> = r
                .out
                .iter()
                .enumerate()
                .filter_map(|(i, o)| match o {
                    Output::Send {
                        to,
                        msg: Message::Control(ControlMsg::LockRemoteWindows(request)),
                        ..
                    } => {
                        assert_eq!(*to, PEER);
                        assert_eq!(request.source, r.cfg.boot);
                        assert_eq!(request.target, r.admitted_boots[&PEER]);
                        assert_eq!(request.epoch, 7);
                        Some(i)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(requests.len(), 1);
            for modifier in [win, shift] {
                assert!(r.out[..requests[0]].iter().any(|o| matches!(o,
                    Output::Send {msg: Message::Input(InputMsg::Key {code, pressed:false}),..} if *code==modifier)));
            }
            r.drain_outputs();
            key(&mut r, L, true);
            // A focus change cannot rebuild retired Win/Shift before physical UP.
            r.holder = ME;
            r.retarget_modifiers();
            key(&mut r, shift, false);
            key(&mut r, L, true);
            key(&mut r, L, false);
            key(&mut r, win, false);
            assert!(!r.out.iter().any(|o| matches!(
                o,
                Output::Send {
                    msg: Message::Input(_) | Message::Control(ControlMsg::LockRemoteWindows(_)),
                    ..
                } | Output::Inject(InjectEvent::Key { pressed: true, .. })
            )));
            assert!(r.keys.is_empty());
        }
    }
}

#[test]
fn remote_lock_retires_modifiers_pressed_locally_before_crossing() {
    let mut r = router(Os::Windows, Os::Windows, false);
    ready(&mut r);
    for code in [KeyCode::LEFT_GUI, KeyCode::LEFT_SHIFT] {
        assert_eq!(key(&mut r, code, true), Disposition::Pass);
    }
    r.holder = PEER;
    r.claim = PEER;
    r.retarget_modifiers();
    r.drain_outputs();
    assert_eq!(key(&mut r, L, true), Disposition::Swallow);
    let out = r.drain_outputs();
    let lock = out
        .iter()
        .position(|o| {
            matches!(
                o,
                Output::Send {
                    msg: Message::Control(ControlMsg::LockRemoteWindows(_)),
                    ..
                }
            )
        })
        .unwrap();
    for code in [KeyCode::LEFT_GUI, KeyCode::LEFT_SHIFT] {
        assert!(
            out[..lock].contains(&Output::Inject(InjectEvent::Key { code, pressed: false })),
            "the source OS must receive an UP for its previously passed DOWN"
        );
        assert_eq!(key(&mut r, code, false), Disposition::Swallow);
    }
    key(&mut r, L, false);
    assert!(r.keys.is_empty());
}

fn request(r: &Router) -> crate::control::WindowsLockRequest {
    crate::control::WindowsLockRequest {
        source: r.admitted_boots[&PEER],
        target: r.cfg.boot,
        epoch: r.epoch,
        id: 1,
    }
}
fn permits(r: &mut Router) -> Vec<WindowsLockPermit> {
    r.drain_outputs()
        .into_iter()
        .filter_map(|o| match o {
            Output::LockWindows(p) => Some(p),
            _ => None,
        })
        .collect()
}

#[test]
fn windows_lock_receiver_fences_target_source_epoch_capability_and_repeats() {
    for invalid in 0..9 {
        let mut r = router(Os::Windows, Os::Windows, false);
        ready(&mut r);
        let mut q = request(&r);
        match invalid {
            0 => q.target.nonce = [9; 16],
            1 => q.source.nonce = [9; 16],
            2 => q.epoch += 1,
            3 => q.id = 0,
            4 => r.set_peer_windows_lock(PEER, false),
            5 => r.peers.get_mut(&PEER).unwrap().os = Os::MacOS,
            6 => r.holder = PEER,
            7 => r.set_paused(true),
            8 => r.focus_scope = None,
            _ => unreachable!(),
        }
        r.receive_windows_lock(PEER, q);
        assert!(permits(&mut r).is_empty(), "invalid case {invalid}");
    }
    let mut r = router(Os::Windows, Os::Windows, false);
    ready(&mut r);
    let q = request(&r);
    r.receive_windows_lock(ME, q.clone());
    assert!(permits(&mut r).is_empty());
    r.receive_windows_lock(PEER, q.clone());
    r.receive_windows_lock(PEER, q.clone());
    let accepted = permits(&mut r);
    assert_eq!(accepted.len(), 1);
    // A fake native worker consumes the same one-shot authority as the real worker.
    assert!(accepted[0].take());
    assert!(!accepted[0].take());
    r.receive_windows_lock(PEER, q);
    assert!(permits(&mut r).is_empty());
}

#[test]
fn queued_lock_authority_is_revoked_by_focus_pause_or_source_detachment() {
    for change in 0..3 {
        let mut r = router(Os::Windows, Os::Windows, false);
        ready(&mut r);
        r.receive_windows_lock(PEER, request(&r));
        let permit = permits(&mut r).pop().unwrap();
        match change {
            0 => {
                r.epoch += 1;
                r.push_focus_status();
            }
            1 => r.set_paused(true),
            2 => r.detach_input(PEER),
            _ => unreachable!(),
        }
        assert!(!permit.take(), "revocation {change}");
    }
}

#[test]
fn win_l_remains_a_normal_forwarded_pair_on_the_windows_source() {
    let mut r = router(Os::Windows, Os::Windows, true);
    ready(&mut r);
    for (code, pressed) in [
        (KeyCode::LEFT_GUI, true),
        (L, true),
        (L, true),
        (L, false),
        (KeyCode::LEFT_GUI, false),
    ] {
        assert_eq!(key(&mut r, code, pressed), Disposition::Swallow);
    }
    let output = r.drain_outputs();
    assert!(!output.iter().any(|o| matches!(
        o,
        Output::Send {
            msg: Message::Control(ControlMsg::LockRemoteWindows(_)),
            ..
        } | Output::Send {
            msg: Message::Input(InputMsg::Key {
                code: KeyCode::MENU_MASK,
                ..
            }),
            ..
        }
    )));
    assert!(
        output
            .iter()
            .any(|o| matches!(o, Output::Send {msg: Message::Input(InputMsg::Key {code,pressed:true}),..} if *code==L))
    );
    assert!(
        output.iter().any(
            |o| matches!(o, Output::Send {msg: Message::Input(InputMsg::Key {code,pressed:false}),..} if *code==L)
        )
    );
}

#[test]
fn origin_lock_policy_survives_forwarding_and_mapping() {
    use oink_foundation::input_transform::{InputTransform, KeyMapping, KeyPreset, KeyboardMapping};
    for (origin_os, bypass) in [(Os::Windows, true), (Os::MacOS, false), (Os::Other, true)] {
        let mut r = router(Os::Windows, origin_os, false);
        ready(&mut r);
        let relay = DeviceId([3; 32]);
        r.test_connect(relay, 0);
        r.peers.get_mut(&relay).unwrap().os = if bypass { Os::MacOS } else { Os::Windows };
        let code = KeyCode::keyboard(4);
        let mut transform = InputTransform::initial(origin_os.as_str());
        transform.windows.keyboard = KeyboardMapping {
            preset: KeyPreset::Custom,
            table: vec![KeyMapping { from: code, to: L }],
            ..Default::default()
        };
        r.set_input_transforms([(PEER, transform)].into());
        r.drain_outputs();
        r.on_input(
            relay,
            relay,
            0,
            InputMsg::Forwarded {
                origin: r.admitted_boots[&PEER],
                hops: 1,
                msg: Box::new(InputMsg::Key { code, pressed: true }),
            },
            1,
        );
        assert!(r.out.iter().any(|o| matches!(o,Output::Inject(e) if
            *e == if bypass {InjectEvent::KeyWithoutLock{code:L,pressed:true}} else {InjectEvent::Key{code:L,pressed:true}})));
    }
}

#[test]
fn rebuilt_source_input_owner_can_restart_its_request_counter() {
    let mut r = router(Os::Windows, Os::Windows, false);
    ready(&mut r);
    let q = request(&r);
    r.receive_windows_lock(PEER, q.clone());
    assert!(permits(&mut r).pop().unwrap().take());
    r.detach_input(PEER);
    assert!(r.attach_input(q.source, [8; 16], true, false, Default::default(), 0));
    r.peers.get_mut(&PEER).unwrap().focus_ready = true;
    r.focus_scope = Some(FocusScope {
        members: vec![(ME, [1; 16]), (PEER, [8; 16])],
    });
    r.set_peer_windows_lock(PEER, true);
    r.receive_windows_lock(PEER, q);
    assert!(permits(&mut r).pop().unwrap().take());
}

#[test]
fn explicit_lock_ignores_keymaps_and_preserves_other_sources_modifier_references() {
    use oink_foundation::input_transform::{InputTransform, KeyMapping, KeyPreset, KeyboardMapping};
    let mut r = router(Os::Windows, Os::Windows, false);
    ready(&mut r);
    let other = DeviceId([3; 32]);
    r.test_connect(other, 0);
    r.peers.get_mut(&other).unwrap().os = Os::Windows;
    let mut transform = InputTransform::initial("windows");
    transform.windows.keyboard = KeyboardMapping {
        preset: KeyPreset::Custom,
        table: vec![
            KeyMapping {
                from: KeyCode::LEFT_GUI,
                to: KeyCode::LEFT_CTRL,
            },
            KeyMapping {
                from: L,
                to: KeyCode::NONE,
            },
        ],
        ..Default::default()
    };
    r.set_input_transforms([(PEER, transform)].into());
    r.target_key(PEER, KeyCode::LEFT_SHIFT, true);
    r.target_key(other, KeyCode::LEFT_SHIFT, true);
    r.drain_outputs();
    r.target_key(PEER, KeyCode::LEFT_SHIFT, false);
    r.receive_windows_lock(PEER, request(&r));
    assert_eq!(r.key_refs.get(&KeyCode::LEFT_SHIFT), Some(&1));
    assert!(!r.out.iter().any(|o| matches!(
        o,
        Output::Inject(InjectEvent::Key {
            code: KeyCode::LEFT_SHIFT,
            pressed: false
        })
    )));
    assert!(permits(&mut r).pop().unwrap().take());
}
