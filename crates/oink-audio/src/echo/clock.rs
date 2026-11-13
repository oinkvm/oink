//! Robust local device-frame/host-time fit. Callback arrival never enters it.
use super::{Time, TimeQuality};
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clock {
    rate: f64,
    origin_frame: f64,
    origin_ns: f64,
    anchor_frame: f64,
    anchor_ns: f64,
    ns_per_frame: f64,
    samples: u32,
    pub residual_ns: f64,
    pub quality: TimeQuality,
}
impl Clock {
    pub fn new(rate: u32) -> Self {
        Self {
            rate: f64::from(rate),
            origin_frame: 0.0,
            origin_ns: 0.0,
            anchor_frame: 0.0,
            anchor_ns: 0.0,
            ns_per_frame: 1e9 / f64::from(rate),
            samples: 0,
            residual_ns: 0.0,
            quality: TimeQuality::Unknown,
        }
    }
    pub fn observe(&mut self, time: Time) -> Result<(), ()> {
        self.observe_rate(time, None)
    }
    pub fn observe_rate(&mut self, time: Time, native_step: Option<f64>) -> Result<(), ()> {
        let (Some(ns), Some(frame)) = (time.host_ns, time.native_frame) else {
            return Err(());
        };
        if !frame.is_finite() || ns == 0 || f64::from(time.rate) != self.rate || time.quality == TimeQuality::Unknown {
            return Err(());
        }
        let ns = ns as f64;
        if self.samples == 0 {
            self.origin_frame = frame;
            self.origin_ns = ns;
            self.anchor_frame = frame;
            self.anchor_ns = ns;
        } else {
            let residual = ns - self.host(frame);
            if residual.abs() > 10_000_000.0 || frame < self.anchor_frame {
                return Err(());
            }
            self.residual_ns = self.residual_ns * 0.98 + residual.abs() * 0.02;
            let span = frame - self.anchor_frame;
            // Long intervals prevent callback/clock-query jitter from becoming
            // a fast rate servo. Hardware-boundary stamps need less smoothing.
            if span >= self.rate {
                // Estimated stamps (desktop callback/padding presentation
                // estimates) carry callback jitter of several milliseconds and,
                // on WASAPI, a sawtooth of up to one engine period. Over one
                // second that alone reads as thousands of ppm. Measure such a
                // stamp against the fitted anchor instead of the previous raw
                // stamp and bound the step to the physical drift range, so the
                // slow servo averages jitter instead of discarding the epoch.
                // Gross timeline jumps are still rejected by the residual gate.
                let estimated = native_step.is_none() && time.quality != TimeQuality::HardwareBoundary;
                let nominal = 1e9 / self.rate;
                let measured = match native_step {
                    Some(step) => step,
                    None if estimated => ((ns - self.host(self.anchor_frame)) / span)
                        .clamp(nominal / (1.0 + 2_000e-6), nominal / (1.0 - 2_000e-6)),
                    None => (ns - self.anchor_ns) / span,
                };
                let ppm = (nominal / measured - 1.0) * 1e6;
                if ppm.abs() > 2_000.0 + 1e-6 {
                    return Err(());
                }
                let weight = if time.quality == TimeQuality::HardwareBoundary {
                    0.25
                } else {
                    0.05
                };
                let next = if native_step.is_some() {
                    measured
                } else {
                    self.ns_per_frame + (measured - self.ns_per_frame) * weight
                };
                // Pivot at this observation; do not shift old sample positions
                // when the slope changes. Only the reference is resampled later.
                self.origin_ns = self.host(frame) + residual * weight;
                self.origin_frame = frame;
                self.ns_per_frame = next;
                self.anchor_frame = frame;
                self.anchor_ns = ns;
            }
        }
        self.samples = self.samples.saturating_add(1);
        self.quality = time.quality;
        Ok(())
    }
    pub fn host(&self, frame: f64) -> f64 {
        self.origin_ns + (frame - self.origin_frame) * self.ns_per_frame
    }
    pub fn frame(&self, host_ns: f64) -> f64 {
        self.origin_frame + (host_ns - self.origin_ns) / self.ns_per_frame
    }
    pub fn native_step(&self) -> f64 {
        self.ns_per_frame
    }
    pub fn step_48k(&self) -> f64 {
        self.ns_per_frame * self.rate / 48_000.0
    }
    pub fn ppm(&self) -> f64 {
        (1e9 / self.rate / self.ns_per_frame - 1.0) * 1e6
    }
    pub fn known(&self) -> bool {
        self.samples > 0
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_device_drift_and_host_jump() {
        for ppm in [-1000.0, -700.0, -200.0, -50.0, 0.0, 50.0, 200.0, 700.0, 1000.0] {
            let mut model = Clock::new(48_000);
            let now = std::time::Instant::now();
            let step = 1e9 / (48_000.0 * (1.0 + ppm / 1e6));
            for block in 0..4000 {
                let frame = f64::from(block * 480);
                let stamp = Time {
                    device_clock: None,
                    host_ns: Some((1e12 + frame * step) as u64),
                    native_frame: Some(frame),
                    rate: 48_000,
                    quality: TimeQuality::HardwareBoundary,
                    discontinuity: false,
                    delivered_at: now,
                };
                model.observe(stamp).unwrap();
            }
            assert!((model.ppm() - ppm).abs() < 0.1, "{} {}", model.ppm(), ppm);
            assert!(model.residual_ns < 2000.0);
            assert!(
                model
                    .observe(Time {
                        device_clock: None,
                        host_ns: Some((1e12 + 1_920_000.0 * step + 20e6) as u64),
                        native_frame: Some(1_920_000.0),
                        rate: 48_000,
                        quality: TimeQuality::HardwareBoundary,
                        discontinuity: false,
                        delivered_at: now
                    })
                    .is_err()
            );
        }
    }
}
