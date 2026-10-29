//! Edge selection and gesture lifetime; sampling/rules live in crate::damping.
use super::*;
use crate::damping::{DampingMeter, DampingProfile, nonnegative};
use oink_foundation::input::{EdgeResistanceRegion, ScreenCorner, ScreenEdge};

#[derive(Clone, Debug)]
pub(super) struct Resistance {
    screen: ScreenId,
    target: Option<ScreenId>,
    origin: DeviceId,
    normal: (f64, f64),
    rule: Option<usize>,
    profile: DampingProfile,
    meter: DampingMeter,
}

impl Router {
    /// Replace the platform snapshot; a real change invalidates the current push gesture.
    pub fn set_edge_resistance_regions(&mut self, regions: Vec<EdgeResistanceRegion>) {
        if self.edge_regions != regions {
            self.edge_regions = regions;
            self.resistance = None;
        }
    }

    /// Select WHERE independently from WHAT; first matching manual segment wins on every OS.
    fn damping_rule(
        &self,
        p: &ScreenPos,
        n: (f64, f64),
        target: Option<&ScreenId>,
    ) -> Option<(Option<usize>, DampingProfile)> {
        if !self.cfg.settings.damping.rules.is_empty() {
            let config = &self.cfg.settings.damping;
            let edge = screen_edge(n);
            let b = self.displays.iter().find(|d| d.key == p.screen.display)?.bounds;
            let fraction = if n.0 != 0.0 {
                (p.local.y - b.y) / b.h
            } else {
                (p.local.x - b.x) / b.w
            };
            for (index, rule) in config.rules.iter().enumerate() {
                if rule.segment.matches(
                    p.screen.device,
                    target.map_or(self.me(), |s| s.device),
                    &p.screen.display,
                    edge,
                    fraction,
                ) {
                    return Some((Some(index), rule.profile));
                }
            }
        }
        (target.is_some_and(|screen| screen != &p.screen && self.protected_entry(screen))
            || (self.cfg.os == Os::MacOS && self.reserved_edge(p, n)))
        .then(|| (None, self.cfg.settings.edge_damping_profile()))
    }

    fn reserved_edge(&self, p: &ScreenPos, n: (f64, f64)) -> bool {
        let Some(region) = self.edge_regions.iter().find(|r| r.display == p.screen.display) else {
            return false;
        };
        let edge = screen_edge(n);
        if region.edges.contains(&edge) {
            return true;
        }
        // Use the tangent's physical scale: calibrated axes need not have equal plane/point.
        let Some((b, k)) = self.resistance_geometry(p, (n.1, n.0)) else {
            return false;
        };
        let fraction = nonnegative(self.cfg.settings.edge_resistance_corner_fraction, 0.005);
        region.corners.iter().any(|corner| {
            let distance = match (edge, corner) {
                (ScreenEdge::Left | ScreenEdge::Right, ScreenCorner::TopLeft | ScreenCorner::TopRight)
                    if (edge == ScreenEdge::Left) == (*corner == ScreenCorner::TopLeft) =>
                {
                    p.local.y - b.y
                }
                (ScreenEdge::Left | ScreenEdge::Right, ScreenCorner::BottomLeft | ScreenCorner::BottomRight)
                    if (edge == ScreenEdge::Left) == (*corner == ScreenCorner::BottomLeft) =>
                {
                    b.bottom() - p.local.y
                }
                (ScreenEdge::Top | ScreenEdge::Bottom, ScreenCorner::TopLeft | ScreenCorner::BottomLeft)
                    if (edge == ScreenEdge::Top) == (*corner == ScreenCorner::TopLeft) =>
                {
                    p.local.x - b.x
                }
                (ScreenEdge::Top | ScreenEdge::Bottom, ScreenCorner::TopRight | ScreenCorner::BottomRight)
                    if (edge == ScreenEdge::Top) == (*corner == ScreenCorner::TopRight) =>
                {
                    b.right() - p.local.x
                }
                _ => return false,
            };
            distance >= 0.0 && distance * k <= fraction
        })
    }

    /// Geometry uses the calibrated physical rectangle, not the motion gain or the destination screen.
    fn resistance_geometry(&self, p: &ScreenPos, normal: (f64, f64)) -> Option<(Rect, f64)> {
        let b = self.displays.iter().find(|d| d.key == p.screen.display)?.bounds;
        let a = self
            .layout
            .to_plane(&ScreenPos::new(p.screen.clone(), Point::new(b.x, b.y)))?;
        let z = self
            .layout
            .to_plane(&ScreenPos::new(p.screen.clone(), Point::new(b.right(), b.bottom())))?;
        let (w, h) = ((z.x - a.x).abs(), (z.y - a.y).abs());
        let diagonal = w.hypot(h);
        let k = if normal.0 != 0.0 { w / b.w } else { h / b.h } / diagonal;
        (k.is_finite() && k > 0.0).then_some((b, k))
    }

