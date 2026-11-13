//! Route selection between the direct connections of one pair (protocol-v2
//! section 11, route-selection design sections 4 and 5). Standby connections
//! stay authenticated with N0 only; the dialer probes them; the arbiter keeps
//! the pair's pin and runs every cut-over, reusing the relay-to-direct drain.
use super::*;
use crate::link::control::{RouteAction, RoutePin, RouteState, route_refusal};
use crate::link::route_policy::{self as policy, Activity};
use std::net::Ipv4Addr;

/// Standby connections per pair, both directions together.
pub(super) const MAX_STANDBY: usize = 8;
/// Route probes carry this bit; Low pacing probes never do.
pub(crate) const ROUTE_NONCE: u64 = 1 << 63;
/// Route messages kept for a link that is not Ready yet (a bound, not a wait).
const EARLY_MESSAGES: usize = 8;

/// What the executor knows and the core does not: per-link activity and
/// Quinn's own RTT estimate of a connection.
pub(in crate::link) trait RouteEnv {
    fn activity(&self, link: LinkId) -> Activity;
    fn quinn_rtt(&self, connection: ConnectionId) -> Option<Duration>;
}

#[derive(Debug, Default, Clone)]
pub(super) struct Rounds(VecDeque<Option<Duration>>);
impl Rounds {
    fn push(&mut self, value: Option<Duration>) {
        if self.0.len() == policy::KEPT_ROUNDS {
            self.0.pop_front();
        }
        self.0.push_back(value);
    }
    fn stats(&self) -> Option<policy::Stats> {
        policy::stats(self.0.iter())
    }
    fn measured(&self) -> bool {
        !self.0.is_empty()
    }
    /// Unreachable display: the newest round was lost and none of the window answered.
    fn answering(&self) -> bool {
        self.0.iter().rev().take(policy::WINDOW).any(Option::is_some)
    }
}

#[derive(Debug)]
struct Round {
    next_send: Instant,
    sent: u8,
    last_sent: Instant,
    pending: Vec<(u64, Instant)>,
    best: Option<Duration>,
}

#[derive(Debug)]
pub(super) struct Probe {
    rounds: Rounds,
    round: Option<Round>,
    next: Instant,
    lost: u8,
    good: u8,
    /// The peer answered a probe on this connection, so it has authenticated
    /// it too (the probe follows the bootstrap on the same N0 stream).
    answered: bool,
}
impl Probe {
    /// `next`: when the first round runs.
    fn new(next: Instant, rounds: Rounds) -> Self {
        Self {
            rounds,
            round: None,
            next,
            lost: 0,
            good: 0,
            answered: false,
        }
    }
}

#[derive(Debug)]
struct Slot {
    address: SocketAddr,
    connection: Option<ConnectionId>,
    dialing: Option<ConnectionId>,
    failures: u8,
    next: Instant,
    unreachable: bool,
    local: bool,
}

#[derive(Debug)]
struct Manual {
    /// Follower: waiting for the arbiter's answer to this request.
    request: Option<u64>,
    pin: Option<RoutePin>,
    deadline: Instant,
}

#[derive(Debug, Default)]
pub(super) struct PeerRoute {
    link: Option<LinkId>,
    addresses: Vec<SocketAddr>,
    slots: Vec<Slot>,
    probes: HashMap<ConnectionId, Probe>,
    current: Rounds,
    current_end: Option<Instant>,
    current_best: Option<Duration>,
    /// Arbiter: re-check a decided switch that waits for idle or the dwell.
    recheck: Option<Instant>,
    pin: Option<RoutePin>,
    pin_connection: Option<ConnectionId>,
    set_by: Option<DeviceId>,
    record: Option<RouteRecord>,
    announced: bool,
    reported: Option<Option<RouteRecord>>,
    dwell: Option<Instant>,
    switched: Option<(Instant, (DeviceId, Ipv4Addr))>,
    cooldown: HashMap<(DeviceId, Ipv4Addr), Instant>,
    proposals: HashMap<ConnectionId, (Duration, Instant)>,
    manual: Option<Manual>,
    deferred: Option<(u64, RouteAction, Instant)>,
    answer: Option<u64>,
    pending_switch: Option<Ipv4Addr>,
    pin_proposal: Option<(RoutePin, ConnectionId)>,
    next_request: u64,
    /// Follower: its own record, to be proposed once the arbiter announced none.
    restore: Option<RouteRecord>,
    /// Route messages that arrived before this end's own Ready: the other end
    /// becomes Ready first and announces at once (the arbiter's first state,
    /// or a follower's request). Applied in order in `route_link_ready`.
    early: Vec<(LinkId, LinkMsg)>,
}
impl PeerRoute {
    fn clear_link(&mut self) {
        self.link = None;
        self.probes.clear();
        self.current = Rounds::default();
        self.current_end = None;
        self.current_best = None;
        self.recheck = None;
        self.pin = None;
        self.pin_connection = None;
        self.set_by = None;
        self.record = None;
        self.announced = false;
        self.reported = None;
        self.dwell = None;
        self.proposals.clear();
        self.manual = None;
        self.deferred = None;
        self.answer = None;
        self.pending_switch = None;
        self.pin_proposal = None;
        self.restore = None;
        for slot in &mut self.slots {
            slot.connection = None;
            slot.dialing = None;
        }
    }
}

fn jitter() -> Duration {
    Duration::from_millis(rand::random::<u64>() % (2 * policy::ROUND_JITTER_MS + 1))
}

