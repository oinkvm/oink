//! The control page touchpad's multi-finger recognition, shared by iOS and
//! Android (user 2026-11-12: both phones behave exactly like a Mac trackpad,
//! identically). The phone feeds every touch frame (all fingers in
//! millimetres, after each touch event); this decides two-finger scroll /
//! magnify / rotate and the Dock gestures (three / four finger swipes, four /
//! five finger pinch), integrates their progress and velocity, and ends them
//! the way the Mac trackpad driver does. The page keeps one-finger input
//! (pointer, taps, tap-to-drag, edge lock), two-finger taps and the
//! "单指划动 = 滚动" swap; it tells each frame whether it leaves the touch's
//! fingers to this recognizer, and learns from the [`Claim`] whether this
//! recognizer took the touch.
//!
//! Sources: macOS 26.6.2 MultitouchHID, checked against native gesture
//! segments. Recovered and used here:
//! - the Dock configurations: horizontal / vertical swipes integrate the
//!   fingers' movement with a 2 mm start threshold and a 65 mm completion
//!   scale, the pinch ("Polar Swipe") with 3 mm and 32 mm; the prediction
//!   time is 0.5 s (`createDefaultMotionSensitivitiesDictionary`). Here they
//!   apply to the phone surface's millimetres directly; the driver's own
//!   motion acceleration before the integration (`AccelThresh` 15,
//!   `AccelPower` 0.2) is not recovered and not applied;
//! - the start (`fireFluidGesture`): the first slide whose integral passes its
//!   start threshold fires, and its integral restarts from that report's
//!   movement (Began is not zero and not the whole movement so far);
//! - the velocity (`updateFluidMomentum` / `momentumFilterAlpha`): an
//!   asymmetric exponential filter of the movement per report, the same filter
//!   as the scroll momentum's start velocity (`momentum_alpha`);
//! - the peak (`updateFluidPeak`) and the end (`hasSufficientFluidMomentum`
//!   for a Dock action): with `p` the progress, `v` the filtered velocity and
//!   `q = p + 0.5·v`, the gesture completes (Ended) only when `p·q > 0`,
//!   `|q| > 0.5` and `|q| ≥ 0.8·|peak|`; otherwise it is Cancelled — also on a
//!   normal lift. Replaying the recorded segments' progress through this
//!   filter and rule reproduces all 26 recorded Ended / Cancelled outcomes;
//! - a finger added to a recognised gesture cancels it and the rest of the
//!   touch does nothing.
//!
//! Not recovered, so chosen here (each marked at its constant): the chord
//! time-in (`hasChordStabilized` waits n × 10 ms in its regular branch, n
//! taken as the finger count), which lift ends a gesture (the first one, with
//! the integrated state before it, so no jump from the remaining fingers),
//! the filter's touch age (from the current finger set), the two-finger
//! decision (the page's former 1.5 mm rule: the largest of travel, pinch and
//! arc), the measures themselves (centroid, mean distance from the centroid),
//! the integration's decay before the start, and the discrete motion pulses
//! (field 115) — none are generated. Magnify and rotate stay one at a time
//! (the Mac runs them together, but their thresholds are not recovered).
use super::{FingerPhase, momentum_alpha};
use oink_foundation::input::{Gesture, GesturePhase, SwipeAxis};

/// One finger on the pad, millimetres from its top left (y down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Finger {
    pub id: u64,
    pub x: f64,
    pub y: f64,
}

/// What this recognizer does with the current touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// Nothing (yet): the touch is the page's.
    Unclaimed,
    /// A two-finger scroll.
    Scroll,
    /// A gesture, or a touch whose gesture is over (its remaining fingers do
    /// nothing).
    Gesture,
}

/// What a touch frame produced, in order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Recognized {
    Gesture(Gesture),
    /// A two-finger scroll event: the fingers' centroid moved `dx`, `dy`
    /// millimetres (finger direction) between `since_us` and `time_us`.
    Scroll {
        phase: FingerPhase,
        dx: f64,
        dy: f64,
        time_us: u64,
        since_us: u64,
    },
}

/// A trackpad has five contacts; more makes the touch do nothing.
const MAX_FINGERS: usize = 5;
/// Chord time-in per finger: a new finger set moves nothing until it has
/// stayed n × 10 ms (MultitouchHID `hasChordStabilized`, its regular branch;
/// n is taken to be the finger count — not recovered for certain).
const CHORD_TIME_IN_US: u64 = 10_000;
/// Two fingers decide scroll / magnify / rotate once one of their measures
/// passes this (the page's former rule, not the driver's).
const TWO_FINGER_SLOP: f64 = 1.5;

/// One fluid Dock configuration (MultitouchHID default motion dictionary).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Fluid {
    /// `FluidAnimationStartThreshold_mm`: the integral that fires the gesture.
    start: f64,
    /// `FluidSwipeCompleteThreshold_mm`: millimetres per progress 1.
    scale: f64,
    /// `FluidSwipeInertiaTime_s`: how far ahead the end predicts.
    inertia: f64,
}

/// "Swipe": Fluid Dock Horizontal / Vertical.
const SWIPE: Fluid = Fluid {
    start: 2.0,
    scale: 65.0,
    inertia: 0.5,
};
/// "Polar Swipe": Fluid Dock Scale (the pinch).
const POLAR_SWIPE: Fluid = Fluid {
    start: 3.0,
    scale: 32.0,
    inertia: 0.5,
};
/// `hasSufficientFluidMomentum`: the predicted progress must pass half way…
const COMPLETE: f64 = 0.5;
/// …and keep at least this share of the peak (float 0.8 in the driver).
const PEAK_KEEP: f64 = 0.800_000_011_9;

/// The degrees of freedom of the Dock gestures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dof {
    Horizontal,
    Vertical,
    Scale,
}

impl Dof {
    const ALL: [Self; 3] = [Self::Horizontal, Self::Vertical, Self::Scale];

    fn axis(self) -> SwipeAxis {
        match self {
            Self::Horizontal => SwipeAxis::Horizontal,
            Self::Vertical => SwipeAxis::Vertical,
            Self::Scale => SwipeAxis::Pinch,
        }
    }

    fn fluid(self) -> Fluid {
        match self {
            Self::Horizontal | Self::Vertical => SWIPE,
            Self::Scale => POLAR_SWIPE,
        }
    }

    /// The finger counts this belongs to, macOS's defaults: three or four
    /// fingers swipe, four or five fingers pinch.
    fn in_chord(self, fingers: usize) -> bool {
        match self {
            Self::Horizontal | Self::Vertical => fingers == 3 || fingers == 4,
            Self::Scale => fingers == 4 || fingers == 5,
        }
    }

