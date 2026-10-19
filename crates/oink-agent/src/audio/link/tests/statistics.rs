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
