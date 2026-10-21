//! Local packet diagnostics and measured output quality in fixed one-second buckets.
//! `lost`/`late` retain legacy sequence accounting for diagnostics only; the UI
//! uses `interruptions`, rendered `concealed_ns`, and independently deduplicated
//! `late_packets`. Neither transport timing nor buffer adjustment creates an
//! interruption. One RT writer per source (or native output), ordinary readers.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

const SECONDS: usize = 60;
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DropoutBucket {
    pub at_ms: u64,
    pub lost: u64,
    pub late: u64,
    pub interruptions: u64,
    pub concealed_ns: u64,
    pub missing_ns: u64,
    pub late_packets: u64,
    pub silence_interruptions: u64,
    pub concealment_interruptions: u64,
    pub discontinuities: u64,
}
impl DropoutBucket {
    pub fn is_empty(&self) -> bool {
        self.count() == 0
            && self.interruptions == 0
            && self.concealed_ns == 0
            && self.missing_ns == 0
            && self.late_packets == 0
            && self.silence_interruptions == 0
            && self.concealment_interruptions == 0
            && self.discontinuities == 0
    }
    pub fn difference(&self, old: Self) -> Self {
        Self {
            at_ms: self.at_ms,
            lost: self.lost.saturating_sub(old.lost),
            late: self.late.saturating_sub(old.late),
            interruptions: self.interruptions.saturating_sub(old.interruptions),
            concealed_ns: self.concealed_ns.saturating_sub(old.concealed_ns),
            missing_ns: self.missing_ns.saturating_sub(old.missing_ns),
            late_packets: self.late_packets.saturating_sub(old.late_packets),
            silence_interruptions: self.silence_interruptions.saturating_sub(old.silence_interruptions),
            concealment_interruptions: self
                .concealment_interruptions
                .saturating_sub(old.concealment_interruptions),
            discontinuities: self.discontinuities.saturating_sub(old.discontinuities),
        }
    }
    pub fn add(&mut self, delta: Self) {
        self.lost += delta.lost;
        self.late += delta.late;
        self.interruptions += delta.interruptions;
        self.concealed_ns += delta.concealed_ns;
        self.missing_ns += delta.missing_ns;
        self.late_packets += delta.late_packets;
        self.silence_interruptions += delta.silence_interruptions;
        self.concealment_interruptions += delta.concealment_interruptions;
        self.discontinuities += delta.discontinuities;
    }

