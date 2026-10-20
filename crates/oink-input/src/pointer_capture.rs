//! Read-only platform evidence and a conservative capture detector.
//! Hiding alone never enters. Oink-owned Remote state is excluded by the caller.
use oink_foundation::Point;

pub mod windows;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptureSample {
    /// Windows-only full-screen and timestamped motion evidence.
    pub windows: Option<windows::WindowsSample>,
    /// Platform-reported visibility, also used to suppress the pointer trail.
    /// On macOS the raw query can retain a stale typing-obscure value; the service
    /// refreshes ambiguous samples before feeding automatic detectors.
    pub hidden: bool,
    /// Deliberate application hiding, excluding macOS hide-until-mouse-moves.
    /// None falls back to visibility; a known typing-only hide still excludes capture.
    /// Windows excludes touch/pen suppression and includes transparent cursors.
    pub application_hidden: Option<bool>,
    /// Windows: a clipping rectangle smaller than the virtual desktop.
    pub confined: bool,
    pub position: Point,
    /// macOS session mouse-movement event counter (wrapping u32); includes injection.
    pub motion_serial: u32,
    /// Counter is meaningful (macOS). Windows uses timestamped motion batches.
    pub motion_observable: bool,
    /// Foreground application/window identity. Changes invalidate accumulated evidence.
    pub foreground: u64,
}

/// Fixed-size diagnostic view. Export on the ordinary recorder consumer only.
#[derive(Clone, Copy, Debug)]
pub struct CaptureEvidence {
    pub reason: &'static str,
    pub stationary_us: u64,
    pub movement_samples: u32,
    pub warp_locked: bool,
    pub enter_pending: bool,
    pub exit_pending: bool,
}

#[derive(Debug, Default)]
pub struct CaptureDetector {
    windows: windows::Detector,
    previous: Option<CaptureSample>,
    stationary_since: u64,
    anchor: Option<Point>,
    movement_samples: u32,
    enter_since: Option<u64>,
    exit_since: Option<u64>,
    active: bool,
    warp_lock: bool,
    diagnostic: Option<(&'static str, bool, u64)>,
}

impl CaptureDetector {
    pub fn evidence(&self, now: u64) -> CaptureEvidence {
        CaptureEvidence {
            reason: self.diagnostic.map_or("sample_unavailable", |d| d.0),
            stationary_us: now.saturating_sub(self.stationary_since),
            movement_samples: self.movement_samples,
            warp_locked: self.warp_lock,
            enter_pending: self.enter_since.is_some() && !self.active,
            exit_pending: self.exit_since.is_some() && self.active,
        }
    }

