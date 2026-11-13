//! Route selection state machine (design 9.2): standby connections, direct-to-direct
//! cut-over, roles, pins, requests and their refusals, restore order, idle gating.
use super::*;
use crate::link::control::{RouteAction, route_refusal};
use crate::link::route_policy::{self as policy, Activity};
use std::net::Ipv4Addr;

const PORT: u16 = 47470;
fn wired(n: usize) -> Ipv4Addr {
    Ipv4Addr::new(10, 77, 0, n as u8 + 1)
}
fn wifi(n: usize) -> Ipv4Addr {
    Ipv4Addr::new(192, 168, 1, n as u8 + 1)
}
fn at(ip: Ipv4Addr) -> SocketAddr {
    SocketAddr::from((ip, PORT))
}
fn ms(value: u64) -> Option<Duration> {
    Some(Duration::from_micros(value * 1000))
}

struct Env {
    busy: bool,
}
impl RouteEnv for Env {
    fn activity(&self, _: LinkId) -> Activity {
        Activity {
            held: self.busy,
            ..Default::default()
        }
    }
    fn quinn_rtt(&self, _: ConnectionId) -> Option<Duration> {
        ms(1)
    }
}

struct Routes {
    r: Rig,
    unreachable: Vec<Ipv4Addr>,
    refused: Vec<(usize, u32)>,
    states: Vec<(usize, Option<u64>)>,
    /// Standby dials authenticate at the dialer only, until `release`: the
    /// other end finishes its TLS bootstrap later. Messages on them wait too.
    hold_auth: bool,
    held_auth: Vec<(usize, Ipv4Addr, ConnectionId)>,
    held_messages: Vec<(usize, ConnectionId, u64, LinkMsg)>,
}
impl Routes {
    /// Two nodes; node 0 is the arbiter (smaller id). `dialer` dials the first
    /// connection over Wi-Fi; then both publish wired and Wi-Fi addresses.
    fn new(dialer: usize) -> Self {
        Self::with(dialer, &[], [None, None])
    }
    fn with(dialer: usize, unreachable: &[Ipv4Addr], records: [Option<RouteRecord>; 2]) -> Self {
        Self::build(dialer, unreachable, records, None)
    }
    /// `first`: the node whose config thread reports Ready first; the other
    /// end readies only after everything the first sent has arrived.
    fn build(dialer: usize, unreachable: &[Ipv4Addr], records: [Option<RouteRecord>; 2], first: Option<usize>) -> Self {
        let mut routes = Self {
            r: Rig::new(2),
            unreachable: unreachable.to_vec(),
            refused: vec![],
            states: vec![],
            hold_auth: false,
            held_auth: vec![],
            held_messages: vec![],
        };
        let other = 1 - dialer;
        routes.connect(dialer, other, wifi(other));
        routes.pump();
        for (n, record) in records.into_iter().enumerate() {
            let peer = routes.r.peer(1 - n);
            let now = routes.r.now;
            // Facts arrive on C0 before Done; the record is handed over before Ready.
            routes.r.nodes[n].update_route_addresses(peer, vec![at(wired(1 - n)), at(wifi(1 - n))], now);
            let link = routes.link(n);
            routes.r.nodes[n].set_route_record(link, record);
        }
        match first {
            None => routes.r.ready(0, 1),
            Some(first) => {
                for n in [first, 1 - first] {
                    let link = routes.link(n);
                    routes.r.nodes[n].set_ready(link);
                    routes.pump();
                }
            }
        }
        routes.pump();
        // `set_ready` stamps the route schedule with the real clock.
        routes.r.now = Instant::now();
        routes.tick(Duration::ZERO, false);
        routes
    }
    fn connect(&mut self, a: usize, b: usize, ip: Ipv4Addr) -> ConnectionId {
        let id = ConnectionId {
            initiator_boot: self.r.nodes[a].boot,
            random: rand::random(),
        };
        self.attach(a, b, ip, id);
        id
    }
    fn attach(&mut self, a: usize, b: usize, ip: Ipv4Addr, id: ConnectionId) {
        self.r.connections.insert(id, (a, b));
        for (local, remote) in [(a, b), (b, a)] {
            let boot = self.r.nodes[remote].boot;
            let now = self.r.now;
            self.r.nodes[local].authenticated_at(boot, id, Some(at(ip)), RelayRole::Station, now);
        }
    }
    fn pump(&mut self) {
        for _ in 0..200 {
            let mut progress = false;
            for n in 0..2 {
                let effects = std::mem::take(&mut self.r.nodes[n].effects);
                for effect in effects {
                    progress = true;
                    match &effect {
                        Effect::Control {
                            connection,
                            tag,
                            message,
                        } => {
                            if let LinkMsg::RouteState(state) = message.as_ref() {
                                self.states.push((n, state.answers));
                                if let Some(reason) = state.refused {
                                    self.refused.push((n, reason));
                                }
                            }
                            if self.held_auth.iter().any(|(_, _, id)| id == connection) {
                                self.held_messages.push((n, *connection, *tag, (**message).clone()));
                            } else if let Some(&(a, b)) = self.r.connections.get(connection) {
                                let now = self.r.now;
                                self.r.nodes[if a == n { b } else { a }].receive(
                                    *connection,
                                    *tag,
                                    (**message).clone(),
                                    now,
                                );
                            }
                        }
                        Effect::DialStandby { address, id, .. } => {
                            let std::net::IpAddr::V4(ip) = address.ip() else {
                                unreachable!()
                            };
                            if self.unreachable.contains(&ip) {
                                let peer = self.r.peer(1 - n);
                                let now = self.r.now;
                                self.r.nodes[n].standby_dial_failed(peer, *id, now);
                            } else if self.hold_auth {
                                self.r.connections.insert(*id, (n, 1 - n));
                                self.held_auth.push((n, ip, *id));
                                let boot = self.r.nodes[1 - n].boot;
                                let now = self.r.now;
                                self.r.nodes[n].authenticated_at(boot, *id, Some(at(ip)), RelayRole::Station, now);
                            } else {
                                self.attach(n, 1 - n, ip, *id);
                            }
                        }
                        Effect::CloseConnection(connection) => {
                            if let Some(&(a, b)) = self.r.connections.get(connection) {
                                let now = self.r.now;
                                self.r.nodes[if a == n { b } else { a }].connection_lost(*connection, now);
                            }
                        }
                        Effect::Freeze(link) => {
                            let link = *link;
                            self.r.nodes[n].drained(link);
                        }
                        _ => {}
                    }
                    self.r.observed.push((n, effect));
                }
            }
            if !progress {
                return;
            }
        }
        panic!("route loop did not settle");
    }
    /// The other ends of held standby dials authenticate now; what was sent
    /// on them is delivered behind that, as on one ordered N0 stream.
    fn release(&mut self) {
        self.hold_auth = false;
        let now = self.r.now;
        for (n, ip, id) in std::mem::take(&mut self.held_auth) {
            let boot = self.r.nodes[n].boot;
            self.r.nodes[1 - n].authenticated_at(boot, id, Some(at(ip)), RelayRole::Station, now);
        }
        for (n, connection, tag, message) in std::mem::take(&mut self.held_messages) {
            self.r.nodes[1 - n].receive(connection, tag, message, now);
        }
        self.pump();
    }
    fn tick(&mut self, advance: Duration, busy: bool) {
        self.r.now += advance;
        for n in 0..2 {
            let now = self.r.now;
            self.r.nodes[n].route_tick(now, &Env { busy });
        }
        self.pump();
    }
    fn link(&self, n: usize) -> LinkId {
        self.r.link(n, 1 - n)
    }
    fn current(&self, n: usize) -> ConnectionId {
        self.r.nodes[n].links[&self.link(n)].hop.connection_id
    }
    fn own(&self, n: usize, ip: Ipv4Addr) -> ConnectionId {
        self.r.nodes[n]
            .own_connection_to(self.r.peer(1 - n), ip)
            .expect("own connection")
    }
    fn view(&self, n: usize) -> RouteView {
        self.r.nodes[n].route_view(self.link(n)).expect("view")
    }
    fn choose(&mut self, n: usize, choice: RouteChoice) -> bool {
        let peer = self.r.peer(1 - n);
        let now = self.r.now;
        let accepted = self.r.nodes[n].set_route(peer, choice, now);
        self.pump();
        accepted
    }
    fn measure(&mut self, n: usize, connection: Option<ConnectionId>, rtt: u64, good: u8) {
        let peer = self.r.peer(1 - n);
        self.r.nodes[n].test_rounds(peer, connection, &[ms(rtt); 6], good);
    }
    fn pin(&self, n: usize) -> Option<RouteRecord> {
        self.view(n).pin
    }
}

