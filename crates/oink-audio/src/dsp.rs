//! Bounded audio transport DSP with explicit negotiated Opus and PCM profiles.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::sink::quality::OutputKind;
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
// Reserve adaptation only; oscillator correction remains independently bounded.
const MAX_ADAPTATION_PPM: f64 = 2_000.0;
const MAX_COMPRESSION_PPM: u32 = 5_000;
// -60 dBFS peak on both channels for 20 ms. This admits quiet portions of
// background noise while rejecting -50 dBFS sustained material; not an RMS gate.
const QUIET_PEAK: f32 = 0.001;
const TARGET_DECAY_INTERVAL: Duration = Duration::from_millis(500);
const MAX_JITTER_MS: f64 = 250.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    Opus,
    Pcm16,
    Pcm24,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameDuration {
    Ms20,
    Samples160,
    Ms10,
    Ms5,
    Ms2p5,
}
impl FrameDuration {
    pub const fn samples(self) -> usize {
        match self {
            Self::Ms20 => 960,
            Self::Samples160 => 160,
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
    /// Continuous lower demand required before decay; independent of codec.
    pub target_stable_secs: u32,
    /// Reserve recovery only, excluding oscillator correction.
    pub max_compression_ppm: u32,
    pub max_plc_frames: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub sample_rate: u32,
    pub codec: Codec,
    pub frame_duration: FrameDuration,
    /// Opus CBR rate; PCM must specify its actual 1,536,000 bit/s stream rate.
    pub bitrate: u32,
    pub jitter: JitterConfig,
}

impl Config {
    pub const fn opus_10ms() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            codec: Codec::Opus,
            frame_duration: FrameDuration::Ms10,
            bitrate: 128_000,
            jitter: JitterConfig {
                initial_frames: 3,
                min_frames: 3,
                max_frames: 3,
                adaptive: false,
                target_stable_secs: 3,
                max_compression_ppm: MAX_COMPRESSION_PPM,
                max_plc_frames: MAX_PLC_FRAMES,
            },
        }
    }
    pub const fn low_latency_pcm() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            codec: Codec::Pcm16,
            frame_duration: FrameDuration::Ms2p5,
            bitrate: 1_536_000,
            jitter: JitterConfig {
                initial_frames: 2,
                min_frames: 2,
                max_frames: 100,
                adaptive: true,
                target_stable_secs: 3,
                max_compression_ppm: MAX_COMPRESSION_PPM,
                max_plc_frames: 8,
            },
        }
    }
    pub const fn low_latency_opus() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            codec: Codec::Opus,
            frame_duration: FrameDuration::Ms5,
            bitrate: 320_000,
            jitter: JitterConfig {
                initial_frames: 1,
                min_frames: 1,
                max_frames: 50,
                adaptive: true,
                target_stable_secs: 3,
                max_compression_ppm: MAX_COMPRESSION_PPM,
                max_plc_frames: 4,
            },
        }
    }
    pub const fn music_opus() -> Self {
        Self {
            frame_duration: FrameDuration::Ms20,
            jitter: JitterConfig {
                initial_frames: 1,
                min_frames: 1,
                max_frames: 12,
                adaptive: true,
                target_stable_secs: 30,
                max_compression_ppm: 2_000,
                max_plc_frames: 1,
            },
            ..Self::low_latency_opus()
        }
    }
    pub const fn lossless(sample_rate: u32) -> Self {
        let minimum = (sample_rate as usize * 5).div_ceil(160_000);
        Self {
            sample_rate,
            codec: Codec::Pcm24,
            frame_duration: FrameDuration::Samples160,
            bitrate: sample_rate * 48,
            jitter: JitterConfig {
                initial_frames: minimum,
                min_frames: minimum,
                max_frames: sample_rate as usize / 640,
                adaptive: true,
                target_stable_secs: 30,
                max_compression_ppm: 2_000,
                max_plc_frames: sample_rate as usize / 8_000,
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
        self.frame_size() as f64 * 1000.0 / f64::from(self.sample_rate)
    }
    pub fn payload_bytes(self) -> usize {
        match self.codec {
            Codec::Pcm16 => self.frame_samples() * 2,
            Codec::Pcm24 => self.frame_samples() * 3,
            Codec::Opus => (u64::from(self.bitrate) * self.frame_size() as u64 / (u64::from(SAMPLE_RATE) * 8)) as usize,
        }
    }
    pub fn validate(self) -> Result<(), String> {
        if self.jitter.max_compression_ppm > MAX_COMPRESSION_PPM {
            return Err("reserve compression must not exceed 5000 ppm".into());
        }
        if !(8_000..=192_000).contains(&self.sample_rate)
            || (self.codec != Codec::Pcm24 && self.sample_rate != SAMPLE_RATE)
            || (self.codec == Codec::Pcm24 && self.frame_duration != FrameDuration::Samples160)
            || self.jitter.min_frames == 0
            || self.jitter.min_frames as f64 * self.frame_duration_ms() < 5.0
            || self.jitter.initial_frames < self.jitter.min_frames
            || self.jitter.initial_frames > self.jitter.max_frames
            || self.jitter.max_frames > 400
            || self.jitter.max_frames as f64 * self.frame_duration_ms() > MAX_JITTER_MS
            || self.jitter.max_plc_frames == 0
            || self.jitter.max_plc_frames > 24
            || self.jitter.max_plc_frames as f64 * self.frame_duration_ms() > 60.0
        {
            return Err("jitter bounds must be nonzero, ordered, and at most 250 ms; PLC at most 60 ms".into());
        }
        let opus_max_bitrate = if matches!(self.frame_duration, FrameDuration::Ms5 | FrameDuration::Ms20) {
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
            Codec::Pcm24 if self.bitrate != self.sample_rate * 48 => {
                return Err("PCM24 bitrate must equal sample rate times 48".into());
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
            Codec::Pcm16 | Codec::Pcm24 => payload.len() == config.payload_bytes(),
            Codec::Opus => opus::packet::get_nb_samples(payload, SAMPLE_RATE).ok() == Some(config.frame_size()),
        }
}

enum EncoderCodec {
    Opus(opus::Encoder),
    Pcm16,
    Pcm24,
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
            Codec::Pcm24 => (EncoderCodec::Pcm24, 0),
            Codec::Opus => {
                let application = if config.frame_duration == FrameDuration::Ms20 {
                    opus::Application::Audio
                } else {
                    opus::Application::LowDelay
                };
                let mut encoder =
                    opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, application).map_err(|e| e.to_string())?;
                encoder
                    .set_bitrate(opus::Bitrate::Bits(config.bitrate as i32))
                    .map_err(|e| e.to_string())?;
                encoder.set_vbr(false).map_err(|e| e.to_string())?;
                if matches!(config.frame_duration, FrameDuration::Ms5 | FrameDuration::Ms20) {
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
                if config.frame_duration == FrameDuration::Ms20 {
                    encoder.set_signal(opus::Signal::Music).map_err(|e| e.to_string())?;
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
            EncoderCodec::Pcm24 => {
                for (sample, bytes) in samples.iter().zip(packet.as_chunks_mut::<3>().0) {
                    let value = (sample.clamp(-1.0, 1.0) * 8_388_608.0)
                        .round()
                        .clamp(-8_388_608.0, 8_388_607.0) as i32;
                    bytes.copy_from_slice(&value.to_le_bytes()[..3]);
                }
                Ok(samples.len() * 3)
            }
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
    /// Pull requests which exhausted their fixed codec-call budget.
    #[serde(default)]
    pub work_limited: u64,
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
    /// Near-silent decoded media removed without changing its playback pitch.
    pub quiet_discarded_ms: f64,
    /// No quiet insertion is currently performed; reserve builds by rate or re-prefill.
    pub quiet_inserted_ms: f64,
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
    /// Concealment subset of decode_ns; zero when render tracing is disabled.
    #[serde(default)]
    pub plc_ns: u64,
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
// Sorted, preallocated deque. The normal in-order insert/pop moves no queued
// payloads; reordering shifts at most the fixed packet capacity.
struct PacketQueue(VecDeque<(u64, QueuedPacket)>);
impl PacketQueue {
    fn new(capacity: usize) -> Self {
        let storage = prepared_deque(capacity, || {
            (
                0,
                QueuedPacket {
                    payload: [0; MAX_PACKET_BYTES],
                    len: 0,
                    arrival: Instant::now(),
                },
            )
        });
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
        self.0.front().map(|(seq, packet)| (seq, packet))
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
            .and_then(|at| self.0.remove(at).map(|(_, packet)| packet))
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
    kind: OutputKind,
}

enum DecoderCodec {
    Opus(opus::Decoder),
    Pcm16,
    Pcm24,
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
    dropout_counters: Option<std::sync::Arc<std::sync::OnceLock<std::sync::Arc<crate::sink::DropoutCounters>>>>,
    dropout_ledger: crate::sink::DropoutLedger,
    late_ledger: crate::sink::DropoutLedger,
    output_kinds: [OutputKind; FRAME_SIZE * 2],
    output_frames: usize,
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
    kind_history: Vec<OutputKind>,
    kind_cursor: usize,
    drift: DriftController,
    adaptive: AdaptiveJitter,
    adjustment: PlayoutAdjustment,
    wake_backlog: bool,
    quiet_cooldown_frames: usize,
    draining: bool,
    retiring: bool,
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
        let _ = crate::diagnostics::monotonic_us(Instant::now());
        let decoder = match config.codec {
            Codec::Opus => {
                DecoderCodec::Opus(opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).map_err(|e| e.to_string())?)
            }
            Codec::Pcm16 => DecoderCodec::Pcm16,
            Codec::Pcm24 => DecoderCodec::Pcm24,
        };
        let mut resampler = DriftResampler::new(CHANNELS);
        resampler.set_rates(config.sample_rate, SAMPLE_RATE);
        let kind_history = vec![OutputKind::Intentional; resampler.kernel.delay()];
        Ok(Self {
            dropout_counters: None,
            dropout_ledger: Default::default(),
            late_ledger: Default::default(),
            output_kinds: [OutputKind::Intentional; FRAME_SIZE * 2],
            output_frames: config.frame_size(),
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
            resampler,
            kind_history,
            kind_cursor: 0,
            spans: prepared_deque(MAX_RESAMPLE_FRAMES / config.frame_size() + 2, || DecodedSpan {
                sequence: 0,
                frames: 0,
                arrival: None,
                concealed: false,
                kind: OutputKind::Audio,
            }),
            drift: DriftController::default(),
            adaptive: AdaptiveJitter::new(config),
            adjustment: PlayoutAdjustment::default(),
            wake_backlog: false,
            quiet_cooldown_frames: 0,
            draining: false,
            retiring: false,
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
    /// Set on the ordinary owner before publishing this Receiver. The store is
    /// pinned throughout retirement; RT only borrows its published counter.
    pub(crate) fn set_dropout_store(
        &mut self,
        store: std::sync::Arc<std::sync::OnceLock<std::sync::Arc<crate::sink::DropoutCounters>>>,
    ) {
        self.dropout_counters = Some(store);
    }
    #[cfg(test)]
    pub(crate) fn set_dropout_counters(&mut self, counters: Option<&std::sync::Arc<crate::sink::DropoutCounters>>) {
        self.dropout_counters = counters.map(|counter| std::sync::Arc::new(std::sync::OnceLock::from(counter.clone())));
    }
    fn dropout(&mut self, first: u64, last: u64, at: Instant, late: bool) {
        if let Some(counters) = self.dropout_counters.as_ref().and_then(|store| store.get()) {
            let count = self.dropout_ledger.insert(first, last);
            counters.record(
                (crate::diagnostics::monotonic_us(at).max(0) / 1000) as u64,
                if late { 0 } else { count },
                if late { count } else { 0 },
            );
        }
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
                self.adjustment.change_target(
                    (self.adaptive.target as f64 - previous as f64)
                        * self.config.frame_size() as f64
                        * f64::from(SAMPLE_RATE)
                        / f64::from(self.config.sample_rate),
                );
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
            if let Some(counters) = self.dropout_counters.as_ref().and_then(|store| store.get()) {
                counters.record_quality(crate::sink::DropoutBucket {
                    at_ms: (crate::diagnostics::monotonic_us(arrival).max(0) / 1000) as u64,
                    late_packets: self.late_ledger.insert(seq, seq),
                    ..Default::default()
                });
            }
            self.dropout(seq, seq, arrival, true);
            return false;
        }
        if self.recovering
            && self.next.is_none()
            && self
                .packets
                .first_key_value()
                .is_some_and(|(&first, _)| seq > first && seq - first >= maximum as u64)
        {
            // Refill has no playout cursor to advance its sequence window. If
            // another outage strands less than the required reserve, retaining
            // that old window rejects all future media forever. Start the refill
            // at the new forward window; keep the decoded watermark (no replays)
            // and jitter history (the outage still requires its learned reserve).
            self.packets.0.clear();
            self.first_packet = None;
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
        self.stats.peak_buffered_ms = self
            .stats
            .peak_buffered_ms
            .max(self.occupancy() as f64 / (f64::from(self.config.sample_rate) / 1000.0));
    }
    pub fn stats(&self) -> Stats {
        let ms = self.config.frame_duration_ms();
        Stats {
            queued_frames: self.packets.len(),
            buffered_ms: self.occupancy() as f64 / (f64::from(self.config.sample_rate) / 1000.0),
            drift_ppm: self.drift.ppm,
            frame_duration_ms: ms,
            initial_target_ms: self.config.jitter.initial_frames as f64 * ms,
            current_target_ms: self.adaptive.target as f64 * ms,
            min_target_ms: self.config.jitter.min_frames as f64 * ms,
            max_target_ms: self.config.jitter.max_frames as f64 * ms,
            packet_queue_ms: self.packets.len() as f64 * ms,
            decoded_queue_ms: self.resampler.buffered_frames() as f64 / (f64::from(self.config.sample_rate) / 1000.0),
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
    /// Fixed-codec-frame adapter for callers/tests. Production pull uses the
    /// same core with the callback's shared, finite codec-call budget.
    pub fn render_into(
        &mut self,
        now: Instant,
        demand_frames: usize,
        output: &mut [f32],
        trace: Option<&mut RenderTrace>,
    ) {
        if output.len() != self.config.frame_samples() {
            output.fill(0.0);
            return;
        }
        let mut budget = 6;
        self.pull_into(now, demand_frames, output, &mut budget, trace);
    }

    /// Consume exactly the requested stereo frames, retaining only decoded
    /// codec remainder and drift phase. The caller tiles requests above 480
    /// frames and shares `codec_budget` across every tile of one callback.
    /// Each actual decode/PLC call consumes one unit; invalid decode plus PLC
    /// consumes two. Exhaustion yields finite missing samples, never a retry.
    /// Configure on the ordinary owner before publishing this receiver.
    pub fn set_output_rate(&mut self, rate: u32) {
        self.resampler.set_rates(self.config.sample_rate, rate);
        self.kind_history = vec![OutputKind::Intentional; self.resampler.kernel.delay()];
        self.kind_cursor = 0;
    }
    pub fn output_rate(&self) -> u32 {
        (f64::from(self.config.sample_rate) / self.resampler.rate_ratio).round() as u32
    }

    pub fn pull_into(
        &mut self,
        now: Instant,
        demand_frames: usize,
        output: &mut [f32],
        codec_budget: &mut usize,
        mut trace: Option<&mut RenderTrace>,
    ) {
        output.fill(0.0);
        self.output_kinds.fill(if self.recovering && !self.draining {
            OutputKind::Missing
        } else {
            OutputKind::Intentional
        });
        self.output_frames = (output.len() / CHANNELS).min(FRAME_SIZE * 2);
        if output.is_empty() || !output.len().is_multiple_of(CHANNELS) || output.len() > FRAME_SAMPLES * 2 {
            return;
        }
        let frame_size = self.config.frame_size();
        let requested = output.len() / CHANNELS;
        let elapsed_ms = requested as f64 * 1000.0 / f64::from(self.output_rate());
        let source_requested = (requested as f64 * self.resampler.rate_ratio).ceil() as usize;
        let source_demand = (demand_frames as f64 * self.resampler.rate_ratio).ceil() as usize;
        let fixed_profile = self.config.is_opus_10ms();
        // The jitter reserve is additional to all samples already demanded by
        // this native callback, not just the one codec frame returned here.
        let target = if fixed_profile {
            self.adaptive.target * frame_size
        } else {
            (self.adaptive.target * frame_size)
                .saturating_add(source_demand.max(source_requested))
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
                // A freshly delivered capture batch is media for its whole
                // interval, not pre-existing jitter reserve. Starting it at
                // once can ask for the next packet before the next batch is
                // available. Honor the existing jitter wait in that case;
                // aged device-open backlog already satisfies it.
                (self.adaptive.source_batch_ms > 0.0
                    // Several batches already contain reserve. One codec frame
                    // of rounding covers emission spread within a capture batch.
                    && self.occupancy() <= (self.adaptive.source_batch_ms * (f64::from(self.config.sample_rate) / 1000.0)).ceil() as usize + frame_size * 2
                    && waited
                        < Duration::from_secs_f64((self.adaptive.target * frame_size) as f64 / f64::from(self.config.sample_rate)))
                    || (self.occupancy() < target
                        && waited
                            < Duration::from_secs_f64(
                                target.saturating_sub(frame_size).max(self.adaptive.target * frame_size) as f64
                                    / f64::from(self.config.sample_rate),
                            ))
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
            let first = self.packets.first_key_value().map(|(&seq, _)| seq);
            if let (Some(last), Some(first)) = (self.last_decoded, first)
                && first > last.saturating_add(1)
            {
                self.dropout(last + 1, first - 1, now, false);
            }
            self.next = first;
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
        // Sender timestamps exclude capture batching from network jitter, but
        // the rate controller must still distinguish its normal queue swing
        // from excess reserve. Otherwise it accelerates toward an empty trough.
        // Controllers retain their calibrated 48k-frame units at every native rate.
        let error = (self.occupancy() as f64
            - target as f64
            - self.adaptive.source_queue_phase_ms(now) * (f64::from(self.config.sample_rate) / 1000.0))
            * f64::from(SAMPLE_RATE)
            / f64::from(self.config.sample_rate);
        let adapting = (!fixed_profile || self.wake_backlog) && self.adjustment.active();
        let clock_ppm = if adapting {
            // A deliberate change of reserve is not oscillator drift. Keep
            // the learned clock rate while its extra samples are built/drained.
            self.drift.ppm
        } else if fixed_profile {
            self.drift.update_duration(error, elapsed_ms)
        } else {
            self.drift.update_low_latency(error, elapsed_ms)
        };
        let adaptation_ppm = if adapting {
            // Wake catch-up and target adaptation share the same bidirectional
            // pitch budget. Clock correction is independent and unchanged.
            self.adjustment.update_duration(
                error,
                (frame_size as f64 * f64::from(SAMPLE_RATE) / f64::from(self.config.sample_rate)).round() as usize,
                (elapsed_ms * 48.0).round() as usize,
                self.config.jitter.max_compression_ppm,
            )
        } else {
            0.0
        };
        self.quiet_cooldown_frames = self.quiet_cooldown_frames.saturating_sub(requested);
        let mut ppm = clock_ppm
            + if self.quiet_cooldown_frames > 0 && adaptation_ppm > 0.0 {
                0.0
            } else {
                adaptation_ppm
            };
        if !self.adjustment.active() {
            self.wake_backlog = false;
        }
        let required = if fixed_profile {
            self.resampler.required_frames(requested, ppm)
        } else {
            self.resampler.required_frames_without_lookahead(requested, ppm)
        };
        while self.resampler.buffered_frames() < required {
            // At an idle handover the remaining received media is finite. Fill the
            // rest with zeros instead of manufacturing a PLC tail or dropping PCM.
            if self.draining && self.packets.is_empty() {
                self.finish_draining_tail();
                break;
            }
            let Some(seq) = self.next else {
                break;
            };
            if !fixed_profile
                && self.packets.is_empty()
                && self.resampler.buffered_frames() >= source_requested.saturating_sub(1)
                && required - self.resampler.buffered_frames() == 1
            {
                break;
            }
            if !self.packets.contains_key(&seq)
                && (self.consecutive_loss >= self.config.jitter.max_plc_frames
                    || (self.packets.is_empty() && self.empty_queue_loss >= self.config.jitter.max_plc_frames))
            {
                self.rebuffer_at(Some(now));
                return;
            }
            if *codec_budget == 0 {
                self.stats.work_limited += 1;
                break;
            }
            if !self.decode_next(now, codec_budget, trace.as_deref_mut()) {
                return;
            }
        }
        // Opportunistic lookahead uses only the remaining shared decode budget
        // and already received consecutive packets. It never conceals missing packets,
        // holds the output, or changes normal healthy/startup/draining behavior.
        if adapting
            && !self.draining
            && !self.recovering
            && self.consecutive_loss == 0
            && self
                .spans
                .iter()
                .all(|span| span.kind == OutputKind::Audio && !span.concealed)
            // Protect a whole codec arrival swing as well as the 5 ms cut:
            // a packet arriving is not proof of excess reserve at its trough.
            && error >= (self.config.frame_duration_ms() + 6.0) * 48.0
            && self.quiet_cooldown_frames == 0
        {
            let quiet_frames = (self.config.sample_rate as usize * 20).div_ceil(1000);
            let drop_frames = self.config.sample_rate as usize * 5 / 1000;
            let fade_frames = (self.config.sample_rate as usize).div_ceil(1000);
            let lookahead = quiet_frames.max(required + drop_frames);
            while self.resampler.buffered_frames() < lookahead
                && *codec_budget >= 2
                && self
                    .resampler
                    .samples
                    .iter()
                    .take(quiet_frames * CHANNELS)
                    .all(|sample| sample.abs() <= QUIET_PEAK)
                && self.next.is_some_and(|seq| self.packets.contains_key(&seq))
            {
                if !self.decode_next(now, codec_budget, trace.as_deref_mut()) {
                    return;
                }
                // A corrupt received packet uses its normal bounded PLC path,
                // but optional lookahead must stop before another such decode.
                if self.consecutive_loss > 0 {
                    break;
                }
            }
            if self.resampler.buffered_frames() >= lookahead
                && self
                    .spans
                    .iter()
                    .all(|span| span.kind == OutputKind::Audio && !span.concealed)
                && self
                    .resampler
                    .discard_quiet_prefix(drop_frames, quiet_frames, fade_frames)
            {
                // FIR history still describes emitted media, not discarded media.
                // Advance source attribution without advancing that output history.
                self.discard_spans(drop_frames);
                self.stats.quiet_discarded_ms += drop_frames as f64 * 1000.0 / f64::from(self.config.sample_rate);
                self.quiet_cooldown_frames = (self.output_rate() as usize * 20).div_ceil(1000);
                ppm = clock_ppm;
            }
        }
        if let Some(trace) = trace.as_deref_mut()
            && let Some(first) = self.spans.front()
        {
            trace.sequence = Some(first.sequence);
            trace.sequence_offset_ns = ((frame_size.saturating_sub(first.frames) as f64 + self.resampler.phase) * 1e9
                / f64::from(self.config.sample_rate))
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
                        .required_frames(requested, ppm)
                        .min(self.resampler.buffered_frames())
                {
                    break;
                }
            }
        }
        self.classify_output(ppm, !fixed_profile || self.draining);
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
        // Report adaptation actually given to the resampler separately from
        // the unchanged oscillator correction.
        let applied_adaptation_ppm = ppm - clock_ppm;
        self.stats.adaptation_ppm = applied_adaptation_ppm;
        let adjusted_ms = applied_adaptation_ppm * elapsed_ms / 1_000_000.0;
        self.stats.adaptation_stretched_ms += (-adjusted_ms).max(0.0);
        self.stats.adaptation_compressed_ms += adjusted_ms.max(0.0);
        self.consume_spans(before - self.resampler.buffered_frames());
        self.finish_draining_tail();
        if let Some(last) = output.as_chunks::<CHANNELS>().0.last() {
            self.last_output.copy_from_slice(last);
        }
    }
    pub(crate) fn output_kinds(&self) -> &[OutputKind] {
        &self.output_kinds[..self.output_frames]
    }
    fn finish_draining_tail(&mut self) {
        if self.draining && self.packets.is_empty() && self.resampler.finish_tail(self.retiring) {
            self.spans.push_back(DecodedSpan {
                sequence: self.last_decoded.unwrap_or(0),
                frames: self.resampler.kernel.taps() - 1,
                arrival: None,
                concealed: false,
                kind: OutputKind::Intentional,
            });
        }
    }
    fn classify_output(&mut self, ppm: f64, hold_boundary: bool) {
        let buffered = self.resampler.buffered_frames();
        let mut phase = self.resampler.phase;
        let step = self.resampler.ratio(ppm);
        let mut spans = self.spans.iter();
        let mut span = spans.next();
        let mut end = span.map_or(0, |s| s.frames);
        let fallback = if self.draining {
            OutputKind::Intentional
        } else {
            OutputKind::Missing
        };
        for kind in &mut self.output_kinds[..self.output_frames] {
            let raw = phase.ceil() as usize;
            if raw >= buffered && !(hold_boundary && raw == buffered && buffered > 0) {
                *kind = fallback;
            } else if raw < self.kind_history.len() {
                *kind = self.kind_history[(self.kind_cursor + raw) % self.kind_history.len()];
            } else {
                let index = raw - self.kind_history.len();
                while index >= end {
                    span = spans.next();
                    if let Some(s) = span {
                        end += s.frames;
                    } else {
                        break;
                    }
                }
                *kind = span.map_or(fallback, |s| s.kind);
            }
            phase += step;
        }
    }
    fn decode_next(&mut self, now: Instant, codec_budget: &mut usize, trace: Option<&mut RenderTrace>) -> bool {
        let Some(seq) = self.next else {
            return false;
        };
        let started = trace.as_ref().map(|_| Instant::now());
        let packet = self.packets.remove(&seq);
        if packet.is_some() {
            *codec_budget -= 1;
        }
        let arrival = packet.as_ref().map(|packet| packet.arrival);
        let mut storage = [0.0; FRAME_SAMPLES * 2];
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
            (DecoderCodec::Pcm24, Some(packet)) => {
                for (sample, bytes) in pcm.iter_mut().zip(packet.payload[..packet.len].as_chunks::<3>().0) {
                    let value =
                        i32::from_le_bytes([bytes[0], bytes[1], bytes[2], if bytes[2] & 0x80 != 0 { 255 } else { 0 }]);
                    *sample = value as f32 / 8_388_608.0;
                }
                true
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
        let mut kind = OutputKind::Audio;
        let mut plc_ns = 0;
        if valid {
            self.consecutive_loss = 0;
            self.healthy_frames =
                (self.healthy_frames + self.config.frame_size()).min(self.config.sample_rate as usize);
            if self.healthy_frames == self.config.sample_rate as usize {
                self.empty_queue_loss = 0;
            }
        } else {
            kind = OutputKind::Missing;
            self.healthy_frames = 0;
            self.stats.lost += 1;
            self.dropout(seq, seq, now, false);
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
                self.rebuffer_at(Some(now));
                return false;
            }
            let plc_started = trace.as_ref().map(|_| Instant::now());
            match &mut self.decoder {
                DecoderCodec::Opus(decoder) => {
                    if *codec_budget == 0 {
                        self.stats.work_limited += 1;
                        pcm.fill(0.0);
                    } else {
                        *codec_budget -= 1;
                        if decoder.decode_float(&[], pcm, false).ok() != Some(self.config.frame_size()) {
                            pcm.fill(0.0);
                        } else {
                            kind = OutputKind::Concealed;
                        }
                    }
                }
                DecoderCodec::Pcm16 | DecoderCodec::Pcm24 => {
                    // PCM fallback also consumes one bounded work unit.
                    *codec_budget = codec_budget.saturating_sub(1);
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
            if let Some(started) = plc_started {
                plc_ns = duration_ns(started.elapsed());
            }
        }
        self.resampler.push(pcm);
        self.spans.push_back(DecodedSpan {
            sequence: seq,
            frames: self.config.frame_size(),
            arrival,
            concealed: !valid,
            kind,
        });
        self.consumed[seq as usize % self.consumed.len()] = Some((seq, now));
        self.last_decoded = Some(seq);
        self.next = seq.checked_add(1);
        if let Some(trace) = trace {
            trace.decoded_first_sequence.get_or_insert(seq);
            trace.decoded_last_sequence = Some(seq);
            trace.decode_ns += duration_ns(started.unwrap().elapsed());
            trace.plc_ns += plc_ns;
        }
        true
    }
    fn consume_spans(&mut self, mut frames: usize) {
        while frames > 0 {
            let Some(span) = self.spans.front_mut() else {
                break;
            };
            let consumed = frames.min(span.frames);
            for _ in 0..consumed {
                self.kind_history[self.kind_cursor] = span.kind;
                self.kind_cursor = (self.kind_cursor + 1) % self.kind_history.len();
            }
            span.frames -= consumed;
            frames -= consumed;
            if span.frames == 0 {
                self.spans.pop_front();
            }
        }
    }
    fn discard_spans(&mut self, mut frames: usize) {
        while frames > 0 {
            let Some(span) = self.spans.front_mut() else { break };
            let discarded = frames.min(span.frames);
            span.frames -= discarded;
            frames -= discarded;
            if span.frames == 0 {
                self.spans.pop_front();
            }
        }
    }
    pub(crate) fn set_retiring(&mut self, retiring: bool) {
        self.retiring = retiring;
    }
    pub(crate) fn set_draining(&mut self, draining: bool) {
        self.draining = draining;
    }
    /// New media burst, using the existing preallocated storage on the RT worker.
    pub fn reset_burst(&mut self) {
        self.dropout_ledger.clear();
        self.late_ledger.clear();
        self.packets.0.clear();
        self.rebuffer_at(None);
        self.consumed.fill(None);
        self.late_packet = None;
        self.last_decoded = None;
        self.adaptive.arrivals.clear();
        self.adaptive.target = self.config.jitter.initial_frames;
        self.adaptive.anchor = None;
        self.adaptive.jitter_ms = 0.0;
        self.adaptive.spread_ms = 0.0;
        self.adaptive.source_batch_ms = 0.0;
        self.adaptive.lower_candidate = None;
        self.adaptive.last_decay = None;
        self.stats = Stats::default();
        self.rejections = Rejections::default();
        self.wake_backlog = true;
        self.recovering = false;
        self.draining = false;
    }
    fn rebuffer_at(&mut self, at: Option<Instant>) {
        self.output_kinds.fill(if at.is_some() && !self.draining {
            OutputKind::Missing
        } else {
            OutputKind::Intentional
        });
        // Starvation can retire an already decoded partial packet. It really
        // loses queued audio; intentional burst boundaries do not count it.
        if let Some(at) = at {
            for index in 0..self.spans.len() {
                let sequence = self.spans[index].sequence;
                self.dropout(sequence, sequence, at, false);
            }
        }
        self.next = None;
        self.prefill_ticks = 0;
        self.first_packet = self.packets.values().map(|packet| packet.arrival).min();
        self.consecutive_loss = 0;
        self.empty_queue_loss = 0;
        self.healthy_frames = 0;
        self.recovering = true;
        self.resampler.clear();
        self.spans.clear();
        self.kind_history.fill(OutputKind::Intentional);
        self.kind_cursor = 0;
        self.drift = DriftController::default();
        self.adjustment = PlayoutAdjustment::default();
        self.quiet_cooldown_frames = 0;
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
    source_phase_ms: f64,
}
struct AdaptiveJitter {
    config: Config,
    target: usize,
    arrivals: VecDeque<Arrival>,
    anchor: Option<(u64, Instant, Option<u64>)>,
    jitter_ms: f64,
    spread_ms: f64,
    source_batch_ms: f64,
    lower_candidate: Option<(Instant, usize)>,
    last_decay: Option<Instant>,
}
impl AdaptiveJitter {
    fn source_queue_phase_ms(&self, now: Instant) -> f64 {
        if !self.anchor.is_some_and(|(_, _, source)| source.is_some()) {
            return 0.0;
        }
        let age_ms = self
            .arrivals
            .back()
            .and_then(|last| now.checked_duration_since(last.at))
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0;
        // Four 5 ms packets emitted together make the queue fall through 20 ms
        // before its next refill. Subtract that phase relative to the ordinary
        // single-packet mean, rather than treating its upper half as surplus.
        let frame_ms = self.config.frame_duration_ms();
        (self.source_batch_ms + frame_ms - age_ms).max(0.0) - frame_ms * 0.5
    }

    fn new(config: Config) -> Self {
        Self {
            config,
            target: config.jitter.initial_frames,
            arrivals: prepared_deque(MAX_ARRIVALS + 1, || Arrival {
                sequence: 0,
                at: Instant::now(),
                transit_ms: 0.0,
                source_phase_ms: 0.0,
            }),
            anchor: None,
            jitter_ms: 0.0,
            spread_ms: 0.0,
            source_batch_ms: 0.0,
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
            self.source_batch_ms = 0.0;
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
            source_phase_ms: source_elapsed - seq_delta * self.config.frame_duration_ms(),
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
        let mut source_min = f64::INFINITY;
        let mut source_max = f64::NEG_INFINITY;
        for (value, arrival) in values.iter_mut().zip(&self.arrivals) {
            *value = arrival.transit_ms;
            source_min = source_min.min(arrival.source_phase_ms);
            source_max = source_max.max(arrival.source_phase_ms);
        }
        let values = &mut values[..self.arrivals.len()];
        values.sort_unstable_by(f64::total_cmp);
        // Retain the upper envelope of a burst, while ignoring one isolated
        // late/corrupt timestamp. A 99th percentile in a 400-packet window
        // discards 10 ms of a 200 ms batch and under-protects its next arrival.
        let upper = values.len().saturating_sub(2);
        self.spread_ms = (values[upper] - values[0]).max(0.0);
        // This is source availability, separate from transport variation. With
        // no sender timestamp source_elapsed already follows media time, making
        // this zero; arrival-based jitter then accounts for the batch itself.
        self.source_batch_ms = (source_max - source_min).clamp(
            0.0,
            self.config.jitter.max_frames as f64 * self.config.frame_duration_ms(),
        );
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
            // one-second measurement window clears, require the profile's stable
            // interval of lower demand, then decay in rounded codec frames. Receiver
            // drains the resulting reserve by rate correction, never a skip.
            let (since, candidate) = self.lower_candidate.get_or_insert((at, desired));
            *candidate = (*candidate).max(desired);
            if at.duration_since(*since) >= Duration::from_secs(u64::from(self.config.jitter.target_stable_secs))
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

/// Deliberate reserve changes use continuous resampling with at most -0.2%
/// expansion and a per-profile compression ceiling, plus clock correction. This does
/// not preserve pitch. Building 20 ms takes at least 10 s; rate-only draining
/// takes at least 4/10 s by profile. Starvation still needs bounded PLC/re-prefill.
/// This controller only changes rate; the receiver separately handles quiet cuts.
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
    fn update_duration(
        &mut self,
        occupancy_error: f64,
        codec_frames: usize,
        consumed_frames: usize,
        max_compression_ppm: u32,
    ) -> f64 {
        // One full arrival batch does not imply a full playout reserve. Follow
        // a 250 ms smoothed occupancy error until it stays near the requested
        // reserve for a second; this also drains overshoot after re-prefill.
        // With no target change this controller is never engaged, so a healthy
        // stream still uses exactly the original oscillator drift path.
        let dt = consumed_frames as f64 / f64::from(SAMPLE_RATE);
        self.filtered_error += (1.0 - (-dt / 0.250).exp()) * (occupancy_error - self.filtered_error);
        let mut requested = (self.filtered_error * 20.0).clamp(-MAX_ADAPTATION_PPM, f64::from(max_compression_ppm));
        // Never deliberately drain a reserve which is currently below target.
        // The phase/rate still slews continuously; a packet gap is not a skip.
        if occupancy_error < 0.0 && requested > 0.0 {
            requested = 0.0;
        }
        // Retain the existing slew and continuous phase within the smaller cap.
        let slew = consumed_frames as f64 / 48.0 * 100.0;
        self.ppm += (requested - self.ppm).clamp(-slew, slew);
        if self.filtered_error.abs() < codec_frames as f64 * 0.5 && self.ppm.abs() < 1000.0 {
            self.settled_frames += consumed_frames;
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

    #[cfg(test)]
    fn update(&mut self, error_frames: f64) -> f64 {
        self.update_duration(error_frames, 10.0)
    }
    fn update_duration(&mut self, error_frames: f64, elapsed_ms: f64) -> f64 {
        let dt = elapsed_ms / 10.0;
        self.filtered_error += (1.0 - 0.98_f64.powf(dt)) * (error_frames - self.filtered_error);
        self.integral = (self.integral + self.filtered_error * 0.003 * dt).clamp(-1000.0, 1000.0);
        let target = (self.filtered_error * 0.5 + self.integral).clamp(-1000.0, 1000.0);
        self.ppm += (target - self.ppm).clamp(-10.0 * dt, 10.0 * dt);
        self.ppm
    }
}

/// Band-limited asynchronous conversion. Positive ppm consumes input faster.
#[derive(Debug)]
pub struct DriftResampler {
    tail_pending: bool,
    rate_ratio: f64,
    kernel: crate::resample::Kernel,
    history: crate::resample::History,
    samples: VecDeque<f32>,
    channels: usize,
    phase: f64,
}

impl DriftResampler {
    pub fn new(channels: usize) -> Self {
        let kernel = crate::resample::Kernel::new(1.011);
        Self {
            tail_pending: false,
            rate_ratio: 1.0,
            history: crate::resample::History::new(channels.clamp(1, 32), kernel.taps()),
            samples: prepared_deque((MAX_RESAMPLE_FRAMES + kernel.taps()) * channels.clamp(1, 32), || 0.0),
            kernel,
            channels: channels.clamp(1, 32),
            phase: 0.0,
        }
    }

    fn set_rates(&mut self, input: u32, output: u32) {
        self.rate_ratio = f64::from(input) / f64::from(output);
        self.kernel = crate::resample::Kernel::new(self.rate_ratio * 1.011);
        self.history = crate::resample::History::new(self.channels, self.kernel.taps());
        self.samples
            .reserve((MAX_RESAMPLE_FRAMES + self.kernel.taps()) * self.channels - self.samples.len());
    }
    fn ratio(&self, ppm: f64) -> f64 {
        self.rate_ratio * Self::step(ppm)
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.tail_pending |= !samples.is_empty();
        for frame in samples.chunks_exact(self.channels) {
            if self.buffered_frames() == MAX_RESAMPLE_FRAMES {
                self.samples.drain(..self.channels);
                self.phase = 0.0;
            }
            self.samples
                .extend(frame.iter().map(|&v| if v.is_finite() { v } else { 0.0 }));
        }
    }

    fn finish_tail(&mut self, hold: bool) -> bool {
        if !self.tail_pending {
            return false;
        }
        // Retirement already applies a downstream fade which holds the last
        // frame after PCM ends. Extend that same endpoint through the FIR so
        // zero padding cannot introduce a new edge before the fade completes.
        let mut last = [0.0; 32];
        if hold {
            let frames = self.buffered_frames();
            for (channel, value) in last[..self.channels].iter_mut().enumerate() {
                *value = if frames == 0 {
                    self.history.get(-1, channel)
                } else {
                    self.samples[(frames - 1) * self.channels + channel]
                };
            }
        }
        self.samples
            .extend((0..(self.kernel.taps() - 1) * self.channels).map(|i| last[i % self.channels]));
        self.tail_pending = false;
        true
    }
    pub fn buffered_frames(&self) -> usize {
        self.samples.len() / self.channels
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.history.clear();
        self.tail_pending = false;
        self.phase = 0.0;
    }

    /// Remove only a verified near-silent prefix. The shared stereo offset and
    /// 1 ms linear crossfade preserve the first sample and the original endpoint.
    /// With fade <= drop, writes cannot overwrite a later read. No allocation.
    fn discard_quiet_prefix(&mut self, drop: usize, guard: usize, fade: usize) -> bool {
        if drop == 0
            || fade < 2
            || fade > drop
            || guard < drop + fade
            || self.buffered_frames() < guard
            || self
                .samples
                .iter()
                .take(guard * self.channels)
                .any(|sample| sample.abs() > QUIET_PEAK)
        {
            return false;
        }
        for frame in 0..fade {
            let weight = frame as f32 / (fade - 1) as f32;
            for channel in 0..self.channels {
                let first = self.samples[frame * self.channels + channel];
                let second = self.samples[(drop + frame) * self.channels + channel];
                self.samples[(drop + frame) * self.channels + channel] = if frame + 1 == fade {
                    second
                } else {
                    first + (second - first) * weight
                };
            }
        }
        self.samples.drain(..drop * self.channels);
        true
    }

    fn step(ppm: f64) -> f64 {
        1.0 + if ppm.is_finite() {
            ppm.clamp(-(MAX_ADAPTATION_PPM + 1000.0), f64::from(MAX_COMPRESSION_PPM) + 1000.0)
        } else {
            0.0
        } / 1_000_000.0
    }

    fn required_frames(&self, frames: usize, ppm: f64) -> usize {
        if frames == 0 {
            return 0;
        }
        ((self.phase + (frames - 1) as f64 * self.ratio(ppm)).ceil() as usize + 1)
            .max((self.phase + frames as f64 * self.ratio(ppm)).floor() as usize)
    }
    fn required_frames_without_lookahead(&self, frames: usize, ppm: f64) -> usize {
        if frames == 0 {
            return 0;
        }
        ((self.phase + (frames - 1) as f64 * self.ratio(ppm)).floor() as usize + 1)
            .max((self.phase + frames as f64 * self.ratio(ppm)).floor() as usize)
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
        let step = self.ratio(ppm);
        for frame in out.chunks_exact_mut(self.channels) {
            let index = self.phase.floor() as usize;
            if index >= self.buffered_frames() {
                if !(hold_boundary && index == self.buffered_frames() && index > 0) {
                    break;
                }
                held_samples += 1;
            }
            for (channel, sample) in frame.iter_mut().enumerate() {
                *sample = self.kernel.sample(self.phase, |index| {
                    if index < 0 {
                        self.history.get(index, channel)
                    } else {
                        self.samples[index.min(self.buffered_frames() as isize - 1) as usize * self.channels + channel]
                    }
                });
            }
            self.phase += step;
        }
        let consumed = (self.phase.floor() as usize).min(self.buffered_frames());
        self.history
            .retain(consumed, |frame, channel| self.samples[frame * self.channels + channel]);
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
    fn arbitrary_pull_consumes_requested_samples_and_keeps_only_codec_remainder() {
        let mut config = Config {
            sample_rate: SAMPLE_RATE,
            codec: Codec::Pcm16,
            bitrate: 1_536_000,
            ..Config::low_latency_opus()
        };
        config.jitter.adaptive = false;
        let mut encoder = Encoder::new_with_config(config).unwrap();
        let packet = encoder.encode(&vec![0.25; config.frame_samples()]).unwrap();
        for frames in [1, 64, 128, 139, 140, 256, 441, 480] {
            let mut receiver = Receiver::new_with_config(config).unwrap();
            let origin = Instant::now();
            for seq in 0..4 {
                assert!(receiver.push_slice_at(seq, &packet, origin, None));
            }
            let mut output = [0.0; FRAME_SAMPLES];
            let mut budget = 4;
            crate::allocation_audit::assert_no_heap(|| {
                receiver.pull_into(
                    origin + Duration::from_millis(20),
                    frames,
                    &mut output[..frames * CHANNELS],
                    &mut budget,
                    None,
                );
            });
            let decoded = frames.div_ceil(config.frame_size());
            assert_eq!(4 - budget, decoded, "request={frames}");
            assert_eq!(
                receiver.resampler.buffered_frames(),
                decoded * config.frame_size() - frames
            );
            assert_eq!(receiver.output_kinds().len(), frames);
            assert!(
                receiver.output_kinds()[..frames.min(47)]
                    .iter()
                    .all(|kind| *kind == OutputKind::Intentional)
            );
            assert!(
                receiver.output_kinds()[frames.min(48)..]
                    .iter()
                    .all(|kind| *kind == OutputKind::Audio)
            );
            assert!(
                output[(96.min(frames)) * CHANNELS..frames * CHANNELS]
                    .iter()
                    .all(|sample| (*sample - 0.25).abs() < 0.0001)
            );
        }
        let origin = Instant::now();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        for seq in 0..4 {
            receiver.push_slice_at(seq, &packet, origin, None);
        }
        for remaining in [112, 224] {
            receiver.pull_into(origin + Duration::from_millis(20), 128, &mut [0.0; 256], &mut 2, None);
            assert_eq!(receiver.resampler.buffered_frames(), remaining);
        }
    }

    #[test]
    fn pull_codec_budget_is_shared_across_tiles_and_exhaustion_is_finite_missing() {
        let mut config = Config::low_latency_opus();
        config.jitter.adaptive = false;
        let mut encoder = Encoder::new_with_config(config).unwrap();
        let packet = encoder.encode(&vec![0.1; config.frame_samples()]).unwrap();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        let origin = Instant::now();
        for seq in 0..8 {
            receiver.push_slice_at(seq, &packet, origin, None);
        }
        let mut budget = 1;
        let mut output = [0.0; 240];
        crate::allocation_audit::assert_no_heap(|| {
            receiver.pull_into(origin, 480, &mut output, &mut budget, None);
            receiver.pull_into(origin, 360, &mut output, &mut budget, None);
            assert_eq!(budget, 0);
            assert_eq!(receiver.stats().queued_frames, 7);
            receiver.pull_into(origin, 240, &mut output, &mut budget, None);
        });
        assert_eq!(receiver.stats().queued_frames, 7, "no unbudgeted decode");
        assert_eq!(receiver.stats().work_limited, 1);
        assert!(output.iter().all(|sample| *sample == 0.0));
        assert!(receiver.output_kinds().iter().all(|kind| *kind == OutputKind::Missing));
        receiver.pull_into(origin, 120, &mut output, &mut 1, None);
        assert_eq!(receiver.stats().queued_frames, 6);
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
    fn output_provenance_tracks_fractional_mixed_spans_and_bounded_boundary_holds() {
        let mut r = Receiver::new_with_config(Config::low_latency_opus()).unwrap();
        r.resampler.push(&[0.2; 480]);
        r.resampler.phase = 0.5;
        r.spans.push_back(DecodedSpan {
            sequence: 1,
            frames: 120,
            arrival: None,
            concealed: false,
            kind: OutputKind::Audio,
        });
        r.spans.push_back(DecodedSpan {
            sequence: 2,
            frames: 120,
            arrival: None,
            concealed: true,
            kind: OutputKind::Concealed,
        });
        r.classify_output(0.0, true);
        assert!(r.output_kinds()[..47].iter().all(|k| *k == OutputKind::Intentional));
        assert!(r.output_kinds()[47..167].iter().all(|k| *k == OutputKind::Audio));
        assert!(r.output_kinds()[167..].iter().all(|k| *k == OutputKind::Concealed));
        r.rebuffer_at(Some(Instant::now()));
        assert!(r.output_kinds().iter().all(|k| *k == OutputKind::Missing));
        r.reset_burst();
        assert!(r.output_kinds().iter().all(|k| *k == OutputKind::Intentional));
    }
    #[test]
    fn dropouts_deduplicate_concealment_late_and_replays_in_the_same_ring() {
        let mut config = Config::low_latency_pcm();
        config.jitter.adaptive = false;
        let payload = pcm_packet(config, 0.2);
        let now = Instant::now();
        let counters = std::sync::Arc::new(crate::sink::DropoutCounters::default());
        let mut receiver = Receiver::new_with_config(config).unwrap();
        receiver.set_dropout_counters(Some(&counters));
        for seq in [0, 1, 3, 4, 5] {
            assert!(receiver.push_at(seq, payload.clone(), now));
        }
        for tick in 0..8 {
            receiver.render_at(now + Duration::from_millis(1000 + tick * 3));
        }
        assert!(receiver.stats().lost > 0);
        let at = (crate::diagnostics::monotonic_us(now + Duration::from_millis(1500)).max(0) / 1000) as u64;
        let total = || {
            counters
                .snapshot(at)
                .into_iter()
                .flatten()
                .map(|b| b.count())
                .sum::<u64>()
        };
        let before = total();
        for seq in [2, 2] {
            assert!(!receiver.push_at(seq, payload.clone(), now + Duration::from_millis(1400)));
        }
        assert_eq!(total(), before, "concealed then late is one dropout");
        for seq in [0, 0] {
            assert!(!receiver.push_at(seq, payload.clone(), now + Duration::from_millis(1400)));
        }
        assert_eq!(
            total(),
            before + 1,
            "receiver-classified late replays count at most once"
        );
        receiver.reset_burst();
        assert_eq!(total(), before + 1);
    }
    #[test]
    fn starvation_discard_of_decoded_audio_counts_but_burst_reset_does_not() {
        let config = Config::low_latency_pcm();
        let now = Instant::now();
        let c = std::sync::Arc::new(crate::sink::DropoutCounters::default());
        let mut r = Receiver::new_with_config(config).unwrap();
        r.set_dropout_counters(Some(&c));
        let payload = pcm_packet(config, 0.2);
        assert!(r.push_at(99, payload.clone(), now));
        r.next = Some(99);
        assert!(r.decode_next(now, &mut 2, None));
        assert!(r.resampler.buffered_frames() > 0);
        let at = (crate::diagnostics::monotonic_us(now).max(0) / 1000) as u64;
        assert!(c.snapshot(at).iter().all(Option::is_none));
        r.rebuffer_at(Some(now));
        assert_eq!(c.snapshot(at).into_iter().flatten().map(|b| b.count()).sum::<u64>(), 1);
        assert!(!r.push_at(99, payload.clone(), now));
        assert_eq!(c.snapshot(at).into_iter().flatten().map(|b| b.count()).sum::<u64>(), 1);
        r.reset_burst();
        assert!(r.push_at(100, payload, now));
        r.next = Some(100);
        assert!(r.decode_next(now, &mut 2, None));
        r.reset_burst();
        assert_eq!(c.snapshot(at).into_iter().flatten().map(|b| b.count()).sum::<u64>(), 1);
    }
    #[test]
    fn recovery_skips_count_only_on_playout_and_later_arrivals_do_not_count_again() {
        let mut config = Config::low_latency_pcm();
        config.jitter.adaptive = false;
        let payload = pcm_packet(config, 0.2);
        let now = Instant::now();
        let c = std::sync::Arc::new(crate::sink::DropoutCounters::default());
        let mut r = Receiver::new_with_config(config).unwrap();
        r.set_dropout_counters(Some(&c));
        for seq in 0..4 {
            assert!(r.push_at(seq, payload.clone(), now));
        }
        // Admission/window rejection alone does not assert an audible gap.
        assert!(!r.push_at(1000000, payload.clone(), now));
        assert!(
            c.snapshot((crate::diagnostics::monotonic_us(now).max(0) / 1000) as u64)
                .iter()
                .all(Option::is_none)
        );
        for tick in 0..24 {
            r.render_at(now + Duration::from_millis(1000 + tick * 3));
        }
        assert!(r.next.is_none());
        let last = r.last_decoded.unwrap();
        let at = (crate::diagnostics::monotonic_us(now + Duration::from_millis(2000)).max(0) / 1000) as u64;
        let total = || c.snapshot(at).into_iter().flatten().map(|b| b.count()).sum::<u64>();
        let before = total();
        let first = last + 50;
        for seq in first..first + 4 {
            assert!(r.push_at(seq, payload.clone(), now + Duration::from_millis(1500)));
        }
        r.render_at(now + Duration::from_millis(1600));
        assert_eq!(total(), before + 49);
        assert!(!r.push_at(first - 1, payload, now + Duration::from_millis(1700)));
        assert_eq!(total(), before + 49);
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
                    let expected = rendered as f64 * DriftResampler::step(ppm) - resampler.kernel.delay() as f64;
                    if rendered > 96 {
                        assert!((f64::from(frame[0]) - expected).abs() < 0.05);
                    }
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
        let mut receiver = Receiver::new_with_config(config).unwrap();
        let now = Instant::now();
        receiver.push_at(0, encoded, now);
        receiver.next = Some(0);
        assert!(receiver.decode_next(now, &mut 2, None));
        for (actual, expected) in receiver.resampler.samples.iter().zip(&input) {
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
        assert_eq!(receiver.stats().boundary_hold_samples, 0);
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
    fn adaptive_and_fixed_playout_settle_near_minimum_without_extra_loss() {
        let adaptive = simulate_arrival_trace(Config::low_latency_pcm(), 40_000);
        let mut fixed = Config::low_latency_pcm();
        fixed.jitter.adaptive = false;
        let fixed = simulate_arrival_trace(fixed, 40_000);
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
        // Over the longer settling interval the fixed controller also drains
        // toward its minimum. Compare within one packet of callback phase,
        // rather than requiring adaptive to beat an equally settled stream.
        assert!(adaptive.buffered_ms <= fixed.buffered_ms + Config::low_latency_pcm().frame_duration_ms());
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
        let mut events: Vec<_> = (0..24000_u64)
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
        for tick in 0..24000_u64 {
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
            if now_ns >= 56_000_000_000 {
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
                let mut events: Vec<_> = (0..130_000_000 / period_us)
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
                for now_us in (0..130_000_000).step_by(1000) {
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
    fn several_source_batches_already_supply_startup_reserve() {
        let config = Config::low_latency_opus();
        let mut encoder = Encoder::new_with_config(config).unwrap();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        receiver.reset_burst();
        let origin = Instant::now();
        let mut payload = [0; MAX_PACKET_BYTES];
        // A network burst delivers three complete capture batches together.
        // Its media already covers the measured network reserve; another wait
        // would needlessly retain fresh packets behind the filled queue.
        for frame in 0..12 {
            let len = encoder.encode_into(&[0.1; 480], &mut payload).unwrap();
            assert!(receiver.push_slice_at(
                frame + 1,
                &payload[..len],
                origin,
                Some(frame / 4 * 20_000_000 + frame % 4 * 100),
            ));
        }
        assert!(receiver.stats().current_target_ms > 5.0);
        receiver.pull_into(origin, 128, &mut [0.0; 256], &mut 4, None);
        assert_eq!(receiver.stats().startup_hold_frames, 0);
        assert!(receiver.stats().queued_frames < 12);
        assert_eq!(receiver.stats().lost, 0);
    }

    #[test]
    fn healthy_twenty_ms_source_batches_do_not_repeatedly_conceal_with_native_pull() {
        // Four 5 ms codec packets from one continuous capture buffer are emitted
        // together. Every packet arrives immediately and in order. Exercise actual
        // native pulls, including device-clock mismatch, not fixed-frame rendering.
        for opened_us in [1000_u64, 33_000] {
            for callback_frames in [128_usize, 480] {
                for device_ppm in [-700.0, 0.0, 700.0] {
                    let config = Config::low_latency_opus();
                    let mut encoder = Encoder::new_with_config(config).unwrap();
                    let mut receiver = Receiver::new_with_config(config).unwrap();
                    receiver.reset_burst();
                    let origin = Instant::now();
                    let mut payload = [0; MAX_PACKET_BYTES];
                    let mut output = [0.0; FRAME_SAMPLES];
                    let mut batch = 0_u64;
                    let mut callback = 0_u64;
                    let mut learned_loss = 0;
                    for now_us in (0..15_000_000).step_by(100) {
                        while batch * 20_000 <= now_us {
                            let emission_us = batch * 20_000;
                            for packet in 0..4 {
                                let len = encoder.encode_into(&[0.1; 480], &mut payload).unwrap();
                                receiver.push_slice_at(
                                    batch * 4 + packet + 1,
                                    &payload[..len],
                                    origin + Duration::from_micros(emission_us),
                                    Some(emission_us * 1000 + packet * 100),
                                );
                            }
                            batch += 1;
                        }
                        while opened_us as f64
                            + callback as f64 * callback_frames as f64 / 0.048 / (1.0 + device_ppm / 1_000_000.0)
                            <= now_us as f64
                        {
                            let mut budget = 2 * ((callback_frames * 1011).div_ceil(config.frame_size() * 1000) + 1);
                            receiver.pull_into(
                                origin + Duration::from_micros(now_us),
                                callback_frames,
                                &mut output[..callback_frames * CHANNELS],
                                &mut budget,
                                None,
                            );
                            callback += 1;
                        }
                        if now_us == 8_000_000 {
                            learned_loss = receiver.stats().lost;
                        }
                    }
                    let stats = receiver.stats();
                    assert_eq!(stats.current_target_ms, 5.0);
                    assert_eq!(stats.latency_trimmed_frames, 0);
                    assert_eq!(stats.work_limited, 0);
                    assert_eq!(stats.rejected, 0);
                    assert_eq!(
                        stats.lost, learned_loss,
                        "healthy batches: open_us={opened_us}, callback={callback_frames}, ppm={device_ppm}: {stats:?}"
                    );
                    assert_eq!(
                        stats.lost + stats.late,
                        0,
                        "a healthy start must not lose media: {stats:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn continued_burst_after_fallback_wake_rebuilds_reserve_on_a_steady_batched_source() {
        for continued in [true, false] {
            for callback_phase_us in [0, 1000, 2500] {
                let config = Config::low_latency_opus();
                let mut encoder = Encoder::new_with_config(config).unwrap();
                let mut payload = [0; MAX_PACKET_BYTES];
                // Sustained tone stays audible after Opus DC rejection.
                let tone: [f32; 480] =
                    std::array::from_fn(|i| 0.1 * (std::f32::consts::TAU * 1200.0 * (i / 2) as f32 / 48_000.0).sin());
                // Retain old encoder state, as in a fixed_profile same-burst wake. A new
                // receiver must recover its timing even without a paired source reset.
                for _ in 0..32 {
                    encoder.encode_into(&tone, &mut payload).unwrap();
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
                for now_us in (0..22_000_000).step_by(100) {
                    // WASAPI emits two 5 ms packets per 10 ms capture buffer. Source
                    // timestamps preserve that cadence, not invented 5 ms spacing.
                    // At +0.5% catch-up, perturb the source while adjustment is
                    // still engaged; twelve seconds is already past settling.
                    let phase = if next_batch >= 400 { 3000 } else { 0 };
                    while next_batch * 10_000 + phase <= now_us {
                        let arrival_us = next_batch * 10_000 + phase;
                        let at = origin + Duration::from_micros(arrival_us);
                        for packet in 0..2 {
                            let sequence = first_sequence + next_batch * 2 + packet;
                            let len = encoder.encode_into(&tone, &mut payload).unwrap();
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
                    // Observe fourteen full seconds after the post-change settling
                    // interval, not just the final second or final sample.
                    if now_us >= 8_000_000 {
                        assert_eq!(
                            receiver.stats().lost,
                            losses_after_learning,
                            "steady playout at {now_us}"
                        );
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
        // Start after the configured jitter wait so this exercises starvation,
        // independently of the cold-start hold for the initial two-packet batch.
        for tick in 0..200 {
            let at = origin + Duration::from_millis((tick + 1) * 5);
            if tick >= 3 && tick % 2 == 1 {
                receiver.push_slice_at(tick + 1, &payload, at, Some((tick + 1) * 5_000_000));
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
    fn starvation_refill_reanchors_after_a_second_packet_outage() {
        for mut config in [
            Config::low_latency_pcm(),
            Config::low_latency_opus(),
            Config::opus_10ms(),
        ] {
            config.jitter.adaptive = false;
            let origin = Instant::now();
            let mut encoder = Encoder::new_with_config(config).unwrap();
            let payload = encoder.encode(&[0.2; FRAME_SAMPLES][..config.frame_samples()]).unwrap();
            let mut receiver = Receiver::new_with_config(config).unwrap();
            let mut output = [0.0; FRAME_SAMPLES];
            assert!(receiver.push_slice_at(1, &payload, origin, None));
            for tick in 0..40 {
                receiver.render_into(
                    origin + Duration::from_millis(tick * 10),
                    config.frame_size(),
                    &mut output[..config.frame_samples()],
                    None,
                );
            }
            assert!(receiver.recovering && receiver.next.is_none());
            // One packet arrives during refill, then another loss interval carries
            // the sender beyond this partial queue's entire sequence window.
            let stranded = 100;
            assert!(receiver.push_slice_at(stranded, &payload, origin + Duration::from_secs(1), None));
            receiver.render_into(
                origin + Duration::from_secs(1),
                config.frame_size(),
                &mut output[..config.frame_samples()],
                None,
            );
            assert!(receiver.recovering && receiver.next.is_none());
            let resumed = stranded + receiver.queue_capacity() as u64 * 2;
            assert!(
                receiver.push_slice_at(resumed, &payload, origin + Duration::from_secs(2), None),
                "healthy media must replace a stranded refill window"
            );
            let required = receiver.adaptive.target + usize::from(!config.is_opus_10ms());
            for offset in 1..required {
                assert!(receiver.push_slice_at(
                    resumed + offset as u64,
                    &payload,
                    origin + Duration::from_secs(2),
                    None
                ));
            }
            receiver.render_into(
                origin + Duration::from_secs(2),
                config.frame_size(),
                &mut output[..config.frame_samples()],
                None,
            );
            assert!(!receiver.recovering);
            assert!(
                output[..config.frame_samples()]
                    .iter()
                    .any(|sample| sample.abs() > 0.01)
            );
            assert!(receiver.last_decoded.is_some_and(|seq| seq >= resumed));
            assert!(!receiver.push_slice_at(stranded, &payload, origin + Duration::from_secs(2), None));
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
        assert!(output[192..].iter().all(|sample| *sample > 0.19));
        assert_eq!(receiver.stats().lost, losses);
    }
    #[test]
    fn nonquiet_wake_backlog_drains_with_profile_rate_budget_without_trimming_media() {
        for config in [
            Config::low_latency_pcm(),
            Config::low_latency_opus(),
            Config::opus_10ms(),
            Config::music_opus(),
            Config::lossless(48_000),
        ] {
            let mut encoder = Encoder::new_with_config(config).unwrap();
            let mut receiver = Receiver::new_with_config(config).unwrap();
            receiver.reset_burst();
            let origin = Instant::now();
            let mut payload = [0; MAX_PACKET_BYTES];
            let tone: [f32; FRAME_SAMPLES * 2] =
                std::array::from_fn(|i| 0.1 * (std::f32::consts::TAU * 1200.0 * (i / 2) as f32 / 48_000.0).sin());
            let len = encoder
                .encode_into(&tone[..config.frame_samples()], &mut payload)
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
            let mut pcm = [0.0; FRAME_SAMPLES * 2];
            for tick in 1..=(60_000.0 / config.frame_duration_ms()) as u64 {
                let sequence = backlog_frames + tick;
                let at = origin + Duration::from_nanos(sequence * step_ns);
                assert!(receiver.push_slice_at(sequence, &payload[..len], at, Some(sequence * step_ns)));
                receiver.render_into(at, config.frame_size(), &mut pcm[..config.frame_samples()], None);
                let stats = receiver.stats();
                maximum_ppm = maximum_ppm.max(stats.adaptation_ppm);
                assert!(stats.adaptation_ppm >= -MAX_ADAPTATION_PPM - 1e-9);
                assert!(stats.adaptation_ppm <= f64::from(config.jitter.max_compression_ppm) + 1e-9);
                assert_eq!(stats.latency_trimmed_frames, 0);
                assert_eq!(stats.lost, 0);
            }
            let stats = receiver.stats();
            assert!(maximum_ppm >= f64::from(config.jitter.max_compression_ppm) - 1e-9);
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

#[cfg(test)]
mod quality_profile_tests {
    use super::*;
    #[test]
    fn finite_stream_drain_emits_the_complete_fir_tail() {
        let mut converter = DriftResampler::new(2);
        let mut input = [0.0; 128];
        input[126] = 1.0;
        converter.push(&input);
        assert!(converter.finish_tail(false));
        assert!(!converter.finish_tail(false));
        let actual = converter.render(256, 0.0);
        for (i, frame) in actual.as_chunks::<2>().0.iter().enumerate() {
            let expected = converter.kernel.sample(i as f64, |j| if j == 63 { 1.0 } else { 0.0 });
            assert!((frame[0] - expected).abs() < 1e-6, "tail frame {i}");
            assert_eq!(frame[1], 0.0);
        }
        assert_eq!(converter.buffered_frames(), 0);
    }
    #[test]
    fn native_pcm_clocks_keep_bounded_reserve_in_both_drift_directions() {
        for rate in [44_100, 96_000] {
            for ppm in [-700.0, 700.0] {
                let config = Config::lossless(rate);
                let mut receiver = Receiver::new_with_config(config).unwrap();
                let mut encoder = Encoder::new_with_config(config).unwrap();
                let packet = encoder.encode(&vec![0.1; config.frame_samples()]).unwrap();
                let origin = Instant::now();
                let period = config.frame_duration_ms() / 1000.0 / (1.0 + ppm / 1e6);
                let mut sequence = 0;
                for tick in 0..2000 {
                    let elapsed = tick as f64 * 120.0 / 48000.0;
                    let now = origin + Duration::from_secs_f64(elapsed);
                    while sequence as f64 * period <= elapsed {
                        assert!(receiver.push_with_source_time_at(
                            sequence,
                            packet.clone(),
                            origin + Duration::from_secs_f64(sequence as f64 * period),
                            (sequence as f64 * period * 1e9) as u64
                        ));
                        sequence += 1;
                    }
                    receiver.pull_into(now, 120, &mut [0.0; 240], &mut 8, None);
                    assert!(receiver.stats().buffered_ms < 20.0);
                }
                let stats = receiver.stats();
                assert_eq!(
                    stats.lost + stats.late + stats.rejected + stats.latency_trimmed_frames,
                    0,
                    "{rate} {ppm}: {stats:?}"
                );
                eprintln!(
                    "native PCM clock {rate} {ppm}ppm, 5 seconds: buffer {} ms, correction {}ppm",
                    stats.buffered_ms, stats.drift_ppm
                );
            }
        }
    }
    #[test]
    fn playback_conversion_and_drift_share_one_impulse_delay() {
        for (input, output) in [(44_100, 48_000), (48_000, 44_100), (48_000, 48_000), (96_000, 48_000)] {
            for ppm in [-700.0, 0.0, 700.0] {
                let mut converter = DriftResampler::new(2);
                converter.set_rates(input, output);
                let mut impulse = [0.0; 1024];
                impulse[0] = 1.0;
                converter.push(&impulse);
                let rendered = converter.render(256, ppm);
                let peak = (0..256)
                    .max_by(|a, b| rendered[*a * 2].abs().total_cmp(&rendered[*b * 2].abs()))
                    .unwrap();
                let expected =
                    converter.kernel.delay() as f64 * f64::from(output) / f64::from(input) / DriftResampler::step(ppm);
                assert!((peak as f64 - expected).abs() <= 0.5);
                assert!(rendered.as_chunks::<2>().0.iter().all(|pair| pair[1] == 0.0));
                eprintln!(
                    "playback {input}->{output}, {ppm}ppm: impulse peak {peak} output frames, {} ms",
                    peak as f64 * 1000.0 / f64::from(output)
                );
            }
        }
    }
    #[test]
    fn quality_profiles_encode_decode_and_report_real_parameters() {
        for config in [
            Config::low_latency_opus(),
            Config::music_opus(),
            Config::lossless(44_100),
            Config::lossless(48_000),
            Config::lossless(96_000),
        ] {
            let mut encoder = Encoder::new_with_config(config).unwrap();
            if let EncoderCodec::Opus(inner) = &mut encoder.inner {
                let application = if config.frame_duration == FrameDuration::Ms20 {
                    opus::Application::Audio
                } else {
                    opus::Application::LowDelay
                };
                assert_eq!(inner.get_application().unwrap(), application);
                assert_eq!(inner.get_bitrate().unwrap(), opus::Bitrate::Bits(320_000));
                assert!(!inner.get_vbr().unwrap());
                assert_eq!(inner.get_complexity().unwrap(), 10);
            }
            let mut receiver = Receiver::new_with_config(config).unwrap();
            let origin = Instant::now();
            let samples: Vec<_> = (0..config.frame_size())
                .flat_map(|i| {
                    let value = (std::f32::consts::TAU * 997.0 * i as f32 / config.sample_rate as f32).sin() * 0.1;
                    [value, -value]
                })
                .collect();
            for sequence in 0..8 {
                let payload = encoder.encode(&samples).unwrap();
                assert_eq!(payload.len(), config.payload_bytes());
                assert!(valid_payload(config, &payload));
                assert!(receiver.push_at(sequence, payload, origin));
            }
            let mut output = [0.0; 960];
            receiver.pull_into(origin + Duration::from_millis(100), 480, &mut output, &mut 8, None);
            assert!(output.iter().all(|value| value.is_finite()));
            assert!(output.iter().any(|value| value.abs() > 0.01));
            assert_eq!(receiver.stats().decode_errors, 0);
            eprintln!(
                "profile {:?}: rate {}, frame {:.6} ms, lookahead {} samples, payload {} bytes",
                config.codec,
                config.sample_rate,
                config.frame_duration_ms(),
                encoder.lookahead_samples(),
                config.payload_bytes()
            );
        }
    }
    #[test]
    fn pcm24_round_trip_preserves_all_twenty_four_bits() {
        let config = Config::lossless(44_100);
        let values = [-8_388_608_i32, -8_388_607, -1, 0, 1, 8_388_607];
        let input: Vec<_> = (0..config.frame_samples())
            .map(|i| values[i % values.len()] as f32 / 8_388_608.0)
            .collect();
        let payload = Encoder::new_with_config(config).unwrap().encode(&input).unwrap();
        let mut receiver = Receiver::new_with_config(config).unwrap();
        receiver.push_at(0, payload, Instant::now());
        receiver.next = Some(0);
        assert!(receiver.decode_next(Instant::now(), &mut 2, None));
        assert_eq!(receiver.resampler.samples.iter().copied().collect::<Vec<_>>(), input);
    }
}

#[cfg(test)]
mod pitch_budget_tests {
    use super::*;

    #[test]
    fn reserve_rate_is_bounded_and_profile_holds_are_explicit() {
        for ppm in [-30_000.0, -3000.0, 0.0, 3000.0, 30_000.0] {
            assert!((DriftResampler::step(ppm) - (1.0 + ppm.clamp(-3000.0, 6000.0) / 1e6)).abs() < 1e-12);
        }
        assert_eq!(DriftResampler::step(f64::NAN), 1.0);
        for cap in [2000, 5000] {
            for error in [-48_000.0, 48_000.0] {
                let mut adjustment = PlayoutAdjustment::default();
                adjustment.change_target(-error);
                for _ in 0..1000 {
                    let ppm = adjustment.update_duration(error, 160, 128, cap);
                    assert!((-MAX_ADAPTATION_PPM..=f64::from(cap)).contains(&ppm));
                }
                assert_eq!(
                    adjustment.ppm,
                    if error < 0.0 {
                        -MAX_ADAPTATION_PPM
                    } else {
                        f64::from(cap)
                    }
                );
            }
        }
        assert_eq!(Config::low_latency_pcm().jitter.max_compression_ppm, 5000);
        assert_eq!(Config::low_latency_opus().jitter.max_compression_ppm, 5000);
        assert_eq!(Config::music_opus().jitter.max_compression_ppm, 2000);
        assert_eq!(Config::low_latency_pcm().jitter.target_stable_secs, 3);
        assert_eq!(Config::low_latency_opus().jitter.target_stable_secs, 3);
        assert_eq!(Config::music_opus().jitter.target_stable_secs, 30);
        for &rate in crate::negotiation::SAMPLE_RATES {
            assert_eq!(Config::lossless(rate).jitter.target_stable_secs, 30);
            assert_eq!(Config::lossless(rate).jitter.max_compression_ppm, 2000);
        }
    }

    // Synthetic packet-level reconstruction of the *pattern* in the per-second
    // receiver log (not an exact packet replay, which that log cannot supply).
    // Recurring 100 ms episodes with 15/18/22 ms extra transit, separated by
    // 5/7/10 seconds; then 40 s
    // of clean traffic. Device callback is 128 frames, source clock is exact.
    #[test]
    fn recurring_wifi_excursions_keep_music_reserve_and_bound_pitch() {
        for config in [
            Config::low_latency_pcm(),
            Config::low_latency_opus(),
            Config::music_opus(),
            Config::lossless(48_000),
        ] {
            let origin = Instant::now();
            let packet = Encoder::new_with_config(config)
                .unwrap()
                .encode(&vec![0.05; config.frame_samples()])
                .unwrap();
            let stalls = [
                (5_000_000, 15_000),
                (12_000_000, 18_000),
                (22_000_000, 22_000),
                (27_000_000, 15_000),
                (34_000_000, 18_000),
                (44_000_000, 22_000),
                (49_000_000, 15_000),
                (56_000_000, 18_000),
            ];
            let mut events: Vec<_> = (0..100 * config.sample_rate as u64 / config.frame_size() as u64)
                .map(|seq| {
                    let source = seq * config.frame_size() as u64 * 1_000_000 / config.sample_rate as u64;
                    let arrival = stalls
                        .iter()
                        .find(|&&(start, _)| source >= start && source < start + 100_000)
                        .map_or(source, |&(_, length)| source + length)
                        + 3000;
                    (arrival, source, seq)
                })
                .collect();
            events.sort_unstable();
            let mut receiver = Receiver::new_with_config(config).unwrap();
            let mut index = 0;
            let mut output = [0.0; 256];
            let mut peak_ppm = 0.0_f64;
            let mut recurring_decreases = 0;
            let mut learned_loss = 0;
            let mut recurring_loss = 0;
            let mut recurring_rebuffers = 0;
            let mut peak_target = 0.0_f64;
            let mut recovery_hold_ms = 0.0;
            for tick in 0..37_500_u64 {
                let now_us = tick * 128 * 1_000_000 / 48_000;
                let now = origin + Duration::from_micros(now_us);
                let mut budget = 6;
                let was_recovering = receiver.recovering;
                let previous_holds = receiver.stats.startup_hold_frames;
                crate::allocation_audit::assert_no_heap(|| {
                    while index < events.len() && events[index].0 <= now_us {
                        let (arrival, source, sequence) = events[index];
                        receiver.push_slice_at(
                            sequence,
                            &packet,
                            origin + Duration::from_micros(arrival),
                            Some(source * 1000),
                        );
                        index += 1;
                    }
                    receiver.pull_into(now, 128, &mut output, &mut budget, None);
                });
                let stats = receiver.stats();
                if was_recovering && stats.startup_hold_frames > previous_holds {
                    recovery_hold_ms += 128.0 / 48.0;
                }
                peak_ppm = peak_ppm.max(stats.adaptation_ppm.abs());
                peak_target = peak_target.max(stats.current_target_ms);
                assert!(stats.adaptation_ppm >= -MAX_ADAPTATION_PPM - 1e-9);
                assert!(stats.adaptation_ppm <= f64::from(config.jitter.max_compression_ppm) + 1e-9);
                if now_us < 30_000_000 {
                    learned_loss = stats.lost;
                }
                if now_us < 60_000_000 {
                    recurring_decreases = stats.target_decreases;
                    recurring_loss = stats.lost;
                    recurring_rebuffers = stats.rebufferings;
                }
            }
            let stats = receiver.stats();
            eprintln!(
                "wifi profile={:?}/{} hold={} peak_target_ms={peak_target:.3} recurring_decreases={recurring_decreases} peak_ppm={peak_ppm:.3} learned_loss={learned_loss} recurring_loss={recurring_loss} recurring_rebuffers={recurring_rebuffers} recovery_hold_ms={recovery_hold_ms:.3} final_buffer_ms={:.3} stretched_ms={:.3} compressed_ms={:.3}",
                config.codec,
                config.frame_size(),
                config.jitter.target_stable_secs,
                stats.buffered_ms,
                stats.adaptation_stretched_ms,
                stats.adaptation_compressed_ms
            );
            if config.jitter.target_stable_secs == 30 {
                assert_eq!(recurring_decreases, 0);
                // This trace establishes reserve primarily by starvation and
                // re-prefill, not continuous slowing. Do not claim that the
                // small resampling contribution alone absorbs these stalls.
                assert_eq!(recurring_rebuffers, 1);
                assert!(recovery_hold_ms > 0.0);
                assert!(stats.adaptation_stretched_ms < (peak_target - stats.initial_target_ms) * 0.5);
                assert_eq!(recurring_loss, learned_loss, "no further PLC after reserve is learned");
            } else {
                assert!(recurring_decreases > 0, "Smooth still decays aggressively");
            }
            assert!(stats.target_decreases > 0, "music eventually releases the reserve too");
            assert_eq!(stats.current_target_ms, stats.min_target_ms);
            assert_eq!(stats.work_limited, 0);
        }
    }
}

#[cfg(test)]
mod quiet_recovery_tests {
    use super::*;

    #[test]
    fn quiet_splices_are_stereo_coherent_bounded_and_allocation_free_at_all_rates() {
        for &rate in crate::negotiation::SAMPLE_RATES {
            let guard = (rate as usize * 20).div_ceil(1000);
            let drop = rate as usize * 5 / 1000;
            let fade = (rate as usize).div_ceil(1000);
            for amplitude in [0.0, QUIET_PEAK * 0.9, 10.0_f32.powf(-50.0 / 20.0), 0.01, 0.05] {
                let samples: Vec<_> = (0..guard + drop)
                    .flat_map(|i| {
                        let phase = std::f32::consts::TAU * i as f32 / rate as f32;
                        let sample = amplitude
                            * (0.5 * (phase * 440.0).cos()
                                + 0.3 * (phase * 554.365).cos()
                                + 0.2 * (phase * 659.255).cos());
                        [sample, -sample]
                    })
                    .collect();
                let mut resampler = DriftResampler::new(CHANNELS);
                resampler.push(&samples);
                let mut did_drop = false;
                crate::allocation_audit::assert_no_heap(|| {
                    did_drop = resampler.discard_quiet_prefix(drop, guard, fade);
                });
                if amplitude <= QUIET_PEAK {
                    assert!(did_drop, "rate={rate}");
                    assert_eq!(resampler.buffered_frames(), guard);
                    assert_eq!(resampler.samples[0], samples[0]);
                    assert_eq!(resampler.samples[(fade - 1) * 2], samples[(fade - 1 + drop) * 2]);
                    assert!(
                        resampler
                            .samples
                            .iter()
                            .skip(fade * 2)
                            .eq(samples.iter().skip((drop + fade) * 2))
                    );
                    for i in 0..resampler.buffered_frames() {
                        assert_eq!(resampler.samples[i * 2], -resampler.samples[i * 2 + 1]);
                        if i > 0 {
                            assert!(
                                (resampler.samples[i * 2] - resampler.samples[(i - 1) * 2]).abs() <= QUIET_PEAK * 2.0
                            );
                        }
                    }
                } else {
                    assert!(!did_drop, "weak tails and music must survive at {rate}");
                    assert!(resampler.samples.iter().copied().eq(samples));
                }
            }
            // A transient in either channel disqualifies the whole window,
            // including an anti-phase signal which a mid-only gate would miss.
            let mut samples = vec![0.0; (guard + drop) * 2];
            samples[(guard - 1) * 2 + 1] = 0.01;
            let mut resampler = DriftResampler::new(CHANNELS);
            resampler.push(&samples);
            assert!(!resampler.discard_quiet_prefix(drop, guard, fade));
            assert!(resampler.samples.iter().copied().eq(samples));
        }
    }

    // Offline timing exercise: aged startup backlog and a deliberate target
    // reduction. Both use real receiver decode, attribution and shared budgets.
    fn recover(config: Config, extra_ms: usize, wake: bool, quiet: bool) -> (Stats, f64) {
        recover_material(config, extra_ms, wake, quiet, None)
    }
    fn recover_material(
        config: Config,
        extra_ms: usize,
        wake: bool,
        quiet: bool,
        material: Option<&[Vec<u8>]>,
    ) -> (Stats, f64) {
        let mut receiver = Receiver::new_with_config(config).unwrap();
        if wake {
            receiver.reset_burst();
        }
        let mut encoder = Encoder::new_with_config(config).unwrap();
        let lookahead = encoder.lookahead_samples();
        let packet = encoder
            .encode(&vec![if quiet { 0.0 } else { 0.05 }; config.frame_samples()])
            .unwrap();
        let origin = Instant::now();
        let backlog = ((extra_ms + config.frame_duration_ms().ceil() as usize * config.jitter.initial_frames) as f64
            / config.frame_duration_ms())
        .ceil() as u64;
        let mut sequence = 0;
        let source_origin = origin - Duration::from_secs_f64(backlog as f64 * config.frame_duration_ms() / 1000.0);
        let mut pcm = [0.0; 256];
        let mut recovery_ms = f64::INFINITY;
        let mut started = false;
        // A high target provides the finite prefill for the target-drop case.
        if !wake {
            receiver.adaptive.target += (extra_ms as f64 / config.frame_duration_ms()).ceil() as usize;
            receiver.config.jitter.adaptive = false;
        }
        for tick in 0..if quiet { 4500_u64 } else { 22_500 } {
            // At most 12/60 s, including one second of settled output.
            let elapsed_us = tick * 128 * 1_000_000 / 48_000;
            let at = origin + Duration::from_micros(elapsed_us);
            let mut budget = 6;
            let discarded_before = receiver.stats.quiet_discarded_ms;
            let source_cursor = receiver
                .spans
                .front()
                .map(|span| {
                    span.sequence as usize * config.frame_size() + config.frame_size().saturating_sub(span.frames)
                })
                .or_else(|| receiver.next.map(|seq| seq as usize * config.frame_size()));
            crate::allocation_audit::assert_no_heap(|| {
                while sequence
                    < backlog + elapsed_us * u64::from(config.sample_rate) / (config.frame_size() as u64 * 1_000_000)
                {
                    let source_ns =
                        sequence * config.frame_size() as u64 * 1_000_000_000 / u64::from(config.sample_rate);
                    receiver.push_slice_at(
                        sequence,
                        material.map_or(packet.as_slice(), |packets| {
                            packets[sequence as usize % packets.len()].as_slice()
                        }),
                        source_origin + Duration::from_nanos(source_ns),
                        Some(source_ns),
                    );
                    sequence += 1;
                }
                receiver.pull_into(at, 128, &mut pcm, &mut budget, None);
            });
            if material.is_some() && receiver.stats.quiet_discarded_ms > discarded_before {
                let start = source_cursor.expect("active source cursor");
                let removed = ((receiver.stats.quiet_discarded_ms - discarded_before) * 48.0).round() as usize;
                assert!(
                    (start..start + removed).all(|frame| frame.saturating_sub(lookahead) % 48_000 >= 28_800),
                    "speech cut: {start} + {removed}"
                );
            }
            if !started && receiver.next.is_some() {
                started = true;
                if !wake {
                    let delta = (receiver.adaptive.target - config.jitter.initial_frames) * config.frame_size();
                    receiver.adaptive.target = config.jitter.initial_frames;
                    receiver
                        .adjustment
                        .change_target(-(delta as f64) * 48_000.0 / f64::from(config.sample_rate));
                }
            }
            assert_eq!(
                receiver.spans.iter().map(|span| span.frames).sum::<usize>(),
                receiver.resampler.buffered_frames()
            );
            let stats = receiver.stats();
            assert_eq!(stats.quiet_inserted_ms, 0.0);
            if started
                && stats.buffered_ms
                    <= config.jitter.initial_frames as f64 * config.frame_duration_ms()
                        + config.frame_duration_ms()
                        + 3.0
            {
                recovery_ms = recovery_ms.min(elapsed_us as f64 / 1000.0);
            } else {
                // A trough-only crossing is not recovery: the whole following
                // second, including packet-arrival peaks, must stay in bounds.
                recovery_ms = f64::INFINITY;
            }
            if elapsed_us as f64 / 1000.0 > recovery_ms + 1000.0 {
                break;
            }
        }
        (receiver.stats(), recovery_ms)
    }

    #[test]
    fn quiet_backlog_recovers_in_seconds_without_discarding_music() {
        for config in [
            Config::low_latency_pcm(),
            Config::low_latency_opus(),
            Config::music_opus(),
            Config::lossless(48_000),
            Config::lossless(192_000),
        ] {
            for (extra, wake) in [(100, false), (80, true)] {
                let (quiet, recovery) = recover(config, extra, wake, true);
                eprintln!(
                    "quiet recovery {:?}/{} extra={extra} wake={wake} recovery_ms={recovery:.3} discarded_ms={:.3} resampled_ms={:.3} lost={} rebuffer={}",
                    config.codec,
                    config.sample_rate,
                    quiet.quiet_discarded_ms,
                    quiet.adaptation_compressed_ms,
                    quiet.lost,
                    quiet.rebufferings
                );
                assert!(recovery < 10_000.0, "{quiet:?}");
                assert!(quiet.quiet_discarded_ms >= extra as f64 - 30.0, "{quiet:?}");
                assert_eq!(quiet.lost, 0, "{quiet:?}");
                assert_eq!(quiet.work_limited, 0);
                assert_eq!(quiet.rebufferings, 0);
            }
        }
        // Same Quality/Lossless +0.2% cap as the pre-quiet implementation.
        // Non-quiet material is the pure-resampling timing reference.
        for (extra, wake) in [(100, false), (80, true)] {
            let (music, baseline_ms) = recover(Config::lossless(48_000), extra, wake, false);
            eprintln!("rate-only reference extra={extra} wake={wake} recovery_ms={baseline_ms:.3}");
            assert_eq!(music.quiet_discarded_ms, 0.0);
            assert_eq!(music.lost, 0);
            assert!(music.adaptation_compressed_ms > 0.0);
            assert!(baseline_ms > if wake { 30_000.0 } else { 40_000.0 });
            assert!(baseline_ms < 60_000.0);
        }
    }
    fn pink_noise(frames: usize) -> Vec<f32> {
        // Deterministic Voss octave-bank approximation, plus white high band.
        let mut seed = 0x12345678_u32;
        let mut rows = [0.0_f32; 16];
        let mut random = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32 * 2.0 - 1.0
        };
        for row in &mut rows {
            *row = random();
        }
        let mut noise: Vec<_> = (1..=frames)
            .map(|i| {
                rows[i.trailing_zeros().min(15) as usize] = random();
                rows.iter().sum::<f32>() + random()
            })
            .collect();
        let mean = noise.iter().sum::<f32>() / frames as f32;
        for sample in &mut noise {
            *sample -= mean;
        }
        let peak = noise.iter().fold(0.0_f32, |a, b| a.max(b.abs()));
        for sample in &mut noise {
            *sample *= 10.0_f32.powf(-55.0 / 20.0) / peak;
        }
        noise
    }

    #[test]
    fn speech_pauses_with_minus_55_dbfs_peak_pink_noise_recover_without_cutting_voice() {
        let noise = pink_noise(48_000);
        let rms = (noise.iter().map(|x| x * x).sum::<f32>() / noise.len() as f32).sqrt();
        eprintln!("pink noise peak_dbfs=-55 rms_dbfs={:.3}", 20.0 * rms.log10());
        for db in [-65.0_f32, -60.0, -55.0] {
            let threshold = 10.0_f32.powf(db / 20.0);
            let eligible = noise
                .as_chunks::<960>()
                .0
                .iter()
                .filter(|window| window.iter().all(|sample| sample.abs() <= threshold))
                .count();
            eprintln!("pink gate_dbfs={db} eligible_windows={eligible}/50");
        }
        let speech: Vec<_> = noise
            .iter()
            .enumerate()
            .flat_map(|(i, &n)| {
                let phase = std::f32::consts::TAU * 120.0 * i as f32 / 48_000.0;
                let voice = if i < 28_800 {
                    0.08 * (phase.sin() + 0.4 * (phase * 3.0).sin() + 0.2 * (phase * 7.0).sin())
                } else {
                    0.0
                };
                [voice + n, voice - n]
            })
            .collect();
        for config in [Config::low_latency_opus(), Config::lossless(48_000)] {
            let mut encoder = Encoder::new_with_config(config).unwrap();
            let packets: Vec<_> = speech
                .chunks_exact(config.frame_samples())
                .map(|frame| encoder.encode(frame).unwrap())
                .collect();
            for (extra, wake) in [(100, false), (80, true)] {
                let (stats, recovery) = recover_material(config, extra, wake, true, Some(&packets));
                eprintln!(
                    "speech {:?} extra={extra} wake={wake} recovery_ms={recovery:.3} discarded_ms={:.3} lost={}",
                    config.codec, stats.quiet_discarded_ms, stats.lost
                );
                assert!(stats.quiet_discarded_ms > 30.0, "{stats:?}");
                assert!(recovery < 10_000.0);
                assert_eq!(stats.lost, 0);
                assert_eq!(stats.work_limited, 0);
            }
        }
    }

    #[test]
    fn low_level_sine_splice_has_no_abrupt_edge_at_any_rate() {
        let amplitude = 10.0_f32.powf(-61.0 / 20.0);
        for &rate in crate::negotiation::SAMPLE_RATES {
            for frequency in [110.0_f32, 440.0, 997.0] {
                let guard = (rate as usize * 20).div_ceil(1000);
                let drop = rate as usize * 5 / 1000;
                let fade = (rate as usize).div_ceil(1000);
                let samples: Vec<_> = (0..guard + drop)
                    .flat_map(|i| {
                        let v = amplitude * (std::f32::consts::TAU * frequency * i as f32 / rate as f32).sin();
                        [v, -v]
                    })
                    .collect();
                let mut resampler = DriftResampler::new(2);
                resampler.push(&samples);
                assert!(resampler.discard_quiet_prefix(drop, guard, fade));
                let max_delta = (1..resampler.buffered_frames())
                    .map(|i| (resampler.samples[2 * i] - resampler.samples[2 * (i - 1)]).abs())
                    .fold(0.0_f32, f32::max);
                // Original sine slope plus the crossfade's bounded weight change.
                let bound = 2.0 * amplitude * (std::f32::consts::PI * frequency / rate as f32).sin().abs()
                    + 2.0 * amplitude / (fade - 1) as f32;
                assert!(
                    max_delta <= bound + 1e-8,
                    "rate={rate} hz={frequency} delta={max_delta} bound={bound}"
                );
                if rate == 48_000 {
                    eprintln!("sine rate={rate} hz={frequency} max_delta={max_delta:.9} bound={bound:.9}");
                }
            }
        }
    }
}
