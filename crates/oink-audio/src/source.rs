//! Fixed-storage capture-to-encoder path shared by both native backends. It
//! applies the existing 20 ms software backlog policy and 5 ms gain ramp. The
//! ordinary agent owns packet serialization, send results and session-scoped mute ownership.
use crate::{dsp, forward_volume::Gain, handoff, levels, realtime};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Post-gain audio silence threshold for stream idling: -90 dBFS peak.
pub const SOUND_THRESHOLD: f32 = 0.000_031_622_776;
pub fn has_sound(samples: &[f32]) -> bool {
    samples.iter().any(|x| x.is_finite() && x.abs() > SOUND_THRESHOLD)
}
pub const SILENCE_TIMEOUT: Duration = Duration::from_secs(5);
const PRE_ROLL_SAMPLES: usize = 960;

const MAX_SAMPLES: usize = 48_000 * 2 / 50;
pub const MAX_BACKLOG: Duration = Duration::from_millis(20);
#[derive(Clone, Copy, Debug)]
pub struct Stamp {
    pub first_sample_at: Option<Instant>,
    pub callback_at: Option<Instant>,
    pub delivered_at: Instant,
    /// Native source position, in the backend's input-rate domain. A backend
    /// supplying this must check its frame sequence and report any loss through
    /// `discontinuity`; a synthetic count of delivered samples is not sufficient.
    pub source_frame_position: Option<u64>,
    pub discontinuity: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub stamp: Stamp,
    pub samples: usize,
    pub buffered_samples: usize,
    pub discarded_samples: usize,
}
#[derive(Clone, Copy, Debug)]
pub struct Packet {
    /// Advances even when the bounded handoff drops an encoded frame.
    pub ordinal: u64,
    pub burst: u64,
    pub sleep: bool,
    pub payload: [u8; dsp::MAX_PACKET_BYTES],
    pub len: usize,
    pub silent: bool,
    pub delivered_at: Instant,
    pub encode_start: Instant,
    pub encode_end: Instant,
    pub remaining_samples: usize,
}
type PacketWriter = Arc<dyn Fn(&Packet) -> bool + Send + Sync>;

struct Reconfigure {
    encoder: dsp::Encoder,
    revision: u64,
}

struct Shared {
    requested: crossbeam_queue::ArrayQueue<Reconfigure>,
    retired: crossbeam_queue::ArrayQueue<dsp::Encoder>,
    applied_revision: AtomicU64,
    writer: std::sync::OnceLock<PacketWriter>,
    percent: AtomicU16,
    muted: AtomicBool,
    signal: AtomicBool,
    frames: AtomicU64,
    asleep: AtomicBool,
    burst: AtomicU64,
    active: AtomicBool,
    error: AtomicI32,
    dropped: AtomicU64,
    observation_dropped: AtomicU64,
    metrics: realtime::Counters,
}
pub struct Encoded {
    requested_revision: u64,
    packets: handoff::Consumer<Option<Packet>>,
    observations: handoff::Consumer<Option<Observation>>,
    levels: handoff::Consumer<Option<levels::Window>>,
    latest_levels: Option<levels::Window>,
    shared: Arc<Shared>,
}
impl std::fmt::Debug for Encoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodedCapture").finish_non_exhaustive()
    }
}
impl Encoded {
    /// Prepare a fresh codec on the ordinary owner without closing native capture
    /// or its mute lease. Forwarding must stay disabled until reconfigured().
    pub fn reconfigure(&mut self, config: dsp::Config) -> Result<(), String> {
        let encoder = dsp::Encoder::new_with_config(config)?;
        while self.shared.retired.pop().is_some() {}
        self.requested_revision = self
            .requested_revision
            .checked_add(1)
            .ok_or("encoder revision exhausted")?;
        // Only the latest route matters; superseded codecs are destroyed here,
        // never on the capture thread.
        self.shared.requested.force_push(Reconfigure {
            encoder,
            revision: self.requested_revision,
        });
        Ok(())
    }
    pub fn reconfigured(&self) -> bool {
        self.shared.applied_revision.load(Ordering::Acquire) == self.requested_revision
    }

    /// Install M1's writer on the capture/encoder thread. The callback must not wait
    /// for any ordinary thread; it serializes and posts directly to the datagram queue.
    pub fn set_packet_writer(&self, writer: Arc<dyn Fn(&Packet) -> bool + Send + Sync>) {
        let _ = self.shared.writer.set(writer);
    }
    pub fn set_percent(&self, percent: u16) {
        self.shared.percent.store(percent.min(200), Ordering::Release);
    }
    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Release);
    }
    pub fn sleeping(&self) -> bool {
        self.shared.asleep.load(Ordering::Acquire)
    }
    pub fn burst(&self) -> u64 {
        self.shared.burst.load(Ordering::Acquire)
    }
    /// Delivered PCM progress, including delivered digital silence. Native
    /// backends may omit zero buffers, so this alone cannot detect callback stalls.
    pub fn frames(&self) -> u64 {
        self.shared.frames.load(Ordering::Acquire)
    }
    pub fn has_signal(&self) -> bool {
        self.shared.signal.load(Ordering::Acquire)
    }
    pub fn try_recv(&mut self) -> Option<Packet> {
        self.try_recv_at(Instant::now())
    }
    fn try_recv_at(&mut self, now: Instant) -> Option<Packet> {
        // At most the existing 20 ms capacity: bounded even after a stalled
        // ordinary sender; stale queued sound is never replayed.
        for _ in 0..8 {
            let packet = self.packets.pop().flatten()?;
            if now.saturating_duration_since(packet.delivered_at) <= MAX_BACKLOG {
                return Some(packet);
            }
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
        None
    }
    pub fn observation(&mut self) -> Option<Observation> {
        self.observations.pop().flatten()
    }
    /// Reads only aggregate metadata; no callback locks or PCM copies.
    pub fn levels(&mut self) -> Option<crate::CaptureLevels> {
        // Bounded by the handoff's capacity, even if control stopped polling.
        for _ in 0..2 {
            if let Some(window) = self.levels.pop().flatten() {
                self.latest_levels = Some(window);
            } else {
                break;
            }
        }
        self.latest_levels.map(|window| window.levels(Instant::now()))
    }
    pub fn take_metrics(&self) -> realtime::Metrics {
        self.shared.metrics.take()
    }
    pub fn take_dropped(&self) -> u64 {
        self.shared.dropped.swap(0, Ordering::Relaxed)
    }
    pub fn error(&self) -> Option<&'static str> {
        (self.shared.error.load(Ordering::Acquire) != 0).then_some("real-time source encoding failed")
    }
}
impl Drop for Encoded {
    fn drop(&mut self) {
        self.shared.active.store(false, Ordering::Release);
    }
}

