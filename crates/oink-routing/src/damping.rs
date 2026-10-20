//! Platform-independent damping: normalized distance, sampled velocity/acceleration and rules.
//!
//! Callers choose WHERE a gesture applies and normalize displacement to screen diagonals.
//! This module defines WHAT must accumulate. Gesture lifetime, geometry, input ownership and
//! platform behavior deliberately stay with the caller. Timestamps are monotonic microseconds.
use oink_foundation::DeviceId;
use oink_foundation::input::ScreenEdge;
use serde::{Deserialize, Serialize};

/// Crossing rule. DepthOnly compares the threshold literally; EnergyDepth
/// uses bounded defaults for invalid numeric parameters.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DampingProfile {
    EnergyDepth {
        energy: f64,
        acceleration: f64,
        max_weight: f64,
        max_depth: f64,
    },
    DepthOnly {
        depth: f64,
    },
}

impl Default for DampingProfile {
    fn default() -> Self {
        Self::EnergyDepth {
            energy: 0.024,
            acceleration: 7.3,
            max_weight: 3.0,
            max_depth: 0.08,
        }
    }
}

/// A completed normalized sample. The sampler owns time windows; profiles own the response.
#[derive(Clone, Copy, Debug)]
pub struct DampingSample {
    pub depth: f64,
    pub velocity: f64,
    pub acceleration: f64,
    pub duration: f64,
    pub has_outward_motion: bool,
}

impl DampingProfile {
    /// Empty bins do not integrate the smoothing tail. DepthOnly needs no energy.
    pub fn energy_increment(self, sample: DampingSample) -> f64 {
        match self {
            Self::EnergyDepth {
                acceleration,
                max_weight,
                ..
            } if sample.has_outward_motion => {
                let weight =
                    (1.0 + sample.acceleration / positive(acceleration, 7.3)).clamp(0.0, positive(max_weight, 3.0));
                sample.velocity * sample.velocity * weight * sample.duration
            }
            _ => 0.0,
        }
    }

    pub fn crossed(self, depth: f64, energy: f64) -> bool {
        match self {
            Self::EnergyDepth {
                energy: threshold,
                max_depth,
                ..
            } => depth >= positive(max_depth, 0.08) || energy >= nonnegative(threshold, 0.024),
            Self::DepthOnly { depth: threshold } => depth >= threshold,
        }
    }
}

/// An untimed depth accumulator for policies such as dragging. It shares the DepthOnly rule
/// without imposing the edge meter's sampling or gesture-reset semantics.
#[derive(Clone, Copy, Debug, Default)]
pub struct DampingDepth(f64);

impl DampingDepth {
    pub fn new(depth: f64) -> Self {
        Self(depth)
    }
    pub fn push(&mut self, delta: f64) {
        self.0 += delta;
    }
    pub fn crossed(self, threshold: f64) -> bool {
        DampingProfile::DepthOnly { depth: threshold }.crossed(self.0, 0.0)
    }
}

/// WHERE: a directed departing-device → receiving-device edge interval. Missing selectors
/// match all devices/displays. Fractions run top→bottom for vertical edges and left→right for
/// horizontal edges, inclusive. Invalid intervals never match. Input device is not the source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DampingSegment {
    pub source: Option<DeviceId>,
    pub target: Option<DeviceId>,
    pub display: Option<String>,
    pub edge: ScreenEdge,
    pub start: f64,
    pub end: f64,
}

impl DampingSegment {
    pub(crate) fn matches(
        &self,
        source: DeviceId,
        target: DeviceId,
        display: &str,
        edge: ScreenEdge,
        fraction: f64,
    ) -> bool {
        self.source.is_none_or(|s| s == source)
            && self.target.is_none_or(|t| t == target)
            && self.display.as_ref().is_none_or(|d| d == display)
            && self.edge == edge
            && self.start.is_finite()
            && self.end.is_finite()
            && self.start >= 0.0
            && self.end <= 1.0
            && self.start <= self.end
            && fraction >= self.start
            && fraction <= self.end
    }
}

/// Binds a location to a rule; callers may reuse an identical profile on any number of segments.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DampingRule {
    pub segment: DampingSegment,
    pub profile: DampingProfile,
}

/// Advanced configuration; no UI. First matching manual rule overrides the platform
/// rule, on any OS. Without a manual match, platform regions retain their existing behavior.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DampingConfig {
    pub platform_profile: DampingProfile,
    pub rules: Vec<DampingRule>,
}

