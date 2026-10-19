//! Input module: ordinary-pool stream management for I0 and I1.
//!
//! I0 carries input, focus (with hand-off notices) and drag-and-drop control, which need
//! one order. Two rules beyond the shared machine:
//!
//! 1. **The pair.** Each end manages only the I0 it writes. It reopens it for exactly
//!    four reasons: its own post reported full (or busy); the reader closed or rejected
//!    it; this device's "can be controlled" changed; this device resumed from pause.
//!    Never because the peer's stream closed or was replaced. The pair is *attached* when
//!    the outbound stream is open, the inbound one exists and its opening state has been
//!    read, and the release of keys from the previous detach has completed. It is
//!    *detached* as soon as either stream is gone or the inbound one is replaced.
//! 2. **Open at once.** Both directions open when the link is ready and this device is
//!    not paused, so that each side has the other's focus state before any hand-off.
//!
//! The task here runs on the ordinary thread pool: it owns the machines, allocates the
//! handles and passes them to the input thread with fixed-size commands
//! (`threads::input::InputCommand`). Attach, detach and everything the router sees
//! happen on the input thread, in the adapter ([`RouterAdapter`]).
use oink_foundation::{DeviceId, input::FeelProfile};
use oink_protocol::frame::StreamKind;
use oink_routing::{Packet as Message, control::ControlMsg, dnd::DndMsg};

pub(crate) mod wire;

pub const KINDS: &[StreamKind] = &[StreamKind::Input];

/// The opening state of I0, in this order; reading the last one ends the opening.
/// 1 `ControlMsg::InputCapability`, 2 `ControlMsg::Feel`, 3 `ControlMsg::FocusState`,
/// 4 `ControlMsg::GameMode`, 5 `DndMsg::Caps`.
pub const OPENING_MESSAGES: usize = 5;

/// What the inbound I0 has told about its writer. Lives and dies with that stream: cleared
/// when it closes or is replaced, kept while only the outbound stream is reopened. Between
/// attaches (and while this device is paused) state messages update it without reaching
/// the router; at every attach the router gets the newest values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReceiveState {
    /// From `InputCapability`. The router's `sharing` flag for the peer.
    pub controllable: Option<bool>,
    pub touchpad_contacts: Option<u8>,
    pub feel: Option<FeelProfile>,
    /// Newest `ControlMsg::FocusState` (carries the peer's router instance).
    pub focus: Option<ControlMsg>,
    /// Newest `ControlMsg::GameMode`.
    pub game_mode: Option<ControlMsg>,
    /// Newest `DndMsg::Caps`.
    pub dnd_caps: Option<DndMsg>,
}

impl ReceiveState {
    /// All five opening messages have been read.
    pub fn opening_read(&self, admission: &wire::PeerAdmission) -> bool {
        self.controllable.is_some()
            && self.feel.is_some()
            && self.focus.is_some()
            && (!admission.relative() || self.game_mode.is_some())
            && (!admission.drag() || self.dnd_caps.is_some())
    }
}

/// Input-thread adapter: a completed stream pair attaches the peer with its opening
/// state. Detach removes online eligibility and asks the router to release that peer's
/// contributions; the injection thread acknowledges completion before another attach.
/// Exact stream identities remain in the input owner, outside the router.
pub trait RouterAdapter {
    fn attach(&mut self, admission: &wire::PeerAdmission, state: &ReceiveState, peer_os: oink_routing::Os);
    fn detach(&mut self, peer: DeviceId);
    fn attached(&self, peer: DeviceId) -> bool;
    /// A message read from the inbound I0. Attached: to the router, state messages also
    /// recorded. Not attached: state messages recorded only; actions dropped.
    fn inbound(&mut self, peer: DeviceId, message: Message);
    /// This device's opening state, read from the router without changing it (the router
    /// sends nothing before the pair is attached).
    fn opening_state(&self, controllable: bool) -> [Message; OPENING_MESSAGES];
}

impl ReceiveState {
    /// State messages keep their newest value even while the pair is detached.
    pub fn record(&mut self, message: &Message) {
        match message {
            Message::Control(ControlMsg::InputCapability {
                controllable,
                touchpad_contacts,
            }) => {
                self.controllable = Some(*controllable);
                self.touchpad_contacts = *touchpad_contacts;
            }
            Message::Control(ControlMsg::Feel { profile }) => self.feel = Some(profile.clone()),
            Message::Control(m @ ControlMsg::FocusState { .. }) => self.focus = Some(m.clone()),
            Message::Control(m @ ControlMsg::GameMode { .. }) => self.game_mode = Some(m.clone()),
            Message::Dnd(m @ DndMsg::Caps { .. }) => self.dnd_caps = Some(m.clone()),
            _ => {}
        }
    }
}