    /// Its coordinate (mm): the centroid's x or y, or the fingers' mean
    /// distance from the centroid.
    fn coordinate(self, fingers: &[Finger]) -> f64 {
        match self {
            Self::Horizontal => centroid(fingers).0,
            Self::Vertical => centroid(fingers).1,
            Self::Scale => spread(fingers),
        }
    }

    /// From the pad's direction to the wire's (`Gesture::DockSwipe`):
    /// horizontal positive = fingers moved left, vertical negative = fingers
    /// moved up, pinch negative = fingers closed in.
    fn wire(self) -> f64 {
        match self {
            Self::Horizontal => -1.0,
            Self::Vertical | Self::Scale => 1.0,
        }
    }
}

/// One fluid slide (MTSlideGesture): the integral, its peak and the filtered
/// velocity.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Slide {
    /// Millimetres integrated (since it fired, once it has).
    s: f64,
    /// `updateFluidPeak`: the integral's extreme, replaced when the integral
    /// changes sign.
    peak: f64,
    /// Progress per second.
    v: f64,
    /// Movement of reports that took no time, for the next report's velocity.
    pending: f64,
}

impl Slide {
    /// One report: `delta` mm over `dt` seconds, `age` seconds into the finger
    /// set (`integrateFluidGesture`: integral, peak, then momentum).
    fn integrate(&mut self, delta: f64, dt: f64, age: f64, scale: f64) {
        self.s += delta;
        if self.peak * self.s < 0.0 || self.s.abs() > self.peak.abs() {
            self.peak = self.s;
        }
        self.pending += delta;
        if dt > 0.0 {
            let u = self.pending / scale / dt;
            self.pending = 0.0;
            let a = momentum_alpha(dt, age, u.abs() > self.v.abs());
            self.v = a * self.v + (1.0 - a) * u;
        }
    }

    /// `hasSufficientFluidMomentum` for a Dock action.
    fn completes(&self, fluid: Fluid) -> bool {
        let p = self.s / fluid.scale;
        let q = p + fluid.inertia * self.v;
        p * q > 0.0 && q.abs() > COMPLETE && q.abs() >= PEAK_KEEP * (self.peak / fluid.scale).abs()
    }
}

/// The Dock gesture event of `slide`. Began / Changed carry no velocity (as the
/// driver's events); the end carries the filtered one.
fn dock(dof: Dof, phase: GesturePhase, slide: &Slide) -> Gesture {
    Gesture::DockSwipe {
        axis: dof.axis(),
        phase,
        progress: dof.wire() * slide.s / dof.fluid().scale,
        velocity: if phase.is_end() { dof.wire() * slide.v } else { 0.0 },
    }
}

/// The end of a running Dock gesture after the fingers rested `dt` seconds:
/// Ended when it completes, otherwise (or when `cancel`) Cancelled.
fn end_fluid(dof: Dof, mut slide: Slide, dt: f64, age: f64, cancel: bool) -> Gesture {
    let fluid = dof.fluid();
    // No report since the last movement: the velocity decays over the rest.
    slide.integrate(0.0, dt, age, fluid.scale);
    let phase = if !cancel && slide.completes(fluid) {
        GesturePhase::Ended
    } else {
        GesturePhase::Cancelled
    };
    dock(dof, phase, &slide)
}

fn seconds(us: u64) -> f64 {
    us as f64 / 1e6
}

fn centroid(fingers: &[Finger]) -> (f64, f64) {
    let n = fingers.len().max(1) as f64;
    let (x, y) = fingers.iter().fold((0.0, 0.0), |(x, y), f| (x + f.x, y + f.y));
    (x / n, y / n)
}

/// Mean distance of the fingers from their centroid.
fn spread(fingers: &[Finger]) -> f64 {
    let (cx, cy) = centroid(fingers);
    let n = fingers.len().max(1) as f64;
    fingers.iter().map(|f| (f.x - cx).hypot(f.y - cy)).sum::<f64>() / n
}

/// Two fingers (ordered by id): their distance and their angle in degrees,
/// counter-clockwise with y down.
fn pair(fingers: &[Finger]) -> Option<(f64, f64)> {
    let [a, b] = fingers else {
        return None;
    };
    let (a, b) = if a.id <= b.id { (a, b) } else { (b, a) };
    Some(((b.x - a.x).hypot(b.y - a.y), (a.y - b.y).atan2(b.x - a.x).to_degrees()))
}

/// An angle difference in (-180, 180].
fn turn(degrees: f64) -> f64 {
    let d = degrees % 360.0;
    if d > 180.0 {
        d - 360.0
    } else if d <= -180.0 {
        d + 360.0
    } else {
        d
    }
}

fn same_fingers(a: &[Finger], b: &[Finger]) -> bool {
    a.len() == b.len() && a.iter().all(|f| b.iter().any(|g| g.id == f.id))
}

/// A timed-in finger set that has not started anything yet.
#[derive(Clone, Debug)]
struct Base {
    fingers: Vec<Finger>,
    time_us: u64,
    /// Each degree of freedom's coordinate at the last report.
    coords: [f64; 3],
    /// Each degree of freedom's slide since the time-in.
    slides: [Slide; 3],
}

impl Base {
    fn new(fingers: &[Finger], time_us: u64) -> Self {
        Self {
            fingers: fingers.to_vec(),
            time_us,
            coords: Dof::ALL.map(|dof| dof.coordinate(fingers)),
            slides: [Slide::default(); 3],
        }
    }

    /// One report of the same fingers: the slides integrate; what starts, if
    /// anything (its first event goes to `out`).
    fn step(&mut self, fingers: &[Finger], time_us: u64, dt: f64, age: f64, out: &mut Vec<Recognized>) -> Option<Mode> {
        let mut deltas = [0.0; 3];
        for (i, dof) in Dof::ALL.into_iter().enumerate() {
            let c = dof.coordinate(fingers);
            deltas[i] = c - self.coords[i];
            self.coords[i] = c;
            self.slides[i].integrate(deltas[i], dt, age, dof.fluid().scale);
        }
        let n = fingers.len();
        if n == 2 {
            return self.two_fingers(fingers, time_us, out);
        }
        // The first slide of this chord past its start threshold fires; when
        // several pass in one report, the one furthest past.
        let past = |i: usize, dof: Dof| self.slides[i].s.abs() / dof.fluid().start;
        let (i, dof) = Dof::ALL
            .into_iter()
            .enumerate()
            .filter(|&(i, dof)| dof.in_chord(n) && past(i, dof) > 1.0)
            .max_by(|a, b| past(a.0, a.1).total_cmp(&past(b.0, b.1)))?;
        let mut slide = self.slides[i];
        // fireFluidGesture: the integral restarts from this report's movement.
        slide.s = deltas[i];
        out.push(Recognized::Gesture(dock(dof, GesturePhase::Began, &slide)));
        Some(Mode::Fluid {
            dof,
            slide,
            coord: self.coords[i],
        })
    }

