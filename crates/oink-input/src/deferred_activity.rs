//! A coalescing signal from native input to ordinary activity work. Producers never
//! touch the worker's state/IPC lock, allocate, or wait for a previous declaration.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
struct Pending {
    activity: AtomicBool,
    stopped: AtomicBool,
}
pub(crate) struct DeferredActivity {
    pending: Arc<Pending>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl DeferredActivity {
    pub fn new(mut declare: impl FnMut() + Send + 'static) -> std::io::Result<Self> {
        let pending = Arc::new(Pending::default());
        let state = pending.clone();
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("oink-user-activity".into())
            .spawn(move || {
                // A worker can be constructed by a caller with elevated QoS.
                // powerd/HID idle IPC is ordinary work, never inherited input QoS.
                #[cfg(target_os = "macos")]
                // SAFETY: changes only this worker, with a valid ordinary QoS class.
                let policy = unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_DEFAULT, 0) };
                #[cfg(not(target_os = "macos"))]
                let policy = 0;
                if policy != 0 {
                    let _ = ready.send(Err(std::io::Error::from_raw_os_error(policy)));
                    return;
                }
                let _ = ready.send(Ok(()));
                loop {
                    // An opaque IPC implementation may itself use thread::park
                    // and consume an unpark token. Check durable predicates
                    // before parking, including after every declaration.
                    if state.stopped.load(Ordering::Acquire) {
                        break;
                    }
                    if state.activity.swap(false, Ordering::AcqRel) {
                        declare();
                        continue;
                    }
                    std::thread::park();
                }
            })?;
        if let Err(error) = started.recv().map_err(std::io::Error::other)? {
            let _ = worker.join();
            return Err(error);
        }
        Ok(Self {
            pending,
            worker: Some(worker),
        })
    }
    pub fn request(&self) {
        // unpark retains a token even before park; an event arriving during IPC
        // schedules one subsequent check. The ordinary declaration owns throttling.
        if !self.pending.activity.swap(true, Ordering::AcqRel) {
            self.worker.as_ref().expect("live activity worker").thread().unpark();
        }
    }
}
impl Drop for DeferredActivity {
    fn drop(&mut self) {
        self.pending.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;
    #[test]
    fn ipc_consuming_the_wake_token_cannot_lose_pending_activity() {
        let (entered, ready) = mpsc::channel();
        let (done, completed) = mpsc::channel();
        let mut calls = 0;
        let worker = DeferredActivity::new(move || {
            calls += 1;
            if calls == 1 {
                entered.send(()).unwrap();
                std::thread::park();
            } else {
                done.send(()).unwrap();
            }
        })
        .unwrap();
        worker.request();
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.request(); // explicitly consumed by the fake IPC's park
        let result = completed.recv_timeout(Duration::from_secs(3));
        drop(worker); // releases an old, broken outer-loop park before asserting
        assert!(result.is_ok(), "IPC consumed the activity wake token");
    }
    #[test]
    fn ipc_consuming_the_stop_token_cannot_strand_teardown() {
        let (entered, ready) = mpsc::channel();
        let worker = DeferredActivity::new(move || {
            entered.send(()).unwrap();
            std::thread::park();
        })
        .unwrap();
        worker.request();
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        let thread = worker.worker.as_ref().unwrap().thread().clone();
        let (done, completed) = mpsc::channel();
        let teardown = std::thread::spawn(move || {
            drop(worker);
            done.send(()).unwrap();
        });
        let result = completed.recv_timeout(Duration::from_secs(3));
        if result.is_err() {
            thread.unpark();
        }
        teardown.join().unwrap();
        assert!(result.is_ok(), "IPC consumed the stop wake token");
    }
}
