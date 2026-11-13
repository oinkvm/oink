//! Executes the shared member decisions (`oink_network::member`) on this
//! desktop owner: its C0 table, commit/apply pipeline and API events. No
//! membership decision is taken here.
use super::*;
use oink_network::member::{Links, Out};

/// Read-only view of the owner's C0 table for the shared decisions.
pub(crate) struct SessionView<'a>(pub &'a BTreeMap<LinkId, Link>);
impl Links for SessionView<'_> {
    fn facts(&self, link: LinkId) -> Option<member::LinkFacts> {
        let session = self.0.get(&link)?;
        Some(member::LinkFacts {
            boot: session.peer_boot,
            greeted: session.peer.as_ref()?.anchor().ok()?,
            ready: session.ready,
            departing: session.departing,
            admitted: session.admitted_anchor,
        })
    }
    fn greeted(&self) -> Vec<(LinkId, member::LinkFacts)> {
        self.0.keys().filter_map(|id| Some((*id, self.facts(*id)?))).collect()
    }
}

fn phase(value: member::MigrationPhase) -> api::MigrationPhase {
    match value {
        member::MigrationPhase::ProofWaiting => api::MigrationPhase::ProofWaiting,
        member::MigrationPhase::Adopting => api::MigrationPhase::Adopting,
        member::MigrationPhase::Adopted => api::MigrationPhase::Adopted,
        member::MigrationPhase::Conflict => api::MigrationPhase::Conflict,
    }
}

impl Owner {
    /// Run the shared decisions' actions in order. `origin` is the C0 the
    /// input came from (a failed commit fails that call).
    pub(crate) fn execute(&mut self, origin: Option<LinkId>, outs: Vec<Out>) -> v2::Result<()> {
        let _ = origin;
        for out in outs {
            match out {
                Out::Send { link, message } => self.send(link, message)?,
                Out::SendPeer { peer, message } => {
                    if let Some(link) = self.peer_link(peer) {
                        let _ = self.send(link, message);
                    }
                }
                Out::ArmAdmission { link } => {
                    if let Some(session) = self.sessions.get_mut(&link)
                        && !session.ready
                    {
                        session.deadline = Some(Instant::now() + ADMISSION_TIMEOUT);
                    }
                }
                Out::OldMemberLane { link } => self.links.set_old_member_lane(link, true),
                Out::BeginAdmission { link } => self.begin_admission(link)?,
                Out::AdmitPeer { peer } => {
                    if let Some(link) = self.peer_link(peer) {
                        self.begin_admission(link)?;
                    }
                }
                Out::Commit(candidate) => {
                    let digest = candidate.digest()?;
                    if let Err(error) = self.commit(candidate) {
                        if !self.needs_apply || self.state.digest()? != digest {
                            return Err(v2::ErrorCode::StateConflict.into());
                        }
                        // The authenticated update is durable. A local native
                        // failure keeps the previous applied view and streams
                        // until tick retries; admission stays valid.
                        tracing::warn!(%error, "saved group update awaits native application");
                    }
                }
                Out::Accepted { link, state } => {
                    let session = self.sessions.get_mut(&link).ok_or(v2::ErrorCode::NotReady)?;
                    let advanced = session
                        .accepted
                        .as_ref()
                        .is_none_or(|previous| previous.digest().ok() != state.digest().ok());
                    session.accepted = Some(state);
                    if advanced {
                        session.deadline = Some(Instant::now() + ADMISSION_TIMEOUT);
                    }
                    self.links.set_half_open(link);
                }
                Out::HalfOpen { link } => self.links.set_half_open(link),
                Out::Ready { link } => self.ready(link)?,
                Out::SendStateIfDiffers { link, acknowledged } => {
                    // Done acknowledges the peer's frozen Reply, while commit may
                    // preserve a newer local register or reconcile pointer slots.
                    if self.state.digest()? != acknowledged {
                        self.send(link, Message::ConfigState(self.state.wire().clone()))?;
                    }
                }
                Out::AwaitRemoved { link } | Out::RetireRemoved { link } => {
                    // The removed member closes after applying our signed state;
                    // this bounds an unresponsive one. It never consumes state.
                    if let Some(session) = self.sessions.get_mut(&link) {
                        session.deadline = Some(Instant::now() + Duration::from_millis(200));
                    }
                }
                Out::CloseMembers { devices, reason } => {
                    let links = self
                        .sessions
                        .iter()
                        .filter_map(|(id, s)| devices.contains(&s.peer_boot.device_id).then_some(*id))
                        .collect::<Vec<_>>();
                    for link in links {
                        self.close(link, reason);
                    }
                }
                Out::Revoked(candidate) => self.receive_revocation(candidate)?,
                Out::Project => {
                    self.project().map_err(|_| v2::ErrorCode::StateConflict)?;
                }
                Out::PairingChanged => self.publish_pairings(),
                Out::Completed {
                    pair,
                    bundle,
                    reconciled,
                } => self
                    .completed(pair, &bundle, reconciled)
                    .map_err(|_| v2::ErrorCode::StateConflict)?,
                Out::PairingFailed { pair, problem } => {
                    if let Some(peer) = self.endpoints.get(&pair).cloned() {
                        let event = api::PairingEvent::PairingFailed {
                            event_id: DecimalU64(0),
                            pair_id: pairing::key(pair),
                            peer,
                            // Peer reasons are validated on C0 entry; local ones are fixed.
                            problem: api::PairProblem::try_from(&problem).expect("validated problem"),
                        };
                        let _ = self.durable_event(event, None);
                    }
                }
                Out::RecoveredFailure { recovery, problem } => {
                    self.recovered_failure(&recovery, problem)
                        .map_err(|_| v2::ErrorCode::StateConflict)?;
                }
                Out::Migration(status) => {
                    let reference = |a: Anchor| api::GroupRef {
                        group: a.group.into(),
                        root: DeviceId(a.root).to_hex(),
                    };
                    self.shared.migration.publish(Some(api::GroupMigrationStatus {
                        source: reference(status.source),
                        target: status.target.map(reference),
                        phase: phase(status.phase),
                        problem: status.problem.as_ref().and_then(|p| api::PairProblem::try_from(p).ok()),
                    }));
                }
            }
        }
        Ok(())
    }

