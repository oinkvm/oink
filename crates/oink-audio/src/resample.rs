//! Fractional-delay, band-limited conversion shared by capture and playout.
//! The base FIR delays the signal by 48 input frames (1 ms at 48 kHz).
//! Larger downsampling ratios scale its support to preserve the audible passband.
//! Tables are prepared off the audio thread. Adjacent phases are interpolated
//! to avoid phase-quantisation distortion as asynchronous clocks move.
const TAPS: usize = 96;
const PHASES: usize = 1024;

#[derive(Debug)]
pub(crate) struct Kernel {
    coefficients: Vec<f32>,
    taps: usize,
}
impl Kernel {
    pub(crate) fn new(step: f64) -> Self {
        let cutoff = 0.97 / step.max(1.0);
        // At 192 -> 48 kHz, 96 input taps would roll off 20 kHz by 0.43 dB.
        // Scale in whole input/output-rate multiples; ordinary 44.1/48k
        // conversion retains 96 taps and its original one-millisecond budget.
        let taps = TAPS * step.floor().max(1.0) as usize;
        let delay = taps / 2;
        let mut coefficients = vec![0.0; taps * (PHASES + 1)];
        for (phase, table) in coefficients.chunks_exact_mut(taps).enumerate() {
            let fraction = phase as f64 / PHASES as f64;
            let mut sum = 0.0;
            for (tap, coefficient) in table.iter_mut().enumerate() {
                let x = tap as f64 - delay as f64 + fraction;
                let sinc = if x.abs() < 1e-12 {
                    cutoff
                } else {
                    (std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x)
                };
                let angle = std::f64::consts::PI * x / delay as f64;
                let window = 0.42 + 0.5 * angle.cos() + 0.08 * (2.0 * angle).cos();
                *coefficient = (sinc * window) as f32;
                sum += *coefficient;
            }
            for coefficient in table {
                *coefficient /= sum;
            }
        }
        Self { coefficients, taps }
    }
    pub(crate) fn taps(&self) -> usize {
        self.taps
    }
    pub(crate) fn delay(&self) -> usize {
        self.taps / 2
    }
    pub(crate) fn sample(&self, position: f64, mut input: impl FnMut(isize) -> f32) -> f32 {
        let phase = position.fract() * PHASES as f64;
        let index = phase.floor() as usize;
        let fraction = (phase - index as f64) as f32;
        let center = position.floor() as isize;
        let mut sum = 0.0;
        let a = &self.coefficients[index * self.taps..][..self.taps];
        let b = &self.coefficients[(index + 1) * self.taps..][..self.taps];
        for (tap, (&a, &b)) in a.iter().zip(b).enumerate() {
            sum += input(center - tap as isize) * (a + (b - a) * fraction);
        }
        sum
    }
}

/// Retains the FIR's past without allocating in callbacks.
#[derive(Debug)]
pub(crate) struct History {
    samples: Vec<f32>,
    channels: usize,
}
impl History {
    pub(crate) fn new(channels: usize, frames: usize) -> Self {
        Self {
            samples: vec![0.0; frames * channels],
            channels,
        }
    }
    pub(crate) fn clear(&mut self) {
        self.samples.fill(0.0);
    }
    pub(crate) fn get(&self, frame: isize, channel: usize) -> f32 {
        self.samples
            [((self.samples.len() as isize / self.channels as isize + frame) as usize) * self.channels + channel]
    }
    pub(crate) fn retain(&mut self, frames: usize, mut input: impl FnMut(usize, usize) -> f32) {
        let capacity = self.samples.len() / self.channels;
        let count = frames.min(capacity);
        self.samples.copy_within(count * self.channels.., 0);
        for frame in 0..count {
            for channel in 0..self.channels {
                self.samples[(capacity - count + frame) * self.channels + channel] =
                    input(frames - count + frame, channel);
            }
        }
    }
}

use std::time::{Duration, Instant};

pub(crate) struct StereoResampler {
    kernel: crate::resample::Kernel,
    history: crate::resample::History,
    step: f64,
    input_rate: f64,
    position: f64,
    pending: Vec<f32>,
    pending_time: Option<Instant>,
    pending_callback: Option<Instant>,
    pending_sequence: u64,
}

