//! Rust-owned v2 network owner for mobile.
//!
//! Native shells provide discovery addresses and user pairing decisions. They
//! do not create sockets, encode frames, or decide membership. The owner keeps
//! durable GroupState, runs C0 greeting and signed admission, and exposes a
//! ready link to the transfer stream only after admission succeeds.

mod blocks;
mod device_control;
mod emergency;
mod facts;
mod input_opening;
mod member_host;
mod membership;
mod outbound;
pub use blocks::{BlockError, BlockedDevice, MAX_BLOCKS};
pub use device_control::DeviceControlCompletion;
pub use input_opening::{DefaultInputOpening, InputOpening};

use oink_foundation::{
    DeviceId, GroupId,
    realtime::Tier,
    session::{Boot, PairId},
};
use oink_network::{
    LowOptions, admission,
    discovery::Advert,
    group_state::State,
    link::{
        ConfigEvent, DatagramRecv, DatagramSend, Direction, LinkConfig, LinkId, LinkLayer, LinkPath, LinkState,
        ModuleEvent, QueueSpec, SendStream, StreamId,
    },
    negotiation::OfferProfile,
    pairing,
};
use oink_protocol::{
    frame::{StreamKind, StreamTag},
    v2::{
        self, RawValue, dto,
        frame::OpaqueFrame,
        schema::{WireDto, WireValue},
    },
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock, Weak, atomic::AtomicBool},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct MobileNetworkConfig {
    pub identity_seed: [u8; 32],
    pub boot: Boot,
    pub bind: SocketAddr,
    /// A persisted v2 GroupState. Production callers must provide this or
    /// group_state_path; this owner never invents a group identity.
    pub group_state: Option<Vec<u8>>,
    pub group_state_path: PathBuf,
    pub pairing_journal_path: PathBuf,
    pub device_name: String,
    pub os: String,
    /// iPhone vs iPad (or phone vs tablet) as reported by the host; published
    /// in DeviceFacts key 31 so PCs can draw the matching badge.
    pub form_factor: Option<dto::FormFactor>,
    /// Kept for source compatibility. These values never grant membership.
    pub trusted_peers: Vec<DeviceId>,
    pub trust_path: Option<PathBuf>,
    /// The process kind fixes the negotiated capabilities for its lifetime.
    pub process: MobileProcess,
}

/// Receiver of this phone's I0 traffic (privileged Android control, later
/// the embedded router). Both calls hold no network lock.
pub trait MobileInputSink: Send + Sync {
    /// A link became Ready (its streams are open) or was lost; read
    /// `MobileNetwork::links` for the current table.
    fn links_changed(&self);
    /// One I0 message from the currently admitted, ready link of `peer`,
    /// decoded with that link's admission context. Called on the link's
    /// receive task, so messages of one link arrive in order.
    fn message(&self, peer: Boot, link: LinkId, message: v2::Message);
    /// The peer closed or refused the I0 it writes to us while the link stays
    /// up (it paused, or retired its input session): release whatever it
    /// holds pressed here and drop it as an input source. Its next I0 starts
    /// over with an opening.
    fn input_closed(&self, _peer: DeviceId) {}
    /// `peer`'s DeviceFacts changed (a newer fact sequence arrived on its
    /// control stream); read `MobileNetwork::peer_facts` again. Facts usually
    /// arrive after the peer's I0 opening, so the router cannot take them only
    /// when the I0 pair attaches.
    fn facts_changed(&self, _peer: DeviceId) {}
    /// The shared layout register changed. Read it again before routing motion.
    fn layout_changed(&self) {}
    /// A member's emergency pause reached this phone (once per event): input
    /// received but not yet injected is dropped and everything pressed here
    /// or by this phone elsewhere is released, including the Android helper.
    fn emergency_pause(&self) {}
}

#[derive(Default)]
struct InputSinkSlot(RwLock<Option<Arc<dyn MobileInputSink>>>);

impl std::fmt::Debug for InputSinkSlot {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("InputSinkSlot").finish_non_exhaustive()
    }
}

impl InputSinkSlot {
    fn get(&self) -> Option<Arc<dyn MobileInputSink>> {
        self.0.read().expect("input sink lock poisoned").clone()
    }
}

/// How long the first connect round of a session waits for discovery. mDNS
/// has no "all answers are in" signal; devices on the link answer the first
/// queries within about a second, and the second query (1 s after the
/// first) catches a missed answer. Within this window and while a dial it
/// started is still running, "no device online" is not an answer yet.
const FIRST_ROUND_DISCOVERY: Duration = Duration::from_secs(3);

/// Link changes for the operations that wait on them (share targets, the
/// current-text read, device cards), and the session's first connect round.
#[derive(Debug)]
struct Connectivity {
    changed: tokio::sync::watch::Sender<u64>,
    round: Mutex<ConnectRound>,
    /// Per peer, how many attempts to reach it ended without a link (dial
    /// failed, incompatible, or the link was lost). `confirm_reachable`
    /// waits for this to move.
    unreached: Mutex<HashMap<DeviceId, u64>>,
}

#[derive(Debug, Default)]
struct ConnectRound {
    id: u64,
    discovering: bool,
    /// Dials of this owner whose outcome (Ready, failure, loss) is pending.
    dialing: HashSet<DeviceId>,
}

impl Connectivity {
    fn changed(&self) {
        self.changed.send_modify(|value| *value = value.wrapping_add(1));
    }

    fn dial_ended(&self, peer: DeviceId) {
        self.round
            .lock()
            .expect("connect round mutex poisoned")
            .dialing
            .remove(&peer);
        self.changed();
    }

    /// An attempt to reach `peer` ended without a link.
    fn unreached(&self, peer: DeviceId) {
        *self
            .unreached
            .lock()
            .expect("unreached mutex poisoned")
            .entry(peer)
            .or_default() += 1;
        self.dial_ended(peer);
    }

    fn unreached_count(&self, peer: DeviceId) -> u64 {
        self.unreached
            .lock()
            .expect("unreached mutex poisoned")
            .get(&peer)
            .copied()
            .unwrap_or(0)
    }
}

/// What kind of process runs this core. Capabilities mean "can decode" and
/// follow from the build and the process kind only; they never change while
/// the process runs (protocol-v2 1.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MobileProcess {
    /// The app: clipboard, device options, input (I0), audio with microphone,
    /// diagnostics.
    #[default]
    App,
    /// A short-lived share session (the iOS share extension): clipboard and
    /// device options only.
    Share,
    /// ReplayKit upload extension: exclusive session owner, device controls and speaker source.
    Broadcast,
}

/// Live facts from the native owners. They describe what is usable right now
/// and never change the negotiated capabilities: audio availability goes to
/// the endpoint directions (`supported`), `privileged_input_ready` to the I0
/// `InputCapability.controllable`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MobilePlatformFacts {
    pub microphone_send: bool,
    pub speaker_receive: bool,
    /// Native output capture currently available (helper / broadcast extension).
    pub speaker_send: bool,
    /// The Android privileged helper accepts input, including asleep or at keyguard.
    pub privileged_input_ready: bool,
}

/// Why a persisted `group-state.v2` cannot start a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupStateProblem {
    /// There is no state file (first launch is provisioned explicitly).
    Missing,
    /// The file does not decode or verify (CBOR, anchor, signatures).
    Unreadable,
    /// A valid group that does not contain this identity, e.g. a state file
    /// restored onto another iPhone whose Keychain identity stayed behind.
    NotMember,
}

/// Decodes and verifies a persisted group state for `device`.
pub fn decode_group_state(bytes: &[u8], device: DeviceId) -> Result<State, MobileNetworkError> {
    let unreadable = |detail: String| MobileNetworkError::GroupState {
        problem: GroupStateProblem::Unreadable,
        detail,
    };
    let wire = v2::cbor::decode_map(bytes, v2::limits::CONFIG_STATE).map_err(|error| unreadable(error.to_string()))?;
    let state = dto::GroupState::from_raw(wire)
        .and_then(State::from_wire)
        .map_err(|error| unreadable(error.to_string()))?;
    if !state.roster().contains(device) {
        return Err(MobileNetworkError::GroupState {
            problem: GroupStateProblem::NotMember,
            detail: "group roster does not contain this device identity".into(),
        });
    }
    Ok(state)
}

#[derive(Debug, thiserror::Error)]
pub enum MobileNetworkError {
    #[error("network start failed: {0}")]
    Start(String),
    #[error("persistent v2 group state is unusable ({problem:?}): {detail}")]
    GroupState { problem: GroupStateProblem, detail: String },
    #[error("network peer is not an admitted group member: {0}")]
    Unauthorized(DeviceId),
    #[error("network peer is not ready: {0}")]
    NotReady(DeviceId),
    #[error("network dial refused: {0}")]
    Dial(String),
    #[error("network stream failed: {0}")]
    Stream(String),
    #[error("pairing refused: {0:?}")]
    Pairing(dto::PairProblem),
    #[error("v2 protocol error: {0}")]
    Protocol(String),
}

#[derive(Debug)]
struct ControlSession {
    peer_boot: Boot,
    greeting: admission::GreetingSession,
    peer: Option<admission::Peer>,
    facts: Option<dto::DeviceFacts>,
    pending_controls: HashMap<[u8; 16], device_control::Pending>,
    sent: Option<State>,
    accepted: Option<State>,
    /// The anchor this C0 was last admitted under; a root change retires the
    /// admission but keeps this lane for the peer's signed transition proof.
    admitted_anchor: Option<oink_network::roster::Anchor>,
    /// Pairing and merge messages run only on a direct C0 (protocol §6.6).
    direct: bool,
    ready: bool,
    departing: bool,
    outbound: bool,
    tx: SendStream,
}

#[derive(Debug)]
struct NetworkState {
    clipboard: Mutex<Weak<crate::MobileV2Session<MobileNetworkLink>>>,
    runtime: tokio::runtime::Handle,
    links: LinkLayer,
    identity: Arc<oink_network::identity::Identity>,
    state: Mutex<State>,
    state_path: PathBuf,
    advert: Advert,
    local_facts: Mutex<dto::DeviceFacts>,
    controls: Mutex<HashMap<LinkId, ControlSession>>,
    peer_links: Mutex<HashMap<DeviceId, LinkId>>,
    senders: Mutex<HashMap<(DeviceId, StreamKind), SendStream>>,
    /// The latest D0 hello of each peer with a ready link. A phone is not a
    /// diagnostics holder; nothing else on D0 is for it.
    diagnostics_hello: Mutex<HashMap<DeviceId, dto::DiagnosticsHello>>,
    process: MobileProcess,
    platform_facts: RwLock<MobilePlatformFacts>,
    controllable: AtomicBool,
    input_opening: RwLock<input_opening::Provider>,
    input_motion: Mutex<HashMap<DeviceId, oink_network::link::DatagramSend>>,
    profile: OfferProfile,
    audio_inbound: Mutex<VecDeque<AudioInbound>>,
    input_sink: InputSinkSlot,
    /// The streams this phone writes, one machine per link and kind.
    outbound: outbound::Outbound,
    connectivity: Connectivity,
    audio_changed: tokio::sync::Notify,
    audio_tx: Mutex<HashMap<StreamId, DatagramSend>>,
    audio_rx: Mutex<HashMap<LinkId, AudioReceive>>,
    audio_config: Mutex<VecDeque<(GroupId, Boot, v2::Message)>>,
    /// The group's speaker exit as this phone's volume-key context, published
    /// by the media owner and read by the embedded router at each press.
    volume_exit: Mutex<Option<oink_routing::input::VolumeRoute>>,
    /// Volume keys this phone executes as the exit, for the media owner.
    exit_volume_keys: Mutex<VecDeque<crate::input_router::ExitVolumeKey>>,
    /// Shared member decisions (`oink_network::member`). Lock order:
    /// pairing → controls → state.
    pairing: Mutex<oink_network::member::Membership>,
    /// Emergency events already acknowledged (origin Boot, sequence).
    emergencies: Mutex<std::collections::HashSet<(Boot, u64)>>,
    blocks: Mutex<blocks::BlockList>,
}

#[derive(Debug)]
struct AudioInbound {
    stream: StreamId,
    peer: DeviceId,
    group: GroupId,
    frame: Vec<u8>,
}

#[derive(Debug)]
struct AudioReceive {
    stream: StreamId,
    datagram: Option<DatagramRecv>,
}

/// What `cancel_pairing` did: a cancel after the target's durable decision
/// cannot withdraw it (protocol §6.6 cancel/commit boundary).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    Cancelled,
    CommitUnconfirmed,
    AlreadyCommitted,
}

#[derive(Clone, Debug)]
pub struct MobileNetworkLink {
    state: Arc<NetworkState>,
}

#[derive(Clone, Debug)]
pub struct NetworkDiagnosticSink {
    pending: tokio::sync::mpsc::Sender<crate::DiagnosticSegment>,
    discarded: Arc<std::sync::atomic::AtomicU64>,
}

impl NetworkDiagnosticSink {
    pub fn new(network: MobileNetwork) -> Self {
        use std::sync::atomic::Ordering;

        let (pending, mut receiver) = tokio::sync::mpsc::channel::<crate::DiagnosticSegment>(4);
        let discarded = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let lost = discarded.clone();
        let runtime = network.state.runtime.clone();
        runtime.spawn(async move {
            while let Some(mut segment) = receiver.recv().await {
                segment.evicted_events = segment.evicted_events.saturating_add(lost.swap(0, Ordering::Relaxed));
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    upload_diagnostics(&network, &segment),
                )
                .await;
                if !matches!(&result, Ok(Ok(()))) {
                    count_discarded(&lost, &segment);
                }
            }
        });
        Self { pending, discarded }
    }
}

impl crate::DiagnosticSink for NetworkDiagnosticSink {
    fn submit_segment(&self, segment: crate::DiagnosticSegment) -> Result<(), String> {
        self.pending.try_send(segment).map_err(|error| {
            count_discarded(&self.discarded, &error.into_inner());
            "diagnostics upload queue unavailable".to_owned()
        })
    }
}

fn count_discarded(lost: &std::sync::atomic::AtomicU64, segment: &crate::DiagnosticSegment) {
    use std::sync::atomic::Ordering;
    let count = segment
        .events
        .iter()
        .fold(segment.evicted_events, |total, event| {
            total.saturating_add(u64::from(event.count))
        })
        .saturating_add(segment.logs.len() as u64);
    let _ = lost.try_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
        Some(total.saturating_add(count))
    });
}

