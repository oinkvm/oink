//! Single media-reader burst admission, with atomic observations for the control owner.
use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Lost-Sleep safeguard, measured from the last valid media packet received.
/// The source's five-second silence timer normally sleeps first by event.
pub const PACKET_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Bursts {
    epoch: Instant,
    generation: AtomicU64,
    highest: AtomicU64,
    sleeping: AtomicBool,
    last_packet_ns: AtomicU64,
}
impl Default for Bursts {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}
impl Bursts {
    pub fn new(now: Instant) -> Self {
        Self {
            epoch: now,
            generation: AtomicU64::new(0),
            highest: AtomicU64::new(0),
            sleeping: AtomicBool::new(false),
            last_packet_ns: AtomicU64::new(0),
        }
    }
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    /// Called by the sole media reader, only after payload/stream validation.
    pub fn media(&self, burst: u64, sequence: u64, arrived: Instant) -> bool {
        let current = self.generation();
        if burst == 0
            || burst < current
            || (burst == current
                && self.sleeping.load(Ordering::Acquire)
                && sequence <= self.highest.load(Ordering::Acquire))
        {
            return false;
        }
        if burst > current {
            self.highest.store(sequence, Ordering::Release);
            self.generation.store(burst, Ordering::Release);
        } else {
            self.highest.fetch_max(sequence, Ordering::AcqRel);
        }
        self.last_packet_ns.fetch_max(
            arrived
                .saturating_duration_since(self.epoch)
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64,
            Ordering::AcqRel,
        );
        self.sleeping.store(false, Ordering::Release);
        true
    }
    pub fn sleep(&self, burst: u64, sequence: u64) {
        let current = self.generation();
        if burst == 0 || burst < current || (burst == current && sequence < self.highest.load(Ordering::Acquire)) {
            return;
        }
        self.generation.store(burst, Ordering::Release);
        self.highest.store(sequence, Ordering::Release);
        self.sleeping.store(true, Ordering::Release);
    }
    /// A timeout is missing media, not proof of source silence. Only a validated
    /// Sleep event suppresses transport health checks.
    pub fn media_missing(&self, now: Instant, timeout: Duration) -> bool {
        !self.sleeping.load(Ordering::Acquire)
            && now.saturating_duration_since(
                self.epoch + Duration::from_nanos(self.last_packet_ns.load(Ordering::Acquire)),
            ) > timeout
    }
    pub fn asleep(&self, now: Instant) -> bool {
        self.sleeping.load(Ordering::Acquire)
            || now.saturating_duration_since(
                self.epoch + Duration::from_nanos(self.last_packet_ns.load(Ordering::Acquire)),
            ) > PACKET_TIMEOUT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lost_sleep_has_thirty_second_fallback_and_lost_first_media_still_wakes() {
        let now = Instant::now();
        let state = Bursts::new(now);
        assert!(state.media(1, 20, now));
        assert!(!state.asleep(now + Duration::from_secs(29)));
        assert!(!state.asleep(now + Duration::from_secs(30)));
        assert!(state.asleep(now + Duration::from_secs(30) + Duration::from_nanos(1)));
        assert!(state.media(2, 2, now + Duration::from_secs(31))); // sequence 1 lost
        assert!(!state.asleep(now + Duration::from_secs(31)));
        assert_eq!(state.generation(), 2);
    }
    #[test]
    fn late_sleep_old_media_and_old_sequence_cannot_put_new_media_to_sleep() {
        let now = Instant::now();
        let state = Bursts::new(now);
        assert!(state.media(1, 50, now));
        state.sleep(1, 49);
        assert!(!state.asleep(now));
        state.sleep(1, 50);
        assert!(state.asleep(now));
        assert!(!state.media(1, 50, now));
        assert!(state.media(2, 2, now));
        state.sleep(1, 500);
        assert!(!state.asleep(now));
        assert!(!state.media(1, 501, now));
        state.sleep(2, 1);
        assert!(!state.asleep(now));
    }
    #[test]
    fn sleep_can_arrive_before_any_media_in_its_burst() {
        let now = Instant::now();
        let state = Bursts::new(now);
        state.sleep(4, 100);
        assert!(state.asleep(now));
        assert!(!state.media(4, 99, now));
        assert!(state.media(5, 1, now));
        assert!(!state.asleep(now));
    }
}
