#![cfg(debug_assertions)]
//! Integration fixtures link the ordinary library, without Rust's cfg(test).
use oink_agent::{
    api::AgentApi,
    node::{Node, NodeOptions},
};
use oink_test_support::{clipboard::FakeClipboard, input::FakeInput};
use std::{sync::Arc, time::Duration};

fn options(directory: &std::path::Path) -> NodeOptions {
    NodeOptions {
        net: oink_test_support::network_config(directory.into(), "synthetic", 0),
        input: Arc::new(FakeInput::new()),
        displays: Vec::new(),
        display_backend: None,
        clipboard: Some(Arc::new(FakeClipboard::new())),
        mode_switch_in_callback: true,
        capture_reports_key_repeat: false,
        cursor_trail: false,
    }
}

async fn assert_native_free(node: &Node) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = node.audio_status().unwrap();
            if let Some(local) = status.devices.iter().find(|device| device.device == node.id()) {
                assert_eq!(local.capture, "unit-test");
                assert!(local.outputs.is_empty());
                assert!(status.capture_info.is_none());
                assert!(status.playback_info.is_none());
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    #[cfg(target_os = "macos")]
    assert_eq!(
        node.file_provider_status().unwrap().state,
        oink_agent::api::FileProviderState::Checking
    );
}

#[tokio::test]
async fn explicit_fake_platform_keeps_real_node_startup_native_free() {
    let directory = tempfile::tempdir().unwrap();
    let node = Node::start_with_fake_platform(options(directory.path())).await.unwrap();
    assert_native_free(&node).await;
    node.shutdown().await;
}

#[test]
fn explicit_fake_platform_keeps_host_startup_native_free() {
    let directory = tempfile::tempdir().unwrap();
    let host = oink_agent::host::AgentHost::start_with_fake_platform(options(directory.path()), false).unwrap();
    host.block_on(assert_native_free(host.node()));
    host.shutdown();
}
