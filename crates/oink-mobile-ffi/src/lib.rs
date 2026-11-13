//! UniFFI boundary for the v2 mobile owner.
//!
//! The session types in this crate are local typed projections. Network
//! traffic remains owned by the v2 adapter and is never represented by a
//! second FFI wire format.

uniffi::setup_scaffolding!();

#[uniffi::export]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

use oink_mobile_core::{DeliveryFailure, DeliveryState, PageCursor, TransferStore};
use std::sync::{Arc, Mutex};

mod adb_local;
mod android_input;
mod audio;
mod audio_config;
mod device_only;
mod diagnostics;
mod exit_volume;
mod forward;
mod logging;
mod media_session;
mod microphone_controls;
mod microphone_transport;
mod mobile_api;
mod playout_volume;
mod source_transport;
mod speaker_transport;
pub use adb_local::{adb_pair_local, adb_public_key, adb_start_local};
pub use android_input::*;
pub use diagnostics::MobileDiagnosticCollector;
pub use exit_volume::{MobileExitVolumeEndpoint, MobileExitVolumeReading, MobileExitVolumeStatus, MobileVolumeKey};
pub use media_session::*;
pub use mobile_api::*;
mod transfer;
pub use transfer::{MobilePeer, MobileShareItem, MobileShareSensitivity};

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MobileCoreError {
    #[error("storage error: {detail}")]
    Storage { detail: String },
    #[error("invalid argument: {detail}")]
    InvalidArgument { detail: String },
}

/// The rich text kept next to a text message's plain text (2026-11-12).
/// The plain text stays `MobileMessage.text`; 复制文本 (copy plain) writes
/// only that. A message whose rich text keeps no formatting once cleaned
/// has none (`transfer_rich_text` is `None`): it is shown and copied as
/// plain text, without 复制文本.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MobileRichText {
    /// Cleaned markup for the bubble (`oink_mobile_core::rich_text`, the
    /// transfer page spec「富文本气泡」): `b i u s code pre a h1–h6 ul ol li
    /// blockquote hr table tr th td p div br` and `img` (its `alt` only),
    /// with no attribute but `a href` (http, https, mailto, tel), `ol
    /// start` and `img alt`. No script, style, form, frame, image source or
    /// other remote resource survives. Render it as markup only (never in a
    /// web view that runs scripts), in the bubble's own font and colour;
    /// open a link only on tap.
    pub display_html: String,
    /// HTML for the clipboard (复制, copy rich): the source HTML, or HTML
    /// converted from RTF when only RTF was kept. Android writes it with
    /// `ClipData.newHtmlText`; iOS as `public.html`.
    pub html: String,
    /// The source RTF when that is what was kept; iOS also writes it as
    /// `public.rtf`.
    pub rtf: Option<String>,
}

