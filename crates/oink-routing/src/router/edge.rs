//! One exit identity from topology, shared by drag pressure and edge resistance.
use super::*;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ExitEdge {
    pub source: ScreenId,
    pub normal: (f64, f64),
    portal: Option<Portal>,
}
#[derive(Clone, Debug, PartialEq)]
struct Portal {
    target: ScreenId,
    channel: (f64, f64),
    zone: Option<(f64, f64)>,
}
impl ExitEdge {
    #[cfg(test)]
    pub fn fixture(source: ScreenId, normal: (f64, f64)) -> Self {
        Self {
            source,
            normal,
            portal: None,
        }
    }

    pub fn contains(&self, b: Rect, p: Point) -> bool {
        let n = self.normal;
        if n.0 < 0. {
            p.x < b.x + EDGE_UNITS
        } else if n.0 > 0. {
            p.x >= b.right() - EDGE_UNITS
        } else if n.1 < 0. {
            p.y < b.y + EDGE_UNITS
        } else {
            p.y >= b.bottom() - EDGE_UNITS
        }
    }
}
impl Router {
    /// Preserve the virtual parked point when macOS reports the same outer
    /// edge with a different inward clamp. Otherwise set_cursor would retire
    /// the portal's resistance on every remote batch, so it never accumulates.
    pub(super) fn target_readback_constrained_point(&self, actual: Point) -> Option<Point> {
        // Windows receipts already floor + normalize + decode SendInput's
        // exact pixel (including right/bottom -1 and negative-display edges),
        // so their OS edge clamp is Expected before reaching this path. macOS
        // can instead return an inward fractional/last-pixel clamp different
        // from its submitted HID/CG endpoint. Do not widen Windows warp matching.
        if self.cfg.os != Os::MacOS || self.system_clip_active() {
            return None;
        }
        let from = self.pointer.as_ref()?;
        let display = self.displays.iter().find(|d| d.key == from.screen.display)?;
        let b = display.bounds;
        if !b.contains(actual) || !b.contains(from.local) {
            return None;
        }
        // Match confinement per axis: preserve the parked normal, but adopt the
        // observed tangent. Sliding along an edge must not reset its pressure.
        const QUANTUM: f64 = 1. / 256.;
        for (normal, expected_distance, actual_distance) in [
            ((-1., 0.), from.local.x - b.x, actual.x - b.x),
            ((1., 0.), b.right() - from.local.x, b.right() - actual.x),
            ((0., -1.), from.local.y - b.y, actual.y - b.y),
            ((0., 1.), b.bottom() - from.local.y, b.bottom() - actual.y),
        ] {
            if expected_distance > QUANTUM || actual_distance > EDGE_UNITS {
                continue;
            }
            let constrained = Point::new(
                if normal.0 != 0. { from.local.x } else { actual.x },
                if normal.1 != 0. { from.local.y } else { actual.y },
            );
            let at = ScreenPos::new(from.screen.clone(), constrained);
            if self
                .displays
                .iter()
                .any(|d| d.key != display.key && resistance::native_seam(b, d.bounds, constrained, normal))
            {
                continue;
            }
            if self
                .layout
                .first_portal_native(&at, normal)
                .is_some_and(|p| p.to.screen.device != self.me())
            {
                return Some(constrained);
            }
        }
        None
    }

    pub(super) fn exit_edge(&self, from: &ScreenPos, delta: (f64, f64)) -> Option<ExitEdge> {
        if let Some(portal) = self.layout.first_portal_native(from, delta) {
            use oink_display::layout::Side;
            let normal = match portal.side {
                Side::Left => (-1., 0.),
                Side::Right => (1., 0.),
                Side::Top => (0., -1.),
                Side::Bottom => (0., 1.),
            };
            return Some(ExitEdge {
                source: from.screen.clone(),
                normal,
                portal: Some(Portal {
                    target: ScreenId::new(portal.to.screen.device, portal.to.screen.display),
                    channel: portal.along,
                    zone: portal.trigger_along,
                }),
            });
        }
        // Simple layouts and protected walls have no portal metadata. Their
        // rectangle exit remains the normal; production portals always use the
        // topology's decision after wall sliding and corner selection above.
        let normal = self.rectangle_exit_normal(from, delta)?;
        Some(ExitEdge {
            source: from.screen.clone(),
            normal,
            portal: None,
        })
    }
    fn rectangle_exit_normal(&self, from: &ScreenPos, delta: (f64, f64)) -> Option<(f64, f64)> {
        let b = self
            .displays
            .iter()
            .find(|d| d.key == from.screen.display)
            .map(|d| d.bounds)?;
        let p = from.local;
        let (dx, dy) = delta;
        // Fraction of the remaining native movement before each rectangle edge.
        let tx = if dx > 0.0 {
            Some(((b.right() - p.x) / dx).max(0.0))
        } else if dx < 0.0 {
            Some(((p.x - b.x) / -dx).max(0.0))
        } else {
            None
        };
        let ty = if dy > 0.0 {
            Some(((b.bottom() - p.y) / dy).max(0.0))
        } else if dy < 0.0 {
            Some(((p.y - b.y) / -dy).max(0.0))
        } else {
            None
        };
        let horizontal = match (tx, ty) {
            (Some(a), Some(b)) => a < b || (a == b && dx.abs() >= dy.abs()),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return None,
        };
        Some(if horizontal {
            (dx.signum(), 0.0)
        } else {
            (0.0, dy.signum())
        })
    }
}
