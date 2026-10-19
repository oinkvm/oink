//! Low-priority operations: one pooled TCP+TLS connection per operation, carrying large
//! content that is not urgent. The read/write interface is the
//! pool's existing one (open an operation, read and write whole messages, finish); the
//! work happens on the calling ordinary-thread task, directly on TCP, not on the network
//! thread and not through the stream queues.
//!
//! The pool's grant belongs to the link: opened when the link becomes ready (including a
//! half-open link after validated C0 Done), closed with the link; at a
//! cut-over the operations on the old path are cancelled. An operation belongs to the
//! module that started it; that module cancels its own operations (pause, the offer
//! stream it depends on closing). The network layer never judges that for a module.
use super::types::LinkId;
use oink_foundation::DeviceId;
use oink_protocol::{frame::StreamKind, v1::Message};

#[derive(Debug, thiserror::Error)]
pub enum LowError {
    /// Explicit refusal: the link is not ready at the peer, or no direction of the hop
    /// can be dialled. Only this operation failed; no stream is affected.
    #[error("low-priority channel unavailable: {0}")]
    Unavailable(crate::LowUnavailable),
    /// The link went away or was cut over to another path; the operation is interrupted.
    /// File transfers resume from the verified offset.
    #[error("operation interrupted")]
    Interrupted,
    #[error("the operation's byte budget is exhausted")]
    Budget,
    #[error("message too large")]
    TooLarge,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// One operation, both directions (a request and its answers).
#[derive(Debug)]
pub struct LowStream {
    link: LinkId,
    peer: DeviceId,
    kind: StreamKind,
    inner: super::tcp::bulk::BulkStream,
}

impl LowStream {
    pub(super) fn new(inner: super::tcp::bulk::BulkStream) -> Self {
        Self {
            link: inner.link,
            peer: inner.peer,
            kind: inner.kind,
            inner,
        }
    }
    /// Existing operation nonce, identical at both endpoints. Local diagnostics only.
    pub fn diagnostic_id(&self) -> [u8; 16] {
        self.inner.diagnostic_id
    }

    /// Route of this exact Low operation; ordinary-thread local diagnostics only.
    pub fn path(&self) -> super::LinkPath {
        self.inner.path
    }
    pub fn link(&self) -> LinkId {
        self.link
    }
    pub fn peer(&self) -> DeviceId {
        self.peer
    }
    /// The module that started the operation.
    pub fn kind(&self) -> StreamKind {
        self.kind
    }
    /// Read-only outgoing pacing observation for ordinary transfer diagnostics.
    pub fn pacing_observer(&self) -> std::sync::Arc<dyn Fn() -> Option<crate::LowPacingStatus> + Send + Sync> {
        self.inner.pacing_observer()
    }
    /// Validity of this exact operation for a deferred disk/native effect. A
    /// graceful finish preserves it; abort or retiring the path invalidates it.
    pub fn effect_validity(&self) -> std::sync::Arc<dyn Fn() -> bool + Send + Sync> {
        self.inner.effect_validity()
    }
    pub async fn send(&mut self, message: &Message) -> Result<(), LowError> {
        self.inner.send(message).await
    }
    /// `Ok(None)` at the peer's orderly end.
    pub async fn recv(&mut self) -> Result<Option<Message>, LowError> {
        self.inner.recv().await
    }
    /// Orderly end of this side's writing; the connection returns to the pool when both
    /// sides have finished.
    pub fn finish(&mut self) {
        self.inner.finish();
    }
    /// Abort; the peer's pending read fails with `Interrupted`. Also what dropping does.
    pub fn cancel(&mut self) {
        self.inner.cancel();
    }
}

/// An operation the peer started, handed to the module registered for `stream.kind()`.
/// Always preceded by "link ready" for its link.
#[derive(Debug)]
pub struct InboundLow {
    pub stream: LowStream,
    /// The first message of the operation.
    pub first: Message,
}

impl From<crate::NetError> for LowError {
    fn from(error: crate::NetError) -> Self {
        match error {
            crate::NetError::LowUnavailable(failure) => Self::Unavailable(failure),
            crate::NetError::TooLarge { .. } => Self::TooLarge,
            crate::NetError::Io(error) => Self::Io(error),
            crate::NetError::Closed | crate::NetError::Stopped | crate::NetError::NotReady => Self::Interrupted,
            error => Self::Io(std::io::Error::other(error)),
        }
    }
}
