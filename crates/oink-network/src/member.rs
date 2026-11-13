//! Group-member decisions shared by every Oink owner (desktop and phone).
//!
//! This module decides; it performs no network or group-store I/O. Inputs are
//! the pairing journal and merge transactions it owns, the owner's current
//! signed `State`, a read-only view of the owner's C0 sessions ([`Links`]) and
//! the received Message. Outputs ([`Out`]) are executed in order by the owner:
//! send a Message, commit a state through the owner's own pipeline, mark a
//! session, report a pairing outcome. Session tables, the commit/apply/layout
//! pipeline, persistence and dialing stay with each owner.
//!
//! Covered here, for both ends: admission, revocation and its tombstone, merge
//! adoption and proof service for old-root members, durable commit-decision
//! recovery, permit acquisition and release, supersede, startup journal
//! recovery and projection replay, pairing request entry checks, the mapping of
//! merge failures to pairing problems, and the rule for unhandled messages.
use crate::{
    admission, group_merge,
    group_state::State,
    identity::Identity,
    link::{Disconnect, LinkId},
    pair_journal::{self, Decision, Journal, Recovery},
    pair_merge::{self, Transactions},
    pair_objects,
    pairing::{self, Admission, Phase, Sessions},
    roster::Anchor,
    wire_identity as wire,
};
use oink_foundation::{
    DeviceId,
    session::{Boot, PairId},
};
use oink_protocol::v2::{
    self, Message, RawMap, RawValue, dto,
    schema::{WireDto, WireValue},
};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

mod recovery;
#[cfg(test)]
mod tests;

/// A proof transfer or lookup answer that makes no progress for this long is
/// reported as a timeout of the migration (the C0 keeps its own admission
/// lifetime).
const PROOF_DEADLINE: Duration = Duration::from_secs(10);
/// A durable decision answered `Pending` is asked again after this delay while
/// its target stays connected (the target is still running that pairing).
const REQUERY: Duration = Duration::from_secs(5);
/// Merge and proof objects staged per owner, and per C0.
const STAGING_BYTES: usize = 4 * 1024 * 1024;
const STAGING_PER_LINK: usize = 2;

/// Migration progress of this member's own group root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationPhase {
    ProofWaiting,
    Adopting,
    Adopted,
    Conflict,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Migration {
    pub source: Anchor,
    pub target: Option<Anchor>,
    pub phase: MigrationPhase,
    pub problem: Option<dto::PairProblem>,
}

/// One action for the owner, executed in order. Any failure of a `Commit`
/// stops the remaining actions of that call and closes the C0 it came from.
#[derive(Debug)]
pub enum Out {
    /// Post on this exact C0.
    Send {
        link: LinkId,
        message: Message,
    },
    /// Post on the peer's currently selected C0; nothing if it has none.
    SendPeer {
        peer: Boot,
        message: Message,
    },
    /// (Re)arm the bounded lifetime of this not yet admitted C0.
    ArmAdmission {
        link: LinkId,
    },
    /// Old-root proof traffic may use this C0 even when it is relayed.
    OldMemberLane {
        link: LinkId,
    },
    /// Send our admission Request on this C0.
    BeginAdmission {
        link: LinkId,
    },
    /// Send our admission Request on the peer's selected C0, if any.
    AdmitPeer {
        peer: Boot,
    },
    /// Save and apply this signed state through the owner's pipeline.
    Commit(State),
    /// The C0 answered our Request; record this accepted state, half-open it.
    Accepted {
        link: LinkId,
        state: State,
    },
    HalfOpen {
        link: LinkId,
    },
    /// Admission finished on this C0: owner bookkeeping, then [`Membership::ready`].
    Ready {
        link: LinkId,
    },
    /// After `Ready`: publish our state if it differs from the acknowledged one.
    SendStateIfDiffers {
        link: LinkId,
        acknowledged: [u8; 32],
    },
    /// A member we removed greeted with our root: keep a short lifetime and
    /// wait for its Request.
    AwaitRemoved {
        link: LinkId,
    },
    /// Our signed state was sent to a removed member: business authority ends
    /// now, the C0 drains and is closed within a bounded time.
    RetireRemoved {
        link: LinkId,
    },
    /// Close every C0 of these devices (their removal was applied).
    CloseMembers {
        devices: Vec<DeviceId>,
        reason: Disconnect,
    },
    /// A signed state removes this device: commit it, close every C0 and form
    /// a fresh one-device group.
    Revoked(State),
    /// The journal holds a committed projection: run [`Membership::projection`].
    Project,
    /// The pairing snapshot changed.
    PairingChanged,
    /// Both receipts are exchanged: run [`Membership::complete`] with the
    /// owner's outcome record.
    Completed {
        pair: PairId,
        bundle: dto::MergeTransferBundle,
        reconciled: bool,
    },
    /// A pairing ended with this reason (after the transaction was settled).
    PairingFailed {
        pair: PairId,
        problem: dto::PairProblem,
    },
    /// Decision recovery learned that the target aborted this decision.
    RecoveredFailure {
        recovery: Recovery,
        problem: dto::PairProblem,
    },
    Migration(Migration),
}

/// What the owner knows about one of its C0 sessions.
#[derive(Clone, Debug)]
pub struct LinkFacts {
    pub boot: Boot,
    /// The root the peer greeted with (immutable for this C0).
    pub greeted: Anchor,
    pub ready: bool,
    pub departing: bool,
    pub admitted: Option<Anchor>,
}

/// Read-only view of the owner's C0 table.
pub trait Links {
    fn facts(&self, link: LinkId) -> Option<LinkFacts>;
    /// Every greeted C0 (ready or not).
    fn greeted(&self) -> Vec<(LinkId, LinkFacts)>;
}