impl Core {
    fn route_entry(&mut self, peer: DeviceId) -> &mut PeerRoute {
        self.routes.entry(peer).or_default()
    }
    fn direct_link(&self, peer: DeviceId) -> Option<LinkId> {
        self.links
            .values()
            .find(|l| {
                l.peer.device_id == peer && l.installed && l.state == LinkState::Ready && l.path() == LinkPath::Direct
            })
            .map(|l| l.id)
    }
    fn relayed_link(&self, peer: DeviceId) -> Option<LinkId> {
        self.links
            .values()
            .find(|l| {
                l.peer.device_id == peer
                    && l.installed
                    && l.state == LinkState::Ready
                    && matches!(l.path(), LinkPath::Relayed { .. })
            })
            .map(|l| l.id)
    }
    /// (dialer, target address) of an authenticated connection.
    fn route_key(&self, connection: ConnectionId) -> Option<(DeviceId, Ipv4Addr)> {
        let candidate = self.candidates.get(&connection)?;
        let SocketAddr::V4(target) = candidate.target? else {
            return None;
        };
        Some((connection.initiator_boot.device_id, *target.ip()))
    }
    fn own(&self, connection: ConnectionId) -> bool {
        connection.initiator_boot == self.boot
    }
    pub(super) fn standby(&self, peer: DeviceId, connection: ConnectionId) -> bool {
        self.candidates
            .get(&connection)
            .is_some_and(|c| c.boot.device_id == peer && c.choice == Choice::Standby)
    }
    pub(super) fn standby_count(&self, peer: DeviceId) -> usize {
        self.candidates
            .values()
            .filter(|c| c.boot.device_id == peer && c.choice == Choice::Standby)
            .count()
    }
    fn pin_matches(&self, pin: RoutePin, connection: ConnectionId) -> bool {
        self.route_key(connection) == Some((pin.dialer, pin.target))
            || self
                .candidates
                .get(&connection)
                .and_then(|c| self.routes.get(&c.boot.device_id))
                .is_some_and(|r| {
                    r.pin == Some(pin)
                        && r.pin_connection == Some(connection)
                        && connection.initiator_boot.device_id == pin.dialer
                })
    }
    fn pin_connection(&self, peer: DeviceId, pin: RoutePin) -> Option<ConnectionId> {
        // Several ports/connections can name the same IP. The current one is
        // already effective; don't bind the acceptor to an arbitrary standby.
        if let Some(current) = self.direct_link(peer).map(|id| self.links[&id].hop.connection_id)
            && self.pin_matches(pin, current)
        {
            return Some(current);
        }
        self.candidates
            .iter()
            .filter(|(_, c)| c.boot.device_id == peer && matches!(c.choice, Choice::Standby | Choice::Selected(_)))
            .map(|(id, _)| *id)
            .find(|id| self.pin_matches(pin, *id))
    }
    pub(super) fn own_connection_to(&self, peer: DeviceId, ip: Ipv4Addr) -> Option<ConnectionId> {
        self.pin_connection(
            peer,
            RoutePin {
                dialer: self.boot.device_id,
                target: ip,
            },
        )
    }
    fn local_record(&self, pin: Option<RoutePin>) -> Option<RouteRecord> {
        pin.map(|pin| {
            if pin.dialer == self.boot.device_id {
                RouteRecord::Dial(pin.target)
            } else {
                RouteRecord::Incoming(pin.target)
            }
        })
    }
    fn report_route(&mut self, peer: DeviceId) {
        let Some(route) = self.routes.get(&peer) else {
            return;
        };
        let Some(link) = route.link else {
            return;
        };
        let record = self.local_record(route.pin);
        if route.reported == Some(record) {
            return;
        }
        self.routes.get_mut(&peer).expect("route").reported = Some(record);
        self.effects.push_back(Effect::RouteRecord { link, peer, record });
    }

    // ---- inputs from the owner -------------------------------------------------

    /// Addresses the peer published in its facts (IPv4 with its QUIC port).
    pub fn update_route_addresses(&mut self, peer: DeviceId, mut addresses: Vec<SocketAddr>, now: Instant) {
        addresses.retain(|a| a.is_ipv4() && a.port() != 0 && !a.ip().is_unspecified() && !a.ip().is_multicast());
        addresses.dedup();
        let local_ips = self.local_ips.clone();
        let route = self.route_entry(peer);
        if route.addresses == addresses {
            return;
        }
        route.addresses = addresses.clone();
        let mut removed = Vec::new();
        route.slots.retain(|slot| {
            let keep = addresses.contains(&slot.address);
            if !keep && let Some(connection) = slot.connection {
                removed.push(connection);
            }
            keep
        });
        for address in &addresses {
            if !route.slots.iter().any(|s| s.address == *address) {
                route.slots.push(Slot {
                    address: *address,
                    connection: None,
                    dialing: None,
                    failures: 0,
                    next: now,
                    unreachable: false,
                    local: false,
                });
            }
        }
        // A change of the peer's addresses resets every back-off (4.1).
        for slot in &mut route.slots {
            slot.next = now;
            slot.failures = 0;
            slot.local = matches!(slot.address.ip(), std::net::IpAddr::V4(ip) if local_ips.contains(&ip));
        }
        for connection in removed {
            if self.standby(peer, connection) {
                self.close_standby(peer, connection);
            }
        }
        self.attach_slots(peer);
    }
    /// This device's addresses changed: back-offs reset; equal addresses are not dialed.
    pub fn update_local_addresses(&mut self, ips: Vec<Ipv4Addr>, now: Instant) {
        if self.local_ips == ips {
            return;
        }
        self.local_ips = ips;
        for route in self.routes.values_mut() {
            for slot in &mut route.slots {
                slot.local = matches!(slot.address.ip(), std::net::IpAddr::V4(ip) if self.local_ips.contains(&ip));
                slot.next = now;
                slot.failures = 0;
            }
        }
    }
    /// Whole-group pause or sleep: no standby dials or probe rounds.
    pub fn set_route_probing(&mut self, enabled: bool, now: Instant) {
        if self.probing == enabled {
            return;
        }
        self.probing = enabled;
        for route in self.routes.values_mut() {
            for probe in route.probes.values_mut() {
                probe.round = None;
                probe.next = now;
            }
            for slot in &mut route.slots {
                slot.next = now;
            }
        }
    }
    /// The owner's persisted pin of this peer, given before `set_ready`.
    pub fn set_route_record(&mut self, id: LinkId, record: Option<RouteRecord>) {
        let Some(peer) = self.links.get(&id).map(|l| l.peer.device_id) else {
            return;
        };
        self.route_entry(peer).record = record;
    }
    // ---- standby connections ---------------------------------------------------

