//! Group-member C0 commands. Pending replies belong to the authenticated control
//! session so a delayed result from another link cannot complete an edit.

use super::*;
use oink_protocol::v2::{Message, registry::Transport, schema::WireDto};
use std::time::Duration;
use tokio::sync::oneshot;

type Failure = dto::ControlFailure;

const MAX_PENDING: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceControlCompletion {
    pub outcome: dto::ControlOutcome,
    pub reason: Failure,
}

#[derive(Debug)]
pub(super) struct Pending {
    request: Message,
    result: oneshot::Sender<Result<DeviceControlCompletion, Failure>>,
}

fn field<T>(value: v2::Result<T>) -> Result<T, Failure> {
    value.map_err(|_| Failure::InvalidValue)
}

fn request_header(request: &Message) -> Result<(dto::Id16, dto::Boot), Failure> {
    match request {
        Message::ConfigSetLocalOption(value) => Ok((field(value.request_id())?, field(value.target())?)),
        Message::ConfigPauseDevice(value) => Ok((field(value.request_id())?, field(value.target())?)),
        Message::ConfigSetExitVolume(value) => {
            Ok((field(value.request_id())?, field(field(value.expected())?.exit())?))
        }
        _ => Err(Failure::Unsupported),
    }
}

/// Receive-side registry admission of an inbound device-control frame.
fn admit(session: &ControlSession, direct: bool, frame: OpaqueFrame<'_>) -> Result<(), Failure> {
    let agreement = &session.peer.as_ref().ok_or(Failure::Offline)?.agreement;
    v2::admission::Context {
        kind: 2,
        transport: Transport::Reliable,
        phase: v2::admission::Phase::Member,
        agreement,
        direct,
        old_member_lane: false,
    }
    .admit_frame(frame)
    .map_err(|_| Failure::Unsupported)
}

impl MobileNetwork {
    /// Submit a fenced owner edit and return its typed completion. A missing
    /// reply after the frame was queued leaves the mutation outcome unknown.
    pub async fn device_control(&self, request: Message) -> Result<DeviceControlCompletion, Failure> {
        let (id, target) = request_header(&request)?;
        let target = field(oink_network::wire_identity::read_boot(&target))?;
        let rx = {
            let links = self.links();
            let link = links
                .by_peer(target.device_id)
                .filter(|link| link.state == oink_network::link::LinkState::Ready)
                .ok_or(Failure::Offline)?;
            if link.peer_boot != target {
                return Err(Failure::StaleBoot);
            }
            let mut controls = self.state.controls.lock().map_err(|_| Failure::ApplyFailed)?;
            let session = controls
                .get_mut(&link.link)
                .filter(|session| session.ready)
                .ok_or(Failure::Offline)?;
            if session.peer_boot != target {
                return Err(Failure::StaleBoot);
            }
            let group = self.state.state.lock().map_err(|_| Failure::ApplyFailed)?;
            if !group.roster().contains(target.device_id) {
                return Err(Failure::NotMember);
            }

            let frame = field(v2::frame::encode(&request, v2::limits::CONTROL_FRAME))?;
            // The link layer's send gate refuses what this C0 did not
            // negotiate; ask it before registering a pending reply.
            if session.tx.admits(&frame).is_err() {
                return Err(Failure::Unsupported);
            }
            session.pending_controls.retain(|_, entry| !entry.result.is_closed());
            if session.pending_controls.len() >= MAX_PENDING || session.pending_controls.contains_key(&id.0) {
                return Err(Failure::Busy);
            }
            let (result, rx) = oneshot::channel();
            session.pending_controls.insert(id.0, Pending { request, result });
            if session.tx.post(frame).is_err() {
                session.pending_controls.remove(&id.0);
                return Err(Failure::LinkLost);
            }
            rx
        };
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .map_err(|_| Failure::OutcomeUnknown)?
            .map_err(|_| Failure::OutcomeUnknown)?
    }

    /// Whether the ready link to `peer` can carry message `type_id` (a
    /// `registry::type_id` constant): the send gate's own registry rule.
    pub fn peer_permits(&self, peer: DeviceId, type_id: u16) -> bool {
        self.links()
            .by_peer(peer)
            .is_some_and(|link| link.state == oink_network::link::LinkState::Ready && link.permits(type_id))
    }
}

