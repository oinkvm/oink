//! Near misses extend into walls only; existing portal segments stay unchanged.
use super::{ScreenId, Side, portal::Seg};
use serde::{Deserialize, Serialize};

/// Compiled source-edge interval and the nearest channel endpoint on the target.
/// Coordinates are plane reference units, along y for left/right and x for top/bottom.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerZone {
    pub screen: ScreenId,
    pub side: Side,
    pub from: f64,
    pub to: f64,
    pub target: ScreenId,
    pub entry_along: f64,
    pub gap_distance: f64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Zone {
    pub from: f64,
    pub to: f64,
    pub target: usize,
    pub gap: f64,
    pub entry: f64,
    pub channel: (f64, f64),
}

pub(super) fn compile(segs: &[Seg], size: f64) -> Vec<Zone> {
    let size = if size.is_finite() {
        size.clamp(0., 50. / 686.)
    } else {
        0.
    };
    let mut zones = Vec::new();
    if size == 0. {
        return zones;
    }
    for (i, wall) in segs.iter().enumerate().filter(|(_, s)| s.portal.is_none()) {
        let left = i.checked_sub(1).map(|i| &segs[i]).filter(|s| s.portal.is_some());
        let right = segs.get(i + 1).filter(|s| s.portal.is_some());
        let mid = (wall.from + wall.to) / 2.;
        if let Some(s) = left {
            let (target, gap) = s.portal.unwrap();
            zones.push(Zone {
                from: wall.from,
                to: (wall.from + size).min(if right.is_some() { mid } else { wall.to }),
                target,
                gap,
                entry: s.to,
                channel: (s.from, s.to),
            });
        }
        if let Some(s) = right {
            let (target, gap) = s.portal.unwrap();
            zones.push(Zone {
                from: (wall.to - size).max(if left.is_some() { mid } else { wall.from }),
                to: wall.to,
                target,
                gap,
                entry: s.from,
                channel: (s.from, s.to),
            });
        }
    }
    zones
}
