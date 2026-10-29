//! Bounded recovery of Oink's own unresolved Local+covered restore.
use super::*;

pub(super) const TIMER: usize = 4;
pub(super) const MESSAGE: u32 = WM_APP + 7;
const DEADLINE: Duration = Duration::from_millis(250);
const UNCOVER_SETTLE: Duration = Duration::from_millis(50);

pub(super) struct LocalRestore {
    generation: u64,
    target: POINT,
    since: Instant,
    attempted: bool,
    verified: bool,
    settle_until: Option<Instant>,
    report: Option<crate::native_lifecycle::Data>,
}

fn showing() -> Option<bool> {
    #[cfg(all(test, debug_assertions))]
    if let Some(hidden) = super::super::cursor::test_native::hidden() {
        return Some(!hidden);
    }
    let mut info = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: initialized output storage; failures stay unknown.
    unsafe {
        GetCursorInfo(&mut info)
            .ok()
            .map(|_| info.flags.0 & CURSOR_SHOWING.0 != 0)
    }
}

impl State {
    pub(super) fn request_restore(&mut self, at: Point, complete: Option<RestoreCompletion>) -> Option<Marker> {
        let marker = self.request(Transition::restore(at));
        self.restore_complete = complete.map(|complete| (self.transitions, complete));
        if self.pending.is_none() && self.local_restore.is_none() {
            if let Some((_, complete)) = self.restore_complete.take() {
                complete(false);
            }
        } else {
            self.check_restore_completion();
        }
        marker
    }

    pub(super) fn cancel_restore(&mut self) {
        self.finish_reset_report(false);
        self.local_restore = None;
        self.stop_restore_timer();
        if let Some((_, complete)) = self.restore_complete.take() {
            complete(false);
        }
    }

