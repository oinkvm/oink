//! Local capture scheduling. MT source timestamps are not CG/uptime timestamps.
use oink_foundation::contacts::Sample;

#[derive(Clone, Copy, Debug)]
pub(crate) struct TimedSample {
    pub sample: Sample,
    pub ready_us: u64,
    pub token: u64,
}
impl TimedSample {
    pub fn new(sample: Sample, received_us: u64) -> Self {
        Self {
            ready_us: received_us.saturating_add(8_000),
            sample,
            token: 0,
        }
    }
    pub fn remaining_us(&self, now_us: u64) -> u64 {
        self.ready_us.saturating_sub(now_us)
    }
}

use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicU64, Ordering};

// One CAS orders native callback entry/completion against focus boundaries.
// Bits 0/1: known/touching, bit 2: idle at entry; bits 3..10: callbacks in
// flight (one native subscription); bits 11..63: monotonically advancing epoch.
const FLAGS: u64 = 7;
const CALLBACK: u64 = 8;
const CALLBACKS: u64 = 0x7f8;
const EPOCH: u64 = 1 << 11;

/// Latest validated state plus one atomically captured boundary idle snapshot.
#[derive(Default)]
pub(crate) struct Gate(AtomicU64);
#[derive(Clone, Copy, Debug)]
pub(crate) struct Status(u64);
impl Status {
    pub fn token(self) -> u64 {
        self.0 & !(CALLBACKS | 3)
    }
    pub fn touching(self) -> bool {
        self.0 & 2 != 0
    }
    pub fn observed(self) -> bool {
        self.0 & 1 != 0
    }
    pub fn validated_idle(self) -> bool {
        self.0 & (CALLBACKS | 3) == 1
    }
    fn idle_at_entry(self) -> bool {
        self.0 & 4 != 0
    }
}
impl Gate {
    pub fn status(&self) -> Status {
        Status(self.0.load(Ordering::Acquire))
    }
    pub fn begin(&self) -> Status {
        Status(self.0.fetch_add(CALLBACK, Ordering::AcqRel))
    }
    pub fn finish(&self, touching: bool) -> (Status, Status) {
        self.update(touching, true)
    }
    pub fn observe(&self, touching: bool) -> (Status, Status) {
        self.update(touching, false)
    }
    fn update(&self, touching: bool, finish: bool) -> (Status, Status) {
        let bits = 1 | (u64::from(touching) * 2);
        let old = self
            .0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                Some(((old - if finish { CALLBACK } else { 0 }) & !3) | bits)
            })
            .unwrap();
        (
            Status(old),
            Status(((old - if finish { CALLBACK } else { 0 }) & !3) | bits),
        )
    }
    pub fn boundary(&self) {
        self.0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                let idle = if Status(old).validated_idle() { 4 } else { 0 };
                Some((old.wrapping_add(EPOCH) & !FLAGS) | (old & 3) | idle)
            })
            .unwrap();
    }
}

