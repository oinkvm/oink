//! Numeric, bounded gesture observations. No input decisions or native calls.
use oink_foundation::{
    DeviceId,
    input::{Gesture, GesturePhase, SwipeAxis},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Capture,
    WindowsMapper,
    MacSynth,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GestureKind {
    Vertical,
    Horizontal,
    Pinch,
    Magnify,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Observed,
    Pending,
    TaskView,
    TaskViewClose,
    DesktopLeft,
    DesktopRight,
    ShowDesktop,
    BelowThreshold,
    WrongDirection,
    Cancelled,
    CtrlWheel,
    NativeDockControl,
    NativeMagnify,
    NativeCancel,
    SubmissionFailed,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub kind: GestureKind,
    pub phase: GesturePhase,
    /// Content identity of the ordered numeric frames; not a unique wire attempt ID.
    pub identity: u64,
    pub begin_identity: u64,
    pub missing_begin: bool,
    pub replaced: bool,
    pub progress: f64,
    pub velocity: f64,
    pub peak_abs_progress: f64,
    pub total_amount: f64,
    pub duration_us: u64,
    pub changed_events: u64,
    /// Absolute number produced, plus signed balance for gestures that reverse.
    pub ctrl_wheel_notches: u64,
    pub ctrl_wheel_balance: i64,
    pub decision: Decision,
    /// None for capture and native macOS replay, which have no mapper threshold.
    pub threshold: Option<f64>,
    /// Windows fallback's foreground sample at a committed swipe. None = not queried.
    #[serde(default)]
    pub task_view_showing: Option<bool>,
}
#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub stage: Stage,
    pub source: Option<DeviceId>,
    pub summary: Summary,
}
#[derive(Clone, Copy, Debug)]
pub struct Outcome {
    pub decision: Decision,
    pub threshold: Option<f64>,
    pub notches: i32,
}
#[derive(Clone, Copy, Debug)]
struct Active {
    source: Option<DeviceId>,
    start_us: u64,
    summary: Summary,
}
#[derive(Debug, Default)]
pub struct Tracker {
    generation: u64,
    active: [Option<Active>; 4],
}

impl Tracker {
    /// Changed only updates fixed numeric state; Began/Ended/Cancelled return rows.
    pub fn observe(
        &mut self,
        gesture: Gesture,
        at_us: u64,
        generation: u64,
        source: Option<DeviceId>,
        outcome: Outcome,
    ) -> Option<Summary> {
        let (kind, phase, progress, velocity, amount) = match gesture {
            Gesture::DockSwipe {
                axis,
                phase,
                progress,
                velocity,
            } => (
                match axis {
                    SwipeAxis::Vertical => GestureKind::Vertical,
                    SwipeAxis::Horizontal => GestureKind::Horizontal,
                    SwipeAxis::Pinch => GestureKind::Pinch,
                },
                phase,
                progress,
                velocity,
                0.,
            ),
            Gesture::Magnify { phase, amount } => (GestureKind::Magnify, phase, 0., 0., amount),
            _ => return None,
        };
        if self.generation != generation {
            self.active = [None; 4];
            self.generation = generation;
        }
        let slot = kind as usize;
        let Outcome {
            decision,
            threshold,
            notches,
        } = outcome;
        let previous = self.active[slot].filter(|a| a.source == source);
        let began = phase == GesturePhase::Began;
        let fingerprint = frame_identity(kind, phase, progress, velocity, amount);
        let mut active = previous.filter(|_| !began).unwrap_or(Active {
            source,
            start_us: at_us,
            summary: Summary {
                kind,
                phase,
                identity: 0,
                begin_identity: fingerprint,
                missing_begin: !began,
                replaced: began && previous.is_some(),
                progress: 0.,
                velocity: 0.,
                peak_abs_progress: 0.,
                total_amount: 0.,
                duration_us: 0,
                changed_events: 0,
                ctrl_wheel_notches: 0,
                ctrl_wheel_balance: 0,
                decision,
                threshold,
                task_view_showing: None,
            },
        });
        let row = &mut active.summary;
        row.identity = fold(row.identity, fingerprint);
        row.phase = phase;
        row.progress = progress;
        row.velocity = velocity;
        row.peak_abs_progress = row.peak_abs_progress.max(progress.abs());
        row.total_amount += amount;
        row.duration_us = at_us.saturating_sub(active.start_us);
        row.changed_events = row
            .changed_events
            .saturating_add(u64::from(phase == GesturePhase::Changed));
        row.ctrl_wheel_notches = row.ctrl_wheel_notches.saturating_add(u64::from(notches.unsigned_abs()));
        row.ctrl_wheel_balance = row.ctrl_wheel_balance.saturating_add(i64::from(notches));
        row.decision = decision;
        row.threshold = threshold;
        let summary = *row;
        self.active[slot] = (!phase.is_end()).then_some(active);
        (began || phase.is_end()).then_some(summary)
    }
}
fn fold(previous: u64, value: u64) -> u64 {
    (previous ^ value).wrapping_mul(0x100000001b3)
}
fn frame_identity(kind: GestureKind, phase: GesturePhase, progress: f64, velocity: f64, amount: f64) -> u64 {
    [
        kind as u64,
        phase as u64,
        progress.to_bits(),
        velocity.to_bits(),
        amount.to_bits(),
    ]
    .into_iter()
    .fold(0xcbf29ce484222325, fold)
}

