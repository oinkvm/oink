//! The Mac trackpad's pointer acceleration, scroll acceleration and scroll
//! momentum, for the control page's touchpad (user 2026-11-12: the phone feels
//! exactly like a Mac trackpad).
//!
//! Sources: IOHIDFamily (`IOHIDAcceleration.cpp/.hpp`,
//! `IOHIDAccelerationAlgorithm.cpp`, `IOHIDPointerScrollFilter.cpp`) for both
//! accelerators; MultitouchHID `MTTrackpadEventDispatcher` and WebKit's
//! `MomentumEventDispatcher.cpp` for momentum start, decay and end rules.
//! The curve parameters are the built-in MacBook trackpad's `HIDAccelCurves`
//! and `HIDScrollAccelCurves`.
//!
//! The phone's finger movement is converted to the trackpad's device units
//! (400 dpi counts) and report rate (120 Hz) first, so the same formulas apply
//! unchanged. The output units are the Mac's: points for the pointer (one
//! pointer unit is one logical point of the pointer base computer) and pixels
//! for scrolling.
//!
//! The multi-finger gestures (two-finger scroll / magnify / rotate, the Dock
//! swipes and pinch) are recognised from the raw touches in [`gestures`], the
//! same for iOS and Android.
use std::collections::VecDeque;

pub mod gestures;

const FIXED: f64 = 65536.0;
/// `FRAME_RATE` and `SCREEN_RESOLUTION` of `IOHIDAcceleration.hpp`; the
/// parametric curve's output scale `kCursorScale` is their ratio.
const FRAME_RATE: f64 = 67.0;
const CURSOR_SCALE: f64 = 96.0 / FRAME_RATE;
/// `HIDPointerResolution` and `HIDScrollResolution` (400 << 16).
const RESOLUTION: f64 = 400.0;
const COUNTS_PER_MM: f64 = RESOLUTION / 25.4;
/// `HIDPointerReportRate`: an interval shorter than one report counts as one.
pub const REPORT_PERIOD: f64 = 1.0 / 120.0;

/// System Settings › Trackpad › Tracking speed, its ten steps
/// (`com.apple.trackpad.scaling`); the phone's speed 1–10 is step 1–10.
pub const TRACKING_SPEEDS: [f64; 10] = [0.0, 0.125, 0.5, 0.6875, 0.875, 1.0, 1.5, 2.0, 2.5, 3.0];
/// macOS's default tracking speed, step 4 (0.6875).
pub const DEFAULT_STEP: u8 = 4;
/// The trackpad's scroll acceleration at the default scroll speed
/// (`HIDTrackpadScrollAcceleration` = 20480 / 65536).
pub const SCROLL_ACCELERATION: f64 = 20480.0 / FIXED;

/// One `HIDAccelCurves` entry in 16.16 fixed point: index, gain linear,
/// parabolic, cubic, quartic, tangent speed linear, tangent speed parabolic root.
type Row = [u32; 7];
const POINTER_CURVES: [Row; 10] = [
    [0, 65536, 0, 0, 0, 484966, 1376256],
    [8192, 64881, 32768, 5243, 0, 478413, 1310720],
    [32768, 64225, 43254, 6554, 0, 471859, 1245184],
    [45056, 62915, 54395, 7864, 0, 465306, 1179648],
    [57344, 61604, 65536, 9830, 0, 458752, 1114112],
    [65536, 60293, 75366, 11796, 0, 458752, 1048576],
    [98304, 58327, 85197, 13763, 0, 458752, 983040],
    [131072, 56361, 95027, 15729, 0, 458752, 917504],
    [163840, 54395, 108790, 18350, 0, 458752, 851968],
    [196608, 65536, 123208, 23593, 0, 458752, 786432],
];
const SCROLL_CURVES: [Row; 10] = [
    [0, 65536, 0, 0, 0, 393216, 786432],
    [8192, 62259, 39322, 0, 0, 406323, 786432],
    [32768, 58982, 58982, 0, 0, 419430, 786432],
    [45056, 55706, 78643, 0, 0, 432538, 786432],
    [57344, 52429, 91750, 0, 0, 445645, 786432],
    [65536, 49152, 104858, 0, 0, 458752, 786432],
    [98304, 45875, 117965, 0, 0, 471859, 786432],
    [131072, 42598, 131072, 0, 0, 484966, 786432],
    [163840, 39322, 144179, 0, 0, 498074, 786432],
    [196608, 36045, 157286, 0, 0, 511181, 786432],
];

/// `IOHIDParametricAcceleration`: a quartic polynomial up to the linear
/// tangent speed, its tangent line up to the parabolic-root speed, then
/// `sqrt(m·x + b)` continuing the tangent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Curve {
    gain: [f64; 4],
    tangent_linear: f64,
    tangent_root: f64,
}

