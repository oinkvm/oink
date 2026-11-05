//! Virtual clocks only; no native input, device locks, or installed applications.
use super::*;
const A: DeviceId = DeviceId([1; 32]);
const B: DeviceId = DeviceId([2; 32]);
fn pair() -> [Router; 2] {
    let mut nodes = [A, B].map(|device_id| {
        Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id,
                    nonce: [device_id.0[0]; 16],
                },
                "test",
                Os::Other,
            ),
            connection_tests::session_layout(A, B),
        )
    });
    nodes[0].session = [1; 16];
    nodes[1].session = [2; 16];
    for (i, peer) in [(0, B), (1, A)] {
        nodes[i].test_connect(peer, 0);
        nodes[i].peers.get_mut(&peer).unwrap().focus_ready = true;
        nodes[i].focus_joined = true;
        nodes[i].fresh_join = false;
        nodes[i].epoch = 1;
        nodes[i].focus_scope = Some(nodes[i].new_focus_scope());
        nodes[i].drain_outputs();
    }
    nodes
}
fn focus_record(router: &Router, to: DeviceId) -> Message {
    router
        .heartbeat_messages(to)
        .into_iter()
        .find(|m| matches!(m, Message::Control(ControlMsg::FocusState { .. })))
        .unwrap()
}
fn deliver(from: &mut Router, to: &mut Router, now: Micros) -> usize {
    let mut confirms = 0;
    for output in from.drain_outputs() {
        if let Output::Send { to: recipient, msg, .. } = output
            && recipient == to.me()
        {
            confirms += usize::from(matches!(msg, Message::Control(ControlMsg::FocusState { .. })));
            to.input_message(from.me(), msg, now);
        }
    }
    confirms
}
#[test]
fn old_1500ms_window_and_new_two_second_focus_gossip_confirm_immediately() {
    let [mut a, mut b] = pair();
    a.epoch = 2;
    a.holder = B;
    a.claim = B;
    b.holder = B;
    b.claim = B;
    // A knows the handoff, but B's pre-handoff state was already in flight.
    a.input_message(B, focus_record(&b, A), 100);
    assert_eq!(a.unconfirmed_since, Some(100));
    assert_eq!(deliver(&mut a, &mut b, 200), 1);
    assert_eq!(b.epoch, 2);
    assert_eq!(
        deliver(&mut b, &mut a, 300),
        1,
        "accepting the correction must confirm before the next 2 s gossip"
    );
    assert_eq!(a.unconfirmed_since, None);
    // Project the exact old receiver deadline without adding any legacy code to
    // production: old tick islands iff unconfirmed_since + 1.5 s has elapsed.
    for now in [1_500_099, 1_500_100, 2_000_000] {
        assert!(!a.unconfirmed_since.is_some_and(|since| now >= since + 1_500_000));
        a.tick(now);
        assert_eq!(a.focus().holder, B);
    }
    for now in [2_000_000, 4_000_000] {
        b.input_message(A, focus_record(&a, B), now);
        assert_eq!(
            deliver(&mut b, &mut a, now),
            0,
            "equal periodic claims must not form a reply loop"
        );
    }
}
#[test]
fn new_receiver_accepts_old_half_second_gossip_without_replies_or_focus_expiry() {
    let [mut a, mut b] = pair();
    a.epoch = 2;
    a.holder = B;
    a.claim = B;
    b.epoch = 2;
    b.holder = B;
    b.claim = B;
    for now in (500_000..=6_000_000).step_by(500_000) {
        a.input_message(B, focus_record(&b, A), now);
        a.tick(now);
        assert_eq!(a.focus().holder, B);
        assert_eq!(a.unconfirmed_since, None);
        assert_eq!(deliver(&mut a, &mut b, now), 0);
    }
}
#[test]
fn focus_enter_confirms_once_and_game_and_keys_still_send_immediately() {
    let [mut a, mut b] = pair();
    let enter = Message::Control(ControlMsg::FocusEnter {
        epoch: 2,
        display: "screen".into(),
        pos: Point::new(20., 20.),
        scope: Some(a.new_focus_scope()),
        pointer_source: Some(A),
    });
    b.input_message(A, enter.clone(), 100);
    assert_eq!(deliver(&mut b, &mut a, 100), 1);
    b.input_message(A, enter, 200);
    assert_eq!(deliver(&mut b, &mut a, 200), 0);
    b.set_game_mode(GameMode::On);
    assert!(b.drain_outputs().iter().any(|o| matches!(
        o,
        Output::Send {
            msg: Message::Control(ControlMsg::GameMode { active: true, .. }),
            ..
        }
    )));
    a.holder = B;
    a.claim = B;
    let code = KeyCode::keyboard(4);
    for (pressed, now) in [(true, 300), (false, 400)] {
        a.on_local_event(
            &CapturedEvent {
                time_us: now,
                physical: true,
                source: SourceKind::Keyboard,
                cursor: None,
                event: InputEvent::Key { code, pressed },
            },
            now,
        );
        assert!(a.drain_outputs().iter().any(|o| matches!(o, Output::Send { msg: Message::Input(InputMsg::Key { pressed: value, .. }), .. } if *value == pressed)));
        assert!(
            a.heartbeat_messages(B).iter().any(
                |m| matches!(m, Message::Input(InputMsg::HeldSync { keys, .. }) if keys.contains(&code) == pressed)
            )
        );
    }
}
#[test]
fn idle_router_has_no_periodic_send_timer_and_snapshot_registration_sends_nothing() {
    let [mut a, _] = pair();
    assert_eq!(a.sync_deadline(), None);
    for now in [500_000, 1_000_000, 2_000_000, 4_000_000] {
        a.tick(now);
        let first = a.heartbeat_messages(B);
        assert!(
            first
                .iter()
                .any(|m| matches!(m, Message::Input(InputMsg::HeldSync { .. })))
        );
        assert!(a.drain_outputs().iter().all(|o| !matches!(
            o,
            Output::Send {
                msg: Message::Control(ControlMsg::FocusState { .. } | ControlMsg::GameMode { .. })
                    | Message::Input(InputMsg::HeldSync { .. }),
                ..
            }
        )));
    }
}

#[test]
fn lost_up_waits_for_next_two_second_snapshot_without_receiver_expiry() {
    let [mut a, mut b] = pair();
    a.holder = B;
    a.claim = B;
    b.claim = B;
    let code = KeyCode::keyboard(4);
    for (pressed, now) in [(true, 1), (false, 2_000_001)] {
        a.on_local_event(
            &CapturedEvent {
                time_us: now,
                physical: true,
                source: SourceKind::Keyboard,
                cursor: None,
                event: InputEvent::Key { code, pressed },
            },
            now,
        );
        if pressed {
            deliver(&mut a, &mut b, now);
        } else {
            a.drain_outputs();
        }
    }
    assert!(b.held[&A].keys.contains_key(&code));
    a.tick(3_999_999);
    b.tick(3_999_999);
    assert!(
        b.held[&A].keys.contains_key(&code),
        "missing snapshots never expire a held key"
    );
    assert!(a.drain_outputs().iter().all(|o| !matches!(
        o,
        Output::Send {
            msg: Message::Input(InputMsg::HeldSync { .. }),
            ..
        }
    )));
    for message in a.heartbeat_messages(B) {
        b.input_message(A, message, 4_000_000);
    }
    assert!(!b.held.get(&A).is_some_and(|held| held.keys.contains_key(&code)));
    assert!(b.drain_outputs().iter().any(
        |o| matches!(o, Output::Inject(InjectEvent::Key { code: released, pressed: false }) if *released == code)
    ));
}
