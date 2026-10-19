//! Source-side forwarding control, independent of OS volume and mute.
//! Curve measured with real VolumeDecibels reads on both supported Macs.

/// One millisecond at 48 kHz. Fixed for every setting, so changing gain never
/// changes stream alignment. Only aggregate levels are diagnosed.
pub const LOOKAHEAD_FRAMES: usize = 48;
const DELAY: usize = LOOKAHEAD_FRAMES + 1;
const MIN_HOLD: usize = 96;
const MAX_HOLD: usize = 1440;
pub const MAX_BOOST_DB: f64 = 12.098_328;
const MAC_RANGE_DB: f64 = 63.5;
const JOIN_SLOPE: f64 = MAC_RANGE_DB / 2.0;
const BOOST_CURVATURE: f64 = (JOIN_SLOPE / MAX_BOOST_DB) * (JOIN_SLOPE / MAX_BOOST_DB) - 1.5;

pub fn validate_percent(percent: u16) -> Result<(), String> {
    if percent <= 200 {
        Ok(())
    } else {
        Err("forwarding_volume_percent must be in 0..=200".into())
    }
}
/// Persisted control positions keep their numbers; no migration. One key press
/// moves ten points, independent of source/exit platform. Absence defaults to 100.
pub fn adjust(percent: u16, up: bool) -> u16 {
    if up {
        percent.saturating_add(10).min(200)
    } else {
        percent.saturating_sub(10)
    }
}
/// Continuous fit of actual VolumeDecibels at all 16 Mac slider positions.
/// Above unity the rational extension matches value, slope AND curvature (C2),
/// stays monotonic and reaches the measured useful +12.098328 dB at position 200.
/// Zero is exact mute; the device's finite minimum is not digital silence.
pub fn decibels(position: f64) -> Option<f64> {
    let x = position.clamp(0.0, 200.0) / 100.0;
    if x <= 0.0 || !x.is_finite() {
        return None;
    }
    Some(if x <= 1.0 {
        MAC_RANGE_DB * (x.sqrt() - 1.0)
    } else {
        let t = x - 1.0;
        JOIN_SLOPE * t / (1.0 + 0.5 * t + BOOST_CURVATURE * t * t).sqrt()
    })
}
pub fn amplitude(percent: u16) -> f32 {
    decibels(f64::from(percent)).map_or(0.0, |db| 10.0_f64.powf(db / 20.0) as f32)
}

/// 5 ms control ramp and a stereo-linked feed-forward peak limiter. Each peak
/// schedules a continuous 1 ms attack through the pending samples. A 10 ms
/// exponential release restores headroom between transients. No hard clipping
/// is used to create loudness; the final clamp only guards floating-point error.
pub struct Gain {
    current: f32,
    target: f32,
    ramp_step: f32,
    remaining: usize,
    limit: f32,
    hold: usize,
    hold_period: usize,
    since_peak: usize,
    previous_peak: f32,
    peak_rising: bool,
    cursor: usize,
    delayed: [[f32; 2]; DELAY],
    envelope: [f32; DELAY],
}
impl std::fmt::Debug for Gain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Pending audio must never enter diagnostic formatting.
        f.debug_struct("Gain")
            .field("current", &self.current)
            .field("target", &self.target)
            .field("remaining", &self.remaining)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}
