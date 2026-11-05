use super::*;
use oink_foundation::{DeviceId, session::Boot};
use oink_routing::{
    Chord, HotkeyBinding, Os, Output, Packet, Router, RouterConfig, SimpleLayout, StatusEvent,
    control::{ControlMsg, FocusScope},
    input::InputMsg,
};

const ME: DeviceId = DeviceId([1; 32]);
const PEER: DeviceId = DeviceId([2; 32]);
const L: KeyCode = KeyCode::keyboard(0x0f);

fn focus_remote(r: &mut Router, epoch: u64) {
    assert!(r.input_message(
        PEER,
        Packet::Control(ControlMsg::FocusState {
            epoch,
            holder: PEER,
            lock_state: None,
            scope: Some(FocusScope {
                members: vec![(ME, r.routing_session()), (PEER, [2; 16])]
            }),
            session: Some([2; 16]),
        }),
        1
    ));
    assert_eq!(r.focus().holder, PEER);
}
fn router(action: &str) -> Router {
    let mut r = Router::new(
        RouterConfig::new(
            Boot {
                device_id: ME,
                nonce: [1; 16],
            },
            "source",
            Os::Windows,
        ),
        Arc::new(SimpleLayout::new()),
    );
    assert!(r.attach_input(
        Boot {
            device_id: PEER,
            nonce: [2; 16]
        },
        [2; 16],
        true,
        false,
        Default::default(),
        0
    ));
    r.set_peer_os(PEER, Os::Windows, 0);
    focus_remote(&mut r, 7);
    r.set_peer_windows_lock(PEER, true);
    if action == "mapped_drop" {
        use oink_foundation::input_transform::{InputTransform, KeyMapping, KeyPreset, KeyboardMapping};
        let mut transform = InputTransform::initial("windows");
        transform.windows.keyboard = KeyboardMapping {
            preset: KeyPreset::Custom,
            table: vec![KeyMapping {
                from: L,
                to: KeyCode::NONE,
            }],
            ..Default::default()
        };
        r.set_input_transforms([(ME, transform)].into());
    } else {
        r.set_hotkeys(vec![HotkeyBinding {
            chord: Chord::parse(&["Meta", "Shift", "L"]).unwrap(),
            action: action.into(),
        }]);
    }
    r.drain_outputs();
    r
}
fn deliver(vk: u32, scan: u32, extended: bool, injected: bool, pressed: bool) -> LRESULT {
    let info = KBDLLHOOKSTRUCT {
        vkCode: vk,
        scanCode: scan,
        flags: KBDLLHOOKSTRUCT_FLAGS(
            u32::from(extended) | if injected { LLKHF_INJECTED.0 } else { 0 } | if pressed { 0 } else { 0x80 },
        ),
        ..Default::default()
    };
    keyboard_proc_with_next(
        0,
        WPARAM(if pressed { WM_KEYDOWN } else { WM_KEYUP } as usize),
        LPARAM((&info as *const KBDLLHOOKSTRUCT) as isize),
        || LRESULT(0),
    )
}

