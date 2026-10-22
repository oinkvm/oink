//! Serialized audio owner commands, authenticated claims and native output binding.
use super::*;
use dto::{ControlFailure as Failure, LocalOption};

impl Owner {
    pub(super) fn members(&self) -> BTreeSet<DeviceId> {
        self.group
            .members()
            .members
            .iter()
            .map(|member| member.device)
            .collect()
    }
    pub(super) fn current_boot(&self, peer: DeviceId) -> Option<dto::Boot> {
        if peer == self.links.id() {
            Some(boot(self.links.boot()))
        } else if self.members().contains(&peer) {
            self.group.peer_boot(peer).map(boot)
        } else {
            None
        }
    }
    pub(super) fn requester(&self, requester: &dto::Boot) -> Result<DeviceId, Failure> {
        let peer = read_device(&requester.device_id().map_err(|_| Failure::InvalidValue)?)
            .map_err(|_| Failure::Unsupported)?;
        if !self.members().contains(&peer) {
            return Err(Failure::NotMember);
        }
        if self.current_boot(peer).as_ref() != Some(requester) {
            return Err(Failure::StaleBoot);
        }
        Ok(peer)
    }
    pub(super) fn authorize(&self, requester: &dto::Boot, group: GroupId) -> Result<DeviceId, Failure> {
        if self.group.applied().group != group || lock(&self.store).owner().active_group() != Some(group) {
            return Err(Failure::NotMember);
        }
        self.requester(requester)
    }
    pub(super) fn persist_claims(&mut self, next: Claims) -> Result<(), Failure> {
        let group = self.active_group.ok_or(Failure::NotMember)?;
        let me = self.links.id();
        let records = GroupClaims {
            seen_counter: next.seen_counter(),
            watermark: next.watermark().cloned(),
            records: next
                .records()
                .map(|record| ClaimRecord {
                    owner: record.owner,
                    revision: record.revision,
                    claim: record.claim.clone(),
                })
                .collect(),
        };
        let mut candidate = lock(&self.store).owner().clone();
        candidate
            .commit_claims(group, me, records.clone())
            .map_err(|_| Failure::InvalidValue)?;
        self.preflight_preferences(
            candidate.preferences(),
            candidate
                .claims(self.active_group.ok_or(Failure::NotMember)?)
                .watermark(),
        )?;
        lock(&self.store)
            .update_owner(|owner| owner.commit_claims(group, me, records))
            .map_err(|_| Failure::PersistenceFailed)?;
        self.claims = next;
        Ok(())
    }
    fn preflight_preferences(
        &self,
        preferences: Vec<dto::DevicePreference>,
        watermark: Option<dto::AudioExitClaim>,
    ) -> Result<(), Failure> {
        let mut facts = (*self.group.local_facts()).clone();
        let claim = preferences
            .iter()
            .find(|item| item.option() == Ok(LocalOption::AudioExitClaim))
            .and_then(|item| item.value().ok())
            .unwrap_or(RawValue::Null);
        facts
            .edit(|raw| {
                raw.insert(18, RawValue::Array(preferences.iter().map(WireValue::write).collect()));
                raw.insert(20, claim);
                raw.insert(21, watermark.write());
            })
            .map_err(|_| Failure::InvalidValue)?;
        v1::frame::encode(&Message::ConfigFacts(facts), v1::limits::CONFIG_FRAME).map_err(|_| Failure::InvalidValue)?;
        Ok(())
    }
    pub(super) fn publish_preferences(&self) -> Result<(), Failure> {
        let preferences = lock(&self.store).owner().preferences();
        let pause = self
            .group
            .local_facts()
            .pause()
            .map_err(|_| Failure::StateConflict)?
            .ok_or(Failure::Busy)?;
        self.group
            .publish_local_owner(preferences, pause)
            .map_err(|_| Failure::ApplyFailed)?;
        Ok(())
    }
    pub(super) fn apply(&mut self) {
        let gate = self.gate.clone();
        let _guard = lock(&gate);
        if let Err(reason) = self.apply_locked() {
            tracing::warn!(reason = reason.number(), "audio owner reconciliation failed");
            // A persistence or baseline failure never authorizes a guessed exit.
            self.transition(Resolution::Unknown);
        }
    }
    pub(super) fn apply_locked(&mut self) -> Result<(), Failure> {
        let (active_group, saved) = {
            let config = lock(&self.store);
            let group = config.owner().active_group();
            (group, group.map(|id| config.owner().claims(id)))
        };
        if active_group != self.active_group {
            self.transition(Resolution::Unknown);
            self.active_group = active_group;
            self.claims = if let Some(saved) = saved {
                let mut claims = Claims::restore(
                    saved.records.into_iter().map(|record| Record {
                        owner: record.owner,
                        revision: record.revision,
                        claim: record.claim,
                    }),
                    saved.seen_counter,
                )?;
                if let Some(watermark) = saved.watermark {
                    claims.observe_watermark(watermark)?;
                }
                claims
            } else {
                Claims::default()
            };
        }
        self.paused = self.group.applied().paused;
        self.streams.pause_changed(self.paused || self.update_suspended);
        let Some(group) = self.active_group else {
            self.transition(Resolution::Unknown);
            return Ok(());
        };
        let members = self.members();
        let boots: Vec<_> = members.iter().filter_map(|peer| self.current_boot(*peer)).collect();
        self.exit_volume.retain_sources(|source| boots.contains(source));
        let mut available = BTreeSet::new();
        let mut candidate = self.claims.clone();
        let mut changed = false;
        let table = self.group.members();
        for member in &table.members {
            let facts = if member.is_self {
                Some(self.group.local_facts())
            } else {
                member.facts.clone()
            };
            let Some(facts) = facts else { continue };
            if facts.group().ok().map(|id| id.0) != Some(group.0)
                || self.current_boot(member.device).as_ref() != facts.boot().ok().as_ref()
            {
                continue;
            }
            let Some(preference) = facts.preferences().ok().flatten().and_then(|items| {
                items
                    .0
                    .into_iter()
                    .find(|item| item.option() == Ok(LocalOption::AudioExitClaim))
            }) else {
                continue;
            };
            let claim = Option::<dto::AudioExitClaim>::read(&preference.value().map_err(|_| Failure::InvalidValue)?)
                .map_err(|_| Failure::InvalidValue)?;
            if claim != facts.audio_exit_claim().map_err(|_| Failure::InvalidValue)? {
                return Err(Failure::StateConflict);
            }
            changed |= candidate.observe(
                member.device,
                Record {
                    owner: member.device,
                    revision: preference.revision().map_err(|_| Failure::InvalidValue)?,
                    claim,
                },
            )?;
            if let Some(watermark) = facts.audio_claim_watermark().map_err(|_| Failure::InvalidValue)? {
                changed |= candidate.observe_watermark(watermark)?;
            }
            let awake = facts.observed().is_ok_and(|state| state.awake() == Ok(true));
            let unpaused = facts
                .pause()
                .ok()
                .flatten()
                .is_some_and(|pause| pause.paused() == Ok(false));
            if awake && unpaused {
                available.insert(member.device);
            }
        }
        // The local durable record is authoritative even while its publication is
        // queued behind this owner command. Third-party facts cannot supply it.
        let local = lock(&self.store)
            .owner()
            .item(LocalOption::AudioExitClaim)
            .ok_or(Failure::StateConflict)?;
        changed |= candidate.observe(
            self.links.id(),
            Record {
                owner: self.links.id(),
                revision: local.revision().map_err(|_| Failure::InvalidValue)?,
                claim: Option::<dto::AudioExitClaim>::read(&local.value().map_err(|_| Failure::InvalidValue)?)
                    .map_err(|_| Failure::InvalidValue)?,
            },
        )?;
        if !self.paused && !self.power_state.sleeping {
            available.insert(self.links.id());
        }
        let yielded = candidate.yield_to_winner(self.links.id())?;
        if let Some(record) = yielded {
            // Retire all sessions and pinned native keys before publishing yield.
            self.retire_sessions();
            self.retire_output();
            self.publish_observations()?;
            changed |= candidate.observe(self.links.id(), record)?;
        }
        if changed {
            self.persist_claims(candidate)?;
            self.publish_preferences()?;
        }
        let resolution = self.claims.resolve(&members, &available);
        self.transition(resolution);
        if let Some(prepared) = self.source_session.prepared() {
            let output = prepared.session().receiver_output().ok();
            let current = self.resolved_volume().and_then(|state| state.version().ok());
            // C0 facts and M0 preparation have independent delivery order. A
            // newer authenticated preparation can precede its fact publication;
            // only a known newer/equivocating binding retires it here.
            let retired = output.as_ref().zip(current.as_ref()).is_some_and(|(output, version)| {
                output.receiver() != version.exit()
                    || output.generation().ok() < version.output_generation().ok()
                    || (output.generation() == version.output_generation() && output.output_id() != version.output_id())
            });
            if retired {
                if let Some(peer) = self.resolution.owner() {
                    self.streams.forget_preparation(peer);
                }
                self.source_session.retire();
                self.selected = None;
                let _ = self.release_capture();
                self.needs_generation = false;
            }
        }
        let retired: Vec<_> = self
            .receive_sessions
            .iter()
            .filter_map(|(peer, session)| {
                (self.current_boot(*peer).as_ref() != session.offer().source().ok().as_ref()).then_some(*peer)
            })
            .collect();
        for peer in retired {
            self.streams.forget_preparation(peer);
            self.receive_sessions.remove(&peer);
            self.prepared.remove(&peer);
            self.receivers.remove(&peer);
            self.offers.remove(&peer);
        }
        self.refresh_exit_binding(false)?;
        self.refresh_volume_context();
        let peers: Vec<_> = self.links.links().links.iter().map(|link| link.peer).collect();
        for peer in peers {
            self.prepare_peer(peer);
        }
        Ok(())
    }
    pub(super) fn retire_sessions(&mut self) {
        self.remote_problem = None;
        self.streams.retire_sessions();
        self.source_session.retire();
        self.receive_sessions.clear();
        self.offers.clear();
        self.prepared.clear();
        self.selected = None;
        self.receivers.clear();
        self.playback = None;
        self.starting_playback = None;
        self.playback_idle = false;
        self.draining = false;
    }
    pub(super) fn retire_output(&mut self) {
        self.exit_volume.retire();
        self.bound_output = None;
        self.volume.publish(None);
    }
    pub(super) fn transition(&mut self, resolution: Resolution) {
        self.forwarding_pause
            .set_pause_allowed(matches!(resolution, Resolution::None { .. }));
        if resolution == self.resolution {
            return;
        }
        let previous = self.resolution;
        // An unavailable selected exit still suppresses player pause, but releases
        // the capture lease immediately. Only an explicit no-exit barrier pauses.
        let keep_capture = previous.owner().is_some_and(|owner| owner != self.links.id())
            && resolution.owner().is_some_and(|owner| owner != self.links.id())
            && resolution.available()
            && !self.paused
            && !self.power_state.sleeping
            && self.settings.forwarding_enabled
            && self.capture.is_some()
            && !self.provisional;
        self.retire_sessions();
        if previous.stamp() != resolution.stamp() {
            self.retire_output();
        }
        self.resolution = resolution;
        if keep_capture {
            self.exit_switch_deadline
                .get_or_insert(Instant::now() + EXIT_SWITCH_LEASE);
        } else {
            self.stop_source();
        }
        self.source_failed = false;
        self.playback_failed = false;
        self.error = None;
        self.restart_playback = false;
        self.output_invalidated_at = None;
    }
    pub(super) fn install_settings(&mut self, mut settings: Settings) -> Result<(), String> {
        let gate = self.gate.clone();
        let _guard = lock(&gate);
        self.install_settings_locked(&mut settings)
            .map_err(|reason| format!("audio settings failure {}", reason.number()))
    }
    pub(super) fn install_settings_locked(&mut self, settings: &mut Settings) -> Result<(), Failure> {
        self.persist_settings(settings)?;
        self.publish_preferences()?;
        self.publish_observations()?;
        self.publish(self.phase());
        Ok(())
    }
    fn persist_settings(&mut self, settings: &mut Settings) -> Result<(), Failure> {
        self.persist_settings_patch(settings, &serde_json::json!({}))
    }
    fn persist_settings_patch(&mut self, settings: &mut Settings, patch: &serde_json::Value) -> Result<(), Failure> {
        oink_audio::validate_receive_output(settings.receive_output.as_deref()).map_err(|_| Failure::InvalidValue)?;
        oink_audio::forward_volume::validate_percent(settings.forwarding_volume_percent)
            .map_err(|_| Failure::InvalidValue)?;
        if !self.settings.forwarding_enabled && settings.forwarding_enabled {
            settings.forwarding_muted = false;
        }
        if settings.forwarding_enabled {
            settings.forwarding_off_reason = None;
        }
        let preferences = {
            let config = lock(&self.store);
            let mut candidate = config.settings.clone();
            candidate.audio = settings.clone();
            config
                .preview_preferences(&candidate)
                .map_err(|_| Failure::InvalidValue)?
        };
        self.preflight_preferences(preferences, self.claims.watermark().cloned())?;
        lock(&self.store)
            .update_local(&serde_json::json!({"audio":patch}), |config| {
                config.audio = settings.clone()
            })
            .map_err(|_| Failure::PersistenceFailed)?;
        self.apply_settings(settings.clone());
        Ok(())
    }
    pub(super) fn apply_settings(&mut self, settings: Settings) {
        let output_changed = self.settings.receive_output != settings.receive_output;
        let capture_changed = self.settings.capture_scope != settings.capture_scope
            || self.settings.keep_local_playback != settings.keep_local_playback;
        let format_changed = self.settings.preferred_format != settings.preferred_format;
        self.settings = settings;
        self.forwarding_off_reason = self.settings.forwarding_off_reason;
        if !self.settings.forwarding_enabled {
            self.stop_source();
        } else if capture_changed {
            let _ = self.release_capture();
        }
        if output_changed || format_changed {
            self.retire_sessions();
            self.playback_failed = false;
            let _ = self.refresh_exit_binding(true);
        }
        if let Some(encoded) = &self.encoded {
            encoded.set_percent(self.settings.forwarding_volume_percent);
            encoded.set_muted(self.settings.forwarding_muted);
        }
        self.source_failed = false;
        self.error = None;
        self.report_pending = true;
        self.refresh_volume_context();
    }
    pub(super) fn configure(
        &mut self,
        patch: crate::api::PreferencePatch<Settings>,
        reply: SyncSender<Result<(), String>>,
    ) {
        let gate = self.gate.clone();
        let _guard = lock(&gate);
        let result = patch
            .apply(&self.settings)
            .map_err(|error| format!("{error:?}"))
            .and_then(|mut settings| {
                self.persist_settings_patch(&mut settings, &patch.value())
                    .and_then(|()| self.publish_preferences())
                    .and_then(|()| self.publish_observations())
                    .map_err(|reason| format!("audio settings failure {}", reason.number()))?;
                self.publish(self.phase());
                Ok(())
            });
        let _ = reply.send(result);
    }
    #[cfg(test)]
    pub(super) fn set_option(
        &mut self,
        requester: &dto::Boot,
        group: GroupId,
        option: LocalOption,
        expected: u64,
        patch: RawValue,
    ) -> Result<OwnerOptionApplied, Failure> {
        self.set_option_with_persistence(requester, group, option, expected, patch, false)
    }
    pub(super) fn set_option_with_persistence(
        &mut self,
        requester: &dto::Boot,
        group: GroupId,
        option: LocalOption,
        expected: u64,
        patch: RawValue,
        defer_persistence: bool,
    ) -> Result<OwnerOptionApplied, Failure> {
        let gate = self.gate.clone();
        let _guard = lock(&gate);
        self.authorize(requester, group)?;
        if !matches!(option, LocalOption::ForwardingEnabled | LocalOption::ForwardingControls) {
            return Err(Failure::UnsupportedOption);
        }
        let before = lock(&self.store)
            .owner()
            .item(option)
            .ok_or(Failure::UnsupportedOption)?;
        if before.revision() != Ok(expected) {
            return Err(Failure::StaleRevision);
        }
        if option == LocalOption::ForwardingEnabled {
            let mut settings = self.settings.clone();
            settings.forwarding_enabled = bool::read(&patch).map_err(|_| Failure::InvalidValue)?;
            self.persist_settings(&mut settings)?;
        } else {
            let mut candidate = lock(&self.store).owner().clone();
            candidate.edit(option, expected, patch.clone())?;
            self.preflight_preferences(
                candidate.preferences(),
                candidate
                    .claims(self.active_group.ok_or(Failure::NotMember)?)
                    .watermark(),
            )?;
            if defer_persistence {
                lock(&self.store).edit_forwarding_deferred(expected, patch)?;
            } else {
                // Released peers and the final drag position require durability,
                // even when their value is already the current in-memory value.
                lock(&self.store).edit_owner(option, expected, patch)?;
            }
            let settings = lock(&self.store).settings.audio.clone();
            self.apply_settings(settings);
        }
        let published = self.publish_preferences().and_then(|()| self.publish_observations());
        self.publish(self.phase());
        Ok(self.option_receipt(option, &before, published))
    }
    fn option_receipt(
        &self,
        option: LocalOption,
        before: &dto::DevicePreference,
        result: Result<(), Failure>,
    ) -> OwnerOptionApplied {
        let current = lock(&self.store)
            .owner()
            .item(option)
            .expect("validated applied option");
        let facts = self.group.local_facts();
        let fact_sequence = facts
            .preferences()
            .ok()
            .flatten()
            .filter(|preferences| preferences.0.iter().any(|item| item == &current))
            .and_then(|_| facts.fact_sequence().ok());
        let (outcome, reason) = match result {
            Err(reason) => (dto::ControlOutcome::Failed, reason),
            Ok(()) if current == *before => (dto::ControlOutcome::NoChange, Failure::None),
            Ok(()) => (dto::ControlOutcome::Applied, Failure::None),
        };
        OwnerOptionApplied {
            current,
            outcome,
            reason,
            persisted: option != LocalOption::ForwardingControls || lock(&self.store).forwarding_persisted(),
            fact_sequence,
        }
    }
    pub(super) fn set_claim(
        &mut self,
        requester: &dto::Boot,
        group: GroupId,
        claimed: bool,
        expected: u64,
    ) -> Result<OwnerOptionApplied, Failure> {
        let gate = self.gate.clone();
        let _guard = lock(&gate);
        self.authorize(requester, group)?;
        self.apply_locked()?;
        let option = LocalOption::AudioExitClaim;
        let before = lock(&self.store)
            .owner()
            .item(option)
            .ok_or(Failure::UnsupportedOption)?;
        if let Some(record) = self
            .claims
            .command(self.links.id(), expected, claimed, &self.members())?
        {
            let mut candidate = self.claims.clone();
            candidate.observe(self.links.id(), record)?;
            self.persist_claims(candidate)?;
        }
        let published = self
            .publish_preferences()
            .and_then(|()| self.apply_locked())
            .and_then(|()| self.publish_observations());
        self.publish(self.phase());
        Ok(self.option_receipt(option, &before, published))
    }
    pub(super) fn volume_key(&mut self, code: oink_foundation::input::KeyCode) {
        // Modified volume applies only to a currently forwarding source.
        if !self.settings.forwarding_enabled || self.resolution.owner().is_none_or(|owner| owner == self.links.id()) {
            return;
        }
        let mut settings = self.settings.clone();
        match oink_audio::volume::KeyAction::from_code(code) {
            Some(oink_audio::volume::KeyAction::Up) => {
                settings.forwarding_volume_percent =
                    oink_audio::forward_volume::adjust(settings.forwarding_volume_percent, true)
            }
            Some(oink_audio::volume::KeyAction::Down) => {
                settings.forwarding_volume_percent =
                    oink_audio::forward_volume::adjust(settings.forwarding_volume_percent, false)
            }
            Some(oink_audio::volume::KeyAction::Mute) => settings.forwarding_muted = !settings.forwarding_muted,
            None => return,
        }
        if let Err(error) = self.install_settings(settings) {
            self.error = Some(error);
        }
    }
}

