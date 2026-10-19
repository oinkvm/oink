//! Physical-pixel parking geometry, independent of native window/drag calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct Monitor {
    pub bounds: Rect,
    pub work: Rect,
    /// Full autohide bar thickness at LEFT, TOP, RIGHT, BOTTOM, including when
    /// its HWND is currently outside the monitor and rcWork fills the display.
    pub autohide: [i32; 4],
}
impl Monitor {
    fn usable(self) -> Rect {
        let r = self.bounds;
        Rect {
            left: self.work.left.max(r.left.saturating_add(self.autohide[0])).max(r.left),
            top: self.work.top.max(r.top.saturating_add(self.autohide[1])).max(r.top),
            right: self
                .work
                .right
                .min(r.right.saturating_sub(self.autohide[2]))
                .min(r.right),
            bottom: self
                .work
                .bottom
                .min(r.bottom.saturating_sub(self.autohide[3]))
                .min(r.bottom),
        }
    }
}

pub(super) fn select(monitors: &[Monitor], occluders: &[Rect]) -> Option<(Rect, (i32, i32))> {
    let first = monitors.first()?.bounds;
    let bounds = monitors.iter().fold(first, |a, m| Rect {
        left: a.left.min(m.bounds.left),
        top: a.top.min(m.bounds.top),
        right: a.right.max(m.bounds.right),
        bottom: a.bottom.max(m.bounds.bottom),
    });
    // A top-left cursor hotspot hides the body only on a RIGHT or BOTTOM
    // exterior edge. Never move into an appbar and try to out-rank the shell:
    // taskbar drag handling can reassert itself after a successful hit test.
    // Prefer the right edge; a taskbar there instead permits a free bottom.
    for right_edge in [true, false] {
        let mut best = None;
        for monitor in monitors {
            let usable = monitor.usable();
            if usable.left >= usable.right || usable.top >= usable.bottom {
                continue;
            }
            let (fixed, lo, hi) = if right_edge {
                if monitor.bounds.right != bounds.right || usable.right != bounds.right {
                    continue;
                }
                (bounds.right - 1, usable.top, usable.bottom)
            } else {
                if monitor.bounds.bottom != bounds.bottom || usable.bottom != bounds.bottom {
                    continue;
                }
                (bounds.bottom - 1, usable.left, usable.right)
            };
            let mut spans = vec![(lo, hi)];
            for obstacle in occluders {
                let (cross_lo, cross_hi, along_lo, along_hi) = if right_edge {
                    (obstacle.left, obstacle.right, obstacle.top, obstacle.bottom)
                } else {
                    (obstacle.top, obstacle.bottom, obstacle.left, obstacle.right)
                };
                if fixed < cross_lo || fixed >= cross_hi {
                    continue;
                }
                let mut next = Vec::new();
                for (a, b) in spans {
                    if along_hi <= a || along_lo >= b {
                        next.push((a, b));
                        continue;
                    }
                    if a < along_lo {
                        next.push((a, along_lo));
                    }
                    if along_hi < b {
                        next.push((along_hi, b));
                    }
                }
                spans = next;
            }
            for (a, b) in spans {
                // The longest gap's midpoint stays away from adjacent appbars
                // and other topmost surfaces. Tie-breaking ignores enumeration order.
                let rank = (i64::from(b) - i64::from(a), a, b);
                if best.as_ref().is_none_or(|(old, _)| rank > *old) {
                    let middle = (i64::from(a) + (i64::from(b) - i64::from(a) - 1) / 2) as i32;
                    let point = if right_edge { (fixed, middle) } else { (middle, fixed) };
                    best = Some((rank, point));
                }
            }
        }
        if let Some((_, point)) = best {
            return Some((bounds, point));
        }
    }
    // All positive exterior edges are reserved/occluded. There is no point
    // satisfying this contract; the caller reports an explicit hold failure.
    None
}
#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }
    fn screen(work: Rect) -> Monitor {
        Monitor {
            bounds: rect(0, 0, 3840, 2160),
            work,
            autohide: [0; 4],
        }
    }
    #[test]
    fn bottom_taskbar_parks_at_right_work_area_midpoint() {
        assert_eq!(select(&[screen(rect(0, 0, 3840, 2088))], &[]).unwrap().1, (3839, 1043));
    }
    #[test]
    fn taskbars_on_every_edge_leave_the_cursor_on_a_free_positive_edge() {
        for (work, point) in [
            (rect(0, 72, 3840, 2160), (3839, 1115)),
            (rect(72, 0, 3840, 2160), (3839, 1079)),
            (rect(0, 0, 3768, 2160), (1883, 2159)),
            (rect(0, 0, 3840, 2088), (3839, 1043)),
        ] {
            assert_eq!(select(&[screen(work)], &[]).unwrap().1, point);
        }
    }
    #[test]
    fn autohide_bars_reserve_their_full_edge_even_when_work_area_is_full() {
        for (edge, point) in [
            (0, (3839, 1079)),
            (1, (3839, 1115)),
            (2, (1883, 2159)),
            (3, (3839, 1043)),
        ] {
            let mut m = screen(rect(0, 0, 3840, 2160));
            m.autohide[edge] = 72;
            assert_eq!(select(&[m], &[]).unwrap().1, point);
        }
    }
    #[test]
    fn visible_topmost_obstacles_are_subtracted_before_choosing_a_midpoint() {
        let m = screen(rect(0, 0, 3840, 2088));
        assert_eq!(select(&[m], &[rect(3700, 500, 3840, 2000)]).unwrap().1, (3839, 249));
        assert!(select(&[m], &[rect(3700, 0, 3840, 2088)]).is_none());
    }
    #[test]
    fn right_appbar_uses_free_bottom_and_both_reserved_edges_refuse_parking() {
        assert_eq!(select(&[screen(rect(0, 0, 3768, 2160))], &[]).unwrap().1, (1883, 2159));
        assert!(select(&[screen(rect(0, 0, 3768, 2088))], &[]).is_none());
    }
    #[test]
    fn negative_origins_gaps_and_monitor_order_do_not_create_a_virtual_corner() {
        let mut monitors = [
            Monitor {
                bounds: rect(-1920, 0, 0, 1080),
                work: rect(-1920, 0, 0, 1000),
                autohide: [0; 4],
            },
            Monitor {
                bounds: rect(200, -900, 1800, 0),
                work: rect(200, -900, 1800, 0),
                autohide: [0; 4],
            },
        ];
        let expected = select(&monitors, &[]).unwrap();
        assert_eq!(expected.1, (1799, -451));
        assert_eq!(expected.0, rect(-1920, -900, 1800, 1080));
        monitors.reverse();
        assert_eq!(select(&monitors, &[]), Some(expected));
        assert!(select(&[], &[]).is_none());
    }
}
