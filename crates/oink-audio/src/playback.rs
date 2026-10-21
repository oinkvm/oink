//! Selected-device rendering with a small, sample-counted queue and optional
//! nonblocking diagnostic traces. The realtime callback performs no I/O.
use crate::{OutputDevice, handoff, realtime};
#[cfg(not(target_os = "macos"))]
use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use crate::macos_playback::{NativeOutputTiming, NativePlayback};
#[cfg(windows)]
use crate::windows_playback::{NativeOutputTiming, NativePlayback};

#[cfg(not(any(target_os = "macos", windows)))]
use unsupported::{NativeOutputTiming, NativePlayback};

#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported {
    use super::Instant;
    #[derive(Clone, Copy)]
    pub struct NativeOutputTiming {
        pub callback_at: Instant,
        pub playback_at: Option<Instant>,
        pub sample_rate: u32,
        pub channels: u16,
        pub padding_frames: u32,
        pub buffer_frames: u32,
    }
    #[derive(serde::Serialize)]
    pub struct NativeOutputInfo {
        pub sample_rate: u32,
        pub channels: u16,
    }
    pub struct NativePlayback;
    type RenderCallback = Box<dyn FnMut(&mut [f32], NativeOutputTiming) + Send>;
    impl NativePlayback {
        pub fn start(
            _: &str,
            _: Option<u32>,
            _: RenderCallback,
            _: Box<dyn FnMut(String) + Send>,
        ) -> Result<Self, String> {
            Err("native audio playback unsupported on this platform".into())
        }
        pub fn take_schedule(&mut self) -> Result<crate::realtime::Schedule, String> {
            Err("native output unsupported".into())
        }
        pub fn info(&self) -> NativeOutputInfo {
            NativeOutputInfo {
                sample_rate: 48_000,
                channels: 2,
            }
        }
    }
}

pub fn outputs() -> Result<Vec<OutputDevice>, String> {
    #[cfg(target_os = "macos")]
    {
        crate::macos_devices::outputs()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let host = cpal::default_host();
        let default = host
            .default_output_device()
            .and_then(|d| d.id().ok())
            .map(|id| id.to_string());
        let mut result = Vec::new();
        for d in host.output_devices().map_err(|e| e.to_string())? {
            let id = d.id().map_err(|e| e.to_string())?.to_string();
            result.push(OutputDevice {
                is_default: default.as_ref() == Some(&id),
                id,
                name: crate::device_name(&d.description().map_err(|e| e.to_string())?).to_string(),
            });
        }
        Ok(result)
    }
}

