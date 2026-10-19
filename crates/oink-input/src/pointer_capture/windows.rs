//! Windows policy, independent of Win32. Sampling and input only supply facts.
//! Motion thresholds match router/game_capture.rs; macOS policy is unchanged.
const ENTER_US: u64 = 300_000;
const MOTION_US: u64 = 250_000;
const EXIT_US: u64 = 200_000;
const MAX_GAP_US: u64 = 100_000;
const MIN_MOVES: u32 = 10;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MotionBatch {
    first: u64,
    last: u64,
    count: u32,
    max_gap: u64,
}

impl MotionBatch {
    /// Record once per nonzero physical report or successful remote injection.
    pub fn record(&mut self, now: u64) {
        if self.count == 0 {
            self.first = now;
        } else {
            self.max_gap = self.max_gap.max(now.saturating_sub(self.last));
        }
        self.last = now;
        self.count = self.count.saturating_add(1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowsSample {
    /// Same monotonic clock as the motion timestamps, independent of router time.
    pub sampled_at: u64,
    pub fullscreen: bool,
    /// Drained since the preceding platform sample. Never replay a batch.
    pub motion: MotionBatch,
}

/// Ignore zero displacement, buttons, wheels and invalid remote deltas.
pub fn nonzero_motion(dx: f64, dy: f64) -> bool {
    dx.is_finite() && dy.is_finite() && (dx != 0. || dy != 0.)
}

/// CURSOR_SUPPRESSED is touch/pen suppression, never capture evidence.
pub fn cursor_hidden(showing: bool, suppressed: bool, transparent: Option<bool>) -> bool {
    !suppressed && (!showing || transparent == Some(true))
}

/// Both renders must preserve RGB on black AND white, including AND/XOR cursors.
/// Alpha bytes in a GDI destination are not visibility evidence.
pub fn unchanged_render(pixels: &[u32], background: u32) -> bool {
    !pixels.is_empty() && pixels.iter().all(|p| p & 0x00ff_ffff == background)
}

#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Bounds {
    fn valid(self) -> bool {
        self.right > self.left && self.bottom > self.top
    }

    pub fn covers(self, monitor: Self) -> bool {
        self.valid()
            && monitor.valid()
            && self.left <= monitor.left
            && self.top <= monitor.top
            && self.right >= monitor.right
            && self.bottom >= monitor.bottom
    }

    pub fn confined(self, desktop: Self) -> bool {
        self.valid()
            && desktop.valid()
            && desktop.covers(self)
            && (i64::from(self.right) - i64::from(self.left) < i64::from(desktop.right) - i64::from(desktop.left)
                || i64::from(self.bottom) - i64::from(self.top) < i64::from(desktop.bottom) - i64::from(desktop.top))
    }
}

#[derive(Debug, Default)]
pub(super) struct Detector {
    foreground: Option<u64>,
    sampled_at: Option<u64>,
    hidden_since: Option<u64>,
    movement_since: Option<u64>,
    last_move: Option<u64>,
    moves: u32,
    released_since: Option<u64>,
    pub active: bool,
}

impl Detector {
    fn clear_motion(&mut self) {
        self.movement_since = None;
        self.last_move = None;
        self.moves = 0;
    }

    pub fn update(&mut self, s: super::CaptureSample, w: WindowsSample) -> bool {
        let now = w.sampled_at;
        if self.foreground != Some(s.foreground) {
            *self = Self::default();
        }
        self.foreground = Some(s.foreground);
        let continuous = self.sampled_at.is_some_and(|t| now.saturating_sub(t) <= MAX_GAP_US);
        let previous_sample = self.sampled_at;
        self.sampled_at = Some(now);
        let evidence = s.hidden && s.application_hidden != Some(false) && (s.confined || w.fullscreen);
        if !evidence {
            self.hidden_since = None;
            self.clear_motion();
            let since = *self.released_since.get_or_insert(now);
            if now.saturating_sub(since) >= EXIT_US {
                self.active = false;
            }
            return self.active;
        }
        self.released_since = None;
        if self.active {
            return true; // Once established, no motion is needed to maintain capture.
        }
        if !continuous || self.hidden_since.is_none() {
            self.hidden_since = Some(now);
            self.clear_motion();
            return false; // Discard movement preceding a qualifying visibility sample.
        }
        let b = w.motion;
        if b.count == 0 {
            if self.last_move.is_none_or(|t| now.saturating_sub(t) > MAX_GAP_US) {
                self.clear_motion();
            }
            return false; // A burst followed by waiting never completes entry.
        }
        if b.max_gap > MAX_GAP_US
            || now.saturating_sub(b.last) > MAX_GAP_US
            || previous_sample.is_none_or(|t| b.first < t)
        {
            self.clear_motion();
            return false;
        }
        if self.last_move.is_none_or(|t| b.first.saturating_sub(t) > MAX_GAP_US) {
            self.clear_motion();
            self.movement_since = Some(b.first);
        }
        self.moves = self.moves.saturating_add(b.count);
        self.last_move = Some(b.last);
        self.active = self.moves >= MIN_MOVES
            && self
                .movement_since
                .is_some_and(|t| b.last.saturating_sub(t) >= MOTION_US)
            && self.hidden_since.is_some_and(|t| now.saturating_sub(t) >= ENTER_US);
        self.active
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::pointer_capture::{CaptureDetector, CaptureSample};
    use oink_foundation::Point;

    fn sample(now: u64, hidden: bool, fullscreen: bool, confined: bool, moves: &[u64]) -> CaptureSample {
        let mut motion = MotionBatch::default();
        for &t in moves {
            motion.record(t);
        }
        CaptureSample {
            windows: Some(WindowsSample {
                sampled_at: now,
                fullscreen,
                motion,
            }),
            hidden,
            application_hidden: Some(hidden),
            confined,
            position: Point::new(100., 100.),
            motion_serial: 0,
            motion_observable: false,
            foreground: 42,
        }
    }
    fn feed(d: &mut CaptureDetector, s: CaptureSample) -> bool {
        d.update(Some(s), true, false, s.windows.unwrap().sampled_at)
    }
    fn enter(d: &mut CaptureDetector, start: u64, fullscreen: bool, confined: bool) {
        assert!(!feed(d, sample(start, true, fullscreen, confined, &[])));
        for i in 1..=6 {
            let t = start + i * 50_000;
            assert_eq!(feed(d, sample(t, true, fullscreen, confined, &[t - 25_000, t])), i == 6);
        }
    }

    #[test]
    fn hidden_or_transparent_excluding_touch_and_pen() {
        assert!(cursor_hidden(false, false, None));
        assert!(cursor_hidden(true, false, Some(true)));
        assert!(!cursor_hidden(true, false, Some(false)));
        assert!(!cursor_hidden(true, false, None));
        for showing in [false, true] {
            for transparent in [None, Some(false), Some(true)] {
                assert!(!cursor_hidden(showing, true, transparent));
            }
        }
        // The trail still sees an invisible cursor when touch/pen suppresses it.
        // The separate eligibility flag must veto capture even with moving input.
        let mut d = CaptureDetector::default();
        for i in 0..30 {
            let t = i * 50_000;
            let mut s = sample(t, true, true, true, &[t]);
            s.application_hidden = Some(cursor_hidden(false, true, Some(true)));
            assert!(!feed(&mut d, s));
        }
    }

    #[test]
    fn transparent_render_checks_rgb_on_both_backgrounds() {
        assert!(unchanged_render(&[0, 0xff00_0000], 0));
        assert!(unchanged_render(&[0x00ff_ffff, 0xffff_ffff], 0x00ff_ffff));
        // Solid black/white, partially visible and invert/XOR images aren't blank.
        assert!(!unchanged_render(&[0], 0x00ff_ffff));
        assert!(!unchanged_render(&[0x00ff_ffff], 0));
        assert!(!unchanged_render(&[0, 1], 0));
        assert!(!unchanged_render(&[], 0));
    }

    #[test]
    fn fullscreen_and_clip_geometry_including_negative_desktop_and_one_pixel() {
        let desktop = Bounds {
            left: -1920,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let monitor = Bounds { right: 0, ..desktop };
        assert!(monitor.covers(monitor)); // exclusive or borderless full-screen bounds
        assert!(desktop.covers(monitor)); // spanning window
        assert!(
            !Bounds {
                bottom: 1040,
                ..monitor
            }
            .covers(monitor)
        ); // taskbar remains
        assert!(!Bounds { right: -1, ..monitor }.covers(monitor));
        assert!(monitor.confined(desktop)); // one monitor of multi-monitor desktop
        assert!(
            Bounds {
                left: 100,
                top: 100,
                right: 101,
                bottom: 101
            }
            .confined(desktop)
        );
        assert!(!desktop.confined(desktop));
        assert!(
            !Bounds {
                right: -1920,
                ..desktop
            }
            .confined(desktop)
        );
        assert!(
            !Bounds {
                left: -3000,
                right: -2999,
                ..desktop
            }
            .confined(desktop)
        );
    }

    #[test]
    fn all_three_conditions_required_and_either_scope_is_sufficient() {
        for fullscreen in [false, true] {
            for confined in [false, true] {
                let mut d = CaptureDetector::default();
                if fullscreen || confined {
                    enter(&mut d, 0, fullscreen, confined);
                } else {
                    for i in 0..30 {
                        let t = i * 50_000;
                        assert!(!feed(&mut d, sample(t, true, false, false, &[t])));
                    }
                }
                let mut d = CaptureDetector::default();
                for i in 0..30 {
                    let t = i * 50_000;
                    assert!(!feed(&mut d, sample(t, false, fullscreen, confined, &[t])));
                }
            }
        }
    }

    #[test]
    fn hidden_idle_never_enters_but_confirmed_capture_survives_idle() {
        let mut d = CaptureDetector::default();
        for i in 0..50 {
            assert!(!feed(&mut d, sample(i * 50_000, true, true, true, &[])));
        }
        enter(&mut d, 3_000_000, true, false);
        assert!(feed(&mut d, sample(9_000_000, true, true, false, &[])));
    }

    #[test]
    fn fullscreen_video_slides_and_typing_reveal_on_move_do_not_enter() {
        for confined in [false, true] {
            let mut d = CaptureDetector::default();
            for cycle in 0..4 {
                let base = cycle * 1_000_000;
                for i in 0..10 {
                    assert!(!feed(&mut d, sample(base + i * 50_000, true, true, confined, &[])));
                }
                for i in 10..20 {
                    let t = base + i * 50_000;
                    assert!(!feed(&mut d, sample(t, false, true, confined, &[t - 25_000, t])));
                }
            }
        }
    }

    #[test]
    fn short_bursts_sparse_moves_and_sampling_gaps_do_not_enter() {
        let mut d = CaptureDetector::default();
        feed(&mut d, sample(0, true, true, false, &[]));
        let burst: Vec<_> = (1..=20).map(|i| i * 1000).collect();
        assert!(!feed(&mut d, sample(50_000, true, true, false, &burst)));
        for i in 2..30 {
            assert!(!feed(&mut d, sample(i * 50_000, true, true, false, &[])));
        }
        // Enough elapsed time, fewer than ten events in each continuous run.
        let mut d = CaptureDetector::default();
        for i in 0..9 {
            assert!(!feed(&mut d, sample(i * 50_000, true, true, false, &[i * 50_000])));
        }
        // >100 ms breaks motion, even when visibility samples remain fresh.
        for i in 9..12 {
            assert!(!feed(&mut d, sample(i * 50_000, true, true, false, &[])));
        }
        for i in 12..20 {
            assert!(!feed(&mut d, sample(i * 50_000, true, true, false, &[i * 50_000])));
        }
        // Sparse observations cannot bridge unobserved visibility intervals.
        let mut d = CaptureDetector::default();
        for i in 0..30 {
            assert!(!feed(&mut d, sample(i * 150_000, true, true, false, &[i * 150_000])));
        }
    }

    #[test]
    fn movement_duration_does_not_bypass_300ms_entry_stability() {
        let mut d = CaptureDetector::default();
        assert!(!feed(&mut d, sample(0, true, true, false, &[])));
        for i in 1..=30 {
            let t = i * 10_000;
            // By 260 ms there are 26 moves spanning 250 ms, but entry still waits.
            assert_eq!(feed(&mut d, sample(t, true, true, false, &[t])), i == 30);
        }
    }

    #[test]
    fn visibility_interruption_clears_candidate_and_release_debounces_200ms() {
        let mut d = CaptureDetector::default();
        for i in 0..6 {
            let t = i * 50_000;
            assert!(!feed(&mut d, sample(t, true, true, false, &[t])));
        }
        assert!(!feed(&mut d, sample(300_000, false, true, false, &[300_000])));
        enter(&mut d, 350_000, true, false);
        assert!(feed(&mut d, sample(700_000, false, true, false, &[])));
        assert!(feed(&mut d, sample(899_999, false, true, false, &[])));
        assert!(!feed(&mut d, sample(900_000, false, true, false, &[])));
        enter(&mut d, 950_000, true, false);
    }

    #[test]
    fn foreground_focus_pause_off_and_missing_sample_clear_all_evidence() {
        let mut d = CaptureDetector::default();
        enter(&mut d, 0, true, false);
        let mut changed = sample(350_000, true, true, false, &[350_000]);
        changed.foreground += 1;
        assert!(!feed(&mut d, changed));
        for missing in [false, true] {
            let mut d = CaptureDetector::default();
            enter(&mut d, 0, false, true);
            let s = sample(350_000, true, false, true, &[350_000]);
            // The service marks focus-away, pause and Windows manual modes ineligible.
            assert!(!d.update(if missing { None } else { Some(s) }, missing, false, 350_000));
            enter(&mut d, 400_000, false, true);
        }
    }

    #[test]
    fn losing_fullscreen_and_clip_exits_but_either_one_keeps_capture() {
        let mut d = CaptureDetector::default();
        enter(&mut d, 0, true, false);
        assert!(feed(&mut d, sample(350_000, true, false, true, &[])));
        assert!(feed(&mut d, sample(400_000, true, false, false, &[])));
        assert!(!feed(&mut d, sample(600_000, true, false, false, &[])));
    }

    #[test]
    fn always_hidden_slides_and_custom_cursor_apps_are_known_false_positives() {
        // This policy cannot distinguish them from a full-screen game. Use Off.
        let mut d = CaptureDetector::default();
        enter(&mut d, 0, true, false);
    }
}

#[cfg(all(test, debug_assertions))]
mod motion_tests {
    use super::*;

    #[test]
    fn nonzero_physical_and_remote_deltas_share_the_same_evidence() {
        assert!(nonzero_motion(1., 0.));
        assert!(nonzero_motion(0., -0.5));
        assert!(!nonzero_motion(0., -0.));
        assert!(!nonzero_motion(f64::NAN, 1.));
        assert!(!nonzero_motion(1., f64::INFINITY));
        let mut batch = MotionBatch::default();
        for (now, dx, dy) in [(0, 1., 0.), (50_000, 0., 0.), (100_000, 0., 1.)] {
            if nonzero_motion(dx, dy) {
                batch.record(now);
            }
        }
        assert_eq!(
            (batch.count, batch.first, batch.last, batch.max_gap),
            (2, 0, 100_000, 100_000)
        );
    }
}
