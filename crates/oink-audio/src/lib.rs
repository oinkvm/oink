//! System-output capture, playback and claim-bound exit volume controls.
//! Capture never changes endpoint volume or default devices; source mute uses a
//! durable restoration journal. Explicit exit controls target one exact output.
mod capture_permission;
pub mod claims;
#[cfg(any(target_os = "macos", test))]
mod endpoint_mute;
pub mod media;
pub mod model;
pub mod negotiation;
pub mod session;
pub mod volume;
pub use capture_permission::CapturePreflight;
#[cfg(target_os = "macos")]
pub use capture_permission::{capture_permission_preflight, request_capture_permission};
#[cfg(test)]
mod allocation_audit;
pub mod device_notifications;
pub mod diagnostics;
pub mod dsp;
pub mod forward_volume;
mod handoff;
mod levels;
pub use levels::{CaptureLevels, SampleLevels};
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod macos_mute;
#[cfg(target_os = "macos")]
pub use macos_mute::recover;
#[cfg(target_os = "macos")]
pub mod macos_devices;
#[cfg(target_os = "macos")]
mod macos_playback;
pub mod media_pause;
pub mod playback;
pub mod power;
pub mod realtime;
pub mod recovery;
pub mod scheduling;
pub mod sink;
pub mod source;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_playback;
#[cfg(windows)]
pub(crate) mod windows_timer;
#[cfg(target_os = "macos")]
pub use macos::{Capture, MacCaptureOptions};
#[cfg(windows)]
pub use windows::{Capture, endpoint_state, recover};

pub use model::{
    AudioPhase, CapturePermission, DeviceAudioState, ForwardingOffReason, OutputDevice, OutputLatency, Problem,
    ReceiverReport,
};
pub use negotiation::Profile as StreamFormat;

/// Preserve the Windows endpoint's full friendly name. CPAL 0.17 puts
/// DeviceDesc (e.g. `Speakers`) in name(), and Device_FriendlyName
/// (`Speakers (USB Audio)`) in the first extended line. Keep that full
/// name for speaker output enumeration.
#[cfg(not(target_os = "macos"))]
pub(crate) fn device_name(description: &cpal::DeviceDescription) -> &str {
    #[cfg(windows)]
    if let Some(friendly) = description.extended().first() {
        return friendly;
    }
    description.name()
}
use serde::{Deserialize, Serialize};

