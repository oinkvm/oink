//! Full, immutable card snapshots and typed local control results.
//! Unknown data is distinct from a successful resource report. Only the owner
//! may diagnose a resource or confirm a control; transport acceptance is pending.

use crate::api::{DeviceKey, GroupKey};
use serde::{Deserialize, Serialize};

macro_rules! codes {
    ($name:ident { $($value:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($value),+ }
    };
}

codes!(CardOs {
    Macos,
    Windows,
    Linux,
    Other
});
codes!(CardLogo {
    Macos,
    Windows10,
    Linux,
    Generic
});
codes!(CardSection {
    ThisDevice,
    Online,
    Offline
});
codes!(OfflineKind {
    Disconnected,
    Connecting,
    AddressStale,
    ProtocolIncompatible
});
codes!(SignalColour {
    Green,
    Yellow,
    Red,
    Grey
});
codes!(SharingMode {
    Shared,
    Local,
    Disabled
});
codes!(UserFacingCode {
    Unspecified,
    PermissionRequired,
    PermissionDenied,
    CaptureUnavailable,
    InjectionUnavailable,
    SecureInput,
    DeviceUnavailable,
    CleanupFailed,
    Unsupported,
    BackendFailed,
    LinkLost,
    Timeout,
    StorageFull,
    InvalidValue,
    StateConflict,
    LocalServiceUnavailable,
    InvalidAddress,
    NotMember,
    PersistenceFailed,
    NetworkStopped,
    Unrecognized
});
codes!(ResourceFaultCode {
    None,
    PermissionRequired,
    PermissionDenied,
    CaptureUnavailable,
    InjectionUnavailable,
    SecureInput,
    DeviceUnavailable,
    CleanupFailed,
    Unsupported,
    BackendFailed,
    Unrecognized
});
codes!(CardCondition {
    Ready,
    LocalPaused,
    Sleeping,
    Starting,
    WaitingForTarget,
    ScreenDisabledFallback,
    Faulted,
    Unsupported,
    StateUnknown,
    StateStale
});
codes!(CardActionUnavailable {
    Offline,
    Unsupported,
    StateUnknown,
    StateStale,
    RemoteFault,
    NotMember,
    Busy
});
codes!(CardCommand {
    SetScreenMode,
    SetLocalOption,
    ClearAudioExit,
    MakeAudioExit,
    SetGameMode,
    SetDevicePaused
});
codes!(SharingIcon {
    Shared,
    Local,
    Disabled,
    Fault,
    Unknown
});
codes!(SpeakerMode {
    Unused,
    Receiver,
    Source
});
codes!(SpeakerIcon {
    Unused,
    Exit,
    Forwarding,
    Fault,
    Unknown
});
codes!(SpeakerActivity {
    Idle,
    Playing,
    Forwarding,
    SoundSleep,
    Preparing,
    WaitingForExit,
    Paused,
    Sleeping,
    Failed,
    Unknown
});
codes!(CursorIcon {
    Local,
    Remote,
    Locked,
    Unknown
});
codes!(CursorContext {
    Group,
    Independent,
    LocalPaused,
    Unknown
});
codes!(GamePreference { Auto, On, Off });
codes!(PauseReason {
    None,
    LocalUser,
    RemoteUser,
    Emergency,
    InputBacklog,
    Unknown
});
codes!(DeliveryState {
    Queued,
    Confirmed,
    Unconfirmed,
    Unreachable
});
codes!(ExitVolumeCondition {
    Ready,
    Offline,
    StateUnknown,
    Unsupported,
    Unavailable,
    PermissionDenied,
    BackendFailed
});
codes!(ExitVolumeRefusal {
    Offline,
    Unsupported,
    StateUnknown,
    StateStale,
    RemoteFault,
    NotMember,
    Busy,
    OutputUnavailable,
    PermissionDenied
});
codes!(DeviceControlFailure {
    Offline,
    Unsupported,
    StaleBoot,
    StaleRevision,
    NotMember,
    ResourceFault,
    InvalidValue,
    Busy,
    PersistenceFailed,
    ApplyFailed,
    LinkLost,
    OutcomeUnknown,
    NotExit,
    RouteChanged,
    OutputChanged,
    OutputUnavailable,
    VolumeUnsupported,
    PermissionDenied,
    Superseded,
    UnsupportedOption,
    StateConflict,
    Unrecognized
});

/// Decimal text preserves all u64 revision bits in JavaScript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DecimalU64(pub u64);
impl Serialize for DecimalU64 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}
impl<'de> Deserialize<'de> for DecimalU64 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.is_empty() || (text.len() > 1 && text.starts_with('0')) || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(serde::de::Error::custom("expected canonical decimal u64"));
        }
        text.parse().map(Self).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Signal {
    Measured {
        rtt_ms: f64,
        bars: u8,
        colour: SignalColour,
    },
    Unknown {
        rtt_ms: Option<f64>,
        bars: u8,
        colour: SignalColour,
    },
    Offline {
        rtt_ms: Option<f64>,
        bars: u8,
        colour: SignalColour,
    },
}
impl Signal {
    pub fn online(rtt_ms: Option<f64>) -> Self {
        match rtt_ms.filter(|v| v.is_finite() && *v >= 0.) {
            Some(rtt_ms) => {
                let (bars, colour) = if rtt_ms < 20. {
                    (3, SignalColour::Green)
                } else if rtt_ms <= 50. {
                    (2, SignalColour::Yellow)
                } else {
                    (1, SignalColour::Red)
                };
                Self::Measured { rtt_ms, bars, colour }
            }
            None => Self::Unknown {
                rtt_ms: None,
                bars: 0,
                colour: SignalColour::Grey,
            },
        }
    }
    pub fn offline() -> Self {
        Self::Offline {
            rtt_ms: None,
            bars: 0,
            colour: SignalColour::Grey,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardConnection {
    #[serde(rename = "self")]
    ThisDevice,
    Online {
        signal: Signal,
    },
    Offline {
        reason: OfflineKind,
        signal: Signal,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserFacingProblem {
    pub code: UserFacingCode,
    pub unknown_code: Option<u32>,
    pub native_code: Option<u32>,
    pub retry_after_ms: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceFault {
    pub code: ResourceFaultCode,
    pub unknown_code: Option<u32>,
    pub problem: Option<UserFacingProblem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardAction {
    Invoke { command: CardCommand, token: String },
    OpenPermissions,
    Unavailable { reason: CardActionUnavailable },
}
impl CardAction {
    pub fn unavailable(reason: CardActionUnavailable) -> Self {
        Self::Unavailable { reason }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceCard<M = SharingMode, I = SharingIcon> {
    pub action: CardAction,
    pub configured_mode: Option<M>,
    pub icon: I,
    pub fault: Option<ResourceFault>,
    pub condition: CardCondition,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardCard {
    #[serde(flatten)]
    pub resource: ResourceCard,
    pub disabled: Option<bool>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorCard {
    pub icon: CursorIcon,
    pub focus_device: Option<DeviceKey>,
    pub focus_name: Option<String>,
    pub locked_device: Option<DeviceKey>,
    pub context: CursorContext,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardingCard {
    pub volume_percent: u16,
    pub volume_bars: u8,
    pub muted: bool,
}
impl ForwardingCard {
    pub fn new(volume_percent: u16, muted: bool) -> Option<Self> {
        let volume_bars = match volume_percent {
            0 => 0,
            1..=33 => 1,
            34..=66 => 2,
            67..=100 => 3,
            101..=200 => 4,
            _ => return None,
        };
        Some(Self {
            volume_percent,
            volume_bars,
            muted,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeakerCard {
    #[serde(flatten)]
    pub resource: ResourceCard<SpeakerMode, SpeakerIcon>,
    pub make_exit_action: CardAction,
    pub forwarding_controls_token: Option<String>,
    pub forwarding_enabled: Option<bool>,
    pub activity: SpeakerActivity,
    pub receiver_device: Option<DeviceKey>,
    pub receiver_name: Option<String>,
    pub active_sources: Option<u8>,
    pub receive_ready: Option<bool>,
    pub forwarding: Option<ForwardingCard>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameActions {
    pub auto: CardAction,
    pub on: CardAction,
    pub off: CardAction,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameCard {
    pub preference: Option<GamePreference>,
    pub active: Option<bool>,
    pub detected: Option<bool>,
    pub actions: GameActions,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCardActions {
    pub screen_shared: CardAction,
    pub screen_local: CardAction,
    pub mouse_shared: CardAction,
    pub mouse_local: CardAction,
    pub mouse_disabled: CardAction,
    pub keyboard_shared: CardAction,
    pub keyboard_local: CardAction,
    pub keyboard_disabled: CardAction,
    pub keyboard_enabled: CardAction,
    pub clipboard_shared: CardAction,
    pub clipboard_local: CardAction,
    pub forwarding_enabled: CardAction,
    pub forwarding_disabled: CardAction,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCardStatus {
    pub actions: DeviceCardActions,
    pub cursor: CursorCard,
    pub screen: ResourceCard,
    pub mouse: ResourceCard,
    pub keyboard: KeyboardCard,
    pub speaker: SpeakerCard,
    pub clipboard: ResourceCard,
    pub game: GameCard,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalPauseCard {
    pub action: CardAction,
    pub paused: bool,
    pub reason: PauseReason,
    pub revision: DecimalU64,
    pub can_resume: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCard {
    pub device: DeviceKey,
    pub name: String,
    pub os: CardOs,
    pub logo: CardLogo,
    pub is_self: bool,
    pub section: CardSection,
    pub connection: CardConnection,
    pub last_seen_ms: Option<u64>,
    pub status: Option<DeviceCardStatus>,
    pub pause: Option<LocalPauseCard>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitVolumeCard {
    pub receiver_device: DeviceKey,
    pub receiver_name: String,
    pub output_name: Option<String>,
    pub volume_percent: Option<u8>,
    pub muted: Option<bool>,
    pub condition: ExitVolumeCondition,
    pub version_token: Option<String>,
    pub can_set: bool,
    pub refusal: Option<ExitVolumeRefusal>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmergencyRecipient {
    pub device: DeviceKey,
    pub state: DeliveryState,
    pub cleanup_pending: Option<bool>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmergencyDeliveryCard {
    pub event_token: String,
    pub recipients: Vec<EmergencyRecipient>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCardsView {
    pub schema: u8,
    pub stream: String,
    pub revision: DecimalU64,
    pub group: Option<GroupKey>,
    pub generated_at_ms: u64,
    pub exit_volume: Option<ExitVolumeCard>,
    pub emergency_delivery: Option<EmergencyDeliveryCard>,
    pub cards: Vec<DeviceCard>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum DeviceControlResult {
    Applied,
    NoChange,
    Refused {
        reason: DeviceControlFailure,
        unknown_code: Option<u32>,
    },
    Failed {
        reason: DeviceControlFailure,
        unknown_code: Option<u32>,
    },
}
impl DeviceControlResult {
    pub fn refused(reason: DeviceControlFailure) -> Self {
        Self::Refused {
            reason,
            unknown_code: None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceControlAccepted {
    pub request_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PauseAllAccepted {
    pub request_id: String,
    pub event_token: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceControlCompleted {
    pub request_id: String,
    pub result: DeviceControlResult,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum DeviceControlReply {
    #[serde(rename = "device_control_accepted")]
    Accepted(DeviceControlAccepted),
    #[serde(rename = "device_control_completed")]
    Completed(DeviceControlCompleted),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rtt_uses_exact_boundaries_and_preserves_missing_samples() {
        for (rtt, bars, colour) in [
            (0., 3, "green"),
            (19.999, 3, "green"),
            (20., 2, "yellow"),
            (50., 2, "yellow"),
            (50.001, 1, "red"),
        ] {
            assert_eq!(
                serde_json::to_value(Signal::online(Some(rtt))).unwrap(),
                json!({"kind":"measured","rttMs":rtt,"bars":bars,"colour":colour})
            );
        }
        for rtt in [None, Some(-1.), Some(f64::NAN), Some(f64::INFINITY)] {
            assert_eq!(
                serde_json::to_value(Signal::online(rtt)).unwrap(),
                json!({"kind":"unknown","rttMs":null,"bars":0,"colour":"grey"})
            );
        }
    }

    #[test]
    fn revisions_round_trip_without_javascript_precision_loss() {
        let serialized = serde_json::to_string(&DecimalU64(u64::MAX)).unwrap();
        assert_eq!(serialized, "\"18446744073709551615\"");
        assert_eq!(serde_json::from_str::<DecimalU64>(&serialized).unwrap().0, u64::MAX);
        for invalid in ["1", "\"01\"", "\"-1\"", "\"+1\"", "\"18446744073709551616\""] {
            assert!(serde_json::from_str::<DecimalU64>(invalid).is_err());
        }
    }

    #[test]
    fn forwarding_gain_and_mute_are_independent() {
        for (percent, bars) in [
            (0, 0),
            (1, 1),
            (33, 1),
            (34, 2),
            (66, 2),
            (67, 3),
            (100, 3),
            (101, 4),
            (200, 4),
        ] {
            for muted in [false, true] {
                let value = ForwardingCard::new(percent, muted).unwrap();
                assert_eq!(
                    (value.volume_percent, value.volume_bars, value.muted),
                    (percent, bars, muted)
                );
            }
        }
        assert!(ForwardingCard::new(201, false).is_none());
    }

    #[test]
    fn control_result_payload_matches_the_typescript_contract() {
        let result = DeviceControlResult::Refused {
            reason: DeviceControlFailure::Unrecognized,
            unknown_code: Some(900),
        };
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({"outcome":"refused","reason":"unrecognized","unknownCode":900})
        );
        assert_eq!(
            serde_json::to_value(DeviceControlResult::Applied).unwrap(),
            json!({"outcome":"applied"})
        );
        let result = DeviceControlCompleted {
            request_id: "request".into(),
            result: DeviceControlResult::NoChange,
        };
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({"requestId":"request","result":{"outcome":"no_change"}})
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedDevice {
    pub device: DeviceKey,
    pub name: Option<String>,
    pub os: Option<String>,
    pub blocked_at_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedDevicesSnapshot {
    pub stream: String,
    pub revision: DecimalU64,
    pub entries: Vec<BlockedDevice>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockEdited {
    pub operation_id: String,
    pub changed: bool,
    pub snapshot: BlockedDevicesSnapshot,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockListError {
    /// Open numeric codes preserve unknown future failures.
    pub code: u32,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BlockReply {
    Error(BlockListError),
    Edited(BlockEdited),
}
