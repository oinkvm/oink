//! Refresh ambiguous visibility only when automatic capture needs it.
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::pointer_capture::CaptureSample;

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Action {
    Read,
    Refresh,
    Withhold,
}

#[derive(Default)]
pub(super) struct RefreshGate {
    motion: Option<(Instant, u32)>,
    foreground: Option<u64>,
    verified: bool,
    last_refresh: Option<Instant>,
}

impl RefreshGate {
    pub fn motion(&mut self, now: Instant, keyboard_serial: u32) {
        self.motion = Some((now, keyboard_serial));
    }

    pub fn key(&mut self) {
        self.motion = None;
    }

    pub fn action(
        &mut self,
        sample: CaptureSample,
        eligible: bool,
        captured: bool,
        now: Instant,
        keyboard_serial: u32,
    ) -> Action {
        // Consume motion even for a visible/precise/ineligible sample: an old
        // move must not reveal the next typing-only hide. Own injected keydowns
        // clear it immediately; the OS counter also excludes later local keys.
        let moved = self.motion.take().is_some_and(|(at, serial)| {
            now.saturating_duration_since(at) <= Duration::from_millis(100) && serial == keyboard_serial
        });
        let same_app = self.foreground == Some(sample.foreground);
        self.foreground = eligible.then_some(sample.foreground);
        if !eligible || !sample.hidden || sample.application_hidden.is_some() {
            self.verified = false;
            return Action::Read;
        }
        if !same_app {
            self.verified = false;
        }
        // A verified hidden episode needs only one refresh before entry. Once
        // captured, recheck while idle too, so stale release visibility cannot
        // hold capture forever. Never refresh visible or precise samples.
        let maintaining = captured && same_app;
        let due = self
            .last_refresh
            .is_none_or(|at| now.saturating_duration_since(at) >= REFRESH_INTERVAL);
        if (maintaining || (!self.verified && moved)) && due {
            // Reserve the interval even on RPC failure. Visible/foreground/key
            // transitions must not reset it: ALL attempts are >=500 ms apart,
            // at most two in any half-open one-second interval, never per move.
            self.last_refresh = Some(now);
            return Action::Refresh;
        }
        if maintaining || self.verified {
            Action::Read
        } else {
            // Do not feed unverified hidden evidence during the cooldown or
            // before fresh remote motion. This may delay unknown-path entry;
            // it also leaves typing at rest hidden without risking false entry.
            Action::Withhold
        }
    }

    pub fn completed(&mut self, hidden_after_refresh: bool) {
        // Only an actually hidden reread confirms the episode. A stale query
        // repaired to visible must not authorize the next typing-only hide.
        // Keys do not revoke an already-confirmed hidden episode: repeated W
        // while entering a real game would otherwise reset detection forever.
        self.verified = hidden_after_refresh;
    }
}