    /// Two fingers: past the slop, the largest of pinch (distance), arc (the
    /// turn at the fingers) and travel (centroid) decides magnify, rotate or
    /// scroll; the first event carries the change from the time-in.
    fn two_fingers(&self, fingers: &[Finger], time_us: u64, out: &mut Vec<Recognized>) -> Option<Mode> {
        let ((span, angle), (span0, angle0)) = (pair(fingers)?, pair(&self.fingers)?);
        let (c, c0) = (centroid(fingers), centroid(&self.fingers));
        let travel = (c.0 - c0.0).hypot(c.1 - c0.1);
        let pinch = (span - span0).abs();
        let arc = turn(angle - angle0).abs().to_radians() * span0 / 2.0;
        if travel.max(pinch).max(arc) <= TWO_FINGER_SLOP {
            return None;
        }
        Some(if pinch >= travel.max(arc) && span0 > 0.0 {
            out.push(Recognized::Gesture(Gesture::Magnify {
                phase: GesturePhase::Began,
                amount: span / span0 - 1.0,
            }));
            Mode::Magnify { span }
        } else if arc > travel {
            out.push(Recognized::Gesture(Gesture::Rotate {
                phase: GesturePhase::Began,
                degrees: turn(angle - angle0),
            }));
            Mode::Rotate { angle }
        } else {
            out.push(Recognized::Scroll {
                phase: FingerPhase::Began,
                dx: c.0 - c0.0,
                dy: c.1 - c0.1,
                time_us,
                since_us: self.time_us,
            });
            Mode::Scroll {
                centroid: c,
                last_us: time_us,
            }
        })
    }
}

#[derive(Clone, Debug)]
enum Mode {
    /// Nothing started; the base once the finger set has timed in.
    Candidate(Option<Base>),
    Scroll {
        centroid: (f64, f64),
        /// The last scroll event (or finger change).
        last_us: u64,
    },
    Magnify {
        span: f64,
    },
    Rotate {
        angle: f64,
    },
    Fluid {
        dof: Dof,
        slide: Slide,
        coord: f64,
    },
    /// Taken, and over: the rest of the touch does nothing.
    Ignored,
}

/// One touch: from the first finger down to the last finger up.
#[derive(Clone, Debug)]
struct Touch {
    mode: Mode,
    /// When the current finger set started.
    chord_us: u64,
    /// The last frame.
    last_us: u64,
}

impl Touch {
    fn claim(&self) -> Claim {
        match self.mode {
            Mode::Candidate(_) => Claim::Unclaimed,
            Mode::Scroll { .. } => Claim::Scroll,
            Mode::Magnify { .. } | Mode::Rotate { .. } | Mode::Fluid { .. } | Mode::Ignored => Claim::Gesture,
        }
    }

    fn step(
        &mut self,
        time_us: u64,
        previous: &[Finger],
        fingers: &[Finger],
        multi_finger: bool,
        out: &mut Vec<Recognized>,
    ) {
        let dt = seconds(time_us.saturating_sub(self.last_us));
        let age = seconds(time_us.saturating_sub(self.chord_us));
        self.last_us = time_us;
        let same = same_fingers(previous, fingers);
        let fewer = fingers.len() < previous.len();
        if !same {
            self.chord_us = time_us;
        }
        let chord_us = self.chord_us;
        let n = fingers.len();
        let next = match &mut self.mode {
            Mode::Ignored => None,
            Mode::Candidate(base) => {
                if !same || !multi_finger || n < 2 {
                    *base = None;
                }
                if !multi_finger || n < 2 {
                    None
                } else if n > MAX_FINGERS {
                    Some(Mode::Ignored)
                } else if let Some(base) = base {
                    base.step(fingers, time_us, dt, age, out)
                } else {
                    if time_us.saturating_sub(chord_us) >= CHORD_TIME_IN_US * n as u64 {
                        *base = Some(Base::new(fingers, time_us));
                    }
                    None
                }
            }
            Mode::Scroll {
                centroid: last,
                last_us,
            } => {
                let c = centroid(fingers);
                if n < 2 {
                    // Fewer than two fingers end it (the core may glide on).
                    out.push(Recognized::Scroll {
                        phase: FingerPhase::Ended,
                        dx: 0.0,
                        dy: 0.0,
                        time_us,
                        since_us: *last_us,
                    });
                    Some(Mode::Ignored)
                } else {
                    // Once scrolling, the touch keeps scrolling with the
                    // fingers' centroid (user 2026-11-12, checked on a Mac); a
                    // finger change goes on from the new centroid, no jump.
                    if !same {
                        *last_us = time_us;
                    } else if c != *last {
                        out.push(Recognized::Scroll {
                            phase: FingerPhase::Changed,
                            dx: c.0 - last.0,
                            dy: c.1 - last.1,
                            time_us,
                            since_us: *last_us,
                        });
                        *last_us = time_us;
                    }
                    *last = c;
                    None
                }
            }
            Mode::Magnify { span } => {
                if same {
                    let now = pair(fingers).map_or(*span, |(s, _)| s);
                    if *span > 0.0 && now != *span {
                        out.push(Recognized::Gesture(Gesture::Magnify {
                            phase: GesturePhase::Changed,
                            amount: now / *span - 1.0,
                        }));
                    }
                    *span = now;
                    None
                } else {
                    // Fewer fingers end it, more cancel it.
                    out.push(Recognized::Gesture(Gesture::Magnify {
                        phase: if fewer {
                            GesturePhase::Ended
                        } else {
                            GesturePhase::Cancelled
                        },
                        amount: 0.0,
                    }));
                    Some(Mode::Ignored)
                }
            }
            Mode::Rotate { angle } => {
                if same {
                    let now = pair(fingers).map_or(*angle, |(_, a)| a);
                    let degrees = turn(now - *angle);
                    if degrees != 0.0 {
                        out.push(Recognized::Gesture(Gesture::Rotate {
                            phase: GesturePhase::Changed,
                            degrees,
                        }));
                    }
                    *angle = now;
                    None
                } else {
                    out.push(Recognized::Gesture(Gesture::Rotate {
                        phase: if fewer {
                            GesturePhase::Ended
                        } else {
                            GesturePhase::Cancelled
                        },
                        degrees: 0.0,
                    }));
                    Some(Mode::Ignored)
                }
            }
            Mode::Fluid { dof, slide, coord } => {
                if same {
                    let c = dof.coordinate(fingers);
                    let delta = c - *coord;
                    *coord = c;
                    slide.integrate(delta, dt, age, dof.fluid().scale);
                    // The driver sends nothing for a report without movement.
                    if delta != 0.0 {
                        out.push(Recognized::Gesture(dock(*dof, GesturePhase::Changed, slide)));
                    }
                    None
                } else {
                    // The first lift ends it as the driver decides, from the
                    // integral before the lift (the remaining fingers' new
                    // centroid or spread would jump); an added finger cancels
                    // it. The rest of the touch does nothing.
                    out.push(Recognized::Gesture(end_fluid(*dof, *slide, dt, age, !fewer)));
                    Some(Mode::Ignored)
                }
            }
        };
        if let Some(mode) = next {
            self.mode = mode;
        }
    }

