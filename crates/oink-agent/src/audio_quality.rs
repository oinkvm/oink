//! Local receive-only rolling diagnostics. Never replicated or persisted.
use crate::api::{AudioQuality, AudioQualitySample, AudioSourceQuality};
use oink_foundation::DeviceId;
use std::collections::{HashMap, HashSet, VecDeque};

const WINDOW_MS: u64 = 60_000;
const SLOTS: usize = 240;
#[derive(Clone, Copy)]
pub(crate) struct Observation {
    pub device: DeviceId,
    pub buffer_ms: Option<f64>,
    pub bytes: u64,
    pub glitches: [Option<oink_audio::sink::DropoutBucket>; 60],
    pub session: u64,
}
#[derive(Default)]
pub(crate) struct History {
    sources: HashMap<DeviceId, Source>,
    revision: u64,
    output: Vec<oink_audio::sink::DropoutBucket>,
}
#[derive(Default)]
struct Source {
    name: String,
    samples: VecDeque<(AudioQualitySample, u64)>,
    glitches: VecDeque<oink_audio::sink::DropoutBucket>,
    previous: Option<(u64, Observation)>,
}
impl History {
    pub fn record_output(&mut self, buckets: [Option<oink_audio::sink::DropoutBucket>; 60]) {
        self.output = buckets.into_iter().flatten().collect();
    }
    pub fn record(&mut self, at_ms: u64, observations: impl IntoIterator<Item = (String, Observation)>) {
        self.update(at_ms, observations, true);
    }
    pub fn flush(&mut self, at_ms: u64, observations: impl IntoIterator<Item = (String, Observation)>) {
        self.update(at_ms, observations, false);
    }
    fn update(&mut self, at_ms: u64, observations: impl IntoIterator<Item = (String, Observation)>, full: bool) {
        let at_ms = at_ms / 250 * 250;
        let mut current = HashSet::new();
        for (name, observation) in observations {
            current.insert(observation.device);
            let source = self.sources.entry(observation.device).or_default();
            source.name = name;
            let bytes = source.previous.map_or(observation.bytes, |(_, old)| {
                if old.session == observation.session {
                    observation.bytes.saturating_sub(old.bytes)
                } else {
                    observation.bytes
                }
            });
            for bucket in observation.glitches.into_iter().flatten() {
                if at_ms / 1000 < bucket.at_ms / 1000 || at_ms / 1000 - bucket.at_ms / 1000 >= 60 {
                    continue;
                }
                let old = source
                    .previous
                    .filter(|(_, old)| old.session == observation.session)
                    .and_then(|(_, old)| old.glitches.into_iter().flatten().find(|b| b.at_ms == bucket.at_ms))
                    .unwrap_or_default();
                let delta = bucket.difference(old);
                if delta.is_empty() {
                    continue;
                }
                if let Some(existing) = source.glitches.iter_mut().find(|b| b.at_ms == bucket.at_ms) {
                    existing.add(delta);
                } else {
                    source.glitches.push_back(delta);
                }
            }
            source.glitches.make_contiguous().sort_by_key(|b| b.at_ms);
            while source.glitches.len() > 60 {
                source.glitches.pop_front();
            }
            source.previous = Some((at_ms, observation));
            // Silent admitted sessions keep a baseline, but do not manufacture
            // zero-buffer observations or keep a finished line alive forever.
            if observation.buffer_ms.is_none() && bytes == 0 {
                continue;
            }
            if let Some((sample, old_bytes)) = source.samples.back_mut().filter(|(s, _)| s.at_ms == at_ms) {
                sample.buffer_ms = observation.buffer_ms.or(sample.buffer_ms);
                *old_bytes += bytes;
            } else {
                if source.samples.len() == SLOTS {
                    source.samples.pop_front();
                }
                source.samples.push_back((
                    AudioQualitySample {
                        at_ms,
                        buffer_ms: observation.buffer_ms,
                    },
                    bytes,
                ));
            }
        }
        self.sources.retain(|device, source| {
            source
                .samples
                .retain(|(s, _)| at_ms.saturating_sub(s.at_ms) < WINDOW_MS);
            source
                .glitches
                .retain(|b| at_ms / 1000 >= b.at_ms / 1000 && at_ms / 1000 - b.at_ms / 1000 < 60);
            !full || current.contains(device) || !source.samples.is_empty() || !source.glitches.is_empty()
        });
        self.revision += 1;
    }
    pub fn view(&self, at_ms: u64) -> AudioQuality {
        let mut sources = Vec::new();
        for (device, source) in &self.sources {
            let window: Vec<_> = source
                .samples
                .iter()
                .filter(|(s, _)| at_ms.saturating_sub(s.at_ms) < WINDOW_MS)
                .collect();
            let glitches: Vec<_> = source
                .glitches
                .iter()
                .filter(|b| at_ms / 1000 >= b.at_ms / 1000 && at_ms / 1000 - b.at_ms / 1000 < 60)
                .copied()
                .collect();
            if window.is_empty() && glitches.is_empty() {
                continue;
            }
            let values: Vec<_> = window
                .iter()
                .filter_map(|(s, _)| s.buffer_ms)
                .filter(|v| v.is_finite() && *v >= 0.)
                .collect();

            sources.push(AudioSourceQuality {
                device: (*device).into(),
                name: source.name.clone(),
                samples: window.iter().map(|(s, _)| s.clone()).collect(),
                buffer_avg_ms: (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64),
                // Always the complete trailing 60 s, including inactive time.
                bitrate_kbps: window.iter().map(|(_, bytes)| *bytes as f64).sum::<f64>() * 8. / 60_000.,
                interruptions: glitches.iter().map(|b| b.interruptions).sum(),
                concealed_ms: glitches.iter().map(|b| b.concealed_ns as f64 / 1_000_000.0).sum(),
                late_packets: glitches.iter().map(|b| b.late_packets).sum(),
                glitches,
            });
        }
        sources.sort_by_key(|s| s.device.0);
        let output: Vec<_> = self
            .output
            .iter()
            .filter(|b| at_ms / 1000 >= b.at_ms / 1000 && at_ms / 1000 - b.at_ms / 1000 < 60)
            .copied()
            .collect();
        AudioQuality {
            revision: self.revision,
            sampled_at_ms: at_ms,
            sources,
            output: (!output.is_empty()).then(|| crate::api::AudioOutputQuality {
                interruptions: output.iter().map(|b| b.interruptions).sum(),
                missing_ms: output.iter().map(|b| b.missing_ns as f64 / 1_000_000.0).sum(),
                glitches: output,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(value: u64) -> (String, Observation) {
        (
            "Source".into(),
            Observation {
                device: DeviceId([2; 32]),
                buffer_ms: Some(value as f64),
                bytes: value * 1000,
                glitches: {
                    let mut b = [None; 60];
                    b[0] = Some(oink_audio::sink::DropoutBucket {
                        at_ms: 0,
                        lost: 0,
                        late_packets: value,
                        interruptions: value,
                        ..Default::default()
                    });
                    b
                },
                session: 1,
            },
        )
    }
    #[test]
    fn window_average_payload_bitrate_and_late_deltas_are_bounded() {
        let mut h = History::default();
        for tick in 1..=480 {
            h.record(tick * 250, [input(tick)]);
        }
        let v = h.view(120_000);
        let s = &v.sources[0];
        assert_eq!(s.samples.len(), 240);
        assert_eq!(s.buffer_avg_ms, Some(360.5));
        assert_eq!(s.bitrate_kbps, 32.);
        assert_eq!(s.interruptions, s.glitches.iter().map(|b| b.interruptions).sum::<u64>());
        h.record(120_250, []);
        assert_eq!(h.view(120_250).sources[0].samples.len(), 239);
        assert!(h.view(180_000).sources.is_empty());
    }
    #[test]
    fn restart_and_inactive_gaps_do_not_turn_into_counter_wraps_or_zero_buffers() {
        let mut h = History::default();
        h.record(250, [input(1)]);
        h.record(500, []);
        let (name, mut next) = input(3);
        next.session = 2;
        h.record(750, [(name, next)]);
        let v = h.view(750);
        assert_eq!(v.sources[0].samples.len(), 2);
        assert_eq!(v.sources[0].interruptions, 4);
        assert_eq!(v.sources[0].bitrate_kbps, 4000. * 8. / 60000.);
    }
    #[test]
    fn silent_sessions_expire_without_recounting_payload_on_resume() {
        let mut h = History::default();
        h.record(250, [input(1)]);
        let (name, mut silent) = input(2);
        silent.buffer_ms = None;
        h.record(500, [(name.clone(), silent)]);
        for tick in 3..=250 {
            h.record(tick * 250, [(name.clone(), silent)]);
        }
        assert!(h.view(62500).sources.is_empty());
        let (name, mut resumed) = input(3);
        resumed.glitches[0] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 62000,
            lost: 1,
            late: 0,
            interruptions: 1,
            ..Default::default()
        });
        h.record(62750, [(name, resumed)]);
        let s = &h.view(62750).sources[0];
        assert_eq!(s.interruptions, 1);
        assert_eq!(s.buffer_avg_ms, Some(3.));
        assert_eq!(s.bitrate_kbps, 1000. * 8. / 60000.);
    }
    #[test]
    fn final_counter_flush_shares_a_tick_without_duplicating_samples() {
        let mut h = History::default();
        h.record(250, [input(1)]);
        let (name, mut end) = input(2);
        end.buffer_ms = None;
        h.flush(299, [(name, end)]);
        let s = &h.view(300).sources[0];
        assert_eq!(s.samples.len(), 1);
        assert_eq!(s.interruptions, 2);
        assert_eq!(s.buffer_avg_ms, Some(1.));
    }
    #[test]
    fn source_seconds_merge_sessions_deduplicate_snapshots_and_expire_at_boundary() {
        let mut h = History::default();
        let (name, mut o) = input(0);
        o.glitches[0] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 0,
            lost: 1,
            late: 1,
            interruptions: 2,
            ..Default::default()
        });
        h.record(999, [(name.clone(), o)]);
        h.record(1000, [(name.clone(), o)]);
        assert_eq!(h.view(1000).sources[0].interruptions, 2);
        o.glitches[1] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 1000,
            lost: 2,
            late: 0,
            interruptions: 2,
            ..Default::default()
        });
        h.record(1250, [(name.clone(), o)]);
        assert_eq!(h.view(59999).sources[0].interruptions, 4);
        assert_eq!(h.view(60000).sources[0].interruptions, 2);
        assert!(h.view(61000).sources[0].glitches.is_empty());
        o.session = 2;
        o.glitches = [None; 60];
        o.glitches[0] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 1000,
            lost: 1,
            late: 0,
            interruptions: 1,
            ..Default::default()
        });
        h.flush(1500, [(name, o)]);
        let s = &h.view(1500).sources[0];
        assert_eq!(s.interruptions, 5);
        assert_eq!(s.glitches.len(), 2);
    }
    #[test]
    fn concealed_duration_and_late_packets_do_not_become_interruptions_across_sessions() {
        let mut h = History::default();
        let (name, mut o) = input(0);
        o.glitches[0] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 1000,
            lost: 13,
            concealed_ns: 60_000_000,
            late_packets: 12,
            ..Default::default()
        });
        h.record(1250, [(name.clone(), o)]);
        h.flush(1299, [(name.clone(), o)]);
        o.session = 2;
        o.glitches = [None; 60];
        h.record(1500, [(name.clone(), o)]);
        h.record(1750, [(name, o)]);
        let view = h.view(1750);
        assert_eq!(view.sources[0].interruptions, 0);
        assert_eq!(view.sources[0].concealed_ms, 60.0);
        assert_eq!(view.sources[0].late_packets, 12);
        assert!(view.output.is_none());
        let mut output = [None; 60];
        output[1] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 1000,
            interruptions: 1,
            missing_ns: 10_000_000,
            ..Default::default()
        });
        h.record_output(output);
        h.record_output(output); // snapshots, not deltas
        assert_eq!(h.view(1750).output.unwrap().interruptions, 1);
        assert_eq!(h.view(1750).sources[0].interruptions, 0);
        assert!(h.view(61000).output.is_none());
    }
    #[test]
    fn local_api_shape_has_only_receive_window_data() {
        let mut h = History::default();
        h.record(250, [input(1)]);
        assert!(matches!(
            serde_json::from_value::<crate::api::Request>(serde_json::json!({"method":"audio_quality"})).unwrap(),
            crate::api::Request::AudioQuality
        ));
        let event = serde_json::to_value(crate::api::ApiEvent::AudioQuality(h.view(250))).unwrap();
        assert_eq!(event["type"], "audio_quality");
        let json = serde_json::to_value(h.view(250)).unwrap();
        assert_eq!(json["sources"][0]["samples"][0]["buffer_ms"], 1.);
        assert_eq!(json["sources"][0]["interruptions"], 1);
        assert_eq!(json["sources"][0]["glitches"][0]["interruptions"], 1);
        assert_eq!(json["sources"][0]["late_packets"], 1);
        assert_eq!(json["sources"][0]["name"], "Source");
        assert_eq!(serde_json::from_value::<AudioQuality>(json).unwrap(), h.view(250));
    }
}
