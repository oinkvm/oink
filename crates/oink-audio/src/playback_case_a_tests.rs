//! Device-free Case A product path on the desktop: the playback renderer with
//! its own `RenderTap` stamps, the capture-callback block assembler with its
//! own stamps, and the audio owner's maintenance passes. The microphone hears
//! an acoustic echo of exactly what the renderer produced.
use super::*;
use crate::{microphone::CaptureBlocks, sink::quality::OutputKind};

struct FarEnd {
    signal: Arc<Vec<f32>>,
    cursor: usize,
}
impl PullInput for FarEnd {
    fn begin(&mut self, _: usize, _: Instant) {}
    fn pull(&mut self, out: &mut [f32], _: usize, _: Instant) -> PullBlock {
        for frame in out.as_chunks_mut::<2>().0 {
            let value = self.signal.get(self.cursor).copied().unwrap_or(0.0);
            *frame = [value, -0.4 * value];
            self.cursor += 1;
        }
        let mut sources = [0; MIX_SOURCES];
        sources[0] = 7;
        PullBlock {
            sources,
            kind: OutputKind::Audio,
        }
    }
    fn end(&mut self) {}
    fn gap(&mut self, _: OutputGap) {}
    fn drained(&self) -> bool {
        false
    }
}

fn random(seed: &mut u32) -> f32 {
    *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    (*seed >> 8) as f32 / 8388608.0 - 1.0
}

const RATE: usize = 48_000;
const RENDER_PERIOD: usize = 480;
const CAPTURE_PERIOD: usize = 512;
const OUTPUT_LATENCY: Duration = Duration::from_millis(12);
const INPUT_LATENCY: Duration = Duration::from_millis(6);
const ACOUSTIC_FRAMES: usize = 96;

fn at_frames(base: Instant, frames: usize) -> Instant {
    base + Duration::from_nanos((frames as u64) * 1_000_000_000 / RATE as u64)
}

