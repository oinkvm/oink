//! Small top-edge inset; logical routing coordinates never use this point.
use oink_foundation::{Point, Rect, dnd::DestinationSnapshot};

// Dock on macOS 26.6.2 observes the top one-point strip. Two points
// clears it while staying inside our existing 48-point catcher.
pub(super) const TOP_INSET: f64 = 2.0;

// Admission deliberately ignores the CURRENT WindowServer hit and AppKit's
// previous sample. The caller verifies ownership at the escape target and
// requires a matching native callback after the move.
pub(super) fn can_escape(snapshot: &DestinationSnapshot) -> bool {
    snapshot.destination_ours && snapshot.visible
}

pub(super) fn owns_frozen_source(snapshot: &DestinationSnapshot, anchor: Point) -> bool {
    can_escape(snapshot) && snapshot.mouse_hit_ours && snapshot.mouse_location == anchor
}

pub(super) fn inset(anchor: Point, catcher: Rect, screens: &[Rect]) -> Option<Point> {
    let screen = screens.iter().find(|s| s.contains(anchor))?;
    if !anchor.x.is_finite() || !anchor.y.is_finite() || !catcher.contains(anchor) {
        return None;
    }
    let at = if anchor.y >= screen.y + TOP_INSET {
        anchor
    } else {
        // Match input's containing-integer Quartz drag endpoint exactly, also
        // at negative origins. Otherwise a fractional x would never receive
        // the exact native position acknowledgement for the submitted event.
        Point::new(anchor.x.floor(), (screen.y + TOP_INSET).floor())
    };
    (screen.contains(at) && catcher.contains(at)).then_some(at)
}

pub(super) fn cover(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect::new(x, y, a.right().max(b.right()) - x, a.bottom().max(b.bottom()) - y)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escape_admission_allows_stale_edge_hit_and_session_sample() {
        let snapshot = DestinationSnapshot {
            destination_ours: true,
            visible: true,
            mouse_hit_ours: false,
            mouse_location: Point::new(659., 0.),
            session_location: Some(Point::new(660., 0.)),
            ..Default::default()
        };
        assert!(can_escape(&snapshot));
        assert!(!owns_frozen_source(&snapshot, snapshot.mouse_location));
        assert!(!can_escape(&DestinationSnapshot {
            destination_ours: false,
            ..snapshot
        }));
        assert!(!can_escape(&DestinationSnapshot {
            visible: false,
            ..snapshot
        }));
    }
    #[test]
    fn top_edge_moves_two_points_inside_the_existing_strip() {
        let screen = Rect::new(0., 0., 1728., 1117.);
        let catcher = Rect::new(0., 0., 1728., 48.);
        let edge = Point::new(655., 0.);
        assert_eq!(inset(edge, catcher, &[screen]), Some(Point::new(655., 2.)));
        assert_eq!(edge, Point::new(655., 0.), "logical edge is independent");
        for y in [1., 4., 6., 8., 24.] {
            assert_eq!(
                inset(Point::new(655., y), catcher, &[screen]),
                Some(Point::new(655., y.max(2.)))
            );
        }
    }
    #[test]
    fn inset_uses_source_display_origin_and_never_crosses_a_seam() {
        let primary = Rect::new(0., 0., 1728., 1117.);
        let upper = Rect::new(-200., -1200., 1920., 1200.);
        assert_eq!(
            inset(
                Point::new(500., -1200.),
                Rect::new(-200., -1200., 1920., 48.),
                &[primary, upper]
            ),
            Some(Point::new(500., -1198.))
        );
        assert_eq!(
            inset(
                Point::new(500., -1.),
                Rect::new(-200., -48., 1920., 48.),
                &[primary, upper]
            ),
            Some(Point::new(500., -1.))
        );
        assert_eq!(
            inset(Point::new(500., 0.), Rect::new(0., 0., 1728., 48.), &[primary, upper]),
            Some(Point::new(500., 2.))
        );
    }
    #[test]
    fn fractional_top_anchor_matches_the_input_drag_endpoint() {
        let screen = Rect::new(-200., -1200., 1920., 1200.);
        let catcher = Rect::new(-200., -1200., 1920., 48.);
        assert_eq!(
            inset(Point::new(-55.25, -1199.5), catcher, &[screen]),
            Some(Point::new(-56., -1198.))
        );
        assert_eq!(
            inset(Point::new(432.75, -1200.), catcher, &[screen]),
            Some(Point::new(432., -1198.))
        );
        let already_inset = Point::new(-55.25, -1190.5);
        assert_eq!(inset(already_inset, catcher, &[screen]), Some(already_inset));
    }

    #[test]
    fn no_move_outside_the_current_surface_or_display() {
        let screen = Rect::new(0., 0., 100., 100.);
        assert!(inset(Point::new(50., 0.), Rect::new(0., 0., 100., 1.), &[screen]).is_none());
        assert!(inset(Point::new(150., 0.), screen, &[screen]).is_none());
        assert!(inset(Point::new(f64::NAN, 0.), screen, &[screen]).is_none());
        assert_eq!(
            inset(Point::new(0., 50.), Rect::new(0., 0., 48., 100.), &[screen]),
            Some(Point::new(0., 50.))
        );
    }
}
