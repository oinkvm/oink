//! 文件传输用到的网络接口：由服务进程用组网（精确 session token / `BulkStream`）实现。
//!
//! The clipboard owns only this transport interface; the agent supplies routing.

use crate::model::ContentMsg;
use oink_foundation::DeviceId;
use oink_foundation::content::ClipId;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

/// 装箱的 future（接口需要 `dyn`，不能用 `async fn`）。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 打开到对端的大块数据流。
pub trait BulkLink: Send + Sync + 'static {
    /// Existing human-readable peer label, used only in local transfer logs.
    fn peer_label(&self, peer: DeviceId) -> String {
        peer.short()
    }

    /// Freeze offer/source identity when a download is created, before a native
    /// reader can arrive. A bound link may reopen streams after ordinary loss,
    /// but must never select a different offer or source process.
    fn bind(self: Arc<Self>, peer: DeviceId, offer: ClipId) -> Arc<dyn BulkLink>;
    /// Terminal operation cancellation; static links need no external signal.
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }

    /// Persistent namespaces reopen using current membership and connection,
    /// independently of the original offer and source process. Static adapters
    /// must opt in explicitly; unsupported peers cannot use a legacy fallback.
    fn bind_persistent(self: Arc<Self>, _peer: DeviceId) -> io::Result<Arc<dyn BulkLink>> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn supports_cloud_links(&self) -> bool {
        false
    }

    fn supports_namespace(&self) -> bool {
        true
    }

    /// Static local links use the implemented native contracts. Network links
    /// override this with the authenticated logical endpoints' negotiated profiles.
    fn select_representation(
        &self,
        inventory: &crate::model::Inventory,
        representation: crate::model::RepresentationId,
        target: oink_foundation::clipboard::ClipPlatform,
    ) -> io::Result<(crate::model::ItemId, String)> {
        crate::negotiation::selection(inventory, representation, target)
            .ok_or_else(|| crate::content::unavailable(crate::model::Unavailable::Unsupported))
    }

    /// 打开一条到 `peer` 的新流。没有连接时返回错误（调用方会退避重试）。
    fn open(&self, peer: DeviceId) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>>;
    /// Whole-operation size is retained across constituent streams and retries.
    fn open_with_options(
        &self,
        peer: DeviceId,
        _options: oink_foundation::transfer::LowOptions,
    ) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>> {
        self.open(peer)
    }
}

/// Non-private pacing observation; no controller rate or peer identity.
#[derive(Clone, Copy, Debug)]
pub struct TransferPacing {
    pub limited: bool,
    pub rtt: Option<std::time::Duration>,
    pub target: std::time::Duration,
}
pub type PacingObserver = Arc<dyn Fn() -> Option<TransferPacing> + Send + Sync>;

/// 一条大块数据流：双向、可靠有序，以「帧」为单位收发（一帧 = 一条类型化 `Message`）。
pub trait BulkChannel: Send {
    /// Existing Low nonce, for local diagnostics only; never changes a frame.
    fn diagnostic_id(&self) -> [u8; 16] {
        [0; 16]
    }

    /// Read-only snapshot callback, usable while a send/receive is pending.
    fn pacing_observer(&self) -> PacingObserver {
        Arc::new(|| None)
    }

    /// Local logging only; the route of this exact operation, frozen at open.
    fn transfer_path(&self) -> &'static str {
        "unknown"
    }

    /// Retain the original stream's permission through deferred disk work.
    /// Completion does not invalidate this guard; scope revocation does.
    /// Static in-memory channels have no external authority to revoke.
    fn effect_validity(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        Arc::new(|| true)
    }
    fn send<'a>(&'a mut self, frame: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>>;
    /// 收一帧；对端正常结束时返回 `None`。
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>>;
    /// Close this side's writing. Consume the peer's None before releasing the
    /// channel: dropping an unread Low side interrupts the operation.
    fn finish(&mut self);
}
