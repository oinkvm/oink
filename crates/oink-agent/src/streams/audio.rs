//! Audio control owns admitted v1 sessions; native workers alone consume M1.
use oink_audio::{negotiation::Profile as StreamFormat, session::Prepared};
pub(crate) mod transport;
use oink_audio::media::Kind as MediaKind;
use transport::{MediaReceiver, MediaSender};

pub const KINDS: &[StreamKind] = &[StreamKind::Audio];

/// One exit this device sends to.
#[derive(Debug)]
pub struct ForwardTo {
    pub peer: DeviceId,
    pub stream_id: u64,
    pub format: StreamFormat,
    pub gain: f32,
    pub packets: std::sync::Arc<dyn MediaSender>,
}

/// One source this device plays.
#[derive(Debug)]
pub struct ForwardFrom {
    pub admission: std::sync::Arc<ReceiveAdmission>,
    pub peer: DeviceId,
    pub stream_id: u64,
    pub format: StreamFormat,
    pub gain: f32,
    /// False while this device is paused: packets are taken and dropped.
    pub playing: bool,
    pub packets: std::sync::Arc<dyn MediaReceiver>,
}

/// Control publishes identity once; callback only borrows this atomic gate.
#[derive(Debug)]
pub struct ReceiveAdmission {
    active: std::sync::atomic::AtomicBool,
    playing: std::sync::atomic::AtomicBool,
}
impl Default for ReceiveAdmission {
    fn default() -> Self {
        Self {
            active: true.into(),
            playing: true.into(),
        }
    }
}
fn publish_receive_table(target: &oink_foundation::snapshot::Snapshot<ForwardTable>, mut next: ForwardTable) {
    use std::sync::{Arc, atomic::Ordering};
    let old = target.load();
    for source in &mut next.from {
        if let Some(previous) = old.from.iter().find(|p| {
            p.peer == source.peer
                && p.stream_id == source.stream_id
                && p.format == source.format
                && p.packets.follows() == source.packets.follows()
        }) {
            source.admission = previous.admission.clone();
        }
        source.admission.playing.store(source.playing, Ordering::Release);
    }
    for previous in &old.from {
        if !next.from.iter().any(|p| Arc::ptr_eq(&p.admission, &previous.admission)) {
            previous.admission.active.store(false, Ordering::Release);
        }
    }
    target.publish(next); // old owning snapshots are reclaimed only on control
}

/// Snapshot read by the audio realtime threads (`Snapshot<ForwardTable>`), produced by
/// the audio control task. Replaced whole whenever a route, format, gain or M1 handle
/// changes.
#[derive(Debug, Default)]
pub struct ForwardTable {
    pub to: Vec<ForwardTo>,
    pub from: Vec<ForwardFrom>,
}

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
    v1::{self, Message, admission, frame::OpaqueFrame},
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
    microphone_rx: Option<std::sync::Arc<dyn MediaReceiver>>,
    speaker_rx: Option<std::sync::Arc<dyn MediaReceiver>>,
    wanted: bool,
    supported: bool,
    opening: Vec<Message>,
}

/// Ordinary-thread owner of this module's streams. `poll` returns messages to its
/// business consumer; it never waits for another module or for a low-priority operation.
#[derive(Debug)]
pub struct AudioStreams {
    links: LinkLayer,
    config: ConfigModule,
    wake: std::sync::Arc<tokio::sync::Notify>,
    media_wake: std::sync::Arc<std::sync::atomic::AtomicBool>,
    peers: HashMap<DeviceId, Peer>,
    paused: bool,
    low: Vec<InboundLow>,
    closed: Vec<(DeviceId, StreamId)>,
    source: Option<(DeviceId, Prepared)>,
    pending_opening: HashMap<DeviceId, Vec<Message>>,
    receivers: HashMap<DeviceId, Prepared>,
    microphone_opening: HashMap<DeviceId, Vec<Message>>,
    microphone_sources: HashMap<DeviceId, (u64, StreamFormat)>,
    microphone_receiver: Option<(DeviceId, u64, StreamFormat)>,
    microphone_forward: std::sync::Arc<oink_foundation::snapshot::Snapshot<ForwardTable>>,
    media_reports: HashMap<(MediaKind, DeviceId), (u64, u64, bool, Instant)>,
    forwarding_enabled: bool,
    forwarding_gain: f32,
    forward: std::sync::Arc<oink_foundation::snapshot::Snapshot<ForwardTable>>,
}

impl AudioStreams {
    pub fn new(links: LinkLayer, config: ConfigModule) -> Self {
        let applied = config.applied();
        Self {
            links,
            config,
            wake: Default::default(),
            media_wake: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            peers: HashMap::new(),
            paused: applied.paused,
            low: Vec::new(),
            closed: Vec::new(),
            source: None,
            pending_opening: HashMap::new(),
            receivers: HashMap::new(),
            microphone_opening: HashMap::new(),
            microphone_sources: HashMap::new(),
            microphone_receiver: None,
            microphone_forward: Default::default(),
            media_reports: HashMap::new(),
            forwarding_enabled: false,
            forwarding_gain: 1.0,
            forward: Default::default(),
        }
    }

    /// Clone before the task loop, then await `notified()` alongside module/config
    /// events and the retry deadline. Poll this owner after any of those wake-ups.
    pub fn set_media_wake(&self, enabled: bool) {
        if enabled && !self.media_wake.swap(enabled, std::sync::atomic::Ordering::AcqRel) {
            // Prime once: an arrival can race the ownership change while notifications
            // are disabled. The dormant owner must inspect an already queued onset.
            self.wake.notify_one();
        } else if !enabled {
            self.media_wake.store(false, std::sync::atomic::Ordering::Release);
        }
    }
    pub fn wake(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.wake.clone()
    }

