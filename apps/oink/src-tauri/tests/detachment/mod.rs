//! Test-private child roles exercise detachment; no fixture bin enters the product.
use super::{TestProcess, wait_marker};
use std::process::{Command, Stdio};

#[test]
fn detached_child() {
    let Some(dir) = std::env::var_os("OINK_TEST_DETACH_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    if std::env::var_os("OINK_TEST_DETACH_CALLER").is_some() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "detachment::detached_child", "--nocapture"])
            .env_remove("OINK_TEST_DETACH_CALLER");
        let mut child = oink_agent::backend::launch::spawn_detached(&mut command).unwrap();
        std::fs::write(dir.join("caller-ready"), child.id().to_string()).unwrap();
        wait_marker(&dir, "caller-stop");
        child.wait().unwrap();
        return;
    }
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    assert!(input.is_empty(), "child inherited caller stdin");
    let report = serde_json::json!({"pid": std::process::id(), "lifetime": oink_agent::backend::launch::lifetime()});
    #[cfg(unix)]
    let report = {
        let mut report = report;
        // SAFETY: query only this private child's session/process group.
        unsafe {
            report["sid"] = libc::getsid(0).into();
            report["pgid"] = libc::getpgrp().into();
        }
        report
    };
    // The parent waits for the path before reading JSON. Publish atomically so
    // path visibility cannot race the write and expose an empty report.
    std::fs::write(dir.join("detached.json.tmp"), report.to_string()).unwrap();
    std::fs::rename(dir.join("detached.json.tmp"), dir.join("detached.json")).unwrap();
    wait_marker(&dir, "survive");
    std::fs::write(dir.join("detached-alive"), []).unwrap();
}

#[test]
fn detached_private_child_outlives_its_caller() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stdin"), "caller-only secret").unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "detachment::detached_child", "--nocapture"])
        .env("OINK_TEST_DETACH_DIR", dir.path())
        .env("OINK_TEST_DETACH_CALLER", "1")
        .stdin(std::fs::File::open(dir.path().join("stdin")).unwrap())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut caller = TestProcess(command.spawn().unwrap());
    wait_marker(dir.path(), "caller-ready");
    wait_marker(dir.path(), "detached.json");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("detached.json")).unwrap()).unwrap();
    #[cfg(unix)]
    {
        // SAFETY: query only our still-live private caller child.
        let caller_session = unsafe { libc::getsid(caller.0.id() as i32) };
        // SAFETY: same child PID, no memory access or signal is involved.
        let caller_group = unsafe { libc::getpgid(caller.0.id() as i32) };
        assert_ne!(report["sid"], serde_json::json!(caller_session));
        assert_ne!(report["pgid"], serde_json::json!(caller_group));
    }
    let _ = report;
    caller.kill();
    std::fs::write(dir.path().join("survive"), []).unwrap();
    wait_marker(dir.path(), "detached-alive");
}
