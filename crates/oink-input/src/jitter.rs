//! Delay variation without clock synchronisation. The input owner keeps one
//! estimator per source and transport; the ordinary clock drains numeric atomics.
use crate::source_time::CaptureTime;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const SECONDS: usize = 30;
const SECOND_US: u64 = 1_000_000;
const BINS: usize = 1026; // zero, ceil-ms through 1024ms, and overflow.
pub const LATE_US: [u64; 5] = [4_000, 8_000, 16_000, 32_000, 64_000];
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Distribution {
    pub count: u64,
    pub min_us: u64,
    /// Histogram upper bounds, with 1ms resolution through 1024ms. Overflow
    /// uses the window maximum, never silently clips a long stall to 1024ms.
    pub median_upper_us: u64,
    pub p95_upper_us: u64,
    pub max_us: u64,
    pub late: [u64; 5],
    pub percentile_overflow: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathSample {
    /// Source capture to network ingress, relative to the best recent transit.
    pub ingress: Distribution,
    /// Source capture to completed input-owner dispatch, relative to its own baseline.
    pub dispatch: Distribution,
    pub baseline_resets: u64,
    /// Capture timestamps arriving behind the latest source timestamp; this can
    /// also reflect multiple native producers on ordered I0, not QUIC reordering.
    pub source_reordered: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    pub reliable: PathSample,
    pub lossy: PathSample,
}
#[derive(Debug)]
struct Histogram {
    counts: [AtomicU64; BINS],
    min: AtomicU64,
    max: AtomicU64,
}
impl Default for Histogram {
    fn default() -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            min: AtomicU64::new(u64::MAX),
            max: AtomicU64::new(0),
        }
    }
}
impl Histogram {
    fn record(&self, us: u64) {
        self.min.fetch_min(us, Ordering::Relaxed);
        self.max.fetch_max(us, Ordering::Relaxed);
        self.counts[us.div_ceil(1000).min(BINS as u64 - 1) as usize].fetch_add(1, Ordering::Relaxed);
    }
    fn take(&self) -> Distribution {
        let min = self.min.swap(u64::MAX, Ordering::Relaxed);
        let max = self.max.swap(0, Ordering::Relaxed);
        let bins: [u64; BINS] = std::array::from_fn(|i| self.counts[i].swap(0, Ordering::Relaxed));
        let count: u64 = bins.iter().sum();
        if count == 0 {
            return Distribution::default();
        }
        let mut overflow = false;
        let mut percentile = |percent: u64| {
            let rank = (count * percent).div_ceil(100);
            let mut seen = 0;
            for (i, n) in bins.iter().enumerate() {
                seen += n;
                if seen >= rank {
                    if i == BINS - 1 {
                        overflow = true;
                        return max;
                    }
                    return i as u64 * 1000;
                }
            }
            max
        };
        let median_upper_us = percentile(50);
        let p95_upper_us = percentile(95);
        Distribution {
            count,
            min_us: if min == u64::MAX { 0 } else { min },
            median_upper_us,
            p95_upper_us,
            max_us: max,
            late: LATE_US.map(|threshold| bins[(threshold / 1000 + 1) as usize..].iter().sum()),
            percentile_overflow: overflow,
        }
    }
}
#[derive(Debug, Default)]
struct PathCounters {
    ingress: Histogram,
    dispatch: Histogram,
    resets: AtomicU64,
    reordered: AtomicU64,
}
/// One source-owned writer, one ordinary reader; no lock or per-event records.
/// The ordinary reader rotates buffers and waits for the finite atomic write to
/// finish. The realtime writer never waits for the ordinary reader, even if it
/// is stopped while draining. Counts and extrema refer to the same window.
#[derive(Debug, Default)]
pub struct Counters {
    buffers: [[PathCounters; 2]; 2],
    /// Bit 0 = active buffer; bit 1 = the sole writer is publishing a sample.
    state: AtomicUsize,
}
impl Counters {
    pub fn record(&self, lossy: bool, ingress: Observation, dispatch: Observation) {
        let before = self.state.fetch_or(2, Ordering::AcqRel);
        assert_eq!(before & 2, 0, "jitter counters require one input owner");
        let p = &self.buffers[before & 1][usize::from(lossy)];
        p.ingress.record(ingress.delay_us);
        p.dispatch.record(dispatch.delay_us);
        p.resets.fetch_add(u64::from(ingress.reset), Ordering::Relaxed);
        p.reordered.fetch_add(u64::from(ingress.reordered), Ordering::Relaxed);
        self.state.fetch_and(!2, Ordering::Release);
    }
    pub fn take(&self) -> Sample {
        let old = self.state.fetch_xor(1, Ordering::AcqRel) & 1;
        // Ordinary work only. No system call, allocation, clock read, or routing
        // occurs inside the writer's short atomic publication section.
        while self.state.load(Ordering::Acquire) & 2 != 0 {
            std::thread::yield_now();
        }
        let take = |p: &PathCounters| PathSample {
            ingress: p.ingress.take(),
            dispatch: p.dispatch.take(),
            baseline_resets: p.resets.swap(0, Ordering::Relaxed),
            source_reordered: p.reordered.swap(0, Ordering::Relaxed),
        };
        Sample {
            reliable: take(&self.buffers[old][0]),
            lossy: take(&self.buffers[old][1]),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    pub delay_us: u64,
    pub reset: bool,
    pub reordered: bool,
}
#[derive(Clone, Copy, Debug)]
struct Minimum {
    second: u64,
    transit: i64,
}
#[derive(Debug)]
pub struct Estimator {
    source: Option<CaptureTime>,
    unwrapped: i64,
    origin_us: u64,
    last_us: u64,
    minima: [Option<Minimum>; SECONDS],
}
impl Default for Estimator {
    fn default() -> Self {
        Self {
            source: None,
            unwrapped: 0,
            origin_us: 0,
            last_us: 0,
            minima: [None; SECONDS],
        }
    }
}
impl Estimator {
    pub fn observe(&mut self, source: CaptureTime, local_us: u64) -> Observation {
        let reset = self.source.is_none() || local_us.saturating_sub(self.last_us) >= SECONDS as u64 * SECOND_US;
        if reset {
            *self = Self::default();
            self.origin_us = local_us;
            self.source = Some(source);
        }
        let delta = source.0.wrapping_sub(self.source.unwrap().0) as i32;
        let source_us = self.unwrapped + i64::from(delta);
        let reordered = delta < 0;
        // I0 delivery is ordered, but its native sources need not be: a pending
        // contact sample can follow a newer CG pointer event. Keep its true late
        // transit instead of resetting the baseline and hiding that delay.
        if !reordered {
            self.source = Some(source);
            self.unwrapped = source_us;
        }
        self.last_us = self.last_us.max(local_us);
        let transit = local_us.saturating_sub(self.origin_us) as i64 - source_us;
        let second = local_us / SECOND_US;
        let slot = &mut self.minima[second as usize % SECONDS];
        if slot.is_none_or(|old| old.second != second || transit < old.transit) {
            *slot = Some(Minimum { second, transit });
        }
        let baseline = self
            .minima
            .iter()
            .flatten()
            .filter(|m| second.saturating_sub(m.second) < SECONDS as u64)
            .map(|m| m.transit)
            .min()
            .unwrap_or(transit);
        Observation {
            delay_us: transit.saturating_sub(baseline).max(0) as u64,
            reset,
            reordered,
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn concurrent_window_rotation_preserves_counts_and_matching_extrema() {
        let counters = std::sync::Arc::new(Counters::default());
        let producer = counters.clone();
        let writer = std::thread::spawn(move || {
            for us in 1..=10_000 {
                let o = Observation {
                    delay_us: us,
                    reset: false,
                    reordered: false,
                };
                producer.record(false, o, o);
            }
        });
        let mut count = 0;
        let mut min = u64::MAX;
        let mut max = 0;
        let mut collect = || {
            let sample = counters.take().reliable.ingress;
            count += sample.count;
            if sample.count != 0 {
                assert!(sample.min_us <= sample.max_us);
                min = min.min(sample.min_us);
                max = max.max(sample.max_us);
            }
        };
        while !writer.is_finished() {
            collect();
        }
        writer.join().unwrap();
        collect();
        assert_eq!((count, min, max), (10_000, 1, 10_000));
    }
    #[test]
    fn controlled_transit_and_dispatch_stalls_are_measured_separately() {
        let mut ingress = Estimator::default();
        let mut dispatch = Estimator::default();
        let counters = Counters::default();
        // All numbers are virtual clock readings, no scheduler/time-luck test.
        // A 1ms transit baseline becomes 9ms; a 1ms dispatch baseline then
        // waits an additional 13ms. FIFO dispatch keeps the following event late.
        for (capture, arrival, routed) in [
            (0, 501_000, 502_000),
            (8_000, 509_000, 510_000),
            (16_000, 525_000, 539_000),
            (24_000, 525_000, 539_000),
        ] {
            let time = CaptureTime(capture);
            counters.record(false, ingress.observe(time, arrival), dispatch.observe(time, routed));
        }
        let sample = counters.take().reliable;
        assert_eq!(sample.ingress.max_us, 8000);
        assert_eq!(sample.dispatch.max_us, 21000);
        assert_eq!(sample.ingress.late, [1, 0, 0, 0, 0]);
        assert_eq!(sample.dispatch.late, [2, 2, 1, 0, 0]);
        assert_eq!(sample.ingress.count, 4);
    }
    #[test]
    fn offsets_wrap_loss_reorder_and_local_idle_do_not_break_intervals() {
        let mut e = Estimator::default();
        let start = u32::MAX - 4000;
        assert!(e.observe(CaptureTime(start), 90_000_000).reset);
        assert_eq!(e.observe(CaptureTime(start.wrapping_add(8000)), 90_008_000).delay_us, 0);
        assert_eq!(
            e.observe(CaptureTime(start.wrapping_add(24000)), 90_032_000).delay_us,
            8000
        );
        let old = e.observe(CaptureTime(start.wrapping_add(16000)), 90_033_000);
        assert!(old.reordered);
        assert_eq!(old.delay_us, 17000);
        assert_eq!(
            e.observe(CaptureTime(start.wrapping_add(40000)), 90_040_000).delay_us,
            0
        );
        assert!(e.observe(CaptureTime(2), 4_000_000_000).reset);
    }
    #[test]
    fn recent_baseline_expires_but_older_capture_on_i0_stays_visible() {
        let mut e = Estimator::default();
        e.observe(CaptureTime(0), 0);
        assert_eq!(e.observe(CaptureTime(1_000_000), 1_005_000).delay_us, 5000);
        for second in 2..=31 {
            let o = e.observe(CaptureTime(second * 1_000_000), u64::from(second) * SECOND_US + 5000);
            if second == 31 {
                assert_eq!(o.delay_us, 0);
            }
        }
        let older = e.observe(CaptureTime(30_000_000), 32_000_000);
        assert!(!older.reset);
        assert!(older.reordered);
        assert_eq!(older.delay_us, 1_995_000);
    }
    #[test]
    fn summaries_have_exact_extrema_bins_bounded_percentiles_and_drain() {
        let c = Counters::default();
        for us in [0, 100, 4000, 4001, 8001, 17000, 33000, 65000, 2_000_000] {
            let o = Observation {
                delay_us: us,
                reset: us == 0,
                reordered: us == 17000,
            };
            c.record(
                false,
                o,
                Observation {
                    delay_us: us + 8000,
                    ..o
                },
            );
        }
        let s = c.take();
        assert_eq!(s.reliable.ingress.count, 9);
        assert_eq!(s.reliable.ingress.min_us, 0);
        assert_eq!(s.reliable.ingress.max_us, 2_000_000);
        assert_eq!(s.reliable.ingress.median_upper_us, 9000);
        assert_eq!(s.reliable.ingress.p95_upper_us, 2_000_000);
        assert!(s.reliable.ingress.percentile_overflow);
        assert_eq!(s.reliable.ingress.late, [6, 5, 4, 3, 2]);
        assert_eq!(s.reliable.dispatch.min_us, 8000);
        assert_eq!(s.reliable.baseline_resets, 1);
        assert_eq!(s.reliable.source_reordered, 1);
        assert_eq!(c.take(), Sample::default());
    }
}