async fn upload_diagnostics(network: &MobileNetwork, segment: &crate::DiagnosticSegment) -> Result<(), String> {
    if network.state.process != MobileProcess::App {
        return Err("diagnostics are not collected in a share session".into());
    }
    let source = network.state.identity.id();
    if segment.mobile_device_id != hex(source) {
        return Err("diagnostics source does not match this mobile owner".into());
    }
    let preferred = segment.proxy_id.as_deref().map(parse_device).transpose()?;
    let table = network.state.links.links();
    let peer = {
        let state = network.state.state.lock().map_err(|_| "group state mutex poisoned")?;
        let roster = state.roster();
        let eligible = |entry: &&oink_network::link::LinkEntry| {
            entry.state == LinkState::Ready
                && entry.path == LinkPath::Direct
                && entry.permits(v2::registry::type_id::DiagnosticsChunk)
                && roster.members().any(|member| {
                    member.device == entry.peer && matches!(member.os.as_deref(), Some("macos" | "windows"))
                })
        };
        table
            .links
            .iter()
            .filter(eligible)
            .min_by_key(|entry| (preferred != Some(entry.peer), entry.link))
            .ok_or("no ready desktop diagnostic proxy")?
    };
    let source_wire = oink_network::wire_identity::device(source).write();
    let proxy_wire = oink_network::wire_identity::device(peer.peer).write();
    let session = parse_id16(&segment.session_id)?;
    let batch = parse_id16(&segment.batch_id)?;
    let error_text = |error: v2::Error| error.to_string();
    let hello = dto::DiagnosticsHello::from_raw(
        [
            (1, RawValue::Text(segment.session_id.clone())),
            (2, true.write()),
            (3, false.write()),
            (4, false.write()),
            (5, source_wire.clone()),
            (6, proxy_wire.clone()),
            (7, RawValue::Bytes(session.to_vec())),
            (8, true.write()),
            (9, (crate::diagnostics::AGGREGATE_WINDOW_MS as u32).write()),
        ]
        .into(),
    )
    .map_err(error_text)?;
    // Reliable D0 and Low are independent. The Low operation carries the
    // complete source/session identity even if Hello is still queued.
    let _ = network.send_message(peer.peer, v2::Message::DiagnosticsHello(hello));
    let payload = serde_json::to_vec(&crate::DiagnosticUpload {
        events: segment.events.clone(),
        logs: segment.logs.clone(),
        dropped_events: segment.evicted_events,
    })
    .map_err(|error| error.to_string())?;
    if payload.len() > crate::DiagnosticUpload::MAX_BYTES {
        return Err("mobile diagnostic segment too large".into());
    }
    const CHUNK_BYTES: usize = 16 * 1024;
    const UPLOAD_BYTES: u64 = crate::DiagnosticUpload::MAX_BYTES as u64 + 16 * 1024;
    let mut stream = network
        .state
        .links
        .open_low(
            peer.link,
            StreamKind::Diagnostics,
            LowOptions {
                send_max: UPLOAD_BYTES,
                receive_max: 4096,
                operation_max: UPLOAD_BYTES + 4096,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    let metadata = || {
        [
            (5, source_wire.clone()),
            (6, proxy_wire.clone()),
            (7, RawValue::Bytes(session.to_vec())),
            (8, RawValue::Bytes(batch.to_vec())),
            (9, segment.sequence.write()),
        ]
    };
    for (index, bytes) in payload.chunks(CHUNK_BYTES).enumerate() {
        let chunk = dto::DiagnosticsChunk::from_raw(
            [
                (1, RawValue::Bytes(bytes.to_vec())),
                (2, RawValue::Bytes(session.to_vec())),
                (3, segment.sequence.write()),
                (4, ((index * CHUNK_BYTES) as u32).write()),
            ]
            .into_iter()
            .chain(metadata())
            .collect(),
        )
        .map_err(error_text)?;
        stream
            .send(&v2::Message::DiagnosticsChunk(chunk))
            .await
            .map_err(|error| error.to_string())?;
    }
    let end = dto::DiagnosticsSegmentEnd::from_raw(
        [
            (1, RawValue::Bytes(session.to_vec())),
            (2, segment.sequence.write()),
            (3, (payload.len() as u32).write()),
            (4, (payload.len() as u32).write()),
        ]
        .into_iter()
        .chain(metadata())
        .collect(),
    )
    .map_err(error_text)?;
    stream
        .send(&v2::Message::DiagnosticsSegmentEnd(end))
        .await
        .map_err(|error| error.to_string())?;
    stream.finish();
    let Some(v2::Message::DiagnosticsEnd(receipt)) = stream.recv().await.map_err(|error| error.to_string())? else {
        return Err("missing diagnostic upload receipt".into());
    };
    if receipt.request_id().map_err(error_text)?.0 != session
        || receipt.partial().map_err(error_text)?
        || stream.recv().await.map_err(|error| error.to_string())?.is_some()
    {
        return Err("invalid diagnostic upload receipt".into());
    }
    Ok(())
}

impl MobileNetworkLink {
    pub fn attach_clipboard(&self, transfer: &Arc<crate::MobileV2Session<Self>>) {
        *self.state.clipboard.lock().expect("mobile clipboard mutex poisoned") = Arc::downgrade(transfer);
    }
}

impl crate::MobileV2Link for MobileNetworkLink {
    fn send_frame(&self, peer_id: &str, frame: Vec<u8>) -> Result<(), crate::MobileV2TransportError> {
        let peer = parse_device(peer_id).map_err(crate::MobileV2TransportError::Link)?;
        let senders = self
            .state
            .senders
            .lock()
            .map_err(|_| crate::MobileV2TransportError::Link("network sender mutex poisoned".into()))?;
        let Some(sender) = senders.get(&(peer, StreamKind::Clipboard)) else {
            drop(senders);
            // A desktop that paused clipboard sharing refused our B0: say so at
            // once instead of looking offline.
            if outbound::paused(&self.state, peer, StreamKind::Clipboard) {
                return Err(crate::MobileV2TransportError::PeerPaused(peer_id.into()));
            }
            return Err(crate::MobileV2TransportError::PeerUnavailable(peer_id.into()));
        };
        sender
            .post(frame)
            .map_err(|error| crate::MobileV2TransportError::Link(error.to_string()))
    }

    fn is_pc_member(&self, peer: &str) -> bool {
        let Ok(peer) = parse_device(peer) else {
            return false;
        };
        self.state
            .state
            .lock()
            .expect("group state mutex")
            .roster()
            .member(peer)
            .and_then(|member| member.os.as_deref())
            .is_some_and(|os| matches!(os.to_ascii_lowercase().as_str(), "macos" | "mac" | "windows" | "linux"))
    }
    fn supports_history(&self, peer: &str) -> bool {
        parse_device(peer)
            .ok()
            .and_then(|peer| self.state.links.links().by_peer(peer).cloned())
            .is_some_and(|entry| entry.permits(v2::registry::type_id::HistorySummary))
    }
    fn group_id(&self) -> Option<GroupId> {
        Some(self.state.state.lock().ok()?.roster().anchor().group)
    }

    fn peer_boot(&self, peer_id: &str) -> Option<Boot> {
        let peer = parse_device(peer_id).ok()?;
        self.state.links.links().by_peer(peer).map(|entry| entry.peer_boot)
    }

    fn is_member(&self, peer_id: &str) -> bool {
        parse_device(peer_id).is_ok_and(|peer| {
            self.state
                .state
                .lock()
                .expect("mobile group mutex poisoned")
                .roster()
                .contains(peer)
        })
    }

    fn is_peer_ready(&self, peer_id: &str) -> Option<bool> {
        Some(parse_device(peer_id).is_ok_and(|peer| self.state.links.links().ready(peer)))
    }

    /// A share session is short-lived (it ends when the extension closes),
    /// so it does not take phone-to-phone transfers.
    fn accepts_direct(&self) -> bool {
        self.state.process == MobileProcess::App
    }

    fn send_low(
        &self,
        peer_id: &str,
        operation: crate::ClipboardLowOperation,
    ) -> Result<(), crate::MobileV2TransportError> {
        use crate::MobileV2TransportError::{Link, PeerUnavailable};

        let peer = parse_device(peer_id).map_err(Link)?;
        let table = self.state.links.links();
        let entry = table
            .by_peer(peer)
            .filter(|entry| entry.state == oink_network::link::LinkState::Ready)
            .ok_or_else(|| PeerUnavailable(peer_id.into()))?;
        let link = entry.link;
        let state = self.state.clone();
        let transfer = clipboard_owner(&state).ok_or_else(|| Link("clipboard transfer owner is unavailable".into()))?;
        let download = operation
            .read
            .as_ref()
            .map(|request| transfer.download_attempt(request))
            .transpose()?
            .flatten();
        let peer_id = peer_id.to_owned();
        self.state.runtime.spawn(async move {
            let operation_id = operation.operation_id;
            let history = operation.is_history();
            let read = operation.read.clone();
            let result = async {
                let mut stream = state
                    .links
                    .open_low(
                        link,
                        StreamKind::Clipboard,
                        LowOptions {
                            send_max: u64::MAX / 2,
                            receive_max: u64::MAX / 2,
                            operation_max: u64::MAX,
                        },
                    )
                    .await
                    .map_err(|error| Link(error.to_string()))?;
                if let Some(request) = &read {
                    stream.send(request).await.map_err(|error| Link(error.to_string()))?;
                } else if !history {
                    transfer.upload_opened(&peer_id, operation_id);
                }
                operation
                    .send_parts(&mut stream, |sent, total| {
                        transfer.send_progress(operation_id, sent, total);
                    })
                    .await?;
                // For a read, the desktop's native worker watches the
                // request half for cancellation until it sends its reply.
                // Uploads instead require EOF before the receiver publishes.
                if read.is_none() {
                    stream.finish();
                }
                // A read waits for its source only while bytes keep coming:
                // the same 300 s no-progress rule as the PC content reader.
                // `ContentPreparing` keeps a listing alive but, as on the PC,
                // does not reset the timer. The upload half has no such
                // limit; its end is the target's ShareResult or link loss.
                let mut deadline = read.is_some().then(|| tokio::time::Instant::now() + READ_NO_PROGRESS);
                let mut completed = false;
                while let Some(message) = recv_low(&mut stream, deadline).await? {
                    let Some(request) = &read else {
                        return Err(Link("unexpected clipboard upload response".into()));
                    };
                    if !matches!(message, v2::Message::ContentPreparing(_)) {
                        deadline = Some(tokio::time::Instant::now() + READ_NO_PROGRESS);
                    }
                    let final_record = matches!(
                        message,
                        v2::Message::ContentRepresentationEnd(_)
                            | v2::Message::ContentFileEnd(_)
                            | v2::Message::ContentDirectory(_)
                            | v2::Message::ContentThumbnail(_)
                    );
                    if final_record && (recv_low(&mut stream, deadline).await?.is_some() || !stream.effect_validity()())
                    {
                        return Err(Link(
                            "clipboard response was interrupted or contains trailing records".into(),
                        ));
                    }
                    completed = transfer.receive_low_reply(operation_id, request, download, message)?;
                    if completed {
                        stream.finish();
                        break;
                    }
                }
                if read.is_some() && !completed {
                    return Err(Link("clipboard response ended before completion".into()));
                }
                Ok(())
            }
            .await;
            if history {
                transfer.finish_history_send(&peer_id, operation_id, result);
            } else {
                transfer.finish_low(&peer_id, operation_id, read.is_none(), download, result.err());
            }
        });
        Ok(())
    }
}

/// No-progress limit of a phone-side content read, matching the PC's
/// `NO_PROGRESS_MS` (300 s). Only a source that sends nothing for this long
/// fails the download; the attempt is released so the user can retry.
pub(crate) const READ_NO_PROGRESS: Duration = Duration::from_secs(300);

async fn recv_low(
    stream: &mut oink_network::link::LowStream,
    deadline: Option<tokio::time::Instant>,
) -> Result<Option<v2::Message>, crate::MobileV2TransportError> {
    use crate::MobileV2TransportError::Link;
    let received = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, stream.recv())
            .await
            .map_err(|_| Link(format!("no content progress for {} s", READ_NO_PROGRESS.as_secs())))?,
        None => stream.recv().await,
    };
    received.map_err(|error| Link(error.to_string()))
}

#[derive(Clone, Debug)]
pub struct MobileNetwork {
    state: Arc<NetworkState>,
    pub link: Arc<MobileNetworkLink>,
}

impl MobileNetwork {
    pub async fn start(config: MobileNetworkConfig) -> Result<Self, MobileNetworkError> {
        let identity = Arc::new(oink_network::identity::Identity::from_seed(config.identity_seed));
        let supplied_state = config.group_state.is_some();
        let bytes = match config.group_state {
            Some(bytes) => bytes,
            None => std::fs::read(&config.group_state_path).map_err(|error| MobileNetworkError::GroupState {
                problem: if error.kind() == std::io::ErrorKind::NotFound {
                    GroupStateProblem::Missing
                } else {
                    GroupStateProblem::Unreadable
                },
                detail: format!("{}: {error}", config.group_state_path.display()),
            })?,
        };
        // Fence uncommitted decisions before serving any query, then finish a
        // committed projection that the previous process did not persist.
        let (mut member, pending) =
            oink_network::member::Membership::start(&config.pairing_journal_path, identity.clone(), config.boot)
                .map_err(|error| MobileNetworkError::Start(error.to_string()))?;
        // A phone keeps one request waiting for the user (user 2026-11-11);
        // the next one is refused at once with TooManyPendingRequests.
        member.pairs.set_pending_limit(1);
        let state = match pending {
            Some(wire) => {
                let digest = oink_network::group_merge::digest(&wire).map_err(protocol_start)?;
                let state = decode_group_state(
                    &wire.encode(v2::limits::CONFIG_STATE).map_err(protocol_start)?,
                    config.boot.device_id,
                )?;
                persist_state(&config.group_state_path, &state)?;
                member
                    .replayed(digest)
                    .map_err(|error| MobileNetworkError::Start(error.to_string()))?;
                state
            }
            None => {
                let state = decode_group_state(&bytes, config.boot.device_id)?;
                if supplied_state {
                    persist_state(&config.group_state_path, &state)?;
                }
                state
            }
        };
        let process = config.process;
        let profile = profile_for(process);
        // Everything that can fail is loaded before the link layer starts, so
        // a failed start leaves no listener or network thread behind.
        let blocks = blocks::BlockList::load(config.pairing_journal_path.with_file_name(blocks::FILE))?;
        let (links, mut config_port) = LinkLayer::start(LinkConfig {
            profile: profile.clone(),
            identity: identity.clone(),
            boot: config.boot,
            bind: config.bind,
            tier: Tier::Elevated,
        })
        .await
        .map_err(|error| MobileNetworkError::Start(error.to_string()))?;
        let stream_kinds = vec![
            StreamKind::Clipboard,
            StreamKind::Audio,
            StreamKind::Diagnostics,
            StreamKind::Input,
        ];
        let mut module_port = links.register(&stream_kinds);
        let mut advert = Advert::local(
            identity.id(),
            config.device_name,
            state.roster().anchor().group,
            state.roster().members().count() as u8,
            links.local_addr().port(),
            config.os,
        );
        advert.device_kind = dto::DeviceKind::Mobile;
        let local_facts = match facts::new(config.boot, &advert, &profile, links.local_addr(), config.form_factor) {
            Ok(facts) => facts,
            Err(error) => {
                links.stop().await;
                return Err(protocol_start(error));
            }
        };
        let state = Arc::new(NetworkState {
            clipboard: Mutex::new(Weak::new()),
            runtime: tokio::runtime::Handle::current(),
            links: links.clone(),
            identity: identity.clone(),
            state: Mutex::new(state),
            state_path: config.group_state_path,
            advert,
            local_facts: Mutex::new(local_facts),
            controls: Mutex::new(HashMap::new()),
            peer_links: Mutex::new(HashMap::new()),
            senders: Mutex::new(HashMap::new()),
            diagnostics_hello: Mutex::new(HashMap::new()),
            process,
            platform_facts: RwLock::new(MobilePlatformFacts::default()),
            controllable: AtomicBool::new(false),
            input_opening: RwLock::new(input_opening::Provider(Arc::new(DefaultInputOpening))),
            input_motion: Mutex::new(HashMap::new()),
            profile,
            audio_inbound: Mutex::new(VecDeque::new()),
            input_sink: InputSinkSlot::default(),
            outbound: outbound::Outbound::default(),
            connectivity: Connectivity {
                changed: tokio::sync::watch::channel(0).0,
                round: Mutex::new(ConnectRound::default()),
                unreached: Mutex::new(HashMap::new()),
            },
            audio_changed: tokio::sync::Notify::new(),
            audio_tx: Mutex::new(HashMap::new()),
            audio_rx: Mutex::new(HashMap::new()),
            audio_config: Mutex::new(VecDeque::new()),
            volume_exit: Mutex::new(None),
            exit_volume_keys: Mutex::new(VecDeque::new()),
            blocks: Mutex::new(blocks),
            pairing: Mutex::new(member),
            emergencies: Mutex::new(std::collections::HashSet::new()),
        });
        let worker_state = state.clone();
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel::<(LinkId, Result<Vec<u8>, String>)>();
        tokio::spawn(async move {
            let mut pairing_tick = tokio::time::interval(std::time::Duration::from_secs(1));
            pairing_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut endpoint_poll = 0u8;
            loop {
                tokio::select! {
                    _ = pairing_tick.tick() => {
                        let _ = member_host::tick(&worker_state);
                        // Interfaces change without a callback here; the PC polls
                        // them every 2 s as well (route list freshness, design 3.1).
                        endpoint_poll = (endpoint_poll + 1) % 2;
                        if endpoint_poll == 0 {
                            facts::refresh_endpoints(&worker_state);
                        }
                    }
                    event = config_port.next() => {
                        let Some(event) = event else { break };
                        if let Err(error) = connected(&worker_state, event, control_tx.clone()) {
                            // C0 errors are handled by the link lifetime below;
                            // no untrusted peer detail is retained in diagnostics.
                            eprintln!("mobile C0 connection rejected: {error}");
                        }
                    }
                    Some((link, bytes)) = control_rx.recv() => {
                        if let Err(error) = control(&worker_state, link, bytes) {
                            // Same diagnostic as the desktop owner: why this C0 closed.
                            tracing::debug!(%error, link = link.0, "mobile C0 validation failed");
                            worker_state.links.disconnect(link, oink_network::link::Disconnect::HandshakeFailed);
                        }
                    }
                    event = module_port.next() => {
                        let Some(event) = event else { break };
                        module_event(&worker_state, event);
                    }
                }
            }
        });
        let link = Arc::new(MobileNetworkLink { state: state.clone() });
        let network = Self { state, link };
        network.begin_connect_round();
        Ok(network)
    }

    /// Starts a connect round: until discovery had its window and every dial
    /// started meanwhile ended, `connect_round_settled` is false. Called on
    /// start and when the host resumes a suspended session.
    pub fn begin_connect_round(&self) {
        let id = {
            let mut round = self
                .state
                .connectivity
                .round
                .lock()
                .expect("connect round mutex poisoned");
            round.id = round.id.wrapping_add(1);
            round.discovering = true;
            round.id
        };
        self.state.connectivity.changed();
        let state = Arc::downgrade(&self.state);
        self.state.runtime.spawn(async move {
            tokio::time::sleep(FIRST_ROUND_DISCOVERY).await;
            let Some(state) = state.upgrade() else { return };
            let mut round = state.connectivity.round.lock().expect("connect round mutex poisoned");
            if round.id == id {
                round.discovering = false;
                drop(round);
                state.connectivity.changed();
            }
        });
    }

    /// The current connect round ended: no device that is not ready now is
    /// expected to become ready because of it. Alone in its group, a phone
    /// has nothing to wait for.
    pub fn connect_round_settled(&self) -> bool {
        let alone = self
            .state
            .state
            .lock()
            .expect("group state mutex poisoned")
            .roster()
            .members()
            .all(|member| member.device == self.state.identity.id());
        let round = self
            .state
            .connectivity
            .round
            .lock()
            .expect("connect round mutex poisoned");
        alone || (!round.discovering && round.dialing.is_empty())
    }