#[test]
fn second_connection_is_kept_as_standby_on_both_ends_up_to_the_cap() {
    let mut s = Routes::new(1);
    // The scheduler dialed every published address from both ends.
    for n in 0..2 {
        let peer = s.r.peer(1 - n);
        assert_eq!(s.r.nodes[n].standby_count(peer), 3, "node {n}");
    }
    let rejected = s
        .r
        .observed
        .iter()
        .filter(|(_, e)| matches!(e, Effect::Control { message, .. } if matches!(**message, LinkMsg::ConnectionReject { .. })))
        .count();
    assert_eq!(rejected, 0);
    for extra in 0..6 {
        s.connect(1, 0, Ipv4Addr::new(172, 16, 0, extra));
    }
    s.pump();
    let peer = s.r.peer(1);
    assert_eq!(s.r.nodes[0].standby_count(peer), route::MAX_STANDBY);
    assert!(s.r.observed.iter().any(
        |(_, e)| matches!(e, Effect::Control { message, .. } if matches!(**message, LinkMsg::ConnectionReject { reason: Refusal::Superseded, .. }))
    ));
}

fn reject_count(s: &Routes) -> usize {
    s.r.observed
        .iter()
        .filter(|(_, e)| matches!(e, Effect::Control { message, .. } if matches!(**message, LinkMsg::ConnectionReject { .. })))
        .count()
}

#[test]
fn direct_to_direct_cutover_keeps_old_connection_as_standby_and_bumps_revision() {
    let mut s = Routes::new(0);
    let old = s.current(0);
    let revision = s.r.nodes[0].links[&s.link(0)].wire.route_revision;
    let before = s.r.observed.len();
    assert!(s.choose(0, RouteChoice::Pin(wired(1))));
    let target = s.own(0, wired(1));
    for n in 0..2 {
        let link = &s.r.nodes[n].links[&s.link(n)];
        assert_eq!(link.hop.connection_id, target, "node {n}");
        assert_eq!(link.wire.route_revision, revision.next().unwrap());
        assert_eq!(link.path(), LinkPath::Direct);
        assert!(
            s.r.nodes[n].standby(s.r.peer(1 - n), old),
            "old connection is a standby on {n}"
        );
        assert!(s.r.nodes[n].selected_connection_id(target));
    }
    assert!(
        !s.r.observed.iter().any(
            |(_, e)| matches!(e, Effect::Control { message, .. } if matches!(**message, LinkMsg::PathCancel { .. }))
        )
    );
    assert_eq!(
        s.r.observed
            .iter()
            .filter(|(_, e)| matches!(e, Effect::PathChanged { .. }))
            .count(),
        2
    );
    assert!(
        !s.r.observed[before..]
            .iter()
            .any(|(_, e)| matches!(e, Effect::DialFinished(_)))
    );
    assert_eq!(reject_count(&s), 0);
    // A late frame of the old path no longer routes to the link.
    let old_tag = s.r.nodes[0].links[&s.link(0)].hop.route_tag;
    assert!(s.r.nodes[0].route(old, old_tag).is_none());
}

