//! The announcement debounce of local clipboard changes (clipboard.md
//! 2026-11-12), shared by the PC capture (`oink_clipboard::sync::Ordering`)
//! and the phone's privileged clipboard capture: a clipboard written several
//! times in quick succession is read and announced once, in its last state.
use std::time::{Duration, Instant};

/// Quiet period after the last change before the clipboard is read and
/// announced. 250 ms covers an application's double write observed ~180 ms
/// apart (75 ms did not): an announced intermediate state is replaced
/// (`Superseded`) before anyone can read it, and phones keep it as a record
/// that never loads.
pub const SOURCE_QUIET_PERIOD: Duration = Duration::from_millis(250);
/// A clipboard that keeps changing is still announced: at most this long
/// after the first change of a burst, with its state at that moment.
pub const SOURCE_BURST_CAP: Duration = Duration::from_secs(1);

/// An uncaptured burst of local changes: when its first change was seen and
/// when it is captured (the capture reads whatever the clipboard holds then).
#[derive(Clone, Copy, Debug, Default)]
pub struct Burst {
    pending: Option<(Instant, Instant)>,
}

impl Burst {
    /// A local change: each one restarts the quiet period, but never past the
    /// cap counted from the burst's first change. Returns the deadline.
    pub fn change(&mut self, now: Instant) -> Instant {
        let first = self.pending.map_or(now, |(first, _)| first);
        let deadline = (now + SOURCE_QUIET_PERIOD).min(first + SOURCE_BURST_CAP);
        self.pending = Some((first, deadline));
        deadline
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.pending.map(|(_, deadline)| deadline)
    }

    /// True once the deadline passed; the capture this allows ends the burst
    /// (a change seen after it starts a new one).
    pub fn due(&mut self, now: Instant) -> bool {
        if self.pending.is_some_and(|(_, deadline)| now >= deadline) {
            self.pending = None;
            true
        } else {
            false
        }
    }

    /// Drops a pending burst (own write, pause, revocation): nothing of it is
    /// read or announced.
    pub fn clear(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn a_burst_is_captured_once_after_its_last_change_or_at_the_cap() {
        let at = Instant::now();
        let mut burst = Burst::default();
        assert!(!burst.due(at));
        assert_eq!(burst.change(at), at + SOURCE_QUIET_PERIOD);
        assert_eq!(burst.change(at + ms(200)), at + ms(200) + SOURCE_QUIET_PERIOD);
        assert!(!burst.due(at + ms(449)));
        assert!(burst.due(at + ms(450)));
        assert!(!burst.due(at + ms(451)), "the capture ended the burst");
        // A clipboard that keeps changing is captured at the cap.
        let start = at + ms(2000);
        for step in 0..10 {
            burst.change(start + ms(step * 150));
        }
        assert_eq!(burst.deadline(), Some(start + SOURCE_BURST_CAP));
        burst.clear();
        assert!(!burst.due(start + ms(5000)));
    }
}