impl Curve {
    fn row(row: &Row) -> (f64, Self) {
        let f = |i: usize| f64::from(row[i]) / FIXED;
        (
            f(0),
            Self {
                gain: [f(1), f(2), f(3), f(4)],
                tangent_linear: f(5),
                tangent_root: f(6),
            },
        )
    }

    /// The curve for `acceleration`: the last entry whose index is not above
    /// it, every parameter blended linearly towards the next entry.
    fn interpolated(table: &[Row], acceleration: f64) -> Self {
        let rows: Vec<(f64, Self)> = table.iter().map(Self::row).collect();
        let at = rows.iter().rposition(|(index, _)| *index <= acceleration).unwrap_or(0);
        let (index, low) = rows[at];
        let Some(&(next, high)) = rows.get(at + 1) else {
            return low;
        };
        let ratio = ((acceleration - index) / (next - index)).clamp(0.0, 1.0);
        let mix = |a: f64, b: f64| a + ratio * (b - a);
        Self {
            gain: std::array::from_fn(|i| mix(low.gain[i], high.gain[i])),
            tangent_linear: mix(low.tangent_linear, high.tangent_linear),
            tangent_root: mix(low.tangent_root, high.tangent_root),
        }
    }

    pub fn pointer(step: u8) -> Self {
        Self::interpolated(&POINTER_CURVES, TRACKING_SPEEDS[usize::from(step.clamp(1, 10)) - 1])
    }

    pub fn scroll() -> Self {
        Self::interpolated(&SCROLL_CURVES, SCROLL_ACCELERATION)
    }

    fn quartic(&self, x: f64) -> f64 {
        let [l, p, c, q] = self.gain;
        l * x + (p * x).powi(2) + (c * x).powi(3) + (q * x).powi(4)
    }

    fn slope(&self, x: f64) -> f64 {
        let [l, p, c, q] = self.gain;
        l + 2. * x * p.powi(2) + 3. * x.powi(2) * c.powi(3) + 4. * x.powi(3) * q.powi(4)
    }

    /// The curve at `x` (speed in the algorithm's units).
    fn eval(&self, x: f64) -> f64 {
        let (t1, t2) = (self.tangent_linear, self.tangent_root);
        if t1 != 0. {
            if x <= t1 {
                return self.quartic(x);
            }
            let m0 = self.slope(t1);
            let b0 = self.quartic(t1) - m0 * t1;
            if t2 == 0. || x <= t2 {
                return m0 * x + b0;
            }
            let y1 = m0 * t2 + b0;
            let m1 = 2. * y1 * m0;
            (m1 * x + y1 * y1 - m1 * t2).sqrt()
        } else if t2 != 0. && x > t2 {
            let y0 = self.quartic(t2);
            let m1 = self.slope(t2);
            (m1 * x + y0 * y0 - m1 * t2).sqrt()
        } else {
            self.quartic(x)
        }
    }

    /// `multiplier(v)`: the speed `v` (device counts per report) scaled by
    /// `resolution / FRAME_RATE`, through the curve, times `kCursorScale`.
    /// The accelerated movement is `value · multiplier(v) / v`.
    fn multiplier(&self, v: f64) -> f64 {
        self.eval(v / (RESOLUTION / FRAME_RATE)) * CURSOR_SCALE
    }
}

/// `IOHIDPointerAccelerator::accelerate` for one finger movement of
/// `(dx, dy)` millimetres over `dt` seconds: points on the target.
pub fn pointer_points(curve: &Curve, dx: f64, dy: f64, dt: f64) -> (f64, f64) {
    let (cx, cy) = (dx * COUNTS_PER_MM, dy * COUNTS_PER_MM);
    let counts = cx.hypot(cy);
    if counts == 0. || !counts.is_finite() {
        return (0., 0.);
    }
    // Speed per report: a longer interval lowers it, a shorter one counts as
    // one report period (never higher).
    let v = (counts * REPORT_PERIOD / dt.max(REPORT_PERIOD)).max(1. / FIXED);
    let m = curve.multiplier(v) / v;
    (cx * m, cy * m)
}

/// `SCROLL_EVENT_AVARAGE_LENGHT`, `SCROLL_CLEAR_THRESHOLD_MS`,
/// `SCROLL_EVENT_THRESHOLD_MS` and the `SCROLL_MULTIPLIER_*` velocity
/// polynomial (`IOHIDAcceleration.hpp`).
const SCROLL_HISTORY: usize = 9;
const SCROLL_CLEAR_MS: f64 = 500.0;
const SCROLL_EVENT_MS: f64 = 150.0;
const SCROLL_A: f64 = 2.0 / FIXED;
const SCROLL_B: f64 = 955.0 / FIXED;
const SCROLL_C: f64 = 98369.0 / FIXED;

/// `IOHIDScrollAccelerator` for one axis (the trackpad has one per axis).
#[derive(Debug, Default)]
struct ScrollAxis {
    /// (interval ms, |counts|), newest first.
    events: VecDeque<(f64, f64)>,
    last_us: Option<u64>,
    positive: Option<bool>,
}