thread_local! {
    static SOURCE: std::cell::Cell<Option<DeviceId>> = const { std::cell::Cell::new(None) };
}
pub fn source() -> Option<DeviceId> {
    SOURCE.get()
}
/// Synchronous native call only; restores even on panic. Never changes input.
pub fn with_source<T>(source: Option<DeviceId>, f: impl FnOnce() -> T) -> T {
    struct Reset(Option<DeviceId>);
    impl Drop for Reset {
        fn drop(&mut self) {
            SOURCE.set(self.0);
        }
    }
    let _reset = Reset(SOURCE.replace(source));
    f()
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    fn swipe(phase: GesturePhase, progress: f64) -> Gesture {
        Gesture::DockSwipe {
            axis: SwipeAxis::Vertical,
            phase,
            progress,
            velocity: 1.25,
        }
    }
    const OBSERVED: Outcome = Outcome {
        decision: Decision::Observed,
        threshold: None,
        notches: 0,
    };
    #[test]
    fn numeric_identity_matches_across_clocks_and_changed_never_produces_rows() {
        let mut source = Tracker::default();
        let mut receiver = Tracker::default();
        let peer = Some(DeviceId([1; 32]));
        for (phase, progress, at_us) in [
            (GesturePhase::Began, 0., 0),
            (GesturePhase::Changed, -0.8, 100),
            (GesturePhase::Changed, -0.7, 200),
            (GesturePhase::Ended, -0.4, 300),
        ] {
            let a = source.observe(swipe(phase, progress), at_us, 1, peer, OBSERVED);
            let b = receiver.observe(
                swipe(phase, progress),
                at_us + 10000,
                9,
                peer,
                Outcome {
                    decision: Decision::BelowThreshold,
                    threshold: Some(0.5),
                    notches: 0,
                },
            );
            assert_eq!(a.is_some(), phase != GesturePhase::Changed);
            if let (Some(a), Some(b)) = (a, b) {
                assert_eq!(a.identity, b.identity);
                assert_eq!(a.begin_identity, b.begin_identity);
                assert_eq!(a.duration_us, b.duration_us);
                assert_eq!(a.velocity, 1.25);
                if phase == GesturePhase::Ended {
                    assert_eq!(a.peak_abs_progress, 0.8);
                    assert_eq!(a.changed_events, 2);
                    assert_eq!(a.duration_us, 300);
                    assert_eq!(b.decision, Decision::BelowThreshold);
                }
            }
        }
    }
    #[test]
    fn interrupted_recording_source_change_and_restart_are_explicit() {
        let mut t = Tracker::default();
        let peer = Some(DeviceId([1; 32]));
        assert!(
            !t.observe(swipe(GesturePhase::Began, 0.), 0, 1, peer, OBSERVED)
                .unwrap()
                .missing_begin
        );
        assert!(
            t.observe(swipe(GesturePhase::Began, 0.), 1, 1, peer, OBSERVED)
                .unwrap()
                .replaced
        );
        let row = t
            .observe(swipe(GesturePhase::Ended, -0.8), 2, 2, peer, OBSERVED)
            .unwrap();
        assert!(row.missing_begin);
        assert_eq!(row.changed_events, 0);
        assert_eq!(row.duration_us, 0);
        t.observe(swipe(GesturePhase::Began, 0.), 3, 2, peer, OBSERVED);
        assert!(
            t.observe(swipe(GesturePhase::Cancelled, -0.8), 4, 2, None, OBSERVED)
                .unwrap()
                .missing_begin
        );
    }
    #[test]
    fn scoped_origin_restores_after_nested_call_and_panic() {
        let a = Some(DeviceId([1; 32]));
        let b = Some(DeviceId([2; 32]));
        with_source(a, || {
            assert_eq!(source(), a);
            let result = std::panic::catch_unwind(|| {
                with_source(b, || {
                    assert_eq!(source(), b);
                    panic!("test");
                })
            });
            assert!(result.is_err());
            assert_eq!(source(), a);
        });
        assert_eq!(source(), None);
    }
}