    /// The last finger lifted (or the system took the touch away): what runs
    /// ends (or is cancelled).
    fn finish(self, time_us: u64, cancelled: bool, out: &mut Vec<Recognized>) {
        let dt = seconds(time_us.saturating_sub(self.last_us));
        let age = seconds(time_us.saturating_sub(self.chord_us));
        let phase = if cancelled {
            GesturePhase::Cancelled
        } else {
            GesturePhase::Ended
        };
        match self.mode {
            Mode::Scroll { last_us, .. } => out.push(Recognized::Scroll {
                phase: if cancelled {
                    FingerPhase::Cancelled
                } else {
                    FingerPhase::Ended
                },
                dx: 0.0,
                dy: 0.0,
                time_us,
                since_us: last_us,
            }),
            Mode::Magnify { .. } => out.push(Recognized::Gesture(Gesture::Magnify { phase, amount: 0.0 })),
            Mode::Rotate { .. } => out.push(Recognized::Gesture(Gesture::Rotate { phase, degrees: 0.0 })),
            Mode::Fluid { dof, slide, .. } => {
                out.push(Recognized::Gesture(end_fluid(dof, slide, dt, age, cancelled)));
            }
            Mode::Candidate(_) | Mode::Ignored => {}
        }
    }
}

/// The multi-finger recognizer of one control page.
#[derive(Clone, Debug, Default)]
pub struct Recognizer {
    /// The fingers of the last frame.
    fingers: Vec<Finger>,
    touch: Option<Touch>,
}

impl Recognizer {
    /// One touch frame: every finger on the pad after a touch event (none when
    /// the last one lifted), at the touch time `time_us`. `cancelled`: the
    /// system took the touch away. `multi_finger`: the page leaves this
    /// touch's fingers to this recognizer (it does not while it drags, scrolls
    /// with one finger or moves the pointer with two); once taken, a touch
    /// stays here until its last finger lifts. What the frame produced goes to
    /// `out`; the result is what this recognizer now does with the touch.
    pub fn frame(
        &mut self,
        time_us: u64,
        fingers: &[Finger],
        cancelled: bool,
        multi_finger: bool,
        out: &mut Vec<Recognized>,
    ) -> Claim {
        let previous = std::mem::take(&mut self.fingers);
        if cancelled || fingers.is_empty() {
            if let Some(touch) = self.touch.take() {
                touch.finish(time_us, cancelled, out);
            }
            return Claim::Unclaimed;
        }
        self.fingers = fingers.to_vec();
        let touch = self.touch.get_or_insert(Touch {
            mode: Mode::Candidate(None),
            chord_us: time_us,
            last_us: time_us,
        });
        touch.step(time_us, &previous, fingers, multi_finger, out);
        touch.claim()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000;

    /// `n` fingers evenly on a circle of radius `r` around (`cx`, `cy`), ids 1…n.
    fn hand(n: usize, cx: f64, cy: f64, r: f64) -> Vec<Finger> {
        (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / n as f64 + 0.3;
                Finger {
                    id: i as u64 + 1,
                    x: cx + r * a.cos(),
                    y: cy + r * a.sin(),
                }
            })
            .collect()
    }

    struct Pad {
        r: Recognizer,
        /// Everything produced so far.
        all: Vec<Recognized>,
    }

    impl Pad {
        fn new() -> Self {
            Self {
                r: Recognizer::default(),
                all: Vec::new(),
            }
        }

        /// One frame at `ms`; what it produced and the claim.
        fn at(&mut self, ms: f64, fingers: &[Finger]) -> (Claim, Vec<Recognized>) {
            self.frame(ms, fingers, false, true)
        }

        fn frame(&mut self, ms: f64, fingers: &[Finger], cancelled: bool, multi: bool) -> (Claim, Vec<Recognized>) {
            let mut out = Vec::new();
            let claim = self
                .r
                .frame((ms * MS as f64) as u64, fingers, cancelled, multi, &mut out);
            self.all.extend(out.iter().copied());
            (claim, out)
        }

        fn docks(&self) -> Vec<(SwipeAxis, GesturePhase, f64, f64)> {
            docks(&self.all)
        }
    }

    fn docks(out: &[Recognized]) -> Vec<(SwipeAxis, GesturePhase, f64, f64)> {
        out.iter()
            .filter_map(|r| match r {
                Recognized::Gesture(Gesture::DockSwipe {
                    axis,
                    phase,
                    progress,
                    velocity,
                }) => Some((*axis, *phase, *progress, *velocity)),
                _ => None,
            })
            .collect()
    }

    fn phases(out: &[Recognized]) -> Vec<GesturePhase> {
        out.iter()
            .filter_map(|r| match r {
                Recognized::Gesture(g) => g.phase(),
                Recognized::Scroll { .. } => None,
            })
            .collect()
    }

    fn last_end(pad: &Pad) -> (SwipeAxis, GesturePhase, f64, f64) {
        *pad.docks().last().expect("a Dock gesture")
    }

    /// A hand of `n` fingers moved by `step(k)` (centre x, centre y, radius)
    /// for frames k = 0..=frames every `period_ms`, starting at `t0`; returns
    /// the time of the last frame.
    fn sweep(
        pad: &mut Pad,
        n: usize,
        t0: f64,
        period_ms: f64,
        frames: usize,
        step: impl Fn(usize) -> (f64, f64, f64),
    ) -> f64 {
        let mut t = t0;
        for k in 0..=frames {
            t = t0 + period_ms * k as f64;
            let (x, y, r) = step(k);
            pad.at(t, &hand(n, x, y, r));
        }
        t
    }

    // ---- The driver's end rule on recorded native segments ----

    /// Native Dock gesture regression fixtures (macOS 26.6.2): (ms since Began,
    /// progress) of each event, the last one the end event (long holds are
    /// thinned to every 8th event, keeping the peak and the last events), the
    /// recorded end phase and end velocity (progress per second).
    struct Segment {
        name: &'static str,
        dof: Dof,
        samples: &'static [(f64, f64)],
        ended: bool,
        velocity: f64,
    }

