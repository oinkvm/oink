//! One-hop reservation and forwarding. Only ready direct links can relay.
use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) struct Request {
    pub id: PathRequestId,
    pub round: u64,
    pub deadline: Instant,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct Nomination {
    pub request: Request,
    pub reservation: RelayReservation,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct PendingRequest {
    connection: ConnectionId,
    pub request: Request,
    reservation: Option<RelayReservation>,
    expected_target_boot: Option<Boot>,
    target: DeviceId,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Transit {
    request: Request,
    reservation: RelayReservation,
    source: Boot,
    target: Boot,
    installed: Option<PathInstall>,
    confirmed: bool,
}
impl Transit {
    pub(super) fn deadline(&self) -> Option<Instant> {
        (!self.confirmed).then_some(self.request.deadline)
    }
    fn other(self, connection: ConnectionId) -> Option<ConnectionId> {
        if connection == self.reservation.source_connection {
            Some(self.reservation.target_connection)
        } else if connection == self.reservation.target_connection {
            Some(self.reservation.source_connection)
        } else {
            None
        }
    }
}
impl Core {
    pub(super) fn relay_neighbor_ready(&mut self, connection: ConnectionId, now: Instant) {
        // A live search also applies to newly admitted neighbors. Continue the
        // same request within its original budget; do not wait for a failed
        // attempt and the caller's reconnect backoff to discover this route.
        let neighbor = self.connection_boot(connection).expect("ready connection");
        let requests = self
            .dials
            .iter()
            .filter_map(|(peer, dial)| {
                dial.request
                    .filter(|r| *peer != neighbor.device_id && now < r.deadline)
                    .map(|r| (*peer, r))
            })
            .collect::<Vec<_>>();
        for (target, request) in requests {
            self.send(
                connection,
                0,
                LinkMsg::PathRequest {
                    reservation: None,
                    request_id: request.id,
                    expected_target_boot: None,
                    target,
                    round: request.round,
                },
            );
        }
        let waiting = std::mem::take(&mut self.pending_relay);
        for pending in waiting {
            self.relay_request(pending, now);
        }
    }

    fn defer_relay_request(&mut self, pending: PendingRequest) {
        // Retain at most one copy per source/request, without renewing expiry.
        // Only admitted or HalfOpen direct neighbors can reach this queue.
        if self
            .pending_relay
            .iter()
            .any(|p| p.connection == pending.connection && p.request.id == pending.request.id)
            || self.pending_relay.len() >= MAX_DIALS * crate::roster::MAX_MEMBERS
            || self
                .pending_relay
                .iter()
                .filter(|p| p.connection == pending.connection)
                .count()
                >= MAX_DIALS
        {
            return;
        }
        self.pending_relay.push(pending);
    }
    pub fn transit_paths(&self) -> Vec<(PathInstall, Boot, Boot, RelayReservation)> {
        self.transit
            .iter()
            .filter(|t| t.confirmed)
            .filter_map(|t| t.installed.map(|wire| (wire, t.source, t.target, t.reservation)))
            .collect()
    }

    pub(super) fn request_relay(&mut self, peer: DeviceId, now: Instant) {
        if self.dials.get(&peer).is_none_or(|d| d.request.is_some()) {
            return;
        }
        let request = Request {
            id: PathRequestId {
                source_boot: self.boot,
                random: rand::random(),
            },
            round: self.number(),
            deadline: now + RELAY_TIMEOUT,
        };
        self.dials.get_mut(&peer).expect("dial").request = Some(request);
        let neighbors: Vec<_> = self
            .links
            .values()
            .filter(|l| {
                l.peer.device_id != peer && l.installed && l.state == LinkState::Ready && l.path() == LinkPath::Direct
            })
            .map(|l| l.hop.connection_id)
            .collect();
        for connection in neighbors {
            self.send(
                connection,
                0,
                LinkMsg::PathRequest {
                    reservation: None,
                    request_id: request.id,
                    expected_target_boot: None,
                    target: peer,
                    round: request.round,
                },
            );
        }
    }
    pub(super) fn cancel_request(&mut self, peer: DeviceId) {
        let Some(request) = self.dials.get_mut(&peer).and_then(|d| d.request.take()) else {
            return;
        };
        let neighbors: Vec<_> = self
            .links
            .values()
            .filter(|l| l.path() == LinkPath::Direct && l.state == LinkState::Ready)
            .map(|l| l.hop.connection_id)
            .collect();
        for connection in neighbors {
            self.send(
                connection,
                0,
                LinkMsg::PathCancel {
                    reference: PathReference::Request(request.id),
                    reason: Refusal::Superseded,
                },
            );
        }
    }
    fn close_transit(&mut self, t: Transit) {
        if let Some(wire) = t.installed {
            self.effects.push_back(Effect::CloseTransit(wire.path_id));
        }
    }
    pub(super) fn retire_relay_authority(&mut self, connection: ConnectionId) {
        let peer = self.connection_boot(connection).map(|boot| boot.device_id);
        self.pending_relay
            .retain(|p| p.connection != connection && Some(p.target) != peer);
        // The physical connection stays available for C0 recovery. Notify
        // this endpoint too; connection_lost ordinarily relies on its loss.
        let notices = self
            .transit
            .iter()
            .filter(|t| t.other(connection).is_some())
            .map(|t| (t.request.id, t.installed))
            .collect::<Vec<_>>();
        for (request, installed) in notices {
            if let Some(wire) = installed {
                self.send(
                    connection,
                    wire.next_hop_binding.route_tag,
                    LinkMsg::PathDown { path_id: wire.path_id },
                );
            } else {
                self.send(
                    connection,
                    0,
                    LinkMsg::PathCancel {
                        reference: PathReference::Request(request),
                        reason: Refusal::Closed,
                    },
                );
            }
        }
        self.relay_connection_lost(connection);
    }
    pub(super) fn relay_connection_lost(&mut self, connection: ConnectionId) {
        self.pending_relay.retain(|p| p.connection != connection);
        let mut index = 0;
        while index < self.transit.len() {
            let t = self.transit[index];
            let Some(other) = t.other(connection) else {
                index += 1;
                continue;
            };
            self.transit.remove(index);
            self.close_transit(t);
            if let Some(wire) = t.installed {
                self.send(
                    other,
                    wire.next_hop_binding.route_tag,
                    LinkMsg::PathDown { path_id: wire.path_id },
                );
            } else {
                self.send(
                    other,
                    0,
                    LinkMsg::PathCancel {
                        reference: PathReference::Request(t.request.id),
                        reason: Refusal::Closed,
                    },
                );
            }
        }
    }
    pub fn relay_route(&self, connection: ConnectionId, tag: u64) -> Option<(ConnectionId, PathId)> {
        self.transit.iter().find_map(|t| {
            let wire = t.installed?;
            (wire.next_hop_binding.route_tag == tag)
                .then(|| t.other(connection).map(|other| (other, wire.path_id)))
                .flatten()
        })
    }
    fn tag_conflict(&self, connection: ConnectionId, tag: u64, path: PathId) -> bool {
        self.links
            .values()
            .any(|l| l.wire.path_id != path && l.hop.connection_id == connection && l.hop.route_tag == tag)
            || self.transit.iter().any(|t| {
                t.other(connection).is_some()
                    && t.installed
                        .is_some_and(|i| i.path_id != path && i.next_hop_binding.route_tag == tag)
            })
    }
    pub(super) fn relay_control(
        &mut self,
        connection: ConnectionId,
        tag: u64,
        message: &LinkMsg,
        now: Instant,
    ) -> bool {
        if tag != 0 {
            let Some((other, path)) = self.relay_route(connection, tag) else {
                return false;
            };
            self.send(other, tag, message.clone());
            if matches!(message, LinkMsg::PathDown { .. } | LinkMsg::PathCancel { .. }) {
                let index = self
                    .transit
                    .iter()
                    .position(|t| t.installed.is_some_and(|i| i.path_id == path))
                    .expect("transit");
                let t = self.transit.remove(index);
                self.close_transit(t);
            }
            return true;
        }
        match *message {
            LinkMsg::PathRequest {
                reservation,
                request_id,
                expected_target_boot,
                target,
                round,
            } => {
                self.relay_request(
                    PendingRequest {
                        connection,
                        request: Request {
                            id: request_id,
                            round,
                            deadline: now + RELAY_TIMEOUT,
                        },
                        reservation,
                        expected_target_boot,
                        target,
                    },
                    now,
                );
                true
            }
            LinkMsg::PathNominate {
                request_id,
                reservation,
                endpoint_boot,
            } => {
                if reservation.relay == self.boot {
                    if let Some(t) = self.transit.iter().find(|t| {
                        t.request.id == request_id
                            && t.reservation == reservation
                            && t.reservation.target_connection == connection
                            && t.target == endpoint_boot
                            && now < t.request.deadline
                    }) {
                        self.send(t.reservation.source_connection, 0, message.clone());
                    }
                } else if request_id.source_boot == self.boot {
                    let peer = endpoint_boot.device_id;
                    let Some(request) = self
                        .dials
                        .get(&peer)
                        .and_then(|d| d.request)
                        .filter(|r| r.id == request_id && now < r.deadline)
                    else {
                        return true;
                    };
                    if self.peer_link(peer).is_some()
                        // In a simultaneous request, the follower may already
                        // have nominated the opposite request. The arbiter's
                        // nomination wins; only the arbiter keeps its first pick.
                        || self.arbiter(peer) && self.nominations.contains_key(&peer)
                        || reservation.source_connection != connection
                        || self.connection_boot(connection) != Some(reservation.relay)
                    {
                        return true;
                    }
                    self.observe_peer_boot(endpoint_boot, now);
                    let nomination = Nomination { request, reservation };
                    self.nominations.insert(peer, nomination);
                    if self.arbiter(peer) {
                        self.install_relay(endpoint_boot, nomination, connection, now);
                    }
                }
                true
            }
            LinkMsg::PathInstall(mut wire) if wire.reservation.is_some_and(|r| r.relay == self.boot) => {
                let reservation = wire.reservation.expect("reserved");
                let Some(index) = self.transit.iter().position(|t| {
                    t.reservation == reservation && t.request.id == wire.request_id && now < t.request.deadline
                }) else {
                    return true;
                };
                let t = self.transit[index];
                let pair = BootPair::new(t.source, t.target).expect("pair");
                let coordinator = if pair.coordinator() == t.source {
                    reservation.source_connection
                } else {
                    reservation.target_connection
                };
                if connection != coordinator
                    || pair != wire.path_id.boot_pair
                    || wire.next_hop_binding.route_tag == 0
                    || t.installed.is_some_and(|i| i != wire)
                    || self.tag_conflict(
                        reservation.source_connection,
                        wire.next_hop_binding.route_tag,
                        wire.path_id,
                    )
                    || self.tag_conflict(
                        reservation.target_connection,
                        wire.next_hop_binding.route_tag,
                        wire.path_id,
                    )
                {
                    return true;
                }
                let other = t.other(connection).expect("endpoint");
                self.transit[index].installed = Some(wire);
                wire.next_hop_binding = NextHopBinding {
                    peer: self.boot.device_id,
                    connection_id: other,
                    route_tag: wire.next_hop_binding.route_tag,
                };
                self.send(other, 0, LinkMsg::PathInstall(wire));
                true
            }
            LinkMsg::PathInstalled(wire) if wire.reservation.is_some_and(|r| r.relay == self.boot) => {
                let Some(index) = self.transit.iter().position(|t| {
                    t.reservation == wire.reservation.expect("reserved")
                        && t.installed
                            .is_some_and(|i| i.path_id == wire.path_id && i.route_revision == wire.route_revision)
                }) else {
                    return true;
                };
                let t = self.transit[index];
                let original = t.installed.expect("installed");
                let coordinator = original.next_hop_binding.connection_id;
                if t.other(connection) != Some(coordinator) {
                    return true;
                }
                self.transit[index].confirmed = true;
                self.send(coordinator, 0, LinkMsg::PathInstalled(original));
                true
            }
            LinkMsg::PathCancel {
                reference: PathReference::Installed(path_id),
                ..
            } => {
                if let Some(index) = self
                    .transit
                    .iter()
                    .position(|t| t.other(connection).is_some() && t.installed.is_some_and(|i| i.path_id == path_id))
                {
                    let t = self.transit.remove(index);
                    self.close_transit(t);
                }
                true
            }
            LinkMsg::PathCancel {
                reference: PathReference::Request(request_id),
                ..
            } => {
                self.pending_relay
                    .retain(|p| p.connection != connection || p.request.id != request_id);
                if let Some(index) = self
                    .transit
                    .iter()
                    .position(|t| t.request.id == request_id && t.other(connection).is_some())
                {
                    let t = self.transit.remove(index);
                    self.close_transit(t);
                    self.send(t.other(connection).expect("endpoint"), 0, message.clone());
                }
                self.nominations.retain(|_, n| n.request.id != request_id);
                true
            }
            _ => false,
        }
    }
    fn relay_request(&mut self, pending: PendingRequest, now: Instant) {
        let PendingRequest {
            connection,
            request,
            reservation,
            expected_target_boot,
            target,
        } = pending;
        let Request {
            id: request_id, round, ..
        } = request;
        if now >= request.deadline {
            return;
        }

        let Some(neighbor) = self.connection_boot(connection) else {
            return;
        };
        if request_id.source_boot.device_id == target {
            return;
        }
        if self.ready_direct(neighbor.device_id) != Some(connection) {
            // C0 can finish at the sender before this endpoint consumes
            // Done. Retain the request, but never route before Ready.
            if self.links.values().any(|link| {
                link.installed
                    && link.hop.connection_id == connection
                    && link.path() == LinkPath::Direct
                    && link.state == LinkState::HalfOpen
            }) {
                self.defer_relay_request(pending);
            }
            return;
        }
        if target == self.boot.device_id {
            let Some(reservation) = reservation.filter(|r| r.relay == neighbor && r.target_connection == connection)
            else {
                return;
            };
            let source = request_id.source_boot;
            if self.peer_link(source.device_id).is_some() {
                self.send(
                    connection,
                    0,
                    LinkMsg::PathCancel {
                        reference: PathReference::Request(request_id),
                        reason: Refusal::Superseded,
                    },
                );
                return;
            }
            if self.nominations.get(&source.device_id).is_some_and(|n| {
                n.request.id.source_boot == source
                    && (round < n.request.round
                        || round == n.request.round && (n.request.id != request_id || n.reservation != reservation))
            }) {
                return;
            }
            self.observe_peer_boot(source, now);
            let nomination = Nomination { request, reservation };
            self.nominations.insert(source.device_id, nomination);
            self.send(
                connection,
                0,
                LinkMsg::PathNominate {
                    request_id,
                    reservation,
                    endpoint_boot: self.boot,
                },
            );
            if self.arbiter(source.device_id) {
                self.install_relay(source, nomination, connection, now);
            }
        } else {
            if reservation.is_some()
                || request_id.source_boot != neighbor
                || self.transit.iter().any(|t| t.request.id == request_id)
            {
                return;
            }
            let Some(target_connection) = self.ready_direct(target) else {
                self.defer_relay_request(pending);
                return;
            };
            let target_boot = self.connection_boot(target_connection).expect("ready connection");
            let reservation = RelayReservation {
                relay: self.boot,
                random: rand::random(),
                source_connection: connection,
                target_connection,
            };
            self.transit.push(Transit {
                request,
                reservation,
                source: neighbor,
                target: target_boot,
                installed: None,
                confirmed: false,
            });
            self.send(
                target_connection,
                0,
                LinkMsg::PathRequest {
                    reservation: Some(reservation),
                    request_id,
                    expected_target_boot,
                    target,
                    round,
                },
            );
        }
    }
    fn install_relay(&mut self, peer: Boot, nomination: Nomination, connection: ConnectionId, now: Instant) {
        let hop = NextHopBinding {
            peer: nomination.reservation.relay.device_id,
            connection_id: connection,
            // The reservation is allocated by the relay; its random identity also
            // supplies the relay's route tag, unchanged across its two hops.
            route_tag: u64::from_le_bytes(nomination.reservation.random[..8].try_into().expect("reservation tag"))
                .max(1),
        };
        let wire = PathInstall {
            request_id: nomination.request.id,
            path_id: PathId {
                boot_pair: BootPair::new(self.boot, peer).expect("pair"),
                coordinator_counter: self.number(),
            },
            route_revision: RouteRevision(0),
            reservation: Some(nomination.reservation),
            next_hop_binding: hop,
        };
        self.new_link(peer, wire, hop, false, now);
        self.send(connection, 0, LinkMsg::PathInstall(wire));
    }
    pub(super) fn relay_tick(&mut self, now: Instant) {
        self.pending_relay.retain(|p| now < p.request.deadline);
        self.nominations.retain(|_, n| now < n.request.deadline);
        let mut index = 0;
        while index < self.transit.len() {
            let t = self.transit[index];
            if t.confirmed || now < t.request.deadline {
                index += 1;
                continue;
            }
            self.transit.remove(index);
            self.close_transit(t);
            for connection in [t.reservation.source_connection, t.reservation.target_connection] {
                self.send(
                    connection,
                    0,
                    LinkMsg::PathCancel {
                        reference: PathReference::Request(t.request.id),
                        reason: Refusal::Closed,
                    },
                );
            }
        }
        let expired: Vec<_> = self
            .dials
            .iter()
            .filter(|(_, d)| d.request.is_some_and(|r| now >= r.deadline))
            .map(|(peer, _)| *peer)
            .collect();
        for peer in expired {
            self.cancel_request(peer);
            if self.dials.get(&peer).is_some_and(|d| !d.direct) {
                self.fail_dial(peer, DialFailure::Unreachable);
            }
        }
    }
}