    /// Changes whenever a link became ready or was lost, a dial ended, a
    /// peer's facts changed or the connect round advanced.
    pub fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.state.connectivity.changed.subscribe()
    }

    /// Reaches group member `peer` now: true once its link is ready (at
    /// once when it already is), false when this attempt ends without a
    /// link (every address and relay failed, it is incompatible, or the
    /// link was lost) or `peer` is not a member. There is no time limit of
    /// its own: the dial's budget decides. A dial or link already under way
    /// is waited for instead of starting another.
    pub async fn confirm_reachable(&self, peer: DeviceId, candidates: Vec<SocketAddr>) -> bool {
        let mut changes = self.subscribe_changes();
        let before = self.state.connectivity.unreached_count(peer);
        let member = |network: &Self| network.members().iter().any(|member| member.device == peer);
        if self.state.links.links().ready(peer) {
            return true;
        }
        if peer == self.state.identity.id() || !member(self) {
            return false;
        }
        // Busy or AlreadyLinked: an attempt is under way; its outcome counts.
        let _ = self.dial_member(peer, candidates, oink_network::link::DialOrigin::Initial);
        loop {
            if self.state.links.links().ready(peer) {
                return true;
            }
            if self.state.connectivity.unreached_count(peer) != before || !member(self) {
                return false;
            }
            if changes.changed().await.is_err() {
                return false;
            }
        }
    }

    /// Replaces this kind's stream on every link (its opening changed, e.g.
    /// whether this phone can be controlled), without counting a failure.
    pub fn reopen_outbound(&self, kind: StreamKind) {
        outbound::reopen(&self.state, kind);
    }

    /// The stream of `kind` this phone writes to `peer` now, if open.
    pub fn outbound_stream(&self, peer: DeviceId, kind: StreamKind) -> Option<StreamId> {
        self.state
            .senders
            .lock()
            .expect("mobile sender mutex poisoned")
            .get(&(peer, kind))
            .map(SendStream::id)
    }

    pub fn set_input_sink(&self, sink: Option<Arc<dyn MobileInputSink>>) {
        *self.state.input_sink.0.write().expect("input sink lock poisoned") = sink.clone();
        if let Some(sink) = sink {
            sink.layout_changed();
        }
    }

    /// The last diagnostics hello a peer sent on D0 while its link is ready.
    pub fn diagnostics_hello(&self, peer: DeviceId) -> Option<dto::DiagnosticsHello> {
        self.state
            .diagnostics_hello
            .lock()
            .expect("diagnostics hello mutex poisoned")
            .get(&peer)
            .cloned()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.state.links.local_addr()
    }

    pub fn advertisement(&self) -> Advert {
        let group = self.state.state.lock().expect("group state mutex poisoned");
        advert_for(&self.state, &group)
    }

    pub fn members(&self) -> Vec<oink_network::roster::Member> {
        self.state
            .state
            .lock()
            .expect("group state mutex poisoned")
            .roster()
            .members()
            .cloned()
            .collect()
    }

    pub fn links(&self) -> Arc<oink_network::link::LinkTable> {
        self.state.links.links()
    }

    /// Reopens the endpoint socket on the same port. Android tags a socket with
    /// the network the process was bound to when it was created; after the
    /// host moved the process to another network (Wi-Fi reconnect), the old
    /// socket can no longer reach the LAN (review H17).
    pub async fn rebind_endpoint(&self) -> Result<(), MobileNetworkError> {
        self.state
            .links
            .rebind_socket()
            .await
            .map_err(|error| MobileNetworkError::Start(format!("endpoint socket rebind: {error}")))
    }

    /// This phone is the speaker exit or sends its microphone: on iOS the QUIC
    /// socket that carries the audio datagrams is marked voice meanwhile
    /// (user 2026-11-11). Call on change only.
    pub fn set_audio_marking(&self, audio: bool) {
        self.state.links.set_audio_marking(audio);
    }

    pub fn suspend_links(&self) {
        for link in &self.links().links {
            self.state
                .links
                .disconnect(link.link, oink_network::link::Disconnect::Sleeping);
        }
    }

    pub fn authenticated_advert(&self, peer: DeviceId) -> Option<Advert> {
        // The C0 receive path acquires controls before peer_links.
        let controls = self.state.controls.lock().ok()?;
        let links = self.state.peer_links.lock().ok()?;
        let session = controls.get(links.get(&peer)?)?;
        Advert::from_wire(&session.peer.as_ref()?.greeting.advert().ok()?).ok()
    }

    pub async fn probe_address(&self, address: SocketAddr) -> Result<DeviceId, oink_network::link::AddressProbeError> {
        self.state.links.probe_address(address).await
    }

    pub fn input_pointer_base(&self) -> Option<DeviceId> {
        self.state.state.lock().ok()?.pointer_base().ok().flatten()
    }

    /// Current group-shared geometry, consumed by the embedded input router.
    pub fn input_layout(&self) -> Option<dto::LayoutValue> {
        let state = self.state.state.lock().ok()?;
        v2::layout::register_value(&state.layout().ok()?).ok()
    }

    pub fn state_bytes(&self) -> Result<Vec<u8>, MobileNetworkError> {
        self.state
            .state
            .lock()
            .map_err(|_| MobileNetworkError::Stream("group state mutex poisoned".into()))?
            .wire()
            .encode(v2::limits::CONFIG_STATE)
            .map_err(protocol_start)
    }

    pub fn admitted_peers(&self) -> Result<Vec<DeviceId>, MobileNetworkError> {
        Ok(self
            .state
            .state
            .lock()
            .map_err(|_| MobileNetworkError::Stream("group state mutex poisoned".into()))?
            .roster()
            .members()
            .map(|member| member.device)
            .filter(|device| *device != self.state.identity.id())
            .collect())
    }

    pub fn process(&self) -> MobileProcess {
        self.state.process
    }

    pub fn platform_facts(&self) -> MobilePlatformFacts {
        *self
            .state
            .platform_facts
            .read()
            .expect("mobile platform facts lock poisoned")
    }

    /// Record what the native owners can do right now. No link is touched and
    /// nothing is renegotiated (protocol-v2 1.1): the media owner reads the
    /// audio facts into its endpoint directions, and a changed
    /// `privileged_input_ready` reopens the outbound I0s with the new
    /// `InputCapability.controllable`.
    pub fn update_platform_facts(&self, facts: MobilePlatformFacts) {
        *self
            .state
            .platform_facts
            .write()
            .expect("mobile platform facts lock poisoned") = facts;
        self.set_controllable(facts.privileged_input_ready);
        self.state.audio_changed.notify_one();
    }

    pub fn diagnostic_sink(&self) -> Result<NetworkDiagnosticSink, MobileNetworkError> {
        if self.state.process != MobileProcess::App {
            return Err(MobileNetworkError::Protocol(
                "diagnostics are not collected in a share session".into(),
            ));
        }
        Ok(NetworkDiagnosticSink::new(self.clone()))
    }

    pub fn boot(&self) -> Boot {
        self.state.links.boot()
    }

    /// Outbound audio streams of admitted peers, including their current Boot.
    pub fn audio_peers(&self) -> Vec<(Boot, StreamId)> {
        let links = self.links();
        let controls = self.state.controls.lock().expect("mobile control mutex");
        let senders = self.state.senders.lock().expect("mobile sender mutex");
        senders
            .iter()
            .filter_map(|((_, kind), sender)| {
                if *kind != StreamKind::Audio {
                    return None;
                }
                let stream = sender.id();
                let session = controls.get(&stream.link)?;
                let path = links.by_peer(session.peer_boot.device_id)?;
                (session.ready
                    && path.opens(StreamKind::Audio)
                    && path.link == stream.link
                    && path.state == oink_network::link::LinkState::Ready)
                    .then_some((session.peer_boot, stream))
            })
            .collect()
    }

    pub fn send_audio_message(
        &self,
        peer: Boot,
        stream: StreamId,
        message: &v2::Message,
    ) -> Result<(), MobileNetworkError> {
        let kind = StreamKind::Audio;
        let frame = v2::frame::encode(message, kind.max_message()).map_err(protocol_start)?;
        let senders = self.state.senders.lock().expect("mobile sender mutex");
        let sender = senders
            .get(&(peer.device_id, kind))
            .filter(|sender| sender.id() == stream)
            .ok_or(MobileNetworkError::NotReady(peer.device_id))?;
        sender
            .post(frame)
            .map_err(|error| MobileNetworkError::Stream(error.to_string()))
    }

    pub fn audio_support(&self, peer: Boot) -> Vec<dto::AudioSupport> {
        let controls = self.state.controls.lock().expect("mobile control mutex");
        controls
            .values()
            .filter(|session| session.ready && session.peer_boot == peer)
            .find_map(|session| session.peer.as_ref().map(|peer| peer.audio.clone()))
            .unwrap_or_default()
    }

    pub fn audio_datagram_sender(&self, stream: StreamId) -> Option<DatagramSend> {
        self.state
            .audio_tx
            .lock()
            .expect("audio sender mutex")
            .get(&stream)
            .cloned()
    }

    /// The caller owns the single receive side of M1 for this peer.
    pub fn audio_datagram_receiver(&self, peer: Boot) -> Option<DatagramRecv> {
        let (_, stream) = self.audio_peers().into_iter().find(|(boot, _)| *boot == peer)?;
        self.state
            .audio_rx
            .lock()
            .expect("audio receiver mutex")
            .get(&stream.link)
            .and_then(|receive| receive.datagram.clone())
    }

    pub fn poll_audio_config(&self) -> Vec<(Boot, v2::Message)> {
        let current = self.group_id();
        self.state
            .audio_config
            .lock()
            .expect("audio config mutex")
            .drain(..)
            .filter_map(|(group, peer, message)| (current == group).then_some((peer, message)))
            .collect()
    }

    pub fn send_audio_config_result(&self, peer: Boot, message: v2::Message) -> Result<(), MobileNetworkError> {
        let links = self.links();
        let link = links
            .by_peer(peer.device_id)
            .ok_or(MobileNetworkError::NotReady(peer.device_id))?;
        let controls = self.state.controls.lock().expect("mobile controls mutex");
        let session = controls
            .get(&link.link)
            .filter(|session| session.ready && session.peer_boot == peer)
            .ok_or(MobileNetworkError::NotReady(peer.device_id))?;
        let frame = v2::frame::encode(&message, v2::limits::CONFIG_FRAME).map_err(protocol_start)?;
        session
            .tx
            .post(frame)
            .map_err(|error| MobileNetworkError::Stream(error.to_string()))
    }

    pub async fn audio_changed(&self) {
        self.state.audio_changed.notified().await;
    }

    /// The media owner's current speaker exit for the control page's volume
    /// keys (`None`: no exit, or its volume is not Ready).
    pub fn set_volume_exit(&self, route: Option<oink_routing::input::VolumeRoute>) {
        *self.state.volume_exit.lock().expect("volume exit mutex") = route;
    }

    pub fn volume_exit(&self) -> Option<oink_routing::input::VolumeRoute> {
        self.state.volume_exit.lock().expect("volume exit mutex").clone()
    }

    /// A volume key for this phone as the exit. Keys are bounded like audio
    /// commands; a full queue drops the key (detection log) instead of the link.
    pub fn push_exit_volume_key(&self, key: crate::input_router::ExitVolumeKey) {
        let mut queued = self.state.exit_volume_keys.lock().expect("exit volume keys mutex");
        if queued.len() >= 128 {
            tracing::warn!(source = ?key.source.device_id, "exit volume key dropped: queue full");
            return;
        }
        queued.push_back(key);
        drop(queued);
        self.state.audio_changed.notify_one();
    }

    pub fn poll_exit_volume_keys(&self) -> Vec<crate::input_router::ExitVolumeKey> {
        self.state
            .exit_volume_keys
            .lock()
            .expect("exit volume keys mutex")
            .drain(..)
            .collect()
    }

    pub fn advertised_capabilities(&self) -> Vec<String> {
        self.state.profile.capabilities.clone()
    }

    pub fn device_id(&self) -> String {
        hex(self.state.identity.id())
    }

    /// Post an already typed v2 message on the stream selected by its domain.
    /// Callers never provide a frame or a JSON transport request.
    pub fn send_message(&self, peer: DeviceId, message: v2::Message) -> Result<(), MobileNetworkError> {
        let kind = message_stream_kind(&message).ok_or_else(|| {
            MobileNetworkError::Protocol(format!("message {} has no mobile stream route", message.type_id()))
        })?;
        let frame = v2::frame::encode(&message, kind.max_message()).map_err(protocol_start)?;
        let senders = self
            .state
            .senders
            .lock()
            .map_err(|_| MobileNetworkError::Stream("mobile sender mutex poisoned".into()))?;
        let sender = senders.get(&(peer, kind)).ok_or(MobileNetworkError::NotReady(peer))?;
        sender
            .post(frame)
            .map_err(|error| MobileNetworkError::Stream(error.to_string()))
    }

    /// Each audio message remains associated with the authenticated link
    /// that delivered it. A buffered frame from a superseded link is dropped.
    pub fn poll_audio_messages(&self) -> Vec<(Boot, v2::Message)> {
        let inbound = std::mem::take(&mut *self.state.audio_inbound.lock().expect("mobile audio mutex"));
        let group = self.group_id();
        let links = self.links();
        let controls = self.state.controls.lock().expect("mobile control mutex");
        let receivers = self.state.audio_rx.lock().expect("audio receiver mutex");
        inbound
            .into_iter()
            .filter_map(|incoming| {
                let link = links.by_peer(incoming.peer)?;
                if incoming.group != group
                    || link.link != incoming.stream.link
                    || receivers.get(&link.link)?.stream != incoming.stream
                    || link.state != oink_network::link::LinkState::Ready
                {
                    return None;
                }
                let session = controls.get(&link.link).filter(|session| session.ready)?;
                let context = v2::admission::Context {
                    kind: StreamKind::Audio as u16,
                    transport: v2::registry::Transport::Reliable,
                    phase: v2::admission::Phase::Member,
                    agreement: &session.peer.as_ref()?.agreement,
                    direct: matches!(link.path, oink_network::link::LinkPath::Direct),
                    old_member_lane: false,
                };
                let frame = OpaqueFrame::parse_exact(&incoming.frame, StreamKind::Audio.max_message()).ok()?;
                let message = context.decode(frame).ok()?;
                Some((session.peer_boot, message))
            })
            .collect()
    }

    /// Direct dial only: pairing and an explicit address. TLS authenticates the
    /// discovered identity, while C0 admission gates business streams. This
    /// permits a newly discovered device to reach ConfigPair without treating
    /// it as a trusted member.
    pub fn dial(&self, peer: DeviceId, candidates: Vec<SocketAddr>) -> Result<(), MobileNetworkError> {
        self.state
            .links
            .dial(
                peer,
                candidates,
                oink_network::link::DialMode::DirectOnly,
                oink_network::link::DialOrigin::Initial,
            )
            .map_err(|error| MobileNetworkError::Dial(error.to_string()))?;
        self.dial_started(peer);
        Ok(())
    }

    /// A dial started while the round discovers is part of the round until
    /// its outcome (Ready, failure or loss). Later redials are not: after
    /// the first round "no device online" is a valid answer.
    fn dial_started(&self, peer: DeviceId) {
        let mut round = self
            .state
            .connectivity
            .round
            .lock()
            .expect("connect round mutex poisoned");
        if round.discovering {
            round.dialing.insert(peer);
        }
    }

    /// Reconnect a group member as a PC does: the candidate addresses first,
    /// then (or at once without any) a relay through a PC that reaches it.
    /// `origin` is `BackgroundRetry` for a timed retry after a failed attempt.
    pub fn dial_member(
        &self,
        peer: DeviceId,
        candidates: Vec<SocketAddr>,
        origin: oink_network::link::DialOrigin,
    ) -> Result<(), MobileNetworkError> {
        self.state
            .links
            .dial(peer, candidates, oink_network::link::DialMode::PreferDirect, origin)
            .map_err(|error| MobileNetworkError::Dial(error.to_string()))?;
        self.dial_started(peer);
        Ok(())
    }

    /// Replace the reconnect candidates of a known peer (e.g. a user-edited
    /// member address); a live link keeps running on its current path.
    pub fn update_addresses(&self, peer: DeviceId, candidates: Vec<SocketAddr>) {
        self.state.links.update_addresses(peer, candidates);
    }

    pub fn dial_hex(&self, peer: &str, candidates: Vec<SocketAddr>) -> Result<(), MobileNetworkError> {
        self.dial(parse_device(peer).map_err(MobileNetworkError::Stream)?, candidates)
    }

    /// Legacy callers cannot grant membership. The roster is the only trust
    /// source after this migration.
    pub fn authorize(&self, peer: DeviceId) -> Result<(), MobileNetworkError> {
        if self
            .state
            .state
            .lock()
            .map_err(|_| MobileNetworkError::Stream("group state mutex poisoned".into()))?
            .roster()
            .contains(peer)
        {
            Ok(())
        } else {
            Err(MobileNetworkError::Unauthorized(peer))
        }
    }

    pub fn authorize_hex(&self, peer: &str) -> Result<(), MobileNetworkError> {
        self.authorize(parse_device(peer).map_err(MobileNetworkError::Stream)?)
    }

    /// Start the existing v2 ConfigPair protocol on an authenticated direct
    /// C0 link. The returned key is only a UI correlation; PAKE frames still
    /// travel as the frozen Message variants on C0.
    pub fn start_pairing(&self, peer: DeviceId) -> Result<String, MobileNetworkError> {
        let remote = {
            let controls = self
                .state
                .controls
                .lock()
                .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?;
            let link = self
                .state
                .peer_links
                .lock()
                .map_err(|_| MobileNetworkError::Stream("peer link mutex poisoned".into()))?
                .get(&peer)
                .copied()
                .ok_or(MobileNetworkError::NotReady(peer))?;
            controls
                .get(&link)
                .and_then(|session| session.peer.as_ref())
                .ok_or(MobileNetworkError::NotReady(peer))?
                .anchor()
                .map_err(protocol_start)?
        };
        self.start_pairing_with_anchor(peer, remote)
    }

    /// Start pairing when discovery supplied the remote roster anchor. C0's
    /// greeting intentionally carries only the group id; the root id must come
    /// from authenticated group discovery or a previously stored peer record.
    pub fn start_pairing_with_anchor(
        &self,
        peer: DeviceId,
        remote: oink_network::roster::Anchor,
    ) -> Result<String, MobileNetworkError> {
        let (peer_boot, peer_info) = {
            let controls = self
                .state
                .controls
                .lock()
                .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?;
            let link = self
                .state
                .peer_links
                .lock()
                .map_err(|_| MobileNetworkError::Stream("peer link mutex poisoned".into()))?
                .get(&peer)
                .copied()
                .ok_or(MobileNetworkError::NotReady(peer))?;
            let session = controls.get(&link).ok_or(MobileNetworkError::NotReady(peer))?;
            if session.peer.is_none() {
                return Err(MobileNetworkError::NotReady(peer));
            }
            (session.peer_boot, session.peer.as_ref().expect("peer").clone())
        };
        let local = self
            .state
            .state
            .lock()
            .map_err(|_| MobileNetworkError::Stream("group state mutex poisoned".into()))?
            .clone();
        if remote == local.roster().anchor() {
            return Err(MobileNetworkError::Protocol(
                "pairing requires distinct group anchors".into(),
            ));
        }
        let id = PairId {
            initiator_boot: self.state.links.boot(),
            random: rand::random(),
        };
        let context = dto::PairContext::from_raw(
            [
                (1, oink_network::wire_identity::pair(id).write()),
                (2, oink_network::wire_identity::boot(self.state.links.boot()).write()),
                (3, oink_network::wire_identity::boot(peer_boot).write()),
                (4, RawValue::Bytes(local.roster().anchor().group.0.to_vec())),
                (5, RawValue::Bytes(remote.group.0.to_vec())),
                (6, (local.roster().members().count() as u8).write()),
                (
                    7,
                    peer_info
                        .greeting
                        .advert()
                        .map_err(protocol_start)?
                        .count()
                        .map_err(protocol_start)?
                        .write(),
                ),
                (8, RawValue::Bytes(remote.group.0.to_vec())),
                (9, RawValue::Bytes(peer_info.hash().map_err(protocol_start)?.to_vec())),
                (10, RawValue::from(dto::PAIRING_SUITE)),
                (11, RawValue::Bytes(remote.root.to_vec())),
                (12, RawValue::Bytes(local.roster().anchor().root.to_vec())),
            ]
            .into(),
        )
        .map_err(protocol_start)?;
        member_host::pairing(&self.state, |pairs| {
            pairs.start(context, peer_boot, peer_info.transcript.clone())
        })?;
        Ok(pairing::key(id))
    }

    pub fn pairing_code(&self, key: &str) -> Result<String, MobileNetworkError> {
        let id = pairing::parse_key(key).ok_or_else(|| MobileNetworkError::Stream("invalid pairing id".into()))?;
        member_host::lock(&self.state)?
            .pairs
            .code(id)
            .map(|code| code.to_string())
            .map_err(pairing_error)
    }

    pub fn accept_pairing(&self, key: &str, revision: u64) -> Result<(), MobileNetworkError> {
        let id = pairing::parse_key(key).ok_or_else(|| MobileNetworkError::Stream("invalid pairing id".into()))?;
        member_host::pairing(&self.state, |pairs| pairs.accept(id, revision))
    }

    pub fn submit_pairing_code(
        &self,
        key: &str,
        revision: u64,
        attempt: u8,
        code: &str,
    ) -> Result<(), MobileNetworkError> {
        let id = pairing::parse_key(key).ok_or_else(|| MobileNetworkError::Stream("invalid pairing id".into()))?;
        member_host::pairing(&self.state, |pairs| {
            pairs.submit(id, revision, attempt, code, Instant::now())
        })
    }

    pub fn decline_pairing(&self, key: &str, revision: u64) -> Result<(), MobileNetworkError> {
        let id = pairing::parse_key(key).ok_or_else(|| MobileNetworkError::Stream("invalid pairing id".into()))?;
        member_host::pairing(&self.state, |pairs| pairs.decline(id, revision))
    }

    /// Cancel follows the target's durable decision (protocol §6.6): before it
    /// the pairing ends and its permit is released; after it the merge stands.
    pub fn cancel_pairing(&self, key: &str, revision: u64) -> Result<CancelOutcome, MobileNetworkError> {
        let id = pairing::parse_key(key).ok_or_else(|| MobileNetworkError::Stream("invalid pairing id".into()))?;
        let mut member = member_host::lock(&self.state)?;
        let current = member_host::current(&self.state)?;
        let (cancelled, outs) = member.cancel(id, revision, &current).map_err(pairing_error)?;
        member_host::execute(&self.state, &mut member, None, outs)?;
        Ok(match cancelled {
            oink_network::member::Cancelled::Cancelled => CancelOutcome::Cancelled,
            oink_network::member::Cancelled::CommitUnconfirmed => CancelOutcome::CommitUnconfirmed,
            oink_network::member::Cancelled::AlreadyCommitted(_) => CancelOutcome::AlreadyCommitted,
        })
    }

    pub fn pairing_snapshot(&self) -> Result<pairing::Snapshot, MobileNetworkError> {
        Ok(member_host::lock(&self.state)?.pairs.snapshot())
    }

    /// Resolves when the network layer has ended: `Ok` after [`Self::stop`],
    /// its `NetworkFatal` when it ended on its own (user 2026-11-12: fail
    /// fast). Every link is lost by then. Its Boot cannot come back with a new
    /// layer (peers keep the Boot's selection watermark), so the session's
    /// host restarts the whole session, never this network.
    pub async fn exited(&self) -> Result<(), oink_network::link::NetworkFatal> {
        self.state.links.exited().await
    }

    /// Test only: end the network layer as on a dead endpoint driver.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn inject_fatal_for_test(&self) {
        oink_network::link::testing::inject_fatal(&self.state.links);
    }

    pub async fn stop(&self) {
        self.state.links.stop().await;
        // Waiters re-check their session state (stopped) on this change.
        self.state.connectivity.changed();
    }
}

