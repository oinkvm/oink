use super::*;

struct Rig {
    nodes: Vec<Core>,
    connections: HashMap<ConnectionId, (usize, usize)>,
    now: Instant,
    observed: Vec<(usize, Effect)>,
}
impl Rig {
    fn new(count: u8) -> Self {
        Self {
            nodes: (1..=count)
                .map(|n| {
                    Core::new(Boot {
                        device_id: DeviceId([n; 32]),
                        nonce: [n; 16],
                    })
                })
                .collect(),
            connections: HashMap::new(),
            now: Instant::now(),
            observed: vec![],
        }
    }
    fn peer(&self, n: usize) -> DeviceId {
        self.nodes[n].boot.device_id
    }
    fn connect(&mut self, a: usize, b: usize) -> ConnectionId {
        self.connect_with(a, b, true)
    }
    /// Node `n` is a phone: a relay endpoint that never forwards.
    fn phone(&mut self, n: usize) {
        self.nodes[n].set_forwards(false);
    }
    /// `relay`: whether the two ends negotiated oink.relay/1. Each end sees
    /// the other as a relay station unless the other is a phone.
    fn connect_with(&mut self, a: usize, b: usize, relay: bool) -> ConnectionId {
        let id = ConnectionId {
            initiator_boot: self.nodes[a].boot,
            random: rand::random(),
        };
        self.connections.insert(id, (a, b));
        for (local, remote) in [(a, b), (b, a)] {
            let boot = self.nodes[remote].boot;
            let role = match (relay, self.nodes[remote].forwards) {
                (false, _) => RelayRole::None,
                (true, false) => RelayRole::Endpoint,
                (true, true) => RelayRole::Station,
            };
            self.nodes[local].authenticated_at(boot, id, None, role, self.now);
        }
        id
    }
    fn path_requests_on(&self, connection: ConnectionId) -> usize {
        self.observed
            .iter()
            .filter(|(_, e)| {
                matches!(e, Effect::Control { connection: c, message, .. }
                if *c == connection && matches!(**message, LinkMsg::PathRequest { .. }))
            })
            .count()
    }
    fn pump(&mut self) {
        self.pump_holding(&mut vec![], |_, _| false);
    }
    fn pump_holding(&mut self, held: &mut Vec<(usize, Effect)>, hold: impl Fn(usize, &Effect) -> bool) {
        for _ in 0..100 {
            let mut progress = false;
            for n in 0..self.nodes.len() {
                let effects = std::mem::take(&mut self.nodes[n].effects);
                for effect in effects {
                    progress = true;
                    if hold(n, &effect) {
                        held.push((n, effect));
                        continue;
                    }
                    if let Effect::Control {
                        connection,
                        tag,
                        message,
                    } = &effect
                    {
                        let &(a, b) = self.connections.get(connection).expect("connection");
                        self.nodes[if a == n { b } else { a }].receive(
                            *connection,
                            *tag,
                            (**message).clone(),
                            self.now,
                        );
                    }
                    self.observed.push((n, effect));
                }
            }
            if !progress {
                return;
            }
        }
        panic!("control loop did not settle");
    }
    fn link(&self, a: usize, b: usize) -> LinkId {
        self.nodes[a].peer_link(self.peer(b)).expect("link")
    }
    fn ready(&mut self, a: usize, b: usize) {
        for (local, remote) in [(a, b), (b, a)] {
            let id = self.link(local, remote);
            self.nodes[local].set_ready(id);
        }
        self.pump();
    }
    fn relay(&mut self) {
        self.connect(0, 1);
        self.connect(1, 2);
        self.pump();
        self.ready(0, 1);
        self.ready(1, 2);
        let target = self.peer(2);
        self.nodes[0]
            .dial(target, vec![], DialMode::PreferDirect, DialOrigin::Initial, self.now)
            .unwrap();
        self.pump();
    }
}
#[test]
fn simultaneous_dials_choose_first_at_arbiter_and_complementary_direction() {
    let mut r = Rig::new(2);
    let first = r.connect(1, 0);
    r.connect(0, 1);
    r.pump();
    let a = &r.nodes[0].links[&r.link(0, 1)];
    let b = &r.nodes[1].links[&r.link(1, 0)];
    assert_eq!(a.hop.connection_id, first);
    assert_eq!(b.hop.connection_id, first);
    assert_eq!(a.direction, Direction::Inbound);
    assert_eq!(b.direction, Direction::Outbound);
    assert_eq!(
        r.observed
            .iter()
            .filter(|(_, e)| matches!(e, Effect::Connected(_)))
            .count(),
        2
    );
}
#[test]
fn inbound_waits_for_explicit_admission_done_and_readiness_is_once() {
    let mut r = Rig::new(2);
    r.connect(0, 1);
    r.pump();
    let id = r.link(1, 0);
    assert_eq!(r.nodes[1].inbound(id), Err(StreamCode::NotReady));
    r.nodes[1].set_half_open(id);
    assert_eq!(r.nodes[1].inbound(id), Err(StreamCode::NotReady));
    assert_eq!(r.nodes[1].links[&id].state, LinkState::HalfOpen);
    r.nodes[1].set_ready(id);
    assert_eq!(r.nodes[1].inbound(id), Ok(()));
    r.pump();
    assert_eq!(
        r.observed
            .iter()
            .filter(|(n, e)| *n == 1 && matches!(e, Effect::Ready(_)))
            .count(),
        1
    );
}
#[test]
fn heartbeat_measurement_time_is_the_matching_pong_not_a_later_snapshot() {
    let mut r = Rig::new(2);
    let c = r.connect(0, 1);
    r.pump();
    let id = r.link(0, 1);
    assert_eq!(r.nodes[0].links[&id].rtt_measured_at(), None);
    r.nodes[0].tick(r.now);
    let link = &r.nodes[0].links[&id];
    let tag = link.hop.route_tag;
    let pulse = Pulse {
        path_id: link.wire.path_id,
        route_revision: link.wire.route_revision,
        nonce: link.heartbeat.pending.back().unwrap().0,
    };
    let completed = r.now + Duration::from_millis(14);
    r.nodes[0].receive(c, tag, LinkMsg::Pong(pulse), completed);
    assert_eq!(r.nodes[0].links[&id].rtt(), Some(Duration::from_millis(14)));
    assert_eq!(r.nodes[0].links[&id].rtt_measured_at(), Some(completed));
    // A repeated/unsolicited Pong and a later statistics tick are not measurements.
    let sampled = completed + Duration::from_millis(170);
    r.nodes[0].receive(c, tag, LinkMsg::Pong(pulse), sampled);
    r.nodes[0].tick(sampled);
    assert_eq!(r.nodes[0].links[&id].rtt_measured_at(), Some(completed));
}
#[test]
fn only_matching_pong_advances_heartbeat() {
    let mut r = Rig::new(2);
    let c = r.connect(0, 1);
    r.pump();
    let id = r.link(0, 1);
    r.nodes[0].tick(r.now);
    let link = &r.nodes[0].links[&id];
    let pulse = Pulse {
        path_id: link.wire.path_id,
        route_revision: link.wire.route_revision,
        nonce: 999,
    };
    let tag = link.hop.route_tag;
    r.nodes[0].receive(c, tag, LinkMsg::Pong(pulse), r.now + Duration::from_secs(1));
    r.nodes[0].tick(r.now + HEARTBEAT_TIMEOUT);
    assert!(!r.nodes[0].links.contains_key(&id));
    assert!(r.nodes[0].effects.iter().any(|e| matches!(
        e,
        Effect::Lost {
            reason: LinkLost::HeartbeatTimeout,
            ..
        }
    )));
}

#[test]
fn heartbeat_installation_wait_uses_only_the_installation_deadline() {
    let mut r = Rig::new(2);
    r.connect(0, 1);
    let mut held = vec![];
    r.pump_holding(&mut held, |_, effect| {
        matches!(effect, Effect::Control { message, .. } if matches!(message.as_ref(), LinkMsg::PathInstalled(_)))
    });
    assert_eq!(held.len(), 1);
    let pending = (0..2)
        .find(|&n| r.nodes[n].links.values().any(|link| !link.installed))
        .unwrap();
    let id = r.link(pending, 1 - pending);
    let deadline = r.nodes[pending].links[&id].installation_deadline;
    let before_deadline = r.now + HEARTBEAT_TIMEOUT;
    assert!(before_deadline < deadline);

    r.nodes[pending].tick(before_deadline);
    assert!(
        r.nodes[pending].links.contains_key(&id),
        "a path cannot miss heartbeats before it can send them"
    );
    assert_eq!(r.nodes[pending].next_deadline(), Some(deadline));
    assert!(!r.nodes[pending].effects.iter().any(|effect| {
        matches!(effect, Effect::Control { message, .. } if matches!(message.as_ref(), LinkMsg::Pulse(_)))
    }));

    // Holding Installed forever still obeys the existing installation budget.
    r.nodes[pending].tick(deadline);
    assert!(!r.nodes[pending].links.contains_key(&id));
}