/// Concrete adapter owned by the native input loop. It has no system backend and can
/// also drive the real router in the simulator. Input stream identities stay outside it.
#[derive(Debug)]
pub struct Adapter {
    router: oink_routing::Router,
    received: std::collections::HashMap<DeviceId, ReceiveState>,
    active: std::collections::HashSet<DeviceId>,
    now: oink_routing::Micros,
}
impl Adapter {
    pub fn new(router: oink_routing::Router) -> Self {
        Self {
            router,
            received: Default::default(),
            active: Default::default(),
            now: 0,
        }
    }
    pub fn router(&self) -> &oink_routing::Router {
        &self.router
    }
    pub fn router_mut(&mut self) -> &mut oink_routing::Router {
        &mut self.router
    }
    pub fn at(&mut self, now: oink_routing::Micros) {
        self.now = now;
    }
    pub fn receive_state(&self, peer: DeviceId) -> Option<&ReceiveState> {
        self.received.get(&peer)
    }
    pub fn clear_received(&mut self, peer: DeviceId) {
        self.received.remove(&peer);
    }
}
impl std::ops::Deref for Adapter {
    type Target = oink_routing::Router;
    fn deref(&self) -> &Self::Target {
        &self.router
    }
}
impl std::ops::DerefMut for Adapter {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.router
    }
}
impl RouterAdapter for Adapter {
    fn attach(&mut self, admission: &wire::PeerAdmission, state: &ReceiveState, peer_os: oink_routing::Os) {
        let peer = admission.boot.device_id;
        if self.attached(peer) || !state.opening_read(admission) {
            return;
        }
        let Some(ControlMsg::FocusState {
            session: Some(instance),
            ..
        }) = state.focus.as_ref()
        else {
            return;
        };
        let enabled = admission.drag() && matches!(state.dnd_caps, Some(DndMsg::Caps { enabled: true }));
        self.router.set_peer_os(peer, peer_os, self.now);
        if !self.router.attach_input(
            admission.boot,
            *instance,
            state.controllable == Some(true),
            enabled,
            state.feel.clone().expect("complete opening"),
            self.now,
        ) {
            return;
        }
        self.router.set_peer_touchpad_contacts(peer, state.touchpad_contacts);
        self.active.insert(peer);
        self.received.insert(peer, state.clone());
        for message in [
            state.focus.clone().map(Message::Control),
            state.game_mode.clone().map(Message::Control),
        ]
        .into_iter()
        .flatten()
        {
            self.router.input_message(peer, message, self.now);
        }
        self.router.resend_input_preferences(peer);
    }
    fn detach(&mut self, peer: DeviceId) {
        if self.active.remove(&peer) {
            self.router.detach_input(peer);
        }
    }
    fn attached(&self, peer: DeviceId) -> bool {
        self.active.contains(&peer)
    }
    fn inbound(&mut self, peer: DeviceId, message: Message) {
        self.received.entry(peer).or_default().record(&message);
        if self.active.contains(&peer) {
            self.router.input_message(peer, message, self.now);
        }
    }
    fn opening_state(&self, controllable: bool) -> [Message; OPENING_MESSAGES] {
        self.router.input_opening(controllable)
    }
}

use super::{
    StreamModule,
    machine::{Admit, Failure, InboundOrdinal, Step, StreamMachine},
};
use crate::{
    config_module::ConfigModule,
    threads::input::{InputCommand, InputNotice, InputNotices, InputThread, Which},
};
use oink_network::link::{
    InboundLow, InboundStream, LinkId, LinkLayer, LinkLost, ModuleEvent, QueueSpec, StreamClosed, StreamId,
};
use oink_protocol::frame::{Priority, StreamCode};
use std::{collections::HashMap, sync::Arc, time::Instant};

