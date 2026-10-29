//! Event-driven shared WASAPI rendering. No endpoint controls are changed.
//!
//! `Some(0)` requests the driver's minimum IAudioClient3 engine period;
//! `Some(n)` rounds n up to a supported fundamental period. `None` deliberately
//! retains the legacy default-buffer baseline. A failed IAudioClient3 request
//! falls back to a fresh IAudioClient and records the original HRESULT.

use crate::wasapi_timing::observe_shared_render;
use crate::windows_timer::TimerResolution;
use serde::Serialize;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, IAudioClient, IAudioClient3, IAudioClock,
    IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject,
};
use windows::core::{GUID, Interface, PCWSTR, w};

#[derive(Clone, Copy, Debug)]
pub struct NativeOutputTiming {
    pub period_frames: u32,
    pub native_gap_frames: Option<u64>,
    pub callback_at: Instant,
    /// Padding-based first-frame engine queue estimate, validated by a live
    /// audio clock. Not a measured DAC/speaker timestamp. None during startup,
    /// zero padding (next engine handoff is unknown), stale clock, or
    /// when the estimate is already past. See info().timing_method.
    pub playback_at: Option<Instant>,
    pub sample_rate: u32,
    pub channels: u16,
    pub padding_frames: u32,
    pub buffer_frames: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct NativeOutputInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: String,
    pub requested_period_frames: Option<u32>,
    pub minimum_period_frames: Option<u32>,
    pub fundamental_period_frames: Option<u32>,
    pub default_period_frames: u32,
    pub maximum_period_frames: Option<u32>,
    /// Queried engine period, or legacy GetDevicePeriod if unavailable.
    pub actual_period_frames: u32,
    pub actual_period_source: String,
    pub buffer_frames: u32,
    /// Low-latency mode intentionally leaves excess driver capacity unused.
    pub queue_target_frames: u32,
    /// Driver-reported maximum stream latency, not added to the padding-based
    /// estimate (doing so can double-count engine buffering).
    pub stream_latency_ms: Option<f64>,
    pub backend: String,
    pub fallback_reason: Option<String>,
    pub mmcss_enabled: bool,
    pub mmcss_error: Option<String>,
    /// WinMM request, not measured/guaranteed scheduler resolution.
    pub timer_resolution_requested_ms: Option<u32>,
    pub timer_resolution_request_succeeded: bool,
    pub timer_resolution_error: Option<String>,
    pub timing_method: String,
}

type RenderCallback = Box<dyn crate::playback::NativeRender>;

#[derive(Debug)]
pub struct NativePlayback {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    info: NativeOutputInfo,
}
impl NativePlayback {
    /// Callbacks run on the render thread: they must not block, allocate, or do
    /// I/O. Samples are interleaved float at the actual device rate/channels.
    pub fn start(
        endpoint_raw_id: &str,
        requested: Option<u32>,
        callback: RenderCallback,
        mut on_error: Box<dyn FnMut(String) + Send>,
    ) -> Result<Self, String> {
        if endpoint_raw_id.is_empty() || endpoint_raw_id.contains('\0') || endpoint_raw_id.len() > 4096 {
            return Err("invalid output endpoint ID".into());
        }
        let endpoint = endpoint_raw_id.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("oink-wasapi-render".into())
            .spawn(move || {
                if let Err(error) = run(
                    &endpoint,
                    requested,
                    callback,
                    &worker_stop,
                    &ready_tx,
                    on_error.as_mut(),
                ) {
                    let _ = ready_tx.try_send(Err(error.clone()));
                    on_error(error);
                }
            })
            .map_err(|e| format!("spawn WASAPI render: {e}"))?;
        match ready_rx.recv() {
            Ok(Ok(info)) => Ok(Self {
                stop,
                worker: Some(worker),
                info,
            }),
            result => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(match result {
                    Ok(Err(error)) => error,
                    _ => "WASAPI render worker exited before ready".into(),
                })
            }
        }
    }

    pub fn shutdown(&mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        true // this joined thread was the only callback executor
    }
    pub fn info(&self) -> NativeOutputInfo {
        self.info.clone()
    }
}
impl Drop for NativePlayback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn api_error(operation: &str, error: windows::core::Error) -> String {
    format!("{operation}: {error} (HRESULT 0x{:08X})", error.code().0 as u32)
}

