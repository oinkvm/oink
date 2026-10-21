//! Ownership of Oink's native clip lease, separate from application confinement.
use oink_foundation::Rect;
#[derive(Default)]
pub(crate) struct Ownership {
    owned: Option<Rect>,
    underlying_application: Option<Rect>,
}
impl Ownership {
    pub(crate) fn acquired(&mut self, rect: Rect) {
        self.owned = Some(rect);
        self.underlying_application = None;
    }
    pub(crate) fn acquired_confinement(&mut self, rect: Rect, previous: Rect, desktop: Rect) {
        let application = self.application(previous, desktop);
        self.acquired(rect);
        self.underlying_application = application;
    }
    pub(crate) fn released(&mut self, rect: Rect) {
        if self.owned == Some(rect) {
            self.owned = None;
            self.underlying_application = None;
        }
    }
    pub(crate) fn application(&mut self, rect: Rect, desktop: Rect) -> Option<Rect> {
        if self.owned == Some(rect) {
            return self.underlying_application;
        }
        // Once another rectangle is observed the old lease cannot become ours
        // again merely because an application later chooses the same pixels.
        self.owned = None;
        self.underlying_application = None;
        let valid = |r: Rect| {
            [r.x, r.y, r.w, r.h, r.right(), r.bottom()]
                .into_iter()
                .all(f64::is_finite)
                && r.w >= 1.
                && r.h >= 1.
        };
        // GetClipCursor returns the current virtual-screen bounds even when
        // unconfined. A foreign clip exactly equal to that entire desktop is
        // indistinguishable from ClipCursor(NULL), so only that exact rectangle
        // is treated as none. A single monitor on a larger desktop is a clip.
        // https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-getclipcursor
        (valid(rect)
            && valid(desktop)
            && rect.x >= desktop.x
            && rect.y >= desktop.y
            && rect.right() <= desktop.right()
            && rect.bottom() <= desktop.bottom()
            && rect != desktop)
            .then_some(rect)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const DESKTOP: Rect = Rect::new(-1000., 0., 3000., 1000.);
    const PIXEL: Rect = Rect::new(20., 30., 1., 1.);
    #[test]
    fn confinement_lease_excludes_our_walls_but_retains_a_preexisting_application_clip() {
        let foreign = Rect::new(-500., 20., 1700., 700.);
        let ours = Rect::new(0., 20., 1000., 700.);
        let mut owner = Ownership::default();
        owner.acquired_confinement(ours, foreign, DESKTOP);
        assert_eq!(owner.application(ours, DESKTOP), Some(foreign));
        owner.released(ours);
        assert_eq!(owner.application(foreign, DESKTOP), Some(foreign));
        owner.acquired_confinement(ours, DESKTOP, DESKTOP);
        assert_eq!(owner.application(ours, DESKTOP), None);
        let takeover = Rect::new(20., 30., 400., 300.);
        assert_eq!(owner.application(takeover, DESKTOP), Some(takeover));
        assert_eq!(owner.application(ours, DESKTOP), Some(ours));
    }
    #[test]
    fn cursor_clip_own_lease_is_excluded_but_same_size_application_is_not() {
        let mut owner = Ownership::default();
        assert_eq!(owner.application(PIXEL, DESKTOP), Some(PIXEL));
        owner.acquired(PIXEL);
        assert_eq!(owner.application(PIXEL, DESKTOP), None);
        owner.released(PIXEL);
        assert_eq!(owner.application(PIXEL, DESKTOP), Some(PIXEL));
    }
    #[test]
    fn cursor_clip_takeover_cannot_revive_old_ownership() {
        let mut owner = Ownership::default();
        owner.acquired(PIXEL);
        let app = Rect::new(-500., 0., 1000., 1000.);
        assert_eq!(owner.application(app, DESKTOP), Some(app));
        assert_eq!(owner.application(PIXEL, DESKTOP), Some(PIXEL));
    }
    #[test]
    fn cursor_clip_full_desktop_and_invalid_rectangles_are_not_constraints() {
        let mut owner = Ownership::default();
        for rect in [
            DESKTOP,
            Rect::new(0., 0., 0., 1.),
            Rect::new(-1001., 0., 3000., 1000.),
            Rect::new(f64::NAN, 0., 1., 1.),
        ] {
            assert_eq!(owner.application(rect, DESKTOP), None);
        }
        assert_eq!(
            owner.application(Rect::new(-300., 0., 600., 1000.), DESKTOP),
            Some(Rect::new(-300., 0., 600., 1000.))
        );
    }

    #[test]
    fn cursor_clip_full_monitor_and_almost_full_desktop_are_foreign_constraints() {
        let mut owner = Ownership::default();
        for clip in [
            Rect::new(-1000., 0., 1000., 1000.),
            Rect::new(0., 0., 2000., 1000.),
            Rect::new(-1000., 0., 2999., 1000.),
            Rect::new(-1000., 0., 3000., 999.),
        ] {
            assert_eq!(owner.application(clip, DESKTOP), Some(clip));
        }
        assert_eq!(owner.application(DESKTOP, DESKTOP), None);
    }
}