impl Owner {
    pub(super) fn selected_output(&self) -> Option<String> {
        let outputs = &self
            .devices
            .iter()
            .find(|device| device.device == self.links.id())?
            .outputs;
        match &self.settings.receive_output {
            Some(selected) => outputs
                .iter()
                .any(|output| &output.id == selected)
                .then(|| selected.clone()),
            None => outputs
                .iter()
                .find(|output| output.is_default)
                .map(|output| output.id.clone()),
        }
    }
    pub(super) fn refresh_exit_binding(&mut self, force: bool) -> Result<(), Failure> {
        if self.resolution.owner() != Some(self.links.id()) {
            if self.exit_volume.state().is_some() {
                self.exit_volume.retire();
                self.bound_output = None;
            }
            return Ok(());
        }
        if self.awaiting_output_snapshot {
            return Ok(());
        }
        let output = self.selected_output();
        let claim = self.resolution.stamp().ok_or(Failure::NotExit)?.to_wire();
        let same = self.exit_volume.state().is_some_and(|state| {
            state
                .version()
                .is_ok_and(|version| version.exit_claim() == Ok(claim.clone()))
        });
        if !force && output == self.bound_output && same {
            return Ok(());
        }
        self.streams.retire_sessions();
        self.source_session.retire();
        self.receive_sessions.clear();
        self.prepared.clear();
        self.receivers.clear();
        self.playback = None;
        self.starting_playback = None;
        self.playback_idle = false;
        self.draining = false;
        self.stop_source();
        self.bound_output = output.clone();
        let endpoint = if self.mute_cleanup_error.is_some() || self.platform.capture_cleanup_failed() {
            Err(oink_audio::volume::Fault {
                status: dto::ExitVolumeStatus::BackendFailed,
                native_code: None,
            })
        } else if let Some(output) = &output {
            let wake = self.streams.wake();
            self.platform
                .volume_endpoint(output, Arc::new(move || wake.notify_one()))
        } else {
            Err(oink_audio::volume::Fault::unavailable())
        };
        self.exit_volume.bind(claim, output, endpoint)?;
        self.refresh_volume_context();
        Ok(())
    }
    pub(super) fn output_binding(&self) -> Option<dto::ReceiverOutputBinding> {
        let version = self.exit_volume.state()?.version().ok()?;
        dto::ReceiverOutputBinding::from_raw(
            [
                (1, version.exit().ok()?.write()),
                (2, version.output_generation().ok()?.write()),
                (3, version.output_id().ok()?.write()),
            ]
            .into(),
        )
        .ok()
    }
    pub(super) fn resolved_volume(&self) -> Option<dto::ExitVolumeState> {
        let stamp = self.resolution.stamp()?;
        if stamp.owner == self.links.id() {
            return self.exit_volume.state().cloned();
        }
        let current = self.current_boot(stamp.owner)?;
        let table = self.group.members();
        let facts = table
            .members
            .iter()
            .find(|member| member.device == stamp.owner)?
            .facts
            .as_ref()?;
        if facts.boot().ok()? != current || !facts.observed().ok()?.awake().ok()? {
            return None;
        }
        let state = facts.exit_volume().ok()??;
        let version = state.version().ok()?;
        (version.exit().ok()? == current && version.exit_claim().ok()? == stamp.to_wire()).then_some(state)
    }
    pub(super) fn current_volume_context(&self) -> Option<Arc<dto::AudioSessionContext>> {
        let state = self.resolved_volume()?;
        if state.status() != Ok(dto::ExitVolumeStatus::Ready) {
            return None;
        }
        // Volume/mute revision changes do not move a held key's target.
        if let Some(context) = self.volume.load().as_ref() {
            let version = state.version().ok()?;
            let binding = context.receiver_output().ok()?;
            if context.source() == Ok(boot(self.links.boot()))
                && context.exit_claim() == version.exit_claim()
                && binding.receiver() == version.exit()
                && binding.generation() == version.output_generation()
                && binding.output_id() == version.output_id()
            {
                return Some(context.clone());
            }
        }
        oink_audio::session::volume_context(&boot(self.links.boot()), &state)
            .ok()
            .map(Arc::new)
    }
    pub(super) fn refresh_volume_context(&self) {
        let context = self.current_volume_context();
        if *self.volume.load() != context {
            self.volume.publish(context);
        }
    }
    pub(super) fn set_exit_volume(
        &mut self,
        requester: &dto::Boot,
        group: GroupId,
        request: dto::SetExitVolume,
    ) -> Result<dto::ExitVolumeResult, Failure> {
        let gate = self.gate.clone();
        let _guard = lock(&gate);
        let authorization = self.authorize(requester, group).and_then(|_| self.apply_locked());
        let mut result = if let Err(reason) = authorization {
            dto::ExitVolumeResult::from_raw(
                [
                    (1, request.request_id().map_err(|_| Failure::InvalidValue)?.write()),
                    (2, boot(self.links.boot()).write()),
                    (3, dto::ControlOutcome::Refused.write()),
                    (4, reason.write()),
                    (5, self.exit_volume.state().cloned().write()),
                ]
                .into(),
            )
            .map_err(|_| Failure::InvalidValue)?
        } else {
            self.exit_volume.set(requester, request)
        };
        // Publication failure cannot be acknowledged as an applied command. The
        // native readback remains visible in the failed result; do not roll back.
        if self.publish_observations().is_err() {
            result
                .edit(|raw| {
                    raw.insert(3, dto::ControlOutcome::Failed.write());
                    raw.insert(4, Failure::ApplyFailed.write());
                })
                .map_err(|_| Failure::InvalidValue)?;
        }
        self.refresh_volume_context();
        self.publish(self.phase());
        Ok(result)
    }
    pub(super) fn exit_volume_key(&mut self, event: oink_audio::volume::KeyEvent) -> Result<bool, Failure> {
        let gate = self.gate.clone();
        let _guard = lock(&gate);
        self.requester(&event.source)?;
        self.apply_locked()?;
        let result = self.exit_volume.key(event);
        // Failed native steps can still change actual volume. Publish readback
        // and any resulting endpoint fault before returning the causal receipt.
        self.publish_observations()?;
        self.refresh_volume_context();
        result
    }
    fn selected_output_missing(&self) -> bool {
        self.resolution.owner() == Some(self.links.id())
            && self.devices.iter().any(|device| device.device == self.links.id())
            && self.bound_output.is_none()
    }
    pub(super) fn phase(&self) -> AudioPhase {
        if self.mute_cleanup_error.is_some()
            || self.platform.capture_cleanup_failed()
            || self.source_failed
            || self.playback_failed
            || self.selected_output_missing()
        {
            AudioPhase::Failed
        } else if self.paused {
            AudioPhase::Paused
        } else if self.power_state.sleeping {
            AudioPhase::SoundSleep
        } else if self.provisional || self.starting_playback.is_some() || self.exit_switch_deadline.is_some() {
            AudioPhase::Preparing
        } else if self.capture.is_some() {
            if self.encoded.as_ref().is_some_and(|encoded| encoded.sleeping()) {
                AudioPhase::SoundSleep
            } else {
                AudioPhase::Forwarding
            }
        } else if self.playback.is_some() || self.playback_idle {
            AudioPhase::Receiving
        } else if self.resolution.owner() == Some(self.links.id()) {
            AudioPhase::Preparing
        } else if self.settings.forwarding_enabled && !matches!(self.resolution, Resolution::None { .. }) {
            AudioPhase::WaitingForExit
        } else {
            AudioPhase::Idle
        }
    }
    pub(super) fn problem(&self) -> Option<Problem> {
        let code = if self.mute_cleanup_error.is_some() || self.platform.capture_cleanup_failed() {
            dto::UserFacingCode::CleanupFailed
        } else if self.permission == CapturePermission::Denied
            && self.settings.forwarding_enabled
            && self.resolution.owner() != Some(self.links.id())
        {
            dto::UserFacingCode::PermissionDenied
        } else if let Some(problem) = self.remote_problem {
            return Some(problem);
        } else if self.source_failed {
            match self.error.as_deref() {
                Some(error) if error.starts_with("capture_permission_required:") => {
                    dto::UserFacingCode::PermissionRequired
                }
                Some(error) if error.starts_with("capture_permission_denied:") => dto::UserFacingCode::PermissionDenied,
                _ => dto::UserFacingCode::CaptureUnavailable,
            }
        } else if self.playback_failed || self.selected_output_missing() {
            dto::UserFacingCode::DeviceUnavailable
        } else if self.error.is_some() {
            dto::UserFacingCode::BackendFailed
        } else {
            return None;
        };
        Some(Problem::new(code))
    }
    pub(super) fn publish_observations(&mut self) -> Result<(), Failure> {
        let outputs = self
            .devices
            .iter()
            .find(|device| device.device == self.links.id())
            .into_iter()
            .flat_map(|device| device.outputs.iter())
            .take(32)
            .filter_map(|output| {
                if oink_audio::validate_receive_output(Some(&output.id)).is_err() {
                    return None;
                }
                let mut name = output.name.clone();
                while name.len() > 256 {
                    name.pop();
                }
                let latency = self
                    .output_latencies
                    .iter()
                    .find(|value| value.output == output.id)
                    .and_then(|value| value.reported_latency_ms);
                dto::AudioOutputFact::from_raw(
                    [
                        (1, RawValue::Text(output.id.clone())),
                        (2, RawValue::Text(name)),
                        (3, output.is_default.write()),
                        (4, latency.write()),
                    ]
                    .into(),
                )
                .ok()
            })
            .collect();
        let phase = self.phase();
        let activity = match phase {
            AudioPhase::Forwarding => dto::SpeakerActivity::SourceForwarding,
            AudioPhase::Receiving if self.receivers.is_empty() => dto::SpeakerActivity::ExitReady,
            AudioPhase::Receiving => dto::SpeakerActivity::ExitPlaying,
            AudioPhase::SoundSleep if self.power_state.sleeping => dto::SpeakerActivity::SystemSleeping,
            AudioPhase::SoundSleep => dto::SpeakerActivity::SourceSoundSleep,
            AudioPhase::Preparing => dto::SpeakerActivity::Preparing,
            AudioPhase::WaitingForExit => dto::SpeakerActivity::WaitingForExit,
            AudioPhase::Paused => dto::SpeakerActivity::LocalPaused,
            AudioPhase::Failed => dto::SpeakerActivity::Failed,
            _ => dto::SpeakerActivity::Unused,
        };
        let source = self
            .source_session
            .prepared()
            .filter(|_| self.capture.is_some() && !self.provisional)
            .map(|prepared| prepared.session());
        let is_exit = self.resolution.owner() == Some(self.links.id());
        let speaker = dto::SpeakerObservation::from_raw(
            [
                (1, source.write()),
                (2, activity.write()),
                (3, self.settings.forwarding_volume_percent.write()),
                (4, self.settings.forwarding_muted.write()),
                (5, self.output_binding().write()),
                (
                    6,
                    is_exit.then_some(self.receivers.len().min(MAX_SOURCES) as u8).write(),
                ),
                (
                    7,
                    is_exit
                        .then_some(
                            (self.playback.is_some() || self.playback_idle)
                                && !self.playback_failed
                                && self.mute_cleanup_error.is_none(),
                        )
                        .write(),
                ),
                (
                    8,
                    self.resolution
                        .owner()
                        .filter(|owner| *owner != self.links.id())
                        .map(oink_network::wire_identity::device)
                        .write(),
                ),
                (
                    9,
                    self.local_output
                        .value()
                        .map(|reading| {
                            dto::LocalOutputVolume::from_raw(
                                [(1, reading.percent.write()), (2, reading.muted.write())].into(),
                            )
                            .expect("bounded volume reading")
                        })
                        .write(),
                ),
            ]
            .into(),
        )
        .map_err(|_| Failure::InvalidValue)?;
        // A completed healthy observation is an explicit None code. Omitting
        // the field means "not observed" to the card projector and disables its
        // controls. Unknown capture authorization alone is not an audio fault:
        // this device can still be idle/receive; a blocked source reports the
        // actionable PermissionRequired state instead of CaptureUnavailable.
        let mut fault = [(1, dto::ResourceFaultCode::None.write())]
            .into_iter()
            .collect::<oink_protocol::v1::RawMap>();
        if let Some(problem) = self.problem() {
            fault.insert(1, dto::ResourceFaultCode::from_number(problem.code.number()).write());
            fault.insert(3, problem.to_wire().write());
        }
        let fault = Some(dto::ResourceFault::from_raw(fault).expect("typed resource observation"));
        let volume = self.exit_volume.state().cloned();
        let observation = (self.permission.to_wire(), outputs, speaker, fault, volume.clone());
        if self.published_observations.as_ref() != Some(&observation) {
            self.group
                .publish_local_audio(
                    observation.0,
                    observation.1.clone(),
                    observation.2.clone(),
                    observation.3.clone(),
                    observation.4.clone(),
                )
                .map_err(|_| Failure::ApplyFailed)?;
            self.published_observations = Some(observation);
        }
        if volume != self.volume_published {
            if let Some(state) = &volume {
                let changed = dto::ExitVolumeChanged::from_raw([(1, state.write())].into()).expect("volume state");
                for member in self.group.members().members.iter().filter(|member| !member.is_self) {
                    if self
                        .group
                        .peer_agreement(member.device)
                        .is_some_and(|agreement| agreement.contains("oink.audio.exit-volume/1"))
                    {
                        self.group
                            .send_device_control(member.device, Message::ConfigExitVolumeChanged(changed.clone()))
                            .map_err(|_| Failure::ApplyFailed)?;
                    }
                }
            }
            self.volume_published = volume;
        }
        Ok(())
    }
}

