//! Diagnostics lifecycle uses D0; explicit saves use independent Low operations.
//! Recording continues while sharing is paused. Native records are projected and
//! written on ordinary threads, with no raw payloads or endpoint identities.

pub const KINDS: &[StreamKind] = &[StreamKind::Diagnostics];

use super::{
    StreamModule,
    machine::{Admit, Failure, InboundOrdinal, Step, StreamMachine},
};
use crate::config_module::ConfigModule;
use oink_foundation::DeviceId;
use oink_network::link::{
    InboundLow, InboundStream, LinkId, LinkLayer, LinkLost, PostError, QueueSpec, RecvStream, SendStream, StreamClosed,
    StreamDir, StreamId,
};
use oink_protocol::{
    frame::{StreamCode, StreamKind},
    v1::{
        self, Message, RawValue, dto,
        schema::{WireDto, WireValue},
    },
};
use std::{collections::HashMap, time::Instant};

#[derive(Debug)]
struct Peer {
    link: LinkId,
    machine: StreamMachine,
    ordinal: InboundOrdinal,
    tx: Option<SendStream>,
    rx: Option<RecvStream>,
    wanted: bool,
    supported: bool,
    opening: Vec<Message>,
    received: Vec<Message>,
    operations: Vec<tokio::task::JoinHandle<()>>,
}

/// Ordinary-thread owner of this module's streams. `poll` returns messages to its
/// business consumer; it never waits for another module or for a low-priority operation.
#[derive(Debug)]
pub struct DiagnosticsStreams {
    links: LinkLayer,
    config: ConfigModule,
    wake: std::sync::Arc<tokio::sync::Notify>,
    peers: HashMap<DeviceId, Peer>,
    low: Vec<InboundLow>,
    closed: Vec<(DeviceId, StreamId)>,
    recorder: Option<crate::diagnostics::Recorder>,
    instances: HashMap<DeviceId, String>,
    hello: Option<Message>,
}

impl DiagnosticsStreams {
    pub fn new(links: LinkLayer, config: ConfigModule) -> Self {
        Self {
            links,
            config,
            wake: Default::default(),
            peers: HashMap::new(),
            low: Vec::new(),
            closed: Vec::new(),
            recorder: None,
            instances: HashMap::new(),
            hello: None,
        }
    }

    /// Clone before the task loop, then await `notified()` alongside module/config
    /// events and the retry deadline. Poll this owner after any of those wake-ups.
    pub fn wake(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.wake.clone()
    }

    /// Replace current state before the next open; no historical message is retained.
    pub fn set_opening(&mut self, peer: DeviceId, messages: Vec<Message>) {
        assert!(messages.iter().all(Self::valid), "message belongs to another stream");
        if let Some(state) = self.peers.get_mut(&peer) {
            state.wanted |= !messages.is_empty();
            state.opening = messages;
        }
    }

    fn valid(message: &Message) -> bool {
        matches!(message, Message::DiagnosticsHello(_) | Message::DiagnosticsGoodbye(_))
    }
    fn supported(&self, peer: DeviceId) -> bool {
        self.config
            .peer_agreement(peer)
            .is_some_and(|a| a.contains("oink.diagnostics.collect/1"))
    }

