//! Default-render-endpoint WASAPI loopback capture.
//!
//! This backend never changes endpoint volume or the default device. Source
//! muting is opt-in and requires a durable recovery directory. Only the mute bit
//! is changed, with an event-context GUID and external-change ownership tracking.
//! Endpoint loopback includes this process: the coordinator must stop capture
//! before starting playback on an exit device. Exclusive-mode and protected audio
//! are not guaranteed to be capturable. Muted-loopback preservation was tested
//! on the user-authorized Windows headphones endpoint; it is not a guarantee for
//! arbitrary Windows drivers. See `recovery` for crash/user-intent limitations.

use crate::CaptureInfo;
use crate::recovery::{EndpointBackend, EndpointState, Journal, RecoveryOutcome, RecoveryReport};
#[path = "windows_capture.rs"]
mod capture_engine;
#[path = "windows_source_devices.rs"]
mod source_devices;
use capture_engine::{OpenedCapture, QpcClock};
use std::path::{Path, PathBuf};

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
    AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE_ACTIVE,
    Endpoints::{IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl},
    IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateGuid, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

/// Owns the capture worker. Packets are interleaved stereo f32 at 48,000 Hz.
///
/// Packet sizes follow the system audio engine period, not Opus framing. A full
/// consumer queue drops a packet instead of blocking the audio engine; consumers
/// must drain promptly and impose their own bounded framing queue.
#[derive(Debug)]
pub struct Capture {
    forwarding_stop: crate::media_pause::CaptureStop,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    local_controls_changed: Arc<AtomicBool>,
    endpoint_changes: Arc<source_devices::Changes>,
    finished: Arc<AtomicBool>,
    restart: Arc<Mutex<Option<String>>>,
    cleanup_error: Arc<Mutex<Option<String>>>,
    worker: Option<JoinHandle<()>>,
    muted_owned: Arc<AtomicBool>,
    info: CaptureInfo,
}

impl Capture {
    pub fn default_output_changed(&self) -> bool {
        self.endpoint_changes.default.load(Ordering::Acquire)
    }
    pub fn restart_reason(&self) -> Option<String> {
        self.restart.lock().ok().and_then(|reason| reason.clone())
    }
    pub fn finish(&mut self) -> Option<String> {
        self.shutdown();
        self.cleanup_error.lock().ok().and_then(|error| error.clone())
    }
    /// Bind the logical forwarding session; None suppresses an internal replacement.
    pub fn set_forwarding_pause(&self, pause: Option<crate::media_pause::ForwardingPause>) {
        self.forwarding_stop.attach(pause);
    }
    pub fn start_encoded(encoder: crate::source::Encoder, mute: bool, dir: PathBuf) -> Result<Self, String> {
        Self::start_inner(Box::new(encoder), mute.then_some(dir), Some(0))
    }

    /// Actual WASAPI parameters observed after stream startup, including an
    /// explicit fallback reason. Requested period is never reported as actual.
    pub fn info(&self) -> CaptureInfo {
        self.info.clone()
    }

    fn start_inner(
        encoder: Box<crate::source::Encoder>,
        recovery_dir: Option<PathBuf>,
        requested: Option<u32>,
    ) -> Result<Self, String> {
        let forwarding_stop = crate::media_pause::CaptureStop::default();
        let worker_forwarding_stop = forwarding_stop.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let worker_stop = Arc::clone(&stop);
        let worker_error = Arc::clone(&error);
        let local_controls_changed = Arc::new(AtomicBool::new(false));
        let worker_local_controls_changed = Arc::clone(&local_controls_changed);
        let endpoint_changes = Arc::new(source_devices::Changes::default());
        let worker_changes = endpoint_changes.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = finished.clone();
        let restart = Arc::new(Mutex::new(None));
        let worker_restart = restart.clone();
        let cleanup_error = Arc::new(Mutex::new(None));
        let worker_cleanup_error = cleanup_error.clone();
        let muted_owned = Arc::new(AtomicBool::new(false));
        let worker_muted = Arc::clone(&muted_owned);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("oink-wasapi-capture".into())
            .spawn(move || {
                let result = capture_worker(
                    encoder,
                    &ready_tx,
                    recovery_dir.as_deref(),
                    worker_muted,
                    requested,
                    worker_forwarding_stop,
                    WorkerSignals {
                        stop: &worker_stop,
                        changes: &worker_changes,
                        cleanup_error: &worker_cleanup_error,
                    },
                );

                if let Ok(true) = result {
                    worker_local_controls_changed.store(true, Ordering::Release);
                } else if let Err(error) = result {
                    // If initialization failed this delivers the cause to start;
                    // after successful startup the receiver is already gone.
                    let _ = ready_tx.try_send(Err(error.clone()));
                    tracing::warn!(%error, "WASAPI capture stopped");
                    if let Ok(mut current) = worker_error.lock() {
                        *current = Some(error);
                    }
                }
                if worker_changes.default.load(Ordering::Acquire) || worker_changes.format.load(Ordering::Acquire) {
                    let reason = if worker_changes.default.load(Ordering::Acquire) {
                        "default audio output changed"
                    } else {
                        "audio output stream format changed"
                    };
                    if worker_cleanup_error.lock().unwrap().is_none() {
                        *worker_restart.lock().unwrap() = Some(reason.into());
                    }
                    tracing::info!(reason, "WASAPI capture endpoint notification handled");
                }
                worker_finished.store(
                    worker_local_controls_changed.load(Ordering::Acquire)
                        || worker_changes.default.load(Ordering::Acquire)
                        || worker_changes.format.load(Ordering::Acquire),
                    Ordering::Release,
                );
                crate::device_notifications::notify();
            })
            .map_err(|error| format!("spawn WASAPI capture: {error}"))?;
        let mut capture = Self {
            forwarding_stop,
            stop,
            error,
            local_controls_changed,
            endpoint_changes,
            finished,
            restart,
            cleanup_error,
            worker: Some(worker),
            muted_owned,
            info: CaptureInfo::default(),
        };
        match ready_rx.recv() {
            Ok(Ok(info)) => {
                capture.info = info;
                Ok(capture)
            }
            Ok(Err(error)) => {
                capture.shutdown();
                Err(error)
            }
            Err(_) => {
                capture.shutdown();
                Err("WASAPI capture worker terminated during initialization".into())
            }
        }
    }

