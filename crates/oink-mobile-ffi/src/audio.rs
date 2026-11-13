//! C ABI for the mobile PCM boundary.
//!
//! Native shells own the AVAudioEngine/AAudio stream. Rust owns the callback
//! handles, the final output tap, the bounded input handoff and the local Case
//! A worker. Callback functions are atomics and fixed buffers only; the core
//! callback is invoked on the bounded input worker, never on the I/O callback.

use crate::microphone_transport::MicRoute;
use oink_audio::{
    echo::{Control, Processor, RenderTap, Summary, SummaryPublisher, Time, TimeQuality},
    forward_volume, media,
    microphone::resample::Resampler,
    mobile_native::{self, Format, MAX_FRAMES},
    negotiation::Profile,
    sink::{NativePull, NativeSources},
    source,
};
use oink_foundation::snapshot::Snapshot;
use std::{
    cell::UnsafeCell,
    collections::HashMap,
    ffi::c_void,
    ptr, slice,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Instant,
};

const BLOCK: usize = oink_audio::echo::BLOCK;
const MAX_INPUT_SAMPLES: usize = MAX_FRAMES * 2;
const MAX_RESAMPLED_FRAMES: usize = 48_000 / 50 + 2;

pub type RenderCallback = unsafe extern "C" fn(*mut c_void, *mut f32, u32, u64);
pub type CaptureCallback = unsafe extern "C" fn(*mut c_void, *const f32, u32, u64);

fn controls() -> &'static Mutex<HashMap<u64, Weak<Control>>> {
    static CONTROLS: OnceLock<Mutex<HashMap<u64, Weak<Control>>>> = OnceLock::new();
    CONTROLS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn control_for(generation: u64) -> Result<Arc<Control>, String> {
    let mut all = controls()
        .lock()
        .map_err(|_| "audio control registry poisoned".to_string())?;
    if let Some(control) = all.get(&generation).and_then(Weak::upgrade) {
        return Ok(control);
    }
    all.retain(|_, control| control.strong_count() != 0);
    let control = Arc::new(Control::new()?);
    all.insert(generation, Arc::downgrade(&control));
    Ok(control)
}

#[derive(Clone)]
pub(crate) enum NativeAudio {
    Output(Arc<OinkAudioOutput>),
    Input(Arc<OinkAudioInput>),
    Forward(Arc<crate::forward::OinkAudioForward>),
}

enum RegisteredAudio {
    Output(Weak<OinkAudioOutput>),
    Input(Weak<OinkAudioInput>),
    Forward(Weak<crate::forward::OinkAudioForward>),
}

fn handles() -> &'static Mutex<HashMap<u64, RegisteredAudio>> {
    static HANDLES: OnceLock<Mutex<HashMap<u64, RegisteredAudio>>> = OnceLock::new();
    HANDLES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn register_forward(handle: &Arc<crate::forward::OinkAudioForward>) {
    handles().lock().expect("audio handle registry").insert(
        Arc::as_ptr(handle) as u64,
        RegisteredAudio::Forward(Arc::downgrade(handle)),
    );
}

pub(crate) fn unregister_forward(address: u64) {
    handles().lock().expect("audio handle registry").remove(&address);
}

