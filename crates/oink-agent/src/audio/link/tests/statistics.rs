//! Synthetic clocks exercise publication cadence without timers or native IO.
use super::*;

#[tokio::test]
async fn measurement_clock_does_not_republish_idle_state_and_source_phase_is_typed() {
    let mut running = Running::new().await;
    let counts = Arc::new(Mutex::new((0_usize, 0_usize)));
    let states = counts.clone();
    running.owners[0].on_status = Some(Arc::new(move |_| lock(&states).0 += 1));
    let statistics = counts.clone();
    running.owners[0].on_statistics = Some(Arc::new(move |_| lock(&statistics).1 += 1));
    let base = Instant::now();
    running.owners[0].report_at = base;
    for second in 1..=60 {
        running.owners[0].publish_at(AudioPhase::Idle, base + Duration::from_secs(second));
    }
    assert_eq!(lock(&counts).1, 60);
    assert!(lock(&counts).0 <= 1, "unchanged idle state is not a statistics clock");
    running.claim(1, true);
    running.until(|r| r.owners[0].capture.is_some()).await;
    running.supply(0, 0.1);
    running.owners[0].publish_at(AudioPhase::Forwarding, base + Duration::from_secs(61));
    assert_eq!(lock(&running.owners[0].state).phase, AudioPhase::Forwarding);
    assert!(lock(&counts).1 > 60);
    running.stop().await;
}

#[tokio::test]
async fn local_audio_quality_samples_without_a_ui_and_flushes_on_receiver_retirement() {
    let mut running = Running::new().await;
    let history = Arc::new(Mutex::new(crate::audio_quality::History::default()));
    let captured = history.clone();
    running.owners[1].on_quality = Some(Arc::new(move |at, observations, output, final_bucket| {
        if let Some(output) = output {
            lock(&captured).record_output(output);
        }
        let named = observations.into_iter().map(|o| ("Source".into(), o));
        if final_bucket {
            lock(&captured).flush(at, named);
        } else {
            lock(&captured).record(at, named);
        }
    }));
    running.claim(1, true);
    running.until(|r| r.owners[0].capture.is_some()).await;
    for _ in 0..8 {
        running.supply(0, 0.1);
    }
    running
        .until(|r| {
            r.owners[1].receivers.values().any(|r| {
                // This fixture's fake output has no RT worker; consume the real
                // QUIC handoff explicitly, as the existing silence tests do.
                let _ = r.reader.read();
                r.reader.payload_bytes() > 0
            })
        })
        .await;
    let now = Instant::now();
    running.owners[1].quality_at = now - Duration::from_millis(250);
    running.owners[1].publish_at(AudioPhase::Idle, now);
    let at = (oink_audio::diagnostics::monotonic_us(now).max(0) / 1000) as u64;
    let view = lock(&history).view(at);
    assert_eq!(view.sources.len(), 1);
    assert!(view.sources[0].bitrate_kbps > 0.);
    let bytes = running.owners[1]
        .receivers
        .values()
        .next()
        .unwrap()
        .reader
        .payload_bytes();
    running.owners[1].receivers.clear();
    let view = lock(&history).view(at);
    assert_eq!(view.sources[0].bitrate_kbps, bytes as f64 * 8. / 60000.);
    running.stop().await;
}

