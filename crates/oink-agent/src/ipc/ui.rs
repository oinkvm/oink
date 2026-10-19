//! Explicit product Quit reaches the UI even after its backend subscription ends.
//! This endpoint admits only authenticated stop, never business API requests.
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
                let accepted = serde_json::from_str::<Envelope>(&line)
                    .is_ok_and(|request| request.token == endpoint.token && request.stop && request.call.is_none());
                let reply = if accepted {
                    serde_json::json!({"ok": null})
                } else {
                    serde_json::json!({"err": ApiError::InvalidArgument { message: "UI endpoint accepts only authenticated stop".into() }})
                };
                let _ = tokio::time::timeout(Duration::from_secs(1), write_line(&mut writer, &reply)).await;
                if accepted {
                    tracing::info!("explicit product quit received by UI; exiting all windows and panels");
                    quit();
                    return;
                }
            }
        });
        Ok(Self { task, file })
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