impl MobileRichText {
    pub(crate) fn read(rich: &oink_mobile_core::RichText) -> Option<Self> {
        let clipboard = oink_mobile_core::rich_text::clipboard(rich)?;
        Some(Self {
            display_html: oink_mobile_core::rich_text::display_html(rich)?,
            html: clipboard.html,
            rtf: clipboard.rtf,
        })
    }
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct MobileMessage {
    #[uniffi(default=None)]
    pub history_order: Option<String>,
    pub id: i64,
    pub created_ms: i64,
    pub sender_id: String,
    pub proxy_id: Option<String>,
    pub content_id: String,
    pub kind: String,
    pub display_name: String,
    pub mime: Option<String>,
    pub size: u64,
    pub text: Option<String>,
    /// "Html" / "Rtf". On a text message: it keeps rich text next to its
    /// plain text (`transfer_rich_text`), shown formatted and copied by 复制.
    pub rich_text_format: Option<String>,
    pub download_state: String,
    pub local_path: Option<String>,
    pub source_online: bool,
    pub source_availability: String,
    /// Records of one share (one offer received, or one send) carry the same
    /// operation id; hosts show such a run of attachments as one batch
    /// (stacked cards). Several files of one offer are separate records.
    pub operation_id: Vec<u8>,
    pub message_id: Vec<u8>,
    pub item_id: u32,
    pub representation_id: u32,
    /// Folder message owning this record when it is a file downloaded from
    /// inside a folder; such records never appear in `page`.
    pub parent_id: Option<i64>,
    /// Listing node of a folder child (matches `MobileDirectoryEntry.id`).
    pub directory_node: Option<u64>,
    /// File count of an own-sent folder message when known (packaged file
    /// count). Render "N 个文件" from it instead of `display_name`.
    pub item_count: Option<u32>,
    /// True for messages this device sent (right-hand bubble).
    pub is_outgoing: bool,
    /// Send state of an own message; `None` for received messages.
    pub delivery_state: Option<MobileDeliveryState>,
    pub delivery_failure: Option<MobileDeliveryFailure>,
    /// Technical failure detail for diagnostics, not for display.
    pub delivery_detail: Option<String>,
    /// Oink clip marker (`oink_foundation::clipboard_marker`): when the user
    /// copies this message, the host writes these bytes next to the content,
    /// unchanged. None for a message without a clip identity (direct share).
    pub clip_marker: Option<Vec<u8>>,
    /// The source PC marked this content as a password or other secret
    /// (macOS ConcealedType, Windows ExcludeClipboardContentFromMonitorProcessing,
    /// ...). Hosts show it as `***` until the user taps "show" (fetching the
    /// body with the ordinary download call when it is not here yet), never
    /// put it in notifications or other previews, and copy it to the phone
    /// clipboard with the system's sensitive marker. It is never downloaded
    /// automatically.
    pub sensitive: bool,
    /// Thumbnail request state: "None", "Loading", "Ready", "Expired",
    /// "Unsupported" or "Failed" (protocol-v2 §4.4).
    pub thumbnail_state: String,
    /// Preview file (JPEG or PNG) when `thumbnail_state` is "Ready".
    pub thumbnail_path: Option<String>,
    /// Pixel size of the preview; its aspect ratio is the original's.
    pub thumbnail_width: Option<u32>,
    pub thumbnail_height: Option<u32>,
    /// What an image message shows; `None` for other kinds. Both hosts
    /// render exactly these states.
    pub image_preview: Option<MobileImagePreview>,
    /// The other phone of a phone-to-phone transfer: the target of an own
    /// message ("已发送给 X"), the sender of a received one. None for
    /// broadcast and PC-directed messages.
    #[uniffi(default = None)]
    pub direct_peer: Option<String>,
    /// The target's name when an own phone-to-phone message was sent.
    #[uniffi(default = None)]
    pub direct_peer_name: Option<String>,
    /// The pixel size whose aspect ratio an image message is shown at (220
    /// wide, ratio clamped to 0.75–2): the original's, as the host gave it,
    /// for an own image; the thumbnail's for a received one once it is
    /// ready. None while unknown (260 : 146 placeholder) and for other kinds.
    #[uniffi(default = None)]
    pub image_width: Option<u32>,
    #[uniffi(default = None)]
    pub image_height: Option<u32>,
    /// The download button of a received message (file card, folder page
    /// row); None for own messages.
    #[uniffi(default = None)]
    pub download_status: Option<MobileDownloadStatus>,
}

/// Download button state of a received message (core `DownloadStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MobileDownloadStatus {
    /// Not downloaded: the download button (`transfer_download`).
    NotDownloaded,
    /// Downloading from a PC or receiving phone to phone: the progress ring.
    Downloading,
    /// On this phone: the check mark.
    Downloaded,
    /// 「下载失败」: a download from a PC failed. Retry with the same call as
    /// the download button.
    DownloadFailed,
    /// A phone-to-phone receive failed (`delivery_failure` says why); it
    /// cannot be retried.
    ReceiveFailed,
    /// A phone-to-phone receive cancelled by either end (「已取消」).
    Cancelled,
    /// 「来源设备离线」: the download button, disabled.
    SourceOffline,
    /// 「内容已失效」: no button, tapping does nothing.
    Expired,
}

impl From<oink_mobile_core::DownloadStatus> for MobileDownloadStatus {
    fn from(value: oink_mobile_core::DownloadStatus) -> Self {
        use oink_mobile_core::DownloadStatus;
        match value {
            DownloadStatus::NotDownloaded => Self::NotDownloaded,
            DownloadStatus::Downloading => Self::Downloading,
            DownloadStatus::Downloaded => Self::Downloaded,
            DownloadStatus::DownloadFailed => Self::DownloadFailed,
            DownloadStatus::ReceiveFailed => Self::ReceiveFailed,
            DownloadStatus::Cancelled => Self::Cancelled,
            DownloadStatus::SourceOffline => Self::SourceOffline,
            DownloadStatus::Expired => Self::Expired,
        }
    }
}

