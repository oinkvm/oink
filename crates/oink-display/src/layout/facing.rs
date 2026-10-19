//! Authoritative rectangle-facing geometry, mirrored by arrange/facing.ts.
//! A facing interval has positive length (corner-only contact is excluded) and
//! nonnegative normal gap, allowing only scale-relative arithmetic roundoff.
//! Distances remain f64 reference units until native/canvas rasterization.
//! Routing applies its explicit minimum/snap/blocker policy after this geometry.
use super::{Side, portal::edge};
use oink_foundation::Rect;

pub fn tolerance(rects: &[Rect]) -> f64 {
    rects.iter().map(|r| r.w.max(r.h)).fold(1e-100, f64::max) * 1e-10
}

/// A source-edge interval in absolute along-edge coordinates.
#[derive(Clone, Copy, Debug)]
pub struct Facing {
    pub gap: f64,
    pub from: f64,
    pub to: f64,
}

pub fn facing(a: Rect, b: Rect, side: Side, eps: f64) -> Option<Facing> {
    let (at, lo, hi) = edge(&a, side);
    let (bt, blo, bhi) = edge(&b, side.opposite());
    let gap = match side {
        Side::Right | Side::Bottom => bt - at,
        Side::Left | Side::Top => at - bt,
    };
    let (from, to) = (lo.max(blo), hi.min(bhi));
    (gap >= -eps && to - from > eps).then_some(Facing {
        gap: gap.max(0.),
        from,
        to,
    })
}

/// Collision uses the same length tolerance as facing, never an area tolerance
/// or a display-pixel allowance that can hide real penetration at a short seam.
pub fn overlaps(a: Rect, b: Rect, eps: f64) -> bool {
    a.right().min(b.right()) - a.x.max(b.x) > eps && a.bottom().min(b.bottom()) - a.y.max(b.y) > eps
}

/// Every directed pair, followed by nearest-visible intervals (including walls).
/// Source-relative endpoints omit normal gap distance, precisely the invariant
/// compaction may preserve. The target index breaks equal-distance ties.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct EdgeProjection {
    pub kind: &'static str,
    pub screen: usize,
    /// Left, right, top, bottom (also the TypeScript order).
    pub side: usize,
    pub from: f64,
    pub to: f64,
    pub target: Option<usize>,
}
pub const SIDES: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

pub fn edge_projections(rects: &[Rect], eps: f64) -> Vec<EdgeProjection> {
    (0..rects.len())
        .flat_map(|i| source_projections(rects, i, eps))
        .collect()
}

pub fn source_projections(rects: &[Rect], i: usize, eps: f64) -> Vec<EdgeProjection> {
    let mut result = Vec::new();
    let a = rects[i];
    for (side_index, side) in SIDES.into_iter().enumerate() {
        let (_, lo, hi) = edge(&a, side);
        let candidates: Vec<_> = rects
            .iter()
            .enumerate()
            .filter_map(|(j, &b)| (i != j).then(|| facing(a, b, side, eps)).flatten().map(|f| (j, f)))
            .collect();
        for &(j, f) in &candidates {
            result.push(EdgeProjection {
                kind: "pair",
                screen: i,
                side: side_index,
                from: f.from - lo,
                to: f.to - lo,
                target: Some(j),
            });
        }
        let mut cuts = vec![lo, hi];
        for (_, f) in &candidates {
            cuts.extend([f.from, f.to]);
        }
        cuts.sort_by(f64::total_cmp);
        cuts.dedup();
        let mut last: Option<usize> = None;
        for cut in cuts.windows(2) {
            let (from, to) = (cut[0], cut[1]);
            if to - from <= eps {
                continue;
            }
            let mid = (from + to) / 2.;
            let target = candidates
                .iter()
                .filter(|(_, f)| f.from <= mid && mid <= f.to)
                .min_by(|a, b| a.1.gap.total_cmp(&b.1.gap).then(a.0.cmp(&b.0)))
                .map(|(j, _)| *j);
            if let Some(index) = last
                && result[index].target == target
            {
                result[index].to = to - lo;
            } else {
                last = Some(result.len());
                result.push(EdgeProjection {
                    kind: "visible",
                    screen: i,
                    side: side_index,
                    from: from - lo,
                    to: to - lo,
                    target,
                });
            }
        }
    }
    result
}

pub fn same_projections(a: &[EdgeProjection], b: &[EdgeProjection], eps: f64) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(p, q)| {
            p.kind == q.kind
                && p.screen == q.screen
                && p.side == q.side
                && p.target == q.target
                && (p.from - q.from).abs() <= eps
                && (p.to - q.to).abs() <= eps
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corners_penetration_and_short_contacts_have_one_definition() {
        let a = Rect::new(0., 0., 100., 100.);
        let eps = tolerance(&[a]);
        assert!(facing(a, Rect::new(100., 100., 100., 100.), Side::Right, eps).is_none());
        assert!(facing(a, Rect::new(99.75, 0., 100., 100.), Side::Right, eps).is_none());
        assert!(facing(a, Rect::new(100. - 1e-9, 99.99, 100., 100.), Side::Right, eps).is_some());
    }
}
