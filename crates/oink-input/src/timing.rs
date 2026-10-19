//! Source-owned input measurements. Native owners only update fixed atomics;
//! the ordinary statistics clock drains maxima once per sample, never per event.
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::Instant;

pub const CALL_CLASSES: usize = 8;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallClass {
    Motion,
    Button,
    Key,
    Scroll,
    Gesture,
    Mode,
    Release,
    Other,
}
impl CallClass {
    pub fn event(event: &crate::InjectEvent) -> Self {
        use crate::InjectEvent::*;
        match event {
            MoveTo { .. } | MoveToWithDelta { .. } | MoveRelative { .. } => Self::Motion,
            Button { .. } => Self::Button,
            Key { .. } | Volume { .. } => Self::Key,
            Scroll { .. } | TouchpadScroll { .. } => Self::Scroll,
            Gesture(_) | TouchpadContacts(_) => Self::Gesture,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    /// Actual sampling window; a stalled ordinary clock never replays old seconds.
    pub window_us: u64,
    pub capture_callbacks: u64,
    pub capture_callback_max_us: u64,
    /// Windows low-level event timestamp to callback entry (coarse OS milliseconds).
    pub capture_os_wait_max_us: u64,
    pub inject_completed: u64,
    pub inject_queue_wait_max_us: u64,
    /// Array indexed by CallClass; includes all time inside the backend call.
    pub inject_native_max_us: [u64; CALL_CLASSES],
    /// Includes the in-flight/deferred item, so a blocked native call stays visible.
    pub oldest_pending_us: u64,
    pub pending: u64,
    pub native_inflight_class: Option<CallClass>,
    pub native_inflight_age_us: u64,
    /// Relative source-to-receiver variation, not absolute one-way latency.
    pub jitter: crate::jitter::Sample,
}
static CAPTURE_COUNT: AtomicU64 = AtomicU64::new(0);
static CAPTURE_MAX: AtomicU64 = AtomicU64::new(0);
static CAPTURE_WAIT: AtomicU64 = AtomicU64::new(0);
pub(crate) struct CaptureTimer(Instant);
impl CaptureTimer {
    pub fn new() -> Self {
        Self(Instant::now())
    }
    #[cfg(windows)]
    pub fn os_wait(milliseconds: u32) {
        CAPTURE_WAIT.fetch_max(u64::from(milliseconds) * 1000, Ordering::Relaxed);
    }
}
impl Drop for CaptureTimer {
    fn drop(&mut self) {
        CAPTURE_COUNT.fetch_add(1, Ordering::Relaxed);
        CAPTURE_MAX.fetch_max(self.0.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}
pub fn take_capture(sample: &mut Sample) {
    sample.capture_callbacks = CAPTURE_COUNT.swap(0, Ordering::Relaxed);
    sample.capture_callback_max_us = CAPTURE_MAX.swap(0, Ordering::Relaxed);
    sample.capture_os_wait_max_us = CAPTURE_WAIT.swap(0, Ordering::Relaxed);
}
/// One timestamp slot per queued item plus the one taken by the serial injector.
/// Scanning is ordinary work; producers publish/clear exactly one slot each.
#[derive(Debug)]
pub struct InjectionTiming {
    start: Instant,
    next: AtomicUsize,
    slots: Box<[AtomicU64]>,
    wait_max: AtomicU64,
    native_max: [AtomicU64; CALL_CLASSES],
    completed: AtomicU64,
    inflight: AtomicU64,
}
#[derive(Debug)]
pub struct Pending {
    owner: Arc<InjectionTiming>,
    slot: usize,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.owner.slots[self.slot].store(0, Ordering::Release);
    }
}
impl InjectionTiming {
    pub fn new(queue_capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            next: AtomicUsize::new(0),
            slots: (0..=queue_capacity).map(|_| AtomicU64::new(0)).collect(),
            wait_max: AtomicU64::new(0),
            native_max: std::array::from_fn(|_| AtomicU64::new(0)),
            completed: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
        })
    }
    fn micros(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.start).as_micros() as u64 + 1
    }
    pub fn pending(self: &Arc<Self>, at: Instant) -> Pending {
        let first = self.next.fetch_add(1, Ordering::Relaxed);
        // Discarding a backlog can free newer slots while an older native call
        // still owns its slot. Find a free slot instead of overwriting that age.
        for offset in 0..self.slots.len() {
            let slot = first.wrapping_add(offset) % self.slots.len();
            if self.slots[slot]
                .compare_exchange(0, self.micros(at), Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Pending {
                    owner: self.clone(),
                    slot,
                };
            }
        }
        panic!("input timing reservation exceeded");
    }

