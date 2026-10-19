#![cfg(debug_assertions)]
//! Local pause lifecycle through real Node owners with a fake OS input backend.
use oink_agent::{
    api::AgentApi,
    node::{Node, NodeOptions},
};
use oink_input::InputBackend;
use oink_test_support::input::FakeInput;
use std::{sync::Arc, time::Duration};

async fn start(dir: &std::path::Path, input: &FakeInput) -> Node {
    Node::start_with_fake_platform(NodeOptions {
        net: oink_test_support::network_config(dir.to_path_buf(), "pause-test", 0),
        input: Arc::new(input.clone()),
        displays: vec![],
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        cursor_trail: false,
    })
    .await
    .unwrap()
}
async fn settled(node: &Node, paused: bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = node.pause_status();
            if state.paused == paused && !state.settling && (paused || node.capture_active() == Some(true)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("local workers settled");
}
async fn input_ready(node: &Node, input: &FakeInput) {
    // Capture and injection belong to separate workers. A resumed capture tap
    // does not imply that the new injection session has been created yet.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = node.pause_status();
            if !state.paused && !state.settling && input.capture_active() == Some(true) && input.injection_active() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("capture and injection sessions ready");
}
async fn pause(node: &Node, id: u64, paused: bool) {
    let reply = node
        .set_local_paused(format!("00000000-0000-4000-8000-{id:012x}"), paused)
        .unwrap();
    assert!(
        !matches!(
            reply,
            oink_agent::device_cards::DeviceControlReply::Completed(oink_agent::device_cards::DeviceControlCompleted {
                result: oink_agent::device_cards::DeviceControlResult::Refused { .. }
                    | oink_agent::device_cards::DeviceControlResult::Failed { .. },
                ..
            })
        ),
        "{reply:?}"
    );
    settled(node, paused).await;
}
#[test]
fn local_pause_detaches_input_resumes_fresh_and_resets_on_restart() {
    test_watchdog::run(
        module_path!(),
        "local_pause_detaches_input_resumes_fresh_and_resets_on_restart",
        Some(4),
        async {
            let dir = tempfile::tempdir().unwrap();
            let input = FakeInput::new();
            let node = start(dir.path(), &input).await;
            input_ready(&node, &input).await;
            assert_eq!(input.capture_active(), Some(true));
            let mut settings = node.input_settings().unwrap();
            settings.hotkeys = vec![oink_agent::api::Hotkey {
                action: oink_agent::api::HotkeyAction::ToggleSharing,
                keys: vec!["Ctrl".into(), "Alt".into(), "S".into()],
                enabled: true,
            }];
            node.set_input_settings(settings.into()).unwrap();
            let old = input.input_safety();
            let capture_sessions = input.capture_sessions();
            let injection_sessions = input.injection_sessions();
            pause(&node, 1, true).await;
            assert!(node.pause_status().capture_detached);
            assert!(!node.pause_status().emergency);
            assert_eq!(input.capture_active(), Some(false));
            assert!(!input.injection_active());
            assert_eq!(input.input_safety().epoch, old.epoch + 1);
            tokio::time::timeout(Duration::from_secs(5), async {
                while input.paused_hotkey().is_none() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let hotkey = input.paused_hotkey().unwrap();
            assert_eq!(hotkey.key, oink_input::KeyCode::keyboard(22));
            assert!(hotkey.control && hotkey.alt && !hotkey.shift && !hotkey.meta);
            assert_eq!(
                input.feed(&oink_input::CapturedEvent {
                    physical: true,
                    event: oink_input::InputEvent::Key {
                        code: oink_input::KeyCode::ESCAPE,
                        pressed: true
                    },
                    source: oink_input::SourceKind::Keyboard,
                    time_us: 0,
                    cursor: None,
                }),
                oink_input::Disposition::Pass,
                "paused tap-based emergency detection is inactive"
            );
            input.fire_resume_hotkey();
            input_ready(&node, &input).await;
            assert!(input.paused_hotkey().is_none());
            assert_eq!(input.input_safety().epoch, old.epoch + 2);
            assert_eq!(input.capture_sessions(), capture_sessions + 1);
            assert_eq!(input.injection_sessions(), injection_sessions + 1);
            assert!(input.injection_active());
            pause(&node, 2, true).await;
            node.shutdown().await;
            let input = FakeInput::new();
            let node = start(dir.path(), &input).await;
            input_ready(&node, &input).await;
            assert_eq!(input.capture_active(), Some(true));
            node.shutdown().await;
        },
    );
}
#[test]
fn diagnostics_preference_survives_pause_and_fresh_boot() {
    test_watchdog::run(
        module_path!(),
        "diagnostics_preference_survives_pause_and_fresh_boot",
        Some(4),
        async {
            let dir = tempfile::tempdir().unwrap();
            let node = start(dir.path(), &FakeInput::new()).await;
            node.set_diagnostics_enabled(true).unwrap();
            pause(&node, 1, true).await;
            for configured in [false, true, false] {
                node.set_diagnostics_enabled(configured).unwrap();
                let status = node.diagnostics_status().unwrap();
                assert_eq!(status.configured_enabled, configured);
                assert!(status.suspended && !status.enabled);
            }
            pause(&node, 2, false).await;
            let status = node.diagnostics_status().unwrap();
            assert!(!status.configured_enabled && !status.enabled && !status.suspended);
            pause(&node, 3, true).await;
            node.set_diagnostics_enabled(true).unwrap();
            node.shutdown().await;
            let node = start(dir.path(), &FakeInput::new()).await;
            settled(&node, false).await;
            let status = node.diagnostics_status().unwrap();
            assert!(status.configured_enabled && status.enabled && !status.suspended);
            node.shutdown().await;
        },
    );
}
#[test]
fn completed_pause_request_replay_does_not_override_a_later_resume() {
    test_watchdog::run(
        module_path!(),
        "completed_pause_request_replay_does_not_override_a_later_resume",
        Some(4),
        async {
            let dir = tempfile::tempdir().unwrap();
            let node = start(dir.path(), &FakeInput::new()).await;
            pause(&node, 1, true).await;
            pause(&node, 2, false).await;
            let reply = node
                .set_local_paused("00000000-0000-4000-8000-000000000001".into(), true)
                .unwrap();
            assert!(matches!(
                reply,
                oink_agent::device_cards::DeviceControlReply::Completed(_)
            ));
            settled(&node, false).await;
            assert!(!node.pause_status().paused);
            node.shutdown().await;
        },
    );
}
#[path = "common/watchdog.rs"]
mod test_watchdog;