#[test]
fn follower_passive_choosing_an_address_replaces_the_route_with_its_own_dial() {
    let mut s = Routes::new(0);
    assert_eq!(s.view(1).direction, Direction::Inbound);
    assert!(s.choose(1, RouteChoice::Pin(wired(0))));
    let own = s.own(1, wired(0));
    assert_eq!(s.current(0), own);
    assert_eq!(s.current(1), own);
    assert_eq!(s.pin(1), Some(RouteRecord::Dial(wired(0))));
    assert_eq!(s.pin(0), Some(RouteRecord::Incoming(wired(0))));
    assert_eq!(s.view(0).direction, Direction::Inbound);
    assert_eq!(s.view(1).direction, Direction::Outbound);
    assert_eq!(s.view(0).pin_set_by, Some(s.r.peer(1)));
    assert!(!s.view(1).pending_manual);
    assert!(s.states.iter().any(|(n, answers)| *n == 0 && answers.is_some()));
}

#[test]
fn arbiter_passive_choosing_an_address_replaces_the_route_with_its_own_dial() {
    let mut s = Routes::new(1);
    assert_eq!(s.view(0).direction, Direction::Inbound);
    assert!(s.choose(0, RouteChoice::Pin(wired(1))));
    assert_eq!(s.current(1), s.own(0, wired(1)));
    assert_eq!(s.pin(0), Some(RouteRecord::Dial(wired(1))));
    assert_eq!(s.pin(1), Some(RouteRecord::Incoming(wired(1))));
    assert_eq!(s.view(1).direction, Direction::Inbound);
}

#[test]
fn automatic_from_either_end_only_clears_the_pair_preference_even_without_own_routes() {
    for dialer in 0..2 {
        for clicker in 0..2 {
            let mut s = Routes::with(dialer, &[wired(dialer), wifi(dialer)], [None, None]);
            assert!(s.choose(dialer, RouteChoice::Pin(wifi(1 - dialer))));
            let current = s.current(0);
            assert!(s.choose(clicker, RouteChoice::Auto));
            for n in 0..2 {
                assert_eq!(s.pin(n), None);
                assert_eq!(s.current(n), current);
            }
        }
    }
}

#[test]
fn failed_click_is_remembered_on_both_ends_and_another_click_clears_it() {
    for clicker in 0..2 {
        let target = wired(1 - clicker);
        let mut s = Routes::with(0, &[target], [None, None]);
        let current = s.current(0);
        assert!(s.choose(clicker, RouteChoice::Pin(target)));
        assert_eq!(s.pin(clicker), Some(RouteRecord::Dial(target)));
        assert_eq!(s.pin(1 - clicker), Some(RouteRecord::Incoming(target)));
        for n in 0..2 {
            assert!(s.view(n).pin_unavailable);
        }
        assert_eq!(s.current(0), current);
        assert!(s.choose(1 - clicker, RouteChoice::Auto));
        s.unreachable.clear();
        s.tick(policy::BACKOFF[0], false);
        for n in 0..2 {
            assert_eq!(s.pin(n), None);
        }
        assert_eq!(s.current(0), current, "old dial cannot resurrect the choice");
    }
}

#[test]
fn invalid_requests_get_their_refusal_reason_and_change_nothing() {
    let mut s = Routes::new(1);
    let link = s.link(0);
    let tag = s.r.nodes[0].links[&link].hop.route_tag;
    let path_id = s.r.nodes[0].links[&link].wire.path_id;
    let current = s.current(0);
    let arbiter_dial = s.own(0, wired(1));
    let send = |s: &mut Routes, request_id: u64, action: RouteAction| {
        let now = s.r.now;
        s.r.nodes[0].receive(
            current,
            tag,
            LinkMsg::RouteRequest {
                path_id,
                request_id,
                action,
            },
            now,
        );
        s.pump();
    };
    send(
        &mut s,
        1,
        RouteAction::Use {
            connection: arbiter_dial,
            pinned: None,
        },
    );
    assert!(s.refused.contains(&(0, route_refusal::NOT_REQUESTER_DIALED)));
    send(
        &mut s,
        2,
        RouteAction::Propose {
            connection: current,
            median_us: 10,
        },
    );
    assert!(s.refused.contains(&(0, route_refusal::NOT_STANDBY_OR_CURRENT)));
    let foreign = ConnectionId {
        initiator_boot: s.r.nodes[0].boot,
        random: [9; 16],
    };
    send(
        &mut s,
        3,
        RouteAction::Use {
            connection: foreign,
            pinned: None,
        },
    );
    assert!(s.refused.contains(&(0, route_refusal::CONNECTION_GONE)));
    // The requester's own connection that never arrives is refused after one budget.
    let missing = ConnectionId {
        initiator_boot: s.r.nodes[1].boot,
        random: [8; 16],
    };
    let before = s.refused.len();
    send(
        &mut s,
        4,
        RouteAction::Use {
            connection: missing,
            pinned: None,
        },
    );
    assert_eq!(s.refused.len(), before, "deferred while its bootstrap may still finish");
    s.tick(DIAL_BUDGET, false);
    assert_eq!(s.refused.last(), Some(&(0, route_refusal::CONNECTION_GONE)));
    assert_eq!(s.current(0), current);
    assert_eq!(s.pin(0), None);
}