/// Observation state belongs to the ordinary observer. Injection publishes a
/// numeric hint only; a WindowServer round trip must not hold Injector.state.
#[derive(Default)]
pub(super) struct Observer {
    start: std::sync::OnceLock<Instant>,
    revision: AtomicU64,
    boundary_pending: AtomicBool,
    hint_revision: AtomicU64,
    moved: AtomicU64,
    serial: AtomicU32,
    gate: Mutex<(u64, RefreshGate)>,
}
impl Observer {
    pub fn initialize(&self) {
        let _ = self.start.set(Instant::now());
    }
    pub fn motion(&self, now: Instant, serial: u32) {
        self.revision.fetch_add(1, Ordering::AcqRel);
        self.moved.store(
            now.saturating_duration_since(*self.start.get().expect("initialized visibility observer"))
                .as_micros() as u64
                + 1,
            Ordering::Relaxed,
        );
        self.serial.store(serial, Ordering::Relaxed);
        self.hint_revision.fetch_add(1, Ordering::Relaxed);
        self.revision.fetch_add(1, Ordering::Release);
    }
    pub fn key(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
        self.moved.store(0, Ordering::Relaxed);
        self.hint_revision.fetch_add(1, Ordering::Relaxed);
        self.revision.fetch_add(1, Ordering::Release);
    }
    pub fn boundary(&self) {
        self.key();
        // Requests never wait for the observer's potentially in-flight RPC.
        self.boundary_pending.store(true, Ordering::Release);
    }
    pub fn changed(&self) {
        self.revision.fetch_add(2, Ordering::Release);
    }
    pub fn observe(
        &self,
        sample: CaptureSample,
        eligible: bool,
        captured: bool,
        keyboard_serial: u32,
        refresh: impl FnOnce() -> Option<CaptureSample>,
    ) -> Option<CaptureSample> {
        self.observe_at(sample, eligible, captured, keyboard_serial, Instant::now(), refresh)
    }
    fn observe_at(
        &self,
        sample: CaptureSample,
        eligible: bool,
        captured: bool,
        keyboard_serial: u32,
        now: Instant,
        refresh: impl FnOnce() -> Option<CaptureSample>,
    ) -> Option<CaptureSample> {
        let (revision, hint_revision, moved, serial) = loop {
            let before = self.revision.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let moved = self.moved.load(Ordering::Relaxed);
            let serial = self.serial.load(Ordering::Relaxed);
            let hint_revision = self.hint_revision.load(Ordering::Relaxed);
            // Complete hint reads before validating the seqlock on ARM too.
            std::sync::atomic::fence(Ordering::Acquire);
            if before == self.revision.load(Ordering::Acquire) {
                break (before, hint_revision, moved, serial);
            }
        };
        let mut state = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if self.boundary_pending.swap(false, Ordering::AcqRel) {
            // Withheld self-owned samples never visit RefreshGate::action.
            // Revoke the old episode explicitly, preserving the RPC rate limit.
            state.1.motion = None;
            state.1.foreground = None;
            state.1.verified = false;
        }
        // Any injected event invalidates an in-flight visibility query, but only
        // a new motion/key hint may update RefreshGate. Reimporting an old move
        // after a button/scroll would authorize a typing-only hidden episode.
        if state.0 != hint_revision {
            state.0 = hint_revision;
            if moved == 0 {
                state.1.key();
            } else {
                state.1.motion(
                    *self.start.get().expect("initialized visibility observer") + Duration::from_micros(moved - 1),
                    serial,
                );
            }
        }
        match state.1.action(sample, eligible, captured, now, keyboard_serial) {
            Action::Read => Some(sample),
            Action::Withhold => None,
            Action::Refresh => {
                let refreshed = refresh();
                // Without the old injector mutex, input may change during the RPC.
                // Such a reread cannot authorize automatic capture of a stale hide.
                let current = self.revision.load(Ordering::Acquire) == revision;
                state.1.completed(current && refreshed.is_some_and(|s| s.hidden));
                if current { refreshed } else { None }
            }
        }
    }
}

type Connection = unsafe extern "C" fn() -> i32;
type Cursor = unsafe extern "C" fn(i32) -> i32;
type ConnectionPid = unsafe extern "C" fn(i32, *mut i32) -> i32;

struct Functions {
    connection: Connection,
    obscure: Cursor,
    reveal: Cursor,
    connection_pid: ConnectionPid,
}

