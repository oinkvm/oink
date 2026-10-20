#![cfg(debug_assertions)]
//! Real local IPC calls, subscriptions, graceful shutdown, and reconnect behavior.
//! Native services are disabled and platform devices are injected fakes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use oink_agent::api::{AgentApi, ApiError, ApiEvent};
use oink_agent::ipc::{Client, Server};
use oink_agent::node::{Node, NodeOptions};
use oink_agent::remote::{EventsEnd, RemoteApi};
use oink_foundation::display::DisplayInfo;
use oink_foundation::{Point, Rect, Size};
use oink_test_support::input::FakeInput;

fn display() -> DisplayInfo {
    DisplayInfo {
        key: "main".into(),
        name: "main".into(),
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

async fn start(dir: &std::path::Path) -> Node {
    start_with_input(dir, FakeInput::new()).await
}

async fn start_with_input(dir: &std::path::Path, input: FakeInput) -> Node {
    input.set_cursor(Point::new(800.0, 500.0));
    Node::start_with_fake_platform(NodeOptions {
        // Allocate a distinct QUIC/discovery pair through the shared fixture.
        net: oink_test_support::network_config(dir.to_owned(), "svc", 0),
        input: Arc::new(input),
        displays: vec![display()],
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        cursor_trail: false,
    })
    .await
    .unwrap()
}

#[test]
fn partial_preference_commands_persist_only_explicit_local_choices() {
    let _scenario = test_watchdog::Scenario::new(
        module_path!(),
        "partial_preference_commands_persist_only_explicit_local_choices",
    );
    use serde_json::{Value, json};
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let node = rt.block_on(start(dir.path()));
    let server = rt.block_on(Server::start_on(node.clone(), 0)).unwrap();
    let remote = RemoteApi::new(dir.path());
    let read =
        || -> Value { serde_json::from_slice(&std::fs::read(dir.path().join("settings.json")).unwrap()).unwrap() };
    let before = read();
    assert_eq!(before["local"], json!({}));
    remote
        .set_ui_preferences(serde_json::from_value(json!({"show_status_icon":true})).unwrap())
        .unwrap();
    remote
        .set_ui_preferences(serde_json::from_value(json!({"confirm_quit":false})).unwrap())
        .unwrap();
    remote
        .set_ui_preferences(serde_json::from_value(json!({"theme_accent":{"dark":"#112233"}})).unwrap())
        .unwrap();
    remote
        .set_clipboard_settings(serde_json::from_value(json!({"files":false})).unwrap())
        .unwrap();
    remote
        .set_clipboard_settings(serde_json::from_value(json!({"text":true})).unwrap())
        .unwrap();
    remote
        .set_audio_settings(serde_json::from_value(json!({"keep_local_playback":false,"receive_output":null})).unwrap())
        .unwrap();
    remote
        .set_input_settings(
            serde_json::from_value(json!({"event_frequency":"hz125","emergency_hold_ms":5000})).unwrap(),
        )
        .unwrap();
    let applied = read();
    assert_eq!(
        applied["local"],
        json!({
            "show_status_icon":true,"confirm_quit":false,"theme_accent":{"dark":"#112233"},
            "clipboard":{"files":false,"text":true},"audio":{"keep_local_playback":false,"receive_output":null},
            "event_frequency":"hz125","emergency_hold_ms":5000
        })
    );
    assert_eq!(applied["device_options_cbor"], before["device_options_cbor"]);
    assert_eq!(remote.ui_preferences().unwrap().theme_accent.light.as_str(), "#FF6B8E");
    assert!(!remote.clipboard_settings().unwrap().files);
    assert!(remote.clipboard_settings().unwrap().images);
    assert!(
        remote
            .set_ui_preferences(serde_json::from_value(json!({"confirm_quit":null})).unwrap())
            .is_err()
    );
    assert!(
        remote
            .set_clipboard_settings(serde_json::from_value(json!({"unknown":false})).unwrap())
            .is_err()
    );
    assert!(
        remote
            .set_audio_settings(serde_json::from_value(json!({"forwarding_volume_percent":201})).unwrap())
            .is_err()
    );
    assert!(
        remote
            .set_input_settings(serde_json::from_value(json!({"device_gain":0})).unwrap())
            .is_err()
    );
    assert!(
        remote
            .set_ui_preferences(serde_json::from_value(json!({"layout_projection_enabled":false})).unwrap())
            .is_err()
    );
    assert!(
        serde_json::to_value(remote.ui_preferences().unwrap())
            .unwrap()
            .get("layout_projection_enabled")
            .is_none()
    );
    assert_eq!(read(), applied);
    rt.block_on(server.close());
    rt.block_on(node.shutdown());
    let config = oink_agent::config::AgentConfig::load_from(dir.path()).unwrap();
    assert!(!config.settings.confirm_quit);
    assert!(config.settings.show_status_icon);
    assert!(!config.settings.clipboard.unwrap().files);
    rt.shutdown_timeout(Duration::from_millis(200));
}

#[test]
fn tolerant_settings_startup_preserves_files_and_reports_local_issues() {
    test_watchdog::run(
        module_path!(),
        "tolerant_settings_startup_preserves_files_and_reports_local_issues",
        Some(4),
        tolerant_settings_startup_preserves_files_and_reports_local_issues_scenario(),
    );
}

async fn tolerant_settings_startup_preserves_files_and_reports_local_issues_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let mut config = oink_agent::config::AgentConfig::load_from(dir.path()).unwrap();
    config.save().unwrap();
    let mut original: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    original["local"]["layout_projection_enabled"] = serde_json::json!(true);
    original["local"]["future"] = serde_json::json!({"keep":true});
    original["local"]["log_level"] = serde_json::json!("future");
    original["local"]["hotkeys"] = serde_json::json!([{"action":"future","keys":[],"enabled":false}]);
    let original = serde_json::to_string(&original).unwrap();
    std::fs::write(&path, &original).unwrap();
    let node = start(dir.path()).await;
    let status = node.status().unwrap();
    assert!(
        status
            .settings_issues
            .iter()
            .any(|issue| issue.path == "settings.log_level")
    );
    assert!(
        status
            .settings_issues
            .iter()
            .any(|issue| issue.path == "settings.future")
    );
    assert!(
        status
            .settings_issues
            .iter()
            .any(|issue| issue.path == "settings.layout_projection_enabled")
    );
    assert!(
        serde_json::to_value(node.ui_preferences().unwrap())
            .unwrap()
            .get("layout_projection_enabled")
            .is_none()
    );
    node.preview_layout_projection(1, vec![], vec![], false).unwrap();
    node.preview_layout_projection(1, vec![], vec![], true).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()["local"],
        serde_json::from_str::<serde_json::Value>(&original).unwrap()["local"]
    );
    node.shutdown().await;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()["local"],
        serde_json::from_str::<serde_json::Value>(&original).unwrap()["local"]
    );
}

