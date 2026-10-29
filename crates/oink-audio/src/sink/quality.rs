//! Output provenance, not packet deadlines. No sample storage, locks or allocation.
use super::{DropoutBucket, DropoutCounters};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum OutputKind {
    #[default]
    Intentional,
    Audio,
    Concealed,
    Missing,
}

/// A causal five-millisecond splice: blend from the last emitted stereo frame
/// into the new signal. No lookahead, inserted frames, allocation, or reserve.
/// This is only applied at known provenance changes, never to normal PCM edges.
#[derive(Default)]
pub(crate) struct SpliceRamp {
    previous: Option<OutputKind>,
    last: [f32; 2],
    anchor: [f32; 2],
    remaining: usize,
    frames: usize,
}
impl SpliceRamp {
    /// Only repair future PCM after an observed native gap. Past output cannot
    /// be repaired; retain the actual last submitted frame as the splice anchor.
    pub(crate) fn interrupted(&mut self) {
        self.previous = Some(OutputKind::Missing);
    }

    pub(crate) fn apply(&mut self, pcm: &mut [f32; 2], kind: OutputKind, rate: u32) {
        if self.previous.is_some_and(|previous| {
            previous != kind && previous != OutputKind::Intentional && kind != OutputKind::Intentional
        }) {
            self.anchor = self.last;
            self.frames = (rate as usize * 5).div_ceil(1000).max(1);
            self.remaining = self.frames;
        }
        if kind == OutputKind::Intentional {
            self.remaining = 0;
        }
        if self.remaining > 0 {
            self.remaining -= 1;
            let old = self.remaining as f32 / self.frames as f32;
            for (sample, anchor) in pcm.iter_mut().zip(self.anchor) {
                *sample = anchor * old + *sample * (1.0 - old);
            }
        }
        self.last = *pcm;
        self.previous = Some(kind);
    }
}

