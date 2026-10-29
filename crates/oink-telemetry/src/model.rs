use crate::{Error, Result};
use serde::{Deserialize, Serialize};
pub const POLICY_VERSION: u16 = 3;
pub const SCHEMA_VERSION: u16 = 3;

macro_rules! closed {
    ($name:ident { $($v:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($v),+ }
    };
}
closed!(Choice {
    Unasked,
    Denied,
    Granted
});
closed!(Region { Eu, Us });
closed!(Component {
    Agent,
    UiRust,
    UiWebview,
    FileProvider
});
closed!(CrashKind {
    RustPanic,
    NativeException,
    SwiftTrap,
    JsUnhandled,
    Unknown
});
closed!(ExceptionClass {
    Panic,
    BadAccess,
    BadInstruction,
    Arithmetic,
    Breakpoint,
    Abort,
    Resource,
    Guard,
    Unhandled,
    Unknown
});
closed!(ImageSlot {
    Agent,
    UiRust,
    FileProvider
});
closed!(ScriptSlot { Main });
closed!(Platform {
    Macos,
    Windows,
    Unknown
});
closed!(NetworkPath { Direct, Relay, Unknown });
closed!(Transport { Reliable, Lossy });
closed!(Codec { Pcm, Opus, Unknown });
closed!(StartBoundary {
    AppkitWillBegin,
    OleLoopEnter,
    ApiBeginAccepted,
    Unobserved
});
closed!(TargetStartBoundary {
    AppkitSessionReturned,
    OleCallEnter
});
closed!(SourceStage {
    ProbeRecognition,
    NativeRead,
    MetadataCompletion,
    Inventory,
    InventoryToLearned,
    LearnedToNotice,
    RecognizedToNotice
});
closed!(SetupResult {
    Ready,
    Failed,
    Cancelled,
    Timeout
});
closed!(PrepareStage {
    SourceRead,
    Normalization,
    Registration,
    NativePreparation,
    ParentVisible,
    ChildrenEnumerated,
    FirstUrl,
    Metadata,
    EntryToBegin,
    TargetPreparation,
    PreparationQueue,
    BeginDispatch,
    PendingWindow,
    SourcePress,
    NativeAcknowledgement,
    WritersPreview,
    NativeBegin,
    Preview,
    Namespace
});
closed!(SetupStage {
    Dial,
    InstanceAuth,
    Selection,
    PathInstall,
    ConfigReady,
    Migration,
    LowTcp,
    LowTls,
    LowBind,
    LowReady
});
closed!(AudioStage { Total, Packet, Decoded });
closed!(Family {
    Drag,
    Link,
    Input,
    Audio,
    Clipboard
});
closed!(OperationResult {
    Completed,
    Failed,
    TimedOut,
    Cancelled
});
closed!(TransferFailure {
    None,
    Unavailable,
    Permission,
    InvalidData,
    ResourceLimit,
    Io,
    Cancelled,
    Timeout
});
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SizeBucket {
    #[serde(rename = "unknown")]
    Unknown,
    #[serde(rename = "empty")]
    Empty,
    #[serde(rename = "le_64_kib")]
    UpTo64KiB,
    #[serde(rename = "le_1_mib")]
    UpTo1MiB,
    #[serde(rename = "le_16_mib")]
    UpTo16MiB,
    #[serde(rename = "le_256_mib")]
    UpTo256MiB,
    #[serde(rename = "gt_256_mib")]
    Larger,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThroughputBucket {
    #[serde(rename = "unknown")]
    Unknown,
    #[serde(rename = "lt_1_mib_s")]
    Below1MiBps,
    #[serde(rename = "le_10_mib_s")]
    UpTo10MiBps,
    #[serde(rename = "le_100_mib_s")]
    UpTo100MiBps,
    #[serde(rename = "gt_100_mib_s")]
    Above100MiBps,
}
impl SizeBucket {
    pub fn from_bytes(bytes: u64) -> Self {
        match bytes {
            0 => Self::Empty,
            1..=65_536 => Self::UpTo64KiB,
            65_537..=1_048_576 => Self::UpTo1MiB,
            1_048_577..=16_777_216 => Self::UpTo16MiB,
            16_777_217..=268_435_456 => Self::UpTo256MiB,
            _ => Self::Larger,
        }
    }
}
impl ThroughputBucket {
    pub fn measured(bytes: u64, elapsed_us: u64) -> Self {
        if bytes == 0 || elapsed_us == 0 {
            return Self::Unknown;
        }
        let rate = u128::from(bytes) * 1_000_000 / u128::from(elapsed_us);
        if rate < 1_048_576 {
            Self::Below1MiBps
        } else if rate <= 10_485_760 {
            Self::UpTo10MiBps
        } else if rate <= 104_857_600 {
            Self::UpTo100MiBps
        } else {
            Self::Above100MiBps
        }
    }
}
closed!(HistogramSchema {
    Duration14,
    Audio8,
    Jitter6
});
closed!(SamplingScheme { IndependentV3 });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportConsent {
    pub choice: Choice,
    pub policy_version: u16,
    pub generation: u64,
}
impl Default for ReportConsent {
    fn default() -> Self {
        Self {
            choice: Choice::Unasked,
            policy_version: POLICY_VERSION,
            generation: 0,
        }
    }
}
impl ReportConsent {
    pub fn granted(self) -> bool {
        self.choice == Choice::Granted && self.policy_version == POLICY_VERSION && self.generation != 0
    }
    pub fn change(self, expected: u64, choice: Choice, policy: u16) -> Result<Self> {
        if expected != self.generation {
            return Err(Error::Stale);
        }
        if policy != POLICY_VERSION || choice == Choice::Unasked {
            return Err(Error::Invalid);
        }
        Ok(Self {
            choice,
            policy_version: policy,
            generation: self.generation.checked_add(1).ok_or(Error::Limit)?,
        })
    }
}

#[cfg(test)]
mod consent_tests {
    use super::*;
    #[test]
    fn the_first_question_cannot_be_reset_after_a_choice() {
        let consent = ReportConsent::default();
        assert!(!consent.granted());
        let denied = consent.change(0, Choice::Denied, POLICY_VERSION).unwrap();
        assert_eq!(denied.change(1, Choice::Unasked, POLICY_VERSION), Err(Error::Invalid));
        assert_eq!(denied.choice, Choice::Denied);
        let restored: ReportConsent = serde_json::from_slice(&serde_json::to_vec(&denied).unwrap()).unwrap();
        assert_eq!(restored, denied);
        assert!(!restored.granted());
        assert!(restored.change(1, Choice::Granted, POLICY_VERSION).unwrap().granted());
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
    pub build: u32,
}
impl AppVersion {
    pub fn release(self) -> String {
        format!(
            "com.hachiwii.oink@{}.{}.{}+{}",
            self.major, self.minor, self.patch, self.build
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsVersionClass {
    Windows10,
    Windows11,
    MacOSMajor(u8),
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildContext {
    pub app_version: AppVersion,
    pub os_version_class: OsVersionClass,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformPair {
    pub source: Platform,
    pub destination: Platform,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "metric", rename_all = "snake_case", deny_unknown_fields)]
pub enum Metric {
    ClipboardPublish,
    ClipboardApply {
        result: OperationResult,
    },
    NativeRepresentationRead {
        result: OperationResult,
    },
    ClipboardTransfer {
        result: OperationResult,
        reason: TransferFailure,
    },
    FileFirstByte {
        size: SizeBucket,
    },
    FileComplete {
        result: OperationResult,
        reason: TransferFailure,
        size: SizeBucket,
        throughput: ThroughputBucket,
    },
    DragDropAccepted {
        result: OperationResult,
    },
    DragTransfer {
        result: OperationResult,
        reason: TransferFailure,
    },
    DragStart {
        boundary: StartBoundary,
    },
    DragTargetStart {
        boundary: TargetStartBoundary,
    },
    DragSourceStage {
        stage: SourceStage,
    },
    DragQueueWait,
    DragBeginWorker,
    DragPrepareStage {
        stage: PrepareStage,
    },
    LinkSetup {
        result: SetupResult,
    },
    LinkSetupStage {
        stage: SetupStage,
    },
    LowSetup {
        stage: SetupStage,
    },
    InputReceiveToNativeComplete,
    InputQueueWaitWindowMax,
    InputCaptureCallbackWindowMax,
    InputOldestPending,
    InputIngressVariation,
    InputDispatchVariation,
    AudioBuffered {
        stage: AudioStage,
    },
    AudioReceiveDispatch,
    AudioDispatchInsert,
    AudioPushToPresentationEstimate,
    AudioReportedOutputLatency,
    NetworkRtt,
}
impl Metric {
    pub fn family(self) -> Family {
        match self {
            Self::DragDropAccepted { .. }
            | Self::DragTransfer { .. }
            | Self::DragStart { .. }
            | Self::DragTargetStart { .. }
            | Self::DragSourceStage { .. }
            | Self::DragQueueWait
            | Self::DragBeginWorker
            | Self::DragPrepareStage { .. } => Family::Drag,
            Self::LinkSetup { .. } | Self::LinkSetupStage { .. } | Self::LowSetup { .. } | Self::NetworkRtt => {
                Family::Link
            }
            Self::InputReceiveToNativeComplete
            | Self::InputQueueWaitWindowMax
            | Self::InputCaptureCallbackWindowMax
            | Self::InputOldestPending
            | Self::InputIngressVariation
            | Self::InputDispatchVariation => Family::Input,
            Self::ClipboardPublish
            | Self::ClipboardApply { .. }
            | Self::NativeRepresentationRead { .. }
            | Self::ClipboardTransfer { .. }
            | Self::FileFirstByte { .. }
            | Self::FileComplete { .. } => Family::Clipboard,
            _ => Family::Audio,
        }
    }
    pub fn interval(self) -> bool {
        matches!(
            self,
            Self::ClipboardPublish
                | Self::ClipboardApply { .. }
                | Self::NativeRepresentationRead { .. }
                | Self::ClipboardTransfer { .. }
                | Self::FileFirstByte { .. }
                | Self::FileComplete { .. }
                | Self::DragDropAccepted { .. }
                | Self::DragTransfer { .. }
                | Self::DragStart { .. }
                | Self::DragTargetStart { .. }
                | Self::DragSourceStage { .. }
                | Self::DragQueueWait
                | Self::DragBeginWorker
                | Self::DragPrepareStage {
                    stage: PrepareStage::SourceRead
                        | PrepareStage::Normalization
                        | PrepareStage::Registration
                        | PrepareStage::NativePreparation
                        | PrepareStage::Metadata
                        | PrepareStage::EntryToBegin
                        | PrepareStage::TargetPreparation
                        | PrepareStage::PreparationQueue
                        | PrepareStage::BeginDispatch
                        | PrepareStage::PendingWindow
                        | PrepareStage::SourcePress
                        | PrepareStage::NativeAcknowledgement
                        | PrepareStage::WritersPreview
                        | PrepareStage::NativeBegin
                        | PrepareStage::Preview
                        | PrepareStage::Namespace
                }
                | Self::LinkSetup { .. }
                | Self::LinkSetupStage { .. }
                | Self::LowSetup { .. }
                | Self::InputReceiveToNativeComplete
                | Self::AudioPushToPresentationEstimate
        )
    }
    pub fn histogram(self) -> Option<HistogramSchema> {
        match self {
            Self::InputIngressVariation | Self::InputDispatchVariation => Some(HistogramSchema::Jitter6),
            Self::AudioReceiveDispatch | Self::AudioDispatchInsert => Some(HistogramSchema::Audio8),
            Self::InputQueueWaitWindowMax
            | Self::InputCaptureCallbackWindowMax
            | Self::InputOldestPending
            | Self::AudioBuffered { .. }
            | Self::AudioReportedOutputLatency
            | Self::NetworkRtt
            | Self::DragPrepareStage {
                stage: PrepareStage::ParentVisible | PrepareStage::ChildrenEnumerated | PrepareStage::FirstUrl,
            } => Some(HistogramSchema::Duration14),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::ClipboardPublish => "clipboard_publish",
            Self::ClipboardApply { .. } => "clipboard_apply",
            Self::NativeRepresentationRead { .. } => "native_representation_read",
            Self::ClipboardTransfer { .. } => "clipboard_transfer",
            Self::FileFirstByte { .. } => "file_first_byte",
            Self::FileComplete { .. } => "file_complete",
            Self::DragDropAccepted { .. } => "drag_drop_accepted",
            Self::DragTransfer { .. } => "drag_transfer",
            Self::DragStart { .. } => "drag_start",
            Self::DragTargetStart { .. } => "drag_target_start",
            Self::DragSourceStage { .. } => "drag_source_stage",
            Self::DragQueueWait => "drag_queue_wait",
            Self::DragBeginWorker => "drag_begin_worker",
            Self::DragPrepareStage { .. } => "drag_prepare_stage",
            Self::LinkSetup { .. } => "link_setup",
            Self::LinkSetupStage { .. } => "link_setup_stage",
            Self::LowSetup { .. } => "low_setup",
            Self::NetworkRtt => "network_rtt",
            Self::InputReceiveToNativeComplete => "input_receive_to_native_complete",
            Self::InputQueueWaitWindowMax => "input_queue_wait_window_max",
            Self::InputCaptureCallbackWindowMax => "input_capture_callback_window_max",
            Self::InputOldestPending => "input_oldest_pending",
            Self::InputIngressVariation => "input_ingress_variation",
            Self::InputDispatchVariation => "input_dispatch_variation",
            Self::AudioBuffered { .. } => "audio_buffered",
            Self::AudioReceiveDispatch => "audio_receive_dispatch",
            Self::AudioDispatchInsert => "audio_dispatch_insert",
            Self::AudioPushToPresentationEstimate => "audio_push_to_presentation_estimate",
            Self::AudioReportedOutputLatency => "audio_reported_output_latency",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasurementContext {
    pub build: BuildContext,
    pub metric: Metric,
    pub platform_pair: Option<PlatformPair>,
    pub network_path: Option<NetworkPath>,
    pub transport: Option<Transport>,
    pub codec: Option<Codec>,
}
impl MeasurementContext {
    pub fn validate(self) -> Result<()> {
        let family = self.metric.family();
        if (self.codec.is_some() && family != Family::Audio)
            || (self.transport.is_some() && family != Family::Input)
            || (family == Family::Clipboard && (self.platform_pair.is_some() || self.network_path.is_some()))
            || (matches!(self.metric, Metric::InputCaptureCallbackWindowMax)
                && (self.platform_pair.is_some() || self.network_path.is_some() || self.transport.is_some()))
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Native {
        image: ImageSlot,
        offset: u64,
        debug_id: [u8; 16],
        debug_age: u32,
        code_id: Option<u64>,
        image_size: u64,
    },
    Script {
        script: ScriptSlot,
        line: u32,
        column: u32,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafeCrash {
    pub build: BuildContext,
    pub component: Component,
    pub crash_kind: CrashKind,
    pub exception_class: ExceptionClass,
    pub exception_code: Option<u32>,
    // Two fixed arrays keep both producer allocation and deserialization bounded.
    pub frames: [[Option<Frame>; 32]; 2],
    pub callsite: Option<u32>,
    pub line: Option<u32>,
    pub column: Option<u32>,
}
impl SafeCrash {
    pub fn empty(
        build: BuildContext,
        component: Component,
        crash_kind: CrashKind,
        exception_class: ExceptionClass,
    ) -> Self {
        Self {
            build,
            component,
            crash_kind,
            exception_class,
            exception_code: None,
            frames: [[None; 32]; 2],
            callsite: None,
            line: None,
            column: None,
        }
    }
    pub fn frames(&self) -> impl Iterator<Item = &Frame> {
        self.frames.iter().flatten().flatten()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredInterval {
    pub context: MeasurementContext,
    pub elapsed_us: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformanceDistribution {
    pub context: MeasurementContext,
    pub histogram: HistogramSchema,
    pub bins: [u32; 14],
    pub sample_count: u32,
    pub missing: u32,
    pub saturated: bool,
    pub window_us: Option<u64>,
    pub seal_us: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "report", content = "data", rename_all = "snake_case", deny_unknown_fields)]
// Fixed storage also permits allocation-free numeric producer queues.
#[allow(clippy::large_enum_variant)]
pub enum Report {
    SafeCrash(SafeCrash),
    MeasuredInterval(MeasuredInterval),
    PerformanceDistribution(PerformanceDistribution),
}
impl Report {
    pub fn crash(&self) -> bool {
        matches!(self, Self::SafeCrash(_))
    }
    pub fn build(&self) -> BuildContext {
        match self {
            Self::SafeCrash(c) => c.build,
            Self::MeasuredInterval(m) => m.context.build,
            Self::PerformanceDistribution(m) => m.context.build,
        }
    }
    pub fn context(&self) -> Option<MeasurementContext> {
        match self {
            Self::SafeCrash(_) => None,
            Self::MeasuredInterval(m) => Some(m.context),
            Self::PerformanceDistribution(m) => Some(m.context),
        }
    }
    pub fn duration_us(&self) -> u64 {
        match self {
            Self::SafeCrash(_) => 0,
            Self::MeasuredInterval(m) => m.elapsed_us,
            Self::PerformanceDistribution(m) => m.seal_us,
        }
    }
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::SafeCrash(c) => {
                if c.frames().any(|f| match f {
                    Frame::Native {
                        debug_id,
                        offset,
                        image_size,
                        ..
                    } => *debug_id == [0; 16] || *image_size == 0 || *offset >= *image_size,
                    Frame::Script { line, .. } => *line == 0,
                }) {
                    return Err(Error::Invalid);
                }
            }
            Self::MeasuredInterval(m) => {
                m.context.validate()?;
                if !m.context.metric.interval() || m.elapsed_us > 120_000_000 {
                    return Err(Error::Invalid);
                }
            }
            Self::PerformanceDistribution(m) => {
                m.context.validate()?;
                let n = match m.histogram {
                    HistogramSchema::Duration14 => 14,
                    HistogramSchema::Audio8 => 8,
                    HistogramSchema::Jitter6 => 6,
                };
                if m.context.metric.histogram() != Some(m.histogram)
                    || m.sample_count == 0
                    || m.bins[n..].iter().any(|n| *n != 0)
                    || m.bins.iter().fold(0u32, |a, b| a.saturating_add(*b)) != m.sample_count
                    || m.seal_us > 120_000_000
                {
                    return Err(Error::Invalid);
                }
                if matches!(
                    m.context.metric,
                    Metric::InputQueueWaitWindowMax
                        | Metric::InputCaptureCallbackWindowMax
                        | Metric::InputOldestPending
                ) && m.window_us.filter(|v| *v > 0).is_none()
                {
                    return Err(Error::Invalid);
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedReport {
    pub schema_version: u16,
    pub policy_version: u16,
    pub event_id: [u8; 16],
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub occurrence_hour: u64,
    /// Four source-commit bytes, captured on admission and persisted with the
    /// report. Never read a runtime label or relabel a queue after an upgrade.
    pub source_revision: Option<[u8; 4]>,
    pub report: Report,
}
impl AdmittedReport {
    pub(crate) fn new(report: Report, at: u64) -> Result<Self> {
        report.validate()?;
        let mut ids = [0; 40];
        getrandom::fill(&mut ids).map_err(|_| Error::Entropy)?;
        Ok(Self {
            schema_version: 3,
            policy_version: 3,
            event_id: ids[..16].try_into().unwrap(),
            trace_id: ids[16..32].try_into().unwrap(),
            span_id: ids[32..].try_into().unwrap(),
            occurrence_hour: at / 3600 * 3600,
            source_revision: source_revision(
                report.build().app_version,
                option_env!("OINK_RELEASE_VERSION").unwrap_or(""),
                option_env!("OINK_RELEASE_BUILD").unwrap_or(""),
                option_env!("OINK_SOURCE_COMMIT").unwrap_or(""),
            ),
            report,
        })
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 3
            || self.policy_version != 3
            || !self.occurrence_hour.is_multiple_of(3600)
            || self.event_id == [0; 16]
            || self.trace_id == [0; 16]
            || self.span_id == [0; 8]
        {
            return Err(Error::Invalid);
        }
        self.report.validate()
    }
}

fn source_revision(version: AppVersion, display: &str, build: &str, commit: &str) -> Option<[u8; 4]> {
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return None;
    }
    if build.parse::<u32>().ok() != Some(version.build)
        || display
            != format!(
                "{}.{}.{}-beta.{}",
                version.major,
                version.minor,
                version.patch,
                &commit[..8]
            )
    {
        return None;
    }
    Some(std::array::from_fn(|i| {
        u8::from_str_radix(&commit[i * 2..i * 2 + 2], 16).unwrap()
    }))
}

#[cfg(test)]
mod revision_tests {
    use super::*;

    #[test]
    fn only_the_matching_compiled_build_supplies_a_commit_suffix() {
        let version = AppVersion {
            major: 0,
            minor: 1,
            patch: 0,
            build: 17,
        };
        let commit = "0123abcd".repeat(5);
        assert_eq!(
            source_revision(version, "0.1.0-beta.0123abcd", "17", &commit),
            Some([1, 35, 171, 205])
        );
        for (display, build, source) in [
            ("0.1.0-beta.1", "17", commit.as_str()),
            ("0.1.0-beta.0123abcd", "18", commit.as_str()),
            ("0.2.0-beta.0123abcd", "17", commit.as_str()),
            ("0.1.0-beta.0123abcd", "17", "arbitrary-runtime-name"),
        ] {
            assert_eq!(source_revision(version, display, build, source), None);
        }
    }
}

#[cfg(test)]
mod transfer_bucket_tests {
    use super::*;
    #[test]
    fn sizes_and_rates_have_fixed_coarse_boundaries() {
        assert_eq!(SizeBucket::from_bytes(0), SizeBucket::Empty);
        assert_eq!(SizeBucket::from_bytes(65_536), SizeBucket::UpTo64KiB);
        assert_eq!(SizeBucket::from_bytes(65_537), SizeBucket::UpTo1MiB);
        assert_eq!(SizeBucket::from_bytes(16_777_216), SizeBucket::UpTo16MiB);
        assert_eq!(SizeBucket::from_bytes(u64::MAX), SizeBucket::Larger);
        assert_eq!(ThroughputBucket::measured(1, 0), ThroughputBucket::Unknown);
        assert_eq!(ThroughputBucket::measured(0, 1), ThroughputBucket::Unknown);
        assert_eq!(
            ThroughputBucket::measured(1_048_575, 1_000_000),
            ThroughputBucket::Below1MiBps
        );
        assert_eq!(
            ThroughputBucket::measured(1_048_576, 1_000_000),
            ThroughputBucket::UpTo10MiBps
        );
        assert_eq!(
            ThroughputBucket::measured(10_485_760, 1_000_000),
            ThroughputBucket::UpTo10MiBps
        );
        assert_eq!(
            ThroughputBucket::measured(104_857_600, 1_000_000),
            ThroughputBucket::UpTo100MiBps
        );
        assert_eq!(ThroughputBucket::measured(u64::MAX, 1), ThroughputBucket::Above100MiBps);
    }
}
