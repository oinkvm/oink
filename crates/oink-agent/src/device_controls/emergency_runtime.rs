//! Emergency transport is a volatile one-shot operation, independent of the
//! ordinary owner queue. Native safety runs before durable keyboard cleanup.
use super::{
    emergency::{Event, Recipient},
    runtime::boot,
    transactions::Requester,
};
use crate::{
    api::{ApiError, ApiEvent, ApiResult, DeviceKey},
    device_cards::*,
    node::{Node, lock},
    threads::input::InputCommand,
};
use oink_protocol::v1::{
    Message, dto,
    schema::{WireDto, WireValue},
};
use std::time::Instant;

#[cfg(all(test, debug_assertions))]
mod tests;

type Failure = dto::ControlFailure;
fn api_error(reason: Failure) -> ApiError {
    ApiError::InvalidState {
        message: format!("emergency owner rejected: {reason:?}"),
    }
}
impl Node {
    pub(crate) fn pause_all_control(&self, request_id: String) -> ApiResult<PauseAllAccepted> {
        let id = crate::device_cards::request_id(&request_id).map_err(|_| api_error(Failure::InvalidValue))?;
        let snapshot = self
            .originate_emergency(Some(id), None)?
            .ok_or_else(|| api_error(Failure::OutcomeUnknown))?;
        Ok(PauseAllAccepted {
            request_id,
            event_token: snapshot.event_token,
        })
    }
    /// A sampled local sequence closes the gap before the coalesced notice is
    /// delivered. Received emergencies never advance this local-only counter.
    pub(crate) fn consume_local_emergency(&self) {
        self.consume_input_backlog();
        let Ok(status) = self.input_status() else { return };
        if status.local_emergency_sequence == 0 {
            return;
        }
        if let Err(error) = self.originate_emergency(None, Some(status.local_emergency_sequence)) {
            tracing::warn!(%error,"local emergency publication failed");
        }
    }
    fn consume_input_backlog(&self) {
        let Ok(status) = self.input_status() else { return };
        if status.backlog_sequence == 0 {
            return;
        }
        {
            let mut state = lock(&self.inner.controls);
            if status.backlog_sequence <= state.backlog_sequence {
                return;
            }
            state.backlog_sequence = status.backlog_sequence;
            state.pause.input_backlog();
        }
        if let Some(clip) = self.clipboard() {
            clip.set_paused(true);
        }
        if let Some(diagnostics) = self.inner.diagnostics.get() {
            diagnostics.recorder.set_suspended(true);
        }
        if let Err(error) = self.publish_owner_preferences() {
            tracing::warn!(?error, "input backlog pause publication failed");
        }
        tracing::error!(
            "input backlog exceeded 5 seconds; all queued input discarded; this device's sharing paused (InputBacklog)"
        );
        self.emit(ApiEvent::Pause(self.pause_status()));
        self.emit_group();
        self.refresh_device_cards();
    }
    fn originate_emergency(
        &self,
        request: Option<[u8; 16]>,
        local_sequence: Option<u64>,
    ) -> ApiResult<Option<EmergencyDeliveryCard>> {
        let config = self.link_config()?;
        let group = config.applied().group;
        let members = config.applied().members.clone();
        let mut recipients: Vec<_> = members
            .into_iter()
            .map(|device| {
                let nonce = if device == self.id() {
                    Some(self.inner.boot.nonce)
                } else {
                    config.peer_boot(device).map(|boot| boot.nonce)
                };
                Recipient {
                    device: device.into(),
                    boot: nonce,
                    state: if nonce.is_some() {
                        DeliveryState::Queued
                    } else {
                        DeliveryState::Unreachable
                    },
                    cleanup_pending: None,
                }
            })
            .collect();
        recipients.sort_by_key(|recipient| recipient.device);
        let origin = Requester {
            device: self.id().into(),
            nonce: self.inner.boot.nonce,
        };
        let (event, snapshot, outcome, start_worker) = {
            let mut state = lock(&self.inner.controls);
            if let Some(request) = request
                && let Some(snapshot) = state.emergencies.request(request)
            {
                return Ok(Some(snapshot));
            }
            if let Some(sequence) = local_sequence {
                if sequence <= state.local_emergency_sequence {
                    return Ok(None);
                }
                state.local_emergency_sequence = sequence;
            }
            let sequence = state.pause.next_event_sequence().map_err(api_error)?;
            let event = dto::EmergencyEventId::from_raw(
                [(1, boot(origin).write()), (2, sequence.write())].into_iter().collect(),
            )
            .map_err(|_| api_error(Failure::InvalidValue))?;
            let outcome = state
                .pause
                .emergency(origin, super::requests::wire_device(origin.device), sequence)
                .map_err(api_error)?;
            state.safety_pending = true;
            let start_worker = !state.emergency_worker;
            state.emergency_worker = true;
            let delivery = Event {
                request,
                group,
                event: event.clone(),
                recipients,
                deadline: Instant::now() + super::transactions::DEADLINE,
            };
            let snapshot = delivery.snapshot();
            state.emergencies.insert(delivery);
            (event, snapshot, outcome, start_worker)
        };
        self.emit_emergency_notice(self.id());
        // This enqueues native safety first; C0 and disk work follow separately.
        self.apply_emergency_safety(group, event.clone(), outcome, self.id().into(), start_worker);
        let node = self.clone();
        let targets = snapshot.recipients.clone();
        self.inner.rt.spawn_blocking(move || {
            for recipient in targets {
                if recipient.device.0 == node.id() || recipient.state != DeliveryState::Queued {
                    continue;
                }
                // Re-read admission before each send; no offline or reconnect queue.
                if config.applied().group != group {
                    break;
                }
                let Some(target) = config.peer_boot(recipient.device.0) else {
                    continue;
                };
                let expected = lock(&node.inner.controls)
                    .emergencies
                    .target_nonce(&event, recipient.device);
                if expected != Some(target.nonce) {
                    continue;
                }
                let request = dto::EmergencyPause::from_raw(
                    [
                        (1, event.write()),
                        (
                            2,
                            boot(Requester {
                                device: recipient.device,
                                nonce: target.nonce,
                            })
                            .write(),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                );
                if let Ok(request) = request {
                    let _ = config.send_device_control(recipient.device.0, Message::ConfigEmergencyPause(request));
                }
            }
        });
        self.refresh_device_cards();
        Ok(Some(snapshot))
    }
    pub(crate) fn receive_emergency(&self, peer: DeviceKey, request: dto::EmergencyPause) {
        let Ok(config) = self.link_config() else { return };
        let Some(session) = config.peer_boot(peer.0) else {
            return;
        };
        let group = config.applied().group;
        if !config.applied().members.contains(&peer.0) {
            return;
        }
        let (Ok(event), Ok(target)) = (request.event(), request.target()) else {
            return;
        };
        let requester = Requester {
            device: peer,
            nonce: session.nonce,
        };
        if event.origin().ok() != Some(boot(requester))
            || target
                != boot(Requester {
                    device: self.id().into(),
                    nonce: self.inner.boot.nonce,
                })
        {
            return;
        }
        let Ok(sequence) = event.sequence() else { return };
        let (outcome, start_worker) = {
            let mut state = lock(&self.inner.controls);
            let Ok(outcome) = state
                .pause
                .emergency(requester, super::requests::wire_device(peer), sequence)
            else {
                return;
            };
            if outcome == dto::EmergencyOutcome::AlreadySeen {
                (outcome, false)
            } else {
                state.safety_pending = true;
                let start = !state.emergency_worker;
                state.emergency_worker = true;
                (outcome, start)
            }
        };
        if outcome != dto::EmergencyOutcome::AlreadySeen {
            self.emit_emergency_notice(peer.0);
        }
        self.apply_emergency_safety(group, event, outcome, peer, start_worker);
    }
    fn emit_emergency_notice(&self, origin: oink_foundation::DeviceId) {
        if let Some(clip) = self.clipboard() {
            clip.set_paused(true);
        }
        if let Some(diagnostics) = self.inner.diagnostics.get() {
            diagnostics.recorder.set_suspended(true);
            let recorder = diagnostics.recorder.clone();
            // The event owner already deduplicated this emergency. Collection
            // runs independently so disk/network waits cannot delay safety ACKs.
            self.inner.rt.spawn_blocking(move || {
                let _ = recorder.save(crate::diagnostics::Reason::Emergency);
            });
        }
        self.emit(ApiEvent::EmergencyStop(crate::api::EmergencyStop {
            device: origin.into(),
            device_name: self.device_name(origin),
            local: origin == self.id(),
            at_ms: crate::layout_sync::now_ms() as i64,
        }));
        self.emit(ApiEvent::Pause(self.pause_status()));
    }
    fn apply_emergency_safety(
        &self,
        group: oink_foundation::GroupId,
        event: dto::EmergencyEventId,
        outcome: dto::EmergencyOutcome,
        recipient: DeviceKey,
        start_worker: bool,
    ) {
        if !start_worker {
            let pending = lock(&self.inner.controls).safety_pending;
            self.send_emergency_ack(group, event, outcome, recipient, pending);
            return;
        }
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        let sent = if let Ok(input) = self.input_thread() {
            input.send(InputCommand::ReceiveEmergency { reply });
            true
        } else {
            false
        };
        let node = self.clone();
        self.inner.rt.spawn_blocking(move || {
            let applied = if sent {
                result
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .ok()
                    .and_then(Result::ok)
            } else {
                None
            };
            let cleanup_pending = applied.as_ref().is_none_or(|applied| applied.cleanup_pending);
            let _gate = node.owner_gate();
            let saved = {
                let mut config = lock(&node.inner.config);
                let item = config.owner().item(dto::LocalOption::KeyboardDisabled);
                item.ok_or(Failure::UnsupportedOption)
                    .and_then(|item| item.revision().map_err(|_| Failure::InvalidValue))
                    .and_then(|revision| config.edit_owner(dto::LocalOption::KeyboardDisabled, revision, false.write()))
            };
            let publication = node.publish_owner_preferences();
            {
                let mut state = lock(&node.inner.controls);
                if let Some(applied) = applied {
                    state.observed_emergency_sequence = applied.emergency_sequence;
                }
                state.safety_pending = saved.is_err();
                state.emergency_worker = false;
            }
            drop(_gate);
            node.emit(ApiEvent::Pause(node.pause_status()));
            node.refresh_device_cards();
            node.send_emergency_ack(
                group,
                event,
                outcome,
                recipient,
                cleanup_pending || saved.is_err() || publication.is_err(),
            );
        });
    }
    fn send_emergency_ack(
        &self,
        group: oink_foundation::GroupId,
        event: dto::EmergencyEventId,
        outcome: dto::EmergencyOutcome,
        recipient: DeviceKey,
        cleanup_pending: bool,
    ) {
        let revision = lock(&self.inner.controls).pause.revision();
        let Ok(ack) = dto::EmergencyPauseAck::from_raw(
            [
                (1, event.write()),
                (
                    2,
                    boot(Requester {
                        device: self.id().into(),
                        nonce: self.inner.boot.nonce,
                    })
                    .write(),
                ),
                (3, outcome.write()),
                (4, revision.write()),
                (5, cleanup_pending.write()),
            ]
            .into_iter()
            .collect(),
        ) else {
            return;
        };
        if recipient.0 == self.id() {
            lock(&self.inner.controls)
                .emergencies
                .acknowledge(group, recipient, self.inner.boot.nonce, &ack);
            self.refresh_device_cards();
        } else if let Ok(config) = self.link_config() {
            if config.applied().group != group
                || event
                    .origin()
                    .ok()
                    .and_then(|boot| boot.nonce().ok())
                    .map(|nonce| nonce.0)
                    != config.peer_boot(recipient.0).map(|boot| boot.nonce)
            {
                return;
            }
            let _ = config.send_device_control(recipient.0, Message::ConfigEmergencyPauseAck(ack));
        }
    }
    pub(crate) fn receive_emergency_ack(&self, peer: DeviceKey, ack: dto::EmergencyPauseAck) {
        let Ok(config) = self.link_config() else { return };
        let Some(session) = config.peer_boot(peer.0) else {
            return;
        };
        if lock(&self.inner.controls)
            .emergencies
            .acknowledge(config.applied().group, peer, session.nonce, &ack)
        {
            self.refresh_device_cards();
        }
    }
}
