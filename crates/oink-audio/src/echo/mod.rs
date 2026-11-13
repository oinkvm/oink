//! Device-local, single-microphone AEC. No audio leaves this adapter except the
//! existing shared microphone stream; reference data never enters the network.
pub(crate) mod clock;
mod control;
mod lines;
mod processor;
mod reference;
mod system;

pub(crate) use control::WorkerControl;
pub use control::{Control, SummaryPublisher};
pub use lines::{LogLines, SummaryLog, TARGET as LOG_TARGET};
pub use processor::{Processor, Summary};
pub use reference::{OutputRegistration, RenderTap};
use std::{sync::Arc, sync::OnceLock, time::Instant};
pub use system::{SystemFeed, SystemPacket, SystemReference};

pub const BLOCK: usize = 480;
pub(crate) const CHUNK: usize = 128;
/// Measured peak group delay of the pinned 48 kHz AEC3/filter-bank path. Raw
/// bypass is delayed equally while AEC is armed; Off removes this delay.
pub const ENGINE_DELAY: usize = 430;
pub const ALGORITHM: &str = "webrtc-aec3-d0569cfa-oink1";

/// A process-local monotonic epoch used only when a desktop backend exposes
/// callback instants but not its native counter in the Rust callback. Mac and
/// Windows owners use the same epoch for render and capture, so this remains
/// an estimated boundary and never claims a hardware timestamp.
pub fn estimated_host_ns(at: Instant) -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    at.saturating_duration_since(*EPOCH.get_or_init(Instant::now))
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}

/// Desktop capture-side Case A owner. The output side is the single
/// `RenderTap` held by the playback callback; this object only owns the
/// worker-side processor and shares its `Control`.
///
/// It runs in the native capture callback and never creates or frees an
/// engine: the audio owner keeps `control()` and calls `Control::update` on
/// its own maintenance passes (see that method).
#[derive(Debug)]
pub struct CaseAInput {
    control: Arc<Control>,
    processor: Processor,
    enabled: bool,
    summaries: SummaryPublisher,
}

impl CaseAInput {
    pub fn new(control: Arc<Control>, enabled: bool) -> Result<Self, String> {
        let processor = Processor::new(Some(control.clone()), Instant::now())?;
        // Per-second summaries cross to the owner as fixed values; it formats
        // and logs them (`Control::take_summary`), never the capture callback.
        let summaries = control.summary_publisher();
        Ok(Self {
            control,
            processor,
            enabled,
            summaries,
        })
    }