/// Request admission policy of this owner (the decision is shared).
#[derive(Clone, Copy, Debug, Default)]
pub struct Gate {
    /// Pairing is offered at all on this owner.
    pub enabled: bool,
    pub blocked: bool,
    /// One pairing at a time (desktop welcome connect step).
    pub exclusive: bool,
    /// An owner-side pairing step not visible in the sessions (a desktop's
    /// pending connect) makes an exclusive owner busy.
    pub external_busy: bool,
}

/// What cancelling a pairing did (protocol §6.6 cancel/commit boundary).
#[derive(Debug)]
pub enum Cancelled {
    Cancelled,
    CommitUnconfirmed,
    AlreadyCommitted(dto::MergeTransferBundle),
}

/// A committed projection ready to be installed by the owner.
#[derive(Debug)]
pub struct Projection {
    pub candidate: State,
    pub digest: [u8; 32],
    /// The decision's bundle (owner hooks such as layout-profile handoff).
    pub bundle: Option<dto::MergeTransferBundle>,
    /// The root changes: these ready C0s lose their admission now.
    pub retire: Vec<LinkId>,
    /// Gossip of the signed transition and superseded pairings (once).
    pub outs: Vec<Out>,
}

#[derive(Clone, Debug)]
pub(crate) enum Purpose {
    Adoption(Anchor),
    Decision { pair: PairId, decision: u64 },
}

#[derive(Clone, Debug)]
pub(crate) struct Expected {
    reference: dto::ObjectRef,
    context: dto::ObjectContext,
    purpose: Purpose,
    deadline: Instant,
}

/// After a projection: continue on this C0.
#[derive(Debug)]
struct Followup {
    link: LinkId,
    receipt: Option<Message>,
    follow_greeting: bool,
}

impl std::fmt::Debug for Membership {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Membership")
            .field("pairs", &self.pairs.snapshot().revision)
            .field("lookups", &self.lookups.len())
            .field("expected", &self.expected.len())
            .finish_non_exhaustive()
    }
}

pub struct Membership {
    pub pairs: Sessions,
    pub transactions: Transactions,
    pub journal: Journal,
    identity: Arc<Identity>,
    objects: pair_objects::Objects,
    expected: HashMap<(LinkId, [u8; 32]), Expected>,
    lookups: HashMap<LinkId, (Anchor, Instant)>,
    recovery_due: HashMap<PairId, Instant>,
    followups: Vec<Followup>,
    announced: Option<[u8; 32]>,
    /// Merge effects waiting behind a projection the owner has not installed.
    pending: VecDeque<pair_merge::Effect>,
    /// A projection is out and not yet installed: `pending` stays queued even
    /// when another path (a superseded pairing releasing its permit) re-enters
    /// [`Membership::merge_effects`] while the owner installs it.
    installing: bool,
    migration: Option<Migration>,
}

fn violation(code: v2::ErrorCode) -> v2::Error {
    code.into()
}

fn journal_error(_: pair_journal::Error) -> v2::Error {
    v2::ErrorCode::StateConflict.into()
}

fn merge_error(_: pair_merge::Error) -> v2::Error {
    v2::ErrorCode::StateConflict.into()
}

/// The reason a failed merge step ends its pairing with (same on every owner).
pub fn merge_problem(error: &pair_merge::Error) -> dto::PairProblem {
    let code = match error {
        pair_merge::Error::Journal(pair_journal::Error::Io(_)) => dto::PairCode::LocalStorageFailed,
        pair_merge::Error::Wire(error) => match error.code {
            v2::ErrorCode::UnsupportedValue => dto::PairCode::UnsupportedLayout,
            v2::ErrorCode::LimitExceeded => dto::PairCode::MergeTooLarge,
            v2::ErrorCode::AuthenticationFailed => dto::PairCode::AuthenticationFailed,
            _ => dto::PairCode::StateChanged,
        },
        _ => dto::PairCode::MergeConflict,
    };
    pairing::problem(code, dto::PairStage::Prepare)
}

/// A peer's pairing reason becomes a local record and API value. Its optional
/// PairId must re-encode within the local bound (H16).
pub fn validate_problem(problem: &dto::PairProblem) -> v2::Result<()> {
    problem.code()?;
    problem.stage()?;
    if let Some(winner) = problem.winner()? {
        winner.encode(8192)?;
    }
    problem.retry_after_ms()?;
    problem.attempts_left()?;
    problem.needed()?;
    problem.limit()?;
    problem.protocol_code()?;
    problem.limit_kind()?;
    problem.cause()?;
    Ok(())
}

/// The answer to a C0 Message this owner does not handle although its
/// capability was negotiated: a typed error on the Message scope; the C0 stays.
pub fn unhandled(link: LinkId, message: &Message) -> v2::Result<Out> {
    Ok(Out::Send {
        link,
        message: Message::ProtocolError(dto::ProtocolError::from_raw(
            [
                (1, (v2::ErrorCode::UnknownMessage.number() as u16).write()),
                (2, dto::ErrorScope::Message.write()),
                (3, message.type_id().write()),
            ]
            .into(),
        )?),
    })
}

/// Pairing (request/PAKE/merge) and old-root recovery Messages: everything
/// [`Membership::receive`] owns. Admission (`ConfigRequest/Reply/Done`) and
/// `ConfigState` have their own entry points.
pub fn handles(message: &Message) -> bool {
    matches!(
        message,
        Message::ConfigPairRequest(_)
            | Message::ConfigPairQueued(_)
            | Message::ConfigPairAccept(_)
            | Message::ConfigPairDeclined(_)
            | Message::ConfigPairPake(_)
            | Message::ConfigPairFeedback(_)
            | Message::ConfigPairCancel(_)
            | Message::ConfigPairEnded(_)
            | Message::ConfigPairPrepare(_)
            | Message::ConfigPairReady(_)
            | Message::ConfigPairCommitPermit(_)
            | Message::ConfigPairCommit(_)
            | Message::ConfigPairCommitted(_)
            | Message::ConfigPairObjectBegin(_)
            | Message::ConfigPairObjectChunk(_)
            | Message::ConfigPairObjectEnd(_)
            | Message::ConfigPairReceipt(_)
            | Message::ConfigPairDecisionQuery(_)
            | Message::ConfigPairDecisionReply(_)
            | Message::ConfigMergeLookup(_)
            | Message::ConfigMergeTransfer(_)
            | Message::ConfigMergeReceipt(_)
    )
}

