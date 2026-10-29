//! Application-focus authority, independent of native windows, transport and clocks.
//!
//! Pointer claims are supplied by the existing pointer owner. This model never
//! elects P, blocks its keyboard delivery, migrates held keys or restores an app.
use std::collections::{BTreeMap, HashMap, HashSet};

use oink_foundation::{
    DeviceId,
    application_focus::{Foreground, Observation, ObservationStamp, ParkResult},
    session::Boot,
};
use serde::{Deserialize, Serialize};

use crate::control::FocusScope;

pub mod keys;

pub const PARK_TIMEOUT_US: u64 = 250_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    pub epoch: u64,
    pub pointer: Boot,
    pub scope: FocusScope,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub boot: Boot,
    pub routing_instance: [u8; 16],
    pub capable: bool,
    /// Only shared mouse + keyboard endpoints may be sampled or parked.
    pub parkable: bool,
    pub controllable: bool,
}
impl Member {
    pub fn participates(&self) -> bool {
        self.capable && self.controllable
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Off,
    Preparing,
    Active,
    Suspended,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    #[default]
    Disabled,
    Ready,
    NoPointer,
    PointerNotParticipating,
    AwaitingApplication,
    DesktopUnavailable,
    Drag,
    Game,
    CounterExhausted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub claim: Claim,
    pub revision: u64,
    pub keyboard: Option<Boot>,
    pub phase: Phase,
    pub reason: Reason,
    pub participants: Vec<Boot>,
}

/// A forwarding generation; retiring it releases only application-forward refs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub claim: Claim,
    pub revision: u64,
    pub destination: Boot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParkRequest {
    pub claim: Claim,
    pub revision: u64,
    pub operation: u64,
    pub target: Boot,
    pub expected: ObservationStamp,
}

/// Numeric/typed evidence copied off the realtime owner; never key contents or
/// native app/window identities. The ordinary diagnostic consumer formats it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Diagnostic {
    State {
        epoch: u64,
        revision: u64,
        pointer: DeviceId,
        keyboard: Option<DeviceId>,
        phase: Phase,
        reason: Reason,
    },
    Observation {
        epoch: u64,
        observer: DeviceId,
        observation: Observation,
        accepted: bool,
    },
    Park {
        epoch: u64,
        revision: u64,
        operation: u64,
        target: DeviceId,
        expected: ObservationStamp,
        stage: ParkStage,
        result: Option<ParkResult>,
    },
    Dismiss {
        epoch: u64,
    },
    Key {
        epoch: u64,
        revision: u64,
        destination: DeviceId,
        stage: KeyStage,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkStage {
    Requested,
    Received,
    Completed,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyStage {
    Sent,
    Received,
    Rejected(KeyRefusal),
    Injected,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyRefusal {
    UnknownSource,
    NoState,
    Sender,
    HopLimit,
    Generation,
    NotActive,
    KeyboardOwner,
    NotParticipant,
    Retired,
    LocalUnavailable,
    Scope,
}

/// Messages on the negotiated, ordered input lane. The transport authenticates
/// the sender Boot; it must not be inferred from a field in this record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub claim: Claim,
    pub action: Action,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Action {
    Observe(Observation),
    Prepare(ParkRequest),
    Commit(Snapshot),
    Applied { request: ParkRequest, result: ParkResult },
    Snapshot(Snapshot),
    Retire(Route),
    Sample,
    Cancel(ParkRequest),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Must precede publishing a replacement route on the same ordered downstream.
    Retire(Route),
    /// Release only incoming application-forward refs for an obsolete claim.
    RetireIncoming(Claim),
    Publish(Snapshot),
    Sample(Boot),
    Park(ParkRequest),
    CancelPark(ParkRequest),
    /// Close our hidden window. Never activate a previously saved application.
    Dismiss,
    ParkFinished {
        request: ParkRequest,
        result: ParkResult,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Context {
    /// One default-off setting for the whole group.
    pub enabled: bool,
    pub claim: Option<Claim>,
    pub members: Vec<Member>,
    /// Pause, drag, secure/session desktop or unavailable native capture/observer.
    pub suspended: Option<Reason>,
    pub game: bool,
}

#[derive(Debug)]
struct Pending {
    request: ParkRequest,
    deadline: u64,
}

#[derive(Debug)]
pub struct Model {
    #[cfg(debug_assertions)]
    test_clock: Option<oink_foundation::application_focus::TestClock>,
    local: Boot,
    context: Option<Context>,
    state: Option<Snapshot>,
    observations: HashMap<Boot, Observation>,
    pending: BTreeMap<DeviceId, Pending>,
    local_pending: Option<Pending>,
    remote_operations: HashMap<Boot, u64>,
    /// A local application activation never schedules another park attempt.
    attempted: HashSet<Boot>,
    waiting: HashSet<Boot>,
    preparation_deadline: Option<u64>,
    entry: bool,
    lost_hidden: bool,
    operation: u64,
    effects: Vec<Effect>,
}

impl Model {
    pub fn new(local: Boot) -> Self {
        Self {
            #[cfg(debug_assertions)]
            test_clock: None,
            local,
            context: None,
            state: None,
            observations: HashMap::new(),
            pending: BTreeMap::new(),
            local_pending: None,
            remote_operations: HashMap::new(),
            attempted: HashSet::new(),
            waiting: HashSet::new(),
            preparation_deadline: None,
            entry: false,
            lost_hidden: false,
            operation: 0,
            effects: Vec::new(),
        }
    }

    #[cfg(debug_assertions)]
    pub fn set_test_clock(&mut self, clock: oink_foundation::application_focus::TestClock) {
        assert!(
            self.context.is_none(),
            "install the test clock before configuring the model"
        );
        self.test_clock = Some(clock);
    }

    fn now(&self, wall_micros: u64) -> u64 {
        #[cfg(debug_assertions)]
        if let Some(clock) = &self.test_clock {
            return clock.now();
        }
        wall_micros
    }

    pub fn state(&self) -> Option<&Snapshot> {
        self.state.as_ref()
    }

    pub fn drain_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    pub fn deadline(&self) -> Option<u64> {
        // A manual clock is advanced by test events, never by the native owner's
        // wall-clock timer. Returning virtual timestamps here would busy-spin.
        #[cfg(debug_assertions)]
        if self.test_clock.is_some() {
            return None;
        }
        self.preparation_deadline
            .into_iter()
            .chain(self.pending.values().map(|p| p.deadline))
            .chain(self.local_pending.as_ref().map(|p| p.deadline))
            .min()
    }

    fn member(&self, boot: Boot) -> Option<&Member> {
        self.context.as_ref()?.members.iter().find(|m| m.boot == boot)
    }

    fn eligible(&self, boot: Boot) -> bool {
        self.context.as_ref().is_some_and(|c| c.enabled)
            && self.member(boot).is_some_and(|m| {
                m.participates()
                    && self
                        .context
                        .as_ref()
                        .and_then(|c| c.claim.as_ref())
                        .is_some_and(|c| c.scope.members.contains(&(boot.device_id, m.routing_instance)))
            })
    }

    pub fn can_park(&self, boot: Boot) -> bool {
        self.eligible(boot) && self.member(boot).is_some_and(|m| m.parkable)
    }

    fn authority(&self) -> bool {
        self.context
            .as_ref()
            .and_then(|c| c.claim.as_ref())
            .is_some_and(|c| c.pointer == self.local)
            && self.eligible(self.local)
    }

    fn participants(&self) -> Vec<Boot> {
        self.context.as_ref().map_or_else(Vec::new, |c| {
            c.members
                .iter()
                .filter(|m| self.eligible(m.boot))
                .map(|m| m.boot)
                .collect()
        })
    }

    /// One withdrawal, then no model work while disabled. Keep operation fences
    /// across re-enabling so delayed native/peer results cannot become current.
    pub fn disable(&mut self) {
        self.retire_route();
        self.cancel_parks();
        if let Some(state) = self.state.as_mut() {
            self.effects.push(Effect::RetireIncoming(state.claim.clone()));
            // Retain the revision fence across off/on in the same claim.
            state.keyboard = None;
            state.phase = Phase::Off;
            state.reason = Reason::Disabled;
            state.participants.clear();
        }
        if let Some(context) = self.context.as_mut() {
            context.enabled = false;
            self.effects.push(Effect::Dismiss);
        }
        self.observations.clear();
        self.attempted.clear();
        self.entry = false;
        self.lost_hidden = false;
    }

    /// Additional delivery only. The caller always preserves P's normal delivery.
    pub fn forward_route(&self) -> Option<Route> {
        let state = self.state.as_ref()?;
        let destination = state.keyboard?;
        if !self.authority()
            || state.phase != Phase::Active
            || destination == self.local
            || !self.eligible(destination)
            || self.entry
        {
            return None;
        }
        Some(Route {
            claim: state.claim.clone(),
            revision: state.revision,
            destination,
        })
    }

    fn retire_route(&mut self) {
        if let Some(route) = self.forward_route() {
            self.effects.push(Effect::Retire(route));
        }
    }

    fn cancel_parks(&mut self) {
        if let Some(pending) = self.local_pending.take() {
            self.effects.push(Effect::CancelPark(pending.request));
        }
        for (_, pending) in std::mem::take(&mut self.pending) {
            self.effects.push(Effect::CancelPark(pending.request));
        }
        self.waiting.clear();
        self.preparation_deadline = None;
    }

    /// Called after the existing router has accepted its current pointer claim.
    /// Canonical input prevents an unrelated fact reorder from restarting a session.
    pub fn configure(&mut self, mut context: Context, now: u64) {
        let now = self.now(now);
        context.members.sort_by_key(|m| m.boot.device_id);
        if self.context.as_ref() == Some(&context) {
            return;
        }
        let was_parkable = self.can_park(self.local);
        let old = self.state.clone();
        let claim_changed = self.context.as_ref().and_then(|c| c.claim.as_ref()) != context.claim.as_ref();
        self.retire_route();
        self.cancel_parks();
        if let Some(state) = &old {
            self.effects.push(Effect::RetireIncoming(state.claim.clone()));
        }
        self.observations
            .retain(|boot, _| context.members.iter().any(|m| m.boot == *boot && m.parkable));
        self.remote_operations
            .retain(|boot, _| context.members.iter().any(|m| m.boot == *boot));
        self.attempted.retain(|boot| {
            context.enabled
                && context
                    .members
                    .iter()
                    .any(|m| m.boot == *boot && m.participates() && m.parkable)
        });
        self.context = Some(context);
        if was_parkable && self.eligible(self.local) && !self.can_park(self.local) {
            self.effects.push(Effect::Dismiss);
        }
        let context = self.context.as_ref().expect("installed context");
        let Some(claim) = context.claim.clone() else {
            self.state = None;
            self.effects.push(Effect::Dismiss);
            return;
        };
        let mut keyboard = old.as_ref().and_then(|s| s.keyboard);
        let revision = if claim_changed {
            0
        } else {
            old.as_ref().map_or(0, |s| s.revision)
        };
        self.state = Some(Snapshot {
            claim: claim.clone(),
            revision,
            keyboard,
            phase: Phase::Preparing,
            reason: Reason::AwaitingApplication,
            participants: self.participants(),
        });
        self.entry =
            claim_changed || old.is_none() || keyboard.is_none() || keyboard.is_some_and(|b| self.member(b).is_none());
        if claim_changed {
            self.lost_hidden = false;
        }
        if !self.eligible(self.local) {
            self.effects.push(Effect::Dismiss);
            self.transition(Some(claim.pointer), Phase::Off, Reason::Disabled);
            return;
        }
        if let Some(reason) = context.suspended {
            self.transition(keyboard, Phase::Suspended, reason);
            self.effects.push(Effect::Dismiss);
            return;
        }
        if !self.eligible(claim.pointer) {
            // Legacy/unavailable P uses its original keyboard path. Other parked
            // devices stay parked. No special restoration or entry rule exists.
            self.transition(Some(claim.pointer), Phase::Suspended, Reason::PointerNotParticipating);
            return;
        }
        if !self.authority() {
            // Group configuration and input control arrive on separate streams.
            // Announce readiness locally even if the authority's first Sample
            // arrived before this member applied the group enable setting.
            if self.can_park(self.local) {
                self.effects.push(Effect::Sample(self.local));
            }
            return;
        }
        if !self.can_park(self.local) {
            // Local input must remain usable: stale Hidden evidence cannot keep K elsewhere.
            if let Some(old) = keyboard.filter(|b| *b != self.local) {
                self.attempted.remove(&old);
            }
            keyboard = Some(self.local);
            self.entry = false;
            self.lost_hidden = false;
            self.transition(keyboard, Phase::Preparing, Reason::AwaitingApplication);
        }
        // Only an already observed native Hidden foreground can preserve K
        // across entry. Pending activation/sample work is not confirmation.
        if self.entry
            && self
                .observations
                .get(&self.local)
                .is_none_or(|o| o.foreground != Foreground::Hidden)
        {
            if let Some(old) = keyboard.filter(|b| *b != self.local) {
                self.attempted.remove(&old);
            }
            keyboard = Some(self.local);
            self.transition(keyboard, Phase::Preparing, Reason::AwaitingApplication);
        }
        if self.context.as_ref().is_some_and(|c| c.game) && keyboard != Some(self.local) {
            self.transition(keyboard, Phase::Suspended, Reason::Game);
            self.effects.push(Effect::Dismiss);
            return;
        }
        self.preparation_deadline = Some(now.saturating_add(PARK_TIMEOUT_US));
        self.waiting = self
            .participants()
            .into_iter()
            .filter(|b| *b != self.local && self.can_park(*b) && !self.attempted.contains(b))
            .collect();
        for boot in self.participants() {
            if self.can_park(boot) {
                self.effects.push(Effect::Sample(boot));
            }
        }
        if !self.can_park(self.local) {
            self.finish_preparing();
        }
    }

    fn transition(&mut self, keyboard: Option<Boot>, phase: Phase, reason: Reason) {
        self.retire_route();
        let publish = self.authority();
        let participants = self.participants();
        let Some(state) = self.state.as_mut() else { return };
        let Some(revision) = (if publish {
            state.revision.checked_add(1)
        } else {
            Some(state.revision)
        }) else {
            state.phase = Phase::Suspended;
            state.reason = Reason::CounterExhausted;
            return;
        };
        state.revision = revision;
        state.keyboard = keyboard;
        state.phase = phase;
        state.reason = reason;
        state.participants = participants;
        tracing::info!(target: "oink_application_focus", epoch = state.claim.epoch,
            revision = state.revision, ?phase, ?reason,
            pointer_local = state.claim.pointer == self.local,
            keyboard_local = keyboard == Some(self.local),
            split = keyboard.is_some_and(|k| k != state.claim.pointer), "routing transition");
        if publish {
            self.effects.push(Effect::Publish(state.clone()));
        }
    }

    /// The input owner attaches its accepted claim to native samples. Peer
    /// observations must be authenticated as the observing Boot, never forwarded.
    pub fn observe(&mut self, claim: &Claim, observer: Boot, observation: Observation, now: u64) -> bool {
        let now = self.now(now);
        if self.state.as_ref().is_none_or(|s| &s.claim != claim)
            || !self.can_park(observer)
            || observation.stamp.generation == 0
            || observation.stamp.sequence == 0
            || self.observations.get(&observer).is_some_and(|old| {
                old.stamp > observation.stamp || old.stamp == observation.stamp && *old != observation
            })
        {
            return false;
        }
        let previous = self.observations.insert(observer, observation);
        let fresh = previous != Some(observation);
        // A stable native sample is still a response to preparation after a
        // context change. Forward it, but never replay an old activation edge.
        if !fresh
            && !self.entry
            && !self.waiting.contains(&observer)
            && self.state.as_ref().is_none_or(|s| s.phase != Phase::Preparing)
        {
            return true;
        }
        if observer == self.local
            && self.local_pending.as_ref().is_some_and(|p| {
                observation.park_operation != Some(p.request.operation) && observation.stamp != p.request.expected
            })
        {
            let pending = self.local_pending.take().expect("checked pending");
            self.effects.push(Effect::CancelPark(pending.request.clone()));
            self.effects.push(Effect::ParkFinished {
                request: pending.request,
                result: ParkResult::Stale,
            });
        }
        if let Some(pending) = self.pending.get(&observer.device_id)
            && observation.park_operation != Some(pending.request.operation)
            && observation.stamp != pending.request.expected
        {
            let pending = self.pending.remove(&observer.device_id).expect("checked pending");
            self.effects.push(Effect::CancelPark(pending.request));
        }
        if !self.authority() {
            return true;
        }
        if observer != self.local {
            if self.waiting.remove(&observer) {
                self.park_once(observer, now);
            }
            self.finish_preparing();
            return true;
        }
        let foreground = observation.foreground;
        // Mouse-down suspends forwarding before native activation is observed.
        // Keep the loss evidence until mouse-up resumes the same pointer claim;
        // otherwise a click can leave K on the previously focused application.
        self.lost_hidden |= fresh
            && (observation.hidden_lost
                || previous.is_some_and(|o| o.foreground == Foreground::Hidden) && foreground != Foreground::Hidden);
        if self.context.as_ref().is_some_and(|c| c.suspended.is_some()) {
            return true;
        }
        if foreground == Foreground::Unavailable {
            self.transition(Some(self.local), Phase::Suspended, Reason::DesktopUnavailable);
            self.cancel_parks();
            self.effects.push(Effect::Dismiss);
            return true;
        }
        if self
            .state
            .as_ref()
            .is_some_and(|s| s.phase == Phase::Suspended && s.reason == Reason::DesktopUnavailable)
        {
            // Native desktop recovery need not change routing/configuration. Keep
            // observing through suspension and resume without replaying presses.
            self.transition(
                self.state.as_ref().and_then(|s| s.keyboard),
                Phase::Preparing,
                Reason::AwaitingApplication,
            );
        }
        if self.entry || foreground == Foreground::Unknown || self.lost_hidden && foreground.ordinary() {
            let old_keyboard = self.state.as_ref().and_then(|s| s.keyboard);
            // Hidden entry keeps K, including an old/non-capable K. The latter
            // cannot be forwarded to: the user clicks an app here to claim K.
            let keyboard = if foreground == Foreground::Hidden {
                old_keyboard
            } else {
                Some(self.local)
            };
            self.entry = false;
            self.lost_hidden = false;
            self.transition(keyboard, Phase::Preparing, Reason::AwaitingApplication);
            if keyboard != old_keyboard
                && let Some(old) = old_keyboard.filter(|b| *b != self.local)
            {
                self.attempted.remove(&old);
                // A resume sample may already be in flight. Never fence the
                // new park against cached evidence from the previous K: its
                // native owner may have refreshed a different window meanwhile.
                // Await that sample, or request one, before the single attempt.
                if self.can_park(old) {
                    self.preparation_deadline
                        .get_or_insert(now.saturating_add(PARK_TIMEOUT_US));
                    if self.waiting.insert(old) {
                        self.effects.push(Effect::Sample(old));
                    }
                }
            }
        }
        if self.context.as_ref().is_some_and(|c| c.game)
            && self.state.as_ref().and_then(|s| s.keyboard) != Some(self.local)
        {
            self.transition(
                self.state.as_ref().and_then(|s| s.keyboard),
                Phase::Suspended,
                Reason::Game,
            );
            return true;
        }
        self.finish_preparing();
        true
    }

    fn park_once(&mut self, target: Boot, now: u64) {
        if !self.authority()
            || !self.can_park(target)
            || self.state.as_ref().and_then(|s| s.keyboard) == Some(target)
            || !self.attempted.insert(target)
        {
            return;
        }
        let Some(observation) = self.observations.get(&target).copied() else {
            return;
        };
        let Some(operation) = self.operation.checked_add(1) else {
            return;
        };
        self.operation = operation;
        let state = self.state.as_ref().expect("authority state");
        let request = ParkRequest {
            claim: state.claim.clone(),
            revision: state.revision,
            operation,
            target,
            expected: observation.stamp,
        };
        let result = match observation.foreground {
            Foreground::Hidden => Some(ParkResult::Confirmed),
            Foreground::Fullscreen => Some(ParkResult::FullscreenExempt),
            Foreground::Unknown | Foreground::OrdinaryUnknown => Some(ParkResult::UnknownExempt),
            Foreground::Unavailable => Some(ParkResult::Unavailable),
            Foreground::Windowed => None,
        };
        tracing::info!(target: "oink_application_focus", epoch = request.claim.epoch,
            revision = request.revision, operation = request.operation,
            generation = request.expected.generation, sequence = request.expected.sequence,
            ?result, "park decision");
        if let Some(result) = result {
            self.effects.push(Effect::ParkFinished { request, result });
        } else {
            self.pending.insert(
                target.device_id,
                Pending {
                    request: request.clone(),
                    deadline: now.saturating_add(PARK_TIMEOUT_US),
                },
            );
            self.effects.push(Effect::Park(request));
        }
    }

    pub fn park_completed(&mut self, request: &ParkRequest, result: ParkResult) -> bool {
        if !self.authority() && self.local_pending.as_ref().is_some_and(|p| p.request == *request) {
            self.local_pending = None;
            self.effects.push(Effect::ParkFinished {
                request: request.clone(),
                result,
            });
            return true;
        }
        if !self.authority()
            || self.state.as_ref().is_none_or(|s| s.claim != request.claim)
            || self
                .pending
                .get(&request.target.device_id)
                .is_none_or(|p| p.request != *request)
        {
            return false;
        }
        self.pending.remove(&request.target.device_id);
        self.effects.push(Effect::ParkFinished {
            request: request.clone(),
            result,
        });
        self.finish_preparing();
        true
    }

    fn finish_preparing(&mut self) {
        if self.entry || !self.waiting.is_empty() || !self.pending.is_empty() {
            return;
        }
        let Some(state) = &self.state else { return };
        if state.phase != Phase::Preparing {
            return;
        }
        if self.can_park(self.local)
            && self
                .observations
                .get(&self.local)
                .is_none_or(|o| o.foreground == Foreground::Unavailable)
        {
            return;
        }
        let keyboard = state.keyboard;
        self.preparation_deadline = None;
        let reason = if keyboard.is_some_and(|boot| self.eligible(boot)) {
            Reason::Ready
        } else {
            Reason::AwaitingApplication
        };
        self.transition(keyboard, Phase::Active, reason);
    }

    pub fn tick(&mut self, now: u64) {
        let now = self.now(now);
        if self.local_pending.as_ref().is_some_and(|p| now >= p.deadline) {
            let pending = self.local_pending.take().expect("expired pending");
            self.effects.push(Effect::CancelPark(pending.request.clone()));
            self.effects.push(Effect::ParkFinished {
                request: pending.request,
                result: ParkResult::Denied,
            });
        }
        if self.preparation_deadline.is_some_and(|d| now >= d) {
            self.attempted.extend(self.waiting.drain());
            self.preparation_deadline = None;
            if self.entry && self.authority() {
                // No entry observation arrived. A cached Hidden state cannot
                // preserve the old K indefinitely when the fresh sample stalls.
                self.entry = false;
                self.lost_hidden = false;
                self.transition(Some(self.local), Phase::Active, Reason::Ready);
            }
        }
        let expired: Vec<_> = self
            .pending
            .values()
            .filter(|p| now >= p.deadline)
            .map(|p| p.request.clone())
            .collect();
        for request in expired {
            self.effects.push(Effect::CancelPark(request.clone()));
            self.park_completed(&request, ParkResult::Denied);
        }
        self.finish_preparing();
    }

    /// A remote authority can request one conditional native operation. A local
    /// activation observed after sampling cancels it before backend execution;
    /// the backend must also compare the stamp atomically on its owner thread.
    pub fn accept_prepare(&mut self, author: Boot, request: ParkRequest, now: u64) -> bool {
        let now = self.now(now);
        if author == self.local
            || author != request.claim.pointer
            || request.target != self.local
            || request.operation == 0
            || !self.eligible(author)
            || !self.can_park(self.local)
            || self.context.as_ref().is_none_or(|c| c.suspended.is_some() || c.game)
            || self
                .state
                .as_ref()
                .is_none_or(|s| s.claim != request.claim || request.revision < s.revision)
            || self
                .remote_operations
                .get(&author)
                .is_some_and(|old| *old >= request.operation)
        {
            return false;
        }
        self.remote_operations.insert(author, request.operation);
        if let Some(previous) = self.local_pending.take() {
            self.effects.push(Effect::CancelPark(previous.request));
        }
        let result = match self.observations.get(&self.local) {
            Some(o) if o.stamp == request.expected => match o.foreground {
                Foreground::Windowed => None,
                Foreground::Hidden => Some(ParkResult::Confirmed),
                Foreground::Fullscreen => Some(ParkResult::FullscreenExempt),
                Foreground::OrdinaryUnknown | Foreground::Unknown => Some(ParkResult::UnknownExempt),
                Foreground::Unavailable => Some(ParkResult::Unavailable),
            },
            _ => Some(ParkResult::Stale),
        };
        tracing::info!(target: "oink_application_focus", epoch = request.claim.epoch,
            revision = request.revision, operation = request.operation,
            generation = request.expected.generation, sequence = request.expected.sequence,
            ?result, "received park decision");
        if let Some(result) = result {
            self.effects.push(Effect::ParkFinished { request, result });
        } else {
            self.local_pending = Some(Pending {
                request: request.clone(),
                deadline: now.saturating_add(PARK_TIMEOUT_US),
            });
            self.effects.push(Effect::Park(request));
        }
        true
    }

    pub fn accept_cancel(&mut self, author: Boot, request: &ParkRequest) -> bool {
        if author != request.claim.pointer || self.local_pending.as_ref().is_none_or(|p| p.request != *request) {
            return false;
        }
        let pending = self.local_pending.take().expect("matched pending");
        self.effects.push(Effect::CancelPark(pending.request));
        true
    }

    /// Snapshot repair cannot change the pointer claim, revive an old Boot, or
    /// execute parking again. Only its actual capable pointer authority may send it.
    pub fn accept_snapshot(&mut self, author: Boot, snapshot: Snapshot) -> bool {
        if author == self.local
            || !self.eligible(author)
            || author != snapshot.claim.pointer
            || self
                .state
                .as_ref()
                .is_none_or(|s| s.claim != snapshot.claim || snapshot.revision <= s.revision)
            || snapshot.participants != self.participants()
            || snapshot.revision == 0
            || snapshot.keyboard.is_some_and(|b| self.member(b).is_none())
        {
            return false;
        }
        if !self.eligible(self.local)
            || self.context.as_ref().is_some_and(|c| c.suspended.is_some())
            || self
                .context
                .as_ref()
                .is_some_and(|c| c.game && snapshot.keyboard != Some(snapshot.claim.pointer))
        {
            return false;
        }
        if let Some(previous) = &self.state {
            // Snapshot repair also fences references from an older generation,
            // even if the explicit retirement was lost with its input stream.
            self.effects.push(Effect::RetireIncoming(previous.claim.clone()));
        }
        self.state = Some(snapshot);
        true
    }
}

#[cfg(test)]
mod tests;
