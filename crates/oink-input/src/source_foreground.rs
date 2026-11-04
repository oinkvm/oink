//! One foreground attempt per source departure. Native windows never leave their UI thread.
pub(crate) trait Native {
    type Window: Copy + Eq;
    fn foreground(&self) -> Option<Self::Window>;
    fn valid(&self, window: Self::Window) -> bool;
    fn show_capture(&mut self);
    fn hide_capture(&mut self);
    fn minimized(&self, window: Self::Window) -> bool;
    /// Queue a restore without activating when the target UI thread handles it.
    fn restore_without_activation(&mut self, window: Self::Window);
    /// Native implementations log a refused attempt, without retrying it.
    fn activate(&mut self, window: Self::Window);
}
#[derive(Default)]
pub(crate) struct Lease<W> {
    enabled: bool,
    previous: Option<W>,
}
impl<W: Copy + Eq> Lease<W> {
    pub fn update(&mut self, enabled: bool, capture: W, native: &mut impl Native<Window = W>) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            self.previous = native.foreground().filter(|window| *window != capture);
            native.show_capture();
            native.activate(capture);
        } else {
            if let Some(previous) = self.previous.take()
                && native.foreground() == Some(capture)
                && native.valid(previous)
            {
                if native.minimized(previous) {
                    // The other UI thread may process this after a new application
                    // takes foreground. Only the immediate request below may activate;
                    // the deferred show must preserve whichever window is then foreground.
                    native.restore_without_activation(previous);
                }
                native.activate(previous);
            }
            native.hide_capture();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Desktop {
        foreground: Option<u64>,
        minimized: bool,
        closed: bool,
        refuse: bool,
        actions: Vec<(&'static str, u64)>,
        pending_restores: Vec<(u64, bool)>,
    }
    impl Native for Desktop {
        type Window = u64;
        fn foreground(&self) -> Option<u64> {
            self.foreground
        }
        fn valid(&self, _: u64) -> bool {
            !self.closed
        }
        fn show_capture(&mut self) {
            self.actions.push(("show", 2));
        }
        fn hide_capture(&mut self) {
            self.actions.push(("hide", 2));
        }
        fn minimized(&self, _: u64) -> bool {
            self.minimized
        }
        fn restore_without_activation(&mut self, window: u64) {
            self.actions.push(("restore", window));
            // Model both native show modes, including their delayed activation.
            self.pending_restores.push((window, false));
        }
        fn activate(&mut self, window: u64) {
            self.actions.push(("activate", window));
            if !self.refuse {
                self.foreground = Some(window);
            }
        }
    }
    impl Desktop {
        fn complete_restores(&mut self) {
            for (window, activate) in self.pending_restores.drain(..) {
                self.minimized = false;
                if activate {
                    self.foreground = Some(window);
                }
            }
        }
    }
    fn departing() -> (Lease<u64>, Desktop) {
        let mut lease = Lease::default();
        let mut native = Desktop {
            foreground: Some(1),
            ..Default::default()
        };
        lease.update(true, 2, &mut native);
        (lease, native)
    }
    #[test]
    fn third_party_foreground_before_return_is_preserved_without_restoring_the_previous_window() {
        let (mut lease, mut native) = departing();
        native.foreground = Some(3);
        native.minimized = true;
        lease.update(false, 2, &mut native);
        assert_eq!(&native.actions[2..], [("hide", 2)]);
        assert!(native.pending_restores.is_empty());
        assert_eq!(native.foreground, Some(3));
        assert!(lease.previous.is_none());
    }
    #[test]
    fn capture_still_foreground_restores_the_previous_window() {
        let (mut lease, mut native) = departing();
        lease.update(false, 2, &mut native);
        assert_eq!(&native.actions[2..], [("activate", 1), ("hide", 2)]);
        assert_eq!(native.foreground, Some(1));
    }
    #[test]
    fn destroyed_previous_window_is_neither_restored_nor_activated() {
        let (mut lease, mut native) = departing();
        native.closed = true;
        native.minimized = true;
        lease.update(false, 2, &mut native);
        assert_eq!(&native.actions[2..], [("hide", 2)]);
        assert!(native.pending_restores.is_empty());
    }
    #[test]
    fn delayed_restore_cannot_steal_foreground_from_a_new_application() {
        let (mut lease, mut native) = departing();
        native.minimized = true;
        lease.update(false, 2, &mut native);
        assert_eq!(native.pending_restores, [(1, false)]);
        // The old UI thread has not consumed its restore request yet. A new
        // application finishes starting and obtains foreground in the meantime.
        native.foreground = Some(3);
        native.complete_restores();
        assert!(!native.minimized);
        assert_eq!(native.foreground, Some(3));
    }
    #[test]
    fn takes_once_and_restores_a_minimized_application_only_while_still_foreground() {
        for (current, closed) in [
            (Some(2), false),
            (Some(3), false),
            (Some(1), false),
            (None, false),
            (Some(2), true),
        ] {
            let mut lease = Lease::default();
            let mut native = Desktop {
                foreground: Some(1),
                ..Default::default()
            };
            lease.update(true, 2, &mut native);
            lease.update(true, 2, &mut native);
            assert_eq!(native.actions, [("show", 2), ("activate", 2)]);
            native.foreground = current;
            native.minimized = true;
            native.closed = closed;
            lease.update(false, 2, &mut native);
            if current == Some(2) && !closed {
                assert_eq!(&native.actions[2..], [("restore", 1), ("activate", 1), ("hide", 2)]);
            } else {
                assert_eq!(&native.actions[2..], [("hide", 2)]);
            }
        }
    }
    #[test]
    fn denial_is_not_retried_and_the_next_departure_remembers_the_new_application() {
        let mut lease = Lease::default();
        let mut native = Desktop {
            foreground: Some(1),
            refuse: true,
            ..Default::default()
        };
        lease.update(true, 2, &mut native);
        for _ in 0..10 {
            lease.update(true, 2, &mut native);
        }
        lease.update(false, 2, &mut native);
        assert_eq!(native.actions, [("show", 2), ("activate", 2), ("hide", 2)]);
        native.refuse = false;
        native.foreground = Some(3);
        lease.update(true, 2, &mut native);
        lease.update(false, 2, &mut native);
        assert_eq!(native.foreground, Some(3));
    }
}
