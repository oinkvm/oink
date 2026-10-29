//! Link control plane. Time is supplied by the executor; transitions only emit effects.
//! Candidate selection, matched heartbeats and relay reservations are ported from the
//! old owner core. No session, acknowledgement, replay or consumer state is retained.
mod relay;
#[cfg(test)]
mod tests;
use super::*;
use crate::link::control::{
    LinkMsg, Migration, MigrationId, NextHopBinding, PathInstall, PathReference, PathRequestId, Pulse, Refusal,
    RelayReservation,
};
use oink_foundation::session::{BootPair, ConnectionId, PathId, RouteRevision};
use relay::{Nomination, PendingRequest, Request, Transit};
use std::{
    collections::{HashMap, VecDeque},
    time::Instant,
};

#[derive(Clone, Debug)]
pub(super) enum Effect {
    Control {
        connection: ConnectionId,
        tag: u64,
        message: Box<LinkMsg>,
    },
    CloseConnection(ConnectionId),
    Connected(LinkId),
    Ready(LinkId),
    Lost {
        link: LinkId,
        peer: DeviceId,
        path: LinkPath,
        reason: LinkLost,
    },
    Dial {
        peer: DeviceId,
        addresses: Vec<SocketAddr>,
    },
    DialFailed(DeviceId, DialFailure),
    DialFinished(DeviceId),
    Freeze(LinkId),
    PathChanged {
        link: LinkId,
        old: NextHopBinding,
    },
    CloseTransit(PathId),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    Candidate,
    Offered(u64),
    Selected(u64),
}
#[derive(Debug)]
struct Candidate {
    boot: Boot,
    arrival: u64,
    started: Instant,
    choice: Choice,
    deadline: Instant,
}
#[derive(Debug)]
struct Heartbeat {
    next: Instant,
    nonce: u64,
    pending: VecDeque<(u64, Instant)>,
    progress: Instant,
    rtt: Option<Duration>,
}
impl Heartbeat {
    fn new(now: Instant) -> Self {
        Self {
            next: now,
            nonce: 0,
            pending: VecDeque::new(),
            progress: now,
            rtt: None,
        }
    }
}
#[derive(Debug)]
struct Cutover {
    wire: Migration,
    next_hop: NextHopBinding,
    deadline: Instant,
    drained: bool,
    peer_drained: bool,
    commit_sent: bool,
    drain_sent: bool,
    started: Instant,
    stage_started: Instant,
    counts_known: bool,
    writers_pending: usize,
    readers_pending: usize,
}
#[derive(Debug)]
pub(super) struct Link {
    pub id: LinkId,
    pub peer: Boot,
    pub state: LinkState,
    pub wire: PathInstall,
    pub hop: NextHopBinding,
    pub direction: Direction,
    pub installed: bool,
    installation_deadline: Instant,
    report_setup_started: Instant,
    report_config_started: Option<Instant>,
    heartbeat: Heartbeat,
    cutover: Option<Cutover>,
    addresses: Vec<SocketAddr>,
    next_direct: Instant,
    direct_failures: usize,
}
impl Link {
    pub fn path(&self) -> LinkPath {
        if self.wire.reservation.is_some() {
            LinkPath::Relayed { via: self.hop.peer }
        } else {
            LinkPath::Direct
        }
    }
    pub fn frozen(&self) -> bool {
        self.cutover.is_some()
    }
    pub fn rtt(&self) -> Option<Duration> {
        self.heartbeat.rtt
    }
    pub fn rtt_measured_at(&self) -> Option<Instant> {
        self.heartbeat.rtt.map(|_| self.heartbeat.progress)
    }
}
#[derive(Debug)]
struct Dial {
    mode: DialMode,
    addresses: Vec<SocketAddr>,
    direct: bool,
    deadline: Instant,
    request: Option<Request>,
}
#[derive(Debug)]
pub(super) struct Core {
    pub boot: Boot,
    pub links: HashMap<LinkId, Link>,
    candidates: HashMap<ConnectionId, Candidate>,
    choices: HashMap<DeviceId, (Boot, u64)>,
    dials: HashMap<DeviceId, Dial>,
    nominations: HashMap<DeviceId, Nomination>,
    pending_relay: Vec<PendingRequest>,
    pub transit: Vec<Transit>,
    counter: u64,
    pub effects: VecDeque<Effect>,
}
impl Cutover {
    fn waiting(&self, coordinator: bool) -> &'static str {
        if !self.drained {
            "local_drain"
        } else if coordinator && !self.peer_drained {
            "peer_drained"
        } else if coordinator {
            "peer_committed"
        } else {
            "peer_commit"
        }
    }
    fn trace(
        &self,
        peer: DeviceId,
        coordinator: bool,
        stage: &'static str,
        result: &'static str,
        reason: &'static str,
        now: Instant,
    ) {
        if matches!(result, "completed" | "failed") {
            super::timing::record(
                super::timing::Measurement::Stage(super::timing::Stage::Migration),
                self.stage_started,
                now,
            );
        }
        tracing::info!(target: "oink_link_diagnostics", kind = "migration_step",
            peer_id = peer.to_hex().as_str(), initiator = if coordinator { "local" } else { "peer" },
            operation = diagnostics::hex(&self.wire.new_connection.random).as_str(),
            path_counter = self.wire.old_path.coordinator_counter,
            revision = self.wire.migration_id.to_revision.0, stage, result, reason,
            waiting = self.waiting(coordinator),
            elapsed_ms = now.saturating_duration_since(self.stage_started).as_secs_f64() * 1000.,
            duration_ms = now.saturating_duration_since(self.started).as_secs_f64() * 1000.,
            local_drained = self.drained, peer_drained = self.peer_drained,
            commit_queued = self.commit_sent, drained_queued = self.drain_sent,
            drain_counts_known = self.counts_known, writers_pending = self.writers_pending, readers_pending = self.readers_pending,
            "Link migration step");
    }
}
fn path_trace(
    peer: DeviceId,
    path: &'static str,
    via: Option<DeviceId>,
    initiator: &'static str,
    reason: &'static str,
) {
    tracing::info!(target: "oink_link_diagnostics", kind = "path_transition", peer_id = peer.to_hex().as_str(),
        network_path = path, via_id = via.map(|p| p.to_hex()).unwrap_or_default().as_str(), initiator, reason,
        "Peer path changed");
}
impl Core {
    pub fn new(boot: Boot) -> Self {
        Self {
            boot,
            links: HashMap::new(),
            candidates: HashMap::new(),
            choices: HashMap::new(),
            dials: HashMap::new(),
            nominations: HashMap::new(),
            pending_relay: Vec::new(),
            transit: Vec::new(),
            counter: 0,
            effects: VecDeque::new(),
        }
    }
    fn number(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }
    fn arbiter(&self, peer: DeviceId) -> bool {
        self.boot.device_id < peer
    }
    pub fn peer_link(&self, peer: DeviceId) -> Option<LinkId> {
        self.links.values().find(|l| l.peer.device_id == peer).map(|l| l.id)
    }
    pub fn connection_boot(&self, id: ConnectionId) -> Option<Boot> {
        self.candidates.get(&id).map(|c| c.boot)
    }
    pub fn selected_connection_id(&self, id: ConnectionId) -> bool {
        self.candidates
            .get(&id)
            .is_some_and(|c| matches!(c.choice, Choice::Selected(_)))
    }
    fn selected_connection(&self, peer: DeviceId) -> Option<ConnectionId> {
        self.candidates
            .iter()
            .find(|(_, c)| c.boot.device_id == peer && matches!(c.choice, Choice::Selected(_)))
            .map(|(id, _)| *id)
    }
    fn ready_direct(&self, peer: DeviceId) -> Option<ConnectionId> {
        self.links
            .values()
            .find(|l| {
                l.peer.device_id == peer && l.installed && l.state == LinkState::Ready && l.path() == LinkPath::Direct
            })
            .map(|l| l.hop.connection_id)
    }
    fn send(&mut self, connection: ConnectionId, tag: u64, message: LinkMsg) {
        self.effects.push_back(Effect::Control {
            connection,
            tag,
            message: Box::new(message),
        });
    }
    fn path_send(&mut self, id: LinkId, message: LinkMsg) {
        if let Some(link) = self.links.get(&id) {
            self.send(link.hop.connection_id, link.hop.route_tag, message);
        }
    }
    pub fn dial(
        &mut self,
        peer: DeviceId,
        addresses: Vec<SocketAddr>,
        mode: DialMode,
        now: Instant,
    ) -> Result<(), DialRefused> {
        if self.peer_link(peer).is_some() {
            return Err(DialRefused::AlreadyLinked);
        }
        if self.dials.contains_key(&peer) || self.dials.len() >= MAX_DIALS {
            return Err(DialRefused::Busy);
        }
        path_trace(peer, "establishing", None, "local", "dial");
        let direct = !addresses.is_empty();
        self.dials.insert(
            peer,
            Dial {
                mode,
                addresses: addresses.clone(),
                direct,
                deadline: now + DIAL_BUDGET,
                request: None,
            },
        );
        if direct {
            self.effects.push_back(Effect::Dial { peer, addresses });
        }
        if mode == DialMode::All || !direct && mode == DialMode::PreferDirect {
            self.request_relay(peer, now);
        }
        if !direct && mode == DialMode::DirectOnly {
            self.fail_dial(peer, DialFailure::Unreachable);
        }
        Ok(())
    }
    fn fail_dial(&mut self, peer: DeviceId, reason: DialFailure) {
        if let Some(dial) = self.dials.remove(&peer)
            && self.peer_link(peer).is_none()
        {
            super::timing::record(
                super::timing::Measurement::Setup(super::timing::Result::Failed),
                dial.deadline - DIAL_BUDGET,
                Instant::now(),
            );
            path_trace(peer, "offline", None, "local", "dial_failed");
            self.effects.push_back(Effect::DialFailed(peer, reason));
        }
    }
    pub fn dial_identity_mismatch(&mut self, peer: DeviceId, now: Instant) {
        if self
            .dials
            .get(&peer)
            .is_some_and(|dial| dial.mode == DialMode::DirectOnly)
        {
            self.fail_dial(peer, DialFailure::IdentityChanged);
        } else {
            self.dial_done(peer, now);
        }
    }
    pub fn dial_done(&mut self, peer: DeviceId, now: Instant) {
        if let Some(id) = self.peer_link(peer) {
            if self.dials.remove(&peer).is_none() {
                return;
            }
            let link = self.links.get_mut(&id).expect("link");
            link.next_direct = now + DIRECT_RETRY[(link.direct_failures + 1).min(DIRECT_RETRY.len() - 1)];
            link.direct_failures += 1;
            self.effects.push_back(Effect::DialFinished(peer));
            return;
        }
        let Some(dial) = self.dials.get_mut(&peer) else {
            return;
        };
        dial.direct = false;
        if self.candidates.values().any(|c| c.boot.device_id == peer) {
            return;
        }
        match dial.mode {
            DialMode::DirectOnly => self.fail_dial(peer, DialFailure::Unreachable),
            DialMode::PreferDirect => self.request_relay(peer, now),
            DialMode::All => {
                if dial.request.is_none() {
                    self.fail_dial(peer, DialFailure::Unreachable);
                }
            }
        }
    }
    pub fn authenticated(&mut self, boot: Boot, connection: ConnectionId, now: Instant) {
        if boot.device_id == self.boot.device_id || self.candidates.contains_key(&connection) {
            return;
        }
        self.observe_peer_boot(boot, now);
        if self.selected_connection(boot.device_id).is_some() {
            tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", peer_id = boot.device_id.to_hex().as_str(), operation = diagnostics::hex(&connection.random).as_str(), stage = "selection", result = "failed", reason = "superseded", "Connection rejected");
            self.send(
                connection,
                0,
                LinkMsg::ConnectionReject {
                    id: connection,
                    reason: Refusal::Superseded,
                },
            );
            self.effects.push_back(Effect::CloseConnection(connection));
            return;
        }
        let arrival = self.number();
        self.candidates.insert(
            connection,
            Candidate {
                boot,
                arrival,
                started: now,
                choice: Choice::Candidate,
                deadline: now + DIAL_BUDGET,
            },
        );
        tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", peer_id = boot.device_id.to_hex().as_str(),
            operation = diagnostics::hex(&connection.random).as_str(), stage = "instance", result = "completed",
            initiator = if connection.initiator_boot == self.boot { "local" } else { "peer" }, reason = "none", "Connection instance authenticated");
        self.selection_trace(connection, "entered", "authenticated", 0);
        self.offer_candidate(boot.device_id);
    }
    fn observe_peer_boot(&mut self, boot: Boot, now: Instant) {
        // The watermark survives connection/path loss, so its boot must survive
        // too. A restart can arrive after all old candidates have disappeared.
        let choice = self.choices.entry(boot.device_id).or_insert((boot, 0));
        if choice.0 != boot {
            tracing::info!(target: "oink_link_diagnostics", kind = "connection_step",
                peer_id = boot.device_id.to_hex().as_str(), stage = "selection", result = "entered",
                reason = "peer_restarted", choice_watermark = choice.1,
                peer_boot = diagnostics::hex(&boot.nonce).as_str(), "Selection watermark reset for peer boot");
            *choice = (boot, 0);
        }
        if let Some(id) = self.peer_link(boot.device_id)
            && self.links[&id].peer != boot
        {
            self.lose(id, LinkLost::PeerRestarted);
        }
        let stale: Vec<_> = self
            .candidates
            .iter()
            .filter(|(_, c)| c.boot.device_id == boot.device_id && c.boot != boot)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.connection_lost(id, now);
            self.effects.push_back(Effect::CloseConnection(id));
        }
    }
    pub(super) fn selection_wait(&self, id: ConnectionId) -> (&'static str, &'static str) {
        match self.candidates.get(&id).map(|c| c.choice) {
            Some(Choice::Offered(_)) => ("connection_selected", "peer"),
            Some(Choice::Selected(_)) => ("none", "none"),
            Some(Choice::Candidate) if self.arbiter(self.candidates[&id].boot.device_id) => {
                let waiting =
                    if self.candidates.values().any(|other| {
                        other.boot == self.candidates[&id].boot && matches!(other.choice, Choice::Offered(_))
                    }) {
                        "other_candidate"
                    } else {
                        "local_selection"
                    };
                (waiting, "local")
            }
            Some(Choice::Candidate) => ("connection_select", "peer"),
            None => ("instance", "peer"),
        }
    }
    fn selection_trace(&self, id: ConnectionId, result: &'static str, reason: &'static str, counter: u64) {
        let Some(candidate) = self.candidates.get(&id) else {
            return;
        };
        let (waiting, waiting_on) = self.selection_wait(id);
        let counter = match (counter, candidate.choice) {
            (0, Choice::Offered(value) | Choice::Selected(value)) => value,
            _ => counter,
        };
        tracing::info!(target: "oink_link_diagnostics", kind = "connection_step",
            peer_id = candidate.boot.device_id.to_hex().as_str(), operation = diagnostics::hex(&id.random).as_str(),
            stage = "selection", result, reason, waiting, waiting_on,
            elapsed_ms = Instant::now().saturating_duration_since(candidate.started).as_secs_f64() * 1000.,
            arbiter = if self.arbiter(candidate.boot.device_id) { "local" } else { "peer" },
            choice_counter = counter, choice_watermark = self.choices.get(&candidate.boot.device_id).map_or(0, |(_, counter)| *counter),
            local_boot = diagnostics::hex(&self.boot.nonce).as_str(), peer_boot = diagnostics::hex(&candidate.boot.nonce).as_str(),
            "Connection selection transition");
    }
    fn offer_candidate(&mut self, peer: DeviceId) {
        if !self.arbiter(peer)
            || self
                .candidates
                .values()
                .any(|c| c.boot.device_id == peer && c.choice != Choice::Candidate)
        {
            return;
        }
        let Some((&id, candidate)) = self
            .candidates
            .iter()
            .filter(|(_, c)| c.boot.device_id == peer)
            .min_by_key(|(_, c)| c.arrival)
        else {
            return;
        };
        let pair = BootPair::new(self.boot, candidate.boot).expect("distinct peers");
        let counter = &mut self.choices.get_mut(&peer).expect("observed peer boot").1;
        *counter += 1;
        let counter = *counter;
        self.candidates.get_mut(&id).expect("candidate").choice = Choice::Offered(counter);
        self.selection_trace(id, "entered", "offer", counter);
        self.send(
            id,
            0,
            LinkMsg::ConnectionSelect {
                boot_pair: pair,
                connection_id: id,
                choice_counter: counter,
            },
        );
    }
    fn selected(&mut self, id: ConnectionId, counter: u64, now: Instant) {
        let Some(candidate) = self.candidates.get_mut(&id) else {
            return;
        };
        super::timing::record(
            super::timing::Measurement::Stage(super::timing::Stage::Selection),
            candidate.started,
            now,
        );
        candidate.choice = Choice::Selected(counter);
        let peer = candidate.boot;
        tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", peer_id = peer.device_id.to_hex().as_str(),
            operation = diagnostics::hex(&id.random).as_str(), stage = "selection", result = "completed", reason = "selected", elapsed_ms = now.saturating_duration_since(candidate.started).as_secs_f64() * 1000., "Connection selected");
        self.choices.insert(peer.device_id, (peer, counter));
        let losers: Vec<_> = self
            .candidates
            .iter()
            .filter(|(cid, c)| **cid != id && c.boot.device_id == peer.device_id)
            .map(|(cid, _)| *cid)
            .collect();
        for loser in losers {
            tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", peer_id = peer.device_id.to_hex().as_str(),
                operation = diagnostics::hex(&loser.random).as_str(), stage = "selection", result = "failed", reason = "superseded", "Connection rejected");
            self.candidates.remove(&loser);
            self.effects.push_back(Effect::CloseConnection(loser));
        }
        if self.arbiter(peer.device_id) {
            if let Some(link) = self.peer_link(peer.device_id) {
                self.begin_cutover(link, id, now);
            } else {
                self.install_direct(peer, id, now);
            }
        }
    }
    fn install_direct(&mut self, peer: Boot, connection: ConnectionId, now: Instant) {
        let wire = PathInstall {
            request_id: PathRequestId {
                source_boot: self.boot,
                random: rand::random(),
            },
            path_id: PathId {
                boot_pair: BootPair::new(self.boot, peer).expect("pair"),
                coordinator_counter: self.number(),
            },
            route_revision: RouteRevision(0),
            reservation: None,
            next_hop_binding: NextHopBinding {
                peer: self.boot.device_id,
                connection_id: connection,
                route_tag: self.number(),
            },
        };
        let hop = NextHopBinding {
            peer: peer.device_id,
            ..wire.next_hop_binding
        };
        self.new_link(peer, wire, hop, false, now);
        self.send(connection, 0, LinkMsg::PathInstall(wire));
    }
    fn new_link(
        &mut self,
        peer: Boot,
        wire: PathInstall,
        hop: NextHopBinding,
        installed: bool,
        now: Instant,
    ) -> LinkId {
        let id = LinkId(self.number());
        let direction = if wire.reservation.is_some() {
            if self.arbiter(peer.device_id) {
                Direction::Outbound
            } else {
                Direction::Inbound
            }
        } else if hop.connection_id.initiator_boot == self.boot {
            Direction::Outbound
        } else {
            Direction::Inbound
        };
        let addresses = self
            .dials
            .get(&peer.device_id)
            .map_or_else(Vec::new, |d| d.addresses.clone());
        if !self.dials.contains_key(&peer.device_id) {
            path_trace(peer.device_id, "establishing", None, "peer", "accepted");
        }
        self.links.insert(
            id,
            Link {
                id,
                peer,
                state: LinkState::Connected,
                wire,
                hop,
                direction,
                installed,
                installation_deadline: now + DIAL_BUDGET,
                report_setup_started: self
                    .dials
                    .get(&peer.device_id)
                    .map_or(now, |dial| dial.deadline - DIAL_BUDGET),
                report_config_started: None,
                heartbeat: Heartbeat::new(now),
                cutover: None,
                addresses,
                next_direct: now + DIRECT_RETRY[0],
                direct_failures: 0,
            },
        );
        tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", peer_id = peer.device_id.to_hex().as_str(), operation = diagnostics::hex(&hop.connection_id.random).as_str(), stage = "path_install", result = if installed { "completed" } else { "entered" }, reason = "none", path_counter = wire.path_id.coordinator_counter, "Path installation");
        if installed {
            self.connected(id);
        }
        id
    }
    fn connected(&mut self, id: LinkId) {
        let peer = self.links[&id].peer.device_id;
        let direct = self.links[&id].path() == LinkPath::Direct;
        if direct {
            self.cancel_request(peer);
            self.dials.remove(&peer);
            self.effects.push_back(Effect::DialFinished(peer));
        } else if self.dials.get(&peer).is_some_and(|d| !d.direct) {
            self.dials.remove(&peer);
            self.effects.push_back(Effect::DialFinished(peer));
        }
        let link = self.links.get_mut(&id).expect("connected link");
        let report_now = Instant::now();
        super::timing::record(
            super::timing::Measurement::Stage(super::timing::Stage::PathInstall),
            link.installation_deadline - DIAL_BUDGET,
            report_now,
        );
        link.report_config_started = Some(report_now);
        tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", peer_id = peer.to_hex().as_str(), operation = diagnostics::hex(&link.hop.connection_id.random).as_str(), stage = "path_install", result = "completed", reason = "none", elapsed_ms = Instant::now().saturating_duration_since(link.installation_deadline - DIAL_BUDGET).as_secs_f64() * 1000., "Path installed");
        tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", peer_id = peer.to_hex().as_str(), operation = diagnostics::hex(&link.hop.connection_id.random).as_str(), stage = "config_ready", result = "entered", reason = "none", "Waiting for config handshake");
        self.effects.push_back(Effect::Connected(id));
    }
    pub fn set_half_open(&mut self, id: LinkId) {
        if let Some(link) = self.links.get_mut(&id)
            && link.state == LinkState::Connected
        {
            link.state = LinkState::HalfOpen;
        }
    }
    pub fn set_ready(&mut self, id: LinkId) {
        let mut direct = None;
        let mut relay = None;
        if let Some(link) = self.links.get_mut(&id)
            && link.state != LinkState::Ready
        {
            path_trace(
                link.peer.device_id,
                if link.wire.reservation.is_some() {
                    "relay"
                } else {
                    "direct"
                },
                link.wire.reservation.map(|_| link.hop.peer),
                if link.direction == Direction::Outbound {
                    "local"
                } else {
                    "peer"
                },
                "ready",
            );
            tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", peer_id = link.peer.device_id.to_hex().as_str(), operation = diagnostics::hex(&link.hop.connection_id.random).as_str(), stage = "config_ready", result = "completed", reason = "ready", elapsed_ms = Instant::now().saturating_duration_since(link.installation_deadline - DIAL_BUDGET).as_secs_f64() * 1000., "Config handshake completed");
            let report_now = Instant::now();
            super::timing::record(
                super::timing::Measurement::Setup(super::timing::Result::Ready),
                link.report_setup_started,
                report_now,
            );
            if let Some(started) = link.report_config_started {
                super::timing::record(
                    super::timing::Measurement::Stage(super::timing::Stage::ConfigReady),
                    started,
                    report_now,
                );
            }
            link.state = LinkState::Ready;
            if link.path() == LinkPath::Direct {
                direct = Some(link.hop.connection_id);
            } else {
                relay = Some(link.peer.device_id);
            }
            self.effects.push_back(Effect::Ready(id));
        }
        if let Some(connection) = direct {
            self.relay_neighbor_ready(connection, Instant::now());
        }
        if let Some(peer) = relay
            && self.arbiter(peer)
            && let Some(connection) = self.selected_connection(peer)
        {
            self.begin_cutover(id, connection, Instant::now());
        }
    }
    pub fn inbound(&mut self, id: LinkId) -> Result<(), oink_protocol::frame::StreamCode> {
        match self.links.get(&id).map(|l| l.state) {
            Some(LinkState::Ready) => Ok(()),
            _ => Err(StreamCode::NotReady),
        }
    }
    pub fn retire_authority(&mut self, id: LinkId) {
        let Some(link) = self.links.get_mut(&id) else { return };
        link.state = LinkState::Connected;
        let connection = link.hop.connection_id;
        if link.path() == LinkPath::Direct {
            self.retire_relay_authority(connection);
        }
    }
    pub fn lose(&mut self, id: LinkId, reason: LinkLost) {
        let Some(link) = self.links.remove(&id) else {
            return;
        };
        let reason_label = match reason {
            LinkLost::CutoverTimeout => "cutover_timeout",
            LinkLost::ConnectionLost => "connection_lost",
            LinkLost::HeartbeatTimeout => "heartbeat_timeout",
            LinkLost::PeerRestarted => "peer_restarted",
            LinkLost::RelayLost => "relay_lost",
            LinkLost::Stopped => "stopped",
            LinkLost::Local(why) => match why {
                Disconnect::MemberRemoved => "member_removed",
                Disconnect::LeavingGroup => "leaving_group",
                Disconnect::Sleeping => "sleeping",
                Disconnect::Stopping => "stopped",
                Disconnect::HandshakeFailed => "handshake_failed",
                Disconnect::HandshakeTimeout => "handshake_timeout",
                Disconnect::ControlStreamFailed => "control_stream_failed",
                Disconnect::InputOpeningTimeout => "input_opening_timeout",
            },
        };
        if !link.installed || link.state != LinkState::Ready {
            let result = match reason {
                LinkLost::Stopped
                | LinkLost::Local(Disconnect::Stopping | Disconnect::LeavingGroup | Disconnect::Sleeping) => {
                    super::timing::Result::Cancelled
                }
                LinkLost::Local(Disconnect::HandshakeTimeout) => super::timing::Result::Timeout,
                _ => super::timing::Result::Failed,
            };
            super::timing::record(
                super::timing::Measurement::Setup(result),
                link.report_setup_started,
                Instant::now(),
            );
            tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", peer_id = link.peer.device_id.to_hex().as_str(), operation = diagnostics::hex(&link.hop.connection_id.random).as_str(), stage = if link.installed { "config_ready" } else { "path_install" }, result = "failed", reason = reason_label, "Connection establishment failed");
        }
        if let Some(m) = &link.cutover {
            m.trace(
                link.peer.device_id,
                self.arbiter(link.peer.device_id),
                m.waiting(self.arbiter(link.peer.device_id)),
                "failed",
                reason_label,
                Instant::now(),
            );
        }
        path_trace(
            link.peer.device_id,
            "offline",
            None,
            match reason {
                LinkLost::Local(_) | LinkLost::Stopped => "local",
                LinkLost::PeerRestarted => "peer",
                _ => "unknown",
            },
            reason_label,
        );
        let dialing = self.dials.remove(&link.peer.device_id).is_some();
        if dialing && !link.installed {
            self.effects
                .push_back(Effect::DialFailed(link.peer.device_id, DialFailure::Unreachable));
        }
        self.nominations.remove(&link.peer.device_id);
        self.path_down(&link);
        if link.wire.reservation.is_none() {
            self.candidates.remove(&link.hop.connection_id);
            self.effects.push_back(Effect::CloseConnection(link.hop.connection_id));
            self.relay_connection_lost(link.hop.connection_id);
            let dependent: Vec<_> = self
                .links
                .values()
                .filter(|l| l.hop.connection_id == link.hop.connection_id)
                .map(|l| l.id)
                .collect();
            for dependent in dependent {
                self.lose(dependent, LinkLost::RelayLost);
            }
        }
        if let Some(cutover) = link.cutover {
            self.candidates.remove(&cutover.next_hop.connection_id);
            self.effects
                .push_back(Effect::CloseConnection(cutover.next_hop.connection_id));
        }
        if link.installed {
            self.effects.push_back(Effect::Lost {
                link: id,
                peer: link.peer.device_id,
                path: if link.wire.reservation.is_some() {
                    LinkPath::Relayed { via: link.hop.peer }
                } else {
                    LinkPath::Direct
                },
                reason,
            });
        }
    }
    fn path_down(&mut self, link: &Link) {
        if link.wire.reservation.is_some() {
            self.send(
                link.hop.connection_id,
                link.hop.route_tag,
                LinkMsg::PathDown {
                    path_id: link.wire.path_id,
                },
            );
        }
    }
    pub fn connection_lost(&mut self, connection: ConnectionId, _now: Instant) {
        self.relay_connection_lost(connection);
        let lost: Vec<_> = self
            .links
            .values()
            .filter(|l| {
                l.hop.connection_id == connection
                    || l.cutover
                        .as_ref()
                        .is_some_and(|m| m.next_hop.connection_id == connection)
            })
            .map(|l| l.id)
            .collect();
        for id in lost {
            self.lose(id, LinkLost::ConnectionLost);
        }
        if let Some(candidate) = self.candidates.remove(&connection) {
            self.offer_candidate(candidate.boot.device_id);
        }
    }
    pub fn path_internal_closed(&mut self, connection: ConnectionId, tag: u64) {
        let Some(id) = self.route(connection, tag) else {
            return;
        };
        let link = &self.links[&id];
        // Once M has sent the commit, both sides have drained the old path.
        // F retires that path after queuing its confirmation on the new one;
        // old N0 teardown can arrive first across these independent connections.
        // Keep waiting for that confirmation under the existing cutover deadline.
        if link.hop.connection_id == connection
            && link.hop.route_tag == tag
            && link.cutover.as_ref().is_some_and(|m| m.commit_sent)
        {
            return;
        }
        self.lose(id, LinkLost::ConnectionLost);
    }
    pub fn update_addresses(&mut self, peer: DeviceId, addresses: Vec<SocketAddr>, now: Instant) {
        // Discovery can learn a direct route while a relay search is still
        // pending. Add only unseen candidates to that attempt; do not discard
        // the hint until failure/backoff or restart its deadline.
        if let Some(dial) = self.dials.get_mut(&peer)
            && now < dial.deadline
        {
            let mut fresh = Vec::new();
            for address in &addresses {
                if !dial.addresses.contains(address) {
                    dial.addresses.push(*address);
                    fresh.push(*address);
                }
            }
            if !fresh.is_empty() {
                dial.direct = true;
                self.effects.push_back(Effect::Dial { peer, addresses: fresh });
            }
        }
        if let Some(id) = self.peer_link(peer) {
            let link = self.links.get_mut(&id).expect("link");
            if link.addresses == addresses {
                return;
            }
            link.addresses = addresses;
            link.next_direct = now;
            self.retry_direct(now);
        }
    }
    fn retry_direct(&mut self, now: Instant) {
        let due: Vec<_> = self
            .links
            .values()
            .filter(|l| {
                l.installed
                    && l.wire.reservation.is_some()
                    && l.cutover.is_none()
                    && now >= l.next_direct
                    && !self.dials.contains_key(&l.peer.device_id)
                    && !l.addresses.is_empty()
            })
            .map(|l| (l.peer.device_id, l.addresses.clone()))
            .collect();
        for (peer, addresses) in due {
            if self.dials.len() >= MAX_DIALS {
                break;
            }
            self.dials.insert(
                peer,
                Dial {
                    mode: DialMode::DirectOnly,
                    addresses: addresses.clone(),
                    direct: true,
                    deadline: now + DIAL_BUDGET,
                    request: None,
                },
            );
            self.effects.push_back(Effect::Dial { peer, addresses });
        }
    }
    pub fn route(&self, connection: ConnectionId, tag: u64) -> Option<LinkId> {
        self.links
            .values()
            .find(|l| {
                l.hop.connection_id == connection && l.hop.route_tag == tag
                    || l.cutover.as_ref().is_some_and(|m| {
                        m.commit_sent && m.next_hop.connection_id == connection && m.next_hop.route_tag == tag
                    })
            })
            .map(|l| l.id)
    }
    fn migration_control_trace(
        &self,
        wire: Migration,
        stage: &'static str,
        result: &'static str,
        reason: &'static str,
    ) {
        if let Some(peer) = wire.old_path.boot_pair.other(self.boot) {
            tracing::info!(target: "oink_link_diagnostics", kind = "migration_step", peer_id = peer.device_id.to_hex().as_str(),
                operation = diagnostics::hex(&wire.new_connection.random).as_str(), path_counter = wire.old_path.coordinator_counter,
                revision = wire.migration_id.to_revision.0, stage, result, reason, "Migration control observed");
        }
    }
    pub fn receive(&mut self, connection: ConnectionId, tag: u64, message: LinkMsg, now: Instant) {
        if self.relay_control(connection, tag, &message, now) {
            return;
        }
        let Some(boot) = self.connection_boot(connection) else {
            return;
        };
        let migration = match &message {
            LinkMsg::MigrateBegin {
                path_id,
                connection_id,
                migration_id,
                ..
            } => Some((
                Migration {
                    migration_id: *migration_id,
                    old_path: *path_id,
                    new_connection: *connection_id,
                },
                "begin_received",
            )),
            LinkMsg::MigrateDrained(wire) => Some((*wire, "peer_drained")),
            LinkMsg::MigrateCommit(wire) => Some((*wire, "peer_commit")),
            LinkMsg::MigrateCommitted(wire) => Some((*wire, "peer_committed")),
            _ => None,
        };
        if let Some((wire, stage)) = migration {
            self.migration_control_trace(wire, stage, "received", "none");
        }
        match message {
            LinkMsg::ConnectionSelect {
                boot_pair,
                connection_id,
                choice_counter,
            } => {
                let reason = if tag != 0 {
                    "wrong_route"
                } else if self.arbiter(boot.device_id) {
                    "wrong_role"
                } else if connection_id != connection {
                    "connection_mismatch"
                } else if BootPair::new(self.boot, boot) != Some(boot_pair) {
                    "boot_mismatch"
                } else if choice_counter == 0 {
                    "zero_counter"
                } else if choice_counter < self.choices.get(&boot.device_id).map_or(0, |(_, counter)| *counter) {
                    "stale_counter"
                } else if self
                    .selected_connection(boot.device_id)
                    .is_some_and(|c| c != connection)
                {
                    "superseded"
                } else {
                    "accepted"
                };
                self.selection_trace(connection, "received", reason, choice_counter);
                if reason != "accepted" {
                    return;
                }
                self.send(
                    connection,
                    0,
                    LinkMsg::ConnectionSelected {
                        boot_pair,
                        connection_id,
                        choice_counter,
                    },
                );
                self.selection_trace(connection, "entered", "selected_reply", choice_counter);
                self.selected(connection, choice_counter, now);
            }
            LinkMsg::ConnectionSelected {
                boot_pair,
                connection_id,
                choice_counter,
            } => {
                let accepted = tag == 0
                    && self.arbiter(boot.device_id)
                    && connection_id == connection
                    && BootPair::new(self.boot, boot) == Some(boot_pair)
                    && self.candidates[&connection].choice == Choice::Offered(choice_counter);
                self.selection_trace(
                    connection,
                    "received",
                    if accepted { "accepted" } else { "unexpected_reply" },
                    choice_counter,
                );
                if accepted {
                    self.selected(connection, choice_counter, now);
                }
            }
            LinkMsg::ConnectionReject { id, .. } if id == connection => {
                self.connection_lost(connection, now);
                self.effects.push_back(Effect::CloseConnection(connection));
            }
            LinkMsg::PathInstall(wire) => self.accept_install(connection, wire, now),
            LinkMsg::PathInstalled(wire) => {
                if let Some(id) = self
                    .links
                    .values()
                    .find(|l| !l.installed && l.wire == wire && l.hop.connection_id == connection)
                    .map(|l| l.id)
                {
                    let link = self.links.get_mut(&id).expect("link");
                    link.installed = true;
                    // Pulses can only start on an installed path. Time spent
                    // awaiting Installed belongs to the installation budget.
                    link.heartbeat = Heartbeat::new(now);
                    self.connected(id);
                    // Selection can finish while the relay's install reply is
                    // in flight. Reconcile on either transition, not just Select.
                    let peer = self.links[&id].peer.device_id;
                    if let Some(direct) = self.selected_connection(peer) {
                        self.begin_cutover(id, direct, now);
                    }
                }
            }
            LinkMsg::Pulse(pulse) | LinkMsg::Pong(pulse) => {
                let pong = matches!(message, LinkMsg::Pong(_));
                let Some(id) = self.route(connection, tag) else {
                    return;
                };
                let link = self.links.get_mut(&id).expect("link");
                if pulse.path_id != link.wire.path_id || pulse.route_revision != link.wire.route_revision {
                    return;
                }
                if pong {
                    if let Some(index) = link
                        .heartbeat
                        .pending
                        .iter()
                        .position(|(nonce, _)| *nonce == pulse.nonce)
                    {
                        let (_, sent) = link.heartbeat.pending[index];
                        link.heartbeat.pending.drain(..=index);
                        link.heartbeat.progress = now;
                        link.heartbeat.rtt = Some(now.saturating_duration_since(sent));
                    }
                } else {
                    self.path_send(id, LinkMsg::Pong(pulse));
                }
            }
            LinkMsg::PathDown { path_id } => {
                if let Some(id) = self.route(connection, tag)
                    && self.links[&id].wire.path_id == path_id
                {
                    self.lose(id, LinkLost::RelayLost);
                }
            }
            LinkMsg::MigrateBegin {
                path_id,
                from_revision,
                to_revision,
                connection_id,
                migration_id,
            } => {
                let observation = Migration {
                    migration_id,
                    old_path: path_id,
                    new_connection: connection_id,
                };
                let Some(id) = self.route(connection, tag) else {
                    self.migration_control_trace(observation, "begin_received", "failed", "route_missing");
                    return;
                };
                let link = &self.links[&id];
                if self.arbiter(link.peer.device_id)
                    || link.wire.path_id != path_id
                    || link.wire.route_revision != from_revision
                    || from_revision.next() != Some(to_revision)
                    || link.cutover.is_some()
                    || self.selected_connection(link.peer.device_id) != Some(connection_id)
                {
                    let reason = if self.arbiter(link.peer.device_id) {
                        "wrong_role"
                    } else if link.wire.path_id != path_id {
                        "path_mismatch"
                    } else if link.wire.route_revision != from_revision || from_revision.next() != Some(to_revision) {
                        "revision_mismatch"
                    } else if link.cutover.is_some() {
                        "already_migrating"
                    } else {
                        "not_selected"
                    };
                    self.migration_control_trace(observation, "begin_received", "failed", reason);
                    return;
                }
                let next_hop = NextHopBinding {
                    peer: link.peer.device_id,
                    connection_id,
                    route_tag: link.hop.route_tag,
                };
                self.links.get_mut(&id).expect("link").cutover = Some(Cutover {
                    wire: Migration {
                        migration_id,
                        old_path: path_id,
                        new_connection: connection_id,
                    },
                    next_hop,
                    deadline: now + CUTOVER_TIMEOUT,
                    drained: false,
                    peer_drained: false,
                    commit_sent: false,
                    drain_sent: false,
                    started: now,
                    stage_started: now,
                    counts_known: false,
                    writers_pending: 0,
                    readers_pending: 0,
                });
                let link = &self.links[&id];
                let m = link.cutover.as_ref().expect("cutover");
                m.trace(link.peer.device_id, false, "begin_received", "completed", "none", now);
                m.trace(link.peer.device_id, false, "local_drain", "entered", "none", now);
                self.effects.push_back(Effect::Freeze(id));
            }
            LinkMsg::MigrateDrained(wire) => {
                if let Some(id) = self.route(connection, tag)
                    && let Some(m) = self
                        .links
                        .get_mut(&id)
                        .and_then(|l| l.cutover.as_mut())
                        .filter(|m| m.wire == wire)
                {
                    if !m.peer_drained {
                        m.peer_drained = true;
                        m.trace(boot.device_id, true, "peer_drained", "completed", "none", now);
                    }
                    self.advance_cutover(id);
                } else {
                    self.migration_control_trace(wire, "peer_drained", "failed", "migration_mismatch");
                }
            }
            LinkMsg::MigrateCommit(wire) => {
                let id = self
                    .links
                    .values()
                    .find(|l| {
                        l.cutover
                            .as_ref()
                            .is_some_and(|m| m.wire == wire && m.next_hop.connection_id == connection && m.drained)
                    })
                    .map(|l| l.id);
                if let Some(id) = id {
                    let link = &self.links[&id];
                    link.cutover.as_ref().expect("cutover").trace(
                        link.peer.device_id,
                        false,
                        "peer_commit",
                        "completed",
                        "none",
                        now,
                    );
                    link.cutover.as_ref().expect("cutover").trace(
                        link.peer.device_id,
                        false,
                        "committed_queued",
                        "completed",
                        "none",
                        now,
                    );
                    self.send(connection, 0, LinkMsg::MigrateCommitted(wire));
                    self.finish_cutover(id, now);
                } else {
                    self.migration_control_trace(wire, "peer_commit", "failed", "migration_mismatch");
                }
            }
            LinkMsg::MigrateCommitted(wire) => {
                let id = self
                    .links
                    .values()
                    .find(|l| {
                        l.cutover
                            .as_ref()
                            .is_some_and(|m| m.wire == wire && m.next_hop.connection_id == connection && m.commit_sent)
                    })
                    .map(|l| l.id);
                if let Some(id) = id {
                    let link = &self.links[&id];
                    link.cutover.as_ref().expect("cutover").trace(
                        link.peer.device_id,
                        true,
                        "peer_committed",
                        "completed",
                        "none",
                        now,
                    );
                    self.finish_cutover(id, now);
                } else {
                    self.migration_control_trace(wire, "peer_committed", "failed", "migration_mismatch");
                }
            }
            LinkMsg::HopProbe { nonce } => self.send(connection, 0, LinkMsg::HopPong { nonce }),
            _ => {}
        }
    }
    fn accept_install(&mut self, connection: ConnectionId, wire: PathInstall, now: Instant) {
        let Some(peer) = wire.path_id.boot_pair.other(self.boot) else {
            return;
        };
        if self.arbiter(peer.device_id)
            || !wire.path_id.valid()
            || wire.next_hop_binding.connection_id != connection
            || wire.next_hop_binding.route_tag == 0
        {
            return;
        }
        let valid = if let Some(reservation) = wire.reservation {
            self.nominations.get(&peer.device_id).is_some_and(|n| {
                n.request.id == wire.request_id
                    && n.reservation == reservation
                    && now < n.request.deadline
                    && self.connection_boot(connection) == Some(reservation.relay)
            })
        } else {
            self.selected_connection(peer.device_id) == Some(connection)
                && self.connection_boot(connection) == Some(peer)
        };
        if !valid || self.peer_link(peer.device_id).is_some() {
            return;
        }
        self.observe_peer_boot(peer, now);
        self.send(connection, 0, LinkMsg::PathInstalled(wire));
        self.new_link(peer, wire, wire.next_hop_binding, true, now);
    }
    fn begin_cutover(&mut self, id: LinkId, connection: ConnectionId, now: Instant) {
        let link = &self.links[&id];
        if !link.installed {
            tracing::info!(target: "oink_link_diagnostics", kind = "connection_step",
                peer_id = link.peer.device_id.to_hex().as_str(), operation = diagnostics::hex(&connection.random).as_str(),
                stage = "path_install", result = "entered", reason = "none", waiting = "path_install", waiting_on = "peer",
                path_counter = link.wire.path_id.coordinator_counter, "Selected direct candidate waits for path installation");
            return;
        }
        if link.wire.reservation.is_none() || link.cutover.is_some() {
            return;
        }
        // C0 must finish admission before migration can freeze its writer.
        // Half-open business readers wait for Ready and cannot drain yet.
        if link.state != LinkState::Ready {
            return;
        }
        let to_revision = link.wire.route_revision.next().expect("route revision");
        let migration_id = MigrationId {
            path_id: link.wire.path_id,
            to_revision,
        };
        let message = LinkMsg::MigrateBegin {
            path_id: link.wire.path_id,
            from_revision: link.wire.route_revision,
            to_revision,
            connection_id: connection,
            migration_id,
        };
        let next_hop = NextHopBinding {
            peer: link.peer.device_id,
            connection_id: connection,
            route_tag: link.hop.route_tag,
        };
        let wire = Migration {
            migration_id,
            old_path: link.wire.path_id,
            new_connection: connection,
        };
        self.links.get_mut(&id).expect("link").cutover = Some(Cutover {
            wire,
            next_hop,
            deadline: now + CUTOVER_TIMEOUT,
            drained: false,
            peer_drained: false,
            commit_sent: false,
            drain_sent: false,
            started: now,
            stage_started: now,
            counts_known: false,
            writers_pending: 0,
            readers_pending: 0,
        });
        self.path_send(id, message);
        let link = &self.links[&id];
        let m = link.cutover.as_ref().expect("cutover");
        m.trace(link.peer.device_id, true, "begin_queued", "completed", "none", now);
        m.trace(link.peer.device_id, true, "local_drain", "entered", "none", now);
        self.effects.push_back(Effect::Freeze(id));
    }
    pub fn drain_counts(&mut self, id: LinkId, writers: usize, readers: usize) {
        if let Some(m) = self.links.get_mut(&id).and_then(|l| l.cutover.as_mut()) {
            m.counts_known = true;
            m.writers_pending = writers;
            m.readers_pending = readers;
        }
    }
    pub fn drained(&mut self, id: LinkId) {
        let Some(peer) = self.links.get(&id).map(|l| l.peer.device_id) else {
            return;
        };
        let coordinator = self.arbiter(peer);
        if let Some(m) = self.links.get_mut(&id).and_then(|l| l.cutover.as_mut()) {
            if m.drained {
                return;
            }
            m.drained = true;
            m.trace(peer, coordinator, "local_drain", "completed", "none", Instant::now());
            self.advance_cutover(id);
        }
    }
    fn advance_cutover(&mut self, id: LinkId) {
        let Some(link) = self.links.get_mut(&id) else {
            return;
        };
        let Some(m) = link.cutover.as_mut() else {
            return;
        };
        if !m.drained || m.commit_sent {
            return;
        }
        let wire = m.wire;
        if self.boot.device_id < link.peer.device_id {
            if !m.peer_drained {
                m.stage_started = Instant::now();
                m.trace(
                    link.peer.device_id,
                    true,
                    "peer_drained",
                    "entered",
                    "none",
                    Instant::now(),
                );
                return;
            }
            m.commit_sent = true;
            m.stage_started = Instant::now();
            m.trace(
                link.peer.device_id,
                true,
                "commit_queued",
                "completed",
                "none",
                Instant::now(),
            );
            m.trace(
                link.peer.device_id,
                true,
                "peer_committed",
                "entered",
                "none",
                Instant::now(),
            );
            let connection = m.next_hop.connection_id;
            self.send(connection, 0, LinkMsg::MigrateCommit(wire));
        } else if !m.drain_sent {
            m.drain_sent = true;
            m.stage_started = Instant::now();
            m.trace(
                link.peer.device_id,
                false,
                "drained_queued",
                "completed",
                "none",
                Instant::now(),
            );
            m.trace(
                link.peer.device_id,
                false,
                "peer_commit",
                "entered",
                "none",
                Instant::now(),
            );
            self.path_send(id, LinkMsg::MigrateDrained(wire));
        }
    }
    fn finish_cutover(&mut self, id: LinkId, now: Instant) {
        let link = self.links.get_mut(&id).expect("link");
        let Some(m) = link.cutover.take() else {
            return;
        };
        m.trace(
            link.peer.device_id,
            self.boot.device_id < link.peer.device_id,
            "cutover",
            "completed",
            "none",
            now,
        );
        path_trace(
            link.peer.device_id,
            "direct",
            None,
            if self.boot.device_id < link.peer.device_id {
                "local"
            } else {
                "peer"
            },
            "cutover",
        );
        let old = link.hop;
        link.hop = m.next_hop;
        link.wire.route_revision = m.wire.migration_id.to_revision;
        link.wire.next_hop_binding = m.next_hop;
        link.wire.reservation = None;
        link.heartbeat = Heartbeat::new(now);
        let path_id = link.wire.path_id;
        let peer = link.peer.device_id;
        self.dials.remove(&peer);
        self.effects.push_back(Effect::DialFinished(peer));
        self.send(
            old.connection_id,
            0,
            LinkMsg::PathCancel {
                reference: PathReference::Installed(path_id),
                reason: Refusal::Superseded,
            },
        );
        self.effects.push_back(Effect::PathChanged { link: id, old });
    }
    fn candidate_waiting_for_path(&self, connection: ConnectionId, candidate: &Candidate) -> bool {
        // Selection can finish while a prior relay install is still pending.
        // If that install fails, the selected direct connection has no path or
        // cutover to time it out. Keep its original candidate budget until a
        // link owns it; an orphan selection would reject every later candidate.
        !matches!(candidate.choice, Choice::Selected(_))
            || !self.links.values().any(|link| {
                link.hop.connection_id == connection
                    || link
                        .cutover
                        .as_ref()
                        .is_some_and(|cutover| cutover.next_hop.connection_id == connection)
            })
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        let mut next = None;
        let mut include = |at: Instant| next = Some(next.map_or(at, |old: Instant| old.min(at)));
        for (&connection, c) in &self.candidates {
            if self.candidate_waiting_for_path(connection, c) {
                include(c.deadline);
            }
        }
        for l in self.links.values() {
            if !l.installed {
                include(l.installation_deadline);
            }
            if let Some(c) = &l.cutover {
                include(c.deadline);
            }
            if l.installed {
                include(l.heartbeat.progress + HEARTBEAT_TIMEOUT);
                include(l.heartbeat.next);
            }
            if l.installed
                && l.wire.reservation.is_some()
                && l.cutover.is_none()
                && !l.addresses.is_empty()
                && !self.dials.contains_key(&l.peer.device_id)
                && self.dials.len() < MAX_DIALS
            {
                include(l.next_direct);
            }
        }
        for d in self.dials.values() {
            // The executor owns candidate/round deadlines, including time spent
            // waiting for a global handshake slot. Only relay deadlines live here.
            if !d.direct && d.request.is_none() {
                include(d.deadline);
            }
            if let Some(r) = d.request {
                include(r.deadline);
            }
        }
        for n in self.nominations.values() {
            include(n.request.deadline);
        }
        for pending in &self.pending_relay {
            include(pending.request.deadline);
        }
        for t in &self.transit {
            if let Some(at) = t.deadline() {
                include(at);
            }
        }
        next
    }
    pub fn tick(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .candidates
            .iter()
            .filter(|(connection, c)| self.candidate_waiting_for_path(**connection, c) && now >= c.deadline)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.selection_trace(id, "failed", "timeout", 0);
            self.connection_lost(id, now);
            self.effects.push_back(Effect::CloseConnection(id));
        }
        let mut lost = Vec::new();
        for (&id, link) in &mut self.links {
            let reason = if !link.installed && now >= link.installation_deadline {
                Some(LinkLost::ConnectionLost)
            } else if link.cutover.as_ref().is_some_and(|m| now >= m.deadline) {
                Some(LinkLost::CutoverTimeout)
            } else if link.installed && now.saturating_duration_since(link.heartbeat.progress) >= HEARTBEAT_TIMEOUT {
                Some(LinkLost::HeartbeatTimeout)
            } else {
                None
            };
            if let Some(reason) = reason {
                lost.push((id, reason));
                continue;
            }
            if link.installed && now >= link.heartbeat.next {
                link.heartbeat.nonce += 1;
                let nonce = link.heartbeat.nonce;
                link.heartbeat.pending.push_back((nonce, now));
                link.heartbeat.next = now + HEARTBEAT_INTERVAL;
                let pulse = Pulse {
                    path_id: link.wire.path_id,
                    route_revision: link.wire.route_revision,
                    nonce,
                };
                self.effects.push_back(Effect::Control {
                    connection: link.hop.connection_id,
                    tag: link.hop.route_tag,
                    message: Box::new(LinkMsg::Pulse(pulse)),
                });
            }
        }
        for (id, reason) in lost {
            self.lose(id, reason);
        }
        self.relay_tick(now);
        let failed: Vec<_> = self
            .dials
            .iter()
            .filter(|(_, d)| !d.direct && d.request.is_none() && now >= d.deadline)
            .map(|(peer, _)| *peer)
            .collect();
        for peer in failed {
            self.fail_dial(peer, DialFailure::Unreachable);
        }
        self.retry_direct(now);
    }
}
