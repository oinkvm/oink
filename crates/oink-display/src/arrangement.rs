//! Pure native-origin planning. Hidden screens participate; mirror followers do
//! not introduce another desktop rectangle. No function here writes a display.
use oink_foundation::{Point, Rect};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeDisplay {
    pub key: String,
    pub bounds: Rect,
    pub primary: bool,
    pub mirror_of: Option<String>,
    /// Opaque platform mode fingerprint, not a mode that may be restored.
    pub mode: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeConfiguration {
    pub displays: Vec<NativeDisplay>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeOrigin {
    pub key: String,
    pub origin: Point,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArrangementError {
    #[error("invalid native geometry")]
    Invalid,
    #[error("native displays overlap")]
    Overlap,
    #[error("native displays must form one edge-connected desktop")]
    Disconnected,
    #[error("native configuration changed")]
    Changed,
}

pub fn valid_integer(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0 && value >= i32::MIN as f64 && value <= i32::MAX as f64
}

fn valid_rect(r: Rect) -> bool {
    [r.x, r.y, r.w, r.h, r.x + r.w, r.y + r.h]
        .into_iter()
        .all(valid_integer)
        && r.w > 0.0
        && r.h > 0.0
}

pub fn overlaps(a: Rect, b: Rect) -> bool {
    (a.x + a.w).min(b.x + b.w) > a.x.max(b.x) && (a.y + a.h).min(b.y + b.h) > a.y.max(b.y)
}

pub fn shares_edge(a: Rect, b: Rect) -> bool {
    ((a.x + a.w == b.x || b.x + b.w == a.x) && (a.y + a.h).min(b.y + b.h) > a.y.max(b.y))
        || ((a.y + a.h == b.y || b.y + b.h == a.y) && (a.x + a.w).min(b.x + b.w) > a.x.max(b.x))
}

pub fn validate_rectangles(rectangles: &[Rect]) -> Result<(), ArrangementError> {
    if rectangles.is_empty() || rectangles.len() > 256 || rectangles.iter().any(|r| !valid_rect(*r)) {
        return Err(ArrangementError::Invalid);
    }
    for (i, a) in rectangles.iter().enumerate() {
        if rectangles[i + 1..].iter().any(|b| overlaps(*a, *b)) {
            return Err(ArrangementError::Overlap);
        }
    }
    let mut reached = BTreeSet::from([0]);
    let mut queue = vec![0];
    while let Some(i) = queue.pop() {
        for (j, r) in rectangles.iter().enumerate() {
            if !reached.contains(&j) && shares_edge(rectangles[i], *r) {
                reached.insert(j);
                queue.push(j);
            }
        }
    }
    if reached.len() != rectangles.len() {
        return Err(ArrangementError::Disconnected);
    }
    Ok(())
}

impl NativeConfiguration {
    pub fn canonicalize(&mut self) {
        self.displays.sort_by(|a, b| a.key.cmp(&b.key));
    }

    pub fn sources(&self) -> impl Iterator<Item = &NativeDisplay> {
        self.displays.iter().filter(|d| d.mirror_of.is_none())
    }

    /// Validate a complete origin-only edit without changing modes, primary or
    /// mirror membership. The real writer re-reads this baseline before apply.
    pub fn with_origins(&self, origins: &[NativeOrigin]) -> Result<Self, ArrangementError> {
        if self.displays.is_empty() || self.displays.len() > 256 {
            return Err(ArrangementError::Invalid);
        }
        let keys = self.displays.iter().map(|d| d.key.as_str()).collect::<BTreeSet<_>>();
        if keys.len() != self.displays.len() || self.sources().filter(|d| d.primary).count() != 1 {
            return Err(ArrangementError::Invalid);
        }
        let supplied = origins.iter().map(|o| o.key.as_str()).collect::<BTreeSet<_>>();
        if supplied.len() != origins.len() || supplied != self.sources().map(|d| d.key.as_str()).collect() {
            return Err(ArrangementError::Changed);
        }
        let mut next = self.clone();
        for display in &mut next.displays {
            let source_key = display.mirror_of.as_deref().unwrap_or(&display.key);
            let origin = origins
                .iter()
                .find(|o| o.key == source_key)
                .ok_or(ArrangementError::Invalid)?;
            if !valid_integer(origin.origin.x) || !valid_integer(origin.origin.y) {
                return Err(ArrangementError::Invalid);
            }
            display.bounds.x = origin.origin.x;
            display.bounds.y = origin.origin.y;
            if display.primary && origin.origin != Point::default() {
                return Err(ArrangementError::Invalid);
            }
        }
        validate_rectangles(&next.sources().map(|d| d.bounds).collect::<Vec<_>>())?;
        next.canonicalize();
        Ok(next)
    }
}

/// Closest integer position for a moved rectangle. The whole graph, including
/// stationary components disconnected by moving a bridge, must remain connected.
pub fn snap(rectangles: &[Rect], moving: usize, desired: Point) -> Result<Point, ArrangementError> {
    if moving >= rectangles.len() || rectangles.len() > 256 || rectangles.iter().any(|r| !valid_rect(*r)) {
        return Err(ArrangementError::Invalid);
    }
    let desired = Point::new(desired.x.round(), desired.y.round());
    if !valid_integer(desired.x) || !valid_integer(desired.y) {
        return Err(ArrangementError::Invalid);
    }
    let m = rectangles[moving];
    let mut xs = vec![desired.x];
    let mut ys = vec![desired.y];
    for (i, r) in rectangles.iter().enumerate() {
        if i == moving {
            continue;
        }
        // Edge equalities plus positive integer-span interval ends. Including
        // obstacle breakpoints makes the nearest valid point finite to search.
        xs.extend([r.x - m.w, r.x + r.w, r.x + 1.0 - m.w, r.x + r.w - 1.0]);
        ys.extend([r.y - m.h, r.y + r.h, r.y + 1.0 - m.h, r.y + r.h - 1.0]);
    }
    xs.sort_by(f64::total_cmp);
    ys.sort_by(f64::total_cmp);
    xs.dedup();
    ys.dedup();
    let mut best: Option<(f64, Point)> = None;
    let mut test = rectangles.to_vec();
    for x in xs {
        for y in &ys {
            let position = Point::new(x, *y);
            let cost = (x - desired.x).powi(2) + (*y - desired.y).powi(2);
            if best.as_ref().is_some_and(|(c, _)| cost >= *c) {
                continue;
            }
            test[moving].x = x;
            test[moving].y = *y;
            if validate_rectangles(&test).is_ok() {
                best = Some((cost, position));
            }
        }
    }
    best.map(|(_, p)| p).ok_or(ArrangementError::Disconnected)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(x: f64, y: f64) -> Rect {
        Rect::new(x, y, 100.0, 100.0)
    }

    #[test]
    fn connectivity_checks_the_whole_graph_not_each_screens_neighbor_count() {
        assert_eq!(
            validate_rectangles(&[r(0.0, 0.0), r(100.0, 100.0)]),
            Err(ArrangementError::Disconnected)
        );
        assert_eq!(
            validate_rectangles(&[r(0.0, 0.0), r(100.0, 0.0), r(300.0, 0.0), r(400.0, 0.0)]),
            Err(ArrangementError::Disconnected)
        );
        assert_eq!(
            validate_rectangles(&[r(0.0, 0.0), r(99.0, 0.0)]),
            Err(ArrangementError::Overlap)
        );
        assert!(validate_rectangles(&[r(0.0, 0.0), r(-100.0, 99.0)]).is_ok());
        assert!(validate_rectangles(&[r(0.0, 0.0)]).is_ok());
    }

    #[test]
    fn snapping_reconnects_bridge_and_does_not_accept_corner_contact() {
        let rs = [r(0.0, 0.0), r(100.0, 0.0), r(200.0, 0.0)];
        let p = snap(&rs, 1, Point::new(100.0, 130.0)).unwrap();
        assert_eq!(p, Point::new(100.0, 99.0));
        let p = snap(&rs[..2], 1, Point::new(100.0, -100.0)).unwrap();
        assert!(p == Point::new(99.0, -100.0) || p == Point::new(100.0, -99.0));
    }

    #[test]
    fn clone_followers_do_not_overlap_their_desktop_source() {
        let config = NativeConfiguration {
            displays: vec![
                NativeDisplay {
                    key: "a".into(),
                    bounds: r(0.0, 0.0),
                    primary: true,
                    mirror_of: None,
                    mode: "mode-a".into(),
                },
                NativeDisplay {
                    key: "b".into(),
                    bounds: r(0.0, 0.0),
                    primary: false,
                    mirror_of: Some("a".into()),
                    mode: "mode-b".into(),
                },
                NativeDisplay {
                    key: "c".into(),
                    bounds: r(100.0, 0.0),
                    primary: false,
                    mirror_of: None,
                    mode: "mode-c".into(),
                },
            ],
        };
        let next = config
            .with_origins(&[
                NativeOrigin {
                    key: "a".into(),
                    origin: Point::default(),
                },
                NativeOrigin {
                    key: "c".into(),
                    origin: Point::new(0.0, -100.0),
                },
            ])
            .unwrap();
        assert_eq!(next.displays[1], config.displays[1]);
        assert_eq!(next.displays[2].mode, "mode-c");
        assert!(config.with_origins(&[]).is_err());
    }
}
