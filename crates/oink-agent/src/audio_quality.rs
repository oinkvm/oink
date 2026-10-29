//! Local receive-only rolling diagnostics. Never replicated or persisted.
use crate::api::{AudioQuality, AudioQualitySample, AudioSourceQuality};
use oink_foundation::DeviceId;
use std::collections::{HashMap, HashSet, VecDeque};

const WINDOW_MS: u64 = 300_000;
const SLOTS: usize = 1200;
#[derive(Clone, Copy)]
pub(crate) struct Observation {
    pub device: DeviceId,
    pub kind: oink_audio::media::Kind,
    pub buffer_ms: Option<f64>,
    pub bytes: u64,
    pub glitches: [Option<oink_audio::sink::DropoutBucket>; oink_audio::sink::QUALITY_SECONDS],
    pub session: u64,
}
#[derive(Default)]
pub(crate) struct History {
    sources: HashMap<(DeviceId, oink_audio::media::Kind), Source>,
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
    pub fn record_output(
        &mut self,
        buckets: [Option<oink_audio::sink::DropoutBucket>; oink_audio::sink::QUALITY_SECONDS],
    ) {
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
            current.insert((observation.device, observation.kind));
            let source = self.sources.entry((observation.device, observation.kind)).or_default();
            source.name = name;
            let bytes = source.previous.as_ref().map_or(observation.bytes, |(_, old)| {
                if old.session == observation.session {
                    observation.bytes.saturating_sub(old.bytes)
                } else {
                    observation.bytes
                }
            });
            for bucket in observation.glitches.into_iter().flatten() {
                if at_ms / 1000 < bucket.at_ms / 1000 || at_ms / 1000 - bucket.at_ms / 1000 >= WINDOW_MS / 1000 {
                    continue;
                }
                let old = source
                    .previous
                    .as_ref()
                    .filter(|(_, old)| old.session == observation.session)
                    .and_then(|(_, old)| old.glitches.iter().flatten().find(|b| b.at_ms == bucket.at_ms).copied())
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
            while source.glitches.len() > oink_audio::sink::QUALITY_SECONDS {
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
                .retain(|b| at_ms / 1000 >= b.at_ms / 1000 && at_ms / 1000 - b.at_ms / 1000 < WINDOW_MS / 1000);
            !full || current.contains(device) || !source.samples.is_empty() || !source.glitches.is_empty()
        });
        self.revision += 1;
    }
    pub fn view(&self, at_ms: u64) -> AudioQuality {
        let mut sources = Vec::new();
        for ((device, kind), source) in &self.sources {
            let window: Vec<_> = source
                .samples
                .iter()
                .filter(|(s, _)| at_ms.saturating_sub(s.at_ms) < WINDOW_MS)
                .collect();
            let glitches: Vec<_> = source
                .glitches
                .iter()
                .filter(|b| at_ms / 1000 >= b.at_ms / 1000 && at_ms / 1000 - b.at_ms / 1000 < WINDOW_MS / 1000)
                .copied()
                .collect();
            if window.is_empty() && glitches.is_empty() {
                continue;
            }
            let stats = |window_ms: u64| {
                let samples: Vec<_> = window
                    .iter()
                    .filter(|(s, _)| at_ms.saturating_sub(s.at_ms) < window_ms)
                    .collect();
                let values: Vec<_> = samples
                    .iter()
                    .filter_map(|(s, _)| s.buffer_ms)
                    .filter(|v| v.is_finite() && *v >= 0.)
                    .collect();
                let buckets: Vec<_> = glitches
                    .iter()
                    .filter(|b| at_ms / 1000 - b.at_ms / 1000 < window_ms / 1000)
                    .collect();
                crate::api::AudioWindowStats {
                    buffer_avg_ms: (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64),
                    // Include inactive time in the full selected window.
                    bitrate_kbps: samples.iter().map(|(_, bytes)| *bytes as f64).sum::<f64>() * 8. / window_ms as f64,
                    interruptions: buckets.iter().map(|b| b.interruptions).sum(),
                    output_interruptions: buckets.iter().map(|b| b.output_interruptions).sum(),
                    discontinuities: buckets.iter().map(|b| b.discontinuities).sum(),
                    concealed_ms: buckets.iter().map(|b| b.concealed_ns as f64 / 1_000_000.).sum(),
                    late_packets: buckets.iter().map(|b| b.late_packets).sum(),
                }
            };
            let minute = stats(60_000);
            sources.push(AudioSourceQuality {
                stats_5m: stats(WINDOW_MS),
                device: (*device).into(),
                kind: *kind,
                name: source.name.clone(),
                samples: window.iter().map(|(s, _)| s.clone()).collect(),
                buffer_avg_ms: minute.buffer_avg_ms,
                bitrate_kbps: minute.bitrate_kbps,
                interruptions: minute.interruptions,
                discontinuities: minute.discontinuities,
                output_interruptions: minute.output_interruptions,
                concealed_ms: minute.concealed_ms,
                late_packets: minute.late_packets,
                glitches,
            });
        }
        sources.sort_by_key(|s| (s.device.0, s.kind));
        let output: Vec<_> = self
            .output
            .iter()
            .filter(|b| at_ms / 1000 >= b.at_ms / 1000 && at_ms / 1000 - b.at_ms / 1000 < WINDOW_MS / 1000)
            .copied()
            .collect();
        let output_stats = |window_ms: u64| {
            let buckets: Vec<_> = output
                .iter()
                .filter(|b| at_ms / 1000 - b.at_ms / 1000 < window_ms / 1000)
                .collect();
            crate::api::OutputWindowStats {
                interruptions: buckets.iter().map(|b| b.interruptions).sum(),
                discontinuities: buckets.iter().map(|b| b.discontinuities).sum(),
                missing_ms: buckets.iter().map(|b| b.missing_ns as f64 / 1_000_000.).sum(),
                callbacks: buckets.iter().map(|b| b.callbacks).sum(),
                callback_over_budget: buckets.iter().map(|b| b.callback_over_budget).sum(),
                native_timing_callbacks: buckets.iter().map(|b| b.native_timing_callbacks).sum(),
                native_gap_frames: buckets.iter().map(|b| b.native_gap_frames).sum(),
                callback_max_work_us: buckets.iter().map(|b| b.callback_max_work_us).max().unwrap_or(0),
                callback_min_margin_us: buckets.iter().filter_map(|b| b.callback_min_margin_us).min(),
            }
        };
        let minute = output_stats(60_000);
        let five_minutes = output_stats(WINDOW_MS);
        AudioQuality {
            revision: self.revision,
            sampled_at_ms: at_ms,
            sources,
            output: (!output.is_empty()).then_some(crate::api::AudioOutputQuality {
                stats_5m: five_minutes,
                interruptions: minute.interruptions,
                missing_ms: minute.missing_ms,
                discontinuities: minute.discontinuities,
                callbacks: minute.callbacks,
                callback_over_budget: minute.callback_over_budget,
                native_timing_callbacks: minute.native_timing_callbacks,
                native_gap_frames: minute.native_gap_frames,
                callback_max_work_us: minute.callback_max_work_us,
                callback_min_margin_us: minute.callback_min_margin_us,
                glitches: output,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_metrics_keep_distinct_one_and_five_minute_windows_and_unknown_timing() {
        let mut history = History::default();
        let mut buckets = [None; oink_audio::sink::QUALITY_SECONDS];
        buckets[10] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 10_000,
            callbacks: 1,
            callback_over_budget: 1,
            callback_max_work_us: 3000,
            callback_min_margin_us: Some(-333),
            native_timing_callbacks: 1,
            native_gap_frames: 480,
            ..Default::default()
        });
        buckets[230] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 230_000,
            callbacks: 2,
            callback_max_work_us: 200,
            callback_min_margin_us: Some(2467),
            ..Default::default()
        });
        history.record_output(buckets);
        let output = history.view(240_000).output.unwrap();
        assert_eq!(output.callbacks, 2);
        assert_eq!(output.callback_over_budget, 0);
        assert_eq!(output.callback_min_margin_us, Some(2467));
        assert_eq!(
            output.native_timing_callbacks, 0,
            "no native evidence is unknown, not a healthy clock"
        );
        assert_eq!(output.stats_5m.callbacks, 3);
        assert_eq!(output.stats_5m.callback_over_budget, 1);
        assert_eq!(output.stats_5m.callback_min_margin_us, Some(-333));
        assert_eq!(output.stats_5m.callback_max_work_us, 3000);
        assert_eq!(output.stats_5m.native_gap_frames, 480);
        assert_eq!(output.interruptions, 0, "deadline alone must not invent audible gaps");
    }

