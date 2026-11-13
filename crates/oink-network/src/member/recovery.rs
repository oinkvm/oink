//! Old-root proof traffic and durable decision recovery. Both survive fresh
//! Boots: the journal, not a live pairing, is the authority.
use super::*;

pub(super) fn group_context(anchor: Anchor) -> v2::Result<dto::ObjectContext> {
    dto::ObjectContext::from_raw(
        [
            (1, dto::ObjectContextKind::GroupRef.write()),
            (2, RawValue::Map([(1, group_merge::group_ref(anchor).write())].into())),
        ]
        .into(),
    )
}

fn pair_context(pair: PairId) -> v2::Result<dto::ObjectContext> {
    dto::ObjectContext::from_raw(
        [
            (1, dto::ObjectContextKind::PairId.write()),
            (2, RawValue::Map([(1, wire::pair(pair).write())].into())),
        ]
        .into(),
    )
}

/// The header (MergeTransfer, or a Committed decision reply) and the object.
pub(super) fn proof(
    link: LinkId,
    bundle: &dto::MergeTransferBundle,
    context: dto::ObjectContext,
    decision: Option<(PairId, [u8; 32])>,
) -> v2::Result<Vec<Out>> {
    let (reference, messages) = pair_objects::transfer(context, dto::ObjectKind::Transfer, bundle.raw())?;
    let header = if let Some((pair, intent)) = decision {
        decision_reply(
            pair,
            intent,
            dto::DecisionKind::Committed,
            [(1, reference.write())].into(),
        )?
    } else {
        Message::ConfigMergeTransfer(dto::MergeTransfer::from_raw([(1, reference.write())].into())?)
    };
    Ok(std::iter::once(header)
        .chain(messages)
        .map(|message| Out::Send { link, message })
        .collect())
}

fn decision_reply(pair: PairId, intent: [u8; 32], kind: dto::DecisionKind, body: RawMap) -> v2::Result<Message> {
    Ok(Message::ConfigPairDecisionReply(dto::PairDecisionReply::from_raw(
        [
            (1, wire::pair(pair).write()),
            (2, RawValue::Bytes(intent.to_vec())),
            (3, RawValue::Map([(1, kind.write()), (2, RawValue::Map(body))].into())),
        ]
        .into(),
    )?))
}

impl Membership {
    /// Ask a known member that greeted with another root for the transition
    /// out of our current root.
    pub(super) fn lookup(
        &mut self,
        link: LinkId,
        facts: &LinkFacts,
        current: &State,
        now: Instant,
        out: &mut Vec<Out>,
    ) -> v2::Result<()> {
        if !current.roster().contains(facts.boot.device_id) {
            return Err(violation(v2::ErrorCode::AuthenticationFailed));
        }
        let source = current.roster().anchor();
        // Greeting recovery and a historical decision can request the same
        // proof concurrently: one reply slot per link and source root.
        if self.lookups.get(&link).is_some_and(|(pending, _)| *pending == source) {
            return Ok(());
        }
        self.lookups.insert(link, (source, now + PROOF_DEADLINE));
        if !facts.ready {
            out.push(Out::ArmAdmission { link });
        }
        out.push(Out::OldMemberLane { link });
        self.report(
            out,
            Migration {
                source,
                target: Some(facts.greeted),
                phase: MigrationPhase::ProofWaiting,
                problem: None,
            },
        );
        out.push(Out::Send {
            link,
            message: Message::ConfigMergeLookup(dto::MergeLookup::from_raw(
                [
                    (1, RawValue::Bytes(source.group.0.to_vec())),
                    (2, RawValue::Bytes(source.root.to_vec())),
                    (3, wire::device(self.identity.id()).write()),
                ]
                .into(),
            )?),
        });
        Ok(())
    }

    fn recovery(&self, pair: PairId) -> Option<Recovery> {
        self.journal
            .recoveries()
            .ok()?
            .into_iter()
            .filter(|r| r.pair == pair)
            .max_by_key(|r| r.decision)
    }

