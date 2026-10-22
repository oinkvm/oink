use super::*;
impl Owner {
    pub(crate) fn receive_revocation(&mut self, candidate: State) -> v1::Result<()> {
        self.commit(candidate).map_err(|_| v1::ErrorCode::StateConflict)?;
        let links = self.sessions.keys().copied().collect::<Vec<_>>();
        for link in links {
            self.close(link, Disconnect::MemberRemoved);
        }
        self.fresh_after_revocation()
            .map_err(|_| v1::ErrorCode::StateConflict)?;
        Ok(())
    }
    pub(crate) fn fresh_after_revocation(&mut self) -> ApiResult<()> {
        let fresh = State::create(
            &self.identity,
            &self.local.value().name().map_err(internal)?.0,
            &self.local.value().os().map_err(internal)?.0,
            now_ms(),
        )
        .map_err(internal)?;
        if self.commit(fresh.clone()).is_err() {
            self.pending_revocation = Some(fresh);
            return Ok(());
        }
        self.displays_changed(self.displays.clone())
    }

    pub(crate) fn commit(&mut self, candidate: State) -> ApiResult<()> {
        let digest = candidate.digest().map_err(internal)?;
        if let Some(pending) = self.journal.pending_projection().map_err(internal)?
            && oink_network::group_merge::digest(&pending).map_err(internal)? != digest
        {
            return Err(internal("committed projection pending"));
        }
        if digest == self.state.digest().map_err(internal)? {
            if self.needs_apply {
                self.finish_apply()?;
            }
            return Ok(());
        }
        let gate = self.owner_gate.clone();
        let changes_group = candidate.roster().anchor() != self.state.roster().anchor();
        let _guard = if changes_group {
            Some(gate.try_lock().map_err(|_| ApiError::InvalidState {
                message: "owner busy".into(),
            })?)
        } else {
            None
        };
        let mut stored = self.stored.clone();
        stored.state = candidate.wire().clone();
        group_store::save(&self.data_dir.join(group_store::STATE_FILE), &stored).map_err(internal)?;
        if changes_group {
            let mut config = self.config.lock().map_err(internal)?;
            let group = candidate.roster().anchor().group;
            let mut next_owner = config.owner().clone();
            next_owner.switch_group(group, self.identity.id()).map_err(internal)?;
            let mut next_facts = self.local.clone();
            next_facts
                .group(group, &next_owner, self.identity.id())
                .map_err(internal)?;
            config
                .update_owner(|owner| owner.switch_group(group, self.identity.id()))
                .map_err(internal)?;
            self.local = next_facts;
        }
        self.state = candidate;
        self.publish_nearby();
        self.stored = stored;
        self.needs_apply = true;
        let _ = self.refresh_diagnostics();
        self.finish_apply()
    }
    fn finish_apply(&mut self) -> ApiResult<()> {
        self.application.apply_state(&self.stored).map_err(internal)?;
        self.local
            .edit(|raw| {
                raw.insert(5, RawValue::Bytes(self.state.roster().anchor().group.0.to_vec()));
                Ok(())
            })
            .map_err(internal)?;
        self.needs_apply = false;
        if let Err(error) = self.publish() {
            self.needs_apply = true;
            return Err(error);
        }
        self.broadcast(Message::ConfigState(self.state.wire().clone()));
        self.shared.emit(ConfigChange::State {
            items: vec![Item::Members, Item::Layout],
        });
        if let Err(error) = self.reconcile_observed_displays() {
            self.needs_apply = true;
            return Err(error);
        }
        Ok(())
    }
    fn reconcile_observed_displays(&mut self) -> ApiResult<()> {
        // A whole-layout LWW winner can contain this owner's old native bounds
        // when two owners observe displays concurrently. No new OS callback will
        // follow: the actual displays already changed. Re-publish our observed
        // native geometry against the winning register, preserving its device
        // transforms and other owners. The second commit is stable, so this does
        // not create a publication loop or issue any native display write.
        // An unenumerated startup is not an observation of zero displays, and a
        // signed merge projection must remain exact until its journal is settled.
        if !self.displays_observed
            || !self.state.roster().contains(self.identity.id())
            || self.journal.pending_projection().map_err(internal)?.is_some()
        {
            return Ok(());
        }
        let register = self.state.layout().map_err(internal)?;
        let layout = dto::LayoutValue::from_raw(register.value().map_err(internal)?).map_err(internal)?;
        let update =
            layout_wire::sync_local_displays(&layout, self.identity.id(), &self.displays, &self.detached, now_ms())
                .map_err(internal)?;
        let Some(local) = update
            .layout
            .devices()
            .map_err(internal)?
            .0
            .into_iter()
            .find(|d| d.device().and_then(|id| wire::read_device(&id)) == Ok(self.identity.id()))
        else {
            return Ok(());
        };
        // The ordinary display-change path may repair collisions. This path is
        // only restoring lost owner facts: never normalize a remote canvas edit
        // or move another computer while correcting native geometry.
        let mut groups = layout.devices().map_err(internal)?.0;
        if let Some(group) = groups.iter_mut().find(|g| g.device() == local.device()) {
            let mut retained = group.raw().clone();
            retained.insert(4, local.screens().map_err(internal)?.write());
            *group = dto::DevicePlacement::from_raw(retained).map_err(internal)?;
        } else {
            groups.push(local);
        }
        let mut raw = layout.raw().clone();
        raw.insert(1, RawValue::Array(groups.iter().map(WireValue::write).collect()));
        if &raw == layout.raw() {
            return Ok(());
        }
        detached::save(&self.data_dir.join(detached::DETACHED_FILE), &update.detached).map_err(internal)?;
        self.detached = update.detached;
        let mut stored = self.stored.clone();
        stored
            .edit_layout(&register.version().map_err(internal)?, |value| {
                *value = raw;
                Ok(())
            })
            .map_err(internal)?;
        self.commit(State::verify(stored.state, self.state.roster().anchor()).map_err(internal)?)
    }
    pub(crate) fn project_decision(&mut self) -> ApiResult<bool> {
        let Some(wire) = self.journal.pending_projection().map_err(internal)? else {
            return Ok(true);
        };
        if let Some(bundle) = self.journal.pending_bundle().map_err(internal)? {
            let intent = bundle.certificate().and_then(|c| c.intent()).map_err(internal)?;
            if let Err(error) = self.application.prepare_profile_join(&intent) {
                tracing::warn!(%error, "profile handoff deferred before group projection");
                self.projection_retry = Some(Instant::now() + Duration::from_millis(50));
                return Ok(false);
            }
        }
        let roster = wire.roster().map_err(internal)?;
        let anchor = Anchor {
            group: GroupId(wire.group().map_err(internal)?.0),
            root: roster.root_id().map_err(internal)?.0,
        };
        let candidate = State::verify(wire, anchor).map_err(internal)?;
        let digest = candidate.digest().map_err(internal)?;
        if self.projection_announced != Some(digest) {
            if let Some(bundle) = self.journal.pending_bundle().map_err(internal)? {
                self.gossip_transition(&bundle).map_err(internal)?;
                let intent = bundle.certificate().map_err(internal)?.intent().map_err(internal)?;
                let pair = wire::read_pair(&intent.pair_id().map_err(internal)?).map_err(internal)?;
                self.supersede(pair, &intent);
            }
            self.projection_announced = Some(digest);
        }
        if candidate.roster().anchor() != self.state.roster().anchor() {
            for (id, session) in &mut self.sessions {
                if session.ready {
                    session.ready = false;
                    self.links.retire(*id);
                    // The transition proof was queued on every old ready C0
                    // before retiring business authority. Resume signed member
                    // admission on those same C0s after projection, including
                    // links between two members that learned the proof elsewhere.
                    // Otherwise neither endpoint restarts their retired lane.
                    if candidate.roster().contains(session.peer_boot.device_id)
                        && !self.recovery_projected.iter().any(|(link, _, _)| link == id)
                    {
                        self.recovery_projected.push((*id, None, false));
                    }
                }
            }
        }
        if let Err(error) = self.commit(candidate) {
            tracing::warn!(%error, "committed group projection deferred");
            self.projection_retry = Some(Instant::now() + Duration::from_millis(50));
            return Ok(false);
        }
        self.journal.projected(digest).map_err(internal)?;
        self.projection_retry = None;
        self.projection_announced = None;
        Ok(true)
    }
    pub(crate) fn publish(&mut self) -> ApiResult<()> {
        if self.needs_apply {
            return Err(internal("native group application pending"));
        }
        self.observe_links()?;
        let links = self.links.links();
        let ready: BTreeSet<_> = links
            .links
            .iter()
            .filter(|link| {
                link.state == oink_network::link::LinkState::Ready
                    && self.sessions.get(&link.link).is_some_and(|session| session.ready)
            })
            .map(|link| link.peer)
            .collect();
        let previous = self.shared.members.load();
        let previous_group = self.shared.applied.load().group;
        let mut went_offline = false;
        for member in &previous.members {
            if !member.is_self
                && member.online
                && (previous_group != self.state.roster().anchor().group || !ready.contains(&member.device))
            {
                // Last seen is the end of our live logical link, measured by
                // this device's wall clock, never the peer's clock or facts age.
                self.presence
                    .observe(previous_group, member.device, now_ms())
                    .map_err(internal)?;
                went_offline = true;
            }
        }
        if went_offline && let Err(error) = self.presence.flush() {
            tracing::warn!(%error, "could not persist last live link time; will retry");
        }
        let register = self.state.layout().map_err(internal)?;
        let value = dto::LayoutValue::from_raw(register.value().map_err(internal)?).map_err(internal)?;
        let mut layout = layout_wire::from_wire(&value).map_err(internal)?;
        // Retained geometry is replicated verbatim, while executable geometry
        // can only reference current members. Offline members remain in the roster.
        layout
            .screens
            .retain(|screen| self.state.roster().contains(screen.id.device));
        let layout = Arc::new(layout);
        let mut owners = BTreeMap::new();
        for (device, facts) in std::iter::once((self.identity.id(), self.local.value()))
            .chain(self.facts.iter().map(|(d, f)| (*d, f.as_ref())))
        {
            if self.state.roster().contains(device)
                && let Some(preferences) = facts.preferences().map_err(internal)?
                && let Ok(owner) = layout_wire::owner_preferences(&preferences.0)
            {
                owners.insert(device, owner);
            }
        }
        let owners = Arc::new(owners);
        let pause = self.local.value().pause().map_err(internal)?;
        let paused = pause.as_ref().is_some_and(|p| p.paused() == Ok(true));
        let emergency = pause
            .as_ref()
            .is_some_and(|p| p.reason() == Ok(dto::PauseReason::Emergency));
        let members = self.state.roster().members().map(|m| m.device).collect::<Vec<_>>();
        let devices = if paused {
            BTreeSet::new()
        } else {
            members
                .iter()
                .copied()
                .filter(|device| {
                    if *device == self.identity.id() {
                        return true;
                    }
                    let Some(facts) = self.facts.get(device) else {
                        return false;
                    };
                    self.sessions.values().any(|s| {
                        s.ready
                            && s.peer_boot.device_id == *device
                            && facts.boot().and_then(|b| wire::read_boot(&b)) == Ok(s.peer_boot)
                    }) && !facts.pause().ok().flatten().is_some_and(|p| p.paused() == Ok(true))
                        && owners.get(device).is_some_and(|o| o.screen_shared)
                })
                .collect()
        };
        let version = register.version().map_err(internal)?;
        let router = RouterLayout {
            group: self.state.roster().anchor().group,
            version: version.clone(),
            layout: layout.clone(),
            owners: owners.clone(),
            topology: TopologyLayout::from_effective(&layout.effective(&owners), |d| devices.contains(d)),
            devices,
        };
        let pause_changed = {
            let previous = self.shared.applied.load();
            previous.paused != paused || previous.emergency != emergency
        };
        self.shared.router.publish(router.clone());
        // Retained owner preferences describe offline drawing only. The router
        // above keeps its live-only owner baseline and readiness checks.
        let mut drawing_owners = (*owners).clone();
        for device in &members {
            if !drawing_owners.contains_key(device)
                && !self.facts.contains_key(device)
                && let Some(facts) = self.cache.get(self.state.roster().anchor().group, *device)
                && let Some(preferences) = facts.preferences().map_err(internal)?
                && let Ok(owner) = layout_wire::owner_preferences(&preferences.0)
            {
                drawing_owners.insert(*device, owner);
            }
        }
        let drawing_owners = Arc::new(drawing_owners);
        self.shared.applied.publish(AppliedState {
            group: self.state.roster().anchor().group,
            roster_digest: self.state.roster().digest().map_err(internal)?,
            members: members.clone(),
            layout: layout.clone(),
            owners: drawing_owners.clone(),
            layout_source: value.source().map_err(internal)?,
            version: version.clone(),
            paused,
            emergency,
        });
        self.shared.displays.publish(LocalDisplays {
            displays: self.displays.clone(),
            layout: layout.clone(),
            owners: drawing_owners,
        });
        let table: Vec<MemberInfo> = self
            .state
            .roster()
            .members()
            .map(|member| {
                let own = member.device == self.identity.id();
                let facts = if own {
                    Some(Arc::new(self.local.value().clone()))
                } else {
                    self.facts.get(&member.device).cloned().or_else(|| {
                        self.cache
                            .get(self.state.roster().anchor().group, member.device)
                            .map(Arc::new)
                    })
                };
                MemberInfo {
                    device: member.device,
                    is_self: own,
                    name: facts
                        .as_ref()
                        .and_then(|f| f.name().ok())
                        .map(|n| n.0)
                        .or_else(|| member.name.clone())
                        .unwrap_or_default(),
                    os: facts
                        .as_ref()
                        .and_then(|f| f.os().ok())
                        .map(|n| n.0)
                        .or_else(|| member.os.clone())
                        .unwrap_or_default(),
                    online: own || ready.contains(&member.device),
                    facts,
                    added_by: Some(member.issuer),
                    added_at_ms: member.issued_ms,
                    last_connected_ms: self
                        .presence
                        .last_connected(self.state.roster().anchor().group, member.device),
                    protocol_incompatible: self.incompatible.contains(&member.device),
                    address_stale: self.dials.get(&member.device).is_some_and(|d| d.failures >= 4),
                }
            })
            .collect();
        let _ = self.refresh_diagnostics();
        self.shared.members.publish(MemberTable { members: table });
        self.shared.local_facts.publish(self.local.value().clone());
        let advert = Advert::local(
            self.identity.id(),
            self.local.value().name().map_err(internal)?.0,
            self.state.roster().anchor().group,
            members.len() as u8,
            self.links.local_addr().port(),
            self.local.value().os().map_err(internal)?.0,
        );
        self.advert.send_replace(advert);
        self.shared.addresses.publish(
            oink_network::discovery::local_ipv4()
                .into_iter()
                .map(|(ip, _)| std::net::IpAddr::V4(ip))
                .collect(),
        );

        // Local owner pause is not a signed GroupState edit, but application
        // modules still need the applied-state notification to stop their writers.
        if pause_changed {
            self.shared.emit(ConfigChange::State {
                items: vec![Item::Pause],
            });
        }
        self.shared.emit(ConfigChange::Members);
        Ok(())
    }
    pub(crate) fn publish_facts(&mut self) -> ApiResult<u64> {
        if self.needs_apply || self.journal.pending_projection().map_err(internal)?.is_some() {
            return Err(internal("group projection pending"));
        }
        self.observe_links()?;
        self.shared.local_facts.publish(self.local.value().clone());
        self.broadcast(Message::ConfigFacts(self.local.value().clone()));
        self.shared.emit(ConfigChange::Facts {
            device: self.identity.id(),
            capabilities_changed: false,
        });
        self.local.value().fact_sequence().map_err(internal)
    }
    fn observe_links(&mut self) -> ApiResult<()> {
        let mut links = self
            .links
            .links()
            .links
            .iter()
            .filter(|link| {
                self.state.roster().contains(link.peer)
                    && self.sessions.get(&link.link).is_some_and(|session| session.ready)
            })
            .cloned()
            .collect::<Vec<_>>();
        links.sort_by_key(|link| link.peer);
        self.local.links(&links).map_err(internal)?;
        Ok(())
    }
    pub(crate) fn publish_owner(
        &mut self,
        preferences: Vec<dto::DevicePreference>,
        pause: dto::LocalPauseState,
    ) -> ApiResult<u64> {
        let watermark = self
            .config
            .lock()
            .map_err(internal)?
            .owner()
            .claims(self.stored.group())
            .watermark();
        self.local.owner(preferences, pause, watermark).map_err(internal)?;
        self.publish()?;
        self.publish_facts()
    }
    pub(crate) fn publish_audio(
        &mut self,
        permission: dto::CapturePermission,
        outputs: Vec<dto::AudioOutputFact>,
        speaker: dto::SpeakerObservation,
        fault: Option<dto::ResourceFault>,
        exit_volume: Option<dto::ExitVolumeState>,
    ) -> ApiResult<u64> {
        self.local
            .audio(permission, outputs, speaker, fault, exit_volume)
            .map_err(internal)?;
        self.publish_facts()
    }
    pub(crate) fn publish_input(
        &mut self,
        mouse: dto::SharingMode,
        keyboard: dto::SharingMode,
        faults: dto::ResourceFaults,
        options: dto::LocalOptionsSummary,
        game: Option<dto::GameObservation>,
        accessibility_granted: Option<bool>,
    ) -> ApiResult<u64> {
        self.local
            .input(mouse, keyboard, faults, options, game, accessibility_granted)
            .map_err(internal)?;
        self.publish()?;
        self.publish_facts()
    }
    pub(crate) fn publish_native_arrangement(&mut self, token: String) -> ApiResult<u64> {
        if self
            .local
            .value()
            .native_arrangement_token()
            .map_err(internal)?
            .as_ref()
            .map(|t| t.0.as_str())
            == Some(token.as_str())
        {
            return self.local.value().fact_sequence().map_err(internal);
        }
        self.local
            .edit(|raw| {
                raw.insert(22, RawValue::Text(token));
                Ok(())
            })
            .map_err(internal)?;
        // Profile queries read the member table, not local_facts. Publish the
        // completed observation there before returning; otherwise an unrelated
        // startup publication exposes this token later and invalidates Create.
        self.publish()?;
        self.publish_facts()
    }
    pub(crate) fn publish_claim(&mut self, claim: dto::AudioExitClaim) -> ApiResult<u64> {
        self.local
            .edit(|raw| {
                raw.insert(20, claim.write());
                Ok(())
            })
            .map_err(internal)?;
        self.publish_facts()
    }
    pub(crate) fn device_control(&mut self, peer: DeviceId, message: Message) -> ApiResult<()> {
        if !(0x0d23..=0x0d30).contains(&message.type_id()) {
            return Err(invalid("invalid device control message"));
        }
        let id = self
            .sessions
            .iter()
            .find_map(|(id, s)| {
                (s.ready && s.peer_boot.device_id == peer && self.state.roster().contains(peer)).then_some(*id)
            })
            .ok_or_else(stopped)?;
        if (0x0d2c..=0x0d2e).contains(&message.type_id())
            && !self.links.links().links.iter().find(|l| l.link == id).is_some_and(|l| {
                l.state == oink_network::link::LinkState::Ready && l.agreement.contains("oink.notify/1")
            })
        {
            return Err(invalid("notification capability was not negotiated"));
        }
        if (0x0d2f..=0x0d30).contains(&message.type_id())
            && !self
                .links
                .links()
                .links
                .iter()
                .find(|l| l.link == id)
                .is_some_and(|l| l.agreement.contains("oink.layout.arrangement/1"))
        {
            return Err(invalid("arrangement capability was not negotiated"));
        }
        self.send(id, message).map_err(internal)
    }
    pub(crate) fn protocol_error(&mut self, link: LinkId, error: dto::ProtocolError) -> ApiResult<()> {
        let session = self.sessions.get(&link).ok_or_else(stopped)?;
        if !session.ready || !self.state.roster().contains(session.peer_boot.device_id) {
            return Err(stopped());
        }
        self.send(link, Message::ProtocolError(error)).map_err(internal)
    }
    pub(crate) fn edit_layout(
        &mut self,
        edit: Box<dyn FnOnce(&mut Layout) + Send>,
        source: dto::LayoutSource,
    ) -> ApiResult<()> {
        let register = self.state.layout().map_err(internal)?;
        let before = dto::LayoutValue::from_raw(register.value().map_err(internal)?).map_err(internal)?;
        let mut domain = layout_wire::from_wire(&before).map_err(internal)?;
        edit(&mut domain);
        let after = layout_wire::to_wire(&domain, source).map_err(internal)?;
        let mut stored = self.stored.clone();
        stored
            .edit_layout(&register.version().map_err(internal)?, |raw| {
                layout_wire::patch(raw, &before, &after)
            })
            .map_err(internal)?;
        let candidate = State::verify(stored.state.clone(), self.state.roster().anchor()).map_err(internal)?;
        self.commit(candidate)
    }
    pub(crate) fn displays_changed(&mut self, displays: Vec<DisplayInfo>) -> ApiResult<()> {
        self.displays_observed = true;
        self.displays = displays.clone();
        self.observe_display_facts()?;
        let _ = self.refresh_diagnostics();
        let register = self.state.layout().map_err(internal)?;
        let before = dto::LayoutValue::from_raw(register.value().map_err(internal)?).map_err(internal)?;
        let update = layout_wire::sync_local_displays(&before, self.identity.id(), &displays, &self.detached, now_ms())
            .map_err(internal)?;
        detached::save(&self.data_dir.join(detached::DETACHED_FILE), &update.detached).map_err(internal)?;
        self.detached = update.detached;
        if update.changed {
            let mut stored = self.stored.clone();
            stored
                .edit_layout(&register.version().map_err(internal)?, |raw| {
                    *raw = update.layout.raw().clone();
                    Ok(())
                })
                .map_err(internal)?;
            self.commit(State::verify(stored.state, self.state.roster().anchor()).map_err(internal)?)?;
        }
        let ids = update
            .layout
            .screens()
            .map_err(internal)?
            .0
            .into_iter()
            .map(|s| s.id())
            .collect::<v1::Result<Vec<_>>>()
            .map_err(internal)?;
        self.detached.applied(&ids);
        detached::save(&self.data_dir.join(detached::DETACHED_FILE), &self.detached).map_err(internal)?;
        self.displays = displays;
        self.publish_displays()?;
        self.publish()?;
        self.shared.emit(ConfigChange::LocalDisplays);
        Ok(())
    }
    pub(crate) fn publish_displays(&mut self) -> ApiResult<()> {
        self.observe_display_facts()?;
        let _ = self.refresh_diagnostics();
        self.publish_facts()?;
        Ok(())
    }
    fn observe_display_facts(&mut self) -> ApiResult<()> {
        fn rect(r: oink_foundation::Rect) -> RawValue {
            RawValue::Map([(1, r.x.write()), (2, r.y.write()), (3, r.w.write()), (4, r.h.write())].into())
        }
        let mut displays = Vec::new();
        for d in &self.displays {
            let mut raw: v1::RawMap = [
                (1, RawValue::from(d.key.as_str())),
                (2, RawValue::from(d.name.as_str())),
                (3, rect(d.bounds)),
                (5, d.pixel_width.write()),
                (6, d.pixel_height.write()),
                (8, d.rotation_deg.write()),
                (9, d.pixels_per_unit().write()),
                (10, d.is_primary.write()),
                (11, d.is_builtin.write()),
                (12, d.is_virtual.write()),
            ]
            .into();
            if let Some(size) = d.reference_extent {
                raw.insert(7, RawValue::Map([(1, size.w.write()), (2, size.h.write())].into()));
            }
            displays.push(dto::DisplayFact::from_raw(raw).map_err(internal)?.write());
        }
        self.local
            .edit(|raw| {
                raw.insert(12, RawValue::Array(displays));
                Ok(())
            })
            .map_err(internal)?;
        Ok(())
    }
    // Network interfaces change independently of displays, including while the
    // machine is asleep. The owner polls them and refreshes immediately on wake.
    pub(super) fn observe_endpoint_facts(&mut self) -> ApiResult<bool> {
        // A specific IPv4 listener cannot answer on the host's other interfaces.
        // Advertising those addresses makes authenticated peers dial unreachable
        // sockets, including loopback-only fixtures. Wildcard listeners retain
        // the interface observer so network changes still refresh their Facts.
        let addresses = match self.links.local_addr().ip() {
            std::net::IpAddr::V4(ip) if !ip.is_unspecified() => vec![ip],
            _ => oink_network::discovery::local_ipv4()
                .into_iter()
                .map(|(ip, _)| ip)
                .collect(),
        };
        let endpoints = addresses
            .into_iter()
            .map(|ip| {
                dto::EndpointFact::from_raw(
                    [
                        (1, dto::AddressFamily::Ipv4.write()),
                        (2, RawValue::Bytes(ip.octets().to_vec())),
                        (3, self.links.local_addr().port().write()),
                        (4, 0u32.write()),
                    ]
                    .into(),
                )
                .map(|e| e.write())
            })
            .collect::<v1::Result<Vec<_>>>()
            .map_err(internal)?;
        self.local
            .edit(|raw| {
                raw.insert(10, RawValue::Array(endpoints));
                Ok(())
            })
            .map_err(internal)
    }
    pub(crate) fn remove_member(&mut self, device: DeviceId, expected: Option<[u8; 32]>) -> ApiResult<()> {
        if device == self.identity.id() {
            return Err(invalid("use leave group"));
        }
        let roster = self
            .state
            .roster()
            .remove(&self.identity, device, expected, now_ms())
            .map_err(internal)?;
        let candidate = self.state.with_roster(&roster).map_err(internal)?;
        self.commit(candidate)?;
        // The signed revocation was just queued by commit. Retire application
        // authority now, retaining C0 until the recipient applies it and closes.
        // An immediate disconnect can discard that frame and leave the removed
        // peer believing it still belongs to this group indefinitely.
        for (id, session) in &mut self.sessions {
            if session.peer_boot.device_id == device {
                session.ready = false;
                session.departing = true;
                session.deadline = Some(Instant::now() + Duration::from_secs(10));
                self.links.retire_for_departure(*id);
            }
        }
        Ok(())
    }
    pub(crate) fn leave_group(&mut self) -> ApiResult<()> {
        if self.state.roster().members().count() == 1 {
            return Err(invalid("already alone"));
        }
        let roster = self
            .state
            .roster()
            .remove(&self.identity, self.identity.id(), None, now_ms())
            .map_err(internal)?;
        let revoked = self.state.with_roster(&roster).map_err(internal)?;
        // Persist and apply our signed departure before notifying the group.
        // Starting the new group may wait for an owner preference transaction;
        // that contention must not roll back or partially publish the departure.
        self.commit(revoked)?;
        // Retire business authority immediately, but let the reliable C0 writer
        // deliver the signed departure. Closing the transport here discards its
        // queued state frame. The recipient closes after applying our removal;
        // the ordinary C0 deadline bounds an unresponsive recipient.
        for (id, session) in &mut self.sessions {
            session.ready = false;
            session.departing = true;
            session.deadline = Some(Instant::now() + Duration::from_secs(10));
            self.links.retire_for_departure(*id);
        }
        self.fresh_after_revocation()
    }
    pub(crate) fn member_addresses(&self, device: DeviceId) -> ApiResult<Vec<String>> {
        if !self.state.roster().contains(device) {
            return Err(ApiError::NotMember);
        }
        Ok(self
            .address_book
            .candidates(device)
            .into_iter()
            .map(|a| a.to_string())
            .collect())
    }
    pub(crate) fn set_member_address(&mut self, device: DeviceId, address: String) -> ApiResult<()> {
        if !self.state.roster().contains(device) || device == self.identity.id() {
            return Err(ApiError::NotMember);
        }
        let address = address.parse::<SocketAddr>().map_err(|_| ApiError::InvalidAddress)?;
        if !address.is_ipv4()
            || address.port() == 0
            || address.ip().is_unspecified()
            || address.ip().is_multicast()
            || address.ip() == std::net::Ipv4Addr::BROADCAST
        {
            return Err(ApiError::InvalidAddress);
        }
        self.address_book
            .remember(device, address)
            .map_err(|_| ApiError::PersistenceFailed)?;
        self.links.update_addresses(device, self.member_candidates(device));
        self.dials.entry(device).or_default().after = None;
        self.tick();
        Ok(())
    }
    pub(crate) fn router_compiled(&mut self, compiled: crate::diagnostics::layout::RouterCompilation) {
        self.router_compilation = Some(compiled);
        let _ = self.refresh_diagnostics();
    }
}
