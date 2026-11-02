//! One foreground attempt per source departure. Native windows never leave their UI thread.
pub(crate) trait Native {
    type Window: Copy + Eq;
    fn foreground(&self) -> Option<Self::Window>;
    fn valid(&self, window: Self::Window) -> bool;
    fn show_capture(&mut self);
    fn hide_capture(&mut self);
    fn minimized(&self, window: Self::Window) -> bool;
    fn restore(&mut self, window: Self::Window);
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
                    native.restore(previous);
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
        fn restore(&mut self, window: u64) {
            self.actions.push(("restore", window));
            self.minimized = false;
        }
        fn activate(&mut self, window: u64) {
            self.actions.push(("activate", window));
            if !self.refuse {
                self.foreground = Some(window);
            }
        }
    }
    #[test]
    fn takes_once_and_restores_a_minimized_application_only_while_still_foreground() {
        for (current, closed) in [(Some(2), false), (Some(3), false), (Some(2), true)] {
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
