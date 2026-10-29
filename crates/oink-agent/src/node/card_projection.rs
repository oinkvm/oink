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
                microphone_resolution: Default::default(),
                local_exit_selection_ready: false,
                selected_exit: None,
                exit_claim: None,
                emergency_delivery,
            };
        };
        let state = config.applied();
        // A current local option revision does not prove that the owner can
        // order a selection above every member. Use the same durable baseline
        // coverage as the audio command, including records retained while peers
        // are offline; unknown historical resolution alone must not block it.
        let local_exit_selection_ready = {
            let store = lock(&self.inner.config);
            let owner = store.owner();
            let claims = owner.claims(state.group);
            owner.active_group() == Some(state.group)
                && state
                    .members
                    .iter()
                    .all(|member| claims.records.iter().any(|record| record.owner == *member))
        };
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
            microphone_resolution: self
                .inner
                .audio
                .get()
                .map(|audio| audio.status().microphone_resolution)
                .unwrap_or_default(),
            local_exit_selection_ready,
            selected_exit,
            exit_claim,
            emergency_delivery,
        }
    }
    pub(crate) fn refresh_device_cards(&self) {
        self.refresh_device_cards_with(|| self.card_inputs());
    }
    fn refresh_device_cards_with(&self, collect: impl FnOnce() -> CardInputs) {
        // Serialize collection as well as projection: an observer that read old
        // owner facts must not publish after a newer command completion and put
        // stale revision fences back into the native menu or device cards.
        let changed = {
            let mut cards = lock(&self.inner.cards);
            let inputs = collect();
            cards.update(&inputs, crate::layout_sync::now_ms())
        };
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

    pub(crate) fn device_card_signals(&self, sampled_at_ms: u64) -> Option<DeviceCardSignals> {
        let cards = lock(&self.inner.cards).snapshot()?;
        let links = self.links().ok().map(|links| links.links());
        Some(DeviceCardSignals {
            stream: cards.stream.clone(),
            revision: cards.revision,
            sampled_at_ms,
            signals: cards
                .cards
                .iter()
                .filter(|card| matches!(card.connection, CardConnection::Online { .. }))
                .map(|card| DeviceCardSignal {
                    device: card.device,
                    rtt_ms: links
                        .as_ref()
                        .and_then(|table| table.by_peer(card.device.0))
                        .filter(|link| link.state == oink_network::link::LinkState::Ready)
                        .and_then(|link| link.rtt)
                        .map(|rtt| rtt.as_secs_f64() * 1000.),
                })
                .collect(),
        })
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::api::AgentApi;
    use crate::snapshot_tests::{Displays, start_with_clip, until};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delayed_card_refresh_cannot_replace_newer_native_menu_tokens() {
        let _watchdog = crate::test_watchdog::Scenario::new(module_path!(), "serialized card publication");
        let dir = tempfile::tempdir().unwrap();
        let node = start_with_clip(
            &oink_test_support::network_config(dir.path().into(), "synthetic", 0),
            &Displays::new("screen"),
            None,
        )
        .await;
        until("native mouse action ready", || {
            node.device_cards()
                .unwrap()
                .cards
                .iter()
                .find(|c| c.is_self)
                .and_then(|c| c.status.as_ref())
                .is_some_and(|s| matches!(s.actions.mouse_disabled, CardAction::Invoke { .. }))
        })
        .await;
        let (read, captured) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let old = node.clone();
        let worker = std::thread::spawn(move || {
            old.refresh_device_cards_with(|| {
                let inputs = old.card_inputs();
                read.send(()).unwrap();
                resume.recv().unwrap();
                inputs
            })
        });
        captured.recv().unwrap();
        // Collection must already own the projection lock. With the old split
        // scope, a command completion could publish here and then be overwritten
        // when this earlier collector resumed (including its stale action token).
        let collection_serialized = node.inner.cards.try_lock().is_err();
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            collection_serialized,
            "another publication can overtake an in-progress card snapshot"
        );

        // Exercise both revision transitions through the real native dispatcher.
        // Consume owner completion explicitly, so a refused asynchronous command
        // reports its reason instead of becoming a generic menu timeout.
        let (completed, mut received) = tokio::sync::mpsc::unbounded_channel();
        let subscription = node.subscribe(Arc::new(move |event| {
            if let api::ApiEvent::DeviceControlCompleted(done) = event {
                completed.send(done.result.clone()).unwrap();
            }
        }));
        for (id, expected) in [
            ("mouse-disabled", SharingMode::Disabled),
            ("mouse-shared", SharingMode::Shared),
        ] {
            let n = node.clone();
            tokio::task::spawn_blocking(move || crate::native_controls::dispatch(&n, id))
                .await
                .unwrap()
                .unwrap();
            let result = received.recv().await.unwrap();
            assert!(
                matches!(result, DeviceControlResult::Applied | DeviceControlResult::NoChange),
                "{id}: {result:?}"
            );
            let cards = node.device_cards().unwrap();
            let status = cards.cards.iter().find(|c| c.is_self).unwrap().status.as_ref().unwrap();
            assert_eq!(status.mouse.configured_mode, Some(expected));
        }
        node.unsubscribe(subscription);
        node.shutdown().await;
    }
}