pub(super) fn handles(message: &Message) -> bool {
    matches!(
        message,
        Message::ConfigLocalEditResult(_)
            | Message::ConfigPauseDeviceResult(_)
            | Message::ConfigExitVolumeResult(_)
            | Message::ConfigExitVolumeChanged(_)
            | Message::ConfigPauseDevice(_)
            | Message::ControlPowerState(_)
            | Message::NotifyShow(_)
            | Message::NotifyAction(_)
            | Message::NotifyDismiss(_)
    )
}

/// A management client does not own the desktop-only options it can edit on a peer.
pub(super) fn refuse_owner_request(session: &ControlSession, message: Message) -> Result<(), MobileNetworkError> {
    let (id, target) =
        request_header(&message).map_err(|_| MobileNetworkError::Protocol("unexpected C0 message".into()))?;
    let mut raw: v2::RawMap = [
        (1, id.write()),
        (2, target.write()),
        (3, dto::ControlOutcome::Refused.write()),
        (4, Failure::Unsupported.write()),
    ]
    .into();
    let response = match message {
        Message::ConfigSetLocalOption(_) => {
            raw.insert(7, false.write());
            Message::ConfigLocalEditResult(dto::LocalEditResult::from_raw(raw).map_err(protocol_start)?)
        }
        Message::ConfigPauseDevice(_) => {
            Message::ConfigPauseDeviceResult(dto::PauseDeviceResult::from_raw(raw).map_err(protocol_start)?)
        }
        Message::ConfigSetExitVolume(_) => {
            Message::ConfigExitVolumeResult(dto::ExitVolumeResult::from_raw(raw).map_err(protocol_start)?)
        }
        _ => return Err(MobileNetworkError::Protocol("unexpected C0 message".into())),
    };
    let frame = v2::frame::encode(&response, v2::limits::CONTROL_FRAME).map_err(protocol_start)?;
    session
        .tx
        .post(frame)
        .map_err(|error| MobileNetworkError::Stream(error.to_string()))
}

pub(super) fn receive_unsupported(
    session: &ControlSession,
    frame: OpaqueFrame<'_>,
    message: Message,
) -> Result<(), MobileNetworkError> {
    admit(session, true, frame).map_err(|_| MobileNetworkError::Protocol("unnegotiated device control".into()))?;
    refuse_owner_request(session, message)
}

pub(super) fn receive(
    session: &mut ControlSession,
    frame: OpaqueFrame<'_>,
    direct: bool,
    message: Message,
) -> Result<(), MobileNetworkError> {
    admit(session, direct, frame).map_err(|_| MobileNetworkError::Protocol("unnegotiated device control".into()))?;
    let id = match &message {
        Message::ConfigLocalEditResult(value) => value.request_id(),
        Message::ConfigPauseDeviceResult(value) => value.request_id(),
        Message::ConfigExitVolumeResult(value) => value.request_id(),
        Message::ConfigPauseDevice(_) => return refuse_owner_request(session, message),
        _ => return Ok(()),
    }
    .map_err(protocol_start)?;
    if let Some(pending) = session.pending_controls.remove(&id.0) {
        let _ = pending.result.send(validate(&pending.request, &message));
    }
    Ok(())
}

fn success(outcome: dto::ControlOutcome, reason: Failure) -> Result<bool, Failure> {
    let applied = match outcome {
        dto::ControlOutcome::Applied | dto::ControlOutcome::NoChange => true,
        dto::ControlOutcome::Refused | dto::ControlOutcome::Failed => false,
        _ => return Err(Failure::StateConflict),
    };
    if applied != (reason == Failure::None) {
        return Err(Failure::StateConflict);
    }
    Ok(applied)
}

fn check_revision(outcome: dto::ControlOutcome, expected: u64, current: u64) -> Result<(), Failure> {
    let valid = if outcome == dto::ControlOutcome::Applied {
        current > expected
    } else {
        current == expected
    };
    valid.then_some(()).ok_or(Failure::StateConflict)
}

