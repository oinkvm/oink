//! Source-clocked connection history; no UI subscription owns measurement state.
//! RTT reuses end-to-end link heartbeat measurements. Jitter reuses the input
//! estimator's capture-to-dispatch excess delay above its 30-second minimum;
//! only aggregation changes here: event-weighted sum/count/max over the same
//! 60 one-second bins as the RTT chart, across reliable and lossy inputs.

use crate::api::{ConnectionQuality, LinkKind, PeerQuality, QualitySample};
use oink_foundation::DeviceId;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering, fence};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

// RTT points and jitter moments occupy the same 60 one-second bins.
const WINDOW_MS: u64 = 60_000;
const SLOTS: usize = 61;

pub(crate) fn now_ms_at(now: Instant) -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    now.saturating_duration_since(*ORIGIN.get_or_init(|| now)).as_secs() * 1000
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Moments {
    pub count: u64,
    pub sum_us: u64,
    pub max_us: u64,
}
#[derive(Debug, Default)]
struct Bucket {
    sequence: AtomicU64,
    second: AtomicU64,
    count: AtomicU64,
    sum: AtomicU64,
    max: AtomicU64,
}
/// One input-owner writer. Readers retry a finite atomic publication; the
/// realtime writer never locks, waits, allocates or reads a clock here.
#[derive(Debug)]
pub(crate) struct JitterCounters {
    buckets: [Bucket; SLOTS],
}
impl Default for JitterCounters {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| Bucket::default()),
        }
    }
}
impl JitterCounters {
    pub fn record(&self, at_ms: u64, delay_us: u64) {
        let second = at_ms / 1000;
        let b = &self.buckets[second as usize % SLOTS];
        b.sequence.fetch_add(1, Ordering::AcqRel);
        if b.second.load(Ordering::Relaxed) != second {
            b.second.store(second, Ordering::Relaxed);
            b.count.store(0, Ordering::Relaxed);
            b.sum.store(0, Ordering::Relaxed);
            b.max.store(0, Ordering::Relaxed);
        }
        b.count.store(b.count.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        b.sum.store(
            b.sum.load(Ordering::Relaxed).saturating_add(delay_us),
            Ordering::Relaxed,
        );
        b.max
            .store(b.max.load(Ordering::Relaxed).max(delay_us), Ordering::Relaxed);
        b.sequence.fetch_add(1, Ordering::Release);
    }
    pub fn window(&self, at_ms: u64) -> Moments {
        let now = at_ms / 1000;
        let mut result = Moments::default();
        for b in &self.buckets {
            let (second, value) = loop {
                let before = b.sequence.load(Ordering::Acquire);
                if before % 2 != 0 {
                    std::hint::spin_loop();
                    continue;
                }
                let second = b.second.load(Ordering::Relaxed);
                let value = Moments {
                    count: b.count.load(Ordering::Relaxed),
                    sum_us: b.sum.load(Ordering::Relaxed),
                    max_us: b.max.load(Ordering::Relaxed),
                };
                fence(Ordering::Acquire);
                if before == b.sequence.load(Ordering::Relaxed) {
                    break (second, value);
                }
            };
            if second <= now && now - second < 60 {
                result.count += value.count;
                result.sum_us = result.sum_us.saturating_add(value.sum_us);
                result.max_us = result.max_us.max(value.max_us);
            }
        }
        result
    }
}
#[derive(Debug, Default)]
pub(crate) struct History {
    peers: HashMap<DeviceId, VecDeque<QualitySample>>,
    jitter: HashMap<DeviceId, Arc<JitterCounters>>,
    metadata: HashMap<DeviceId, (String, LinkKind)>,
    sampled_at_ms: u64,
    revision: u64,
}
impl History {
    pub fn record(
        &mut self,
        at_ms: u64,
        peers: impl IntoIterator<Item = (DeviceId, String, LinkKind, Option<f64>, Option<Arc<JitterCounters>>)>,
    ) {
        let mut current = Vec::new();
        for (peer, name, path, rtt_ms, jitter) in peers {
            self.metadata.insert(peer, (name, path));
            current.push(peer);
            match jitter {
                Some(counters) => {
                    self.jitter.insert(peer, counters);
                }
                None => {
                    self.jitter.remove(&peer);
                }
            }
            let samples = self.peers.entry(peer).or_default();
            samples.retain(|s| at_ms.saturating_sub(s.at_ms) < WINDOW_MS);
            if samples.len() >= SLOTS {
                samples.pop_front();
            }
            samples.push_back(QualitySample {
                at_ms,
                rtt_ms: rtt_ms.filter(|v| v.is_finite() && *v >= 0.),
            });
        }
        self.peers.retain(|peer, _| current.contains(peer));
        self.jitter.retain(|peer, _| current.contains(peer));
        self.metadata.retain(|peer, _| current.contains(peer));
        self.sampled_at_ms = at_ms;
        self.revision += 1;
    }
    pub fn view(&self, at_ms: u64) -> ConnectionQuality {
        let mut peers: Vec<_> = self
            .peers
            .iter()
            .map(|(peer, history)| {
                let samples: Vec<_> = history
                    .iter()
                    .filter(|s| at_ms.saturating_sub(s.at_ms) < WINDOW_MS)
                    .cloned()
                    .collect();
                let mut values: Vec<_> = samples.iter().filter_map(|s| s.rtt_ms).collect();
                values.sort_by(f64::total_cmp);
                // Nearest rank: ceil(n*p/100), shared with the input histogram.
                let percentile = |p: usize| values.get((values.len() * p).div_ceil(100).saturating_sub(1)).copied();
                let jitter = self.jitter.get(peer).map(|c| c.window(at_ms)).unwrap_or_default();
                PeerQuality {
                    device: (*peer).into(),
                    name: self.metadata[peer].0.clone(),
                    path: self.metadata[peer].1.clone(),
                    current_ms: samples.last().and_then(|s| s.rtt_ms),
                    p50_ms: percentile(50),
                    p90_ms: percentile(90),
                    p99_ms: percentile(99),
                    max_ms: values.last().copied(),
                    average_jitter_ms: (jitter.count > 0).then(|| jitter.sum_us as f64 / jitter.count as f64 / 1000.),
                    max_jitter_ms: (jitter.count > 0).then_some(jitter.max_us as f64 / 1000.),
                    samples,
                }
            })
            .collect();
        peers.sort_by_key(|a| a.device.0);
        ConnectionQuality {
            revision: self.revision,
            sampled_at_ms: at_ms.max(self.sampled_at_ms),
            peers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::DeviceId;

    #[test]
    fn nearest_rank_percentiles_and_event_weighted_jitter_share_sixty_seconds() {
        let peer = DeviceId([2; 32]);
        let mut history = History::default();
        let jitter = Arc::new(JitterCounters::default());
        for second in 1..=100 {
            jitter.record(second * 1000, 2000);
            jitter.record(second * 1000, 4000);
            history.record(
                second * 1000,
                [(
                    peer,
                    "Peer".into(),
                    crate::api::LinkKind::Direct,
                    Some(second as f64),
                    Some(jitter.clone()),
                )],
            );
        }
        let view = history.view(100_000);
        let row = &view.peers[0];
        assert_eq!(row.samples.len(), 60);
        assert_eq!(
            (row.current_ms, row.p50_ms, row.p90_ms, row.p99_ms, row.max_ms),
            (Some(100.), Some(70.), Some(94.), Some(100.), Some(100.))
        );
        assert_eq!(row.average_jitter_ms, Some(3.));
        assert_eq!(row.max_jitter_ms, Some(4.));
        assert!(history.view(160_000).peers[0].samples.is_empty());
        assert_eq!(history.view(160_000).peers[0].average_jitter_ms, None);
    }

    #[test]
    fn missing_rtt_is_a_gap_not_zero_and_leaving_removes_peer_history() {
        let peer = DeviceId([2; 32]);
        let mut history = History::default();
        history.record(
            1000,
            [(peer, "Peer".into(), crate::api::LinkKind::Direct, Some(4.), None)],
        );
        history.record(2000, [(peer, "Peer".into(), crate::api::LinkKind::Direct, None, None)]);
        let view = history.view(2000);
        assert_eq!(view.peers[0].current_ms, None);
        assert_eq!(view.peers[0].p50_ms, Some(4.));
        assert_eq!(view.peers[0].average_jitter_ms, None);
        history.record(3000, []);
        assert!(history.view(3000).peers.is_empty());
    }

    #[test]
    fn history_is_independent_of_subscriptions_and_remains_bounded() {
        let peer = DeviceId([2; 32]);
        let mut history = History::default();
        history.record(
            1000,
            [(peer, "Peer".into(), crate::api::LinkKind::Direct, Some(1.), None)],
        );
        let opened = history.view(1000);
        drop(opened); // Closing a page only drops its snapshot.
        for second in 2..=120 {
            history.record(
                second * 1000,
                [(peer, "Peer".into(), crate::api::LinkKind::Direct, Some(2.), None)],
            );
        }
        assert_eq!(history.view(120_000).peers[0].samples.len(), 60);
        assert!(history.peers[&peer].len() <= 61);
    }

    #[test]
    fn moment_rotation_keeps_exact_event_weighted_sum_and_max() {
        let c = JitterCounters::default();
        c.record(1000, 1000);
        c.record(1000, 5000);
        c.record(2000, 3000);
        assert_eq!(
            c.window(2000),
            Moments {
                count: 3,
                sum_us: 9000,
                max_us: 5000
            }
        );
        assert_eq!(c.window(62000), Moments::default());
    }
    #[test]
    fn jitter_expires_by_event_second_and_weights_each_input_not_each_bucket() {
        let c = JitterCounters::default();
        c.record(0, 90_000);
        c.record(1000, 1000);
        c.record(2000, 3000);
        c.record(2000, 5000);
        assert_eq!(
            c.window(60_000),
            Moments {
                count: 3,
                sum_us: 9000,
                max_us: 5000
            }
        );
        assert_eq!(
            c.window(61_000),
            Moments {
                count: 2,
                sum_us: 8000,
                max_us: 5000
            }
        );
        assert_eq!(c.window(62_000), Moments::default());
    }
    #[test]
    fn concurrent_readers_see_matching_jitter_count_sum_and_max() {
        let c = Arc::new(JitterCounters::default());
        let producer = c.clone();
        let writer = std::thread::spawn(move || {
            for _ in 0..10_000 {
                producer.record(1000, 1);
            }
        });
        while !writer.is_finished() {
            let m = c.window(1000);
            assert_eq!(m.count, m.sum_us);
            assert_eq!(m.max_us, u64::from(m.count > 0));
        }
        writer.join().unwrap();
        assert_eq!(c.window(1000).count, 10_000);
    }
    #[test]
    fn current_route_changes_without_resetting_latency_history() {
        let peer = DeviceId([2; 32]);
        let via = DeviceId([3; 32]);
        let mut history = History::default();
        history.record(1000, [(peer, "Peer".into(), LinkKind::Direct, Some(4.), None)]);
        history.record(
            2000,
            [(
                peer,
                "Peer".into(),
                LinkKind::Relay {
                    via: via.into(),
                    via_name: "Relay".into(),
                },
                Some(6.),
                None,
            )],
        );
        let view = history.view(2000);
        assert!(matches!(&view.peers[0].path, LinkKind::Relay { via_name, .. } if via_name == "Relay"));
        assert_eq!(view.peers[0].samples.len(), 2);
    }
}
