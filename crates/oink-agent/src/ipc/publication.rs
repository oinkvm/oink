//! Endpoint publication belongs to the listener's lifetime. Retry only this
//! idempotent document, retaining the same bound port and authentication token.
use std::{
    io,
    sync::{Mutex, mpsc},
    thread::JoinHandle,
    time::Duration,
};

#[derive(Debug)]
pub(super) struct Publication {
    cancel: Option<mpsc::Sender<()>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl Publication {
    pub(super) fn start(mut publish: impl FnMut() -> io::Result<()> + Send + 'static) -> io::Result<Self> {
        match publish() {
            Ok(()) => Ok(Self {
                cancel: None,
                worker: Mutex::new(None),
            }),
            Err(error) => {
                tracing::warn!(error_kind=?error.kind(), os_error=error.raw_os_error(),
                    "IPC endpoint publication failed; keeping listener alive and retrying");
                let (cancel, cancelled) = mpsc::channel();
                let worker = std::thread::Builder::new()
                    .name("ipc-publication".into())
                    .spawn(move || {
                        let mut delay = Duration::from_millis(100);
                        loop {
                            // Interrupt the capped backoff on shutdown. A native write
                            // already in progress must finish before endpoint removal.
                            if cancelled.recv_timeout(delay) != Err(mpsc::RecvTimeoutError::Timeout) {
                                return;
                            }
                            if publish().is_ok() {
                                tracing::info!("IPC endpoint publication recovered");
                                return;
                            }
                            delay = (delay * 2).min(Duration::from_secs(2));
                        }
                    })?;
                Ok(Self {
                    cancel: Some(cancel),
                    worker: Mutex::new(Some(worker)),
                })
            }
        }
    }
    fn cancel(&self) {
        if let Some(cancel) = &self.cancel {
            let _ = cancel.send(());
        }
    }
    fn take_worker(&self) -> Option<JoinHandle<()>> {
        self.worker.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub(super) fn stop(&self) {
        self.cancel();
        if let Some(worker) = self.take_worker() {
            let _ = worker.join();
        }
    }
    pub(super) async fn stop_async(&self) {
        self.cancel();
        if let Some(worker) = self.take_worker() {
            // Do not block the async host's shutdown deadline on native file I/O.
            let _ = tokio::task::spawn_blocking(move || worker.join()).await;
        }
    }
}
impl Drop for Publication {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };

    #[test]
    fn shutdown_drains_in_flight_write_before_endpoint_removal() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ipc.json");
        let output = path.clone();
        let (entered, entering) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut first = true;
        let owner = Publication::start(move || {
            if std::mem::take(&mut first) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            entered.send(()).unwrap();
            released.recv().unwrap();
            std::fs::write(&output, b"endpoint")
        })
        .unwrap();
        entering.recv_timeout(Duration::from_secs(10)).unwrap();
        // The writer remains blocked until shutdown has been requested.
        owner.cancel();
        release.send(()).unwrap();
        owner.stop();
        std::fs::remove_file(&path).unwrap();
        drop(owner);
        assert!(!path.exists());
    }

    #[test]
    fn publication_recovers_after_transient_failure() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = attempts.clone();
        let (published, success) = mpsc::channel();
        let owner = Publication::start(move || {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            published.send(()).unwrap();
            Ok(())
        })
        .expect("a failed publish must leave its listener alive and retry");
        success.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(owner);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