#[test]
fn heartbeat_installation_completion_starts_a_fresh_liveness_budget() {
    let mut r = Rig::new(2);
    r.connect(0, 1);
    let mut held = vec![];
    r.pump_holding(&mut held, |_, effect| {
        matches!(effect, Effect::Control { message, .. } if matches!(message.as_ref(), LinkMsg::PathInstalled(_)))
    });
    assert_eq!(held.len(), 1);
    let pending = (0..2)
        .find(|&n| r.nodes[n].links.values().any(|link| !link.installed))
        .unwrap();
    let id = r.link(pending, 1 - pending);
    r.now += HEARTBEAT_TIMEOUT + Duration::from_millis(100);
    assert!(r.now < r.nodes[pending].links[&id].installation_deadline);
    for (n, effect) in held {
        r.nodes[n].effects.push_back(effect);
    }
    r.pump();
    assert!(r.nodes[pending].links[&id].installed);

    // No Pong is delivered after installation: the complete heartbeat budget
    // must be available, and expiry must still occur at its original length.
    r.nodes[pending].tick(r.now + HEARTBEAT_TIMEOUT - Duration::from_millis(1));
    assert!(r.nodes[pending].links.contains_key(&id));
    r.nodes[pending].tick(r.now + HEARTBEAT_TIMEOUT);
    assert!(!r.nodes[pending].links.contains_key(&id));
    assert!(r.nodes[pending].effects.iter().any(|effect| {
        matches!(effect, Effect::Lost { link, reason: LinkLost::HeartbeatTimeout, .. } if *link == id)
    }));
}
#[test]
fn pending_relay_request_follows_direct_readiness_without_a_new_attempt() {
    // Both C0 endpoints can finish in either order, independently on each hop.
    for source in [0, 2] {
        for order in [[0, 1, 2, 3], [3, 2, 1, 0], [0, 3, 1, 2], [1, 2, 0, 3]] {
            let mut r = Rig::new(3);
            let peer = r.peer(2 - source);
            r.nodes[source]
                .dial(peer, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
                .unwrap();
            let request = r.nodes[source].dials[&peer].request.unwrap();
            r.pump();
            r.connect(0, 1);
            r.connect(1, 2);
            r.pump();
            let ends = [(0, 1), (1, 0), (1, 2), (2, 1)];
            for (local, remote) in ends {
                let id = r.link(local, remote);
                r.nodes[local].set_half_open(id);
            }
            for (index, end) in order.into_iter().enumerate() {
                let (local, remote) = ends[end];
                let id = r.link(local, remote);
                r.nodes[local].set_ready(id);
                r.pump();
                if index < 3 {
                    assert!(
                        r.nodes[source].peer_link(peer).is_none(),
                        "no route before both hops are admitted"
                    );
                }
            }
            let id = r.nodes[source]
                .peer_link(peer)
                .expect("pending request must use newly admitted hops");
            let link = &r.nodes[source].links[&id];
            assert_eq!(link.path(), LinkPath::Relayed { via: r.peer(1) });
            assert_eq!(link.wire.request_id, request.id, "no replacement request");
            assert!(r.observed.iter().all(|(_, e)| !matches!(e, Effect::DialFailed(_, _))));
            assert_eq!(r.now + RELAY_TIMEOUT, request.deadline);
        }
    }
}

#[test]
fn relay_search_skips_a_neighbor_that_does_not_forward() {
    // Node 1 is a phone: a relay endpoint (or, before 0.4.0's relay endpoint,
    // no oink.relay/1 at all), never a relay station. Node 0 searches a route
    // to node 2, starting before and after the phone link is ready.
    for (dial_first, relay) in [(true, false), (false, false), (true, true), (false, true)] {
        let mut r = Rig::new(3);
        let peer = r.peer(2);
        if dial_first {
            r.nodes[0]
                .dial(peer, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
                .unwrap();
            r.pump();
        }
        r.phone(1);
        let phone = r.connect_with(0, 1, relay);
        r.pump();
        r.ready(0, 1);
        if !dial_first {
            r.nodes[0]
                .dial(peer, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
                .unwrap();
            r.pump();
        }
        assert!(r.nodes[0].dials[&peer].request.is_some(), "the search is live");
        assert_eq!(r.path_requests_on(phone), 0, "a phone is never asked to relay");
        assert!(r.nodes[0].peer_link(r.peer(1)).is_some(), "the phone link stays up");
    }
}

#[test]
fn relay_does_not_forward_a_request_to_a_target_without_relay() {
    // Node 2 is a device without oink.relay/1, reachable only from node 1.
    let mut r = Rig::new(3);
    r.connect(0, 1);
    r.phone(2);
    let phone = r.connect_with(1, 2, false);
    r.pump();
    r.ready(0, 1);
    r.ready(1, 2);
    let target = r.peer(2);
    r.nodes[0]
        .dial(target, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
        .unwrap();
    r.pump();
    assert_eq!(r.path_requests_on(phone), 0);
    assert!(r.nodes[0].peer_link(target).is_none());
    assert!(r.nodes[1].peer_link(target).is_some(), "the relay keeps its phone link");
}

#[test]
fn a_pc_reaches_a_phone_endpoint_through_a_pc_relay_station() {
    // Node 0 (PC A) cannot reach node 2 (phone) directly; node 1 (PC B) can.
    // Either end may start the search: the phone also dials its members.
    for source in [0, 2] {
        let mut r = Rig::new(3);
        r.phone(2);
        r.connect(0, 1);
        let phone = r.connect(1, 2);
        r.pump();
        r.ready(0, 1);
        r.ready(1, 2);
        let target = r.peer(2 - source);
        r.nodes[source]
            .dial(target, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
            .unwrap();
        r.pump();
        for (local, remote) in [(0, 2), (2, 0)] {
            let id = r.nodes[local].peer_link(r.peer(remote)).expect("relayed link");
            assert_eq!(r.nodes[local].links[&id].path(), LinkPath::Relayed { via: r.peer(1) });
        }
        assert_eq!(r.nodes[1].transit_paths().len(), 1, "the PC carries the path");
        if source == 0 {
            assert_eq!(
                r.path_requests_on(phone),
                1,
                "only the reserved request reaches the phone"
            );
        }
        assert!(r.observed.iter().all(|(_, e)| !matches!(e, Effect::DialFailed(_, _))));
    }
}

#[test]
fn a_relay_endpoint_never_forwards_another_devices_path() {
    // Node 1 is a phone with direct links to PCs 0 and 2. A search from node 0
    // never asks it, and a request it receives anyway (a peer bug) is not
    // forwarded.
    let mut r = Rig::new(3);
    r.phone(1);
    let to_phone = r.connect(0, 1);
    let onward = r.connect(1, 2);
    r.pump();
    r.ready(0, 1);
    r.ready(1, 2);
    let target = r.peer(2);
    r.nodes[0]
        .dial(target, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
        .unwrap();
    r.pump();
    assert_eq!(r.path_requests_on(to_phone), 0, "a phone is never asked to relay");
    let request = PathRequestId {
        source_boot: r.nodes[0].boot,
        random: [7; 16],
    };
    r.nodes[1].receive(
        to_phone,
        0,
        LinkMsg::PathRequest {
            reservation: None,
            request_id: request,
            expected_target_boot: None,
            target,
            round: 1,
        },
        r.now,
    );
    r.pump();
    assert_eq!(r.path_requests_on(onward), 0, "the phone forwards nothing");
    assert!(r.nodes[1].transit_paths().is_empty());
    r.now += RELAY_TIMEOUT;
    r.nodes[0].tick(r.now);
    r.pump();
    assert!(r.nodes[0].peer_link(target).is_none());
    assert!(
        r.observed
            .iter()
            .any(|(n, e)| *n == 0 && matches!(e, Effect::DialFailed(peer, DialFailure::Unreachable) if *peer == target))
    );
}

#[test]
fn losing_the_relay_station_takes_the_relayed_phone_link_down() {
    let mut r = Rig::new(3);
    r.phone(2);
    let station = r.connect(0, 1);
    r.connect(1, 2);
    r.pump();
    r.ready(0, 1);
    r.ready(1, 2);
    let phone = r.peer(2);
    r.nodes[0]
        .dial(phone, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
        .unwrap();
    r.pump();
    let relayed = r.link(0, 2);
    let at_phone = r.link(2, 0);
    // PC A loses its connection to the relay station: the relayed link ends
    // at A, and the station tells the phone so its end goes too.
    r.nodes[0].connection_lost(station, r.now);
    r.nodes[1].connection_lost(station, r.now);
    r.pump();
    assert!(r.nodes[0].peer_link(phone).is_none());
    assert!(!r.nodes[2].links.contains_key(&at_phone));
    assert!(r.nodes[1].transit_paths().is_empty());
    for (n, link) in [(0, relayed), (2, at_phone)] {
        assert!(
            r.observed
                .iter()
                .any(|(m, e)| *m == n && matches!(e, Effect::Lost { link: l, .. } if *l == link)),
            "node {n} reports the relayed link lost"
        );
    }
}

#[test]
fn expired_relay_search_is_not_revived_by_a_new_neighbor() {
    let mut r = Rig::new(3);
    let peer = r.peer(2);
    r.nodes[0]
        .dial(peer, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
        .unwrap();
    r.now += RELAY_TIMEOUT;
    r.nodes[0].tick(r.now);
    r.pump();
    assert!(!r.nodes[0].dials.contains_key(&peer));
    r.connect(0, 1);
    r.connect(1, 2);
    r.pump();
    r.ready(0, 1);
    r.ready(1, 2);
    assert!(r.nodes[0].peer_link(peer).is_none());
    assert!(r.observed.iter().all(|(_, effect)| !matches!(effect,
        Effect::Control { message, .. } if matches!(**message, LinkMsg::PathRequest { .. })
    )));
}

#[test]
fn deferred_relay_requests_remain_bounded_and_retire_without_renewing_expiry() {
    for finish in ["expiry", "cancel", "lost", "revoked"] {
        let mut r = Rig::new(3);
        let source = r.connect(0, 1);
        let target = r.connect(1, 2);
        r.pump();
        let request_id = PathRequestId {
            source_boot: r.nodes[0].boot,
            random: [91; 16],
        };
        let message = LinkMsg::PathRequest {
            reservation: None,
            request_id,
            expected_target_boot: None,
            target: r.peer(2),
            round: 1,
        };
        r.nodes[1].receive(source, 0, message.clone(), r.now);
        assert!(
            r.nodes[1].pending_relay.is_empty(),
            "unadmitted connections cannot queue requests"
        );
        let link = r.link(1, 0);
        r.nodes[1].set_half_open(link);
        r.nodes[1].receive(source, 0, message.clone(), r.now);
        r.nodes[1].receive(source, 0, message, r.now + Duration::from_secs(2));
        assert_eq!(r.nodes[1].pending_relay.len(), 1);
        let deadline = r.nodes[1].pending_relay[0].request.deadline;
        assert_eq!(deadline, r.now + RELAY_TIMEOUT, "duplicates do not renew expiry");
        let cancel = LinkMsg::PathCancel {
            reference: PathReference::Request(request_id),
            reason: Refusal::Closed,
        };
        r.nodes[1].receive(target, 0, cancel.clone(), r.now);
        assert_eq!(r.nodes[1].pending_relay.len(), 1, "another connection cannot cancel it");
        r.nodes[1].set_ready(link);
        assert_eq!(r.nodes[1].pending_relay[0].request.deadline, deadline);
        assert!(r.nodes[1].transit.is_empty(), "target hop is still unadmitted");
        for n in 1..=MAX_DIALS {
            r.nodes[1].receive(
                source,
                0,
                LinkMsg::PathRequest {
                    reservation: None,
                    request_id: PathRequestId {
                        source_boot: request_id.source_boot,
                        random: [n as u8; 16],
                    },
                    expected_target_boot: None,
                    target: DeviceId([3; 32]),
                    round: 1,
                },
                r.now,
            );
        }
        assert_eq!(r.nodes[1].pending_relay.len(), MAX_DIALS);
        match finish {
            "expiry" => r.nodes[1].relay_tick(deadline),
            "cancel" => {
                r.nodes[1].receive(source, 0, cancel, r.now);
                assert!(r.nodes[1].pending_relay.iter().all(|p| p.request.id != request_id));
                r.nodes[1].relay_tick(deadline);
            }
            "lost" => r.nodes[1].connection_lost(source, r.now),
            "revoked" => r.nodes[1].retire_authority(link),
            _ => unreachable!(),
        }
        assert!(r.nodes[1].pending_relay.is_empty());
        assert!(r.nodes[1].transit.is_empty());
    }
}

#[test]
fn relay_requires_ready_direct_neighbors_and_installs_one_hop() {
    let mut r = Rig::new(3);
    r.relay();
    for (a, b) in [(0, 2), (2, 0)] {
        assert_eq!(
            r.nodes[a].links[&r.link(a, b)].path(),
            LinkPath::Relayed { via: r.peer(1) }
        );
    }
    assert_eq!(r.nodes[1].transit.len(), 1);
    let id = r.link(0, 2);
    let hop = r.nodes[0].links[&id].hop;
    assert!(r.nodes[1].relay_route(hop.connection_id, hop.route_tag).is_some());
    r.nodes[0].tick(r.now);
    r.pump();
    assert!(r.nodes[0].links[&id].rtt().is_some());
}
#[test]
fn cutover_preserves_link_and_waits_for_both_drain_reports() {
    let mut r = Rig::new(3);
    r.relay();
    r.ready(0, 2);
    let a = r.link(0, 2);
    let b = r.link(2, 0);
    r.connect(2, 0);
    r.pump();
    assert!(r.nodes[0].links[&a].frozen());
    assert!(r.nodes[2].links[&b].frozen());
    r.nodes[0].drained(a);
    r.pump();
    assert!(r.nodes[0].links[&a].frozen());
    r.nodes[2].drained(b);
    r.pump();
    for (local, id) in [(0, a), (2, b)] {
        assert_eq!(r.nodes[local].links[&id].path(), LinkPath::Direct);
        assert!(!r.nodes[local].links[&id].frozen());
    }
    assert!(r.nodes[1].transit.is_empty());
    assert_eq!(
        r.observed
            .iter()
            .filter(|(_, e)| matches!(e, Effect::PathChanged { .. }))
            .count(),
        2
    );
    assert_eq!(
        r.observed
            .iter()
            .filter(|(_, e)| matches!(e, Effect::Connected(_)))
            .count(),
        6
    );
}
#[test]
fn old_n0_retirement_before_confirmation_preserves_committed_cutover() {
    let mut r = Rig::new(3);
    r.relay();
    r.ready(0, 2);
    let a = r.link(0, 2);
    let b = r.link(2, 0);
    let old = r.nodes[0].links[&a].hop;
    let direct = r.connect(0, 2);
    r.pump();
    let wire = r.nodes[0].links[&a].cutover.as_ref().unwrap().wire;
    r.nodes[0].drained(a);
    r.nodes[2].drained(b);
    r.nodes[0].receive(old.connection_id, old.route_tag, LinkMsg::MigrateDrained(wire), r.now);
    r.nodes[2].receive(direct, 0, LinkMsg::MigrateCommit(wire), r.now);
    assert_eq!(r.nodes[2].links[&b].path(), LinkPath::Direct);
    // Deliver both old N0 half closures before delivering F's queued confirmation.
    for _ in 0..2 {
        r.nodes[0].path_internal_closed(old.connection_id, old.route_tag);
        assert!(r.nodes[0].links[&a].frozen());
    }
    r.nodes[0].receive(direct, 0, LinkMsg::MigrateCommitted(wire), r.now);
    assert_eq!(r.nodes[0].links[&a].path(), LinkPath::Direct);
    assert!(!r.nodes[0].effects.iter().any(|e| matches!(e, Effect::Lost { .. })));
}

#[test]
fn old_n0_retirement_does_not_hide_cutover_failures() {
    for failure in 0..4 {
        let mut r = Rig::new(3);
        r.relay();
        r.ready(0, 2);
        let a = r.link(0, 2);
        let old = r.nodes[0].links[&a].hop;
        let direct = r.connect(0, 2);
        r.pump();
        let wire = r.nodes[0].links[&a].cutover.as_ref().unwrap().wire;
        if failure != 0 {
            r.nodes[0].drained(a);
            r.nodes[0].receive(old.connection_id, old.route_tag, LinkMsg::MigrateDrained(wire), r.now);
        }
        r.nodes[0].path_internal_closed(old.connection_id, old.route_tag);
        match failure {
            0 => {} // Before commit, old N0 is still required.
            1 => r.nodes[0].connection_lost(direct, r.now),
            2 => r.nodes[0].connection_lost(old.connection_id, r.now),
            _ => r.nodes[0].tick(r.now + CUTOVER_TIMEOUT),
        }
        assert!(!r.nodes[0].links.contains_key(&a));
        assert!(r.nodes[0].effects.iter().any(|e| {
            let Effect::Lost { link, reason, .. } = e else {
                return false;
            };
            *link == a
                && match failure {
                    // Losing the relay connection can first retire either its
                    // direct-neighbor link or this dependent relayed link.
                    2 => matches!(reason, LinkLost::RelayLost | LinkLost::ConnectionLost),
                    3 => *reason == LinkLost::CutoverTimeout,
                    _ => *reason == LinkLost::ConnectionLost,
                }
        }));
    }
}

#[test]
fn cutover_timeout_loses_link_and_new_connection() {
    let mut r = Rig::new(3);
    r.relay();
    r.ready(0, 2);
    let id = r.link(0, 2);
    let direct = r.connect(0, 2);
    r.pump();
    r.nodes[0].tick(r.now + CUTOVER_TIMEOUT);
    assert!(!r.nodes[0].links.contains_key(&id));
    assert!(
        r.nodes[0]
            .effects
            .iter()
            .any(|e| matches!(e,Effect::CloseConnection(c) if *c == direct))
    );
    assert!(r.nodes[0].effects.iter().any(|e| matches!(
        e,
        Effect::Lost {
            reason: LinkLost::CutoverTimeout,
            ..
        }
    )));
}
#[test]
fn dial_cap_and_failed_dial_leave_no_retained_target() {
    assert_eq!(MAX_DIALS, 8);
    let mut r = Rig::new((MAX_DIALS + 2) as u8);
    let addresses = vec!["127.0.0.1:9".parse().unwrap()];
    for i in 1..=MAX_DIALS {
        let p = r.peer(i);
        r.nodes[0]
            .dial(p, addresses.clone(), DialMode::DirectOnly, DialOrigin::Initial, r.now)
            .unwrap();
    }
    let ninth = r.peer(MAX_DIALS + 1);
    assert_eq!(
        r.nodes[0].dial(ninth, addresses.clone(), DialMode::All, DialOrigin::Initial, r.now),
        Err(DialRefused::Busy)
    );
    let p = r.peer(1);
    r.nodes[0].dial_done(p, r.now);
    assert!(!r.nodes[0].dials.contains_key(&p));
    assert!(r.nodes[0].peer_link(p).is_none());
    r.nodes[0]
        .dial(ninth, addresses, DialMode::DirectOnly, DialOrigin::Initial, r.now)
        .unwrap();
    assert_eq!(r.nodes[0].dials.len(), MAX_DIALS);
}

#[test]
fn loser_authentication_and_close_callbacks_in_both_orders_never_notify() {
    for close_first in [false, true] {
        let mut r = Rig::new(2);
        let winner = r.connect(0, 1);
        r.pump();
        let loser = ConnectionId {
            initiator_boot: r.nodes[1].boot,
            random: rand::random(),
        };
        r.connections.insert(loser, (0, 1));
        for (local, remote) in [(0, 1), (1, 0)] {
            let boot = r.nodes[remote].boot;
            if close_first {
                r.nodes[local].connection_lost(loser, r.now);
            }
            r.nodes[local].authenticated(boot, loser, r.now);
            if !close_first {
                r.nodes[local].connection_lost(loser, r.now);
            }
        }
        r.pump();
        for (local, remote) in [(0, 1), (1, 0)] {
            assert_eq!(r.nodes[local].links[&r.link(local, remote)].hop.connection_id, winner);
        }
        assert_eq!(
            r.observed
                .iter()
                .filter(|(_, e)| matches!(e, Effect::Connected(_)))
                .count(),
            2
        );
        assert!(!r.observed.iter().any(|(_, e)| matches!(e, Effect::Lost { .. })));
    }
}

#[test]
fn selected_direct_candidate_expires_when_pending_relay_installation_is_lost() {
    let mut r = Rig::new(3);
    r.connect(0, 1);
    r.connect(1, 2);
    r.pump();
    r.ready(0, 1);
    r.ready(1, 2);
    let target = r.peer(2);
    r.nodes[0]
        .dial(target, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
        .unwrap();
    let mut held = Vec::new();
    r.pump_holding(&mut held, |_, effect| {
        matches!(effect, Effect::Control { message, .. } if matches!(message.as_ref(), LinkMsg::PathInstall(_)))
    });
    let relay = r.link(0, 2);
    assert!(!r.nodes[0].links[&relay].installed);
    // A direct candidate is selected before the earlier relay installation
    // completes. It waits for that installation, without a cutover yet.
    let direct = r.connect(2, 0);
    r.pump();
    assert!(r.nodes[0].selected_connection_id(direct));
    assert!(r.nodes[2].selected_connection_id(direct));
    assert!(r.nodes[0].links[&relay].cutover.is_none());
    r.nodes[0].lose(relay, LinkLost::HeartbeatTimeout);
    r.nodes[0].effects.clear();
    r.nodes[2].effects.clear();
    r.now += DIAL_BUDGET;
    for node in [0, 2] {
        r.nodes[node].tick(r.now);
        assert!(
            !r.nodes[node].candidates.contains_key(&direct),
            "selected candidate without a path must retain its setup deadline; node={node}"
        );
        assert!(
            r.nodes[node]
                .effects
                .iter()
                .any(|effect| { matches!(effect, Effect::CloseConnection(connection) if *connection == direct) })
        );
    }
    // Deliver the executor's close callbacks, then show that the stale
    // selection cannot reject the next authenticated connection as superseded.
    for node in [0, 2] {
        r.nodes[node].connection_lost(direct, r.now);
    }
    r.pump();
    let replacement = r.connect(2, 0);
    r.pump();
    for (local, remote) in [(0, 2), (2, 0)] {
        let link = &r.nodes[local].links[&r.link(local, remote)];
        assert!(link.installed);
        assert_eq!(link.hop.connection_id, replacement);
        assert_eq!(link.path(), LinkPath::Direct);
    }
}

#[test]
fn executor_finishes_direct_candidates_and_loss_closes_the_selected_connection() {
    let mut r = Rig::new(2);
    let peer = r.peer(1);
    let addresses = vec!["127.0.0.1:1".parse().unwrap(), "127.0.0.1:2".parse().unwrap()];
    r.nodes[0]
        .dial(
            peer,
            addresses.clone(),
            DialMode::DirectOnly,
            DialOrigin::Initial,
            r.now,
        )
        .unwrap();
    assert!(
        r.nodes[0]
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Dial { addresses: got, .. } if *got == addresses))
    );
    r.nodes[0].tick(r.now + DIAL_BUDGET);
    assert!(r.nodes[0].dials.contains_key(&peer));
    r.nodes[0].dial_done(peer, r.now + DIAL_BUDGET);
    assert!(
        r.nodes[0]
            .effects
            .iter()
            .any(|e| matches!(e, Effect::DialFailed(p, _) if *p == peer))
    );
    let connection = r.connect(0, 1);
    r.pump();
    let link = r.link(0, 1);
    r.nodes[0].lose(link, LinkLost::HeartbeatTimeout);
    assert!(
        r.nodes[0]
            .effects
            .iter()
            .any(|e| matches!(e, Effect::CloseConnection(c) if *c == connection))
    );
}

#[test]
fn changed_peer_boot_retires_old_link_and_starts_selection_at_one() {
    let mut r = Rig::new(2);
    r.connect(0, 1);
    r.pump();
    let old = r.link(0, 1);
    let mut boot = r.nodes[1].boot;
    boot.nonce = [99; 16];
    let connection = ConnectionId {
        initiator_boot: boot,
        random: rand::random(),
    };
    r.nodes[0].authenticated(boot, connection, r.now);
    assert!(!r.nodes[0].links.contains_key(&old));
    assert!(
        r.nodes[0]
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Lost { link, reason: LinkLost::PeerRestarted, .. } if *link == old))
    );
    assert!(r.nodes[0].effects.iter().any(|e| matches!(e, Effect::Control { message, .. } if matches!(message.as_ref(), LinkMsg::ConnectionSelect { connection_id, choice_counter: 1, .. } if *connection_id == connection))));
}

#[test]
fn all_mode_relay_arrival_keeps_direct_slot_until_its_budget_finishes() {
    let mut r = Rig::new(3);
    r.connect(0, 1);
    r.connect(1, 2);
    r.pump();
    r.ready(0, 1);
    r.ready(1, 2);
    r.observed.clear();
    let peer = r.peer(2);
    r.nodes[0]
        .dial(
            peer,
            vec!["127.0.0.1:9".parse().unwrap()],
            DialMode::All,
            DialOrigin::Initial,
            r.now,
        )
        .unwrap();
    r.pump();
    assert_eq!(
        r.nodes[0].links[&r.link(0, 2)].path(),
        LinkPath::Relayed { via: r.peer(1) }
    );
    assert!(r.nodes[0].dials.contains_key(&peer));
    assert!(
        !r.observed
            .iter()
            .any(|(n, e)| *n == 0 && matches!(e, Effect::DialFinished(p) if *p == peer))
    );
    r.nodes[0].dial_done(peer, r.now + DIAL_BUDGET);
    assert!(!r.nodes[0].dials.contains_key(&peer));
    assert!(
        r.nodes[0]
            .effects
            .iter()
            .any(|e| matches!(e, Effect::DialFinished(p) if *p == peer))
    );
    let failures = r.nodes[0].links[&r.link(0, 2)].direct_failures;
    r.nodes[0].dial_done(peer, r.now + DIAL_BUDGET);
    assert_eq!(r.nodes[0].links[&r.link(0, 2)].direct_failures, failures);
}

#[test]
fn direct_arrival_cancels_an_outstanding_all_mode_relay_request() {
    let mut r = Rig::new(3);
    r.connect(0, 1);
    r.connect(1, 2);
    r.pump();
    r.ready(0, 1);
    r.ready(1, 2);
    r.observed.clear();
    let target = r.peer(2);
    r.nodes[0]
        .dial(
            target,
            vec!["127.0.0.1:9".parse().unwrap()],
            DialMode::All,
            DialOrigin::Initial,
            r.now,
        )
        .unwrap();
    // Direct authentication completes while relay requests are still in flight.
    r.connect(0, 2);
    r.pump();
    assert_eq!(r.nodes[0].links[&r.link(0, 2)].path(), LinkPath::Direct);
    assert_eq!(r.nodes[2].links[&r.link(2, 0)].path(), LinkPath::Direct);
    assert!(r.nodes[1].transit.is_empty());
    assert_eq!(
        r.observed
            .iter()
            .filter(|(_, e)| matches!(e, Effect::Connected(_)))
            .count(),
        2
    );
    assert!(!r.observed.iter().any(|(_, e)| matches!(e, Effect::Lost { .. })));
}

/// A relayed link retries the direct path 15 s after it was
/// installed, then 15, 30, 60, 60… s after each failed attempt. The config module calls
/// `update_addresses` for every discovery sighting (every 2 s per peer) and for every
/// device-facts message (every 5 s), with the same addresses as before.
#[test]
fn unchanged_address_updates_keep_the_direct_retry_schedule_of_a_relayed_link() {
    let mut r = Rig::new(3);
    r.relay();
    let target = r.peer(2);
    let link = r.link(0, 2);
    assert!(matches!(r.nodes[0].links[&link].path(), LinkPath::Relayed { .. }));
    r.observed.clear();
    let address: SocketAddr = "192.0.2.7:47800".parse().unwrap();
    let start = r.now;
    let mut next_sighting = start;
    while r.now < start + Duration::from_secs(60) {
        if r.now >= next_sighting {
            // The same unreachable address every time: nothing new was learned.
            r.nodes[0].update_addresses(target, vec![address], r.now);
            next_sighting = r.now + Duration::from_secs(2);
        }
        for n in 0..3 {
            let now = r.now;
            r.nodes[n].tick(now);
        }
        r.pump();
        r.now += Duration::from_millis(100);
    }
    assert!(r.nodes[0].links.contains_key(&link), "the relayed link itself stays up");
    let dials = r
        .observed
        .iter()
        .filter(|(n, e)| *n == 0 && matches!(e, Effect::Dial { peer, .. } if *peer == target))
        .count();
    // One attempt for the first address, then the 15 s, 15 s and 30 s steps fit in a minute.
    assert!(
        dials <= 4,
        "{dials} direct attempts in 60 s on a relayed link whose address never changed"
    );
}

#[test]
fn migration_trace_names_all_waits_on_each_side_without_poll_records() {
    super::super::diagnostics::tests::init();
    use tracing_subscriber::layer::SubscriberExt;
    let rows = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(super::super::diagnostics::tests::Capture(rows.clone()));
    tracing::subscriber::with_default(subscriber, || {
        let mut r = Rig::new(3);
        r.relay();
        r.ready(0, 2);
        let a = r.link(0, 2);
        let b = r.link(2, 0);
        r.connect(0, 2);
        r.pump();
        r.nodes[0].drain_counts(a, 2, 1);
        assert_eq!(
            r.nodes[0].links[&a].cutover.as_ref().unwrap().waiting(true),
            "local_drain"
        );
        r.nodes[0].drained(a);
        assert_eq!(
            r.nodes[0].links[&a].cutover.as_ref().unwrap().waiting(true),
            "peer_drained"
        );
        let before = rows.lock().unwrap().len();
        r.nodes[0].drained(a);
        r.nodes[0].drained(a);
        assert_eq!(rows.lock().unwrap().len(), before);
        r.nodes[2].drained(b);
        assert_eq!(
            r.nodes[2].links[&b].cutover.as_ref().unwrap().waiting(false),
            "peer_commit"
        );
        let wire = r.nodes[0].links[&a].cutover.as_ref().unwrap().wire;
        let hop = r.nodes[0].links[&a].hop;
        r.nodes[0].receive(hop.connection_id, hop.route_tag, LinkMsg::MigrateDrained(wire), r.now);
        assert_eq!(
            r.nodes[0].links[&a].cutover.as_ref().unwrap().waiting(true),
            "peer_committed"
        );
        r.nodes[0].tick(r.now + CUTOVER_TIMEOUT);
    });
    let rows = rows.lock().unwrap();
    assert!(
        rows.iter()
            .any(|row| row.contains(&("reason".into(), "cutover_timeout".into()))
                && row.contains(&("waiting".into(), "peer_committed".into()))
                && row.contains(&("peer_drained".into(), "true".into()))),
        "all rows: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains(&("network_path".into(), "relay".into()))
                && row.iter().any(|(key, _)| key == "via_id")),
        "path rows: {rows:?}"
    );
}

#[test]
fn selection_restart_after_all_old_candidates_are_gone() {
    for relay_state in ["none", "ready", "pending"] {
        let mut r = Rig::new(3);
        // Preserve a real watermark, but remove every object that used to carry
        // its peer boot. No elapsed-time assumption or fabricated counter.
        for _ in 0..6 {
            let connection = r.connect(0, 2);
            r.pump();
            assert!(r.nodes[2].selected_connection_id(connection));
            r.nodes[0].connection_lost(connection, r.now);
            r.nodes[2].connection_lost(connection, r.now);
            r.pump();
        }
        let mut boot = r.nodes[0].boot;
        boot.nonce = [99; 16];
        r.nodes[0] = Core::new(boot);
        if relay_state == "ready" {
            r.relay();
        } else if relay_state == "pending" {
            r.connect(0, 1);
            r.connect(1, 2);
            r.pump();
            r.ready(0, 1);
            r.ready(1, 2);
            let peer = r.peer(2);
            r.nodes[0]
                .dial(peer, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
                .unwrap();
            // Leave the relay request in flight until both candidates exist.
        }
        // Both dial directions authenticate before any selection is delivered.
        let first = r.connect(2, 0);
        r.connect(0, 2);
        r.pump();
        assert!(r.nodes[0].selected_connection_id(first), "relay_state={relay_state}");
        assert!(r.nodes[2].selected_connection_id(first), "relay_state={relay_state}");
    }
}

#[test]
fn selection_watermark_survives_loss_in_the_same_boot() {
    let mut r = Rig::new(2);
    for _ in 0..3 {
        let connection = r.connect(0, 1);
        r.pump();
        r.nodes[0].connection_lost(connection, r.now);
        r.nodes[1].connection_lost(connection, r.now);
        r.pump();
    }
    let connection = r.connect(0, 1);
    let pair = BootPair::new(r.nodes[0].boot, r.nodes[1].boot).unwrap();
    r.nodes[1].receive(
        connection,
        0,
        LinkMsg::ConnectionSelect {
            boot_pair: pair,
            connection_id: connection,
            choice_counter: 1,
        },
        r.now,
    );
    assert!(!r.nodes[1].selected_connection_id(connection));
    assert!(r.nodes[1].effects.is_empty(), "a stale offer must not be acknowledged");
    assert_eq!(r.nodes[0].selection_wait(connection), ("connection_selected", "peer"));
    assert_eq!(r.nodes[1].selection_wait(connection), ("connection_select", "peer"));
    r.pump();
    assert!(r.nodes[0].selected_connection_id(connection));
    assert!(r.nodes[1].selected_connection_id(connection));
}

#[test]
fn selected_direct_candidate_does_not_freeze_an_unfinished_config_handshake() {
    let mut r = Rig::new(3);
    r.relay();
    let a = r.link(0, 2);
    let b = r.link(2, 0);
    r.nodes[0].set_half_open(a);
    r.nodes[2].set_half_open(b);
    r.connect(0, 2);
    r.pump();
    // Reply/Done still need the old C0 stream. Freezing now prevents admission
    // from completing and can leave business readers waiting for Ready forever.
    assert!(!r.nodes[0].links[&a].frozen());
    assert!(!r.nodes[2].links[&b].frozen());
    r.ready(0, 2);
    r.pump();
    assert!(r.nodes[0].links[&a].frozen());
    assert!(r.nodes[2].links[&b].frozen());
    r.nodes[0].drained(a);
    r.nodes[2].drained(b);
    r.pump();
    assert_eq!(r.nodes[0].links[&a].path(), LinkPath::Direct);
    assert_eq!(r.nodes[2].links[&b].path(), LinkPath::Direct);
}

#[test]
fn selected_direct_candidate_waits_for_pending_relay_installation() {
    let mut r = Rig::new(3);
    r.connect(0, 1);
    r.connect(1, 2);
    r.pump();
    r.ready(0, 1);
    r.ready(1, 2);
    let peer = r.peer(2);
    r.nodes[0]
        .dial(peer, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
        .unwrap();
    let mut held = vec![];
    r.pump_holding(&mut held, |n, effect| {
        n == 1
            && matches!(effect,
        Effect::Control { message, .. } if matches!(message.as_ref(), LinkMsg::PathInstalled(_)))
    });
    assert_eq!(held.len(), 1);
    let a = r.link(0, 2);
    let b = r.link(2, 0);
    assert!(!r.nodes[0].links[&a].installed);
    assert!(r.nodes[2].links[&b].installed);
    let direct = r.connect(0, 2);
    r.connect(2, 0);
    r.pump();
    assert!(r.nodes[0].selected_connection_id(direct));
    assert!(r.nodes[2].selected_connection_id(direct));
    assert!(r.nodes[0].links[&a].cutover.is_none());
    for (n, effect) in held {
        r.nodes[n].effects.push_back(effect);
    }
    r.pump();
    assert!(r.nodes[0].links[&a].cutover.is_none());
    r.ready(0, 2);
    assert!(
        r.nodes[0].links[&a].cutover.is_some(),
        "relay admission must reconcile the selected direct candidate"
    );
    r.nodes[0].drained(a);
    r.nodes[2].drained(b);
    r.pump();
    assert_eq!(r.nodes[0].links[&a].path(), LinkPath::Direct);
    assert_eq!(r.nodes[2].links[&b].path(), LinkPath::Direct);
}

#[test]
fn simultaneous_relay_discovery_obeys_the_pair_arbiter() {
    let mut r = Rig::new(3);
    r.connect(0, 1);
    r.connect(1, 2);
    r.pump();
    r.ready(0, 1);
    r.ready(1, 2);
    for (local, remote) in [(0, 2), (2, 0)] {
        let peer = r.peer(remote);
        r.nodes[local]
            .dial(peer, vec![], DialMode::All, DialOrigin::Initial, r.now)
            .unwrap();
    }
    r.pump();
    let a = r.link(0, 2);
    let b = r.link(2, 0);
    assert!(r.nodes[0].links[&a].installed);
    assert!(r.nodes[2].links[&b].installed);
    assert_eq!(r.nodes[0].links[&a].wire.path_id, r.nodes[2].links[&b].wire.path_id);
}

#[test]
fn retiring_direct_member_revokes_existing_transit_but_preserves_its_c0_link() {
    let mut rig = Rig::new(3);
    rig.relay();
    let direct = rig.link(1, 0);
    let relay = rig.link(0, 2);
    let hop = rig.nodes[0].links[&relay].hop;
    assert!(rig.nodes[1].relay_route(hop.connection_id, hop.route_tag).is_some());
    rig.nodes[1].retire_authority(direct);
    assert!(rig.nodes[1].links.contains_key(&direct));
    assert_eq!(rig.nodes[1].inbound(direct), Err(StreamCode::NotReady));
    assert!(rig.nodes[1].transit.is_empty());
    assert!(rig.nodes[1].relay_route(hop.connection_id, hop.route_tag).is_none());
    rig.pump();
    assert!(rig.nodes[0].peer_link(rig.peer(2)).is_none());
    assert!(rig.nodes[2].peer_link(rig.peer(0)).is_none());
}

#[test]
fn fresh_addresses_join_a_pending_relay_search_once() {
    let mut r = Rig::new(2);
    let target = r.peer(1);
    r.nodes[0]
        .dial(target, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
        .unwrap();
    let deadline = r.nodes[0].dials[&target].deadline;
    r.nodes[0].effects.clear();
    let first: SocketAddr = "127.0.0.1:47001".parse().unwrap();
    let second: SocketAddr = "127.0.0.1:47002".parse().unwrap();
    for addresses in [vec![first], vec![first], vec![first, second], vec![second, first]] {
        r.nodes[0].update_addresses(target, addresses, r.now);
    }
    let candidates = r.nodes[0]
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Dial { peer, addresses, .. } if *peer == target => Some(addresses.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(candidates, vec![vec![first], vec![second]]);
    assert_eq!(r.nodes[0].dials[&target].deadline, deadline);
    assert!(r.nodes[0].dials[&target].direct);
}

#[test]
fn relay_search_expiry_during_path_install_still_reports_dial_failure() {
    let mut r = Rig::new(3);
    r.connect(0, 1);
    r.connect(1, 2);
    r.pump();
    r.ready(0, 1);
    r.ready(1, 2);
    r.observed.clear();
    let peer = r.peer(2);
    r.nodes[0]
        .dial(peer, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
        .unwrap();
    let search_deadline = r.now + RELAY_TIMEOUT;
    // Installation starts after the relay search, so its acknowledgement can
    // remain in flight when the earlier search budget expires.
    r.now += Duration::from_millis(100);
    let mut held = Vec::new();
    r.pump_holding(&mut held, |n, e| {
        n == 0 && matches!(e, Effect::Control { message, .. } if matches!(message.as_ref(), LinkMsg::PathInstall(_)))
    });
    let link = r.link(0, 2);
    assert!(!r.nodes[0].links[&link].installed);
    let install_deadline = r.nodes[0].links[&link].installation_deadline;
    assert!(search_deadline < install_deadline);
    // Other admitted neighbors keep answering heartbeats throughout this queue.
    for l in r.nodes[0].links.values_mut().filter(|l| l.installed) {
        l.heartbeat.progress = install_deadline;
    }
    r.nodes[0].tick(search_deadline);
    r.nodes[0].tick(install_deadline);
    assert!(r.nodes[0].peer_link(peer).is_none());
    assert_eq!(
        r.nodes[0]
            .effects
            .iter()
            .filter(|e| matches!(e, Effect::DialFailed(p, _) if *p == peer))
            .count(),
        1,
        "an uninstalled path must not swallow the dial result and strand its caller active"
    );
    assert!(
        !r.observed
            .iter()
            .any(|(n, e)| *n == 0 && matches!(e, Effect::Connected(_)))
    );
    // The caller can now consume DialFailed and use its normal retry schedule.
    // No stale dial/candidate reservation may prevent that next real attempt.
    r.now = install_deadline;
    r.nodes[0]
        .dial(peer, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
        .unwrap();
    r.pump();
    assert!(r.nodes[0].links[&r.link(0, 2)].installed);
    assert!(r.nodes[2].links[&r.link(2, 0)].installed);
}

#[test]
fn exhausted_candidates_during_path_install_still_report_dial_failure() {
    let mut r = Rig::new(2);
    let peer = r.peer(1);
    r.nodes[0]
        .dial(
            peer,
            vec!["127.0.0.1:9".parse().unwrap()],
            DialMode::DirectOnly,
            DialOrigin::Initial,
            r.now,
        )
        .unwrap();
    // A reverse incoming candidate wins selection while the outgoing
    // candidates finish. Path installation has not acknowledged success yet.
    r.connect(1, 0);
    let mut held = Vec::new();
    r.pump_holding(&mut held, |n, e| {
        n == 0 && matches!(e, Effect::Control { message, .. } if matches!(message.as_ref(), LinkMsg::PathInstall(_)))
    });
    let link = r.link(0, 1);
    assert!(!r.nodes[0].links[&link].installed);
    r.nodes[0].dial_done(peer, r.now);
    r.nodes[0].tick(r.now + DIAL_BUDGET);
    assert!(r.nodes[0].peer_link(peer).is_none());
    assert_eq!(
        r.nodes[0]
            .effects
            .iter()
            .filter(|e| matches!(e, Effect::DialFailed(p, _) if *p == peer))
            .count(),
        1,
        "finishing TLS candidates cannot finish a dial before the path exists"
    );
    assert!(
        !r.observed
            .iter()
            .any(|(n, e)| *n == 0 && matches!(e, Effect::Connected(_)))
    );
}

#[test]
fn exhausted_candidates_during_path_install_can_still_connect() {
    let mut r = Rig::new(2);
    let peer = r.peer(1);
    r.nodes[0]
        .dial(
            peer,
            vec!["127.0.0.1:9".parse().unwrap()],
            DialMode::DirectOnly,
            DialOrigin::Initial,
            r.now,
        )
        .unwrap();
    r.now += Duration::from_millis(100);
    r.connect(1, 0);
    let mut held = Vec::new();
    r.pump_holding(&mut held, |n, e| {
        n == 0 && matches!(e, Effect::Control { message, .. } if matches!(message.as_ref(), LinkMsg::PathInstall(_)))
    });
    let link = r.link(0, 1);
    r.nodes[0].dial_done(peer, r.now);
    // The old dial budget can finish before the path's existing installation
    // budget. Neither fail it early nor spin on that obsolete dial deadline.
    r.now += DIAL_BUDGET - Duration::from_millis(50);
    r.nodes[0].links.get_mut(&link).unwrap().heartbeat.progress = r.now;
    r.nodes[0].tick(r.now);
    assert!(r.nodes[0].links.contains_key(&link));
    assert!(r.nodes[0].next_deadline().unwrap() > r.now);
    assert!(r.nodes[0].dials.contains_key(&peer));
    for (_, effect) in held {
        let Effect::Control {
            connection,
            tag,
            message,
        } = effect
        else {
            unreachable!()
        };
        r.nodes[1].receive(connection, tag, *message, r.now);
    }
    r.pump();
    assert!(r.nodes[0].links[&link].installed);
    assert!(!r.nodes[0].dials.contains_key(&peer));
    assert_eq!(
        r.observed
            .iter()
            .filter(|(n, e)| *n == 0 && matches!(e, Effect::Connected(id) if *id == link))
            .count(),
        1
    );
    assert!(
        !r.observed
            .iter()
            .any(|(n, e)| *n == 0 && matches!(e, Effect::DialFailed(p, _) if *p == peer))
    );
}

#[test]
fn dial_queued_during_inbound_path_install_receives_a_result() {
    for completes in [false, true] {
        let mut r = Rig::new(2);
        let peer = r.peer(1);
        // Installing paths are absent from LinkLayer's public snapshot, so an
        // ordinary owner can reserve and queue its dial before Connected exists.
        r.connect(1, 0);
        let mut held = Vec::new();
        r.pump_holding(&mut held, |n, e| {
            n == 0
                && matches!(e, Effect::Control { message, .. } if matches!(message.as_ref(), LinkMsg::PathInstall(_)))
        });
        let link = r.link(0, 1);
        assert!(!r.nodes[0].links[&link].installed);
        assert!(!r.nodes[0].dials.contains_key(&peer));
        r.nodes[0]
            .dial(peer, vec![], DialMode::PreferDirect, DialOrigin::Initial, r.now)
            .unwrap();
        if completes {
            for (_, effect) in held {
                let Effect::Control {
                    connection,
                    tag,
                    message,
                } = effect
                else {
                    unreachable!()
                };
                r.nodes[1].receive(connection, tag, *message, r.now);
            }
            r.pump();
            assert!(
                r.observed
                    .iter()
                    .any(|(n, e)| *n == 0 && matches!(e, Effect::Connected(id) if *id == link))
            );
        } else {
            r.nodes[0].tick(r.now + DIAL_BUDGET);
            assert!(
                r.nodes[0]
                    .effects
                    .iter()
                    .any(|e| matches!(e, Effect::DialFailed(p, _) if *p == peer))
            );
        }
        assert!(!r.nodes[0].dials.contains_key(&peer));
    }
}

#[test]
fn heartbeat_is_half_second_and_existing_loss_deadline_is_two_seconds_on_direct_and_relay() {
    assert_eq!(HEARTBEAT_INTERVAL, Duration::from_millis(500));
    assert_eq!(HEARTBEAT_TIMEOUT, Duration::from_secs(2));
    for relayed in [false, true] {
        let mut r = Rig::new(if relayed { 3 } else { 2 });
        if relayed {
            r.relay();
        } else {
            r.connect(0, 1);
            r.pump();
        }
        let link = r.link(0, if relayed { 2 } else { 1 });
        let started = r.now;
        let path = r.nodes[0].links[&link].wire.path_id;
        let step = |r: &mut Rig, at| {
            r.now = at;
            for node in &mut r.nodes {
                node.tick(at);
            }
            // Lose only this end-to-end Pong. Relay underlay links keep their
            // own matched answers; their independent loss must not win this test.
            r.pump_holding(&mut vec![], |_, effect| matches!(effect,
                Effect::Control { message, .. } if matches!(message.as_ref(), LinkMsg::Pong(pong) if pong.path_id == path)));
        };
        step(&mut r, started);
        assert_eq!(r.nodes[0].links[&link].heartbeat.nonce, 1);
        step(&mut r, started + Duration::from_millis(499));
        assert_eq!(r.nodes[0].links[&link].heartbeat.nonce, 1);
        step(&mut r, started + Duration::from_millis(500));
        assert_eq!(r.nodes[0].links[&link].heartbeat.nonce, 2);
        step(&mut r, started + Duration::from_millis(1999));
        assert!(r.nodes[0].links.contains_key(&link));
        r.nodes[0].tick(started + Duration::from_secs(2));
        assert!(!r.nodes[0].links.contains_key(&link));
        assert!(r.nodes[0].effects.iter().any(|effect| matches!(effect,
            Effect::Lost { link: lost, reason: LinkLost::HeartbeatTimeout, .. } if *lost == link)));
    }
}

#[test]
fn lost_decision_identifies_binding_and_distinguishes_unobserved_from_recent_data() {
    super::super::diagnostics::tests::init();
    use tracing_subscriber::layer::SubscriberExt;
    let rows = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(super::super::diagnostics::tests::Capture(rows.clone()));
    tracing::subscriber::with_default(subscriber, || {
        let mut r = Rig::new(2);
        r.connect(0, 1);
        r.pump();
        let id = r.link(0, 1);
        let connection = r.nodes[0].links[&id].hop.connection_id;
        let progress = r.nodes[0].links[&id].heartbeat.progress;
        let binding = r.nodes[0].links[&id].hop;
        let wire = r.nodes[0].links[&id].wire;
        // A received Pulse is observable without counting as a matched Pong.
        r.nodes[0].receive(
            connection,
            binding.route_tag,
            LinkMsg::Pulse(Pulse {
                path_id: wire.path_id,
                route_revision: wire.route_revision,
                nonce: 77,
            }),
            r.now,
        );
        r.nodes[0].peer_data_received(id, r.now);
        assert_eq!(
            r.nodes[0].links[&id].heartbeat.progress, progress,
            "observations cannot renew liveness"
        );
        r.nodes[0].lose(id, LinkLost::HeartbeatTimeout);
        let other = r.link(1, 0);
        r.nodes[1].lose(other, LinkLost::ConnectionLost);
        let logs = rows.lock().unwrap();
        let row = logs
            .iter()
            .find(|row| row.contains(&("kind".into(), "link_lost".into())))
            .unwrap();
        for (key, value) in [
            ("reason", "heartbeat_timeout".to_string()),
            ("decision_side", "local".to_string()),
            ("operation", super::super::diagnostics::hex(&connection.random)),
            ("link_id", id.0.to_string()),
            ("peer_data_known", "true".to_string()),
        ] {
            assert!(row.contains(&(key.into(), value)), "{row:?}");
        }
        assert!(row.iter().any(|(key, _)| key == "path_id"));
        assert!(row.iter().any(|(key, _)| key == "heartbeat_age_ms"));
        assert!(row.contains(&("heartbeat_known".into(), "true".into())));
        assert!(row.contains(&("pong_known".into(), "false".into())));
        assert!(logs.iter().any(|row| row.contains(&("kind".into(), "link_lost".into()))
            && row.contains(&("peer_data_known".into(), "false".into()))));
    });
}

/// Review M8: a standby recorded beside a selection that still waited for its
/// relay-to-direct cut-over outlived that selection and blocked every later
/// direct candidate, so the link stayed relayed for good.
#[test]
fn a_selection_waiting_for_relay_admission_keeps_no_standby() {
    let mut r = Rig::new(3);
    r.relay();
    let peer = r.peer(2);
    // The relayed link is installed but not admitted (C0 is slow); a direct
    // candidate is selected and waits for the relay-to-direct cut-over.
    let first = r.connect(0, 2);
    r.pump();
    assert!(r.nodes[0].selected_connection_id(first));
    let second = r.connect(2, 0);
    r.pump();
    assert!(!r.nodes[0].standby(peer, second));
    // The selected connection dies before the relayed link is admitted.
    let now = r.now;
    for n in [0, 2] {
        r.nodes[n].connection_lost(first, now);
    }
    r.pump();
    let third = r.connect(0, 2);
    r.pump();
    assert!(r.nodes[0].selected_connection_id(third));
    assert!(r.nodes[2].selected_connection_id(third));
}

/// Review H3, change plan 6.1: a connection that carries another path (the
/// first hop of a relayed link, or one end of a transit) is in use, and no
/// switch, automatic or manual, moves a link away from it: the source's own
/// choice and the relay's own choice remain remembered but cannot move
/// traffic until the shared-path guard clears.
#[test]
fn no_route_switch_leaves_a_relayed_path_on_a_standby() {
    let mut r = Rig::new(3);
    r.relay();
    r.ready(0, 2);
    let old = r.nodes[0].links[&r.link(0, 1)].hop.connection_id;
    let onward = r.nodes[1].links[&r.link(1, 2)].hop.connection_id;
    assert_eq!(r.nodes[0].links[&r.link(0, 2)].hop.connection_id, old);
    assert_eq!(r.nodes[1].transit.len(), 1);
    assert!(r.nodes[0].connection_in_use(old));
    assert!(r.nodes[1].connection_in_use(old) && r.nodes[1].connection_in_use(onward));
    // A second address on each hop of the relayed path, dialed by `dialer`.
    fn second(r: &mut Rig, dialer: usize, acceptor: usize, last: u8) -> (std::net::Ipv4Addr, ConnectionId) {
        let ip = std::net::Ipv4Addr::new(10, 77, 0, last);
        let id = ConnectionId {
            initiator_boot: r.nodes[dialer].boot,
            random: rand::random(),
        };
        r.connections.insert(id, (dialer, acceptor));
        let now = r.now;
        for (local, remote) in [(dialer, acceptor), (acceptor, dialer)] {
            let boot = r.nodes[remote].boot;
            r.nodes[local].authenticated_at(boot, id, Some(SocketAddr::from((ip, 47470))), RelayRole::Station, now);
        }
        r.pump();
        assert!(r.nodes[dialer].standby(r.peer(acceptor), id));
        (ip, id)
    }
    let now = r.now;
    // Node 0 (arbiter of its pair with the relay) picks the relay's second address.
    let (ip, _) = second(&mut r, 0, 1, 2);
    let relay = r.peer(1);
    assert!(r.nodes[0].set_route(relay, RouteChoice::Pin(ip), now));
    // The relay (arbiter of its pair with node 2) picks node 2's second address.
    let (ip, _) = second(&mut r, 1, 2, 3);
    let target = r.peer(2);
    assert!(r.nodes[1].set_route(target, RouteChoice::Pin(ip), now));
    // Node 2 (follower of that pair) asks for its own second connection.
    let (ip, _) = second(&mut r, 2, 1, 4);
    assert!(r.nodes[2].set_route(relay, RouteChoice::Pin(ip), now));
    r.pump();
    assert_eq!(
        r.nodes[2].route_view(r.link(2, 1)).unwrap().pin,
        Some(RouteRecord::Dial(ip))
    );
    assert_eq!(r.nodes[0].links[&r.link(0, 1)].hop.connection_id, old);
    assert_eq!(r.nodes[1].links[&r.link(1, 0)].hop.connection_id, old);
    assert_eq!(r.nodes[1].links[&r.link(1, 2)].hop.connection_id, onward);
    assert_eq!(r.nodes[1].transit.len(), 1);
    // Revoking node 0 at the relay retires the transit it carries.
    let b = r.link(1, 0);
    r.nodes[1].retire_authority(b);
    assert!(r.nodes[1].transit.is_empty());
}

#[path = "route_tests.rs"]
mod route_tests;

#[test]
fn relay_role_reads_the_peers_own_station_announcement() {
    use oink_protocol::v2::admission::Agreement;
    let caps = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
    let agreed = |ids: &[&str]| Agreement::new(caps(ids)).unwrap();
    // PC to PC: both offer the station role; it is also agreed.
    let pcs = agreed(&["oink.core/2", RELAY_ENDPOINT, RELAY_TRANSIT]);
    assert_eq!(
        RelayRole::of(&pcs, &caps(&[RELAY_ENDPOINT, RELAY_TRANSIT])),
        RelayRole::Station
    );
    // PC and phone: only the endpoint is agreed. The PC sees an endpoint;
    // the phone sees the PC's own announcement and may ask it to relay.
    let mixed = agreed(&["oink.core/2", RELAY_ENDPOINT]);
    assert_eq!(RelayRole::of(&mixed, &caps(&[RELAY_ENDPOINT])), RelayRole::Endpoint);
    assert_eq!(
        RelayRole::of(&mixed, &caps(&[RELAY_ENDPOINT, RELAY_TRANSIT])),
        RelayRole::Station
    );
    // No relay path protocol: no role, whatever the peer announces.
    let none = agreed(&["oink.core/2"]);
    assert_eq!(RelayRole::of(&none, &caps(&[RELAY_TRANSIT])), RelayRole::None);
}

#[test]
fn background_retry_failures_are_not_setup_samples() {
    use super::super::timing::{Measurement, Result as Outcome, tests::capture};
    for (origin, sampled) in [(DialOrigin::Initial, true), (DialOrigin::BackgroundRetry, false)] {
        // Unreachable at once: no candidate address for a direct-only dial.
        let mut r = Rig::new(2);
        let peer = r.peer(1);
        let events = capture(|| {
            r.nodes[0]
                .dial(peer, vec![], DialMode::DirectOnly, origin, r.now)
                .unwrap();
        });
        assert!(
            r.nodes[0]
                .effects
                .iter()
                .any(|e| matches!(e, Effect::DialFailed(p, _) if *p == peer))
        );
        assert_eq!(
            events.contains(&Measurement::Setup(Outcome::Failed)),
            sampled,
            "{origin:?} dial failure"
        );

        // The peer accepts TLS but admission never completes (a sleeping peer).
        let mut r = Rig::new(2);
        let peer = r.peer(1);
        r.nodes[0]
            .dial(
                peer,
                vec!["127.0.0.1:9".parse().unwrap()],
                DialMode::PreferDirect,
                origin,
                r.now,
            )
            .unwrap();
        r.connect(0, 1);
        r.pump();
        let link = r.link(0, 1);
        assert!(r.nodes[0].links[&link].installed);
        let events = capture(|| r.nodes[0].lose(link, LinkLost::Local(Disconnect::HandshakeTimeout)));
        assert_eq!(
            events.contains(&Measurement::Setup(Outcome::Timeout)),
            sampled,
            "{origin:?} loss before Ready"
        );
    }
}

#[test]
fn every_dial_that_reaches_ready_is_a_setup_sample() {
    use super::super::timing::{Measurement, Result as Outcome, tests::capture};
    // A member that became reachable during background retries, and an
    // initial dial (first link after pairing, or a reconnect after a lost link).
    for origin in [DialOrigin::Initial, DialOrigin::BackgroundRetry] {
        let mut r = Rig::new(2);
        let peer = r.peer(1);
        r.nodes[0]
            .dial(
                peer,
                vec!["127.0.0.1:9".parse().unwrap()],
                DialMode::PreferDirect,
                origin,
                r.now,
            )
            .unwrap();
        r.connect(0, 1);
        r.pump();
        let link = r.link(0, 1);
        let events = capture(|| r.nodes[0].set_ready(link));
        assert!(events.contains(&Measurement::Setup(Outcome::Ready)), "{origin:?}");
    }
}
