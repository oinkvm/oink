//! Private homes and closed anonymous pipes; no installed app or desktop access.
use std::process::{Command, Output, Stdio};

fn closed_output(args: &[&str], stderr: bool) -> Output {
    let home = tempfile::tempdir().unwrap();
    let (reader, writer) = std::io::pipe().unwrap();
    // Close before spawn so even a single short write deterministically fails.
    drop(reader);
    let mut command = Command::new(env!("CARGO_BIN_EXE_oink-agent"));
    command.arg("--home").arg(home.path()).args(args);
    if stderr {
        command.stderr(Stdio::from(writer));
    } else {
        command.stdout(Stdio::from(writer));
    }
    command.output().unwrap()
}

#[test]
fn closed_stdout_exits_quietly_without_abort() {
    for args in [
        &["config", "path"][..],
        &["config", "show"],
        &["config", "init"],
        &["stop"],
        &["--help"],
        &["--version"],
    ] {
        let output = closed_output(args, false);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
    }
}

#[test]
fn closed_stderr_exits_quietly_without_abort() {
    // Invalid call parameters fail before IPC, then exercise the error reporter.
    for args in [
        &["call", "not_an_api"][..],
        &["not_a_command"],
        &["diagnostics", "save"],
    ] {
        let output = closed_output(args, true);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
    }
}

#[test]
#[cfg(windows)] // Unix stdio deliberately ignores EBADF from read-only handles.
fn non_pipe_output_error_is_reported() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("read-only-output");
    std::fs::write(&file, b"").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_oink-agent"))
        .arg("--home")
        .arg(home.path())
        .args(["config", "path"])
        .stdout(std::fs::File::open(file).unwrap())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("错误："), "{output:?}");
}

#[test]
fn logging_to_closed_stderr_does_not_abort() {
    let home = tempfile::tempdir().unwrap();
    let mut config = oink_agent::config::AgentConfig::load_from(home.path()).unwrap();
    config.save().unwrap();
    let path = config.settings_path();
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["local"]["log_level"] = serde_json::json!(42);
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = Command::new(env!("CARGO_BIN_EXE_oink-agent"))
        .arg("--home")
        .arg(home.path())
        .args(["call", "not_an_api"])
        .stderr(Stdio::from(writer))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn ordinary_errors_remain_reported() {
    let home = tempfile::tempdir().unwrap();
    for (args, code, message) in [
        (&["call", "not_an_api"][..], 1, "unknown variant"),
        (&["not_a_command"][..], 2, "unrecognized subcommand"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_oink-agent"))
            .arg("--home")
            .arg(home.path())
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(message), "{output:?}");
    }
}

#[test]
fn closed_event_stdout_exits_quietly_without_abort() {
    use std::io::{BufRead, Write};
    let home = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    std::fs::write(
        home.path().join(oink_agent::ipc::IPC_FILE),
        serde_json::to_vec(&serde_json::json!({
            "port": listener.local_addr().unwrap().port(),
            "token": "cli-output-test",
        }))
        .unwrap(),
    )
    .unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut request = String::new();
        std::io::BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut request)
            .unwrap();
        let request: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(request["subscribe"], true);
        stream
            .write_all(b"{\"ok\":{}}\n{\"event\":{\"type\":\"show_window\"}}\n")
            .unwrap();
    });
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = Command::new(env!("CARGO_BIN_EXE_oink-agent"))
        .arg("--home")
        .arg(home.path())
        .arg("events")
        .stdout(Stdio::from(writer))
        .output()
        .unwrap();
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}
