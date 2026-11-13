//! The mobile transfer owner for protocol v2.
//!
//! This module is the boundary between the local transfer state machine and
//! the network.  Native shells provide a byte transport only; they never
//! construct a DTO, JSON request, or private frame.  Every outbound and
//! inbound record is encoded/decoded through the one `v2::Message` enum.

mod clear_history;
mod direct;
mod fresh;
mod history;
use fresh::FreshClipboard;
pub use fresh::FreshClipboardObserver;

use crate::{
    AttachmentKind, DeliveryFailure, DeliveryState, IncomingAnnouncement, MessageRecord, Peer, SharePayload,
    SourceState, TransferCoordinator, TransferStore, Transport, TransportError, TransportRequest,
    store::{OutgoingItem, OutgoingRich, image_mime_for_name, rich_content_id, stored_file_name},
};
use oink_foundation::{DeviceId, GroupId, session::Boot};
use oink_network::link::LowStream;
use oink_protocol::v2::{
    self, RawMap, RawValue, dto,
    frame::OpaqueFrame,
    schema::{FixedBytes, WireDto, WireValue},
};
use std::{
    collections::HashMap,
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_FRAME: usize = v2::limits::RELIABLE_FRAME;
const MAX_PENDING: usize = 64;
/// How long a share may wait for the target's Accept/Reject. A started share
/// is never expired by this; it ends with the target's result, a stream
/// failure or the link going down.
const SHARE_LIFETIME: Duration = Duration::from_secs(120);
const MAX_DIRECT_TEXT: u64 = 64 * 1024;
/// Largest text body a download accepts.
const MAX_TEXT_DOWNLOAD: u64 = 1024 * 1024;
/// A text message's rich text is fetched automatically up to this size
/// (2026-11-12); a larger one is not fetched and the message stays plain.
/// Copied web pages carry markup several times their text; 4 MiB keeps
/// such a page's HTML while bounding what an automatic read costs.
const MAX_RICH_DOWNLOAD: u64 = 4 * 1024 * 1024;
const MAX_SHARE_ITEMS: u16 = 64;
const MAX_DIRECT_BYTES: u64 = MAX_DIRECT_TEXT * MAX_SHARE_ITEMS as u64;
const CONTENT_CHUNK: usize = 64 * 1024;
const MAX_FOLDER_ENTRIES: usize = 4096;
const MAX_CURRENT_QUERIES: usize = 4;
const CURRENT_TIMEOUT: Duration = Duration::from_secs(8);
/// Thumbnail request limits (protocol-v2 §4.4): long edge and encoded size.
const THUMBNAIL_EDGE: u16 = 512;
const THUMBNAIL_BYTES: u32 = 256 * 1024;

/// Largest image `receive_current_clipboard` hands to a host. The phone's
/// shortcut returns the whole image to Shortcuts, which holds it in memory
/// and puts it on the clipboard; a bigger image is reported as too large
/// (before downloading when its size is announced) instead of returned.
pub const MAX_CURRENT_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

type ShareKey = (String, [u8; 16]);
type CurrentReply = Result<Option<CurrentSelection>, MobileV2TransportError>;
/// Receiver of transfer events. It runs on the emitting thread, sometimes
/// while session locks are held: it must only hand the event on (the FFI
/// queues it for its listener task) and never call back into the session.
pub type TransferEventSink = Arc<dyn Fn(TransferEvent) + Send + Sync>;
#[derive(Default)]
struct EventSlot(Mutex<Option<TransferEventSink>>);

impl std::fmt::Debug for EventSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSlot").finish_non_exhaustive()
    }
}

/// The record a current-clipboard request answers with (see `receive_offer`).
#[derive(Clone, Copy, Debug)]
struct CurrentSelection {
    record_id: crate::MessageId,
    sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentClipboardText {
    pub text: String,
    pub sensitive: bool,
    /// The text's rich text, written next to it (2026-11-12).
    pub rich: Option<crate::ClipboardRich>,
    /// Oink clip marker of the offer this text came from; the host writes it
    /// next to the text (`oink_foundation::clipboard_marker`).
    pub marker: Option<Vec<u8>>,
}

/// A downloaded image of the group's current clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentClipboardImage {
    /// The downloaded body in the transfer store (the same file the
    /// conversation shows); the host reads it and must not move or delete it.
    pub path: String,
    pub mime: Option<String>,
    pub size: u64,
    pub sensitive: bool,
    /// Oink clip marker of the offer, as for text.
    pub marker: Option<Vec<u8>>,
}

/// The group's current clipboard as a one-tap copy takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentClipboard {
    Text(CurrentClipboardText),
    Image(CurrentClipboardImage),
    /// The image is bigger than `limit` (`MAX_CURRENT_IMAGE_BYTES`); it is
    /// not handed over.
    ImageTooLarge {
        size: u64,
        limit: u64,
    },
    /// The current content is a file, a folder or rich text only; the host
    /// points the user to the transfer page.
    Unsupported {
        kind: AttachmentKind,
    },
}

#[derive(Debug)]
struct PendingCurrent {
    deadline: Instant,
    reply: Option<tokio::sync::oneshot::Sender<CurrentReply>>,
}

#[derive(Debug)]
struct OutgoingShare {
    share: Arc<PreparedShare>,
    delivery: dto::ShareDelivery,
    created: Instant,
    started: bool,
    /// Proxy candidates after this one, in order. A rejection that only says
    /// something about this PC (receiving off, paused, busy) before it
    /// accepted moves the share to the next one (protocol-v2 5.2).
    fallback: Vec<String>,
    /// Phone to phone: once its Low stream is open the receiver decides the
    /// outcome (its ShareResult), or the link loss does.
    direct: bool,
    /// The upload's Low stream opened.
    opened: bool,
}

#[derive(Debug)]
struct PreparedShare {
    payload: SharePayload,
    folders: HashMap<u32, Arc<tempfile::NamedTempFile>>,
    /// Packaged file count per folder item, shown as the folder's item count.
    folder_files: HashMap<u32, u32>,
    total: u64,
}

impl PreparedShare {
    fn new(payload: SharePayload, staging: &Path) -> Result<Self, MobileV2TransportError> {
        if payload.items.is_empty() || payload.items.len() > usize::from(MAX_SHARE_ITEMS) {
            return Err(link_error("unsupported number of clipboard share items"));
        }
        let mut total = 0u64;
        let mut folders = HashMap::new();
        let mut folder_files = HashMap::new();
        for (index, item) in payload.items.iter().enumerate() {
            let bytes = if let crate::SendItem::Folder { path, .. } = item {
                let (archive, files) = archive_folder(Path::new(path), staging).map_err(io_failure)?;
                let len = archive.as_file().metadata().map_err(io_failure)?.len();
                folders.insert(index as u32, Arc::new(archive));
                folder_files.insert(index as u32, files);
                len
            } else {
                item.size()
            };
            total = total
                .checked_add(bytes)
                .ok_or_else(|| link_error("clipboard share size overflows"))?;
        }
        Ok(Self {
            payload,
            folders,
            folder_files,
            total,
        })
    }
}

/// Packages a shared folder (stored, no compression) for the proxy. There is
/// no size cap (user 2026-11-10): entries of 4 GiB or more are written as
/// ZIP64, and the phone's free space is the only bound.
fn archive_folder(root: &Path, staging: &Path) -> std::io::Result<(tempfile::NamedTempFile, u32)> {
    use std::io::Error;
    if !root.symlink_metadata()?.is_dir() {
        return Err(Error::other("shared folder is not a directory"));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(staging)?;
    let mut zip = zip::ZipWriter::new(temporary.as_file_mut());
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut directories = vec![root.to_path_buf()];
    let mut entries = 0;
    let mut files = 0u32;
    while let Some(directory) = directories.pop() {
        for child in fs::read_dir(directory)? {
            let child = child?;
            entries += 1;
            if entries > MAX_FOLDER_ENTRIES {
                return Err(Error::other("shared folder has too many entries"));
            }
            let path = child.path();
            let relative = path.strip_prefix(root).expect("descendant of selected folder");
            // ZIP entry names always use '/', whatever the host's separator
            // ('\' on Windows); a component may contain neither '\' nor ':'.
            let components = relative
                .components()
                .map(|component| {
                    component
                        .as_os_str()
                        .to_str()
                        .filter(|part| !part.contains(['\\', ':']))
                })
                .collect::<Option<Vec<_>>>()
                .filter(|parts| parts.len() <= 64)
                .ok_or_else(|| Error::other("unsupported shared folder name"))?;
            let name = components.join("/");
            let name = name.as_str();
            let kind = child.file_type()?;
            if kind.is_dir() {
                zip.add_directory(format!("{name}/"), options)?;
                directories.push(path);
            } else if kind.is_file() {
                let input = fs::File::open(&path)?;
                let size = input.metadata()?.len();
                zip.start_file(name, options.large_file(size >= u64::from(u32::MAX)))?;
                if std::io::copy(&mut input.take(size + 1), &mut zip)? != size {
                    return Err(Error::other("shared file changed while packaging its folder"));
                }
                files += 1;
            } else {
                return Err(Error::other("shared folder contains a symlink or special file"));
            }
        }
    }
    zip.finish()?;
    Ok((temporary, files))
}

#[derive(Clone)]
struct IncomingShare {
    sensitive: bool,
    message_id: [u8; 16],
    fresh_ticket: Option<u64>,
    source: dto::Boot,
    count: u16,
    expected_bytes: u64,
    bytes: u64,
    text: Option<Vec<u8>>,
    items: Vec<IncomingItem>,
    created: Instant,
    started: bool,
}

#[derive(Clone)]
struct IncomingItem {
    announcement: IncomingAnnouncement,
    bytes: Vec<u8>,
    complete: bool,
}

impl IncomingShare {
    fn receive_part(&mut self, peer: &str, operation_id: [u8; 16], message: v2::Message) -> Result<bool, v2::Error> {
        use v2::ErrorCode::{StateConflict, UnsupportedValue};
        match message {
            v2::Message::ClipboardShareTextChunk(chunk) => {
                if self.count != 1 || chunk.operation_id()?.0 != operation_id {
                    return Err(StateConflict.into());
                }
                let bytes = chunk.bytes()?.0;
                let text = self.text.get_or_insert_with(Vec::new);
                if chunk.offset()? != text.len() as u64
                    || bytes.is_empty()
                    || self.bytes + bytes.len() as u64 > self.expected_bytes.min(MAX_DIRECT_TEXT)
                {
                    return Err(StateConflict.into());
                }
                self.bytes += bytes.len() as u64;
                text.extend(bytes);
            }
            v2::Message::ContentOffer(offer) => {
                let offer_id = offer.offer()?;
                let size = offer.bytes()?.ok_or(UnsupportedValue)?;
                // Attachments come phone to phone (`direct`); this path only
                // carries broadcast text, with the rich text of a text item
                // as its further representations (2026-11-12).
                let item_id = offer.item_id()?;
                let representation = offer.representation_id()?;
                let rich = matches!(offer.kind()?, dto::ContentKind::Html | dto::ContentKind::Rtf);
                if rich {
                    let reps: Vec<_> = self
                        .items
                        .iter()
                        .filter(|item| item.announcement.item_id == item_id)
                        .collect();
                    let first_text = reps
                        .first()
                        .is_some_and(|first| first.announcement.kind == AttachmentKind::Text && first.complete);
                    if !first_text
                        || self.items.last().map(|last| last.announcement.item_id) != Some(item_id)
                        || reps.len() as u32 != representation
                        || reps.iter().any(|rep| {
                            !rep.complete
                                || rep.announcement.mime.as_deref() == offer.mime().ok().map(|m| m.0).as_deref()
                        })
                    {
                        return Err(StateConflict.into());
                    }
                }
                let limit = match offer.kind()? {
                    dto::ContentKind::Text | dto::ContentKind::Url => MAX_DIRECT_TEXT,
                    dto::ContentKind::Html | dto::ContentKind::Rtf => MAX_DIRECT_TEXT,
                    _ => return Err(UnsupportedValue.into()),
                };
                let items = self.distinct_items();
                if self.text.is_some()
                    || (!rich && (items >= usize::from(self.count) || representation != 0))
                    || size > limit
                    || self.bytes + size > self.expected_bytes
                    || offer.operation_id()?.0 != operation_id
                    || offer.message_id()?.0 != self.message_id
                    || offer.source()? != self.source
                    || offer.proxy()?.is_some()
                    || offer.real_sender()? != device_id(peer)?
                    || offer_id != self_offer_id(peer, operation_id, offer.item_id()?)?
                    || offer.availability()? != dto::ContentAvailability::Ready
                    || self
                        .items
                        .iter()
                        .any(|item| !item.complete || (!rich && item.announcement.item_id == item_id))
                {
                    return Err(StateConflict.into());
                }
                self.items.push(IncomingItem {
                    announcement: content_announcement(&offer)?,
                    bytes: Vec::new(),
                    complete: false,
                });
            }
            v2::Message::ContentRepresentationBytes(chunk) => {
                let item = self.items.last_mut().ok_or(StateConflict)?;
                let id =
                    content_id_for_representation(&chunk.offer()?, item.announcement.item_id, chunk.representation()?)?;
                let bytes = chunk.bytes()?.0;
                if item.complete
                    || item.announcement.content_id != id
                    || bytes.is_empty()
                    || chunk.offset()? != item.bytes.len() as u64
                    || item.bytes.len() as u64 + bytes.len() as u64 > item.announcement.size
                {
                    return Err(StateConflict.into());
                }
                self.bytes += bytes.len() as u64;
                item.bytes.extend(bytes);
            }
            v2::Message::ContentRepresentationEnd(end) => {
                let item = self.items.last_mut().ok_or(StateConflict)?;
                let id =
                    content_id_for_representation(&end.offer()?, item.announcement.item_id, end.representation()?)?;
                if item.complete
                    || item.announcement.content_id != id
                    || end.size()? != item.bytes.len() as u64
                    || end.size()? != item.announcement.size
                {
                    return Err(StateConflict.into());
                }
                item.announcement.text =
                    Some(String::from_utf8(std::mem::take(&mut item.bytes)).map_err(|_| UnsupportedValue)?);
                item.complete = true;
            }
            v2::Message::ClipboardShareTextEnd(end) => {
                if self.count == 1 && self.expected_bytes == 0 && self.items.is_empty() && self.text.is_none() {
                    self.text = Some(Vec::new());
                }
                let count = self.distinct_items() + usize::from(self.text.is_some());
                if end.operation_id()?.0 != operation_id
                    || self.bytes != self.expected_bytes
                    || count != usize::from(self.count)
                    || self.items.iter().any(|item| !item.complete)
                {
                    return Err(StateConflict.into());
                }
                return Ok(true);
            }
            _ => return Err(UnsupportedValue.into()),
        }
        Ok(false)
    }

    /// Items received so far; a text item's rich text is not an item.
    fn distinct_items(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.announcement.kind != AttachmentKind::RichText)
            .count()
    }
}

fn self_offer_id(peer: &str, operation: [u8; 16], item_id: u32) -> Result<dto::OfferId, v2::Error> {
    let public_key = device_id(peer)?.public_key()?.0;
    let device = DeviceId(public_key.try_into().map_err(|_| v2::ErrorCode::MalformedFrame)?);
    Ok(offer_id(device, operation, item_id))
}

/// One Low-priority clipboard operation. The operation owns file paths and
/// streams their bytes on the network worker without loading a file in full.
#[doc(hidden)]
#[derive(Debug)]
pub struct ClipboardLowOperation {
    pub(crate) operation_id: [u8; 16],
    pub(crate) read: Option<v2::Message>,
    pub(crate) parts: Vec<LowPart>,
    total: u64,
    /// Set when the user cancels the share this upload belongs to.
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
}

#[derive(Debug)]
pub(crate) enum LowPart {
    Message(v2::Message),
    Representation {
        offer: dto::OfferId,
        representation: u32,
        size: u64,
        source: BodySource,
    },
}

#[derive(Debug)]
pub(crate) enum BodySource {
    File(PathBuf),
    Archive(Arc<tempfile::NamedTempFile>),
    Inline(Vec<u8>),
}

impl ClipboardLowOperation {
    pub(crate) fn is_history(&self) -> bool {
        matches!(
            self.parts.as_slice(),
            [LowPart::Message(
                v2::Message::HistoryIndex(_) | v2::Message::HistoryRecords(_) | v2::Message::HistoryDelta(_)
            )]
        )
    }
    pub(crate) fn request(operation_id: [u8; 16], message: v2::Message) -> Self {
        Self {
            operation_id,
            read: Some(message),
            parts: Vec::new(),
            total: 0,
            cancel: None,
        }
    }

    pub(crate) async fn send_parts(
        self,
        stream: &mut LowStream,
        mut progress: impl FnMut(u64, u64) + Send,
    ) -> Result<(), MobileV2TransportError> {
        let mut sent = 0;
        let mut last_report = Instant::now();
        let mut cancel = self.cancel;
        for part in self.parts {
            match part {
                LowPart::Message(message) => {
                    send_unless_cancelled(stream, &message, &mut cancel).await?;
                    if let v2::Message::ClipboardShareTextChunk(chunk) = message {
                        sent += chunk.bytes().map_err(protocol_error)?.0.len() as u64;
                    }
                }
                LowPart::Representation {
                    offer,
                    representation,
                    size,
                    source,
                } => {
                    let mut source: Box<dyn Read + Send> = match source {
                        BodySource::File(path) => Box::new(fs::File::open(path).map_err(link_error)?),
                        BodySource::Archive(file) => Box::new(file.reopen().map_err(link_error)?),
                        BodySource::Inline(bytes) => Box::new(Cursor::new(bytes)),
                    };
                    let mut offset = 0;
                    let mut buffer = vec![0; CONTENT_CHUNK];
                    while offset < size {
                        let length = (size - offset).min(buffer.len() as u64) as usize;
                        source.read_exact(&mut buffer[..length]).map_err(link_error)?;
                        let message = dto::ContentRepresentationBytes::from_raw(
                            [
                                (1, offer.write()),
                                (2, representation.write()),
                                (3, offset.write()),
                                (4, RawValue::Bytes(buffer[..length].to_vec())),
                            ]
                            .into(),
                        )
                        .map_err(protocol_error)?;
                        send_unless_cancelled(stream, &v2::Message::ContentRepresentationBytes(message), &mut cancel)
                            .await?;
                        offset += length as u64;
                        sent += length as u64;
                        if last_report.elapsed() >= Duration::from_millis(250) {
                            progress(sent, self.total);
                            last_report = Instant::now();
                        }
                    }
                    if source.read(&mut buffer[..1]).map_err(link_error)? != 0 {
                        return Err(link_error("shared content changed while sending"));
                    }
                    let end = dto::ContentRepresentationEnd::from_raw(
                        [(1, offer.write()), (2, representation.write()), (3, size.write())].into(),
                    )
                    .map_err(protocol_error)?;
                    send_unless_cancelled(stream, &v2::Message::ContentRepresentationEnd(end), &mut cancel).await?;
                }
            }
        }
        if self.read.is_none() {
            progress(sent, self.total);
        }
        Ok(())
    }
}

