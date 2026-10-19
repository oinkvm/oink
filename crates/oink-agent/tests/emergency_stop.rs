#![cfg(debug_assertions)]
//! Emergency delivery across two real loopback Nodes with fake OS backends.
//! Pause is Boot-local; resuming one recipient never resumes the origin.

use oink_agent as test_agent;
mod common;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oink_agent::api::{AgentApi, AlertKind, ApiEvent};
use oink_agent::device_cards::{DeviceControlReply, DeviceControlResult};
use oink_agent::node::{Node, NodeOptions};
use oink_foundation::display::DisplayInfo;
use oink_foundation::input::{CapturedEvent, InputEvent, KeyCode, SourceKind};
use oink_foundation::{Point, Rect, Size};
use oink_protocol::v1::dto;
use oink_test_support::clipboard::FakeClipboard;
use oink_test_support::input::FakeInput;

fn display(key: &str) -> DisplayInfo {
    DisplayInfo {
        key: key.into(),
        name: key.into(),
        bounds: Rect::new(0.0, 0.0, 1600.0, 1000.0),
        pixel_width: 1600,
        pixel_height: 1000,
        reference_extent: Some(Size::new(400.0 / 686.0, 250.0 / 686.0)),
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }
}

struct Dev {
    node: Node,
    input: FakeInput,
    events: Arc<Mutex<Vec<ApiEvent>>>,
    _dir: tempfile::TempDir,
}

async fn start(name: &str, port: u16) -> Dev {
    let dir = tempfile::tempdir().unwrap();
    let input = FakeInput::new();
    input.set_cursor(Point::new(800.0, 500.0));
    let node = Node::start_with_fake_platform(NodeOptions {
        net: oink_test_support::network_config(dir.path().to_owned(), name, port),
        input: Arc::new(input.clone()),
        displays: vec![display(name)],
        display_backend: None,
        clipboard: Some(Arc::new(FakeClipboard::new())),
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        cursor_trail: false,
    })
    .await
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    node.subscribe(Arc::new(move |ev: &ApiEvent| {
        if matches!(ev, ApiEvent::EmergencyStop(_)) {
            sink.lock().unwrap().push(ev.clone());
        }
    }));
    Dev {
        node,
        input,
        events,
        _dir: dir,
    }
}

async fn wait_for(what: &str, secs: f64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn api<T: Send + 'static>(n: &Node, f: impl FnOnce(&Node) -> T + Send + 'static) -> T {
    let n = n.clone();
    tokio::task::spawn_blocking(move || f(&n)).await.unwrap()
}

async fn resume(n: &Node, request_id: &'static str) {
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let subscription = n.subscribe(Arc::new(move |event| {
        if let ApiEvent::DeviceControlCompleted(completed) = event
            && completed.request_id == request_id
        {
            let _ = send.send(completed.clone());
        }
    }));
    let reply = api(n, move |n| n.set_local_paused(request_id.into(), false))
        .await
        .unwrap();
    let completed = match reply {
        DeviceControlReply::Completed(completed) => completed,
        DeviceControlReply::Accepted(_) => tokio::time::timeout(Duration::from_secs(3), receive.recv())
            .await
            .expect("local resume must complete")
            .unwrap(),
    };
    n.unsubscribe(subscription);
    assert_eq!(completed.result, DeviceControlResult::Applied, "{completed:?}");
}

fn key(input: &FakeInput, code: KeyCode, pressed: bool) {
    input.feed(&CapturedEvent {
        physical: true,
        event: InputEvent::Key { code, pressed },
        source: SourceKind::Keyboard,
        time_us: 0,
        cursor: None,
    });
}

fn stopped_by(n: &Node) -> Option<String> {
    let o = n.overview().unwrap();
    o.alerts.iter().find_map(|a| match &a.kind {
        AlertKind::EmergencyStop { device } => Some(device.0.to_hex()),
        _ => None,
    })
}

