//! WASAPI stream negotiation and QPC timestamp mapping. No endpoint settings or
//! sidecar render streams are used to force a global engine period.

use crate::CaptureInfo;
use std::time::{Duration, Instant};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
        IAudioClient3, IMMDevice, WAVEFORMATEX,
    },
    System::{
        Com::{CLSCTX_ALL, CoTaskMemFree},
        Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
        Threading::CreateEventW,
    },
};
use windows::core::Interface;

const RATE: u32 = 48_000;
const HNS_PER_SECOND: u64 = 10_000_000;
const BASELINE_BUFFER_HNS: i64 = 200_000;
const LEGACY_FLAGS: u32 =
    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

#[derive(Clone, Copy, Debug)]
struct Periods {
    default: u32,
    fundamental: u32,
    minimum: u32,
    maximum: u32,
}
impl Periods {
    fn choose(self, requested: u32) -> Result<u32, String> {
        if self.fundamental == 0 || self.minimum == 0 || self.minimum > self.maximum {
            return Err("driver returned invalid shared-engine period bounds".into());
        }
        let wanted = if requested == 0 {
            self.minimum
        } else {
            requested.max(self.minimum)
        };
        let rounded = u64::from(wanted).div_ceil(u64::from(self.fundamental)) * u64::from(self.fundamental);
        let largest = self.maximum / self.fundamental * self.fundamental;
        if largest < self.minimum {
            return Err("driver has no valid shared-engine period quantum".into());
        }
        Ok(rounded.min(u64::from(largest)) as u32)
    }
}