pub struct Encoder {
    encoder: dsp::Encoder,
    gain: Gain,
    pcm: [f32; MAX_SAMPLES],
    len: usize,
    delivered_at: Option<Instant>,
    provisional: bool,
    packets: handoff::Producer<Option<Packet>>,
    observations: handoff::Producer<Option<Observation>>,
    levels: handoff::Producer<Option<levels::Window>>,
    level_started: Instant,
    level_sequence: u64,
    before_gain: levels::Accumulator,
    after_gain: levels::Accumulator,
    wasapi_buffers: Option<u64>,
    wasapi_silent_buffers: Option<u64>,
    shared: Arc<Shared>,
    quiet_since: Option<Instant>,
    pre_roll_at: Option<Instant>,
    pre_roll_cursor: usize,
    pre_roll: [f32; PRE_ROLL_SAMPLES],
    ordinal: u64,
}
impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureEncoder")
            .field("config", &self.encoder.config())
            .finish_non_exhaustive()
    }
}
pub fn channel(config: dsp::Config, percent: u16, provisional: bool) -> Result<(Encoder, Encoded), String> {
    let encoder = dsp::Encoder::new_with_config(config)?;
    let capacity = (20.0 / config.frame_duration_ms()) as usize;
    let (packets, rx) = handoff::channel(capacity, None);
    let (observations, observed) = handoff::channel(8, None);
    let (levels, measured) = handoff::channel(2, None);
    let shared = Arc::new(Shared {
        requested: crossbeam_queue::ArrayQueue::new(1),
        // One retired codec plus a replacement already in flight when the
        // ordinary owner drains retirement and publishes the next request.
        retired: crossbeam_queue::ArrayQueue::new(2),
        applied_revision: AtomicU64::new(0),
        writer: Default::default(),
        percent: AtomicU16::new(percent.min(200)),
        muted: AtomicBool::new(false),
        signal: AtomicBool::new(false),
        frames: AtomicU64::new(0),
        asleep: AtomicBool::new(false),
        burst: AtomicU64::new(1),
        active: AtomicBool::new(true),
        error: AtomicI32::new(0),
        dropped: AtomicU64::new(0),
        observation_dropped: AtomicU64::new(0),
        metrics: realtime::Counters::default(),
    });
    Ok((
        Encoder {
            encoder,
            gain: Gain::new(percent),
            pcm: [0.0; MAX_SAMPLES],
            len: 0,
            delivered_at: None,
            provisional,
            packets,
            observations,
            levels,
            level_started: Instant::now(),
            level_sequence: 0,
            before_gain: Default::default(),
            after_gain: Default::default(),
            wasapi_buffers: None,
            wasapi_silent_buffers: None,
            shared: shared.clone(),
            quiet_since: Some(Instant::now()),
            pre_roll_at: None,
            pre_roll_cursor: 0,
            pre_roll: [0.0; PRE_ROLL_SAMPLES],
            ordinal: 0,
        },
        Encoded {
            requested_revision: 0,
            packets: rx,
            observations: observed,
            levels: measured,
            latest_levels: None,
            shared,
        },
    ))
}
impl Encoder {
    fn apply_reconfiguration(&mut self) {
        if let Some(next) = self.shared.requested.pop() {
            let old = std::mem::replace(&mut self.encoder, next.encoder);
            // Single capture producer and ordinary consumer; reconfigure drains
            // retirement before every request, so these two slots cannot fill.
            self.shared.retired.push(old).expect("bounded encoder retirement");
            self.len = 0;
            self.delivered_at = None;
            self.gain.discard_pending();
            self.pre_roll.fill(0.0);
            self.pre_roll_cursor = 0;
            self.pre_roll_at = None;
            self.ordinal = 0;
            self.shared.burst.fetch_add(1, Ordering::Release);
            self.shared.applied_revision.store(next.revision, Ordering::Release);
        }
    }