    /// Keep a gesture on the same active portal or protected gap edge segment.
    /// Tangential motion does not add energy.
    pub(super) fn retain_resistance(&mut self, p: &ScreenPos, origin: DeviceId, delta: (f64, f64)) {
        if self.pointer_lock_escape_held() {
            self.resistance = None;
            return;
        }
        let Some(r) = &self.resistance else { return };
        let n = r.normal;
        let valid = p.screen == r.screen
            && self
                .damping_rule(p, n, r.target.as_ref())
                .is_some_and(|(rule, _)| rule == r.rule)
            && origin == r.origin
            && !self.leave_blocked()
            && self
                .resistance_geometry(p, n)
                .is_some_and(|(b, _)| edge_distance(b, p.local, n) <= EDGE_UNITS)
            && match &r.target {
                Some(target) => {
                    self.input_ready(target.device)
                        && self
                            .layout
                            .exit_through_edge(p, (n.0 * 2.0, n.1 * 2.0))
                            .is_some_and(|dst| dst.screen == *target)
                }
                None => self.gap_allowed() && self.cfg.os == Os::MacOS && self.reserved_edge(p, n),
            };
        if !valid {
            self.resistance = None;
            return;
        }
        let inward = delta.0 * n.0 + delta.1 * n.1;
        if inward < 0.0 {
            let k = self.resistance_geometry(p, n).unwrap().1;
            let r = self.resistance.as_mut().unwrap();
            r.meter.retreat(-inward * k);
            if r.meter.depth() <= 0.0 {
                self.resistance = None;
            }
        }
    }

    /// true = may hand off. DnD/blocked handoffs are handled before this function.
    pub(super) fn push_resistance(
        &mut self,
        origin: DeviceId,
        at: &ScreenPos,
        before: Option<&ScreenPos>,
        dst: &ScreenPos,
        delta: (f64, f64),
        now: Micros,
    ) -> bool {
        self.push_edge_resistance(origin, at, before, Some(dst), delta, now)
    }

    /// Gap travel has no selected destination. Reuse the platform's protected
    /// segments and damping profile; manual rules do not create system edges.
    pub(super) fn push_gap_resistance(
        &mut self,
        origin: DeviceId,
        at: &ScreenPos,
        before: &ScreenPos,
        delta: (f64, f64),
        now: Micros,
    ) -> bool {
        if self.cfg.os != Os::MacOS || !self.exit_normal(at, delta).is_some_and(|n| self.reserved_edge(at, n)) {
            self.resistance = None;
            return true;
        }
        self.push_edge_resistance(origin, at, Some(before), None, delta, now)
    }

    fn push_edge_resistance(
        &mut self,
        origin: DeviceId,
        at: &ScreenPos,
        before: Option<&ScreenPos>,
        dst: Option<&ScreenPos>,
        delta: (f64, f64),
        now: Micros,
    ) -> bool {
        if self.pointer_lock_escape_held() {
            self.resistance = None;
            return true;
        }
        let Some(normal) = self.exit_normal(at, delta) else {
            return true;
        };
        let Some((rule, profile)) = self.damping_rule(at, normal, dst.map(|p| &p.screen)) else {
            self.resistance = None;
            return true;
        };
        let Some((bounds, k)) = self.resistance_geometry(at, normal) else {
            return true;
        };
        // An internal system seam cannot activate Dock/menu/hot corners.
        if !dst.is_some_and(|p| self.protected_entry(&p.screen))
            && self
                .displays
                .iter()
                .any(|d| d.key != at.screen.display && native_seam(bounds, d.bounds, at.local, normal))
        {
            return true;
        }
        let target = dst.map(|p| p.screen.clone());
        let same = self.resistance.as_ref().is_some_and(|r| {
            r.screen == at.screen && r.target == target && r.origin == origin && r.normal == normal && r.rule == rule
        });
        if !same {
            self.resistance = Some(Resistance {
                screen: at.screen.clone(),
                target,
                origin,
                normal,
                rule,
                profile,
                meter: DampingMeter::new(now),
            });
        }
        let mut out = (delta.0 * normal.0 + delta.1 * normal.1).max(0.0);
        // The portion before reaching the edge is ordinary local movement, not push depth.
        if let Some(p) = before.filter(|p| p.screen == at.screen) {
            out = (out - (edge_distance(bounds, p.local, normal) - EDGE_UNITS).max(0.0)).max(0.0);
        }
        if out == 0.0 {
            return false;
        }
        let resistance = self.resistance.as_mut().unwrap();
        let meter = &mut resistance.meter;
        let crossed = meter.push(out * k, now, resistance.profile);
        if crossed {
            tracing::debug!(
                depth = meter.depth(),
                energy = meter.energy(),
                ?normal,
                "edge resistance crossed"
            );
        }
        crossed
    }
}