impl ScrollAxis {
    /// One scroll report of `counts` at `time_us` → pixels.
    fn accelerate(&mut self, curve: &Curve, counts: f64, time_us: u64) -> f64 {
        if counts == 0. || !counts.is_finite() {
            return 0.;
        }
        let dt = self
            .last_us
            .map_or(f64::INFINITY, |last| time_us.saturating_sub(last) as f64 / 1000.);
        self.last_us = Some(time_us);
        let positive = counts > 0.;
        // A direction change or a pause restarts the average.
        if self.positive != Some(positive) || dt > SCROLL_CLEAR_MS {
            self.events.clear();
        }
        self.positive = Some(positive);
        self.events.push_front((dt, counts.abs()));
        self.events.truncate(SCROLL_HISTORY);
        let (mut total_dt, mut total, mut n) = (0., 0., 0.);
        for &(dt, value) in &self.events {
            total += value;
            n += 1.;
            if dt > SCROLL_EVENT_MS {
                total_dt += SCROLL_EVENT_MS;
                break;
            }
            total_dt += dt;
            if total_dt >= SCROLL_CLEAR_MS {
                break;
            }
        }
        // The trackpad has no scroll report rate: the rate factor is
        // FRAME_RATE / FRAME_RATE = 1.
        let t = (total_dt / n).clamp(1., SCROLL_EVENT_MS);
        let v = (((SCROLL_A * t - SCROLL_B) * t + SCROLL_C) * total / n).max(1. / FIXED);
        // The accelerator scales to wheel lines (× SCROLL_PIXEL_TO_WHEEL_SCALE,
        // 0.1) and the scroll event's point delta is ten times that.
        counts * curve.multiplier(v) / v
    }
}

/// The trackpad's momentum generator (MultitouchHID `MTTrackpadEventDispatcher`,
/// inspected on macOS 26.6.2): the lift's start velocity comes from
/// an asymmetric exponential filter of the finger movement; the curve is
/// generated in 1/60 s steps, each decayed by `alpha^(step / 8 ms)` (alpha
/// 0.975, towards 0.91 below 250 counts/s) and dispatched as two 120 Hz events.
pub const MOMENTUM_STEP: f64 = 1.0 / 60.0;
/// Momentum events go out at 120 Hz, as the recorded Mac trackpad's do.
pub const MOMENTUM_FRAME: f64 = 1.0 / 120.0;
const MOMENTUM_DECAY: f64 = 0.975;
const MOMENTUM_TAIL_DECAY: f64 = 0.91;
const MOMENTUM_TAIL_VELOCITY: f64 = 250.0;
const DECAY_BASIS: f64 = 0.008_000_000_38;
/// The curve ends once both axes are slower than this (counts per step):
/// the native tail velocity (250 counts/s), below which the generator
/// switches to its short integer tail.
const MOMENTUM_STOP: f64 = MOMENTUM_TAIL_VELOCITY * MOMENTUM_STEP;
/// At most 2.2 s of momentum (132 steps).
const MOMENTUM_MAX_STEPS: u32 = 132;
/// `updateMomentumMickeys`: the filter's time basis, the touch age over which
/// a rising movement blends from fast (0.25 per basis) to slow (0.8), and the
/// falling factor (0.85 per basis).
const FILTER_AGE: f64 = 0.4;
const FILTER_RISING_OLD: f64 = 0.800_000_011_9;
const FILTER_FALLING: f64 = 0.850_000_023_8;
/// `possiblyStartMomentum`: the filtered movement (truncated to whole counts)
/// must be at least 5 counts long; an axis more than 3x the other zeroes it;
/// momentum starts at most every 0.05 s; nothing starts when the fingers did
/// not move for 0.1 s before the lift (`momentumHistoryTimedOut`).
const START_MIN: f64 = 5.0;
const START_AXIS_RATIO: f64 = 3.0;
const START_MIN_INTERVAL_US: u64 = 50_000;
const HISTORY_TIMEOUT_US: u64 = 100_000;

fn momentum_decay(raw: (f64, f64)) -> f64 {
    let v0 = raw.0.hypot(raw.1) / MOMENTUM_STEP;
    let weak = ((MOMENTUM_TAIL_VELOCITY - v0) / MOMENTUM_TAIL_VELOCITY).max(0.);
    let alpha = MOMENTUM_DECAY - (MOMENTUM_DECAY - MOMENTUM_TAIL_DECAY) * weak;
    alpha.powf(MOMENTUM_STEP / DECAY_BASIS)
}

