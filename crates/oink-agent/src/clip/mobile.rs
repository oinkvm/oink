//! Desktop owner for mobile clipboard uploads. B0 negotiates the operation;
//! its Low stream stages bytes before a guarded native publication.
use super::*;
use crate::mobile_v2::PROXY_CACHE_DIR;
use oink_clipboard::{
    NativeSpec,
    content::namespace::{Files, Local, Namespace},
    model::{
        ContentItem, ContentSize, ItemId, Medium, NativeFormat, NodeId, OfferId, REPRESENTATION_BYTES, Representation,
        RepresentationData, RepresentationId, StandardFormat,
    },
    sync::files::TargetOs,
};
use oink_foundation::clipboard::{ClipData, ClipPlatform, Sensitivity};
use oink_network::link::{InboundLow, LinkId};
use oink_protocol::v2::{
    self, Message,
    schema::{WireDto, WireValue},
};
use std::{fs, io, path::Path};
use tokio::io::AsyncWriteExt;

const MAX_ITEMS: u16 = 64;
/// A broadcast carries text only, at most this much per item.
const BROADCAST_TEXT_BYTES: u64 = 64 * 1024;
/// A directed phone share has no size cap (user 2026-11-10): large files and
/// folders are written to the proxy cache as they stream in, and the disk is
/// the only bound. The value only guards the byte counters from overflow.
const MAX_SHARE_BYTES: u64 = u64::MAX;
const IDLE: Duration = Duration::from_secs(30);

pub(super) type Operation = (DeviceId, [u8; 16]);
pub(super) struct Pending {
    pub begin: dto::ClipboardShareBegin,
    pub link: LinkId,
    pub group: oink_foundation::GroupId,
    pub revision: u64,
    pub expires: Instant,
    pub receive_permit: content::ReaderGuard,
    pub proxy_permit: Option<content::ReaderGuard>,
}

pub(super) struct Received {
    pub operation: Operation,
    pub pending: Pending,
    pub valid: Arc<dyn Fn() -> bool + Send + Sync>,
    pub payload: io::Result<Payload>,
}

#[derive(serde::Deserialize, serde::Serialize)]
enum Item {
    Data {
        id: u32,
        format: StandardFormat,
        size: u64,
    },
    File {
        id: u32,
        name: String,
        folder: bool,
        /// A proxied photo also carries a decoded PNG at `r-<id>`, so it is
        /// published like a picture copied on this PC: file plus bitmap.
        #[serde(default)]
        bitmap: Option<u64>,
    },
}

impl Item {
    /// The share item this belongs to: a text item's rich text is a further
    /// representation of it (`data_id`).
    fn item(&self) -> u32 {
        match self {
            Self::Data { id, .. } => id % u32::from(MAX_ITEMS),
            Self::File { id, .. } => *id,
        }
    }