#[test]
fn later_operation_wins_and_both_ends_converge() {
    let mut s = Routes::new(0);
    assert!(s.choose(0, RouteChoice::Pin(wired(1))));
    assert_eq!(s.pin(1), Some(RouteRecord::Incoming(wired(1))));
    assert!(s.choose(1, RouteChoice::Pin(wifi(0))));
    assert_eq!(s.current(0), s.own(1, wifi(0)));
    assert_eq!(s.pin(0), Some(RouteRecord::Incoming(wifi(0))));
    assert_eq!(s.pin(1), Some(RouteRecord::Dial(wifi(0))));
    let records: Vec<_> =
        s.r.observed
            .iter()
            .filter_map(|(n, e)| match e {
                Effect::RouteRecord { record, .. } => Some((*n, *record)),
                _ => None,
            })
            .collect();
    assert_eq!(records.last().copied(), Some((1, Some(RouteRecord::Dial(wifi(0))))));
    assert!(records.contains(&(0, Some(RouteRecord::Incoming(wifi(0))))));
}

#[test]
fn restart_restores_arbiter_record_first_then_follower_and_arbiter_wins_mismatch() {
    // The arbiter's record applies at once; its target is not current, so it
    // stays pinned and unavailable (automatic fallback) until it returns.
    let s = Routes::with(0, &[], [Some(RouteRecord::Dial(wired(1))), None]);
    assert_eq!(s.pin(0), Some(RouteRecord::Dial(wired(1))));
    assert_eq!(s.pin(1), Some(RouteRecord::Incoming(wired(1))));
    assert!(!s.view(0).pin_unavailable);
    // Only the follower has a record: it proposes it after the arbiter's state.
    let s = Routes::with(0, &[], [None, Some(RouteRecord::Dial(wired(0)))]);
    assert_eq!(s.current(0), s.own(1, wired(0)));
    assert_eq!(s.pin(0), Some(RouteRecord::Incoming(wired(0))));
    assert_eq!(s.pin(1), Some(RouteRecord::Dial(wired(0))));
    // Both have records that disagree: the arbiter's wins.
    let s = Routes::with(
        0,
        &[],
        [Some(RouteRecord::Dial(wifi(1))), Some(RouteRecord::Dial(wired(0)))],
    );
    assert_eq!(s.pin(0), Some(RouteRecord::Dial(wifi(1))));
    assert_eq!(s.pin(1), Some(RouteRecord::Incoming(wifi(1))));
    assert_eq!(s.current(0), s.own(0, wifi(1)));
}

#[test]
fn pinned_target_returns_as_soon_as_authenticated_even_while_busy() {
    for owner in 0..2 {
        let target_ip = wired(1 - owner);
        let mut records = [None, None];
        records[owner] = Some(RouteRecord::Dial(target_ip));
        let mut s = Routes::with(0, &[target_ip], records);
        let current = s.current(0);
        assert!(s.view(owner).pin_unavailable);
        s.unreachable.clear();
        s.tick(policy::BACKOFF[0], true);
        assert_ne!(s.current(0), current);
        assert_eq!(s.current(0), s.own(owner, target_ip));
        assert!(!s.view(owner).pin_unavailable);
    }
}

#[test]
fn automatic_switch_waits_for_idle_and_then_dwells() {
    let mut s = Routes::new(0);
    let fast = s.own(0, wired(1));
    let first = s.current(0);
    for _ in 0..3 {
        s.measure(0, None, 10, 0);
        s.measure(0, Some(fast), 1, policy::CONFIRM_ROUNDS);
        s.tick(policy::ROUND, true);
        assert_eq!(s.current(0), first);
    }
    s.measure(0, None, 10, 0);
    s.measure(0, Some(fast), 1, policy::CONFIRM_ROUNDS);
    s.tick(policy::ROUND, false);
    assert_eq!(s.current(0), fast);
    // Within the dwell time a better route does not move the link again.
    s.measure(0, None, 10, 0);
    s.measure(0, Some(first), 1, policy::CONFIRM_ROUNDS);
    s.tick(policy::ROUND, false);
    assert_eq!(s.current(0), fast);
    s.r.now += policy::DWELL;
    s.measure(0, None, 10, 0);
    s.measure(0, Some(first), 1, policy::CONFIRM_ROUNDS);
    s.tick(policy::ROUND, false);
    assert_eq!(s.current(0), first);
}

#[test]
fn follower_proposal_switches_at_the_arbiter_when_idle() {
    let mut s = Routes::new(0);
    let own = s.own(1, wired(0));
    let first = s.current(0);
    s.measure(1, None, 10, 0);
    s.measure(1, Some(own), 1, policy::CONFIRM_ROUNDS);
    s.tick(policy::ROUND, false);
    // The arbiter decides on its next round.
    s.measure(1, None, 10, 0);
    s.measure(1, Some(own), 1, policy::CONFIRM_ROUNDS);
    s.tick(policy::ROUND, false);
    assert_ne!(s.current(0), first);
    assert_eq!(s.current(0), own);
    assert_eq!(s.pin(0), None);
}

