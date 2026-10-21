//! Physical-screen projections. Layout reference units, never cursor gain,
//! determine their extent. Nothing in this module changes OS display positions.

use oink_foundation::{DeviceId, Point, Rect};

use crate::layout::{EffectiveLayout, Layout, OwnerLayoutPreferences, ScreenId, Side};
use std::collections::BTreeMap;

/// A shallow slice of the neighbouring rectangle is translated across the seam.
pub const DEPTH: f64 = 14.0 / 686.0;
pub const MIN_SIZE: f64 = 50.0 / 686.0;
pub const MAX_SIZE: f64 = 4000.0 / 686.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Projection {
    pub host: ScreenId,
    pub neighbour: ScreenId,
    pub side: Side,
    /// Entire neighbouring rectangle after the shallow inward translation, in
    /// host global coordinates. Round this rectangle *before* clipping it.
    pub rect: Rect,
    /// Physical host boundary, further restricted to the visible portal segment.
    pub clip: Rect,
    pub visible: Rect,
    pub reference_per_native_unit: f64,
}

fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let (x, y) = (a.x.max(b.x), a.y.max(b.y));
    let (w, h) = (a.right().min(b.right()) - x, a.bottom().min(b.bottom()) - y);
    (w > 0.0 && h > 0.0).then_some(Rect::new(x, y, w, h))
}

/// A dimension annotation along a real edge, normalized from left/top to right/bottom.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeSpan {
    pub side: Side,
    pub neighbour: ScreenId,
    pub a: f64,
    pub b: f64,
}

/// Orthogonal projection of the nearest rectangle at each point on an edge.
/// Gaps are allowed. A nearer rectangle occludes only its covered interval;
/// corner contact and rectangles overlapping the host are not candidates.
/// Same-computer screens use the same visibility and annotation geometry.
pub fn edge_spans(host: Rect, others: &[(ScreenId, Rect)]) -> Vec<EdgeSpan> {
    use crate::layout::facing::{SIDES, source_projections, tolerance};
    let rects: Vec<_> = std::iter::once(host).chain(others.iter().map(|(_, r)| *r)).collect();
    let mut spans: Vec<_> = source_projections(&rects, 0, tolerance(&rects))
        .into_iter()
        .filter(|p| p.kind == "visible" && p.target.is_some())
        .map(|p| {
            let side = SIDES[p.side];
            let length = if side.is_vertical() { host.h } else { host.w };
            EdgeSpan {
                side,
                neighbour: others[p.target.unwrap() - 1].0.clone(),
                a: p.from / length,
                b: p.to / length,
            }
        })
        .collect();
    // Retain the clockwise annotation paint order; facing decisions use SIDES.
    spans.sort_by_key(|s| match s.side {
        Side::Top => 0,
        Side::Right => 1,
        Side::Bottom => 2,
        Side::Left => 3,
    });
    spans
}