impl StereoResampler {
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn new(rate: f64) -> Self {
        Self::with_rates(rate, 48_000.0, 4096)
    }
    pub(crate) fn with_rates(rate: f64, output_rate: f64, max_frames: usize) -> Self {
        let kernel = Kernel::new(rate / output_rate);
        Self {
            history: History::new(2, kernel.taps()),
            kernel,
            step: rate / output_rate,
            input_rate: rate,
            position: 0.0,
            pending: {
                let mut pending = vec![0.0; (max_frames + 1) * 2];
                std::hint::black_box(&mut pending);
                pending.clear();
                pending
            },
            pending_time: None,
            pending_callback: None,
            pending_sequence: 0,
        }
    }

    pub(crate) fn output_capacity(&self, input_samples: usize) -> usize {
        ((self.pending.len() + input_samples) as f64 / self.step).ceil() as usize + 4
    }
    pub(crate) fn reset(&mut self) {
        self.pending.clear();
        self.history.clear();
        self.position = 0.0;
        self.pending_time = None;
        self.pending_callback = None;
        self.pending_sequence = 0;
    }
    pub(crate) fn process_timed_into(
        &mut self,
        input: &[f32],
        first_sample_at: Option<Instant>,
        callback_at: Option<Instant>,
        sequence: u64,
        output: &mut [f32],
    ) -> (usize, crate::source::Stamp) {
        let previous_frames = self.pending.len() / 2;
        if previous_frames == 0 {
            self.pending_time = first_sample_at;
            self.pending_callback = callback_at;
            self.pending_sequence = sequence;
        }
        let rate = self.input_rate;
        let stamp = crate::source::Stamp {
            first_sample_at: self
                .pending_time
                .and_then(|at| at.checked_add(Duration::from_secs_f64(self.position / rate))),
            callback_at: self.pending_callback,
            source_frame_position: Some(self.pending_sequence + self.position as u64),
            delivered_at: Instant::now(),
            discontinuity: false,
        };
        let mut count = 0;
        if self.step == 1.0 {
            count = input.len();
            output[..count].copy_from_slice(input);
        } else {
            self.pending.extend_from_slice(input);
            let frames = self.pending.len() / 2;
            while self.position + 1.0 < frames as f64 && count + 2 <= output.len() {
                for channel in 0..2 {
                    output[count + channel] = self.kernel.sample(self.position, |index| {
                        if index < 0 {
                            self.history.get(index, channel)
                        } else {
                            self.pending[index as usize * 2 + channel]
                        }
                    });
                }
                count += 2;
                self.position += self.step;
            }
            let consumed = (self.position as usize).min(frames);
            self.history
                .retain(consumed, |frame, channel| self.pending[frame * 2 + channel]);
            self.pending.drain(..consumed * 2);
            self.position -= consumed as f64;
        }
        let consumed = previous_frames + input.len() / 2 - self.pending.len() / 2;
        if consumed >= previous_frames {
            self.pending_time = first_sample_at
                .and_then(|at| at.checked_add(Duration::from_secs_f64((consumed - previous_frames) as f64 / rate)));
            self.pending_callback = callback_at;
            self.pending_sequence = sequence + (consumed - previous_frames) as u64;
        } else {
            self.pending_time = self
                .pending_time
                .and_then(|at| at.checked_add(Duration::from_secs_f64(consumed as f64 / rate)));
            self.pending_sequence += consumed as u64;
        }
        (count, stamp)
    }
}

