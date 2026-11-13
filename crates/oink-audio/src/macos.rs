//! macOS 14.2+ system-output capture. Starting may present the system audio
//! recording permission prompt. Group members trust each other, so an enabled
//! group route is enough to start; only the system permission gates capture.
//!
//! The private Core Audio tap excludes this process and remains unmuted. Source
//! silence uses a durable hardware-mute lease, upstream-independent of tap PCM.
//! No default-device selection or volume is written. A requested physical
//! IO-buffer lease is device-global and conditionally restored on stop.

#[path = "macos_events.rs"]
mod events;

use std::{
    ffi::{CStr, CString, c_char, c_void},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(test)]
use crate::CaptureBlock;
use crate::CaptureInfo;

/// Explicit CoreAudio topology controls for production routing and bounded
/// diagnostics. No failed selection falls back to a different topology.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MacCaptureOptions {
    /// Required for source muting; fixed per-user directory across restarts.
    pub recovery_dir: Option<std::path::PathBuf>,
    pub output_uid: Option<String>,
    pub global_tap: bool,
    pub physical_master: bool,
    pub drift_compensation: bool,
    pub requested_buffer_frames: Option<u32>,
    pub physical_buffer_before_tap: bool,
    pub tap_private: bool,
    /// Expensive HAL snapshots for explicit diagnostic runs, disabled in production.
    pub collect_diagnostics: bool,
}
impl Default for MacCaptureOptions {
    fn default() -> Self {
        Self {
            recovery_dir: None,
            output_uid: None,
            global_tap: false,
            physical_master: true,
            drift_compensation: false,
            requested_buffer_frames: Some(128),
            physical_buffer_before_tap: true,
            tap_private: true,
            collect_diagnostics: false,
        }
    }
}
impl MacCaptureOptions {
    fn native_mode(&self) -> u32 {
        match (self.global_tap, self.physical_master, self.drift_compensation) {
            (true, false, true) => 0,
            (false, true, false) => 1,
            (false, true, true) => 2,
            (true, true, true) => 3,
            (true, true, false) => 4,
            (false, false, false) => 5,
            (false, false, true) => 6,
            (true, false, false) => 7,
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct NativeInfo {
    sample_rate: f64,
    physical_sample_rate: f64,
    tap_sample_rate: f64,
    buffer_frames: u32,
    latency_frames: u32,
    safety_frames: u32,
    physical_device: u32,
    physical_buffer_frames: u32,
    physical_latency_frames: u32,
    physical_safety_frames: u32,
    buffer_request_status: i32,
    physical_original_buffer_frames: u32,
    physical_lease_buffer_frames: u32,
    physical_lease_changed: u32,
    physical_buffer_request_status: i32,
    capture_mode: u32,
}

struct SourceMute {
    lease: Option<crate::macos_mute::MuteLease>,
    owned: Arc<AtomicBool>,
    cleanup: Arc<Mutex<Option<String>>>,
}
impl SourceMute {
    /// Keep the endpoint muted for the next capture of the same forwarding.
    /// A lease that lost exact ownership is restored here instead.
    fn into_held(mut self) -> Option<HeldSourceMute> {
        if !self.lease.as_ref().is_some_and(|lease| lease.transferable()) {
            return None; // Drop restores (or releases) through the journal.
        }
        self.owned.store(false, Ordering::Release);
        self.lease.take().map(HeldSourceMute)
    }
}
impl std::fmt::Debug for SourceMute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceMute")
            .field("endpoint", &self.lease.as_ref().map(|lease| lease.uid()))
            .finish()
    }
}
impl Drop for SourceMute {
    fn drop(&mut self) {
        self.owned.store(false, Ordering::Release);
        if let Some(lease) = &mut self.lease
            && let Err(error) = lease.finish()
            && let Ok(mut state) = self.cleanup.lock()
        {
            *state = Some(error);
        }
    }
}

/// The source-output mute lease of one forwarding session, between a retired
/// capture and its replacement. A capture rebuild is not a forwarding end: the
/// endpoint stays muted and no media pause runs. Dropping it (forwarding ended
/// before a replacement adopted it) restores local output, pausing media first.
pub struct HeldSourceMute(crate::macos_mute::MuteLease);
impl HeldSourceMute {
    /// Restore local output now and report an unconfirmed restoration.
    pub fn finish(mut self) -> Option<String> {
        self.0.finish().err()
    }
}
impl std::fmt::Debug for HeldSourceMute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("HeldSourceMute").field(&self.0.uid()).finish()
    }
}

unsafe extern "C" {
    fn oink_audio_capture_omitted_silent_frames(capture: *mut c_void) -> u64;
    fn oink_audio_capture_events(
        capture: *mut c_void,
        events: *mut events::NativeEvent,
        capacity: usize,
        dropped: *mut u64,
        trimmed: *mut u64,
        audio_dropped: *mut u64,
    ) -> usize;
    fn oink_audio_thread_scheduling(
        capture: *mut c_void,
        qos: *mut i32,
        relative: *mut i32,
        policy: *mut i32,
        priority: *mut i32,
        error: *mut i32,
    );
    fn oink_audio_capture_start(
        mute: bool,
        requested_frames: u32,
        capture_mode: u32,
        selected_uid: *const c_char,
        buffer_before_tap: bool,
        tap_private: bool,
        collect_diagnostics: bool,
        realtime: bool,
        sample_rate: *mut f64,
        error: *mut c_char,
        error_size: usize,
    ) -> *mut c_void;
    fn oink_audio_reference_tap_start(
        output_uid: *const c_char,
        sample_rate: *mut f64,
        error: *mut c_char,
        error_size: usize,
    ) -> *mut c_void;
    fn oink_audio_capture_wake(capture: *mut c_void);
    fn oink_audio_capture_wait(capture: *mut c_void) -> i32;
    fn oink_audio_capture_device_id(capture: *mut c_void) -> u32;
    fn oink_audio_capture_read(
        capture: *mut c_void,
        output: *mut f32,
        capacity_frames: usize,
        host_ns: *mut u64,
        callback_ns: *mut u64,
        sequence: *mut u64,
    ) -> usize;
    fn oink_audio_capture_info(capture: *mut c_void, info: *mut NativeInfo);
    fn oink_audio_capture_physical_uid(capture: *mut c_void, buffer: *mut c_char, capacity: usize) -> bool;
    fn oink_audio_default_output_uid(buffer: *mut c_char, capacity: usize) -> i32;
    fn oink_audio_capture_diagnostics(capture: *mut c_void, buffer: *mut c_char, capacity: usize) -> usize;
    fn oink_audio_host_time_ns() -> u64;
    fn oink_audio_capture_stop(capture: *mut c_void, error: *mut c_char, capacity: usize) -> i32;
    fn oink_audio_capture_cleanup_failed() -> bool;
    fn oink_audio_capture_check(capture: *mut c_void, error: *mut c_char, error_size: usize) -> i32;
    #[cfg(test)]
    fn oink_audio_capture_format_health(status: i32, supported: bool, changed: bool) -> i32;
}

