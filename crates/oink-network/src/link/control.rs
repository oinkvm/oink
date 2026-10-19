//! Private network control values; v1 DTO conversion is the only wire path.
use oink_foundation::{
    DeviceId,
    session::{Boot, BootPair, ConnectionId, PathId, RouteRevision},
};

/// Installed path of a link. A relayed path names the relay's reservation; each end only
/// records its next hop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathInstall {
    pub request_id: PathRequestId,
    pub path_id: PathId,
    pub route_revision: RouteRevision,
    pub reservation: Option<RelayReservation>,
    pub next_hop_binding: NextHopBinding,
}

/// End-to-end heartbeat, every 250 ms; the link is lost when no matching `Pong` arrived
/// for 1500 ms. On a relayed link the relay only carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pulse {
    pub path_id: PathId,
    pub route_revision: RouteRevision,
    pub nonce: u64,
}

/// One step of the relay-to-direct cut-over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Migration {
    pub migration_id: MigrationId,
    pub old_path: PathId,
    pub new_connection: ConnectionId,
}

/// Which link a low-priority operation belongs to. `source` and `target` are the logical
/// ends; a relay forwards it and only replaces `hop`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LowBinding {
    pub path_id: PathId,
    pub route_revision: RouteRevision,
    pub source: Boot,
    pub target: Boot,
    pub hop: NextHopBinding,
}

/// One low-priority operation or one private pool action (warm-up, race).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LowRequest {
    pub request: [u8; 16],
    pub connection: [u8; 16],
    pub options: Option<crate::LowOptions>,
    pub extra: bool,
    pub keep_alive: bool,
    pub purpose: crate::LowTcpPurpose,
    pub paced: bool,
    pub binding: LowBinding,
    pub route_tag: u64,
    /// The module that started the operation; the inbound side hands the operation to the
    /// module registered for this kind. Only meaningful for `LowTcpPurpose::Transfer`.
    pub kind: u16,
}

/// Pool control (on N1) and TCP setup/end frames (on the pooled connection).
/// Same set as `session::LowControl`, bound to the link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LowMsg {
    Info {
        paced: bool,
        port: Option<u16>,
        failure: Option<String>,
    },
    /// The point at which the receiving network layer decides by the link's state:
    /// ready → hand to the module; half-open → hold until C0 Done; connected →
    /// answer `Ready` with an error ("unavailable").
    Open(LowRequest),
    Ready {
        request: [u8; 16],
        result: Result<(), String>,
    },
    Reverse {
        request: [u8; 16],
    },
    Cancel {
        request: [u8; 16],
    },
    /// First frame on the TCP connection; carries the same request as `Open`.
    Bind {
        request: LowRequest,
        reverse: bool,
    },
    Record {
        bytes: Vec<u8>,
    },
    End {
        request: [u8; 16],
    },
    EndAck {
        request: [u8; 16],
    },
    RaceWinner {
        request: [u8; 16],
        reverse: bool,
    },
    Refresh,
    RelayUnavailable {
        request: [u8; 16],
        failure: crate::LowUnavailable,
    },
}

impl LowMsg {
    /// True for the messages that travel on N1; the others travel on the TCP connection.
    pub fn is_hop_control(&self) -> bool {
        !matches!(
            self,
            Self::Bind { .. }
                | Self::Record { .. }
                | Self::End { .. }
                | Self::EndAck { .. }
                | Self::RelayUnavailable { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkMsg {
    ConnectionSelect {
        boot_pair: BootPair,
        connection_id: ConnectionId,
        choice_counter: u64,
    },
    ConnectionSelected {
        boot_pair: BootPair,
        connection_id: ConnectionId,
        choice_counter: u64,
    },
    ConnectionReject {
        id: ConnectionId,
        reason: Refusal,
    },
    /// N0, one-hop relay.
    PathRequest {
        /// None at the source; the relay adds its reservation when forwarding.
        reservation: Option<RelayReservation>,
        request_id: PathRequestId,
        expected_target_boot: Option<Boot>,
        target: DeviceId,
        round: u64,
    },
    PathNominate {
        request_id: PathRequestId,
        reservation: RelayReservation,
        endpoint_boot: Boot,
    },
    PathInstall(PathInstall),
    PathInstalled(PathInstall),
    PathCancel {
        reference: PathReference,
        reason: Refusal,
    },
    PathDown {
        path_id: PathId,
    },
    /// N0, heartbeat.
    Pulse(Pulse),
    Pong(Pulse),
    /// N0 or N1, single-hop probe for low-priority pacing.
    HopProbe {
        nonce: u64,
    },
    HopPong {
        nonce: u64,
    },
    /// N0, cut-over: `Begin` and `Drained` on the old path, `Commit` and
    /// `Committed` on the new connection.
    MigrateBegin {
        path_id: PathId,
        from_revision: RouteRevision,
        to_revision: RouteRevision,
        connection_id: ConnectionId,
        migration_id: MigrationId,
    },
    MigrateDrained(Migration),
    MigrateCommit(Migration),
    MigrateCommitted(Migration),
    /// N1 or the pooled TCP connection, see [`LowMsg::is_hop_control`].
    Low {
        binding: LowBinding,
        message: Box<LowMsg>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    NotReady,
    Paused,
    Closed,
    Busy,
    Invalid,
    NotMember,
    DifferentGroups,
    Superseded,
    Expired,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MigrationId {
    pub path_id: PathId,
    pub to_revision: RouteRevision,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathRequestId {
    pub source_boot: Boot,
    pub random: [u8; 16],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathReference {
    Request(PathRequestId),
    Installed(PathId),
}

/// Exact authenticated next-hop reference. Endpoints never receive Quinn handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NextHopBinding {
    pub peer: DeviceId,
    pub connection_id: ConnectionId,
    pub route_tag: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayReservation {
    pub relay: Boot,
    pub random: [u8; 16],
    pub source_connection: ConnectionId,
    pub target_connection: ConnectionId,
}