struct Event(HANDLE);
impl Event {
    fn new() -> Result<Self, String> {
        // SAFETY: no caller pointers, an unnamed auto-reset event owned here.
        unsafe { CreateEventW(None, false, false, None) }
            .map(Self)
            .map_err(|e| format!("create WASAPI capture event: {e}"))
    }
}
impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: unique live handle, closed after all audio interfaces.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub(super) struct OpenedCapture {
    // Drop COM references before releasing the event registered with the client.
    pub client: IAudioClient,
    pub capture: IAudioCaptureClient,
    event: Option<Event>,
    pub max_frames: u32,
    pub info: CaptureInfo,
}
impl OpenedCapture {
    /// None selects the driver-default shared-engine period.
    /// Some(0) requests the driver minimum; Some(n) rounds to a valid quantum.
    pub fn open(device: &IMMDevice, requested: Option<u32>) -> Result<Self, String> {
        let format = WAVEFORMATEX {
            wFormatTag: 3, // IEEE float
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let mut notes = Vec::new();
        let probe = activate(device)?;
        let client3 = probe.cast::<IAudioClient3>().ok();
        let periods = match client3.as_ref().map(|client| shared_periods(client, &format)) {
            Some(Ok(periods)) => Some(periods),
            Some(Err(error)) => {
                notes.push(error);
                None
            }
            None => {
                notes.push("IAudioClient3 unavailable".into());
                None
            }
        };
        let legacy_default = device_default_period(&probe);
        let mut selected_period = None;
        let mut initialized = None;
        if let Some(requested) = requested
            && let (Some(client3), Some(periods)) = (client3.as_ref(), periods)
        {
            match periods.choose(requested) {
                Ok(period) => {
                    selected_period = Some(period);
                    let event = Event::new()?;
                    // SAFETY: this is a render endpoint and a valid 48kHz
                    // float format. Always retain LOOPBACK: omitting it
                    // would silently open a render stream instead. Some
                    // drivers reject LOOPBACK with the Client3 initializer;
                    // report the HRESULT and reopen a fresh legacy client.
                    let result = unsafe {
                        client3
                            .InitializeSharedAudioStream(
                                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                                period,
                                &format,
                                None,
                            )
                            .and_then(|()| client3.SetEventHandle(event.0))
                            // A driver accepting the flags still must expose a
                            // capture service; never mistake a render-only
                            // initialization for usable loopback.
                            .and_then(|()| client3.GetService::<IAudioCaptureClient>().map(|_| ()))
                    };
                    match result {
                        Ok(()) => initialized = Some((probe.clone(), Some(event), "wasapi_client3_loopback")),
                        Err(error) => {
                            notes.push(format!("IAudioClient3 loopback at {period} frames rejected: {error}"))
                        }
                    }
                }
                Err(error) => notes.push(error),
            }
        }
        // A failed Initialize is not reusable. Release both references before
        // activating the conservative shared-mode loopback fallback.
        drop(client3);
        drop(probe);
        let (client, event, backend) = match initialized {
            Some(opened) => opened,
            None if requested.is_none() => (
                legacy(device, &format, BASELINE_BUFFER_HNS, None)?,
                None,
                "wasapi_loopback_baseline_20ms_poll",
            ),
            None => {
                // hnsBufferDuration=0 asks WASAPI for its minimum legal shared
                // buffer. It does not change the endpoint's global settings.
                let event = Event::new()?;
                match legacy(device, &format, 0, Some(&event)) {
                    Ok(client) => (client, Some(event), "wasapi_loopback_shared_event_fallback"),
                    Err(error) => {
                        notes.push(format!("event-driven loopback fallback rejected: {error}"));
                        (
                            legacy(device, &format, 0, None)?,
                            None,
                            "wasapi_loopback_shared_poll_fallback",
                        )
                    }
                }
            }
        };
        // SAFETY: Initialize succeeded; all calls stay on the owning COM worker.
        let (max_frames, capture, latency) = unsafe {
            (
                client
                    .GetBufferSize()
                    .map_err(|e| format!("get WASAPI buffer size: {e}"))?,
                client
                    .GetService::<IAudioCaptureClient>()
                    .map_err(|e| format!("get WASAPI capture service: {e}"))?,
                client.GetStreamLatency().ok().and_then(hns_to_frames),
            )
        };
        let info = CaptureInfo {
            backend: backend.into(),
            // This is the actual initialized WASAPI client format. Engine
            // periods obtained in another native mix rate are rescaled to this
            // unit; raw engine rate/frames are retained in the diagnostic log.
            sample_rate: RATE,
            requested_buffer_frames: if requested.is_none() {
                hns_to_frames(BASELINE_BUFFER_HNS)
            } else {
                selected_period.or(requested)
            },
            actual_buffer_frames: max_frames,
            engine_period_frames: None, // query after Start, never infer from request
            engine_default_period_frames: periods.map(|p| p.default).or(legacy_default),
            engine_min_period_frames: periods.map(|p| p.minimum),
            engine_max_period_frames: periods.map(|p| p.maximum),
            engine_fundamental_period_frames: periods.map(|p| p.fundamental),
            reported_latency_frames: latency,
            timestamp_source: "WASAPI loopback pre-output render-clock first-frame QPC100ns; timestamp-error packets have no hardware time".into(),
            fallback_reason: (!notes.is_empty()).then(|| notes.join("; ")),
            ..CaptureInfo::default()
        };
        Ok(Self {
            client,
            capture,
            event,
            max_frames,
            info,
        })
    }

    /// Observe the engine after Start. An unavailable actual period is None,
    /// not the period we requested or the advertised default.
    pub fn report_started(&mut self) {
        match current_period(&self.client) {
            Ok((frames, rate)) => {
                self.info.engine_period_frames = rescale_frames(frames, rate, RATE);
                if self.info.backend == "wasapi_client3_loopback"
                    && let (Some(requested), Some(actual)) =
                        (self.info.requested_buffer_frames, self.info.engine_period_frames)
                    && requested != actual
                {
                    let detail =
                        format!("requested Client3 period {requested} frames, observed actual {actual} frames");
                    if let Some(reason) = &mut self.info.fallback_reason {
                        reason.push_str("; ");
                        reason.push_str(&detail);
                    } else {
                        self.info.fallback_reason = Some(detail);
                    }
                }
                tracing::info!(engine_frames = frames, engine_sample_rate = rate, client_sample_rate = RATE,
                    ?self.info, "WASAPI capture parameters (observed after Start)");
            }
            Err(error) => {
                let detail = format!("actual engine period unavailable: {error}");
                if let Some(reason) = &mut self.info.fallback_reason {
                    reason.push_str("; ");
                    reason.push_str(&detail);
                } else {
                    self.info.fallback_reason = Some(detail);
                }
                tracing::info!(?self.info, "WASAPI capture parameters (actual engine period unreported)");
            }
        }
    }

    pub fn realtime_event(&self) -> Option<usize> {
        self.event.as_ref().map(|event| event.0.0 as usize)
    }
}

fn activate(device: &IMMDevice) -> Result<IAudioClient, String> {
    // SAFETY: caller holds a live endpoint on its initialized COM thread.
    unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| format!("activate WASAPI client: {e}"))
}

