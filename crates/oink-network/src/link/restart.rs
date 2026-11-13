//! Restart pacing for an owner whose network layer ended with a
//! [`NetworkFatal`](super::NetworkFatal). The owner restarts the layer through
//! its normal startup path (PC: the backend host; phone: the app's session);
//! this only decides how long to wait first, so a layer that fails again at
//! once cannot become a restart storm (user 2026-11-12).
use std::time::Duration;

/// Delay before the first restart; doubles per consecutive failure.
pub const FIRST_DELAY: Duration = Duration::from_secs(1);
/// Upper bound of the delay.
pub const MAX_DELAY: Duration = Duration::from_secs(30);
/// A layer that ran at least this long before failing counts as stable: the
/// next delay starts over at [`FIRST_DELAY`].
pub const STABLE_RUN: Duration = Duration::from_secs(60);

/// Exponential back-off: 1 s, 2 s, 4 s, ... capped at 30 s, reset after a
/// stable minute. One per owner, kept across the restarts it paces.
#[derive(Clone, Debug, Default)]
pub struct Backoff {
    /// Consecutive failures that each followed a run shorter than [`STABLE_RUN`].
    failures: u32,
}

impl Backoff {
    /// No failure yet (usable in a `static`).
    pub const fn new() -> Self {
        Self { failures: 0 }
    }
    /// The layer failed after running for `ran_for`: how long to wait before
    /// starting it again, and which consecutive restart this is (1-based).
    pub fn next(&mut self, ran_for: Duration) -> (Duration, u32) {
        if ran_for >= STABLE_RUN {
            self.failures = 0;
        }
        let delay = FIRST_DELAY
            .checked_mul(1u32 << self.failures.min(16))
            .map_or(MAX_DELAY, |delay| delay.min(MAX_DELAY));
        self.failures = self.failures.saturating_add(1);
        (delay, self.failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_from_one_second_cap_at_thirty_and_reset_after_a_stable_minute() {
        let mut backoff = Backoff::default();
        let quick = Duration::from_secs(2);
        let delays: Vec<_> = (0..8).map(|_| backoff.next(quick)).collect();
        let secs: Vec<_> = delays.iter().map(|(delay, _)| delay.as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(
            delays.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
        // Just short of a stable minute still counts as a quick failure.
        assert_eq!(backoff.next(STABLE_RUN - Duration::from_millis(1)), (MAX_DELAY, 9));
        // A stable minute resets the sequence.
        assert_eq!(backoff.next(STABLE_RUN), (FIRST_DELAY, 1));
        assert_eq!(backoff.next(quick), (Duration::from_secs(2), 2));
        // Saturates without overflow after very many failures.
        for _ in 0..100 {
            backoff.next(Duration::ZERO);
        }
        assert_eq!(backoff.next(Duration::ZERO).0, MAX_DELAY);
    }
}
