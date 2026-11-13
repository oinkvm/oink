//! Deterministic offline numerical/CPU probe; never opens an audio device.
//! Optional --rir <mono 48k float32 WAV> tests a recorded room response.
use oink_aec3::Engine;
use std::time::Instant;
const RATE: usize = 48_000;
#[derive(Debug)]
pub struct Measurement {
    pub linear_erle: f64,
    pub suppression: f64,
    pub near_corr: f64,
    pub near_gain: f64,
    pub lag: usize,
    pub cpu_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
}
fn noise(seed: &mut u32) -> f32 {
    *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    ((*seed >> 8) as f32 / 8388608.0 - 1.0) * 0.3
}
fn cpu() -> u64 {
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct T {
            sec: i64,
            ns: i64,
        }
        unsafe extern "C" {
            fn clock_gettime(id: i32, t: *mut T) -> i32;
        }
        let mut t = T { sec: 0, ns: 0 };
        // SAFETY: Darwin thread CPU clock writes the initialized local timespec.
        unsafe {
            clock_gettime(16, &mut t);
        }
        t.sec as u64 * 1_000_000_000 + t.ns as u64
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        struct T {
            low: u32,
            high: u32,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn GetThreadTimes(t: *mut std::ffi::c_void, c: *mut T, e: *mut T, k: *mut T, u: *mut T) -> i32;
        }
        let (mut c, mut e, mut k, mut u) = (
            T { low: 0, high: 0 },
            T { low: 0, high: 0 },
            T { low: 0, high: 0 },
            T { low: 0, high: 0 },
        );
        // SAFETY: current-thread pseudo handle and writable local FILETIME outputs.
        unsafe {
            GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u);
        }
        (((u64::from(k.high) << 32) | u64::from(k.low)) + ((u64::from(u.high) << 32) | u64::from(u.low))) * 100
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        0
    }
}
pub fn run(delay: usize, near: bool, echo: bool, change: bool, rir: Option<&[f32]>) -> Measurement {
    let mut engine = Engine::new();
    let n = RATE * 20;
    let (mut left, mut right, mut voice, mut mic, mut result) = (
        vec![0.0f32; n],
        vec![0.0f32; n],
        vec![0.0f32; n],
        vec![0.0f32; n],
        vec![0.0f32; n],
    );
    let mut seed = 17;
    for i in 1..n {
        left[i] = 0.75 * left[i - 1] + noise(&mut seed) * 0.25;
        right[i] = 0.7 * right[i - 1] + noise(&mut seed) * 0.3;
        let t = i as f32 / RATE as f32;
        let envelope = (t * 4.1).sin().max(0.0).powi(2);
        voice[i] = envelope
            * ((t * std::f32::consts::TAU * 173.0).sin() * 0.12
                + (t * std::f32::consts::TAU * 346.0).sin() * 0.06
                + noise(&mut seed) * 0.08);
        if near && i > RATE * 4 {
            mic[i] += voice[i];
        }
        if echo && rir.is_none() && i > delay + 1500 {
            let gain = if change && i > RATE * 10 { 0.3 } else { 1.0 };
            mic[i] += gain
                * (0.55 * left[i - delay] + 0.2 * left[i - delay - 170] - 0.15 * right[i - delay - 340]
                    + 0.08 * right[i - delay - 1500]);
        }
    }
    if let Some(rir) = rir {
        let echo = convolve(&left, rir);
        for i in delay..n {
            mic[i] += echo[i - delay];
        }
    }
    let mut durations = Vec::with_capacity(n / 480);
    let (mut lp, mut rp) = (0.0f64, 0.0f64);
    let (mut erl, mut erle, mut internal_delay) = (0.0, 0.0, 0);
    let mut cpu_total = 0;
    for b in 0..n / 480 {
        let i = b * 480;
        let mut reference = [[0.0; 480]; 2];
        reference[0].copy_from_slice(&left[i..i + 480]);
        reference[1].copy_from_slice(&right[i..i + 480]);
        let input: &[f32; 480] = mic[i..i + 480].try_into().unwrap();
        let output: &mut [f32; 480] = (&mut result[i..i + 480]).try_into().unwrap();
        let mut linear = [0.0; 160];
        let started = Instant::now();
        let c = cpu();
        engine.process(&reference, input, output, &mut linear, change && b == 1000);
        cpu_total += cpu().saturating_sub(c);
        durations.push(started.elapsed().as_nanos() as u64);
        if b > 1400 {
            let (c, l) = engine.linear_powers();
            rp += c;
            lp += l;
        }
        if b % 100 == 0 {
            (erl, erle, internal_delay) = engine.metrics();
        }
        assert!(output.iter().all(|s| s.is_finite()));
    }
    durations.sort_unstable();
    let begin = RATE * 14;
    let end = n - 1024;
    let mut best = (0, f64::NEG_INFINITY, 0.0);
    if near {
        for lag in 350..500 {
            let (mut xy, mut xx, mut yy) = (0.0, 0.0, 0.0);
            for i in (begin..end).step_by(8) {
                let x = f64::from(voice[i]);
                let y = f64::from(result[i + lag]);
                xy += x * y;
                xx += x * x;
                yy += y * y;
            }
            let corr = xy / (xx * yy).sqrt().max(1e-20);
            if corr > best.1 {
                best = (lag, corr, 20.0 * (xy / xx).max(1e-20).log10());
            }
        }
    }
    let op = result[begin..end].iter().map(|x| f64::from(x * x)).sum::<f64>();
    let mp = mic[begin..end].iter().map(|x| f64::from(x * x)).sum::<f64>();
    let m = Measurement {
        linear_erle: 10.0 * (rp / lp.max(1e-30)).log10(),
        suppression: 10.0 * (mp / op.max(1e-30)).log10(),
        near_corr: best.1,
        near_gain: best.2,
        lag: best.0,
        cpu_us: cpu_total as f64 / 1000.0 / (n / 480) as f64,
        p95_us: durations[durations.len() * 95 / 100] as f64 / 1000.0,
        p99_us: durations[durations.len() * 99 / 100] as f64 / 1000.0,
        max_us: durations[durations.len() - 1] as f64 / 1000.0,
    };
    println!(
        "delay_ms={} near={} echo={} path_change={} recorded_rir={} {:?} engine=({erl:.2},{erle:.2},{internal_delay})",
        delay / 48,
        near,
        echo,
        change,
        rir.is_some(),
        m
    );
    m
}
// Test-only radix-2 FFT keeps recorded-RIR convolution reproducible without
// Python/scipy or an additional production dependency. No realtime path calls it.
fn fft(a: &mut [[f64; 2]], inverse: bool) {
    let n = a.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            a.swap(i, j);
        }
    }
    let mut width = 2;
    while width <= n {
        let angle = if inverse { 2.0 } else { -2.0 } * std::f64::consts::PI / width as f64;
        let step = [angle.cos(), angle.sin()];
        for start in (0..n).step_by(width) {
            let mut w = [1.0, 0.0];
            for k in 0..width / 2 {
                let u = a[start + k];
                let x = a[start + k + width / 2];
                let v = [x[0] * w[0] - x[1] * w[1], x[0] * w[1] + x[1] * w[0]];
                a[start + k] = [u[0] + v[0], u[1] + v[1]];
                a[start + k + width / 2] = [u[0] - v[0], u[1] - v[1]];
                w = [w[0] * step[0] - w[1] * step[1], w[0] * step[1] + w[1] * step[0]];
            }
        }
        width *= 2;
    }
    if inverse {
        for x in a {
            x[0] /= n as f64;
            x[1] /= n as f64;
        }
    }
}
pub fn convolve(signal: &[f32], rir: &[f32]) -> Vec<f32> {
    let size = (signal.len() + rir.len() - 1).next_power_of_two();
    let (mut a, mut b) = (vec![[0.0; 2]; size], vec![[0.0; 2]; size]);
    for (i, x) in signal.iter().enumerate() {
        a[i][0] = f64::from(*x);
    }
    for (i, x) in rir.iter().enumerate() {
        b[i][0] = f64::from(*x);
    }
    fft(&mut a, false);
    fft(&mut b, false);
    for (i, x) in a.iter_mut().enumerate() {
        *x = [x[0] * b[i][0] - x[1] * b[i][1], x[0] * b[i][1] + x[1] * b[i][0]];
    }
    fft(&mut a, true);
    a[..signal.len()].iter().map(|x| x[0] as f32).collect()
}
pub fn read_rir(path: &std::path::Path) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("read recorded RIR");
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let (mut offset, mut data, mut valid) = (12, None, false);
    while offset + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let chunk = &bytes[offset + 8..offset + 8 + size];
        match &bytes[offset..offset + 4] {
            b"fmt " => {
                valid = chunk[..4] == [3, 0, 1, 0]
                    && u32::from_le_bytes(chunk[4..8].try_into().unwrap()) == 48_000
                    && chunk[14..16] == [32, 0];
            }
            b"data" => data = Some(chunk),
            _ => (),
        };
        offset += 8 + size + size % 2;
    }
    assert!(valid, "RIR must be mono 48k float32 WAV");
    let mut rir: Vec<f32> = data
        .unwrap()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    assert!(rir.iter().all(|x| x.is_finite()));
    let peak = rir
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
        .unwrap()
        .0;
    // Remove the measurement tool's bulk pre-delay while retaining 2 ms before
    // its peak and the complete recorded decay. Normalize only echo energy.
    rir.drain(..peak.saturating_sub(96));
    let norm = rir.iter().map(|x| f64::from(x * x)).sum::<f64>().sqrt();
    for x in &mut rir {
        *x *= 0.6 / norm as f32;
    }
    rir
}
pub fn suite() {
    for delay in [0, 960, 3840, 12000] {
        let m = run(delay, false, true, false, None);
        assert!(m.linear_erle > 20.0, "{m:?}");
        assert!(m.suppression > 12.0, "{m:?}");
    }
    for echo in [false, true] {
        let m = run(960, true, echo, false, None);
        assert!(m.near_corr > 0.93, "{m:?}");
        assert!((-2.5..1.0).contains(&m.near_gain), "{m:?}");
        assert!((425..436).contains(&m.lag), "{m:?}");
    }
    let changed = run(960, false, true, true, None);
    assert!(changed.linear_erle > 18.0, "{changed:?}"); // Stationary noise is deliberately a stress case for AEC3 comfort-noise
    // replacement after a -10.5 dB path change. Linear ERLE is the cancellation
    // criterion; the audible output must still improve by at least 6 dB.
    assert!(changed.suppression > 6.0, "{changed:?}");
}
fn main() {
    suite();
    let mut args = std::env::args().skip(1);
    if let Some(flag) = args.next() {
        assert_eq!(flag, "--rir");
        let rir = read_rir(std::path::Path::new(&args.next().expect("--rir path")));
        let m = run(960, false, true, false, Some(&rir));
        assert!(m.linear_erle > 12.0, "{m:?}");
        assert!(m.suppression > 10.0, "{m:?}");
        let m = run(960, true, true, false, Some(&rir));
        assert!(m.near_corr > 0.85, "{m:?}");
        assert!((-4.0..1.0).contains(&m.near_gain), "{m:?}");
    }
}
