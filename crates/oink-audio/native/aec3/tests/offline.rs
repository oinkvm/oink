#[allow(dead_code)]
#[path = "../examples/offline.rs"]
mod harness;
#[test]
fn deterministic_delay_doubletalk_and_volume_change() {
    harness::suite();
}
#[test]
#[ignore = "requires explicit OINK_AEC_RIR path to a recorded mono 48k float32 room response"]
fn recorded_room_response() {
    let path = std::env::var_os("OINK_AEC_RIR").expect("OINK_AEC_RIR must identify the recorded fixture");
    let rir = harness::read_rir(std::path::Path::new(&path));
    let m = harness::run(960, false, true, false, Some(&rir));
    assert!(m.linear_erle > 12.0, "{m:?}");
    assert!(m.suppression > 10.0, "{m:?}");
    let m = harness::run(960, true, true, false, Some(&rir));
    assert!(m.near_corr > 0.85, "{m:?}");
    assert!((-4.0..1.0).contains(&m.near_gain), "{m:?}");
}

/// The 0.4.0 howling loop (2026-11-12), offline: the sent microphone is played
/// back on the far side with gain (a microphone test app), forwarded back to
/// this device's speaker and picked up again by its microphone after a short
/// acoustic delay. The loop gain is above 1, so the raw microphone howls.
/// AEC3's output is sent from the very first block (no convergence gate).
///
/// What AEC3 can and cannot do on a cold engine (measured 2026-11-12 by
/// instrumenting the pinned engine): before its render delay controller has
/// locked onto the echo delay there is no delay estimate, so the residual
/// echo estimator takes its direct-path delay from the unconverged linear
/// filter (`AecState::FilterDelay` without an external delay). On the first
/// render burst that delay jumped to 26-28 blocks (~110 ms), the echo
/// estimate pointed at the silence before the burst, and the suppressor
/// passed that first ~100 ms burst almost unchanged (0.4 dB). The controller
/// locks within that burst; from the next trip on the echo estimate is
/// aligned and the returning echo is suppressed by > 30 dB. Upstream WebRTC
/// behaves the same, and the gate it replaced was worse: it passed raw audio
/// for many trips (30 confident + 50 render-active blocks first), long enough
/// to howl. So the test requires: the loop is broken at the second return,
/// never builds up, and the filter converges afterwards.
#[test]
fn closed_loop_echo_is_broken_at_the_second_return_and_converges() {
    const BLOCK: usize = 480;
    const RATE: usize = 48_000;
    // Far side: record, play back 2.5x louder; network and playback add 250 ms.
    const GAIN: f32 = 2.5;
    const NETWORK: usize = RATE / 4;
    // Acoustic path of the speaker/microphone device: 2 ms direct + a reflection.
    const DIRECT: usize = 96;
    const REFLECTION: usize = DIRECT + 170;
    const LOOP_SECONDS: usize = 10;
    const OPEN_SECONDS: usize = 6;
    // The engine's measured near-only output delay; the raw control is delayed
    // equally, so both loops have the same round trip.
    const ENGINE_DELAY: usize = 430;
    let n = RATE * (LOOP_SECONDS + OPEN_SECONDS);
    let mut seed = 41_u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (seed >> 8) as f32 / 8388608.0 - 1.0
    };
    // 100 ms of speech-like near end, otherwise only a -80 dBFS noise floor.
    let near: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            let floor = noise() * 1e-4;
            if (0.10..0.20).contains(&t) {
                let envelope = ((t - 0.10) * std::f32::consts::PI / 0.10).sin();
                floor
                    + envelope
                        * ((t * std::f32::consts::TAU * 173.0).sin() * 0.06
                            + (t * std::f32::consts::TAU * 346.0).sin() * 0.03
                            + noise() * 0.04)
            } else {
                floor
            }
        })
        .collect();
    // Independent far-end program for the open-loop phase.
    let mut program = vec![0.0f32; n];
    for i in 1..n {
        program[i] = 0.75 * program[i - 1] + noise() * 0.08;
    }
    let loop_end = RATE * LOOP_SECONDS;
    let echo = |render: &[f32], i: usize| {
        let at = |delay: usize| i.checked_sub(delay).map_or(0.0, |j| render[j]);
        0.6 * at(DIRECT) + 0.2 * at(REFLECTION)
    };
    let render_at = |sent: &[f32], i: usize| {
        if i < loop_end {
            i.checked_sub(NETWORK)
                .map_or(0.0, |j| (GAIN * sent[j]).clamp(-1.0, 1.0))
        } else {
            program[i]
        }
    };
    let rms = |samples: &[f32]| (samples.iter().map(|s| f64::from(s * s)).sum::<f64>() / samples.len() as f64).sqrt();

    // Raw control: the same loop sending the delay-matched raw microphone.
    let (mut render, mut mic, mut sent) = (vec![0.0f32; n], vec![0.0f32; n], vec![0.0f32; n]);
    for i in 0..loop_end {
        render[i] = render_at(&sent, i);
        mic[i] = near[i] + echo(&render, i);
        sent[i] = i.checked_sub(ENGINE_DELAY).map_or(0.0, |j| mic[j]);
    }
    let near_rms = rms(&near[RATE / 10..RATE / 5]);
    let late = RATE * 6..loop_end;
    let raw_late = rms(&sent[late.clone()]);
    assert!(
        raw_late > 3.0 * near_rms,
        "the raw loop must howl: late {raw_late} near {near_rms}"
    );

    // AEC3's output is sent from the first block.
    let mut engine = oink_aec3::Engine::new();
    let (mut render, mut mic, mut sent) = (vec![0.0f32; n], vec![0.0f32; n], vec![0.0f32; n]);
    let (mut capture_power, mut linear_power) = (0.0f64, 0.0f64);
    for block in 0..n / BLOCK {
        let first = block * BLOCK;
        // NETWORK exceeds one block: this block's render needs earlier output only.
        for i in first..first + BLOCK {
            render[i] = render_at(&sent, i);
            mic[i] = near[i] + echo(&render, i);
        }
        let reference: [f32; BLOCK] = render[first..first + BLOCK].try_into().unwrap();
        let input: &[f32; BLOCK] = mic[first..first + BLOCK].try_into().unwrap();
        let output: &mut [f32; BLOCK] = (&mut sent[first..first + BLOCK]).try_into().unwrap();
        let mut linear = [0.0; 160];
        engine.process(&[reference, reference], input, output, &mut linear, false);
        assert!(output.iter().all(|s| s.is_finite()));
        if first >= n - RATE * 3 {
            let (c, l) = engine.linear_powers();
            capture_power += c;
            linear_power += l;
        }
    }
    // Each returning echo arrives while the near end is silent: the first
    // after one 250 ms trip, the second one trip later. The output lags the
    // microphone by the engine delay.
    let attenuation_at = |window: std::ops::Range<usize>| {
        let shifted = window.start + ENGINE_DELAY..window.end + ENGINE_DELAY;
        let echo_rms = rms(&mic[window.clone()]);
        assert!(
            echo_rms > 10.0 * rms(&near[window]),
            "fixture: the window holds an echo"
        );
        20.0 * (echo_rms / rms(&sent[shifted])).log10()
    };
    let first_attenuation = attenuation_at(RATE * 36 / 100..RATE * 60 / 100);
    let second_attenuation = attenuation_at(RATE * 61 / 100..RATE * 85 / 100);
    let processed_late = rms(&sent[late]);
    let open = n - RATE * 3..n;
    let suppression = 20.0 * (rms(&mic[open.clone()]) / rms(&sent[open])).log10();
    let linear_erle = 10.0 * (capture_power / linear_power.max(1e-30)).log10();
    eprintln!(
        "closed loop: near {near_rms:.4} raw late {raw_late:.4} processed late {processed_late:.6} \
         echo attenuation first {first_attenuation:.1} dB second {second_attenuation:.1} dB; \
         open loop linear ERLE {linear_erle:.1} dB \
         suppression {suppression:.1} dB (erl, erle, delay) {:?}",
        engine.metrics()
    );
    // The first burst on a cold engine may pass (see above); the second may not.
    assert!(
        second_attenuation > 20.0,
        "second echo attenuation {second_attenuation} dB (first {first_attenuation} dB)"
    );
    assert!(
        processed_late < 0.1 * near_rms,
        "the loop must not build up: late {processed_late} near {near_rms}"
    );
    assert!(linear_erle > 15.0, "linear ERLE {linear_erle}");
    assert!(suppression > 12.0, "suppression {suppression}");
}
