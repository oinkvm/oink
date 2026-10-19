#![cfg(debug_assertions)]
//! 防回归（bug 7）：空闲的服务进程不能占满 CPU。
//!
//! 以独立测试子进程运行真实 Node/IPC 和 FakeInput，在启动完成后的
//! 3 秒空闲窗口里读进程的 CPU 时间。任何一个线程空转都会让它接近 3 秒；正常空闲只有几十毫秒。
//! 只在 macOS 上跑：用 `ps -o time=` 读 CPU 时间（精度 10 ms）。

#![cfg(target_os = "macos")]

use std::process::{Command, Stdio};
use std::time::Duration;

/// `ps -o time=` 的输出（`[[hh:]mm:]ss.cc`）转成秒。
fn cpu_seconds(pid: u32) -> Option<f64> {
    let out = Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    s.split(':')
        .try_fold(0.0, |acc, part| part.parse::<f64>().ok().map(|v| acc * 60.0 + v))
}

#[test]
#[ignore = "CPU timing qualification requires an otherwise idle dedicated machine"]
fn idle_service_does_not_spin() {
    let _scenario = test_watchdog::Scenario::new(module_path!(), "idle_service_does_not_spin");
    let dir = std::env::temp_dir().join(format!("oink-idle-cpu-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("stderr.log");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "idle_fake_service_child", "--nocapture"])
        .env("OINK_IDLE_CHILD", &dir)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("spawn oink-agent");
    let pid = child.id();

    std::thread::sleep(Duration::from_millis(2500));
    let t0 = cpu_seconds(pid);
    std::thread::sleep(Duration::from_secs(3));
    let t1 = cpu_seconds(pid);
    let status = child.wait().expect("wait oink-agent");
    let err = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);

    let (Some(t0), Some(t1)) = (t0, t1) else {
        panic!("服务进程提前退出（{status}）：{err}");
    };
    let busy = t1 - t0;
    assert!(busy < 0.9, "空闲 3 秒内服务进程用了 {busy:.2} 秒 CPU（有线程在空转）");
    assert!(status.success(), "服务进程退出码 {status}：{err}");
}

/// This child exercises the production owners without installing native input hooks.
#[test]
fn idle_fake_service_child() {
    let _scenario = test_watchdog::Scenario::new(module_path!(), "idle_fake_service_child");
    let Some(directory) = std::env::var_os("OINK_IDLE_CHILD") else {
        return;
    };
    let host = oink_agent::host::AgentHost::start_with_fake_platform(
        oink_agent::node::NodeOptions {
            net: oink_test_support::network_config(directory.into(), "idle", 0),
            input: std::sync::Arc::new(oink_test_support::input::FakeInput::new()),
            displays: vec![],
            display_backend: None,
            clipboard: Some(std::sync::Arc::new(oink_test_support::clipboard::FakeClipboard::new())),
            mode_switch_in_callback: true,
            capture_reports_key_repeat: false,
            cursor_trail: false,
        },
        true,
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(7));
    host.shutdown();
}

#[path = "common/watchdog.rs"]
mod test_watchdog;