#[test]
fn desktop_case_a_cancels_the_echo_of_rendered_output_with_one_engine() {
    // The estimated host epoch must precede every fixture instant.
    let _ = echo::estimated_host_ns(Instant::now());
    let base = Instant::now() + Duration::from_secs(1);
    let seconds = 14;
    let captured_frames = RATE * seconds;
    let mut seed = 17_u32;
    let mut far = vec![0.0f32; captured_frames + RATE];
    for i in 1..far.len() {
        far[i] = 0.7 * far[i - 1] + 0.08 * random(&mut seed);
    }
    let control = Arc::new(echo::Control::new().unwrap());
    let mut render = Render::new(
        Box::new(FarEnd {
            signal: Arc::new(far),
            cursor: 0,
        }),
        RenderOptions {
            metrics: Arc::new(CallbackMetrics::default()),
            started: base,
            callback_ms: Arc::new(AtomicU64::new(0)),
            traces: None,
            trace_dropped: Arc::new(AtomicU64::new(0)),
            underrun_frames: Arc::new(AtomicU64::new(0)),
            render_tap: Some(echo::RenderTap::new(control.clone())),
        },
    );
    let mut input = echo::CaseAInput::new(control.clone(), true).unwrap();
    input.set_test_work_budget(Duration::MAX);
    let owner_control = input.control();
    let mut blocks = CaptureBlocks::new(RATE as u32, Some(input));

    // What the speaker actually played (left channel), by output frame.
    let mut played = Vec::<f32>::new();
    let mut mic = Vec::<f32>::new();
    let mut processed = Vec::<f32>::new();
    let mut output = [0.0f32; RENDER_PERIOD * 2];
    let (mut rendered, mut capture_callbacks, mut owner_passes) = (0usize, 0usize, 0usize);
    let mut jitter = 91_u32;
    while capture_callbacks * CAPTURE_PERIOD < captured_frames {
        let render_at = at_frames(base, rendered * RENDER_PERIOD);
        let capture_at = at_frames(base, (capture_callbacks + 1) * CAPTURE_PERIOD)
            + INPUT_LATENCY
            + Duration::from_micros((random(&mut jitter).abs() * 2000.0) as u64);
        let owner_at = base + Duration::from_millis(20 * owner_passes as u64);
        if owner_at <= render_at && owner_at <= capture_at {
            // The audio owner's 20 ms maintenance pass (microphone runtime tick).
            owner_control.update(true, true, true, owner_at);
            owner_passes += 1;
        } else if render_at <= capture_at {
            let presented = render_at + OUTPUT_LATENCY;
            let skew = Duration::from_micros((random(&mut jitter).abs() * 300.0) as u64);
            render.fill(
                &mut output,
                NativeOutputTiming {
                    period_frames: RENDER_PERIOD as u32,
                    native_gap_frames: None,
                    callback_at: render_at,
                    playback_at: Some(presented + skew),
                    sample_rate: RATE as u32,
                    channels: 2,
                    padding_frames: 0,
                    buffer_frames: RENDER_PERIOD as u32,
                },
            );
            played.extend(output.as_chunks::<2>().0.iter().map(|frame| frame[0]));
            rendered += 1;
        } else {
            // Speaker frame f sounds at base + OUTPUT_LATENCY + f; the mic
            // sample c is taken at base + c.
            let delay = OUTPUT_LATENCY.as_nanos() as usize * RATE / 1_000_000_000 + ACOUSTIC_FRAMES;
            let first = capture_callbacks * CAPTURE_PERIOD;
            let samples: Vec<f32> = (first..first + CAPTURE_PERIOD)
                .map(|c| {
                    c.checked_sub(delay)
                        .and_then(|f| played.get(f))
                        .map_or(0.0, |s| 0.5 * s)
                })
                .collect();
            mic.extend_from_slice(&samples);
            blocks.callback(CAPTURE_PERIOD, samples.iter().copied(), capture_at, &mut |pcm, _| {
                processed.extend(pcm.as_chunks::<2>().0.iter().map(|frame| frame[0]))
            });
            capture_callbacks += 1;
        }
    }
    let tail = RATE * 4;
    let energy = |samples: &[f32]| samples.iter().map(|s| f64::from(s * s)).sum::<f64>();
    let raw = energy(&mic[mic.len() - tail..]);
    let out = energy(&processed[processed.len() - tail..]);
    assert!(raw > 1.0, "fixture must contain echo: {raw}");
    let attenuation = 10.0 * (raw / out).log10();
    eprintln!(
        "desktop product path attenuation={attenuation:.2} dB engines={} owner_passes={owner_passes}",
        control.engines_built()
    );
    assert!(attenuation > 10.0, "attenuation={attenuation}");
    assert_eq!(
        control.engines_built(),
        1,
        "an unchanged configuration builds one engine"
    );
    assert!(!control.reset_pending());
}

#[test]
fn desktop_capture_callback_never_builds_an_engine() {
    let _ = echo::estimated_host_ns(Instant::now());
    let base = Instant::now() + Duration::from_secs(1);
    let control = Arc::new(echo::Control::new().unwrap());
    let mut tap = echo::RenderTap::new(control.clone());
    tap.supply(
        &[0.0; 2],
        2,
        echo::Time {
            host_ns: Some(1),
            device_clock: None,
            native_frame: Some(0.0),
            rate: 48_000,
            quality: echo::TimeQuality::Estimated,
            discontinuity: false,
            delivered_at: base,
        },
    );
    let mut blocks = CaptureBlocks::new(48_000, Some(echo::CaseAInput::new(control.clone(), true).unwrap()));
    for callback in 0..200 {
        let at = at_frames(base, (callback + 1) * CAPTURE_PERIOD);
        blocks.callback(
            CAPTURE_PERIOD,
            std::iter::repeat_n(0.1, CAPTURE_PERIOD),
            at,
            &mut |_, _| {},
        );
    }
    assert_eq!(control.engines_built(), 0, "only the owner's update creates engines");
    control.update(true, true, true, base);
    assert_eq!(control.engines_built(), 1);
}

