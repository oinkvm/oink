//! Route selection policy: pure functions over probe samples and link activity.
//! Timing and thresholds follow the route-selection design (sections 4.2-4.5).
use std::time::{Duration, Instant};

/// Rounds kept per route, and the window the comparisons use.
pub(crate) const KEPT_ROUNDS: usize = 12;
pub(crate) const WINDOW: usize = 6;
/// Consecutive rounds a candidate must be clearly better before it is chosen
/// (user rule: one round a minute, three better rounds in a row). Every round
/// from a candidate's first counts, so three consecutive rounds are also its
/// three samples. Remembered user targets use authentication, not this threshold.
pub(crate) const CONFIRM_ROUNDS: u8 = 3;
/// Background cadence, independent of any open menu: one round a minute
/// (+-1 s), three probes 20 ms apart. A new standby's first round runs as soon
/// as it is authenticated; the current route's first after `CURRENT_SAMPLE`.
/// The crate's own tests over real QUIC connections cannot step a clock, so
/// they run the same schedule on a two-second round; core tests step their
/// clock by `ROUND` and only need it longer than `IDLE_RECHECK`.
pub(crate) const ROUND: Duration = if cfg!(test) {
    Duration::from_secs(2)
} else {
    Duration::from_secs(60)
};
pub(crate) const ROUND_JITTER_MS: u64 = if cfg!(test) { 200 } else { 1000 };
pub(crate) const PROBES: u8 = 3;
pub(crate) const PROBE_SPACING: Duration = Duration::from_millis(20);
pub(crate) const PROBE_LOST: Duration = Duration::from_secs(1);
/// The current route's round sample is the lowest heartbeat RTT of the last
/// seconds of the round, comparable with three probes taken at once.
pub(crate) const CURRENT_SAMPLE: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(5)
};
/// Consecutive fully lost rounds that make an established route unreachable.
pub(crate) const LOST_ROUNDS: u8 = 3;
/// No automatic switch within this time after any switch.
pub(crate) const DWELL: Duration = Duration::from_secs(60);
/// A failed target, or one lost within `DWELL` of being chosen, sits out this long.
pub(crate) const COOLDOWN: Duration = Duration::from_secs(300);
/// The single idle window (decision D2).
pub(crate) const IDLE: Duration = Duration::from_secs(3);
/// A decided switch waiting for idle (or the dwell) is re-checked this often,
/// so a busy moment at the round does not cost a whole round.
pub(crate) const IDLE_RECHECK: Duration = Duration::from_secs(1);
/// Back-off of unreachable addresses, on the one-minute round.
pub(crate) const BACKOFF: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
];
/// A follower's proposal is acted on only while this fresh: it is resent every
/// round, so one and a half rounds always cover the arbiter's next decision.
pub(crate) const PROPOSAL_FRESH: Duration = Duration::from_secs(90);
/// The arbiter rejects a proposal whose median differs from its own QUIC RTT
/// estimate of that connection by more than this factor.
pub(crate) const PROPOSAL_TOLERANCE: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stats {
    pub median: Duration,
    pub p90: Duration,
    pub rounds: usize,
    pub losses: usize,
}

/// Statistics of the newest `WINDOW` rounds (`None` is a lost round).
pub(crate) fn stats<'a>(rounds: impl DoubleEndedIterator<Item = &'a Option<Duration>>) -> Option<Stats> {
    let window: Vec<_> = rounds.rev().take(WINDOW).copied().collect();
    let mut values: Vec<_> = window.iter().flatten().copied().collect();
    if values.is_empty() {
        return None;
    }
    values.sort();
    let median = if values.len() % 2 == 1 {
        values[values.len() / 2]
    } else {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2
    };
    let p90 = values[((values.len() * 9).div_ceil(10)).saturating_sub(1)];
    Some(Stats {
        median,
        p90,
        rounds: window.len(),
        losses: window.len() - values.len(),
    })
}

/// One round's comparison (section 4.3): clearly lower median, no worse p90,
/// none lost in the window. The consecutive-round count (`CONFIRM_ROUNDS`),
/// cool-down and idle are separate; a separate minimum number of samples
/// would push "three rounds in a row" to five.
pub(crate) fn clearly_better(current: Stats, candidate: Stats) -> bool {
    let margin = Duration::from_millis(2).max(current.median * 3 / 10);
    candidate.losses == 0
        && current
            .median
            .checked_sub(candidate.median)
            .is_some_and(|gain| gain >= margin)
        && candidate.p90 <= current.p90
}

