//! Local packet diagnostics and measured output quality in fixed one-second buckets.
//! `lost`/`late` retain legacy sequence accounting for diagnostics only; the UI
//! uses `interruptions`, rendered `concealed_ns`, and independently deduplicated
//! `late_packets`. Neither transport timing nor buffer adjustment creates an
//! interruption. One RT writer per source (or native output), ordinary readers.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Five minutes of fixed one-second diagnostic buckets.
pub const QUALITY_SECONDS: usize = 300;
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
    /// Shared output interruptions affecting this source; also included in interruptions.
    #[serde(default)]
    pub output_interruptions: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub callbacks: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub callback_over_budget: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub native_timing_callbacks: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub native_gap_frames: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub callback_max_work_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_min_margin_us: Option<i64>,
}
fn is_zero(value: &u64) -> bool {
    *value == 0
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
            && self.output_interruptions == 0
            && self.callbacks == 0
            && self.callback_over_budget == 0
            && self.native_timing_callbacks == 0
            && self.native_gap_frames == 0
            && self.callback_max_work_us == 0
            && self.callback_min_margin_us.is_none()
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
            output_interruptions: self.output_interruptions.saturating_sub(old.output_interruptions),
            callbacks: self.callbacks.saturating_sub(old.callbacks),
            callback_over_budget: self.callback_over_budget.saturating_sub(old.callback_over_budget),
            native_timing_callbacks: self.native_timing_callbacks.saturating_sub(old.native_timing_callbacks),
            native_gap_frames: self.native_gap_frames.saturating_sub(old.native_gap_frames),
            callback_max_work_us: if self.callback_max_work_us > old.callback_max_work_us {
                self.callback_max_work_us
            } else {
                0
            },
            callback_min_margin_us: self
                .callback_min_margin_us
                .filter(|value| old.callback_min_margin_us.is_none_or(|old| *value < old)),
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
        self.output_interruptions += delta.output_interruptions;
        self.callbacks += delta.callbacks;
        self.callback_over_budget += delta.callback_over_budget;
        self.native_timing_callbacks += delta.native_timing_callbacks;
        self.native_gap_frames += delta.native_gap_frames;
        self.callback_max_work_us = self.callback_max_work_us.max(delta.callback_max_work_us);
        if let Some(value) = delta.callback_min_margin_us {
            self.callback_min_margin_us = Some(self.callback_min_margin_us.map_or(value, |old| old.min(value)));
        }
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
    output_interruptions: AtomicU64,
    callbacks: AtomicU64,
    callback_over_budget: AtomicU64,
    native_timing_callbacks: AtomicU64,
    native_gap_frames: AtomicU64,
    callback_max_work_us: AtomicU64,
    callback_min_margin_us: AtomicI64,
}
/// One receiver/render writer, ordinary-thread readers. Storage is fixed.
#[derive(Debug)]
pub struct DropoutCounters {
    slots: [Slot; QUALITY_SECONDS],
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
        let slot = &self.slots[second as usize % QUALITY_SECONDS];
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
            slot.output_interruptions.store(0, Ordering::SeqCst);
            slot.callbacks.store(0, Ordering::SeqCst);
            slot.callback_over_budget.store(0, Ordering::SeqCst);
            slot.native_timing_callbacks.store(0, Ordering::SeqCst);
            slot.native_gap_frames.store(0, Ordering::SeqCst);
            slot.callback_max_work_us.store(0, Ordering::SeqCst);
            slot.callback_min_margin_us.store(i64::MAX, Ordering::SeqCst);

