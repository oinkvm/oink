//! Monotonic age of bounded queued and in-flight work. Guards retain timestamps
//! across ownership transfers; generation invalidation retires obsolete work.
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};
const RESERVED: u64 = u64::MAX;
#[derive(Debug, Default)]
struct Slot {
    key: AtomicU64,
    stamp: AtomicU64,
    epoch: AtomicU64,
}
impl Slot {
    fn read(&self) -> Option<(u64, u64, u64)> {
        let key = self.key.load(Ordering::Acquire);
        if key == 0 || key == RESERVED {
            return None;
        }
        let stamp = self.stamp.load(Ordering::Relaxed);
        let epoch = self.epoch.load(Ordering::Relaxed);
        (self.key.load(Ordering::Acquire) == key).then_some((key, stamp, epoch))
    }
}
#[derive(Debug)]
pub struct PendingAge {
    start: Instant,
    next: AtomicUsize,
    identity: AtomicU64,
    slots: Box<[Slot]>,
}
#[derive(Debug)]
pub struct Pending {
    owner: Arc<PendingAge>,
    slot: usize,
    key: u64,
}
impl Drop for Pending {
    fn drop(&mut self) {
        // An invalidated guard must never clear a newer occupant of its slot.
        let _ = self.owner.slots[self.slot]
            .key
            .compare_exchange(self.key, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}
impl PendingAge {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            next: AtomicUsize::new(0),
            identity: AtomicU64::new(1),
            slots: (0..capacity).map(|_| Slot::default()).collect(),
        })
    }
    pub fn track(self: &Arc<Self>, at: Instant) -> Option<Pending> {
        self.track_epoch(at, 0)
    }
    pub fn track_epoch(self: &Arc<Self>, at: Instant, epoch: u64) -> Option<Pending> {
        let first = self.next.fetch_add(1, Ordering::Relaxed);
        let stamp = at.saturating_duration_since(self.start).as_nanos() as u64;
        for offset in 0..self.slots.len() {
            let index = first.wrapping_add(offset) % self.slots.len();
            let slot = &self.slots[index];
            if slot
                .key
                .compare_exchange(0, RESERVED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let key = self.identity.fetch_add(1, Ordering::Relaxed);
                slot.stamp.store(stamp, Ordering::Relaxed);
                slot.epoch.store(epoch, Ordering::Relaxed);
                slot.key.store(key, Ordering::Release);
                return Some(Pending {
                    owner: self.clone(),
                    slot: index,
                    key,
                });
            }
        }
        None
    }
    pub fn discard_before(&self, epoch: u64) {
        for slot in &self.slots {
            if let Some((key, _, previous)) = slot.read()
                && previous < epoch
            {
                let _ = slot.key.compare_exchange(key, 0, Ordering::AcqRel, Ordering::Acquire);
            }
        }
    }
    pub fn oldest(&self) -> Option<Instant> {
        self.oldest_matching(|_| true)
    }
    pub fn oldest_in_epoch(&self, epoch: u64) -> Option<Instant> {
        self.oldest_matching(|current| current == epoch)
    }
    fn oldest_matching(&self, accepts: impl Fn(u64) -> bool) -> Option<Instant> {
        self.slots
            .iter()
            .filter_map(|slot| slot.read())
            .filter(|(_, _, epoch)| accepts(*epoch))
            .map(|(_, stamp, _)| stamp)
            .min()
            .map(|stamp| self.start + std::time::Duration::from_nanos(stamp))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oldest_survives_queue_drain_until_inflight_completion() {
        let age = PendingAge::new(3);
        let at = Instant::now();
        let flight = age.track(at).unwrap();
        for n in 1..8 {
            let queued = age.track(at + std::time::Duration::from_millis(n)).unwrap();
            assert_eq!(age.oldest(), Some(at));
            drop(queued);
        }
        drop(flight);
        assert_eq!(age.oldest(), None);
    }
    #[test]
    fn old_guard_cannot_clear_reused_slot_after_epoch_invalidation() {
        let age = PendingAge::new(1);
        let at = Instant::now();
        let old = age.track_epoch(at, 1).unwrap();
        age.discard_before(2);
        let fresh = age.track_epoch(at, 2).unwrap();
        drop(old);
        assert_eq!(age.oldest_in_epoch(1), None);
        assert_eq!(age.oldest_in_epoch(2), Some(at));
        drop(fresh);
        assert_eq!(age.oldest(), None);
    }
}