/// One message of an upload, abandoned at once when its share is cancelled
/// (the caller then drops the Low stream, which aborts it at the peer).
async fn send_unless_cancelled(
    stream: &mut LowStream,
    message: &v2::Message,
    cancel: &mut Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<(), MobileV2TransportError> {
    let Some(cancel) = cancel else {
        return stream.send(message).await.map_err(link_error);
    };
    tokio::select! {
        sent = stream.send(message) => sent.map_err(link_error),
        () = cancelled(cancel) => Err(MobileV2TransportError::Cancelled),
    }
}

/// Completes when the share is cancelled; never once it settled without a
/// cancel (its signal is gone).
async fn cancelled(cancel: &mut tokio::sync::watch::Receiver<bool>) {
    let settled = cancel.wait_for(|cancelled| *cancelled).await.is_err();
    if settled {
        std::future::pending::<()>().await;
    }
}

/// Rust-owned v2 transport seam. `frame` is a bounded v2 frame produced by
/// this crate. The host only chooses the already-authenticated link and sends
/// these bytes through it.
pub trait MobileV2Link: Send + Sync + 'static {
    fn send_frame(&self, peer_id: &str, frame: Vec<u8>) -> Result<(), MobileV2TransportError>;
    fn send_low(&self, peer_id: &str, operation: ClipboardLowOperation) -> Result<(), MobileV2TransportError>;
    fn supports_history(&self, _peer: &str) -> bool {
        true
    }
    fn is_pc_member(&self, _peer: &str) -> bool {
        true
    }
    fn group_id(&self) -> Option<GroupId> {
        None
    }
    fn peer_boot(&self, _peer_id: &str) -> Option<Boot> {
        None
    }
    fn is_member(&self, _peer_id: &str) -> bool {
        true
    }
    fn is_peer_ready(&self, _peer_id: &str) -> Option<bool> {
        None
    }
    /// This process keeps phone-to-phone transfers (the app does; a share
    /// extension session refuses them).
    fn accepts_direct(&self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MobileV2Identity {
    pub device: DeviceId,
    pub group: GroupId,
    pub boot: Boot,
}

impl MobileV2Identity {
    pub fn from_parts(device: [u8; 32], group: [u8; 16], nonce: [u8; 16]) -> Self {
        let device = DeviceId(device);
        Self {
            device,
            group: GroupId(group),
            boot: Boot {
                device_id: device,
                nonce,
            },
        }
    }
}

/// Transfer failures keep their kind up to the host (contract section 9):
/// an offline, evicted, deleted or replaced source is never reported as a
/// lost link or an empty result.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum MobileV2TransportError {
    #[error("v2 protocol: {0}")]
    Protocol(String),
    #[error("link: {0}")]
    Link(String),
    #[error("peer {0} is unavailable")]
    PeerUnavailable(String),
    /// The desktop paused clipboard sharing: it closed or refused B0 with
    /// `StreamCode::Paused`. Retryable once it resumes.
    #[error("clipboard sharing is paused on {0}")]
    PeerPaused(String),
    /// The source turned off sending its clipboard. Retryable once it is on.
    #[error("the source does not send its clipboard")]
    SendDisabled,
    /// No online device can take this request (share target, current text).
    #[error("no target device is online")]
    NoTarget,
    /// The content's source cannot serve it (offline, evicted, no such
    /// item, replaced, unsupported).
    #[error("content source unavailable: {0:?}")]
    SourceUnavailable(crate::SourceAvailability),
    /// The source's current clipboard expired before it could be read.
    #[error("the current clipboard expired")]
    Expired,
    #[error("message {0} does not exist")]
    NotFound(i64),
    #[error("message {0} was sent by this device and has no remote source")]
    OwnMessage(i64),
    #[error("a download of this message is already running")]
    InProgress,
    /// This phone has no room for the content (copying, packaging).
    #[error("not enough storage space on this phone")]
    StorageFull,
    /// The user cancelled the share before anything was recorded.
    #[error("the share was cancelled")]
    Cancelled,
    /// Reading the content to share failed on this device.
    #[error("cannot read the shared content: {0}")]
    Local(String),
    #[error("transfer storage: {0}")]
    Storage(String),
}

impl From<crate::StoreError> for MobileV2TransportError {
    fn from(error: crate::StoreError) -> Self {
        use crate::StoreError;
        match error {
            StoreError::MessageNotFound(id) => Self::NotFound(id),
            StoreError::OwnMessage(id) => Self::OwnMessage(id),
            StoreError::SourceOffline => Self::SourceUnavailable(crate::SourceAvailability::SourceOffline),
            StoreError::SourceUnavailable(availability) => Self::SourceUnavailable(availability),
            StoreError::DownloadInProgress => Self::InProgress,
            error => Self::Storage(error.to_string()),
        }
    }
}

impl From<crate::CoordinatorError> for MobileV2TransportError {
    fn from(error: crate::CoordinatorError) -> Self {
        match error {
            crate::CoordinatorError::Store(error) => error.into(),
            crate::CoordinatorError::Transport(error) => Self::Link(error.message),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransferEvent {
    MessageReceived {
        peer_id: String,
        message: String,
    },
    /// A history record was created or changed. `parent_id` is set for a
    /// file inside a folder message. `auto_open_ready` means this record now
    /// fills the auto-open slot: the host calls `take_ready_auto_open`.
    Announcement {
        record_id: i64,
        content_id: String,
        parent_id: Option<i64>,
        auto_open_ready: bool,
        /// `MessageRecord::sensitive` of the record.
        sensitive: bool,
    },
    /// Progress of one record's download. `operation_id` is the offer's
    /// operation and is shared by every item of a multi-item copy; hosts key
    /// progress by `record_id`.
    DownloadProgress {
        operation_id: [u8; 16],
        record_id: i64,
        received: u64,
        total: Option<u64>,
    },
    RateSample {
        operation_id: [u8; 16],
        bytes_per_second: u64,
    },
    /// Upload progress of one own share; `record_ids` are its own records.
    SendProgress {
        operation_id: [u8; 16],
        record_ids: Vec<i64>,
        sent: u64,
        total: Option<u64>,
    },
    /// Terminal outcome of one own share (all targets settled). `status` is
    /// `"Complete"` or `"Failed"`; `failure`/`detail` explain a failure.
    /// Inline Complete means publication reached the transport (Low waits for EOF),
    /// not that every PC applied its clipboard or announced Holding.
    DeliveryEvent {
        operation_id: [u8; 16],
        record_ids: Vec<i64>,
        status: String,
        delivery: DeliveryState,
        failure: Option<DeliveryFailure>,
        detail: Option<String>,
    },
    Error {
        operation_id: Option<[u8; 16]>,
        record_id: Option<i64>,
        detail: String,
    },
    /// The core deleted these records (a cancelled own share, or a
    /// phone-to-phone transfer its sender cancelled): drop them.
    Removed {
        record_ids: Vec<i64>,
    },
    /// A phone-to-phone transfer started arriving: its first record exists.
    /// Hosts in the background notify with it.
    DirectIncoming {
        operation_id: [u8; 16],
        sender_id: String,
        record_ids: Vec<i64>,
        item_count: u32,
        first_name: String,
        total_bytes: u64,
    },
    /// A phone-to-phone receive ended (`failure` None: complete).
    DirectEnded {
        operation_id: [u8; 16],
        record_ids: Vec<i64>,
        completed: bool,
        failure: Option<DeliveryFailure>,
    },
    /// One page of a folder listing. `folder_message_id` is the conversation
    /// record that owns the folder and `directory` the node the listing was
    /// requested with (0 for the root page, however the folder numbers its
    /// root); the next page is requested with `next_version` + `next_offset`
    /// (both present or both absent).
    DirectoryPage {
        operation_id: [u8; 16],
        folder_message_id: i64,
        directory: u64,
        entries: Vec<DirectoryEntry>,
        next_version: Option<u64>,
        next_offset: Option<u32>,
    },
    /// A share sent with `send_share_when_online` waits for its offline
    /// target (`waiting`, emitted once the first connect round ended), or
    /// that target came online and the share starts now (`!waiting`, only
    /// after a `waiting`). Nothing is recorded while it waits.
    ShareWaiting {
        operation_id: [u8; 16],
        waiting: bool,
    },
    /// The listing `operation_id` of `directory` (the node it was requested
    /// with, 0 for the root) of folder message `folder_message_id` ended
    /// without a page. `retryable` is false when the content is gone for
    /// good (the local copy was cleared, the source no longer has it).
    DirectoryFailed {
        operation_id: [u8; 16],
        folder_message_id: i64,
        directory: u64,
        error: MobileV2TransportError,
        retryable: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectoryEntry {
    pub id: u64,
    pub parent: Option<u64>,
    pub name: String,
    pub kind: String,
    pub size: Option<u64>,
    pub version: u64,
    /// The record of this file under its folder message, when it has one:
    /// every file of a folder received phone to phone (complete, with its
    /// `local_path`), a PC folder's file once downloaded or asked for a
    /// thumbnail. Set on listing pages; ignored when an entry is passed in.
    #[serde(default)]
    pub child: Option<MessageRecord>,
}

#[derive(Debug)]
struct V2Transport<L> {
    /// Accept-window timers hold this, not the transport.
    me: Weak<Self>,
    link: Arc<L>,
    identity: MobileV2Identity,
    peers: Arc<Mutex<Vec<Peer>>>,
    events: Arc<EventSlot>,
    store: Arc<Mutex<TransferStore>>,
    outgoing: Mutex<HashMap<ShareKey, OutgoingShare>>,
    /// Upload cancel signals of own shares, by operation.
    uploads: Mutex<HashMap<[u8; 16], tokio::sync::watch::Sender<bool>>>,
    /// Shares cancelled before `send_share` recorded them.
    cancelled_early: Mutex<std::collections::HashSet<[u8; 16]>>,
    deliveries: Mutex<HashMap<[u8; 16], Delivery>>,
    history_publications: Mutex<std::collections::HashSet<ShareKey>>,
    clock_publications: Mutex<HashMap<[u8; 16], history::ClockPublication>>,
    current_queries: Mutex<HashMap<ShareKey, PendingCurrent>>,
    staging: PathBuf,
}

type Failure = (DeliveryFailure, String);

/// Aggregated delivery of one own share over all its targets. `pending`
/// counts started targets plus one guard held by `send_share` until routing
/// returned, so an early result cannot settle a broadcast still starting.
#[derive(Debug)]
struct Delivery {
    record_ids: Vec<i64>,
    pending: usize,
    delivered: bool,
    failure: Option<Failure>,
    /// The user cancelled it; it ends as Cancelled unless a target already
    /// took it.
    cancelled: bool,
}

/// A rejection only about that PC: another candidate may take the share.
fn rejection_names_the_target(reason: Option<dto::ShareRejectReason>) -> bool {
    matches!(
        reason,
        Some(dto::ShareRejectReason::ReceiveOff | dto::ShareRejectReason::Paused | dto::ShareRejectReason::Busy)
    )
}

fn rejection_failure(status: dto::ShareStatus, reason: Option<dto::ShareRejectReason>) -> Failure {
    match reason {
        Some(reason) => (DeliveryFailure::Rejected, format!("{status:?}: {reason:?}")),
        None => share_status_result(status)
            .err()
            .unwrap_or((DeliveryFailure::Rejected, format!("{status:?}"))),
    }
}

fn share_status_result(status: dto::ShareStatus) -> Result<(), Failure> {
    match status {
        dto::ShareStatus::Complete => Ok(()),
        dto::ShareStatus::Failed => Err((
            DeliveryFailure::PeerFailed,
            "the target could not publish the share".into(),
        )),
        // Only a receiving phone answers so: its user cancelled.
        dto::ShareStatus::Cancelled => Err((DeliveryFailure::Cancelled, "the receiver cancelled".into())),
        status => Err((DeliveryFailure::Rejected, format!("{status:?}"))),
    }
}

impl<L: MobileV2Link> V2Transport<L> {
    fn emit(&self, event: TransferEvent) {
        let sink = self.events.0.lock().expect("transfer events mutex poisoned").clone();
        if let Some(sink) = sink {
            sink(event);
        }
    }

    /// Opens the delivery before the own records exist, so a cancel that
    /// arrives while the share is still being recorded is not lost.
    fn begin_delivery(&self, operation_id: [u8; 16]) {
        self.deliveries.lock().expect("delivery mutex poisoned").insert(
            operation_id,
            Delivery {
                record_ids: Vec::new(),
                pending: 1,
                delivered: false,
                failure: None,
                cancelled: false,
            },
        );
    }

    fn set_delivery_records(&self, operation_id: [u8; 16], record_ids: Vec<i64>) {
        if let Some(delivery) = self
            .deliveries
            .lock()
            .expect("delivery mutex poisoned")
            .get_mut(&operation_id)
        {
            delivery.record_ids = record_ids;
        }
    }

    /// Nothing was recorded: the delivery never existed for the host.
    fn abandon_delivery(&self, operation_id: [u8; 16]) {
        self.deliveries
            .lock()
            .expect("delivery mutex poisoned")
            .remove(&operation_id);
    }

    fn delivery_cancelled(&self, operation_id: [u8; 16]) -> bool {
        self.deliveries
            .lock()
            .expect("delivery mutex poisoned")
            .get(&operation_id)
            .is_some_and(|delivery| delivery.cancelled)
    }

    fn upload_cancel(&self, operation_id: [u8; 16]) -> tokio::sync::watch::Receiver<bool> {
        self.uploads
            .lock()
            .expect("upload cancel mutex poisoned")
            .entry(operation_id)
            .or_insert_with(|| tokio::sync::watch::channel(false).0)
            .subscribe()
    }

    /// Cancels a share in progress: every target is told (ClipboardShareCancel
    /// on B0) and settled as Cancelled, running uploads stop and drop their
    /// Low stream. False when no delivery of `operation_id` is running.
    fn cancel(&self, operation_id: [u8; 16]) -> bool {
        {
            let mut deliveries = self.deliveries.lock().expect("delivery mutex poisoned");
            let Some(delivery) = deliveries.get_mut(&operation_id) else {
                return false;
            };
            delivery.cancelled = true;
        }
        let clock_wait = self
            .clock_publications
            .lock()
            .expect("clock publication mutex")
            .remove(&operation_id);
        self.uploads
            .lock()
            .expect("upload cancel mutex poisoned")
            .entry(operation_id)
            .or_insert_with(|| tokio::sync::watch::channel(false).0)
            .send_replace(true);
        let targets: Vec<String> = {
            let mut outgoing = self.outgoing.lock().expect("outgoing share mutex poisoned");
            let targets: Vec<String> = outgoing
                .keys()
                .filter(|(_, operation)| *operation == operation_id)
                .map(|(peer, _)| peer.clone())
                .collect();
            for peer in &targets {
                outgoing.remove(&(peer.clone(), operation_id));
            }
            targets
        };
        for peer in targets {
            if let Ok(cancel) = dto::ClipboardShareCancel::from_raw([(1, id16(operation_id).write())].into()) {
                // A target that is already gone needs no cancel.
                let _ = self.send_message(&peer, v2::Message::ClipboardShareCancel(cancel));
            }
            self.settle(operation_id, Err(Some(cancelled_failure())));
        }
        if clock_wait.is_some() {
            self.settle(operation_id, Err(Some(cancelled_failure())));
        }
        true
    }

    fn delivery_records(&self, operation_id: [u8; 16]) -> Vec<i64> {
        self.deliveries
            .lock()
            .expect("delivery mutex poisoned")
            .get(&operation_id)
            .map_or_else(Vec::new, |delivery| delivery.record_ids.clone())
    }

    /// Records why routing to one target failed without settling the share.
    fn note_failure(&self, operation_id: [u8; 16], failure: Failure) {
        if let Some(delivery) = self
            .deliveries
            .lock()
            .expect("delivery mutex poisoned")
            .get_mut(&operation_id)
        {
            delivery.failure = Some(failure);
        }
    }

    /// Settles one target (or the routing guard). When the last one settles,
    /// the own records get their final state and one DeliveryEvent is sent.
    fn settle(&self, operation_id: [u8; 16], result: Result<(), Option<Failure>>) {
        let finished = {
            let mut deliveries = self.deliveries.lock().expect("delivery mutex poisoned");
            let Some(delivery) = deliveries.get_mut(&operation_id) else {
                return;
            };
            match result {
                Ok(()) => delivery.delivered = true,
                Err(Some(failure)) => delivery.failure = Some(failure),
                Err(None) => {}
            }
            delivery.pending = delivery.pending.saturating_sub(1);
            if delivery.pending != 0 {
                return;
            }
            deliveries.remove(&operation_id).expect("settled delivery")
        };
        self.uploads
            .lock()
            .expect("upload cancel mutex poisoned")
            .remove(&operation_id);
        let mut finished = finished;
        if finished.cancelled {
            finished.failure = Some(cancelled_failure());
        }
        let (delivery, failure) = if finished.delivered {
            (DeliveryState::Sent, None)
        } else {
            (
                DeliveryState::Failed,
                Some(
                    finished
                        .failure
                        .unwrap_or((DeliveryFailure::Interrupted, "the share ended without a result".into())),
                ),
            )
        };
        if let Err(error) = self.store.lock().expect("transfer store mutex poisoned").set_delivery(
            &finished.record_ids,
            delivery,
            failure.as_ref().map(|(reason, detail)| (*reason, detail.as_str())),
        ) {
            self.emit(TransferEvent::Error {
                operation_id: Some(operation_id),
                record_id: finished.record_ids.first().copied(),
                detail: error.to_string(),
            });
        }
        let removed = finished.cancelled && !finished.delivered;
        self.emit(TransferEvent::DeliveryEvent {
            operation_id,
            record_ids: finished.record_ids.clone(),
            status: if failure.is_none() { "Complete" } else { "Failed" }.into(),
            delivery,
            failure: failure.as_ref().map(|(reason, _)| *reason),
            detail: failure.map(|(_, detail)| detail),
        });
        // The user cancelled an own share: it leaves the history on both
        // ends (2026-11-11 review: "取消发送同时把这条消息从记录中移除").
        if removed && !finished.record_ids.is_empty() {
            {
                let mut store = self.store.lock().expect("transfer store mutex poisoned");
                for id in &finished.record_ids {
                    let _ = store.delete_message(*id);
                }
            }
            self.emit(TransferEvent::Removed {
                record_ids: finished.record_ids,
            });
        }
    }

    /// The Low stream of an own upload to `peer` opened.
    pub(crate) fn upload_opened(&self, peer: &str, operation_id: [u8; 16]) {
        if let Some(share) = self
            .outgoing
            .lock()
            .expect("outgoing share mutex poisoned")
            .get_mut(&(peer.to_owned(), operation_id))
        {
            share.opened = true;
        }
    }

    /// The upload of an own share to `peer` failed. A phone-to-phone
    /// upload whose stream was open waits for the receiver's result (it saw
    /// the stream break too) or the link loss; otherwise the target ends now.
    /// True when the outcome is still to come.
    fn upload_failed(&self, peer: &str, operation_id: [u8; 16], detail: String) -> bool {
        let waits = {
            let outgoing = self.outgoing.lock().expect("outgoing share mutex poisoned");
            match outgoing.get(&(peer.to_owned(), operation_id)) {
                Some(share) if share.direct => Some(share.opened),
                _ => None,
            }
        };
        match waits {
            Some(true) => {
                self.note_failure(operation_id, (DeliveryFailure::Interrupted, detail));
                return true;
            }
            Some(false) => {
                // The receiver accepted but never got the stream: release it.
                if let Ok(cancel) = dto::ClipboardShareCancel::from_raw([(1, id16(operation_id).write())].into()) {
                    let _ = self.send_message(peer, v2::Message::ClipboardShareCancel(cancel));
                }
                self.finish_share(peer, operation_id, Err((DeliveryFailure::Interrupted, detail)));
            }
            None => self.finish_share(peer, operation_id, Err((DeliveryFailure::Interrupted, detail))),
        }
        false
    }

    /// Fails shares the target never answered. Started shares are not timed
    /// out here: a large upload may take far longer than the accept window.
    /// Each share arms one timer for its window (`start_share`).
    fn expire_unaccepted(&self) {
        let expired: Vec<ShareKey> = {
            let mut outgoing = self.outgoing.lock().expect("outgoing share mutex poisoned");
            let keys: Vec<ShareKey> = outgoing
                .iter()
                .filter(|(_, share)| !share.started && share.created.elapsed() >= SHARE_LIFETIME)
                .map(|(key, _)| key.clone())
                .collect();
            for key in &keys {
                outgoing.remove(key);
            }
            keys
        };
        for (_, operation_id) in expired {
            self.settle(
                operation_id,
                Err(Some((
                    DeliveryFailure::NotAccepted,
                    format!(
                        "the target did not accept the share within {} s",
                        SHARE_LIFETIME.as_secs()
                    ),
                ))),
            );
        }
    }

    fn send_message(&self, peer: &str, message: v2::Message) -> Result<(), MobileV2TransportError> {
        let history = matches!(
            &message,
            v2::Message::HistoryRecords(_) | v2::Message::HistoryDelta(_) | v2::Message::HistoryIndex(_)
        );
        let frame = v2::frame::encode(&message, if history { v2::limits::LOW_FRAME } else { MAX_FRAME })
            .map_err(protocol_error)?;
        if history && frame.len() > MAX_FRAME {
            return self.link.send_low(
                peer,
                ClipboardLowOperation {
                    operation_id: rand::random(),
                    read: None,
                    parts: vec![LowPart::Message(message)],
                    total: 0,
                    cancel: None,
                },
            );
        }
        self.link.send_frame(peer, frame)
    }

    fn send_current(
        &self,
        peer: &str,
        request_id: [u8; 16],
        max_items: u16,
        reply: Option<tokio::sync::oneshot::Sender<CurrentReply>>,
    ) -> Result<(), MobileV2TransportError> {
        if !self.peers().iter().any(|candidate| {
            candidate.id == peer && candidate.state == crate::PeerState::Ready && candidate.accepts_clipboard
        }) {
            return Err(MobileV2TransportError::PeerUnavailable(peer.into()));
        }
        let mut pending = self.current_queries.lock().expect("current query mutex poisoned");
        let now = Instant::now();
        pending.retain(|_, query| query.deadline > now && query.reply.as_ref().is_none_or(|reply| !reply.is_closed()));
        let key = (peer.to_owned(), request_id);
        if pending.len() >= MAX_CURRENT_QUERIES || pending.contains_key(&key) {
            return Err(link_error("clipboard current request already in progress"));
        }
        let mut identity = self.identity;
        if let Some(group) = self.link.group_id() {
            identity.group = group;
        }
        let message = current_query(&identity, request_id, max_items).map_err(protocol_error)?;
        self.send_message(peer, message)?;
        pending.insert(
            key,
            PendingCurrent {
                deadline: now + CURRENT_TIMEOUT,
                reply,
            },
        );
        Ok(())
    }

    fn peers(&self) -> Vec<Peer> {
        self.peers.lock().expect("peer mutex poisoned").clone()
    }
}

impl<L: MobileV2Link> Transport for V2Transport<L> {
    fn send(&self, request: TransportRequest) -> Result<(), TransportError> {
        match request {
            TransportRequest::CurrentQuery {
                request_id,
                peer_id,
                max_items,
            } => {
                self.send_current(&peer_id, request_id, max_items, None)
                    .map_err(|error| TransportError::new(error.to_string()))?;
            }
            TransportRequest::Broadcast { payload } => {
                if let Err(error) = self.broadcast_history_text(&payload) {
                    let detail = error.to_string();
                    self.note_failure(payload.operation_id, start_failure(error));
                    return Err(TransportError::new(detail));
                }
            }
            TransportRequest::ProxyShare { proxy_id, payload } => {
                let mut peers = self.peers();
                peers.retain(|p| {
                    p.state == crate::PeerState::Ready
                        && p.accepts_clipboard
                        && p.kind == crate::PeerKind::Pc
                        && !p.relayed
                        && !p.clipboard_pending
                });
                peers.sort_by_key(|peer| (peer.id != proxy_id, peer.relayed, peer.connection_order));
                let share = Arc::new(self.prepare(payload)?);
                let operation_id = share.payload.operation_id;
                let candidates: Vec<String> = peers.into_iter().map(|peer| peer.id).collect();
                for (index, peer) in candidates.iter().enumerate() {
                    let fallback = candidates[index + 1..].to_vec();
                    match self.start_share(peer, share.clone(), dto::ShareDelivery::Directed, fallback, false) {
                        Ok(()) => {
                            self.record_proxy(operation_id, peer);
                            return Ok(());
                        }
                        Err(MobileV2TransportError::PeerUnavailable(_)) => continue,
                        Err(error) => {
                            let failure = start_failure(error);
                            self.note_failure(operation_id, failure.clone());
                            return Err(TransportError::new(failure.1));
                        }
                    }
                }
                let failure = (DeliveryFailure::NoTarget, "no clipboard proxy is ready".to_owned());
                self.note_failure(operation_id, failure.clone());
                return Err(TransportError::new(failure.1));
            }
            TransportRequest::DirectShare { target_id, payload } => {
                let share = Arc::new(self.prepare(payload)?);
                let operation_id = share.payload.operation_id;
                if let Err(error) = self.start_share(&target_id, share, dto::ShareDelivery::Directed, Vec::new(), true)
                {
                    let failure = start_failure(error);
                    self.note_failure(operation_id, failure.clone());
                    return Err(TransportError::new(failure.1));
                }
            }
            TransportRequest::Download {
                operation_id,
                item_id,
                representation_id,
                content_id,
                source_id,
                ..
            } => {
                let offer = parse_content_id(&content_id).map_err(|e| TransportError::new(e.to_string()))?;
                let source = {
                    let store = self.store.lock().expect("transfer store mutex poisoned");
                    store
                        .by_content_id(&content_id)
                        .ok()
                        .flatten()
                        .and_then(|r| store.history_source(r.id).ok().flatten())
                        .unwrap_or(source_id)
                };
                let peer = self
                    .peers()
                    .into_iter()
                    .find(|p| p.state == crate::PeerState::Ready && p.id == source);
                let Some(peer) = peer else {
                    return Err(TransportError::new("download source is unavailable"));
                };
                let read = if let Some((node, version)) = parse_file_content(&content_id) {
                    let raw = RawMap::from([
                        (1, offer.write()),
                        (2, node.write()),
                        (3, version.write()),
                        (4, 0u64.write()),
                        (6, clipboard_demand().write()),
                    ]);
                    dto::ContentReadFile::from_raw(raw).map(v2::Message::ContentReadFile)
                } else {
                    dto::ClipboardContentRead::from_raw(
                        [
                            (1, id16(operation_id).write()),
                            (2, offer.write()),
                            (3, item_id.write()),
                            (4, representation_id.write()),
                        ]
                        .into(),
                    )
                    .map(v2::Message::ClipboardContentRead)
                }
                .map_err(|e| TransportError::new(e.to_string()))?;
                self.link
                    .send_low(&peer.id, ClipboardLowOperation::request(operation_id, read))
                    .map_err(|e| TransportError::new(e.to_string()))?;
            }
        }
        Ok(())
    }
}

fn cancelled_failure() -> Failure {
    (DeliveryFailure::Cancelled, "the share was cancelled".into())
}

fn start_failure(error: MobileV2TransportError) -> Failure {
    match error {
        MobileV2TransportError::Cancelled => cancelled_failure(),
        MobileV2TransportError::PeerUnavailable(peer) => {
            (DeliveryFailure::NoTarget, format!("peer {peer} is unavailable"))
        }
        MobileV2TransportError::NoTarget => (DeliveryFailure::NoTarget, error.to_string()),
        MobileV2TransportError::PeerPaused(_) => (DeliveryFailure::Paused, error.to_string()),
        error => (DeliveryFailure::Interrupted, error.to_string()),
    }
}

impl<L: MobileV2Link> V2Transport<L> {
    /// Packages the share once for all targets and records the packaged
    /// file count of each folder on its own record.
    fn prepare(&self, payload: SharePayload) -> Result<PreparedShare, TransportError> {
        let operation_id = payload.operation_id;
        let share = PreparedShare::new(payload, &self.staging).map_err(|error| {
            let reason = if error == MobileV2TransportError::StorageFull {
                DeliveryFailure::StorageFull
            } else {
                DeliveryFailure::Local
            };
            self.note_failure(operation_id, (reason, error.to_string()));
            TransportError::new(error.to_string())
        })?;
        let records = self.delivery_records(operation_id);
        let store = self.store.lock().expect("transfer store mutex poisoned");
        for (index, files) in &share.folder_files {
            if let Some(record) = records.get(*index as usize) {
                store
                    .set_item_count(*record, *files)
                    .map_err(|error| TransportError::new(error.to_string()))?;
            }
        }
        Ok(share)
    }

    /// The proxy actually used may differ from the requested one when it
    /// went offline before sending (D17); the own records show the real one.
    fn record_proxy(&self, operation_id: [u8; 16], proxy: &str) {
        let records = self.delivery_records(operation_id);
        let store = self.store.lock().expect("transfer store mutex poisoned");
        if let Err(error) = store.set_outgoing_proxy(&records, proxy) {
            drop(store);
            self.emit(TransferEvent::Error {
                operation_id: Some(operation_id),
                record_id: records.first().copied(),
                detail: error.to_string(),
            });
        }
    }

    fn start_share(
        &self,
        peer: &str,
        share: Arc<PreparedShare>,
        delivery: dto::ShareDelivery,
        fallback: Vec<String>,
        direct: bool,
    ) -> Result<(), MobileV2TransportError> {
        if self.delivery_cancelled(share.payload.operation_id) {
            return Err(MobileV2TransportError::Cancelled);
        }
        let begin = share_begin(&self.identity, &share, delivery).map_err(protocol_error)?;
        self.expire_unaccepted();
        let operation_id = share.payload.operation_id;
        let mut outgoing = self.outgoing.lock().expect("outgoing share mutex poisoned");
        let key = (peer.to_owned(), operation_id);
        if outgoing.contains_key(&key) || outgoing.len() >= MAX_PENDING {
            return Err(link_error("share is already pending or the queue is full"));
        }
        outgoing.insert(
            key.clone(),
            OutgoingShare {
                share,
                delivery,
                created: Instant::now(),
                started: false,
                fallback,
                direct,
                opened: false,
            },
        );
        if let Err(error) = self.send_message(peer, begin) {
            outgoing.remove(&key);
            return Err(error);
        }
        if let Some(delivery) = self
            .deliveries
            .lock()
            .expect("delivery mutex poisoned")
            .get_mut(&operation_id)
        {
            delivery.pending += 1;
        }
        drop(outgoing);
        // The target's Accept or Reject normally ends the wait; this timer
        // only settles a target that never answers. Without a runtime
        // (synchronous unit tests) the next share expires it instead.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let me = self.me.clone();
            runtime.spawn(async move {
                tokio::time::sleep(SHARE_LIFETIME).await;
                if let Some(transport) = me.upgrade() {
                    transport.expire_unaccepted();
                }
            });
        }
        Ok(())
    }

    fn share_accepted(&self, peer: &str, operation_id: [u8; 16]) -> Result<(), MobileV2TransportError> {
        let key = (peer.to_owned(), operation_id);
        let share = {
            let mut outgoing = self.outgoing.lock().expect("outgoing share mutex poisoned");
            let Some(share) = outgoing.get_mut(&key) else {
                return Ok(());
            };
            if share.started {
                return Ok(());
            }
            share.started = true;
            share.share.clone()
        };
        let cancel = self.upload_cancel(operation_id);
        let result = share_operation(&self.identity, &share).and_then(|mut operation| {
            operation.cancel = Some(cancel);
            self.link.send_low(peer, operation)
        });
        if let Err(error) = result {
            self.finish_share(
                peer,
                operation_id,
                Err((DeliveryFailure::Interrupted, error.to_string())),
            );
            return Err(error);
        }
        Ok(())
    }

    /// A PC refused a share before accepting it. A proxy share whose refusal
    /// only concerns that PC moves, before anything was committed, to the next
    /// candidate in the fixed order; anything else ends this target with the
    /// PC's reason (protocol-v2 5.2, review M17).
    fn share_rejected(
        &self,
        peer: &str,
        operation_id: [u8; 16],
        status: dto::ShareStatus,
        reason: Option<dto::ShareRejectReason>,
    ) {
        let key = (peer.to_owned(), operation_id);
        let movable = {
            let mut outgoing = self.outgoing.lock().expect("outgoing share mutex poisoned");
            if outgoing.get(&key).is_some_and(|share| {
                !share.started
                    && share.delivery == dto::ShareDelivery::Directed
                    && !share.direct
                    && rejection_names_the_target(reason)
            }) {
                outgoing.remove(&key)
            } else {
                None
            }
        };
        let Some(rejected) = movable else {
            self.finish_share(peer, operation_id, Err(rejection_failure(status, reason)));
            return;
        };
        let ready = self.peers();
        for (index, next) in rejected.fallback.iter().enumerate() {
            let usable = ready.iter().any(|candidate| {
                candidate.id == *next && candidate.state == crate::PeerState::Ready && candidate.accepts_clipboard
            });
            if usable
                && self
                    .start_share(
                        next,
                        rejected.share.clone(),
                        dto::ShareDelivery::Directed,
                        rejected.fallback[index + 1..].to_vec(),
                        false,
                    )
                    .is_ok()
            {
                self.record_proxy(operation_id, next);
                // The refused target is settled without a failure; the next
                // candidate now carries the share.
                self.settle(operation_id, Err(None));
                return;
            }
        }
        self.settle(operation_id, Err(Some(rejection_failure(status, reason))));
    }

    /// Ends one target of an own share; only the first end of a target counts.
    fn finish_share(&self, peer: &str, operation_id: [u8; 16], result: Result<(), Failure>) {
        if self
            .outgoing
            .lock()
            .expect("outgoing share mutex poisoned")
            .remove(&(peer.to_owned(), operation_id))
            .is_some()
        {
            self.settle(operation_id, result.map_err(Some));
        }
    }
}

fn share_operation(
    identity: &MobileV2Identity,
    share: &PreparedShare,
) -> Result<ClipboardLowOperation, MobileV2TransportError> {
    let payload = &share.payload;
    let mut parts = Vec::new();
    let mut offset = 0u64;
    for (index, item) in payload.items.iter().enumerate() {
        if payload.items.len() == 1
            && let crate::SendItem::Text { text } = item
            && text.len() as u64 <= MAX_DIRECT_TEXT
        {
            for bytes in text.as_bytes().chunks(4096) {
                let chunk = dto::ClipboardShareTextChunk::from_raw(
                    [
                        (1, id16(payload.operation_id).write()),
                        (2, offset.write()),
                        (3, RawValue::Bytes(bytes.to_vec())),
                    ]
                    .into(),
                )
                .map_err(protocol_error)?;
                parts.push(LowPart::Message(v2::Message::ClipboardShareTextChunk(chunk)));
                offset += bytes.len() as u64;
            }
            continue;
        }
        let archive = share.folders.get(&(index as u32));
        // A text item is one item with a representation per format: plain
        // text, then its rich text (2026-11-12). Attachments have one.
        let representations = match item {
            crate::SendItem::Text { .. } | crate::SendItem::RichText { .. } => item.representations().len() as u32,
            _ => 1,
        };
        for representation in 0..representations {
            let offer = content_offer(identity, payload, item, index as u32, representation, archive)
                .map_err(protocol_error)?;
            let source = match item {
                crate::SendItem::Image { path, .. } | crate::SendItem::File { path, .. } => {
                    BodySource::File(PathBuf::from(path))
                }
                crate::SendItem::Folder { .. } => {
                    BodySource::Archive(archive.ok_or_else(|| link_error("folder archive is missing"))?.clone())
                }
                crate::SendItem::Text { .. } | crate::SendItem::RichText { .. } => {
                    BodySource::Inline(item.representations()[representation as usize].1.as_bytes().to_vec())
                }
            };
            let body = LowPart::Representation {
                offer: offer.offer().map_err(protocol_error)?,
                representation: offer.representation_id().map_err(protocol_error)?,
                size: offer
                    .bytes()
                    .map_err(protocol_error)?
                    .ok_or_else(|| link_error("unknown share size"))?,
                source,
            };
            parts.push(LowPart::Message(v2::Message::ContentOffer(offer)));
            parts.push(body);
        }
    }
    let end = dto::ClipboardShareTextEnd::from_raw([(1, id16(payload.operation_id).write())].into())
        .map_err(protocol_error)?;
    parts.push(LowPart::Message(v2::Message::ClipboardShareTextEnd(end)));
    Ok(ClipboardLowOperation {
        operation_id: payload.operation_id,
        read: None,
        parts,
        total: share.total,
        cancel: None,
    })
}

/// One share waiting for its offline target (`send_share_when_online`).
/// The payload stays with the waiting caller; this keeps what deciding
/// "a target is ready" needs and the caller's wake-up.
struct TargetWait {
    payload: SharePayload,
    choice: crate::ProxyChoice,
    /// `ShareWaiting { waiting: true }` was emitted.
    announced: bool,
    wake: tokio::sync::oneshot::Sender<TargetWaitEnd>,
}

enum TargetWaitEnd {
    /// A target is ready: record and send now.
    Ready,
    /// `cancel_share` while waiting: nothing is recorded.
    Cancelled,
}

/// Drops the wait of a caller that went away (its future was dropped).
struct TargetWaitGuard<'a> {
    waits: &'a Mutex<HashMap<[u8; 16], TargetWait>>,
    operation_id: [u8; 16],
}

impl Drop for TargetWaitGuard<'_> {
    fn drop(&mut self) {
        self.waits
            .lock()
            .expect("target wait mutex poisoned")
            .remove(&self.operation_id);
    }
}

pub struct MobileV2Session<L> {
    coordinator: TransferCoordinator<V2Transport<L>>,
    transport: Arc<V2Transport<L>>,
    peers: Arc<Mutex<Vec<Peer>>>,
    events: Arc<EventSlot>,
    /// A failed store takeover, reported once a sink is attached.
    startup_error: Mutex<Option<String>>,
    /// Shares recorded while no target was ready and the session's first
    /// round of discovery and dialing was still running ("waiting for a
    /// target"). They go out when a target is ready, or fail with NoTarget
    /// once the round ended without one (`set_targets`).
    waiting: Mutex<Vec<(SharePayload, crate::ProxyChoice)>>,
    /// Shares that wait for their offline target before anything is
    /// recorded (`send_share_when_online`, user 2026-11-11), by operation.
    target_waits: Mutex<HashMap<[u8; 16], TargetWait>>,
    /// The first connect round ended; without a target a share now fails.
    /// A session without a network owner (tests) starts settled.
    targets_settled: AtomicBool,
    incoming: Mutex<HashMap<ShareKey, IncomingShare>>,
    /// Phone-to-phone receives (`direct`).
    direct: Mutex<HashMap<ShareKey, direct::DirectReceive>>,
    attachments: Mutex<crate::v2_attachments::ClipboardAttachments>,
    /// Folder message owning each in-flight remote directory listing, and
    /// the node the host asked for (0: the root).
    directory_ops: Mutex<HashMap<[u8; 16], (i64, u64)>>,
    /// Record and store attempt of each in-flight thumbnail request.
    thumbnail_ops: Mutex<HashMap<[u8; 16], ThumbnailOp>>,
    download_changed: tokio::sync::Notify,
    fresh_clipboard: FreshClipboard,
    history: Mutex<history::HistorySync>,
}

impl<L: MobileV2Link> std::fmt::Debug for MobileV2Session<L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobileV2Session").finish_non_exhaustive()
    }
}

impl<L: MobileV2Link> MobileV2Session<L> {
    pub fn new(mut store: crate::TransferStore, identity: MobileV2Identity, link: Arc<L>) -> Self {
        let peers = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(EventSlot::default());
        let startup_error = store
            .take_over()
            .and_then(|()| store.reset_interrupted_thumbnails())
            .and_then(|()| store.initialize_history_group(identity.group).map(|_| ()))
            .err()
            .map(|error| error.to_string());
        let staging = store.staging_dir();
        let store = Arc::new(Mutex::new(store));
        let transport = Arc::new_cyclic(|me| V2Transport {
            me: me.clone(),
            link,
            identity,
            peers: peers.clone(),
            events: events.clone(),
            store: store.clone(),
            outgoing: Mutex::new(HashMap::new()),
            uploads: Mutex::new(HashMap::new()),
            cancelled_early: Mutex::new(std::collections::HashSet::new()),
            deliveries: Mutex::new(HashMap::new()),
            history_publications: Mutex::new(std::collections::HashSet::new()),
            clock_publications: Mutex::new(HashMap::new()),
            current_queries: Mutex::new(HashMap::new()),
            staging,
        });
        Self {
            coordinator: TransferCoordinator::with_store(store, transport.clone()),
            transport,
            peers,
            events,
            startup_error: Mutex::new(startup_error),
            waiting: Mutex::new(Vec::new()),
            target_waits: Mutex::new(HashMap::new()),
            targets_settled: AtomicBool::new(true),
            incoming: Mutex::new(HashMap::new()),
            direct: Mutex::new(HashMap::new()),
            attachments: Mutex::new(crate::v2_attachments::ClipboardAttachments::default()),
            directory_ops: Mutex::new(HashMap::new()),
            thumbnail_ops: Mutex::new(HashMap::new()),
            download_changed: tokio::sync::Notify::new(),
            fresh_clipboard: FreshClipboard::default(),
            history: Mutex::new(history::HistorySync::default()),
        }
    }

    /// This phone's group identity (the source of its own clipboard copies).
    pub fn device_id(&self) -> DeviceId {
        self.transport.identity.device
    }

    pub fn attach_fresh_clipboard_observer(&self, observer: Arc<dyn FreshClipboardObserver>) {
        self.fresh_clipboard.attach(observer);
    }

    /// Runs `f` on this session's own transfer store. History reads, single
    /// message deletes and cache clearing must go through here while the
    /// session runs: a second `TransferStore` on the same database cannot see
    /// or drop the live download attempts, so a deleted record could keep
    /// receiving bytes.
    pub fn with_store<R>(&self, f: impl FnOnce(&mut TransferStore) -> R) -> R {
        let result = f(&mut self.coordinator.store.lock().expect("transfer store mutex poisoned"));
        // A cleared cache or deleted record ends what a current-text read
        // was waiting for.
        self.download_changed.notify_waiters();
        result
    }

    /// Every transfer event goes to `sink` as it happens (see
    /// [`TransferEventSink`]); there is nothing to poll.
    pub fn set_event_sink(&self, sink: TransferEventSink) {
        *self.events.0.lock().expect("transfer events mutex poisoned") = Some(sink);
        if let Some(detail) = self.startup_error.lock().expect("startup error mutex poisoned").take() {
            self.error(None, None, detail);
        }
    }

    pub fn set_peers(&self, peers: Vec<Peer>) {
        *self.peers.lock().expect("peer mutex poisoned") = peers;
        self.transport.check_clock_targets();
        if let Err(error) = self.sync_history_group() {
            self.error(None, None, error.to_string());
        }
        if let Err(error) = self.update_history_peer() {
            self.error(None, None, error.to_string());
        }
        if let Err(error) = self.refresh_history_availability() {
            self.error(None, None, error.to_string());
        }
    }

    /// The ready peers and whether the first connect round ended. Waiting
    /// shares go out as soon as a target for them is ready; once the round
    /// ended, the rest fail with NoTarget. Routing (which may package a
    /// folder) runs on a blocking thread, never on the caller's.
    pub fn set_targets(self: &Arc<Self>, peers: Vec<Peer>, settled: bool) {
        self.set_peers(peers);
        self.targets_settled.store(settled, Ordering::SeqCst);
        self.wake_target_waits(settled);
        let due: Vec<_> = {
            let mut waiting = self.waiting.lock().expect("waiting share mutex poisoned");
            let (due, keep) = std::mem::take(&mut *waiting)
                .into_iter()
                .partition(|(payload, choice)| settled || self.has_target(payload, choice));
            *waiting = keep;
            due
        };
        for (payload, choice) in due {
            let session = self.clone();
            // The outcome reaches the host as the share's DeliveryEvent.
            let route = move || session.route(payload, choice);
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => drop(runtime.spawn_blocking(route)),
                Err(_) => route(),
            }
        }
    }

    /// Shares waiting for their offline target: those whose target is ready
    /// now start (their callers record and send them); once the first
    /// connect round ended, the rest say they wait. The events are pushed
    /// under the lock so `waiting: false` always precedes the share's own
    /// records (the sink only queues them).
    fn wake_target_waits(&self, settled: bool) {
        let mut waits = self.target_waits.lock().expect("target wait mutex poisoned");
        let ready: Vec<[u8; 16]> = waits
            .iter()
            .filter(|(_, wait)| self.has_target(&wait.payload, &wait.choice))
            .map(|(operation_id, _)| *operation_id)
            .collect();
        for operation_id in ready {
            let wait = waits.remove(&operation_id).expect("ready wait is present");
            if wait.announced {
                self.transport.emit(TransferEvent::ShareWaiting {
                    operation_id,
                    waiting: false,
                });
            }
            let _ = wait.wake.send(TargetWaitEnd::Ready);
        }
        if settled {
            for (operation_id, wait) in waits.iter_mut().filter(|(_, wait)| !wait.announced) {
                wait.announced = true;
                self.transport.emit(TransferEvent::ShareWaiting {
                    operation_id: *operation_id,
                    waiting: true,
                });
            }
        }
    }

    /// Ends target waits and pending publication clock barriers when the session stops.
    pub fn stop_target_waits(&self) {
        self.transport.abort_all_clock_publications((
            DeliveryFailure::Interrupted,
            "session stopped before clock synchronization".into(),
        ));
        self.target_waits.lock().expect("target wait mutex poisoned").clear();
    }

    /// `send_share` for a share that goes to the group's only other device
    /// without a device picker (native-rewrite.md, user 2026-11-11). While
    /// no target can take it, nothing is recorded: the share waits for the
    /// target to come online (the core's own presence, no host polling and
    /// no timer), announced by `ShareWaiting { waiting: true }` once the
    /// first connect round ended. When the target is ready it is recorded
    /// and sent as by `send_share` (`ShareWaiting { waiting: false }` first
    /// if the wait was announced). `cancel_share` while waiting returns
    /// `Cancelled` here and leaves no record; the session stopping returns
    /// a link error. The caller's staged files must stay until this returns.
    pub async fn send_share_when_online(
        self: &Arc<Self>,
        payload: SharePayload,
        choice: crate::ProxyChoice,
    ) -> Result<Vec<i64>, MobileV2TransportError> {
        let operation_id = payload.operation_id;
        if self
            .transport
            .cancelled_early
            .lock()
            .expect("early cancel mutex poisoned")
            .remove(&operation_id)
        {
            return Err(MobileV2TransportError::Cancelled);
        }
        let wake = {
            let mut waits = self.target_waits.lock().expect("target wait mutex poisoned");
            if self.has_target(&payload, &choice) {
                None
            } else {
                let (wake, woken) = tokio::sync::oneshot::channel();
                let announced = self.targets_settled.load(Ordering::SeqCst);
                if announced {
                    self.transport.emit(TransferEvent::ShareWaiting {
                        operation_id,
                        waiting: true,
                    });
                }
                waits.insert(
                    operation_id,
                    TargetWait {
                        payload: payload.clone(),
                        choice: choice.clone(),
                        announced,
                        wake,
                    },
                );
                Some(woken)
            }
        };
        if let Some(woken) = wake {
            let _guard = TargetWaitGuard {
                waits: &self.target_waits,
                operation_id,
            };
            match woken.await {
                Ok(TargetWaitEnd::Ready) => {}
                Ok(TargetWaitEnd::Cancelled) => return Err(MobileV2TransportError::Cancelled),
                Err(_) => return Err(link_error("the session stopped while the share waited for its target")),
            }
        }
        let session = self.clone();
        tokio::task::spawn_blocking(move || session.send_share(payload, choice))
            .await
            .map_err(link_error)?
    }

    /// True when `payload` could be routed now: any ready clipboard peer
    /// for a broadcast, an eligible proxy otherwise.
    fn has_target(&self, payload: &SharePayload, choice: &crate::ProxyChoice) -> bool {
        let peers = self.peers();
        match self.coordinator.plan_send(payload.clone(), &peers, choice.clone()) {
            Ok(crate::SendPlan::Broadcast { .. }) => peers
                .iter()
                .any(|peer| peer.state == crate::PeerState::Ready && peer.accepts_clipboard),
            Ok(crate::SendPlan::Proxy { .. } | crate::SendPlan::Direct { .. }) => true,
            Err(_) => false,
        }
    }
    pub fn request_current(&self, request_id: [u8; 16], max_items: u16) -> Result<(), MobileV2TransportError> {
        self.coordinator
            .request_current(request_id, max_items, &self.peers())
            .map(|_| ())
            .map_err(|error| match error {
                crate::coordinator::SendError::NoProxy => MobileV2TransportError::NoTarget,
                crate::coordinator::SendError::Transport(error) => MobileV2TransportError::Link(error.message),
            })
    }
    /// The text of `peer`'s current clipboard; None when it is empty or not
    /// text (an image is not downloaded).
    pub async fn receive_current_text(
        &self,
        peer: &str,
    ) -> Result<Option<CurrentClipboardText>, MobileV2TransportError> {
        Ok(match self.current_clipboard(peer, false).await? {
            Some(CurrentClipboard::Text(text)) => Some(text),
            _ => None,
        })
    }

    /// `peer`'s current clipboard for a one-tap copy: text, or an image
    /// downloaded on demand up to `MAX_CURRENT_IMAGE_BYTES`. Files, folders
    /// and rich-text-only content come back as `Unsupported`.
    pub async fn receive_current_clipboard(
        &self,
        peer: &str,
    ) -> Result<Option<CurrentClipboard>, MobileV2TransportError> {
        self.current_clipboard(peer, true).await
    }

    async fn current_clipboard(
        &self,
        peer: &str,
        images: bool,
    ) -> Result<Option<CurrentClipboard>, MobileV2TransportError> {
        let request_id = rand::random();
        let (send, receive) = tokio::sync::oneshot::channel();
        self.transport.send_current(peer, request_id, 64, Some(send))?;
        enum Picked {
            Done(Option<CurrentClipboard>),
            Image(Box<MessageRecord>, bool),
        }
        // The answer and a text body arrive within CURRENT_TIMEOUT; an image
        // download continues past it (`current_image`).
        let picked = tokio::time::timeout(CURRENT_TIMEOUT, async {
            let Some(selection) = receive
                .await
                .map_err(|_| link_error("clipboard current request expired"))??
            else {
                return Ok(Picked::Done(None));
            };
            loop {
                let changed = self.download_changed.notified();
                let record = self
                    .coordinator
                    .store
                    .lock()
                    .expect("transfer store mutex poisoned")
                    .by_id(selection.record_id)
                    .map_err(link_error)?;
                let Some(record) = record else {
                    return Err(MobileV2TransportError::NotFound(selection.record_id));
                };
                match record.kind {
                    AttachmentKind::Text => {}
                    AttachmentKind::Image if images => return Ok(Picked::Image(Box::new(record), selection.sensitive)),
                    kind => return Ok(Picked::Done(Some(CurrentClipboard::Unsupported { kind }))),
                }
                if record.download == crate::DownloadState::Complete {
                    let rich = {
                        let store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
                        // Its rich text follows the text (`rich_ready`).
                        if rich_downloading(&store, record.id) {
                            None
                        } else {
                            Some(rich_for(&store, record.id))
                        }
                    };
                    let Some(rich) = rich else {
                        changed.await;
                        continue;
                    };
                    return Ok(Picked::Done(record.text.map(|text| {
                        CurrentClipboard::Text(CurrentClipboardText {
                            text,
                            sensitive: selection.sensitive,
                            rich,
                            marker: clip_marker(&record.content_id),
                        })
                    })));
                }
                if record.source_availability != crate::SourceAvailability::Ready {
                    return Err(MobileV2TransportError::SourceUnavailable(record.source_availability));
                }
                if record.download != crate::DownloadState::Downloading {
                    return Err(link_error("clipboard text download failed"));
                }
                changed.await;
            }
        })
        .await
        .map_err(|_| link_error("clipboard current request timed out"))??;
        match picked {
            Picked::Done(current) => Ok(current),
            Picked::Image(record, sensitive) => self.current_image(*record, sensitive).await.map(Some),
        }
    }

    /// Downloads the current clipboard's image unless it is already here.
    /// The wait has no host timeout: the download ends complete or failed
    /// with the link, like a download from the conversation.
    async fn current_image(
        &self,
        record: MessageRecord,
        sensitive: bool,
    ) -> Result<CurrentClipboard, MobileV2TransportError> {
        let limit = MAX_CURRENT_IMAGE_BYTES;
        if record.size > limit {
            return Ok(CurrentClipboard::ImageTooLarge {
                size: record.size,
                limit,
            });
        }
        let mut started = false;
        loop {
            let changed = self.download_changed.notified();
            let record = self
                .coordinator
                .store
                .lock()
                .expect("transfer store mutex poisoned")
                .by_id(record.id)
                .map_err(link_error)?
                .ok_or_else(|| link_error("clipboard image was deleted"))?;
            match record.download {
                crate::DownloadState::Complete => {
                    let path = record
                        .local_path
                        .ok_or_else(|| link_error("clipboard image has no body"))?;
                    // An image of unknown size is checked once it is here.
                    let size = fs::metadata(&path).map_err(link_error)?.len();
                    if size > limit {
                        return Ok(CurrentClipboard::ImageTooLarge { size, limit });
                    }
                    return Ok(CurrentClipboard::Image(CurrentClipboardImage {
                        path,
                        mime: record.mime,
                        size,
                        sensitive,
                        marker: clip_marker(&record.content_id),
                    }));
                }
                crate::DownloadState::Downloading => changed.await,
                crate::DownloadState::Placeholder | crate::DownloadState::Failed if !started => {
                    self.coordinator
                        .start_download(record.id)
                        .map_err(|error| link_error(error.to_string()))?;
                    started = true;
                }
                crate::DownloadState::Placeholder | crate::DownloadState::Failed => {
                    return Err(link_error("clipboard image download failed"));
                }
            }
        }
    }

    /// Sends one share and records it in history as this device's own
    /// message(s): one record per item, `outgoing=true`, delivery `Sending`
    /// until every target settled (then `Sent` or `Failed` with a reason).
    /// Image and file bodies are copied into the store first, so the own
    /// bubble keeps its content and the upload never depends on a temporary
    /// host file (share extension staging). Returns the own record ids.
    ///
    /// The size limit is checked from file metadata before anything is
    /// copied. The copies are made without holding the store lock. This is
    /// blocking file I/O: hosts call it off their UI thread (the FFI runs it
    /// on a blocking worker). With no target ready yet while the session's
    /// first connect round runs, the share waits for one (`set_targets`).
    ///
    /// An error means nothing was recorded (too large, unreadable, storage).
    /// Once the own records exist the outcome, routing failures included,
    /// is their single `DeliveryEvent`.
    pub fn send_share(
        &self,
        mut payload: SharePayload,
        choice: crate::ProxyChoice,
    ) -> Result<Vec<i64>, MobileV2TransportError> {
        let operation_id = payload.operation_id;
        if self
            .transport
            .cancelled_early
            .lock()
            .expect("early cancel mutex poisoned")
            .remove(&operation_id)
        {
            // Cancelled before it reached us: nothing is recorded.
            return Err(MobileV2TransportError::Cancelled);
        }
        share_metadata(&mut payload.items)?;
        self.transport.begin_delivery(operation_id);
        let records = match self.record_outgoing(&mut payload) {
            Ok(records) => records,
            Err(error) => {
                self.transport.abandon_delivery(operation_id);
                return Err(error);
            }
        };
        let mut records = records;
        if let crate::ProxyChoice::Direct { device, name } = &choice
            && payload.items.iter().any(|item| item.kind() != AttachmentKind::Text)
        {
            let ids: Vec<i64> = records.iter().map(|record| record.id).collect();
            let store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            store.set_direct(&ids, device, Some(name))?;
            for record in &mut records {
                record.direct_peer = Some(device.clone());
                record.direct_peer_name = Some(name.clone());
            }
        }
        let record_ids: Vec<i64> = records.iter().map(|record| record.id).collect();
        self.transport.set_delivery_records(operation_id, record_ids.clone());
        for record in &records {
            self.announcement(record, false);
        }
        if self.transport.delivery_cancelled(operation_id) {
            // Cancelled while it was being recorded: the records end at once.
            self.transport.settle(operation_id, Err(None));
            return Ok(record_ids);
        }
        {
            let mut waiting = self.waiting.lock().expect("waiting share mutex poisoned");
            if !self.targets_settled.load(Ordering::SeqCst) && !self.has_target(&payload, &choice) {
                waiting.push((payload, choice));
                return Ok(record_ids);
            }
        }
        self.route(payload, choice);
        Ok(record_ids)
    }

    /// Routes one recorded share and releases its routing guard; targets
    /// that started settle later. A routing failure is recorded on the own
    /// records and reported by their DeliveryEvent.
    fn route(&self, payload: SharePayload, choice: crate::ProxyChoice) {
        let operation_id = payload.operation_id;
        let result = self
            .coordinator
            .plan_send(payload, &self.peers(), choice)
            .and_then(|plan| self.coordinator.send(plan));
        if let Err(error) = &result {
            let generic = match error {
                crate::coordinator::SendError::NoProxy => (DeliveryFailure::NoTarget, error.to_string()),
                crate::coordinator::SendError::Transport(_) => (DeliveryFailure::Interrupted, error.to_string()),
            };
            // A failure noted by the transport (no target, packaging error)
            // is more precise than the generic routing error.
            if let Some(delivery) = self
                .transport
                .deliveries
                .lock()
                .expect("delivery mutex poisoned")
                .get_mut(&operation_id)
            {
                delivery.failure.get_or_insert(generic);
            }
        }
        self.transport.settle(operation_id, Err(None));
    }

    /// Cancels an own share. Running: every target gets ClipboardShareCancel,
    /// uploads stop, the records end as Cancelled with the share's single
    /// DeliveryEvent. Not reached `send_share` yet, or waiting for its
    /// offline target (`send_share_when_online`): that call returns
    /// `Cancelled` and records nothing. Already finished, or unknown with
    /// own records: nothing happens.
    pub fn cancel_share(&self, operation_id: [u8; 16]) -> Result<(), MobileV2TransportError> {
        // Waiting for its offline target: nothing was recorded or sent.
        if let Some(wait) = self
            .target_waits
            .lock()
            .expect("target wait mutex poisoned")
            .remove(&operation_id)
        {
            let _ = wait.wake.send(TargetWaitEnd::Cancelled);
            return Ok(());
        }
        let waited = {
            let mut waiting = self.waiting.lock().expect("waiting share mutex poisoned");
            let before = waiting.len();
            waiting.retain(|(payload, _)| payload.operation_id != operation_id);
            waiting.len() != before
        };
        if self.transport.cancel(operation_id) {
            if waited {
                // Release the routing guard it held while waiting.
                self.transport.settle(operation_id, Err(None));
            }
            return Ok(());
        }
        let recorded = !self
            .coordinator
            .store
            .lock()
            .expect("transfer store mutex poisoned")
            .outgoing_records(operation_id)?
            .is_empty();
        if !recorded {
            self.transport
                .cancelled_early
                .lock()
                .expect("early cancel mutex poisoned")
                .insert(operation_id);
        }
        Ok(())
    }

    /// Puts the bodies of `payload` into staging without the store lock (a
    /// host-staged file is moved there, anything else is copied), then moves
    /// them to their attachment directories and records the own messages in
    /// one short locked step.
    fn record_outgoing(&self, payload: &mut SharePayload) -> Result<Vec<MessageRecord>, MobileV2TransportError> {
        let sender_id = hex(&self.transport.identity.device.0);
        let operation = hex(&payload.operation_id);
        let mut items = Vec::with_capacity(payload.items.len());
        // (staged directory, final attachment directory)
        let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
        let staging = self.transport.staging.clone();
        let result = (|| {
            for (index, item) in payload.items.iter_mut().enumerate() {
                let content_id = format!("out-{operation}-{index}");
                let image_size = match item {
                    crate::SendItem::Image { dimensions, .. } => *dimensions,
                    _ => None,
                };
                let mut copy_into =
                    |content_id: &str, body: Body<'_>, name: &str| -> Result<PathBuf, MobileV2TransportError> {
                        let directory = self
                            .coordinator
                            .store
                            .lock()
                            .expect("transfer store mutex poisoned")
                            .content_path(content_id)?;
                        let staged = staging.join(content_id);
                        fs::create_dir_all(&staged).map_err(io_failure)?;
                        moves.push((staged.clone(), directory.clone()));
                        match body {
                            // The host's staged copy is ours now: one rename on the
                            // same volume. A failed rename is an error, never a copy.
                            Body::Take(source) => fs::rename(source, staged.join(name)),
                            Body::Copy(source) => fs::copy(source, staged.join(name)).map(drop),
                            Body::Bytes(bytes) => fs::write(staged.join(name), bytes),
                        }
                        .map_err(io_failure)?;
                        Ok(directory.join(name))
                    };
                let mut rich = None;
                let (kind, display_name, mime, size, text, rich_text_format, local_path) = match item {
                    crate::SendItem::Text { text } => (
                        AttachmentKind::Text,
                        "Text".to_owned(),
                        Some("text/plain".to_owned()),
                        text.len() as u64,
                        Some(text.clone()),
                        None,
                        None,
                    ),
                    // A text message keeps one rich text next to its plain
                    // text, like a received one: HTML, else RTF (both still
                    // go on the wire).
                    crate::SendItem::RichText { text, html, rtf } => {
                        let (format, body, mime) = match (html, rtf) {
                            (Some(html), _) => (crate::RichTextFormat::Html, html, "text/html"),
                            (None, Some(rtf)) => (crate::RichTextFormat::Rtf, rtf, "application/rtf"),
                            (None, None) => return Err(link_error("rich text item without rich text")),
                        };
                        let rich_id = rich_content_id(&content_id);
                        let path = copy_into(
                            &rich_id,
                            Body::Bytes(body.as_bytes()),
                            &stored_file_name(AttachmentKind::RichText, "", Some(mime), Some(format)),
                        )?;
                        rich = Some(OutgoingRich {
                            content_id: rich_id,
                            format,
                            path,
                            size: body.len() as u64,
                        });
                        (
                            AttachmentKind::Text,
                            "Text".to_owned(),
                            Some("text/plain".to_owned()),
                            text.len() as u64,
                            Some(text.clone()),
                            Some(format),
                            None,
                        )
                    }
                    crate::SendItem::Image {
                        path,
                        encoded_size,
                        mime,
                        staged,
                        ..
                    } => {
                        let name = stored_file_name(AttachmentKind::Image, &file_name(path), Some(mime), None);
                        let copy = copy_into(&content_id, Body::of(path, *staged), &name)?;
                        *path = copy.to_string_lossy().into_owned();
                        (
                            AttachmentKind::Image,
                            name,
                            Some(mime.clone()),
                            *encoded_size,
                            None,
                            None,
                            Some(copy),
                        )
                    }
                    crate::SendItem::File {
                        path,
                        name,
                        size,
                        mime,
                        staged,
                    } => {
                        let stored = stored_file_name(AttachmentKind::File, name, mime.as_deref(), None);
                        let copy = copy_into(&content_id, Body::of(path, *staged), &stored)?;
                        *path = copy.to_string_lossy().into_owned();
                        (
                            AttachmentKind::File,
                            name.clone(),
                            mime.clone(),
                            *size,
                            None,
                            None,
                            Some(copy),
                        )
                    }
                    crate::SendItem::Folder { path, name } => (
                        AttachmentKind::Folder,
                        if name.is_empty() { file_name(path) } else { name.clone() },
                        None,
                        0,
                        None,
                        None,
                        None,
                    ),
                };
                items.push(OutgoingItem {
                    sender_id: sender_id.clone(),
                    content_id,
                    kind,
                    display_name,
                    mime,
                    size,
                    text,
                    rich_text_format,
                    local_path,
                    operation_id: payload.operation_id,
                    message_id: payload.message_id,
                    item_id: index as u32,
                    image_size,
                    rich,
                });
            }
            let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            for (staged, directory) in &moves {
                if directory.exists() {
                    fs::remove_dir_all(directory).map_err(storage_error)?;
                }
                fs::rename(staged, directory).map_err(io_failure)?;
            }
            Ok(store.insert_outgoing_sensitive(
                now_ms(),
                items,
                matches!(
                    payload.sensitivity,
                    oink_foundation::clipboard::Sensitivity::Concealed
                        | oink_foundation::clipboard::Sensitivity::Transient
                ),
            )?)
        })();
        if result.is_err() {
            for (staged, directory) in moves {
                let _ = fs::remove_dir_all(staged);
                let _ = fs::remove_dir_all(directory);
            }
        }
        result
    }

    /// Downloads one message. `open=true` makes it the single auto-open
    /// target (see `take_ready_auto_open`); `open=false` clears that slot.
    pub fn request_download(&self, message_id: i64, open: bool) -> Result<(), MobileV2TransportError> {
        let ready = self.coordinator.request_download(message_id, open)?;
        if ready {
            // End the store guard before announcement/history/event processing.
            let record = self
                .coordinator
                .store
                .lock()
                .expect("transfer store mutex poisoned")
                .by_id(message_id)
                .map_err(link_error)?;
            if let Some(record) = record {
                self.announcement(&record, true);
            }
        }
        Ok(())
    }

    /// Downloads a file entry returned by a directory listing. The file keeps
    /// the original offer, sender and proxy identity of its parent message
    /// but is stored as a child of the folder message, not as a new message.
    /// Returns the child record id; calling it again for the same entry
    /// reuses (and re-downloads) that child.
    pub fn request_directory_file(
        &self,
        operation_id: [u8; 16],
        folder_message_id: i64,
        entry: DirectoryEntry,
        open: bool,
    ) -> Result<i64, MobileV2TransportError> {
        check_directory_file(&entry)?;
        let record = {
            let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            let (parent, offer) = received_folder(&store, folder_message_id)?;
            if parent.direct_peer.is_some() {
                // A folder received phone to phone is on this phone: its
                // files are complete child records already (none to fetch).
                let child = store
                    .by_content_id(&file_content_id(&offer, entry.id, entry.version).map_err(protocol_error)?)
                    .map_err(link_error)?
                    .filter(|child| child.parent_id == Some(parent.id))
                    .ok_or(MobileV2TransportError::NotFound(folder_message_id))?;
                drop(store);
                self.request_download(child.id, open)?;
                return Ok(child.id);
            }
            insert_directory_child(&mut store, &parent, &offer, operation_id, entry)?
        };
        self.announcement(&record, false);
        self.request_download(record.id, open)?;
        Ok(record.id)
    }

    /// Asks the source PC for a preview of image file `entry` of a PC folder
    /// message when its folder page row becomes visible (spec 2026-11-11
    /// 「文件夹页」): the same request, cache and states as an image message
    /// (`request_thumbnail`), on the entry's child record, which is created
    /// (not downloaded) when the entry has none yet; downloading the file
    /// later uses the same record. Returns that child's id. `None` when the
    /// core fetches no thumbnail for the entry: it is not a file with an
    /// image name, or the folder was received phone to phone (its files are
    /// on this phone; the host makes their thumbnails itself).
    pub fn request_directory_thumbnail(
        &self,
        folder_message_id: i64,
        entry: DirectoryEntry,
    ) -> Result<Option<i64>, MobileV2TransportError> {
        if entry.kind != "file" {
            return Ok(None);
        }
        check_directory_file(&entry)?;
        if image_mime_for_name(&entry.name).is_none() {
            return Ok(None);
        }
        let child = {
            let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            let (parent, offer) = received_folder(&store, folder_message_id)?;
            if parent.direct_peer.is_some() {
                return Ok(None);
            }
            let existing = store
                .by_content_id(&file_content_id(&offer, entry.id, entry.version).map_err(protocol_error)?)
                .map_err(link_error)?
                .filter(|child| child.parent_id == Some(parent.id));
            match existing {
                Some(child) => child,
                None => {
                    // The child's own operation id, as a download of it
                    // would get from the host (`request_directory_file`).
                    let mut operation_id = [0u8; 16];
                    rand::Rng::fill(&mut rand::thread_rng(), &mut operation_id);
                    insert_directory_child(&mut store, &parent, &offer, operation_id, entry)?
                }
            }
        };
        // Announces the child when a request goes out (Loading), and again
        // with the answer, like an image message.
        self.request_thumbnail(child.id)?;
        Ok(Some(child.id))
    }

    /// Asks the source PC for a preview of an image message or of an image
    /// file of a PC folder (protocol-v2 §4.4). Images are never downloaded
    /// automatically: hosts call this when the message (or folder page row)
    /// becomes visible, and the original is downloaded only when the user
    /// opens it (`request_download`). It is a no-op (`Ok(false)`) unless the
    /// record is a received image, not downloaded or downloading, without a
    /// final thumbnail state, and its source is online. The answer also
    /// records the original's size, which Mac and Windows offers announce as
    /// unknown for bitmaps.
    pub fn request_thumbnail(&self, record_id: i64) -> Result<bool, MobileV2TransportError> {
        let (record, peer, read, operation_id) = {
            let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            let record = store
                .by_id(record_id)
                .map_err(link_error)?
                .ok_or_else(|| link_error("message no longer exists"))?;
            if !thumbnail_wanted(&record) || store.thumbnail_attempt(record.id).is_some() {
                return Ok(false);
            }
            let offer = parse_content_id(&record.content_id).map_err(protocol_error)?;
            let peer = store
                .history_source(record.id)
                .map_err(link_error)?
                .unwrap_or_else(|| record.proxy_id.clone().unwrap_or(record.sender_id.clone()));
            if !self
                .peers()
                .iter()
                .any(|candidate| candidate.id == peer && candidate.state == crate::PeerState::Ready)
            {
                return Ok(false);
            }
            let mut operation_id = [0u8; 16];
            rand::Rng::fill(&mut rand::thread_rng(), &mut operation_id);
            let mut raw = RawMap::from([
                (1, id16(operation_id).write()),
                (2, offer.write()),
                (7, THUMBNAIL_EDGE.write()),
                (8, THUMBNAIL_BYTES.write()),
            ]);
            match parse_file_content(&record.content_id) {
                Some((node, version)) => {
                    raw.insert(5, node.write());
                    raw.insert(6, version.write());
                }
                None => {
                    raw.insert(3, record.item_id.write());
                    raw.insert(4, record.representation_id.write());
                }
            }
            let read = dto::ContentThumbnailRead::from_raw(raw).map_err(protocol_error)?;
            let attempt = store.begin_thumbnail(record.id).map_err(link_error)?;
            self.thumbnail_ops
                .lock()
                .expect("thumbnail operation mutex poisoned")
                .insert(
                    operation_id,
                    ThumbnailOp {
                        record: record.id,
                        attempt,
                    },
                );
            (record, peer, read, operation_id)
        };
        let sent = self.transport.link.send_low(
            &peer,
            ClipboardLowOperation::request(operation_id, v2::Message::ContentThumbnailRead(read)),
        );
        if let Err(error) = sent {
            self.finish_thumbnail(operation_id, crate::ThumbnailState::Failed);
            return Err(error);
        }
        let refreshed = self
            .coordinator
            .store
            .lock()
            .expect("transfer store mutex poisoned")
            .by_id(record.id);
        if let Ok(Some(record)) = refreshed {
            self.announcement(&record, false);
        }
        Ok(true)
    }

    /// Ends a thumbnail request without a preview and publishes the record.
    fn finish_thumbnail(&self, operation_id: [u8; 16], state: crate::ThumbnailState) {
        let Some(ThumbnailOp { record: id, attempt }) = self
            .thumbnail_ops
            .lock()
            .expect("thumbnail operation mutex poisoned")
            .remove(&operation_id)
        else {
            return;
        };
        let record = {
            let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            if store.finish_thumbnail(id, attempt, state).is_err() {
                return;
            }
            store.by_id(id).ok().flatten()
        };
        if let Some(record) = record {
            self.announcement(&record, false);
        }
    }

    /// Applies the PC's `ContentThumbnail` answer.
    fn receive_thumbnail(
        &self,
        operation_id: [u8; 16],
        read: &dto::ContentThumbnailRead,
        message: v2::Message,
    ) -> Result<bool, MobileV2TransportError> {
        let v2::Message::ContentThumbnail(reply) = message else {
            return Err(protocol_error(v2::ErrorCode::StateConflict.into()));
        };
        if reply.operation_id().map_err(protocol_error)? != read.operation_id().map_err(protocol_error)? {
            return Err(protocol_error(v2::ErrorCode::StateConflict.into()));
        }
        let state = match reply.status().map_err(protocol_error)? {
            dto::ThumbnailStatus::Ready => {
                let (Some(mime), Some(width), Some(height), Some(bytes)) = (
                    reply.mime().map_err(protocol_error)?,
                    reply.width().map_err(protocol_error)?,
                    reply.height().map_err(protocol_error)?,
                    reply.bytes().map_err(protocol_error)?,
                ) else {
                    return Err(protocol_error(v2::ErrorCode::MalformedFrame.into()));
                };
                let source_bytes = reply.source_bytes().map_err(protocol_error)?;
                let Some(ThumbnailOp { record: id, attempt }) = self
                    .thumbnail_ops
                    .lock()
                    .expect("thumbnail operation mutex poisoned")
                    .remove(&operation_id)
                else {
                    return Ok(true);
                };
                let record = {
                    let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
                    store
                        .complete_thumbnail(
                            id,
                            attempt,
                            crate::store::ReceivedThumbnail {
                                mime: &mime.0,
                                width,
                                height,
                                bytes: &bytes.0,
                                source_bytes,
                            },
                        )
                        .map_err(link_error)?;
                    store.by_id(id).map_err(link_error)?
                };
                if let Some(record) = record {
                    self.announcement(&record, false);
                }
                return Ok(true);
            }
            dto::ThumbnailStatus::Expired => crate::ThumbnailState::Expired,
            dto::ThumbnailStatus::Unsupported => crate::ThumbnailState::Unsupported,
            dto::ThumbnailStatus::Failed | dto::ThumbnailStatus::Unknown(_) => crate::ThumbnailState::Failed,
        };
        let retry = if state == crate::ThumbnailState::Expired {
            self.thumbnail_ops
                .lock()
                .expect("thumbnail operation mutex poisoned")
                .get(&operation_id)
                .map(|op| op.record)
        } else {
            None
        };
        let source = retry.and_then(|id| self.with_store(|s| s.history_source(id)).ok().flatten());
        self.finish_thumbnail(operation_id, state);
        if let (Some(id), Some(source)) = (retry, source)
            && self.reject_history_holder(id, &source)?
        {
            self.with_store(|s| s.reset_history_thumbnail(id))?;
            self.request_thumbnail(id)?;
        }
        Ok(true)
    }

    /// Clears the auto-open slot (leaving the transfer page, tapping another
    /// item, or dismissing the pending open).
    pub fn cancel_auto_open(&self) {
        self.coordinator.cancel_auto_open();
    }

    /// Takes the auto-open target once its file is complete. Returns `None`
    /// while the download is still running or after the slot was cleared.
    pub fn take_ready_auto_open(&self) -> Result<Option<crate::MessageRecord>, MobileV2TransportError> {
        self.coordinator.take_ready_auto_open().map_err(link_error)
    }

    pub fn receive_frame(&self, peer_id: &str, frame: &[u8]) -> Result<(), MobileV2TransportError> {
        let message = OpaqueFrame::parse_exact(frame, MAX_FRAME)
            .map_err(|e| MobileV2TransportError::Protocol(e.to_string()))?
            .decode()
            .map_err(|e| MobileV2TransportError::Protocol(e.to_string()))?;
        self.receive_message(peer_id, message)
    }

    pub fn receive_message(&self, peer_id: &str, message: v2::Message) -> Result<(), MobileV2TransportError> {
        self.transport.emit(TransferEvent::MessageReceived {
            peer_id: peer_id.into(),
            message: message_name(&message).into(),
        });
        match message {
            message @ (v2::Message::HistorySummary(_)
            | v2::Message::HistoryIndexRequest(_)
            | v2::Message::HistoryPull(_)
            | v2::Message::HistoryIndex(_)
            | v2::Message::HistoryRecords(_)
            | v2::Message::HistoryDelta(_)) => self.receive_history(peer_id, message),
            message @ (v2::Message::AttachmentHead(_) | v2::Message::AttachmentItems(_)) => {
                let offer = self
                    .attachments
                    .lock()
                    .expect("clipboard attachment mutex poisoned")
                    .push(peer_id, message)
                    .map_err(protocol_error)?;
                if let Some(offer) = offer {
                    self.receive_offer(peer_id, &offer, false).map_err(protocol_error)?;
                }
                Ok(())
            }
            v2::Message::ClipboardCurrentQuery(query) => self.reply_clock_query(peer_id, &query),
            v2::Message::ClipboardOffer(offer) => self
                .receive_offer(peer_id, &offer, false)
                .map(|_| ())
                .map_err(protocol_error),
            v2::Message::ClipboardHeadUpdate(update) => {
                let group = self.transport.link.group_id().unwrap_or(self.transport.identity.group);
                if update.group().map_err(protocol_error)?.0 != group.0 {
                    return Err(protocol_error(v2::ErrorCode::StateConflict.into()));
                }
                self.validate_boot(peer_id, &update.responder().map_err(protocol_error)?)
                    .map_err(protocol_error)?;
                let offer = update.offer().map_err(protocol_error)?;
                let origin = offer
                    .id()
                    .and_then(|id| id.clip())
                    .and_then(|clip| clip.origin())
                    .and_then(|origin| origin.public_key())
                    .map_err(protocol_error)?;
                self.receive_offer(&hex(&origin.0), &offer, update.fresh().map_err(protocol_error)?)
                    .map(|_| ())
                    .map_err(protocol_error)
            }
            v2::Message::ClipboardCurrentResult(result) => {
                self.receive_current(peer_id, &result).map_err(protocol_error)
            }
            v2::Message::ContentOffer(offer) | v2::Message::ClipboardContentOffer(offer) => {
                self.validate_boot(peer_id, &offer.source().map_err(protocol_error)?)
                    .map_err(protocol_error)?;
                self.ingest(content_announcement(&offer).map_err(protocol_error)?)
                    .map(|_| ())
                    .map_err(protocol_error)
            }
            v2::Message::ClipboardShareBegin(begin) => self.accept_share(peer_id, &begin),
            v2::Message::ClipboardShareAccept(accept) => self
                .transport
                .share_accepted(peer_id, accept.operation_id().map_err(protocol_error)?.0),
            v2::Message::ClipboardShareResult(result) => {
                self.transport.finish_share(
                    peer_id,
                    result.operation_id().map_err(protocol_error)?.0,
                    share_status_result(result.status().map_err(protocol_error)?),
                );
                Ok(())
            }
            v2::Message::ClipboardShareReject(result) => {
                self.transport.share_rejected(
                    peer_id,
                    result.operation_id().map_err(protocol_error)?.0,
                    result.status().map_err(protocol_error)?,
                    result.reason().map_err(protocol_error)?,
                );
                Ok(())
            }
            v2::Message::ClipboardShareCancel(cancel) => {
                let operation_id = cancel.operation_id().map_err(protocol_error)?.0;
                self.cancel_incoming(peer_id, operation_id);
                self.direct_cancelled_by_sender(peer_id, operation_id);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn validate_boot(&self, peer: &str, claimed: &dto::Boot) -> Result<(), v2::Error> {
        if claimed.device_id()? != device_id(peer)?
            || self
                .transport
                .link
                .peer_boot(peer)
                .is_some_and(|authenticated| *claimed != boot(authenticated))
        {
            return Err(v2::ErrorCode::StateConflict.into());
        }
        Ok(())
    }

    fn receive_current(&self, peer: &str, result: &dto::ClipboardCurrentResult) -> Result<(), v2::Error> {
        let operation_id = result.request_id()?.0;
        if self.transport.is_clock_query(peer, operation_id) {
            let clock = result
                .responder()
                .and_then(|boot| self.validate_boot(peer, &boot))
                .and_then(|()| result.clock());
            return self.transport.receive_publication_clock(peer, operation_id, clock);
        }
        self.validate_boot(peer, &result.responder()?)?;
        let pending = self
            .transport
            .current_queries
            .lock()
            .expect("current query mutex poisoned")
            .remove(&(peer.to_owned(), operation_id));
        let Some(pending) = pending.filter(|pending| pending.deadline > Instant::now()) else {
            return Ok(());
        };
        let clock = result.clock()?;
        self.with_store(|store| store.observe_history_clock(clock))
            .map_err(|_| v2::ErrorCode::StateConflict)?;
        let found = self.current_result(result);
        let invalid = match &found {
            Err(MobileV2TransportError::Protocol(detail)) => Some(detail.clone()),
            _ => None,
        };
        if let Some(reply) = pending.reply {
            let _ = reply.send(found);
        }
        // An unavailable current clipboard is an answer, not a peer error.
        invalid.map_or(Ok(()), |_| Err(v2::ErrorCode::StateConflict.into()))
    }

    /// The selected record of a current result. Empty is `None`; an
    /// offline, expired or unsupported source keeps its own error.
    fn current_result(&self, result: &dto::ClipboardCurrentResult) -> CurrentReply {
        use crate::SourceAvailability;
        match result.status().map_err(protocol_error)? {
            dto::CurrentResultStatus::Ready => {
                let offer = result
                    .offer()
                    .map_err(protocol_error)?
                    .ok_or_else(|| protocol_error(v2::ErrorCode::StateConflict.into()))?;
                let origin = offer
                    .id()
                    .and_then(|id| id.clip())
                    .and_then(|clip| clip.origin())
                    .map_err(protocol_error)?;
                let provider = result.source().map_err(protocol_error)?.unwrap_or(origin.clone());
                let source = hex(&origin.public_key().map_err(protocol_error)?.0);
                let provider = hex(&provider.public_key().map_err(protocol_error)?.0);
                if !self.transport.link.is_member(&provider) {
                    return Err(protocol_error(v2::ErrorCode::StateConflict.into()));
                }
                if self.transport.link.is_peer_ready(&provider) == Some(false)
                    || !self.peers().iter().any(|peer| {
                        peer.id == provider && peer.state == crate::PeerState::Ready && peer.accepts_clipboard
                    })
                {
                    return Err(MobileV2TransportError::SourceUnavailable(
                        SourceAvailability::SourceOffline,
                    ));
                }
                let sensitive = matches!(
                    offer.sensitivity().map_err(protocol_error)?,
                    dto::Sensitivity::Concealed | dto::Sensitivity::Transient
                );
                let selection = self
                    .receive_offer(&source, &offer, false)
                    .map_err(protocol_error)?
                    .map(|record_id| CurrentSelection { record_id, sensitive });
                if let Some(selection) = selection {
                    self.with_store(|store| store.set_history_source(selection.record_id, &provider))?;
                    let record = self.with_store(|store| store.by_id(selection.record_id))?;
                    if record.is_some_and(|r| {
                        r.kind == AttachmentKind::Text
                            && r.text.is_none()
                            && r.download != crate::DownloadState::Downloading
                    }) {
                        self.coordinator
                            .start_download(selection.record_id)
                            .map_err(|e| link_error(e.to_string()))?;
                    }
                }
                // Reading the current clipboard is the user asking for it: a
                // sensitive text body, never fetched automatically, is fetched now.
                if let Some(selection) = selection.filter(|selection| selection.sensitive) {
                    let placeholder_text = self
                        .coordinator
                        .store
                        .lock()
                        .expect("transfer store mutex poisoned")
                        .by_id(selection.record_id)
                        .map_err(link_error)?
                        .is_some_and(|record| {
                            record.kind == AttachmentKind::Text && record.download == crate::DownloadState::Placeholder
                        });
                    if placeholder_text {
                        self.coordinator
                            .start_download(selection.record_id)
                            .map_err(|error| link_error(error.to_string()))?;
                    }
                }
                Ok(selection)
            }
            dto::CurrentResultStatus::Empty => Ok(None),
            dto::CurrentResultStatus::SourceOffline => Err(MobileV2TransportError::SourceUnavailable(
                SourceAvailability::SourceOffline,
            )),
            dto::CurrentResultStatus::Expired => Err(MobileV2TransportError::Expired),
            _ => Err(MobileV2TransportError::SourceUnavailable(
                SourceAvailability::Unsupported,
            )),
        }
    }

    fn receive_offer(
        &self,
        peer: &str,
        offer: &dto::ClipboardOffer,
        fresh: bool,
    ) -> Result<Option<crate::MessageId>, v2::Error> {
        // Returns the record a current-clipboard request answers with.
        let source = device_id(peer)?;
        let sender = offer.real_sender()?;
        let proxy = offer.proxy()?;
        if offer.id()?.clip()?.origin()? != source
            || (sender == source && proxy.is_some())
            || (sender != source && proxy.as_ref() != Some(&source))
            || !self.transport.link.is_member(&hex(&sender.public_key()?.0))
            || !self.transport.link.is_member(peer)
        {
            return Err(v2::ErrorCode::StateConflict.into());
        }
        if sender == device(self.transport.identity.device) {
            let shared = history::domain_offer(&offer.id()?)?;
            let store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            let own = store
                .outgoing_records(offer.operation_id()?.0)
                .map_err(|_| v2::ErrorCode::StateConflict)?;
            for record in own {
                store
                    .link_shared(record.id, shared)
                    .map_err(|_| v2::ErrorCode::StateConflict)?;
                store
                    .set_history_order(
                        record.id,
                        offer.time_ms()?.min(i64::MAX as u64) as i64,
                        oink_foundation::clipboard_history::OrderKey {
                            clock: offer.clock()?,
                            origin: shared.clip.origin,
                            seq: shared.clip.seq,
                            generation: shared.generation,
                        },
                    )
                    .map_err(|_| v2::ErrorCode::StateConflict)?;
            }
        }
        if !self
            .purge_sensitive_for_head(offer)
            .map_err(|_| v2::ErrorCode::StateConflict)?
        {
            if sender == device(self.transport.identity.device) {
                let shared = history::domain_offer(&offer.id()?)?;
                let removed = self
                    .with_store(|store| {
                        let records = store.shared_records(shared)?;
                        let mut removed = Vec::new();
                        for record in records {
                            if record.sensitive {
                                store.delete_message(record.id)?;
                                removed.push(record.id);
                            }
                        }
                        Ok::<_, crate::StoreError>(removed)
                    })
                    .map_err(|_| v2::ErrorCode::StateConflict)?;
                if !removed.is_empty() {
                    self.transport.emit(TransferEvent::Removed { record_ids: removed });
                }
            }
            return Ok(None);
        }
        let shared = history::domain_offer(&offer.id()?)?;
        let hidden = self
            .with_store(|store| store.load_history().map(|history| history.hidden.contains(&shared)))
            .map_err(|_| v2::ErrorCode::StateConflict)?;
        if hidden {
            if fresh {
                self.fresh_clipboard
                    .group_changed(offer.id().ok().and_then(|id| offer_marker(&id)));
            }
            return Ok(None);
        }
        if sender == device(self.transport.identity.device) {
            // A proxy re-announces what this phone sent (real_sender = us,
            // same operation id). The own records already show it, so the
            // echo is not stored again; a current-clipboard request is
            // answered from the own records.
            let shared = history::domain_offer(&offer.id()?)?;
            let store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            let own = store.shared_records(shared).map_err(|_| v2::ErrorCode::StateConflict)?;
            let own = if own.is_empty() {
                store
                    .outgoing_records(offer.operation_id()?.0)
                    .map_err(|_| v2::ErrorCode::StateConflict)?
            } else {
                own
            };
            for record in &own {
                store
                    .link_shared(record.id, shared)
                    .map_err(|_| v2::ErrorCode::StateConflict)?;
            }
            return Ok(current_record(own.iter().map(|record| (record.kind, record.id))));
        }
        let automatic = fresh && offer.sensitivity()? == dto::Sensitivity::Normal;
        if fresh {
            // The group's current clip is now this one, not whatever this
            // phone's clipboard holds (privileged capture, D11).
            self.fresh_clipboard
                .group_changed(offer.id().ok().and_then(|id| offer_marker(&id)));
        }
        let mut text = None;
        let mut announcements = offer_announcements(offer)?;
        if self.transport.link.is_peer_ready(peer) == Some(false) {
            for (announcement, rich) in &mut announcements {
                announcement.source.availability = crate::SourceAvailability::SourceOffline;
                if let Some(rich) = rich {
                    rich.source.availability = crate::SourceAvailability::SourceOffline;
                }
            }
        }
        // Every item is its own record, also when one offer carries many
        // files: hosts show the records of one operation as a batch
        // (stacked cards), each downloadable and openable by itself.
        let mut records = Vec::new();
        for (announcement, rich) in announcements {
            let ticket = (automatic
                && text.is_none()
                && announcement.kind == AttachmentKind::Text
                && announcement.source.is_online())
            .then(|| {
                self.fresh_clipboard
                    .begin(&announcement.content_id, &announcement.sender_id)
            })
            .flatten();
            let record = self.ingest_with_rich(announcement, rich)?;
            {
                let store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
                let id = history::domain_offer(&offer.id()?)?;
                store
                    .link_shared(record.id, id)
                    .map_err(|_| v2::ErrorCode::StateConflict)?;
                store
                    .set_history_order(
                        record.id,
                        offer.time_ms()?.min(i64::MAX as u64) as i64,
                        oink_foundation::clipboard_history::OrderKey {
                            clock: offer.clock()?,
                            origin: id.clip.origin,
                            seq: id.clip.seq,
                            generation: id.generation,
                        },
                    )
                    .map_err(|_| v2::ErrorCode::StateConflict)?;
            }
            if let Some(ticket) = ticket {
                // The Low response can complete concurrently with B0. Register
                // the ticket while holding the same store lock as completion.
                let store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
                let mut fetch = None;
                if let Ok(Some(current)) = store.by_id(record.id) {
                    if let Some(text) = current.text.as_deref() {
                        // Already here (announced again): its rich text too,
                        // unless that is still to come.
                        fetch = rich_to_fetch(&store, current.id);
                        if fetch.is_some() || rich_downloading(&store, current.id) {
                            self.fresh_clipboard.waiting(&current.content_id, record.id, ticket);
                        } else {
                            self.fresh_clipboard.complete(
                                ticket,
                                &current.sender_id,
                                text,
                                rich_for(&store, current.id),
                                clip_marker(&current.content_id),
                            );
                        }
                    } else if current.download == crate::DownloadState::Downloading {
                        self.fresh_clipboard.waiting(&current.content_id, record.id, ticket);
                    }
                }
                drop(store);
                if let Some(child) = fetch {
                    self.fetch_rich(record.id, child);
                }
            }
            if text.is_none() && record.kind == AttachmentKind::Text {
                text = Some(record.id);
            }
            records.push((record.kind, record.id));
        }
        Ok(current_record(records))
    }

    fn ingest(&self, announcement: IncomingAnnouncement) -> Result<crate::MessageRecord, v2::Error> {
        self.ingest_with_rich(announcement, None)
    }

    fn ingest_with_rich(
        &self,
        announcement: IncomingAnnouncement,
        rich: Option<IncomingAnnouncement>,
    ) -> Result<crate::MessageRecord, v2::Error> {
        let ingested = self
            .coordinator
            .ingest_with_rich(now_ms(), announcement, rich)
            .map_err(|_| v2::ErrorCode::StateConflict)?;
        let record = ingested.record;
        if let Some(error) = ingested.download_error {
            self.transport.emit(TransferEvent::Error {
                operation_id: Some(record.operation_id),
                record_id: Some(record.id),
                detail: error.message,
            });
        }
        self.announcement(&record, false);
        Ok(record)
    }

    /// Starts downloading `child`, the rich text of text message `parent`.
    /// When it cannot start, the message is complete as plain text.
    fn fetch_rich(&self, parent: crate::MessageId, child: crate::MessageId) {
        if let Err(error) = self.coordinator.start_download(child) {
            tracing::debug!(%error, "rich text of a text message was not fetched");
            self.rich_ready(parent);
        }
    }

    /// The rich text of text message `parent` arrived or will not: the
    /// message's clipboard write (if one waits) goes ahead with what is
    /// here, and hosts redraw the message.
    fn rich_ready(&self, parent: crate::MessageId) {
        let store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
        let Ok(Some(record)) = store.by_id(parent) else { return };
        if let Some(text) = record.text.as_deref() {
            self.fresh_clipboard.downloaded(
                record.id,
                &record.sender_id,
                text,
                rich_for(&store, record.id),
                clip_marker(&record.content_id),
            );
        }
        drop(store);
        self.announcement(&record, false);
        self.download_changed.notify_waiters();
    }

    fn announcement(&self, record: &crate::MessageRecord, auto_open_ready: bool) {
        self.transport.emit(TransferEvent::Announcement {
            record_id: record.id,
            content_id: record.content_id.clone(),
            parent_id: record.parent_id,
            auto_open_ready,
            sensitive: record.sensitive,
        });
    }

    pub(crate) fn upload_opened(&self, peer: &str, operation_id: [u8; 16]) {
        self.transport.upload_opened(peer, operation_id);
    }

    pub(crate) fn send_progress(&self, operation_id: [u8; 16], sent: u64, total: u64) {
        self.transport.emit(TransferEvent::SendProgress {
            operation_id,
            record_ids: self.transport.delivery_records(operation_id),
            sent,
            total: Some(total),
        });
    }

    pub(crate) fn error(&self, operation_id: Option<[u8; 16]>, record_id: Option<i64>, detail: String) {
        self.transport.emit(TransferEvent::Error {
            operation_id,
            record_id,
            detail,
        });
    }

    fn accept_share(&self, peer: &str, begin: &dto::ClipboardShareBegin) -> Result<(), MobileV2TransportError> {
        // A share directed to this phone is a phone-to-phone transfer; a
        // broadcast carries text only.
        if begin.delivery().map_err(protocol_error)? == dto::ShareDelivery::Directed {
            return self.accept_direct(peer, begin);
        }
        let operation_id = begin.operation_id().map_err(protocol_error)?.0;
        let source = begin.source().map_err(protocol_error)?;
        self.validate_boot(peer, &source).map_err(protocol_error)?;
        let sender = begin.origin_sender().map_err(protocol_error)?;
        let count = begin.item_count().map_err(protocol_error)?;
        let bytes = begin.bytes().map_err(protocol_error)?.unwrap_or(u64::MAX);
        let supported = sender == source.device_id().map_err(protocol_error)?
            && begin.delivery().map_err(protocol_error)? == dto::ShareDelivery::Broadcast
            && begin
                .target()
                .map_err(protocol_error)?
                .is_none_or(|target| target == boot(self.transport.identity.boot))
            && matches!(
                begin.kind().map_err(protocol_error)?,
                dto::ShareKind::Text | dto::ShareKind::Url
            )
            && (1..=MAX_SHARE_ITEMS).contains(&count)
            && bytes <= MAX_DIRECT_TEXT * u64::from(count);
        let accepted = if supported {
            let mut incoming = self.incoming.lock().expect("incoming share mutex poisoned");
            incoming.retain(|_, share| share.started || share.created.elapsed() < SHARE_LIFETIME);
            let key = (peer.to_owned(), operation_id);
            let reserved = incoming.values().map(|share| share.expected_bytes).sum::<u64>();
            if incoming.len() < MAX_PENDING
                && !incoming.keys().any(|(source, _)| source == peer)
                && reserved + bytes <= MAX_DIRECT_BYTES
            {
                incoming.insert(
                    key,
                    IncomingShare {
                        sensitive: begin
                            .sensitivity()
                            .map_err(protocol_error)?
                            .is_some_and(|v| matches!(v, dto::Sensitivity::Concealed | dto::Sensitivity::Transient)),
                        message_id: begin.message_id().map_err(protocol_error)?.0,
                        fresh_ticket: matches!(
                            begin.kind().map_err(protocol_error)?,
                            dto::ShareKind::Text | dto::ShareKind::Url
                        )
                        .then(|| {
                            self.fresh_clipboard
                                .begin(&format!("text-{peer}-{}", hex(&operation_id)), peer)
                        })
                        .flatten(),
                        source,
                        count,
                        expected_bytes: bytes,
                        bytes: 0,
                        text: None,
                        items: Vec::new(),
                        created: Instant::now(),
                        started: false,
                    },
                );
                true
            } else {
                false
            }
        } else {
            false
        };
        if accepted {
            // Another device's text is now the newest content (D11).
            self.fresh_clipboard.group_changed(None);
        }
        let raw = RawMap::from([(1, id16(operation_id).write())]);
        let message = if accepted {
            v2::Message::ClipboardShareAccept(dto::ClipboardShareAccept::from_raw(raw).map_err(protocol_error)?)
        } else {
            let mut raw = raw;
            raw.insert(2, dto::ShareStatus::Unsupported.write());
            v2::Message::ClipboardShareReject(dto::ClipboardShareReject::from_raw(raw).map_err(protocol_error)?)
        };
        let result = self.transport.send_message(peer, message);
        if result.is_err() && accepted {
            self.cancel_incoming(peer, operation_id);
        }
        result
    }

    pub(crate) async fn receive_share_stream(
        &self,
        peer: &str,
        first: v2::Message,
        stream: &mut LowStream,
    ) -> Result<(), MobileV2TransportError> {
        let operation_id = match &first {
            v2::Message::ClipboardShareTextChunk(chunk) => chunk.operation_id(),
            v2::Message::ClipboardShareTextEnd(end) => end.operation_id(),
            v2::Message::ContentOffer(offer) => offer.operation_id(),
            _ => return Err(protocol_error(v2::ErrorCode::UnsupportedValue.into())),
        }
        .map_err(protocol_error)?
        .0;
        if self.has_direct(peer, operation_id) {
            return self.receive_direct(peer, operation_id, first, stream).await;
        }
        let key = (peer.to_owned(), operation_id);
        let share = {
            let mut incoming = self.incoming.lock().expect("incoming share mutex poisoned");
            let share = incoming
                .get_mut(&key)
                .filter(|share| !share.started && share.created.elapsed() < SHARE_LIFETIME)
                .ok_or_else(|| link_error("share was not accepted"))?;
            share.started = true;
            share.clone()
        };
        let result = tokio::time::timeout(
            SHARE_LIFETIME,
            self.receive_share_body(peer, operation_id, share, first, stream),
        )
        .await
        .unwrap_or_else(|_| Err(link_error("clipboard share body timed out")));
        self.cancel_incoming(peer, operation_id);
        let status = if result.is_ok() {
            dto::ShareStatus::Complete
        } else {
            dto::ShareStatus::Failed
        };
        let reply = dto::ClipboardShareResult::from_raw([(1, id16(operation_id).write()), (2, status.write())].into())
            .map_err(protocol_error)?;
        self.transport
            .send_message(peer, v2::Message::ClipboardShareResult(reply))?;
        result
    }

    async fn receive_share_body(
        &self,
        peer: &str,
        operation_id: [u8; 16],
        mut share: IncomingShare,
        first: v2::Message,
        stream: &mut LowStream,
    ) -> Result<(), MobileV2TransportError> {
        let mut message = first;
        loop {
            if share
                .receive_part(peer, operation_id, message)
                .map_err(protocol_error)?
            {
                break;
            }
            message = stream
                .recv()
                .await
                .map_err(link_error)?
                .ok_or_else(|| link_error("clipboard share ended without completion"))?;
        }
        if stream.recv().await.map_err(link_error)?.is_some()
            || !stream.effect_validity()()
            || !self
                .incoming
                .lock()
                .expect("incoming share mutex poisoned")
                .contains_key(&(peer.to_owned(), operation_id))
        {
            return Err(link_error("clipboard share was interrupted or has trailing records"));
        }
        stream.finish();
        self.persist_share(peer, operation_id, share)
    }

    fn persist_share(
        &self,
        peer: &str,
        operation_id: [u8; 16],
        share: IncomingShare,
    ) -> Result<(), MobileV2TransportError> {
        let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
        let chunked = share.text.is_some();
        let mut announced = Vec::new();
        if let Some(bytes) = share.text {
            let text = String::from_utf8(bytes).map_err(link_error)?;
            let content_id = format!("text-{peer}-{}", hex(&operation_id));
            let announcement = IncomingAnnouncement {
                sender_id: peer.to_owned(),
                proxy_id: None,
                content_id,
                kind: AttachmentKind::Text,
                display_name: "Text".into(),
                mime: Some("text/plain".into()),
                size: text.len() as u64,
                text: Some(text),
                rich_text_format: None,
                source: SourceState {
                    source_id: peer.to_owned(),
                    availability: crate::SourceAvailability::Ready,
                },
                operation_id,
                message_id: share.message_id,
                item_id: 0,
                representation_id: 0,
                // Preserve the source classification carried by ClipboardShareBegin.
                sensitive: share.sensitive,
            };
            let record = store.insert_announcement(now_ms(), announcement).map_err(link_error)?;
            announced.push(record.clone());
            if let Some((ticket, text)) = share.fresh_ticket.zip(record.text.as_deref()) {
                self.fresh_clipboard.complete(ticket, peer, text, None, None);
            }
        }
        // A single text sent with its rich text arrives as representations.
        let mut fresh_ticket = share.fresh_ticket.filter(|_| !chunked && share.count == 1);
        let mut text_record: Option<MessageRecord> = None;
        for mut item in share.items {
            if item.announcement.kind == AttachmentKind::RichText {
                // The rich text of the text item before it (2026-11-12): kept
                // as that message's child, HTML before RTF (the first wins).
                let Some(parent) = text_record.as_ref() else {
                    return Err(link_error("rich text without its text"));
                };
                if store.rich_child(parent.id).map_err(link_error)?.is_none() {
                    let body = item.announcement.text.take().unwrap_or_default();
                    store
                        .insert_rich_body(now_ms(), parent.id, item.announcement, body.as_bytes())
                        .map_err(link_error)?;
                }
                continue;
            }
            let record = store
                .insert_announcement(now_ms(), item.announcement)
                .map_err(link_error)?;
            if record.download != crate::DownloadState::Complete {
                store.begin_download(record.id).map_err(link_error)?;
                store.append_download(record.id, 0, &item.bytes).map_err(link_error)?;
                store.complete_download(record.id).map_err(link_error)?;
            }
            if record.kind == AttachmentKind::Text {
                if let Some(previous) = text_record.take() {
                    announced.push(previous);
                }
                text_record = Some(record);
            } else {
                announced.push(record);
            }
        }
        announced.extend(text_record);
        announced.sort_by_key(|record| record.id);
        for record in &announced {
            // The first text with its rich text goes to the clipboard.
            if let Some(ticket) = fresh_ticket.take_if(|_| record.kind == AttachmentKind::Text)
                && let Some(text) = record.text.as_deref()
            {
                self.fresh_clipboard
                    .complete(ticket, peer, text, rich_for(&store, record.id), None);
            }
        }
        drop(store);
        for record in &announced {
            self.announcement(record, false);
        }
        Ok(())
    }

    fn cancel_incoming(&self, peer: &str, operation_id: [u8; 16]) {
        self.incoming
            .lock()
            .expect("incoming share mutex poisoned")
            .remove(&(peer.to_owned(), operation_id));
    }

    pub(crate) fn peer_ready(&self, peer: &str) {
        if let Err(error) = self
            .coordinator
            .store
            .lock()
            .expect("transfer store mutex poisoned")
            .set_peer_online(peer, true)
        {
            self.error(None, None, error.to_string());
        }
    }

    /// The desktop paused clipboard sharing (it closed or refused B0 with
    /// `Paused`): what was asked of it fails now with that reason instead of
    /// waiting for an answer it cannot send. A share it has not accepted
    /// ends as Paused; current-clipboard reads end with `PeerPaused`.
    pub(crate) fn peer_paused(&self, peer: &str) {
        self.transport.abort_clock_peer(
            peer,
            (DeliveryFailure::Paused, "clock source paused clipboard sharing".into()),
        );
        let waiting: Vec<[u8; 16]> = self
            .transport
            .outgoing
            .lock()
            .expect("outgoing share mutex poisoned")
            .iter()
            .filter(|((target, _), share)| target == peer && !share.started)
            .map(|((_, operation_id), _)| *operation_id)
            .collect();
        for operation_id in waiting {
            self.transport.finish_share(
                peer,
                operation_id,
                Err((
                    DeliveryFailure::Paused,
                    format!("clipboard sharing is paused on {peer}"),
                )),
            );
        }
        self.transport
            .current_queries
            .lock()
            .expect("current query mutex poisoned")
            .retain(|(target, _), query| {
                if target != peer {
                    return true;
                }
                if let Some(reply) = query.reply.take() {
                    let _ = reply.send(Err(MobileV2TransportError::PeerPaused(peer.to_owned())));
                }
                false
            });
    }

    pub(crate) fn peer_lost(&self, peer: &str) {
        self.transport
            .abort_clock_peer(peer, (DeliveryFailure::Interrupted, "clock source disconnected".into()));
        self.attachments
            .lock()
            .expect("clipboard attachment mutex poisoned")
            .remove(peer);
        if let Err(error) = self
            .coordinator
            .store
            .lock()
            .expect("transfer store mutex poisoned")
            .set_peer_online(peer, false)
        {
            self.error(None, None, error.to_string());
        }
        let outgoing = {
            let mut outgoing = self.transport.outgoing.lock().expect("outgoing share mutex poisoned");
            let mut lost = Vec::new();
            outgoing.retain(|(target, operation_id), share| {
                if target != peer {
                    return true;
                }
                lost.push((*operation_id, share.direct));
                false
            });
            lost
        };
        for (operation_id, direct) in outgoing {
            let failure = if direct {
                (
                    DeliveryFailure::PeerDisconnected,
                    "the receiving phone disconnected".into(),
                )
            } else {
                (
                    DeliveryFailure::Interrupted,
                    "the link to the target was lost during the share".into(),
                )
            };
            self.transport.settle(operation_id, Err(Some(failure)));
        }
        self.incoming
            .lock()
            .expect("incoming share mutex poisoned")
            .retain(|(source, _), _| source != peer);
        self.direct_peer_lost(peer);
        let mut current = self
            .transport
            .current_queries
            .lock()
            .expect("current query mutex poisoned");
        current.retain(|(target, operation_id), query| {
            if target != peer {
                return true;
            }
            let detail = "clipboard current peer disconnected";
            if let Some(reply) = query.reply.take() {
                let _ = reply.send(Err(link_error(detail)));
            }
            self.error(Some(*operation_id), None, detail.into());
            false
        });
    }

    pub(crate) fn download_attempt(&self, request: &v2::Message) -> Result<Option<(i64, u64)>, MobileV2TransportError> {
        if !matches!(
            request,
            v2::Message::ClipboardContentRead(_) | v2::Message::ContentReadFile(_)
        ) {
            return Ok(None);
        }
        let record = self
            .download_record(request)?
            .ok_or_else(|| link_error("download no longer exists"))?;
        let store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
        let attempt = store
            .attempt(record.id)
            .ok_or_else(|| link_error("download is no longer active"))?;
        Ok(Some((record.id, attempt)))
    }

    pub(crate) fn finish_low(
        &self,
        peer: &str,
        operation_id: [u8; 16],
        is_share: bool,
        download: Option<(i64, u64)>,
        error: Option<MobileV2TransportError>,
    ) {
        let listing = self
            .directory_ops
            .lock()
            .expect("directory operation mutex poisoned")
            .remove(&operation_id);
        if error.is_some() {
            // A thumbnail answered with a final state was already removed;
            // only an interrupted request is still registered here.
            self.finish_thumbnail(operation_id, crate::ThumbnailState::Failed);
        }
        let Some(error) = error else { return };
        if let Some((folder, directory)) = listing {
            // The folder view waiting for this page ends in the failure.
            self.directory_failed(operation_id, folder, directory, error);
            return;
        }
        let detail = error.to_string();
        let mut record_id = None;
        if let Some((id, attempt)) = download {
            let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            if store.attempt(id) != Some(attempt) {
                return;
            }
            // A failed download releases its attempt, so the user can retry
            // it right away. Content the source no longer has keeps that
            // reason on the record instead of looking retryable.
            let _ = match error {
                MobileV2TransportError::SourceUnavailable(reason) => store.mark_unavailable(id, reason),
                _ => store.fail_download(id),
            };
            let failed = store.by_id(id).ok().flatten();
            drop(store);
            if matches!(
                error,
                MobileV2TransportError::SourceUnavailable(
                    crate::SourceAvailability::NoSuchItem | crate::SourceAvailability::Superseded
                )
            ) && self.reject_history_holder(id, peer).unwrap_or(false)
                && self.coordinator.start_download(id).is_ok()
            {
                return;
            }
            self.fresh_clipboard.failed(id);
            self.coordinator.finish_auto_open(id);
            self.download_changed.notify_waiters();
            if let Some(record) = failed {
                match record.parent_id.filter(|_| record.kind == AttachmentKind::RichText) {
                    // A text message whose rich text did not come stays
                    // plain text; its clipboard write goes ahead with it.
                    Some(parent) => {
                        self.rich_ready(parent);
                        return;
                    }
                    None => self.announcement(&record, false),
                }
            }
            record_id = Some(id);
        }
        if is_share {
            if error == MobileV2TransportError::Cancelled {
                // The cancel already settled this target.
                return;
            }
            if self.transport.upload_failed(peer, operation_id, detail.clone()) {
                // The receiving phone's result (or the link loss) reports it.
                return;
            }
        }
        self.error(Some(operation_id), record_id, detail);
    }

    fn download_record(&self, request: &v2::Message) -> Result<Option<crate::MessageRecord>, MobileV2TransportError> {
        let store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
        match request {
            v2::Message::ClipboardContentRead(request) => {
                let offer = request.offer().map_err(protocol_error)?;
                let item = request.item_id().map_err(protocol_error)?;
                let rep = request.representation_id().map_err(protocol_error)?;
                let key = content_id_for_representation(&offer, item, rep).map_err(protocol_error)?;
                let old_key = content_id_from_offer(&offer).map_err(protocol_error)?;
                store.by_wire_content(&key, &old_key, item, rep).map_err(link_error)
            }
            v2::Message::ContentReadFile(request) => {
                let offer = request.offer().map_err(protocol_error)?;
                let key = file_content_id(
                    &offer,
                    request.node().map_err(protocol_error)?,
                    request.version().map_err(protocol_error)?,
                )
                .map_err(protocol_error)?;
                store.by_content_id(&key).map_err(link_error)
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn receive_low_reply(
        &self,
        operation_id: [u8; 16],
        request: &v2::Message,
        download: Option<(i64, u64)>,
        message: v2::Message,
    ) -> Result<bool, MobileV2TransportError> {
        let result = match request {
            v2::Message::ContentThumbnailRead(read) => return self.receive_thumbnail(operation_id, read, message),
            v2::Message::ClipboardContentRead(read) => representation_reply(read, message).map_err(protocol_error)?,
            v2::Message::ContentReadFile(read) => file_reply(read, message).map_err(protocol_error)?,
            v2::Message::ContentListDirectory(list) => {
                return match message {
                    v2::Message::ContentDirectory(directory)
                        if directory.offer().map_err(protocol_error)? == list.offer().map_err(protocol_error)?
                            && directory
                                .page()
                                .and_then(|page| page.directory())
                                .map_err(protocol_error)?
                                == list.node().map_err(protocol_error)? =>
                    {
                        self.receive_directory(operation_id, &directory)
                            .map_err(protocol_error)?;
                        Ok(true)
                    }
                    v2::Message::ContentPreparing(preparing)
                        if preparing.offer().map_err(protocol_error)? == list.offer().map_err(protocol_error)?
                            && preparing.node().map_err(protocol_error)? == list.node().map_err(protocol_error)? =>
                    {
                        Ok(false)
                    }
                    v2::Message::ContentDirectoryFailed(failed) => Err(content_unavailable(
                        failed.reason().map_err(protocol_error)?,
                        &self.listing_source(operation_id),
                    )),
                    _ => Err(protocol_error(v2::ErrorCode::StateConflict.into())),
                };
            }
            _ => return Err(protocol_error(v2::ErrorCode::UnsupportedValue.into())),
        };
        let (record_id, attempt) = download.ok_or_else(|| link_error("download attempt is missing"))?;
        self.apply_download_reply(operation_id, record_id, attempt, result)
    }

    fn apply_download_reply(
        &self,
        operation_id: [u8; 16],
        record_id: i64,
        attempt: u64,
        reply: DownloadReply,
    ) -> Result<bool, MobileV2TransportError> {
        let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
        if store.attempt(record_id) != Some(attempt) {
            return Err(link_error("download has been replaced or cancelled"));
        }
        let record = store
            .by_id(record_id)
            .map_err(link_error)?
            .filter(|record| record.download == crate::DownloadState::Downloading)
            .ok_or_else(|| link_error("download no longer exists"))?;
        match reply {
            DownloadReply::Chunk { offset, bytes } => {
                let received = offset
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| link_error("download offset overflow"))?;
                // A file read is bounded by its listed size; a representation by
                // what its source offered (no phone-side cap, user 2026-11-10).
                let is_file_read = parse_file_content(&record.content_id).is_some();
                if bytes.is_empty()
                    || (is_file_read && record.size != 0 && received > record.size)
                    || (record.kind == AttachmentKind::Text && received > MAX_TEXT_DOWNLOAD)
                    || (record.kind == AttachmentKind::RichText
                        && record.parent_id.is_some()
                        && received > MAX_RICH_DOWNLOAD)
                {
                    return Err(link_error("download exceeds expected size or has an empty chunk"));
                }
                store.append_download(record.id, offset, &bytes).map_err(link_error)?;
                self.transport.emit(TransferEvent::DownloadProgress {
                    operation_id,
                    record_id: record.id,
                    received,
                    total: (record.size != 0).then_some(record.size),
                });
                Ok(false)
            }
            DownloadReply::End(size) => {
                let mut fetch = None;
                let path = record
                    .local_path
                    .as_deref()
                    .ok_or_else(|| link_error("download has no local path"))?;
                if fs::metadata(path).map_err(link_error)?.len() != size
                    || (parse_file_content(&record.content_id).is_some() && record.size != 0 && size != record.size)
                {
                    return Err(link_error("download completed with the wrong size"));
                }
                if record.kind == AttachmentKind::Text {
                    let text = fs::read_to_string(path).map_err(link_error)?;
                    let text = if record.mime.as_deref() == Some(dto::URL_FORMAT) {
                        serde_json::from_str::<serde_json::Value>(&text)
                            .ok()
                            .and_then(|value| value.get("url").and_then(serde_json::Value::as_str).map(str::to_owned))
                            .filter(|url| !url.is_empty())
                            .ok_or_else(|| link_error("invalid shared URL"))?
                    } else {
                        text
                    };
                    store.complete_text(record.id, &text).map_err(link_error)?;
                    // Its rich text comes next; a waiting clipboard write
                    // waits for it (`rich_ready`).
                    fetch = rich_to_fetch(&store, record.id);
                    if fetch.is_none() && !rich_downloading(&store, record.id) {
                        self.fresh_clipboard.downloaded(
                            record.id,
                            &record.sender_id,
                            &text,
                            rich_for(&store, record.id),
                            clip_marker(&record.content_id),
                        );
                    }
                } else {
                    store.complete_download(record.id).map_err(link_error)?;
                }
                drop(store);
                let auto_open_ready = self.coordinator.complete_auto_open(record.id);
                self.transport.emit(TransferEvent::DownloadProgress {
                    operation_id,
                    record_id: record.id,
                    received: size,
                    total: Some(size),
                });
                if let Some(child) = fetch {
                    self.fetch_rich(record.id, child);
                }
                // A text message's rich text is part of that message.
                match record.parent_id.filter(|_| record.kind == AttachmentKind::RichText) {
                    Some(parent) => self.rich_ready(parent),
                    None => self.announcement(&record, auto_open_ready),
                }
                self.download_changed.notify_waiters();
                Ok(true)
            }
            DownloadReply::Failed(reason) => {
                // The source no longer has the original (retention over,
                // clipboard moved on, cache cleared): the image is expired
                // for good, not a failure to retry.
                if record.kind == AttachmentKind::Image
                    && store.history_source(record.id).map_err(link_error)?.is_none()
                    && matches!(
                        reason,
                        dto::ContentUnavailable::NoSuchItem | dto::ContentUnavailable::Superseded
                    )
                {
                    store.expire(record.id).map_err(link_error)?;
                }
                Err(content_unavailable(
                    reason,
                    record.proxy_id.as_deref().unwrap_or(&record.sender_id),
                ))
            }
        }
    }

    fn peers(&self) -> Vec<Peer> {
        self.peers.lock().expect("peer mutex poisoned").clone()
    }

    /// The device serving remote listing `operation_id` (its folder's proxy
    /// or sender).
    fn listing_source(&self, operation_id: [u8; 16]) -> String {
        let folder = self
            .directory_ops
            .lock()
            .expect("directory operation mutex poisoned")
            .get(&operation_id)
            .map(|(folder, _)| *folder);
        folder
            .and_then(|id| {
                let store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
                store.by_id(id).ok().flatten()
            })
            .map(|record| record.proxy_id.unwrap_or(record.sender_id))
            .unwrap_or_default()
    }
}

/// Why a source refused a body or listing, kept apart for the host (contract
/// section 9). `source` is the device that refused.
fn content_unavailable(reason: dto::ContentUnavailable, source: &str) -> MobileV2TransportError {
    match reason {
        dto::ContentUnavailable::Superseded => {
            MobileV2TransportError::SourceUnavailable(crate::SourceAvailability::Superseded)
        }
        dto::ContentUnavailable::NoSuchItem => {
            MobileV2TransportError::SourceUnavailable(crate::SourceAvailability::NoSuchItem)
        }
        // The PC's clipboard sharing is paused, or it stopped sending:
        // retryable once that changes, not a lost link.
        dto::ContentUnavailable::Paused => MobileV2TransportError::PeerPaused(source.into()),
        dto::ContentUnavailable::SendDisabled => MobileV2TransportError::SendDisabled,
        reason => link_error(format!("{reason:?}")),
    }
}

impl<L: MobileV2Link> MobileV2Session<L> {
    /// Lists one page of a folder message. `content_id` is the folder
    /// message's content id, `node` 0 means its root; follow-up pages pass
    /// the `next_version`/`next_offset` of the previous `DirectoryPage`. The
    /// page reports the `node` asked for, so the host finds its root page
    /// under 0 whatever id the folder gives its root.
    ///
    /// A listing of a received folder that cannot be made (local copy gone,
    /// source offline, refused by the source, link lost) ends in
    /// `DirectoryFailed` for the same folder and node, now or when the source
    /// answers, so the host's folder view stops loading. An error of this
    /// call only means `content_id` names no received folder.
    pub fn list_directory(
        &self,
        operation_id: [u8; 16],
        content_id: &str,
        node: u64,
        cursor: Option<(u64, u32)>,
    ) -> Result<(), MobileV2TransportError> {
        let folder = {
            let store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            let folder = store
                .by_content_id(content_id)
                .map_err(link_error)?
                .filter(|record| record.kind == AttachmentKind::Folder && record.parent_id.is_none())
                .ok_or_else(|| link_error("folder message is unavailable"))?;
            if folder.outgoing {
                return Err(link_error("an own folder has no remote listing"));
            }
            folder
        };
        let result = if folder.direct_peer.is_some() {
            self.list_local_directory(operation_id, &folder, node)
        } else {
            self.list_remote_directory(operation_id, &folder, node, cursor)
        };
        if let Err(error) = result {
            self.directory_failed(operation_id, folder.id, node, error);
        }
        Ok(())
    }

    /// The listing of `directory` (as the host asked for it) of folder
    /// message `folder_message_id` ended without a page.
    fn directory_failed(
        &self,
        operation_id: [u8; 16],
        folder_message_id: i64,
        directory: u64,
        error: MobileV2TransportError,
    ) {
        use crate::SourceAvailability::{Evicted, NoSuchItem, Superseded, Unsupported};
        // Content its source no longer has (or never had) stays unavailable
        // however often the user retries.
        let retryable = !matches!(
            error,
            MobileV2TransportError::SourceUnavailable(Evicted | NoSuchItem | Superseded | Unsupported)
                | MobileV2TransportError::NotFound(_)
                | MobileV2TransportError::OwnMessage(_)
        );
        self.transport.emit(TransferEvent::DirectoryFailed {
            operation_id,
            folder_message_id,
            directory,
            error,
            retryable,
        });
    }

    fn list_remote_directory(
        &self,
        operation_id: [u8; 16],
        folder: &crate::MessageRecord,
        node: u64,
        cursor: Option<(u64, u32)>,
    ) -> Result<(), MobileV2TransportError> {
        let content_id = folder.content_id.as_str();
        let source = self
            .with_store(|store| store.history_source(folder.id))
            .map_err(link_error)?
            .unwrap_or_else(|| content_id.split(':').nth(1).unwrap_or_default().into());
        let peer = self
            .peers()
            .into_iter()
            .find(|peer| peer.state == crate::PeerState::Ready && (source.is_empty() || peer.id == source))
            .ok_or_else(|| MobileV2TransportError::PeerUnavailable(source.clone()))?;
        let offer = parse_content_id(content_id).map_err(protocol_error)?;
        let requested = node;
        let node = if node == 0 {
            parse_file_content(content_id).map_or(node, |(root, _)| root)
        } else {
            node
        };
        let mut raw = RawMap::from([(1, offer.write()), (2, node.write()), (4, clipboard_demand().write())]);
        if let Some((version, offset)) = cursor {
            raw.insert(
                3,
                dto::DirectoryCursor::from_raw([(1, version.write()), (2, offset.write())].into())
                    .map_err(protocol_error)?
                    .write(),
            );
        }
        let message = dto::ContentListDirectory::from_raw(raw).map_err(protocol_error)?;
        let mut listings = self.directory_ops.lock().expect("directory operation mutex poisoned");
        if listings.insert(operation_id, (folder.id, requested)).is_some() {
            return Err(link_error("directory listing operation id is already in use"));
        }
        drop(listings);
        let result = self.transport.link.send_low(
            &peer.id,
            ClipboardLowOperation::request(operation_id, v2::Message::ContentListDirectory(message)),
        );
        if result.is_err() {
            self.directory_ops
                .lock()
                .expect("directory operation mutex poisoned")
                .remove(&operation_id);
        }
        result
    }

    fn receive_directory(&self, operation_id: [u8; 16], directory: &dto::ContentDirectory) -> Result<(), v2::Error> {
        let page = directory.page()?;
        let mut entries = page
            .entries()?
            .0
            .into_iter()
            .map(directory_entry)
            .collect::<Result<Vec<_>, _>>()?;
        let next = page.next()?;
        let next_version = next.as_ref().map(|cursor| cursor.version()).transpose()?;
        let next_offset = next.map(|cursor| cursor.offset()).transpose()?;
        let (folder_message_id, requested) = *self
            .directory_ops
            .lock()
            .expect("directory operation mutex poisoned")
            .get(&operation_id)
            .ok_or(v2::ErrorCode::StateConflict)?;
        self.attach_children(folder_message_id, &mut entries)
            .map_err(|_| v2::ErrorCode::StateConflict)?;
        self.transport.emit(TransferEvent::DirectoryPage {
            operation_id,
            folder_message_id,
            // The host asked for 0 (the root); the source answered with the
            // root's own node id (checked against the request).
            directory: requested,
            entries,
            next_version,
            next_offset,
        });
        Ok(())
    }

    /// Puts on each file entry of a listing page its record under folder
    /// message `folder_id`, when it has one (download state, local file,
    /// thumbnail), so the folder page shows them without a lookup per row.
    fn attach_children(&self, folder_id: i64, entries: &mut [DirectoryEntry]) -> Result<(), crate::StoreError> {
        let store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
        let Some(folder) = store.by_id(folder_id)? else {
            return Ok(());
        };
        let offer = parse_content_id(&folder.content_id).map_err(|_| crate::StoreError::InvalidContentId)?;
        for entry in entries.iter_mut().filter(|entry| entry.kind == "file") {
            let content_id =
                file_content_id(&offer, entry.id, entry.version).map_err(|_| crate::StoreError::InvalidContentId)?;
            entry.child = store
                .by_content_id(&content_id)?
                .filter(|child| child.parent_id == Some(folder_id));
        }
        Ok(())
    }
}

enum DownloadReply {
    Chunk { offset: u64, bytes: Vec<u8> },
    End(u64),
    Failed(dto::ContentUnavailable),
}

fn representation_reply(read: &dto::ClipboardContentRead, message: v2::Message) -> Result<DownloadReply, v2::Error> {
    let offer = read.offer()?;
    let representation = read.representation_id()?;
    match message {
        v2::Message::ContentRepresentationBytes(chunk)
            if chunk.offer()? == offer && chunk.representation()? == representation =>
        {
            Ok(DownloadReply::Chunk {
                offset: chunk.offset()?,
                bytes: chunk.bytes()?.0,
            })
        }
        v2::Message::ContentRepresentationEnd(end)
            if end.offer()? == offer && end.representation()? == representation =>
        {
            Ok(DownloadReply::End(end.size()?))
        }
        v2::Message::ContentRepresentationFailed(failed)
            if failed.offer()? == offer && failed.representation()? == representation =>
        {
            Ok(DownloadReply::Failed(failed.reason()?))
        }
        _ => Err(v2::ErrorCode::StateConflict.into()),
    }
}

fn file_reply(read: &dto::ContentReadFile, message: v2::Message) -> Result<DownloadReply, v2::Error> {
    let offer = read.offer()?;
    let node = read.node()?;
    let version = read.version()?;
    match message {
        v2::Message::ContentFileBlock(chunk)
            if chunk.offer()? == offer && chunk.node()? == node && chunk.version()? == version =>
        {
            Ok(DownloadReply::Chunk {
                offset: chunk.offset()?,
                bytes: chunk.bytes()?.0,
            })
        }
        v2::Message::ContentFileEnd(end)
            if end.offer()? == offer && end.node()? == node && end.version()? == version =>
        {
            Ok(DownloadReply::End(end.size()?))
        }
        v2::Message::ContentFileFailed(failed) if failed.offer()? == offer && failed.node()? == node => {
            Ok(DownloadReply::Failed(failed.reason()?))
        }
        _ => Err(v2::ErrorCode::StateConflict.into()),
    }
}

fn clipboard_demand() -> dto::Demand {
    dto::Demand::from_raw([(1, dto::DemandKind::Clipboard.write())].into()).expect("fixed clipboard demand")
}

pub(crate) fn current_query(
    identity: &MobileV2Identity,
    request_id: [u8; 16],
    max_items: u16,
) -> Result<v2::Message, v2::Error> {
    Ok(v2::Message::ClipboardCurrentQuery(
        dto::ClipboardCurrentQuery::from_raw(
            [
                (1, id16(request_id).write()),
                (2, id16(identity.group.0).write()),
                (3, boot(identity.boot).write()),
                (4, max_items.write()),
            ]
            .into(),
        )?,
    ))
}

/// A listing entry a file can be stored from: a file with a plain name.
fn check_directory_file(entry: &DirectoryEntry) -> Result<(), MobileV2TransportError> {
    let name = entry.name.as_str();
    if entry.kind != "file" || name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\', '\0']) {
        return Err(link_error("invalid directory entry name"));
    }
    Ok(())
}

/// Received folder message `id` and the offer its files belong to.
fn received_folder(store: &TransferStore, id: i64) -> Result<(MessageRecord, dto::OfferId), MobileV2TransportError> {
    let folder = store
        .by_id(id)
        .map_err(link_error)?
        .filter(|record| record.kind == AttachmentKind::Folder && record.parent_id.is_none())
        .ok_or_else(|| link_error("folder message is unavailable"))?;
    if folder.outgoing {
        return Err(link_error("an own folder has no remote listing"));
    }
    let offer = parse_content_id(&folder.content_id).map_err(protocol_error)?;
    Ok((folder, offer))
}

/// Records file `entry` of PC folder message `parent` as its child (or
/// refreshes the child it already has): the parent's offer, sender and proxy
/// identity, never a conversation message of its own.
fn insert_directory_child(
    store: &mut TransferStore,
    parent: &MessageRecord,
    offer: &dto::OfferId,
    operation_id: [u8; 16],
    entry: DirectoryEntry,
) -> Result<MessageRecord, MobileV2TransportError> {
    let mime = image_mime_for_name(&entry.name);
    store
        .insert_directory_file(
            now_ms(),
            parent.id,
            IncomingAnnouncement {
                sender_id: parent.sender_id.clone(),
                proxy_id: parent.proxy_id.clone(),
                content_id: file_content_id(offer, entry.id, entry.version).map_err(protocol_error)?,
                kind: if mime.is_some() {
                    AttachmentKind::Image
                } else {
                    AttachmentKind::File
                },
                display_name: entry.name,
                mime: mime.map(str::to_owned),
                size: entry.size.unwrap_or(0),
                text: None,
                rich_text_format: None,
                source: SourceState {
                    source_id: parent.proxy_id.clone().unwrap_or_else(|| parent.sender_id.clone()),
                    availability: parent.source_availability,
                },
                operation_id,
                message_id: parent.message_id,
                item_id: 0,
                representation_id: 0,
                sensitive: parent.sensitive,
            },
        )
        .map_err(link_error)
}

fn directory_entry(node: dto::FileNode) -> Result<DirectoryEntry, v2::Error> {
    let kind = match node.kind()? {
        dto::NodeKind::File => "file",
        dto::NodeKind::Directory => "directory",
        dto::NodeKind::Symlink => "symlink",
        dto::NodeKind::Unknown(_) => "unknown",
    };
    let size = match node.size()?.variant()? {
        v2::schema::Open::Known(dto::ContentSizeKind::Known) => {
            Some(dto::KnownSize::from_raw(node.size()?.body()?)?.bytes()?)
        }
        _ => None,
    };
    Ok(DirectoryEntry {
        id: node.id()?,
        parent: node.parent()?,
        name: node.name()?.0,
        kind: kind.into(),
        size,
        version: node.version()?,
        child: None,
    })
}

fn content_offer(
    identity: &MobileV2Identity,
    payload: &SharePayload,
    item: &crate::SendItem,
    item_id: u32,
    representation_id: u32,
    archive: Option<&Arc<tempfile::NamedTempFile>>,
) -> Result<dto::ContentOffer, v2::Error> {
    let offer = offer_id(identity.device, payload.operation_id, item_id);
    let (kind, mime, display_name, bytes) = match item {
        crate::SendItem::Image {
            mime,
            path,
            encoded_size,
            ..
        } => (
            dto::ContentKind::Image,
            mime.clone(),
            file_name(path),
            Some(*encoded_size),
        ),
        crate::SendItem::File { mime, name, size, .. } => (
            dto::ContentKind::File,
            mime.clone().unwrap_or_else(|| "application/octet-stream".into()),
            name.clone(),
            Some(*size),
        ),
        crate::SendItem::Folder { path, name } => (
            dto::ContentKind::Folder,
            "application/zip".into(),
            if name.is_empty() { file_name(path) } else { name.clone() },
            Some(
                archive
                    .ok_or(v2::ErrorCode::StateConflict)?
                    .as_file()
                    .metadata()
                    .map_err(|_| v2::ErrorCode::StateConflict)?
                    .len(),
            ),
        ),
        crate::SendItem::Text { .. } | crate::SendItem::RichText { .. } => {
            let (format, body) = *item
                .representations()
                .get(representation_id as usize)
                .ok_or(v2::ErrorCode::StateConflict)?;
            let (kind, mime, name) = match format {
                crate::TextFormat::Plain => (dto::ContentKind::Text, "text/plain", "Text"),
                crate::TextFormat::Html => (dto::ContentKind::Html, "text/html", "rich text"),
                crate::TextFormat::Rtf => (dto::ContentKind::Rtf, "application/rtf", "rich text"),
            };
            (kind, mime.into(), name.into(), Some(body.len() as u64))
        }
    };
    let mut raw = RawMap::from([
        (1, offer.write()),
        (2, id16(payload.operation_id).write()),
        (3, id16(payload.message_id).write()),
        (4, item_id.write()),
        (5, representation_id.write()),
        (6, boot(identity.boot).write()),
        (7, device(identity.device).write()),
        (9, kind.write()),
        (10, RawValue::Text(mime)),
        (12, bytes.map_or(RawValue::Null, |value| value.write())),
        (13, dto::ContentAvailability::Ready.write()),
        (14, now_ms().max(0).write()),
    ]);
    raw.insert(11, RawValue::Text(display_name));
    // The original's displayed size: a receiving phone shows the image at
    // its ratio from the first byte on (a PC has its own decoder).
    if let crate::SendItem::Image {
        dimensions: Some((width, height)),
        ..
    } = item
    {
        raw.insert(15, width.write());
        raw.insert(16, height.write());
    }
    dto::ContentOffer::from_raw(raw)
}

fn offer_id(origin: DeviceId, operation: [u8; 16], item_id: u32) -> dto::OfferId {
    let seq = u64::from_le_bytes(operation[..8].try_into().expect("eight byte operation prefix"));
    dto::OfferId::from_raw(
        [
            (
                1,
                dto::ClipId::from_raw([(1, device(origin).write()), (2, seq.write())].into())
                    .expect("fixed clip")
                    .write(),
            ),
            (2, (u64::from(item_id) + 1).write()),
        ]
        .into(),
    )
    .expect("fixed offer")
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("shared item")
        .to_owned()
}

fn share_begin(
    identity: &MobileV2Identity,
    share: &PreparedShare,
    delivery: dto::ShareDelivery,
) -> Result<v2::Message, v2::Error> {
    let payload = &share.payload;
    let item = payload.items.first().ok_or(v2::ErrorCode::UnsupportedValue)?;
    let kind = match item.kind() {
        AttachmentKind::Text => dto::ShareKind::Text,
        AttachmentKind::Image => dto::ShareKind::Image,
        AttachmentKind::File => dto::ShareKind::File,
        AttachmentKind::Folder => dto::ShareKind::Folder,
        AttachmentKind::RichText => dto::ShareKind::Html,
    };
    let mut raw = RawMap::from([
        (1, id16(payload.operation_id).write()),
        (2, id16(payload.message_id).write()),
        (3, device(identity.device).write()),
        (4, delivery.write()),
        (5, kind.write()),
        (6, (payload.items.len() as u16).write()),
        (9, boot(identity.boot).write()),
    ]);
    if payload.sensitivity != oink_foundation::clipboard::Sensitivity::Normal {
        let sensitivity = match payload.sensitivity {
            oink_foundation::clipboard::Sensitivity::Normal => dto::Sensitivity::Normal,
            oink_foundation::clipboard::Sensitivity::Concealed => dto::Sensitivity::Concealed,
            oink_foundation::clipboard::Sensitivity::Transient => dto::Sensitivity::Transient,
            oink_foundation::clipboard::Sensitivity::AutoGenerated => dto::Sensitivity::AutoGenerated,
        };
        raw.insert(11, sensitivity.write());
    }
    raw.insert(7, share.total.write());
    Ok(v2::Message::ClipboardShareBegin(dto::ClipboardShareBegin::from_raw(
        raw,
    )?))
}

/// The records an offer is shown as: per item the announcement and, for
/// plain text, the rich text kept next to it.
#[allow(clippy::type_complexity)]
fn offer_announcements(
    offer: &dto::ClipboardOffer,
) -> Result<Vec<(IncomingAnnouncement, Option<IncomingAnnouncement>)>, v2::Error> {
    let inventory = offer.inventory()?;
    let offer_id = offer.id()?;
    let sender_id = hex(&offer.real_sender()?.public_key()?.0);
    let source_id = hex(&offer_id.clip()?.origin()?.public_key()?.0);
    let proxy_id = offer
        .proxy()?
        .map(|id| id.public_key().map(|key| hex(&key.0)))
        .transpose()?;
    let operation_id = offer.operation_id()?.0;
    let message_id = offer.message_id()?.0;
    let sensitive = matches!(
        offer.sensitivity()?,
        dto::Sensitivity::Concealed | dto::Sensitivity::Transient
    );
    let source = SourceState {
        source_id,
        availability: crate::SourceAvailability::Ready,
    };
    let roots = inventory.roots()?.0;
    if !roots.is_empty() {
        return roots
            .into_iter()
            .filter_map(|root| {
                let result = (|| {
                    let name = root.name()?.0;
                    // An image file (a photo a phone sent through a proxy, a
                    // copied .jpg) is presented as an image, not a file card.
                    let image = image_mime_for_name(&name);
                    let kind = match root.kind()? {
                        dto::NodeKind::Directory => AttachmentKind::Folder,
                        dto::NodeKind::File if image.is_some() => AttachmentKind::Image,
                        dto::NodeKind::File => AttachmentKind::File,
                        _ => return Ok(None),
                    };
                    Ok(Some(IncomingAnnouncement {
                        sender_id: sender_id.clone(),
                        proxy_id: proxy_id.clone(),
                        content_id: file_content_id(&offer_id, root.id()?, root.version()?)?,
                        kind,
                        mime: (kind == AttachmentKind::Image)
                            .then(|| image.map(str::to_owned))
                            .flatten(),
                        display_name: name,
                        size: known_size(&root.size()?)?.unwrap_or(0),
                        text: None,
                        rich_text_format: None,
                        source: source.clone(),
                        operation_id,
                        message_id,
                        item_id: 0,
                        representation_id: 0,
                        sensitive,
                    }))
                })();
                // A file root has no rich text.
                result
                    .map(|found| found.map(|announcement| (announcement, None)))
                    .transpose()
            })
            .collect();
    }

    let mut announcements = Vec::new();
    for item in inventory.items()?.0 {
        // Every portable representation, best kind first (the first of
        // equally ranked ones): the item shows the best one; plain text also
        // keeps the item's rich text, HTML before RTF (2026-11-12, matching
        // the PC's `phone_representations`).
        let mut candidates: Vec<(u8, IncomingAnnouncement)> = Vec::new();
        for representation in item.representations()?.0 {
            let Some(format) = representation.portable_format()? else {
                continue;
            };
            let Some((priority, kind, mime, rich_text_format)) = portable_format(&format.0) else {
                continue;
            };
            if candidates.iter().any(|(current, _)| *current == priority) {
                continue;
            }
            let data = representation.data()?;
            use dto::RepresentationDataKind::{Inline, Reader};
            use v2::schema::Open::Known;
            let size = match data.variant()? {
                Known(Inline) => dto::RepresentationInline::from_raw(data.body()?)?.bytes()?.0.len() as u64,
                Known(Reader) => known_size(&dto::RepresentationReader::from_raw(data.body()?)?.size()?)?.unwrap_or(0),
                _ => continue,
            };
            let item_id = item.id()?;
            let representation_id = representation.id()?;
            candidates.push((
                priority,
                IncomingAnnouncement {
                    sender_id: sender_id.clone(),
                    proxy_id: proxy_id.clone(),
                    content_id: content_id_for_representation(&offer_id, item_id, representation_id)?,
                    kind,
                    // A bitmap representation has no file name; give it one
                    // with the right extension for saving and opening.
                    display_name: match kind {
                        AttachmentKind::Image => {
                            format!("image.{}", crate::store::extension_for_mime(mime).unwrap_or("png"))
                        }
                        kind => kind.as_str().to_owned(),
                    },
                    mime: Some(mime.to_owned()),
                    size,
                    text: None,
                    rich_text_format,
                    source: source.clone(),
                    operation_id,
                    message_id,
                    item_id,
                    representation_id,
                    sensitive,
                },
            ));
        }
        candidates.sort_by_key(|(priority, _)| *priority);
        let mut candidates = candidates.into_iter();
        let Some((_, mut shown)) = candidates.next() else {
            continue;
        };
        let rich = (shown.kind == AttachmentKind::Text && shown.mime.as_deref() == Some("text/plain"))
            .then(|| candidates.find(|(_, rich)| rich.kind == AttachmentKind::RichText))
            .flatten()
            .map(|(_, rich)| rich);
        if let Some(rich) = &rich {
            shown.rich_text_format = rich.rich_text_format;
        }
        announcements.push((shown, rich));
    }
    Ok(announcements)
}

#[derive(Clone, Copy, Debug)]
struct ThumbnailOp {
    record: i64,
    attempt: u64,
}

/// Whether an image record gets a thumbnail (protocol-v2 §4.4): every
/// received image, a message or a file of a PC folder (its folder page row,
/// spec 2026-11-11), that is not downloaded, without a final thumbnail
/// state, from an available source.
fn thumbnail_wanted(record: &crate::MessageRecord) -> bool {
    record.kind == AttachmentKind::Image
        && !record.outgoing
        && matches!(
            record.download,
            crate::DownloadState::Placeholder | crate::DownloadState::Failed
        )
        && (matches!(
            record.thumbnail,
            crate::ThumbnailState::None | crate::ThumbnailState::Failed
        ) || record.thumbnail_width.is_some_and(|width| width <= 160))
        && record.source_availability == crate::SourceAvailability::Ready
        && record.content_id.starts_with("v2:")
}

fn known_size(size: &dto::ContentSize) -> Result<Option<u64>, v2::Error> {
    if size.variant()? == v2::schema::Open::Known(dto::ContentSizeKind::Known) {
        Ok(Some(dto::KnownSize::from_raw(size.body()?)?.bytes()?))
    } else {
        Ok(None)
    }
}

fn portable_format(format: &str) -> Option<(u8, AttachmentKind, &'static str, Option<crate::RichTextFormat>)> {
    use crate::RichTextFormat::{Html, Rtf};
    use AttachmentKind::{Image, RichText, Text};
    Some(match format {
        "mime:image/png" => (0, Image, "image/png", None),
        "mime:text/plain;charset=utf-8" => (1, Text, "text/plain", None),
        dto::URL_FORMAT => (2, Text, dto::URL_FORMAT, None),
        "mime:text/html;charset=utf-8" => (3, RichText, "text/html", Some(Html)),
        "mime:application/rtf" => (4, RichText, "application/rtf", Some(Rtf)),
        _ => return None,
    })
}

fn content_announcement(offer: &dto::ContentOffer) -> Result<IncomingAnnouncement, v2::Error> {
    let sender = offer.real_sender()?.public_key()?.0;
    let source = offer.source()?.device_id()?.public_key()?.0;
    let kind = match offer.kind()? {
        dto::ContentKind::Text | dto::ContentKind::Url => AttachmentKind::Text,
        dto::ContentKind::Image => AttachmentKind::Image,
        dto::ContentKind::Folder => AttachmentKind::Folder,
        dto::ContentKind::Html | dto::ContentKind::Rtf => AttachmentKind::RichText,
        _ => AttachmentKind::File,
    };
    let content_id = content_id_for_representation(&offer.offer()?, offer.item_id()?, offer.representation_id()?)?;
    let proxy = match offer.proxy()? {
        Some(p) => Some(hex(&p.public_key()?.0)),
        None => None,
    };
    Ok(IncomingAnnouncement {
        sender_id: hex(&sender),
        proxy_id: proxy,
        content_id,
        kind,
        display_name: offer
            .display_name()?
            .map(|v| v.0)
            .unwrap_or_else(|| "shared item".into()),
        mime: Some(offer.mime()?.0),
        size: offer.bytes()?.unwrap_or(0),
        text: None,
        rich_text_format: match offer.kind()? {
            dto::ContentKind::Html => Some(crate::RichTextFormat::Html),
            dto::ContentKind::Rtf => Some(crate::RichTextFormat::Rtf),
            _ => None,
        },
        source: SourceState {
            source_id: hex(&source),
            availability: match offer.availability()? {
                dto::ContentAvailability::Ready => crate::SourceAvailability::Ready,
                dto::ContentAvailability::SourceOffline => crate::SourceAvailability::SourceOffline,
                dto::ContentAvailability::Evicted => crate::SourceAvailability::Evicted,
                dto::ContentAvailability::NoSuchItem => crate::SourceAvailability::NoSuchItem,
                dto::ContentAvailability::Unsupported | dto::ContentAvailability::Unknown(_) => {
                    crate::SourceAvailability::Unsupported
                }
            },
        },
        operation_id: offer.operation_id()?.0,
        message_id: offer.message_id()?.0,
        item_id: offer.item_id()?,
        representation_id: offer.representation_id()?,
        // ContentOffer (a phone's share) has no sensitivity field.
        sensitive: false,
    })
}

fn content_id_from_offer(offer: &dto::OfferId) -> Result<String, v2::Error> {
    let clip = offer.clip()?;
    Ok(format!(
        "v2:{}:{}:{}",
        hex(&clip.origin()?.public_key()?.0),
        clip.seq()?,
        offer.generation()?
    ))
}

/// Oink clip marker (`oink_foundation::clipboard_marker`) for content written
/// into the phone's clipboard from a received offer's record: the offer's own
/// identity, so the device that announced it recognises the clip when a
/// synchronizer (Universal Clipboard) brings it back. None for records that
/// did not come from an offer (direct shares).
/// The record of an offer that a current-clipboard request answers with: the
/// first text or image record, else the first record (a file, folder or rich
/// text, which a one-tap copy reports as unsupported).
fn current_record(records: impl IntoIterator<Item = (AttachmentKind, crate::MessageId)>) -> Option<crate::MessageId> {
    let mut first = None;
    for (kind, id) in records {
        if matches!(kind, AttachmentKind::Text | AttachmentKind::Image) {
            return Some(id);
        }
        first.get_or_insert(id);
    }
    first
}

pub(crate) fn clip_marker(content_id: &str) -> Option<Vec<u8>> {
    offer_marker(&parse_content_id(content_id).ok()?)
}

/// The rich text of text message `parent` for the clipboard, once here.
fn rich_for(store: &TransferStore, parent: crate::MessageId) -> Option<crate::ClipboardRich> {
    store
        .rich_text(parent)
        .ok()
        .flatten()
        .as_ref()
        .and_then(crate::rich_text::clipboard)
}

/// The rich-text child of text message `parent` to download now: not here
/// yet, its source available, and not above the automatic limit.
fn rich_to_fetch(store: &TransferStore, parent: crate::MessageId) -> Option<crate::MessageId> {
    let child = store.rich_child(parent).ok().flatten()?;
    (matches!(
        child.download,
        crate::DownloadState::Placeholder | crate::DownloadState::Failed
    ) && child.source_availability == crate::SourceAvailability::Ready
        && child.size <= MAX_RICH_DOWNLOAD)
        .then_some(child.id)
}

fn rich_downloading(store: &TransferStore, parent: crate::MessageId) -> bool {
    store
        .rich_child(parent)
        .ok()
        .flatten()
        .is_some_and(|child| child.download == crate::DownloadState::Downloading)
}

/// The Oink clip marker naming `offer`.
fn offer_marker(offer: &dto::OfferId) -> Option<Vec<u8>> {
    let clip = offer.clip().ok()?;
    let origin = DeviceId(clip.origin().ok()?.public_key().ok()?.0.try_into().ok()?);
    oink_foundation::clipboard_marker::encode(oink_foundation::content::OfferId {
        clip: oink_foundation::content::ClipId {
            origin,
            seq: clip.seq().ok()?,
        },
        generation: offer.generation().ok()?,
    })
}

fn content_id_for_representation(offer: &dto::OfferId, item: u32, rep: u32) -> Result<String, v2::Error> {
    Ok(format!("{}:r:{item}:{rep}", content_id_from_offer(offer)?))
}

fn file_content_id(offer: &dto::OfferId, node: u64, version: u64) -> Result<String, v2::Error> {
    Ok(format!("{}:f:{node}:{version}", content_id_from_offer(offer)?))
}

pub(crate) fn parse_file_content(value: &str) -> Option<(u64, u64)> {
    let (_, suffix) = value.split_once(":f:")?;
    let (node, version) = suffix.split_once(':')?;
    Some((node.parse().ok()?, version.parse().ok()?))
}

fn parse_content_id(value: &str) -> Result<dto::OfferId, v2::Error> {
    let fields: Vec<_> = value
        .strip_prefix("v2:")
        .ok_or(v2::ErrorCode::MalformedFrame)?
        .split(':')
        .collect();
    if !matches!(fields.len(), 3 | 6) || fields[0].len() != 64 || !fields[0].is_ascii() {
        return Err(v2::ErrorCode::MalformedFrame.into());
    }
    let bytes = (0..fields[0].len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&fields[0][i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| v2::ErrorCode::MalformedFrame)?;
    if bytes.len() != 32 {
        return Err(v2::ErrorCode::MalformedFrame.into());
    }
    let clip = dto::ClipId::from_raw(
        [
            (1, device_bytes(&bytes)?.write()),
            (
                2,
                fields[1]
                    .parse::<u64>()
                    .map_err(|_| v2::ErrorCode::MalformedFrame)?
                    .write(),
            ),
        ]
        .into(),
    )?;
    dto::OfferId::from_raw(
        [
            (1, clip.write()),
            (
                2,
                fields[2]
                    .parse::<u64>()
                    .map_err(|_| v2::ErrorCode::MalformedFrame)?
                    .write(),
            ),
        ]
        .into(),
    )
}

fn device_id(value: &str) -> Result<dto::DeviceId, v2::Error> {
    if value.len() != 64 || !value.is_ascii() {
        return Err(v2::ErrorCode::MalformedFrame.into());
    }
    let bytes = (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| v2::ErrorCode::MalformedFrame)?;
    device_bytes(&bytes)
}
fn device_bytes(bytes: &[u8]) -> Result<dto::DeviceId, v2::Error> {
    if bytes.len() != 32 {
        return Err(v2::ErrorCode::MalformedFrame.into());
    }
    dto::DeviceId::from_raw(
        [
            (1, RawValue::Text(dto::ED25519.into())),
            (2, RawValue::Bytes(bytes.to_vec())),
        ]
        .into(),
    )
}
fn device(value: DeviceId) -> dto::DeviceId {
    device_bytes(&value.0).expect("fixed DeviceId")
}
fn boot(value: Boot) -> dto::Boot {
    dto::Boot::from_raw([(1, device(value.device_id).write()), (2, id16(value.nonce).write())].into())
        .expect("fixed Boot")
}
fn id16(value: [u8; 16]) -> dto::Id16 {
    FixedBytes(value)
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(i64::MAX as u128) as i64)
}
fn message_name(message: &v2::Message) -> &'static str {
    match message {
        v2::Message::ClipboardOffer(_) => "ClipboardOffer",
        v2::Message::ClipboardHeadUpdate(_) => "ClipboardHeadUpdate",
        v2::Message::ClipboardCurrentQuery(_) => "ClipboardCurrentQuery",
        v2::Message::ClipboardCurrentResult(_) => "ClipboardCurrentResult",
        v2::Message::ContentOffer(_) => "ContentOffer",
        v2::Message::ClipboardContentOffer(_) => "ClipboardContentOffer",
        v2::Message::ClipboardShareTextChunk(_) => "ClipboardShareTextChunk",
        v2::Message::ClipboardShareTextEnd(_) => "ClipboardShareTextEnd",
        v2::Message::ContentRepresentationBytes(_) => "ContentRepresentationBytes",
        v2::Message::ContentRepresentationEnd(_) => "ContentRepresentationEnd",
        v2::Message::ClipboardShareResult(_) => "ClipboardShareResult",
        v2::Message::ContentThumbnail(_) => "ContentThumbnail",
        _ => "Other",
    }
}
fn link_error(error: impl std::fmt::Display) -> MobileV2TransportError {
    MobileV2TransportError::Link(error.to_string())
}

fn storage_error(error: impl std::fmt::Display) -> MobileV2TransportError {
    MobileV2TransportError::Storage(error.to_string())
}

fn local_error(error: impl std::fmt::Display) -> MobileV2TransportError {
    MobileV2TransportError::Local(error.to_string())
}

/// A body put into staging for an own message.
enum Body<'a> {
    /// A host-staged file the core takes over.
    Take(&'a str),
    Copy(&'a str),
    Bytes(&'a [u8]),
}

impl<'a> Body<'a> {
    fn of(path: &'a str, staged: bool) -> Self {
        if staged { Self::Take(path) } else { Self::Copy(path) }
    }
}

/// Fills the real sizes of a share from file metadata and checks, before any
/// byte is copied or packaged, that every item can be read. There is no size
/// cap (user 2026-11-10). Folders are walked by metadata only, with the same
/// rules as their packaging.
fn share_metadata(items: &mut [crate::SendItem]) -> Result<(), MobileV2TransportError> {
    for item in items {
        match item {
            crate::SendItem::Text { .. } | crate::SendItem::RichText { .. } => {}
            crate::SendItem::Image { path, encoded_size, .. } => {
                let metadata = fs::symlink_metadata(&*path).map_err(local_error)?;
                if !metadata.is_file() {
                    return Err(local_error("shared image is not a regular file"));
                }
                *encoded_size = metadata.len();
            }
            crate::SendItem::File { path, size, .. } => {
                let metadata = fs::symlink_metadata(&*path).map_err(local_error)?;
                if !metadata.is_file() {
                    return Err(local_error("shared attachment is not a regular file"));
                }
                *size = metadata.len();
            }
            crate::SendItem::Folder { path, .. } => check_folder(Path::new(path))?,
        }
    }
    Ok(())
}

/// Rejects, from metadata, what `archive_folder` would reject.
fn check_folder(root: &Path) -> Result<(), MobileV2TransportError> {
    if !fs::symlink_metadata(root).map_err(local_error)?.is_dir() {
        return Err(local_error("shared folder is not a directory"));
    }
    let mut directories = vec![root.to_path_buf()];
    let mut entries = 0;
    while let Some(directory) = directories.pop() {
        for child in fs::read_dir(directory).map_err(local_error)? {
            let child = child.map_err(local_error)?;
            entries += 1;
            if entries > MAX_FOLDER_ENTRIES {
                return Err(local_error("shared folder has too many entries"));
            }
            let kind = child.file_type().map_err(local_error)?;
            if kind.is_dir() {
                directories.push(child.path());
            } else if !kind.is_file() {
                return Err(local_error("shared folder contains a symlink or special file"));
            }
        }
    }
    Ok(())
}

/// A failed local file operation of a share: no room is its own reason.
fn io_failure(error: std::io::Error) -> MobileV2TransportError {
    if error.kind() == std::io::ErrorKind::StorageFull {
        MobileV2TransportError::StorageFull
    } else {
        local_error(error)
    }
}

fn protocol_error(error: v2::Error) -> MobileV2TransportError {
    MobileV2TransportError::Protocol(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PeerKind, PeerState, SendItem, SendOrigin, TransferStore};
    use std::sync::Mutex;
    #[derive(Default)]
    struct FakeLink {
        sent: Mutex<Vec<(String, Vec<u8>)>>,
        low: Mutex<Vec<ClipboardLowOperation>>,
        session: Mutex<Weak<MobileV2Session<Self>>>,
        manual_clock: AtomicBool,
        clock_requests: Mutex<Vec<(String, dto::ClipboardCurrentQuery)>>,
    }
    pub(crate) fn clock_result(
        peer: &str,
        query: &dto::ClipboardCurrentQuery,
        clock: u64,
    ) -> dto::ClipboardCurrentResult {
        dto::ClipboardCurrentResult::from_raw(
            [
                (1, query.request_id().unwrap().write()),
                (
                    2,
                    dto::Boot::from_raw([(1, device_id(peer).unwrap().write()), (2, id16([8; 16]).write())].into())
                        .unwrap()
                        .write(),
                ),
                (3, dto::CurrentResultStatus::Empty.write()),
                (7, clock.write()),
            ]
            .into(),
        )
        .unwrap()
    }
    impl MobileV2Link for FakeLink {
        fn send_frame(&self, peer_id: &str, frame: Vec<u8>) -> Result<(), MobileV2TransportError> {
            let session = self.session.lock().unwrap().upgrade();
            if let (Some(session), v2::Message::ClipboardCurrentQuery(query)) = (
                session,
                OpaqueFrame::parse_exact(&frame, MAX_FRAME).unwrap().decode().unwrap(),
            ) && session.transport.is_clock_query(peer_id, query.request_id().unwrap().0)
            {
                // The fixture PC answers publication metadata immediately by default.
                // Capture these RPCs separately from application frames; manual tests
                // release them in controlled order to exercise the actual barrier.
                self.clock_requests
                    .lock()
                    .unwrap()
                    .push((peer_id.into(), query.clone()));
                if !self.manual_clock.load(Ordering::SeqCst) {
                    session.receive_message(
                        peer_id,
                        v2::Message::ClipboardCurrentResult(clock_result(peer_id, &query, 0)),
                    )?;
                }
                return Ok(());
            }
            self.sent.lock().unwrap().push((peer_id.into(), frame));
            Ok(())
        }
        fn send_low(&self, _peer_id: &str, operation: ClipboardLowOperation) -> Result<(), MobileV2TransportError> {
            self.low.lock().unwrap().push(operation);
            Ok(())
        }
    }
    /// A session whose pushed events the test reads back with `take_events`.
    struct Recorded {
        session: Arc<MobileV2Session<FakeLink>>,
        events: Arc<Mutex<Vec<TransferEvent>>>,
    }
    impl std::ops::Deref for Recorded {
        type Target = MobileV2Session<FakeLink>;
        fn deref(&self) -> &Self::Target {
            &self.session
        }
    }
    impl Recorded {
        fn take_events(&self) -> Vec<TransferEvent> {
            std::mem::take(&mut *self.events.lock().unwrap())
        }
    }
    fn session(link: Arc<FakeLink>) -> (tempfile::TempDir, Recorded) {
        let dir = tempfile::tempdir().unwrap();
        let store = TransferStore::open(dir.path().join("db"), dir.path().join("files")).unwrap();
        let session = Arc::new(MobileV2Session::new(
            store,
            MobileV2Identity::from_parts([1; 32], [2; 16], [3; 16]),
            link.clone(),
        ));
        *link.session.lock().unwrap() = Arc::downgrade(&session);
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        session.set_event_sink(Arc::new(move |event| sink.lock().unwrap().push(event)));
        (dir, Recorded { session, events })
    }
    fn inline_share(operation: u8) -> SharePayload {
        SharePayload {
            sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
            items: vec![SendItem::text(
                "new phone head".into(),
                Some("<b>new phone head</b>".into()),
                None,
            )],
            origin: SendOrigin::TransferPage,
            operation_id: [operation; 16],
            message_id: [operation; 16],
        }
    }
    #[test]
    fn publication_waits_for_every_pc_clock_without_subscribing_and_becomes_new_head() {
        let link = Arc::new(FakeLink::default());
        link.manual_clock.store(true, Ordering::SeqCst);
        let (_dir, s) = session(link.clone());
        let first = "04".repeat(32);
        let second = "05".repeat(32);
        s.set_peers(vec![pc(&first), pc(&second)]);
        let ids = s.send_share(inline_share(61), crate::ProxyChoice::Automatic).unwrap();
        assert!(
            link.sent.lock().unwrap().is_empty(),
            "clock probes neither publish nor subscribe"
        );
        assert!(
            s.take_events()
                .iter()
                .all(|e| !matches!(e, TransferEvent::DeliveryEvent { .. }))
        );
        let queries = link.clock_requests.lock().unwrap().clone();
        assert_eq!(queries.len(), 2);
        for (index, (peer, query)) in queries.iter().enumerate() {
            let clock = if *peer == first { 200 } else { 900 };
            s.receive_message(
                peer,
                v2::Message::ClipboardCurrentResult(clock_result(peer, query, clock)),
            )
            .unwrap();
            if index == 0 {
                assert!(
                    link.sent.lock().unwrap().is_empty(),
                    "another PC clock is still missing"
                );
                assert_eq!(s.with_store(|store| store.load_history()).unwrap().sequence, 0);
            }
        }
        let history = s.with_store(|store| store.load_history()).unwrap();
        assert_eq!(history.bodies.records()[0].clock, 901);
        assert_eq!(history.clock, 901);
        assert_eq!(
            s.with_store(|store| store.by_id(ids[0])).unwrap().unwrap().delivery,
            Some(DeliveryState::Sent)
        );
        let sent = link.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|(_, bytes)| matches!(
            OpaqueFrame::parse_exact(bytes, MAX_FRAME).unwrap().decode().unwrap(),
            v2::Message::HistoryDelta(_)
        )));
    }
    #[test]
    fn cancelled_clock_barrier_ignores_late_replies_without_allocating_an_identity() {
        let link = Arc::new(FakeLink::default());
        link.manual_clock.store(true, Ordering::SeqCst);
        let (_dir, s) = session(link.clone());
        let peer = "04".repeat(32);
        s.set_peers(vec![pc(&peer)]);
        let ids = s.send_share(inline_share(62), crate::ProxyChoice::Automatic).unwrap();
        let query = link.clock_requests.lock().unwrap()[0].1.clone();
        s.cancel_share([62; 16]).unwrap();
        s.receive_message(
            &peer,
            v2::Message::ClipboardCurrentResult(clock_result(&peer, &query, 400)),
        )
        .unwrap();
        assert_eq!(s.with_store(|store| store.load_history()).unwrap().sequence, 0);
        assert!(link.sent.lock().unwrap().is_empty());
        assert!(
            s.with_store(|store| store.by_id(ids[0])).unwrap().is_none(),
            "cancelled own records are removed"
        );
        assert!(s.take_events().iter().any(|event| matches!(
            event,
            TransferEvent::DeliveryEvent {
                failure: Some(DeliveryFailure::Cancelled),
                ..
            }
        )));
    }
    #[test]
    fn clock_barrier_fails_on_pause_or_link_loss_and_keeps_observed_clock() {
        for paused in [false, true] {
            let link = Arc::new(FakeLink::default());
            link.manual_clock.store(true, Ordering::SeqCst);
            let (_dir, s) = session(link.clone());
            let a = "04".repeat(32);
            let b = "05".repeat(32);
            s.set_peers(vec![pc(&a), pc(&b)]);
            let ids = s.send_share(inline_share(63), crate::ProxyChoice::Automatic).unwrap();
            let query = link
                .clock_requests
                .lock()
                .unwrap()
                .iter()
                .find(|(p, _)| p == &a)
                .unwrap()
                .1
                .clone();
            s.receive_message(&a, v2::Message::ClipboardCurrentResult(clock_result(&a, &query, 333)))
                .unwrap();
            if paused {
                let mut peer = pc(&b);
                peer.accepts_clipboard = false;
                s.set_peers(vec![pc(&a), peer]);
            } else {
                s.peer_lost(&b);
            }
            assert_eq!(s.with_store(|store| store.load_history()).unwrap().clock, 333);
            assert_eq!(s.with_store(|store| store.load_history()).unwrap().sequence, 0);
            assert_eq!(
                s.with_store(|store| store.by_id(ids[0])).unwrap().unwrap().delivery,
                Some(DeliveryState::Failed)
            );
            assert!(link.sent.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn a_clock_barrier_does_not_count_as_success_when_low_publication_fails() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let peer = "04".repeat(32);
        s.set_peers(vec![pc(&peer)]);
        let mut payload = inline_share(65);
        payload.items = vec![SendItem::Text {
            text: "x".repeat(20_000),
        }];
        let ids = s.send_share(payload, crate::ProxyChoice::Automatic).unwrap();
        assert_eq!(link.low.lock().unwrap().len(), 1);
        assert_eq!(
            s.with_store(|store| store.by_id(ids[0])).unwrap().unwrap().delivery,
            Some(DeliveryState::Sending)
        );
        s.finish_history_send(&peer, [65; 16], Err(link_error("publication interrupted before EOF")));
        assert_eq!(
            s.with_store(|store| store.by_id(ids[0])).unwrap().unwrap().delivery,
            Some(DeliveryState::Failed)
        );
    }
    #[test]
    fn a_clock_persistence_error_settles_without_holding_the_store() {
        let link = Arc::new(FakeLink::default());
        link.manual_clock.store(true, Ordering::SeqCst);
        let (dir, s) = session(link.clone());
        let peer = "04".repeat(32);
        s.set_peers(vec![pc(&peer)]);
        let ids = s.send_share(inline_share(67), crate::ProxyChoice::Automatic).unwrap();
        let query = link.clock_requests.lock().unwrap()[0].1.clone();
        // The fixture's metadata store fails while the outgoing message table remains usable.
        rusqlite::Connection::open(dir.path().join("db"))
            .unwrap()
            .execute_batch("DROP TABLE shared_history")
            .unwrap();
        assert!(
            s.receive_message(
                &peer,
                v2::Message::ClipboardCurrentResult(clock_result(&peer, &query, 50))
            )
            .is_err()
        );
        assert_eq!(
            s.with_store(|store| store.by_id(ids[0])).unwrap().unwrap().delivery,
            Some(DeliveryState::Failed)
        );
        assert!(link.sent.lock().unwrap().is_empty());
    }
    #[test]
    fn session_shutdown_ends_a_clock_barrier_without_publishing() {
        let link = Arc::new(FakeLink::default());
        link.manual_clock.store(true, Ordering::SeqCst);
        let (_dir, s) = session(link.clone());
        let peer = "04".repeat(32);
        s.set_peers(vec![pc(&peer)]);
        let ids = s.send_share(inline_share(66), crate::ProxyChoice::Automatic).unwrap();
        s.stop_target_waits();
        let query = link.clock_requests.lock().unwrap()[0].1.clone();
        s.receive_message(
            &peer,
            v2::Message::ClipboardCurrentResult(clock_result(&peer, &query, 77)),
        )
        .unwrap();
        assert_eq!(s.with_store(|store| store.load_history()).unwrap().sequence, 0);
        assert_eq!(
            s.with_store(|store| store.by_id(ids[0])).unwrap().unwrap().delivery,
            Some(DeliveryState::Failed)
        );
        assert!(link.sent.lock().unwrap().is_empty());
    }
    fn accept(session: &MobileV2Session<FakeLink>, operation_id: [u8; 16]) {
        let accept = dto::ClipboardShareAccept::from_raw([(1, id16(operation_id).write())].into()).unwrap();
        session
            .receive_message(&"04".repeat(32), v2::Message::ClipboardShareAccept(accept))
            .unwrap();
    }
    #[test]
    fn fully_inline_text_is_published_as_phone_record_without_holding() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        s.set_peers(vec![Peer {
            id: "04".repeat(32),
            kind: PeerKind::Pc,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        }]);
        s.send_share(
            SharePayload {
                sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
                items: vec![SendItem::Text { text: "hello".into() }],
                origin: SendOrigin::TransferPage,
                operation_id: [4; 16],
                message_id: [5; 16],
            },
            crate::ProxyChoice::Automatic,
        )
        .unwrap();
        let sent = link
            .sent
            .lock()
            .unwrap()
            .first()
            .map(|(_, frame)| frame.clone())
            .unwrap();
        assert!(
            matches!(OpaqueFrame::parse_exact(&sent,MAX_FRAME).unwrap().decode().unwrap(),v2::Message::HistoryDelta(delta) if matches!(delta.to_domain().unwrap(),oink_foundation::clipboard_history::HistoryChange::Record(r) if r.id.clip.origin==s.device_id() && r.inline[0].bytes==b"hello"))
        );
        assert!(link.low.lock().unwrap().is_empty());
        assert_eq!(link.sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn attachment_share_uses_content_offer_and_representation_bytes_for_proxy() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        std::fs::write(&path, b"proxy content").unwrap();
        s.set_peers(vec![Peer {
            id: "04".repeat(32),
            kind: PeerKind::Pc,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        }]);
        s.send_share(
            SharePayload {
                sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
                items: vec![SendItem::File {
                    path: path.to_string_lossy().into_owned(),
                    name: "payload.bin".into(),
                    size: 13,
                    mime: Some("application/octet-stream".into()),
                    staged: false,
                }],
                origin: SendOrigin::IosShare,
                operation_id: [6; 16],
                message_id: [7; 16],
            },
            crate::ProxyChoice::Automatic,
        )
        .unwrap();
        let frame = link.sent.lock().unwrap()[0].1.clone();
        let v2::Message::ClipboardShareBegin(begin) =
            OpaqueFrame::parse_exact(&frame, MAX_FRAME).unwrap().decode().unwrap()
        else {
            panic!("expected a share begin")
        };
        assert_eq!(begin.delivery().unwrap(), dto::ShareDelivery::Directed);
        accept(&s, [6; 16]);
        let low = link.low.lock().unwrap();
        assert!(matches!(
            low[0].parts.as_slice(),
            [LowPart::Message(v2::Message::ContentOffer(offer)),
             LowPart::Representation { size: 13, source: BodySource::File(_), .. },
             LowPart::Message(v2::Message::ClipboardShareTextEnd(_))]
             if offer.kind().unwrap() == dto::ContentKind::File && offer.proxy().unwrap().is_none()
        ));
    }
    #[test]
    fn clip_marker_names_the_offer_of_a_received_record_and_nothing_else() {
        let origin = DeviceId([7; 32]);
        let offer = offer_id(origin, [5; 16], 2);
        let expected = oink_foundation::content::OfferId {
            clip: oink_foundation::content::ClipId {
                origin,
                seq: u64::from_le_bytes([5; 8]),
            },
            generation: 3,
        };
        for content_id in [
            content_id_for_representation(&offer, 1, 2).unwrap(),
            file_content_id(&offer, 4, 1).unwrap(),
        ] {
            let marker = clip_marker(&content_id).unwrap();
            assert_eq!(Some(marker), oink_foundation::clipboard_marker::encode(expected));
        }
        // A direct share has no clip identity.
        assert_eq!(clip_marker(&format!("text-{}-{}", hex(&origin.0), hex(&[5; 16]))), None);
    }
    #[test]
    fn fake_link_frame_round_trip_decodes_only_v2_message() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let message = current_query(&MobileV2Identity::from_parts([4; 32], [2; 16], [3; 16]), [8; 16], 4).unwrap();
        let frame = v2::frame::encode(&message, MAX_FRAME).unwrap();
        s.receive_frame(&"04".repeat(32), &frame).unwrap();
        assert!(
            matches!(s.take_events().first(), Some(TransferEvent::MessageReceived { message, .. }) if message == "ClipboardCurrentQuery")
        );
    }
    #[test]
    fn content_offer_chunk_end_round_trip_updates_store() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link);
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        let clip = dto::ClipId::from_raw([(1, device(source.device).write()), (2, 9u64.write())].into()).unwrap();
        let offer_id = dto::OfferId::from_raw([(1, clip.write()), (2, 1u64.write())].into()).unwrap();
        let operation_id = [8; 16];
        let offer = dto::ContentOffer::from_raw(
            [
                (1, offer_id.write()),
                (2, id16(operation_id).write()),
                (3, id16([9; 16]).write()),
                (4, 1u32.write()),
                (5, 2u32.write()),
                (6, boot(source.boot).write()),
                (7, device(source.device).write()),
                (9, dto::ContentKind::File.write()),
                (10, RawValue::Text("application/octet-stream".into())),
                (13, dto::ContentAvailability::Ready.write()),
                (14, 10u64.write()),
            ]
            .into(),
        )
        .unwrap();
        s.receive_message(&"07".repeat(32), v2::Message::ContentOffer(offer))
            .unwrap();
        let record = s
            .coordinator
            .store
            .lock()
            .unwrap()
            .page(None, 10)
            .unwrap()
            .messages
            .remove(0);
        s.coordinator.store.lock().unwrap().begin_download(record.id).unwrap();
        let request = v2::Message::ClipboardContentRead(
            dto::ClipboardContentRead::from_raw(
                [
                    (1, id16(operation_id).write()),
                    (2, offer_id.write()),
                    (3, 1u32.write()),
                    (4, 2u32.write()),
                ]
                .into(),
            )
            .unwrap(),
        );
        let bytes = dto::ContentRepresentationBytes::from_raw(
            [
                (1, offer_id.write()),
                (2, 2u32.write()),
                (3, 0u64.write()),
                (4, RawValue::Bytes(b"abc".to_vec())),
            ]
            .into(),
        )
        .unwrap();
        let attempt = s.download_attempt(&request).unwrap();
        s.receive_low_reply(
            operation_id,
            &request,
            attempt,
            v2::Message::ContentRepresentationBytes(bytes),
        )
        .unwrap();
        let end = dto::ContentRepresentationEnd::from_raw(
            [(1, offer_id.write()), (2, 2u32.write()), (3, 3u64.write())].into(),
        )
        .unwrap();
        s.receive_low_reply(
            operation_id,
            &request,
            attempt,
            v2::Message::ContentRepresentationEnd(end),
        )
        .unwrap();
        let done = s.coordinator.store.lock().unwrap().by_id(record.id).unwrap().unwrap();
        assert_eq!(done.download, crate::DownloadState::Complete);
        assert_eq!(std::fs::read(done.local_path.unwrap()).unwrap(), b"abc");
    }

    #[tokio::test]
    async fn current_text_ignores_unrelated_replies_and_waits_for_the_offered_body() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [4; 16]);
        let peer = source.device.to_hex();
        s.set_peers(vec![Peer {
            id: peer.clone(),
            kind: PeerKind::Pc,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        }]);
        let receive = s.receive_current_text(&peer);
        tokio::pin!(receive);
        assert!(
            std::future::poll_fn(|cx| {
                use std::future::Future;
                std::task::Poll::Ready(receive.as_mut().poll(cx).is_pending())
            })
            .await
        );
        let frame = link.sent.lock().unwrap()[0].1.clone();
        let v2::Message::ClipboardCurrentQuery(query) =
            OpaqueFrame::parse_exact(&frame, MAX_FRAME).unwrap().decode().unwrap()
        else {
            panic!("expected current query")
        };
        let wrong = dto::ClipboardCurrentResult::from_raw(
            [
                (7, 0u64.write()),
                (1, id16([42; 16]).write()),
                (2, boot(source.boot).write()),
                (3, dto::CurrentResultStatus::Empty.write()),
            ]
            .into(),
        )
        .unwrap();
        s.receive_message(&peer, v2::Message::ClipboardCurrentResult(wrong))
            .unwrap();
        assert!(
            std::future::poll_fn(|cx| {
                use std::future::Future;
                std::task::Poll::Ready(receive.as_mut().poll(cx).is_pending())
            })
            .await
        );

        let clip = dto::ClipId::from_raw([(1, device(source.device).write()), (2, 9u64.write())].into()).unwrap();
        let id = dto::OfferId::from_raw([(1, clip.write()), (2, 1u64.write())].into()).unwrap();
        let body = b"current text";
        let format = RawValue::from("mime:text/plain;charset=utf-8");
        let representation = dto::Representation::from_raw(
            [
                (1, 2u32.write()),
                (2, format.clone()),
                (3, dto::Medium::Bytes.write()),
                (4, 0u32.write()),
                (5, 0i32.write()),
                (6, format),
                (
                    7,
                    RawValue::Map(
                        [
                            (1, dto::RepresentationDataKind::Inline.write()),
                            (2, RawValue::Map([(1, RawValue::Bytes(body.to_vec()))].into())),
                        ]
                        .into(),
                    ),
                ),
            ]
            .into(),
        )
        .unwrap();
        let item = dto::ContentItem::from_raw(
            [
                (1, 1u32.write()),
                (3, RawValue::Array(vec![representation.write()])),
                (4, false.write()),
            ]
            .into(),
        )
        .unwrap();
        let inventory = dto::Inventory::from_raw(
            [
                (1, id.write()),
                (2, RawValue::from("macos")),
                (3, RawValue::Array(vec![item.write()])),
                (4, RawValue::Array(Vec::new())),
            ]
            .into(),
        )
        .unwrap();
        let offer = dto::ClipboardOffer::from_raw(
            [
                (1, id.write()),
                (2, 1u64.write()),
                (3, 1u64.write()),
                (4, dto::Sensitivity::Concealed.write()),
                (6, inventory.write()),
                (7, 0u64.write()),
                (8, device(source.device).write()),
                (10, id16([8; 16]).write()),
                (11, id16([9; 16]).write()),
                (12, 1u32.write()),
                (13, oink_protocol::v2::RawValue::Array(vec![])),
            ]
            .into(),
        )
        .unwrap();
        let correct = dto::ClipboardCurrentResult::from_raw(
            [
                (7, 0u64.write()),
                (1, query.request_id().unwrap().write()),
                (2, boot(source.boot).write()),
                (3, dto::CurrentResultStatus::Ready.write()),
                (4, offer.write()),
                (5, device(source.device).write()),
            ]
            .into(),
        )
        .unwrap();
        s.receive_message(&peer, v2::Message::ClipboardCurrentResult(correct))
            .unwrap();
        let read = link.low.lock().unwrap()[0].read.as_ref().unwrap().clone();
        let attempt = s.download_attempt(&read).unwrap();
        assert!(
            std::future::poll_fn(|cx| {
                use std::future::Future;
                std::task::Poll::Ready(receive.as_mut().poll(cx).is_pending())
            })
            .await
        );
        let chunk = dto::ContentRepresentationBytes::from_raw(
            [
                (1, id.write()),
                (2, 2u32.write()),
                (3, 0u64.write()),
                (4, RawValue::Bytes(body.to_vec())),
            ]
            .into(),
        )
        .unwrap();
        s.receive_low_reply([8; 16], &read, attempt, v2::Message::ContentRepresentationBytes(chunk))
            .unwrap();
        let end = dto::ContentRepresentationEnd::from_raw(
            [(1, id.write()), (2, 2u32.write()), (3, (body.len() as u64).write())].into(),
        )
        .unwrap();
        s.receive_low_reply([8; 16], &read, attempt, v2::Message::ContentRepresentationEnd(end))
            .unwrap();
        assert_eq!(
            receive.await.unwrap(),
            Some(CurrentClipboardText {
                text: "current text".into(),
                sensitive: true,
                rich: None,
                // The offer's identity: the PC that announced it recognises
                // this text when Universal Clipboard brings it back.
                marker: oink_foundation::clipboard_marker::encode(oink_foundation::content::OfferId {
                    clip: oink_foundation::content::ClipId {
                        origin: source.device,
                        seq: 9
                    },
                    generation: 1
                })
            })
        );
    }

    /// Polls `future` once; true while it is still waiting.
    async fn still_pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) -> bool {
        std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx).is_pending())).await
    }

    /// A ready PC for current-clipboard requests.
    fn current_source(s: &MobileV2Session<FakeLink>) -> (MobileV2Identity, String) {
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [4; 16]);
        let peer = source.device.to_hex();
        s.set_peers(vec![Peer {
            id: peer.clone(),
            kind: PeerKind::Pc,
            state: PeerState::Ready,
            accepts_clipboard: true,
            clipboard_pending: false,
            connection_order: 1,
            relayed: false,
        }]);
        (source, peer)
    }

    fn current_offer_id(source: &MobileV2Identity) -> dto::OfferId {
        let clip = dto::ClipId::from_raw([(1, device(source.device).write()), (2, 9u64.write())].into()).unwrap();
        dto::OfferId::from_raw([(1, clip.write()), (2, 1u64.write())].into()).unwrap()
    }

    /// One content item whose single representation has `format` and `data`.
    fn current_item(format: &str, data: RawValue) -> RawValue {
        let format = RawValue::from(format);
        let representation = dto::Representation::from_raw(
            [
                (1, 2u32.write()),
                (2, format.clone()),
                (3, dto::Medium::Bytes.write()),
                (4, 0u32.write()),
                (5, 0i32.write()),
                (6, format),
                (7, data),
            ]
            .into(),
        )
        .unwrap();
        dto::ContentItem::from_raw(
            [
                (1, 1u32.write()),
                (3, RawValue::Array(vec![representation.write()])),
                (4, false.write()),
            ]
            .into(),
        )
        .unwrap()
        .write()
    }

    fn inline_data(body: &[u8]) -> RawValue {
        RawValue::Map(
            [
                (1, dto::RepresentationDataKind::Inline.write()),
                (2, RawValue::Map([(1, RawValue::Bytes(body.to_vec()))].into())),
            ]
            .into(),
        )
    }

    fn reader_data(size: u64) -> RawValue {
        let size = RawValue::Map(
            [
                (1, dto::ContentSizeKind::Known.write()),
                (2, RawValue::Map([(1, size.write())].into())),
            ]
            .into(),
        );
        RawValue::Map(
            [
                (1, dto::RepresentationDataKind::Reader.write()),
                (2, RawValue::Map([(1, size)].into())),
            ]
            .into(),
        )
    }

    /// Answers the session's pending current query with an offer of `items` and `roots`.
    fn answer_current(
        s: &MobileV2Session<FakeLink>,
        link: &FakeLink,
        source: &MobileV2Identity,
        peer: &str,
        items: Vec<RawValue>,
        roots: Vec<RawValue>,
    ) {
        let frame = link.sent.lock().unwrap().last().unwrap().1.clone();
        let v2::Message::ClipboardCurrentQuery(query) =
            OpaqueFrame::parse_exact(&frame, MAX_FRAME).unwrap().decode().unwrap()
        else {
            panic!("expected current query")
        };
        let id = current_offer_id(source);
        let inventory = dto::Inventory::from_raw(
            [
                (1, id.write()),
                (2, RawValue::from("macos")),
                (3, RawValue::Array(items)),
                (4, RawValue::Array(roots)),
            ]
            .into(),
        )
        .unwrap();
        let offer = dto::ClipboardOffer::from_raw(
            [
                (1, id.write()),
                (2, 1u64.write()),
                (3, 1u64.write()),
                (4, dto::Sensitivity::Normal.write()),
                (6, inventory.write()),
                (7, 0u64.write()),
                (8, device(source.device).write()),
                (10, id16([8; 16]).write()),
                (11, id16([9; 16]).write()),
                (12, 1u32.write()),
                (13, oink_protocol::v2::RawValue::Array(vec![])),
            ]
            .into(),
        )
        .unwrap();
        let result = dto::ClipboardCurrentResult::from_raw(
            [
                (7, 0u64.write()),
                (1, query.request_id().unwrap().write()),
                (2, boot(source.boot).write()),
                (3, dto::CurrentResultStatus::Ready.write()),
                (4, offer.write()),
                (5, device(source.device).write()),
            ]
            .into(),
        )
        .unwrap();
        s.receive_message(peer, v2::Message::ClipboardCurrentResult(result))
            .unwrap();
    }

    #[tokio::test]
    async fn current_clipboard_downloads_an_image_on_demand() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let (source, peer) = current_source(&s);
        let body = b"\x89PNG\r\n\x1a\nimage body";
        let receive = s.receive_current_clipboard(&peer);
        tokio::pin!(receive);
        assert!(still_pending(receive.as_mut()).await);
        answer_current(
            &s,
            &link,
            &source,
            &peer,
            vec![current_item("mime:image/png", inline_data(body))],
            vec![],
        );
        // Automatic image download is off, so the request starts it.
        assert!(link.low.lock().unwrap().is_empty());
        assert!(still_pending(receive.as_mut()).await);
        let read = link.low.lock().unwrap()[0].read.as_ref().unwrap().clone();
        let attempt = s.download_attempt(&read).unwrap();
        let id = current_offer_id(&source);
        let chunk = dto::ContentRepresentationBytes::from_raw(
            [
                (1, id.write()),
                (2, 2u32.write()),
                (3, 0u64.write()),
                (4, RawValue::Bytes(body.to_vec())),
            ]
            .into(),
        )
        .unwrap();
        s.receive_low_reply([8; 16], &read, attempt, v2::Message::ContentRepresentationBytes(chunk))
            .unwrap();
        let end = dto::ContentRepresentationEnd::from_raw(
            [(1, id.write()), (2, 2u32.write()), (3, (body.len() as u64).write())].into(),
        )
        .unwrap();
        s.receive_low_reply([8; 16], &read, attempt, v2::Message::ContentRepresentationEnd(end))
            .unwrap();
        let Some(CurrentClipboard::Image(image)) = receive.await.unwrap() else {
            panic!("expected an image")
        };
        assert_eq!(std::fs::read(&image.path).unwrap(), body);
        assert_eq!(image.mime.as_deref(), Some("image/png"));
        assert_eq!(image.size, body.len() as u64);
        assert!(image.marker.is_some());
        // The image is the conversation's record too, downloaded once.
        assert_eq!(link.low.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn current_clipboard_reports_an_oversized_image_without_downloading_it() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let (source, peer) = current_source(&s);
        let size = MAX_CURRENT_IMAGE_BYTES + 1;
        let receive = s.receive_current_clipboard(&peer);
        tokio::pin!(receive);
        assert!(still_pending(receive.as_mut()).await);
        answer_current(
            &s,
            &link,
            &source,
            &peer,
            vec![current_item("mime:image/png", reader_data(size))],
            vec![],
        );
        assert_eq!(
            receive.await.unwrap(),
            Some(CurrentClipboard::ImageTooLarge {
                size,
                limit: MAX_CURRENT_IMAGE_BYTES
            })
        );
        assert!(link.low.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn current_clipboard_reports_files_as_unsupported() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let (source, peer) = current_source(&s);
        let root = dto::FileNode::from_raw(
            [
                (1, 1u64.write()),
                (3, RawValue::from("notes.pdf")),
                (4, dto::NodeKind::File.write()),
                (
                    5,
                    RawValue::Map([(1, dto::ContentSizeKind::UnknownSize.write())].into()),
                ),
                (6, 1u64.write()),
            ]
            .into(),
        )
        .unwrap();
        let receive = s.receive_current_clipboard(&peer);
        tokio::pin!(receive);
        assert!(still_pending(receive.as_mut()).await);
        answer_current(&s, &link, &source, &peer, vec![], vec![root.write()]);
        assert_eq!(
            receive.await.unwrap(),
            Some(CurrentClipboard::Unsupported {
                kind: AttachmentKind::File
            })
        );
        assert!(link.low.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn current_text_does_not_download_an_image() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let (source, peer) = current_source(&s);
        let receive = s.receive_current_text(&peer);
        tokio::pin!(receive);
        assert!(still_pending(receive.as_mut()).await);
        answer_current(
            &s,
            &link,
            &source,
            &peer,
            vec![current_item("mime:image/png", inline_data(b"\x89PNG"))],
            vec![],
        );
        assert_eq!(receive.await.unwrap(), None);
        assert!(link.low.lock().unwrap().is_empty());
    }

    /// 2026-11-12: text with rich text goes out as one item whose
    /// representations are the plain text, HTML and RTF; the own message is
    /// text that keeps its HTML.
    #[test]
    fn rich_text_share_sends_every_format_as_representations_of_one_item() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        s.set_peers(vec![pc(&"04".repeat(32))]);
        let html = "<p>hi <b>there</b></p>";
        let rtf = r"{\rtf1 hi \b there\b0}";
        s.send_share(
            SharePayload {
                sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
                items: vec![SendItem::text("hi there".into(), Some(html.into()), Some(rtf.into()))],
                origin: SendOrigin::TransferPage,
                operation_id: [4; 16],
                message_id: [5; 16],
            },
            crate::ProxyChoice::Automatic,
        )
        .unwrap();
        let sent = link.sent.lock().unwrap()[0].1.clone();
        let v2::Message::HistoryDelta(delta) = OpaqueFrame::parse_exact(&sent, MAX_FRAME).unwrap().decode().unwrap()
        else {
            panic!("expected history delta");
        };
        let oink_foundation::clipboard_history::HistoryChange::Record(record) = delta.to_domain().unwrap() else {
            panic!("expected record");
        };
        assert_eq!(record.items.len(), 1);
        let bodies: Vec<_> = record
            .inline
            .iter()
            .map(|p| (p.representation, String::from_utf8(p.bytes.clone()).unwrap()))
            .collect();
        assert_eq!(bodies, vec![(0, "hi there".into()), (1, html.into()), (2, rtf.into())]);
        assert!(link.low.lock().unwrap().is_empty());
        let store = s.coordinator.store.lock().unwrap();
        let page = store.page(None, 10).unwrap().messages;
        assert_eq!(page.len(), 1, "the rich text is not a message of its own");
        assert_eq!(page[0].kind, AttachmentKind::Text);
        assert_eq!(page[0].text.as_deref(), Some("hi there"));
        assert_eq!(page[0].rich_text_format, Some(crate::RichTextFormat::Html));
        let rich = store.rich_text(page[0].id).unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&rich.path).unwrap(), html);
        assert_eq!(
            crate::rich_text::clipboard(&rich),
            Some(crate::ClipboardRich {
                html: html.into(),
                rtf: None
            })
        );
    }

    /// One PC clipboard item whose representations are `reps` (id, portable
    /// format, Reader size).
    fn reader_item(reps: &[(u32, &str, u64)]) -> RawValue {
        let representations = reps
            .iter()
            .map(|(id, format, size)| {
                let format = RawValue::from(*format);
                dto::Representation::from_raw(
                    [
                        (1, id.write()),
                        (2, format.clone()),
                        (3, dto::Medium::Bytes.write()),
                        (4, 0u32.write()),
                        (5, 0i32.write()),
                        (6, format),
                        (7, reader_data(*size)),
                    ]
                    .into(),
                )
                .unwrap()
                .write()
            })
            .collect();
        dto::ContentItem::from_raw(
            [
                (1, 1u32.write()),
                (3, RawValue::Array(representations)),
                (4, false.write()),
            ]
            .into(),
        )
        .unwrap()
        .write()
    }

    /// Answers download `index` of `link` with `body` in one chunk.
    fn answer_download(s: &MobileV2Session<FakeLink>, link: &FakeLink, index: usize, body: &[u8]) {
        let read = link.low.lock().unwrap()[index].read.clone().unwrap();
        let v2::Message::ClipboardContentRead(request) = &read else {
            panic!("expected a representation read")
        };
        let offer = request.offer().unwrap();
        let representation = request.representation_id().unwrap();
        let attempt = s.download_attempt(&read).unwrap();
        let bytes = dto::ContentRepresentationBytes::from_raw(
            [
                (1, offer.write()),
                (2, representation.write()),
                (3, 0u64.write()),
                (4, RawValue::Bytes(body.to_vec())),
            ]
            .into(),
        )
        .unwrap();
        s.receive_low_reply([8; 16], &read, attempt, v2::Message::ContentRepresentationBytes(bytes))
            .unwrap();
        let end = dto::ContentRepresentationEnd::from_raw(
            [
                (1, offer.write()),
                (2, representation.write()),
                (3, (body.len() as u64).write()),
            ]
            .into(),
        )
        .unwrap();
        s.receive_low_reply([8; 16], &read, attempt, v2::Message::ContentRepresentationEnd(end))
            .unwrap();
    }

    /// 2026-11-12: a PC copy with plain text, HTML and RTF is one text
    /// message that keeps the HTML (fetched after the text, the RTF is not)
    /// as its hidden child; the clipboard gets the HTML with the text.
    #[test]
    fn a_pc_text_keeps_its_html_next_to_the_plain_text() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let (source, peer) = current_source(&s);
        let html = b"<p>hello <i>you</i></p>";
        let id = current_offer_id(&source);
        let inventory = dto::Inventory::from_raw(
            [
                (1, id.write()),
                (2, RawValue::from("macos")),
                (
                    3,
                    RawValue::Array(vec![reader_item(&[
                        (2, "mime:application/rtf", 30),
                        (3, "mime:text/html;charset=utf-8", html.len() as u64),
                        (4, "mime:text/plain;charset=utf-8", 9),
                    ])]),
                ),
                (4, RawValue::Array(Vec::new())),
            ]
            .into(),
        )
        .unwrap();
        let offer = dto::ClipboardOffer::from_raw(
            [
                (1, id.write()),
                (2, 1u64.write()),
                (3, 1u64.write()),
                (4, dto::Sensitivity::Normal.write()),
                (6, inventory.write()),
                (7, 0u64.write()),
                (8, device(source.device).write()),
                (10, id16([8; 16]).write()),
                (11, id16([9; 16]).write()),
                (12, 1u32.write()),
                (13, oink_protocol::v2::RawValue::Array(vec![])),
            ]
            .into(),
        )
        .unwrap();
        s.receive_message(&peer, v2::Message::ClipboardOffer(offer)).unwrap();
        let page = s.coordinator.store.lock().unwrap().page(None, 10).unwrap().messages;
        assert_eq!(page.len(), 1);
        let message = page[0].clone();
        assert_eq!((message.kind, message.representation_id), (AttachmentKind::Text, 4));
        assert_eq!(message.rich_text_format, Some(crate::RichTextFormat::Html));
        // The text first; its HTML only once the text is here.
        assert_eq!(link.low.lock().unwrap().len(), 1);
        answer_download(&s, &link, 0, b"hello you");
        assert_eq!(link.low.lock().unwrap().len(), 2);
        let v2::Message::ClipboardContentRead(read) = link.low.lock().unwrap()[1].read.clone().unwrap() else {
            panic!("expected the HTML read")
        };
        assert_eq!(read.representation_id().unwrap(), 3);
        answer_download(&s, &link, 1, html);
        let store = s.coordinator.store.lock().unwrap();
        let record = store.by_id(message.id).unwrap().unwrap();
        assert_eq!(record.text.as_deref(), Some("hello you"));
        let rich = store.rich_text(message.id).unwrap().unwrap();
        assert_eq!(crate::rich_text::clipboard(&rich).unwrap().html.as_bytes(), html);
        assert_eq!(store.page(None, 10).unwrap().messages.len(), 1);
        drop(store);
        assert!(s.take_events().iter().any(|event| matches!(event,
            TransferEvent::Announcement { record_id, .. } if *record_id == message.id)));
    }

    #[test]
    fn current_record_prefers_the_first_text_or_image() {
        use AttachmentKind::{File, Folder, Image, RichText, Text};
        assert_eq!(current_record([(File, 1), (Image, 2), (Text, 3)]), Some(2));
        assert_eq!(current_record([(RichText, 1), (Text, 2)]), Some(2));
        assert_eq!(current_record([(Folder, 1), (File, 2)]), Some(1));
        assert_eq!(current_record([]), None);
    }

    #[test]
    fn split_offer_records_every_root_as_its_own_message_of_one_operation() {
        let link = Arc::new(FakeLink::default());
        let (_dir, session) = session(link.clone());
        let peer = DeviceId([7; 32]);
        let peer_id = hex(&peer.0);
        let offer = offer_id(peer, [8; 16], 0);
        let root = |id: u64, name: &str| {
            dto::FileNode::from_raw(
                [
                    (1, id.write()),
                    (3, RawValue::from(name)),
                    (4, dto::NodeKind::File.write()),
                    (
                        5,
                        RawValue::Map([(1, dto::ContentSizeKind::UnknownSize.write())].into()),
                    ),
                    (6, 1u64.write()),
                ]
                .into(),
            )
            .unwrap()
        };
        let prefix = dto::Inventory::from_raw(
            [
                (1, offer.write()),
                (2, RawValue::from("test")),
                (3, RawValue::Array(vec![])),
                (4, RawValue::Array(vec![root(1, "first.txt").write()])),
            ]
            .into(),
        )
        .unwrap();
        let prefix = dto::ClipboardOffer::from_raw(
            [
                (1, offer.write()),
                (2, 1u64.write()),
                (3, 1u64.write()),
                (4, dto::Sensitivity::Normal.write()),
                (6, prefix.write()),
                (7, 1u64.write()),
                (8, device(peer).write()),
                (10, id16([8; 16]).write()),
                (11, id16([9; 16]).write()),
                (12, 2u32.write()),
                (13, oink_protocol::v2::RawValue::Array(vec![])),
            ]
            .into(),
        )
        .unwrap();
        let head = dto::AttachmentHeader::from_raw(
            [
                (1, dto::AttachmentKind::Offer.write()),
                (2, RawValue::Map([(1, prefix.write())].into())),
            ]
            .into(),
        )
        .unwrap();
        let head = v2::Message::AttachmentHead(
            dto::AttachmentHead::from_raw([(1, 1u64.write()), (2, head.write()), (3, 1u32.write())].into()).unwrap(),
        );
        let tail = v2::Message::AttachmentItems(
            dto::AttachmentItems::from_raw(
                [
                    (1, 1u64.write()),
                    (
                        2,
                        RawValue::Map(
                            [
                                (1, RawValue::Array(vec![])),
                                (2, RawValue::Array(vec![root(2, "second.txt").write()])),
                            ]
                            .into(),
                        ),
                    ),
                    (3, dto::AttachmentKind::Offer.write()),
                    (4, offer.write()),
                    (5, 1u32.write()),
                ]
                .into(),
            )
            .unwrap(),
        );
        session.receive_message(&peer_id, tail.clone()).unwrap();
        session.receive_message(&peer_id, head.clone()).unwrap();
        session.receive_message(&peer_id, tail).unwrap();
        session.receive_message(&peer_id, head).unwrap();
        let page = session.coordinator.store.lock().unwrap().page(None, 10).unwrap();
        // Newest first; both roots of the one offer, each its own file record.
        let names: Vec<_> = page.messages.iter().rev().map(|m| m.display_name.as_str()).collect();
        assert_eq!(names, ["first.txt", "second.txt"]);
        assert!(page.messages.iter().all(|m| m.kind == AttachmentKind::File
            && m.operation_id == [8; 16]
            && m.parent_id.is_none()
            && m.item_count.is_none()));
        let events = session.take_events();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, TransferEvent::Announcement { .. }))
                .count(),
            2
        );
    }

    fn pc(id: &str) -> Peer {
        Peer {
            id: id.into(),
            kind: PeerKind::Pc,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        }
    }

    fn result(session: &MobileV2Session<FakeLink>, peer: &str, operation_id: [u8; 16], status: dto::ShareStatus) {
        let result =
            dto::ClipboardShareResult::from_raw([(1, id16(operation_id).write()), (2, status.write())].into()).unwrap();
        session
            .receive_message(peer, v2::Message::ClipboardShareResult(result))
            .unwrap();
    }

    fn deliveries(events: &[TransferEvent]) -> Vec<(Vec<i64>, DeliveryState, Option<DeliveryFailure>)> {
        events
            .iter()
            .filter_map(|event| match event {
                TransferEvent::DeliveryEvent {
                    record_ids,
                    delivery,
                    failure,
                    ..
                } => Some((record_ids.clone(), *delivery, *failure)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn own_share_is_recorded_and_settles_on_the_target_result() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let peer = "04".repeat(32);
        s.set_peers(vec![pc(&peer)]);
        let ids = s
            .send_share(
                SharePayload {
                    sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
                    items: vec![SendItem::Text {
                        text: "hello".repeat(7000),
                    }],
                    origin: SendOrigin::TransferPage,
                    operation_id: [4; 16],
                    message_id: [5; 16],
                },
                crate::ProxyChoice::Automatic,
            )
            .unwrap();
        let record = s.coordinator.store.lock().unwrap().by_id(ids[0]).unwrap().unwrap();
        assert!(record.outgoing);
        assert_eq!(record.text.as_deref(), Some("hello".repeat(7000).as_str()));
        assert_eq!(record.sender_id, hex(&[1; 32]));
        assert_eq!(record.delivery, Some(DeliveryState::Sending));
        let events = s.take_events();
        assert!(deliveries(&events).is_empty());
        accept(&s, [4; 16]);
        result(&s, &peer, [4; 16], dto::ShareStatus::Complete);
        assert_eq!(
            deliveries(&s.take_events()),
            vec![(ids.clone(), DeliveryState::Sent, None)]
        );
        let record = s.coordinator.store.lock().unwrap().by_id(ids[0]).unwrap().unwrap();
        assert_eq!(record.delivery, Some(DeliveryState::Sent));
        assert!(matches!(
            s.request_download(ids[0], false),
            Err(MobileV2TransportError::OwnMessage(id)) if id == ids[0]
        ));
    }

    #[test]
    fn a_rejected_or_unrouted_share_fails_with_a_reason() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let text = |operation: u8| SharePayload {
            sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
            items: vec![SendItem::Text {
                text: if operation == 1 {
                    "x".into()
                } else {
                    "x".repeat(33 * 1024)
                },
            }],
            origin: SendOrigin::TransferPage,
            operation_id: [operation; 16],
            message_id: [operation; 16],
        };
        s.send_share(text(1), crate::ProxyChoice::Automatic).unwrap();
        let failed = deliveries(&s.take_events());
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].1, DeliveryState::Failed);
        assert_eq!(failed[0].2, Some(DeliveryFailure::NoTarget));

        let peer = "04".repeat(32);
        s.set_peers(vec![pc(&peer)]);
        s.send_share(text(2), crate::ProxyChoice::Automatic).unwrap();
        let reject = dto::ClipboardShareReject::from_raw(
            [(1, id16([2; 16]).write()), (2, dto::ShareStatus::Unsupported.write())].into(),
        )
        .unwrap();
        s.receive_message(&peer, v2::Message::ClipboardShareReject(reject))
            .unwrap();
        let rejected = deliveries(&s.take_events());
        assert_eq!(rejected[0].2, Some(DeliveryFailure::Rejected));
        let record = s
            .coordinator
            .store
            .lock()
            .unwrap()
            .by_id(rejected[0].0[0])
            .unwrap()
            .unwrap();
        assert_eq!(record.delivery_detail.as_deref(), Some("Unsupported"));
    }

    /// Review M17: a proxy PC that refuses because of its own state (receiving
    /// off, paused, busy) before accepting is replaced by the next candidate;
    /// a refusal for any other reason fails the share with that reason.
    #[test]
    fn a_proxy_refused_for_its_own_state_moves_to_the_next_candidate() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let first = "04".repeat(32);
        let second = "05".repeat(32);
        let mut later = pc(&second);
        later.connection_order = 2;
        s.set_peers(vec![pc(&first), later]);
        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("report.pdf");
        std::fs::write(&path, b"pdf body").unwrap();
        let file = |operation: u8| SharePayload {
            sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
            items: vec![SendItem::File {
                path: path.to_string_lossy().into_owned(),
                name: "report.pdf".into(),
                size: 0,
                mime: None,
                staged: false,
            }],
            origin: SendOrigin::TransferPage,
            operation_id: [operation; 16],
            message_id: [operation; 16],
        };
        let begins = |peer: &str| {
            link.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|(target, frame)| {
                    target == peer
                        && v2::frame::OpaqueFrame::parse_exact(frame, MAX_FRAME)
                            .is_ok_and(|frame| frame.header.type_id == v2::registry::type_id::ClipboardShareBegin)
                })
                .count()
        };
        let reject = |peer: &str, operation: u8, reason: dto::ShareRejectReason| {
            let reject = dto::ClipboardShareReject::from_raw(
                [
                    (1, id16([operation; 16]).write()),
                    (2, dto::ShareStatus::Unsupported.write()),
                    (3, reason.write()),
                ]
                .into(),
            )
            .unwrap();
            s.receive_message(peer, v2::Message::ClipboardShareReject(reject))
                .unwrap();
        };

        let ids = s.send_share(file(6), crate::ProxyChoice::Automatic).unwrap();
        assert_eq!((begins(&first), begins(&second)), (1, 0));
        reject(&first, 6, dto::ShareRejectReason::Paused);
        assert_eq!((begins(&first), begins(&second)), (1, 1), "moved to the next PC");
        assert!(deliveries(&s.take_events()).is_empty(), "not settled by the refusal");
        let record = s.coordinator.store.lock().unwrap().by_id(ids[0]).unwrap().unwrap();
        assert_eq!(record.proxy_id.as_deref(), Some(second.as_str()));
        result(&s, &second, [6; 16], dto::ShareStatus::Complete);
        assert_eq!(deliveries(&s.take_events()), vec![(ids, DeliveryState::Sent, None)]);

        // Too large is not about that PC: no other candidate is tried.
        let ids = s.send_share(file(7), crate::ProxyChoice::Automatic).unwrap();
        reject(&first, 7, dto::ShareRejectReason::TooLarge);
        assert_eq!(begins(&second), 1);
        assert_eq!(
            deliveries(&s.take_events()),
            vec![(ids.clone(), DeliveryState::Failed, Some(DeliveryFailure::Rejected))]
        );
        let record = s.coordinator.store.lock().unwrap().by_id(ids[0]).unwrap().unwrap();
        assert_eq!(record.delivery_detail.as_deref(), Some("Unsupported: TooLarge"));

        // The last candidate also refusing ends the share with its reason.
        let ids = s.send_share(file(8), crate::ProxyChoice::Automatic).unwrap();
        reject(&first, 8, dto::ShareRejectReason::ReceiveOff);
        reject(&second, 8, dto::ShareRejectReason::Busy);
        assert_eq!(
            deliveries(&s.take_events()),
            vec![(ids, DeliveryState::Failed, Some(DeliveryFailure::Rejected))]
        );
    }

    #[test]
    fn a_started_large_share_outlives_the_accept_window_and_unanswered_ones_fail() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let peer = "04".repeat(32);
        s.set_peers(vec![pc(&peer)]);
        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("movie.mov");
        std::fs::write(&path, b"large body").unwrap();
        let file = |operation: u8| SharePayload {
            sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
            items: vec![SendItem::File {
                path: path.to_string_lossy().into_owned(),
                name: "movie.mov".into(),
                size: 0,
                mime: None,
                staged: false,
            }],
            origin: SendOrigin::TransferPage,
            operation_id: [operation; 16],
            message_id: [operation; 16],
        };
        let started = s.send_share(file(1), crate::ProxyChoice::Automatic).unwrap();
        accept(&s, [1; 16]);
        let unanswered = s.send_share(file(2), crate::ProxyChoice::Automatic).unwrap();
        let old = Instant::now()
            .checked_sub(SHARE_LIFETIME + Duration::from_secs(1))
            .unwrap();
        for share in s.transport.outgoing.lock().unwrap().values_mut() {
            share.created = old;
        }
        s.send_share(file(3), crate::ProxyChoice::Automatic).unwrap();
        let events = s.take_events();
        assert_eq!(
            deliveries(&events),
            vec![(unanswered, DeliveryState::Failed, Some(DeliveryFailure::NotAccepted))]
        );
        result(&s, &peer, [1; 16], dto::ShareStatus::Complete);
        assert_eq!(deliveries(&s.take_events()), vec![(started, DeliveryState::Sent, None)]);
    }

    #[test]
    fn jpeg_photos_are_sent_as_images_through_the_proxy_and_kept_for_display() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let peer = "04".repeat(32);
        s.set_peers(vec![pc(&peer)]);
        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("IMG_0001.JPG");
        std::fs::write(&path, b"jpeg body").unwrap();
        let ids = s
            .send_share(
                SharePayload {
                    sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
                    items: vec![SendItem::Image {
                        path: path.to_string_lossy().into_owned(),
                        encoded_size: 0,
                        mime: "image/jpeg".into(),
                        staged: false,
                        dimensions: Some((3024, 4032)),
                    }],
                    origin: SendOrigin::IosShare,
                    operation_id: [6; 16],
                    message_id: [7; 16],
                },
                crate::ProxyChoice::Automatic,
            )
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        let record = s.coordinator.store.lock().unwrap().by_id(ids[0]).unwrap().unwrap();
        assert_eq!(record.kind, AttachmentKind::Image);
        assert_eq!(record.proxy_id.as_deref(), Some(peer.as_str()));
        // Shown at the original's ratio (a portrait photo), not cropped.
        assert_eq!(record.image_size(), Some((3024, 4032)));
        let copy = PathBuf::from(record.local_path.unwrap());
        assert_eq!(copy.file_name().unwrap(), "IMG_0001.JPG");
        assert_eq!(std::fs::read(&copy).unwrap(), b"jpeg body");
        accept(&s, [6; 16]);
        let low = link.low.lock().unwrap();
        assert!(matches!(
            low[0].parts.as_slice(),
            [LowPart::Message(v2::Message::ContentOffer(offer)),
             LowPart::Representation { size: 9, source: BodySource::File(source), .. },
             LowPart::Message(v2::Message::ClipboardShareTextEnd(_))]
             if offer.kind().unwrap() == dto::ContentKind::Image
                && offer.mime().unwrap().0 == "image/jpeg"
                && *source == copy
        ));
    }

    /// Every image of one share (a batch) keeps its own original size, to a
    /// PC and phone to phone alike (spec 2026-11-11 「图片尺寸」).
    #[test]
    fn each_image_of_a_shared_batch_records_its_original_size() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let peer = "04".repeat(32);
        s.set_peers(vec![pc(&peer), phone(&"05".repeat(32))]);
        let files = tempfile::tempdir().unwrap();
        let write = |name: &str| {
            let path = files.path().join(name);
            std::fs::write(&path, name.as_bytes()).unwrap();
            path.to_string_lossy().into_owned()
        };
        let image = |name: &str, mime: &str, dimensions| SendItem::Image {
            path: write(name),
            encoded_size: 0,
            mime: mime.into(),
            staged: false,
            dimensions,
        };
        let batch = |operation: u8| SharePayload {
            sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
            items: vec![
                image(
                    format!("portrait-{operation}.heic").as_str(),
                    "image/heic",
                    Some((3024, 4032)),
                ),
                image(
                    format!("wide-{operation}.png").as_str(),
                    "image/png",
                    Some((2400, 1080)),
                ),
                image(format!("unknown-{operation}.gif").as_str(), "image/gif", None),
                SendItem::File {
                    path: write(format!("notes-{operation}.txt").as_str()),
                    name: format!("notes-{operation}.txt"),
                    size: 0,
                    mime: None,
                    staged: false,
                },
            ],
            origin: SendOrigin::TransferPage,
            operation_id: [operation; 16],
            message_id: [operation; 16],
        };
        for (operation, choice) in [
            (21, crate::ProxyChoice::Automatic),
            (
                22,
                crate::ProxyChoice::Direct {
                    device: "05".repeat(32),
                    name: "Tablet".into(),
                },
            ),
        ] {
            let ids = s.send_share(batch(operation), choice).unwrap();
            let sizes: Vec<_> = ids
                .iter()
                .map(|id| s.with_store(|store| store.by_id(*id)).unwrap().unwrap().image_size())
                .collect();
            assert_eq!(sizes, vec![Some((3024, 4032)), Some((2400, 1080)), None, None]);
        }
    }

    /// A PC folder's image rows get thumbnails the way image messages do:
    /// the same request on a child record of the folder (created without a
    /// download), the same states and cache, and the listing pages carry
    /// each file's child (spec 2026-11-11 「文件夹页」).
    #[test]
    fn a_pc_folder_image_row_gets_a_thumbnail_on_its_child_record() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        let peer = hex(&source.device.0);
        s.set_peers(vec![pc(&peer)]);
        let folder = s
            .with_store(|store| {
                store.insert_announcement(
                    1,
                    IncomingAnnouncement {
                        sender_id: peer.clone(),
                        proxy_id: None,
                        content_id: format!("v2:{peer}:9:1:f:1:1"),
                        kind: AttachmentKind::Folder,
                        display_name: "Trip".into(),
                        mime: None,
                        size: 0,
                        text: None,
                        rich_text_format: None,
                        source: SourceState {
                            source_id: peer.clone(),
                            availability: crate::SourceAvailability::Ready,
                        },
                        operation_id: [8; 16],
                        message_id: [9; 16],
                        item_id: 0,
                        representation_id: 0,
                        sensitive: false,
                    },
                )
            })
            .unwrap();
        let offer = parse_content_id(&folder.content_id).unwrap();
        let node = |id: u64, name: &str, kind: dto::NodeKind| {
            dto::FileNode::from_raw(
                [
                    (1, id.write()),
                    (2, 1u64.write()),
                    (3, RawValue::from(name)),
                    (4, kind.write()),
                    (
                        5,
                        RawValue::Map([(1, dto::ContentSizeKind::UnknownSize.write())].into()),
                    ),
                    (6, 3u64.write()),
                ]
                .into(),
            )
            .unwrap()
        };
        let list = |operation: u8| -> Vec<DirectoryEntry> {
            s.list_directory([operation; 16], &folder.content_id, 0, None).unwrap();
            let read = link.low.lock().unwrap().last().unwrap().read.clone().unwrap();
            assert!(matches!(read, v2::Message::ContentListDirectory(_)));
            let page = dto::DirectoryPage::from_raw(
                [
                    (1, 1u64.write()),
                    (2, 3u64.write()),
                    (
                        3,
                        RawValue::Array(vec![
                            node(5, "beach.jpg", dto::NodeKind::File).write(),
                            node(6, "notes.txt", dto::NodeKind::File).write(),
                            node(7, "day 2", dto::NodeKind::Directory).write(),
                        ]),
                    ),
                ]
                .into(),
            )
            .unwrap();
            let directory = dto::ContentDirectory::from_raw([(1, offer.write()), (2, page.write())].into()).unwrap();
            assert!(
                s.receive_low_reply([operation; 16], &read, None, v2::Message::ContentDirectory(directory))
                    .unwrap()
            );
            s.finish_low(&peer, [operation; 16], false, None, None);
            s.take_events()
                .into_iter()
                .find_map(|event| match event {
                    TransferEvent::DirectoryPage {
                        operation_id,
                        folder_message_id,
                        entries,
                        ..
                    } if operation_id == [operation; 16] => {
                        assert_eq!(folder_message_id, folder.id);
                        Some(entries)
                    }
                    _ => None,
                })
                .expect("a listing page")
        };
        let entries = list(1);
        assert_eq!(entries.len(), 3);
        assert!(
            entries.iter().all(|entry| entry.child.is_none()),
            "nothing recorded yet"
        );
        let (beach, notes, day) = (entries[0].clone(), entries[1].clone(), entries[2].clone());

        // Only image files get a thumbnail from the core.
        let requests = link.low.lock().unwrap().len();
        assert_eq!(s.request_directory_thumbnail(folder.id, notes.clone()).unwrap(), None);
        assert_eq!(s.request_directory_thumbnail(folder.id, day).unwrap(), None);
        assert_eq!(link.low.lock().unwrap().len(), requests);

        // The visible image row asks the PC for the file's preview.
        let child = s
            .request_directory_thumbnail(folder.id, beach.clone())
            .unwrap()
            .unwrap();
        let (operation, read) = {
            let low = link.low.lock().unwrap();
            assert_eq!(low.len(), requests + 1);
            let last = low.last().unwrap();
            (last.operation_id, last.read.clone().unwrap())
        };
        let v2::Message::ContentThumbnailRead(request) = &read else {
            panic!("expected a thumbnail read, got {read:?}");
        };
        assert_eq!(
            (request.node().unwrap(), request.version().unwrap()),
            (Some(5), Some(3))
        );
        assert_eq!(request.offer().unwrap(), offer);
        let record = s.with_store(|store| store.by_id(child)).unwrap().unwrap();
        assert_eq!(
            (record.parent_id, record.directory_node(), record.kind),
            (Some(folder.id), Some(5), AttachmentKind::Image)
        );
        assert_eq!(record.download, crate::DownloadState::Placeholder);
        assert_eq!(record.download_status(), Some(crate::DownloadStatus::NotDownloaded));
        assert_eq!(record.image_preview(), Some(crate::ImagePreview::Loading));
        // One request at a time, and the same child for the same row.
        assert_eq!(
            s.request_directory_thumbnail(folder.id, beach.clone()).unwrap(),
            Some(child)
        );
        assert_eq!(link.low.lock().unwrap().len(), requests + 1);
        s.receive_low_reply(
            operation,
            &read,
            None,
            thumbnail_reply(operation, dto::ThumbnailStatus::Ready),
        )
        .unwrap();
        s.finish_low(&peer, operation, false, None, None);
        let record = s.with_store(|store| store.by_id(child)).unwrap().unwrap();
        assert_eq!(record.image_preview(), Some(crate::ImagePreview::Thumbnail));
        assert_eq!(record.image_size(), Some((512, 288)));
        assert_eq!(
            fs::read(record.thumbnail_path.as_deref().unwrap()).unwrap(),
            [0xff, 0xd8, 0xff]
        );
        assert!(s.take_events().iter().any(|event| matches!(event,
            TransferEvent::Announcement { record_id, parent_id: Some(parent), .. }
                if *record_id == child && *parent == folder.id)));
        // A ready thumbnail is final, for the row as for a message.
        assert_eq!(
            s.request_directory_thumbnail(folder.id, beach.clone()).unwrap(),
            Some(child)
        );
        assert!(!s.request_thumbnail(child).unwrap());
        assert_eq!(link.low.lock().unwrap().len(), requests + 1);
        // It stays a folder child, out of the conversation.
        let page = s.with_store(|store| store.page(None, 10)).unwrap();
        assert_eq!(
            page.messages.iter().map(|message| message.id).collect::<Vec<_>>(),
            vec![folder.id]
        );

        // The next listing carries the row's child with its thumbnail.
        let entries = list(2);
        let listed = entries[0].child.as_ref().expect("the image row's child");
        assert_eq!(listed.id, child);
        assert_eq!(listed.thumbnail_path, record.thumbnail_path);
        assert!(entries[1].child.is_none() && entries[2].child.is_none());
        // Downloading the file later uses the same record.
        assert_eq!(
            s.request_directory_file([3; 16], folder.id, beach, false).unwrap(),
            child
        );
    }

    #[test]
    fn a_proxy_echo_of_an_own_share_is_not_stored_again() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link);
        let proxy = DeviceId([4; 32]);
        let proxy_id = hex(&proxy.0);
        s.set_peers(vec![pc(&proxy_id)]);
        let long = "x".repeat(70 * 1024);
        let ids = s
            .send_share(
                SharePayload {
                    sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
                    items: vec![SendItem::Text { text: long.clone() }],
                    origin: SendOrigin::TransferPage,
                    operation_id: [6; 16],
                    message_id: [7; 16],
                },
                crate::ProxyChoice::Automatic,
            )
            .unwrap();
        let id = offer_id(proxy, [9; 16], 0);
        let inventory = dto::Inventory::from_raw(
            [
                (1, id.write()),
                (2, RawValue::from("macos")),
                (3, RawValue::Array(Vec::new())),
                (4, RawValue::Array(Vec::new())),
            ]
            .into(),
        )
        .unwrap();
        let echo = dto::ClipboardOffer::from_raw(
            [
                (1, id.write()),
                (2, 1u64.write()),
                (3, 1u64.write()),
                (4, dto::Sensitivity::Normal.write()),
                (6, inventory.write()),
                (7, 0u64.write()),
                (8, device(DeviceId([1; 32])).write()),
                (9, device(proxy).write()),
                (10, id16([6; 16]).write()),
                (11, id16([7; 16]).write()),
                (12, 1u32.write()),
                (13, oink_protocol::v2::RawValue::Array(vec![])),
            ]
            .into(),
        )
        .unwrap();
        assert_eq!(s.receive_offer(&proxy_id, &echo, true).unwrap(), Some(ids[0]));
        assert_eq!(
            s.coordinator
                .store
                .lock()
                .unwrap()
                .page(None, 10)
                .unwrap()
                .messages
                .len(),
            1
        );
    }

    #[test]
    fn a_failed_download_is_retryable_and_progress_is_keyed_by_record() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        let peer = hex(&source.device.0);
        s.set_peers(vec![pc(&peer)]);
        let clip = dto::ClipId::from_raw([(1, device(source.device).write()), (2, 9u64.write())].into()).unwrap();
        let offer_id = dto::OfferId::from_raw([(1, clip.write()), (2, 1u64.write())].into()).unwrap();
        let offer = |item: u32| {
            dto::ContentOffer::from_raw(
                [
                    (1, offer_id.write()),
                    (2, id16([8; 16]).write()),
                    (3, id16([9; 16]).write()),
                    (4, item.write()),
                    (5, 0u32.write()),
                    (6, boot(source.boot).write()),
                    (7, device(source.device).write()),
                    (9, dto::ContentKind::Image.write()),
                    (10, RawValue::Text("image/png".into())),
                    (12, 3u64.write()),
                    (13, dto::ContentAvailability::Ready.write()),
                    (14, 10u64.write()),
                ]
                .into(),
            )
            .unwrap()
        };
        for item in [1, 2] {
            s.receive_message(&peer, v2::Message::ContentOffer(offer(item)))
                .unwrap();
        }
        let page = s.coordinator.store.lock().unwrap().page(None, 10).unwrap();
        let second = page.messages[0].id;
        assert_eq!(page.messages[0].display_name, "shared item");
        s.request_download(second, true).unwrap();
        let read = link.low.lock().unwrap()[0].read.clone().unwrap();
        let attempt = s.download_attempt(&read).unwrap();
        s.finish_low(
            &peer,
            [8; 16],
            false,
            attempt,
            Some(MobileV2TransportError::Link("no content progress for 300 s".into())),
        );
        let failed = s.coordinator.store.lock().unwrap().by_id(second).unwrap().unwrap();
        assert_eq!(failed.download, crate::DownloadState::Failed);
        assert!(s.take_events().iter().any(|event| matches!(event,
            TransferEvent::Error { record_id: Some(id), .. } if *id == second)));
        assert!(s.take_ready_auto_open().unwrap().is_none());

        s.request_download(second, true).unwrap();
        let read = link.low.lock().unwrap()[1].read.clone().unwrap();
        let attempt = s.download_attempt(&read).unwrap();
        let bytes = dto::ContentRepresentationBytes::from_raw(
            [
                (1, offer_id.write()),
                (2, 0u32.write()),
                (3, 0u64.write()),
                (4, RawValue::Bytes(b"png".to_vec())),
            ]
            .into(),
        )
        .unwrap();
        s.receive_low_reply([8; 16], &read, attempt, v2::Message::ContentRepresentationBytes(bytes))
            .unwrap();
        let end = dto::ContentRepresentationEnd::from_raw(
            [(1, offer_id.write()), (2, 0u32.write()), (3, 3u64.write())].into(),
        )
        .unwrap();
        s.receive_low_reply([8; 16], &read, attempt, v2::Message::ContentRepresentationEnd(end))
            .unwrap();
        let events = s.take_events();
        assert!(events.iter().all(|event| !matches!(event,
            TransferEvent::DownloadProgress { record_id, .. } if *record_id != second)));
        assert!(events.iter().any(|event| matches!(event,
            TransferEvent::Announcement { record_id, auto_open_ready: true, .. } if *record_id == second)));
        let open = s.take_ready_auto_open().unwrap().unwrap();
        assert_eq!(open.id, second);
        assert!(open.local_path.unwrap().ends_with("shared item.png"));
    }

    /// A received bitmap message of `size` bytes from PC `source`.
    fn image_offer(source: &MobileV2Identity, seq: u64, size: Option<u64>) -> (dto::OfferId, dto::ContentOffer) {
        let clip = dto::ClipId::from_raw([(1, device(source.device).write()), (2, seq.write())].into()).unwrap();
        let offer_id = dto::OfferId::from_raw([(1, clip.write()), (2, 1u64.write())].into()).unwrap();
        let mut raw = RawMap::from([
            (1, offer_id.write()),
            (2, id16([8; 16]).write()),
            (3, id16([9; 16]).write()),
            (4, 1u32.write()),
            (5, 2u32.write()),
            (6, boot(source.boot).write()),
            (7, device(source.device).write()),
            (9, dto::ContentKind::Image.write()),
            (10, RawValue::Text("image/png".into())),
            (13, dto::ContentAvailability::Ready.write()),
            (14, 10u64.write()),
        ]);
        if let Some(size) = size {
            raw.insert(12, size.write());
        }
        (offer_id, dto::ContentOffer::from_raw(raw).unwrap())
    }

    fn thumbnail_reply(operation: [u8; 16], status: dto::ThumbnailStatus) -> v2::Message {
        let mut raw = RawMap::from([(1, id16(operation).write()), (2, status.write())]);
        if status == dto::ThumbnailStatus::Ready {
            raw.insert(3, RawValue::Text("image/jpeg".into()));
            raw.insert(4, 512u32.write());
            raw.insert(5, 288u32.write());
            raw.insert(6, RawValue::Bytes(vec![0xff, 0xd8, 0xff]));
            raw.insert(7, 6_000_000u64.write());
        }
        v2::Message::ContentThumbnail(dto::ContentThumbnail::from_raw(raw).unwrap())
    }

    #[test]
    fn every_image_gets_only_a_thumbnail_and_the_original_on_open() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        let peer = hex(&source.device.0);
        s.set_peers(vec![pc(&peer)]);
        // User decision 2026-11-10: no original is downloaded on arrival,
        // large, small or of unknown size; nothing is requested either until
        // the message becomes visible.
        let (_, big) = image_offer(&source, 9, Some(5 * 1024 * 1024));
        s.receive_message(&peer, v2::Message::ContentOffer(big)).unwrap();
        let (_, small) = image_offer(&source, 10, Some(1024));
        s.receive_message(&peer, v2::Message::ContentOffer(small)).unwrap();
        let (_, unknown) = image_offer(&source, 11, None);
        s.receive_message(&peer, v2::Message::ContentOffer(unknown)).unwrap();
        assert!(link.low.lock().unwrap().is_empty(), "nothing is fetched on arrival");
        let page = s.with_store(|store| store.page(None, 10)).unwrap();
        let (unknown, small, big) = (page.messages[0].id, page.messages[1].id, page.messages[2].id);
        for id in [unknown, small, big] {
            let record = s.with_store(|store| store.by_id(id)).unwrap().unwrap();
            assert_eq!(record.download, crate::DownloadState::Placeholder);
        }
        // Visible: each asks for a thumbnail, once.
        assert!(s.request_thumbnail(small).unwrap());
        assert!(s.request_thumbnail(unknown).unwrap());
        assert!(s.request_thumbnail(big).unwrap());
        assert_eq!(link.low.lock().unwrap().len(), 3);
        assert!(
            link.low
                .lock()
                .unwrap()
                .iter()
                .all(|operation| matches!(operation.read, Some(v2::Message::ContentThumbnailRead(_))))
        );
        assert!(!s.request_thumbnail(big).unwrap(), "one request at a time");
        let (operation, read) = {
            let low = link.low.lock().unwrap();
            let operation = low.last().unwrap();
            (operation.operation_id, operation.read.clone().unwrap())
        };
        let v2::Message::ContentThumbnailRead(request) = &read else {
            panic!("expected a thumbnail read, got {read:?}");
        };
        assert_eq!(
            (request.item_id().unwrap(), request.representation_id().unwrap()),
            (Some(1), Some(2))
        );
        assert_eq!(
            (request.max_edge().unwrap(), request.max_bytes().unwrap()),
            (THUMBNAIL_EDGE, THUMBNAIL_BYTES)
        );
        let record = s.with_store(|store| store.by_id(big)).unwrap().unwrap();
        assert_eq!(record.image_preview(), Some(crate::ImagePreview::Loading));
        assert!(s.download_attempt(&read).unwrap().is_none(), "not a download");
        assert!(
            s.receive_low_reply(
                operation,
                &read,
                None,
                thumbnail_reply(operation, dto::ThumbnailStatus::Ready)
            )
            .unwrap()
        );
        s.finish_low(&peer, operation, false, None, None);
        let record = s.with_store(|store| store.by_id(big)).unwrap().unwrap();
        assert_eq!(record.image_preview(), Some(crate::ImagePreview::Thumbnail));
        assert_eq!(
            (record.thumbnail_width, record.thumbnail_height),
            (Some(512), Some(288))
        );
        assert_eq!(
            fs::read(record.thumbnail_path.as_deref().unwrap()).unwrap(),
            [0xff, 0xd8, 0xff]
        );
        assert!(s.take_events().iter().any(|event| matches!(event,
            TransferEvent::Announcement { record_id, .. } if *record_id == big)));
        assert!(!s.request_thumbnail(big).unwrap(), "a ready thumbnail is final");
        // Opening the image downloads the original; the thumbnail stays meanwhile.
        s.request_download(big, true).unwrap();
        let download = link.low.lock().unwrap().last().unwrap().read.clone().unwrap();
        assert!(matches!(download, v2::Message::ClipboardContentRead(_)));
        let record = s.with_store(|store| store.by_id(big)).unwrap().unwrap();
        assert_eq!(record.download, crate::DownloadState::Downloading);
        assert_eq!(record.image_preview(), Some(crate::ImagePreview::Thumbnail));
    }

    /// Mac/Windows offers announce bitmaps without a size: the thumbnail
    /// answer records the original's real size, and still nothing more is
    /// downloaded until the user opens the image.
    #[test]
    fn a_thumbnail_records_the_original_size_and_never_starts_the_download() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        let peer = hex(&source.device.0);
        s.set_peers(vec![pc(&peer)]);
        let answer = |seq: u64, size: u64| {
            let (_, offer) = image_offer(&source, seq, None);
            s.receive_message(&peer, v2::Message::ContentOffer(offer)).unwrap();
            let id = s.with_store(|store| store.page(None, 10)).unwrap().messages[0].id;
            assert!(s.request_thumbnail(id).unwrap(), "requested when visible");
            let (operation, read) = {
                let low = link.low.lock().unwrap();
                let last = low.last().unwrap();
                (last.operation_id, last.read.clone().unwrap())
            };
            assert!(matches!(read, v2::Message::ContentThumbnailRead(_)));
            let mut raw = RawMap::from([
                (1, id16(operation).write()),
                (2, dto::ThumbnailStatus::Ready.write()),
                (3, RawValue::Text("image/jpeg".into())),
                (4, 512u32.write()),
                (5, 288u32.write()),
                (6, RawValue::Bytes(vec![0xff, 0xd8, 0xff])),
            ]);
            raw.insert(7, size.write());
            let reply = v2::Message::ContentThumbnail(dto::ContentThumbnail::from_raw(raw).unwrap());
            s.receive_low_reply(operation, &read, None, reply).unwrap();
            s.finish_low(&peer, operation, false, None, None);
            id
        };
        for (seq, size) in [(9, 1024), (10, 6 * 1024 * 1024)] {
            let id = answer(seq, size);
            let record = s.with_store(|store| store.by_id(id)).unwrap().unwrap();
            assert_eq!(record.size, size);
            assert_eq!(record.download, crate::DownloadState::Placeholder);
            assert_eq!(record.image_preview(), Some(crate::ImagePreview::Thumbnail));
            assert!(matches!(
                link.low.lock().unwrap().last().unwrap().read,
                Some(v2::Message::ContentThumbnailRead(_))
            ));
        }
        assert!(s.take_ready_auto_open().unwrap().is_none(), "never opened by itself");
    }

    #[test]
    fn a_missing_original_is_expired_and_a_lost_link_is_retried() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        let peer = hex(&source.device.0);
        s.set_peers(vec![pc(&peer)]);
        let (_, unknown) = image_offer(&source, 9, None);
        s.receive_message(&peer, v2::Message::ContentOffer(unknown)).unwrap();
        let id = s.with_store(|store| store.page(None, 10)).unwrap().messages[0].id;
        assert!(s.request_thumbnail(id).unwrap());
        assert!(!s.request_thumbnail(id).unwrap(), "already requested");
        let operation = link.low.lock().unwrap().last().unwrap().operation_id;
        s.finish_low(
            &peer,
            operation,
            false,
            None,
            Some(MobileV2TransportError::Link("link lost".into())),
        );
        let record = s.with_store(|store| store.by_id(id)).unwrap().unwrap();
        assert_eq!(record.thumbnail, crate::ThumbnailState::Failed);
        assert_eq!(record.image_preview(), Some(crate::ImagePreview::Failed));

        assert!(s.request_thumbnail(id).unwrap(), "a failed request is retried");
        let (operation, read) = {
            let low = link.low.lock().unwrap();
            let last = low.last().unwrap();
            (last.operation_id, last.read.clone().unwrap())
        };
        s.receive_low_reply(
            operation,
            &read,
            None,
            thumbnail_reply(operation, dto::ThumbnailStatus::Expired),
        )
        .unwrap();
        s.finish_low(&peer, operation, false, None, None);
        let record = s.with_store(|store| store.by_id(id)).unwrap().unwrap();
        assert_eq!(record.image_preview(), Some(crate::ImagePreview::Expired));
        assert_eq!(record.source_availability, crate::SourceAvailability::NoSuchItem);
        assert!(!s.request_thumbnail(id).unwrap());
        assert!(s.request_download(id, true).is_err(), "nothing to download");

        // An offline source is not asked; the bubble shows it as offline.
        let (_, other) = image_offer(&source, 11, None);
        s.receive_message(&peer, v2::Message::ContentOffer(other)).unwrap();
        let other = s.with_store(|store| store.page(None, 10)).unwrap().messages[0].id;
        s.peer_lost(&peer);
        s.set_peers(Vec::new());
        assert!(!s.request_thumbnail(other).unwrap());
        assert_eq!(
            s.with_store(|store| store.by_id(other))
                .unwrap()
                .unwrap()
                .image_preview(),
            Some(crate::ImagePreview::Offline)
        );
    }

    #[test]
    fn a_full_download_of_a_gone_image_expires_it() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        let peer = hex(&source.device.0);
        s.set_peers(vec![pc(&peer)]);
        let (offer_id, big) = image_offer(&source, 9, Some(5 * 1024 * 1024));
        s.receive_message(&peer, v2::Message::ContentOffer(big)).unwrap();
        let id = s.with_store(|store| store.page(None, 10)).unwrap().messages[0].id;
        s.request_download(id, true).unwrap();
        let read = link.low.lock().unwrap().last().unwrap().read.clone().unwrap();
        let attempt = s.download_attempt(&read).unwrap();
        let failed = dto::ContentRepresentationFailed::from_raw(
            [
                (1, offer_id.write()),
                (2, 2u32.write()),
                (3, dto::ContentUnavailable::NoSuchItem.write()),
            ]
            .into(),
        )
        .unwrap();
        let error = s
            .receive_low_reply(
                [8; 16],
                &read,
                attempt,
                v2::Message::ContentRepresentationFailed(failed),
            )
            .unwrap_err();
        s.finish_low(&peer, [8; 16], false, attempt, Some(error));
        let record = s.with_store(|store| store.by_id(id)).unwrap().unwrap();
        assert_eq!(record.image_preview(), Some(crate::ImagePreview::Expired));
    }

    /// A PC that paused clipboard sharing or stopped sending answers a read
    /// with that reason; the download ends with it, not as a lost link.
    #[test]
    fn a_read_refused_as_paused_or_send_off_reports_the_reason() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        let peer = hex(&source.device.0);
        s.set_peers(vec![pc(&peer)]);
        for (seq, reason, expected) in [
            (
                9,
                dto::ContentUnavailable::Paused,
                MobileV2TransportError::PeerPaused(peer.clone()),
            ),
            (
                10,
                dto::ContentUnavailable::SendDisabled,
                MobileV2TransportError::SendDisabled,
            ),
        ] {
            let (offer_id, offer) = image_offer(&source, seq, Some(5 * 1024 * 1024));
            s.receive_message(&peer, v2::Message::ContentOffer(offer)).unwrap();
            let id = s.with_store(|store| store.page(None, 10)).unwrap().messages[0].id;
            s.request_download(id, false).unwrap();
            let read = link.low.lock().unwrap().last().unwrap().read.clone().unwrap();
            let attempt = s.download_attempt(&read).unwrap();
            let failed = dto::ContentRepresentationFailed::from_raw(
                [(1, offer_id.write()), (2, 2u32.write()), (3, reason.write())].into(),
            )
            .unwrap();
            let error = s
                .receive_low_reply(
                    [8; 16],
                    &read,
                    attempt,
                    v2::Message::ContentRepresentationFailed(failed),
                )
                .unwrap_err();
            assert_eq!(error, expected);
        }
    }

    #[test]
    fn deleting_through_the_session_store_stops_the_live_download() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        let peer = hex(&source.device.0);
        s.set_peers(vec![pc(&peer)]);
        let clip = dto::ClipId::from_raw([(1, device(source.device).write()), (2, 9u64.write())].into()).unwrap();
        let offer_id = dto::OfferId::from_raw([(1, clip.write()), (2, 1u64.write())].into()).unwrap();
        let offer = dto::ContentOffer::from_raw(
            [
                (1, offer_id.write()),
                (2, id16([8; 16]).write()),
                (3, id16([9; 16]).write()),
                (4, 1u32.write()),
                (5, 0u32.write()),
                (6, boot(source.boot).write()),
                (7, device(source.device).write()),
                (9, dto::ContentKind::Image.write()),
                (10, RawValue::Text("image/png".into())),
                (12, 3u64.write()),
                (13, dto::ContentAvailability::Ready.write()),
                (14, 10u64.write()),
            ]
            .into(),
        )
        .unwrap();
        s.receive_message(&peer, v2::Message::ContentOffer(offer)).unwrap();
        let id = s.with_store(|store| store.page(None, 10)).unwrap().messages[0].id;
        s.request_download(id, true).unwrap();
        let read = link.low.lock().unwrap()[0].read.clone().unwrap();
        let attempt = s.download_attempt(&read).unwrap();

        assert!(s.with_store(|store| store.attempt(id)).is_some());
        s.with_store(|store| store.delete_message(id)).unwrap();
        assert!(s.with_store(|store| store.attempt(id)).is_none());
        s.take_events();
        let bytes = dto::ContentRepresentationBytes::from_raw(
            [
                (1, offer_id.write()),
                (2, 0u32.write()),
                (3, 0u64.write()),
                (4, RawValue::Bytes(b"png".to_vec())),
            ]
            .into(),
        )
        .unwrap();
        let _ = s.receive_low_reply([8; 16], &read, attempt, v2::Message::ContentRepresentationBytes(bytes));
        assert!(s.with_store(|store| store.by_id(id)).unwrap().is_none());
        assert!(!s.take_events().iter().any(|event| matches!(event,
            TransferEvent::Announcement { record_id, .. } if *record_id == id)));
        assert!(s.take_ready_auto_open().unwrap().is_none());
    }

    /// A one-item text ClipboardOffer from `source` with the given sensitivity.
    pub(crate) fn text_offer(
        source: &MobileV2Identity,
        seq: u64,
        sensitivity: dto::Sensitivity,
    ) -> dto::ClipboardOffer {
        let clip = dto::ClipId::from_raw([(1, device(source.device).write()), (2, seq.write())].into()).unwrap();
        let id = dto::OfferId::from_raw([(1, clip.write()), (2, seq.write())].into()).unwrap();
        let format = RawValue::from("mime:text/plain;charset=utf-8");
        let representation = dto::Representation::from_raw(
            [
                (1, 2u32.write()),
                (2, format.clone()),
                (3, dto::Medium::Bytes.write()),
                (4, 0u32.write()),
                (5, 0i32.write()),
                (6, format),
                (
                    7,
                    RawValue::Map(
                        [
                            (1, dto::RepresentationDataKind::Inline.write()),
                            (2, RawValue::Map([(1, RawValue::Bytes(b"hunter2".to_vec()))].into())),
                        ]
                        .into(),
                    ),
                ),
            ]
            .into(),
        )
        .unwrap();
        let item = dto::ContentItem::from_raw(
            [
                (1, 1u32.write()),
                (3, RawValue::Array(vec![representation.write()])),
                (4, false.write()),
            ]
            .into(),
        )
        .unwrap();
        let inventory = dto::Inventory::from_raw(
            [
                (1, id.write()),
                (2, RawValue::from("macos")),
                (3, RawValue::Array(vec![item.write()])),
                (4, RawValue::Array(Vec::new())),
            ]
            .into(),
        )
        .unwrap();
        dto::ClipboardOffer::from_raw(
            [
                (1, id.write()),
                (2, seq.write()),
                (3, seq.write()),
                (4, sensitivity.write()),
                (6, inventory.write()),
                (7, 0u64.write()),
                (8, device(source.device).write()),
                (10, id16([seq as u8; 16]).write()),
                (11, id16([seq as u8 + 1; 16]).write()),
                (12, 1u32.write()),
                (13, oink_protocol::v2::RawValue::Array(vec![])),
            ]
            .into(),
        )
        .unwrap()
    }

    /// A PC copy marked as a secret (Concealed) is recorded as sensitive,
    /// is announced as sensitive, is not downloaded automatically, and is
    /// completely removed when an ordinary copy supersedes it.
    #[test]
    fn concealed_clipboard_head_is_sensitive_and_is_erased_when_superseded() {
        let link = Arc::new(FakeLink::default());
        let (dir, session) = session(link.clone());
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        let peer = hex(&source.device.0);
        session.set_peers(vec![pc(&peer)]);
        session
            .receive_message(
                &peer,
                v2::Message::ClipboardOffer(text_offer(&source, 4, dto::Sensitivity::Concealed)),
            )
            .unwrap();
        assert!(
            link.low.lock().unwrap().is_empty(),
            "a secret is not fetched automatically"
        );
        assert!(
            session
                .take_events()
                .iter()
                .any(|event| matches!(event, TransferEvent::Announcement { sensitive: true, .. }))
        );
        session
            .receive_message(
                &peer,
                v2::Message::ClipboardOffer(text_offer(&source, 5, dto::Sensitivity::Normal)),
            )
            .unwrap();
        assert_eq!(link.low.lock().unwrap().len(), 1, "ordinary text is fetched as before");
        // A delayed old secret cannot return after the newer head erased it, even in background.
        session
            .receive_message(
                &peer,
                v2::Message::ClipboardOffer(text_offer(&source, 4, dto::Sensitivity::Concealed)),
            )
            .unwrap();
        assert!(
            session
                .take_events()
                .iter()
                .any(|event| matches!(event, TransferEvent::Removed { .. }))
        );
        drop(session);

        let store = TransferStore::open(dir.path().join("db"), dir.path().join("files")).unwrap();
        let page = store.page(None, 10).unwrap().messages;
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].operation_id, [5; 16]);
        assert!(!page[0].sensitive);
    }

    #[test]
    fn folder_proxy_archives_relative_entries_and_cleans_up_staging() {
        let directory = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let subfolder = directory.path().join("subfolder");
        fs::create_dir(&subfolder).unwrap();
        fs::write(subfolder.join("file.txt"), "example").unwrap();
        let (archive, files) = archive_folder(directory.path(), staging.path()).unwrap();
        assert_eq!(files, 1);
        let mut zip = zip::ZipArchive::new(archive.reopen().unwrap()).unwrap();
        let mut body = String::new();
        zip.by_name("subfolder/file.txt")
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "example");
        drop(zip);
        drop(archive);
        assert_eq!(fs::read_dir(staging.path()).unwrap().count(), 0);
    }

    /// Hidden and dot-named folders and files are ordinary names: a shared
    /// folder named `.photos` with a `.config/.env` inside packages as is,
    /// with '/' between components on every platform.
    #[test]
    fn dot_named_folders_and_files_are_archived() {
        let parent = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = parent.path().join(".photos");
        let hidden = root.join(".config");
        fs::create_dir_all(&hidden).unwrap();
        fs::write(hidden.join(".env"), "secret").unwrap();
        fs::write(root.join("..dots.txt"), "dots").unwrap();
        let (archive, files) = archive_folder(&root, staging.path()).unwrap();
        assert_eq!(files, 2);
        let mut zip = zip::ZipArchive::new(archive.reopen().unwrap()).unwrap();
        let mut body = String::new();
        zip.by_name(".config/.env").unwrap().read_to_string(&mut body).unwrap();
        assert_eq!(body, "secret");
        assert!(zip.by_name("..dots.txt").is_ok());
    }

    fn text_share(operation: u8) -> SharePayload {
        SharePayload {
            sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
            items: vec![SendItem::Text { text: "hello".into() }],
            origin: SendOrigin::TransferPage,
            operation_id: [operation; 16],
            message_id: [operation; 16],
        }
    }

    /// H11: right after a session started no target is ready yet. The share
    /// waits for the first one instead of failing at once, and fails with
    /// NoTarget only once the first connect round ended without one.
    #[test]
    fn a_share_waits_for_its_first_target_and_fails_only_after_the_round() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        s.session.set_targets(Vec::new(), false);
        let waiting = s.send_share(text_share(1), crate::ProxyChoice::Automatic).unwrap();
        assert!(link.sent.lock().unwrap().is_empty());
        assert!(deliveries(&s.take_events()).is_empty());
        let peer = "04".repeat(32);
        s.session.set_targets(vec![pc(&peer)], false);
        assert_eq!(
            link.sent.lock().unwrap().len(),
            1,
            "record goes out when its target is ready"
        );
        accept(&s, [1; 16]);
        result(&s, &peer, [1; 16], dto::ShareStatus::Complete);
        assert_eq!(deliveries(&s.take_events()), vec![(waiting, DeliveryState::Sent, None)]);

        s.session.set_targets(Vec::new(), false);
        let unrouted = s.send_share(text_share(2), crate::ProxyChoice::Automatic).unwrap();
        assert!(deliveries(&s.take_events()).is_empty());
        s.session.set_targets(Vec::new(), true);
        assert_eq!(
            deliveries(&s.take_events()),
            vec![(unrouted, DeliveryState::Failed, Some(DeliveryFailure::NoTarget))]
        );
    }

    /// User 2026-11-10: there is no 256 MiB cap. A host-staged file of any
    /// size is taken over into the store (moved, not copied a second time:
    /// the stored file is the same inode on the phones' Unix file systems)
    /// and offered with its real size; the host's staged path is gone.
    #[test]
    fn a_large_staged_share_is_taken_over_without_a_cap_or_a_second_copy() {
        let link = Arc::new(FakeLink::default());
        let (dir, s) = session(link.clone());
        s.set_peers(vec![pc(&"04".repeat(32))]);
        // Host staging on the store's volume (share extension / share intent).
        let staging = dir.path().join("host-staging").join("0");
        fs::create_dir_all(&staging).unwrap();
        let movie = staging.join("movie.mov");
        // Sparse: the length is real, no 300 MiB are written.
        let large = 300 * 1024 * 1024;
        fs::File::create(&movie).unwrap().set_len(large).unwrap();
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&movie).unwrap());
        let mut share = text_share(1);
        share.items = vec![SendItem::File {
            path: movie.to_string_lossy().into_owned(),
            name: "movie.mov".into(),
            size: 0,
            mime: None,
            staged: true,
        }];
        let ids = s.send_share(share, crate::ProxyChoice::Automatic).unwrap();
        assert!(!movie.exists(), "the staged file left the host's staging");
        let record = s.coordinator.store.lock().unwrap().by_id(ids[0]).unwrap().unwrap();
        let stored = record.local_path.unwrap();
        assert_eq!(fs::metadata(&stored).unwrap().len(), large);
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&fs::metadata(&stored).unwrap()),
            inode,
            "moved into the store, not copied"
        );
        let frame = link.sent.lock().unwrap()[0].1.clone();
        let v2::Message::ClipboardShareBegin(begin) =
            OpaqueFrame::parse_exact(&frame, MAX_FRAME).unwrap().decode().unwrap()
        else {
            panic!("expected a share begin")
        };
        assert_eq!(begin.bytes().unwrap(), Some(large), "offered with its real size");
        assert!(deliveries(&s.take_events()).is_empty(), "nothing failed");
    }

    fn sent_cancels(link: &FakeLink) -> Vec<(String, [u8; 16])> {
        link.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(
                |(peer, frame)| match OpaqueFrame::parse_exact(frame, MAX_FRAME).ok()?.decode().ok()? {
                    v2::Message::ClipboardShareCancel(cancel) => Some((peer.clone(), cancel.operation_id().ok()?.0)),
                    _ => None,
                },
            )
            .collect()
    }

    /// iOS M31: cancelling a share in flight tells its target, stops the
    /// upload, and ends the own records as Cancelled with the single
    /// DeliveryEvent. The packaged folder in staging goes with the share; a
    /// second cancel and the target's late result change nothing.
    #[test]
    fn cancelling_a_share_in_flight_tells_the_target_and_ends_it_cancelled() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let peer = "04".repeat(32);
        s.set_peers(vec![pc(&peer)]);
        let files = tempfile::tempdir().unwrap();
        let folder = files.path().join("Photos");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("a.txt"), b"a").unwrap();
        let mut share = text_share(5);
        share.items = vec![SendItem::Folder {
            path: folder.to_string_lossy().into_owned(),
            name: "Photos".into(),
        }];
        let ids = s.send_share(share, crate::ProxyChoice::Automatic).unwrap();
        accept(&s, [5; 16]);
        assert_eq!(link.low.lock().unwrap().len(), 1, "the upload started");
        assert!(deliveries(&s.take_events()).is_empty());

        s.cancel_share([5; 16]).unwrap();
        assert_eq!(sent_cancels(&link), vec![(peer.clone(), [5; 16])]);
        let upload = link.low.lock().unwrap().pop().unwrap();
        assert!(*upload.cancel.as_ref().unwrap().borrow(), "the upload was told to stop");
        let events = s.take_events();
        assert_eq!(
            deliveries(&events),
            vec![(ids.clone(), DeliveryState::Failed, Some(DeliveryFailure::Cancelled))]
        );
        // A cancelled own share leaves the history (2026-11-11 review).
        assert!(events.contains(&TransferEvent::Removed {
            record_ids: ids.clone()
        }));
        assert!(s.coordinator.store.lock().unwrap().by_id(ids[0]).unwrap().is_none());
        // The upload task drops its operation when it stops; the archive goes.
        drop(upload);
        let staging = s.coordinator.store.lock().unwrap().staging_dir();
        assert_eq!(fs::read_dir(staging).unwrap().count(), 0, "no packaged folder left");

        s.cancel_share([5; 16]).unwrap();
        result(&s, &peer, [5; 16], dto::ShareStatus::Complete);
        assert!(deliveries(&s.take_events()).is_empty(), "nothing after the cancel");
        assert_eq!(sent_cancels(&link).len(), 1);
    }

    /// A share cancelled before `send_share` recorded it (the hand-over
    /// request was cancelled at once) records nothing; a finished share is
    /// not touched by a cancel.
    #[test]
    fn a_share_cancelled_before_it_is_recorded_records_nothing_and_finished_ones_stay() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let peer = "04".repeat(32);
        s.set_peers(vec![pc(&peer)]);
        s.cancel_share([6; 16]).unwrap();
        assert_eq!(
            s.send_share(text_share(6), crate::ProxyChoice::Automatic),
            Err(MobileV2TransportError::Cancelled)
        );
        assert!(
            s.coordinator
                .store
                .lock()
                .unwrap()
                .page(None, 10)
                .unwrap()
                .messages
                .is_empty()
        );
        assert!(link.sent.lock().unwrap().is_empty());

        let ids = s.send_share(text_share(7), crate::ProxyChoice::Automatic).unwrap();
        accept(&s, [7; 16]);
        result(&s, &peer, [7; 16], dto::ShareStatus::Complete);
        assert_eq!(
            deliveries(&s.take_events()),
            vec![(ids.clone(), DeliveryState::Sent, None)]
        );
        s.cancel_share([7; 16]).unwrap();
        assert!(deliveries(&s.take_events()).is_empty());
        assert!(sent_cancels(&link).is_empty());
        let record = s.coordinator.store.lock().unwrap().by_id(ids[0]).unwrap().unwrap();
        assert_eq!(record.delivery, Some(DeliveryState::Sent));
    }

    /// A share still waiting for its first target ends as Cancelled at once.
    #[test]
    fn a_waiting_share_ends_cancelled() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        s.session.set_targets(Vec::new(), false);
        let ids = s.send_share(text_share(8), crate::ProxyChoice::Automatic).unwrap();
        s.cancel_share([8; 16]).unwrap();
        assert_eq!(
            deliveries(&s.take_events()),
            vec![(ids, DeliveryState::Failed, Some(DeliveryFailure::Cancelled))]
        );
        s.session.set_targets(vec![pc(&"04".repeat(32))], true);
        assert!(
            link.sent.lock().unwrap().is_empty(),
            "the cancelled share never goes out"
        );
    }

    fn phone(id: &str) -> Peer {
        Peer {
            kind: PeerKind::Mobile,
            ..pc(id)
        }
    }

    /// A file for the group's only other device, a phone: sent with
    /// `send_share_when_online`.
    fn file_for_phone(dir: &Path, operation: u8, phone: &str) -> (SharePayload, crate::ProxyChoice) {
        let path = dir.join(format!("file-{operation}.bin"));
        fs::write(&path, b"to the phone").unwrap();
        (
            SharePayload {
                sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
                items: vec![SendItem::File {
                    path: path.to_string_lossy().into_owned(),
                    name: "file.bin".into(),
                    size: 12,
                    mime: None,
                    staged: false,
                }],
                origin: SendOrigin::AndroidShare,
                operation_id: [operation; 16],
                message_id: [operation; 16],
            },
            crate::ProxyChoice::Direct {
                device: phone.into(),
                name: "iPhone".into(),
            },
        )
    }

    fn waiting_events(events: &[TransferEvent]) -> Vec<([u8; 16], bool)> {
        events
            .iter()
            .filter_map(|event| match event {
                TransferEvent::ShareWaiting { operation_id, waiting } => Some((*operation_id, *waiting)),
                _ => None,
            })
            .collect()
    }

    async fn until(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition reached");
    }

    fn own_records(s: &Recorded, operation: u8) -> Vec<MessageRecord> {
        s.coordinator
            .store
            .lock()
            .unwrap()
            .outgoing_records([operation; 16])
            .unwrap()
    }

    /// User 2026-11-11: a share to the group's only other device while it
    /// is offline records nothing and says it waits (after the first
    /// connect round); as soon as the device comes online it starts by
    /// itself: `waiting: false` first, then its own records, then the
    /// share goes out to that device.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_share_waits_unrecorded_for_its_offline_target_and_starts_when_it_comes_online() {
        let link = Arc::new(FakeLink::default());
        let (dir, s) = session(link.clone());
        let iphone = "05".repeat(32);
        s.session.set_targets(Vec::new(), false);
        let (payload, choice) = file_for_phone(dir.path(), 9, &iphone);
        let session = s.session.clone();
        let share = tokio::spawn(async move { session.send_share_when_online(payload, choice).await });
        until(|| s.target_waits.lock().unwrap().contains_key(&[9; 16])).await;
        // Still searching: no 「对方不在线」 yet, and nothing recorded or sent.
        assert!(waiting_events(&s.take_events()).is_empty());
        s.session.set_targets(Vec::new(), true);
        assert_eq!(waiting_events(&s.take_events()), vec![([9; 16], true)]);
        assert!(own_records(&s, 9).is_empty());
        assert!(link.sent.lock().unwrap().is_empty());
        // Other presence updates while it stays offline say nothing new.
        s.session.set_targets(vec![pc(&"04".repeat(32))], true);
        assert!(waiting_events(&s.take_events()).is_empty());
        assert!(!share.is_finished());

        s.session.set_targets(vec![phone(&iphone)], true);
        let ids = share.await.unwrap().expect("the share started");
        assert_eq!(ids.len(), 1);
        let events = s.take_events();
        assert!(
            matches!(
                events.first(),
                Some(TransferEvent::ShareWaiting {
                    operation_id: [9, ..],
                    waiting: false
                })
            ),
            "the wait ends before the records appear: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TransferEvent::Announcement { record_id, .. } if *record_id == ids[0]))
        );
        let records = own_records(&s, 9);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].direct_peer.as_deref(), Some(iphone.as_str()));
        assert_eq!(records[0].delivery, Some(DeliveryState::Sending));
        let sent = link.sent.lock().unwrap();
        assert!(
            !sent.is_empty() && sent.iter().all(|(peer, _)| *peer == iphone),
            "sent to the phone only"
        );
    }

    /// A share whose target is online goes out at once: no wait, no
    /// `ShareWaiting`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_share_for_an_online_target_does_not_wait() {
        let link = Arc::new(FakeLink::default());
        let (dir, s) = session(link.clone());
        let iphone = "05".repeat(32);
        s.session.set_targets(vec![phone(&iphone)], true);
        let (payload, choice) = file_for_phone(dir.path(), 10, &iphone);
        let ids = s.session.send_share_when_online(payload, choice).await.unwrap();
        assert_eq!(ids.len(), 1);
        assert!(waiting_events(&s.take_events()).is_empty());
        assert!(!link.sent.lock().unwrap().is_empty());
    }

    /// 取消 while the share waits for its offline target: the call ends as
    /// Cancelled, nothing is recorded, no DeliveryEvent, and the target
    /// coming online later sends nothing. The session stopping ends a
    /// waiting share the same way, unrecorded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_share_that_waits_for_its_target_leaves_nothing() {
        let link = Arc::new(FakeLink::default());
        let (dir, s) = session(link.clone());
        let iphone = "05".repeat(32);
        s.session.set_targets(Vec::new(), true);
        let (payload, choice) = file_for_phone(dir.path(), 11, &iphone);
        let session = s.session.clone();
        let share = tokio::spawn(async move { session.send_share_when_online(payload, choice).await });
        until(|| s.target_waits.lock().unwrap().contains_key(&[11; 16])).await;
        assert_eq!(waiting_events(&s.take_events()), vec![([11; 16], true)]);
        s.cancel_share([11; 16]).unwrap();
        assert!(matches!(share.await.unwrap(), Err(MobileV2TransportError::Cancelled)));
        s.session.set_targets(vec![phone(&iphone)], true);
        assert!(
            link.sent.lock().unwrap().is_empty(),
            "the cancelled share never goes out"
        );
        assert!(own_records(&s, 11).is_empty());
        let events = s.take_events();
        assert!(deliveries(&events).is_empty());
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, TransferEvent::Announcement { .. }))
        );

        s.session.set_targets(Vec::new(), true);
        let (payload, choice) = file_for_phone(dir.path(), 12, &iphone);
        let session = s.session.clone();
        let share = tokio::spawn(async move { session.send_share_when_online(payload, choice).await });
        until(|| s.target_waits.lock().unwrap().contains_key(&[12; 16])).await;
        s.stop_target_waits();
        assert!(matches!(share.await.unwrap(), Err(MobileV2TransportError::Link(_))));
        assert!(own_records(&s, 12).is_empty());
    }

    /// A caller that went away (its future dropped) leaves no wait behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_waiting_share_leaves_no_wait_behind() {
        let link = Arc::new(FakeLink::default());
        let (dir, s) = session(link.clone());
        let iphone = "05".repeat(32);
        s.session.set_targets(Vec::new(), true);
        let (payload, choice) = file_for_phone(dir.path(), 13, &iphone);
        let session = s.session.clone();
        let share = tokio::spawn(async move { session.send_share_when_online(payload, choice).await });
        until(|| s.target_waits.lock().unwrap().contains_key(&[13; 16])).await;
        share.abort();
        until(|| s.target_waits.lock().unwrap().is_empty()).await;
        s.session.set_targets(vec![phone(&iphone)], true);
        assert!(link.sent.lock().unwrap().is_empty());
    }

    /// No size cap for folders either: a folder over 256 MiB passes the
    /// metadata check and is recorded (it waits for a target here, so the
    /// test packages nothing).
    #[test]
    fn a_folder_over_256_mib_is_accepted() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        s.session.set_targets(Vec::new(), false);
        let files = tempfile::tempdir().unwrap();
        let folder = files.path().join("folder");
        fs::create_dir(&folder).unwrap();
        for name in ["a.bin", "b.bin"] {
            fs::File::create(folder.join(name))
                .unwrap()
                .set_len(150 * 1024 * 1024)
                .unwrap();
        }
        let mut share = text_share(2);
        share.items = vec![SendItem::Folder {
            path: folder.to_string_lossy().into_owned(),
            name: "folder".into(),
        }];
        let ids = s.send_share(share, crate::ProxyChoice::Automatic).unwrap();
        assert_eq!(ids.len(), 1);
        assert!(deliveries(&s.take_events()).is_empty());
    }

    /// M18: a source that replaced (or no longer has) the content says so,
    /// and the record keeps that reason instead of looking retryable.
    #[test]
    fn content_the_source_replaced_keeps_its_reason_on_the_record() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        let peer = hex(&source.device.0);
        s.set_peers(vec![pc(&peer)]);
        let clip = dto::ClipId::from_raw([(1, device(source.device).write()), (2, 9u64.write())].into()).unwrap();
        let offer_id = dto::OfferId::from_raw([(1, clip.write()), (2, 1u64.write())].into()).unwrap();
        let offer = dto::ContentOffer::from_raw(
            [
                (1, offer_id.write()),
                (2, id16([8; 16]).write()),
                (3, id16([9; 16]).write()),
                (4, 1u32.write()),
                (5, 0u32.write()),
                (6, boot(source.boot).write()),
                (7, device(source.device).write()),
                (9, dto::ContentKind::Image.write()),
                (10, RawValue::Text("image/png".into())),
                (12, 3u64.write()),
                (13, dto::ContentAvailability::Ready.write()),
                (14, 10u64.write()),
            ]
            .into(),
        )
        .unwrap();
        s.receive_message(&peer, v2::Message::ContentOffer(offer)).unwrap();
        let id = s.coordinator.store.lock().unwrap().page(None, 10).unwrap().messages[0].id;
        s.request_download(id, false).unwrap();
        let read = link.low.lock().unwrap()[0].read.clone().unwrap();
        let attempt = s.download_attempt(&read).unwrap();
        let failed = dto::ContentRepresentationFailed::from_raw(
            [
                (1, offer_id.write()),
                (2, 0u32.write()),
                (3, dto::ContentUnavailable::Superseded.write()),
            ]
            .into(),
        )
        .unwrap();
        let error = s
            .receive_low_reply(
                [8; 16],
                &read,
                attempt,
                v2::Message::ContentRepresentationFailed(failed),
            )
            .unwrap_err();
        let superseded = MobileV2TransportError::SourceUnavailable(crate::SourceAvailability::Superseded);
        assert_eq!(error, superseded);
        s.finish_low(&peer, [8; 16], false, attempt, Some(error));
        let record = s.coordinator.store.lock().unwrap().by_id(id).unwrap().unwrap();
        assert_eq!(record.download, crate::DownloadState::Failed);
        assert_eq!(record.source_availability, crate::SourceAvailability::Superseded);
        assert_eq!(s.request_download(id, false), Err(superseded));
    }

    /// M18: an offline or expired current source is its own answer, not an
    /// empty clipboard.
    #[tokio::test]
    async fn an_offline_or_expired_current_source_keeps_its_reason() {
        use crate::SourceAvailability;
        for (status, expected) in [
            (dto::CurrentResultStatus::Empty, Ok(None)),
            (
                dto::CurrentResultStatus::SourceOffline,
                Err(MobileV2TransportError::SourceUnavailable(
                    SourceAvailability::SourceOffline,
                )),
            ),
            (dto::CurrentResultStatus::Expired, Err(MobileV2TransportError::Expired)),
        ] {
            let link = Arc::new(FakeLink::default());
            let (_dir, s) = session(link.clone());
            let source = MobileV2Identity::from_parts([7; 32], [2; 16], [4; 16]);
            let peer = source.device.to_hex();
            s.set_peers(vec![pc(&peer)]);
            let receive = s.receive_current_text(&peer);
            tokio::pin!(receive);
            assert!(
                std::future::poll_fn(|cx| {
                    use std::future::Future;
                    std::task::Poll::Ready(receive.as_mut().poll(cx).is_pending())
                })
                .await
            );
            let frame = link.sent.lock().unwrap()[0].1.clone();
            let v2::Message::ClipboardCurrentQuery(query) =
                OpaqueFrame::parse_exact(&frame, MAX_FRAME).unwrap().decode().unwrap()
            else {
                panic!("expected current query")
            };
            let answer = dto::ClipboardCurrentResult::from_raw(
                [
                    (7, 0u64.write()),
                    (1, query.request_id().unwrap().write()),
                    (2, boot(source.boot).write()),
                    (3, status.write()),
                ]
                .into(),
            )
            .unwrap();
            s.receive_message(&peer, v2::Message::ClipboardCurrentResult(answer))
                .unwrap();
            assert_eq!(receive.await, expected);
        }
    }

    /// M16 / L11: a new session owns the store: unfinished downloads of the
    /// previous process failed, no source is online until its link is ready
    /// again, and staging and unowned attachment directories are gone.
    #[test]
    fn a_new_session_settles_what_the_previous_process_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let open = || TransferStore::open(dir.path().join("db"), dir.path().join("files")).unwrap();
        let mut store = open();
        let source = "07".repeat(32);
        let record = store
            .insert_announcement(
                1,
                IncomingAnnouncement {
                    sender_id: source.clone(),
                    proxy_id: None,
                    content_id: "photo".into(),
                    kind: AttachmentKind::Image,
                    display_name: "photo.png".into(),
                    mime: Some("image/png".into()),
                    size: 10,
                    text: None,
                    rich_text_format: None,
                    source: SourceState {
                        source_id: source.clone(),
                        availability: crate::SourceAvailability::Ready,
                    },
                    operation_id: [1; 16],
                    message_id: [2; 16],
                    item_id: 0,
                    representation_id: 0,
                    sensitive: false,
                },
            )
            .unwrap();
        store.begin_download(record.id).unwrap();
        drop(store);
        let files = dir.path().join("files");
        fs::create_dir_all(files.join("out-orphan-0")).unwrap();
        fs::write(files.join("out-orphan-0").join("copy.bin"), b"orphan").unwrap();
        fs::write(files.join(".tmpArchive"), b"stale archive").unwrap();
        fs::create_dir_all(files.join(".staging").join("out-x-0")).unwrap();

        let s = MobileV2Session::new(
            open(),
            MobileV2Identity::from_parts([1; 32], [2; 16], [3; 16]),
            Arc::new(FakeLink::default()),
        );
        let settled = s.coordinator.store.lock().unwrap().by_id(record.id).unwrap().unwrap();
        assert_eq!(settled.download, crate::DownloadState::Failed);
        assert_eq!(settled.source_availability, crate::SourceAvailability::SourceOffline);
        assert!(!files.join("out-orphan-0").exists());
        assert!(!files.join(".tmpArchive").exists());
        assert_eq!(fs::read_dir(files.join(".staging")).unwrap().count(), 0);
        assert!(files.join("photo").exists(), "the record's own directory stays");
        s.peer_ready(&source);
        let online = s.coordinator.store.lock().unwrap().by_id(record.id).unwrap().unwrap();
        assert_eq!(online.source_availability, crate::SourceAvailability::Ready);
        assert!(online.source_online);
    }

    /// A Ready PC `source` for listings.
    fn pc_peer(s: &Recorded, source: &str) {
        s.set_peers(vec![Peer {
            id: source.into(),
            kind: PeerKind::Pc,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        }]);
    }

    /// A received folder message from `source` with `content_id`.
    fn received_folder(s: &Recorded, source: &str, content_id: &str) -> crate::MessageRecord {
        s.coordinator
            .store
            .lock()
            .unwrap()
            .insert_announcement(
                1,
                IncomingAnnouncement {
                    sender_id: source.into(),
                    proxy_id: None,
                    content_id: content_id.into(),
                    kind: AttachmentKind::Folder,
                    display_name: "Trip".into(),
                    mime: None,
                    size: 0,
                    text: None,
                    rich_text_format: None,
                    source: SourceState {
                        source_id: source.into(),
                        availability: crate::SourceAvailability::Ready,
                    },
                    operation_id: [5; 16],
                    message_id: [6; 16],
                    item_id: 0,
                    representation_id: 0,
                    sensitive: false,
                },
            )
            .unwrap()
    }

    /// What the host hears about listings: pages and failures as
    /// (operation, folder, directory, Ok(()) | Err((error, retryable))), and
    /// whether any plain transfer error came along.
    #[allow(clippy::type_complexity)]
    fn listing_events(
        s: &Recorded,
    ) -> (
        Vec<([u8; 16], i64, u64, Result<(), (MobileV2TransportError, bool)>)>,
        bool,
    ) {
        let mut listings = Vec::new();
        let mut errors = false;
        for event in s.take_events() {
            match event {
                TransferEvent::DirectoryPage {
                    operation_id,
                    folder_message_id,
                    directory,
                    ..
                } => listings.push((operation_id, folder_message_id, directory, Ok(()))),
                TransferEvent::DirectoryFailed {
                    operation_id,
                    folder_message_id,
                    directory,
                    error,
                    retryable,
                } => listings.push((operation_id, folder_message_id, directory, Err((error, retryable)))),
                TransferEvent::Error { .. } => errors = true,
                _ => {}
            }
        }
        (listings, errors)
    }

    /// The host asks for a folder's root as node 0 and files the page under
    /// the node it asked for. A PC folder's root has its own node id, which
    /// goes on the wire; the page still reports 0, or the host's root page
    /// never fills and its folder view loads forever.
    #[test]
    fn a_root_listing_reports_the_node_the_host_asked_for() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = "07".repeat(32);
        pc_peer(&s, &source);
        let offer = offer_id(DeviceId([7; 32]), [5; 16], 1);
        let content_id = file_content_id(&offer, 77, 3).unwrap();
        let folder = received_folder(&s, &source, &content_id);
        s.list_directory([9; 16], &content_id, 0, None).unwrap();
        let request = link.low.lock().unwrap()[0].read.clone().unwrap();
        let v2::Message::ContentListDirectory(list) = &request else {
            panic!("expected a directory listing request")
        };
        assert_eq!(list.node().unwrap(), 77, "the source is asked for the root's own node");
        let page = dto::DirectoryPage::from_raw(
            [(1, 77u64.write()), (2, 3u64.write()), (3, RawValue::Array(Vec::new()))].into(),
        )
        .unwrap();
        let reply = dto::ContentDirectory::from_raw([(1, offer.write()), (2, page.write())].into()).unwrap();
        assert!(
            s.receive_low_reply([9; 16], &request, None, v2::Message::ContentDirectory(reply))
                .unwrap()
        );
        assert_eq!(listing_events(&s), (vec![([9; 16], folder.id, 0, Ok(()))], false));
    }

    /// A PC that refuses a listing ends the folder view's wait with that
    /// reason, under the node the host asked for: content the PC no longer
    /// has is final, a lost link can be retried.
    #[test]
    fn a_refused_or_interrupted_listing_ends_in_a_directory_failure() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = "07".repeat(32);
        pc_peer(&s, &source);
        let offer = offer_id(DeviceId([7; 32]), [5; 16], 1);
        let content_id = file_content_id(&offer, 77, 3).unwrap();
        let folder = received_folder(&s, &source, &content_id);

        s.list_directory([9; 16], &content_id, 0, None).unwrap();
        let request = link.low.lock().unwrap()[0].read.clone().unwrap();
        let failed = dto::ContentDirectoryFailed::from_raw(
            [
                (1, offer.write()),
                (2, 77u64.write()),
                (3, dto::ContentUnavailable::NoSuchItem.write()),
            ]
            .into(),
        )
        .unwrap();
        let error = s
            .receive_low_reply([9; 16], &request, None, v2::Message::ContentDirectoryFailed(failed))
            .unwrap_err();
        s.finish_low(&source, [9; 16], false, None, Some(error));
        let gone = MobileV2TransportError::SourceUnavailable(crate::SourceAvailability::NoSuchItem);
        assert_eq!(
            listing_events(&s),
            (vec![([9; 16], folder.id, 0, Err((gone, false)))], false),
            "one page-level failure instead of a toast-only transfer error"
        );

        s.list_directory([10; 16], &content_id, 12, None).unwrap();
        let lost = MobileV2TransportError::Link("link lost".into());
        s.finish_low(&source, [10; 16], false, None, Some(lost.clone()));
        assert_eq!(
            listing_events(&s),
            (vec![([10; 16], folder.id, 12, Err((lost, true)))], false)
        );
    }

    /// A listing that cannot even be asked for (its PC is offline) fails the
    /// same way, at once, and can be retried.
    #[test]
    fn a_listing_of_an_offline_source_fails_at_once_and_can_be_retried() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = "07".repeat(32);
        let offer = offer_id(DeviceId([7; 32]), [5; 16], 1);
        let content_id = file_content_id(&offer, 77, 3).unwrap();
        let folder = received_folder(&s, &source, &content_id);
        s.list_directory([9; 16], &content_id, 0, None).unwrap();
        assert!(link.low.lock().unwrap().is_empty());
        assert_eq!(
            listing_events(&s),
            (
                vec![(
                    [9; 16],
                    folder.id,
                    0,
                    Err((MobileV2TransportError::PeerUnavailable(source), true))
                )],
                false
            )
        );
    }

    /// A folder received phone to phone whose local copy was cleared cannot
    /// be listed again (its sender keeps nothing): the folder view shows it
    /// as gone, with nothing to retry.
    #[test]
    fn a_cleared_phone_to_phone_folder_fails_its_listing_for_good() {
        let link = Arc::new(FakeLink::default());
        let (_dir, s) = session(link.clone());
        let source = "07".repeat(32);
        let offer = offer_id(DeviceId([7; 32]), [5; 16], 1);
        let content_id = content_id_for_representation(&offer, 1, 0).unwrap();
        let folder = received_folder(&s, &source, &content_id);
        {
            let mut store = s.coordinator.store.lock().unwrap();
            store.set_direct(&[folder.id], &source, None).unwrap();
            store.clear_cached_files().unwrap();
        }
        s.list_directory([9; 16], &content_id, 0, None).unwrap();
        assert!(link.low.lock().unwrap().is_empty(), "the sending phone is never asked");
        let evicted = MobileV2TransportError::SourceUnavailable(crate::SourceAvailability::Evicted);
        assert_eq!(
            listing_events(&s),
            (vec![([9; 16], folder.id, 0, Err((evicted, false)))], false)
        );
    }
}