const BIN_US: u64 = 10_000;
const DT: f64 = 0.01;

#[derive(Clone, Debug)]
pub struct DampingMeter {
    depth: f64,
    energy: f64,
    bin: u64,
    distance: f64,
    velocities: [f64; 3],
    accelerations: [f64; 3],
    velocity: f64,
}

impl DampingMeter {
    pub fn new(now: u64) -> Self {
        Self {
            depth: 0.0,
            energy: 0.0,
            bin: now / BIN_US,
            distance: 0.0,
            velocities: [0.0; 3],
            accelerations: [0.0; 3],
            velocity: 0.0,
        }
    }

    /// Current net depth, in screen diagonals.
    pub fn depth(&self) -> f64 {
        self.depth
    }

    /// Accumulated normalized energy.
    pub fn energy(&self) -> f64 {
        self.energy
    }

    /// Move inward without contributing velocity or energy; callers own gesture reset policy.
    pub fn retreat(&mut self, du: f64) {
        self.depth -= du;
    }

    fn finish_bin(&mut self, profile: DampingProfile) {
        self.velocities.rotate_right(1);
        self.velocities[0] = self.distance / DT;
        let v = self.velocities.iter().sum::<f64>() / 3.0;
        self.accelerations.rotate_right(1);
        self.accelerations[0] = (v - self.velocity) / DT;
        let a = self.accelerations.iter().sum::<f64>() / 3.0;
        self.energy += profile.energy_increment(DampingSample {
            depth: self.depth,
            velocity: v,
            acceleration: a,
            duration: DT,
            has_outward_motion: self.distance > 0.0,
        });
        self.velocity = v;
        self.distance = 0.0;
    }

    /// Add a nonnegative outward displacement in screen diagonals. Use retreat for inward motion.
    pub fn push(&mut self, du: f64, now: u64, profile: DampingProfile) -> bool {
        let bin = now / BIN_US;
        let elapsed = bin.saturating_sub(self.bin);
        // After six empty bins both finite windows are zero; bound work after arbitrarily long idle.
        for _ in 0..elapsed.min(7) {
            self.finish_bin(profile);
        }
        self.bin = self.bin.max(bin);
        self.depth += du;
        self.distance += du;
        profile.crossed(self.depth, self.energy)
    }
}