    /// Replace current state before the next open; no historical message is retained.
    pub fn set_opening(&mut self, peer: DeviceId, messages: Vec<Message>) {
        assert!(messages.iter().all(Self::valid), "message belongs to another stream");
        self.pending_opening.insert(peer, messages.clone());
        self.refresh_opening(peer);
    }
    pub fn set_microphone_opening(&mut self, peer: DeviceId, messages: Vec<Message>) {
        assert!(messages.iter().all(Self::valid), "message belongs to another stream");
        if messages.is_empty() {
            self.microphone_opening.remove(&peer);
        } else {
            self.microphone_opening.insert(peer, messages);
        }
        self.refresh_opening(peer);
    }
    fn refresh_opening(&mut self, peer: DeviceId) {
        if let Some(state) = self.peers.get_mut(&peer) {
            state.opening = self
                .pending_opening
                .get(&peer)
                .into_iter()
                .flatten()
                .chain(self.microphone_opening.get(&peer).into_iter().flatten())
                .cloned()
                .collect();
            state.wanted |= !state.opening.is_empty();
        }
    }

    /// Telemetry can open M0 between any audio-capable members, without M1
    /// admission. The negotiated capability replaces a separate support message.
    pub fn enable_state_reports(&mut self, peer: DeviceId) {
        if self.config.peer_supports(peer, "oink.audio.state/1") {
            if let Some(state) = self.peers.get_mut(&peer) {
                state.wanted = true;
            }
            self.advance(peer, Instant::now());
        }
    }
    fn valid(message: &Message) -> bool {
        matches!(
            message,
            Message::AudioSourceRouteOffer(_)
                | Message::AudioPrepared(_)
                | Message::AudioPreparedAck(_)
                | Message::AudioPrepareRefused(_)
                | Message::AudioRouteFault(_)
                | Message::AudioState(_)
                | Message::AudioMediaState(_)
                | Message::MicrophoneDemand(_)
                | Message::MicrophonePrepared(_)
                | Message::MicrophoneReady(_)
                | Message::MicrophoneRenew(_)
                | Message::MicrophoneStop(_)
        )
    }
    fn supported(&self, peer: DeviceId) -> bool {
        self.config.applied().members.contains(&peer) && self.config.peer_supports(peer, "oink.audio/1")
    }
    fn context(agreement: &admission::Agreement, direct: bool) -> admission::Context<'_> {
        admission::Context {
            kind: 5,
            transport: v1::registry::Transport::Reliable,
            phase: admission::Phase::Member,
            agreement,
            direct,
            old_member_lane: false,
        }
    }
    fn advance(&mut self, peer: DeviceId, now: Instant) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        // M0 carries lifecycle and volume state even while media is paused.
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
            tracing::debug!(?peer, stream=?tx.id(), "closing Audio stream");
            self.links.close_stream(tx.id(), StreamCode::None);
        }
        state.packets_tx = None;
        let Some(tag) = open else {
            self.publish_forward();
            return;
        };
        let opened = self.links.open_stream(
            state.link,
            StreamKind::Audio.priority(),
            tag,
            QueueSpec::reliable(StreamKind::Audio).0,
            Some(QueueSpec::datagram(StreamKind::Audio)),
        );
        let Ok((tx, packets)) = opened else {
            state.machine.poll(false, state.wanted, now);
            self.publish_forward();
            return;
        };
        tracing::debug!(?peer, stream=?tx.id(), "opening Audio stream");
        state.tx = Some(tx);
        state.packets_tx = packets;
        let opening = state.opening.clone();
        self.publish_forward();
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
        let agreement = self.config.peer_agreement(peer).ok_or(PostError::Closed)?;
        let bytes = v1::frame::encode(message, v1::limits::RELIABLE_FRAME).map_err(|_| PostError::TooLarge)?;
        let frame = OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME).map_err(|_| PostError::TooLarge)?;
        let direct = self
            .links
            .links()
            .by_peer(peer)
            .is_some_and(|link| matches!(link.path, oink_network::link::LinkPath::Direct));
        Self::context(&agreement, direct)
            .admit(frame.header)
            .map_err(|_| PostError::Closed)?;
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
            let mut violation = None;
            // A maintenance poll usually has no M0 message. Borrow admission
            // from the live snapshot instead of cloning every capability string
            // on each 20 ms audio tick, including empty receive queues.
            let links = self.links.links();
            let link = links.by_peer(peer);
            let direct = link.is_some_and(|link| matches!(link.path, oink_network::link::LinkPath::Direct));
            let agreement = link
                .filter(|link| link.state == oink_network::link::LinkState::Ready)
                .map(|link| &link.agreement);
            let state = self.peers.get_mut(&peer).unwrap();
            if let Some(rx) = state.rx.as_mut() {
                while let Ok(Some(bytes)) = rx.take() {
                    let message = match agreement.and_then(|agreement| {
                        let context = Self::context(agreement, direct);
                        let frame = match OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME) {
                            Ok(frame) => frame,
                            Err(error) => {
                                violation = Some(context.payload_violation(0, error.code));
                                return None;
                            }
                        };
                        match context.decode(frame) {
                            Ok(message) if Self::valid(&message) => Some(message),
                            Ok(_) => {
                                violation = Some(
                                    context.payload_violation(frame.header.type_id, v1::ErrorCode::InvalidChannel),
                                );
                                None
                            }
                            Err(error) => {
                                violation = Some(error);
                                None
                            }
                        }
                    }) {
                        Some(message) if Self::valid(&message) => message,
                        _ => {
                            invalid = Some(rx.id());
                            break;
                        }
                    };
                    delivered.push((peer, message));
                }
            }
            if let Some(stream) = invalid {
                // Cleanup must be final: a preceding buffered message cannot restore
                // the receive state after this same stream was rejected.
                delivered.retain(|(source, _)| *source != peer);
                if let Some(violation) = violation {
                    self.links.report_protocol_violation(stream.link, violation);
                }
                self.links.close_stream(stream, StreamCode::Invalid);
                self.stream_closed(stream, StreamClosed::Local);
            }
        }
        // Route changes in advance/stream_closed publish at their mutation site.
        // A maintenance poll must not churn ArcSwap tables read by RT workers.
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

