//! Explicit product Quit reaches the UI even after its backend subscription ends.
//! This endpoint admits authenticated open/stop only, never business API requests.
use super::*;

const FILE: &str = "ui-ipc.json";

#[derive(Debug)]
pub struct UiControl {
    task: tokio::task::JoinHandle<()>,
    file: PathBuf,
}

impl UiControl {
    pub(crate) fn verify_stopped(data_dir: &Path) -> anyhow::Result<()> {
        verify_no_listener(data_dir, FILE)
    }

    /// The UI owns its instance lock before publishing this endpoint.
    pub async fn start(data_dir: &Path, quit: impl FnOnce() + Send + 'static) -> anyhow::Result<Self> {
        Self::start_with_open(data_dir, quit, || {}).await
    }

    /// Authenticated open works across Medium/High integrity without relaxing
    /// the native window message filter for Low-integrity or other-user callers.
    pub async fn start_with_open(
        data_dir: &Path,
        quit: impl FnOnce() + Send + 'static,
        open: impl Fn() + Send + 'static,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|error| anyhow::anyhow!("UI IPC token entropy: {error}"))?;
        let endpoint = Endpoint {
            port: listener.local_addr()?.port(),
            token: secret.iter().map(|byte| format!("{byte:02x}")).collect(),
        };
        let file = data_dir.join(FILE);
        write_private(&file, &serde_json::to_string(&endpoint)?)?;
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let Ok(Ok(Some(line))) = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await else {
                    continue;
                };
                let request = serde_json::from_str::<Envelope>(&line).ok().filter(|request| {
                    request.token == endpoint.token && (request.stop || request.open_ui) && request.call.is_none()
                });
                let accepted = request.is_some();
                let stop = request.is_some_and(|request| request.stop);
                let reply = if accepted {
                    serde_json::json!({"ok": null})
                } else {
                    serde_json::json!({"err": ApiError::InvalidArgument { message: "UI endpoint accepts only authenticated open or stop".into() }})
                };
                let _ = tokio::time::timeout(Duration::from_secs(1), write_line(&mut writer, &reply)).await;
                if accepted && !stop {
                    open();
                }
                if stop {
                    tracing::info!("explicit product quit received by UI; exiting all windows and panels");
                    quit();
                    return;
                }
            }
        });
        Ok(Self { task, file })
    }

    /// Show the existing UI after the caller grants foreground permission.
    pub fn request_open(data_dir: &Path) -> anyhow::Result<()> {
        // Ownership/foreground HWND can precede setup while the first UI starts
        // its backend. Wait for its endpoint, including a stale file from a crash.
        // Retry only missing-file/refused-connect failures, before any delivery;
        // never replay an open after a sent request fails or loses its reply.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let result: anyhow::Result<()> = (|| {
                let client = Client {
                    endpoint: Endpoint::read_file(data_dir, FILE)?,
                    client_id: api::request_context::new_client(),
                };
                client.roundtrip_with_timeout(
                    &Envelope {
                        open_ui: true,
                        ..client.envelope()
                    },
                    Some(Duration::from_secs(3)),
                )?;
                Ok(())
            })();
            match result {
                Err(error)
                    if error.chain().any(|cause| {
                        cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
                            matches!(
                                e.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                            )
                        })
                    }) && std::time::Instant::now() < deadline
                        && crate::instance::is_ui_running(data_dir) == Some(true) =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                result => return result,
            }
        }
    }

    /// Build-independent maintenance operation, like backend stop. Ownership
    /// must be checked by the caller; a leftover endpoint file proves nothing.
    pub fn request_stop(data_dir: &Path, timeout: Duration) -> anyhow::Result<()> {
        let client = Client {
            endpoint: Endpoint::read_file(data_dir, FILE)?,
            client_id: api::request_context::new_client(),
        };
        client.roundtrip_with_timeout(
            &Envelope {
                stop: true,
                ..client.envelope()
            },
            Some(timeout),
        )?;
        Ok(())
    }
}

impl Drop for UiControl {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_ui_without_visible_ownership_cannot_mean_successful_stop() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let control = runtime
            .block_on(UiControl::start(dir.path(), || panic!("unowned stop")))
            .unwrap();
        assert!(crate::host::stop_service(dir.path(), Duration::ZERO).is_err());
        drop(control);
        assert!(crate::host::stop_service(dir.path(), Duration::ZERO).unwrap());
    }

    #[test]
    fn ui_open_waits_for_the_starting_owners_endpoint() {
        for stale in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let owner = crate::instance::acquire_ui(dir.path()).unwrap();
            if stale {
                let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
                let port = listener.local_addr().unwrap().port();
                drop(listener);
                write_private(
                    &dir.path().join(FILE),
                    &serde_json::to_string(&Endpoint {
                        port,
                        token: "stale".into(),
                    })
                    .unwrap(),
                )
                .unwrap();
            }
            let path = dir.path().to_owned();
            let opening = std::thread::spawn(move || UiControl::request_open(&path));
            // Leave the endpoint absent/stale briefly, as during backend startup.
            std::thread::sleep(Duration::from_millis(50));
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let (opened, requests) = std::sync::mpsc::channel();
            let control = runtime
                .block_on(UiControl::start_with_open(
                    dir.path(),
                    || {},
                    move || opened.send(()).unwrap(),
                ))
                .unwrap();
            opening.join().unwrap().unwrap();
            requests.recv_timeout(Duration::from_secs(1)).unwrap();
            drop(control);
            drop(owner);
        }
    }

    #[test]
    fn ui_open_requires_the_secret_and_does_not_quit_the_ui() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (opened, requests) = std::sync::mpsc::channel();
        let control = runtime
            .block_on(UiControl::start_with_open(
                dir.path(),
                || panic!("open quit UI"),
                move || opened.send(()).unwrap(),
            ))
            .unwrap();
        let mut client = Client {
            endpoint: Endpoint::read_file(dir.path(), FILE).unwrap(),
            client_id: api::request_context::new_client(),
        };
        client.endpoint.token = "wrong".into();
        assert!(
            client
                .roundtrip_with_timeout(
                    &Envelope {
                        open_ui: true,
                        ..client.envelope()
                    },
                    Some(Duration::from_secs(1))
                )
                .is_err()
        );
        assert!(requests.try_recv().is_err());
        UiControl::request_open(dir.path()).unwrap();
        requests.recv_timeout(Duration::from_secs(1)).unwrap();
        UiControl::request_open(dir.path()).unwrap();
        requests.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(control);
    }

    #[test]
    fn ui_control_accepts_only_authenticated_quit_and_removes_its_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (quit, requested) = std::sync::mpsc::channel();
        let control = runtime
            .block_on(UiControl::start(dir.path(), move || quit.send(()).unwrap()))
            .unwrap();
        let mut client = Client {
            endpoint: Endpoint::read_file(dir.path(), FILE).unwrap(),
            client_id: api::request_context::new_client(),
        };
        let token = client.endpoint.token.clone();
        client.endpoint.token = "wrong".into();
        assert!(client.stop().is_err());
        client.endpoint.token = token;
        assert!(client.call(Request::Status).is_err());
        assert!(requested.try_recv().is_err());
        UiControl::request_stop(dir.path(), Duration::from_secs(1)).unwrap();
        requested.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(control);
        assert!(!dir.path().join(FILE).exists());
    }
}
