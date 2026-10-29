//! Dedicated interactive network owner and ordinary metadata/pairing/Low services.
//! All QUIC state lives on one private current-thread executor. Native callers
//! use exact-session atomic gates and bounded queues through `session`.

use oink_foundation::DeviceId;
use std::time::Duration;

mod activity;
#[cfg(target_os = "macos")]
mod local_network;
#[cfg(target_os = "macos")]
pub use local_network::{LocalNetworkAccess, local_network_access, observe_local_network_access};
pub mod admission;

mod config;
pub mod discovery;
pub mod group_merge;
pub mod group_pake;
pub mod group_state;
pub mod identity;
pub mod link;
pub mod negotiation;
pub mod pair_journal;
pub mod pair_merge;
pub mod pair_objects;
pub mod pairing;
pub mod roster;
pub mod wire_identity;

mod pace;
mod record;

mod qos;

mod tls;

pub use config::{DEFAULT_PORT, MdnsMode, NetworkConfig};
pub use discovery::Advert as DiscoveryAdvert;
pub use discovery::{Sighting as DiscoverySighting, Source as DiscoverySource};
mod low_types;
pub use low_types::{LowFailureReason, LowOptions, LowTcpPurpose, LowUnavailable};

/// Process-wide UDP send workaround counters; no endpoint addresses are retained.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UdpSendStatus {
    pub windows_source_hint_resends: u64,
    pub windows_source_hint_segmented_resends: u64,
}

pub fn udp_send_status() -> UdpSendStatus {
    qos::udp_send_status()
}

/// 组网错误。
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("{0}")]
    LowUnavailable(LowUnavailable),
    #[error("datagram was not admitted: {0}")]
    DatagramDropped(&'static str),
    #[error("ordered send queue is full")]
    Full,
    #[error("message is too large (limit {limit} bytes)")]
    TooLarge { limit: usize },
    #[error("device group is fully paused")]
    Paused,
    #[error("business session is not ready")]
    NotReady,
    #[error("send context is no longer current")]
    Closed,
    #[error("I/O 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("QUIC 端点错误：{0}")]
    Endpoint(String),
    #[error("与 {0} 没有连接")]
    NotConnected(DeviceId),
    #[error("没有找到这个设备组")]
    UnknownGroup,
    #[error("没有这条加入请求")]
    UnknownRequest,
    #[error("当前没有进行中的加入")]
    NotJoining,
    #[error("本机所在的组里还有其他设备，离开前需要用户确认")]
    NeedsLeaveConfirmation,
    #[error("连接不上 {0}（对方不在线、端口不对，或被防火墙挡住）")]
    Unreachable(String),
    #[error("地址无效：{0}")]
    BadAddress(String),
    #[error("操作无效：{0}")]
    Invalid(String),
    #[error("组网已停止")]
    Stopped,
}

/// Source of an already accepted activity notification. Group pulses may report
/// physical input or real audio; this observation only updates local Low pacing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserActivitySource {
    Local,
    Group,
}

/// The outgoing Low budget shared by all streams to the next hop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LowPacingStatus {
    /// Whether the saved controller rate currently limits this outgoing budget.
    pub limited: bool,
    pub local_activity: bool,
    pub group_activity: bool,
    /// Unmarked (source reports sound) speaker media within
    /// three seconds. Silent marked packets may still be flowing/acknowledged.
    pub audio_activity: bool,
    /// Saved controller rate; not a rate limit when `limited` is false.
    pub rate_bytes_per_second: u64,
    pub active_streams: usize,
    pub rtt: Option<Duration>,
    pub observation_stale: bool,
    pub target: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TcpBulkPhase {
    OnDemand,
    Dialing,
    ReverseDialing,
    Ready,
    Unavailable,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcpBulkStatus {
    pub phase: TcpBulkPhase,
    pub active_connections: usize,
    pub idle_connections: usize,
    pub opening_connections: usize,
    pub queued_requests: usize,
    /// Per requester, with one overflow slot for whole operations below 4 MiB.
    pub regular_cap: u8,
    pub extra_cap: u8,
    pub connections_opened: u64,
    pub connections_reused: u64,
    pub prefer_reverse: bool,
    pub warm_ready: bool,
    pub warm_time: Option<Duration>,
    pub dialer: Option<DeviceId>,
    pub cipher: Option<&'static str>,
    pub last_failure: Option<LowUnavailable>,
    pub first_dial_failure: Option<String>,
    pub pacing: Option<LowPacingStatus>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkTransport {
    TcpTls,
}

pub mod pointer_slots;
