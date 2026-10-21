use std::{io, time::Duration};

pub(super) const ACCESS_DENIED: i32 = 0xc0000022u32 as i32;
const DEADLINE: Duration = Duration::from_secs(5);

pub(super) struct RenameError {
    pub error: io::Error,
    pub status: i32,
}

pub(super) struct Outcome {
    pub result: io::Result<()>,
    pub attempts: u32,
    pub elapsed: Duration,
}

pub(super) fn publish(
    mut rename: impl FnMut() -> Result<(), RenameError>,
    mut retry_allowed: impl FnMut() -> bool,
    now: impl Fn() -> Duration,
    mut sleep: impl FnMut(Duration),
) -> Outcome {
    let started = now();
    let mut attempts = 0;
    let result = (|| {
        let mut original = None;
        let mut backoff = Duration::from_millis(10);
        loop {
            // Check again after sleeping: scheduler delays cannot admit a late
            // attempt. An in-flight synchronous native call cannot be cancelled.
            if attempts > 0 && now().saturating_sub(started) >= DEADLINE {
                return Err(original.expect("only access denial admits another attempt"));
            }
            attempts += 1;
            match rename() {
                Ok(()) => return Ok(()),
                Err(failure) if failure.error.raw_os_error() == Some(5) && failure.status == ACCESS_DENIED => {
                    if !retry_allowed() {
                        return Err(failure.error);
                    }
                    original.get_or_insert(failure.error);
                    let elapsed = now().saturating_sub(started);
                    if elapsed >= DEADLINE {
                        return Err(original.expect("the first access denial is retained"));
                    }
                    sleep(backoff.min(DEADLINE - elapsed));
                    backoff = (backoff * 2).min(Duration::from_millis(250));
                }
                Err(failure) => return Err(failure.error),
            }
        }
    })();
    Outcome {
        result,
        attempts,
        elapsed: now().saturating_sub(started),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn denied() -> RenameError {
        RenameError {
            error: io::Error::from_raw_os_error(5),
            status: ACCESS_DENIED,
        }
    }

    #[test]
    fn transient_denials_publish_with_short_capped_backoff() {
        let elapsed = Cell::new(Duration::ZERO);
        let sleeps = RefCell::new(Vec::new());
        let mut calls = 0;
        let outcome = publish(
            || {
                calls += 1;
                if calls <= 8 { Err(denied()) } else { Ok(()) }
            },
            || true,
            || elapsed.get(),
            |delay| {
                sleeps.borrow_mut().push(delay.as_millis());
                elapsed.set(elapsed.get() + delay);
            },
        );
        assert!(outcome.result.is_ok());
        assert_eq!(outcome.attempts, 9);
        assert_eq!(*sleeps.borrow(), [10, 20, 40, 80, 160, 250, 250, 250]);
        assert_eq!(outcome.elapsed, Duration::from_millis(1060));
    }

    #[test]
    fn persistent_denial_stops_at_deadline_without_a_final_rename() {
        let elapsed = Cell::new(Duration::ZERO);
        let attempted_at = RefCell::new(Vec::new());
        let outcome = publish(
            || {
                attempted_at.borrow_mut().push(elapsed.get());
                Err(denied())
            },
            || true,
            || elapsed.get(),
            |delay| elapsed.set(elapsed.get() + delay),
        );
        assert_eq!(outcome.result.unwrap_err().raw_os_error(), Some(5));
        assert_eq!(outcome.elapsed, DEADLINE);
        assert!(outcome.attempts > 1);
        assert_eq!(outcome.attempts as usize, attempted_at.borrow().len());
        assert!(attempted_at.borrow().iter().all(|time| *time < DEADLINE));
    }

    #[test]
    fn other_errors_and_other_native_statuses_never_retry() {
        for (code, status) in [
            (32, ACCESS_DENIED),
            (5, 0xc0000056u32 as i32),
            (3, 0xc000003au32 as i32),
        ] {
            let outcome = publish(
                || {
                    Err(RenameError {
                        error: io::Error::from_raw_os_error(code),
                        status,
                    })
                },
                || panic!("non-retryable failure must not probe the target"),
                || Duration::ZERO,
                |_| panic!("non-retryable failure must not sleep"),
            );
            assert_eq!(outcome.result.unwrap_err().raw_os_error(), Some(code));
            assert_eq!(outcome.attempts, 1);
        }
    }

    #[test]
    fn known_permanent_target_denial_fails_fast() {
        let outcome = publish(
            || Err(denied()),
            || false,
            || Duration::ZERO,
            |_| panic!("permanent denial must not sleep"),
        );
        assert_eq!(outcome.result.unwrap_err().raw_os_error(), Some(5));
        assert_eq!(outcome.attempts, 1);
    }

    #[test]
    fn another_error_after_a_denial_is_returned_without_more_waiting() {
        let elapsed = Cell::new(Duration::ZERO);
        let mut calls = 0;
        let outcome = publish(
            || {
                calls += 1;
                Err(if calls == 1 {
                    denied()
                } else {
                    RenameError {
                        error: io::Error::from_raw_os_error(32),
                        status: 0xc0000043u32 as i32,
                    }
                })
            },
            || true,
            || elapsed.get(),
            |delay| elapsed.set(elapsed.get() + delay),
        );
        assert_eq!(outcome.result.unwrap_err().raw_os_error(), Some(32));
        assert_eq!(outcome.attempts, 2);
        assert_eq!(outcome.elapsed, Duration::from_millis(10));
    }

    #[test]
    fn native_call_time_consumes_the_retry_budget() {
        let elapsed = Cell::new(Duration::ZERO);
        let outcome = publish(
            || {
                elapsed.set(Duration::from_secs(6));
                Err(denied())
            },
            || true,
            || elapsed.get(),
            |_| panic!("native call already exhausted the budget"),
        );
        assert_eq!(outcome.result.unwrap_err().raw_os_error(), Some(5));
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.elapsed, Duration::from_secs(6));
    }

    #[test]
    fn overslept_backoff_cannot_admit_an_attempt_after_deadline() {
        let elapsed = Cell::new(Duration::ZERO);
        let outcome = publish(
            || Err(denied()),
            || true,
            || elapsed.get(),
            |_| elapsed.set(DEADLINE + Duration::from_secs(1)),
        );
        assert_eq!(outcome.result.unwrap_err().raw_os_error(), Some(5));
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.elapsed, Duration::from_secs(6));
    }
}
