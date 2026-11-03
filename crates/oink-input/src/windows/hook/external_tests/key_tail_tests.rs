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
