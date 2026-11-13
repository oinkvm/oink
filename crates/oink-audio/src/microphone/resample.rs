//! Mono capture uses the same band-limited kernel as speaker audio.
#[derive(Debug)]
pub struct Resampler {
    history: Vec<f32>,
    kernel: crate::resample::Kernel,
    input_position: u64,
    output_position: u64,
    step: f64,
}
impl Resampler {
    pub fn new(rate: u32) -> Self {
        let step = f64::from(rate) / 48_000.0;
        let kernel = crate::resample::Kernel::new(step);
        Self {
            history: vec![0.0; kernel.taps()],
            kernel,
            input_position: 0,
            output_position: 0,
            step,
        }
    }
    pub fn reset(&mut self) {
        self.history.fill(0.0);
        self.input_position = 0;
        self.output_position = 0;
    }
    pub fn delay_ns(&self) -> u64 {
        (self.kernel.delay() as f64 * 1_000_000_000.0 / (48_000.0 * self.step)) as u64
    }

    pub fn push(&mut self, value: f32, mut emit: impl FnMut(f32)) {
        let value = if value.is_finite() { value } else { 0.0 };
        if self.step == 1.0 {
            emit(value);
            return;
        }
        let current = self.input_position;
        let len = self.history.len();
        self.history[current as usize % len] = value;
        while (self.output_position as f64 * self.step).floor() <= current as f64 {
            emit(self.kernel.sample(self.output_position as f64 * self.step, |index| {
                if index < 0 {
                    0.0
                } else {
                    self.history[index as usize % len]
                }
            }));
            self.output_position += 1;
        }
        self.input_position += 1;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn convert(rate: u32, frequency: f64) -> Vec<f32> {
        let mut converter = Resampler::new(rate);
        let mut output = Vec::new();
        for i in 0..rate {
            let input = (2.0 * std::f64::consts::PI * frequency * f64::from(i) / f64::from(rate)).sin() as f32;
            converter.push(input, |sample| output.push(sample));
        }
        output
    }
    #[test]
    fn microphone_impulse_has_shared_filter_delay_and_unity_bypass() {
        for rate in [44_100, 48_000, 96_000] {
            let mut converter = Resampler::new(rate);
            let mut output = Vec::new();
            for i in 0..256 {
                converter.push(if i == 0 { 1.0 } else { 0.0 }, |v| output.push(v));
            }
            let peak = (0..output.len())
                .max_by(|a, b| output[*a].abs().total_cmp(&output[*b].abs()))
                .unwrap();
            let expected = if rate == 48_000 {
                0.0
            } else {
                converter.kernel.delay() as f64 * 48_000.0 / f64::from(rate)
            };
            assert!((peak as f64 - expected).abs() <= 0.5);
            eprintln!(
                "microphone {rate}->48000: peak {peak} frames, {} ms",
                peak as f64 / 48.0
            );
        }
    }
    #[test]
    fn sample_count_and_passband() {
        for rate in [16_000, 44_100, 48_000, 96_000] {
            let output = convert(rate, 1000.0);
            assert!((47_900..48_001).contains(&output.len()));
            let rms = (output[200..].iter().map(|s| s * s).sum::<f32>() / (output.len() - 200) as f32).sqrt();
            assert!((0.69..0.72).contains(&rms), "{rate}: {rms}");
        }
    }
    #[test]
    fn rejects_above_destination_nyquist() {
        let output = convert(96_000, 32_000.0);
        let rms = (output[200..].iter().map(|s| s * s).sum::<f32>() / (output.len() - 200) as f32).sqrt();
        assert!(rms < 0.002, "alias rms {rms}");
    }
}
