//! Streaming windowed-sinc conversion with anti-aliasing on downsampling.
//! Fixed history and coefficient tables; independent of native callback boundaries.
#[cfg(any(target_os = "macos", windows, test))]
pub(super) struct Resampler {
    history: [f32; 64],
    coefficients: Vec<[f32; 64]>,
    input_position: u64,
    next_output: f64,
    step: f64,
}
#[cfg(any(target_os = "macos", windows, test))]
impl Resampler {
    pub fn new(rate: u32) -> Self {
        let cutoff = (48_000.0 / f64::from(rate)).min(1.0) * 0.90;
        let mut coefficients = vec![[0.0; 64]; 256];
        for (phase, table) in coefficients.iter_mut().enumerate() {
            let fraction = phase as f64 / 256.0;
            let mut sum = 0.0;
            for (tap, coefficient) in table.iter_mut().enumerate() {
                let x = tap as f64 - 31.0 - fraction;
                let sinc = if x.abs() < 1e-12 {
                    cutoff
                } else {
                    (std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x)
                };
                let window = 0.5 + 0.5 * (std::f64::consts::PI * x / 32.0).cos();
                *coefficient = (sinc * window) as f32;
                sum += *coefficient;
            }
            for coefficient in table {
                *coefficient /= sum;
            }
        }
        Self {
            history: [0.0; 64],
            coefficients,
            input_position: 0,
            next_output: 0.0,
            step: f64::from(rate) / 48_000.0,
        }
    }
    pub fn push(&mut self, value: f32, mut emit: impl FnMut(f32)) {
        let current = self.input_position;
        self.history[current as usize % 64] = if value.is_finite() { value } else { 0.0 };
        // 32 input frames of lookahead: always retain all 64 filter samples.
        while self.next_output.floor() + 32.0 <= current as f64 {
            let center = self.next_output.floor() as i64;
            let phase = ((self.next_output.fract() * 256.0) as usize).min(255);
            let mut sample = 0.0;
            for (tap, coefficient) in self.coefficients[phase].iter().enumerate() {
                let index = center - 31 + tap as i64;
                if index >= 0 {
                    sample += self.history[index as usize % 64] * coefficient;
                }
            }
            emit(sample);
            self.next_output += self.step;
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