    pub fn oldest_pending(&self) -> Option<Instant> {
        self.slots
            .iter()
            .map(|slot| slot.load(Ordering::Acquire))
            .filter(|stamp| *stamp != 0)
            .min()
            .map(|stamp| self.start + std::time::Duration::from_micros(stamp - 1))
    }
    pub fn waited(&self, micros: u64) {
        self.wait_max.fetch_max(micros, Ordering::Relaxed);
    }
    pub fn native_begin(&self, class: CallClass, at: Instant) {
        self.inflight
            .store((self.micros(at) << 4) | (class as u64 + 1), Ordering::Release);
    }
    pub fn native_end(&self, class: CallClass, micros: u64) {
        self.native_max[class as usize].fetch_max(micros, Ordering::Relaxed);
        self.completed.fetch_add(1, Ordering::Relaxed);
        self.inflight.store(0, Ordering::Release);
    }
    pub fn take(&self, at: Instant) -> Sample {
        let now = self.micros(at);
        let mut oldest = None;
        let mut pending = 0;
        for stamp in &self.slots {
            let stamp = stamp.load(Ordering::Acquire);
            if stamp != 0 {
                pending += 1;
                oldest = Some(oldest.map_or(stamp, |old: u64| old.min(stamp)));
            }
        }
        let native = self.inflight.load(Ordering::Acquire);
        let classes = [
            CallClass::Motion,
            CallClass::Button,
            CallClass::Key,
            CallClass::Scroll,
            CallClass::Gesture,
            CallClass::Mode,
            CallClass::Release,
            CallClass::Other,
        ];
        Sample {
            inject_completed: self.completed.swap(0, Ordering::Relaxed),
            inject_queue_wait_max_us: self.wait_max.swap(0, Ordering::Relaxed),
            inject_native_max_us: std::array::from_fn(|i| self.native_max[i].swap(0, Ordering::Relaxed)),
            oldest_pending_us: oldest.map_or(0, |old| now.saturating_sub(old)),
            pending,
            native_inflight_class: (native != 0).then(|| classes[(native as usize & 15) - 1]),
            native_inflight_age_us: if native == 0 {
                0
            } else {
                now.saturating_sub(native >> 4)
            },
            ..Default::default()
        }
    }
}
#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn maxima_reset_but_blocked_pending_and_native_age_remain_visible() {
        let timing = InjectionTiming::new(2);
        let at = Instant::now();
        let first = timing.pending(at);
        let second = timing.pending(at + std::time::Duration::from_micros(10));
        timing.waited(30);
        timing.waited(20);
        timing.native_begin(CallClass::Motion, at);
        timing.native_end(CallClass::Motion, 100);
        timing.native_begin(CallClass::Key, at);
        let one = timing.take(at + std::time::Duration::from_micros(200));
        assert_eq!(one.inject_queue_wait_max_us, 30);
        assert_eq!(one.inject_native_max_us[0], 100);
        assert_eq!(
            (one.pending, one.oldest_pending_us, one.native_inflight_age_us),
            (2, 200, 200)
        );
        drop(first);
        let two = timing.take(at + std::time::Duration::from_micros(300));
        assert_eq!((two.pending, two.oldest_pending_us), (1, 290));
        assert_eq!(two.inject_queue_wait_max_us, 0);
        assert_eq!(two.native_inflight_class, Some(CallClass::Key));
        drop(second);
        timing.native_end(CallClass::Key, 400);
        assert_eq!(timing.take(at).pending, 0);
    }
}