#[derive(Default)]
pub(crate) struct Delivery {
    pub pending: Option<TimedSample>,
    token: Option<u64>,
    seed_lift: bool,
    pub attested_lift: bool,
    boundary_pending: bool,
}
impl Delivery {
    pub fn sync(&mut self, status: Status) {
        if self.token != Some(status.token()) {
            self.pending = None;
            self.token = Some(status.token());
            self.seed_lift = status.idle_at_entry();
            self.boundary_pending = true;
        }
    }
    pub fn take_boundary(&mut self, status: Status) -> bool {
        self.sync(status);
        std::mem::take(&mut self.boundary_pending)
    }
    pub fn pop(&mut self, queue: &ArrayQueue<TimedSample>, status: Status, now_us: u64) -> Option<Sample> {
        self.sync(status);
        self.attested_lift = false;
        // A mode commit may race the caller's preceding take_boundary(). Never
        // expose a frame of the new epoch until its control barrier is consumed.
        if self.boundary_pending {
            return None;
        }
        // A producer cannot extend a drain indefinitely on the capture owner.
        for _ in 0..=queue.capacity() {
            let sample = self.pending.take().or_else(|| queue.pop())?;
            if sample.token != status.token() {
                continue;
            }
            if sample.remaining_us(now_us) != 0 {
                self.pending = Some(sample);
                return None;
            }
            if std::mem::take(&mut self.seed_lift) && sample.sample.len != 0 {
                // Attest the entry's atomic, validated idle snapshot, without
                // exposing local geometry or admitting a navigation touch. Use
                // the following frame's source clock, never a CG/MT subtraction.
                self.pending = Some(sample);
                self.attested_lift = true;
                return Some(Sample {
                    len: 0,
                    contacts: Default::default(),
                    ..sample.sample
                });
            }
            return Some(sample.sample);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quartz_wait_uses_receipt_clock_with_separated_source_domains() {
        for source_us in [1_704_673_432_000, 2_999_999_999_000] {
            let received_us = 2_099_254_623_000;
            let queued = TimedSample::new(
                Sample {
                    time_us: source_us,
                    ..Default::default()
                },
                received_us,
            );
            assert_eq!(queued.remaining_us(received_us), 8_000);
            assert_eq!(queued.remaining_us(received_us + 7_999), 1);
            assert_eq!(queued.remaining_us(received_us + 8_000), 0);
            assert_eq!(
                queued.sample.time_us, source_us,
                "wire timestamp stays in the MT domain"
            );
        }
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    fn queued(gate: &Gate, touching: bool, received_us: u64) -> TimedSample {
        let (_, status) = gate.observe(touching);
        let mut sample = TimedSample::new(
            Sample {
                len: u8::from(touching),
                time_us: received_us / 2,
                ..Default::default()
            },
            received_us,
        );
        sample.token = status.token();
        sample
    }
    #[test]
    fn entry_drops_queued_local_lift_and_requires_a_current_lift_when_down() {
        let gate = Gate::default();
        let q = ArrayQueue::new(8);
        let mut delivery = Delivery::default();
        q.push(queued(&gate, false, 1000)).unwrap();
        assert!(delivery.take_boundary(gate.status()));
        assert!(!delivery.take_boundary(gate.status()));
        gate.observe(true); // Local geometry is intentionally not queued.
        assert!(gate.status().observed());
        assert!(gate.status().touching());
        gate.boundary();
        assert!(
            delivery.pop(&q, gate.status(), 20000).is_none(),
            "new epoch cannot overtake its barrier"
        );
        assert!(delivery.take_boundary(gate.status()));
        assert!(!delivery.take_boundary(gate.status()));
        q.push(queued(&gate, true, 2000)).unwrap();
        assert_eq!(delivery.pop(&q, gate.status(), 20_000).unwrap().len, 1);
        assert!(
            delivery.pop(&q, gate.status(), 20_000).is_none(),
            "old zero must not follow the entry"
        );
        q.push(queued(&gate, false, 3000)).unwrap();
        assert_eq!(delivery.pop(&q, gate.status(), 20_000).unwrap().len, 0);
    }
    #[test]
    fn known_idle_entry_arms_the_first_fresh_touch_without_a_priming_gesture() {
        let gate = Gate::default();
        let q = ArrayQueue::new(8);
        let mut delivery = Delivery::default();
        gate.observe(false);
        gate.boundary();
        assert!(delivery.take_boundary(gate.status()));
        q.push(queued(&gate, true, 1000)).unwrap();
        assert_eq!(delivery.pop(&q, gate.status(), 9000).unwrap().len, 0);
        assert_eq!(delivery.pop(&q, gate.status(), 9000).unwrap().len, 1);
        assert!(delivery.pop(&q, gate.status(), 9000).is_none());
    }
    #[test]
    fn a_boundary_retires_pending_delay_and_unknown_capture_cannot_attest_idle() {
        let gate = Gate::default();
        let q = ArrayQueue::new(8);
        let mut delivery = Delivery::default();
        gate.boundary();
        assert!(delivery.take_boundary(gate.status()));
        q.push(queued(&gate, true, 1000)).unwrap();
        assert!(delivery.pop(&q, gate.status(), 1000).is_none());
        gate.boundary();
        assert!(
            delivery.pop(&q, gate.status(), 20000).is_none(),
            "new epoch cannot overtake its barrier"
        );
        assert!(delivery.take_boundary(gate.status()));
        assert!(!delivery.take_boundary(gate.status()));
        q.push(queued(&gate, true, 2000)).unwrap();
        assert_eq!(delivery.pop(&q, gate.status(), 10000).unwrap().time_us, 1000);
        assert!(delivery.pop(&q, gate.status(), 10000).is_none());
    }
}

#[cfg(test)]
mod boundary_race_tests {
    use super::*;
    #[test]
    fn an_inflight_old_callback_cannot_attest_idle_or_join_the_new_epoch() {
        let gate = Gate::default();
        gate.observe(false);
        let old = gate.begin();
        gate.boundary();
        let (_, current) = gate.finish(true);
        assert_ne!(old.token(), current.token());
        assert!(!current.idle_at_entry());
        let q = ArrayQueue::new(8);
        let mut delayed = TimedSample::new(Sample::default(), 1000);
        delayed.token = old.token();
        q.push(delayed).unwrap();
        let mut delivery = Delivery::default();
        assert!(delivery.take_boundary(current));
        assert!(delivery.pop(&q, current, 10000).is_none());
        assert!(q.is_empty());
    }
}
