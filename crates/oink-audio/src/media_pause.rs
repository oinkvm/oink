//! Pause-only media control at the end of a logical speaker-forwarding session.
//! Terminal capture failures and the agent share a ticket so pause precedes our
//! mute release even when the native worker discovers the failure first.
//! Internal capture rebuilds retire only the physical registration; the audio
//! owner keeps the same logical ticket until forwarding really stops.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

pub const PAUSE_BUDGET: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub struct ForwardingPause {
    state: Arc<AtomicUsize>,
    allowed: Arc<AtomicBool>,
    pause: Arc<dyn Fn() + Send + Sync>,
}
const STOPPING: usize = 1;
const STOPPED: usize = 2;
const ARMED: usize = 4;
const SUBMISSION: usize = 8;

// Only shutdown waits. The audio callback reserves and releases a submission with
// atomics, so it never takes the ordinary control task's restoration lock.
struct Submission<'a> {
    state: &'a AtomicUsize,
    sent: bool,
}
impl Drop for Submission<'_> {
    fn drop(&mut self) {
        if self.sent {
            self.state.fetch_or(ARMED, Ordering::Release);
        }
        self.state.fetch_sub(SUBMISSION, Ordering::Release);
    }
}
impl std::fmt::Debug for ForwardingPause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwardingPause").finish_non_exhaustive()
    }
}
impl Default for ForwardingPause {
    fn default() -> Self {
        Self::with_backend(pause_players)
    }
}
impl ForwardingPause {
    /// A backend must return within PAUSE_BUDGET, including any player wait.
    pub fn with_backend(pause: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            state: Arc::new(AtomicUsize::new(0)),
            allowed: Arc::new(AtomicBool::new(true)),
            pause: Arc::new(pause),
        }
    }
    /// The route owner permits player pause only after a definitive no-exit
    /// resolution. Native failure, source suspension and exit replacement still
    /// restore local mute, but must not pause the user's player.
    pub fn set_pause_allowed(&self, allowed: bool) {
        self.allowed.store(allowed, Ordering::Release);
    }
    /// Called only after a non-provisional audio packet was submitted.
    pub fn arm(&self) {
        let _ = self.state.try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
            (state & STOPPING == 0).then_some(state | ARMED)
        });
    }
    /// Reserve a nonblocking submission before teardown can release native mute.
    /// The RT path never waits for an ordinary thread or enters a shared mutex.
    pub fn submit<T, E>(&self, submit: impl FnOnce() -> Result<T, E>) -> Option<Result<T, E>> {
        self.state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & STOPPING == 0).then_some(state + SUBMISSION)
            })
            .ok()?;
        let mut submission = Submission {
            state: &self.state,
            sent: false,
        };
        let result = submit();
        submission.sent = result.is_ok();
        Some(result)
    }
    pub fn is_ended(&self) -> bool {
        self.state.load(Ordering::Acquire) & STOPPING != 0
    }
    pub fn stop(&self) {
        let previous = self.state.fetch_or(STOPPING, Ordering::AcqRel);
        if previous & STOPPING != 0 {
            while self.state.load(Ordering::Acquire) & STOPPED == 0 {
                std::thread::yield_now();
            }
            return;
        }
        // A stream post is bounded and nonblocking. Finish that existing post,
        // then pause at most once before either teardown caller restores mute.
        while self.state.load(Ordering::Acquire) >= SUBMISSION {
            std::thread::yield_now();
        }
        if self.state.load(Ordering::Acquire) & ARMED != 0 && self.allowed.load(Ordering::Acquire) {
            (self.pause)();
        }
        self.state.fetch_or(STOPPED, Ordering::Release);
    }
}

/// Registration in one physical capture, detached only for internal replacement.
#[derive(Clone, Debug, Default)]
pub(crate) struct CaptureStop(Arc<Mutex<CaptureState>>);
#[derive(Debug, Default)]
struct CaptureState {
    pause: Option<ForwardingPause>,
    end: CaptureEnd,
}
#[derive(Debug, Default, PartialEq, Eq)]
enum CaptureEnd {
    #[default]
    Live,
    Rebuild,
    Stopped,
}
impl CaptureStop {
    pub(crate) fn attach(&self, pause: Option<ForwardingPause>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.end == CaptureEnd::Stopped {
            if let Some(pause) = pause {
                pause.stop();
            }
        } else if state.end == CaptureEnd::Live {
            state.pause = pause;
        }
    }
    /// Retire only this physical capture. The audio owner retains the logical
    /// forwarding ticket across default/format rebuilds on both platforms.
    /// Keep this distinction after retirement: startup's late attach and the
    /// BeforeRestore guard must not stop a ticket that the new capture reuses.
    pub(crate) fn finish_rebuild(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.end == CaptureEnd::Live {
            state.end = CaptureEnd::Rebuild;
            state.pause = None;
        }
    }
    pub(crate) fn finish(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.end == CaptureEnd::Rebuild {
            return;
        }
        state.end = CaptureEnd::Stopped;
        if let Some(pause) = state.pause.take() {
            pause.stop();
        }
    }
}