/// A running private process tap. Its worker emits 48 kHz interleaved stereo.
#[derive(Debug)]
pub struct Capture {
    muted_owned: Arc<AtomicBool>,
    local_controls_changed: Arc<AtomicBool>,
    default_output_changed: Arc<AtomicBool>,
    cleanup_error: Arc<Mutex<Option<String>>>,
    info: CaptureInfo,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    error: Arc<Mutex<Option<String>>>,
    restart: Arc<Mutex<Option<String>>>,
    omitted_silent_frames: Arc<CaptureProgress>,
    /// The worker leaves its mute lease here on exit; the owner decides between
    /// restoring it (forwarding end) and handing it to the next capture.
    parked_mute: Arc<Mutex<Option<SourceMute>>>,
}

impl Capture {
    /// Sticky for this process. Once HAL cleanup fails, creating another tap is
    /// forbidden even after the default output changes; restart the application
    /// after checking local audio. This query never accesses audio hardware.
    pub fn cleanup_failed() -> bool {
        // SAFETY: Reads only a native process-local atomic flag.
        unsafe { oink_audio_capture_cleanup_failed() }
    }

    /// Production data plane: encoding and gain execute in the dedicated
    /// device-workgroup member; this owner keeps health and HAL lifetime.
    pub fn start_encoded(
        encoder: crate::source::Encoder,
        mute: bool,
        options: MacCaptureOptions,
    ) -> Result<Self, String> {
        Self::start_encoded_with(encoder, mute, None, options)
    }

    /// Muted replacement capture for the same forwarding session: adopts the
    /// retired capture's mute lease instead of restoring and re-muting, and
    /// binds the new tap to that lease's exact endpoint.
    pub fn continue_encoded(
        encoder: crate::source::Encoder,
        held: HeldSourceMute,
        options: MacCaptureOptions,
    ) -> Result<Self, String> {
        Self::start_encoded_with(encoder, true, Some(held), options)
    }

    fn start_encoded_with(
        encoder: crate::source::Encoder,
        mute: bool,
        held: Option<HeldSourceMute>,
        options: MacCaptureOptions,
    ) -> Result<Self, String> {
        let mode = options.native_mode();
        let selected_uid = options
            .output_uid
            .as_deref()
            .map(|uid| CString::new(uid).map_err(|_| "Capture output UID contains NUL".to_string()))
            .transpose()?;
        Self::start_config(
            Box::new(encoder),
            mute,
            options.requested_buffer_frames,
            mode,
            selected_uid,
            options.physical_buffer_before_tap,
            options.tap_private,
            options.collect_diagnostics,
            options.recovery_dir,
            held,
        )
    }

    /// The UID query does not create a tap or change system settings. Callers
    /// can use it to retry a failed default-device capture only after a switch.
    pub fn current_default_output_id() -> Result<String, String> {
        let mut uid = [0 as c_char; 1024];
        // SAFETY: Writable output buffer; success returns a terminated C string.
        let status = unsafe { oink_audio_default_output_uid(uid.as_mut_ptr(), uid.len()) };
        if status != 0 {
            return Err(format!("Read default output UID: Core Audio status {status}"));
        }
        // SAFETY: A successful native call guarantees NUL termination.
        Ok(unsafe { CStr::from_ptr(uid.as_ptr()) }.to_string_lossy().into_owned())
    }

    /// A default-device change is published only after the old native tap has
    /// stopped and released mute. Drop this Capture, clear pending PCM, and call
    /// the same API/options again to follow the new default without fallback.
    pub fn restart_reason(&self) -> Option<String> {
        self.restart.lock().ok().and_then(|reason| reason.clone())
    }

    /// Frames the live native tap deliberately omitted after its digital-zero tail.
    /// Read live progress, independently of the ordinary owner's health polling.
    pub fn omitted_silent_frames(&self) -> u64 {
        self.omitted_silent_frames.frames()
    }

    pub fn diagnostics(&self) -> serde_json::Value {
        self.info.diagnostics.clone()
    }

    /// Explicit historical global-tap API, retained for scope choice/baselines.
    /// None preserves HAL's default for baseline measurements; Some(0) requests
    /// the private aggregate's minimum, Some(n) requests n frames within its range.
    /// Optimized capture also leases the default physical output buffer before
    /// tap creation. This device-global change is conditionally restored on stop;
    /// crashes cannot restore it and same-value external writes are indistinguishable.
    pub fn info(&self) -> CaptureInfo {
        self.info.clone()
    }

