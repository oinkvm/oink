//! Virtual clocks and input-only delivery: no native capture or injection.
use super::*;

const SOURCE: DeviceId = DeviceId([1; 32]);
const TARGET: DeviceId = DeviceId([2; 32]);
const THIRD: DeviceId = DeviceId([3; 32]);
const TAB: KeyCode = KeyCode::keyboard(0x2b);
const LETTER: KeyCode = KeyCode::keyboard(4);

fn nodes(ids: &[DeviceId]) -> Vec<Router> {
    ids.iter()
        .map(|&id| {
            let mut router = Router::new(
                RouterConfig::new(
                    oink_foundation::session::Boot {
                        device_id: id,
                        nonce: [id.0[0]; 16],
                    },
                    "test",
                    if id == SOURCE { Os::MacOS } else { Os::Windows },
                ),
                Arc::new(crate::SimpleLayout::new()),
            );
            router.session = [id.0[0]; 16];
            for &peer in ids.iter().filter(|&&peer| peer != id) {
                router.test_connect(peer, 0);
                router.peers.get_mut(&peer).unwrap().focus_ready = true;
            }
            router.focus_joined = true;
            router.fresh_join = false;
            router.epoch = 1;
            router.focus_scope = Some(router.new_focus_scope());
            router.drain_outputs();
            router
        })
        .collect()
}

fn focus(router: &mut Router, holder: DeviceId) {
    // Deliberately control when each peer learns the handoff. The harness only
    // delivers input, so control gossip cannot hide the stale-view reflection.
    router.holder = holder;
    router.claim = holder;
}

fn key(router: &mut Router, code: KeyCode, pressed: bool, now: Micros) {
    capture(router, InputEvent::Key { code, pressed }, now);
}

fn capture(router: &mut Router, event: InputEvent, now: Micros) {
    router.on_local_event(
        &CapturedEvent {
            time_us: now,
            physical: true,
            source: SourceKind::Keyboard,
            cursor: None,
            event,
        },
        now,
    );
}

fn flush(nodes: &mut [Router], now: Micros) -> Vec<(DeviceId, InjectEvent)> {
    flush_filter(nodes, now, |_, _| true)
}

fn flush_filter(
    nodes: &mut [Router],
    now: Micros,
    deliver: impl Fn(DeviceId, &InputMsg) -> bool,
) -> Vec<(DeviceId, InjectEvent)> {
    let mut injected = Vec::new();
    for _ in 0..16 {
        let mut sent = Vec::new();
        for router in nodes.iter_mut() {
            for output in router.drain_outputs() {
                match output {
                    Output::Send {
                        to,
                        msg: Message::Input(input),
                        ..
                    } => {
                        sent.push((router.me(), to, input));
                    }
                    Output::Inject(event) => injected.push((router.me(), event)),
                    _ => {}
                }
            }
        }
        if sent.is_empty() {
            return injected;
        }
        for (from, to, input) in sent {
            if !deliver(to, &input) {
                continue;
            }
            let receiver = nodes.iter_mut().find(|r| r.me() == to).unwrap();
            assert!(receiver.input_message(from, Message::Input(input), now));
        }
    }
    panic!("input forwarding did not terminate at its hop limit");
}

fn injected_key(device: DeviceId, code: KeyCode, pressed: bool) -> (DeviceId, InjectEvent) {
    (device, InjectEvent::Key { code, pressed })
}

fn reflected_history() -> Vec<Router> {
    let mut nodes = nodes(&[SOURCE, TARGET]);
    focus(&mut nodes[0], TARGET);
    focus(&mut nodes[1], SOURCE);
    // While focus views disagree, a prior press returns to its source and is
    // forwarded once more. Its UP clears the presses, retaining repair routes.
    key(&mut nodes[0], LETTER, true, 10);
    assert!(flush(&mut nodes, 10).is_empty());
    key(&mut nodes[0], LETTER, false, 20);
    assert!(flush(&mut nodes, 20).is_empty());
    assert!(nodes.iter().all(|r| r.forwarded.is_empty()));
    assert!(nodes[0].forwarded_sync.contains(&(SOURCE, TARGET)));
    focus(&mut nodes[1], TARGET);
    nodes
}