/// Local capture selection; never propagated by the group audio route.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureScope {
    #[default]
    DefaultOutput,
    Global,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Exact local output identifier; None follows the local default output.
    /// The selected exit never carries another device's output identifier.
    #[serde(deserialize_with = "deserialize_receive_output")]
    pub receive_output: Option<String>,
    /// Committed atomically with an automatic forwarding disable.
    pub forwarding_off_reason: Option<ForwardingOffReason>,
    /// Keep the source audible locally while forwarding. Local only.
    pub keep_local_playback: bool,
    pub capture_scope: CaptureScope,
    /// Preferred receive format on this device when it is the audio exit.
    /// Local only: sources follow the exit's MediaPrepared; unsupported preferences
    /// fall back to the supported intersection.
    pub preferred_format: StreamFormat,
    /// Local source opt-in, off on a fresh install. Selecting an exit does not
    /// enable sources; this preference does not affect exit playback.
    pub forwarding_enabled: bool,
    /// Local forwarding control position: 0..=200, 100 is unity.
    /// A continuous fit of the measured Mac dB curve, with a smooth boost
    /// extension. This persisted value is not an amplitude percentage.
    #[serde(deserialize_with = "deserialize_forwarding_volume")]
    pub forwarding_volume_percent: u16,
    /// Local persisted mute after forwarding gain; gain remains unchanged.
    pub forwarding_muted: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            receive_output: None,
            forwarding_off_reason: None,
            keep_local_playback: false,
            capture_scope: CaptureScope::DefaultOutput,
            preferred_format: StreamFormat::Opus5ms,
            forwarding_enabled: false,
            forwarding_volume_percent: 100,
            forwarding_muted: false,
        }
    }
}
fn deserialize_forwarding_volume<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u16, D::Error> {
    let percent = u16::deserialize(deserializer)?;
    if percent > 200 {
        return Err(serde::de::Error::custom("forwarding_volume_percent must be in 0..=200"));
    }
    Ok(percent)
}
fn deserialize_receive_output<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let output = Option::<String>::deserialize(deserializer)?;
    validate_receive_output(output.as_deref()).map_err(serde::de::Error::custom)?;
    Ok(output)
}
pub fn validate_receive_output(output: Option<&str>) -> Result<(), &'static str> {
    if output.is_some_and(|id| id.is_empty() || id.len() > 512 || id.contains('\0')) {
        return Err("receive_output must be a nonempty output identifier of at most 512 bytes");
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceOutputs {
    pub device: oink_foundation::DeviceId,
    pub outputs: Vec<OutputDevice>,
    pub capture: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceState {
    pub device: oink_foundation::DeviceId,
    pub state: DeviceAudioState,
    /// Milliseconds since this report arrived. Reports older than 3 seconds are stale.
    pub age_ms: u64,
}
/// Why an enabled forwarding switch currently has no media channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspensionReason {
    Connecting,
    GroupPaused,
    Sleeping,
    Preparing,
    ExitUnavailable,
    DeviceUnavailable,
    MuteReleaseUnconfirmed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// This device's last-three-seconds audio hold, from the network pacer.
    /// Speaker silence is excluded.
    /// Forwarding can remain enabled and transport marked silence while false.
    pub audio_activity: bool,
    pub settings: Settings,
    #[serde(default)]
    pub resolution: claims::Resolution,
    pub devices: Vec<DeviceOutputs>,
    #[serde(default)]
    pub device_states: Vec<DeviceState>,
    #[serde(default)]
    pub capture_permission: CapturePermission,
    #[serde(with = "model::phase_json")]
    pub phase: AudioPhase,
    pub suspension: Option<SuspensionReason>,
    #[serde(default)]
    pub forwarding_off_reason: Option<ForwardingOffReason>,
    pub problem: Option<Problem>,
    pub source_muted: bool,
    #[serde(default)]
    pub mute_release_unconfirmed: bool,
    pub mute_recovery: Option<recovery::RecoveryReport>,
    #[serde(default)]
    pub capture_info: Option<CaptureInfo>,
    /// Last completed level window of this machine's local captured source.
    /// None before the first window or while capture is stopped.
    #[serde(default)]
    pub capture_levels: Option<CaptureLevels>,
    /// Most recent local capture restart cause; retained after a successful rebuild.
    #[serde(default)]
    pub capture_restart_reason: Option<Problem>,
    #[serde(default)]
    pub playback_info: Option<playback::PlaybackInfo>,
    pub sent: u64,
    pub send_dropped: u64,
    pub sources: Vec<(oink_foundation::DeviceId, dsp::Stats)>,
    /// Measured QUIC round-trip time (not audio end-to-end latency).
    pub network_rtt_ms: Option<f64>,
    /// Reserved for a future measured estimate. None until calibrated; RTT is separate.
    pub estimated_latency_ms: Option<f64>,
}
impl Default for Status {
    fn default() -> Self {
        Self {
            audio_activity: Default::default(),
            settings: Default::default(),
            resolution: Default::default(),
            devices: Default::default(),
            device_states: Default::default(),
            capture_permission: Default::default(),
            phase: AudioPhase::Idle,
            suspension: Default::default(),
            forwarding_off_reason: Default::default(),
            problem: Default::default(),
            source_muted: Default::default(),
            mute_release_unconfirmed: Default::default(),
            mute_recovery: Default::default(),
            capture_info: Default::default(),
            capture_levels: Default::default(),
            capture_restart_reason: Default::default(),
            playback_info: Default::default(),
            sent: Default::default(),
            send_dropped: Default::default(),
            sources: Default::default(),
            network_rtt_ms: Default::default(),
            estimated_latency_ms: Default::default(),
        }
    }
}
/// Capture backend metadata, independent of current consent. Authorization is
/// observed separately; this label must never be used as a permission check.
pub fn capture_capability() -> &'static str {
    if cfg!(target_os = "macos") {
        "core_audio_tap_14_2"
    } else if cfg!(windows) {
        "wasapi_endpoint_loopback_journaled_mute_driver_dependent"
    } else {
        "unsupported"
    }
}
#[cfg(not(any(target_os = "macos", windows)))]
#[derive(Debug)]
pub struct Capture;
#[cfg(not(any(target_os = "macos", windows)))]
impl Capture {
    pub fn set_forwarding_pause(&self, _: Option<media_pause::ForwardingPause>) {}
    pub fn info(&self) -> CaptureInfo {
        CaptureInfo::default()
    }
    pub fn error(&self) -> Option<String> {
        None
    }
    pub fn source_muted(&self) -> bool {
        false
    }
}

/// Timed 48 kHz interleaved stereo PCM. Platform timestamps are optional; no
/// caller may silently replace missing hardware timing with dequeue time.
#[cfg(all(test, target_os = "macos"))]
#[derive(Clone, Debug)]
pub(crate) struct CaptureBlock {
    pub samples: Vec<f32>,
    pub first_sample_at: Option<std::time::Instant>,
    pub callback_at: Option<std::time::Instant>,
    pub source_frame_position: Option<u64>,
}

