#![cfg(debug_assertions)]
//! Isolated IPC lifecycle tests. The input backend is fake; no installed app is used.
use oink_agent::ipc::Server;
use oink_agent::node::{Node, NodeOptions};
use oink_agent::remote::{EventsEnd, RemoteApi};
use oink_foundation::display::DisplayInfo;
use oink_foundation::{Point, Rect, Size};
use oink_test_support::input::FakeInput;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;
#[cfg(any(target_os = "macos", windows))]
mod detachment;
struct Svc {
    rt: tokio::runtime::Runtime,
    node: Node,
    server: Option<Server>,
}

fn start(dir: &Path, port: u16) -> Svc {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let input = FakeInput::new();
    input.set_cursor(Point::new(800.0, 500.0));
    let node = rt
        // The fixture owns private IPC/data and fake input. It must not attach
        // to the installed update/input service when run in a desktop session.
        .block_on(Node::start_with_fake_platform(NodeOptions {
            net: oink_test_support::network_config(dir.to_owned(), "svc", port),
            input: Arc::new(input),
            displays: vec![DisplayInfo {
                key: "main".into(),
                name: "main".into(),
                bounds: Rect::new(0.0, 0.0, 1600.0, 1000.0),
                pixel_width: 1600,
                pixel_height: 1000,
                reference_extent: Some(Size::new(400.0, 250.0)),
                rotation_deg: 0,
                is_primary: true,
                is_builtin: false,
                is_virtual: false,
            }],
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: true,
            cursor_trail: false,
        }))
        .unwrap();
    let server = rt.block_on(Server::start_on(node.clone(), 0)).unwrap();
    Svc {
        rt,
        node,
        server: Some(server),
    }
}

impl Svc {
    /// 像服务进程一样：收到 stop 后关闭 IPC（先通知订阅者）再退出。
    fn serve_until_stopped(mut self) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            self.rt.block_on(self.node.wait_for_stop());
            if let Some(s) = self.server.take() {
                self.rt.block_on(s.close());
            }
            self.rt.block_on(self.node.shutdown());
        })
    }

    /// 像崩溃一样：不通知订阅者，连接直接断开。
    fn crash(mut self) {
        drop(self.server.take());
        self.rt.shutdown_timeout(Duration::from_millis(200));
    }
}

#[test]
fn reachable_but_unowned_endpoint_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let service = start(dir.path(), 0);
    let remote = RemoteApi::new(dir.path());
    assert!(remote.hello().is_ok());
    assert!(oink_ui::service::check_backend_ownership(dir.path()).is_err());
    let lock = oink_agent::instance::acquire(dir.path()).unwrap();
    assert!(oink_ui::service::check_backend_ownership(dir.path()).is_ok());
    drop(lock);
    assert!(oink_ui::service::check_backend_ownership(dir.path()).is_err());
    assert!(oink_agent::host::stop_service(dir.path(), Duration::from_millis(100)).is_err());
    assert!(remote.hello().is_ok(), "unowned endpoint received a stop request");
    service.crash();
}

#[test]
fn product_stop_cannot_succeed_while_a_ui_still_owns_the_home() {
    let dir = tempfile::tempdir().unwrap();
    let _ui = oink_agent::instance::acquire_ui(dir.path()).unwrap();
    assert!(
        !oink_agent::host::stop_service(dir.path(), Duration::from_millis(40)).unwrap(),
        "backend absence alone does not mean Oink has quit"
    );
}

#[test]
fn ipc_rejects_missing_or_different_build_before_serving_ui_calls() {
    use std::io::{BufRead, Write};
    let dir = tempfile::tempdir().unwrap();
    let service = start(dir.path(), 0);
    let endpoint: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("ipc.json")).unwrap()).unwrap();
    let current = serde_json::to_value(oink_agent::ipc::BuildIdentity::current()).unwrap();
    let mut mismatches = vec![serde_json::Value::Null];
    for field in ["version", "build", "source"] {
        let mut identity = current.clone();
        identity[field] = "different".into();
        mismatches.push(identity);
    }
    for build in mismatches {
        for operation in ["hello", "subscribe", "ui_ready", "call"] {
            let mut request = serde_json::json!({"token": endpoint["token"]});
            if !build.is_null() {
                request["build"] = build.clone();
            }
            request[operation] = if operation == "call" {
                serde_json::json!({"method":"status"})
            } else {
                true.into()
            };
            let mut stream =
                std::net::TcpStream::connect(("127.0.0.1", endpoint["port"].as_u64().unwrap() as u16)).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            writeln!(stream, "{request}").unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream).read_line(&mut line).unwrap();
            let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(reply["closing"], "build_mismatch", "{operation}: {reply}");
            assert!(reply.get("ok").is_none());
        }
    }
    // Maintenance must still be able to stop an older/different installed build.
    let stopped = service.serve_until_stopped();
    RemoteApi::new(dir.path()).request_stop().unwrap();
    stopped.join().unwrap();
}