impl Membership {
    /// Load the journal, fence uncommitted decisions, and return the committed
    /// projection the previous process did not install. The owner writes it to
    /// its group store, then calls [`Membership::replayed`].
    pub fn start(
        path: &Path,
        identity: Arc<Identity>,
        boot: Boot,
    ) -> Result<(Self, Option<dto::GroupState>), pair_journal::Error> {
        let mut journal = Journal::load(path, identity.id())?;
        journal.recover()?;
        let pending = journal.pending_projection()?;
        Ok((
            Self {
                pairs: Sessions::new(boot),
                transactions: Transactions::default(),
                journal,
                identity,
                objects: Default::default(),
                expected: HashMap::new(),
                lookups: HashMap::new(),
                recovery_due: HashMap::new(),
                followups: Vec::new(),
                announced: None,
                pending: VecDeque::new(),
                installing: false,
                migration: None,
            },
            pending,
        ))
    }

    /// The replayed projection is in the owner's store: `digest` is the
    /// digest of exactly the journal's retained state that was written.
    pub fn replayed(&mut self, digest: [u8; 32]) -> Result<(), pair_journal::Error> {
        self.journal.projected(digest)
    }

    /// Look up the transition out of our root on this C0 now.
    pub fn request_lookup(
        &mut self,
        link: LinkId,
        facts: &LinkFacts,
        current: &State,
        now: Instant,
    ) -> v2::Result<Vec<Out>> {
        let mut out = Vec::new();
        self.lookup(link, facts, current, now, &mut out)?;
        Ok(out)
    }

    pub fn lookups_pending(&self) -> bool {
        !self.lookups.is_empty()
    }

    /// A proof object with this digest is expected on this C0.
    pub fn expects_proof(&self, link: LinkId, digest: [u8; 32]) -> bool {
        self.expected.contains_key(&(link, digest))
    }

    pub fn expects_any_proof(&self) -> bool {
        !self.expected.is_empty()
    }