struct Com;
impl Com {
    fn initialize() -> Result<Self, String> {
        // SAFETY: called once on this dedicated worker, paired by Drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.map_err(|e| api_error("CoInitializeEx", e))?;
        Ok(Self)
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: all COM interfaces have been dropped on this same thread.
        unsafe { CoUninitialize() }
    }
}
struct Event(HANDLE);
impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: uniquely owned event, no waits remain when dropped.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Mmcss(HANDLE);
impl Drop for Mmcss {
    fn drop(&mut self) {
        // SAFETY: restore only this thread's successful MMCSS registration.
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.0);
        }
    }
}
struct MixFormat(*mut WAVEFORMATEX);
impl Drop for MixFormat {
    fn drop(&mut self) {
        // SAFETY: GetMixFormat allocates with CoTaskMemAlloc; single owner.
        unsafe {
            CoTaskMemFree(Some(self.0.cast()));
        }
    }
}
struct Running(IAudioClient);
impl Drop for Running {
    fn drop(&mut self) {
        // SAFETY: owned live interface on the initialized COM worker.
        unsafe {
            let _ = self.0.Stop();
        }
    }
}

#[derive(Clone, Copy)]
enum Encoding {
    Float32,
    Pcm16,
}
fn encoding(format: &MixFormat) -> Result<Encoding, String> {
    // SAFETY: GetMixFormat returned the base header plus cbSize bytes.
    let base = unsafe { *format.0 };
    let tag = base.wFormatTag;
    let bits = base.wBitsPerSample;
    let subformat = if tag == 0xfffe {
        if usize::from(base.cbSize) < std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()
        {
            return Err("truncated WAVEFORMATEXTENSIBLE".into());
        }
        // SAFETY: cbSize checked; packed fields read by value, never referenced.
        let ext = unsafe { *format.0.cast::<WAVEFORMATEXTENSIBLE>() };
        // SAFETY: valid bits is the PCM member for supported subformats below.
        let valid_bits = unsafe { ext.Samples.wValidBitsPerSample };
        if valid_bits != 0 && valid_bits != bits {
            return Err(format!("unsupported output valid bits {valid_bits}/{bits}"));
        }
        ext.SubFormat
    } else {
        GUID::from_u128(((tag as u128) << 96) | 0x0000_0010_8000_00aa_0038_9b71)
    };
    let result = if bits == 32 && subformat == GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71) {
        Encoding::Float32
    } else if bits == 16 && subformat == GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71) {
        Encoding::Pcm16
    } else {
        return Err(format!(
            "unsupported WASAPI mix format: tag {tag:#x}, bits {bits}, subtype {subformat:?}"
        ));
    };
    if base.nChannels == 0
        || base.nChannels > 64
        || base.nSamplesPerSec == 0
        || u32::from(base.nBlockAlign) != u32::from(base.nChannels) * u32::from(bits / 8)
    {
        return Err("invalid WASAPI mix rate/channels/block alignment".into());
    }
    Ok(result)
}

fn aligned_period(requested: u32, minimum: u32, fundamental: u32, maximum: u32) -> Result<u32, String> {
    if minimum == 0 || fundamental == 0 || maximum < minimum {
        return Err("driver returned invalid shared engine periods".into());
    }
    let rounded = u64::from(requested.max(minimum)).div_ceil(u64::from(fundamental)) * u64::from(fundamental);
    let largest = maximum / fundamental * fundamental;
    if largest < minimum {
        return Err("driver returned no aligned shared engine period".into());
    }
    Ok(rounded.min(u64::from(largest)) as u32)
}

fn frames_from_hns(hns: i64, rate: u32) -> u32 {
    ((hns.max(0) as u64 * u64::from(rate)).div_ceil(10_000_000)).min(u64::from(u32::MAX)) as u32
}

