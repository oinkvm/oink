//! One pacing timer and one external wake source on the capture run loop.
use crate::{InputError, capture_loop::Wake};
use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFRetained, CFRunLoop, CFRunLoopActivity, CFRunLoopObserver, CFRunLoopObserverContext,
    CFRunLoopSource, CFRunLoopSourceContext, CFRunLoopTimer, kCFRunLoopCommonModes,
};
use std::{ffi::c_void, sync::Arc, time::Instant};
struct Signal {
    run_loop: CFRetained<CFRunLoop>,
    source: CFRetained<CFRunLoopSource>,
    gate: oink_foundation::wake::WakeGate,
}
// SAFETY: CFRunLoopSourceSignal/CFRunLoopWakeUp are the cross-thread signalling API;
// owned CF references keep both objects alive until the last wake handle is dropped.
unsafe impl Send for Signal {}
// SAFETY: only cross-thread-safe wake operations access the retained object.
unsafe impl Sync for Signal {}
pub(super) struct Timer {
    signal: Arc<Signal>,
    timer: CFRetained<CFRunLoopTimer>,
    observer: CFRetained<CFRunLoopObserver>,
}
unsafe extern "C-unwind" fn perform(_: *mut c_void) {
    if let Some(run_loop) = CFRunLoop::current() {
        run_loop.stop();
    }
}
unsafe extern "C-unwind" fn fire(_: *mut CFRunLoopTimer, _: *mut c_void) {
    if let Some(run_loop) = CFRunLoop::current() {
        run_loop.stop();
    }
}
unsafe extern "C-unwind" fn waiting(_: *mut CFRunLoopObserver, activity: CFRunLoopActivity, info: *mut c_void) {
    // SAFETY: Timer retains Signal until this observer is invalidated and removed.
    let signal = unsafe { &*info.cast::<Signal>() };
    if activity == CFRunLoopActivity::BeforeWaiting {
        if !signal.gate.prepare_wait() {
            signal.run_loop.stop();
        }
    } else {
        signal.gate.awake();
    }
}
impl Timer {
    pub(super) fn new(run_loop: CFRetained<CFRunLoop>) -> Result<Self, InputError> {
        // SAFETY: null info has no retain/release callbacks; perform accesses only this run loop.
        let mut context: CFRunLoopSourceContext = unsafe { std::mem::zeroed() };
        context.perform = Some(perform);
        // SAFETY: context lives through creation; CF copies the context by value.
        let source = unsafe { CFRunLoopSource::new(None, 0, &mut context) }
            .ok_or_else(|| InputError::Platform("input wake source creation failed".into()))?;
        // A repeating source stays valid after firing and can be rearmed at each deadline.
        // The distant repeat date is replaced after every callback; no periodic polling.
        // SAFETY: no context; callback only stops its own run loop.
        let timer = unsafe { CFRunLoopTimer::new(None, 1e20, 1e20, 0, 0, Some(fire), std::ptr::null_mut()) }
            .ok_or_else(|| InputError::Platform("input pacing timer creation failed".into()))?;
        // SAFETY: system-owned constant.
        let mode = unsafe { kCFRunLoopCommonModes };
        run_loop.add_source(Some(&source), mode);
        run_loop.add_timer(Some(&timer), mode);
        let signal = Arc::new(Signal {
            run_loop,
            source,
            gate: Default::default(),
        });
        // SAFETY: zeroed optional callbacks; info borrows the Arc retained by Timer.
        let mut observer_context: CFRunLoopObserverContext = unsafe { std::mem::zeroed() };
        observer_context.info = Arc::as_ptr(&signal).cast_mut().cast();
        // SAFETY: the callback only touches Signal, whose lifetime covers registration.
        let observer = unsafe {
            CFRunLoopObserver::new(
                None,
                (CFRunLoopActivity::BeforeWaiting | CFRunLoopActivity::AfterWaiting).0,
                true,
                0,
                Some(waiting),
                &mut observer_context,
            )
        }
        .ok_or_else(|| InputError::Platform("input wait observer creation failed".into()))?;
        signal.run_loop.add_observer(Some(&observer), mode);
        Ok(Self {
            signal,
            timer,
            observer,
        })
    }
    pub(super) fn wake(&self) -> Wake {
        let signal = self.signal.clone();
        Arc::new(move || {
            let sleeping = signal.gate.notify();
            signal.source.signal();
            if sleeping {
                signal.run_loop.wake_up();
            }
        })
    }
    pub(super) fn begin_turn(&self) {
        self.signal.gate.begin_turn();
    }
    pub(super) fn arm(&self, due: Option<Instant>) {
        let fire = due.map_or(1e20, |at| {
            CFAbsoluteTimeGetCurrent() + at.saturating_duration_since(Instant::now()).as_secs_f64()
        });
        self.timer.set_next_fire_date(fire);
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        self.observer.invalidate();
        self.timer.invalidate();
        self.signal.source.invalidate();
        // SAFETY: system-owned mode constant, objects remain retained during removal.
        let mode = unsafe { kCFRunLoopCommonModes };
        self.signal.run_loop.remove_observer(Some(&self.observer), mode);
        self.signal.run_loop.remove_timer(Some(&self.timer), mode);
        self.signal.run_loop.remove_source(Some(&self.signal.source), mode);
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn pacing_timer_and_network_source_wake_the_same_run_loop() {
        std::thread::spawn(|| {
            let run_loop = CFRunLoop::current().unwrap();
            let timer = Timer::new(run_loop).unwrap();
            // SAFETY: system-owned run-loop mode constant.
            let mode = unsafe { objc2_core_foundation::kCFRunLoopDefaultMode };
            let due = Instant::now() + std::time::Duration::from_millis(10);
            timer.arm(Some(due));
            CFRunLoop::run_in_mode(mode, 1., false);
            assert!(Instant::now() >= due);
            timer.arm(None);
            timer.wake()();
            let before = Instant::now();
            CFRunLoop::run_in_mode(mode, 1., false);
            assert!(before.elapsed() < std::time::Duration::from_millis(500));
            timer.begin_turn();
            let wake = timer.wake();
            let producer = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                wake();
            });
            let before = Instant::now();
            CFRunLoop::run_in_mode(mode, 1., false);
            producer.join().unwrap();
            assert!(before.elapsed() < std::time::Duration::from_millis(500));
        })
        .join()
        .unwrap();
    }
}