    pub fn update(&mut self, sample: Option<CaptureSample>, eligible: bool, interior: bool, now: u64) -> bool {
        let Some(s) = sample.filter(|_| eligible) else {
            self.reset();
            self.log_transition(
                if !eligible {
                    "not_focus_or_paused"
                } else {
                    "sample_unavailable"
                },
                sample,
                now,
            );
            return false;
        };
        if let Some(w) = s.windows {
            self.active = self.windows.update(s, w);
            self.log_transition(
                if !s.hidden || s.application_hidden == Some(false) {
                    "cursor_visible_or_suppressed"
                } else if !(s.confined || w.fullscreen) {
                    "windows_not_fullscreen_or_confined"
                } else if self.active {
                    "windows_hidden_motion"
                } else {
                    "windows_capture_pending"
                },
                Some(s),
                now,
            );
            self.previous = Some(s);
            return self.active;
        }
        // Hide-cause is supplemental. HID motion can clear server-side typing
        // hiding while the macOS visibility query remains stale. A known cause
        // excludes that case; unsupported readers use visibility reconciled by
        // the platform hook before this pure detector.
        let hidden = s.hidden && s.application_hidden.unwrap_or(true);
        let same_app = self.previous.is_some_and(|p| p.foreground == s.foreground);
        let stationary = same_app
            && self
                .anchor
                .is_some_and(|p| (p.x - s.position.x).abs() <= 1. && (p.y - s.position.y).abs() <= 1.);
        let hide_warp = s.motion_observable
            && hidden
            && self.previous.is_some_and(|p| {
                !(p.hidden && p.application_hidden.unwrap_or(true))
                    && p.motion_serial == s.motion_serial
                    && (p.position.x - s.position.x).hypot(p.position.y - s.position.y) >= 8.
            });
        if !same_app {
            self.reset();
            self.stationary_since = now;
        }
        if hide_warp && interior {
            self.warp_lock = true;
        }
        if !hidden {
            self.warp_lock = false;
        }
        if !stationary || !hidden || !interior {
            self.stationary_since = now;
            self.anchor = Some(s.position);
            self.movement_samples = 0;
        } else if self.previous.is_some_and(|p| p.motion_serial != s.motion_serial) {
            self.movement_samples = self.movement_samples.saturating_add(1);
        }
        // Require motion in three distinct sampling intervals at one interior point.
        // Once established, idle while still hidden at that point is not a release.
        let motion_lock = s.motion_observable
            && interior
            && stationary
            && (self.active || (self.movement_samples >= 3 && now.saturating_sub(self.stationary_since) >= 250_000));
        let evidence = hidden && (s.confined || motion_lock || self.warp_lock);
        if evidence {
            self.exit_since = None;
            let start = *self.enter_since.get_or_insert(now);
            if now.saturating_sub(start) >= 300_000 {
                self.active = true;
            }
        } else {
            self.enter_since = None;
            let start = *self.exit_since.get_or_insert(now);
            if now.saturating_sub(start) >= 200_000 {
                self.active = false;
            }
        }
        let reason = if !s.hidden {
            "cursor_visible"
        } else if !hidden {
            "cursor_obscured"
        } else if s.confined {
            "hidden_confined"
        } else if self.warp_lock {
            "hidden_warp"
        } else if motion_lock {
            "hidden_stationary_motion"
        } else {
            "hidden_without_capture_evidence"
        };
        self.log_transition(reason, Some(s), now);
        self.previous = Some(s);
        self.active
    }

    fn reset(&mut self) {
        let diagnostic = self.diagnostic;
        *self = Self {
            diagnostic,
            ..Self::default()
        };
    }