fn route_probes(s: &Routes, node: usize) -> usize {
    s.r.observed
        .iter()
        .filter(|(n, e)| {
            *n == node
                && matches!(e, Effect::Control { message, .. }
                    if matches!(**message, LinkMsg::HopProbe { nonce } if nonce & route::ROUTE_NONCE != 0))
        })
        .count()
}

#[test]
fn probes_run_in_the_background_once_a_round_starting_at_once() {
    let mut s = Routes::new(0);
    // Node 0's own standby (to the wired address) runs its first round at
    // once: three probes 20 ms apart, no menu involved.
    for _ in 0..3 {
        s.tick(policy::PROBE_SPACING, false);
    }
    assert_eq!(route_probes(&s, 0), 3);
    s.tick(Duration::ZERO, false);
    let row = |s: &Routes| {
        s.view(0)
            .rows
            .into_iter()
            .find(|r| r.address == at(wired(1)))
            .expect("row")
    };
    assert_eq!(row(&s).state, RouteRowState::Reachable, "stored after one round");
    assert!(row(&s).median.is_some());
    // Nothing more until the next round, one `ROUND` (+-jitter) later.
    let jitter = Duration::from_millis(policy::ROUND_JITTER_MS);
    s.tick(policy::ROUND - jitter - Duration::from_millis(1), false);
    assert_eq!(route_probes(&s, 0), 3);
    s.tick(jitter * 2 + Duration::from_millis(1), false);
    assert_eq!(route_probes(&s, 0), 4);
}

#[test]
fn unreachable_addresses_back_off_on_the_round() {
    let mut s = Routes::with(0, &[wired(1)], [None, None]);
    let dials = |s: &Routes| {
        s.r.observed
            .iter()
            .filter(|(n, e)| *n == 0 && matches!(e, Effect::DialStandby { address, .. } if *address == at(wired(1))))
            .count()
    };
    assert_eq!(dials(&s), 1);
    s.tick(policy::BACKOFF[0] - Duration::from_millis(1), false);
    assert_eq!(dials(&s), 1);
    s.tick(Duration::from_millis(1), false);
    assert_eq!(dials(&s), 2);
    s.tick(policy::BACKOFF[1], false);
    assert_eq!(dials(&s), 3);
    s.tick(policy::BACKOFF[2], false);
    s.tick(policy::BACKOFF[2], false);
    assert_eq!(dials(&s), 5, "five minutes at most");
}

/// Regression (0.4.0-inhouse.110, "always measuring"): a link that became
/// Ready over a relay and then cut over to a direct connection never started
/// route probing, so every row stayed "measuring" for the link's lifetime.
#[test]
fn a_relayed_link_that_cuts_over_to_direct_starts_route_probing() {
    let mut r = Rig::new(3);
    r.relay();
    r.ready(0, 2);
    let peer = r.peer(2);
    let now = r.now;
    r.nodes[0].update_route_addresses(peer, vec![at(wired(2)), at(wifi(2))], now);
    let a = r.link(0, 2);
    let b = r.link(2, 0);
    r.connect(2, 0);
    r.pump();
    r.nodes[0].drained(a);
    r.nodes[2].drained(b);
    r.pump();
    assert_eq!(r.nodes[0].links[&a].path(), LinkPath::Direct);
    r.nodes[0].effects.clear();
    let now = Instant::now();
    r.nodes[0].route_tick(now, &Env { busy: false });
    let dialed: Vec<_> = r.nodes[0]
        .effects
        .iter()
        .filter_map(|e| match e {
            Effect::DialStandby { peer: p, address, .. } if *p == peer => Some(*address),
            _ => None,
        })
        .collect();
    assert_eq!(dialed, vec![at(wired(2)), at(wifi(2))], "standby dials start");
    assert!(r.nodes[0].route_view(a).expect("view").relay.is_none());
}

#[test]
fn over_a_relay_addresses_are_listed_and_an_answering_one_can_be_chosen() {
    let mut r = Rig::new(3);
    r.relay();
    r.ready(0, 2);
    let peer = r.peer(2);
    let via = r.peer(1);
    let now = r.now;
    r.nodes[0].update_route_addresses(peer, vec![at(wired(2)), at(wifi(2))], now);
    let a = r.link(0, 2);
    let view = r.nodes[0].route_view(a).expect("relayed view");
    assert_eq!(view.relay, Some(via));
    assert!(view.rows.iter().all(|row| row.state == RouteRowState::Measuring));
    r.nodes[0].effects.clear();
    let now = Instant::now();
    r.nodes[0].route_tick(now, &Env { busy: false });
    let dials: Vec<_> = r.nodes[0]
        .effects
        .drain(..)
        .filter_map(|e| match e {
            Effect::DialStandby { address, id, .. } => Some((address, id)),
            _ => None,
        })
        .collect();
    assert_eq!(dials.len(), 2, "both addresses are tried over the relay");
    // The Wi-Fi address does not answer: UI disables it, while a direct API
    // choice still remembers it independently of whether a dial succeeds.
    let (_, failed) = dials.iter().find(|(a, _)| *a == at(wifi(2))).copied().unwrap();
    r.nodes[0].standby_dial_failed(peer, failed, now);
    let view = r.nodes[0].route_view(a).unwrap();
    let state = |ip| view.rows.iter().find(|row| row.address == at(ip)).unwrap().state;
    assert_eq!(state(wifi(2)), RouteRowState::Unreachable);
    assert_eq!(state(wired(2)), RouteRowState::Measuring);
    assert!(r.nodes[0].set_route(peer, RouteChoice::Pin(wifi(2)), now));
    assert_eq!(r.nodes[0].route_view(a).unwrap().pin, Some(RouteRecord::Dial(wifi(2))));
    // Choosing the wired address waits for its dial, which then answers:
    // the relay cuts over to it and the choice becomes the pin.
    assert!(r.nodes[0].set_route(peer, RouteChoice::Pin(wired(2)), now));
    let (_, id) = dials.iter().find(|(a, _)| *a == at(wired(2))).copied().unwrap();
    r.connections.insert(id, (0, 2));
    for (local, remote, target) in [(0, 2, at(wired(2))), (2, 0, at(wifi(0)))] {
        let boot = r.nodes[remote].boot;
        r.nodes[local].authenticated_at(boot, id, Some(target), RelayRole::Station, now);
    }
    r.pump();
    let b = r.link(2, 0);
    r.nodes[0].drained(a);
    r.nodes[2].drained(b);
    r.pump();
    assert_eq!(r.nodes[0].links[&a].hop.connection_id, id);
    let view = r.nodes[0].route_view(a).unwrap();
    assert!(view.relay.is_none());
    assert_eq!(view.pin, Some(RouteRecord::Dial(wired(2))));
    assert_eq!(view.direction, Direction::Outbound);
    assert_eq!(
        r.nodes[2].route_view(b).unwrap().pin,
        Some(RouteRecord::Incoming(wired(2)))
    );
}