#[test]
fn backend_disappearance_ends_ui_subscription_without_restart() {
    let _scenario = watchdog::Scenario::new(
        module_path!(),
        "backend_disappearance_ends_ui_subscription_without_restart",
    );
    for crash in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let service = start(dir.path(), 0);
        let remote = RemoteApi::new(dir.path());
        let follower = remote.clone();
        let (connected_tx, connected_rx) = std::sync::mpsc::channel();
        let (ended_tx, ended_rx) = std::sync::mpsc::channel();
        let ui = std::thread::spawn(move || {
            oink_ui::service::follow(
                &follower,
                |_| connected_tx.send(()).unwrap(),
                |end| ended_tx.send(end).unwrap(),
            )
        });
        connected_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if crash {
            service.crash();
        } else {
            let stopped = service.serve_until_stopped();
            remote.request_stop().unwrap();
            stopped.join().unwrap();
        }
        let end = ended_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(if crash {
            matches!(end, EventsEnd::Lost(_))
        } else {
            end == EventsEnd::Stopping
        });
        ui.join().unwrap();
        assert!(RemoteApi::new(dir.path()).hello().is_err());
    }
}

#[test]
fn ui_disappearance_keeps_backend_available() {
    let _scenario = watchdog::Scenario::new(module_path!(), "ui_disappearance_keeps_backend_available");
    let dir = tempfile::tempdir().unwrap();
    let service = start(dir.path(), 0);
    let remote = RemoteApi::new(dir.path());
    let follower = remote.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let ui = std::thread::spawn(move || {
        oink_ui::service::follow(
            &follower,
            |_| tx.send(()).unwrap(),
            |end| assert_eq!(end, EventsEnd::Cancelled),
        )
    });
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    watchdog::phase("IPC cancellation");
    remote.stop_following();
    watchdog::phase("join cancelled subscription reader");
    ui.join().unwrap();
    drop(remote);
    assert!(RemoteApi::new(dir.path()).hello().is_ok());
    let stopped = service.serve_until_stopped();
    RemoteApi::new(dir.path()).request_stop().unwrap();
    stopped.join().unwrap();
}

// The child is this test executable, with fake input and its own data directory.
// No test launch mode or fixture is compiled into either application executable.
struct TestProcess(Child);

