#![cfg(debug_assertions)]
//! Link assembly acceptance: unreadable state leaves a working IPC-only backend.
//! Real local sockets and IPC, fake input; no installed app or native injection.
use oink_agent::{
    api::{AgentApi, AlertKind, ApiError, ServiceState},
    config_module::group_store::{self, STATE_FILE},
    ipc::Server,
    node::{Node, NodeOptions},
    remote::RemoteApi,
};
use oink_input::InputBackend;
use oink_test_support::input::FakeInput;
use std::{
    net::{TcpListener, UdpSocket},
    sync::Arc,
};

#[test]
fn unreadable_group_keeps_ipc_and_reset_starts_sharing_in_same_node() {
    let _scenario = test_watchdog::Scenario::new(
        module_path!(),
        "unreadable_group_keeps_ipc_and_reset_starts_sharing_in_same_node",
    );
    let dir = tempfile::tempdir().unwrap();
    let bad = b"{ incomplete group state";
    std::fs::write(dir.path().join(STATE_FILE), bad).unwrap();
    // Discovery is adjacent to QUIC; keep this fixture out of the ephemeral range.
    let (reserved, beacon) = (30000..45000)
        .find_map(|port| {
            let socket = UdpSocket::bind(("127.0.0.1", port)).ok()?;
            let beacon = UdpSocket::bind(("127.0.0.1", port + 1)).ok()?;
            let low = TcpListener::bind(("127.0.0.1", port)).ok()?;
            drop(low);
            Some((socket, beacon))
        })
        .expect("free loopback ports");
    let port = reserved.local_addr().unwrap().port();
    drop((reserved, beacon));
    let input = Arc::new(FakeInput::new());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut config = oink_test_support::network_config(dir.path().into(), "recovery", port);
    config.beacon = true;
    let node = rt
        .block_on(Node::start_with_fake_platform(NodeOptions {
            net: config,
            input: input.clone(),
            displays: vec![],
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: true,
            cursor_trail: false,
        }))
        .unwrap();
    let identity = node.id();
    assert!(node.links().is_err());
    assert!(node.input_thread().is_err());
    assert_eq!(input.capture_active(), Some(false));
    let udp = UdpSocket::bind(("127.0.0.1", port)).unwrap();
    let tcp = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let discovery = UdpSocket::bind(("127.0.0.1", port + 1)).unwrap();
    let server = rt.block_on(Server::start_on(node.clone(), 0)).unwrap();
    let api = RemoteApi::new(dir.path());
    let overview = api.overview().unwrap();
    assert!(overview.group.is_none());
    assert_eq!(overview.service, ServiceState::Degraded);
    assert!(
        overview
            .alerts
            .iter()
            .any(|a| a.kind == AlertKind::GroupStateUnreadable)
    );
    assert_eq!(std::fs::read(dir.path().join(STATE_FILE)).unwrap(), bad);
    drop((udp, tcp, discovery));
    api.reset_group_state().unwrap();
    assert_eq!(node.id(), identity);
    assert!(node.links().is_ok());
    assert!(node.input_thread().is_ok());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while input.capture_active() != Some(true) {
        assert!(std::time::Instant::now() < deadline, "reset must activate capture");
        std::thread::yield_now();
    }
    let group = api.group().unwrap();
    assert!(group.id.is_some());
    assert_eq!(group.members.len(), 1);
    assert!(group.members[0].is_self);
    let stored = group_store::load(&dir.path().join(STATE_FILE), identity)
        .unwrap()
        .unwrap();
    let owner = oink_agent::config::AgentConfig::load_from(dir.path()).unwrap();
    assert_eq!(owner.owner().active_group(), Some(stored.group()));
    assert!(!owner.settings.keyboard_disabled);
    let envelope: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join(STATE_FILE)).unwrap()).unwrap();
    assert_eq!(envelope["schema"], 1);
    assert!(envelope.get("state_cbor").is_some());
    for retired in ["key", "pause", "audio_exit", "keyboards", "input_transforms"] {
        assert!(envelope.get(retired).is_none());
    }
    assert!(
        !api.overview()
            .unwrap()
            .alerts
            .iter()
            .any(|a| a.kind == AlertKind::GroupStateUnreadable)
    );
    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|f| f.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("group-state-unreadable-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(std::fs::read(&backups[0]).unwrap(), bad);
    assert!(matches!(api.reset_group_state(), Err(ApiError::InvalidState { .. })));
    assert!(api.leave_group().is_err());
    rt.block_on(server.close());
    rt.block_on(node.shutdown());
    assert_eq!(input.capture_active(), Some(false));
    TcpListener::bind(("127.0.0.1", port)).unwrap();
    UdpSocket::bind(("127.0.0.1", port)).unwrap();
    UdpSocket::bind(("127.0.0.1", port + 1)).unwrap();
}

#[path = "common/watchdog.rs"]
mod test_watchdog;