    pub(super) fn standby_authenticated(&mut self, peer: DeviceId, connection: ConnectionId, now: Instant) {
        let own = self.own(connection);
        let target = self.candidates.get(&connection).and_then(|c| c.target);
        tracing::info!(target: "oink_link_diagnostics", kind = "route_step", peer_id = peer.to_hex().as_str(),
            operation = diagnostics::hex(&connection.random).as_str(), stage = "standby", result = "completed",
            initiator = if own { "local" } else { "peer" }, address = ?target, "Standby connection kept");
        if own {
            self.try_restore(peer, now);
            let route = self.route_entry(peer);
            if let Some(slot) = route
                .slots
                .iter_mut()
                .find(|s| s.dialing == Some(connection) || Some(s.address) == target)
            {
                slot.dialing = None;
                slot.connection = Some(connection);
                slot.failures = 0;
                slot.unreachable = false;
            }
            route.probes.insert(connection, Probe::new(now, Rounds::default()));
        }
        let deferred = self
            .routes
            .get(&peer)
            .and_then(|r| r.deferred)
            .filter(|(_, action, _)| matches!(action, RouteAction::Use { connection: c, .. } if *c == connection));
        if let Some((request, action, _)) = deferred {
            self.route_entry(peer).deferred = None;
            if let Some(id) = self.direct_link(peer) {
                self.route_request(id, peer, request, action, now);
            }
        }
        if own {
            self.route_probes(peer, now);
        }
    }
    /// An own route dial arrived while no connection is selected (the link runs
    /// over a relay): it competes as an ordinary candidate, and once selected
    /// the relay cuts over to it.
    pub(super) fn route_dial_candidate(&mut self, peer: DeviceId, connection: ConnectionId) {
        if let Some(route) = self.routes.get_mut(&peer)
            && let Some(slot) = route.slots.iter_mut().find(|s| s.dialing == Some(connection))
        {
            slot.dialing = None;
            slot.unreachable = false;
        }
    }
    /// A link became Ready over a relay: try every direct address at once.
    pub(super) fn route_relay_ready(&mut self, peer: DeviceId, now: Instant) {
        if let Some(route) = self.routes.get_mut(&peer) {
            for slot in &mut route.slots {
                slot.next = now;
                slot.failures = 0;
            }
        }
    }
    /// Whether the route state still waits for this dial; the executor drops
    /// one that is not (its link went down, or its result came already).
    pub fn standby_dial_wanted(&self, peer: DeviceId, connection: ConnectionId) -> bool {
        self.routes
            .get(&peer)
            .is_some_and(|r| r.slots.iter().any(|s| s.dialing == Some(connection)))
    }
    /// A route dial ended before it authenticated here: its bootstrap failed,
    /// or this end refused it (`Superseded`). Without this the address would
    /// stay "measuring" for the rest of the link.
    pub(super) fn route_dial_lost(&mut self, connection: ConnectionId, now: Instant) {
        let peer = self
            .routes
            .iter()
            .find(|(_, r)| r.slots.iter().any(|s| s.dialing == Some(connection)))
            .map(|(peer, _)| *peer);
        if let Some(peer) = peer {
            self.standby_dial_failed(peer, connection, now);
        }
    }
    /// The executor could not open this standby dial (no answer, wrong identity, self).
    pub fn standby_dial_failed(&mut self, peer: DeviceId, connection: ConnectionId, now: Instant) {
        let Some(route) = self.routes.get_mut(&peer) else {
            return;
        };
        if let Some(slot) = route.slots.iter_mut().find(|s| s.dialing == Some(connection)) {
            slot.dialing = None;
            slot.unreachable = true;
            slot.failures = slot.failures.saturating_add(1);
            slot.next = now + policy::backoff(slot.failures);
        }
    }
    /// Drop a standby the route state no longer wants (its address was
    /// withdrawn, or its probes stopped answering). One that still carries
    /// another path stays: that path's own heartbeat decides about it.
    fn close_standby(&mut self, peer: DeviceId, connection: ConnectionId) {
        if self.connection_in_use(connection) {
            return;
        }
        self.candidates.remove(&connection);
        self.effects.push_back(Effect::CloseConnection(connection));
        self.route_standby_gone(peer, connection, Instant::now());
    }
    /// A standby connection ended (closed, lost, or rejected by probes).
    pub(super) fn route_standby_gone(&mut self, peer: DeviceId, connection: ConnectionId, now: Instant) {
        let Some(route) = self.routes.get_mut(&peer) else {
            return;
        };
        route.probes.remove(&connection);
        route.proposals.remove(&connection);
        if let Some(slot) = route.slots.iter_mut().find(|s| s.connection == Some(connection)) {
            slot.connection = None;
            slot.unreachable = true;
            slot.failures = slot.failures.saturating_add(1);
            slot.next = now + policy::backoff(slot.failures);
        }
    }
    /// Map own connections to the slot of their address.
    fn attach_slots(&mut self, peer: DeviceId) {
        let own: Vec<_> = self
            .candidates
            .iter()
            .filter(|(id, c)| {
                c.boot.device_id == peer
                    && id.initiator_boot == self.boot
                    && matches!(c.choice, Choice::Standby | Choice::Selected(_))
            })
            .filter_map(|(id, c)| c.target.map(|t| (*id, t)))
            .collect();
        let Some(route) = self.routes.get_mut(&peer) else {
            return;
        };
        for slot in &mut route.slots {
            slot.connection = own.iter().find(|(_, t)| *t == slot.address).map(|(id, _)| *id);
            if slot.connection.is_some() {
                slot.unreachable = false;
            }
        }
    }

    // ---- link lifecycle --------------------------------------------------------

    /// The link became a Ready direct link: at Ready, or when a relayed link
    /// cut over to its direct connection. Route probing starts here.
    pub(super) fn route_link_ready(&mut self, id: LinkId, now: Instant) {
        let link = &self.links[&id];
        if link.state != LinkState::Ready {
            return;
        }
        let peer = link.peer.device_id;
        let arbiter = self.arbiter(peer);
        let direct = link.path() == LinkPath::Direct;
        tracing::info!(target: "oink_link_diagnostics", kind = "route_step", peer_id = peer.to_hex().as_str(),
            operation = diagnostics::hex(&link.hop.connection_id.random).as_str(), stage = "probing",
            result = "entered", arbiter, "Route probing started");
        let route = self.route_entry(peer);
        route.link = Some(id);
        // The owner already holds this record; report only later changes.
        route.reported = Some(route.record);
        // The first round of the current route ends after one sample window.
        route.current_end = direct.then_some(now + policy::CURRENT_SAMPLE);
        route.current_best = None;
        // Wake from sleep and a fresh link reset every back-off (4.1).
        for slot in &mut route.slots {
            slot.next = now;
            slot.failures = 0;
        }
        self.attach_slots(peer);
        if arbiter {
            let record = self.routes[&peer].record;
            let pin = record.map(|record| match record {
                RouteRecord::Dial(ip) => RoutePin {
                    dialer: self.boot.device_id,
                    target: ip,
                },
                RouteRecord::Incoming(ip) => RoutePin {
                    dialer: peer,
                    target: ip,
                },
            });
            let route = self.route_entry(peer);
            route.pin = pin;
            route.set_by = None;
            route.announced = true;
            self.send_route_state(id, None, None);
            self.report_route(peer);
        }
        let early = std::mem::take(&mut self.route_entry(peer).early);
        for (link, message) in early {
            if link == id {
                self.apply_route_message(id, peer, message, now);
            }
        }
    }
    pub(super) fn route_link_lost(&mut self, link: &Link, reason: LinkLost, now: Instant) {
        let peer = link.peer.device_id;
        let standby: Vec<_> = self
            .candidates
            .iter()
            .filter(|(_, c)| c.boot.device_id == peer && c.choice == Choice::Standby)
            .map(|(id, _)| *id)
            .collect();
        for connection in &standby {
            self.candidates.remove(connection);
            self.effects.push_back(Effect::CloseConnection(*connection));
        }
        let Some(route) = self.routes.get_mut(&peer) else {
            return;
        };
        // Whether or not the link ever bound the route state (a relayed link,
        // or one lost before Ready): probes of the closed standbys end, and
        // dials made for it are no longer wanted (the executor drops them).
        route.probes.retain(|connection, _| !standby.contains(connection));
        route.proposals.retain(|connection, _| !standby.contains(connection));
        route.early.retain(|(id, _)| *id != link.id);
        for slot in &mut route.slots {
            slot.dialing = None;
            if slot.connection.is_some_and(|c| standby.contains(&c)) {
                slot.connection = None;
            }
        }
        if route.link != Some(link.id) {
            return;
        }
        // A target that failed its cut-over, or died soon after being chosen,
        // sits out the cool-down (4.5).
        if let Some((at, key)) = route.switched
            && (reason == LinkLost::CutoverTimeout || now.saturating_duration_since(at) < policy::DWELL)
        {
            route.cooldown.insert(key, now + policy::COOLDOWN);
        }
        route.switched = None;
        route.clear_link();
    }

    // ---- heartbeat and probe samples ------------------------------------------