/// What the full-system reference capture does in a scenario.
#[derive(Clone, Copy, PartialEq)]
enum System {
    /// The platform cannot provide it (for example no permission).
    Unavailable,
    /// Loopback of the device mix, from start to end.
    Running,
    /// The capture fails at `at` seconds and is not reopened.
    FailsAt(usize),
    /// The output changes at `at` seconds; a new capture (new device, own
    /// native position line) opens one second later.
    OutputChangesAt(usize),
}
struct Scenario {
    system: System,
    /// Another local program plays through the same output (never via Oink).
    other: bool,
    /// Seconds during which nothing plays; the loopback endpoint delivers no
    /// packets then (WASAPI behaviour for a silent endpoint).
    pause: Option<(usize, usize)>,
    /// Extra loopback delivery delay beyond the normal one period.
    late: Duration,
    seconds: usize,
}
struct Outcome {
    attenuation: f64,
    engines: u64,
    control: Arc<echo::Control>,
    last: Option<echo::Summary>,
    silence_frames: u64,
    late_blocks: u64,
    /// A per-second summary reported the idle-endpoint reason.
    idle_seen: bool,
}

/// The device-free desktop product path with a loopback-like system
/// reference: the renderer and its `RenderTap`, the device mixing Oink with
/// another local program, a polled loopback reader feeding `SystemFeed`
/// (stamped at the engine mix time, delivered one period later), the capture
/// callback block assembler, and the owner's 20 ms maintenance passes.
fn run_scenario(scenario: Scenario) -> Outcome {
    const LOOPBACK_STAMP: Duration = Duration::from_millis(2);
    const LOOPBACK_DELIVERY: Duration = Duration::from_millis(10);
    const POLL: Duration = Duration::from_millis(2);
    let _ = echo::estimated_host_ns(Instant::now());
    let base = Instant::now() + Duration::from_secs(1);
    let captured_frames = RATE * scenario.seconds;
    let paused = |frame: usize| {
        scenario
            .pause
            .is_some_and(|(from, to)| (from * RATE..to * RATE).contains(&frame))
    };
    let (mut seed, mut other_seed) = (17_u32, 29_u32);
    let mut far = vec![0.0f32; captured_frames + RATE];
    let mut other = vec![0.0f32; captured_frames + RATE];
    for i in 1..far.len() {
        far[i] = 0.7 * far[i - 1] + 0.08 * random(&mut seed);
        other[i] = 0.5 * other[i - 1] + 0.16 * random(&mut other_seed);
    }
    for i in 0..far.len() {
        if paused(i) {
            far[i] = 0.0;
        }
        if paused(i) || !scenario.other {
            other[i] = 0.0;
        }
    }
    let control = Arc::new(echo::Control::new().unwrap());
    let mut render = Render::new(
        Box::new(FarEnd {
            signal: Arc::new(far),
            cursor: 0,
        }),
        RenderOptions {
            metrics: Arc::new(CallbackMetrics::default()),
            started: base,
            callback_ms: Arc::new(AtomicU64::new(0)),
            traces: None,
            trace_dropped: Arc::new(AtomicU64::new(0)),
            underrun_frames: Arc::new(AtomicU64::new(0)),
            render_tap: Some(echo::RenderTap::new(control.clone())),
        },
    );
    let open = |control: &Arc<echo::Control>| {
        echo::SystemFeed::new(
            control.clone(),
            RATE as u32,
            true,
            LOOPBACK_DELIVERY * 2 + scenario.late + Duration::from_millis(10),
            0,
        )
    };
    let mut feed = match scenario.system {
        System::Unavailable => {
            echo::SystemFeed::unavailable(
                &control,
                echo::SystemStatus::PermissionDenied,
                "System Audio Recording not granted (fixture)".into(),
            );
            None
        }
        _ => Some(open(&control)),
    };
    let mut input = echo::CaseAInput::new(control.clone(), true).unwrap();
    input.set_test_work_budget(Duration::MAX);
    let owner_control = input.control();
    let mut blocks = CaptureBlocks::new(RATE as u32, Some(input));

    let mut played = Vec::<f32>::new();
    let mut mic = Vec::<f32>::new();
    let mut processed = Vec::<f32>::new();
    let mut output = [0.0f32; RENDER_PERIOD * 2];
    // (arrival, stamp, native position, interleaved device mix)
    let mut loopback = std::collections::VecDeque::<(Instant, Instant, u64, Vec<f32>)>::new();
    let mut position_base = 0u64;
    let (mut rendered, mut capture_callbacks, mut owner_passes, mut polls) = (0usize, 0usize, 0usize, 0usize);
    let mut jitter = 91_u32;
    let mut last = None;
    let mut silence_frames = 0;
    let mut late_blocks = 0;
    let mut idle_seen = false;
    while capture_callbacks * CAPTURE_PERIOD < captured_frames {
        let render_at = at_frames(base, rendered * RENDER_PERIOD);
        let capture_at = at_frames(base, (capture_callbacks + 1) * CAPTURE_PERIOD)
            + INPUT_LATENCY
            + Duration::from_micros((random(&mut jitter).abs() * 2000.0) as u64);
        let owner_at = base + Duration::from_millis(20 * owner_passes as u64);
        let poll_at = base + POLL * polls as u32;
        let next = owner_at.min(render_at).min(capture_at).min(poll_at);
        if next == owner_at {
            let second = owner_passes / 50;
            if owner_passes % 50 == 0 {
                match scenario.system {
                    System::FailsAt(at) if second == at => {
                        if let Some(feed) = feed.take() {
                            feed.fail(
                                echo::SystemStatus::CaptureFailed,
                                "loopback GetBuffer failed (fixture)".into(),
                            );
                        }
                    }
                    System::OutputChangesAt(at) if second == at => {
                        if let Some(feed) = feed.take() {
                            feed.fail(
                                echo::SystemStatus::OutputChanged,
                                "endpoint invalidated (fixture)".into(),
                            );
                        }
                        loopback.clear();
                    }
                    System::OutputChangesAt(at) if second == at + 1 => {
                        // The new endpoint's own position line starts at zero.
                        position_base = (rendered * RENDER_PERIOD) as u64;
                        feed = Some(open(&control));
                    }
                    _ => {}
                }
            }
            owner_control.update(true, true, true, owner_at);
            while let Some(summary) = owner_control.take_summary() {
                silence_frames += summary.reference_silence_frames;
                late_blocks += summary.reference_late_blocks;
                idle_seen |= summary.reason == echo::Reason::ReferenceIdle;
                last = Some(summary);
            }
            owner_passes += 1;
        } else if next == render_at {
            render.fill(
                &mut output,
                NativeOutputTiming {
                    period_frames: RENDER_PERIOD as u32,
                    native_gap_frames: None,
                    callback_at: render_at,
                    playback_at: Some(render_at + OUTPUT_LATENCY),
                    sample_rate: RATE as u32,
                    channels: 2,
                    padding_frames: 0,
                    buffer_frames: RENDER_PERIOD as u32,
                },
            );
            // The device mixes the other program's stream with Oink's.
            let first = rendered * RENDER_PERIOD;
            let mut mix = Vec::with_capacity(RENDER_PERIOD * 2);
            for (i, frame) in output.as_chunks::<2>().0.iter().enumerate() {
                let o = other[first + i];
                played.push(frame[0] + o);
                mix.extend_from_slice(&[frame[0] + o, frame[1] + o]);
            }
            // A silent endpoint delivers no loopback packets at all.
            if !paused(first) {
                let stamp = render_at + LOOPBACK_STAMP;
                let arrival = stamp
                    + LOOPBACK_DELIVERY
                    + scenario.late
                    + Duration::from_micros((random(&mut jitter).abs() * 1500.0) as u64);
                loopback.push_back((arrival, stamp, first as u64 - position_base, mix));
            }
            rendered += 1;
        } else if next == poll_at {
            let mut delivered = false;
            while loopback.front().is_some_and(|p| p.0 <= poll_at) {
                let (_, stamp, position, mix) = loopback.pop_front().unwrap();
                if let Some(feed) = &mut feed {
                    feed.packet(echo::SystemPacket {
                        samples: &mix,
                        stamp: Some(stamp),
                        position: Some(position),
                        discontinuity: false,
                        arrived: poll_at,
                    });
                    delivered = true;
                }
            }
            if !delivered && let Some(feed) = &mut feed {
                feed.idle(poll_at);
            }
            polls += 1;
        } else {
            let delay = OUTPUT_LATENCY.as_nanos() as usize * RATE / 1_000_000_000 + ACOUSTIC_FRAMES;
            let first = capture_callbacks * CAPTURE_PERIOD;
            let samples: Vec<f32> = (first..first + CAPTURE_PERIOD)
                .map(|c| {
                    c.checked_sub(delay)
                        .and_then(|f| played.get(f))
                        .map_or(0.0, |s| 0.5 * s)
                })
                .collect();
            mic.extend_from_slice(&samples);
            blocks.callback(CAPTURE_PERIOD, samples.iter().copied(), capture_at, &mut |pcm, _| {
                processed.extend(pcm.as_chunks::<2>().0.iter().map(|frame| frame[0]))
            });
            capture_callbacks += 1;
        }
    }
    let tail = RATE * 4;
    let energy = |samples: &[f32]| samples.iter().map(|s| f64::from(s * s)).sum::<f64>();
    let raw = energy(&mic[mic.len() - tail..]);
    let out = energy(&processed[processed.len() - tail..]);
    assert!(raw > 1.0, "fixture must contain echo: {raw}");
    drop(feed);
    Outcome {
        attenuation: 10.0 * (raw / out).log10(),
        engines: control.engines_built(),
        control,
        last,
        silence_frames,
        late_blocks,
        idle_seen,
    }
}