impl StreamModule for AudioStreams {
    const KINDS: &'static [StreamKind] = KINDS;
    fn link_ready(&mut self, link: LinkId, peer: DeviceId) {
        let supported = self.supported(peer);
        let state = self.peers.entry(peer).or_insert_with(|| Peer {
            link,
            machine: StreamMachine::new(StreamKind::Audio),
            ordinal: InboundOrdinal::default(),
            tx: None,
            rx: None,
            packets_tx: None,
            packets_rx: None,
            microphone_rx: None,
            speaker_rx: None,
            wanted: false,
            supported,
            opening: Vec::new(),
        });
        state.supported = supported;
        self.refresh_opening(peer);
        self.advance(peer, Instant::now());
    }
    fn link_lost(&mut self, link: LinkId, peer: DeviceId, _reason: LinkLost) {
        if self.peers.get(&peer).is_none_or(|p| p.link != link) {
            return;
        }
        self.forget_preparation(peer);
        self.forget_microphone(peer);
        let state = self.peers.remove(&peer).unwrap();
        if let Some(rx) = state.rx {
            self.closed.push((peer, rx.id()));
        }
        if let Some(tx) = state.tx {
            self.closed.push((peer, tx.id()));
        }
        self.low.retain(|op| op.stream.link() != link);
        self.publish_forward();
    }
    fn inbound(&mut self, stream: InboundStream) {
        let Some(state) = self.peers.get_mut(&stream.peer).filter(|p| p.link == stream.link) else {
            stream.reject(StreamCode::NotReady);
            return;
        };
        if stream.tag.kind != StreamKind::Audio {
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
        self.receivers.remove(&stream.peer);
        let (mut rx, lanes) = stream.accept_partitioned(
            QueueSpec::reliable(StreamKind::Audio).1,
            QueueSpec::datagram(StreamKind::Audio),
            [
                [MediaKind::Speaker.packet_id(), MediaKind::Speaker.sleep_id()],
                [MediaKind::Microphone.packet_id(), MediaKind::Microphone.sleep_id()],
            ],
        );
        let (packets, microphone) =
            lanes.map_or((None, None), |[speaker, microphone]| (Some(speaker), Some(microphone)));
        let wake = self.wake.clone();
        rx.set_wake(std::sync::Arc::new(move || wake.notify_one()));
        // Acceptance and wake installation can race the first network read.
        self.wake.notify_one();
        state.rx = Some(rx);
        if let Some(packets) = &packets {
            let wake = self.wake.clone();
            let enabled = self.media_wake.clone();
            packets.set_wake(std::sync::Arc::new(move || {
                if enabled.load(std::sync::atomic::Ordering::Acquire) {
                    wake.notify_one();
                }
            }));
        }
        state.speaker_rx = packets
            .as_ref()
            .map(|packets| std::sync::Arc::new(packets.clone()) as std::sync::Arc<dyn MediaReceiver>);
        state.microphone_rx =
            microphone.map(|packets| std::sync::Arc::new(packets) as std::sync::Arc<dyn MediaReceiver>);
        state.packets_rx = packets;
        self.publish_forward();
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
                self.receivers.remove(&peer);
                state.rx = None;
                state.packets_rx = None;
                state.speaker_rx = None;
                state.microphone_rx = None;
                self.closed.push((peer, stream));
            }
            StreamDir::Out if state.tx.as_ref().is_some_and(|s| s.id() == stream) => {
                state.tx = None;
                state.packets_tx = None;
                self.closed.push((peer, stream));
                let failure = match reason {
                    StreamClosed::Peer(code) => Failure::Peer(code),
                    StreamClosed::Local | StreamClosed::LinkLost => {
                        self.publish_forward();
                        return;
                    }
                };
                let now = Instant::now();
                let step = state.machine.failed(failure, now);
                self.step(peer, step, now);
            }
            _ => {}
        }
        self.publish_forward();
    }
    fn pause_changed(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        let now = Instant::now();
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            self.advance(peer, now);
        }
        self.publish_forward();
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

impl ForwardTable {
    /// Installed by the control task once; execution is on the source RT thread.
    pub fn source_writer(
        table: std::sync::Arc<oink_foundation::snapshot::Snapshot<Self>>,
        started: Instant,
    ) -> std::sync::Arc<dyn Fn(&oink_audio::source::Packet) -> bool + Send + Sync> {
        Self::source_writer_kind(table, started, MediaKind::Speaker)
    }
    pub fn source_writer_kind(
        table: std::sync::Arc<oink_foundation::snapshot::Snapshot<Self>>,
        started: Instant,
        kind: MediaKind,
    ) -> std::sync::Arc<dyn Fn(&oink_audio::source::Packet) -> bool + Send + Sync> {
        std::sync::Arc::new(move |packet| {
            let table = table.load();
            let mut sent = false;
            for destination in &table.to {
                let mut bytes = [0; oink_audio::media::FRAME_CAPACITY];
                if let Ok(len) =
                    oink_audio::media::encode_kind(kind, packet, destination.stream_id, started, &mut bytes)
                {
                    sent |= destination.packets.post(&bytes[..len]).is_ok();
                }
            }
            sent
        })
    }

    /// Installed on a DSP source; execution is on the exit RT thread. Neither this
    /// closure nor `source_writer` enters the audio control task.
    pub fn sink_reader(
        table: std::sync::Arc<oink_foundation::snapshot::Snapshot<Self>>,
        peer: DeviceId,
    ) -> std::sync::Arc<dyn Fn() -> (bool, Option<oink_audio::sink::Packet>) + Send + Sync> {
        Self::sink_reader_with_activity(table, peer, None)
    }

    pub fn sink_reader_with_activity(
        table: std::sync::Arc<oink_foundation::snapshot::Snapshot<Self>>,
        peer: DeviceId,
        activity: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    ) -> std::sync::Arc<dyn Fn() -> (bool, Option<oink_audio::sink::Packet>) + Send + Sync> {
        let reader = std::sync::Arc::new(MediaReader::new(table, peer, activity));
        std::sync::Arc::new(move || reader.read())
    }
}