    pub(super) fn route_heartbeat(&mut self, peer: DeviceId, rtt: Duration, now: Instant) {
        if let Some(route) = self.routes.get_mut(&peer)
            && route.link.is_some()
            && route.current_end.is_some_and(|end| now + policy::CURRENT_SAMPLE >= end)
        {
            route.current_best = Some(route.current_best.map_or(rtt, |best| best.min(rtt)));
        }
    }
    pub fn route_pong(&mut self, connection: ConnectionId, nonce: u64, now: Instant) {
        let Some(peer) = self.candidates.get(&connection).map(|c| c.boot.device_id) else {
            return;
        };
        let Some(route) = self.routes.get_mut(&peer) else {
            return;
        };
        if let Some(probe) = route.probes.get_mut(&connection) {
            probe.answered = true;
            if let Some(round) = probe.round.as_mut()
                && let Some(index) = round.pending.iter().position(|(n, _)| *n == nonce)
            {
                let (_, sent) = round.pending.remove(index);
                let rtt = now.saturating_duration_since(sent);
                round.best = Some(round.best.map_or(rtt, |best| best.min(rtt)));
            }
        }
        self.return_to_pin(peer, now);
    }

    // ---- periodic work ---------------------------------------------------------

    pub(in crate::link) fn route_tick(&mut self, now: Instant, env: &dyn RouteEnv) {
        let peers: Vec<_> = self.routes.keys().copied().collect();
        for peer in peers {
            let Some(id) = self.direct_link(peer) else {
                if self.relayed_link(peer).is_some() {
                    // Over a relay the addresses are still dialed on the round's
                    // back-off; one that answers becomes the direct connection.
                    self.route_dials(peer, now);
                    let route = self.routes.get_mut(&peer).expect("route");
                    if route.manual.as_ref().is_some_and(|m| now >= m.deadline) {
                        route.manual = None;
                    }
                }
                continue;
            };
            if self.routes[&peer].link != Some(id) {
                continue;
            }
            self.route_dials(peer, now);
            self.route_probes(peer, now);
            self.return_to_pin(peer, now);
            self.route_current(peer, id, now, env);
            let route = self.routes.get_mut(&peer).expect("route");
            if let Some((request, _, deadline)) = route.deferred
                && now >= deadline
            {
                route.deferred = None;
                self.send_route_state(id, Some(request), Some(route_refusal::CONNECTION_GONE));
            }
            let route = self.routes.get_mut(&peer).expect("route");
            if route.manual.as_ref().is_some_and(|m| now >= m.deadline) {
                route.manual = None;
            }
            route.cooldown.retain(|_, until| now < *until);
            route.proposals.retain(|_, (_, at)| now < *at + policy::PROPOSAL_FRESH);
        }
    }
    fn make_route_room(&mut self, peer: DeviceId) {
        if self.standby_count(peer) < MAX_STANDBY {
            return;
        }
        let victim = self
            .candidates
            .iter()
            .filter(|(id, c)| {
                c.boot.device_id == peer
                    && c.choice == Choice::Standby
                    && !self.connection_in_use(**id)
                    && !self
                        .routes
                        .get(&peer)
                        .and_then(|r| r.pin)
                        .is_some_and(|p| self.pin_matches(p, **id))
            })
            .min_by_key(|(_, c)| c.arrival)
            .map(|(id, _)| *id);
        if let Some(victim) = victim {
            self.close_standby(peer, victim);
        }
    }
    /// A remembered dial must not be starved by the standby cap. Before the
    /// accepting side knows the dialer's NAT target binding, the authenticated
    /// dialer may introduce that connection; it is bound explicitly by Use.
    pub(super) fn route_room_for(
        &mut self,
        peer: DeviceId,
        connection: ConnectionId,
        target: Option<SocketAddr>,
    ) -> bool {
        if let Some(pin) = self.routes.get(&peer).and_then(|r| r.pin)
            && pin.dialer == connection.initiator_boot.device_id
            && self.pin_connection(peer, pin).is_none()
            && (!self.own(connection) || target.is_some_and(|a| a.ip() == pin.target))
        {
            self.make_route_room(peer);
        }
        self.standby_count(peer) < MAX_STANDBY
    }
    fn route_dials(&mut self, peer: DeviceId, now: Instant) {
        if !self.probing {
            return;
        }
        let preferred = self
            .routes
            .get(&peer)
            .and_then(|r| r.manual.as_ref().and_then(|m| m.pin).or(r.pin))
            .filter(|pin| pin.dialer == self.boot.device_id)
            .map(|pin| pin.target);
        if preferred.is_some_and(|ip| self.own_connection_to(peer, ip).is_none()) {
            self.make_route_room(peer);
        }
        if self.standby_count(peer) >= MAX_STANDBY {
            return;
        }
        let boot = self.boot;
        let route = self.routes.get_mut(&peer).expect("route");
        let mut dials = Vec::new();
        route
            .slots
            .sort_by_key(|slot| !preferred.is_some_and(|ip| slot.address.ip() == ip));
        for slot in &mut route.slots {
            if slot.local || slot.connection.is_some() || slot.dialing.is_some() || now < slot.next {
                continue;
            }
            let id = ConnectionId {
                initiator_boot: boot,
                random: rand::random(),
            };
            slot.dialing = Some(id);
            // The executor reports the outcome; a stale result never matches a new id.
            slot.next = now + policy::backoff(slot.failures.saturating_add(1));
            dials.push((slot.address, id));
        }
        for (address, id) in dials {
            self.effects.push_back(Effect::DialStandby { peer, address, id });
        }
    }
    fn route_probes(&mut self, peer: DeviceId, now: Instant) {
        if !self.probing {
            return;
        }
        let mut sends = Vec::new();
        let mut finished = Vec::new();
        let route = self.routes.get_mut(&peer).expect("route");
        for (&connection, probe) in &mut route.probes {
            if probe.round.is_none() && now >= probe.next {
                probe.round = Some(Round {
                    next_send: now,
                    sent: 0,
                    last_sent: now,
                    pending: Vec::new(),
                    best: None,
                });
            }
            let Some(round) = probe.round.as_mut() else {
                continue;
            };
            while round.sent < policy::PROBES && now >= round.next_send {
                self.route_nonce += 1;
                let nonce = ROUTE_NONCE | self.route_nonce;
                round.pending.push((nonce, now));
                round.sent += 1;
                round.last_sent = now;
                round.next_send = now + policy::PROBE_SPACING;
                sends.push((connection, nonce));
            }
            if round.sent == policy::PROBES && (round.pending.is_empty() || now >= round.last_sent + policy::PROBE_LOST)
            {
                finished.push(connection);
            }
        }
        for (connection, nonce) in sends {
            self.send(connection, 0, LinkMsg::HopProbe { nonce });
        }
        for connection in finished {
            self.probe_round_done(peer, connection, now);
        }
    }
    fn probe_round_done(&mut self, peer: DeviceId, connection: ConnectionId, now: Instant) {
        let key = self.route_key(connection);
        let route = self.routes.get_mut(&peer).expect("route");
        let current_stats = route.current.stats();
        let Some(probe) = route.probes.get_mut(&connection) else {
            return;
        };
        let best = probe.round.take().and_then(|r| r.best);
        probe.rounds.push(best);
        probe.next = now + policy::ROUND - Duration::from_millis(policy::ROUND_JITTER_MS) + jitter();
        if best.is_none() {
            probe.lost += 1;
            if probe.lost >= policy::LOST_ROUNDS {
                tracing::info!(target: "oink_link_diagnostics", kind = "route_step", peer_id = peer.to_hex().as_str(),
                    operation = diagnostics::hex(&connection.random).as_str(), stage = "probe", result = "failed",
                    reason = "rounds_lost", "Standby connection unreachable");
                if self.standby(peer, connection) {
                    self.close_standby(peer, connection);
                }
                return;
            }
        } else {
            probe.lost = 0;
        }
        let cooled = key.is_some_and(|key| route.cooldown.get(&key).is_some_and(|until| now < *until));
        let better = !cooled
            && current_stats
                .zip(probe.rounds.stats())
                .is_some_and(|(current, candidate)| policy::clearly_better(current, candidate));
        probe.good = if better { probe.good.saturating_add(1) } else { 0 };
    }
    fn route_current(&mut self, peer: DeviceId, id: LinkId, now: Instant, env: &dyn RouteEnv) {
        let route = self.routes.get_mut(&peer).expect("route");
        let Some(end) = route.current_end else {
            return;
        };
        if now < end {
            // Between rounds only a decided switch waiting for idle is re-checked.
            if route.recheck.is_some_and(|at| now >= at) {
                route.recheck = None;
                self.arbiter_decide(peer, id, now, env);
            }
            return;
        }
        let best = route.current_best.take();
        route.current.push(best);
        route.current_end = Some(now + policy::ROUND);
        if self.arbiter(peer) {
            self.arbiter_decide(peer, id, now, env);
        } else {
            self.follower_propose(peer, id);
        }
    }

