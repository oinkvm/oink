//! Screenless member (protocol §14.2): two desktops A, B and a phone P that is
//! not in the layout. Messages are delivered in order between routers.
use super::*;
use crate::control::{EdgeSide, EdgeState};

const A: DeviceId = DeviceId([1; 32]);
const B: DeviceId = DeviceId([2; 32]);
const P: DeviceId = DeviceId([9; 32]);

fn display(key: &str, bounds: Rect) -> DisplayInfo {
    DisplayInfo {
        key: key.into(),
        name: key.into(),
        bounds,
        pixel_width: bounds.w as u32,
        pixel_height: bounds.h as u32,
        reference_extent: None,
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }
}

fn boot(id: DeviceId) -> oink_foundation::session::Boot {
    oink_foundation::session::Boot {
        device_id: id,
        nonce: [id.0[0]; 16],
    }
}

fn layout() -> Arc<crate::SimpleLayout> {
    let mut layout = crate::SimpleLayout::new();
    let bounds = Rect::new(0., 0., 1000., 800.);
    layout.add_screen(ScreenId::new(A, "a"), bounds, Rect::new(0., 0., 1000., 800.));
    layout.add_screen(ScreenId::new(B, "b"), bounds, Rect::new(1000., 0., 1000., 800.));
    Arc::new(layout)
}

/// Routers for A, B and P, all attached to each other and focus-ready.
fn group() -> Vec<Router> {
    let bounds = Rect::new(0., 0., 1000., 800.);
    let mut routers = Vec::new();
    for (id, name) in [(A, "a"), (B, "b"), (P, "p")] {
        let mut config = RouterConfig::new(boot(id), name, if id == P { Os::Other } else { Os::Windows });
        config.screenless = id == P;
        let mut r = Router::new(
            config,
            if id == P {
                Arc::new(crate::SimpleLayout::new())
            } else {
                layout()
            },
        );
        // The instance each peer is attached with (production learns it from FocusState).
        r.session = [id.0[0]; 16];
        if id != P {
            r.set_local_displays(vec![display(name, bounds)]);
            r.set_cursor(Point::new(500., 400.));
        }
        r.set_game_mode(GameMode::Off);
        for (other, other_name) in [(A, "a"), (B, "b"), (P, "p")] {
            if other != id {
                assert!(r.attach_input(boot(other), [other.0[0]; 16], true, true, FeelProfile::default(), 0));
                r.peers.get_mut(&other).unwrap().focus_ready = true;
                if other != P {
                    r.set_peer_displays(other, vec![display(other_name, bounds)]);
                }
            }
        }
        r.drain_outputs();
        routers.push(r);
    }
    routers
}

/// Messages delivered between routers: (from, to, message).
type Delivered = Vec<(DeviceId, DeviceId, Message)>;

/// Deliver every Send until quiet; returns the delivered messages and every
/// other output, per router index.
fn settle(routers: &mut [Router], now: Micros) -> (Delivered, Vec<Vec<Output>>) {
    let mut sent = Vec::new();
    let mut other = vec![Vec::new(); routers.len()];
    for _ in 0..30 {
        let mut messages = Vec::new();
        for (i, r) in routers.iter_mut().enumerate() {
            let from = r.me();
            for output in r.drain_outputs() {
                match output {
                    Output::Send { to, msg, .. } => messages.push((from, to, msg)),
                    output => other[i].push(output),
                }
            }
        }
        if messages.is_empty() {
            return (sent, other);
        }
        for (from, to, msg) in messages {
            if let Some(r) = routers.iter_mut().find(|r| r.me() == to) {
                r.input_message(from, msg.clone(), now);
            }
            sent.push((from, to, msg));
        }
    }
    panic!("routers must converge");
}

fn moved(outputs: &[Output]) -> bool {
    outputs.iter().any(|o| {
        matches!(
            o,
            Output::Inject(InjectEvent::MoveTo { .. } | InjectEvent::MoveToWithDelta { .. })
        )
    })
}

#[test]
fn first_input_without_focus_or_selection_is_dropped_and_reported() {
    let mut g = group();
    assert!(!g[2].member_input(MemberInput::Motion { dx: 5., dy: 0. }, 1));
    let outputs = g[2].drain_outputs();
    assert!(
        outputs
            .iter()
            .any(|o| matches!(o, Output::Status(StatusEvent::NoTarget)))
    );
    assert!(!outputs.iter().any(|o| matches!(o, Output::Send { .. })));
    // A screenless member never outputs capture or cursor placement.
    assert!(
        !outputs
            .iter()
            .any(|o| matches!(o, Output::SetMode(_) | Output::RestoreLocal { .. }))
    );
}