    #[test]
    fn both_windows_use_raw_samples_payload_and_source_and_output_buckets() {
        let mut h = History::default();
        let (name, mut old) = input(10);
        old.glitches[0].as_mut().unwrap().concealed_ns = 5_000_000;
        old.glitches[0].as_mut().unwrap().output_interruptions = 3;
        old.glitches[0].as_mut().unwrap().discontinuities = 4;
        h.record(250, [(name.clone(), old)]);
        let (_, mut recent) = input(30);
        recent.glitches[0] = None;
        recent.glitches[1] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 100_000,
            interruptions: 2,
            output_interruptions: 1,
            discontinuities: 2,
            concealed_ns: 7_000_000,
            late_packets: 3,
            ..Default::default()
        });
        h.record(100_250, [(name, recent)]);
        let mut output = [None; oink_audio::sink::QUALITY_SECONDS];
        output[0] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 0,
            interruptions: 4,
            discontinuities: 5,
            missing_ns: 8_000_000,
            ..Default::default()
        });
        output[1] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 100_000,
            interruptions: 1,
            discontinuities: 2,
            missing_ns: 2_000_000,
            ..Default::default()
        });
        h.record_output(output);
        let view = h.view(100_250);
        let source = &view.sources[0];
        assert_eq!(
            (source.buffer_avg_ms, source.bitrate_kbps),
            (Some(30.), 20_000. * 8. / 60_000.)
        );
        assert_eq!(
            (source.interruptions, source.concealed_ms, source.late_packets),
            (2, 7., 3)
        );
        assert_eq!((source.output_interruptions, source.discontinuities), (1, 2));
        let five = &source.stats_5m;
        assert_eq!((five.output_interruptions, five.discontinuities), (4, 6));
        assert_eq!(
            (five.buffer_avg_ms, five.bitrate_kbps),
            (Some(20.), 30_000. * 8. / 300_000.)
        );
        assert_eq!(
            (five.interruptions, five.concealed_ms, five.late_packets),
            (12, 12., 13)
        );
        let output = view.output.as_ref().unwrap();
        assert_eq!((output.interruptions, output.missing_ms), (1, 2.));
        assert_eq!((output.stats_5m.interruptions, output.stats_5m.missing_ms), (5, 10.));
        assert_eq!((output.discontinuities, output.stats_5m.discontinuities), (2, 7));
        // Roundtrip the extended local IPC shape, including both statistics.
        let json = serde_json::to_vec(&view).unwrap();
        assert_eq!(serde_json::from_slice::<AudioQuality>(&json).unwrap(), view);
    }

    #[test]
    fn five_minute_history_has_bounded_memory_and_json_size() {
        let mut h = History::default();
        for tick in 1..=2400 {
            let (name, mut observation) = input(tick);
            observation.glitches[0].as_mut().unwrap().at_ms = tick / 4 * 1000;
            h.record(tick * 250, [(name, observation)]);
        }
        let view = h.view(600_000);
        assert_eq!(view.sources[0].samples.len(), 1200);
        assert!(view.sources[0].glitches.len() <= 300);
        // 300 fixed buckets now also carry native callback deadline/timing metrics.
        assert!(std::mem::size_of::<Observation>() < 48_000);
        assert!(serde_json::to_vec(&view).unwrap().len() < 160_000);
    }

    #[test]
    fn simultaneous_microphone_and_speaker_keep_independent_counters_and_retirement() {
        let mut history = History::default();
        let speaker = input(1);
        let mut microphone = input(4);
        microphone.1.kind = oink_audio::media::Kind::Microphone;
        microphone.1.session = 2;
        history.record(250, [speaker.clone(), microphone.clone()]);
        let view = history.view(250);
        assert_eq!(view.sources.len(), 2);
        assert!(
            view.sources
                .iter()
                .any(|s| s.kind == oink_audio::media::Kind::Speaker && s.buffer_avg_ms == Some(1.))
        );
        assert!(
            view.sources
                .iter()
                .any(|s| s.kind == oink_audio::media::Kind::Microphone && s.buffer_avg_ms == Some(4.))
        );
        microphone.1.buffer_ms = None;
        history.flush(500, [microphone]);
        let view = history.view(500);
        assert_eq!(view.sources.len(), 2);
        assert_eq!(
            view.sources
                .iter()
                .find(|s| s.kind == oink_audio::media::Kind::Speaker)
                .unwrap()
                .bitrate_kbps,
            1000. * 8. / 60_000.
        );
    }
    fn input(value: u64) -> (String, Observation) {
        (
            "Source".into(),
            Observation {
                device: DeviceId([2; 32]),
                kind: oink_audio::media::Kind::Speaker,
                buffer_ms: Some(value as f64),
                bytes: value * 1000,
                glitches: {
                    let mut b = [None; oink_audio::sink::QUALITY_SECONDS];
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
        for tick in 1..=1440 {
            h.record(tick * 250, [input(tick)]);
        }
        let v = h.view(360_000);
        let s = &v.sources[0];
        assert_eq!(s.samples.len(), 1200);
        assert_eq!(s.buffer_avg_ms, Some(1320.5));
        assert_eq!(s.bitrate_kbps, 32.);
        assert_eq!(s.interruptions, s.glitches.iter().map(|b| b.interruptions).sum::<u64>());
        h.record(360_250, []);
        assert_eq!(h.view(360_250).sources[0].samples.len(), 1199);
        assert!(h.view(660_000).sources.is_empty());
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
        for tick in 3..=1250 {
            h.record(tick * 250, [(name.clone(), silent)]);
        }
        assert!(h.view(312500).sources.is_empty());
        let (name, mut resumed) = input(3);
        resumed.glitches[0] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 312000,
            lost: 1,
            late: 0,
            interruptions: 1,
            ..Default::default()
        });
        h.record(312750, [(name, resumed)]);
        let s = &h.view(312750).sources[0];
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
        assert!(h.view(301000).sources[0].glitches.is_empty());
        o.session = 2;
        o.glitches = [None; oink_audio::sink::QUALITY_SECONDS];
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
        o.glitches = [None; oink_audio::sink::QUALITY_SECONDS];
        h.record(1500, [(name.clone(), o)]);
        h.record(1750, [(name, o)]);
        let view = h.view(1750);
        assert_eq!(view.sources[0].interruptions, 0);
        assert_eq!(view.sources[0].concealed_ms, 60.0);
        assert_eq!(view.sources[0].late_packets, 12);
        assert!(view.output.is_none());
        let mut output = [None; oink_audio::sink::QUALITY_SECONDS];
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
        assert!(h.view(301000).output.is_none());
    }
    #[test]
    fn attributed_output_interruptions_survive_history_and_local_api() {
        let mut history = History::default();
        let (name, mut observation) = input(0);
        observation.glitches[1] = Some(oink_audio::sink::DropoutBucket {
            at_ms: 1000,
            interruptions: 1,
            discontinuities: 2,
            output_interruptions: 1,
            ..Default::default()
        });
        history.record(1250, [(name.clone(), observation)]);
        history.record(1500, [(name, observation)]);
        let view = history.view(1500);
        assert_eq!(view.sources[0].interruptions, 1);
        assert_eq!(view.sources[0].glitches[0].output_interruptions, 1);
        assert_eq!(view.sources[0].glitches[0].discontinuities, 2);
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["sources"][0]["glitches"][0]["output_interruptions"], 1);
        assert_eq!(serde_json::from_value::<AudioQuality>(json).unwrap(), view);
        assert_eq!(history.view(61500).sources[0].output_interruptions, 0);
        assert_eq!(history.view(61500).sources[0].discontinuities, 0);
        assert_eq!(history.view(61500).sources[0].stats_5m.output_interruptions, 1);
        assert_eq!(history.view(61500).sources[0].stats_5m.discontinuities, 2);
        assert!(history.view(301500).sources.is_empty());
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
