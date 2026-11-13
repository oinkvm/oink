use super::*;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
fn time(frame: usize, now: Instant) -> Time {
    Time {
        device_clock: None,
        host_ns: Some(1_000_000_000 + (frame as u64 * 1_000_000_000 / 48_000)),
        native_frame: Some(frame as f64),
        rate: 48_000,
        quality: TimeQuality::HardwareBoundary,
        discontinuity: false,
        delivered_at: now,
    }
}
#[test]
fn activation_needs_all_local_facts_and_never_adds_demand() {
    for switch in [false, true] {
        for mic in [false, true] {
            for exit in [false, true] {
                for capture in [false, true] {
                    assert_eq!(eligible(switch, mic, exit, capture), switch && mic && exit && capture);
                }
            }
        }
    }
    let control = Arc::new(Control::new().unwrap());
    let mut tap = RenderTap::new(control.clone());
    let now = Instant::now();
    tap.supply(&[0.0; 2], 2, time(0, now));
    control.update(true, true, true, now);
    assert_eq!(control.mode(), 1, "claims alone must not open a mic");
    let processor = Processor::new(Some(control.clone()), now).unwrap();
    control.update(true, true, true, now);
    assert!(processor.enabled());
    control.update(false, true, true, now);
    assert!(!processor.enabled());
    drop(processor);
    control.update(true, true, true, now);
    assert_eq!(control.mode(), 1);
}

#[test]
fn desktop_case_a_input_uses_the_playback_control_and_stays_finite_without_reference() {
    let now = Instant::now();
    let control = Arc::new(Control::new().unwrap());
    let mut tap = RenderTap::new(control.clone());
    tap.supply(&[0.0; BLOCK * 2], 2, time(0, now));
    let mut input = CaseAInput::new(control, true).unwrap();
    let output = input.process_block(&[0.125; BLOCK], 1_000, now, false);
    assert!(output.iter().all(|sample| sample.is_finite()));
}

