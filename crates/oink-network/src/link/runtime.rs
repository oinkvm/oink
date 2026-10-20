//! The single network thread. Quinn drivers run on this same current-thread runtime;
//! stream work is polled here, in the three measured time-budget classes.
mod low;
use super::control::LinkMsg;
use super::{
    core::{Core, Effect},
    handle::{Command, Shared},
    pipe::Pipe,
    wire::{self, Delivery, Reader, Source, Writer},
    *,
};
use oink_foundation::{realtime, session::ConnectionId};
use oink_protocol::frame::{self, StreamHeader};
use oink_protocol::v1::{
    self,
    admission::{Agreement, Context as Admission, Phase},
    registry::Transport,
};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::atomic::Ordering,
    task::{Context, Poll, Waker},
    time::Instant,
};
use tokio::sync::{mpsc, oneshot};

struct Signal(Arc<tokio::sync::Notify>);
impl std::task::Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.0.notify_one();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.notify_one();
    }
}
struct Physical {
    connection: quinn::Connection,
    _probe_endpoint: Option<quinn::Endpoint>,
    peer: DeviceId,
    id: Option<ConnectionId>,
    deadline: Instant,
    outbound: bool,
    datagram_source_cursor: usize,
    next_flow: u32,
    bootstrap: crate::negotiation::Bootstrap,
    incoming: wire::Waiter<Result<quinn::RecvStream, quinn::ConnectionError>>,
    datagrams: wire::Waiter<Result<bytes::Bytes, quinn::ConnectionError>>,
}
impl Drop for Physical {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"link retired");
    }
}
type Connecting = Pin<Box<dyn Future<Output = Result<quinn::Connection, quinn::ConnectionError>>>>;
struct Handshake {
    trace: Option<diagnostics::CandidateDial>,
    future: Connecting,
    peer: Option<DeviceId>,
    id: Option<ConnectionId>,
    deadline: Instant,
}
struct AddressProbe {
    handshake: Handshake,
    endpoint: quinn::Endpoint,
    socket: Arc<super::probe_socket::Socket>,
    reply: oneshot::Sender<Result<DeviceId, AddressProbeError>>,
}
struct Dialing {
    peer: DeviceId,
    identity_mismatch: bool,
    addresses: VecDeque<SocketAddr>,
    attempts: Vec<Handshake>,
}
struct ControlQueues {
    tx: Arc<queue::Queue>,
    rx: Arc<queue::Queue>,
    announced: bool,
}
struct Network {
    profile: crate::negotiation::OfferProfile,
    logical: HashMap<LinkId, Agreement>,
    formats: HashMap<LinkId, v1::dto::FormatSupport>,
    audio: HashMap<LinkId, Vec<v1::dto::AudioSupport>>,
    old_lanes: std::collections::HashSet<LinkId>,
    retired_members: std::collections::HashSet<LinkId>,
    #[cfg(any(test, feature = "test-support"))]
    blocked_direct: std::collections::HashSet<DeviceId>,
    violations: HashMap<(Boot, Option<oink_foundation::session::PathId>), (Instant, u32, u64)>,
    started: Instant,
    core: Core,
    endpoint: quinn::Endpoint,
    probe_client: quinn::ClientConfig,
    accepting: wire::Waiter<Option<quinn::Incoming>>,
    shared: Arc<Shared>,
    events: mpsc::UnboundedSender<ConfigEvent>,
    commands: mpsc::UnboundedReceiver<Command>,
    modules: HashMap<StreamKind, mpsc::UnboundedSender<ModuleEvent>>,
    physical: HashMap<usize, Physical>,
    connections: HashMap<ConnectionId, usize>,
    inbound: Vec<Handshake>,
    dials: Vec<Dialing>,
    address_probes: Vec<AddressProbe>,
    next_uid: usize,
    // Rotate pointers, not whole stream states, on every idle sweep.
    writers: VecDeque<Box<Writer>>,
    readers: VecDeque<Box<Reader>>,
    pipes: VecDeque<Box<Pipe>>,
    controls: HashMap<LinkId, ControlQueues>,
    schedule: SchedulerStatus,
    next_snapshot: Instant,
    next_readback: Instant,
    stop: bool,
    cursors: [usize; 3],
    idle_visits: [usize; 3],
    datagram_cursor: usize,
    datagram_buffer: Vec<u8>,
    connection_cursor: usize,
    n1: HashMap<ConnectionId, Arc<super::tcp::N1>>,
    n1_retry: HashMap<ConnectionId, Instant>,
    low_port: Arc<super::tcp::port::Port>,
    probes: HashMap<ConnectionId, low::Probe>,
}
fn transport(
    credentials: &crate::tls::Credentials,
) -> Result<(quinn::ServerConfig, quinn::ClientConfig), crate::NetError> {
    let build = || -> anyhow::Result<_> {
        let mut t = quinn::TransportConfig::default();
        t.max_concurrent_uni_streams(frame::MAX_UNI_STREAMS.into())
            .max_concurrent_bidi_streams(frame::MAX_BIDI_STREAMS.into())
            .stream_receive_window(frame::STREAM_RECEIVE_WINDOW.into())
            .keep_alive_interval(Some(KEEP_ALIVE))
            .max_idle_timeout(Some(IDLE_TIMEOUT.try_into()?))
            .send_window(crate::qos::SEND_WINDOW)
            .datagram_send_buffer_size(crate::qos::DATAGRAM_SEND_BUFFER);
        crate::qos::interactive_congestion(&mut t);
        let t = Arc::new(t);
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(quinn::crypto::rustls::QuicServerConfig::try_from(
            credentials.server_config()?,
        )?));
        server.transport = t.clone();
        let mut client = quinn::ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(
            credentials.client_config(None)?,
        )?));
        client.transport_config(t);
        Ok((server, client))
    };
    build().map_err(|e| crate::NetError::Endpoint(e.to_string()))
}
pub(super) async fn run(
    config: LinkConfig,
    credentials: Arc<crate::tls::Credentials>,
    shared: Arc<Shared>,
    commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<ConfigEvent>,
    start: oneshot::Sender<Result<(), crate::NetError>>,
) {
    let applied = match realtime::apply(realtime::ThreadRole::Network, config.tier) {
        Ok(applied) => Some(applied),
        Err(error) => {
            tracing::warn!(%error, tier = ?config.tier, "network scheduling policy could not be applied");
            None
        }
    };
    let setup = || -> Result<(quinn::Endpoint, quinn::ClientConfig), crate::NetError> {
        let (server, client) = transport(&credentials)?;
        let socket = std::net::UdpSocket::bind(config.bind)?;
        crate::qos::set_service_type(&socket, crate::qos::ServiceType::Voice);
        let socket = crate::qos::wrap_udp_socket(socket)?;
        let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
            Default::default(),
            Some(server),
            socket,
            Arc::new(quinn::TokioRuntime),
        )?;
        endpoint.set_default_client_config(client.clone());
        Ok((endpoint, client))
    };
    let (endpoint, probe_client) = match setup() {
        Ok(endpoint) => endpoint,
        Err(e) => {
            let _ = start.send(Err(e));
            return;
        }
    };
    match endpoint.local_addr() {
        Ok(address) => {
            let _ = shared.address.set(address);
        }
        Err(e) => {
            let _ = start.send(Err(e.into()));
            return;
        }
    }
    let now = Instant::now();
    let mut network = Network {
        profile: config.profile,
        logical: HashMap::new(),
        formats: HashMap::new(),
        audio: HashMap::new(),
        old_lanes: std::collections::HashSet::new(),
        retired_members: std::collections::HashSet::new(),
        #[cfg(any(test, feature = "test-support"))]
        blocked_direct: std::collections::HashSet::new(),
        violations: HashMap::new(),
        started: now,
        core: Core::new(config.boot),
        endpoint,
        probe_client,
        accepting: Default::default(),
        shared: shared.clone(),
        events,
        commands,
        modules: HashMap::new(),
        physical: HashMap::new(),
        connections: HashMap::new(),
        inbound: vec![],
        dials: vec![],
        address_probes: Vec::new(),
        next_uid: 0,
        writers: VecDeque::new(),
        readers: VecDeque::new(),
        pipes: VecDeque::new(),
        controls: HashMap::new(),
        schedule: SchedulerStatus {
            applied,
            ..Default::default()
        },
        next_snapshot: now,
        next_readback: now + realtime::READBACK_INTERVAL,
        stop: false,
        cursors: [0; 3],
        idle_visits: [0; 3],
        datagram_cursor: 0,
        datagram_buffer: Vec::with_capacity(frame::MAX_DATAGRAM),
        connection_cursor: 0,
        n1: HashMap::new(),
        n1_retry: HashMap::new(),
        low_port: Arc::new(super::tcp::port::Port {
            boot: config.boot,
            hub: shared.hub.clone(),
            commands: shared.commands.clone(),
        }),
        probes: HashMap::new(),
    };
    network.publish();
    let _ = start.send(Ok(()));
    let waker = Waker::from(Arc::new(Signal(shared.wake.clone())));
    let mut expected: Option<Instant> = None;
    while !network.stop {
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let began = Instant::now();
        if let Some(expected) = expected {
            let late = began.saturating_duration_since(expected);
            if late > ROUND_BUDGET {
                network.schedule.late_rounds += 1;
                network.schedule.late_max = network.schedule.late_max.max(late);
            }
        }
        let mut cx = Context::from_waker(&waker);
        network.core.tick(began);
        network.probe_tick(began);
        network.retry_n1(began);
        network.effects();
        let mut progress = network.service(0, began + OWN_BUDGET, &mut cx);
        progress |= network.service(1, began + OWN_BUDGET + INTERACTIVE_BUDGET, &mut cx);
        progress |= network.service(2, began + ROUND_BUDGET, &mut cx);
        network.check_drained();
        network.effects();
        let ended = Instant::now();
        let elapsed = ended.duration_since(began);
        if elapsed > ROUND_BUDGET {
            network.schedule.over_budget += 1;
            network.schedule.over_budget_max = network.schedule.over_budget_max.max(elapsed);
        }
        if ended >= network.next_readback {
            if realtime::read_back().ok() != network.schedule.applied {
                network.schedule.demoted += 1;
            }
            network.next_readback = ended + realtime::READBACK_INTERVAL;
        }
        if ended >= network.next_snapshot {
            network.publish();
            network.next_snapshot = ended + Duration::from_millis(100);
        }
        if progress {
            expected = Some(ended);
        } else {
            let deadline = network.next_deadline();
            // A real timer has an expected start time even when the owner parks.
            // Queue notifications have no externally timestamped due time.
            expected = tokio::select! {
                _ = shared.wake.notified() => None,
                _ = tokio::time::sleep_until(deadline.into()) => Some(deadline),
            };
            network.idle_visits = [0; 3];
        }
    }
    let ids: Vec<_> = network.core.links.keys().copied().collect();
    for id in ids {
        network.core.lose(id, LinkLost::Stopped);
    }
    network.effects();
    network.shared.hub.stopped.store(true, Ordering::Release);
    network.shared.hub.changed.notify_waiters();
    network.endpoint.close(0u32.into(), b"stopped");
    network.writers.clear();
    network.readers.clear();
    network.pipes.clear();
    network.physical.clear();
    network.publish();
}
impl Network {
    fn next_deadline(&self) -> Instant {
        // Statistics refresh on work; only scheduling readback has an idle deadline.
        let mut next = self.next_readback;
        let mut include = |at: Instant| next = next.min(at);
        if let Some(at) = self.core.next_deadline() {
            include(at);
        }
        for p in self.physical.values() {
            if p.id.is_none_or(|id| !self.core.selected_connection_id(id)) {
                include(p.deadline);
            }
        }
        for h in &self.inbound {
            include(h.deadline);
        }
        for probe in &self.address_probes {
            include(probe.handshake.deadline);
        }
        for d in &self.dials {
            for h in &d.attempts {
                include(h.deadline);
            }
        }
        for (&id, &at) in &self.n1_retry {
            // Cleanup completion wakes us; an expired retry must not spin while it runs.
            if !self.shared.hub.status.load().iter().any(|s| s.connection == id) {
                include(at);
            }
        }
        for probe in self.probes.values() {
            include(
                probe
                    .pending
                    .map_or(probe.next, |(_, sent)| sent + Duration::from_millis(500)),
            );
        }
        next
    }
    fn flow(&mut self, uid: usize) -> Option<u32> {
        let physical = self.physical.get_mut(&uid)?;
        if let Some(flow) = physical.next_flow.checked_add(1) {
            physical.next_flow = flow;
            Some(flow)
        } else {
            physical.connection.close(12u32.into(), b"flow space exhausted");
            None
        }
    }
    fn physical_for(&self, id: LinkId) -> Option<usize> {
        self.core
            .links
            .get(&id)
            .and_then(|l| self.connections.get(&l.hop.connection_id))
            .copied()
    }
    fn send_control(&mut self, uid: usize, tag: u64, message: LinkMsg) {
        self.idle_visits = [0; 3];
        let Ok(bytes) = super::control_wire::encode(&message) else {
            return;
        };
        self.send_private(uid, tag, bytes);
    }
    fn send_private(&mut self, uid: usize, tag: u64, bytes: Vec<u8>) {
        if let Some(w) = self
            .writers
            .iter_mut()
            .find(|w| w.uid == uid && w.header.route_tag == tag && w.header.tag.kind == StreamKind::Internal)
        {
            w.private_post(queue::Queued {
                pending: None,
                arrived: None,
                bytes,
                timing: None,
            });
            return;
        }
        let bytes = queue::Queued {
            pending: None,
            arrived: None,
            bytes,
            timing: None,
        };
        let header = StreamHeader {
            route_tag: tag,
            priority: Priority::Interactive,
            tag: StreamTag {
                kind: StreamKind::Internal,
                ordinal: 1,
            },
            datagram_flow: 0,
        };
        self.writers.push_back(Box::new(Writer::new(
            uid,
            header,
            None,
            Source::Private(VecDeque::from([bytes])),
            None,
        )));
    }
    fn module_event(&self, kind: StreamKind, event: ModuleEvent) {
        if let Some(sender) = self.modules.get(&kind) {
            let _ = sender.send(event);
        }
    }
    fn report_violation(
        &mut self,
        peer: Boot,
        path: Option<oink_foundation::session::PathId>,
        v: v1::admission::Violation,
    ) {
        let now = Instant::now();
        let rate = self.violations.entry((peer, path)).or_insert((now, 0, 0));
        if now.duration_since(rate.0) >= Duration::from_secs(1) {
            rate.0 = now;
            rate.1 = 0;
        }
        if rate.1 >= v1::limits::ERROR_RATE_PER_SECOND {
            rate.2 = rate.2.saturating_add(1);
            return;
        }
        rate.1 += 1;
        let suppressed = std::mem::take(&mut rate.2);
        let scope = match v.isolation {
            v1::dto::Isolation::PhysicalConnection => v1::dto::ErrorScope::Connection,
            v1::dto::Isolation::LogicalLink => v1::dto::ErrorScope::LogicalLink,
            v1::dto::Isolation::LowOperation => v1::dto::ErrorScope::LowOperation,
            _ => v1::dto::ErrorScope::Stream,
        };
        if v.reply {
            use v1::schema::{WireDto, WireValue};
            let mut raw: v1::RawMap = [(1, v.code.number().write()), (2, scope.write())].into();
            if let Some(type_id) = v.type_id {
                raw.insert(3, type_id.write());
            }
            raw.insert(4, v.kind.write());
            if let Some(capability) = v.capability {
                raw.insert(7, v1::RawValue::from(capability));
            }
            if let Ok(error) = v1::dto::ProtocolError::from_raw(raw)
                && let Ok(bytes) = v1::frame::encode(&v1::Message::ProtocolError(error), v1::limits::CONFIG_FRAME)
            {
                if let Some(path) = path {
                    if let Some((id, _)) = self
                        .core
                        .links
                        .iter()
                        .find(|(_, link)| link.peer == peer && link.wire.path_id == path)
                        && let Some(control) = self.controls.get(id)
                    {
                        let _ = control.tx.post(bytes, v1::limits::CONFIG_FRAME);
                    }
                } else if let Some(uid) = self.physical.iter().find_map(|(uid, physical)| {
                    physical
                        .bootstrap
                        .selected()
                        .is_some_and(|selected| selected.peer_boot == peer)
                        .then_some(*uid)
                }) {
                    self.send_private(uid, 0, bytes);
                }
            }
        }
        let violation = v1::admission::ProtocolViolation {
            code: v.code,
            scope,
            peer: crate::wire_identity::device(peer.device_id),
            boot: crate::wire_identity::boot(peer),
            path: path.map(crate::wire_identity::path),
            kind: Some(v.kind),
            type_id: v.type_id,
            capability: v.capability.map(str::to_owned),
            experimental: v.experimental,
            isolation: v.isolation,
            at_us: now.duration_since(self.started).as_micros().min(u64::MAX as u128) as u64,
            suppressed,
        };
        // One diagnostics owner records every family, including N0/C0. Fanout
        // to business owners would duplicate the same local event.
        self.module_event(
            StreamKind::Diagnostics,
            ModuleEvent::ProtocolViolation {
                peer: peer.device_id,
                violation,
            },
        );
    }
    fn module_all(&self, make: impl Fn() -> ModuleEvent) {
        let mut sent: Vec<&mpsc::UnboundedSender<ModuleEvent>> = vec![];
        for sender in self.modules.values() {
            if !sent.iter().any(|s| s.same_channel(sender)) {
                let _ = sender.send(make());
                sent.push(sender);
            }
        }
    }
    fn control_queues(&mut self, id: LinkId) {
        if self.controls.contains_key(&id) {
            return;
        }
        let Some(uid) = self.physical_for(id) else {
            return;
        };
        let (txspec, rxspec) = QueueSpec::reliable(StreamKind::Config);
        let tx = queue::Queue::new(txspec);
        let rx = queue::Queue::new(rxspec);
        tx.set_wake(self.shared.wake_fn());
        rx.set_producer_wake(self.shared.wake_fn());
        let tag = StreamTag {
            kind: StreamKind::Config,
            ordinal: 1,
        };
        let sid = StreamId {
            link: id,
            dir: StreamDir::Out,
            tag,
        };
        let header = StreamHeader {
            route_tag: self.core.links[&id].hop.route_tag,
            priority: Priority::Normal,
            tag,
            datagram_flow: 0,
        };
        self.writers.push_back(Box::new(Writer::new(
            uid,
            header,
            Some(sid),
            Source::User(tx.clone()),
            None,
        )));
        self.controls.insert(
            id,
            ControlQueues {
                tx,
                rx,
                announced: false,
            },
        );
    }
    fn effects(&mut self) {
        while let Some(effect) = self.core.effects.pop_front() {
            self.idle_visits = [0; 3];
            match effect {
                Effect::Control {
                    connection,
                    tag,
                    message,
                } => {
                    if let Some(&uid) = self.connections.get(&connection) {
                        self.send_control(uid, tag, *message);
                    }
                }
                Effect::CloseConnection(connection) => {
                    if let Some(uid) = self.connections.remove(&connection) {
                        self.remove_physical(uid);
                    }
                }
                Effect::Connected(id) => {
                    self.ensure_n1(self.core.links[&id].hop.connection_id);
                    self.control_queues(id);
                    let Some(q) = self.controls.get_mut(&id) else {
                        continue;
                    };
                    if q.announced {
                        continue;
                    }
                    q.announced = true;
                    let link = &self.core.links[&id];
                    let tag = StreamTag {
                        kind: StreamKind::Config,
                        ordinal: 1,
                    };
                    let event = LinkConnected {
                        link: id,
                        peer: link.peer.device_id,
                        peer_boot: link.peer,
                        path_id: link.wire.path_id,
                        route_revision: link.wire.route_revision.0,
                        negotiation: (link.path() == LinkPath::Direct)
                            .then(|| {
                                self.connections
                                    .get(&link.hop.connection_id)
                                    .and_then(|uid| self.physical.get(uid))
                                    .and_then(|p| p.bootstrap.selected())
                                    .cloned()
                            })
                            .flatten(),
                        direction: link.direction,
                        path: link.path(),
                        remote_address: if link.path() == LinkPath::Direct {
                            self.connections
                                .get(&link.hop.connection_id)
                                .and_then(|uid| self.physical.get(uid))
                                .map(|physical| physical.connection.remote_address())
                        } else {
                            None
                        },
                        control_tx: SendStream {
                            id: StreamId {
                                link: id,
                                dir: StreamDir::Out,
                                tag,
                            },
                            queue: q.tx.clone(),
                        },
                        control_rx: RecvStream {
                            id: StreamId {
                                link: id,
                                dir: StreamDir::In,
                                tag,
                            },
                            queue: q.rx.clone(),
                        },
                    };
                    self.publish();
                    let _ = self.events.send(ConfigEvent::Connected(Box::new(event)));
                }
                Effect::Ready(id) => {
                    if let Some(link) = self.core.links.get(&id) {
                        let peer = link.peer.device_id;
                        self.publish();
                        let _ = self.events.send(ConfigEvent::Ready { link: id, peer });
                        self.module_all(|| ModuleEvent::LinkReady { link: id, peer });
                    }
                }
                Effect::Lost {
                    link,
                    peer,
                    path,
                    reason,
                } => {
                    self.close_link_streams(link);
                    self.dials.retain(|dial| dial.peer != peer);
                    self.shared.release_dial(peer);
                    self.publish();
                    let _ = self.events.send(ConfigEvent::Lost {
                        link,
                        peer,
                        path,
                        reason,
                    });
                    self.module_all(|| ModuleEvent::LinkLost { link, peer, reason });
                }
                Effect::Dial { peer, addresses } => {
                    self.shared.track_dial(peer);
                    let mut unique = VecDeque::new();
                    for address in addresses {
                        if !unique.contains(&address) {
                            unique.push_back(address);
                        }
                    }
                    if let Some(dial) = self.dials.iter_mut().find(|dial| dial.peer == peer) {
                        // New hints join the same candidate owner. A second
                        // owner could report completion while this one's
                        // handshakes are still running.
                        dial.addresses.extend(unique);
                    } else {
                        self.dials.push(Dialing {
                            identity_mismatch: false,
                            peer,
                            addresses: unique,
                            attempts: Vec::new(),
                        });
                    }
                }
                Effect::DialFinished(peer) => {
                    self.dials.retain(|dial| dial.peer != peer);
                    self.shared.release_dial(peer);
                }
                Effect::DialFailed(peer, reason) => {
                    self.dials.retain(|dial| dial.peer != peer);
                    self.shared.release_dial(peer);
                    let _ = self.events.send(ConfigEvent::DialFailed { peer, reason });
                }
                Effect::Freeze(link) => {
                    self.publish_low();
                    for writer in self.writers.iter().filter(|w| w.id.is_some_and(|s| s.link == link)) {
                        if let Some(queue) = &writer.datagram {
                            while queue.take().ok().flatten().is_some() {
                                queue.dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                Effect::PathChanged { link, old } => {
                    self.ensure_n1(self.core.links[&link].hop.connection_id);
                    if let Some(uid) = self.physical_for(link) {
                        let route = self.core.links[&link].hop.route_tag;
                        let count = self.writers.len();
                        for _ in 0..count {
                            let mut writer = self.writers.pop_front().expect("writer");
                            if writer.id.is_some_and(|s| s.link == link) {
                                let flow = if writer.datagram.is_some() {
                                    let Some(flow) = self.flow(uid) else {
                                        writer.abort(StreamCode::LimitExceeded);
                                        continue;
                                    };
                                    flow
                                } else {
                                    0
                                };
                                writer.rebind(uid, route, flow);
                            }
                            self.writers.push_back(writer);
                        }
                        self.writers.retain(|w| {
                            !(w.id.is_none()
                                && w.header.route_tag == old.route_tag
                                && self
                                    .physical
                                    .get(&w.uid)
                                    .is_some_and(|p| p.id == Some(old.connection_id)))
                        });
                        self.readers.retain(|r| {
                            !(r.id.is_none()
                                && r.header.is_some_and(|h| h.route_tag == old.route_tag)
                                && self
                                    .physical
                                    .get(&r.uid)
                                    .is_some_and(|p| p.id == Some(old.connection_id)))
                        });
                    }
                    self.publish();
                    let path = LinkPath::Direct;
                    let _ = self.events.send(ConfigEvent::PathChanged { link, path });
                    self.module_all(|| ModuleEvent::PathChanged { link, path });
                }
                Effect::CloseTransit(path) => {
                    self.pipes.retain(|p| p.path != path);
                    self.publish_low();
                }
            }
        }
    }
    fn close_link_streams(&mut self, link: LinkId) {
        let mut ids = vec![];
        self.writers.retain_mut(|w| {
            if w.id.is_some_and(|s| s.link == link) {
                w.abort(StreamCode::None);
                ids.push(w.id.expect("id"));
                false
            } else {
                true
            }
        });
        self.readers.retain_mut(|r| {
            if r.id.is_some_and(|s| s.link == link) {
                r.stop(StreamCode::None);
                ids.push(r.id.expect("id"));
                false
            } else {
                true
            }
        });
        self.violations.retain(|(peer, path), _| {
            path.is_none()
                || self
                    .core
                    .links
                    .iter()
                    .any(|(id, l)| *id != link && l.peer == *peer && Some(l.wire.path_id) == *path)
        });
        self.logical.remove(&link);
        self.formats.remove(&link);
        self.audio.remove(&link);
        self.old_lanes.remove(&link);
        self.retired_members.remove(&link);
        if let Some(q) = self.controls.remove(&link) {
            q.tx.close();
            q.rx.close();
        }
        for id in ids {
            self.closed(id, StreamClosed::LinkLost);
        }
    }
    fn remove_physical(&mut self, uid: usize) {
        if let Some(physical) = self.physical.remove(&uid) {
            if let Some(selected) = physical.bootstrap.selected() {
                self.violations.remove(&(selected.peer_boot, None));
            }
            if let Some(id) = physical.id {
                self.connections.remove(&id);
                self.core.connection_lost(id, Instant::now());
            }
            if physical.outbound {
                self.core.dial_done(physical.peer, Instant::now());
            }
        }
        self.pipes.retain(|p| p.source != uid && p.target != uid);
        self.n1
            .retain(|connection, _| self.connections.contains_key(connection));
    }
    fn closed(&self, id: StreamId, reason: StreamClosed) {
        if id.tag.kind == StreamKind::Config {
            let _ = self.events.send(ConfigEvent::ControlClosed { stream: id, reason });
        } else {
            self.module_event(id.tag.kind, ModuleEvent::StreamClosed { stream: id, reason });
        }
    }
    fn publish(&self) {
        self.publish_low();
        let low_status = self.shared.hub.status.load();
        let low_failures = self.shared.hub.failures.load();
        let links = self
            .core
            .links
            .values()
            .filter(|l| l.installed)
            .map(|l| {
                let stats = self
                    .connections
                    .get(&l.hop.connection_id)
                    .and_then(|uid| self.physical.get(uid))
                    .map(|p| p.connection.stats());
                let pool = low_status
                    .iter()
                    .find(|s| s.connection == l.hop.connection_id)
                    .filter(|_| self.n1.contains_key(&l.hop.connection_id));
                let failure = low_failures
                    .iter()
                    .find(|(binding, _)| {
                        binding.path_id == l.wire.path_id && binding.route_revision == l.wire.route_revision
                    })
                    .map(|(_, failure)| failure.clone())
                    .or_else(|| pool.filter(|p| !p.available).and_then(|p| p.failure.clone()));
                LinkEntry {
                    link: l.id,
                    peer: l.peer.device_id,
                    peer_boot: l.peer,
                    path_id: l.wire.path_id,
                    route_revision: l.wire.route_revision.0,
                    agreement: self
                        .logical
                        .get(&l.id)
                        .cloned()
                        .or_else(|| {
                            (l.path() == LinkPath::Direct)
                                .then(|| {
                                    self.connections
                                        .get(&l.hop.connection_id)
                                        .and_then(|uid| self.physical.get(uid))
                                        .and_then(|p| p.bootstrap.selected())
                                        .map(|s| s.agreement.clone())
                                })
                                .flatten()
                        })
                        .unwrap_or_default(),
                    peer_audio: self.audio.get(&l.id).cloned().or_else(|| {
                        (l.path() == LinkPath::Direct)
                            .then(|| {
                                self.connections
                                    .get(&l.hop.connection_id)
                                    .and_then(|uid| self.physical.get(uid))
                                    .and_then(|p| p.bootstrap.selected())
                                    .and_then(|s| s.peer_audio().ok())
                            })
                            .flatten()
                    }),
                    peer_formats: self.formats.get(&l.id).cloned().or_else(|| {
                        (l.path() == LinkPath::Direct)
                            .then(|| {
                                self.connections
                                    .get(&l.hop.connection_id)
                                    .and_then(|uid| self.physical.get(uid))
                                    .and_then(|p| p.bootstrap.selected())
                                    .and_then(|s| s.peer_formats().ok())
                            })
                            .flatten()
                    }),
                    state: l.state,
                    path: l.path(),
                    remote_address: (l.path() == LinkPath::Direct)
                        .then(|| {
                            self.connections
                                .get(&l.hop.connection_id)
                                .and_then(|uid| self.physical.get(uid))
                                .map(|p| p.connection.remote_address())
                        })
                        .flatten(),
                    rtt: l.rtt(),
                    rtt_measured_at: l.rtt_measured_at(),
                    lost_packets: stats.as_ref().map_or(0, |s| s.path.lost_packets),
                    sent_packets: stats.as_ref().map_or(0, |s| s.path.sent_packets),
                    udp_bytes_sent: stats.as_ref().map_or(0, |s| s.udp_tx.bytes),
                    udp_datagrams_sent: stats.as_ref().map_or(0, |s| s.udp_tx.datagrams),
                    udp_bytes_received: stats.as_ref().map_or(0, |s| s.udp_rx.bytes),
                    low_available: failure.is_none() && pool.is_some_and(|p| p.available),
                    low_failure: failure,
                }
            })
            .collect();
        self.shared.links.store(Arc::new(LinkTable { links }));
        self.shared.scheduler.store(Arc::new(self.schedule.clone()));
        let mut counters = vec![];
        for w in &self.writers {
            if let (Some(id), Source::User(q)) = (w.id, &w.source) {
                counters.push(counter(id, q, w.datagram.as_deref()));
            }
        }
        for r in &self.readers {
            if let (Some(id), Some(q)) = (r.id, &r.queue) {
                counters.push(counter(id, q, r.datagram.as_deref()));
            }
        }
        self.shared.counters.store(Arc::new(counters));
    }
    fn command(&mut self) -> bool {
        let Ok(command) = self.commands.try_recv() else {
            return false;
        };
        match command {
            Command::ConfigFence(applied) => {
                let _ = applied.send(());
            }
            #[cfg(test)]
            Command::InspectConnections(reply) => {
                let _ = reply.send((
                    self.physical.len(),
                    self.dials.iter().map(|d| d.attempts.len()).sum(),
                    self.endpoint.open_connections(),
                ));
            }
            Command::Dial(peer, addresses, mode) => {
                if let Err(reason) = self.core.dial(peer, addresses, mode, Instant::now()) {
                    self.shared.release_dial(peer);
                    if reason != DialRefused::AlreadyLinked {
                        let _ = self.events.send(ConfigEvent::DialFailed {
                            peer,
                            reason: DialFailure::Refused,
                        });
                    }
                }
            }
            Command::Disconnect(id, reason) => self.core.lose(id, LinkLost::Local(reason)),
            Command::HalfOpen(id) => {
                self.core.set_half_open(id);
                self.publish();
            }
            Command::Ready(id) => {
                self.old_lanes.remove(&id);
                self.retired_members.remove(&id);
                self.core.set_ready(id);
            }
            Command::OldMemberLane(id, enabled) => {
                if enabled {
                    self.old_lanes.insert(id);
                } else {
                    self.old_lanes.remove(&id);
                }
                self.publish();
            }
            Command::Retire(id) => {
                let admitted = self.retired_members.contains(&id)
                    || self
                        .core
                        .links
                        .get(&id)
                        .is_some_and(|link| link.state == LinkState::Ready);
                self.core.retire_authority(id);
                if let Some(link) = self.core.links.get_mut(&id) {
                    let peer = link.peer.device_id;
                    link.state = LinkState::Connected;
                    self.old_lanes.insert(id);
                    if admitted {
                        self.retired_members.insert(id);
                    } else {
                        self.retired_members.remove(&id);
                    }
                    self.writers.retain_mut(|w| {
                        if w.id.is_some_and(|s| s.link == id && s.tag.kind.is_business()) {
                            w.abort(StreamCode::NotReady);
                            false
                        } else {
                            true
                        }
                    });
                    self.readers.retain_mut(|r| {
                        if r.id.is_some_and(|s| s.link == id && s.tag.kind.is_business()) {
                            r.stop(StreamCode::NotReady);
                            false
                        } else {
                            true
                        }
                    });
                    self.publish();
                    self.module_all(|| ModuleEvent::LinkLost {
                        link: id,
                        peer,
                        reason: LinkLost::Local(Disconnect::LeavingGroup),
                    });
                }
            }
            Command::Addresses(peer, addresses) => self.core.update_addresses(peer, addresses, Instant::now()),
            Command::Register(kinds, sender) => {
                for kind in kinds {
                    self.modules.insert(kind, sender.clone());
                }
            }
            Command::Open {
                id,
                priority,
                queue,
                datagram,
            } => {
                if let Some(uid) = self.physical_for(id.link) {
                    let flow = if datagram.is_some() {
                        let Some(flow) = self.flow(uid) else {
                            queue.close();
                            if let Some(q) = datagram {
                                q.close();
                            }
                            self.closed(id, StreamClosed::Peer(StreamCode::LimitExceeded));
                            return true;
                        };
                        flow
                    } else {
                        0
                    };
                    let header = StreamHeader {
                        route_tag: self.core.links[&id.link].hop.route_tag,
                        priority,
                        tag: id.tag,
                        datagram_flow: flow,
                    };
                    self.writers.push_back(Box::new(Writer::new(
                        uid,
                        header,
                        Some(id),
                        Source::User(queue),
                        datagram,
                    )));
                } else {
                    queue.close();
                    if let Some(q) = datagram {
                        q.close();
                    }
                    self.closed(id, StreamClosed::LinkLost);
                }
            }
            Command::Accept { id, queue, datagram } => {
                if let Some(reader) = self.readers.iter_mut().find(|r| r.id == Some(id) && r.queue.is_none()) {
                    queue.set_producer_wake(self.shared.wake_fn());
                    reader.queue = Some(queue);
                    reader.datagram = datagram;
                } else {
                    queue.close();
                    if let Some(q) = datagram {
                        q.close();
                    }
                }
            }
            Command::Close(id, code) => {
                let mut found = false;
                self.writers.retain_mut(|w| {
                    if w.id == Some(id) {
                        w.abort(code);
                        found = true;
                        false
                    } else {
                        true
                    }
                });
                self.readers.retain_mut(|r| {
                    if r.id == Some(id) {
                        r.stop(code);
                        found = true;
                        false
                    } else {
                        true
                    }
                });
                if found {
                    self.closed(id, StreamClosed::Local);
                }
            }
            Command::ProbeAddress(address, reply) => {
                if self.address_probes.len() >= MAX_DIALS {
                    let _ = reply.send(Err(AddressProbeError::Busy));
                } else {
                    let setup = || -> Result<_, AddressProbeError> {
                        let socket = super::probe_socket::Socket::new(address, self.shared.wake.clone())
                            .map_err(|e| AddressProbeError::from_io(&e))?;
                        let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
                            Default::default(),
                            None,
                            socket.clone(),
                            Arc::new(quinn::TokioRuntime),
                        )
                        .map_err(|e| AddressProbeError::from_io(&e))?;
                        endpoint.set_default_client_config(self.probe_client.clone());
                        let connecting = endpoint
                            .connect(address, crate::tls::SERVER_NAME)
                            .map_err(AddressProbeError::from_connect)?;
                        Ok((endpoint, socket, connecting))
                    };
                    match setup() {
                        Ok((endpoint, socket, connecting)) => self.address_probes.push(AddressProbe {
                            endpoint,
                            socket,
                            handshake: Handshake {
                                trace: None,
                                future: Box::pin(connecting),
                                peer: None,
                                id: Some(ConnectionId {
                                    initiator_boot: self.core.boot,
                                    random: rand::random(),
                                }),
                                deadline: Instant::now() + ADDRESS_QUERY_BUDGET,
                            },
                            reply,
                        }),
                        Err(error) => {
                            let _ = reply.send(Err(error));
                        }
                    }
                }
            }
            Command::Agreement(link, agreement) => {
                if self.core.links.contains_key(&link) {
                    self.logical.insert(link, agreement);
                }
            }
            Command::Audio(link, audio) => {
                if self.core.links.contains_key(&link) {
                    self.audio.insert(link, audio);
                }
            }
            Command::Formats(link, formats) => {
                if self.core.links.contains_key(&link) {
                    self.formats.insert(link, formats);
                }
            }
            Command::Violation(link, violation) => {
                if let Some(l) = self.core.links.get(&link) {
                    self.report_violation(l.peer, Some(l.wire.path_id), violation);
                }
            }
            #[cfg(any(test, feature = "test-support"))]
            Command::BlockDirect(peer, blocked) => {
                if blocked {
                    self.blocked_direct.insert(peer);
                    for physical in self.physical.values().filter(|p| p.peer == peer) {
                        physical.connection.close(0u32.into(), b"test direct path unavailable");
                    }
                } else {
                    self.blocked_direct.remove(&peer);
                }
            }
            Command::Stop => self.stop = true,
            #[cfg(test)]
            Command::ResetN1(link) => {
                if let Some(uid) = self.physical_for(link) {
                    self.close_n1(uid);
                }
            }
            Command::Probe(connection, active) => {
                if active {
                    self.probes.entry(connection).or_insert(low::Probe {
                        next: Instant::now(),
                        nonce: 0,
                        pending: None,
                    });
                } else {
                    self.probes.remove(&connection);
                }
            }
            Command::LowInbound(inbound) => {
                let link = inbound.stream.link();
                if self.core.links.get(&link).is_some_and(|l| l.state == LinkState::Ready) {
                    self.module_event(inbound.stream.kind(), ModuleEvent::InboundLow(inbound));
                }
            }
        }
        self.effects();
        true
    }
    fn service(&mut self, class: u8, until: Instant, cx: &mut Context<'_>) -> bool {
        let mut progressed = false;
        // Carry an unfinished empty sweep across budget boundaries. With many
        // streams, requiring a complete sweep in one 100/250/150 us slice would
        // otherwise keep a wholly idle network awake forever.
        let mut empty = self.idle_visits[class as usize];
        let visits = 5
            * (1 + self
                .writers
                .len()
                .max(self.readers.len())
                .max(self.pipes.len())
                .max(self.physical.len()));
        while empty < visits && !self.stop {
            // Reuse the budget check's clock read for this polling iteration.
            // Each iteration still checks its budget against a fresh instant.
            let now = Instant::now();
            if now >= until {
                break;
            }
            let cursor = self.cursors[class as usize];
            self.cursors[class as usize] = (cursor + 1) % 5;
            let work = match cursor {
                0 => self.write_one(class, cx),
                1 => self.read_one(class, cx),
                2 => self.pipe_one(class, cx),
                3 if class == 1 => self.datagram_one(cx),
                3 if class == 2 => self.command(),
                4 if class == 2 => self.connections_one(now, cx),
                _ => false,
            };
            self.effects();
            if work {
                progressed = true;
                self.idle_visits = [0; 3];
                empty = 0;
            } else {
                empty += 1;
            }
        }
        self.idle_visits[class as usize] = empty;
        // A time-limited pass has not yet proved that every source is idle.
        progressed || empty < visits
    }
    fn write_one(&mut self, class: u8, cx: &mut Context<'_>) -> bool {
        let Some(index) = self.writers.iter().position(|w| stream_class(w.header) == class) else {
            return false;
        };
        let mut writer = self.writers.remove(index).expect("writer");
        let frozen = writer
            .id
            .and_then(|id| self.core.links.get(&id.link))
            .is_some_and(|l| l.frozen());
        if frozen && let Some(q) = &writer.datagram {
            while q.take().ok().flatten().is_some() {
                q.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        let result = self
            .physical
            .get(&writer.uid)
            .map_or(Err(StreamClosed::LinkLost), |p| writer.step(&p.connection, frozen, cx));
        match result {
            Ok(progress) => {
                self.writers.push_back(writer);
                progress
            }
            Err(reason) => {
                writer.abort(match reason {
                    StreamClosed::Peer(code) => code,
                    _ => StreamCode::None,
                });
                if let Some(id) = writer.id {
                    self.closed(id, reason);
                } else {
                    self.private_closed(writer.uid, writer.header);
                }
                true
            }
        }
    }
    fn read_one(&mut self, class: u8, cx: &mut Context<'_>) -> bool {
        let Some(index) = self
            .readers
            .iter()
            .position(|r| r.header.map_or(2, stream_class) == class)
        else {
            return false;
        };
        let mut reader = self.readers.remove(index).expect("reader");
        if reader.pending_admission {
            let ready = reader
                .id
                .and_then(|id| self.core.links.get(&id.link))
                .is_some_and(|link| link.state == LinkState::Ready);
            if ready {
                reader.pending_admission = false;
                let header = reader.header.expect("held admission header").to_wire();
                self.header(reader, header);
                return true;
            }
            self.readers.push_back(reader);
            return false;
        }
        if let Some(delivery) = reader.pending_low.take() {
            reader.pending_low = self.deliver_low(delivery);
            let progress = reader.pending_low.is_none();
            self.readers.push_back(reader);
            return progress;
        }
        let mut rejected = None;
        let result = if self.physical.contains_key(&reader.uid) {
            let uid = reader.uid;
            reader.step(cx, |header, frame| {
                self.admission(uid, header)
                    .ok_or(StreamClosed::Peer(StreamCode::NotReady))?
                    .admit(frame)
                    .map_err(|violation| {
                        let code = StreamCode::from_wire(violation.code.number());
                        rejected = Some(violation);
                        StreamClosed::Peer(code)
                    })
            })
        } else {
            Err(StreamClosed::LinkLost)
        };
        if let Some(violation) = rejected {
            let endpoint = reader
                .id
                .and_then(|id| self.core.links.get(&id.link))
                .map(|l| (l.peer, Some(l.wire.path_id)))
                .or_else(|| {
                    self.physical
                        .get(&reader.uid)
                        .and_then(|p| p.bootstrap.selected())
                        .map(|s| (s.peer_boot, None))
                });
            if let Some((peer, path)) = endpoint {
                self.report_violation(peer, path, violation);
            }
        }
        match result {
            Ok(Delivery::Header(header)) => {
                self.header(reader, header);
                true
            }
            Ok(Delivery::Message(bytes)) => {
                let header = reader.header.expect("header");
                if header.tag.kind == StreamKind::LowControl {
                    reader.pending_low = self.low_message(reader.uid, bytes);
                } else {
                    self.message(reader.uid, header, bytes);
                }
                self.readers.push_back(reader);
                true
            }
            Ok(Delivery::Finished) => {
                if reader.id.is_some() {
                    // FIN is only the cutover drain marker; ordinary
                    // application closure uses RESET. It can precede MigrateBegin
                    // on N0, so retain the finished reader and its queues even
                    // before this side has frozen. The new stream reuses them.
                    self.readers.push_back(reader);
                } else {
                    self.private_closed(reader.uid, reader.header.expect("header"));
                }
                true
            }
            Ok(delivery) => {
                self.readers.push_back(reader);
                matches!(delivery, Delivery::Progress)
            }
            Err(reason) => {
                reader.stop(match reason {
                    StreamClosed::Peer(code) => code,
                    _ => StreamCode::None,
                });
                if let Some(id) = reader.id {
                    self.closed(id, reason);
                } else if let Some(header) = reader.header {
                    self.private_closed(reader.uid, header);
                }
                true
            }
        }
    }
    fn private_closed(&mut self, uid: usize, header: StreamHeader) {
        if header.tag.kind == StreamKind::LowControl {
            self.close_n1(uid);
            return;
        }
        if header.tag.kind != StreamKind::Internal {
            return;
        }
        if header.route_tag == 0
            && let Some(p) = self.physical.get(&uid)
        {
            let (waiting, waiting_on) =
                p.id.map(|id| self.core.selection_wait(id))
                    .unwrap_or(("instance", "peer"));
            tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", peer_id = p.peer.to_hex().as_str(),
                operation = p.id.map(|id| diagnostics::hex(&id.random)).unwrap_or_default().as_str(),
                stage = if waiting == "none" { "quic" } else if waiting == "instance" { "instance" } else { "selection" },
                result = "failed", reason = "control_stream_failed", waiting, waiting_on, "Connection internal stream closed");
        }
        let Some(connection) = self.physical.get(&uid).and_then(|p| p.id) else {
            self.remove_physical(uid);
            return;
        };
        if header.route_tag == 0 {
            self.remove_physical(uid);
        } else {
            self.core.path_internal_closed(connection, header.route_tag);
        }
    }
    fn header(&mut self, mut reader: Box<Reader>, raw_header: v1::frame::StreamHeader) {
        let Some(physical) = self.physical.get(&reader.uid) else {
            return;
        };
        if let Some(connection) = physical.id
            && let Some((other, path)) = self.core.relay_route(connection, raw_header.route_tag)
        {
            let Some(&target) = self.connections.get(&other) else {
                reader.stop(StreamCode::None);
                return;
            };
            let flow = if raw_header.datagram_flow != 0 {
                let Some(flow) = self.flow(target) else {
                    reader.stop(StreamCode::LimitExceeded);
                    return;
                };
                flow
            } else {
                0
            };
            self.pipes.push_back(Box::new(Pipe::new(
                path,
                reader.uid,
                target,
                reader.stream,
                raw_header,
                flow,
            )));
            return;
        }
        let encoded = raw_header.encode().expect("validated stream header");
        let Ok(Some((header, _))) = StreamHeader::decode(&encoded) else {
            reader.stop(StreamCode::Unsupported);
            return;
        };
        reader.header = Some(header);
        if self
            .admission(reader.uid, header)
            .is_none_or(|context| context.admit_stream(&header.to_wire()).is_err())
        {
            reader.stop(StreamCode::Invalid);
            return;
        }
        if header.tag.kind == StreamKind::Internal || header.tag.kind == StreamKind::LowControl {
            if header.tag.kind == StreamKind::Internal && header.priority != Priority::Interactive {
                reader.stop(StreamCode::Invalid);
                return;
            }
            self.readers.push_back(reader);
            return;
        }
        let Some(connection) = physical.id else {
            reader.stop(StreamCode::Invalid);
            return;
        };
        let Some(link) = self.core.route(connection, raw_header.route_tag) else {
            reader.stop(StreamCode::Invalid);
            return;
        };
        let id = StreamId {
            link,
            dir: StreamDir::In,
            tag: header.tag,
        };
        reader.id = Some(id);
        if let Some(index) = self.readers.iter().position(|r| r.id == Some(id) && r.finished) {
            let old = self.readers.remove(index).expect("finished reader");
            reader.queue = old.queue;
            reader.datagram = old.datagram;
            self.readers.push_back(reader);
            return;
        }
        if header.tag.kind == StreamKind::Config {
            self.control_queues(link);
            if let Some(q) = self.controls.get(&link) {
                reader.queue = Some(q.rx.clone());
            }
            self.readers.push_back(reader);
            return;
        }
        if self
            .core
            .links
            .get(&link)
            .is_some_and(|state| state.state == LinkState::HalfOpen)
        {
            // Cross-stream QUIC ordering does not order I0/M0 after C0.Done.
            // Keep only the bounded header until C0 validates that exact Done.
            reader.pending_admission = true;
            self.readers.push_back(reader);
            return;
        }
        if let Err(code) = self.core.inbound(link) {
            reader.stop(code);
            return;
        }
        self.effects();
        if !self.modules.contains_key(&header.tag.kind) {
            reader.stop(StreamCode::Unsupported);
            return;
        }
        let inbound = InboundStream {
            link,
            peer: self.core.links[&link].peer.device_id,
            tag: header.tag,
            priority: header.priority,
            has_datagram: raw_header.datagram_flow != 0,
            command: Some(self.shared.commands.clone()),
            wake: self.shared.wake.clone(),
        };
        self.module_event(header.tag.kind, ModuleEvent::Inbound(inbound));
        self.readers.push_back(reader);
    }
    fn message(&mut self, uid: usize, header: StreamHeader, bytes: Vec<u8>) {
        let Some(physical) = self.physical.get_mut(&uid) else {
            return;
        };
        if !physical.bootstrap.complete() {
            if header.route_tag != 0 {
                self.remove_physical(uid);
                return;
            }
            let Ok(replies) = physical.bootstrap.receive(&bytes, Instant::now()) else {
                self.remove_physical(uid);
                return;
            };
            let selected = physical
                .bootstrap
                .complete()
                .then(|| physical.bootstrap.selected().cloned())
                .flatten();
            for reply in replies {
                self.send_private(uid, 0, reply);
            }
            if let Some(selected) = selected {
                if self
                    .connections
                    .get(&selected.connection)
                    .is_some_and(|old| *old != uid)
                {
                    self.remove_physical(uid);
                    return;
                }
                self.physical.get_mut(&uid).expect("candidate").id = Some(selected.connection);
                self.connections.insert(selected.connection, uid);
                self.core
                    .authenticated(selected.peer_boot, selected.connection, Instant::now());
            }
            return;
        }
        let Ok(message) = super::control_wire::decode(&bytes) else {
            self.private_closed(uid, header);
            return;
        };
        if let Some(connection) = physical.id {
            if let LinkMsg::HopPong { nonce } = message {
                self.probe_pong(connection, nonce);
            } else {
                let route_change = matches!(
                    message,
                    LinkMsg::PathInstall(_)
                        | LinkMsg::PathInstalled(_)
                        | LinkMsg::PathCancel { .. }
                        | LinkMsg::PathDown { .. }
                );
                self.core.receive(connection, header.route_tag, message, Instant::now());
                if route_change {
                    self.publish_low();
                }
            }
        }
    }
    fn admission(&self, uid: usize, header: StreamHeader) -> Option<Admission<'_>> {
        let physical = self.physical.get(&uid)?;
        let selected = physical.bootstrap.selected();
        static EMPTY: std::sync::LazyLock<Agreement> = std::sync::LazyLock::new(Agreement::default);
        let mut agreement = selected.map(|s| &s.agreement).unwrap_or(&EMPTY);
        let mut phase = if physical.bootstrap.complete() {
            Phase::Member
        } else {
            Phase::PhysicalBootstrap
        };
        let mut direct = true;
        let mut old_member_lane = false;
        if header.tag.kind == StreamKind::Config || header.tag.kind.is_business() {
            let link = self.core.route(physical.id?, header.route_tag)?;
            let link_state = self.core.links.get(&link)?;
            old_member_lane = self.old_lanes.contains(&link);
            direct = link_state.path() == LinkPath::Direct;
            if let Some(logical) = self.logical.get(&link) {
                agreement = logical;
            }
            // In-flight member frames may precede our migration proof or signed
            // departure. Preserve only C0 frame admission for a previously ready
            // lane; its owner validates/drains old frames without applying them.
            // Business streams remain revoked until fresh admission completes.
            phase = if link_state.state == LinkState::Ready
                || link_state.state == LinkState::HalfOpen
                || (header.tag.kind == StreamKind::Config && self.retired_members.contains(&link))
            {
                Phase::Member
            } else if direct || self.logical.contains_key(&link) {
                Phase::Admission
            } else {
                Phase::LogicalBootstrap
            };
        }
        Some(Admission {
            kind: header.tag.kind as u16,
            transport: Transport::Reliable,
            phase,
            agreement,
            direct,
            old_member_lane,
        })
    }
    fn pipe_one(&mut self, class: u8, cx: &mut Context<'_>) -> bool {
        let Some(index) = self.pipes.iter().position(|p| {
            (if p.header.kind == 0 {
                0
            } else if p.header.priority == 0 {
                1
            } else {
                2
            }) == class
        }) else {
            return false;
        };
        let mut pipe = self.pipes.remove(index).expect("pipe");
        let progress = if let Some(p) = self.physical.get(&pipe.target) {
            pipe.step(&p.connection, cx)
        } else {
            pipe.stop(0u32.into());
            true
        };
        if !pipe.done() {
            self.pipes.push_back(pipe);
        }
        progress
    }
    fn check_drained(&mut self) {
        let ids: Vec<_> = self.core.links.values().filter(|l| l.frozen()).map(|l| l.id).collect();
        for id in ids {
            let writers = self
                .writers
                .iter()
                .filter(|w| w.id.is_some_and(|s| s.link == id) && !w.finished)
                .count();
            let readers = self
                .readers
                .iter()
                .filter(|r| r.id.is_some_and(|s| s.link == id) && !r.finished)
                .count();
            self.core.drain_counts(id, writers, readers);
            if writers == 0 && readers == 0 {
                self.core.drained(id);
            }
        }
    }
    fn datagram_one(&mut self, cx: &mut Context<'_>) -> bool {
        let mut progress = false;
        if self.physical.is_empty() {
            return false;
        }
        self.datagram_cursor = (self.datagram_cursor + 1) % self.physical.len();
        let uid = *self.physical.keys().nth(self.datagram_cursor).expect("physical cursor");
        let connection = self.physical[&uid].connection.clone();
        if let Poll::Ready(Ok(bytes)) = self.physical.get_mut(&uid).expect("physical").datagrams.poll(cx, || {
            let connection = connection.clone();
            Box::pin(async move { connection.read_datagram().await })
        }) {
            progress = true;
            if (4..=frame::MAX_DATAGRAM).contains(&bytes.len()) {
                let flow = u32::from_le_bytes(bytes[..4].try_into().expect("flow prefix"));
                let prefix = 4;
                if flow == 0 {
                    return progress;
                }
                if let Some(pipe) = self.pipes.iter().find(|p| p.source == uid && p.original_flow == flow) {
                    let _ = pipe.datagrams.post_lossy(&bytes[prefix..]);
                } else if let Some(reader) = self
                    .readers
                    .iter()
                    .find(|r| r.uid == uid && r.header.is_some_and(|h| h.datagram_flow == flow))
                    && let Some(queue) = &reader.datagram
                {
                    if reader
                        .id
                        .and_then(|id| self.core.links.get(&id.link))
                        .is_some_and(|l| l.frozen())
                    {
                        queue.dropped.fetch_add(1, Ordering::Relaxed);
                    } else if let Some(mut admission) = reader.header.and_then(|header| self.admission(uid, header)) {
                        admission.transport = Transport::Datagram;
                        if let Ok(frame) =
                            v1::frame::OpaqueFrame::parse_exact(&bytes[prefix..], v1::limits::DATAGRAM - 4)
                            && admission.admit(frame.header).is_ok()
                        {
                            let _ = queue.post_lossy(frame.bytes());
                        }
                    }
                }
            }
        }
        let count = self
            .writers
            .iter()
            .filter(|w| w.uid == uid && w.stream.is_some() && !w.finished && w.datagram.is_some())
            .count()
            + self.pipes.iter().filter(|p| p.target == uid).count();
        let cursor = &mut self.physical.get_mut(&uid).expect("physical").datagram_source_cursor;
        if count == 0 {
            return progress;
        }
        let start = (*cursor + 1) % count;
        // Visit the suffix, then the prefix, without allocating or rescanning for
        // each idle flow. A delayed flow does not hold up unrelated datagrams.
        for (skip, take) in [(start, count - start), (0, start)] {
            let sources = self
                .writers
                .iter_mut()
                .filter(|w| w.uid == uid && w.stream.is_some() && !w.finished)
                .filter_map(|w| w.datagram.as_ref().map(|q| (w.header.datagram_flow, q)))
                .chain(
                    self.pipes
                        .iter_mut()
                        .filter(|p| p.target == uid)
                        .map(|p| (p.header.datagram_flow, &p.datagrams)),
                );
            for (index, (flow, queue)) in sources.enumerate().skip(skip).take(take) {
                *cursor = index;
                self.datagram_buffer.clear();
                self.datagram_buffer.extend_from_slice(&flow.to_le_bytes());
                let prefix = self.datagram_buffer.len();
                self.datagram_buffer.resize(frame::MAX_DATAGRAM, 0);
                if let Ok(Some(len)) = queue.take_into(&mut self.datagram_buffer[prefix..]) {
                    self.datagram_buffer.truncate(prefix + len);
                    let _ = connection.send_datagram(bytes::Bytes::copy_from_slice(&self.datagram_buffer));
                    return true;
                }
            }
        }
        progress
    }
    fn connections_one(&mut self, now: Instant, cx: &mut Context<'_>) -> bool {
        let mut progress = false;
        let lost: Vec<_> = self
            .physical
            .iter()
            .filter(|(_, p)| {
                p.connection.close_reason().is_some()
                    || now >= p.deadline && p.id.is_none_or(|id| !self.core.selected_connection_id(id))
            })
            .map(|(uid, _)| *uid)
            .collect();
        for uid in lost {
            if let Some(p) = self.physical.get(&uid) {
                let error = p.connection.close_reason();
                let (waiting, waiting_on) =
                    p.id.map(|id| self.core.selection_wait(id))
                        .unwrap_or(("instance", "peer"));
                tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", peer_id = p.peer.to_hex().as_str(),
                    operation = p.id.map(|id| diagnostics::hex(&id.random)).unwrap_or_default().as_str(),
                    stage = match p.id { None => "instance", Some(id) if !self.core.selected_connection_id(id) => "selection", Some(_) => "quic" },
                    result = "failed", waiting, waiting_on, reason = error.as_ref().map_or("timeout", diagnostics::quic_reason),
                    elapsed_ms = now.saturating_duration_since(p.deadline - DIAL_BUDGET).as_secs_f64() * 1000., error = ?error, "Physical connection ended");
            }
            self.remove_physical(uid);
            progress = true;
        }
        let keys: Vec<_> = self.physical.keys().copied().collect();
        if !keys.is_empty() {
            self.connection_cursor = (self.connection_cursor + 1) % keys.len();
            let uid = keys[self.connection_cursor];
            let physical = self.physical.get_mut(&uid).expect("physical");
            let connection = physical.connection.clone();
            if let Poll::Ready(Ok(stream)) = physical.incoming.poll(cx, || {
                let connection = connection.clone();
                Box::pin(async move { connection.accept_uni().await })
            }) {
                self.readers.push_back(Box::new(Reader::new(uid, stream)));
                progress = true;
            }
        }
        if let Poll::Ready(Some(incoming)) = self.accepting.poll(cx, || {
            let endpoint = self.endpoint.clone();
            Box::pin(async move { endpoint.accept().await })
        }) {
            let pending = self.inbound.len()
                + self
                    .physical
                    .values()
                    .filter(|p| !p.outbound && p.id.is_none_or(|id| !self.core.selected_connection_id(id)))
                    .count();
            if pending >= MAX_INBOUND_PENDING {
                incoming.refuse();
            } else if let Ok(connecting) = incoming.accept() {
                self.inbound.push(Handshake {
                    trace: None,
                    future: Box::pin(connecting),
                    peer: None,
                    id: None,
                    deadline: now + DIAL_BUDGET,
                });
            }
            progress = true;
        }
        let mut index = 0;
        while index < self.inbound.len() {
            if now >= self.inbound[index].deadline {
                self.inbound.swap_remove(index);
                progress = true;
                continue;
            }
            match self.inbound[index].future.as_mut().poll(cx) {
                Poll::Ready(result) => {
                    let handshake = self.inbound.swap_remove(index);
                    if let Ok(connection) = result {
                        self.established(connection, handshake.peer, handshake.id, None);
                    }
                    progress = true;
                }
                Poll::Pending => index += 1,
            }
        }
        let mut probe_index = 0;
        while probe_index < self.address_probes.len() {
            let error = self.address_probes[probe_index]
                .socket
                .error
                .lock()
                .expect("probe socket error")
                .take();
            if let Some(error) = error {
                let probe = self.address_probes.swap_remove(probe_index);
                let _ = probe.reply.send(Err(error));
                progress = true;
                continue;
            }
            if self.address_probes[probe_index].reply.is_closed()
                || now >= self.address_probes[probe_index].handshake.deadline
            {
                let probe = self.address_probes.swap_remove(probe_index);
                let _ = probe.reply.send(Err(AddressProbeError::Timeout));
                progress = true;
                continue;
            }
            match self.address_probes[probe_index].handshake.future.as_mut().poll(cx) {
                Poll::Ready(result) => {
                    let probe = self.address_probes.swap_remove(probe_index);
                    let result = match result {
                        Ok(connection) => {
                            let peer = tls_peer(&connection);
                            match peer {
                                Some(peer) if peer == self.core.boot.device_id => {
                                    connection.close(11u32.into(), b"self identity");
                                    Err(AddressProbeError::SelfDevice)
                                }
                                Some(peer)
                                    if self.established(
                                        connection,
                                        Some(peer),
                                        probe.handshake.id,
                                        Some(probe.endpoint),
                                    ) =>
                                {
                                    Ok(peer)
                                }
                                _ => Err(AddressProbeError::Authentication),
                            }
                        }
                        Err(error) => Err(AddressProbeError::from_connection(error)),
                    };
                    let _ = probe.reply.send(result);
                    progress = true;
                }
                Poll::Pending => probe_index += 1,
            }
        }
        let mut index = 0;
        let mut active: usize = self.dials.iter().map(|d| d.attempts.len()).sum();
        while index < self.dials.len() {
            let mut dial = self.dials.remove(index);
            let mut winner = false;
            let mut candidate = 0;
            while candidate < dial.attempts.len() {
                let attempt = &mut dial.attempts[candidate];
                if now >= attempt.deadline {
                    // Dropping Quinn's Connecting releases its connection reference
                    // and closes the unfinished QUIC connection, including a TLS
                    // result that completed before we got to poll it.
                    if let Some(trace) = &mut attempt.trace {
                        trace.end("failed", "timeout", "QUIC dial deadline");
                    }
                    dial.attempts.remove(candidate);
                    active -= 1;
                    progress = true;
                } else if let Poll::Ready(result) = attempt.future.as_mut().poll(cx) {
                    let mut attempt = dial.attempts.remove(candidate);
                    active -= 1;
                    progress = true;
                    match result {
                        Ok(connection) => {
                            dial.identity_mismatch |= tls_peer(&connection).is_some_and(|peer| peer != dial.peer);
                            let accepted = self.established(connection, Some(dial.peer), attempt.id, None);
                            if let Some(trace) = &mut attempt.trace {
                                trace.end(
                                    if accepted { "completed" } else { "failed" },
                                    if accepted { "authenticated" } else { "identity_rejected" },
                                    "",
                                );
                            }
                            if accepted {
                                winner = true;
                                break;
                            }
                        }
                        Err(error) => {
                            if let Some(trace) = &mut attempt.trace {
                                trace.end("failed", diagnostics::quic_reason(&error), &error.to_string());
                            }
                        }
                    }
                } else {
                    candidate += 1;
                }
            }
            if winner {
                // TLS validated the expected device identity. Cancel this peer's
                // other candidates immediately; the existing smaller-ID arbiter
                // still selects/installs across any simultaneous reverse dial.
                active -= dial.attempts.len();
                continue;
            }
            if dial.attempts.is_empty() {
                if dial.addresses.is_empty() {
                    if dial.identity_mismatch {
                        self.core.dial_identity_mismatch(dial.peer, now);
                    } else {
                        self.core.dial_done(dial.peer, now);
                    }
                    progress = true;
                    continue;
                }
                // Simple bounded rounds: fill currently free global slots in
                // candidate order, wait for this round, then try the remainder.
                // No budget is consumed while waiting for a slot.
                while active < MAX_DIALS {
                    let Some(address) = dial.addresses.pop_front() else {
                        break;
                    };
                    let id = ConnectionId {
                        initiator_boot: self.core.boot,
                        random: rand::random(),
                    };
                    let mut trace = diagnostics::CandidateDial::start(dial.peer, address, diagnostics::hex(&id.random));
                    match self.endpoint.connect(address, crate::tls::SERVER_NAME) {
                        Ok(connecting) => {
                            dial.attempts.push(Handshake {
                                trace: Some(trace),
                                future: Box::pin(connecting),
                                peer: Some(dial.peer),
                                id: Some(id),
                                deadline: now + DIAL_BUDGET,
                            });
                            active += 1;
                        }
                        Err(error) => trace.end("failed", "connect_error", &error.to_string()),
                    }
                    progress = true;
                }
            }
            self.dials.insert(index, dial);
            index += 1;
        }
        progress
    }
    fn established(
        &mut self,
        connection: quinn::Connection,
        expected: Option<DeviceId>,
        id: Option<ConnectionId>,
        probe_endpoint: Option<quinn::Endpoint>,
    ) -> bool {
        let peer = tls_peer(&connection);
        let Some(peer) =
            peer.filter(|peer| *peer != self.core.boot.device_id && expected.is_none_or(|expected| expected == *peer))
        else {
            connection.close(0u32.into(), b"identity rejected");
            return false;
        };
        #[cfg(any(test, feature = "test-support"))]
        if self.blocked_direct.contains(&peer) {
            connection.close(0u32.into(), b"test direct path unavailable");
            return false;
        }
        let Ok((bootstrap, offer)) =
            crate::negotiation::Bootstrap::new(self.core.boot, peer, id, self.profile.clone(), Instant::now())
        else {
            connection.close(4u32.into(), b"invalid local offer");
            return false;
        };
        self.next_uid += 1;
        let uid = self.next_uid;
        self.physical.insert(
            uid,
            Physical {
                connection,
                _probe_endpoint: probe_endpoint,
                peer,
                id,
                deadline: bootstrap.deadline,
                outbound: id.is_some(),
                datagram_source_cursor: 0,
                next_flow: 0,
                bootstrap,
                incoming: Default::default(),
                datagrams: Default::default(),
            },
        );
        if let Some(offer) = offer {
            self.send_private(uid, 0, offer);
        }
        true
    }
}
fn tls_peer(connection: &quinn::Connection) -> Option<DeviceId> {
    connection
        .peer_identity()
        .and_then(|i| i.downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>().ok())
        .and_then(|chain| chain.first().and_then(|cert| crate::tls::device_id_from_cert(cert)))
}
fn stream_class(header: StreamHeader) -> u8 {
    if header.tag.kind == StreamKind::Internal {
        0
    } else if header.priority == Priority::Interactive {
        1
    } else {
        2
    }
}
fn counter(stream: StreamId, queue: &queue::Queue, datagram: Option<&queue::DatagramQueue>) -> StreamCounters {
    StreamCounters {
        stream,
        depth: (queue.len() + datagram.map_or(0, queue::DatagramQueue::len)) as u32,
        dropped: queue.dropped.load(Ordering::Relaxed) + datagram.map_or(0, |q| q.dropped.load(Ordering::Relaxed)),
        full: queue.full.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests;
