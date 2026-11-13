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
    v2::{self, Message, dto, schema::Open},
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
    history_operations: Vec<tokio::task::JoinHandle<()>>,
}

/// Ordinary-thread owner of this module's streams. `poll` returns messages to its
/// business consumer; it never waits for another module or for a low-priority operation.
#[derive(Debug)]
pub struct ClipboardStreams {
    links: LinkLayer,
    config: ConfigModule,
    wake: std::sync::Arc<tokio::sync::Notify>,
    peers: HashMap<DeviceId, Peer>,
    paused: bool,
    low: Vec<InboundLow>,
    closed: Vec<(DeviceId, StreamId)>,
    offer: Option<dto::ClipboardOffer>,
    history_summary: Option<dto::HistorySummary>,
    witnessed_offer: Option<dto::ClipboardOffer>,
    /// Order of the newest group offer witnessed (kept across local changes,
    /// unlike `witnessed_offer`). An own offer below it is no longer the
    /// group's current clip and is never put on a stream again.
    newest: Option<(u64, DeviceId)>,
    next_attachment: u64,
    assemblies: crate::attachment::link::Assemblies,
    files: std::sync::Arc<files::FileLinks>,
}

impl ClipboardStreams {
    pub fn new(links: LinkLayer, config: ConfigModule) -> Self {
        let paused = config.applied().paused;
        let files = files::FileLinks::new(links.clone(), StreamKind::Clipboard);
        files.set_log_config(config.clone());
        files.pause_changed(paused);
        Self {
            files,
            links,
            config,
            wake: Default::default(),
            peers: HashMap::new(),
            paused,
            low: Vec::new(),
            closed: Vec::new(),
            offer: None,
            history_summary: None,
            witnessed_offer: None,
            newest: None,
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
            Message::ClipboardOffer(_)
            | Message::ClipboardCurrentQuery(_)
            | Message::ClipboardCurrentResult(_)
            | Message::ClipboardHeadUpdate(_)
            | Message::ClipboardShareBegin(_)
            | Message::ClipboardShareAccept(_)
            | Message::ClipboardShareReject(_)
            | Message::ClipboardShareResult(_)
            | Message::ClipboardShareCancel(_)
            | Message::HistorySummary(_)
            | Message::HistoryIndexRequest(_)
            | Message::HistoryIndex(_)
            | Message::HistoryPull(_)
            | Message::HistoryRecords(_)
            | Message::HistoryDelta(_) => true,
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
            .is_some_and(|entry| entry.opens(StreamKind::Clipboard))
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
        // A paused B0 closes with `Paused` so the peer knows why.
        let code = if self.paused {
            StreamCode::Paused
        } else {
            StreamCode::None
        };
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        let open = match step {
            Step::Nothing => return,
            Step::Close(_) => None,
            Step::Open(tag) | Step::Reopen { open: tag, .. } => Some(tag),
        };
        if let Some(tx) = state.tx.take() {
            tracing::debug!(?peer, stream=?tx.id(), ?code, "closing Clipboard stream");
            // Through the handle: a `close_stream` command can lose its code
            // to the dropped handle's own close (the peer would see `None`).
            tx.close_with(code);
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
        let mut opening = self.peers[&peer].opening.clone();
        if self.history_capable(peer)
            && !self.history_phone(peer)
            && let Some(summary) = &self.history_summary
        {
            opening.push(Message::HistorySummary(summary.clone()));
        }
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
        let bytes = v2::frame::encode(message, v2::limits::RELIABLE_FRAME).map_err(|_| PostError::TooLarge)?;
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
        let sent_offer = match message {
            Message::ClipboardOffer(offer) => Some(offer),
            Message::AttachmentHead(_) => self.offer.as_ref(),
            _ => None,
        };
        if result.is_ok()
            && let Some(offer) = sent_offer
            && let Ok(id) = offer.id().and_then(oink_clipboard::wire::local_offer_id)
        {
            oink_clipboard::copy_diagnostics::event(
                Some(id),
                oink_clipboard::copy_diagnostics::Stage::OfferSent,
                oink_clipboard::copy_diagnostics::Values {
                    channel: oink_clipboard::copy_diagnostics::Channel::Reliable as u8,
                    result: 1,
                    ..Default::default()
                },
            );
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
    fn history_phone(&self, peer: DeviceId) -> bool {
        self.config
            .members()
            .members
            .iter()
            .any(|m| m.device == peer && m.is_mobile())
    }
    fn history_capable(&self, peer: DeviceId) -> bool {
        self.links.links().by_peer(peer).is_some_and(|e| e.permits(0x0356))
    }
    pub fn set_history_summary(&mut self, summary: dto::HistorySummary, exchange: bool) {
        self.history_summary = Some(summary.clone());
        if exchange && !self.paused {
            for peer in self.peers.keys().copied().collect::<Vec<_>>() {
                if self.history_capable(peer) && !self.history_phone(peer) {
                    let _ = self.send(peer, &Message::HistorySummary(summary.clone()), Instant::now());
                }
            }
        }
    }
    #[cfg(all(test, debug_assertions))]
    pub(crate) fn history_operations_for_test(&self, peer: DeviceId) -> usize {
        self.peers.get(&peer).map_or(0, |p| {
            p.history_operations.iter().filter(|task| !task.is_finished()).count()
        })
    }
    pub fn send_history(&mut self, peer: DeviceId, message: &Message) -> Result<(), PostError> {
        if self.paused || !self.history_capable(peer) {
            return Err(PostError::Closed);
        }
        // A Reliable holder must not overtake the record sent on Low. The
        // outbox retries after orderly completion, without an extra timeout.
        if self
            .peers
            .get(&peer)
            .is_some_and(|p| p.history_operations.iter().any(|task| !task.is_finished()))
        {
            return Err(PostError::Full);
        }
        if v2::frame::encode(message, v2::limits::RELIABLE_FRAME).is_ok() {
            return self.send(peer, message, Instant::now());
        }
        let bytes = v2::frame::encode(message, v2::limits::LOW_FRAME)
            .map_err(|_| PostError::TooLarge)?
            .len();
        let state = self.peers.get_mut(&peer).ok_or(PostError::Closed)?;
        let link = state.link;
        let links = self.links.clone();
        let message = message.clone();
        let send_max = oink_foundation::transfer::LowOptions::message_bytes(bytes);
        let wake = self.wake.clone();
        state.history_operations.push(tokio::spawn(async move {
            let options = oink_foundation::transfer::LowOptions {
                send_max,
                receive_max: 0,
                operation_max: send_max,
            };
            if let Ok(mut stream) = links.open_low(link, StreamKind::Clipboard, options).await
                && stream.send(&message).await.is_ok()
            {
                stream.finish();
                // FIN is half-close. Retain the Low handle through the peer's
                // EOF: dropping after our FIN cancels its deferred effect.
                while let Ok(Some(_)) = stream.recv().await {}
            }
            wake.notify_one();
        }));
        Ok(())
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
            state.history_operations.retain(|operation| !operation.is_finished());
            if let Some(stream) = invalid {
                // Through the handle, which `stream_closed` then drops: a
                // `close_stream` command can lose its code to that drop.
                if let Some(rx) = state.rx.as_ref().filter(|rx| rx.id() == stream) {
                    rx.close_with(StreamCode::Invalid);
                }
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
                        // Through the handle, which `stream_closed` then drops.
                        if let Some(rx) = self
                            .peers
                            .get(&peer)
                            .and_then(|state| state.rx.as_ref())
                            .filter(|rx| rx.id() == stream)
                        {
                            rx.close_with(StreamCode::Invalid);
                        }
                        self.stream_closed(stream, StreamClosed::Local);
                    }
                },
                message @ Message::ClipboardOffer(_) => {
                    self.assemblies.clear(stream);
                    self.record_offer(peer, message, &mut applied);
                }
                message => applied.push((peer, message)),
            }
        }
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            self.accept_early_tail(peer);
        }
        if !self.paused {
            for (peer, state) in &mut self.peers {
                for message in state.received.drain(..) {
                    // The file binding made when this offer arrived during the
                    // pause was cancelled when sharing resumed; the offer is
                    // current again now that it is delivered.
                    if let Message::ClipboardOffer(offer) = &message
                        && let Ok(id) = offer.id().and_then(oink_clipboard::wire::local_offer_id)
                    {
                        self.files.accept(*peer, id.clip);
                    }
                    applied.push((*peer, message));
                }
            }
        }
        applied
    }