    // ---- decisions -------------------------------------------------------------

    /// The best own connection measured clearly better for `CONFIRM_ROUNDS`.
    fn own_better(&self, peer: DeviceId, current: ConnectionId) -> Option<(ConnectionId, Duration)> {
        let route = self.routes.get(&peer)?;
        route
            .probes
            .iter()
            .filter(|(c, p)| **c != current && p.good >= policy::CONFIRM_ROUNDS && self.standby(peer, **c))
            .filter_map(|(c, p)| p.rounds.stats().map(|s| (*c, s.median)))
            .min_by_key(|(_, median)| *median)
    }
    fn arbiter_decide(&mut self, peer: DeviceId, id: LinkId, now: Instant, env: &dyn RouteEnv) {
        let link = &self.links[&id];
        let current = link.hop.connection_id;
        let busy_link = link.cutover.is_some();
        self.routes.get_mut(&peer).expect("route").recheck = None;
        let route = &self.routes[&peer];
        if route.pin.is_some_and(|pin| self.pin_matches(pin, current)) {
            self.routes.get_mut(&peer).expect("route").pending_switch = None;
            return;
        }
        // A remembered target is handled independently by return_to_pin;
        // while unavailable, ordinary automatic fallback remains unchanged.
        let mut best = self.own_better(peer, current);
        for (&connection, &(median, _)) in &route.proposals {
            let cooled = self
                .route_key(connection)
                .is_some_and(|key| route.cooldown.get(&key).is_some_and(|until| now < *until));
            if cooled
                || !self.standby(peer, connection)
                || !policy::proposal_plausible(median, env.quinn_rtt(connection))
            {
                continue;
            }
            if best.is_none_or(|(_, m)| median < m) {
                best = Some((connection, median));
            }
        }
        let target = best.map(|(c, _)| c);
        let target_ip = target.and_then(|c| self.route_key(c)).map(|(_, ip)| ip);
        let route = self.routes.get_mut(&peer).expect("route");
        route.pending_switch = target_ip;
        let Some(target) = target else {
            return;
        };
        let dwelling = route.dwell.is_some_and(|until| now < until);
        // Technical preconditions (4.2): ready, no cut-over, no other path on
        // the current connection (`switch_blocked`).
        if busy_link || dwelling || self.switch_blocked(id) || !policy::idle(env.activity(id), now) {
            // Keep the decision; execute it once idle instead of a round later.
            self.routes.get_mut(&peer).expect("route").recheck = Some(now + policy::IDLE_RECHECK);
            return;
        }
        tracing::info!(target: "oink_link_diagnostics", kind = "route_step", peer_id = peer.to_hex().as_str(),
            operation = diagnostics::hex(&target.random).as_str(), stage = "decision", result = "entered",
            reason = "lower_rtt", "Automatic route switch");
        if self.route_switch(id, peer, target, now) {
            self.routes.get_mut(&peer).expect("route").pending_switch = None;
        }
    }
    /// Follower: propose a better automatic fallback once per heartbeat round.
    fn follower_propose(&mut self, peer: DeviceId, id: LinkId) {
        let current = self.links[&id].hop.connection_id;
        let route = &self.routes[&peer];
        if route.manual.is_some() {
            return;
        }
        let target = if route.pin.is_some_and(|pin| self.pin_matches(pin, current)) {
            None
        } else {
            self.own_better(peer, current).map(|(c, _)| c)
        };
        let Some(connection) = target else {
            return;
        };
        let Some(stats) = self.routes[&peer]
            .probes
            .get(&connection)
            .and_then(|p| p.rounds.stats())
        else {
            return;
        };
        let median_us = stats.median.as_micros().min(u128::from(u32::MAX)) as u32;
        self.route_request_send(id, peer, RouteAction::Propose { connection, median_us });
    }
    /// Start the direct-to-direct cut-over to `target` (arbiter only).
    /// The precondition of every switch, automatic and manual alike (design
    /// 4.2, change plan 6.1): no other path rides the current connection,
    /// neither a relayed link of this device (its first hop) nor a transit it
    /// forwards. Moving away would leave that path on a standby.
    fn switch_blocked(&self, id: LinkId) -> bool {
        self.connection_used_by_other(self.links[&id].hop.connection_id, Some(id))
    }
    fn route_switch(&mut self, id: LinkId, peer: DeviceId, target: ConnectionId, now: Instant) -> bool {
        if !self.arbiter(peer) || !self.standby(peer, target) || self.switch_blocked(id) {
            return false;
        }
        if !self.begin_route_cutover(id, target, now) {
            return false;
        }
        let key = self.route_key(target);
        let route = self.route_entry(peer);
        route.dwell = Some(now + policy::DWELL);
        route.switched = key.map(|key| (now, key));
        route.proposals.clear();
        true
    }

    // ---- manual operations -----------------------------------------------------