fn validate(request: &Message, response: &Message) -> Result<DeviceControlCompletion, Failure> {
    let (outcome, reason) = match (request, response) {
        (Message::ConfigSetLocalOption(request), Message::ConfigLocalEditResult(reply)) => {
            if field(request.request_id())? != field(reply.request_id())?
                || field(request.target())? != field(reply.target())?
            {
                return Err(Failure::StateConflict);
            }
            let outcome = field(reply.outcome())?;
            let reason = field(reply.reason())?;
            if success(outcome, reason)? {
                let current = field(reply.current())?.ok_or(Failure::StateConflict)?;
                if field(current.option())? != field(request.option())? {
                    return Err(Failure::StateConflict);
                }
                if !field(reply.persisted())? || field(reply.fact_sequence())?.is_none_or(|seq| seq == 0) {
                    return Err(Failure::StateConflict);
                }
                check_revision(outcome, field(request.expected_revision())?, field(current.revision())?)?;
            }
            (outcome, reason)
        }
        (Message::ConfigPauseDevice(request), Message::ConfigPauseDeviceResult(reply)) => {
            if field(request.request_id())? != field(reply.request_id())?
                || field(request.target())? != field(reply.target())?
            {
                return Err(Failure::StateConflict);
            }
            let outcome = field(reply.outcome())?;
            let reason = field(reply.reason())?;
            if success(outcome, reason)? {
                let current = field(reply.current())?.ok_or(Failure::StateConflict)?;
                if field(current.paused())? != field(request.paused())?
                    || field(reply.fact_sequence())?.is_none_or(|seq| seq == 0)
                {
                    return Err(Failure::StateConflict);
                }
                check_revision(outcome, field(request.expected_revision())?, field(current.revision())?)?;
            }
            (outcome, reason)
        }
        (Message::ConfigSetExitVolume(request), Message::ConfigExitVolumeResult(reply)) => {
            let expected = field(request.expected())?;
            if field(request.request_id())? != field(reply.request_id())?
                || field(expected.exit())? != field(reply.exit())?
            {
                return Err(Failure::StateConflict);
            }
            let outcome = field(reply.outcome())?;
            let reason = field(reply.reason())?;
            if success(outcome, reason)? {
                let current = field(reply.current())?.ok_or(Failure::StateConflict)?;
                let version = field(current.version())?;
                if field(version.exit())? != field(expected.exit())?
                    || field(version.route_version())? != field(expected.route_version())?
                    || field(version.output_id())? != field(expected.output_id())?
                    || field(version.output_generation())? != field(expected.output_generation())?
                    || field(version.revision())? < field(expected.revision())?
                    || field(current.status())? != dto::ExitVolumeStatus::Ready
                    || field(current.volume_percent())?.is_none()
                    || field(current.muted())?.is_none()
                {
                    return Err(Failure::StateConflict);
                }
            }
            (outcome, reason)
        }
        _ => return Err(Failure::StateConflict),
    };
    Ok(DeviceControlCompletion { outcome, reason })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_protocol::v2::schema::FixedBytes;

    #[test]
    fn owner_gain_completion_needs_matching_boot_and_durable_published_revision() {
        let owner = oink_network::wire_identity::boot(Boot {
            device_id: DeviceId([1; 32]),
            nonce: [2; 16],
        });
        let stale_owner = oink_network::wire_identity::boot(Boot {
            device_id: DeviceId([1; 32]),
            nonce: [9; 16],
        });
        let id = FixedBytes([3; 16]);
        let option = dto::LocalOption::MicrophoneControls;
        let gain = RawValue::Map([(1, 130u16.write()), (2, false.write())].into());
        let request = Message::ConfigSetLocalOption(
            dto::SetLocalOption::from_raw(
                [
                    (1, id.write()),
                    (2, owner.write()),
                    (3, option.write()),
                    (4, gain.clone()),
                    (5, 5u64.write()),
                ]
                .into(),
            )
            .unwrap(),
        );
        let result = |boot: &dto::Boot, revision: u64, published, persisted| {
            let state =
                dto::LocalOptionState::from_raw([(1, option.write()), (2, gain.clone()), (3, revision.write())].into())
                    .unwrap();
            Message::ConfigLocalEditResult(
                dto::LocalEditResult::from_raw(
                    [
                        (1, id.write()),
                        (2, boot.write()),
                        (3, dto::ControlOutcome::Applied.write()),
                        (4, Failure::None.write()),
                        (5, state.write()),
                        (6, published),
                        (7, persisted),
                    ]
                    .into(),
                )
                .unwrap(),
            )
        };
        assert_eq!(
            validate(&request, &result(&owner, 6, 21u64.write(), true.write())),
            Ok(DeviceControlCompletion {
                outcome: dto::ControlOutcome::Applied,
                reason: Failure::None,
            }),
        );
        for invalid in [
            result(&stale_owner, 6, 21u64.write(), true.write()),
            result(&owner, 5, 21u64.write(), true.write()),
            result(&owner, 6, RawValue::Null, true.write()),
            result(&owner, 6, 21u64.write(), false.write()),
        ] {
            assert_eq!(validate(&request, &invalid), Err(Failure::StateConflict));
        }
    }
}