    /// Terminal capture failure, including endpoint removal/default switching.
    /// A coordinator should publish this and stop the source stream.
    pub fn error(&self) -> Option<String> {
        if let Some(error) = self.error.lock().ok().and_then(|error| error.clone()) {
            return Some(error);
        }
        // Check the successful-stop latch after completion is observed, so a
        // worker finishing between these reads cannot become a spurious fault.
        (self.worker.as_ref().is_some_and(JoinHandle::is_finished) && !self.finished.load(Ordering::Acquire))
            .then(|| "WASAPI capture worker stopped".to_string())
    }

    /// Whether this live capture owns a successful mute. An external endpoint
    /// event clears this immediately, even when the user's chosen state is muted.
    /// Use endpoint_state for the actual system mute bit and volume.
    pub fn source_muted(&self) -> bool {
        self.muted_owned.load(Ordering::Acquire)
    }

    /// Successful capture stop after an external endpoint control notification.
    /// Published only after IO and the mute lease have been released.
    pub fn local_controls_changed(&self) -> bool {
        self.local_controls_changed.load(Ordering::Acquire)
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self, String> {
        // SAFETY: this dedicated worker initializes COM once, and its guard is
        // dropped on the same thread after every COM interface has been dropped.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|error| format!("initialize COM: {error}"))?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: paired with the successful initialization on this thread.
        unsafe { CoUninitialize() };
    }
}

struct RunningClient(IAudioClient);

impl Drop for RunningClient {
    fn drop(&mut self) {
        // SAFETY: the initialized client belongs to this COM worker; Stop is
        // called before its interfaces and COM apartment are released.
        let _ = unsafe { self.0.Stop() };
    }
}

fn endpoint_id(device: &IMMDevice) -> Result<String, String> {
    // SAFETY: GetId returns a COM-owned null-terminated string. Copy it before
    // releasing its allocation with the documented allocator.
    unsafe {
        let id = device.GetId().map_err(|error| error.to_string())?;
        let result = id.to_string();
        CoTaskMemFree(Some(id.0.cast()));
        result.map_err(|error| error.to_string())
    }
}

struct WorkerSignals<'a> {
    stop: &'a AtomicBool,
    changes: &'a Arc<source_devices::Changes>,
    cleanup_error: &'a Mutex<Option<String>>,
}
fn capture_worker(
    encoder: Box<crate::source::Encoder>,
    ready: &SyncSender<Result<CaptureInfo, String>>,
    recovery_dir: Option<&Path>,
    muted_owned: Arc<AtomicBool>,
    requested: Option<u32>,
    forwarding_stop: crate::media_pause::CaptureStop,
    signals: WorkerSignals<'_>,
) -> Result<bool, String> {
    let WorkerSignals {
        stop,
        changes,
        cleanup_error,
    } = signals;
    let _com = ComApartment::initialize()?;
    // SAFETY: all interfaces are created, used, and released on this initialized
    // COM thread. WASAPI receives a valid 48kHz stereo IEEE float format. Buffer
    // pointers are copied before ReleaseBuffer; silent buffers are never read.
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|error| format!("create audio device enumerator: {error}"))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|error| format!("get default audio output: {error}"))?;
        let id = endpoint_id(&device).map_err(|error| format!("read audio endpoint ID: {error}"))?;
        let _notifications = source_devices::Subscription::new(enumerator.clone(), &id, changes.clone())?;
        // Registration follows selection; re-read once so a switch in that
        // gap still executes the default-change rule. The mute lease retains
        // the selected device and restores only that exact endpoint.
        if !default_is(&enumerator, &id)? {
            changes.default.store(true, Ordering::Release);
        }
        // Diagnostic read only: preservation through mute is driver-dependent.
        if let Ok(volume) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            && let Ok(muted) = volume.GetMute()
        {
            tracing::info!(endpoint = %id, muted = muted.as_bool(), volume = ?volume.GetMasterVolumeLevelScalar().ok(), "WASAPI endpoint state (read only)");
        }
        let mut opened = OpenedCapture::open(&device, requested)?;
        opened.info.physical_output_id = Some(id.clone());
        opened.info.timer_resolution_requested_ms = requested.map(|_| 1);
        let _timer_resolution = if requested.is_some() {
            match crate::windows_timer::TimerResolution::request_1ms() {
                Ok(guard) => {
                    opened.info.timer_resolution_request_succeeded = true;
                    Some(guard)
                }
                Err(error) => {
                    tracing::warn!(%error, "audio timer resolution request failed");
                    opened.info.timer_resolution_error = Some(error);
                    None
                }
            }
        } else {
            None
        };
        let max_frames = opened.max_frames;
        let buffer_duration =
            Duration::from_nanos((u64::from(max_frames) * 1_000_000_000).div_ceil(u64::from(opened.info.sample_rate)));
        let clock = match QpcClock::calibrate() {
            Ok(clock) => {
                opened.info.timestamp_source = format!(
                    "WASAPI loopback pre-output render-clock first-frame QPC100ns mapped to Instant; may be scheduled ahead of dequeue by actual buffer {} frames @ {} Hz ({} ns) plus calibration {} ns; not causal capture-ready time; TIMESTAMP_ERROR => None",
                    max_frames,
                    opened.info.sample_rate,
                    buffer_duration.as_nanos(),
                    clock.uncertainty_ns
                );
                Some(clock)
            }
            Err(error) => {
                opened.info.timestamp_source = format!("hardware timestamp unavailable: {error}");
                tracing::warn!(%error, "QPC calibration failed; capture timing remains unknown");
                None
            }
        };
        // Declare the lease first so unwinding drops the running audio client
        // before restoration touches its endpoint's mute state.
        let mut mute_lease = None;
        opened
            .client
            .Start()
            .map_err(|error| format!("start WASAPI capture: {error}"))?;
        let running = RunningClient(opened.client.clone());
        opened.report_started();
        if let Some(dir) = recovery_dir {
            mute_lease = Some(MuteLease::start(device, dir, muted_owned)?);
        }
        let pause_before_restore = crate::media_pause::BeforeRestore(forwarding_stop);
        let mut realtime = RealtimeCapture::start(&opened, clock, buffer_duration, encoder)?;
        if ready.send(Ok(opened.info.clone())).is_err() {
            return Ok(false);
        }
        let result: Result<bool, String> = (|| {
            while !stop.load(Ordering::Acquire) {
                if let Some(lease) = &mut mute_lease
                    && lease.check_ownership()?
                {
                    return Ok(true);
                }
                if changes.default.load(Ordering::Acquire) || changes.format.load(Ordering::Acquire) {
                    return Ok(false);
                }
                if realtime.finished() {
                    return Err(format!("WASAPI realtime capture stopped: {}", realtime.error()));
                }
                thread::sleep(Duration::from_millis(2));
            }
            Ok(false)
        })();
        // Stream invalidation can precede MMDevice's default notification. One
        // read on failure lets the owner apply the default-change rule instead
        // of merely suspending on a WASAPI fault. Keep the actual stop result;
        // an external-controls takeover needs no extra identity classification.
        if result.is_err() {
            match default_is(&enumerator, &id) {
                Ok(false) => {
                    changes.default.store(true, Ordering::Release);
                }
                Ok(true) => {}
                Err(error) => tracing::warn!(%error, "cannot classify stopped capture endpoint identity"),
            }
        }
        realtime.stop();
        finish_capture_session(changes, recovery_dir.is_some(), &pause_before_restore.0);
        drop(running);
        if let Some(lease) = &mut mute_lease
            && let Err(error) = lease.finish()
        {
            *cleanup_error.lock().unwrap() = Some(error.clone());
            return Err(error);
        }
        result
    }
}