/// A local application (music, video, notification) plays through the same
/// physical output as Oink but never through Oink's renderer. With only the
/// Oink playback reference (the behaviour before the full-system reference)
/// that echo stays; with the system mix as reference it is removed.
#[test]
fn desktop_case_a_cancels_the_echo_of_other_local_programs() {
    let oink_only = run_scenario(Scenario {
        system: System::Unavailable,
        other: true,
        pause: None,
        late: Duration::ZERO,
        seconds: 14,
    });
    let system = run_scenario(Scenario {
        system: System::Running,
        other: true,
        pause: None,
        late: Duration::ZERO,
        seconds: 14,
    });
    let summary = system.last.unwrap();
    eprintln!(
        "other-program echo: oink-only reference {:.2} dB, system reference {:.2} dB engines={} read_delay_us={} late_max_us={} late_blocks={} system_bytes={}",
        oink_only.attenuation,
        system.attenuation,
        system.engines,
        summary.reference_read_delay_us,
        summary.reference_late_max_us,
        system.late_blocks,
        summary.system_bytes,
    );
    assert!(oink_only.attenuation < 3.0, "oink-only={}", oink_only.attenuation);
    assert!(system.attenuation > 10.0, "system={}", system.attenuation);
    assert_eq!(system.engines, 1, "an unchanged configuration builds one engine");
    assert_eq!(summary.reference_source, echo::ReferenceSource::System);
    assert_eq!(summary.system_status, echo::SystemStatus::Running);
    // Packets arrive up to ~3.5 ms after their last frame's stamp (delivery
    // jitter plus the 2 ms poll); reads also span capture-stamp jitter (2 ms)
    // and the interpolation taps. The settled read delay covers those only.
    assert!(
        (3_000..=10_000).contains(&summary.reference_read_delay_us),
        "{}",
        summary.reference_read_delay_us
    );
}