fn advert_for(state: &NetworkState, group: &State) -> Advert {
    let mut advert = state.advert.clone();
    advert.group = group.roster().anchor().group;
    advert.count = group.roster().members().count() as u8;
    if let Ok(facts) = state.local_facts.lock()
        && let Ok(name) = facts.name()
    {
        advert.name = name.0;
    }
    advert
}

fn connected(
    state: &Arc<NetworkState>,
    event: ConfigEvent,
    control_tx: tokio::sync::mpsc::UnboundedSender<(LinkId, Result<Vec<u8>, String>)>,
) -> Result<(), MobileNetworkError> {
    let event = match event {
        ConfigEvent::Connected(event) => *event,
        ConfigEvent::Lost { link, peer, .. } => {
            let (removed, replaced) = {
                let mut controls = state.controls.lock().expect("mobile control mutex poisoned");
                let removed = controls.remove(&link);
                // Another C0 of the same Boot (connected, possibly not yet
                // selected) carries on: a member redial replaced this one.
                let replaced = removed
                    .as_ref()
                    .is_some_and(|session| controls.values().any(|other| other.peer_boot == session.peer_boot));
                (removed, replaced)
            };
            let mut peer_links = state.peer_links.lock().expect("mobile peer link mutex poisoned");
            if peer_links.get(&peer) == Some(&link) {
                peer_links.remove(&peer);
            }
            drop(peer_links);
            // Interrupted object staging must not count against later pairings.
            // A pairing with this peer ends with LinkLost only when no
            // replacement C0 of the same Boot carries on (a member redial
            // replaces the pairing link; the desktop owner does the same).
            let ended = removed.filter(|_| !replaced).map(|session| session.peer_boot);
            let lost = member_host::lock(state).and_then(|mut member| {
                let current = member_host::current(state)?;
                let outs = member.link_lost(link, ended, &current);
                let _ = member_host::execute(state, &mut member, None, outs);
                Ok(())
            });
            state.connectivity.unreached(peer);
            return lost;
        }
        ConfigEvent::DialFailed { peer, .. } | ConfigEvent::ProtocolIncompatible { peer } => {
            state.connectivity.unreached(peer);
            return Ok(());
        }
        _ => return Ok(()),
    };
    let state_guard = state
        .state
        .lock()
        .map_err(|_| MobileNetworkError::Stream("group state mutex poisoned".into()))?;
    // A relayed link (through a PC relay station) negotiates its capabilities
    // on C0, bound to the installed route; a direct link reuses N0's result.
    let relay = admission::relay_context(state.links.boot(), &event).map_err(protocol_start)?;
    let (greeting, hello) = admission::GreetingSession::new(
        state.links.boot(),
        event.peer_boot,
        event.direction == Direction::Outbound,
        state.profile.clone(),
        event.negotiation,
        relay,
        advert_for(state, &state_guard),
        state_guard.roster().anchor(),
        state_guard.roster().digest().map_err(protocol_start)?,
        Instant::now(),
    )
    .map_err(protocol_start)?;
    drop(state_guard);
    let link = event.link;
    let mut receiver = event.control_rx;
    let tx = control_tx.clone();
    tokio::spawn(async move {
        loop {
            let next = receiver.next().await.map_err(|error| error.to_string());
            let ended = next.is_err();
            if tx.send((link, next)).is_err() || ended {
                break;
            }
        }
    });
    event.control_tx.set_wake(Arc::new(|| {}));
    state
        .controls
        .lock()
        .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?
        .insert(
            link,
            ControlSession {
                peer_boot: event.peer_boot,
                greeting,
                peer: None,
                facts: None,
                pending_controls: HashMap::new(),
                sent: None,
                accepted: None,
                admitted_anchor: None,
                ready: false,
                departing: false,
                outbound: event.direction == Direction::Outbound,
                direct: event.path == LinkPath::Direct,
                tx: event.control_tx,
            },
        );
    if let Some(hello) = hello {
        state
            .controls
            .lock()
            .expect("control mutex poisoned")
            .get(&link)
            .expect("inserted")
            .tx
            .post(hello)
            .map_err(|error| MobileNetworkError::Stream(error.to_string()))?;
    }
    Ok(())
}

fn control(state: &Arc<NetworkState>, link: LinkId, bytes: Result<Vec<u8>, String>) -> Result<(), MobileNetworkError> {
    let bytes = bytes.map_err(MobileNetworkError::Stream)?;
    let mut member = member_host::lock(state)?;
    let mut controls = state
        .controls
        .lock()
        .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?;
    let session = controls
        .get_mut(&link)
        .ok_or_else(|| MobileNetworkError::Stream("unknown C0 link".into()))?;
    if session.departing {
        return Ok(());
    }
    let now = Instant::now();
    if session.peer.is_none() {
        let response = session.greeting.receive(&bytes, now).map_err(protocol_start)?;
        if let Some(response) = response {
            session
                .tx
                .post(response)
                .map_err(|error| MobileNetworkError::Stream(error.to_string()))?;
        }
        let peer = session
            .greeting
            .result()
            .cloned()
            .ok_or_else(|| MobileNetworkError::Stream("incomplete C0 greeting".into()))?;
        state.links.set_agreement(link, peer.agreement.clone());
        state.links.set_peer_formats(link, peer.formats.clone());
        state.links.set_peer_audio(link, peer.audio.clone());
        let peer_device = session.peer_boot.device_id;
        session.peer = Some(peer);
        let outbound = session.outbound;
        let facts = member_host::facts_of(session).ok_or(MobileNetworkError::NotReady(peer_device))?;
        state
            .peer_links
            .lock()
            .expect("peer link mutex poisoned")
            .insert(peer_device, link);
        drop(controls);
        let current = member_host::current(state)?;
        let outs = member
            .greeted(link, &facts, outbound, &current, now)
            .map_err(protocol_start)?;
        return member_host::execute(state, &mut member, Some(link), outs);
    }
    let frame = OpaqueFrame::parse_exact(&bytes, v2::limits::CONFIG_FRAME).map_err(protocol_start)?;
    let message = frame.decode().map_err(protocol_start)?;
    let peer_boot = session.peer_boot;
    let peer = peer_boot.device_id;
    let facts = member_host::facts_of(session).ok_or(MobileNetworkError::NotReady(peer))?;
    let greeting = session.peer.clone().expect("greeting");
    let current = member_host::current(state)?;
    let member_ready = session.ready && current.roster().contains(peer);
    if oink_network::member::handles(&message) {
        let direct = session.direct;
        drop(controls);
        let gate = oink_network::member::Gate {
            enabled: true,
            blocked: state
                .blocks
                .lock()
                .map_err(|_| MobileNetworkError::Stream("block list mutex poisoned".into()))?
                .contains(peer),
            // The phone takes requests whenever Oink runs, on any page and in
            // the background; they wait in its request list (user 2026-11-11).
            exclusive: false,
            external_busy: false,
        };
        let outs = member
            .receive(link, &facts, &greeting, direct, message, &current, gate, now)
            .map_err(protocol_start)?;
        return member_host::execute(state, &mut member, Some(link), outs);
    }
    let outs = match message {
        v2::Message::ConfigRequest(request) => {
            drop(controls);
            member
                .request(link, &facts, &request, &current, 0)
                .map_err(protocol_start)?
        }
        v2::Message::ConfigReply(reply) => {
            let sent = session
                .sent
                .clone()
                .ok_or_else(|| MobileNetworkError::Stream("reply without request".into()))?;
            drop(controls);
            member
                .reply(link, &facts, &sent, &reply, &current)
                .map_err(protocol_start)?
        }
        v2::Message::ConfigDone(done) => {
            let accepted = session
                .accepted
                .clone()
                .ok_or_else(|| MobileNetworkError::Stream("done without accepted state".into()))?;
            drop(controls);
            member.done(link, &accepted, &done).map_err(protocol_start)?
        }
        v2::Message::ConfigState(incoming) => {
            if !session.ready {
                // Frames of the group we just left may still be queued behind
                // the peer's proof: verify and drain them, apply nothing.
                let old = member
                    .retired(&facts, &current, incoming.group().map_err(protocol_start)?.0)
                    .ok_or_else(|| protocol_start(v2::ErrorCode::NotReady.into()))?;
                State::verify(incoming, old).map_err(protocol_start)?;
                return Ok(());
            }
            drop(controls);
            member
                .state_update(link, peer, &incoming, &current)
                .map_err(protocol_start)?
        }
        v2::Message::ConfigFacts(incoming) => {
            let retired = incoming
                .group()
                .ok()
                .and_then(|group| member.retired(&facts, &current, group.0))
                .is_some();
            let changed = !retired && facts::receive(session, &current, incoming).map_err(protocol_start)?;
            let facts_peer = session.peer_boot.device_id;
            let peer_paused = session
                .facts
                .as_ref()
                .and_then(|facts| facts.pause().ok().flatten())
                .is_some_and(|pause| pause.paused() == Ok(true));
            if changed {
                if let Some(facts) = &session.facts {
                    state
                        .links
                        .update_route_addresses(session.peer_boot.device_id, facts::route_addresses(facts));
                }
                state.audio_changed.notify_one();
                state.connectivity.changed();
            }
            drop(controls);
            drop(member);
            if changed {
                outbound::peer_facts(state, link, peer_paused);
                if let Some(sink) = state.input_sink.get() {
                    sink.facts_changed(facts_peer);
                }
            }
            return Ok(());
        }
        message if device_control::handles(&message) => {
            if !member_ready {
                return Err(MobileNetworkError::Protocol(
                    "device control requires admitted group membership".into(),
                ));
            }
            let direct = state
                .links
                .links()
                .by_peer(peer)
                .is_some_and(|entry| entry.link == link && entry.path == oink_network::link::LinkPath::Direct);
            return device_control::receive(session, frame, direct, message);
        }
        request @ (v2::Message::ConfigSetExitVolume(_) | v2::Message::ConfigSetLocalOption(_))
            if member_ready && matches!(state.process, MobileProcess::App | MobileProcess::Broadcast) =>
        {
            let mut queued = state.audio_config.lock().expect("audio config mutex");
            if queued.len() >= 128 {
                return Err(MobileNetworkError::Protocol("too many mobile audio commands".into()));
            }
            queued.push_back((current.roster().anchor().group, session.peer_boot, request));
            state.audio_changed.notify_one();
            return Ok(());
        }
        message @ (v2::Message::ConfigSetLocalOption(_) | v2::Message::ConfigSetExitVolume(_)) if member_ready => {
            return device_control::receive_unsupported(session, frame, message);
        }
        v2::Message::ConfigEmergencyPause(request) if member_ready => {
            drop(controls);
            emergency::receive(state, link, peer_boot, request)?
        }
        // An error from a member: nothing to answer.
        v2::Message::ProtocolError(_) if member_ready => return Ok(()),
        // Negotiated but not handled on a phone: a typed error, the C0 stays
        // (change plan §2.2). Only an admitted member gets one.
        message if member_ready => {
            drop(controls);
            vec![oink_network::member::unhandled(link, &message).map_err(protocol_start)?]
        }
        _ => return Err(MobileNetworkError::Protocol("unexpected C0 message".into())),
    };
    member_host::execute(state, &mut member, Some(link), outs)
}

