//! Complete device-free native-pull callback, including receive, quality counters,
//! mixing and native format adaptation. Compare with resource_stages DSP totals.
use oink_audio::{
    dsp,
    negotiation::Profile,
    sink::{Packet, Simulator},
};
use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};
fn main() {
    if cfg!(debug_assertions) {
        eprintln!("use --release for resource measurements");
        std::process::exit(2);
    }
    println!(
        "PULL,profile,loss,trace,seconds,fill_ns_per_s,queue_ns_per_s,diagnostics_ns_per_s,max_fill_us,lost,limited"
    );
    for profile in Profile::ALL {
        for loss in [false, true] {
            for trace in [false, true] {
                run(profile, loss, trace);
            }
        }
    }
}
fn run(profile: Profile, loss: bool, trace: bool) {
    let config = profile.config();
    let origin = Instant::now();
    let pcm: Vec<_> = (0..config.frame_size())
        .flat_map(|i| {
            let v = (i as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin() * 0.2;
            [v, -v]
        })
        .collect();
    let encoded = dsp::Encoder::new_with_config(config).unwrap().encode(&pcm).unwrap();
    let mut payload = [0; dsp::MAX_PACKET_BYTES];
    payload[..encoded.len()].copy_from_slice(&encoded);
    let mut sim = Simulator::new(trace);
    sim.set_output_quality_counters(Default::default());
    let mut source = sim.source(config).unwrap();
    source.set_dropout_counters(Default::default());
    let queue = Arc::new(crossbeam_queue::ArrayQueue::new(64));
    let read = queue.clone();
    source.set_packet_reader(Arc::new(move |budget| {
        if *budget == 0 {
            return (true, None);
        }
        *budget -= 1;
        (true, read.pop())
    }));
    let seconds = 20u64;
    let ticks = seconds * 48_000 / 128;
    let mut sequence = 0u64;
    let mut next = 0u64;
    let mut output = [0.0; 256];
    let (mut fill, mut send, mut diagnostic, mut maximum) = (0u128, 0u128, 0u128, 0u128);
    for tick in 0..ticks {
        let time_ns = tick * 128 * 1_000_000_000 / 48_000;
        let at = origin + Duration::from_nanos(time_ns);
        while next <= time_ns {
            if !loss || !(40..42).contains(&(sequence % 100)) {
                let packet = Packet {
                    burst: 1,
                    sequence,
                    payload,
                    len: encoded.len(),
                    arrived: origin + Duration::from_nanos(next),
                    network_read: None,
                    source_emit: Some(next),
                };
                let began = Instant::now();
                queue.push(packet).unwrap();
                send += began.elapsed().as_nanos();
            }
            sequence += 1;
            next = sequence * u64::from(profile.frame_us()) * 1000;
        }
        let began = Instant::now();
        sim.fill(&mut output, 48_000, 2, 128, at, None);
        let elapsed = began.elapsed().as_nanos();
        fill += elapsed;
        maximum = maximum.max(elapsed);
        black_box(&output);
        let began = Instant::now();
        source.poll();
        while let Some(outcome) = source.outcome() {
            black_box(outcome);
        }
        if tick % 375 == 0 {
            let metrics = sim.metrics();
            if trace {
                black_box(serde_json::to_vec(&serde_json::json!({"callbacks": metrics.callbacks, "max_render_us": metrics.max_render_us, "underrun_frames": metrics.underrun_frames, "source": source.stats()})).unwrap());
            }
        }
        diagnostic += began.elapsed().as_nanos();
    }
    let stats = source.stats();
    println!(
        "PULL,{profile:?},{loss},{trace},{seconds},{},{},{},{},{},{}",
        fill / u128::from(seconds),
        send / u128::from(seconds),
        diagnostic / u128::from(seconds),
        maximum / 1000,
        stats.lost,
        stats.work_limited
    );
    assert_eq!(stats.work_limited, 0);
    if loss {
        assert!(stats.lost > 0);
    }
    assert!(output.iter().all(|s| s.is_finite()));
}
