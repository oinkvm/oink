//! Platform neutral native audio callback boundaries.
//!
//! Android AAudio and iOS AVAudioEngine use this seam for the parts that must
//! remain identical: one callback owner per direction, bounded float PCM,
//! generation fencing, and hardware first frame timestamps.  Codec, claims,
//! network control and the device-local AEC policy stay with the Rust owner.

use crate::handoff;
use std::{
    cell::UnsafeCell,
    ffi::c_void,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
};

pub const MAX_FRAMES: usize = 4096;

/// The callback is installed by the ordinary owner before native I/O starts.
/// It must only touch the supplied PCM buffer and return; it must not enter a
/// foreign runtime, allocate, lock, or perform network work.
pub type RenderCallback = unsafe extern "C" fn(*mut c_void, *mut f32, u32, u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
    pub generation: u64,
}

impl Format {
    pub fn validate(self) -> Result<(), String> {
        if !(8_000..=192_000).contains(&self.rate) {
            return Err("native audio rate must be 8–192 kHz".into());
        }
        if !(1..=2).contains(&self.channels) {
            return Err("native audio must be mono or stereo".into());
        }
        if self.generation == 0 {
            return Err("native audio generation must be nonzero".into());
        }
        Ok(())
    }
}

struct OutputControl {
    active: AtomicBool,
    callback: AtomicUsize,
    owner: AtomicPtr<c_void>,
}

