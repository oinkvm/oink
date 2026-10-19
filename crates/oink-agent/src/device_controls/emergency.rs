//! Volatile delivery coverage for one-shot emergency events. Retrying a local
//! request never creates a second event, including after recipients resume.
use crate::{
    api::DeviceKey,
    device_cards::{DeliveryState, EmergencyDeliveryCard, EmergencyRecipient},
};
use oink_protocol::v1::{dto, schema::WireDto};
use std::{collections::VecDeque, time::Instant};

pub(crate) struct Recipient {
    pub device: DeviceKey,
    pub boot: Option<[u8; 16]>,
    pub state: DeliveryState,
    pub cleanup_pending: Option<bool>,
}
pub(crate) struct Event {
    pub request: Option<[u8; 16]>,
    pub group: oink_foundation::GroupId,
    pub event: dto::EmergencyEventId,
    pub recipients: Vec<Recipient>,
    pub deadline: Instant,
}
impl Event {
    pub fn snapshot(&self) -> EmergencyDeliveryCard {
        use base64::Engine;
        EmergencyDeliveryCard {
            event_token: base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(self.event.encode(1024).expect("locally constructed event")),
            recipients: self
                .recipients
                .iter()
                .map(|recipient| EmergencyRecipient {
                    device: recipient.device,
                    state: recipient.state,
                    cleanup_pending: recipient.cleanup_pending,
                })
                .collect(),
        }
    }
}
#[derive(Default)]
pub(crate) struct EmergencyEvents {
    events: VecDeque<Event>,
}
impl EmergencyEvents {
    pub fn target_nonce(&self, id: &dto::EmergencyEventId, peer: DeviceKey) -> Option<[u8; 16]> {
        self.events
            .iter()
            .find(|event| event.event == *id)?
            .recipients
            .iter()
            .find(|recipient| recipient.device == peer)?
            .boot
    }
    pub fn latest(&self) -> Option<EmergencyDeliveryCard> {
        self.events.back().map(Event::snapshot)
    }
    pub fn request(&self, id: [u8; 16]) -> Option<EmergencyDeliveryCard> {
        self.events
            .iter()
            .find(|event| event.request == Some(id))
            .map(Event::snapshot)
    }
    pub fn insert(&mut self, event: Event) {
        if self.events.len() == 256 {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }
    pub fn acknowledge(
        &mut self,
        group: oink_foundation::GroupId,
        peer: DeviceKey,
        live_nonce: [u8; 16],
        ack: &dto::EmergencyPauseAck,
    ) -> bool {
        let (Ok(event_id), Ok(target), Ok(cleanup_pending)) = (ack.event(), ack.target(), ack.cleanup_pending()) else {
            return false;
        };
        if target.device_id().ok() != Some(super::requests::wire_device(peer))
            || target.nonce().ok().map(|nonce| nonce.0) != Some(live_nonce)
        {
            return false;
        }
        let Some(event) = self
            .events
            .iter_mut()
            .find(|event| event.group == group && event.event == event_id)
        else {
            return false;
        };
        let Some(recipient) = event
            .recipients
            .iter_mut()
            .find(|recipient| recipient.device == peer && recipient.boot == Some(live_nonce))
        else {
            return false;
        };
        if recipient.state != DeliveryState::Queued {
            return false;
        }
        recipient.state = DeliveryState::Confirmed;
        recipient.cleanup_pending = Some(cleanup_pending);
        true
    }
    pub fn expire(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for event in &mut self.events {
            if event.deadline > now {
                continue;
            }
            for recipient in &mut event.recipients {
                if recipient.state == DeliveryState::Queued {
                    recipient.state = DeliveryState::Unconfirmed;
                    changed = true;
                }
            }
        }
        changed
    }
    pub fn retire(
        &mut self,
        group: oink_foundation::GroupId,
        mut live: impl FnMut(DeviceKey, [u8; 16]) -> bool,
    ) -> bool {
        let mut changed = false;
        for event in &mut self.events {
            for recipient in &mut event.recipients {
                if recipient.state == DeliveryState::Queued
                    && (event.group != group || recipient.boot.is_none_or(|nonce| !live(recipient.device, nonce)))
                {
                    recipient.state = DeliveryState::Unconfirmed;
                    changed = true;
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_protocol::v1::schema::WireValue;
    #[test]
    fn coverage_ack_is_session_scoped_and_request_retry_keeps_event() {
        let peer = DeviceKey(oink_foundation::DeviceId([3; 32]));
        let group = oink_foundation::GroupId([4; 16]);
        let target = super::super::requests::wire_boot(crate::device_cards::OwnerVersion {
            group: group.into(),
            device: peer,
            boot_nonce: [5; 16],
            revision: 1,
        });
        let event =
            dto::EmergencyEventId::from_raw([(1, target.write()), (2, 1u64.write())].into_iter().collect()).unwrap();
        let deadline = Instant::now();
        let mut events = EmergencyEvents::default();
        events.insert(Event {
            request: Some([7; 16]),
            group,
            event: event.clone(),
            deadline,
            recipients: vec![Recipient {
                device: peer,
                boot: Some([5; 16]),
                state: DeliveryState::Queued,
                cleanup_pending: None,
            }],
        });
        let token = events.request([7; 16]).unwrap().event_token;
        let ack = dto::EmergencyPauseAck::from_raw(
            [
                (1, event.write()),
                (2, target.write()),
                (3, dto::EmergencyOutcome::EnteredPause.write()),
                (4, 2u64.write()),
                (5, false.write()),
            ]
            .into_iter()
            .collect(),
        )
        .unwrap();
        assert!(!events.acknowledge(group, peer, [6; 16], &ack));
        assert!(events.acknowledge(group, peer, [5; 16], &ack));
        assert!(!events.acknowledge(group, peer, [5; 16], &ack));
        assert!(!events.expire(deadline));
        assert!(!events.retire(group, |_, _| false));
        assert_eq!(events.request([7; 16]).unwrap().event_token, token);
        assert_eq!(events.latest().unwrap().recipients[0].state, DeliveryState::Confirmed);
    }
}
