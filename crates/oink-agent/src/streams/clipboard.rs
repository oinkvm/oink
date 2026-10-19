//! Inventory-only clipboard announcements and bounded content operations.
#[path = "../clip/stream_link.rs"]
pub mod files;

pub const KINDS: &[StreamKind] = &[StreamKind::Clipboard];

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
    v1::{self, Message, dto, schema::Open},
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Debug)]
struct EarlyTail {
    expires: Instant,
    operation: InboundLow,
    bytes: usize,
}

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
    tail: Option<Message>,
    received: Vec<Message>,
    operations: HashMap<u64, tokio::task::JoinHandle<()>>,
    // Payload already consumed; retain the Low handle until both ENDs arrive.
    finishing: Vec<tokio::task::JoinHandle<()>>,
    early_tail: Option<EarlyTail>,
}

/// Ordinary-thread owner of this module's streams. `poll` returns messages to its
/// business consumer; it never waits for another module or for a low-priority operation.
#[derive(Debug)]
pub struct ClipboardStreams {
    links: LinkLayer,
    wake: std::sync::Arc<tokio::sync::Notify>,
    peers: HashMap<DeviceId, Peer>,
    paused: bool,
    low: Vec<InboundLow>,
    closed: Vec<(DeviceId, StreamId)>,
    offer: Option<dto::ClipboardOffer>,
    next_attachment: u64,
    assemblies: crate::attachment::link::Assemblies,
    files: std::sync::Arc<files::FileLinks>,
}

impl ClipboardStreams {
    #[cfg(test)]
    pub(super) fn attachment_count(&self) -> u64 {
        self.next_attachment
    }