    fn stop_restore_timer(&self) {
        if !self.hwnd.is_invalid() {
            // SAFETY: this hook owner owns the window/timer.
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER);
            }
        }
    }

    fn arm_restore_timer(&self, delay: Duration) {
        if !self.hwnd.is_invalid() {
            let milliseconds = delay
                .as_millis()
                .saturating_add(u128::from(!delay.subsec_nanos().is_multiple_of(1_000_000)));
            // SAFETY: this hook owner owns the existing window/timer. SetTimer
            // quantizes deadlines; an early delivery must rearm only the remaining
            // interval, never wait a second full 250 ms periodic tick.
            unsafe {
                SetTimer(
                    Some(self.hwnd),
                    TIMER,
                    milliseconds.clamp(1, u32::MAX as u128) as u32,
                    None,
                );
            }
        }
    }

    pub(super) fn begin_restore(&mut self, at: Option<POINT>) {
        self.local_restore = at.map(|target| LocalRestore {
            generation: self.transitions,
            target,
            since: Instant::now(),
            attempted: false,
            verified: false,
            settle_until: None,
            report: None,
        });
        if self.local_restore.is_some() {
            self.arm_restore_timer(DEADLINE);
        }
    }

    pub(super) fn check_restore_completion(&mut self) {
        if self.pending.is_some() || !matches!(self.mode, CaptureMode::Local) {
            return;
        }
        let Some(restore) = self.local_restore.as_ref() else {
            return;
        };
        if restore.generation != self.transitions {
            return;
        }
        let position = cursor_pos_checked().ok();
        // A visible park is accepted ONLY after the explicitly authorized safe reset.
        let placed = position.is_some_and(|p| restore.attempted || p != self.park || p == restore.target);
        if !self.hider.owns_cover() && placed && showing() == Some(true) {
            self.local_restore.as_mut().unwrap().verified = true;
            self.stop_restore_timer();
            self.finish_reset_report(true);
            if let Some((generation, complete)) = self.restore_complete.take() {
                complete(generation == self.transitions);
            }
        }
    }

    fn finish_reset_report(&mut self, success: bool) {
        if let Some(mut data) = self.local_restore.as_mut().and_then(|restore| restore.report.take()) {
            data.cursor = cursor_pos_checked().ok().map(|p| [p.x, p.y]);
            data.cursor_showing = showing();
            data.hidden = data.cursor_showing.map(|showing| !showing);
            data.success = Some(success);
            if !self.hider.owns_cover() {
                data.steps = data.steps.map(|steps| steps | 16);
            }
            if data.cursor_showing == Some(true) {
                data.steps = data.steps.map(|steps| steps | 32);
            }
            crate::native_lifecycle::emit_lifecycle(crate::native_lifecycle::Kind::RestoreSafetyReset, data);
            tracing::warn!(
                generation = data.extra,
                trigger = data.reason,
                steps = data.steps,
                success,
                "Windows cursor restore safety reset; field cause remains unresolved"
            );
        }
    }

    fn reset_eligible(&self, generation: u64) -> bool {
        self.transitions == generation
            && self.pending.is_none()
            && matches!(self.mode, CaptureMode::Local)
            && self.capture_enabled
            && self.continuity.available()
            && !self.relative_requested
            && self.drag_anchor.is_none()
            && crate::capture_loop::pointer_holder()
            && self.hider.owns_cover()
            && (showing() == Some(true) || self.hider.cover_owns_cursor())
            && self
                .local_restore
                .as_ref()
                .is_some_and(|r| r.generation == generation && !r.attempted && !r.verified)
    }

    pub(super) fn reset_restore(&mut self, generation: u64, trigger: u32) {
        if !self.reset_eligible(generation) {
            return;
        }
        // User-authorized exception for the unresolved exact-edge field case:
        // marker acceptance / SendInput completion can leave our cover up at park.
        // Run once, outside the LL hook, with synchronous SetCursorPos rather than
        // re-entering the same marker path. Prefer a verified target, but even a
        // visible park is safer than an invisible cursor. Never touch another
        // window's NULL cursor or ShowCursor count. See docs/design/windows-cursor-restore.md.
        // Recovery was verified with rejected markers, park confinement and suppressed
        // completion notifications on Windows 11 26200; field origin 22631 is unresolved.
        if IN_HOOK.with(Cell::get) {
            return;
        }
        let restore = self.local_restore.as_mut().unwrap();
        restore.attempted = true;
        restore.settle_until = Some(Instant::now() + UNCOVER_SETTLE);
        let clamped = clamp_to_monitor_checked(restore.target);
        let target = clamped.unwrap_or(restore.target);
        let before = cursor_pos_checked().ok();
        let hidden_before = showing().map(|showing| !showing);
        let result = clamped.map_or_else(
            || {
                Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_FAIL,
                    "restore monitor unavailable",
                ))
            },
            super::super::warp::restore_synchronously,
        );
        let after_move = cursor_pos_checked().ok();
        let mut steps = 8 | u32::from(clamped.is_some());
        if result.is_ok() {
            steps |= 2;
        }
        if after_move.is_some_and(|at| at != self.park) {
            steps |= 4;
        }
        restore.report = Some(crate::native_lifecycle::Data {
            extra: generation,
            reason: trigger,
            point: Some([target.x, target.y]),
            park: Some([self.park.x, self.park.y]),
            cursor_before: before.map(|p| [p.x, p.y]),
            cursor_after_move: after_move.map(|p| [p.x, p.y]),
            hidden_before,
            steps: Some(steps),
            flags: result.err().map_or(0, |error| error.code().0 as u32),
            ..Default::default()
        });
        self.restoring_from_marker = None;
        self.hider.show_safely();
        // One bounded ACK/visibility check, never a second placement.
        self.arm_restore_timer(UNCOVER_SETTLE);
        self.check_restore_completion();
    }

    pub(super) fn restore_deadline(&mut self) {
        self.restore_deadline_at(Instant::now());
    }

    // Tests provide exact instants instead of depending on clock resolution or
    // parallel-runner scheduling; production samples the clock once per delivery.
    fn restore_deadline_at(&mut self, now: Instant) {
        let Some(restore) = &self.local_restore else {
            self.stop_restore_timer();
            return;
        };
        if restore.generation != self.transitions {
            return;
        }
        if restore.attempted {
            // Exact equality is due: checked_duration_since returns Some(ZERO),
            // which must not rearm the timer or postpone completion by a tick.
            let remaining = restore
                .settle_until
                .and_then(|until| until.checked_duration_since(now))
                .filter(|remaining| !remaining.is_zero());
            let expired = remaining.is_none();
            self.check_restore_completion();
            if !expired && !self.local_restore.as_ref().is_some_and(|r| r.verified) {
                self.arm_restore_timer(remaining.unwrap());
            }
            if expired && !self.local_restore.as_ref().is_some_and(|r| r.verified) {
                self.stop_restore_timer();
                self.finish_reset_report(false);
                if let Some((_, complete)) = self.restore_complete.take() {
                    complete(false);
                }
            }
            return;
        }
        // A queued timer from an earlier generation cannot shorten a newer deadline.
        if let Some(remaining) = DEADLINE
            .checked_sub(now.saturating_duration_since(restore.since))
            .filter(|remaining| !remaining.is_zero())
        {
            self.arm_restore_timer(remaining);
            return;
        }
        let generation = restore.generation;
        self.stop_restore_timer();
        self.check_restore_completion();
        if self.local_restore.as_ref().is_some_and(|r| r.verified) {
            return;
        }
        let eligible = self.reset_eligible(generation);
        self.reset_restore(generation, 2);
        if !eligible && let Some((_, complete)) = self.restore_complete.take() {
            complete(false);
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::windows::cursor::test_native as native;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn stuck() -> (native::Guard, State, u64) {
        let guard = native::Guard::at(POINT { x: 1920, y: 1080 });
        let mut state = super::super::tests::remote_state();
        state.hider = native::hider();
        state.execute(Transition::Remote(state.park), Via::Marker);
        native::set_showing(false);
        let marker = state
            .request(Transition::Local(Some(POINT { x: 3839, y: 900 })))
            .unwrap();
        assert!(matches!(
            state.prepare_marker(marker.target, marker.extra),
            Prepared::Pass
        ));
        (guard, state, marker.sequence)
    }

    #[test]
    fn deadline_resets_once_and_completion_waits_for_verified_uncover() {
        let (_guard, mut state, generation) = stuck();
        let completed = Arc::new(AtomicUsize::new(0));
        let result = completed.clone();
        state.restore_complete = Some((
            generation,
            Box::new(move |success| {
                assert!(success);
                result.fetch_add(1, Ordering::Relaxed);
            }),
        ));
        let started = state.local_restore.as_ref().unwrap().since;
        state.restore_deadline_at(started);
        state.restore_deadline_at(started + DEADLINE - Duration::from_nanos(1));
        assert!(state.hider.is_hidden(), "a deadline cannot fire early");
        state.restore_deadline_at(started + DEADLINE);
        assert_eq!(cursor_pos(), POINT { x: 3839, y: 900 });
        assert_eq!(
            completed.load(Ordering::Relaxed),
            0,
            "hide submission is not completion"
        );
        state.hider.finish_show(2);
        state.check_restore_completion();
        assert_eq!(native::hidden(), Some(false));
        assert_eq!(completed.load(Ordering::Relaxed), 1);
        let count = native::moves().len();
        state.reset_restore(generation, 1);
        state.restore_deadline();
        assert_eq!(native::moves().len(), count);
        assert_eq!(completed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn missing_cover_ack_finishes_false_without_retrying_the_reset() {
        let (_guard, mut state, generation) = stuck();
        let completed = Arc::new(AtomicUsize::new(0));
        let result = completed.clone();
        state.restore_complete = Some((
            generation,
            Box::new(move |success| {
                assert!(!success);
                result.fetch_add(1, Ordering::Relaxed);
            }),
        ));
        state.uncover(generation);
        let count = native::moves().len();
        assert_eq!(completed.load(Ordering::Relaxed), 0);
        let deadline = state.local_restore.as_ref().unwrap().settle_until.unwrap();
        state.restore_deadline_at(deadline - Duration::from_nanos(1));
        assert_eq!(completed.load(Ordering::Relaxed), 0);
        state.restore_deadline_at(deadline);
        assert_eq!(completed.load(Ordering::Relaxed), 1);
        state.reset_restore(generation, 1);
        state.restore_deadline();
        assert_eq!(native::moves().len(), count);
        assert_eq!(completed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn superseding_restore_cancels_completion_and_old_timer_cannot_reset_new_generation() {
        let (_guard, mut state, old) = stuck();
        let completed = Arc::new(AtomicUsize::new(0));
        let result = completed.clone();
        state.restore_complete = Some((
            old,
            Box::new(move |success| {
                assert!(!success);
                result.fetch_add(1, Ordering::Relaxed);
            }),
        ));
        let new = state
            .request(Transition::Local(Some(POINT { x: 3839, y: 901 })))
            .unwrap();
        state.prepare_marker(new.target, new.extra);
        assert_eq!(completed.load(Ordering::Relaxed), 1);
        let count = native::moves().len();
        state.reset_restore(old, 1);
        let started = state.local_restore.as_ref().unwrap().since;
        state.restore_deadline_at(started);
        assert_eq!(native::moves().len(), count);
        assert!(state.hider.is_hidden());
        assert!(!state.local_restore.as_ref().unwrap().attempted);
    }

    #[test]
    fn failed_synchronous_position_still_uncovers_our_park() {
        let (_guard, mut state, generation) = stuck();
        native::ignore_warps();
        state.uncover(generation);
        state.hider.finish_show(2);
        state.check_restore_completion();
        assert_eq!(cursor_pos(), state.park);
        assert_eq!(native::hidden(), Some(false));
        assert!(state.local_restore.as_ref().unwrap().verified);
    }

    #[test]
    fn stale_generation_and_remote_mode_never_reset() {
        let (_guard, mut state, generation) = stuck();
        let count = native::moves().len();
        state.reset_restore(generation - 1, 1);
        assert!(state.hider.is_hidden());
        state.mode = CaptureMode::Remote {
            park: to_point(state.park),
        };
        state.reset_restore(generation, 1);
        assert_eq!(native::moves().len(), count);
        assert_eq!(native::hidden(), Some(true));
    }

    #[test]
    fn third_party_hide_and_game_mode_are_not_repaired() {
        let (_guard, mut state, generation) = stuck();
        state.relative_requested = true;
        state.reset_restore(generation, 1);
        assert!(state.hider.is_hidden());
        state.relative_requested = false;
        native::foreign_cursor();
        let count = native::moves().len();
        state.reset_restore(generation, 1);
        assert_eq!(
            native::moves().len(),
            count,
            "an existing cover does not own another window's hidden cursor"
        );
        assert!(state.hider.is_hidden());
        state.hider.show_at(None);
        state.hider.finish_show(2);
        native::set_showing(false); // A different owner hides after our cover is gone.
        let count = native::moves().len();
        state.reset_restore(generation, 2);
        assert_eq!(native::moves().len(), count);
        assert_eq!(native::hidden(), Some(true));
    }

    #[test]
    fn current_router_not_holder_cannot_reset_a_local_native_mode() {
        struct Owner;
        impl crate::capture_loop::CaptureLoop for Owner {
            fn set_wake(&mut self, _: crate::capture_loop::Wake) {}
            fn event(&mut self, _: &CapturedEvent) -> Disposition {
                Disposition::Pass
            }
            fn poll(&mut self, _: Instant) -> Option<Instant> {
                None
            }
            fn stopped(&self) -> bool {
                false
            }
        }
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                crate::capture_loop::clear();
            }
        }
        let (_guard, mut state, generation) = stuck();
        crate::capture_loop::install(Box::new(Owner), Arc::new(|| {}));
        let _clear = Clear;
        let count = native::moves().len();
        state.reset_restore(generation, 1);
        assert_eq!(native::moves().len(), count);
        assert_eq!(native::hidden(), Some(true));
    }

    #[test]
    fn normal_marker_and_visible_uncover_never_reset() {
        let (_guard, mut state, generation) = stuck();
        native::warp(POINT { x: 3839, y: 900 });
        native::set_showing(true);
        assert!(state.uncover(generation));
        state.hider.finish_show(2);
        state.check_restore_completion();
        let count = native::moves().len();
        let started = state.local_restore.as_ref().unwrap().since;
        state.restore_deadline_at(started + DEADLINE);
        assert!(!state.local_restore.as_ref().unwrap().attempted);
        assert!(state.local_restore.as_ref().unwrap().verified);
        assert_eq!(native::moves().len(), count);
    }
}
