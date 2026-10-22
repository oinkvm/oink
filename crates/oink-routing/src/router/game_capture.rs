//! macOS remote-absolute blind spot: motion followed by application-hidden time is a product
//! policy, NOT proof that another process decoupled the cursor. CGEvent's relative
//! injection keeps its supplied location even when the cursor is still associated,
//! so switching injection to relative cannot serve as a coupling probe.
use super::Micros;

const ENTER_US: Micros = 100_000;
const EXIT_US: Micros = 100_000; // Same visibility debounce as the macOS passive detector.
const MIN_MOVES: u32 = 1;

/// Platform snapshot after any unknown-visibility reconciliation; the router
/// supplies remote injection activity. This detector never mutates cursor state.
#[derive(Clone, Copy, Debug)]
pub struct PointerCaptureObservation {
    pub hidden: bool,
    /// Excludes typing hide-until-move. Unknown falls back to the existing visibility/motion rule.
    pub application_hidden: Option<bool>,
    pub foreground: u64,
}

#[derive(Debug, Default)]
pub(super) struct RemoteCapture {
    sample: Option<PointerCaptureObservation>,
    movement_since: Option<Micros>,
    moves: u32,
    released_since: Option<Micros>,
    pub detected: bool,
}

impl RemoteCapture {
    pub fn reset(&mut self, reason: &'static str) {
        if self.detected {
            tracing::info!(reason, "remote hidden pointer capture ended");
        }
        *self = Self::default();
    }

    fn clear_movement(&mut self) {
        self.movement_since = None;
        self.moves = 0;
    }

    /// Only called for accepted, nonzero remote absolute motion, never raw/game
    /// packets or the focus device's own physical input.
    pub fn motion(&mut self, now: Micros) {
        if self.detected
            || !self
                .sample
                .is_some_and(|s| s.hidden && s.application_hidden.unwrap_or(true))
        {
            return;
        }
        self.movement_since.get_or_insert(now);
        self.moves = self.moves.saturating_add(1);
    }

    pub fn local_motion(&mut self) {
        // A mixed/local-input interval cannot establish remote-only evidence.
        self.clear_movement();
    }

    pub fn observe(&mut self, sample: Option<PointerCaptureObservation>, now: Micros) {
        let Some(s) = sample else {
            self.reset("sample_unavailable");
            return;
        };
        if self.sample.is_some_and(|p| p.foreground != s.foreground) {
            self.reset("foreground_changed");
        }
        self.sample = Some(s);
        if !s.hidden || s.application_hidden == Some(false) {
            self.clear_movement();
            let since = *self.released_since.get_or_insert(now);
            if self.detected && now.saturating_sub(since) >= EXIT_US {
                self.detected = false;
                tracing::info!(
                    reason = if s.hidden { "cursor_obscured" } else { "cursor_visible" },
                    released_us = now.saturating_sub(since),
                    "remote hidden pointer capture ended"
                );
            }
            return;
        }
        self.released_since = None;
        if self.detected {
            return; // No movement is necessary to keep a confirmed hidden capture.
        }
        if let Some(since) = self.movement_since
            && now.saturating_sub(since) >= ENTER_US
            && self.moves >= MIN_MOVES
        {
            self.detected = true;
            tracing::info!(
                reason = "hidden_during_remote_absolute_motion",
                hidden_motion_us = now.saturating_sub(since),
                remote_moves = self.moves,
                foreground = s.foreground,
                "remote hidden pointer capture detected"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hidden() -> PointerCaptureObservation {
        PointerCaptureObservation {
            hidden: true,
            application_hidden: Some(true),
            foreground: 42,
        }
    }

    #[test]
    fn enters_at_100ms_after_one_move() {
        let mut d = RemoteCapture::default();
        d.observe(Some(hidden()), 0);
        d.motion(1_000);
        d.observe(Some(hidden()), 100_999);
        assert!(!d.detected);
        d.observe(Some(hidden()), 101_000);
        assert!(d.detected);
    }

    #[test]
    fn exits_at_100ms_after_capture_release() {
        let mut d = RemoteCapture::default();
        d.observe(Some(hidden()), 0);
        for now in (0..=600_000).step_by(50_000) {
            d.motion(now);
            d.observe(Some(hidden()), now);
        }
        assert!(d.detected);
        let released = PointerCaptureObservation {
            hidden: false,
            ..hidden()
        };
        d.observe(Some(released), 700_000);
        d.observe(Some(released), 799_999);
        assert!(d.detected);
        d.observe(Some(released), 800_000);
        assert!(!d.detected);
    }

    #[test]
    fn hiding_without_motion_and_typing_hiding_cannot_enter() {
        for application_hidden in [Some(true), Some(false)] {
            let mut d = RemoteCapture::default();
            let sample = PointerCaptureObservation {
                application_hidden,
                ..hidden()
            };
            d.observe(Some(sample), 0);
            for now in (50_000..=500_000).step_by(50_000) {
                if application_hidden == Some(false) {
                    d.motion(now);
                }
                d.observe(Some(sample), now);
                assert!(!d.detected);
            }
        }
    }

    #[test]
    fn foreground_change_and_local_input_revoke_pending_remote_motion() {
        for foreground_change in [false, true] {
            let mut d = RemoteCapture::default();
            d.observe(Some(hidden()), 0);
            d.motion(1_000);
            let mut sample = hidden();
            if foreground_change {
                sample.foreground += 1;
                d.observe(Some(sample), 50_000);
            } else {
                d.local_motion();
            }
            d.observe(Some(sample), 200_000);
            assert!(!d.detected);
            d.motion(201_000);
            d.observe(Some(sample), 301_000);
            assert!(d.detected);
        }
    }

    #[test]
    fn sampling_and_movement_gaps_do_not_discard_entry_evidence() {
        for second_move in [false, true] {
            let mut d = RemoteCapture::default();
            d.observe(Some(hidden()), 0);
            d.motion(200_000); // More than 100ms since the visibility sample.
            if second_move {
                d.motion(400_000); // More than 100ms between movements.
            }
            d.observe(Some(hidden()), 600_000);
            assert!(d.detected);
        }
    }
}
