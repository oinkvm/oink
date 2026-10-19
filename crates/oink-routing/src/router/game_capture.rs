//! macOS remote-absolute blind spot: sustained motion while application-hidden is a product
//! policy, NOT proof that another process decoupled the cursor. CGEvent's relative
//! injection keeps its supplied location even when the cursor is still associated,
//! so switching injection to relative cannot serve as a coupling probe.
use super::Micros;

const ENTER_US: Micros = 250_000;
const EXIT_US: Micros = 200_000; // Same visibility debounce as the passive detector.
const MAX_GAP_US: Micros = 100_000;
const MIN_MOVES: u32 = 10;

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
    sample_at: Micros,
    movement_since: Option<Micros>,
    last_move: Option<Micros>,
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
        self.last_move = None;
        self.moves = 0;
    }

    /// Only called for accepted, nonzero remote absolute motion, never raw/game
    /// packets or the focus device's own physical input.
    pub fn motion(&mut self, now: Micros) {
        if self.detected
            || !self
                .sample
                .is_some_and(|s| s.hidden && s.application_hidden.unwrap_or(true))
            || now.saturating_sub(self.sample_at) > MAX_GAP_US
        {
            return;
        }
        if self.last_move.is_none_or(|t| now.saturating_sub(t) > MAX_GAP_US) {
            self.clear_movement();
            self.movement_since = Some(now);
        }
        self.moves = self.moves.saturating_add(1);
        self.last_move = Some(now);
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
        if now.saturating_sub(self.sample_at) > MAX_GAP_US {
            self.clear_movement();
        }
        self.sample = Some(s);
        self.sample_at = now;
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
        if self.last_move.is_none_or(|t| now.saturating_sub(t) > MAX_GAP_US) {
            self.clear_movement();
            return;
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
