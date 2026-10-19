//! Cancels a registration attempt, never the lifetime of its activated delivery.
use super::*;
use std::sync::Condvar;

#[derive(Debug, Default)]
struct State {
    cancelled: AtomicBool,
    started: AtomicBool,
    running: Mutex<bool>,
    finished: Condvar,
}

/// One handle per ordinary Announced attempt. Clone for the worker/Enter owner.
#[derive(Clone, Debug, Default)]
pub struct PreparationCancellation(Arc<State>);
impl PreparationCancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    /// Blocking worker API. A not-yet-started worker cannot subsequently start.
    /// Does not wait for the caller to put the result into its lifecycle Slot.
    pub fn cancel_and_wait(&self) {
        self.cancel();
        let mut running = self.0.running.lock().unwrap();
        while *running {
            running = self.0.finished.wait(running).unwrap();
        }
    }
    pub(super) fn enter(&self) -> Result<Running<'_>, ClipboardError> {
        let mut running = self.0.running.lock().unwrap();
        if self.is_cancelled() {
            return Err(ClipboardError::FileProvider(FileProviderReason::Cancelled));
        }
        if self.0.started.swap(true, Ordering::AcqRel) {
            return Err(error("preparation cancellation handle already in use"));
        }
        *running = true;
        Ok(Running(self))
    }
}
pub(super) struct Running<'a>(&'a PreparationCancellation);
impl Drop for Running<'_> {
    fn drop(&mut self) {
        *self.0.0.running.lock().unwrap() = false;
        self.0.0.finished.notify_all();
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn cancelled_before_worker_start_cannot_later_spawn_a_helper() {
        let cancel = PreparationCancellation::new();
        cancel.cancel_and_wait();
        assert!(cancel.enter().is_err());
    }
    #[test]
    fn independent_cancel_waits_until_ordinary_helper_is_reaped() {
        let cancel = PreparationCancellation::new();
        let worker_cancel = cancel.clone();
        let (started, ready) = std::sync::mpsc::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = finished.clone();
        let worker = std::thread::spawn(move || {
            let _running = worker_cancel.enter().unwrap();
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "exec sleep 30"]);
            let polls = AtomicU64::new(0);
            let result = run_manager(command, Instant::now() + MANAGER_TIMEOUT, &|| {
                // First check precedes spawn; second check owns a live child.
                if polls.fetch_add(1, Ordering::Relaxed) == 1 {
                    started.send(()).unwrap();
                }
                !worker_cancel.is_cancelled()
            });
            assert!(matches!(
                result,
                Err(ClipboardError::FileProvider(FileProviderReason::Cancelled))
            ));
            worker_finished.store(true, Ordering::Release);
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let before = Instant::now();
        cancel.cancel_and_wait();
        assert!(finished.load(Ordering::Acquire));
        assert!(before.elapsed() < Duration::from_secs(2));
        worker.join().unwrap();
    }
}