fn finish_capture_session(changes: &source_devices::Changes, muted: bool, stop: &crate::media_pause::CaptureStop) {
    // A format rebuild, or following a default while keeping local playback,
    // replaces only the physical capture. Do not end the logical forwarding
    // ticket or pause players: its encoder submissions must work after reopen.
    // A changed default with a mute lease ends forwarding before restoring the
    // exact OLD endpoint. The new default is never controlled by this handle.
    let default = changes.default.load(Ordering::Acquire);
    if (default && !muted) || (!default && changes.format.load(Ordering::Acquire)) {
        stop.finish_rebuild();
    }
    stop.finish();
}

fn default_is(enumerator: &IMMDeviceEnumerator, id: &str) -> Result<bool, String> {
    // SAFETY: capture worker's initialized MTA owns the enumerator. Read only.
    match unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) } {
        Ok(device) => endpoint_id(&device).map(|current| current == id),
        Err(error) if error.code() == windows::core::HRESULT(0x80070490_u32 as i32) => Ok(false),
        Err(error) => Err(format!("read default audio endpoint identity: {error}")),
    }
}

struct RealtimeCaptureData {
    capture: windows::Win32::Media::Audio::IAudioCaptureClient,
    event: Option<usize>,
    max_frames: u32,
    clock: Option<QpcClock>,
    buffer_duration: Duration,
    expected_position: Option<u64>,
    scratch: Vec<f32>,
}
// SAFETY: interface is created in the process MTA and moved to another MTA
// thread. Only that thread calls GetBuffer/ReleaseBuffer. The parent retains
// the initialized client/event and joins this child before Stop or COM release.
unsafe impl Send for RealtimeCaptureData {}
impl RealtimeCaptureData {
    fn wait(&self) -> Result<(), i32> {
        match self.event {
            Some(event) => {
                // SAFETY: parent owns live event until after child join.
                let status = unsafe { WaitForSingleObject(HANDLE(event as *mut _), 2) };
                if status == WAIT_OBJECT_0 || status == WAIT_TIMEOUT {
                    Ok(())
                } else {
                    Err(status.0 as i32)
                }
            }
            None => {
                thread::sleep(Duration::from_millis(2));
                Ok(())
            }
        }
    }
    fn work(&mut self, encoder: &mut crate::source::Encoder) -> Result<(), i32> {
        use windows::core::Interface;
        for _ in 0..8 {
            let mut pending = 0;
            // Use raw HRESULTs on this path: windows::Error can fetch/allocate
            // COM error text. The ordinary guardian formats only numeric errors.
            // SAFETY: valid live capture interface; scalar outputs are writable.
            let result = unsafe { (self.capture.vtable().GetNextPacketSize)(self.capture.as_raw(), &mut pending) };
            if result.0 < 0 {
                return Err(result.0);
            }
            if pending == 0 {
                break;
            }
            let (mut data, mut frames, mut flags, mut position, mut qpc) = (std::ptr::null_mut(), 0, 0, 0, 0);
            // SAFETY: pointers remain valid until the matching ReleaseBuffer.
            let result = unsafe {
                (self.capture.vtable().GetBuffer)(
                    self.capture.as_raw(),
                    &mut data,
                    &mut frames,
                    &mut flags,
                    &mut position,
                    &mut qpc,
                )
            };
            if result.0 < 0 {
                return Err(result.0);
            }
            let observed = Instant::now();
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            if frames > self.max_frames || (!silent && frames > 0 && data.is_null()) {
                // SAFETY: release the buffer even on invalid metadata.
                unsafe {
                    let _ = (self.capture.vtable().ReleaseBuffer)(self.capture.as_raw(), frames);
                }
                return Err(-1);
            }
            let samples = &mut self.scratch[..frames as usize * 2];
            if silent {
                samples.fill(0.0);
            } else if frames > 0 {
                // SAFETY: exactly the returned 48 kHz stereo f32 byte buffer;
                // use byte reads rather than assuming float pointer alignment.
                let bytes = unsafe { std::slice::from_raw_parts(data, frames as usize * 8) };
                for (sample, bytes) in samples.iter_mut().zip(bytes.as_chunks::<4>().0) {
                    *sample = f32::from_le_bytes(*bytes);
                }
            }
            // SAFETY: same interface/thread and exact returned frame count.
            let result = unsafe { (self.capture.vtable().ReleaseBuffer)(self.capture.as_raw(), frames) };
            if result.0 < 0 {
                return Err(result.0);
            }
            if samples.is_empty() {
                continue;
            }
            let position = (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0).then_some(position);
            let stamp = crate::source::Stamp {
                first_sample_at: position.and_then(|_| {
                    self.clock
                        .as_ref()
                        .and_then(|clock| clock.map(qpc, observed, self.buffer_duration))
                }),
                callback_at: None,
                delivered_at: Instant::now(),
                source_frame_position: position,
                discontinuity: flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0
                    || position
                        .zip(self.expected_position)
                        .is_some_and(|(position, expected)| position != expected),
            };
            self.expected_position = position.and_then(|p| p.checked_add(u64::from(frames)));
            encoder.observe_wasapi_buffer(silent);
            encoder.supply(samples, stamp);
        }
        Ok(())
    }
}
struct RealtimeCapture {
    stop: Arc<AtomicBool>,
    state: Arc<[std::sync::atomic::AtomicI32; 2]>,
    event: Option<usize>,
    worker: Option<
        JoinHandle<(
            Box<crate::source::Encoder>,
            RealtimeCaptureData,
            crate::realtime::Schedule,
        )>,
    >,
}
impl RealtimeCapture {
    fn start(
        opened: &OpenedCapture,
        clock: Option<QpcClock>,
        buffer_duration: Duration,
        mut encoder: Box<crate::source::Encoder>,
    ) -> Result<Self, String> {
        let mut data = RealtimeCaptureData {
            capture: opened.capture.clone(),
            event: opened.realtime_event(),
            max_frames: opened.max_frames,
            clock,
            buffer_duration,
            expected_position: None,
            scratch: vec![0.0; opened.max_frames as usize * 2],
        };
        let event = data.event;
        let mut schedule = crate::realtime::Schedule::device(0, 0)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let state = Arc::new([
            std::sync::atomic::AtomicI32::new(0),
            std::sync::atomic::AtomicI32::new(0),
        ]);
        let status = state.clone();
        let worker = thread::Builder::new()
            .name("oink-audio-encode".into())
            .spawn(move || {
                let com = ComApartment::initialize();
                if com.is_err() {
                    status[1].store(-1, Ordering::Release);
                    status[0].store(2, Ordering::Release);
                    return (encoder, data, schedule);
                }
                let mut previous: Option<Instant> = None;
                {
                    match schedule.enter() {
                        Ok(guard) => {
                            status[0].store(1, Ordering::Release);
                            while !stopping.load(Ordering::Acquire) && encoder.active() {
                                if let Err(error) = data.wait() {
                                    status[1].store(error, Ordering::Release);
                                    break;
                                }
                                if stopping.load(Ordering::Acquire) {
                                    break;
                                }
                                let now = Instant::now();
                                if let Err(error) = data.work(&mut encoder) {
                                    status[1].store(error, Ordering::Release);
                                    break;
                                }
                                encoder.tick(Instant::now());
                                encoder
                                    .observe_work(previous.map(|at| now.saturating_duration_since(at)), now.elapsed());
                                previous = Some(now);
                            }
                            drop(guard);
                        }
                        Err(error) => {
                            status[1].store(error, Ordering::Release);
                            status[0].store(2, Ordering::Release);
                        }
                    }
                }
                // The COM reference is returned to the parent MTA and dropped there,
                // after join. This child's apartment leaves only after work stops.
                (encoder, data, schedule)
            })
            .map_err(|e| format!("start WASAPI realtime encoder: {e}"))?;
        let mut child = Self {
            stop,
            state,
            event,
            worker: Some(worker),
        };
        let started = Instant::now();
        while child.state[0].load(Ordering::Acquire) == 0 && started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(1));
        }
        if child.state[0].load(Ordering::Acquire) != 1 {
            child.stop();
            return Err(format!("WASAPI realtime scheduling failed: {}", child.error()));
        }
        Ok(child)
    }
    fn stop(&mut self) {
        if self.worker.is_none() {
            return;
        }
        self.stop.store(true, Ordering::Release);
        if let Some(event) = self.event {
            // SAFETY: parent retains this event until after the join below.
            unsafe {
                let _ = windows::Win32::System::Threading::SetEvent(HANDLE(event as *mut _));
            }
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
    fn error(&self) -> i32 {
        self.state[1].load(Ordering::Acquire)
    }
    fn finished(&self) -> bool {
        self.worker.as_ref().is_some_and(JoinHandle::is_finished)
    }
}
impl Drop for RealtimeCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Query the selected endpoint without modifying any setting. `None` selects
/// the default console render endpoint; `Some` addresses the exact saved ID.
pub fn endpoint_state(endpoint: Option<&str>) -> Result<EndpointState, String> {
    let endpoint = endpoint.map(str::to_owned);
    thread::spawn(move || {
        let _com = ComApartment::initialize()?;
        let enumerator = enumerator()?;
        // SAFETY: this worker owns a COM apartment and all returned interfaces.
        let device = unsafe {
            match endpoint {
                Some(id) => exact_device(&enumerator, &id)?.ok_or("audio endpoint unavailable")?,
                None => enumerator
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    .map_err(|e| e.to_string())?,
            }
        };
        let id = endpoint_id(&device)?;
        // SAFETY: device is live on this initialized COM thread.
        let volume: IAudioEndpointVolume =
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| format!("activate endpoint volume: {e}"))?;
        read_volume(&volume, &id)
    })
    .join()
    .map_err(|_| "audio endpoint query worker panicked".to_string())?
}

