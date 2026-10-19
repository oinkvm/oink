//! Proof-only old-root traffic and durable decision recovery survive fresh Boots.
use super::*;
use oink_network::{
    group_merge,
    pair_journal::{Decision, Recovery},
    pair_objects,
};
#[derive(Clone)]
pub(super) enum Purpose {
    Adoption(Anchor),
    Decision { pair: PairId, decision: u64 },
}
#[derive(Clone)]
pub(super) struct Expected {
    pub reference: dto::ObjectRef,
    pub context: dto::ObjectContext,
    pub purpose: Purpose,
    pub deadline: Instant,
}
fn group_context(anchor: Anchor) -> v1::Result<dto::ObjectContext> {
    dto::ObjectContext::from_raw(
        [
            (1, dto::ObjectContextKind::GroupRef.write()),
            (2, RawValue::Map([(1, group_merge::group_ref(anchor).write())].into())),
        ]
        .into(),
    )
}
fn pair_context(pair: PairId) -> v1::Result<dto::ObjectContext> {
    dto::ObjectContext::from_raw(
        [
            (1, dto::ObjectContextKind::PairId.write()),
            (2, RawValue::Map([(1, wire::pair(pair).write())].into())),
        ]
        .into(),
    )
}
impl Owner {
    pub(crate) fn lookup(&mut self, link: LinkId) -> v1::Result<()> {
        let source = self.state.roster().anchor();
        // Greeting recovery and a historical decision can request the same
        // proof concurrently. There is one reply slot per link and source root;
        // do not create a second uncorrelated completion for that lookup.
        if self.lookups.get(&link).is_some_and(|(pending, _)| *pending == source) {
            return Ok(());
        }
        let peer = self.sessions[&link].peer.as_ref().ok_or(v1::ErrorCode::NotReady)?;
        let target = peer.anchor()?;
        self.lookups
            .insert(link, (source, Instant::now() + Duration::from_secs(10)));
        self.links.set_old_member_lane(link, true);
        self.migration(source, Some(target), api::MigrationPhase::ProofWaiting, None);
        self.send(
            link,
            Message::ConfigMergeLookup(dto::MergeLookup::from_raw(
                [
                    (1, RawValue::Bytes(source.group.0.to_vec())),
                    (2, RawValue::Bytes(source.root.to_vec())),
                    (3, wire::device(self.identity.id()).write()),
                ]
                .into(),
            )?),
        )
    }
    pub(crate) fn migration(
        &mut self,
        source: Anchor,
        target: Option<Anchor>,
        phase: api::MigrationPhase,
        problem: Option<dto::PairProblem>,
    ) {
        let reference = |a: Anchor| api::GroupRef {
            group: a.group.into(),
            root: DeviceId(a.root).to_hex(),
        };
        let status = api::GroupMigrationStatus {
            source: reference(source),
            target: target.map(reference),
            phase,
            problem: problem.as_ref().and_then(|p| api::PairProblem::try_from(p).ok()),
        };
        self.shared.migration.publish(Some(status.clone()));
        self.shared
            .emit_pairing(api::PairingEvent::GroupMigrationChanged(status));
    }
    pub(crate) fn recovery_message(&mut self, id: LinkId, peer: Boot, message: Message) -> v1::Result<()> {
        match message {
            Message::ConfigMergeLookup(lookup) => {
                if wire::read_device(&lookup.requester()?)? != peer.device_id {
                    return Err(v1::ErrorCode::AuthenticationFailed.into());
                }
                let source = Anchor {
                    group: GroupId(lookup.source_group()?.0),
                    root: lookup.source_root()?.0,
                };
                match self
                    .journal
                    .lookup(peer.device_id, source, lookup.after()?.map(|h| h.0))
                {
                    Ok(Some(bundle)) => self.send_proof(id, &bundle, group_context(source)?, None)?,
                    Ok(None)
                        if source == self.state.roster().anchor()
                            && lookup.after()?.is_none()
                            && self.state.roster().contains(peer.device_id) =>
                    {
                        // A greeting can predate a concurrently applied migration.
                        // The peer is asking about the root we already share, not
                        // a missing outgoing transition. Resume ordinary signed
                        // membership admission; the greeting grants no authority.
                        self.begin_admission(id)?;
                    }
                    result => {
                        let code = if matches!(result, Err(oink_network::pair_journal::Error::Conflict)) {
                            dto::PairCode::MergeConflict
                        } else {
                            dto::PairCode::MergeProofUnavailable
                        };
                        self.send(
                            id,
                            Message::ConfigPairEnded(dto::PairEnded::from_raw(
                                [
                                    (2, pairing::problem(code, dto::PairStage::Adopt).write()),
                                    (4, group_merge::group_ref(source).write()),
                                ]
                                .into(),
                            )?),
                        )?;
                    }
                }
            }
            Message::ConfigMergeTransfer(transfer) => {
                let reference = transfer.bundle()?;
                if reference.kind()? != dto::ObjectKind::Transfer {
                    return Err(v1::ErrorCode::MalformedFrame.into());
                }
                let source = if let Some((source, _)) = self.lookups.get(&id) {
                    *source
                } else {
                    // A previously admitted member may repeat its history proof
                    // while the adopted group is awaiting admission. Keep the
                    // locally validated anchor; the peer's greeting alone never
                    // grants this lane. The signed endpoints are checked below.
                    self.sessions[&id].admitted_anchor.ok_or(v1::ErrorCode::NotReady)?
                };
                self.expect_proof(id, reference, group_context(source)?, Purpose::Adoption(source))?;
            }
            Message::ConfigPairDecisionQuery(query) => {
                let pair = wire::read_pair(&query.pair_id()?)?;
                let decision = query.permit_decision()?;
                let known = self
                    .journal
                    .recoveries()
                    .map_err(|_| v1::ErrorCode::StateConflict)?
                    .into_iter()
                    .find(|r| r.pair == pair && r.decision == decision);
                if known
                    .as_ref()
                    .is_none_or(|r| r.target != self.identity.id() || r.source != peer.device_id)
                {
                    return Err(v1::ErrorCode::AuthenticationFailed.into());
                }
                let result = self
                    .journal
                    .query(peer.device_id, pair, query.intent_digest()?.0, query.permit_decision()?)
                    .map_err(|_| v1::ErrorCode::StateConflict)?;
                match result {
                    Decision::Committed(bundle) => {
                        self.send_proof(id, &bundle, pair_context(pair)?, Some((pair, query.intent_digest()?.0)))?
                    }
                    Decision::Aborted(problem) => self.send(
                        id,
                        Message::ConfigPairDecisionReply(dto::PairDecisionReply::from_raw(
                            [
                                (1, wire::pair(pair).write()),
                                (2, query.intent_digest()?.write()),
                                (
                                    3,
                                    RawValue::Map(
                                        [
                                            (1, dto::DecisionKind::Aborted.write()),
                                            (2, RawValue::Map([(1, problem.write())].into())),
                                        ]
                                        .into(),
                                    ),
                                ),
                            ]
                            .into(),
                        )?),
                    )?,
                    Decision::Pending | Decision::Unconfirmed => self.send(
                        id,
                        Message::ConfigPairDecisionReply(dto::PairDecisionReply::from_raw(
                            [
                                (1, wire::pair(pair).write()),
                                (2, query.intent_digest()?.write()),
                                (
                                    3,
                                    RawValue::Map(
                                        [
                                            (1, dto::DecisionKind::Pending.write()),
                                            (2, RawValue::Map(v1::RawMap::new())),
                                        ]
                                        .into(),
                                    ),
                                ),
                            ]
                            .into(),
                        )?),
                    )?,
                }
            }
            Message::ConfigPairDecisionReply(reply) => {
                let pair = wire::read_pair(&reply.pair_id()?)?;
                let recovery = self.recovery(pair).ok_or(v1::ErrorCode::AuthenticationFailed)?;
                if recovery.source != self.identity.id()
                    || recovery.target != peer.device_id
                    || group_merge::digest(&recovery.plan.intent()?)? != reply.intent_digest()?.0
                {
                    return Err(v1::ErrorCode::AuthenticationFailed.into());
                }
                let outcome = reply.outcome()?;
                match outcome.variant()? {
                    v1::schema::Open::Known(dto::DecisionKind::Committed) => {
                        let reference = dto::DecisionCommitted::from_raw(outcome.body()?)?.bundle()?;
                        self.expect_proof(
                            id,
                            reference,
                            pair_context(pair)?,
                            Purpose::Decision {
                                pair,
                                decision: recovery.decision,
                            },
                        )?;
                    }
                    v1::schema::Open::Known(dto::DecisionKind::Aborted) => {
                        let problem = dto::DecisionAborted::from_raw(outcome.body()?)?.problem()?;
                        self.journal
                            .resolved_abort(pair, recovery.decision, peer.device_id, problem.clone())
                            .map_err(|_| v1::ErrorCode::StateConflict)?;
                        self.recovered_failure(&recovery, problem)
                            .map_err(|_| v1::ErrorCode::StateConflict)?;
                    }
                    v1::schema::Open::Known(dto::DecisionKind::Pending) => {}
                    _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
                }
            }
            Message::ConfigPairObjectBegin(begin) => {
                let reference = begin.object()?;
                let expected = self
                    .proof_expected
                    .get(&(id, reference.digest()?.0))
                    .ok_or(v1::ErrorCode::AuthenticationFailed)?;
                if reference != expected.reference {
                    return Err(v1::ErrorCode::AuthenticationFailed.into());
                }
                let mut allowed = begin.context()? == expected.context;
                // Unsolicited member gossip identifies its signed source in the
                // object context; execution still waits for target-root checks.
                if !allowed
                    && matches!(expected.purpose, Purpose::Adoption(_))
                    && self.sessions[&id].admitted_anchor.is_some()
                {
                    allowed = begin.context()?.variant()? == v1::schema::Open::Known(dto::ObjectContextKind::GroupRef);
                }
                self.proof_objects.begin(
                    id.0,
                    begin,
                    |_, kind| allowed && kind == dto::ObjectKind::Transfer,
                    Instant::now(),
                )?;
            }
            Message::ConfigPairObjectChunk(chunk) => {
                let digest = chunk.digest()?.0;
                self.proof_objects.chunk(id.0, chunk, Instant::now())?;
                if let Some(expected) = self.proof_expected.get_mut(&(id, digest)) {
                    expected.deadline = Instant::now() + Duration::from_secs(10);
                }
            }
            Message::ConfigPairObjectEnd(end) => {
                let complete = self.proof_objects.end(id.0, end)?;
                let expected = self
                    .proof_expected
                    .remove(&(id, complete.reference.digest()?.0))
                    .ok_or(v1::ErrorCode::StateConflict)?;
                let bundle = dto::MergeTransferBundle::from_raw(complete.raw)?;
                let intent = bundle.certificate()?.intent()?;
                let source = group_merge::anchor(&intent.source()?)?;
                let target = group_merge::anchor(&intent.target()?)?;
                match expected.purpose {
                    Purpose::Decision { pair, decision } => {
                        let recovery = self.recovery(pair).ok_or(v1::ErrorCode::AuthenticationFailed)?;
                        if recovery.target != peer.device_id
                            || recovery.source != self.identity.id()
                            || bundle.plan() != Ok(recovery.plan.clone())
                            || bundle.certificate()?.permit_decision()? != decision
                        {
                            return Err(v1::ErrorCode::AuthenticationFailed.into());
                        }
                        self.journal
                            .commit(&bundle, &self.state)
                            .map_err(|_| v1::ErrorCode::StateConflict)?;
                        // A committed decision can outlive the peer's group.
                        // Its old proof does not authorize admission to an unrelated
                        // current root after an explicit reset or a later migration.
                        self.recovery_projected.push((id, None, true));
                        self.merge_effects(vec![oink_network::pair_merge::Effect::Project])
                            .map_err(|_| v1::ErrorCode::StateConflict)?;
                    }
                    Purpose::Adoption(expected_source) => {
                        if self.lookups.contains_key(&id) && source != expected_source {
                            return Err(v1::ErrorCode::AuthenticationFailed.into());
                        }
                        if source != self.state.roster().anchor() && target != self.state.roster().anchor() {
                            return Err(v1::ErrorCode::AuthenticationFailed.into());
                        }
                        group_merge::verify_bundle(&bundle, source)?;
                        let conflict = self
                            .journal
                            .history_conflict(&bundle)
                            .map_err(|_| v1::ErrorCode::StateConflict)?;
                        if conflict {
                            self.journal
                                .adopt(&bundle, &self.state, false)
                                .map_err(|_| v1::ErrorCode::StateConflict)?;
                            self.migration(
                                source,
                                Some(target),
                                api::MigrationPhase::Conflict,
                                Some(pairing::problem(dto::PairCode::MergeConflict, dto::PairStage::Adopt)),
                            );
                            self.lookups.remove(&id);
                            return Ok(());
                        }
                        let apply = source == self.state.roster().anchor();
                        self.journal
                            .adopt(&bundle, &self.state, apply)
                            .map_err(|_| v1::ErrorCode::StateConflict)?;
                        self.lookups.remove(&id);
                        let receipt = Message::ConfigMergeReceipt(dto::MergeReceipt::from_raw(
                            [
                                (1, intent.merge_id()?.write()),
                                (2, wire::device(self.identity.id()).write()),
                            ]
                            .into(),
                        )?);
                        if apply {
                            self.migration(source, Some(target), api::MigrationPhase::Adopting, None);
                            // A live member's greeting may predate even its last
                            // admission. Its validated source anchor lets us re-admit
                            // against the target instead of looking up a transition
                            // out of that target. A greeting for a later root still
                            // needs the next historical proof after this projection.
                            let session = &self.sessions[&id];
                            let later_root = session.admitted_anchor != Some(source)
                                && session.peer.as_ref().ok_or(v1::ErrorCode::NotReady)?.anchor()? != source;
                            self.recovery_projected.push((id, Some(receipt), later_root));
                            self.merge_effects(vec![oink_network::pair_merge::Effect::Project])
                                .map_err(|_| v1::ErrorCode::StateConflict)?;
                        } else {
                            self.send(id, receipt)?;
                        }
                    }
                }
            }
            Message::ConfigPairEnded(ended) => {
                let group = ended.lookup()?.ok_or(v1::ErrorCode::InvalidChannel)?;
                let source = group_merge::anchor(&group)?;
                if self.lookups.get(&id).is_none_or(|(expected, _)| *expected != source) {
                    return Err(v1::ErrorCode::StateConflict.into());
                }
                self.lookups.remove(&id);
                self.migration(source, None, api::MigrationPhase::ProofWaiting, Some(ended.problem()?));
            }
            Message::ConfigPairReceipt(receipt) => {
                let pair = wire::read_pair(&receipt.pair_id()?)?;
                let recovery = self.recovery(pair).ok_or(v1::ErrorCode::StateConflict)?;
                let Decision::Committed(bundle) = &recovery.outcome else {
                    return Err(v1::ErrorCode::StateConflict.into());
                };
                if !self.sessions[&id].ready
                    || receipt.merge_id()? != bundle.certificate()?.intent()?.merge_id()?
                    || receipt.state_digest()? != bundle.certificate()?.result_digest()?
                {
                    return Err(v1::ErrorCode::StateConflict.into());
                }
                let expected_peer = if self.identity.id() == recovery.source {
                    recovery.target
                } else {
                    recovery.source
                };
                if peer.device_id != expected_peer || receipt.reply()? != (self.identity.id() == recovery.source) {
                    return Err(v1::ErrorCode::AuthenticationFailed.into());
                }
                self.merge_effects(vec![oink_network::pair_merge::Effect::Completed {
                    pair,
                    bundle: bundle.clone(),
                    reconciled: true,
                }])
                .map_err(|_| v1::ErrorCode::StateConflict)?;
                if !receipt.reply()? {
                    let mut raw = receipt.into_raw();
                    raw.insert(4, true.write());
                    self.send(id, Message::ConfigPairReceipt(dto::PairReceipt::from_raw(raw)?))?;
                }
            }
            Message::ConfigMergeReceipt(receipt) => {
                if wire::read_device(&receipt.adopter()?)? != peer.device_id {
                    return Err(v1::ErrorCode::AuthenticationFailed.into());
                }
            }
            _ => return Err(v1::ErrorCode::InvalidChannel.into()),
        }
        Ok(())
    }
    fn expect_proof(
        &mut self,
        link: LinkId,
        reference: dto::ObjectRef,
        context: dto::ObjectContext,
        purpose: Purpose,
    ) -> v1::Result<()> {
        if reference.kind()? != dto::ObjectKind::Transfer {
            return Err(v1::ErrorCode::MalformedFrame.into());
        }
        let key = (link, reference.digest()?.0);
        if let Some(old) = self.proof_expected.get(&key) {
            if old.reference == reference && old.context == context {
                return Ok(());
            }
            return Err(v1::ErrorCode::StateConflict.into());
        }
        if self.proof_expected.keys().filter(|(id, _)| *id == link).count() >= 2 {
            return Err(v1::ErrorCode::LimitExceeded.into());
        }
        self.proof_expected.insert(
            key,
            Expected {
                reference,
                context,
                purpose,
                deadline: Instant::now() + Duration::from_secs(10),
            },
        );
        Ok(())
    }
    fn send_proof(
        &mut self,
        link: LinkId,
        bundle: &dto::MergeTransferBundle,
        context: dto::ObjectContext,
        decision: Option<(PairId, [u8; 32])>,
    ) -> v1::Result<()> {
        let (reference, messages) = pair_objects::transfer(context, dto::ObjectKind::Transfer, bundle.raw())?;
        let header = if let Some((pair, intent)) = decision {
            Message::ConfigPairDecisionReply(dto::PairDecisionReply::from_raw(
                [
                    (1, wire::pair(pair).write()),
                    (2, RawValue::Bytes(intent.to_vec())),
                    (
                        3,
                        RawValue::Map(
                            [
                                (1, dto::DecisionKind::Committed.write()),
                                (2, RawValue::Map([(1, reference.write())].into())),
                            ]
                            .into(),
                        ),
                    ),
                ]
                .into(),
            )?)
        } else {
            Message::ConfigMergeTransfer(dto::MergeTransfer::from_raw([(1, reference.write())].into())?)
        };
        self.send(link, header)?;
        for message in messages {
            self.send(link, message)?;
        }
        Ok(())
    }
    pub(crate) fn recovery(&self, pair: PairId) -> Option<Recovery> {
        self.journal
            .recoveries()
            .ok()?
            .into_iter()
            .filter(|r| r.pair == pair)
            .max_by_key(|r| r.decision)
    }
    pub(crate) fn expire_recovery(&mut self, now: Instant) {
        if let Ok(recoveries) = self.journal.recoveries() {
            self.recovery_due.retain(|pair, _| {
                recoveries.iter().any(|r| {
                    r.pair == *pair
                        && !r.receipt
                        && !matches!(r.outcome, Decision::Aborted(_))
                        && self
                            .sessions
                            .values()
                            .any(|s| s.peer.is_some() && s.peer_boot.device_id == r.target)
                })
            });
        }
        self.proof_objects.expire(now);
        let expired = self
            .proof_expected
            .iter()
            .filter_map(|(key, value)| (now >= value.deadline).then_some(*key))
            .collect::<Vec<_>>();
        for key in expired {
            self.proof_objects.discard(key.0.0, key.1);
            if let Some(expected) = self.proof_expected.remove(&key) {
                match expected.purpose {
                    Purpose::Adoption(source) => self.migration(
                        source,
                        None,
                        api::MigrationPhase::ProofWaiting,
                        Some(pairing::problem(dto::PairCode::NetworkTimeout, dto::PairStage::Adopt)),
                    ),
                    Purpose::Decision { pair, .. } => {
                        self.recovery_due.insert(pair, now);
                    }
                }
            }
        }
        let expired = self
            .lookups
            .iter()
            .filter_map(|(id, (source, deadline))| (now >= *deadline).then_some((*id, *source)))
            .collect::<Vec<_>>();
        for (id, source) in expired {
            self.lookups.remove(&id);
            self.migration(
                source,
                None,
                api::MigrationPhase::ProofWaiting,
                Some(pairing::problem(dto::PairCode::NetworkTimeout, dto::PairStage::Adopt)),
            );
        }
    }
    pub(crate) fn reconcile_peer(&mut self, link: LinkId) -> v1::Result<()> {
        let peer = self.sessions[&link].peer_boot.device_id;
        let now = Instant::now();
        for recovery in self.journal.recoveries().map_err(|_| v1::ErrorCode::StateConflict)? {
            if recovery.receipt || recovery.target != peer || recovery.source != self.identity.id() {
                continue;
            }
            if self
                .pairs
                .view(recovery.pair)
                .is_some_and(|view| !matches!(view.phase, pairing::Phase::Ended | pairing::Phase::Completed))
            {
                continue;
            }
            if self.recovery_due.get(&recovery.pair).is_some_and(|at| now < *at) {
                continue;
            }
            match recovery.outcome {
                Decision::Pending | Decision::Unconfirmed | Decision::Committed(_) => {
                    self.send(
                        link,
                        Message::ConfigPairDecisionQuery(dto::PairDecisionQuery::from_raw(
                            [
                                (1, wire::pair(recovery.pair).write()),
                                (
                                    2,
                                    RawValue::Bytes(group_merge::digest(&recovery.plan.intent()?)?.to_vec()),
                                ),
                                (3, recovery.decision.write()),
                            ]
                            .into(),
                        )?),
                    )?;
                    self.recovery_due.insert(recovery.pair, now + Duration::from_secs(5));
                }
                Decision::Aborted(_) => {}
            }
        }
        Ok(())
    }
    pub(crate) fn finish_recovery_projection(&mut self) -> v1::Result<()> {
        for (id, receipt, follow_greeting) in std::mem::take(&mut self.recovery_projected) {
            if !self.sessions.contains_key(&id) {
                continue;
            }
            if let Some(receipt) = receipt {
                self.send(id, receipt)?;
            }
            // Sending a receipt can retire a full/closed control queue and
            // remove this session synchronously. Continue only on that lifetime.
            let Some(session) = self.sessions.get(&id) else {
                continue;
            };
            if !follow_greeting
                || session.peer.as_ref().ok_or(v1::ErrorCode::NotReady)?.anchor()? == self.state.roster().anchor()
            {
                self.begin_admission(id)?;
            } else {
                self.lookup(id)?;
            }
        }
        Ok(())
    }
    pub(crate) fn recovery_ready(&mut self, link: LinkId) -> v1::Result<()> {
        let peer = self.sessions[&link].peer_boot.device_id;
        for recovery in self.journal.recoveries().map_err(|_| v1::ErrorCode::StateConflict)? {
            if recovery.receipt
                || recovery.source != self.identity.id()
                || recovery.target != peer
                || self.transactions.contains(recovery.pair)
            {
                continue;
            }
            if let Decision::Committed(bundle) = recovery.outcome {
                let certificate = bundle.certificate()?;
                self.send(
                    link,
                    Message::ConfigPairReceipt(dto::PairReceipt::from_raw(
                        [
                            (1, wire::pair(recovery.pair).write()),
                            (2, certificate.intent()?.merge_id()?.write()),
                            (3, certificate.result_digest()?.write()),
                            (4, false.write()),
                        ]
                        .into(),
                    )?),
                )?;
            }
        }
        if let Some(status) = self.shared.migration.load().as_ref().as_ref()
            && status.phase == api::MigrationPhase::Adopting
        {
            let mut status = status.clone();
            status.phase = api::MigrationPhase::Adopted;
            self.shared.migration.publish(Some(status.clone()));
            self.shared
                .emit_pairing(api::PairingEvent::GroupMigrationChanged(status));
        }
        Ok(())
    }
    pub(crate) fn gossip_transition(&mut self, bundle: &dto::MergeTransferBundle) -> v1::Result<()> {
        let source = group_merge::anchor(&bundle.certificate()?.intent()?.source()?)?;
        let links = self
            .sessions
            .iter()
            .filter_map(|(id, session)| session.ready.then_some(*id))
            .collect::<Vec<_>>();
        for link in links {
            self.send_proof(link, bundle, group_context(source)?, None)?;
        }
        Ok(())
    }
}
