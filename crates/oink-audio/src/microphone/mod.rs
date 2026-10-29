//! Explicit-device microphone capture. Constructing/querying never opens a mic.
//! The coordinator owns demand leases; dropping Capture closes the OS stream.
//! Policy, demand leases and media transport belong to the agent's microphone actor.
#[cfg(any(target_os = "macos", windows))]
mod capture;
mod resample;
mod selection;
pub mod session;
pub use selection::{DefaultInputHistory, InputCandidate, InputSelection, SelectionError};
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_default;
#[cfg(any(target_os = "macos", windows))]
pub use capture::{Capture, default_input, input_candidates, inputs, switch_default_input, virtual_output};
#[cfg(target_os = "macos")]
pub mod macos;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InputDevice {
    pub id: String,
    pub name: String,
}
impl InputDevice {
    /// Forwarding loopbacks cannot be captured as a microphone source, including
    /// when the OS default input points at our own virtual recording endpoint.
    pub fn is_forwarding_loopback(&self) -> bool {
        self.id.ends_with(MAC_INPUT_UID) || self.id.ends_with("com.across.microphone.input.v1")
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Granted,
    NotDetermined,
    Denied,
    Restricted,
    SystemControlled,
    #[default]
    Unsupported,
}
#[cfg(target_os = "macos")]
pub use macos::DemandMonitor;
use serde::{Deserialize, Serialize};
#[cfg(windows)]
pub use windows::{DemandMonitor, WorkerApartment};

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAMES_PER_BLOCK: usize = 240;
pub const MAC_INPUT_UID: &str = "com.hachiwii.oink.microphone.input.v1";
pub const MAC_FEED_UID: &str = "com.hachiwii.oink.microphone.feed.v1";

/// Read-only. In particular, this never requests AVFoundation authorization.
pub fn permission() -> Permission {
    #[cfg(target_os = "macos")]
    return macos::permission();
    #[cfg(windows)]
    return Permission::SystemControlled;
    #[cfg(not(any(target_os = "macos", windows)))]
    Permission::Unsupported
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum VirtualOutput {
    Missing,
    /// More than one match must be resolved by explicit ID, never first-match.
    Ambiguous(Vec<InputDevice>),
    Ready {
        render_id: String,
        recording_name: String,
    },
    #[default]
    Unsupported,
}

/// Explicit local UI action only. Querying permission or configuring a route
/// never calls this. The completion is observed through permission().
pub fn request_permission() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return macos::request_permission();
    #[cfg(not(target_os = "macos"))]
    Err("microphone authorization is managed by the operating system".into())
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn inputs() -> Result<Vec<InputDevice>, String> {
    Ok(Vec::new())
}
#[cfg(not(any(target_os = "macos", windows)))]
pub fn virtual_output() -> Result<VirtualOutput, String> {
    Ok(VirtualOutput::Unsupported)
}

/// Read-only demand observation. An epoch change retires an old consumer lifetime
/// even if StopIO and StartIO both happened between two actor polls.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Demand {
    pub active: bool,
    pub epoch: u64,
    /// Unknown/missing/unobservable is inactive, never a request to capture.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureInfo {
    pub device: InputDevice,
    pub backend: &'static str,
    pub hardware_sample_rate: u32,
    pub hardware_channels: u16,
    pub resampler_delay_frames: u32,
}

/// Both values must come from current local policy / a live receiver demand lease.
/// This guard prevents accidental starts during capability discovery. Stage two
/// must drop Capture immediately on revocation, last consumer stop or lease expiry.
#[derive(Clone, Copy, Debug)]
pub struct StartPermit {
    pub user_enabled: bool,
    pub receiver_has_consumer: bool,
}
impl StartPermit {
    pub fn check(self) -> Result<(), String> {
        if !self.user_enabled {
            return Err("microphone forwarding is disabled".into());
        }
        if !self.receiver_has_consumer {
            return Err("virtual microphone has no active consumer".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_requires_both_local_consent_and_consumer() {
        for enabled in [false, true] {
            for consumer in [false, true] {
                assert_eq!(
                    StartPermit {
                        user_enabled: enabled,
                        receiver_has_consumer: consumer
                    }
                    .check()
                    .is_ok(),
                    enabled && consumer
                );
            }
        }
    }
}

/// Local device-owner projections. Sharing itself is a group-scoped ordered
/// claim; a remote request can never bypass this device's permission preflight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub input: Option<String>,
    pub volume_percent: u16,
    pub muted: bool,
    pub receive_muted: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            input: None,
            volume_percent: 100,
            muted: false,
            // A fresh device stays Local until Receive is explicitly selected.
            receive_muted: true,
        }
    }
}

#[cfg(test)]
mod mode_tests {
    #[test]
    fn fresh_microphone_stays_local() {
        assert!(super::Settings::default().receive_muted);
    }
}