    pub fn count(&self) -> u64 {
        self.lost + self.late
    }
}
#[derive(Debug, Default)]
struct Slot {
    stamp: AtomicU64,
    lost: AtomicU64,
    late: AtomicU64,
    interruptions: AtomicU64,
    concealed_ns: AtomicU64,
    missing_ns: AtomicU64,
    late_packets: AtomicU64,
    silence_interruptions: AtomicU64,
    concealment_interruptions: AtomicU64,
    discontinuities: AtomicU64,
}
/// One receiver/render writer, ordinary-thread readers. Storage is fixed.
#[derive(Debug)]
pub struct DropoutCounters {
    slots: [Slot; SECONDS],
}
impl Default for DropoutCounters {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| Slot::default()),
        }
    }
}
impl DropoutCounters {
    pub(crate) fn record(&self, at_ms: u64, lost: u64, late: u64) {
        self.record_quality(DropoutBucket {
            at_ms,
            lost,
            late,
            ..Default::default()
        });
    }
    pub(crate) fn record_quality(&self, delta: DropoutBucket) {
        let DropoutBucket { at_ms, lost, late, .. } = delta;
        if delta.is_empty() {
            return;
        }
        let second = at_ms / 1000;
        let stamp = second + 1;
        let slot = &self.slots[second as usize % SECONDS];
        let old_stamp = slot.stamp.load(Ordering::SeqCst);
        if old_stamp != u64::MAX && old_stamp > stamp {
            return;
        }
        if old_stamp != stamp {
            slot.stamp.store(u64::MAX, Ordering::SeqCst);
            slot.lost.store(0, Ordering::SeqCst);
            slot.late.store(0, Ordering::SeqCst);
            slot.interruptions.store(0, Ordering::SeqCst);
            slot.concealed_ns.store(0, Ordering::SeqCst);
            slot.missing_ns.store(0, Ordering::SeqCst);
            slot.late_packets.store(0, Ordering::SeqCst);
            slot.silence_interruptions.store(0, Ordering::SeqCst);
            slot.concealment_interruptions.store(0, Ordering::SeqCst);
            slot.discontinuities.store(0, Ordering::SeqCst);

            slot.stamp.store(stamp, Ordering::SeqCst);
        }
        slot.lost.fetch_add(lost, Ordering::SeqCst);
        slot.late.fetch_add(late, Ordering::SeqCst);
        slot.interruptions.fetch_add(delta.interruptions, Ordering::SeqCst);
        slot.concealed_ns.fetch_add(delta.concealed_ns, Ordering::SeqCst);
        slot.missing_ns.fetch_add(delta.missing_ns, Ordering::SeqCst);
        slot.late_packets.fetch_add(delta.late_packets, Ordering::SeqCst);
        slot.silence_interruptions
            .fetch_add(delta.silence_interruptions, Ordering::SeqCst);
        slot.concealment_interruptions
            .fetch_add(delta.concealment_interruptions, Ordering::SeqCst);
        slot.discontinuities.fetch_add(delta.discontinuities, Ordering::SeqCst);
    }
    pub fn snapshot(&self, at_ms: u64) -> [Option<DropoutBucket>; SECONDS] {
        let second = at_ms / 1000;
        std::array::from_fn(|index| {
            let slot = &self.slots[index];
            let stamp = slot.stamp.load(Ordering::SeqCst);
            if stamp == 0 || stamp == u64::MAX {
                return None;
            }
            let bucket_second = stamp - 1;
            if bucket_second > second || second - bucket_second >= SECONDS as u64 {
                return None;
            }
            let bucket = DropoutBucket {
                at_ms: bucket_second * 1000,
                lost: slot.lost.load(Ordering::SeqCst),
                late: slot.late.load(Ordering::SeqCst),
                interruptions: slot.interruptions.load(Ordering::SeqCst),
                concealed_ns: slot.concealed_ns.load(Ordering::SeqCst),
                missing_ns: slot.missing_ns.load(Ordering::SeqCst),
                late_packets: slot.late_packets.load(Ordering::SeqCst),
                silence_interruptions: slot.silence_interruptions.load(Ordering::SeqCst),
                concealment_interruptions: slot.concealment_interruptions.load(Ordering::SeqCst),
                discontinuities: slot.discontinuities.load(Ordering::SeqCst),
            };
            (slot.stamp.load(Ordering::SeqCst) == stamp && !bucket.is_empty()).then_some(bucket)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn second_boundaries_expiry_and_wrap_keep_exact_counts() {
        let c = DropoutCounters::default();
        c.record(999, 2, 0);
        c.record(1000, 0, 1);
        c.record(1999, 1, 0);
        let at = c.snapshot(1999);
        assert_eq!(
            at[0],
            Some(DropoutBucket {
                at_ms: 0,
                lost: 2,
                late: 0,
                ..Default::default()
            })
        );
        assert_eq!(
            at[1],
            Some(DropoutBucket {
                at_ms: 1000,
                lost: 1,
                late: 1,
                ..Default::default()
            })
        );
        assert!(c.snapshot(59999)[0].is_some());
        assert!(c.snapshot(60000)[0].is_none());
        c.record(60000, 3, 4);
        assert_eq!(c.snapshot(60000)[0].unwrap().count(), 7);
        assert!(c.snapshot(61000)[1].is_none());
        assert!(c.snapshot(120000).iter().all(Option::is_none));
    }
    #[test]
    fn zero_events_do_not_create_markers_and_storage_has_sixty_slots() {
        let c = DropoutCounters::default();
        c.record(0, 0, 0);
        assert!(c.snapshot(0).iter().all(Option::is_none));
        for s in 0..120 {
            c.record(s * 1000, 1, 0);
        }
        assert_eq!(c.snapshot(119999).into_iter().flatten().count(), 60);
    }
}

#[derive(Clone, Copy, Debug)]
struct Range {
    first: u64,
    last: u64,
}
/// A compact, preallocated union of counted sequence ranges. 32768 disjoint
/// ranges exceed all packets in a 60 s window at the fastest supported 400 Hz.
/// Retired old ranges become a watermark: old replays cannot be counted again.
#[derive(Debug)]
pub(crate) struct Ledger {
    ranges: Vec<Range>,
    retired: Option<u64>,
}
impl Default for Ledger {
    fn default() -> Self {
        Self {
            ranges: {
                let mut storage = vec![Range { first: 0, last: 0 }; 32768];
                storage.clear();
                storage
            },
            retired: None,
        }
    }
}
impl Ledger {
    pub fn clear(&mut self) {
        self.ranges.clear();
        self.retired = None;
    }
    pub fn insert(&mut self, mut first: u64, last: u64) -> u64 {
        if let Some(retired) = self.retired {
            if last <= retired {
                return 0;
            }
            first = first.max(retired.saturating_add(1));
        }
        if first > last {
            return 0;
        }
        let start = self.ranges.partition_point(|r| r.last.saturating_add(1) < first);
        let mut end = start;
        let mut merged = Range { first, last };
        let mut fresh = last.saturating_sub(first).saturating_add(1);
        while let Some(range) = self
            .ranges
            .get(end)
            .filter(|r| r.first <= merged.last.saturating_add(1))
        {
            let lo = first.max(range.first);
            let hi = last.min(range.last);
            if lo <= hi {
                fresh = fresh.saturating_sub(hi - lo + 1);
            }
            merged.first = merged.first.min(range.first);
            merged.last = merged.last.max(range.last);
            end += 1;
        }
        if start == end {
            if self.ranges.len() == 32768 {
                self.retired = Some(self.ranges.remove(0).last);
                return self.insert(first, last);
            }
            self.ranges.insert(start, merged);
        } else {
            self.ranges[start] = merged;
            self.ranges.drain(start + 1..end);
        }
        fresh
    }
}
#[cfg(test)]
mod ledger_tests {
    use super::*;
    #[test]
    fn unique_sequences_merge_ranges_without_double_counting_lost_then_late() {
        let mut l = Ledger::default();
        assert_eq!(l.insert(2, 2), 1);
        assert_eq!(l.insert(2, 2), 0);
        assert_eq!(l.insert(4, 8), 5);
        assert_eq!(l.insert(1, 6), 2);
        assert_eq!(l.insert(0, 10), 3);
        assert_eq!(l.insert(0, 10), 0);
        l.clear();
        assert_eq!(l.insert(2, 2), 1);
    }
    #[test]
    fn ledger_is_allocation_free_and_old_replays_do_not_return_after_capacity() {
        let mut l = Ledger::default();
        crate::allocation_audit::assert_no_heap(|| {
            for seq in 0..32770 {
                assert_eq!(l.insert(seq * 2, seq * 2), 1);
            }
            assert_eq!(l.insert(0, 0), 0);
            assert_eq!(l.insert(3, 65537), 32768);
        });
    }
}
