//! Verified-pair transaction owner. Durable decisions precede projection and
//! transmission; callers serialize this with membership edits and C0 admission.
use crate::{
    group_merge as merge,
    group_state::State,
    identity::Identity,
    pair_journal::{Journal, Permit, Role},
    pair_objects::{self, Objects},
    pairing::{Direction, View},
    wire_identity as wire,
};
use oink_foundation::{
    DeviceId,
    session::{Boot, PairId},
};
use oink_protocol::v1::{
    self, Message, RawMap, RawValue, dto,
    schema::{WireDto, WireValue},
};
use std::{collections::HashMap, time::Instant};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid merge protocol: {0}")]
    Wire(#[from] v1::Error),
    #[error("merge decision persistence failed: {0}")]
    Journal(#[from] crate::pair_journal::Error),
}
type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub enum Effect {
    Send {
        peer: Boot,
        message: Message,
    },
    /// Replay and durably mark Journal::pending_projection before sending the
    /// subsequent effects. The durable decision has already happened.
    Project,
    Admit {
        pair: PairId,
        peer: Boot,
    },
    Completed {
        pair: PairId,
        bundle: dto::MergeTransferBundle,
        reconciled: bool,
    },
}
#[derive(Debug)]
struct Flow {
    view: View,
    source: State,
    seed: Option<dto::MergeSeed>,
    plan: Option<dto::MergePlan>,
    permit: Option<Permit>,
    bundle: Option<dto::MergeTransferBundle>,
    expected: Option<dto::ObjectRef>,
    peer_ready: bool,
    admitted: bool,
}
#[derive(Debug)]
pub enum Cancel {
    Cancelled,
    Unconfirmed,
    Committed(dto::MergeTransferBundle),
}
#[derive(Debug, Default)]
pub struct Transactions {
    flows: HashMap<PairId, Flow>,
    objects: Objects,
    completed: std::collections::VecDeque<PairId>,
}
fn conflict() -> Error {
    v1::ErrorCode::StateConflict.into()
}
impl From<v1::ErrorCode> for Error {
    fn from(value: v1::ErrorCode) -> Self {
        Self::Wire(value.into())
    }
}
fn send(flow: &Flow, message: Message) -> Effect {
    Effect::Send {
        peer: flow.view.peer,
        message,
    }
}
fn context(pair: PairId) -> v1::Result<dto::ObjectContext> {
    dto::ObjectContext::from_raw(
        [
            (1, dto::ObjectContextKind::PairId.write()),
            (2, RawValue::Map([(1, wire::pair(pair).write())].into())),
        ]
        .into(),
    )
}
fn object_pair(context: &dto::ObjectContext) -> v1::Result<PairId> {
    if context.variant()? != v1::schema::Open::Known(dto::ObjectContextKind::PairId) {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    wire::read_pair(&dto::ObjectPairContext::from_raw(context.body()?)?.pair_id()?)
}
fn transfer<T: WireDto>(pair: PairId, kind: dto::ObjectKind, value: &T) -> v1::Result<(dto::ObjectRef, Vec<Message>)> {
    pair_objects::transfer(context(pair)?, kind, value.raw())
}
impl Transactions {
    pub fn verified(&mut self, view: View, state: &State, journal: &mut Journal) -> Result<Vec<Effect>> {
        if self.flows.contains_key(&view.id) {
            return Ok(Vec::new());
        }
        let mut flow = Flow {
            view,
            source: state.clone(),
            seed: None,
            plan: None,
            permit: None,
            bundle: None,
            expected: None,
            peer_ready: false,
            admitted: false,
        };
        let mut effects = Vec::new();
        if flow.view.direction == Direction::Outgoing {
            let target = crate::roster::Anchor {
                group: oink_foundation::GroupId(flow.view.context.target_group()?.0),
                root: flow.view.context.target_root_id()?.0,
            };
            let sequence = journal.next_source_sequence(state.roster().anchor())?;
            let seed = merge::seed(state, target, sequence)?;
            let (reference, messages) = transfer(flow.view.id, dto::ObjectKind::SourceState, state.wire())?;
            effects.push(send(
                &flow,
                Message::ConfigPairPrepare(dto::PairPrepare::from_raw(
                    [
                        (1, wire::pair(flow.view.id).write()),
                        (2, seed.write()),
                        (3, reference.write()),
                    ]
                    .into(),
                )?),
            ));
            effects.extend(messages.into_iter().map(|message| send(&flow, message)));
            flow.seed = Some(seed);
        }
        self.flows.insert(flow.view.id, flow);
        Ok(effects)
    }
    fn aborted_reply(flow: &Flow, permit: &Permit) -> Result<Effect> {
        let reason = crate::pairing::problem(dto::PairCode::StateChanged, dto::PairStage::Prepare);
        Ok(send(
            flow,
            Message::ConfigPairDecisionReply(dto::PairDecisionReply::from_raw(
                [
                    (1, wire::pair(flow.view.id).write()),
                    (2, RawValue::Bytes(permit.intent.to_vec())),
                    (
                        3,
                        RawValue::Map(
                            [
                                (1, dto::DecisionKind::Aborted.write()),
                                (2, RawValue::Map([(1, reason.write())].into())),
                            ]
                            .into(),
                        ),
                    ),
                ]
                .into(),
            )?),
        ))
    }
    fn fence_target(flow: &mut Flow, journal: &mut Journal) -> Result<Vec<Effect>> {
        let mut effects = Vec::new();
        if let Some(permit) = &flow.permit {
            journal.abort(
                flow.view.id,
                permit.decision,
                crate::pairing::problem(dto::PairCode::StateChanged, dto::PairStage::Prepare),
            )?;
            effects.push(Self::aborted_reply(flow, permit)?);
        }
        flow.permit = None;
        flow.peer_ready = false;
        Ok(effects)
    }
    fn restart_source(flow: &mut Flow, state: &State, journal: &mut Journal) -> Result<Vec<Effect>> {
        if state.roster().anchor() != flow.source.roster().anchor() {
            return Err(conflict());
        }
        if let Some(permit) = &flow.permit {
            journal.abort(
                flow.view.id,
                permit.decision,
                crate::pairing::problem(dto::PairCode::StateChanged, dto::PairStage::Prepare),
            )?;
        }
        let target = crate::roster::Anchor {
            group: oink_foundation::GroupId(flow.view.context.target_group()?.0),
            root: flow.view.context.target_root_id()?.0,
        };
        let seed = merge::seed(state, target, journal.next_source_sequence(state.roster().anchor())?)?;
        let (reference, messages) = transfer(flow.view.id, dto::ObjectKind::SourceState, state.wire())?;
        let mut effects = vec![send(
            flow,
            Message::ConfigPairPrepare(dto::PairPrepare::from_raw(
                [
                    (1, wire::pair(flow.view.id).write()),
                    (2, seed.write()),
                    (3, reference.write()),
                ]
                .into(),
            )?),
        )];
        effects.extend(messages.into_iter().map(|message| send(flow, message)));
        flow.source = state.clone();
        flow.seed = Some(seed);
        flow.plan = None;
        flow.permit = None;
        flow.peer_ready = false;
        Ok(effects)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn receive(
        &mut self,
        link: u64,
        peer: Boot,
        message: Message,
        identity: &Identity,
        state: &State,
        journal: &mut Journal,
        now: Instant,
        now_ms: u64,
    ) -> Result<Vec<Effect>> {
        match message {
            Message::ConfigPairObjectBegin(begin) => {
                let pair = object_pair(&begin.context()?)?;
                let flow = self.flows.get(&pair).ok_or_else(conflict)?;
                let allowed = flow.view.peer == peer && flow.expected.as_ref() == Some(&begin.object()?);
                self.objects.begin(link, begin, |_, _| allowed, now)?;
                Ok(Vec::new())
            }
            Message::ConfigPairObjectChunk(chunk) => {
                self.objects.chunk(link, chunk, now)?;
                Ok(Vec::new())
            }
            Message::ConfigPairObjectEnd(end) => {
                let complete = self.objects.end(link, end)?;
                let pair = object_pair(&complete.context)?;
                let flow = self.flows.get_mut(&pair).ok_or_else(conflict)?;
                if flow.view.peer != peer || flow.expected.as_ref() != Some(&complete.reference) {
                    return Err(conflict());
                }
                flow.expected = None;
                Self::object(
                    flow,
                    complete.reference.kind()?,
                    complete.raw,
                    identity,
                    state,
                    journal,
                    now_ms,
                )
            }
            Message::ConfigPairPrepare(prepare) => {
                let pair = wire::read_pair(&prepare.pair_id()?)?;
                let flow = self.flow(pair, peer)?;
                if flow.view.direction != Direction::Incoming
                    || prepare.source()?.kind()? != dto::ObjectKind::SourceState
                {
                    return Err(conflict());
                }
                if let Some(bundle) = &flow.bundle {
                    return Self::committed(flow, bundle);
                }
                let seed = prepare.seed()?;
                let changed = flow.seed.as_ref().is_some_and(|old| *old != seed);
                let effects = if changed {
                    Self::fence_target(flow, journal)?
                } else {
                    Vec::new()
                };
                if changed {
                    flow.plan = None;
                }
                flow.seed = Some(seed);
                flow.expected = Some(prepare.source()?);
                Ok(effects)
            }
            Message::ConfigPairReady(ready) => {
                let pair = wire::read_pair(&ready.pair_id()?)?;
                let flow = self.flow(pair, peer)?;
                if ready.plan()?.kind()? != dto::ObjectKind::Plan {
                    return Err(conflict());
                }
                if flow.view.direction == Direction::Incoming {
                    let plan = flow.plan.as_ref().ok_or_else(conflict)?;
                    if plan.intent()? != ready.intent()? || merge::digest(plan)? != ready.plan()?.digest()?.0 {
                        return Ok(Vec::new()); // An earlier plan echo cannot ready a rebased candidate.
                    }
                    flow.peer_ready = true;
                    return Self::grant(flow, identity.id(), journal);
                }
                if ready.intent()?.source_state_digest()?.0
                    != flow.seed.as_ref().ok_or_else(conflict)?.source_state_digest()?.0
                {
                    return Ok(Vec::new());
                }
                if let Some(plan) = &flow.plan {
                    if (plan.intent()? != ready.intent()? || merge::digest(plan)? != ready.plan()?.digest()?.0)
                        && flow.permit.is_some()
                    {
                        return Err(conflict());
                    }
                    flow.expected = Some(ready.plan()?);
                    return Ok(Vec::new());
                }
                // The complete plan also binds this intent, checked on assembly.
                if ready.intent()?.pair_id()? != wire::pair(pair) {
                    return Err(conflict());
                }
                flow.expected = Some(ready.plan()?);
                Ok(Vec::new())
            }
            Message::ConfigPairCommitPermit(value) => {
                let pair = wire::read_pair(&value.pair_id()?)?;
                let flow = self.flow(pair, peer)?;
                let plan = flow.plan.as_ref().ok_or_else(conflict)?;
                if peer.device_id != identity.id().min(peer.device_id)
                    || value.intent_digest()?.0 != merge::digest(&plan.intent()?)?
                    || value.plan_digest()?.0 != merge::digest(plan)?
                {
                    return Err(conflict());
                }
                let permit = Permit {
                    pair,
                    intent: value.intent_digest()?.0,
                    plan: value.plan_digest()?.0,
                    decision: value.decision()?,
                };
                if flow
                    .permit
                    .as_ref()
                    .is_some_and(|old| old.decision != permit.decision || old.plan != permit.plan)
                {
                    return Err(conflict());
                }
                let role = if flow.view.direction == Direction::Outgoing {
                    Role::Source
                } else {
                    Role::Target
                };
                journal.prepare(
                    plan,
                    permit.decision,
                    role,
                    (role == Role::Source).then_some(flow.source.wire()),
                )?;
                flow.permit = Some(permit);
                Self::authorize(flow, identity, state, journal)
            }
            Message::ConfigPairCommit(value) => {
                let pair = wire::read_pair(&value.pair_id()?)?;
                let flow = self.flow(pair, peer)?;
                if flow.view.direction != Direction::Incoming {
                    return Err(conflict());
                }
                if let Ok(crate::pair_journal::Decision::Aborted(_)) =
                    journal.decision(peer.device_id, pair, value.intent_digest()?.0, value.permit_decision()?)
                {
                    let old = journal
                        .recoveries()?
                        .into_iter()
                        .find(|r| r.pair == pair && r.decision == value.permit_decision().unwrap_or(0))
                        .ok_or_else(conflict)?;
                    let permit = Permit {
                        pair,
                        decision: old.decision,
                        intent: merge::digest(&old.plan.intent()?)?,
                        plan: merge::digest(&old.plan)?,
                    };
                    if value.intent_digest()?.0 != permit.intent || value.plan_digest()?.0 != permit.plan {
                        return Err(conflict());
                    }
                    return Ok(vec![Self::aborted_reply(flow, &permit)?]);
                }
                let permit = flow.permit.as_ref().ok_or_else(conflict)?;
                if value.permit_decision()? != permit.decision
                    || value.intent_digest()?.0 != permit.intent
                    || value.plan_digest()?.0 != permit.plan
                {
                    return Err(conflict());
                }
                if let Some(bundle) = &flow.bundle {
                    return Self::committed(flow, bundle);
                }
                if flow
                    .plan
                    .as_ref()
                    .ok_or_else(conflict)?
                    .intent()?
                    .target_base_digest()?
                    .0
                    != state.digest()?
                {
                    let mut effects = Self::fence_target(flow, journal)?;
                    let plan = merge::prepare(
                        &flow.source,
                        state,
                        &flow.view.context,
                        flow.seed.as_ref().ok_or_else(conflict)?,
                        now_ms,
                    )?;
                    flow.plan = Some(plan);
                    effects.extend(Self::ready(flow)?);
                    return Ok(effects);
                }
                let permit = flow.permit.as_ref().ok_or_else(conflict)?;
                let bundle = merge::commit(
                    identity,
                    flow.source.roster(),
                    state,
                    flow.plan.as_ref().ok_or_else(conflict)?,
                    value.source_authorization()?,
                    permit.decision,
                )?;
                journal.commit(&bundle, state)?;
                let mut effects = vec![Effect::Project];
                effects.extend(Self::committed(flow, &bundle)?);
                flow.bundle = Some(bundle);
                Ok(effects)
            }
            Message::ConfigPairCommitted(value) => {
                let pair = wire::read_pair(&value.pair_id()?)?;
                let flow = self.flow(pair, peer)?;
                if flow.view.direction != Direction::Outgoing || value.transfer()?.kind()? != dto::ObjectKind::Transfer
                {
                    return Err(conflict());
                }
                flow.expected = Some(value.transfer()?);
                Ok(Vec::new())
            }
            Message::ConfigPairDecisionReply(reply) => {
                let pair = wire::read_pair(&reply.pair_id()?)?;
                let flow = self.flow(pair, peer)?;
                if flow.view.direction != Direction::Outgoing {
                    return Err(conflict());
                }
                let outcome = reply.outcome()?;
                if outcome.variant()? != v1::schema::Open::Known(dto::DecisionKind::Aborted) {
                    return Err(conflict());
                }
                let reason = dto::DecisionAborted::from_raw(outcome.body()?)?.problem()?;
                if reason.code()? != dto::PairCode::StateChanged {
                    return Err(conflict());
                }
                let old = journal
                    .recoveries()?
                    .into_iter()
                    .find(|r| {
                        r.pair == pair
                            && merge::digest(&r.plan.intent().expect("validated plan")).ok()
                                == Some(reply.intent_digest().expect("validated reply").0)
                    })
                    .ok_or_else(conflict)?;
                journal.resolved_abort(pair, old.decision, peer.device_id, reason)?;
                if flow.permit.as_ref().is_some_and(|p| p.decision == old.decision) {
                    flow.permit = None;
                    flow.plan = None;
                    flow.peer_ready = false;
                }
                Ok(Vec::new())
            }
            Message::ConfigPairReceipt(receipt) => {
                let pair = wire::read_pair(&receipt.pair_id()?)?;
                let completed = self.completed.contains(&pair);
                let flow = self.flow(pair, peer)?;
                let bundle = flow.bundle.as_ref().ok_or_else(conflict)?;
                if !flow.admitted
                    || receipt.merge_id()? != bundle.certificate()?.intent()?.merge_id()?
                    || receipt.state_digest()? != bundle.certificate()?.result_digest()?
                {
                    return Err(conflict());
                }
                let reply = receipt.reply()?;
                if reply != (flow.view.direction == Direction::Outgoing) {
                    return Err(conflict());
                }
                // A repeated receipt still needs its acknowledgement, but must
                // not repeat completion side effects (including superseding a
                // newer pairing) after the owner durably finished this pair.
                let mut effects = Vec::new();
                if !completed {
                    effects.push(Effect::Completed {
                        pair,
                        bundle: bundle.clone(),
                        reconciled: false,
                    });
                }
                if !reply {
                    effects.push(send(flow, Self::receipt(flow, true)?));
                }
                Ok(effects)
            }
            _ => Err(v1::ErrorCode::InvalidChannel.into()),
        }
    }
    fn flow(&mut self, pair: PairId, peer: Boot) -> Result<&mut Flow> {
        let flow = self.flows.get_mut(&pair).ok_or_else(conflict)?;
        if flow.view.peer != peer {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
        Ok(flow)
    }
    fn object(
        flow: &mut Flow,
        kind: dto::ObjectKind,
        raw: RawMap,
        identity: &Identity,
        state: &State,
        journal: &mut Journal,
        now: u64,
    ) -> Result<Vec<Effect>> {
        match kind {
            dto::ObjectKind::SourceState if flow.view.direction == Direction::Incoming => {
                let seed = flow.seed.as_ref().ok_or_else(conflict)?;
                let source = State::verify(dto::GroupState::from_raw(raw)?, merge::anchor(&seed.source()?)?)?;
                let plan = merge::prepare(&source, state, &flow.view.context, seed, now)?;
                flow.source = source;
                flow.plan = Some(plan);
                Self::ready(flow)
            }
            dto::ObjectKind::Plan if flow.view.direction == Direction::Outgoing => {
                if flow.bundle.is_none() && state.digest()? != flow.source.digest()? {
                    return Self::restart_source(flow, state, journal);
                }
                let plan = dto::MergePlan::from_raw(raw)?;
                merge::verify_plan(
                    &flow.source,
                    &flow.view.context,
                    flow.seed.as_ref().ok_or_else(conflict)?,
                    &plan,
                )?;
                if let Some(existing) = &flow.plan {
                    if *existing == plan {
                        return Self::authorize(flow, identity, state, journal);
                    }
                    if flow.permit.is_some() {
                        return Err(conflict());
                    }
                }
                flow.plan = Some(plan);
                flow.peer_ready = true;
                let mut effects = Self::ready(flow)?;
                effects.extend(Self::grant(flow, identity.id(), journal)?);
                effects.extend(Self::authorize(flow, identity, state, journal)?);
                Ok(effects)
            }
            dto::ObjectKind::Transfer if flow.view.direction == Direction::Outgoing => {
                let bundle = dto::MergeTransferBundle::from_raw(raw)?;
                if bundle.plan()? != *flow.plan.as_ref().ok_or_else(conflict)?
                    || bundle.certificate()?.permit_decision()? != flow.permit.as_ref().ok_or_else(conflict)?.decision
                {
                    return Err(conflict());
                }
                if let Some(existing) = &flow.bundle {
                    if *existing != bundle {
                        return Err(conflict());
                    }
                    return if flow.admitted {
                        Ok(vec![send(flow, Self::receipt(flow, false)?)])
                    } else {
                        Ok(vec![Effect::Admit {
                            pair: flow.view.id,
                            peer: flow.view.peer,
                        }])
                    };
                }
                journal.commit(&bundle, state)?;
                flow.bundle = Some(bundle);
                Ok(vec![
                    Effect::Project,
                    Effect::Admit {
                        pair: flow.view.id,
                        peer: flow.view.peer,
                    },
                ])
            }
            _ => Err(conflict()),
        }
    }
    fn ready(flow: &Flow) -> Result<Vec<Effect>> {
        let plan = flow.plan.as_ref().ok_or_else(conflict)?;
        let (reference, messages) = transfer(flow.view.id, dto::ObjectKind::Plan, plan)?;
        let mut effects = vec![send(
            flow,
            Message::ConfigPairReady(dto::PairReady::from_raw(
                [
                    (1, wire::pair(flow.view.id).write()),
                    (2, plan.intent()?.write()),
                    (3, reference.write()),
                ]
                .into(),
            )?),
        )];
        // Source echoes the already known plan reference to confirm readiness.
        if flow.view.direction == Direction::Incoming {
            effects.extend(messages.into_iter().map(|message| send(flow, message)));
        }
        Ok(effects)
    }
    fn grant(flow: &mut Flow, local: DeviceId, journal: &mut Journal) -> Result<Vec<Effect>> {
        if !flow.peer_ready || local > flow.view.peer.device_id {
            return Ok(Vec::new());
        }
        let plan = flow.plan.as_ref().ok_or_else(conflict)?;
        let permit = if let Some(permit) = &flow.permit {
            permit.clone()
        } else {
            match journal.permit(plan) {
                Ok(permit) => permit,
                Err(crate::pair_journal::Error::Conflict) => return Ok(Vec::new()),
                Err(error) => return Err(error.into()),
            }
        };
        let role = if flow.view.direction == Direction::Outgoing {
            Role::Source
        } else {
            Role::Target
        };
        if flow.permit.is_none() {
            journal.prepare(
                plan,
                permit.decision,
                role,
                (role == Role::Source).then_some(flow.source.wire()),
            )?;
        }
        let message = Message::ConfigPairCommitPermit(dto::PairCommitPermit::from_raw(
            [
                (1, wire::pair(permit.pair).write()),
                (2, RawValue::Bytes(permit.intent.to_vec())),
                (3, permit.decision.write()),
                (4, RawValue::Bytes(permit.plan.to_vec())),
            ]
            .into(),
        )?);
        flow.permit = Some(permit);
        Ok(vec![send(flow, message)])
    }
    fn authorize(flow: &mut Flow, identity: &Identity, state: &State, journal: &mut Journal) -> Result<Vec<Effect>> {
        if flow.view.direction != Direction::Outgoing || flow.permit.is_none() {
            return Ok(Vec::new());
        }
        if flow.bundle.is_some() {
            return if flow.admitted {
                Ok(vec![send(flow, Self::receipt(flow, false)?)])
            } else {
                Ok(vec![Effect::Admit {
                    pair: flow.view.id,
                    peer: flow.view.peer,
                }])
            };
        }
        if state.digest()? != flow.source.digest()? {
            return Self::restart_source(flow, state, journal);
        }
        let plan = flow.plan.as_ref().ok_or_else(conflict)?;
        let permit = flow.permit.as_ref().ok_or_else(conflict)?;
        let signature = merge::authorization(identity, plan, permit.decision)?;
        journal.commit_sent(flow.view.id, permit.decision)?;
        Ok(vec![send(
            flow,
            Message::ConfigPairCommit(dto::PairCommit::from_raw(
                [
                    (1, wire::pair(flow.view.id).write()),
                    (2, RawValue::Bytes(permit.intent.to_vec())),
                    (3, signature.write()),
                    (4, permit.decision.write()),
                    (5, RawValue::Bytes(permit.plan.to_vec())),
                ]
                .into(),
            )?),
        )])
    }
    fn committed(flow: &Flow, bundle: &dto::MergeTransferBundle) -> Result<Vec<Effect>> {
        let (reference, messages) = transfer(flow.view.id, dto::ObjectKind::Transfer, bundle)?;
        let mut effects = vec![send(
            flow,
            Message::ConfigPairCommitted(dto::PairCommitted::from_raw(
                [(1, wire::pair(flow.view.id).write()), (2, reference.write())].into(),
            )?),
        )];
        effects.extend(messages.into_iter().map(|message| send(flow, message)));
        Ok(effects)
    }
    fn receipt(flow: &Flow, reply: bool) -> Result<Message> {
        let certificate = flow.bundle.as_ref().ok_or_else(conflict)?.certificate()?;
        Ok(Message::ConfigPairReceipt(dto::PairReceipt::from_raw(
            [
                (1, wire::pair(flow.view.id).write()),
                (2, certificate.intent()?.merge_id()?.write()),
                (3, certificate.result_digest()?.write()),
                (4, reply.write()),
            ]
            .into(),
        )?))
    }
    pub fn admitted(&mut self, pair: PairId) -> Result<Vec<Effect>> {
        // A later C0 admission also visits retained completed flows. Their
        // durable receipt exchange has finished; it is not a new send occasion.
        if self.completed.contains(&pair) {
            return Ok(Vec::new());
        }
        let flow = self.flows.get_mut(&pair).ok_or_else(conflict)?;
        if flow.bundle.is_none() {
            return Err(conflict());
        }
        flow.admitted = true;
        if flow.view.direction == Direction::Outgoing {
            Ok(vec![send(flow, Self::receipt(flow, false)?)])
        } else {
            Ok(Vec::new())
        }
    }
    pub fn cancel(&mut self, pair: PairId, journal: &mut Journal) -> Result<Cancel> {
        let Some(flow) = self.flows.get(&pair) else {
            return Ok(Cancel::Cancelled);
        };
        if let Some(bundle) = &flow.bundle {
            return Ok(Cancel::Committed(bundle.clone()));
        }
        if let Some(permit) = &flow.permit {
            let recovery = journal
                .recoveries()?
                .into_iter()
                .find(|r| r.pair == pair && r.decision == permit.decision)
                .ok_or_else(conflict)?;
            if recovery.commit_sent || matches!(recovery.outcome, crate::pair_journal::Decision::Unconfirmed) {
                return Ok(Cancel::Unconfirmed);
            }
            journal.abort(
                pair,
                permit.decision,
                crate::pairing::problem(dto::PairCode::CancelledLocally, dto::PairStage::Commit),
            )?;
        }
        self.flows.remove(&pair);
        Ok(Cancel::Cancelled)
    }
    pub fn contains(&self, pair: PairId) -> bool {
        self.flows.contains_key(&pair)
    }
    pub fn staging_bytes(&self) -> usize {
        self.objects.staging_bytes()
    }
    pub fn staging_count(&self, link: u64) -> usize {
        self.objects.staging_count(link)
    }
    pub fn has_object(&self, link: u64, digest: [u8; 32]) -> bool {
        self.objects.contains(link, digest)
    }
    pub fn associated_pair(&self, link: u64, message: &Message) -> Option<PairId> {
        match message {
            Message::ConfigPairObjectBegin(value) => object_pair(&value.context().ok()?).ok(),
            Message::ConfigPairObjectChunk(value) => {
                object_pair(self.objects.context(link, value.digest().ok()?.0)?).ok()
            }
            Message::ConfigPairObjectEnd(value) => {
                object_pair(self.objects.context(link, value.digest().ok()?.0)?).ok()
            }
            _ => dto::PairId::read(message.raw().get(&1)?)
                .and_then(|id| wire::read_pair(&id))
                .ok(),
        }
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.objects.next_deadline()
    }
    pub fn expire(&mut self, now: Instant) -> Vec<PairId> {
        self.objects
            .expire(now)
            .into_iter()
            .filter_map(|(_, context)| object_pair(&context).ok())
            .collect()
    }
    pub fn completed(&mut self, pair: PairId) {
        if !self.completed.contains(&pair) {
            self.completed.push_back(pair);
        }
        while self.completed.len() > 256 {
            if let Some(old) = self.completed.pop_front() {
                self.flows.remove(&old);
            }
        }
    }
    pub fn retry_permits(&mut self, identity: &Identity, state: &State, journal: &mut Journal) -> Result<Vec<Effect>> {
        let mut pairs = self
            .flows
            .iter()
            .filter_map(|(id, flow)| (flow.bundle.is_none() && flow.permit.is_none() && flow.peer_ready).then_some(*id))
            .collect::<Vec<_>>();
        pairs.sort_by_key(|id| wire::pair(*id).encode(256).expect("pair id"));
        let mut result = Vec::new();
        for pair in pairs {
            let flow = self.flows.get_mut(&pair).expect("flow");
            result.extend(Self::grant(flow, identity.id(), journal)?);
            result.extend(Self::authorize(flow, identity, state, journal)?);
        }
        Ok(result)
    }
    pub fn drop_link(&mut self, link: u64) {
        self.objects.drop_link(link);
    }
    pub fn bundle(&self, pair: PairId) -> Option<&dto::MergeTransferBundle> {
        self.flows.get(&pair)?.bundle.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{admission, pairing::Phase};
    use std::collections::VecDeque;
    struct Side {
        identity: Identity,
        state: State,
        journal: Journal,
        transactions: Transactions,
        boot: Boot,
    }
    fn pair(sides: &[Side; 2]) -> View {
        let source = &sides[0];
        let target = &sides[1];
        let id = PairId {
            initiator_boot: source.boot,
            random: [3; 16],
        };
        let context = dto::PairContext::from_raw(
            [
                (1, wire::pair(id).write()),
                (2, wire::boot(source.boot).write()),
                (3, wire::boot(target.boot).write()),
                (4, RawValue::Bytes(source.state.roster().anchor().group.0.to_vec())),
                (5, RawValue::Bytes(target.state.roster().anchor().group.0.to_vec())),
                (6, 2u8.write()),
                (7, 1u8.write()),
                (8, RawValue::Bytes(target.state.roster().anchor().group.0.to_vec())),
                (9, RawValue::Bytes([0; 32].to_vec())),
                (10, RawValue::from(dto::PAIRING_SUITE)),
                (11, RawValue::Bytes(target.state.roster().anchor().root.to_vec())),
                (12, RawValue::Bytes(source.state.roster().anchor().root.to_vec())),
            ]
            .into(),
        )
        .unwrap();
        View {
            id,
            revision: 1,
            peer: target.boot,
            context,
            direction: Direction::Outgoing,
            phase: Phase::Preparing,
            attempts_left: 3,
            feedback: dto::PairFeedbackKind::Verified,
            terminal: None,
        }
    }
    fn effects(
        index: usize,
        result: Vec<Effect>,
        sides: &mut [Side; 2],
        queue: &mut VecDeque<(usize, Message)>,
        completed: &mut [usize; 2],
    ) {
        for effect in result {
            match effect {
                Effect::Send { peer, message } => {
                    assert_eq!(peer, sides[1 - index].boot);
                    let bytes = v1::frame::encode(&message, v1::limits::CONFIG_FRAME).unwrap();
                    let decoded = v1::frame::OpaqueFrame::parse_exact(&bytes, v1::limits::CONFIG_FRAME)
                        .unwrap()
                        .decode()
                        .unwrap();
                    queue.push_back((1 - index, decoded));
                }
                Effect::Project => {
                    let wire = sides[index].journal.pending_projection().unwrap().unwrap();
                    let roster = wire.roster().unwrap();
                    let anchor = crate::roster::Anchor {
                        group: oink_foundation::GroupId(roster.group().unwrap().0),
                        root: roster.root_id().unwrap().0,
                    };
                    sides[index].state = State::verify(wire, anchor).unwrap();
                    sides[index]
                        .journal
                        .projected(sides[index].state.digest().unwrap())
                        .unwrap();
                }
                Effect::Admit { pair, .. } => {
                    let hash = [0; 32];
                    let Message::ConfigRequest(request) =
                        admission::request(&sides[0].state, sides[0].identity.id(), hash, 0).unwrap()
                    else {
                        panic!()
                    };
                    let accepted = admission::receive_request(
                        &sides[1].state,
                        &request,
                        sides[1].identity.id(),
                        sides[0].identity.id(),
                        hash,
                    )
                    .unwrap();
                    let Message::ConfigReply(reply) =
                        admission::reply(&accepted, sides[1].identity.id(), hash, 0).unwrap()
                    else {
                        panic!()
                    };
                    let accepted = admission::receive_reply(
                        &sides[0].state,
                        &sides[0].state,
                        &reply,
                        sides[0].identity.id(),
                        sides[1].identity.id(),
                        hash,
                    )
                    .unwrap();
                    let Message::ConfigDone(done) = admission::done(&accepted.acknowledged, hash).unwrap() else {
                        panic!()
                    };
                    admission::receive_done(&sides[1].state, &done, hash).unwrap();
                    for i in [1, 0] {
                        let result = sides[i].transactions.admitted(pair).unwrap();
                        effects(i, result, sides, queue, completed);
                    }
                }
                Effect::Completed { pair, bundle, .. } => {
                    let id = sides[index]
                        .journal
                        .receipt(
                            pair,
                            bundle.certificate().unwrap().permit_decision().unwrap(),
                            [(1, true.write())].into(),
                        )
                        .unwrap();
                    assert_eq!(id, 1);
                    completed[index] += 1;
                }
            }
        }
    }
    #[test]
    fn directional_transaction_admission_and_receipts_with_either_coordinator() {
        for (reverse, changed_source, changed_target) in [
            (false, false, false),
            (true, false, false),
            (false, true, true),
            (true, true, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let a = Identity::from_seed([1; 32]);
            let b = Identity::from_seed([2; 32]);
            let identities = if reverse { [b, a] } else { [a, b] };
            let mut sides = identities
                .into_iter()
                .enumerate()
                .map(|(i, identity)| {
                    let state = State::create(&identity, "local", "test", 1).unwrap();
                    let boot = Boot {
                        device_id: identity.id(),
                        nonce: [i as u8; 16],
                    };
                    let journal = Journal::load(&dir.path().join(format!("{i}.cbor")), identity.id()).unwrap();
                    Side {
                        identity,
                        state,
                        journal,
                        transactions: Transactions::default(),
                        boot,
                    }
                })
                .collect::<Vec<_>>()
                .try_into()
                .ok()
                .unwrap();
            let offline = Identity::from_seed([9; 32]);
            let s: &mut [Side; 2] = &mut sides;
            s[0].state = s[0]
                .state
                .with_roster(
                    &s[0]
                        .state
                        .roster()
                        .add(&s[0].identity, offline.id(), "offline", "test", 2)
                        .unwrap(),
                )
                .unwrap();
            let target_anchor = sides[1].state.roster().anchor();
            let view = pair(&sides);
            let mut incoming = view.clone();
            incoming.peer = sides[0].boot;
            incoming.direction = Direction::Incoming;
            sides[1]
                .transactions
                .verified(incoming, &sides[1].state, &mut sides[1].journal)
                .unwrap();
            let initial = sides[0]
                .transactions
                .verified(view.clone(), &sides[0].state, &mut sides[0].journal)
                .unwrap();
            let mut queue = VecDeque::new();
            let mut completed = [0; 2];
            effects(0, initial, &mut sides, &mut queue, &mut completed);
            let mut count = 0;
            let mut source_changed = false;
            let mut target_changed = false;
            let mut stale_commit = None;
            while let Some((index, message)) = queue.pop_front() {
                count += 1;
                assert!(count < 200);
                let change_source = changed_source
                    && !source_changed
                    && index == 0
                    && matches!(message, Message::ConfigPairObjectEnd(_));
                let change_target =
                    changed_target && !target_changed && index == 1 && matches!(message, Message::ConfigPairCommit(_));
                if change_source || change_target {
                    let mut raw = sides[index].state.wire().raw().clone();
                    raw.insert(99, 123u64.write());
                    sides[index].state = State::verify(
                        dto::GroupState::from_raw(raw).unwrap(),
                        sides[index].state.roster().anchor(),
                    )
                    .unwrap();
                    if change_source {
                        source_changed = true;
                    } else {
                        target_changed = true;
                        stale_commit = Some(message.clone());
                    }
                }
                if index == 1
                    && matches!(message, Message::ConfigPairCommit(_))
                    && !change_target
                    && let Some(stale) = stale_commit.take()
                {
                    queue.push_back((1, stale));
                }
                let peer = sides[1 - index].boot;
                let side = &mut sides[index];
                let result = side
                    .transactions
                    .receive(
                        1,
                        peer,
                        message,
                        &side.identity,
                        &side.state,
                        &mut side.journal,
                        Instant::now(),
                        10,
                    )
                    .unwrap();
                effects(index, result, &mut sides, &mut queue, &mut completed);
            }
            assert_eq!(completed, [1, 1]);
            for side in &sides {
                assert_eq!(side.state.roster().anchor(), target_anchor);
                assert_eq!(side.state.roster().members().count(), 3);
                assert!(side.state.roster().contains(offline.id()));
                assert_eq!(side.journal.events(0).unwrap().len(), 1);
            }
            assert_eq!(sides[0].state.digest().unwrap(), sides[1].state.digest().unwrap());
            for index in [0, 1] {
                sides[index].transactions.completed(view.id);
                let readmitted = sides[index].transactions.admitted(view.id).unwrap();
                let receipt =
                    Transactions::receipt(&sides[1 - index].transactions.flows[&view.id], index == 0).unwrap();
                let peer = sides[1 - index].boot;
                let side = &mut sides[index];
                let repeated = side
                    .transactions
                    .receive(
                        1,
                        peer,
                        receipt,
                        &side.identity,
                        &side.state,
                        &mut side.journal,
                        Instant::now(),
                        11,
                    )
                    .unwrap();
                let completions = repeated
                    .iter()
                    .filter(|effect| matches!(effect, Effect::Completed { .. }))
                    .count();
                assert!(
                    readmitted.is_empty() && completions == 0,
                    "completed transaction replay: readmission sends={}, repeated completions={completions}",
                    readmitted.len(),
                );
                assert_eq!(
                    repeated.len(),
                    usize::from(index == 1),
                    "target still acknowledges a repeated receipt"
                );
            }
        }
    }
}