    const SEGMENTS: &[Segment] = &[
        // A quick short pinch out: Ended on its velocity below half way.
        Segment {
            name: "D02",
            dof: Dof::Scale,
            samples: &[
                (0.0, -0.05348205566),
                (8.0, -0.1194458008),
                (16.2, -0.2211456299),
                (24.1, -0.2652435303),
                (56.2, -0.2652435303),
            ],
            ended: true,
            velocity: -6.001861572,
        },
        // Pinched to a third and released slowly: Cancelled.
        Segment {
            name: "D05",
            dof: Dof::Scale,
            samples: &[
                (0.0, 0.02209472656),
                (64.6, 0.1703186035),
                (129.2, 0.2630310059),
                (193.7, 0.2923278809),
                (282.5, 0.3066253662),
                (476.1, 0.3205566406),
                (944.3, 0.3231964111),
                (976.5, 0.3229217529),
                (984.6, 0.3231201172),
                (992.7, 0.3215026855),
                (1000.7, 0.3217773438),
                (1008.6, 0.3217773438),
            ],
            ended: false,
            velocity: -0.02751159668,
        },
        // Past half way, a slight retreat from the peak: still Ended.
        Segment {
            name: "D06",
            dof: Dof::Scale,
            samples: &[
                (0.0, 0.01684570312),
                (64.7, 0.1627807617),
                (129.2, 0.2441558838),
                (193.8, 0.3220672607),
                (258.2, 0.454498291),
                (322.9, 0.5834350586),
                (387.4, 0.6262817383),
                (452.0, 0.641418457),
                (524.7, 0.6564483643),
                (589.2, 0.6650848389),
                (653.8, 0.6905212402),
                (718.4, 0.7214813232),
                (782.9, 0.7652282715),
                (847.4, 0.7855529785),
                (984.7, 0.7936248779),
                (1089.5, 0.7994232178),
                (1444.7, 0.7849731445),
                (1509.2, 0.7447814941),
                (1573.8, 0.7209777832),
                (1638.4, 0.7064666748),
                (1702.8, 0.6960754395),
                (1751.3, 0.6910400391),
                (1783.6, 0.6905670166),
                (1848.2, 0.6888885498),
                (1856.3, 0.6872253418),
                (1864.2, 0.6876678467),
                (1880.1, 0.6876678467),
            ],
            ended: true,
            velocity: -0.05865478515625,
        },
        // A quick pinch back: Ended below half way.
        Segment {
            name: "D07",
            dof: Dof::Scale,
            samples: &[
                (0.0, -0.04096984863),
                (8.1, -0.09335327148),
                (16.2, -0.1502990723),
                (24.3, -0.2034454346),
                (32.3, -0.2629241943),
                (40.3, -0.3108215332),
                (48.4, -0.3488769531),
                (56.3, -0.3829193115),
                (80.3, -0.3829193115),
            ],
            ended: true,
            velocity: -4.516967773,
        },
        // Nearly complete, held, but backed off from a peak of 1.15 to 0.89
        // before the lift: Cancelled (the springing back is the native rule).
        Segment {
            name: "D08",
            dof: Dof::Scale,
            samples: &[
                (0.0, 0.01777648926),
                (64.4, 0.1658172607),
                (129.0, 0.2526397705),
                (193.5, 0.3315887451),
                (258.1, 0.4265136719),
                (322.6, 0.5501098633),
                (387.3, 0.6745300293),
                (451.8, 0.7914581299),
                (516.4, 0.8964691162),
                (581.0, 0.9664459229),
                (645.5, 0.9927062988),
                (710.0, 1.052902222),
                (774.6, 1.083068848),
                (839.2, 1.114883423),
                (903.8, 1.134262085),
                (1008.6, 1.147567749),
                (1178.2, 1.137466431),
                (1275.0, 1.121902466),
                (1347.7, 1.098937988),
                (1412.2, 1.07371521),
                (1541.3, 1.063171387),
                (1605.9, 1.033157349),
                (1670.5, 1.007385254),
                (1735.0, 0.9928131104),
                (1799.6, 0.9605560303),
                (1864.2, 0.9256439209),
                (1985.2, 0.9069824219),
                (2057.9, 0.9040222168),
                (2074.0, 0.9018554688),
                (2082.1, 0.8974609375),
                (2090.2, 0.8946990967),
                (2098.0, 0.888092041),
                (2106.0, 0.888092041),
            ],
            ended: false,
            velocity: -0.356964111328125,
        },
        // Nearly complete, held still, released: Ended.
        Segment {
            name: "D09",
            dof: Dof::Scale,
            samples: &[
                (0.0, 0.02139282227),
                (64.4, 0.1577758789),
                (129.0, 0.2495269775),
                (193.5, 0.3406677246),
                (258.1, 0.4528808594),
                (322.7, 0.5145111084),
                (387.3, 0.5507965088),
                (451.7, 0.6050415039),
                (516.5, 0.6631011963),
                (580.9, 0.7445831299),
                (645.5, 0.7971191406),
                (710.0, 0.84375),
                (774.7, 0.8877716064),
                (895.8, 0.8957366943),
                (1000.6, 0.9098510742),
                (1347.7, 0.9258728027),
                (1533.3, 0.9242095947),
                (1557.5, 0.9259338379),
                (1622.2, 0.9258270264),
                (1630.2, 0.9263153076),
                (1638.1, 0.9228820801),
                (1646.0, 0.9228820801),
            ],
            ended: true,
            velocity: -0.0720062255859375,
        },
        // A space switch given up at 0.4 and released 133 ms later: Cancelled.
        Segment {
            name: "D22",
            dof: Dof::Horizontal,
            samples: &[
                (0.0, -0.007751464844),
                (33.3, -0.04144287109),
                (76.1, -0.08572387695),
                (112.5, -0.1316680908),
                (145.8, -0.1703186035),
                (179.1, -0.209197998),
                (229.2, -0.2725372314),
                (262.5, -0.3051300049),
                (295.8, -0.3323669434),
                (345.8, -0.3678436279),
                (379.2, -0.382019043),
                (412.5, -0.3918151855),
                (445.8, -0.3986663818),
                (485.3, -0.4019165039),
                (487.5, -0.4020233154),
                (493.8, -0.4020996094),
                (495.8, -0.4022674561),
                (501.9, -0.4023895264),
                (634.8, -0.4023895264),
            ],
            ended: false,
            velocity: -0.01116943359,
        },
        // A flick down after Mission Control: Ended at 0.16.
        Segment {
            name: "D24",
            dof: Dof::Vertical,
            samples: &[
                (0.0, 0.01937866211),
                (11.1, 0.04304504395),
                (12.5, 0.04772949219),
                (16.7, 0.07235717773),
                (20.8, 0.080078125),
                (25.0, 0.1121673584),
                (29.1, 0.1191864014),
                (33.3, 0.1568450928),
                (37.5, 0.1620635986),
                (41.6, 0.1620635986),
            ],
            ended: true,
            velocity: 4.988342285,
        },
    ];