impl TestProcess {
    fn spawn(dir: &Path, role: &str) -> Self {
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "process_fixture", "--nocapture"])
                .env("OINK_TEST_LIFETIME_DIR", dir)
                .env("OINK_TEST_LIFETIME_ROLE", role)
                .spawn()
                .unwrap(),
        )
    }

    fn exited(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "child exited with {status}");
                return;
            }
            assert!(std::time::Instant::now() < deadline, "child did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for TestProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_marker(dir: &Path, name: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !dir.join(name).exists() {
        assert!(std::time::Instant::now() < deadline, "missing marker {name}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn process_fixture() {
    let _scenario = watchdog::Scenario::new(module_path!(), "process_fixture");
    let Some(dir) = std::env::var_os("OINK_TEST_LIFETIME_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    match std::env::var("OINK_TEST_LIFETIME_ROLE").unwrap().as_str() {
        "ui-only" => {
            let _lock = oink_agent::instance::acquire_ui(&dir).unwrap();
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let (quit, requested) = std::sync::mpsc::channel();
            let _control = runtime
                .block_on(oink_agent::ipc::UiControl::start(&dir, move || {
                    quit.send(()).unwrap();
                }))
                .unwrap();
            std::fs::write(dir.join("ui-ready"), []).unwrap();
            requested.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        "backend" | "update-backend" | "native-backend" => {
            if std::env::var("OINK_TEST_LIFETIME_ROLE").unwrap() == "native-backend" {
                oink_agent::logs::init_service();
            }
            let _lock = oink_agent::instance::acquire(&dir).unwrap();
            let service = start(&dir, 0);
            std::fs::write(
                dir.join("backend-lifetime.json"),
                serde_json::to_vec(&oink_agent::backend::launch::lifetime()).unwrap(),
            )
            .unwrap();
            std::fs::write(dir.join("backend-ready"), []).unwrap();
            if std::env::var("OINK_TEST_LIFETIME_ROLE").unwrap() == "update-backend" {
                let node = service.node.clone();
                let path = dir.clone();
                std::thread::spawn(move || {
                    wait_marker(&path, "emit-update");
                    node.emit(oink_agent::api::ApiEvent::ExitForUpdate);
                });
            }
            if std::env::var("OINK_TEST_LIFETIME_ROLE").unwrap() == "native-backend" {
                let node = service.node.clone();
                let path = dir.clone();
                std::thread::spawn(move || {
                    let deadline = std::time::Instant::now() + Duration::from_secs(90);
                    while std::time::Instant::now() < deadline {
                        let command = path.join("native-event.json");
                        if let Ok(bytes) = std::fs::read(&command) {
                            let event = serde_json::from_slice(&bytes).unwrap();
                            std::fs::remove_file(command).unwrap();
                            node.emit(event);
                        }
                        if path.join("native-stop").exists() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    node.request_stop();
                });
            }
            service.serve_until_stopped().join().unwrap();
        }
        "ui" => {
            let _lock = oink_agent::instance::acquire_ui(&dir).unwrap();
            use oink_agent::api::{AgentApi, ApiEvent};
            let remote = RemoteApi::new(&dir);
            let cancelled = remote.clone();
            remote.subscribe(Arc::new(move |event| {
                if matches!(event, ApiEvent::ExitForUpdate) {
                    cancelled.stop_following();
                }
            }));
            oink_ui::service::follow(
                &remote,
                |_| {
                    oink_agent::ipc::Client::connect(&dir).unwrap().ui_ready().unwrap();
                    std::fs::write(dir.join("ui-ready"), []).unwrap();
                },
                |end| {
                    assert!(matches!(
                        end,
                        EventsEnd::Stopping | EventsEnd::Lost(_) | EventsEnd::Cancelled
                    ))
                },
            );
        }
        "duplicate-backend" | "duplicate-ui" => {
            let role = std::env::var("OINK_TEST_LIFETIME_ROLE").unwrap();
            let result = if role == "duplicate-ui" {
                oink_agent::instance::acquire_ui(&dir)
            } else {
                oink_agent::instance::acquire(&dir)
            };
            assert!(
                result
                    .unwrap_err()
                    .downcast_ref::<oink_agent::instance::AlreadyRunning>()
                    .is_some()
            );
        }
        "reopen-wait" => {
            std::fs::write(dir.join("wait-ready"), []).unwrap();
            oink_ui::service::wait_previous_pair(&dir).unwrap();
            let _backend = oink_agent::instance::acquire(&dir).unwrap();
            let _ui = oink_agent::instance::acquire_ui(&dir).unwrap();
            std::fs::write(dir.join("pair-released"), []).unwrap();
        }
        role => panic!("unknown fixture role {role}"),
    }
}

#[test]
fn backend_process_exit_ends_client_process_without_restart() {
    let _scenario = watchdog::Scenario::new(
        module_path!(),
        "backend_process_exit_ends_client_process_without_restart",
    );
    for crash in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut backend = TestProcess::spawn(dir.path(), "backend");
        wait_marker(dir.path(), "backend-ready");
        let mut ui = TestProcess::spawn(dir.path(), "ui");
        wait_marker(dir.path(), "ui-ready");
        if crash {
            backend.kill();
        } else {
            RemoteApi::new(dir.path()).request_stop().unwrap();
            backend.exited();
        }
        ui.exited();
        assert_eq!(oink_agent::instance::is_running(dir.path()), Some(false));
        assert_eq!(oink_agent::instance::is_ui_running(dir.path()), Some(false));
        assert!(RemoteApi::new(dir.path()).hello().is_err());
    }
}

#[test]
fn client_process_death_leaves_backend_process_serving_ipc() {
    let _scenario = watchdog::Scenario::new(
        module_path!(),
        "client_process_death_leaves_backend_process_serving_ipc",
    );
    let dir = tempfile::tempdir().unwrap();
    let mut backend = TestProcess::spawn(dir.path(), "backend");
    wait_marker(dir.path(), "backend-ready");
    let mut ui = TestProcess::spawn(dir.path(), "ui");
    wait_marker(dir.path(), "ui-ready");
    ui.kill();
    assert!(backend.0.try_wait().unwrap().is_none());
    assert_eq!(oink_agent::instance::is_running(dir.path()), Some(true));
    let remote = RemoteApi::new(dir.path());
    assert!(remote.hello().is_ok());
    let expected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("backend-lifetime.json")).unwrap()).unwrap();
    assert_eq!(
        oink_agent::api::AgentApi::status(&remote).unwrap().backend_lifetime,
        expected["status"]
    );
    assert_eq!(
        serde_json::to_value(oink_agent::api::AgentApi::status(&remote).unwrap().backend_uiaccess).unwrap(),
        expected["uiaccess"]
    );
    remote.request_stop().unwrap();
    backend.exited();
}