/// Drop this guard *before* the native tap or Windows mute lease.
pub(crate) struct BeforeRestore(pub(crate) CaptureStop);
impl Drop for BeforeRestore {
    fn drop(&mut self) {
        self.0.finish();
    }
}

fn pause_players() {
    static BUSY: std::sync::OnceLock<Arc<std::sync::atomic::AtomicBool>> = std::sync::OnceLock::new();
    pause_bounded(BUSY.get_or_init(Default::default).clone(), platform::pause);
}

fn pause_bounded(
    busy: Arc<std::sync::atomic::AtomicBool>,
    pause: impl FnOnce(Instant) -> Result<(), String> + Send + 'static,
) {
    use std::sync::atomic::Ordering;
    // A stalled OS call must not accumulate threads on repeated stop/start.
    // Do not queue commands: a stale command could pause a later user session.
    if busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        tracing::warn!("media pause unavailable: previous OS request is still running");
        return;
    }
    let deadline = Instant::now() + PAUSE_BUDGET;
    let worker_busy = busy.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("oink-media-pause".into())
        .spawn(move || {
            struct Release(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for Release {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let _release = Release(worker_busy);
            let result = pause(deadline);
            let _ = tx.send(result);
        });
    if let Err(error) = spawned {
        busy.store(false, Ordering::Release);
        tracing::warn!(%error, "media pause worker unavailable");
        return;
    }
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "media pause failed; restoring local output"),
        Err(_) => tracing::warn!("media pause exceeded 200ms; restoring local output"),
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    unsafe extern "C" {
        fn oink_pause_media() -> bool;
    }
    pub(super) fn pause(deadline: Instant) -> Result<(), String> {
        if Instant::now() >= deadline {
            return Err("pause expired before dispatch".into());
        }
        // SAFETY: the bridge resolves its optional C ABI at runtime; no pointers
        // cross this boundary and only the pause opcode is accepted.
        if unsafe { oink_pause_media() } {
            // SendCommand acknowledges enqueue, not the player changing state.
            // Keep our mute for up to 150ms within the shared budget so the main
            // run loop can process pause before local audio returns.
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(150)),
            );
            Ok(())
        } else {
            Err("MediaRemote pause unavailable/rejected".into())
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as Manager;
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
    pub(super) fn pause(deadline: Instant) -> Result<(), String> {
        // SAFETY: a fresh dedicated thread owns this WinRT apartment.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|e| e.to_string())?;
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                // SAFETY: balances this worker's successful RoInitialize.
                unsafe { RoUninitialize() };
            }
        }
        let _apartment = Apartment;
        let manager = Manager::RequestAsync()
            .and_then(|op| op.join())
            .map_err(|e| e.to_string())?;
        let sessions = manager.GetSessions().map_err(|e| e.to_string())?;
        let mut requests = Vec::new();
        for index in 0..sessions.Size().map_err(|e| e.to_string())? {
            if Instant::now() >= deadline {
                break;
            }
            if let Ok(session) = sessions.GetAt(index) {
                if Instant::now() >= deadline {
                    break;
                }
                // Never TogglePlayPause, even for a paused or stopped session.
                // Dispatch all sessions before waiting for any individual app.
                match session.TryPauseAsync() {
                    Ok(op) => requests.push(op),
                    Err(error) => tracing::debug!(%error, "media session rejected pause"),
                }
            }
        }
        for op in requests {
            if Instant::now() >= deadline {
                break;
            }
            // The outer caller enforces the total bound. Pending WinRT requests
            // may complete later; never submit another request after the deadline.
            if let Err(error) = op.join() {
                tracing::debug!(%error, "media session pause failed");
            }
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;
    pub(super) fn pause(_: Instant) -> Result<(), String> {
        Err("media pause unsupported".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn selected_exit_suppresses_native_failure_pause_until_explicit_clear() {
        let calls = Arc::new(AtomicUsize::new(0));
        for cleared in [false, true] {
            let count = calls.clone();
            let ticket = ForwardingPause::with_backend(move || {
                count.fetch_add(1, Ordering::Relaxed);
            });
            ticket.set_pause_allowed(false);
            ticket.arm();
            let capture = CaptureStop::default();
            capture.attach(Some(ticket.clone()));
            if cleared {
                ticket.set_pause_allowed(true);
            }
            capture.finish();
            ticket.stop();
            assert_eq!(calls.load(Ordering::Relaxed), usize::from(cleared));
        }
    }
    #[test]
    fn shared_capture_and_agent_stop_once_and_idle_never_pauses() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let pause = ForwardingPause::with_backend(move || {
            count.fetch_add(1, Ordering::Relaxed);
        });
        let idle = CaptureStop::default();
        idle.finish();
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let native = CaptureStop::default();
        native.attach(Some(pause.clone()));
        pause.arm();
        native.finish();
        pause.stop();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn internal_replacement_preserves_logical_pause_and_guard_precedes_restore() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let log = events.clone();
        let pause = ForwardingPause::with_backend(move || log.lock().unwrap().push("pause"));
        let old = CaptureStop::default();
        old.attach(Some(pause.clone()));
        pause.arm();
        old.attach(None);
        old.finish();
        assert!(events.lock().unwrap().is_empty());
        let replacement = CaptureStop::default();
        replacement.attach(Some(pause.clone()));
        drop(BeforeRestore(replacement));
        events.lock().unwrap().push("restore");
        pause.stop();
        assert_eq!(*events.lock().unwrap(), ["pause", "restore"]);
    }
}

#[cfg(test)]
mod bound_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    #[test]
    fn stalled_os_request_is_bounded_and_not_queued() {
        let busy = Arc::new(AtomicBool::new(false));
        let (release, wait) = std::sync::mpsc::channel();
        let begin = Instant::now();
        pause_bounded(busy.clone(), move |_| {
            let _ = wait.recv();
            Ok(())
        });
        assert!(begin.elapsed() >= PAUSE_BUDGET);
        assert!(begin.elapsed() < Duration::from_secs(1));
        assert!(busy.load(Ordering::Acquire));
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        pause_bounded(busy, move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        release.send(()).unwrap();
    }
    #[test]
    fn failed_os_request_returns_without_holding_restoration() {
        let busy = Arc::new(AtomicBool::new(false));
        pause_bounded(busy.clone(), |_| Err("unavailable".into()));
        // Completion message can arrive before the worker's final release.
        let end = Instant::now() + Duration::from_secs(1);
        while busy.load(Ordering::Acquire) && Instant::now() < end {
            std::thread::yield_now();
        }
        assert!(!busy.load(Ordering::Acquire));
    }
    #[test]
    fn realtime_submission_has_no_heap_work_and_teardown_waits_for_its_result() {
        let paused = Arc::new(AtomicBool::new(false));
        let observed = paused.clone();
        let ticket = ForwardingPause::with_backend(move || {
            observed.store(true, Ordering::Release);
        });
        crate::allocation_audit::assert_no_heap(|| {
            assert_eq!(ticket.submit(|| Ok::<_, ()>(())), Some(Ok(())));
        });
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, finish) = std::sync::mpsc::channel();
        let source = ticket.clone();
        let writer = std::thread::spawn(move || {
            source.submit(|| {
                entered.send(()).unwrap();
                finish.recv().unwrap();
                Ok::<_, ()>(())
            })
        });
        waiting.recv().unwrap();
        let owner = ticket.clone();
        let stopping = std::thread::spawn(move || owner.stop());
        while !ticket.is_ended() {
            std::thread::yield_now();
        }
        assert!(!paused.load(Ordering::Acquire));
        assert_eq!(ticket.submit(|| Ok::<_, ()>(())), None);
        release.send(()).unwrap();
        assert_eq!(writer.join().unwrap(), Some(Ok(())));
        stopping.join().unwrap();
        assert!(paused.load(Ordering::Acquire));
    }

    #[test]
    fn no_submission_after_native_end_and_failed_sends_do_not_arm() {
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let ticket = ForwardingPause::with_backend(move || {
            calls.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(ticket.submit(|| Err::<(), _>("send failed")), Some(Err("send failed")));
        ticket.stop();
        assert_eq!(ticket.submit(|| Ok::<_, ()>(())), None);
        ticket.arm(); // A late success notification cannot resurrect an ended episode.
        ticket.stop();
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn internal_retirement_preserves_the_ticket_even_if_owner_attaches_after_native_stop() {
        for attach_before in [false, true] {
            let ticket = ForwardingPause::with_backend(|| {});
            let physical = CaptureStop::default();
            if attach_before {
                physical.attach(Some(ticket.clone()));
            }
            physical.finish_rebuild();
            physical.finish(); // BeforeRestore::drop after explicit cleanup.
            if !attach_before {
                physical.attach(Some(ticket.clone()));
            }
            let rebuilt = CaptureStop::default();
            rebuilt.attach(Some(ticket.clone()));
            assert_eq!(ticket.submit(|| Ok::<_, ()>(())), Some(Ok(())));
            ticket.stop();
            assert!(ticket.submit(|| Ok::<_, ()>(())).is_none());
        }
    }
}