    pub fn new(links: LinkLayer, config: ConfigModule) -> Self {
        let paused = config.applied().paused;
        let files = files::FileLinks::new(links.clone(), StreamKind::Clipboard);
        files.set_log_config(config.clone());
        files.pause_changed(paused);
        Self {
            files,
            links,
            wake: Default::default(),
            peers: HashMap::new(),
            paused,
            low: Vec::new(),
            closed: Vec::new(),
            offer: None,
            next_attachment: 0,
            assemblies: Default::default(),
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
        match message {
            Message::ClipboardOffer(_) => true,
            Message::AttachmentHead(head) => {
                head.header().and_then(|h| h.variant()) == Ok(Open::Known(dto::AttachmentKind::Offer))
            }
            Message::AttachmentResult(result) => result.kind() == Ok(dto::AttachmentKind::Offer),
            _ => false,
        }
    }
    fn supported(&self, peer: DeviceId) -> bool {
        self.links
            .links()
            .by_peer(peer)
            .is_some_and(|entry| entry.agreement.contains("oink.clipboard/1"))
    }

    fn advance(&mut self, peer: DeviceId, now: Instant) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        let allowed = state.supported && !self.paused;
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
            tracing::debug!(?peer, stream=?tx.id(), "closing Clipboard stream");
            self.links.close_stream(tx.id(), StreamCode::None);
        }
        state.packets_tx = None;
        for (_, operation) in state.operations.drain() {
            operation.abort();
        }
        let Some(tag) = open else {
            return;
        };
        let opened = self.links.open_stream(
            state.link,
            StreamKind::Clipboard.priority(),
            tag,
            QueueSpec::reliable(StreamKind::Clipboard).0,
            None,
        );
        let Ok((tx, packets)) = opened else {
            state.machine.poll(false, state.wanted, now);
            return;
        };
        tracing::debug!(?peer, stream=?tx.id(), "opening Clipboard stream");
        state.tx = Some(tx);
        state.packets_tx = packets;
        self.refresh_offer(peer);
        let opening = self.peers[&peer].opening.clone();
        for message in opening {
            if self.post(peer, &message, now).is_err() {
                return;
            }
        }
        self.send_tail(peer);
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
        for state in self.peers.values_mut() {
            if state.early_tail.as_ref().is_some_and(|tail| now >= tail.expires) {
                state.early_tail.take(); // Dropping cancels the unadmitted Low operation.
            }
        }
        for (stream, reply) in self.assemblies.expire(now) {
            if let Some(peer) = self
                .peers
                .iter()
                .find(|(_, state)| state.link == stream.link)
                .map(|(peer, _)| *peer)
            {
                let _ = self.send(peer, &reply, now);
            }
        }
        let peers: Vec<_> = self.peers.keys().copied().collect();
        let mut delivered = Vec::new();
        for peer in peers {
            self.advance(peer, now);
            let mut invalid = None;
            let state = self.peers.get_mut(&peer).unwrap();
            if let Some(rx) = state.rx.as_mut() {
                while let Ok(Some(bytes)) = rx.take() {
                    let message = match decode_reliable(&self.links, peer, state.link, &bytes) {
                        Ok(message) if Self::valid(&message) => message,
                        _ => {
                            invalid = Some(rx.id());
                            break;
                        }
                    };
                    delivered.push((peer, message));
                }
            }
            state.operations.retain(|_, operation| !operation.is_finished());
            if let Some(stream) = invalid {
                self.links.close_stream(stream, StreamCode::Invalid);
                self.stream_closed(stream, StreamClosed::Local);
            }
        }
        let mut applied = Vec::new();
        for (peer, message) in delivered {
            let stream = self.peers.get(&peer).and_then(|p| p.rx.as_ref()).map(RecvStream::id);
            let Some(stream) = stream else {
                continue;
            };
            match message {
                Message::AttachmentResult(result) => {
                    if let Ok(id) = result.id()
                        && let Some(operation) = self.peers.get_mut(&peer).unwrap().operations.remove(&id)
                    {
                        operation.abort();
                    }
                }
                message @ Message::AttachmentHead(_) => match self.assemblies.receive(stream, message, now) {
                    Ok((offer, replies)) => {
                        for reply in replies {
                            let _ = self.send(peer, &reply, now);
                        }
                        if let Some(offer) = offer {
                            self.record_offer(peer, offer, &mut applied);
                        }
                    }
                    Err(_) => {
                        self.links.close_stream(stream, StreamCode::Invalid);
                        self.stream_closed(stream, StreamClosed::Local);
                    }
                },
                message => {
                    self.assemblies.clear(stream);
                    self.record_offer(peer, message, &mut applied);
                }
            }
        }
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            self.accept_early_tail(peer);
        }
        if !self.paused {
            for (peer, state) in &mut self.peers {
                for message in state.received.drain(..) {
                    applied.push((*peer, message));
                }
            }
        }
        applied
    }

    pub fn inbound_id(&self, peer: DeviceId) -> Option<StreamId> {
        self.peers.get(&peer)?.rx.as_ref().map(RecvStream::id)
    }
    pub fn link_layer(&self) -> LinkLayer {
        self.links.clone()
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.peers
            .values()
            .filter_map(|p| p.machine.deadline())
            .chain(
                self.peers
                    .values()
                    .filter_map(|p| p.early_tail.as_ref().map(|tail| tail.expires)),
            )
            .chain(self.assemblies.deadline())
            .min()
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

impl ClipboardStreams {
    fn accept_early_tail(&mut self, peer: DeviceId) {
        let Some(state) = self.peers.get_mut(&peer) else { return };
        let matches = state.early_tail.as_ref().is_some_and(|tail| {
            Instant::now() < tail.expires
                && state.rx.as_ref().is_some_and(|rx| {
                    matches!(&tail.operation.first, Message::AttachmentItems(items)
                    if self.assemblies.can_receive_tail(rx.id(), items))
                })
        });
        if matches {
            let tail = state.early_tail.take().unwrap();
            self.inbound_low(tail.operation);
        }
    }
    fn opening_state(&self, kind: StreamKind, peer: DeviceId) -> Vec<Message> {
        if kind != StreamKind::Clipboard {
            return Vec::new();
        }
        self.peers.get(&peer).map_or_else(Vec::new, |p| p.opening.clone())
    }
}

impl StreamModule for ClipboardStreams {
    const KINDS: &'static [StreamKind] = KINDS;
    fn link_ready(&mut self, link: LinkId, peer: DeviceId) {
        if let Some(entry) = self.links.links().by_peer(peer).filter(|entry| entry.link == link) {
            self.files.link_ready(peer, link, entry.peer_boot);
        }
        let supported = self.supported(peer);
        self.peers.entry(peer).or_insert_with(|| Peer {
            link,
            machine: StreamMachine::new(StreamKind::Clipboard),
            ordinal: InboundOrdinal::default(),
            tx: None,
            rx: None,
            packets_tx: None,
            packets_rx: None,
            wanted: false,
            supported,
            opening: Vec::new(),
            tail: None,
            received: Vec::new(),
            operations: HashMap::new(),
            finishing: Vec::new(),
            early_tail: None,
        });
        self.refresh_offer(peer);
        self.advance(peer, Instant::now());
    }
    fn link_lost(&mut self, link: LinkId, peer: DeviceId, reason: LinkLost) {
        if self.peers.get(&peer).is_none_or(|p| p.link != link) {
            return;
        }
        self.files.link_lost(peer, link, reason);
        let state = self.peers.remove(&peer).unwrap();
        for (_, operation) in state.operations {
            operation.abort();
        }
        for operation in state.finishing {
            operation.abort();
        }
        if let Some(rx) = state.rx {
            self.assemblies.clear(rx.id());
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
        if stream.tag.kind != StreamKind::Clipboard {
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
            self.assemblies.clear(old.id());
            // Outgoing attachment tails belong to our outbound stream, not this
            // independently replaceable inbound stream.
        }
        state.received.clear();
        let peer = stream.peer;
        let (mut rx, packets) = stream.accept(
            QueueSpec::reliable(StreamKind::Clipboard).1,
            QueueSpec::datagram(StreamKind::Clipboard),
        );
        self.files.inbound(peer, rx.id());
        let wake = self.wake.clone();
        rx.set_wake(std::sync::Arc::new(move || wake.notify_one()));
        // Acceptance and wake installation can race the first network read.
        self.wake.notify_one();
        state.rx = Some(rx);
        state.packets_rx = packets;
        self.accept_early_tail(peer);
    }
    fn inbound_low(&mut self, mut operation: InboundLow) {
        let peer = operation.stream.peer();
        if operation.stream.kind() != StreamKind::Clipboard
            || !self.peers.get(&peer).is_some_and(|p| p.link == operation.stream.link())
        {
            operation.stream.cancel();
            return;
        }
        if let Message::AttachmentItems(items) = &operation.first {
            if items.family() != Ok(dto::AttachmentKind::Offer) {
                operation.stream.cancel();
                return;
            }
            let Some(stream) = self
                .inbound_id(peer)
                .filter(|stream| self.assemblies.can_receive_tail(*stream, items))
            else {
                // TCP and QUIC are independently scheduled. A new tail can
                // precede a replacement stream while the old stream still lives.
                // Retain it under the link until its matching head identifies
                // the reliable ordinal; never bind it to whichever rx is current.
                let bytes =
                    v1::frame::encode(&operation.first, v1::limits::LOW_FRAME).map_or(usize::MAX, |frame| frame.len());
                let queued: usize = self
                    .peers
                    .values()
                    .filter_map(|state| state.early_tail.as_ref().map(|tail| tail.bytes))
                    .sum();
                let entries = self.peers.values().filter(|state| state.early_tail.is_some()).count();
                let state = self.peers.get_mut(&peer).unwrap();
                let old = state.early_tail.as_ref().map_or(0, |tail| tail.bytes);
                if entries >= 64 && state.early_tail.is_none()
                    || bytes > 32 * 1024 * 1024 - queued + old
                    || state.early_tail.as_ref().is_some_and(|tail| {
                        matches!(&tail.operation.first, Message::AttachmentItems(old) if old.id().ok() >= items.id().ok())
                    })
                {
                    operation.stream.cancel();
                    return;
                }
                state.early_tail = Some(EarlyTail {
                    expires: Instant::now() + Duration::from_secs(10),
                    operation,
                    bytes,
                });
                return;
            };
            match self.assemblies.receive(stream, operation.first.clone(), Instant::now()) {
                Ok((offer, replies)) => {
                    for reply in replies {
                        let _ = self.send(peer, &reply, Instant::now());
                    }
                    if let Some(offer) = offer {
                        let mut received = vec![];
                        self.record_offer(peer, offer, &mut received);
                        self.peers
                            .get_mut(&peer)
                            .unwrap()
                            .received
                            .extend(received.into_iter().map(|(_, m)| m));
                    }
                }
                Err(_) => {
                    operation.stream.cancel();
                    return;
                }
            }
            operation.stream.finish();
            let state = self.peers.get_mut(&peer).unwrap();
            state.finishing.retain(|task| !task.is_finished());
            state.finishing.push(tokio::spawn(async move {
                let _ = operation.stream.recv().await;
            }));
        } else if oink_clipboard::wire::local_content(operation.first.clone()).is_ok() {
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
                self.files.stream_closed(peer, stream);
                self.assemblies.clear(stream);
                state.rx = None;
                state.packets_rx = None;
                state.received.clear();
                // An unmatched Low tail belongs to the link, not this ordinal.
                // A replacement stream can still carry its matching head.
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
    fn pause_changed(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        self.files.pause_changed(paused);
        if paused {
            for state in self.peers.values_mut() {
                for (_, operation) in state.operations.drain() {
                    operation.abort();
                }
            }
        }
        let now = Instant::now();
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            self.advance(peer, now);
        }
    }
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

impl ClipboardStreams {
    pub fn file_links(&self) -> std::sync::Arc<files::FileLinks> {
        self.files.clone()
    }
    /// A new local copy replaces the state used by every subsequent open and retry.
    pub fn set_offer(&mut self, offer: Option<dto::ClipboardOffer>) {
        self.offer = offer;
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            let already_open = self.peers[&peer].tx.is_some();
            if already_open || self.offer.is_none() {
                self.refresh_offer(peer);
            } else {
                // Opening (including a deferred retry/resume) constructs the
                // current offer once, when the stream can actually send it.
                self.peers.get_mut(&peer).unwrap().wanted = true;
            }
            self.advance(peer, Instant::now());
            if already_open {
                let sent = self
                    .opening_state(StreamKind::Clipboard, peer)
                    .iter()
                    .all(|message| self.post(peer, message, Instant::now()).is_ok());
                if sent {
                    self.send_tail(peer);
                }
            }
        }
    }
    fn refresh_offer(&mut self, peer: DeviceId) {
        let Some(offer) = self.offer.clone() else {
            if let Some(state) = self.peers.get_mut(&peer) {
                for (_, operation) in state.operations.drain() {
                    operation.abort();
                }
                state.opening.clear();
                state.tail = None;
            }
            return;
        };
        self.next_attachment = self
            .next_attachment
            .checked_add(1)
            .expect("attachment identity exhausted");
        let Ok((message, tail)) = crate::attachment::split(self.next_attachment, Message::ClipboardOffer(offer)) else {
            return;
        };
        if let Some(state) = self.peers.get_mut(&peer) {
            for (_, operation) in state.operations.drain() {
                operation.abort();
            }
            state.opening = vec![message];
            state.tail = tail;
            state.wanted = true;
        }
    }
    fn send_tail(&mut self, peer: DeviceId) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        let Some(message) = state.tail.clone() else {
            return;
        };
        if self.paused || state.tx.is_none() {
            return;
        }
        let links = self.links.clone();
        let link = state.link;
        let bytes = v1::frame::encode(&message, v1::limits::LOW_FRAME).map_or(0, |b| b.len());
        let send_max = oink_foundation::transfer::LowOptions::message_bytes(bytes);
        let Message::AttachmentItems(items) = &message else {
            return;
        };
        state.operations.insert(
            items.id().expect("constructed attachment id"),
            tokio::spawn(async move {
                let options = oink_foundation::transfer::LowOptions {
                    send_max,
                    receive_max: 0,
                    operation_max: send_max,
                };
                if let Ok(mut stream) = links.open_low(link, StreamKind::Clipboard, options).await
                    && stream.send(&message).await.is_ok()
                {
                    stream.finish();
                    let _ = stream.recv().await;
                }
            }),
        );
    }
    fn record_offer(&mut self, peer: DeviceId, message: Message, applied: &mut Vec<(DeviceId, Message)>) {
        if let Message::ClipboardOffer(offer) = &message
            && let Ok(id) = offer.id().and_then(oink_clipboard::wire::local_offer_id)
        {
            self.files.accept(peer, id.clip);
        }
        if self.paused {
            self.peers.get_mut(&peer).unwrap().received = vec![message];
        } else {
            self.peers.get_mut(&peer).unwrap().received.clear();
            applied.push((peer, message));
        }
    }
}

impl Drop for ClipboardStreams {
    fn drop(&mut self) {
        for peer in self.peers.values_mut() {
            for (_, operation) in peer.operations.drain() {
                operation.abort();
            }
            for operation in peer.finishing.drain(..) {
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
    ClipboardStreams,
    Clipboard,
    |owner: &mut ClipboardStreams, _peer: DeviceId, message: Message| {
        if let Message::ClipboardOffer(offer) = message {
            owner.set_offer(Some(offer));
        }
    }
);

fn decode_reliable(links: &LinkLayer, peer: DeviceId, link: LinkId, bytes: &[u8]) -> Result<Message, ()> {
    use v1::{
        admission::{Context, Phase},
        registry::Transport,
    };
    let table = links.links();
    let entry = table.by_peer(peer).filter(|e| e.link == link).ok_or(())?;
    let context = Context {
        kind: 4,
        transport: Transport::Reliable,
        phase: Phase::Member,
        agreement: &entry.agreement,
        direct: matches!(entry.path, oink_network::link::LinkPath::Direct),
        old_member_lane: false,
    };
    let frame = v1::frame::OpaqueFrame::parse_exact(bytes, v1::limits::RELIABLE_FRAME).map_err(|e| {
        links.report_protocol_violation(link, context.payload_violation(0, e.code));
    })?;
    context
        .decode(frame)
        .map_err(|v| links.report_protocol_violation(link, v))
}