#[test]
fn reference_is_stereo_and_rejects_unknown_gaps_and_switches() {
    let now = Instant::now();
    let control = Arc::new(Control::new().unwrap());
    let mut worker = WorkerControl::new(control.clone()).unwrap();
    let mut tap = RenderTap::new(control.clone());
    tap.supply(&[0.0; 2], 2, time(0, now));
    control.update(true, true, true, now);
    assert!(worker.apply());
    let stereo: Vec<f32> = (0..1920)
        .flat_map(|n| {
            let v = (n as f32 * 0.07).sin();
            [v, -v]
        })
        .collect();
    tap.supply(&stereo, 2, time(0, now));
    let reference = worker.active.as_mut().unwrap().reference.as_mut().unwrap();
    assert_eq!(reference.ingest(), None);
    let mut block = [[0.0; BLOCK]; 2];
    reference
        .block(time(480, now).host_ns.unwrap(), 1e9 / 48_000.0, &mut block)
        .unwrap();
    assert!(block[0].iter().map(|s| s * s).sum::<f32>() > 100.0);
    assert!(block[0].iter().zip(&block[1]).all(|(a, b)| (a + b).abs() < 1e-6));
    let mut unknown = time(1920, now);
    unknown.quality = TimeQuality::Unknown;
    tap.supply(&[0.0; 256], 2, unknown);
    assert_eq!(reference.ingest(), Some(Reason::TimestampUnknown));
    tap.supply(&[0.0; 256], 2, time(4000, now));
    let _ = reference.ingest();
    drop(tap);
    assert_eq!(control.output(), 0);
    let mut replacement = RenderTap::new(control.clone());
    replacement.supply(&[0.0; 2], 2, time(0, now));
    control.update(true, true, true, now + Duration::from_secs(1));
    assert!(worker.apply());
    assert_eq!(
        worker.active.as_ref().unwrap().reference.as_ref().unwrap().generation,
        control.output()
    );
}
#[test]
fn control_coalescing_and_repeated_capture_retirement_preserve_unique_reference() {
    let now = Instant::now();
    let control = Arc::new(Control::new().unwrap());
    let mut tap = RenderTap::new(control.clone());
    tap.supply(&[0.0; 2], 2, time(0, now));
    for round in 0..10 {
        let mut worker = WorkerControl::new(control.clone()).unwrap();
        assert!(WorkerControl::new(control.clone()).is_err());
        for enabled in [true, false, true, false, true] {
            control.update(enabled, true, true, now + Duration::from_secs(round));
        }
        assert!(worker.apply());
        assert!(worker.active.as_ref().unwrap().reference.is_some());
        drop(worker);
    }
}
#[test]
fn raw_bypass_is_exact_off_and_finite_on_loss() {
    let now = Instant::now();
    let mut processor = Processor::new(None, now).unwrap();
    let raw = std::array::from_fn::<_, 240, _>(|i| (i as f32 * 0.07).sin() * 0.2);
    let mut output = [0.0; 240];
    processor.raw(&raw, &mut output, now);
    assert_eq!(raw, output);
    processor.invalidate(Reason::ReferenceGap, now);
    processor.raw(&raw, &mut output, now);
    assert_eq!(raw, output);
    let summary = processor.summary(now + Duration::from_secs(1)).unwrap();
    assert_eq!(summary.state, State::Off);
    assert_eq!(summary.erle_db, None);
}
#[test]
fn integrated_reference_and_processor_converge_on_echo() {
    let origin = Instant::now();
    let control = Arc::new(Control::new().unwrap());
    let mut tap = RenderTap::new(control.clone());
    let mut processor = Processor::new(Some(control.clone()), origin).unwrap();
    processor.test_work_budget = Some(Duration::MAX);
    tap.supply(&[0.0; 2], 2, time(0, origin));
    control.update(true, true, true, origin);
    let n = 48_000 * 14;
    let mut far = vec![0.0f32; n + BLOCK * 3];
    let mut seed = 17_u32;
    for i in 1..far.len() {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        far[i] = 0.7 * far[i - 1] + 0.08 * ((seed >> 8) as f32 / 8388608.0 - 1.0);
    }
    let (mut raw_power, mut processed_power, mut active) = (0.0f64, 0.0f64, 0);
    let (mut early_raw, mut early_processed, mut bypassed) = (0.0f64, 0.0f64, 0);
    for b in 0..n / BLOCK {
        let now = origin + Duration::from_millis(b as u64 * 10);
        let stereo: [f32; BLOCK * 2] =
            std::array::from_fn(|i| far[b * BLOCK + i / 2] * if i % 2 == 0 { 1.0 } else { -0.4 });
        tap.supply(&stereo, 2, time(b * BLOCK, now));
        control.update(true, true, true, now);
        processor.prepare(now);
        if b < 3 {
            continue;
        }
        let frame = (b - 2) * BLOCK;
        let mic: [f32; BLOCK] = std::array::from_fn(|i| {
            if frame + i >= 960 {
                0.5 * far[frame + i - 960]
            } else {
                0.0
            }
        });
        let mut out = [0.0; BLOCK];
        processor.process(&mic, time(frame, now), 1e9 / 48_000.0, &mut out, now);
        if b < 103 {
            early_raw += mic.iter().map(|x| f64::from(x * x)).sum::<f64>();
            early_processed += out.iter().map(|x| f64::from(x * x)).sum::<f64>();
        }
        if let Some(s) = processor.summary(now) {
            if s.state == State::Active {
                active += 1;
            }
            bypassed += s.bypass_blocks;
            eprintln!(
                "echo state {:?} reason {:?} erle {:?} cpu {}us work_p99_bucket {}us work_max {}us cpu_max {}us deadline {}",
                s.state,
                s.reason,
                s.erle_db,
                s.cpu_us,
                s.block_work_p99_us,
                s.block_work_max_us,
                s.block_cpu_max_us,
                s.deadline_misses
            );
        }
        if b > 800 {
            raw_power += mic.iter().map(|x| f64::from(x * x)).sum::<f64>();
            processed_power += out.iter().map(|x| f64::from(x * x)).sum::<f64>();
        }
    }
    let attenuation = 10.0 * (raw_power / processed_power).log10();
    let early = 10.0 * (early_raw / early_processed).log10();
    eprintln!(
        "integrated echo attenuation={attenuation:.2} dB first_second={early:.2} dB bypassed={bypassed} active_windows={active}"
    );
    assert!(active >= 4);
    assert!(attenuation > 10.0, "attenuation={attenuation}");
    // AEC3's output is sent from the first processed block, not after the
    // linear filter shows ERLE: no convergence period passes raw echo.
    assert!(bypassed < 10, "bypassed={bypassed}");
    assert!(early > 3.0, "first-second attenuation={early}");
    // Speaker route/device replacement and capture restart cannot inherit an
    // old filter's Active state. A stale output teardown cannot clear the new one.
    let mut replacement = RenderTap::new(control.clone());
    let now = origin + Duration::from_secs(15);
    replacement.supply(&[0.0; 2], 2, time(0, now));
    drop(tap);
    control.update(true, true, true, now);
    processor.prepare(now);
    let summary = processor.summary(now).unwrap();
    assert_eq!(summary.state, State::Converging);
    assert_eq!(summary.erle_db, None);
    assert!(processor.enabled());
}
#[test]
fn callback_and_worker_work_do_not_allocate_or_free_including_first_and_reset_blocks() {
    let origin = Instant::now();
    let control = Arc::new(Control::new().unwrap());
    let mut tap = RenderTap::new(control.clone());
    let mut processor = Processor::new(Some(control.clone()), origin).unwrap();
    tap.supply(&[0.0; 2], 2, time(0, origin));
    control.update(true, true, true, origin);
    let mut out = [0.0; BLOCK];
    for block in 0..1600 {
        let now = origin + Duration::from_millis(block as u64 * 10);
        if block % 200 == 100 {
            processor.invalidate(Reason::CaptureGap, now);
        }
        control.update(block % 400 != 300, true, true, now);
        crate::allocation_audit::assert_no_heap(|| {
            let samples: [f32; BLOCK * 2] = std::array::from_fn(|i| {
                if block < 100 {
                    0.0
                } else {
                    (i as f32 * 0.08).sin() * 0.1
                }
            });
            tap.supply(&samples, 2, time(block * BLOCK, now));
            processor.prepare(now);
            if block >= 3 {
                processor.process(
                    &[0.0; BLOCK],
                    time((block - 2) * BLOCK, now),
                    1e9 / 48_000.0,
                    &mut out,
                    now,
                );
            }
            let _ = processor.summary(now);
        });
    }
}
#[test]
fn old_output_retirement_cannot_clear_a_replacement() {
    let control = Arc::new(Control::new().unwrap());
    let old = RenderTap::new(control.clone());
    let replacement = RenderTap::new(control.clone());
    control.update(true, true, true, Instant::now());
    let epoch = control.output();
    assert_ne!(epoch, 0);
    drop(old);
    assert_eq!(control.output(), epoch);
    drop(replacement);
    assert_eq!(control.output(), 0);
}
#[test]
fn missing_reference_bypasses_the_block_with_raw_audio_and_keeps_a_clean_engine() {
    let origin = Instant::now();
    let control = Arc::new(Control::new().unwrap());
    let mut tap = RenderTap::new(control.clone());
    let mut processor = Processor::new(Some(control.clone()), origin).unwrap();
    tap.supply(&[0.0; 2], 2, time(0, origin));
    control.update(true, true, true, origin);
    processor.prepare(origin);
    let mut out = [0.0; BLOCK];
    // No reference yet: the block is still produced (never silence) from the
    // delay-matched raw path, and nothing asks for another engine.
    processor.process(&[0.1; BLOCK], time(0, origin), 1e9 / 48_000.0, &mut out, origin);
    assert!(out.iter().all(|s| s.is_finite()));
    // Once the 430-frame raw delay is established the speech passes unchanged.
    processor.process(&[0.1; BLOCK], time(BLOCK, origin), 1e9 / 48_000.0, &mut out, origin);
    assert!(
        out.iter().all(|s| (*s - 0.1).abs() < 1e-6),
        "a late reference must not silence speech"
    );
    assert!(!control.reset_pending());
    let mut unknown = time(2 * BLOCK, origin);
    unknown.quality = TimeQuality::Unknown;
    processor.process(&[0.1; BLOCK], unknown, 1e9 / 48_000.0, &mut out, origin);
    assert!(out.iter().all(|s| (*s - 0.1).abs() < 1e-6));
    assert!(!control.reset_pending(), "an engine that never adapted is not rebuilt");
    let s = processor.summary(origin + Duration::from_secs(1)).unwrap();
    assert_eq!(s.state, State::Degraded);
    assert_eq!(s.reason, Reason::TimestampUnknown);
    assert_eq!(s.bypass_blocks, 3);
    assert_eq!(s.erle_db, None);
    assert_eq!(control.engines_built(), 1);
}