#[test]
fn reopen_waits_for_both_process_roles_in_either_exit_order() {
    let _scenario = watchdog::Scenario::new(
        module_path!(),
        "reopen_waits_for_both_process_roles_in_either_exit_order",
    );
    for backend_first in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let backend = oink_agent::instance::acquire(dir.path()).unwrap();
        let ui = oink_agent::instance::acquire_ui(dir.path()).unwrap();
        let mut reopen = TestProcess::spawn(dir.path(), "reopen-wait");
        wait_marker(dir.path(), "wait-ready");
        let (first, last) = if backend_first { (backend, ui) } else { (ui, backend) };
        drop(first);
        std::thread::sleep(Duration::from_millis(150));
        assert!(!dir.path().join("pair-released").exists());
        assert!(reopen.0.try_wait().unwrap().is_none());
        drop(last);
        reopen.exited();
        assert!(dir.path().join("pair-released").exists());
    }
}

#[test]
fn update_event_exits_only_the_private_ui_child() {
    let dir = tempfile::tempdir().unwrap();
    let mut backend = TestProcess::spawn(dir.path(), "update-backend");
    wait_marker(dir.path(), "backend-ready");
    let mut ui = TestProcess::spawn(dir.path(), "ui");
    wait_marker(dir.path(), "ui-ready");
    std::fs::write(dir.path().join("emit-update"), []).unwrap();
    ui.exited();
    assert!(backend.0.try_wait().unwrap().is_none());
    let remote = RemoteApi::new(dir.path());
    assert!(remote.hello().is_ok());
    remote.request_stop().unwrap();
    backend.exited();
}

#[test]
fn both_roles_reject_duplicate_private_children_and_release_locks_on_exit() {
    let dir = tempfile::tempdir().unwrap();
    let mut backend = TestProcess::spawn(dir.path(), "backend");
    wait_marker(dir.path(), "backend-ready");
    let mut ui = TestProcess::spawn(dir.path(), "ui");
    wait_marker(dir.path(), "ui-ready");
    for role in ["duplicate-backend", "duplicate-ui"] {
        TestProcess::spawn(dir.path(), role).exited();
    }
    RemoteApi::new(dir.path()).request_stop().unwrap();
    backend.exited();
    ui.exited();
    // Persistent lock files do not mean a live instance still owns them.
    let _backend = oink_agent::instance::acquire(dir.path()).unwrap();
    let _ui = oink_agent::instance::acquire_ui(dir.path()).unwrap();
}

#[test]
fn product_stop_waits_for_the_ui_after_the_backend_has_exited() {
    let dir = tempfile::tempdir().unwrap();
    let mut backend = TestProcess::spawn(dir.path(), "backend");
    wait_marker(dir.path(), "backend-ready");
    let ui = oink_agent::instance::acquire_ui(dir.path()).unwrap();
    let path = dir.path().to_owned();
    let stopping = std::thread::spawn(move || oink_agent::host::stop_service(&path, Duration::from_secs(3)));
    backend.exited();
    assert!(!stopping.is_finished(), "stop succeeded with a live UI owner");
    drop(ui);
    assert!(stopping.join().unwrap().unwrap());
}

#[test]
fn product_quit_ends_a_ui_process_even_without_a_backend_subscription() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = TestProcess::spawn(dir.path(), "ui-only");
    wait_marker(dir.path(), "ui-ready");
    assert_eq!(oink_agent::instance::is_running(dir.path()), Some(false));
    assert!(oink_agent::host::stop_service(dir.path(), Duration::from_millis(200)).unwrap());
    ui.exited();
    assert_eq!(oink_agent::instance::is_ui_running(dir.path()), Some(false));
}

#[test]
fn ui_rejects_a_backend_build_before_connected_or_events_are_delivered() {
    use std::io::{BufRead, Write};
    for subscribe in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        std::fs::write(
            dir.path().join("ipc.json"),
            serde_json::json!({"port":listener.local_addr().unwrap().port(),"token":"private-test"}).to_string(),
        )
        .unwrap();
        let backend = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            std::io::BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let mut info = oink_agent::ipc::ServiceInfo::current();
            info.build.build.push_str("-different");
            writeln!(stream, "{}", serde_json::json!({"ok":info})).unwrap();
        });
        let remote = RemoteApi::new(dir.path());
        if subscribe {
            assert_eq!(
                remote.follow_events(|_| panic!("mismatched backend admitted UI")),
                EventsEnd::BuildMismatch
            );
        } else {
            assert!(remote.hello().is_err());
        }
        assert!(remote.build_mismatch());
        backend.join().unwrap();
    }
}

#[path = "../../../../crates/oink-agent/tests/common/watchdog.rs"]
mod watchdog;