/// Recover an abandoned mute record. This operation locks the same directory as
/// live source capture, so it cannot restore another process's active lease.
/// After an unclean exit, matching state is a best-effort ownership check only;
/// consult `crash_intent_uncertain` in the result before reporting guarantees.
pub fn recover(dir: &Path) -> Result<RecoveryReport, String> {
    let dir = dir.to_path_buf();
    thread::spawn(move || {
        let _com = ComApartment::initialize()?;
        let _system_lock = EndpointMuteLock::acquire()?;
        let mut journal = Journal::open(&dir)?;
        recover_locked(&mut journal)
    })
    .join()
    .map_err(|_| "audio recovery worker panicked".to_string())?
}

/// Endpoint controls are machine-wide even when Oink instances use different
/// OINK_HOME directories or Windows sessions. Never wait for a live owner.
/// The journal file lock remains necessary for record integrity and tests.
struct EndpointMuteLock(HANDLE);
impl EndpointMuteLock {
    fn acquire() -> Result<Self, String> {
        // SAFETY: constant NUL-terminated name, default security descriptor,
        // and a valid handle used only by the thread that acquires ownership.
        unsafe {
            let handle = CreateMutexW(None, false, windows::core::w!("Global\\Oink.AudioMuteRecovery.v1"))
                .map_err(|e| format!("cannot acquire global audio mute ownership (another user or access denied); no endpoint changed: {e}"))?;
            match WaitForSingleObject(handle, 0) {
                WAIT_OBJECT_0 => Ok(Self(handle)),
                WAIT_ABANDONED => {
                    // Windows transfers ownership to this thread after the
                    // former owner dies. The durable journal decides recovery.
                    tracing::warn!("previous global audio mute owner exited unexpectedly; checking recovery journal");
                    Ok(Self(handle))
                }
                result => {
                    let error = if result == WAIT_TIMEOUT {
                        "another process owns endpoint mute/recovery; refusing concurrent audio source".to_string()
                    } else {
                        format!(
                            "global audio mute ownership wait failed: {}",
                            windows::core::Error::from_thread()
                        )
                    };
                    let _ = CloseHandle(handle);
                    Err(error)
                }
            }
        }
    }
}
impl Drop for EndpointMuteLock {
    fn drop(&mut self) {
        // SAFETY: this guard stays on the same worker that acquired the mutex,
        // and is dropped after restoration and the journal/observer are closed.
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator, String> {
    // SAFETY: callers initialize COM and keep the interface on that thread.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|e| format!("create audio device enumerator: {e}"))
}

fn exact_device(enumerator: &IMMDeviceEnumerator, id: &str) -> Result<Option<IMMDevice>, String> {
    if id.is_empty() || id.contains('\0') || id.len() > 4096 {
        return Err("invalid audio endpoint ID".into());
    }
    let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
    // SAFETY: the ID is NUL-terminated and valid through this synchronous call.
    // Unavailable endpoints retain their journal; never substitute the default.
    unsafe {
        let Ok(device) = enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())) else {
            return Ok(None);
        };
        if device.GetState().map_err(|e| e.to_string())? != DEVICE_STATE_ACTIVE {
            return Ok(None);
        }
        Ok(Some(device))
    }
}

