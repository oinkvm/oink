//! Session-scoped control execution. Admission and terminal caches are bounded;
//! durable/native work runs on ordinary blocking workers under the shared owner
//! gate. Audio delegates acquire that gate inside their own native owner.
use super::{
    completion,
    pause::PauseOwner,
    requests,
    transactions::{Admission, DEADLINE, RequestKey, Requester, Transactions},
};
use crate::{
    api::{ApiEvent, DeviceKey},
    device_cards::*,
    node::{Node, lock},
};
use oink_protocol::v1::{
    Message, dto,
    schema::{FixedBytes, WireDto, WireValue},
};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};
type Failure = dto::ControlFailure;

pub(crate) struct Pending {
    pub id: String,
    pub binding: TokenBinding,
    pub request: Message,
    pub body: Vec<u8>,
    pub deadline: Instant,
}
struct Finished {
    id: [u8; 16],
    binding: TokenBinding,
    body: Vec<u8>,
    result: DeviceControlCompleted,
}
#[derive(Default)]
pub(crate) struct State {
    pub pause: PauseOwner,
    pub(super) emergencies: super::emergency::EmergencyEvents,
    pub(super) local_emergency_sequence: u64,
    pub(super) backlog_sequence: u64,
    pub(super) observed_emergency_sequence: u64,
    pub(super) safety_pending: bool,
    pub(super) disable_safety_pending: bool,
    pub(super) emergency_worker: bool,
    group: Option<oink_foundation::GroupId>,
    owner_requests: Transactions<Message>,
    pending: BTreeMap<[u8; 16], Pending>,
    finished: VecDeque<Finished>,
}
impl State {
    pub(crate) fn safety_pending(&self) -> bool {
        self.safety_pending || self.disable_safety_pending
    }
    pub(crate) fn emergency_delivery(&self) -> Option<EmergencyDeliveryCard> {
        self.emergencies.latest()
    }
    fn repeated_local_pause(&self, id: &str, device: DeviceKey, paused: bool) -> Option<DeviceControlReply> {
        let key = crate::device_cards::request_id(id).ok()?;
        let agrees = |binding: &TokenBinding| {
            binding.owner.device == device && binding.operation == (TokenOperation::Pause { paused })
        };
        if let Some(old) = self.pending.get(&key) {
            return Some(if agrees(&old.binding) {
                accepted(id)
            } else {
                failed(id, Failure::StateConflict)
            });
        }
        self.finished.iter().find(|old| old.id == key).map(|old| {
            if agrees(&old.binding) {
                DeviceControlReply::Completed(old.result.clone())
            } else {
                failed(id, Failure::StateConflict)
            }
        })
    }
    fn admit(&mut self, id: &str, binding: TokenBinding, request: Message) -> Result<DeviceControlReply, Failure> {
        let key = crate::device_cards::request_id(id).map_err(|_| Failure::InvalidValue)?;
        let body = requests::request_body(&request).map_err(|_| Failure::InvalidValue)?;
        if let Some(old) = self.finished.iter().find(|old| old.id == key) {
            return if old.body == body {
                Ok(DeviceControlReply::Completed(old.result.clone()))
            } else {
                Err(Failure::StateConflict)
            };
        }
        if let Some(old) = self.pending.get(&key) {
            return if old.body == body {
                Ok(accepted(id))
            } else {
                Err(Failure::StateConflict)
            };
        }
        if self
            .pending
            .values()
            .filter(|p| p.binding.owner.device == binding.owner.device)
            .count()
            >= 4
        {
            return Err(Failure::Busy);
        }
        self.pending.insert(
            key,
            Pending {
                id: id.into(),
                binding,
                request,
                body,
                deadline: Instant::now() + DEADLINE,
            },
        );
        Ok(accepted(id))
    }
    fn finish(&mut self, id: &str, result: DeviceControlResult) -> Option<DeviceControlCompleted> {
        let key = crate::device_cards::request_id(id).ok()?;
        let pending = self.pending.remove(&key)?;
        let result = DeviceControlCompleted {
            request_id: pending.id,
            result,
        };
        if self.finished.len() == 256 {
            self.finished.pop_front();
        }
        self.finished.push_back(Finished {
            id: key,
            binding: pending.binding,
            body: pending.body,
            result: result.clone(),
        });
        Some(result)
    }
}
fn accepted(id: &str) -> DeviceControlReply {
    DeviceControlReply::Accepted(DeviceControlAccepted { request_id: id.into() })
}
fn failed(id: &str, reason: Failure) -> DeviceControlReply {
    DeviceControlReply::Completed(DeviceControlCompleted {
        request_id: id.into(),
        result: DeviceControlResult::from_owner(dto::ControlOutcome::Refused, reason),
    })
}
fn field<T>(result: oink_protocol::v1::Result<T>) -> Result<T, Failure> {
    result.map_err(|_| Failure::InvalidValue)
}
pub(super) fn boot(requester: Requester) -> dto::Boot {
    requests::wire_boot(OwnerVersion {
        group: oink_foundation::GroupId([0; 16]).into(),
        device: requester.device,
        boot_nonce: requester.nonce,
        revision: 1,
    })
}
pub(crate) fn fresh_request_id() -> String {
    uuid(crate::api::request_context::new_client())
}
fn uuid(id: [u8; 16]) -> String {
    let hex: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}
