use serde::{Deserialize, Serialize};

pub type MessageId = i64;
/// Wire adapters replace these opaque bytes with the protocol v2 `Id16` type.
pub type Id16 = [u8; 16];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachmentKind {
    Text,
    Image,
    File,
    Folder,
    RichText,
}

impl AttachmentKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::File => "file",
            Self::Folder => "folder",
            Self::RichText => "rich_text",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "text" => Self::Text,
            "image" => Self::Image,
            "file" => Self::File,
            "folder" => Self::Folder,
            "rich_text" => Self::RichText,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RichTextFormat {
    Html,
    Rtf,
}

impl RichTextFormat {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Rtf => "rtf",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadState {
    Placeholder,
    Downloading,
    Complete,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceAvailability {
    Ready,
    SourceOffline,
    Evicted,
    NoSuchItem,
    /// The source replaced this content with a newer one.
    Superseded,
    Unsupported,
}

impl SourceAvailability {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::SourceOffline => "source_offline",
            Self::Evicted => "evicted",
            Self::NoSuchItem => "no_such_item",
            Self::Superseded => "superseded",
            Self::Unsupported => "unsupported",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "ready" => Self::Ready,
            "source_offline" => Self::SourceOffline,
            "evicted" => Self::Evicted,
            "no_such_item" => Self::NoSuchItem,
            "superseded" => Self::Superseded,
            "unsupported" => Self::Unsupported,
            _ => return None,
        })
    }
}

impl DownloadState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Placeholder => "placeholder",
            Self::Downloading => "downloading",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "placeholder" => Self::Placeholder,
            "downloading" => Self::Downloading,
            "complete" => Self::Complete,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// Preview of an image message that is not downloaded (protocol-v2 §4.4).
/// Only independent received image messages above the automatic download
/// threshold (or of unknown size) have one; it is requested lazily when the
/// message becomes visible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThumbnailState {
    /// Not requested (yet), or an earlier request was interrupted.
    #[default]
    None,
    Loading,
    /// `thumbnail_path` holds the preview.
    Ready,
    /// The source no longer has the original (24 hours passed, it restarted
    /// without it, or its cache was cleared). Final.
    Expired,
    /// The source has no decoder for the image or no preview fits. Final;
    /// the original can still be downloaded.
    Unsupported,
    /// The request failed (link loss, read error); requested again on the
    /// next visibility.
    Failed,
}

impl ThumbnailState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Expired => "expired",
            Self::Unsupported => "unsupported",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "none" => Self::None,
            "loading" => Self::Loading,
            "ready" => Self::Ready,
            "expired" => Self::Expired,
            "unsupported" => Self::Unsupported,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// What an image message shows, derived from its download, thumbnail and
/// source state, so iOS and Android present the same states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImagePreview {
    /// The downloaded original (`local_path`).
    Original,
    /// The thumbnail (`thumbnail_path`); the original may be downloading.
    Thumbnail,
    /// Waiting for a thumbnail or for the automatic download.
    Loading,
    /// 「图片已失效」: the source no longer has the original.
    Expired,
    /// 「暂时无法加载」: the source device is offline.
    Offline,
    /// The thumbnail or download failed; opening the image retries.
    Failed,
    /// Nothing to show yet (no preview available); opening downloads it.
    Placeholder,
}

/// The download button state of a received message (file card, folder page
/// row), derived from its download and source state, so iOS and Android
/// tell a retryable failed download from a phone-to-phone receive that
/// cannot be fetched again (spec 2026-11-11 「下载失败」 vs 「接收失败」).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadStatus {
    /// Not downloaded: the download button.
    NotDownloaded,
    /// Downloading from a PC, or receiving phone to phone.
    Downloading,
    /// The body is on this phone.
    Downloaded,
    /// 「下载失败」: a download from a PC failed; downloading again retries.
    DownloadFailed,
    /// A phone-to-phone receive failed (`delivery_failure` says why). Its
    /// sender keeps nothing to serve, so it cannot be retried.
    ReceiveFailed,
    /// A phone-to-phone receive cancelled by either end (「已取消」).
    Cancelled,
    /// 「来源设备离线」: nothing can be fetched until the source is back.
    SourceOffline,
    /// 「内容已失效」: the source no longer has it (or it was cleared here
    /// and cannot be fetched again).
    Expired,
}

/// Delivery of a message this device sent. Incoming records have no delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryState {
    Sending,
    Sent,
    Failed,
}

impl DeliveryState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "sending" => Self::Sending,
            "sent" => Self::Sent,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// Why a send failed. This is the UI-facing reason; the record's
