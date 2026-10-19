//! Raw Input registration queries run on an ordinary observer. The hook watchdog only
//! compares the capture timestamps it already owns. Window removal synchronizes only
//! during shutdown, before unregistering Raw Input and destroying the message window.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use windows::Win32::Foundation::HWND;

#[derive(Debug, Default)]
pub(super) struct State {
    window: Mutex<Option<isize>>,
    stopped: AtomicBool,
    capturing: AtomicBool,
}
impl State {
    pub(super) fn set_capturing(&self, capturing: bool) {
        self.capturing.store(capturing, Ordering::Release);
    }
    pub(super) fn set_window(&self, window: Option<HWND>) {
        *self.window.lock().unwrap_or_else(|e| e.into_inner()) = window.map(|h| h.0 as isize);
    }
}
#[derive(Debug)]
pub(super) struct Observer {
    pub(super) state: Arc<State>,
    join: Option<std::thread::JoinHandle<()>>,
}
impl Observer {
    pub(super) fn start() -> std::io::Result<Self> {
        let state = Arc::new(State::default());
        let read = state.clone();
        let join = std::thread::Builder::new()
            .name("oink-input-observer".into())
            .spawn(move || {
                while !read.stopped.load(Ordering::Acquire) {
                    {
                        let window = read.window.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(window) = *window
                            && read.capturing.load(Ordering::Acquire)
                        {
                            let window = HWND(window as *mut _);
                            if let Err(found) = super::super::rawinput::registration_ok(window) {
                                super::super::trace::action(
                                    crate::native_lifecycle::Kind::Registration,
                                    None,
                                    1,
                                    Some(false),
                                );
                                tracing::warn!(?found, "Raw Input registration changed; restoring input observation");
                                if let Err(error) = super::super::rawinput::register(window) {
                                    tracing::warn!(%error, "cannot restore Raw Input registration");
                                }
                            }
                            if !read.capturing.load(Ordering::Acquire) {
                                super::super::rawinput::unregister();
                            }
                        }
                    }
                    std::thread::park_timeout(std::time::Duration::from_secs(1));
                }
            })?;
        Ok(Self {
            state,
            join: Some(join),
        })
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        self.state.stopped.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            join.thread().unpark();
            let _ = join.join();
        }
    }
}
