//! Callback-side recording: only atomics and a preallocated lock-free ring.
//! No allocation, locks, clocks, syscalls, formatting, wakeup, disk, or network.
//! Owners supply timestamps they already measured. Ordinary source-statistics
//! workers drain the ring; callbacks never wake or call the report worker.
use crate::{Metric, Transport};
use std::{cell::UnsafeCell, mem::MaybeUninit};
use std::{
    sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
    time::Instant,
};
#[derive(Clone, Copy, Debug)]
pub struct Stamp {
    pub generation: u64,
    pub at: Instant,
    pub transport: Option<Transport>,
}
#[derive(Clone, Copy, Debug)]
pub struct NumericInterval {
    pub stamp: Stamp,
    pub metric: Metric,
    pub elapsed_us: u64,
}
#[derive(Debug)]
pub struct Ring {
    generation: AtomicU64,
    enabled_offset_us: AtomicU64,
    epoch: Instant,
    phase: AtomicU64,
    records: Box<[Slot]>,
    next: AtomicUsize,
    read: AtomicUsize,
}
impl Ring {
    /// Ordinary owner only. The random phase is chosen there, never in a callback.
    pub fn new(capacity: usize, epoch: Instant, input_phase: u8) -> Self {
        Self {
            generation: AtomicU64::new(0),
            enabled_offset_us: AtomicU64::new(0),
            epoch,
            phase: AtomicU64::new(input_phase as u64 % 100),
            records: (0..capacity.max(1))
                .map(|_| Slot {
                    state: AtomicU8::new(0),
                    value: UnsafeCell::new(MaybeUninit::uninit()),
                })
                .collect(),
            next: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        }
    }
    pub fn generation(&self) -> Option<u64> {
        let g = self.generation.load(Ordering::Acquire);
        (g != 0).then_some(g)
    }
    /// Ordinary owner; release-publishes the new collection boundary.
    pub fn arm(&self, generation: u64, at: Instant) {
        self.enabled_offset_us.store(
            at.saturating_duration_since(self.epoch).as_micros() as u64,
            Ordering::Relaxed,
        );
        self.generation.store(generation, Ordering::Release);
    }
    pub fn disarm(&self) {
        self.generation.store(0, Ordering::Release);
    }
    #[inline]
    pub fn stamp(&self, at: Instant) -> Option<Stamp> {
        let generation = self.generation()?;
        let elapsed = at.checked_duration_since(self.epoch)?.as_micros();
        if elapsed < self.enabled_offset_us.load(Ordering::Relaxed) as u128 {
            return None;
        }
        Some(Stamp {
            generation,
            at,
            transport: None,
        })
    }
    #[inline]
    pub fn input_stamp(&self, at: Instant, lossy: bool) -> Option<Stamp> {
        // Disabled: one acquire load; no phase counter or clock read.
        let mut stamp = self.stamp(at)?;
        if !self.phase.fetch_add(1, Ordering::Relaxed).is_multiple_of(100) {
            return None;
        }
        stamp.transport = Some(if lossy { Transport::Lossy } else { Transport::Reliable });
        Some(stamp)
    }
    #[inline]
    pub fn record(&self, stamp: Stamp, metric: Metric, end: Instant) {
        if self.generation() != Some(stamp.generation) {
            return;
        }
        let Some(elapsed) = end.checked_duration_since(stamp.at) else {
            return;
        };
        if elapsed.as_secs() > 120 || elapsed.as_micros() > 120_000_000 {
            return;
        }
        // Each slot is a single-producer/single-consumer ownership handoff.
        // One CAS attempt only: a full slot or contending producer loses this
        // sample. Unlike a generic channel, this cannot spin/yield a thread.
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.records.len();
        let slot = &self.records[index];
        if slot
            .state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        // SAFETY: state 1 exclusively owns this slot until release publication.
        unsafe {
            (*slot.value.get()).write(NumericInterval {
                stamp,
                metric,
                elapsed_us: elapsed.as_micros() as u64,
            });
        }
        slot.state.store(2, Ordering::Release);
    }
    /// Ordinary owner only.
    pub fn pop(&self) -> Option<NumericInterval> {
        for _ in 0..self.records.len() {
            let index = self.read.fetch_add(1, Ordering::Relaxed) % self.records.len();
            let slot = &self.records[index];
            if slot
                .state
                .compare_exchange(2, 3, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            // SAFETY: acquire observed the initialized publication; state 3
            // excludes all writers until this read releases the slot below.
            let value = unsafe { (*slot.value.get()).assume_init_read() };
            slot.state.store(0, Ordering::Release);
            return Some(value);
        }
        None
    }
    pub fn clear(&self) {
        while self.pop().is_some() {}
    }
}

struct Slot {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<NumericInterval>>,
}
impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot").finish_non_exhaustive()
    }
}
// SAFETY: the slot's atomic publication state provides exclusive value ownership.
// Both sides make one bounded attempt, and no borrowed reference escapes.
unsafe impl Sync for Slot {}
