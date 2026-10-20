#![cfg(debug_assertions)]
//! Real authenticated network and production agent hooks, with FakeInput only.
//! This does not claim OS capture, injection, screen wake, or Wi-Fi validation.
use oink_agent as test_agent;
mod common;

use oink_agent::{
    api::AgentApi,
    node::{Node, NodeOptions},
};
use oink_foundation::input::{CapturedEvent, InputEvent, SourceKind};
use oink_input::InputBackend;
use oink_test_support::input::FakeInput;
use std::{path::Path, sync::Arc, time::Duration};

async fn wait(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(12), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out: {what}"));
}
async fn start(path: &Path, name: &str, port: u16) -> (Node, FakeInput) {
    let input = FakeInput::new();
    let node = Node::start_with_fake_platform(NodeOptions {
        net: oink_test_support::network_config(path.into(), name, port),
        input: Arc::new(input.clone()),
        displays: vec![],
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        cursor_trail: false,
    })
    .await
    .unwrap();
    (node, input)
}
fn motion(input: &FakeInput, physical: bool, dx: f64) {
    input.feed(&CapturedEvent {
        physical,
        event: InputEvent::PointerDelta { dx, dy: 0.0 },
        source: SourceKind::Mouse,
        time_us: 0,
        cursor: None,
    });
}

#[test]
fn physical_user_hooks_ignore_wake_setting_but_respect_full_pause() {
    common::watchdog::run(
        module_path!(),
        "physical_user_hooks_ignore_wake_setting_but_respect_full_pause",
        Some(4),
        physical_user_hooks_ignore_wake_setting_but_respect_full_pause_scenario(),
    );
}

async fn physical_user_hooks_ignore_wake_setting_but_respect_full_pause_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let (a, ai) = start(&dir.path().join("a"), "activity-a", 0).await;
    let (b, bi) = start(&dir.path().join("b"), "activity-b", 0).await;
    common::pair(&a, &b).await;
    for node in [&a, &b] {
        let mut settings = node.input_settings().unwrap();
        settings.wake_displays_on_input = false;
        node.set_input_settings(settings.into()).unwrap();
    }
    let sa = || a.low_pacing(b.id()).unwrap();
    let sb = || b.low_pacing(a.id()).unwrap();
    motion(&ai, false, 1.0);
    motion(&ai, true, 0.0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!sa().limited && !sb().limited);
    motion(&ai, true, 1.0);
    wait("own and group activity despite wake disabled", || {
        sa().local_activity && sb().group_activity
    })
    .await;
    assert!(
        !sa().group_activity && !sb().local_activity,
        "receipt never becomes physical activity or rebroadcasts"
    );
    assert_eq!(ai.idle_reset_count(), 0);
    assert_eq!(bi.idle_reset_count(), 0);
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(1100)).await;
        motion(&ai, true, 1.0);
        wait("renewed local and remote activity pacing", || {
            sa().limited && sb().limited
        })
        .await;
    }
    let capture_sessions = ai.capture_sessions();
    let injection_sessions = ai.injection_sessions();
    let old = common::input_id(&a, b.id()).unwrap();
    a.set_local_paused("00000000-0000-4000-8000-000000000001".into(), true)
        .unwrap();
    wait("local pause settled", || {
        a.pause_status().paused && !a.pause_status().settling
    })
    .await;
    assert!(!b.pause_status().paused);
    assert_eq!(ai.capture_active(), Some(false));
    assert!(!ai.injection_active());
    motion(&ai, true, 1.0);
    tokio::time::sleep(Duration::from_millis(3200)).await;
    assert!(a.low_pacing(b.id()).is_none_or(|s| !s.limited));
    assert!(b.low_pacing(a.id()).is_none_or(|s| !s.limited));
    assert!(common::input_id(&a, b.id()).is_none());
    assert!(
        b.low_pacing(a.id()).is_none_or(|s| !s.group_activity),
        "retired session cannot report group activity"
    );
    a.set_local_paused("00000000-0000-4000-8000-000000000002".into(), false)
        .unwrap();
    wait("both resumed", || {
        [&a, &b]
            .iter()
            .all(|n| !n.pause_status().paused && !n.pause_status().settling)
            && common::input_id(&a, b.id()).is_some()
            && common::input_id(&b, a.id()).is_some()
            && ai.capture_active() == Some(true)
            && ai.injection_active()
    })
    .await;
    assert_eq!(ai.capture_sessions(), capture_sessions + 1);
    assert_eq!(ai.injection_sessions(), injection_sessions + 1);
    assert!(
        !sa().limited && !sb().limited,
        "paused input left no activity for the new session"
    );
    motion(&bi, true, 1.0);
    wait("first physical event after pause", || {
        sb().local_activity && sa().group_activity
    })
    .await;
    assert!(!sa().local_activity && !sa().audio_activity);
    assert!(sa().rate_bytes_per_second <= 2 * 1024 * 1024);
    assert_ne!(common::input_id(&a, b.id()).unwrap(), old);
    // Source/sink packet callbacks are exercised by the audio runtime tests. Here
    // the shared activity hook and its API projection have one observable window.
    for node in [&a, &b] {
        node.links().unwrap().audio_activity();
    }
    for node in [&a, &b] {
        assert!(node.audio_status().unwrap().audio_activity);
        assert!(node.links().unwrap().audio_active());
    }
    tokio::time::sleep(Duration::from_millis(3050)).await;
    for node in [&a, &b] {
        assert!(!node.audio_status().unwrap().audio_activity);
    }
    a.shutdown().await;
    b.shutdown().await;
}
