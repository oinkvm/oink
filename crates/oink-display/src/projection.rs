//! Physical-screen projections. Layout reference units, never cursor gain,
//! determine their extent. Nothing in this module changes OS display positions.

use oink_foundation::{DeviceId, Point, Rect};

use crate::layout::{EffectiveLayout, ScreenId, Side};

/// A shallow slice of the neighbouring rectangle is translated across the seam.
pub const DEPTH: f64 = 14.0 / 686.0;

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
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Layout, OwnerLayoutPreferences, ScreenPlacement, SizeSource};
    use std::collections::BTreeMap;

    fn screen(device: u8, key: &str, rect: Rect, local: Rect) -> ScreenPlacement {
        ScreenPlacement {
            id: ScreenId::new(DeviceId([device; 32]), key),
            name: key.into(),
            rect,
            local,
            size_source: SizeSource::Reported,

            primary: true,
            builtin: false,
            device_hidden: false,
            screen_hidden: false,
            device_visibility_known: false,
            screen_visibility_known: false,
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
