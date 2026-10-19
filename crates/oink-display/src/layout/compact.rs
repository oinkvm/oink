//! Contact compaction of rigid computers. Compare directed source-edge intervals
//! and destinations, ignoring only normal gap distance. Routing deadzones and
//! optional wall extension do not decide whether shared geometry can move.
use super::facing::{edge_projections, facing, overlaps, same_projections, tolerance};
use super::{Layout, Side};
use oink_foundation::{DeviceId, Rect};

fn start(r: Rect, axis: usize) -> f64 {
    if axis == 0 { r.x } else { r.y }
}
fn extent(r: Rect, axis: usize) -> f64 {
    if axis == 0 { r.w } else { r.h }
}
fn translated(rects: &[Rect], moving: &[bool], axis: usize, delta: f64) -> Vec<Rect> {
    rects
        .iter()
        .zip(moving)
        .map(|(&r, &moving)| {
            if !moving {
                r
            } else if axis == 0 {
                Rect::new(r.x + delta, r.y, r.w, r.h)
            } else {
                Rect::new(r.x, r.y + delta, r.w, r.h)
            }
        })
        .collect()
}

impl Layout {
    /// Close safe inter-computer gaps, including gaps left by another editor.
    /// Every accepted axis translation reaches first contact and preserves the
    /// complete edge projection for the whole path. Existing contacts never open.
    pub fn compact(&mut self) {
        self.compact_pinned(None);
    }

    pub(super) fn compact_pinned(&mut self, pin: Option<DeviceId>) {
        let indices: Vec<_> = self
            .screens
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.is_valid().then_some(i))
            .collect();
        let devices: Vec<_> = indices.iter().map(|&i| self.screens[i].id.device).collect();
        let groups: std::collections::BTreeSet<_> = devices.iter().copied().collect();
        let mut rects: Vec<_> = indices.iter().map(|&i| self.screens[i].rect).collect();
        let eps = tolerance(&rects);
        // An editor preview may still be resolving a collision. Such a layout
        // has no valid facing invariant; never turn penetration into a new seam.
        if rects
            .iter()
            .enumerate()
            .any(|(i, &a)| rects.iter().skip(i + 1).any(|&b| overlaps(a, b, eps)))
        {
            return;
        }
        let before = edge_projections(&rects, eps);
        loop {
            let mut best: Option<(f64, Vec<Rect>)> = None;
            for &device in &groups {
                if Some(device) == pin {
                    continue;
                }
                let moving: Vec<_> = devices.iter().map(|&d| d == device).collect();
                for axis in 0..2 {
                    let gaps = [-1., 1.].map(|dir| {
                        let mut gap = f64::INFINITY;
                        for (i, &a) in rects.iter().enumerate() {
                            if !moving[i] {
                                continue;
                            }
                            for (j, &b) in rects.iter().enumerate() {
                                if moving[j] {
                                    continue;
                                }
                                let side = match (axis, dir > 0.) {
                                    (0, false) => Side::Left,
                                    (0, true) => Side::Right,
                                    (_, false) => Side::Top,
                                    (_, true) => Side::Bottom,
                                };
                                if let Some(f) = facing(a, b, side, eps) {
                                    gap = gap.min(f.gap);
                                }
                            }
                        }
                        gap
                    });
                    if gaps.iter().any(|&g| g <= eps) {
                        continue;
                    }
                    for (dir, distance) in [-1., 1.].into_iter().zip(gaps) {
                        if !distance.is_finite() || best.as_ref().is_some_and(|(d, _)| distance >= d - eps) {
                            continue;
                        }
                        // Endpoint coincidences partition all possible projection
                        // changes. Check the open intervals and the final contact.
                        let mut events = vec![0., distance];
                        for (i, &a) in rects.iter().enumerate() {
                            if !moving[i] {
                                continue;
                            }
                            for (j, &b) in rects.iter().enumerate() {
                                if moving[j] {
                                    continue;
                                }
                                for p in [start(a, axis), start(a, axis) + extent(a, axis)] {
                                    for q in [start(b, axis), start(b, axis) + extent(b, axis)] {
                                        let t = dir * (q - p);
                                        if t > eps && t < distance - eps {
                                            events.push(t);
                                        }
                                    }
                                }
                            }
                        }
                        events.sort_by(f64::total_cmp);
                        let samples = std::iter::once(distance).chain(events.windows(2).map(|w| (w[0] + w[1]) / 2.));
                        if samples.into_iter().all(|t| {
                            same_projections(
                                &before,
                                &edge_projections(&translated(&rects, &moving, axis, dir * t), eps),
                                eps,
                            )
                        }) {
                            best = Some((distance, translated(&rects, &moving, axis, dir * distance)));
                        }
                    }
                }
            }
            let Some((_, next)) = best else {
                break;
            };
            rects = next;
            // Every accepted move adds a contact. The zero-gap guard prevents
            // normal separation, and preserved projections prevent sliding it.
        }
        for (&i, r) in indices.iter().zip(rects) {
            self.screens[i].rect = r;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ScreenId, ScreenPlacement, SizeSource};
    use super::*;

