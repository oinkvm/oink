//! Real network M1 -> MediaReader -> native pull core, without opening devices.
use super::*;
use oink_audio::{
    dsp, media,
    pull_test_support::{CHeapAudit, Priority},
    sink::Simulator,
};
use oink_foundation::snapshot::Snapshot;
use oink_network::link::testing::{partitioned_datagram_pair, post_datagram_at};
use oink_protocol::frame::StreamTag;
use std::{sync::Arc, time::Duration};

struct Harness {
    sim: Simulator,
    senders: Vec<DatagramSend>,
    _readers: Vec<Arc<MediaReader>>,
    tables: Vec<Arc<Snapshot<ForwardTable>>>,
    sources: Vec<oink_audio::sink::Source>,
    payload: Vec<u8>,
    origin: Instant,
}
impl Harness {
    fn new(count: usize) -> Self {
        let mut sim = Simulator::new(true);
        sim.set_output_quality_counters(Default::default());
        let mut senders = Vec::new();
        let mut readers = Vec::new();
        let mut tables = Vec::new();
        let mut sources = Vec::new();
        for index in 0..count {
            let peer = DeviceId([index as u8 + 1; 32]);
            let (send, [recv, _]) = partitioned_datagram_pair(
                LinkId(1),
                StreamTag {
                    kind: StreamKind::Audio,
                    ordinal: 1,
                },
                QueueSpec {
                    messages: 64,
                    bytes: 64 * 1100,
                },
                [[0x0604, 0x0606], [0x0618, 0x0619]],
            );
            let table = Arc::new(Snapshot::new(ForwardTable {
                to: vec![],
                from: vec![ForwardFrom {
                    admission: Default::default(),
                    peer,
                    stream_id: 5,
                    format: StreamFormat::Opus5ms,
                    gain: 1.0,
                    playing: true,
                    packets: Arc::new(recv),
                }],
            }));
            let reader = Arc::new(MediaReader::new(table.clone(), peer, None));
            reader.give_realtime();
            let source = sim.source(dsp::Config::low_latency_opus()).unwrap();
            source.set_dropout_counters(reader.dropouts.clone());
            let callback = reader.clone();
            source.set_packet_reader(Arc::new(move |budget| callback.realtime(budget)));
            senders.push(send);
            readers.push(reader);
            tables.push(table);
            sources.push(source);
        }
        let mut encoder = dsp::Encoder::new_with_config(dsp::Config::low_latency_opus()).unwrap();
        let pcm: Vec<_> = (0..240)
            .flat_map(|i| {
                let sample = (i as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin() * 0.02;
                [sample, -sample]
            })
            .collect();
        let payload = encoder.encode(&pcm).unwrap();
        Self {
            sim,
            senders,
            _readers: readers,
            tables,
            sources,
            payload,
            origin: Instant::now(),
        }
    }
    fn post(&self, sequence: u64, burst: u64, at: Instant, bad: bool) {
        let payload = if bad {
            &[0x8b, 0x41][..]
        } else {
            self.payload.as_slice()
        };
        let mut packet = oink_audio::source::Packet {
            ordinal: sequence,
            burst,
            sleep: false,
            payload: [0; dsp::MAX_PACKET_BYTES],
            len: payload.len(),
            silent: false,
            delivered_at: at,
            encode_start: at,
            encode_end: at,
            remaining_samples: 0,
        };
        packet.payload[..payload.len()].copy_from_slice(payload);
        let mut bytes = [0; media::FRAME_CAPACITY];
        let len = media::encode(&packet, 5, self.origin, &mut bytes).unwrap();
        for sender in &self.senders {
            post_datagram_at(sender, &bytes[..len], at).unwrap();
        }
    }
}
#[test]
fn real_m1_to_fill_has_no_heap_through_bad_packets_first_plc_reset_retirement_and_invalidation() {
    let mut harness = Harness::new(2);
    let c_heap = CHeapAudit::load();
    // Initialize the interposer TLS before auditing a native-equivalent executor.
    if let Some(audit) = c_heap {
        assert!(
            audit.measure(|| {
                std::hint::black_box(vec![7u8; 1234]);
            }) > 0,
            "interposer positive control"
        );
        assert_eq!(audit.measure(|| {}), 0);
    }
    let mut output = [0.0; 256];
    let (mut seen_plc, mut seen_decode_error) = (false, false);
    for tick in 0..160u64 {
        let at = harness.origin + Duration::from_micros(tick * 2667);
        if tick % 2 == 0 && !(30..38).contains(&tick) {
            harness.post(tick / 2, if tick < 90 { 1 } else { 2 }, at, (60..68).contains(&tick));
        }
        if tick == 120 {
            publish_receive_table(&harness.tables[0], ForwardTable::default());
        }
        if tick == 130 {
            drop(harness.sources.pop());
        }
        let mut work = || {
            crate::allocation_audit::assert_no_heap(|| {
                harness
                    .sim
                    .fill(&mut output, 48_000, 2, 128, at, (tick == 110).then_some(480))
            })
        };
        if let Some(audit) = c_heap {
            assert_eq!(
                audit.measure(work),
                0,
                "C/Rust allocator used inside M1->fill at {tick}"
            );
        } else {
            work();
        }
        for source in &mut harness.sources {
            source.poll();
            seen_plc |= source.stats().lost != 0;
            seen_decode_error |= source.stats().decode_errors != 0;
        }
    }
    assert!(
        seen_plc && seen_decode_error,
        "audit must execute PLC and invalid-codec recovery"
    );
    assert!(output.iter().all(|value| value.is_finite()));
    harness.sim.invalidate();
    crate::allocation_audit::assert_no_heap(|| harness.sim.fill(&mut output, 48_000, 2, 128, Instant::now(), None));
    assert_eq!(output, [0.0; 256]);
    eprintln!(
        "real M1->fill audit: Rust alloc/free/realloc=0 C-interposer={}",
        c_heap.is_some()
    );
}

#[test]
fn real_m1_malformed_records_consume_one_callback_scan_budget() {
    let mut harness = Harness::new(1);
    let mut bytes = [0; media::FRAME_CAPACITY];
    let len = oink_protocol::v1::hot::encode_into(
        0x0606,
        &[
            (1, oink_protocol::v1::hot::Value::Uint(5)),
            (2, oink_protocol::v1::hot::Value::Uint(1)),
            (3, oink_protocol::v1::hot::Value::Uint(1)),
        ],
        &mut bytes,
        media::FRAME_CAPACITY,
    )
    .unwrap();
    bytes[8] = 0xf4;
    for _ in 0..16 {
        post_datagram_at(&harness.senders[0], &bytes[..len], harness.origin).unwrap();
    }
    crate::allocation_audit::assert_no_heap(|| harness.sim.fill(&mut [0.0; 256], 48_000, 2, 128, harness.origin, None));
    let mut remaining = 8;
    crate::allocation_audit::assert_no_heap(|| {
        assert!(harness._readers[0].realtime(&mut remaining).1.is_none());
    });
    assert_eq!(remaining, 0, "exactly eight raw records remained after fill");
    let mut empty = 8;
    assert!(harness._readers[0].realtime(&mut empty).1.is_none());
    assert_eq!(empty, 7, "empty queue requires one bounded attempt");
}

#[test]
#[ignore = "Release benchmark, 16 real M1 readers, synthetic Mach/MMCSS cadence; no native devices"]
// An explicitly requested ignored test must reject debug builds rather than
// silently report unoptimized timings as a performance result.
#[allow(clippy::assertions_on_constants)]
fn pull_callback_benchmark() {
    assert!(!cfg!(debug_assertions), "benchmark requires --release");
    let count: usize = std::env::var("OINK_BENCH_SOURCES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    let samples: usize = std::env::var("OINK_BENCH_SAMPLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);
    let only_short = std::env::var_os("OINK_BENCH_SHORT").is_some();
    eprintln!(
        "PULLBENCH,rate,frames,sources,mode,p999_us,max_us,mean_us,period_us,plc,decode_errors,limited,heap,priority"
    );
    for rate in [44_100u32, 48_000, 96_000] {
        for frames in [128usize, 256, 480, 441] {
            if only_short && frames != 128 {
                continue;
            }
            for mode in [0, 1, 2] {
                let mut harness = Harness::new(count);
                let mut output = vec![0.0; frames * 2];
                let mut native_copy = vec![0i16; frames * 2];
                let mut timings = Vec::with_capacity(samples);
                let period = Duration::from_secs_f64(frames as f64 / f64::from(rate));
                let mut sequence = 0;
                let mut next_emit = harness.origin;
                let mut actual_due = Instant::now();
                let priority = Priority::enter(period.as_nanos() as u64).unwrap();
                let mut heap = 0;
                let c_heap = CHeapAudit::load();
                for tick in 0..samples + 200 {
                    let at = harness.origin + Duration::from_secs_f64(tick as f64 * frames as f64 / f64::from(rate));
                    while next_emit <= at {
                        let burst = (128..132).contains(&(sequence % 256));
                        if mode != 1 || !burst {
                            harness.post(sequence, 1, next_emit, mode == 2 && burst);
                        }
                        sequence += 1;
                        next_emit += Duration::from_millis(5);
                    }
                    let began = Instant::now();
                    let mut work = || {
                        crate::allocation_audit::assert_no_heap(|| {
                            harness.sim.fill(&mut output, rate, 2, frames as u32, at, None)
                        })
                    };
                    if let Some(audit) = c_heap {
                        heap += audit.measure(work);
                    } else {
                        work();
                    }
                    // Include representative native integer format conversion/copy.
                    for (dst, sample) in native_copy.iter_mut().zip(&output) {
                        *dst = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
                    }
                    std::hint::black_box(&native_copy);
                    let ns = began.elapsed().as_nanos() as u64;
                    for source in &mut harness.sources {
                        source.poll();
                    }
                    if tick >= 200 {
                        timings.push(ns);
                    }
                    // Sleeping is outside the measured work; a continuously
                    // busy RT thread would be demoted and is not a HAL cadence.
                    actual_due += period;
                    std::thread::sleep(actual_due.saturating_duration_since(Instant::now()));
                }
                drop(priority);
                for source in &mut harness.sources {
                    source.poll();
                }
                let (mut plc, mut errors, mut limited) = (0, 0, 0);
                for source in &harness.sources {
                    let stats = source.stats();
                    plc += stats.lost;
                    errors += stats.decode_errors;
                    limited += stats.work_limited;
                }
                timings.sort_unstable();
                let p999 = timings[(timings.len() * 999 / 1000).min(timings.len() - 1)] as f64 / 1000.0;
                let max = *timings.last().unwrap() as f64 / 1000.0;
                let mean = timings.iter().sum::<u64>() as f64 / timings.len() as f64 / 1000.0;
                eprintln!(
                    "PULLBENCH,{rate},{frames},{count},{mode},{p999:.3},{max:.3},{mean:.3},{:.3},{plc},{errors},{limited},{heap},ok",
                    period.as_secs_f64() * 1e6
                );
                assert_eq!(heap, 0);
                assert_eq!(limited, 0);
                if mode == 0 {
                    assert_eq!(plc, 0, "healthy synthetic clocks must not manufacture PLC");
                }
            }
        }
    }
}

#[test]
fn real_m1_arbitrary_pull_preserves_reserve_with_independent_packet_clocks() {
    for (rate, frames) in [(44_100u32, 128usize), (48_000, 128), (48_000, 480), (96_000, 128)] {
        for sender_ppm in [-700.0, 0.0, 700.0] {
            let mut harness = Harness::new(1);
            let mut output = vec![0.0; frames * 2];
            let packet_seconds = 0.005 / (1.0 + sender_ppm / 1_000_000.0);
            let mut sequence = 0u64;
            let ticks = 30 * rate as usize / frames;
            for tick in 0..ticks {
                let at = harness.origin + Duration::from_secs_f64(tick as f64 * frames as f64 / f64::from(rate));
                loop {
                    let arrived = harness.origin + Duration::from_secs_f64(sequence as f64 * packet_seconds);
                    if arrived > at {
                        break;
                    }
                    harness.post(sequence, 1, arrived, false);
                    sequence += 1;
                }
                harness.sim.fill(&mut output, rate, 2, frames as u32, at, None);
                harness.sources[0].poll();
            }
            let stats = harness.sources[0].stats();
            assert_eq!(
                stats.lost + stats.late + stats.rejected + stats.work_limited,
                0,
                "rate={rate} frames={frames} sender_ppm={sender_ppm}: {stats:?}"
            );
        }
    }
}
