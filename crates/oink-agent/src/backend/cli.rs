//! Typed command-line IPC clients: call, event subscription and explicit reopen.
//! These operations use the same authenticated local endpoint as the UI.

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use crate::api::{ApiEvent, Request};
use crate::ipc::{Client, Endpoint, Envelope};

/// `oink-agent call <method> [json-params]`: call any API method by name. `method` and
/// `params` form an `api::Request` (`{"method": …, "params": …}`); the reply is printed
/// as JSON. Unknown method or malformed parameters are an error before anything is sent.
pub fn call(data_dir: &Path, method: &str, params: Option<&str>) -> anyhow::Result<serde_json::Value> {
    let mut value = serde_json::json!({"method": method});
    if let Some(params) = params {
        value["params"] = serde_json::from_str(params)?;
    }
    let request: Request = serde_json::from_value(value)?;
    Client::connect(data_dir)?.call(request)
}

/// `oink-agent events`: subscribe and print every `ApiEvent` as one JSON line until the
/// backend goes away or the process is interrupted.
pub fn events(data_dir: &Path, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
    let endpoint = Endpoint::read(data_dir)?;
    let mut stream =
        std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], endpoint.port).into(), Duration::from_secs(2))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    serde_json::to_writer(
        &mut stream,
        &Envelope {
            token: endpoint.token,
            build: Some(crate::ipc::BuildIdentity::current()),
            subscribe: true,
            ..Envelope::default()
        },
    )?;
    stream.write_all(b"\n")?;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    anyhow::ensure!(reader.read_line(&mut line)? != 0, "后台未确认订阅");
    let reply: serde_json::Value = serde_json::from_str(&line)?;
    anyhow::ensure!(reply.get("ok").is_some(), "后台拒绝订阅：{reply}");
    reader.get_ref().set_read_timeout(None)?;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let value: serde_json::Value = serde_json::from_str(&line)?;
        if let Some(event) = value.get("event") {
            let event: ApiEvent = serde_json::from_value(event.clone())?;
            serde_json::to_writer(&mut *out, &event)?;
            out.write_all(b"\n")?;
            out.flush()?;
        } else if let Some(reason) = value.get("closing") {
            anyhow::ensure!(reason == crate::ipc::CLOSING_STOPPING, "订阅已断开：{reason}");
            return Ok(());
        }
    }
}

/// `oink-agent reopen`: does not use `AgentApi::reopen_oink`. Ask the backend to stop,
/// wait for it to exit, then start the entry process (macOS: the backend; Windows: the
/// tray-and-UI process without the autostart flag, which starts the backend). Works on
/// Windows when no tray-and-UI process is running.
pub fn reopen(data_dir: &Path) -> anyhow::Result<()> {
    let executable = std::env::current_exe()?;
    let parent = executable.parent().ok_or_else(|| anyhow::anyhow!("找不到应用目录"))?;
    let entry = parent.join(if cfg!(windows) {
        "oink.exe"
    } else {
        super::MAC_BACKEND_EXECUTABLE
    });
    anyhow::ensure!(entry.is_file(), "找不到应用入口：{}", entry.display());
    anyhow::ensure!(
        crate::host::stop_service(data_dir, Duration::from_secs(10))?,
        "后台未退出"
    );
    super::launch::reopen_entry(&entry, data_dir)?;
    Ok(())
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    fn server(
        respond: impl FnOnce(Envelope, &mut std::net::TcpStream) + Send + 'static,
    ) -> (tempfile::TempDir, std::thread::JoinHandle<()>) {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        std::fs::write(
            dir.path().join(crate::ipc::IPC_FILE),
            serde_json::to_vec(&Endpoint {
                port: listener.local_addr().unwrap().port(),
                token: "cli-test-token".into(),
            })
            .unwrap(),
        )
        .unwrap();
        let task = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Envelope = serde_json::from_str(&line).unwrap();
            assert_eq!(request.token, "cli-test-token");
            respond(request, &mut stream);
        });
        (dir, task)
    }

    #[test]
    fn generic_call_validates_before_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let error = call(dir.path(), "not_an_api", None).unwrap_err().to_string();
        assert!(error.contains("unknown variant"), "{error}");
        let error = call(
            dir.path(),
            "set_local_paused",
            Some(r#"{"requestId":"00000000-0000-4000-8000-000000000001","paused":5}"#),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("boolean"), "{error}");
    }

    #[test]
    fn generic_call_transmits_parameters_and_returns_json() {
        let (dir, task) = server(|request, stream| {
            assert!(matches!(
                request.call,
                Some(Request::SetLocalPaused { paused: true, .. })
            ));
            stream.write_all(b"{\"ok\":{\"accepted\":true}}\n").unwrap();
        });
        assert_eq!(
            call(
                dir.path(),
                "set_local_paused",
                Some(r#"{"requestId":"00000000-0000-4000-8000-000000000001","paused":true}"#)
            )
            .unwrap(),
            serde_json::json!({"accepted":true})
        );
        task.join().unwrap();
    }

    #[test]
    fn events_print_one_json_line_and_end_with_backend() {
        let (dir, task) = server(|request, stream| {
            assert!(request.subscribe);
            stream.write_all(b"{\"ok\":{}}\n").unwrap();
            let event = serde_json::json!({"event": ApiEvent::ShowWindow});
            serde_json::to_writer(&mut *stream, &event).unwrap();
            stream.write_all(b"\n{\"closing\":\"stopping\"}\n").unwrap();
        });
        let mut out = Vec::new();
        events(dir.path(), &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 1);
        task.join().unwrap();
    }
}