impl AudioStreams {
    /// Send intent on reliable M0 as well as media on M1. This is bounded to
    /// state changes plus one heartbeat per second, with current-stream fencing.
    pub fn report_media(&mut self, kind: MediaKind, encoded: &oink_audio::source::Encoded, now: Instant) {
        use v1::schema::{WireDto, WireValue};
        let table = match kind {
            MediaKind::Speaker => self.forward.load(),
            MediaKind::Microphone => self.microphone_forward.load(),
        };
        let destinations: Vec<_> = table.to.iter().map(|d| (d.peer, d.stream_id)).collect();
        drop(table);
        self.media_reports
            .retain(|(k, peer), _| *k != kind || destinations.iter().any(|(p, _)| p == peer));
        let burst = encoded.burst();
        let sleeping = encoded.sleeping();
        for (peer, stream) in destinations {
            if !self.config.peer_supports(peer, "oink.audio.media-health/1") {
                continue;
            }
            let key = (kind, peer);
            if self
                .media_reports
                .get(&key)
                .is_some_and(|(old_stream, old_burst, old_sleep, at)| {
                    (*old_stream, *old_burst, *old_sleep) == (stream, burst, sleeping)
                        && now.saturating_duration_since(*at) < std::time::Duration::from_secs(1)
                })
            {
                continue;
            }
            let state = v1::dto::AudioMediaState::from_raw(
                [
                    (1, kind.to_wire().write()),
                    (2, stream.write()),
                    (3, burst.write()),
                    (4, sleeping.write()),
                ]
                .into(),
            )
            .expect("bounded media state");
            if self.send(peer, &Message::AudioMediaState(state), now).is_ok() {
                self.media_reports.insert(key, (stream, burst, sleeping, now));
            }
        }
    }
    pub fn forward_table(&self) -> std::sync::Arc<oink_foundation::snapshot::Snapshot<ForwardTable>> {
        self.forward.clone()
    }
    pub fn retire_sessions(&mut self) {
        self.source = None;
        self.receivers.clear();
        self.pending_opening.clear();
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            self.refresh_opening(peer);
        }
        self.publish_forward();
    }
    pub fn set_forwarding(&mut self, enabled: bool, gain: f32) {
        if self.forwarding_enabled == enabled && self.forwarding_gain == gain {
            return;
        }
        self.forwarding_enabled = enabled;
        self.forwarding_gain = gain;
        self.publish_forward();
    }
    pub fn forget_preparation(&mut self, peer: DeviceId) {
        self.pending_opening.remove(&peer);
        if self.source.as_ref().is_some_and(|(source, _)| *source == peer) {
            self.source = None;
        }
        self.receivers.remove(&peer);
        self.refresh_opening(peer);
        self.publish_forward();
    }
    /// Called only after the current source sent its successful PreparedAck.
    pub fn admit_source(&mut self, peer: DeviceId, prepared: Prepared) {
        self.source = Some((peer, prepared));
        self.publish_forward();
    }
    /// Called only after this exit validated the current source's PreparedAck.
    pub fn admit_receiver(&mut self, peer: DeviceId, prepared: Prepared) {
        self.receivers.insert(peer, prepared);
        self.publish_forward();
    }
    pub fn microphone_table(&self) -> std::sync::Arc<oink_foundation::snapshot::Snapshot<ForwardTable>> {
        self.microphone_forward.clone()
    }
    pub fn admit_microphone_source(&mut self, peer: DeviceId, stream: u64, format: StreamFormat) {
        self.microphone_sources.insert(peer, (stream, format));
        self.publish_forward();
    }
    pub fn admit_microphone_receiver(&mut self, peer: DeviceId, stream: u64, format: StreamFormat) {
        self.microphone_receiver = Some((peer, stream, format));
        self.publish_forward();
    }
    pub fn forget_microphone(&mut self, peer: DeviceId) {
        self.microphone_sources.remove(&peer);
        if self.microphone_receiver.as_ref().is_some_and(|(p, _, _)| *p == peer) {
            self.microphone_receiver = None;
        }
        self.microphone_opening.remove(&peer);
        self.refresh_opening(peer);
        self.publish_forward();
    }
    fn publish_forward(&self) {
        let mut microphone = ForwardTable::default();
        for (peer, state) in &self.peers {
            let receive = self.microphone_receiver.filter(|(p, _, _)| p == peer);
            if let Some((_, stream_id, format)) = receive
                && let Some(packets) = &state.microphone_rx
            {
                microphone.from.push(ForwardFrom {
                    admission: Default::default(),
                    peer: *peer,
                    stream_id,
                    format,
                    gain: 1.0,
                    playing: !self.paused,
                    packets: packets.clone(),
                });
            }
            if !self.paused
                && let Some((stream_id, format)) = self.microphone_sources.get(peer)
                && let Some(packets) = &state.packets_tx
            {
                microphone.to.push(ForwardTo {
                    peer: *peer,
                    stream_id: *stream_id,
                    format: *format,
                    gain: 1.0,
                    packets: std::sync::Arc::new(packets.clone()),
                });
            }
        }
        publish_receive_table(&self.microphone_forward, microphone);
        let mut table = ForwardTable::default();
        for (peer, prepared) in &self.receivers {
            if let Some(packets) = self.peers.get(peer).and_then(|p| p.speaker_rx.as_ref()) {
                table.from.push(ForwardFrom {
                    admission: Default::default(),
                    peer: *peer,
                    stream_id: prepared.stream_id(),
                    format: prepared.profile,
                    gain: 1.0,
                    playing: !self.paused,
                    packets: packets.clone(),
                });
            }
        }
        if !self.paused
            && self.forwarding_enabled
            && let Some((peer, prepared)) = &self.source
            && let Some(packets) = self.peers.get(peer).and_then(|p| p.packets_tx.as_ref())
        {
            table.to.push(ForwardTo {
                peer: *peer,
                stream_id: prepared.stream_id(),
                format: prepared.profile,
                gain: self.forwarding_gain,
                packets: std::sync::Arc::new(packets.clone()),
            });
        }
        publish_receive_table(&self.forward, table);
    }
}