#[allow(dead_code)]
#[path = "../../native/aec3/examples/offline.rs"]
mod engine_harness;
#[test]
#[ignore = "requires explicit OINK_AEC_RIR path; no audio devices are opened"]
fn recorded_room_end_to_end_activation_and_near_end_preservation() {
    let path = std::env::var_os("OINK_AEC_RIR").expect("OINK_AEC_RIR recorded mono 48k float32 WAV");
    let rir = engine_harness::read_rir(std::path::Path::new(&path));
    let n = 48_000 * 24;
    let (mut far, mut voice) = (vec![0.0; n + 1440], vec![0.0; n + 1440]);
    let mut seed = 17_u32;
    for i in 1..far.len() {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        far[i] = far[i - 1] * 0.7 + 0.08 * ((seed >> 8) as f32 / 8388608.0 - 1.0);
        let t = i as f32 / 48_000.0;
        voice[i] = if i > 48_000 * 12 {
            (t * 4.1).sin().max(0.0).powi(2)
                * ((t * std::f32::consts::TAU * 173.0).sin() * 0.12 + (t * std::f32::consts::TAU * 346.0).sin() * 0.06)
        } else {
            0.0
        };
    }
    let echo = engine_harness::convolve(&far, &rir);
    let mut result = vec![0.0; n];
    let origin = Instant::now();
    let control = Arc::new(Control::new().unwrap());
    let mut tap = RenderTap::new(control.clone());
    let mut processor = Processor::new(Some(control.clone()), origin).unwrap();
    processor.test_work_budget = Some(Duration::MAX);
    tap.supply(&[0.0; 2], 2, time(0, origin));
    control.update(true, true, true, origin);
    let (mut raw_energy, mut out_energy, mut first_active) = (0.0f64, 0.0f64, None);
    for b in 0..n / BLOCK {
        let now = origin + Duration::from_millis(b as u64 * 10);
        let stereo: [f32; BLOCK * 2] = std::array::from_fn(|i| if i % 2 == 0 { far[b * BLOCK + i / 2] } else { 0.0 });
        tap.supply(&stereo, 2, time(b * BLOCK, now));
        control.update(true, true, true, now);
        processor.prepare(now);
        if b < 3 {
            continue;
        }
        let frame = (b - 2) * BLOCK;
        let mic: [f32; BLOCK] = std::array::from_fn(|i| echo[frame + i - 480] + voice[frame + i]);
        let mut out = [0.0; BLOCK];
        processor.process(&mic, time(frame, now), 1e9 / 48_000.0, &mut out, now);
        result[frame..frame + BLOCK].copy_from_slice(&out);
        if let Some(s) = processor.summary(now) {
            if s.state == State::Active && first_active.is_none() {
                first_active = Some(b);
            }
            eprintln!(
                "recorded full path {:?} {:?} ERLE {:?} deadline {}",
                s.state, s.reason, s.erle_db, s.deadline_misses
            );
        }
        if (800..1100).contains(&b) {
            raw_energy += mic.iter().map(|s| f64::from(s * s)).sum::<f64>();
            out_energy += out.iter().map(|s| f64::from(s * s)).sum::<f64>();
        }
    }
    let attenuation = 10.0 * (raw_energy / out_energy).log10();
    let (mut xy, mut xx, mut yy) = (0.0, 0.0, 0.0);
    for i in 48_000 * 18..48_000 * 23 {
        let x = f64::from(voice[i]);
        let y = f64::from(result[i + 435]);
        xy += x * y;
        xx += x * x;
        yy += y * y;
    }
    let corr = xy / (xx * yy).sqrt();
    let gain = 20.0 * (xy / xx).log10();
    eprintln!(
        "recorded full path first_active={first_active:?} attenuation={attenuation:.2}dB near_corr={corr:.4} near_gain={gain:.2}dB"
    );
    assert!(first_active.is_some_and(|b| b < 800));
    assert!(attenuation > 10.0);
    assert!(corr > 0.85);
    assert!((-4.0..1.0).contains(&gain));
}
#[test]
fn reference_asrc_tracks_independent_clocks_without_downmixing() {
    let origin = Instant::now();
    for (render_ppm, capture_ppm) in [
        (-1000.0, 1000.0),
        (-700.0, 200.0),
        (-50.0, 0.0),
        (50.0, -200.0),
        (700.0, -50.0),
        (1000.0, -1000.0),
    ] {
        let control = Arc::new(Control::new().unwrap());
        let mut worker = WorkerControl::new(control.clone()).unwrap();
        let mut tap = RenderTap::new(control.clone());
        tap.supply(&[0.0; 2], 2, time(0, origin));
        control.update(true, true, true, origin);
        worker.apply();
        let reference = worker.active.as_mut().unwrap().reference.as_mut().unwrap();
        let render_step = 1e9 / (48_000.0 * (1.0 + render_ppm / 1e6));
        let capture_step = 1e9 / (48_000.0 * (1.0 + capture_ppm / 1e6));
        let mut native = 0;
        let (mut error, mut count, mut max_alignment) = (0.0f64, 0, 0.0f64);
        for block in 1..4000 {
            let host = 1e9 + block as f64 * 480.0 * capture_step;
            while 1e9 + (native as f64) * render_step < host + 25e6 {
                let samples: [f32; 256] = std::array::from_fn(|i| {
                    let value =
                        ((native + i / 2) as f64 * render_step / 1e9 * std::f64::consts::TAU * 500.0).sin() as f32;
                    if i % 2 == 0 { value } else { -value }
                });
                let mut stamp = time(native, origin);
                stamp.host_ns = Some((1e9 + native as f64 * render_step) as u64);
                tap.supply(&samples, 2, stamp);
                assert_eq!(reference.ingest(), None);
                native += 128;
            }
            let mut out = [[0.0; BLOCK]; 2];
            reference.block(host as u64,capture_step,&mut out).unwrap_or_else(|reason|panic!("block={block} render={render_ppm} capture={capture_ppm} native={native} alignment={} ratio={} reason={reason:?}",reference.alignment_error_frames,reference.ratio()));
            if block > 3500 {
                for (i, (left, right)) in out[0].iter().zip(&out[1]).enumerate() {
                    let expected =
                        (((host - 1e9) + i as f64 * capture_step) / 1e9 * std::f64::consts::TAU * 500.0).sin();
                    error += (f64::from(*left) - expected).powi(2);
                    assert!((left + right).abs() < 1e-5);
                    count += 1;
                }
                max_alignment = max_alignment.max(reference.alignment_error_frames.abs());
            }
        }
        let rms = (error / f64::from(count)).sqrt();
        eprintln!(
            "ASRC render={render_ppm}ppm capture={capture_ppm}ppm rms={rms:.5} alignment={max_alignment:.4}frames ratio={:.8}",
            reference.ratio()
        );
        assert!(rms < 0.02);
        assert!(max_alignment < 1.0);
        assert!((reference.clock.ppm() - render_ppm).abs() < 0.2);
    }
}

