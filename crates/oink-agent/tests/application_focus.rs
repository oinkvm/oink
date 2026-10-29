#![cfg(debug_assertions)]
//! Real three-node QUIC streams, ordinary native-command bridge and fake OS.
//! These assertions do not claim native foreground/window behavior.
//! Host scheduling must not decide whether a 250ms park/entry attempt expires.
//! A shared manual clock drives both routing deadlines and native permits;
//! timeout scenarios advance it explicitly while a native sample is held.
use oink_agent as test_agent;
mod common;
use oink_agent::{
    api::AgentApi,
    node::{Node, NodeOptions},
    threads::input::InputCommand,
};
use oink_foundation::{
    DeviceId, Point, Rect, Size,
    application_focus::{Foreground, TestClock},
    display::DisplayInfo,
    input::{CapturedEvent, InjectEvent, InputEvent, KeyCode, SourceKind},
};
use oink_routing::application_focus::{Phase, Snapshot};
use oink_test_support::{clipboard::FakeClipboard, input::FakeInput};
use std::sync::Arc;

struct Dev {
    node: Node,
    input: FakeInput,
    _dir: tempfile::TempDir,
}
async fn start(name: &str, clock: &TestClock) -> Dev {
    let dir = tempfile::tempdir().unwrap();
    let input = FakeInput::new();
    input.set_application_focus_clock(clock.clone());
    input.set_cursor(Point::new(800., 500.));
    input.set_application_foreground(Foreground::Windowed);
    let node = Node::start_with_fake_platform(NodeOptions {
        net: oink_test_support::network_config(dir.path().to_owned(), name, 0),
        input: Arc::new(input.clone()),
        displays: vec![DisplayInfo {
            key: name.into(),
            name: name.into(),
            bounds: Rect::new(0., 0., 1600., 1000.),
            pixel_width: 1600,
            pixel_height: 1000,
            reference_extent: Some(Size::new(400. / 686., 250. / 686.)),
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        }],
        display_backend: None,
        clipboard: Some(Arc::new(FakeClipboard::new())),
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        cursor_trail: false,
    })
    .await
    .unwrap();
    Dev { node, input, _dir: dir }
}
fn state(d: &Dev) -> Option<Snapshot> {
    d.node.input_thread().unwrap().status().application_focus.clone()
}
async fn focus(source: &Dev, target: DeviceId, all: &[&Dev]) {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    source
        .node
        .input_thread()
        .unwrap()
        .send(InputCommand::LockFocusTo { target, reply: tx });
    assert!(
        tokio::task::spawn_blocking(move || rx.recv())
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
    common::watchdog::until("pointer claim convergence", || {
        all.iter()
            .all(|d| d.node.input_thread().unwrap().status().focus == Some(target))
    })
    .await;
}
fn feed(d: &Dev, code: KeyCode, pressed: bool) {
    d.input.feed(&CapturedEvent {
        time_us: 0,
        physical: true,
        source: SourceKind::Keyboard,
        cursor: None,
        event: InputEvent::Key { code, pressed },
    });
}
fn keys(d: &Dev) -> Vec<(KeyCode, bool)> {
    d.input
        .take_injected()
        .into_iter()
        .filter_map(|e| {
            if let InjectEvent::Key { code, pressed } = e {
                Some((code, pressed))
            } else {
                None
            }
        })
        .collect()
}
async fn collect(d: &Dev, count: usize) -> Vec<(KeyCode, bool)> {
    let mut got = Vec::new();
    common::watchdog::until("injected key delivery", || {
        got.extend(keys(d));
        got.len() >= count
    })
    .await;
    got
}

async fn set_enabled(controller: &Dev, all: &[&Dev], enabled: bool) {
    let node = controller.node.clone();
    tokio::task::spawn_blocking(move || {
        node.set_input_settings(
            serde_json::from_value(serde_json::json!({
                "application_focus_enabled": enabled
            }))
            .unwrap(),
        )
    })
    .await
    .unwrap()
    .unwrap();
    common::watchdog::until("group application-focus setting converges", || {
        all.iter()
            .all(|d| d.node.input_settings().unwrap().application_focus_enabled == enabled)
    })
    .await;
}

#[test]
fn application_focus_three_node_quic_delivery_and_retirement() {
    common::watchdog::run(
        module_path!(),
        "application_focus_three_node_quic_delivery_and_retirement",
        Some(4),
        scenario(),
    );
}
async fn scenario() {
    let _load = CpuLoad::from_env();
    let clock = TestClock::default();
    let [a, b, c] = ready_group(&clock).await;
    let all = [&a, &b, &c];
    assert!(
        all.iter()
            .all(|d| !d.node.input_settings().unwrap().application_focus_enabled)
    );
    // One CLI/API edit is replicated as a group register, not N owner transactions.
    set_enabled(&a, &all, true).await;
    common::watchdog::until("prepare parks non-keyboard owners", || {
        all.iter()
            .all(|d| state(d).is_some_and(|s| s.phase == Phase::Active && s.keyboard == Some(a.node.boot())))
            && b.input.application_foreground() == Some(Foreground::Hidden)
            && c.input.application_foreground() == Some(Foreground::Hidden)
    })
    .await;
    focus(&a, b.node.id(), &all).await;
    common::watchdog::until("hidden pointer keeps keyboard", || {
        all.iter().all(|d| {
            state(d).is_some_and(|s| {
                s.phase == Phase::Active && s.claim.pointer == b.node.boot() && s.keyboard == Some(a.node.boot())
            })
        })
    })
    .await;
    for d in all {
        keys(d);
    }
    let letter = KeyCode::keyboard(4);
    feed(&c, letter, true);
    assert_eq!(collect(&b, 1).await, vec![(letter, true)]);
    assert_eq!(collect(&a, 1).await, vec![(letter, true)]);
    // The ordinary observer installed by MainCommand captures its claim; the
    // fake backend emits through that callback, just like a native notification.
    b.input.set_application_foreground(Foreground::Windowed);
    common::watchdog::until("keyboard commit reaches all streams", || {
        all.iter()
            .all(|d| state(d).is_some_and(|s| s.phase == Phase::Active && s.keyboard == Some(b.node.boot())))
    })
    .await;
    assert_eq!(collect(&a, 1).await, vec![(letter, false)]);
    assert!(keys(&b).is_empty(), "P's existing press is not retired or replayed");
    feed(&c, letter, true); // same source OS repeat counter travels over QUIC
    feed(&c, letter, false);
    assert_eq!(collect(&b, 2).await, vec![(letter, true), (letter, false)]);
    assert!(keys(&a).is_empty(), "retired K receives no tails");
    // Any member can disable the whole group. Every native owner closes its sink.
    set_enabled(&c, &all, false).await;
    common::watchdog::until("group disable retires every participant", || {
        all.iter()
            .all(|d| state(d).is_none() && d.input.application_foreground() != Some(Foreground::Hidden))
    })
    .await;
    for d in all {
        let cards = serde_json::to_value(d.node.device_cards().unwrap()).unwrap();
        assert!(
            !cards.to_string().contains("application_focus"),
            "K stays internal, not in card status"
        );
    }
    // Closing a sink does not restore an app. Supply ordinary foregrounds for
    // the next parking scenario explicitly through the fake native observer.
    a.input.set_application_foreground(Foreground::Windowed);
    c.input.set_application_foreground(Foreground::Windowed);
    // Re-enable from a different member and check the one durable group choice.
    set_enabled(&b, &all, true).await;
    common::watchdog::until("new group setting reaches routing", || {
        all.iter()
            .all(|d| state(d).is_some_and(|s| s.phase == Phase::Active && s.keyboard == Some(b.node.boot())))
    })
    .await;
    for d in all {
        let stored = oink_agent::config_module::group_store::load(
            &d._dir.path().join(oink_agent::config_module::group_store::STATE_FILE),
            d.node.id(),
        )
        .unwrap()
        .unwrap();
        assert!(stored.application_focus_enabled().unwrap());
    }
    // Owner-mode facts travel over the ordinary configuration/input streams.
    // A local-mode K keeps receiving application keys without a native sink.
    use oink_agent::device_cards::CardAction;
    let card = b
        .node
        .device_cards()
        .unwrap()
        .cards
        .into_iter()
        .find(|card| card.is_self)
        .unwrap();
    let CardAction::Invoke { token, .. } = card.status.unwrap().actions.keyboard_local else {
        panic!("keyboard-local action must be available");
    };
    let node = b.node.clone();
    tokio::task::spawn_blocking(move || {
        node.invoke_device_card_action("28282828-2828-2828-2828-282828282828".into(), token)
    })
    .await
    .unwrap()
    .unwrap();
    common::watchdog::until("local K remains a participant", || {
        b.node
            .device_cards()
            .unwrap()
            .cards
            .iter()
            .find(|c| c.is_self)
            .and_then(|c| c.status.as_ref())
            .and_then(|s| s.keyboard.resource.configured_mode)
            == Some(oink_agent::device_cards::SharingMode::Local)
            && b.node.input_thread().unwrap().status().keyboard_mode == oink_foundation::input::InputMode::Local
            && all.iter().all(|d| {
                state(d).is_some_and(|s| {
                    s.phase == Phase::Active
                        && s.keyboard == Some(b.node.boot())
                        && s.participants.contains(&b.node.boot())
                })
            })
            && b.input.application_foreground() != Some(Foreground::Hidden)
    })
    .await;
    assert_eq!(a.input.application_foreground(), Some(Foreground::Hidden));
    focus(&b, a.node.id(), &all).await;
    common::watchdog::until("shared pointer preserves local K", || {
        all.iter().all(|d| {
            state(d).is_some_and(|s| {
                s.phase == Phase::Active && s.claim.pointer == a.node.boot() && s.keyboard == Some(b.node.boot())
            })
        })
    })
    .await;
    for d in all {
        keys(d);
    }
    feed(&c, letter, true);
    feed(&c, letter, false);
    assert_eq!(collect(&a, 2).await, vec![(letter, true), (letter, false)]);
    assert_eq!(collect(&b, 2).await, vec![(letter, true), (letter, false)]);
    a.input.set_application_foreground(Foreground::Windowed);
    common::watchdog::until("K leaves local endpoint without parking it", || {
        all.iter()
            .all(|d| state(d).is_some_and(|s| s.phase == Phase::Active && s.keyboard == Some(a.node.boot())))
    })
    .await;
    assert_ne!(b.input.application_foreground(), Some(Foreground::Hidden));
    let (entered, release) = a.input.block_next_application_sample();
    focus(&a, b.node.id(), &all).await;
    common::watchdog::until("old K sample is held during local entry", || entered.try_recv().is_ok()).await;
    // Local entry claims K without waiting for a native foreground sample.
    // Verify that decision before advancing time, so timeout fallback cannot
    // conceal an incorrect decision. Parking the old K may then time out.
    common::watchdog::until("entry into local endpoint claims K before timeout", || {
        all.iter()
            .all(|d| state(d).is_some_and(|s| s.claim.pointer == b.node.boot() && s.keyboard == Some(b.node.boot())))
    })
    .await;
    clock.advance(oink_routing::application_focus::PARK_TIMEOUT_US);
    for d in all {
        d.node.input_thread().unwrap().send(InputCommand::DiagnosticsSnapshot);
    }
    common::watchdog::until("local entry finishes preparation", || {
        all.iter()
            .all(|d| state(d).is_some_and(|s| s.phase == Phase::Active && s.keyboard == Some(b.node.boot())))
    })
    .await;
    drop(release);
    assert_ne!(b.input.application_foreground(), Some(Foreground::Hidden));
    c.node.shutdown().await;
    b.node.shutdown().await;
    a.node.shutdown().await;
}

async fn ready_group(clock: &TestClock) -> [Dev; 3] {
    let a = start("alpha", clock).await;
    let b = start("beta", clock).await;
    let c = start("gamma", clock).await;
    common::pair(&a.node, &b.node).await;
    common::pair(&a.node, &c.node).await;
    common::query(&b.node, &c.node).await;
    let all = [&a, &b, &c];
    common::watchdog::until("three input openings and layout", || {
        all.iter().all(|d| {
            let s = d.node.input_thread().unwrap().status();
            s.peers.iter().filter(|p| p.online).count() == 2 && d.node.layout().0.screens.len() == 3
        })
    })
    .await;
    focus(&a, a.node.id(), &all).await;
    [a, b, c]
}

#[test]
fn application_focus_preparation_timeout_allows_windowed_target() {
    common::watchdog::run(module_path!(), "preparation_timeout", Some(4), timeout_scenario(false));
}

#[test]
fn application_focus_entry_timeout_claims_pointer_despite_hidden_foreground() {
    common::watchdog::run(module_path!(), "entry_timeout", Some(4), timeout_scenario(true));
}

async fn timeout_scenario(entry: bool) {
    let _load = CpuLoad::from_env();
    let clock = TestClock::default();
    let [a, b, c] = ready_group(&clock).await;
    let all = [&a, &b, &c];
    if entry {
        set_enabled(&a, &all, true).await;
        common::watchdog::until("parked entry fixture ready", || {
            all.iter()
                .all(|d| state(d).is_some_and(|s| s.phase == Phase::Active && s.keyboard == Some(a.node.boot())))
                && b.input.application_foreground() == Some(Foreground::Hidden)
                && c.input.application_foreground() == Some(Foreground::Hidden)
        })
        .await;
    }
    let (entered, release) = b.input.block_next_application_sample();
    if entry {
        focus(&a, b.node.id(), &all).await;
    } else {
        set_enabled(&a, &all, true).await;
    }
    common::watchdog::until("native sample gate entered", || entered.try_recv().is_ok()).await;
    let pointer = if entry { &b } else { &a };
    common::watchdog::until("authority preparing while sample is blocked", || {
        state(pointer).is_some_and(|s| s.phase == Phase::Preparing && s.keyboard == Some(a.node.boot()))
    })
    .await;
    // This advances the *unchanged* product budget, rather than sleeping until
    // a loaded host happens to service an input/native thread in a certain order.
    clock.advance(oink_routing::application_focus::PARK_TIMEOUT_US);
    for d in all {
        d.node.input_thread().unwrap().send(InputCommand::DiagnosticsSnapshot);
    }
    common::watchdog::until("sample deadline selects the documented fallback", || {
        all.iter()
            .all(|d| state(d).is_some_and(|s| s.phase == Phase::Active && s.keyboard == Some(pointer.node.boot())))
    })
    .await;
    assert_eq!(
        b.input.application_foreground(),
        Some(if entry {
            Foreground::Hidden
        } else {
            Foreground::Windowed
        })
    );
    drop(release);
    set_enabled(&a, &all, false).await;
    common::watchdog::until("timeout fixture withdraws", || all.iter().all(|d| state(d).is_none())).await;
    c.node.shutdown().await;
    b.node.shutdown().await;
    a.node.shutdown().await;
}

// Opt-in host pressure for repeatable CI/load investigations; scoped to this
// fixture so the shared cargo lane never leaves stress workers behind.
struct CpuLoad {
    stop: Arc<std::sync::atomic::AtomicBool>,
    workers: Vec<std::thread::JoinHandle<u64>>,
}
impl CpuLoad {
    fn from_env() -> Self {
        let count = std::env::var("OINK_FOCUS_LOAD_THREADS")
            .map(|value| value.parse::<usize>().expect("numeric load thread count"))
            .unwrap_or(0);
        assert!(count <= 32, "bound the opt-in CPU load");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let workers = (0..count)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut iterations = 0;
                    let mut x = 1.01_f64;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        for _ in 0..1024 {
                            x = std::hint::black_box(x + 1.001).sqrt();
                        }
                        iterations += 1024;
                    }
                    std::hint::black_box(x);
                    iterations
                })
            })
            .collect();
        if count != 0 {
            eprintln!("FOCUS_LOAD start workers={count}");
        }
        Self { stop, workers }
    }
}
impl Drop for CpuLoad {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let count = self.workers.len();
        let iterations: u64 = self.workers.drain(..).map(|worker| worker.join().unwrap()).sum();
        if count != 0 {
            eprintln!("FOCUS_LOAD stop workers={count} iterations={iterations}");
        }
    }
}