#[test]
fn link_loss_closes_standbys_and_keeps_the_pin_record() {
    let mut s = Routes::new(0);
    assert!(s.choose(0, RouteChoice::Pin(wired(1))));
    let peer = s.r.peer(1);
    assert!(s.r.nodes[0].standby_count(peer) > 0);
    let link = s.link(0);
    s.r.nodes[0].lose(link, LinkLost::HeartbeatTimeout);
    assert_eq!(s.r.nodes[0].standby_count(peer), 0);
    // The owner keeps its record: no record change is reported on loss.
    let after: Vec<_> = s.r.nodes[0]
        .effects
        .iter()
        .filter(|e| matches!(e, Effect::RouteRecord { .. }))
        .collect();
    assert!(after.is_empty());
}

/// Review H4: the arbiter readies first (it readies on the reply; the
/// follower only once Done reaches its config thread) and announces its
/// state at once. The follower is not Ready yet; the state must not be lost.
#[test]
fn route_state_sent_before_the_follower_is_ready_still_applies() {
    // The arbiter's own pin of its dial: the follower learns it is passive-pinned.
    let s = Routes::build(0, &[], [Some(RouteRecord::Dial(wifi(1))), None], Some(0));
    assert_eq!(s.pin(1), Some(RouteRecord::Incoming(wifi(1))));
    assert_eq!(s.pin(0), Some(RouteRecord::Dial(wifi(1))));
    // Only the follower has a record: the arbiter's empty first state still
    // reaches it and it restores its own pin.
    let s = Routes::build(1, &[], [None, Some(RouteRecord::Dial(wifi(0)))], Some(0));
    assert_eq!(s.pin(0), Some(RouteRecord::Incoming(wifi(0))));
    assert_eq!(s.pin(1), Some(RouteRecord::Dial(wifi(0))));
}

/// Review M6: the arbiter picks an address it has no standby to yet. Its own
/// dial authenticates here first; the follower knows the connection only
/// after its TLS bootstrap ends. The switch must wait for that.
#[test]
fn a_manual_switch_to_a_fresh_dial_waits_until_the_follower_has_it() {
    let mut s = Routes::with(0, &[wired(1)], [None, None]);
    s.unreachable.clear();
    s.hold_auth = true;
    assert!(s.choose(0, RouteChoice::Pin(wired(1))));
    let (_, _, fresh) = *s.held_auth.first().expect("dial to the chosen address");
    assert_ne!(s.current(0), fresh, "nothing switched before the follower has it");
    s.release();
    for n in 0..2 {
        assert_eq!(s.current(n), fresh, "node {n}");
    }
    assert_eq!(s.pin(0), Some(RouteRecord::Dial(wired(1))));
    assert_eq!(s.pin(1), Some(RouteRecord::Incoming(wired(1))));
}

/// Review L2 (user rule): one round a minute, three clearly better rounds in
/// a row qualify a route, counted from its first round; a switch restarts
/// the run of every other standby.
#[test]
fn three_better_rounds_in_a_row_qualify_and_a_switch_restarts_other_runs() {
    let mut s = Routes::new(0);
    let peer = s.r.peer(1);
    let now = s.r.now;
    let wired_own = s.own(0, wired(1));
    s.r.nodes[0].test_rounds(peer, None, &[ms(10); 6], 0);
    s.r.nodes[0].test_rounds(peer, Some(wired_own), &[], 0);
    for round in 1..=policy::CONFIRM_ROUNDS {
        s.r.nodes[0].test_probe_round(peer, wired_own, ms(1), now);
        assert_eq!(s.r.nodes[0].test_good(peer, wired_own), round, "round {round}");
    }
    // The follower's standby to the arbiter's wired address was two rounds
    // into its run when the arbiter switched elsewhere.
    let follower_own = s.own(1, wired(0));
    let arbiter = s.r.peer(0);
    s.r.nodes[1].test_rounds(arbiter, Some(follower_own), &[ms(1); 6], 2);
    assert!(s.choose(0, RouteChoice::Pin(wired(1))));
    assert_eq!(s.current(1), wired_own);
    assert_eq!(s.r.nodes[1].test_good(s.r.peer(0), follower_own), 0);
}

