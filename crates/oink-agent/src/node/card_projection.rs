//! Assemble immutable cards from authoritative snapshots. Discovery and relay
//! hop observations never stand in for a member's current logical session.
use super::*;
use crate::device_cards::*;
impl Node {
    pub(crate) fn card_inputs(&self) -> CardInputs {
        let focus = self
            .input_status()
            .ok()
            .map(|status| status.group_focus.clone())
            .map_or_else(FocusObservation::default, |focus| FocusObservation {
                participating: focus.participating,
                valid: focus.valid,
                independent: focus.independent,
                device: focus.holder.map(Into::into),
                locked_device: focus.locked_device.map(Into::into),
            });
        let emergency_delivery = lock(&self.inner.controls).emergency_delivery();
        let Ok(config) = self.link_config() else {
            return CardInputs {
                group: None,
                members: vec![CardMember {
                    device: self.id().into(),
                    name: self.device_name(self.id()),
                    os: std::env::consts::OS.into(),
                    is_self: true,
                    ready: false,
                    offline_reason: OfflineKind::Disconnected,
                    last_seen_ms: None,
                    rtt_ms: None,
                    live_boot: Some(self.inner.boot.nonce),
                    facts: None,
                    capabilities: vec![],
                }],
                focus,
                exit_baseline: false,
                selected_exit: None,
                exit_claim: None,
                emergency_delivery,
            };
        };
        let state = config.applied();
        let links = self.links().ok().map(|links| links.links());
        let members = config
            .members()
            .members
            .iter()
            .map(|member| {
                let link = links.as_ref().and_then(|links| links.by_peer(member.device));
                let ready =
                    member.is_self || link.is_some_and(|link| link.state == oink_network::link::LinkState::Ready);
                let live_boot = if member.is_self {
                    Some(self.inner.boot.nonce)
                } else {
                    config.peer_boot(member.device).map(|boot| boot.nonce)
                };
                let facts = if member.is_self {
                    Some(config.local_facts())
                } else {
                    member.facts.clone()
                };
                let capabilities = if member.is_self {
                    facts
                        .as_ref()
                        .and_then(|facts| facts.capabilities().ok())
                        .and_then(|caps| caps.supported().ok())
                        .map(|caps| caps.0.into_iter().map(|cap| cap.0).collect())
                        .unwrap_or_default()
                } else {
                    config
                        .peer_agreement(member.device)
                        .map(|agreement| agreement.enabled().map(str::to_owned).collect())
                        .unwrap_or_default()
                };
                CardMember {
                    device: member.device.into(),
                    name: member.name.clone(),
                    os: member.os.clone(),
                    is_self: member.is_self,
                    ready,
                    offline_reason: if member.protocol_incompatible {
                        OfflineKind::ProtocolIncompatible
                    } else if member.address_stale {
                        OfflineKind::AddressStale
                    } else if link.is_some() {
                        OfflineKind::Connecting
                    } else {
                        OfflineKind::Disconnected
                    },
                    last_seen_ms: member.last_connected_ms,
                    rtt_ms: link.and_then(|link| link.rtt).map(|rtt| rtt.as_secs_f64() * 1000.),
                    live_boot,
                    facts,
                    capabilities,
                }
            })
            .collect();
        let resolution = self.inner.audio.get().map(|audio| audio.status().resolution);
        let (exit_baseline, selected_exit, exit_claim) = match resolution {
            Some(oink_audio::claims::Resolution::None { .. }) => (true, None, None),
            Some(oink_audio::claims::Resolution::Exit { stamp, .. }) => (
                true,
                Some(stamp.owner.into()),
                Some((stamp.counter, stamp.owner.into())),
            ),
            _ => (false, None, None),
        };
        CardInputs {
            group: Some(state.group.into()),
            members,
            focus,
            exit_baseline,
            selected_exit,
            exit_claim,
            emergency_delivery,
        }
    }
    pub(crate) fn refresh_device_cards(&self) {
        let inputs = self.card_inputs();
        let changed = lock(&self.inner.cards).update(&inputs, crate::layout_sync::now_ms());
        if let Some(changed) = changed {
            self.emit(ApiEvent::DeviceCards((*changed).clone()));
        }
    }
    pub(crate) fn device_cards_snapshot(&self) -> api::ApiResult<DeviceCardsView> {
        if lock(&self.inner.cards).snapshot().is_none() {
            self.refresh_device_cards();
        }
        lock(&self.inner.cards)
            .snapshot()
            .map(|snapshot| (*snapshot).clone())
            .ok_or_else(sharing_not_started)
    }
}
