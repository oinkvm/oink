//! Windows source foreground policy. Native window ownership lives in the input backend.
use super::*;

impl Router {
    /// Whether this source should own an ordinary foreground capture window.
    pub fn source_foreground_capture(&self) -> bool {
        self.cfg.settings.take_foreground_on_leave
            && self.holder != self.me()
            && !self.paused
            && self.capture_available
            && !self.mouse_local()
            && !self.keyboard_local()
            && !self.dnd_pointer_active()
            // Application focus deliberately keeps K on the old application
            // after P leaves. Its own fenced park window owns that workflow;
            // an independent source sink would steal typing from K or race it.
            && !self.application.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ME: DeviceId = DeviceId([1; 32]);
    const PEER: DeviceId = DeviceId([2; 32]);
    fn router() -> Router {
        let mut r = Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: ME,
                    nonce: [1; 16],
                },
                "source",
                Os::Windows,
            ),
            connection_tests::session_layout(ME, PEER),
        );
        r.test_connect(PEER, 0);
        r
    }
    #[test]
    fn setting_modes_native_drag_and_application_focus_keep_the_source_application() {
        let mut r = router();
        r.become_source(1, PEER);
        for (mouse, keyboard) in [
            (InputMode::Local, InputMode::Shared),
            (InputMode::Shared, InputMode::Local),
        ] {
            r.cfg.settings.mouse_mode = mouse;
            r.cfg.settings.keyboard_mode = keyboard;
            assert!(!r.source_foreground_capture());
        }
        r.cfg.settings.mouse_mode = InputMode::Shared;
        r.cfg.settings.keyboard_mode = InputMode::Shared;
        r.cfg.settings.take_foreground_on_leave = false;
        assert!(!r.source_foreground_capture());
        r.cfg.settings.take_foreground_on_leave = true;
        r.application.enabled = true;
        assert!(!r.source_foreground_capture());
        r.application.enabled = false;
        r.dnd.state = dnd::State::Hosting {
            id: oink_foundation::content::ClipId { origin: ME, seq: 1 },
            input: ME,
            items: vec![],
            push: None,
            begin_failed: false,
        };
        assert!(!r.source_foreground_capture());
        r.dnd.state = dnd::State::Idle;
        assert!(r.source_foreground_capture());
    }
    #[test]
    fn game_and_focus_lock_do_not_take_foreground_without_departure() {
        let mut r = router();
        for game in [true, false] {
            r.set_game_mode(if game { GameMode::On } else { GameMode::Off });
            r.set_locked(!game);
            r.local_motion_unconstrained(5000., 0., Some(Point::new(99., 50.)), 1);
            assert_eq!(r.focus().holder, ME);
            assert!(!r.source_foreground_capture());
        }
    }
    #[test]
    fn unavailable_capture_or_suspended_session_releases_source_foreground() {
        let mut r = router();
        r.become_source(1, PEER);
        assert!(r.source_foreground_capture());
        r.set_capture_available(false);
        assert!(!r.source_foreground_capture());
        r.set_capture_available(true);
        r.become_source(2, PEER);
        assert!(r.source_foreground_capture());
        r.set_input_suspended(true);
        assert!(!r.source_foreground_capture());
    }
    #[test]
    fn windows_departure_takes_foreground_and_return_releases_it() {
        let mut r = router();
        assert!(!r.source_foreground_capture());
        r.become_source(1, PEER);
        assert!(r.source_foreground_capture());
        r.holder = ME;
        assert!(!r.source_foreground_capture());
    }
}