fn run(
    endpoint: &str,
    requested: Option<u32>,
    mut callback: RenderCallback,
    stop: &AtomicBool,
    ready: &mpsc::SyncSender<Result<NativeOutputInfo, String>>,
    on_error: &mut dyn FnMut(String),
) -> Result<(), String> {
    let _com = Com::initialize()?;
    // SAFETY: COM apartment active for every interface's lifetime below.
    let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|e| api_error("create endpoint enumerator", e))?;
    let wide: Vec<u16> = endpoint.encode_utf16().chain(Some(0)).collect();
    // SAFETY: NUL-terminated endpoint ID valid throughout synchronous call.
    let device =
        unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }.map_err(|e| api_error("open output endpoint", e))?;
    let mut client = activate(&device)?;
    // SAFETY: active COM client; returned allocation is wrapped immediately.
    let format = MixFormat(unsafe { client.GetMixFormat() }.map_err(|e| api_error("GetMixFormat", e))?);
    if format.0.is_null() {
        return Err("GetMixFormat returned null".into());
    }
    let encoding = encoding(&format)?;
    // SAFETY: nonnull valid format checked above, copy packed values.
    let base = unsafe { *format.0 };
    let rate = base.nSamplesPerSec;
    let channels = base.nChannels;
    let mut default_hns = 0;
    // SAFETY: valid client and writable scalar; no exclusive minimum requested.
    unsafe { client.GetDevicePeriod(Some(&mut default_hns), None) }.map_err(|e| api_error("GetDevicePeriod", e))?;
    let mut info = NativeOutputInfo {
        sample_rate: rate,
        channels,
        sample_format: match encoding {
            Encoding::Float32 => "f32",
            Encoding::Pcm16 => "i16",
        }
        .into(),
        requested_period_frames: requested,
        minimum_period_frames: None,
        fundamental_period_frames: None,
        default_period_frames: frames_from_hns(default_hns, rate),
        maximum_period_frames: None,
        actual_period_frames: frames_from_hns(default_hns, rate),
        actual_period_source: "GetDevicePeriod_legacy_default".into(),
        buffer_frames: 0,
        queue_target_frames: 0,
        stream_latency_ms: None,
        backend: "IAudioClient_default_shared".into(),
        fallback_reason: None,
        mmcss_enabled: false,
        mmcss_error: None,
        timer_resolution_requested_ms: requested.map(|_| 1),
        timer_resolution_request_succeeded: false,
        timer_resolution_error: None,
        timing_method:
            "GetCurrentPadding_queue_estimate_validated_by_IAudioClock_QPC; zero_padding_unknown; excludes_unreported_hardware_latency; not_DAC_measurement; native_continuity_unknown"
                .into(),
    };
    let _timer_resolution = if requested.is_some() {
        match TimerResolution::request_1ms() {
            Ok(guard) => {
                info.timer_resolution_request_succeeded = true;
                Some(guard)
            }
            Err(error) => {
                info.timer_resolution_error = Some(error);
                None
            }
        }
    } else {
        None
    };
    let client3 = client
        .cast::<IAudioClient3>()
        .map_err(|e| api_error("QueryInterface IAudioClient3", e));
    let periods = client3.as_ref().map_err(Clone::clone).and_then(|c| {
        let (mut default, mut fundamental, mut minimum, mut maximum) = (0, 0, 0, 0);
        // SAFETY: mix format and output scalars remain valid for call.
        unsafe { c.GetSharedModeEnginePeriod(format.0, &mut default, &mut fundamental, &mut minimum, &mut maximum) }
            .map_err(|e| api_error("GetSharedModeEnginePeriod", e))?;
        info.default_period_frames = default;
        info.minimum_period_frames = Some(minimum);
        info.fundamental_period_frames = Some(fundamental);
        info.maximum_period_frames = Some(maximum);
        Ok((minimum, fundamental, maximum))
    });
    let mut low_latency_initialized = false;
    if let Some(target) = requested {
        let attempt = periods.and_then(|(minimum, fundamental, maximum)| {
            let period = aligned_period(target, minimum, fundamental, maximum)?;
            let c = client3.as_ref().map_err(Clone::clone)?;
            // SAFETY: selected supported aligned shared period, valid mix format.
            unsafe { c.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, period, format.0, None) }
                .map_err(|e| api_error("InitializeSharedAudioStream", e))?;
            info.actual_period_frames = period;
            info.actual_period_source = "InitializeSharedAudioStream_requested_period".into();
            Ok(())
        });
        match attempt {
            Ok(()) => {
                low_latency_initialized = true;
                info.backend = "IAudioClient3_shared".into();
            }
            Err(error) => {
                info.fallback_reason = Some(error);
                // A failed Initialize can leave a client unusable; start fresh.
                client = activate(&device)?;
            }
        }
    }
    if !low_latency_initialized {
        // SAFETY: uninitialized client, default shared duration and periodicity.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                0,
                0,
                format.0,
                None,
            )
        }
        .map_err(|e| api_error("Initialize legacy shared fallback", e))?;
    }
    if let Ok(c) = client.cast::<IAudioClient3>() {
        let mut current_format = std::ptr::null_mut();
        let mut period = 0;
        // SAFETY: valid output pointers; API allocates format with COM allocator.
        let current = unsafe { c.GetCurrentSharedModeEnginePeriod(&mut current_format, &mut period) };
        let _current_format = MixFormat(current_format);
        if current.is_ok() && period != 0 {
            info.actual_period_frames = period;
            info.actual_period_source = "GetCurrentSharedModeEnginePeriod".into();
        }
    }
    // SAFETY: initialized client; queries do not modify endpoint controls.
    info.buffer_frames = unsafe { client.GetBufferSize() }.map_err(|e| api_error("GetBufferSize", e))?;
    // SAFETY: initialized client; latency is explicitly reported separately.
    info.stream_latency_ms = unsafe { client.GetStreamLatency() }.ok().map(|v| v as f64 / 10_000.0);
    if info.buffer_frames == 0 || info.actual_period_frames == 0 {
        return Err("driver returned zero render buffer/period".into());
    }
    info.queue_target_frames = if requested.is_some() {
        info.actual_period_frames.min(info.buffer_frames)
    } else {
        info.buffer_frames
    };
    // SAFETY: unnamed auto-reset event, owned by this worker.
    let event = Event(unsafe { CreateEventW(None, false, false, None) }.map_err(|e| api_error("CreateEventW", e))?);
    // SAFETY: event lives until after rendering stops.
    unsafe { client.SetEventHandle(event.0) }.map_err(|e| api_error("SetEventHandle", e))?;
    // SAFETY: initialized render client provides services, owned on same thread.
    let render: IAudioRenderClient = unsafe { client.GetService() }.map_err(|e| api_error("GetService render", e))?;
    // SAFETY: optional clock service queried from initialized client.
    let clock: Option<IAudioClock> = unsafe { client.GetService() }.ok();
    let qpc = Qpc::new();
    let mut scratch = vec![0.0f32; info.queue_target_frames as usize * channels as usize];
    // All owning metadata and scratch are prepared before MMCSS. One real
    // registration covers initial pull/prime and every later event callback.
    info.mmcss_enabled = true;
    let ready_report = info.clone();
    let mut task_index = 0;
    // SAFETY: applies only to this dedicated thread; guard reverts before errors.
    let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }
        .map_err(|error| api_error("register render MMCSS", error))?;
    let mmcss = Mmcss(handle);
    let mut ready_published = false;
    let result = (|| -> Result<(), RenderFailure> {
        render_once(
            &client,
            &render,
            None,
            qpc.as_ref(),
            &info,
            encoding,
            &mut scratch,
            &mut callback,
        )?;
        // SAFETY: live initialized client with a primed render buffer.
        let status = unsafe { (Interface::vtable(&client).Start)(Interface::as_raw(&client)) };
        if status.is_err() {
            return Err(RenderFailure("Start render", status.0));
        }
        if ready.send(Ok(ready_report)).is_err() {
            return Err(RenderFailure("render startup caller disconnected", -1));
        }
        ready_published = true;
        while !stop.load(Ordering::Acquire) {
            // SAFETY: event lives until after client Stop.
            match unsafe { WaitForSingleObject(event.0, 50) } {
                WAIT_OBJECT_0 => render_once(
                    &client,
                    &render,
                    clock.as_ref(),
                    qpc.as_ref(),
                    &info,
                    encoding,
                    &mut scratch,
                    &mut callback,
                )?,
                WAIT_TIMEOUT => continue,
                _ => return Err(RenderFailure("wait render event", -1)),
            }
        }
        Ok(())
    })();
    drop(mmcss);
    let _running = Running(client.clone());
    let result = result.map_err(RenderFailure::message);
    if let Err(error) = result {
        if !ready_published {
            return Err(error);
        }
        // This thread is ordinary again. Publish failure before waiting for the
        // owner to retire callback-owned DSP; only then release native services.
        on_error(error);
        while !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
    }
    Ok(())
}