    /// Called only by the Windows capture owner, once per nonempty GetBuffer.
    pub fn observe_wasapi_buffer(&mut self, silent: bool) {
        *self.wasapi_buffers.get_or_insert(0) += 1;
        *self.wasapi_silent_buffers.get_or_insert(0) += u64::from(silent);
    }
    fn publish_levels(&mut self, ended: Instant) {
        let started = self.level_started;
        if ended.saturating_duration_since(started) < Duration::from_secs(1) {
            return;
        }
        self.level_sequence += 1;
        let window = levels::Window {
            sequence: self.level_sequence,
            started,
            ended,
            before: std::mem::take(&mut self.before_gain),
            after: std::mem::take(&mut self.after_gain),
            wasapi_buffers: self.wasapi_buffers,
            wasapi_silent_buffers: self.wasapi_silent_buffers,
        };
        // A stalled status reader loses metadata; it never blocks audio.
        let _ = self.levels.push(Some(window));
        self.level_started = ended;
        self.wasapi_buffers = self.wasapi_buffers.map(|_| 0);
        self.wasapi_silent_buffers = self.wasapi_silent_buffers.map(|_| 0);
    }
    pub fn active(&self) -> bool {
        self.shared.active.load(Ordering::Acquire) && self.shared.error.load(Ordering::Acquire) == 0
    }
    pub fn observe_work(&self, gap: Option<Duration>, work: Duration) {
        self.shared.metrics.observe(gap, work);
    }
    /// Also called on capture waits with no frames (WASAPI may emit no silent buffers).
    pub fn tick(&mut self, now: Instant) {
        self.apply_reconfiguration();
        // Native taps/loopback may deliver no buffers at all while idle. Complete
        // metadata windows on the capture worker's existing wait cadence as well
        // as supply; frames=0 means unmeasured, never invented digital silence.
        self.publish_levels(now);
        self.tick_sleep(now);
    }
    fn tick_sleep(&mut self, now: Instant) {
        if !self.provisional
            && !self.shared.asleep.load(Ordering::Acquire)
            && self
                .quiet_since
                .is_some_and(|at| now.saturating_duration_since(at) >= SILENCE_TIMEOUT)
        {
            self.shared.asleep.store(true, Ordering::Release);
            self.emit(
                &[],
                true,
                Stamp {
                    first_sample_at: None,
                    callback_at: None,
                    delivered_at: now,
                    source_frame_position: None,
                    discontinuity: false,
                },
            );
        }
    }
    pub fn supply(&mut self, mut samples: &[f32], stamp: Stamp) {
        self.apply_reconfiguration();
        // A worker can resume with sound after a capture/scheduling gap before
        // its next wait tick. Expire the real-time silence interval first, so
        // that sound cannot erase an already elapsed sleep and reuse old Opus
        // state. Continuous silent callbacks and no callbacks use the same timer.
        self.tick_sleep(stamp.delivered_at);
        if samples.is_empty() || !samples.len().is_multiple_of(2) {
            return;
        }
        self.shared
            .frames
            .fetch_add((samples.len() / 2) as u64, Ordering::Release);
        let mut signal = false;
        for frame in samples.as_chunks::<2>().0 {
            signal |= self.before_gain.frame(frame);
        }
        if signal {
            self.shared.signal.store(true, Ordering::Release);
        }
        // Native backends already check frame positions and discontinuity flags.
        // A delivery gap is not missing PCM: Windows loopback can dequeue
        // consecutive engine buffers together after >20 ms. Clearing lookahead
        // inserts a 1 ms zero notch; clearing the partial codec frame loses PCM.
        // Positions may be in the pre-resampling rate, so do not reconstruct
        // their expected value from this 48 kHz slice. Fall back to delivery age
        // only when the backend cannot establish native continuity.
        let native_continuity_known = stamp.source_frame_position.is_some();
        if stamp.discontinuity
            || (!native_continuity_known
                && self
                    .pre_roll_at
                    .is_some_and(|at| stamp.delivered_at.saturating_duration_since(at) > MAX_BACKLOG))
        {
            self.gain.discard_pending();
            self.pre_roll.fill(0.0);
            self.pre_roll_cursor = 0;
        }
        self.pre_roll_at = Some(stamp.delivered_at);
        if stamp.discontinuity
            || (!native_continuity_known
                && self
                    .delivered_at
                    .is_some_and(|at| stamp.delivered_at.saturating_duration_since(at) > MAX_BACKLOG))
        {
            self.len = 0;
            self.delivered_at = None;
        }
        let frame_samples = self.encoder.config().frame_samples();
        let count = samples.len();
        let total = self.len.saturating_add(count);
        let discard = total
            .saturating_sub(MAX_SAMPLES)
            .div_ceil(frame_samples)
            .saturating_mul(frame_samples)
            .min(total);
        if discard >= self.len {
            samples = &samples[discard - self.len..];
            self.len = 0;
            self.delivered_at = None;
        } else if discard > 0 {
            self.pcm.copy_within(discard..self.len, 0);
            self.len -= discard;
        }
        if self.delivered_at.is_none() {
            self.delivered_at = Some(stamp.delivered_at);
        }
        self.pcm[self.len..self.len + samples.len()].copy_from_slice(samples);
        self.len += samples.len();
        let observation = Observation {
            stamp,
            samples: count,
            buffered_samples: self.len,
            discarded_samples: discard,
        };
        if self.observations.push(Some(observation)).is_err() {
            self.shared.observation_dropped.fetch_add(1, Ordering::Relaxed);
        }
        if self.provisional {
            self.len = 0;
            self.delivered_at = None;
            self.publish_levels(stamp.delivered_at);
            return;
        }
        // Atomic input is clamped before publication; this validation cannot
        // allocate an error, and the existing Gain implementation is unchanged.
        let _ = self.gain.set_percent(self.shared.percent.load(Ordering::Acquire));
        for _ in 0..8 {
            if self.len < frame_samples {
                break;
            }
            let mut frame = [0.0; dsp::FRAME_SAMPLES];
            frame[..frame_samples].copy_from_slice(&self.pcm[..frame_samples]);
            self.pcm.copy_within(frame_samples..self.len, 0);
            self.len -= frame_samples;
            self.gain.process(&mut frame[..frame_samples]);
            if self.shared.muted.load(Ordering::Acquire) {
                frame[..frame_samples].fill(0.0);
            }
            for stereo in frame[..frame_samples].as_chunks::<2>().0 {
                self.after_gain.frame(stereo);
            }
            let frame = &frame[..frame_samples];
            let now = stamp.delivered_at;
            let sound = has_sound(frame);
            let quiet_since = self.quiet_since.get_or_insert(now);
            if sound {
                *quiet_since = now;
            }
            let sleeping = self.shared.asleep.load(Ordering::Relaxed);
            if sleeping && sound {
                if self.encoder.reset().is_err() {
                    self.shared.error.store(1, Ordering::Release);
                    return;
                }
                self.ordinal = 0;
                self.shared.burst.fetch_add(1, Ordering::Release);
                self.shared.asleep.store(false, Ordering::Release);
                let pre_roll = self.pre_roll;
                for offset in (0..PRE_ROLL_SAMPLES).step_by(frame_samples) {
                    let at = (self.pre_roll_cursor + offset) % PRE_ROLL_SAMPLES;
                    self.emit(&pre_roll[at..at + frame_samples], false, stamp);
                }
            }
            if !self.shared.asleep.load(Ordering::Relaxed) {
                self.emit(frame, false, stamp);
            }
            self.pre_roll[self.pre_roll_cursor..self.pre_roll_cursor + frame_samples].copy_from_slice(frame);
            self.pre_roll_cursor = (self.pre_roll_cursor + frame_samples) % PRE_ROLL_SAMPLES;
            // Remainder now belongs to the current native block, if any. Its
            // age is software delivery time, not a native hardware timestamp.
            self.delivered_at = (self.len > 0).then_some(stamp.delivered_at);
        }
        // Levels continue at their original cadence when silence skips encoding.
        self.publish_levels(stamp.delivered_at);
    }
    fn emit(&mut self, frame: &[f32], sleep: bool, stamp: Stamp) {
        let now = Instant::now();
        if !sleep {
            let Some(next) = self.ordinal.checked_add(1) else {
                self.shared.error.store(1, Ordering::Release);
                return;
            };
            self.ordinal = next;
        }
        let mut packet = Packet {
            ordinal: self.ordinal,
            burst: self.shared.burst.load(Ordering::Relaxed),
            sleep,
            payload: [0; dsp::MAX_PACKET_BYTES],
            len: 0,
            silent: !has_sound(frame),
            delivered_at: self.delivered_at.unwrap_or(stamp.delivered_at),
            encode_start: now,
            encode_end: now,
            remaining_samples: self.len,
        };
        if !sleep {
            match self.encoder.encode_into(frame, &mut packet.payload) {
                Ok(len) => packet.len = len,
                Err(_) => {
                    self.shared.error.store(1, Ordering::Release);
                    return;
                }
            }
        }
        packet.encode_end = Instant::now();
        let accepted = if let Some(write) = self.shared.writer.get() {
            write(&packet)
        } else {
            self.packets.push(Some(packet)).is_ok()
        };
        if !accepted {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stamp() -> Stamp {
        Stamp {
            first_sample_at: None,
            callback_at: None,
            delivered_at: Instant::now(),
            source_frame_position: None,
            discontinuity: false,
        }
    }
    #[test]
    fn direct_writer_keeps_running_without_the_control_consumer() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 100, false).unwrap();
        let sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observed = sent.clone();
        output.set_packet_writer(Arc::new(move |packet| {
            assert_eq!(packet.ordinal, observed.fetch_add(1, Ordering::Relaxed) + 1);
            assert!(packet.len > 0);
            true
        }));
        crate::allocation_audit::assert_no_heap(|| {
            // No ordinary-thread endpoint is serviced, including diagnostics.
            for _ in 0..2000 {
                encoder.supply(&[0.01; 240], stamp());
            }
        });
        assert_eq!(sent.load(Ordering::Relaxed), 2000);
        assert_eq!(output.take_dropped(), 0);
        assert!(output.try_recv().is_none());
    }
    #[test]
    fn encoder_handoff_gain_changes_and_full_queue_do_no_heap_work() {
        for config in [
            dsp::Config::opus_10ms(),
            dsp::Config::low_latency_opus(),
            dsp::Config::low_latency_pcm(),
        ] {
            let (mut encoder, mut output) = channel(config, 100, false).unwrap();
            let input = [0.01; 256];
            crate::allocation_audit::assert_no_heap(|| {
                for i in 0..2000 {
                    if i == 500 {
                        output.set_percent(0);
                    }
                    if i == 1000 {
                        output.set_percent(150);
                    }
                    encoder.supply(&input, stamp());
                    if i % 32 != 0 {
                        while output.try_recv().is_some() {}
                    }
                    while output.observation().is_some() {}
                }
            });
            assert!(output.has_signal());
            assert!(output.error().is_none());
        }
    }
    fn assert_continuous_native_batch(block_frames: usize, config: dsp::Config) {
        // Consecutive WASAPI buffers can arrive together after slightly over
        // 20 ms. Delivery cadence must not alter the encoded audio.
        let render = |batched: bool| {
            let (mut encoder, mut output) = channel(config, 100, false).unwrap();
            let origin = Instant::now();
            let mut result = Vec::new();
            for block in 0..8 {
                let at = origin
                    + Duration::from_micros(if batched {
                        (block / 2) * 21_000 + (block % 2) * 100
                    } else {
                        block * 10_000
                    });
                encoder.supply(
                    &vec![0.25; block_frames * 2],
                    Stamp {
                        first_sample_at: None,
                        callback_at: None,
                        delivered_at: at,
                        source_frame_position: Some(block * block_frames as u64),
                        discontinuity: false,
                    },
                );
                while let Some(packet) = output.packets.pop().flatten() {
                    result.extend_from_slice(&packet.payload[..packet.len]);
                }
            }
            result
        };
        let actual = render(true);
        let expected = render(false);
        assert_eq!(actual.len(), expected.len(), "block_frames={block_frames}");
        assert_eq!(
            actual.iter().zip(&expected).position(|(a, b)| a != b),
            None,
            "batching must not change PCM, block_frames={block_frames}"
        );
    }
    #[test]
    fn continuous_native_frames_survive_batched_delivery_limiter_lookahead() {
        for config in [
            dsp::Config::low_latency_pcm(),
            dsp::Config::low_latency_opus(),
            dsp::Config::opus_10ms(),
        ] {
            assert_continuous_native_batch(480, config);
        }
    }
    #[test]
    fn continuous_native_frames_survive_batched_delivery_partial_codec_frame() {
        for config in [
            dsp::Config::low_latency_pcm(),
            dsp::Config::low_latency_opus(),
            dsp::Config::opus_10ms(),
        ] {
            assert_continuous_native_batch(441, config);
        }
    }

