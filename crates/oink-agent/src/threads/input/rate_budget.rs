//! A two-event token bucket. Refill follows elapsed time, not the last flush.
use crate::api::EventFrequency;
use std::time::{Duration, Instant};

const CAPACITY: u32 = 2;

pub(super) struct RateBudget {
    frequency: EventFrequency,
    refilled: Option<Instant>,
    credit: Duration,
}
impl Default for RateBudget {
    fn default() -> Self {
        Self {
            frequency: EventFrequency::default(),
            refilled: None,
            credit: Duration::ZERO,
        }
    }
}
impl RateBudget {
    pub(super) fn frequency(&self) -> EventFrequency {
        self.frequency
    }

    pub(super) fn reset(&mut self, frequency: EventFrequency, now: Instant) {
        self.frequency = frequency;
        self.refilled = None;
        self.refill(now);
    }

    fn interval(&self) -> Option<Duration> {
        match self.frequency {
            EventFrequency::Hz125 => Some(Duration::from_millis(8)),
            EventFrequency::Unlimited => None,
        }
    }

    fn refill(&mut self, now: Instant) {
        let Some(interval) = self.interval() else {
            return;
        };
        let capacity = interval * CAPACITY;
        self.credit = match self.refilled {
            Some(previous) => self
                .credit
                .saturating_add(now.saturating_duration_since(previous))
                .min(capacity),
            None => capacity,
        };
        // A caller can take its timestamp before waiting for the coalescer lock.
        // Never move the clock back and subsequently count that time twice.
        self.refilled = Some(self.refilled.map_or(now, |previous| previous.max(now)));
    }

    pub(super) fn ready_at(&mut self, now: Instant) -> Instant {
        self.refill(now);
        match self.interval() {
            Some(interval) if self.credit < interval => self.refilled.unwrap_or(now) + (interval - self.credit),
            _ => now,
        }
    }

    pub(super) fn consume(&mut self, now: Instant) {
        self.refill(now);
        if let Some(interval) = self.interval() {
            // An ordering boundary may flush early. It uses any current credit;
            // it does not reset the clock or delay the key/button that follows.
            self.credit = self.credit.saturating_sub(interval);
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn a_timestamp_taken_before_the_lock_cannot_refill_twice() {
        let now = Instant::now();
        let mut budget = RateBudget::default();
        budget.consume(now);
        budget.consume(now);
        budget.consume(now + Duration::from_millis(8));
        assert_eq!(budget.ready_at(now), now + Duration::from_millis(16));
        assert_eq!(
            budget.ready_at(now + Duration::from_millis(8)),
            now + Duration::from_millis(16)
        );
    }
}
