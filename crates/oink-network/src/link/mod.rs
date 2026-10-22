//! The network layer owns links and streams.
//!
//! What the layer does: keeps the links that are established, each in one of three
//! states; inside, paths, the connection pool, end-to-end heartbeat, selection between
//! simultaneous dials, one-hop relay, the relay-to-direct cut-over, and its private
//! internal stream (N0) and pool-control stream (N1), neither of which is exposed.
//! What it does not do: discovery, address book, deciding whom to dial or when to retry,
//! any knowledge of members, groups, pause, or the content of upper streams.
//!
//! Threads. All state lives on the thread `oink-network` with its private
//! single-thread runtime. [`LinkLayer`] is a cloneable handle for ordinary threads: every
//! method only sends a small fixed-size command and returns; results come back as
//! notifications ([`events`]). Stream handles ([`stream`]) are used directly by a stream's
//! one writer and one reader thread, realtime threads included. Snapshots ([`snapshot`])
//! can be read anywhere.
//!
//! Reliability, retransmission, flow and congestion control are QUIC's and kernel TCP's.
//! There are no sequence numbers, acknowledgements, replay buffers or credits here.

mod control;
mod control_wire;
mod core;
pub(crate) mod diagnostics;
pub mod events;
mod handle;
pub mod low;
mod pipe;
mod probe_socket;
mod queue;
mod runtime;
pub mod snapshot;
pub mod stream;
mod tcp;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod testing;
pub mod timing;
pub mod types;
mod wire;

pub use events::{ConfigEvent, ConfigPort, LinkConnected, ModuleEvent, ModulePort};
pub use low::{InboundLow, LowError, LowStream};
pub use snapshot::{LinkEntry, LinkTable, SchedulerStatus, StreamCounters};
pub use stream::{DatagramError, DatagramRecv, DatagramSend, InboundStream, RecvStream, SendStream, Wake};
pub use types::*;

use oink_foundation::{DeviceId, realtime::Tier, session::Boot};
use oink_protocol::frame::{Priority, StreamCode, StreamKind, StreamTag};
use std::{net::SocketAddr, sync::Arc, time::Duration};

/// End-to-end heartbeat on N0 (also across a relay, which only carries it).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
/// No matching answer for this long: the link is lost.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(1500);
/// QUIC idle time-out and keep-alive. A black-holed connection ends after 10 to 15 s;
/// the heartbeat decides first.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
pub const KEEP_ALIVE: Duration = Duration::from_secs(5);
/// Admitted peer dials and concurrent outbound candidate handshakes are each
/// limited to eight. Every candidate uses one handshake slot; peers beyond the
/// available slots wait for the next round in admission/address order.
pub const MAX_DIALS: usize = 8;
/// Full TLS budget for each candidate, starting when its round gets a slot.
pub const DIAL_BUDGET: Duration = Duration::from_secs(3);
/// Manual address search, including TLS, selection and the C0 peer greeting.
pub const ADDRESS_QUERY_BUDGET: Duration = Duration::from_secs(5);
/// Inbound connections accepted and still in TLS or selection.
pub const MAX_INBOUND_PENDING: usize = 32;
/// A relay request without a result, and a relay's reservation, expire after this.
pub const RELAY_TIMEOUT: Duration = Duration::from_secs(3);
/// From "begin cut-over" to commit; exceeding it loses the link.
pub const CUTOVER_TIMEOUT: Duration = Duration::from_millis(500);
/// Direct attempts of a relayed link: first after 15 s, then this long after each
/// attempt ended (the last value repeats). `update_addresses` triggers one at once.
pub const DIRECT_RETRY: [Duration; 4] = [
    Duration::from_secs(15),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

/// Time budgets of one round of the network thread's loop: two yields to the QUIC
/// drivers, then own work (N0, heartbeat, cut-over, selection, relay), interactive
/// streams and all datagram flows, then normal streams, commands and new connections.
/// Unused time carries over to the next step; a step stops after the message in hand.
pub const ROUND_BUDGET: Duration = Duration::from_micros(500);
pub const OWN_BUDGET: Duration = Duration::from_micros(100);
pub const INTERACTIVE_BUDGET: Duration = Duration::from_micros(250);
pub const NORMAL_BUDGET: Duration = Duration::from_micros(150);

#[derive(Clone)]
pub struct LinkConfig {
    pub profile: crate::negotiation::OfferProfile,
    pub identity: Arc<crate::identity::Identity>,
    /// This process's instance (device id plus a start-up nonce).
    pub boot: Boot,
    /// UDP address of the QUIC endpoint; the TCP pool listens next to it.
    pub bind: SocketAddr,
    /// Scheduling tier of the network thread; chosen per platform after measuring.
    pub tier: Tier,
}

impl std::fmt::Debug for LinkConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkConfig")
            .field("boot", &self.boot)
            .field("bind", &self.bind)
            .field("tier", &self.tier)
            .finish_non_exhaustive()
    }
}