fn read_volume(volume: &IAudioEndpointVolume, id: &str) -> Result<EndpointState, String> {
    // SAFETY: the interface remains live on its COM worker thread. Reads only.
    unsafe {
        Ok(EndpointState {
            endpoint: id.into(),
            muted: volume
                .GetMute()
                .map_err(|e| format!("read endpoint mute: {e}"))?
                .as_bool(),
            volume: volume
                .GetMasterVolumeLevelScalar()
                .map_err(|e| format!("read endpoint volume: {e}"))?,
        })
    }
}

#[windows::core::implement(IAudioEndpointVolumeCallback)]
struct VolumeCallback {
    context: windows::core::GUID,
    revoked: Arc<AtomicBool>,
    muted_owned: Arc<AtomicBool>,
    mute_changed: Arc<AtomicBool>,
}
impl IAudioEndpointVolumeCallback_Impl for VolumeCallback_Impl {
    fn OnNotify(&self, notify: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        if notify.is_null() {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                0x80004003_u32 as i32,
            )));
        }
        // SAFETY: WASAPI supplies this notification for the duration of the
        // callback. Read only the fixed header; no flexible-array access.
        let (context, muted) = unsafe { ((*notify).guidEventContext, (*notify).bMuted.as_bool()) };
        if context != self.context {
            // This is an endpoint state notification, never a key press. It
            // only relinquishes this source endpoint's mute lease. In
            // keep-local mode there is no subscription; Windows applies its
            // restored volume/mute locally whatever device has input focus.
            // No scalar, focus selection, exit message or forwarding-gain
            // action crosses this boundary.
            // Microsoft requires nonblocking callbacks. Revoke immediately in
            // memory; the capture worker persists the sticky latch on its next
            // 2ms iteration (or synchronously before restoration on shutdown).
            // Do not call COM, fsync, unregister, or take a lock in OnNotify.
            // Our lease's applied state is muted=true. An external unmute
            // permanently gives up mute-only restoration, even if toggled back.
            if !muted {
                self.mute_changed.store(true, Ordering::Release);
            }
            self.revoked.store(true, Ordering::Release);
            self.muted_owned.store(false, Ordering::Release);
        }
        Ok(())
    }
}

