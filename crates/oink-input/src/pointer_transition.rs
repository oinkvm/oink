//! One observation when physical motion resumes after cursor placement or a mode
//! transition. Own placements update a fixed-size summary; they do not emit rows.
//! This observer never changes the compensation ledger or any event value.
use oink_foundation::Point;
use serde::{Deserialize, Serialize};

use crate::warp_delta::DeltaSnapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlacementKind {
    Owner,
    Tagged,
    NativeButton,
    NativeMotion,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub kind: PlacementKind,
    pub at: Point,
    /// Native delta fields before own-event metadata is rewritten.
    pub delta: (f64, f64),
    pub timestamp_ns: u64,
    pub source_state: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub remote: bool,
    /// Native cursor read at freeze, or the completed restore target. Never the
    /// router's requested Windows park position on a Mac.
    pub mode_cursor: Option<Point>,
    pub mode_timestamp_ns: u64,
    pub placements: u64,
    pub last_placement: Option<Placement>,
    pub event_position: Point,
    pub event_timestamp_ns: u64,
    pub source_state: i64,
    pub raw_delta: (f64, f64),
    pub corrected_delta: (f64, f64),
    pub before: DeltaSnapshot,
    pub after: DeltaSnapshot,
}

#[derive(Debug, Default)]
pub struct Tracker {
    armed: bool,
    mode_cursor: Option<Point>,
    mode_timestamp_ns: u64,
    placements: u64,
    last_placement: Option<Placement>,
}

impl Tracker {
    pub fn mode(&mut self, cursor: Point, timestamp_ns: u64) {
        self.armed = true;
        self.mode_cursor = Some(cursor);
        self.mode_timestamp_ns = timestamp_ns;
    }

    pub fn placed(&mut self, placement: Placement) {
        self.armed = true;
        self.placements += 1;
        self.last_placement = Some(placement);
    }

    pub fn resumed(&mut self, mut sample: Observation) -> Option<Observation> {
        if !std::mem::take(&mut self.armed) {
            return None;
        }
        sample.mode_cursor = self.mode_cursor;
        sample.mode_timestamp_ns = self.mode_timestamp_ns;
        sample.placements = std::mem::take(&mut self.placements);
        sample.last_placement = self.last_placement.take();
        Some(sample)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::warp_delta::WarpDeltaFix;

    fn sample(raw: (f64, f64), fix: &mut WarpDeltaFix, at: Point) -> Observation {
        let before = fix.snapshot();
        let corrected_delta = fix.motion(raw, at, 100, true);
        Observation {
            remote: true,
            mode_cursor: None,
            mode_timestamp_ns: 0,
            placements: 0,
            last_placement: None,
            event_position: at,
            event_timestamp_ns: 100,
            source_state: 1,
            raw_delta: raw,
            corrected_delta,
            before,
            after: fix.snapshot(),
        }
    }

    #[test]
    fn first_motion_distinguishes_folded_displacement_from_an_unfolded_sample() {
        let base = Point::new(318.51171875, 595.390625);
        let placed = Point::new(4.234375, 552.390625);
        for folded in [false, true] {
            let mut fix = WarpDeltaFix::new();
            fix.motion((0., 0.), base, 1, true);
            let mut tracker = Tracker::default();
            for timestamp_ns in 2..10 {
                if folded {
                    fix.posted_absolute_motion(placed, timestamp_ns);
                } else {
                    fix.native_absolute_motion(placed, timestamp_ns);
                }
                tracker.placed(Placement {
                    kind: if folded {
                        PlacementKind::Tagged
                    } else {
                        PlacementKind::NativeMotion
                    },
                    at: placed,
                    delta: (0., 0.),
                    timestamp_ns,
                    source_state: 1,
                });
            }
            fix.enter_remote();
            tracker.mode(placed, 10);
            let raw = if folded {
                (placed.x - base.x, placed.y - base.y)
            } else {
                (0., 0.)
            };
            let row = tracker.resumed(sample(raw, &mut fix, placed)).unwrap();
            assert_eq!(row.placements, 8);
            assert_eq!(row.before.hid_location, Some(if folded { base } else { placed }));
            assert_eq!(row.mode_cursor, Some(placed));
            assert_eq!(row.raw_delta, raw);
            assert_eq!(row.corrected_delta, (0., 0.));
            assert_eq!(row.after.fold, None);
            for _ in 0..1000 {
                assert!(tracker.resumed(sample((0., 2.), &mut fix, placed)).is_none());
            }
        }
    }
}