/// `delivery_detail` keeps the technical detail for diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryFailure {
    /// No ready PC (proxy) or clipboard peer was available when sending.
    NoTarget,
    /// The target never accepted the share within the acceptance window.
    NotAccepted,
    /// The target declined the share (unsupported, disabled, too large, ...).
    Rejected,
    /// The target received the share but could not publish it.
    PeerFailed,
    /// The link or stream ended while the share was in flight, or the app
    /// session ended before a result arrived.
    Interrupted,
    /// Local preparation failed (reading or packaging the shared content).
    Local,
    /// This phone ran out of storage while preparing the share.
    StorageFull,
    /// The target paused clipboard sharing.
    Paused,
    /// The user cancelled the share (on a phone-to-phone transfer: either
    /// end's user).
    Cancelled,
    /// A phone-to-phone transfer lost the other phone: its link went down
    /// (network, or its app was suspended or closed). Not resumed.
    PeerDisconnected,
}

impl DeliveryFailure {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NoTarget => "no_target",
            Self::NotAccepted => "not_accepted",
            Self::Rejected => "rejected",
            Self::PeerFailed => "peer_failed",
            Self::Interrupted => "interrupted",
            Self::Local => "local",
            Self::StorageFull => "storage_full",
            Self::Cancelled => "cancelled",
            Self::Paused => "paused",
            Self::PeerDisconnected => "peer_disconnected",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "no_target" => Self::NoTarget,
            "not_accepted" => Self::NotAccepted,
            "rejected" => Self::Rejected,
            "peer_failed" => Self::PeerFailed,
            "interrupted" => Self::Interrupted,
            "local" => Self::Local,
            "storage_full" => Self::StorageFull,
            "cancelled" => Self::Cancelled,
            "paused" => Self::Paused,
            "peer_disconnected" => Self::PeerDisconnected,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRecord {
    /// Stable shared order when display times tie; local/direct records have none.
    pub history_order: Option<String>,
    pub id: MessageId,
    pub created_ms: i64,
    pub sender_id: String,
    pub proxy_id: Option<String>,
    pub content_id: String,
    pub kind: AttachmentKind,
    pub display_name: String,
    pub mime: Option<String>,
    pub size: u64,
    pub text: Option<String>,
    /// A `RichText` record's format. On a `Text` record: the format of the
    /// rich text kept next to the plain text (2026-11-12), a hidden child
    /// record (`TransferStore::rich_text`); None when the text has none.
    pub rich_text_format: Option<RichTextFormat>,
    pub download: DownloadState,
    pub local_path: Option<String>,
    pub source_online: bool,
    pub source_availability: SourceAvailability,
    pub operation_id: Id16,
    pub message_id: Id16,
    pub item_id: u32,
    pub representation_id: u32,
    /// Set for a file downloaded from inside a folder message, and for the
    /// rich text of a text message. Such records never appear in the
    /// conversation page; they belong to `parent_id`.
    pub parent_id: Option<MessageId>,
    /// Number of files of an own-sent folder message (its packaged file
    /// count), when known. Hosts localize the "N 个文件" label from it.
    pub item_count: Option<u32>,
    /// True for messages this device sent.
    pub outgoing: bool,
    pub delivery: Option<DeliveryState>,
    pub delivery_failure: Option<DeliveryFailure>,
    pub delivery_detail: Option<String>,
    /// The source marked this content as a password or other secret
    /// (ClipboardOffer sensitivity other than Normal: macOS ConcealedType,
    /// Windows ExcludeClipboardContentFromMonitorProcessing, ...). Hosts hide
    /// it until the user shows it, never put it in previews, and write it to
    /// the phone clipboard with the system's own sensitive marker.
    pub sensitive: bool,
    pub thumbnail: ThumbnailState,
    /// Preview file (JPEG or PNG) when `thumbnail == Ready`.
    pub thumbnail_path: Option<String>,
    /// Pixel size of the preview; its aspect ratio is the original's.
    pub thumbnail_width: Option<u32>,
    pub thumbnail_height: Option<u32>,
    /// The other phone of a phone-to-phone transfer: the target of an own
    /// message, the sender of a received one. None otherwise.
    pub direct_peer: Option<String>,
    /// The target's name when an own phone-to-phone message was sent.
    pub direct_peer_name: Option<String>,
    /// Pixel size of the original of an own sent image as it is displayed
    /// (orientation applied), as the host read it when sharing. None for
    /// everything else; see `image_size`.
    pub image_width: Option<u32>,
    pub image_height: Option<u32>,
}

impl MessageRecord {
    /// The size whose aspect ratio an image message is shown at: the
    /// original's for an own image, the thumbnail's (same ratio) for a
    /// received one once it is ready; None while unknown.
    pub fn image_size(&self) -> Option<(u32, u32)> {
        if self.kind != AttachmentKind::Image {
            return None;
        }
        match (self.image_width, self.image_height) {
            (Some(width), Some(height)) => Some((width, height)),
            _ => self.thumbnail_width.zip(self.thumbnail_height),
        }
    }