    #[test]
    fn native_discontinuity_and_unknown_position_still_discard_stale_audio() {
        for known_position in [false, true] {
            for discontinuity in [false, true] {
                for gap_ms in [10, 21] {
                    let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 100, false).unwrap();
                    let first = Stamp {
                        source_frame_position: known_position.then_some(0),
                        ..stamp()
                    };
                    encoder.supply(&[0.25; 260], first);
                    assert_eq!(encoder.len, 20);
                    output.packets.pop().unwrap();
                    crate::allocation_audit::assert_no_heap(|| {
                        encoder.supply(
                            &[0.0; 240],
                            Stamp {
                                delivered_at: first.delivered_at + Duration::from_millis(gap_ms),
                                source_frame_position: known_position.then_some(130),
                                discontinuity,
                                ..first
                            },
                        );
                    });
                    let discarded = discontinuity || (!known_position && gap_ms > 20);
                    assert_eq!(encoder.len, if discarded { 0 } else { 20 });
                    let packet = output.packets.pop().flatten().unwrap();
                    assert_eq!(packet.payload[..packet.len].iter().all(|b| *b == 0), discarded);
                    if discarded {
                        assert!(encoder.pre_roll.iter().all(|sample| *sample == 0.0));
                    }
                }
            }
        }
    }

    #[test]
    fn handoff_overflow_preserves_a_gap_in_encoded_ordinals() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 100, false).unwrap();
        for _ in 0..10 {
            encoder.supply(&[0.01; 240], stamp());
        }
        let mut last = 0;
        while let Some(packet) = output.try_recv() {
            last = packet.ordinal;
        }
        assert_eq!(last, 8);
        encoder.supply(&[0.01; 240], stamp());
        assert_eq!(output.try_recv().unwrap().ordinal, 11);
        assert_eq!(output.take_dropped(), 2);
    }
    #[test]
    fn provisional_capture_observes_signal_but_never_encodes_media() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 100, true).unwrap();
        encoder.supply(&[0.02; 960], stamp());
        assert!(output.has_signal());
        assert!(output.try_recv().is_none());
    }
    #[test]
    fn silence_is_computed_after_gain_and_the_ramp() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 0, false).unwrap();
        let first = stamp();
        encoder.supply(&[0.02; 240], first);
        assert!(output.try_recv_at(first.delivered_at).unwrap().silent);
        output.set_percent(100);
        // Scheduling delay is not a capture discontinuity in this gain test.
        encoder.supply(
            &[0.02; 480],
            Stamp {
                delivered_at: first.delivered_at + Duration::from_micros(2500),
                ..first
            },
        );
        assert!(
            !output
                .try_recv_at(first.delivered_at + Duration::from_micros(2500))
                .unwrap()
                .silent
        );
    }
    #[test]
    fn level_windows_include_silence_gain_limiter_and_wasapi_flags_without_heap_work() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 200, false).unwrap();
        let start = encoder.level_started;
        crate::allocation_audit::assert_no_heap(|| {
            for i in 0..=400 {
                let stamp = Stamp {
                    delivered_at: start + Duration::from_micros(i * 2500),
                    ..stamp()
                };
                let silent = i % 2 == 0;
                encoder.observe_wasapi_buffer(silent);
                encoder.supply(&[if silent { 0.0 } else { 0.75 }; 240], stamp);
            }
        });
        let levels = output.levels().unwrap();
        assert_eq!(levels.window_ms, 1000.0);
        assert_eq!(levels.window_sequence, 1);
        assert_eq!(levels.before_gain.frames, 401 * 120);
        assert_eq!(levels.before_gain.peak_dbfs, Some(20.0 * 0.75_f64.log10()));
        assert_eq!(levels.before_gain.zero_frame_fraction, 201.0 / 401.0);
        assert_eq!(levels.after_gain.unwrap().peak_dbfs, Some(0.0));
        assert_eq!(levels.after_gain.unwrap().zero_frame_fraction, 201.0 / 401.0);
        assert_eq!(levels.wasapi_buffers, Some(401));
        assert_eq!(levels.wasapi_silent_buffers, Some(201));
        output.set_percent(0);
        for i in 401..=800 {
            encoder.observe_wasapi_buffer(false);
            encoder.supply(
                &[0.01; 240],
                Stamp {
                    delivered_at: start + Duration::from_micros(i * 2500),
                    ..stamp()
                },
            );
        }
        let next = output.levels().unwrap();
        assert_eq!(next.window_sequence, 2);
        assert_eq!(next.before_gain.frames, 400 * 120);
        assert_eq!(next.wasapi_buffers, Some(400));
        assert_eq!(next.wasapi_silent_buffers, Some(0));
        assert!(next.after_gain.unwrap().zero_frame_fraction > 0.99);
    }
    #[test]
    fn provisional_levels_have_no_post_gain_measurement_and_handoff_never_blocks() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 0, true).unwrap();
        let start = Instant::now();
        assert!(output.levels().is_none());
        crate::allocation_audit::assert_no_heap(|| {
            for i in 0..=2000 {
                encoder.supply(
                    &[0.0; 240],
                    Stamp {
                        delivered_at: start + Duration::from_millis(i * 5),
                        ..stamp()
                    },
                );
            }
        });
        let levels = output.levels().unwrap();
        assert_eq!(levels.window_sequence, 2); // later windows dropped by a stalled reader
        assert_eq!(levels.before_gain.peak_dbfs, None);
        assert_eq!(levels.before_gain.rms_dbfs, None);
        assert_eq!(levels.before_gain.zero_frame_fraction, 1.0);
        assert!(levels.after_gain.is_none());
        assert!(levels.wasapi_silent_buffers.is_none());
        assert!(output.try_recv().is_none());
    }
}