impl NativeAudio {
    pub(crate) fn lookup(address: u64) -> Option<Self> {
        match handles().lock().expect("audio handle registry").get(&address)? {
            RegisteredAudio::Output(output) => output.upgrade().map(Self::Output),
            RegisteredAudio::Input(input) => input.upgrade().map(Self::Input),
            RegisteredAudio::Forward(handle) => handle.upgrade().map(Self::Forward),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        match self {
            Self::Output(output) => output.native.format().generation,
            Self::Input(input) => input.format.generation,
            Self::Forward(handle) => handle.format.generation,
        }
    }

    pub(crate) fn revoke(&self) {
        match self {
            Self::Output(output) => output.native.revoke(),
            Self::Input(input) => input.native.revoke(),
            Self::Forward(handle) => handle.revoke(),
        }
    }

    pub(crate) fn sources(&self) -> Option<Arc<NativeSources>> {
        match self {
            Self::Output(output) => Some(output.sources.clone()),
            Self::Input(_) | Self::Forward(_) => None,
        }
    }

    pub(crate) fn set_microphone_routes(&self, routes: Vec<MicRoute>) {
        if let Self::Input(input) = self
            && *input.routes.load() != routes
        {
            input.routes.publish(routes);
        }
    }

    /// The speaker output's playout gain (iOS exit volume); `None` plays the
    /// PCM untouched. Takes effect at the next callback with a 5 ms ramp.
    pub(crate) fn set_playout_gain(&self, percent: Option<u16>) {
        if let Self::Output(output) = self {
            let target = percent.map_or(PLAYOUT_OFF, |percent| u32::from(percent.min(200)));
            output.playout_target.store(target, Ordering::Release);
        }
    }

    pub(crate) fn set_microphone_gain(&self, gain: u16, muted: bool) {
        if let Self::Input(input) = self {
            let control = input.mic_controls.lock().expect("microphone controls mutex");
            control.set_percent(gain);
            control.set_muted(muted);
        }
    }

    pub(crate) fn take_aec_stats(&self) -> Option<AecStats> {
        let Self::Input(input) = self else { return None };
        Some(AecStats {
            blocks: input.aec_blocks.swap(0, Ordering::Relaxed),
            bypass_blocks: input.aec_bypass.swap(0, Ordering::Relaxed),
            deadline_misses: input.aec_deadline_misses.swap(0, Ordering::Relaxed),
        })
    }

    pub(crate) fn set_aec(&self, enabled: bool, local_mic: bool, local_exit: bool) {
        if let Self::Input(input) = self {
            input.aec_enabled.store(enabled, Ordering::Release);
            input.local_mic.store(local_mic, Ordering::Release);
            input.local_exit.store(local_exit, Ordering::Release);
            input.service_aec();
        }
    }

    /// Media owner pass: create/retire Case A engines here, never on the
    /// capture worker. Returns when the owner must serve a pending reset.
    pub(crate) fn service_aec(&self) -> Option<Instant> {
        let Self::Input(input) = self else { return None };
        input.service_aec()
    }

    /// The capture worker wakes the media owner through this when it asks
    /// for a fresh AEC engine.
    pub(crate) fn set_aec_waker(&self, waker: Arc<tokio::sync::Notify>) {
        if let Self::Input(input) = self {
            let _ = input.aec_waker.set(waker);
        }
    }

    /// The next processor window of a capture handle (media owner only).
    pub(crate) fn take_aec_summary(&self) -> Option<Summary> {
        let Self::Input(input) = self else { return None };
        input.control.take_summary()
    }

    /// Native callbacks since the last call, and how many of them came
    /// without a valid host time (iOS `kAudioTimeStampHostTimeValid` unset,
    /// Android no AAudio timestamp): those blocks cannot be aligned.
    pub(crate) fn take_host_time_counts(&self) -> (u64, u64) {
        let (callbacks, unknown) = match self {
            Self::Output(output) => (&output.callbacks, &output.host_unknown),
            Self::Input(input) => (&input.callbacks, &input.host_unknown),
            Self::Forward(_) => return (0, 0),
        };
        (callbacks.swap(0, Ordering::Relaxed), unknown.swap(0, Ordering::Relaxed))
    }

    /// Case A pairs an output and an input through the one control of their
    /// native generation (`control_for`). Handles of different generations
    /// never meet: the input's processor then has no reference at all.
    pub(crate) fn shares_case_a_control(&self, other: &Self) -> bool {
        let control = |audio: &Self| match audio {
            Self::Output(output) => output.control.clone(),
            Self::Input(input) => input.control.clone(),
            Self::Forward(_) => unreachable!("forward never participates in AEC"),
        };
        Arc::ptr_eq(&control(self), &control(other))
    }
}

#[repr(C)]
pub struct OinkAudioOutput {
    native: mobile_native::Output,
    renderer: UnsafeCell<NativePull>,
    sources: Arc<NativeSources>,
    control: Arc<Control>,
    tap: UnsafeCell<RenderTap>,
    /// Native frame position of the next pulled frame (callback-owned).
    tap_frame: UnsafeCell<u64>,
    render: AtomicUsize,
    render_context: AtomicPtr<c_void>,
    /// Pulls, and pulls without a host time, since the owner last read them.
    callbacks: AtomicU64,
    host_unknown: AtomicU64,
    /// iOS exit volume (`crate::playout_volume`): the forwarding-gain position
    /// 0..=200 the callback applies, or `PLAYOUT_OFF` (Android: the system
    /// volume applies and the PCM passes untouched).
    playout_target: AtomicU32,
    /// Callback-owned gain stage (5 ms ramp, peak limiter, 1 ms lookahead).
    playout: UnsafeCell<forward_volume::Gain>,
}

const PLAYOUT_OFF: u32 = u32::MAX;

// SAFETY: native output callbacks are serialized by AVAudioEngine/AAudio. The
// only mutable tap and playout-gain access is from that one callback; all callback registration
// state is atomic and the control owner waits for quiescence before destroy.
unsafe impl Sync for OinkAudioOutput {}

#[repr(C)]
pub struct OinkAudioInput {
    format: Format,
    native: mobile_native::Input,
    control: Arc<Control>,
    processor: UnsafeCell<Processor>,
    scratch: UnsafeCell<[f32; MAX_INPUT_SAMPLES]>,
    pending: UnsafeCell<Pending>,
    resampler: UnsafeCell<Resampler>,
    mic_encoder: UnsafeCell<source::Encoder>,
    mic_controls: Mutex<source::Encoded>,
    routes: Arc<Snapshot<Vec<MicRoute>>>,
    capture: AtomicUsize,
    capture_context: AtomicPtr<c_void>,
    aec_enabled: AtomicBool,
    local_mic: AtomicBool,
    local_exit: AtomicBool,
    /// AEC window totals since the media owner last drained them: blocks
    /// seen, bypassed (no valid reference / not eligible) and over budget.
    aec_blocks: AtomicU64,
    aec_bypass: AtomicU64,
    aec_deadline_misses: AtomicU64,
    /// Media owner wakeup for a pending engine reset, and whether the worker
    /// already sent it for the current request.
    aec_waker: OnceLock<Arc<tokio::sync::Notify>>,
    aec_signalled: AtomicBool,
    /// Processor windows for the owner's `oink_echo` log (worker-owned end).
    summaries: UnsafeCell<SummaryPublisher>,
    /// Pushes, and pushes without a host time, since the owner last read them.
    callbacks: AtomicU64,
    host_unknown: AtomicU64,
}

/// Drained AEC statistics of one capture handle (contracts/audio-v2.md:
/// bypass and over-budget blocks are recorded, never reported as processed).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AecStats {
    pub blocks: u64,
    pub bypass_blocks: u64,
    pub deadline_misses: u64,
}

// SAFETY: `native` is an SPSC handoff; its producer is the native callback and
// its consumer/processor/scratch are owned by the worker. Callback registration
// and AEC policy are atomic. The owner must stop and join both before destroy.
unsafe impl Sync for OinkAudioInput {}

struct Pending {
    samples: [f32; BLOCK],
    frames: usize,
    host_ns: u64,
    discontinuity: bool,
    /// 48 kHz capture frames handed on in finished blocks.
    position: u64,
}

impl Default for Pending {
    fn default() -> Self {
        Self {
            samples: [0.0; BLOCK],
            frames: 0,
            host_ns: 0,
            discontinuity: false,
            position: 0,
        }
    }
}