impl Drop for AudioStreams {
    fn drop(&mut self) {
        publish_receive_table(&self.forward, ForwardTable::default());
        publish_receive_table(&self.microphone_forward, ForwardTable::default());
        for peer in self.peers.values_mut() {
            if let Some(stream) = peer.tx.take() {
                self.links.close_stream(stream.id(), StreamCode::None);
            }
            if let Some(stream) = peer.rx.take() {
                self.links.close_stream(stream.id(), StreamCode::None);
            }
        }
    }
}

/// Ownership changes only after the realtime reader has left its short read section.
/// The dormant owner keeps these same admission facts and consumes from the same M1.
pub struct MediaReader {
    pub quality_id: u64,
    pub kind: MediaKind,
    pub dropouts: std::sync::Arc<oink_audio::sink::DropoutCounters>,
    payload_bytes: std::sync::atomic::AtomicU64,
    transport_failed: std::sync::atomic::AtomicBool,
    // Ordinary control only; the realtime reader never touches this mutex.
    expected_media: std::sync::Mutex<Option<(u64, bool, Instant)>>,
    admission: std::sync::Arc<ReceiveAdmission>,
    packets: Option<std::sync::Arc<dyn MediaReceiver>>,
    activity_pending: std::sync::atomic::AtomicBool,
    peer: DeviceId,
    identity: Option<(StreamId, u64, StreamFormat)>,
    activity: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    pub bursts: oink_audio::idle::Bursts,
    // 0 = ordinary owner, 1 = RT may read, 2 = RT read in progress.
    owner: std::sync::atomic::AtomicU8,
}
impl MediaReader {
    pub fn new(
        table: std::sync::Arc<oink_foundation::snapshot::Snapshot<ForwardTable>>,
        peer: DeviceId,
        activity: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self::new_kind(table, peer, activity, MediaKind::Speaker)
    }
    pub fn new_kind(
        table: std::sync::Arc<oink_foundation::snapshot::Snapshot<ForwardTable>>,
        peer: DeviceId,
        activity: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
        kind: MediaKind,
    ) -> Self {
        static NEXT_QUALITY_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let snapshot = table.load();
        let source = snapshot.from.iter().find(|source| source.peer == peer);
        let identity = source.map(|source| (source.packets.follows(), source.stream_id, source.format));
        let admission = source.map_or_else(
            || {
                std::sync::Arc::new(ReceiveAdmission {
                    active: false.into(),
                    playing: false.into(),
                })
            },
            |source| {
                source
                    .admission
                    .playing
                    .store(source.playing, std::sync::atomic::Ordering::Release);
                source.admission.clone()
            },
        );
        let packets = source.map(|source| source.packets.clone());
        Self {
            quality_id: NEXT_QUALITY_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            kind,
            dropouts: Default::default(),
            payload_bytes: Default::default(),
            transport_failed: false.into(),
            expected_media: Default::default(),
            admission,
            packets,
            activity_pending: false.into(),
            peer,
            identity,
            activity,
            bursts: Default::default(),
            owner: std::sync::atomic::AtomicU8::new(0),
        }
    }
    pub fn observe_media(&self, state: &v1::dto::AudioMediaState, now: Instant) {
        if state.kind() != Ok(self.kind.to_wire()) || state.stream_id().ok() != self.identity.map(|i| i.1) {
            return;
        }
        let (Ok(burst), Ok(sleeping)) = (state.burst(), state.sleeping()) else {
            return;
        };
        if burst == 0 || burst < self.bursts.generation() {
            return;
        }
        let mut expected = self.expected_media.lock().unwrap_or_else(|p| p.into_inner());
        if expected.is_some_and(|(old, _, _)| burst < old) {
            return;
        }
        if expected.is_none_or(|(old, asleep, _)| (old, asleep) != (burst, sleeping)) {
            *expected = Some((burst, sleeping, now));
        }
    }
    pub fn health_error(&self, now: Instant) -> Option<&'static str> {
        use std::{sync::atomic::Ordering, time::Duration};
        if !self.admission.active.load(Ordering::Acquire) || !self.admission.playing.load(Ordering::Acquire) {
            return None;
        }
        if self.transport_failed.load(Ordering::Acquire) {
            return Some("audio media channel failed");
        }
        // Five seconds of genuine quiet precede the encoder's Sleep. Allow that
        // interval plus transport/startup slack, but do not reinterpret loss as
        // the old thirty-second idle fallback. A reliable new burst wakes this
        // check even when all wake media was blackholed after a previous Sleep.
        let timeout = Duration::from_secs(8);
        if let Some((burst, sleeping, since)) = *self.expected_media.lock().unwrap_or_else(|p| p.into_inner()) {
            if sleeping && burst >= self.bursts.generation() {
                return None;
            }
            if burst > self.bursts.generation() {
                return (now.saturating_duration_since(since) > timeout).then_some("audio media wake timed out");
            }
        }
        self.bursts
            .media_missing(now, timeout)
            .then_some("audio media delivery stalled")
    }
    pub fn payload_bytes(&self) -> u64 {
        self.payload_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn take_control(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.owner
            .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            || self.owner.load(Ordering::Acquire) == 0
    }
    pub fn give_realtime(&self) {
        use std::sync::atomic::Ordering;
        let _ = self.owner.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    }
    pub fn realtime(&self, budget: &mut usize) -> (bool, Option<oink_audio::sink::Packet>) {
        use std::sync::atomic::Ordering;
        if self
            .owner
            .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // Admission is frozen for shutdown. The existing receiver must still
            // render its queued onset/tail before the device can drain to zero.
            return (true, None);
        }
        let result = self.read_bounded(budget);
        self.owner.store(1, Ordering::Release);
        result
    }
    pub fn poll_activity(&self) {
        if self.activity_pending.swap(false, std::sync::atomic::Ordering::AcqRel)
            && let Some(activity) = &self.activity
        {
            activity();
        }
    }
    pub fn read(&self) -> (bool, Option<oink_audio::sink::Packet>) {
        let result = self.read_bounded(&mut 8);
        self.poll_activity();
        result
    }
    fn read_bounded(&self, budget: &mut usize) -> (bool, Option<oink_audio::sink::Packet>) {
        use std::sync::atomic::Ordering;
        if !self.admission.active.load(Ordering::Acquire) {
            return (false, None);
        }
        let (Some(packets), Some((_, stream_id, format))) = (&self.packets, self.identity) else {
            return (false, None);
        };
        let playing = self.admission.playing.load(Ordering::Acquire);
        while *budget != 0 {
            *budget -= 1;
            let mut bytes = [0; oink_audio::media::FRAME_CAPACITY];
            let (len, arrived) = match packets.take_timed_into(&mut bytes) {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(_) => {
                    self.transport_failed.store(true, Ordering::Release);
                    break;
                }
            };
            if !playing {
                continue;
            }
            match oink_audio::media::decode_kind(self.kind, &bytes[..len], stream_id, format) {
                Ok(oink_audio::media::Media::Sleep { burst, sequence }) => {
                    self.bursts.sleep(burst, sequence);
                }
                Ok(oink_audio::media::Media::Packet {
                    burst,
                    sequence,
                    source_time,
                    payload,
                    silent,
                }) => {
                    self.payload_bytes
                        .fetch_add(payload.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    if !self.bursts.media(burst, sequence, arrived) {
                        continue;
                    }
                    if !silent {
                        self.activity_pending.store(true, Ordering::Release);
                    }
                    let mut packet = oink_audio::sink::Packet {
                        burst,
                        sequence,
                        payload: [0; oink_audio::dsp::MAX_PACKET_BYTES],
                        len: payload.len(),
                        arrived,
                        network_read: Some(arrived),
                        source_emit: Some(source_time),
                    };
                    packet.payload[..payload.len()].copy_from_slice(payload);
                    return (true, Some(packet));
                }
                _ => {}
            }
        }
        (playing, None)
    }
}