fn legacy(
    device: &IMMDevice,
    format: &WAVEFORMATEX,
    buffer_hns: i64,
    event: Option<&Event>,
) -> Result<IAudioClient, String> {
    let client = activate(device)?;
    // SAFETY: a fresh client, initialized in shared loopback mode with a valid
    // float format. The auto-reset event outlives the initialized client.
    unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                LEGACY_FLAGS
                    | if event.is_some() {
                        AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    } else {
                        0
                    },
                buffer_hns,
                0,
                format,
                None,
            )
            .map_err(|e| format!("initialize shared WASAPI loopback: {e}"))?;
        if let Some(event) = event {
            client
                .SetEventHandle(event.0)
                .map_err(|e| format!("register WASAPI capture event: {e}"))?;
        }
    }
    Ok(client)
}

fn shared_periods(client: &IAudioClient3, format: &WAVEFORMATEX) -> Result<Periods, String> {
    let mut periods = Periods {
        default: 0,
        fundamental: 0,
        minimum: 0,
        maximum: 0,
    };
    // SAFETY: all out parameters and the format remain live through the call.
    unsafe {
        client.GetSharedModeEnginePeriod(
            format,
            &mut periods.default,
            &mut periods.fundamental,
            &mut periods.minimum,
            &mut periods.maximum,
        )
    }
    .map_err(|e| format!("query shared-engine periods: {e}"))?;
    Ok(periods)
}

fn device_default_period(client: &IAudioClient) -> Option<u32> {
    let mut default_hns = 0;
    // SAFETY: valid out pointer. Do not use the legacy minimum as a shared
    // engine minimum: GetDevicePeriod's minimum is for exclusive mode.
    unsafe { client.GetDevicePeriod(Some(&mut default_hns), None) }.ok()?;
    hns_to_frames(default_hns)
}

fn current_period(client: &IAudioClient) -> Result<(u32, u32), String> {
    let client = client.cast::<IAudioClient3>().map_err(|e| e.to_string())?;
    let mut format = std::ptr::null_mut();
    let mut frames = 0;
    // SAFETY: valid out pointers. GetCurrentSharedModeEnginePeriod allocates
    // format with CoTaskMemAlloc; free it after copying the sample rate.
    unsafe {
        client
            .GetCurrentSharedModeEnginePeriod(&mut format, &mut frames)
            .map_err(|e| e.to_string())?;
        if format.is_null() {
            return Err("WASAPI returned a null engine format".into());
        }
        let rate = (*format).nSamplesPerSec;
        CoTaskMemFree(Some(format.cast()));
        if rate == 0 || frames == 0 {
            return Err("WASAPI returned an invalid engine period".into());
        }
        Ok((frames, rate))
    }
}