impl OinkAudioOutput {
    fn pull(&self, samples: &mut [f32], frames: usize, host_ns: u64) {
        self.native.pull(samples, frames, host_ns);
        let target = self.playout_target.load(Ordering::Acquire);
        // The gain stage is stereo-linked; the iOS output is always stereo.
        if target != PLAYOUT_OFF && self.native.format().channels == 2 {
            // SAFETY: one serialized native output callback owns the gain stage.
            let gain = unsafe { &mut *self.playout.get() };
            // Only 0..=200 is ever stored, so the target is always valid.
            let _ = gain.set_percent(target as u16);
            gain.process(&mut samples[..frames * 2]);
        }
        // SAFETY: one serialized native output callback owns the position.
        let tap_frame = unsafe { &mut *self.tap_frame.get() };
        let frame = *tap_frame;
        *tap_frame += frames as u64;
        let time = Time {
            host_ns: (host_ns != 0).then_some(host_ns),
            device_clock: None,
            // Pulls are contiguous output frames; the reference clock fit
            // needs this position next to the host time.
            native_frame: Some(frame as f64),
            rate: self.native.format().rate,
            quality: if host_ns == 0 {
                TimeQuality::Unknown
            } else {
                TimeQuality::HardwareBoundary
            },
            discontinuity: host_ns == 0,
            delivered_at: Instant::now(),
        };
        // SAFETY: one serialized native output callback owns this tap, and the
        // caller keeps the handle alive until the callback admission barrier.
        unsafe { &mut *self.tap.get() }.supply(samples, self.native.format().channels as usize, time);
    }
}

impl OinkAudioInput {
    fn service_aec(&self) -> Option<Instant> {
        self.control.update(
            self.aec_enabled.load(Ordering::Acquire),
            self.local_mic.load(Ordering::Acquire),
            self.local_exit.load(Ordering::Acquire),
            Instant::now(),
        );
        self.control.service_at()
    }

    fn emit(&self, samples: &[f32], frames: usize, host_ns: u64) {
        let callback = self.capture.load(Ordering::Acquire);
        if callback == 0 {
            return;
        }
        let context = self.capture_context.load(Ordering::Acquire);
        // SAFETY: callback/context are installed on the control thread and
        // retained until native worker quiescence; samples are worker scratch.
        let callback: CaptureCallback = unsafe { std::mem::transmute(callback) };
        // SAFETY: the callback pointer and context are installed together on
        // the owner thread and remain valid until the worker is quiescent.
        unsafe { callback(context, samples.as_ptr(), frames as u32, host_ns) };
    }

    fn process(&self) {
        // SAFETY: the worker is the sole owner of these cells.
        let scratch = unsafe { &mut *self.scratch.get() };
        let batch = self.native.drain(scratch);
        if batch.frames == 0 {
            return;
        }
        let channels = self.format.channels as usize;
        let samples = &scratch[..batch.frames * channels];
        let mut mono = [0.0; MAX_RESAMPLED_FRAMES];
        let (frames, host_ns) = if self.format.rate == 48_000 {
            for (frame, output) in samples.chunks_exact(channels).zip(&mut mono) {
                *output = frame[0];
            }
            (batch.frames, batch.host_ns)
        } else {
            // Registered callbacks keep the native PCM format. The network
            // encoder and AEC share the resampled 48 kHz capture below.
            self.emit(samples, batch.frames, batch.host_ns);
            // SAFETY: the capture worker exclusively owns the resampler.
            let resampler = unsafe { &mut *self.resampler.get() };
            if batch.discontinuity {
                resampler.reset();
            }
            let mut frames = 0;
            for frame in samples.chunks_exact(channels) {
                resampler.push(frame[0], |sample| {
                    if let Some(output) = mono.get_mut(frames) {
                        *output = sample;
                        frames += 1;
                    }
                });
            }
            (frames, batch.host_ns.saturating_sub(resampler.delay_ns()))
        };

        // SAFETY: the capture worker exclusively owns the pending block.
        let pending = unsafe { &mut *self.pending.get() };
        if batch.discontinuity {
            pending.frames = 0;
            pending.discontinuity = true;
        }
        let mut offset = 0;
        while offset < frames {
            if pending.frames == 0 {
                pending.host_ns = if host_ns == 0 {
                    0
                } else {
                    host_ns.saturating_add(offset as u64 * 1_000_000_000 / 48_000)
                };
            }
            let count = (BLOCK - pending.frames).min(frames - offset);
            pending.samples[pending.frames..pending.frames + count].copy_from_slice(&mono[offset..offset + count]);
            pending.frames += count;
            offset += count;
            if pending.frames == BLOCK {
                self.process_block(pending);
                pending.frames = 0;
                pending.discontinuity = false;
                pending.position += BLOCK as u64;
            }
        }
    }