impl std::fmt::Debug for MediaReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaReader")
            .field("peer", &self.peer)
            .field("bursts", &self.bursts)
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, debug_assertions))]
mod direct_media_tests {
    use super::transport::testing::datagram_pair;
    use super::*;
    use oink_foundation::snapshot::Snapshot;
    use oink_protocol::frame::StreamTag;
    use std::sync::Arc;
    const PEER: DeviceId = DeviceId([2; 32]);
    fn packet(sequence: u64) -> oink_audio::source::Packet {
        let now = Instant::now();
        let mut payload = [0; oink_audio::dsp::MAX_PACKET_BYTES];
        payload[..4].copy_from_slice(&[1, 2, 3, 4]);
        oink_audio::source::Packet {
            ordinal: sequence,
            burst: 1,
            sleep: false,
            payload,
            len: 480,
            silent: false,
            delivered_at: now,
            encode_start: now,
            encode_end: now,
            remaining_samples: 0,
        }
    }
    fn table_to(packets: Arc<dyn MediaSender>, generation: u64) -> ForwardTable {
        ForwardTable {
            to: vec![ForwardTo {
                peer: PEER,
                stream_id: generation,
                format: StreamFormat::Pcm16_2p5ms,
                gain: 1.0,
                packets,
            }],
            from: vec![],
        }
    }
    fn table_from(packets: Arc<dyn MediaReceiver>, playing: bool) -> ForwardTable {
        ForwardTable {
            to: vec![],
            from: vec![ForwardFrom {
                admission: Default::default(),
                peer: PEER,
                stream_id: 5,
                format: StreamFormat::Pcm16_2p5ms,
                gain: 1.0,
                playing,
                packets,
            }],
        }
    }
    #[test]
    fn media_health_detects_blackholes_after_sleep_and_fences_old_streams() {
        use std::time::Duration;
        use v1::schema::{WireDto, WireValue};
        let (send, recv) = datagram_pair(
            LinkId(1),
            StreamTag {
                kind: StreamKind::Audio,
                ordinal: 1,
            },
            QueueSpec {
                messages: 16,
                bytes: 16384,
            },
        );
        let reader = MediaReader::new(Arc::new(Snapshot::new(table_from(recv, true))), PEER, None);
        let now = Instant::now();
        assert!(reader.health_error(now + Duration::from_secs(9)).is_some());
        let mut sleep = packet(1);
        sleep.sleep = true;
        let mut bytes = [0; oink_audio::media::FRAME_CAPACITY];
        let len = oink_audio::media::encode(&sleep, 5, now, &mut bytes).unwrap();
        send.post(&bytes[..len]).unwrap();
        assert!(reader.read().1.is_none());
        assert_eq!(reader.health_error(now + Duration::from_secs(60)), None);
        let state = |stream: u64, burst: u64, sleeping: bool| {
            v1::dto::AudioMediaState::from_raw(
                [
                    (1, v1::dto::AudioMediaKind::Speaker.write()),
                    (2, stream.write()),
                    (3, burst.write()),
                    (4, sleeping.write()),
                ]
                .into(),
            )
            .unwrap()
        };
        reader.observe_media(&state(4, 2, false), now); // retired stream cannot wake or fault this one
        assert_eq!(reader.health_error(now + Duration::from_secs(60)), None);
        reader.observe_media(&state(5, 2, false), now);
        assert_eq!(reader.health_error(now + Duration::from_secs(1)), None);
        reader.observe_media(&state(5, 1, true), now + Duration::from_secs(2)); // late sleep cannot hide a new burst
        assert!(reader.health_error(now + Duration::from_secs(9)).is_some());
        let mut sound = packet(1);
        sound.burst = 2;
        let len = oink_audio::media::encode(&sound, 5, now, &mut bytes).unwrap();
        send.post(&bytes[..len]).unwrap();
        assert!(reader.read().1.is_some());
        assert_eq!(reader.health_error(Instant::now()), None);
    }