    /// The recorded progress through a slide (the driver's own events: the
    /// first after an 8 ms report from rest), the end decided at the last
    /// event: whether it completes, and the velocity.
    fn replay(segment: &Segment) -> (bool, f64) {
        let fluid = segment.dof.fluid();
        let mut slide = Slide::default();
        let (mut t, mut p) = (-8.0, 0.0);
        for &(ms, progress) in segment.samples {
            slide.integrate(
                (progress - p) * fluid.scale,
                (ms - t) / 1000.0,
                ms.max(0.0) / 1000.0,
                fluid.scale,
            );
            (t, p) = (ms, progress);
        }
        (slide.completes(fluid), slide.v)
    }

    #[test]
    fn recorded_native_segments_end_as_the_mac_ended_them() {
        for segment in SEGMENTS {
            let (completes, v) = replay(segment);
            assert_eq!(completes, segment.ended, "{}: v = {v}", segment.name);
            // The filter is the driver's; its sensor reports between the
            // events and the time before Began are not in the recording, so
            // the velocity is close, not equal.
            assert_eq!(v.signum(), segment.velocity.signum(), "{}", segment.name);
            assert!(
                (v - segment.velocity).abs() <= 0.4 * segment.velocity.abs(),
                "{}: {v} vs recorded {}",
                segment.name,
                segment.velocity
            );
        }
    }

    /// The end rule on the recorded end values (REPORT §8.2): progress,
    /// velocity and peak as the driver had them.
    #[test]
    fn the_end_rule_on_recorded_end_values() {
        let rule = |p: f64, v: f64, peak: f64| {
            Slide {
                s: p * 32.0,
                peak: peak * 32.0,
                v,
                pending: 0.0,
            }
            .completes(POLAR_SWIPE)
        };
        // D05: not past half way.
        assert!(!rule(0.321777, -0.027512, 0.323196));
        // D06: a slight retreat keeps it.
        assert!(rule(0.687668, -0.058655, 0.799423));
        // D08: backed off below 0.8 of the peak.
        assert!(!rule(0.888092, -0.356964, 1.147568));
        // D09: held near the end.
        assert!(rule(0.922882, -0.072006, 0.926315));
        // D22 (negative direction): not past half way.
        assert!(!rule(-0.402390, -0.011169, -0.402390));
        // Strictly past half way.
        assert!(!rule(0.5, 0.0, 0.5));
        assert!(rule(0.5001, 0.0, 0.6));
        // A flick: below half way, its velocity carries it.
        assert!(rule(0.2, 1.0, 0.2));
        // Moving back towards the start at the lift: the prediction changes sign.
        assert!(!rule(0.3, -1.0, 0.6));
    }

    #[test]
    fn the_peak_follows_the_extreme_and_restarts_on_reversal() {
        let mut s = Slide::default();
        for d in [1.0, 2.0, -0.5] {
            s.integrate(d, 0.008, 0.0, 65.0);
        }
        assert_eq!((s.s, s.peak), (2.5, 3.0));
        s.integrate(-4.0, 0.008, 0.0, 65.0);
        assert_eq!(s.peak, -1.5, "the integral changed sign");
        s.integrate(0.5, 0.008, 0.0, 65.0);
        assert_eq!(s.peak, -1.5);
    }

    // ---- Recognition from touches ----

    /// Four fingers close in at 50 mm/s and lift: Launchpad. Nothing until the
    /// chord timed in (40 ms) and the spread changed 3 mm; the progress is the
    /// spread change after that over 32 mm.
    #[test]
    fn four_fingers_pinch_with_the_native_scale_and_end() {
        let mut pad = Pad::new();
        let end = sweep(&mut pad, 4, 0.0, 8.0, 30, |k| (30.0, 30.0, 20.0 - 0.4 * k as f64));
        let seen = pad.docks();
        assert_eq!(seen[0].0, SwipeAxis::Pinch);
        assert_eq!(seen[0].1, GesturePhase::Began);
        // Timed in at 40 ms (k = 5), fired at k = 13 (3.2 mm), from that report's 0.4 mm.
        assert!((seen[0].2 - -0.4 / 32.0).abs() < 1e-9, "{seen:?}");
        assert_eq!(seen.len(), 1 + 17);
        assert!(seen[1..].iter().all(|d| d.1 == GesturePhase::Changed && d.3 == 0.0));
        let (claim, lift) = pad.at(end + 8.0, &hand(4, 30.0, 30.0, 8.0)[1..]);
        assert_eq!(claim, Claim::Gesture);
        let lift = docks(&lift);
        assert_eq!(lift.len(), 1);
        let (axis, phase, progress, velocity) = lift[0];
        assert_eq!((axis, phase), (SwipeAxis::Pinch, GesturePhase::Ended));
        assert!((progress - -7.2 / 32.0).abs() < 1e-9, "{progress}");
        assert!(velocity < -1.0, "{velocity}");
    }

    /// The same pinch slowly (over 2.4 s, released at once): below half way
    /// and slow, so it does not complete (a Mac springs back here too).
    #[test]
    fn a_slow_short_pinch_is_cancelled() {
        let mut pad = Pad::new();
        let end = sweep(&mut pad, 4, 0.0, 80.0, 30, |k| (30.0, 30.0, 20.0 - 0.4 * k as f64));
        pad.at(end + 8.0, &hand(4, 30.0, 30.0, 8.0)[1..]);
        let (_, phase, progress, _) = last_end(&pad);
        assert_eq!(phase, GesturePhase::Cancelled);
        assert!(progress < 0.0);
    }

    #[test]
    fn three_fingers_swipe_left_and_up_with_the_wire_signs() {
        let mut pad = Pad::new();
        let end = sweep(&mut pad, 3, 0.0, 8.0, 30, |k| (60.0 - 0.9 * k as f64, 30.0, 12.0));
        pad.at(end + 8.0, &[]);
        let seen = pad.docks();
        assert_eq!(seen[0].0, SwipeAxis::Horizontal);
        // Timed in at 32 ms (k = 4), fired at k = 7 (2.7 mm): progress from 0.9 mm.
        assert!((seen[0].2 - 0.9 / 65.0).abs() < 1e-9, "{seen:?}");
        let (_, phase, progress, velocity) = last_end(&pad);
        assert_eq!(phase, GesturePhase::Ended);
        assert!(
            (progress - 24.0 * 0.9 / 65.0).abs() < 1e-9,
            "fingers left = positive: {progress}"
        );
        assert!(velocity > 1.0);

        let mut up = Pad::new();
        let end = sweep(&mut up, 3, 0.0, 8.0, 30, |k| (30.0, 60.0 - 0.9 * k as f64, 12.0));
        up.at(end + 8.0, &[]);
        let (axis, phase, progress, velocity) = last_end(&up);
        assert_eq!((axis, phase), (SwipeAxis::Vertical, GesturePhase::Ended));
        assert!(
            progress < -0.3 && velocity < -1.0,
            "fingers up = negative: {progress} {velocity}"
        );
    }

