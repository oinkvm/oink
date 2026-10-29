//! Source-clocked RTT history; no UI subscription owns measurement state.

use crate::api::{ConnectionQuality, LinkKind, PeerQuality, QualitySample};
use oink_foundation::DeviceId;
use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;
use std::time::Instant;

// Five minutes of source-clocked, one-second RTT samples.
const WINDOW_MS: u64 = 300_000;
const SLOTS: usize = 300;

pub(crate) fn now_ms_at(now: Instant) -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    now.saturating_duration_since(*ORIGIN.get_or_init(|| now)).as_secs() * 1000
}

#[derive(Debug, Default)]
pub(crate) struct History {
    peers: HashMap<DeviceId, VecDeque<QualitySample>>,
    metadata: HashMap<DeviceId, (String, LinkKind)>,
    sampled_at_ms: u64,
    revision: u64,
}
impl History {
    pub fn record(&mut self, at_ms: u64, peers: impl IntoIterator<Item = (DeviceId, String, LinkKind, Option<f64>)>) {
        let mut current = Vec::new();
        for (peer, name, path, rtt_ms) in peers {
            self.metadata.insert(peer, (name, path));
            current.push(peer);
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
                let stats = |window_ms| {
                    let mut values: Vec<_> = samples
                        .iter()
                        .filter(|s| at_ms.saturating_sub(s.at_ms) < window_ms)
                        .filter_map(|s| s.rtt_ms)
                        .collect();
                    values.sort_by(f64::total_cmp);
                    // Nearest rank over raw finite points, never bucket maxima.
                    let percentile = |p: usize| values.get((values.len() * p).div_ceil(100).saturating_sub(1)).copied();
                    crate::api::LatencyWindowStats {
                        p50_ms: percentile(50),
                        p90_ms: percentile(90),
                        p99_ms: percentile(99),
                        max_ms: values.last().copied(),
                    }
                };
                let minute = stats(60_000);
                PeerQuality {
                    stats_5m: stats(WINDOW_MS),
                    device: (*peer).into(),
                    name: self.metadata[peer].0.clone(),
                    path: self.metadata[peer].1.clone(),
                    current_ms: samples.last().and_then(|s| s.rtt_ms),
                    p50_ms: minute.p50_ms,
                    p90_ms: minute.p90_ms,
                    p99_ms: minute.p99_ms,
                    max_ms: minute.max_ms,
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
    fn nearest_rank_percentiles_cover_both_raw_rtt_windows() {
        let peer = DeviceId([2; 32]);
        let mut history = History::default();
        for second in 1..=100 {
            history.record(
                second * 1000,
                [(peer, "Peer".into(), crate::api::LinkKind::Direct, Some(second as f64))],
            );
        }
        let view = history.view(100_000);
        let row = &view.peers[0];
        assert_eq!(row.samples.len(), 100);
        assert_eq!(
            (row.stats_5m.p50_ms, row.stats_5m.p90_ms, row.stats_5m.p99_ms),
            (Some(50.), Some(90.), Some(99.))
        );
        assert_eq!(
            (row.current_ms, row.p50_ms, row.p90_ms, row.p99_ms, row.max_ms),
            (Some(100.), Some(70.), Some(94.), Some(100.), Some(100.))
        );
        assert!(history.view(400_000).peers[0].samples.is_empty());
    }

    #[test]
    fn missing_rtt_is_a_gap_not_zero_and_leaving_removes_peer_history() {
        let peer = DeviceId([2; 32]);
        let mut history = History::default();
        history.record(1000, [(peer, "Peer".into(), crate::api::LinkKind::Direct, Some(4.))]);
        history.record(2000, [(peer, "Peer".into(), crate::api::LinkKind::Direct, None)]);
        let view = history.view(2000);
        assert_eq!(view.peers[0].current_ms, None);
        assert_eq!(view.peers[0].p50_ms, Some(4.));
        history.record(3000, []);
        assert!(history.view(3000).peers.is_empty());
    }

    #[test]
    fn p90_excludes_gaps_and_invalid_rtt_and_preserves_raw_values() {
        let peer = DeviceId([2; 32]);
        let mut history = History::default();
        for (i, value) in [
            Some(f64::NAN),
            Some(-1.),
            Some(f64::INFINITY),
            None,
            Some(0.),
            Some(1.),
            Some(2.),
            Some(3.),
            Some(4.),
            Some(5.),
            Some(6.),
            Some(7.),
            Some(1500.),
            Some(2000.),
        ]
        .into_iter()
        .enumerate()
        {
            history.record(i as u64 * 1000, [(peer, "Peer".into(), LinkKind::Direct, value)]);
        }
        let view = history.view(13_000);
        let row = &view.peers[0];
        assert_eq!(row.samples.len(), 14);
        assert_eq!(row.samples.iter().filter(|s| s.rtt_ms.is_none()).count(), 4);
        assert_eq!(row.p90_ms, Some(1500.)); // Rank 9 of 10, without display clipping.
        let expired = history.view(73_000);
        assert_eq!(expired.peers[0].p90_ms, None);
        let ipc = serde_json::to_value(row).unwrap();
        assert_eq!(ipc["p90_ms"], 1500.);
        assert!(ipc.get("average_jitter_ms").is_none());
        assert!(ipc.get("max_jitter_ms").is_none());
    }

    #[test]
    fn history_is_independent_of_subscriptions_and_remains_bounded() {
        let peer = DeviceId([2; 32]);
        let mut history = History::default();
        history.record(1000, [(peer, "Peer".into(), crate::api::LinkKind::Direct, Some(1.))]);
        let opened = history.view(1000);
        drop(opened); // Closing a page only drops its snapshot.
        for second in 2..=600 {
            history.record(
                second * 1000,
                [(peer, "Peer".into(), crate::api::LinkKind::Direct, Some(2.))],
            );
        }
        assert_eq!(history.view(600_000).peers[0].samples.len(), 300);
        assert!(history.peers[&peer].len() <= SLOTS);
    }

    #[test]
    fn current_route_changes_without_resetting_latency_history() {
        let peer = DeviceId([2; 32]);
        let via = DeviceId([3; 32]);
        let mut history = History::default();
        history.record(1000, [(peer, "Peer".into(), LinkKind::Direct, Some(4.))]);
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
            )],
        );
        let view = history.view(2000);
        assert!(matches!(&view.peers[0].path, LinkKind::Relay { via_name, .. } if via_name == "Relay"));
        assert_eq!(view.peers[0].samples.len(), 2);
    }
}