    fn advance(&mut self, peer: DeviceId, now: Instant) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        let allowed = state.supported;
        let step = state.machine.poll(allowed, state.wanted, now);
        self.step(peer, step, now);
    }

    fn step(&mut self, peer: DeviceId, step: Step, now: Instant) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        let open = match step {
            Step::Nothing => return,
            Step::Close(_) => None,
            Step::Open(tag) | Step::Reopen { open: tag, .. } => Some(tag),
        };
        if let Some(tx) = state.tx.take() {
            tracing::debug!(?peer, stream=?tx.id(), "closing Diagnostics stream");
            self.links.close_stream(tx.id(), StreamCode::None);
        }
        let Some(tag) = open else {
            return;
        };
        let opened = self.links.open_stream(
            state.link,
            StreamKind::Diagnostics.priority(),
            tag,
            QueueSpec::reliable(StreamKind::Diagnostics).0,
            None,
        );
        let Ok((tx, _)) = opened else {
            state.machine.poll(false, state.wanted, now);
            return;
        };
        tracing::debug!(?peer, stream=?tx.id(), "opening Diagnostics stream");
        state.tx = Some(tx);
        let opening = state.opening.clone();
        for message in opening {
            if self.post(peer, &message, now).is_err() {
                break;
            }
        }
    }

    fn post(&mut self, peer: DeviceId, message: &Message, now: Instant) -> Result<(), PostError> {
        if !Self::valid(message) {
            return Err(PostError::TooLarge);
        }
        let bytes = v1::frame::encode(message, v1::limits::RELIABLE_FRAME).map_err(|_| PostError::TooLarge)?;
        let state = self.peers.get_mut(&peer).ok_or(PostError::Closed)?;
        let result = state.tx.as_ref().ok_or(PostError::Closed)?.post(bytes);
        let failure = match result {
            Err(PostError::Full) => Some(Failure::Full),
            Err(PostError::Closed) => Some(Failure::Peer(StreamCode::None)),
            _ => None,
        };
        if let Some(failure) = failure {
            let step = state.machine.failed(failure, now);
            self.step(peer, step, now);
        }
        result
    }

    pub fn send(&mut self, peer: DeviceId, message: &Message, now: Instant) -> Result<(), PostError> {
        if let Some(state) = self.peers.get_mut(&peer) {
            state.wanted = true;
        }
        self.advance(peer, now);
        self.post(peer, message, now)
    }

    /// Call on queue wake and at the earliest `deadline`. Only this module is serviced.
    pub fn poll(&mut self, now: Instant) -> Vec<(DeviceId, Message)> {
        let peers: Vec<_> = self.peers.keys().copied().collect();
        let mut delivered = Vec::new();
        for peer in peers {
            self.advance(peer, now);
            let mut invalid = None;
            let state = self.peers.get_mut(&peer).unwrap();
            if let Some(rx) = state.rx.as_mut() {
                while let Ok(Some(bytes)) = rx.take() {
                    let message =
                        match super::wire::decode(&self.links, state.link, StreamKind::Diagnostics, &bytes, |message| {
                            if Self::valid(&message) {
                                Ok(message)
                            } else {
                                Err(v1::ErrorCode::UnknownMessage.into())
                            }
                        }) {
                            Ok(message) => message,
                            Err(code) => {
                                invalid = Some((rx.id(), code));
                                break;
                            }
                        };
                    delivered.push((peer, message));
                }
            }
            state.operations.retain(|operation| !operation.is_finished());
            if let Some((stream, code)) = invalid {
                // Cleanup must be final: a preceding buffered message cannot restore
                // the receive state after this same stream was rejected.
                delivered.retain(|(source, _)| *source != peer);
                self.links.close_stream(stream, code);
                self.stream_closed(stream, StreamClosed::Local);
            }
        }
        for (peer, message) in &delivered {
            if let Message::DiagnosticsHello(hello) = message
                && let (Ok(instance), Ok(previous_unclean)) = (hello.instance(), hello.previous_unclean())
            {
                let old = self.instances.insert(*peer, instance.0.clone());
                if old.as_ref().is_some_and(|old| old != &instance.0)
                    && previous_unclean
                    && let Some(recorder) = self.recorder.clone().filter(|r| r.enabled())
                {
                    tokio::task::spawn_blocking(move || {
                        if recorder.save(crate::diagnostics::Reason::PeerRestart).is_err() {
                            tracing::warn!("peer-restart diagnostic save failed");
                        }
                    });
                }
                if let Some(recorder) = &self.recorder {
                    recorder.peer_lifecycle(*peer, "instance_observed", Some(&instance.0));
                }
            }
        }
        delivered
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.peers.values().filter_map(|p| p.machine.deadline()).min()
    }
    /// The business consumer clears only the state belonging to these exact handles.
    pub fn take_closed(&mut self) -> Vec<(DeviceId, StreamId)> {
        std::mem::take(&mut self.closed)
    }
    /// Each operation is served in its own task, including while locally paused.
    pub fn take_low(&mut self) -> Vec<InboundLow> {
        std::mem::take(&mut self.low)
    }
}