fn activate(device: &IMMDevice) -> Result<IAudioClient, String> {
    // SAFETY: caller owns a COM apartment and keeps returned client there.
    unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| api_error("activate output IAudioClient", e))
}

#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "QueryPerformanceCounter"]
    fn query_counter(counter: *mut i64) -> i32;
}
struct Qpc {
    frequency: i64,
}
impl Qpc {
    fn new() -> Option<Self> {
        let mut frequency = 0;
        // SAFETY: valid writable scalar.
        unsafe { QueryPerformanceFrequency(&mut frequency) }.ok()?;
        (frequency > 0).then_some(Self { frequency })
    }
    fn clock_instant(&self, clock: &IAudioClock) -> Option<Instant> {
        let (mut position, mut clock_hns, mut counter) = (0, 0, 0);
        // Preserve S_FALSE: the generated GetPosition wrapper folds it into
        // Ok(()), although WASAPI says the reading then has reduced accuracy.
        // SAFETY: live clock interface, its own vtable, and valid output scalars.
        let status =
            unsafe { (Interface::vtable(clock).GetPosition)(Interface::as_raw(clock), &mut position, &mut clock_hns) };
        if status.0 != 0 || position == 0 || clock_hns == 0 {
            return None;
        }
        let before = Instant::now();
        // SAFETY: valid writable scalar, same system QPC source as IAudioClock.
        if unsafe { query_counter(&mut counter) } == 0 {
            return None;
        }
        let after = Instant::now();
        let instant = before + after.duration_since(before) / 2;
        let now_hns = counter as i128 * 10_000_000 / self.frequency as i128;
        let delta_hns = now_hns - clock_hns as i128;
        // Reject stopped/invalid clock timestamps instead of inventing timing.
        if !(0..=10_000_000).contains(&delta_hns) {
            return None;
        }
        instant.checked_sub(Duration::from_nanos(delta_hns as u64 * 100))
    }
}