            slot.stamp.store(stamp, Ordering::SeqCst);
        }
        slot.lost.fetch_add(lost, Ordering::SeqCst);
        slot.late.fetch_add(late, Ordering::SeqCst);
        if delta.interruptions != 0 {
            slot.interruptions.fetch_add(delta.interruptions, Ordering::SeqCst);
        }
        if delta.concealed_ns != 0 {
            slot.concealed_ns.fetch_add(delta.concealed_ns, Ordering::SeqCst);
        }
        if delta.missing_ns != 0 {
            slot.missing_ns.fetch_add(delta.missing_ns, Ordering::SeqCst);
        }
        if delta.late_packets != 0 {
            slot.late_packets.fetch_add(delta.late_packets, Ordering::SeqCst);
        }
        if delta.silence_interruptions != 0 {
            slot.silence_interruptions
                .fetch_add(delta.silence_interruptions, Ordering::SeqCst);
        }
        if delta.concealment_interruptions != 0 {
            slot.concealment_interruptions
                .fetch_add(delta.concealment_interruptions, Ordering::SeqCst);
        }
        if delta.discontinuities != 0 {
            slot.discontinuities.fetch_add(delta.discontinuities, Ordering::SeqCst);
        }
        if delta.output_interruptions != 0 {
            slot.output_interruptions
                .fetch_add(delta.output_interruptions, Ordering::SeqCst);
        }

        if delta.callbacks != 0 {
            slot.callbacks.fetch_add(delta.callbacks, Ordering::SeqCst);
        }
        if delta.callback_over_budget != 0 {
            slot.callback_over_budget
                .fetch_add(delta.callback_over_budget, Ordering::SeqCst);
        }
        if delta.native_timing_callbacks != 0 {
            slot.native_timing_callbacks
                .fetch_add(delta.native_timing_callbacks, Ordering::SeqCst);
        }
        if delta.native_gap_frames != 0 {
            slot.native_gap_frames
                .fetch_add(delta.native_gap_frames, Ordering::SeqCst);
        }
        if delta.callback_max_work_us != 0 {
            slot.callback_max_work_us
                .fetch_max(delta.callback_max_work_us, Ordering::SeqCst);
        }
        if let Some(value) = delta.callback_min_margin_us {
            slot.callback_min_margin_us.fetch_min(value, Ordering::SeqCst);
        }
    }

    pub fn snapshot(&self, at_ms: u64) -> [Option<DropoutBucket>; QUALITY_SECONDS] {
        let second = at_ms / 1000;
        std::array::from_fn(|index| {
            let slot = &self.slots[index];
            let stamp = slot.stamp.load(Ordering::SeqCst);
            if stamp == 0 || stamp == u64::MAX {
                return None;
            }
            let bucket_second = stamp - 1;
            if bucket_second > second || second - bucket_second >= QUALITY_SECONDS as u64 {
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
                output_interruptions: slot.output_interruptions.load(Ordering::SeqCst),
                callbacks: slot.callbacks.load(Ordering::SeqCst),
                callback_over_budget: slot.callback_over_budget.load(Ordering::SeqCst),
                native_timing_callbacks: slot.native_timing_callbacks.load(Ordering::SeqCst),
                native_gap_frames: slot.native_gap_frames.load(Ordering::SeqCst),
                callback_max_work_us: slot.callback_max_work_us.load(Ordering::SeqCst),
                callback_min_margin_us: match slot.callback_min_margin_us.load(Ordering::SeqCst) {
                    i64::MAX => None,
                    value => Some(value),
                },
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
        assert!(c.snapshot(299999)[0].is_some());
        assert!(c.snapshot(300000)[0].is_none());
        c.record(300000, 3, 4);
        assert_eq!(c.snapshot(300000)[0].unwrap().count(), 7);
        assert!(c.snapshot(301000)[1].is_none());
        assert!(c.snapshot(600000).iter().all(Option::is_none));
    }
    #[test]
    fn zero_events_do_not_create_markers_and_storage_has_three_hundred_slots() {
        let c = DropoutCounters::default();
        c.record(0, 0, 0);
        assert!(c.snapshot(0).iter().all(Option::is_none));
        for s in 0..600 {
            c.record(s * 1000, 1, 0);
        }
        assert_eq!(c.snapshot(599999).into_iter().flatten().count(), 300);
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