impl Default for Gain {
    fn default() -> Self {
        Self {
            current: 1.0,
            target: 1.0,
            ramp_step: 0.0,
            remaining: 0,
            limit: 1.0,
            hold: 0,
            hold_period: MIN_HOLD,
            since_peak: MAX_HOLD + 1,
            previous_peak: 0.0,
            peak_rising: false,
            cursor: 0,
            delayed: [[0.0; 2]; DELAY],
            envelope: [1.0; DELAY],
        }
    }
}
impl Gain {
    pub fn new(percent: u16) -> Self {
        let gain = amplitude(percent);
        Self {
            current: gain,
            target: gain,
            ..Self::default()
        }
    }
    pub fn set_percent(&mut self, percent: u16) -> Result<(), String> {
        validate_percent(percent)?;
        let target = amplitude(percent);
        if self.target != target {
            self.target = target;
            self.remaining = 240;
            self.ramp_step = (target - self.current) / 240.0;
        }
        Ok(())
    }
    /// Native capture gaps must never replay delayed samples from the old burst.
    pub fn discard_pending(&mut self) {
        self.delayed.fill([0.0; 2]);
        self.envelope.fill(1.0);
        self.limit = 1.0;
        self.hold = 0;
        self.hold_period = MIN_HOLD;
        self.since_peak = MAX_HOLD + 1;
        self.previous_peak = 0.0;
        self.peak_rising = false;
    }
    pub fn process(&mut self, samples: &mut [f32]) {
        self.process_observed(samples, |_| {});
    }
    pub(crate) fn process_observed(&mut self, samples: &mut [f32], mut observe: impl FnMut(&[f32])) {
        for frame in samples.chunks_mut(2) {
            if self.remaining != 0 {
                self.current += self.ramp_step;
                self.remaining -= 1;
                if self.remaining == 0 {
                    self.current = self.target;
                }
            }
            let mut input = [0.0; 2];
            for (out, sample) in input.iter_mut().zip(frame.iter()) {
                let scaled = *sample * self.current;
                *out = if scaled.is_finite() { scaled } else { 0.0 };
            }
            let peak = input[0].abs().max(input[1].abs());
            let required = if peak > 1.0 { 1.0 / peak } else { 1.0 };
            self.delayed[self.cursor] = input;
            self.envelope[self.cursor] = required;
            if required < 1.0 {
                for back in 1..=LOOKAHEAD_FRAMES {
                    let index = (self.cursor + DELAY - back) % DELAY;
                    let attack = required + (1.0 - required) * (back as f32 / LOOKAHEAD_FRAMES as f32);
                    self.envelope[index] = self.envelope[index].min(attack);
                }
            }
            let out = (self.cursor + 1) % DELAY;
            // Hold only peaks that need the current attenuation (within
            // 0.1% to cover a sampled sine's changing peak phase). Merely
            // exceeding unity must not latch a past, much larger transient.
            let out_peak = self.delayed[out][0].abs().max(self.delayed[out][1].abs());
            self.since_peak = (self.since_peak + 1).min(MAX_HOLD + 1);
            if self.since_peak > MAX_HOLD {
                self.hold_period = MIN_HOLD;
            }
            // Follow repeated limiting peaks for 1.25 periods (2..30 ms).
            // Bass must not recover between half-waves; isolated transients
            // retain the short hold instead of keeping later material down.
            if out_peak < self.previous_peak
                && self.peak_rising
                && self.previous_peak > 1.0
                && 1.0 / self.previous_peak <= self.limit * 1.001
            {
                if self.since_peak <= MAX_HOLD {
                    self.hold_period = (self.since_peak * 5 / 4).clamp(MIN_HOLD, MAX_HOLD);
                }
                self.since_peak = 0;
                self.hold = self.hold.max(self.hold_period);
            }
            if out_peak != self.previous_peak {
                self.peak_rising = out_peak > self.previous_peak;
            }
            self.previous_peak = out_peak;
            if out_peak > 1.0 && 1.0 / out_peak <= self.limit * 1.001 {
                self.hold = self.hold_period;
            }
            if self.hold == 0 {
                self.limit += (1.0 - self.limit) / 480.0;
            } else {
                self.hold -= 1;
            }
            self.limit = self.limit.min(self.envelope[out]);
            for (sample, delayed) in frame.iter_mut().zip(self.delayed[out]) {
                *sample = (delayed * self.limit).clamp(-1.0, 1.0);
            }
            self.cursor = out;
            observe(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuous_control_ten_percent_is_the_fitted_macos_scalar() {
        let mut gain = Gain::new(10);
        let mut samples = [0.001; 960];
        gain.process(&mut samples);
        let db = 20.0 * (samples[959] / 0.001).log10();
        assert!((db - -43.419_537).abs() < 0.001, "continuous c=10: {db}");
    }
    #[test]
    fn macos_measured_steps_are_decibels_not_amplitude_percent() {
        // Real VolumeDecibels reads at n/16 on both supported test Macs.
        for (position, db) in [(25, -31.75), (50, -18.598_72), (75, -8.507_388), (100, 0.0)] {
            let mut gain = Gain::new(position);
            let mut samples = [0.001; 960];
            gain.process(&mut samples);
            let measured = 20.0 * (samples[959] / 0.001).log10();
            assert!((measured - db).abs() < 0.001, "position={position}: {measured} != {db}");
        }
    }
    #[test]
    fn unity_is_bit_exact_after_fixed_lookahead_and_partition_independent() {
        let original: Vec<_> = (0..48_000)
            .flat_map(|i| {
                let sample = (std::f32::consts::TAU * 997.0 * i as f32 / 48_000.0).sin();
                [sample, -sample]
            })
            .collect();
        let mut whole = original.clone();
        whole.extend([0.0; LOOKAHEAD_FRAMES * 2]);
        let mut split = whole.clone();
        Gain::new(100).process(&mut whole);
        let mut gain = Gain::new(100);
        for block in split.chunks_mut(30) {
            gain.set_percent(100).unwrap();
            gain.process(block);
        }
        assert_eq!(whole, split);
        assert_eq!(&whole[..LOOKAHEAD_FRAMES * 2], &[0.0; LOOKAHEAD_FRAMES * 2]);
        assert!(
            whole[LOOKAHEAD_FRAMES * 2..]
                .iter()
                .zip(original)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
    }
    #[test]
    fn continuous_fit_matches_all_measured_points_and_has_a_smooth_monotonic_boost() {
        let measured = [
            -47.625,
            -41.049_358_4,
            -36.003_692_6,
            -31.75,
            -28.002_420_4,
            -24.614_349_4,
            -21.498_699_2,
            -18.598_720_6,
            -15.875,
            -13.298_841_5,
            -10.848_580_4,
            -8.507_388_11,
            -6.261_874_2,
            -4.101_187_23,
            -2.016_387_7,
            0.0,
        ];
        for (i, db) in measured.into_iter().enumerate() {
            assert!((decibels((i + 1) as f64 * 100.0 / 16.0).unwrap() - db).abs() < 0.00001);
        }
        assert_eq!(decibels(0.0), None);
        assert_eq!(amplitude(0), 0.0);
        assert_eq!(amplitude(100), 1.0);
        for (position, expected) in [(110.0, 3.021_931), (150.0, 9.851_366), (200.0, 12.098_328)] {
            assert!((decibels(position).unwrap() - expected).abs() < 0.000001);
        }
        let mut previous = decibels(0.01).unwrap();
        for i in 2..=20_000 {
            let db = decibels(i as f64 / 100.0).unwrap();
            assert!(db > previous);
            previous = db;
        }
        let h = 0.001;
        let left = (decibels(100.0).unwrap() - decibels(100.0 - h).unwrap()) / h;
        let right = (decibels(100.0 + h).unwrap() - decibels(100.0).unwrap()) / h;
        assert!((left - right).abs() < 0.00001);
        for position in (0..=200).step_by(10) {
            assert_eq!(adjust(position, true), (position + 10).min(200));
            assert_eq!(adjust(position, false), position.saturating_sub(10));
        }
    }
    #[test]
    fn quiet_audio_has_the_requested_gain_including_boost_and_exact_mute() {
        for volume in [0, 6, 25, 50, 75, 100, 117, 150, 200] {
            let mut gain = Gain::new(volume);
            let mut samples = [0.01; 960];
            gain.process(&mut samples);
            assert!(
                samples[96..]
                    .iter()
                    .all(|s| (*s - amplitude(volume) * 0.01).abs() < 1e-6)
            );
            gain.set_percent(0).unwrap();
            gain.process(&mut [0.0; 960]);
            let mut silent = [0.01; 240];
            gain.process(&mut silent);
            assert_eq!(silent, [0.0; 240]);
        }
    }
    #[test]
    fn ramp_is_linked_partition_independent_and_retargets_without_jump() {
        let mut a = Gain::new(100);
        let mut b = Gain::new(100);
        a.set_percent(0).unwrap();
        b.set_percent(0).unwrap();
        let mut whole = vec![0.1; 576];
        let mut split = whole.clone();
        a.process(&mut whole);
        for chunk in split.chunks_mut(30) {
            b.set_percent(0).unwrap();
            b.process(chunk);
        }
        assert_eq!(whole, split);
        assert!(whole.as_chunks::<2>().0.iter().all(|pair| pair[0] == pair[1]));
        assert_eq!(whole[575], 0.0);
        assert!(whole[96..].windows(2).all(|w| (w[0] - w[1]).abs() < 0.00042));
        a.set_percent(200).unwrap();
        assert!(a.set_percent(201).is_err());
        let mut up = [0.1; 576];
        a.process(&mut up);
        assert!(up[96] < 0.002);
        assert!((up[575] - 0.1 * amplitude(200)).abs() < 1e-6);
        a.set_percent(0).unwrap();
        a.process(&mut [0.1; 240]);
        let value = a.current;
        a.set_percent(100).unwrap();
        assert_eq!(a.current, value, "retarget must not jump to the previous target");
    }
    #[test]
    fn sustained_bass_boost_does_not_modulate_between_periodic_peaks() {
        for hz in [20.0, 50.0, 100.0, 997.0] {
            let mut gain = Gain::new(200);
            let mut audio: Vec<_> = (0..48_000)
                .flat_map(|i| {
                    let sample = (std::f64::consts::TAU * hz * i as f64 / 48_000.0).sin() as f32 * 0.9;
                    [sample, sample]
                })
                .collect();
            gain.process(&mut audio);
            let error = (24_000..48_000)
                .map(|i| {
                    let expected = (std::f64::consts::TAU * hz * (i - LOOKAHEAD_FRAMES) as f64 / 48_000.0).sin();
                    (f64::from(audio[i * 2]) - expected).powi(2)
                })
                .sum::<f64>()
                / 24_000.0;
            assert!(
                error.sqrt() < 0.001,
                "{hz}Hz must preserve the sustained waveform, RMS error {}",
                error.sqrt()
            );
        }
    }
    #[test]
    fn limiter_recovers_from_a_larger_peak_while_smaller_overloads_continue() {
        let mut gain = Gain::new(100);
        gain.process(&mut [2.0; 480]);
        let mut body = [1.5; 9600];
        gain.process(&mut body);
        assert!(
            body[9599] > 0.99,
            "a past peak must not pin quieter sustained audio: {}",
            body[9599]
        );
    }
    #[test]
    fn limiter_anticipates_peaks_is_stereo_linked_and_recovers_between_transients() {
        let mut gain = Gain::new(100);
        let mut audio = vec![0.0; 4800];
        for frame in audio.as_chunks_mut::<2>().0.iter_mut() {
            *frame = [0.1, -0.05];
        }
        audio[480] = 2.0;
        audio[481] = -1.0;
        audio[1000] = f32::NAN;
        audio[1001] = f32::INFINITY;
        gain.process(&mut audio);
        let peak_at = 480 + LOOKAHEAD_FRAMES * 2;
        assert_eq!(audio[peak_at], 1.0);
        assert_eq!(audio[peak_at + 1], -0.5);
        assert!(
            audio[peak_at - 2] < 0.06,
            "attack begins before the peak reaches output"
        );
        assert!(audio.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        assert_eq!(&audio[1096..1098], &[0.0, 0.0]);
        assert!(audio[4798] > 0.099, "short release restores ordinary program level");
        gain.discard_pending();
        let mut gap = [0.0; 96];
        gain.process(&mut gap);
        assert_eq!(gap, [0.0; 96]);
    }
}
