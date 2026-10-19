//! Display callbacks wake the observer; the 300 ms debounce and 2 s fallback
//! enumeration are real deadlines. The fallback covers hosts without a main
//! CFRunLoop to receive CoreGraphics callbacks. There is no 100 ms polling slice.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFRetained, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext, kCFRunLoopDefaultMode};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
    CGDisplayRemoveReconfigurationCallback, CGError,
};

use crate::{ChangeHandler, DisplayError};

const DEBOUNCE: Duration = Duration::from_millis(300);
const POLL: Duration = Duration::from_secs(2);

struct Shared {
    dirty: AtomicBool,
    worker: std::sync::OnceLock<Signal>,
    stop: AtomicBool,
    /// Owner turns, for the idle regression.
    iterations: AtomicU64,
    handlers: Mutex<Vec<ChangeHandler>>,
}
struct Signal {
    run_loop: CFRetained<CFRunLoop>,
    source: CFRetained<CFRunLoopSource>,
}
// SAFETY: retained objects are only signalled/woken through CF's cross-thread API.
unsafe impl Send for Signal {}
// SAFETY: the same wake operations are thread-safe; registration stays on the owner.
unsafe impl Sync for Signal {}
impl Signal {
    fn wake(&self) {
        self.source.signal();
        self.run_loop.wake_up();
    }
}
unsafe extern "C-unwind" fn wake_owner(_: *mut c_void) {
    if let Some(run_loop) = CFRunLoop::current() {
        run_loop.stop();
    }
}

pub(crate) struct Watcher {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher").finish_non_exhaustive()
    }
}

unsafe extern "C-unwind" fn on_reconfigure(
    _display: CGDirectDisplayID,
    flags: CGDisplayChangeSummaryFlags,
    user_info: *mut c_void,
) {
    if flags == CGDisplayChangeSummaryFlags::BeginConfigurationFlag {
        return;
    }
    // SAFETY: user_info 是 `Arc<Shared>` 的指针，在回调注销之前保持有效（见 `run`）。
    let shared = unsafe { &*(user_info as *const Shared) };
    shared.dirty.store(true, Ordering::Release);
    if let Some(worker) = shared.worker.get() {
        worker.wake();
    }
}

impl Watcher {
    pub(crate) fn start(first: ChangeHandler) -> Result<Self, DisplayError> {
        let shared = Arc::new(Shared {
            dirty: AtomicBool::new(false),
            worker: std::sync::OnceLock::new(),
            stop: AtomicBool::new(false),
            iterations: AtomicU64::new(0),
            handlers: Mutex::new(vec![first]),
        });
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("oink-display-watch".into())
            .spawn(move || run(s))
            .map_err(|e| DisplayError::Platform(format!("创建线程失败：{e}")))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    pub(crate) fn add_handler(&self, h: ChangeHandler) {
        self.shared.handlers.lock().unwrap_or_else(|e| e.into_inner()).push(h);
    }

    #[cfg(test)]
    fn iterations(&self) -> u64 {
        self.shared.iterations.load(Ordering::Relaxed)
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(worker) = self.shared.worker.get() {
            worker.wake();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(shared: Arc<Shared>) {
    let run_loop = CFRunLoop::current().expect("display owner run loop");
    // SAFETY: no context ownership callbacks; perform stops only its own run loop.
    let mut context: CFRunLoopSourceContext = unsafe { std::mem::zeroed() };
    context.perform = Some(wake_owner);
    // SAFETY: CF copies the valid source context during creation.
    let source = unsafe { CFRunLoopSource::new(None, 0, &mut context) }.expect("display wake source");
    // SAFETY: system-owned run-loop mode constant.
    let mode = unsafe { kCFRunLoopDefaultMode };
    run_loop.add_source(Some(&source), mode);
    let _ = shared.worker.set(Signal { run_loop, source });
    let ptr = Arc::as_ptr(&shared) as *mut c_void;
    // SAFETY: 回调是 C ABI 函数；ptr 在下面注销之前一直有效（`shared` 由本线程持有）。
    let err = unsafe { CGDisplayRegisterReconfigurationCallback(Some(on_reconfigure), ptr) };
    let registered = err == CGError::Success;
    if !registered {
        tracing::warn!(code = err.0, "注册显示器变化回调失败，只靠轮询");
    }

    let mut last = super::enumerate().ok();
    let mut dirty_since: Option<Instant> = None;
    let mut last_poll = Instant::now();
    while !shared.stop.load(Ordering::Acquire) {
        shared.iterations.fetch_add(1, Ordering::Relaxed);
        if shared.dirty.swap(false, Ordering::AcqRel) {
            dirty_since = Some(Instant::now());
        }
        let settled = dirty_since.is_some_and(|t| t.elapsed() >= DEBOUNCE);
        let poll_due = dirty_since.is_none() && last_poll.elapsed() >= POLL;
        if !(settled || poll_due) {
            let due = dirty_since.map_or(last_poll + POLL, |at| at + DEBOUNCE);
            // Keep servicing native callbacks, with an actual source so the
            // run loop blocks instead of returning Finished on an empty loop.
            CFRunLoop::run_in_mode(mode, due.saturating_duration_since(Instant::now()).as_secs_f64(), false);
            continue;
        }
        dirty_since = None;
        last_poll = Instant::now();
        let cur = super::enumerate().ok();
        if cur.is_some() && cur != last {
            tracing::debug!("显示器配置变化");
            last = cur;
            let handlers = shared.handlers.lock().unwrap_or_else(|e| e.into_inner()).clone();
            for h in handlers {
                h();
            }
        }
    }

    if registered {
        // SAFETY: 与注册时的参数一致。
        unsafe { CGDisplayRemoveReconfigurationCallback(Some(on_reconfigure), ptr) };
    }
    let signal = shared.worker.get().expect("display wake registered");
    signal.source.invalidate();
    signal.run_loop.remove_source(Some(&signal.source), mode);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Callback-free idle waits for the two-second fallback, not a polling slice.
    #[test]
    fn idle_watcher_does_not_spin() {
        let w = Watcher::start(Arc::new(|| {})).expect("start watcher");
        std::thread::sleep(Duration::from_millis(300));
        let before = w.iterations();
        std::thread::sleep(Duration::from_millis(1500));
        let n = w.iterations() - before;
        assert!(n <= 2, "idle display observer made {n} turns");
    }
}
