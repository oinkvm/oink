//! Cancellation geometry in the source's native coordinate space.
use oink_foundation::{Point, Rect};

/// Last exit from the union, including disjoint source displays. Taking the
/// first display's boundary would start the animation in the middle of the
/// source desktop when the segment crosses a second source display.
pub(super) fn last_exit(origin: Point, target: Point, displays: &[Rect]) -> Option<Point> {
    if ![origin.x, origin.y, target.x, target.y].iter().all(|x| x.is_finite())
        || !displays.iter().any(|r| r.contains(origin))
        || displays.iter().any(|r| r.contains(target))
    {
        return None;
    }
    let dx = target.x - origin.x;
    let dy = target.y - origin.y;
    let mut last: Option<(f64, Rect)> = None;
    for r in displays {
        let mut enter: f64 = 0.;
        let mut leave: f64 = 1.;
        let mut intersects = true;
        for (at, delta, lo, hi) in [(origin.x, dx, r.x, r.right()), (origin.y, dy, r.y, r.bottom())] {
            if delta == 0. {
                if at < lo || at > hi {
                    intersects = false;
                    break;
                }
            } else {
                let a = (lo - at) / delta;
                let b = (hi - at) / delta;
                enter = enter.max(a.min(b));
                leave = leave.min(a.max(b));
            }
        }
        if intersects && enter <= leave && leave < 1. && last.is_none_or(|(t, _)| leave > t) {
            last = Some((leave, *r));
        }
    }
    last.map(|(t, r)| r.clamp(Point::new(origin.x + t * dx, origin.y + t * dy)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn near(got: Option<Point>, want: Point) {
        let got = got.unwrap();
        assert!(
            (got.x - want.x).abs() < 0.001 && (got.y - want.y).abs() < 0.001,
            "{got:?} != {want:?}"
        );
    }
    #[test]
    fn cancellation_uses_the_last_union_boundary_not_bounding_box_or_first_display() {
        let screens = [Rect::new(0., 0., 100., 100.), Rect::new(150., 0., 100., 100.)];
        near(
            last_exit(Point::new(50., 50.), Point::new(350., 50.), &screens),
            Point::new(250., 50.),
        );
        near(
            last_exit(Point::new(50., 50.), Point::new(300., 200.), &screens),
            Point::new(100., 80.),
        );
        near(
            last_exit(Point::new(50., 50.), Point::new(-50., -50.), &screens),
            Point::new(0., 0.),
        );
        near(
            last_exit(Point::new(50., 50.), Point::new(50., -50.), &screens),
            Point::new(50., 0.),
        );
        assert!(last_exit(Point::new(50., 50.), Point::new(160., 50.), &screens).is_none());
        assert!(last_exit(Point::new(50., 50.), Point::new(f64::NAN, 0.), &screens).is_none());
    }
    #[test]
    fn overlapping_displays_do_not_create_an_internal_exit() {
        near(
            last_exit(
                Point::new(10., 25.),
                Point::new(300., 25.),
                &[Rect::new(0., 0., 100., 100.), Rect::new(50., 0., 100., 100.)],
            ),
            Point::new(150., 25.),
        );
    }
}
