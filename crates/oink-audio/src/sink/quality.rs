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

/// One episode can have several reasons; twenty milliseconds of real media
/// closes it. Thresholds are diagnostic heuristics, not guarantees of audibility.
#[derive(Default)]
pub(crate) struct Interruptions {
    established: bool,
    previous: [f32; 2],
    previous_kind: OutputKind,
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
    pub fn frame(&mut self, at_ms: u64, rate: u32, kind: OutputKind, pcm: [f32; 2], counters: &DropoutCounters) {
        if kind == OutputKind::Intentional {
            self.reset();
            return;
        }
        let rate = u64::from(rate.max(1));
        // Only forced missing-output boundaries qualify. Natural PCM edges and
        // Opus PLC recovery are not a waveform click detector.
        let forced_edge = self.established
            && (kind == OutputKind::Missing) != (self.previous_kind == OutputKind::Missing)
            && pcm.iter().zip(self.previous).any(|(a, b)| (*a - b).abs() >= 0.01);
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
        if self.established && fresh != 0 {
            counters.record_quality(DropoutBucket {
                at_ms: *self.episode_at.get_or_insert(at_ms),
                interruptions: u64::from(!self.counted),
                silence_interruptions: u64::from(fresh & 1 != 0),
                concealment_interruptions: u64::from(fresh & 2 != 0),
                discontinuities: u64::from(fresh & 4 != 0),
                ..Default::default()
            });
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
    fn short_plc_smooth_adaptation_and_intentional_silence_are_not_interruptions() {
        let c = DropoutCounters::default();
        let mut t = Interruptions::default();
        run(&mut t, &c, OutputKind::Intentional, 48000, 0.0);
        run(&mut t, &c, OutputKind::Audio, 480, 0.2);
        run(&mut t, &c, OutputKind::Concealed, 960, 0.2);
        assert_eq!(total(&c), 0); // exactly twenty milliseconds is allowed
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