    /// Fingers that land one after another: the chord is chosen again with
    /// each finger until something starts. Three fingers pinching start
    /// nothing (three fingers have no pinch), a slight drift stays below the
    /// swipe's 2 mm, and the fourth finger makes it a pinch.
    #[test]
    fn a_late_finger_before_recognition_chooses_the_chord_again() {
        let mut pad = Pad::new();
        let three = |k: usize| {
            let mut h = hand(4, 30.0 - 0.02 * k as f64, 30.0, 20.0 - 0.1 * k as f64);
            h.truncate(3);
            h
        };
        for k in 0..=12 {
            let (claim, out) = pad.at(8.0 * k as f64, &three(k));
            assert_eq!(claim, Claim::Unclaimed);
            assert!(out.is_empty(), "{out:?}");
        }
        // The fourth finger lands at 104 ms; the four pinch from 112 ms.
        let t = sweep(&mut pad, 4, 104.0, 8.0, 30, |k| (29.76, 30.0, 18.8 - 0.4 * k as f64));
        pad.at(t + 8.0, &[]);
        let seen = pad.docks();
        assert!(seen.iter().all(|d| d.0 == SwipeAxis::Pinch), "{seen:?}");
        assert_eq!(seen.last().unwrap().1, GesturePhase::Ended);
    }

    /// A finger added to a running gesture cancels it, and the rest of the
    /// touch does nothing.
    #[test]
    fn a_late_finger_after_recognition_cancels_and_the_touch_is_ignored() {
        let mut pad = Pad::new();
        let t = sweep(&mut pad, 3, 0.0, 8.0, 20, |k| (60.0 - k as f64, 30.0, 12.0));
        assert_eq!(pad.docks()[0].1, GesturePhase::Began);
        let mut four = hand(3, 40.0, 30.0, 12.0);
        four.push(Finger {
            id: 9,
            x: 70.0,
            y: 40.0,
        });
        let (claim, out) = pad.at(t + 8.0, &four);
        assert_eq!(claim, Claim::Gesture);
        assert_eq!(phases(&out), [GesturePhase::Cancelled]);
        // The four now pinch hard and swipe: nothing.
        for k in 1..=30 {
            let moved: Vec<Finger> = four
                .iter()
                .map(|f| Finger {
                    x: f.x * 0.98 - k as f64,
                    ..*f
                })
                .collect();
            let (claim, out) = pad.at(t + 8.0 + 8.0 * k as f64, &moved);
            assert_eq!(claim, Claim::Gesture);
            assert!(out.is_empty());
        }
        assert_eq!(pad.at(t + 400.0, &[]), (Claim::Unclaimed, Vec::new()));
    }

    /// Fingers lifting one by one: the first lift ends the gesture from the
    /// integral before it (no jump from the remaining fingers' spread), the
    /// later lifts do nothing.
    #[test]
    fn sequential_lifts_end_once_at_the_first() {
        let mut pad = Pad::new();
        let t = sweep(&mut pad, 4, 0.0, 8.0, 30, |k| (30.0, 30.0, 20.0 - 0.4 * k as f64));
        let last_changed = pad.docks().last().unwrap().2;
        let fingers = hand(4, 30.0, 30.0, 8.0);
        let mut ends = Vec::new();
        for lifted in 1..=4 {
            let (_, out) = pad.at(t + 30.0 * lifted as f64, &fingers[lifted..]);
            ends.extend(docks(&out));
        }
        assert_eq!(ends.len(), 1, "{ends:?}");
        assert_eq!(ends[0].1, GesturePhase::Ended);
        assert_eq!(ends[0].2, last_changed);
    }

    /// Three fingers move left 0.9 mm every 16 ms (56 mm/s) for `out` reports,
    /// then right for `back` reports, rest `hold_ms` and lift. Timed in at
    /// k = 2, fired at k = 5 (2.7 mm), so the progress counts `out - 4` steps
    /// out.
    fn swipe_hold_release(out: usize, back: usize, hold_ms: f64) -> (GesturePhase, f64, f64) {
        let mut pad = Pad::new();
        let path = move |k: usize| {
            let steps = if k <= out { k as f64 } else { (2 * out - k) as f64 };
            (100.0 - 0.9 * steps, 30.0, 12.0)
        };
        let t = sweep(&mut pad, 3, 0.0, 16.0, out + back, path);
        pad.at(t + hold_ms, &[]);
        let (_, phase, progress, velocity) = last_end(&pad);
        (phase, progress, velocity)
    }

    /// The user's case: nearly complete, held, released. The Mac completes it
    /// when the progress is past half way and has not backed off below 0.8 of
    /// its peak, and only then.
    #[test]
    fn near_complete_hold_then_release_completes_per_the_native_rule() {
        // 48 steps: p = 43.2 / 65 = 0.66, held 0.5 s.
        let (phase, progress, velocity) = swipe_hold_release(52, 0, 500.0);
        assert_eq!(phase, GesturePhase::Ended);
        assert!((progress - 48.0 * 0.9 / 65.0).abs() < 1e-9, "{progress}");
        assert!(velocity.abs() < 0.01, "rested: {velocity}");
        // A small retreat before the lift (0.80 → 0.71) still completes.
        let (phase, ..) = swipe_hold_release(62, 7, 500.0);
        assert_eq!(phase, GesturePhase::Ended);
        // Backed off from 0.90 to 0.60: springs back.
        let (phase, progress, _) = swipe_hold_release(69, 22, 500.0);
        assert_eq!(phase, GesturePhase::Cancelled);
        assert!((progress - 43.0 * 0.9 / 65.0).abs() < 1e-9, "{progress}");
        // Held at 0.3: springs back.
        let (phase, ..) = swipe_hold_release(26, 0, 500.0);
        assert_eq!(phase, GesturePhase::Cancelled);
    }

    #[test]
    fn the_system_taking_the_touch_away_cancels_even_a_complete_gesture() {
        let mut pad = Pad::new();
        let t = sweep(&mut pad, 3, 0.0, 8.0, 60, |k| (100.0 - k as f64, 30.0, 12.0));
        let (claim, out) = pad.frame(t + 8.0, &[], true, true);
        assert_eq!(claim, Claim::Unclaimed);
        assert_eq!(phases(&out), [GesturePhase::Cancelled]);
    }

    fn scrolls(out: &[Recognized]) -> Vec<(FingerPhase, f64, f64)> {
        out.iter()
            .filter_map(|r| match *r {
                Recognized::Scroll { phase, dx, dy, .. } => Some((phase, dx, dy)),
                Recognized::Gesture(_) => None,
            })
            .collect()
    }

    fn two(x: f64, y: f64) -> [Finger; 2] {
        [Finger { id: 1, x, y }, Finger { id: 2, x: x + 20.0, y }]
    }