    pub fn inbound_id(&self, peer: DeviceId) -> Option<StreamId> {
        self.peers.get(&peer)?.rx.as_ref().map(RecvStream::id)
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
            wanted: self.offer.is_some() || self.history_summary.is_some(),
            supported,
            opening: Vec::new(),
            tail: None,
            received: Vec::new(),
            operations: HashMap::new(),
            finishing: Vec::new(),
            early_tail: None,
            history_operations: vec![],
        });
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
        for operation in state.history_operations {
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
        // While clipboard sharing is paused nothing can be answered on B0: a
        // phone opening it to ask learns why at once (it never waits for a
        // reply this device cannot send).
        if self.paused {
            stream.reject(StreamCode::Paused);
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
        // The peer is sharing again: a B0 it refused as Paused reopens now.
        let resumed = state.machine.peer_resumed();
        self.accept_early_tail(peer);
        if resumed {
            self.advance(peer, Instant::now());
        }
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
                    v2::frame::encode(&operation.first, v2::limits::LOW_FRAME).map_or(usize::MAX, |frame| frame.len());
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
        } else if matches!(
            operation.first,
            Message::HistoryDelta(_)
                | Message::HistoryRecords(_)
                | Message::HistoryIndex(_)
                | Message::ClipboardContentRead(_)
                | Message::ContentThumbnailRead(_)
                | Message::ClipboardShareTextChunk(_)
                | Message::ClipboardShareTextEnd(_)
                | Message::ContentOffer(_)
        ) || oink_clipboard::wire::local_content(operation.first.clone()).is_ok()
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
            for (peer, state) in &mut self.peers {
                for task in state.history_operations.drain(..) {
                    task.abort();
                }
                for (_, operation) in state.operations.drain() {
                    operation.abort();
                }
                // B0 closes both ways: the peer's requests could not be answered.
                if let Some(rx) = state.rx.take() {
                    rx.close_with(StreamCode::Paused);
                    self.assemblies.clear(rx.id());
                    self.closed.push((*peer, rx.id()));
                }
            }
        }
        let now = Instant::now();
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            self.advance(peer, now);
        }
    }
    fn facts_changed(&mut self, peer: DeviceId, capabilities_changed: bool) {
        // New facts may be the peer's resume (its pause state is a fact). A
        // late `Paused` refusal of a B0 opened during the peer's pause would
        // otherwise park it until some later Facts (`StreamMachine::peer_facts`).
        let peer_paused = super::peer_facts_paused(&self.config, peer);
        let now = Instant::now();
        let (redial, resumed) = self.peers.get_mut(&peer).map_or((Step::Nothing, false), |state| {
            let redial = peer_paused.map_or(Step::Nothing, |paused| state.machine.peer_facts(paused, now));
            (redial, state.machine.peer_resumed())
        });
        self.step(peer, redial, now);
        if !capabilities_changed {
            if resumed {
                self.advance(peer, Instant::now());
            }
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
    pub fn network(&self) -> LinkLayer {
        self.links.clone()
    }
    pub fn file_links(&self) -> std::sync::Arc<files::FileLinks> {
        self.files.clone()
    }
    /// A new local copy replaces the state used by every subsequent open and retry.
    ///
    /// An own offer that a newer group offer already replaced (republished
    /// after a pause on a device that does not receive) is not announced: it
    /// is not the group's current clip. The witnessed newer offer stays.
    pub fn set_offer(&mut self, mut offer: Option<dto::ClipboardOffer>) {
        if let Some(stale) = offer.as_ref().filter(|offer| self.superseded(offer)) {
            tracing::debug!(
                offer = ?stale.id().ok().and_then(|id| oink_clipboard::wire::local_offer_id(id).ok()),
                "own clipboard offer superseded by a newer group offer: not announced"
            );
            offer = None;
        } else {
            self.witnessed_offer = offer.clone();
        }
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
    /// A group offer this device has seen: accepted, or (receiving off) only
    /// observed. True when it is newer than the witnessed one.
    ///
    /// Our own announced offer that it orders after is withdrawn from every
    /// stream: it is no longer the group's current clip, so a peer that
    /// connects later (a phone reads it automatically as this PC's current
    /// clipboard) must not get it. Writing the newer clip here replaced the
    /// own offer's native source, so it is not even readable any more
    /// (2026-11-12: a Windows test machine kept pushing its replaced offer 1302 to phones).
    pub(crate) fn witness_offer(&mut self, offer: &dto::ClipboardOffer) -> bool {
        let order = offer_order(offer);
        if order > self.newest {
            self.newest = order;
        }
        if self.offer.as_ref().is_some_and(|own| self.superseded(own)) {
            tracing::debug!(
                offer = ?self.offer.as_ref().and_then(|own| own.id().ok()).and_then(|id| oink_clipboard::wire::local_offer_id(id).ok()),
                "own clipboard offer superseded by a newer group offer: withdrawn"
            );
            self.withdraw_offer();
        }
        if order > self.witnessed_offer.as_ref().and_then(offer_order) {
            self.witnessed_offer = Some(offer.clone());
            true
        } else {
            false
        }
    }

    pub(crate) fn witnessed_offer(&self) -> Option<&dto::ClipboardOffer> {
        self.witnessed_offer.as_ref()
    }

    /// The own offer put on new and reopened streams.
    #[cfg(all(test, debug_assertions))]
    pub(crate) fn announced_offer(&self) -> Option<&dto::ClipboardOffer> {
        self.offer.as_ref()
    }

    fn superseded(&self, offer: &dto::ClipboardOffer) -> bool {
        offer_order(offer) < self.newest
    }

    pub(crate) fn forget_offer(&mut self, id: oink_clipboard::model::OfferId) {
        let matches =
            |offer: &dto::ClipboardOffer| offer.id().and_then(oink_clipboard::wire::local_offer_id).ok() == Some(id);
        if self.offer.as_ref().is_some_and(matches) {
            self.withdraw_offer();
        }
        if self.witnessed_offer.as_ref().is_some_and(matches) {
            self.witnessed_offer = None;
        }
        for peer in self.peers.values_mut() {
            peer.received
                .retain(|message| !matches!(message,Message::ClipboardOffer(offer) if matches(offer)));
        }
    }

    /// Stops announcing the own offer without touching the witnessed one.
    fn withdraw_offer(&mut self) {
        self.offer = None;
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            self.refresh_offer(peer);
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
        let bytes = v2::frame::encode(&message, v2::limits::LOW_FRAME).map_or(0, |b| b.len());
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
            oink_clipboard::copy_diagnostics::event(
                Some(id),
                oink_clipboard::copy_diagnostics::Stage::OfferReceived,
                oink_clipboard::copy_diagnostics::Values::default(),
            );
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
            for task in peer.history_operations.drain(..) {
                task.abort();
            }
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

/// The group's copy order of an offer (clipboard.md §6): Lamport clock, then
/// origin device.
fn offer_order(offer: &dto::ClipboardOffer) -> Option<(u64, DeviceId)> {
    let id = oink_clipboard::wire::local_offer_id(offer.id().ok()?).ok()?;
    Some((offer.clock().ok()?, id.clip.origin))
}

fn decode_reliable(links: &LinkLayer, peer: DeviceId, link: LinkId, bytes: &[u8]) -> Result<Message, ()> {
    use v2::{
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
    let frame = v2::frame::OpaqueFrame::parse_exact(bytes, v2::limits::RELIABLE_FRAME).map_err(|e| {
        links.report_protocol_violation(link, context.payload_violation(0, e.code));
    })?;
    context
        .decode(frame)
        .map_err(|v| links.report_protocol_violation(link, v))
}
