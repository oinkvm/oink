//! Notifications from the network thread to ordinary-thread tasks.
//!
//! Order guaranteed per link: `Connected` → `Ready` (exactly once) → inbound streams and
//! inbound low-priority operations → `Lost` (exactly once, after which every stream of
//! the link is closed). A candidate connection that lost the selection never produces a
//! notification. The layer does not hold inbound streams back for anyone.
use super::{
    low::InboundLow,
    stream::{InboundStream, RecvStream, SendStream},
    types::{DialFailure, Direction, LinkId, LinkLost, LinkPath, StreamClosed, StreamId},
};
use oink_foundation::{DeviceId, session::Boot};

/// A link exists. The two C0 streams are the same ones before and after ready.
#[derive(Debug)]
pub struct LinkConnected {
    pub link: LinkId,
    pub peer: DeviceId,
    /// Process instance exchanged on N0. Not part of the handshake messages.
    pub peer_boot: Boot,
    pub path_id: oink_foundation::session::PathId,
    pub route_revision: u64,
    /// Physical endpoint negotiation; relayed endpoints negotiate on C0.
    pub negotiation: Option<crate::negotiation::Selected>,
    /// `Outbound` = this end is the handshake initiator A.
    pub direction: Direction,
    pub path: LinkPath,
    /// Actual successful direct address; relayed links have none.
    pub remote_address: Option<std::net::SocketAddr>,
    /// C0 written by this device.
    pub control_tx: SendStream,
    /// C0 written by the peer.
    pub control_rx: RecvStream,
}

/// Everything the config module is told. Single consumer.
#[derive(Debug)]
pub enum ConfigEvent {
    Connected(Box<LinkConnected>),
    /// Explicit C0 admission completed; HalfOpen never grants business authority.
    Ready {
        link: LinkId,
        peer: DeviceId,
    },
    Lost {
        link: LinkId,
        peer: DeviceId,
        /// The path the link had last ("last time relayed" for the next dial mode).
        path: LinkPath,
        reason: LinkLost,
    },
    /// Display and diagnostics only.
    PathChanged {
        link: LinkId,
        path: LinkPath,
    },
    DialFailed {
        peer: DeviceId,
        reason: DialFailure,
    },
    /// One of the two C0 handles closed while the link lives.
    ControlClosed {
        stream: StreamId,
        reason: StreamClosed,
    },
}

/// Receiver of [`ConfigEvent`]s, returned once by `LinkLayer::start`.
#[derive(Debug)]
pub struct ConfigPort(pub(super) tokio::sync::mpsc::UnboundedReceiver<ConfigEvent>);

impl ConfigPort {
    /// `None` after the layer stopped. Never drops an event.
    pub async fn next(&mut self) -> Option<ConfigEvent> {
        self.0.recv().await
    }
}

/// Everything a business module is told about links and about streams of its kinds.
#[derive(Debug)]
pub enum ModuleEvent {
    /// Central diagnostics delivery for every protocol family, including N0/C0.
    /// Sent only to the diagnostics ModulePort to avoid duplicate recording.
    ProtocolViolation {
        peer: DeviceId,
        violation: oink_protocol::v1::admission::ProtocolViolation,
    },
    LinkReady {
        link: LinkId,
        peer: DeviceId,
    },
    LinkLost {
        link: LinkId,
        peer: DeviceId,
        reason: LinkLost,
    },
    PathChanged {
        link: LinkId,
        path: LinkPath,
    },
    /// A stream of a registered kind arrived on a ready link.
    Inbound(InboundStream),
    /// A low-priority operation of a registered kind arrived on a ready link.
    InboundLow(Box<InboundLow>),
    /// A handle of a registered kind closed (outbound or inbound).
    StreamClosed {
        stream: StreamId,
        reason: StreamClosed,
    },
}

/// Receiver of [`ModuleEvent`]s, from `LinkLayer::register`.
#[derive(Debug)]
pub struct ModulePort(pub(super) tokio::sync::mpsc::UnboundedReceiver<ModuleEvent>);

impl ModulePort {
    /// `None` after the layer stopped. Never drops an event; a module that does not read
    /// its port only delays itself (and, for an unanswered inbound stream, a cut-over).
    pub async fn next(&mut self) -> Option<ModuleEvent> {
        self.0.recv().await
    }
}
