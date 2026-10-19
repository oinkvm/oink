//! Local audio observations and the explicit v1 telemetry projection.
pub use dto::{AudioPhase, UserFacingCode};
use oink_foundation::DeviceId;
use oink_protocol::v1::{
    self, RawMap, RawValue, dto,
    schema::{WireDto, WireValue},
};
use serde::{Deserialize, Serialize};

macro_rules! numeric_json {
    ($module:ident,$ty:ty) => {
        pub(crate) mod $module {
            use serde::{Deserialize, Deserializer, Serializer};
            pub fn serialize<S: Serializer>(value: &$ty, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_u32(value.number())
            }
            pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<$ty, D::Error> {
                let number = u32::deserialize(deserializer)?;
                if number == u32::MAX {
                    return Err(serde::de::Error::custom("reserved enum value"));
                }
                Ok(<$ty>::from_number(number))
            }
        }
    };
}
numeric_json!(phase_json, oink_protocol::v1::dto::AudioPhase);
numeric_json!(problem_json, oink_protocol::v1::dto::UserFacingCode);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    #[serde(with = "problem_json")]
    pub code: UserFacingCode,
    pub native_code: Option<u32>,
    pub retry_after_ms: Option<u32>,
}
impl Problem {
    pub const fn new(code: UserFacingCode) -> Self {
        Self {
            code,
            native_code: None,
            retry_after_ms: None,
        }
    }
    pub fn to_wire(self) -> dto::UserFacingProblem {
        dto::UserFacingProblem::from_raw(
            [
                (1, self.code.write()),
                (2, self.native_code.write()),
                (3, self.retry_after_ms.write()),
            ]
            .into(),
        )
        .expect("typed problem")
    }
    pub fn from_wire(value: dto::UserFacingProblem) -> v1::Result<Self> {
        Ok(Self {
            code: value.code()?,
            native_code: value.native_code()?,
            retry_after_ms: value.retry_after_ms()?,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputLatency {
    pub output: String,
    pub reported_latency_ms: Option<f64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReceiverReport {
    pub source: DeviceId,
    pub received: u64,
    pub concealed: u64,
    pub late: u64,
    pub buffered_ms: f64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturePermission {
    #[default]
    NotRequested,
    Unknown,
    Authorized,
    Denied,
    NotRequired,
}
impl CapturePermission {
    pub fn to_wire(self) -> dto::CapturePermission {
        match self {
            Self::NotRequested => dto::CapturePermission::NotRequested,
            Self::Unknown => dto::CapturePermission::Indeterminate,
            Self::Authorized => dto::CapturePermission::Authorized,
            Self::Denied => dto::CapturePermission::Denied,
            Self::NotRequired => dto::CapturePermission::NotRequired,
        }
    }
    pub fn from_wire(value: dto::CapturePermission) -> Self {
        match value {
            dto::CapturePermission::NotRequested => Self::NotRequested,
            dto::CapturePermission::Authorized => Self::Authorized,
            dto::CapturePermission::Denied => Self::Denied,
            dto::CapturePermission::NotRequired => Self::NotRequired,
            _ => Self::Unknown,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardingOffReason {
    LocalVolumeChanged,
    DefaultOutputChanged,
}
impl ForwardingOffReason {
    pub fn to_wire(self) -> dto::ForwardingOffReason {
        match self {
            Self::LocalVolumeChanged => dto::ForwardingOffReason::LocalVolumeChanged,
            Self::DefaultOutputChanged => dto::ForwardingOffReason::DefaultOutputChanged,
        }
    }
    pub fn from_wire(value: dto::ForwardingOffReason) -> Option<Self> {
        match value {
            dto::ForwardingOffReason::LocalVolumeChanged => Some(Self::LocalVolumeChanged),
            dto::ForwardingOffReason::DefaultOutputChanged => Some(Self::DefaultOutputChanged),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceAudioState {
    #[serde(with = "phase_json")]
    pub phase: AudioPhase,
    pub problem: Option<Problem>,
    pub permission: CapturePermission,
    pub source_muted: bool,
    pub forwarding_enabled: bool,
    pub forwarding_off_reason: Option<ForwardingOffReason>,
    pub forwarding_muted: bool,
    pub forwarding_volume_percent: u16,
    pub mute_release_unconfirmed: bool,
    pub sent: u64,
    pub send_dropped: u64,
    pub outputs: Vec<OutputLatency>,
    pub receivers: Vec<ReceiverReport>,
}
impl DeviceAudioState {
    pub fn to_wire(&self) -> v1::Result<dto::AudioState> {
        let outputs = self
            .outputs
            .iter()
            .map(|output| {
                dto::OutputLatency::from_raw(
                    [
                        (1, RawValue::Text(output.output.clone())),
                        (2, output.reported_latency_ms.write()),
                    ]
                    .into(),
                )
                .map(|output| output.write())
            })
            .collect::<v1::Result<Vec<_>>>()?;
        let receivers = self
            .receivers
            .iter()
            .map(|report| {
                dto::ReceiverReport::from_raw(
                    [
                        (
                            1,
                            dto::DeviceId::from_raw(
                                [(1, dto::ED25519.into()), (2, RawValue::Bytes(report.source.0.to_vec()))].into(),
                            )?
                            .write(),
                        ),
                        (2, report.received.write()),
                        (3, report.concealed.write()),
                        (4, report.late.write()),
                        (5, report.buffered_ms.write()),
                    ]
                    .into(),
                )
                .map(|report| report.write())
            })
            .collect::<v1::Result<Vec<_>>>()?;
        dto::AudioState::from_raw(RawMap::from([
            (3, self.permission.to_wire().write()),
            (4, self.source_muted.write()),
            (5, self.forwarding_enabled.write()),
            (6, self.forwarding_off_reason.map(ForwardingOffReason::to_wire).write()),
            (7, self.forwarding_muted.write()),
            (8, self.forwarding_volume_percent.write()),
            (9, self.mute_release_unconfirmed.write()),
            (10, self.sent.write()),
            (11, self.send_dropped.write()),
            (12, RawValue::Array(outputs)),
            (13, RawValue::Array(receivers)),
            (14, self.phase.write()),
            (15, self.problem.map(Problem::to_wire).write()),
        ]))
    }
    pub fn from_wire(value: dto::AudioState) -> v1::Result<Self> {
        let outputs = value
            .outputs()?
            .0
            .into_iter()
            .map(|output| {
                Ok(OutputLatency {
                    output: output.output()?.0,
                    reported_latency_ms: output.reported_latency_ms()?,
                })
            })
            .collect::<v1::Result<Vec<_>>>()?;
        let receivers = value
            .receivers()?
            .0
            .into_iter()
            .map(|report| {
                let source = report.source()?;
                source.require_supported()?;
                Ok(ReceiverReport {
                    source: DeviceId(
                        source
                            .public_key()?
                            .0
                            .try_into()
                            .map_err(|_| v1::ErrorCode::MalformedFrame)?,
                    ),
                    received: report.received()?,
                    concealed: report.concealed()?,
                    late: report.late()?,
                    buffered_ms: report.buffered_ms()?,
                })
            })
            .collect::<v1::Result<Vec<_>>>()?;
        Ok(Self {
            phase: value.phase()?,
            problem: value.problem()?.map(Problem::from_wire).transpose()?,
            permission: CapturePermission::from_wire(value.permission()?),
            source_muted: value.source_muted()?,
            forwarding_enabled: value.forwarding_enabled()?,
            forwarding_off_reason: value.forwarding_off_reason()?.and_then(ForwardingOffReason::from_wire),
            forwarding_muted: value.forwarding_muted()?,
            forwarding_volume_percent: value.forwarding_volume_percent()?,
            mute_release_unconfirmed: value.mute_release_unconfirmed()?,
            sent: value.sent()?,
            send_dropped: value.send_dropped()?,
            outputs,
            receivers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_state_preserves_open_phase_and_actual_unknown_measurements() {
        let state = DeviceAudioState {
            phase: AudioPhase::WaitingForExit,
            problem: None,
            permission: CapturePermission::Unknown,
            source_muted: false,
            forwarding_enabled: true,
            forwarding_off_reason: None,
            forwarding_muted: false,
            forwarding_volume_percent: 100,
            mute_release_unconfirmed: false,
            sent: 0,
            send_dropped: 0,
            outputs: vec![OutputLatency {
                output: "synthetic-output".into(),
                reported_latency_ms: None,
            }],
            receivers: Vec::new(),
        };
        assert_eq!(DeviceAudioState::from_wire(state.to_wire().unwrap()).unwrap(), state);
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["phase"], 5);
        assert!(json["outputs"][0]["reported_latency_ms"].is_null());
        let mut future = state;
        future.phase = AudioPhase::Unknown(500);
        future.problem = Some(Problem::new(UserFacingCode::Unknown(999)));
        assert_eq!(DeviceAudioState::from_wire(future.to_wire().unwrap()).unwrap(), future);
        assert_eq!(
            serde_json::from_value::<DeviceAudioState>(serde_json::to_value(&future).unwrap()).unwrap(),
            future
        );
    }
}