    /// The download button state of a received message (None for own
    /// messages). Order matters: a body on this phone wins, then a running
    /// transfer, then a phone-to-phone failure (its source is gone for good,
    /// which is not "expired content"), then a source that no longer has the
    /// content, then an offline source, and only then a failed download,
    /// which can be retried.
    pub fn download_status(&self) -> Option<DownloadStatus> {
        if self.outgoing {
            return None;
        }
        Some(match self.download {
            DownloadState::Complete => DownloadStatus::Downloaded,
            DownloadState::Downloading => DownloadStatus::Downloading,
            DownloadState::Failed if self.direct_peer.is_some() => {
                if self.delivery_failure == Some(DeliveryFailure::Cancelled) {
                    DownloadStatus::Cancelled
                } else {
                    DownloadStatus::ReceiveFailed
                }
            }
            _ if matches!(
                self.source_availability,
                SourceAvailability::NoSuchItem
                    | SourceAvailability::Evicted
                    | SourceAvailability::Superseded
                    | SourceAvailability::Unsupported
            ) =>
            {
                DownloadStatus::Expired
            }
            _ if self.source_availability == SourceAvailability::SourceOffline => DownloadStatus::SourceOffline,
            DownloadState::Failed => DownloadStatus::DownloadFailed,
            DownloadState::Placeholder => DownloadStatus::NotDownloaded,
        })
    }

    /// The state an image message presents (None for other kinds).
    pub fn image_preview(&self) -> Option<ImagePreview> {
        if self.kind != AttachmentKind::Image {
            return None;
        }
        let original = self.download == DownloadState::Complete && self.local_path.is_some();
        Some(if original {
            ImagePreview::Original
        } else if matches!(
            self.source_availability,
            SourceAvailability::NoSuchItem | SourceAvailability::Evicted | SourceAvailability::Superseded
        ) || self.thumbnail == ThumbnailState::Expired
        {
            ImagePreview::Expired
        } else if matches!(
            self.thumbnail,
            ThumbnailState::Ready | ThumbnailState::Loading | ThumbnailState::Failed
        ) && self.thumbnail_path.is_some()
        {
            ImagePreview::Thumbnail
        } else if self.source_availability == SourceAvailability::SourceOffline {
            ImagePreview::Offline
        } else if self.thumbnail == ThumbnailState::Loading || self.download == DownloadState::Downloading {
            ImagePreview::Loading
        } else if self.thumbnail == ThumbnailState::Failed || self.download == DownloadState::Failed {
            ImagePreview::Failed
        } else {
            ImagePreview::Placeholder
        })
    }

    /// Directory node of a folder child record, parsed from its v2 identity.
    pub fn directory_node(&self) -> Option<u64> {
        self.parent_id?;
        let (_, suffix) = self.content_id.split_once(":f:")?;
        suffix.split_once(':')?.0.parse().ok()
    }

