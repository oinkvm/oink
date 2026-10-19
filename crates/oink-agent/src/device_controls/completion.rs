//! Validate an owner response against the exact outstanding request before it
//! clears pending UI state. A decodable response alone is not evidence of apply.
use crate::device_cards::DeviceControlResult;
use oink_protocol::v1::{Message, dto};
type Failure = dto::ControlFailure;

fn field<T>(value: oink_protocol::v1::Result<T>) -> Result<T, Failure> {
    value.map_err(|_| Failure::InvalidValue)
}
fn success(outcome: dto::ControlOutcome, reason: Failure) -> Result<bool, Failure> {
    let applied = matches!(outcome, dto::ControlOutcome::Applied | dto::ControlOutcome::NoChange);
    if applied && reason != Failure::None {
        return Err(Failure::StateConflict);
    }
    Ok(applied)
}
fn revision(outcome: dto::ControlOutcome, expected: u64, current: u64) -> Result<(), Failure> {
    let valid = match outcome {
        dto::ControlOutcome::Applied => current > expected,
        dto::ControlOutcome::NoChange => current == expected,
        _ => true,
    };
    valid.then_some(()).ok_or(Failure::StateConflict)
}

pub fn validate(request: &Message, response: &Message) -> Result<DeviceControlResult, Failure> {
    let (outcome, reason) = match (request, response) {
        (Message::ConfigSetLocalOption(request), Message::ConfigLocalEditResult(response)) => {
            if field(request.request_id())? != field(response.request_id())?
                || field(request.target())? != field(response.target())?
            {
                return Err(Failure::StateConflict);
            }
            let outcome = field(response.outcome())?;
            let reason = field(response.reason())?;
            if success(outcome, reason)? {
                let current = field(response.current())?.ok_or(Failure::StateConflict)?;
                if field(current.option())? != field(request.option())?
                    || !field(response.persisted())?
                    || field(response.fact_sequence())?.is_none_or(|sequence| sequence == 0)
                {
                    return Err(Failure::StateConflict);
                }
                revision(outcome, field(request.expected_revision())?, field(current.revision())?)?;
            }
            (outcome, reason)
        }
        (Message::ConfigPauseDevice(request), Message::ConfigPauseDeviceResult(response)) => {
            if field(request.request_id())? != field(response.request_id())?
                || field(request.target())? != field(response.target())?
            {
                return Err(Failure::StateConflict);
            }
            let outcome = field(response.outcome())?;
            let reason = field(response.reason())?;
            if success(outcome, reason)? {
                let current = field(response.current())?.ok_or(Failure::StateConflict)?;
                if field(current.paused())? != field(request.paused())?
                    || field(response.fact_sequence())?.is_none_or(|sequence| sequence == 0)
                {
                    return Err(Failure::StateConflict);
                }
                revision(outcome, field(request.expected_revision())?, field(current.revision())?)?;
            }
            (outcome, reason)
        }
        (Message::ConfigSetExitVolume(request), Message::ConfigExitVolumeResult(response)) => {
            let expected = field(request.expected())?;
            if field(request.request_id())? != field(response.request_id())?
                || field(expected.exit())? != field(response.exit())?
            {
                return Err(Failure::StateConflict);
            }
            let outcome = field(response.outcome())?;
            let reason = field(response.reason())?;
            if success(outcome, reason)? {
                let current = field(response.current())?.ok_or(Failure::StateConflict)?;
                let version = field(current.version())?;
                if field(version.exit())? != field(expected.exit())?
                    || field(version.exit_claim())? != field(expected.exit_claim())?
                    || field(version.output_id())? != field(expected.output_id())?
                    || field(version.output_generation())? != field(expected.output_generation())?
                    || field(current.status())? != dto::ExitVolumeStatus::Ready
                {
                    return Err(Failure::StateConflict);
                }
                // Native outputs may quantize a requested percent. Their actual
                // readback is authoritative and must be present, not fabricated.
                if field(current.volume_percent())?.is_none() || field(current.muted())?.is_none() {
                    return Err(Failure::StateConflict);
                }
                // A successful native write can quantize back to the previous
                // reading. It need not create a new observation revision.
                if field(version.revision())? < field(expected.revision())? {
                    return Err(Failure::StateConflict);
                }
            }
            (outcome, reason)
        }
        _ => return Err(Failure::StateConflict),
    };
    Ok(DeviceControlResult::from_owner(outcome, reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{device_cards::*, device_controls::requests};
    use oink_protocol::v1::{
        RawValue,
        schema::{WireDto, WireValue},
    };
    fn pause_request() -> Message {
        requests::action_request(
            &TokenBinding {
                owner: OwnerVersion {
                    group: oink_foundation::GroupId([1; 16]).into(),
                    device: oink_foundation::DeviceId([2; 32]).into(),
                    boot_nonce: [3; 16],
                    revision: 1,
                },
                operation: TokenOperation::Pause { paused: true },
            },
            "00112233-4455-6677-8899-aabbccddeeff",
        )
        .unwrap()
    }
    #[test]
    fn accepted_pause_requires_matching_applied_state_and_publication() {
        let request = pause_request();
        let Message::ConfigPauseDevice(command) = &request else {
            unreachable!()
        };
        let mut pause = super::super::pause::PauseOwner::default();
        pause.command(1, true, true).unwrap();
        let raw = [
            (1, command.request_id().unwrap().write()),
            (2, command.target().unwrap().write()),
            (3, dto::ControlOutcome::Applied.write()),
            (4, Failure::None.write()),
            (5, pause.state().write()),
            (6, 9u64.write()),
        ]
        .into_iter()
        .collect();
        let response = dto::PauseDeviceResult::from_raw(raw).unwrap();
        assert_eq!(
            validate(&request, &Message::ConfigPauseDeviceResult(response.clone())),
            Ok(DeviceControlResult::Applied)
        );
        let mut missing = response.raw().clone();
        missing.remove(&6);
        assert_eq!(
            validate(
                &request,
                &Message::ConfigPauseDeviceResult(dto::PauseDeviceResult::from_raw(missing).unwrap())
            ),
            Err(Failure::StateConflict)
        );
        let mut wrong = response.raw().clone();
        wrong.insert(1, RawValue::Bytes(vec![9; 16]));
        assert_eq!(
            validate(
                &request,
                &Message::ConfigPauseDeviceResult(dto::PauseDeviceResult::from_raw(wrong).unwrap())
            ),
            Err(Failure::StateConflict)
        );
        let mut unchanged = response.raw().clone();
        unchanged.insert(3, dto::ControlOutcome::NoChange.write());
        assert_eq!(
            validate(
                &request,
                &Message::ConfigPauseDeviceResult(dto::PauseDeviceResult::from_raw(unchanged).unwrap())
            ),
            Err(Failure::StateConflict)
        );
    }
}