    /// Remember the click before trying the route. Reconnection and discovery
    /// remain unrestricted; a preference only controls the connected pair.
    pub fn set_route(&mut self, peer: DeviceId, choice: RouteChoice, now: Instant) -> bool {
        let Some(id) = self.direct_link(peer).or_else(|| self.relayed_link(peer)) else {
            return false;
        };
        let pin = match choice {
            RouteChoice::Auto => None,
            RouteChoice::Pin(ip) => {
                let existing = self.own_connection_to(peer, ip).is_some();
                let route = self.route_entry(peer);
                if let Some(slot) = route.slots.iter_mut().find(|s| s.address.ip() == ip) {
                    if slot.local {
                        return false;
                    }
                    slot.next = now;
                    slot.failures = 0;
                } else if !existing {
                    return false;
                }
                Some(RoutePin {
                    dialer: self.boot.device_id,
                    target: ip,
                })
            }
        };
        // Supersede pending restoration/dials even when the new target fails.
        let route = self.route_entry(peer);
        route.restore = None;
        route.manual = None;
        route.deferred = None;
        if self.arbiter(peer) {
            self.remember_route(peer, pin, self.boot.device_id);
            self.send_route_state(id, None, None);
            self.return_to_pin(peer, now);
        } else {
            self.manual_request(id, peer, RouteAction::Remember { pin }, now);
        }
        self.route_dials(peer, now);
        true
    }

    fn remember_route(&mut self, peer: DeviceId, pin: Option<RoutePin>, author: DeviceId) {
        let record = self.local_record(pin);
        let binding = pin.and_then(|pin| self.pin_connection(peer, pin));
        let route = self.route_entry(peer);
        route.pin = pin;
        route.pin_connection = binding;
        route.set_by = Some(author);
        route.record = record;
        route.restore = None;
        route.manual = None;
        route.deferred = None;
        route.pending_switch = None;
        route.pin_proposal = None;
        self.report_route(peer);
    }

    /// A remembered route is a user choice: once authenticated at both ends,
    /// return immediately, without automatic RTT, dwell or idle thresholds.
    /// The existing drain and shared-path guards still protect active traffic.
    fn return_to_pin(&mut self, peer: DeviceId, now: Instant) {
        let Some(id) = self.direct_link(peer) else {
            return;
        };
        let current = self.links[&id].hop.connection_id;
        let Some(pin) = self.routes.get(&peer).and_then(|r| r.pin) else {
            return;
        };
        if self.pin_matches(pin, current)
            && (self.arbiter(peer) || !self.own(current) || self.routes[&peer].pin_connection == Some(current))
        {
            return;
        }
        let Some(connection) = self.pin_connection(peer, pin) else {
            return;
        };
        if connection != current
            && self.own(connection)
            && !self.routes[&peer].probes.get(&connection).is_some_and(|p| p.answered)
        {
            return;
        }
        if self.arbiter(peer) {
            if !self.own(connection) && self.routes[&peer].pin_connection != Some(connection) {
                return;
            }
            if self.links[&id].cutover.is_none() && !self.switch_blocked(id) {
                self.route_switch(id, peer, connection, now);
            }
        } else if self.own(connection) && self.routes[&peer].pin_proposal != Some((pin, connection)) {
            self.route_entry(peer).pin_proposal = Some((pin, connection));
            self.route_request_send(
                id,
                peer,
                RouteAction::Use {
                    connection,
                    pinned: Some(pin.target),
                },
            );
        }
    }
    fn manual_request(&mut self, id: LinkId, peer: DeviceId, action: RouteAction, now: Instant) {
        let RouteAction::Remember { pin } = action else {
            unreachable!("manual preference");
        };
        let request = self.route_request_send(id, peer, action);
        self.route_entry(peer).manual = Some(Manual {
            request: Some(request),
            pin,
            deadline: now + CUTOVER_TIMEOUT + HEARTBEAT_TIMEOUT + DIAL_BUDGET,
        });
    }
    fn route_request_send(&mut self, id: LinkId, peer: DeviceId, action: RouteAction) -> u64 {
        let route = self.route_entry(peer);
        route.next_request = route.next_request.wrapping_add(1).max(1);
        let request_id = route.next_request;
        let path_id = self.links[&id].wire.path_id;
        self.path_send(
            id,
            LinkMsg::RouteRequest {
                path_id,
                request_id,
                action,
            },
        );
        request_id
    }
    fn send_route_state(&mut self, id: LinkId, answers: Option<u64>, refused: Option<u32>) {
        let link = &self.links[&id];
        let peer = link.peer.device_id;
        let route = &self.routes[&peer];
        let state = RouteState {
            path_id: link.wire.path_id,
            route_revision: link.wire.route_revision,
            active: link.hop.connection_id,
            pin: route.pin,
            pin_connection: route.pin.and_then(|pin| self.pin_connection(peer, pin)),
            set_by: route.set_by,
            answers,
            refused,
        };
        self.path_send(id, LinkMsg::RouteState(state));
    }

    // ---- messages --------------------------------------------------------------