fn post_control(state: &NetworkState, peer: DeviceId, message: v2::Message) -> Result<(), MobileNetworkError> {
    let frame = v2::frame::encode(&message, v2::limits::CONFIG_FRAME).map_err(protocol_start)?;
    let link = state
        .peer_links
        .lock()
        .map_err(|_| MobileNetworkError::Stream("peer link mutex poisoned".into()))?
        .get(&peer)
        .copied()
        .ok_or(MobileNetworkError::NotReady(peer))?;
    state
        .controls
        .lock()
        .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?
        .get(&link)
        .ok_or(MobileNetworkError::NotReady(peer))?
        .tx
        .post(frame)
        .map_err(|error| MobileNetworkError::Stream(error.to_string()))
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn clipboard_owner(state: &NetworkState) -> Option<Arc<crate::MobileV2Session<MobileNetworkLink>>> {
    state.clipboard.lock().ok()?.upgrade()
}

fn module_event(state: &Arc<NetworkState>, event: ModuleEvent) {
    match event {
        ModuleEvent::LinkReady { link, peer } => {
            outbound::link_ready(state, link, peer);
            state.audio_changed.notify_one();
            if let Some(transfer) = clipboard_owner(state) {
                transfer.peer_ready(&hex(peer));
            }
            state.connectivity.dial_ended(peer);
            if let Some(sink) = state.input_sink.get() {
                sink.links_changed();
            }
        }
        ModuleEvent::Inbound(stream)
            if matches!(
                stream.tag.kind,
                StreamKind::Clipboard | StreamKind::Audio | StreamKind::Diagnostics | StreamKind::Input
            ) =>
        {
            let peer = stream.peer;
            let link = stream.link;
            let kind = stream.tag.kind;
            if !stream_enabled(state, kind) {
                return;
            }
            if kind == StreamKind::Audio
                && state
                    .audio_rx
                    .lock()
                    .expect("audio receiver mutex")
                    .get(&link)
                    .is_some_and(|prior| prior.stream.tag.ordinal >= stream.tag.ordinal)
            {
                stream.reject(oink_protocol::frame::StreamCode::StaleOrdinal);
                return;
            }
            let (mut receiver, datagram) = stream.accept(QueueSpec::reliable(kind).1, QueueSpec::datagram(kind));
            // A peer that opens a stream to us is sharing again.
            outbound::peer_resumed(state, link);
            if kind == StreamKind::Audio {
                state.audio_rx.lock().expect("audio receiver mutex").insert(
                    link,
                    AudioReceive {
                        stream: receiver.id(),
                        datagram,
                    },
                );
                state.audio_changed.notify_one();
            }
            let target = state.clone();
            tokio::spawn(async move {
                while let Ok(frame) = receiver.next().await {
                    if kind == StreamKind::Audio {
                        let group = target.state.lock().expect("group state mutex").roster().anchor().group;
                        let mut pending = target.audio_inbound.lock().expect("mobile audio mutex");
                        if pending.len() >= 128 {
                            drop(pending);
                            target
                                .links
                                .disconnect(link, oink_network::link::Disconnect::HandshakeFailed);
                            break;
                        }
                        pending.push_back(AudioInbound {
                            stream: receiver.id(),
                            peer,
                            group,
                            frame,
                        });
                        target.audio_changed.notify_one();
                        continue;
                    }
                    match kind {
                        StreamKind::Clipboard => {
                            // The transfer owner is attached right after the
                            // network starts, before discovery dials anyone;
                            // after shutdown nothing reads clipboard traffic.
                            let Some(transfer) = clipboard_owner(&target) else {
                                continue;
                            };
                            if target
                                .links
                                .links()
                                .by_peer(peer)
                                .is_some_and(|entry| entry.link == link)
                                && let Err(error) = transfer.receive_frame(&hex(peer), &frame)
                            {
                                transfer.error(None, None, error.to_string());
                            }
                        }
                        StreamKind::Input => {
                            // Dispatched here, in order, without any host poll.
                            if let Some((boot, message)) = decode_input(&target, peer, link, &frame)
                                && let Some(sink) = target.input_sink.get()
                            {
                                sink.message(boot, link, message);
                            }
                        }
                        StreamKind::Diagnostics => {
                            if let Ok(v2::Message::DiagnosticsHello(hello)) =
                                OpaqueFrame::parse_exact(&frame, kind.max_message()).and_then(|frame| frame.decode())
                            {
                                target
                                    .diagnostics_hello
                                    .lock()
                                    .expect("diagnostics hello mutex poisoned")
                                    .insert(peer, hello);
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
        ModuleEvent::InboundLow(inbound)
            if matches!(inbound.stream.kind(), StreamKind::Clipboard | StreamKind::Diagnostics) =>
        {
            let oink_network::link::InboundLow { mut stream, first } = *inbound;
            let peer = stream.peer();
            let kind = stream.kind();
            if !stream_enabled(state, kind) {
                return;
            }
            // A phone holds no diagnostics to collect (PCs gather from PCs
            // only) and, without a transfer owner, takes no clipboard
            // operation: refuse instead of leaving the operation unread.
            let transfer = (kind == StreamKind::Clipboard)
                .then(|| clipboard_owner(state))
                .flatten();
            let Some(transfer) = transfer else {
                stream.cancel();
                return;
            };
            tokio::spawn(async move {
                if matches!(
                    first,
                    v2::Message::AttachmentItems(_)
                        | v2::Message::HistoryIndex(_)
                        | v2::Message::HistoryRecords(_)
                        | v2::Message::HistoryDelta(_)
                ) {
                    if matches!(stream.recv().await, Ok(None)) && stream.effect_validity()() {
                        match transfer.receive_message(&hex(peer), first.clone()) {
                            Ok(()) => stream.finish(),
                            Err(error) => {
                                stream.cancel();
                                transfer.fail_history_receive(&hex(peer), &first, error.to_string());
                                transfer.error(None, None, error.to_string());
                            }
                        }
                    } else {
                        stream.cancel();
                        transfer.fail_history_receive(&hex(peer), &first, "history response interrupted".into());
                    }
                    return;
                }
                if let Err(error) = transfer.receive_share_stream(&hex(peer), first, &mut stream).await {
                    stream.cancel();
                    transfer.error(None, None, error.to_string());
                }
            });
        }
        ModuleEvent::LinkLost { link, peer, .. } => {
            outbound::link_lost(state, link);
            let replaced = state
                .links
                .links()
                .by_peer(peer)
                .is_some_and(|entry| entry.link != link);
            if !replaced && let Some(transfer) = clipboard_owner(state) {
                transfer.peer_lost(&hex(peer));
            }
            state
                .senders
                .lock()
                .expect("mobile sender mutex")
                .retain(|_, sender| sender.id().link != link);
            state
                .input_motion
                .lock()
                .expect("input motion mutex poisoned")
                .retain(|_, motion| motion.follows().link != link);
            // `peer_links` names the peer's C0 and belongs to the C0 owner
            // (ConfigEvent::Lost). This event also comes for a retirement,
            // which ends only the business streams while C0 drains on.
            state
                .audio_tx
                .lock()
                .expect("audio sender mutex")
                .retain(|stream, _| stream.link != link);
            state.audio_rx.lock().expect("audio receiver mutex").remove(&link);
            if !replaced {
                state
                    .diagnostics_hello
                    .lock()
                    .expect("diagnostics hello mutex poisoned")
                    .remove(&peer);
            }
            state.audio_changed.notify_one();
            state.connectivity.changed();
            if let Some(sink) = state.input_sink.get() {
                sink.links_changed();
            }
        }
        ModuleEvent::StreamClosed { stream, reason } => {
            if stream.tag.kind == StreamKind::Audio {
                state.audio_tx.lock().expect("audio sender mutex").remove(&stream);
                state
                    .audio_rx
                    .lock()
                    .expect("audio receiver mutex")
                    .retain(|_, receiver| receiver.stream != stream);
                state.audio_changed.notify_one();
            }
            outbound::closed(state, stream, reason);
        }
        _ => {}
    }
}

/// One I0 frame from `link`, decoded with its admission context when that
/// link is the peer's admitted, ready one. Anything else is dropped.
fn decode_input(state: &NetworkState, peer: DeviceId, link: LinkId, bytes: &[u8]) -> Option<(Boot, v2::Message)> {
    // The C0 receive path acquires controls before peer_links.
    let controls = state.controls.lock().expect("mobile control mutex");
    if state.peer_links.lock().expect("mobile peer link mutex").get(&peer) != Some(&link) {
        return None;
    }
    let session = controls.get(&link).filter(|session| session.ready)?;
    let direct = state
        .links
        .links()
        .by_peer(peer)
        .is_some_and(|entry| entry.link == link && entry.path == LinkPath::Direct);
    let context = v2::admission::Context {
        kind: StreamKind::Input as u16,
        transport: v2::registry::Transport::Reliable,
        phase: v2::admission::Phase::Member,
        agreement: &session.peer.as_ref()?.agreement,
        direct,
        old_member_lane: false,
    };
    let frame = OpaqueFrame::parse_exact(bytes, StreamKind::Input.max_message()).ok()?;
    Some((session.peer_boot, context.decode(frame).ok()?))
}

/// Inbound streams this process serves. The link layer already admitted the
/// kind against the negotiation.
fn stream_enabled(state: &NetworkState, kind: StreamKind) -> bool {
    match kind {
        StreamKind::Clipboard => state.process != MobileProcess::Broadcast,
        StreamKind::Audio => matches!(state.process, MobileProcess::App | MobileProcess::Broadcast),
        StreamKind::Diagnostics | StreamKind::Input => state.process == MobileProcess::App,
        _ => false,
    }
}

/// The negotiated capability set of a process kind, fixed for its lifetime.
fn profile_for(process: MobileProcess) -> OfferProfile {
    let mut profile = OfferProfile::default();
    profile
        .capabilities
        .retain(|capability| matches!(capability.as_str(), "oink.core/2" | "oink.group/2"));
    profile.capabilities.push("oink.low/1".into());
    profile.capabilities.push("oink.clipboard/2".into());
    profile
        .capabilities
        .push(oink_foundation::clipboard_history::HISTORY_CAPABILITY.into());
    profile.capabilities.push("oink.device/2".into());
    // A phone terminates relayed paths (a PC reaches it through another PC)
    // but never forwards one: no oink.relay.transit/1 (protocol-v2 §1.1).
    profile.capabilities.push(oink_network::link::RELAY_ENDPOINT.into());
    let formats = RawValue::Array(
        [
            "mime:text/plain;charset=utf-8",
            "mime:text/html;charset=utf-8",
            "mime:application/rtf",
            "mime:image/png",
            "oink:file-namespace/1",
            dto::URL_FORMAT,
        ]
        .into_iter()
        .map(RawValue::from)
        .collect(),
    );
    profile.formats = dto::FormatSupport::from_raw(
        [
            (1, formats.clone()),
            (2, formats),
            (3, RawValue::Array(Vec::new())),
            (4, RawValue::Array(vec![dto::Medium::Bytes.write()])),
        ]
        .into(),
    )
    .expect("constant mobile clipboard profile");
    if process == MobileProcess::App {
        profile.capabilities.extend(
            [
                "oink.audio/2",
                "oink.audio.microphone/1",
                "oink.diagnostics/2",
                "oink.input/2",
            ]
            .map(String::from),
        );
        profile.audio = oink_audio::negotiation::audio();
    }
    if process == MobileProcess::Broadcast {
        // No clipboard, input, microphone or diagnostics in the constrained extension.
        profile
            .capabilities
            .retain(|cap| cap != "oink.clipboard/2" && cap != "oink.low/1");
        profile.capabilities.push("oink.audio/2".into());
        profile.audio = oink_audio::negotiation::audio();
    }
    profile.capabilities.sort();
    profile.capabilities.dedup();
    profile
}

fn message_stream_kind(message: &v2::Message) -> Option<StreamKind> {
    let entry = v2::registry::message(message.type_id())?;
    entry
        .routes
        .first()
        .and_then(|route| StreamKind::from_wire(route.kind as u8))
}

fn persist_state(path: &Path, state: &State) -> Result<(), MobileNetworkError> {
    let bytes = state.wire().encode(v2::limits::CONFIG_STATE).map_err(protocol_start)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| MobileNetworkError::Start(error.to_string()))?;
    }
    oink_foundation::storage::atomic_write(path, &bytes).map_err(|error| MobileNetworkError::Start(error.to_string()))
}

fn protocol_start(error: v2::Error) -> MobileNetworkError {
    MobileNetworkError::Protocol(error.to_string())
}

fn pairing_error(error: oink_protocol::v2::dto::PairProblem) -> MobileNetworkError {
    MobileNetworkError::Pairing(error)
}

fn parse_device(value: &str) -> Result<DeviceId, String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("device id must be 32-byte hex".into());
    }
    let bytes = (0..64)
        .step_by(2)
        .map(|offset| u8::from_str_radix(&value[offset..offset + 2], 16).map_err(|_| "invalid device id".to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DeviceId(bytes.try_into().map_err(|_| "invalid device id")?))
}

fn parse_id16(value: &str) -> Result<[u8; 16], String> {
    let normalized;
    let value = if value.len() == 36 && [8, 13, 18, 23].iter().all(|index| value.as_bytes()[*index] == b'-') {
        normalized = value.replace('-', "");
        normalized.as_str()
    } else {
        value
    };
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("id16 must be 16-byte hex or a UUID".into());
    }
    let mut bytes = [0u8; 16];
    for (index, output) in bytes.iter_mut().enumerate() {
        *output = u8::from_str_radix(&value[2 * index..2 * index + 2], 16).map_err(|_| "invalid id16")?;
    }
    Ok(bytes)
}

fn hex(device: DeviceId) -> String {
    device.0.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MobileV2Link;
    use oink_protocol::v2::RawMap;
    use std::sync::atomic::Ordering;

    fn state_bytes(seed: [u8; 32], name: &str) -> Vec<u8> {
        let identity = oink_network::identity::Identity::from_seed(seed);
        State::create(&identity, name, "test", 1)
            .unwrap()
            .wire()
            .encode(v2::limits::CONFIG_STATE)
            .unwrap()
    }

    fn shared_state_bytes() -> (Vec<u8>, Vec<u8>) {
        let left = oink_network::identity::Identity::from_seed([11; 32]);
        let right = oink_network::identity::Identity::from_seed([12; 32]);
        let base = State::create(&left, "left", "test", 1).unwrap();
        let roster = base.roster().add(&left, right.id(), "right", "test", 2).unwrap();
        let shared = base.with_roster(&roster).unwrap();
        let bytes = shared.wire().encode(v2::limits::CONFIG_STATE).unwrap();
        (bytes.clone(), bytes)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mobile_network_requires_persisted_group_state_before_business_link() {
        let left_identity = oink_network::identity::Identity::from_seed([11; 32]);
        let right_identity = oink_network::identity::Identity::from_seed([12; 32]);
        let left_dir = tempfile::tempdir().unwrap();
        let result = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: [11; 32],
            boot: Boot {
                device_id: left_identity.id(),
                nonce: [1; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: None,
            group_state_path: left_dir.path().join("group-state.v2"),
            pairing_journal_path: left_dir.path().join("pairing-recovery.cbor"),
            device_name: "left".into(),
            os: "test".into(),
            form_factor: None,
            trusted_peers: vec![right_identity.id()],
            trust_path: None,
            process: MobileProcess::App,
        })
        .await;
        assert!(matches!(
            result,
            Err(MobileNetworkError::GroupState {
                problem: GroupStateProblem::Missing,
                ..
            })
        ));
    }

    #[test]
    fn persisted_state_round_trips_without_manufacturing_an_anchor() {
        let bytes = state_bytes([11; 32], "left");
        let map = v2::cbor::decode_map(&bytes, v2::limits::CONFIG_STATE).unwrap();
        let state = State::from_wire(dto::GroupState::from_raw(map).unwrap()).unwrap();
        assert!(
            state
                .roster()
                .contains(oink_network::identity::Identity::from_seed([11; 32]).id())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capability_profile_matches_ready_mobile_owners() {
        let identity = oink_network::identity::Identity::from_seed([31; 32]);
        let dir = tempfile::tempdir().unwrap();
        let network = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: [31; 32],
            boot: Boot {
                device_id: identity.id(),
                nonce: [9; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(state_bytes([31; 32], "android")),
            group_state_path: dir.path().join("group-state.v2"),
            pairing_journal_path: dir.path().join("pairing-recovery.cbor"),
            device_name: "android".into(),
            os: "android".into(),
            form_factor: None,
            trusted_peers: Vec::new(),
            trust_path: None,
            process: MobileProcess::App,
        })
        .await
        .unwrap();
        let profile = network.advertised_capabilities();
        for capability in [
            "oink.core/2",
            "oink.group/2",
            "oink.clipboard/2",
            "oink.low/1",
            "oink.audio/2",
            "oink.audio.microphone/1",
            "oink.diagnostics/2",
            "oink.input/2",
        ] {
            assert!(profile.iter().any(|value| value == capability), "missing {capability}");
        }
        assert!(profile.iter().any(|value| value == "oink.relay/1"), "relay endpoint");
        assert!(
            !profile.iter().any(|value| value == "oink.relay.transit/1"),
            "never a relay station"
        );
        assert!(!profile.iter().any(|value| value == "oink.pairing/2"));
        // Review M37 / plan 4: platform facts never change what was negotiated.
        assert!(!network.controllable());
        network.update_platform_facts(MobilePlatformFacts {
            microphone_send: true,
            speaker_receive: true,
            speaker_send: true,
            privileged_input_ready: true,
        });
        assert!(network.controllable());
        assert_eq!(network.advertised_capabilities(), profile);
        network.update_platform_facts(MobilePlatformFacts::default());
        assert!(!network.controllable());
        assert_eq!(network.advertised_capabilities(), profile);
        network.stop().await;
    }

    /// PCs list a phone's version from its DeviceFacts. The host's App
    /// version replaces the core crate's version (0.4.0 phones showed 0.3.6).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_hosts_app_version_is_published_in_the_facts() {
        let identity = oink_network::identity::Identity::from_seed([32; 32]);
        let dir = tempfile::tempdir().unwrap();
        let network = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: [32; 32],
            boot: Boot {
                device_id: identity.id(),
                nonce: [9; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(state_bytes([32; 32], "android")),
            group_state_path: dir.path().join("group-state.v2"),
            pairing_journal_path: dir.path().join("pairing-recovery.cbor"),
            device_name: "android".into(),
            os: "android".into(),
            form_factor: None,
            trusted_peers: Vec::new(),
            trust_path: None,
            process: MobileProcess::App,
        })
        .await
        .unwrap();
        let versions = |network: &MobileNetwork| {
            let facts = network.state.local_facts.lock().unwrap().clone();
            (facts.build().unwrap().0, facts.app_version().unwrap().0)
        };
        let (_, before) = versions(&network);
        assert_eq!(before, env!("CARGO_PKG_VERSION"));
        assert!(network.set_app_version("0.4.0-inhouse.6").unwrap());
        assert_eq!(
            versions(&network),
            ("0.4.0-inhouse.6".to_owned(), "0.4.0-inhouse.6".to_owned())
        );
        assert!(!network.set_app_version("0.4.0-inhouse.6").unwrap(), "unchanged");
        network.stop().await;
    }

    /// User 2026-11-12: a network layer that ends on its own is reported to
    /// the session with its reason; a requested stop is not a failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_network_reports_a_fatal_exit_and_a_requested_stop_apart() {
        let start = |seed: [u8; 32], dir: &std::path::Path| {
            let identity = oink_network::identity::Identity::from_seed(seed);
            MobileNetwork::start(MobileNetworkConfig {
                identity_seed: seed,
                boot: Boot {
                    device_id: identity.id(),
                    nonce: [9; 16],
                },
                bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                group_state: Some(state_bytes(seed, "android")),
                group_state_path: dir.join("group-state.v2"),
                pairing_journal_path: dir.join("pairing-recovery.cbor"),
                device_name: "android".into(),
                os: "android".into(),
                form_factor: None,
                trusted_peers: Vec::new(),
                trust_path: None,
                process: MobileProcess::App,
            })
        };
        let dir = tempfile::tempdir().unwrap();
        let failing = start([33; 32], dir.path()).await.unwrap();
        failing.inject_fatal_for_test();
        let exited = tokio::time::timeout(Duration::from_secs(5), failing.exited())
            .await
            .expect("the layer ends on its own");
        assert!(
            matches!(
                exited,
                Err(oink_network::link::NetworkFatal::EndpointDriverStopped { .. })
            ),
            "{exited:?}"
        );
        assert!(failing.links().links.is_empty());
        let other = tempfile::tempdir().unwrap();
        let stopped = start([34; 32], other.path()).await.unwrap();
        stopped.stop().await;
        assert_eq!(stopped.exited().await, Ok(()));
    }

    #[test]
    fn broadcast_profile_is_audio_and_device_control_without_app_streams() {
        let profile = profile_for(MobileProcess::Broadcast);
        for capability in ["oink.core/2", "oink.group/2", "oink.device/2", "oink.audio/2"] {
            assert!(
                profile.capabilities.iter().any(|value| value == capability),
                "{capability}"
            );
        }
        for capability in [
            "oink.clipboard/2",
            "oink.low/1",
            "oink.input/2",
            "oink.audio.microphone/1",
            "oink.diagnostics/2",
        ] {
            assert!(
                !profile.capabilities.iter().any(|value| value == capability),
                "{capability}"
            );
        }
        assert!(!profile.audio.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn share_process_negotiates_only_clipboard_and_device_options() {
        let identity = oink_network::identity::Identity::from_seed([32; 32]);
        let dir = tempfile::tempdir().unwrap();
        let network = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: [32; 32],
            boot: Boot {
                device_id: identity.id(),
                nonce: [10; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(state_bytes([32; 32], "ordinary-mobile")),
            group_state_path: dir.path().join("group-state.v2"),
            pairing_journal_path: dir.path().join("pairing-recovery.cbor"),
            device_name: "ordinary-mobile".into(),
            os: "ios".into(),
            form_factor: None,
            trusted_peers: Vec::new(),
            trust_path: None,
            process: MobileProcess::Share,
        })
        .await
        .unwrap();
        let profile = network.advertised_capabilities();
        for capability in [
            "oink.core/2",
            "oink.group/2",
            "oink.clipboard/2",
            "oink.low/1",
            "oink.device/2",
        ] {
            assert!(profile.iter().any(|value| value == capability), "missing {capability}");
        }
        for capability in [
            "oink.input/2",
            "oink.audio/2",
            "oink.audio.microphone/1",
            "oink.diagnostics/2",
            "oink.pairing/2",
        ] {
            assert!(
                !profile.iter().any(|value| value == capability),
                "unexpected {capability}"
            );
        }
        network.stop().await;
    }

    #[test]
    fn input_and_audio_messages_use_frozen_stream_routes() {
        for name in [
            "InputKey",
            "InputMotion",
            "AudioEndpointState",
            "AudioEndpointRequest",
            "DiagnosticsHello",
            "DiagnosticsChunk",
        ] {
            let entry = v2::registry::MESSAGES.iter().find(|entry| entry.name == name).unwrap();
            let kind = StreamKind::from_wire(entry.routes[0].kind as u8).unwrap();
            assert_eq!(
                kind,
                match name {
                    "InputKey" | "InputMotion" => StreamKind::Input,
                    "AudioEndpointState" | "AudioEndpointRequest" => StreamKind::Audio,
                    _ => StreamKind::Diagnostics,
                }
            );
        }
    }

    async fn start_member(
        seed: [u8; 32],
        nonce: u8,
        state: Vec<u8>,
        dir: &Path,
        process: MobileProcess,
    ) -> MobileNetwork {
        let identity = oink_network::identity::Identity::from_seed(seed);
        MobileNetwork::start(MobileNetworkConfig {
            identity_seed: seed,
            boot: Boot {
                device_id: identity.id(),
                nonce: [nonce; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(state),
            group_state_path: dir.join("group-state.v2"),
            pairing_journal_path: dir.join("pairing-recovery.cbor"),
            device_name: "member".into(),
            os: "test".into(),
            form_factor: None,
            trusted_peers: vec![],
            trust_path: None,
            process,
        })
        .await
        .unwrap()
    }

    struct RecordingSink {
        messages: tokio::sync::mpsc::UnboundedSender<(Boot, v2::Message)>,
        link_changes: std::sync::atomic::AtomicUsize,
        closed: Mutex<Vec<DeviceId>>,
        emergencies: std::sync::atomic::AtomicUsize,
        facts: Mutex<Vec<DeviceId>>,
    }

    impl MobileInputSink for RecordingSink {
        fn links_changed(&self) {
            self.link_changes.fetch_add(1, Ordering::SeqCst);
        }
        fn message(&self, peer: Boot, _link: LinkId, message: v2::Message) {
            let _ = self.messages.send((peer, message));
        }
        fn input_closed(&self, peer: DeviceId) {
            self.closed.lock().unwrap().push(peer);
        }
        fn emergency_pause(&self) {
            self.emergencies.fetch_add(1, Ordering::SeqCst);
        }
        fn facts_changed(&self, peer: DeviceId) {
            self.facts.lock().unwrap().push(peer);
        }
    }

    fn recording_sink() -> (
        Arc<RecordingSink>,
        tokio::sync::mpsc::UnboundedReceiver<(Boot, v2::Message)>,
    ) {
        let (messages, received) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(RecordingSink {
                messages,
                link_changes: Default::default(),
                closed: Mutex::new(Vec::new()),
                emergencies: Default::default(),
                facts: Mutex::new(Vec::new()),
            }),
            received,
        )
    }

    fn held_sync() -> v2::Message {
        use oink_protocol::v2::schema::List;
        v2::Message::InputHeldSync(
            dto::InputHeldSync::from_raw(
                [
                    (1, List::<_, 256>(vec![5u32]).write()),
                    (2, List::<dto::MouseButton, 32>(vec![]).write()),
                ]
                .into(),
            )
            .unwrap(),
        )
    }

    /// The next input message that is not part of an I0 opening (each
    /// outbound I0 starts with the core's opening records).
    async fn next_input(
        received: &mut tokio::sync::mpsc::UnboundedReceiver<(Boot, v2::Message)>,
        what: &str,
    ) -> (Boot, v2::Message) {
        loop {
            let (peer, message) = tokio::time::timeout(std::time::Duration::from_secs(2), received.recv())
                .await
                .unwrap_or_else(|_| panic!("{what}"))
                .unwrap();
            if !matches!(
                message,
                v2::Message::InputCapability(_)
                    | v2::Message::ControlFeel(_)
                    | v2::Message::ControlFocusState(_)
                    | v2::Message::ControlGameMode(_)
            ) {
                return (peer, message);
            }
        }
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !done() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}"));
    }

    /// Installed 0.4.0-inhouse.4 (2026-11-12): the input router took a peer's
    /// screens only when its I0 attached, which is usually before the peer's
    /// DeviceFacts arrive. The input owner is told whenever a peer's facts
    /// arrive or change, and they are readable then.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_input_owner_hears_when_a_peers_facts_arrive() {
        let privileged = MobileProcess::App;
        let (left_state, right_state) = shared_state_bytes();
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let left = start_member([11; 32], 1, left_state, left_dir.path(), privileged).await;
        let right = start_member([12; 32], 2, right_state, right_dir.path(), privileged).await;
        let (left_sink, _) = recording_sink();
        left.set_input_sink(Some(left_sink.clone()));
        let right_id = right.state.identity.id();
        left.dial(right_id, vec![right.local_addr()]).unwrap();
        until("the peer's facts reached the input owner", || {
            left_sink.facts.lock().unwrap().contains(&right_id)
        })
        .await;
        assert!(
            left.peer_facts(right_id).is_some(),
            "the facts it was told about are readable"
        );
        left.stop().await;
        right.stop().await;
    }

    /// PAUSE-AUDIT 1: a desktop that pauses or resumes input retires the I0
    /// the phone writes (Cancelled) and the one it writes itself. The phone
    /// reopens its I0 at once with the next ordinal and the opening goes out
    /// again; the closed inbound I0 releases what that desktop held. The link
    /// stays up throughout (no opening timeout, no reconnect).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retired_input_stream_reopens_at_once_without_dropping_the_link() {
        use oink_network::link::StreamDir;
        let privileged = MobileProcess::App;
        let (left_state, right_state) = shared_state_bytes();
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let left = start_member([11; 32], 1, left_state, left_dir.path(), privileged).await;
        let right = start_member([12; 32], 2, right_state, right_dir.path(), privileged).await;
        let (left_sink, _) = recording_sink();
        let (right_sink, mut received) = recording_sink();
        left.set_input_sink(Some(left_sink.clone()));
        right.set_input_sink(Some(right_sink.clone()));
        let left_id = left.state.identity.id();
        let right_id = right.state.identity.id();
        left.dial(right_id, vec![right.local_addr()]).unwrap();
        until("both I0 open", || {
            left.outbound_stream(right_id, StreamKind::Input).is_some()
                && right.outbound_stream(left_id, StreamKind::Input).is_some()
        })
        .await;
        let link = left.links().by_peer(right_id).unwrap().link;
        let right_link = right.links().by_peer(left_id).unwrap().link;
        let first = left.outbound_stream(right_id, StreamKind::Input).unwrap();
        assert_eq!(first.tag.ordinal, 1);
        // A QUIC stream reaches its reader with its first message.
        left.send_message(right_id, held_sync()).unwrap();
        next_input(&mut received, "input crossed the first stream").await;
        right.send_message(left_id, held_sync()).unwrap();
        let opened = left_sink.link_changes.load(Ordering::SeqCst);

        // The desktop retires the phone's I0 (its reader).
        right.state.links.close_stream(
            StreamId {
                link: right_link,
                dir: StreamDir::In,
                tag: first.tag,
            },
            oink_protocol::frame::StreamCode::Cancelled,
        );
        until("the phone reopened its I0", || {
            left.outbound_stream(right_id, StreamKind::Input)
                .is_some_and(|stream| stream.tag.ordinal == 2)
        })
        .await;
        assert!(
            left_sink.link_changes.load(Ordering::SeqCst) > opened,
            "the input owner was told to send the opening again"
        );
        left.send_message(right_id, held_sync()).unwrap();
        let (peer, message) = next_input(&mut received, "input crossed the reopened stream").await;
        assert_eq!(peer, left.boot());
        assert!(matches!(message, v2::Message::InputHeldSync(_)));

        // The desktop retires the I0 it writes to the phone.
        let theirs = right.outbound_stream(left_id, StreamKind::Input).unwrap();
        right
            .state
            .links
            .close_stream(theirs, oink_protocol::frame::StreamCode::Cancelled);
        until("the phone released what the desktop held", || {
            left_sink.closed.lock().unwrap().contains(&right_id)
        })
        .await;
        assert_eq!(left.links().by_peer(right_id).unwrap().link, link, "the link stayed up");
        assert_eq!(right.links().by_peer(left_id).unwrap().link, right_link);
        left.stop().await;
        right.stop().await;
    }

    /// PAUSE-AUDIT 1 / B0: a desktop that paused clipboard sharing refuses the
    /// phone's B0 with `Paused`. Requests fail at once with that reason; the
    /// phone does not retry until the desktop shows it resumed (new facts),
    /// then B0 is back on the same link.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_paused_clipboard_stream_fails_requests_at_once_and_returns_on_resume() {
        use crate::MobileV2Link;
        use oink_network::link::StreamDir;
        let (left_state, right_state) = shared_state_bytes();
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let none = MobileProcess::Share;
        let left = start_member([11; 32], 1, left_state, left_dir.path(), none).await;
        let right = start_member([12; 32], 2, right_state, right_dir.path(), none).await;
        let left_id = left.state.identity.id();
        let right_id = right.state.identity.id();
        let store =
            crate::TransferStore::open(left_dir.path().join("t.sqlite"), left_dir.path().join("files")).unwrap();
        let transfer = Arc::new(crate::MobileV2Session::new(
            store,
            crate::MobileV2Identity::from_parts(left_id.0, left.group_id().0, [1; 16]),
            left.link.clone(),
        ));
        left.link.attach_clipboard(&transfer);
        transfer.set_peers(vec![crate::Peer {
            id: hex(right_id),
            kind: crate::PeerKind::Pc,
            state: crate::PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        }]);
        let right_store =
            crate::TransferStore::open(right_dir.path().join("t.sqlite"), right_dir.path().join("files")).unwrap();
        let right_transfer = Arc::new(crate::MobileV2Session::new(
            right_store,
            crate::MobileV2Identity::from_parts(right_id.0, right.group_id().0, [2; 16]),
            right.link.clone(),
        ));
        right.link.attach_clipboard(&right_transfer);
        let (seen, mut right_events) = tokio::sync::mpsc::unbounded_channel();
        right_transfer.set_event_sink(Arc::new(move |event| {
            let _ = seen.send(event);
        }));
        left.dial(right_id, vec![right.local_addr()]).unwrap();
        until("B0 open", || {
            left.outbound_stream(right_id, StreamKind::Clipboard).is_some()
        })
        .await;
        let link = left.links().by_peer(right_id).unwrap().link;
        let right_link = right.links().by_peer(left_id).unwrap().link;
        let first = left.outbound_stream(right_id, StreamKind::Clipboard).unwrap();
        // Use a passive C1 message for the stream lifecycle probe. CurrentQuery
        // now has a real metadata response, which can open the reverse B0 while
        // this fixture deliberately refuses only its incoming direction.
        let frame = v2::frame::encode(
            &v2::Message::ClipboardShareCancel(
                dto::ClipboardShareCancel::from_raw([(1, v2::RawValue::Bytes(vec![9; 16]))].into()).unwrap(),
            ),
            v2::limits::RELIABLE_FRAME,
        )
        .unwrap();
        // A QUIC stream reaches its reader with its first message.
        left.link.send_frame(&hex(right_id), frame.clone()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), right_events.recv())
            .await
            .expect("the first B0 frame arrived")
            .unwrap();
        right.state.links.close_stream(
            StreamId {
                link: right_link,
                dir: StreamDir::In,
                tag: first.tag,
            },
            oink_protocol::frame::StreamCode::Paused,
        );
        until("B0 refused as paused", || {
            matches!(
                left.link.send_frame(&hex(right_id), frame.clone()),
                Err(crate::MobileV2TransportError::PeerPaused(_))
            )
        })
        .await;
        assert_eq!(
            transfer.receive_current_text(&hex(right_id)).await,
            Err(crate::MobileV2TransportError::PeerPaused(hex(right_id))),
            "a request to the paused desktop fails at once with its reason"
        );
        assert!(
            left.outbound_stream(right_id, StreamKind::Clipboard).is_none(),
            "no retry while paused"
        );

        // The desktop resumes; its facts change.
        right.rename_local("resumed".into()).unwrap();
        until("B0 reopened after the resume", || {
            left.outbound_stream(right_id, StreamKind::Clipboard)
                .is_some_and(|stream| stream.tag.ordinal == 2)
        })
        .await;
        left.link.send_frame(&hex(right_id), frame).unwrap();
        assert_eq!(left.links().by_peer(right_id).unwrap().link, link, "the link stayed up");
        left.stop().await;
        right.stop().await;
    }

    /// The phone reopens its I0 while the desktop is paused; the desktop
    /// refuses it as `Paused`. That refusal travels on its own stream and can
    /// reach the phone after the desktop's resumed Facts. Applied to the
    /// current I0 it parked it as "peer paused" with nothing left to wake it
    /// (the phone has no opening timeout) until some later Facts change.
    /// The events are delivered to the phone's stream owner in that order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_paused_refusal_arriving_after_the_resume_does_not_park_the_phone_i0() {
        late_paused_refusal(StreamKind::Input, MobileProcess::App).await;
    }

    /// The same order for B0: a paused desktop refuses B0 too, and a parked
    /// B0 fails every clipboard request to it with `Paused`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_paused_refusal_arriving_after_the_resume_does_not_park_the_phone_b0() {
        late_paused_refusal(StreamKind::Clipboard, MobileProcess::Share).await;
    }

    /// Link events, backoff timers and host calls each decide a step under
    /// the machines lock and carry it out after it, on their own threads. Two
    /// reopens carried out in the other order than decided must still leave
    /// the machine's current stream installed: the older open replaced it and
    /// the I0 then wrote on a stream the machine had retired.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn steps_carried_out_out_of_order_keep_the_current_stream_installed() {
        let (left_state, right_state) = shared_state_bytes();
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let left = start_member([11; 32], 1, left_state, left_dir.path(), MobileProcess::App).await;
        let right = start_member([12; 32], 2, right_state, right_dir.path(), MobileProcess::App).await;
        let (left_sink, _) = recording_sink();
        let (right_sink, _right_received) = recording_sink();
        left.set_input_sink(Some(left_sink));
        right.set_input_sink(Some(right_sink));
        let right_id = right.state.identity.id();
        left.dial(right_id, vec![right.local_addr()]).unwrap();
        until("the phone's I0 open", || {
            left.outbound_stream(right_id, StreamKind::Input).is_some()
        })
        .await;
        let first = left.outbound_stream(right_id, StreamKind::Input).unwrap();
        let older = outbound::reopen_decided(&left.state, StreamKind::Input);
        let newer = outbound::reopen_decided(&left.state, StreamKind::Input);
        newer();
        older();
        assert_eq!(
            left.outbound_stream(right_id, StreamKind::Input)
                .map(|stream| stream.tag.ordinal),
            Some(first.tag.ordinal + 2),
            "the older reopen replaced the current I0's sender"
        );
        left.stop().await;
        right.stop().await;
    }

    async fn late_paused_refusal(kind: StreamKind, process: MobileProcess) {
        use oink_network::link::StreamClosed;
        let (left_state, right_state) = shared_state_bytes();
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let left = start_member([11; 32], 1, left_state, left_dir.path(), process).await;
        let right = start_member([12; 32], 2, right_state, right_dir.path(), process).await;
        let (left_sink, _) = recording_sink();
        let (right_sink, _right_received) = recording_sink();
        left.set_input_sink(Some(left_sink));
        right.set_input_sink(Some(right_sink));
        let right_id = right.state.identity.id();
        left.dial(right_id, vec![right.local_addr()]).unwrap();
        until("the phone's stream open", || {
            left.outbound_stream(right_id, kind).is_some()
        })
        .await;
        let link = left.links().by_peer(right_id).unwrap().link;
        // The peer's own Facts carry each pause, so Facts that the phone's
        // link task handles on its own (the first ones, or any later change)
        // agree with the order the test delivers. The test still hands the
        // stream owner each pause change itself, so the step does not depend
        // on when that task runs; a second delivery of the same Facts changes
        // nothing.
        let pause = |paused: bool, revision: u64| {
            right
                .edit_local_facts(|raw| {
                    raw.insert(
                        19,
                        dto::LocalPauseState::from_raw(
                            [
                                (1, paused.write()),
                                (
                                    2,
                                    if paused {
                                        dto::PauseReason::LocalUser
                                    } else {
                                        dto::PauseReason::None
                                    }
                                    .write(),
                                ),
                                (3, revision.write()),
                            ]
                            .into(),
                        )?
                        .write(),
                    );
                    Ok(())
                })
                .unwrap();
        };
        let seen = |paused: bool| {
            left.state
                .controls
                .lock()
                .unwrap()
                .get(&link)
                .and_then(|session| session.facts.as_ref())
                .and_then(|facts| facts.pause().ok().flatten())
                .is_some_and(|pause| pause.paused() == Ok(paused))
        };
        // The desktop's Facts show it paused.
        pause(true, 2);
        until("the phone holds the paused Facts", || seen(true)).await;
        outbound::peer_facts(&left.state, link, true);
        // The phone reopens its stream during the pause (as after any close).
        outbound::reopen(&left.state, kind);
        let refused = left.outbound_stream(right_id, kind).unwrap();
        // The desktop's resumed Facts, then its refusal of that stream.
        pause(false, 3);
        until("the phone holds the resumed Facts", || seen(false)).await;
        outbound::peer_facts(&left.state, link, false);
        outbound::closed(
            &left.state,
            refused,
            StreamClosed::Peer(oink_protocol::frame::StreamCode::Paused),
        );
        assert!(
            !outbound::paused(&left.state, right_id, kind),
            "a Paused refusal that arrived after the resume parked the phone's {kind:?}"
        );
        // The replacement may be carried out by the link task's own delivery
        // of the resumed Facts, on another thread.
        until("the phone's stream is open on a newer stream", || {
            left.outbound_stream(right_id, kind)
                .is_some_and(|stream| stream.tag.ordinal > refused.tag.ordinal)
        })
        .await;
        left.stop().await;
        right.stop().await;
    }

    /// H10: I0 reaches the input owner from the link's receive task. Nothing
    /// is queued for a host to poll.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_reaches_the_sink_without_a_host_poll() {
        use oink_protocol::v2::schema::List;
        let privileged = MobileProcess::App;
        let (left_state, right_state) = shared_state_bytes();
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let left = start_member([11; 32], 1, left_state, left_dir.path(), privileged).await;
        let right = start_member([12; 32], 2, right_state, right_dir.path(), privileged).await;
        let (sink, mut received) = recording_sink();
        right.set_input_sink(Some(sink.clone()));
        let right_id = right.state.identity.id();
        left.dial(right_id, vec![right.local_addr()]).unwrap();
        let held = v2::Message::InputHeldSync(
            dto::InputHeldSync::from_raw(
                [
                    (1, List::<_, 256>(vec![5u32]).write()),
                    (2, List::<dto::MouseButton, 32>(vec![]).write()),
                ]
                .into(),
            )
            .unwrap(),
        );
        let mut changes = left.subscribe_changes();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while left.send_message(right_id, held.clone()).is_err() {
                changes.changed().await.unwrap();
            }
        })
        .await
        .expect("the I0 stream opened on the ready link");
        let (peer, message) = next_input(&mut received, "I0 message delivered").await;
        assert_eq!(peer, left.boot());
        assert!(matches!(message, v2::Message::InputHeldSync(_)));
        assert!(sink.link_changes.load(Ordering::SeqCst) >= 1);
        left.stop().await;
        right.stop().await;
    }

    /// H11: the first connect round lasts while discovery has its window and
    /// while a dial started in it runs; a ready link ends that dial.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_first_connect_round_settles_after_discovery_and_its_dials() {
        let (left_state, right_state) = shared_state_bytes();
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let none = MobileProcess::Share;
        let left = start_member([11; 32], 1, left_state, left_dir.path(), none).await;
        let right = start_member([12; 32], 2, right_state, right_dir.path(), none).await;
        let right_id = right.state.identity.id();
        let mut changes = left.subscribe_changes();
        assert!(!left.connect_round_settled());
        left.dial(right_id, vec![right.local_addr()]).unwrap();
        let started = Instant::now();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !left.connect_round_settled() {
                changes.changed().await.unwrap();
            }
        })
        .await
        .expect("the round settled");
        assert!(started.elapsed() >= FIRST_ROUND_DISCOVERY - std::time::Duration::from_secs(1));
        assert!(left.links().ready(right_id), "the round's dial ended with a ready link");
        left.stop().await;
        right.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_phone_alone_in_its_group_has_no_round_to_wait_for() {
        let dir = tempfile::tempdir().unwrap();
        let alone = start_member(
            [11; 32],
            1,
            state_bytes([11; 32], "alone"),
            dir.path(),
            MobileProcess::Share,
        )
        .await;
        assert!(alone.connect_round_settled());
        alone.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn c0_greeting_and_signed_admission_gate_clipboard_stream() {
        let left_identity = oink_network::identity::Identity::from_seed([11; 32]);
        let right_identity = oink_network::identity::Identity::from_seed([12; 32]);
        let (left_state, right_state) = shared_state_bytes();
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let left = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: [11; 32],
            boot: Boot {
                device_id: left_identity.id(),
                nonce: [1; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(left_state),
            group_state_path: left_dir.path().join("group-state.v2"),
            pairing_journal_path: left_dir.path().join("pairing-recovery.cbor"),
            device_name: "left".into(),
            os: "test".into(),
            form_factor: None,
            trusted_peers: vec![],
            trust_path: None,
            process: MobileProcess::App,
        })
        .await
        .unwrap();
        let right = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: [12; 32],
            boot: Boot {
                device_id: right_identity.id(),
                nonce: [2; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(right_state),
            group_state_path: right_dir.path().join("group-state.v2"),
            pairing_journal_path: right_dir.path().join("pairing-recovery.cbor"),
            device_name: "right".into(),
            os: "test".into(),
            form_factor: None,
            trusted_peers: vec![],
            trust_path: None,
            process: MobileProcess::App,
        })
        .await
        .unwrap();
        let frame = v2::frame::encode(
            &crate::v2_session::current_query(
                &crate::MobileV2Identity::from_parts(left_identity.id().0, left.group_id().0, [1; 16]),
                [9; 16],
                4,
            )
            .unwrap(),
            v2::limits::RELIABLE_FRAME,
        )
        .unwrap();
        let transfer = |node: &MobileNetwork, root: &Path, identity: DeviceId, nonce: [u8; 16]| {
            let store = crate::TransferStore::open(root.join("transfer.sqlite"), root.join("files")).unwrap();
            let group = node.link.group_id().unwrap();
            let owner = Arc::new(crate::MobileV2Session::new(
                store,
                crate::MobileV2Identity::from_parts(identity.0, group.0, nonce),
                node.link.clone(),
            ));
            node.link.attach_clipboard(&owner);
            let (sender, events) = tokio::sync::mpsc::unbounded_channel();
            owner.set_event_sink(Arc::new(move |event| {
                let _ = sender.send(event);
            }));
            (owner, events)
        };
        let (left_transfer, mut left_events) = transfer(&left, left_dir.path(), left_identity.id(), [1; 16]);
        let (right_transfer, mut right_events) = transfer(&right, right_dir.path(), right_identity.id(), [2; 16]);
        left.dial(right_identity.id(), vec![right.local_addr()]).unwrap();
        let mut sent = false;
        for _ in 0..400 {
            if left.link.send_frame(&hex(right_identity.id()), frame.clone()).is_ok() {
                sent = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(sent, "business stream opened before C0 admission");
        // The frame reaches the transfer owner directly, without a queue.
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(crate::TransferEvent::MessageReceived { message, .. }) = right_events.recv().await
                    && message == "ClipboardCurrentQuery"
                {
                    break;
                }
            }
        })
        .await
        .expect("admitted C0 link did not deliver v2 business frame");
        let facts = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(facts) = right.peer_facts(left_identity.id()) {
                    break facts;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("mobile facts did not arrive on admitted C0");
        assert_eq!(facts.device_kind(), Ok(dto::DeviceKind::Mobile));
        assert_eq!(facts.group().unwrap().0, right.group_id().0);
        assert_eq!(
            oink_network::wire_identity::read_boot(&facts.boot().unwrap())
                .unwrap()
                .nonce,
            [1; 16]
        );
        left_transfer.set_peers(vec![crate::Peer {
            id: hex(right_identity.id()),
            kind: crate::PeerKind::Mobile,
            state: crate::PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        }]);
        right_transfer.set_peers(vec![crate::Peer {
            id: hex(left_identity.id()),
            kind: crate::PeerKind::Mobile,
            state: crate::PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        }]);
        right_transfer.set_history_foreground(true).unwrap();
        left_transfer
            .send_share(
                crate::SharePayload {
                    sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
                    items: vec![crate::SendItem::Text {
                        text: "real Low clipboard sample".repeat(900),
                    }],
                    origin: crate::SendOrigin::TransferPage,
                    operation_id: [7; 16],
                    message_id: [8; 16],
                },
                crate::ProxyChoice::Automatic,
            )
            .unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            let mut delivered = false;
            let mut received = false;
            loop {
                tokio::select! {
                    Some(event) = left_events.recv() => match event {
                        crate::TransferEvent::DeliveryEvent { status, .. } if status == "Complete" => delivered=true,
                        crate::TransferEvent::Error { detail, .. } => return Err(detail),
                        _ => {}
                    },
                    Some(event) = right_events.recv() => match event {
                        crate::TransferEvent::Error { detail, .. } => return Err(detail),
                        crate::TransferEvent::Announcement {..}=>received=true,
                        _=>{},
                    },
                }
                if delivered && received {
                    return Ok(());
                }
            }
        })
        .await
        .unwrap();
        outcome.unwrap();
        let store =
            crate::TransferStore::open(right_dir.path().join("transfer.sqlite"), right_dir.path().join("files"))
                .unwrap();
        let page = store.page(None, 5).unwrap();
        assert_eq!(
            page.messages[0].text.as_deref(),
            Some("real Low clipboard sample".repeat(900).as_str())
        );
        left.rename_local("renamed sender".into()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while right
                .peer_facts(left_identity.id())
                .and_then(|facts| facts.name().ok())
                .is_none_or(|name| name.0 != "renamed sender")
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("authenticated rename reached the peer");
        assert_eq!(left.advertisement().name, "renamed sender");

        // Review M37 / plan 4: the privileged helper becoming ready (or the
        // screen locking) is a fact, not a capability change. The link stays;
        // the peer gets a fresh I0 whose opening says the phone is controllable.
        let link_before = left.links().by_peer(right_identity.id()).map(|entry| entry.link);
        assert!(link_before.is_some());
        let (sink, mut input) = recording_sink();
        right.set_input_sink(Some(sink));
        left.update_platform_facts(MobilePlatformFacts {
            microphone_send: true,
            speaker_receive: true,
            speaker_send: true,
            privileged_input_ready: true,
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some((peer, message)) = input.recv().await {
                if peer.device_id == left_identity.id()
                    && matches!(&message, v2::Message::InputCapability(capability)
                        if capability.controllable() == Ok(true))
                {
                    break;
                }
            }
        })
        .await
        .expect("the reopened I0 announced controllable");
        let link_after = left
            .links()
            .by_peer(right_identity.id())
            .map(|entry| (entry.link, entry.state));
        assert_eq!(link_after, link_before.map(|link| (link, LinkState::Ready)));
        assert!(
            left.links()
                .by_peer(right_identity.id())
                .is_some_and(|entry| entry.agreement.contains("oink.input/2"))
        );

        // The privileged-helper state is a live fact: set, peers see it; cleared, it is gone.
        let privileged_on_peer = || {
            right
                .peer_facts(left_identity.id())
                .and_then(|facts| facts.android_privileged().ok())
        };
        assert_eq!(privileged_on_peer(), Some(false));
        assert!(left.set_android_privileged(true).unwrap());
        assert!(
            !left.set_android_privileged(true).unwrap(),
            "unchanged state is not republished"
        );
        assert!(left.local_android_privileged());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while privileged_on_peer() != Some(true) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("privileged state reached the peer");
        assert!(left.set_android_privileged(false).unwrap());
        assert!(!left.local_android_privileged());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while privileged_on_peer() != Some(false) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cleared privileged state reached the peer");

        let previous_group = right.group_id();
        left.remove_member(right_identity.id()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while right.group_id() == previous_group {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("signed revocation reached the removed mobile");
        assert_eq!(left.group_id(), previous_group);
        assert_eq!(left.members()[0].device, left_identity.id());
        assert_eq!(right.members()[0].device, right_identity.id());
        assert_eq!(left.members().len(), 1);
        assert_eq!(right.members().len(), 1);
        assert_eq!(
            right.state_bytes().unwrap(),
            std::fs::read(right_dir.path().join("group-state.v2")).unwrap()
        );

        left.stop().await;
        right.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn config_pair_pake_merge_and_admission_persist_a_trusted_roster() {
        let left_identity = oink_network::identity::Identity::from_seed([21; 32]);
        let right_identity = oink_network::identity::Identity::from_seed([22; 32]);
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let left_state = state_bytes([21; 32], "left");
        let right_state = state_bytes([22; 32], "right");
        let right_anchor = State::from_wire(
            dto::GroupState::from_raw(v2::cbor::decode_map(&right_state, v2::limits::CONFIG_STATE).unwrap()).unwrap(),
        )
        .unwrap()
        .roster()
        .anchor();
        let left = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: [21; 32],
            boot: Boot {
                device_id: left_identity.id(),
                nonce: [3; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(left_state),
            group_state_path: left_dir.path().join("group-state.v2"),
            pairing_journal_path: left_dir.path().join("pairing-recovery.cbor"),
            device_name: "left".into(),
            os: "test".into(),
            form_factor: None,
            trusted_peers: vec![],
            trust_path: None,
            process: MobileProcess::App,
        })
        .await
        .unwrap();
        let right = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: [22; 32],
            boot: Boot {
                device_id: right_identity.id(),
                nonce: [4; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(right_state),
            group_state_path: right_dir.path().join("group-state.v2"),
            pairing_journal_path: right_dir.path().join("pairing-recovery.cbor"),
            device_name: "right".into(),
            os: "test".into(),
            form_factor: None,
            trusted_peers: vec![],
            trust_path: None,
            process: MobileProcess::App,
        })
        .await
        .unwrap();
        left.dial(right_identity.id(), vec![right.local_addr()]).unwrap();
        let mut key = None;
        let mut last_error = String::new();
        for _ in 0..400 {
            match left.start_pairing_with_anchor(right_identity.id(), right_anchor) {
                Ok(value) => {
                    key = Some(value);
                    break;
                }
                Err(error) => last_error = error.to_string(),
            }
            let _ = left.dial(right_identity.id(), vec![right.local_addr()]);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let key = key.unwrap_or_else(|| panic!("C0 did not expose the authenticated peer for pairing: {last_error}"));
        eprintln!("start_pairing key={key}");
        // The request waits in the phone's request list until the user takes
        // it (user rule 2026-11-11); nothing accepts it on its own.
        let code = left.pairing_code(&key).unwrap();
        let mut request = None;
        for _ in 0..2000 {
            if let Some(view) = right
                .pairing_snapshot()
                .unwrap()
                .requests
                .into_iter()
                .find(|view| pairing::key(view.id) == key)
            {
                request = Some(view);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let request = request.expect("the request reaches the phone's request list");
        assert_eq!(request.phase, pairing::Phase::Requested);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let waiting = right.pairing_snapshot().unwrap();
        assert!(
            waiting.requests.iter().any(|view| pairing::key(view.id) == key)
                && !waiting.sessions.iter().any(|view| pairing::key(view.id) == key),
            "the request stays a request until accepted"
        );
        right.accept_pairing(&key, request.revision).unwrap();
        let accepted = right
            .pairing_snapshot()
            .unwrap()
            .sessions
            .into_iter()
            .find(|view| pairing::key(view.id) == key && view.phase == pairing::Phase::EnterCode)
            .expect("accepting shows code entry");
        right.submit_pairing_code(&key, accepted.revision, 1, &code).unwrap();
        let mut completed = false;
        for _ in 0..2000 {
            let left_done = left
                .pairing_snapshot()
                .unwrap()
                .sessions
                .iter()
                .any(|view| pairing::key(view.id) == key && view.phase == pairing::Phase::Completed);
            let right_done = right
                .pairing_snapshot()
                .unwrap()
                .sessions
                .iter()
                .any(|view| pairing::key(view.id) == key && view.phase == pairing::Phase::Completed);
            if left_done && right_done {
                completed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(completed, "ConfigPair did not reach durable completion");
        let left_snapshot = left.pairing_snapshot().unwrap();
        let right_snapshot = right.pairing_snapshot().unwrap();
        eprintln!(
            "PairingSnapshot completed left={:?} right={:?}",
            left_snapshot.sessions.iter().map(|view| view.phase).collect::<Vec<_>>(),
            right_snapshot
                .sessions
                .iter()
                .map(|view| view.phase)
                .collect::<Vec<_>>()
        );
        assert!(
            left_snapshot
                .sessions
                .iter()
                .any(|view| pairing::key(view.id) == key && view.phase == pairing::Phase::Completed)
        );
        assert!(
            right_snapshot
                .sessions
                .iter()
                .any(|view| pairing::key(view.id) == key && view.phase == pairing::Phase::Completed)
        );
        assert!(left.admitted_peers().unwrap().contains(&right_identity.id()));
        assert!(right.admitted_peers().unwrap().contains(&left_identity.id()));
        let left_persisted = State::from_wire(
            dto::GroupState::from_raw(
                v2::cbor::decode_map(
                    &std::fs::read(left_dir.path().join("group-state.v2")).unwrap(),
                    v2::limits::CONFIG_STATE,
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let right_persisted = State::from_wire(
            dto::GroupState::from_raw(
                v2::cbor::decode_map(
                    &std::fs::read(right_dir.path().join("group-state.v2")).unwrap(),
                    v2::limits::CONFIG_STATE,
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        eprintln!(
            "roster persistence left_contains_right={} right_contains_left={}",
            left_persisted.roster().contains(right_identity.id()),
            right_persisted.roster().contains(left_identity.id())
        );
        assert!(left_persisted.roster().contains(right_identity.id()));
        assert!(right_persisted.roster().contains(left_identity.id()));
        let paired_group = left.group_id();
        right.leave_group().unwrap();
        assert_ne!(right.group_id(), paired_group);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while left.members().iter().any(|member| member.device == right_identity.id()) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("signed local departure reached the remaining peer");
        assert_eq!(left.group_id(), paired_group);
        assert_eq!(right.members().len(), 1);
        assert_eq!(right.members()[0].device, right_identity.id());
        left.stop().await;
        right.stop().await;
    }

    /// Route selection is baseline core/2 on phones too (protocol-v2 11.3):
    /// a phone publishes its endpoints, keeps standby connections, answers
    /// probes and takes part in the direct-to-direct cut-over.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mobile_core_keeps_standby_routes_and_follows_a_route_switch() {
        let left_identity = oink_network::identity::Identity::from_seed([11; 32]);
        let right_identity = oink_network::identity::Identity::from_seed([12; 32]);
        let (left_state, right_state) = shared_state_bytes();
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let start = |seed: u8, nonce: u8, state: Vec<u8>, dir: &Path, name: &str| {
            let identity = oink_network::identity::Identity::from_seed([seed; 32]);
            MobileNetwork::start(MobileNetworkConfig {
                identity_seed: [seed; 32],
                boot: Boot {
                    device_id: identity.id(),
                    nonce: [nonce; 16],
                },
                bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                group_state: Some(state),
                group_state_path: dir.join("group-state.v2"),
                pairing_journal_path: dir.join("pairing-recovery.cbor"),
                device_name: name.into(),
                os: "test".into(),
                form_factor: None,
                trusted_peers: vec![],
                trust_path: None,
                process: MobileProcess::App,
            })
        };
        let left = start(11, 1, left_state, left_dir.path(), "left").await.unwrap();
        let right = start(12, 2, right_state, right_dir.path(), "right").await.unwrap();
        left.dial(right_identity.id(), vec![right.local_addr()]).unwrap();
        let facts = tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                if let Some(facts) = right.peer_facts(left_identity.id()) {
                    break facts;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("admitted facts");
        assert_eq!(facts::route_addresses(&facts), vec![left.local_addr()]);
        // The right phone dials the left one's published endpoint: a standby
        // next to the left phone's current dial, probed until reachable.
        let reachable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(route) = right.links().by_peer(left_identity.id()).and_then(|e| e.route.clone())
                    && route.rows.iter().any(|row| {
                        row.address == left.local_addr() && row.state == oink_network::link::RouteRowState::Reachable
                    })
                {
                    break route;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("standby route measured");
        assert_eq!(
            reachable.direction,
            Direction::Inbound,
            "the left phone dialed the link"
        );
        assert!(reachable.can_replace);
        let link = right.links().by_peer(left_identity.id()).unwrap().link;
        // A route choice (from a PC in practice) moves the link to this end's dial.
        assert!(right.state.links.set_route(
            left_identity.id(),
            oink_network::link::RouteChoice::Pin(std::net::Ipv4Addr::LOCALHOST)
        ));
        let switched = tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                if let Some(route) = left.links().by_peer(right_identity.id()).and_then(|e| e.route.clone())
                    && route.direction == Direction::Inbound
                    && route.pin == Some(oink_network::link::RouteRecord::Incoming(std::net::Ipv4Addr::LOCALHOST))
                {
                    break route;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the other phone follows the switch and the pin");
        assert_eq!(switched.pin_set_by, Some(right_identity.id()));
        assert_eq!(
            right.links().by_peer(left_identity.id()).unwrap().link,
            link,
            "same logical link"
        );
        assert!(right.peer_facts(left_identity.id()).is_some(), "C0 survives the switch");
        left.stop().await;
        right.stop().await;
    }

    /// A minimal instance of a registered DTO built from its schema: required
    /// fields only, the smallest value of each shape.
    fn sample(shape: &str, depth: usize) -> Option<RawValue> {
        use oink_protocol::v2::registry;
        let shape: String = shape.chars().filter(|c| !c.is_whitespace()).collect();
        if depth > 12 {
            return None;
        }
        let inner = |prefix: &str| shape.strip_prefix(prefix).and_then(|rest| rest.strip_suffix('>'));
        let size = |text: &str| text.rsplit(',').next().and_then(|n| n.parse::<usize>().ok());
        Some(match shape.as_str() {
            "u8" | "u16" | "u32" | "u64" | "usize" => RawValue::Uint(1),
            "i8" | "i16" | "i32" | "i64" => RawValue::Uint(1),
            "bool" => RawValue::Bool(false),
            "f32" => RawValue::F32(0.0),
            "f64" => RawValue::F64(0.0),
            "Id16" => RawValue::Bytes(vec![1; 16]),
            "Hash32" => RawValue::Bytes(vec![1; 32]),
            "RawMap" => RawValue::Map(RawMap::new()),
            // Identity values carry their own validation (algorithm, lengths).
            "DeviceId" => oink_network::wire_identity::device(DeviceId([7; 32])).write(),
            "Boot" => oink_network::wire_identity::boot(Boot {
                device_id: DeviceId([7; 32]),
                nonce: [1; 16],
            })
            .write(),
            "PairId" => oink_network::wire_identity::pair(PairId {
                initiator_boot: Boot {
                    device_id: DeviceId([7; 32]),
                    nonce: [1; 16],
                },
                random: [2; 16],
            })
            .write(),
            "RawValue" => RawValue::Null,
            _ if shape.starts_with("Option<") => RawValue::Null,
            _ if shape.starts_with("List<") => RawValue::Array(Vec::new()),
            _ if shape.starts_with("FixedBytes<") => RawValue::Bytes(vec![1; size(inner("FixedBytes<")?)?]),
            _ if shape.starts_with("Bytes<") => RawValue::Bytes(vec![1]),
            _ if shape.starts_with("Text<") || shape.starts_with("Ascii<") => RawValue::Text("x".into()),
            _ if shape.starts_with("OpenEnvelope<") => {
                let kind = sample(inner("OpenEnvelope<")?, depth + 1)?;
                RawValue::Map([(1, kind), (2, RawValue::Map(RawMap::new()))].into())
            }
            _ => {
                if let Some((_, values)) = registry_enums().iter().find(|(name, _)| *name == shape) {
                    return Some(RawValue::Uint(u64::from(values.first()?.0)));
                }
                let schema = registry::schema_registry().schemas.iter().find(|s| s.name == shape)?;
                return sample_map(schema, depth + 1).map(RawValue::Map);
            }
        })
    }

    fn registry_enums() -> &'static [(&'static str, &'static [(u32, &'static str)])] {
        oink_protocol::v2::registry::schema_registry().enums
    }

    fn sample_map(schema: &oink_protocol::v2::schema::Schema, depth: usize) -> Option<RawMap> {
        let mut raw = RawMap::new();
        for field in schema.fields {
            if field.absence == "required" {
                raw.insert(field.key, sample(field.shape, depth)?);
            }
        }
        Some(raw)
    }

    /// Change plan §2 acceptance: every registered message a phone negotiates
    /// on C0, sent by an admitted member, leaves the C0 up. Pairing, recovery
    /// and admission messages carry signatures and bindings (their violation
    /// closes C0 by rule); they have their own decision tests in
    /// `oink_network::member` and the node scenarios.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_negotiated_c0_message_from_a_member_keeps_the_link() {
        use oink_protocol::v2::registry;
        let left_identity = oink_network::identity::Identity::from_seed([11; 32]);
        let right_identity = oink_network::identity::Identity::from_seed([12; 32]);
        let (left_state, right_state) = shared_state_bytes();
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let start = |seed: u8, nonce: u8, state: Vec<u8>, dir: &Path| {
            let identity = oink_network::identity::Identity::from_seed([seed; 32]);
            MobileNetwork::start(MobileNetworkConfig {
                identity_seed: [seed; 32],
                boot: Boot {
                    device_id: identity.id(),
                    nonce: [nonce; 16],
                },
                bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                group_state: Some(state),
                group_state_path: dir.join("group-state.v2"),
                pairing_journal_path: dir.join("pairing-recovery.cbor"),
                device_name: "phone".into(),
                os: "android".into(),
                form_factor: None,
                trusted_peers: vec![],
                trust_path: None,
                process: MobileProcess::App,
            })
        };
        let left = start(11, 1, left_state, left_dir.path()).await.unwrap();
        let right = start(12, 2, right_state, right_dir.path()).await.unwrap();
        left.dial(right_identity.id(), vec![right.local_addr()]).unwrap();
        let ready = |net: &MobileNetwork, peer: DeviceId| net.peer_facts(peer).is_some();
        for _ in 0..400 {
            if ready(&left, right_identity.id()) && ready(&right, left_identity.id()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(ready(&left, right_identity.id()) && ready(&right, left_identity.id()));
        let link = |net: &MobileNetwork, peer: DeviceId| net.state.peer_links.lock().unwrap().get(&peer).copied();
        let original = link(&right, left_identity.id()).unwrap();
        let agreement = right
            .state
            .links
            .links()
            .by_peer(left_identity.id())
            .unwrap()
            .agreement
            .clone();
        let mut sent = Vec::new();
        let mut unbuildable = Vec::new();
        for entry in registry::MESSAGES {
            let on_c0 = entry.routes.iter().any(|route| {
                route.kind == StreamKind::Config as u16
                    && route.extra_capabilities.iter().all(|cap| agreement.contains(cap))
            });
            if !on_c0 || !entry.capabilities.iter().all(|cap| agreement.contains(cap)) {
                continue;
            }
            let message = registry::message(entry.id)
                .and_then(|entry| sample_map(&entry.schema, 0))
                .and_then(|raw| v2::Message::from_payload(entry.id, raw).ok());
            let Some(message) = message else {
                unbuildable.push(entry.name);
                continue;
            };
            if oink_network::member::handles(&message)
                || matches!(
                    message,
                    v2::Message::ConfigRequest(_)
                        | v2::Message::ConfigReply(_)
                        | v2::Message::ConfigDone(_)
                        | v2::Message::ConfigState(_)
                        | v2::Message::ConfigFacts(_)
                        | v2::Message::ConfigHello(_)
                        | v2::Message::ConfigInfo(_)
                )
            {
                continue;
            }
            post_control(&left.state, right_identity.id(), message).unwrap();
            sent.push(entry.name);
            for _ in 0..20 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            assert_eq!(
                link(&right, left_identity.id()),
                Some(original),
                "{} closed the phone's C0",
                entry.name
            );
        }
        eprintln!("sent to a ready phone C0: {sent:?}; no minimal instance: {unbuildable:?}");
        assert!(sent.contains(&"ConfigEmergencyPause"), "{sent:?}");
        assert!(!sent.is_empty());
        left.stop().await;
        right.stop().await;
    }

    /// Retiring a link withdraws only its business streams; the C0 drains a
    /// signed departure and stays this peer's C0 until the link is lost. The
    /// module side's LinkLost for the retirement must not forget it: a phone
    /// that did would report the desktop gone while the link layer still has
    /// it, and its next dial to that desktop is refused as already linked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retired_c0_stays_the_peers_c0_until_the_link_is_lost() {
        let left_identity = oink_network::identity::Identity::from_seed([11; 32]);
        let right_identity = oink_network::identity::Identity::from_seed([12; 32]);
        let (left_state, right_state) = shared_state_bytes();
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let start = |seed: u8, nonce: u8, state: Vec<u8>, dir: &Path| {
            let identity = oink_network::identity::Identity::from_seed([seed; 32]);
            MobileNetwork::start(MobileNetworkConfig {
                identity_seed: [seed; 32],
                boot: Boot {
                    device_id: identity.id(),
                    nonce: [nonce; 16],
                },
                bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                group_state: Some(state),
                group_state_path: dir.join("group-state.v2"),
                pairing_journal_path: dir.join("pairing-recovery.cbor"),
                device_name: "phone".into(),
                os: "android".into(),
                form_factor: None,
                trusted_peers: vec![],
                trust_path: None,
                process: MobileProcess::App,
            })
        };
        let left = start(11, 1, left_state, left_dir.path()).await.unwrap();
        let right = start(12, 2, right_state, right_dir.path()).await.unwrap();
        left.dial(right_identity.id(), vec![right.local_addr()]).unwrap();
        let peer = right_identity.id();
        for _ in 0..400 {
            if left.peer_facts(peer).is_some() && right.peer_facts(left_identity.id()).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(left.peer_facts(peer).is_some());
        let link = left.state.peer_links.lock().unwrap().get(&peer).copied().unwrap();
        left.state.links.retire_for_departure(link);
        // The retirement reaches the module port at once; give it time to land.
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            assert!(
                left.authenticated_advert(peer).is_some(),
                "the retired C0 was forgotten while the link layer still has it"
            );
        }
        assert!(left.state.links.links().by_peer(peer).is_some());
        left.state
            .links
            .disconnect(link, oink_network::link::Disconnect::LeavingGroup);
        for _ in 0..400 {
            if left.authenticated_advert(peer).is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(left.authenticated_advert(peer).is_none());
        assert!(
            left.state.links.links().by_peer(peer).is_none(),
            "C0 forgotten before the link"
        );
        left.stop().await;
        right.stop().await;
    }

    /// M1: a phone acknowledges an emergency pause addressed to its Boot once
    /// as EnteredPause and then as AlreadySeen; a stale one is ignored.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emergency_pause_is_acknowledged_once_per_event() {
        let identity = oink_network::identity::Identity::from_seed([31; 32]);
        let dir = tempfile::tempdir().unwrap();
        let phone = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: [31; 32],
            boot: Boot {
                device_id: identity.id(),
                nonce: [3; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(state_bytes([31; 32], "phone")),
            group_state_path: dir.path().join("group-state.v2"),
            pairing_journal_path: dir.path().join("pairing-recovery.cbor"),
            device_name: "phone".into(),
            os: "android".into(),
            form_factor: None,
            trusted_peers: vec![],
            trust_path: None,
            process: MobileProcess::App,
        })
        .await
        .unwrap();
        let origin = Boot {
            device_id: oink_network::identity::Identity::from_seed([32; 32]).id(),
            nonce: [4; 16],
        };
        let request = |target: Boot| {
            dto::EmergencyPause::from_raw(
                [
                    (
                        1,
                        RawValue::Map(
                            [
                                (1, oink_network::wire_identity::boot(origin).write()),
                                (2, 7u64.write()),
                            ]
                            .into(),
                        ),
                    ),
                    (2, oink_network::wire_identity::boot(target).write()),
                ]
                .into(),
            )
            .unwrap()
        };
        let link = LinkId(1);
        let outcome = |outs: Vec<oink_network::member::Out>| match &outs[..] {
            [
                oink_network::member::Out::Send {
                    message: v2::Message::ConfigEmergencyPauseAck(ack),
                    ..
                },
            ] => Some(ack.outcome().unwrap()),
            [] => None,
            other => panic!("{other:?}"),
        };
        let target = phone.state.links.boot();
        // The input owner drops and releases once per event (it holds no queue here).
        let (sink, _) = recording_sink();
        phone.set_input_sink(Some(sink.clone()));
        let first = emergency::receive(&phone.state, link, origin, request(target)).unwrap();
        assert_eq!(outcome(first), Some(dto::EmergencyOutcome::EnteredPause));
        assert_eq!(sink.emergencies.load(Ordering::SeqCst), 1);
        let again = emergency::receive(&phone.state, link, origin, request(target)).unwrap();
        assert_eq!(outcome(again), Some(dto::EmergencyOutcome::AlreadySeen));
        assert_eq!(sink.emergencies.load(Ordering::SeqCst), 1);
        let stale = Boot {
            nonce: [9; 16],
            ..target
        };
        assert_eq!(
            outcome(emergency::receive(&phone.state, link, origin, request(stale)).unwrap()),
            None
        );
        phone.stop().await;
    }
}
