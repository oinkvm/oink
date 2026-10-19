//! Read-only snapshots produced by the network thread, readable from any thread. Each is replaced whole; a reader takes no lock.
use super::types::{LinkId, LinkPath, LinkState, StreamId};
use oink_foundation::{DeviceId, realtime::Applied, session::Boot};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct LinkEntry {
    pub link: LinkId,
    pub peer: DeviceId,
    pub peer_boot: Boot,
    pub path_id: oink_foundation::session::PathId,
    pub route_revision: u64,
    pub agreement: oink_protocol::v1::admission::Agreement,
    pub peer_formats: Option<oink_protocol::v1::dto::FormatSupport>,
    pub peer_audio: Option<Vec<oink_protocol::v1::dto::AudioSupport>>,
    pub state: LinkState,
    pub path: LinkPath,
    /// Actual direct QUIC peer address; relayed links have no end-to-end socket address.
    pub remote_address: Option<std::net::SocketAddr>,
    /// End-to-end, from the heartbeat.
    pub rtt: Option<Duration>,
    /// When the matching Pong was processed, not when this snapshot was published.
    /// Process-local only; diagnostics records its age to align RTT with packet traces.
    pub rtt_measured_at: Option<Instant>,
    pub lost_packets: u64,
    pub sent_packets: u64,
    /// QUIC UDP payload bytes on the selected physical connection (no IP/UDP headers).
    pub udp_bytes_sent: u64,
    pub udp_datagrams_sent: u64,
    pub udp_bytes_received: u64,
    pub low_available: bool,
    /// When the low-priority channel is unavailable: which directed hop failed and why.
    pub low_failure: Option<crate::LowUnavailable>,
}

/// Established links only; dial attempts are not links. Path and connection are one
/// snapshot, never two separate reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkTable {
    pub links: Vec<LinkEntry>,
}

impl LinkTable {
    pub fn by_peer(&self, peer: DeviceId) -> Option<&LinkEntry> {
        self.links.iter().find(|l| l.peer == peer)
    }
    /// "Online" everywhere above the network layer means exactly this.
    pub fn ready(&self, peer: DeviceId) -> bool {
        self.by_peer(peer).is_some_and(|l| l.state == LinkState::Ready)
    }
    /// The handshake's "connected members": peers with a ready link.
    pub fn ready_count(&self) -> usize {
        self.links.iter().filter(|l| l.state == LinkState::Ready).count()
    }
}

/// Detection only; nothing reacts to these numbers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SchedulerStatus {
    /// Policy read back from the OS for the network thread.
    pub applied: Option<Applied>,
    /// Rounds whose execution exceeded the round budget, and the longest of them.
    pub over_budget: u64,
    pub over_budget_max: Duration,
    /// Periodic read-backs that differed from what was set.
    pub demoted: u64,
    /// Rounds that started later than they should have, and the largest lateness.
    pub late_rounds: u64,
    pub late_max: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamCounters {
    pub stream: StreamId,
    /// Messages currently queued at this end.
    pub depth: u32,
    /// Datagram flows: messages dropped (oldest first) or refused as too large.
    pub dropped: u64,
    /// Reliable outbound: posts answered `Full`.
    pub full: u64,
}