    /// Oink clip marker (`oink_foundation::clipboard_marker`) a host writes
    /// next to this record's content when the user copies it from the
    /// conversation: the identity of the offer it came from. None for a
    /// record without a clip identity (a direct share).
    pub fn clip_marker(&self) -> Option<Vec<u8>> {
        crate::v2_session::clip_marker(&self.content_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageCursor {
    pub created_ms: i64,
    pub id: MessageId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub messages: Vec<MessageRecord>,
    pub next: Option<PageCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerKind {
    Pc,
    Mobile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerState {
    Ready,
    Offline,
    Connecting,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    pub kind: PeerKind,
    pub state: PeerState,
    pub accepts_clipboard: bool,
    pub connection_order: u64,
    /// The link runs through a PC relay station. A direct peer is preferred
    /// as proxy or current-query target; a relayed one still works.
    #[serde(default)]
    pub relayed: bool,
    /// The peer's DeviceFacts (its pause) have not arrived yet,
    /// so `accepts_clipboard` only reflects the negotiation. Such a peer ranks
    /// after peers known to accept content (protocol-v2 5.2, review M17).
    #[serde(default)]
    pub clipboard_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceState {
    pub source_id: String,
    pub availability: SourceAvailability,
}

impl SourceState {
    pub fn is_online(&self) -> bool {
        self.availability == SourceAvailability::Ready
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingAnnouncement {
    pub sender_id: String,
    pub proxy_id: Option<String>,
    pub content_id: String,
    pub kind: AttachmentKind,
    pub display_name: String,
    pub mime: Option<String>,
    pub size: u64,
    pub text: Option<String>,
    pub rich_text_format: Option<RichTextFormat>,
    pub source: SourceState,
    pub operation_id: Id16,
    pub message_id: Id16,
    pub item_id: u32,
    pub representation_id: u32,
    /// See `MessageRecord::sensitive`.
    pub sensitive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendItem {
    Text {
        text: String,
    },
    /// `staged`: the host's own staged copy, on the same volume as the
    /// attachment store. The core takes it over (moves it into the store)
    /// instead of copying it; the host no longer deletes it once handed over.
    /// Otherwise (e.g. a document-picker file) the core copies the file.
    /// `dimensions`: the original's displayed pixel size (orientation
    /// applied) as the host read it; recorded on the own message so both
    /// apps show it at its aspect ratio.
    Image {
        path: String,
        encoded_size: u64,
        mime: String,
        staged: bool,
        dimensions: Option<(u32, u32)>,
    },
    File {
        path: String,
        name: String,
        size: u64,
        mime: Option<String>,
        staged: bool,
    },
    Folder {
        path: String,
        name: String,
    },
    /// Text with the rich formats the phone has for it (2026-11-12: phones
    /// send every format): one item whose representations are the plain
    /// text, then HTML and RTF when present. It is a text message (kind
    /// `Text`) whose rich text is kept next to it.
    RichText {
        text: String,
        html: Option<String>,
        rtf: Option<String>,
    },
}

impl SendItem {
    /// Text, as a rich text item when `html` or `rtf` is present.
    pub fn text(text: String, html: Option<String>, rtf: Option<String>) -> Self {
        if html.is_none() && rtf.is_none() {
            Self::Text { text }
        } else {
            Self::RichText { text, html, rtf }
        }
    }

    pub fn kind(&self) -> AttachmentKind {
        match self {
            Self::Text { .. } | Self::RichText { .. } => AttachmentKind::Text,
            Self::Image { .. } => AttachmentKind::Image,
            Self::File { .. } => AttachmentKind::File,
            Self::Folder { .. } => AttachmentKind::Folder,
        }
    }

    /// Bytes on the wire: every representation of the item.
    pub fn size(&self) -> u64 {
        match self {
            Self::Text { text } => text.len() as u64,
            Self::Image { encoded_size, .. } => *encoded_size,
            Self::File { size, .. } => *size,
            Self::Folder { .. } => 0,
            Self::RichText { .. } => self.representations().iter().map(|(_, body)| body.len() as u64).sum(),
        }
    }

    /// The representations of a text item in wire order (representation id =
    /// index): plain text, then HTML, then RTF. Empty for attachments.
    pub fn representations(&self) -> Vec<(TextFormat, &str)> {
        match self {
            Self::Text { text } => vec![(TextFormat::Plain, text.as_str())],
            Self::RichText { text, html, rtf } => std::iter::once((TextFormat::Plain, text.as_str()))
                .chain(html.as_deref().map(|html| (TextFormat::Html, html)))
                .chain(rtf.as_deref().map(|rtf| (TextFormat::Rtf, rtf)))
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// One representation of a text item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextFormat {
    Plain,
    Html,
    Rtf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendOrigin {
    TransferPage,
    IosShare,
    AndroidShare,
    AndroidOpenWith,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharePayload {
    pub sensitivity: oink_foundation::clipboard::Sensitivity,
    pub items: Vec<SendItem>,
    pub origin: SendOrigin,
    pub operation_id: Id16,
    pub message_id: Id16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyChoice {
    Automatic,
    /// A PC the user chose as proxy (preferred while online).
    Device(String),
    /// A phone the user chose: attachments go to it alone, peer to peer.
    /// `name` is recorded on the own messages ("已发送给 X").
    Direct {
        device: String,
        name: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendPlan {
    Broadcast {
        payload: SharePayload,
    },
    Proxy {
        proxy_id: String,
        payload: SharePayload,
    },
    /// Phone to phone, to `target_id` only.
    Direct {
        target_id: String,
        payload: SharePayload,
    },
}