impl Node {
    pub(crate) fn receive_device_control(&self, peer: DeviceKey, message: Message) {
        match message {
            Message::LayoutArrangementRequest(_) | Message::LayoutArrangementResult(_) => {
                self.receive_arrangement(peer, message)
            }
            Message::ConfigSetLocalOption(_) | Message::ConfigPauseDevice(_) | Message::ConfigSetExitVolume(_) => {
                self.receive_owner_request(peer, message)
            }
            Message::ConfigLocalEditResult(_)
            | Message::ConfigPauseDeviceResult(_)
            | Message::ConfigExitVolumeResult(_) => self.receive_control_response(peer, message),
            Message::ConfigEmergencyPause(request) => self.receive_emergency(peer, request),
            Message::ConfigEmergencyPauseAck(ack) => self.receive_emergency_ack(peer, ack),
            Message::ConfigExitVolumeChanged(_) => self.refresh_device_cards(),
            Message::NotifyShow(_) | Message::NotifyAction(_) | Message::NotifyDismiss(_) => {
                self.receive_notification(peer.0, message)
            }
            _ => {}
        }
    }
    pub(crate) fn reconcile_controls(&self) {
        self.consume_local_emergency();
        let Ok(config) = self.link_config() else { return };
        let applied = config.applied();
        let (retired, changed) = {
            let mut state = lock(&self.inner.controls);
            let group_changed = state
                .group
                .replace(applied.group)
                .is_some_and(|group| group != applied.group);
            let admitted = |requester: Requester| {
                !group_changed
                    && applied.members.contains(&requester.device.0)
                    && if requester.device.0 == self.id() {
                        requester.nonce == self.inner.boot.nonce
                    } else {
                        config
                            .peer_boot(requester.device.0)
                            .is_none_or(|boot| boot.nonce == requester.nonce)
                    }
            };
            state.pause.retain_origins(admitted);
            state.owner_requests.retain_requesters(admitted);
            let mut changed = state.emergencies.expire(Instant::now());
            changed |= state.emergencies.retire(applied.group, |device, nonce| {
                device.0 == self.id() && nonce == self.inner.boot.nonce
                    || config.peer_boot(device.0).is_some_and(|boot| boot.nonce == nonce)
            });
            let retired = state
                .pending
                .values()
                .filter(|pending| self.validate_control_target(&pending.binding).is_err())
                .map(|pending| pending.id.clone())
                .collect::<Vec<_>>();
            (retired, changed)
        };
        for id in retired {
            self.finish_control(
                &id,
                DeviceControlResult::from_owner(dto::ControlOutcome::Failed, Failure::OutcomeUnknown),
            );
        }
        if changed {
            self.refresh_device_cards();
        }
    }
    pub(crate) fn edit_device_option(
        &self,
        device: DeviceKey,
        option: dto::LocalOption,
        value: oink_protocol::v1::RawValue,
    ) -> crate::api::ApiResult<()> {
        self.edit_device_option_with(device, option, |_| Ok(value))
    }
    pub(crate) fn edit_device_option_with(
        &self,
        device: DeviceKey,
        option: dto::LocalOption,
        patch: impl FnOnce(oink_protocol::v1::RawValue) -> crate::api::ApiResult<oink_protocol::v1::RawValue>,
    ) -> crate::api::ApiResult<()> {
        let config = self.link_config()?;
        let nonce = if device.0 == self.id() {
            self.inner.boot.nonce
        } else {
            config
                .peer_boot(device.0)
                .ok_or_else(|| crate::api::ApiError::Unavailable {
                    message: "Device is offline".into(),
                })?
                .nonce
        };
        let facts = if device.0 == self.id() {
            Some((*config.local_facts()).clone())
        } else {
            self.device_facts(device.0)
        };
        let (revision, current) = facts
            .and_then(|facts| {
                let boot = facts.boot().ok()?;
                if oink_network::wire_identity::read_device(&boot.device_id().ok()?).ok()? != device.0
                    || boot.nonce().ok()?.0 != nonce
                {
                    return None;
                }
                let item = facts
                    .preferences()
                    .ok()
                    .flatten()?
                    .0
                    .into_iter()
                    .find(|item| item.option() == Ok(option))?;
                Some((item.revision().ok()?, item.value().ok()?))
            })
            .ok_or_else(|| crate::api::ApiError::Unavailable {
                message: "Current owner preference is unavailable".into(),
            })?;
        // Derive a composite patch from the same owner snapshot as its CAS fence.
        let value = patch(current)?;
        let binding = TokenBinding {
            owner: OwnerVersion {
                group: config.applied().group.into(),
                device,
                boot_nonce: nonce,
                revision,
            },
            operation: TokenOperation::OwnerOption {
                option: option.number(),
            },
        };
        let id = fresh_request_id();
        let request = requests::option_request(&binding, &id, option.number(), value).map_err(|reason| {
            crate::api::ApiError::InvalidArgument {
                message: format!("{reason:?}"),
            }
        })?;
        let reply = self.invoke_control(id.clone(), binding, request);
        let result = match reply {
            DeviceControlReply::Completed(result) => result.result,
            DeviceControlReply::Accepted(_) => {
                let key = crate::device_cards::request_id(&id).expect("generated UUID");
                let deadline = Instant::now() + DEADLINE;
                loop {
                    if let Some(result) = lock(&self.inner.controls)
                        .finished
                        .iter()
                        .find(|result| result.id == key)
                        .map(|result| result.result.result.clone())
                    {
                        break result;
                    }
                    if Instant::now() >= deadline {
                        break DeviceControlResult::from_owner(dto::ControlOutcome::Failed, Failure::OutcomeUnknown);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        };
        match result {
            DeviceControlResult::Applied | DeviceControlResult::NoChange => Ok(()),
            DeviceControlResult::Refused { reason, .. } | DeviceControlResult::Failed { reason, .. } => {
                Err(crate::api::ApiError::InvalidState {
                    message: format!("{reason:?}"),
                })
            }
        }
    }
    pub(crate) fn card_control(
        &self,
        id: String,
        token: String,
        make: impl FnOnce(&TokenBinding, &str) -> Result<Message, DeviceControlFailure>,
    ) -> DeviceControlReply {
        let binding = match lock(&self.inner.cards).tokens().read(&token) {
            Ok(binding) => binding,
            Err(reason) => {
                return DeviceControlReply::Completed(DeviceControlCompleted {
                    request_id: id,
                    result: DeviceControlResult::refused(reason),
                });
            }
        };
        match make(&binding, &id) {
            Ok(request) => self.invoke_control(id, binding, request),
            Err(reason) => DeviceControlReply::Completed(DeviceControlCompleted {
                request_id: id,
                result: DeviceControlResult::refused(reason),
            }),
        }
    }
    pub(crate) fn local_pause_control(&self, id: String, paused: bool) -> DeviceControlReply {
        if let Some(reply) = lock(&self.inner.controls).repeated_local_pause(&id, self.id().into(), paused) {
            return reply;
        }
        self.consume_local_emergency();
        let Ok(config) = self.link_config() else {
            return failed(&id, Failure::Offline);
        };
        let binding = TokenBinding {
            owner: OwnerVersion {
                group: config.applied().group.into(),
                device: self.id().into(),
                boot_nonce: self.inner.boot.nonce,
                revision: lock(&self.inner.controls).pause.revision(),
            },
            operation: TokenOperation::Pause { paused },
        };
        match requests::action_request(&binding, &id) {
            Ok(request) => self.invoke_control(id, binding, request),
            Err(reason) => DeviceControlReply::Completed(DeviceControlCompleted {
                request_id: id,
                result: DeviceControlResult::refused(reason),
            }),
        }
    }
    pub(crate) fn invoke_control(&self, id: String, binding: TokenBinding, request: Message) -> DeviceControlReply {
        if let Err(reason) = self.validate_control_target(&binding) {
            return failed(&id, reason);
        }
        let reply = {
            let mut state = lock(&self.inner.controls);
            let Ok(key) = crate::device_cards::request_id(&id) else {
                return failed(&id, Failure::InvalidValue);
            };
            let is_pending = state.pending.contains_key(&key);
            match state.admit(&id, binding.clone(), request.clone()) {
                Ok(reply) if is_pending || matches!(reply, DeviceControlReply::Completed(_)) => return reply,
                Ok(reply) => reply,
                Err(reason) => return failed(&id, reason),
            }
        };
        let node = self.clone();
        let request_id = id.clone();
        self.inner.rt.spawn(async move {
            if binding.owner.device.0 == node.id() {
                let owner = node.clone();
                let result = tokio::task::spawn_blocking(move || {
                    owner.execute_owner_request(
                        Requester {
                            device: owner.id().into(),
                            nonce: owner.inner.boot.nonce,
                        },
                        binding.owner.group.0,
                        request,
                        false,
                    )
                })
                .await;
                match result {
                    Ok(Ok(response)) => node.receive_control_response(node.id().into(), response),
                    Ok(Err(reason)) => node.finish_control(
                        &request_id,
                        DeviceControlResult::from_owner(dto::ControlOutcome::Failed, reason),
                    ),
                    Err(_) => node.finish_control(
                        &request_id,
                        DeviceControlResult::from_owner(dto::ControlOutcome::Failed, Failure::OutcomeUnknown),
                    ),
                }
            } else if node
                .link_config()
                .and_then(|config| config.send_device_control(binding.owner.device.0, request))
                .is_err()
            {
                node.finish_control(
                    &request_id,
                    DeviceControlResult::from_owner(dto::ControlOutcome::Failed, Failure::OutcomeUnknown),
                );
            }
        });
        let node = self.clone();
        self.inner.rt.spawn(async move {
            tokio::time::sleep(DEADLINE).await;
            let expired = lock(&node.inner.controls)
                .pending
                .get(&crate::device_cards::request_id(&id).expect("admitted UUID"))
                .is_some_and(|p| p.deadline <= Instant::now());
            if expired {
                node.finish_control(
                    &id,
                    DeviceControlResult::from_owner(dto::ControlOutcome::Failed, Failure::OutcomeUnknown),
                );
            }
        });
        reply
    }
    fn finish_control(&self, id: &str, result: DeviceControlResult) {
        let completed = lock(&self.inner.controls).finish(id, result);
        // Rebuild after publication, before completion removes the optimistic UI.
        self.refresh_device_cards();
        if let Some(completed) = completed {
            self.emit(ApiEvent::DeviceControlCompleted(completed));
        }
    }
    pub(crate) fn receive_control_response(&self, peer: DeviceKey, response: Message) {
        let id = match &response {
            Message::ConfigLocalEditResult(result) => result.request_id(),
            Message::ConfigPauseDeviceResult(result) => result.request_id(),
            Message::ConfigExitVolumeResult(result) => result.request_id(),
            _ => return,
        };
        let Ok(id) = id else { return };
        let id = uuid(id.0);
        let result = {
            let state = lock(&self.inner.controls);
            let Some(pending) = state
                .pending
                .get(&crate::device_cards::request_id(&id).expect("wire UUID"))
            else {
                return;
            };
            if pending.binding.owner.device != peer {
                return;
            }
            // Session authority is checked again even for a cryptographically
            // authentic response from a retired owner Boot.
            if self.validate_control_target(&pending.binding).is_err() {
                return;
            }
            completion::validate(&pending.request, &response)
        };
        if let Ok(result) = result {
            self.finish_control(&id, result);
        }
    }
    fn validate_control_target(&self, binding: &TokenBinding) -> Result<(), Failure> {
        let config = self.link_config().map_err(|_| Failure::Offline)?;
        if config.applied().group != binding.owner.group.0
            || !config.applied().members.contains(&binding.owner.device.0)
        {
            return Err(Failure::NotMember);
        }
        let nonce = if binding.owner.device.0 == self.id() {
            self.inner.boot.nonce
        } else {
            config.peer_boot(binding.owner.device.0).ok_or(Failure::Offline)?.nonce
        };
        if nonce != binding.owner.boot_nonce {
            return Err(Failure::StaleBoot);
        }
        if binding.owner.device.0 != self.id() {
            let capability = if matches!(
                binding.operation,
                TokenOperation::Option {
                    edit: crate::device_cards::CardOptionEdit::MouseMode(crate::device_cards::SharingMode::Disabled)
                }
            ) {
                "oink.input.mouse-disabled/1"
            } else if matches!(binding.operation, TokenOperation::ExitVolume { .. }) {
                "oink.audio.exit-volume/1"
            } else {
                "oink.device-options/1"
            };
            if !config
                .peer_agreement(binding.owner.device.0)
                .is_some_and(|agreement| agreement.contains(capability))
            {
                return Err(Failure::Unsupported);
            }
        }
        Ok(())
    }
    pub(crate) fn receive_owner_request(&self, peer: DeviceKey, request: Message) {
        let Ok(config) = self.link_config() else { return };
        let Some(session) = config.peer_boot(peer.0) else {
            return;
        };
        let requester = Requester {
            device: peer,
            nonce: session.nonce,
        };
        let group = config.applied().group;
        match self.admit_owner(requester, group, &request) {
            Ok(Admission::Pending) => return,
            Ok(Admission::Replay(response)) => {
                let _ = config.send_device_control(peer.0, response);
                return;
            }
            Err(reason) => {
                if let Ok(response) = self.refuse_owner(&request, reason) {
                    let _ = config.send_device_control(peer.0, response);
                }
                return;
            }
            Ok(Admission::New) => {}
        }
        let node = self.clone();
        self.inner.rt.spawn_blocking(move || {
            if let Ok(response) = node.execute_owner_request(requester, group, request, true) {
                // A reconnected peer cannot receive a terminal result belonging
                // to its previous requester Boot.
                if config
                    .peer_boot(peer.0)
                    .is_some_and(|live| live.nonce == requester.nonce)
                {
                    let _ = config.send_device_control(peer.0, response);
                }
            }
        });
    }
    fn admit_owner(
        &self,
        requester: Requester,
        group: oink_foundation::GroupId,
        request: &Message,
    ) -> Result<Admission<Message>, Failure> {
        self.owner_authority(requester, group, request)?;
        let (id, _) = request_header(request)?;
        let key = RequestKey { requester, id };
        let body = requests::request_body(request).map_err(|_| Failure::InvalidValue)?;
        lock(&self.inner.controls)
            .owner_requests
            .admit(key, &body, Instant::now())
    }
    pub(super) fn owner_authority(
        &self,
        requester: Requester,
        group: oink_foundation::GroupId,
        request: &Message,
    ) -> Result<(), Failure> {
        self.consume_local_emergency();
        if lock(&self.inner.controls).safety_pending() {
            return Err(Failure::Busy);
        }
        let (_, target) = request_header(request)?;
        let config = self.link_config().map_err(|_| Failure::Offline)?;
        if config.applied().group != group || !config.applied().members.contains(&requester.device.0) {
            return Err(Failure::NotMember);
        }
        if requester.device.0 != self.id() {
            if !config
                .peer_boot(requester.device.0)
                .is_some_and(|live| live.nonce == requester.nonce)
            {
                return Err(Failure::StaleBoot);
            }
            let capability = if matches!(request, Message::ConfigSetLocalOption(edit) if edit.option().ok() == Some(oink_protocol::v1::dto::LocalOption::MouseMode) && (edit.value().ok().is_some_and(|v| v == oink_protocol::v1::RawValue::Uint(2)) || self.settings().routing.mouse_mode == oink_foundation::input::InputMode::Disabled))
            {
                "oink.input.mouse-disabled/1"
            } else if matches!(request, Message::ConfigSetExitVolume(_)) {
                "oink.audio.exit-volume/1"
            } else {
                "oink.device-options/1"
            };
            if !config
                .peer_agreement(requester.device.0)
                .is_some_and(|a| a.contains(capability))
            {
                return Err(Failure::Unsupported);
            }
        }
        if target
            != boot(Requester {
                device: self.id().into(),
                nonce: self.inner.boot.nonce,
            })
        {
            return Err(Failure::StaleBoot);
        }
        Ok(())
    }
    fn execute_owner_request(
        &self,
        requester: Requester,
        group: oink_foundation::GroupId,
        request: Message,
        admitted: bool,
    ) -> Result<Message, Failure> {
        let (id, _) = request_header(&request)?;
        let key = RequestKey { requester, id };
        if !admitted {
            match self.admit_owner(requester, group, &request) {
                Ok(Admission::Replay(result)) => return Ok(result),
                Ok(Admission::Pending) => return Err(Failure::Busy),
                Ok(Admission::New) => {}
                Err(reason) => return self.refuse_owner(&request, reason),
            }
        }
        if let Err(reason) = self.owner_authority(requester, group, &request) {
            let response = self.refuse_owner(&request, reason)?;
            lock(&self.inner.controls)
                .owner_requests
                .complete(key, response.clone());
            return Ok(response);
        }
        let original = request.clone();
        let response = match request {
            Message::ConfigSetLocalOption(request) => self.apply_option_request(requester, group, request),
            Message::ConfigPauseDevice(request) => self.apply_pause_request(requester, group, request),
            Message::ConfigSetExitVolume(request) => match self.inner.audio.get() {
                Some(audio) => self
                    .inner
                    .rt
                    .block_on(audio.set_exit_volume(boot(requester), group, request))
                    .map(Message::ConfigExitVolumeResult),
                None => Err(Failure::Offline),
            },
            _ => unreachable!(),
        };
        let response = match response {
            Ok(response) => response,
            Err(reason) => self.refuse_owner(&original, reason)?,
        };
        lock(&self.inner.controls)
            .owner_requests
            .complete(key, response.clone());
        Ok(response)
    }
    fn refuse_owner(&self, request: &Message, reason: Failure) -> Result<Message, Failure> {
        let (id, _) = request_header(request)?;
        let target = boot(Requester {
            device: self.id().into(),
            nonce: self.inner.boot.nonce,
        });
        let outcome = if matches!(
            reason,
            Failure::PersistenceFailed | Failure::ApplyFailed | Failure::OutcomeUnknown
        ) {
            dto::ControlOutcome::Failed
        } else {
            dto::ControlOutcome::Refused
        };
        let mut raw: oink_protocol::v1::RawMap = [
            (1, FixedBytes(id).write()),
            (2, target.write()),
            (3, outcome.write()),
            (4, reason.write()),
        ]
        .into_iter()
        .collect();
        match request {
            Message::ConfigSetLocalOption(_) => {
                raw.insert(7, false.write());
                dto::LocalEditResult::from_raw(raw)
                    .map(Message::ConfigLocalEditResult)
                    .map_err(|_| Failure::InvalidValue)
            }
            Message::ConfigPauseDevice(_) => dto::PauseDeviceResult::from_raw(raw)
                .map(Message::ConfigPauseDeviceResult)
                .map_err(|_| Failure::InvalidValue),
            Message::ConfigSetExitVolume(_) => dto::ExitVolumeResult::from_raw(raw)
                .map(Message::ConfigExitVolumeResult)
                .map_err(|_| Failure::InvalidValue),
            _ => Err(Failure::InvalidValue),
        }
    }
}

fn request_header(request: &Message) -> Result<([u8; 16], dto::Boot), Failure> {
    Ok(match request {
        Message::ConfigSetLocalOption(request) => (field(request.request_id())?.0, field(request.target())?),
        Message::ConfigPauseDevice(request) => (field(request.request_id())?.0, field(request.target())?),
        Message::ConfigSetExitVolume(request) => (
            field(request.request_id())?.0,
            field(field(request.expected())?.exit())?,
        ),
        _ => return Err(Failure::InvalidValue),
    })
}

#[cfg(all(test, debug_assertions))]
#[path = "runtime_tests.rs"]
mod tests;