/// Review L4: a route dial whose bootstrap fails, and one still running when
/// its link goes down, must not stay "measuring" forever.
#[test]
fn route_dials_end_with_a_failed_bootstrap_and_with_their_link() {
    let mut r = Rig::new(3);
    r.relay();
    r.ready(0, 2);
    let peer = r.peer(2);
    let now = r.now;
    r.nodes[0].update_route_addresses(peer, vec![at(wired(2)), at(wifi(2))], now);
    let a = r.link(0, 2);
    let b = r.link(2, 0);
    let now = Instant::now();
    let dials = |r: &mut Rig| -> Vec<(SocketAddr, ConnectionId)> {
        r.nodes[0]
            .effects
            .drain(..)
            .filter_map(|e| match e {
                Effect::DialStandby { address, id, .. } => Some((address, id)),
                _ => None,
            })
            .collect()
    };
    r.nodes[0].effects.clear();
    r.nodes[0].route_tick(now, &Env { busy: false });
    let first = dials(&mut r);
    let id = |address| first.iter().find(|(a, _)| *a == address).unwrap().1;
    // The wired dial reached the peer but its bootstrap failed: the executor
    // drops it before it ever authenticates.
    r.nodes[0].connection_lost(id(at(wired(2))), now);
    let view = r.nodes[0].route_view(a).unwrap();
    let state = |ip| view.rows.iter().find(|row| row.address == at(ip)).unwrap().state;
    assert_eq!(state(wired(2)), RouteRowState::Unreachable);
    assert!(!r.nodes[0].standby_dial_wanted(peer, id(at(wired(2)))));
    // The relay goes down while the Wi-Fi dial still runs: it is not wanted
    // any more, and the next link dials that address afresh.
    assert!(r.nodes[0].standby_dial_wanted(peer, id(at(wifi(2)))));
    r.nodes[0].lose(a, LinkLost::RelayLost);
    r.nodes[2].lose(b, LinkLost::RelayLost);
    assert!(!r.nodes[0].standby_dial_wanted(peer, id(at(wifi(2)))));
    r.connect(0, 2);
    r.pump();
    r.ready(0, 2);
    r.nodes[0].effects.clear();
    r.nodes[0].route_tick(Instant::now(), &Env { busy: false });
    let again: Vec<_> = dials(&mut r).into_iter().map(|(address, _)| address).collect();
    assert_eq!(again, vec![at(wired(2)), at(wifi(2))]);
}

#[test]
fn reconnect_over_opposite_direction_keeps_manual_memory_and_returns_after_recovery() {
    for owner in 0..2 {
        let mut first = Routes::new(owner);
        let target = wired(1 - owner);
        assert!(first.choose(owner, RouteChoice::Pin(target)));
        // Exactly the records emitted to each agent's persistent AddressStore.
        let mut records = [None, None];
        for (n, effect) in &first.r.observed {
            if let Effect::RouteRecord { record, .. } = effect {
                records[*n] = *record;
            }
        }
        assert_eq!(records[owner], Some(RouteRecord::Dial(target)));
        assert_eq!(records[1 - owner], Some(RouteRecord::Incoming(target)));
        // A fresh process connects through the OTHER device, on Wi-Fi, while
        // the remembered wired IP is down. Initial dialing was unrestricted.
        let mut next = Routes::with(1 - owner, &[target], records);
        assert_eq!(next.view(owner).direction, Direction::Inbound);
        assert_eq!(next.pin(owner), records[owner]);
        assert!(next.view(0).pin_unavailable && next.view(1).pin_unavailable);
        next.unreachable.clear();
        next.tick(policy::BACKOFF[0], true);
        assert_eq!(next.current(0), next.own(owner, target));
        for n in 0..2 {
            assert!(!next.view(n).pin_unavailable);
        }
    }
}

#[test]
fn another_ip_click_supersedes_unavailable_memory_and_late_dial_results() {
    for owner in 0..2 {
        let old = wired(1 - owner);
        let mut s = Routes::with(0, &[old], [None, None]);
        assert!(s.choose(owner, RouteChoice::Pin(old)));
        let other = 1 - owner;
        let target = wifi(owner);
        assert!(s.choose(other, RouteChoice::Pin(target)));
        let current = s.current(0);
        s.unreachable.clear();
        s.tick(policy::BACKOFF[0], true);
        assert_eq!(s.current(0), current);
        assert_eq!(s.pin(other), Some(RouteRecord::Dial(target)));
        assert_eq!(s.pin(owner), Some(RouteRecord::Incoming(target)));
    }
}

#[test]
fn relayed_pair_synchronizes_and_clears_memory_without_changing_relay_presentation() {
    let mut r = Rig::new(3);
    r.relay();
    r.ready(0, 2);
    let a = r.link(0, 2);
    let b = r.link(2, 0);
    let peer = r.peer(0);
    let now = r.now;
    r.nodes[2].update_route_addresses(peer, vec![at(wired(0))], now);
    assert!(r.nodes[2].set_route(peer, RouteChoice::Pin(wired(0)), now));
    r.pump();
    let va = r.nodes[0].route_view(a).unwrap();
    let vb = r.nodes[2].route_view(b).unwrap();
    assert_eq!(va.relay, Some(r.peer(1)));
    assert_eq!(vb.relay, Some(r.peer(1)));
    assert_eq!(va.pin, Some(RouteRecord::Incoming(wired(0))));
    assert_eq!(vb.pin, Some(RouteRecord::Dial(wired(0))));
    assert!(va.pin_unavailable && vb.pin_unavailable);
    let peer = r.peer(2);
    assert!(r.nodes[0].set_route(peer, RouteChoice::Auto, now));
    r.pump();
    assert_eq!(r.nodes[0].route_view(a).unwrap().pin, None);
    assert_eq!(r.nodes[2].route_view(b).unwrap().pin, None);
    assert_eq!(r.nodes[0].links[&a].path(), LinkPath::Relayed { via: r.peer(1) });
}