/// `MTChordIntegrating::momentumFilterAlpha`: the weight the filtered value
/// keeps over a report that took `dt` seconds, `age` seconds into the touch;
/// `rising` when the report's movement is faster than the filtered value. The
/// driver uses the same filter for the scroll momentum's start velocity and
/// for the Dock gestures' velocity ([`gestures`]).
fn momentum_alpha(dt: f64, age: f64, rising: bool) -> f64 {
    let k = dt / DECAY_BASIS;
    if rising {
        // A young touch follows at once, an older one more slowly.
        let r = (age / FILTER_AGE).clamp(0., 1.);
        let young = (-2. * k).exp2();
        young + (FILTER_RISING_OLD.powf(k) - young) * r
    } else {
        FILTER_FALLING.powf(k)
    }
}

/// One step of the start-velocity filter over a report's movement `d` (counts)
/// that took `dt` seconds, `age` seconds into the touch.
fn filter_step(previous: (f64, f64), d: (f64, f64), dt: f64, age: f64) -> (f64, f64) {
    let a = momentum_alpha(dt, age, previous.0.hypot(previous.1) < d.0.hypot(d.1));
    let mix = |p: f64, x: f64| {
        let f = a * p + (1. - a) * x;
        // A reversed axis starts again from zero.
        if f * x < 0. { 0. } else { f }
    };
    (mix(previous.0, d.0), mix(previous.1, d.1))
}

/// The momentum's first step (counts per 1/60 s) from the filtered movement
/// per report, or none.
fn momentum_start(filtered: (f64, f64)) -> Option<(f64, f64)> {
    let (mut x, mut y) = (filtered.0.trunc(), filtered.1.trunc());
    if x.abs() > START_AXIS_RATIO * y.abs() {
        y = 0.;
    } else if y.abs() > START_AXIS_RATIO * x.abs() {
        x = 0.;
    }
    if x.hypot(y) < START_MIN {
        return None;
    }
    // Per report period (the device interval) → per 1/60 s step.
    let scale = MOMENTUM_STEP / REPORT_PERIOD;
    Some((x * scale, y * scale))
}

/// The phases this phone sends for one touchpad scroll, as a Mac trackpad
/// source does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FingerPhase {
    Began,
    Changed,
    Ended,
    Cancelled,
}

/// One accelerated scroll event in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollStep {
    pub dx: f64,
    pub dy: f64,
    pub phase: oink_foundation::input::ScrollPhase,
}

/// A Mac trackpad's scrolling: finger phases accelerated like the trackpad's
/// scroll events, then momentum events after a lift.
#[derive(Debug)]
pub struct Scroller {
    curve: Curve,
    axes: [ScrollAxis; 2],
    fingers: bool,
    /// The start-velocity filter (counts per report).
    filtered: (f64, f64),
    /// When this touch's scroll started (the filter's touch age).
    touch_us: u64,
    /// The last report that moved.
    moved_us: Option<u64>,
    /// The last momentum start.
    started_us: Option<u64>,
    momentum: Option<Momentum>,
}

#[derive(Clone, Copy, Debug)]
struct Momentum {
    /// Counts of the current 1/60 s step.
    raw: (f64, f64),
    time_us: u64,
    /// Events still to send of the current step (two per step).
    halves: u8,
    /// Steps left before the 2.2 s limit.
    steps: u32,
    /// The current step was the last one.
    last: bool,
}

impl Default for Scroller {
    fn default() -> Self {
        Self {
            curve: Curve::scroll(),
            axes: Default::default(),
            fingers: false,
            filtered: (0., 0.),
            touch_us: 0,
            moved_us: None,
            started_us: None,
            momentum: None,
        }
    }
}

impl Scroller {
    pub fn momentum_active(&self) -> bool {
        self.momentum.is_some()
    }

    /// A finger touched the pad: running momentum ends (as on a Mac).
    pub fn touch(&mut self) -> Option<ScrollStep> {
        use oink_foundation::input::ScrollPhase;
        self.momentum.take().map(|_| ScrollStep {
            dx: 0.,
            dy: 0.,
            phase: ScrollPhase::MomentumEnded,
        })
    }

    /// Stop without telling anyone (the page closed or an emergency pause).
    pub fn reset(&mut self) {
        self.momentum = None;
        self.fingers = false;
        self.filtered = (0., 0.);
        self.moved_us = None;
    }