    fn layout(rects: &[(u8, Rect)]) -> Layout {
        Layout {
            screens: rects
                .iter()
                .enumerate()
                .map(|(i, &(d, rect))| ScreenPlacement {
                    id: ScreenId::new(DeviceId([d; 32]), i.to_string()),
                    name: "Synthetic screen".into(),
                    rect,
                    local: rect,
                    size_source: SizeSource::Manual,
                    primary: false,
                    builtin: false,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn screenshot_contact_and_subpixel_penetration_cannot_pull_lower_right_up() {
        for scale in [1., 1. / 4096.] {
            for penetration in [0., 1e-8, 0.25] {
                let mut l = layout(&[
                    (1, Rect::new(493., 0., 741., 416.8125)),
                    (2, Rect::new(0., 416.8125, 604., 604. * 1117. / 1728.)),
                    (3, Rect::new(604. - penetration, 472.8125, 604., 604. * 1117. / 1728.)),
                ]);
                for s in &mut l.screens {
                    s.rect = Rect::new(s.rect.x * scale, s.rect.y * scale, s.rect.w * scale, s.rect.h * scale);
                }
                let before = l.clone();
                l.compact();
                assert_eq!(l, before);
                l.normalize();
                assert_eq!(
                    l.screens[2].rect.y, before.screens[2].rect.y,
                    "collision repair must not pull upward"
                );
                assert!(l.overlaps().is_empty());
            }
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(128))]
        #[test]
        fn generated_compaction_preserves_pairs_and_renderer(
            data in proptest::collection::vec((0u8..4, -6i32..7, -6i32..7, 1i32..4, 1i32..4), 2..8),
            scale in proptest::sample::select(vec![1., 1. / 4096., 16.])
        ) {
            let mut l = layout(&data.iter().map(|&(d, x, y, w, h)|
                (d, Rect::new(x as f64 * 100. * scale, y as f64 * 100. * scale, w as f64 * 50. * scale, h as f64 * 50. * scale))
            ).collect::<Vec<_>>());
            let rects = |l: &Layout| l.screens.iter().map(|s| s.rect).collect::<Vec<_>>();
            let before_rects = rects(&l);
            let eps = tolerance(&before_rects);
            let before = edge_projections(&before_rects, eps);
            l.compact();
            let after = edge_projections(&rects(&l), eps);
            proptest::prop_assert!(same_projections(&before, &after, eps));
            for (host, r) in l.screens.iter().enumerate() {
                let others = l.screens.iter().enumerate().filter(|(i, _)| *i != host)
                    .map(|(_, s)| (s.id.clone(), s.rect)).collect::<Vec<_>>();
                let spans = crate::projection::edge_spans(r.rect, &others);
                let visible = after.iter().filter(|p| p.screen == host && p.kind == "visible" && p.target.is_some()).collect::<Vec<_>>();
                proptest::prop_assert_eq!(spans.len(), visible.len());
                for p in visible {
                    let side = super::super::facing::SIDES[p.side];
                    let length = if side.is_vertical() { r.rect.h } else { r.rect.w };
                    proptest::prop_assert!(spans.iter().any(|s| s.side == side && s.neighbour == l.screens[p.target.unwrap()].id
                        && (s.a * length - p.from).abs() <= eps && (s.b * length - p.to).abs() <= eps));
                }
            }
        }
    }

    #[test]
    fn compaction_preserves_all_directed_projections_and_native_group_shape() {
        for scale in [1., 1. / 4096.] {
            let mut l = layout(&[
                (1, Rect::new(508. * scale, 0., 868. * scale, 488. * scale)),
                (2, Rect::new(0., 525. * scale, 558. * scale, 361. * scale)),
                (3, Rect::new(737. * scale, 543. * scale, 584. * scale, 377. * scale)),
            ]);
            let before = edge_projections(&l.screens.iter().map(|s| s.rect).collect::<Vec<_>>(), 1e-9 * scale);
            l.compact();
            assert!((l.screens[0].rect.y - 37. * scale).abs() < 1e-9 * scale);
            let after = edge_projections(&l.screens.iter().map(|s| s.rect).collect::<Vec<_>>(), 1e-9 * scale);
            assert!(same_projections(&before, &after, 1e-9 * scale));
        }
        let mut step = layout(&[
            (1, Rect::new(0., 0., 100., 100.)),
            (1, Rect::new(100., 50., 100., 100.)),
            (2, Rect::new(100., 200., 100., 100.)),
        ]);
        let before = step.clone();
        step.compact();
        assert_eq!(step.screens[0].rect.y, 50.);
        assert_eq!(step.screens[1].rect.y, 100.);
        assert_eq!(step.screens[2], before.screens[2]);
        for (a, b) in step.screens.iter().zip(&before.screens) {
            assert_eq!(a.local, b.local);
            assert_eq!((a.rect.w, a.rect.h), (b.rect.w, b.rect.h));
        }
    }

    #[test]
    fn desktop_holes_and_existing_contact_do_not_pull_or_slide() {
        let mut hole = layout(&[
            (1, Rect::new(0., 0., 100., 100.)),
            (1, Rect::new(200., 0., 100., 100.)),
            (2, Rect::new(120., 160., 60., 100.)),
        ]);
        let before = hole.clone();
        hole.compact();
        assert_eq!(hole, before);
        let mut contact = layout(&[
            (1, Rect::new(0., 0., 100., 100.)),
            (2, Rect::new(100., 20., 100., 60.)),
            (3, Rect::new(300., 20., 100., 60.)),
        ]);
        let before = contact.clone();
        contact.compact();
        assert_eq!(contact.screens[..2], before.screens[..2]);
        assert_eq!(contact.screens[2].rect.x, 200.);
    }

    #[test]
    fn preserves_every_pair_projection_at_a_rounded_contact() {
        // A wide upper screen touches the lower-left screen. The lower-right
        // screen is farther down. Closing their horizontal gap would slide the
        // lower-left screen's projection along the upper screen's bottom edge.
        // Keep the roundoff from a native extent multiplied by uniform scale.
        for scale in [1., 1. / 4096.] {
            let mut l = layout(&[
                (
                    1,
                    Rect::new(0., 0., 3461.5555555555566 * scale, 1947.1250000000007 * scale),
                ),
                (
                    2,
                    Rect::new(1139. * scale, 2185. * scale, 2055. * scale, 1328.3767361111109 * scale),
                ),
                (
                    3,
                    Rect::new(
                        -1687. * scale,
                        1947.125 * scale,
                        1966. * scale,
                        1270.8460648148146 * scale,
                    ),
                ),
            ]);
            let before = l.clone();
            let spans = |l: &Layout| {
                l.screens
                    .iter()
                    .map(|host| {
                        crate::projection::edge_spans(
                            host.rect,
                            &l.screens
                                .iter()
                                .filter(|s| s.id != host.id)
                                .map(|s| (s.id.clone(), s.rect))
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let projections = spans(&l);
            l.compact();
            assert_eq!(
                spans(&l),
                projections,
                "another computer's edge projection changed at scale {scale}"
            );
            assert_eq!(l, before, "the lower gap is intentional at scale {scale}");
            if scale < 1. {
                // Backend saves repair collisions before compacting. Native-scale
                // roundoff is below collision area tolerance and must still retain
                // the complete contact projection during that normalization path.
                l.normalize();
                assert_eq!(l, before);
            }
        }
    }
}