/// Preview geometry is derived from every member's rigid computer transform.
/// It is independent of pointer wall extension, minimum portals and snapping.
/// `affected` optionally restricts arranging previews in both directions.
pub fn compute(
    layout: &EffectiveLayout,
    device: DeviceId,
    online: impl Fn(&DeviceId) -> bool,
    affected: Option<&[ScreenId]>,
) -> Vec<Projection> {
    let screens: Vec<_> = layout
        .screens()
        .iter()
        .filter(|s| s.is_valid() && s.enabled && online(&s.id.device))
        .collect();
    let mut out = Vec::new();
    for host in screens.iter().filter(|s| s.id.device == device) {
        let others = screens
            .iter()
            .filter(|s| s.id != host.id)
            .map(|s| (s.id.clone(), s.rect))
            .collect::<Vec<_>>();
        let (sx, sy) = host.physical_scale();
        for span in edge_spans(host.rect, &others) {
            if affected.is_some_and(|ids| !ids.contains(&host.id) && !ids.contains(&span.neighbour)) {
                continue;
            }
            let neighbour = screens
                .iter()
                .find(|s| s.id == span.neighbour)
                .expect("candidate present");
            let side = span.side;
            let depth = DEPTH.min(if side.is_vertical() { host.rect.w } else { host.rect.h } * 0.15);
            let mut r = neighbour.rect;
            match side {
                Side::Left => r.x = host.rect.x + depth - r.w,
                Side::Right => r.x = host.rect.right() - depth,
                Side::Top => r.y = host.rect.y + depth - r.h,
                Side::Bottom => r.y = host.rect.bottom() - depth,
            }
            let p = host.plane_to_local(Point::new(r.x, r.y));
            let rect = Rect::new(p.x, p.y, r.w / sx, r.h / sy);
            let mut clip = host.local;
            if side.is_vertical() {
                clip.y += span.a * host.local.h;
                clip.h = (span.b - span.a) * host.local.h;
            } else {
                clip.x += span.a * host.local.w;
                clip.w = (span.b - span.a) * host.local.w;
            }
            if let Some(visible) = intersection(rect, clip) {
                out.push(Projection {
                    host: host.id.clone(),
                    neighbour: neighbour.id.clone(),
                    side,
                    rect,
                    clip,
                    visible,
                    reference_per_native_unit: if side.is_vertical() { sy } else { sx },
                });
            }
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Start,
    Middle,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitRegion {
    pub handle: Handle,
    pub rect: Rect,
}

impl Projection {
    /// Separate small handle windows can use these same rectangles. The outer
    /// three native units stay click-through so native edge crossing / hot corners
    /// retain their existing path. A clipped-away end has no resize handle.
    pub fn handles(&self) -> Vec<HitRegion> {
        // Native same-computer travel stays entirely with the OS. Its labels
        // must never create an input window or a layout-edit gesture.
        if self.host.device == self.neighbour.device {
            return vec![];
        }
        let v = self.visible;
        let vertical = self.side.is_vertical();
        let (lo, hi, full_lo, full_hi) = if vertical {
            (v.y, v.bottom(), self.rect.y, self.rect.bottom())
        } else {
            (v.x, v.right(), self.rect.x, self.rect.right())
        };
        let span = hi - lo;
        let end = 20.0_f64.min(span / 4.0);
        let middle = 72.0_f64.min(span / 3.0);
        let inset = 3.0;
        let band = match self.side {
            Side::Left => Rect::new(v.x + inset, v.y, (v.w - inset).min(20.0), v.h),
            Side::Right => Rect::new((v.right() - inset - 20.0).max(v.x), v.y, (v.w - inset).min(20.0), v.h),
            Side::Top => Rect::new(v.x, v.y + inset, v.w, (v.h - inset).min(20.0)),
            Side::Bottom => Rect::new(v.x, (v.bottom() - inset - 20.0).max(v.y), v.w, (v.h - inset).min(20.0)),
        };
        if band.w <= 0.0 || band.h <= 0.0 {
            return vec![];
        }
        let region = |handle, a, length| HitRegion {
            handle,
            rect: if vertical {
                Rect::new(band.x, a, band.w, length)
            } else {
                Rect::new(a, band.y, length, band.h)
            },
        };
        let mut out = Vec::new();
        if (lo - full_lo).abs() < 1e-7 {
            out.push(region(Handle::Start, lo, end));
        }
        out.push(region(Handle::Middle, (lo + hi - middle) / 2.0, middle));
        if (hi - full_hi).abs() < 1e-7 {
            out.push(region(Handle::End, hi - end, end));
        }
        out
    }

    pub fn hit_test(&self, p: Point) -> Option<Handle> {
        self.handles()
            .into_iter()
            .find(|h| h.rect.contains(p))
            .map(|h| h.handle)
    }

    /// Absolute gesture mapping from its initial layout, never accumulated from
    /// rounded frames. End resize keeps the opposite end and facing seam fixed,
    /// preserves aspect ratio, and has the editor's Manual size semantics.
    pub fn edit(&self, initial: &Layout, handle: Handle, delta_units: f64) -> Option<Layout> {
        self.edit_with_preferences(initial, &BTreeMap::new(), handle, delta_units)
    }

    /// Visibility controls collision and size constraints, while every member
    /// (including hidden screens) follows the resulting rigid group transform.
    pub fn edit_with_preferences(
        &self,
        initial: &Layout,
        owners: &BTreeMap<DeviceId, OwnerLayoutPreferences>,
        handle: Handle,
        delta_units: f64,
    ) -> Option<Layout> {
        if self.host.device == self.neighbour.device || !delta_units.is_finite() {
            return None;
        }
        let mut next = initial.clone();
        let source = initial.screen(&self.neighbour)?;
        let enabled = |s: &crate::layout::ScreenPlacement| {
            owners.get(&s.id.device).cloned().unwrap_or_default().screen(s).enabled
        };
        if !enabled(source) {
            return None;
        }
        let min = initial
            .screens
            .iter()
            .filter(|s| s.id.device == source.id.device && enabled(s))
            .map(|s| s.rect.w.min(s.rect.h))
            .fold(f64::INFINITY, f64::min);
        let max = initial
            .screens
            .iter()
            .filter(|s| s.id.device == source.id.device && enabled(s))
            .map(|s| s.rect.w.max(s.rect.h))
            .fold(0.0, f64::max);
        if !source.is_valid() || MIN_SIZE / min > MAX_SIZE / max {
            return None;
        }
        let r = source.rect;
        let d = delta_units * self.reference_per_native_unit;
        if !d.is_finite() {
            return None;
        }
        let others: Vec<_> = initial
            .screens
            .iter()
            .filter(|s| s.id.device != source.id.device && s.is_valid() && enabled(s))
            .map(|s| s.rect)
            .collect();
        let group = initial
            .screens
            .iter()
            .filter(|s| s.id.device == source.id.device && enabled(s))
            .collect::<Vec<_>>();
        let fits = |rect: Rect| {
            let k = rect.w / r.w;
            [rect.x, rect.y, rect.w, rect.h].iter().all(|n| n.is_finite())
                && group.iter().all(|s| {
                    let moved = Rect::new(
                        rect.x + (s.rect.x - r.x) * k,
                        rect.y + (s.rect.y - r.y) * k,
                        s.rect.w * k,
                        s.rect.h * k,
                    );
                    others.iter().all(|o| {
                        !crate::layout::facing::overlaps(moved, *o, crate::layout::facing::tolerance(&[moved, *o]))
                    })
                })
        };
        let vertical = self.side.is_vertical();
        if handle == Handle::Middle {
            let moved = |delta| {
                if vertical {
                    Rect::new(r.x, r.y + delta, r.w, r.h)
                } else {
                    Rect::new(r.x + delta, r.y, r.w, r.h)
                }
            };
            // The free canvas can resolve overlap on either axis. A native edge
            // handle has only one axis: retain its seam and choose the nearest
            // legal along-edge boundary instead of pushing it off this edge.
            let delta = std::iter::once(d)
                .chain(std::iter::once(0.0))
                .chain(group.iter().flat_map(|member| {
                    others.iter().flat_map(|other| {
                        if vertical {
                            [other.y - member.rect.bottom(), other.bottom() - member.rect.y]
                        } else {
                            [other.x - member.rect.right(), other.right() - member.rect.x]
                        }
                    })
                }))
                .filter(|delta| fits(moved(*delta)))
                .min_by(|a, b| (a - d).abs().total_cmp(&(b - d).abs()))
                .unwrap_or(0.0);
            for target in next.screens.iter_mut().filter(|s| s.id.device == source.id.device) {
                target.translate(if vertical { 0.0 } else { delta }, if vertical { delta } else { 0.0 });
            }
        } else {
            let length = if vertical { r.h } else { r.w };
            let sign = if handle == Handle::Start { -1.0 } else { 1.0 };
            let minimum = MIN_SIZE / min;
            let mut k = ((length + sign * d) / length).clamp(minimum, MAX_SIZE / max);
            let anchored = |k: f64| {
                let (w, h) = (r.w * k, r.h * k);
                let x = if self.side == Side::Left || (!vertical && handle == Handle::Start) {
                    r.right() - w
                } else {
                    r.x
                };
                let y = if self.side == Side::Top || (vertical && handle == Handle::Start) {
                    r.bottom() - h
                } else {
                    r.y
                };
                Rect::new(x, y, w, h)
            };
            // Every member's four bounds are affine in scale. A remote
            // member can hit an obstacle and later move past it, so endpoint
            // fit is not monotonic. Intersect the four strict overlap
            // inequalities over the complete path, and stop at its first hit.
            let wanted = anchored(k);
            let mut fraction = 1.0_f64;
            for member in &group {
                let end = Rect::new(
                    wanted.x + (member.rect.x - r.x) * k,
                    wanted.y + (member.rect.y - r.y) * k,
                    member.rect.w * k,
                    member.rect.h * k,
                );
                for obstacle in &others {
                    if let Some(hit) = first_overlap_fraction(member.rect, end, *obstacle) {
                        fraction = fraction.min(hit);
                    }
                }
            }
            if fraction < 1.0 {
                if !fits(r) {
                    return None;
                }
                k = 1.0 + (k - 1.0) * fraction;
            }

            let rect = anchored(k);
            if (rect.w - r.w).abs() < 1e-8 / 686.0 && (rect.h - r.h).abs() < 1e-8 / 686.0 {
                return Some(initial.clone());
            }
            let (origin, scale) = initial.device_transform(source.id.device)?;
            next.place_device(
                source.id.device,
                Point::new(rect.x + (origin.x - r.x) * k, rect.y + (origin.y - r.y) * k),
                scale * k,
                true,
            );
        }
        Some(next)
    }
}

/// First contact on an affine rectangle path. The overlap interval is the
/// intersection of four linear inequalities; checking only either endpoint
/// misses an obstacle that a translated/scaled group member passes through.
fn first_overlap_fraction(from: Rect, to: Rect, obstacle: Rect) -> Option<f64> {
    // Stop at geometric contact. A tolerance derived from the requested end
    // scale could permit penetration at the smaller, actual stopping scale.
    let eps = 0.0;
    let (mut enter, mut leave) = (0.0_f64, 1.0_f64);
    for (start, end) in [
        (from.right() - obstacle.x, to.right() - obstacle.x),
        (obstacle.right() - from.x, obstacle.right() - to.x),
        (from.bottom() - obstacle.y, to.bottom() - obstacle.y),
        (obstacle.bottom() - from.y, obstacle.bottom() - to.y),
    ] {
        let slope = end - start;
        if slope == 0.0 {
            if start <= eps {
                return None;
            }
        } else {
            let contact = (eps - start) / slope;
            if slope > 0.0 {
                enter = enter.max(contact);
            } else {
                leave = leave.min(contact);
            }
        }
    }
    (enter < leave).then_some(enter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{ScreenPlacement, SizeSource};

    fn screen(device: u8, key: &str, rect: Rect, local: Rect) -> ScreenPlacement {
        ScreenPlacement {
            id: ScreenId::new(DeviceId([device; 32]), key),
            name: key.into(),
            rect,
            local,
            size_source: SizeSource::Reported,

            primary: true,
            builtin: false,
        }
    }
    fn fixture(side: Side) -> Layout {
        let r = match side {
            Side::Left => Rect::new(-200.0 / 686.0, 50.0 / 686.0, 200.0 / 686.0, 100.0 / 686.0),
            Side::Right => Rect::new(400.0 / 686.0, 50.0 / 686.0, 200.0 / 686.0, 100.0 / 686.0),
            Side::Top => Rect::new(50.0 / 686.0, -100.0 / 686.0, 200.0 / 686.0, 100.0 / 686.0),
            Side::Bottom => Rect::new(50.0 / 686.0, 300.0 / 686.0, 200.0 / 686.0, 100.0 / 686.0),
        };
        Layout {
            screens: vec![
                screen(
                    1,
                    "host",
                    Rect::new(0.0 / 686.0, 0.0 / 686.0, 400.0 / 686.0, 300.0 / 686.0),
                    Rect::new(-800.0, 100.0, 1600.0, 1200.0),
                ),
                screen(2, "peer", r, Rect::new(0.0, 0.0, 1000.0, 500.0)),
            ],
            ..Layout::default()
        }
    }
    fn first(l: &Layout) -> Projection {
        compute(&l.effective(&Default::default()), DeviceId([1; 32]), |_| true, None).remove(0)
    }

    #[test]
    fn screenshot_geometry_projects_every_exposed_pair_on_native_screens() {
        let rects = [
            Rect::new(493., 0., 741., 416.8125),
            Rect::new(0., 416.8125 - 1e-8, 604., 604. * 1117. / 1728.),
            Rect::new(604., 472.8125, 604., 604. * 1117. / 1728.),
        ];
        let l = Layout {
            screens: rects
                .into_iter()
                .enumerate()
                .map(|(i, r)| {
                    screen(
                        i as u8 + 1,
                        "screen",
                        Rect::new(r.x / 4096., r.y / 4096., r.w / 4096., r.h / 4096.),
                        if i == 0 {
                            Rect::new(0., 0., 3840., 2160.)
                        } else {
                            Rect::new(0., 0., 1728., 1117.)
                        },
                    )
                })
                .collect(),
            ..Default::default()
        };
        let effective = l.effective(&Default::default());
        for device in 1..=3 {
            let p = compute(&effective, DeviceId([device; 32]), |_| true, None);
            assert_eq!(p.len(), 2, "both other computers project onto each host");
            assert!(p.iter().all(|p| p.visible.w > 0. && p.visible.h > 0.));
        }
        let upper = compute(&effective, DeviceId([1; 32]), |_| true, None);
        let short = upper.iter().find(|p| p.neighbour.device == DeviceId([2; 32])).unwrap();
        assert_eq!(short.side, Side::Bottom);
        assert!((short.clip.w - 111. / 741. * 3840.).abs() < 1e-8);
    }

    #[test]
    fn short_lower_left_contact_survives_roundoff() {
        // Screenshot proportions; native resolutions are independent of canvas size.
        for scale in [1., 1. / 4096.] {
            let rects = [
                Rect::new(493., 0., 741., 416.8125),
                Rect::new(0., 416.8125 - 1e-8, 604., 604. * 1117. / 1728.),
                Rect::new(604., 472.8125, 604., 604. * 1117. / 1728.),
            ]
            .map(|r| Rect::new(r.x * scale, r.y * scale, r.w * scale, r.h * scale));
            let others = [
                (ScreenId::new(DeviceId([2; 32]), "lower-left"), rects[1]),
                (ScreenId::new(DeviceId([3; 32]), "lower-right"), rects[2]),
            ];
            let spans = edge_spans(rects[0], &others);
            assert!(
                spans
                    .iter()
                    .any(|s| s.side == Side::Bottom && s.neighbour == others[0].0),
                "short contact missing at {scale}"
            );
        }
    }

    #[test]
    fn four_edges_extent_depth_and_native_scale() {
        for side in Side::ALL {
            let l = fixture(side);
            let p = first(&l);
            assert_eq!(p.side, side);
            assert_eq!(p.rect.w, 800.0);
            assert_eq!(p.rect.h, 400.0);
            assert_eq!(p.reference_per_native_unit, 0.25 / 686.0);
            assert!((if side.is_vertical() { p.visible.w } else { p.visible.h } - 56.0).abs() < 1e-9);
            assert_eq!(
                if side.is_vertical() { p.visible.h } else { p.visible.w },
                if side.is_vertical() { 400.0 } else { 800.0 }
            );
        }
    }
    #[test]
    fn round_full_rectangle_then_clip_to_screen_and_occluding_segment() {
        let mut l = fixture(Side::Bottom);
        l.screens[1].rect.x = -50.0 / 686.0;
        let p = first(&l);
        assert_eq!(p.rect.x, -1000.0);
        assert_eq!(p.visible.x, -800.0);
        assert_eq!(p.visible.w, 600.0);
        assert!(!p.handles().iter().any(|h| h.handle == Handle::Start));
        // A closer display hides a more distant candidate completely.
        let mut farther = l.screens[1].clone();
        farther.id.display = "behind".into();
        farther.rect.y += 200.0 / 686.0;
        l.screens.push(farther);
        assert_eq!(
            compute(&l.effective(&Default::default()), DeviceId([1; 32]), |_| true, None).len(),
            1
        );
    }
    #[test]
    fn previews_are_bidirectional_online_and_per_monitor() {
        let mut l = fixture(Side::Right);
        let affected = [l.screens[1].id.clone()];
        assert_eq!(
            compute(
                &l.effective(&Default::default()),
                DeviceId([1; 32]),
                |_| true,
                Some(&affected)
            )
            .len(),
            1
        );
        assert_eq!(
            compute(
                &l.effective(&Default::default()),
                DeviceId([2; 32]),
                |_| true,
                Some(&affected)
            )
            .len(),
            1
        );
        assert!(
            compute(
                &l.effective(&Default::default()),
                DeviceId([1; 32]),
                |id| *id != DeviceId([2; 32]),
                None
            )
            .is_empty()
        );
        l.screens[1].id.device = DeviceId([1; 32]);
        let p = compute(&l.effective(&Default::default()), DeviceId([1; 32]), |_| true, None);
        assert_eq!(p.len(), 2, "both native screens annotate their neighbour");
        for p in p {
            assert_eq!(p.host.device, p.neighbour.device);
            assert!(
                p.handles().is_empty(),
                "same-machine annotations cannot intercept native crossing"
            );
            assert_eq!(p.hit_test(p.visible.center()), None);
            assert_eq!(p.edit(&l, Handle::Middle, 10.), None);
        }
    }
    #[test]
    fn hidden_local_screens_are_absent_and_do_not_occlude_remote_projections() {
        let mut l = fixture(Side::Right);
        let mut local = l.screens[1].clone();
        local.id = ScreenId::new(l.screens[0].id.device, "local-second");
        local.rect.x += 50. / 686.;
        l.screens[1].rect.x += 300. / 686.;
        local.primary = false;
        l.screens.push(local);
        let enabled = compute(&l.effective(&Default::default()), DeviceId([1; 32]), |_| true, None);
        assert!(enabled.iter().any(|p| p.neighbour.display == "local-second"));
        assert!(
            !enabled
                .iter()
                .any(|p| p.host.display == "host" && p.neighbour.display == "peer")
        );
        let mut owner = OwnerLayoutPreferences::default();
        owner.screens.insert(
            "local-second".into(),
            crate::layout::ScreenPreference {
                enabled: false,
                ..Default::default()
            },
        );
        let owners = BTreeMap::from([(DeviceId([1; 32]), owner)]);
        let hidden = compute(&l.effective(&owners), DeviceId([1; 32]), |_| true, None);
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].host.display, "host");
        assert_eq!(hidden[0].neighbour.display, "peer");
    }
    #[test]
    fn no_corner_projection_and_gain_does_not_change_geometry() {
        let mut l = fixture(Side::Right);
        let p = first(&l);
        let mut owners = std::collections::BTreeMap::new();
        owners.insert(
            DeviceId([1; 32]),
            crate::layout::OwnerLayoutPreferences {
                device_gain: 5.0,
                ..Default::default()
            },
        );
        assert_eq!(
            compute(&l.effective(&owners), DeviceId([1; 32]), |_| true, None).remove(0),
            p
        );
        l.screens[1].rect.y = 300.0 / 686.0;
        assert!(compute(&l.effective(&Default::default()), DeviceId([1; 32]), |_| true, None).is_empty());
    }
    #[test]
    fn only_handles_hit_and_outer_edge_remains_free() {
        for side in Side::ALL {
            let p = first(&fixture(side));
            for h in p.handles() {
                assert_eq!(p.hit_test(h.rect.center()), Some(h.handle));
            }
            let mut edge = p.visible.center();
            match side {
                Side::Left => edge.x = p.clip.x + 1.0,
                Side::Right => edge.x = p.clip.right() - 1.0,
                Side::Top => edge.y = p.clip.y + 1.0,
                Side::Bottom => edge.y = p.clip.bottom() - 1.0,
            }
            assert_eq!(p.hit_test(edge), None);
        }
    }
    #[test]
    fn even_fractionally_clipped_real_ends_have_no_handles() {
        let mut p = first(&fixture(Side::Top));
        p.visible.x = p.rect.x + 0.25;
        p.visible.w = p.rect.w - 0.5;
        assert_eq!(
            p.handles().iter().map(|h| h.handle).collect::<Vec<_>>(),
            [Handle::Middle]
        );
    }
    #[test]
    fn middle_move_preserves_size_and_uses_host_scale() {
        let l = fixture(Side::Right);
        let next = first(&l).edit(&l, Handle::Middle, 80.0).unwrap();
        assert_eq!(
            next.screens[1].rect,
            Rect::new(400.0 / 686.0, 70.0 / 686.0, 200.0 / 686.0, 100.0 / 686.0)
        );
        assert_eq!(next.screens[1].size_source, SizeSource::Reported);
        assert_eq!(l.screens[1].rect.y, 50.0 / 686.0);
    }
    #[test]
    fn collisions_keep_the_move_axis_and_resize_anchors() {
        let mut l = fixture(Side::Right);
        // The unconstrained editor would move the neighbour 10/686 reference units away from
        // the host to dodge this narrow obstacle. Native dragging must stay on it.
        l.screens.push(screen(
            3,
            "obstacle",
            Rect::new(400.0 / 686.0, 160.0 / 686.0, 10.0 / 686.0, 200.0 / 686.0),
            Rect::new(0.0, 0.0, 40.0, 800.0),
        ));
        let p = first(&l);
        let next = p.edit(&l, Handle::Middle, 400.0).unwrap();
        assert_eq!(
            next.screens[1].rect,
            Rect::new(400.0 / 686.0, 60.0 / 686.0, 200.0 / 686.0, 100.0 / 686.0)
        );
        assert_eq!(next.screens[0], l.screens[0]);
        assert_eq!(next.screens[2], l.screens[2]);
        let next = p.edit(&l, Handle::End, 400.0).unwrap();
        let resized = next.screens[1].rect;
        assert!((resized.h - 110.0 / 686.0).abs() < 1e-7);
        assert_eq!((resized.x, resized.y), (400.0 / 686.0, 50.0 / 686.0));
        assert_eq!(resized.w / resized.h, 2.0);
        assert_eq!(next.screens[0], l.screens[0]);
        assert_eq!(next.screens[2], l.screens[2]);
        assert!(next.overlaps().is_empty());
    }
    #[test]
    fn clicking_an_end_preserves_reported_size() {
        let l = fixture(Side::Right);
        assert_eq!(first(&l).edit(&l, Handle::Start, 0.0), Some(l));
    }
    #[test]
    fn end_resize_preserves_aspect_opposite_endpoint_and_seam_on_all_edges() {
        for side in Side::ALL {
            for handle in [Handle::Start, Handle::End] {
                let l = fixture(side);
                let p = first(&l);
                let next = p
                    .edit(&l, handle, if handle == Handle::Start { -80.0 } else { 80.0 })
                    .unwrap();
                let (a, b) = (&l.screens[1], &next.screens[1]);
                assert_eq!(b.rect.w / b.rect.h, a.rect.w / a.rect.h);
                assert_eq!(b.size_source, SizeSource::Manual);
                match side {
                    Side::Left => assert!((b.rect.right() - a.rect.right()).abs() < 1e-12),
                    Side::Right => assert!((b.rect.x - a.rect.x).abs() < 1e-12),
                    Side::Top => assert!((b.rect.bottom() - a.rect.bottom()).abs() < 1e-12),
                    Side::Bottom => assert!((b.rect.y - a.rect.y).abs() < 1e-12),
                }
                if handle == Handle::Start {
                    let before = if side.is_vertical() {
                        a.rect.bottom()
                    } else {
                        a.rect.right()
                    };
                    let after = if side.is_vertical() {
                        b.rect.bottom()
                    } else {
                        b.rect.right()
                    };
                    assert!((before - after).abs() < 1e-12);
                }
                assert!(next.overlaps().is_empty());
            }
        }
    }
    #[test]
    fn resize_limits_and_invalid_motion() {
        let l = fixture(Side::Right);
        let p = first(&l);
        let next = p.edit(&l, Handle::End, -1e9).unwrap();
        assert!((next.screens[1].rect.h - MIN_SIZE).abs() < 1e-12);
        let next = p.edit(&l, Handle::End, 1e9).unwrap();
        assert!((next.screens[1].rect.w - MAX_SIZE).abs() < 1e-12);
        assert!(p.edit(&l, Handle::Middle, f64::NAN).is_none());
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;
    use crate::layout::{ScreenPlacement, SizeSource};
    #[test]
    fn native_handles_transform_every_screen_of_the_neighbour_computer() {
        let a = DeviceId([1; 32]);
        let b = DeviceId([2; 32]);
        let make = |device, key: &str, local, rect| ScreenPlacement {
            id: ScreenId::new(device, key),
            name: key.into(),
            local,
            rect,
            size_source: SizeSource::Reported,
            primary: true,
            builtin: false,
        };
        let layout = Layout {
            screens: vec![
                make(
                    a,
                    "host",
                    Rect::new(0.0, 0.0, 1000.0, 600.0),
                    Rect::new(0.0, 0.0, 1.0, 0.6),
                ),
                make(
                    b,
                    "main",
                    Rect::new(0.0, 0.0, 1000.0, 600.0),
                    Rect::new(1.0, 0.1, 1.0, 0.6),
                ),
                make(
                    b,
                    "side",
                    Rect::new(1000.0, 200.0, 800.0, 400.0),
                    Rect::new(2.0, 0.3, 0.8, 0.4),
                ),
            ],
            ..Default::default()
        };
        let projection = compute(&layout.effective(&Default::default()), a, |_| true, None).remove(0);
        for handle in [Handle::Middle, Handle::Start, Handle::End] {
            let next = projection.edit(&layout, handle, 20.0).unwrap();
            let one = next.screen(&ScreenId::new(b, "main")).unwrap();
            let two = next.screen(&ScreenId::new(b, "side")).unwrap();
            let scale = one.rect.w / one.local.w;
            assert!((two.rect.w / two.local.w - scale).abs() < 1e-12);
            assert!((two.rect.x - one.rect.x - 1000.0 * scale).abs() < 1e-12);
            assert!((two.rect.y - one.rect.y - 200.0 * scale).abs() < 1e-12);
            assert_eq!(next.screens[0], layout.screens[0]);
            assert_eq!(next.screens[1].local, layout.screens[1].local);
        }
        let mut obstructed = layout.clone();
        obstructed.screens[2].local.y = 400.0;
        obstructed.screens[2].rect.y = 0.5;
        obstructed.screens.push(make(
            DeviceId([3; 32]),
            "obstacle",
            Rect::new(0.0, 0.0, 800.0, 400.0),
            Rect::new(2.0, 1.0, 0.8, 0.4),
        ));
        let projection = compute(&obstructed.effective(&Default::default()), a, |_| true, None).remove(0);
        let next = projection.edit(&obstructed, Handle::Middle, 300.0).unwrap();
        assert!((next.screens[2].rect.y - obstructed.screens[2].rect.y - 0.1).abs() < 1e-12);
        assert!((next.screens[1].rect.y - obstructed.screens[1].rect.y - 0.1).abs() < 1e-12);
    }
}

#[cfg(test)]
mod span_tests {
    use super::*;
    fn id(n: u8) -> ScreenId {
        ScreenId::new(DeviceId([n; 32]), "screen")
    }
    #[test]
    fn prototype_full_edges_partial_occlusion_corner_and_overlap() {
        let r = Rect::new(0.0, 0.0, 1512.0, 982.0);
        let spans = edge_spans(
            r,
            &[
                (id(1), Rect::new(-1048.0, -1440.0, 2560.0, 1440.0)),
                (id(2), Rect::new(1512.0, -70.0, 1920.0, 1082.0)),
                (id(3), Rect::new(1512.0, -1150.0, 1920.0, 1080.0)),
            ],
        );
        assert_eq!(
            spans,
            vec![
                EdgeSpan {
                    side: Side::Top,
                    neighbour: id(1),
                    a: 0.0,
                    b: 1.0
                },
                EdgeSpan {
                    side: Side::Right,
                    neighbour: id(2),
                    a: 0.0,
                    b: 1.0
                }
            ]
        );
        let r = Rect::new(0.0, 0.0, 1000.0, 500.0);
        assert_eq!(
            edge_spans(
                r,
                &[
                    (id(1), Rect::new(0.0, -2000.0, 1000.0, 500.0)),
                    (id(2), Rect::new(250.0, -400.0, 500.0, 300.0))
                ]
            ),
            vec![
                EdgeSpan {
                    side: Side::Top,
                    neighbour: id(1),
                    a: 0.0,
                    b: 0.25
                },
                EdgeSpan {
                    side: Side::Top,
                    neighbour: id(2),
                    a: 0.25,
                    b: 0.75
                },
                EdgeSpan {
                    side: Side::Top,
                    neighbour: id(1),
                    a: 0.75,
                    b: 1.0
                }
            ]
        );
        assert!(
            edge_spans(
                r,
                &[
                    (id(1), Rect::new(1000.0, 500.0, 300.0, 300.0)),
                    (id(2), Rect::new(900.0, 100.0, 400.0, 200.0))
                ]
            )
            .is_empty()
        );
    }
}