    /// One finger scroll event: `(dx, dy)` millimetres moved between
    /// `since_us` and `time_us`. A Began first ends running momentum; an Ended
    /// may start momentum (its first event is returned at once, the rest come
    /// from [`Self::momentum_frame`]).
    pub fn finger(&mut self, phase: FingerPhase, dx: f64, dy: f64, time_us: u64, since_us: u64) -> Vec<ScrollStep> {
        use oink_foundation::input::ScrollPhase;
        let mut out = Vec::new();
        if phase == FingerPhase::Began {
            out.extend(self.touch());
            self.fingers = true;
            self.filtered = (0., 0.);
            self.moved_us = None;
            self.touch_us = since_us;
        } else if !self.fingers {
            return out;
        }
        let counts = (dx * COUNTS_PER_MM, dy * COUNTS_PER_MM);
        let (px, py) = self.reports(counts, time_us, since_us);
        let wire = match phase {
            FingerPhase::Began => ScrollPhase::Began,
            FingerPhase::Changed => ScrollPhase::Changed,
            FingerPhase::Ended => ScrollPhase::Ended,
            FingerPhase::Cancelled => ScrollPhase::Cancelled,
        };
        out.push(ScrollStep {
            dx: px,
            dy: py,
            phase: wire,
        });
        match phase {
            FingerPhase::Ended => {
                self.fingers = false;
                if let Some(raw) = self.lift(time_us) {
                    self.started_us = Some(time_us);
                    self.momentum = Some(Momentum {
                        raw,
                        time_us,
                        halves: 0,
                        steps: MOMENTUM_MAX_STEPS,
                        last: false,
                    });
                    if let Some(first) = self.momentum_frame() {
                        out.push(ScrollStep {
                            phase: ScrollPhase::MomentumBegan,
                            ..first
                        });
                    }
                }
            }
            FingerPhase::Cancelled => self.fingers = false,
            _ => {}
        }
        out
    }

    /// The momentum's first step at a lift at `time_us`, or none.
    fn lift(&self, time_us: u64) -> Option<(f64, f64)> {
        let moved = self.moved_us?;
        if time_us.saturating_sub(moved) > HISTORY_TIMEOUT_US {
            return None;
        }
        if self
            .started_us
            .is_some_and(|started| time_us.saturating_sub(started) <= START_MIN_INTERVAL_US)
        {
            return None;
        }
        momentum_start(self.filtered)
    }

    /// The next momentum event (one per [`MOMENTUM_FRAME`]); MomentumEnded
    /// when it has run out, then `None`.
    pub fn momentum_frame(&mut self) -> Option<ScrollStep> {
        use oink_foundation::input::ScrollPhase;
        let mut m = self.momentum?;
        if m.halves == 0 {
            if m.last {
                self.momentum = None;
                return Some(ScrollStep {
                    dx: 0.,
                    dy: 0.,
                    phase: ScrollPhase::MomentumEnded,
                });
            }
            // The generator decays first, then emits the step; the step that
            // falls below the end still goes out.
            let decay = momentum_decay(m.raw);
            m.raw = (m.raw.0 * decay, m.raw.1 * decay);
            m.steps -= 1;
            m.last = (m.raw.0.abs() < MOMENTUM_STOP && m.raw.1.abs() < MOMENTUM_STOP) || m.steps == 0;
            m.halves = 2;
        }
        m.halves -= 1;
        m.time_us += (MOMENTUM_FRAME * 1e6) as u64;
        // Each event carries half the step; IOHIDPointerScrollFilter scales a
        // momentum event by its dispatch rate / 60 (= 2) before accelerating
        // and back after, so the accelerator sees the whole step.
        let rate = MOMENTUM_STEP / MOMENTUM_FRAME;
        let dx = self.axes[0].accelerate(&self.curve, m.raw.0 / 2. * rate, m.time_us) / rate;
        let dy = self.axes[1].accelerate(&self.curve, m.raw.1 / 2. * rate, m.time_us) / rate;
        self.momentum = Some(m);
        Some(ScrollStep {
            dx,
            dy,
            phase: ScrollPhase::Momentum,
        })
    }