#[test]
fn no_coupling_stays_engaged_with_one_engine_and_a_reference_gap_degrades() {
    // Headphones: the reference plays, the microphone hears none of it. AEC3
    // handles this itself (its suppressor finds no echo); there is no
    // separate no-coupling bypass that would stop processing while the
    // speaker plays, and no engine is rebuilt for it.
    let origin = Instant::now();
    let control = Arc::new(Control::new().unwrap());
    let mut tap = RenderTap::new(control.clone());
    let mut processor = Processor::new(Some(control.clone()), origin).unwrap();
    processor.test_work_budget = Some(Duration::MAX);
    tap.supply(&[0.0; 2], 2, time(0, origin));
    control.update(true, true, true, origin);
    let mut seed = 57_u32;
    let mut summaries = Vec::new();
    for b in 0..1400 {
        let now = origin + Duration::from_millis(b as u64 * 10);
        let stereo: [f32; BLOCK * 2] = std::array::from_fn(|_| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            if b < 200 {
                0.0
            } else {
                ((seed >> 8) as f32 / 8388608.0 - 1.0) * 0.1
            }
        });
        tap.supply(&stereo, 2, time(b * BLOCK, now));
        control.update(true, true, true, now);
        processor.prepare(now);
        if b >= 3 {
            let mut out = [0.0; BLOCK];
            processor.process(&[0.0; BLOCK], time((b - 2) * BLOCK, now), 1e9 / 48_000.0, &mut out, now);
            // A silent microphone stays silent (AEC3 comfort noise is at
            // its -96 dBFS floor at most).
            assert!(out.iter().all(|s| s.is_finite() && s.abs() < 1e-3));
        }
        if let Some(s) = processor.summary(now) {
            summaries.push((b, s));
        }
    }
    for (b, s) in &summaries {
        assert_eq!(s.erle_db, None, "no qualified ERLE without an echo");
        if *b > 300 {
            assert!(s.state.engaged(), "block {b}: {:?} {:?}", s.state, s.reason);
            assert_eq!(s.bypass_blocks, 0, "block {b}");
        }
    }
    assert_eq!(control.engines_built(), 1);
    assert!(!control.reset_pending());
    let now = origin + Duration::from_secs(45);
    control.update(true, true, true, now);
    processor.prepare(now);
    // Device frames can restart after sleep. Explicit discontinuity invalidates
    // the reference epoch; resumed silence is real data, never a filled gap.
    let mut resumed = time(0, now);
    resumed.host_ns = Some(46_000_000_000);
    resumed.discontinuity = true;
    tap.supply(&[0.0; BLOCK * 2], 2, resumed);
    processor.prepare(now);
    let mut out = [0.0; BLOCK];
    processor.process(&[0.0; BLOCK], resumed, 1e9 / 48_000.0, &mut out, now);
    assert!(out.iter().all(|s| s.is_finite()));
    assert_eq!(
        processor.summary(now + Duration::from_secs(1)).unwrap().state,
        State::Degraded
    );
}