#[test]
fn local_key_up_after_remote_hotkey_ends_press_before_first_returned_down() {
    for action in [
        "lock_remote_windows",
        "return_pointer_home",
        "lock_focus",
        "mapped_drop",
    ] {
        for injected in [true, false] {
            for reentrant in 0..3 {
                let f = Fixture::new();
                f.forward.store(true, Ordering::Relaxed);
                let r = Arc::new(Mutex::new(router(action)));
                let owner = r.clone();
                let recorded = f.events.clone();
                with_state(|s| {
                    s.handler = Arc::new(move |event| {
                        recorded.lock().unwrap().push(event.clone());
                        owner.lock().unwrap().on_local_event(event, 1)
                    })
                });
                deliver(0x5b, 0x5b, true, injected, true);
                deliver(0xa1, 0x36, true, injected, true);
                deliver(0x4c, 0x26, false, injected, true);
                let out = r.lock().unwrap().drain_outputs();
                if action != "mapped_drop" {
                    assert!(out.iter().any(|o| matches!(o,
                        Output::Status(StatusEvent::Hotkey { action: a }) if a == action)));
                }
                assert!(!out.iter().any(|o| matches!(o,
                    Output::Send { msg: Packet::Input(InputMsg::Key { code, pressed: true }), .. } if *code == L)));
                r.lock().unwrap().return_pointer_home();
                assert_eq!(r.lock().unwrap().focus().holder, ME);
                // Changing the requested external policy cannot split a held
                // chord, including a release which needs deferred dispatch.
                f.forward.store(false, Ordering::Relaxed);
                if reentrant != 0 {
                    // The nested UP must pass without borrowing the router;
                    // its observed edge still has to end the original press.
                    with_state(|s| s.execute(Transition::Local(None), Via::InHook));
                    if reentrant == 1 {
                        STATE.with(|slot| {
                            let _borrow = slot.borrow_mut();
                            assert_eq!(deliver(0x4c, 0x26, false, injected, false), LRESULT(0));
                        });
                    } else {
                        crate::capture_loop::install(Box::new(ReentrantOwner(injected)), Arc::new(|| {}));
                        crate::capture_loop::poll(Instant::now());
                    }
                    assert!(!f.events.lock().unwrap().iter().any(|e| e.event
                        == InputEvent::Key {
                            code: L,
                            pressed: false
                        }));
                } else {
                    with_state(|s| s.execute(Transition::Local(None), Via::InHook));
                    deliver(0x4c, 0x26, false, injected, false);
                }
                deliver(0xa1, 0x36, true, injected, false);
                deliver(0x5b, 0x5b, true, injected, false);
                r.lock().unwrap().set_input_transforms(Default::default());
                f.forward.store(true, Ordering::Relaxed);
                focus_remote(&mut r.lock().unwrap(), 20);
                with_state(|s| {
                    s.mode = CaptureMode::Remote {
                        park: Point::new(1920., 1080.),
                    }
                });
                r.lock().unwrap().drain_outputs();
                deliver(0x4c, 0x26, false, injected, true);
                deliver(0x4c, 0x26, false, injected, false);
                let out = r.lock().unwrap().drain_outputs();
                let edges: Vec<_> = out
                    .iter()
                    .filter_map(|o| match o {
                        Output::Send {
                            to: PEER,
                            msg: Packet::Input(InputMsg::Key { code, pressed }),
                            ..
                        } if *code == L => Some(*pressed),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    edges,
                    [true, false],
                    "action={action} injected={injected} reentrant={reentrant}"
                );
                assert_eq!(
                    f.events
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|e| e.event
                            == InputEvent::Key {
                                code: L,
                                pressed: false
                            })
                        .count(),
                    2
                );
            }
        }
    }
}

// poll() holds the real capture owner's RefCell borrow while the nested UP
// arrives, just as a native call made by routing mode work can do.
struct ReentrantOwner(bool);
impl crate::capture_loop::CaptureLoop for ReentrantOwner {
    fn set_wake(&mut self, _: crate::capture_loop::Wake) {}
    fn event(&mut self, _: &CapturedEvent) -> Disposition {
        panic!("fixture handler owns input")
    }
    fn poll(&mut self, _: Instant) -> Option<Instant> {
        assert_eq!(deliver(0x4c, 0x26, false, self.0, false), LRESULT(0));
        None
    }
    fn stopped(&self) -> bool {
        false
    }
    fn forward_injected_input(&self) -> bool {
        true
    }
}

