//! Mobile transfer domain owned by Rust.
//!
//! The crate deliberately has no protocol dependency.  Network DTOs belong to
//! the v2 protocol owner; callers translate [`TransportRequest`] to the one
//! shared `Message` enum before putting it on the wire.

pub mod android_gestures;
pub mod android_input;
mod coordinator;
mod diagnostics;
pub mod input_router;
pub mod key_layouts;
mod model;
mod network;
pub mod rich_text;
mod store;
pub mod trackpad;
mod v2_attachments;
mod v2_session;

use serde::{Deserialize, Serialize};

pub use coordinator::{CoordinatorError, CurrentQuery, CurrentQueryResult, Ingested, TransferCoordinator, Transport};
pub use diagnostics::{
    DiagnosticCode, DiagnosticEvent, DiagnosticKind, DiagnosticLog, DiagnosticSegment, DiagnosticSink,
    DiagnosticUpload, LogLevel, LogValue, MobileDiagnostics,
};
pub use model::{
    AttachmentKind, DeliveryFailure, DeliveryState, DownloadState, DownloadStatus, Id16, ImagePreview,
    IncomingAnnouncement, MessageId, MessageRecord, Page, PageCursor, Peer, PeerKind, PeerState, ProxyChoice,
    RichTextFormat, SendItem, SendOrigin, SendPlan, SharePayload, SourceAvailability, SourceState, TextFormat,
    ThumbnailState,
};
pub use network::{
    BlockError, BlockedDevice, CancelOutcome, DefaultInputOpening, DeviceControlCompletion, GroupStateProblem,
    InputOpening, MAX_BLOCKS, MobileInputSink, MobileNetwork, MobileNetworkConfig, MobileNetworkError,
    MobileNetworkLink, MobilePlatformFacts, MobileProcess, NetworkDiagnosticSink, decode_group_state,
};
pub use rich_text::ClipboardRich;
pub use store::{RichText, StoreError, TransferStore};
pub use v2_session::{
    ClipboardLowOperation, CurrentClipboard, CurrentClipboardImage, CurrentClipboardText, DirectoryEntry,
    FreshClipboardObserver, MAX_CURRENT_IMAGE_BYTES, MobileV2Identity, MobileV2Link, MobileV2Session,
    MobileV2TransportError, TransferEvent, TransferEventSink,
};

/// A network operation produced by the domain layer.  An adapter owned by
/// the protocol/network module maps this to a v2 `Message` variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportRequest {
    CurrentQuery {
        request_id: Id16,
        peer_id: String,
        max_items: u16,
    },
    Broadcast {
        payload: SharePayload,
    },
    ProxyShare {
        proxy_id: String,
        payload: SharePayload,
    },
    /// Phone to phone: only `target_id` keeps it (protocol-v2 5.2).
    DirectShare {
        target_id: String,
        payload: SharePayload,
    },
    Download {
        operation_id: Id16,
        message_id: Id16,
        item_id: u32,
        representation_id: u32,
        source_id: String,
        content_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportError {
    pub message: String,
}

impl TransportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
pub use input_router::{
    ControlEdge, ControlError, ControlObserver, ControlSettings, ControlState, ControlTarget, ExitVolumeKey,
    FingerPhase, InputNetwork, InputRouter, PadTouch, PadTouches, ScreenSink,
};
