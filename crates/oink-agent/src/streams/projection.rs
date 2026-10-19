//! P0 carries ephemeral previews on reliable streams for capable group members.

pub const KINDS: &[StreamKind] = &[StreamKind::Projection];

use super::{
    StreamModule,
    machine::{Admit, Failure, InboundOrdinal, Step, StreamMachine},
};
use crate::config_module::ConfigModule;
use oink_foundation::DeviceId;
use oink_network::link::{
    DatagramRecv, DatagramSend, InboundLow, InboundStream, LinkId, LinkLayer, LinkLost, PostError, QueueSpec,
    RecvStream, SendStream, StreamClosed, StreamDir, StreamId,
};
use oink_protocol::{
    frame::{StreamCode, StreamKind},
    v1::{self, Message},
};
use std::{collections::HashMap, time::Instant};

#[derive(Debug)]
struct Peer {
    link: LinkId,
    machine: StreamMachine,
    ordinal: InboundOrdinal,
    tx: Option<SendStream>,
    rx: Option<RecvStream>,
    packets_tx: Option<DatagramSend>,
    packets_rx: Option<DatagramRecv>,
    wanted: bool,
    supported: bool,
    opening: Vec<Message>,
    received: Vec<Message>,
    operations: Vec<tokio::task::JoinHandle<()>>,
}

/// Ordinary-thread owner of this module's streams. `poll` returns messages to its
/// business consumer; it never waits for another module or for a low-priority operation.
#[derive(Debug)]
pub struct ProjectionStreams {
    links: LinkLayer,
    config: ConfigModule,
    wake: std::sync::Arc<tokio::sync::Notify>,
    peers: HashMap<DeviceId, Peer>,
    low: Vec<InboundLow>,
    closed: Vec<(DeviceId, StreamId)>,
}

impl ProjectionStreams {
    pub fn new(links: LinkLayer, config: ConfigModule) -> Self {
        Self {
            links,
            config,
            wake: Default::default(),
            peers: HashMap::new(),
            low: Vec::new(),
            closed: Vec::new(),
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
        matches!(
            message,
            Message::ProjectionPreview(_) | Message::ProjectionMode(_) | Message::ProjectionGesture(_)
        )
    }

    fn supported(&self, peer: DeviceId) -> bool {
        self.config
            .peer_agreement(peer)
            .is_some_and(|agreement| agreement.contains("oink.layout.projection/1"))
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
            tracing::debug!(?peer, stream=?tx.id(), "closing Projection stream");
            self.links.close_stream(tx.id(), StreamCode::None);
        }
        state.packets_tx = None;
        let Some(tag) = open else {
            return;
        };
        let opened = self.links.open_stream(
            state.link,
            StreamKind::Projection.priority(),
            tag,
            QueueSpec::reliable(StreamKind::Projection).0,
            None,
        );
        let Ok((tx, packets)) = opened else {
            state.machine.poll(false, state.wanted, now);
            return;
        };
        tracing::debug!(?peer, stream=?tx.id(), "opening Projection stream");
        state.tx = Some(tx);
        state.packets_tx = packets;
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
    pub fn poll(&mut self, now: Instant) -> Vec<(DeviceId, crate::projection::model::Action)> {
        let peers: Vec<_> = self.peers.keys().copied().collect();
        let mut delivered = Vec::new();
        for peer in peers {
            self.advance(peer, now);
            let mut invalid = None;
            let state = self.peers.get_mut(&peer).unwrap();
            if let Some(rx) = state.rx.as_mut() {
                while let Ok(Some(bytes)) = rx.take() {
                    let message = match super::wire::decode(
                        &self.links,
                        state.link,
                        StreamKind::Projection,
                        &bytes,
                        crate::projection::wire::decode,
                    ) {
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

impl StreamModule for ProjectionStreams {
    const KINDS: &'static [StreamKind] = KINDS;
    fn link_ready(&mut self, link: LinkId, peer: DeviceId) {
        let supported = self.supported(peer);
        self.peers.entry(peer).or_insert_with(|| Peer {
            link,
            machine: StreamMachine::new(StreamKind::Projection),
            ordinal: InboundOrdinal::default(),
            tx: None,
            rx: None,
            packets_tx: None,
            packets_rx: None,
            wanted: false,
            supported,
            opening: Vec::new(),
            received: Vec::new(),
            operations: Vec::new(),
        });
    }
    fn link_lost(&mut self, link: LinkId, peer: DeviceId, _reason: LinkLost) {
        if self.peers.get(&peer).is_none_or(|p| p.link != link) {
            return;
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
        if !state.supported || stream.tag.kind != StreamKind::Projection {
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
        let (mut rx, packets) = stream.accept(
            QueueSpec::reliable(StreamKind::Projection).1,
            QueueSpec::datagram(StreamKind::Projection),
        );
        let wake = self.wake.clone();
        rx.set_wake(std::sync::Arc::new(move || wake.notify_one()));
        // Acceptance and wake installation can race the first network read.
        self.wake.notify_one();
        state.rx = Some(rx);
        state.packets_rx = packets;
    }
    fn inbound_low(&mut self, mut operation: InboundLow) {
        operation.stream.cancel();
    }
    fn stream_closed(&mut self, stream: StreamId, reason: StreamClosed) {
        let Some((peer, state)) = self.peers.iter_mut().find(|(_, p)| p.link == stream.link) else {
            return;
        };
        let peer = *peer;
        match stream.dir {
            StreamDir::In if state.rx.as_ref().is_some_and(|s| s.id() == stream) => {
                state.rx = None;
                state.packets_rx = None;
                state.received.clear();
                for operation in state.operations.drain(..) {
                    operation.abort();
                }
                self.closed.push((peer, stream));
            }
            StreamDir::Out if state.tx.as_ref().is_some_and(|s| s.id() == stream) => {
                state.tx = None;
                state.packets_tx = None;
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

impl Drop for ProjectionStreams {
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