#[test]
fn held_hotkey_is_not_rearmed_by_focus_or_reentrant_repeat() {
    for injected in [true, false] {
        let f = Fixture::new();
        f.forward.store(true, Ordering::Relaxed);
        let r = Arc::new(Mutex::new(router("lock_remote_windows")));
        let owner = r.clone();
        with_state(|s| s.handler = Arc::new(move |event| owner.lock().unwrap().on_local_event(event, 1)));
        deliver(0x5b, 0x5b, true, injected, true);
        deliver(0xa1, 0x36, true, injected, true);
        deliver(0x4c, 0x26, false, injected, true);
        assert_eq!(
            r.lock()
                .unwrap()
                .drain_outputs()
                .iter()
                .filter(|o| matches!(
                    o,
                    Output::Send {
                        msg: Packet::Control(ControlMsg::LockRemoteWindows(_)),
                        ..
                    }
                ))
                .count(),
            1
        );
        r.lock().unwrap().return_pointer_home();
        with_state(|s| s.execute(Transition::Local(None), Via::InHook));
        deliver(0x4c, 0x26, false, injected, true);
        STATE.with(|slot| {
            let _borrow = slot.borrow_mut();
            deliver(0x4c, 0x26, false, injected, true);
        });
        focus_remote(&mut r.lock().unwrap(), 20);
        with_state(|s| {
            s.mode = CaptureMode::Remote {
                park: Point::new(1920., 1080.),
            }
        });
        deliver(0x4c, 0x26, false, injected, true);
        assert!(!r.lock().unwrap().drain_outputs().iter().any(|o| matches!(
            o,
            Output::Send {
                msg: Packet::Control(ControlMsg::LockRemoteWindows(_)),
                ..
            }
        )));
        deliver(0x4c, 0x26, false, injected, false);
        deliver(0x4c, 0x26, false, injected, true);
        assert_eq!(
            r.lock()
                .unwrap()
                .drain_outputs()
                .iter()
                .filter(|o| matches!(
                    o,
                    Output::Send {
                        msg: Packet::Control(ControlMsg::LockRemoteWindows(_)),
                        ..
                    }
                ))
                .count(),
            1
        );
    }
}

#[test]
fn deferred_key_up_drains_at_safe_turn_without_another_key_and_keeps_order() {
    let f = Fixture::new();
    f.forward.store(true, Ordering::Relaxed);
    for (vk, scan) in [(0x4c, 0x26), (0x41, 0x1e)] {
        deliver(vk, scan, false, true, true);
    }
    STATE.with(|slot| {
        let mut state = slot.borrow_mut();
        state.as_mut().unwrap().execute(Transition::Local(None), Via::InHook);
        for (vk, scan) in [(0x4c, 0x26), (0x41, 0x1e)] {
            deliver(vk, scan, false, true, false);
            deliver(vk, scan, false, true, false); // duplicate UP coalesces
        }
    });
    key_releases::drain();
    let events = f.events.lock().unwrap();
    assert_eq!(events.len(), 4);
    assert_eq!(
        events[2].event,
        InputEvent::Key {
            code: L,
            pressed: false
        }
    );
    assert_eq!(
        events[3].event,
        InputEvent::Key {
            code: KeyCode::keyboard(4),
            pressed: false
        }
    );
    assert_eq!(with_state(|s| s.keys.len()), Some(0));
}

#[test]
fn default_external_local_up_stays_outside_routing_even_on_reentry() {
    let f = Fixture::new();
    deliver(0x4c, 0x26, false, true, true);
    STATE.with(|slot| {
        let _borrow = slot.borrow_mut();
        deliver(0x4c, 0x26, false, true, false);
    });
    key_releases::drain();
    assert!(f.events.lock().unwrap().is_empty());
}

#[test]
fn reentrant_fake_shift_up_cannot_displace_the_real_shift_release() {
    let f = Fixture::new();
    deliver(0xa1, 0x36, true, false, true);
    STATE.with(|slot| {
        let _borrow = slot.borrow_mut();
        deliver(0xa1, 0x236, true, false, false);
        deliver(0xa1, 0x36, true, false, false);
    });
    key_releases::drain();
    let events = f.events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[1].event,
        InputEvent::Key {
            code: KeyCode::RIGHT_SHIFT,
            pressed: false
        }
    );
    assert!(physical_keys().is_empty());
    assert_eq!(with_state(|s| s.keys.len()), Some(0));
}