    fn process_block(&self, pending: &Pending) {
        let now = Instant::now();
        let enabled = self.aec_enabled.load(Ordering::Acquire);
        let time = Time {
            host_ns: (pending.host_ns != 0).then_some(pending.host_ns),
            device_clock: None,
            native_frame: None,
            rate: 48_000,
            quality: if pending.host_ns == 0 {
                TimeQuality::Unknown
            } else {
                TimeQuality::HardwareBoundary
            },
            discontinuity: pending.discontinuity || pending.host_ns == 0,
            delivered_at: now,
        };
        let mut output = [0.0; BLOCK];
        // SAFETY: the capture worker exclusively owns the processor.
        let processor = unsafe { &mut *self.processor.get() };
        processor.prepare(now);
        if enabled {
            processor.process(&pending.samples, time, 1_000_000_000.0 / 48_000.0, &mut output, now);
        } else {
            processor.raw(&pending.samples, &mut output, now);
        }
        // Engines are built by the media owner (`service_aec`), not here.
        if !self.control.needs_service() {
            self.aec_signalled.store(false, Ordering::Relaxed);
        } else if !self.aec_signalled.swap(true, Ordering::Relaxed)
            && let Some(waker) = self.aec_waker.get()
        {
            waker.notify_one();
        }
        // One summary per window (<= 1 s). A user-disabled AEC is not a bypass.
        if let Some(summary) = processor.summary(now) {
            if enabled {
                self.aec_blocks.fetch_add(summary.blocks, Ordering::Relaxed);
                self.aec_bypass.fetch_add(summary.bypass_blocks, Ordering::Relaxed);
                self.aec_deadline_misses
                    .fetch_add(summary.deadline_misses, Ordering::Relaxed);
            }
            // SAFETY: the capture worker exclusively owns the publisher.
            unsafe { &mut *self.summaries.get() }.publish(summary);
        }

        let mut stereo = [0.0; BLOCK * 2];
        for (sample, frame) in output.iter().zip(stereo.as_chunks_mut::<2>().0) {
            *frame = [*sample; 2];
        }
        if !self.routes.load().is_empty() {
            let stamp = source::Stamp {
                first_sample_at: None,
                callback_at: None,
                delivered_at: now,
                // This path accounts for every native frame itself: the
                // callback hands each one to the bounded handoff, a refused
                // frame marks the next batch discontinuous, and a
                // discontinuity drops the partial block above. So the
                // encoder must not also guess loss from delivery age (a gap
                // over 20 ms clears its limiter lookahead and pre-roll).
                // Blocks reach the encoder in native bursts, and Android
                // AAudio without a FAST input path delivers 20 ms bursts
                // (an Android test device: burst=960), so the first block of a burst often
                // came just over 20 ms after the last one and 1 ms of audio
                // was cut each time: a crackle under the microphone signal.
                source_frame_position: Some(pending.position),
                discontinuity: pending.discontinuity,
            };
            // SAFETY: the capture worker exclusively owns the encoder.
            unsafe { &mut *self.mic_encoder.get() }.supply(&stereo, stamp);
        }
        if self.format.rate == 48_000 {
            let samples: &[f32] = if self.format.channels == 1 { &output } else { &stereo };
            self.emit(samples, BLOCK, pending.host_ns);
        }
    }
}

unsafe extern "C" fn render_dispatch(owner: *mut c_void, samples: *mut f32, frames: u32, host_ns: u64) {
    if owner.is_null() || samples.is_null() {
        return;
    }
    // SAFETY: owner is the live OinkAudioOutput installed by pull().
    let output = unsafe { &*(owner.cast::<OinkAudioOutput>()) };
    let callback = output.render.load(Ordering::Acquire);
    if callback == 0 {
        let channels = output.native.format().channels;
        // SAFETY: mobile_native::Output already checked the PCM buffer extent.
        let samples = unsafe { slice::from_raw_parts_mut(samples, frames as usize * channels as usize) };
        // SAFETY: one serialized native output callback owns NativePull.
        unsafe { &mut *output.renderer.get() }.fill(samples, channels, Instant::now());
        return;
    }
    let context = output.render_context.load(Ordering::Acquire);
    // SAFETY: the atomic value was written from a valid `RenderCallback` and
    // is cleared before the owner/context can be released.
    let callback: RenderCallback = unsafe { std::mem::transmute(callback) };
    // SAFETY: native output owns the samples buffer for this callback, and
    // callback admission keeps the context alive through its invocation.
    unsafe { callback(context, samples, frames, host_ns) };
}