#[test]
fn first_input_moves_the_pointer_to_the_selected_device() {
    let mut g = group();
    g[2].set_selected_target(Some(B));
    assert!(g[2].member_input(MemberInput::Motion { dx: 5., dy: 0. }, 1));
    let (sent, outputs) = settle(&mut g, 1);
    let enter = sent
        .iter()
        .find_map(|(from, to, msg)| match msg {
            Message::Control(
                ControlMsg::MovePointerEnter {
                    epoch, display, pos, ..
                }
                | ControlMsg::FocusEnter {
                    epoch, display, pos, ..
                },
            ) if *from == P && *to == B => Some((*epoch, display.clone(), *pos)),
            _ => None,
        })
        .expect("an entry to the selected device");
    // Landing: centre of B's primary screen from its facts; epoch 0 -> 1.
    assert_eq!(enter, (1, "b".to_string(), Point::new(500., 400.)));
    for r in &g {
        assert_eq!(r.focus_claim(), (1, B), "{:?}", r.me());
    }
    assert_eq!(g[2].member_target(), Some(B));
    assert!(
        !outputs[2]
            .iter()
            .any(|o| matches!(o, Output::SetMode(_) | Output::RestoreLocal { .. }))
    );
    // The motion that chose the target reaches it in pointer units.
    assert!(sent.iter().any(|(from, to, msg)| *from == P
        && *to == B
        && matches!(msg, Message::Input(InputMsg::PointerMotion { dx, .. }) if *dx == 5.)));
    assert!(moved(&outputs[1]));
}

#[test]
fn first_input_joins_an_existing_focus_without_moving_it() {
    let mut g = group();
    // A crosses into B: epoch 1, holder B, the phone only learns it by gossip.
    g[0].move_pointer_to(B).unwrap();
    let _ = settle(&mut g, 1);
    let (epoch, holder) = g[1].focus_claim();
    assert_eq!(holder, B);
    // The phone restarted after that hand-off: its new instance is not in the scope.
    g[2] = {
        let mut config = RouterConfig::new(boot(P), "p", Os::Other);
        config.screenless = true;
        let mut r = Router::new(config, Arc::new(crate::SimpleLayout::new()));
        r.session = [99; 16];
        r.set_game_mode(GameMode::Off);
        for (other, name) in [(A, "a"), (B, "b")] {
            assert!(r.attach_input(boot(other), [other.0[0]; 16], true, true, FeelProfile::default(), 0));
            r.peers.get_mut(&other).unwrap().focus_ready = true;
            r.set_peer_displays(other, vec![display(name, Rect::new(0., 0., 1000., 800.))]);
        }
        r
    };
    for i in [0, 1] {
        g[i].peers.get_mut(&P).unwrap().session = Some([99; 16]);
        let (from, state) = (g[i].me(), g[i].input_opening(true)[2].clone());
        g[2].input_message(from, state, 1);
    }
    g[2].drain_outputs();
    assert!(!g[1].focus_scope.as_ref().unwrap().members.contains(&(P, [99; 16])));
    assert_eq!(g[2].member_target(), None);
    g[2].set_selected_target(Some(A));
    assert!(g[2].member_input(
        MemberInput::Key {
            code: KeyCode::keyboard(0x04),
            pressed: true
        },
        2
    ));
    let (sent, _) = settle(&mut g, 2);
    // Joined with FocusResume; focus, epoch and pointer unchanged.
    assert!(sent.iter().any(|(from, _, msg)| *from == P
        && matches!(msg, Message::Control(ControlMsg::FocusResume { holder, .. }) if *holder == B)));
    assert!(
        !sent
            .iter()
            .any(|(_, _, msg)| matches!(msg, Message::Control(ControlMsg::MovePointerEnter { .. })))
    );
    for r in &g {
        assert_eq!(r.focus_claim(), (epoch, B));
    }
    assert!(g[1].focus_scope.as_ref().unwrap().members.contains(&(P, g[2].session)));
    assert!(sent.iter().any(|(from, to, msg)| *from == P
        && *to == B
        && matches!(msg, Message::Input(InputMsg::Key { pressed: true, .. }))));
}

#[test]
fn losing_the_holder_leaves_the_phone_without_a_target_until_its_next_input() {
    let mut g = group();
    g[2].set_selected_target(Some(B));
    g[2].member_input(MemberInput::Motion { dx: 1., dy: 0. }, 1);
    let _ = settle(&mut g, 1);
    assert_eq!(g[2].member_target(), Some(B));
    g[2].test_disconnect(B, 2);
    g[0].test_disconnect(B, 2);
    assert_eq!(g[2].member_target(), None);
    assert!(
        !g[2]
            .drain_outputs()
            .iter()
            .any(|o| matches!(o, Output::SetMode(_) | Output::RestoreLocal { .. }))
    );
    // Next input: no valid focus any more, so the selected device is entered.
    g[2].set_selected_target(Some(A));
    assert!(g[2].member_input(MemberInput::Motion { dx: 1., dy: 0. }, 3));
    let (sent, _) = settle(&mut g[..1], 3);
    let _ = sent;
    let outputs = g[2].drain_outputs();
    assert!(outputs.iter().any(|o| matches!(
        o,
        Output::Send {
            to,
            msg: Message::Control(ControlMsg::MovePointerEnter { epoch: 2, .. } | ControlMsg::FocusEnter { epoch: 2, .. }),
            ..
        } if *to == A
    )));
}

