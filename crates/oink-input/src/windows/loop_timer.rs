//! High-resolution pacing and queue arrival share the hook thread's message wait.
use crate::{InputError, capture_loop::Wake};
use std::{sync::Arc, time::Instant};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_FAILED},
    System::Threading::*,
    UI::WindowsAndMessaging::*,
};
struct Event(HANDLE, oink_foundation::wake::WakeGate);
// SAFETY: a kernel event may be signalled from any thread; Arc owns its lifetime.
unsafe impl Send for Event {}
// SAFETY: only cross-thread-safe wake operations access the retained object.
unsafe impl Sync for Event {}
impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the handle and closes it exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
pub(super) struct Timer {
    event: Arc<Event>,
    timer: HANDLE,
}
impl Timer {
    pub(super) fn new() -> Result<Self, InputError> {
        let fail = |e: windows::core::Error| InputError::Platform(e.to_string());
        // SAFETY: unnamed non-inherited auto-reset event and high-resolution timer.
        let event = Arc::new(Event(
            unsafe { CreateEventW(None, false, false, None) }.map_err(fail)?,
            Default::default(),
        ));
        // SAFETY: unnamed, non-inherited high-resolution timer, owned by this loop.
        let timer =
            unsafe { CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0) }
                .map_err(fail)?;
        Ok(Self { event, timer })
    }
    pub(super) fn wake(&self) -> Wake {
        let event = self.event.clone();
        Arc::new(move || {
            if event.1.notify() {
                // SAFETY: owned event, alive until the last wake closure is dropped.
                unsafe {
                    let _ = SetEvent(event.0);
                }
            }
        })
    }
    pub(super) fn arm(&self, due: Option<Instant>) -> Result<(), InputError> {
        // SAFETY: owned timer, relative due time in 100 ns units; no callback.
        unsafe {
            if let Some(due) = due {
                let ticks = (due.saturating_duration_since(Instant::now()).as_nanos() / 100).max(1) as i64;
                SetWaitableTimer(self.timer, &-ticks, 0, None, None, false)
            } else {
                CancelWaitableTimer(self.timer)
            }
        }
        .map_err(|e| InputError::Platform(e.to_string()))
    }
    pub(super) fn begin_turn(&self) {
        self.event.1.begin_turn();
    }
    pub(super) fn wait(&self) -> Result<(), InputError> {
        if !self.event.1.prepare_wait() {
            return Ok(());
        }
        // MWMO_INPUTAVAILABLE also notices messages already observed by a previous peek.
        // No sleep, ordinary task, or process-wide timer-resolution setting intervenes.
        // SAFETY: both handles remain owned and valid through the wait.
        let result = unsafe {
            MsgWaitForMultipleObjectsEx(
                Some(&[self.event.0, self.timer]),
                INFINITE,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        self.event.1.awake();
        if result == WAIT_FAILED {
            Err(InputError::Platform(windows::core::Error::from_thread().to_string()))
        } else {
            Ok(())
        }
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the handle and closes it exactly once.
        unsafe {
            let _ = CloseHandle(self.timer);
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn high_resolution_deadline_and_external_wake_share_native_wait() {
        let timer = Timer::new().unwrap();
        let due = Instant::now() + std::time::Duration::from_millis(10);
        timer.arm(Some(due)).unwrap();
        timer.wait().unwrap();
        assert!(Instant::now() >= due);
        timer.arm(None).unwrap();
        timer.wake()();
        timer.wait().unwrap();
    }
}
