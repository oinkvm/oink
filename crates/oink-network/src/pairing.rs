//! Independent direct pairing sessions. The configuration owner serializes all
//! calls and durably commits a merge before advancing an authenticated session.
use crate::{
    group_pake::{self, InitiatorPake, ResponderPake},
    wire_identity as wire,
};
use oink_foundation::{
    DeviceId,
    session::{Boot, PairId},
};
use oink_protocol::v1::{
    self, Message, RawValue, dto,
    schema::{WireDto, WireValue},
};
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub const MAX_PENDING: usize = 3;
pub const MAX_ACTIVE: usize = 32;
pub const MAX_PER_PEER: usize = 4;
const MAX_TERMINAL: usize = 256;
const PROGRESS_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Requested,
    ShowCode,
    EnterCode,
    CheckingCode,
    Preparing,
    Committing,
    Admitting,
    Completed,
    Ended,
}
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub id: PairId,
    pub revision: u64,
    pub peer: Boot,
    pub context: dto::PairContext,
    pub direction: Direction,
    pub phase: Phase,
    pub attempts_left: u8,
    pub feedback: dto::PairFeedbackKind,
    pub terminal: Option<dto::PairProblem>,
}
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub revision: u64,
    pub requests: Vec<View>,
    pub sessions: Vec<View>,
}
#[derive(Debug)]
pub enum Effect {
    /// Mandatory reliable C0 delivery. The owner ends the link if it cannot queue it.
    Send {
        peer: Boot,
        message: Message,
    },
    Changed,
    /// Both PAKE confirmations passed. Snapshot planning may now begin.
    Verified(PairId),
    Completed(PairId),
    Ended {
        id: PairId,
        problem: dto::PairProblem,
    },
}
pub type Result<T> = std::result::Result<T, dto::PairProblem>;
#[derive(Clone, Debug)]
pub struct Transcript {
    pub hello: Vec<u8>,
    pub info: Vec<u8>,
}
struct Attempt {
    number: u8,
    receiver: Option<ResponderPake>,
    initiator: Option<InitiatorPake>,
    incoming: Vec<(dto::PairStepKind, Message, Message)>,
}
struct Session {
    view: View,
    transcript: Transcript,
    code: Option<Zeroizing<String>>,
    accepted: bool,
    deadline: Option<Instant>,
    attempt: Attempt,
    request: dto::PairRequest,
}
struct Terminal {
    peer: Boot,
    acknowledgement: Message,
    view: Option<View>,
}
#[derive(Clone, Copy)]
struct Bucket {
    tokens: u8,
    updated: Instant,
}
impl Bucket {
    fn consume(&mut self, now: Instant) -> Option<u32> {
        let refill = (now.saturating_duration_since(self.updated).as_secs() / 10).min(4) as u8;
        if refill > 0 {
            self.tokens = self.tokens.saturating_add(refill).min(4);
            self.updated += Duration::from_secs(u64::from(refill) * 10);
            if self.tokens == 4 {
                self.updated = now;
            }
        }
        if self.tokens > 0 {
            self.tokens -= 1;
            None
        } else {
            Some(
                (10_000u128.saturating_sub(now.saturating_duration_since(self.updated).as_millis())).min(10_000) as u32,
            )
        }
    }
}
pub struct Sessions {
    local: Boot,
    revision: u64,
    active: HashMap<PairId, Session>,
    terminal: HashMap<PairId, Terminal>,
    order: VecDeque<PairId>,
    rates: HashMap<DeviceId, Bucket>,
}
impl Sessions {
    pub fn new(local: Boot) -> Self {
        Self {
            local,
            revision: 0,
            active: HashMap::new(),
            terminal: HashMap::new(),
            order: VecDeque::new(),
            rates: HashMap::new(),
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        let mut result = Snapshot {
            revision: self.revision,
            ..Snapshot::default()
        };
        for session in self.active.values() {
            if session.view.direction == Direction::Incoming && !session.accepted {
                result.requests.push(session.view.clone());
            } else {
                result.sessions.push(session.view.clone());
            }
        }
        result
            .sessions
            .extend(self.order.iter().filter_map(|id| self.terminal.get(id)?.view.clone()));
        result.requests.sort_by_key(|v| key(v.id));
        result.sessions.sort_by_key(|v| key(v.id));
        result
    }
    pub fn view(&self, id: PairId) -> Option<&View> {
        self.active
            .get(&id)
            .map(|s| &s.view)
            .or_else(|| self.terminal.get(&id).and_then(|s| s.view.as_ref()))
    }
    pub fn knows(&self, id: PairId) -> bool {
        self.active.contains_key(&id) || self.terminal.contains_key(&id)
    }
    pub fn terminal_problem(&mut self, id: PairId, reason: dto::PairProblem) {
        if let Some(terminal) = self.terminal.get_mut(&id) {
            terminal.acknowledgement = ended(id, reason.clone());
            if let Some(view) = &mut terminal.view {
                view.terminal = Some(reason);
            }
        }
    }
    pub fn code(&self, id: PairId) -> Result<Zeroizing<String>> {
        self.active
            .get(&id)
            .and_then(|s| s.code.clone())
            .ok_or_else(|| self.closed(id))
    }
    pub fn has_lease(&self, peer: Boot) -> bool {
        self.active.values().any(|s| s.view.peer == peer)
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.active.values().filter_map(|s| s.deadline).min()
    }
    fn changed(&mut self, id: PairId) {
        self.revision = self.revision.checked_add(1).expect("pairing revision exhausted");
        if let Some(session) = self.active.get_mut(&id) {
            session.view.revision = self.revision;
        }
    }
    fn closed(&self, id: PairId) -> dto::PairProblem {
        let mut p = problem(dto::PairCode::PairClosed, dto::PairStage::Code);
        if let Some(cause) = self
            .terminal
            .get(&id)
            .and_then(|t| t.view.as_ref())
            .and_then(|v| v.terminal.as_ref())
            .and_then(|p| p.code().ok())
        {
            p = with(p, 10, cause.write());
        }
        p
    }
    fn current(&self, id: PairId, revision: u64) -> Result<&Session> {
        let session = self.active.get(&id).ok_or_else(|| self.closed(id))?;
        if revision != session.view.revision {
            return Err(problem(dto::PairCode::StaleRevision, dto::PairStage::Code));
        }
        Ok(session)
    }
    fn capacity(&self, peer: Boot, pending: bool) -> Result<()> {
        if pending
            && self
                .active
                .values()
                .filter(|s| s.view.direction == Direction::Incoming && !s.accepted)
                .count()
                >= MAX_PENDING
        {
            let mut p = problem(dto::PairCode::TooManyPendingRequests, dto::PairStage::Request);
            p = with(p, 6, 4u32.write());
            p = with(p, 7, 3u32.write());
            p = with(p, 9, dto::PairLimitKind::PendingRequests.write());
            return Err(p);
        }
        if self.active.len() >= MAX_ACTIVE
            || self
                .active
                .values()
                .filter(|s| s.view.peer.device_id == peer.device_id)
                .count()
                >= MAX_PER_PEER
        {
            return Err(problem(dto::PairCode::QueueFull, dto::PairStage::Request));
        }
        Ok(())
    }
    pub fn start(&mut self, context: dto::PairContext, peer: Boot, transcript: Transcript) -> Result<Vec<Effect>> {
        let id = wire::read_pair(&context.pair_id().map_err(protocol)?).map_err(protocol)?;
        if id.initiator_boot != self.local
            || wire::read_boot(&context.initiator().map_err(protocol)?).map_err(protocol)? != self.local
            || wire::read_boot(&context.responder().map_err(protocol)?).map_err(protocol)? != peer
        {
            return Err(problem(dto::PairCode::AuthenticationFailed, dto::PairStage::Request));
        }
        self.capacity(peer, false)?;
        if self.active.contains_key(&id) || self.terminal.contains_key(&id) {
            return Err(self.closed(id));
        }
        let request =
            dto::PairRequest::from_raw([(1, wire::pair(id).write()), (2, context.write())].into()).map_err(protocol)?;
        let message = Message::ConfigPairRequest(request.clone());
        self.active.insert(
            id,
            Session {
                view: View {
                    id,
                    revision: 0,
                    peer,
                    context,
                    direction: Direction::Outgoing,
                    phase: Phase::ShowCode,
                    attempts_left: 3,
                    feedback: dto::PairFeedbackKind::Waiting,
                    terminal: None,
                },
                transcript,
                code: Some(group_pake::generate_code()),
                accepted: false,
                deadline: None,
                attempt: Attempt {
                    number: 0,
                    receiver: None,
                    initiator: None,
                    incoming: Vec::new(),
                },
                request,
            },
        );
        self.changed(id);
        Ok(vec![Effect::Send { peer, message }, Effect::Changed])
    }
    /// Context/authentication is checked before rate, block and capacity policy.
    pub fn request(
        &mut self,
        request: dto::PairRequest,
        peer: Boot,
        transcript: Transcript,
        blocked: bool,
        now: Instant,
    ) -> Result<Vec<Effect>> {
        let context = request.context().map_err(protocol)?;
        let id = wire::read_pair(&request.pair_id().map_err(protocol)?).map_err(protocol)?;
        if wire::read_pair(&context.pair_id().map_err(protocol)?).map_err(protocol)? != id
            || id.initiator_boot != peer
            || wire::read_boot(&context.initiator().map_err(protocol)?).map_err(protocol)? != peer
            || wire::read_boot(&context.responder().map_err(protocol)?).map_err(protocol)? != self.local
        {
            return Err(problem(dto::PairCode::AuthenticationFailed, dto::PairStage::Request));
        }
        if let Some(terminal) = self.terminal.get(&id) {
            if terminal.peer != peer {
                return Err(protocol(v1::ErrorCode::AuthenticationFailed.into()));
            }
            return Ok(vec![Effect::Send {
                peer,
                message: terminal.acknowledgement.clone(),
            }]);
        }
        if let Some(session) = self.active.get(&id) {
            if session.request != request {
                return Err(protocol(v1::ErrorCode::StateConflict.into()));
            }
            let message = if session.accepted { accepted(id) } else { queued(id) };
            return Ok(vec![Effect::Send { peer, message }]);
        }
        if self.rates.len() >= 4096 && !self.rates.contains_key(&peer.device_id) {
            self.rates
                .retain(|_, bucket| now.saturating_duration_since(bucket.updated) < Duration::from_secs(40));
            if self.rates.len() >= 4096 {
                return Ok(self.refuse(
                    id,
                    peer,
                    with(
                        problem(dto::PairCode::RateLimited, dto::PairStage::Request),
                        3,
                        10_000u32.write(),
                    ),
                ));
            }
        }
        if let Some(retry) = self
            .rates
            .entry(peer.device_id)
            .or_insert(Bucket {
                tokens: 4,
                updated: now,
            })
            .consume(now)
        {
            return Ok(self.refuse(
                id,
                peer,
                with(
                    problem(dto::PairCode::RateLimited, dto::PairStage::Request),
                    3,
                    retry.write(),
                ),
            ));
        }
        if blocked {
            return Ok(self.refuse(id, peer, problem(dto::PairCode::Declined, dto::PairStage::Request)));
        }
        if let Err(problem) = self.capacity(peer, true) {
            return Ok(self.refuse(id, peer, problem));
        }
        self.active.insert(
            id,
            Session {
                view: View {
                    id,
                    revision: 0,
                    peer,
                    context,
                    direction: Direction::Incoming,
                    phase: Phase::Requested,
                    attempts_left: 3,
                    feedback: dto::PairFeedbackKind::Waiting,
                    terminal: None,
                },
                transcript,
                code: None,
                accepted: false,
                deadline: None,
                attempt: Attempt {
                    number: 0,
                    receiver: None,
                    initiator: None,
                    incoming: Vec::new(),
                },
                request,
            },
        );
        self.changed(id);
        Ok(vec![
            Effect::Send {
                peer,
                message: queued(id),
            },
            Effect::Changed,
        ])
    }
    fn cache(&mut self, id: PairId, terminal: Terminal) {
        if !self.terminal.contains_key(&id) {
            self.order.push_back(id);
        }
        self.terminal.insert(id, terminal);
        while self.order.len() > MAX_TERMINAL {
            if let Some(old) = self.order.pop_front() {
                self.terminal.remove(&old);
            }
        }
    }
    pub fn refuse(&mut self, id: PairId, peer: Boot, reason: dto::PairProblem) -> Vec<Effect> {
        let message = Message::ConfigPairDeclined(
            dto::PairDeclined::from_raw([(1, wire::pair(id).write()), (2, reason.write())].into()).expect("decline"),
        );
        self.cache(
            id,
            Terminal {
                peer,
                acknowledgement: message.clone(),
                view: None,
            },
        );
        vec![Effect::Send { peer, message }]
    }
    pub fn accept(&mut self, id: PairId, revision: u64) -> Result<Vec<Effect>> {
        let s = self.current(id, revision)?;
        if s.view.direction != Direction::Incoming || s.accepted {
            return Err(problem(dto::PairCode::StateChanged, dto::PairStage::Request));
        }
        let s = self.active.get_mut(&id).expect("checked");
        s.accepted = true;
        s.view.phase = Phase::EnterCode;
        let peer = s.view.peer;
        self.changed(id);
        Ok(vec![
            Effect::Send {
                peer,
                message: accepted(id),
            },
            Effect::Changed,
        ])
    }
    pub fn decline(&mut self, id: PairId, revision: u64) -> Result<Vec<Effect>> {
        let s = self.current(id, revision)?;
        if s.view.direction != Direction::Incoming || s.accepted {
            return Err(problem(dto::PairCode::StateChanged, dto::PairStage::Request));
        }
        let reason = problem(dto::PairCode::Declined, dto::PairStage::Request);
        let peer = s.view.peer;
        let mut effects = self.end(id, reason.clone());
        effects.insert(
            0,
            Effect::Send {
                peer,
                message: Message::ConfigPairDeclined(
                    dto::PairDeclined::from_raw([(1, wire::pair(id).write()), (2, reason.write())].into())
                        .expect("decline"),
                ),
            },
        );
        Ok(effects)
    }
    pub fn cancel(&mut self, id: PairId, revision: u64) -> Result<Vec<Effect>> {
        let peer = self.current(id, revision)?.view.peer;
        let reason = problem(dto::PairCode::CancelledLocally, dto::PairStage::Code);
        let mut effects = self.end(id, reason.clone());
        effects.insert(
            0,
            Effect::Send {
                peer,
                message: Message::ConfigPairCancel(
                    dto::PairCancel::from_raw([(1, wire::pair(id).write()), (2, reason.write())].into())
                        .expect("cancel"),
                ),
            },
        );
        Ok(effects)
    }
    /// Called only after a durable local block installation succeeds.
    pub fn block_pending(&mut self, device: DeviceId) -> Vec<Effect> {
        let rows = self
            .active
            .iter()
            .filter(|(_, s)| s.view.peer.device_id == device && s.view.direction == Direction::Incoming && !s.accepted)
            .map(|(&id, s)| (id, s.view.peer))
            .collect::<Vec<_>>();
        rows.into_iter()
            .flat_map(|(id, peer)| {
                let reason = problem(dto::PairCode::Declined, dto::PairStage::Request);
                let message = Message::ConfigPairDeclined(
                    dto::PairDeclined::from_raw([(1, wire::pair(id).write()), (2, reason.write())].into())
                        .expect("decline"),
                );
                // A saved block dismisses pending requests without a local failure.
                // Retain the terminal view for late Accept and cache the ordinary
                // decline for retries; the remote initiator still sees Declined.
                assert!(self.finish(id, reason, message.clone()));
                [Effect::Send { peer, message }, Effect::Changed]
            })
            .collect()
    }
    pub fn submit(&mut self, id: PairId, revision: u64, attempt: u8, code: &str, now: Instant) -> Result<Vec<Effect>> {
        let s = self.current(id, revision)?;
        if !group_pake::valid_code(code) {
            return Err(problem(dto::PairCode::InvalidCodeSyntax, dto::PairStage::Code));
        }
        if s.view.direction != Direction::Incoming
            || !s.accepted
            || s.view.phase != Phase::EnterCode
            || attempt != s.attempt.number + 1
            || attempt > 3
        {
            return Err(problem(dto::PairCode::StateChanged, dto::PairStage::Code));
        }
        let (receiver, start) = ResponderPake::start(
            code,
            s.view.context.clone(),
            attempt,
            &s.transcript.hello,
            &s.transcript.info,
        )
        .map_err(|_| problem(dto::PairCode::ProtocolViolation, dto::PairStage::Code))?;
        let message = pake(id, attempt, dto::PairStepKind::Start, start.write());
        let s = self.active.get_mut(&id).expect("checked");
        s.attempt.number = attempt;
        s.attempt.receiver = Some(receiver);
        s.deadline = Some(now + PROGRESS_TIMEOUT);
        s.view.phase = Phase::CheckingCode;
        s.view.feedback = dto::PairFeedbackKind::Waiting;
        s.view.attempts_left = 3 - attempt;
        let peer = s.view.peer;
        self.changed(id);
        Ok(vec![Effect::Send { peer, message }, Effect::Changed])
    }
    pub fn receive(&mut self, peer: Boot, message: Message, now: Instant) -> Result<Vec<Effect>> {
        let id = message_pair(&message)?;
        if let Some(t) = self.terminal.get(&id) {
            if t.peer != peer {
                return Err(problem(dto::PairCode::AuthenticationFailed, dto::PairStage::Code));
            }
            return Ok(if matches!(message, Message::ConfigPairEnded(_)) {
                Vec::new()
            } else {
                vec![Effect::Send {
                    peer,
                    message: t.acknowledgement.clone(),
                }]
            });
        }
        let s = self.active.get(&id).ok_or_else(|| self.closed(id))?;
        if s.view.peer != peer {
            return Err(problem(dto::PairCode::AuthenticationFailed, dto::PairStage::Code));
        }
        match message {
            Message::ConfigPairQueued(_) => Ok(Vec::new()),
            Message::ConfigPairAccept(_) => {
                if s.view.direction != Direction::Outgoing {
                    return Err(protocol(v1::ErrorCode::StateConflict.into()));
                }
                if s.accepted {
                    return Ok(Vec::new());
                }
                self.active.get_mut(&id).unwrap().accepted = true;
                self.changed(id);
                Ok(vec![Effect::Changed])
            }
            Message::ConfigPairDeclined(declined) => {
                let p = declined.reason().map_err(protocol)?;
                let mut effects = self.end(id, p.clone());
                effects.insert(
                    0,
                    Effect::Send {
                        peer,
                        message: ended(id, p),
                    },
                );
                Ok(effects)
            }
            Message::ConfigPairCancel(cancel) => {
                let p = cancel.reason().map_err(protocol)?;
                let p = if p.code().map_err(protocol)? == dto::PairCode::CancelledLocally {
                    problem(dto::PairCode::CancelledByPeer, dto::PairStage::Code)
                } else {
                    p
                };
                let mut effects = self.end(id, p.clone());
                effects.insert(
                    0,
                    Effect::Send {
                        peer,
                        message: ended(id, p),
                    },
                );
                Ok(effects)
            }
            Message::ConfigPairEnded(end) => Ok(self.end(id, end.problem().map_err(protocol)?)),
            Message::ConfigPairPake(record) => self.receive_pake(id, record, now),
            Message::ConfigPairFeedback(feedback) => {
                if feedback.attempt().map_err(protocol)? != s.attempt.number {
                    return Err(protocol(v1::ErrorCode::StateConflict.into()));
                }
                let kind = feedback.feedback().map_err(protocol)?;
                if kind==dto::PairFeedbackKind::WrongCode {self.wrong(id)}
                else if kind==dto::PairFeedbackKind::Verified && s.view.direction==Direction::Incoming && s.view.phase==Phase::CheckingCode
                    && s.attempt.incoming.iter().any(|(_,_,reply)| matches!(reply,Message::ConfigPairPake(p) if p.attempt().ok()==Some(s.attempt.number) && p.step().ok().is_some_and(|step|step.raw().get(&1)==Some(&dto::PairStepKind::Finish.write())))) {
                    self.verified(id,now)
                } else {Ok(Vec::new())}
            }
            _ => Err(protocol(v1::ErrorCode::StateConflict.into())),
        }
    }
    fn receive_pake(&mut self, id: PairId, record: dto::PairPake, now: Instant) -> Result<Vec<Effect>> {
        let attempt = record.attempt().map_err(protocol)?;
        let step = record.step().map_err(protocol)?;
        let kind = dto::PairStepKind::read(
            step.raw()
                .get(&1)
                .ok_or_else(|| protocol(v1::ErrorCode::MalformedFrame.into()))?,
        )
        .map_err(protocol)?;
        if record.suite().map_err(protocol)?.0 != dto::PAIRING_SUITE {
            return Err(problem(dto::PairCode::NoCommonCrypto, dto::PairStage::Code));
        }
        let incoming = Message::ConfigPairPake(record.clone());
        let s = self.active.get_mut(&id).expect("checked");
        if !s.accepted {
            return Err(protocol(v1::ErrorCode::StateConflict.into()));
        }
        if let Some((_, old, reply)) =
            s.attempt.incoming.iter().find(|(k, m, _)| {
                *k == kind && matches!(m,Message::ConfigPairPake(p) if p.attempt().ok()==Some(attempt))
            })
        {
            if *old != incoming {
                return Err(protocol(v1::ErrorCode::StateConflict.into()));
            }
            return Ok(vec![Effect::Send {
                peer: s.view.peer,
                message: reply.clone(),
            }]);
        }
        let body = step.body().map_err(protocol)?;
        let reply = match kind {
            dto::PairStepKind::Start => {
                if s.view.direction != Direction::Outgoing
                    || !matches!(s.view.phase, Phase::ShowCode)
                    || attempt != s.attempt.number + 1
                {
                    return Err(protocol(v1::ErrorCode::StateConflict.into()));
                }
                let start = dto::PakeStart::from_raw(body).map_err(protocol)?;
                if start.context().map_err(protocol)? != s.view.context {
                    return Err(problem(dto::PairCode::AuthenticationFailed, dto::PairStage::Code));
                }
                let (initiator, reply) = InitiatorPake::respond(
                    s.code.as_ref().expect("outgoing code"),
                    &s.view.context,
                    attempt,
                    &s.transcript.hello,
                    &s.transcript.info,
                    &start.message().map_err(protocol)?.0,
                )
                .map_err(|_| problem(dto::PairCode::ProtocolViolation, dto::PairStage::Code))?;
                s.attempt.number = attempt;
                s.attempt.initiator = Some(initiator);
                s.view.feedback = dto::PairFeedbackKind::Waiting;
                s.view.phase = Phase::CheckingCode;
                s.view.attempts_left = 3 - attempt;
                s.deadline = Some(now + PROGRESS_TIMEOUT);
                pake(id, attempt, dto::PairStepKind::Reply, reply.write())
            }
            dto::PairStepKind::Reply => {
                if s.view.direction != Direction::Incoming || attempt != s.attempt.number {
                    return Err(protocol(v1::ErrorCode::StateConflict.into()));
                }
                let reply = dto::PakeReply::from_raw(body).map_err(protocol)?;
                let receiver = s
                    .attempt
                    .receiver
                    .take()
                    .ok_or_else(|| protocol(v1::ErrorCode::StateConflict.into()))?;
                match receiver.finish(
                    &reply.message().map_err(protocol)?.0,
                    &reply.confirmation().map_err(protocol)?.0,
                ) {
                    Ok(finish) => pake(id, attempt, dto::PairStepKind::Finish, finish.write()),
                    Err(group_pake::PakeError::WrongCode) => feedback(id, attempt, dto::PairFeedbackKind::WrongCode),
                    Err(_) => return Err(problem(dto::PairCode::ProtocolViolation, dto::PairStage::Code)),
                }
            }
            dto::PairStepKind::Finish => {
                if s.view.direction != Direction::Outgoing || attempt != s.attempt.number {
                    return Err(protocol(v1::ErrorCode::StateConflict.into()));
                }
                let finish = dto::PakeFinish::from_raw(body).map_err(protocol)?;
                let initiator = s
                    .attempt
                    .initiator
                    .take()
                    .ok_or_else(|| protocol(v1::ErrorCode::StateConflict.into()))?;
                match initiator.finish(&finish.confirmation().map_err(protocol)?.0) {
                    Ok(()) => feedback(id, attempt, dto::PairFeedbackKind::Verified),
                    Err(group_pake::PakeError::WrongCode) => feedback(id, attempt, dto::PairFeedbackKind::WrongCode),
                    Err(_) => return Err(problem(dto::PairCode::ProtocolViolation, dto::PairStage::Code)),
                }
            }
            _ => return Err(protocol(v1::ErrorCode::UnsupportedValue.into())),
        };
        s.attempt.incoming.push((kind, incoming, reply.clone()));
        let peer = s.view.peer;
        let wrong = matches!(&reply,Message::ConfigPairFeedback(f) if f.feedback().ok()==Some(dto::PairFeedbackKind::WrongCode));
        let verified =
            matches!(&reply,Message::ConfigPairFeedback(f) if f.feedback().ok()==Some(dto::PairFeedbackKind::Verified));
        let mut effects = vec![Effect::Send { peer, message: reply }];
        if wrong {
            effects.extend(self.wrong(id)?);
        } else if verified {
            effects.extend(self.verified(id, now)?);
        } else {
            self.changed(id);
            effects.push(Effect::Changed);
        }
        Ok(effects)
    }
    fn wrong(&mut self, id: PairId) -> Result<Vec<Effect>> {
        let s = self.active.get_mut(&id).expect("checked");
        if s.attempt.number == 3 {
            let p = with(
                problem(dto::PairCode::TooManyAttempts, dto::PairStage::Code),
                4,
                0u8.write(),
            );
            let peer = s.view.peer;
            let mut effects = self.end(id, p.clone());
            effects.insert(
                0,
                Effect::Send {
                    peer,
                    message: ended(id, p),
                },
            );
            return Ok(effects);
        }
        if s.view.feedback == dto::PairFeedbackKind::WrongCode {
            return Ok(Vec::new());
        }
        s.attempt.receiver = None;
        s.attempt.initiator = None;
        s.deadline = None;
        s.view.feedback = dto::PairFeedbackKind::WrongCode;
        s.view.phase = if s.view.direction == Direction::Incoming {
            Phase::EnterCode
        } else {
            Phase::ShowCode
        };
        self.changed(id);
        Ok(vec![Effect::Changed])
    }
    fn verified(&mut self, id: PairId, now: Instant) -> Result<Vec<Effect>> {
        let s = self.active.get_mut(&id).expect("checked");
        if s.view.phase == Phase::Preparing {
            return Ok(Vec::new());
        }
        s.code = None;
        s.view.phase = Phase::Preparing;
        s.view.feedback = dto::PairFeedbackKind::Verified;
        s.deadline = Some(now + PROGRESS_TIMEOUT);
        self.changed(id);
        Ok(vec![Effect::Changed, Effect::Verified(id)])
    }
    pub fn progress(&mut self, id: PairId, phase: Phase, now: Instant) -> Result<Vec<Effect>> {
        let s = self
            .active
            .get_mut(&id)
            .ok_or_else(|| problem(dto::PairCode::PairClosed, dto::PairStage::Commit))?;
        if !matches!(
            (s.view.phase, phase),
            (Phase::Preparing, Phase::Preparing | Phase::Committing)
                | (Phase::Committing, Phase::Committing | Phase::Admitting)
                | (Phase::Admitting, Phase::Admitting)
        ) {
            return Err(problem(dto::PairCode::StateChanged, dto::PairStage::Commit));
        }
        s.view.phase = phase;
        s.deadline = Some(now + PROGRESS_TIMEOUT);
        self.changed(id);
        Ok(vec![Effect::Changed])
    }
    /// Called only after durable adoption, membership admission and receipt.
    pub fn complete(&mut self, id: PairId) -> Result<Vec<Effect>> {
        let s = self.active.get(&id).ok_or_else(|| self.closed(id))?;
        if s.view.phase != Phase::Admitting {
            return Err(problem(dto::PairCode::StateChanged, dto::PairStage::Adopt));
        }
        let mut s = self.active.remove(&id).expect("checked");
        self.changed(id);
        s.view.phase = Phase::Completed;
        s.view.revision = self.revision;
        let ack = ended(id, problem(dto::PairCode::AlreadyCommitted, dto::PairStage::Adopt));
        self.cache(
            id,
            Terminal {
                peer: s.view.peer,
                acknowledgement: ack,
                view: Some(s.view),
            },
        );
        Ok(vec![Effect::Changed, Effect::Completed(id)])
    }
    pub fn fail(&mut self, id: PairId, reason: dto::PairProblem) -> Vec<Effect> {
        let Some(peer) = self.active.get(&id).map(|s| s.view.peer) else {
            return Vec::new();
        };
        let mut effects = self.end(id, reason.clone());
        effects.insert(
            0,
            Effect::Send {
                peer,
                message: ended(id, reason),
            },
        );
        effects
    }
    fn end(&mut self, id: PairId, reason: dto::PairProblem) -> Vec<Effect> {
        if !self.finish(id, reason.clone(), ended(id, reason.clone())) {
            return Vec::new();
        }
        vec![Effect::Changed, Effect::Ended { id, problem: reason }]
    }
    fn finish(&mut self, id: PairId, reason: dto::PairProblem, acknowledgement: Message) -> bool {
        let Some(mut s) = self.active.remove(&id) else {
            return false;
        };
        self.changed(id);
        s.view.phase = Phase::Ended;
        s.view.terminal = Some(reason.clone());
        s.view.revision = self.revision;
        self.cache(
            id,
            Terminal {
                peer: s.view.peer,
                acknowledgement,
                view: Some(s.view),
            },
        );
        true
    }
    pub fn lost(&mut self, peer: Boot) -> Vec<Effect> {
        let ids = self
            .active
            .iter()
            .filter(|(_, s)| s.view.peer == peer)
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        ids.into_iter()
            .flat_map(|id| self.end(id, problem(dto::PairCode::LinkLost, dto::PairStage::Connect)))
            .collect()
    }
    pub fn shutdown(&mut self) -> Vec<Effect> {
        let ids = self.active.keys().copied().collect::<Vec<_>>();
        ids.into_iter()
            .flat_map(|id| self.fail(id, problem(dto::PairCode::PeerClosed, dto::PairStage::Connect)))
            .collect()
    }
    pub fn tick(&mut self, now: Instant) -> Vec<Effect> {
        let expired = self
            .active
            .iter()
            .filter(|(_, s)| s.deadline.is_some_and(|d| now >= d))
            .map(|(&id, s)| {
                (
                    id,
                    match s.view.phase {
                        Phase::CheckingCode => dto::PairStage::Code,
                        Phase::Preparing => dto::PairStage::Prepare,
                        Phase::Committing => dto::PairStage::Commit,
                        _ => dto::PairStage::Adopt,
                    },
                )
            })
            .collect::<Vec<_>>();
        expired
            .into_iter()
            .flat_map(|(id, stage)| self.fail(id, problem(dto::PairCode::NetworkTimeout, stage)))
            .collect()
    }
}
pub fn key(id: PairId) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(wire::pair(id).encode(512).expect("pair identity"))
}
pub fn parse_key(value: &str) -> Option<PairId> {
    use base64::Engine;
    if value.len() > 700 {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value).ok()?;
    let pair = dto::PairId::from_raw(v1::cbor::decode_map(&bytes, 512).ok()?).ok()?;
    wire::read_pair(&pair).ok()
}
pub fn problem(code: dto::PairCode, stage: dto::PairStage) -> dto::PairProblem {
    dto::PairProblem::from_raw([(1, code.write()), (2, stage.write())].into()).expect("typed problem")
}
fn with(p: dto::PairProblem, key: u32, value: RawValue) -> dto::PairProblem {
    let mut raw = p.into_raw();
    raw.insert(key, value);
    dto::PairProblem::from_raw(raw).expect("problem field")
}
fn protocol(_: v1::Error) -> dto::PairProblem {
    problem(dto::PairCode::ProtocolViolation, dto::PairStage::Code)
}
fn queued(id: PairId) -> Message {
    Message::ConfigPairQueued(dto::PairQueued::from_raw([(1, wire::pair(id).write())].into()).expect("queued"))
}
fn accepted(id: PairId) -> Message {
    Message::ConfigPairAccept(dto::PairAccept::from_raw([(1, wire::pair(id).write())].into()).expect("accepted"))
}
fn ended(id: PairId, p: dto::PairProblem) -> Message {
    Message::ConfigPairEnded(
        dto::PairEnded::from_raw([(1, wire::pair(id).write()), (2, p.write())].into()).expect("ended"),
    )
}
fn feedback(id: PairId, attempt: u8, kind: dto::PairFeedbackKind) -> Message {
    Message::ConfigPairFeedback(
        dto::PairFeedback::from_raw(
            [
                (1, wire::pair(id).write()),
                (2, attempt.write()),
                (3, kind.write()),
                (4, (3 - attempt).write()),
            ]
            .into(),
        )
        .expect("feedback"),
    )
}
fn pake(id: PairId, attempt: u8, kind: dto::PairStepKind, body: RawValue) -> Message {
    Message::ConfigPairPake(
        dto::PairPake::from_raw(
            [
                (1, wire::pair(id).write()),
                (2, RawValue::from(dto::PAIRING_SUITE)),
                (3, RawValue::Map([(1, kind.write()), (2, body)].into())),
                (4, attempt.write()),
            ]
            .into(),
        )
        .expect("pake"),
    )
}
fn message_pair(message: &Message) -> Result<PairId> {
    let pair = match message {
        Message::ConfigPairQueued(v) => v.pair_id(),
        Message::ConfigPairAccept(v) => v.pair_id(),
        Message::ConfigPairDeclined(v) => v.pair_id(),
        Message::ConfigPairCancel(v) => v.pair_id(),
        Message::ConfigPairEnded(v) => v
            .pair_id()
            .and_then(|id| id.ok_or(v1::ErrorCode::MalformedFrame.into())),
        Message::ConfigPairPake(v) => v.pair_id(),
        Message::ConfigPairFeedback(v) => v.pair_id(),
        _ => return Err(protocol(v1::ErrorCode::StateConflict.into())),
    }
    .map_err(protocol)?;
    wire::read_pair(&pair).map_err(protocol)
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions")
            .field("active", &self.active.len())
            .field("terminal", &self.terminal.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn boot(n: u8) -> Boot {
        Boot {
            device_id: DeviceId([n; 32]),
            nonce: [n; 16],
        }
    }
    fn transcript() -> Transcript {
        Transcript {
            hello: vec![0, 13, 1, 0, 0, 0, 0xa0],
            info: vec![1, 13, 1, 0, 0, 0, 0xa0],
        }
    }
    fn context(i: Boot, r: Boot, n: u8) -> dto::PairContext {
        dto::PairContext::from_raw(
            [
                (
                    1,
                    wire::pair(PairId {
                        initiator_boot: i,
                        random: [n; 16],
                    })
                    .write(),
                ),
                (2, wire::boot(i).write()),
                (3, wire::boot(r).write()),
                (4, RawValue::Bytes(vec![1; 16])),
                (5, RawValue::Bytes(vec![2; 16])),
                (6, 1u8.write()),
                (7, 1u8.write()),
                (8, RawValue::Bytes(vec![2; 16])),
                (9, RawValue::Bytes(vec![3; 32])),
                (10, RawValue::from(dto::PAIRING_SUITE)),
                (11, RawValue::Bytes(vec![4; 32])),
                (12, RawValue::Bytes(vec![5; 32])),
            ]
            .into(),
        )
        .unwrap()
    }
    fn request(i: Boot, r: Boot, n: u8) -> dto::PairRequest {
        let c = context(i, r, n);
        dto::PairRequest::from_raw([(1, c.pair_id().unwrap().write()), (2, c.write())].into()).unwrap()
    }
    fn sent(effects: Vec<Effect>) -> Vec<Message> {
        effects
            .into_iter()
            .filter_map(|e| match e {
                Effect::Send { message, .. } => Some(message),
                _ => None,
            })
            .collect()
    }
    fn one(effects: Vec<Effect>) -> Message {
        sent(effects).into_iter().next().unwrap()
    }
    fn connect(i: &mut Sessions, r: &mut Sessions, n: u8, now: Instant) -> PairId {
        let Message::ConfigPairRequest(req) =
            one(i.start(context(i.local, r.local, n), r.local, transcript()).unwrap())
        else {
            panic!()
        };
        let id = wire::read_pair(&req.pair_id().unwrap()).unwrap();
        let reply = one(r.request(req, i.local, transcript(), false, now).unwrap());
        i.receive(r.local, reply, now).unwrap();
        let reply = one(r.accept(id, r.view(id).unwrap().revision).unwrap());
        i.receive(r.local, reply, now).unwrap();
        id
    }
    #[test]
    fn waiting_has_no_deadline_and_crossed_requests_remain_independent() {
        let now = Instant::now();
        let mut a = Sessions::new(boot(1));
        let mut b = Sessions::new(boot(2));
        let ab = connect(&mut a, &mut b, 1, now);
        let ba = connect(&mut b, &mut a, 2, now);
        assert_eq!(a.snapshot().sessions.len(), 2);
        assert_eq!(b.snapshot().sessions.len(), 2);
        assert!(a.tick(now + Duration::from_secs(86_400)).is_empty());
        assert!(b.tick(now + Duration::from_secs(86_400)).is_empty());
        assert!(a.next_deadline().is_none());
        assert!(b.next_deadline().is_none());
        let cancel = one(a.cancel(ab, a.view(ab).unwrap().revision).unwrap());
        let ack = one(b.receive(a.local, cancel.clone(), now).unwrap());
        assert!(a.receive(b.local, ack.clone(), now).unwrap().is_empty());
        assert_eq!(one(b.receive(a.local, cancel, now).unwrap()), ack);
        assert!(a.active.contains_key(&ba));
        assert!(b.active.contains_key(&ba));
        assert_eq!(
            b.submit(ab, 0, 1, "123456", now).unwrap_err().code().unwrap(),
            dto::PairCode::PairClosed
        );
        assert_eq!(parse_key(&key(ab)), Some(ab));
        assert!(parse_key("invalid").is_none());
    }
    #[test]
    fn pending_cap_is_atomic_and_block_precedes_it_without_receiver_event() {
        let now = Instant::now();
        let mut r = Sessions::new(boot(9));
        for n in 1..=3 {
            let effects = r
                .request(request(boot(n), r.local, n), boot(n), transcript(), false, now)
                .unwrap();
            assert!(matches!(one(effects), Message::ConfigPairQueued(_)));
        }
        for (blocked, expected) in [
            (true, dto::PairCode::Declined),
            (false, dto::PairCode::TooManyPendingRequests),
        ] {
            let before = r.snapshot().revision;
            let req = request(boot(4), r.local, if blocked { 4 } else { 5 });
            let effects = r.request(req.clone(), boot(4), transcript(), blocked, now).unwrap();
            assert_eq!(effects.len(), 1);
            let Message::ConfigPairDeclined(declined) = one(effects) else {
                panic!()
            };
            assert_eq!(declined.reason().unwrap().code().unwrap(), expected);
            if blocked {
                assert_eq!(declined.reason().unwrap().raw().len(), 2);
            }
            assert_eq!(r.snapshot().revision, before);
            assert_eq!(r.snapshot().requests.len(), 3);
            let row = r.snapshot().requests[0].clone();
            r.accept(row.id, row.revision).unwrap();
            assert!(matches!(
                one(r.request(req, boot(4), transcript(), false, now).unwrap()),
                Message::ConfigPairDeclined(_)
            ));
            if blocked {
                r.request(request(boot(5), r.local, 6), boot(5), transcript(), false, now)
                    .unwrap();
            }
        }
    }
    #[test]
    fn blocking_pending_requests_declines_peer_without_local_failure() {
        let now = Instant::now();
        let mut initiator = Sessions::new(boot(1));
        let mut receiver = Sessions::new(boot(2));
        let Message::ConfigPairRequest(request) = one(initiator
            .start(
                context(initiator.local, receiver.local, 1),
                receiver.local,
                transcript(),
            )
            .unwrap())
        else {
            panic!()
        };
        let id = wire::read_pair(&request.pair_id().unwrap()).unwrap();
        let queued = one(receiver
            .request(request.clone(), initiator.local, transcript(), false, now)
            .unwrap());
        initiator.receive(receiver.local, queued, now).unwrap();
        let before = receiver.snapshot().revision;
        let effects = receiver.block_pending(initiator.local.device_id);
        assert!(effects.iter().any(|e| matches!(e, Effect::Changed)));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Ended { .. })),
            "a durable local block is an expected request dismissal, not a pairing failure"
        );
        let declined = one(effects);
        let Message::ConfigPairDeclined(ref message) = declined else {
            panic!()
        };
        assert_eq!(
            message.reason().unwrap(),
            problem(dto::PairCode::Declined, dto::PairStage::Request)
        );
        assert!(receiver.snapshot().requests.is_empty());
        assert!(receiver.snapshot().revision > before);
        assert!(receiver.block_pending(initiator.local.device_id).is_empty());
        assert_eq!(
            one(receiver
                .request(request, initiator.local, transcript(), true, now)
                .unwrap()),
            declined,
            "retries must retain the ordinary decline response"
        );
        assert!(
            initiator
                .receive(receiver.local, declined, now)
                .unwrap()
                .iter()
                .any(|effect| {
                    matches!(effect, Effect::Ended { id: ended, problem } if *ended == id
                && problem.code().unwrap() == dto::PairCode::Declined)
                })
        );
    }
    #[test]
    fn block_accept_order_and_rate_budget_survive_boot_changes() {
        let now = Instant::now();
        let mut r = Sessions::new(boot(9));
        let peer = boot(1);
        let req = request(peer, r.local, 1);
        let id = wire::read_pair(&req.pair_id().unwrap()).unwrap();
        r.request(req, peer, transcript(), false, now).unwrap();
        let rev = r.view(id).unwrap().revision;
        r.block_pending(peer.device_id);
        assert_eq!(
            r.accept(id, rev).unwrap_err().cause().unwrap(),
            Some(dto::PairCode::Declined)
        );
        let mut peer2 = peer;
        peer2.nonce = [8; 16];
        for n in 2..=4 {
            assert_eq!(
                r.request(request(peer2, r.local, n), peer2, transcript(), true, now)
                    .unwrap()
                    .len(),
                1
            );
        }
        let Message::ConfigPairDeclined(d) = one(r
            .request(request(peer2, r.local, 5), peer2, transcript(), true, now)
            .unwrap())
        else {
            panic!()
        };
        assert_eq!(d.reason().unwrap().code().unwrap(), dto::PairCode::RateLimited);
        let now = now + Duration::from_secs(10);
        let req = request(peer2, r.local, 6);
        let id = wire::read_pair(&req.pair_id().unwrap()).unwrap();
        r.request(req, peer2, transcript(), false, now).unwrap();
        r.accept(id, r.view(id).unwrap().revision).unwrap();
        assert!(r.block_pending(peer.device_id).is_empty());
        assert!(r.active.contains_key(&id));
    }
    #[test]
    fn third_correct_attempt_succeeds_and_retransmissions_do_not_consume_attempts() {
        let now = Instant::now();
        let mut i = Sessions::new(boot(1));
        let mut r = Sessions::new(boot(2));
        let id = connect(&mut i, &mut r, 1, now);
        let code = i.code(id).unwrap();
        let wrong = if *code == "111111" { "222222" } else { "111111" };
        assert_eq!(
            r.submit(id, r.view(id).unwrap().revision, 1, "12345", now)
                .unwrap_err()
                .code()
                .unwrap(),
            dto::PairCode::InvalidCodeSyntax
        );
        for attempt in 1..=3 {
            let code = if attempt == 3 { code.as_str() } else { wrong };
            let start = one(r.submit(id, r.view(id).unwrap().revision, attempt, code, now).unwrap());
            let reply = one(i.receive(r.local, start.clone(), now).unwrap());
            assert_eq!(one(i.receive(r.local, start, now).unwrap()), reply);
            let finish = one(r.receive(i.local, reply.clone(), now).unwrap());
            assert_eq!(one(r.receive(i.local, reply, now).unwrap()), finish);
            let replies = i.receive(r.local, finish, now).unwrap();
            if attempt == 3 {
                assert!(replies.iter().any(|e| matches!(e,Effect::Verified(x) if *x==id)));
                let verified = one(replies);
                let effects = r.receive(i.local, verified, now).unwrap();
                assert!(effects.iter().any(|e| matches!(e,Effect::Verified(x) if *x==id)));
            } else {
                assert!(sent(replies).is_empty());
                assert_eq!(r.view(id).unwrap().phase, Phase::EnterCode);
                assert_eq!(i.view(id).unwrap().phase, Phase::ShowCode);
            }
        }
        assert!(i.code(id).is_err());
        assert_eq!(i.view(id).unwrap().phase, Phase::Preparing);
        assert_eq!(r.view(id).unwrap().phase, Phase::Preparing);
    }
    #[test]
    fn verified_feedback_cannot_replace_pake_confirmation_and_submit_times_out() {
        let now = Instant::now();
        let mut i = Sessions::new(boot(1));
        let mut r = Sessions::new(boot(2));
        let id = connect(&mut i, &mut r, 1, now);
        r.submit(id, r.view(id).unwrap().revision, 1, "111111", now).unwrap();
        assert!(
            r.receive(i.local, feedback(id, 1, dto::PairFeedbackKind::Verified), now)
                .unwrap()
                .is_empty()
        );
        assert_eq!(r.view(id).unwrap().phase, Phase::CheckingCode);
        let effects = r.tick(now + Duration::from_secs(10));
        assert!(effects.iter().any(|e| matches!(e, Effect::Ended { .. })));
        assert_eq!(
            r.view(id).unwrap().terminal.as_ref().unwrap().code().unwrap(),
            dto::PairCode::NetworkTimeout
        );
    }
}