/// Activity of one link, observed by the arbiter (it sees both directions).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Activity {
    /// Newest input event, Low operation end or audio packet.
    pub last: Option<Instant>,
    /// A key or mouse button is held in either direction.
    pub held: bool,
    /// Open Low operations (file, large clipboard, drag content, wallpaper...).
    pub low_open: usize,
}

/// Decision D2: no input event, Low operation or audio packet for 3 s, nothing held,
/// no Low operation open. There is no upper bound on the wait.
pub(crate) fn idle(activity: Activity, now: Instant) -> bool {
    !activity.held
        && activity.low_open == 0
        && activity
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= IDLE)
}

/// Whether a follower's measured median agrees with the arbiter's own estimate.
pub(crate) fn proposal_plausible(median: Duration, quinn: Option<Duration>) -> bool {
    let Some(quinn) = quinn else {
        return false;
    };
    median <= quinn * PROPOSAL_TOLERANCE && quinn <= median * PROPOSAL_TOLERANCE
}

pub(crate) fn backoff(failures: u8) -> Duration {
    BACKOFF[usize::from(failures.saturating_sub(1)).min(BACKOFF.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ms(value: f64) -> Option<Duration> {
        Some(Duration::from_micros((value * 1000.).round() as u64))
    }
    fn window(values: &[Option<Duration>]) -> Stats {
        stats(values.iter()).unwrap()
    }

    #[test]
    fn thresholds_need_two_ms_or_thirty_percent() {
        let current = window(&[ms(10.); 6]);
        // exactly 30 % of 10 ms = 3 ms gain: switch
        assert!(clearly_better(current, window(&[ms(7.); 6])));
        assert!(!clearly_better(current, window(&[ms(7.1); 6])));
        // small values: the 2 ms floor applies
        let current = window(&[ms(4.); 6]);
        assert!(clearly_better(current, window(&[ms(2.); 6])));
        assert!(!clearly_better(current, window(&[ms(2.1); 6])));
        // the design's examples
        assert!(clearly_better(window(&[ms(13.); 6]), window(&[ms(0.4); 6])));
        assert!(clearly_better(window(&[ms(4.); 6]), window(&[ms(0.5); 6])));
        assert!(!clearly_better(window(&[ms(0.5); 6]), window(&[ms(0.3); 6])));
        assert!(!clearly_better(window(&[ms(1.); 6]), window(&[ms(2.); 6])));
    }

    #[test]
    fn p90_and_loss_block_a_switch_and_one_round_compares() {
        let current = window(&[ms(10.), ms(10.), ms(10.), ms(10.), ms(10.), ms(11.)]);
        let spiky = window(&[ms(1.), ms(1.), ms(1.), ms(1.), ms(1.), ms(30.)]);
        assert!(!clearly_better(current, spiky));
        let lossy = window(&[ms(1.), ms(1.), ms(1.), ms(1.), ms(1.), None]);
        assert_eq!(lossy.losses, 1);
        assert!(!clearly_better(current, lossy));
        // Every round compares from the first one; the three-in-a-row count
        // lives with the caller.
        assert!(clearly_better(current, window(&[ms(1.)])));
    }

    #[test]
    fn statistics_use_the_newest_six_rounds() {
        let mut rounds = vec![ms(50.); 6];
        rounds.extend([ms(1.); 6]);
        let value = stats(rounds.iter()).unwrap();
        assert_eq!(value.median, Duration::from_millis(1));
        assert_eq!(value.rounds, 6);
        assert!(stats([None, None].iter()).is_none());
    }

    #[test]
    fn idle_window_is_exactly_three_seconds_and_held_or_low_is_busy() {
        let now = Instant::now();
        let at = |back: f64| Activity {
            last: Some(now - Duration::from_secs_f64(back)),
            ..Default::default()
        };
        assert!(idle(at(3.0), now));
        assert!(!idle(at(2.9), now));
        assert!(idle(Activity::default(), now));
        assert!(!idle(Activity { held: true, ..at(60.) }, now));
        assert!(!idle(Activity { low_open: 1, ..at(60.) }, now));
    }

    #[test]
    fn proposals_must_agree_with_quinn_within_twice() {
        let ms = Duration::from_millis;
        assert!(proposal_plausible(ms(2), Some(ms(4))));
        assert!(!proposal_plausible(ms(2), Some(ms(5))));
        assert!(!proposal_plausible(ms(5), Some(ms(2))));
        assert!(!proposal_plausible(ms(5), None));
    }

    #[test]
    fn backoff_steps_and_saturates() {
        assert_eq!(backoff(1), Duration::from_secs(60));
        assert_eq!(backoff(2), Duration::from_secs(120));
        assert_eq!(backoff(3), Duration::from_secs(300));
        assert_eq!(backoff(40), Duration::from_secs(300));
    }
}
