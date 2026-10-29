//! Source-owned scrolling translated to two parallel synthetic contacts.
//!
//! The Windows recognizer accelerates contact velocity and has a pan threshold.
//! Invert the contact-speed gain before injection, retaining the source's curve.
//! The model was checked against native output on Windows 11 build 26200.9457;
//! the recognizer is not a documented cross-version contract.
use oink_foundation::input::ScrollPhase;
use std::time::Duration;

/// Bridge Ended to the optional source momentum without a synthetic re-touch.
pub(crate) const MOMENTUM_GRACE: Duration = Duration::from_millis(32);

#[derive(Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Idle,
    Fingers,
    AwaitMomentum,
    Momentum,
    Wheel,
}

const ORIGIN: i32 = 500_000;
const LIMIT: i32 = 450_000;
const LEAD_IN: f64 = 320.0;
// Normalize only the recognizer clock, never wall-clock delivery. At real source
// intervals of 1–2ms, timestamp quantization and velocity history change gain.
// 96ms keeps the measured inverse stable while contact speed still varies with
// each source delta. Same 1915px trace: real clock 1936px, normalized 1906px.
const FRAME_MS: u32 = 96;

/// The measured gain is 1 + 4*min(v_mm_per_second/200, 1), per axis.
/// Thus a = h*(1 + .2*h/dt) below the cap, where a is the desired displacement
/// divided by DPI/2540 and the user's pan-sensitivity multiplier; h is HIMETRIC.
/// The positive quadratic root is monotonic, with the linear branch above 200mm/s.
/// Coefficients from local ninput 10.0.26100.9278 analysis, independently checked
/// at 100/160/640mm/s: measured .227/.3175/.37797 physical px/HIMETRIC at 192DPI.
fn inverse(amount: f64, pixels_per_himetric: f64) -> f64 {
    let a = amount.abs() / pixels_per_himetric;
    let dt = f64::from(FRAME_MS);
    let h = if a >= 100.0 * dt {
        a / 5.0
    } else {
        2.0 * a / (1.0 + (1.0 + 0.8 * a / dt).sqrt())
    };
    h.copysign(amount)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub x: i32,
    pub y: i32,
    pub tick: u32,
    pub cancel: bool,
}

pub(crate) trait Sink {
    fn frame(&mut self, frame: Frame) -> bool;
    fn stop_inertia(&mut self);
}

#[derive(Debug)]
pub(crate) struct Pan {
    position: (i32, i32),
    fraction: (f64, f64),
    tick: u32,
    active: bool,
    phase: Phase,
}

impl Default for Pan {
    fn default() -> Self {
        Self {
            position: (ORIGIN, ORIGIN),
            fraction: (0.0, 0.0),
            tick: 1,
            active: false,
            phase: Phase::Idle,
        }
    }
}

impl Pan {
    pub fn active(&self) -> bool {
        self.active
    }

    pub fn awaiting_momentum(&self) -> bool {
        self.active && self.phase == Phase::AwaitMomentum
    }

    pub fn idle_timeout(&self) -> Duration {
        if self.awaiting_momentum() {
            MOMENTUM_GRACE
        } else {
            Duration::from_millis(500)
        }
    }

    fn send(&mut self, sink: &mut impl Sink, x: i32, y: i32, elapsed: u32, cancel: bool) -> bool {
        self.tick = self.tick.wrapping_add(elapsed).max(1);
        if !sink.frame(Frame {
            x,
            y,
            tick: self.tick,
            cancel,
        }) {
            return false;
        }
        self.position = (x, y);
        true
    }

    pub fn cancel(&mut self, sink: &mut impl Sink) -> bool {
        self.phase = Phase::Idle;
        if !self.active {
            return true;
        }
        // CANCEL alone retains the last moving velocity for Windows' next-pan
        // boost: four quick equal pans measured 288/555/1226/2593 CSS px, even
        // with a 600ms logical gap. A stationary in-contact frame first clears
        // that velocity (261/264/265/269px). Neither call waits in wall time.
        // Tested on build 26200.9457; a regular UP can also start OS inertia.
        let stopped = self.send(sink, self.position.0, self.position.1, 100, false);
        let cancelled = self.send(sink, self.position.0, self.position.1, 100, true);
        self.active = false;
        self.fraction = (0.0, 0.0);
        sink.stop_inertia();
        stopped && cancelled
    }

