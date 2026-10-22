//! Windows policy, independent of Win32. Sampling and input only supply facts.
//! Two consecutive qualifying samples plus intervening motion enter; two
//! consecutive failing samples exit. macOS policy is unchanged.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MotionBatch {
    last: Option<u64>,
}

impl MotionBatch {
    /// Record once per nonzero physical report or successful remote injection.
    pub fn record(&mut self, now: u64) {
        self.last = Some(now);
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
    previous: Option<(u64, bool)>,
    pub active: bool,
}

impl Detector {
    pub fn update(&mut self, s: super::CaptureSample, w: WindowsSample) -> bool {
        if self.foreground != Some(s.foreground) {
            *self = Self::default();
        }
        self.foreground = Some(s.foreground);
        let evidence = s.hidden && s.application_hidden != Some(false) && (s.confined || w.fullscreen);
        if let Some((previous_at, previous_evidence)) = self.previous {
            if evidence && previous_evidence {
                // The batch is drained under the same lock used to timestamp
                // this sample. Only movement after the preceding sample counts;
                // old/replayed batches cannot acquire a new capture interval.
                if w.motion.last.is_some_and(|t| previous_at < t && t <= w.sampled_at) {
                    self.active = true;
                }
            } else if !evidence && !previous_evidence {
                self.active = false;
            }
        }
        self.previous = Some((w.sampled_at, evidence));
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
        assert!(feed(
            d,
            sample(start + 50_000, true, fullscreen, confined, &[start + 25_000])
        ));
    }

    #[test]
    fn two_qualifying_samples_with_intervening_motion_enter() {
        for (fullscreen, confined) in [(true, false), (false, true), (true, true)] {
            let mut d = CaptureDetector::default();
            assert!(!feed(&mut d, sample(0, true, fullscreen, confined, &[])));
            assert!(feed(&mut d, sample(50_000, true, fullscreen, confined, &[25_000])));
        }
    }

    #[test]
    fn two_qualifying_samples_without_motion_do_not_enter() {
        let mut d = CaptureDetector::default();
        assert!(!feed(&mut d, sample(0, true, true, false, &[])));
        assert!(!feed(&mut d, sample(50_000, true, true, false, &[])));
    }

    #[test]
    fn two_failing_samples_exit() {
        let mut d = CaptureDetector::default();
        enter(&mut d, 0, true, false);
        assert!(feed(&mut d, sample(100_000, false, true, false, &[])));
        assert!(!feed(&mut d, sample(150_000, false, true, false, &[])));
    }

    #[test]
    fn one_failing_sample_does_not_exit() {
        let mut d = CaptureDetector::default();
        enter(&mut d, 0, true, false);
        assert!(feed(&mut d, sample(100_000, false, true, false, &[])));
        assert!(feed(&mut d, sample(150_000, true, true, false, &[])));
        assert!(feed(&mut d, sample(200_000, false, true, false, &[])));
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
    fn one_move_enters_without_duration_or_sampling_gap_thresholds() {
        for gap in [1, 50_000, 150_000, 1_000_000] {
            let mut d = CaptureDetector::default();
            assert!(!feed(&mut d, sample(0, true, true, false, &[])));
            assert!(feed(&mut d, sample(gap, true, true, false, &[gap])));
        }
    }

    #[test]
    fn movement_before_the_first_sample_or_outside_the_interval_cannot_enter() {
        let mut d = CaptureDetector::default();
        assert!(!feed(&mut d, sample(50_000, true, true, false, &[25_000])));
        assert!(!feed(&mut d, sample(100_000, true, true, false, &[])));
        assert!(!feed(&mut d, sample(150_000, true, true, false, &[25_000])));
        assert!(!feed(&mut d, sample(200_000, true, true, false, &[150_000])));
        assert!(!feed(&mut d, sample(250_000, true, true, false, &[300_000])));
        assert!(feed(&mut d, sample(300_000, true, true, false, &[275_000])));
    }

    #[test]
    fn one_failing_sample_breaks_the_entry_pair() {
        let mut d = CaptureDetector::default();
        assert!(!feed(&mut d, sample(0, true, true, false, &[])));
        assert!(!feed(&mut d, sample(50_000, false, true, false, &[25_000])));
        assert!(!feed(&mut d, sample(100_000, true, true, false, &[75_000])));
        assert!(feed(&mut d, sample(150_000, true, true, false, &[125_000])));
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
        assert_eq!(batch.last, Some(100_000));
    }
}