    #[allow(clippy::too_many_arguments)] // Mirrors the native initialization options.
    fn start_config(
        encoder: Box<crate::source::Encoder>,
        mute: bool,
        requested: Option<u32>,
        capture_mode: u32,
        selected_uid: Option<CString>,
        buffer_before_tap: bool,
        tap_private: bool,
        collect_diagnostics: bool,
        recovery_dir: Option<std::path::PathBuf>,
        held: Option<HeldSourceMute>,
    ) -> Result<Self, String> {
        if mute && held.is_none() && recovery_dir.is_none() {
            return Err("muted macOS capture requires a durable recovery directory".into());
        }
        let muted_owned = Arc::new(AtomicBool::new(false));
        let worker_muted = muted_owned.clone();
        let local_controls_changed = Arc::new(AtomicBool::new(false));
        let worker_controls = local_controls_changed.clone();
        let default_output_changed = Arc::new(AtomicBool::new(false));
        let worker_default = default_output_changed.clone();
        let cleanup_error = Arc::new(Mutex::new(None));
        let worker_cleanup = cleanup_error.clone();
        let restart = Arc::new(Mutex::new(None));
        let worker_restart = restart.clone();
        let omitted_silent_frames = Arc::new(CaptureProgress::default());
        let worker_omitted = omitted_silent_frames.clone();
        let parked_mute = Arc::new(Mutex::new(None));
        let worker_parked = parked_mute.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let error_state = Arc::new(Mutex::new(None));
        let worker_error = Arc::clone(&error_state);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("oink-audio-tap".into())
            .spawn(move || {
                // Establish the write-ahead endpoint lease BEFORE native tap
                // start. New app clients inherit hardware mute without tap
                // rebuilding. Already-muted outputs remain the user's mute.
                // A rebuild of the same forwarding adopts the retired capture's
                // lease: the endpoint never unmutes between the two taps.
                let held = held.map(|held| SourceMute { lease: Some(held.0), owned: worker_muted.clone(), cleanup: worker_cleanup.clone() });
                let (lease_uid, mut mute_lease) = match held {
                    Some(mut held) if mute => {
                        let lease = held.lease.as_mut().expect("held lease");
                        lease.rebind_owner();
                        (Some(lease.uid().to_owned()), Some(held))
                    }
                    held => {
                        // An unmuted capture ends a held mute: dropping restores it.
                        drop(held);
                        let lease_uid = if mute {
                            match selected_uid.as_ref().map(|uid| uid.to_string_lossy().into_owned())
                                .map(Ok).unwrap_or_else(Self::current_default_output_id) {
                                Ok(uid) => Some(uid),
                                Err(error) => { let _ = ready_tx.send(Err(error)); return; }
                            }
                        } else { None };
                        let mute_lease = match (recovery_dir.as_deref(), lease_uid.as_deref()) {
                            (Some(dir), Some(uid)) => match crate::macos_mute::MuteLease::start(dir, uid) {
                                Ok(lease) => Some(SourceMute { lease: Some(lease), owned: worker_muted.clone(), cleanup: worker_cleanup.clone() }),
                                Err(error) => { let _ = ready_tx.send(Err(error)); return; }
                            },
                            _ => None,
                        };
                        (lease_uid, mute_lease)
                    }
                };
                worker_muted.store(mute_lease.is_some(), Ordering::Release);
                // Bind the tap to the exact output we muted, closing a default
                // switch between lease acquisition and tap construction.
                let binding = lease_uid.as_deref().map(|uid| CString::new(uid).expect("validated endpoint UID"));
                let mut rate = 0.0;
                let mut error = [0 as c_char; 1024];
                // SAFETY: Both output pointers are valid for their declared
                // lengths. The returned handle is used only by this worker.
                let raw = unsafe {
                    oink_audio_capture_start(
                        false,
                        requested.unwrap_or(u32::MAX),
                        capture_mode,
                        binding.as_ref().or(selected_uid.as_ref()).map_or(std::ptr::null(), |uid| uid.as_ptr()),
                        buffer_before_tap,
                        tap_private,
                        collect_diagnostics,
                        true,
                        &mut rate,
                        error.as_mut_ptr(),
                        error.len(),
                    )
                };
                if raw.is_null() {
                    // SAFETY: The bridge always writes a NUL-terminated error
                    // on failure, and the array was initialized with zeroes.
                    let message = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned();
                    let _ = ready_tx.send(Err(message));
                    return;
                }
                // An RAII guard also stops the tap if worker code unwinds.
                let mut native = NativeCapture::new(raw, worker_omitted);
                let mut native_info = NativeInfo::default();
                // SAFETY: NativeInfo matches the bridge's C layout; handle is live.
                unsafe { oink_audio_capture_info(native.0, &mut native_info) };
                let period_frames = native_info.buffer_frames;
                let mut info = native_info.into_info(requested);
                let mut physical_uid = [0 as c_char; 512];
                // SAFETY: The handle is live; the bridge writes a bounded C string
                // on success, and the destination is initialized with zeroes.
                if unsafe { oink_audio_capture_physical_uid(native.0, physical_uid.as_mut_ptr(), physical_uid.len()) } {
                    // SAFETY: Successful bridge call produced a NUL-terminated UID.
                    info.physical_output_id = Some(unsafe { CStr::from_ptr(physical_uid.as_ptr()) }.to_string_lossy().into_owned());
                }
                if selected_uid.is_some() {
                    let selection = "Explicit output binding; system default output unchanged; default-output changes do not alter this binding";
                    info.fallback_reason = Some(match info.fallback_reason.take() {
                        Some(reason) => format!("{selection}; {reason}"),
                        None => selection.into(),
                    });
                }
                if collect_diagnostics {
                    info.diagnostics = native_diagnostics(native.0);
                }
                let mut realtime = match RealtimeCapture::start(native.0, rate, period_frames, encoder) {
                    Ok(worker) => worker,
                    Err(error) => { let _ = ready_tx.send(Err(error)); return; }
                };
                if ready_tx.send(Ok(info)).is_err() {
                    return;
                }
                let clock = HostClock::new();
                let mut events = events::Events::new(mute, &clock);
                let mut last_check = Instant::now();
                while !stopping.load(Ordering::Acquire) {
                    if let Some(lease) = mute_lease.as_mut().and_then(|mute| mute.lease.as_mut()) {
                        match lease.check_ownership() {
                            Ok(true) => { worker_controls.store(true, Ordering::Release); break; }
                            Err(error) => { *worker_error.lock().unwrap() = Some(error); break; }
                            Ok(false) => {}
                        }
                        if lease.default_changed() {
                            worker_default.store(true, Ordering::Release);
                            break;
                        }
                    }
                    events.drain(native.0, &clock);
                    if last_check.elapsed() >= Duration::from_secs(1) {
                        let check_started = Instant::now();
                        // SAFETY: The native handle is live and error is writable
                        // for its full length. A failed check writes a C string.
                        let health = unsafe { oink_audio_capture_check(native.0, error.as_mut_ptr(), error.len()) };
                        let elapsed = check_started.elapsed();
                        events.health.observe(elapsed);
                        if elapsed >= Duration::from_millis(20) {
                            tracing::info!(start_us = crate::diagnostics::monotonic_us(check_started),
                                elapsed_us = elapsed.as_micros() as u64, health, "audio capture health check slow");
                        }
                        if health != 0 {
                            // SAFETY: The check writes a bounded NUL-terminated
                            // message on every failure path.
                            let message = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned();
                            // Restart requires successful cleanup, not merely
                            // invoking Drop. A failed HAL teardown is terminal.
                            realtime.stop();
                            let default_changed = mute && Self::current_default_output_id().ok().as_ref() != lease_uid.as_ref();
                            if default_changed { worker_default.store(true, Ordering::Release); }
                            let exit_health = if default_changed { 1 } else { health };
                            let cleanup = native.finish();
                            if let Err(error) = &cleanup { *worker_cleanup.lock().unwrap() = Some(error.clone()); }
                            let (restart_reason, message) = capture_exit(exit_health, message, cleanup);
                            if let Some(reason) = &restart_reason {
                                tracing::info!(%reason, "audio capture release confirmed; rebuilding via generation handshake");
                            }
                            if let Ok(mut state) = worker_restart.lock() {
                                *state = restart_reason;
                            }
                            if let Ok(mut state) = worker_error.lock() {
                                *state = Some(message);
                            }
                            break;
                        }
                        last_check = Instant::now();
                    }
                    if realtime.finished() {
                        if let Ok(mut state) = worker_error.lock() { *state = Some(format!("audio capture realtime worker stopped: {}", realtime.error())); }
                        break;
                    }
                    // PCM onset wakes the encoder independently. Endpoint observers
                    // unpark this owner immediately; only health/diagnostics need a timer.
                    thread::park_timeout(Duration::from_secs(1));
                }
                realtime.stop();
                if let Err(error) = native.finish()
                    && let Ok(mut state) = worker_error.lock()
                {
                    *worker_cleanup.lock().unwrap() = Some(error.clone());
                    *state = Some(error);
                }
                // The tap is gone; the mute is not. Only the owner knows whether
                // this exit ends forwarding (restore) or is a rebuild (handover).
                worker_muted.store(false, Ordering::Release);
                if let Some(mute) = mute_lease.take() {
                    *worker_parked.lock().unwrap() = Some(mute);
                }
            })
            .map_err(|error| format!("Start audio capture worker: {error}"))?;
        match ready_rx.recv() {
            Ok(Ok(info)) => Ok(Self {
                muted_owned,
                local_controls_changed,
                default_output_changed,
                cleanup_error,
                info,
                stop,
                worker: Some(worker),
                error: error_state,
                restart,
                omitted_silent_frames,
                parked_mute,
            }),
            result => {
                let _ = worker.join();
                let error = match result {
                    Ok(Err(error)) => error,
                    _ => "Audio capture worker stopped during initialization".into(),
                };
                match cleanup_error.lock().ok().and_then(|error| error.clone()) {
                    Some(cleanup) => Err(format!("capture_mute_release_unconfirmed: {error}; {cleanup}")),
                    None => Err(error),
                }
            }
        }
    }