#[cfg(test)]
mod sleep_tests {
    use super::*;
    fn supply(encoder: &mut Encoder, samples: &[f32], now: Instant) {
        encoder.supply(
            samples,
            Stamp {
                first_sample_at: None,
                callback_at: None,
                delivered_at: now,
                source_frame_position: None,
                discontinuity: false,
            },
        );
    }
    #[test]
    fn forwarding_mute_preserves_gain_and_sleeps_then_unmute_wakes_a_new_burst() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 150, false).unwrap();
        let now = Instant::now();
        supply(&mut encoder, &[0.2; 240], now);
        assert!(!output.packets.pop().flatten().unwrap().silent);
        output.set_muted(true);
        supply(&mut encoder, &[0.2; 240], now + Duration::from_millis(5));
        let muted = output.packets.pop().flatten().unwrap();
        assert!(muted.silent);
        assert!(muted.payload[..muted.len].iter().all(|x| *x == 0));
        assert_eq!(output.shared.percent.load(Ordering::Acquire), 150);
        encoder.tick(now + SILENCE_TIMEOUT + Duration::from_millis(10));
        assert!(output.sleeping());
        assert!(output.packets.pop().flatten().unwrap().sleep);
        output.set_percent(155);
        supply(
            &mut encoder,
            &[0.2; 240],
            now + SILENCE_TIMEOUT + Duration::from_millis(15),
        );
        assert!(output.sleeping());
        output.set_muted(false);
        supply(
            &mut encoder,
            &[0.2; 240],
            now + SILENCE_TIMEOUT + Duration::from_millis(20),
        );
        assert!(!output.sleeping());
        assert_eq!(output.burst(), 2);
        let packets: Vec<_> = std::iter::from_fn(|| output.packets.pop().flatten()).collect();
        assert!(packets.iter().all(|packet| packet.burst == 2));
        assert!(packets.iter().any(|packet| !packet.silent));
        assert_eq!(output.shared.percent.load(Ordering::Acquire), 155);
    }
    #[test]
    fn no_frames_sleeps_at_five_seconds_and_reports_sleep_once() {
        let config = dsp::Config::low_latency_opus();
        let (mut encoder, mut output) = channel(config, 100, false).unwrap();
        let now = Instant::now();
        supply(&mut encoder, &[0.3; 480], now);
        let last = output.packets.pop().flatten().unwrap();
        let exit = crate::idle::Bursts::new(now);
        assert!(exit.media(last.burst, last.ordinal, now));
        // No native callbacks: the worker still ticks on its capture wait.
        encoder.tick(now + Duration::from_millis(4999));
        assert!(!output.sleeping());
        crate::allocation_audit::assert_no_heap(|| encoder.tick(now + Duration::from_secs(5)));
        assert!(
            output.sleeping(),
            "source must sleep after five real-time silent seconds"
        );
        let sleep = output.packets.pop().flatten().unwrap();
        assert!(sleep.sleep);
        assert_eq!((sleep.burst, sleep.ordinal), (last.burst, last.ordinal));
        exit.sleep(sleep.burst, sleep.ordinal);
        assert!(exit.asleep(now + Duration::from_secs(5)));
        encoder.tick(now + Duration::from_secs(6));
        assert!(output.packets.pop().is_none());
    }

    #[test]
    fn lost_sleep_fallback_then_resume_has_fresh_codec_preroll_and_onset() {
        for config in [dsp::Config::low_latency_opus(), dsp::Config::low_latency_pcm()] {
            let (mut encoder, mut output) = channel(config, 100, false).unwrap();
            let now = Instant::now();
            let exit = crate::idle::Bursts::new(now);
            // Prime a stateful old encoder. The exit can already have consumed it.
            for frame in 0..16 {
                let at = now + Duration::from_millis(frame * 5);
                supply(&mut encoder, &[0.3; dsp::FRAME_SAMPLES][..config.frame_samples()], at);
                let packet = output.packets.pop().flatten().unwrap();
                assert!(exit.media(packet.burst, packet.ordinal, at));
            }
            let wake = now + Duration::from_secs(31);
            assert!(exit.asleep(wake)); // Sleep was lost; no encoder tick before supply either.
            let mut onset = [0.2; dsp::FRAME_SAMPLES];
            onset[..120].fill(0.0);
            supply(&mut encoder, &onset[..config.frame_samples()], wake);
            assert_eq!(output.burst(), 2, "resume must not continue the old encoder burst");
            assert!(!output.sleeping());
            let mut delayed_onset = [0.0; dsp::FRAME_SAMPLES];
            let delay = crate::forward_volume::LOOKAHEAD_FRAMES * 2;
            delayed_onset[delay..config.frame_samples()].copy_from_slice(&onset[..config.frame_samples() - delay]);
            let mut fresh_encoder = dsp::Encoder::new_with_config(config).unwrap();
            let mut receiver = dsp::Receiver::new_with_config(config).unwrap();
            receiver.reset_burst();
            let mut reference = dsp::Receiver::new_with_config(config).unwrap();
            reference.reset_burst();
            let mut expected = [0; dsp::MAX_PACKET_BYTES];
            for ordinal in 1..=PRE_ROLL_SAMPLES / config.frame_samples() + 1 {
                let mut packet = output.packets.pop().flatten().unwrap();
                if packet.sleep {
                    // gap detection can publish Sleep immediately before waking
                    packet = output.packets.pop().flatten().unwrap();
                }
                assert_eq!((packet.burst, packet.ordinal), (2, ordinal as u64));
                let frame = if ordinal <= PRE_ROLL_SAMPLES / config.frame_samples() {
                    &[0.0; dsp::FRAME_SAMPLES][..config.frame_samples()]
                } else {
                    &delayed_onset[..config.frame_samples()]
                };
                let len = fresh_encoder.encode_into(frame, &mut expected).unwrap();
                assert_eq!(
                    &packet.payload[..packet.len],
                    &expected[..len],
                    "encoder reset must pair with fresh decoder"
                );
                assert!(exit.media(packet.burst, packet.ordinal, wake));
                assert!(receiver.push_slice_at(packet.ordinal, &packet.payload[..packet.len], wake, None));
                assert!(reference.push_slice_at(packet.ordinal, &expected[..len], wake, None));
            }
            assert!(!exit.asleep(wake));
            let mut decoded = Vec::new();
            for tick in 0..8 {
                let at = wake + Duration::from_millis(tick * 5);
                let a = receiver.render_at(at);
                assert_eq!(a, reference.render_at(at));
                decoded.extend(a);
            }
            assert!(
                decoded.iter().any(|sample| sample.abs() > 0.1),
                "onset PCM must reach output"
            );
        }
    }

    #[test]
    fn continuously_delivered_silent_packets_sleep_after_five_seconds() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_opus(), 100, false).unwrap();
        let now = Instant::now();
        supply(&mut encoder, &[0.3; 480], now);
        output.packets.pop().unwrap();
        for frame in 1..1000 {
            let at = now + Duration::from_millis(frame * 5);
            encoder.tick(at);
            supply(&mut encoder, &[0.0; 480], at);
            assert!(!output.sleeping());
            assert!(!output.packets.pop().flatten().unwrap().sleep);
        }
        // The lookahead tail makes the following 5 ms packet the last sound.
        encoder.tick(now + Duration::from_millis(5005));
        assert!(output.sleeping());
        assert!(output.packets.pop().flatten().unwrap().sleep);
    }

    #[test]
    fn silence_gap_matrix_keeps_short_bursts_and_resets_long_bursts_even_if_sleep_is_lost() {
        for config in [dsp::Config::low_latency_opus(), dsp::Config::low_latency_pcm()] {
            for delivered_silence in [false, true] {
                for lose_sleep in [false, true] {
                    for gap_ms in [1000, 4900, 5100, 29000, 31000] {
                        let (mut encoder, mut output) = channel(config, 100, false).unwrap();
                        let now = Instant::now();
                        let exit = crate::idle::Bursts::new(now);
                        supply(&mut encoder, &[0.3; dsp::FRAME_SAMPLES][..config.frame_samples()], now);
                        let packet = output.packets.pop().flatten().unwrap();
                        assert!(exit.media(packet.burst, packet.ordinal, now));
                        let step_us = (config.frame_duration_ms() * 1000.0) as u64;
                        if delivered_silence {
                            for us in (step_us..gap_ms * 1000).step_by(step_us as usize) {
                                let at = now + Duration::from_micros(us);
                                supply(&mut encoder, &[0.0; dsp::FRAME_SAMPLES][..config.frame_samples()], at);
                                while let Some(packet) = output.packets.pop().flatten() {
                                    if packet.sleep {
                                        if !lose_sleep {
                                            exit.sleep(packet.burst, packet.ordinal);
                                        }
                                    } else {
                                        assert!(exit.media(packet.burst, packet.ordinal, at));
                                    }
                                }
                            }
                        } else {
                            encoder.tick(now + Duration::from_millis(gap_ms));
                            if let Some(packet) = output.packets.pop().flatten() {
                                assert!(packet.sleep);
                                if !lose_sleep {
                                    exit.sleep(packet.burst, packet.ordinal);
                                }
                            }
                        }
                        let should_sleep = gap_ms >= 5000;
                        assert_eq!(
                            output.sleeping(),
                            should_sleep,
                            "gap={gap_ms} frames={delivered_silence}"
                        );
                        // A lost event leaves the exit truthfully awake until its
                        // 30 s packet timeout; it never sleeps before the source.
                        let fallback = !delivered_silence && gap_ms > 30000;
                        assert_eq!(
                            exit.asleep(now + Duration::from_millis(gap_ms)),
                            should_sleep && (!lose_sleep || fallback)
                        );
                        let wake = now + Duration::from_millis(gap_ms);
                        supply(&mut encoder, &[0.2; dsp::FRAME_SAMPLES][..config.frame_samples()], wake);
                        assert_eq!(output.burst(), if should_sleep { 2 } else { 1 });
                        assert!(!output.sleeping());
                        let mut count = 0;
                        while let Some(packet) = output.packets.pop().flatten() {
                            assert!(!packet.sleep);
                            if should_sleep {
                                assert_eq!(packet.ordinal, count + 1);
                            }
                            assert!(exit.media(packet.burst, packet.ordinal, wake));
                            count += 1;
                        }
                        assert_eq!(
                            count,
                            if should_sleep {
                                (PRE_ROLL_SAMPLES / config.frame_samples() + 1) as u64
                            } else {
                                1
                            }
                        );
                        assert!(!exit.asleep(wake));
                    }
                }
            }
        }
    }
    #[test]
    fn sound_after_a_missed_worker_tick_expires_silence_before_updating_activity() {
        for gap_ms in [1000, 4900, 5100, 29000, 31000] {
            let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 100, false).unwrap();
            let now = Instant::now();
            supply(&mut encoder, &[0.3; 240], now);
            output.packets.pop().unwrap();
            supply(&mut encoder, &[0.2; 240], now + Duration::from_millis(gap_ms));
            assert_eq!(output.burst(), if gap_ms < 5000 { 1 } else { 2 });
            assert!(!output.sleeping());
        }
    }
    #[test]
    fn two_sources_with_different_last_sound_times_sleep_and_wake_independently() {
        let config = dsp::Config::low_latency_pcm();
        let (mut a, mut out_a) = channel(config, 100, false).unwrap();
        let (mut b, mut out_b) = channel(config, 100, false).unwrap();
        let now = Instant::now();
        let exits = [crate::idle::Bursts::new(now), crate::idle::Bursts::new(now)];
        for (index, (source, output)) in [(&mut a, &mut out_a), (&mut b, &mut out_b)].into_iter().enumerate() {
            supply(source, &[0.3; 240], now);
            let packet = output.packets.pop().flatten().unwrap();
            assert!(exits[index].media(packet.burst, packet.ordinal, now));
        }
        supply(&mut b, &[0.2; 240], now + Duration::from_millis(4900));
        let packet = out_b.packets.pop().flatten().unwrap();
        assert_eq!(packet.burst, 1);
        assert!(exits[1].media(packet.burst, packet.ordinal, now + Duration::from_millis(4900)));
        a.tick(now + Duration::from_secs(5));
        b.tick(now + Duration::from_secs(5));
        let sleep = out_a.packets.pop().flatten().unwrap();
        assert!(sleep.sleep);
        exits[0].sleep(sleep.burst, sleep.ordinal);
        assert!(out_a.sleeping() && !out_b.sleeping());
        assert!(!exits.iter().all(|exit| exit.asleep(now + Duration::from_secs(5))));
        b.tick(now + Duration::from_millis(9900));
        let sleep = out_b.packets.pop().flatten().unwrap();
        exits[1].sleep(sleep.burst, sleep.ordinal);
        assert!(exits.iter().all(|exit| exit.asleep(now + Duration::from_millis(9900))));
        supply(&mut a, &[0.2; 240], now + Duration::from_secs(10));
        while let Some(packet) = out_a.packets.pop().flatten() {
            assert_eq!(packet.burst, 2);
            assert!(exits[0].media(packet.burst, packet.ordinal, now + Duration::from_secs(10)));
        }
        assert!(!out_a.sleeping() && out_b.sleeping());
        assert!(!exits[0].asleep(now + Duration::from_secs(10)));
        assert!(exits[1].asleep(now + Duration::from_secs(10)));
    }
    #[test]
    fn shared_threshold_is_strict_minus_ninety_db_peak() {
        for samples in [
            &[][..],
            &[0.0, -0.0],
            &[SOUND_THRESHOLD, -SOUND_THRESHOLD],
            &[1.0 / 32768.0],
            &[f32::NAN, f32::INFINITY],
        ] {
            assert!(!has_sound(samples));
        }
        assert!(has_sound(&[f32::from_bits(SOUND_THRESHOLD.to_bits() + 1)]));
        assert!(has_sound(&[-0.0001])); // -80 dBFS is sound, not background silence.
    }
    #[test]
    fn five_seconds_without_frames_sleeps_once_and_first_sound_frame_wakes() {
        let config = dsp::Config::low_latency_pcm();
        let (mut encoder, mut output) = channel(config, 100, false).unwrap();
        let now = Instant::now();
        encoder.quiet_since = Some(now);
        encoder.tick(now + SILENCE_TIMEOUT - Duration::from_nanos(1));
        assert!(!output.sleeping());
        encoder.tick(now + SILENCE_TIMEOUT);
        assert!(output.sleeping());
        let sleep = output.packets.pop().flatten().unwrap();
        assert!(sleep.sleep);
        assert_eq!((sleep.burst, sleep.ordinal, sleep.len), (1, 0, 0));
        encoder.tick(now + Duration::from_secs(31));
        assert!(output.packets.pop().is_none());
        supply(&mut encoder, &[0.5; 240], now + Duration::from_secs(31));
        assert!(!output.sleeping());
        for ordinal in 1..=5 {
            let packet = output.packets.pop().flatten().unwrap();
            assert_eq!((packet.burst, packet.ordinal, packet.sleep), (2, ordinal, false));
            assert_eq!(packet.silent, ordinal < 5);
            let delay = crate::forward_volume::LOOKAHEAD_FRAMES * 2;
            for (i, sample) in packet.payload[..packet.len].as_chunks::<2>().0.iter().enumerate() {
                assert_eq!(
                    i16::from_le_bytes(*sample),
                    if ordinal == 5 && i >= delay { 16384 } else { 0 }
                );
            }
        }
    }
    #[test]
    fn pre_roll_is_last_ten_ms_post_gain_pcm_and_keeps_entire_trigger() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 100, false).unwrap();
        let start = Instant::now() + Duration::from_secs(31);
        encoder.tick(start);
        assert!(output.packets.pop().flatten().unwrap().sleep);
        let values = [1.0 / 32768.0, -1.0 / 32768.0, 0.0, 1.0 / 32768.0];
        for (i, value) in values.iter().enumerate() {
            supply(
                &mut encoder,
                &[*value; 240],
                start + Duration::from_micros(i as u64 * 2500),
            );
            assert!(output.packets.pop().is_none());
        }
        let mut onset = [0.25; 240];
        onset[..120].fill(0.0); // Keep the silent beginning of the triggering frame too.
        supply(&mut encoder, &onset, start + Duration::from_millis(10));
        let mut expected = vec![0.0; crate::forward_volume::LOOKAHEAD_FRAMES * 2];
        for value in values {
            expected.extend([value; 240]);
        }
        expected.extend(onset);
        for expected in expected[..1200].as_chunks::<240>().0 {
            let packet = output.packets.pop().flatten().unwrap();
            for (bytes, expected) in packet.payload[..packet.len].as_chunks::<2>().0.iter().zip(expected) {
                assert_eq!(i16::from_le_bytes(*bytes), (expected * 32767.0).round() as i16);
            }
        }
    }
    #[test]
    fn capture_levels_continue_while_encoding_sleeps() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 0, false).unwrap();
        let now = Instant::now();
        supply(&mut encoder, &[0.2; 240], now);
        encoder.tick(now + Duration::from_secs(31));
        supply(&mut encoder, &[0.2; 240], now + Duration::from_secs(31));
        assert!(output.sleeping());
        let first = output.levels().unwrap();
        crate::allocation_audit::assert_no_heap(|| {
            supply(&mut encoder, &[0.3; 240], now + Duration::from_secs(32));
        });
        let next = output.levels().unwrap();
        assert!(output.sleeping());
        assert_eq!(next.window_sequence, first.window_sequence + 1);
        assert!(next.before_gain.peak_dbfs.unwrap() > -11.0);
        assert_eq!(next.after_gain.unwrap().peak_dbfs, None);
        assert_eq!(next.after_gain.unwrap().zero_frame_fraction, 1.0);
    }
    #[test]
    fn capture_levels_complete_without_any_native_buffers_and_after_partial_window() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 100, false).unwrap();
        let now = Instant::now();
        assert!(output.levels().is_none());
        encoder.tick(now + Duration::from_secs(5));
        let empty = output.levels().expect("capture wait must complete a no-buffer window");
        assert_eq!(empty.window_sequence, 1);
        assert_eq!(empty.before_gain.frames, 0);
        assert_eq!(empty.before_gain.peak_dbfs, None);
        assert_eq!(empty.before_gain.rms_dbfs, None);
        assert_eq!(empty.before_gain.zero_frame_fraction, 0.0);
        assert!(empty.after_gain.is_none());
        assert!(serde_json::to_string(&empty).is_ok());
        supply(&mut encoder, &[0.25; 240], now + Duration::from_millis(1100));
        encoder.tick(now + Duration::from_secs(6));
        let partial = output.levels().unwrap();
        assert_eq!(partial.window_sequence, 2);
        assert_eq!(partial.before_gain.frames, 120);
        assert!(partial.before_gain.peak_dbfs.unwrap() > -13.0);
        assert_eq!(partial.after_gain.unwrap().frames, 120);
        crate::allocation_audit::assert_no_heap(|| encoder.tick(now + Duration::from_secs(33)));
        let asleep = output.levels().unwrap();
        assert!(output.sleeping());
        assert_eq!(asleep.window_sequence, 3);
        assert_eq!(asleep.before_gain.frames, 0);
        assert_eq!(asleep.before_gain.zero_frame_fraction, 0.0);
    }
    #[test]
    fn wake_resets_encoder_in_place_without_allocating_or_fading() {
        for config in [
            dsp::Config::opus_10ms(),
            dsp::Config::low_latency_opus(),
            dsp::Config::low_latency_pcm(),
        ] {
            let (mut encoder, mut output) = channel(config, 100, false).unwrap();
            let now = Instant::now();
            let input = [0.2; dsp::FRAME_SAMPLES];
            supply(&mut encoder, &input[..config.frame_samples()], now);
            while output.packets.pop().is_some() {}
            encoder.tick(now + SILENCE_TIMEOUT);
            assert!(output.packets.pop().flatten().unwrap().sleep);
            crate::allocation_audit::assert_no_heap(|| {
                supply(
                    &mut encoder,
                    &input[..config.frame_samples()],
                    now + SILENCE_TIMEOUT + Duration::from_secs(1),
                );
            });
            let mut fresh = dsp::Encoder::new_with_config(config).unwrap();
            let mut expected = [0; dsp::MAX_PACKET_BYTES];
            for _ in 0..PRE_ROLL_SAMPLES / config.frame_samples() {
                let len = fresh
                    .encode_into(&[0.0; dsp::FRAME_SAMPLES][..config.frame_samples()], &mut expected)
                    .unwrap();
                let packet = output.packets.pop().flatten().unwrap();
                assert_eq!(&packet.payload[..packet.len], &expected[..len]);
            }
            let delay = crate::forward_volume::LOOKAHEAD_FRAMES * 2;
            let mut delayed = [0.0; dsp::FRAME_SAMPLES];
            delayed[delay..config.frame_samples()].copy_from_slice(&input[..config.frame_samples() - delay]);
            let len = fresh
                .encode_into(&delayed[..config.frame_samples()], &mut expected)
                .unwrap();
            let packet = output.packets.pop().flatten().unwrap();
            assert_eq!(&packet.payload[..packet.len], &expected[..len]);
        }
    }
}