#[test]
fn reflected_history_does_not_release_direct_tab_on_periodic_sync() {
    let mut nodes = reflected_history();
    key(&mut nodes[0], TAB, true, 30);
    assert_eq!(flush(&mut nodes, 30), [injected_key(TARGET, TAB, true)]);
    // Hold without any repeats for three seconds, then forward slow native
    // repeats. Neither the hold nor a repeat depends on heartbeat cadence.
    for turn in 1..=32 {
        let now = turn * 250_000;
        for router in &mut nodes {
            router.tick(now);
        }
        assert!(
            flush(&mut nodes, now).is_empty(),
            "spurious release on sync turn {turn}"
        );
        if turn > 12 {
            key(&mut nodes[0], TAB, true, now + 1);
            assert_eq!(flush(&mut nodes, now + 1), [injected_key(TARGET, TAB, true)]);
        }
    }
    key(&mut nodes[0], TAB, false, 8_100_000);
    assert_eq!(flush(&mut nodes, 8_100_000), [injected_key(TARGET, TAB, false)]);
}

#[test]
fn reflected_history_preserves_direct_button_and_still_repairs_a_lost_up() {
    let mut nodes = reflected_history();
    let button = MouseButton::Right;
    capture(&mut nodes[0], InputEvent::Button { button, pressed: true }, 30);
    assert_eq!(
        flush(&mut nodes, 30),
        [(TARGET, InjectEvent::Button { button, pressed: true })]
    );
    for turn in 1..=12 {
        let now = turn * 250_000;
        for router in &mut nodes {
            router.tick(now);
        }
        assert!(flush(&mut nodes, now).is_empty());
    }
    capture(&mut nodes[0], InputEvent::Button { button, pressed: false }, 3_100_000);
    nodes[0].drain_outputs(); // Lose the direct UP; state repair must still release it.
    nodes[0].tick(3_250_000);
    assert_eq!(
        flush(&mut nodes, 3_250_000),
        [(TARGET, InjectEvent::Button { button, pressed: false })]
    );
}

#[test]
fn reflected_live_press_and_direct_press_share_one_source_snapshot() {
    let mut nodes = nodes(&[SOURCE, TARGET, THIRD]);
    focus(&mut nodes[0], TARGET);
    focus(&mut nodes[1], SOURCE);
    focus(&mut nodes[2], THIRD);
    key(&mut nodes[0], LETTER, true, 10); // Press's original destination is TARGET.
    focus(&mut nodes[0], THIRD); // Before the queued press returns via TARGET.
    assert_eq!(flush(&mut nodes, 10), [injected_key(THIRD, LETTER, true)]);
    key(&mut nodes[0], TAB, true, 20); // Direct route to the same receiver.
    assert_eq!(flush(&mut nodes, 20), [injected_key(THIRD, TAB, true)]);
    assert_eq!(nodes[0].forwarded[&(SOURCE, HeldItem::Key(LETTER))], THIRD);

    for turn in 1..=8 {
        let now = turn * 250_000;
        for router in &mut nodes {
            router.tick(now);
        }
        assert!(
            flush(&mut nodes, now).is_empty(),
            "disjoint snapshots on sync turn {turn}"
        );
    }
    // Preserve the original UP route for the reflected press and the direct UP.
    key(&mut nodes[0], LETTER, false, 2_100_000);
    assert_eq!(flush(&mut nodes, 2_100_000), [injected_key(THIRD, LETTER, false)]);
    key(&mut nodes[0], TAB, false, 2_100_010);
    assert_eq!(flush(&mut nodes, 2_100_010), [injected_key(THIRD, TAB, false)]);
}

#[test]
fn reflected_button_survives_empty_direct_state_and_repairs_a_lost_forwarded_up() {
    let mut nodes = nodes(&[SOURCE, TARGET, THIRD]);
    focus(&mut nodes[0], TARGET);
    focus(&mut nodes[1], SOURCE);
    focus(&mut nodes[2], THIRD);
    let button = MouseButton::Right;
    capture(&mut nodes[0], InputEvent::Button { button, pressed: true }, 10);
    focus(&mut nodes[0], THIRD);
    assert_eq!(
        flush(&mut nodes, 10),
        [(THIRD, InjectEvent::Button { button, pressed: true })]
    );
    for turn in 1..=12 {
        let now = turn * 250_000;
        for router in &mut nodes {
            router.tick(now);
        }
        assert!(flush(&mut nodes, now).is_empty());
    }
    capture(&mut nodes[0], InputEvent::Button { button, pressed: false }, 3_100_000);
    assert!(flush_filter(&mut nodes, 3_100_000, |to, _| to != THIRD).is_empty());
    assert!(nodes[0].forwarded.is_empty());
    // The same combined source snapshot owns reflected cleanup, including when
    // the physical source's original destination was a different peer.
    nodes[0].tick(3_250_000);
    assert_eq!(
        flush(&mut nodes, 3_250_000),
        [(THIRD, InjectEvent::Button { button, pressed: false })]
    );
}