    /// The trackpad reports at 120 Hz: a phone event covering a longer
    /// interval is accelerated (and filtered for the start velocity) as that
    /// many equal reports, so both see the trackpad's report stream.
    fn reports(&mut self, counts: (f64, f64), time_us: u64, since_us: u64) -> (f64, f64) {
        let span = time_us.saturating_sub(since_us) as f64 / 1e6;
        let n = if span * 1000. <= SCROLL_EVENT_MS {
            (span / REPORT_PERIOD)
                .round()
                .clamp(1., SCROLL_EVENT_MS / 1000. / REPORT_PERIOD)
        } else {
            1.
        };
        let step = (span / n * 1e6) as u64;
        let report = (counts.0 / n, counts.1 / n);
        let mut out = (0., 0.);
        for i in 1..=n as u64 {
            let at = if i == n as u64 { time_us } else { since_us + step * i };
            out.0 += self.axes[0].accelerate(&self.curve, report.0, at);
            out.1 += self.axes[1].accelerate(&self.curve, report.1, at);
            if report != (0., 0.) {
                let dt = (span / n).max(1e-4);
                let age = at.saturating_sub(self.touch_us) as f64 / 1e6;
                self.filtered = filter_step(self.filtered, report, dt, age);
                self.moved_us = Some(at);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::input::ScrollPhase;

    /// Points per millimetre at a steady finger speed (one movement per
    /// report period).
    fn gain(step: u8, mm_per_s: f64) -> f64 {
        let mm = mm_per_s * REPORT_PERIOD;
        pointer_points(&Curve::pointer(step), mm, 0., REPORT_PERIOD).0 / mm
    }

    /// `mac-curve-compare.out.txt`, second table (400 dpi, 120 Hz): points per
    /// millimetre for tracking speed 0 (step 1), 0.6875 (4), 1.0 (6), 1.5 (7)
    /// and 3.0 (10).
    #[test]
    fn pointer_gain_matches_the_macos_curves() {
        let table: [(f64, [f64; 5]); 11] = [
            (5., [3.78, 3.91, 4.03, 4.07, 5.25]),
            (10., [3.78, 4.20, 4.58, 4.77, 6.72]),
            (20., [3.78, 4.77, 5.68, 6.18, 9.69]),
            (40., [3.78, 5.92, 7.89, 9.01, 15.66]),
            (60., [3.78, 7.07, 10.11, 11.85, 21.70]),
            (100., [3.78, 9.38, 14.57, 17.57, 34.00]),
            (150., [3.78, 12.28, 20.20, 24.81, 49.74]),
            (200., [3.78, 15.20, 25.88, 32.12, 65.92]),
            (300., [3.78, 21.08, 37.40, 47.01, 99.54]),
            (450., [3.78, 27.85, 50.41, 63.86, 138.32]),
            (600., [3.78, 31.28, 56.98, 72.38, 156.70]),
        ];
        for (speed, expected) in table {
            for (step, want) in [1, 4, 6, 7, 10].into_iter().zip(expected) {
                let got = gain(step, speed);
                assert!(
                    (got - want).abs() < 0.006,
                    "step {step} at {speed} mm/s: {got} != {want}"
                );
            }
        }
    }

    #[test]
    fn the_steps_are_the_tracking_speed_curves_and_default_is_step_4() {
        assert_eq!(DEFAULT_STEP, 4);
        assert_eq!(TRACKING_SPEEDS[usize::from(DEFAULT_STEP) - 1], 0.6875);
        for (i, row) in POINTER_CURVES.iter().enumerate() {
            assert_eq!(f64::from(row[0]) / FIXED, TRACKING_SPEEDS[i]);
            assert_eq!(
                Curve::pointer(i as u8 + 1),
                Curve::row(row).1,
                "a step is exactly its curve"
            );
        }
        // Out of range steps clamp.
        assert_eq!(Curve::pointer(0), Curve::pointer(1));
        assert_eq!(Curve::pointer(11), Curve::pointer(10));
    }

    #[test]
    fn curves_between_entries_blend_every_parameter() {
        // The default scroll acceleration 0.3125 lies halfway between 0.125
        // and 0.5.
        let c = Curve::scroll();
        let (_, low) = Curve::row(&SCROLL_CURVES[1]);
        let (_, high) = Curve::row(&SCROLL_CURVES[2]);
        assert!((c.gain[0] - (low.gain[0] + high.gain[0]) / 2.).abs() < 1e-12);
        assert!((c.gain[1] - (low.gain[1] + high.gain[1]) / 2.).abs() < 1e-12);
        assert!((c.tangent_linear - (low.tangent_linear + high.tangent_linear) / 2.).abs() < 1e-12);
    }

    #[test]
    fn the_curve_is_continuous_at_both_tangent_points() {
        for step in 1..=10 {
            let c = Curve::pointer(step);
            for t in [c.tangent_linear, c.tangent_root] {
                let (a, b) = (c.eval(t - 1e-9), c.eval(t + 1e-9));
                assert!((a - b).abs() < 1e-6, "step {step} at {t}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn pointer_speed_is_per_report_period() {
        let c = Curve::pointer(4);
        // The same 1 mm in one report or in a few microseconds: one period.
        assert_eq!(
            pointer_points(&c, 1., 0., 1e-6),
            pointer_points(&c, 1., 0., REPORT_PERIOD)
        );
        // Over a longer interval it is slower, so it gains less per mm.
        assert!(pointer_points(&c, 1., 0., 0.1).0 < pointer_points(&c, 1., 0., REPORT_PERIOD).0);
        // Direction is kept and nothing in gives nothing out.
        let (x, y) = pointer_points(&c, -0.3, 0.4, REPORT_PERIOD);
        assert!(x < 0. && y > 0. && (x / y + 0.75).abs() < 1e-12);
        assert_eq!(pointer_points(&c, 0., 0., REPORT_PERIOD), (0., 0.));
    }

    /// Steady scrolling at `mm_per_s` for 200 ms in phone events of `period`
    /// seconds: pixels per millimetre of the last event.
    fn scroll_gain(mm_per_s: f64, period: f64) -> f64 {
        let mut s = Scroller::default();
        let mut t = 1_000_000u64;
        let step_us = (period * 1e6) as u64;
        let mm = mm_per_s * period;
        let mut last = 0.;
        for i in 0..(0.2 / period) as usize {
            let phase = if i == 0 {
                FingerPhase::Began
            } else {
                FingerPhase::Changed
            };
            let out = s.finger(phase, 0., mm, t + step_us, t);
            last = out.last().unwrap().dy / mm;
            t += step_us;
        }
        last
    }

    #[test]
    fn scrolling_accelerates_with_finger_speed() {
        let slow = scroll_gain(1., REPORT_PERIOD);
        let fast = scroll_gain(200., REPORT_PERIOD);
        // Slowly the curve is nearly linear: 96 px per inch times its linear gain.
        let linear = Curve::scroll().gain[0] * 96. / 25.4;
        assert!((slow / linear - 1.).abs() < 0.05, "{slow} vs {linear}");
        assert!(fast > 2. * slow, "{fast} vs {slow}");
    }

    #[test]
    fn a_60_hz_phone_scrolls_like_the_120_hz_trackpad() {
        for speed in [10., 50., 150.] {
            let trackpad = scroll_gain(speed, REPORT_PERIOD);
            let phone = scroll_gain(speed, 2. * REPORT_PERIOD);
            assert!((phone / trackpad - 1.).abs() < 0.02, "{speed}: {phone} vs {trackpad}");
        }
    }

    #[test]
    fn scroll_phases_follow_the_fingers_then_momentum_runs_out() {
        let mut s = Scroller::default();
        let mut t = 8_333u64;
        let first = s.finger(FingerPhase::Began, 0., 1., 8_333, 0);
        assert_eq!(first.iter().map(|e| e.phase).collect::<Vec<_>>(), [ScrollPhase::Began]);
        for _ in 0..10 {
            let out = s.finger(FingerPhase::Changed, 0., 1., t + 16_667, t);
            assert_eq!(out[0].phase, ScrollPhase::Changed);
            assert!(out[0].dy > 0.);
            t += 16_667;
        }
        // 1 mm per 1/60 s = 60 mm/s: momentum starts with the lift.
        let lift = s.finger(FingerPhase::Ended, 0., 0., t + 5_000, t);
        assert_eq!(
            lift.iter().map(|e| e.phase).collect::<Vec<_>>(),
            [ScrollPhase::Ended, ScrollPhase::MomentumBegan]
        );
        assert!(lift[1].dy > 0.);
        let mut events = 0;
        let mut previous = f64::INFINITY;
        loop {
            let step = s.momentum_frame().expect("events until MomentumEnded");
            if step.phase == ScrollPhase::MomentumEnded {
                break;
            }
            assert_eq!(step.phase, ScrollPhase::Momentum);
            assert!(step.dy > 0.);
            // The first events may still rise while the accelerator's average
            // moves from the finger to the momentum (as recorded on a Mac).
            if events > 10 {
                assert!(step.dy <= previous + 1e-9, "momentum only slows down");
            }
            previous = step.dy;
            events += 1;
            assert!(events < 2 * MOMENTUM_MAX_STEPS as usize);
        }
        assert!(events > 10, "a fling glides for a while: {events} events");
        assert_eq!(s.momentum_frame(), None);
    }

    #[test]
    fn momentum_decays_like_the_system_curve() {
        // Fast: 0.975 per 8 ms; slow (below 250 counts/s): towards 0.91.
        let fast = momentum_decay((100., 0.));
        assert!((fast - 0.975f64.powf(MOMENTUM_STEP / DECAY_BASIS)).abs() < 1e-12);
        let resting = momentum_decay((0., 0.));
        assert!((resting - 0.91f64.powf(MOMENTUM_STEP / DECAY_BASIS)).abs() < 1e-12);
    }

    #[test]
    fn the_start_filter_is_asymmetric_and_resets_reversed_axes() {
        let close = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < 1e-5 && (a.1 - b.1).abs() < 1e-5;
        // Rising in a young touch: 0.25 of the old value per 8 ms.
        assert!(close(filter_step((0., 0.), (10., 0.), 0.008, 0.), (7.5, 0.)));
        // Rising after 0.4 s of touch: 0.8 of the old value.
        assert!(close(filter_step((0., 0.), (10., 0.), 0.008, 0.4), (2., 0.)));
        // Falling: 0.85 of the old value per 8 ms, whatever the age.
        assert!(close(filter_step((10., 0.), (0., 0.), 0.008, 0.), (8.5, 0.)));
        assert!(close(
            filter_step((10., 0.), (0., 0.), 0.016, 1.),
            (10. * 0.85f64.powf(2.), 0.)
        ));
        // An axis whose filtered value disagrees with the movement starts from zero.
        assert!(close(filter_step((10., 0.), (-30., 5.), 0.008, 1.), (0., 1.)));
    }

    #[test]
    fn momentum_starts_from_at_least_5_counts_along_one_axis() {
        assert_eq!(momentum_start((4.9, 0.)), None, "truncated to 4");
        assert_eq!(momentum_start((3., 3.9)), None, "length below 5");
        // An axis more than 3x the other zeroes it; the rest scales to 1/60 s.
        assert_eq!(momentum_start((5.9, 1.2)), Some((10., 0.)));
        assert_eq!(momentum_start((-7.9, 0.5)), Some((-14., 0.)));
        assert_eq!(momentum_start((6., 3.)), Some((12., 6.)));
    }

    /// A one-report fling of `counts` per report, lifted `rest_us` after its
    /// last movement; whether momentum started.
    fn fling(s: &mut Scroller, start_us: u64, mm: f64, rest_us: u64) -> bool {
        s.finger(FingerPhase::Began, 0., mm, start_us + 8_333, start_us);
        s.finger(FingerPhase::Changed, 0., mm, start_us + 16_667, start_us + 8_333);
        let out = s.finger(
            FingerPhase::Ended,
            0.,
            0.,
            start_us + 16_667 + rest_us,
            start_us + 16_667,
        );
        out.iter().any(|e| e.phase == ScrollPhase::MomentumBegan)
    }

    #[test]
    fn no_momentum_after_a_rest_or_right_after_another_start() {
        // History timeout 0.1 s.
        assert!(fling(&mut Scroller::default(), 0, 2., 99_000));
        assert!(!fling(&mut Scroller::default(), 0, 2., 101_000));
        // Starts at most every 0.05 s.
        let mut s = Scroller::default();
        assert!(fling(&mut s, 0, 2., 1_000));
        let lift = 17_667;
        assert!(!fling(&mut s, lift, 2., 1_000), "the next lift 18 ms later");
        assert!(fling(&mut s, lift + 40_000, 2., 1_000), "58 ms after the last start");
    }

    #[test]
    fn momentum_is_two_events_per_step_and_ends_at_the_tail_velocity_or_after_2_2_s() {
        let run = |raw: (f64, f64)| {
            let mut s = Scroller {
                momentum: Some(Momentum {
                    raw,
                    time_us: 0,
                    halves: 0,
                    steps: MOMENTUM_MAX_STEPS,
                    last: false,
                }),
                ..Scroller::default()
            };
            let mut events = 0;
            while let Some(step) = s.momentum_frame() {
                if step.phase == ScrollPhase::MomentumEnded {
                    break;
                }
                events += 1;
            }
            events
        };
        // 5 counts/step decays 4.74, 4.50, 4.27, 4.05: the step below 250 counts/s
        // (4.17 per step) is the last, two events each.
        assert_eq!(run((0., 5.)), 8);
        // A huge fling stops at 2.2 s: 132 steps.
        assert_eq!(run((0., 10_000.)), 2 * MOMENTUM_MAX_STEPS as usize);
    }

    #[test]
    fn a_resting_lift_or_cancel_has_no_momentum_and_a_touch_ends_it() {
        let mut s = Scroller::default();
        s.finger(FingerPhase::Began, 0., 2., 8_333, 0);
        s.finger(FingerPhase::Changed, 0., 2., 16_667, 8_333);
        // Lifted 120 ms after the fingers stopped: no momentum.
        let out = s.finger(FingerPhase::Ended, 0., 0., 136_667, 16_667);
        assert_eq!(out.iter().map(|e| e.phase).collect::<Vec<_>>(), [ScrollPhase::Ended]);
        assert!(!s.momentum_active());

        s.finger(FingerPhase::Began, 0., 2., 1_008_333, 1_000_000);
        let out = s.finger(FingerPhase::Cancelled, 0., 0., 1_016_667, 1_008_333);
        assert_eq!(
            out.iter().map(|e| e.phase).collect::<Vec<_>>(),
            [ScrollPhase::Cancelled]
        );
        assert!(!s.momentum_active());

        s.finger(FingerPhase::Began, 0., 2., 2_008_333, 2_000_000);
        s.finger(FingerPhase::Changed, 0., 2., 2_016_667, 2_008_333);
        s.finger(FingerPhase::Ended, 0., 0., 2_020_000, 2_016_667);
        assert!(s.momentum_active());
        assert_eq!(s.touch().map(|e| e.phase), Some(ScrollPhase::MomentumEnded));
        assert!(!s.momentum_active());
        assert_eq!(s.touch(), None);
        // A new scroll also ends momentum first.
        s.finger(FingerPhase::Began, 0., 2., 3_008_333, 3_000_000);
        s.finger(FingerPhase::Changed, 0., 2., 3_016_667, 3_008_333);
        s.finger(FingerPhase::Ended, 0., 0., 3_020_000, 3_016_667);
        let again = s.finger(FingerPhase::Began, 0., 1., 3_100_000, 3_090_000);
        assert_eq!(
            again.iter().map(|e| e.phase).collect::<Vec<_>>(),
            [ScrollPhase::MomentumEnded, ScrollPhase::Began]
        );
    }

    #[test]
    fn changes_without_a_began_are_dropped() {
        let mut s = Scroller::default();
        assert!(s.finger(FingerPhase::Changed, 0., 1., 8_333, 0).is_empty());
        assert!(s.finger(FingerPhase::Ended, 0., 0., 16_667, 8_333).is_empty());
    }
}
