//! Best-effort pause immediately before an owned local-output restore.
//! No route, source, exit, packet or player-state decisions belong here.
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

// 200ms allows macOS's enqueue-only command 150ms to reach the player, plus
// dispatch overhead, without allowing a stuck OS broker to hold capture teardown.
// This is a best-effort grace period, not a guarantee that every player obeys.
const PAUSE_BUDGET: Duration = Duration::from_millis(200);

#[cfg(any(target_os = "macos", windows))]
pub(crate) fn before_restore() {
    static BUSY: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();
    pause_bounded(BUSY.get_or_init(Default::default).clone(), platform::pause);
}

fn pause_bounded(busy: Arc<AtomicBool>, pause: impl FnOnce(Instant) -> Result<(), String> + Send + 'static) {
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
    // No capture or endpoint handle crosses into this worker: restoration can
    // release those resources safely even if the OS call outlives its budget.
    let spawned = std::thread::Builder::new()
        .name("oink-media-pause".into())
        .spawn(move || {
            struct Release(Arc<AtomicBool>);
            impl Drop for Release {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let release = Release(worker_busy);
            let result = pause(deadline);
            drop(release);
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
        let session = manager.GetCurrentSession().map_err(|e| e.to_string())?;
        if Instant::now() >= deadline {
            return Err("pause expired before dispatch".into());
        }
        // GSMTC is asynchronous and an unresponsive player (or broker) can hang
        // even RequestAsync/COM calls. Only this disposable MTA worker may wait;
        // the restore caller has its own 200ms timeout. Never use a media-key
        // toggle: it could start an already-paused player. This targets the OS
        // current session, matching macOS Now Playing; no app enumeration or keys.
        // This narrowed wiring still needs Windows installation-time validation.
        session
            .TryPauseAsync()
            .and_then(|op| op.join())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::{EndpointBackend, EndpointState, Journal, RestorePermit};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct Backend {
        busy: Arc<AtomicBool>,
        wait: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        entered: std::sync::mpsc::Sender<()>,
        state: Mutex<EndpointState>,
        calls: Arc<AtomicUsize>,
    }
    impl EndpointBackend for Backend {
        fn read(&self, _: &str) -> Result<Option<EndpointState>, String> {
            Ok(Some(self.state.lock().unwrap().clone()))
        }
        fn mute(&self, _: &str) -> Result<(), String> {
            self.state.lock().unwrap().muted = true;
            Ok(())
        }
        fn ownership_lost(&self) -> bool {
            false
        }
        fn restore_mute(&self, _: &str, _: bool, _: &RestorePermit) -> Result<(), String> {
            self.state.lock().unwrap().muted = false;
            Ok(())
        }
        fn pause_before_restore(&self) {
            let wait = self.wait.lock().unwrap().take().unwrap();
            let entered = self.entered.clone();
            let calls = self.calls.clone();
            pause_bounded(self.busy.clone(), move |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                entered.send(()).unwrap();
                // A synthetic OS call that never returns until explicitly released.
                wait.recv().unwrap();
                Ok(())
            });
        }
    }

    #[test]
    fn hung_native_pause_bounds_real_restore_and_does_not_stall_other_threads_or_queue_commands() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let (entered, entering) = std::sync::mpsc::channel();
        let busy = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = Backend {
            busy: busy.clone(),
            wait: Mutex::new(Some(wait)),
            entered,
            calls: calls.clone(),
            state: Mutex::new(EndpointState {
                endpoint: "synthetic".into(),
                volume: 0.3,
                muted: false,
            }),
        };
        journal.prepare(backend.read("synthetic").unwrap().unwrap()).unwrap();
        backend.mute("synthetic").unwrap();
        let worker = std::thread::spawn(move || {
            let began = Instant::now();
            journal.recover(&backend, false).unwrap();
            (began.elapsed(), backend.state.into_inner().unwrap())
        });
        entering.recv_timeout(Duration::from_secs(2)).unwrap();
        // Simulate a heartbeat/network owner continuing while OS pause is hung.
        let (heartbeat, beat) = std::sync::mpsc::channel();
        std::thread::spawn(move || heartbeat.send(()).unwrap());
        beat.recv_timeout(Duration::from_millis(100)).unwrap();
        assert!(busy.load(Ordering::Acquire));
        let (elapsed, restored) = worker.join().unwrap();
        assert!(!restored.muted);
        assert!(elapsed >= PAUSE_BUDGET);
        // Scheduler/filesystem tolerance is not part of the 200ms OS wait budget.
        assert!(elapsed < PAUSE_BUDGET + Duration::from_millis(150), "{elapsed:?}");
        let attempted = calls.clone();
        pause_bounded(busy.clone(), move |_| {
            attempted.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while busy.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(!busy.load(Ordering::Acquire));
    }

    #[test]
    fn failed_pause_does_not_delay_restoration() {
        let busy = Arc::new(AtomicBool::new(false));
        let began = Instant::now();
        pause_bounded(busy, |_| Err("unavailable".into()));
        assert!(began.elapsed() < PAUSE_BUDGET);
    }
}