#[cfg(test)]
mod quality_tests {
    use super::*;
    fn response(input_rate: f64, output_rate: f64, frequency: f64, ppm: f64) -> (f64, f64) {
        let step = input_rate / output_rate * (1.0 + ppm / 1e6);
        let kernel = Kernel::new(input_rate / output_rate * 1.011);
        let n = 8192;
        let mut signal = 0.0;
        let mut error = 0.0;
        for i in 0..n {
            let position = (i + 256) as f64 * step;
            let actual = kernel.sample(position, |index| {
                (std::f64::consts::TAU * frequency * index as f64 / input_rate).sin() as f32
            });
            let expected = (std::f64::consts::TAU * frequency * (position - kernel.delay() as f64) / input_rate).sin();
            signal += expected * expected;
            error += (f64::from(actual) - expected).powi(2);
        }
        (signal, error)
    }
    #[test]
    fn capture_chunk_boundaries_preserve_samples_and_impulse_delay() {
        for (input_rate, output_rate) in [(44_100.0, 48_000.0), (96_000.0, 48_000.0), (44_100.0, 44_100.0)] {
            let input: Vec<_> = (0..4096)
                .flat_map(|i| {
                    let value = (i as f64 * 0.7).sin() as f32 * 0.1;
                    [value, -value]
                })
                .collect();
            let mut whole = StereoResampler::with_rates(input_rate, output_rate, 4096);
            let mut expected = vec![0.0; whole.output_capacity(input.len())];
            let (count, _) = whole.process_timed_into(&input, None, None, 0, &mut expected);
            expected.truncate(count);
            let mut chunked = StereoResampler::with_rates(input_rate, output_rate, 4096);
            let mut actual = Vec::new();
            let mut scratch = [0.0; 1024];
            for chunk in input.chunks(254) {
                let (count, _) = chunked.process_timed_into(chunk, None, None, 0, &mut scratch);
                actual.extend_from_slice(&scratch[..count]);
            }
            assert_eq!(actual.len(), expected.len());
            assert!(actual.iter().zip(expected).all(|(a, b)| (a - b).abs() < 2e-6));
            let mut converter = StereoResampler::with_rates(input_rate, output_rate, 512);
            let mut impulse = [0.0; 1024];
            impulse[0] = 1.0;
            let mut output = vec![0.0; converter.output_capacity(impulse.len())];
            let (count, _) = converter.process_timed_into(&impulse, None, None, 0, &mut output);
            let peak = (0..count / 2)
                .max_by(|a, b| output[*a * 2].abs().total_cmp(&output[*b * 2].abs()))
                .unwrap();
            let expected = if input_rate == output_rate {
                0.0
            } else {
                converter.kernel.delay() as f64 * output_rate / input_rate
            };
            assert!((peak as f64 - expected).abs() <= 0.5);
            eprintln!(
                "capture {input_rate}->{output_rate}: impulse peak {peak} output frames, {} ms",
                peak as f64 * 1000.0 / output_rate
            );
        }
    }
    #[test]
    fn audible_band_survives_conversion_and_clock_drift() {
        for (input, output) in [
            (44_100.0, 48_000.0),
            (48_000.0, 44_100.0),
            (48_000.0, 48_000.0),
            (96_000.0, 48_000.0),
            (192_000.0, 48_000.0),
        ] {
            for ppm in [-700.0, 0.0, 700.0] {
                for frequency in [997.0, 10_003.0, 18_001.0, 20_003.0] {
                    let (signal, error) = response(input, output, frequency, ppm);
                    assert!(
                        error / signal < 0.0002,
                        "{input}->{output} {frequency}Hz {ppm}ppm: {} dB error",
                        10.0 * (error / signal).log10()
                    );
                }
            }
        }
    }
    #[test]
    fn downsampling_rejects_out_of_band_energy() {
        for (input, output, frequency) in [(96_000.0, 48_000.0, 32_000.0), (48_000.0, 44_100.0, 23_500.0)] {
            let kernel = Kernel::new(input / output);
            let energy: f64 = (256..8192)
                .map(|i| {
                    let value = kernel.sample(i as f64 * input / output, |index| {
                        (std::f64::consts::TAU * frequency * index as f64 / input).sin() as f32
                    });
                    f64::from(value).powi(2)
                })
                .sum();
            assert!(energy / ((8192 - 256) as f64) < 1e-7, "alias energy {energy}");
        }
    }
    #[test]
    fn impulse_measures_filter_latency_and_unity_capture_bypasses_it() {
        let kernel = Kernel::new(1.0);
        let peak = (0..96)
            .max_by(|a, b| {
                let sample = |i| kernel.sample(i as f64, |j| if j == 0 { 1.0 } else { 0.0 }).abs();
                sample(*a).total_cmp(&sample(*b))
            })
            .unwrap();
        assert_eq!(peak, 48);
        let mut converter = StereoResampler::with_rates(48_000.0, 48_000.0, 128);
        let mut input = [0.0; 256];
        input[0] = 1.0;
        input[1] = -1.0;
        let mut output = [0.0; 256];
        let (count, _) = converter.process_timed_into(&input, None, None, 0, &mut output);
        assert_eq!(count, input.len());
        assert_eq!(input, output);
    }
}