    pub fn expire(&mut self, sink: &mut impl Sink) -> bool {
        let fingers = self.phase == Phase::Fingers;
        let tail = matches!(self.phase, Phase::AwaitMomentum | Phase::Momentum);
        let ok = self.cancel(sink);
        if fingers {
            // Retain the existing recovery for fingers paused beyond the idle
            // watchdog: a later Changed can resume the physical finger scroll.
            self.phase = Phase::Fingers;
        } else if tail {
            // Network scheduling can delay MomentumBegan past the short bridge.
            // Preserve those deltas via wheel fallback, never another contact DOWN.
            self.phase = Phase::Wheel;
        }
        ok
    }

    /// Return unsubmitted fractions of the source event for exact wheel fallback.
    /// A successful native event returns (0,0). Priming never consumes source data.
    pub fn scroll(
        &mut self,
        sink: &mut impl Sink,
        pixels: (f64, f64),
        pixels_per_himetric: f64,
        phase: ScrollPhase,
    ) -> (f64, f64) {
        if phase == ScrollPhase::Began {
            if !self.cancel(sink) {
                return (1.0, 1.0);
            }
            self.phase = Phase::Fingers;
        } else if phase == ScrollPhase::Cancelled {
            self.cancel(sink);
            return (0.0, 0.0);
        } else if self.phase == Phase::Wheel {
            if phase == ScrollPhase::MomentumEnded {
                self.phase = Phase::Idle;
            }
            return (1.0, 1.0);
        } else {
            match phase {
                ScrollPhase::MomentumBegan if self.awaiting_momentum() => self.phase = Phase::Momentum,
                ScrollPhase::Momentum | ScrollPhase::MomentumEnded if self.phase == Phase::Momentum => {}
                ScrollPhase::Changed | ScrollPhase::Ended if self.phase == Phase::Fingers => {}
                // Interrupted/late momentum cannot create contacts or end a new touch.
                _ => return (0.0, 0.0),
            }
        }
        if !pixels.0.is_finite() || !pixels.1.is_finite() {
            self.cancel(sink);
            return (0.0, 0.0);
        }
        if !pixels_per_himetric.is_finite() || pixels_per_himetric <= 0.0 {
            self.cancel(sink);
            return (1.0, 1.0);
        }
        if !self.active {
            self.position = (ORIGIN, ORIGIN);
        }
        let delta = (
            inverse(pixels.0, pixels_per_himetric),
            inverse(pixels.1, pixels_per_himetric),
        );
        if delta.0 != 0.0 || delta.1 != 0.0 {
            // Bound conversions, without re-touching inside a gesture.
            if delta.0.abs().max(delta.1.abs()) > f64::from(LIMIT) {
                self.cancel(sink);
                self.phase = Phase::Wheel;
                return (1.0, 1.0);
            }
            if (f64::from(self.position.0 - ORIGIN) + delta.0).abs() > f64::from(LIMIT)
                || (f64::from(self.position.1 - ORIGIN) + delta.1).abs() > f64::from(LIMIT)
            {
                // Never recenter/re-touch inside an active fling. Use the wheel
                // fallback for the rest of an exceptionally long gesture.
                self.cancel(sink);
                self.phase = Phase::Wheel;
                return (1.0, 1.0);
            }
            if !self.active {
                self.position = (ORIGIN, ORIGIN);
                if !self.send(sink, ORIGIN, ORIGIN, 1, false) {
                    return (1.0, 1.0);
                }
                self.active = true;
                // Fixed separation prevents pinch. Lead-in crosses recognition
                // threshold without adding it to the source displacement.
                let length = delta.0.hypot(delta.1);
                let x = ORIGIN + (LEAD_IN * delta.0 / length).round() as i32;
                let y = ORIGIN + (LEAD_IN * delta.1 / length).round() as i32;
                if !self.send(sink, x, y, 100, false) {
                    return (1.0, 1.0);
                }
            }
            let start = self.position;
            let target = (
                start.0.saturating_add((delta.0 + self.fraction.0).round() as i32),
                start.1.saturating_add((delta.1 + self.fraction.1).round() as i32),
            );
            self.fraction = (
                delta.0 + self.fraction.0 - f64::from(target.0 - start.0),
                delta.1 + self.fraction.1 - f64::from(target.1 - start.1),
            );
            if !self.send(sink, target.0, target.1, FRAME_MS, false) {
                return (1.0, 1.0);
            }
        }
        if phase == ScrollPhase::Ended {
            self.phase = Phase::AwaitMomentum;
        } else if phase == ScrollPhase::MomentumEnded {
            self.cancel(sink);
        }
        (0.0, 0.0)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    const SCALE: f64 = 192.0 / 2540.0;
    #[derive(Default)]
    struct Recorder {
        frames: Vec<Frame>,
        fail_at: Option<usize>,
        stops: usize,
    }
    impl Sink for Recorder {
        fn frame(&mut self, f: Frame) -> bool {
            if self.fail_at == Some(self.frames.len()) {
                return false;
            }
            self.frames.push(f);
            true
        }
        fn stop_inertia(&mut self) {
            self.stops += 1;
        }
    }
    fn start_fling(pan: &mut Pan, sink: &mut Recorder) {
        pan.scroll(sink, (0., -20.), SCALE, ScrollPhase::Began);
        pan.scroll(sink, (0., 0.), SCALE, ScrollPhase::Ended);
        pan.scroll(sink, (0., -10.), SCALE, ScrollPhase::MomentumBegan);
    }
    fn starts(frames: &[Frame]) -> usize {
        frames
            .iter()
            .enumerate()
            .filter(|(i, f)| !f.cancel && (*i == 0 || frames[*i - 1].cancel))
            .count()
    }
    #[test]
    fn continuous_fling_has_one_contact_sequence() {
        let (mut pan, mut sink) = (Pan::default(), Recorder::default());
        start_fling(&mut pan, &mut sink);
        pan.scroll(&mut sink, (0., -2.), SCALE, ScrollPhase::Momentum);
        pan.scroll(&mut sink, (0., 0.), SCALE, ScrollPhase::MomentumEnded);
        assert_eq!(starts(&sink.frames), 1);
        assert_eq!(sink.frames.iter().filter(|f| f.cancel).count(), 1);
        assert!(!pan.active());
    }
    #[test]
    fn continuous_fling_never_touches_down_during_momentum() {
        let (mut pan, mut sink) = (Pan::default(), Recorder::default());
        pan.scroll(&mut sink, (0., -20.), SCALE, ScrollPhase::Began);
        pan.scroll(&mut sink, (0., 0.), SCALE, ScrollPhase::Ended);
        let before = sink.frames.len();
        pan.scroll(&mut sink, (0., -10.), SCALE, ScrollPhase::MomentumBegan);
        assert_eq!(
            sink.frames.len(),
            before + 1,
            "momentum is one movement frame, not a new DOWN/lead-in"
        );
        assert!(sink.frames[before].y < sink.frames[before - 1].y);
    }
    #[test]
    fn continuous_fling_lifts_at_accumulated_end_coordinates() {
        let (mut pan, mut sink) = (Pan::default(), Recorder::default());
        start_fling(&mut pan, &mut sink);
        for amount in [5., 1., 0.2, 0.01] {
            pan.scroll(&mut sink, (0., -amount), SCALE, ScrollPhase::Momentum);
        }
        let end = *sink.frames.last().unwrap();
        pan.scroll(&mut sink, (0., 0.), SCALE, ScrollPhase::MomentumEnded);
        let lift = sink.frames.last().unwrap();
        assert!(lift.cancel);
        assert_eq!((lift.x, lift.y), (end.x, end.y));
        let expected = f64::from(ORIGIN)
            - LEAD_IN
            - [20., 10., 5., 1., 0.2, 0.01]
                .into_iter()
                .map(|d| inverse(d, SCALE))
                .sum::<f64>();
        assert!((f64::from(lift.y) - expected).abs() <= 0.5);
        assert!(sink.frames.windows(2).all(|w| w[1].y <= w[0].y));
    }
    #[test]
    fn continuous_fling_new_touch_stops_tail_without_retouch() {
        let (mut pan, mut sink) = (Pan::default(), Recorder::default());
        start_fling(&mut pan, &mut sink);
        let last = *sink.frames.last().unwrap();
        pan.scroll(&mut sink, (0., 0.), SCALE, ScrollPhase::Began);
        assert!(!pan.active());
        assert_eq!(sink.frames.last().unwrap().y, last.y);
        let stopped = sink.frames.len();
        for phase in [ScrollPhase::Momentum, ScrollPhase::MomentumEnded] {
            pan.scroll(&mut sink, (0., -2.), SCALE, phase);
        }
        assert_eq!(sink.frames.len(), stopped, "old momentum cannot revive after touch");
        assert!(!pan.active());
    }

    #[test]
    fn no_momentum_expires_promptly_and_late_tail_never_retouches() {
        let (mut pan, mut sink) = (Pan::default(), Recorder::default());
        pan.scroll(&mut sink, (0., -20.), SCALE, ScrollPhase::Began);
        pan.scroll(&mut sink, (0., 0.), SCALE, ScrollPhase::Ended);
        assert!(pan.awaiting_momentum());
        assert_eq!(pan.idle_timeout(), Duration::from_millis(32));
        let last = *sink.frames.last().unwrap();
        pan.expire(&mut sink);
        assert!(!pan.active());
        assert_eq!(sink.frames.last().unwrap().y, last.y);
        let count = sink.frames.len();
        assert_eq!(
            pan.scroll(&mut sink, (0., -5.), SCALE, ScrollPhase::MomentumBegan),
            (1., 1.)
        );
        assert_eq!(sink.frames.len(), count);
    }
    #[test]
    fn new_finger_scroll_ignores_old_momentum_end_and_cancel_never_revives() {
        let (mut pan, mut sink) = (Pan::default(), Recorder::default());
        start_fling(&mut pan, &mut sink);
        pan.scroll(&mut sink, (0., 0.), SCALE, ScrollPhase::Began);
        pan.scroll(&mut sink, (0., 10.), SCALE, ScrollPhase::Changed);
        let count = sink.frames.len();
        pan.scroll(&mut sink, (0., 0.), SCALE, ScrollPhase::MomentumEnded);
        assert_eq!(sink.frames.len(), count);
        assert!(pan.active());
        pan.scroll(&mut sink, (0., 0.), SCALE, ScrollPhase::Cancelled);
        let count = sink.frames.len();
        pan.scroll(&mut sink, (0., -10.), SCALE, ScrollPhase::MomentumBegan);
        assert_eq!(sink.frames.len(), count);
        assert!(!pan.active());
    }

    #[test]
    fn idle_fingers_can_resume_but_idle_momentum_never_retouches() {
        let (mut pan, mut sink) = (Pan::default(), Recorder::default());
        pan.scroll(&mut sink, (0., -20.), SCALE, ScrollPhase::Began);
        pan.expire(&mut sink);
        pan.scroll(&mut sink, (0., -5.), SCALE, ScrollPhase::Changed);
        assert!(pan.active());
        pan.scroll(&mut sink, (0., 0.), SCALE, ScrollPhase::Ended);
        pan.scroll(&mut sink, (0., -3.), SCALE, ScrollPhase::MomentumBegan);
        pan.expire(&mut sink);
        let count = sink.frames.len();
        assert_eq!(pan.scroll(&mut sink, (0., -1.), SCALE, ScrollPhase::Momentum), (1., 1.));
        assert_eq!(sink.frames.len(), count);
        assert!(!pan.active());
    }

    #[test]
    fn reversal_and_horizontal_preserve_displacement_and_fraction() {
        let (mut pan, mut sink) = (Pan::default(), Recorder::default());
        pan.scroll(&mut sink, (20.0, 0.0), SCALE, ScrollPhase::Began);
        let at = pan.position;
        for _ in 0..100 {
            pan.scroll(&mut sink, (-0.2, 0.0), SCALE, ScrollPhase::Changed);
        }
        assert!((f64::from(at.0 - pan.position.0) - 100.0 * inverse(0.2, SCALE)).abs() < 1.0);
        assert_eq!(at.1, pan.position.1);
    }
    #[test]
    fn failed_frame_returns_whole_event_without_replaying_previous_frames() {
        let (mut pan, mut sink) = (Pan::default(), Recorder::default());
        assert_eq!(
            pan.scroll(&mut sink, (0.0, -10.0), SCALE, ScrollPhase::Began),
            (0.0, 0.0)
        );
        sink.fail_at = Some(sink.frames.len());
        assert_eq!(
            pan.scroll(&mut sink, (0.0, -100.0), SCALE, ScrollPhase::Changed),
            (1.0, 1.0)
        );
        sink.fail_at = None;
        assert!(pan.cancel(&mut sink));
        assert!(!pan.active());
    }
    #[test]
    fn inverse_matches_measured_steady_velocities_and_handles_cap() {
        for speed in [0.01_f64, 8.0, 40.0, 100.0, 160.0, 200.0, 640.0] {
            let h = speed * f64::from(FRAME_MS) / 10.0;
            let pixels = h * SCALE * (1.0 + 4.0 * (speed / 200.0).min(1.0));
            for sensitivity in [0.5, 1.0, 2.0] {
                assert!((inverse(pixels * sensitivity, SCALE * sensitivity) - h).abs() < 1e-8);
                assert!((inverse(-pixels * sensitivity, SCALE * sensitivity) + h).abs() < 1e-8);
            }
        }
    }
}