fn functions() -> Option<&'static Functions> {
    static FUNCTIONS: OnceLock<Option<Functions>> = OnceLock::new();
    FUNCTIONS
        .get_or_init(|| {
            let connection = super::system::find_symbol(c"_CGSDefaultConnection");
            let obscure = super::system::find_symbol(c"SLSObscureCursor");
            let reveal = super::system::find_symbol(c"SLSRevealCursor");
            let connection_pid = super::system::find_symbol(c"SLSConnectionGetPID");
            if [connection, obscure, reveal, connection_pid]
                .iter()
                .any(|p| p.is_null())
            {
                return None;
            }
            // SAFETY: the exported WindowServer client entry points have these
            // C signatures. Check every symbol before any cursor mutation.
            Some(unsafe {
                Functions {
                    connection: std::mem::transmute::<*mut std::ffi::c_void, Connection>(connection),
                    obscure: std::mem::transmute::<*mut std::ffi::c_void, Cursor>(obscure),
                    reveal: std::mem::transmute::<*mut std::ffi::c_void, Cursor>(reveal),
                    connection_pid: std::mem::transmute::<*mut std::ffi::c_void, ConnectionPid>(connection_pid),
                }
            })
        })
        .as_ref()
}

/// Does not hide/unhide an application's hide count, warp, decouple, post an
/// event, change the cursor image, or activate a window. Only the unknown path
/// calls this, outside the tap/router lock, under the injector's ordering mutex.
pub(super) fn resynchronize() -> bool {
    tracing::debug!("ambiguous cursor visibility refresh started");
    let result = synchronize();
    static LAST: Mutex<Option<Option<&'static str>>> = Mutex::new(None);
    let failure = result.err();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if *last != Some(failure) {
        let previous = last.replace(failure);
        if let Some(reason) = failure {
            tracing::warn!(
                reason,
                "ambiguous cursor visibility refresh failed; discarding capture sample"
            );
        } else if previous.is_some_and(|p| p.is_some()) {
            tracing::info!("ambiguous cursor visibility refresh recovered");
        }
    }
    failure.is_none()
}

