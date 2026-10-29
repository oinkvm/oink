//! Additional storage/work bounds; no devices. Full M1 evidence is in agent pull_tests.
use oink_audio::{
    dsp,
    pull_test_support::Priority,
    sink::{Packet, Simulator},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
fn main() -> Result<(), String> {
    eprintln!("PULLBOUNDS,codec,frames,channels,saturated,p999_us,max_us,period_us,limited");
    for config in [dsp::Config::low_latency_opus(), dsp::Config::low_latency_pcm()] {
        for (frames, channels, saturated) in [
            (128usize, 64u16, false),
            (1024, 64, false),
            (128, 2, true),
            (16384, 1, true),
        ] {
            let origin = Instant::now();
            let mut sim = Simulator::new(true);
            sim.set_output_quality_counters(Default::default());
            let mut encoder = dsp::Encoder::new_with_config(config).map_err(|e| e.to_string())?;
            let pcm: Vec<_> = (0..config.frame_size())
                .flat_map(|i| {
                    let s = (i as f32 * 0.1).sin() * 0.02;
                    [s, -s]
                })
                .collect();
            let encoded = encoder.encode(&pcm).map_err(|e| e.to_string())?;
            let mut sources = Vec::new();
            let clock = Arc::new(AtomicU64::new(0));
            for _ in 0..16 {
                let source = sim.source(config)?;
                source.set_dropout_counters(Default::default());
                let mut payload = [0; dsp::MAX_PACKET_BYTES];
                payload[..encoded.len()].copy_from_slice(&encoded);
                let len = encoded.len();
                let sequence = AtomicU64::new(0);
                let clock = clock.clone();
                source.set_packet_reader(Arc::new(move |budget| {
                    if *budget == 0 {
                        return (true, None);
                    }
                    *budget -= 1;
                    let seq = sequence.load(Ordering::Relaxed);
                    let emit_ns = seq * config.frame_size() as u64 * 1_000_000_000 / 48_000;
                    if !saturated && emit_ns > clock.load(Ordering::Relaxed) {
                        return (true, None);
                    }
                    sequence.store(seq + 1, Ordering::Relaxed);
                    (
                        true,
                        Some(Packet {
                            burst: 1,
                            sequence: seq,
                            payload,
                            len,
                            arrived: origin + Duration::from_nanos(emit_ns),
                            network_read: None,
                            source_emit: Some(emit_ns),
                        }),
                    )
                }));
                sources.push(source);
            }
            let period = Duration::from_secs_f64(frames as f64 / 48_000.0);
            let samples = if frames == 16384 { 64 } else { 1200 };
            let mut timings = Vec::with_capacity(samples);
            let mut output = vec![0.0; frames * usize::from(channels)];
            let priority = // Mach rejects a 170 ms computation reservation for the storage-limit case.
                // Cap the synthetic policy period; actual demand/cadence stay unchanged.
                Priority::enter(period.min(Duration::from_millis(20)).as_nanos() as u64).map_err(|e| format!("priority: {e}"))?;
            let mut due = Instant::now();
            for tick in 0..samples + 20 {
                let ns = tick as u64 * frames as u64 * 1_000_000_000 / 48_000;
                clock.store(ns, Ordering::Relaxed);
                let began = Instant::now();
                sim.fill(
                    &mut output,
                    48_000,
                    channels,
                    frames as u32,
                    origin + Duration::from_nanos(ns),
                    None,
                );
                let elapsed = began.elapsed().as_micros();
                if tick >= 20 {
                    timings.push(elapsed);
                }
                for source in &mut sources {
                    source.poll();
                }
                assert!(output.iter().all(|s| s.is_finite()));
                due += period;
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
            }
            drop(priority);
            timings.sort_unstable();
            let limited: u64 = sources.iter().map(|s| s.stats().work_limited).sum();
            eprintln!(
                "PULLBOUNDS,{:?},{frames},{channels},{saturated},{},{},{},{}",
                config.codec,
                timings[(samples * 999 / 1000).min(samples - 1)],
                timings.last().unwrap(),
                period.as_micros(),
                limited
            );
            assert_eq!(limited, 0);
        }
    }
    Ok(())
}