#[derive(Debug)]
struct PeerStreams {
    link: LinkId,
    boot: oink_foundation::session::Boot,
    machine: StreamMachine,
    ordinal: InboundOrdinal,
    outbound: Option<StreamId>,
    inbound: Option<StreamId>,
    wait_since: Option<Instant>,
    wait_reported: bool,
}
/// Ordinary-thread I0/I1 stream management. The native input owner receives handles and
/// never waits for this task to accept its peer's new stream or open its own replacement.
#[derive(Debug)]
pub struct InputStreams {
    links: LinkLayer,
    config: ConfigModule,
    input: Arc<InputThread>,
    peers: HashMap<DeviceId, PeerStreams>,
    paused: bool,
    controllable: bool,
    touchpad_contacts: Option<u8>,
}
impl InputStreams {
    pub fn new(links: LinkLayer, config: ConfigModule, input: Arc<InputThread>, controllable: bool) -> Self {
        let touchpad_contacts = input.touchpad_contacts();
        let paused = config.applied().paused;
        input.send(InputCommand::LocalDisplays(config.local_displays().displays.clone()));
        input.send(InputCommand::Controllable(controllable));
        input.send(InputCommand::Paused(paused));
        Self {
            links,
            config,
            input,
            peers: HashMap::new(),
            paused,
            controllable,
            touchpad_contacts,
        }
    }
    /// Spawn before dialing starts so every ready link is observed.
    pub fn spawn(
        links: LinkLayer,
        config: ConfigModule,
        input: Arc<InputThread>,
        notices: InputNotices,
        controllable: bool,
    ) -> tokio::task::JoinHandle<()> {
        let port = links.register(KINDS);
        Self::spawn_with_port(links, config, input, notices, controllable, port)
    }
    /// Consume a port registered before config discovery begins.
    pub fn spawn_with_port(
        links: LinkLayer,
        config: ConfigModule,
        input: Arc<InputThread>,
        notices: InputNotices,
        controllable: bool,
        port: oink_network::link::ModulePort,
    ) -> tokio::task::JoinHandle<()> {
        let changes = config.subscribe();
        let this = Self::new(links, config, input, controllable);
        tokio::spawn(this.run(port, changes, notices))
    }
    async fn run(
        mut self,
        mut port: oink_network::link::ModulePort,
        mut changes: crate::config_module::ConfigChanges,
        mut notices: InputNotices,
    ) {
        let mut diagnostics_at = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let due = self.peers.values().filter_map(|p| p.machine.deadline()).min();
            tokio::select! {
                event = port.next() => match event {
                    Some(ModuleEvent::LinkReady { link, peer }) => self.link_ready(link, peer),
                    Some(ModuleEvent::LinkLost { link, peer, reason }) => self.link_lost(link, peer, reason),
                    Some(ModuleEvent::Inbound(stream)) => self.inbound(stream),
                    Some(ModuleEvent::InboundLow(operation)) => self.inbound_low(*operation),
                    Some(ModuleEvent::StreamClosed { stream, reason }) => self.stream_closed(stream, reason),
                    Some(ModuleEvent::PathChanged { .. }) => {},
                    // Network violations are broadcast to D0, which records them once.
                    Some(ModuleEvent::ProtocolViolation { .. }) => {},
                    None => break,
                },
                change = changes.next() => match change {
                    Some(crate::config_module::ConfigChange::Facts { device, capabilities_changed }) => {
                        self.facts_changed(device,capabilities_changed);
                    },
                    Some(crate::config_module::ConfigChange::LocalDisplays) => self.input.send(InputCommand::LocalDisplays(self.config.local_displays().displays.clone())),
                    Some(_) => {
                        self.input.wake()(); // Apply the shared layout even when native capability stays None (Mac source).
                        self.refresh_contacts();
                        self.pause_changed(self.config.applied().paused);
                    },
                    None => break,
                },
                notice = notices.next() => match notice {
                    Some(notice) => self.notice(notice),
                    None => break,
                },
                _ = tokio::time::sleep_until(diagnostics_at.into()) => {
                    self.refresh_contacts();
                    self.input.report_command_rejections();
                    diagnostics_at = Instant::now() + std::time::Duration::from_secs(5);
                },
                _ = async {
                    match due { Some(due) => tokio::time::sleep_until(due.into()).await, None => std::future::pending().await }
                } => self.poll(),
            }
            self.report_waits(Instant::now());
        }
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            Self::wait_end(peer, &self.peers[&peer], "stopped", Instant::now());
            if let Some(stream) = self.peers[&peer].outbound {
                self.links.close_stream(stream, StreamCode::None);
            }
            self.input.send(InputCommand::OutboundGone { peer });
        }
    }
    fn wait_end(peer: DeviceId, p: &PeerStreams, reason: &'static str, now: Instant) {
        if p.wait_reported {
            tracing::info!(target: "oink_link_diagnostics", kind = "input_wait", peer_id = peer.to_hex().as_str(),
                stage = "input_opening", result = "ended", reason,
                elapsed_ms = p.wait_since.map_or(0., |at| now.saturating_duration_since(at).as_secs_f64() * 1000.),
                "Input opening long wait ended");
        }
    }
    fn report_waits(&mut self, now: Instant) {
        let status = self.input.status();
        for (peer, p) in &mut self.peers {
            Self::observe_wait(
                *peer,
                p,
                status.peers.iter().find(|s| s.peer == *peer),
                self.paused,
                now,
            );
        }
    }
    fn observe_wait(
        peer: DeviceId,
        p: &mut PeerStreams,
        observed: Option<&crate::threads::input::PeerInput>,
        paused: bool,
        now: Instant,
    ) {
        if paused || observed.is_some_and(|s| s.attached) {
            InputStreams::wait_end(peer, p, if paused { "paused" } else { "attached" }, now);
            p.wait_since = None;
            p.wait_reported = false;
            return;
        }
        let since = p.wait_since.get_or_insert(now);
        if !p.wait_reported && now.saturating_duration_since(*since) >= std::time::Duration::from_secs(3) {
            p.wait_reported = true;
            tracing::info!(target: "oink_link_diagnostics", kind = "input_wait", peer_id = peer.to_hex().as_str(),
                    stage = "input_opening", result = "long", reason = "opening_incomplete",
                    elapsed_ms = now.saturating_duration_since(*since).as_secs_f64() * 1000.,
                    wait_outbound = observed.is_none_or(|s| s.outbound.is_none()),
                    wait_inbound = observed.is_none_or(|s| s.inbound.is_none()),
                    opening_received = observed.map_or(0, |s| s.opening_received),
                    wait_release = observed.is_some_and(|s| s.releasing),
                    "Input opening long wait");
        }
    }
    fn refresh_contacts(&mut self) {
        let contacts = self.input.touchpad_contacts();
        if contacts == self.touchpad_contacts {
            return;
        }
        self.touchpad_contacts = contacts;
        let now = Instant::now();
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            let step = self.peers.get_mut(&peer).unwrap().machine.reopen(now);
            self.step(peer, step);
        }
    }
    fn controllable_changed(&mut self, controllable: bool) {
        if self.controllable == controllable {
            return;
        }
        self.controllable = controllable;
        let now = Instant::now();
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            let step = self.peers.get_mut(&peer).unwrap().machine.reopen(now);
            self.step(peer, step);
        }
    }
    fn os(&self, peer: DeviceId) -> oink_routing::Os {
        self.config
            .members()
            .members
            .iter()
            .find(|m| m.device == peer)
            .map(|m| oink_routing::Os::parse(&m.os))
            .unwrap_or(oink_routing::Os::Other)
    }
    fn poll(&mut self) {
        let now = Instant::now();
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            let step = self.peers.get_mut(&peer).unwrap().machine.poll(!self.paused, true, now);
            self.step(peer, step);
        }
    }
    fn step(&mut self, peer: DeviceId, step: Step) {
        let (close, open) = match step {
            Step::Nothing => return,
            Step::Close(close) => (Some(close), None),
            Step::Open(open) => (None, Some(open)),
            Step::Reopen { close, open } => (Some(close), Some(open)),
        };
        let p = self.peers.get_mut(&peer).unwrap();
        if let Some(tag) = close {
            if let Some(stream) = p.outbound.take().filter(|s| s.tag == tag) {
                self.links.close_stream(stream, StreamCode::None);
            }
            self.input.send(InputCommand::OutboundGone { peer });
        }
        if let Some(tag) = open {
            match self.links.open_stream(
                p.link,
                Priority::Interactive,
                tag,
                QueueSpec::reliable(StreamKind::Input).0,
                self.config
                    .peer_agreement(peer)
                    .filter(|agreement| agreement.contains("oink.input.relative/1"))
                    .map(|_| QueueSpec::datagram(StreamKind::Input)),
            ) {
                Ok((input, motion)) => {
                    p.outbound = Some(input.id());
                    self.input.send(InputCommand::Outbound { peer, input, motion });
                }
                Err(_) => {
                    p.machine.poll(false, true, Instant::now());
                }
            }
        }
    }
    fn notice(&mut self, notice: InputNotice) {
        match notice {
            InputNotice::Controllable(controllable) => self.controllable_changed(controllable),
            InputNotice::ProtocolViolation {
                peer,
                stream,
                type_id,
                code,
                datagram,
            } => {
                let Some(pair) = self
                    .peers
                    .get(&peer)
                    .filter(|p| p.link == stream.link && p.inbound == Some(stream))
                else {
                    return;
                };
                let Some(agreement) = self.config.peer_agreement(peer) else {
                    return;
                };
                let admission = wire::PeerAdmission {
                    boot: pair.boot,
                    link: pair.link,
                    agreement,
                };
                self.links
                    .report_protocol_violation(stream.link, admission.violation(type_id, code, datagram));
            }
            InputNotice::Fault {
                peer,
                which,
                full,
                code,
                stream,
            } => {
                tracing::debug!(?peer, ?which, full, ?code, ?stream, "input owner fault");
                let Some(p) = self.peers.get_mut(&peer) else {
                    return;
                };
                match which {
                    Which::Outbound if p.outbound == Some(stream) => {
                        self.links.close_stream(stream, code);
                        p.outbound = None;
                        let step = p
                            .machine
                            .failed(if full { Failure::Full } else { Failure::Peer(code) }, Instant::now());
                        self.step(peer, step);
                        self.poll();
                    }
                    Which::Inbound if p.inbound == Some(stream) => {
                        self.links.close_stream(stream, code);
                        p.inbound = None;
                    }
                    _ => {}
                }
            }
            InputNotice::EmergencyPause => {
                self.pause_changed(true);
                // The configuration owner consumes the input owner's local emergency sequence and publishes one C0 event.
            }
        }
    }
}
impl StreamModule for InputStreams {
    const KINDS: &'static [StreamKind] = KINDS;
    fn link_ready(&mut self, link: LinkId, peer: DeviceId) {
        if self.peers.get(&peer).is_some_and(|p| p.link == link) {
            return;
        }
        if let Some(old) = self.peers.remove(&peer) {
            Self::wait_end(peer, &old, "link_lost", Instant::now());
            self.input.send(InputCommand::OutboundGone { peer });
            if let Some(stream) = old.inbound {
                self.input.send(InputCommand::InboundGone { peer, stream });
            }
        }
        let (Some(boot), Some(agreement)) = (self.config.peer_boot(peer), self.config.peer_agreement(peer)) else {
            return;
        };
        self.peers.insert(
            peer,
            PeerStreams {
                link,
                boot,
                machine: StreamMachine::new(StreamKind::Input),
                ordinal: InboundOrdinal::default(),
                outbound: None,
                inbound: None,
                wait_since: Some(Instant::now()),
                wait_reported: false,
            },
        );
        self.input.send(InputCommand::PeerAdmission(wire::PeerAdmission {
            boot,
            link,
            agreement,
        }));
        // The facts may already be cached before this task observes LinkReady.
        self.facts_changed(peer, false);
        self.poll();
    }
    fn link_lost(&mut self, link: LinkId, peer: DeviceId, _: LinkLost) {
        if !self.peers.get(&peer).is_some_and(|p| p.link == link) {
            return;
        }
        let old = self.peers.remove(&peer).unwrap();
        Self::wait_end(peer, &old, "link_lost", Instant::now());
        self.input.send(InputCommand::RetirePeer(old.boot));
        self.input.send(InputCommand::OutboundGone { peer });
        if let Some(stream) = old.inbound {
            self.input.send(InputCommand::InboundGone { peer, stream });
        }
    }
    fn inbound(&mut self, stream: InboundStream) {
        let peer = stream.peer;
        let Some(p) = self.peers.get_mut(&peer).filter(|p| p.link == stream.link) else {
            stream.reject(StreamCode::NotReady);
            return;
        };
        let relative = self
            .config
            .peer_agreement(peer)
            .is_some_and(|agreement| agreement.contains("oink.input.relative/1"));
        if stream.tag.kind != StreamKind::Input
            || stream.priority != Priority::Interactive
            || stream.has_datagram != relative
        {
            stream.reject(StreamCode::Invalid);
            return;
        }
        if p.ordinal.admit(stream.tag.ordinal) == Admit::Stale {
            stream.reject(StreamCode::StaleOrdinal);
            return;
        }
        if let Some(old) = p.inbound.take() {
            self.links.close_stream(old, StreamCode::None);
            self.input.send(InputCommand::InboundGone { peer, stream: old });
        }
        let (input, motion) = stream.accept(
            QueueSpec::reliable(StreamKind::Input).1,
            QueueSpec::datagram(StreamKind::Input),
        );
        p.inbound = Some(input.id());
        self.input.send(InputCommand::Inbound {
            peer,
            input,
            motion,
            peer_os: self.os(peer),
        });
    }
    fn inbound_low(&mut self, mut operation: InboundLow) {
        operation.stream.cancel();
    }
    fn stream_closed(&mut self, stream: StreamId, reason: StreamClosed) {
        let Some(peer) = self
            .peers
            .iter()
            .find_map(|(peer, p)| (p.link == stream.link).then_some(*peer))
        else {
            return;
        };
        let p = self.peers.get_mut(&peer).unwrap();
        if p.inbound == Some(stream) {
            p.inbound = None;
            self.input.send(InputCommand::InboundGone { peer, stream });
        } else if p.outbound == Some(stream) {
            p.outbound = None;
            self.input.send(InputCommand::OutboundGone { peer });
            let step = match reason {
                StreamClosed::Peer(code) => p.machine.failed(Failure::Peer(code), Instant::now()),
                StreamClosed::LinkLost | StreamClosed::Local => p.machine.poll(false, true, Instant::now()),
            };
            self.step(peer, step);
            if reason != StreamClosed::LinkLost {
                self.poll();
            }
        }
    }
    fn pause_changed(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        self.input.send(InputCommand::Paused(paused));
        self.poll();
    }
    fn facts_changed(&mut self, peer: DeviceId, capabilities_changed: bool) {
        if peer == self.links.id() {
            return;
        }
        let Some(boot) = self.config.peer_boot(peer) else {
            return;
        };
        let members = self.config.members();
        let Some(facts) = members
            .members
            .iter()
            .find(|member| member.device == peer)
            .and_then(|member| member.facts.as_ref())
        else {
            return;
        };
        let project = || -> oink_protocol::v1::Result<_> {
            use oink_protocol::v1::{dto, schema::WireValue};
            if oink_network::wire_identity::read_boot(&facts.boot()?)? != boot {
                return Err(oink_protocol::v1::ErrorCode::AuthenticationFailed.into());
            }
            let mut displays = Vec::new();
            for display in facts.displays()?.0 {
                let bounds = display.bounds()?;
                let extent = display
                    .reference_extent()?
                    .map(|size| Ok::<_, oink_protocol::v1::Error>(oink_foundation::Size::new(size.w()?, size.h()?)))
                    .transpose()?;
                displays.push(oink_foundation::display::DisplayInfo {
                    key: display.key()?.0,
                    name: display.name()?.0,
                    bounds: oink_foundation::Rect::new(bounds.x()?, bounds.y()?, bounds.w()?, bounds.h()?),
                    pixel_width: display.pixel_width()?,
                    pixel_height: display.pixel_height()?,
                    reference_extent: extent,
                    rotation_deg: display.rotation_deg()?,
                    is_primary: display.is_primary()?,
                    is_builtin: display.is_builtin()?,
                    is_virtual: display.is_virtual()?,
                });
            }
            let mut transform = None;
            for option in facts.preferences()?.map_or(Vec::new(), |values| values.0) {
                if option.option()? == dto::LocalOption::InputTransform {
                    let value = dto::InputTransformValue::read(&option.value()?)?;
                    transform = Some(
                        crate::config::read_transform(value)
                            .map_err(|_| oink_protocol::v1::ErrorCode::UnsupportedValue)?,
                    );
                }
            }
            Ok((displays, transform, oink_routing::Os::parse(&facts.os()?.0)))
        };
        if let Ok((displays, transform, os)) = project() {
            self.input.send(InputCommand::PeerFacts {
                boot,
                os,
                displays,
                transform,
            });
        }
        if capabilities_changed && let Some(p) = self.peers.get_mut(&peer) {
            p.machine.capabilities_changed();
            self.poll();
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_foundation::{
        Point,
        input::{InjectEvent, KeyCode},
    };
    use oink_routing::input::InputMsg;
    use oink_routing::{Os, Output, Router, RouterConfig, SimpleLayout};
    const ME: DeviceId = DeviceId([1; 32]);
    const PEER: DeviceId = DeviceId([2; 32]);
    pub(super) fn boot(device_id: DeviceId) -> oink_foundation::session::Boot {
        oink_foundation::session::Boot {
            device_id,
            nonce: [device_id.0[0]; 16],
        }
    }
    pub(super) fn admission(peer: DeviceId) -> wire::PeerAdmission {
        wire::PeerAdmission {
            boot: boot(peer),
            link: oink_network::link::LinkId(1),
            agreement: Arc::new(
                oink_protocol::v1::admission::Agreement::new(
                    [
                        "oink.core/1",
                        "oink.group/1",
                        "oink.clipboard/1",
                        "oink.low/1",
                        "oink.input/1",
                        "oink.input.relative/1",
                        "oink.drag/1",
                    ]
                    .into_iter()
                    .map(str::to_owned),
                )
                .unwrap(),
            ),
        }
    }
    fn adapter() -> Adapter {
        Adapter::new(Router::new(
            RouterConfig::new(boot(ME), "local", Os::MacOS),
            Arc::new(SimpleLayout::default()),
        ))
    }
    fn opening() -> ReceiveState {
        let remote = Router::new(
            RouterConfig::new(boot(PEER), "peer", Os::Windows),
            Arc::new(SimpleLayout::default()),
        );
        let mut state = ReceiveState::default();
        for message in remote.input_opening(true) {
            state.record(&message);
        }
        state
    }
    fn key(pressed: bool) -> Message {
        Message::Input(InputMsg::Key {
            code: KeyCode::keyboard(4),
            pressed,
        })
    }
    pub(super) fn enter(adapter: &Adapter) -> Message {
        let Some(ControlMsg::FocusState {
            session: Some(session), ..
        }) = adapter.receive_state(PEER).and_then(|state| state.focus.as_ref())
        else {
            panic!("accepted peer focus session missing");
        };
        Message::Control(ControlMsg::FocusEnter {
            epoch: 2,
            display: "screen".into(),
            pos: Point::new(5., 5.),
            scope: Some(oink_routing::control::FocusScope {
                members: vec![(ME, adapter.router().routing_session()), (PEER, *session)],
            }),
            pointer_source: Some(PEER),
        })
    }
    #[test]
    fn five_opening_records_are_read_only_and_have_fixed_order() {
        let mut a = adapter();
        a.router_mut().drain_outputs();
        let messages = a.opening_state(false);
        assert!(matches!(
            messages[0],
            Message::Control(ControlMsg::InputCapability {
                controllable: false,
                touchpad_contacts: None
            })
        ));
        assert!(matches!(messages[1], Message::Control(ControlMsg::Feel { .. })));
        assert!(matches!(
            messages[2],
            Message::Control(ControlMsg::FocusState { session: Some(_), .. })
        ));
        assert!(matches!(messages[3], Message::Control(ControlMsg::GameMode { .. })));
        assert!(matches!(messages[4], Message::Dnd(DndMsg::Caps { .. })));
        assert!(a.router_mut().drain_outputs().is_empty());
        let mut state = ReceiveState::default();
        for message in &messages[..4] {
            state.record(message);
            assert!(!state.opening_read(&admission(PEER)));
        }
        state.record(&messages[4]);
        assert!(state.opening_read(&admission(PEER)));
    }
    #[test]
    fn detached_actions_do_not_enter_router_and_state_survives_outbound_reopen() {
        let mut a = adapter();
        let state = opening();
        a.router_mut().drain_outputs();
        a.inbound(PEER, key(true));
        assert!(a.router_mut().drain_outputs().is_empty());
        a.attach(&admission(PEER), &state, Os::Windows);
        assert!(a.attached(PEER));
        a.inbound(PEER, enter(&a));
        a.inbound(PEER, key(true));
        assert!(
            a.router_mut()
                .drain_outputs()
                .iter()
                .any(|o| matches!(o, Output::Inject(InjectEvent::Key { pressed: true, .. })))
        );
        a.detach(PEER);
        let released = a.router_mut().drain_outputs();
        assert_eq!(
            released
                .iter()
                .filter(|o| matches!(o, Output::Inject(InjectEvent::Key { pressed: false, .. })))
                .count(),
            1
        );
        a.detach(PEER);
        a.inbound(PEER, key(false));
        assert!(
            !a.router_mut()
                .drain_outputs()
                .iter()
                .any(|o| matches!(o, Output::Inject(_)))
        );
        assert_eq!(a.receive_state(PEER), Some(&state));
        let mut updated = state.feel.unwrap();
        updated.key_repeat_delay_ms = 731;
        a.inbound(
            PEER,
            Message::Control(ControlMsg::Feel {
                profile: updated.clone(),
            }),
        );
        let newest = a.receive_state(PEER).unwrap().clone();
        a.attach(&admission(PEER), &newest, Os::Windows);
        assert_eq!(a.router().peer_feel(PEER), Some(&updated));
        assert!(a.router_mut().drain_outputs().iter().any(|o| matches!(
            o,
            Output::Send {
                msg: Message::Control(ControlMsg::Feel { .. }),
                ..
            }
        )));
    }
    #[test]
    fn replacing_inbound_discards_receive_state_and_requires_all_five_again() {
        let mut a = adapter();
        let state = opening();
        a.attach(&admission(PEER), &state, Os::Windows);
        a.detach(PEER);
        a.clear_received(PEER);
        a.inbound(
            PEER,
            Message::Control(ControlMsg::Feel {
                profile: FeelProfile::default(),
            }),
        );
        let incomplete = a.receive_state(PEER).unwrap().clone();
        a.attach(&admission(PEER), &incomplete, Os::MacOS);
        assert!(!a.attached(PEER));
        assert!(!incomplete.opening_read(&admission(PEER)));
    }
    #[test]
    fn reattach_uses_latest_focus_and_resends_latest_local_preferences() {
        let mut a = adapter();
        let mut state = opening();
        a.attach(&admission(PEER), &state, Os::Windows);
        a.detach(PEER);
        let mut newest = state.focus.clone().unwrap();
        if let ControlMsg::FocusState { epoch, .. } = &mut newest {
            *epoch = 29;
        }
        a.inbound(PEER, Message::Control(newest.clone()));
        state.focus = Some(newest);
        let feel = FeelProfile {
            key_repeat_delay_ms: 832,
            ..Default::default()
        };
        a.router_mut().set_local_feel(feel.clone());
        a.router_mut().set_dnd_available(true);
        a.router_mut().drain_outputs();
        let recorded = a.receive_state(PEER).unwrap().clone();
        a.attach(&admission(PEER), &recorded, Os::Windows);
        assert_eq!(a.receive_state(PEER).unwrap().focus, state.focus);
        let outputs = a.router_mut().drain_outputs();
        assert!(outputs.iter().any(|o| matches!(o, Output::Send { msg: Message::Control(ControlMsg::Feel { profile }), .. } if profile == &feel)));
        assert!(outputs.iter().any(|o| matches!(
            o,
            Output::Send {
                msg: Message::Dnd(DndMsg::Caps { enabled: true }),
                ..
            }
        )));
    }
    #[test]
    fn cannot_be_controlled_keeps_pair_and_group_pause_changes_router_instance() {
        let mut a = adapter();
        let state = opening();
        a.attach(&admission(PEER), &state, Os::Windows);
        let initial = a.router().routing_session();
        a.router_mut().set_paused(true);
        assert!(a.attached(PEER));
        assert_eq!(a.router().routing_session(), initial);
        a.inbound(PEER, enter(&a));
        a.inbound(PEER, key(true));
        assert!(
            !a.router_mut()
                .drain_outputs()
                .iter()
                .any(|o| matches!(o, Output::Inject(InjectEvent::Key { pressed: true, .. })))
        );
        a.router_mut().set_fully_paused(true);
        a.detach(PEER);
        assert_ne!(a.router().routing_session(), initial);
    }
}

#[cfg(all(test, debug_assertions))]
mod os_tests {
    use super::tests::{admission, boot, enter};
    use super::*;
    use oink_foundation::input::{InjectEvent, KeyCode};
    use oink_routing::input::InputMsg;
    use oink_routing::{Os, Output, Router, RouterConfig, SimpleLayout};

    #[test]
    fn handshake_os_maps_modifiers_before_facts_and_facts_override_it() {
        let local = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let mut input = Adapter::new(Router::new(
            RouterConfig::new(boot(local), "local", Os::MacOS),
            Arc::new(SimpleLayout::new()),
        ));
        let remote = Router::new(
            RouterConfig::new(boot(peer), "peer", Os::Windows),
            Arc::new(SimpleLayout::new()),
        );
        let mut state = ReceiveState::default();
        for message in remote.input_opening(true) {
            state.record(&message);
        }
        input.attach(&admission(peer), &state, Os::Windows);
        input.inbound(peer, enter(&input));
        input.drain_outputs();
        input.inbound(
            peer,
            Message::Input(InputMsg::Key {
                code: KeyCode::LEFT_CTRL,
                pressed: true,
            }),
        );
        assert!(input.drain_outputs().iter().any(|event| matches!(
            event,
            Output::Inject(InjectEvent::Key {
                code: KeyCode::LEFT_GUI,
                pressed: true
            })
        )));
        input.inbound(
            peer,
            Message::Input(InputMsg::Key {
                code: KeyCode::LEFT_CTRL,
                pressed: false,
            }),
        );
        input.drain_outputs();
        input.set_peer_os(peer, Os::MacOS, 0);
        input.inbound(
            peer,
            Message::Input(InputMsg::Key {
                code: KeyCode::LEFT_CTRL,
                pressed: true,
            }),
        );
        assert!(input.drain_outputs().iter().any(|event| matches!(
            event,
            Output::Inject(InjectEvent::Key {
                code: KeyCode::LEFT_CTRL,
                pressed: true
            })
        )));
        assert!(input.attached(peer));
    }
}

#[cfg(all(test, debug_assertions))]
mod wait_tests {
    use super::*;
    #[test]
    fn long_wait_reports_once_and_resets_on_attach_pause_or_loss() {
        let now = Instant::now();
        let peer = DeviceId([1; 32]);
        let mut p = PeerStreams {
            boot: super::tests::boot(peer),
            link: LinkId(1),
            machine: StreamMachine::new(StreamKind::Input),
            ordinal: InboundOrdinal::default(),
            outbound: None,
            inbound: None,
            wait_since: Some(now),
            wait_reported: false,
        };
        InputStreams::observe_wait(peer, &mut p, None, false, now + std::time::Duration::from_secs(2));
        assert!(!p.wait_reported);
        InputStreams::observe_wait(peer, &mut p, None, false, now + std::time::Duration::from_secs(3));
        assert!(p.wait_reported);
        let since = p.wait_since;
        InputStreams::observe_wait(peer, &mut p, None, false, now + std::time::Duration::from_secs(100));
        assert_eq!(p.wait_since, since);
        InputStreams::observe_wait(peer, &mut p, None, true, now + std::time::Duration::from_secs(101));
        assert!(!p.wait_reported);
        assert!(p.wait_since.is_none());
        InputStreams::observe_wait(peer, &mut p, None, false, now + std::time::Duration::from_secs(102));
        assert_eq!(p.wait_since, Some(now + std::time::Duration::from_secs(102)));
        let observed = crate::threads::input::PeerInput {
            peer,
            inbound: None,
            outbound: None,
            opening_received: 5,
            releasing: false,
            attached: true,
            online: false,
            touchpad_contacts: None,
            feel: None,
        };
        InputStreams::observe_wait(
            peer,
            &mut p,
            Some(&observed),
            false,
            now + std::time::Duration::from_secs(106),
        );
        assert!(p.wait_since.is_none());
        assert!(!p.wait_reported);
    }
}
