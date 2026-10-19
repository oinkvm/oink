//! Aggregate metadata only; no samples leave the capture/encoder thread.
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SampleLevels {
    /// None is digital silence (-infinity dBFS) if frames > 0; no measurement if 0.
    pub peak_dbfs: Option<f64>,
    pub rms_dbfs: Option<f64>,
    /// A frame is zero only when both stereo samples are exactly zero.
    /// Zero when frames=0 (no samples measured, not a digital-silence assertion).
    pub zero_frame_fraction: f64,
    pub frames: u64,
    pub nonfinite_samples: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureLevels {
    pub window_sequence: u64,
    pub window_ms: f64,
    /// Age of the last completed capture-worker window. No-buffer windows have
    /// frames=0; age grows only if the worker stops publishing metadata.
    pub age_ms: u64,
    pub before_gain: SampleLevels,
    /// None when no frames were processed, including permission verification.
    pub after_gain: Option<SampleLevels>,
    /// Windows only. Counts GetBuffer results, including SILENT results whose
    /// data pointer is not read. None before the first Windows buffer, or on a
    /// backend that does not expose this flag; later empty windows report zero.
    pub wasapi_buffers: Option<u64>,
    pub wasapi_silent_buffers: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Accumulator {
    peak: f64,
    squares: f64,
    frames: u64,
    zero_frames: u64,
    nonfinite_samples: u64,
}
impl Accumulator {
    pub(crate) fn frame(&mut self, frame: &[f32]) -> bool {
        self.frames += 1;
        let mut zero = true;
        let mut signal = false;
        for &sample in frame {
            zero &= sample == 0.0;
            if sample.is_finite() {
                signal |= sample.abs() > 1e-7;
                let sample = f64::from(sample);
                self.peak = self.peak.max(sample.abs());
                self.squares += sample * sample;
            } else {
                self.nonfinite_samples += 1;
            }
        }
        self.zero_frames += u64::from(zero);
        signal
    }
    fn levels(self) -> SampleLevels {
        let samples = self.frames * 2 - self.nonfinite_samples;
        SampleLevels {
            peak_dbfs: dbfs(self.peak),
            rms_dbfs: (samples > 0)
                .then(|| dbfs((self.squares / samples as f64).sqrt()))
                .flatten(),
            zero_frame_fraction: if self.frames == 0 {
                0.0
            } else {
                self.zero_frames as f64 / self.frames as f64
            },
            frames: self.frames,
            nonfinite_samples: self.nonfinite_samples,
        }
    }
}
fn dbfs(amplitude: f64) -> Option<f64> {
    (amplitude > 0.0).then(|| 20.0 * amplitude.log10())
}

/// Copy-only handoff; conversion to dBFS happens on the ordinary status thread.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Window {
    pub sequence: u64,
    pub started: Instant,
    pub ended: Instant,
    pub before: Accumulator,
    pub after: Accumulator,
    pub wasapi_buffers: Option<u64>,
    pub wasapi_silent_buffers: Option<u64>,
}
impl Window {
    pub(crate) fn levels(self, now: Instant) -> CaptureLevels {
        CaptureLevels {
            window_sequence: self.sequence,
            window_ms: self.ended.saturating_duration_since(self.started).as_secs_f64() * 1000.0,
            age_ms: now.saturating_duration_since(self.ended).as_millis() as u64,
            before_gain: self.before.levels(),
            after_gain: (self.after.frames > 0).then(|| self.after.levels()),
            wasapi_buffers: self.wasapi_buffers,
            wasapi_silent_buffers: self.wasapi_silent_buffers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn silence_full_scale_and_known_amplitude() {
        for (sample, expected) in [(0.0, None), (1.0, Some(0.0)), (0.5, Some(-6.020599913279624))] {
            let mut accumulator = Accumulator::default();
            accumulator.frame(&[sample, -sample]);
            let levels = accumulator.levels();
            assert_eq!(levels.peak_dbfs, expected);
            assert_eq!(levels.rms_dbfs, expected);
            assert_eq!(levels.zero_frame_fraction, if sample == 0.0 { 1.0 } else { 0.0 });
            if sample == 0.0 {
                assert_eq!(
                    serde_json::to_value(levels).unwrap()["peak_dbfs"],
                    serde_json::Value::Null
                );
            }
        }
    }
    #[test]
    fn zero_fraction_counts_stereo_frames_and_rms_counts_samples() {
        let mut accumulator = Accumulator::default();
        accumulator.frame(&[0.0, -0.0]);
        accumulator.frame(&[0.0, 1.0]);
        let levels = accumulator.levels();
        assert_eq!(levels.frames, 2);
        assert_eq!(levels.zero_frame_fraction, 0.5);
        assert_eq!(levels.peak_dbfs, Some(0.0));
        assert_eq!(levels.rms_dbfs, dbfs(0.5));
    }
    #[test]
    fn nonfinite_input_is_counted_without_nonfinite_json() {
        let mut accumulator = Accumulator::default();
        accumulator.frame(&[f32::NAN, f32::INFINITY]);
        let levels = accumulator.levels();
        assert_eq!(levels.nonfinite_samples, 2);
        assert_eq!(levels.zero_frame_fraction, 0.0);
        assert!(serde_json::to_value(levels).is_ok());
    }
}