/// A Case A processor whose microphone hears `0.5 * far` 20 ms after the
/// reference (`far`) is rendered.
struct EchoFixture {
    origin: Instant,
    control: Arc<Control>,
    tap: RenderTap,
    processor: Processor,
    far: Vec<f32>,
    summaries: Vec<Summary>,
}
impl EchoFixture {
    /// `blocks` blocks of far end; `silent` blocks render silence instead.
    fn new(blocks: usize, silent: std::ops::Range<usize>, seed: u32) -> Self {
        let origin = Instant::now();
        let control = Arc::new(Control::new().unwrap());
        let mut tap = RenderTap::new(control.clone());
        let mut processor = Processor::new(Some(control.clone()), origin).unwrap();
        processor.test_work_budget = Some(Duration::MAX);
        tap.supply(&[0.0; 2], 2, time(0, origin));
        control.update(true, true, true, origin);
        let mut far = vec![0.0f32; (blocks + 3) * BLOCK];
        let mut seed = seed;
        for i in 1..far.len() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            far[i] = if silent.contains(&(i / BLOCK)) {
                0.0
            } else {
                0.7 * far[i - 1] + 0.08 * ((seed >> 8) as f32 / 8388608.0 - 1.0)
            };
        }
        Self {
            origin,
            control,
            tap,
            processor,
            far,
            summaries: Vec::new(),
        }
    }
    /// Render `blocks`; the microphone blocks in `lost` never reach the
    /// processor (refused by a full handoff), and the next one arrives marked
    /// discontinuous at its true capture position. Returns (microphone,
    /// output) energy of each processed block.
    fn run(&mut self, blocks: std::ops::Range<usize>, lost: std::ops::Range<usize>) -> Vec<(f64, f64)> {
        let mut energies = Vec::new();
        for b in blocks {
            let now = self.origin + Duration::from_millis(b as u64 * 10);
            let stereo: [f32; BLOCK * 2] = std::array::from_fn(|i| self.far[b * BLOCK + i / 2]);
            self.tap.supply(&stereo, 2, time(b * BLOCK, now));
            self.control.update(true, true, true, now);
            self.processor.prepare(now);
            if b < 3 || lost.contains(&b) {
                continue;
            }
            let frame = (b - 2) * BLOCK;
            let mic: [f32; BLOCK] = std::array::from_fn(|i| {
                if frame + i >= 960 {
                    0.5 * self.far[frame + i - 960]
                } else {
                    0.0
                }
            });
            let mut stamp = time(frame, now);
            stamp.discontinuity = !lost.is_empty() && b == lost.end;
            let mut out = [0.0; BLOCK];
            self.processor.process(&mic, stamp, 1e9 / 48_000.0, &mut out, now);
            energies.push((
                mic.iter().map(|x| f64::from(x * x)).sum::<f64>(),
                out.iter().map(|x| f64::from(x * x)).sum::<f64>(),
            ));
            if let Some(s) = self.processor.summary(now) {
                self.summaries.push(s);
            }
        }
        energies
    }
}
fn attenuation(energies: &[(f64, f64)]) -> f64 {
    let (mic, out) = energies
        .iter()
        .fold((0.0, 0.0), |(m, o), (mic, out)| (m + mic, o + out));
    10.0 * (mic / out).log10()
}