    pub(super) fn route_message(&mut self, connection: ConnectionId, tag: u64, message: LinkMsg, now: Instant) {
        let Some(id) = self.route(connection, tag) else {
            return;
        };
        let link = &self.links[&id];
        let peer = link.peer.device_id;
        let accepted = match &message {
            LinkMsg::RouteRequest { path_id, .. } => self.arbiter(peer) && *path_id == link.wire.path_id,
            LinkMsg::RouteState(state) => !self.arbiter(peer) && state.path_id == link.wire.path_id,
            _ => false,
        };
        if !accepted {
            return;
        }
        if link.state != LinkState::Ready {
            // The other end became Ready first (the arbiter readies on the
            // reply, the follower only once Done reaches its config thread)
            // and announced at once; it does not repeat itself. Keep the
            // message for this end's own Ready instead of dropping it.
            let early = &mut self.route_entry(peer).early;
            early.retain(|(link, _)| *link == id);
            if early.len() == EARLY_MESSAGES {
                early.remove(0);
            }
            early.push((id, message));
            return;
        }
        self.apply_route_message(id, peer, message, now);
    }
    fn apply_route_message(&mut self, id: LinkId, peer: DeviceId, message: LinkMsg, now: Instant) {
        match message {
            LinkMsg::RouteRequest { request_id, action, .. } => self.route_request(id, peer, request_id, action, now),
            LinkMsg::RouteState(state) => self.route_state(peer, state, now),
            _ => {}
        }
    }
    fn route_request(&mut self, id: LinkId, peer: DeviceId, request: u64, action: RouteAction, now: Instant) {
        let current = self.links[&id].hop.connection_id;
        let refuse = |core: &mut Self, reason| core.send_route_state(id, Some(request), Some(reason));
        if self.links[&id].cutover.is_some() && matches!(action, RouteAction::Use { pinned: None, .. }) {
            return refuse(self, route_refusal::MIGRATING);
        }
        match action {
            RouteAction::Use { connection, pinned } => {
                if !self.candidates.contains_key(&connection) {
                    // The requester finishes TLS bootstrap before this end does;
                    // wait for the arriving connection within one dial budget.
                    if connection.initiator_boot.device_id == peer {
                        self.route_entry(peer).deferred = Some((request, action, now + DIAL_BUDGET));
                        return;
                    }
                    return refuse(self, route_refusal::CONNECTION_GONE);
                }
                if connection.initiator_boot.device_id != peer {
                    return refuse(self, route_refusal::NOT_REQUESTER_DIALED);
                }
                if connection != current && !self.standby(peer, connection) {
                    return refuse(self, route_refusal::NOT_STANDBY_OR_CURRENT);
                }
                if connection != current && pinned.is_none() && self.switch_blocked(id) {
                    return refuse(self, route_refusal::CARRIES_PATHS);
                }
                if self.routes[&peer].pin != pinned.map(|target| RoutePin { dialer: peer, target }) {
                    return refuse(self, route_refusal::CONNECTION_GONE);
                }
                // The authenticated dialer names the exact connection it made
                // to this remembered IP. Its public/NAT target can differ from
                // our local socket IP, so local target comparison is insufficient.
                self.route_entry(peer).pin_connection = pinned.map(|_| connection);
                if connection == current
                    || (pinned.is_some() && (self.links[&id].cutover.is_some() || self.switch_blocked(id)))
                {
                    // A shared path or earlier cut-over delays execution, not
                    // the remembered target/binding. The next tick retries it.
                    self.send_route_state(id, Some(request), None);
                } else if self.route_switch(id, peer, connection, now) {
                    self.route_entry(peer).answer = Some(request);
                } else {
                    refuse(self, route_refusal::MIGRATING);
                }
                self.report_route(peer);
            }
            RouteAction::Remember { pin } => {
                if pin.is_some_and(|p| p.dialer != peer && p.dialer != self.boot.device_id) {
                    return refuse(self, route_refusal::NOT_REQUESTER_DIALED);
                }
                self.remember_route(peer, pin, peer);
                self.send_route_state(id, Some(request), None);
                self.return_to_pin(peer, now);
            }
            RouteAction::Propose { connection, median_us } => {
                if !self.candidates.contains_key(&connection) {
                    return refuse(self, route_refusal::CONNECTION_GONE);
                }
                if connection.initiator_boot.device_id != peer {
                    return refuse(self, route_refusal::NOT_REQUESTER_DIALED);
                }
                if !self.standby(peer, connection) {
                    return refuse(self, route_refusal::NOT_STANDBY_OR_CURRENT);
                }
                self.route_entry(peer)
                    .proposals
                    .insert(connection, (Duration::from_micros(median_us.into()), now));
                self.return_to_pin(peer, now);
            }
        }
    }
    fn route_state(&mut self, peer: DeviceId, state: RouteState, now: Instant) {
        let initial = !self.routes.get(&peer).is_some_and(|r| r.announced);
        let route = self.route_entry(peer);
        route.announced = true;
        if route.pin != state.pin {
            route.pin_proposal = None;
        }
        route.pin = state.pin;
        route.pin_connection = state.pin_connection;
        route.set_by = state.set_by;
        if state.answers.is_some() && route.manual.as_ref().and_then(|m| m.request) == state.answers {
            route.manual = None;
        }
        if state.refused.is_some() {
            tracing::info!(target: "oink_link_diagnostics", kind = "route_step", peer_id = peer.to_hex().as_str(),
                stage = "request", result = "failed", refused = state.refused, "Route request refused");
        }
        if state.pin.is_some() || !initial {
            route.restore = None;
        }
        if initial && state.pin.is_none() && route.record.is_some() && route.manual.is_none() {
            // The arbiter has no record; restore this end's own (5.5).
            route.restore = route.record;
            self.try_restore(peer, now);
            return;
        }
        let record = self.local_record(state.pin);
        self.route_entry(peer).record = record;
        self.report_route(peer);
        self.return_to_pin(peer, now);
    }

    /// Restore preference metadata immediately; reachability never determines
    /// whether a remembered user choice is retained on either end.
    fn try_restore(&mut self, peer: DeviceId, now: Instant) {
        let Some(id) = self.direct_link(peer).or_else(|| self.relayed_link(peer)) else {
            return;
        };
        let Some(record) = self.route_entry(peer).restore.take() else {
            return;
        };
        let pin = match record {
            RouteRecord::Dial(target) => RoutePin {
                dialer: self.boot.device_id,
                target,
            },
            RouteRecord::Incoming(target) => RoutePin { dialer: peer, target },
        };
        self.manual_request(id, peer, RouteAction::Remember { pin: Some(pin) }, now);
    }

    // ---- cut-over completion ---------------------------------------------------

    /// After a direct-to-direct cut-over: the old connection is a standby again,
    /// samples follow their connections, the arbiter announces the result.
    pub(super) fn route_cutover_finished(&mut self, id: LinkId, old: ConnectionId, new: ConnectionId, now: Instant) {
        let peer = self.links[&id].peer.device_id;
        let own_old = self.own(old);
        let route = self.route_entry(peer);
        let previous = std::mem::take(&mut route.current);
        route.current = route.probes.remove(&new).map(|p| p.rounds).unwrap_or_default();
        route.current_best = None;
        route.current_end = Some(now + policy::ROUND);
        route.recheck = None;
        route.proposals.clear();
        route.pin_proposal = None;
        // Every other standby was compared with the old current route; its
        // run of better rounds starts again against the new one.
        for probe in route.probes.values_mut() {
            probe.good = 0;
        }
        if own_old {
            route.probes.insert(old, Probe::new(now + policy::ROUND, previous));
        }
        tracing::info!(target: "oink_link_diagnostics", kind = "route_step", peer_id = peer.to_hex().as_str(),
            operation = diagnostics::hex(&new.random).as_str(), stage = "cutover", result = "completed",
            previous = diagnostics::hex(&old.random).as_str(), "Route switched");
        self.attach_slots(peer);
        if self.arbiter(peer) {
            let answer = self.route_entry(peer).answer.take();
            self.send_route_state(id, answer, None);
            self.report_route(peer);
        }
        // A click can have been queued on the old connection after its drain.
        // Re-submit any unacknowledged preference on the new path; the old
        // request's acknowledgement cannot clear this new request ID.
        if !self.arbiter(peer)
            && let Some(pin) = self.routes[&peer].manual.as_ref().map(|m| m.pin)
        {
            self.manual_request(id, peer, RouteAction::Remember { pin }, now);
        }
        self.return_to_pin(peer, now);
    }

    // ---- deadlines and projection ----------------------------------------------

    pub(super) fn route_deadlines(&self, include: &mut dyn FnMut(Instant)) {
        for (peer, route) in &self.routes {
            let Some(link) = route.link else {
                if self.relayed_link(*peer).is_some() {
                    if let Some(manual) = &route.manual {
                        include(manual.deadline);
                    }
                    if self.probing {
                        for slot in &route.slots {
                            if !slot.local && slot.connection.is_none() && slot.dialing.is_none() {
                                include(slot.next);
                            }
                        }
                    }
                }
                continue;
            };
            if !self.links.contains_key(&link) {
                continue;
            }
            if let Some(end) = route.current_end {
                include(end);
            }
            if let Some(at) = route.recheck {
                include(at);
            }
            if let Some((_, _, deadline)) = route.deferred {
                include(deadline);
            }
            if let Some(manual) = &route.manual {
                include(manual.deadline);
            }
            if !self.probing {
                continue;
            }
            for probe in route.probes.values() {
                match &probe.round {
                    Some(round) if round.sent < policy::PROBES => include(round.next_send),
                    Some(round) => include(round.last_sent + policy::PROBE_LOST),
                    None => include(probe.next),
                }
            }
            if self.standby_count(*peer) < MAX_STANDBY {
                for slot in &route.slots {
                    if !slot.local && slot.connection.is_none() && slot.dialing.is_none() {
                        include(slot.next);
                    }
                }
            }
        }
    }