    pub fn source_muted(&self) -> bool {
        self.muted_owned.load(Ordering::Acquire) && self.error().is_none() && !self.local_controls_changed()
    }

    pub fn local_controls_changed(&self) -> bool {
        self.local_controls_changed.load(Ordering::Acquire)
    }
    pub fn default_output_changed(&self) -> bool {
        self.default_output_changed.load(Ordering::Acquire)
    }
    /// Join IO and restore the lease before the owner publishes stop/off state.
    pub fn finish(&mut self) -> Option<String> {
        self.join_worker();
        // Dropping the parked lease pauses media and restores local output.
        drop(self.parked_mute.lock().ok().and_then(|mut parked| parked.take()));
        self.cleanup_result()
    }

    /// Join IO for a rebuild within the same forwarding session. The mute lease
    /// is returned for `continue_encoded` instead of being restored, unless it
    /// already lost exact ownership (then it is restored or released here).
    pub fn finish_keep_mute(&mut self) -> (Option<String>, Option<HeldSourceMute>) {
        self.join_worker();
        let held = self
            .parked_mute
            .lock()
            .ok()
            .and_then(|mut parked| parked.take())
            .and_then(SourceMute::into_held);
        (self.cleanup_result(), held)
    }

    fn join_worker(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }

    fn cleanup_result(&self) -> Option<String> {
        let cleanup = self.cleanup_error.lock().ok().and_then(|error| error.clone());
        if let Some(error) = &cleanup {
            if let Ok(mut state) = self.error.lock() {
                *state = Some(error.clone());
            }
            // A failed lease release must not announce a successful native
            // rebuild that could lose its pending old journal.
            if let Ok(mut restart) = self.restart.lock() {
                *restart = None;
            }
        }
        cleanup
    }