/// The exit keeps rendering (silence) through source silence now, so the
/// reference stays continuous and silent for long stretches. Silence is a
/// valid reference: the engine and its adapted filter stay, and when sound
/// returns the echo is cancelled at once, without re-convergence.
#[test]
fn a_silent_reference_keeps_the_engine_and_its_adapted_filter() {
    // 6 s of program, 8 s of silence, 4 s of program.
    let mut fixture = EchoFixture::new(1800, 600..1400, 23);
    let before = fixture.run(0..600, 0..0);
    let silent_from = fixture.summaries.len();
    let _ = fixture.run(600..1400, 0..0);
    let silent_to = fixture.summaries.len();
    let after = fixture.run(1400..1800, 0..0);
    for s in &fixture.summaries[silent_from + 1..silent_to] {
        assert!(s.state.engaged(), "{:?} {:?}", s.state, s.reason);
        assert_eq!(s.bypass_blocks, 0);
    }
    assert!(
        fixture.summaries[silent_from..]
            .iter()
            .any(|s| s.state == State::Active)
    );
    let before = attenuation(&before[before.len() - 200..]);
    // The first second after the silence (its echo needs 20 ms to arrive).
    let resumed = attenuation(&after[2..100]);
    eprintln!("silent reference: before {before:.1} dB, first second after silence {resumed:.1} dB");
    assert!(before > 10.0, "before={before}");
    assert!(resumed > 10.0, "after silence={resumed}");
    assert_eq!(fixture.control.engines_built(), 1, "silence never rebuilds the engine");
    assert!(!fixture.control.reset_pending());
}