/// Presentation state of an image message (core `ImagePreview`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MobileImagePreview {
    /// The downloaded original at `local_path`.
    Original,
    /// The thumbnail at `thumbnail_path`; the original may be downloading.
    Thumbnail,
    /// Waiting for a thumbnail or the automatic download.
    Loading,
    /// 「图片已失效」: the source no longer has the original. Opening does nothing.
    Expired,
    /// 「暂时无法加载」: the source device is offline.
    Offline,
    /// The thumbnail or download failed; opening the image retries the download.
    Failed,
    /// No preview yet; call `transfer_request_thumbnail` when visible,
    /// opening downloads the original.
    Placeholder,
}

impl From<oink_mobile_core::ImagePreview> for MobileImagePreview {
    fn from(value: oink_mobile_core::ImagePreview) -> Self {
        use oink_mobile_core::ImagePreview;
        match value {
            ImagePreview::Original => Self::Original,
            ImagePreview::Thumbnail => Self::Thumbnail,
            ImagePreview::Loading => Self::Loading,
            ImagePreview::Expired => Self::Expired,
            ImagePreview::Offline => Self::Offline,
            ImagePreview::Failed => Self::Failed,
            ImagePreview::Placeholder => Self::Placeholder,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MobileDeliveryState {
    Sending,
    Sent,
    Failed,
}

/// Why an own message failed; hosts map each case to a localized reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MobileDeliveryFailure {
    /// No PC (proxy) or clipboard device was online.
    NoTarget,
    /// The target never answered the share.
    NotAccepted,
    /// The target declined it (unsupported content, receiving disabled, ...).
    Rejected,
    /// The target received it but could not publish it.
    PeerFailed,
    /// The connection ended during the share (or the app was closed).
    Interrupted,
    /// The content could not be read or packaged on this phone.
    Local,
    /// This phone ran out of storage while preparing the share.
    StorageFull,
    /// The target paused clipboard sharing.
    Paused,
    /// The user cancelled (`transfer_cancel_share`; on a phone-to-phone
    /// transfer also the other end's user).
    Cancelled,
    /// A phone-to-phone transfer lost the other phone (it disconnected, or
    /// its app was suspended or closed). 「对方已断开」.
    PeerDisconnected,
}

impl From<DeliveryState> for MobileDeliveryState {
    fn from(value: DeliveryState) -> Self {
        match value {
            DeliveryState::Sending => Self::Sending,
            DeliveryState::Sent => Self::Sent,
            DeliveryState::Failed => Self::Failed,
        }
    }
}

impl From<DeliveryFailure> for MobileDeliveryFailure {
    fn from(value: DeliveryFailure) -> Self {
        match value {
            DeliveryFailure::NoTarget => Self::NoTarget,
            DeliveryFailure::NotAccepted => Self::NotAccepted,
            DeliveryFailure::Rejected => Self::Rejected,
            DeliveryFailure::PeerFailed => Self::PeerFailed,
            DeliveryFailure::Interrupted => Self::Interrupted,
            DeliveryFailure::Local => Self::Local,
            DeliveryFailure::StorageFull => Self::StorageFull,
            DeliveryFailure::Cancelled => Self::Cancelled,
            DeliveryFailure::Paused => Self::Paused,
            DeliveryFailure::PeerDisconnected => Self::PeerDisconnected,
        }
    }
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct MobilePage {
    pub messages: Vec<MobileMessage>,
    pub next_created_ms: Option<i64>,
    pub next_id: Option<i64>,
}

/// One layout on both platforms; hosts get the attachment size from
/// `MobileTransferStore.cached_bytes` instead of walking these directories.
pub(crate) fn transfer_store_paths(data_dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    (data_dir.join("transfer.sqlite"), data_dir.join("transfers"))
}

#[derive(Debug, uniffi::Object)]
pub struct MobileTransferStore {
    inner: Mutex<TransferStore>,
}

#[uniffi::export]
impl MobileTransferStore {
    #[uniffi::constructor]
    pub fn open(database_path: String, files_root: String) -> Result<Arc<Self>, MobileCoreError> {
        let inner = TransferStore::open(database_path, files_root).map_err(storage_error)?;
        Ok(Arc::new(Self {
            inner: Mutex::new(inner),
        }))
    }

    /// Open the same transfer history and attachment directory as MobileSession.
    #[uniffi::constructor]
    pub fn open_in_session(data_dir: String, _platform: MobilePlatform) -> Result<Arc<Self>, MobileCoreError> {
        let (database, attachments) = transfer_store_paths(std::path::Path::new(&data_dir));
        let inner = TransferStore::open(database, attachments).map_err(storage_error)?;
        Ok(Arc::new(Self {
            inner: Mutex::new(inner),
        }))
    }

    pub fn page(
        &self,
        cursor_created_ms: Option<i64>,
        cursor_id: Option<i64>,
        limit: u32,
    ) -> Result<MobilePage, MobileCoreError> {
        let cursor = match (cursor_created_ms, cursor_id) {
            (None, None) => None,
            (Some(created_ms), Some(id)) => Some(PageCursor { created_ms, id }),
            _ => {
                return Err(MobileCoreError::InvalidArgument {
                    detail: "cursor requires created_ms and id".into(),
                });
            }
        };
        let page = self
            .inner
            .lock()
            .map_err(|_| MobileCoreError::Storage {
                detail: "store lock poisoned".into(),
            })?
            .page(cursor, limit as usize)
            .map_err(storage_error)?;
        let (next_created_ms, next_id) = page.next.map_or((None, None), |c| (Some(c.created_ms), Some(c.id)));
        Ok(MobilePage {
            messages: page.messages.into_iter().map(Into::into).collect(),
            next_created_ms,
            next_id,
        })
    }

    /// One record by id (a conversation message or a folder child).
    pub fn message(&self, message_id: i64) -> Result<Option<MobileMessage>, MobileCoreError> {
        Ok(self.lock()?.by_id(message_id).map_err(storage_error)?.map(Into::into))
    }

    /// The rich text kept with text message `message_id` (same as
    /// `MobileSession.transfer_rich_text`).
    pub fn rich_text(&self, message_id: i64) -> Result<Option<MobileRichText>, MobileCoreError> {
        Ok(self
            .lock()?
            .rich_text(message_id)
            .map_err(storage_error)?
            .as_ref()
            .and_then(MobileRichText::read))
    }

    /// Files downloaded from inside folder message `folder_message_id`, so a
    /// folder viewer can show each entry's download state and local path
    /// (match `directory_node` with the listing entry id).
    pub fn folder_children(&self, folder_message_id: i64) -> Result<Vec<MobileMessage>, MobileCoreError> {
        Ok(self
            .lock()?
            .folder_children(folder_message_id)
            .map_err(storage_error)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub fn delete_message(&self, message_id: i64) -> Result<(), MobileCoreError> {
        self.inner
            .lock()
            .map_err(|_| MobileCoreError::Storage {
                detail: "store lock poisoned".into(),
            })?
            .delete_message(message_id)
            .map_err(storage_error)
    }

    pub fn clear_cached_files(&self) -> Result<(), MobileCoreError> {
        self.inner
            .lock()
            .map_err(|_| MobileCoreError::Storage {
                detail: "store lock poisoned".into(),
            })?
            .clear_cached_files()
            .map_err(storage_error)
    }

    /// Bytes of every attachment "清理缓存" would delete (received
    /// downloads and copies of own sent images/files).
    pub fn cached_bytes(&self) -> Result<u64, MobileCoreError> {
        self.lock()?.cached_bytes().map_err(storage_error)
    }

    /// "清理传输记录" without a session (`MobileSession.transfer_clear_history`
    /// while one runs): deletes every finished record and its files. Nothing
    /// pushes an event here; reload the page afterwards.
    pub fn clear_history(&self) -> Result<(), MobileCoreError> {
        self.lock()?
            .clear_history(&std::collections::HashSet::new())
            .map(drop)
            .map_err(storage_error)
    }
}

impl MobileTransferStore {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, TransferStore>, MobileCoreError> {
        self.inner.lock().map_err(|_| MobileCoreError::Storage {
            detail: "store lock poisoned".into(),
        })
    }
}

fn storage_error(error: impl std::fmt::Display) -> MobileCoreError {
    MobileCoreError::Storage {
        detail: error.to_string(),
    }
}

impl From<oink_mobile_core::MessageRecord> for MobileMessage {
    fn from(value: oink_mobile_core::MessageRecord) -> Self {
        let directory_node = value.directory_node();
        let clip_marker = value.clip_marker();
        let image_preview = value.image_preview().map(Into::into);
        let image_size = value.image_size();
        let download_status = value.download_status().map(Into::into);
        Self {
            history_order: value.history_order,
            clip_marker,
            image_preview,
            image_width: image_size.map(|(width, _)| width),
            image_height: image_size.map(|(_, height)| height),
            download_status,
            thumbnail_state: format!("{:?}", value.thumbnail),
            thumbnail_path: value.thumbnail_path,
            thumbnail_width: value.thumbnail_width,
            thumbnail_height: value.thumbnail_height,
            parent_id: value.parent_id,
            directory_node,
            item_count: value.item_count,
            is_outgoing: value.outgoing,
            delivery_state: value.delivery.map(Into::into),
            delivery_failure: value.delivery_failure.map(Into::into),
            delivery_detail: value.delivery_detail,
            id: value.id,
            created_ms: value.created_ms,
            sender_id: value.sender_id,
            proxy_id: value.proxy_id,
            content_id: value.content_id,
            kind: format!("{:?}", value.kind),
            display_name: value.display_name,
            mime: value.mime,
            size: value.size,
            text: value.text,
            rich_text_format: value.rich_text_format.map(|x| format!("{:?}", x)),
            download_state: format!("{:?}", value.download),
            local_path: value.local_path,
            source_online: value.source_online,
            source_availability: format!("{:?}", value.source_availability),
            operation_id: value.operation_id.to_vec(),
            message_id: value.message_id.to_vec(),
            item_id: value.item_id,
            representation_id: value.representation_id,
            sensitive: value.sensitive,
            direct_peer: value.direct_peer,
            direct_peer_name: value.direct_peer_name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::{
        DeviceId, clipboard_marker,
        content::{ClipId, OfferId},
    };
    use oink_mobile_core::{AttachmentKind, DownloadState, MessageRecord, SourceAvailability};

    fn record(content_id: String) -> MessageRecord {
        MessageRecord {
            history_order: None,
            id: 1,
            created_ms: 0,
            sender_id: "pc".into(),
            proxy_id: None,
            content_id,
            kind: AttachmentKind::Text,
            display_name: String::new(),
            mime: None,
            size: 4,
            text: Some("text".into()),
            rich_text_format: None,
            download: DownloadState::Complete,
            local_path: None,
            source_online: true,
            source_availability: SourceAvailability::Ready,
            operation_id: [0; 16],
            message_id: [0; 16],
            item_id: 0,
            representation_id: 0,
            parent_id: None,
            item_count: None,
            outgoing: false,
            delivery: None,
            delivery_failure: None,
            delivery_detail: None,
            sensitive: false,
            thumbnail: Default::default(),
            thumbnail_path: None,
            thumbnail_width: None,
            thumbnail_height: None,
            direct_peer: None,
            direct_peer_name: None,
            image_width: None,
            image_height: None,
        }
    }

    /// Hosts read one aspect source and one button state off the message.
    #[test]
    fn message_carries_the_image_size_and_the_download_status() {
        let mut own = record("out-1-0".into());
        own.kind = AttachmentKind::Image;
        own.outgoing = true;
        (own.image_width, own.image_height) = (Some(3024), Some(4032));
        let own = MobileMessage::from(own);
        assert_eq!((own.image_width, own.image_height), (Some(3024), Some(4032)));
        assert_eq!(own.download_status, None);

        let mut received = record("in-1".into());
        received.kind = AttachmentKind::Image;
        received.download = DownloadState::Failed;
        (received.thumbnail_width, received.thumbnail_height) = (Some(512), Some(384));
        let received = MobileMessage::from(received);
        assert_eq!((received.image_width, received.image_height), (Some(512), Some(384)));
        assert_eq!(received.download_status, Some(MobileDownloadStatus::DownloadFailed));

        let mut direct = record("in-2".into());
        direct.download = DownloadState::Failed;
        direct.direct_peer = Some("tablet".into());
        direct.source_availability = SourceAvailability::NoSuchItem;
        direct.delivery_failure = Some(DeliveryFailure::PeerDisconnected);
        let direct = MobileMessage::from(direct);
        assert_eq!(direct.download_status, Some(MobileDownloadStatus::ReceiveFailed));
        assert_eq!((direct.image_width, direct.image_height), (None, None), "not an image");
    }

    #[test]
    fn message_carries_the_clip_marker_of_its_offer_and_none_without_one() {
        let origin = "07".repeat(32);
        let message = MobileMessage::from(record(format!("v2:{origin}:9:3:r:0:1")));
        let expected = clipboard_marker::encode(OfferId {
            clip: ClipId {
                origin: DeviceId([7; 32]),
                seq: 9,
            },
            generation: 3,
        });
        assert!(expected.is_some());
        assert_eq!(message.clip_marker, expected);
        // A direct share has no clip identity.
        let shared = MobileMessage::from(record(format!("text-{origin}-{}", "05".repeat(16))));
        assert_eq!(shared.clip_marker, None);
    }
}