    #[test]
    fn media_receive_error_is_not_an_empty_queue() {
        #[derive(Debug)]
        struct Closed(StreamId);
        impl MediaReceiver for Closed {
            fn follows(&self) -> StreamId {
                self.0
            }
            fn dropped(&self) -> u64 {
                0
            }
            fn take_timed_into(
                &self,
                _: &mut [u8],
            ) -> Result<Option<(usize, Instant)>, oink_network::link::DatagramError> {
                Err(oink_network::link::DatagramError::Closed)
            }
        }
        let closed = Arc::new(Closed(StreamId {
            link: LinkId(1),
            tag: StreamTag {
                kind: StreamKind::Audio,
                ordinal: 1,
            },
            dir: StreamDir::In,
        }));
        let table = Arc::new(Snapshot::new(table_from(closed, true)));
        let reader = MediaReader::new(table.clone(), PEER, None);
        reader.give_realtime();
        crate::allocation_audit::assert_no_heap(|| {
            assert!(reader.realtime(&mut 8).1.is_none());
        });
        assert_eq!(reader.health_error(Instant::now()), Some("audio media channel failed"));
        publish_receive_table(&table, ForwardTable::default());
        assert_eq!(reader.health_error(Instant::now()), None); // retired queues do not fault their replacement
    }

    #[test]
    fn real_m1_reader_has_one_scan_budget_and_no_heap_or_activity_notification() {
        let (send, [recv, _mic]) = oink_network::link::testing::partitioned_datagram_pair(
            LinkId(1),
            StreamTag {
                kind: StreamKind::Audio,
                ordinal: 1,
            },
            QueueSpec {
                messages: 32,
                bytes: 32768,
            },
            [[0x0604, 0x0606], [0x0618, 0x0619]],
        );
        let table = Arc::new(Snapshot::new(table_from(Arc::new(recv), true)));
        let activity = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = activity.clone();
        let reader = MediaReader::new(
            table.clone(),
            PEER,
            Some(Arc::new(move || {
                observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })),
        );
        reader.give_realtime();
        let mut bytes = [0; oink_audio::media::FRAME_CAPACITY];
        for seq in 0..9 {
            let mut media = packet(seq);
            media.sleep = seq < 8;
            let len = oink_audio::media::encode(&media, 5, media.encode_end, &mut bytes).unwrap();
            send.post(&bytes[..len]).unwrap();
        }
        crate::allocation_audit::assert_no_heap(|| {
            let mut budget = 8;
            assert!(reader.realtime(&mut budget).1.is_none());
            assert_eq!(budget, 0);
            assert!(reader.realtime(&mut 8).1.is_some());
        });
        assert_eq!(activity.load(std::sync::atomic::Ordering::Relaxed), 0);
        reader.poll_activity();
        assert_eq!(activity.load(std::sync::atomic::Ordering::Relaxed), 1);
        publish_receive_table(&table, ForwardTable::default());
        crate::allocation_audit::assert_no_heap(|| {
            assert!(matches!(reader.realtime(&mut 8), (false, None)));
        });
    }