#[cfg(test)]
mod reconfigure_tests {
    use super::*;

    #[test]
    fn codec_replacement_keeps_capture_and_applies_latest_request_while_idle() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 100, false).unwrap();
        output.reconfigure(dsp::Config::opus_10ms()).unwrap();
        output.reconfigure(dsp::Config::low_latency_opus()).unwrap();
        assert!(!output.reconfigured());
        encoder.tick(Instant::now());
        assert!(output.reconfigured());
        assert!(encoder.active());
        assert_eq!(encoder.encoder.config(), dsp::Config::low_latency_opus());
        let now = Instant::now();
        encoder.supply(
            &[0.1; 480],
            Stamp {
                first_sample_at: None,
                callback_at: Some(now),
                delivered_at: now,
                source_frame_position: None,
                discontinuity: false,
            },
        );
        let packet = output.try_recv().unwrap();
        assert_eq!((packet.ordinal, packet.burst), (1, 2));
        assert!(dsp::valid_payload(
            dsp::Config::low_latency_opus(),
            &packet.payload[..packet.len]
        ));
        assert_eq!(output.shared.retired.len(), 1);
        output.reconfigure(dsp::Config::low_latency_pcm()).unwrap();
        assert!(
            output.shared.retired.is_empty(),
            "ordinary owner reclaims retired codec"
        );
        encoder.tick(Instant::now());
        assert!(output.reconfigured());
    }

    #[test]
    fn concurrent_replacements_bound_retirement_and_acknowledge_latest_codec() {
        let (mut encoder, mut output) = channel(dsp::Config::low_latency_pcm(), 100, false).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let worker_done = done.clone();
        let worker = std::thread::spawn(move || {
            while !worker_done.load(Ordering::Acquire) {
                encoder.tick(Instant::now());
                std::thread::yield_now();
            }
            encoder.tick(Instant::now());
            encoder
        });
        for _ in 0..1000 {
            output.reconfigure(dsp::Config::low_latency_pcm()).unwrap();
            std::thread::yield_now();
        }
        done.store(true, Ordering::Release);
        let encoder = worker.join().unwrap();
        assert!(output.reconfigured());
        assert!(encoder.active());
        assert!(output.shared.retired.len() <= 2);
    }
}
