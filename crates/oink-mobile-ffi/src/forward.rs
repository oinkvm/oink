//! Platform output capture, using the desktop source encoder and M1 packet path.
//! Push runs on a serialized native worker (never the hardware callback).
use crate::audio;
use oink_audio::{media, microphone::resample::Resampler, mobile_native::Format, negotiation::Profile, source};
use oink_foundation::snapshot::Snapshot;
use oink_network::link::DatagramSend;
use std::{
    ptr, slice,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[derive(Clone)]
pub(crate) struct ForwardRoute {
    pub stream: u64,
    pub sender: DatagramSend,
}

struct Worker {
    encoder: source::Encoder,
    resamplers: [Resampler; 2],
    /// Hardware timestamp anchor detects missing or out-of-order native frames.
    last_ns: u64,
    position: u64,
}

#[repr(C)]
pub struct OinkAudioForward {
    pub(crate) format: Format,
    active: AtomicBool,
    route: Arc<Snapshot<Option<ForwardRoute>>>,
    worker: Mutex<Worker>,
    controls: Mutex<source::Encoded>,
}

impl OinkAudioForward {
    pub(crate) fn revoke(&self) {
        self.active.store(false, Ordering::Release);
        self.route.publish(None);
    }
    pub(crate) fn set_route(&self, route: Option<ForwardRoute>) {
        self.route.publish(route);
    }
    pub(crate) fn set_gain(&self, gain: u16, muted: bool) {
        let control = self.controls.lock().expect("forward controls");
        control.set_percent(gain);
        control.set_muted(muted);
    }

    fn push(&self, samples: &[f32], frames: u32, host_ns: u64, discontinuity: bool) -> u32 {
        if !self.active.load(Ordering::Acquire) {
            return 0;
        }
        let mut worker = self.worker.lock().expect("forward worker");
        if !self.active.load(Ordering::Acquire) {
            return 0;
        }
        // AudioRecord/ReplayKit timestamp fits can drift between callbacks;
        // delivery spacing does not prove missing PCM. The platform reports
        // loss explicitly. Only an actual host-clock reset adds a discontinuity.
        let reset = host_ns != 0 && worker.last_ns != 0 && host_ns < worker.last_ns;
        let discontinuity = discontinuity || reset;
        worker.last_ns = host_ns;
        if discontinuity {
            for resampler in &mut worker.resamplers {
                resampler.reset();
            }
        }
        // No route means no encoding or network traffic (e.g. iOS awaiting an exit).
        if self.route.load().is_none() {
            return frames;
        }
        let mut pcm = Vec::with_capacity(frames as usize * 2 * 48_000 / self.format.rate as usize + 4);
        for frame in samples.chunks_exact(self.format.channels as usize) {
            let mut left = [0.0; 8];
            let mut count = 0;
            worker.resamplers[0].push(frame[0], |value| {
                left[count] = value;
                count += 1;
            });
            let mut index = 0;
            worker.resamplers[1].push(frame[frame.len() - 1], |value| {
                pcm.extend_from_slice(&[left[index], value]);
                index += 1;
            });
        }
        let now = Instant::now();
        let position = worker.position;
        worker.position += (pcm.len() / 2) as u64;
        // ReplayKit commonly delivers 1024-frame buffers (>20 ms). Feed
        // contiguous codec blocks so the desktop encoder's bounded software
        // backlog policy does not mistake a native burst for queued old audio.
        for (offset, chunk) in pcm.chunks(480).enumerate() {
            worker.encoder.supply(
                chunk,
                source::Stamp {
                    first_sample_at: None,
                    callback_at: None,
                    delivered_at: now,
                    source_frame_position: Some(position + (offset * 240) as u64),
                    discontinuity: discontinuity && offset == 0,
                },
            );
        }
        frames
    }
}

/// Control thread creates one fixed-format handle per native generation.
#[unsafe(no_mangle)]
pub extern "C" fn oink_audio_forward_create(rate: u32, channels: u16, generation: u64) -> *mut OinkAudioForward {
    let format = Format {
        rate,
        channels,
        generation,
    };
    if format.validate().is_err() {
        return ptr::null_mut();
    }
    let Ok((encoder, controls)) = source::channel(Profile::Opus5ms.config(), 100, false) else {
        return ptr::null_mut();
    };
    let route = Arc::new(Snapshot::<Option<ForwardRoute>>::default());
    let published = route.clone();
    let started = Instant::now();
    controls.set_packet_writer(Arc::new(move |packet| {
        // Hardware can rebuild while the exit keeps the same prepared stream.
        // As for microphone capture, never reuse a burst in a native generation.
        let mut packet = *packet;
        packet.burst = (generation << 32) | packet.burst.min(u64::from(u32::MAX));
        let route = published.load();
        let Some(route) = route.as_ref() else {
            return false;
        };
        let mut bytes = [0; media::FRAME_CAPACITY];
        media::encode(&packet, route.stream, started, &mut bytes)
            .is_ok_and(|len| route.sender.post(&bytes[..len]).is_ok())
    }));
    let handle = Arc::new(OinkAudioForward {
        format,
        active: AtomicBool::new(true),
        route,
        worker: Mutex::new(Worker {
            encoder,
            resamplers: [Resampler::new(rate), Resampler::new(rate)],
            last_ns: 0,
            position: 0,
        }),
        controls: Mutex::new(controls),
    });
    audio::register_forward(&handle);
    Arc::into_raw(handle).cast_mut()
}

/// # Safety
/// Pointer and samples remain valid for the call; stop/join pushes before destroy.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_forward_push(
    handle: *mut OinkAudioForward,
    samples: *const f32,
    frames: u32,
    host_ns: u64,
    discontinuity: u8,
) -> u32 {
    // SAFETY: caller retains the created handle until all native workers join.
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return 0;
    };
    if samples.is_null() || frames == 0 || frames > 4096 {
        return 0;
    }
    // SAFETY: caller supplies interleaved storage for these validated frames/channels.
    let samples = unsafe { slice::from_raw_parts(samples, frames as usize * handle.format.channels as usize) };
    handle.push(samples, frames, host_ns, discontinuity != 0)
}