    #[test]
    fn realtime_admission_reclaims_snapshots_on_control_and_pins_reader_until_retirement() {
        #[derive(Debug)]
        struct Observed {
            inner: Arc<dyn MediaReceiver>,
            destroyed: Arc<std::sync::atomic::AtomicBool>,
            owner: std::thread::ThreadId,
        }
        impl MediaReceiver for Observed {
            fn follows(&self) -> StreamId {
                self.inner.follows()
            }
            fn dropped(&self) -> u64 {
                self.inner.dropped()
            }
            fn take_timed_into(
                &self,
                out: &mut [u8],
            ) -> Result<Option<(usize, Instant)>, oink_network::link::DatagramError> {
                self.inner.take_timed_into(out)
            }
        }
        impl Drop for Observed {
            fn drop(&mut self) {
                assert_eq!(self.owner, std::thread::current().id());
                self.destroyed.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        let (_send, recv) = datagram_pair(
            LinkId(1),
            StreamTag {
                kind: StreamKind::Audio,
                ordinal: 1,
            },
            QueueSpec {
                messages: 8,
                bytes: 8192,
            },
        );
        let destroyed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let packets: Arc<dyn MediaReceiver> = Arc::new(Observed {
            inner: recv,
            destroyed: destroyed.clone(),
            owner: std::thread::current().id(),
        });
        let table = Arc::new(Snapshot::new(table_from(packets, true)));
        let old = Arc::downgrade(&table.load());
        let reader = Arc::new(MediaReader::new(table.clone(), PEER, None));
        reader.give_realtime();
        publish_receive_table(&table, ForwardTable::default());
        assert!(
            old.upgrade().is_none(),
            "callback must not retain owning ForwardTable snapshots"
        );
        let rt = reader.clone();
        std::thread::spawn(move || {
            assert!(matches!(rt.realtime(&mut 8), (false, None)));
        })
        .join()
        .unwrap();
        assert!(!destroyed.load(std::sync::atomic::Ordering::Acquire));
        drop(reader); // control retirement owns the final destructor
        assert!(destroyed.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn media_uses_m1_without_an_ordinary_control_task_or_packet_handoff() {
        let (send, recv) = datagram_pair(
            LinkId(1),
            StreamTag {
                kind: StreamKind::Audio,
                ordinal: 1,
            },
            QueueSpec {
                messages: 16,
                bytes: 16384,
            },
        );
        let source = Arc::new(Snapshot::new(table_to(send, 5)));
        let sink = Arc::new(Snapshot::new(table_from(recv, true)));
        let write = ForwardTable::source_writer(source, Instant::now());
        let read = ForwardTable::sink_reader(sink, PEER);
        let (mut encoder, mut output) =
            oink_audio::source::channel(oink_audio::dsp::Config::low_latency_pcm(), 100, false).unwrap();
        output.set_packet_writer(write);
        for sequence in 1..=100 {
            let now = Instant::now();
            encoder.supply(
                &[0.1; 240],
                oink_audio::source::Stamp {
                    first_sample_at: None,
                    callback_at: Some(now),
                    delivered_at: now,
                    source_frame_position: None,
                    discontinuity: false,
                },
            );
            assert!(output.try_recv().is_none(), "ordinary packet handoff stays empty");
            let (playing, received) = read();
            assert!(playing);
            let received = received.unwrap();
            assert_eq!(received.sequence, sequence);
            assert_eq!(received.len, 480);
            assert!(received.payload[..received.len].iter().any(|byte| *byte != 0));
        }
    }
    #[test]
    fn replacement_snapshot_keeps_flow_pause_discards_and_route_clear_stops_source() {
        let (send, recv) = datagram_pair(
            LinkId(1),
            StreamTag {
                kind: StreamKind::Audio,
                ordinal: 1,
            },
            QueueSpec {
                messages: 16,
                bytes: 16384,
            },
        );
        let source = Arc::new(Snapshot::new(table_to(send.clone(), 5)));
        let sink = Arc::new(Snapshot::new(table_from(recv.clone(), true)));
        let write = ForwardTable::source_writer(source.clone(), Instant::now());
        let activity = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = activity.clone();
        let read = ForwardTable::sink_reader_with_activity(
            sink.clone(),
            PEER,
            Some(Arc::new(move || {
                observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })),
        );
        source.publish(table_to(send.clone(), 5));
        publish_receive_table(&sink, table_from(recv.clone(), false));
        assert!(write(&packet(1)));
        assert!(matches!(read(), (false, None)));
        publish_receive_table(&sink, table_from(recv, true));
        assert!(read().1.is_none(), "paused packet was consumed rather than delayed");
        assert!(write(&packet(2)));
        assert_eq!(read().1.unwrap().sequence, 2);
        assert_eq!(activity.load(std::sync::atomic::Ordering::Relaxed), 1);
        let mut silence = packet(3);
        silence.silent = true;
        assert!(write(&silence));
        assert!(read().1.is_some());
        assert_eq!(activity.load(std::sync::atomic::Ordering::Relaxed), 1);
        source.publish(table_to(send, 4));
        assert!(write(&packet(3)));
        assert!(read().1.is_none(), "old generation discarded");
        assert_eq!(activity.load(std::sync::atomic::Ordering::Relaxed), 1);
        source.publish(ForwardTable::default());
        assert!(!write(&packet(4)));
        assert!(read().1.is_none());
    }
    #[test]
    fn dormant_datagrams_wake_with_real_arrival_and_reject_late_sleep_and_bad_media() {
        let (send, recv) = datagram_pair(
            LinkId(1),
            StreamTag {
                kind: StreamKind::Audio,
                ordinal: 1,
            },
            QueueSpec::datagram(StreamKind::Audio),
        );
        let source = Arc::new(Snapshot::new(table_to(send.clone(), 5)));
        let sink = Arc::new(Snapshot::new(table_from(recv, true)));
        let write = ForwardTable::source_writer(source, Instant::now());
        let reader = MediaReader::new(sink, PEER, None);
        let mut old_sleep = packet(20);
        old_sleep.sleep = true;
        assert!(write(&old_sleep));
        let (playing, received) = reader.read();
        assert!(playing && received.is_none());
        assert!(reader.bursts.asleep(Instant::now()));
        let mut media = packet(2); // first media packet of burst 2 was lost
        media.burst = 2;
        let before = Instant::now();
        assert!(write(&media));
        let after = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let (playing, received) = reader.read();
        assert!(playing);
        let received = received.unwrap();
        assert_eq!((received.burst, received.sequence), (2, 2));
        assert!(received.arrived >= before && received.arrived <= after);
        assert_eq!(received.network_read, Some(received.arrived));
        assert!(write(&old_sleep));
        let (playing, packet) = reader.read();
        assert!(playing);
        assert!(packet.is_none());
        media.sleep = true;
        assert!(write(&media));
        assert!(reader.read().1.is_none());
        assert!(reader.bursts.asleep(Instant::now()));
        media.sleep = false;
        media.burst = 3;
        media.len = 1; // malformed PCM cannot wake or change burst admission
        assert!(write(&media));
        assert!(reader.read().1.is_none());
        assert!(reader.bursts.asleep(Instant::now()));
        assert_eq!(reader.bursts.generation(), 2);
        media.len = 480;
        assert!(write(&media));
        assert!(reader.read().1.is_some());
        assert_eq!(reader.bursts.generation(), 3);
        assert_eq!(
            reader.payload_bytes(),
            960,
            "payload only; no M1 headers, sleep messages or malformed media"
        );
    }
    #[test]
    fn reader_handover_cannot_steal_an_inflight_realtime_read() {
        use std::sync::atomic::Ordering;
        let reader = MediaReader::new(Arc::default(), PEER, None);
        reader.give_realtime();
        assert_eq!(reader.owner.load(Ordering::Acquire), 1);
        reader.owner.store(2, Ordering::Release); // RT claimed its short read section
        assert!(!reader.take_control());
        reader.give_realtime(); // a cancelled multi-source stop must leave it claimed
        assert_eq!(reader.owner.load(Ordering::Acquire), 2);
        reader.owner.store(1, Ordering::Release);
        assert!(reader.take_control());
        assert_eq!(reader.owner.load(Ordering::Acquire), 0);
        let (playing, packet) = reader.realtime(&mut 8);
        assert!(playing && packet.is_none());
    }
}

#[cfg(all(test, debug_assertions))]
super::lifecycle_tests::owner_lifecycle_tests!(
    AudioStreams,
    Audio,
    |owner: &mut AudioStreams, peer: DeviceId, message: Message| {
        owner.set_opening(peer, vec![message]);
    }
);

#[cfg(test)]
mod pull_tests;