    /// The control the owner services with `Control::update`.
    pub fn control(&self) -> Arc<Control> {
        self.control.clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_test_work_budget(&mut self, budget: std::time::Duration) {
        self.processor.set_test_work_budget(budget);
    }

    /// Process one 10 ms 48 kHz mono block on the capture worker. The caller
    /// supplies the native capture boundary; no reference PCM crosses this
    /// API or the network.
    pub fn process_block(
        &mut self,
        mic: &[f32; BLOCK],
        host_ns: u64,
        now: Instant,
        discontinuity: bool,
    ) -> [f32; BLOCK] {
        self.processor.prepare(now);
        let time = Time {
            host_ns: (host_ns != 0).then_some(host_ns),
            device_clock: None,
            native_frame: None,
            rate: 48_000,
            quality: if host_ns == 0 {
                TimeQuality::Unknown
            } else {
                TimeQuality::Estimated
            },
            discontinuity,
            delivered_at: now,
        };
        let mut out = [0.0; BLOCK];
        if self.enabled {
            self.processor
                .process(mic, time, 1_000_000_000.0 / 48_000.0, &mut out, now);
        } else {
            self.processor.raw(mic, &mut out, now);
        }
        if let Some(summary) = self.processor.summary(now) {
            self.summaries.publish(summary);
        }
        out
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeQuality {
    HardwareBoundary,
    Estimated,
    #[default]
    Unknown,
}
impl TimeQuality {
    /// Stable log/diagnostics name (the serde name).
    pub const fn name(self) -> &'static str {
        match self {
            Self::HardwareBoundary => "hardware_boundary",
            Self::Estimated => "estimated",
            Self::Unknown => "unknown",
        }
    }
}
/// Native first-frame metadata, independent of callback arrival. Host time is
/// mach_absolute_time nanoseconds or QPC nanoseconds, never wall time. Only
/// compare endpoints on this device. The epoch is owned by each native stream.
#[derive(Clone, Copy, Debug)]
pub struct DeviceClock {
    /// Device position converted from IAudioClock units using GetFrequency.
    pub frame: f64,
    pub host_ns: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct Time {
    pub host_ns: Option<u64>,
    /// Optional independent native render-clock pair, used for rate only.
    pub device_clock: Option<DeviceClock>,
    pub native_frame: Option<f64>,
    pub rate: u32,
    pub quality: TimeQuality,
    pub discontinuity: bool,
    pub delivered_at: Instant,
}
impl Time {
    pub(crate) fn offset(self, frames: usize) -> Self {
        Self {
            host_ns: self
                .host_ns
                .and_then(|n| n.checked_add(frames as u64 * 1_000_000_000 / u64::from(self.rate))),
            native_frame: self.native_frame.map(|n| n + frames as f64),
            discontinuity: self.discontinuity && frames == 0,
            ..self
        }
    }
}
/// What the processor does with the microphone. Only `Converging` and
/// `Active` send AEC3's processed output; the two differ only in the linear
/// filter's evidence (diagnostics), never in what is sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[repr(u8)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// The local switch is off: raw microphone, no engine delay.
    Off = 0,
    /// Not Case A here (no local exit/reference yet): raw microphone.
    #[default]
    Armed = 1,
    /// Engaged: AEC3 output (linear filter + residual echo suppressor) is
    /// sent; the linear filter has not yet shown sustained ERLE.
    Converging = 2,
    /// Engaged, and the linear filter has shown >= 6 dB ERLE for 300 ms.
    Active = 3,
    /// This block could not be processed (see `Reason`): delay-matched raw.
    Degraded = 5,
}
impl State {
    /// Stable log/diagnostics name (the serde name).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Armed => "armed",
            Self::Converging => "converging",
            Self::Active => "active",
            Self::Degraded => "degraded",
        }
    }
    /// AEC3 output is what this state sends.
    pub const fn engaged(self) -> bool {
        matches!(self, Self::Converging | Self::Active)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[repr(u8)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    #[default]
    None = 0,
    SwitchOff = 1,
    NoLocalRoute = 2,
    ReferenceMissing = 3,
    TimestampUnknown = 4,
    ClockUnstable = 5,
    ReferenceGap = 6,
    CaptureGap = 7,
    Deadline = 8,
    Overload = 9,
    Rebuilding = 10,
    OutputChanged = 11,
    Nonfinite = 13,
    Scheduling = 14,
    /// The selected reference arrives later than the causal read range.
    ReferenceLate = 15,
    /// The system output renders nothing (loopback idle): blocks pass raw
    /// until the silence is confirmed or audio resumes; not a fault.
    ReferenceIdle = 16,
}
impl Reason {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::SwitchOff,
            2 => Self::NoLocalRoute,
            3 => Self::ReferenceMissing,
            4 => Self::TimestampUnknown,
            5 => Self::ClockUnstable,
            6 => Self::ReferenceGap,
            7 => Self::CaptureGap,
            8 => Self::Deadline,
            9 => Self::Overload,
            10 => Self::Rebuilding,
            11 => Self::OutputChanged,
            13 => Self::Nonfinite,
            14 => Self::Scheduling,
            15 => Self::ReferenceLate,
            16 => Self::ReferenceIdle,
            _ => Self::None,
        }
    }
    /// Stable log/diagnostics name (the serde name).
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SwitchOff => "switch-off",
            Self::NoLocalRoute => "no-local-route",
            Self::ReferenceMissing => "reference-missing",
            Self::TimestampUnknown => "timestamp-unknown",
            Self::ClockUnstable => "clock-unstable",
            Self::ReferenceGap => "reference-gap",
            Self::CaptureGap => "capture-gap",
            Self::Deadline => "deadline",
            Self::Overload => "overload",
            Self::Rebuilding => "rebuilding",
            Self::OutputChanged => "output-changed",
            Self::Nonfinite => "nonfinite",
            Self::Scheduling => "scheduling",
            Self::ReferenceLate => "reference-late",
            Self::ReferenceIdle => "reference-idle",
        }
    }
}
/// The reference the processor cancels against. Never both at once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[repr(u8)]
#[serde(rename_all = "kebab-case")]
pub enum ReferenceSource {
    #[default]
    None = 0,
    /// Only Oink's own forwarded playback (fallback, and phones).
    Oink = 1,
    /// The whole output-device mix: Oink and every other local program.
    System = 2,
}
impl ReferenceSource {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Oink,
            2 => Self::System,
            _ => Self::None,
        }
    }
    fn kind(self) -> Option<reference::Kind> {
        match self {
            Self::None => None,
            Self::Oink => Some(reference::Kind::Oink),
            Self::System => Some(reference::Kind::System),
        }
    }
    /// Stable log/diagnostics name (the serde name).
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Oink => "oink",
            Self::System => "system",
        }
    }
}
/// State of the full-system reference capture and, when it is not running,
/// why the Oink playback reference is used instead. A capability choice, not
/// a masked fault: the platform's own failure text is logged with it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[repr(u8)]
#[serde(rename_all = "kebab-case")]
pub enum SystemStatus {
    /// Not started by this owner (phone, or AEC not eligible here).
    #[default]
    NotRequested = 0,
    Starting = 1,
    Running = 2,
    /// OS, API or output device cannot provide it (for example macOS before
    /// 14.2, or a duplex/multi-stream output on macOS).
    Unsupported = 3,
    /// macOS System Audio Recording is not granted; Oink does not prompt here.
    PermissionDenied = 4,
    StartFailed = 5,
    /// The running capture stopped with an error.
    CaptureFailed = 6,
    /// The output endpoint went away or changed; the playback owner reopens.
    OutputChanged = 7,
}
impl SystemStatus {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Starting,
            2 => Self::Running,
            3 => Self::Unsupported,
            4 => Self::PermissionDenied,
            5 => Self::StartFailed,
            6 => Self::CaptureFailed,
            7 => Self::OutputChanged,
            _ => Self::NotRequested,
        }
    }
    /// Stable log/diagnostics name (the serde name).
    pub const fn name(self) -> &'static str {
        match self {
            Self::NotRequested => "not-requested",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Unsupported => "unsupported",
            Self::PermissionDenied => "permission-denied",
            Self::StartFailed => "start-failed",
            Self::CaptureFailed => "capture-failed",
            Self::OutputChanged => "output-changed",
        }
    }
}
/// Claims remain independent. This rule adds no microphone demand and does not
/// require a particular receiver to also send speaker audio.
pub const fn eligible(enabled: bool, local_mic: bool, local_exit: bool, capture_running: bool) -> bool {
    enabled && local_mic && local_exit && capture_running
}

#[cfg(test)]
mod tests;