    /// Runtime errors stop the native tap immediately, restoring local sound.
    /// A worker panic or unexpected exit is also surfaced to the caller.
    pub fn error(&self) -> Option<String> {
        if let Ok(state) = self.error.lock()
            && let Some(error) = state.as_ref()
        {
            return Some(error.clone());
        }
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            return Some("System audio capture worker stopped".into());
        }
        None
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn native_diagnostics(capture: *mut c_void) -> serde_json::Value {
    // SAFETY: Live worker-owned handle; null buffer asks only for required size.
    let size = unsafe { oink_audio_capture_diagnostics(capture, std::ptr::null_mut(), 0) };
    if size == 0 || size > 4 * 1024 * 1024 {
        return serde_json::json!({ "read_error": "Native diagnostics size unavailable or exceeds bound", "size": size });
    }
    let mut buffer = vec![0_u8; size];
    // SAFETY: Buffer has the full size returned above; C writes only within it.
    let required = unsafe { oink_audio_capture_diagnostics(capture, buffer.as_mut_ptr().cast(), buffer.len()) };
    if required == 0 || required > buffer.len() {
        return serde_json::json!({ "read_error": "Native diagnostics changed size", "size": required });
    }
    serde_json::from_slice(&buffer[..required - 1]).unwrap_or_else(
        |error| serde_json::json!({ "read_error": format!("Invalid native diagnostics JSON: {error}") }),
    )
}

impl NativeInfo {
    fn into_info(self, requested: Option<u32>) -> CaptureInfo {
        let known = |value| (value != u32::MAX).then_some(value);
        let mut reasons = Vec::new();
        if self.tap_sample_rate != self.sample_rate {
            reasons.push(format!("Tap format is {} Hz; aggregate input IO is {} Hz; native timestamps and frame counts use aggregate IO rate", self.tap_sample_rate, self.sample_rate));
        }
        if self.physical_buffer_request_status != 0 {
            reasons.push(format!(
                "Physical output buffer lease unavailable: Core Audio status {}",
                self.physical_buffer_request_status
            ));
        }
        if self.physical_lease_changed != 0 {
            reasons.push("Physical output buffer is device-global; graceful stop restores only if current equals leased value; crash recovery unavailable and same-value external writes indistinguishable".into());
        }
        if self.buffer_request_status != 0 {
            reasons.push(format!(
                "Private aggregate buffer request failed: Core Audio status {}",
                self.buffer_request_status
            ));
        }
        if requested.is_some_and(|frames| frames > 0 && frames != self.buffer_frames) {
            reasons.push(format!(
                "Private aggregate uses {} frames after request",
                self.buffer_frames
            ));
        }
        if self.physical_sample_rate != self.sample_rate {
            reasons.push(format!(
                "Physical output frame counts use {} Hz (aggregate input IO uses {} Hz)",
                self.physical_sample_rate, self.sample_rate
            ));
        }
        CaptureInfo {
            backend: match self.capture_mode {
                1 => "core_audio_device_tap_physical_clock_no_drift",
                2 => "core_audio_device_tap_physical_clock_drift",
                3 => "core_audio_global_tap_physical_clock_drift_experiment",
                4 => "core_audio_global_tap_physical_clock_no_drift_experiment",
                5 => "core_audio_device_tap_without_physical_master_no_drift",
                6 => "core_audio_device_tap_without_physical_master_drift",
                7 => "core_audio_global_tap_without_physical_master_no_drift",
                _ => "core_audio_private_process_tap",
            }
            .into(),
            sample_rate: self.sample_rate as u32,
            requested_buffer_frames: requested,
            actual_buffer_frames: known(self.buffer_frames).unwrap_or(0),
            reported_latency_frames: known(self.latency_frames),
            safety_offset_frames: known(self.safety_frames),
            physical_output_sample_rate: (self.physical_sample_rate > 0.0).then_some(self.physical_sample_rate as u32),
            physical_output_buffer_frames: known(self.physical_buffer_frames),
            physical_output_original_buffer_frames: known(self.physical_original_buffer_frames),
            physical_output_buffer_lease_changed: self.physical_lease_changed != 0,
            physical_output_latency_frames: known(self.physical_latency_frames),
            physical_output_safety_offset_frames: known(self.physical_safety_frames),
            timestamp_source: "IOProc inputDataTime.mHostTime; callback mach_absolute_time; ring frame offsets".into(),
            fallback_reason: (!reasons.is_empty()).then(|| reasons.join("; ")),
            ..CaptureInfo::default()
        }
    }
}

// Anchor independent clock domains with a short bracket, without assuming
// Instant's private representation or substituting worker dequeue time.
struct HostClock {
    host_ns: u64,
    instant: Instant,
    bracket_us: u64,
}
impl HostClock {
    fn new() -> Self {
        let before = Instant::now();
        // SAFETY: Pure monotonic clock read, no pointers or state requirements.
        let host_ns = unsafe { oink_audio_host_time_ns() };
        let after = Instant::now();
        Self {
            host_ns,
            instant: before + after.duration_since(before) / 2,
            bracket_us: after.duration_since(before).as_micros() as u64,
        }
    }
    fn instant(&self, ns: u64) -> Option<Instant> {
        if ns == 0 {
            return None;
        }
        if ns >= self.host_ns {
            self.instant.checked_add(Duration::from_nanos(ns - self.host_ns))
        } else {
            self.instant.checked_sub(Duration::from_nanos(self.host_ns - ns))
        }
    }
}

// The ordinary owner keeps native alive and must join this guard before any
// mute restoration or HAL close, including unwind and device-change paths.
#[derive(Clone, Copy, Debug)]
struct CaptureReader(*mut c_void);
// SAFETY: only the RT child calls read/wait. Ordinary owner may wake and query
// health/events; those fields are immutable or independent atomic SPSC state.
unsafe impl Send for CaptureReader {}
impl CaptureReader {
    fn raw(&self) -> *mut c_void {
        self.0
    }
}
type CaptureRealtimeResources = (
    Box<crate::source::Encoder>,
    StereoResampler,
    Vec<f32>,
    crate::realtime::Schedule,
);
struct RealtimeCapture {
    reader: CaptureReader,
    stop: Arc<AtomicBool>,
    state: Arc<[std::sync::atomic::AtomicI32; 2]>,
    worker: Option<JoinHandle<CaptureRealtimeResources>>,
}
impl RealtimeCapture {
    fn start(
        raw: *mut c_void,
        rate: f64,
        period_frames: u32,
        mut encoder: Box<crate::source::Encoder>,
    ) -> Result<Self, String> {
        if !rate.is_finite() || !(8_000.0..=768_000.0).contains(&rate) {
            return Err("unsupported capture rate for bounded realtime conversion".into());
        }
        // SAFETY: ordinary owner has completed native startup and retains it.
        let device = unsafe { oink_audio_capture_device_id(raw) };
        let period_ns = (f64::from(period_frames) * 1e9 / rate).round() as u64;
        let mut schedule = crate::realtime::Schedule::device(device, period_ns)?;
        let target_rate = encoder.config().sample_rate;
        if encoder.config().codec == crate::dsp::Codec::Pcm24 && rate.round() as u32 != target_rate {
            return Err("source sample rate changed during audio preparation".into());
        }
        let mut resampler = StereoResampler::with_rates(rate, f64::from(target_rate), 4096);
        let mut output = vec![0.0; resampler.output_capacity(8192)];
        let clock = HostClock::new();
        let reader = CaptureReader(raw);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let state = Arc::new([
            std::sync::atomic::AtomicI32::new(0),
            std::sync::atomic::AtomicI32::new(0),
        ]);
        let status = state.clone();
        let owner = thread::current();
        let worker = thread::Builder::new()
            .name("oink-audio-encode".into())
            .spawn(move || {
                let mut input = [0.0; 8192];
                let mut expected_sequence = None;
                let mut first = true;
                let mut previous: Option<Instant> = None;
                {
                    match schedule.enter() {
                        Ok(guard) => {
                            status[0].store(1, Ordering::Release);
                            while !stopping.load(Ordering::Acquire) && encoder.active() {
                                // Scheduling boundary, outside the bounded work unit.
                                // SAFETY: native owner outlives this joined child.
                                let error = unsafe { oink_audio_capture_wait(reader.raw()) };
                                if error != 0 {
                                    status[1].store(error, Ordering::Release);
                                    break;
                                }
                                if stopping.load(Ordering::Acquire) {
                                    break;
                                }
                                let now = Instant::now();
                                for _ in 0..8 {
                                    let (mut host_ns, mut callback_ns, mut sequence) = (0, 0, 0);
                                    // SAFETY: sole consumer; fixed 4096 stereo-frame
                                    // output and writable metadata match the C API.
                                    let frames = unsafe {
                                        oink_audio_capture_read(
                                            reader.raw(),
                                            input.as_mut_ptr(),
                                            input.len() / 2,
                                            &mut host_ns,
                                            &mut callback_ns,
                                            &mut sequence,
                                        )
                                    };
                                    if frames == 0 {
                                        break;
                                    }
                                    let discontinuity =
                                        first || expected_sequence.is_some_and(|expected| sequence != expected);
                                    if discontinuity {
                                        resampler.reset();
                                    }
                                    expected_sequence = Some(sequence + frames as u64);
                                    first = false;
                                    let (samples, mut stamp) = resampler.process_timed_into(
                                        &input[..frames * 2],
                                        clock.instant(host_ns),
                                        clock.instant(callback_ns),
                                        sequence,
                                        &mut output,
                                    );
                                    stamp.discontinuity = discontinuity;
                                    if samples > 0 {
                                        encoder.supply(&output[..samples], stamp);
                                    }
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
                owner.unpark();
                (encoder, resampler, output, schedule)
            })
            .map_err(|e| format!("start capture realtime worker: {e}"))?;
        let mut child = Self {
            reader,
            stop,
            state,
            worker: Some(worker),
        };
        let started = Instant::now();
        while child.state[0].load(Ordering::Acquire) == 0 && started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(1));
        }
        if child.state[0].load(Ordering::Acquire) != 1 {
            child.stop();
            return Err(format!("capture realtime scheduling failed: {}", child.error()));
        }
        // A callback may have published audio before the child joined.
        // SAFETY: live native capture with a prepared semaphore.
        unsafe {
            oink_audio_capture_wake(raw);
        }
        Ok(child)
    }
    fn stop(&mut self) {
        if self.worker.is_none() {
            return;
        }
        self.stop.store(true, Ordering::Release);
        // SAFETY: owner still holds native; stop is always before native.finish.
        unsafe {
            oink_audio_capture_wake(self.reader.raw());
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

/// Native ring (stereo samples + host/callback/sequence per frame) and the
/// diagnostic event ring of one native capture, from `native/macos.m`.
const NATIVE_CAPTURE_BYTES: usize = 8192 * (2 * 4 + 3 * 8) + 1024 * 56;

/// Full-system echo reference: a process tap of the exact output device Oink
/// plays to, including Oink's own process, polled on a workgroup thread that
/// only copies into the reference handoff. Returns when `stop` is set (Ok) or
/// with the reason the Oink playback reference must be used instead.
pub(crate) fn run_system_reference(
    control: &Arc<crate::echo::Control>,
    output_id: &str,
    stop: &AtomicBool,
) -> Result<(), (crate::echo::SystemStatus, String)> {
    use crate::{
        CapturePreflight,
        echo::{SystemFeed, SystemPacket, SystemStatus},
    };
    let uid = output_id.strip_prefix("coreaudio:").ok_or_else(|| {
        (
            SystemStatus::Unsupported,
            format!("not a CoreAudio output: {output_id}"),
        )
    })?;
    // Only an existing grant: the echo reference never presents the system
    // prompt and never warns in the UI; the speaker-forwarding flow asks.
    match crate::capture_permission_preflight() {
        CapturePreflight::Authorized => {}
        state => {
            return Err((
                SystemStatus::PermissionDenied,
                format!("System Audio Recording not granted ({state:?})"),
            ));
        }
    }
    if Capture::cleanup_failed() {
        return Err((
            SystemStatus::StartFailed,
            "an earlier tap cleanup failed; taps are disabled for this process".into(),
        ));
    }
    let uid = CString::new(uid).map_err(|_| (SystemStatus::Unsupported, "output UID contains NUL".into()))?;
    let mut rate = 0.0;
    let mut error = [0 as c_char; 1024];
    // SAFETY: valid C string and writable outputs; the handle is owned below.
    let raw = unsafe { oink_audio_reference_tap_start(uid.as_ptr(), &mut rate, error.as_mut_ptr(), error.len()) };
    if raw.is_null() {
        // SAFETY: the bridge writes a NUL-terminated message on failure.
        let message = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned();
        let status = if message.starts_with("capture_permission_denied") {
            SystemStatus::PermissionDenied
        } else if message.contains("output-only")
            || message.contains("exactly one output stream")
            || message.contains("macOS 14.2")
            || message.contains("Unsupported system audio format")
        {
            SystemStatus::Unsupported
        } else {
            SystemStatus::StartFailed
        };
        return Err((status, message));
    }
    let mut native = NativeCapture::new(raw, Arc::new(CaptureProgress::default()));
    let mut info = NativeInfo::default();
    // SAFETY: live handle; NativeInfo matches the bridge layout.
    unsafe { oink_audio_capture_info(native.0, &mut info) };
    let period_frames = if info.buffer_frames == u32::MAX {
        512
    } else {
        info.buffer_frames
    };
    let period_ns = (f64::from(period_frames) * 1e9 / rate).round() as u64;
    // SAFETY: live handle owned by this thread.
    let device = unsafe { oink_audio_capture_device_id(native.0) };
    let result = (|| {
        let mut schedule =
            crate::realtime::Schedule::device(device, period_ns).map_err(|e| (SystemStatus::StartFailed, e))?;
        let clock = HostClock::new();
        // The tap delivers exact zeros itself while nothing plays, so a
        // delivery gap is lost audio, never silence.
        let mut feed = SystemFeed::new(
            control.clone(),
            rate.round() as u32,
            false,
            Duration::from_millis(50),
            NATIVE_CAPTURE_BYTES,
        );
        tracing::info!(
            rate,
            period_frames,
            latency_frames = info.latency_frames,
            physical_latency_frames = info.physical_latency_frames,
            physical_safety_frames = info.physical_safety_frames,
            "full-system echo reference tap running"
        );
        let guard = schedule
            .enter()
            .map_err(|e| (SystemStatus::StartFailed, format!("enter audio workgroup: {e}")))?;
        let mut input = [0.0f32; 4096 * 2];
        let mut last_check = Instant::now();
        let mut failure = None;
        while !stop.load(Ordering::Acquire) {
            for _ in 0..8 {
                let (mut host_ns, mut callback_ns, mut sequence) = (0, 0, 0);
                // SAFETY: sole reader; fixed 4096 stereo frames of output.
                let frames = unsafe {
                    oink_audio_capture_read(
                        native.0,
                        input.as_mut_ptr(),
                        input.len() / 2,
                        &mut host_ns,
                        &mut callback_ns,
                        &mut sequence,
                    )
                };
                if frames == 0 {
                    break;
                }
                feed.packet(SystemPacket {
                    samples: &input[..frames * 2],
                    stamp: clock.instant(host_ns),
                    position: Some(sequence),
                    discontinuity: false,
                    arrived: Instant::now(),
                });
            }
            let now = Instant::now();
            feed.account(now);
            if now.saturating_duration_since(last_check) >= Duration::from_secs(1) {
                last_check = now;
                let mut message = [0 as c_char; 1024];
                // SAFETY: live handle; bounded writable error buffer.
                let health = unsafe { oink_audio_capture_check(native.0, message.as_mut_ptr(), message.len()) };
                if health != 0 {
                    // SAFETY: a failed check writes a NUL-terminated message.
                    let text = unsafe { CStr::from_ptr(message.as_ptr()) }
                        .to_string_lossy()
                        .into_owned();
                    let status = if health == 2 {
                        SystemStatus::OutputChanged
                    } else {
                        SystemStatus::CaptureFailed
                    };
                    failure = Some((status, text));
                    break;
                }
            }
            // Polled: copies stay off the HAL thread, the owner can stop it
            // without a wake handle, and lateness is measured, not assumed.
            thread::sleep(Duration::from_millis(2));
        }
        drop(guard);
        match failure {
            Some((status, text)) => {
                feed.fail(status, text.clone());
                Err((status, text))
            }
            None => Ok(()),
        }
    })();
    let cleanup = native.finish();
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err((SystemStatus::CaptureFailed, error)),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Only ordinary control/teardown threads take this lock; the HAL callback and
/// encoder never do. A 1s health-poll mirror can be stale past the watchdog's
/// 500ms deadline even while callbacks continuously omit healthy digital zeros.
#[derive(Debug, Default)]
struct CaptureProgress(Mutex<Option<CaptureReader>>);
impl CaptureProgress {
    fn frames(&self) -> u64 {
        let native = self.0.lock().unwrap_or_else(|e| e.into_inner());
        native.as_ref().map_or(0, |reader| {
            // SAFETY: NativeCapture invalidates this slot under the same lock
            // before stopping/freeing HAL, including error and unwind paths.
            unsafe { oink_audio_capture_omitted_silent_frames(reader.raw()) }
        })
    }
}

struct NativeCapture(*mut c_void, Arc<CaptureProgress>);

impl NativeCapture {
    fn new(raw: *mut c_void, progress: Arc<CaptureProgress>) -> Self {
        *progress.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(CaptureReader(raw));
        Self(raw, progress)
    }
    fn finish(&mut self) -> Result<(), String> {
        self.finish_with(|raw| {
            let mut error = [0 as c_char; 1024];
            // SAFETY: Unique live handle, consumed once. HAL either frees its
            // storage after proven quiescence or intentionally retains it.
            let status = unsafe { oink_audio_capture_stop(raw, error.as_mut_ptr(), error.len()) };
            if status == 0 {
                return Ok(());
            }
            // SAFETY: Native cleanup initializes and bounds the error string.
            let detail = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy();
            let message = format!(
                "Capture cleanup failed (status {status}); tap/mute release may be incomplete; \
                 capture is disabled for this process. Exit, verify local audio, then restart the application: {detail}"
            );
            tracing::error!(%message);
            Err(message)
        })
    }

    fn finish_with(&mut self, shutdown: impl FnOnce(*mut c_void) -> Result<(), String>) -> Result<(), String> {
        // Wait for any in-flight atomic read, then prevent all future native
        // reads before cleanup. Even a retained/failed HAL context is retired.
        *self.1.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let raw = std::mem::replace(&mut self.0, std::ptr::null_mut());
        if raw.is_null() { Ok(()) } else { shutdown(raw) }
    }
}

fn capture_exit(health: i32, reason: String, cleanup: Result<(), String>) -> (Option<String>, String) {
    match cleanup {
        Ok(()) => ((health == 2).then(|| reason.clone()), reason),
        Err(error) => (None, format!("{reason}; {error}")),
    }
}

impl Drop for NativeCapture {
    fn drop(&mut self) {
        // Covers early return/unwind. Explicit finish already consumed the
        // pointer, including on failure; a retained HAL context is never freed
        // by a second cleanup attempt. finish logs failures on this path too.
        let _ = self.finish();
    }
}

// Continuous interpolation carries one frame and fractional position across
// HAL callback boundaries; never round each buffer's resampled length.
use crate::resample::StereoResampler;

#[cfg(test)]
impl StereoResampler {
    #[cfg(test)]
    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        self.process_timed(input, None, None, 0).samples
    }

    #[cfg(test)]
    fn process_timed(
        &mut self,
        input: &[f32],
        first_sample_at: Option<Instant>,
        callback_at: Option<Instant>,
        sequence: u64,
    ) -> CaptureBlock {
        let mut samples = vec![0.0; self.output_capacity(input.len())];
        let (count, stamp) = self.process_timed_into(input, first_sample_at, callback_at, sequence, &mut samples);
        samples.truncate(count);
        CaptureBlock {
            samples,
            first_sample_at: stamp.first_sample_at,
            callback_at: stamp.callback_at,
            source_frame_position: stamp.source_frame_position,
        }
    }
}

pub(crate) fn log_thread_scheduling(raw: *mut c_void, role: &str) {
    let (mut qos, mut relative, mut policy, mut priority, mut error) = (0, 0, 0, 0, 0);
    // SAFETY: Null selects current thread; otherwise caller holds a live capture.
    unsafe { oink_audio_thread_scheduling(raw, &mut qos, &mut relative, &mut policy, &mut priority, &mut error) };
    tracing::info!(
        role,
        qos,
        relative,
        policy,
        priority,
        error,
        "audio thread scheduling (Mach policy/priority; requested QoS, -1=unavailable; read only)"
    );
}

#[cfg(test)]
mod tests {
    use super::{NativeCapture, StereoResampler, capture_exit};

    #[test]
    fn silent_callback_progress_is_visible_without_the_one_second_health_poll() {
        use super::*;
        unsafe extern "C" {
            fn oink_audio_capture_test_silent_new() -> *mut c_void;
            fn oink_audio_capture_test_silent_callback(raw: *mut c_void);
            fn oink_audio_capture_test_silent_free(raw: *mut c_void);
        }
        let capture = Capture {
            muted_owned: Arc::new(AtomicBool::new(false)),
            local_controls_changed: Arc::new(AtomicBool::new(false)),
            default_output_changed: Arc::new(AtomicBool::new(false)),
            cleanup_error: Arc::new(Mutex::new(None)),
            info: NativeInfo::default().into_info(None),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
            error: Arc::new(Mutex::new(None)),
            restart: Arc::new(Mutex::new(None)),
            omitted_silent_frames: Arc::new(CaptureProgress::default()),
            parked_mute: Arc::new(Mutex::new(None)),
        };
        // SAFETY: the fixture allocates a ring/semaphore only, without a HAL
        // device, permission request, mute lease or settings change.
        let raw = unsafe { oink_audio_capture_test_silent_new() };
        assert!(!raw.is_null());
        let mut native = NativeCapture::new(raw, capture.omitted_silent_frames.clone());
        // No owner health/diagnostics poll runs between these callbacks and reads.
        // The watchdog's 500ms deadline is shorter than that owner's 1s poll.
        let mut observed = Vec::new();
        for _ in 1..=400 {
            // SAFETY: owned fixture; invokes the production exact-zero callback.
            unsafe { oink_audio_capture_test_silent_callback(raw) };
            observed.push(capture.omitted_silent_frames());
        }
        native
            .finish_with(|raw| {
                // SAFETY: no HAL lifetime; destroy only this fixture's ring/semaphore.
                unsafe { oink_audio_capture_test_silent_free(raw) };
                Ok(())
            })
            .unwrap();
        assert_eq!(capture.omitted_silent_frames(), 0);
        assert_eq!(observed, (1..=400).map(|step| step * 128).collect::<Vec<_>>());
    }

    #[test]
    fn muted_capture_requires_durable_recovery_before_any_native_call() {
        let (encoder, _) = crate::source::channel(crate::dsp::Config::low_latency_opus(), 100, false).unwrap();
        let error = super::Capture::start_encoded(encoder, true, super::MacCaptureOptions::default()).unwrap_err();
        assert!(error.contains("durable recovery directory"));
    }

    #[test]
    fn supported_format_change_requests_internal_rebuild_without_masking_invalid_formats() {
        // SAFETY: pure native classifier; no HAL call, tap or endpoint mutation.
        unsafe {
            assert_eq!(super::oink_audio_capture_format_health(0, true, true), 2);
            assert_eq!(super::oink_audio_capture_format_health(0, true, false), 0);
            assert_eq!(super::oink_audio_capture_format_health(-1, true, true), 1);
            assert_eq!(super::oink_audio_capture_format_health(0, false, true), 1);
        }
    }

    #[test]
    fn cleanup_failure_blocks_default_device_restart_and_preserves_both_errors() {
        let (restart, error) = capture_exit(2, "default output changed".into(), Err("AudioDeviceStop failed".into()));
        assert!(restart.is_none());
        assert!(error.contains("default output changed"));
        assert!(error.contains("AudioDeviceStop failed"));
        let (restart, _) = capture_exit(2, "default output changed".into(), Ok(()));
        assert_eq!(restart.as_deref(), Some("default output changed"));
        assert!(capture_exit(1, "aggregate lost".into(), Ok(())).0.is_none());
    }

    #[test]
    fn explicit_shutdown_consumes_handle_once_even_when_cleanup_fails() {
        // Rust-owned memory and an injected closure: no CoreAudio call occurs.
        for fail in [false, true] {
            let raw = Box::into_raw(Box::new(0_u8)).cast();
            let progress = std::sync::Arc::new(super::CaptureProgress::default());
            let mut native = NativeCapture::new(raw, progress.clone());
            let result = native.finish_with(|handle| {
                assert_eq!(handle, raw);
                // SAFETY: The test allocated this pointer immediately above.
                unsafe { drop(Box::from_raw(handle.cast::<u8>())) };
                if fail { Err("HAL cleanup failed".into()) } else { Ok(()) }
            });
            assert_eq!(result.is_err(), fail);
            assert!(progress.0.lock().unwrap().is_none());
            native.finish_with(|_| panic!("cleanup repeated")).unwrap();
            // Drop sees the consumed handle, including after the error path.
            drop(native);
        }
    }

    #[test]
    fn resampling_is_independent_of_capture_buffer_boundaries() {
        let input: Vec<f32> = (0..8820).map(|sample| (sample as f32 * 0.02).sin()).collect();
        let whole = StereoResampler::new(44_100.0).process(&input);
        let mut split = StereoResampler::new(44_100.0);
        let chunks: Vec<f32> = input.chunks(254).flat_map(|part| split.process(part)).collect();
        assert_eq!(chunks.len(), whole.len());
        assert!((chunks.len() as i64 - 9600).abs() <= 4);
        assert!(chunks.iter().zip(&whole).all(|(a, b)| (a - b).abs() < 0.0001));
    }

    #[test]
    fn timed_resampling_tracks_first_output_sample_across_callbacks() {
        use std::time::{Duration, Instant};
        let epoch = Instant::now();
        for rate in [8_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let mut converter = StereoResampler::new(rate);
            let mut emitted = 0;
            for index in 0..100 {
                let sequence = index * 127;
                let first = epoch + Duration::from_secs_f64(sequence as f64 / rate);
                let block = converter.process_timed(
                    &[0.25; 254],
                    Some(first),
                    Some(first + Duration::from_millis(5)),
                    sequence,
                );
                if !block.samples.is_empty() {
                    let expected = epoch + Duration::from_secs_f64(emitted as f64 / 48_000.0);
                    let actual = block.first_sample_at.unwrap();
                    let error = if actual > expected {
                        actual - expected
                    } else {
                        expected - actual
                    };
                    assert!(error < Duration::from_nanos(5), "rate={rate}, error={error:?}");
                    let expected_position = (emitted as f64 * rate / 48_000.0).floor() as i64;
                    assert!((block.source_frame_position.unwrap() as i64 - expected_position).abs() <= 1);
                    emitted += block.samples.len() / 2;
                }
            }
        }
    }

    #[test]
    fn fixed_capture_conversion_matches_samples_and_timestamps_without_heap_work() {
        use std::time::{Duration, Instant};
        let epoch = Instant::now();
        let input: Vec<_> = (0..8192).map(|i| (i as f32 * 0.02).sin()).collect();
        let mut output = vec![0.0; 50_000];
        for rate in [8_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0, 768_000.0] {
            let mut old = StereoResampler::new(rate);
            let mut fixed = StereoResampler::new(rate);
            for i in 0..100 {
                if i == 50 {
                    old.reset();
                    fixed.reset();
                }
                let sequence = i * 4096;
                let at = epoch + Duration::from_secs_f64(sequence as f64 / rate);
                let expected = old.process_timed(&input, Some(at), Some(at), sequence);
                crate::allocation_audit::assert_no_heap(|| {
                    let (count, stamp) = fixed.process_timed_into(&input, Some(at), Some(at), sequence, &mut output);
                    assert_eq!(&output[..count], expected.samples.as_slice());
                    assert_eq!(stamp.first_sample_at, expected.first_sample_at);
                    assert_eq!(stamp.callback_at, expected.callback_at);
                    assert_eq!(stamp.source_frame_position, expected.source_frame_position);
                });
            }
        }
    }

    #[test]
    fn missing_hardware_time_is_never_replaced_with_worker_time() {
        let mut converter = StereoResampler::new(44_100.0);
        let block = converter.process_timed(&[0.25; 254], None, None, 0);
        assert!(block.first_sample_at.is_none());
        assert!(block.callback_at.is_none());
    }

    #[test]
    fn resampling_preserves_stereo_and_dc() {
        for rate in [8_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let mut converter = StereoResampler::new(rate);
            let mut output = Vec::new();
            for _ in 0..256 {
                output.extend(converter.process(&[0.25, -0.5, 0.25, -0.5]));
            }
            assert!(!output.is_empty());
            let settled = if rate == 48_000.0 {
                0
            } else {
                (96.0 * (rate / 48_000.0).floor().max(1.0) * 48_000.0 / rate).ceil() as usize
            };
            assert!(
                output.as_chunks::<2>().0[settled..]
                    .iter()
                    .all(|frame| (frame[0] - 0.25).abs() < 1e-6 && (frame[1] + 0.5).abs() < 1e-6)
            );
        }
    }
}