fn saved_layout(dir: &std::path::Path) -> oink_display::layout::Layout {
    use oink_protocol::v1::schema::WireDto;
    let identity = oink_network::identity::Identity::load_or_create(dir).unwrap();
    let stored = oink_agent::config_module::group_store::load(
        &dir.join(oink_agent::config_module::group_store::STATE_FILE),
        identity.id(),
    )
    .unwrap()
    .unwrap();
    let value =
        oink_protocol::v1::dto::LayoutValue::from_raw(stored.layout_register().unwrap().value().unwrap()).unwrap();
    oink_agent::config_module::layout_wire::from_wire(&value).unwrap()
}

#[test]
fn configured_game_hotkey_reaches_the_agent_action() {
    let _scenario = test_watchdog::Scenario::new(module_path!(), "configured_game_hotkey_reaches_the_agent_action");
    use oink_agent::api::HotkeyAction;
    use oink_input::{CapturedEvent, Disposition, InputEvent, KeyCode, SourceKind};
    let dir = std::env::temp_dir().join(format!("oink-game-hotkey-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let input = FakeInput::new();
    let node = rt.block_on(start_with_input(&dir, input.clone()));
    let mut settings = node.input_settings().unwrap();
    let game = settings
        .hotkeys
        .iter_mut()
        .find(|h| h.action == HotkeyAction::ToggleGameMode)
        .unwrap();
    assert!(game.enabled, "all default shortcuts are enabled");
    game.enabled = true;
    game.keys = vec!["Ctrl".into(), "Alt".into(), "G".into()];
    node.set_input_settings(settings.into()).unwrap();
    let feed = |code, pressed| {
        input.feed(&CapturedEvent {
            physical: true,
            event: InputEvent::Key { code, pressed },
            source: SourceKind::Keyboard,
            time_us: 0,
            cursor: None,
        })
    };
    for active in [true, false] {
        feed(KeyCode::LEFT_CTRL, true);
        feed(KeyCode::LEFT_ALT, true);
        assert_eq!(feed(KeyCode::keyboard(0x0a), true), Disposition::Swallow);
        wait("game hotkey action", || node.game_mode().unwrap().active == active);
        assert_eq!(feed(KeyCode::keyboard(0x0a), false), Disposition::Swallow);
        feed(KeyCode::LEFT_ALT, false);
        feed(KeyCode::LEFT_CTRL, false);
    }
    assert_eq!(node.game_mode().unwrap().mode, oink_routing::GameMode::Off);
    rt.shutdown_timeout(Duration::from_millis(200));
    let _ = std::fs::remove_dir_all(&dir);
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let end = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if f() {
            return;
        }
        if std::time::Instant::now() >= end {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn remote_pairing_installs_membership_and_fences_prepared_client() {
    use oink_agent::{api::pairing, device_cards::DecimalU64};
    let _scenario = test_watchdog::Scenario::new(
        module_path!(),
        "remote_pairing_installs_membership_and_fences_prepared_client",
    );
    let temp = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let a_dir = temp.path().join("a");
    let b_dir = temp.path().join("b");
    let a = rt.block_on(start(&a_dir));
    let b = rt.block_on(start(&b_dir));
    let sa = rt.block_on(Server::start_on(a.clone(), 0)).unwrap();
    let sb = rt.block_on(Server::start_on(b.clone(), 0)).unwrap();
    let ra = RemoteApi::new(&a_dir);
    let rb = RemoteApi::new(&b_dir);
    let (send, receive) = std::sync::mpsc::channel();
    let subscription = a.subscribe(Arc::new(move |event| {
        if let ApiEvent::AddressQueryFinished { result, .. } = event {
            let _ = send.send(result.clone());
        }
    }));
    let operation = "00000000-0000-4000-8000-000000000001".to_string();
    assert!(matches!(
        ra.query_pairing_address(
            operation,
            DecimalU64(1),
            "127.0.0.1".into(),
            Some(b.network_info().unwrap().port)
        )
        .unwrap(),
        pairing::PairReply::Value(_)
    ));
    let pairing::AddressQueryResult::Found { peer } = receive.recv_timeout(Duration::from_secs(8)).unwrap() else {
        panic!("prepared query failed")
    };
    a.unsubscribe(subscription);
    let target = pairing::PairTarget::PreparedQuery {
        query_id: peer.query_id,
        generation: peer.generation,
    };
    let foreign = RemoteApi::new(&a_dir);
    assert!(matches!(
        foreign
            .start_pairing("00000000-0000-4000-8000-000000000002".into(), target.clone())
            .unwrap(),
        pairing::PairReply::Problem(_)
    ));
    let pairing::PairReply::Value(pairing::Started::Started { pair_id }) = ra
        .start_pairing("00000000-0000-4000-8000-000000000003".into(), target)
        .unwrap()
    else {
        panic!("pairing start failed")
    };
    wait("incoming pairing request", || {
        rb.pairings()
            .unwrap()
            .requests
            .iter()
            .any(|request| request.id == pair_id)
    });
    let request = rb
        .pairings()
        .unwrap()
        .requests
        .into_iter()
        .find(|request| request.id == pair_id)
        .unwrap();
    assert!(matches!(
        rb.accept_pairing(request.id, request.revision).unwrap(),
        pairing::PairReply::Value(_)
    ));
    wait("code display", || {
        ra.pairings()
            .unwrap()
            .sessions
            .iter()
            .any(|session| session.id == pair_id && session.phase == 1)
    });
    let code = ra.pairing_code(pair_id.clone()).unwrap().six_digits;
    wait("code entry", || {
        rb.pairings()
            .unwrap()
            .sessions
            .iter()
            .any(|session| session.id == pair_id && session.phase == 2)
    });
    let session = rb
        .pairings()
        .unwrap()
        .sessions
        .into_iter()
        .find(|session| session.id == pair_id)
        .unwrap();
    assert!(matches!(
        rb.submit_pairing_code("foreign-round".into(), session.revision, 1, code.clone())
            .unwrap(),
        pairing::PairReply::Problem(_)
    ));
    assert!(matches!(
        rb.submit_pairing_code(pair_id.clone(), session.revision, 1, code)
            .unwrap(),
        pairing::PairReply::Value(_)
    ));
    wait("IPC pairing committed on both peers", || {
        [&ra, &rb].iter().all(|api| {
            api.group().unwrap().members.len() == 2
                && api
                    .pairings()
                    .unwrap()
                    .sessions
                    .iter()
                    .any(|s| s.id == pair_id && s.phase == 7)
        })
    });
    assert_eq!(ra.group().unwrap().id, rb.group().unwrap().id);
    // The address editor is local storage access, including when the member is
    // offline; dispatch both commands through real authenticated IPC.
    assert!(!ra.member_addresses(b.id().into()).unwrap().is_empty());
    rt.block_on(sb.close());
    rt.block_on(b.shutdown());
    wait("offline member", || {
        ra.group()
            .unwrap()
            .members
            .iter()
            .any(|m| m.device.id.0 == b.id() && !m.reachable)
    });
    ra.set_member_address(b.id().into(), "192.0.2.7:47470".into()).unwrap();
    assert_eq!(ra.member_addresses(b.id().into()).unwrap()[0], "192.0.2.7:47470");
    assert_eq!(
        ra.set_member_address(b.id().into(), "bad address".into()),
        Err(ApiError::InvalidAddress)
    );
    assert_eq!(
        ra.member_addresses(oink_foundation::DeviceId([99; 32]).into()),
        Err(ApiError::NotMember)
    );
    let address_file = a_dir.join("addresses.json");
    std::fs::remove_file(&address_file).unwrap();
    std::fs::create_dir(&address_file).unwrap();
    assert_eq!(
        ra.set_member_address(b.id().into(), "192.0.2.8:47470".into()),
        Err(ApiError::PersistenceFailed)
    );
    rt.block_on(sa.close());
    rt.block_on(a.shutdown());
    rt.shutdown_timeout(Duration::from_millis(200));
}

#[test]
fn remote_api_calls_subscribes_and_sees_stop() {
    let _scenario = test_watchdog::Scenario::new(module_path!(), "remote_api_calls_subscribes_and_sees_stop");
    let dir = std::env::temp_dir().join(format!("oink-remote-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let node = rt.block_on(start(&dir));
    let server = rt.block_on(Server::start_on(node.clone(), 0)).unwrap();

    let remote = RemoteApi::new(&dir);
    // 调用：与服务进程内的结果相同。
    let local = node.overview().unwrap();
    let over = remote.overview().unwrap();
    assert_eq!(over.this_device.id, local.this_device.id);
    assert_eq!(over.this_device.app_version, env!("OINK_RELEASE_VERSION"));
    assert!(remote.layout().unwrap().geometry.screens.len() == 1);
    let hello = remote.hello().unwrap();
    assert_eq!(hello.pid, std::process::id());
    assert_eq!(hello.version, env!("OINK_RELEASE_VERSION"));
    // 错误原样返回。
    assert!(matches!(
        remote.query_pairing_address(
            "00000000-0000-4000-8000-000000000001".into(),
            oink_agent::device_cards::DecimalU64(1),
            "not an ip".into(),
            None
        ),
        Ok(oink_agent::api::pairing::PairReply::Problem(_))
    ));

    // 事件：订阅回调跨连接保留；连上之后的变化都会到达。
    let seen: Arc<Mutex<Vec<ApiEvent>>> = Arc::default();
    {
        let seen = seen.clone();
        remote.subscribe(Arc::new(move |e: &ApiEvent| seen.lock().unwrap().push(e.clone())));
    }
    let (conn_tx, conn_rx) = std::sync::mpsc::channel();
    let follower = {
        let remote = remote.clone();
        std::thread::spawn(move || {
            remote.follow_events(|info| {
                conn_tx.send(info.clone()).unwrap();
            })
        })
    };
    assert_eq!(
        conn_rx.recv_timeout(Duration::from_secs(5)).unwrap().pid,
        std::process::id()
    );
    // Recorder recovery is asynchronous even for an empty isolated directory.
    // The export API explicitly rejects requests until that startup phase completes.
    wait("diagnostics startup recovery", || {
        !remote.diagnostics_status().unwrap().recovering
    });
    let bundle = remote.export_diagnostics().unwrap();
    assert_eq!(bundle.cutoff_complete, Some(true));
    assert_eq!(bundle.included_device_count, Some(1));
    assert_eq!(bundle.missing_device_count, Some(0));
    let status = remote.diagnostics_status().unwrap();
    assert!(status.oldest_utc_us.is_some());
    assert!(status.newest_utc_us.is_some());
    assert!(std::path::Path::new(&bundle.path).exists());
    assert_eq!(
        std::path::Path::new(&bundle.path).parent().unwrap(),
        std::path::Path::new(&remote.diagnostics_folder().unwrap())
    );
    let mut settings = remote.audio_status().unwrap().settings;
    settings.keep_local_playback = !settings.keep_local_playback;
    remote.set_audio_settings(settings.clone().into()).unwrap();
    assert_eq!(remote.audio_status().unwrap().settings, settings);
    wait("audio API event", || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ApiEvent::Audio(status) if status.settings == settings))
    });
    for frequency in [
        oink_agent::api::EventFrequency::Unlimited,
        oink_agent::api::EventFrequency::Hz125,
    ] {
        let mut input = remote.input_settings().unwrap();
        input.event_frequency = frequency;
        remote.set_input_settings(input.into()).unwrap();
        assert_eq!(remote.input_settings().unwrap().event_frequency, frequency);
        assert_eq!(
            oink_agent::config::AgentConfig::load_from(&dir)
                .unwrap()
                .settings
                .event_frequency,
            frequency
        );
        wait("event frequency IPC event", || {
            seen.lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, ApiEvent::InputSettings(s) if s.event_frequency == frequency))
        });
    }
    remote.set_game_mode(oink_routing::GameMode::On).unwrap();
    wait("game mode input-owner application", || {
        remote.game_mode().unwrap().active
    });
    assert_eq!(remote.game_mode().unwrap().holder, local.this_device.id);
    wait("game mode API event", || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ApiEvent::GameMode(s) if s.active))
    });
    remote.toggle_game_mode().unwrap();
    // The reply acknowledges a queued command, not input-owner application.
    // Wait before sending Auto, which could replace a still-pending Off command.
    wait("game mode toggle input-owner application", || {
        let status = remote.game_mode().unwrap();
        status.mode == oink_routing::GameMode::Off && !status.active
    });
    wait("game mode toggled-off API event", || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ApiEvent::GameMode(s) if s.mode == oink_routing::GameMode::Off && !s.active))
    });
    remote.set_game_mode(oink_routing::GameMode::Auto).unwrap();
    remote
        .set_local_paused("00000000-0000-4000-8000-000000000002".into(), true)
        .unwrap();
    wait("overview event with sharing off", || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ApiEvent::Overview(o) if !o.sharing_enabled))
    });

    // 按请求退出：订阅方收到「正在退出」。
    let d = dir.clone();
    Client::connect(&d).unwrap().stop().unwrap();
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), node.wait_for_stop())
            .await
            .unwrap()
    });
    assert_eq!(follower.join().unwrap(), EventsEnd::Stopping);
    rt.block_on(server.close());
    rt.block_on(node.shutdown());
    assert!(!dir.join("ipc.json").exists());
    assert!(matches!(remote.overview(), Err(ApiError::Unavailable { .. })));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remote_api_reconnects_to_new_endpoint_and_sees_crash() {
    let _scenario =
        test_watchdog::Scenario::new(module_path!(), "remote_api_reconnects_to_new_endpoint_and_sees_crash");
    let dir = std::env::temp_dir().join(format!("oink-remote-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let node = rt.block_on(start(&dir));
    let remote = RemoteApi::new(&dir);
    assert!(matches!(remote.overview(), Err(ApiError::Unavailable { .. })));

    // 先开一个 IPC，再换一个（新的端口与令牌，模拟服务重启）：同一个 RemoteApi 重新读 ipc.json 后照常调用。
    let first = rt.block_on(Server::start_on(node.clone(), 0)).unwrap();
    let id = remote.overview().unwrap().this_device.id;
    rt.block_on(first.close());
    let server = rt.block_on(Server::start_on(node.clone(), 0)).unwrap();
    assert!(
        matches!(remote.overview(), Err(ApiError::Unavailable { .. })),
        "stale pooled connection"
    );
    assert_eq!(remote.overview().unwrap().this_device.id, id);

    // 服务被直接结束（没有预告）时订阅方得知连接丢失。
    let (conn_tx, conn_rx) = std::sync::mpsc::channel();
    let follower = {
        let remote = remote.clone();
        std::thread::spawn(move || remote.follow_events(|_| conn_tx.send(()).unwrap()))
    };
    conn_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // 模拟崩溃：运行时连同所有连接一起消失，不发 closing（ipc.json 留下）。
    drop(server);
    rt.shutdown_timeout(Duration::from_millis(200));
    let end = follower.join().unwrap();
    assert!(matches!(end, EventsEnd::Lost(_)), "crash was reported as {end:?}");
    assert!(dir.join("ipc.json").exists());
    assert!(matches!(remote.hello(), Err(ApiError::Unavailable { .. })));

    // 打断跟随（界面退出）。
    let remote2 = remote.clone();
    let t = std::thread::spawn(move || remote2.follow_events(|_| {}));
    std::thread::sleep(Duration::from_millis(50));
    remote.stop_following();
    // 服务已不在：要么连不上（Lost），要么被打断（Cancelled）。
    assert!(matches!(t.join().unwrap(), EventsEnd::Lost(_) | EventsEnd::Cancelled));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn measured_layout_restore_is_exact_and_interactive_resize_is_proportional_over_ipc() {
    use oink_agent::api::{Placement, ScreenKey};
    let _scenario = test_watchdog::Scenario::new(
        module_path!(),
        "measured_layout_restore_is_exact_and_interactive_resize_is_proportional_over_ipc",
    );
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let node = rt.block_on(start(dir.path()));
    let server = rt.block_on(Server::start_on(node.clone(), 0)).unwrap();
    let remote = RemoteApi::new(dir.path());
    let key = ScreenKey {
        device: node.id().into(),
        display: "main".into(),
    };
    // Undo restores one uniform native-to-canvas scale.
    let placement = Placement {
        device: node.id().into(),
        origin: Point::new(0.0, 0.0),
        scale: 344.2 / 686.0 / 1600.0,
    };
    let saved = remote.save_layout(vec![placement.clone()]).unwrap();
    assert_eq!(
        saved.geometry.screens[0].size,
        Size::new(344.2 / 686.0, 344.2 / 686.0 * 0.625)
    );
    // The editor request is a resize of that measured rectangle, not a new measurement.
    let resized = remote
        .set_screen_size(key, Size::new(688.4 / 686.0, 300.0 / 686.0))
        .unwrap();
    assert_eq!(
        resized.geometry.screens[0].size,
        Size::new(688.4 / 686.0, 688.4 / 686.0 * 0.625)
    );
    let restored = remote.save_layout(vec![placement.clone()]).unwrap();
    assert_eq!(
        restored.geometry.screens[0].size,
        Size::new(344.2 / 686.0, 344.2 / 686.0 * 0.625)
    );
    let persisted = saved_layout(dir.path());
    assert_eq!(
        (persisted.screens[0].rect.w, persisted.screens[0].rect.h),
        (344.2 / 686.0, 344.2 / 686.0 * 0.625)
    );
    server.stop();
    rt.block_on(node.shutdown());
}

#[test]
fn reset_uses_live_display_facts_without_os_writes() {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let node = rt.block_on(start(dir.path()));
    let server = rt.block_on(Server::start_on(node.clone(), 0)).unwrap();
    let remote = RemoteApi::new(dir.path());
    remote
        .set_screen_size(
            oink_agent::api::ScreenKey {
                device: node.id().into(),
                display: "main".into(),
            },
            Size::new(600.0 / 686.0, 375.0 / 686.0),
        )
        .unwrap();
    remote.reset_initial_layout().unwrap();
    assert_eq!(node.layout().0.screens[0].rect.w, 400.0 / 686.0);
    assert!(!dir.path().join("system-arrangement.json").exists());
    server.stop();
    rt.block_on(node.shutdown());
}

/// A startup probe may reach the new listener before its fresh endpoint file.
/// Rejection must not permanently poison the UI's pool with the stale token.
#[test]
fn rejected_hello_reloads_the_endpoint_on_the_next_probe() {
    let _scenario =
        test_watchdog::Scenario::new(module_path!(), "rejected_hello_reloads_the_endpoint_on_the_next_probe");
    let dir = std::env::temp_dir().join(format!("oink-remote-token-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let node = rt.block_on(start(&dir));
    let server = rt.block_on(Server::start_on(node, 0)).unwrap();
    let path = dir.join("ipc.json");
    let fresh = std::fs::read(&path).unwrap();
    let mut stale: serde_json::Value = serde_json::from_slice(&fresh).unwrap();
    stale["token"] = "previous-process-token".into();
    std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
    let remote = RemoteApi::new(&dir);
    assert!(matches!(remote.hello(), Err(ApiError::InvalidArgument { .. })));
    std::fs::write(&path, fresh).unwrap();
    assert_eq!(remote.hello().unwrap().pid, std::process::id());
    rt.block_on(server.close());
    rt.shutdown_timeout(Duration::from_millis(200));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Exercise the shipped CLI argument parser and stdout format against real local IPC.
/// Only this fixture's fake input backend is started; no installed application is used.
#[test]
fn generic_cli_process_calls_api_and_streams_json_events() {
    let _scenario =
        test_watchdog::Scenario::new(module_path!(), "generic_cli_process_calls_api_and_streams_json_events");
    use std::io::BufRead;
    use std::process::{Child, Command, Stdio};

    struct EventProcess(Child);
    impl Drop for EventProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let node = rt.block_on(start(dir.path()));
    let server = rt.block_on(Server::start_on(node.clone(), 0)).unwrap();
    let invoke = |args: &[&str]| {
        let result = Command::new(env!("CARGO_BIN_EXE_oink-agent"))
            .args(args)
            .env("OINK_HOME", dir.path())
            .output()
            .unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap()
    };
    let overview = invoke(&["call", "overview"]);
    assert_eq!(overview["this_device"]["id"], node.id().to_hex());

    let mut events = EventProcess(
        Command::new(env!("CARGO_BIN_EXE_oink-agent"))
            .arg("events")
            .env("OINK_HOME", dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdout = events.0.stdout.take().unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let event: ApiEvent = serde_json::from_str(&line.unwrap()).unwrap();
            if sent.send(event).is_err() {
                break;
            }
        }
    });
    // The initial snapshot proves the subscription exists before the CLI edit.
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    let pause = invoke(&[
        "call",
        "set_local_paused",
        r#"{"requestId":"00000000-0000-4000-8000-000000000001","paused":true}"#,
    ]);
    assert_eq!(pause["type"], "device_control_accepted");
    loop {
        if let ApiEvent::Pause(pause) = received.recv_timeout(Duration::from_secs(5)).unwrap()
            && pause.paused
        {
            break;
        }
    }
    assert!(node.pause_status().paused);
    rt.block_on(server.close());
    wait("CLI events exits after backend closes", || {
        events.0.try_wait().unwrap().is_some()
    });
    assert!(events.0.wait().unwrap().success());
    reader.join().unwrap();
    rt.block_on(node.shutdown());
}

#[path = "common/watchdog.rs"]
mod test_watchdog;