fn format(rate: u32, channels: u16, generation: u64) -> Result<Format, String> {
    let format = Format {
        rate,
        channels,
        generation,
    };
    format.validate()?;
    Ok(format)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_output_create(rate: u32, channels: u16, generation: u64) -> *mut OinkAudioOutput {
    let Ok(format) = format(rate, channels, generation) else {
        return ptr::null_mut();
    };
    let Ok(control) = control_for(generation) else {
        return ptr::null_mut();
    };
    let Ok(native) = mobile_native::Output::new(format) else {
        return ptr::null_mut();
    };
    let tap = RenderTap::new(control.clone());
    let (renderer, sources) = NativePull::new(rate);
    let output = Arc::new(OinkAudioOutput {
        native,
        renderer: UnsafeCell::new(renderer),
        sources,
        control,
        tap: UnsafeCell::new(tap),
        tap_frame: UnsafeCell::new(0),
        render: AtomicUsize::new(0),
        render_context: AtomicPtr::new(ptr::null_mut()),
        callbacks: AtomicU64::new(0),
        host_unknown: AtomicU64::new(0),
        playout_target: AtomicU32::new(PLAYOUT_OFF),
        playout: UnsafeCell::new(forward_volume::Gain::with_rate(100, rate)),
    });
    let address = Arc::as_ptr(&output).cast_mut();
    output.native.set_renderer(address.cast(), Some(render_dispatch));
    handles()
        .lock()
        .expect("audio handle registry")
        .insert(address as u64, RegisteredAudio::Output(Arc::downgrade(&output)));
    Arc::into_raw(output).cast_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_input_create(rate: u32, channels: u16, generation: u64) -> *mut OinkAudioInput {
    let Ok(format) = format(rate, channels, generation) else {
        return ptr::null_mut();
    };
    let Ok(control) = control_for(generation) else {
        return ptr::null_mut();
    };
    let Ok(native) = mobile_native::Input::new(format) else {
        return ptr::null_mut();
    };
    let Ok(processor) = Processor::new(Some(control.clone()), Instant::now()) else {
        return ptr::null_mut();
    };
    let Ok((encoder, controls)) = source::channel(Profile::Opus5ms.config(), 100, false) else {
        return ptr::null_mut();
    };
    let summaries = control.summary_publisher();
    let routes = Arc::new(Snapshot::<Vec<MicRoute>>::default());
    let published = routes.clone();
    let started = Instant::now();
    controls.set_packet_writer(Arc::new(move |packet| {
        // Keep the protocol burst monotonic when native hardware is rebuilt
        // under a receiver lease that still owns the same M1 stream.
        let mut packet = *packet;
        packet.burst = (generation << 32) | packet.burst.min(u64::from(u32::MAX));
        let routes = published.load();
        let mut sent = false;
        let mut bytes = [0; media::FRAME_CAPACITY];
        for route in routes.iter() {
            if let Ok(len) = media::encode_kind(media::Kind::Microphone, &packet, route.stream, started, &mut bytes) {
                sent |= route.sender.post(&bytes[..len]).is_ok();
            }
        }
        sent
    }));
    let input = Arc::new(OinkAudioInput {
        format,
        native,
        control,
        processor: UnsafeCell::new(processor),
        scratch: UnsafeCell::new([0.0; MAX_INPUT_SAMPLES]),
        pending: UnsafeCell::new(Pending::default()),
        resampler: UnsafeCell::new(Resampler::new(rate)),
        mic_encoder: UnsafeCell::new(encoder),
        mic_controls: Mutex::new(controls),
        routes,
        capture: AtomicUsize::new(0),
        capture_context: AtomicPtr::new(ptr::null_mut()),
        aec_enabled: AtomicBool::new(true),
        local_mic: AtomicBool::new(false),
        local_exit: AtomicBool::new(false),
        aec_blocks: AtomicU64::new(0),
        aec_bypass: AtomicU64::new(0),
        aec_deadline_misses: AtomicU64::new(0),
        aec_waker: OnceLock::new(),
        aec_signalled: AtomicBool::new(false),
        summaries: UnsafeCell::new(summaries),
        callbacks: AtomicU64::new(0),
        host_unknown: AtomicU64::new(0),
    });
    handles().lock().expect("audio handle registry").insert(
        Arc::as_ptr(&input) as u64,
        RegisteredAudio::Input(Arc::downgrade(&input)),
    );
    Arc::into_raw(input).cast_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_output_set_renderer(
    output: *mut OinkAudioOutput,
    owner: *mut c_void,
    callback: Option<RenderCallback>,
) {
    if output.is_null() {
        return;
    }
    // SAFETY: null was rejected above; the owner thread keeps this handle
    // alive while changing renderer registration.
    let output = unsafe { &*output };
    output.render_context.store(
        if callback.is_some() { owner } else { ptr::null_mut() },
        Ordering::Release,
    );
    output
        .render
        .store(callback.map_or(0, |callback| callback as usize), Ordering::Release);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_input_set_capture(
    input: *mut OinkAudioInput,
    callback: Option<CaptureCallback>,
    context: *mut c_void,
) {
    if input.is_null() {
        return;
    }
    // SAFETY: null was rejected above; the owner thread keeps this handle
    // alive while changing capture registration.
    let input = unsafe { &*input };
    input.capture_context.store(
        if callback.is_some() { context } else { ptr::null_mut() },
        Ordering::Release,
    );
    input
        .capture
        .store(callback.map_or(0, |callback| callback as usize), Ordering::Release);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_input_set_aec(
    input: *mut OinkAudioInput,
    enabled: u8,
    local_mic: u8,
    local_exit: u8,
) {
    if input.is_null() {
        return;
    }
    // SAFETY: null was rejected above and the caller owns the live input
    // handle for the duration of this atomic policy update.
    let input = unsafe { &*input };
    input.aec_enabled.store(enabled != 0, Ordering::Release);
    input.local_mic.store(local_mic != 0, Ordering::Release);
    input.local_exit.store(local_exit != 0, Ordering::Release);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_output_pull(output: *mut OinkAudioOutput, samples: *mut f32, frames: u32) -> u8 {
    // SAFETY: this compatibility entry point has the same pointer and frame
    // preconditions as `oink_audio_output_pull_at`.
    unsafe { oink_audio_output_pull_at(output, samples, frames, 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_output_pull_at(
    output: *mut OinkAudioOutput,
    samples: *mut f32,
    frames: u32,
    host_ns: u64,
) -> u8 {
    if output.is_null() || samples.is_null() {
        return 0;
    }
    // SAFETY: null was rejected above; the native owner keeps output alive
    // until callback quiescence before calling this function.
    let output = unsafe { &*output };
    let count = frames as usize * output.native.format().channels as usize;
    if frames as usize > MAX_FRAMES {
        return 0;
    }
    // SAFETY: native adapters provide a buffer for exactly frames*channels.
    let samples = unsafe { slice::from_raw_parts_mut(samples, count) };
    output.callbacks.fetch_add(1, Ordering::Relaxed);
    if host_ns == 0 {
        output.host_unknown.fetch_add(1, Ordering::Relaxed);
    }
    output.pull(samples, frames as usize, host_ns);
    output.native.active() as u8
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_input_push(
    input: *mut OinkAudioInput,
    samples: *const f32,
    frames: u32,
    host_ns: u64,
) -> u32 {
    if input.is_null() || samples.is_null() {
        return 0;
    }
    // SAFETY: null was rejected above; the native owner keeps input alive
    // until callback quiescence before calling this function.
    let input = unsafe { &*input };
    let count = frames as usize * input.format.channels as usize;
    if frames as usize > MAX_FRAMES {
        return 0;
    }
    // SAFETY: native adapters provide a readable interleaved buffer.
    let samples = unsafe { slice::from_raw_parts(samples, count) };
    input.callbacks.fetch_add(1, Ordering::Relaxed);
    if host_ns == 0 {
        input.host_unknown.fetch_add(1, Ordering::Relaxed);
    }
    input.native.push(samples, frames as usize, host_ns) as u32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_input_process(input: *mut OinkAudioInput) {
    if input.is_null() {
        return;
    }
    // SAFETY: null was rejected above and the owner serializes process with
    // destruction through the native worker lifecycle.
    unsafe { &*input }.process();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_output_revoke(output: *mut OinkAudioOutput) {
    if !output.is_null() {
        // SAFETY: null was rejected and revoke is the first step of the
        // owner shutdown barrier, before the handle can be destroyed.
        unsafe { &*output }.native.revoke();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_input_revoke(input: *mut OinkAudioInput) {
    if !input.is_null() {
        // SAFETY: null was rejected and revoke is the first step of the
        // owner shutdown barrier, before the handle can be destroyed.
        unsafe { &*input }.native.revoke();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_output_destroy(output: *mut OinkAudioOutput) {
    if !output.is_null() {
        handles()
            .lock()
            .expect("audio handle registry")
            .remove(&(output as u64));
        // SAFETY: native owns exactly one Arc reference returned by create and
        // has stopped callbacks before releasing it.
        unsafe { Arc::from_raw(output) }.native.revoke();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn oink_audio_input_destroy(input: *mut OinkAudioInput) {
    if !input.is_null() {
        handles().lock().expect("audio handle registry").remove(&(input as u64));
        // SAFETY: native owns exactly one Arc reference returned by create and
        // has stopped callbacks before releasing it.
        unsafe { Arc::from_raw(input) }.native.revoke();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static RENDERED: AtomicUsize = AtomicUsize::new(0);
    static CAPTURED: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn render(_: *mut c_void, samples: *mut f32, frames: u32, _: u64) {
        RENDERED.fetch_add(frames as usize, Ordering::Relaxed);
        // SAFETY: the test passes the 2-channel buffer allocated for `frames`.
        unsafe { slice::from_raw_parts_mut(samples, frames as usize * 2).fill(0.25) };
    }

    unsafe extern "C" fn capture(_: *mut c_void, samples: *const f32, frames: u32, host_ns: u64) {
        assert!(!samples.is_null());
        let preceding = CAPTURED.fetch_add(frames as usize, Ordering::Relaxed);
        assert_eq!(host_ns, 2_000 + preceding as u64 * 1_000_000_000 / 48_000);
    }

    #[test]
    fn output_taps_final_rendered_pcm_and_timestamped_pull() {
        // SAFETY: constructor has no pointer preconditions.
        let output = unsafe { oink_audio_output_create(48_000, 2, 7) };
        assert!(!output.is_null());
        // SAFETY: `output` was returned by the constructor and remains live.
        unsafe { oink_audio_output_set_renderer(output, ptr::null_mut(), Some(render)) };
        let mut pcm = [0.0; 480];
        assert_eq!(
            // SAFETY: `pcm` has exactly 240 frames of two-channel storage.
            unsafe { oink_audio_output_pull_at(output, pcm.as_mut_ptr(), 240, 1_000) },
            1
        );
        assert_eq!(RENDERED.load(Ordering::Relaxed), 240);
        assert!(pcm.iter().all(|sample| *sample == 0.25));
        // SAFETY: this is the unique handle returned above and callbacks have
        // completed before test teardown.
        unsafe { oink_audio_output_destroy(output) };
    }

    unsafe extern "C" fn quiet(_: *mut c_void, samples: *mut f32, frames: u32, _: u64) {
        // SAFETY: the test passes the 2-channel buffer allocated for `frames`.
        unsafe { slice::from_raw_parts_mut(samples, frames as usize * 2).fill(0.01) };
    }

    #[test]
    fn playout_gain_applies_the_iphone_exit_volume_to_the_played_pcm() {
        // Off (Android), unity (the default 50), silence (0) and maximum (100).
        for (percent, expected) in [
            (None, 0.01),
            (Some(100), 0.01),
            (Some(0), 0.0),
            (Some(200), 0.01 * forward_volume::amplitude(200)),
        ] {
            // SAFETY: constructor has no pointer preconditions.
            let output = unsafe { oink_audio_output_create(48_000, 2, 4101) };
            assert!(!output.is_null());
            // SAFETY: `output` was returned by the constructor and remains live.
            unsafe { oink_audio_output_set_renderer(output, ptr::null_mut(), Some(quiet)) };
            NativeAudio::lookup(output as u64).unwrap().set_playout_gain(percent);
            let mut pcm = [0.0; 960];
            // Past the 1 ms lookahead and the 5 ms ramp.
            for _ in 0..3 {
                // SAFETY: `pcm` has exactly 480 frames of two-channel storage.
                unsafe { oink_audio_output_pull_at(output, pcm.as_mut_ptr(), 480, 0) };
            }
            assert!(
                pcm.iter().all(|sample| (*sample - expected).abs() < 1e-6),
                "{percent:?}: {} != {expected}",
                pcm[0]
            );
            if percent.is_none_or(|percent| percent == 100) {
                assert!(pcm.iter().all(|sample| *sample == 0.01), "unity is bit-exact");
            }
            // SAFETY: this is the unique handle returned above and callbacks have
            // completed before test teardown.
            unsafe { oink_audio_output_destroy(output) };
        }
    }

    #[test]
    fn input_push_process_delivers_bounded_worker_audio_to_core() {
        CAPTURED.store(0, Ordering::Relaxed);
        // SAFETY: constructor has no pointer preconditions.
        let input = unsafe { oink_audio_input_create(48_000, 1, 8) };
        assert!(!input.is_null());
        // SAFETY: `input` was returned by the constructor and remains live.
        unsafe { oink_audio_input_set_capture(input, Some(capture), ptr::null_mut()) };
        let pcm = vec![0.125; 960];
        assert_eq!(
            // SAFETY: `pcm` is a live readable mono buffer with the declared
            // frame count.
            unsafe { oink_audio_input_push(input, pcm.as_ptr(), pcm.len() as u32, 2_000) },
            pcm.len() as u32
        );
        // SAFETY: `input` remains live and no concurrent process call exists.
        unsafe { oink_audio_input_process(input) };
        assert_eq!(CAPTURED.load(Ordering::Relaxed), 960);
        // SAFETY: this is the unique handle returned above and the worker has
        // finished processing the pushed block.
        unsafe { oink_audio_input_destroy(input) };
    }

    /// Android AAudio without a FAST input path delivers 20 ms bursts, so the
    /// capture worker hands its blocks to the encoder in bursts just over
    /// 20 ms apart. That cadence is not lost audio and must not change what
    /// the receiving PC decodes.
    #[test]
    fn native_burst_cadence_does_not_change_encoded_microphone_audio() {
        use oink_network::link::{LinkId, QueueSpec, testing::datagram_pair};
        use oink_protocol::frame::{StreamKind, StreamTag};
        use std::time::Duration;
        const STREAM: u64 = 7;
        const BURST: usize = 960;
        const CALLBACK: usize = 240;
        let signal: Vec<f32> = (0..BURST * 8)
            .map(|i| 0.25 * (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin())
            .collect();
        let encode = |gap: Duration, generation: u64| -> Vec<Vec<u8>> {
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
            // SAFETY: constructor has no pointer preconditions.
            let input = unsafe { oink_audio_input_create(48_000, 1, generation) };
            assert!(!input.is_null());
            // SAFETY: `input` was returned by the constructor and remains live.
            unsafe { oink_audio_input_set_aec(input, 0, 0, 0) };
            // SAFETY: as above; this test is the only worker.
            unsafe { &*input }
                .routes
                .publish(vec![MicRoute { stream: STREAM, sender }]);
            for (index, burst) in signal.as_chunks::<BURST>().0.iter().enumerate() {
                if index > 0 {
                    std::thread::sleep(gap);
                }
                for (part, chunk) in burst.as_chunks::<CALLBACK>().0.iter().enumerate() {
                    let frame = (index * BURST + part * CALLBACK) as u64;
                    let host_ns = 1_000_000 + frame * 1_000_000_000 / 48_000;
                    assert_eq!(
                        // SAFETY: `chunk` is a live mono buffer of CALLBACK frames.
                        unsafe { oink_audio_input_push(input, chunk.as_ptr(), CALLBACK as u32, host_ns) },
                        CALLBACK as u32
                    );
                }
                // SAFETY: `input` remains live and no concurrent process call exists.
                unsafe { oink_audio_input_process(input) };
            }
            let mut payloads = Vec::new();
            while let Ok(Some(bytes)) = receiver.take() {
                if let Ok(media::Media::Packet { payload, .. }) =
                    media::decode_kind(media::Kind::Microphone, &bytes, STREAM, Profile::Opus5ms)
                {
                    payloads.push(payload.to_vec());
                }
            }
            // SAFETY: this is the unique handle returned above; no worker runs.
            unsafe { oink_audio_input_destroy(input) };
            payloads
        };
        let steady = encode(Duration::ZERO, 51);
        let bursty = encode(Duration::from_millis(21), 52);
        // 8 bursts of 20 ms are 32 Opus frames of 5 ms.
        assert_eq!(steady.len(), 32);
        assert_eq!(bursty.len(), steady.len());
        assert_eq!(
            steady.iter().zip(&bursty).position(|(a, b)| a != b),
            None,
            "the 20 ms burst cadence changed the encoded microphone audio"
        );
    }

    /// Generation pairing: an output and an input meet in one Case A control
    /// only when they share a native generation. An input whose output runs
    /// in another generation has no reference, so its AEC never engages (the
    /// media owner logs `aec_pairing` with `paired=false` for it).
    #[test]
    fn case_a_pairs_only_handles_of_one_generation() {
        // SAFETY: constructors have no pointer preconditions.
        let (other_output, input) = unsafe {
            (
                oink_audio_output_create(48_000, 2, 71),
                oink_audio_input_create(48_000, 1, 72),
            )
        };
        assert!(!other_output.is_null() && !input.is_null());
        let other = NativeAudio::lookup(other_output as u64).unwrap();
        let native = NativeAudio::lookup(input as u64).unwrap();
        assert!(!other.shares_case_a_control(&native));
        native.set_aec(true, true, true);
        let mut pcm = [0.0f32; BLOCK * 2];
        for block in 0..20_u64 {
            let host_ns = 1_000_000_000 + block * 10_000_000;
            // SAFETY: live handles; `pcm` holds BLOCK two-channel frames and
            // its first BLOCK samples are a readable mono block.
            unsafe {
                oink_audio_output_pull_at(other_output, pcm.as_mut_ptr(), BLOCK as u32, host_ns);
                oink_audio_input_push(input, pcm.as_ptr(), BLOCK as u32, host_ns);
                oink_audio_input_process(input);
            }
            native.service_aec();
        }
        let NativeAudio::Input(handle) = &native else {
            unreachable!()
        };
        assert_eq!(
            handle.control.engines_built(),
            0,
            "no reference in its generation: no engine"
        );
        assert_eq!(native.take_host_time_counts(), (20, 0));
        // The output of the input's own generation pairs with it.
        // SAFETY: constructor has no pointer preconditions.
        let output = unsafe { oink_audio_output_create(48_000, 2, 72) };
        let paired = NativeAudio::lookup(output as u64).unwrap();
        assert!(paired.shares_case_a_control(&native));
        drop((other, native, paired));
        // SAFETY: unique handles returned above; no callback runs.
        unsafe {
            oink_audio_input_destroy(input);
            oink_audio_output_destroy(output);
            oink_audio_output_destroy(other_output);
        }
    }

    struct FarEnd {
        signal: Vec<f32>,
        cursor: usize,
        played: Vec<f32>,
    }

    unsafe extern "C" fn far_render(context: *mut c_void, samples: *mut f32, frames: u32, _: u64) {
        // SAFETY: the test owns this FarEnd for the whole pull loop.
        let far = unsafe { &mut *context.cast::<FarEnd>() };
        // SAFETY: two-channel output buffer of `frames` frames.
        let samples = unsafe { slice::from_raw_parts_mut(samples, frames as usize * 2) };
        for frame in samples.as_chunks_mut::<2>().0 {
            let value = far.signal.get(far.cursor).copied().unwrap_or(0.0);
            *frame = [value, -0.4 * value];
            far.played.push(value);
            far.cursor += 1;
        }
    }

    unsafe extern "C" fn collect(context: *mut c_void, samples: *const f32, frames: u32, _: u64) {
        // SAFETY: the test owns this Vec for the whole capture loop.
        let collected = unsafe { &mut *context.cast::<Vec<f32>>() };
        // SAFETY: mono 48 kHz worker output of `frames` frames.
        collected.extend_from_slice(unsafe { slice::from_raw_parts(samples, frames as usize) });
    }

    /// The phone's output pull and microphone push/process entry points, with
    /// the media owner serving AEC, cancel an echo of the pulled output.
    #[test]
    fn mobile_case_a_cancels_the_echo_of_pulled_output_with_one_engine() {
        const BASE_NS: u64 = 5_000_000_000;
        const ACOUSTIC_FRAMES: usize = 96;
        let seconds = 14;
        let mut seed = 23_u32;
        let mut signal = vec![0.0f32; 48_000 * (seconds + 1)];
        for i in 1..signal.len() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            signal[i] = 0.7 * signal[i - 1] + 0.08 * ((seed >> 8) as f32 / 8388608.0 - 1.0);
        }
        let far = Box::into_raw(Box::new(FarEnd {
            signal,
            cursor: 0,
            played: Vec::new(),
        }));
        let processed = Box::into_raw(Box::new(Vec::<f32>::new()));
        // SAFETY: constructors have no pointer preconditions; both directions
        // share generation 41 and therefore one Case A control.
        let (output, input) = unsafe {
            (
                oink_audio_output_create(48_000, 2, 41),
                oink_audio_input_create(48_000, 1, 41),
            )
        };
        assert!(!output.is_null() && !input.is_null());
        // SAFETY: live handles; contexts outlive every callback below.
        unsafe {
            oink_audio_output_set_renderer(output, far.cast(), Some(far_render));
            oink_audio_input_set_capture(input, Some(collect), processed.cast());
        }
        let native = NativeAudio::lookup(input as u64).unwrap();
        let NativeAudio::Input(handle) = &native else {
            unreachable!()
        };
        // SAFETY: no worker runs yet; the test is the processor's only user.
        unsafe { &mut *handle.processor.get() }.set_test_work_budget(std::time::Duration::MAX);
        // Media owner publish: Case A eligible (local mic sending, local exit).
        native.set_aec(true, true, true);
        let mut mic = Vec::new();
        let mut rendered = 0usize;
        for block in 0..48_000 * seconds / BLOCK {
            // Output runs 20 ms ahead of capture; frame f sounds at BASE + f.
            while rendered < (block + 3) * BLOCK {
                let host_ns = BASE_NS + rendered as u64 * 1_000_000_000 / 48_000;
                let mut pcm = [0.0f32; BLOCK * 2];
                // SAFETY: `pcm` holds BLOCK two-channel frames.
                unsafe { oink_audio_output_pull_at(output, pcm.as_mut_ptr(), BLOCK as u32, host_ns) };
                rendered += BLOCK;
            }
            // SAFETY: the pull loop is not running concurrently.
            let played = unsafe { &(*far).played };
            let samples: Vec<f32> = (block * BLOCK..(block + 1) * BLOCK)
                .map(|c| c.checked_sub(ACOUSTIC_FRAMES).map_or(0.0, |f| 0.5 * played[f]))
                .collect();
            mic.extend_from_slice(&samples);
            let host_ns = BASE_NS + (block * BLOCK) as u64 * 1_000_000_000 / 48_000;
            // SAFETY: live input handle and a readable mono block.
            unsafe {
                oink_audio_input_push(input, samples.as_ptr(), BLOCK as u32, host_ns);
                oink_audio_input_process(input);
            }
            if block % 2 == 0 {
                // The media owner's pass (`service_audio`).
                native.service_aec();
            }
        }
        // SAFETY: the capture loop has finished.
        let processed_samples = unsafe { &*processed };
        let tail = 48_000 * 4;
        let energy = |samples: &[f32]| samples.iter().map(|s| f64::from(s * s)).sum::<f64>();
        let raw = energy(&mic[mic.len() - tail..]);
        let out = energy(&processed_samples[processed_samples.len() - tail..]);
        assert!(raw > 1.0, "fixture must contain echo: {raw}");
        let attenuation = 10.0 * (raw / out).log10();
        eprintln!(
            "mobile product path attenuation={attenuation:.2} dB engines={}",
            handle.control.engines_built()
        );
        assert!(attenuation > 10.0, "attenuation={attenuation}");
        assert_eq!(
            handle.control.engines_built(),
            1,
            "an unchanged configuration builds one engine"
        );
        assert!(!handle.control.reset_pending());
        drop(native);
        // SAFETY: unique handles returned above; callbacks have completed.
        unsafe {
            oink_audio_input_destroy(input);
            oink_audio_output_destroy(output);
            drop(Box::from_raw(far));
            drop(Box::from_raw(processed));
        }
    }
}