fn hns_to_frames(hns: i64) -> Option<u32> {
    let hns = u64::try_from(hns).ok()?;
    u32::try_from(
        u128::from(hns)
            .checked_mul(u128::from(RATE))?
            .div_ceil(u128::from(HNS_PER_SECOND)),
    )
    .ok()
}
fn rescale_frames(frames: u32, from: u32, to: u32) -> Option<u32> {
    if from == 0 {
        return None;
    }
    u32::try_from((u64::from(frames) * u64::from(to)).div_ceil(u64::from(from))).ok()
}

/// GetBuffer returns QPC already converted to 100ns units. This calibration
/// brackets QueryPerformanceCounter with Instant reads and uses the narrowest
/// bracket, limiting scheduler interference instead of assuming dequeue time.
#[derive(Clone, Copy)]
pub(super) struct QpcClock {
    anchor: Instant,
    qpc_hns: u64,
    pub uncertainty_ns: u64,
}
impl QpcClock {
    pub fn calibrate() -> Result<Self, String> {
        let mut frequency = 0;
        // SAFETY: writable frequency pointer, no persistent OS state is changed.
        unsafe { QueryPerformanceFrequency(&mut frequency) }.map_err(|e| format!("read QPC frequency: {e}"))?;
        let frequency = u64::try_from(frequency)
            .ok()
            .filter(|&f| f != 0)
            .ok_or("invalid QPC frequency")?;
        let mut best: Option<(Duration, Instant, u64)> = None;
        for _ in 0..8 {
            let before = Instant::now();
            let mut counter = 0;
            // SAFETY: writable counter pointer, called in a live process.
            unsafe { QueryPerformanceCounter(&mut counter) }.map_err(|e| format!("read QPC: {e}"))?;
            let after = Instant::now();
            let elapsed = after.duration_since(before);
            let counter = u64::try_from(counter).map_err(|_| "invalid negative QPC counter")?;
            let hns = qpc_to_hns(counter, frequency).ok_or("QPC conversion overflow")?;
            if best.as_ref().is_none_or(|(old, _, _)| elapsed < *old) {
                best = Some((elapsed, before + elapsed / 2, hns));
            }
        }
        let (bracket, anchor, qpc_hns) = best.ok_or("no QPC calibration samples")?;
        Ok(Self {
            anchor,
            qpc_hns,
            uncertainty_ns: (bracket.as_nanos() / 2 + 100).min(u128::from(u64::MAX)) as u64,
        })
    }

