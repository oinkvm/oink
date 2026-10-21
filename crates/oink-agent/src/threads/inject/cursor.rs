//! Read barriers share the native injection FIFO. No native cursor query runs
//! on a capture callback and an old timer sample cannot rewind routed motion.
use oink_foundation::{Point, Rect, input::InjectEvent};
use oink_input::{
    InputBackend,
    cursor_reconciliation::{Expected, Observation},
};

#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub observation: Observation,
    /// None means query failed, Some(None) means positively unconfined.
    pub clip: Option<Option<Rect>>,
    pub read_us: u64,
}
#[derive(Default)]
pub(super) struct Ledger {
    sequence: u64,
    expected: Option<Expected>,
}
impl Ledger {
    pub fn clear(&mut self) {
        self.sequence = self.sequence.wrapping_add(1);
        self.expected = None;
    }
    pub fn posted(
        &mut self,
        native: &dyn InputBackend,
        scope: u64,
        event: &InjectEvent,
        before: Option<Point>,
        ok: bool,
    ) {
        if !matches!(
            event,
            InjectEvent::MoveTo { .. } | InjectEvent::MoveToWithDelta { .. } | InjectEvent::MoveRelative { .. }
        ) {
            return;
        }
        self.clear();
        if ok && let Some((target, tolerance)) = native.cursor_injection_target(event) {
            self.expected = Some(Expected {
                scope,
                sequence: self.sequence,
                before,
                target,
                tolerance,
            });
        }
    }
    pub fn sample(&self, native: &dyn InputBackend, scope: u64) -> Snapshot {
        let started = std::time::Instant::now();
        let safety = native.input_safety();
        // The clip implementation already excludes Oink's source-capture lease.
        // Query failure is not evidence of release (same policy as today's driver).
        let mut clip = native.system_cursor_clip().ok();
        let mut position = native.cursor_position().ok();
        if native.input_safety() != safety || !safety.active() {
            clip = None;
            position = None;
        }
        Snapshot {
            observation: Observation {
                scope,
                sequence: self.sequence,
                position,
                expected: self.expected,
            },
            clip,
            read_us: started.elapsed().as_micros() as u64,
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_foundation::input::MouseButton;
    use oink_input::cursor_reconciliation::{Decision, Reconciler};
    use std::{sync::Arc, time::Duration};
    #[test]
    fn target_cursor_read_barrier_reports_the_native_receipt_and_never_overtakes_buttons() {
        let backend = Arc::new(oink_test_support::input::FakeInput::new());
        backend.set_cursor(Point::new(100., 100.));
        let handle = crate::threads::inject::start_with_backend(backend.clone(), Box::new(|_| {})).unwrap();
        handle.pointer_scope(7);
        let movement = InjectEvent::MoveTo {
            pos: Point::new(110., 100.),
        };
        let button = InjectEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        };
        for event in [movement.clone(), button.clone()] {
            handle.push(crate::threads::inject::InjectItem::Inject { event, from: None });
        }
        let sample = handle.read_cursor(7).recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(backend.take_injected(), vec![movement, button]);
        assert_eq!(
            sample.observation.expected.unwrap().before,
            Some(Point::new(100., 100.))
        );
        assert_eq!(
            Reconciler::default().observe(7, sample.observation, 0),
            Decision::Expected
        );
        backend.set_cursor(Point::new(400., 100.));
        let sample = handle.read_cursor(7).recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            Reconciler::default().observe(7, sample.observation, 0),
            Decision::Rebase(Point::new(400., 100.))
        );
    }
    #[test]
    fn target_cursor_failed_or_relative_submission_cannot_reuse_absolute_receipt() {
        let backend = oink_test_support::input::FakeInput::new();
        let movement = InjectEvent::MoveTo {
            pos: Point::new(110., 100.),
        };
        let mut ledger = Ledger::default();
        ledger.posted(&backend, 7, &movement, Some(Point::new(100., 100.)), true);
        assert!(ledger.sample(&backend, 7).observation.expected.is_some());
        ledger.posted(&backend, 7, &movement, Some(Point::new(100., 100.)), false);
        assert!(ledger.sample(&backend, 7).observation.expected.is_none());
        ledger.posted(&backend, 7, &movement, Some(Point::new(100., 100.)), true);
        ledger.posted(&backend, 7, &InjectEvent::MoveRelative { dx: 1., dy: 0. }, None, true);
        assert!(ledger.sample(&backend, 7).observation.expected.is_none());
    }
}