    /// A decision query waits for its next round.
    pub fn recovery_scheduled(&self) -> bool {
        !self.recovery_due.is_empty()
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn inject_lookup(&mut self, link: LinkId, source: Anchor, deadline: Instant) {
        self.lookups.insert(link, (source, deadline));
    }

    /// Our MergeLookup on this C0 is outstanding.
    pub fn looking_up(&self, link: LinkId) -> bool {
        self.lookups.contains_key(&link)
    }

    pub fn migration(&self) -> Option<&Migration> {
        self.migration.as_ref()
    }

    fn report(&mut self, out: &mut Vec<Out>, migration: Migration) {
        self.migration = Some(migration.clone());
        out.push(Out::Migration(migration));
    }

    /// The C0 is gone: its staging and lookups go with it; a pairing whose
    /// peer has no replacement C0 ends with LinkLost.
    pub fn link_lost(&mut self, link: LinkId, peer: Option<Boot>, current: &State) -> Vec<Out> {
        self.transactions.drop_link(link.0);
        self.objects.drop_link(link.0);
        self.expected.retain(|(id, _), _| *id != link);
        self.lookups.remove(&link);
        self.followups.retain(|f| f.link != link);
        match peer {
            Some(peer) => {
                let effects = self.pairs.lost(peer);
                self.pairing_effects(effects, current)
            }
            None => Vec::new(),
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        [
            self.transactions.next_deadline(),
            self.objects.next_deadline(),
            self.pairs.next_deadline(),
            self.expected.values().map(|e| e.deadline).min(),
            self.lookups.values().map(|(_, at)| *at).min(),
            self.recovery_due.values().copied().min(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Periodic owner tick: decision queries, proof and object deadlines,
    /// permits that waited for a release, pairing phase deadlines.
    pub fn tick(&mut self, now: Instant, current: &State, links: &dyn Links) -> Vec<Out> {
        let mut out = Vec::new();
        for (link, facts) in links.greeted() {
            if !facts.departing {
                let _ = self.reconcile_peer(link, &facts, now, &mut out);
            }
        }
        self.expire_recovery(now, links, &mut out);
        for pair in self.transactions.expire(now) {
            let effects = self.pairs.fail(
                pair,
                pairing::problem(dto::PairCode::NetworkTimeout, dto::PairStage::Prepare),
            );
            out.extend(self.pairing_effects(effects, current));
        }
        out.extend(self.retry_permits(current));
        let effects = self.pairs.tick(now);
        out.extend(self.pairing_effects(effects, current));
        out
    }

    fn retry_permits(&mut self, current: &State) -> Vec<Out> {
        match self
            .transactions
            .retry_permits(&self.identity, current, &mut self.journal)
        {
            Ok(effects) => self.merge_effects(effects, current),
            Err(_) => Vec::new(),
        }
    }

    /// Effects of a pairing session operation (API command or received step).
    pub fn pairing_effects(&mut self, effects: Vec<pairing::Effect>, current: &State) -> Vec<Out> {
        let mut out = Vec::new();
        let mut queue = VecDeque::from(effects);
        let mut released = false;
        while let Some(effect) = queue.pop_front() {
            match effect {
                pairing::Effect::Send { peer, message } => out.push(Out::SendPeer { peer, message }),
                pairing::Effect::Changed | pairing::Effect::Completed(_) => out.push(Out::PairingChanged),
                pairing::Effect::Verified(pair) => {
                    let Some(view) = self.pairs.view(pair).cloned() else {
                        continue;
                    };
                    match self.transactions.verified(view, current, &mut self.journal) {
                        Ok(effects) => out.extend(self.merge_effects(effects, current)),
                        Err(_) => queue.extend(self.pairs.fail(
                            pair,
                            pairing::problem(dto::PairCode::LocalStorageFailed, dto::PairStage::Prepare),
                        )),
                    }
                }
                pairing::Effect::Ended { id, mut problem } => {
                    // A durable decision is not withdrawn; an uncertain one
                    // stays fenced for decision recovery; otherwise the permit
                    // is released so the same devices can pair again (H15).
                    match self.transactions.cancel(id, &mut self.journal) {
                        Ok(pair_merge::Cancel::Committed(_)) => {
                            out.push(Out::PairingChanged);
                            continue;
                        }
                        Ok(pair_merge::Cancel::Unconfirmed) => {
                            problem = pairing::problem(dto::PairCode::CommitUnconfirmed, dto::PairStage::Commit)
                        }
                        Ok(pair_merge::Cancel::Cancelled) => released = true,
                        Err(_) => problem = pairing::problem(dto::PairCode::LocalStorageFailed, dto::PairStage::Commit),
                    }
                    self.pairs.terminal_problem(id, problem.clone());
                    out.push(Out::PairingChanged);
                    out.push(Out::PairingFailed { pair: id, problem });
                }
            }
        }
        if released {
            out.extend(self.retry_permits(current));
        }
        out
    }

    /// Effects of the merge transaction, in order. A projection suspends the
    /// rest until the owner installed it ([`Membership::projected`]), also
    /// against re-entry from a pairing that ends meanwhile (M34).
    pub fn merge_effects(&mut self, effects: Vec<pair_merge::Effect>, current: &State) -> Vec<Out> {
        self.pending.extend(effects);
        let mut out = Vec::new();
        while !self.installing
            && let Some(effect) = self.pending.pop_front()
        {
            match effect {
                pair_merge::Effect::Send { peer, message } => {
                    if matches!(
                        message,
                        Message::ConfigPairCommit(_)
                            | Message::ConfigPairCommitPermit(_)
                            | Message::ConfigPairCommitted(_)
                    ) && let Some(pair) = message
                        .raw()
                        .get(&1)
                        .and_then(|raw| dto::PairId::read(raw).ok())
                        .and_then(|id| wire::read_pair(&id).ok())
                        && let Ok(effects) = self.pairs.progress(pair, Phase::Committing, Instant::now())
                    {
                        out.extend(self.pairing_effects(effects, current));
                    }
                    out.push(Out::SendPeer { peer, message });
                }
                pair_merge::Effect::Project => {
                    self.installing = true;
                    out.push(Out::Project);
                }
                pair_merge::Effect::Admit { pair, peer } => {
                    for phase in [Phase::Committing, Phase::Admitting] {
                        if let Ok(effects) = self.pairs.progress(pair, phase, Instant::now()) {
                            out.extend(self.pairing_effects(effects, current));
                        }
                    }
                    out.push(Out::AdmitPeer { peer });
                }
                pair_merge::Effect::Completed {
                    pair,
                    bundle,
                    reconciled,
                } => out.push(Out::Completed {
                    pair,
                    bundle,
                    reconciled,
                }),
            }
        }
        out
    }

    /// Record a completed decision with the owner's outcome body (its receipt
    /// releases the endpoint permit), finish the pairing view and end the
    /// sessions this merge made stale. Returns the outcome's event id and
    /// whether it is new.
    pub fn complete(
        &mut self,
        pair: PairId,
        bundle: &dto::MergeTransferBundle,
        event: RawMap,
        current: &State,
    ) -> Result<(u64, bool, Vec<Out>), pair_journal::Error> {
        let certificate = bundle.certificate()?;
        let intent = certificate.intent()?;
        let before = self.journal.last_event_id()?;
        let id = self.journal.receipt(pair, certificate.permit_decision()?, event)?;
        let mut out = Vec::new();
        let now = Instant::now();
        for phase in [Phase::Committing, Phase::Admitting] {
            if let Ok(effects) = self.pairs.progress(pair, phase, now) {
                out.extend(self.pairing_effects(effects, current));
            }
        }
        if let Ok(effects) = self.pairs.complete(pair) {
            out.extend(self.pairing_effects(effects, current));
        }
        out.extend(self.supersede(pair, &intent, current));
        self.transactions.completed(pair);
        out.extend(self.retry_permits(current));
        Ok((id, id > before, out))
    }

    /// Pairings about the source group of a committed merge can no longer
    /// commit (protocol §13.2).
    pub fn supersede(&mut self, winner: PairId, intent: &dto::MergeIntent, current: &State) -> Vec<Out> {
        let source = intent.source().ok().and_then(|s| s.group().ok()).map(|g| g.0);
        let issuers = [intent.source_issuer().ok(), intent.target_issuer().ok()];
        let snapshot = self.pairs.snapshot();
        let mut effects = Vec::new();
        for view in snapshot.requests.into_iter().chain(snapshot.sessions) {
            if view.id == winner || matches!(view.phase, Phase::Completed | Phase::Ended) {
                continue;
            }
            let same = source.is_some()
                && (view.context.source_group().ok().map(|g| g.0) == source
                    || view.context.target_group().ok().map(|g| g.0) == source);
            if !same {
                continue;
            }
            let same_endpoints = issuers
                .iter()
                .flatten()
                .any(|d| wire::read_device(d).ok() == Some(view.peer.device_id));
            let mut reason = pairing::problem(
                if same_endpoints {
                    dto::PairCode::SupersededByPairing
                } else {
                    dto::PairCode::GroupChanged
                },
                dto::PairStage::Commit,
            )
            .into_raw();
            if same_endpoints {
                reason.insert(5, wire::pair(winner).write());
            }
            if let Ok(reason) = dto::PairProblem::from_raw(reason) {
                effects.extend(self.pairs.fail(view.id, reason));
            }
        }
        self.pairing_effects(effects, current)
    }

    /// This device left `group`: pairings whose request named it end.
    pub fn left(&mut self, group: oink_foundation::GroupId, current: &State) -> Vec<Out> {
        let snapshot = self.pairs.snapshot();
        let mut effects = Vec::new();
        for view in snapshot.requests.into_iter().chain(snapshot.sessions) {
            if matches!(view.phase, Phase::Completed | Phase::Ended) {
                continue;
            }
            if view.context.source_group().ok().map(|g| g.0) == Some(group.0)
                || view.context.target_group().ok().map(|g| g.0) == Some(group.0)
            {
                effects.extend(self.pairs.fail(
                    view.id,
                    pairing::problem(dto::PairCode::GroupChanged, dto::PairStage::Commit),
                ));
            }
        }
        self.pairing_effects(effects, current)
    }

    /// A membership edit is refused only while a merge is in flight (its own
    /// deadlines bound it) or a committed projection waits. A request waiting
    /// for a person has no deadline and must not lock the group (L19).
    pub fn edit_allowed(&self) -> Result<bool, pair_journal::Error> {
        Ok(!self
            .pairs
            .snapshot()
            .sessions
            .iter()
            .any(|view| matches!(view.phase, Phase::Preparing | Phase::Committing | Phase::Admitting))
            && self.journal.pending_projection()?.is_none())
    }

    /// Cancel follows the target's durable decision.
    pub fn cancel(
        &mut self,
        pair: PairId,
        revision: u64,
        current: &State,
    ) -> Result<(Cancelled, Vec<Out>), dto::PairProblem> {
        if self.pairs.view(pair).is_some_and(|view| view.revision != revision) {
            return Err(pairing::problem(dto::PairCode::StaleRevision, dto::PairStage::Code));
        }
        match self.transactions.cancel(pair, &mut self.journal) {
            Ok(pair_merge::Cancel::Committed(bundle)) => Ok((Cancelled::AlreadyCommitted(bundle), Vec::new())),
            Ok(pair_merge::Cancel::Unconfirmed) => {
                let effects = self.pairs.fail(
                    pair,
                    pairing::problem(dto::PairCode::CommitUnconfirmed, dto::PairStage::Commit),
                );
                Ok((Cancelled::CommitUnconfirmed, self.pairing_effects(effects, current)))
            }
            Ok(pair_merge::Cancel::Cancelled) => {
                let effects = self.pairs.cancel(pair, revision)?;
                let mut out = self.pairing_effects(effects, current);
                out.extend(self.retry_permits(current));
                Ok((Cancelled::Cancelled, out))
            }
            Err(_) => Err(pairing::problem(
                dto::PairCode::LocalStorageFailed,
                dto::PairStage::Commit,
            )),
        }
    }

    /// The desktop's welcome connect step opened: at most one leftover
    /// request survives, and only when no other pairing is running.
    pub fn open_window(&mut self, external_busy: bool, current: &State) -> Vec<Out> {
        let snapshot = self.pairs.snapshot();
        let busy = external_busy
            || snapshot
                .sessions
                .iter()
                .any(|view| !matches!(view.phase, Phase::Completed | Phase::Ended));
        let mut effects = Vec::new();
        for (index, request) in snapshot.requests.into_iter().enumerate() {
            if index > 0 || busy {
                effects.extend(self.pairs.decline(request.id, request.revision).unwrap_or_default());
            }
        }
        self.pairing_effects(effects, current)
    }

    // ---- C0 messages ----

    /// The C0 greeting completed. `outbound`: this owner dialed.
    pub fn greeted(
        &mut self,
        link: LinkId,
        facts: &LinkFacts,
        outbound: bool,
        current: &State,
        now: Instant,
    ) -> v2::Result<Vec<Out>> {
        let same = facts.greeted == current.roster().anchor();
        let member = current.roster().contains(facts.boot.device_id);
        let mut out = Vec::new();
        if same && !member {
            // A same-root greeting is no membership claim. A device that was
            // never a member is refused; one we removed waits for its Request,
            // answered with the signed removal.
            if current.roster().next_epoch(facts.boot.device_id)? == 1 {
                return Err(violation(v2::ErrorCode::AuthenticationFailed));
            }
            out.push(Out::AwaitRemoved { link });
            return Ok(out);
        }
        self.reconcile_peer(link, facts, now, &mut out)?;
        if same && outbound && member {
            out.push(Out::BeginAdmission { link });
        } else if !same && member {
            // A known member greets with another root: one of us missed a
            // signed transition. Ask for the one out of our root.
            self.lookup(link, facts, current, now, &mut out)?;
        }
        Ok(out)
    }

    /// `ConfigRequest`. `connected` is the owner's ready-link count for the reply.
    pub fn request(
        &mut self,
        link: LinkId,
        facts: &LinkFacts,
        request: &dto::Request,
        current: &State,
        connected: u8,
    ) -> v2::Result<Vec<Out>> {
        let peer = facts.boot.device_id;
        let local = self.identity.id();
        if !current.roster().contains(peer) {
            if facts.greeted != current.roster().anchor() || current.roster().next_epoch(peer)? == 1 {
                return Err(violation(v2::ErrorCode::AuthenticationFailed));
            }
            // We removed this member while it was away. Our signed state
            // carries the removal; it closes after applying it.
            return Ok(vec![
                Out::Send {
                    link,
                    message: admission::reply(current, local, connected)?,
                },
                Out::RetireRemoved { link },
            ]);
        }
        if let Some(candidate) = admission::request_revocation(current, request, local, peer)? {
            return Ok(vec![Out::Revoked(candidate)]);
        }
        let candidate = admission::receive_request(current, request, local, peer)?;
        let reply = admission::reply(&candidate, local, connected)?;
        Ok(vec![
            Out::Commit(candidate.clone()),
            Out::Accepted { link, state: candidate },
            Out::Send { link, message: reply },
        ])
    }

    /// `ConfigReply` to the Request we `sent` on this C0.
    pub fn reply(
        &mut self,
        link: LinkId,
        facts: &LinkFacts,
        sent: &State,
        reply: &dto::Reply,
        current: &State,
    ) -> v2::Result<Vec<Out>> {
        let peer = facts.boot.device_id;
        let local = self.identity.id();
        if !current.roster().contains(peer) {
            return Err(violation(v2::ErrorCode::AuthenticationFailed));
        }
        // A member that removed this device while it was away answers with the
        // signed removal: keep it and form a fresh group.
        if let Some(candidate) = admission::reply_revocation(current, sent, reply, local, peer)? {
            return Ok(vec![Out::Revoked(candidate)]);
        }
        let accepted = admission::receive_reply(current, sent, reply, local, peer)?;
        let acknowledged = accepted.acknowledged.digest()?;
        Ok(vec![
            Out::Commit(accepted.merged),
            // Fence the half-open phase before exposing Done: the peer's first
            // Facts may reach N0 as soon as it reads Done.
            Out::HalfOpen { link },
            Out::Send {
                link,
                message: admission::done(&accepted.acknowledged)?,
            },
            Out::Ready { link },
            Out::SendStateIfDiffers { link, acknowledged },
        ])
    }

    /// `ConfigDone` for the state we `accepted` on this C0.
    pub fn done(&mut self, link: LinkId, accepted: &State, done: &dto::Done) -> v2::Result<Vec<Out>> {
        admission::receive_done(accepted, done)?;
        Ok(vec![Out::Ready { link }])
    }

    /// The owner marked this C0 ready (admitted under `current`).
    pub fn ready(&mut self, link: LinkId, peer: Boot, current: &State) -> v2::Result<Vec<Out>> {
        let mut out = Vec::new();
        let pairs = self
            .pairs
            .snapshot()
            .sessions
            .into_iter()
            .filter(|view| view.peer == peer && self.transactions.bundle(view.id).is_some())
            .map(|view| view.id)
            .collect::<Vec<_>>();
        for pair in pairs {
            let effects = self.transactions.admitted(pair).map_err(merge_error)?;
            out.extend(self.merge_effects(effects, current));
        }
        let anchor = current.roster().anchor();
        if self.lookups.get(&link).is_some_and(|(source, _)| *source == anchor) {
            self.lookups.remove(&link);
            self.report(
                &mut out,
                Migration {
                    source: anchor,
                    target: Some(anchor),
                    phase: MigrationPhase::Adopted,
                    problem: None,
                },
            );
        }
        self.recovery_ready(link, peer.device_id, &mut out)?;
        if let Some(status) = self.migration.clone()
            && status.phase == MigrationPhase::Adopting
        {
            self.report(
                &mut out,
                Migration {
                    phase: MigrationPhase::Adopted,
                    ..status
                },
            );
        }
        Ok(out)
    }

    /// A ready member's `ConfigState`.
    pub fn state_update(
        &mut self,
        link: LinkId,
        peer: DeviceId,
        incoming: &dto::GroupState,
        current: &State,
    ) -> v2::Result<Vec<Out>> {
        let local = self.identity.id();
        let incoming_layout = incoming
            .registers()?
            .0
            .into_iter()
            .find(|register| register.kind() == Ok(2))
            .ok_or(v2::ErrorCode::MalformedFrame)?;
        let order_kind = crate::device_order::REGISTER_KIND;
        let incoming_order = incoming.registers()?.0.into_iter().find(|r| r.kind() == Ok(order_kind));
        let candidate = current.receive_update(incoming, peer, local)?;
        if !candidate.roster().contains(local) {
            return Ok(vec![Out::Revoked(candidate)]);
        }
        // A stale or losing concurrent layout update is valid input, but the
        // sender must learn the register our counter-only merge selected. The
        // device order register converges the same way.
        let resend = candidate.layout()? != incoming_layout || candidate.register(order_kind)? != incoming_order;
        // Absence is a revocation only for a previous member.
        let removed = current
            .roster()
            .members()
            .map(|member| member.device)
            .filter(|device| !candidate.roster().contains(*device))
            .collect::<Vec<_>>();
        let mut out = vec![Out::Commit(candidate.clone())];
        if resend {
            out.push(Out::Send {
                link,
                message: Message::ConfigState(candidate.wire().clone()),
            });
        }
        if !removed.is_empty() {
            out.push(Out::CloseMembers {
                devices: removed,
                reason: Disconnect::MemberRemoved,
            });
        }
        Ok(out)
    }

    /// Whether this group-scoped frame on a not-ready C0 belongs to the root
    /// it was admitted under before a root change (drained, not applied).
    pub fn retired(&self, facts: &LinkFacts, current: &State, group: [u8; 16]) -> Option<Anchor> {
        let old = facts.admitted?;
        (!facts.ready
            && old != current.roster().anchor()
            && current.roster().contains(facts.boot.device_id)
            && old.group.0 == group)
            .then_some(old)
    }

    /// A pairing or recovery Message on an authenticated C0 (see [`handles`]).
    #[allow(clippy::too_many_arguments)]
    pub fn receive(
        &mut self,
        link: LinkId,
        facts: &LinkFacts,
        greeting: &admission::Peer,
        direct: bool,
        message: Message,
        current: &State,
        gate: Gate,
        now: Instant,
    ) -> v2::Result<Vec<Out>> {
        let peer = facts.boot;
        if let Message::ConfigPairObjectBegin(value) = &message {
            let object = value.object()?;
            let digest = object.digest()?.0;
            let existing = self.objects.contains(link.0, digest) || self.transactions.has_object(link.0, digest);
            if !existing
                && (self.objects.staging_bytes() + self.transactions.staging_bytes() + object.bytes()? as usize
                    > STAGING_BYTES
                    || self.objects.staging_count(link.0) + self.transactions.staging_count(link.0) >= STAGING_PER_LINK)
            {
                return Err(violation(v2::ErrorCode::LimitExceeded));
            }
        }
        if let Message::ConfigPairDecisionReply(reply) = &message {
            let pair = wire::read_pair(&reply.pair_id()?)?;
            if self.transactions.contains(pair)
                && reply.outcome()?.variant()? == v2::schema::Open::Known(dto::DecisionKind::Aborted)
                && dto::DecisionAborted::from_raw(reply.outcome()?.body()?)?
                    .problem()?
                    .code()?
                    == dto::PairCode::StateChanged
            {
                // A live target fenced our permit because its state changed
                // before Commit: the running flow rebases transparently.
                let effects = self
                    .transactions
                    .receive(
                        link.0,
                        peer,
                        message,
                        &self.identity,
                        current,
                        &mut self.journal,
                        now,
                        unix_ms(),
                    )
                    .map_err(merge_error)?;
                return Ok(self.merge_effects(effects, current));
            }
        }
        let recovery = match &message {
            Message::ConfigMergeLookup(_)
            | Message::ConfigMergeTransfer(_)
            | Message::ConfigMergeReceipt(_)
            | Message::ConfigPairDecisionQuery(_)
            | Message::ConfigPairDecisionReply(_) => true,
            Message::ConfigPairEnded(value) => value.lookup()?.is_some(),
            Message::ConfigPairReceipt(value) => !self.transactions.contains(wire::read_pair(&value.pair_id()?)?),
            Message::ConfigPairObjectBegin(value) => self.expected.contains_key(&(link, value.object()?.digest()?.0)),
            Message::ConfigPairObjectChunk(value) => self.objects.contains(link.0, value.digest()?.0),
            Message::ConfigPairObjectEnd(value) => self.objects.contains(link.0, value.digest()?.0),
            _ => false,
        };
        if recovery {
            return self.recovery_message(link, facts, message, current, now);
        }
        match message {
            Message::ConfigPairRequest(request) => {
                if !direct {
                    return Err(violation(v2::ErrorCode::InvalidChannel));
                }
                let effects = self.request_entry(peer, greeting, request, current, gate, now)?;
                Ok(self.pairing_effects(effects, current))
            }
            Message::ConfigPairPrepare(_)
            | Message::ConfigPairReady(_)
            | Message::ConfigPairCommitPermit(_)
            | Message::ConfigPairCommit(_)
            | Message::ConfigPairCommitted(_)
            | Message::ConfigPairObjectBegin(_)
            | Message::ConfigPairObjectChunk(_)
            | Message::ConfigPairObjectEnd(_)
            | Message::ConfigPairReceipt(_) => {
                if !direct {
                    return Err(violation(v2::ErrorCode::InvalidChannel));
                }
                let pair = self
                    .transactions
                    .associated_pair(link.0, &message)
                    .ok_or(v2::ErrorCode::StateConflict)?;
                if self.pairs.view(pair).is_some_and(|view| view.peer != peer) {
                    return Err(violation(v2::ErrorCode::AuthenticationFailed));
                }
                match self.transactions.receive(
                    link.0,
                    peer,
                    message,
                    &self.identity,
                    current,
                    &mut self.journal,
                    now,
                    unix_ms(),
                ) {
                    Ok(effects) => {
                        let mut out = Vec::new();
                        if let Some(phase) = self.pairs.view(pair).map(|view| view.phase)
                            && let Ok(effects) = self.pairs.progress(pair, phase, now)
                        {
                            out.extend(self.pairing_effects(effects, current));
                        }
                        out.extend(self.merge_effects(effects, current));
                        Ok(out)
                    }
                    // An explicit merge failure ends the pairing, not the C0.
                    Err(error) => self.pair_problem(link, peer, pair, merge_problem(&error), current),
                }
            }
            Message::ConfigPairQueued(_)
            | Message::ConfigPairAccept(_)
            | Message::ConfigPairDeclined(_)
            | Message::ConfigPairFeedback(_)
            | Message::ConfigPairPake(_)
            | Message::ConfigPairCancel(_)
            | Message::ConfigPairEnded(_) => {
                let pair = self
                    .transactions
                    .associated_pair(link.0, &message)
                    .ok_or(v2::ErrorCode::StateConflict)?;
                if self.pairs.view(pair).is_some_and(|view| view.peer != peer) {
                    return Err(violation(v2::ErrorCode::AuthenticationFailed));
                }
                let reason = match &message {
                    Message::ConfigPairDeclined(value) => Some(value.reason()?),
                    Message::ConfigPairCancel(value) => Some(value.reason()?),
                    Message::ConfigPairEnded(value) => Some(value.problem()?),
                    _ => None,
                };
                if let Some(reason) = &reason {
                    validate_problem(reason).map_err(|_| violation(v2::ErrorCode::LimitExceeded))?;
                }
                match self.pairs.receive(peer, message, now) {
                    Ok(effects) => Ok(self.pairing_effects(effects, current)),
                    Err(problem) => self.pair_problem(link, peer, pair, problem, current),
                }
            }
            _ => Err(violation(v2::ErrorCode::InvalidChannel)),
        }
    }

    /// A request checked against this C0's greeting and our current group
    /// before it becomes a pairing (protocol §13.1).
    fn request_entry(
        &mut self,
        peer: Boot,
        greeting: &admission::Peer,
        request: dto::PairRequest,
        current: &State,
        gate: Gate,
        now: Instant,
    ) -> v2::Result<Vec<pairing::Effect>> {
        let pair = wire::read_pair(&request.pair_id()?)?;
        let known = self.pairs.knows(pair);
        if !known {
            let context = request.context()?;
            if context.negotiation_hash()?.0 != greeting.hash()? {
                return Err(violation(v2::ErrorCode::AuthenticationFailed));
            }
            if !gate.enabled {
                return Ok(self.pairs.refuse(
                    pair,
                    peer,
                    pairing::problem(dto::PairCode::MissingCapability, dto::PairStage::Request),
                ));
            }
            let source = greeting.anchor()?;
            let target = current.roster().anchor();
            if context.source_group()?.0 != source.group.0
                || context.source_root_id()?.0 != source.root
                || context.target_group()?.0 != target.group.0
                || context.target_root_id()?.0 != target.root
                || context.destination_group()?.0 != target.group.0
                || context.source_count()? != greeting.greeting.advert()?.count()?
                || context.target_count()? != current.roster().members().count() as u8
            {
                return Ok(self.pairs.refuse(
                    pair,
                    peer,
                    pairing::problem(dto::PairCode::GroupChanged, dto::PairStage::Request),
                ));
            }
            if source == target {
                return Ok(self.pairs.refuse(
                    pair,
                    peer,
                    pairing::problem(dto::PairCode::AlreadyConnected, dto::PairStage::Request),
                ));
            }
        }
        let snapshot = self.pairs.snapshot();
        let busy = gate.external_busy
            || !snapshot.requests.is_empty()
            || snapshot
                .sessions
                .iter()
                .any(|view| !matches!(view.phase, Phase::Completed | Phase::Ended));
        let admission = if gate.blocked {
            Admission::Blocked
        } else if gate.exclusive && busy && !known {
            Admission::Busy
        } else {
            Admission::Open
        };
        self.pairs
            .request(request, peer, greeting.transcript.clone(), admission, now)
            .map_err(|_| violation(v2::ErrorCode::AuthenticationFailed))
    }

    /// End a live pairing with this reason, or answer a step for a pairing
    /// this owner no longer runs with its own ConfigPairEnded.
    fn pair_problem(
        &mut self,
        link: LinkId,
        peer: Boot,
        pair: PairId,
        problem: dto::PairProblem,
        current: &State,
    ) -> v2::Result<Vec<Out>> {
        if self
            .pairs
            .view(pair)
            .is_some_and(|view| view.peer == peer && !matches!(view.phase, Phase::Ended | Phase::Completed))
        {
            let effects = self.pairs.fail(pair, problem);
            return Ok(self.pairing_effects(effects, current));
        }
        Ok(vec![Out::Send {
            link,
            message: Message::ConfigPairEnded(dto::PairEnded::from_raw(
                [(1, wire::pair(pair).write()), (2, problem.write())].into(),
            )?),
        }])
    }

    // ---- Projection ----

    /// The journal's pending projection, prepared for installation. `ready`
    /// lists the owner's admitted C0s. The signed transition is gossiped and
    /// stale pairings superseded once per projection, even if the owner must
    /// retry the installation.
    pub fn projection(&mut self, current: &State, ready: &[(LinkId, DeviceId)]) -> v2::Result<Option<Projection>> {
        let Some(wire) = self.journal.pending_projection().map_err(journal_error)? else {
            return Ok(None);
        };
        // A 0.3.x result is retained in its own shape; install its projection.
        let wire = crate::legacy_merge::project_state(&wire)?;
        let roster = wire.roster()?;
        let anchor = Anchor {
            group: oink_foundation::GroupId(wire.group()?.0),
            root: crate::roster::root_id(&roster)?,
        };
        let candidate = State::verify(wire, anchor)?;
        let digest = candidate.digest()?;
        let bundle = self.journal.pending_bundle().map_err(journal_error)?;
        let mut outs = Vec::new();
        if self.announced != Some(digest) {
            if let Some(bundle) = &bundle {
                let source = crate::legacy_merge::anchors(bundle)?.0;
                for (link, _) in ready {
                    outs.extend(recovery::proof(*link, bundle, recovery::group_context(source)?, None)?);
                }
                let intent = bundle.certificate()?.intent()?;
                let pair = wire::read_pair(&intent.pair_id()?)?;
                outs.extend(self.supersede(pair, &intent, current));
            }
            self.announced = Some(digest);
        }
        let mut retire = Vec::new();
        if candidate.roster().anchor() != current.roster().anchor() {
            for (link, device) in ready {
                retire.push(*link);
                // The transition proof was queued on every old ready C0 before
                // retiring business authority. Resume signed member admission
                // on those same C0s after projection.
                if candidate.roster().contains(*device) && !self.followups.iter().any(|f| f.link == *link) {
                    self.followups.push(Followup {
                        link: *link,
                        receipt: None,
                        follow_greeting: false,
                    });
                }
            }
        }
        Ok(Some(Projection {
            candidate,
            digest,
            bundle,
            retire,
            outs,
        }))
    }

    /// The owner installed the projection with this digest. Continue the
    /// C0s that waited for it and the merge effects behind it.
    pub fn projected(&mut self, digest: [u8; 32], current: &State, links: &dyn Links) -> v2::Result<Vec<Out>> {
        self.journal.projected(digest).map_err(journal_error)?;
        self.resume(current, links)
    }

    /// Nothing (more) to install: continue the C0s and merge effects that
    /// waited for a projection (an adoption recorded without applying it
    /// still owes its receipt and admission).
    pub fn resume(&mut self, current: &State, links: &dyn Links) -> v2::Result<Vec<Out>> {
        self.announced = None;
        self.installing = false;
        let mut out = Vec::new();
        let now = Instant::now();
        for followup in std::mem::take(&mut self.followups) {
            let Some(facts) = links.facts(followup.link) else {
                continue;
            };
            if let Some(receipt) = followup.receipt {
                out.push(Out::Send {
                    link: followup.link,
                    message: receipt,
                });
            }
            if !followup.follow_greeting || facts.greeted == current.roster().anchor() {
                out.push(Out::BeginAdmission { link: followup.link });
            } else {
                self.lookup(followup.link, &facts, current, now, &mut out)?;
            }
        }
        out.extend(self.merge_effects(Vec::new(), current));
        Ok(out)
    }

    /// Signed removal of `device` (the local device: leave the group).
    pub fn removal(
        &self,
        current: &State,
        device: DeviceId,
        expected: Option<[u8; 32]>,
        now_ms: u64,
    ) -> v2::Result<State> {
        let roster = current.roster().remove(&self.identity, device, expected, now_ms)?;
        current.with_roster(&roster)
    }
}

pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
