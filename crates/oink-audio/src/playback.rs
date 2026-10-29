//! Selected-device pull rendering and bounded, nonblocking diagnostics.
//! The realtime callback performs no logging or diagnostic I/O.
use crate::{OutputDevice, handoff, realtime};
#[cfg(not(target_os = "macos"))]
use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering},
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
        pub period_frames: u32,
        pub native_gap_frames: Option<u64>,
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
    type RenderCallback = Box<dyn super::NativeRender>;
    impl NativePlayback {
        pub fn start(
            _: &str,
            _: Option<u32>,
            _: RenderCallback,
            _: Box<dyn FnMut(String) + Send>,
        ) -> Result<Self, String> {
            Err("native audio playback unsupported on this platform".into())
        }
        pub fn shutdown(&mut self) -> bool {
            true
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

pub(crate) const MIX_SOURCES: usize = 16;
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OutputGap {
    pub sources: [u64; MIX_SOURCES],
    pub delta: crate::sink::DropoutBucket,
}

pub(crate) trait NativeRender: Send {
    fn fill(&mut self, output: &mut [f32], timing: NativeOutputTiming);
    /// Entire adapter work, including native format copy/release.
    fn complete(&mut self, elapsed: Duration);
}

/// One callback owns the entire pull transaction. Admission and DSP state are
/// frozen at begin; end publishes observations and retirement acknowledgements.
pub(crate) trait PullInput: Send {
    fn begin(&mut self, frames: usize, now: Instant);
    fn pull(&mut self, output: &mut [f32], demand: usize, now: Instant) -> PullBlock;
    fn end(&mut self);
    fn gap(&mut self, gap: OutputGap);
    fn drained(&self) -> bool;
    fn work(&self) -> CallbackWork {
        CallbackWork::default()
    }
}

/// Counts for this callback only; no source identity or audio payload. Counts
/// describe bounded work, not CPU time or an OS scheduling guarantee.
#[derive(Clone, Copy, Debug, Default)]
pub struct CallbackWork {
    pub sources: u64,
    pub scan_steps: u64,
    pub codec_calls: u64,
    pub plc_frames: u64,
    pub decode_errors: u64,
    pub work_limited: u64,
}

/// One exceptional native callback, copied to a fixed SPSC queue. All timings
/// are elapsed wall time (including preemption), never mislabeled CPU time.
/// A native gap belongs to the timeline before this callback, so consecutive
/// records retain sequence/time rather than claiming the gap was caused here.
#[derive(Clone, Copy, Debug, Default)]
pub struct CallbackIncident {
    pub sequence: u64,
    pub at_us: i64,
    pub frames: u32,
    pub rate: u32,
    pub budget_ns: u64,
    pub elapsed_ns: u64,
    pub gap_us: u64,
    pub native_gap_frames: Option<u64>,
    pub work: CallbackWork,
    pub missing_frames: u64,
    pub fault: bool,
    // Cumulative offsets from native callback entry: ingress complete, pull
    // complete, output mapping complete, publication/observer complete. The
    // remaining elapsed time is native copy/release. Fixed four clock reads.
    pub phase_ns: [u64; 4],
}
impl CallbackIncident {
    pub fn phase_us(&self) -> [u64; 5] {
        let mut previous = 0;
        std::array::from_fn(|i| {
            let end = if i == 4 { self.elapsed_ns } else { self.phase_ns[i] };
            let value = end.saturating_sub(previous) / 1000;
            previous = end;
            value
        })
    }
    fn exceptional(&self) -> bool {
        (self.budget_ns != 0 && self.elapsed_ns > self.budget_ns)
            || self.native_gap_frames.is_some_and(|gap| gap != 0)
            || self.missing_frames != 0
            || self.fault
    }
}
const CALLBACK_INCIDENT_CAPACITY: usize = 16;
#[derive(Clone, Copy)]
pub(crate) struct PullBlock {
    pub sources: [u64; MIX_SOURCES],
    // Source PLC remains source quality; Missing here denotes a local fault.
    pub kind: crate::sink::quality::OutputKind,
}
#[derive(Debug, Default)]
pub struct PlaybackMetrics {
    pub worker: Option<realtime::Metrics>,
    pub callbacks: u64,
    pub output_deadline_misses: u64,
    pub output_worst_shortfall_us: u64,
    pub worker_deadline_misses: Option<u64>,
    pub worker_worst_lateness_us: Option<u64>,
    pub callback_over_budget: u64,
    pub callback_min_margin_us: Option<i64>,
    pub native_gap_frames: Option<u64>,
    pub attribution_dropped: u64,
    pub max_callback_gap_us: u64,
    pub max_render_us: u64,
    pub underrun_frames: u64,
    pub queued_frames: usize,
    pub callback_incidents: Vec<CallbackIncident>,
    pub callback_incidents_dropped: u64,
}
#[derive(Default)]
struct CallbackMetrics {
    deadline_misses: AtomicU64,
    worst_shortfall_us: AtomicU64,
    over_budget: AtomicU64,
    min_margin_us: AtomicI64,
    margin_known: std::sync::atomic::AtomicBool,
    native_gap_frames: AtomicU64,
    native_timing_known: std::sync::atomic::AtomicBool,
    fault: std::sync::atomic::AtomicBool,
    fade_out: std::sync::atomic::AtomicBool,
    fade_silent_at_ns: AtomicU64,
    drain_silent_at_ns: AtomicU64,
    zero_periods: AtomicUsize,
    quality: std::sync::OnceLock<Arc<crate::sink::DropoutCounters>>,
    quality_active: std::sync::atomic::AtomicBool,
    quality_generation: AtomicU64,
    callbacks: AtomicU64,
    max_gap_us: AtomicU64,
    max_render_us: AtomicU64,
    // Only the ordinary metrics reader locks this consumer. The callback owns
    // its producer directly and never accesses the mutex or allocates a Vec.
    incidents: std::sync::OnceLock<Mutex<handoff::Consumer<CallbackIncident>>>,
    incidents_dropped: AtomicU64,
}
impl CallbackMetrics {
    fn take_incidents(&self) -> Vec<CallbackIncident> {
        let Some(consumer) = self.incidents.get() else {
            return Vec::new();
        };
        let mut consumer = consumer.lock().unwrap_or_else(|e| e.into_inner());
        // Bound draining as well: a continuously failing producer cannot keep
        // this ordinary owner in an unbounded consume-until-empty loop.
        (0..CALLBACK_INCIDENT_CAPACITY).map_while(|_| consumer.pop()).collect()
    }
}
/// A finite tail only: normal playback keeps its existing samples and reserve.
#[derive(Default)]
pub(crate) struct FadeOut {
    last: [f32; 2],
    remaining: Option<usize>,
    frames: usize,
}
impl FadeOut {
    pub(crate) fn start(&mut self, rate: u32) {
        if self.remaining.is_none() {
            self.frames = (rate as usize * 15).div_ceil(1000).max(1);
            self.remaining = Some(self.frames);
        }
    }
    pub(crate) fn fading(&self) -> bool {
        self.remaining.is_some_and(|frames| frames > 0)
    }
    pub(crate) fn finished(&self) -> bool {
        self.remaining == Some(0)
    }
    pub(crate) fn apply(&mut self, stereo: &mut [f32; 2], supplied: bool) {
        if let Some(remaining) = &mut self.remaining {
            if supplied {
                self.last = *stereo;
            }
            // If the finite buffer runs out, continue its last stereo frame;
            // replacing it with zero here would cut the ramp partway through.
            *remaining = remaining.saturating_sub(1);
            let gain = *remaining as f32 / self.frames as f32;
            *stereo = self.last.map(|sample| sample * gain);
        } else {
            self.last = *stereo;
        }
    }
}

pub struct Playback {
    metrics: Arc<CallbackMetrics>,
    stream: NativePlayback,
    error: Arc<Mutex<Option<String>>>,
    started: Instant,
    callback_ms: Arc<AtomicU64>,
    traces: Mutex<handoff::Consumer<PlaybackTrace>>,
    trace_dropped: Arc<AtomicU64>,
    underrun_frames: Arc<AtomicU64>,
    info: PlaybackInfo,
    stopped: bool,
    quiescent: bool,
}
impl std::fmt::Debug for Playback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PullPlayback")
            .field("info", &self.info)
            .finish_non_exhaustive()
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
impl Playback {
    pub(crate) fn start_with_options(
        id: &str,
        options: &PlaybackOptions,
        input: Box<dyn PullInput>,
    ) -> Result<Self, String> {
        Self::start_adapted(id, options, input, |render| render)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn start_silent_test(
        id: &str,
        options: &PlaybackOptions,
        input: Box<dyn PullInput>,
    ) -> Result<Self, String> {
        struct Silent(Box<dyn NativeRender>);
        impl NativeRender for Silent {
            fn fill(&mut self, output: &mut [f32], timing: NativeOutputTiming) {
                self.0.fill(output, timing);
                output.fill(0.0);
            }
            fn complete(&mut self, elapsed: Duration) {
                self.0.complete(elapsed);
            }
        }
        Self::start_adapted(id, options, input, |render| Box::new(Silent(render)))
    }
    fn start_adapted(
        id: &str,
        options: &PlaybackOptions,
        input: Box<dyn PullInput>,
        adapt: impl FnOnce(Box<dyn NativeRender>) -> Box<dyn NativeRender>,
    ) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        let raw_id = id
            .strip_prefix("coreaudio:")
            .ok_or("expected CoreAudio output device ID")?;
        #[cfg(windows)]
        let raw_id = id.strip_prefix("wasapi:").ok_or("expected WASAPI output device ID")?;
        #[cfg(not(any(target_os = "macos", windows)))]
        let raw_id = id;

        let (trace_tx, traces) = handoff::channel(8192, empty_trace());
        let error = Arc::new(Mutex::new(None));
        let started = Instant::now();
        let callback_ms = Arc::new(AtomicU64::new(0));
        let trace_dropped = Arc::new(AtomicU64::new(0));
        let underrun_frames = Arc::new(AtomicU64::new(0));
        let metrics = Arc::new(CallbackMetrics::default());
        let render = Render::new(
            input,
            metrics.clone(),
            started,
            callback_ms.clone(),
            options.trace.then_some(trace_tx),
            trace_dropped.clone(),
            underrun_frames.clone(),
        );
        let e = error.clone();
        let stream = NativePlayback::start(
            raw_id,
            options.requested_buffer_frames,
            adapt(Box::new(render)),
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
            stream,
            error,
            started,
            callback_ms,
            traces: Mutex::new(traces),
            trace_dropped,
            underrun_frames,
            info,
            stopped: false,
            quiescent: false,
        })
    }
    pub fn info(&self) -> PlaybackInfo {
        self.info.clone()
    }
    pub fn take_traces(&self) -> Vec<PlaybackTrace> {
        let mut traces = self.traces.lock().unwrap_or_else(|e| e.into_inner());
        std::iter::from_fn(|| traces.pop()).collect()
    }
    pub fn trace_dropped(&self) -> u64 {
        self.trace_dropped.load(Ordering::Relaxed)
    }
    pub fn take_metrics(&self) -> PlaybackMetrics {
        let known = self.metrics.margin_known.swap(false, Ordering::Relaxed);
        let margin = self.metrics.min_margin_us.swap(i64::MAX, Ordering::Relaxed);
        PlaybackMetrics {
            worker: None,
            callbacks: self.metrics.callbacks.swap(0, Ordering::Relaxed),
            output_deadline_misses: self.metrics.deadline_misses.swap(0, Ordering::Relaxed),
            output_worst_shortfall_us: self.metrics.worst_shortfall_us.swap(0, Ordering::Relaxed),
            callback_over_budget: self.metrics.over_budget.swap(0, Ordering::Relaxed),
            callback_min_margin_us: known.then_some(margin),
            native_gap_frames: self
                .metrics
                .native_timing_known
                .swap(false, Ordering::Relaxed)
                .then(|| self.metrics.native_gap_frames.swap(0, Ordering::Relaxed)),
            max_callback_gap_us: self.metrics.max_gap_us.swap(0, Ordering::Relaxed),
            max_render_us: self.metrics.max_render_us.swap(0, Ordering::Relaxed),
            underrun_frames: self.underrun_frames(),
            callback_incidents: self.metrics.take_incidents(),
            callback_incidents_dropped: self.metrics.incidents_dropped.swap(0, Ordering::Relaxed),
            ..Default::default()
        }
    }
    pub fn set_quality_counters(&self, counters: Arc<crate::sink::DropoutCounters>) {
        let _ = self.metrics.quality.set(counters);
        self.metrics.quality_active.store(true, Ordering::Release);
    }
    pub fn cancel_idle_drain(&self) {
        self.metrics.quality_generation.fetch_add(1, Ordering::Release);
        self.metrics.quality_active.store(true, Ordering::Release);
        self.metrics.drain_silent_at_ns.store(0, Ordering::Release);
    }
    pub fn begin_idle_drain(&self) {
        self.metrics.quality_generation.fetch_add(1, Ordering::Release);
        self.metrics.quality_active.store(false, Ordering::Release);
        self.metrics.drain_silent_at_ns.store(0, Ordering::Release);
    }
    pub fn idle_drained(&self) -> bool {
        let at = self.metrics.drain_silent_at_ns.load(Ordering::Acquire);
        at != 0 && self.started.elapsed().as_nanos() >= u128::from(at)
    }
    pub fn underrun_frames(&self) -> u64 {
        self.underrun_frames.load(Ordering::Relaxed)
    }
    pub fn queued_frames(&self) -> usize {
        0
    }
    pub fn device_invalidated(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.stream.device_invalidated()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
    pub fn error(&self) -> Option<String> {
        if self.metrics.fault.load(Ordering::Acquire) {
            return Some("output pull exceeded bounded format/work limits".into());
        }
        let elapsed = self.started.elapsed();
        if elapsed > Duration::from_millis(500)
            && elapsed.as_millis() as u64 > self.callback_ms.load(Ordering::Relaxed) + 500
        {
            return Some("output callback stalled; restore source playback".into());
        }
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// Fade, stop native callbacks, then prove quiescence before DSP reclamation.
    pub(crate) fn shutdown(&mut self) -> bool {
        if self.stopped {
            return self.quiescent;
        }
        self.metrics.quality_active.store(false, Ordering::Release);
        self.metrics.fade_out.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            let at = self.metrics.fade_silent_at_ns.load(Ordering::Acquire);
            if at != 0 && self.started.elapsed().as_nanos() >= u128::from(at) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.quiescent = self.stream.shutdown();
        self.stopped = true;
        self.quiescent
    }
}
impl Drop for Playback {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// Same-thread scratch covers only this request. No mixed PCM survives except
// the single interpolation boundary frame. Oversized requests fault to silence.
const MAX_PULL_FRAMES: usize = 16_384;
pub(crate) struct Render {
    input: Box<dyn PullInput>,
    scratch: Box<[f32]>,
    kinds: Box<[crate::sink::quality::OutputKind]>,
    carry: Option<([f32; 2], crate::sink::quality::OutputKind)>,
    phase: f64,
    last_sources: [u64; MIX_SOURCES],
    splice: crate::sink::quality::SpliceRamp,
    fade: FadeOut,
    quality: crate::sink::quality::Interruptions,
    quality_generation: u64,
    metrics: Arc<CallbackMetrics>,
    previous_callback: Option<Instant>,
    started: Instant,
    callback_ms: Arc<AtomicU64>,
    traces: Option<handoff::Producer<PlaybackTrace>>,
    trace_dropped: Arc<AtomicU64>,
    underrun_frames: Arc<AtomicU64>,
    pending_trace: Option<PlaybackTrace>,
    pending_period_ns: u64,
    pending_at_ms: u64,
    pending_native_gap: Option<u64>,
    incidents: handoff::Producer<CallbackIncident>,
    pending_incident: CallbackIncident,
    callback_sequence: u64,
}
impl Render {
    fn new(
        input: Box<dyn PullInput>,
        metrics: Arc<CallbackMetrics>,
        started: Instant,
        callback_ms: Arc<AtomicU64>,
        traces: Option<handoff::Producer<PlaybackTrace>>,
        trace_dropped: Arc<AtomicU64>,
        underrun_frames: Arc<AtomicU64>,
    ) -> Self {
        let mut scratch = vec![0.0; (MAX_PULL_FRAMES + 1) * 2].into_boxed_slice();
        let mut kinds = vec![crate::sink::quality::OutputKind::Intentional; MAX_PULL_FRAMES + 1].into_boxed_slice();
        std::hint::black_box(&mut scratch);
        std::hint::black_box(&mut kinds);
        let _ = crate::diagnostics::monotonic_us(started);
        metrics.min_margin_us.store(i64::MAX, Ordering::Relaxed);
        let (incidents, consumer) = handoff::channel(CALLBACK_INCIDENT_CAPACITY, CallbackIncident::default());
        let _ = metrics.incidents.set(Mutex::new(consumer));
        Self {
            input,
            scratch,
            kinds,
            carry: None,
            phase: 0.0,
            last_sources: [0; MIX_SOURCES],
            splice: Default::default(),
            fade: Default::default(),
            quality: Default::default(),
            quality_generation: 0,
            metrics,
            previous_callback: None,
            started,
            callback_ms,
            traces,
            trace_dropped,
            underrun_frames,
            pending_trace: None,
            pending_period_ns: 0,
            pending_at_ms: 0,
            pending_native_gap: None,
            incidents,
            pending_incident: CallbackIncident::default(),
            callback_sequence: 0,
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    fn fill(&mut self, output: &mut [f32], timing: NativeOutputTiming) {
        let began = Instant::now();
        self.fill_inner(output, timing);
        self.complete(began.elapsed());
    }
    fn fill_inner(&mut self, output: &mut [f32], timing: NativeOutputTiming) {
        use crate::sink::quality::OutputKind;
        self.callback_sequence = self.callback_sequence.wrapping_add(1);
        self.pending_incident = CallbackIncident {
            sequence: self.callback_sequence,
            at_us: crate::diagnostics::monotonic_us(timing.callback_at),
            frames: timing.period_frames,
            rate: timing.sample_rate,
            native_gap_frames: timing.native_gap_frames,
            ..Default::default()
        };
        self.pending_period_ns = if timing.sample_rate == 0 {
            0
        } else {
            u64::from(timing.period_frames) * 1_000_000_000 / u64::from(timing.sample_rate)
        };
        if let Some(deadline) = timing.playback_at {
            self.pending_period_ns = self.pending_period_ns.min(
                deadline
                    .saturating_duration_since(timing.callback_at)
                    .as_nanos()
                    .min(u128::from(u64::MAX))
                    .max(1) as u64,
            );
        }
        self.pending_trace = None;
        self.pending_at_ms = (crate::diagnostics::monotonic_us(timing.callback_at).max(0) / 1000) as u64;
        self.pending_native_gap = timing.native_gap_frames;
        self.metrics.callbacks.fetch_add(1, Ordering::Relaxed);
        if let Some(previous) = self.previous_callback.replace(timing.callback_at) {
            self.pending_incident.gap_us = timing.callback_at.saturating_duration_since(previous).as_micros() as u64;
            self.metrics.max_gap_us.fetch_max(
                timing.callback_at.saturating_duration_since(previous).as_micros() as u64,
                Ordering::Relaxed,
            );
        }
        self.callback_ms.store(
            timing.callback_at.saturating_duration_since(self.started).as_millis() as u64,
            Ordering::Relaxed,
        );
        let channels = usize::from(timing.channels);
        let rate = timing.sample_rate;
        if channels == 0
            || channels > 64
            || !(8_000..=192_000).contains(&rate)
            || !output.len().is_multiple_of(channels)
        {
            output.fill(0.0);
            self.metrics.fault.store(true, Ordering::Release);
            return;
        }
        let frames = output.len() / channels;
        if frames == 0 {
            return;
        }
        let step = 48_000.0 / f64::from(rate);
        let total = self.phase + frames as f64 * step;
        let consumed = total.floor() as usize;
        let needed = ((self.phase + (frames - 1) as f64 * step).floor() as usize + 1).max(consumed);
        if needed > MAX_PULL_FRAMES {
            output.fill(0.0);
            self.metrics.fault.store(true, Ordering::Release);
            return;
        }
        let generation = self.metrics.quality_generation.load(Ordering::Acquire);
        let counters = self
            .metrics
            .quality
            .get()
            .filter(|_| self.metrics.quality_active.load(Ordering::Acquire));
        if counters.is_none() || generation != self.quality_generation {
            self.quality.reset();
            self.quality_generation = generation;
        }
        let at_ms = (crate::diagnostics::monotonic_us(timing.callback_at).max(0) / 1000) as u64;
        self.pending_at_ms = at_ms;
        self.pending_native_gap = timing.native_gap_frames;
        // Native timeline gaps are supplied by the adapter; a late callback by
        // itself never increments audible interruption counts.
        if let Some(gap) = timing.native_gap_frames {
            self.metrics.native_timing_known.store(true, Ordering::Relaxed);
            self.metrics.native_gap_frames.fetch_add(gap, Ordering::Relaxed);
            if gap != 0
                && let Some(counters) = counters
            {
                let delta = self.quality.gap(at_ms, rate, gap, counters);
                self.input.gap(OutputGap {
                    sources: self.last_sources,
                    delta,
                });
                self.splice.interrupted();
                self.underrun_frames.fetch_add(gap, Ordering::Relaxed);
            }
        }
        let carried = usize::from(self.carry.is_some());
        if let Some((sample, kind)) = self.carry.take() {
            self.scratch[..2].copy_from_slice(&sample);
            self.kinds[0] = kind;
        }
        self.input.begin(needed.saturating_sub(carried), timing.callback_at);
        self.pending_incident.phase_ns[0] = callback_offset_ns(timing.callback_at);
        let mut sources = [0; MIX_SOURCES];
        let mut offset = carried;
        while offset < needed {
            let end = (offset + 120).min(needed);
            let block = self.input.pull(
                &mut self.scratch[offset * 2..end * 2],
                needed - offset,
                timing.callback_at,
            );
            self.kinds[offset..end].fill(block.kind);
            for id in block.sources.into_iter().filter(|id| *id != 0) {
                if !sources.contains(&id)
                    && let Some(slot) = sources.iter_mut().find(|id| **id == 0)
                {
                    *slot = id;
                }
            }
            offset = end;
        }
        self.last_sources = sources;
        self.pending_incident.phase_ns[1] = callback_offset_ns(timing.callback_at);
        if self.metrics.fade_out.load(Ordering::Acquire) {
            self.fade.start(rate);
        }
        let mut missing = 0;
        let phase_start = self.phase;
        for (offset, frame) in output.chunks_exact_mut(channels).enumerate() {
            let position = phase_start + offset as f64 * step;
            let index = position.floor() as usize;
            let fraction = position.fract() as f32;
            let next = (index + 1).min(needed - 1);
            let mut kind = self.kinds[index];
            if fraction > 0.0 && self.kinds[next] != kind {
                kind = self.kinds[next];
            }
            let mut stereo = [0.0; 2];
            for (channel, sample) in stereo.iter_mut().enumerate() {
                let a = self.scratch[index * 2 + channel];
                *sample = a + (self.scratch[next * 2 + channel] - a) * fraction;
            }
            if let Some(counters) = counters {
                missing += u64::from(kind == OutputKind::Missing && self.quality.established());
                let audible = if channels == 1 {
                    [(stereo[0] + stereo[1]) * 0.5; 2]
                } else {
                    stereo
                };
                let delta = self.quality.frame(at_ms, rate, kind, audible, counters);
                if !delta.is_empty() {
                    self.input.gap(OutputGap {
                        sources: self.last_sources,
                        delta,
                    });
                }
            }
            self.splice.apply(&mut stereo, kind, rate);
            self.fade.apply(&mut stereo, kind != OutputKind::Intentional);
            for (channel, sample) in frame.iter_mut().enumerate() {
                *sample = if channels == 1 {
                    (stereo[0] + stereo[1]) * 0.5
                } else if channel < 2 {
                    stereo[channel]
                } else {
                    0.0
                };
            }
        }
        self.phase = total - consumed as f64;
        if needed > consumed {
            self.carry = Some((
                [self.scratch[consumed * 2], self.scratch[consumed * 2 + 1]],
                self.kinds[consumed],
            ));
        }
        self.pending_incident.phase_ns[2] = callback_offset_ns(timing.callback_at);
        self.input.end();
        self.pending_incident.work = self.input.work();
        self.pending_incident.missing_frames = missing;
        if missing != 0 {
            self.underrun_frames.fetch_add(missing, Ordering::Relaxed);
            self.metrics.deadline_misses.fetch_add(1, Ordering::Relaxed);
            self.metrics
                .worst_shortfall_us
                .fetch_max(missing * 1_000_000 / u64::from(rate), Ordering::Relaxed);
            if let Some(counters) = counters {
                counters.record_quality(crate::sink::DropoutBucket {
                    at_ms,
                    missing_ns: missing * 1_000_000_000 / u64::from(rate),
                    ..Default::default()
                });
            }
        }
        if output.iter().all(|sample| *sample == 0.0) {
            self.metrics.zero_periods.fetch_add(1, Ordering::Release);
            let at = timing.playback_at.unwrap_or_else(|| {
                timing.callback_at
                    + Duration::from_secs_f64(
                        f64::from(timing.padding_frames.max(timing.buffer_frames)) / f64::from(rate),
                    )
            }) + Duration::from_secs_f64(frames as f64 / f64::from(rate));
            let ns = at
                .saturating_duration_since(self.started)
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64;
            if self.fade.finished() {
                let _ = self
                    .metrics
                    .fade_silent_at_ns
                    .compare_exchange(0, ns, Ordering::Release, Ordering::Relaxed);
            }
            if self.input.drained() {
                let _ = self
                    .metrics
                    .drain_silent_at_ns
                    .compare_exchange(0, ns, Ordering::Release, Ordering::Relaxed);
            }
        } else {
            self.metrics.zero_periods.store(0, Ordering::Release);
            self.metrics.drain_silent_at_ns.store(0, Ordering::Release);
        }
        if let Some(observer) = PRESENTATION_OBSERVER.get()
            && (observer.enabled)()
            && let Some(at) = timing.playback_at
        {
            (observer.record)(timing.callback_at, at);
        }
        if self.traces.is_some() {
            self.pending_trace = Some(PlaybackTrace {
                sequence: None,
                pushed_at: timing.callback_at,
                callback_at: timing.callback_at,
                playback_at: timing.playback_at,
                output_frame_offset: 0,
                callback_frames: frames as u32,
                sample_rate: rate,
                native_padding_frames: timing.padding_frames,
                native_buffer_frames: timing.buffer_frames,
                render_ns: 0,
            });
        }
        self.pending_incident.phase_ns[3] = callback_offset_ns(timing.callback_at);
    }
}

fn callback_offset_ns(at: Instant) -> u64 {
    Instant::now()
        .saturating_duration_since(at)
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}

impl NativeRender for Render {
    fn fill(&mut self, output: &mut [f32], timing: NativeOutputTiming) {
        self.fill_inner(output, timing);
    }
    fn complete(&mut self, elapsed: Duration) {
        let ns = elapsed.as_nanos().min(i64::MAX as u128) as i64;
        self.pending_incident.budget_ns = self.pending_period_ns;
        self.pending_incident.elapsed_ns = ns as u64;
        self.pending_incident.fault = self.metrics.fault.load(Ordering::Relaxed);
        if self.pending_incident.exceptional() && self.incidents.push(self.pending_incident).is_err() {
            self.metrics.incidents_dropped.fetch_add(1, Ordering::Relaxed);
        }
        let margin = (self.pending_period_ns.min(i64::MAX as u64) as i64 - ns) / 1000;
        if self.pending_period_ns != 0 {
            self.metrics.min_margin_us.fetch_min(margin, Ordering::Relaxed);
            self.metrics.margin_known.store(true, Ordering::Release);
            if ns > self.pending_period_ns as i64 {
                self.metrics.over_budget.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.metrics
            .max_render_us
            .fetch_max(ns as u64 / 1000, Ordering::Relaxed);
        if let Some(counters) = self.metrics.quality.get() {
            counters.record_quality(crate::sink::DropoutBucket {
                at_ms: self.pending_at_ms,
                callbacks: 1,
                callback_over_budget: u64::from(ns > self.pending_period_ns as i64),
                callback_max_work_us: ns as u64 / 1000,
                callback_min_margin_us: (self.pending_period_ns != 0).then_some(margin),
                native_timing_callbacks: u64::from(self.pending_native_gap.is_some()),
                native_gap_frames: self.pending_native_gap.unwrap_or(0),
                ..Default::default()
            });
        }
        if let Some(mut trace) = self.pending_trace.take()
            && let Some(tx) = &mut self.traces
        {
            trace.render_ns = ns as u64;
            if tx.push(trace).is_err() {
                self.trace_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(feature = "test-support")]
impl Render {
    pub(crate) fn simulated(input: Box<dyn PullInput>, trace: bool) -> Self {
        let (tx, _) = handoff::channel(8192, empty_trace());
        Self::new(
            input,
            Arc::new(CallbackMetrics::default()),
            Instant::now(),
            Arc::new(AtomicU64::new(0)),
            trace.then_some(tx),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        )
    }
    pub(crate) fn simulate(
        &mut self,
        output: &mut [f32],
        rate: u32,
        channels: u16,
        period: u32,
        at: Instant,
        gap: Option<u64>,
    ) {
        self.fill(
            output,
            NativeOutputTiming {
                sample_rate: rate,
                channels,
                period_frames: period,
                callback_at: at,
                playback_at: None,
                padding_frames: 0,
                buffer_frames: period,
                native_gap_frames: gap,
            },
        );
    }
    pub(crate) fn observations(&self) -> PlaybackMetrics {
        PlaybackMetrics {
            worker: None,
            callbacks: self.metrics.callbacks.load(Ordering::Relaxed),
            callback_over_budget: self.metrics.over_budget.load(Ordering::Relaxed),
            callback_min_margin_us: self
                .metrics
                .margin_known
                .load(Ordering::Relaxed)
                .then(|| self.metrics.min_margin_us.load(Ordering::Relaxed)),
            native_gap_frames: self
                .metrics
                .native_timing_known
                .load(Ordering::Relaxed)
                .then(|| self.metrics.native_gap_frames.load(Ordering::Relaxed)),
            max_callback_gap_us: self.metrics.max_gap_us.load(Ordering::Relaxed),
            max_render_us: self.metrics.max_render_us.load(Ordering::Relaxed),
            underrun_frames: self.underrun_frames.load(Ordering::Relaxed),
            ..Default::default()
        }
    }
    pub(crate) fn simulated_quality(&self, counters: Arc<crate::sink::DropoutCounters>) {
        let _ = self.metrics.quality.set(counters);
        self.metrics.quality_active.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::quality::OutputKind;
    struct Control {
        value: std::sync::atomic::AtomicU32,
        kind: AtomicUsize,
        drained: std::sync::atomic::AtomicBool,
        frames: AtomicUsize,
        gaps: AtomicU64,
        gap_first_source: AtomicU64,
    }
    impl Control {
        fn set(&self, value: f32, kind: OutputKind) {
            self.value.store(value.to_bits(), Ordering::Release);
            self.kind.store(kind as usize, Ordering::Release);
        }
    }
    struct TestInput {
        control: Arc<Control>,
        ramp: bool,
        cursor: usize,
    }
    impl PullInput for TestInput {
        fn begin(&mut self, _: usize, _: Instant) {}
        fn pull(&mut self, out: &mut [f32], _: usize, _: Instant) -> PullBlock {
            let kind = match self.control.kind.load(Ordering::Acquire) {
                0 => OutputKind::Intentional,
                1 => OutputKind::Audio,
                2 => OutputKind::Concealed,
                _ => OutputKind::Missing,
            };
            let value = f32::from_bits(self.control.value.load(Ordering::Acquire));
            for frame in out.as_chunks_mut::<2>().0 {
                let sample = if self.ramp { self.cursor as f32 * 0.00001 } else { value };
                *frame = [sample; 2];
                self.cursor += 1;
            }
            self.control.frames.fetch_add(out.len() / 2, Ordering::Relaxed);
            let mut sources = [0; MIX_SOURCES];
            sources[..2].copy_from_slice(&[71, 93]);
            PullBlock { sources, kind }
        }
        fn end(&mut self) {}
        fn gap(&mut self, gap: OutputGap) {
            self.control.gaps.fetch_add(gap.delta.interruptions, Ordering::Relaxed);
            self.control.gap_first_source.store(gap.sources[0], Ordering::Relaxed);
        }
        fn drained(&self) -> bool {
            self.control.drained.load(Ordering::Acquire)
        }
    }
    fn renderer(ramp: bool) -> (Arc<Control>, Render, handoff::Consumer<PlaybackTrace>) {
        let control = Arc::new(Control {
            value: std::sync::atomic::AtomicU32::new(0.25f32.to_bits()),
            kind: AtomicUsize::new(OutputKind::Audio as usize),
            drained: false.into(),
            frames: AtomicUsize::new(0),
            gaps: AtomicU64::new(0),
            gap_first_source: AtomicU64::new(0),
        });
        let metrics = Arc::new(CallbackMetrics::default());
        metrics
            .quality
            .set(Arc::new(crate::sink::DropoutCounters::default()))
            .unwrap();
        metrics.quality_active.store(true, Ordering::Release);
        let (tx, traces) = handoff::channel(4, empty_trace());
        let render = Render::new(
            Box::new(TestInput {
                control: control.clone(),
                ramp,
                cursor: 0,
            }),
            metrics,
            Instant::now(),
            Arc::new(AtomicU64::new(0)),
            Some(tx),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );
        (control, render, traces)
    }
    fn timing(rate: u32) -> NativeOutputTiming {
        NativeOutputTiming {
            callback_at: Instant::now(),
            playback_at: None,
            sample_rate: rate,
            channels: 2,
            padding_frames: 0,
            buffer_frames: 128,
            period_frames: 128,
            native_gap_frames: None,
        }
    }
    // WASAPI consumes endpoint packets in engine periods, but its device clock
    // is sampled at a varying point after each event. All engine deadlines in
    // this simulation are met; clock-query phase is not missing source audio.
    #[test]
    fn wasapi_query_phase_jitter_preserves_tone() {
        struct Tone(usize);
        impl PullInput for Tone {
            fn begin(&mut self, _: usize, _: Instant) {}
            fn pull(&mut self, out: &mut [f32], _: usize, _: Instant) -> PullBlock {
                for frame in out.as_chunks_mut::<2>().0 {
                    frame.fill(tone(self.0));
                    self.0 += 1;
                }
                PullBlock {
                    sources: [1; MIX_SOURCES],
                    kind: OutputKind::Audio,
                }
            }
            fn end(&mut self) {}
            fn gap(&mut self, _: OutputGap) {}
            fn drained(&self) -> bool {
                false
            }
        }
        fn tone(frame: usize) -> f32 {
            (std::f64::consts::TAU * 997.0 * frame as f64 / 48_000.0).sin() as f32 * 0.5
        }
        let (_, mut render, _) = renderer(false);
        render.input = Box::new(Tone(0));
        let start = Instant::now();
        let mut signal = 0.0_f64;
        let mut error = 0.0_f64;
        for callback in 0..200 {
            // 0.125/0.375 ms query delay: only 12 frames of phase variation,
            // far smaller than the 480-frame period. No engine callback missed.
            let phase = if callback % 2 == 0 { 6 } else { 18 };
            let position = callback * 480 + phase;
            let mut at = timing(48_000);
            at.period_frames = 480;
            at.buffer_frames = 960;
            at.callback_at = start + Duration::from_secs_f64(position as f64 / 48_000.0);
            let observation = crate::wasapi_timing::observe_shared_render(
                Some(at.callback_at),
                at.callback_at,
                at.callback_at,
                0,
                48_000,
            );
            at.native_gap_frames = observation.native_gap_frames;
            at.playback_at = observation.playback_at;
            let mut out = [0.0; 960];
            render.fill(&mut out, at);
            for (frame, sample) in out.as_chunks::<2>().0.iter().enumerate() {
                let expected = f64::from(tone(callback * 480 + frame));
                signal += expected * expected;
                error += (f64::from(sample[0]) - expected).powi(2);
            }
        }
        let snr = 10.0 * (signal / error.max(f64::MIN_POSITIVE)).log10();
        eprintln!(
            "WASAPI phase-jitter tone SNR: {snr:.2} dB; inferred missing frames: {}",
            render.underrun_frames.load(Ordering::Relaxed)
        );
        assert!(
            snr > 100.0,
            "healthy PCM was changed by clock-query jitter: {snr:.2} dB"
        );
        assert_eq!(error, 0.0, "healthy tone must remain sample-exact");
        assert_eq!(render.underrun_frames.load(Ordering::Relaxed), 0);
        assert!(!render.metrics.native_timing_known.load(Ordering::Relaxed));
    }

    #[test]
    fn callback_incidents_distinguish_deadline_from_subsequent_native_gap() {
        let (_, mut render, _) = renderer(false);
        let first = timing(48_000);
        render.fill_inner(&mut [0.0; 256], first);
        render.complete(Duration::from_micros(50));
        assert!(render.metrics.take_incidents().is_empty());

        let mut slow = timing(48_000);
        slow.callback_at = first.callback_at + Duration::from_millis(3);
        render.fill_inner(&mut [0.0; 256], slow);
        render.complete(Duration::from_micros(12_515));
        let mut gap = timing(48_000);
        gap.callback_at = slow.callback_at + Duration::from_micros(15_417);
        gap.native_gap_frames = Some(612);
        render.fill_inner(&mut [0.0; 256], gap);
        render.complete(Duration::from_micros(50));

        let incidents = render.metrics.take_incidents();
        assert_eq!(incidents.len(), 2);
        assert_eq!(incidents[0].sequence, 2);
        assert_eq!(incidents[0].elapsed_ns, 12_515_000);
        assert_eq!(incidents[0].budget_ns, 2_666_666);
        assert_eq!(incidents[0].native_gap_frames, None);
        assert_eq!(incidents[1].sequence, 3);
        assert_eq!(incidents[1].native_gap_frames, Some(612));
        assert_eq!(incidents[1].gap_us, 15_417);
        assert_eq!(incidents[1].at_us - incidents[0].at_us, 15_417);
        assert!(incidents[1].elapsed_ns < incidents[1].budget_ns);
        assert!(render.metrics.take_incidents().is_empty());
    }

    #[test]
    fn incident_overflow_and_stalled_consumer_never_block_or_allocate_in_callback() {
        let (_, mut render, _) = renderer(false);
        let metrics = render.metrics.clone();
        // Even while the ordinary diagnostics consumer holds its mutex, RT
        // must continue. This also exercises the first incident and queue-full.
        let held = metrics.incidents.get().unwrap().lock().unwrap();
        crate::allocation_audit::assert_no_heap(|| {
            for _ in 0..100 {
                render.fill_inner(&mut [0.0; 256], timing(48_000));
                render.complete(Duration::from_micros(12_515));
            }
        });
        drop(held);
        let incidents = metrics.take_incidents();
        assert_eq!(incidents.len(), CALLBACK_INCIDENT_CAPACITY);
        assert_eq!(incidents[0].sequence, 1);
        assert_eq!(incidents.last().unwrap().sequence, CALLBACK_INCIDENT_CAPACITY as u64);
        assert_eq!(
            metrics.incidents_dropped.load(Ordering::Relaxed),
            100 - CALLBACK_INCIDENT_CAPACITY as u64
        );
        assert!(metrics.take_incidents().is_empty());
    }

    #[test]
    fn format_and_request_bounds_fail_silent_before_pulling() {
        for (rate, channels, frames) in [
            (48_000, 0, 128),
            (48_000, 65, 128),
            (7999, 2, 128),
            (192_001, 2, 128),
            (8000, 2, 3000),
        ] {
            let (control, mut render, _) = renderer(false);
            let mut output = vec![0.7; frames * channels.max(1) as usize];
            let mut native = timing(rate);
            native.channels = channels;
            crate::allocation_audit::assert_no_heap(|| render.fill(&mut output, native));
            assert!(output.iter().all(|sample| *sample == 0.0));
            assert!(render.metrics.fault.load(Ordering::Acquire));
            assert_eq!(control.frames.load(Ordering::Relaxed), 0);
        }
    }
    #[test]
    fn expired_native_deadline_keeps_a_known_negative_margin() {
        let (_, mut render, _) = renderer(false);
        let mut native = timing(48_000);
        native.playback_at = Some(native.callback_at);
        render.fill_inner(&mut [0.0; 256], native);
        render.complete(Duration::from_micros(100));
        assert!(render.metrics.margin_known.load(Ordering::Acquire));
        assert_eq!(render.metrics.over_budget.load(Ordering::Relaxed), 1);
        assert!(render.metrics.min_margin_us.load(Ordering::Relaxed) < 0);
    }
    #[test]
    fn control_thread_stall_does_not_affect_pull() {
        for frames in [128, 256, 480] {
            let (control, mut render, _) = renderer(false);
            let mut output = vec![0.0; frames * 2];
            // No ordinary-thread refill work occurs between these callbacks.
            for _ in 0..100 {
                crate::allocation_audit::assert_no_heap(|| render.fill(&mut output, timing(48_000)));
                assert!(output.iter().all(|sample| *sample == 0.25));
            }
            assert_eq!(control.frames.load(Ordering::Relaxed), frames * 100);
            assert_eq!(render.underrun_frames.load(Ordering::Relaxed), 0);
        }
    }
    #[test]
    fn native_gap_feedback_keeps_the_contributors_of_consumed_pcm() {
        let (control, mut render, _) = renderer(false);
        render.fill(&mut [0.0; 256], timing(48_000));
        let mut gap = timing(48_000);
        gap.native_gap_frames = Some(480);
        render.fill(&mut [0.0; 256], gap);
        assert_eq!(control.gaps.load(Ordering::Relaxed), 1);
        assert_eq!(control.gap_first_source.load(Ordering::Relaxed), 71);
        assert_eq!(render.metrics.native_gap_frames.load(Ordering::Relaxed), 480);
    }
    #[test]
    fn callback_lateness_without_native_gap_is_not_an_interruption() {
        let (control, mut render, _) = renderer(false);
        let mut native = timing(48_000);
        render.fill(&mut [0.0; 256], native);
        native.callback_at += Duration::from_millis(100);
        render.fill(&mut [0.0; 256], native);
        assert_eq!(control.gaps.load(Ordering::Relaxed), 0);
        assert!(!render.metrics.native_timing_known.load(Ordering::Relaxed));
        assert_eq!(render.metrics.max_gap_us.load(Ordering::Relaxed), 100_000);
    }
    #[test]
    fn short_native_underrun_and_recovery_do_not_step_pcm() {
        // A local bounded-work fault can produce silence during a callback.
        // Its two edges are repairable; a past uncalled native gap is not.
        for rate in [44_100, 48_000, 96_000] {
            let (control, mut render, _) = renderer(false);
            control.set(0.5, OutputKind::Audio);
            render.fill(&mut [0.0; 96], timing(rate));
            let mut last = 0.5;
            for (value, kind) in [(0.0, OutputKind::Missing), (-0.5, OutputKind::Audio)] {
                control.set(value, kind);
                let mut output = vec![0.0; (rate as usize / 100) * 2];
                crate::allocation_audit::assert_no_heap(|| render.fill(&mut output, timing(rate)));
                for sample in output.as_chunks::<2>().0 {
                    assert!((sample[0] - last).abs() < 0.02);
                    last = sample[0];
                }
                assert!((last - value).abs() < 0.001);
            }
        }
    }
    #[test]
    fn exit_pop_native_teardown_fades_before_the_stream_can_stop() {
        for rate in [44_100, 48_000, 96_000] {
            let (control, mut render, _) = renderer(false);
            let mut before = [0.0; 100];
            render.fill(&mut before, timing(rate));
            render.metrics.fade_out.store(true, Ordering::Release);
            control.set(0.0, OutputKind::Intentional);
            let frames = (rate as usize * 15).div_ceil(1000);
            let mut tail = vec![0.0; (frames + 10) * 2];
            crate::allocation_audit::assert_no_heap(|| render.fill(&mut tail, timing(rate)));
            assert!((tail[0] - before[99]).abs() < 0.001);
            for pair in tail.as_chunks::<2>().0.windows(2) {
                assert!((pair[1][0] - pair[0][0]).abs() < 0.001);
            }
            assert!(tail[(frames - 1) * 2..].iter().all(|sample| *sample == 0.0));
            assert_eq!(render.metrics.fade_silent_at_ns.load(Ordering::Acquire), 0);
            let mut native = timing(rate);
            let presented = native.callback_at + Duration::from_millis(40);
            native.playback_at = Some(presented);
            render.fill(&mut [0.0; 240], native);
            assert_eq!(
                render.metrics.fade_silent_at_ns.load(Ordering::Acquire),
                (presented + Duration::from_secs_f64(120.0 / f64::from(rate)))
                    .saturating_duration_since(render.started)
                    .as_nanos() as u64
            );
        }
    }
    #[test]
    fn exit_pop_native_teardown_waits_for_padding_without_presentation_timing() {
        let (control, mut render, _) = renderer(false);
        render.fill(&mut [0.0; 240], timing(48_000));
        render.metrics.fade_out.store(true, Ordering::Release);
        control.set(0.0, OutputKind::Intentional);
        render.fill(&mut [0.0; 1440], timing(48_000));
        let mut native = timing(48_000);
        native.padding_frames = 2400;
        render.fill(&mut [0.0; 240], native);
        assert_eq!(
            render.metrics.fade_silent_at_ns.load(Ordering::Acquire),
            (native.callback_at + Duration::from_micros(52_500))
                .saturating_duration_since(render.started)
                .as_nanos() as u64
        );
    }
    #[test]
    fn native_underrun_is_output_wide_and_uses_device_rate_but_idle_drain_is_excluded() {
        for active in [true, false] {
            let (control, mut render, _) = renderer(false);
            render.fill(&mut [0.0; 256], timing(96_000));
            render.metrics.quality_active.store(active, Ordering::Release);
            control.set(0.0, OutputKind::Missing);
            render.fill(&mut [0.0; 1920], timing(96_000));
            assert_eq!(
                render.underrun_frames.load(Ordering::Relaxed),
                if active { 960 } else { 0 }
            );
            assert_eq!(control.gaps.load(Ordering::Relaxed), u64::from(active));
        }
    }
    #[test]
    fn first_pull_has_no_extra_silent_period_and_drain_waits_for_presentation() {
        let (control, mut render, _) = renderer(false);
        let mut first = [0.0; 256];
        render.fill(&mut first, timing(48_000));
        assert_eq!(first, [0.25; 256]);
        control.set(0.0, OutputKind::Intentional);
        control.drained.store(true, Ordering::Release);
        let mut native = timing(48_000);
        native.playback_at = Some(native.callback_at + Duration::from_millis(20));
        render.fill(&mut [0.0; 256], native);
        assert!(
            render.metrics.drain_silent_at_ns.load(Ordering::Acquire)
                > native.callback_at.saturating_duration_since(render.started).as_nanos() as u64
        );
    }
    #[test]
    fn pull_and_callback_do_no_heap_work_at_native_rates_or_on_overflow() {
        for rate in [44_100, 48_000, 96_000] {
            for frames in [128, 256, 441, 480] {
                let (_, mut render, _) = renderer(false);
                let mut output = vec![0.0; frames * 2];
                crate::allocation_audit::assert_no_heap(|| {
                    for _ in 0..12 {
                        render.fill(&mut output, timing(rate));
                    }
                });
                assert_eq!(render.trace_dropped.load(Ordering::Relaxed), 8);
                assert!(output.iter().all(|sample| *sample == 0.25));
            }
        }
    }
    #[test]
    fn low_rate_output_keeps_phase_across_pull_boundaries() {
        for rate in [16_000, 44_100, 96_000] {
            let (control, mut render, _) = renderer(true);
            let mut actual_frames = 0;
            for size in [128, 441, 256, 128, 480, 127] {
                let mut output = vec![0.0; size * 2];
                render.fill(&mut output, timing(rate));
                for (offset, frame) in output.as_chunks::<2>().0.iter().enumerate() {
                    let expected = (actual_frames + offset) as f64 * 48_000.0 / f64::from(rate) * 0.00001;
                    // Optional future interpolation is held at the callback's last sample.
                    assert!((f64::from(frame[0]) - expected).abs() < 0.000011);
                }
                actual_frames += size;
            }
            assert!(
                (control.frames.load(Ordering::Relaxed) as f64 - actual_frames as f64 * 48_000.0 / f64::from(rate))
                    .abs()
                    <= 1.0
            );
        }
    }
    #[test]
    fn exact_rate_final_sample_does_not_require_future_packet() {
        let (control, mut render, _) = renderer(false);
        render.fill(&mut [0.0; 256], timing(48_000));
        assert_eq!(control.frames.load(Ordering::Relaxed), 128);
        assert!(render.carry.is_none());
    }
    #[test]
    fn trace_marks_the_current_pull_submission_without_queued_pcm_age() {
        let (_, mut render, mut traces) = renderer(false);
        let native = timing(48_000);
        render.fill(&mut [0.0; 256], native);
        let trace = traces.pop().unwrap();
        assert_eq!(trace.pushed_at, native.callback_at);
        assert_eq!(trace.output_frame_offset, 0);
        assert_eq!(trace.callback_frames, 128);
    }
    #[test]
    #[ignore = "Evidence-only: ordinary-thread CPU/timer scheduling is nondeterministic; no native devices"]
    fn software_callback_cpu_and_timer_jitter_stress() {
        let (_, mut render, _) = renderer(false);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut load = Vec::new();
        for seed in 1..=2u64 {
            let stop = stop.clone();
            load.push(std::thread::spawn(move || {
                let mut value = seed;
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..10_000 {
                        value = std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
                    }
                    std::thread::yield_now();
                }
            }));
        }
        let period = Duration::from_secs_f64(128.0 / 48_000.0);
        let mut due = Instant::now() + period;
        let mut lateness = 0;
        for _ in 0..750 {
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            lateness = lateness.max(Instant::now().saturating_duration_since(due).as_micros());
            render.fill(&mut [0.0; 256], timing(48_000));
            due += period;
        }
        stop.store(true, Ordering::Release);
        for thread in load {
            thread.join().unwrap();
        }
        eprintln!(
            "software pull stress: callbacks=750 load_threads=2 timer_worst_lateness_us={lateness} max_render_us={} callback_over_budget={} native_gap=unknown",
            render.metrics.max_render_us.load(Ordering::Relaxed),
            render.metrics.over_budget.load(Ordering::Relaxed)
        );
        assert_eq!(render.underrun_frames.load(Ordering::Relaxed), 0);
    }
}