#[test]
fn long_press_esc_pauses_group_and_resume_is_local() {
    common::watchdog::run(
        module_path!(),
        "long_press_esc_pauses_group_and_resume_is_local",
        Some(4),
        long_press_esc_pauses_group_and_resume_is_local_scenario(),
    );
}

async fn long_press_esc_pauses_group_and_resume_is_local_scenario() {
    oink_agent::logs::init();
    let a = start("alpha", 0).await;
    let b = start("beta", 0).await;

    // 配对（同 two_nodes.rs）。
    common::pair(&a.node, &b.node).await;
    let (an, bn) = (a.node.clone(), b.node.clone());
    wait_for("both online", 10.0, || {
        an.online_set().contains(&bn.id()) && bn.online_set().contains(&an.id())
    })
    .await;
    assert!(an.overview().unwrap().sharing_enabled);

    wait_for("both capture owners ready", 3.0, || {
        [&a, &b]
            .iter()
            .all(|d| oink_input::InputBackend::capture_active(&d.input) == Some(true))
    })
    .await;
    // A 的键盘上按住 Esc。
    key(&a.input, KeyCode::ESCAPE, true);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(an.overview().unwrap().sharing_enabled, "not before the hold time");
    wait_for("A paused", 7.0, || !an.overview().unwrap().sharing_enabled).await;
    wait_for("B paused", 3.0, || !bn.overview().unwrap().sharing_enabled).await;
    wait_for("pause teardown completed", 3.0, || {
        !an.pause_status().settling && !bn.pause_status().settling
    })
    .await;
    key(&a.input, KeyCode::ESCAPE, false);

    let a_hex = an.id().to_hex();
    for n in [&an, &bn] {
        assert_eq!(stopped_by(n).as_deref(), Some(a_hex.as_str()));
        assert!(n.pause_status().emergency);
        assert_eq!(n.pause_status().author_name, Some(an.device_name(an.id())));
    }
    for d in [&a, &b] {
        assert!(d.node.pause_status().capture_detached);
        assert!(!d.input.injection_active());
        assert_eq!(oink_input::InputBackend::capture_active(&d.input), Some(false));
    }
    // 事件：A 是本机触发（界面把窗口弹到前台），B 不是。
    let ev = |d: &Dev| {
        d.events.lock().unwrap().iter().find_map(|e| match e {
            ApiEvent::EmergencyStop(s) => Some(s.local),
            _ => None,
        })
    };
    assert_eq!(ev(&a), Some(true));
    assert_eq!(ev(&b), Some(false));

    let b_revision = bn.pause_status().revision;
    resume(&bn, "00000000-0000-4000-8000-000000000001").await;
    wait_for("B resumed locally", 3.0, || !bn.pause_status().paused).await;
    let resumed = bn.pause_status();
    assert!(!resumed.emergency && !resumed.settling);
    assert_eq!(resumed.author, None);
    assert_eq!(resumed.author_name, None);
    assert_eq!(resumed.revision, b_revision + 1);
    wait_for("B resume fact published to paused A", 3.0, || {
        an.device_facts(bn.id())
            .and_then(|facts| facts.pause().ok().flatten())
            .is_some_and(|pause| {
                pause.paused() == Ok(false)
                    && pause.reason() == Ok(dto::PauseReason::None)
                    && pause.revision() == Ok(b_revision + 1)
            })
    })
    .await;
    assert!(an.pause_status().paused);
    assert!(an.pause_status().emergency);
    assert_eq!(stopped_by(&bn), None);
    assert_eq!(a.events.lock().unwrap().len(), 1, "recipient must not rebroadcast");
    assert_eq!(b.events.lock().unwrap().len(), 1);
    resume(&an, "00000000-0000-4000-8000-000000000002").await;
    let resumed = an.pause_status();
    assert!(!resumed.paused && !resumed.emergency && !resumed.settling);
    assert_eq!(resumed.author, None);
    common::active(&[&an, &bn]).await;

    a.node.shutdown().await;
    b.node.shutdown().await;
}