#[test]
fn clearing_during_authentication_prevents_the_late_connection_from_restoring_the_pin() {
    let mut s = Routes::with(0, &[wired(1)], [None, None]);
    let current = s.current(0);
    s.unreachable.clear();
    s.hold_auth = true;
    assert!(s.choose(0, RouteChoice::Pin(wired(1))));
    assert!(s.view(0).pin_unavailable && s.view(1).pin_unavailable);
    assert!(s.choose(1, RouteChoice::Auto));
    s.release();
    for n in 0..2 {
        assert_eq!(s.pin(n), None);
        assert_eq!(s.current(n), current);
    }
}

#[test]
fn a_later_click_during_cutover_supersedes_the_old_choice() {
    let mut s = Routes::new(0);
    let peer = s.r.peer(1);
    let now = s.r.now;
    assert!(s.r.nodes[0].set_route(peer, RouteChoice::Pin(wired(1)), now));
    assert!(s.r.nodes[0].links[&s.link(0)].cutover.is_some());
    assert!(s.choose(1, RouteChoice::Pin(wifi(0))));
    s.tick(Duration::ZERO, false);
    assert_eq!(s.pin(1), Some(RouteRecord::Dial(wifi(0))));
    assert_eq!(s.pin(0), Some(RouteRecord::Incoming(wifi(0))));
    assert_eq!(s.current(0), s.own(1, wifi(0)));
}

#[test]
fn manual_route_uses_connection_identity_when_nat_changes_the_acceptors_local_ip() {
    for owner in 0..2 {
        let mut s = Routes::new(0);
        let target = Ipv4Addr::new(198, 51, 100, 9);
        let local = Ipv4Addr::new(192, 0, 2, 9);
        let id = ConnectionId {
            initiator_boot: s.r.nodes[owner].boot,
            random: rand::random(),
        };
        s.r.connections.insert(id, (owner, 1 - owner));
        for (n, peer, ip) in [(owner, 1 - owner, target), (1 - owner, owner, local)] {
            let boot = s.r.nodes[peer].boot;
            let now = s.r.now;
            s.r.nodes[n].authenticated_at(boot, id, Some(at(ip)), RelayRole::Station, now);
        }
        s.pump();
        assert!(s.choose(owner, RouteChoice::Pin(target)));
        assert_eq!(s.current(0), id);
        assert!(s.choose(1 - owner, RouteChoice::Auto));
        assert!(s.choose(owner, RouteChoice::Pin(target)));
        for n in 0..2 {
            assert!(!s.view(n).pin_unavailable, "owner {owner}, end {n}");
        }
        // In particular the accepting arbiter must not treat its local/NAT IP
        // as evidence that the chosen public address is unavailable.
        s.measure(0, None, 10, 0);
        let other = s.own(0, wired(1));
        if other != id {
            s.measure(0, Some(other), 1, policy::CONFIRM_ROUNDS);
        }
        s.tick(policy::ROUND, false);
        assert_eq!(s.current(0), id);
    }
}

#[test]
fn remembered_dial_gets_a_slot_when_unrelated_standbys_fill_the_cap() {
    for owner in 0..2 {
        let target = wired(1 - owner);
        let mut s = Routes::with(0, &[target], [None, None]);
        for last in 10..20 {
            s.connect(1 - owner, owner, Ipv4Addr::new(203, 0, 113, last));
        }
        s.pump();
        assert_eq!(s.r.nodes[owner].standby_count(s.r.peer(1 - owner)), route::MAX_STANDBY);
        s.unreachable.clear();
        assert!(s.choose(owner, RouteChoice::Pin(target)));
        s.tick(Duration::ZERO, false);
        assert_eq!(s.current(0), s.own(owner, target));
        for n in 0..2 {
            assert!(!s.view(n).pin_unavailable);
        }
    }
}

#[test]
fn a_follower_choice_waiting_for_a_shared_path_runs_when_that_path_retires() {
    let mut r = Rig::new(3);
    r.relay();
    r.ready(0, 2);
    let a = r.link(1, 2);
    let b = r.link(2, 1);
    let old = r.nodes[1].links[&a].hop.connection_id;
    let id = ConnectionId {
        initiator_boot: r.nodes[2].boot,
        random: rand::random(),
    };
    r.connections.insert(id, (2, 1));
    let now = r.now;
    for (n, peer) in [(1, 2), (2, 1)] {
        let boot = r.nodes[peer].boot;
        r.nodes[n].authenticated_at(boot, id, Some(at(wired(1))), RelayRole::Station, now);
    }
    r.pump();
    let peer = r.peer(1);
    assert!(r.nodes[2].set_route(peer, RouteChoice::Pin(wired(1)), now));
    r.pump();
    assert_eq!(r.nodes[1].links[&a].hop.connection_id, old);
    assert_eq!(
        r.nodes[1].route_view(a).unwrap().pin,
        Some(RouteRecord::Incoming(wired(1)))
    );
    let transit_source = r.link(1, 0);
    r.nodes[1].retire_authority(transit_source);
    for n in [1, 2] {
        r.nodes[n].route_tick(now, &Env { busy: true });
    }
    r.pump();
    r.nodes[1].drained(a);
    r.nodes[2].drained(b);
    r.pump();
    assert_eq!(r.nodes[1].links[&a].hop.connection_id, id);
    assert_eq!(r.nodes[2].links[&b].hop.connection_id, id);
}