/// A native output handle with an explicit generation fence. Native code owns
/// the I/O stream; the Rust owner owns this object until callbacks quiesce.
pub struct Output {
    format: Format,
    control: OutputControl,
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobileNativeOutput")
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl Output {
    pub fn new(format: Format) -> Result<Self, String> {
        format.validate()?;
        Ok(Self {
            format,
            control: OutputControl {
                active: AtomicBool::new(true),
                callback: AtomicUsize::new(0),
                owner: AtomicPtr::new(std::ptr::null_mut()),
            },
        })
    }

    pub fn format(&self) -> Format {
        self.format
    }

    /// Install or replace the playback pull callback before starting/rebuilding
    /// native I/O. Replacing it is an ordinary-thread operation.
    pub fn set_renderer(&self, owner: *mut c_void, callback: Option<RenderCallback>) {
        self.control.owner.store(owner, Ordering::Release);
        self.control
            .callback
            .store(callback.map_or(0, |f| f as usize), Ordering::Release);
    }

    /// Fill the complete native buffer. Zeroing first preserves the silence
    /// guarantee if the callback is absent, stale, or rejects the generation.
    pub fn pull(&self, samples: &mut [f32], frames: usize, host_ns: u64) {
        samples.fill(0.0);
        if !self.control.active.load(Ordering::Acquire)
            || frames > MAX_FRAMES
            || frames.saturating_mul(usize::from(self.format.channels)) > samples.len()
        {
            return;
        }
        let callback = self.control.callback.load(Ordering::Acquire);
        let owner = self.control.owner.load(Ordering::Acquire);
        if callback == 0 {
            return;
        }
        // SAFETY: the owner installed a valid C callback and keeps it alive for
        // the whole native stream; the callback receives the pre-zeroed extent.
        let callback: RenderCallback = unsafe { std::mem::transmute(callback) };
        // SAFETY: the registration contract keeps the owner and callback live
        // until native I/O has quiesced; revoke is followed by that quiescence
        // before the Rust handle is destroyed. The bounds check above proves
        // samples.as_mut_ptr() points to at least frames * channels writable
        // float slots, and the callback receives only that validated extent.
        unsafe { callback(owner, samples.as_mut_ptr(), frames as u32, host_ns) };
    }

    pub fn revoke(&self) {
        self.control.active.store(false, Ordering::Release);
        self.control.callback.store(0, Ordering::Release);
        self.control.owner.store(std::ptr::null_mut(), Ordering::Release);
    }

    pub fn active(&self) -> bool {
        self.control.active.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Frame {
    samples: [f32; 2],
    host_ns: u64,
    discontinuity: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputBatch {
    pub frames: usize,
    pub host_ns: u64,
    pub discontinuity: bool,
}

/// Capture callback handoff. The callback copies at most `MAX_FRAMES` into a
/// preallocated SPSC queue; the ordinary worker drains it and feeds the core's
/// encoder/AEC path. A full queue marks the next batch discontinuous.
pub struct Input {
    format: Format,
    // The producer and consumer are each touched by exactly one native owner:
    // push runs on the I/O callback and drain runs on the bounded worker. They
    // are kept in separate cells so the two sides can operate concurrently
    // without a mutex on the callback path.
    producer: UnsafeCell<handoff::Producer<Frame>>,
    consumer: UnsafeCell<handoff::Consumer<Frame>>,
    active: AtomicBool,
    discontinuity: AtomicBool,
    batch_limit: usize,
}

// SAFETY: producer and consumer are distinct SPSC endpoints. The producer is
// exclusively called by the serialized native callback and the consumer by the
// worker; all shared lifecycle state is atomic.
unsafe impl Sync for Input {}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobileNativeInput")
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl Input {
    pub fn new(format: Format) -> Result<Self, String> {
        format.validate()?;
        let initial = Frame::default();
        let capacity = (format.rate as usize / 25).max(2);
        let (producer, consumer) = handoff::channel(capacity, initial);
        Ok(Self {
            format,
            producer: UnsafeCell::new(producer),
            consumer: UnsafeCell::new(consumer),
            active: AtomicBool::new(true),
            discontinuity: AtomicBool::new(false),
            batch_limit: (format.rate as usize / 50).max(1),
        })
    }

    pub fn format(&self) -> Format {
        self.format
    }

    /// Called only by the one serialized native capture callback.
    pub fn push(&self, samples: &[f32], frames: usize, host_ns: u64) -> usize {
        let channels = usize::from(self.format.channels);
        if !self.active.load(Ordering::Acquire)
            || frames > MAX_FRAMES
            || samples.len() < frames.saturating_mul(channels)
        {
            return 0;
        }
        let mut queued = 0;
        // SAFETY: the native callback is the sole producer owner.
        let producer = unsafe { &mut *self.producer.get() };
        for frame in 0..frames {
            let first = samples[frame * channels];
            let second = if channels == 2 {
                samples[frame * channels + 1]
            } else {
                first
            };
            let time = host_ns.saturating_add(frame as u64 * 1_000_000_000 / u64::from(self.format.rate));
            let value = Frame {
                samples: [first, second],
                host_ns: time,
                discontinuity: self.discontinuity.load(Ordering::Acquire),
            };
            match producer.push(value) {
                Ok(()) => {
                    self.discontinuity.store(false, Ordering::Release);
                    queued += 1;
                }
                Err(_) => self.discontinuity.store(true, Ordering::Release),
            }
        }
        queued
    }

    /// Drain at most one bounded 20 ms batch into caller-owned scratch.
    pub fn drain(&self, output: &mut [f32]) -> InputBatch {
        let channels = usize::from(self.format.channels);
        let capacity = (output.len() / channels).min(self.batch_limit);
        let mut result = InputBatch {
            discontinuity: self.discontinuity.swap(false, Ordering::AcqRel),
            ..InputBatch::default()
        };
        // SAFETY: the worker is the sole consumer owner.
        let consumer = unsafe { &mut *self.consumer.get() };
        while result.frames < capacity {
            let Some(frame) = consumer.pop() else { break };
            if result.frames == 0 {
                result.host_ns = frame.host_ns;
                result.discontinuity |= frame.discontinuity;
            }
            output[result.frames * channels] = frame.samples[0];
            if channels == 2 {
                output[result.frames * channels + 1] = frame.samples[1];
            }
            result.frames += 1;
        }
        result
    }

    pub fn revoke(&self) {
        self.active.store(false, Ordering::Release);
    }
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static LAST_HOST: AtomicU64 = AtomicU64::new(0);
    unsafe extern "C" fn render(owner: *mut c_void, samples: *mut f32, frames: u32, host_ns: u64) {
        LAST_HOST.store(host_ns, Ordering::Release);
        // SAFETY: test callback receives the exact configured extent.
        unsafe { std::slice::from_raw_parts_mut(samples, frames as usize * 2).fill(0.25) };
        assert!(!owner.is_null());
    }

    #[test]
    fn output_callback_receives_timestamp_and_revoke_is_silent() {
        let output = Output::new(Format {
            rate: 48_000,
            channels: 2,
            generation: 7,
        })
        .unwrap();
        let mut samples = [1.0; 480];
        let mut owner = 1_u8;
        output.set_renderer((&mut owner as *mut u8).cast(), Some(render));
        output.pull(&mut samples, 240, 123);
        assert_eq!(LAST_HOST.load(Ordering::Acquire), 123);
        assert!(samples.iter().all(|sample| *sample == 0.25));
        output.revoke();
        samples.fill(1.0);
        output.pull(&mut samples, 240, 456);
        assert!(samples.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn input_batches_preserve_first_host_and_mark_overflow() {
        let format = Format {
            rate: 48_000,
            channels: 1,
            generation: 9,
        };
        let input = Input::new(format).unwrap();
        let samples = vec![0.5; 48_000 / 25 + 20];
        assert!(input.push(&samples, samples.len(), 1_000) < samples.len());
        let mut out = [0.0; 960];
        let first = input.drain(&mut out);
        assert!(first.frames > 0);
        assert_eq!(first.host_ns, 1_000);
        assert!(first.discontinuity || input.drain(&mut out).discontinuity);
    }

    #[test]
    fn invalid_format_and_generation_are_rejected() {
        assert!(
            Format {
                rate: 0,
                channels: 1,
                generation: 1
            }
            .validate()
            .is_err()
        );
        assert!(
            Format {
                rate: 48_000,
                channels: 3,
                generation: 1
            }
            .validate()
            .is_err()
        );
        assert!(
            Format {
                rate: 48_000,
                channels: 1,
                generation: 0
            }
            .validate()
            .is_err()
        );
    }
}
