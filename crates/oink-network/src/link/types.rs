//! Plain types of the network layer's public interface.
use oink_foundation::DeviceId;
use oink_protocol::frame::{StreamCode, StreamKind, StreamTag};
use std::net::SocketAddr;

/// One established link. Local to this process and never reused: a link rebuilt to the
/// same peer has a new id, so anything still holding the old id acts on nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LinkId(pub u64);

/// Why the network layer ended on its own (not by `LinkLayer::stop`). The
/// layer does not repair itself: it closes every link and exits (a link whose
/// connection the dead driver already ended is lost as `ConnectionLost`, the
/// rest as `Stopped`), and its owner restarts it through its normal startup path with
/// a new Boot (`link::restart`). The same Boot cannot simply come back with a
/// new layer: peers keep its selection watermark (`Core::observe_peer_boot`),
/// which a fresh layer would restart from zero.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NetworkFatal {
    /// The shared QUIC endpoint's driver ended while the layer was running:
    /// the socket is no longer read or written, every dial fails at once with
    /// `EndpointStopping` and no inbound connection arrives (2026-11-12: one
    /// receive error ended it and a Mac test machine stayed offline until Oink restarted).
    #[error("shared QUIC endpoint driver stopped (detected by {detected_by}, local address {local_address:?})")]
    EndpointDriverStopped {
        /// `"dial"` (connect returned EndpointStopping) or `"accept"` (the
        /// endpoint stopped yielding incoming connections).
        detected_by: &'static str,
        local_address: Option<SocketAddr>,
    },
}

impl NetworkFatal {
    /// Stable short name for logs, diagnostics and reports.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EndpointDriverStopped { .. } => "endpoint_driver_stopped",
        }
    }
}

/// State of an established link. There is no way back; a link only gets lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// Both ends can exchange data. Only N0 and C0 are in use. Inbound upper streams are
    /// rejected with `StreamCode::NotReady`, inbound low-priority operations refused.
    Connected,
    /// Roster/state accepted but Done is pending. Bounded incoming stream headers
    /// are held without reading payload or delivering application handles.
    HalfOpen,
    /// The handshake is complete. Upper streams and low-priority operations are allowed,
    /// the link may request a relay and serve as one.
    Ready,
}

/// How a link currently reaches its peer. Changes from relayed to direct; a route
/// switch between direct connections keeps it `Direct`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkPath {
    Direct,
    Relayed { via: DeviceId },
}

/// Relay endpoint capability (negotiated): the relay path protocol
/// (`LinkPathRequest`, `LinkPathNominate`, relayed install and migration), so
/// that a relayed path can end here. Phones and PCs offer it.
pub const RELAY_ENDPOINT: &str = "oink.relay/1";
/// Relay station role (announced, one-sided): the offering end forwards other
/// devices' paths and their Low operations. Only PCs offer it.
pub const RELAY_TRANSIT: &str = "oink.relay.transit/1";

/// What the peer of one authenticated connection is in a relay path
/// (protocol-v2 §1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayRole {
    /// No relay path messages on this connection (no oink.relay/1 agreed).
    None,
    /// The peer can terminate a relayed path but forwards none (a phone).
    Endpoint,
    /// The peer can terminate a relayed path and is a relay station (a PC).
    Station,
}
impl RelayRole {
    /// `peer_offer`: the capabilities in the peer's own `ConnectionOffer`.
    /// The station role is the peer's announcement, not an intersection: a
    /// phone asks a PC to relay although it never relays itself.
    pub fn of(agreement: &oink_protocol::v2::admission::Agreement, peer_offer: &[String]) -> Self {
        if !agreement.contains(RELAY_ENDPOINT) {
            Self::None
        } else if peer_offer.iter().any(|capability| capability == RELAY_TRANSIT) {
            Self::Station
        } else {
            Self::Endpoint
        }
    }
}

/// The three dial modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialMode {
    /// Pairing. Never tries a relay.
    DirectOnly,
    /// New target, own restart. Relay only after the direct dial failed (at once when
    /// there are no candidate addresses).
    PreferDirect,
    /// Known target whose last link was relayed: direct dial and relay request together.
    All,
}

/// What started a dial. Only performance reporting reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialOrigin {
    /// A request or recovery signal: pairing, a new member, a manual address, startup,
    /// wake, a network change, a lost ready link. Every outcome is a setup sample.
    Initial,
    /// A timed retry after this peer's previous attempt failed (or a timed direct
    /// upgrade of a relayed link). Only a success is a setup sample: an unreachable
    /// member would otherwise fill `link_setup` with dial-budget timeouts.
    BackgroundRetry,
}

/// Why `LinkLayer::dial` did not start an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DialRefused {
    #[error("invalid address")]
    InvalidAddress,
    #[error("a link to this device already exists")]
    AlreadyLinked,
    /// `MAX_DIALS` attempts are already running.
    #[error("too many dials in progress")]
    Busy,
    #[error("the network layer has stopped")]
    Stopped,
}