    fn format(&self) -> Option<StandardFormat> {
        match self {
            Self::Data { format, .. } => Some(*format),
            Self::File { .. } => None,
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Manifest {
    group: [u8; 16],
    items: Vec<Item>,
}

struct Backing {
    path: PathBuf,
    persistent: AtomicBool,
}
impl Drop for Backing {
    fn drop(&mut self) {
        if !self.persistent.load(Ordering::Acquire) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub(super) struct Payload {
    backing: Arc<Backing>,
    /// The phone that uploaded these files: the authenticated link peer, or
    /// the persisted offer's real sender after a restart.
    sender: DeviceId,
    sensitivity: Sensitivity,
    items: Vec<Item>,
    /// Keeps "清理缓存" away from an upload until it is published or dropped.
    _upload: Option<oink_clipboard::cache::SourceLease>,
}
struct ProviderFiles {
    inventory: Inventory,
    backing: Arc<Backing>,
    files: Option<Arc<dyn Namespace>>,
}
struct Portable;
impl content::Translator for Portable {
    fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
        content::decode_standard(rep, bytes)
    }
}
impl Provider for ProviderFiles {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<oink_clipboard::FileReader> {
        let path = self.backing.path.join(format!("r-{}", id.0));
        // A live read keeps "清理缓存" from removing the proxy directory.
        let lease = oink_clipboard::cache::SourceLease::busy(path.clone());
        Ok(Box::new(LeasedReader(fs::File::open(path)?, lease)))
    }
    fn translator(&self) -> Arc<dyn content::Translator> {
        Arc::new(Portable)
    }
    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        self.files.clone()
    }
}

// The lease field is held only for its Drop: the read ends with the reader.
struct LeasedReader(fs::File, #[allow(dead_code)] oink_clipboard::cache::SourceLease);
impl io::Read for LeasedReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}

impl Payload {
    pub fn spec(&self, id: OfferId) -> io::Result<NativeSpec> {
        let file_paths: Vec<_> = self
            .items
            .iter()
            .filter_map(|item| match item {
                Item::File { id, name, .. } => {
                    Some((NodeId(u64::from(*id) + 1), file_path(&self.backing.path, *id, name)))
                }
                _ => None,
            })
            .collect();

        // The phone's files are received files from that phone: a native
        // receive on this PC files them in the phone's own folder.
        let files = (!file_paths.is_empty())
            .then(|| Local::received_from(file_paths, self.sender))
            .transpose()?;
        let roots = files.as_ref().map_or_else(Vec::new, |namespace| namespace.roots());
        let file_count = self
            .items
            .iter()
            .filter(|item| matches!(item, Item::File { .. }))
            .count();
        if roots.len() != file_count {
            return Err(io::ErrorKind::NotFound.into());
        }
        let namespace = files.map(|files| files as Arc<dyn Namespace>);
        let native_files = namespace
            .clone()
            .map(|namespace| Files::new(namespace, TargetOs::current()))
            .transpose()?;
        let representation = |rep: u32, format: StandardFormat, size: u64| Representation {
            id: RepresentationId(rep),
            format: NativeFormat::Unknown(oink_clipboard::wire::portable(format).0),
            medium: Medium::Bytes,
            aspect: 1,
            index: -1,
            standard: Some(format),
            data: RepresentationData::Reader {
                size: ContentSize::Known(size),
            },
        };
        let data = |item: u32, rep: u32, format: StandardFormat, size: u64| ContentItem {
            id: ItemId(item),
            file: None,
            virtual_files: false,
            representations: vec![representation(rep, format, size)],
        };
        // Every phone image is published with both its file and its bitmap
        // (user 2026-11-12), in the Inventory::bitmap_root shape: the file's
        // anchor item directly followed by its bitmap item. macOS writes each
        // image as one pasteboard item (bitmap + file URL); Windows holds one
        // bitmap, the first image's, next to the whole file list. A non-image
        // file is its anchor only. A text item's rich text joins its plain
        // text as further representations of one item, so the clipboard gets
        // plain text, HTML and RTF together (2026-11-12).
        let mut items: Vec<ContentItem> = Vec::new();
        for item in &self.items {
            match item {
                Item::Data { id, format, size } => match items.last_mut() {
                    Some(last) if *id != item.item() && last.id == ItemId(item.item()) && last.file.is_none() => {
                        last.representations.push(representation(*id, *format, *size));
                    }
                    _ => items.push(data(item.item(), *id, *format, *size)),
                },
                Item::File { id, bitmap, .. } => {
                    items.push(ContentItem {
                        id: ItemId(*id),
                        file: Some(NodeId(u64::from(*id) + 1)),
                        virtual_files: false,
                        representations: Vec::new(),
                    });
                    // The bitmap is its own item next to the file: the content
                    // model never publishes standard formats of a file item
                    // (those are file icons), but a copied picture offers both.
                    items.extend(bitmap.map(|size| data(u32::from(MAX_ITEMS) + *id, *id, StandardFormat::Png, size)));
                }
            }
        }
        let inventory = Inventory {
            id,
            platform: ClipPlatform::Unknown,
            items,
            roots,
        };
        inventory.validate().map_err(content::unavailable)?;
        // The proxy holds every file locally: native writers can name these
        // paths directly instead of serving them through a file provider.
        let mut entries = native_files
            .as_ref()
            .map_or_else(Vec::new, |files| files.roots().to_vec());
        if let Some(namespace) = &namespace {
            for (entry, root) in entries.iter_mut().zip(&inventory.roots) {
                entry.local_path = namespace
                    .tracked_file(root.id)?
                    .map(|tracked| {
                        tracked.resolve().map_err(|e| match e {
                            oink_clipboard::tracked_file::ResolveError::Gone => {
                                content::unavailable(Unavailable::NoSuchItem)
                            }
                            oink_clipboard::tracked_file::ResolveError::Superseded => {
                                content::unavailable(Unavailable::Superseded)
                            }
                            oink_clipboard::tracked_file::ResolveError::Io(e) => e,
                        })
                    })
                    .transpose()?;
            }
        }
        Ok(NativeSpec {
            contents: Arc::new(ProviderFiles {
                inventory: inventory.clone(),
                backing: self.backing.clone(),
                files: namespace,
            }),
            inventory,
            files: entries,
            file_contents: native_files.map(|files| files as Arc<dyn oink_clipboard::FileContents>),
            sensitivity: self.sensitivity,
        })
    }

    pub fn persist(self, group: oink_foundation::GroupId, offer: &dto::ClipboardOffer) -> io::Result<()> {
        let metadata = serde_json::to_vec(&Manifest {
            group: group.0,
            items: self.items,
        })?;
        let offer = offer.encode(v2::limits::LOW_FRAME).map_err(io::Error::other)?;
        fs::write(self.backing.path.join("items.json"), metadata)?;
        fs::write(self.backing.path.join("offer.pending"), offer)?;
        fs::rename(
            self.backing.path.join("offer.pending"),
            self.backing.path.join("offer.cbor"),
        )?;
        self.backing.persistent.store(true, Ordering::Release);
        Ok(())
    }

    pub fn cache_path(&self) -> &Path {
        &self.backing.path
    }
}

pub(super) fn restore(
    cache: &Path,
    group: oink_foundation::GroupId,
    clean_incomplete: bool,
) -> Vec<(PathBuf, dto::ClipboardOffer, NativeSpec)> {
    let mut restored = Vec::new();
    let Ok(entries) = fs::read_dir(cache.join(PROXY_CACHE_DIR)) else {
        return restored;
    };
    for entry in entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
    {
        let path = entry.path();
        let manifest = path.join("offer.cbor");
        if !manifest.is_file() {
            if clean_incomplete {
                let _ = fs::remove_dir_all(&path);
            }
            continue;
        }
        let result = (|| {
            let metadata: Manifest = serde_json::from_slice(&fs::read(path.join("items.json"))?)?;
            if metadata.group != group.0 {
                return Ok(None);
            }
            let raw = v2::cbor::decode_map(&fs::read(manifest)?, v2::limits::LOW_FRAME).map_err(io::Error::other)?;
            let offer = dto::ClipboardOffer::from_raw(raw).map_err(io::Error::other)?;
            let id = oink_clipboard::wire::local_offer_id(offer.id().map_err(io::Error::other)?)
                .map_err(io::Error::other)?;
            let sender = oink_network::wire_identity::read_device(&offer.real_sender().map_err(io::Error::other)?)
                .map_err(io::Error::other)?;
            let payload = Payload {
                backing: Arc::new(Backing {
                    path: path.clone(),
                    persistent: AtomicBool::new(true),
                }),
                sender,
                sensitivity: oink_clipboard::wire::local_sensitivity(offer.sensitivity().map_err(io::Error::other)?)
                    .map_err(io::Error::other)?,
                items: metadata.items,
                _upload: None,
            };
            for item in &payload.items {
                if let Item::File { name, .. } = item {
                    safe_name(Some(name.clone()))?;
                }
            }
            let spec = payload.spec(id)?;
            Ok::<_, io::Error>(Some((path.clone(), offer, spec)))
        })();
        match result {
            Ok(Some(value)) => restored.push(value),
            Ok(None) => {
                if clean_incomplete {
                    let _ = fs::remove_dir_all(&path);
                }
            }
            Err(error) => {
                tracing::debug!(kind=?error.kind(), "mobile proxy cache could not be restored");
                if clean_incomplete {
                    let _ = fs::remove_dir_all(&path);
                }
            }
        }
    }
    restored
}

/// Whether this PC can take the share as described; otherwise why not.
pub(super) fn admissible(
    begin: &dto::ClipboardShareBegin,
    peer: &oink_network::link::LinkEntry,
    me: oink_foundation::session::Boot,
) -> Result<(), dto::ShareRejectReason> {
    shape(begin, peer, me).unwrap_or(Err(dto::ShareRejectReason::Invalid))
}

fn shape(
    begin: &dto::ClipboardShareBegin,
    peer: &oink_network::link::LinkEntry,
    me: oink_foundation::session::Boot,
) -> v2::Result<Result<(), dto::ShareRejectReason>> {
    let source = oink_network::wire_identity::boot(peer.peer_boot);
    let target = oink_network::wire_identity::boot(me);
    if begin.source()? != source
        || begin.origin_sender()? != oink_clipboard::wire::device(peer.peer)
        || begin.target()?.is_some_and(|boot| boot != target)
        || !(1..=MAX_ITEMS).contains(&begin.item_count()?)
    {
        return Ok(Err(dto::ShareRejectReason::Invalid));
    }
    // A PC keeps a directed share and announces it to the group; a
    // broadcast (text only) is for this PC alone (protocol-v2 5.2).
    let max = match (begin.kind()?, begin.delivery()?) {
        (dto::ShareKind::Text, dto::ShareDelivery::Broadcast) => BROADCAST_TEXT_BYTES * u64::from(begin.item_count()?),
        (
            dto::ShareKind::Text
            | dto::ShareKind::File
            | dto::ShareKind::Folder
            | dto::ShareKind::Image
            | dto::ShareKind::Html
            | dto::ShareKind::Rtf,
            dto::ShareDelivery::Directed,
        ) => MAX_SHARE_BYTES,
        _ => return Ok(Err(dto::ShareRejectReason::Invalid)),
    };
    Ok(match begin.bytes()? {
        Some(bytes) if bytes <= max => Ok(()),
        Some(_) => Err(dto::ShareRejectReason::TooLarge),
        None => Err(dto::ShareRejectReason::Invalid),
    })
}

pub(super) fn operation(peer: DeviceId, first: &Message) -> Option<Operation> {
    let id = match first {
        Message::ClipboardShareTextChunk(value) => value.operation_id(),
        Message::ClipboardShareTextEnd(value) => value.operation_id(),
        Message::ContentOffer(value) => value.operation_id(),
        _ => return None,
    };
    Some((peer, id.ok()?.0))
}

/// The refusal of a share before it was accepted, with its reason
/// (protocol-v2 5.2).
pub(super) fn rejection(id: [u8; 16], reason: dto::ShareRejectReason) -> Message {
    let raw = [
        (1, v2::schema::FixedBytes(id).write()),
        (2, dto::ShareStatus::Unsupported.write()),
        (3, reason.write()),
    ]
    .into();
    Message::ClipboardShareReject(dto::ClipboardShareReject::from_raw(raw).expect("valid share rejection"))
}

/// The end of an accepted share.
pub(super) fn result(id: [u8; 16], status: dto::ShareStatus) -> Message {
    let raw = [(1, v2::schema::FixedBytes(id).write()), (2, status.write())].into();
    Message::ClipboardShareResult(dto::ClipboardShareResult::from_raw(raw).expect("valid share result"))
}

pub(super) async fn receive(mut operation: InboundLow, key: Operation, pending: Pending, root: PathBuf) -> Received {
    let valid = operation.stream.effect_validity();
    let payload = receive_payload(&mut operation, &pending.begin, root).await;
    if payload.is_ok() {
        operation.stream.finish();
    } else {
        operation.stream.cancel();
    }
    Received {
        operation: key,
        pending,
        valid,
        payload,
    }
}

async fn receive_payload(
    operation: &mut InboundLow,
    begin: &dto::ClipboardShareBegin,
    root: PathBuf,
) -> io::Result<Payload> {
    let invalid = |_| io::Error::from(io::ErrorKind::InvalidData);
    tokio::fs::create_dir_all(&root).await?;
    let dir = tempfile::Builder::new().prefix("share-").tempdir_in(root)?.keep();
    let mut payload = Payload {
        _upload: Some(oink_clipboard::cache::SourceLease::busy(dir.clone())),
        backing: Arc::new(Backing {
            path: dir,
            persistent: AtomicBool::new(false),
        }),
        sender: operation.stream.peer(),
        sensitivity: oink_clipboard::wire::local_sensitivity(
            begin
                .sensitivity()
                .map_err(invalid)?
                .unwrap_or(dto::Sensitivity::Normal),
        )
        .map_err(invalid)?,
        items: Vec::new(),
    };
    let id = begin.operation_id().map_err(invalid)?;
    let directed = begin.delivery().map_err(invalid)? == dto::ShareDelivery::Directed;
    let total = begin.bytes().map_err(invalid)?.ok_or(io::ErrorKind::InvalidData)?;
    let count = begin.item_count().map_err(invalid)?;
    let mut received: u64 = 0;
    let mut text_bytes = Vec::new();
    let mut open: Option<(dto::ContentOffer, Item, tokio::fs::File, u64)> = None;
    let mut next = Some(operation.first.clone());
    let mut ended = false;
    loop {
        let message = match next.take() {
            Some(message) => Some(message),
            None => tokio::time::timeout(IDLE, operation.stream.recv())
                .await
                .map_err(io::Error::other)?
                .map_err(io::Error::other)?,
        };
        let Some(message) = message else { break };
        if ended {
            return Err(io::ErrorKind::InvalidData.into());
        }
        match message {
            Message::ClipboardShareTextChunk(chunk)
                if open.is_none()
                    && count == 1
                    && begin.kind() == Ok(dto::ShareKind::Text)
                    && chunk.operation_id() == Ok(id.clone())
                    && chunk.offset() == Ok(received) =>
            {
                let bytes = chunk.bytes().map_err(invalid)?.0;
                received = bounded_add(received, bytes.len() as u64, total.min(REPRESENTATION_BYTES))?;
                text_bytes.extend_from_slice(&bytes);
            }
            Message::ContentOffer(offer) if open.is_none() && text_bytes.is_empty() => {
                // A text item carries its rich text (HTML, RTF) as further
                // representations of the same item, right after its plain
                // text (2026-11-12: phones send every format they have).
                let rich = payload
                    .items
                    .last()
                    .filter(|last| offer.item_id() == Ok(last.item()))
                    .map(|last| {
                        let reps: Vec<_> = payload.items.iter().filter(|item| item.item() == last.item()).collect();
                        (last.item(), reps)
                    });
                let (index, representation) = match &rich {
                    Some((item, reps)) => {
                        let formats: Vec<_> = reps.iter().filter_map(|rep| rep.format()).collect();
                        let format = match offer.kind() {
                            Ok(dto::ContentKind::Html) => StandardFormat::Html,
                            Ok(dto::ContentKind::Rtf) => StandardFormat::Rtf,
                            _ => return Err(io::ErrorKind::InvalidData.into()),
                        };
                        if formats.first() != Some(&StandardFormat::Text) || formats.contains(&format) {
                            return Err(io::ErrorKind::InvalidData.into());
                        }
                        (*item, reps.len() as u32)
                    }
                    None => (payload.items.last().map_or(0, |last| last.item() + 1), 0),
                };
                if index >= u32::from(count)
                    || offer.representation_id() != Ok(representation)
                    || offer.operation_id() != Ok(id.clone())
                    || offer.message_id() != begin.message_id()
                    || offer.real_sender() != begin.origin_sender()
                    || offer.source() != begin.source()
                    || offer.proxy() != Ok(None)
                    || offer.item_id() != Ok(index)
                    || offer.availability() != Ok(dto::ContentAvailability::Ready)
                    || !offer
                        .offer()
                        .and_then(oink_clipboard::wire::local_offer_id)
                        .is_ok_and(|offer| offer.clip.origin == operation.stream.peer())
                {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let size = offer.bytes().map_err(invalid)?.ok_or(io::ErrorKind::InvalidData)?;
                bounded_add(received, size, total)?;
                let kind = offer.kind().map_err(invalid)?;
                let data = match kind {
                    dto::ContentKind::Text => Some(StandardFormat::Text),
                    // Every image (PNG too) arrives as a file and is published
                    // with its bitmap, made at the end (user 2026-11-12).
                    dto::ContentKind::Image if directed => None,
                    // Refused below: a broadcast carries text only.
                    dto::ContentKind::Image if offer.mime().is_ok_and(|mime| mime.0 == "image/png") => {
                        Some(StandardFormat::Png)
                    }
                    dto::ContentKind::Html => Some(StandardFormat::Html),
                    dto::ContentKind::Rtf => Some(StandardFormat::Rtf),
                    dto::ContentKind::File => None,
                    dto::ContentKind::Folder if offer.mime().is_ok_and(|mime| mime.0 == "application/zip") => None,
                    _ => return Err(io::ErrorKind::Unsupported.into()),
                };
                if !directed {
                    // A broadcast carries text: plain text, with its rich text.
                    if data != Some(StandardFormat::Text) && rich.is_none() {
                        return Err(io::ErrorKind::PermissionDenied.into());
                    }
                    bounded_add(0, size, BROADCAST_TEXT_BYTES)?;
                }
                let item = if let Some(format) = data {
                    if size > REPRESENTATION_BYTES {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    Item::Data {
                        id: data_id(index, representation),
                        format,
                        size,
                    }
                } else {
                    Item::File {
                        id: index,
                        name: safe_name(
                            offer
                                .display_name()
                                .map_err(invalid)?
                                .map(|s| s.0)
                                .or_else(|| image_name(&offer)),
                        )?,
                        folder: kind == dto::ContentKind::Folder,
                        bitmap: None,
                    }
                };
                let path = match &item {
                    Item::Data { id, .. } => payload.backing.path.join(format!("r-{id}")),
                    Item::File { id, folder: true, .. } => archive_path(&payload.backing.path, *id),
                    Item::File {
                        id,
                        name,
                        folder: false,
                        ..
                    } => file_path(&payload.backing.path, *id, name),
                };
                let parent = path.parent().expect("local staged path");
                tokio::fs::create_dir_all(parent).await?;
                let file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .await?;
                open = Some((offer, item, file, 0));
            }
            Message::ContentRepresentationBytes(chunk) => {
                let Some((offer, _, file, offset)) = open.as_mut() else {
                    return Err(io::ErrorKind::InvalidData.into());
                };
                if chunk.offer() != offer.offer()
                    || chunk.representation() != offer.representation_id()
                    || chunk.offset() != Ok(*offset)
                {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let bytes = chunk.bytes().map_err(invalid)?.0;
                *offset = bounded_add(
                    *offset,
                    bytes.len() as u64,
                    offer.bytes().map_err(invalid)?.unwrap_or(0),
                )?;
                received = bounded_add(received, bytes.len() as u64, total)?;
                file.write_all(&bytes).await?;
            }
            Message::ContentRepresentationEnd(end) => {
                let Some((offer, mut item, mut file, offset)) = open.take() else {
                    return Err(io::ErrorKind::InvalidData.into());
                };
                if end.offer() != offer.offer()
                    || end.representation() != offer.representation_id()
                    || end.size() != Ok(offset)
                    || offer.bytes() != Ok(Some(offset))
                {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                file.flush().await?;
                drop(file);
                if let Item::File {
                    id,
                    name,
                    folder: false,
                    bitmap,
                } = &mut item
                    && directed
                    && is_image(&offer)
                {
                    let source = file_path(&payload.backing.path, *id, name);
                    let target = payload.backing.path.join(format!("r-{id}"));
                    let is_png = offer.mime().is_ok_and(|mime| mime.0.eq_ignore_ascii_case("image/png"));
                    *bitmap = tokio::task::spawn_blocking(move || -> io::Result<u64> {
                        if is_png {
                            // Already the bitmap: its exact bytes, not a
                            // re-encode. A separate copy, because the file is
                            // handed to other apps (moved, edited) once pasted.
                            validate_png_file(&source)?;
                            let size = fs::metadata(&source)?.len();
                            if size > REPRESENTATION_BYTES {
                                return Err(io::ErrorKind::FileTooLarge.into());
                            }
                            fs::copy(&source, &target)?;
                            return Ok(size);
                        }
                        let png = oink_clipboard::image_file::png_from_image_file(&source)?;
                        fs::write(&target, &png)?;
                        Ok(png.len() as u64)
                    })
                    .await
                    .map_err(io::Error::other)?
                    .inspect_err(|error| {
                        // Detection only: the photo remains available as a file.
                        tracing::info!(kind = ?error.kind(), "proxied image has no bitmap (no system decoder, invalid or too large); published as file only");
                    })
                    .ok();
                }
                if let Item::File {
                    id, name, folder: true, ..
                } = &item
                {
                    let archive = archive_path(&payload.backing.path, *id);
                    let destination = file_path(&payload.backing.path, *id, name);
                    let valid = operation.stream.effect_validity();
                    tokio::task::spawn_blocking(move || {
                        extract_folder(&archive, &destination, &*valid)?;
                        fs::remove_file(archive)
                    })
                    .await
                    .map_err(io::Error::other)??;
                }
                payload.items.push(item);
            }
            Message::ClipboardShareTextEnd(end) if end.operation_id() == Ok(id.clone()) && open.is_none() => {
                if count == 1 && begin.kind() == Ok(dto::ShareKind::Text) && payload.items.is_empty() {
                    std::str::from_utf8(&text_bytes).map_err(io::Error::other)?;
                    tokio::fs::write(payload.backing.path.join("r-0"), &text_bytes).await?;
                    payload.items.push(Item::Data {
                        id: 0,
                        format: StandardFormat::Text,
                        size: received,
                    });
                }
                let items = payload.items.last().map_or(0, |last| last.item() + 1);
                if received != total || items != u32::from(count) {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                ended = true;
            }
            _ => return Err(io::ErrorKind::InvalidData.into()),
        }
    }
    if !ended {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(payload)
}

/// Storage and representation id of a data representation: the item's index
/// for its first representation (as before), and above every item index for
/// the rich text that follows plain text, so ids stay unique next to the
/// bitmaps of photos (`r-<item>`). `Item::item` maps it back.
fn data_id(item: u32, representation: u32) -> u32 {
    if representation == 0 {
        item
    } else {
        u32::from(MAX_ITEMS) * (representation + 1) + item
    }
}

fn is_image(offer: &dto::ContentOffer) -> bool {
    matches!(offer.kind(), Ok(dto::ContentKind::Image | dto::ContentKind::File))
        && offer
            .mime()
            .is_ok_and(|mime| mime.0.to_ascii_lowercase().starts_with("image/"))
}

/// A name for an image item sent without one, so the published file keeps a
/// type the system recognizes.
fn image_name(offer: &dto::ContentOffer) -> Option<String> {
    let mime = offer.mime().ok()?.0.to_ascii_lowercase();
    let extension = match mime.as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/heic" => "heic",
        "image/heif" => "heif",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/tiff" => "tiff",
        "image/bmp" => "bmp",
        "image/png" => "png",
        _ => return None,
    };
    Some(format!("image.{extension}"))
}

fn validate_png_file(path: &Path) -> io::Result<()> {
    use std::io::Read;
    let mut header = [0; 24];
    fs::File::open(path)?.read_exact(&mut header)?;
    let width = u32::from_be_bytes(header[16..20].try_into().expect("fixed PNG width"));
    let height = u32::from_be_bytes(header[20..24].try_into().expect("fixed PNG height"));
    if header[..16] != *b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR" || width == 0 || height == 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

fn bounded_add(value: u64, added: u64, limit: u64) -> io::Result<u64> {
    value
        .checked_add(added)
        .filter(|sum| *sum <= limit)
        .ok_or(io::ErrorKind::InvalidData.into())
}

fn safe_name(name: Option<String>) -> io::Result<String> {
    let name = name.unwrap_or_else(|| "shared-file".into());
    let stem = name.split('.').next().unwrap_or_default().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            });
    if name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || reserved
        || name.ends_with(['.', ' '])
        || name.contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*'])
        || name.chars().any(char::is_control)
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(name)
}

fn file_path(root: &Path, id: u32, name: &str) -> PathBuf {
    root.join(format!("file-{id}")).join(name)
}

fn archive_path(root: &Path, id: u32) -> PathBuf {
    root.join(format!("folder-{id}.zip"))
}

fn extract_folder(archive: &Path, destination: &Path, valid: &dyn Fn() -> bool) -> io::Result<()> {
    use std::io::{Read, Write};
    const MAX_DEPTH: usize = 64;
    const MAX_ENTRIES: usize = 65_535;
    let mut archive = zip::ZipArchive::new(fs::File::open(archive)?).map_err(io::Error::other)?;
    if archive.len() > MAX_ENTRIES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    fs::create_dir_all(destination.parent().ok_or(io::ErrorKind::InvalidData)?)?;
    fs::create_dir(destination)?;
    let mut entries = std::collections::HashSet::new();
    let mut total = 0_u64;
    let mut buffer = vec![0; 64 * 1024];
    for index in 0..archive.len() {
        if !valid() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let mut file = archive.by_index(index).map_err(io::Error::other)?;
        let directory = file.is_dir();
        let name = std::str::from_utf8(file.name_raw()).map_err(io::Error::other)?;
        let name = if directory {
            name.strip_suffix('/').ok_or(io::ErrorKind::InvalidData)?
        } else {
            name
        };
        let mode = file.unix_mode().unwrap_or_default() & 0o170000;
        if (mode != 0 && mode != if directory { 0o040000 } else { 0o100000 })
            || file.compression() != zip::CompressionMethod::Stored
            || !entries.insert(name.to_lowercase())
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let parts: Vec<_> = name.split('/').collect();
        if parts.len() > MAX_DEPTH {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut path = destination.to_owned();
        for part in parts {
            path.push(safe_name(Some(part.into()))?);
        }
        total = bounded_add(total, file.size(), MAX_SHARE_BYTES)?;
        if directory {
            if file.size() != 0 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            fs::create_dir(path)?;
        } else {
            let mut output = fs::OpenOptions::new().write(true).create_new(true).open(path)?;
            let mut written = 0;
            loop {
                if !valid() {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                written = bounded_add(written, count as u64, file.size())?;
                output.write_all(&buffer[..count])?;
            }
            if written != file.size() {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
    }
    Ok(())
}

#[cfg(all(test, debug_assertions))]
#[path = "mobile_p2p_tests.rs"]
mod p2p_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn folder_zip(path: &Path, entries: &[(&str, Option<&[u8]>)]) {
        let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            if let Some(bytes) = bytes {
                zip.start_file(*name, options).unwrap();
                zip.write_all(bytes).unwrap();
            } else {
                zip.add_directory(*name, options).unwrap();
            }
        }
        zip.finish().unwrap();
    }

    /// One desktop node with a fake clipboard and one admitted phone
    /// (MobileNetwork + MobileV2Session) in the same group.
    struct MobileFixture {
        _dir: tempfile::TempDir,
        mobile_dir: PathBuf,
        database: PathBuf,
        files: PathBuf,
        desktop: crate::node::Node,
        desktop_clipboard: Arc<oink_test_support::clipboard::FakeClipboard>,
        mobile: oink_mobile_core::MobileNetwork,
        session: Arc<oink_mobile_core::MobileV2Session<oink_mobile_core::MobileNetworkLink>>,
        /// What the session pushed (`take_events`).
        events: EventLog,
        desktop_id: String,
        /// A second desktop of the same group, linked to the first.
        second: Option<(crate::node::Node, Arc<oink_test_support::clipboard::FakeClipboard>)>,
    }

    async fn mobile_fixture() -> MobileFixture {
        mobile_fixture_with(false).await
    }

    async fn mobile_fixture_with(second_desktop: bool) -> MobileFixture {
        use crate::{
            api::AgentApi,
            config_module::group_store::{self, StoredGroupState},
            snapshot_tests::{Displays, common, start_with_clip},
        };
        use oink_mobile_core::{
            MobileNetwork, MobileNetworkConfig, MobileProcess, MobileV2Identity, MobileV2Session, Peer, PeerKind,
            PeerState, TransferStore,
        };
        use oink_network::{group_state::State, identity::Identity};
        use oink_test_support::clipboard::FakeClipboard;

        let dir = tempfile::tempdir().unwrap();
        let pc_dir = dir.path().join("desktop");
        let mobile_dir = dir.path().join("mobile");
        fs::create_dir_all(&pc_dir).unwrap();
        fs::create_dir_all(&mobile_dir).unwrap();
        let pc_identity = Identity::load_or_create(&pc_dir).unwrap();
        let mobile_seed = [42; 32];
        let mobile_identity = Identity::from_seed(mobile_seed);
        let initial = State::create(&pc_identity, "desktop", std::env::consts::OS, 1).unwrap();
        let mut roster = initial
            .roster()
            .add(&pc_identity, mobile_identity.id(), "mobile", "ios", 2)
            .unwrap();
        let second_dir = dir.path().join("second");
        let second_identity = second_desktop.then(|| {
            fs::create_dir_all(&second_dir).unwrap();
            Identity::load_or_create(&second_dir).unwrap()
        });
        if let Some(second) = &second_identity {
            roster = roster
                .add(&pc_identity, second.id(), "second", std::env::consts::OS, 3)
                .unwrap();
        }
        let state = initial.with_roster(&roster).unwrap();
        group_store::save(
            &pc_dir.join(group_store::STATE_FILE),
            &StoredGroupState::new(pc_identity.id(), state.wire().clone()).unwrap(),
        )
        .unwrap();
        if let Some(second) = &second_identity {
            group_store::save(
                &second_dir.join(group_store::STATE_FILE),
                &StoredGroupState::new(second.id(), state.wire().clone()).unwrap(),
            )
            .unwrap();
        }

        let desktop_clipboard = Arc::new(FakeClipboard::new());
        desktop_clipboard.copy(vec![ClipData::Text("desktop clipboard".into())], Sensitivity::Normal);
        let desktop = start_with_clip(
            &oink_test_support::network_config(pc_dir, "desktop", 0),
            &Displays::new("desktop"),
            Some(desktop_clipboard.clone()),
        )
        .await;
        common::until("desktop captured the current clipboard", || {
            !desktop.clipboard_activity().unwrap().is_empty()
        })
        .await;
        let second = if second_desktop {
            let clipboard = Arc::new(FakeClipboard::new());
            let node = start_with_clip(
                &oink_test_support::network_config(second_dir, "second", 0),
                &Displays::new("second"),
                Some(clipboard.clone()),
            )
            .await;
            let address = desktop.links().unwrap().local_addr().to_string();
            node.set_member_address(desktop.id().into(), address).unwrap();
            common::until("both desktops linked", || {
                desktop.links().unwrap().links().ready(node.id()) && node.links().unwrap().links().ready(desktop.id())
            })
            .await;
            Some((node, clipboard))
        } else {
            None
        };

        let identity = MobileV2Identity::from_parts(mobile_identity.id().0, state.wire().group().unwrap().0, [7; 16]);
        let mobile = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: mobile_seed,
            boot: identity.boot,
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(state.wire().encode(v2::limits::CONFIG_STATE).unwrap()),
            group_state_path: mobile_dir.join("group-state.v2"),
            pairing_journal_path: mobile_dir.join("pairing-recovery.cbor"),
            device_name: "mobile".into(),
            os: "ios".into(),
            form_factor: None,
            trusted_peers: Vec::new(),
            trust_path: None,
            process: MobileProcess::App,
        })
        .await
        .unwrap();
        let links = desktop.links().unwrap();
        mobile.dial(desktop.id(), vec![links.local_addr()]).unwrap();
        common::until("signed C0 membership on both sides", || {
            links.links().ready(mobile_identity.id()) && mobile.admitted_peers().unwrap().contains(&desktop.id())
        })
        .await;

        let database = mobile_dir.join("history.sqlite");
        let files = mobile_dir.join("files");
        let session = Arc::new(MobileV2Session::new(
            TransferStore::open(&database, &files).unwrap(),
            identity,
            mobile.link.clone(),
        ));
        let desktop_id = desktop.id().to_hex();
        mobile.link.attach_clipboard(&session);
        session.set_history_foreground(true).unwrap();
        let events = record_events(&session);
        session.set_peers(vec![Peer {
            id: desktop_id.clone(),
            kind: PeerKind::Pc,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        }]);
        MobileFixture {
            _dir: dir,
            mobile_dir,
            database,
            files,
            desktop,
            desktop_clipboard,
            mobile,
            session,
            events,
            desktop_id,
            second,
        }
    }

    #[test]
    fn forwarded_phone_history_updates_pc_over_reliable_and_low() {
        use crate::snapshot_tests::common;
        use oink_foundation::clipboard_history as h;
        crate::test_watchdog::run(module_path!(), "phone_history_broadcast", Some(20), async {
            let fixture = mobile_fixture_with(true).await;
            let (second, second_clipboard) = fixture.second.as_ref().unwrap();
            let phone = oink_network::identity::Identity::from_seed([42; 32]).id();
            assert!(
                second
                    .link_config()
                    .unwrap()
                    .members()
                    .members
                    .iter()
                    .any(|m| m.device == phone && m.is_mobile())
            );
            let clock = fixture
                .desktop
                .clipboard()
                .unwrap()
                .shared
                .history
                .clock()
                .max(second.clipboard().unwrap().shared.history.clock())
                + 100;
            let mut r = h::ClipRecord {
                id: OfferId {
                    clip: ClipId {
                        origin: phone,
                        seq: clock,
                    },
                    generation: 1,
                },
                clock,
                time_ms: 1,
                origin: phone,
                proxy: None,
                sensitivity: Sensitivity::Normal,
                items: vec![h::HistoryItem {
                    id: 0,
                    kind: h::ItemKind::Text,
                    mime: "text/plain".into(),
                    name: None,
                    bytes: Some(14),
                    dimensions: None,
                    child_count: None,
                    children_bytes: None,
                    file_node: None,
                    file_version: None,
                }],
                formats: vec![h::HistoryFormat {
                    item: 0,
                    representation: 0,
                    name: "mime:text/plain;charset=utf-8".into(),
                    portable: Some(h::PortableFormat::Text),
                    bytes: Some(14),
                    file_node: None,
                }],
                digests: vec![],
                inline: vec![h::InlinePayload {
                    item: 0,
                    representation: 0,
                    format: h::PortableFormat::Text,
                    bytes: b"phone reliable".to_vec(),
                }],
                preview: None,
                rev: 0,
            };
            // The sender is a PC relay stub. The phone-authored delta is
            // already validated/merged here; exercise the production forward
            // sender and the other PC's full current-clipboard cut-over.
            let publish = |record: h::ClipRecord| {
                assert!(super::super::history::phone_offer(&record).is_some());
                let clip = fixture.desktop.clipboard().unwrap();
                clip.shared.history.merge(vec![record.clone()], vec![]).unwrap();
                clip.shared.history.ingest_inline(&record).unwrap();
                clip.tx
                    .send(Input::HistoryReady(
                        clip.shared.history.group(),
                        vec![h::HistoryChange::Record(record)],
                    ))
                    .unwrap();
            };
            publish(r.clone());
            common::until("forwarded phone record joins receiving PC window", || {
                second.clipboard().unwrap().shared.history.contains(r.id)
            })
            .await;
            common::until("forwarded phone Reliable record writes PC clipboard", || {
                second_clipboard.items() == vec![ClipData::Text("phone reliable".into())]
            })
            .await;
            for node in [&fixture.desktop, second] {
                assert!(
                    node.clipboard()
                        .unwrap()
                        .shared
                        .history
                        .window()
                        .holders()
                        .iter()
                        .all(|e| e.holder != phone)
                );
            }
            // A relay PC sends the next fully inline phone record over Low.
            // Exercise the same native cut-over as the direct Reliable path.
            r.clock += 1;
            r.id.clip.seq += 1;
            r.inline[0].bytes = vec![b'x'; 20 * 1024];
            r.formats[0].bytes = Some(20 * 1024);
            r.items[0].bytes = Some(20 * 1024);
            let message = Message::HistoryDelta(
                dto::HistoryDelta::from_domain(&h::HistoryChange::Record(r.clone()), [0; 32]).unwrap(),
            );
            assert!(v2::frame::encode(&message, v2::limits::RELIABLE_FRAME).is_err());
            publish(r);
            common::until("forwarded Low phone record writes the PC clipboard", || {
                second_clipboard.items() == vec![ClipData::Text("x".repeat(20 * 1024))]
            })
            .await;
            fixture.mobile.stop().await;
            fixture.desktop.shutdown().await;
            second.shutdown().await;
        });
    }

    #[test]
    fn admitted_mobile_queries_desktop_current_and_shares_over_low() {
        use crate::snapshot_tests::common;
        use oink_mobile_core::{ProxyChoice, SendItem, SendOrigin, SharePayload, TransferEvent, TransferStore};

        crate::test_watchdog::run(module_path!(), "mobile_desktop_clipboard_wire", Some(15), async {
            let MobileFixture {
                _dir,
                mobile_dir,
                database,
                files,
                desktop,
                desktop_clipboard,
                mobile,
                session,
                events,
                desktop_id,
                second: _,
            } = mobile_fixture().await;
            let request = [8; 16];
            common::until("mobile sent a B0 current query", || {
                session.request_current(request, 64).is_ok()
            })
            .await;
            common::until("mobile read desktop current text over Low", || {
                for event in take_events(&events) {
                    if let TransferEvent::Error { detail, .. } = event {
                        panic!("{detail}");
                    }
                }
                // Foreground history may announce metadata more than once;
                // completion is the stored body, not the event count.
                session.with_store(|store| {
                    store.page(None, 8).unwrap().messages.iter().any(|record| {
                        record.sender_id == desktop_id && record.text.as_deref() == Some("desktop clipboard")
                    })
                })
            })
            .await;
            let current = session.with_store(|store| store.page(None, 8).unwrap().messages);
            assert!(
                current
                    .iter()
                    .any(|record| record.sender_id == desktop_id && record.text.as_deref() == Some("desktop clipboard")),
                "{current:?}"
            );

            let operation = [9; 16];
            let text = ClipData::Text("mobile share over Low".into());
            session
                .send_share(
                    SharePayload {
                        sensitivity: Sensitivity::Normal,
                        origin: SendOrigin::TransferPage,
                        items: vec![SendItem::Text {
                            text: "mobile share over Low".into(),
                        }],
                        operation_id: operation,
                        message_id: [10; 16],
                    },
                    ProxyChoice::Automatic,
                )
                .unwrap();
            common::until("mobile share sent", || share_finished(&events, operation)).await;
            common::until("PC wrote the delivered text", || {
                desktop_clipboard.items() == vec![text.clone()]
            })
            .await;
            assert_eq!(desktop_clipboard.items(), vec![text]);

            desktop_clipboard.set_virtual_files(true);
            let shared_folder = mobile_dir.join("send-folder");
            fs::create_dir(&shared_folder).unwrap();
            fs::write(shared_folder.join("shared.txt"), b"folder contents").unwrap();
            let operation = [11; 16];
            session
                .send_share(
                    SharePayload {
                        sensitivity: Sensitivity::Normal,
                        origin: SendOrigin::TransferPage,
                        items: vec![SendItem::Folder {
                            path: shared_folder.to_str().unwrap().into(),
                            name: "send-folder".into(),
                        }],
                        operation_id: operation,
                        message_id: [12; 16],
                    },
                    ProxyChoice::Automatic,
                )
                .unwrap();
            common::until("proxy extracted and published mobile folder", || {
                share_finished(&events, operation)
            })
            .await;
            let published = desktop_clipboard.contents().unwrap().namespace().unwrap();
            let folder = published.roots().pop().unwrap();
            assert_eq!(folder.name, "send-folder");
            let child = published.list(folder.id, None).unwrap().entries.pop().unwrap();
            assert_eq!(child.name, "shared.txt");
            let child = published.select(child.id).unwrap();
            let mut reader = published.open(child.id, child.version, 0).unwrap();
            assert_eq!(content::read_all(&mut reader).unwrap(), b"folder contents");

            // A phone photo (non-PNG) is proxied as a file; the proxy publishes
            // it like a picture copied locally: the file plus a bitmap (D38).
            #[cfg(any(target_os = "macos", windows))]
            {
                use image::ImageEncoder;
                let photo = mobile_dir.join("photo.jpg");
                let rgb: Vec<u8> = (0..8).flat_map(|i| [i as u8 * 30, 100, 200]).collect();
                let mut jpeg = Vec::new();
                image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
                    .write_image(&rgb, 4, 2, image::ExtendedColorType::Rgb8)
                    .unwrap();
                fs::write(&photo, &jpeg).unwrap();
                let operation = [13; 16];
                session
                    .send_share(
                        SharePayload {
                            sensitivity: Sensitivity::Normal,
                            origin: SendOrigin::TransferPage,
                            items: vec![SendItem::Image {
                                path: photo.to_str().unwrap().into(),
                                encoded_size: jpeg.len() as u64,
                                mime: "image/jpeg".into(),
                                staged: false,
                                dimensions: Some((4, 2)),
                            }],
                            operation_id: operation,
                            message_id: [14; 16],
                        },
                        ProxyChoice::Automatic,
                    )
                    .unwrap();
                common::until("proxy published the phone photo", || share_finished(&events, operation)).await;
                // The native write carries both the decoded bitmap and the file.
                let items = desktop_clipboard.items();
                let png = items
                    .iter()
                    .find_map(|item| match item {
                        ClipData::Png(png) => Some(png.clone()),
                        _ => None,
                    })
                    .expect("bitmap published");
                assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
                assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 4);
                assert!(
                    items.iter().any(|item| matches!(item, ClipData::Files(files)
                        if files.iter().any(|file| file.path == "photo.jpg"))),
                    "the photo file stays available: {items:?}"
                );

                // Several images, PNG among them (user 2026-11-12): every
                // image is a file and a bitmap; a PNG's bitmap is its own
                // bytes. The pairing per image is native (macOS one item per
                // image, Windows the first bitmap), covered by oink-clipboard.
                let png = |red: u8| {
                    let mut png = Vec::new();
                    image::codecs::png::PngEncoder::new(&mut png)
                        .write_image(&[red, 0, 0, 255, 0, 255, 0, 255], 2, 1, image::ExtendedColorType::Rgba8)
                        .unwrap();
                    png
                };
                let (first, last) = (png(255), png(10));
                let (first_path, last_path) = (mobile_dir.join("shot.png"), mobile_dir.join("shot-2.png"));
                fs::write(&first_path, &first).unwrap();
                fs::write(&last_path, &last).unwrap();
                let send_image = |path: &Path, bytes: &[u8], mime: &str, dimensions: (u32, u32)| SendItem::Image {
                    path: path.to_str().unwrap().into(),
                    encoded_size: bytes.len() as u64,
                    mime: mime.into(),
                    staged: false,
                    dimensions: Some(dimensions),
                };
                let operation = [15; 16];
                session
                    .send_share(
                        SharePayload {
                            sensitivity: Sensitivity::Normal,
                            origin: SendOrigin::TransferPage,
                            items: vec![
                                send_image(&first_path, &first, "image/png", (2, 1)),
                                send_image(&photo, &jpeg, "image/jpeg", (4, 2)),
                                send_image(&last_path, &last, "image/png", (2, 1)),
                            ],
                            operation_id: operation,
                            message_id: [16; 16],
                        },
                        ProxyChoice::Automatic,
                    )
                    .unwrap();
                common::until("proxy published the phone images", || {
                    share_finished(&events, operation)
                })
                .await;
                let items = desktop_clipboard.items();
                let bitmaps: Vec<_> = items
                    .iter()
                    .filter_map(|item| match item {
                        ClipData::Png(png) => Some(png.clone()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(bitmaps.len(), 3, "{items:?}");
                assert_eq!(bitmaps[0], first, "a PNG is its own bitmap");
                assert_eq!(&bitmaps[1][..8], b"\x89PNG\r\n\x1a\n");
                assert_eq!(bitmaps[2], last);
                let names: Vec<String> = items
                    .iter()
                    .find_map(|item| match item {
                        ClipData::Files(files) => Some(files.iter().map(|file| file.path.clone()).collect()),
                        _ => None,
                    })
                    .expect("the image files");
                assert_eq!(names, ["shot.png", "photo.jpg", "shot-2.png"]);
            }

            mobile.stop().await;
            drop(session);
            let history = TransferStore::open(database, files).unwrap();
            assert!(
                history
                    .page(None, 64)
                    .unwrap()
                    .messages
                    .into_iter()
                    .any(|record| record.text.as_deref() == Some("desktop clipboard"))
            );
            desktop.shutdown().await;
        });
    }

    /// Review M5 (plan 4): facts reach every v2 peer complete and device
    /// options are gated by oink.device/2 alone. A share-session phone, which
    /// negotiates device/2 but not input/2, sees a PC's disabled mouse and can
    /// set it back to shared. Review M17: the PC's share refusal names why.
    #[test]
    fn a_phone_without_input_sees_and_clears_a_disabled_pc_mouse() {
        use crate::{
            config_module::group_store::{self, StoredGroupState},
            snapshot_tests::{Displays, common, start_with_clip},
        };
        use oink_foundation::input::InputMode;
        use oink_mobile_core::{
            MobileNetwork, MobileNetworkConfig, MobileProcess, MobileV2Identity, MobileV2Session, Peer, PeerKind,
            PeerState, ProxyChoice, SendItem, SendOrigin, SharePayload, TransferEvent, TransferStore,
        };
        use oink_network::{group_state::State, identity::Identity};
        use oink_protocol::v2::{RawValue as V, dto};

        crate::test_watchdog::run(module_path!(), "mobile_clears_disabled_mouse", Some(15), async {
            let dir = tempfile::tempdir().unwrap();
            let pc_dir = dir.path().join("desktop");
            let mobile_dir = dir.path().join("mobile");
            fs::create_dir_all(&pc_dir).unwrap();
            fs::create_dir_all(&mobile_dir).unwrap();
            let pc_identity = Identity::load_or_create(&pc_dir).unwrap();
            let mobile_seed = [43; 32];
            let mobile_identity = Identity::from_seed(mobile_seed);
            let initial = State::create(&pc_identity, "desktop", std::env::consts::OS, 1).unwrap();
            let roster = initial
                .roster()
                .add(&pc_identity, mobile_identity.id(), "mobile", "ios", 2)
                .unwrap();
            let state = initial.with_roster(&roster).unwrap();
            group_store::save(
                &pc_dir.join(group_store::STATE_FILE),
                &StoredGroupState::new(pc_identity.id(), state.wire().clone()).unwrap(),
            )
            .unwrap();
            let desktop = start_with_clip(
                &oink_test_support::network_config(pc_dir, "desktop", 0),
                &Displays::new("desktop"),
                Some(Arc::new(oink_test_support::clipboard::FakeClipboard::new())),
            )
            .await;
            let identity =
                MobileV2Identity::from_parts(mobile_identity.id().0, state.wire().group().unwrap().0, [8; 16]);
            let mobile = MobileNetwork::start(MobileNetworkConfig {
                identity_seed: mobile_seed,
                boot: identity.boot,
                bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                group_state: Some(state.wire().encode(v2::limits::CONFIG_STATE).unwrap()),
                group_state_path: mobile_dir.join("group-state.v2"),
                pairing_journal_path: mobile_dir.join("pairing-recovery.cbor"),
                device_name: "mobile".into(),
                os: "ios".into(),
                form_factor: None,
                trusted_peers: Vec::new(),
                trust_path: None,
                process: MobileProcess::Share,
            })
            .await
            .unwrap();
            let links = desktop.links().unwrap();
            mobile.dial(desktop.id(), vec![links.local_addr()]).unwrap();
            common::until("signed C0 membership on both sides", || {
                links.links().ready(mobile_identity.id()) && mobile.admitted_peers().unwrap().contains(&desktop.id())
            })
            .await;
            let table = links.links();
            let agreement = &table.by_peer(mobile_identity.id()).unwrap().agreement;
            assert!(agreement.contains("oink.device/2"));
            assert!(!agreement.contains("oink.input/2"));

            desktop
                .edit_device_option(desktop.id().into(), dto::LocalOption::MouseMode, 2u32.write())
                .unwrap();
            let mouse = |facts: &dto::DeviceFacts| {
                facts.preferences().ok().flatten().and_then(|items| {
                    items
                        .0
                        .into_iter()
                        .find(|item| item.option() == Ok(dto::LocalOption::MouseMode))
                        .map(|item| (item.value().unwrap(), item.revision().unwrap()))
                })
            };
            let mut revision = 0;
            common::until("the phone sees the disabled mouse", || {
                match mobile.peer_facts(desktop.id()).as_ref().and_then(mouse) {
                    Some((V::Uint(2), current)) => {
                        revision = current;
                        true
                    }
                    _ => false,
                }
            })
            .await;
            let request = Message::ConfigSetLocalOption(
                dto::SetLocalOption::from_raw(
                    [
                        (1, V::Bytes(vec![0x5a; 16])),
                        (2, oink_network::wire_identity::boot(desktop.boot()).write()),
                        (3, 1u32.write()),
                        (4, 0u32.write()),
                        (5, revision.write()),
                    ]
                    .into(),
                )
                .unwrap(),
            );
            let completion = mobile.device_control(request).await.unwrap();
            assert_eq!(completion.outcome, dto::ControlOutcome::Applied);
            common::until("the PC mouse is shared again", || {
                desktop.settings().routing.mouse_mode == InputMode::Shared
            })
            .await;

            // Review M17: a PC that does not receive refuses a proxy share with
            // its reason, so the phone can try another PC instead.
            {
                use crate::api::AgentApi;
                let mut settings = desktop.clipboard_settings().unwrap();
                settings.receive = false;
                desktop.set_clipboard_settings(settings.into()).unwrap();
            }
            let session = Arc::new(MobileV2Session::new(
                TransferStore::open(mobile_dir.join("history.sqlite"), mobile_dir.join("files")).unwrap(),
                identity,
                mobile.link.clone(),
            ));
            mobile.link.attach_clipboard(&session);
            session.set_history_foreground(true).unwrap();
            let events = record_events(&session);
            session.set_peers(vec![Peer {
                id: desktop.id().to_hex(),
                kind: PeerKind::Pc,
                state: PeerState::Ready,
                accepts_clipboard: true,
                connection_order: 1,
                relayed: false,
                clipboard_pending: false,
            }]);
            let file = mobile_dir.join("report.pdf");
            fs::write(&file, b"pdf body").unwrap();
            let ids = session
                .send_share(
                    SharePayload {
                        sensitivity: Sensitivity::Normal,
                        origin: SendOrigin::TransferPage,
                        items: vec![SendItem::File {
                            path: file.to_str().unwrap().into(),
                            name: "report.pdf".into(),
                            size: 0,
                            mime: None,
                            staged: false,
                        }],
                        operation_id: [21; 16],
                        message_id: [22; 16],
                    },
                    ProxyChoice::Automatic,
                )
                .unwrap();
            let mut failed = None;
            common::until("the PC refused the share with its reason", || {
                for event in take_events(&events) {
                    if let TransferEvent::DeliveryEvent {
                        record_ids,
                        failure,
                        detail,
                        ..
                    } = event
                        && record_ids == ids
                    {
                        failed = Some((failure, detail));
                    }
                }
                failed.is_some()
            })
            .await;
            let (failure, detail) = failed.unwrap();
            assert_eq!(failure, Some(oink_mobile_core::DeliveryFailure::Rejected));
            assert_eq!(detail.as_deref(), Some("Unsupported: ReceiveOff"));
            mobile.stop().await;
            desktop.shutdown().await;
        });
    }

    /// PC A cannot reach the phone directly; PC B can. A reaches the phone
    /// through B (the phone is a relay endpoint, B the relay station): the link
    /// is admitted on both ends, the phone is online on A, the current query,
    /// relayed Low reads work, shares use the direct PC B and propagate to A,
    /// and losing B takes the relayed link down on both ends.
    #[test]
    fn phone_reaches_a_desktop_through_a_relay_station() {
        use crate::{
            api::AgentApi,
            config_module::group_store::{self, StoredGroupState},
            device_cards::CardConnection,
            snapshot_tests::{Displays, common, start_with_clip},
        };
        use oink_mobile_core::{
            MobileNetwork, MobileNetworkConfig, MobileProcess, MobileV2Identity, MobileV2Session, Peer, PeerKind,
            PeerState, ProxyChoice, SendItem, SendOrigin, SharePayload, TransferEvent, TransferStore,
        };
        use oink_network::{
            group_state::State,
            identity::Identity,
            link::{LinkPath, testing::set_direct_blocked},
        };
        use oink_test_support::clipboard::FakeClipboard;

        crate::test_watchdog::run(module_path!(), "mobile_desktop_relayed", Some(15), async {
            let dir = tempfile::tempdir().unwrap();
            let a_dir = dir.path().join("desktop-a");
            let b_dir = dir.path().join("relay-b");
            let mobile_dir = dir.path().join("mobile");
            for path in [&a_dir, &b_dir, &mobile_dir] {
                fs::create_dir_all(path).unwrap();
            }
            let a_identity = Identity::load_or_create(&a_dir).unwrap();
            let b_identity = Identity::load_or_create(&b_dir).unwrap();
            let mobile_seed = [52; 32];
            let mobile_identity = Identity::from_seed(mobile_seed);
            let initial = State::create(&a_identity, "desktop-a", std::env::consts::OS, 1).unwrap();
            let roster = initial
                .roster()
                .add(&a_identity, b_identity.id(), "relay-b", std::env::consts::OS, 2)
                .unwrap()
                .add(&a_identity, mobile_identity.id(), "mobile", "android", 3)
                .unwrap();
            let state = initial.with_roster(&roster).unwrap();
            for (path, id) in [(&a_dir, a_identity.id()), (&b_dir, b_identity.id())] {
                group_store::save(
                    &path.join(group_store::STATE_FILE),
                    &StoredGroupState::new(id, state.wire().clone()).unwrap(),
                )
                .unwrap();
            }

            let a_clipboard = Arc::new(FakeClipboard::new());
            a_clipboard.copy(vec![ClipData::Text("desktop A clipboard".into())], Sensitivity::Normal);
            let a = start_with_clip(
                &oink_test_support::network_config(a_dir, "desktop-a", 0),
                &Displays::new("desktop-a"),
                Some(a_clipboard.clone()),
            )
            .await;
            let b = start_with_clip(
                &oink_test_support::network_config(b_dir, "relay-b", 0),
                &Displays::new("relay-b"),
                Some(Arc::new(FakeClipboard::new())),
            )
            .await;
            let a_links = a.links().unwrap();
            let b_links = b.links().unwrap();
            // A has no direct path to the phone, in either direction.
            set_direct_blocked(&a_links, mobile_identity.id(), true);
            common::until("desktop A captured its clipboard", || {
                !a.clipboard_activity().unwrap().is_empty()
            })
            .await;
            // No discovery in this fixture: hand A the address of B. A's own
            // member redial (without an address) may hold the dial slot, so
            // retry until one direct dial runs.
            common::until("A and B admitted each other", || {
                let _ = a_links.dial(
                    b.id(),
                    vec![b_links.local_addr()],
                    oink_network::link::DialMode::PreferDirect,
                    oink_network::link::DialOrigin::Initial,
                );
                a_links.links().ready(b.id()) && b_links.links().ready(a.id())
            })
            .await;

            let identity =
                MobileV2Identity::from_parts(mobile_identity.id().0, state.wire().group().unwrap().0, [17; 16]);
            let mobile = MobileNetwork::start(MobileNetworkConfig {
                identity_seed: mobile_seed,
                boot: identity.boot,
                bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                group_state: Some(state.wire().encode(v2::limits::CONFIG_STATE).unwrap()),
                group_state_path: mobile_dir.join("group-state.v2"),
                pairing_journal_path: mobile_dir.join("pairing-recovery.cbor"),
                device_name: "mobile".into(),
                os: "android".into(),
                form_factor: None,
                trusted_peers: Vec::new(),
                trust_path: None,
                process: MobileProcess::App,
            })
            .await
            .unwrap();
            // The phone reaches only B directly. A dials the phone on its own
            // (the member redial); with no direct path it searches a relay.
            mobile.dial(b.id(), vec![b_links.local_addr()]).unwrap();
            let mobile_id = mobile_identity.id();
            common::until("A and the phone admitted each other through B", || {
                a_links.links().ready(mobile_id)
                    && mobile.admitted_peers().unwrap().contains(&a.id())
                    && mobile.links().ready(a.id())
            })
            .await;
            assert_eq!(
                a_links.links().by_peer(mobile_id).unwrap().path,
                LinkPath::Relayed { via: b.id() }
            );
            let at_phone = mobile.links().by_peer(a.id()).unwrap().clone();
            assert_eq!(at_phone.path, LinkPath::Relayed { via: b.id() });
            assert!(at_phone.agreement.contains("oink.clipboard/2"));
            assert!(at_phone.agreement.contains("oink.relay/1"));
            assert!(
                !at_phone.agreement.contains("oink.relay.transit/1"),
                "a phone never forwards"
            );
            common::until("A shows the relayed phone online", || {
                a.device_cards()
                    .unwrap()
                    .cards
                    .iter()
                    .any(|card| card.device.0 == mobile_id && matches!(card.connection, CardConnection::Online { .. }))
            })
            .await;

            let database = mobile_dir.join("history.sqlite");
            let files = mobile_dir.join("files");
            let session = Arc::new(MobileV2Session::new(
                TransferStore::open(&database, &files).unwrap(),
                identity,
                mobile.link.clone(),
            ));
            let a_hex = a.id().to_hex();
            mobile.link.attach_clipboard(&session);
            session.set_history_foreground(true).unwrap();
            let events = record_events(&session);
            session.set_peers(vec![Peer {
                id: a_hex.clone(),
                kind: PeerKind::Pc,
                state: PeerState::Ready,
                accepts_clipboard: true,
                connection_order: 1,
                relayed: true,
                clipboard_pending: false,
            }]);

            // Current query to the relayed PC and its text over Low via B.
            let request = [18; 16];
            common::until("phone sent a current query to A", || {
                session.request_current(request, 64).is_ok()
            })
            .await;
            let mut announcements = 0;
            common::until("phone read A's text through the relay", || {
                for event in take_events(&events) {
                    match event {
                        TransferEvent::Error { detail, .. } => panic!("{detail}"),
                        TransferEvent::Announcement { .. } => announcements += 1,
                        _ => {}
                    }
                }
                announcements >= 2
            })
            .await;

            // Sharing uses direct PCs. The production roster also contains B;
            // keeping only relayed A would leave this fixture with no eligible
            // publisher/proxy under the shared-history routing contract.
            let b_hex = b.id().to_hex();
            session.set_peers(vec![
                Peer {
                    id: a_hex.clone(),
                    kind: PeerKind::Pc,
                    state: PeerState::Ready,
                    accepts_clipboard: true,
                    connection_order: 1,
                    relayed: true,
                    clipboard_pending: false,
                },
                Peer {
                    id: b_hex.clone(),
                    kind: PeerKind::Pc,
                    state: PeerState::Ready,
                    accepts_clipboard: true,
                    connection_order: 2,
                    relayed: false,
                    clipboard_pending: false,
                },
            ]);
            // B forwards this inline publication to A.
            let operation = [19; 16];
            session
                .send_share(
                    SharePayload {
                        sensitivity: Sensitivity::Normal,
                        origin: SendOrigin::TransferPage,
                        items: vec![SendItem::Text {
                            text: "phone text through the relay".into(),
                        }],
                        operation_id: operation,
                        message_id: [20; 16],
                    },
                    ProxyChoice::Automatic,
                )
                .unwrap();
            common::until("relayed text share committed on A", || {
                share_finished(&events, operation)
            })
            .await;
            common::until("A wrote the phone publication forwarded by B", || {
                a_clipboard.items() == vec![ClipData::Text("phone text through the relay".into())]
            })
            .await;
            assert_eq!(
                a_clipboard.items(),
                vec![ClipData::Text("phone text through the relay".into())]
            );

            // B holds the phone's proxy file and serves it to A.
            a_clipboard.set_virtual_files(true);
            let note = mobile_dir.join("note.txt");
            fs::write(&note, b"a small file through the relay").unwrap();
            let operation = [21; 16];
            session
                .send_share(
                    SharePayload {
                        sensitivity: Sensitivity::Normal,
                        origin: SendOrigin::TransferPage,
                        items: vec![SendItem::File {
                            path: note.to_str().unwrap().into(),
                            name: "note.txt".into(),
                            size: 30,
                            mime: Some("text/plain".into()),
                            staged: false,
                        }],
                        operation_id: operation,
                        message_id: [22; 16],
                    },
                    ProxyChoice::Device(b_hex),
                )
                .unwrap();
            common::until("B accepted the proxy file", || share_finished(&events, operation)).await;
            common::until("A wrote the file held by B", || {
                a_clipboard.contents().and_then(|c| c.namespace()).is_some()
            })
            .await;
            let contents = a_clipboard.contents().unwrap();
            // Virtual-file reads are blocking desktop callbacks. Keep their
            // synchronous network bridge outside the async test executor.
            let bytes = tokio::task::spawn_blocking(move || {
                let published = contents.namespace().unwrap();
                let file = published.roots().pop().unwrap();
                assert_eq!(file.name, "note.txt");
                let file = published.select(file.id)?;
                let mut reader = published.open(file.id, file.version, 0)?;
                content::read_all(&mut reader)
            })
            .await
            .unwrap()
            .expect("A reads the proxy file held by B");
            assert_eq!(bytes, b"a small file through the relay");

            // Without the relay station the phone is offline on A, and A on the
            // phone; nothing keeps showing a link that cannot carry anything.
            b.shutdown().await;
            common::until("the relayed link ended on both ends", || {
                a_links.links().by_peer(mobile_id).is_none() && mobile.links().by_peer(a.id()).is_none()
            })
            .await;
            common::until("A shows the phone offline", || {
                a.device_cards()
                    .unwrap()
                    .cards
                    .iter()
                    .any(|card| card.device.0 == mobile_id && matches!(card.connection, CardConnection::Offline { .. }))
            })
            .await;

            mobile.stop().await;
            drop(session);
            a.shutdown().await;
        });
    }

    type EventLog = Arc<std::sync::Mutex<Vec<oink_mobile_core::TransferEvent>>>;

    /// Collects the events the session pushes, for the test to read back.
    fn record_events(session: &oink_mobile_core::MobileV2Session<oink_mobile_core::MobileNetworkLink>) -> EventLog {
        let events = EventLog::default();
        let sink = events.clone();
        session.set_event_sink(Arc::new(move |event| sink.lock().unwrap().push(event)));
        events
    }

    fn take_events(events: &EventLog) -> Vec<oink_mobile_core::TransferEvent> {
        std::mem::take(&mut *events.lock().unwrap())
    }

    fn share_finished(events: &EventLog, expected: [u8; 16]) -> bool {
        use oink_mobile_core::TransferEvent;
        take_events(events).into_iter().any(|event| match event {
            TransferEvent::Error { detail, .. } => panic!("{detail}"),
            TransferEvent::DeliveryEvent {
                operation_id, status, ..
            } if operation_id == expected => {
                assert_eq!(status, "Complete");
                true
            }
            _ => false,
        })
    }

    /// The settled delivery status of one share; other events are ignored.
    fn share_status(events: &EventLog, expected: [u8; 16], status: &mut Option<String>) -> bool {
        for event in take_events(events) {
            if let oink_mobile_core::TransferEvent::DeliveryEvent {
                operation_id,
                status: s,
                ..
            } = event
                && operation_id == expected
                && (s == "Complete" || s == "Failed" || s == "Rejected")
            {
                *status = Some(s);
            }
        }
        status.is_some()
    }

    /// H9: a proxied upload is cached and announced even when this PC's
    /// clipboard changed while the phone uploaded; only writing it into this
    /// PC's system clipboard depends on that.
    #[test]
    fn proxied_upload_is_cached_and_announced_after_a_local_copy() {
        use crate::snapshot_tests::common;
        use oink_mobile_core::{ProxyChoice, SendItem, SendOrigin, SharePayload};
        crate::test_watchdog::run(module_path!(), "proxied_upload_after_local_copy", Some(15), async {
            let fixture = mobile_fixture().await;
            fixture.desktop_clipboard.set_virtual_files(true);
            let upload = fixture.mobile_dir.join("upload.bin");
            let size = 8 * 1024 * 1024;
            fs::write(&upload, vec![7u8; size]).unwrap();
            // The user keeps copying on this PC while the phone uploads.
            let copying = Arc::new(AtomicBool::new(true));
            let copier = {
                let clipboard = fixture.desktop_clipboard.clone();
                let copying = copying.clone();
                std::thread::spawn(move || {
                    let mut n = 0u64;
                    while copying.load(Ordering::Acquire) {
                        n += 1;
                        clipboard.copy(vec![ClipData::Text(format!("local {n}"))], Sensitivity::Normal);
                        std::thread::sleep(Duration::from_millis(2));
                    }
                })
            };
            let operation = [21; 16];
            fixture
                .session
                .send_share(
                    SharePayload {
                        sensitivity: Sensitivity::Normal,
                        origin: SendOrigin::TransferPage,
                        items: vec![SendItem::File {
                            path: upload.to_str().unwrap().into(),
                            name: "upload.bin".into(),
                            size: size as u64,
                            mime: None,
                            staged: false,
                        }],
                        operation_id: operation,
                        message_id: [22; 16],
                    },
                    ProxyChoice::Automatic,
                )
                .unwrap();
            let mut status = None;
            common::until("proxied share settled", || {
                share_status(&fixture.events, operation, &mut status)
            })
            .await;
            copying.store(false, Ordering::Release);
            copier.join().unwrap();
            assert_eq!(status.as_deref(), Some("Complete"));
            let cached = restore(&fixture.desktop.cache_dir(), fixture.desktop.group_id().unwrap(), false);
            assert_eq!(cached.len(), 1, "the upload stays in the proxy cache");
            assert_eq!(cached[0].1.operation_id().unwrap().0, operation);
            assert_eq!(cached[0].1.message_id().unwrap().0, [22; 16]);
            assert!(
                !fixture
                    .desktop_clipboard
                    .items()
                    .iter()
                    .any(|item| matches!(item, ClipData::Files(_))),
                "the later local copy keeps this PC's clipboard"
            );
            fixture.mobile.stop().await;
            fixture.desktop.shutdown().await;
        });
    }

    /// Bug 1: a file the phone shared through one PC (the proxy) is announced
    /// to the group and another PC can read its bytes from the proxy.
    #[test]
    fn proxied_file_reaches_a_second_pc() {
        use crate::snapshot_tests::common;
        use oink_mobile_core::{ProxyChoice, SendItem, SendOrigin, SharePayload};
        crate::test_watchdog::run(module_path!(), "proxied_file_second_pc", Some(20), async {
            let fixture = mobile_fixture_with(true).await;
            fixture.desktop_clipboard.set_virtual_files(true);
            let (second, second_clipboard) = fixture.second.as_ref().unwrap();
            second_clipboard.set_virtual_files(true);
            let upload = fixture.mobile_dir.join("report.txt");
            fs::write(&upload, b"shared through the proxy").unwrap();
            let operation = [25; 16];
            fixture
                .session
                .send_share(
                    SharePayload {
                        sensitivity: Sensitivity::Normal,
                        origin: SendOrigin::TransferPage,
                        items: vec![SendItem::File {
                            path: upload.to_str().unwrap().into(),
                            name: "report.txt".into(),
                            size: 24,
                            mime: Some("text/plain".into()),
                            staged: false,
                        }],
                        operation_id: operation,
                        message_id: [26; 16],
                    },
                    ProxyChoice::Automatic,
                )
                .unwrap();
            let mut status = None;
            common::until("proxied share settled", || {
                share_status(&fixture.events, operation, &mut status)
            })
            .await;
            assert_eq!(status.as_deref(), Some("Complete"));
            common::until("second PC wrote the proxied file", || {
                second_clipboard
                    .items()
                    .iter()
                    .any(|item| matches!(item, ClipData::Files(files) if files.iter().any(|f| f.path == "report.txt")))
            })
            .await;
            let contents = second_clipboard.contents().expect("virtual file contents");
            let bytes = tokio::task::spawn_blocking(move || {
                let published = contents.namespace().unwrap();
                let root = published.roots().pop().unwrap();
                let root = published.select(root.id)?;
                let mut reader = published.open(root.id, root.version, 0)?;
                content::read_all(&mut reader)
            })
            .await
            .unwrap()
            .expect("the second PC reads the file from the proxy");
            assert_eq!(bytes, b"shared through the proxy");
            fixture.mobile.stop().await;
            second.shutdown().await;
            fixture.desktop.shutdown().await;
        });
    }

    /// A phone reading content from a PC that stopped sending learns why: the
    /// read is answered with `SendDisabled`, not cancelled into a bare
    /// "interrupted".
    #[test]
    fn read_from_a_pc_that_stopped_sending_fails_with_the_reason() {
        use crate::{api::AgentApi, snapshot_tests::common};
        use image::ImageEncoder;
        use oink_mobile_core::TransferEvent;
        crate::test_watchdog::run(module_path!(), "read_from_send_disabled_pc", Some(20), async {
            let fixture = mobile_fixture().await;
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(&[9u8; 4 * 4 * 4], 4, 4, image::ExtendedColorType::Rgba8)
                .unwrap();
            let inline_png = png.clone();
            fixture
                .desktop_clipboard
                .copy(vec![ClipData::Png(png)], Sensitivity::Normal);
            let mut inline = None;
            common::until("the phone cached the inline image", || {
                inline = fixture
                    .session
                    .with_store(|store| store.page(None, 20))
                    .unwrap()
                    .messages
                    .into_iter()
                    .find(|record| {
                        record.kind == oink_mobile_core::AttachmentKind::Image
                            && record.download == oink_mobile_core::DownloadState::Complete
                    });
                inline.is_some()
            })
            .await;
            let inline = inline.unwrap();
            // Deterministic noise prevents PNG compression from shrinking this
            // below the history inline budget: the second image needs a read.
            let mut seed = 17u32;
            let pixels = (0..128 * 128 * 4)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                })
                .collect::<Vec<_>>();
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(&pixels, 128, 128, image::ExtendedColorType::Rgba8)
                .unwrap();
            assert!(png.len() > oink_foundation::clipboard_history::INLINE_MAX);
            fixture
                .desktop_clipboard
                .copy(vec![ClipData::Png(png)], Sensitivity::Normal);
            let mut image = None;
            common::until("the phone recorded the PC's image", || {
                let page = fixture
                    .session
                    .with_store(|store| store.page(None, 20))
                    .unwrap()
                    .messages;
                image = page
                    .into_iter()
                    .find(|record| record.kind == oink_mobile_core::AttachmentKind::Image && record.id != inline.id)
                    .map(|record| record.id);
                image.is_some()
            })
            .await;
            let desktop = fixture.desktop.clone();
            tokio::task::spawn_blocking(move || {
                let mut settings = desktop.clipboard_settings().unwrap();
                settings.send = false;
                desktop.set_clipboard_settings(settings.into()).unwrap();
            })
            .await
            .unwrap();
            fixture.session.request_download(inline.id, false).unwrap();
            let cached = fixture
                .session
                .with_store(|store| store.by_id(inline.id))
                .unwrap()
                .unwrap();
            assert_eq!(cached.download, oink_mobile_core::DownloadState::Complete);
            assert_eq!(fs::read(cached.local_path.unwrap()).unwrap(), inline_png);
            take_events(&fixture.events);
            fixture.session.request_download(image.unwrap(), false).unwrap();
            let mut detail = None;
            common::until("the read failed", || {
                for event in take_events(&fixture.events) {
                    if let TransferEvent::Error {
                        record_id: Some(id),
                        detail: d,
                        ..
                    } = event
                        && Some(id) == image
                    {
                        detail = Some(d);
                    }
                }
                detail.is_some()
            })
            .await;
            assert_eq!(
                detail.unwrap(),
                oink_mobile_core::MobileV2TransportError::SendDisabled.to_string()
            );
            fixture.mobile.stop().await;
            fixture.desktop.shutdown().await;
        });
    }

    /// M12: a direct share written into this PC's clipboard does not share
    /// its clip identity with the next local copy, so a write-back of the
    /// shared content is never taken for that copy.
    #[test]
    fn direct_share_marker_is_not_the_next_local_copy() {
        use crate::{api::AgentApi, snapshot_tests::common};
        use oink_mobile_core::{ProxyChoice, SendItem, SendOrigin, SharePayload};
        crate::test_watchdog::run(module_path!(), "direct_share_marker_identity", Some(15), async {
            let fixture = mobile_fixture().await;
            let operation = [23; 16];
            fixture
                .session
                .send_share(
                    SharePayload {
                        sensitivity: Sensitivity::Normal,
                        origin: SendOrigin::TransferPage,
                        items: vec![SendItem::Text { text: "AAA".into() }],
                        operation_id: operation,
                        message_id: [24; 16],
                    },
                    ProxyChoice::Automatic,
                )
                .unwrap();
            let mut status = None;
            common::until("direct share settled", || {
                share_status(&fixture.events, operation, &mut status)
            })
            .await;
            assert_eq!(status.as_deref(), Some("Complete"));
            // Inline delivery completes at merge; native publication runs in
            // the clipboard owner after that protocol operation has finished.
            common::until("PC wrote the inline share", || {
                fixture.desktop_clipboard.items() == vec![ClipData::Text("AAA".into())]
            })
            .await;
            assert_eq!(fixture.desktop_clipboard.items(), vec![ClipData::Text("AAA".into())]);
            let shared = fixture.desktop_clipboard.marker();
            let before = fixture.desktop.clipboard_activity().unwrap().len();
            fixture
                .desktop_clipboard
                .copy(vec![ClipData::Text("BBB".into())], Sensitivity::Normal);
            common::until("local copy announced", || {
                fixture.desktop.clipboard_activity().unwrap().len() > before
            })
            .await;
            let copied = fixture.desktop.clipboard_activity().unwrap().len();
            // A synchronizer writes the shared content back, marker included.
            fixture
                .desktop_clipboard
                .copy_with_marker(vec![ClipData::Text("AAA".into())], shared, Sensitivity::Normal);
            common::until("write-back announced as a new copy", || {
                fixture.desktop.clipboard_activity().unwrap().len() > copied
            })
            .await;
            fixture.mobile.stop().await;
            fixture.desktop.shutdown().await;
        });
    }

    /// Phone images on the proxy PC (user 2026-11-12): every image carries
    /// both its file (the local copy the proxy holds) and its bitmap, paired
    /// per image (`Inventory::bitmap_root`); a non-image file has no bitmap.
    #[test]
    fn proxied_photos_publish_each_file_with_its_bitmap() {
        let names = ["a.jpg", "notes.pdf", "b.png", "c.heic"];
        for count in [1, names.len()] {
            let data = tempfile::tempdir().unwrap();
            let path = data.path().join("share-photos");
            let mut items = Vec::new();
            for (id, name) in names.iter().take(count).enumerate() {
                let id = id as u32;
                fs::create_dir_all(path.join(format!("file-{id}"))).unwrap();
                fs::write(path.join(format!("file-{id}")).join(name), b"file").unwrap();
                let image = !name.ends_with(".pdf");
                if image {
                    fs::write(path.join(format!("r-{id}")), b"png").unwrap();
                }
                items.push(Item::File {
                    id,
                    name: (*name).into(),
                    folder: false,
                    bitmap: image.then_some(3),
                });
            }
            let payload = Payload {
                backing: Arc::new(Backing {
                    path: path.clone(),
                    persistent: AtomicBool::new(false),
                }),
                items,
                sender: DeviceId([4; 32]),
                sensitivity: Sensitivity::Normal,
                _upload: None,
            };
            let spec = payload
                .spec(OfferId {
                    clip: ClipId {
                        origin: DeviceId([3; 32]),
                        seq: 1,
                    },
                    generation: 1,
                })
                .unwrap();
            let inventory = &spec.inventory;
            assert_eq!(inventory.roots.len(), count);
            assert_eq!(
                inventory
                    .roots
                    .iter()
                    .map(|root| root.name.as_str())
                    .collect::<Vec<_>>(),
                names[..count]
            );
            // Each image's bitmap names its own file; the document has none.
            let pictures: Vec<_> = (0..inventory.items.len())
                .filter_map(|index| inventory.bitmap_root(index))
                .collect();
            let expected: Vec<_> = inventory
                .roots
                .iter()
                .filter(|root| !root.name.ends_with(".pdf"))
                .map(|root| root.id)
                .collect();
            assert_eq!(pictures, expected, "{:?}", inventory.items);
            // Nothing else is published besides the files and the bitmaps.
            assert_eq!(inventory.items.len(), count + expected.len(), "{:?}", inventory.items);
            // The bitmaps reach every PC, the first one leading (Windows' one bitmap).
            let bitmaps: Vec<_> = inventory
                .wanted(oink_foundation::clipboard::ClipPlatform::Windows)
                .map(|rep| rep.id)
                .collect();
            assert_eq!(bitmaps.len(), expected.len());
            assert_eq!(bitmaps.first(), Some(&RepresentationId(0)));
            assert_eq!(spec.files.len(), count);
            for (id, (entry, name)) in spec.files.iter().zip(names).enumerate() {
                let resolved = path.join(format!("file-{id}")).join(name).canonicalize().unwrap();
                assert_eq!(entry.local_path.as_deref(), Some(resolved.as_path()));
            }
            // The bitmap of image `id` is read from `r-<id>`.
            let mut reader = spec.contents.open(RepresentationId(0)).unwrap();
            assert_eq!(content::read_all(&mut reader).unwrap(), b"png");
        }
    }

    /// 2026-11-12: a phone's text with its HTML and RTF is published as one
    /// item with all three formats, next to a photo without id clashes.
    #[test]
    fn phone_rich_text_publishes_every_format_as_one_item() {
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join("share-rich");
        fs::create_dir_all(path.join("file-1")).unwrap();
        fs::write(path.join("file-1").join("a.jpg"), b"file").unwrap();
        let bodies: [(u32, StandardFormat, &[u8]); 3] = [
            (data_id(0, 0), StandardFormat::Text, b"hi"),
            (data_id(0, 1), StandardFormat::Html, b"<b>hi</b>"),
            (data_id(0, 2), StandardFormat::Rtf, b"{\\rtf1 hi}"),
        ];
        let mut items = Vec::new();
        for (id, format, body) in bodies {
            fs::write(path.join(format!("r-{id}")), body).unwrap();
            items.push(Item::Data {
                id,
                format,
                size: body.len() as u64,
            });
        }
        fs::write(path.join("r-1"), b"png").unwrap();
        items.push(Item::File {
            id: 1,
            name: "a.jpg".into(),
            folder: false,
            bitmap: Some(3),
        });
        assert_eq!(items.iter().map(Item::item).collect::<Vec<_>>(), [0, 0, 0, 1]);
        let payload = Payload {
            backing: Arc::new(Backing {
                path: path.clone(),
                persistent: AtomicBool::new(false),
            }),
            items,
            sender: DeviceId([4; 32]),
            sensitivity: Sensitivity::Normal,
            _upload: None,
        };
        let spec = payload
            .spec(OfferId {
                clip: ClipId {
                    origin: DeviceId([3; 32]),
                    seq: 1,
                },
                generation: 1,
            })
            .unwrap();
        let text = &spec.inventory.items[0];
        assert_eq!(text.id, ItemId(0));
        assert_eq!(
            text.representations
                .iter()
                .map(|rep| (rep.id, rep.standard))
                .collect::<Vec<_>>(),
            [
                (RepresentationId(0), Some(StandardFormat::Text)),
                (RepresentationId(128), Some(StandardFormat::Html)),
                (RepresentationId(192), Some(StandardFormat::Rtf)),
            ]
        );
        assert_eq!(spec.inventory.items.len(), 3, "text, file, bitmap");
        for (id, _, body) in bodies {
            let mut reader = spec.contents.open(RepresentationId(id)).unwrap();
            assert_eq!(content::read_all(&mut reader).unwrap(), body);
        }
        let mut bitmap = spec.contents.open(RepresentationId(1)).unwrap();
        assert_eq!(content::read_all(&mut bitmap).unwrap(), b"png");
    }

    /// 2026-11-12: text the phone sends with its HTML and RTF reaches the
    /// PC's clipboard with all three formats.
    #[test]
    fn phone_rich_text_reaches_the_pc_clipboard_with_every_format() {
        use crate::snapshot_tests::common;
        use oink_mobile_core::{ProxyChoice, SendItem, SendOrigin, SharePayload};
        crate::test_watchdog::run(module_path!(), "phone_rich_text_clipboard", Some(15), async {
            let fixture = mobile_fixture().await;
            let operation = [31; 16];
            fixture
                .session
                .send_share(
                    SharePayload {
                        sensitivity: Sensitivity::Normal,
                        origin: SendOrigin::TransferPage,
                        items: vec![SendItem::text(
                            "rich from phone".into(),
                            Some("<p>rich <b>from</b> phone</p>".into()),
                            Some("{\\rtf1 rich \\b from\\b0  phone}".into()),
                        )],
                        operation_id: operation,
                        message_id: [32; 16],
                    },
                    ProxyChoice::Automatic,
                )
                .unwrap();
            common::until("rich text share sent", || share_finished(&fixture.events, operation)).await;
            common::until("PC wrote every delivered rich text format", || {
                let items = fixture.desktop_clipboard.items();
                items.contains(&ClipData::Text("rich from phone".into()))
                    && items.contains(&ClipData::Html("<p>rich <b>from</b> phone</p>".into()))
                    && items.contains(&ClipData::Rtf(b"{\\rtf1 rich \\b from\\b0  phone}".to_vec()))
            })
            .await;
            let items = fixture.desktop_clipboard.items();
            assert!(items.contains(&ClipData::Text("rich from phone".into())), "{items:?}");
            assert!(
                items.contains(&ClipData::Html("<p>rich <b>from</b> phone</p>".into())),
                "{items:?}"
            );
            assert!(
                items.contains(&ClipData::Rtf(b"{\\rtf1 rich \\b from\\b0  phone}".to_vec())),
                "{items:?}"
            );
            fixture.mobile.stop().await;
            fixture.desktop.shutdown().await;
        });
    }

    #[test]
    fn proxy_payload_sensitivity_survives_restart() {
        for sensitivity in [Sensitivity::Normal, Sensitivity::Concealed, Sensitivity::Transient] {
            let data = tempfile::tempdir().unwrap();
            let proxy = data.path().join(PROXY_CACHE_DIR).join("share-classified");
            fs::create_dir_all(&proxy).unwrap();
            fs::write(proxy.join("r-0"), b"classified").unwrap();
            let pc = DeviceId([3; 32]);
            let group = oink_foundation::GroupId([4; 16]);
            let id = OfferId {
                clip: ClipId { origin: pc, seq: 1 },
                generation: 1,
            };
            let payload = Payload {
                backing: Arc::new(Backing {
                    path: proxy,
                    persistent: AtomicBool::new(false),
                }),
                sender: DeviceId([5; 32]),
                sensitivity,
                items: vec![Item::Data {
                    id: 0,
                    format: StandardFormat::Text,
                    size: 10,
                }],
                _upload: None,
            };
            let spec = payload.spec(id).unwrap();
            assert_eq!(spec.sensitivity, sensitivity);
            let offer = oink_clipboard::sync::Ordering::new(pc, 0)
                .capture(
                    oink_clipboard::wire::inventory(&spec.inventory).unwrap(),
                    vec![],
                    1,
                    spec.sensitivity,
                    1,
                )
                .unwrap()
                .unwrap();
            payload.persist(group, &offer).unwrap();
            let restored = restore(data.path(), group, false);
            assert_eq!(restored.len(), 1);
            assert_eq!(restored[0].2.sensitivity, sensitivity);
        }
    }

    #[test]
    fn sensitive_proxy_preserves_classification_and_purges_large_body_after_head() {
        use oink_foundation::{GroupId, clipboard_history as h};
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let proxy = cache.join(PROXY_CACHE_DIR).join("share-sensitive");
        fs::create_dir_all(&proxy).unwrap();
        fs::write(proxy.join("r-0"), vec![b'x'; 40 * 1024]).unwrap();
        let pc = DeviceId([3; 32]);
        let phone = DeviceId([4; 32]);
        let group = GroupId([5; 16]);
        let id = OfferId {
            clip: ClipId { origin: pc, seq: 1 },
            generation: 1,
        };
        let payload = Payload {
            backing: Arc::new(Backing {
                path: proxy.clone(),
                persistent: AtomicBool::new(false),
            }),
            sender: phone,
            sensitivity: Sensitivity::Concealed,
            items: vec![Item::Data {
                id: 0,
                format: StandardFormat::Text,
                size: 40 * 1024,
            }],
            _upload: None,
        };
        let spec = payload.spec(id).unwrap();
        assert_eq!(spec.sensitivity, Sensitivity::Concealed);
        let mut order = oink_clipboard::sync::Ordering::new(pc, 0);
        let offer = order
            .capture(
                oink_clipboard::wire::inventory(&spec.inventory).unwrap(),
                vec![],
                1,
                spec.sensitivity,
                1,
            )
            .unwrap()
            .unwrap();
        payload.persist(group, &offer).unwrap();
        let restored = restore(&cache, group, false);
        assert_eq!(restored[0].2.sensitivity, Sensitivity::Concealed);
        let store = oink_clipboard::history::Store::load(
            root.path(),
            &cache,
            group,
            pc,
            oink_clipboard::policy::SharingPolicy::new(true),
        )
        .unwrap();
        let registry = Arc::new(content::Registry::default());
        registry.set_history(store.clone());
        let lease = registry.register(spec.contents.clone()).unwrap();
        lease.set_history(offer.clock().unwrap(), true);
        super::super::history::Capture {
            group,
            store: store.clone(),
            registry: registry.clone(),
            offer,
            provider: spec.contents,
            fingerprint: None,
            proxy: Some(proxy.clone()),
        }
        .run()
        .unwrap();
        let head = store.window().records()[0].clone();
        assert!(head.sensitive());
        assert!(
            head.inline.is_empty(),
            "large sensitive body is served separately while current"
        );
        assert_eq!(store.scope(id), h::HolderScope::TEXT);
        let provider = store.provider(id, None).unwrap();
        let mut reader = content::open_portable(provider.as_ref(), RepresentationId(0)).unwrap();
        let mut next = head;
        next.id.clip.seq += 1;
        next.clock += 1;
        next.sensitivity = Sensitivity::Normal;
        for evicted in store.merge(vec![next], vec![]).unwrap() {
            registry.forget_history(evicted);
        }
        assert!(!proxy.exists());
        assert_eq!(store.bytes.bytes(id), 0);
        assert_eq!(
            content::reason(&std::io::Read::read(&mut reader, &mut [0; 1]).unwrap_err()),
            Unavailable::NoSuchItem
        );
        assert!(restore(&cache, group, false).is_empty());
    }

    #[test]
    fn completed_proxy_restores_only_in_its_group() {
        let data = tempfile::tempdir().unwrap();
        let cache = data.path().join(PROXY_CACHE_DIR);
        let path = cache.join("share-test");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("r-0"), b"cached").unwrap();
        let payload = Payload {
            backing: Arc::new(Backing {
                path,
                persistent: AtomicBool::new(false),
            }),
            sender: DeviceId([3; 32]),
            sensitivity: Sensitivity::Normal,
            items: vec![Item::Data {
                id: 0,
                format: StandardFormat::Text,
                size: 6,
            }],
            _upload: None,
        };
        let device = DeviceId([3; 32]);
        let group = oink_foundation::GroupId([4; 16]);
        let id = OfferId {
            clip: ClipId { origin: device, seq: 1 },
            generation: 1,
        };
        let spec = payload.spec(id).unwrap();
        let inventory = oink_clipboard::wire::inventory(&spec.inventory).unwrap();
        let mut order = oink_clipboard::sync::Ordering::new(device, 0);
        let offer = order
            .capture(inventory, vec![], 1, Sensitivity::Normal, 1)
            .unwrap()
            .unwrap();
        payload.persist(group, &offer).unwrap();

        let mut restored = restore(data.path(), group, true);
        let (_, found, spec) = restored.pop().unwrap();
        assert_eq!(found.id(), offer.id());
        let mut reader = spec.contents.open(RepresentationId(0)).unwrap();
        assert_eq!(content::read_all(&mut reader).unwrap(), b"cached");
        drop(reader);
        drop(spec);
        // Leaving a group purges its proxy cache; it cannot be restored again.
        assert!(restore(data.path(), oink_foundation::GroupId([5; 16]), true).is_empty());
        assert!(!cache.join("share-test").exists());
    }

    /// received-files.md §3: a phone's upload is a received file from that
    /// phone. The proxy's native publication names the phone as its origin,
    /// so the platform files it in the phone's own folder, before and after
    /// a restart restores the proxy cache.
    #[test]
    fn phone_upload_is_received_from_the_phone_across_restart() {
        use oink_protocol::v2::schema::{FixedBytes, WireValue};
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join(PROXY_CACHE_DIR).join("share-phone");
        let file = file_path(&path, 0, "photo.jpg");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"jpeg").unwrap();
        let phone = DeviceId([0x5a; 32]);
        let pc = DeviceId([3; 32]);
        let group = oink_foundation::GroupId([4; 16]);
        let payload = Payload {
            backing: Arc::new(Backing {
                path,
                persistent: AtomicBool::new(false),
            }),
            sender: phone,
            sensitivity: Sensitivity::Normal,
            items: vec![Item::File {
                id: 0,
                name: "photo.jpg".into(),
                folder: false,
                bitmap: None,
            }],
            _upload: None,
        };
        let id = OfferId {
            clip: ClipId { origin: pc, seq: 1 },
            generation: 1,
        };
        let origin = |spec: &NativeSpec| {
            spec.file_contents
                .as_ref()
                .and_then(|files| files.namespace())
                .and_then(|files| files.origin())
        };
        let spec = payload.spec(id).unwrap();
        assert_eq!(
            origin(&spec),
            Some(phone),
            "the proxy filed the phone's upload as its own"
        );

        // The persisted offer is the proxy's announcement: real sender phone.
        let inventory = oink_clipboard::wire::inventory(&spec.inventory).unwrap();
        let mut order = oink_clipboard::sync::Ordering::new(pc, 0);
        let mut raw = order
            .capture(inventory, vec![], 1, Sensitivity::Normal, 1)
            .unwrap()
            .unwrap()
            .into_raw();
        raw.insert(8, oink_clipboard::wire::device(phone).write());
        raw.insert(9, oink_clipboard::wire::device(pc).write());
        raw.insert(10, FixedBytes([6; 16]).write());
        raw.insert(11, FixedBytes([7; 16]).write());
        raw.insert(12, 1_u32.write());
        let offer = dto::ClipboardOffer::from_raw(raw).unwrap();
        payload.persist(group, &offer).unwrap();
        let (_, _, restored) = restore(data.path(), group, true).pop().unwrap();
        assert_eq!(origin(&restored), Some(phone), "a restart lost the phone's folder");
    }

    #[test]
    fn folder_upload_expands_before_publication() {
        let data = tempfile::tempdir().unwrap();
        let archive = data.path().join("folder.zip");
        let destination = data.path().join("folder");
        folder_zip(
            &archive,
            &[("sub", None), ("sub/file.txt", Some(b"payload")), ("empty", None)],
        );
        extract_folder(&archive, &destination, &|| true).unwrap();
        assert_eq!(fs::read(destination.join("sub/file.txt")).unwrap(), b"payload");
        assert!(destination.join("empty").is_dir());
    }

    #[test]
    fn folder_upload_rejects_traversal_and_case_aliases() {
        let data = tempfile::tempdir().unwrap();
        let archive = data.path().join("folder.zip");
        let destination = data.path().join("folder");
        folder_zip(&archive, &[("../outside", Some(b"payload"))]);
        assert!(extract_folder(&archive, &destination, &|| true).is_err());
        assert!(!data.path().join("outside").exists());

        let destination = data.path().join("aliases");
        folder_zip(&archive, &[("Text", Some(b"payload")), ("text", Some(b"different"))]);
        assert!(extract_folder(&archive, &destination, &|| true).is_err());
        assert_eq!(fs::read(destination.join("Text")).unwrap(), b"payload");
    }
}