    #[allow(clippy::too_many_arguments)]
    fn expect_proof(
        &mut self,
        link: LinkId,
        facts: &LinkFacts,
        reference: dto::ObjectRef,
        context: dto::ObjectContext,
        purpose: Purpose,
        now: Instant,
        out: &mut Vec<Out>,
    ) -> v2::Result<()> {
        if reference.kind()? != dto::ObjectKind::Transfer {
            return Err(violation(v2::ErrorCode::MalformedFrame));
        }
        let key = (link, reference.digest()?.0);
        if let Some(old) = self.expected.get(&key) {
            if old.reference == reference && old.context == context {
                return Ok(());
            }
            return Err(violation(v2::ErrorCode::StateConflict));
        }
        if self.expected.keys().filter(|(id, _)| *id == link).count() >= 2 {
            return Err(violation(v2::ErrorCode::LimitExceeded));
        }
        if !facts.ready {
            out.push(Out::ArmAdmission { link });
        }
        self.expected.insert(
            key,
            Expected {
                reference,
                context,
                purpose,
                deadline: now + PROOF_DEADLINE,
            },
        );
        Ok(())
    }

    pub(super) fn recovery_message(
        &mut self,
        link: LinkId,
        facts: &LinkFacts,
        message: Message,
        current: &State,
        now: Instant,
    ) -> v2::Result<Vec<Out>> {
        let peer = facts.boot;
        let local = self.identity.id();
        let mut out = Vec::new();
        match message {
            Message::ConfigMergeLookup(lookup) => {
                if wire::read_device(&lookup.requester()?)? != peer.device_id {
                    return Err(violation(v2::ErrorCode::AuthenticationFailed));
                }
                let source = Anchor {
                    group: oink_foundation::GroupId(lookup.source_group()?.0),
                    root: lookup.source_root()?.0,
                };
                let after = lookup.after()?.map(|h| h.0);
                match self.journal.lookup(peer.device_id, source, after) {
                    Ok(Some(bundle)) => out.extend(proof(link, &bundle, group_context(source)?, None)?),
                    Ok(None)
                        if source == current.roster().anchor()
                            && after.is_none()
                            && current.roster().contains(peer.device_id) =>
                    {
                        // A greeting can predate a concurrently applied
                        // migration: the peer asks about the root we already
                        // share. Resume ordinary signed admission.
                        out.push(Out::BeginAdmission { link });
                    }
                    result => {
                        let code = if matches!(result, Err(pair_journal::Error::Conflict)) {
                            dto::PairCode::MergeConflict
                        } else {
                            dto::PairCode::MergeProofUnavailable
                        };
                        out.push(Out::Send {
                            link,
                            message: Message::ConfigPairEnded(dto::PairEnded::from_raw(
                                [
                                    (2, pairing::problem(code, dto::PairStage::Adopt).write()),
                                    (4, group_merge::group_ref(source).write()),
                                ]
                                .into(),
                            )?),
                        });
                    }
                }
            }
            Message::ConfigMergeTransfer(transfer) => {
                let reference = transfer.bundle()?;
                if reference.kind()? != dto::ObjectKind::Transfer {
                    return Err(violation(v2::ErrorCode::MalformedFrame));
                }
                // Our own lookup, or member gossip on a C0 admitted under the
                // anchor it is leaving. A bare greeting grants neither.
                let source = match self.lookups.get(&link) {
                    Some((source, _)) => *source,
                    None => facts.admitted.ok_or(v2::ErrorCode::NotReady)?,
                };
                self.expect_proof(
                    link,
                    facts,
                    reference,
                    group_context(source)?,
                    Purpose::Adoption(source),
                    now,
                    &mut out,
                )?;
            }
            Message::ConfigPairDecisionQuery(query) => {
                let pair = wire::read_pair(&query.pair_id()?)?;
                let decision = query.permit_decision()?;
                let intent = query.intent_digest()?.0;
                let known = self
                    .journal
                    .recoveries()
                    .map_err(journal_error)?
                    .into_iter()
                    .find(|r| r.pair == pair && r.decision == decision);
                if known
                    .as_ref()
                    .is_none_or(|r| r.target != local || r.source != peer.device_id)
                {
                    return Err(violation(v2::ErrorCode::AuthenticationFailed));
                }
                match self
                    .journal
                    .query(peer.device_id, pair, intent, decision)
                    .map_err(journal_error)?
                {
                    Decision::Committed(bundle) => {
                        out.extend(proof(link, &bundle, pair_context(pair)?, Some((pair, intent)))?)
                    }
                    Decision::Aborted(problem) => out.push(Out::Send {
                        link,
                        message: decision_reply(
                            pair,
                            intent,
                            dto::DecisionKind::Aborted,
                            [(1, problem.write())].into(),
                        )?,
                    }),
                    Decision::Pending | Decision::Unconfirmed => out.push(Out::Send {
                        link,
                        message: decision_reply(pair, intent, dto::DecisionKind::Pending, RawMap::new())?,
                    }),
                }
            }
            Message::ConfigPairDecisionReply(reply) => {
                let pair = wire::read_pair(&reply.pair_id()?)?;
                let recovery = self.recovery(pair).ok_or(v2::ErrorCode::AuthenticationFailed)?;
                if recovery.source != local
                    || recovery.target != peer.device_id
                    || group_merge::digest(&recovery.plan.intent()?)? != reply.intent_digest()?.0
                {
                    return Err(violation(v2::ErrorCode::AuthenticationFailed));
                }
                let outcome = reply.outcome()?;
                match outcome.variant()? {
                    v2::schema::Open::Known(dto::DecisionKind::Committed) => {
                        let reference = dto::DecisionCommitted::from_raw(outcome.body()?)?.bundle()?;
                        self.expect_proof(
                            link,
                            facts,
                            reference,
                            pair_context(pair)?,
                            Purpose::Decision {
                                pair,
                                decision: recovery.decision,
                            },
                            now,
                            &mut out,
                        )?;
                    }
                    v2::schema::Open::Known(dto::DecisionKind::Aborted) => {
                        let problem = dto::DecisionAborted::from_raw(outcome.body()?)?.problem()?;
                        self.journal
                            .resolved_abort(pair, recovery.decision, peer.device_id, problem.clone())
                            .map_err(journal_error)?;
                        out.push(Out::RecoveredFailure { recovery, problem });
                        out.extend(self.retry_permits(current));
                    }
                    // The target still runs that pairing; asked again later.
                    v2::schema::Open::Known(dto::DecisionKind::Pending) => {}
                    _ => return Err(violation(v2::ErrorCode::UnsupportedValue)),
                }
            }
            Message::ConfigPairObjectBegin(begin) => {
                let reference = begin.object()?;
                let expected = self
                    .expected
                    .get(&(link, reference.digest()?.0))
                    .cloned()
                    .ok_or(v2::ErrorCode::AuthenticationFailed)?;
                if reference != expected.reference {
                    return Err(violation(v2::ErrorCode::AuthenticationFailed));
                }
                let mut allowed = begin.context()? == expected.context;
                // Member gossip names its signed source in the object context;
                // execution still waits for the target-root checks.
                if !allowed && matches!(expected.purpose, Purpose::Adoption(_)) && facts.admitted.is_some() {
                    allowed = begin.context()?.variant()? == v2::schema::Open::Known(dto::ObjectContextKind::GroupRef);
                }
                self.objects.begin(
                    link.0,
                    begin,
                    |_, kind| allowed && kind == dto::ObjectKind::Transfer,
                    now,
                )?;
            }
            Message::ConfigPairObjectChunk(chunk) => {
                let digest = chunk.digest()?.0;
                self.objects.chunk(link.0, chunk, now)?;
                // Staging accepts only a new, nonempty, contiguous chunk: a
                // replay cannot extend an otherwise stalled lifetime.
                if !facts.ready {
                    out.push(Out::ArmAdmission { link });
                }
                if let Some(expected) = self.expected.get_mut(&(link, digest)) {
                    expected.deadline = now + PROOF_DEADLINE;
                }
            }
            Message::ConfigPairObjectEnd(end) => {
                let complete = self.objects.end(link.0, end)?;
                let expected = self
                    .expected
                    .remove(&(link, complete.reference.digest()?.0))
                    .ok_or(v2::ErrorCode::StateConflict)?;
                let bundle = dto::MergeTransferBundle::from_raw(complete.raw)?;
                let intent = bundle.certificate()?.intent()?;
                let (source, target) = crate::legacy_merge::anchors(&bundle)?;
                match expected.purpose {
                    Purpose::Decision { pair, decision } => {
                        let recovery = self.recovery(pair).ok_or(v2::ErrorCode::AuthenticationFailed)?;
                        if recovery.target != peer.device_id
                            || recovery.source != local
                            || bundle.plan() != Ok(recovery.plan.clone())
                            || bundle.certificate()?.permit_decision()? != decision
                        {
                            return Err(violation(v2::ErrorCode::AuthenticationFailed));
                        }
                        self.journal.commit(&bundle, current).map_err(journal_error)?;
                        // A committed decision can outlive the peer's group: its
                        // old proof does not authorize admission to an unrelated
                        // current root; follow the greeting.
                        self.followups.push(Followup {
                            link,
                            receipt: None,
                            follow_greeting: true,
                        });
                        out.extend(self.merge_effects(vec![pair_merge::Effect::Project], current));
                    }
                    Purpose::Adoption(expected_source) => {
                        let anchor = current.roster().anchor();
                        if self.lookups.contains_key(&link) && source != expected_source {
                            return Err(violation(v2::ErrorCode::AuthenticationFailed));
                        }
                        if source != anchor && target != anchor {
                            return Err(violation(v2::ErrorCode::AuthenticationFailed));
                        }
                        crate::legacy_merge::verify_any(&bundle)?;
                        if self.journal.history_conflict(&bundle).map_err(journal_error)? {
                            // Two transitions out of one root (D9): kept as
                            // evidence only; a person re-pairs the device.
                            self.journal.adopt(&bundle, current, false).map_err(journal_error)?;
                            self.report(
                                &mut out,
                                Migration {
                                    source,
                                    target: Some(target),
                                    phase: MigrationPhase::Conflict,
                                    problem: Some(pairing::problem(
                                        dto::PairCode::MergeConflict,
                                        dto::PairStage::Adopt,
                                    )),
                                },
                            );
                            self.lookups.remove(&link);
                            return Ok(out);
                        }
                        let apply = source == anchor;
                        self.journal.adopt(&bundle, current, apply).map_err(journal_error)?;
                        let looked_up = self.lookups.remove(&link).is_some();
                        let receipt = Message::ConfigMergeReceipt(dto::MergeReceipt::from_raw(
                            [(1, intent.merge_id()?.write()), (2, wire::device(local).write())].into(),
                        )?);
                        // Another recovery can project the target while this
                        // C0's lookup reply is in flight: it still needs
                        // admission. Unsolicited repeats of installed history
                        // do not restart admission.
                        if apply || (looked_up && !facts.ready) {
                            self.report(
                                &mut out,
                                Migration {
                                    source,
                                    target: Some(target),
                                    phase: MigrationPhase::Adopting,
                                    problem: None,
                                },
                            );
                            // A greeting for a later root still needs the next
                            // historical proof after this projection.
                            let later_root = facts.admitted != Some(source) && facts.greeted != source;
                            self.followups.push(Followup {
                                link,
                                receipt: Some(receipt),
                                follow_greeting: later_root,
                            });
                            out.extend(self.merge_effects(vec![pair_merge::Effect::Project], current));
                        } else {
                            out.push(Out::Send { link, message: receipt });
                        }
                    }
                }
            }
            Message::ConfigPairEnded(ended) => {
                let group = ended.lookup()?.ok_or(v2::ErrorCode::InvalidChannel)?;
                let source = group_merge::anchor(&group)?;
                if self.lookups.get(&link).is_none_or(|(expected, _)| *expected != source) {
                    return Err(violation(v2::ErrorCode::StateConflict));
                }
                self.lookups.remove(&link);
                let problem = ended.problem()?;
                self.report(
                    &mut out,
                    Migration {
                        source,
                        target: None,
                        phase: MigrationPhase::ProofWaiting,
                        problem: Some(problem),
                    },
                );
            }
            Message::ConfigPairReceipt(receipt) => {
                let pair = wire::read_pair(&receipt.pair_id()?)?;
                let recovery = self.recovery(pair).ok_or(v2::ErrorCode::StateConflict)?;
                let Decision::Committed(bundle) = &recovery.outcome else {
                    return Err(violation(v2::ErrorCode::StateConflict));
                };
                if !facts.ready
                    || receipt.merge_id()? != bundle.certificate()?.intent()?.merge_id()?
                    || receipt.state_digest()? != bundle.certificate()?.result_digest()?
                {
                    return Err(violation(v2::ErrorCode::StateConflict));
                }
                let expected_peer = if local == recovery.source {
                    recovery.target
                } else {
                    recovery.source
                };
                if peer.device_id != expected_peer || receipt.reply()? != (local == recovery.source) {
                    return Err(violation(v2::ErrorCode::AuthenticationFailed));
                }
                out.push(Out::Completed {
                    pair,
                    bundle: bundle.clone(),
                    reconciled: true,
                });
                if !receipt.reply()? {
                    let mut raw = receipt.into_raw();
                    raw.insert(4, true.write());
                    out.push(Out::Send {
                        link,
                        message: Message::ConfigPairReceipt(dto::PairReceipt::from_raw(raw)?),
                    });
                }
            }
            Message::ConfigMergeReceipt(receipt) => {
                if wire::read_device(&receipt.adopter()?)? != peer.device_id {
                    return Err(violation(v2::ErrorCode::AuthenticationFailed));
                }
            }
            _ => return Err(violation(v2::ErrorCode::InvalidChannel)),
        }
        Ok(out)
    }

