//! Bounded audio transport DSP with explicit negotiated Opus and PCM profiles.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Frame dimensions of the Opus 10 ms profile.
pub const FRAME_SIZE: usize = 480;
pub const FRAME_SAMPLES: usize = FRAME_SIZE * CHANNELS;
/// PCM16 stereo, 5 ms. With media metadata this remains below a 1200 byte path MTU.
pub const MAX_PACKET_BYTES: usize = 960;
const TARGET_FRAMES: usize = 3;
const MAX_QUEUE_FRAMES: usize = 12;
const MAX_PLC_FRAMES: usize = 6;
const MAX_RESAMPLE_FRAMES: usize = FRAME_SIZE * 32;
const ARRIVAL_WINDOW: Duration = Duration::from_secs(1);
const MAX_ARRIVALS: usize = 512;
const TARGET_STABLE_INTERVAL: Duration = Duration::from_secs(3);
const TARGET_DECAY_INTERVAL: Duration = Duration::from_millis(500);
const MAX_JITTER_MS: f64 = 250.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    Opus,
    Pcm16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameDuration {
    Ms10,
    Ms5,
    Ms2p5,
}
impl FrameDuration {
    pub const fn samples(self) -> usize {
        match self {
            Self::Ms10 => 480,
            Self::Ms5 => 240,
            Self::Ms2p5 => 120,
        }
    }
    pub fn milliseconds(self) -> f64 {
        self.samples() as f64 / 48.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JitterConfig {
    /// In new profiles, target frames are additional playout delay: the frame
    /// currently due is not counted. E.g. 2 x 2.5 ms means a 5 ms startup wait.
    pub initial_frames: usize,
    pub min_frames: usize,
    pub max_frames: usize,
    pub adaptive: bool,
    pub max_plc_frames: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub codec: Codec,
    pub frame_duration: FrameDuration,
    /// Opus CBR rate; PCM must specify its actual 1,536,000 bit/s stream rate.
    pub bitrate: u32,
    pub jitter: JitterConfig,
}

impl Config {
    pub const fn opus_10ms() -> Self {
        Self {
            codec: Codec::Opus,
            frame_duration: FrameDuration::Ms10,
            bitrate: 128_000,
            jitter: JitterConfig {
                initial_frames: 3,
                min_frames: 3,
                max_frames: 3,
                adaptive: false,
                max_plc_frames: MAX_PLC_FRAMES,
            },
        }
    }
    pub const fn low_latency_pcm() -> Self {
        Self {
            codec: Codec::Pcm16,
            frame_duration: FrameDuration::Ms2p5,
            bitrate: 1_536_000,
            jitter: JitterConfig {
                initial_frames: 2,
                min_frames: 2,
                max_frames: 100,
                adaptive: true,
                max_plc_frames: 8,
            },
        }
    }
    pub const fn low_latency_opus() -> Self {
        Self {
            codec: Codec::Opus,
            frame_duration: FrameDuration::Ms5,
            bitrate: 320_000,
            jitter: JitterConfig {
                initial_frames: 1,
                min_frames: 1,
                max_frames: 50,
                adaptive: true,
                max_plc_frames: 4,
            },
        }
    }
    pub const fn frame_size(self) -> usize {
        self.frame_duration.samples()
    }
    pub const fn frame_samples(self) -> usize {
        self.frame_size() * CHANNELS
    }
    pub fn frame_duration_ms(self) -> f64 {
        self.frame_duration.milliseconds()
    }
    pub fn payload_bytes(self) -> usize {
        match self.codec {
            Codec::Pcm16 => self.frame_samples() * 2,
            Codec::Opus => (u64::from(self.bitrate) * self.frame_size() as u64 / (u64::from(SAMPLE_RATE) * 8)) as usize,
        }
    }
    pub fn validate(self) -> Result<(), String> {
        if self.jitter.min_frames == 0
            || self.jitter.min_frames as f64 * self.frame_duration_ms() < 5.0
            || self.jitter.initial_frames < self.jitter.min_frames
            || self.jitter.initial_frames > self.jitter.max_frames
            || self.jitter.max_frames > 100
            || self.jitter.max_frames as f64 * self.frame_duration_ms() > MAX_JITTER_MS
            || self.jitter.max_plc_frames == 0
            || self.jitter.max_plc_frames > 24
            || self.jitter.max_plc_frames as f64 * self.frame_duration_ms() > 60.0
        {
            return Err("jitter bounds must be nonzero, ordered, and at most 250 ms; PLC at most 60 ms".into());
        }
        let opus_max_bitrate = if self.frame_duration == FrameDuration::Ms5 {
            320_000
        } else {
            256_000
        };
        match self.codec {
            Codec::Opus if !(16_000..=opus_max_bitrate).contains(&self.bitrate) => {
                return Err(format!(
                    "Opus CBR must be 16,000..={opus_max_bitrate} bit/s for this frame duration"
                ));
            }
            Codec::Pcm16 if self.bitrate != 1_536_000 => {
                return Err("PCM16 stereo at 48 kHz requires 1,536,000 bit/s".into());
            }
            _ => {}
        }
        if self.payload_bytes() == 0 || self.payload_bytes() > MAX_PACKET_BYTES {
            return Err("media payload exceeds the 960 byte codec budget (PCM10ms is unsupported)".into());
        }
        Ok(())
    }
    fn is_opus_10ms(self) -> bool {
        self == Self::opus_10ms()
    }
}

pub fn valid_payload(config: Config, payload: &[u8]) -> bool {
    !payload.is_empty()
        && payload.len() <= MAX_PACKET_BYTES
        && match config.codec {
            Codec::Pcm16 => payload.len() == config.payload_bytes(),
            Codec::Opus => opus::packet::get_nb_samples(payload, SAMPLE_RATE).ok() == Some(config.frame_size()),
        }
}

enum EncoderCodec {
    Opus(opus::Encoder),
    Pcm16,
}
pub struct Encoder {
    inner: EncoderCodec,
    config: Config,
    lookahead: usize,
}
impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder")
            .field("config", &self.config)
            .field("lookahead_samples", &self.lookahead)
            .finish_non_exhaustive()
    }
}
impl Encoder {
    pub fn new_with_config(config: Config) -> Result<Self, String> {
        config.validate()?;
        let (inner, lookahead) = match config.codec {
            Codec::Pcm16 => (EncoderCodec::Pcm16, 0),
            Codec::Opus => {
                let mut encoder = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opus::Application::LowDelay)
                    .map_err(|e| e.to_string())?;
                encoder
                    .set_bitrate(opus::Bitrate::Bits(config.bitrate as i32))
                    .map_err(|e| e.to_string())?;
                encoder.set_vbr(false).map_err(|e| e.to_string())?;
                if config.frame_duration == FrameDuration::Ms5 {
                    // The negotiated 5 ms profile uses 320 kbit/s CBR (200 bytes)
                    // and preserves stereo/fullband even for mono-like input.
                    // LowDelay above is OPUS_APPLICATION_RESTRICTED_LOWDELAY;
                    // 5 ms is CELT-only, so SILK in-band FEC does not apply.
                    encoder
                        .set_force_channels(Some(opus::Channels::Stereo))
                        .map_err(|e| e.to_string())?;
                    encoder
                        .set_bandwidth(opus::Bandwidth::Fullband)
                        .map_err(|e| e.to_string())?;
                    encoder.set_complexity(10).map_err(|e| e.to_string())?;
                    encoder.set_dtx(false).map_err(|e| e.to_string())?;
                    encoder.set_inband_fec(false).map_err(|e| e.to_string())?;
                }
                let lookahead = encoder.get_lookahead().map_err(|e| e.to_string())? as usize;
                (EncoderCodec::Opus(encoder), lookahead)
            }
        };
        Ok(Self {
            inner,
            config,
            lookahead,
        })
    }
    pub fn reset(&mut self) -> Result<(), &'static str> {
        if let EncoderCodec::Opus(encoder) = &mut self.inner {
            encoder.reset_state().map_err(|_| "Opus reset failed")?;
        }
        Ok(())
    }
    pub fn config(&self) -> Config {
        self.config
    }
    pub fn lookahead_samples(&self) -> usize {
        self.lookahead
    }
    pub fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>, String> {
        let mut packet = [0; MAX_PACKET_BYTES];
        let size = self.encode_into(samples, &mut packet).map_err(str::to_owned)?;
        Ok(packet[..size].to_vec())
    }
    /// Fixed-storage encoder entry point for the dedicated audio worker.
    pub fn encode_into(&mut self, samples: &[f32], packet: &mut [u8; MAX_PACKET_BYTES]) -> Result<usize, &'static str> {
        if samples.len() != self.config.frame_samples() || samples.iter().any(|v| !v.is_finite()) {
            return Err("invalid finite interleaved sample count");
        }
        match &mut self.inner {
            EncoderCodec::Opus(encoder) => encoder.encode_float(samples, packet).map_err(|_| "Opus encode failed"),
            EncoderCodec::Pcm16 => {
                for (sample, bytes) in samples.iter().zip(packet.as_chunks_mut::<2>().0) {
                    bytes.copy_from_slice(&((sample.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
                }
                Ok(samples.len() * 2)
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub received: u64,
    /// Concealed codec frames, not a network packet loss count.
    pub lost: u64,
    pub late: u64,
    pub duplicates: u64,
    pub rejected: u64,
    pub decode_errors: u64,
    pub rebufferings: u64,
    pub queued_frames: usize,
    pub buffered_ms: f64,
    pub drift_ppm: f64,
    pub frame_duration_ms: f64,
    pub initial_target_ms: f64,
    pub current_target_ms: f64,
    pub min_target_ms: f64,
    pub max_target_ms: f64,
    pub packet_queue_ms: f64,
    pub decoded_queue_ms: f64,
    pub peak_buffered_ms: f64,
    /// RFC-style smoothed absolute change of transit time, not one-way delay.
    pub arrival_jitter_ms: f64,
    /// p99 minus minimum transit over the last second (unsynchronised clocks OK).
    /// Uses sender emission time when supplied; sequence fallback includes sender pacing.
    pub arrival_spread_ms: f64,
    /// Packets observed with a sender monotonic emission timestamp.
    pub sender_timed_packets: u64,
    pub startup_hold_frames: u64,
    pub startup_wait_ms: f64,
    pub adaptation_hold_frames: u64,
    pub adaptation_hold_ms: f64,
    /// Applied temporary rate correction for a changed jitter target, separate from
    /// oscillator drift. Negative builds reserve; positive drains it smoothly.
    pub adaptation_ppm: f64,
    pub adaptation_stretched_ms: f64,
    pub adaptation_compressed_ms: f64,
    pub target_increases: u64,
    pub target_decreases: u64,
    pub gap_plc_frames: u64,
    /// Includes source silence/outage and an unannounced end of stream.
    pub empty_queue_plc_frames: u64,
    pub plc_ms: f64,
    pub latency_trimmed_frames: u64,
    pub latency_trimmed_ms: f64,
    pub boundary_hold_samples: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderKind {
    Audio,
    Concealment,
    StartupSilence,
    AdaptiveHold,
    #[default]
    IdleSilence,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderTrace {
    pub sequence: Option<u64>,
    /// Input time offset within `sequence` for the first output sample,
    /// including fractional resampling phase. Diagnostic only; no wire change.
    #[serde(default)]
    pub sequence_offset_ns: u64,
    pub last_sequence: Option<u64>,
    pub decoded_first_sequence: Option<u64>,
    pub decoded_last_sequence: Option<u64>,
    pub decode_ns: u64,
    pub resample_ns: u64,
    pub packet_wait_ns: Option<u64>,
    pub kind: RenderKind,
}
#[derive(Clone, Copy)]
struct QueuedPacket {
    payload: [u8; MAX_PACKET_BYTES],
    len: usize,
    arrival: Instant,
}
// Sorted, preallocated storage. Insert/remove shift at most the existing codec
// queue limit; unlike BTreeMap they never allocate or free in the work loop.
struct PacketQueue(Vec<(u64, QueuedPacket)>);
impl PacketQueue {
    fn new(capacity: usize) -> Self {
        let mut storage = vec![
            (
                0,
                QueuedPacket {
                    payload: [0; MAX_PACKET_BYTES],
                    len: 0,
                    arrival: Instant::now(),
                }
            );
            capacity
        ];
        std::hint::black_box(&mut storage);
        storage.clear();
        Self(storage)
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    fn contains_key(&self, key: &u64) -> bool {
        self.0.binary_search_by_key(key, |(seq, _)| *seq).is_ok()
    }
    fn first_key_value(&self) -> Option<(&u64, &QueuedPacket)> {
        self.0.first().map(|(seq, packet)| (seq, packet))
    }
    fn values(&self) -> impl Iterator<Item = &QueuedPacket> {
        self.0.iter().map(|(_, packet)| packet)
    }
    fn insert(&mut self, seq: u64, packet: QueuedPacket) {
        if let Err(at) = self.0.binary_search_by_key(&seq, |(seq, _)| *seq)
            && self.0.len() < self.0.capacity()
        {
            self.0.insert(at, (seq, packet));
        }
    }
    fn remove(&mut self, seq: &u64) -> Option<QueuedPacket> {
        self.0
            .binary_search_by_key(seq, |(seq, _)| *seq)
            .ok()
            .map(|at| self.0.remove(at).1)
    }
}
fn packet_capacity(config: Config) -> usize {
    if config.is_opus_10ms() {
        MAX_QUEUE_FRAMES
    } else {
        config.jitter.max_frames + (20.0 / config.frame_duration_ms()).ceil() as usize
    }
}
struct DecodedSpan {
    sequence: u64,
    frames: usize,
    arrival: Option<Instant>,
    concealed: bool,
}

enum DecoderCodec {
    Opus(opus::Decoder),
    Pcm16,
}

/// Local rejection reasons for one diagnostic reporting window.
#[derive(Debug, Default, Clone, Copy)]
pub struct Rejections {
    pub invalid_payload: u64,
    pub queue_capacity: u64,
    pub sequence_window: u64,
}

/// Render on the sink's actual consumption clock, not a network-arrival timer.
/// `push_at` must use the packet's local arrival time. No clocks are subtracted
/// across devices; sender-timed pushes compare relative clock intervals. The
/// sequence-only fallback includes source scheduling variation in its estimate.
/// Diagnostic observation only. `consumed_at` is the local render decision that
/// decoded/concealed this sequence, not an estimated hardware playout deadline.
#[derive(Clone, Copy, Debug)]
pub struct LatePacket {
    pub sequence: u64,
    pub consumed_at: Option<Instant>,
}

pub struct Receiver {
    consumed: [Option<(u64, Instant)>; 512],
    late_packet: Option<LatePacket>,
    rejections: Rejections,
    config: Config,
    decoder: DecoderCodec,
    packets: PacketQueue,
    next: Option<u64>,
    last_decoded: Option<u64>,
    prefill_ticks: usize,
    first_packet: Option<Instant>,
    consecutive_loss: usize,
    empty_queue_loss: usize,
    healthy_frames: usize,
    recovering: bool,
    resampler: DriftResampler,
    spans: VecDeque<DecodedSpan>,
    drift: DriftController,
    adaptive: AdaptiveJitter,
    adjustment: PlayoutAdjustment,
    wake_backlog: bool,
    draining: bool,
    last_output: [f32; CHANNELS],
    stats: Stats,
}
impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("config", &self.config)
            .field("next", &self.next)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}
impl Receiver {
    pub fn new_with_config(config: Config) -> Result<Self, String> {
        config.validate()?;
        let decoder = match config.codec {
            Codec::Opus => {
                DecoderCodec::Opus(opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).map_err(|e| e.to_string())?)
            }
            Codec::Pcm16 => DecoderCodec::Pcm16,
        };
        Ok(Self {
            config,
            decoder,
            packets: PacketQueue::new(packet_capacity(config)),
            next: None,
            last_decoded: None,
            consumed: [None; 512],
            late_packet: None,
            prefill_ticks: 0,
            first_packet: None,
            consecutive_loss: 0,
            empty_queue_loss: 0,
            healthy_frames: 0,
            recovering: false,
            resampler: DriftResampler::new(CHANNELS),
            spans: prepared_deque(MAX_RESAMPLE_FRAMES / config.frame_size() + 2, || DecodedSpan {
                sequence: 0,
                frames: 0,
                arrival: None,
                concealed: false,
            }),
            drift: DriftController::default(),
            adaptive: AdaptiveJitter::new(config),
            adjustment: PlayoutAdjustment::default(),
            wake_backlog: false,
            draining: false,
            last_output: [0.0; CHANNELS],
            stats: Stats::default(),
            rejections: Rejections::default(),
        })
    }
    pub fn take_rejections(&mut self) -> Rejections {
        std::mem::take(&mut self.rejections)
    }
    pub fn config(&self) -> Config {
        self.config
    }
    pub fn push(&mut self, seq: u64, payload: Vec<u8>) -> bool {
        self.push_at(seq, payload, Instant::now())
    }
    pub fn push_at(&mut self, seq: u64, payload: Vec<u8>, arrival: Instant) -> bool {
        self.push_slice_at(seq, &payload, arrival, None)
    }
    /// Sender time is monotonic nanoseconds in the sender's own epoch. Only
    /// differences are used: clocks need not share an epoch or be synchronised.
    /// Supply consistently for a stream; a change of timing mode starts a fresh
    /// estimation window. Older peers can continue using `push_at`.
    pub fn push_with_source_time_at(
        &mut self,
        seq: u64,
        payload: Vec<u8>,
        arrival: Instant,
        source_emit_ns: u64,
    ) -> bool {
        self.push_slice_at(seq, &payload, arrival, Some(source_emit_ns))
    }
    pub fn push_slice_at(&mut self, seq: u64, payload: &[u8], arrival: Instant, source_emit_ns: Option<u64>) -> bool {
        self.late_packet = None;
        let valid = valid_payload(self.config, payload);
        if !valid {
            self.rejections.invalid_payload += 1;
            self.stats.rejected += 1;
            return false;
        }
        if self.packets.contains_key(&seq) {
            self.stats.duplicates += 1;
            return false;
        }
        // Include genuinely late arrivals in the variation estimator, but not
        // duplicate/replayed sequences or arbitrary far-future sequence jumps.
        let maximum = self.queue_capacity();
        let reference = self
            .last_decoded
            .or(self.next)
            .or_else(|| self.packets.first_key_value().map(|(&seq, _)| seq));
        let plausible = reference.is_none_or(|last| seq.abs_diff(last) <= maximum as u64 * 2);
        if self.config.jitter.adaptive && plausible {
            let previous = self.adaptive.target;
            self.adaptive.observe(seq, arrival, source_emit_ns);
            self.stats.sender_timed_packets += u64::from(source_emit_ns.is_some());
            if self.adaptive.target > previous {
                self.stats.target_increases += 1;
            } else if self.adaptive.target < previous {
                self.stats.target_decreases += 1;
            }
            if self.adaptive.target != previous {
                self.adjustment
                    .change_target((self.adaptive.target as f64 - previous as f64) * self.config.frame_size() as f64);
                self.drift.filtered_error = 0.0;
            }
        }
        if self.last_decoded.is_some_and(|last| seq <= last) {
            self.late_packet = Some(LatePacket {
                sequence: seq,
                consumed_at: self.consumed[seq as usize % self.consumed.len()]
                    .filter(|(recorded, _)| *recorded == seq)
                    .map(|(_, at)| at),
            });
            self.stats.late += 1;
            return false;
        }
        let first = self
            .next
            .or_else(|| self.packets.first_key_value().map(|(&seq, _)| seq));
        let full = self.packets.len() >= maximum
            || (!self.config.is_opus_10ms()
                && self.occupancy() + self.config.frame_size() > maximum * self.config.frame_size());
        let outside = first.is_some_and(|first| seq.abs_diff(first) >= maximum as u64);
        if full || outside {
            // Reasons may overlap. These counters describe local rejection,
            // not transport loss, and are drained only for diagnostic logging.
            self.rejections.queue_capacity += u64::from(full);
            self.rejections.sequence_window += u64::from(outside);
            self.stats.rejected += 1;
            return false;
        }
        self.first_packet.get_or_insert(arrival);
        let mut packet = QueuedPacket {
            payload: [0; MAX_PACKET_BYTES],
            len: payload.len(),
            arrival,
        };
        packet.payload[..payload.len()].copy_from_slice(payload);
        self.packets.insert(seq, packet);
        self.stats.received += 1;
        self.update_peak();
        true
    }
    fn queue_capacity(&self) -> usize {
        // Existing queue budget includes 20 ms batch headroom, not a new target.
        packet_capacity(self.config)
    }
    pub(crate) fn packet_capacity(&self) -> usize {
        self.queue_capacity()
    }
    pub(crate) fn buffered_frames(&self) -> usize {
        // Fixed-profile admission only counts packets; low-latency admission includes
        // decoded reserve. Preserve both policies across the ingress handoff.
        if self.config.is_opus_10ms() {
            self.packets.len() * self.config.frame_size()
        } else {
            self.occupancy()
        }
    }
    fn occupancy(&self) -> usize {
        self.packets.len() * self.config.frame_size() + self.resampler.buffered_frames()
    }
    fn update_peak(&mut self) {
        self.stats.peak_buffered_ms = self.stats.peak_buffered_ms.max(self.occupancy() as f64 / 48.0);
    }
    pub fn stats(&self) -> Stats {
        let ms = self.config.frame_duration_ms();
        Stats {
            queued_frames: self.packets.len(),
            buffered_ms: self.occupancy() as f64 / 48.0,
            drift_ppm: self.drift.ppm,
            frame_duration_ms: ms,
            initial_target_ms: self.config.jitter.initial_frames as f64 * ms,
            current_target_ms: self.adaptive.target as f64 * ms,
            min_target_ms: self.config.jitter.min_frames as f64 * ms,
            max_target_ms: self.config.jitter.max_frames as f64 * ms,
            packet_queue_ms: self.packets.len() as f64 * ms,
            decoded_queue_ms: self.resampler.buffered_frames() as f64 / 48.0,
            arrival_jitter_ms: self.adaptive.jitter_ms,
            arrival_spread_ms: self.adaptive.spread_ms,
            adaptation_hold_ms: self.stats.adaptation_hold_frames as f64 * ms,
            plc_ms: self.stats.lost as f64 * ms,
            latency_trimmed_ms: self.stats.latency_trimmed_frames as f64 * ms,
            ..self.stats
        }
    }
    /// Drains diagnostic state only. Missing timestamps mean the bounded history
    /// no longer covers the sequence; never substitute sequence distance as time.
    pub fn take_late_packet(&mut self) -> Option<LatePacket> {
        self.late_packet.take()
    }

    pub fn render(&mut self) -> Vec<f32> {
        self.render_at(Instant::now())
    }
    pub fn render_at(&mut self, now: Instant) -> Vec<f32> {
        self.render_inner(now, self.config.frame_size(), None)
    }
    /// Render one codec frame while accounting for the caller's remaining
    /// native callback demand (in 48 kHz stereo frames, including this frame).
    /// A batched callback must retain the jitter reserve after the whole batch,
    /// rather than letting drift correction consume it between codec frames.
    pub fn render_for_demand(&mut self, frames: usize) -> Vec<f32> {
        self.render_inner(Instant::now(), frames, None)
    }
    pub fn render_for_demand_with_trace(&mut self, now: Instant, frames: usize) -> (Vec<f32>, RenderTrace) {
        let mut trace = RenderTrace::default();
        let samples = self.render_inner(now, frames, Some(&mut trace));
        (samples, trace)
    }
    pub fn render_with_trace(&mut self, now: Instant) -> (Vec<f32>, RenderTrace) {
        let mut trace = RenderTrace::default();
        let samples = self.render_inner(now, self.config.frame_size(), Some(&mut trace));
        (samples, trace)
    }
    fn silence(&self) -> Vec<f32> {
        vec![0.0; self.config.frame_samples()]
    }
    fn render_inner(&mut self, now: Instant, demand_frames: usize, trace: Option<&mut RenderTrace>) -> Vec<f32> {
        let mut output = self.silence();
        self.render_into(now, demand_frames, &mut output, trace);
        output
    }
    /// Render into caller-owned, exact codec-frame storage without heap work.
    pub fn render_into(
        &mut self,
        now: Instant,
        demand_frames: usize,
        output: &mut [f32],
        mut trace: Option<&mut RenderTrace>,
    ) {
        output.fill(0.0);
        if output.len() != self.config.frame_samples() {
            return;
        }
        let frame_size = self.config.frame_size();
        let fixed_profile = self.config.is_opus_10ms();
        // The jitter reserve is additional to all samples already demanded by
        // this native callback, not just the one codec frame returned here.
        let target = if fixed_profile {
            self.adaptive.target * frame_size
        } else {
            (self.adaptive.target * frame_size)
                .saturating_add(demand_frames.max(frame_size))
                .min(self.queue_capacity() * frame_size)
        };
        if self.next.is_none() {
            if self.packets.is_empty() {
                return;
            }
            self.prefill_ticks += 1;
            let waiting = if self.recovering && !self.draining {
                // After starvation, old packet timestamps are not evidence of
                // reserve. Wait for the actual demand + target to refill. Cold
                // onset keeps its bounded wait; idle drain can release a finite tail.
                self.occupancy() < target
            } else if fixed_profile {
                self.packets.len() < TARGET_FRAMES && self.prefill_ticks < TARGET_FRAMES
            } else {
                let waited = self
                    .first_packet
                    .and_then(|first| now.checked_duration_since(first))
                    .unwrap_or_default();
                self.occupancy() < target
                    && waited < Duration::from_secs_f64((target - frame_size) as f64 / f64::from(SAMPLE_RATE))
            };
            if waiting {
                self.stats.startup_hold_frames += 1;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.kind = RenderKind::StartupSilence;
                }
                return;
            }
            self.stats.startup_wait_ms = self
                .first_packet
                .and_then(|first| now.checked_duration_since(first))
                .unwrap_or_default()
                .as_secs_f64()
                * 1000.0;
            self.next = self.packets.first_key_value().map(|(&seq, _)| seq);
            self.prefill_ticks = 0;
            self.recovering = false;
            // Re-prefill can overshoot when old and new packets arrive in one
            // batch. Track the actual queue rather than treating a full batch
            // as proof that the requested reserve has been built exactly.
            self.adjustment = PlayoutAdjustment::default();
            if (!fixed_profile && self.stats.target_increases > 0) || self.wake_backlog {
                self.adjustment.change_target(0.0);
            }
        }
        let error = self.occupancy() as f64 - target as f64;
        let adapting = (!fixed_profile || self.wake_backlog) && self.adjustment.active();
        let clock_ppm = if adapting {
            // A deliberate change of reserve is not oscillator drift. Keep
            // the learned clock rate while its extra samples are built/drained.
            self.drift.ppm
        } else if fixed_profile {
            self.drift.update(error)
        } else {
            self.drift.update_low_latency(error, self.config.frame_duration_ms())
        };
        let adaptation_ppm = if adapting {
            // Wake catch-up caps compression at +1%; it must still allow the
            // existing -2% reserve-building direction. Clamping negative rate
            // to zero left the adjustment engaged (and drift frozen) forever.
            self.adjustment.update(error, frame_size)
        } else {
            0.0
        };
        let ppm = if self.wake_backlog {
            (clock_ppm + adaptation_ppm).min(10_000.0)
        } else {
            clock_ppm + adaptation_ppm
        };
        if !self.adjustment.active() {
            self.wake_backlog = false;
        }
        let required = if fixed_profile {
            self.resampler.required_frames(frame_size, ppm)
        } else {
            self.resampler.required_frames_without_lookahead(frame_size, ppm)
        };
        while self.resampler.buffered_frames() < required {
            // At an idle handover the remaining received media is finite. Fill the
            // rest with zeros instead of manufacturing a PLC tail or dropping PCM.
            if self.draining && self.packets.is_empty() {
                break;
            }
            let Some(seq) = self.next else {
                break;
            };
            // Clock correction can put the last output sample one input
            // sample beyond the current codec frame. Do not advance the codec
            // and conceal an entire not-yet-arrived packet just for that sample.
            // This is bounded to one sample, with an otherwise complete frame;
            // a real missing codec frame still follows the ordinary PLC path.
            if !fixed_profile
                && self.packets.is_empty()
                && self.resampler.buffered_frames() >= frame_size - 1
                && required - self.resampler.buffered_frames() == 1
            {
                break;
            }
            if !self.packets.contains_key(&seq)
                && (self.consecutive_loss >= self.config.jitter.max_plc_frames
                    || (self.packets.is_empty() && self.empty_queue_loss >= self.config.jitter.max_plc_frames))
            {
                self.rebuffer();
                return;
            }
            if !self.decode_next(now, trace.as_deref_mut()) {
                return;
            }
        }
        if let Some(trace) = trace.as_deref_mut()
            && let Some(first) = self.spans.front()
        {
            trace.sequence = Some(first.sequence);
            trace.sequence_offset_ns = ((frame_size.saturating_sub(first.frames) as f64 + self.resampler.phase) * 1e9
                / f64::from(SAMPLE_RATE))
            .round() as u64;
            trace.packet_wait_ns = first
                .arrival
                .and_then(|arrival| now.checked_duration_since(arrival))
                .map(duration_ns);
            trace.kind = if first.concealed {
                RenderKind::Concealment
            } else {
                RenderKind::Audio
            };
            let mut span_frames = 0;
            for span in &self.spans {
                trace.last_sequence = Some(span.sequence);
                span_frames += span.frames;
                if span_frames
                    >= self
                        .resampler
                        .required_frames(frame_size, ppm)
                        .min(self.resampler.buffered_frames())
                {
                    break;
                }
            }
        }
        let before = self.resampler.buffered_frames();
        let started = trace.as_ref().map(|_| Instant::now());
        let boundary_holds = self.resampler.render_into_impl(
            output,
            if fixed_profile && !adapting {
                ppm.clamp(-1000.0, 1000.0)
            } else {
                ppm
            },
            !fixed_profile || self.draining,
        );
        if let (Some(trace), Some(started)) = (trace, started) {
            trace.resample_ns += duration_ns(started.elapsed());
        }
        self.stats.boundary_hold_samples += boundary_holds as u64;
        // Report the rate actually given to the resampler, including the wake
        // compression ceiling, rather than an unapplied controller request.
        let applied_adaptation_ppm = ppm - clock_ppm;
        self.stats.adaptation_ppm = applied_adaptation_ppm;
        let adjusted_ms = applied_adaptation_ppm * self.config.frame_duration_ms() / 1_000_000.0;
        self.stats.adaptation_stretched_ms += (-adjusted_ms).max(0.0);
        self.stats.adaptation_compressed_ms += adjusted_ms.max(0.0);
        self.consume_spans(before - self.resampler.buffered_frames());
        if let Some(last) = output.as_chunks::<CHANNELS>().0.last() {
            self.last_output.copy_from_slice(last);
        }
    }
    fn decode_next(&mut self, now: Instant, trace: Option<&mut RenderTrace>) -> bool {
        let Some(seq) = self.next else {
            return false;
        };
        let started = trace.as_ref().map(|_| Instant::now());
        let packet = self.packets.remove(&seq);
        let arrival = packet.as_ref().map(|packet| packet.arrival);
        let mut storage = [0.0; FRAME_SAMPLES];
        let pcm = &mut storage[..self.config.frame_samples()];
        let valid = match (&mut self.decoder, packet) {
            (DecoderCodec::Opus(decoder), Some(packet)) => {
                match decoder.decode_float(&packet.payload[..packet.len], pcm, false) {
                    Ok(size) if size == self.config.frame_size() => true,
                    _ => {
                        self.stats.decode_errors += 1;
                        false
                    }
                }
            }
            (DecoderCodec::Pcm16, Some(packet)) => {
                for (sample, bytes) in pcm
                    .iter_mut()
                    .zip(packet.payload[..packet.len].as_chunks::<2>().0.iter())
                {
                    *sample = (f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32767.0).max(-1.0);
                }
                true
            }
            (_, None) => false,
        };
        if valid {
            self.consecutive_loss = 0;
            self.healthy_frames = (self.healthy_frames + self.config.frame_size()).min(SAMPLE_RATE as usize);
            if self.healthy_frames == SAMPLE_RATE as usize {
                self.empty_queue_loss = 0;
            }
        } else {
            self.healthy_frames = 0;
            self.stats.lost += 1;
            if self.packets.is_empty() {
                self.stats.empty_queue_plc_frames += 1;
                // Successful frames interleaved with empty-queue PLC must not
                // hide persistent starvation. One second of uninterrupted valid
                // media clears this episode, matching reserve-settling time.
                self.empty_queue_loss += 1;
            } else {
                self.stats.gap_plc_frames += 1;
            }
            self.consecutive_loss += 1;
            if self.consecutive_loss > self.config.jitter.max_plc_frames {
                self.rebuffer();
                return false;
            }
            match &mut self.decoder {
                DecoderCodec::Opus(decoder) => {
                    if decoder.decode_float(&[], pcm, false).ok() != Some(self.config.frame_size()) {
                        pcm.fill(0.0);
                    }
                }
                DecoderCodec::Pcm16 => {
                    // PCM has no codec PLC. Fade the last output sample to zero
                    // once, avoiding both invented signal and an indefinite tone.
                    for (i, frame) in pcm.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
                        let gain = if self.consecutive_loss == 1 {
                            1.0 - (i + 1) as f32 / self.config.frame_size() as f32
                        } else {
                            0.0
                        };
                        for (channel, sample) in frame.iter_mut().enumerate() {
                            *sample = self.last_output[channel] * gain;
                        }
                    }
                }
            }
        }
        self.resampler.push(pcm);
        self.spans.push_back(DecodedSpan {
            sequence: seq,
            frames: self.config.frame_size(),
            arrival,
            concealed: !valid,
        });
        self.consumed[seq as usize % self.consumed.len()] = Some((seq, now));
        self.last_decoded = Some(seq);
        self.next = seq.checked_add(1);
        if let Some(trace) = trace {
            trace.decoded_first_sequence.get_or_insert(seq);
            trace.decoded_last_sequence = Some(seq);
            trace.decode_ns += duration_ns(started.unwrap().elapsed());
        }
        true
    }
    fn consume_spans(&mut self, mut frames: usize) {
        while frames > 0 {
            let Some(span) = self.spans.front_mut() else {
                break;
            };
            let consumed = frames.min(span.frames);
            span.frames -= consumed;
            frames -= consumed;
            if span.frames == 0 {
                self.spans.pop_front();
            }
        }
    }
    pub(crate) fn set_draining(&mut self, draining: bool) {
        self.draining = draining;
    }
    /// New media burst, using the existing preallocated storage on the RT worker.
    pub fn reset_burst(&mut self) {
        self.packets.0.clear();
        self.rebuffer();
        self.consumed.fill(None);
        self.late_packet = None;
        self.last_decoded = None;
        self.adaptive.arrivals.clear();
        self.adaptive.target = self.config.jitter.initial_frames;
        self.adaptive.anchor = None;
        self.adaptive.jitter_ms = 0.0;
        self.adaptive.spread_ms = 0.0;
        self.adaptive.lower_candidate = None;
        self.adaptive.last_decay = None;
        self.stats = Stats::default();
        self.rejections = Rejections::default();
        self.wake_backlog = true;
        self.recovering = false;
        self.draining = false;
    }
    fn rebuffer(&mut self) {
        self.next = None;
        self.prefill_ticks = 0;
        self.first_packet = self.packets.values().map(|packet| packet.arrival).min();
        self.consecutive_loss = 0;
        self.empty_queue_loss = 0;
        self.healthy_frames = 0;
        self.recovering = true;
        self.resampler.clear();
        self.spans.clear();
        self.drift = DriftController::default();
        self.adjustment = PlayoutAdjustment::default();
        self.stats.adaptation_ppm = 0.0;
        self.last_output = [0.0; CHANNELS];
        if let DecoderCodec::Opus(decoder) = &mut self.decoder {
            let _ = decoder.reset_state();
        }
        // Keep transport history across a playout outage: otherwise the very
        // burst that emptied the buffer would erase the evidence for raising
        // its reserve. observe() expires old samples by arrival time.
        self.stats.rebufferings += 1;
    }
}
fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

struct Arrival {
    sequence: u64,
    at: Instant,
    transit_ms: f64,
}
struct AdaptiveJitter {
    config: Config,
    target: usize,
    arrivals: VecDeque<Arrival>,
    anchor: Option<(u64, Instant, Option<u64>)>,
    jitter_ms: f64,
    spread_ms: f64,
    lower_candidate: Option<(Instant, usize)>,
    last_decay: Option<Instant>,
}
impl AdaptiveJitter {
    fn new(config: Config) -> Self {
        Self {
            config,
            target: config.jitter.initial_frames,
            arrivals: prepared_deque(MAX_ARRIVALS + 1, || Arrival {
                sequence: 0,
                at: Instant::now(),
                transit_ms: 0.0,
            }),
            anchor: None,
            jitter_ms: 0.0,
            spread_ms: 0.0,
            lower_candidate: None,
            last_decay: None,
        }
    }
    fn observe(&mut self, sequence: u64, at: Instant, source_emit_ns: Option<u64>) {
        if self
            .anchor
            .is_some_and(|(_, _, source)| source.is_some() != source_emit_ns.is_some())
        {
            self.arrivals.clear();
            self.anchor = None;
            self.jitter_ms = 0.0;
            self.lower_candidate = None;
            self.last_decay = None;
        }
        if self.arrivals.iter().any(|arrival| arrival.sequence == sequence) {
            return;
        }
        if self.arrivals.back().is_some_and(|last| at < last.at) {
            return;
        }
        let (first_seq, first_at, first_source) = *self.anchor.get_or_insert((sequence, at, source_emit_ns));
        let elapsed = at.checked_duration_since(first_at).unwrap_or_default().as_secs_f64() * 1000.0;
        let seq_delta = sequence.abs_diff(first_seq) as f64 * if sequence >= first_seq { 1.0 } else { -1.0 };
        let source_elapsed = match (source_emit_ns, first_source) {
            (Some(now), Some(first)) => now.abs_diff(first) as f64 / 1e6 * if now >= first { 1.0 } else { -1.0 },
            _ => seq_delta * self.config.frame_duration_ms(),
        };
        let transit = elapsed - source_elapsed;
        if let Some(previous) = self.arrivals.back() {
            self.jitter_ms += ((transit - previous.transit_ms).abs() - self.jitter_ms) / 16.0;
        }
        self.arrivals.push_back(Arrival {
            sequence,
            at,
            transit_ms: transit,
        });
        while self.arrivals.len() > MAX_ARRIVALS
            || self
                .arrivals
                .front()
                .is_some_and(|a| at.duration_since(a.at) > ARRIVAL_WINDOW)
        {
            self.arrivals.pop_front();
        }
        let mut values = [0.0_f64; MAX_ARRIVALS];
        for (value, arrival) in values.iter_mut().zip(&self.arrivals) {
            *value = arrival.transit_ms;
        }
        let values = &mut values[..self.arrivals.len()];
        values.sort_unstable_by(f64::total_cmp);
        // Retain the upper envelope of a burst, while ignoring one isolated
        // late/corrupt timestamp. A 99th percentile in a 400-packet window
        // discards 10 ms of a 200 ms batch and under-protects its next arrival.
        let upper = values.len().saturating_sub(2);
        self.spread_ms = (values[upper] - values[0]).max(0.0);
        // Half a frame of deadband avoids increasing delay for tiny scheduling
        // noise/clock drift. Capacity alone does not increase the target: a
        // steady link keeps the same 5 ms minimum, even with a 250 ms ceiling.
        let frame_ms = self.config.frame_duration_ms();
        let extra = (((self.spread_ms - frame_ms * 0.5).max(0.0) / frame_ms).ceil() as usize)
            .min(self.config.jitter.max_frames);
        let desired =
            (self.config.jitter.min_frames + extra).clamp(self.config.jitter.min_frames, self.config.jitter.max_frames);
        if desired > self.target {
            self.target = desired;
            self.lower_candidate = None;
            self.last_decay = None;
        } else if desired < self.target {
            // Retain protection through recurring wireless stalls. After the
            // one-second measurement window clears, require three seconds of
            // lower demand, then lower by at most 5 ms per 500 ms. Receiver
            // drains the resulting reserve by rate correction, never a skip.
            let (since, candidate) = self.lower_candidate.get_or_insert((at, desired));
            *candidate = (*candidate).max(desired);
            if at.duration_since(*since) >= TARGET_STABLE_INTERVAL
                && self
                    .last_decay
                    .is_none_or(|last| at.duration_since(last) >= TARGET_DECAY_INTERVAL)
            {
                let step = (5.0 / frame_ms).ceil() as usize;
                self.target = self.target.saturating_sub(step).max(*candidate);
                *candidate = desired;
                self.last_decay = Some(at);
            }
        } else {
            self.lower_candidate = None;
            self.last_decay = None;
        }
    }
}

/// Deliberate reserve changes use a continuous resampling phase, not frame
/// deletion or inserted silence. Building at <=2% takes at least 10 s for
/// 200 ms; a buffer already emptied by the first burst still needs the normal
/// bounded PLC/re-prefill path. Draining at <=1% takes at least 20 s. These
/// small rate changes can shift pitch (about -35/+17 cents); no pitch-preserving
/// time stretcher is claimed. With no target changes this is exactly inactive.
#[derive(Default)]
struct PlayoutAdjustment {
    engaged: bool,
    filtered_error: f64,
    settled_frames: usize,
    ppm: f64,
}
impl PlayoutAdjustment {
    fn change_target(&mut self, delta_frames: f64) {
        self.engaged = true;
        self.settled_frames = 0;
        self.filtered_error -= delta_frames;
    }
    fn active(&self) -> bool {
        self.engaged
    }
    fn update(&mut self, occupancy_error: f64, frame_size: usize) -> f64 {
        // One full arrival batch does not imply a full playout reserve. Follow
        // a 250 ms smoothed occupancy error until it stays near the requested
        // reserve for a second; this also drains overshoot after re-prefill.
        // With no target change this controller is never engaged, so a healthy
        // stream still uses exactly the original oscillator drift path.
        let dt = frame_size as f64 / f64::from(SAMPLE_RATE);
        self.filtered_error += (1.0 - (-dt / 0.250).exp()) * (occupancy_error - self.filtered_error);
        let mut requested = (self.filtered_error * 20.0).clamp(-20_000.0, 10_000.0);
        // Never deliberately drain a reserve which is currently below target.
        // The phase/rate still slews continuously; a packet gap is not a skip.
        if occupancy_error < 0.0 && requested > 0.0 {
            requested = 0.0;
        }
        // 0 -> 2% takes 200 ms; consecutive codec calls share the same phase.
        let slew = frame_size as f64 / 48.0 * 100.0;
        self.ppm += (requested - self.ppm).clamp(-slew, slew);
        if self.filtered_error.abs() < frame_size as f64 * 0.5 && self.ppm.abs() < 1000.0 {
            self.settled_frames += frame_size;
            if self.settled_frames >= SAMPLE_RATE as usize {
                self.engaged = false;
                self.ppm = 0.0;
            }
        } else {
            self.settled_frames = 0;
        }
        self.ppm
    }
}
#[derive(Debug, Default)]
struct DriftController {
    filtered_error: f64,
    integral: f64,
    ppm: f64,
}

impl DriftController {
    fn update_low_latency(&mut self, error_frames: f64, frame_ms: f64) -> f64 {
        let dt = frame_ms / 10.0;
        self.filtered_error += (1.0 - (1.0_f64 - 0.02).powf(dt)) * (error_frames - self.filtered_error);
        self.integral = (self.integral + self.filtered_error * 0.05 * dt).clamp(-1000.0, 1000.0);
        let target = (self.filtered_error * 8.0 + self.integral).clamp(-1000.0, 1000.0);
        self.ppm += (target - self.ppm).clamp(-10.0 * dt, 10.0 * dt);
        self.ppm
    }

    fn update(&mut self, error_frames: f64) -> f64 {
        self.filtered_error += 0.02 * (error_frames - self.filtered_error);
        self.integral = (self.integral + self.filtered_error * 0.003).clamp(-1000.0, 1000.0);
        let target = (self.filtered_error * 0.5 + self.integral).clamp(-1000.0, 1000.0);
        self.ppm += (target - self.ppm).clamp(-10.0, 10.0);
        self.ppm
    }
}

/// Streaming linear interpolator for small clock corrections (not a general
/// band-limited sample-rate converter). Positive ppm consumes input faster.
#[derive(Debug)]
pub struct DriftResampler {
    samples: VecDeque<f32>,
    channels: usize,
    phase: f64,
}

impl DriftResampler {
    pub fn new(channels: usize) -> Self {
        Self {
            samples: prepared_deque(MAX_RESAMPLE_FRAMES * channels.clamp(1, 32), || 0.0),
            channels: channels.clamp(1, 32),
            phase: 0.0,
        }
    }

    pub fn push(&mut self, samples: &[f32]) {
        for frame in samples.chunks_exact(self.channels) {
            if self.buffered_frames() == MAX_RESAMPLE_FRAMES {
                self.samples.drain(..self.channels);
                self.phase = 0.0;
            }
            self.samples
                .extend(frame.iter().map(|&v| if v.is_finite() { v } else { 0.0 }));
        }
    }

    pub fn buffered_frames(&self) -> usize {
        self.samples.len() / self.channels
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.phase = 0.0;
    }

    fn step(ppm: f64) -> f64 {
        1.0 + if ppm.is_finite() {
            ppm.clamp(-21_000.0, 11_000.0)
        } else {
            0.0
        } / 1_000_000.0
    }

    fn required_frames(&self, frames: usize, ppm: f64) -> usize {
        if frames == 0 {
            return 0;
        }
        (self.phase + (frames - 1) as f64 * Self::step(ppm)).ceil() as usize + 1
    }

    fn required_frames_without_lookahead(&self, frames: usize, ppm: f64) -> usize {
        if frames == 0 {
            return 0;
        }
        (self.phase + (frames - 1) as f64 * Self::step(ppm)).floor() as usize + 1
    }

    pub fn render(&mut self, frames: usize, ppm: f64) -> Vec<f32> {
        self.render_impl(frames, ppm.clamp(-1000.0, 1000.0), false).0
    }

    fn render_impl(&mut self, frames: usize, ppm: f64, hold_boundary: bool) -> (Vec<f32>, usize) {
        let mut out = vec![0.0; frames.min(MAX_RESAMPLE_FRAMES) * self.channels];
        let held = self.render_into_impl(&mut out, ppm, hold_boundary);
        (out, held)
    }
    pub fn render_into(&mut self, out: &mut [f32], ppm: f64) {
        self.render_into_impl(out, ppm.clamp(-1000.0, 1000.0), false);
    }
    fn render_into_impl(&mut self, out: &mut [f32], ppm: f64, hold_boundary: bool) -> usize {
        out.fill(0.0);
        if out.len() > MAX_RESAMPLE_FRAMES * self.channels {
            return 0;
        }
        let mut held_samples = 0;
        let step = Self::step(ppm);
        for frame in out.chunks_exact_mut(self.channels) {
            let index = self.phase.floor() as usize;
            let fraction = self.phase - index as f64;
            if hold_boundary && index == self.buffered_frames() && index > 0 {
                // Extend only the final sample for the bounded clock-correction
                // shortage accepted by Receiver. Discard the unfulfilled phase
                // advance below instead of charging it to the next codec frame.
                held_samples += 1;
                for (channel, sample) in frame.iter_mut().enumerate() {
                    *sample = self.samples[(index - 1) * self.channels + channel];
                }
                self.phase += step;
                continue;
            }
            if index >= self.buffered_frames()
                || (!hold_boundary && fraction > 0.0 && index + 1 >= self.buffered_frames())
            {
                break;
            }
            if fraction > 0.0 && index + 1 >= self.buffered_frames() {
                held_samples += 1;
            }
            for (channel, sample) in frame.iter_mut().enumerate() {
                let a = self.samples[index * self.channels + channel];
                let b = self
                    .samples
                    .get((index + 1) * self.channels + channel)
                    .copied()
                    .unwrap_or(a);
                *sample = a + (b - a) * fraction as f32;
            }
            self.phase += step;
        }
        let consumed = (self.phase.floor() as usize).min(self.buffered_frames());
        self.samples.drain(..consumed * self.channels);
        self.phase -= consumed as f64;
        if hold_boundary && self.samples.is_empty() {
            // Unavailable whole samples were held, not consumed. Keeping that
            // debt would request the next packet early again on the next call.
            self.phase = self.phase.fract();
        }
        held_samples
    }
}

/// Unity-gain summing with a hard output ceiling. Non-finite input is silence;
/// shorter streams are zero-padded. Quiet single streams retain their level.
pub fn mix(inputs: &[&[f32]], out: &mut [f32]) {
    for (index, sample) in out.iter_mut().enumerate() {
        let sum: f64 = inputs
            .iter()
            .filter_map(|input| input.get(index))
            .filter(|v| v.is_finite())
            .map(|&v| f64::from(v))
            .sum();
        *sample = sum.clamp(-1.0, 1.0) as f32;
    }
}

// Populate storage on the ordinary owner before handing it to a real-time
// thread. black_box keeps the prefault writes observable before logical clear.
pub(crate) fn prepared_deque<T>(capacity: usize, mut init: impl FnMut() -> T) -> VecDeque<T> {
    let mut storage = VecDeque::with_capacity(capacity);
    storage.resize_with(capacity, &mut init);
    std::hint::black_box(&mut storage);
    storage.clear();
    storage
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frame: usize) -> Vec<f32> {
        (0..FRAME_SIZE)
            .flat_map(|i| {
                let v =
                    (std::f32::consts::TAU * 440.0 * (frame * FRAME_SIZE + i) as f32 / SAMPLE_RATE as f32).sin() * 0.2;
                [v, -v]
            })
            .collect()
    }

    fn packets(count: usize) -> Vec<Vec<u8>> {
        let mut encoder = Encoder::new_with_config(Config::opus_10ms()).unwrap();
        (0..count).map(|i| encoder.encode(&sine(i)).unwrap()).collect()
    }

    #[test]
    fn fixed_storage_encode_receive_render_and_rebuffer_do_no_heap_work() {
        for config in [
            Config::opus_10ms(),
            Config::low_latency_opus(),
            Config::low_latency_pcm(),
        ] {
            let mut encoder = Encoder::new_with_config(config).unwrap();
            let mut receiver = Receiver::new_with_config(config).unwrap();
            let input = [0.002; FRAME_SAMPLES];
            let mut packet = [0; MAX_PACKET_BYTES];
            let mut output = [0.0; FRAME_SAMPLES];
            let first = Instant::now();
            crate::allocation_audit::assert_no_heap(|| {
                for seq in 0..2400 {
                    let now = first + Duration::from_secs_f64(seq as f64 * config.frame_duration_ms() / 1000.0);
                    let size = encoder
                        .encode_into(&input[..config.frame_samples()], &mut packet)
                        .unwrap();
                    // Exercise steady receive, short PLC, exhaustion/rebuffer,
                    // duplicates and full/window rejection within one audit.
                    if seq % 400 < 360 {
                        receiver.push_slice_at(seq, &packet[..size], now, Some(seq * 5_000_000));
                        receiver.push_slice_at(seq, &packet[..size], now, Some(seq * 5_000_000));
                    }
                    let mut trace = RenderTrace::default();
                    receiver.render_into(
                        now,
                        config.frame_size(),
                        &mut output[..config.frame_samples()],
                        Some(&mut trace),
                    );
                    assert!(output.iter().all(|sample| sample.is_finite()));
                    let _ = receiver.stats();
                    let _ = receiver.take_rejections();
                }
            });
            assert!(receiver.stats().rebufferings > 0);
        }
    }

    #[test]
    fn late_diagnostics_use_consumption_clock_and_do_not_change_rendering() {
        let packets = packets(8);
        let start = Instant::now();
        let mut observed = Receiver::new_with_config(Config::opus_10ms()).unwrap();
        let mut untouched = Receiver::new_with_config(Config::opus_10ms()).unwrap();
        for seq in 0..8 {
            assert!(observed.push_at(seq, packets[seq as usize].clone(), start));
            assert!(untouched.push_at(seq, packets[seq as usize].clone(), start));
        }
        for step in 0..6 {
            let at = start + Duration::from_millis(step * 10);
            assert_eq!(observed.render_at(at), untouched.render_at(at));
            let seq = observed.last_decoded.unwrap();
            let late_at = at + Duration::from_millis(37);
            assert!(!observed.push_at(seq, packets[seq as usize].clone(), late_at));
            assert!(!untouched.push_at(seq, packets[seq as usize].clone(), late_at));
            let event = observed.take_late_packet().unwrap();
            assert_eq!(event.sequence, seq);
            assert_eq!(
                late_at.duration_since(event.consumed_at.unwrap()),
                Duration::from_millis(37)
            );
            assert!(observed.take_late_packet().is_none());
            assert_eq!(
                serde_json::to_value(observed.stats()).unwrap(),
                serde_json::to_value(untouched.stats()).unwrap()
            );
        }
        // Old/replayed sequences outside the diagnostic history have no invented
        // deadline; the receiver's existing late/replay behavior is unchanged.
        observed.consumed[0] = Some((512, start));
        assert!(!observed.push_at(0, packets[0].clone(), start));
        assert!(observed.take_late_packet().unwrap().consumed_at.is_none());
    }

    #[test]
    fn codec_has_fixed_bitrate_and_preserves_stereo_audio() {
        let mut encoder = Encoder::new_with_config(Config::opus_10ms()).unwrap();
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut energy = 0.0;
        let mut stereo_error = 0.0;
        for frame in 0..100 {
            let packet = encoder.encode(&sine(frame)).unwrap();
            assert_eq!(packet.len(), 160);
            let mut pcm = [0.0; FRAME_SAMPLES];
            assert_eq!(decoder.decode_float(&packet, &mut pcm, false).unwrap(), FRAME_SIZE);
            energy += pcm.iter().map(|x| x * x).sum::<f32>();
            stereo_error += pcm.as_chunks::<2>().0.iter().map(|v| (v[0] + v[1]).abs()).sum::<f32>();
        }
        assert!(energy > 1000.0);
        assert!(stereo_error / ((100 * FRAME_SIZE) as f32) < 0.01);
        assert!(encoder.encode(&[0.0]).is_err());
        assert!(encoder.encode(&[f32::NAN; FRAME_SAMPLES]).is_err());
    }

    #[test]
    fn reorder_matches_in_order_decode_and_rejects_duplicates_and_late() {
        let packets = packets(8);
        let mut ordered = Receiver::new_with_config(Config::opus_10ms()).unwrap();
        let mut reordered = Receiver::new_with_config(Config::opus_10ms()).unwrap();
        for seq in 0..8 {
            assert!(ordered.push(seq, packets[seq as usize].clone()));
        }
        for seq in [2, 0, 1, 5, 3, 7, 6, 4] {
            assert!(reordered.push(seq, packets[seq as usize].clone()));
        }
        assert!(!reordered.push(2, packets[2].clone()));
        for _ in 0..6 {
            assert_eq!(ordered.render(), reordered.render());
        }
        assert!(!reordered.push(0, packets[0].clone()));
        assert_eq!(reordered.stats().duplicates, 1);
        assert_eq!(reordered.stats().late, 1);
        assert_eq!(reordered.stats().lost, 0);
    }

    #[test]
    fn losses_use_bounded_plc_then_recover_after_long_outage() {
        let p = packets(3);
        let mut receiver = Receiver::new_with_config(Config::opus_10ms()).unwrap();
        for (i, packet) in p.iter().enumerate() {
            assert!(receiver.push(i as u64, packet.clone()));
        }
        for _ in 0..100 {
            assert_eq!(receiver.render().len(), FRAME_SAMPLES);
        }
        assert_eq!(receiver.stats().lost, MAX_PLC_FRAMES as u64);
        assert_eq!(receiver.stats().rebufferings, 1);
        assert!(receiver.render().iter().all(|&v| v == 0.0));
        for (i, packet) in p.into_iter().enumerate() {
            assert!(receiver.push(1000 + i as u64, packet));
        }
        assert!(receiver.render().iter().any(|&v| v != 0.0));
    }

    #[test]
    fn queue_and_sequence_boundaries_are_bounded() {
        let p = packets(1).remove(0);
        let mut receiver = Receiver::new_with_config(Config::opus_10ms()).unwrap();
        assert!(!receiver.push(0, vec![]));
        assert!(!receiver.push(0, vec![0; MAX_PACKET_BYTES + 1]));
        for seq in 0..MAX_QUEUE_FRAMES as u64 {
            assert!(receiver.push(seq, p.clone()));
        }
        assert!(!receiver.push(u64::MAX, p.clone()));
        assert_eq!(receiver.stats().queued_frames, MAX_QUEUE_FRAMES);
        let mut receiver = Receiver::new_with_config(Config::opus_10ms()).unwrap();
        for seq in (u64::MAX - 2)..=u64::MAX {
            assert!(receiver.push(seq, p.clone()));
        }
        for _ in 0..10 {
            receiver.render();
        }
        assert!(!receiver.push(0, p));
        assert_eq!(receiver.stats().late, 1);
    }

    #[test]
    fn wrong_frame_duration_is_rejected() {
        let mut encoder = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opus::Application::LowDelay).unwrap();
        let packet = encoder.encode_vec_float(&[0.0; FRAME_SAMPLES * 2], 512).unwrap();
        assert!(!Receiver::new_with_config(Config::opus_10ms()).unwrap().push(0, packet));
    }

    #[test]
    fn prefill_wait_is_bounded_and_silent() {
        let mut receiver = Receiver::new_with_config(Config::opus_10ms()).unwrap();
        receiver.push(0, packets(1).remove(0));
        for _ in 0..2 {
            assert!(receiver.render().iter().all(|&v| v == 0.0));
        }
        assert!(receiver.render().iter().any(|&v| v != 0.0));
    }

    #[test]
    fn resampler_preserves_phase_across_blocks() {
        for ppm in [-1000.0, 0.0, 1000.0] {
            let mut resampler = DriftResampler::new(2);
            let mut pushed = 0;
            let mut rendered = 0;
            for _ in 0..100 {
                let input: Vec<f32> = (pushed..pushed + 500).flat_map(|i| [i as f32, -(i as f32)]).collect();
                resampler.push(&input);
                pushed += 500;
                let out = resampler.render(480, ppm);
                for frame in out.as_chunks::<2>().0.iter() {
                    let expected = rendered as f64 * DriftResampler::step(ppm);
                    assert!((f64::from(frame[0]) - expected).abs() < 0.01);
                    assert_eq!(frame[0], -frame[1]);
                    rendered += 1;
                }
            }
        }
    }

    #[test]
    fn drift_controller_tracks_both_clock_directions_for_ten_minutes() {
        for clock_ppm in [-700.0, 700.0] {
            let mut controller = DriftController::default();
            let mut occupancy = (TARGET_FRAMES * FRAME_SIZE) as f64;
            for _ in 0..60_000 {
                let ppm = controller.update(occupancy - (TARGET_FRAMES * FRAME_SIZE) as f64);
                occupancy += FRAME_SIZE as f64 * (clock_ppm - ppm) / 1_000_000.0;
                assert!(ppm.abs() <= 1000.0);
                assert!(occupancy > FRAME_SIZE as f64 && occupancy < (5 * FRAME_SIZE) as f64);
            }
            assert!((controller.ppm - clock_ppm).abs() < 5.0);
            assert!((occupancy - (TARGET_FRAMES * FRAME_SIZE) as f64).abs() < 5.0);
        }
    }

    #[test]
    fn packetized_clocks_remain_bounded_without_loss_for_two_minutes() {
        let packet = packets(1).remove(0);
        for clock_ppm in [-700.0, 700.0] {
            let mut receiver = Receiver::new_with_config(Config::opus_10ms()).unwrap();
            let mut seq = 0;
            for _ in 0..TARGET_FRAMES {
                assert!(receiver.push(seq, packet.clone()));
                seq += 1;
            }
            let mut source_phase = 0.0;
            for _ in 0..12_000 {
                let output = receiver.render();
                assert_eq!(output.len(), FRAME_SAMPLES);
                assert!(output.iter().all(|v| v.is_finite()));
                source_phase += 1.0 + clock_ppm / 1_000_000.0;
                while source_phase >= 1.0 {
                    assert!(receiver.push(seq, packet.clone()));
                    seq += 1;
                    source_phase -= 1.0;
                }
                assert!(receiver.stats().buffered_ms < 60.0);
            }
            assert_eq!(receiver.stats().lost, 0, "clock {clock_ppm}");
            assert_eq!(receiver.stats().rebufferings, 0);
        }
    }

    #[test]
    fn resampler_caps_queue_and_sanitizes_input() {
        let mut resampler = DriftResampler::new(2);
        resampler.push(&vec![f32::INFINITY; (MAX_RESAMPLE_FRAMES + 100) * 2]);
        assert_eq!(resampler.buffered_frames(), MAX_RESAMPLE_FRAMES);
        assert!(resampler.render(480, f64::NAN).iter().all(|&v| v == 0.0));
    }

    #[test]
    fn mixing_sums_cancels_limits_and_zero_pads() {
        let mut out = [99.0; 5];
        mix(&[&[0.1, 0.8, -0.8, f32::NAN], &[0.2, 0.8, -0.8, 0.25]], &mut out);
        assert!((out[0] - 0.3).abs() < 1e-6);
        assert_eq!(&out[1..], &[1.0, -1.0, 0.25, 0.0]);
        mix(&[&[0.5], &[-0.5]], &mut out);
        assert_eq!(out, [0.0; 5]);
        mix(&[], &mut out);
        assert_eq!(out, [0.0; 5]);
    }

    fn pcm_packet(config: Config, value: f32) -> Vec<u8> {
        Encoder::new_with_config(config)
            .unwrap()
            .encode(&vec![value; config.frame_samples()])
            .unwrap()
    }

    #[test]
    fn configured_codecs_fit_media_budget_and_validate_bounds() {
        let pcm = Config::low_latency_pcm();
        assert_eq!(pcm.frame_size(), 120);
        assert_eq!(pcm.payload_bytes(), 480);
        let mut pcm5 = pcm;
        pcm5.frame_duration = FrameDuration::Ms5;
        pcm5.jitter.max_frames = 12;
        assert_eq!(pcm5.payload_bytes(), 960);
        assert!(pcm5.validate().is_ok());
        pcm5.frame_duration = FrameDuration::Ms10;
        pcm5.jitter.max_frames = 6;
        pcm5.jitter.max_plc_frames = 6;
        assert!(pcm5.validate().is_err());
        let mut bad = pcm;
        bad.jitter.initial_frames = 101;
        assert!(Receiver::new_with_config(bad).is_err());
        bad = pcm;
        bad.jitter.max_frames = usize::MAX;
        assert!(Encoder::new_with_config(bad).is_err());
        bad = pcm;
        bad.jitter.min_frames = 1;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn configured_opus_is_cbr_and_reports_lookahead() {
        for duration in [FrameDuration::Ms2p5, FrameDuration::Ms5] {
            let mut config = Config::low_latency_pcm();
            config.codec = Codec::Opus;
            config.bitrate = 256_000;
            config.frame_duration = duration;
            config.jitter.max_frames = 12;
            let mut encoder = Encoder::new_with_config(config).unwrap();
            let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
            let packet = encoder.encode(&vec![0.1; config.frame_samples()]).unwrap();
            assert_eq!(packet.len(), config.payload_bytes());
            let mut decoded = vec![0.0; config.frame_samples()];
            assert_eq!(
                decoder.decode_float(&packet, &mut decoded, false).unwrap(),
                config.frame_size()
            );
            assert_eq!(encoder.lookahead_samples(), 120);
        }
    }

    #[test]
    fn opus5_quality_profile_has_explicit_controls_and_fullband_stereo_packets() {
        let config = Config::low_latency_opus();
        assert_eq!(config.bitrate, 320_000);
        assert_eq!(config.frame_size(), 240);
        assert_eq!(config.payload_bytes(), 200);
        let mut encoder = Encoder::new_with_config(config).unwrap();
        assert_eq!(encoder.lookahead_samples(), 120);
        let EncoderCodec::Opus(inner) = &mut encoder.inner else {
            panic!("expected Opus encoder")
        };
        assert_eq!(inner.get_sample_rate().unwrap(), 48_000);
        assert_eq!(inner.get_application().unwrap(), opus::Application::LowDelay);
        assert_eq!(inner.get_bitrate().unwrap(), opus::Bitrate::Bits(320_000));
        assert!(!inner.get_vbr().unwrap());
        assert_eq!(inner.get_force_channels().unwrap(), Some(opus::Channels::Stereo));
        assert_eq!(inner.get_complexity().unwrap(), 10);
        assert!(!inner.get_dtx().unwrap());
        assert!(!inner.get_inband_fec().unwrap());
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        // Silence, identical channels, then distinct channels. This validates
        // codec settings/framing, not perceptual quality or hardware playback.
        for mode in 0..3 {
            for frame in 0..20 {
                let input: Vec<f32> = (0..config.frame_size())
                    .flat_map(|sample| {
                        let time = (frame * config.frame_size() + sample) as f32 / SAMPLE_RATE as f32;
                        let left = if mode == 0 {
                            0.0
                        } else {
                            (time * 1000.0 * std::f32::consts::TAU).sin() * 0.2
                        };
                        let right = if mode == 2 {
                            (time * 1700.0 * std::f32::consts::TAU).sin() * 0.2
                        } else {
                            left
                        };
                        [left, right]
                    })
                    .collect();
                let packet = encoder.encode(&input).unwrap();
                assert_eq!(packet.len(), 200);
                assert_eq!(opus::packet::get_nb_channels(&packet).unwrap(), opus::Channels::Stereo);
                assert_eq!(opus::packet::get_bandwidth(&packet).unwrap(), opus::Bandwidth::Fullband);
                // RFC 6716 TOC configuration 29: CELT-only, fullband, 5 ms.
                assert_eq!(packet[0] >> 3, 29);
                assert_eq!(opus::packet::get_nb_frames(&packet).unwrap(), 1);
                assert_eq!(opus::packet::get_nb_samples(&packet, SAMPLE_RATE).unwrap(), 240);
                let mut output = vec![0.0; config.frame_samples()];
                assert_eq!(decoder.decode_float(&packet, &mut output, false).unwrap(), 240);
                assert!(output.iter().all(|sample| sample.is_finite()));
            }
        }
        let EncoderCodec::Opus(inner) = &mut encoder.inner else {
            unreachable!()
        };
        assert_eq!(inner.get_bandwidth().unwrap(), opus::Bandwidth::Fullband);
    }

    #[test]
    fn opus_ten_ms_profile_and_non_five_ms_bitrate_limit() {
        let mut encoder = Encoder::new_with_config(Config::opus_10ms()).unwrap();
        assert_eq!(encoder.config(), Config::opus_10ms());
        assert_eq!(encoder.config().frame_size(), 480);
        let EncoderCodec::Opus(inner) = &mut encoder.inner else {
            unreachable!()
        };
        assert_eq!(inner.get_bitrate().unwrap(), opus::Bitrate::Bits(128_000));
        assert_eq!(inner.get_force_channels().unwrap(), None);
        assert!(!inner.get_vbr().unwrap());
        let packet = encoder.encode(&vec![0.1; FRAME_SAMPLES]).unwrap();
        assert_eq!(packet.len(), 160);
        assert_eq!(opus::packet::get_nb_samples(&packet, SAMPLE_RATE).unwrap(), 480);
        let mut config = Config::opus_10ms();
        config.bitrate = 320_000;
        assert!(config.validate().is_err());
        config = Config::low_latency_opus();
        config.bitrate = 320_001;
        assert!(config.validate().is_err());
    }

    #[test]
    fn opus5_preserves_jitter_time_budgets_and_bounds_plc_without_nan() {
        for config in [Config::low_latency_opus(), Config::low_latency_pcm()] {
            let frame_ms = config.frame_duration_ms();
            assert_eq!(config.jitter.initial_frames as f64 * frame_ms, 5.0);
            assert_eq!(config.jitter.min_frames as f64 * frame_ms, 5.0);
            assert_eq!(config.jitter.max_frames as f64 * frame_ms, 250.0);
            assert_eq!(config.jitter.max_plc_frames as f64 * frame_ms, 20.0);
        }
        let mut config = Config::low_latency_opus();
        config.jitter.adaptive = false; // Isolate PLC's codec-frame bound.
        let mut encoder = Encoder::new_with_config(config).unwrap();
        let origin = Instant::now();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        for seq in [0, 1, 3] {
            let payload = encoder.encode(&vec![0.05; config.frame_samples()]).unwrap();
            assert!(receiver.push_at(seq, payload, origin));
        }
        for tick in 0..100 {
            let output = receiver.render_at(origin + Duration::from_millis(5 + tick * 5));
            assert!(output.iter().all(|sample| sample.is_finite()));
            if tick > 20 {
                assert!(output.iter().all(|&sample| sample == 0.0));
            }
        }
        let stats = receiver.stats();
        assert_eq!(stats.gap_plc_frames, 1);
        assert_eq!(stats.empty_queue_plc_frames, 4);
        assert_eq!(stats.plc_ms, 25.0); // One isolated gap + the bounded 20 ms tail.
        assert_eq!(stats.rebufferings, 1);
        assert_eq!(stats.decode_errors, 0);
    }

    #[test]
    fn pcm16_roundtrip_clips_and_preserves_stereo_without_codec_delay() {
        let mut config = Config::low_latency_pcm();
        config.jitter.adaptive = false;
        let mut encoder = Encoder::new_with_config(config).unwrap();
        assert_eq!(encoder.lookahead_samples(), 0);
        let input: Vec<_> = (0..config.frame_size())
            .flat_map(|i| if i == 0 { [2.0, -2.0] } else { [0.25, -0.75] })
            .collect();
        let encoded = encoder.encode(&input).unwrap();
        let origin = Instant::now();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        receiver.push_at(0, encoded, origin);
        let (decoded, trace) = receiver.render_with_trace(origin + Duration::from_millis(5));
        assert_eq!(trace.sequence, Some(0));
        assert_eq!(trace.kind, RenderKind::Audio);
        for (actual, expected) in decoded.iter().zip(&input) {
            assert!((actual - expected.clamp(-1.0, 1.0)).abs() <= 1.0 / 32767.0);
        }
        assert_eq!(receiver.stats().lost, 0);
    }

    #[test]
    fn stable_low_latency_stream_starts_at_five_ms_and_never_prefetches_a_packet() {
        let config = Config::low_latency_pcm();
        let payload = pcm_packet(config, 0.25);
        let origin = Instant::now();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        for seq in 0..400 {
            let now = origin + Duration::from_micros(seq * 2500);
            assert!(receiver.push_at(seq, payload.clone(), now));
            let (_, trace) = receiver.render_with_trace(now);
            if seq < 2 {
                assert_eq!(trace.kind, RenderKind::StartupSilence);
                assert_eq!(trace.sequence, None);
            } else {
                assert_eq!(trace.sequence, Some(seq - 2));
                assert_eq!(trace.last_sequence, Some(seq - 2));
                assert_eq!(trace.decoded_first_sequence, Some(seq - 2));
                assert_eq!(trace.decoded_last_sequence, Some(seq - 2));
                assert_eq!(trace.packet_wait_ns, Some(5_000_000));
                assert_eq!(receiver.stats().decoded_queue_ms, 0.0);
            }
        }
        let stats = receiver.stats();
        assert_eq!(stats.current_target_ms, 5.0);
        assert_eq!(stats.startup_wait_ms, 5.0);
        assert_eq!(stats.buffered_ms, 5.0);
        assert_eq!(stats.peak_buffered_ms, 7.5);
        assert_eq!(stats.lost + stats.late + stats.latency_trimmed_frames, 0);
    }

    #[test]
    fn steady_target_does_not_trim_a_callback_burst_with_fractional_residue() {
        let config = Config::low_latency_opus();
        let mut encoder = Encoder::new_with_config(config).unwrap();
        let payload = encoder.encode(&vec![0.1; config.frame_samples()]).unwrap();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        let origin = Instant::now();
        // A 10 ms native callback demands two 5 ms frames at once. Three
        // arriving frames plus one retained interpolation sample must not be
        // mistaken for a request to reduce the unchanged 5 ms jitter target.
        receiver.next = Some(0);
        receiver.resampler.push(&[0.1, 0.1]);
        for sequence in 0..3 {
            assert!(receiver.push_with_source_time_at(sequence, payload.clone(), origin, 0));
        }
        receiver.render_at(origin);
        assert_eq!(receiver.stats().target_decreases, 0);
        assert_eq!(receiver.stats().latency_trimmed_frames, 0);
        assert_eq!(receiver.stats().lost, 0);
    }

    #[test]
    fn callback_batches_preserve_reserve_across_clock_rates_and_start_phases() {
        // Source: 5 ms packets. Worker: 1.5 ms poll. Native playback: 5/10 ms
        // callbacks at -700/0/+700 ppm, with several relative startup phases.
        // Before demand-aware drift/prefill, the 10 ms / +700 ppm cases conceal
        // 450..822 real packets in 30 seconds despite no transport loss.
        for callback_frames in [240, 480] {
            let offsets: &[u32] = if callback_frames == 240 {
                &[1000, 3000, 5000]
            } else {
                &[1000, 3000, 5000, 7000, 9000, 14000]
            };
            for &offset in offsets {
                for device_ppm in [-700.0, 0.0, 700.0] {
                    let config = Config::low_latency_opus();
                    let origin = Instant::now();
                    let mut encoder = Encoder::new_with_config(config).unwrap();
                    let payload = encoder.encode(&vec![0.1; config.frame_samples()]).unwrap();
                    let mut receiver = Receiver::new_with_config(config).unwrap();
                    let mut next_callback = f64::from(offset);
                    let mut next_packet = 0;
                    let mut sequence = 0;
                    let mut output_queue: usize = 0;
                    for elapsed_us in (0..30_000_000_u64).step_by(1500) {
                        let now = origin + Duration::from_micros(elapsed_us);
                        if elapsed_us >= next_packet {
                            receiver.push_with_source_time_at(sequence, payload.clone(), now, elapsed_us * 1000);
                            sequence += 1;
                            next_packet += 5000;
                        }
                        if elapsed_us as f64 >= next_callback {
                            output_queue = output_queue.saturating_sub(callback_frames);
                            next_callback += callback_frames as f64 / 0.048 / (1.0 + device_ppm / 1_000_000.0);
                        }
                        while output_queue < callback_frames {
                            receiver.render_inner(now, callback_frames - output_queue, None);
                            output_queue += config.frame_size();
                        }
                    }
                    let stats = receiver.stats();
                    assert_eq!(
                        stats.lost + stats.late + stats.rejected + stats.latency_trimmed_frames,
                        0,
                        "callback={callback_frames} offset={offset} ppm={device_ppm}: {stats:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn accumulated_positive_drift_does_not_conceal_an_unarrived_packet() {
        let config = Config::low_latency_opus();
        let origin = Instant::now();
        let mut encoder = Encoder::new_with_config(config).unwrap();
        let payload = encoder.encode(&vec![0.1; config.frame_samples()]).unwrap();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        receiver.drift.ppm = 1000.0;
        receiver.drift.integral = 1000.0;
        // The preceding frames have accumulated a fractional clock correction.
        // A native batch now needs the currently available frame. Needing one
        // more sample for that correction must not conceal the entire next one.
        receiver.resampler.phase = 0.9;
        assert!(receiver.push_at(40, payload.clone(), origin));
        let (_, trace) = receiver.render_with_trace(origin + Duration::from_millis(5));
        assert_eq!(trace.decoded_last_sequence, Some(40));
        assert_eq!(receiver.stats().lost, 0);
        assert!(receiver.push_at(41, payload, origin + Duration::from_millis(6)));
        assert_eq!(receiver.stats().late, 0);
    }

    #[test]
    fn fractional_boundary_still_conceals_a_real_missing_codec_frame() {
        let config = Config::low_latency_opus();
        let origin = Instant::now();
        let mut encoder = Encoder::new_with_config(config).unwrap();
        let payload = encoder.encode(&vec![0.1; config.frame_samples()]).unwrap();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        assert!(receiver.push_at(40, payload.clone(), origin));
        receiver.render_at(origin + Duration::from_millis(5));
        // Frame 41 is genuinely missing; the one-sample allowance cannot
        // stretch frame 40 into a substitute codec frame.
        assert!(receiver.push_at(42, payload, origin + Duration::from_millis(10)));
        let (_, trace) = receiver.render_with_trace(origin + Duration::from_millis(10));
        assert_eq!(trace.decoded_first_sequence, Some(41));
        assert_eq!(receiver.stats().lost, 1);
        assert_eq!(receiver.stats().gap_plc_frames, 1);
    }

    #[test]
    fn positive_drift_does_not_conceal_the_next_packet_one_frame_early() {
        let mut config = Config::low_latency_pcm();
        config.jitter.adaptive = false;
        let origin = Instant::now();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        receiver.push_at(40, pcm_packet(config, 0.25), origin);
        receiver.drift.ppm = 1000.0;
        receiver.drift.integral = 1000.0;
        let (_, trace) = receiver.render_with_trace(origin + Duration::from_millis(5));
        assert_eq!(trace.sequence, Some(40));
        assert_eq!(trace.decoded_last_sequence, Some(40));
        assert_eq!(receiver.stats().lost, 0);
        assert_eq!(receiver.stats().boundary_hold_samples, 1);
        assert!(receiver.push_at(41, pcm_packet(config, 0.25), origin + Duration::from_millis(6)));
        assert_eq!(receiver.stats().late, 0);
    }

    fn simulate_arrival_trace(config: Config, duration_ms: u64) -> Stats {
        let frame_us = (config.frame_duration_ms() * 1000.0) as u64;
        let origin = Instant::now();
        let payload = pcm_packet(config, 0.05);
        let mut events: Vec<_> = (0..duration_ms * 1000 / frame_us + 1)
            .map(|seq| {
                let send_us = seq * frame_us;
                // During seconds 1..3, repeated 20ms network-delay excursions;
                // the rest is stable. This is a simulation, not hardware evidence.
                let jitter_us = if (1_000_000..3_000_000).contains(&send_us) && seq % 40 >= 20 {
                    20_000
                } else {
                    0
                };
                (send_us + 3000 + jitter_us, seq)
            })
            .collect();
        events.sort_unstable();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        let mut next_event = 0;
        for tick in 0..duration_ms * 1000 / frame_us {
            let now_us = tick * frame_us;
            while next_event < events.len() && events[next_event].0 <= now_us {
                let (arrival_us, seq) = events[next_event];
                receiver.push_at(seq, payload.clone(), origin + Duration::from_micros(arrival_us));
                next_event += 1;
            }
            receiver.render_at(origin + Duration::from_micros(now_us));
            let stats = receiver.stats();
            assert!(stats.current_target_ms <= 60.0);
            assert!(stats.buffered_ms <= 80.0, "{stats:?}");
        }
        receiver.stats()
    }

    #[test]
    fn arrival_adaptation_preserves_continuity_then_returns_to_real_low_latency() {
        let adaptive = simulate_arrival_trace(Config::low_latency_pcm(), 12_000);
        let mut fixed = Config::low_latency_pcm();
        fixed.jitter.adaptive = false;
        let fixed = simulate_arrival_trace(fixed, 12_000);
        assert!(adaptive.target_increases > 0);
        assert!(adaptive.target_decreases > 0);
        assert!(
            adaptive.adaptation_hold_frames + adaptive.startup_hold_frames > 2,
            "{adaptive:?}"
        );
        assert_eq!(adaptive.latency_trimmed_frames, 0);
        assert_eq!(adaptive.adaptation_hold_frames, 0);
        assert!(adaptive.adaptation_compressed_ms > 0.0);
        assert_eq!(adaptive.current_target_ms, 5.0);
        assert!(adaptive.buffered_ms <= 10.0);
        // Fixed buffering also benefits from removing false burst trims. It
        // may retain extra delay after rebuffering, but must not be artificially
        // damaged just to make the adaptive comparison strictly better.
        assert!(adaptive.lost <= fixed.lost, "adaptive={adaptive:?}, fixed={fixed:?}");
        assert!(adaptive.late <= fixed.late, "adaptive={adaptive:?}, fixed={fixed:?}");
        assert!(adaptive.buffered_ms < fixed.buffered_ms);
    }

    #[test]
    fn low_latency_packet_clocks_stay_bounded_for_two_minutes() {
        let mut config = Config::low_latency_pcm();
        config.jitter.adaptive = false;
        let payload = pcm_packet(config, 0.05);
        for source_ppm in [-700.0, 700.0] {
            let origin = Instant::now();
            let mut receiver = Receiver::new_with_config(config).unwrap();
            let mut seq = 0;
            let source_period_ns = 2_500_000.0 / (1.0 + source_ppm / 1_000_000.0);
            for tick in 0..48_000_u64 {
                let now_ns = tick * 2_500_000;
                while (seq as f64 * source_period_ns) <= now_ns as f64 {
                    assert!(receiver.push_at(
                        seq,
                        payload.clone(),
                        origin + Duration::from_nanos((seq as f64 * source_period_ns) as u64)
                    ));
                    seq += 1;
                }
                receiver.render_at(origin + Duration::from_nanos(now_ns));
                assert!(receiver.stats().buffered_ms < 12.5);
            }
            assert_eq!(receiver.stats().lost, 0, "{source_ppm}: {:?}", receiver.stats());
            assert_eq!(receiver.stats().rebufferings, 0);
            assert_eq!(receiver.stats().latency_trimmed_frames, 0);
        }
    }

    #[test]
    fn configured_plc_duration_is_bounded_and_distinguishes_empty_tail_from_gaps() {
        let mut config = Config::low_latency_pcm();
        config.jitter.adaptive = false;
        let payload = pcm_packet(config, 0.05);
        let origin = Instant::now();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        for seq in [0, 1, 3] {
            receiver.push_at(seq, payload.clone(), origin);
        }
        for tick in 0..100 {
            receiver.render_at(origin + Duration::from_micros(5000 + tick * 2500));
        }
        let stats = receiver.stats();
        assert_eq!(stats.gap_plc_frames, 1);
        assert_eq!(stats.empty_queue_plc_frames, 8);
        assert_eq!(stats.plc_ms, 22.5);
        assert_eq!(stats.rebufferings, 1);
    }

    #[test]
    fn active_target_growth_resamples_without_inserting_silence() {
        let config = Config::low_latency_pcm();
        let payload = pcm_packet(config, 0.05);
        let origin = Instant::now();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        let mut seq = 0_u64;
        for tick in 0..800_u64 {
            let now_us = tick * 2500;
            loop {
                let arrival_us = seq * 2500 + if seq >= 200 { 7500 } else { 0 };
                if arrival_us > now_us {
                    break;
                }
                receiver.push_at(seq, payload.clone(), origin + Duration::from_micros(arrival_us));
                seq += 1;
            }
            let (_, trace) = receiver.render_with_trace(origin + Duration::from_micros(now_us));
            assert_ne!(trace.kind, RenderKind::AdaptiveHold);
        }
        assert!(receiver.stats().adaptation_stretched_ms > 0.0);
        assert_eq!(receiver.stats().adaptation_hold_frames, 0);
        assert!(receiver.stats().current_target_ms <= 15.0);
    }

    #[test]
    fn far_future_sequences_cannot_overflow_or_inflate_adaptive_target() {
        let config = Config::low_latency_pcm();
        let payload = pcm_packet(config, 0.05);
        let origin = Instant::now();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        assert!(receiver.push_at(0, payload.clone(), origin));
        assert!(!receiver.push_at(u64::MAX, payload.clone(), origin + Duration::from_millis(1)));
        assert_eq!(receiver.stats().current_target_ms, 5.0);
        assert!(receiver.push_at(1, payload, origin + Duration::from_micros(2500)));
    }

    #[test]
    fn sender_pacing_does_not_inflate_network_jitter() {
        let config = Config::low_latency_pcm();
        let origin = Instant::now();
        let mut timed = AdaptiveJitter::new(config);
        let mut fallback = AdaptiveJitter::new(config);
        for seq in 0..800 {
            // A slow source with periodic extra capture/scheduling delay; the
            // network itself always takes 3 ms. Sender epoch is unrelated.
            let emitted = seq * 2_500_000 + seq / 40 * 5_000_000;
            let arrival = origin + Duration::from_nanos(emitted + 3_000_000);
            timed.observe(seq, arrival, Some(u64::MAX / 2 + emitted));
            fallback.observe(seq, arrival, None);
        }
        assert_eq!(timed.target, config.jitter.min_frames);
        assert!(timed.spread_ms < 0.001);
        assert!(fallback.target > timed.target);
    }

    #[test]
    fn fifty_ms_network_burst_returns_slowly_without_frame_trims() {
        let config = Config::low_latency_pcm();
        let origin = Instant::now();
        let payload = pcm_packet(config, 0.05);
        let mut receiver = Receiver::new_with_config(config).unwrap();
        let mut events: Vec<_> = (0..8000_u64)
            .map(|seq| {
                let source_ns = seq * 2_500_000;
                // A finite 100 ms interval of delayed network packets, followed by
                // healthy traffic. Reordering is part of this deterministic trace.
                let network_ns = if (400..440).contains(&seq) {
                    53_000_000
                } else {
                    3_000_000
                };
                (source_ns + network_ns, seq, source_ns)
            })
            .collect();
        events.sort_unstable();
        let mut next = 0;
        let mut peak = 0.0_f64;
        for tick in 0..8000_u64 {
            let now_ns = tick * 2_500_000;
            while next < events.len() && events[next].0 <= now_ns {
                let (arrival, seq, emitted) = events[next];
                receiver.push_with_source_time_at(
                    seq,
                    payload.clone(),
                    origin + Duration::from_nanos(arrival),
                    emitted,
                );
                next += 1;
            }
            receiver.render_at(origin + Duration::from_nanos(now_ns));
            let stats = receiver.stats();
            peak = peak.max(stats.current_target_ms);
            assert!(stats.current_target_ms <= 60.0);
            // A changed path can overlap 50 ms of delayed old packets with
            // newly on-time packets. Preserve that bounded transient instead
            // of deleting valid future frames to force the target immediately.
            assert!(stats.buffered_ms <= 115.0, "at {now_ns}: {stats:?}");
            if now_ns >= 16_000_000_000 {
                assert_eq!(stats.current_target_ms, 5.0, "{stats:?}");
                assert!(stats.buffered_ms <= 10.0, "{stats:?}");
            }
        }
        assert!(peak >= 50.0);
        assert_eq!(receiver.stats().latency_trimmed_frames, 0);
        assert!(receiver.stats().adaptation_compressed_ms > 0.0);
    }

    #[test]
    fn timing_mode_change_and_reordered_source_stamps_stay_bounded() {
        let config = Config::low_latency_pcm();
        let origin = Instant::now();
        let mut adaptive = AdaptiveJitter::new(config);
        adaptive.observe(0, origin, None);
        adaptive.observe(1, origin + Duration::from_millis(3), Some(u64::MAX - 1_000_000_000));
        // Earlier source stamp arriving later is ordinary packet reordering,
        // not unsigned timestamp underflow or a requirement to sync clocks.
        adaptive.observe(0, origin + Duration::from_millis(4), Some(u64::MAX - 1_002_500_000));
        assert!(adaptive.spread_ms.is_finite());
        assert!(adaptive.target <= config.jitter.max_frames);
        adaptive.observe(2, origin + Duration::from_millis(5), None);
        assert_eq!(adaptive.arrivals.len(), 1);
        assert_eq!(adaptive.spread_ms, 0.0);
    }

    #[test]
    fn isolated_fifty_ms_late_packet_does_not_raise_steady_target() {
        let config = Config::low_latency_pcm();
        let origin = Instant::now();
        let mut adaptive = AdaptiveJitter::new(config);
        let mut events: Vec<_> = (0..1000_u64)
            .map(|seq| {
                let source = seq * 2_500_000;
                (source + if seq == 400 { 53_000_000 } else { 3_000_000 }, seq, source)
            })
            .collect();
        events.sort_unstable();
        for (arrival, seq, source) in events {
            adaptive.observe(seq, origin + Duration::from_nanos(arrival), Some(source));
            assert_eq!(adaptive.target, config.jitter.min_frames);
        }
    }

    #[test]
    fn extreme_sender_timestamps_are_bounded_and_recover_after_healthy_traffic() {
        let config = Config::low_latency_pcm();
        let origin = Instant::now();
        for scenario in 0..3 {
            let mut adaptive = AdaptiveJitter::new(config);
            for seq in 0..16000_u64 {
                let regular = seq * 2_500_000;
                let source = match scenario {
                    0 if seq == 400 => u64::MAX,                // Isolated corrupt stamp.
                    1 if seq >= 400 => (seq - 400) * 2_500_000, // Sender clock restart.
                    2 if seq >= 400 => u64::MAX / 2 + regular,  // Giant forward epoch change.
                    _ => regular,
                };
                adaptive.observe(seq, origin + Duration::from_nanos(regular), Some(source));
                assert!(adaptive.target <= config.jitter.max_frames);
                assert!(adaptive.spread_ms.is_finite());
                assert!(adaptive.jitter_ms.is_finite());
                if seq >= 14000 {
                    assert_eq!(adaptive.target, config.jitter.min_frames, "scenario {scenario}");
                }
            }
        }
    }

    #[test]
    fn larger_ceiling_has_identical_playout_on_a_steady_link() {
        for config in [Config::low_latency_pcm(), Config::low_latency_opus()] {
            let mut limited = config;
            limited.jitter.max_frames = (60.0 / config.frame_duration_ms()) as usize;
            let mut old_ceiling = Receiver::new_with_config(limited).unwrap();
            let mut new_ceiling = Receiver::new_with_config(config).unwrap();
            let origin = Instant::now();
            let payload = Encoder::new_with_config(config)
                .unwrap()
                .encode(&vec![0.02; config.frame_samples()])
                .unwrap();
            let period_us = (config.frame_duration_ms() * 1000.0) as u64;
            let mut sequence = 0;
            let mut next_callback = 5000;
            let mut output_frames: usize = 0;
            for now_us in (0..20_000_000).step_by(1500) {
                let now = origin + Duration::from_micros(now_us);
                while sequence * period_us + 3000 + (sequence % 3) * 100 <= now_us {
                    let source = sequence * period_us;
                    let arrival = origin + Duration::from_micros(source + 3000 + (sequence % 3) * 100);
                    for receiver in [&mut old_ceiling, &mut new_ceiling] {
                        assert!(receiver.push_with_source_time_at(sequence, payload.clone(), arrival, source * 1000));
                    }
                    sequence += 1;
                }
                if now_us >= next_callback {
                    output_frames = output_frames.saturating_sub(480);
                    next_callback += 10_000;
                }
                while output_frames < 480 {
                    let a = old_ceiling.render_inner(now, 480 - output_frames, None);
                    let b = new_ceiling.render_inner(now, 480 - output_frames, None);
                    assert_eq!(a, b);
                    output_frames += config.frame_size();
                }
                assert_eq!(new_ceiling.stats().current_target_ms, 5.0);
                assert_eq!(old_ceiling.occupancy(), new_ceiling.occupancy());
            }
            let stats = new_ceiling.stats();
            assert_eq!(stats.lost + stats.late + stats.rejected, 0, "{stats:?}");
            assert_eq!(stats.adaptation_stretched_ms + stats.adaptation_compressed_ms, 0.0);
        }
    }

    #[test]
    fn recurring_two_hundred_ms_stalls_retain_reserve_and_drain_without_skips() {
        for config in [Config::low_latency_pcm(), Config::low_latency_opus()] {
            for callback_frames in [128_u64, 480] {
                let origin = Instant::now();
                let payload = Encoder::new_with_config(config)
                    .unwrap()
                    .encode(&vec![0.02; config.frame_samples()])
                    .unwrap();
                let mut receiver = Receiver::new_with_config(config).unwrap();
                let period_us = (config.frame_duration_ms() * 1000.0) as u64;
                let mut events: Vec<_> = (0..45_000_000 / period_us)
                    .map(|sequence| {
                        let source_us = sequence * period_us;
                        // Simulated 200 ms radio outages in seconds 1..8;
                        // every packet returns together, none is discarded.
                        let second = source_us / 1_000_000;
                        let arrival_us = if (1..=8).contains(&second) && source_us % 1_000_000 < 200_000 {
                            second * 1_000_000 + 200_000
                        } else {
                            source_us
                        } + 3000;
                        (arrival_us, sequence, source_us)
                    })
                    .collect();
                events.sort_unstable();
                let mut index = 0;
                let mut learned_loss = 0;
                let mut peak_target = 0.0_f64;
                let mut callback_tick = 0;
                let mut output_frames = 0_usize;
                for now_us in (0..45_000_000).step_by(1000) {
                    let now = origin + Duration::from_micros(now_us);
                    while index < events.len() && events[index].0 <= now_us {
                        let (arrival, sequence, source) = events[index];
                        receiver.push_with_source_time_at(
                            sequence,
                            payload.clone(),
                            origin + Duration::from_micros(arrival),
                            source * 1000,
                        );
                        index += 1;
                    }
                    while callback_tick * callback_frames * 1_000_000 / u64::from(SAMPLE_RATE) <= now_us {
                        output_frames = output_frames.saturating_sub(callback_frames as usize);
                        callback_tick += 1;
                    }
                    while output_frames < callback_frames as usize {
                        receiver.render_inner(now, callback_frames as usize - output_frames, None);
                        output_frames += config.frame_size();
                    }
                    let stats = receiver.stats();
                    peak_target = peak_target.max(stats.current_target_ms);
                    assert!(stats.buffered_ms <= 270.0, "{stats:?}");
                    assert!(stats.current_target_ms <= 250.0);
                    assert_eq!(stats.latency_trimmed_frames + stats.adaptation_hold_frames, 0);
                    if now_us == 3_000_000 {
                        learned_loss = stats.lost;
                    }
                    if (3_000_000..9_000_000).contains(&now_us) {
                        assert_eq!(
                            stats.lost, learned_loss,
                            "after learning at {now_us}, callback {callback_frames}: {stats:?}"
                        );
                    }
                }
                assert!(peak_target >= 200.0, "peak={peak_target}");
                assert!(learned_loss > 0, "the unanticipated first blackout cannot be hidden");
                let stats = receiver.stats();
                assert_eq!(stats.current_target_ms, 5.0);
                assert!(stats.buffered_ms <= 16.0, "{stats:?}");
                assert!(stats.adaptation_compressed_ms > 150.0, "{stats:?}");
            }
        }
    }
}

#[cfg(test)]
mod wake_backlog_tests {
    use super::*;
    #[test]
    fn continued_burst_after_fallback_wake_rebuilds_reserve_on_a_steady_batched_source() {
        for continued in [true, false] {
            for callback_phase_us in [0, 1000, 2500] {
                let config = Config::low_latency_opus();
                let mut encoder = Encoder::new_with_config(config).unwrap();
                let mut payload = [0; MAX_PACKET_BYTES];
                // Retain old encoder state, as in a fixed_profile same-burst wake. A new
                // receiver must recover its timing even without a paired source reset.
                for _ in 0..32 {
                    encoder.encode_into(&[0.1; 480], &mut payload).unwrap();
                }
                if !continued {
                    encoder.reset().unwrap();
                }
                let burst = if continued { 1 } else { 2 };
                let first_sequence = if continued { 33 } else { 1 };
                let opened_us = 33_000 + callback_phase_us;
                let origin = Instant::now();
                let admission = crate::idle::Bursts::new(origin - Duration::from_secs(31));
                assert!(admission.media(1, 32, origin - Duration::from_secs(31)));
                assert!(admission.asleep(origin));
                let mut receiver = Receiver::new_with_config(config).unwrap();
                receiver.reset_burst();
                let mut output = [0.0; FRAME_SAMPLES];
                let mut next_batch = 0_u64;
                let mut callback = 0_u64;
                let mut queued_output = 0_usize;
                let mut minimum_ppm = 0.0_f64;
                let mut losses_after_learning = 0;
                for now_us in (0..15_000_000).step_by(100) {
                    // WASAPI emits two 5 ms packets per 10 ms capture buffer. Source
                    // timestamps preserve that cadence, not invented 5 ms spacing.
                    let phase = if next_batch >= 400 { 3000 } else { 0 };
                    while next_batch * 10_000 + phase <= now_us {
                        let arrival_us = next_batch * 10_000 + phase;
                        let at = origin + Duration::from_micros(arrival_us);
                        for packet in 0..2 {
                            let sequence = first_sequence + next_batch * 2 + packet;
                            let len = encoder.encode_into(&[0.1; 480], &mut payload).unwrap();
                            assert!(admission.media(burst, sequence, at));
                            receiver.push_slice_at(
                                sequence,
                                &payload[..len],
                                at,
                                Some(arrival_us * 1000 + packet * 100),
                            );
                        }
                        next_batch += 1;
                    }
                    // Thirty-three milliseconds of native open backlog, then a 128
                    // sample device callback. No native underrun, packet loss or reset.
                    if now_us < opened_us {
                        continue;
                    }
                    while opened_us + callback * 128 * 1_000_000 / 48_000 <= now_us {
                        queued_output = queued_output.saturating_sub(128);
                        callback += 1;
                    }
                    let at = origin + Duration::from_micros(now_us);
                    while queued_output < 128 {
                        receiver.render_into(at, 128 - queued_output, &mut output[..480], None);
                        queued_output += 240;
                    }
                    minimum_ppm = minimum_ppm.min(receiver.stats().adaptation_ppm);
                    if now_us == 8_000_000 {
                        losses_after_learning = receiver.stats().lost;
                    }
                }
                let stats = receiver.stats();
                eprintln!(
                    "wake continued={continued} phase={callback_phase_us}: {stats:?}; min ppm={minimum_ppm}; learned loss={losses_after_learning}"
                );
                assert!(
                    stats.adaptation_compressed_ms > 10.0,
                    "must exercise cold-open catch-up"
                );
                assert!(
                    stats.adaptation_stretched_ms > 0.0,
                    "negative wake adjustment must reach the resampler: {stats:?}"
                );
                assert!(
                    stats.lost < 20,
                    "must not conceal a healthy source for seconds: {stats:?}"
                );
                assert_eq!(stats.lost, losses_after_learning, "steady playout must recover");
                assert_eq!(stats.latency_trimmed_frames, 0);
            }
        }
    }

    #[test]
    fn interleaved_valid_packets_do_not_allow_unbounded_empty_queue_concealment() {
        let config = Config::low_latency_opus();
        let origin = Instant::now();
        let mut encoder = Encoder::new_with_config(config).unwrap();
        let payload = encoder.encode(&[0.1; 480]).unwrap();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        receiver.reset_burst();
        let mut output = [0.0; 480];
        for sequence in 1..=2 {
            receiver.push_slice_at(sequence, &payload, origin, Some(0));
        }
        // Every valid frame is followed by an empty queue. Consecutive-only
        // PLC counting sees one loss forever, although reserve never recovers.
        for tick in 0..200 {
            let at = origin + Duration::from_millis(tick * 5);
            if tick >= 3 && tick % 2 == 1 {
                receiver.push_slice_at(tick + 1, &payload, at, Some(tick * 5_000_000));
            }
            receiver.render_into(at, 240, &mut output, None);
        }
        assert!(
            receiver.stats().rebufferings > 0,
            "persistent empty-queue PLC must force re-prefill: {:?}",
            receiver.stats()
        );
    }
    #[test]
    fn starvation_refill_requires_actual_reserve_even_when_packet_timestamps_are_old() {
        for mut config in [
            Config::low_latency_opus(),
            Config::low_latency_pcm(),
            Config::opus_10ms(),
        ] {
            config.jitter.adaptive = false; // Isolate prefill from timestamp-based target learning.
            let origin = Instant::now();
            let mut encoder = Encoder::new_with_config(config).unwrap();
            let payload = encoder.encode(&[0.1; FRAME_SAMPLES][..config.frame_samples()]).unwrap();
            let mut receiver = Receiver::new_with_config(config).unwrap();
            let mut output = [0.0; FRAME_SAMPLES];
            for sequence in 0..4 {
                receiver.push_slice_at(sequence, &payload, origin, None);
            }
            for tick in 0..40 {
                receiver.render_into(
                    origin + Duration::from_millis(tick * 10),
                    config.frame_size(),
                    &mut output[..config.frame_samples()],
                    None,
                );
            }
            assert!(receiver.stats().rebufferings > 0);
            let required_packets = receiver.adaptive.target + usize::from(!config.is_opus_10ms());
            // These arrivals are stale by several seconds. Elapsed time alone
            // cannot waive re-prefill, unlike the bounded first-onset wait.
            for packet in 1..=required_packets {
                receiver.push_slice_at(100 + packet as u64, &payload, origin, None);
                receiver.render_into(
                    origin + Duration::from_secs(10),
                    config.frame_size(),
                    &mut output[..config.frame_samples()],
                    None,
                );
                if packet < required_packets {
                    assert!(output.iter().all(|sample| *sample == 0.0));
                    assert!(receiver.next.is_none());
                } else {
                    assert!(output.iter().any(|sample| sample.abs() > 0.01));
                    assert!(receiver.next.is_some());
                }
            }
        }
    }
    #[test]
    fn idle_drain_releases_a_short_tail_held_by_starvation_refill() {
        let mut config = Config::low_latency_pcm();
        config.jitter.adaptive = false;
        let origin = Instant::now();
        let mut encoder = Encoder::new_with_config(config).unwrap();
        let payload = encoder.encode(&[0.2; 240]).unwrap();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        let mut output = [0.0; 240];
        receiver.push_slice_at(1, &payload, origin, None);
        for tick in 0..40 {
            receiver.render_into(origin + Duration::from_millis(tick * 5), 120, &mut output, None);
        }
        assert!(receiver.recovering);
        receiver.push_slice_at(100, &payload, origin, None);
        receiver.render_into(origin + Duration::from_secs(5), 120, &mut output, None);
        assert!(receiver.recovering && output.iter().all(|sample| *sample == 0.0));
        let losses = receiver.stats().lost;
        receiver.set_draining(true);
        receiver.render_into(origin + Duration::from_secs(5), 120, &mut output, None);
        assert!(output.iter().all(|sample| *sample > 0.19));
        assert_eq!(receiver.stats().lost, losses);
    }
    #[test]
    fn wake_backlog_drains_at_up_to_one_percent_without_trimming_media() {
        for config in [
            Config::low_latency_pcm(),
            Config::low_latency_opus(),
            Config::opus_10ms(),
        ] {
            let mut encoder = Encoder::new_with_config(config).unwrap();
            let mut receiver = Receiver::new_with_config(config).unwrap();
            receiver.reset_burst();
            let origin = Instant::now();
            let mut payload = [0; MAX_PACKET_BYTES];
            let len = encoder
                .encode_into(&[0.1; FRAME_SAMPLES][..config.frame_samples()], &mut payload)
                .unwrap();
            let step_ns = config.frame_size() as u64 * 1_000_000_000 / 48_000;
            let backlog_frames = (80.0 / config.frame_duration_ms()) as u64;
            for seq in 1..=backlog_frames {
                assert!(receiver.push_slice_at(
                    seq,
                    &payload[..len],
                    origin + Duration::from_nanos(seq * step_ns),
                    Some(seq * step_ns)
                ));
            }
            let mut maximum_ppm = 0.0_f64;
            let initial = receiver.stats().buffered_ms;
            let mut pcm = [0.0; FRAME_SAMPLES];
            for tick in 1..=(20_000.0 / config.frame_duration_ms()) as u64 {
                let sequence = backlog_frames + tick;
                let at = origin + Duration::from_nanos(sequence * step_ns);
                assert!(receiver.push_slice_at(sequence, &payload[..len], at, Some(sequence * step_ns)));
                receiver.render_into(at, config.frame_size(), &mut pcm[..config.frame_samples()], None);
                let stats = receiver.stats();
                maximum_ppm = maximum_ppm.max(stats.adaptation_ppm);
                assert!(stats.adaptation_ppm <= 10_000.0);
                assert_eq!(stats.latency_trimmed_frames, 0);
                assert_eq!(stats.lost, 0);
            }
            let stats = receiver.stats();
            assert!(maximum_ppm > 9_000.0);
            assert!(stats.buffered_ms < initial - 30.0, "backlog did not drain: {stats:?}");
            assert!(
                stats.buffered_ms <= stats.current_target_ms + config.frame_duration_ms() + 2.0,
                "target not reached: {stats:?}"
            );
            assert!(stats.arrival_jitter_ms < 1e-6);
            assert_eq!(stats.current_target_ms, stats.initial_target_ms);
        }
    }
}