/// # Safety
/// Same lifetime and serialization requirements as `oink_audio_forward_push`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_forward_push_i16(
    handle: *mut OinkAudioForward,
    samples: *const i16,
    frames: u32,
    host_ns: u64,
    discontinuity: u8,
) -> u32 {
    // SAFETY: caller retains the created handle until all native workers join.
    let Some(handle) = (unsafe { handle.as_ref() }) else {
        return 0;
    };
    if samples.is_null() || frames == 0 || frames > 4096 {
        return 0;
    }
    // SAFETY: caller supplies interleaved storage for these validated frames/channels.
    let samples = unsafe { slice::from_raw_parts(samples, frames as usize * handle.format.channels as usize) };
    let samples: Vec<_> = samples.iter().map(|value| f32::from(*value) / 32768.0).collect();
    handle.push(&samples, frames, host_ns, discontinuity != 0)
}

/// # Safety
/// The handle must still be alive; revocation may overlap a push.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_forward_revoke(handle: *mut OinkAudioForward) {
    // SAFETY: revocation requires a live handle and may overlap native pushes.
    if let Some(handle) = unsafe { handle.as_ref() } {
        handle.revoke();
    }
}

/// # Safety
/// Native capture and pushes must be stopped and joined, exactly once per handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_forward_destroy(handle: *mut OinkAudioForward) {
    if !handle.is_null() {
        audio::unregister_forward(handle as u64);
        // SAFETY: consumes the native owner's sole raw Arc after worker quiescence.
        unsafe { Arc::from_raw(handle) }.revoke();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_network::link::{LinkId, QueueSpec, testing::datagram_pair};
    use oink_protocol::frame::{StreamKind, StreamTag};

    #[test]
    fn pcm_formats_resample_encode_and_revoke_on_the_real_packet_path() {
        for (rate, channels) in [(48_000, 2), (44_100, 1), (96_000, 2), (8_000, 1)] {
            let (sender, receiver) = datagram_pair(
                LinkId(1),
                StreamTag {
                    kind: StreamKind::Audio,
                    ordinal: 1,
                },
                QueueSpec {
                    messages: 256,
                    bytes: 1 << 20,
                },
            );
            let handle = oink_audio_forward_create(rate, channels, 42);
            assert!(!handle.is_null());
            // SAFETY: unique handle, bounded live buffers, no concurrent capture.
            unsafe {
                let audio = &*handle;
                audio.set_route(Some(ForwardRoute { stream: 7, sender }));
                for i in 0..24 {
                    let frames = rate / 200;
                    let pcm: Vec<i16> = (0..frames)
                        .flat_map(|frame| {
                            let sample = (8000.0
                                * ((i * frames + frame) as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin())
                                as i16;
                            std::iter::repeat_n(sample, channels as usize)
                        })
                        .collect();
                    assert_eq!(
                        oink_audio_forward_push_i16(handle, pcm.as_ptr(), frames, 1_000_000 + i as u64 * 5_000_000, 0),
                        frames
                    );
                }
                let mut packets = 0;
                while let Ok(Some(bytes)) = receiver.take() {
                    assert!(media::decode(&bytes, 7, Profile::Opus5ms).is_ok());
                    packets += 1;
                }
                assert!(packets > 12, "{rate}/{channels}: {packets}");
                oink_audio_forward_revoke(handle);
                assert_eq!(oink_audio_forward_push_i16(handle, [1i16; 480].as_ptr(), 240, 0, 0), 0);
                oink_audio_forward_destroy(handle);
            }
        }
    }

    #[test]
    fn invalid_format_and_buffers_are_rejected() {
        assert!(oink_audio_forward_create(0, 2, 1).is_null());
        assert!(oink_audio_forward_create(48_000, 3, 1).is_null());
        assert!(oink_audio_forward_create(48_000, 2, 0).is_null());
        let handle = oink_audio_forward_create(48_000, 2, 1);
        // SAFETY: a live handle; invalid lengths/nulls rejected before dereference.
        unsafe {
            assert_eq!(oink_audio_forward_push(handle, ptr::null(), 240, 0, 0), 0);
            assert_eq!(oink_audio_forward_push(handle, [0.0f32].as_ptr(), 4097, 0, 0), 0);
            oink_audio_forward_destroy(handle);
        }
    }

    #[test]
    fn native_bursts_are_not_dropped_and_gain_mute_is_applied_before_encoding() {
        for (rate, frames, minimum) in [(44_100, 1024, 3), (8_000, 4096, 95)] {
            let (sender, receiver) = datagram_pair(
                LinkId(1),
                StreamTag {
                    kind: StreamKind::Audio,
                    ordinal: 1,
                },
                QueueSpec {
                    messages: 256,
                    bytes: 1 << 20,
                },
            );
            let handle = oink_audio_forward_create(rate, 1, 50);
            // SAFETY: unique handle and bounded live mono buffer, no native callback.
            unsafe {
                let audio = &*handle;
                audio.set_route(Some(ForwardRoute { stream: 8, sender }));
                audio.set_gain(200, true);
                let samples = vec![0.25f32; frames as usize];
                assert_eq!(
                    oink_audio_forward_push(handle, samples.as_ptr(), frames, 1_000_000, 0),
                    frames
                );
                let mut count = 0;
                while let Ok(Some(bytes)) = receiver.take() {
                    if let media::Media::Packet { silent, .. } = media::decode(&bytes, 8, Profile::Opus5ms).unwrap() {
                        assert!(silent, "mute is applied before the Opus encoder");
                        count += 1;
                    }
                }
                assert!(count >= minimum, "native burst truncated: rate={rate}, packets={count}");
                oink_audio_forward_destroy(handle);
            }
        }
    }
}