/// Handle to the network layer. Cheap to clone; every method returns without waiting for
/// the network thread unless it says otherwise.
#[derive(Clone, Debug)]
pub struct LinkLayer(Arc<handle::Shared>);

impl LinkLayer {
    /// Start the network thread and bind the endpoint. The [`ConfigPort`] is the config
    /// module's; there is exactly one.
    pub async fn start(config: LinkConfig) -> Result<(Self, ConfigPort), crate::NetError> {
        Self::launch(config).await
    }

    /// Close every link (reason `Stopped`) and stop the thread. Waits for it.
    pub async fn stop(&self) {
        self.0.send(handle::Command::Stop);
        loop {
            let done = self.0.done.notified();
            tokio::pin!(done);
            done.as_mut().enable();
            if self.0.stopped.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            done.await;
        }
        if let Some(pool) = self.0.pool.get() {
            pool.wait_stopped().await;
        }
    }

    pub fn id(&self) -> DeviceId {
        self.0.boot.device_id
    }
    pub fn profile(&self) -> &crate::negotiation::OfferProfile {
        &self.0.profile
    }
    pub fn boot(&self) -> Boot {
        self.0.boot
    }
    /// Bound address of the QUIC endpoint.
    pub fn local_addr(&self) -> SocketAddr {
        *self.0.address.get().expect("started endpoint")
    }

    // ---- config module only -------------------------------------------------------