impl StreamModule for DiagnosticsStreams {
    const KINDS: &'static [StreamKind] = KINDS;
    fn link_ready(&mut self, link: LinkId, peer: DeviceId) {
        let supported = self.supported(peer);
        if let Some(recorder) = &self.recorder {
            recorder.configuration(&self.config);
            recorder.sample_links(&self.links.links());
            recorder.sample_scheduler(&self.links.scheduler());
            recorder.peer_lifecycle(peer, "link_ready", None);
            if let Some(link) = self.links.links().by_peer(peer) {
                recorder.peer_lifecycle(
                    peer,
                    "peer_boot",
                    Some(&recorder.anonymizer().router_instance(&link.peer_boot.nonce)),
                );
            }
        }
        self.peers.entry(peer).or_insert_with(|| Peer {
            link,
            machine: StreamMachine::new(StreamKind::Diagnostics),
            ordinal: InboundOrdinal::default(),
            tx: None,
            rx: None,
            wanted: false,
            supported,
            opening: Vec::new(),
            received: Vec::new(),
            operations: Vec::new(),
        });
        if let Some(hello) = self.hello.clone() {
            self.set_opening(peer, vec![hello]);
            self.advance(peer, Instant::now());
        }
    }
    fn link_lost(&mut self, link: LinkId, peer: DeviceId, _reason: LinkLost) {
        if self.peers.get(&peer).is_none_or(|p| p.link != link) {
            return;
        }
        if let Some(recorder) = &self.recorder {
            let reason = match _reason {
                LinkLost::Local(_) => 0.,
                LinkLost::HeartbeatTimeout => 1.,
                LinkLost::ConnectionLost => 2.,
                LinkLost::RelayLost => 3.,
                LinkLost::CutoverTimeout => 4.,
                LinkLost::PeerRestarted => 5.,
                LinkLost::Stopped => 6.,
            };
            recorder.record(crate::diagnostics::Event::Log {
                module: module_path!().into(),
                line: line!(),
                level: 2,
                fields: vec![("reason".into(), reason)],
                labels: vec![
                    ("peer".into(), recorder.anonymizer().device(peer)),
                    ("phase".into(), "link_lost".into()),
                ],
            });
        }
        let state = self.peers.remove(&peer).unwrap();
        for operation in state.operations {
            operation.abort();
        }
        if let Some(rx) = state.rx {
            self.closed.push((peer, rx.id()));
        }
        if let Some(tx) = state.tx {
            self.closed.push((peer, tx.id()));
        }
        self.low.retain(|op| op.stream.link() != link);
    }
    fn inbound(&mut self, stream: InboundStream) {
        let Some(state) = self.peers.get_mut(&stream.peer).filter(|p| p.link == stream.link) else {
            stream.reject(StreamCode::NotReady);
            return;
        };
        if stream.tag.kind != StreamKind::Diagnostics || stream.has_datagram {
            stream.reject(StreamCode::Unsupported);
            return;
        }
        if matches!(state.ordinal.admit(stream.tag.ordinal), Admit::Stale) {
            stream.reject(StreamCode::StaleOrdinal);
            return;
        }
        if let Some(old) = state.rx.take() {
            self.links.close_stream(old.id(), StreamCode::None);
            self.closed.push((stream.peer, old.id()));
        }
        state.received.clear();
        let (mut rx, _packets) = stream.accept(
            QueueSpec::reliable(StreamKind::Diagnostics).1,
            QueueSpec::datagram(StreamKind::Diagnostics),
        );
        let wake = self.wake.clone();
        rx.set_wake(std::sync::Arc::new(move || wake.notify_one()));
        // Acceptance and wake installation can race the first network read.
        self.wake.notify_one();
        state.rx = Some(rx);
    }
    fn inbound_low(&mut self, mut operation: InboundLow) {
        if operation.stream.kind() == StreamKind::Diagnostics
            && matches!(&operation.first, Message::DiagnosticsRequest(_))
            && self
                .peers
                .get(&operation.stream.peer())
                .is_some_and(|p| p.link == operation.stream.link())
        {
            self.low.push(operation);
        } else {
            operation.stream.cancel();
        }
    }
    fn stream_closed(&mut self, stream: StreamId, reason: StreamClosed) {
        let Some((peer, state)) = self.peers.iter_mut().find(|(_, p)| p.link == stream.link) else {
            return;
        };
        let peer = *peer;
        match stream.dir {
            StreamDir::In if state.rx.as_ref().is_some_and(|s| s.id() == stream) => {
                state.rx = None;
                state.received.clear();
                for operation in state.operations.drain(..) {
                    operation.abort();
                }
                self.closed.push((peer, stream));
            }
            StreamDir::Out if state.tx.as_ref().is_some_and(|s| s.id() == stream) => {
                state.tx = None;
                self.closed.push((peer, stream));
                let failure = match reason {
                    StreamClosed::Peer(code) => Failure::Peer(code),
                    StreamClosed::Local | StreamClosed::LinkLost => return,
                };
                let now = Instant::now();
                let step = state.machine.failed(failure, now);
                self.step(peer, step, now);
            }
            _ => {}
        }
    }
    fn pause_changed(&mut self, _paused: bool) {}
    fn facts_changed(&mut self, peer: DeviceId, capabilities_changed: bool) {
        if !capabilities_changed {
            return;
        }
        let supported = self.supported(peer);
        if let Some(state) = self.peers.get_mut(&peer) {
            state.supported = supported;
            state.machine.capabilities_changed();
        }
        self.advance(peer, Instant::now());
    }
}