#[derive(Clone, Copy)]
struct RenderFailure(&'static str, i32);
impl RenderFailure {
    fn message(self) -> String {
        format!("{}: HRESULT 0x{:08X}", self.0, self.1 as u32)
    }
}
#[allow(clippy::too_many_arguments)]
fn render_once(
    client: &IAudioClient,
    render: &IAudioRenderClient,
    clock: Option<&IAudioClock>,
    qpc: Option<&Qpc>,
    info: &NativeOutputInfo,
    encoding: Encoding,
    scratch: &mut [f32],
    callback: &mut RenderCallback,
) -> Result<(), RenderFailure> {
    let began = Instant::now();
    let clock_at = clock.zip(qpc).and_then(|(clock, qpc)| qpc.clock_instant(clock));
    let padding_at = Instant::now();
    // SAFETY: initialized client on the owning render thread.
    let mut padding = 0;
    // SAFETY: live client vtable and writable scalar.
    let status = unsafe { (Interface::vtable(client).GetCurrentPadding)(Interface::as_raw(client), &mut padding) };
    if status.is_err() {
        return Err(RenderFailure("GetCurrentPadding", status.0));
    }
    let padding_at = padding_at + padding_at.elapsed() / 2;
    let frames = info
        .queue_target_frames
        .saturating_sub(padding)
        .min(info.buffer_frames.saturating_sub(padding));
    if frames == 0 {
        return Ok(());
    }
    let samples = frames as usize * info.channels as usize;
    scratch[..samples].fill(0.0);
    let callback_at = Instant::now();
    let observation = observe_shared_render(clock_at, padding_at, callback_at, padding, info.sample_rate);
    callback.fill(
        &mut scratch[..samples],
        NativeOutputTiming {
            period_frames: info.actual_period_frames,
            native_gap_frames: observation.native_gap_frames,
            callback_at,
            playback_at: observation.playback_at,
            sample_rate: info.sample_rate,
            channels: info.channels,
            padding_frames: padding,
            buffer_frames: info.buffer_frames,
        },
    );
    // SAFETY: frames does not exceed available buffer capacity. Buffer is held
    // only during the copy/conversion, not while arbitrary user callback runs.
    let mut destination = std::ptr::null_mut();
    // SAFETY: live render vtable and writable pointer, requested size checked.
    let status = unsafe { (Interface::vtable(render).GetBuffer)(Interface::as_raw(render), frames, &mut destination) };
    if status.is_err() {
        return Err(RenderFailure("GetBuffer render", status.0));
    }
    if destination.is_null() {
        return Err(RenderFailure("render GetBuffer returned null", -1));
    }
    // SAFETY: WASAPI supplies frames * nBlockAlign writable bytes until release;
    // write_unaligned supports both sample formats regardless of alignment.
    unsafe {
        match encoding {
            Encoding::Float32 => {
                for (i, sample) in scratch[..samples].iter().enumerate() {
                    destination.cast::<f32>().add(i).write_unaligned(if sample.is_finite() {
                        sample.clamp(-1.0, 1.0)
                    } else {
                        0.0
                    });
                }
            }
            Encoding::Pcm16 => {
                for (i, sample) in scratch[..samples].iter().enumerate() {
                    let value = if sample.is_finite() {
                        (sample.clamp(-1.0, 1.0) * 32768.0).round().clamp(-32768.0, 32767.0) as i16
                    } else {
                        0
                    };
                    destination.cast::<i16>().add(i).write_unaligned(value);
                }
            }
        }
        let status = (Interface::vtable(render).ReleaseBuffer)(Interface::as_raw(render), frames, 0);
        if status.is_err() {
            return Err(RenderFailure("ReleaseBuffer render", status.0));
        }
    }
    callback.complete(began.elapsed());
    Ok(())
}
