//! One foreground attempt per source departure. Native windows never leave their UI thread.
pub(crate) trait Native {
    type Window: Copy + Eq;
    fn foreground(&self) -> Option<Self::Window>;
    fn valid(&self, window: Self::Window) -> bool;
    fn show_capture(&mut self);
    fn hide_capture(&mut self);
    fn minimized(&self, window: Self::Window) -> bool;
    /// A newly visible application that became active in its own input queue
    /// while our capture remained system foreground.
    fn new_activation_target(&mut self) -> Option<Self::Window> {
        None
    }
    /// Queue a restore without activating when the target UI thread handles it.
    fn restore_without_activation(&mut self, window: Self::Window);
    /// Native implementations log a refused attempt, without retrying it.
    fn activate(&mut self, window: Self::Window);
    fn released(&mut self, _decision: ReleaseDecision) {}
}
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ReleaseDecision {
    pub previous_available: bool,
    pub capture_foreground: bool,
    pub previous_valid: bool,
    pub new_application: bool,
    pub previous_minimized: bool,
    pub activation_requested: bool,
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
            let previous = self.previous.take();
            let mut decision = ReleaseDecision {
                previous_available: previous.is_some(),
                capture_foreground: native.foreground() == Some(capture),
                ..Default::default()
            };
            if decision.capture_foreground {
                let new = native
                    .new_activation_target()
                    .filter(|window| *window != capture && native.valid(*window));
                let previous = previous.filter(|window| native.valid(*window));
                decision.previous_valid = previous.is_some();
                decision.new_application = new.is_some();
                // Windows may mark a new visible window active in its own UI
                // queue while denying it system foreground. Returning to the
                // old application would override that newly expressed intent.
                if let Some(target) = new.or(previous)
                    && native.foreground() == Some(capture)
                {
                    decision.previous_minimized = new.is_none() && native.minimized(target);
                    if decision.previous_minimized {
                        // The other UI thread may process this after a new application
                        // takes foreground. Only the immediate request below may activate;
                        // the deferred show must preserve whichever window is then foreground.
                        native.restore_without_activation(target);
                    }
                    decision.activation_requested = true;
                    native.activate(target);
                }
            }
            native.hide_capture();
            native.released(decision);
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
        new_application: Option<u64>,
        invalid: Option<u64>,
        foreground_during_selection: Option<u64>,
        decision: Option<ReleaseDecision>,
    }
    impl Native for Desktop {
        type Window = u64;
        fn foreground(&self) -> Option<u64> {
            self.foreground
        }
        fn valid(&self, window: u64) -> bool {
            !self.closed && self.invalid != Some(window)
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
        fn new_activation_target(&mut self) -> Option<u64> {
            if let Some(window) = self.foreground_during_selection {
                self.foreground = Some(window);
            }
            self.new_application
        }
        fn released(&mut self, decision: ReleaseDecision) {
            self.decision = Some(decision);
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
    fn newly_active_application_is_handed_foreground_when_windows_denied_its_background_request() {
        let (mut lease, mut native) = departing();
        // A visible fullscreen application can be active in its own thread
        // while GetForegroundWindow still returns our capture window.
        native.new_application = Some(3);
        native.minimized = true;
        lease.update(false, 2, &mut native);
        assert_eq!(&native.actions[2..], [("activate", 3), ("hide", 2)]);
        assert!(native.pending_restores.is_empty());
        assert_eq!(native.foreground, Some(3));
        let decision = native.decision.unwrap();
        assert!(decision.previous_available && decision.previous_valid && decision.capture_foreground);
        assert!(decision.new_application && decision.activation_requested);
        assert!(!decision.previous_minimized);
    }
    #[test]
    fn existing_foreground_owner_wins_over_a_pending_application() {
        let (mut lease, mut native) = departing();
        native.foreground = Some(4);
        native.new_application = Some(3);
        lease.update(false, 2, &mut native);
        assert_eq!(&native.actions[2..], [("hide", 2)]);
        assert_eq!(native.foreground, Some(4));
    }
    #[test]
    fn foreground_is_checked_again_after_inspecting_other_windows() {
        let (mut lease, mut native) = departing();
        native.new_application = Some(3);
        native.foreground_during_selection = Some(4);
        lease.update(false, 2, &mut native);
        assert_eq!(&native.actions[2..], [("hide", 2)]);
        assert_eq!(native.foreground, Some(4));
    }
    #[test]
    fn closed_new_window_falls_back_to_the_saved_application() {
        let (mut lease, mut native) = departing();
        native.new_application = Some(3);
        native.invalid = Some(3);
        lease.update(false, 2, &mut native);
        assert_eq!(&native.actions[2..], [("activate", 1), ("hide", 2)]);
    }
    #[test]
    fn destroyed_saved_window_does_not_prevent_handing_to_the_new_application() {
        let (mut lease, mut native) = departing();
        native.new_application = Some(3);
        native.invalid = Some(1);
        lease.update(false, 2, &mut native);
        assert_eq!(&native.actions[2..], [("activate", 3), ("hide", 2)]);
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