/// Exact-endpoint TLS query failure; no discovery or pairing request is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AddressProbeError {
    #[error("invalid address")]
    InvalidAddress,
    #[error("too many queries")]
    Busy,
    #[error("service stopped")]
    Stopped,
    #[error("query timed out")]
    Timeout,
    #[error("no route to the network")]
    NoRoute,
    #[error("no usable network interface")]
    NoNetworkInterface,
    #[error("peer unreachable")]
    Unreachable,
    #[error("network socket failed")]
    Socket,
    #[error("connection refused")]
    Refused,
    #[error("TLS identity is not an Oink identity")]
    Authentication,
    #[error("address is this device")]
    SelfDevice,
}

/// Result of a dial attempt that ended without a link. The layer keeps no state about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialFailure {
    /// An authenticated endpoint presented a different full identity.
    IdentityChanged,
    /// No address answered within the budget and (unless `DirectOnly`) no relay was found.
    Unreachable,
    /// The peer refused the connection during selection.
    Refused,
    Stopped,
}

/// Which end started the link. Complementary at the two ends: the outbound end is the
/// handshake initiator A. Direct: the dialer of the connection that survived selection.
/// Relayed: the arbiter (smaller device id), which installs the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Outbound,
    Inbound,
}

/// Reason given by an ordinary owner to `LinkLayer::disconnect`. Carried to the log and
/// to `LinkLost::Local`; the peer only sees the link go away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disconnect {
    MemberRemoved,
    LeavingGroup,
    Sleeping,
    Stopping,
    HandshakeFailed,
    HandshakeTimeout,
    /// C0 was closed by the peer or carried invalid content after ready.
    ControlStreamFailed,
    /// A ready link did not attach its input stream pair before its opening deadline.
    InputOpeningTimeout,
    /// The local capability profile changed; the owner will reconnect using
    /// the replacement profile.
    CapabilityChanged,
}

/// Why an established link is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkLost {
    Local(Disconnect),
    /// No matching heartbeat answer for `HEARTBEAT_TIMEOUT`.
    HeartbeatTimeout,
    /// QUIC error or idle time-out, or the peer closed the connection.
    ConnectionLost,
    /// The relay closed all streams of this link (its other side went away).
    RelayLost,
    /// The cut-over drain exceeded `CUTOVER_TIMEOUT`.
    CutoverTimeout,
    /// A new connection showed a different process instance of the peer.
    PeerRestarted,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StreamDir {
    /// Written by this device.
    Out,
    /// Written by the peer.
    In,
}

/// Identity of a stream handle: link, direction and tag. Fixed size, used in commands
/// and notifications. A late notification for an old ordinal never matches a newer stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamId {
    pub link: LinkId,
    pub dir: StreamDir,
    pub tag: StreamTag,
}

/// Why a stream handle became closed. Reported once per handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamClosed {
    /// The other end closed it: for a stream this device writes, the reader stopped or
    /// rejected it; for a stream the peer writes, the writer reset it.
    Peer(StreamCode),
    LinkLost,
    /// `LinkLayer::close_stream` on this end.
    Local,
}

/// Capacity of one queue (one stream, one end): whichever bound is reached first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueSpec {
    pub messages: u16,
    pub bytes: u32,
}

impl QueueSpec {
    const fn new(messages: u16, kib: u32) -> Self {
        Self {
            messages,
            bytes: kib * 1024,
        }
    }
    /// Initial depths for the reliable stream of `kind`: (send queue, receive queue).
    pub const fn reliable(kind: StreamKind) -> (Self, Self) {
        match kind {
            StreamKind::Input => (Self::new(640, 64), Self::new(192, 64)),
            StreamKind::Config => (Self::new(16, 384), Self::new(16, 384)),
            StreamKind::Clipboard => (Self::new(16, 64), Self::new(16, 64)),
            StreamKind::Diagnostics => (Self::new(32, 64), Self::new(32, 64)),
            _ => (Self::new(8, 64), Self::new(8, 64)),
        }
    }
    /// Initial depth of the datagram flow following `kind`, both ends.
    pub const fn datagram(kind: StreamKind) -> Self {
        match kind {
            StreamKind::Audio => Self::new(64, 64),
            StreamKind::Diagnostics => Self::new(16, 64),
            _ => Self::new(8, 64),
        }
    }
}

/// `SendStream::post` did not take the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PostError {
    /// The send queue is full. The message was not queued; the owning module closes this
    /// stream and backs off.
    #[error("send queue full")]
    Full,
    /// The handle is closed (see the `StreamClosed` notification for the reason).
    #[error("stream closed")]
    Closed,
    /// Larger than the limit of this stream. A programming or data error of the caller.
    #[error("message too large")]
    TooLarge,
    /// The send gate refused the frame: its type has no route on this stream, or
    /// this link did not negotiate the capabilities the registry requires for it
    /// (protocol-v2 1.1). Nothing was queued; the stream stays usable.
    #[error("message not negotiated on this stream")]
    Unnegotiated,
    /// This device's sharing is paused and the registry pause policy of the
    /// message is `Suspend` (protocol-v2 1.1). Nothing was queued; the stream
    /// stays usable and the message may be sent after resuming.
    #[error("sharing is paused")]
    Paused,
}

