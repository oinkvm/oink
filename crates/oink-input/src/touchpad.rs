//! Source-owned scrolling translated to two parallel synthetic contacts.
//!
//! The Windows recognizer accelerates contact velocity and has a pan threshold.
//! Invert the contact-speed gain before injection, retaining the source's curve.
//! The model was checked against native output on Windows 11 build 26200.9457;
//! the recognizer is not a documented cross-version contract.
use oink_foundation::input::ScrollPhase;

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
}

impl Default for Pan {
    fn default() -> Self {
        Self {
            position: (ORIGIN, ORIGIN),
            fraction: (0.0, 0.0),
            tick: 1,
            active: false,
        }
    }
}

impl Pan {
    pub fn active(&self) -> bool {
        self.active
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

    /// Return unsubmitted fractions of the source event for exact wheel fallback.
    /// A successful native event returns (0,0). Priming never consumes source data.
    pub fn scroll(
        &mut self,
        sink: &mut impl Sink,
        pixels: (f64, f64),
        pixels_per_himetric: f64,
        phase: ScrollPhase,
    ) -> (f64, f64) {
        if phase == ScrollPhase::Began && !self.cancel(sink) {
            return (1.0, 1.0);
        }
        if !pixels.0.is_finite() || !pixels.1.is_finite() {
            self.cancel(sink);
            return (0.0, 0.0);
        }
        if !pixels_per_himetric.is_finite() || pixels_per_himetric <= 0.0 {
            self.cancel(sink);
            return (1.0, 1.0);
        }
        let delta = (
            inverse(pixels.0, pixels_per_himetric),
            inverse(pixels.1, pixels_per_himetric),
        );
        if delta.0 != 0.0 || delta.1 != 0.0 {
            // Recenter only at a device boundary; no unbounded integer conversion.
            if delta.0.abs().max(delta.1.abs()) > f64::from(LIMIT) {
                self.cancel(sink);
                return (1.0, 1.0);
            }
            if ((f64::from(self.position.0 - ORIGIN) + delta.0).abs() > f64::from(LIMIT)
                || (f64::from(self.position.1 - ORIGIN) + delta.1).abs() > f64::from(LIMIT))
                && !self.cancel(sink)
            {
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
        if matches!(
            phase,
            ScrollPhase::Ended | ScrollPhase::Cancelled | ScrollPhase::MomentumEnded
        ) {
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
    #[test]
    fn lift_cancels_and_source_momentum_starts_fresh_without_generated_tail() {
        let (mut pan, mut sink) = (Pan::default(), Recorder::default());
        pan.scroll(&mut sink, (0.0, -20.0), SCALE, ScrollPhase::Began);
        assert!(pan.active());
        pan.scroll(&mut sink, (0.0, 0.0), SCALE, ScrollPhase::Ended);
        assert!(sink.frames.last().unwrap().cancel);
        assert!(!pan.active());
        pan.scroll(&mut sink, (0.0, -10.0), SCALE, ScrollPhase::MomentumBegan);
        assert!(pan.active());
        pan.scroll(&mut sink, (0.0, 0.0), SCALE, ScrollPhase::MomentumEnded);
        assert!(!pan.active());
        assert!(sink.frames.last().unwrap().cancel);
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
