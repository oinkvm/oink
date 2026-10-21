//! Serializes WebView creation without keeping a lock across a native message pump.
use std::{collections::VecDeque, sync::Mutex};

#[derive(Debug)]
struct Pending<T> {
    requests: VecDeque<T>,
    running: bool,
}

#[derive(Debug)]
pub(crate) struct CreationQueue<T>(Mutex<Pending<T>>);

impl<T> Default for CreationQueue<T> {
    fn default() -> Self {
        Self(Mutex::new(Pending {
            requests: VecDeque::new(),
            running: false,
        }))
    }
}

impl<T> CreationQueue<T> {
    /// True only for the caller responsible for starting a worker. A reentrant
    /// UI callback queues its request and returns without waiting for that worker.
    pub(crate) fn submit(&self, request: T) -> bool {
        let mut pending = self.0.lock().unwrap();
        pending.requests.push_back(request);
        if pending.running {
            false
        } else {
            pending.running = true;
            true
        }
    }

    pub(crate) fn drain(&self, mut execute: impl FnMut(T)) {
        loop {
            let request = {
                let mut pending = self.0.lock().unwrap();
                match pending.requests.pop_front() {
                    Some(request) => request,
                    None => {
                        pending.running = false;
                        return;
                    }
                }
            };
            // WebView2 pumps messages here. Neither the queue mutex nor any
            // other creation gate may be held while the constructor executes.
            execute(request);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell_policy::View;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    struct Pump {
        queue: CreationQueue<View>,
        web_context: Mutex<()>,
        completed: Mutex<Vec<View>>,
        outer: View,
    }

    impl Pump {
        // This is the production submit/drain protocol. Executing the elected
        // worker inline makes same-thread pump reentry deterministic in this test.
        fn request(&self, view: View) {
            if self.queue.submit(view) {
                self.queue.drain(|view| self.create(view));
            }
        }

        fn create(&self, view: View) {
            // A real shared mutex models Wry's WebContext lock. try_lock makes
            // the former nested constructor fail instead of hanging the suite.
            let _context = self.web_context.try_lock().expect("reentrant WebContext creation");
            if view == self.outer {
                // Model the native constructor's message pump delivering a tray
                // callback and further IPC/menu requests on that same UI thread.
                for nested in [View::Main, View::Panel, View::About, View::Quit] {
                    if nested != self.outer {
                        self.request(nested);
                    }
                }
                assert!(self.completed.lock().unwrap().is_empty());
            }
            self.completed.lock().unwrap().push(view);
        }
    }

    #[test]
    fn native_pump_reentry_queues_panel_and_every_normal_view_without_relocking_context() {
        for outer in [View::Main, View::Panel, View::About, View::Quit] {
            let (send, receive) = mpsc::channel();
            std::thread::spawn(move || {
                let pump = Pump {
                    queue: CreationQueue::default(),
                    web_context: Mutex::new(()),
                    completed: Mutex::new(Vec::new()),
                    outer,
                };
                pump.request(outer);
                send.send(pump.completed.into_inner().unwrap()).unwrap();
            });
            let mut expected = vec![outer];
            expected.extend(
                [View::Main, View::Panel, View::About, View::Quit]
                    .into_iter()
                    .filter(|v| *v != outer),
            );
            assert_eq!(
                receive
                    .recv_timeout(Duration::from_secs(2))
                    .expect("UI callback waited on the creation gate"),
                expected
            );
        }
    }

    #[test]
    fn concurrent_callback_returns_while_creation_is_in_progress_and_idle_worker_can_restart() {
        let queue = Arc::new(CreationQueue::default());
        assert!(queue.submit(View::About));
        let (started_send, started_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let worker_queue = queue.clone();
        let worker = std::thread::spawn(move || {
            let mut done = Vec::new();
            worker_queue.drain(|view| {
                if view == View::About {
                    started_send.send(()).unwrap();
                    release_receive.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                done.push(view);
            });
            done
        });
        started_receive.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!queue.submit(View::Panel));
        release_send.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), [View::About, View::Panel]);
        assert!(queue.submit(View::Quit));
        let mut next = Vec::new();
        queue.drain(|view| next.push(view));
        assert_eq!(next, [View::Quit]);
    }
}