/// Refused/lost capture frames are a capture discontinuity, not a new echo
/// path: the read position re-anchors to the next block, the adapted engine
/// stays (only a restarted stream gets a new one), and cancellation goes on.
#[test]
fn a_capture_gap_keeps_the_adapted_engine() {
    let mut fixture = EchoFixture::new(1200, 0..0, 31);
    let _ = fixture.run(0..800, 0..0);
    // Blocks 800 and 801 (20 ms) are refused; the speaker keeps playing.
    let after = fixture.run(800..1200, 800..802);
    let gaps: u64 = fixture.summaries.iter().map(|s| s.capture_gaps).sum();
    let resumed = attenuation(&after[..100]);
    eprintln!("capture gap: first second after it {resumed:.1} dB");
    assert_eq!(gaps, 1);
    assert!(resumed > 10.0, "after gap={resumed}");
    assert_eq!(
        fixture.control.engines_built(),
        1,
        "a capture gap does not rebuild the engine"
    );
    assert!(!fixture.control.reset_pending());
    assert!(fixture.summaries.last().unwrap().state.engaged());
}

#[test]
fn work_budget_exhaustion_retires_filter_and_returns_original_audio() {
    let origin = Instant::now();
    let control = Arc::new(Control::new().unwrap());
    let mut tap = RenderTap::new(control.clone());
    let mut processor = Processor::new(Some(control.clone()), origin).unwrap();
    tap.supply(&[0.0; 2], 2, time(0, origin));
    control.update(true, true, true, origin);
    processor.prepare(origin);
    processor.test_work_budget = Some(Duration::ZERO);
    let mut out = [0.0; BLOCK];
    // Fully present reference avoids waiting. Constant raw input allows checking
    // both the latency ramp and the unchanged tail of the bypass waveform.
    tap.supply(&[0.0; BLOCK * 8], 2, time(0, origin));
    processor.prepare(origin);
    processor.process(&[0.1; BLOCK], time(480, origin), 1e9 / 48_000.0, &mut out, origin);
    let s = processor.summary(origin + Duration::from_secs(1)).unwrap();
    assert_eq!(s.state, State::Degraded);
    assert_eq!(s.reason, Reason::Overload);
    assert_eq!(s.deadline_misses, 1);
    assert_eq!(s.bypass_blocks, 1);
    assert!(out[430..].iter().all(|x| (*x - 0.1).abs() < 1e-6));
    // The engine adapted on this block, so its overload retires it once.
    assert!(control.reset_pending());
}