struct VolumeSubscription {
    device: IMMDevice,
    id: String,
    volume: IAudioEndpointVolume,
    callback: IAudioEndpointVolumeCallback,
    context: windows::core::GUID,
    revoked: Arc<AtomicBool>,
    muted_owned: Arc<AtomicBool>,
    mute_changed: Arc<AtomicBool>,
}
impl VolumeSubscription {
    fn new(device: IMMDevice, muted_owned: Arc<AtomicBool>) -> Result<Self, String> {
        let id = endpoint_id(&device)?;
        let revoked = Arc::new(AtomicBool::new(false));
        let mute_changed = Arc::new(AtomicBool::new(false));
        // SAFETY: COM is initialized and the device is live on this worker.
        unsafe {
            let context = CoCreateGuid().map_err(|e| format!("create audio event context: {e}"))?;
            let volume: IAudioEndpointVolume = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("activate endpoint mute control: {e}"))?;
            let callback: IAudioEndpointVolumeCallback = VolumeCallback {
                context,
                revoked: Arc::clone(&revoked),
                muted_owned: Arc::clone(&muted_owned),
                mute_changed: Arc::clone(&mute_changed),
            }
            .into();
            volume
                .RegisterControlChangeNotify(&callback)
                .map_err(|e| format!("register endpoint volume observer: {e}"))?;
            Ok(Self {
                device,
                id,
                volume,
                callback,
                context,
                revoked,
                muted_owned,
                mute_changed,
            })
        }
    }
}
impl EndpointBackend for VolumeSubscription {
    fn read(&self, endpoint: &str) -> Result<Option<EndpointState>, String> {
        if endpoint != self.id {
            return Err("audio recovery endpoint mismatch".into());
        }
        // SAFETY: this device is owned by the current COM worker.
        if unsafe { self.device.GetState() }.map_err(|e| e.to_string())? != DEVICE_STATE_ACTIVE {
            return Ok(None);
        }
        read_volume(&self.volume, endpoint).map(Some)
    }
    fn set_mute(&self, endpoint: &str, muted: bool) -> Result<(), String> {
        if endpoint != self.id {
            return Err("audio recovery endpoint mismatch".into());
        }
        if self.ownership_lost() {
            return Err("user changed endpoint controls; mute ownership relinquished".into());
        }
        // SAFETY: live endpoint on its COM worker, and this lease's GUID is valid
        // during the call. Only mute is written; volume/default remain untouched.
        unsafe { self.volume.SetMute(muted, &self.context) }.map_err(|e| format!("set endpoint mute: {e}"))
    }
    fn ownership_lost(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }
    fn mute_ownership_lost(&self) -> bool {
        self.mute_changed.load(Ordering::Acquire)
    }
    fn release_owned_mute(&self, endpoint: &str) -> Result<(), String> {
        if endpoint != self.id {
            return Err("audio recovery endpoint mismatch".into());
        }
        if self.mute_ownership_lost() {
            return Ok(());
        }
        // SAFETY: live interface on its COM worker. Read the exact endpoint's
        // current mute; the user's level is never written, read back or restored.
        if !unsafe { self.volume.GetMute() }.map_err(|e| e.to_string())?.as_bool() || self.mute_ownership_lost() {
            return Ok(());
        }
        // SAFETY: release only the mute this lease applied; its own event GUID
        // prevents our notification from being mistaken for a user mute toggle.
        unsafe { self.volume.SetMute(false, &self.context) }.map_err(|e| format!("release endpoint mute: {e}"))
    }
}
impl Drop for VolumeSubscription {
    fn drop(&mut self) {
        self.muted_owned.store(false, Ordering::Release);
        // SAFETY: unsubscribe outside the callback while both interfaces are
        // live; the COM callback references its own Arcs until delivery ends.
        if let Err(error) = unsafe { self.volume.UnregisterControlChangeNotify(&self.callback) } {
            tracing::warn!(%error, "unregister endpoint volume observer failed");
        }
    }
}

