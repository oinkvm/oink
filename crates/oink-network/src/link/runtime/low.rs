//! Admission stays on the network thread; the pooled TCP work stays ordinary.
use super::super::tcp::{
    self,
    port::{Delivery, Endpoint},
};
use super::*;
use crate::link::control::{LowBinding, LowMsg, NextHopBinding};

pub(super) struct Probe {
    pub next: Instant,
    pub nonce: u64,
    pub pending: Option<(u64, Instant)>,
}
impl Network {
    pub(super) fn ensure_n1(&mut self, connection: ConnectionId) {
        if self.n1.contains_key(&connection) || self.n1_retry.contains_key(&connection) {
            return;
        }
        let Some(&uid) = self.connections.get(&connection) else {
            return;
        };
        let Some(boot) = self.core.connection_boot(connection) else {
            return;
        };
        let (hop, receiver) = tcp::N1::new(connection, boot, self.physical[&uid].connection.remote_address());
        let header = StreamHeader {
            route_tag: 0,
            priority: Priority::Normal,
            tag: StreamTag {
                kind: StreamKind::LowControl,
                ordinal: 1,
            },
            datagram_flow: 0,
        };
        let source = Source::Pool(wire::PoolSource {
            receiver,
            advert: hop.advert.clone(),
            port: self.low_port.clone(),
            prefer_info: true,
        });
        self.writers
            .push_back(Box::new(Writer::new(uid, header, None, source, None)));
        self.n1.insert(connection, hop);
    }
    pub(super) fn close_n1(&mut self, uid: usize) {
        let Some(connection) = self.physical.get(&uid).and_then(|p| p.id) else {
            return;
        };
        self.n1.remove(&connection);
        self.writers.retain_mut(|writer| {
            if writer.uid == uid && writer.header.tag.kind == StreamKind::LowControl {
                writer.abort(StreamCode::None);
                false
            } else {
                true
            }
        });
        self.readers.retain_mut(|reader| {
            if reader.uid == uid && reader.header.is_some_and(|h| h.tag.kind == StreamKind::LowControl) {
                reader.stop(StreamCode::None);
                false
            } else {
                true
            }
        });
        self.n1_retry
            .insert(connection, Instant::now() + Duration::from_secs(1));
        self.publish();
    }
    pub(super) fn retry_n1(&mut self, now: Instant) {
        let retries: Vec<_> = self
            .n1_retry
            .iter()
            .filter_map(|(connection, deadline)| (*deadline <= now).then_some(*connection))
            .collect();
        for connection in retries {
            if !self.connections.contains_key(&connection) {
                self.n1_retry.remove(&connection);
                continue;
            }
            // Ordinary pool cleanup must finish before this same physical hop
            // can admit new operations. This snapshot read never waits on it.
            if self.shared.hub.status.load().iter().any(|s| s.connection == connection) {
                continue;
            }
            self.n1_retry.remove(&connection);
            self.ensure_n1(connection);
            self.publish_low();
        }
    }
    pub(super) fn publish_low(&self) {
        let mut routes: Vec<_> = self
            .core
            .links
            .values()
            .filter(|l| l.installed && !l.frozen() && !self.old_lanes.contains(&l.id))
            .map(|l| tcp::Route {
                agreement: self.logical.get(&l.id).cloned().or_else(|| {
                    (l.path() == LinkPath::Direct)
                        .then(|| {
                            self.connections
                                .get(&l.hop.connection_id)
                                .and_then(|uid| self.physical.get(uid))
                                .and_then(|p| p.bootstrap.selected())
                                .map(|s| s.agreement.clone())
                        })
                        .flatten()
                }),
                endpoint: Some(Endpoint {
                    link: l.id,
                    peer: l.peer,
                }),
                wire: l.wire,
                source: self.core.boot,
                target: l.peer,
                hops: vec![l.hop.connection_id],
                ready: l.state == LinkState::Ready,
            })
            .collect();
        for (wire, source, target, reservation) in self.core.transit_paths() {
            routes.push(tcp::Route {
                agreement: None,
                endpoint: None,
                wire,
                source,
                target,
                hops: vec![reservation.source_connection, reservation.target_connection],
                ready: true,
            });
        }
        let hops: Vec<_> = self
            .n1
            .iter()
            .filter(|(id, _)| self.connections.contains_key(id))
            .map(|(_, h)| h.clone())
            .collect();
        let current = self.shared.hub.view.load();
        if current.routes == routes
            && current.hops.len() == hops.len()
            && current
                .hops
                .iter()
                .all(|old| hops.iter().any(|new| Arc::ptr_eq(old, new)))
        {
            return;
        }
        self.shared.hub.view.store(Arc::new(tcp::View { routes, hops }));
        self.shared.hub.changed.notify_waiters();
    }
    pub(super) fn low_message(&mut self, uid: usize, bytes: Vec<u8>) -> Option<Delivery> {
        let Ok(LinkMsg::Low { binding, message }) = super::super::control_wire::decode(&bytes) else {
            return None;
        };
        let connection = self.physical.get(&uid)?.id?;
        if binding.hop.connection_id != connection || !message.is_hop_control() {
            return None;
        }
        let grant = self.low_port.grant(binding)?;
        let forward = if grant.endpoint.is_none() {
            let (other, _) = self.core.relay_route(connection, binding.hop.route_tag)?;
            let peer = self.core.connection_boot(other)?.device_id;
            Some(self.low_port.grant(LowBinding {
                hop: NextHopBinding {
                    peer,
                    connection_id: other,
                    route_tag: binding.hop.route_tag,
                },
                ..binding
            })?)
        } else {
            None
        };
        let delivery = Delivery {
            grant,
            forward,
            message: *message,
            refusal: None,
        };
        self.deliver_low(delivery)
    }
    pub(super) fn deliver_low(&mut self, mut delivery: Delivery) -> Option<Delivery> {
        if !self.low_port.low_current(delivery.grant) {
            return None;
        }
        if let LowMsg::Open(request) = &delivery.message
            && request.purpose == crate::LowTcpPurpose::Transfer
            && let Some(endpoint) = delivery.grant.endpoint
        {
            if request.binding != delivery.grant.binding {
                return None;
            }
            // N1 may overtake C0 Done. Retain this one decoded message in the
            // reader until the exact link is admitted; never promote it here.
            if self
                .core
                .links
                .get(&endpoint.link)
                .is_some_and(|link| link.state == LinkState::HalfOpen)
            {
                return Some(delivery);
            }
            if self.core.inbound(endpoint.link).is_err() {
                delivery.refusal = Some("unavailable: link is not ready".into());
            } else if u8::try_from(request.kind)
                .ok()
                .and_then(StreamKind::from_wire)
                .is_none_or(|kind| !self.modules.contains_key(&kind) || !self.low_port.admit_kind(delivery.grant, kind))
            {
                delivery.refusal = Some("unavailable: unsupported module".into());
            }
        }
        let hop = self.n1.get(&delivery.grant.binding.hop.connection_id)?;
        match hop.input.try_send(delivery) {
            Ok(()) => None,
            Err(mpsc::error::TrySendError::Full(delivery)) => Some(delivery),
            Err(mpsc::error::TrySendError::Closed(_)) => None,
        }
    }
    pub(super) fn probe_tick(&mut self, now: Instant) {
        let ids: Vec<_> = self.probes.keys().copied().collect();
        for connection in ids {
            let Some(&uid) = self.connections.get(&connection) else {
                self.probes.remove(&connection);
                continue;
            };
            let probe = self.probes.get_mut(&connection).expect("probe");
            if probe
                .pending
                .is_some_and(|(_, sent)| now.saturating_duration_since(sent) >= Duration::from_millis(500))
            {
                probe.pending = None;
                self.shared.hub.observed.rcu(|old| {
                    let mut next = (**old).clone();
                    next.insert(connection, None);
                    next
                });
                self.shared.hub.changed.notify_waiters();
            }
            if now >= probe.next && probe.pending.is_none() {
                probe.nonce += 1;
                let nonce = probe.nonce;
                probe.pending = Some((nonce, now));
                probe.next = now + crate::pace::PERIOD;
                self.send_control(uid, 0, LinkMsg::HopProbe { nonce });
            }
        }
    }
    pub(super) fn probe_pong(&mut self, connection: ConnectionId, nonce: u64) {
        let Some(probe) = self.probes.get_mut(&connection) else {
            return;
        };
        let Some((pending, sent)) = probe.pending else {
            return;
        };
        if pending != nonce {
            return;
        }
        probe.pending = None;
        let rtt = sent.elapsed();
        self.shared.hub.observed.rcu(|old| {
            let mut next = (**old).clone();
            next.insert(connection, Some(rtt));
            next
        });
        self.shared.hub.changed.notify_waiters();
    }
}