    // Only evidence/decision/foreground transitions, never a per-sample trace.
    // In particular, hidden_without_capture_evidence distinguishes an observed
    // hide from a successful capture detection on machines logging at info.
    fn log_transition(&mut self, reason: &'static str, sample: Option<CaptureSample>, now: u64) {
        let foreground = sample.map_or(0, |s| s.foreground);
        let state = (reason, self.active, foreground);
        if self.diagnostic == Some(state) {
            return;
        }
        tracing::info!(
            reason, detected = self.active, foreground,
            hidden = ?sample.map(|s| s.hidden),
            application_hidden = ?sample.and_then(|s| s.application_hidden),
            fullscreen = ?sample.and_then(|s| s.windows.map(|w| w.fullscreen)),
            confined = ?sample.map(|s| s.confined),
            position = ?sample.map(|s| s.position),
            motion_events = ?sample.zip(self.previous).map(|(s, p)| s.motion_serial.wrapping_sub(p.motion_serial)),
            stationary_us = now.saturating_sub(self.stationary_since),
            movement_samples = self.movement_samples,
            enter_pending = self.enter_since.is_some() && !self.active,
            exit_pending = self.exit_since.is_some() && self.active,
            "pointer capture evidence changed"
        );
        self.diagnostic = Some(state);
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    fn sample(hidden: bool, confined: bool, serial: u32) -> Option<CaptureSample> {
        Some(CaptureSample {
            windows: None,
            hidden,
            application_hidden: Some(hidden),
            confined,
            position: Point::new(100., 100.),
            motion_serial: serial,
            motion_observable: true,
            foreground: 42,
        })
    }
    #[test]
    fn hiding_for_video_or_typing_does_not_enter() {
        let mut d = CaptureDetector::default();
        for i in 0..100 {
            assert!(!d.update(sample(true, false, 0), true, true, i * 50_000));
        }
    }
    #[test]
    fn clipping_requires_hidden_and_debounce() {
        let mut d = CaptureDetector::default();
        assert!(!d.update(sample(false, true, 0), true, true, 0));
        assert!(!d.update(sample(true, true, 0), true, true, 100_000));
        assert!(!d.update(sample(true, true, 0), true, true, 399_999));
        assert!(d.update(sample(true, true, 0), true, true, 400_000));
        assert!(d.update(sample(false, false, 0), true, true, 500_000));
        assert!(!d.update(sample(false, false, 0), true, true, 700_000));
    }
    #[test]
    fn stationary_movement_enters_and_idle_does_not_exit() {
        let mut d = CaptureDetector::default();
        for i in 0..11 {
            assert!(!d.update(sample(true, false, i as u32), true, true, i * 50_000));
        }
        assert!(d.update(sample(true, false, 11), true, true, 550_000));
        assert!(d.update(sample(true, false, 11), true, true, 2_000_000));
        assert!(!d.update(sample(true, false, 12), false, true, 2_100_000));
    }
    #[test]
    fn edges_and_foreground_changes_do_not_count() {
        let mut d = CaptureDetector::default();
        for i in 0..100 {
            assert!(!d.update(sample(true, false, i as u32), true, false, i * 50_000));
        }
        assert!(!d.update(sample(true, true, 0), true, true, 5_000_000));
        assert!(d.update(sample(true, true, 0), true, true, 5_300_000));
        let mut s = sample(true, false, 0).unwrap();
        s.foreground = 99;
        assert!(!d.update(Some(s), true, true, 5_400_000));
    }
}

#[cfg(all(test, debug_assertions))]
mod warp_tests {
    use super::*;
    fn sample(hidden: bool, x: f64, serial: u32) -> Option<CaptureSample> {
        Some(CaptureSample {
            windows: None,
            hidden,
            application_hidden: Some(hidden),
            confined: false,
            position: Point::new(x, 100.),
            motion_serial: serial,
            motion_observable: true,
            foreground: 42,
        })
    }
    #[test]
    fn hide_with_no_event_warp_survives_absolute_injections_during_debounce() {
        let mut d = CaptureDetector::default();
        assert!(!d.update(sample(false, 200., 0), true, true, 0));
        assert!(!d.update(sample(true, 100., 0), true, true, 50_000));
        assert!(!d.update(sample(true, 104., 1), true, true, 100_000));
        assert!(d.update(sample(true, 120., 5), true, true, 350_000));
        assert!(d.update(sample(false, 120., 5), true, true, 400_000));
        assert!(!d.update(sample(false, 120., 5), true, true, 600_000));
    }
    #[test]
    fn hidden_movement_and_slow_drift_are_not_a_lock() {
        let mut d = CaptureDetector::default();
        d.update(sample(false, 200., 0), true, true, 0);
        for i in 1..200 {
            assert!(!d.update(
                sample(true, 200. + f64::from(i) * 0.5, i),
                true,
                true,
                u64::from(i) * 50_000
            ));
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod recapture_tests {
    use super::*;

    fn sample(hidden: bool, x: f64, serial: u32) -> Option<CaptureSample> {
        Some(CaptureSample {
            windows: None,
            hidden,
            application_hidden: Some(hidden),
            confined: false,
            position: Point::new(x, 100.),
            motion_serial: serial,
            motion_observable: true,
            foreground: 42,
        })
    }

    #[test]
    fn stationary_capture_release_recapture_has_no_cooldown() {
        let mut d = CaptureDetector::default();
        for cycle in 0..3 {
            let base = cycle * 1_000_000;
            for i in 0..=11 {
                let active = d.update(sample(true, 100., i), true, true, base + u64::from(i) * 50_000);
                if i == 11 {
                    assert!(active);
                }
            }
            assert!(d.update(sample(false, 100., 11), true, true, base + 600_000));
            assert!(!d.update(sample(false, 100., 11), true, true, base + 800_000));
        }
    }

    #[test]
    fn warp_capture_release_recapture_rebuilds_evidence() {
        let mut d = CaptureDetector::default();
        for cycle in 0..3 {
            let base = cycle * 1_000_000;
            assert!(!d.update(sample(false, 200., 0), true, true, base));
            assert!(!d.update(sample(true, 100., 0), true, true, base + 50_000));
            assert!(d.update(sample(true, 120., 1), true, true, base + 350_000));
            assert!(d.update(sample(false, 120., 1), true, true, base + 400_000));
            assert!(!d.update(sample(false, 120., 1), true, true, base + 600_000));
        }
    }

    #[test]
    fn single_visible_frame_does_not_exit_but_does_invalidate_warp_evidence() {
        let mut d = CaptureDetector::default();
        d.update(sample(false, 200., 0), true, true, 0);
        d.update(sample(true, 100., 0), true, true, 50_000);
        assert!(d.update(sample(true, 100., 0), true, true, 350_000));
        assert!(d.update(sample(false, 100., 0), true, true, 400_000));
        assert!(d.update(sample(true, 100., 0), true, true, 450_000));
        assert!(d.update(sample(true, 100., 0), true, true, 900_000));
    }

    #[test]
    fn recapture_without_warp_with_absolute_motion_is_not_observable() {
        // First capture has a visible-to-hidden warp. The second captures at the
        // same point and receives absolute moves: identical to ordinary hidden
        // pointer movement, so it must not inherit the first capture's evidence.
        let mut d = CaptureDetector::default();
        d.update(sample(false, 200., 0), true, true, 0);
        d.update(sample(true, 100., 0), true, true, 50_000);
        assert!(d.update(sample(true, 100., 0), true, true, 350_000));
        d.update(sample(false, 100., 0), true, true, 400_000);
        assert!(!d.update(sample(false, 100., 0), true, true, 600_000));
        for i in 0..30 {
            assert!(!d.update(
                sample(true, 100. + 4. * f64::from(i), i),
                true,
                true,
                700_000 + u64::from(i) * 50_000
            ));
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod hiding_reason_tests {
    use super::*;

    fn sample(application_hidden: Option<bool>, x: f64, serial: u32) -> Option<CaptureSample> {
        Some(CaptureSample {
            windows: None,
            hidden: true,
            application_hidden,
            confined: false,
            position: Point::new(x, 100.),
            motion_serial: serial,
            motion_observable: true,
            foreground: 42,
        })
    }

    #[test]
    fn unknown_cause_uses_stationary_motion_and_visible_release() {
        let mut d = CaptureDetector::default();
        for i in 0..11 {
            assert!(!d.update(sample(None, 100., i), true, true, u64::from(i) * 50_000));
        }
        assert!(d.update(sample(None, 100., 11), true, true, 550_000));
        let mut visible = sample(None, 100., 11).unwrap();
        visible.hidden = false;
        assert!(d.update(Some(visible), true, true, 600_000));
        assert!(!d.update(Some(visible), true, true, 800_000));
    }

    #[test]
    fn unknown_cause_uses_hide_warp() {
        let mut d = CaptureDetector::default();
        let mut visible = sample(None, 200., 0).unwrap();
        visible.hidden = false;
        assert!(!d.update(Some(visible), true, true, 0));
        assert!(!d.update(sample(None, 100., 0), true, true, 50_000));
        assert!(d.update(sample(None, 100., 0), true, true, 350_000));
    }

    #[test]
    fn known_typing_obscure_cannot_use_warp_or_stationary_motion() {
        let mut d = CaptureDetector::default();
        d.update(sample(Some(false), 200., 0), true, true, 0);
        // A coincident warp followed by many stationary motion samples used
        // to make hide-until-move indistinguishable from a captured pointer.
        assert!(!d.update(sample(Some(false), 100., 0), true, true, 50_000));
        for i in 1..100 {
            assert!(!d.update(sample(Some(false), 100., i), true, true, 50_000 + u64::from(i) * 50_000));
        }
    }

    #[test]
    fn releasing_hide_while_still_obscured_exits_and_rebuilds_capture() {
        let mut d = CaptureDetector::default();
        for i in 0..=11 {
            d.update(sample(Some(true), 100., i), true, true, u64::from(i) * 50_000);
        }
        assert!(d.active);
        assert!(d.update(sample(Some(false), 100., 11), true, true, 600_000));
        assert!(d.update(sample(Some(false), 100., 11), true, true, 799_999));
        assert!(!d.update(sample(Some(false), 100., 11), true, true, 800_000));
        for i in 0..=11 {
            d.update(
                sample(Some(true), 100., 12 + i),
                true,
                true,
                850_000 + u64::from(i) * 50_000,
            );
        }
        assert!(d.active);
        // Loss of only the cause retains visibility evidence; total sample loss resets.
        assert!(d.update(sample(None, 100., 24), true, true, 1_450_000));
        assert!(!d.update(None, true, true, 1_500_000));
    }
}