/// Device-free cost of the extra system reference path: the capture thread's
/// copy into the handoff (`SystemFeed::packet`) and the DSP owner's ingest and
/// 48 kHz conversion of the second stream. Prints thread CPU per audio second.
#[test]
fn system_reference_cost_is_measured() {
    let _ = estimated_host_ns(Instant::now());
    let base = Instant::now() + Duration::from_secs(1);
    let control = Arc::new(Control::new().unwrap());
    let mut worker = WorkerControl::new(control.clone()).unwrap();
    let mut feed = SystemFeed::new(control.clone(), 48_000, true, Duration::from_millis(50), 0);
    control.update(true, true, true, base);
    assert!(worker.apply());
    assert!(feed.selected());
    let mut seed = 5_u32;
    let packet: Vec<f32> = (0..960)
        .map(|_| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / 8388608.0 - 1.0
        })
        .collect();
    let seconds = 20;
    let (mut produce_ns, mut consume_ns) = (0u64, 0u64);
    for n in 0..seconds * 100 {
        let stamp = base + Duration::from_millis(10 * n as u64);
        let cpu = processor::cpu_ns();
        feed.packet(SystemPacket {
            samples: &packet,
            stamp: Some(stamp),
            position: Some(480 * n as u64),
            discontinuity: false,
            arrived: stamp + Duration::from_millis(11),
        });
        let middle = processor::cpu_ns();
        let reference = worker.active.as_mut().unwrap().reference.as_mut().unwrap();
        assert_eq!(reference.ingest(), None);
        consume_ns += processor::cpu_ns() - middle;
        produce_ns += middle - cpu;
    }
    // The capture-thread copy, the idle silence fill and the owner-side ingest
    // stay free of heap work, including the gap fill when delivery resumes.
    for n in seconds * 100..seconds * 100 + 300 {
        let stamp = base + Duration::from_millis(10 * n as u64);
        crate::allocation_audit::assert_no_heap(|| {
            if (n / 50) % 2 == 1 {
                feed.idle(stamp + Duration::from_millis(60));
            } else {
                feed.packet(SystemPacket {
                    samples: &packet,
                    stamp: Some(stamp),
                    position: Some(480 * n as u64),
                    discontinuity: false,
                    arrived: stamp + Duration::from_millis(11),
                });
            }
            let reference = worker.active.as_mut().unwrap().reference.as_mut().unwrap();
            let _ = reference.ingest();
        });
    }
    assert_eq!(feed.overlaps, 0);
    let bytes = control.system_bytes.load(std::sync::atomic::Ordering::Relaxed);
    let per_second = |ns: u64| ns as f64 / 1000.0 / seconds as f64;
    eprintln!(
        "system reference cost: capture thread {:.1} us/s ({:.3}% core), DSP ingest {:.1} us/s ({:.3}% core), fixed bytes {}",
        per_second(produce_ns),
        per_second(produce_ns) / 10_000.0,
        per_second(consume_ns),
        per_second(consume_ns) / 10_000.0,
        bytes
    );
    assert!(bytes > 0 && bytes < 2 * 1024 * 1024, "{bytes}");
}
