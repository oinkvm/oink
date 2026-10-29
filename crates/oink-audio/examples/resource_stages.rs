//! Repeatable, device-free audio stage costs. No driver, network socket or desktop.
//! Run in release mode; times are work ns per second of synthetic 48 kHz audio.
use oink_audio::{dsp, media, negotiation::Profile, source};
use std::{
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct WriterTimes {
    encode: AtomicU64,
    packetize: AtomicU64,
    send: AtomicU64,
}
fn ns(at: Instant) -> u64 {
    at.elapsed().as_nanos() as u64
}
fn main() {
    if cfg!(debug_assertions) {
        eprintln!("use --release for resource measurements");
        std::process::exit(2);
    }
    println!(
        "AUDIO,profile,loss,diagnostics,silent,seconds,capture_preprocess_ns,encode_ns,packetize_ns,queue_send_ns,receive_parse_ns,jitter_admit_ns,decode_excluding_plc_ns,plc_ns,resample_ns,render_other_ns,copy_ns,diagnostics_ns,packets,lost,limited"
    );
    for profile in Profile::ALL {
        for loss in [false, true] {
            for diagnostics in [false, true] {
                for silent in [false, true] {
                    if silent && loss {
                        continue;
                    }
                    run(profile, loss, diagnostics, silent);
                }
            }
        }
    }
}
fn run(profile: Profile, loss: bool, diagnostics: bool, silent: bool) {
    let config = profile.config();
    let origin = Instant::now();
    let seconds = 20u64;
    let ticks = seconds * 1_000_000 / u64::from(profile.frame_us());
    let period = Duration::from_micros(u64::from(profile.frame_us()));
    let pcm: Vec<f32> = (0..config.frame_size())
        .flat_map(|i| {
            let value = if silent {
                0.0
            } else {
                (i as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin() * 0.2
            };
            [value, -value]
        })
        .collect();
    let (mut capture, mut encoded) = source::channel(config, 100, false).unwrap();
    let times = Arc::new(WriterTimes::default());
    let queue = Arc::new(crossbeam_queue::ArrayQueue::new(64));
    let callback_times = times.clone();
    let callback_queue = queue.clone();
    encoded.set_packet_writer(Arc::new(move |packet| {
        callback_times.encode.fetch_add(
            packet.encode_end.duration_since(packet.encode_start).as_nanos() as u64,
            Relaxed,
        );
        let began = Instant::now();
        let mut frame = [0; media::FRAME_CAPACITY];
        let mut wire_packet = *packet;
        // Arrival and sender clocks must share the synthetic cadence; CPU work
        // runs faster than real time, while encode duration above remains real.
        wire_packet.encode_end = packet.delivered_at;
        let len = media::encode(&wire_packet, 1, origin, &mut frame).unwrap();
        callback_times.packetize.fetch_add(ns(began), Relaxed);
        let began = Instant::now();
        let result = callback_queue.push((frame, len)).is_ok();
        callback_times.send.fetch_add(ns(began), Relaxed);
        result
    }));
    let mut receiver = dsp::Receiver::new_with_config(config).unwrap();
    let mut output = vec![0.0; config.frame_samples()];
    let mut native = vec![0i16; config.frame_samples()];
    let (mut supply, mut parse, mut admit, mut decode, mut plc, mut resample, mut render, mut copy, mut diagnostic) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut packets = 0u64;
    for tick in 0..ticks {
        let at = origin + period * tick as u32;
        let began = Instant::now();
        capture.supply(
            &pcm,
            source::Stamp {
                first_sample_at: None,
                callback_at: None,
                delivered_at: at,
                source_frame_position: None,
                discontinuity: false,
            },
        );
        supply += ns(began);
        while let Some((bytes, len)) = queue.pop() {
            let began = Instant::now();
            let packet = media::decode(&bytes[..len], 1, profile).unwrap();
            parse += ns(began);
            if let media::Media::Packet {
                sequence,
                payload,
                source_time,
                ..
            } = packet
            {
                packets += 1;
                // A two-packet burst every 100 packets exercises PLC, not corruption.
                if loss && (40..42).contains(&(sequence % 100)) {
                    continue;
                }
                let began = Instant::now();
                assert!(receiver.push_slice_at(sequence, payload, at, Some(source_time)));
                admit += ns(began);
            }
        }
        let mut trace = dsp::RenderTrace::default();
        let mut budget = 6;
        let began = Instant::now();
        receiver.pull_into(at, config.frame_size(), &mut output, &mut budget, Some(&mut trace));
        render += ns(began);
        decode += trace.decode_ns;
        plc += trace.plc_ns;
        resample += trace.resample_ns;
        let began = Instant::now();
        for (out, sample) in native.iter_mut().zip(&output) {
            *out = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        black_box(&native);
        copy += ns(began);
        // Drain the production handoffs even with serialization disabled.
        let began = Instant::now();
        while let Some(observation) = encoded.observation() {
            black_box(observation);
        }
        if tick % (1_000_000 / u64::from(profile.frame_us())) == 0 {
            let levels = encoded.levels();
            let stats = receiver.stats();
            if diagnostics {
                black_box(serde_json::to_vec(&(levels, stats, trace)).unwrap());
            }
        }
        diagnostic += ns(began);
    }
    let encode = times.encode.load(Relaxed);
    let packetize = times.packetize.load(Relaxed);
    let send = times.send.load(Relaxed);
    let preprocessing = supply.saturating_sub(encode + packetize + send);
    let other_render = render.saturating_sub(decode + resample);
    let stats = receiver.stats();
    println!(
        "AUDIO,{profile:?},{loss},{diagnostics},{silent},{seconds},{},{},{},{},{},{},{},{},{},{},{},{},{packets},{},{}",
        preprocessing / seconds,
        encode / seconds,
        packetize / seconds,
        send / seconds,
        parse / seconds,
        admit / seconds,
        (decode - plc) / seconds,
        plc / seconds,
        resample / seconds,
        other_render / seconds,
        copy / seconds,
        diagnostic / seconds,
        stats.lost,
        stats.work_limited
    );
    assert!(output.iter().all(|sample| sample.is_finite()));
    assert_eq!(stats.work_limited, 0);
    assert_eq!(encoded.take_dropped(), 0);
    if loss {
        assert!(stats.lost > 0);
    }
}