impl DiagnosticsStreams {
    pub fn set_recorder(&mut self, recorder: crate::diagnostics::Recorder) {
        self.recorder = Some(recorder);
    }
    pub fn set_hello(&mut self, instance: String, enabled: bool, previous_unclean: Option<String>) {
        let hello = Message::DiagnosticsHello(
            dto::DiagnosticsHello::from_raw(
                [
                    (1, RawValue::Text(instance)),
                    (2, enabled.write()),
                    (3, false.write()),
                    (4, previous_unclean.is_some().write()),
                ]
                .into(),
            )
            .expect("bounded local diagnostic identity"),
        );
        if self.hello.as_ref() == Some(&hello) {
            return;
        }
        self.hello = Some(hello.clone());
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            let already_open = self.peers.get(&peer).is_some_and(|state| state.tx.is_some());
            self.set_opening(peer, vec![hello.clone()]);
            if already_open {
                let _ = self.send(peer, &hello, Instant::now());
            } else {
                self.advance(peer, Instant::now());
            }
        }
    }
    pub fn path_changed(&self, link: LinkId, path: oink_network::link::LinkPath) {
        if let Some(recorder) = &self.recorder
            && let Some((peer, _)) = self.peers.iter().find(|(_, p)| p.link == link)
        {
            let mut labels = vec![
                ("peer".into(), recorder.anonymizer().device(*peer)),
                ("phase".into(), "path_changed".into()),
            ];
            let relayed = match path {
                oink_network::link::LinkPath::Direct => false,
                oink_network::link::LinkPath::Relayed { via } => {
                    labels.push(("via".into(), recorder.anonymizer().device(via)));
                    true
                }
            };
            recorder.record(crate::diagnostics::Event::Log {
                module: module_path!().into(),
                line: line!(),
                level: 2,
                fields: vec![("relayed".into(), f64::from(u8::from(relayed)))],
                labels,
            });
        }
    }
    pub fn sample(&self) {
        if let Some(recorder) = &self.recorder {
            recorder.configuration(&self.config);
            recorder.sample_links(&self.links.links());
            recorder.sample_scheduler(&self.links.scheduler());
        }
    }
}

impl DiagnosticsStreams {
    pub fn inbound_id(&self, peer: DeviceId) -> Option<StreamId> {
        self.peers
            .get(&peer)
            .and_then(|peer| peer.rx.as_ref())
            .map(RecvStream::id)
    }
}

impl Drop for DiagnosticsStreams {
    fn drop(&mut self) {
        for peer in self.peers.values_mut() {
            for operation in peer.operations.drain(..) {
                operation.abort();
            }
            if let Some(stream) = peer.tx.take() {
                self.links.close_stream(stream.id(), StreamCode::None);
            }
            if let Some(stream) = peer.rx.take() {
                self.links.close_stream(stream.id(), StreamCode::None);
            }
        }
    }
}

#[cfg(all(test, debug_assertions))]
super::lifecycle_tests::owner_lifecycle_tests!(
    DiagnosticsStreams,
    Diagnostics,
    |owner: &mut DiagnosticsStreams, peer: DeviceId, message: Message| {
        owner.set_opening(peer, vec![message]);
    }
);
