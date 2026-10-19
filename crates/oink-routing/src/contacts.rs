//! Native multi-finger samples and OS gestures are independent reliable streams.
//! Single-finger motion stays on the pointer path; contacts never synthesize it.
use oink_foundation::contacts::{Frame, Sample};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Ready,
    Single,
    Raw,
    /// A boundary or capture fault cannot transfer a partially completed touch.
    AwaitLift,
}

#[derive(Debug, Default)]
pub struct Source {
    mode: Mode,
    sample: Sample,
    pub transitions: u64,
    pub raw_frames: u64,
    pub gestures: u64,
    pub cancelled: u64,
}
impl Source {
    pub fn mode(&self) -> Mode {
        self.mode
    }
    fn set(&mut self, mode: Mode) {
        if self.mode != mode {
            self.transitions += 1;
            self.mode = mode;
        }
    }
    pub fn ignore_sample(&mut self, sample: Sample) {
        if sample.valid() {
            self.sample = sample;
            if sample.len == 0 {
                self.set(Mode::Ready);
            }
        }
    }
    pub fn reset(&mut self, mut emit: impl FnMut(Frame)) {
        if self.mode == Mode::Raw {
            emit(Frame::cancel(self.sample.device));
            self.cancelled += 1;
        }
        self.set(Mode::AwaitLift);
    }
    pub fn reconfigure(&mut self, emit: impl FnMut(Frame)) {
        self.reset(emit);
        if self.sample.len == 0 {
            self.set(Mode::Ready);
        }
    }
    pub fn sample(&mut self, sample: Sample, mut emit: impl FnMut(Frame)) {
        if !sample.valid() {
            self.reset(emit);
            return;
        }
        if self.sample.device != 0
            && (sample.device != self.sample.device
                || sample.surface != self.sample.surface
                || sample.time_us < self.sample.time_us)
        {
            self.reset(&mut emit);
            self.sample = sample;
            return;
        }
        let previous = self.sample;
        self.sample = sample;
        if self.mode == Mode::Raw {
            emit(Frame::between(Some(&previous), &sample));
            self.raw_frames += 1;
        } else if self.mode != Mode::AwaitLift && sample.len >= 2 {
            // Begin at the first multi-finger frame, without replaying the
            // single-finger prelude or waiting for the OS recognizer to choose.
            emit(Frame::between(None, &sample));
            self.raw_frames += 1;
            self.gestures += 1;
            self.set(Mode::Raw);
        } else if self.mode != Mode::AwaitLift && sample.len == 1 {
            self.set(Mode::Single);
        }
        if sample.len == 0 {
            self.set(Mode::Ready);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::contacts::{Contact, Phase, Surface};
    pub(super) fn sample(time_us: u64, len: u8) -> Sample {
        Sample {
            device: 1,
            time_us,
            surface: Surface {
                width: 15600,
                height: 9600,
            },
            len,
            contacts: std::array::from_fn(|i| Contact {
                id: i as u32 + 1,
                x: 1000,
                y: 2000,
            }),
        }
    }
    #[test]
    fn multi_finger_begin_keeps_every_frame_until_lift() {
        let mut source = Source::default();
        let mut frames = Vec::new();
        for (i, n) in [1, 2, 2, 3, 1, 0].into_iter().enumerate() {
            source.sample(sample(i as u64 * 8000, n), |f| frames.push(f));
        }
        assert_eq!(frames.len(), 5);
        assert_eq!(frames[0].phase, Phase::Begin);
        assert_eq!(frames[0].time_us, 8000);
        assert_eq!(frames.last().unwrap().phase, Phase::End);
        assert_eq!(source.mode(), Mode::Ready);
    }
    #[test]
    fn cancelled_touch_waits_for_full_lift() {
        let mut source = Source::default();
        let mut frames = Vec::new();
        source.sample(sample(0, 2), |f| frames.push(f));
        source.reconfigure(|f| frames.push(f));
        source.sample(sample(8000, 3), |_| panic!("partial touch"));
        source.sample(sample(16000, 1), |_| panic!("partial touch"));
        source.sample(sample(24000, 0), |_| panic!("retired touch"));
        source.sample(sample(32000, 2), |f| frames.push(f));
        assert_eq!(
            frames.iter().map(|f| f.phase).collect::<Vec<_>>(),
            [Phase::Begin, Phase::Cancel, Phase::Begin]
        );
    }
    #[test]
    fn corrupt_frame_cancels_without_forwarding_partial_contacts() {
        let mut source = Source::default();
        source.sample(sample(0, 5), |_| {});
        let mut frames = Vec::new();
        source.sample(sample(8000, 255), |f| frames.push(f));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].phase, Phase::Cancel);
        assert_eq!(source.mode(), Mode::AwaitLift);
    }
}