    pub fn map(&self, qpc_hns: u64, observed_at: Instant, actual_buffer_duration: Duration) -> Option<Instant> {
        if qpc_hns == 0 {
            return None;
        }
        let timestamp = instant_from_hns(self.anchor, self.qpc_hns, qpc_hns)?;
        // Loopback exposes the pre-output render timeline. Its samples can
        // already be readable while their first-frame render-clock timestamp
        // is still ahead of dequeue (a Windows endpoint measured about 10ms with a 22ms
        // actual client buffer). Preserve that supplied time without clamping,
        // subtracting a block duration, or relabeling it as capture-ready time.
        // The physical client buffer, not an arbitrary tolerance, bounds the
        // legitimate look-ahead; further-future/overflow values remain unknown.
        let future_bound = actual_buffer_duration.checked_add(Duration::from_nanos(self.uncertainty_ns))?;
        if timestamp > observed_at.checked_add(future_bound)? {
            return None;
        }
        Some(timestamp)
    }
}
fn qpc_to_hns(counter: u64, frequency: u64) -> Option<u64> {
    if frequency == 0 {
        return None;
    }
    u64::try_from(u128::from(counter) * u128::from(HNS_PER_SECOND) / u128::from(frequency)).ok()
}
fn instant_from_hns(anchor: Instant, anchor_hns: u64, hns: u64) -> Option<Instant> {
    let ns = hns.abs_diff(anchor_hns).checked_mul(100)?;
    if hns >= anchor_hns {
        anchor.checked_add(Duration::from_nanos(ns))
    } else {
        anchor.checked_sub(Duration::from_nanos(ns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_request_obeys_driver_minimum_quantum_and_maximum() {
        let p = Periods {
            default: 480,
            fundamental: 16,
            minimum: 48,
            maximum: 480,
        };
        assert_eq!(p.choose(0).unwrap(), 48);
        assert_eq!(p.choose(1).unwrap(), 48);
        assert_eq!(p.choose(128).unwrap(), 128);
        assert_eq!(p.choose(129).unwrap(), 144);
        assert_eq!(p.choose(u32::MAX).unwrap(), 480);
        assert!(Periods { fundamental: 0, ..p }.choose(128).is_err());
        assert!(Periods { minimum: 481, ..p }.choose(128).is_err());
    }

    #[test]
    fn engine_and_latency_units_use_the_reported_sample_rate() {
        assert_eq!(hns_to_frames(200_000), Some(960));
        assert_eq!(hns_to_frames(100_000), Some(480));
        assert_eq!(hns_to_frames(-1), None);
        assert_eq!(rescale_frames(441, 44_100, 48_000), Some(480));
        assert_eq!(rescale_frames(448, 44_100, 48_000), Some(488));
        assert_eq!(rescale_frames(480, 0, 48_000), None);
    }

    #[test]
    fn pre_output_qpc_lookahead_is_bounded_by_actual_buffer_and_calibration() {
        let anchor = Instant::now();
        let clock = QpcClock {
            anchor,
            qpc_hns: 1_000_000,
            uncertainty_ns: 100,
        };
        // Observed Windows buffer: 1056 frames / 48000 Hz = 22ms. The driver
        // reported about 10ms ahead, which is a legitimate scheduled timeline.
        let actual_buffer = Duration::from_millis(22);
        assert_eq!(
            clock.map(1_100_000, anchor, actual_buffer),
            Some(anchor + Duration::from_millis(10))
        );
        assert_eq!(
            clock.map(1_220_000, anchor, actual_buffer),
            Some(anchor + actual_buffer)
        );
        assert_eq!(
            clock.map(1_220_001, anchor, actual_buffer),
            Some(anchor + actual_buffer + Duration::from_nanos(100))
        );
        assert_eq!(clock.map(1_220_002, anchor, actual_buffer), None);
        assert_eq!(clock.map(1_230_000, anchor, actual_buffer), None);
        // A later dequeue does not shift the original sample timestamp.
        assert_eq!(
            clock.map(1_100_000, anchor + Duration::from_millis(30), actual_buffer),
            Some(anchor + Duration::from_millis(10))
        );
        assert_eq!(clock.map(0, anchor, actual_buffer), None);
    }

    #[test]
    fn qpc_hardware_timestamp_is_independent_of_dequeue_delay() {
        let anchor = Instant::now();
        let clock = QpcClock {
            anchor,
            qpc_hns: 1_000_000,
            uncertainty_ns: 100,
        };
        let expected = anchor - Duration::from_millis(10);
        assert_eq!(clock.map(900_000, anchor, Duration::ZERO), Some(expected));
        assert_eq!(
            clock.map(900_000, anchor + Duration::from_secs(1), Duration::ZERO),
            Some(expected)
        );
        assert_eq!(
            clock.map(1_100_000, anchor + Duration::from_millis(20), Duration::ZERO),
            Some(anchor + Duration::from_millis(10))
        );
        assert_eq!(clock.map(1_100_000, anchor, Duration::ZERO), None);
        assert_eq!(clock.map(0, anchor, Duration::ZERO), None);
        assert_eq!(qpc_to_hns(30_000_000, 3_000_000), Some(100_000_000));
        assert_eq!(qpc_to_hns(123, 0), None);
        assert_eq!(qpc_to_hns(u64::MAX, u64::MAX), Some(HNS_PER_SECOND));
    }
}
