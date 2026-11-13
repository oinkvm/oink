//! Serialized audio owner commands, the global audio configuration and native output binding.
use super::*;
use dto::{ControlFailure as Failure, LocalOption};
use oink_audio::route::{AudioConfig, Inputs, Peer};

/// This device's wall clock for HLC stamps (Unix ms, UTC).
pub(super) fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(super) struct VolumeContextInputs {
    members: Arc<crate::config_module::MemberTable>,
    peer_boot: Option<oink_foundation::session::Boot>,
    resolution: Resolution,
    forwarding_enabled: bool,
    local_volume: Option<dto::ExitVolumeState>,
}

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
        } else if self.group.members().members.iter().any(|member| member.device == peer) {
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
    /// This device's speaker mode is "forward": its forward switch is on and
    /// it is not the exit. The switch of the exit is masked by the role (a PC
    /// turns it on when it becomes the exit, for when it loses it), so it must
    /// never make the exit act as a source, e.g. for volume-key contexts.
    pub(super) fn forwarding(&self) -> bool {
        self.audio.own().mode(Role::Speaker) == Mode::Shared
    }
    /// This device's microphone mode is "receive" (switch on, not the sender).
    pub(super) fn receiving(&self) -> bool {
        self.audio.own().mode(Role::Microphone) == Mode::Shared
    }
    /// The raw forward switch, also while it is masked by the exit role.
    fn forward_switch(&self) -> bool {
        self.audio.own().forward.on
    }
    /// Persist this device's own configuration (device-level) and the clock
    /// after it changed.
    fn persist_own_audio(&mut self) -> Result<(), Failure> {
        let own = self.audio.own();
        let last = self.audio.hlc_last();
        lock(&self.store)
            .update_owner(|owner| owner.commit_audio(own, last))
            .map_err(|_| Failure::PersistenceFailed)?;
        Ok(())
    }
    /// Broadcast the configuration this device executes (nothing while it has
    /// not merged). Every member lists itself and its linked devices, so a
    /// phone that links only to one PC still learns every online device.
    fn publish_audio_config(&self) -> Result<(), Failure> {
        self.group
            .publish_local_audio_config(self.audio.publication())
            .map_err(|_| Failure::ApplyFailed)?;
        Ok(())
    }
    /// Follow a change of this device's own configuration: persist it, and a
    /// forward switch that turned on starts unmuted without the old automatic
    /// off reason; one that turned off stops the source.
    fn own_audio_changed(&mut self, forwarding_before: bool) -> Result<(), Failure> {
        self.persist_own_audio()?;
        let forwarding = self.forwarding();
        // A switch the user (or a setter) turned on starts unmuted; a PC
        // merely losing the exit back to forward keeps its gain and mute.
        if !forwarding_before
            && forwarding
            && self.forward_switch()
            && (self.settings.forwarding_muted || self.forwarding_off_reason.is_some())
        {
            let mut settings = self.settings.clone();
            settings.forwarding_muted = false;
            settings.forwarding_off_reason = None;
            self.persist_settings(&mut settings)?;
        } else if forwarding_before && !forwarding {
            self.stop_source();
        }
        Ok(())
    }
    /// An automatic local change of this device's own forward switch (the
    /// local output was taken over). It is authored like any choice.
    pub(super) fn set_own_forwarding(&mut self, on: bool) {
        let before = self.forwarding();
        if !self.audio.set_own_switch(wall_ms(), Role::Speaker, on) {
            return;
        }
        if let Err(reason) = self
            .own_audio_changed(before)
            .and_then(|()| self.publish_audio_config())
        {
            tracing::warn!(reason = reason.number(), "audio forward switch change failed");
        }
    }
    fn preflight_preferences(&self, preferences: Vec<dto::DevicePreference>) -> Result<(), Failure> {
        let mut facts = (*self.group.local_facts()).clone();
        facts
            .edit(|raw| {
                raw.insert(18, RawValue::Array(preferences.iter().map(WireValue::write).collect()));
            })
            .map_err(|_| Failure::InvalidValue)?;
        v2::frame::encode(&Message::ConfigFacts(facts), v2::limits::CONFIG_FRAME).map_err(|_| Failure::InvalidValue)?;
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
            // An unreadable or unstorable route setting never authorizes a guess.
            self.microphone.resolution = Resolution::Unknown;
            self.stop_microphone();
            self.transition(Resolution::Unknown);
        }
    }
    pub(super) fn apply_locked(&mut self) -> Result<(), Failure> {
        let active_group = lock(&self.store).owner().active_group();
        if active_group != self.active_group {
            self.transition(Resolution::Unknown);
            // A group switch (a whole-group merge too) is a new start: this
            // device's configuration becomes old configuration and merges
            // with the new group (user 2026-11-11 Q3 / B).
            if self.active_group.is_some() {
                self.audio.restart(wall_ms());
            }
            self.active_group = active_group;
        }
        self.paused = self.group.applied().paused;
        self.streams.pause_changed(self.paused || self.update_suspended);
        let Some(group) = self.active_group else {
            self.apply_microphone(&BTreeSet::new());
            self.transition(Resolution::None);
            return Ok(());
        };
        let members = self.members();
        let boots: Vec<_> = members.iter().filter_map(|peer| self.current_boot(*peer)).collect();
        self.exit_volume.retain_sources(|source| boots.contains(source));
        // Linked online members (a live link and its current Boot's facts) and
        // the configuration each publishes; an unjoined member publishes none.
        let mut linked = Vec::new();
        let mut available = BTreeSet::new();
        let table = self.group.members();
        for member in table.members.iter().filter(|member| !member.is_self) {
            let Some(facts) = member.facts.clone() else { continue };
            if facts.group().ok().map(|id| id.0) != Some(group.0)
                || self.current_boot(member.device).as_ref() != facts.boot().ok().as_ref()
            {
                continue;
            }
            let config = match facts.audio_config().map_err(|_| Failure::InvalidValue)? {
                Some(wire) => match AudioConfig::from_wire(&wire) {
                    Ok(config) => Some(config),
                    Err(reason) => {
                        tracing::warn!(
                            reason = reason.number(),
                            "ignoring an audio configuration this build cannot read"
                        );
                        None
                    }
                },
                None => None,
            };
            linked.push(Peer {
                device: member.device,
                config,
            });
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
        if !self.paused && !self.power_state.sleeping {
            available.insert(self.links.id());
        }
        let forwarding_before = self.forwarding();
        let reconciled = self.audio.reconcile(Inputs {
            wall_ms: wall_ms(),
            linked: &linked,
            receive_ready: self.virtual_microphone_ready(),
        });
        for (author, ahead_ms) in &reconciled.clock_ahead {
            tracing::warn!(
                author = %author.short(),
                ahead_ms,
                "received an audio configuration stamp far ahead of this clock"
            );
        }
        if reconciled.merged {
            tracing::info!("merged with the group's audio configuration");
        }
        if reconciled.own {
            self.own_audio_changed(forwarding_before)?;
        }
        // Retire the old route's media and volume presses before relaying.
        self.transition(self.audio.resolve(Role::Speaker, &available));
        self.apply_microphone(&available);
        if reconciled.published {
            self.publish_audio_config()?;
        }
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
                if let Some(peer) = self.resolution.target() {
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
        self.volume_context_inputs = None;
    }
    pub(super) fn transition(&mut self, resolution: Resolution) {
        if resolution == self.resolution {
            return;
        }
        let previous = self.resolution;
        // An unavailable selected exit releases the capture lease immediately.
        let keep_capture = previous.target().is_some_and(|owner| owner != self.links.id())
            && resolution.target().is_some_and(|owner| owner != self.links.id())
            && resolution.available()
            && !self.paused
            && !self.power_state.sleeping
            && self.forwarding()
            && (self.capture.is_some() || self.held_mute.is_some())
            && !self.provisional;
        self.retire_sessions();
        if previous.version() != resolution.version() {
            self.retire_output();
        }
        self.resolution = resolution;
        if keep_capture {
            self.exit_switching = true;
        } else {
            self.stop_source();
        }
        self.source_failed = false;
        self.playback_failed = false;
        self.reset_playback_retry();
        self.error = None;
        self.restart_playback = false;
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
        if self.forwarding() {
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
        self.preflight_preferences(preferences)?;
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
        let keep_local_changed = self.settings.keep_local_playback != settings.keep_local_playback;
        let format_changed = self.settings.quality != settings.quality;
        self.settings = settings;
        self.forwarding_off_reason = self.settings.forwarding_off_reason;
        if !self.forwarding() {
            self.stop_source();
        } else if capture_changed || format_changed {
            // Rebuilding for a new scope or format keeps this forwarding's source
            // mute. Turning local playback on ends that mute itself.
            let _ = self.retire_capture(true, !keep_local_changed);
            if format_changed {
                // Same reset as stop_source, without ending the source mute.
                self.exit_switching = false;
                self.verified = false;
            }
        }
        if output_changed {
            self.retire_sessions();
            self.playback_failed = false;
            self.reset_playback_retry();
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
        if matches!(option, LocalOption::MicrophoneControls | LocalOption::MicrophoneInput) {
            return self.set_microphone_option(option, expected, patch);
        }
        // Forward / receive are the global audio configuration (options 4 / 24
        // are retired): only the forwarding gain is an owner option here.
        if option != LocalOption::ForwardingControls {
            return Err(Failure::UnsupportedOption);
        }
        let before = lock(&self.store)
            .owner()
            .item(option)
            .ok_or(Failure::UnsupportedOption)?;
        if before.revision() != Ok(expected) {
            return Err(Failure::StaleRevision);
        }
        let mut candidate = lock(&self.store).owner().clone();
        candidate.edit(option, expected, patch.clone())?;
        self.preflight_preferences(candidate.preferences())?;
        if defer_persistence {
            lock(&self.store).edit_forwarding_deferred(expected, patch)?;
        } else {
            // Released peers and the final drag position require durability,
            // even when their value is already the current in-memory value.
            lock(&self.store).edit_owner(option, expected, patch)?;
        }
        let settings = lock(&self.store).settings.audio.clone();
        self.apply_settings(settings);
        let published = self.publish_preferences().and_then(|()| self.publish_observations());
        self.publish(self.phase());
        Ok(self.option_receipt(option, &before, published))
    }
    pub(super) fn option_receipt(
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
    /// The local user set `device`'s speaker or microphone mode. This device
    /// authors it in the global audio configuration and broadcasts the whole
    /// configuration (setter decides); the device is not asked. Only what it
    /// can do is checked: a phone is made the exit / sender only on the phone
    /// itself and has no forward / receive; this device receives only with
    /// its virtual microphone installed.
    pub(super) fn set_mode(
        &mut self,
        device: DeviceId,
        role: Role,
        mode: Mode,
    ) -> Result<dto::ControlOutcome, Failure> {
        let gate = self.gate.clone();
        let _guard = lock(&gate);
        self.apply_locked()?;
        self.active_group.ok_or(Failure::NotMember)?;
        let me = self.links.id();
        if !self.members().contains(&device) {
            return Err(Failure::NotMember);
        }
        let offline = || {
            if self.current_boot(device).is_none() {
                Failure::Offline
            } else {
                Failure::Unsupported
            }
        };
        let mobile = device != me
            && self.group.members().members.iter().any(|member| {
                member.device == device
                    && member
                        .facts
                        .as_ref()
                        .is_some_and(|facts| facts.device_kind() == Ok(dto::DeviceKind::Mobile))
            });
        match mode {
            Mode::Holder => {
                let capable = !mobile
                    && match role {
                        Role::Speaker => {
                            device == me
                                || self
                                    .group
                                    .peer_permits(device, oink_protocol::v2::registry::type_id::AudioQualitySourceOffer)
                        }
                        Role::Microphone => {
                            if device == me {
                                self.microphone.native
                            } else {
                                self.group
                                    .peer_permits(device, oink_protocol::v2::registry::type_id::MicrophoneDemand)
                            }
                        }
                    };
                if !capable {
                    return Err(if mobile { Failure::Unsupported } else { offline() });
                }
            }
            Mode::Shared if mobile => return Err(Failure::Unsupported),
            Mode::Shared if device == me && role == Role::Microphone && !self.virtual_microphone_ready() => {
                return Err(Failure::Unsupported);
            }
            Mode::Shared | Mode::Local => {}
        }
        let forwarding_before = self.forwarding();
        if !self.audio.select(wall_ms(), device, role, mode)? {
            return Ok(dto::ControlOutcome::NoChange);
        }
        tracing::info!(?role, ?mode, own = device == me, "authored a global audio mode");
        self.own_audio_changed(forwarding_before)?;
        self.apply_locked()?;
        self.publish_audio_config()?;
        let _ = self.publish_observations();
        self.publish(self.phase());
        Ok(dto::ControlOutcome::Applied)
    }
    pub(super) fn volume_key(&mut self, code: oink_foundation::input::KeyCode) {
        // Modified volume applies only to a currently forwarding source.
        if !self.forwarding() || self.resolution.target().is_none_or(|owner| owner == self.links.id()) {
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
        if self.driver_change_active() {
            return Ok(());
        }
        if self.resolution.target() != Some(self.links.id()) {
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
        let route = self.resolution.version().ok_or(Failure::NotExit)?.to_wire();
        let same = self.exit_volume.state().is_some_and(|state| {
            state
                .version()
                .is_ok_and(|version| version.route_version() == Ok(route.clone()))
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
        if output.is_some() && output != self.bound_output {
            // A fresh endpoint (including a returning default) must not inherit
            // the previous endpoint's failure. Explicitly selected outputs still
            // remain unavailable while absent; never fall back to another UID.
            if self.playback_failed {
                self.error = None;
            }
            self.playback_failed = false;
            self.reset_playback_retry();
        }
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
        self.exit_volume.bind(route, output, endpoint)?;
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
        let route = self.resolution.version()?;
        let exit = self.resolution.target()?;
        if exit == self.links.id() {
            return self.exit_volume.state().cloned();
        }
        let current = self.current_boot(exit)?;
        let table = self.group.members();
        let facts = table
            .members
            .iter()
            .find(|member| member.device == exit)?
            .facts
            .as_ref()?;
        if facts.boot().ok()? != current || !facts.observed().ok()?.awake().ok()? {
            return None;
        }
        let state = facts.exit_volume().ok()??;
        let version = state.version().ok()?;
        (version.exit().ok()? == current && version.route_version().ok()? == route.to_wire()).then_some(state)
    }
    pub(super) fn current_volume_context(&self) -> Option<Arc<dto::AudioSessionContext>> {
        // This snapshot authorizes the focus router's physical volume keys,
        // independently of explicit exit-volume controls or media activity.
        if !self.forwarding() {
            return None;
        }
        let state = self.resolved_volume()?;
        if state.status() != Ok(dto::ExitVolumeStatus::Ready) {
            return None;
        }
        // Volume/mute revision changes do not move a held key's target.
        if let Some(context) = self.volume.load().as_ref() {
            let version = state.version().ok()?;
            let binding = context.receiver_output().ok()?;
            if context.source() == Ok(boot(self.links.boot()))
                && context.route_version() == version.route_version()
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
    pub(super) fn refresh_volume_context(&mut self) {
        let members = self.group.members();
        let peer_boot = self.resolution.target().and_then(|peer| self.group.peer_boot(peer));
        let local_volume = self.exit_volume.state();
        // Facts and native volume observations are immutable snapshots. A mic
        // health tick need not rebuild the speaker's nested wire DTOs while all
        // of these inputs are unchanged. Link loss/boot, roster/facts, route,
        // consent and local output generation changes invalidate immediately.
        if self.volume_context_inputs.as_ref().is_some_and(|last| {
            Arc::ptr_eq(&last.members, &members)
                && last.peer_boot == peer_boot
                && last.resolution == self.resolution
                && last.forwarding_enabled == self.forwarding()
                && last.local_volume.as_ref() == local_volume
        }) {
            return;
        }
        self.volume_context_inputs = Some(VolumeContextInputs {
            members,
            peer_boot,
            resolution: self.resolution,
            forwarding_enabled: self.forwarding(),
            local_volume: local_volume.cloned(),
        });
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
        self.resolution.target() == Some(self.links.id())
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
        } else if self.provisional || self.starting_playback.is_some() || self.exit_switching {
            AudioPhase::Preparing
        } else if self.capture.is_some() {
            if self.encoded.as_ref().is_some_and(|encoded| encoded.sleeping()) {
                AudioPhase::SoundSleep
            } else {
                AudioPhase::Forwarding
            }
        } else if self.playback.is_some() || self.playback_idle {
            AudioPhase::Receiving
        } else if self.resolution.target() == Some(self.links.id()) {
            AudioPhase::Preparing
        } else if self.forwarding() {
            // Forward mode with no exit right now (none chosen, offline,
            // asleep or paused) waits for one; local playback continues.
            AudioPhase::WaitingForExit
        } else {
            AudioPhase::Idle
        }
    }
    pub(super) fn problem(&self) -> Option<Problem> {
        self.local_problem().or(self.remote_problem)
    }
    fn local_problem(&self) -> Option<Problem> {
        let code = if self.mute_cleanup_error.is_some() || self.platform.capture_cleanup_failed() {
            dto::UserFacingCode::CleanupFailed
        } else if self.permission == CapturePermission::Denied
            && self.forwarding()
            && self.resolution.target() != Some(self.links.id())
        {
            dto::UserFacingCode::PermissionDenied
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
        let is_exit = self.resolution.target() == Some(self.links.id());
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
                        .target()
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
            .collect::<oink_protocol::v2::RawMap>();
        if let Some(problem) = self.local_problem() {
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
                    if self.group.peer_permits(
                        member.device,
                        oink_protocol::v2::registry::type_id::ConfigExitVolumeChanged,
                    ) {
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
        // supported_by already rejects PCM16 and anything but Opus5 without quality/1.
        let quality = agreement.permits(oink_protocol::v2::registry::type_id::AudioQualitySourceOffer);
        quality && profile.supported_by(&self.peer_audio(peer), quality, v2::limits::DATAGRAM)
    }
    pub(super) fn prepare_peer(&mut self, peer: DeviceId) {
        if self.update_suspended || self.driver_change_active() {
            return;
        }
        let Some(current_boot) = self.current_boot(peer) else {
            return;
        };
        if self.group.peer_opens(peer, oink_protocol::frame::StreamKind::Audio) {
            self.supported.insert(peer);
            self.streams.enable_state_reports(peer);
        }
        if self.paused || self.power_state.sleeping || !self.resolution.available() {
            return;
        }
        let (Some(stamp), Some(exit)) = (self.resolution.version(), self.resolution.target()) else {
            return;
        };
        if exit != self.links.id() {
            if peer != exit
                || !self.forwarding()
                || !self
                    .group
                    .peer_permits(peer, oink_protocol::v2::registry::type_id::AudioQualitySourceOffer)
            {
                return;
            }
            let same = self.source_session.offer().is_some_and(|offer| {
                offer.receiver() == Ok(current_boot.clone()) && offer.route_version() == Ok(stamp.to_wire())
            });
            if !same {
                self.selected = None;
                self.streams.forget_preparation(peer);
                // Choose the profile before beginning the offer: a begun offer
                // counts as sent above, so an early return must not leave one.
                let profile = if self
                    .group
                    .peer_permits(peer, oink_protocol::v2::registry::type_id::AudioQualitySourceOffer)
                {
                    let rate = if self.settings.quality == oink_audio::negotiation::Quality::Lossless {
                        match self.platform.capture_sample_rate() {
                            Ok(rate) => rate,
                            Err(error) => {
                                self.fail_source(error);
                                return;
                            }
                        }
                    } else {
                        48_000
                    };
                    let Ok(profile) = oink_audio::negotiation::select(
                        self.settings.quality.profile(rate),
                        &self.peer_audio(peer),
                        true,
                        v2::limits::DATAGRAM,
                    ) else {
                        return;
                    };
                    Some(profile)
                } else {
                    None
                };
                if let Ok(offer) = self.source_session.begin(current_boot, stamp.to_wire()).cloned() {
                    let message = match profile {
                        Some(profile) => {
                            let Ok(offer) = self.source_session.set_profile(profile).cloned() else {
                                return;
                            };
                            Message::AudioQualitySourceOffer(offer)
                        }
                        None => Message::AudioSourceRouteOffer(offer),
                    };
                    self.streams.set_opening(peer, vec![message.clone()]);
                    let _ = self.streams.send(peer, &message, Instant::now());
                }
            }
            return;
        }
        // A definite output failure refuses the offer with its reason, so the
        // source stops waiting for this exit (pause audit 6). An output that
        // is still starting is not ready yet and stays silent.
        let failed = if self.mute_cleanup_error.is_some() || self.platform.capture_cleanup_failed() {
            Some(Failure::ResourceFault)
        } else if self.playback_failed {
            Some(Failure::OutputUnavailable)
        } else {
            None
        };
        if let Some(reason) = failed {
            if let Some(offer) = self.offers.remove(&peer) {
                self.refuse_offer(peer, offer, reason);
            }
            return;
        }
        if self.playback.is_none() && !self.playback_idle {
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
            if agreement.permits(oink_protocol::v2::registry::type_id::AudioQualitySourceOffer) {
                offer
                    .preferred_descriptor()
                    .ok()
                    .flatten()
                    .and_then(|descriptor| StreamFormat::from_descriptor(&descriptor).ok())
                    .unwrap_or(StreamFormat::Opus5ms)
            } else {
                StreamFormat::Opus5ms
            },
            &self.peer_audio(peer),
            agreement.permits(oink_protocol::v2::registry::type_id::AudioQualitySourceOffer),
            v2::limits::DATAGRAM,
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
    pub(super) fn reject_prepared(&mut self, peer: DeviceId, prepared: &dto::AudioPrepared, code: v2::ErrorCode) {
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
        let message = match message {
            Message::AudioEndpointState(state) => {
                self.record_mobile_audio_state(peer, state);
                return;
            }
            Message::AudioEndpointResult(result) => {
                self.record_mobile_audio_result(peer, result);
                return;
            }
            message => message,
        };
        let driver_change = self.driver_change_active();
        // M0 stays connected while the installer owns native audio. Keep the
        // latest authenticated source intent: dropping its one reliable offer
        // leaves both peers waiting forever after maintenance. prepare_peer
        // still fences device/session admission until the guard is released.
        if driver_change
            && !matches!(
                &message,
                Message::AudioSourceRouteOffer(_) | Message::AudioQualitySourceOffer(_)
            )
        {
            return;
        }
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
        self.microphone_message(peer, &message);
        match message {
            Message::AudioSourceRouteOffer(offer) | Message::AudioQualitySourceOffer(offer) => {
                let reason = if offer.source() != Ok(current_boot.clone())
                    || offer.receiver() != Ok(boot(self.links.boot()))
                {
                    Some(Failure::StaleBoot)
                } else if self.resolution.target() != Some(self.links.id()) {
                    Some(Failure::NotExit)
                } else if self.resolution.version().map(Version::to_wire) != offer.route_version().ok() {
                    Some(Failure::RouteChanged)
                } else if self.paused || self.power_state.sleeping || (!driver_change && self.bound_output.is_none()) {
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
                if self.resolution.target() != Some(peer) || !self.resolution.available() || self.paused {
                    return;
                }
                let Ok(descriptor) = wire.descriptor() else { return };
                let Ok(profile) = StreamFormat::from_descriptor(&descriptor) else {
                    self.reject_prepared(peer, &wire, v2::ErrorCode::UnsupportedValue);
                    return;
                };
                if !self.profile_allowed(peer, profile) {
                    self.reject_prepared(peer, &wire, v2::ErrorCode::UnnegotiatedCapability);
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
                        if self
                            .group
                            .peer_permits(peer, oink_protocol::v2::registry::type_id::AudioQualitySourceOffer)
                        {
                            Message::AudioQualitySourceOffer(self.source_session.offer().unwrap().clone())
                        } else {
                            Message::AudioSourceRouteOffer(self.source_session.offer().unwrap().clone())
                        },
                        ack_message,
                    ],
                );
                if previous.as_ref() != Some(&prepared) {
                    if self
                        .encoded
                        .as_ref()
                        .is_some_and(|capture| capture.sample_rate() != profile.config().sample_rate)
                    {
                        let _ = self.release_capture();
                    }
                    if self.exit_switching {
                        if let Some(encoded) = &mut self.encoded
                            && let Err(error) = encoded.reconfigure(profile.config())
                        {
                            self.fail_source(error);
                            return;
                        }
                    } else if self.capture.is_some() {
                        if matches!(self.release_capture(), CaptureRetirement::CleanupFailed(_)) {
                            return;
                        }
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
                if matches!(
                    refused.reason(),
                    Ok(Failure::NotExit | Failure::RouteChanged | Failure::OutputChanged)
                ) {
                    // M0 refusal may arrive before C0's new exit/output facts.
                    // Retire only the route; this source is still forwarding.
                    self.exit_switching = self.capture.is_some() || self.held_mute.is_some();
                } else {
                    self.stop_source();
                }
                // Facts/output changes trigger another offer; refusals never
                // manufacture another exit or retry against an old generation.
                self.source_session.retire();
                // The exit's output failed: say so instead of "waiting for exit".
                if matches!(
                    refused.reason(),
                    Ok(Failure::OutputUnavailable | Failure::ResourceFault)
                ) {
                    self.remote_problem = Some(Problem::new(dto::UserFacingCode::DeviceUnavailable));
                }
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
                    // The failed output belongs to the exit. Restore local audio
                    // and wait for a fresh preparation without faulting this device.
                    self.stop_source();
                    self.remote_problem = fault
                        .problem()
                        .ok()
                        .and_then(|problem| Problem::from_wire(problem).ok());
                }
            }
            Message::AudioMediaState(state) => {
                if let Some(receiver) = self.receivers.get(&peer) {
                    receiver.reader.observe_media(&state, Instant::now());
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
