//! Bridge between pointer authority, application-focus reduction and native work.
use super::*;
use crate::application_focus::{self as a, keys};
use oink_foundation::{
    application_focus::{Observation, ParkResult},
    session::Boot,
};

#[derive(Clone, Debug, PartialEq)]
pub enum NativeCommand {
    Sample(a::Claim),
    Park(a::ParkRequest),
    Cancel(a::ParkRequest),
    Dismiss(Option<a::Claim>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Participation {
    capable: bool,
    modes: (InputMode, InputMode),
}

impl Default for Participation {
    fn default() -> Self {
        Self {
            capable: false,
            modes: (InputMode::Disabled, InputMode::Disabled),
        }
    }
}

pub(super) struct State {
    pub(super) enabled: bool,
    pub model: a::Model,
    facts: HashMap<Boot, Participation>,
    pub outgoing: keys::Outgoing,
    pub incoming: keys::Incoming,
    retired: Option<a::Route>,
    diagnostic_state: Option<a::Diagnostic>,
}
impl State {
    pub fn new(local: Boot) -> Self {
        Self {
            enabled: false,
            model: a::Model::new(local),
            facts: HashMap::new(),
            outgoing: keys::Outgoing::default(),
            incoming: keys::Incoming::default(),
            retired: None,
            diagnostic_state: None,
        }
    }
}

impl Router {
    #[cfg(debug_assertions)]
    pub fn set_application_focus_test_clock(&mut self, clock: oink_foundation::application_focus::TestClock) {
        self.application.model.set_test_clock(clock);
    }

    fn application_diagnostic(&self, record: a::Diagnostic) {
        if !self.application.enabled {
            return;
        }
        if let Some(sink) = self.diagnostics.as_ref().filter(|sink| sink.enabled()) {
            sink.application_focus(record);
        }
    }
    fn application_park_diagnostic(&self, request: &a::ParkRequest, stage: a::ParkStage, result: Option<ParkResult>) {
        self.application_diagnostic(a::Diagnostic::Park {
            epoch: request.claim.epoch,
            revision: request.revision,
            operation: request.operation,
            target: request.target.device_id,
            expected: request.expected,
            stage,
            result,
        });
    }
    pub(super) fn application_key(
        &mut self,
        origin: DeviceId,
        hops: u8,
        code: KeyCode,
        physical: Option<KeyCode>,
        pressed: bool,
    ) {
        if !self.application.enabled {
            return;
        }
        // Volume keeps the existing audio-exit transaction; K never chooses a
        // second audio exit. This is the pre-existing hardware-key policy.
        if physical.is_none() && code.is_volume() {
            return;
        }
        let Some(source) = self.admitted_boots.get(&origin).copied() else {
            return;
        };
        let identity = physical.unwrap_or(code);
        let token = if pressed {
            let Some(route) = self.application.model.forward_route() else {
                return;
            };
            if self.holder != self.me() || hops >= self.cfg.settings.max_forward_hops {
                return;
            }
            self.application.outgoing.press(source, identity, &route)
        } else {
            self.application.outgoing.release(source, identity)
        };
        let Some(route) = token else { return };
        self.application_diagnostic(a::Diagnostic::Key {
            epoch: route.epoch,
            revision: route.revision,
            destination: route.destination.device_id,
            stage: a::KeyStage::Sent,
        });
        if pressed {
            self.application
                .outgoing
                .track_sync(source, &route, hops.saturating_add(1));
        }
        self.send(
            route.destination.device_id,
            Message::Input(InputMsg::Forwarded {
                origin: source,
                hops: hops.saturating_add(1),
                msg: Box::new(InputMsg::ApplicationKey {
                    route,
                    code,
                    physical_code: physical,
                    pressed,
                }),
            }),
        );
    }

    pub(super) fn application_repeat(&mut self, origin: DeviceId, hops: u8, code: KeyCode, n: u32) {
        if !self.application.enabled {
            return;
        }
        if code.is_volume() {
            return;
        }
        let Some(source) = self.admitted_boots.get(&origin).copied() else {
            return;
        };
        let Some(route) = self.application.outgoing.repeat(source, code) else {
            return;
        };
        if hops >= self.cfg.settings.max_forward_hops {
            return;
        }
        self.send(
            route.destination.device_id,
            Message::Input(InputMsg::Forwarded {
                origin: source,
                hops: hops.saturating_add(1),
                msg: Box::new(InputMsg::ApplicationRepeat { route, code, n }),
            }),
        );
    }

    pub(super) fn application_input(&mut self, from: DeviceId, origin: DeviceId, hops: u8, input: InputMsg) {
        if !self.application.enabled {
            return;
        }
        let sync_keys = if let InputMsg::ApplicationHeldSync { keys, .. } = &input {
            Some(keys.clone())
        } else {
            None
        };
        let (token, code, physical, edge, repeat) = match input {
            InputMsg::ApplicationKey {
                route,
                code,
                physical_code,
                pressed,
            } => (route, code, physical_code, Some(pressed), None),
            InputMsg::ApplicationRepeat { route, code, n } => (route, code, None, None, Some(n)),
            InputMsg::ApplicationHeldSync { route, .. } => (route, KeyCode(0), None, None, None),
            _ => return,
        };
        let (epoch, revision, destination) = (token.epoch, token.revision, token.destination.device_id);
        let evidence = |stage| a::Diagnostic::Key {
            epoch,
            revision,
            destination,
            stage,
        };
        self.application_diagnostic(evidence(a::KeyStage::Received));
        let Some(source) = self.admitted_boots.get(&origin).copied() else {
            self.application_diagnostic(evidence(a::KeyStage::Rejected(a::KeyRefusal::UnknownSource)));
            return;
        };
        // A matching old press still authorizes cleanup after revision/claim
        // retirement. It never authorizes a DOWN, and cannot release a newer
        // press of this physical key (the incoming ledger compares full tokens).
        if edge == Some(false) {
            if self.admitted_boots.get(&from) == Some(&token.pointer)
                && hops > 0
                && hops <= self.cfg.settings.max_forward_hops
                && token.destination == self.cfg.boot
                && let Some(mapped) = self
                    .application
                    .incoming
                    .release(source, physical.unwrap_or(code), &token)
            {
                self.release_key_ref(mapped);
            }
            return;
        }
        let Some(state) = self.application.model.state() else {
            self.application_diagnostic(evidence(a::KeyStage::Rejected(a::KeyRefusal::NoState)));
            return;
        };
        let route = a::Route {
            claim: state.claim.clone(),
            revision: state.revision,
            destination: self.cfg.boot,
        };
        use a::KeyRefusal as R;
        let refusal = if self.admitted_boots.get(&from) != Some(&token.pointer) {
            Some(R::Sender)
        } else if hops == 0 || hops > self.cfg.settings.max_forward_hops {
            Some(R::HopLimit)
        } else if !token.matches(&route) {
            Some(R::Generation)
        } else if state.phase != a::Phase::Active {
            Some(R::NotActive)
        } else if state.keyboard != Some(self.cfg.boot) || state.claim.pointer == self.cfg.boot {
            Some(R::KeyboardOwner)
        } else if !state.participants.contains(&self.cfg.boot) {
            Some(R::NotParticipant)
        } else if self.application.retired.as_ref() == Some(&route) {
            Some(R::Retired)
        } else if self.paused || !self.capture_available {
            Some(R::LocalUnavailable)
        } else if !self.scope_allows(Some(&state.claim.scope), origin) {
            Some(R::Scope)
        } else {
            None
        };
        if let Some(reason) = refusal {
            self.application_diagnostic(evidence(a::KeyStage::Rejected(reason)));
            return;
        }
        if let Some(keys) = sync_keys {
            for code in self.application.incoming.sync(source, &token, &keys) {
                self.release_key_ref(code);
            }
            return;
        }
        let identity = physical.unwrap_or(code);
        match edge {
            Some(true) => {
                let mapped = if physical.is_some() {
                    code
                } else {
                    self.map_key(origin, code)
                };
                if let Some(mapped) = self.application.incoming.press(source, identity, mapped, token)
                    && mapped != KeyCode::NONE
                {
                    let count = self.key_refs.entry(mapped).or_insert(0);
                    *count += 1;
                    if *count == 1 {
                        self.application_diagnostic(evidence(a::KeyStage::Injected));
                        self.out.push(Output::Inject(InjectEvent::Key {
                            code: mapped,
                            pressed: true,
                        }));
                    }
                }
            }
            Some(false) => {
                if let Some(mapped) = self.application.incoming.release(source, identity, &token) {
                    self.release_key_ref(mapped);
                }
            }
            None => {
                if let Some(mapped) = self
                    .application
                    .incoming
                    .repeat(source, identity, &token, repeat.unwrap_or(0))
                    && repeatable(mapped)
                {
                    self.out.push(Output::Inject(InjectEvent::Key {
                        code: mapped,
                        pressed: true,
                    }));
                }
            }
        }
    }

    pub(super) fn application_state_sync(&mut self, gossip: bool) {
        if !self.application.enabled {
            return;
        }
        if gossip
            && let Some(state) = self.application.model.state().cloned()
            && state.claim.pointer == self.cfg.boot
            && state.phase != a::Phase::Off
        {
            for target in state.participants.iter().copied() {
                self.application_send(target, state.claim.clone(), a::Action::Snapshot(state.clone()));
            }
        }
        for keys::HeldSnapshot {
            source,
            route,
            hops,
            keys,
        } in self.application.outgoing.snapshots()
        {
            self.send(
                route.destination.device_id,
                Message::Input(InputMsg::Forwarded {
                    origin: source,
                    hops,
                    msg: Box::new(InputMsg::ApplicationHeldSync { route, keys }),
                }),
            );
        }
    }

    /// Owner facts are projected independently per device; one legacy endpoint
    /// never disables the capable subset. Boot fencing rejects delayed facts.
    /// Modes are (mouse, keyboard); Local participates but cannot be parked.
    pub fn set_application_focus_participation(
        &mut self,
        boot: Boot,
        capable: bool,
        modes: (InputMode, InputMode),
        now: Micros,
    ) {
        if self.admitted_boots.get(&boot.device_id) != Some(&boot) {
            return;
        }
        // Local Disabled is a physical-source keyboard policy, not a refusal of
        // incoming application keys. Match its frozen v1 Shared+disabled-bool
        // projection. Peers' missing/unknown keyboard mode still fails closed.
        let modes = if boot == self.cfg.boot && modes.1 == InputMode::Disabled {
            (modes.0, InputMode::Shared)
        } else {
            modes
        };
        self.application.facts.insert(boot, Participation { capable, modes });
        self.application_sync(now);
    }

    pub fn application_focus(&self) -> Option<&a::Snapshot> {
        self.application
            .enabled
            .then(|| self.application.model.state())
            .flatten()
    }

    /// Applied from the group register, never from a device's owner facts.
    pub fn set_application_focus_enabled(&mut self, enabled: bool, now: Micros) {
        if self.application.enabled == enabled {
            return;
        }
        self.application.enabled = enabled;
        if enabled {
            self.application_sync(now);
        } else {
            self.application.model.disable();
            self.application_effects();
            self.application.diagnostic_state = None;
        }
    }

    pub fn set_application_focus_capability(&mut self, boot: Boot, capable: bool, now: Micros) {
        if self.admitted_boots.get(&boot.device_id) != Some(&boot) {
            return;
        }
        self.application.facts.entry(boot).or_default().capable = capable;
        self.application_sync(now);
    }

    pub(super) fn application_sync(&mut self, now: Micros) {
        if !self.application.enabled {
            return;
        }
        let claim = self.focus_scope.as_ref().and_then(|scope| {
            (self.epoch != 0 && self.scope_allows(Some(scope), self.claim))
                .then(|| {
                    self.admitted_boots.get(&self.claim).map(|pointer| a::Claim {
                        epoch: self.epoch,
                        pointer: *pointer,
                        scope: scope.clone(),
                    })
                })
                .flatten()
        });
        let mut members = Vec::new();
        self.application
            .facts
            .retain(|boot, _| self.admitted_boots.get(&boot.device_id) == Some(boot));
        for (device, boot) in &self.admitted_boots {
            let facts = self.application.facts.get(boot).copied().unwrap_or_default();
            let local = *device == self.me();
            let peer = self.peers.get(device);
            let Some(instance) = (if local {
                Some(self.session)
            } else {
                peer.filter(|p| p.connected).and_then(|p| p.session)
            }) else {
                continue;
            };
            members.push(a::Member {
                boot: *boot,
                routing_instance: instance,
                capable: facts.capable,
                // Keyboard Disabled only blocks its physical source. Incoming
                // application keys and parking keep the Shared policy, just as
                // before the separate keyboard guard became a source mode.
                parkable: facts.modes == (InputMode::Shared, InputMode::Shared)
                    && (!local
                        || self.cfg.settings.mouse_mode == InputMode::Shared
                            && self.cfg.settings.keyboard_mode != InputMode::Local),
                controllable: facts.modes.0 != InputMode::Disabled
                    && facts.modes.1 != InputMode::Disabled
                    && if local {
                        self.capture_available && !self.paused && self.cfg.settings.mouse_mode != InputMode::Disabled
                    } else {
                        peer.is_some_and(|p| p.sharing)
                    },
            });
        }
        let suspended = if self.paused || self.fully_paused || !self.capture_available || self.screen_local {
            Some(a::Reason::DesktopUnavailable)
        } else if self.dnd_state_name() != "idle" || !self.button_refs.is_empty() || !self.buttons.is_empty() {
            Some(a::Reason::Drag)
        } else {
            None
        };
        self.application.model.configure(
            a::Context {
                enabled: self.application.enabled,
                claim,
                members,
                suspended,
                game: self.game_mode().active,
            },
            now,
        );
        self.application_effects();
    }

    fn application_send(&mut self, target: Boot, claim: a::Claim, action: a::Action) {
        if matches!(action, a::Action::Sample)
            && self.out.iter().any(|output| {
                matches!(output,
            Output::Send { to, msg: Message::Control(ControlMsg::ApplicationFocus(value)), .. }
                if *to == target.device_id && value.claim == claim && matches!(value.action, a::Action::Sample))
            })
        {
            return;
        }
        if target != self.cfg.boot
            && self.admitted_boots.get(&target.device_id) == Some(&target)
            && self.application.facts.get(&target).is_some_and(|p| p.capable)
            && self.peers.get(&target.device_id).is_some_and(|p| p.connected)
        {
            self.send(
                target.device_id,
                Message::Control(ControlMsg::ApplicationFocus(a::Message { claim, action })),
            );
        }
    }

    pub(super) fn application_effects(&mut self) {
        for effect in self.application.model.drain_effects() {
            match effect {
                a::Effect::Retire(route) => {
                    self.application.outgoing.retire(&route);
                    self.application_send(route.destination, route.claim.clone(), a::Action::Retire(route));
                }
                a::Effect::RetireIncoming(_) => {
                    for code in self.application.incoming.clear() {
                        self.release_key_ref(code);
                    }
                }
                a::Effect::Publish(snapshot) => {
                    for target in snapshot.participants.iter().copied() {
                        self.application_send(target, snapshot.claim.clone(), a::Action::Commit(snapshot.clone()));
                    }
                }
                a::Effect::Sample(target) => {
                    if let Some(claim) = self.application.model.state().map(|s| s.claim.clone()) {
                        if target == self.cfg.boot {
                            self.application_sample(claim);
                        } else {
                            self.application_send(target, claim, a::Action::Sample);
                        }
                    }
                }
                a::Effect::Park(request) => {
                    self.application_park_diagnostic(&request, a::ParkStage::Requested, None);
                    if request.target == self.cfg.boot {
                        self.out.push(Output::ApplicationFocus(NativeCommand::Park(request)));
                    } else {
                        self.application_send(request.target, request.claim.clone(), a::Action::Prepare(request));
                    }
                }
                a::Effect::CancelPark(request) => {
                    self.application_park_diagnostic(&request, a::ParkStage::Cancelled, None);
                    if request.target == self.cfg.boot {
                        self.out.push(Output::ApplicationFocus(NativeCommand::Cancel(request)));
                    } else {
                        self.application_send(request.target, request.claim.clone(), a::Action::Cancel(request));
                    }
                }
                a::Effect::Dismiss => {
                    self.application_diagnostic(a::Diagnostic::Dismiss {
                        epoch: self.application.model.state().map_or(0, |s| s.claim.epoch),
                    });
                    let observe = self
                        .application
                        .model
                        .state()
                        .filter(|s| s.phase != a::Phase::Off && self.application.model.can_park(self.cfg.boot))
                        .map(|s| s.claim.clone());
                    self.out.push(Output::ApplicationFocus(NativeCommand::Dismiss(observe)));
                }
                a::Effect::ParkFinished { request, result } => {
                    self.application_park_diagnostic(&request, a::ParkStage::Completed, Some(result));
                    if request.target == self.cfg.boot {
                        self.application_send(
                            request.claim.pointer,
                            request.claim.clone(),
                            a::Action::Applied { request, result },
                        );
                    }
                }
            }
        }
        if !self.diagnostics.as_ref().is_some_and(|sink| sink.enabled()) {
            self.application.diagnostic_state = None;
            return;
        }
        if let Some(state) = self.application.model.state() {
            let record = a::Diagnostic::State {
                epoch: state.claim.epoch,
                revision: state.revision,
                pointer: state.claim.pointer.device_id,
                keyboard: state.keyboard.map(|k| k.device_id),
                phase: state.phase,
                reason: state.reason,
            };
            if self.application.diagnostic_state != Some(record) {
                self.application.diagnostic_state = Some(record);
                self.application_diagnostic(record);
            }
        }
    }

    pub fn application_focus_observed(&mut self, claim: &a::Claim, observation: Observation, now: Micros) {
        if !self.application.enabled {
            return;
        }
        self.application_sync(now);
        let accepted = self.application.model.observe(claim, self.cfg.boot, observation, now);
        self.application_diagnostic(a::Diagnostic::Observation {
            epoch: claim.epoch,
            observer: self.me(),
            observation,
            accepted,
        });
        if accepted {
            self.application_send(claim.pointer, claim.clone(), a::Action::Observe(observation));
            self.application_effects();
        }
    }

    fn application_sample(&mut self, claim: a::Claim) {
        if !self.application.model.can_park(self.cfg.boot) {
            return;
        }
        if !self
            .out
            .iter()
            .any(|output| matches!(output, Output::ApplicationFocus(NativeCommand::Sample(queued)) if *queued == claim))
        {
            self.out.push(Output::ApplicationFocus(NativeCommand::Sample(claim)));
        }
    }

    pub fn application_focus_park_completed(&mut self, request: &a::ParkRequest, result: ParkResult, now: Micros) {
        if !self.application.enabled {
            return;
        }
        self.application_sync(now);
        self.application.model.park_completed(request, result);
        self.application_effects();
    }

    pub(super) fn application_message(&mut self, from: DeviceId, message: a::Message, now: Micros) {
        if !self.application.enabled {
            return;
        }
        let Some(author) = self.admitted_boots.get(&from).copied() else {
            return;
        };
        if !self.application.facts.get(&author).is_some_and(|p| p.capable)
            || self.application.model.state().is_none_or(|s| s.claim != message.claim)
        {
            return;
        }
        match message.action {
            a::Action::Observe(observation) if message.claim.pointer == self.cfg.boot => {
                let accepted = self.application.model.observe(&message.claim, author, observation, now);
                self.application_diagnostic(a::Diagnostic::Observation {
                    epoch: message.claim.epoch,
                    observer: author.device_id,
                    observation,
                    accepted,
                });
            }
            a::Action::Prepare(request) => {
                self.application_park_diagnostic(&request, a::ParkStage::Received, None);
                self.application.model.accept_prepare(author, request, now);
            }
            a::Action::Cancel(request) => {
                self.application.model.accept_cancel(author, &request);
            }
            a::Action::Applied { request, result } if author == request.target => {
                self.application.model.park_completed(&request, result);
            }
            a::Action::Commit(snapshot) | a::Action::Snapshot(snapshot) => {
                self.application.model.accept_snapshot(author, snapshot);
            }
            a::Action::Retire(route) if author == route.claim.pointer && route.destination == self.cfg.boot => {
                for code in self.application.incoming.retire(&route) {
                    self.release_key_ref(code);
                }
                if self
                    .application
                    .model
                    .state()
                    .is_some_and(|s| s.claim == route.claim && s.revision == route.revision)
                {
                    self.application.retired = Some(route);
                }
            }
            a::Action::Sample if author == message.claim.pointer => {
                self.application_sample(message.claim);
            }
            _ => {}
        }
        self.application_effects();
    }
}
