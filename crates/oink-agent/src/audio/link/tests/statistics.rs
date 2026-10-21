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
    running.owners[1].on_quality = Some(Arc::new(move |at, observations, final_bucket| {
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
