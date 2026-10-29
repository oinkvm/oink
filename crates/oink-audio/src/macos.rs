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
    lease: crate::macos_mute::MuteLease,
    owned: Arc<AtomicBool>,
    cleanup: Arc<Mutex<Option<String>>>,
}
impl Drop for SourceMute {
    fn drop(&mut self) {
        self.owned.store(false, Ordering::Release);
        if let Err(error) = self.lease.finish()
            && let Ok(mut state) = self.cleanup.lock()
        {
            *state = Some(error);
        }
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
    forwarding_stop: crate::media_pause::CaptureStop,
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
}

impl Capture {
    /// Bind the logical forwarding session; None suppresses an internal replacement.
    pub fn set_forwarding_pause(&self, pause: Option<crate::media_pause::ForwardingPause>) {
        self.forwarding_stop.attach(pause);
    }
    /// Sticky for this process. Once HAL cleanup fails, creating another tap is
    /// forbidden even after the default output changes; restart the application
    /// after checking local audio. This query never accesses audio hardware.
    pub fn cleanup_failed() -> bool {
        // SAFETY: Reads only a native process-local atomic flag.
        unsafe { oink_audio_capture_cleanup_failed() }
    }

    /// Production data plane: encoding and gain execute in the dedicated
    /// device-workgroup member; this owner keeps health, pause and HAL lifetime.
    pub fn start_encoded(
        encoder: crate::source::Encoder,
        mute: bool,
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
    ) -> Result<Self, String> {
        if mute && recovery_dir.is_none() {
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
        let forwarding_stop = crate::media_pause::CaptureStop::default();
        let worker_forwarding_stop = forwarding_stop.clone();
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
                let lease_uid = if mute {
                    match selected_uid.as_ref().map(|uid| uid.to_string_lossy().into_owned())
                        .map(Ok).unwrap_or_else(Self::current_default_output_id) {
                        Ok(uid) => Some(uid),
                        Err(error) => { let _ = ready_tx.send(Err(error)); return; }
                    }
                } else { None };
                let mut mute_lease = match (recovery_dir.as_deref(), lease_uid.as_deref()) {
                    (Some(dir), Some(uid)) => match crate::macos_mute::MuteLease::start(dir, uid) {
                        Ok(lease) => Some(SourceMute { lease, owned: worker_muted.clone(), cleanup: worker_cleanup.clone() }),
                        Err(error) => { let _ = ready_tx.send(Err(error)); return; }
                    },
                    _ => None,
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
                let pause_before_restore = crate::media_pause::BeforeRestore(worker_forwarding_stop);
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
                    if let Some(lease) = &mut mute_lease {
                        match lease.lease.check_ownership() {
                            Ok(true) => { worker_controls.store(true, Ordering::Release); break; }
                            Err(error) => { *worker_error.lock().unwrap() = Some(error); break; }
                            Ok(false) => {}
                        }
                        if lease.lease.default_changed() {
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
                            finish_capture_health(&pause_before_restore.0, exit_health);
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
                pause_before_restore.0.finish();
                if let Err(error) = native.finish()
                    && let Ok(mut state) = worker_error.lock()
                {
                    *worker_cleanup.lock().unwrap() = Some(error.clone());
                    *state = Some(error);
                }
                worker_muted.store(false, Ordering::Release);
                if let Some(lease) = &mut mute_lease
                    && let Err(error) = lease.lease.finish()
                {
                    *worker_cleanup.lock().unwrap() = Some(error.clone());
                    *worker_error.lock().unwrap() = Some(error);
                    // A failed lease release must not announce a successful
                    // native rebuild that could lose its pending old journal.
                    *worker_restart.lock().unwrap() = None;
                }
            })
            .map_err(|error| format!("Start audio capture worker: {error}"))?;
        match ready_rx.recv() {
            Ok(Ok(info)) => Ok(Self {
                forwarding_stop,
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
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
        self.cleanup_error.lock().ok().and_then(|error| error.clone())
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
// pause/restore or HAL close, including unwind and device-change paths.
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
        let mut resampler = StereoResampler::new(rate);
        let mut output = vec![0.0; ((4097.0 * 48_000.0 / rate).ceil() as usize + 2) * 2];
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

fn finish_capture_health(stop: &crate::media_pause::CaptureStop, health: i32) {
    // Native health=2 requests an internal replacement after confirmed cleanup,
    // not the end of forwarding. Retire the physical registration using the
    // same shape as Windows; only the owner ends the logical ticket on failure
    // or a real user/network stop. The restore guard also respects this state.
    if health == 2 {
        stop.finish_rebuild();
    } else {
        stop.finish();
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
struct StereoResampler {
    step: f64,
    position: f64,
    pending: Vec<f32>,
    pending_time: Option<Instant>,
    pending_callback: Option<Instant>,
    pending_sequence: u64,
}

impl StereoResampler {
    fn new(rate: f64) -> Self {
        Self {
            step: rate / 48_000.0,
            position: 0.0,
            pending: {
                let mut pending = vec![0.0; 8194];
                std::hint::black_box(&mut pending);
                pending.clear();
                pending
            },
            pending_time: None,
            pending_callback: None,
            pending_sequence: 0,
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.position = 0.0;
        self.pending_time = None;
        self.pending_callback = None;
        self.pending_sequence = 0;
    }
    fn process_timed_into(
        &mut self,
        input: &[f32],
        first_sample_at: Option<Instant>,
        callback_at: Option<Instant>,
        sequence: u64,
        output: &mut [f32],
    ) -> (usize, crate::source::Stamp) {
        let previous_frames = self.pending.len() / 2;
        if previous_frames == 0 {
            self.pending_time = first_sample_at;
            self.pending_callback = callback_at;
            self.pending_sequence = sequence;
        }
        let rate = self.step * 48_000.0;
        let stamp = crate::source::Stamp {
            first_sample_at: self
                .pending_time
                .and_then(|at| at.checked_add(Duration::from_secs_f64(self.position / rate))),
            callback_at: self.pending_callback,
            source_frame_position: Some(self.pending_sequence + self.position as u64),
            delivered_at: Instant::now(),
            discontinuity: false,
        };
        let mut count = 0;
        if self.step == 1.0 {
            count = input.len();
            output[..count].copy_from_slice(input);
        } else {
            self.pending.extend_from_slice(input);
            let frames = self.pending.len() / 2;
            while self.position + 1.0 < frames as f64 && count + 2 <= output.len() {
                let frame = self.position as usize;
                let fraction = (self.position - frame as f64) as f32;
                for channel in 0..2 {
                    let a = self.pending[frame * 2 + channel];
                    let b = self.pending[(frame + 1) * 2 + channel];
                    output[count + channel] = a + (b - a) * fraction;
                }
                count += 2;
                self.position += self.step;
            }
            let consumed = (self.position as usize).min(frames);
            self.pending.drain(..consumed * 2);
            self.position -= consumed as f64;
        }
        let consumed = previous_frames + input.len() / 2 - self.pending.len() / 2;
        if consumed >= previous_frames {
            self.pending_time = first_sample_at
                .and_then(|at| at.checked_add(Duration::from_secs_f64((consumed - previous_frames) as f64 / rate)));
            self.pending_callback = callback_at;
            self.pending_sequence = sequence + (consumed - previous_frames) as u64;
        } else {
            self.pending_time = self
                .pending_time
                .and_then(|at| at.checked_add(Duration::from_secs_f64(consumed as f64 / rate)));
            self.pending_sequence += consumed as u64;
        }
        (count, stamp)
    }

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
        let previous_frames = self.pending.len() / 2;
        if previous_frames == 0 {
            self.pending_time = first_sample_at;
            self.pending_callback = callback_at;
            self.pending_sequence = sequence;
        }
        let rate = self.step * 48_000.0;
        let output_time = self
            .pending_time
            .and_then(|time| time.checked_add(Duration::from_secs_f64(self.position / rate)));
        let output_callback = self.pending_callback;
        let output_sequence = self.pending_sequence + self.position as u64;
        let samples = self.resample(input);
        // Update the timestamp of the retained input frame. Refer to the new
        // callback only once the retained frame belongs to its input block.
        let consumed = previous_frames + input.len() / 2 - self.pending.len() / 2;
        if consumed >= previous_frames {
            self.pending_time = first_sample_at
                .and_then(|time| time.checked_add(Duration::from_secs_f64((consumed - previous_frames) as f64 / rate)));
            self.pending_callback = callback_at;
            self.pending_sequence = sequence + (consumed - previous_frames) as u64;
        } else {
            self.pending_time = self
                .pending_time
                .and_then(|time| time.checked_add(Duration::from_secs_f64(consumed as f64 / rate)));
            self.pending_sequence += consumed as u64;
        }
        CaptureBlock {
            samples,
            first_sample_at: output_time,
            callback_at: output_callback,
            source_frame_position: Some(output_sequence),
        }
    }

    #[cfg(test)]
    fn resample(&mut self, input: &[f32]) -> Vec<f32> {
        if self.step == 1.0 {
            return input.to_vec();
        }
        self.pending.extend_from_slice(input);
        let frames = self.pending.len() / 2;
        let mut output = Vec::with_capacity((frames as f64 / self.step).ceil() as usize * 2);
        while self.position + 1.0 < frames as f64 {
            let frame = self.position as usize;
            let fraction = (self.position - frame as f64) as f32;
            for channel in 0..2 {
                let a = self.pending[frame * 2 + channel];
                let b = self.pending[(frame + 1) * 2 + channel];
                output.push(a + (b - a) * fraction);
            }
            self.position += self.step;
        }
        let consumed = (self.position as usize).min(frames);
        self.pending.drain(..consumed * 2);
        self.position -= consumed as f64;
        output
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
            forwarding_stop: Default::default(),
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
    fn default_and_format_rebuild_keep_forwarding_media_submissions_after_resume() {
        use crate::{
            dsp,
            media_pause::{CaptureStop, ForwardingPause},
            source::{self, Stamp},
        };
        use std::{
            sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            },
            time::{Duration, Instant},
        };
        for reason in ["default output changed", "output stream format changed"] {
            let pauses = Arc::new(AtomicUsize::new(0));
            let paused = pauses.clone();
            let logical = ForwardingPause::with_backend(move || {
                paused.fetch_add(1, Ordering::Relaxed);
            });
            let sent = Arc::new(AtomicUsize::new(0));
            let feed = |physical: &CaptureStop, at: Instant| {
                let (mut encoder, output) = source::channel(dsp::Config::low_latency_pcm(), 100, false).unwrap();
                let ticket = logical.clone();
                let packets = sent.clone();
                output.set_packet_writer(Arc::new(move |_| {
                    ticket
                        .submit(|| {
                            packets.fetch_add(1, Ordering::Relaxed);
                            Ok::<_, ()>(())
                        })
                        .is_some_and(|result| result.is_ok())
                }));
                physical.attach(Some(logical.clone()));
                let stamp = |at| Stamp {
                    first_sample_at: None,
                    callback_at: Some(at),
                    delivered_at: at,
                    source_frame_position: None,
                    discontinuity: false,
                };
                encoder.supply(&[0.2; 480], stamp(at));
                // A silent pause followed by resume must still submit encoded
                // media through the same logical ticket after a physical rebuild.
                encoder.tick(at + Duration::from_secs(6));
                encoder.supply(&[0.2; 480], stamp(at + Duration::from_secs(7)));
            };
            let old = CaptureStop::default();
            let at = Instant::now();
            feed(&old, at);
            let before = sent.load(Ordering::Relaxed);
            assert!(before > 0);
            // Same health-stop call used by the real Mac guardian. Cleanup is
            // injected as successful; no tap or system audio endpoint is opened.
            super::finish_capture_health(&old, 2);
            assert!(capture_exit(2, reason.into(), Ok(())).0.is_some());
            let rebuilt = CaptureStop::default();
            feed(&rebuilt, at + Duration::from_secs(8));
            assert!(
                sent.load(Ordering::Relaxed) > before,
                "{reason}: rebuilt/resumed encoder must really submit media"
            );
            assert_eq!(
                pauses.load(Ordering::Relaxed),
                0,
                "internal replacement must not pause players"
            );
            logical.stop();
            assert_eq!(pauses.load(Ordering::Relaxed), 1, "real stop still pauses once");
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
            for _ in 0..100 {
                output.extend(converter.process(&[0.25, -0.5, 0.25, -0.5]));
            }
            assert!(!output.is_empty());
            assert!(output.as_chunks::<2>().0.iter().all(|frame| *frame == [0.25, -0.5]));
        }
    }
}