    /// Try to establish a link to `peer`. `Ok` means accepted; the outcome arrives as
    /// `ConfigEvent::Connected` or `ConfigEvent::DialFailed`. After a failure the layer
    /// keeps nothing; whether and when to retry is the caller's decision.
    /// Opens this exact IPv4 QUIC service and keeps the authenticated connection
    /// for C0 greeting/negotiation. The config owner exposes a prepared result only
    /// after C0 completes; the TLS identity returned here is never a pairing request.
    pub async fn probe_address(&self, address: SocketAddr) -> Result<DeviceId, AddressProbeError> {
        if !address.is_ipv4() || address.port() == 0 || address.ip().is_unspecified() || address.ip().is_multicast() {
            return Err(AddressProbeError::InvalidAddress);
        }
        if self.0.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err(AddressProbeError::Stopped);
        }
        AddressProbeError::check_route(address)?;
        let (send, receive) = tokio::sync::oneshot::channel();
        self.0.send(handle::Command::ProbeAddress(address, send));
        receive.await.map_err(|_| AddressProbeError::Stopped)?
    }

    pub fn dial(&self, peer: DeviceId, candidates: Vec<SocketAddr>, mode: DialMode) -> Result<(), DialRefused> {
        if candidates.iter().any(|addr| !addr.is_ipv4()) {
            return Err(DialRefused::InvalidAddress);
        }
        self.reserve_dial(peer)?;
        self.0.send(handle::Command::Dial(peer, candidates, mode));
        Ok(())
    }

    /// Disconnect at once. Used for every upper-layer reason (member removed, leaving,
    /// sleep, exit, handshake failure or time-out). Unknown or already lost link: nothing.
    pub fn disconnect(&self, link: LinkId, reason: Disconnect) {
        self.0.send(handle::Command::Disconnect(link, reason));
    }

    /// Responder, on entering "wait for done": send this command before posting the
    /// config reply, so that it takes effect before the initiator's upper streams can
    /// arrive. Lost link: nothing, no error.
    pub fn set_peer_formats(&self, link: LinkId, formats: oink_protocol::v1::dto::FormatSupport) {
        self.0.send(handle::Command::Formats(link, formats));
    }
    /// Withdraw application authority while retaining bounded C0 proof exchange.
    pub fn retire(&self, link: LinkId) {
        self.0.send(handle::Command::Retire(link));
    }
    /// Withdraw business authority while a signed departure drains on C0. The
    /// configuration owner must discard inbound C0 and bound this lifetime.
    pub fn retire_for_departure(&self, link: LinkId) {
        self.0.send(handle::Command::Retire(link));
    }
    pub fn set_old_member_lane(&self, link: LinkId, enabled: bool) {
        self.0.send(handle::Command::OldMemberLane(link, enabled));
    }
    pub fn set_peer_audio(&self, link: LinkId, audio: Vec<oink_protocol::v1::dto::AudioSupport>) {
        self.0.send(handle::Command::Audio(link, audio));
    }
    /// A module reports payload-validation failures without copying rejected data.
    pub fn report_protocol_violation(&self, link: LinkId, violation: oink_protocol::v1::admission::Violation) {
        self.0.send(handle::Command::Violation(link, violation));
    }
    pub fn set_agreement(&self, link: LinkId, agreement: oink_protocol::v1::admission::Agreement) {
        self.0.send(handle::Command::Agreement(link, agreement));
    }
    pub fn set_half_open(&self, link: LinkId) {
        self.0.send(handle::Command::HalfOpen(link));
        self.config_phase_applied();
    }

    /// Initiator after posting `Done`; responder on reading `Done`. On a link that is
    /// already ready or lost: nothing. "Ready" is notified once per link.
    pub fn set_ready(&self, link: LinkId) {
        self.0.send(handle::Command::Ready(link));
        self.config_phase_applied();
    }

    fn config_phase_applied(&self) {
        // C0 is owned by an ordinary configuration thread. Its frame writer is
        // scheduled ahead of ordinary commands, so merely enqueuing a phase
        // update before a reply/Done/Facts does not order admission. Acknowledge
        // the update before letting that caller publish the next C0 frame.
        let (applied, receive) = std::sync::mpsc::sync_channel(1);
        self.0.send(handle::Command::ConfigFence(applied));
        // A stopped networking owner drops the acknowledgement along with its
        // queued commands; lost links retain this API's existing no-op behavior.
        let _ = receive.recv();
    }

    /// Newest candidate addresses of `peer`. Fresh addresses join a pending
    /// search; an established relayed link also attempts its direct path at once.
    pub fn update_addresses(&self, peer: DeviceId, mut candidates: Vec<SocketAddr>) {
        candidates.retain(SocketAddr::is_ipv4);
        self.0.send(handle::Command::Addresses(peer, candidates));
    }

    // ---- business modules ---------------------------------------------------------

    /// Register a module for stream kinds, once per kind, at start-up before any link
    /// exists. Inbound streams and operations of a kind nobody registered are rejected
    /// with `StreamCode::Unsupported`. Panics on a non-business kind or a kind taken.
    pub fn register(&self, kinds: &[StreamKind]) -> ModulePort {
        self.register_port(kinds)
    }

    /// Open a reliable stream this device writes, optionally with the datagram flow that
    /// follows it (the flow cannot be added later). Allocates handle and queue(s) on the
    /// calling thread and produces no traffic: the QUIC stream appears at the first post,
    /// waiting for Quinn to provide stream credit. The caller checks "link ready" and "not paused" itself; on a
    /// link that is not ready the peer rejects with `NotReady`.
    /// `tag.kind` must be a business kind (panics otherwise).
    pub fn open_stream(
        &self,
        link: LinkId,
        priority: Priority,
        tag: StreamTag,
        queue: QueueSpec,
        datagram: Option<QueueSpec>,
    ) -> Result<(SendStream, Option<DatagramSend>), LinkGone> {
        self.allocate_stream(link, priority, tag, queue, datagram)
    }

    /// The upper layers' only notion of closing: abort at once, discarding what is queued
    /// and in flight. Either end may close. The peer sees `StreamClosed::Peer(code)`
    /// (the writer's reset, or the reader's stop request, to which the writer's network
    /// layer answers by resetting its half). The local handle reports `Local`.
    /// Unknown or already closed stream: nothing.
    pub fn close_stream(&self, stream: StreamId, code: StreamCode) {
        self.0.send(handle::Command::Close(stream, code));
    }

    /// Start one low-priority operation on a ready link. Waits for a pooled connection
    /// (on the caller's task). `Unavailable` is an explicit answer, not a time-out.
    pub async fn open_low(
        &self,
        link: LinkId,
        kind: StreamKind,
        options: crate::LowOptions,
    ) -> Result<LowStream, LowError> {
        assert!(kind.is_business(), "private Low kind");
        #[cfg(any(test, feature = "test-support"))]
        {
            *self.0.low_open_attempts.lock().unwrap().entry(kind as u8).or_default() += 1;
        }
        self.0
            .pool
            .get()
            .ok_or(LowError::Interrupted)?
            .open(link, kind, options)
            .await
    }

    /// Physical user activity, for pacing low-priority traffic (kept from the old layer).
    pub fn user_activity(&self, source: crate::UserActivitySource) {
        self.0.activity.note(match source {
            crate::UserActivitySource::Local => crate::activity::Kind::Local,
            crate::UserActivitySource::Group => crate::activity::Kind::Group,
        });
    }

    /// Observe a non-silent speaker-media packet at its source or sink, for Low
    /// pacing only. Audio callbacks may call this directly; silent marked packets
    /// do not count and this never broadcasts user activity.
    pub fn audio_activity(&self) {
        self.0.activity.note(crate::activity::Kind::Audio);
    }

    /// Whether a non-silent media packet was observed in the pacer's current hold.
    pub fn audio_active(&self) -> bool {
        self.0.activity.status().audio
    }

    // ---- snapshots ----------------------------------------------------------------

    /// Ordinary-thread diagnostic projection; never authorizes an operation.
    pub fn low_status(&self, link: LinkId) -> Option<crate::TcpBulkStatus> {
        self.0.pool.get()?.status(link)
    }
    pub fn tcp_inbound_verified(&self) -> bool {
        self.0.pool.get().is_some_and(|p| p.tcp_inbound_verified())
    }

    pub fn links(&self) -> Arc<LinkTable> {
        self.0.links.load_full()
    }
    pub fn scheduler(&self) -> Arc<SchedulerStatus> {
        self.0.scheduler.load_full()
    }
    pub fn stream_counters(&self) -> Arc<Vec<StreamCounters>> {
        self.0.counters.load_full()
    }
}

#[cfg(test)]
mod handle_tests;