    /// Two fingers moving within their 20 ms time-in move nothing; the scroll
    /// starts from where the chord timed in.
    #[test]
    fn two_fingers_scroll_after_the_time_in() {
        let mut pad = Pad::new();
        pad.at(0.0, &two(30.0, 50.0)[..1]);
        assert_eq!(pad.at(2.0, &two(30.0, 50.0)).0, Claim::Unclaimed);
        assert_eq!(pad.at(10.0, &two(30.0, 47.0)).0, Claim::Unclaimed, "still timing in");
        assert_eq!(pad.at(22.0, &two(30.0, 46.0)).0, Claim::Unclaimed, "timed in here");
        assert_eq!(
            pad.at(30.0, &two(30.0, 45.0)).0,
            Claim::Unclaimed,
            "1 mm: within the slop"
        );
        let (claim, out) = pad.at(38.0, &two(30.0, 44.0));
        assert_eq!(claim, Claim::Scroll);
        assert_eq!(scrolls(&out), [(FingerPhase::Began, 0.0, -2.0)]);
        assert!(matches!(
            out[0],
            Recognized::Scroll {
                time_us: 38_000,
                since_us: 22_000,
                ..
            }
        ));
        let (_, out) = pad.at(46.0, &two(30.0, 40.0));
        assert_eq!(scrolls(&out), [(FingerPhase::Changed, 0.0, -4.0)]);
        // A third finger: still the scroll, from the new centroid.
        let mut three = two(30.0, 40.0).to_vec();
        three.push(Finger {
            id: 3,
            x: 70.0,
            y: 40.0,
        });
        assert_eq!(pad.at(50.0, &three), (Claim::Scroll, Vec::new()));
        let moved: Vec<Finger> = three.iter().map(|f| Finger { x: f.x - 6.0, ..*f }).collect();
        assert_eq!(scrolls(&pad.at(58.0, &moved).1), [(FingerPhase::Changed, -6.0, 0.0)]);
        // Below two fingers it ends; the remaining finger does nothing.
        pad.at(60.0, &moved[1..]);
        let (claim, out) = pad.at(62.0, &moved[2..]);
        assert_eq!(claim, Claim::Gesture);
        assert_eq!(scrolls(&out), [(FingerPhase::Ended, 0.0, 0.0)]);
        assert!(pad.at(70.0, &[Finger { x: 40.0, ..moved[2] }]).1.is_empty());
    }

    #[test]
    fn two_fingers_magnify_and_rotate() {
        // Two fingers `span` apart about (50, 50), turned `degrees`
        // counter-clockwise.
        let pair_at = |span: f64, degrees: f64| {
            let (s, c) = (
                degrees.to_radians().sin() * span / 2.0,
                degrees.to_radians().cos() * span / 2.0,
            );
            [
                Finger {
                    id: 1,
                    x: 50.0 - c,
                    y: 50.0 + s,
                },
                Finger {
                    id: 2,
                    x: 50.0 + c,
                    y: 50.0 - s,
                },
            ]
        };
        // The single gesture of a frame: (phase, amount or degrees).
        let only = |out: &[Recognized]| match out {
            [Recognized::Gesture(Gesture::Magnify { phase, amount })] => (*phase, *amount),
            [Recognized::Gesture(Gesture::Rotate { phase, degrees })] => (*phase, *degrees),
            _ => panic!("{out:?}"),
        };
        let mut pad = Pad::new();
        pad.at(0.0, &pair_at(30.0, 0.0));
        pad.at(20.0, &pair_at(30.0, 0.0));
        let (claim, out) = pad.at(28.0, &pair_at(33.0, 0.0));
        assert_eq!(claim, Claim::Gesture);
        assert!(matches!(out[0], Recognized::Gesture(Gesture::Magnify { .. })));
        let (phase, amount) = only(&out[..]);
        assert_eq!(phase, GesturePhase::Began);
        assert!((amount - 0.1).abs() < 1e-9, "{amount}");
        let (phase, amount) = only(&pad.at(36.0, &pair_at(36.3, 0.0)).1[..]);
        assert_eq!(phase, GesturePhase::Changed);
        assert!((amount - 0.1).abs() < 1e-9, "{amount}");
        // A third finger cancels it.
        let mut three = pair_at(36.3, 0.0).to_vec();
        three.push(Finger {
            id: 3,
            x: 10.0,
            y: 10.0,
        });
        assert_eq!(
            pad.at(40.0, &three).1,
            [Recognized::Gesture(Gesture::Magnify {
                phase: GesturePhase::Cancelled,
                amount: 0.0
            })]
        );

        let mut turn = Pad::new();
        turn.at(0.0, &pair_at(30.0, 0.0));
        turn.at(20.0, &pair_at(30.0, 0.0));
        let (_, out) = turn.at(28.0, &pair_at(30.0, 10.0));
        assert!(matches!(out[0], Recognized::Gesture(Gesture::Rotate { .. })));
        let (phase, degrees) = only(&out[..]);
        assert_eq!(phase, GesturePhase::Began);
        assert!((degrees - 10.0).abs() < 1e-9, "{degrees}");
        let (_, out) = turn.at(40.0, &pair_at(30.0, 10.0)[..1]);
        assert_eq!(
            out,
            [Recognized::Gesture(Gesture::Rotate {
                phase: GesturePhase::Ended,
                degrees: 0.0
            })]
        );
    }

    /// While the page uses the touch (a drag, the swapped pointer) nothing is
    /// taken; once it lets go, a timed-in chord starts from there.
    #[test]
    fn nothing_is_taken_while_the_page_keeps_the_touch() {
        let mut pad = Pad::new();
        for k in 0..=10 {
            let (claim, out) = pad.frame(
                8.0 * k as f64,
                &hand(3, 60.0 - 2.0 * k as f64, 30.0, 12.0),
                false,
                false,
            );
            assert_eq!((claim, out), (Claim::Unclaimed, Vec::new()));
        }
        // Left to the recognizer: the base is this frame, so the next 3 mm fire.
        assert_eq!(pad.at(88.0, &hand(3, 38.0, 30.0, 12.0)).1, Vec::new());
        let (claim, out) = pad.at(96.0, &hand(3, 35.0, 30.0, 12.0));
        assert_eq!(claim, Claim::Gesture);
        assert_eq!(docks(&out)[0].1, GesturePhase::Began);
    }

    #[test]
    fn six_fingers_make_the_touch_do_nothing() {
        let mut pad = Pad::new();
        assert_eq!(pad.at(0.0, &hand(6, 30.0, 30.0, 15.0)).0, Claim::Gesture);
        let t = sweep(&mut pad, 6, 8.0, 8.0, 20, |k| (30.0 - k as f64, 30.0, 15.0));
        assert!(pad.all.is_empty());
        assert_eq!(pad.at(t + 8.0, &[]).0, Claim::Unclaimed);
    }
}