fn synchronize() -> Result<(), &'static str> {
    let f = functions().ok_or("cursor_refresh_symbols_unavailable")?;
    if !super::system::allow_background_cursor_hiding() {
        return Err("background_cursor_unavailable");
    }
    // On macOS 26.6.2, a real HID move through a modifiable session tap can
    // reveal the cursor while leaving CGCursorIsVisible's shared obscure bit
    // stale. A second move or NO alone does not repair it. YES -> NO refreshes
    // that bit without changing the independent hide count held by a game.
    // NSCursor uses NSApp.contextID (absent in the headless service); a normal
    // background connection is also ignored. Use our OWN default connection
    // with the same background-cursor capability as our Remote cursor hiding.
    // Reveal sends an asynchronous RPC: a read-only, synchronous own-PID query
    // on the same server port orders completion before the visibility reread.
    // An immediate shared-page read was still stale in the native experiment.
    // There is no delay, extra input, foreground connection impersonation, or
    // modification of another app's hide count. RefreshGate requires a fresh
    // remote move after the last key, or an existing capture in the same app;
    // typing at rest outside an active capture is untouched. A hidden episode
    // is verified once before entry; active captures recheck at >=500-ms gaps.
    // An ACTUALLY VISIBLE cursor with a stale hidden read can briefly disappear:
    // 200 refreshes / ~3 s produced 8 blank frames out of 298 at 120 Hz (~8-9 ms
    // each). Actual hide-count capture stayed hidden in all 279 observed frames.
    // This accepted fallback-only tradeoff replaces indefinite false capture;
    // never refresh an already-visible query. It changes no cursor image, focus
    // or position, but is not an optically side-effect-free visibility query.
    // Native background/typing/hide-count and Chromium 149 Pointer Lock were
    // exercised on macOS 14.7.8 / 26.6.2, including stale-query repair on both.
    // SAFETY: resolved C entry points; only our own connection is passed, the
    // PID out-pointer is writable, and reveal is attempted even if obscure errs.
    let (obscure, reveal, fence, pid) = unsafe {
        let connection = (f.connection)();
        if connection == 0 {
            return Err("cursor_connection_unavailable");
        }
        let obscure = (f.obscure)(connection);
        let reveal = (f.reveal)(connection);
        let mut pid = 0;
        let fence = (f.connection_pid)(connection, &mut pid);
        (obscure, reveal, fence, pid)
    };
    if obscure != 0 || reveal != 0 {
        return Err("cursor_refresh_rpc_failed");
    }
    if fence != 0 || pid as u32 != std::process::id() {
        return Err("cursor_refresh_fence_failed");
    }
    Ok(())
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_foundation::Point;

    fn sample(cause: Option<bool>, hidden: bool, foreground: u64) -> CaptureSample {
        CaptureSample {
            windows: None,
            hidden,
            application_hidden: cause,
            foreground,
            confined: false,
            position: Point::new(100., 100.),
            motion_serial: 0,
            motion_observable: true,
        }
    }

    #[test]
    fn focus_boundary_revokes_verified_hidden_episode_without_resetting_refresh_limit() {
        let observer = Observer::default();
        observer.initialize();
        let at = Instant::now();
        let hidden = sample(None, true, 1);
        observer.motion(at, 3);
        assert_eq!(
            observer.observe_at(hidden, true, false, 3, at, || Some(hidden)),
            Some(hidden)
        );
        observer.boundary();
        assert_eq!(
            observer.observe_at(hidden, true, false, 3, at + Duration::from_millis(10), || {
                panic!("no fresh motion in the new context")
            }),
            None
        );
        observer.motion(at + Duration::from_millis(20), 3);
        assert_eq!(
            observer.observe_at(hidden, true, false, 3, at + Duration::from_millis(20), || {
                panic!("boundary must preserve the refresh rate limit")
            }),
            None
        );
        observer.motion(at + Duration::from_millis(500), 3);
        assert_eq!(
            observer.observe_at(hidden, true, false, 3, at + Duration::from_millis(500), || Some(hidden)),
            Some(hidden)
        );
    }

    #[test]
    fn non_motion_input_cannot_reactivate_a_consumed_visibility_hint() {
        let observer = Observer::default();
        observer.initialize();
        let at = Instant::now();
        observer.motion(at, 3);
        let visible = sample(None, false, 1);
        assert_eq!(
            observer.observe_at(visible, true, false, 3, at, || panic!("visible query")),
            Some(visible)
        );
        observer.changed(); // button/scroll invalidates queries, but is not motion
        let hidden = sample(None, true, 1);
        assert_eq!(
            observer.observe_at(hidden, true, false, 3, at + Duration::from_millis(10), || {
                panic!("consumed motion cannot authorize a hidden refresh")
            }),
            None
        );
        observer.motion(at + Duration::from_millis(20), 3);
        assert_eq!(
            observer.observe_at(hidden, true, false, 3, at + Duration::from_millis(20), || Some(hidden)),
            Some(hidden)
        );
    }

    #[test]
    fn precise_visible_and_ineligible_samples_consume_motion_without_refreshing() {
        for (s, eligible) in [
            (sample(Some(true), true, 1), true),
            (sample(Some(false), true, 1), true),
            (sample(None, false, 1), true),
            (sample(None, true, 1), false),
        ] {
            let mut gate = RefreshGate::default();
            let now = Instant::now();
            gate.motion(now, 3);
            assert_eq!(gate.action(s, eligible, true, now, 3), Action::Read);
            assert_eq!(
                gate.action(sample(None, true, 1), true, false, now, 3),
                Action::Withhold
            );
        }
    }

    #[test]
    fn typing_at_rest_and_keys_after_motion_are_not_revealed() {
        let mut gate = RefreshGate::default();
        let now = Instant::now();
        let s = sample(None, true, 1);
        assert_eq!(gate.action(s, true, false, now, 3), Action::Withhold);
        gate.motion(now, 3);
        gate.key();
        assert_eq!(gate.action(s, true, false, now, 3), Action::Withhold);
        gate.motion(now, 3);
        assert_eq!(gate.action(s, true, false, now, 4), Action::Withhold);
        gate.motion(now, 4);
        assert_eq!(gate.action(s, true, false, now, 4), Action::Refresh);
        gate.completed(true);
        assert_eq!(gate.action(s, true, false, now, 4), Action::Read);
        // The refresh reread is visible: the next key-only hide is not verified.
        gate.completed(false);
        gate.key();
        assert_eq!(gate.action(s, true, false, now, 5), Action::Withhold);
    }

    #[test]
    fn one_refresh_verifies_entry_and_capture_refreshes_at_most_twice_a_second() {
        let mut gate = RefreshGate::default();
        let now = Instant::now();
        let s = sample(None, true, 1);
        gate.motion(now, 3);
        assert_eq!(gate.action(s, true, false, now, 3), Action::Refresh);
        gate.completed(true);
        for ms in [50, 250, 499, 500, 1000] {
            let at = now + Duration::from_millis(ms);
            gate.motion(at, 3);
            assert_eq!(gate.action(s, true, false, at, 3), Action::Read);
        }
        let at = now + Duration::from_secs(1);
        assert_eq!(gate.action(s, true, true, at, 4), Action::Refresh);
        gate.completed(true);
        gate.key(); // WASD while captured does not disable periodic recovery.
        for ms in [1, 50, 499] {
            assert_eq!(
                gate.action(s, true, true, at + Duration::from_millis(ms), 5),
                Action::Read
            );
        }
        assert_eq!(gate.action(s, true, true, at + REFRESH_INTERVAL, 5), Action::Refresh);
    }

    #[test]
    fn confirmed_hidden_episode_survives_wasd_during_entry_but_visible_reread_does_not() {
        let mut gate = RefreshGate::default();
        let now = Instant::now();
        let s = sample(None, true, 1);
        gate.motion(now, 3);
        assert_eq!(gate.action(s, true, false, now, 3), Action::Refresh);
        gate.completed(true);
        for serial in 4..24 {
            let at = now + Duration::from_millis(u64::from(serial - 3) * 100);
            gate.key();
            assert_eq!(gate.action(s, true, false, at, serial), Action::Read);
        }
        gate.completed(false);
        gate.key();
        assert_eq!(
            gate.action(s, true, false, now + Duration::from_secs(3), 24),
            Action::Withhold
        );
    }

    #[test]
    fn cooldown_survives_visible_foreground_and_eligibility_changes_and_failures() {
        let mut gate = RefreshGate::default();
        let now = Instant::now();
        gate.motion(now, 3);
        assert_eq!(gate.action(sample(None, true, 1), true, false, now, 3), Action::Refresh);
        gate.completed(false);
        for (ms, s, eligible) in [
            (50, sample(None, false, 1), true),
            (100, sample(None, true, 2), false),
            (200, sample(Some(true), true, 2), true),
        ] {
            assert_eq!(
                gate.action(s, eligible, false, now + Duration::from_millis(ms), 3),
                Action::Read
            );
        }
        let s = sample(None, true, 2);
        for ms in [250, 499] {
            let at = now + Duration::from_millis(ms);
            gate.motion(at, 3);
            assert_eq!(gate.action(s, true, false, at, 3), Action::Withhold);
        }
        let at = now + REFRESH_INTERVAL;
        gate.motion(at, 3);
        assert_eq!(gate.action(s, true, false, at, 3), Action::Refresh);
        gate.completed(true);
        assert_eq!(gate.action(sample(None, true, 3), true, true, at, 3), Action::Withhold);
    }

    #[test]
    fn stale_motion_cannot_start_refresh_but_existing_same_app_capture_can() {
        let mut gate = RefreshGate::default();
        let now = Instant::now();
        let s = sample(None, true, 1);
        gate.motion(now, 3);
        let later = now + Duration::from_secs(10);
        assert_eq!(gate.action(s, true, false, later, 3), Action::Withhold);
        assert_eq!(gate.action(s, true, true, later, 4), Action::Refresh);
    }
}