impl Owner {
    pub(super) fn peer_audio(&self, peer: DeviceId) -> Vec<dto::AudioSupport> {
        self.group.peer_audio(peer).unwrap_or_default()
    }
    pub(super) fn profile_allowed(&self, peer: DeviceId, profile: StreamFormat) -> bool {
        let Some(agreement) = self.group.peer_agreement(peer) else {
            return false;
        };
        agreement.contains("oink.audio/1")
            && profile.supported_by(
                &self.peer_audio(peer),
                agreement.contains("oink.audio.pcm/1"),
                v1::limits::DATAGRAM,
            )
    }
    pub(super) fn prepare_peer(&mut self, peer: DeviceId) {
        if self.update_suspended {
            return;
        }
        let Some(current_boot) = self.current_boot(peer) else {
            return;
        };
        if self
            .group
            .peer_agreement(peer)
            .is_some_and(|agreement| agreement.contains("oink.audio.state/1"))
        {
            self.supported.insert(peer);
            self.streams.enable_state_reports(peer);
        }
        if self.paused || self.power_state.sleeping || !self.resolution.available() {
            return;
        }
        let Some(stamp) = self.resolution.stamp() else { return };
        if stamp.owner != self.links.id() {
            if peer != stamp.owner
                || !self.settings.forwarding_enabled
                || !self
                    .group
                    .peer_agreement(peer)
                    .is_some_and(|agreement| agreement.contains("oink.audio/1"))
            {
                return;
            }
            let same = self.source_session.offer().is_some_and(|offer| {
                offer.receiver() == Ok(current_boot.clone()) && offer.exit_claim() == Ok(stamp.to_wire())
            });
            if !same {
                self.selected = None;
                self.streams.forget_preparation(peer);
                if let Ok(offer) = self.source_session.begin(current_boot, stamp.to_wire()).cloned() {
                    let message = Message::AudioSourceRouteOffer(offer);
                    self.streams.set_opening(peer, vec![message.clone()]);
                    let _ = self.streams.send(peer, &message, Instant::now());
                }
            }
            return;
        }
        if self.mute_cleanup_error.is_some()
            || self.platform.capture_cleanup_failed()
            || self.playback_failed
            || (self.playback.is_none() && !self.playback_idle)
        {
            return;
        }
        let Some(offer) = self.offers.get(&peer).cloned() else {
            return;
        };
        let Some(output) = self
            .output_binding()
            .filter(|output| output.output_id().ok().flatten().is_some())
        else {
            return;
        };
        let Some(agreement) = self.group.peer_agreement(peer) else {
            return;
        };
        let profile = match oink_audio::negotiation::select(
            self.settings.preferred_format,
            &self.peer_audio(peer),
            agreement.contains("oink.audio.pcm/1"),
            v1::limits::DATAGRAM,
        ) {
            Ok(profile) => profile,
            Err(reason) => {
                self.refuse_offer(peer, offer, reason);
                return;
            }
        };
        if let Some(session) = self.receive_sessions.get(&peer)
            && session.offer() == &offer
            && session.prepared().session().receiver_output() == Ok(output.clone())
            && session.prepared().profile == profile
        {
            return;
        }
        if self.receive_sessions.len() >= MAX_SOURCES && !self.receive_sessions.contains_key(&peer) {
            self.refuse_offer(peer, offer, Failure::Busy);
            return;
        }
        match oink_audio::session::Receiver::prepare(offer.clone(), &current_boot, &stamp.to_wire(), &output, profile) {
            Ok(session) => {
                let message = Message::AudioPrepared(session.prepared().wire.clone());
                self.streams.forget_preparation(peer);
                self.receivers.remove(&peer);
                self.prepared.insert(
                    peer,
                    Negotiated {
                        format: profile,
                        generation: session.prepared().stream_id(),
                    },
                );
                self.receive_sessions.insert(peer, session);
                self.streams.set_opening(peer, vec![message.clone()]);
                let _ = self.streams.send(peer, &message, Instant::now());
            }
            Err(reason) => self.refuse_offer(peer, offer, reason),
        }
    }
    pub(super) fn refuse_offer(&mut self, peer: DeviceId, offer: dto::SourceRouteOffer, reason: Failure) {
        if let Ok(refusal) = dto::AudioPrepareRefused::from_raw(
            [
                (1, offer.write()),
                (2, reason.write()),
                (3, self.output_binding().write()),
            ]
            .into(),
        ) {
            let _ = self
                .streams
                .send(peer, &Message::AudioPrepareRefused(refusal), Instant::now());
        }
    }
    pub(super) fn reject_prepared(&mut self, peer: DeviceId, prepared: &dto::AudioPrepared, code: v1::ErrorCode) {
        let Ok(stream) = prepared.stream_id() else { return };
        let failure = dto::ProtocolError::from_raw(
            [
                (1, (code.number() as u16).write()),
                (2, dto::ErrorScope::Message.write()),
                (3, 0x0603_u16.write()),
            ]
            .into(),
        )
        .expect("typed preparation failure");
        let result = dto::TypedResult::from_raw([(1, dto::ResultKind::Failure.write()), (2, failure.write())].into())
            .expect("typed failure result");
        let ack = dto::AudioPreparedAck::from_raw([(1, stream.write()), (2, result.write())].into())
            .expect("typed preparation acknowledgment");
        let _ = self.streams.send(peer, &Message::AudioPreparedAck(ack), Instant::now());
    }
    pub(super) fn message(&mut self, peer: DeviceId, message: Message) {
        if self.update_suspended {
            // Admission may reopen between receiving a fresh offer and the next
            // timer tick. Reconcile first so abort does not discard that offer.
            self.tick();
            if self.update_suspended {
                return;
            }
        }
        let Some(current_boot) = self.current_boot(peer) else {
            return;
        };
        match message {
            Message::AudioSourceRouteOffer(offer) => {
                let reason =
                    if offer.source() != Ok(current_boot.clone()) || offer.receiver() != Ok(boot(self.links.boot())) {
                        Some(Failure::StaleBoot)
                    } else if self.resolution.owner() != Some(self.links.id()) {
                        Some(Failure::NotExit)
                    } else if self.resolution.stamp().map(Stamp::to_wire) != offer.exit_claim().ok() {
                        Some(Failure::RouteChanged)
                    } else if self.paused || self.power_state.sleeping || self.bound_output.is_none() {
                        Some(Failure::OutputUnavailable)
                    } else {
                        None
                    };
                if let Some(reason) = reason {
                    self.refuse_offer(peer, offer, reason);
                    return;
                }
                if self.offers.get(&peer).is_some_and(|previous| {
                    previous.source() == offer.source()
                        && previous.source_route_generation().ok() > offer.source_route_generation().ok()
                }) {
                    return;
                }
                self.offers.insert(peer, offer);
                self.prepare_peer(peer);
            }
            Message::AudioPrepared(wire) => {
                if self.resolution.owner() != Some(peer) || !self.resolution.available() || self.paused {
                    return;
                }
                let Ok(descriptor) = wire.descriptor() else { return };
                let Ok(profile) = StreamFormat::from_descriptor(&descriptor) else {
                    self.reject_prepared(peer, &wire, v1::ErrorCode::UnsupportedValue);
                    return;
                };
                if !self.profile_allowed(peer, profile) {
                    self.reject_prepared(peer, &wire, v1::ErrorCode::UnnegotiatedCapability);
                    return;
                }
                let Ok(output) = wire.session().and_then(|session| session.receiver_output()) else {
                    return;
                };
                if let Some(version) = self.resolved_volume().and_then(|state| state.version().ok())
                    && (output.receiver() != version.exit()
                        || output.generation().ok() < version.output_generation().ok()
                        || (output.generation() == version.output_generation()
                            && output.output_id() != version.output_id()))
                {
                    return;
                }
                let previous = self.source_session.prepared().cloned();
                let Ok(ack) = self.source_session.accept(&current_boot, wire) else {
                    return;
                };
                let prepared = self.source_session.prepared().expect("accepted").clone();
                let ack_message = Message::AudioPreparedAck(ack);
                if self.streams.send(peer, &ack_message, Instant::now()).is_err() {
                    return;
                }
                self.streams.set_opening(
                    peer,
                    vec![
                        Message::AudioSourceRouteOffer(self.source_session.offer().unwrap().clone()),
                        ack_message,
                    ],
                );
                if previous.as_ref() != Some(&prepared) {
                    if self.exit_switch_deadline.is_some() {
                        if let Some(encoded) = &mut self.encoded
                            && let Err(error) = encoded.reconfigure(profile.config())
                        {
                            self.fail_source(error);
                            return;
                        }
                    } else if self.capture.is_some() {
                        let _ = self.release_capture();
                        self.needs_generation = false; // The exit just allocated this fresh stream.
                    }
                }
                self.selected = Some(Negotiated {
                    format: profile,
                    generation: prepared.stream_id(),
                });
                self.source_failed = false;
                self.remote_problem = None;
                self.error = None;
                self.streams.admit_source(peer, prepared);
            }
            Message::AudioPreparedAck(ack) => {
                let Some(session) = self.receive_sessions.get_mut(&peer) else {
                    return;
                };
                if session.acknowledge(&current_boot, &ack).is_ok() {
                    self.streams.admit_receiver(peer, session.prepared().clone());
                } else if ack.stream_id() == Ok(session.prepared().stream_id())
                    && ack.result().is_ok_and(|result| !result.is_success())
                {
                    self.streams.forget_preparation(peer);
                    self.receivers.remove(&peer);
                    self.prepared.remove(&peer);
                    self.receive_sessions.remove(&peer);
                    self.offers.remove(&peer);
                }
            }
            Message::AudioPrepareRefused(refused) => {
                if refused.offer().ok().as_ref() != self.source_session.offer() {
                    return;
                }
                self.streams.forget_preparation(peer);
                self.selected = None;
                self.stop_source();
                // Facts/output changes trigger another offer; refusals never
                // manufacture another exit or retry against an old generation.
                self.source_session.retire();
            }
            Message::AudioRouteFault(fault) => {
                if self
                    .source_session
                    .prepared()
                    .is_some_and(|prepared| fault.session() == Ok(prepared.session()))
                {
                    self.streams.forget_preparation(peer);
                    self.source_session.retire();
                    self.selected = None;
                    self.fail_source("remote audio output failed".into());
                    self.remote_problem = fault
                        .problem()
                        .ok()
                        .and_then(|problem| Problem::from_wire(problem).ok());
                }
            }
            Message::AudioState(wire) if self.supported.contains(&peer) => {
                if let Ok(state) = DeviceAudioState::from_wire(wire) {
                    self.peer_states.insert(peer, (state, Instant::now()));
                }
            }
            _ => {}
        }
    }
}