/// Only touching facing boundaries form a native seam. Sampling a point beyond
/// the edge can jump a small real gap and incorrectly exempt it from damping.
/// Tangent intervals are half-open, like native display containment.
pub(super) fn native_seam(source: Rect, other: Rect, at: Point, normal: (f64, f64)) -> bool {
    let (boundary, facing, tangent, start, end) = if normal.0 > 0.0 {
        (
            source.right(),
            other.x,
            at.y,
            source.y.max(other.y),
            source.bottom().min(other.bottom()),
        )
    } else if normal.0 < 0.0 {
        (
            source.x,
            other.right(),
            at.y,
            source.y.max(other.y),
            source.bottom().min(other.bottom()),
        )
    } else if normal.1 > 0.0 {
        (
            source.bottom(),
            other.y,
            at.x,
            source.x.max(other.x),
            source.right().min(other.right()),
        )
    } else {
        (
            source.y,
            other.bottom(),
            at.x,
            source.x.max(other.x),
            source.right().min(other.right()),
        )
    };
    (boundary - facing).abs() <= 1e-6 && tangent >= start && tangent < end
}

fn edge_distance(b: Rect, p: Point, n: (f64, f64)) -> f64 {
    if n.0 > 0.0 {
        b.right() - p.x
    } else if n.0 < 0.0 {
        p.x - b.x
    } else if n.1 > 0.0 {
        b.bottom() - p.y
    } else {
        p.y - b.y
    }
}

fn screen_edge(n: (f64, f64)) -> ScreenEdge {
    match n {
        (x, _) if x < 0.0 => ScreenEdge::Left,
        (x, _) if x > 0.0 => ScreenEdge::Right,
        (_, y) if y < 0.0 => ScreenEdge::Top,
        _ => ScreenEdge::Bottom,
    }
}

#[cfg(test)]
mod seam_tests {
    use super::*;

    #[test]
    fn native_seams_require_touching_boundaries_and_half_open_tangent_overlap() {
        // Translate the same geometry across both axes, including fractional
        // negative origins, and rotate the touching side through all four edges.
        for origin in [Point::new(0., 0.), Point::new(-1920.25, -1080.5)] {
            let source = Rect::new(origin.x, origin.y, 200., 100.);
            for (normal, touching, first, last) in [
                (
                    (0., 1.),
                    Rect::new(origin.x + 50., source.bottom(), 100., 80.),
                    Point::new(origin.x + 50., source.bottom() - 0.02),
                    Point::new(origin.x + 150., source.bottom() - 0.02),
                ),
                (
                    (0., -1.),
                    Rect::new(origin.x + 50., source.y - 80., 100., 80.),
                    Point::new(origin.x + 50., source.y),
                    Point::new(origin.x + 150., source.y),
                ),
                (
                    (1., 0.),
                    Rect::new(source.right(), origin.y + 25., 80., 50.),
                    Point::new(source.right() - 0.02, origin.y + 25.),
                    Point::new(source.right() - 0.02, origin.y + 75.),
                ),
                (
                    (-1., 0.),
                    Rect::new(source.x - 80., origin.y + 25., 80., 50.),
                    Point::new(source.x, origin.y + 25.),
                    Point::new(source.x, origin.y + 75.),
                ),
            ] {
                assert!(native_seam(source, touching, first, normal));
                assert!(!native_seam(source, touching, last, normal));
                for gap in [0.25, 1., 1.5, 2.] {
                    let other = Rect::new(
                        touching.x + normal.0 * gap,
                        touching.y + normal.1 * gap,
                        touching.w,
                        touching.h,
                    );
                    assert!(
                        !native_seam(source, other, first, normal),
                        "gap={gap}, normal={normal:?}"
                    );
                }
            }
        }
    }
}