/// Without the system reference (here: no System Audio Recording grant) the
/// Oink playback reference keeps working and the diagnostics say why.
#[test]
fn unavailable_system_reference_falls_back_to_the_oink_reference() {
    let outcome = run_scenario(Scenario {
        system: System::Unavailable,
        other: false,
        pause: None,
        late: Duration::ZERO,
        seconds: 14,
    });
    eprintln!("fallback attenuation={:.2} dB", outcome.attenuation);
    assert!(outcome.attenuation > 10.0, "attenuation={}", outcome.attenuation);
    assert_eq!(outcome.engines, 1);
    let summary = outcome.last.unwrap();
    assert_eq!(summary.reference_source, echo::ReferenceSource::Oink);
    assert_eq!(summary.system_status, echo::SystemStatus::PermissionDenied);
    assert!(
        outcome
            .control
            .system_detail()
            .is_some_and(|d| d.contains("not granted"))
    );
}

/// The running capture fails: the Oink reference takes over (one new epoch)
/// and the failure is recorded, not hidden.
#[test]
fn failed_system_capture_switches_to_the_oink_reference() {
    let outcome = run_scenario(Scenario {
        system: System::FailsAt(5),
        other: false,
        pause: None,
        late: Duration::ZERO,
        seconds: 14,
    });
    eprintln!(
        "capture failure: attenuation={:.2} dB engines={}",
        outcome.attenuation, outcome.engines
    );
    assert!(outcome.attenuation > 10.0, "attenuation={}", outcome.attenuation);
    assert_eq!(outcome.engines, 2, "system epoch, then the Oink reference epoch");
    let summary = outcome.last.unwrap();
    assert_eq!(summary.reference_source, echo::ReferenceSource::Oink);
    assert_eq!(summary.system_status, echo::SystemStatus::CaptureFailed);
}