/// The handle is closed and its queue is empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("stream closed")]
pub struct Closed;

/// `LinkLayer::open_stream` did not open the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    /// The link of this call no longer exists.
    #[error("link is gone")]
    LinkGone,
    /// The send gate refused the stream: this link did not negotiate the
    /// registry's stream gate for its kind (protocol-v2 1.1).
    #[error("stream kind not negotiated on this link")]
    Unnegotiated,
}

impl AddressProbeError {
    // Quinn retries UDP send errors until its handshake timeout. Check the local
    // route without sending traffic so an absent interface or route stays typed.
    pub(crate) fn check_route(address: SocketAddr) -> Result<(), Self> {
        if !address.ip().is_loopback() && crate::discovery::local_ipv4().is_empty() {
            return Err(Self::NoNetworkInterface);
        }
        let socket =
            std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).map_err(|error| Self::from_io(&error))?;
        socket.connect(address).map_err(|error| Self::from_io(&error))
    }
    pub(crate) fn from_io(error: &std::io::Error) -> Self {
        use std::io::ErrorKind;
        match error.kind() {
            ErrorKind::NetworkUnreachable => Self::NoRoute,
            ErrorKind::AddrNotAvailable | ErrorKind::NotConnected => Self::NoNetworkInterface,
            ErrorKind::HostUnreachable => Self::Unreachable,
            ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset => Self::Refused,
            ErrorKind::TimedOut => Self::Timeout,
            _ => Self::Socket,
        }
    }
    pub(crate) fn from_connect(error: quinn::ConnectError) -> Self {
        match error {
            quinn::ConnectError::EndpointStopping => Self::Stopped,
            quinn::ConnectError::InvalidRemoteAddress(_) => Self::InvalidAddress,
            _ => Self::Socket,
        }
    }
    pub(crate) fn from_connection(error: quinn::ConnectionError) -> Self {
        match error {
            quinn::ConnectionError::TimedOut => Self::Timeout,
            quinn::ConnectionError::LocallyClosed => Self::Stopped,
            quinn::ConnectionError::CidsExhausted => Self::Busy,
            quinn::ConnectionError::Reset => Self::Refused,
            quinn::ConnectionError::TransportError(_) | quinn::ConnectionError::VersionMismatch => Self::Authentication,
            quinn::ConnectionError::ConnectionClosed(_) | quinn::ConnectionError::ApplicationClosed(_) => Self::Refused,
        }
    }
}

/// The user's route choice for one peer (protocol-v2 section 11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteChoice {
    /// Automatic selection between the pair's direct connections.
    Auto,
    /// Replace the current connection with this device's own dial to this
    /// address of the peer, and keep it.
    Pin(std::net::Ipv4Addr),
}

/// A pin as seen from this end; the owner persists it per peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteRecord {
    /// This device dials this address of the peer.
    Dial(std::net::Ipv4Addr),
    /// The peer dials this address of this device.
    Incoming(std::net::Ipv4Addr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteRowState {
    /// No completed probe round yet.
    Measuring,
    Reachable,
    /// Not dialable, wrong identity, this device itself, or probes lost.
    Unreachable,
}

/// One address of the peer, dialed by this device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteRow {
    pub address: SocketAddr,
    pub state: RouteRowState,
    /// Median of the last six probe rounds (heartbeat rounds for the current row).
    pub median: Option<std::time::Duration>,
    /// This device's dial to this address is the link's current connection.
    pub current: bool,
}

/// Route projection of one ready link: direct, or relayed with its direct
/// addresses still being tried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteView {
    /// Who dialed the current connection: `Inbound` means this device is passive.
    pub direction: Direction,
    /// Outbound: the peer address dialed. Inbound: this device's address it reached.
    pub current_target: Option<std::net::Ipv4Addr>,
    pub current_median: Option<std::time::Duration>,
    /// The pair's pin from this end; `None` is automatic.
    pub pin: Option<RouteRecord>,
    pub pin_set_by: Option<DeviceId>,
    /// A pin exists but the current connection is not the pinned one.
    pub pin_unavailable: bool,
    pub rows: Vec<RouteRow>,
    /// The own dial "Automatic" would use (lowest median reachable).
    pub auto_choice: Option<std::net::Ipv4Addr>,
    /// A better route waiting for the link to become idle.
    pub pending_switch: Option<std::net::Ipv4Addr>,
    /// At least one reachable own dial exists, so this end can replace the route.
    pub can_replace: bool,
    /// A manual operation of this end is still in progress.
    pub pending_manual: bool,
    /// The link runs over this relay: no direct connection is current, the
    /// rows are the peer's addresses this end keeps trying to dial.
    pub relay: Option<DeviceId>,
}
