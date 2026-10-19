//! No provider access may precede fresh-data cleanup / relaunch recovery.
use std::sync::{Condvar, Mutex};

pub(super) struct LaunchPreparation {
    // None = pending, Some(true) = ready, Some(false) = failed until next launch.
    result: Mutex<Option<bool>>,
    changed: Condvar,
}

impl LaunchPreparation {
    pub(super) const fn new() -> Self {
        Self {
            result: Mutex::new(Some(true)),
            changed: Condvar::new(),
        }
    }
    pub(super) fn begin(&self) {
        *self.result.lock().unwrap() = None;
    }
    pub(super) fn finish(&self, success: bool) {
        *self.result.lock().unwrap() = Some(success);
        self.changed.notify_all();
    }
    pub(super) fn result(&self) -> Option<bool> {
        *self.result.lock().unwrap()
    }
    pub(super) fn wait(&self) -> bool {
        let mut result = self.result.lock().unwrap();
        while result.is_none() {
            result = self.changed.wait(result).unwrap();
        }
        *result == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_preparation_blocks_warmup_and_failure_never_adopts_a_domain() {
        for success in [false, true] {
            let gate = std::sync::Arc::new(LaunchPreparation::new());
            gate.begin();
            assert_eq!(gate.result(), None);
            let (send, receive) = std::sync::mpsc::channel();
            let worker_gate = gate.clone();
            let worker = std::thread::spawn(move || send.send(worker_gate.wait()).unwrap());
            assert!(receive.recv_timeout(std::time::Duration::from_millis(20)).is_err());
            gate.finish(success);
            assert_eq!(
                receive.recv_timeout(std::time::Duration::from_secs(1)).unwrap(),
                success
            );
            worker.join().unwrap();
            assert_eq!(gate.result(), Some(success));
        }
    }
}