/// Observed backend settings. Frame counts use `sample_rate`, not the converted
/// 48 kHz stream rate. None means the platform could not report that parameter.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureInfo {
    /// Backend observations.
    pub diagnostics: serde_json::Value,
    pub backend: String,
    pub sample_rate: u32,
    pub requested_buffer_frames: Option<u32>,
    pub actual_buffer_frames: u32,
    pub engine_period_frames: Option<u32>,
    pub engine_default_period_frames: Option<u32>,
    pub engine_min_period_frames: Option<u32>,
    pub engine_max_period_frames: Option<u32>,
    pub engine_fundamental_period_frames: Option<u32>,
    pub reported_latency_frames: Option<u32>,
    pub safety_offset_frames: Option<u32>,
    pub physical_output_id: Option<String>,
    pub physical_output_sample_rate: Option<u32>,
    pub physical_output_buffer_frames: Option<u32>,
    pub physical_output_original_buffer_frames: Option<u32>,
    pub physical_output_buffer_lease_changed: bool,
    pub physical_output_latency_frames: Option<u32>,
    pub physical_output_safety_offset_frames: Option<u32>,
    pub timer_resolution_requested_ms: Option<u32>,
    pub timer_resolution_request_succeeded: bool,
    pub timer_resolution_error: Option<String>,
    pub timestamp_source: String,
    pub fallback_reason: Option<String>,
}

#[cfg(test)]
mod settings_tests {
    use super::*;
    #[test]
    fn capture_scope_defaults_to_current_output_and_global_is_explicit() {
        let settings: Settings = serde_json::from_str(r#"{"keep_local_playback":false}"#).unwrap();
        assert_eq!(settings.capture_scope, CaptureScope::DefaultOutput);
        assert_eq!(
            serde_json::to_value(&settings).unwrap()["capture_scope"],
            "default_output"
        );
        let global: Settings = serde_json::from_str(r#"{"capture_scope":"global"}"#).unwrap();
        assert_eq!(global.capture_scope, CaptureScope::Global);
        assert_eq!(
            serde_json::from_value::<Settings>(serde_json::to_value(global).unwrap())
                .unwrap()
                .capture_scope,
            CaptureScope::Global
        );
        assert!(serde_json::from_str::<Settings>(r#"{"capture_scope":"typo"}"#).is_err());
    }
    #[test]
    fn receive_profile_defaults_to_opus_five_ms_and_round_trips() {
        assert_eq!(Settings::default().preferred_format, StreamFormat::Opus5ms);
        for format in [StreamFormat::Opus5ms, StreamFormat::Pcm16_2p5ms] {
            let json = serde_json::json!({"preferred_format": format});
            let settings: Settings = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(settings.preferred_format, format);
            assert_eq!(
                serde_json::to_value(settings).unwrap()["preferred_format"],
                json["preferred_format"]
            );
        }
        let pcm: Settings = serde_json::from_str(r#"{"preferred_format":"pcm16_2p5ms"}"#).unwrap();
        assert_eq!(pcm.preferred_format, StreamFormat::Pcm16_2p5ms);
        assert!(serde_json::from_str::<Settings>(r#"{"preferred_format":"typo"}"#).is_err());
    }
    #[test]
    fn forwarding_is_opt_in_and_explicit_choices_round_trip() {
        assert!(!Settings::default().forwarding_enabled);
        for enabled in [false, true] {
            let settings: Settings =
                serde_json::from_value(serde_json::json!({"forwarding_enabled": enabled})).unwrap();
            assert_eq!(settings.forwarding_enabled, enabled);
            let restored: Settings = serde_json::from_value(serde_json::to_value(settings).unwrap()).unwrap();
            assert_eq!(restored.forwarding_enabled, enabled);
        }
    }

    #[test]
    fn forwarding_volume_defaults_round_trips_and_rejects_invalid_percentages() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.forwarding_volume_percent, 100);
        assert!(!settings.forwarding_enabled);
        for percent in [0, 50, 100, 150, 200] {
            let settings: Settings =
                serde_json::from_value(serde_json::json!({"forwarding_volume_percent": percent})).unwrap();
            assert_eq!(settings.forwarding_volume_percent, percent);
            assert_eq!(
                serde_json::to_value(&settings).unwrap()["forwarding_volume_percent"],
                percent
            );
        }
        for invalid in [
            serde_json::json!(-1),
            serde_json::json!(201),
            serde_json::json!(1.5),
            serde_json::json!(null),
        ] {
            assert!(
                serde_json::from_value::<Settings>(serde_json::json!({"forwarding_volume_percent": invalid})).is_err()
            );
        }
    }
}

pub mod idle;