struct UnavailableEndpoint;
impl EndpointBackend for UnavailableEndpoint {
    fn read(&self, _: &str) -> Result<Option<EndpointState>, String> {
        Ok(None)
    }
    fn set_mute(&self, _: &str, _: bool) -> Result<(), String> {
        Err("audio endpoint unavailable".into())
    }
    fn ownership_lost(&self) -> bool {
        false
    }
}

fn recover_locked(journal: &mut Journal) -> Result<RecoveryReport, String> {
    let Some(id) = journal.endpoint().map(str::to_owned) else {
        return journal.recover(&UnavailableEndpoint, true);
    };
    if !journal.has_pending_restore() {
        return journal.recover(&UnavailableEndpoint, true);
    }
    let Some(device) = exact_device(&enumerator()?, &id)? else {
        return journal.recover(&UnavailableEndpoint, true);
    };
    let observer = VolumeSubscription::new(device, Arc::new(AtomicBool::new(false)))?;
    journal.recover(&observer, true)
}

struct MuteLease {
    // Field order matters: remove the observer, then release the journal lock,
    // and only finally permit another process to acquire the global mute lock.
    observer: VolumeSubscription,
    journal: Journal,
    _system_lock: EndpointMuteLock,
    finished: bool,
}
impl MuteLease {
    fn start(device: IMMDevice, dir: &Path, muted_owned: Arc<AtomicBool>) -> Result<Self, String> {
        let system_lock = EndpointMuteLock::acquire()?;
        let mut journal = Journal::open(dir)?;
        let report = recover_locked(&mut journal)?;
        if report.outcome == RecoveryOutcome::EndpointUnavailable {
            return Err("previous muted endpoint is unavailable; its recovery journal was retained".into());
        }
        if report.crash_intent_uncertain {
            tracing::warn!(
                ?report,
                "recovered mute using matching state; user actions while process was absent cannot be known"
            );
        }
        let observer = VolumeSubscription::new(device, muted_owned)?;
        let before = observer
            .read(&observer.id)?
            .ok_or("audio endpoint unavailable before mute")?;
        journal.prepare(before.clone())?; // durable write-ahead before SetMute
        let mut lease = Self {
            journal,
            observer,
            _system_lock: system_lock,
            finished: false,
        };
        if lease.observer.ownership_lost() || lease.observer.read(&lease.observer.id)? != Some(before.clone()) {
            lease.journal.revoke()?;
            return Err("endpoint controls changed during mute setup; no mute ownership taken".into());
        }
        lease.observer.set_mute(&lease.observer.id, true)?;
        let mut expected = before;
        expected.muted = true;
        if lease.observer.ownership_lost() || lease.observer.read(&lease.observer.id)? != Some(expected) {
            lease.journal.revoke()?;
            return Err("endpoint controls changed while starting capture; user state takes priority".into());
        }
        lease.observer.muted_owned.store(true, Ordering::Release);
        // Close the atomic publication race with a concurrently delivered user
        // event: callback also clears muted_owned after latching revocation.
        if lease.observer.ownership_lost() {
            lease.observer.muted_owned.store(false, Ordering::Release);
            lease.journal.revoke()?;
            return Err("user changed endpoint controls during mute setup".into());
        }
        Ok(lease)
    }