/// Query device-reported latency without starting playback or requesting capture.
/// Windows reports an initialized stream's latency separately in PlaybackInfo.
pub fn output_latencies(outputs: &[OutputDevice]) -> Vec<crate::OutputLatency> {
    outputs
        .iter()
        .take(64)
        .map(|output| crate::OutputLatency {
            output: output.id.clone(),
            // Latency is queried only when opening the selected endpoint and
            // reported through PlaybackInfo; enumeration never probes formats.
            reported_latency_ms: None,
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct PlaybackOptions {
    /// None explicitly selects the driver-default baseline. Some(0) requests
    /// the supported minimum; Some(n) requests n, with actual values reported.
    pub requested_buffer_frames: Option<u32>,
    /// Bounded callback traces; false keeps diagnostic timestamp work off.
    pub trace: bool,
}
impl Default for PlaybackOptions {
    fn default() -> Self {
        Self {
            requested_buffer_frames: Some(128),
            trace: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaybackInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub requested_buffer_frames: Option<u32>,
    pub backend: serde_json::Value,
    /// Backend-reported output latency, excluding network and application queues.
    /// Overlap with native presentation timestamps has not been calibrated;
    /// display this separately instead of blindly adding it to an E2E estimate.
    #[serde(default)]
    pub reported_output_latency_ms: Option<f64>,
}

/// A content-free native timing hook. The owner must return immediately; no
/// trace sequence, audio sample or endpoint metadata is passed to the observer.
struct PresentationObserver {
    enabled: fn() -> bool,
    record: fn(Instant, Instant),
}
static PRESENTATION_OBSERVER: std::sync::OnceLock<PresentationObserver> = std::sync::OnceLock::new();
pub fn set_presentation_observer(enabled: fn() -> bool, record: fn(Instant, Instant)) {
    let _ = PRESENTATION_OBSERVER.set(PresentationObserver { enabled, record });
}
#[derive(Clone, Copy, Debug)]
pub struct PlaybackTrace {
    pub sequence: Option<u64>,
    pub pushed_at: Instant,
    pub callback_at: Instant,
    /// Native first-frame presentation estimate plus this packet's offset in
    /// the output callback. This is not a microphone/DAC measurement.
    pub playback_at: Option<Instant>,
    pub output_frame_offset: u32,
    pub callback_frames: u32,
    pub sample_rate: u32,
    pub native_padding_frames: u32,
    pub native_buffer_frames: u32,
    /// Whole callback's queue handling, rate conversion and channel mapping CPU.
    /// Repeated for spans in the same callback; do not sum it per packet.
    pub render_ns: u64,
}

#[derive(Clone, Copy, Debug)]
struct QueuedFrame {
    samples: [f32; 4096],
    len: usize,
    sequence: Option<u64>,
    pushed_at: Instant,
}
impl QueuedFrame {
    fn empty() -> Self {
        Self {
            samples: [0.0; 4096],
            len: 0,
            sequence: None,
            pushed_at: Instant::now(),
        }
    }
    fn new(samples: &[f32], sequence: Option<u64>) -> Self {
        let mut frame = Self::empty();
        frame.len = samples.len();
        frame.samples[..samples.len()].copy_from_slice(samples);
        frame.sequence = sequence;
        frame
    }
}
fn empty_trace() -> PlaybackTrace {
    PlaybackTrace {
        sequence: None,
        pushed_at: Instant::now(),
        callback_at: Instant::now(),
        playback_at: None,
        output_frame_offset: 0,
        callback_frames: 0,
        sample_rate: 0,
        native_padding_frames: 0,
        native_buffer_frames: 0,
        render_ns: 0,
    }
}
/// Unique producer detached for the time-critical feeder. Its methods use only
/// fixed storage and atomics. Only the realtime sink owns this producer.
#[derive(Debug)]
pub struct Feed {
    tx: handoff::Producer<QueuedFrame>,
    queued_blocks: Arc<AtomicUsize>,
    queued_samples: Arc<AtomicUsize>,
    last_frame_samples: usize,
    callback_input_frames: Arc<AtomicUsize>,
}
impl Feed {
    pub fn needs_frame(&self) -> bool {
        self.frames_needed() > 0
    }
    pub fn frames_needed(&self) -> usize {
        let target = (self.callback_input_frames.load(Ordering::Relaxed) * 2).max(self.last_frame_samples);
        target
            .saturating_sub(self.queued_samples.load(Ordering::Acquire))
            .div_ceil(2)
    }
    pub fn push(&mut self, samples: &[f32], sequence: Option<u64>) -> bool {
        if samples.is_empty() || !samples.len().is_multiple_of(2) || samples.len() > 4096 {
            return false;
        }
        let count = samples.len();
        self.last_frame_samples = count;
        self.queued_blocks.fetch_add(1, Ordering::Release);
        self.queued_samples.fetch_add(count, Ordering::Release);
        if self.tx.push(QueuedFrame::new(samples, sequence)).is_err() {
            self.queued_blocks.fetch_sub(1, Ordering::AcqRel);
            self.queued_samples.fetch_sub(count, Ordering::AcqRel);
            return false;
        }
        true
    }
}

struct Span {
    remaining: usize,
    sequence: Option<u64>,
    pushed_at: Instant,
    traced: bool,
}

#[derive(Debug, Default)]
pub struct PlaybackMetrics {
    pub worker: Option<realtime::Metrics>,
    pub callbacks: u64,
    pub max_callback_gap_us: u64,
    pub max_render_us: u64,
    pub underrun_frames: u64,
    pub queued_frames: usize,
}
#[derive(Default)]
struct CallbackMetrics {
    zero_periods: AtomicUsize,
    quality: std::sync::OnceLock<Arc<crate::sink::DropoutCounters>>,
    quality_active: std::sync::atomic::AtomicBool,
    quality_generation: AtomicU64,
    callbacks: AtomicU64,
    max_gap_us: AtomicU64,
    max_render_us: AtomicU64,
}

pub struct Playback {
    metrics: Arc<CallbackMetrics>,
    _stream: NativePlayback,
    feed: Option<Feed>,
    wake: realtime::Signal,
    queued_samples: Arc<AtomicUsize>,
    error: Arc<Mutex<Option<String>>>,
    started: Instant,
    callback_ms: Arc<AtomicU64>,
    traces: Mutex<handoff::Consumer<PlaybackTrace>>,
    trace_dropped: Arc<AtomicU64>,
    underrun_frames: Arc<AtomicU64>,
    info: PlaybackInfo,
}
impl std::fmt::Debug for Playback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Playback")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}
impl Playback {
    pub fn start_with_options(id: &str, options: &PlaybackOptions) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        let raw_id = id
            .strip_prefix("coreaudio:")
            .ok_or("expected CoreAudio output device ID")?;
        #[cfg(windows)]
        let raw_id = id.strip_prefix("wasapi:").ok_or("expected WASAPI output device ID")?;
        #[cfg(not(any(target_os = "macos", windows)))]
        let raw_id = id;
        let (tx, rx) = handoff::channel(8, QueuedFrame::empty());
        let (trace_tx, traces) = handoff::channel(8192, empty_trace());
        let wake = realtime::Signal::new()?;
        let queued_blocks = Arc::new(AtomicUsize::new(0));
        let queued_samples = Arc::new(AtomicUsize::new(0));
        let callback_input_frames = Arc::new(AtomicUsize::new(
            options.requested_buffer_frames.unwrap_or(512).max(1) as usize
        ));
        let error = Arc::new(Mutex::new(None));
        let started = Instant::now();
        let callback_ms = Arc::new(AtomicU64::new(0));
        let trace_dropped = Arc::new(AtomicU64::new(0));
        let underrun_frames = Arc::new(AtomicU64::new(0));
        let metrics = Arc::new(CallbackMetrics::default());
        let mut render = Render {
            metrics: metrics.clone(),
            quality: Default::default(),
            quality_generation: 0,
            previous_callback: None,
            first_period: true,
            wake: wake.clone(),
            rx,
            queued_blocks: queued_blocks.clone(),
            queued_samples: queued_samples.clone(),
            callback_input_frames: callback_input_frames.clone(),
            pcm: crate::dsp::prepared_deque(8192, || 0.0),
            spans: crate::dsp::prepared_deque(16, || Span {
                remaining: 0,
                sequence: None,
                pushed_at: Instant::now(),
                traced: false,
            }),
            phase: 0.0,
            started,
            callback_ms: callback_ms.clone(),
            traces: options.trace.then_some(trace_tx),
            pending_traces: {
                let mut traces = vec![empty_trace(); 64];
                std::hint::black_box(&mut traces);
                traces.clear();
                traces
            },
            trace_dropped: trace_dropped.clone(),
            underrun_frames: underrun_frames.clone(),
        };
        let e = error.clone();
        let stream = NativePlayback::start(
            raw_id,
            options.requested_buffer_frames,
            Box::new(move |b, timing| render.fill(b, timing)),
            Box::new(move |error| *e.lock().unwrap_or_else(|e| e.into_inner()) = Some(error)),
        )?;
        let native = stream.info();
        let sample_rate = native.sample_rate;
        let channels = native.channels;
        let backend = serde_json::to_value(native).map_err(|e| e.to_string())?;
        let reported_output_latency_ms = backend
            .get("device_output_latency_frames")
            .and_then(|v| v.as_u64())
            .map(|device| {
                let stream = backend
                    .get("stream_output_latency_frames")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                (device + stream) as f64 * 1000.0 / sample_rate as f64
            })
            .or_else(|| backend.get("stream_latency_ms").and_then(|v| v.as_f64()));
        let info = PlaybackInfo {
            sample_rate,
            channels,
            requested_buffer_frames: options.requested_buffer_frames,
            backend,
            reported_output_latency_ms,
        };
        Ok(Self {
            metrics,
            _stream: stream,
            feed: Some(Feed {
                tx,
                queued_blocks,
                queued_samples: queued_samples.clone(),
                last_frame_samples: 240,
                callback_input_frames,
            }),
            queued_samples,
            wake,
            error,
            started,
            callback_ms,
            traces: Mutex::new(traces),
            trace_dropped,
            underrun_frames,
            info,
        })
    }
    pub fn info(&self) -> PlaybackInfo {
        self.info.clone()
    }
    /// Ordinary-thread setup only. The returned producer is the sole writer,
    /// and must be stopped/joined before dropping this playback device.
    pub fn take_realtime_feed(&mut self) -> Result<(Feed, realtime::Schedule, realtime::Signal), String> {
        let schedule = self._stream.take_schedule()?;
        let feed = self.feed.take().ok_or("audio feeder already taken")?;
        Ok((feed, schedule, self.wake.clone()))
    }
    pub fn take_traces(&self) -> Vec<PlaybackTrace> {
        let mut traces = self.traces.lock().unwrap_or_else(|e| e.into_inner());
        std::iter::from_fn(|| traces.pop()).collect()
    }
    pub fn trace_dropped(&self) -> u64 {
        self.trace_dropped.load(Ordering::Relaxed)
    }
    pub fn take_metrics(&self) -> PlaybackMetrics {
        PlaybackMetrics {
            worker: None,
            callbacks: self.metrics.callbacks.swap(0, Ordering::Relaxed),
            max_callback_gap_us: self.metrics.max_gap_us.swap(0, Ordering::Relaxed),
            max_render_us: self.metrics.max_render_us.swap(0, Ordering::Relaxed),
            underrun_frames: self.underrun_frames(),
            queued_frames: self.queued_frames(),
        }
    }
    pub fn set_quality_counters(&self, counters: Arc<crate::sink::DropoutCounters>) {
        let _ = self.metrics.quality.set(counters);
        self.metrics.quality_active.store(true, Ordering::Release);
    }
    pub fn cancel_idle_drain(&self) {
        self.metrics.quality_generation.fetch_add(1, Ordering::Release);
        self.metrics.quality_active.store(true, Ordering::Release);
    }
    pub fn begin_idle_drain(&self) {
        self.metrics.quality_generation.fetch_add(1, Ordering::Release);
        self.metrics.quality_active.store(false, Ordering::Release);
        self.metrics.zero_periods.store(0, Ordering::Release);
    }
    pub fn idle_drained(&self) -> bool {
        self.metrics.zero_periods.load(Ordering::Acquire) >= 2
    }
    pub fn underrun_frames(&self) -> u64 {
        self.underrun_frames.load(Ordering::Relaxed)
    }
    pub fn queued_frames(&self) -> usize {
        self.queued_samples.load(Ordering::Acquire) / 2
    }
    /// A lost HAL object alone does not authorize retry; require an srst event.
    pub fn device_invalidated(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self._stream.device_invalidated()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
    pub fn error(&self) -> Option<String> {
        let elapsed = self.started.elapsed();
        if elapsed > Duration::from_millis(500)
            && elapsed.as_millis() as u64 > self.callback_ms.load(Ordering::Relaxed) + 500
        {
            return Some("output callback stalled; restore source playback".into());
        }
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

struct Render {
    quality: crate::sink::quality::Interruptions,
    quality_generation: u64,
    metrics: Arc<CallbackMetrics>,
    previous_callback: Option<Instant>,
    first_period: bool,
    rx: handoff::Consumer<QueuedFrame>,
    wake: realtime::Signal,
    queued_blocks: Arc<AtomicUsize>,
    queued_samples: Arc<AtomicUsize>,
    callback_input_frames: Arc<AtomicUsize>,
    pcm: VecDeque<f32>,
    spans: VecDeque<Span>,
    phase: f64,
    started: Instant,
    callback_ms: Arc<AtomicU64>,
    traces: Option<handoff::Producer<PlaybackTrace>>,
    pending_traces: Vec<PlaybackTrace>,
    trace_dropped: Arc<AtomicU64>,
    underrun_frames: Arc<AtomicU64>,
}
impl Render {
    fn fill(&mut self, output: &mut [f32], timing: NativeOutputTiming) {
        let began = Instant::now();
        self.metrics.callbacks.fetch_add(1, Ordering::Relaxed);
        if let Some(previous) = self.previous_callback.replace(timing.callback_at) {
            self.metrics.max_gap_us.fetch_max(
                timing.callback_at.saturating_duration_since(previous).as_micros() as u64,
                Ordering::Relaxed,
            );
        }
        let channels = usize::from(timing.channels).max(1);
        let rate = timing.sample_rate.max(1);
        let output_frames = output.len() / channels;
        self.callback_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
        self.callback_input_frames
            .store((output_frames * 48000).div_ceil(rate as usize), Ordering::Relaxed);
        if std::mem::take(&mut self.first_period) {
            output.fill(0.0);
            self.wake.notify();
            return;
        }
        let step = 48000.0 / f64::from(rate);
        let counters = self
            .metrics
            .quality
            .get()
            .filter(|_| self.metrics.quality_active.load(Ordering::Acquire));
        let generation = self.metrics.quality_generation.load(Ordering::Acquire);
        if counters.is_none() || self.quality_generation != generation {
            self.quality.reset();
            self.quality_generation = generation;
        }
        let at_ms = (crate::diagnostics::monotonic_us(timing.callback_at).max(0) / 1000) as u64;
        let mut missing = 0u64;
        for (offset, frame) in output.chunks_mut(channels).enumerate() {
            // A single future interpolation sample is optional. Never wait for
            // or conceal a whole next packet simply to obtain that sample.
            while self.pcm.len() < 4 {
                let Some(packet) = self.rx.pop() else {
                    break;
                };
                self.queued_blocks.fetch_sub(1, Ordering::AcqRel);
                self.spans.push_back(Span {
                    remaining: packet.len / 2,
                    sequence: packet.sequence,
                    pushed_at: packet.pushed_at,
                    traced: false,
                });
                self.pcm.extend(&packet.samples[..packet.len]);
            }
            let mut stereo = [0.0; 2];
            let supplied = self.pcm.len() >= 2;
            if self.pcm.len() >= 2 {
                if let Some(span) = self.spans.front_mut()
                    && !span.traced
                {
                    span.traced = true;
                    if let Some(observer) = PRESENTATION_OBSERVER.get()
                        && (observer.enabled)()
                        && let Some(playback_at) = timing
                            .playback_at
                            .and_then(|p| p.checked_add(Duration::from_secs_f64(offset as f64 / f64::from(rate))))
                    {
                        (observer.record)(span.pushed_at, playback_at);
                    }
                    if self.traces.is_some() && self.pending_traces.len() < 64 {
                        self.pending_traces.push(PlaybackTrace {
                            sequence: span.sequence,
                            pushed_at: span.pushed_at,
                            callback_at: timing.callback_at,
                            playback_at: timing
                                .playback_at
                                .and_then(|p| p.checked_add(Duration::from_secs_f64(offset as f64 / f64::from(rate)))),
                            output_frame_offset: offset as u32,
                            callback_frames: output_frames as u32,
                            sample_rate: rate,
                            native_padding_frames: timing.padding_frames,
                            native_buffer_frames: timing.buffer_frames,
                            render_ns: 0,
                        });
                    } else if self.traces.is_some() {
                        self.trace_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                for (channel, value) in stereo.iter_mut().enumerate() {
                    let a = self.pcm[channel];
                    let b = self.pcm.get(channel + 2).copied().unwrap_or(a);
                    *value = a + (b - a) * self.phase as f32;
                }
                self.phase += step;
                while self.phase >= 1.0 && self.pcm.len() >= 2 {
                    self.pcm.pop_front();
                    self.pcm.pop_front();
                    self.queued_samples.fetch_sub(2, Ordering::AcqRel);
                    if let Some(span) = self.spans.front_mut() {
                        span.remaining -= 1;
                        if span.remaining == 0 {
                            self.spans.pop_front();
                        }
                    }
                    self.phase -= 1.0;
                    // At low device rates one output consumes several input
                    // samples. Refill at packet boundaries without resetting
                    // the fractional phase or leaving it larger than one.
                    if self.pcm.is_empty()
                        && self.phase >= 1.0
                        && let Some(packet) = self.rx.pop()
                    {
                        self.queued_blocks.fetch_sub(1, Ordering::AcqRel);
                        self.spans.push_back(Span {
                            remaining: packet.len / 2,
                            sequence: packet.sequence,
                            pushed_at: packet.pushed_at,
                            traced: false,
                        });
                        self.pcm.extend(&packet.samples[..packet.len]);
                    }
                }
                if self.pcm.is_empty() && self.phase >= 1.0 {
                    self.phase = 0.0;
                }
            } else {
                self.phase = 0.0;
                self.underrun_frames.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(counters) = counters {
                let kind = if supplied {
                    crate::sink::quality::OutputKind::Audio
                } else {
                    crate::sink::quality::OutputKind::Missing
                };
                missing += u64::from(!supplied && self.quality.established());
                // Channel mapping can remove stereo discontinuities on a mono device.
                let audible = if channels == 1 {
                    [(stereo[0] + stereo[1]) * 0.5; 2]
                } else {
                    stereo
                };
                self.quality.frame(at_ms, rate, kind, audible, counters);
            }
            for (channel, value) in frame.iter_mut().enumerate() {
                *value = if channels == 1 {
                    (stereo[0] + stereo[1]) * 0.5
                } else if channel < 2 {
                    stereo[channel]
                } else {
                    0.0
                };
            }
        }
        if let Some(counters) = counters {
            counters.record_quality(crate::sink::DropoutBucket {
                at_ms,
                missing_ns: missing * 1_000_000_000 / u64::from(rate),
                ..Default::default()
            });
        }
        if output.iter().all(|sample| *sample == 0.0) {
            self.metrics.zero_periods.fetch_add(1, Ordering::Release);
        } else {
            self.metrics.zero_periods.store(0, Ordering::Release);
        }
        self.metrics
            .max_render_us
            .fetch_max(began.elapsed().as_micros() as u64, Ordering::Relaxed);
        if let Some(tx) = &mut self.traces {
            let render_ns = began.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            for mut trace in self.pending_traces.drain(..) {
                trace.render_ns = render_ns;
                if tx.push(trace).is_err() {
                    self.trace_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.wake.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn renderer() -> (handoff::Producer<QueuedFrame>, Render, handoff::Consumer<PlaybackTrace>) {
        let (tx, rx) = handoff::channel(8, QueuedFrame::empty());
        let (traces, trace_rx) = handoff::channel(128, empty_trace());
        let render = Render {
            metrics: Arc::new(CallbackMetrics::default()),
            quality: Default::default(),
            quality_generation: 0,
            previous_callback: None,
            first_period: false,
            wake: realtime::Signal::new().unwrap(),
            rx,
            queued_blocks: Arc::new(AtomicUsize::new(0)),
            queued_samples: Arc::new(AtomicUsize::new(0)),
            callback_input_frames: Arc::new(AtomicUsize::new(0)),
            pcm: crate::dsp::prepared_deque(8192, || 0.0),
            spans: crate::dsp::prepared_deque(16, || Span {
                remaining: 0,
                sequence: None,
                pushed_at: Instant::now(),
                traced: false,
            }),
            phase: 0.0,
            started: Instant::now(),
            callback_ms: Arc::new(AtomicU64::new(0)),
            traces: Some(traces),
            pending_traces: {
                let mut traces = vec![empty_trace(); 64];
                std::hint::black_box(&mut traces);
                traces.clear();
                traces
            },
            trace_dropped: Arc::new(AtomicU64::new(0)),
            underrun_frames: Arc::new(AtomicU64::new(0)),
        };
        (tx, render, trace_rx)
    }
    #[test]
    fn native_underrun_is_output_wide_and_uses_device_rate_but_idle_drain_is_excluded() {
        let (mut tx, mut render, _) = renderer();
        let counters = Arc::new(crate::sink::DropoutCounters::default());
        render.metrics.quality.set(counters.clone()).unwrap();
        render.metrics.quality_active.store(true, Ordering::Release);
        // 44.1 kHz device: 441 missing frames is 10 ms, regardless of input rate.
        let timing = timing(44100);
        let at = (crate::diagnostics::monotonic_us(timing.callback_at).max(0) / 1000) as u64;
        send(&mut tx, &render, vec![0.0; 960], None);
        render.fill(&mut [0.0; 882], timing);
        render.fill(&mut [0.0; 880], timing);
        assert_eq!(
            counters
                .snapshot(at)
                .into_iter()
                .flatten()
                .map(|b| b.interruptions)
                .sum::<u64>(),
            0
        );
        render.fill(&mut [0.0; 4], timing);
        assert_eq!(
            counters
                .snapshot(at)
                .into_iter()
                .flatten()
                .map(|b| b.interruptions)
                .sum::<u64>(),
            1
        );
        render.metrics.quality_active.store(false, Ordering::Release);
        let before = counters.snapshot(at);
        render.fill(&mut [0.0; 882], timing);
        assert_eq!(counters.snapshot(at), before);
    }
    fn send(tx: &mut handoff::Producer<QueuedFrame>, render: &Render, samples: Vec<f32>, sequence: Option<u64>) {
        render.queued_blocks.fetch_add(1, Ordering::Relaxed);
        render.queued_samples.fetch_add(samples.len(), Ordering::Relaxed);
        tx.push(QueuedFrame::new(&samples, sequence)).unwrap();
    }
    fn timing(rate: u32) -> NativeOutputTiming {
        NativeOutputTiming {
            callback_at: Instant::now(),
            playback_at: Some(Instant::now()),
            sample_rate: rate,
            channels: 2,
            padding_frames: 0,
            buffer_frames: 128,
        }
    }
    #[test]
    fn first_period_is_zero_and_onset_stays_queued_then_drain_requires_zero_periods() {
        let (mut tx, mut render, _) = renderer();
        render.first_period = true;
        send(&mut tx, &render, vec![0.25; 240], Some(1));
        let mut output = [1.0; 240];
        render.fill(&mut output, timing(48000));
        assert_eq!(output, [0.0; 240]);
        assert_eq!(render.queued_samples.load(Ordering::Acquire), 240);
        render.fill(&mut output, timing(48000));
        assert_eq!(output, [0.25; 240]);
        assert_eq!(render.metrics.zero_periods.load(Ordering::Acquire), 0);
        for expected in 1..=2 {
            render.fill(&mut output, timing(48000));
            assert_eq!(output, [0.0; 240]);
            assert_eq!(render.metrics.zero_periods.load(Ordering::Acquire), expected);
        }
    }
    #[test]
    fn feed_and_callback_do_no_heap_work_at_native_rates_or_on_overflow() {
        for rate in [8_000, 44_100, 48_000, 96_000, 192_000] {
            let (tx, mut render, mut traces) = renderer();
            render
                .metrics
                .quality
                .set(Arc::new(crate::sink::DropoutCounters::default()))
                .unwrap();
            render.metrics.quality_active.store(true, Ordering::Release);
            let mut feed = Feed {
                tx,
                queued_blocks: render.queued_blocks.clone(),
                queued_samples: render.queued_samples.clone(),
                last_frame_samples: 240,
                callback_input_frames: render.callback_input_frames.clone(),
            };
            let mut output = [0.0; 1024];
            let input = [0.01; 240];
            crate::allocation_audit::assert_no_heap(|| {
                for sequence in 0..2000 {
                    for _ in 0..12 {
                        feed.push(&input, Some(sequence));
                    }
                    render.fill(&mut output, timing(rate));
                    if sequence % 32 == 0 {
                        while traces.pop().is_some() {}
                    }
                }
            });
            assert!(output.iter().all(|s| s.is_finite()));
            assert!(feed.queued_blocks.load(Ordering::Acquire) <= 8);
        }
    }
    #[test]
    fn low_rate_output_keeps_phase_across_queue_boundaries() {
        for rate in [8000, 11025, 22050, 44100, 48000, 96000] {
            let (mut tx, mut render, _) = renderer();
            for chunk in 0..2 {
                send(
                    &mut tx,
                    &render,
                    (0..960).map(|i| ((chunk * 960 + i) / 2) as f32 / 1000.0).collect(),
                    Some(chunk),
                );
            }
            let mut rendered = Vec::new();
            for _ in 0..5 {
                let mut part = vec![0.0; 34];
                render.fill(&mut part, timing(rate));
                rendered.extend(part);
            }
            for (i, frame) in rendered.as_chunks::<2>().0.iter().enumerate() {
                let expected = (i as f64 * 48000.0 / f64::from(rate) / 1000.0) as f32;
                assert!(
                    (frame[0] - expected).abs() < 1e-5,
                    "rate={rate} frame={i}: {} vs {expected}",
                    frame[0]
                );
                assert_eq!(frame[0], frame[1]);
            }
        }
    }
    #[test]
    fn exact_rate_final_sample_does_not_require_future_packet() {
        let (mut tx, mut render, mut traces) = renderer();
        send(&mut tx, &render, vec![0.25; 240], Some(42));
        let mut output = vec![0.0; 240];
        render.fill(&mut output, timing(48000));
        assert!(output.iter().all(|s| *s == 0.25));
        assert_eq!(render.queued_samples.load(Ordering::Relaxed), 0);
        assert_eq!(render.underrun_frames.load(Ordering::Relaxed), 0);
        let trace = traces.pop().unwrap();
        assert_eq!(trace.sequence, Some(42));
        assert_eq!(trace.output_frame_offset, 0);
    }
    #[test]
    fn trace_marks_first_played_sample_after_previous_packet() {
        let (mut tx, mut render, mut traces) = renderer();
        send(&mut tx, &render, vec![0.1; 240], Some(1));
        send(&mut tx, &render, vec![0.2; 240], Some(2));
        let mut output = vec![0.0; 256];
        render.fill(&mut output, timing(48000));
        let records: Vec<_> = std::iter::from_fn(|| traces.pop()).collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].output_frame_offset, 120);
        assert_eq!(render.queued_samples.load(Ordering::Relaxed), 224);
    }
}
