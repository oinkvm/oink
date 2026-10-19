//! Observation only: a submitted or returned CGEvent does not prove Dock consumed it.
//! Keep native capture and injection ledgers separate; neither repairs or cancels input.
use oink_foundation::input::{Gesture, SwipeAxis};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug)]
struct Active {
    sequence: u64,
    first_owner: &'static str,
    last: Gesture,
    frames: u64,
}

#[derive(Clone, Copy, Default)]
pub(super) struct Tracker {
    active: [Option<Active>; 5],
}

fn slot(g: Gesture) -> Option<usize> {
    Some(match g {
        Gesture::DockSwipe {
            axis: SwipeAxis::Horizontal,
            ..
        } => 0,
        Gesture::DockSwipe {
            axis: SwipeAxis::Vertical,
            ..
        } => 1,
        Gesture::DockSwipe {
            axis: SwipeAxis::Pinch, ..
        } => 2,
        Gesture::Magnify { .. } => 3,
        Gesture::Rotate { .. } => 4,
        Gesture::SmartMagnify => return None,
    })
}

impl Tracker {
    pub(super) fn record(&mut self, gesture: Gesture, owner: &'static str, timestamp_ns: u64) {
        let Some(slot) = slot(gesture) else {
            tracing::debug!(?gesture, owner, timestamp_ns, "gesture lifecycle: instantaneous");
            return;
        };
        let previous = self.active[slot];
        let missing_begin = previous.is_none() && !gesture.starts();
        let replaced = gesture.starts() && previous.is_some();
        let mut active = match previous.filter(|_| !gesture.starts()) {
            Some(active) => active,
            None => Active {
                sequence: NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed),
                first_owner: owner,
                last: gesture,
                frames: 0,
            },
        };
        active.last = gesture;
        active.frames += 1;
        let owner_changed = active.first_owner != owner;
        let ended = gesture.ends();
        tracing::debug!(
            sequence = active.sequence,
            ?gesture,
            owner,
            first_owner = active.first_owner,
            frames = active.frames,
            timestamp_ns,
            missing_begin,
            replaced,
            owner_changed,
            ended,
            "gesture lifecycle: frame (delivery unconfirmed)"
        );
        if missing_begin || replaced || owner_changed {
            tracing::warn!(
                sequence = active.sequence,
                ?previous,
                ?gesture,
                owner,
                missing_begin,
                replaced,
                owner_changed,
                "gesture lifecycle: discontinuity (observation only)"
            );
        }
        self.active[slot] = (!ended).then_some(active);
    }

    pub(super) fn boundary(&self, stream: &'static str, reason: &'static str) {
        tracing::debug!(stream, reason, active = ?self.active, "gesture lifecycle: boundary snapshot");
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_foundation::input::GesturePhase;
    fn pinch(phase: GesturePhase) -> Gesture {
        Gesture::DockSwipe {
            axis: SwipeAxis::Pinch,
            phase,
            progress: 0.2,
            velocity: 0.0,
        }
    }
    #[test]
    fn ownership_change_remains_visible_until_the_sequence_ends() {
        let mut t = Tracker::default();
        t.record(pinch(GesturePhase::Began), "pass", 1);
        let id = t.active[2].unwrap().sequence;
        t.record(pinch(GesturePhase::Changed), "swallow", 2);
        assert_eq!(t.active[2].unwrap().first_owner, "pass");
        assert_eq!(t.active[2].unwrap().sequence, id);
        t.boundary("native", "tap_stop");
        assert_eq!(t.active[2].unwrap().frames, 2);
        t.record(pinch(GesturePhase::Ended), "swallow", 3);
        assert!(t.active.iter().all(Option::is_none));
    }
    #[test]
    fn interruption_and_restarted_begin_do_not_merge_sequences() {
        let mut t = Tracker::default();
        t.record(pinch(GesturePhase::Changed), "inject", 1);
        let id = t.active[2].unwrap().sequence;
        t.record(pinch(GesturePhase::Began), "inject", 2);
        assert_ne!(t.active[2].unwrap().sequence, id);
        t.record(
            Gesture::Magnify {
                phase: GesturePhase::Began,
                amount: 0.1,
            },
            "inject",
            3,
        );
        t.record(pinch(GesturePhase::Cancelled), "inject", 4);
        assert!(t.active[2].is_none());
        assert!(t.active[3].is_some());
    }
}