    fn check_ownership(&mut self) -> Result<bool, String> {
        if self.observer.ownership_lost() {
            self.journal.revoke()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn finish(&mut self) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        self.observer.muted_owned.store(false, Ordering::Release);
        if self.observer.ownership_lost() {
            self.journal.release_after_volume_change(&self.observer)?;
        }
        let report = self.journal.recover(&self.observer, false)?;
        self.finished = true;
        if report.outcome == RecoveryOutcome::EndpointUnavailable {
            return Err(
                "muted endpoint unavailable during stop; recovery journal retained for reconnect/startup".into(),
            );
        }
        tracing::info!(?report, "audio endpoint mute lease finished");
        Ok(())
    }
}
impl Drop for MuteLease {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            tracing::error!(%error, "audio mute restoration incomplete; inspect retained recovery journal");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_following_and_format_restart_preserve_the_forwarding_ticket() {
        for (default, format, muted, ends) in [
            (true, false, true, true),
            (true, false, false, false),
            (false, true, true, false),
            (false, true, false, false),
        ] {
            let changes = source_devices::Changes::default();
            changes.default.store(default, Ordering::Release);
            changes.format.store(format, Ordering::Release);
            let pause = crate::media_pause::ForwardingPause::with_backend(|| {});
            let stop = crate::media_pause::CaptureStop::default();
            stop.attach(Some(pause.clone()));
            finish_capture_session(&changes, muted, &stop);
            assert_eq!(pause.is_ended(), ends);
            assert_eq!(
                pause.submit(|| Ok::<_, ()>(())).is_none(),
                ends,
                "reopened capture must still be able to submit media"
            );
        }
    }

    #[test]
    fn local_controls_stop_is_not_a_capture_error() {
        let worker = thread::spawn(|| {});
        while !worker.is_finished() {
            thread::yield_now();
        }
        let capture = Capture {
            forwarding_stop: Default::default(),
            stop: Arc::new(AtomicBool::new(false)),
            error: Arc::new(Mutex::new(None)),
            local_controls_changed: Arc::new(AtomicBool::new(true)),
            endpoint_changes: Arc::default(),
            finished: Arc::new(AtomicBool::new(true)),
            restart: Arc::default(),
            cleanup_error: Arc::default(),
            worker: Some(worker),
            muted_owned: Arc::new(AtomicBool::new(false)),
            info: Default::default(),
        };
        assert!(capture.local_controls_changed());
        assert!(capture.error().is_none());
    }

    #[test]
    fn callback_latches_foreign_events_even_when_values_return_to_original() {
        let context = windows::core::GUID::from_u128(1);
        let revoked = Arc::new(AtomicBool::new(false));
        let muted_owned = Arc::new(AtomicBool::new(true));
        let mute_changed = Arc::new(AtomicBool::new(false));
        let callback: IAudioEndpointVolumeCallback = VolumeCallback {
            context,
            revoked: Arc::clone(&revoked),
            muted_owned: Arc::clone(&muted_owned),
            mute_changed: Arc::clone(&mute_changed),
        }
        .into();
        let mut event = AUDIO_VOLUME_NOTIFICATION_DATA {
            guidEventContext: context,
            bMuted: true.into(),
            fMasterVolume: 0.23,
            ..Default::default()
        };
        // SAFETY: notification points to valid fixed-header storage. This calls
        // our in-process callback directly; it never opens any system endpoint.
        unsafe { callback.OnNotify(&mut event) }.unwrap();
        assert!(!revoked.load(Ordering::Acquire));
        assert!(muted_owned.load(Ordering::Acquire));
        event.guidEventContext = windows::core::GUID::from_u128(2);
        event.fMasterVolume = 0.4;
        // SAFETY: fake level-only notification, no system endpoint access.
        unsafe { callback.OnNotify(&mut event) }.unwrap();
        assert!(revoked.load(Ordering::Acquire));
        assert!(!mute_changed.load(Ordering::Acquire));
        event.bMuted = false.into();
        // SAFETY: same live notification and callback, no OS mutation.
        unsafe { callback.OnNotify(&mut event) }.unwrap();
        event.bMuted = true.into();
        // SAFETY: same valid notification simulating the user toggling back.
        unsafe { callback.OnNotify(&mut event) }.unwrap();
        assert!(revoked.load(Ordering::Acquire));
        assert!(!muted_owned.load(Ordering::Acquire));
        assert!(mute_changed.load(Ordering::Acquire));
        event.guidEventContext = context;
        // SAFETY: own later notification must not reclaim revoked ownership.
        unsafe { callback.OnNotify(&mut event) }.unwrap();
        assert!(revoked.load(Ordering::Acquire));
        assert!(!muted_owned.load(Ordering::Acquire));
    }
}