/// An output change revokes the old system reference history; the reopened
/// capture starts a new reference epoch instead of continuing the old one.
#[test]
fn output_change_revokes_and_rebuilds_the_system_reference() {
    let outcome = run_scenario(Scenario {
        system: System::OutputChangesAt(4),
        other: true,
        pause: None,
        late: Duration::ZERO,
        seconds: 14,
    });
    eprintln!(
        "output change: attenuation={:.2} dB engines={}",
        outcome.attenuation, outcome.engines
    );
    assert!(outcome.attenuation > 10.0, "attenuation={}", outcome.attenuation);
    assert_eq!(outcome.engines, 3, "system, Oink while reopening, new system");
    let summary = outcome.last.unwrap();
    assert_eq!(summary.reference_source, echo::ReferenceSource::System);
    assert_eq!(summary.system_status, echo::SystemStatus::Running);
}

/// Nothing plays for three seconds and the loopback endpoint stops delivering
/// packets. That pause is silence, not a capture failure: the reference line
/// stays continuous (no new engine), the converged filter is kept, and
/// cancellation continues right after (the tail starts 4 s after the pause).
#[test]
fn silent_endpoint_without_loopback_packets_is_silence_not_failure() {
    let outcome = run_scenario(Scenario {
        system: System::Running,
        other: true,
        pause: Some((9, 12)),
        late: Duration::ZERO,
        seconds: 20,
    });
    let summary = outcome.last.unwrap();
    eprintln!(
        "loopback pause: attenuation={:.2} dB engines={} silence_frames={}",
        outcome.attenuation, outcome.engines, outcome.silence_frames
    );
    assert!(outcome.attenuation > 10.0, "attenuation={}", outcome.attenuation);
    assert_eq!(outcome.engines, 1);
    assert!(
        outcome.silence_frames > RATE as u64 * 5 / 2,
        "{}",
        outcome.silence_frames
    );
    assert!(outcome.idle_seen, "the pause is reported as an idle reference");
    assert_eq!(summary.reference_source, echo::ReferenceSource::System);
    assert_eq!(summary.system_status, echo::SystemStatus::Running);
}

/// A reference that arrives far later than its stamp (here 60 ms) would trail
/// its own echo in the microphone. Those blocks bypass as reference-late and
/// are counted; the read delay is not stretched beyond the causal bound.
#[test]
fn reference_later_than_the_causal_range_is_recorded_as_bypass() {
    let outcome = run_scenario(Scenario {
        system: System::Running,
        other: true,
        pause: None,
        late: Duration::from_millis(60),
        seconds: 14,
    });
    let summary = outcome.last.unwrap();
    eprintln!(
        "late reference: attenuation={:.2} dB late_blocks={} read_delay_us={} late_max_us={}",
        outcome.attenuation, outcome.late_blocks, summary.reference_read_delay_us, summary.reference_late_max_us
    );
    assert!(outcome.late_blocks > 1000, "{}", outcome.late_blocks);
    assert_eq!(summary.reason, echo::Reason::ReferenceLate);
    assert!(summary.reference_read_delay_us <= 40_000);
    assert!(summary.reference_late_max_us >= 60_000);
    assert_eq!(outcome.engines, 1);
}