/// One episode can have several reasons; twenty milliseconds of real media
/// closes it. Thresholds are diagnostic heuristics, not guarantees of audibility.
#[derive(Default)]
pub(crate) struct Interruptions {
    established: bool,
    previous: [f32; 2],
    previous_kind: OutputKind,
    boundary_followup: bool,
    boundary_counted: bool,
    missing_frames: u64,
    concealed_frames: u64,
    healthy_frames: u64,
    episode_at: Option<u64>,
    counted: bool,
    reasons: u8,
}
impl Interruptions {
    pub fn established(&self) -> bool {
        self.established
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    /// Native timeline absence, measured without inventing samples for the
    /// unobserved interval. Constant work even after a long device stall.
    pub fn gap(&mut self, at_ms: u64, rate: u32, frames: u64, counters: &DropoutCounters) -> DropoutBucket {
        if !self.established || frames == 0 {
            return DropoutBucket::default();
        }
        self.missing_frames = self.missing_frames.saturating_add(frames);
        self.healthy_frames = 0;
        self.concealed_frames = 0;
        self.previous_kind = OutputKind::Missing;
        self.boundary_followup = false;
        self.boundary_counted = false;
        let reached = u128::from(self.missing_frames) * 1000 >= u128::from(rate.max(1)) * 10;
        let fresh = reached && self.reasons & 1 == 0;
        let delta = DropoutBucket {
            at_ms: *self.episode_at.get_or_insert(at_ms),
            interruptions: u64::from(fresh && !self.counted),
            silence_interruptions: u64::from(fresh),
            missing_ns: frames.saturating_mul(1_000_000_000) / u64::from(rate.max(1)),
            ..Default::default()
        };
        if fresh {
            self.counted = true;
            self.reasons |= 1;
        }
        counters.record_quality(delta);
        delta
    }
    pub fn frame(
        &mut self,
        at_ms: u64,
        rate: u32,
        kind: OutputKind,
        pcm: [f32; 2],
        counters: &DropoutCounters,
    ) -> DropoutBucket {
        if kind == OutputKind::Intentional {
            self.reset();
            return DropoutBucket::default();
        }
        let rate = u64::from(rate.max(1));
        // Inspect engineered boundaries before their repair ramp. Short PLC
        // recovery can click even when it never meets the duration threshold.
        // Adjacent real-media samples are deliberately not a click detector.
        let boundary = kind != self.previous_kind && self.previous_kind != OutputKind::Intentional;
        if boundary {
            self.boundary_counted = false;
        }
        // Fractional interpolation may expose only a tiny amount of the next
        // span on its first sample. Inspect the following full sample too.
        let forced_edge = self.established
            && (boundary || self.boundary_followup)
            && !self.boundary_counted
            && pcm.iter().zip(self.previous).any(|(a, b)| (*a - b).abs() >= 0.01);
        self.boundary_counted |= forced_edge;
        self.boundary_followup = boundary;
        if kind == OutputKind::Audio {
            self.established = true;
            self.healthy_frames += 1;
        } else {
            self.healthy_frames = 0;
            if self.established {
                self.episode_at.get_or_insert(at_ms);
            }
        }
        self.missing_frames = if kind == OutputKind::Missing {
            self.missing_frames + 1
        } else {
            0
        };
        self.concealed_frames = if kind == OutputKind::Concealed {
            self.concealed_frames + 1
        } else {
            0
        };
        let reasons = u8::from(self.missing_frames * 1000 >= rate * 10)
            | (u8::from(self.concealed_frames * 1000 > rate * 20) << 1)
            | (u8::from(forced_edge) << 2);
        let fresh = reasons & !self.reasons;
        let mut delta = DropoutBucket::default();
        if self.established && (fresh != 0 || forced_edge) {
            delta = DropoutBucket {
                at_ms: *self.episode_at.get_or_insert(at_ms),
                interruptions: u64::from(!self.counted),
                silence_interruptions: u64::from(fresh & 1 != 0),
                concealment_interruptions: u64::from(fresh & 2 != 0),
                discontinuities: u64::from(forced_edge),
                ..Default::default()
            };
            counters.record_quality(delta);
            self.counted = true;
            self.reasons |= reasons;
        }
        self.previous = pcm;
        self.previous_kind = kind;
        if self.healthy_frames * 1000 >= rate * 20 {
            self.healthy_frames = rate; // bounded, still above the settle threshold
            self.episode_at = None;
            self.counted = false;
            self.reasons = 0;
        }
        delta
    }
    pub fn block(&mut self, at_ms: u64, kinds: &[OutputKind], pcm: &[f32], counters: &DropoutCounters) {
        let mut concealed = 0;
        let mut missing = 0;
        for (kind, frame) in kinds.iter().copied().zip(pcm.as_chunks::<2>().0) {
            concealed += u64::from(kind == OutputKind::Concealed);
            missing += u64::from(kind == OutputKind::Missing);
            self.frame(at_ms, 48_000, kind, *frame, counters);
        }
        counters.record_quality(DropoutBucket {
            at_ms,
            concealed_ns: concealed * 1_000_000_000 / 48_000,
            missing_ns: missing * 1_000_000_000 / 48_000,
            ..Default::default()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(t: &mut Interruptions, c: &DropoutCounters, kind: OutputKind, frames: usize, value: f32) {
        for _ in 0..frames {
            t.frame(1000, 48000, kind, [value; 2], c);
        }
    }
    fn total(c: &DropoutCounters) -> u64 {
        c.snapshot(1000).into_iter().flatten().map(|b| b.interruptions).sum()
    }
    #[test]
    fn fractional_boundary_repairs_and_counts_the_following_full_sample() {
        let mut ramp = SpliceRamp::default();
        let mut quality = Interruptions::default();
        let counters = DropoutCounters::default();
        let mut previous = 0.5;
        for (kind, raw) in [
            (OutputKind::Audio, 0.5),
            // A tiny fractional interpolation can be below the edge threshold.
            (OutputKind::Concealed, 0.4999),
            (OutputKind::Concealed, -0.5),
        ] {
            quality.frame(1000, 48000, kind, [raw; 2], &counters);
            let mut frame = [raw; 2];
            ramp.apply(&mut frame, kind, 48000);
            assert!((frame[0] - previous).abs() < 0.02);
            previous = frame[0];
        }
        assert_eq!(counters.snapshot(1000)[1].unwrap().discontinuities, 1);
    }

    #[test]
    fn every_engineered_splice_is_bounded_without_delaying_pcm() {
        for rate in [44_100, 48_000, 96_000] {
            let mut ramp = SpliceRamp::default();
            let frames = (rate as usize * 5).div_ceil(1000);
            let mut previous = [1.0, -1.0];
            for (kind, value) in [
                (OutputKind::Audio, 1.0),
                (OutputKind::Concealed, -1.0),
                (OutputKind::Audio, 1.0),
                (OutputKind::Missing, 0.0),
                (OutputKind::Audio, -1.0),
            ] {
                for _ in 0..frames {
                    let mut frame = [value, -value];
                    ramp.apply(&mut frame, kind, rate);
                    for channel in 0..2 {
                        assert!(
                            (frame[channel] - previous[channel]).abs() < 0.02,
                            "{kind:?} at {rate}: {previous:?} -> {frame:?}"
                        );
                    }
                    previous = frame;
                }
                assert_eq!(previous, [value, -value]);
            }
        }
    }

    #[test]
    fn short_plc_recovery_step_is_a_discontinuity() {
        let c = DropoutCounters::default();
        let mut t = Interruptions::default();
        run(&mut t, &c, OutputKind::Audio, 960, 0.2);
        run(&mut t, &c, OutputKind::Concealed, 120, 0.2);
        run(&mut t, &c, OutputKind::Audio, 1, -0.2);
        assert_eq!(total(&c), 1);
        assert_eq!(c.snapshot(1000)[1].unwrap().discontinuities, 1);
    }
    #[test]
    fn short_plc_smooth_adaptation_and_intentional_silence_are_not_interruptions() {
        let c = DropoutCounters::default();
        let mut t = Interruptions::default();
        run(&mut t, &c, OutputKind::Intentional, 48000, 0.0);
        run(&mut t, &c, OutputKind::Audio, 480, 0.2);
        run(&mut t, &c, OutputKind::Concealed, 960, 0.2);
        assert_eq!(total(&c), 0); // exactly twenty milliseconds is allowed
        run(&mut t, &c, OutputKind::Audio, 960, 0.2); // smooth PLC recovery
        run(&mut t, &c, OutputKind::Audio, 960, -0.2); // natural waveform edge
        run(&mut t, &c, OutputKind::Intentional, 48000, 0.0);
        assert_eq!(total(&c), 0);
    }
    #[test]
    fn thresholds_and_recovery_count_episodes_not_frames_or_reasons() {
        let c = DropoutCounters::default();
        let mut t = Interruptions::default();
        run(&mut t, &c, OutputKind::Audio, 960, 0.0);
        run(&mut t, &c, OutputKind::Missing, 479, 0.0);
        assert_eq!(total(&c), 0);
        run(&mut t, &c, OutputKind::Missing, 1, 0.0);
        assert_eq!(total(&c), 1);
        run(&mut t, &c, OutputKind::Audio, 959, 0.2);
        run(&mut t, &c, OutputKind::Concealed, 961, 0.2);
        assert_eq!(total(&c), 1); // not settled, same episode with another reason
        run(&mut t, &c, OutputKind::Audio, 960, 0.2);
        run(&mut t, &c, OutputKind::Concealed, 961, 0.2);
        assert_eq!(total(&c), 2);
    }
    #[test]
    fn forced_discontinuity_counts_immediately_but_fade_and_startup_do_not() {
        let c = DropoutCounters::default();
        let mut t = Interruptions::default();
        run(&mut t, &c, OutputKind::Missing, 960, 0.0);
        assert_eq!(total(&c), 0);
        run(&mut t, &c, OutputKind::Audio, 960, 0.2);
        run(&mut t, &c, OutputKind::Missing, 1, 0.199);
        assert_eq!(total(&c), 0);
        run(&mut t, &c, OutputKind::Audio, 960, 0.2);
        run(&mut t, &c, OutputKind::Missing, 1, 0.0);
        assert_eq!(total(&c), 1);
        run(&mut t, &c, OutputKind::Missing, 960, 0.0);
        assert_eq!(total(&c), 1);
    }
    #[test]
    fn observation_is_allocation_free_and_counts_rendered_duration() {
        let c = DropoutCounters::default();
        let mut t = Interruptions::default();
        crate::allocation_audit::assert_no_heap(|| {
            t.block(1000, &[OutputKind::Audio; 120], &[0.2; 240], &c);
            t.block(1000, &[OutputKind::Concealed; 120], &[0.2; 240], &c);
        });
        let b = c.snapshot(1000)[1].unwrap();
        assert_eq!(b.concealed_ns, 2_500_000);
        assert_eq!(b.interruptions, 0);
    }
}