    pub fn route_view(&self, id: LinkId) -> Option<RouteView> {
        let link = self.links.get(&id)?;
        if link.state != LinkState::Ready {
            return None;
        }
        let peer = link.peer.device_id;
        let route = self.routes.get(&peer)?;
        if let LinkPath::Relayed { via } = link.path() {
            // No probes over a relay: an address answers only by becoming the
            // direct connection; until then it is untried or failed.
            let rows = route
                .slots
                .iter()
                .map(|slot| RouteRow {
                    address: slot.address,
                    state: if slot.local || slot.unreachable {
                        RouteRowState::Unreachable
                    } else {
                        RouteRowState::Measuring
                    },
                    median: None,
                    current: false,
                })
                .collect();
            return Some(RouteView {
                direction: Direction::Outbound,
                current_target: None,
                current_median: None,
                pin: self.local_record(route.pin).or(route.record),
                pin_set_by: None,
                pin_unavailable: route.pin.is_some() || route.record.is_some(),
                rows,
                auto_choice: None,
                pending_switch: None,
                can_replace: true,
                pending_manual: route.manual.is_some(),
                relay: Some(via),
            });
        }
        let current = link.hop.connection_id;
        let active = self.own(current);
        let current_target = self.route_key(current).map(|(_, ip)| ip);
        let current_address = self.candidates.get(&current).and_then(|c| c.target);
        let current_median = route.current.stats().map(|s| s.median);
        let mut rows: Vec<RouteRow> = route
            .slots
            .iter()
            .map(|slot| {
                let is_current = active && Some(slot.address) == current_address;
                let probe = slot.connection.and_then(|c| route.probes.get(&c));
                let (state, median) = if is_current {
                    (
                        if route.current.measured() {
                            RouteRowState::Reachable
                        } else {
                            RouteRowState::Measuring
                        },
                        current_median,
                    )
                } else if slot.local {
                    (RouteRowState::Unreachable, None)
                } else if let Some(probe) = probe {
                    if !probe.rounds.measured() {
                        (RouteRowState::Measuring, None)
                    } else if probe.rounds.answering() {
                        (RouteRowState::Reachable, probe.rounds.stats().map(|s| s.median))
                    } else {
                        (RouteRowState::Unreachable, None)
                    }
                } else if slot.unreachable {
                    (RouteRowState::Unreachable, None)
                } else {
                    (RouteRowState::Measuring, None)
                };
                RouteRow {
                    address: slot.address,
                    state,
                    median,
                    current: is_current,
                }
            })
            .collect();
        // A current own dial outside the published addresses (an address typed
        // while offline) is listed as the current row (5.5).
        if active
            && let Some(address) = current_address
            && !rows.iter().any(|r| r.address == address)
        {
            rows.push(RouteRow {
                address,
                state: if route.current.measured() {
                    RouteRowState::Reachable
                } else {
                    RouteRowState::Measuring
                },
                median: current_median,
                current: true,
            });
        }
        let own_reachable = rows
            .iter()
            .filter(|r| !r.current && r.state != RouteRowState::Unreachable)
            .filter(|r| {
                route
                    .slots
                    .iter()
                    .any(|s| s.address == r.address && s.connection.is_some())
            })
            .count();
        let auto_choice = if active && route.pin.is_none() {
            current_target
        } else {
            rows.iter()
                .filter(|r| r.state == RouteRowState::Reachable)
                .filter_map(|r| r.median.map(|m| (r.address, m)))
                .min_by_key(|(_, m)| *m)
                .and_then(|(a, _)| match a.ip() {
                    std::net::IpAddr::V4(ip) => Some(ip),
                    _ => None,
                })
        };
        Some(RouteView {
            direction: if active {
                Direction::Outbound
            } else {
                Direction::Inbound
            },
            current_target,
            current_median,
            pin: self.local_record(route.pin).or(route.restore),
            pin_set_by: route.set_by,
            pin_unavailable: route.pin.is_some_and(|pin| !self.pin_matches(pin, current)) || route.restore.is_some(),
            rows,
            auto_choice,
            pending_switch: route.pending_switch,
            can_replace: own_reachable > 0 || active,
            pending_manual: route.manual.is_some(),
            relay: None,
        })
    }
}

#[cfg(test)]
impl Core {
    /// Test hook: replace measured rounds (`None` connection: the current one)
    /// and the consecutive-better count, as probe rounds would have left them.
    pub(super) fn test_rounds(
        &mut self,
        peer: DeviceId,
        connection: Option<ConnectionId>,
        rounds: &[Option<Duration>],
        good: u8,
    ) {
        let route = self.route_entry(peer);
        let mut value = Rounds::default();
        for round in rounds {
            value.push(*round);
        }
        match connection {
            None => route.current = value,
            Some(connection) => {
                let probe = route
                    .probes
                    .entry(connection)
                    .or_insert_with(|| Probe::new(Instant::now(), Rounds::default()));
                probe.rounds = value;
                probe.good = good;
            }
        }
    }
    /// Test hook: finish one probe round of an own standby with this result.
    pub(super) fn test_probe_round(
        &mut self,
        peer: DeviceId,
        connection: ConnectionId,
        best: Option<Duration>,
        now: Instant,
    ) {
        let probe = self
            .route_entry(peer)
            .probes
            .get_mut(&connection)
            .expect("own standby probe");
        probe.round = Some(Round {
            next_send: now,
            sent: policy::PROBES,
            last_sent: now,
            pending: Vec::new(),
            best,
        });
        self.probe_round_done(peer, connection, now);
    }
    /// Test hook: this device's own standby connection dialed to `address`.
    pub(in crate::link) fn test_own_standby(&self, address: SocketAddr) -> Option<ConnectionId> {
        self.candidates
            .iter()
            .find(|(id, c)| self.own(**id) && c.choice == Choice::Standby && c.target == Some(address))
            .map(|(id, _)| *id)
    }
    /// Test hook: the arbiter switches at once to its own standby to `address`
    /// (a pin by IP cannot tell loopback routes apart: they differ by port).
    pub(in crate::link) fn test_switch_route(&mut self, peer: DeviceId, address: SocketAddr, now: Instant) -> bool {
        let (Some(connection), Some(id)) = (self.test_own_standby(address), self.direct_link(peer)) else {
            return false;
        };
        self.route_switch(id, peer, connection, now)
    }
    /// Test hook: the connection's run of consecutive clearly better rounds.
    pub(super) fn test_good(&self, peer: DeviceId, connection: ConnectionId) -> u8 {
        self.routes[&peer].probes[&connection].good
    }
}
