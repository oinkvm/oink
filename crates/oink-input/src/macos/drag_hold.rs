//! The native freeze and anchor sample must be one input-owned transition.
use crate::InputError;
use oink_foundation::{Point, dnd::DragAnchor};

pub(super) fn freeze_anchor(
    generation: u64,
    freeze: impl FnOnce() -> Result<(), InputError>,
    sample: impl FnOnce() -> Result<Point, InputError>,
) -> Result<DragAnchor, InputError> {
    freeze()?;
    let at = sample()?;
    Ok(DragAnchor {
        generation,
        at,
        surface: 0,
    })
}

/// Publish the submitted endpoint under the transition lock, then release it
/// before waiting for the Session tap. The capture run loop also takes this
/// lock between callbacks; holding it through ACK waiting can stall the loop
/// until the 250ms deadline. No timeout retry or extra native event is added.
/// This fixes the lock ordering found in macOS 14.7.8/26.6.2 build-53 traces;
/// the revised native path still needs installed-build validation.
pub(super) fn wait_unlocked<T, R>(guard: std::sync::MutexGuard<'_, ()>, submitted: T, wait: impl FnOnce(T) -> R) -> R {
    drop(guard);
    wait(submitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn held_post_allows_capture_loop_to_run_before_waiting_for_ack() {
        let transition = std::sync::Mutex::new(());
        let endpoint = std::sync::Mutex::new(None);
        let guard = transition.lock().unwrap();
        // Simulate the accepted OS post and both published anchor ledgers.
        *endpoint.lock().unwrap() = Some(Point::new(100., 2.));
        let (ack, receipt) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                // The capture loop takes the transition lock before entering
                // its next run-loop turn and delivering the marked event.
                let _capture_turn = transition.lock().unwrap();
                assert_eq!(*endpoint.lock().unwrap(), Some(Point::new(100., 2.)));
                ack.send(()).unwrap();
            });
            wait_unlocked(guard, receipt, |receipt| {
                receipt.recv_timeout(std::time::Duration::from_millis(250)).unwrap();
            });
        });
    }

    #[test]
    fn freeze_anchor_samples_after_physical_and_pending_injected_motion_are_fenced() {
        let cursor = Cell::new(Point::new(655., 0.));
        let fenced = Cell::new(false);
        let anchor = freeze_anchor(
            42,
            || {
                // A previously posted event commits while native freeze is
                // being established. This is not the earlier clipboard sample.
                cursor.set(Point::new(661., 0.));
                fenced.set(true);
                Ok(())
            },
            || {
                assert!(fenced.get(), "sampled an anchor before fencing source motion");
                Ok(cursor.get())
            },
        )
        .unwrap();
        assert_eq!(anchor.at, cursor.get());
        assert_eq!(anchor.generation, 42);
    }

    #[test]
    fn freeze_anchor_does_not_publish_after_failed_native_freeze() {
        let result = freeze_anchor(
            42,
            || Err(InputError::Platform("native freeze failed".into())),
            || panic!("a failed transition must not sample/publish an anchor"),
        );
        assert!(result.is_err());
    }
}