    /// Install the journal's committed projection through the commit
    /// pipeline. A deferred owner-side step retries from tick.
    pub(crate) fn project(&mut self) -> ApiResult<bool> {
        if let Some(bundle) = self.member.journal.pending_bundle().map_err(internal)? {
            let intent = bundle.certificate().and_then(|c| c.intent()).map_err(internal)?;
            if let Err(error) = self.application.prepare_profile_join_with_target(
                &intent,
                &bundle.plan().and_then(|p| p.target_base()).map_err(internal)?,
            ) {
                tracing::warn!(%error, "profile handoff deferred before group projection");
                self.projection_retry = Some(Instant::now() + Duration::from_millis(50));
                return Ok(false);
            }
        }
        let ready = self
            .sessions
            .iter()
            .filter(|(_, s)| s.ready)
            .map(|(id, s)| (*id, s.peer_boot.device_id))
            .collect::<Vec<_>>();
        let Some(projection) = self.member.projection(&self.state, &ready).map_err(internal)? else {
            self.projection_retry = None;
            let outs = self
                .member
                .resume(&self.state, &SessionView(&self.sessions))
                .map_err(internal)?;
            self.execute(None, outs).map_err(internal)?;
            return Ok(true);
        };
        // The transition proof goes out on every old ready C0 before business
        // authority is retired there.
        let _ = self.execute(None, projection.outs);
        for link in projection.retire {
            if let Some(session) = self.sessions.get_mut(&link) {
                session.ready = false;
                session.deadline = Some(Instant::now() + ADMISSION_TIMEOUT);
                self.links.retire(link);
            }
        }
        if let Err(error) = self.commit(projection.candidate) {
            tracing::warn!(%error, "committed group projection deferred");
            self.projection_retry = Some(Instant::now() + Duration::from_millis(50));
            return Ok(false);
        }
        self.projection_retry = None;
        let outs = self
            .member
            .projected(projection.digest, &self.state, &SessionView(&self.sessions))
            .map_err(internal)?;
        self.reconcile_pointer_shortcuts()?;
        self.execute(None, outs).map_err(internal)?;
        Ok(true)
    }

    /// Ask this C0's member for the transition out of our root now.
    #[cfg(test)]
    pub(crate) fn lookup(&mut self, link: LinkId) -> v2::Result<()> {
        let facts = self.link_facts(link).ok_or(v2::ErrorCode::NotReady)?;
        let outs = self.member.request_lookup(link, &facts, &self.state, Instant::now())?;
        self.execute(Some(link), outs)
    }

    /// One shared tick at `now` (proof and lookup deadlines included).
    #[cfg(test)]
    pub(crate) fn expire_recovery(&mut self, now: Instant) {
        let outs = self.member.tick(now, &self.state, &SessionView(&self.sessions));
        let _ = self.execute(None, outs);
    }

    pub(crate) fn pair_effects(&mut self, effects: Vec<pairing::Effect>) {
        let outs = self.member.pairing_effects(effects, &self.state);
        let _ = self.execute(None, outs);
    }
}
