//! Native HAL output with a conditional device-buffer lease. Device IO buffer
//! changes affect other applications; graceful teardown restores the old value
//! only while the current value still equals ours. Crashes cannot restore it,
//! and another client's same-value write cannot be identified by CoreAudio.
use serde::Serialize;
use std::{
    cell::UnsafeCell,
    ffi::{CStr, CString, c_char, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub struct NativeOutputTiming {
    pub period_frames: u32,
    pub native_gap_frames: Option<u64>,
    pub callback_at: Instant,
    /// HAL outputDataTime: first sample passed to hardware, not acoustic arrival.
    pub playback_at: Option<Instant>,
    pub sample_rate: u32,
    pub channels: u16,
    /// No additional application queue in this backend.
    pub padding_frames: u32,
    pub buffer_frames: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct NativeOutputInfo {
    pub backend: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub requested_buffer_frames: Option<u32>,
    pub original_device_buffer_frames: u32,
    pub actual_buffer_frames: u32,
    pub device_output_latency_frames: Option<u32>,
    pub device_output_safety_offset_frames: Option<u32>,
    pub stream_output_latency_frames: Option<u32>,
    pub device_buffer_lease_changed: bool,
    pub timestamp_source: String,
    pub buffer_lease_limitations: String,
    pub fallback_reason: Option<String>,
}

#[repr(C)]
#[derive(Default)]
struct RawInfo {
    sample_rate: f64,
    channels: u32,
    original_buffer_frames: u32,
    buffer_frames: u32,
    latency_frames: u32,
    safety_frames: u32,
    stream_latency_frames: u32,
    buffer_request_status: i32,
    lease_changed: u32,
}
type RenderCallback = Box<dyn crate::playback::NativeRender>;
type RenderBridge = unsafe extern "C" fn(*mut c_void, *mut f32, usize, u32, u32, u32, u64, u64, f64);
unsafe extern "C" {
    fn oink_audio_output_start(
        uid: *const c_char,
        requested: u32,
        render: RenderBridge,
        complete: unsafe extern "C" fn(*mut c_void, u64),
        context: *mut c_void,
        info: *mut RawInfo,
        retained: *mut bool,
        error: *mut c_char,
        error_size: usize,
    ) -> *mut c_void;
    fn oink_audio_output_stop(output: *mut c_void, released: *mut bool) -> i32;
    fn oink_audio_output_check(output: *mut c_void, error: *mut c_char, error_size: usize) -> i32;
    fn oink_audio_host_time_ns() -> u64;
}

#[derive(Debug)]
pub struct NativePlayback {
    quiescent: Arc<AtomicBool>,
    info: NativeOutputInfo,
    invalidated: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl NativePlayback {
    /// None leaves the original buffer unchanged, Some(0) requests minimum,
    /// Some(n) requests n frames clipped to the device's supported range.
    pub fn start(
        endpoint_raw_uid: &str,
        requested: Option<u32>,
        callback: RenderCallback,
        mut on_error: Box<dyn FnMut(String) + Send>,
    ) -> Result<Self, String> {
        let uid = CString::new(endpoint_raw_uid).map_err(|_| "Output UID contains NUL")?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let quiescent = Arc::new(AtomicBool::new(false));
        let worker_quiescent = quiescent.clone();
        let invalidated = Arc::new(AtomicBool::new(false));
        let worker_invalidated = invalidated.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("oink-hal-output".into())
            .spawn(move || {
                let mut state = Box::new(CallbackState {
                    callback: UnsafeCell::new(callback),
                    clock: HostClock::new(),
                    panicked: AtomicBool::new(false),
                    last_callback_ns: AtomicU64::new(0),
                    expected_frame: UnsafeCell::new(None),
                });
                let mut retained = false;
                let mut raw_info = RawInfo::default();
                let mut error = [0 as c_char; 512];
                // SAFETY: Every pointer is valid, UID is NUL terminated. State stays
                // boxed until NativeOutput has stopped and destroyed its IOProc.
                let raw = unsafe {
                    oink_audio_output_start(
                        uid.as_ptr(),
                        requested.unwrap_or(u32::MAX),
                        render_bridge,
                        complete_bridge,
                        (&mut *state as *mut CallbackState).cast(),
                        &mut raw_info,
                        &mut retained,
                        error.as_mut_ptr(),
                        error.len(),
                    )
                };
                if raw.is_null() {
                    if retained {
                        let _ = Box::leak(state);
                    }
                    let _ = ready_tx.send(Err(native_error(&error)));
                    return;
                }
                let mut output = NativeOutput {
                    raw,
                    state: Some(state),
                };
                if ready_tx.send(Ok(raw_info.into_info(requested))).is_err() {
                    return;
                }
                let started = Instant::now();
                while !stopping.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(20));
                    let state = output.state.as_ref().unwrap();
                    if state.panicked.load(Ordering::Acquire) {
                        on_error("HAL output callback panicked".into());
                        break;
                    }
                    // SAFETY: The native object is live and error buffer is writable.
                    let health = unsafe { oink_audio_output_check(output.raw, error.as_mut_ptr(), error.len()) };
                    if health != 0 {
                        worker_invalidated.store(health == 2, Ordering::Release);
                        on_error(native_error(&error));
                        break;
                    }
                    let last = state.last_callback_ns.load(Ordering::Relaxed);
                    // SAFETY: Pure monotonic clock read.
                    let now_ns = unsafe { oink_audio_host_time_ns() };
                    if started.elapsed() > Duration::from_millis(500)
                        && (last == 0 || now_ns.saturating_sub(last) > 500_000_000)
                    {
                        on_error("HAL output callback stalled".into());
                        break;
                    }
                }
                // Error publication asks the ordinary owner to retire the sink.
                // Keep callback ownership pinned until the ordinary owner
                // requests shutdown, then report native quiescence.
                while !stopping.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                let status = output.close();
                worker_quiescent.store(output.state.is_some(), Ordering::Release);
                if status != 0 {
                    on_error(format!("Restore HAL output buffer failed: Core Audio status {status}"));
                }
            })
            .map_err(|err| format!("Start HAL output worker: {err}"))?;
        match ready_rx.recv() {
            Ok(Ok(info)) => Ok(Self {
                info,
                quiescent,
                invalidated,
                stop,
                worker: Some(worker),
            }),
            result => {
                let _ = worker.join();
                Err(match result {
                    Ok(Err(error)) => error,
                    _ => "HAL output worker stopped during initialization".into(),
                })
            }
        }
    }
    pub fn shutdown(&mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.quiescent.load(Ordering::Acquire)
    }
    pub fn device_invalidated(&self) -> bool {
        self.invalidated.load(Ordering::Acquire)
    }
    pub fn info(&self) -> &NativeOutputInfo {
        &self.info
    }
}
impl Drop for NativePlayback {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct NativeOutput {
    raw: *mut c_void,
    state: Option<Box<CallbackState>>,
}
impl NativeOutput {
    fn close(&mut self) -> i32 {
        let ptr = std::mem::replace(&mut self.raw, std::ptr::null_mut());
        let mut released = true;
        // SAFETY: Unique native owner. Native reports whether callbacks quiesced.
        let status = unsafe { oink_audio_output_stop(ptr, &mut released) };
        if !released && let Some(state) = self.state.take() {
            let _ = Box::leak(state);
        }
        status
    }
}
impl Drop for NativeOutput {
    fn drop(&mut self) {
        let status = self.close();
        if status != 0 {
            tracing::warn!(status, "Restore HAL output buffer failed");
        }
    }
}
fn native_error(error: &[c_char]) -> String {
    // SAFETY: All bridge failures write a bounded NUL-terminated message.
    unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned()
}
impl RawInfo {
    fn into_info(self, requested: Option<u32>) -> NativeOutputInfo {
        let known = |value| (value != u32::MAX).then_some(value);
        let mut reasons = Vec::new();
        if self.buffer_request_status != 0 {
            reasons.push(format!(
                "Device buffer request failed: Core Audio status {}",
                self.buffer_request_status
            ));
        }
        if requested.is_some_and(|frames| frames > 0 && frames != self.buffer_frames) {
            reasons.push(format!("HAL uses {} frames after buffer request", self.buffer_frames));
        }
        NativeOutputInfo {
            backend: "core_audio_device_ioproc".into(),
            sample_rate: self.sample_rate as u32,
            channels: self.channels as u16,
            requested_buffer_frames: requested,
            original_device_buffer_frames: self.original_buffer_frames,
            actual_buffer_frames: self.buffer_frames,
            device_output_latency_frames: known(self.latency_frames),
            device_output_safety_offset_frames: known(self.safety_frames),
            stream_output_latency_frames: known(self.stream_latency_frames),
            device_buffer_lease_changed: self.lease_changed != 0,
            timestamp_source: "IOProc outputDataTime.mHostTime; callback entry mach_absolute_time".into(),
            buffer_lease_limitations: "Device-global buffer; restores on graceful stop only if current equals leased value; crash recovery unavailable; same-value external writes indistinguishable".into(),
            fallback_reason: (!reasons.is_empty()).then(|| reasons.join("; ")),
        }
    }
}

struct CallbackState {
    callback: UnsafeCell<RenderCallback>,
    clock: HostClock,
    panicked: AtomicBool,
    last_callback_ns: AtomicU64,
    expected_frame: UnsafeCell<Option<f64>>,
}
unsafe extern "C" fn render_bridge(
    context: *mut c_void,
    samples: *mut f32,
    len: usize,
    rate: u32,
    channels: u32,
    frames: u32,
    callback_ns: u64,
    playback_ns: u64,
    sample_frame: f64,
) {
    // SAFETY: Native HAL serializes callbacks. State and scratch stay live until
    // AudioDeviceStop/DestroyIOProc return; len is the initialized float count.
    let state = unsafe { &*context.cast::<CallbackState>() };
    state.last_callback_ns.store(callback_ns, Ordering::Relaxed);
    // SAFETY: The bridge guarantees a non-null scratch allocation of len floats.
    let samples = unsafe { std::slice::from_raw_parts_mut(samples, len) };
    if state.panicked.load(Ordering::Relaxed) {
        return;
    }
    let Some(callback_at) = state.clock.instant(callback_ns) else {
        return;
    };
    // SAFETY: HAL serializes both render and completion on this IOProc.
    let expected = unsafe { &mut *state.expected_frame.get() };
    let native_gap_frames = if sample_frame.is_finite() {
        let gap = expected
            .filter(|previous| sample_frame >= *previous - 0.5)
            .map(|previous| (sample_frame - previous).max(0.0).round() as u64);
        *expected = Some(sample_frame + frames as f64);
        gap
    } else {
        *expected = None;
        None
    };
    let timing = NativeOutputTiming {
        period_frames: frames,
        native_gap_frames,
        callback_at,
        playback_at: state.clock.instant(playback_ns),
        sample_rate: rate,
        channels: channels as u16,
        padding_frames: 0,
        buffer_frames: frames,
    };
    // SAFETY: Only this serialized HAL callback accesses the closure. The
    // worker accesses separate atomic fields through a shared reference.
    let callback = unsafe { &mut *state.callback.get() };
    if catch_unwind(AssertUnwindSafe(|| callback.fill(samples, timing))).is_err() {
        samples.fill(0.0);
        state.panicked.store(true, Ordering::Release);
    }
}
unsafe extern "C" fn complete_bridge(context: *mut c_void, elapsed_ns: u64) {
    // SAFETY: native completion is serialized immediately after buffer copy.
    let state = unsafe { &*context.cast::<CallbackState>() };
    if state.panicked.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: same sole IOProc owner as render_bridge.
    let callback = unsafe { &mut *state.callback.get() };
    if catch_unwind(AssertUnwindSafe(|| callback.complete(Duration::from_nanos(elapsed_ns)))).is_err() {
        state.panicked.store(true, Ordering::Release);
    }
}
struct HostClock {
    host_ns: u64,
    instant: Instant,
}
impl HostClock {
    fn new() -> Self {
        let before = Instant::now();
        // SAFETY: Pure mach monotonic clock read, no pointers.
        let host_ns = unsafe { oink_audio_host_time_ns() };
        let after = Instant::now();
        Self {
            host_ns,
            instant: before + after.duration_since(before) / 2,
        }
    }
    fn instant(&self, ns: u64) -> Option<Instant> {
        if ns == 0 {
            None
        } else if ns >= self.host_ns {
            self.instant.checked_add(Duration::from_nanos(ns - self.host_ns))
        } else {
            self.instant.checked_sub(Duration::from_nanos(self.host_ns - ns))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hardware_timestamp_is_preserved_without_an_extra_buffer() {
        let epoch = Instant::now();
        let observed = Arc::new(std::sync::Mutex::new(None));
        struct Probe(Arc<std::sync::Mutex<Option<NativeOutputTiming>>>);
        impl crate::playback::NativeRender for Probe {
            fn fill(&mut self, samples: &mut [f32], timing: NativeOutputTiming) {
                samples.fill(0.25);
                *self.0.lock().unwrap() = Some(timing);
            }
            fn complete(&mut self, _: Duration) {}
        }
        let mut state = CallbackState {
            callback: UnsafeCell::new(Box::new(Probe(observed.clone()))),
            expected_frame: UnsafeCell::new(None),
            clock: HostClock {
                host_ns: 1_000_000_000,
                instant: epoch,
            },
            panicked: AtomicBool::new(false),
            last_callback_ns: AtomicU64::new(0),
        };
        let mut samples = [0.0; 256];
        // SAFETY: Live state, valid 128-frame stereo buffer; no concurrent call.
        unsafe {
            render_bridge(
                (&mut state as *mut CallbackState).cast(),
                samples.as_mut_ptr(),
                samples.len(),
                48_000,
                2,
                128,
                1_001_000_000,
                1_005_000_000,
                0.0,
            )
        };
        let timing = observed.lock().unwrap().unwrap();
        assert_eq!(timing.callback_at, epoch + Duration::from_millis(1));
        assert_eq!(timing.playback_at, Some(epoch + Duration::from_millis(5)));
        assert_eq!(timing.padding_frames, 0);
        assert_eq!(samples, [0.25; 256]);
    }
}