    pub(super) fn expire_recovery(&mut self, now: Instant, links: &dyn Links, out: &mut Vec<Out>) {
        if !self.recovery_due.is_empty()
            && let Ok(recoveries) = self.journal.recoveries()
        {
            let greeted = links.greeted();
            self.recovery_due.retain(|pair, _| {
                recoveries.iter().any(|r| {
                    r.pair == *pair
                        && !r.receipt
                        && !matches!(r.outcome, Decision::Aborted(_))
                        && greeted.iter().any(|(_, facts)| facts.boot.device_id == r.target)
                })
            });
        }
        self.objects.expire(now);
        let expired = self
            .expected
            .iter()
            .filter_map(|(key, value)| (now >= value.deadline).then_some(*key))
            .collect::<Vec<_>>();
        for key in expired {
            self.objects.discard(key.0.0, key.1);
            if let Some(expected) = self.expected.remove(&key) {
                match expected.purpose {
                    Purpose::Adoption(source) => self.report(
                        out,
                        Migration {
                            source,
                            target: None,
                            phase: MigrationPhase::ProofWaiting,
                            problem: Some(pairing::problem(dto::PairCode::NetworkTimeout, dto::PairStage::Adopt)),
                        },
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
            self.report(
                out,
                Migration {
                    source,
                    target: None,
                    phase: MigrationPhase::ProofWaiting,
                    problem: Some(pairing::problem(dto::PairCode::NetworkTimeout, dto::PairStage::Adopt)),
                },
            );
        }
    }

    /// Query each unreceipted commit decision this owner authorized as source
    /// toward this peer. The target is authoritative.
    pub(super) fn reconcile_peer(
        &mut self,
        link: LinkId,
        facts: &LinkFacts,
        now: Instant,
        out: &mut Vec<Out>,
    ) -> v2::Result<()> {
        for recovery in self
            .journal
            .unreceipted_recoveries(facts.boot.device_id)
            .map_err(journal_error)?
        {
            if self
                .pairs
                .view(recovery.pair)
                .is_some_and(|view| !matches!(view.phase, Phase::Ended | Phase::Completed))
            {
                continue;
            }
            if self.recovery_due.get(&recovery.pair).is_some_and(|at| now < *at) {
                continue;
            }
            match recovery.outcome {
                Decision::Pending | Decision::Unconfirmed | Decision::Committed(_) => {
                    out.push(Out::Send {
                        link,
                        message: Message::ConfigPairDecisionQuery(dto::PairDecisionQuery::from_raw(
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
                    });
                    self.recovery_due.insert(recovery.pair, now + REQUERY);
                }
                Decision::Aborted(_) => {}
            }
        }
        Ok(())
    }

    /// After signed admission: receipts for decisions recovered as source.
    pub(super) fn recovery_ready(&mut self, link: LinkId, peer: DeviceId, out: &mut Vec<Out>) -> v2::Result<()> {
        for recovery in self.journal.recoveries().map_err(journal_error)? {
            if recovery.receipt
                || recovery.source != self.identity.id()
                || recovery.target != peer
                || self.transactions.contains(recovery.pair)
            {
                continue;
            }
            if let Decision::Committed(bundle) = recovery.outcome {
                let certificate = bundle.certificate()?;
                out.push(Out::Send {
                    link,
                    message: Message::ConfigPairReceipt(dto::PairReceipt::from_raw(
                        [
                            (1, wire::pair(recovery.pair).write()),
                            (2, certificate.intent()?.merge_id()?.write()),
                            (3, certificate.result_digest()?.write()),
                            (4, false.write()),
                        ]
                        .into(),
                    )?),
                });
            }
        }
        Ok(())
    }
}
