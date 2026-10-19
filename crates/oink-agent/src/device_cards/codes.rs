//! Preserve unknown numeric wire codes without classifying them as healthy.
use super::*;
use oink_protocol::v1::{self, dto};

macro_rules! map_code {
    ($value:expr, $source:ident, $target:ident, [$($variant:ident),*]) => {
        match $value {
            $(dto::$source::$variant => ($target::$variant, None),)*
            dto::$source::Unknown(code) => ($target::Unrecognized, Some(code)),
        }
    };
}

impl TryFrom<&dto::UserFacingProblem> for UserFacingProblem {
    type Error = v1::Error;
    fn try_from(value: &dto::UserFacingProblem) -> Result<Self, Self::Error> {
        let (code, unknown_code) = map_code!(
            value.code()?,
            UserFacingCode,
            UserFacingCode,
            [
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
                LocalServiceUnavailable
            ]
        );
        Ok(Self {
            code,
            unknown_code,
            native_code: value.native_code()?,
            retry_after_ms: value.retry_after_ms()?,
        })
    }
}
impl TryFrom<&dto::ResourceFault> for ResourceFault {
    type Error = v1::Error;
    fn try_from(value: &dto::ResourceFault) -> Result<Self, Self::Error> {
        let (code, unknown_code) = map_code!(
            value.code()?,
            ResourceFaultCode,
            ResourceFaultCode,
            [
                None,
                PermissionRequired,
                PermissionDenied,
                CaptureUnavailable,
                InjectionUnavailable,
                SecureInput,
                DeviceUnavailable,
                CleanupFailed,
                Unsupported,
                BackendFailed
            ]
        );
        Ok(Self {
            code,
            unknown_code,
            problem: value.problem()?.as_ref().map(UserFacingProblem::try_from).transpose()?,
        })
    }
}
impl DeviceControlResult {
    /// A future outcome or a refused result with no reason remains a failure.
    pub fn from_owner(outcome: dto::ControlOutcome, reason: dto::ControlFailure) -> Self {
        let success_reason = reason == dto::ControlFailure::None;
        let (reason, unknown_code) = match reason {
            dto::ControlFailure::None => (DeviceControlFailure::Unrecognized, Some(0)),
            dto::ControlFailure::Offline => (DeviceControlFailure::Offline, None),
            dto::ControlFailure::Unsupported => (DeviceControlFailure::Unsupported, None),
            dto::ControlFailure::StaleBoot => (DeviceControlFailure::StaleBoot, None),
            dto::ControlFailure::StaleRevision => (DeviceControlFailure::StaleRevision, None),
            dto::ControlFailure::NotMember => (DeviceControlFailure::NotMember, None),
            dto::ControlFailure::ResourceFault => (DeviceControlFailure::ResourceFault, None),
            dto::ControlFailure::InvalidValue => (DeviceControlFailure::InvalidValue, None),
            dto::ControlFailure::Busy => (DeviceControlFailure::Busy, None),
            dto::ControlFailure::PersistenceFailed => (DeviceControlFailure::PersistenceFailed, None),
            dto::ControlFailure::ApplyFailed => (DeviceControlFailure::ApplyFailed, None),
            dto::ControlFailure::LinkLost => (DeviceControlFailure::LinkLost, None),
            dto::ControlFailure::OutcomeUnknown => (DeviceControlFailure::OutcomeUnknown, None),
            dto::ControlFailure::NotExit => (DeviceControlFailure::NotExit, None),
            dto::ControlFailure::RouteChanged => (DeviceControlFailure::RouteChanged, None),
            dto::ControlFailure::OutputChanged => (DeviceControlFailure::OutputChanged, None),
            dto::ControlFailure::OutputUnavailable => (DeviceControlFailure::OutputUnavailable, None),
            dto::ControlFailure::VolumeUnsupported => (DeviceControlFailure::VolumeUnsupported, None),
            dto::ControlFailure::PermissionDenied => (DeviceControlFailure::PermissionDenied, None),
            dto::ControlFailure::Superseded => (DeviceControlFailure::Superseded, None),
            dto::ControlFailure::UnsupportedOption => (DeviceControlFailure::UnsupportedOption, None),
            dto::ControlFailure::StateConflict => (DeviceControlFailure::StateConflict, None),
            dto::ControlFailure::Unknown(code) => (DeviceControlFailure::Unrecognized, Some(code)),
        };
        match outcome {
            dto::ControlOutcome::Applied if success_reason => Self::Applied,
            dto::ControlOutcome::NoChange if success_reason => Self::NoChange,
            dto::ControlOutcome::Applied | dto::ControlOutcome::NoChange => Self::Failed { reason, unknown_code },
            dto::ControlOutcome::Refused => Self::Refused { reason, unknown_code },
            dto::ControlOutcome::Failed => Self::Failed { reason, unknown_code },
            dto::ControlOutcome::Unknown(code) => Self::Failed {
                reason: DeviceControlFailure::Unrecognized,
                unknown_code: Some(code),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_protocol::v1::{RawMap, RawValue, schema::WireDto};

    #[test]
    fn explicit_healthy_unknown_and_missing_faults_stay_distinct() {
        for number in [0, 1, 9, 999] {
            let wire = dto::ResourceFault::from_raw(RawMap::from([(1, RawValue::Uint(number))])).unwrap();
            let projected = ResourceFault::try_from(&wire).unwrap();
            assert_eq!(projected.code == ResourceFaultCode::None, number == 0);
            assert_eq!(projected.unknown_code, (number == 999).then_some(999));
            assert_ne!(serde_json::to_value(projected).unwrap(), serde_json::Value::Null);
        }
    }

    #[test]
    fn future_control_outcomes_are_never_success() {
        assert!(matches!(
            DeviceControlResult::from_owner(dto::ControlOutcome::Applied, dto::ControlFailure::ApplyFailed),
            DeviceControlResult::Failed {
                reason: DeviceControlFailure::ApplyFailed,
                ..
            }
        ));
        assert_eq!(
            DeviceControlResult::from_owner(dto::ControlOutcome::Unknown(70), dto::ControlFailure::None),
            DeviceControlResult::Failed {
                reason: DeviceControlFailure::Unrecognized,
                unknown_code: Some(70)
            }
        );
        assert_eq!(
            DeviceControlResult::from_owner(dto::ControlOutcome::Refused, dto::ControlFailure::Unknown(71)),
            DeviceControlResult::Refused {
                reason: DeviceControlFailure::Unrecognized,
                unknown_code: Some(71)
            }
        );
    }
}