#[test]
fn speaker_and_microphone_sleep_keep_quality_counters_idle_and_wake_on_media() {
    use crate::streams::audio::{ForwardFrom, MediaReader};
    use oink_audio::media::Kind;
    use oink_foundation::snapshot::Snapshot;
    use oink_network::link::{
        LinkId, QueueSpec,
        testing::{partitioned_datagram_pair, post_datagram_at},
    };
    use oink_protocol::frame::{StreamKind, StreamTag};

    for kind in [Kind::Speaker, Kind::Microphone] {
        // Deliberately precede setup: every ingress/render timestamp below is
        // synthetic, so scheduler delays must not change startup jitter waits.
        let now = Instant::now() - Duration::from_secs(1);
        let peer = DeviceId([42; 32]);
        let (send, [speaker, microphone]) = partitioned_datagram_pair(
            LinkId(1),
            StreamTag {
                kind: StreamKind::Audio,
                ordinal: 1,
            },
            QueueSpec::datagram(StreamKind::Audio),
            [
                [Kind::Speaker.packet_id(), Kind::Speaker.sleep_id()],
                [Kind::Microphone.packet_id(), Kind::Microphone.sleep_id()],
            ],
        );
        let receive = match kind {
            Kind::Speaker => speaker,
            Kind::Microphone => microphone,
        };
        let table = Arc::new(Snapshot::new(ForwardTable {
            to: vec![],
            from: vec![ForwardFrom {
                admission: Default::default(),
                peer,
                stream_id: 7,
                format: StreamFormat::Opus5ms,
                gain: 1.0,
                playing: true,
                packets: Arc::new(receive),
            }],
        }));
        let reader = Arc::new(MediaReader::new_kind(table, peer, None, kind));
        let mut sink = oink_audio::sink::Simulator::new(false);
        let mut source = sink.source(StreamFormat::Opus5ms.config()).unwrap();
        Receiver::connect_source(&source, &reader);
        reader.give_realtime();
        let mut encoder = oink_audio::dsp::Encoder::new_with_config(StreamFormat::Opus5ms.config()).unwrap();
        let payload = encoder.encode(&[0.25; 480]).unwrap();
        let post = |burst, ordinal, sleep, at| {
            let mut packet = oink_audio::source::Packet {
                burst,
                ordinal,
                sleep,
                payload: [0; oink_audio::dsp::MAX_PACKET_BYTES],
                len: payload.len(),
                silent: false,
                delivered_at: at,
                encode_start: at,
                encode_end: at,
                remaining_samples: 0,
            };
            packet.payload[..payload.len()].copy_from_slice(&payload);
            let mut bytes = [0; oink_audio::media::FRAME_CAPACITY];
            let len = oink_audio::media::encode_kind(kind, &packet, 7, now, &mut bytes).unwrap();
            post_datagram_at(&send, &bytes[..len], at).unwrap();
        };
        for sequence in 1..=8 {
            post(1, sequence, false, now);
        }
        let mut output = [0.; 256];
        sink.fill(&mut output, 48_000, 2, 128, now + Duration::from_millis(50), None);
        assert!(output.iter().any(|s| s.abs() > 0.1));
        post(1, 9, true, now + Duration::from_millis(50));
        for tick in 1..=400 {
            let at = now + Duration::from_micros(50_000 + tick * 2667);
            crate::allocation_audit::assert_no_heap(|| sink.fill(&mut output, 48_000, 2, 128, at, None));
        }
        source.poll();
        assert!(reader.bursts.asleep(now + Duration::from_secs(65)));
        assert!(reader.health_error(now + Duration::from_secs(65)).is_none());
        assert_eq!(reader.payload_bytes(), 8 * payload.len() as u64);
        assert_eq!(source.stats().lost, 0, "{kind:?} Sleep manufactured PLC");
        let at = (oink_audio::diagnostics::monotonic_us(now).max(0) / 1000) as u64 + 2000;
        let quality: Vec<_> = reader.dropouts.snapshot(at).into_iter().flatten().collect();
        assert_eq!(quality.iter().map(|b| b.interruptions).sum::<u64>(), 0, "{kind:?}");
        assert_eq!(quality.iter().map(|b| b.concealed_ns).sum::<u64>(), 0, "{kind:?}");
        assert_eq!(quality.iter().map(|b| b.missing_ns).sum::<u64>(), 0, "{kind:?}");
        let wake = now + Duration::from_secs(2);
        post(2, 1, false, wake - Duration::from_millis(50));
        sink.fill(&mut output, 48_000, 2, 128, wake, None);
        assert!(!reader.bursts.asleep(wake));
        assert!(output.iter().any(|s| s.abs() > 0.1), "{kind:?} did not wake on media");
    }
}