#[test]
fn modifier_trace_distinguishes_filtered_and_deferred_releases_without_changing_routes() {
    use keyboard_trace::{Outcome, Stage, TEST_RECORDS};
    let f = Fixture::new();
    TEST_RECORDS.with_borrow_mut(|r| *r = Some(Vec::new()));
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_RECORDS.with_borrow_mut(|r| *r = None);
        }
    }
    let _reset = Reset;
    deliver(0x5b, 0x5b, true, false, true);
    assert!(with_state(|s| s.keys.contains_key(&KeyCode::LEFT_GUI.0)).unwrap());
    // External-local UP is observed, but must not release the physical hold.
    deliver(0x5b, 0x5b, true, true, false);
    assert!(with_state(|s| s.keys.contains_key(&KeyCode::LEFT_GUI.0)).unwrap());
    // A real UP while the owner is borrowed is deferred, not lost.
    STATE.with(|state| {
        let _borrow = state.borrow_mut();
        assert_eq!(deliver(0x5b, 0x5b, true, false, false).0, 0);
    });
    key_releases::drain();
    assert!(!with_state(|s| s.keys.contains_key(&KeyCode::LEFT_GUI.0)).unwrap());
    let events = f.events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0].event,
        InputEvent::Key {
            code: KeyCode::LEFT_GUI,
            pressed: true
        }
    ));
    assert!(matches!(
        events[1].event,
        InputEvent::Key {
            code: KeyCode::LEFT_GUI,
            pressed: false
        }
    ));
    drop(events);
    let records = TEST_RECORDS.with_borrow(|r| r.clone().unwrap());
    assert_eq!(records.iter().filter(|r| r.reason == Stage::Enter as u32).count(), 3);
    assert_eq!(records.iter().filter(|r| r.reason == Stage::Exit as u32).count(), 3);
    assert!(records.iter().any(|r| r.reason == Stage::Exit as u32
        && r.flags >> 16 == Outcome::ExternalLocal as u32
        && r.steps == Some(8)));
    assert!(
        records
            .iter()
            .any(|r| r.reason == Stage::Exit as u32 && r.flags >> 16 == Outcome::Reentrant as u32 && r.steps.is_none())
    );
    assert!(
        records
            .iter()
            .any(|r| r.reason == Stage::Deferred as u32 && r.success == Some(true))
    );
    assert!(
        records
            .iter()
            .any(|r| r.reason == Stage::Drained as u32 && r.steps == Some(0))
    );
    // An ordinary key never adds a modifier trace record.
    let before = records.len();
    deliver(0x41, 0x1e, false, false, true);
    deliver(0x41, 0x1e, false, false, false);
    assert_eq!(TEST_RECORDS.with_borrow(|r| r.as_ref().unwrap().len()), before);
}