fn positive(value: f64, default: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        default
    }
}
pub(crate) fn nonnegative(value: f64, default: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RoutingSettings;

    #[test]
    fn accelerated_push_crosses_shallow_but_braking_does_not() {
        let cfg = RoutingSettings::default();
        let mut fast = DampingMeter::new(0);
        let mut crossed = false;
        for i in 0..20 {
            crossed = fast.push((i + 1) as f64 * 0.0015, i * BIN_US, cfg.edge_damping_profile());
            if crossed {
                break;
            }
        }
        assert!(crossed);
        assert!(fast.depth < 0.06, "depth = {}", fast.depth);
        let mut slow = DampingMeter::new(0);
        for i in 0..50 {
            assert!(!slow.push(0.0005 * (1.0 - i as f64 / 50.0), i * BIN_US, cfg.edge_damping_profile()));
        }
        let energy = slow.energy;
        assert!(!slow.push(0.0, 20_000_000, cfg.edge_damping_profile()));
        assert!(slow.energy < 0.024);
        assert!(slow.energy >= energy); // no decay
    }

    #[test]
    fn high_rate_jitter_and_duplicate_timestamps_do_not_amplify_energy() {
        let cfg = RoutingSettings {
            damping: DampingConfig {
                platform_profile: DampingProfile::EnergyDepth {
                    energy: 100.0,
                    acceleration: 7.3,
                    max_weight: 3.0,
                    max_depth: 10.0,
                },
                ..Default::default()
            },
            ..RoutingSettings::default()
        };
        let mut low = DampingMeter::new(0);
        let mut high = DampingMeter::new(0);
        for bin in 0..20 {
            let distance = (bin + 1) as f64 * 0.0001;
            low.push(distance, bin * BIN_US, cfg.edge_damping_profile());
            for event in 0..10 {
                // Bursts, repeated timestamps, and variable intra-bin spacing still sum identically.
                high.push(
                    distance / 10.0,
                    bin * BIN_US + event / 2 * 1700,
                    cfg.edge_damping_profile(),
                );
            }
        }
        low.push(0.0, 20 * BIN_US, cfg.edge_damping_profile());
        high.push(0.0, 20 * BIN_US, cfg.edge_damping_profile());
        assert!((low.depth - high.depth).abs() < 1e-12);
        assert!((low.energy - high.energy).abs() < 1e-12);
    }

    #[test]
    fn depth_alone_always_crosses_and_invalid_settings_are_safe() {
        let cfg = RoutingSettings {
            damping: DampingConfig {
                platform_profile: DampingProfile::EnergyDepth {
                    energy: f64::MAX,
                    acceleration: 7.3,
                    max_weight: 3.0,
                    max_depth: 0.08,
                },
                ..Default::default()
            },
            ..RoutingSettings::default()
        };
        let mut m = DampingMeter::new(0);
        assert!(!m.push(0.079, 0, cfg.edge_damping_profile()));
        assert!(m.push(0.0011, 0, cfg.edge_damping_profile()));
        let invalid = RoutingSettings {
            damping: DampingConfig {
                platform_profile: DampingProfile::EnergyDepth {
                    energy: f64::NAN,
                    acceleration: 0.0,
                    max_weight: -1.0,
                    max_depth: f64::INFINITY,
                },
                ..Default::default()
            },
            ..RoutingSettings::default()
        };
        let mut m = DampingMeter::new(0);
        assert!(!m.push(0.01, 0, invalid.edge_damping_profile()));
        assert!(!m.push(0.01, BIN_US, invalid.edge_damping_profile()));
        assert!(m.push(0.061, 2 * BIN_US, invalid.edge_damping_profile()));
    }
    #[test]
    fn sampled_profile_and_retreat_preserve_golden_arithmetic() {
        // Repeated timestamps, bin boundaries, braking, inward motion and idle bins.
        let cfg = RoutingSettings {
            damping: DampingConfig {
                platform_profile: DampingProfile::EnergyDepth {
                    energy: 0.031,
                    acceleration: 4.5,
                    max_weight: 2.4,
                    max_depth: 0.12,
                },
                ..Default::default()
            },
            ..RoutingSettings::default()
        };
        let trace: [(f64, u64, f64, f64, bool); 12] = [
            (0.002, 4300, 0.002, 0.0, false),
            (0.003, 4300, 0.005, 0.0, false),
            (0.005, 9800, 0.01, 0.0, false),
            (0.007, 10200, 0.017, 0.0026666666666666666, false),
            (0.001, 20700, 0.018000000000000002, 0.010373333333333332, false),
            (-0.004, 20700, 0.014000000000000002, 0.010373333333333332, false),
            (0.003, 30900, 0.017, 0.019013333333333333, false),
            (0.002, 40500, 0.019000000000000003, 0.02068973936899863, false),
            (0.001, 70000, 0.020000000000000004, 0.02068973936899863, false),
            (0.0, 90000, 0.020000000000000004, 0.02068973936899863, false),
            (0.004, 20000000, 0.024000000000000004, 0.02068973936899863, false),
            (0.1, 20010000, 0.12400000000000001, 0.02104310013717421, true),
        ];
        let mut meter = DampingMeter::new(4300);
        for (du, now, depth, energy, crossed) in trace {
            if du < 0.0 {
                meter.retreat(-du);
            } else {
                assert_eq!(meter.push(du, now, cfg.edge_damping_profile()), crossed, "at {now}");
            }
            assert_eq!(meter.depth().to_bits(), depth.to_bits(), "depth at {now}");
            assert_eq!(meter.energy().to_bits(), energy.to_bits(), "energy at {now}");
        }
    }

    #[test]
    fn depth_only_has_no_energy_and_compares_threshold_literally() {
        let profile = DampingProfile::DepthOnly { depth: 0.024 };
        let mut meter = DampingMeter::new(0);
        let mut depth = DampingDepth::default();
        for (du, now, crossed) in [(0.012, 0, false), (0.0, 1_000_000, false), (0.012, 1_000_001, true)] {
            depth.push(du);
            assert_eq!(depth.crossed(0.024), crossed);
            assert_eq!(meter.push(du, now, profile), crossed);
            assert_eq!(meter.energy(), 0.0);
        }
        assert!(depth.crossed(0.0));
        assert!(depth.crossed(-1.0));
        assert!(!depth.crossed(f64::NAN));
        assert!(!depth.crossed(f64::INFINITY));
        assert_eq!(meter.distance, 0.012, "complete bins reset even for depth-only");
    }
}