#[test]
fn focus_enter_to_a_phone_without_a_screen_is_refused() {
    let mut g = group();
    let scope = g[0].new_focus_scope();
    g[2].input_message(
        A,
        Message::Control(ControlMsg::FocusEnter {
            epoch: 1,
            display: "p".into(),
            pos: Point::new(1., 1.),
            scope: Some(scope),
            pointer_source: None,
        }),
        1,
    );
    let outputs = g[2].drain_outputs();
    assert!(outputs.iter().any(|o| matches!(
        o,
        Output::Send {
            to,
            msg: Message::Control(ControlMsg::FocusState { .. }),
            ..
        } if *to == A
    )));
    assert!(
        !outputs
            .iter()
            .any(|o| matches!(o, Output::SetMode(_) | Output::RestoreLocal { .. }))
    );
    assert_ne!(g[2].focus_claim(), (1, P));
}

#[test]
fn text_and_enter_reach_the_holder_in_order() {
    let mut g = group();
    g[2].set_selected_target(Some(A));
    g[2].member_input(MemberInput::Text("你好".into()), 1);
    g[2].member_input(
        MemberInput::Key {
            code: KeyCode::keyboard(0x28),
            pressed: true,
        },
        1,
    );
    g[2].member_input(
        MemberInput::Key {
            code: KeyCode::keyboard(0x28),
            pressed: false,
        },
        1,
    );
    let (_, outputs) = settle(&mut g, 1);
    let injected: Vec<_> = outputs[0]
        .iter()
        .filter_map(|o| match o {
            Output::Inject(InjectEvent::Text(text)) => Some(text.0.clone()),
            Output::Inject(InjectEvent::Key { code, pressed }) if *code == KeyCode::keyboard(0x28) => {
                Some(format!("enter:{pressed}"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(injected, ["你好", "enter:true", "enter:false"]);
}

#[test]
fn a_phone_disconnecting_releases_what_it_holds() {
    let mut g = group();
    g[2].set_selected_target(Some(A));
    g[2].member_input(
        MemberInput::Key {
            code: KeyCode::LEFT_SHIFT,
            pressed: true,
        },
        1,
    );
    let (_, outputs) = settle(&mut g, 1);
    assert!(
        outputs[0]
            .iter()
            .any(|o| matches!(o, Output::Inject(InjectEvent::Key { pressed: true, .. })))
    );
    g[0].test_disconnect(P, 2);
    g[0].retire_boot(boot(P));
    assert!(
        g[0].drain_outputs()
            .iter()
            .any(|o| matches!(o, Output::Inject(InjectEvent::Key { pressed: false, .. })))
    );
}

#[test]
fn input_while_the_holder_pauses_sharing_is_dropped_there() {
    let mut g = group();
    g[2].set_selected_target(Some(A));
    g[2].member_input(MemberInput::Motion { dx: 1., dy: 0. }, 1);
    let _ = settle(&mut g, 1);
    g[0].set_paused(true);
    let _ = settle(&mut g, 2);
    g[2].member_input(
        MemberInput::Key {
            code: KeyCode::keyboard(0x04),
            pressed: true,
        },
        3,
    );
    let (_, outputs) = settle(&mut g, 3);
    assert!(
        !outputs[0]
            .iter()
            .any(|o| matches!(o, Output::Inject(InjectEvent::Key { pressed: true, .. })))
    );
}

#[test]
fn lock_button_pins_the_current_focus_through_set_focus_lock() {
    let mut g = group();
    assert_eq!(g[2].member_set_locked(true), Err(FocusLockError::Unavailable));
    g[2].set_selected_target(Some(A));
    g[2].member_input(MemberInput::Motion { dx: 1., dy: 0. }, 1);
    let _ = settle(&mut g, 1);
    assert_eq!(g[2].member_set_locked(true), Ok(()));
    let (sent, _) = settle(&mut g, 2);
    assert!(sent.iter().any(|(from, to, msg)| *from == P
        && *to == A
        && matches!(
            msg,
            Message::Control(ControlMsg::SetFocusLock { locked: Some(true), .. })
        )));
    for r in &g {
        assert_eq!(r.group_focus_status().locked_device, Some(A), "{:?}", r.me());
    }
}

#[test]
fn pointer_units_scale_by_the_base_screen_and_report_the_edge() {
    let mut g = group();
    g[0].set_pointer_base(Some(PointerBase {
        screen: ScreenId::new(B, "b"),
        system_scale: 2.0,
    }));
    g[2].set_selected_target(Some(A));
    g[2].member_input(MemberInput::Motion { dx: 1., dy: 0. }, 1);
    let _ = settle(&mut g, 1);
    let before = g[0].pointer.clone().unwrap().local;
    g[2].member_input(MemberInput::Motion { dx: 10., dy: 0. }, 2);
    let _ = settle(&mut g, 2);
    let after = g[0].pointer.clone().unwrap().local;
    // SimpleLayout: one canvas unit per native unit, so 10 pointer units = 20 px.
    assert!((after.x - before.x - 20.).abs() < 1e-9, "{before:?} -> {after:?}");
    // Pushing into A's left edge (a wall) reports Blocked to the phone.
    g[2].member_input(MemberInput::Motion { dx: -5000., dy: 0. }, 3);
    let (sent, outputs) = settle(&mut g, 3);
    assert!(sent.iter().any(|(from, to, msg)| *from == A
        && *to == P
        && matches!(
            msg,
            Message::Control(ControlMsg::EdgeReport {
                state: EdgeState::Blocked,
                edge: EdgeSide::Left,
                ..
            })
        )));
    assert!(outputs[2].iter().any(|o| matches!(
        o,
        Output::Status(StatusEvent::Edge {
            state: EdgeState::Blocked,
            ..
        })
    )));
}

/// A's native delta for `pu` pointer units with B as the pointer base, under
/// a layout where A's and B's canvas rects are `a` and `b` (native 1000x800).
fn native_delta(a: Rect, b: Rect, third: bool, pu: f64) -> f64 {
    let mut layout = crate::SimpleLayout::new();
    let bounds = Rect::new(0., 0., 1000., 800.);
    layout.add_screen(ScreenId::new(A, "a"), bounds, a);
    layout.add_screen(ScreenId::new(B, "b"), bounds, b);
    if third {
        layout.add_screen(
            ScreenId::new(DeviceId([3; 32]), "c"),
            bounds,
            Rect::new(b.x + b.w, b.y, b.w, b.h),
        );
    }
    let mut g = group();
    g[0].set_layout(Arc::new(layout));
    g[0].set_pointer_base(Some(PointerBase {
        screen: ScreenId::new(B, "b"),
        system_scale: 1.5,
    }));
    g[2].set_selected_target(Some(A));
    g[2].member_input(MemberInput::Motion { dx: 0.001, dy: 0. }, 1);
    let _ = settle(&mut g, 1);
    let before = g[0].pointer.clone().unwrap().local.x;
    g[2].member_input(MemberInput::Motion { dx: pu, dy: 0. }, 2);
    let _ = settle(&mut g, 2);
    g[0].pointer.clone().unwrap().local.x - before
}

#[test]
fn pointer_speed_ignores_whole_layout_scale_and_joins_but_follows_a_scaled_target() {
    let a = Rect::new(0., 0., 1000., 800.);
    let b = Rect::new(1000., 0., 1000., 800.);
    let base = native_delta(a, b, false, 10.);
    assert!((base - 15.).abs() < 1e-6, "{base}");
    // The whole layout drawn twice as large: the same distance on A.
    let scaled = native_delta(
        Rect::new(0., 0., 2000., 1600.),
        Rect::new(2000., 0., 2000., 1600.),
        false,
        10.,
    );
    assert!((scaled - base).abs() < 1e-6, "{scaled} vs {base}");
    // A third device joins: unchanged.
    let joined = native_delta(a, b, true, 10.);
    assert!((joined - base).abs() < 1e-6, "{joined} vs {base}");
    // Only A drawn twice as large: the pointer is half as fast on A.
    let large_a = native_delta(Rect::new(0., 0., 2000., 1600.), b, false, 10.);
    assert!((large_a - base / 2.).abs() < 1e-6, "{large_a} vs {base}");
}

#[test]
fn game_mode_motion_from_the_phone_is_relative() {
    let mut g = group();
    g[2].set_selected_target(Some(A));
    g[2].member_input(MemberInput::Motion { dx: 1., dy: 0. }, 1);
    let _ = settle(&mut g, 1);
    assert_eq!(g[2].member_set_game_mode(GameMode::On), Ok(()));
    let _ = settle(&mut g, 2);
    assert!(g[2].game_mode().active, "{:?}", g[2].game_mode());
    g[2].member_input(MemberInput::Motion { dx: 7., dy: -3. }, 3);
    let (sent, outputs) = settle(&mut g, 3);
    assert!(sent.iter().any(|(from, to, msg)| *from == P
        && *to == A
        && matches!(msg, Message::Input(InputMsg::RelativeMotion { .. }))));
    assert!(
        outputs[0]
            .iter()
            .any(|o| matches!(o, Output::Inject(InjectEvent::MoveRelative { .. })))
    );
}

#[test]
fn refused_text_comes_back_with_its_reason() {
    let mut g = group();
    g[2].set_selected_target(Some(A));
    g[2].member_input(MemberInput::Motion { dx: 1., dy: 0. }, 1);
    let _ = settle(&mut g, 1);
    // The holder pauses while the phone still targets it.
    g[0].paused = true;
    assert!(g[2].member_input(MemberInput::Text("x".into()), 2));
    let (sent, outputs) = settle(&mut g, 2);
    assert!(sent.iter().any(|(from, to, msg)| *from == A
        && *to == P
        && matches!(
            msg,
            Message::Control(ControlMsg::TextRefused {
                reason: crate::control::TextRefusal::Paused,
                ..
            })
        )));
    assert!(outputs[2].iter().any(|o| matches!(
        o,
        Output::Status(StatusEvent::TextRefused {
            reason: crate::control::TextRefusal::Paused,
            ..
        })
    )));
}

#[test]
fn an_emergency_pause_releases_what_the_phone_holds_elsewhere() {
    let mut g = group();
    g[2].set_selected_target(Some(A));
    g[2].member_input(
        MemberInput::Key {
            code: KeyCode::LEFT_SHIFT,
            pressed: true,
        },
        1,
    );
    let (_, outputs) = settle(&mut g, 1);
    assert!(
        outputs[0]
            .iter()
            .any(|o| matches!(o, Output::Inject(InjectEvent::Key { pressed: true, .. })))
    );
    g[2].member_emergency(2);
    let (_, outputs) = settle(&mut g, 2);
    assert!(outputs[0].iter().any(
        |o| matches!(o, Output::Inject(InjectEvent::Key { code, pressed: false }) if *code == KeyCode::LEFT_SHIFT)
    ));
}

#[test]
fn entering_the_phones_own_screen_restores_its_pointer_even_when_untargeted() {
    let mut g = group();
    let android = DisplayInfo {
        is_builtin: true,
        ..display(
            oink_foundation::display::ANDROID_INPUT_DISPLAY,
            Rect::new(0., 0., 400., 800.),
        )
    };
    // A privileged Android phone: its own screen is the only local display.
    // Untargeted, its router holds focus on itself without the screen.
    g[2].set_local_displays(vec![android.clone()]);
    g[2].drain_outputs();
    assert_eq!(g[2].focus().holder, P);
    g[0].set_peer_os(P, Os::Android, 1);
    let mut shared = (*layout()).clone();
    shared.add_screen(
        ScreenId::new(P, android.key.clone()),
        android.bounds,
        Rect::new(2000., 0., 400., 800.),
    );
    for router in &mut g {
        router.set_layout(Arc::new(shared.clone()));
    }
    g[0].set_peer_displays(P, vec![android]);
    g[0].move_pointer_to(P).unwrap();
    let (_, outputs) = settle(&mut g, 1);
    assert!(
        outputs[2].iter().any(|o| matches!(o, Output::RestoreLocal { .. })),
        "{:?}",
        outputs[2]
    );
}

/// The control page repeats a held key by sending its down again while it is
/// down (user 2026-11-12): the phone's router forwards those as `KeyRepeat`,
/// and the holder injects one repeat each until the up.
#[test]
fn a_key_pressed_again_while_down_reaches_the_holder_as_repeats() {
    let mut g = group();
    g[2].set_selected_target(Some(A));
    let code = KeyCode::keyboard(0x2A);
    for _ in 0..3 {
        g[2].member_input(MemberInput::Key { code, pressed: true }, 1);
    }
    g[2].member_input(MemberInput::Key { code, pressed: false }, 1);
    let (sent, outputs) = settle(&mut g, 1);
    let repeats: Vec<u32> = sent
        .iter()
        .filter_map(|(from, to, msg)| match msg {
            Message::Input(InputMsg::KeyRepeat { code: c, n }) if *from == P && *to == A && *c == code => Some(*n),
            _ => None,
        })
        .collect();
    assert_eq!(repeats, [1, 2]);
    let injected: Vec<bool> = outputs[0]
        .iter()
        .filter_map(|o| match o {
            Output::Inject(InjectEvent::Key { code: c, pressed }) if *c == code => Some(*pressed),
            _ => None,
        })
        .collect();
    assert_eq!(injected, [true, true, true, false]);
}

/// The control page's touchpad as a trackpad source (user 2026-11-12): the
/// phone sends raw contacts, phased touchpad scroll and gestures, and a
/// receiver that knows it is a phone chooses between them exactly as for a Mac.
mod trackpad_source {
    use super::*;
    use oink_foundation::contacts::{Contact, Phase, Sample, Surface};
    use oink_foundation::input::SwipeAxis;

    fn sample(time_us: u64, fingers: u8, x: i32) -> Sample {
        let mut s = Sample {
            device: 1,
            time_us,
            surface: Surface {
                width: 6_000,
                height: 7_000,
            },
            len: fingers,
            ..Default::default()
        };
        for (i, c) in s.contacts.iter_mut().enumerate().take(usize::from(fingers)) {
            *c = Contact {
                id: i as u32 + 1,
                x: x + i as i32 * 1000,
                y: 3000,
            };
        }
        s
    }

    /// The phone controls B (Windows with native contact injection); B knows
    /// the phone as a phone only when `mobile`.
    fn phone_controls_windows_b(mobile: bool) -> Vec<Router> {
        let mut g = group();
        g[1].set_touchpad_contacts(Some(5));
        // Production: the adapter sets this for every `oink.input/2` peer.
        g[1].set_peer_touchpad_options(P, true);
        g[1].set_peer_mobile(P, mobile);
        g[2].set_selected_target(Some(B));
        assert!(g[2].member_input(MemberInput::Motion { dx: 1., dy: 0. }, 1));
        settle(&mut g, 1);
        g
    }

    fn scroll(phase: ScrollPhase, dy: f64) -> MemberInput {
        MemberInput::Scroll { dx: 0., dy, phase }
    }

    /// Native contact frames and touchpad-scroll phases injected.
    fn injected(outputs: &[Output]) -> (Vec<Phase>, Vec<ScrollPhase>) {
        let contacts = outputs
            .iter()
            .filter_map(|o| match o {
                Output::Inject(InjectEvent::TouchpadContacts(f)) => Some(f.phase),
                _ => None,
            })
            .collect();
        let scroll = outputs
            .iter()
            .filter_map(|o| match o {
                Output::Inject(InjectEvent::TouchpadScroll { wheel, .. }) => Some(wheel.phase),
                _ => None,
            })
            .collect();
        (contacts, scroll)
    }

    /// Two fingers scrolling with momentum, then a three-finger touch: what B
    /// outputs.
    fn scroll_then_three_fingers(g: &mut [Router]) -> Vec<Output> {
        let p = &mut g[2];
        // A touch never starts mid-way at a new target: the lift arms contacts.
        p.member_input(MemberInput::Contacts(sample(5_000, 0, 0)), 2);
        p.member_input(MemberInput::Contacts(sample(10_000, 2, 1000)), 2);
        p.member_input(scroll(ScrollPhase::Began, 4.), 2);
        p.member_input(MemberInput::Contacts(sample(18_000, 2, 1100)), 3);
        p.member_input(scroll(ScrollPhase::Changed, 4.), 3);
        p.member_input(MemberInput::Contacts(sample(26_000, 0, 0)), 4);
        p.member_input(scroll(ScrollPhase::Ended, 0.), 4);
        p.member_input(scroll(ScrollPhase::MomentumBegan, 3.), 5);
        p.member_input(scroll(ScrollPhase::Momentum, 2.), 6);
        p.member_input(scroll(ScrollPhase::MomentumEnded, 0.), 7);
        let (_, outputs) = settle(g, 7);
        let mut b = outputs.into_iter().nth(1).unwrap();
        let p = &mut g[2];
        p.member_input(MemberInput::Contacts(sample(1_000_000, 3, 1000)), 8);
        p.member_input(MemberInput::Contacts(sample(1_008_000, 3, 1500)), 9);
        p.member_input(MemberInput::Contacts(sample(1_016_000, 0, 0)), 10);
        let (_, more) = settle(g, 10);
        b.extend(more.into_iter().nth(1).unwrap());
        b
    }

    #[test]
    fn a_phone_scrolls_a_windows_target_like_a_mac_trackpad() {
        let mut g = phone_controls_windows_b(true);
        let (contacts, scroll) = injected(&scroll_then_three_fingers(&mut g));
        // The two-finger scroll is the source's own phased scroll with its
        // momentum (synthetic precision-touchpad pan); its contacts are not
        // injected. The three-finger touch is native contacts.
        assert_eq!(
            scroll,
            [
                ScrollPhase::Began,
                ScrollPhase::Changed,
                ScrollPhase::Ended,
                ScrollPhase::MomentumBegan,
                ScrollPhase::Momentum,
                ScrollPhase::MomentumEnded
            ]
        );
        assert_eq!(contacts, [Phase::Begin, Phase::Move, Phase::End]);
    }

    #[test]
    fn a_source_not_known_as_a_trackpad_source_keeps_the_legacy_contacts_path() {
        let mut g = phone_controls_windows_b(false);
        let (contacts, scroll) = injected(&scroll_then_three_fingers(&mut g));
        assert!(scroll.is_empty(), "native contacts only: {scroll:?}");
        assert_eq!(contacts.iter().filter(|p| **p == Phase::Begin).count(), 2);
    }

    #[test]
    fn a_phone_gesture_is_replayed_on_a_mac_target() {
        let mut g = group();
        g[1].cfg.os = Os::MacOS;
        g[2].set_selected_target(Some(B));
        let gestures = [
            Gesture::DockSwipe {
                axis: SwipeAxis::Vertical,
                phase: GesturePhase::Began,
                progress: -0.1,
                velocity: 0.,
            },
            Gesture::DockSwipe {
                axis: SwipeAxis::Vertical,
                phase: GesturePhase::Ended,
                progress: -0.6,
                velocity: -2.,
            },
            Gesture::SmartMagnify,
        ];
        for gesture in gestures {
            assert!(g[2].member_input(MemberInput::Gesture(gesture), 1));
        }
        let (_, outputs) = settle(&mut g, 1);
        let replayed: Vec<Gesture> = outputs[1]
            .iter()
            .filter_map(|o| match o {
                Output::Inject(InjectEvent::Gesture(g)) => Some(*g),
                _ => None,
            })
            .collect();
        assert_eq!(replayed, gestures);
    }

    #[test]
    fn a_resting_finger_chooses_no_target() {
        let mut g = group();
        g[2].set_selected_target(Some(B));
        assert!(!g[2].member_input(MemberInput::Contacts(sample(0, 2, 1000)), 1));
        let (sent, _) = settle(&mut g, 1);
        assert!(sent.is_empty(), "{sent:?}");
    }
}

/// The control page's volume keys (user 2026-11-12): they act on the group's
/// speaker exit exactly like the same keys on a desktop source that forwards
/// its audio, never as ordinary keys for the focus computer.
mod volume_keys {
    use super::*;
    use crate::router::test_support::volume_route;

    const UP: KeyCode = KeyCode::new(0x0c, 0xe9);
    const MUTE: KeyCode = KeyCode::new(0x0c, 0xe2);

    /// (pressed, repeat, modifiers) of every `Volume` `from` sent to `to`.
    fn volume(sent: &Delivered, from: DeviceId, to: DeviceId) -> Vec<(bool, bool, u8)> {
        sent.iter()
            .filter_map(|(f, t, msg)| match msg {
                Message::Input(InputMsg::Volume {
                    code,
                    pressed,
                    repeat,
                    modifiers,
                    ..
                }) if *f == from && *t == to && code.is_volume() => Some((*pressed, *repeat, *modifiers)),
                _ => None,
            })
            .collect()
    }

    /// (source device, pressed, repeat) of every key the exit executes.
    fn executed(outputs: &[Output]) -> Vec<(DeviceId, bool, bool)> {
        outputs
            .iter()
            .filter_map(|o| match o {
                Output::ExitVolumeKey {
                    source,
                    pressed,
                    repeat,
                    ..
                } => Some((source.device_id, *pressed, *repeat)),
                _ => None,
            })
            .collect()
    }

    /// Whether `from` sent a volume usage as an ordinary key (or its repeat).
    fn plain_volume_key(sent: &Delivered, from: DeviceId) -> bool {
        sent.iter().any(|(f, _, msg)| {
            *f == from
                && matches!(
                    msg,
                    Message::Input(InputMsg::Key { code, .. } | InputMsg::KeyRepeat { code, .. }) if code.is_volume()
                )
        })
    }

    /// Focus on A (the phone joined it by selecting A), exit B.
    fn focus_on_a_exit_b() -> Vec<Router> {
        let mut g = group();
        g[2].set_selected_target(Some(A));
        assert!(g[2].member_input(MemberInput::Motion { dx: 1., dy: 0. }, 1));
        let _ = settle(&mut g, 1);
        assert_eq!(g[2].member_target(), Some(A));
        g[2].set_volume_exit_probe(Arc::new(|| Some(volume_route(P, B))));
        g
    }

    #[test]
    fn a_phone_volume_key_changes_the_exit_volume_not_the_focus_computer() {
        let mut g = focus_on_a_exit_b();
        // Press, two phone-generated repeats (presses while down), release.
        for pressed in [true, true, true, false] {
            assert!(g[2].member_input(MemberInput::Key { code: UP, pressed }, 2));
        }
        let (sent, outputs) = settle(&mut g, 2);
        assert_eq!(
            volume(&sent, P, B),
            [(true, false, 0), (true, true, 0), (true, true, 0), (false, false, 0)]
        );
        assert!(
            !plain_volume_key(&sent, P),
            "nothing goes to the focus computer: {sent:?}"
        );
        assert!(
            !outputs[0].iter().any(|o| matches!(o, Output::Inject(_))),
            "the focus computer injects nothing"
        );
        assert_eq!(
            executed(&outputs[1]),
            [(P, true, false), (P, true, true), (P, true, true), (P, false, false)]
        );

        // The same key on desktop A, the focus, forwarding its audio to B.
        let mut g = focus_on_a_exit_b();
        g[0].set_volume_exit_probe(Arc::new(|| Some(volume_route(A, B))));
        g[0].local_key(UP, true, 3);
        g[0].local_key(UP, true, 3);
        g[0].local_key(UP, false, 3);
        let (sent, outputs) = settle(&mut g, 3);
        assert_eq!(
            volume(&sent, A, B),
            [(true, false, 0), (true, true, 0), (false, false, 0)]
        );
        assert_eq!(
            executed(&outputs[1]),
            [(A, true, false), (A, true, true), (A, false, false)]
        );
    }

    #[test]
    fn mute_reaches_the_exit_and_held_modifiers_do_not_make_it_a_gain_key() {
        let mut g = focus_on_a_exit_b();
        // A modifier locked on the control page stays with the focus computer.
        assert!(g[2].member_input(
            MemberInput::Key {
                code: KeyCode::LEFT_GUI,
                pressed: true
            },
            2
        ));
        for pressed in [true, false] {
            assert!(g[2].member_input(MemberInput::Key { code: MUTE, pressed }, 2));
        }
        let (sent, outputs) = settle(&mut g, 2);
        assert_eq!(volume(&sent, P, B), [(true, false, 0), (false, false, 0)]);
        assert!(
            !outputs[2]
                .iter()
                .any(|o| matches!(o, Output::Status(StatusEvent::AudioVolumeKey { .. }))),
            "a phone has no forwarding gain"
        );
        assert_eq!(executed(&outputs[1]), [(P, true, false), (P, false, false)]);
    }

    #[test]
    fn without_a_focus_the_key_still_reaches_the_exit_and_chooses_no_target() {
        let mut g = group();
        g[2].set_volume_exit_probe(Arc::new(|| Some(volume_route(P, B))));
        for pressed in [true, false] {
            assert!(g[2].member_input(MemberInput::Key { code: UP, pressed }, 1));
        }
        let (sent, outputs) = settle(&mut g, 1);
        assert_eq!(volume(&sent, P, B), [(true, false, 0), (false, false, 0)]);
        assert!(
            !outputs[2]
                .iter()
                .any(|o| matches!(o, Output::Status(StatusEvent::NoTarget)))
        );
        assert!(
            !sent
                .iter()
                .any(|(_, _, msg)| matches!(msg, Message::Control(ControlMsg::MovePointerEnter { .. }))),
            "a volume key never moves the pointer"
        );
        assert_eq!(g[2].member_target(), None);
    }

    #[test]
    fn without_an_exit_the_key_stays_an_ordinary_key_for_the_focus() {
        let mut g = group();
        g[2].set_selected_target(Some(A));
        g[2].set_volume_exit_probe(Arc::new(|| None));
        for pressed in [true, false] {
            assert!(g[2].member_input(MemberInput::Key { code: UP, pressed }, 1));
        }
        let (sent, outputs) = settle(&mut g, 1);
        assert!(volume(&sent, P, B).is_empty());
        assert!(plain_volume_key(&sent, P));
        assert!(
            outputs[0]
                .iter()
                .any(|o| matches!(o, Output::Inject(InjectEvent::Volume { .. } | InjectEvent::Key { .. })))
        );
    }

    #[test]
    fn an_exit_context_for_another_boot_leaves_the_key_with_the_focus() {
        let mut g = group();
        g[2].set_selected_target(Some(A));
        // A context for an old Boot of B is not B's admitted instance.
        g[2].set_volume_exit_probe(Arc::new(|| {
            let mut route = volume_route(P, B);
            route.target.nonce = [77; 16];
            Some(route)
        }));
        assert!(g[2].member_input(
            MemberInput::Key {
                code: UP,
                pressed: true
            },
            1
        ));
        let (sent, _) = settle(&mut g, 1);
        assert!(volume(&sent, P, B).is_empty());
        assert!(plain_volume_key(&sent, P));
    }

    #[test]
    fn a_phone_that_is_the_exit_executes_its_own_keys() {
        let mut g = focus_on_a_exit_b();
        g[2].set_volume_exit_probe(Arc::new(|| Some(volume_route(P, P))));
        for pressed in [true, true, false] {
            assert!(g[2].member_input(MemberInput::Key { code: UP, pressed }, 2));
        }
        let (sent, outputs) = settle(&mut g, 2);
        assert!(volume(&sent, P, A).is_empty() && volume(&sent, P, B).is_empty());
        assert!(!plain_volume_key(&sent, P));
        assert_eq!(
            executed(&outputs[2]),
            [(P, true, false), (P, true, true), (P, false, false)]
        );
    }

    #[test]
    fn a_phone_exit_executes_the_keys_a_forwarding_focus_sends_it() {
        let mut g = focus_on_a_exit_b();
        g[0].set_volume_exit_probe(Arc::new(|| Some(volume_route(A, P))));
        g[0].local_key(MUTE, true, 3);
        g[0].local_key(MUTE, false, 3);
        let (sent, outputs) = settle(&mut g, 3);
        assert_eq!(volume(&sent, A, P), [(true, false, 0), (false, false, 0)]);
        assert_eq!(executed(&outputs[2]), [(A, true, false), (A, false, false)]);
    }
}

#[test]
fn privileged_phone_crosses_edges_both_ways_with_shared_topology() {
    let mut g = group();
    let phone_key = oink_foundation::display::ANDROID_INPUT_DISPLAY;
    let pc = display("a", Rect::new(0., 0., 1000., 800.));
    let phone = display(phone_key, Rect::new(0., 0., 400., 800.));
    let mut layout = oink_display::layout::Layout::auto(&[(A, vec![pc]), (P, vec![phone.clone()])]);
    layout.place_device(A, Point::default(), 0.001, true);
    layout.place_device(P, Point::new(1., 0.), 0.001, true);
    let topology = Arc::new(crate::TopologyLayout::from_layout(&layout, &Default::default()));
    for router in &mut g {
        router.set_layout(topology.clone());
        if router.me() != P {
            router.set_peer_os(P, Os::Android, 0);
            router.set_peer_displays(P, vec![phone.clone()]);
        }
        router.cfg.settings.damping.platform_profile = crate::damping::DampingProfile::DepthOnly { depth: 0. };
    }
    g[2].set_local_displays(vec![phone]);
    settle(&mut g, 1);
    assert!(!g[2].screen_focus_active(), "topology import never claims focus");
    g[0].set_cursor(Point::new(999., 400.));
    g[0].local_motion(10., 0., Some(Point::new(999., 400.)), 100);
    let (sent, _) = settle(&mut g, 100);
    assert!(sent.iter().any(|(_, to, msg)| *to == P
        && matches!(msg,
        Message::Control(ControlMsg::FocusEnter { display, .. }) if display == phone_key)));
    assert!(g[2].screen_focus_active());
    assert_eq!(g[0].focus().holder, P);
    // The phone's virtual pointer uses ordinary routing and can hand focus back.
    g[2].pointer = Some(ScreenPos::new(ScreenId::new(P, phone_key), Point::new(1., 400.)));
    g[2].remote_motion(A, -0.01, 0., 100_000);
    settle(&mut g, 100_000);
    assert_eq!(g[0].focus().holder, A);
    assert!(!g[2].screen_focus_active());
    // An unavailable phone is a wall in an already compiled topology.
    g[0].set_layout(Arc::new(
        crate::TopologyLayout::from_layout(&layout, &Default::default()).with_online([A].into_iter().collect()),
    ));
    g[0].set_cursor(Point::new(999., 400.));
    g[0].local_motion(10., 0., Some(Point::new(999., 400.)), 200_000);
    settle(&mut g, 200_000);
    assert_eq!(g[0].focus().holder, A);
}
