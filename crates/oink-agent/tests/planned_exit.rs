#![cfg(debug_assertions)]
//! Disposable processes, FakeInput and private homes; never touches installed Oink.
use oink_agent::{host::AgentHost, instance, node::NodeOptions};
use std::{path::Path, process::Command, sync::Arc, time::Duration};

fn fixture_network(home: &Path) -> oink_network::NetworkConfig {
    let mut net = oink_test_support::network_config(home.into(), "exit-fixture", 0);
    // This fixture has no discovery peers. Allocate QUIC and IPC at their actual
    // bind, instead of probing a fixed test port that another process can take.
    net.port = 0;
    net
}

#[test]
fn exit_child() {
    let _scenario = watchdog::Scenario::new(module_path!(), "exit_child");
    let Ok(home) = std::env::var("OINK_TEST_EXIT_HOME") else {
        return;
    };
    let home = std::path::PathBuf::from(home);
    if std::env::var_os("OINK_TEST_EXIT_COMMAND").is_some() {
        assert!(oink_agent::host::stop_service(&home, Duration::from_secs(10)).unwrap());
        return;
    }
    // A file-only owner simulates a backend whose session-local mutex cannot
    // be seen by the CLI. Both still share the same home and authenticated IPC.
    let file_owner = std::env::var_os("OINK_TEST_FILE_OWNER").map(|_| {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(home.join("service.lock"))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&file).unwrap();
        file
    });
    let _lock = (file_owner.is_none() && std::env::var_os("OINK_TEST_NO_OWNER").is_none())
        .then(|| instance::acquire(&home).unwrap());
    let host = AgentHost::start_with_fake_platform(
        NodeOptions {
            net: fixture_network(&home),
            input: Arc::new(oink_test_support::input::FakeInput::new()),
            displays: vec![],
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: true,
            cursor_trail: false,
        },
        true,
    )
    .unwrap();
    std::fs::write(home.join("ready"), b"ready").unwrap();
    if std::env::var_os("OINK_TEST_EXIT_PANIC").is_some() {
        panic!("synthetic fatal host panic");
    }
    host.block_on(host.node().wait_for_stop());
    std::fs::write(home.join("stop-received"), b"stopping").unwrap();
    host.shutdown();
}

#[test]
fn cli_stop_reaches_an_owner_without_a_session_mutex() {
    let _scenario = watchdog::Scenario::new(module_path!(), "cli_stop_reaches_an_owner_without_a_session_mutex");
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["exit_child", "--exact", "--nocapture"])
        .env("OINK_TEST_EXIT_HOME", home.path())
        .env("OINK_TEST_FILE_OWNER", "1")
        .spawn()
        .unwrap();
    wait_for(&mut child, || home.path().join("ready").exists());
    let output = Command::new(env!("CARGO_BIN_EXE_oink-agent"))
        .arg("--home")
        .arg(home.path())
        .arg("stop")
        .output()
        .unwrap();
    let received = home.path().join("stop-received").exists();
    if !received {
        child.kill().unwrap();
    }
    let status = child.wait().unwrap();
    assert!(received, "CLI must deliver stop before reporting success: {output:?}");
    assert!(output.status.success(), "{output:?}");
    assert!(status.success());
    assert_eq!(instance::is_running(home.path()), Some(false));
}

#[test]
fn cli_stop_rejects_a_live_endpoint_without_visible_ownership() {
    let _scenario = watchdog::Scenario::new(
        module_path!(),
        "cli_stop_rejects_a_live_endpoint_without_visible_ownership",
    );
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["exit_child", "--exact", "--nocapture"])
        .env("OINK_TEST_EXIT_HOME", home.path())
        .env("OINK_TEST_NO_OWNER", "1")
        .spawn()
        .unwrap();
    wait_for(&mut child, || home.path().join("ready").exists());
    let output = Command::new(env!("CARGO_BIN_EXE_oink-agent"))
        .arg("--home")
        .arg(home.path())
        .arg("stop")
        .output()
        .unwrap();
    assert!(!home.path().join("stop-received").exists());
    // Cleanup is explicitly directed at this test's private endpoint, never
    // at an installed app or an endpoint selected through the caller's home.
    oink_agent::ipc::Client::connect(home.path()).unwrap().stop().unwrap();
    assert!(child.wait().unwrap().success());
    assert!(
        !output.status.success(),
        "a live unowned endpoint cannot mean success: {output:?}"
    );
    assert!(output.stdout.is_empty(), "false success payload: {output:?}");
}

fn wait_for(child: &mut std::process::Child, ready: impl Fn() -> bool) {
    let until = std::time::Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(child.try_wait().unwrap().is_none(), "fixture exited before readiness");
        if std::time::Instant::now() >= until {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("fixture timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn recoveries(home: &Path) -> usize {
    std::fs::read_dir(home.join("Logs/Diagnostics"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name().to_string_lossy().contains("-recovered_crash-")
                && e.path().extension().is_some_and(|e| e == "zip")
        })
        .count()
}

fn restart_and_check(home: &Path, expected: usize) {
    use oink_agent::api::AgentApi;
    watchdog::phase("restart AgentHost after child exit");
    let host = AgentHost::start_with_fake_platform(
        NodeOptions {
            net: fixture_network(home),
            input: Arc::new(oink_test_support::input::FakeInput::new()),
            displays: vec![],
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: true,
            cursor_trail: false,
        },
        false,
    )
    .unwrap();
    watchdog::phase("wait for crash recovery");
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while host.node().diagnostics_status().unwrap().recovering {
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(recoveries(home), expected);
    watchdog::phase("restarted host shutdown");
    host.shutdown();
}

#[test]
fn fatal_host_panic_keeps_crash_recovery() {
    let _scenario = watchdog::Scenario::new(module_path!(), "fatal_host_panic_keeps_crash_recovery");
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["exit_child", "--exact", "--nocapture"])
        .env("OINK_TEST_EXIT_HOME", home.path())
        .env("OINK_TEST_EXIT_PANIC", "1")
        .spawn()
        .unwrap();
    watchdog::phase(&format!("wait for synthetic-panic child pid={} to exit", child.id()));
    let status = child.wait().unwrap();
    assert!(!status.success());
    restart_and_check(home.path(), 1);
}

#[test]
fn ipc_stop_is_clean_but_process_kill_recovers() {
    let _scenario = watchdog::Scenario::new(module_path!(), "ipc_stop_is_clean_but_process_kill_recovers");
    for kill in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["exit_child", "--exact", "--nocapture"])
            .env("OINK_TEST_EXIT_HOME", home.path())
            .spawn()
            .unwrap();
        wait_for(&mut child, || home.path().join("ready").exists());
        if kill {
            child.kill().unwrap();
        } else {
            assert!(oink_agent::host::stop_service(home.path(), Duration::from_secs(10)).unwrap());
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("shutdown timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(child.wait().unwrap().success(), !kill);
        restart_and_check(home.path(), usize::from(kill));
    }
}

#[path = "common/watchdog.rs"]
mod watchdog;
