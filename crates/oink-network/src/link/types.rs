//! Plain types of the network layer's public interface.
use oink_foundation::DeviceId;
use oink_protocol::frame::{StreamCode, StreamKind, StreamTag};

/// One established link. Local to this process and never reused: a link rebuilt to the
/// same peer has a new id, so anything still holding the old id acts on nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LinkId(pub u64);

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

/// How a link currently reaches its peer. Changes only from relayed to direct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkPath {
    Direct,
    Relayed { via: DeviceId },
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
            StreamKind::Aim | StreamKind::Diagnostics => (Self::new(32, 64), Self::new(32, 64)),
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
}

/// The handle is closed and its queue is empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("stream closed")]
pub struct Closed;

/// The link of this call no longer exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("link is gone")]
pub struct LinkGone;

impl AddressProbeError {
    // Quinn retries UDP send errors until its handshake timeout. Check the local
    // route without sending traffic so an absent interface or route stays typed.
    pub(crate) fn check_route(address: std::net::SocketAddr) -> Result<(), Self> {
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