#[test]
fn modifier_trace_observes_lock_guard_and_own_up_without_clearing_physical_hold() {
    use keyboard_trace::{Outcome, Stage, TEST_RECORDS};
    struct Guard;
    impl crate::capture_loop::CaptureLoop for Guard {
        fn set_wake(&mut self, _: crate::capture_loop::Wake) {}
        fn event(&mut self, _: &CapturedEvent) -> Disposition {
            panic!("guard must bypass handler")
        }
        fn poll(&mut self, _: Instant) -> Option<Instant> {
            None
        }
        fn stopped(&self) -> bool {
            false
        }
        fn keyboard_disabled(&self) -> bool {
            true
        }
        fn keyboard_guard_suspended(&self) -> bool {
            true
        }
    }
    let f = Fixture::new();
    TEST_RECORDS.with_borrow_mut(|r| *r = Some(Vec::new()));
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_RECORDS.with_borrow_mut(|r| *r = None);
        }
    }
    let _reset = Reset;
    deliver(0x5b, 0x5b, true, false, true);
    let own = KBDLLHOOKSTRUCT {
        vkCode: 0x5b,
        scanCode: 0x5b,
        flags: KBDLLHOOKSTRUCT_FLAGS(LLKHF_EXTENDED.0 | LLKHF_INJECTED.0 | 0x80),
        dwExtraInfo: INJECT_TAG,
        ..Default::default()
    };
    keyboard_proc_with_next(
        0,
        WPARAM(WM_KEYUP as usize),
        LPARAM((&own as *const KBDLLHOOKSTRUCT) as isize),
        || LRESULT(0),
    );
    crate::capture_loop::install(Box::new(Guard), Arc::new(|| {}));
    assert_eq!(deliver(0x5b, 0x5b, true, false, false).0, 0);
    assert!(with_state(|s| s.keys.contains_key(&KeyCode::LEFT_GUI.0)).unwrap());
    assert_eq!(f.events.lock().unwrap().len(), 1);
    let records = TEST_RECORDS.with_borrow(|r| r.clone().unwrap());
    for reason in [Outcome::Own, Outcome::LockGuard] {
        assert!(
            records
                .iter()
                .any(|r| r.reason == Stage::Exit as u32 && r.flags >> 16 == reason as u32 && r.steps == Some(8))
        );
    }
}

#[test]
fn boundary_reconcile_retires_only_locally_passed_physical_modifiers() {
    let f = Fixture::new();
    let recorded = f.events.clone();
    with_state(|s| {
        s.handler = Arc::new(move |event| {
            recorded.lock().unwrap().push(event.clone());
            Disposition::Pass
        })
    });
    deliver(0x5b, 0x5b, true, false, true);
    assert_eq!(
        with_state(|s| s.keys.get(&KeyCode::LEFT_GUI.0).copied()),
        Some(Some(false))
    );
    assert_eq!(modifier_reconcile::with_sample_for_test(8, None), 0);
    assert_eq!(modifier_reconcile::with_sample_for_test(8, Some(8)), 0);
    {
        let _in_keyboard_callback = modifier_reconcile::KeyboardCallback::enter();
        assert_eq!(modifier_reconcile::with_sample_for_test(8, Some(0)), 0);
    }
    let events = f.events.lock().unwrap().len();
    assert_eq!(modifier_reconcile::with_sample_for_test(8, Some(0)), 8);
    assert_eq!(modifier_reconcile::with_sample_for_test(8, Some(0)), 0);
    assert_eq!(with_state(|s| s.keys.contains_key(&KeyCode::LEFT_GUI.0)), Some(false));
    assert!(!physical_keys().contains(&KeyCode::LEFT_GUI));
    // Router consumes the returned mask. No callback recursion or OS injection.
    assert_eq!(f.events.lock().unwrap().len(), events);
}

#[test]
fn boundary_reconcile_excludes_swallow_external_and_downstream_rejection() {
    let f = Fixture::new();

    deliver(0x5b, 0x5b, true, false, true);
    assert_eq!(modifier_reconcile::with_sample_for_test(8, Some(0)), 0);
    deliver(0x5b, 0x5b, true, false, false);
    let recorded = f.events.clone();
    with_state(|s| {
        s.handler = Arc::new(move |event| {
            recorded.lock().unwrap().push(event.clone());
            Disposition::Pass
        })
    });
    deliver(0x5b, 0x5b, true, false, true);
    // External-local UP affects system state but is not a physical release.
    // It must invalidate even when the native state is borrowed.
    with_state(|_| {
        deliver(0x5b, 0x5b, true, true, false);
    });
    assert_eq!(modifier_reconcile::with_sample_for_test(8, Some(0)), 0);
    deliver(0x5b, 0x5b, true, false, false);
    let info = KBDLLHOOKSTRUCT {
        vkCode: 0x5b,
        scanCode: 0x5b,
        flags: KBDLLHOOKSTRUCT_FLAGS(1),
        ..Default::default()
    };
    keyboard_proc_with_next(
        0,
        WPARAM(WM_KEYDOWN as usize),
        LPARAM((&info as *const KBDLLHOOKSTRUCT) as isize),
        || LRESULT(1),
    );
    assert_eq!(modifier_reconcile::with_sample_for_test(8, Some(0)), 0);
    deliver(0x5b, 0x5b, true, false, false);
    deliver(0x5b, 0x5b, true, false, true);
    assert_eq!(modifier_reconcile::with_sample_for_test(8, Some(0)), 8);
}

#[test]
fn native_boundary_probe_prevents_lost_win_up_from_becoming_remote_control() {
    let _f = Fixture::new();
    let mut r = Router::new(
        RouterConfig::new(
            Boot {
                device_id: ME,
                nonce: [1; 16],
            },
            "source",
            Os::Windows,
        ),
        Arc::new(SimpleLayout::new()),
    );
    r.attach_input(
        Boot {
            device_id: PEER,
            nonce: [2; 16],
        },
        [2; 16],
        true,
        false,
        Default::default(),
        0,
    );
    r.set_peer_os(PEER, Os::MacOS, 0);
    r.set_modifier_release_probe(Arc::new(|mask| modifier_reconcile::with_sample_for_test(mask, Some(0))));
    let router = Arc::new(Mutex::new(r));
    let owner = router.clone();
    with_state(|s| {
        s.mode = CaptureMode::Local;
        s.handler = Arc::new(move |event| owner.lock().unwrap().on_local_event(event, 1));
    });
    assert_eq!(deliver(0x5b, 0x5b, true, false, true), LRESULT(0));
    // Fixture omits the physical UP, but supplies the boundary's OS-up sample.
    // No real input, async-state query, hook installation or desktop operation.
    let mut r = router.lock().unwrap();
    r.drain_outputs();
    focus_remote(&mut r, 8);
    let out = r.drain_outputs();
    assert!(!out.iter().any(|o| matches!(
        o,
        Output::Send {
            msg: Packet::Input(oink_routing::input::InputMsg::Key {
                code: KeyCode::LEFT_GUI,
                pressed: true
            }),
            ..
        }
    )));
    assert_eq!(with_state(|s| s.keys.contains_key(&KeyCode::LEFT_GUI.0)), Some(false));
    assert!(!physical_keys().contains(&KeyCode::LEFT_GUI));
}

#[test]
fn boundary_reconcile_treats_neutral_synthetic_modifier_as_both_sides() {
    let f = Fixture::new();
    let recorded = f.events.clone();
    with_state(|s| {
        s.handler = Arc::new(move |event| {
            recorded.lock().unwrap().push(event.clone());
            Disposition::Pass
        })
    });
    // Physical LCtrl, RShift and RAlt passed locally; another program then
    // injects neutral UPs. Async zero no longer describes these physical holds.
    deliver(0xa2, 0x1d, false, false, true);
    deliver(0xa1, 0x36, true, false, true);
    deliver(0xa5, 0x38, true, false, true);
    assert_eq!(modifier_reconcile::with_sample_for_test(0x61, Some(0)), 0x61);
    deliver(0xa2, 0x1d, false, false, true);
    deliver(0xa1, 0x36, true, false, true);
    deliver(0xa5, 0x38, true, false, true);
    deliver(0x11, 0, false, true, false);
    deliver(0x10, 0, false, true, false);
    deliver(0x12, 0, false, true, false);
    assert_eq!(modifier_reconcile::with_sample_for_test(0x61, Some(0)), 0);
}
