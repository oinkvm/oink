//! Ordered native representations shared by clipboard and held drag providers.
//! Inventory is metadata; a reader owns the source independently of native UI.
use crate::model::{
    ContentSize, Inventory, REPRESENTATION_BYTES, Representation, RepresentationData, RepresentationId, Unavailable,
};
use crate::{
    ClipData, FileReader,
    store::{Batch, BatchId, StoreKind},
};
use oink_foundation::clipboard::ClipPlatform;
use oink_foundation::content::OfferId;
use std::{
    collections::HashMap,
    io::{self, Read},
    path::Path,
    sync::{Arc, Mutex, Weak},
};

pub mod announced;
pub(crate) mod clipboard;
mod deferred;
pub use deferred::Deferred;
mod progress;
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
pub(crate) mod promise;
mod remote;
pub use progress::{Phase as TransferPhase, Progress as TransferProgress};
pub mod grants;
pub mod linked;
pub mod namespace;
pub mod native_metadata;
pub mod native_read;
mod snapshot;
mod transfer;
pub(crate) mod url;
pub use remote::Remote;
pub use snapshot::Snapshot;
pub use transfer::{FileRetention, Registry, SourceLease, ThumbnailOutcome};

/// Converts a native representation's bytes. Sibling metadata uses the same
/// guarded source provider. Same-platform readers do not invoke this interface.
pub trait Translator: Send + Sync + 'static {
    fn standard(&self, representation: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>>;
    /// Converters may consult guarded sibling metadata, such as a Windows text
    /// representation's CF_LOCALE. Existing translators need no source context.
    fn standard_with_source(
        &self,
        representation: &Representation,
        bytes: &[u8],
        _source: &dyn Provider,
    ) -> io::Result<Vec<ClipData>> {
        self.standard(representation, bytes)
    }
}

/// Clipboard providers validate/read the current system slot; drag providers may
/// own a Drop snapshot. Native objects stay on their owning apartment/thread.
pub type ReaderGuard = Arc<dyn Fn() -> io::Result<()> + Send + Sync>;

pub(crate) fn guarded(reader: FileReader, check: ReaderGuard) -> io::Result<FileReader> {
    check()?;
    struct Checked {
        reader: FileReader,
        check: ReaderGuard,
    }
    impl Read for Checked {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            (self.check)()?;
            let size = self.reader.read(bytes)?;
            (self.check)()?;
            Ok(size)
        }
    }
    // A clipboard-generation guard may return semantic Interrupted after open.
    // Every consumer (including cache io::copy/read_to_end) must see a terminal
    // content error rather than retry it forever as a transient OS interrupt.
    Ok(terminal_reader(Box::new(Checked { reader, check })))
}

/// Content cancellation and a changed selected version are terminal failures,
/// even though their domain IO mapping uses Interrupted. std::Read::read_exact
/// and native IStream adapters retry Interrupted as OS EINTR; exposing a content
/// failure with that kind could spin forever or combine bytes from two versions.
/// Preserve the typed reason and leave actual operating-system interrupts alone.
pub(crate) fn terminal_reader(reader: FileReader) -> FileReader {
    struct Terminal(FileReader);
    impl Read for Terminal {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.0.read(bytes).map_err(|error| {
                let failure = error
                    .get_ref()
                    .and_then(|error| error.downcast_ref::<ContentError>())
                    .map(|error| error.0);
                if let Some(failure @ (Unavailable::Superseded | Unavailable::Cancelled)) = failure {
                    io::Error::other(ContentError(failure))
                } else {
                    error
                }
            })
        }
    }
    Box::new(Terminal(reader))
}

pub trait Provider: Send + Sync + 'static {
    fn inventory(&self) -> Inventory;
    fn open(&self, representation: RepresentationId) -> io::Result<FileReader>;
    /// Read an active provider without authorizing or waiting for native Drop.
    fn open_hover(&self, representation: RepresentationId) -> io::Result<FileReader> {
        self.open(representation)
    }
    /// Optional bounded hover read for artwork preparation. A preview must not
    /// inherit the much longer transfer watchdog or wait for native Drop.
    fn open_preview(&self, _representation: RepresentationId, _timeout: std::time::Duration) -> io::Result<FileReader> {
        Err(unavailable(Unavailable::Unsupported))
    }
    fn translator(&self) -> Arc<dyn Translator>;
    /// Preserve a provider's live authority when inline or translated bytes
    /// create a new reader without opening the original storage again.
    fn reader_guard(&self, _representation: RepresentationId) -> io::Result<ReaderGuard> {
        Ok(Arc::new(|| Ok(())))
    }
    /// Durable local bytes, available only after open succeeds. Native TYMED_FILE
    /// media retain this cache pin independently of clipboard ownership.
    fn local_batch(&self) -> Option<Batch> {
        None
    }
    fn namespace(&self) -> Option<Arc<dyn namespace::Namespace>> {
        None
    }
    fn cancel(&self) {}
}

pub fn items(inventory: &Inventory) -> Vec<ClipData> {
    let mut items: Vec<_> = inventory
        .items
        .iter()
        .map(|item| ClipData::Native {
            platform: inventory.platform,
            item: item.clone(),
        })
        .collect();
    if !inventory.roots.is_empty() {
        items.push(ClipData::FileRoots(inventory.roots.clone()));
    }
    items
}
pub fn inventory(id: OfferId, data: &[ClipData]) -> io::Result<Option<Inventory>> {
    let mut found = None;
    let mut items = Vec::new();
    let mut roots = None;
    for data in data {
        if let ClipData::FileRoots(entries) = data
            && roots.replace(entries.clone()).is_some()
        {
            return Err(unavailable(Unavailable::InvalidData));
        }
        if let ClipData::Native { platform, item } = data {
            if found.is_some_and(|p| p != *platform) {
                return Err(unavailable(Unavailable::InvalidData));
            }
            found = Some(*platform);
            items.push(item.clone());
        }
    }
    let Some(platform) = found else {
        return Ok(None);
    };
    let inventory = Inventory {
        id,
        platform,
        items,
        roots: roots.unwrap_or_default(),
    };
    inventory.validate().map_err(unavailable)?;
    Ok(Some(inventory))
}

struct Identified {
    provider: Arc<dyn Provider>,
    inventory: Inventory,
    files: Option<Arc<dyn namespace::Namespace>>,
}
impl Provider for Identified {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<FileReader> {
        if !self
            .inventory
            .items
            .iter()
            .flat_map(|item| &item.representations)
            .any(|rep| rep.id == id && !matches!(rep.data, RepresentationData::Unavailable(_)))
        {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        self.provider.open(id)
    }
    fn open_hover(&self, id: RepresentationId) -> io::Result<FileReader> {
        if find(&self.inventory, id).is_none_or(|rep| matches!(rep.data, RepresentationData::Unavailable(_))) {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        self.provider.open_hover(id)
    }
    fn translator(&self) -> Arc<dyn Translator> {
        self.provider.translator()
    }
    fn reader_guard(&self, id: RepresentationId) -> io::Result<ReaderGuard> {
        if find(&self.inventory, id).is_none_or(|rep| matches!(rep.data, RepresentationData::Unavailable(_))) {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        self.provider.reader_guard(id)
    }
    fn local_batch(&self) -> Option<Batch> {
        self.provider.local_batch()
    }
    fn namespace(&self) -> Option<Arc<dyn namespace::Namespace>> {
        self.files.clone()
    }
    fn cancel(&self) {
        self.provider.cancel();
    }
}
pub fn identified(provider: Arc<dyn Provider>, id: OfferId) -> Arc<dyn Provider> {
    let mut inventory = provider.inventory();
    inventory.id = id;
    let files = provider.namespace();
    Arc::new(Identified {
        provider,
        inventory,
        files,
    })
}

pub fn with_files(provider: Arc<dyn Provider>, files: Arc<dyn namespace::Namespace>) -> Arc<dyn Provider> {
    Arc::new(namespace::WithFiles { provider, files })
}

/// Settings projection is also the source's serving authority, not only an
/// announcement filter: guessing an omitted representation ID grants nothing.
pub fn projected(provider: Arc<dyn Provider>, inventory: Inventory) -> io::Result<Arc<dyn Provider>> {
    inventory.validate().map_err(unavailable)?;
    let source = provider.inventory();
    if inventory.platform != source.platform
        || inventory.roots.iter().any(|root| !source.roots.contains(root))
        || inventory
            .items
            .iter()
            .flat_map(|item| &item.representations)
            .any(|rep| !find(&source, rep.id).is_some_and(|original| original == rep))
    {
        return Err(unavailable(Unavailable::InvalidData));
    }
    let files = if inventory.namespace_roots().is_empty() {
        None
    } else {
        Some(namespace::Restricted::new(
            provider
                .namespace()
                .ok_or_else(|| unavailable(Unavailable::InvalidData))?,
            inventory.namespace_roots(),
        ) as Arc<dyn namespace::Namespace>)
    };
    Ok(Arc::new(Identified {
        provider,
        inventory,
        files,
    }))
}

/// Serve an already announced offer from a newer, equivalent clipboard.
///
/// A synchronizer that writes our own announced content back replaces the
/// native slot the announced readers were bound to. That write is an echo
/// (`crate::dedupe`) and is not announced again, so peers keep reading the
/// original offer. Each announced representation is served by the newer
/// clipboard's representation with the same native format at the same item
/// position; a representation without such a counterpart (formats the
/// synchronizer dropped) becomes unavailable. File offers are not rebound:
/// their namespace belongs to the original copy.
pub fn rebound(advertised: Inventory, actual: Arc<dyn Provider>) -> io::Result<Arc<dyn Provider>> {
    let current = actual.inventory();
    if current.platform != advertised.platform
        || !advertised.roots.is_empty()
        || !current.roots.is_empty()
        || advertised.items.iter().any(|item| item.file.is_some())
    {
        return Err(unavailable(Unavailable::Unsupported));
    }
    let mut inventory = advertised;
    let mut map = HashMap::new();
    for (index, item) in inventory.items.iter_mut().enumerate() {
        for rep in &mut item.representations {
            let counterpart = current.items.get(index).and_then(|candidate| {
                candidate.representations.iter().find(|other| {
                    other.format == rep.format
                        && !matches!(
                            other.data,
                            RepresentationData::Unavailable(_)
                                | RepresentationData::File(_)
                                | RepresentationData::Promise(..)
                        )
                        // A peer verifies an announced size against the bytes it reads.
                        && match (rep.data.size(), other.data.size()) {
                            (Some(ContentSize::Known(a)), Some(ContentSize::Known(b))) => a == b,
                            (Some(ContentSize::Known(_)), _) => false,
                            _ => true,
                        }
                })
            });
            match counterpart {
                Some(other) if !matches!(rep.data, RepresentationData::Promise(..) | RepresentationData::File(_)) => {
                    map.insert(rep.id, other.id);
                }
                _ => rep.data = RepresentationData::Unavailable(Unavailable::Superseded),
            }
        }
    }
    if map.is_empty() {
        return Err(unavailable(Unavailable::Superseded));
    }
    inventory.validate().map_err(unavailable)?;
    Ok(Arc::new(Rebound { inventory, map, actual }))
}
struct Rebound {
    inventory: Inventory,
    map: HashMap<RepresentationId, RepresentationId>,
    actual: Arc<dyn Provider>,
}
impl Rebound {
    fn mapped(&self, id: RepresentationId) -> io::Result<RepresentationId> {
        self.map
            .get(&id)
            .copied()
            .ok_or_else(|| unavailable(Unavailable::Superseded))
    }
}
impl Provider for Rebound {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<FileReader> {
        self.actual.open(self.mapped(id)?)
    }
    fn open_hover(&self, id: RepresentationId) -> io::Result<FileReader> {
        self.actual.open_hover(self.mapped(id)?)
    }
    fn translator(&self) -> Arc<dyn Translator> {
        self.actual.translator()
    }
    fn reader_guard(&self, id: RepresentationId) -> io::Result<ReaderGuard> {
        self.actual.reader_guard(self.mapped(id)?)
    }
    fn cancel(&self) {
        self.actual.cancel();
    }
}

#[derive(Debug)]
struct ContentError(Unavailable);
impl std::fmt::Display for ContentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("content unavailable")
    }
}
impl std::error::Error for ContentError {}
pub fn unavailable(reason: Unavailable) -> io::Error {
    io::Error::new(
        match reason {
            Unavailable::NoSuchItem => io::ErrorKind::NotFound,
            Unavailable::Cancelled | Unavailable::Superseded => io::ErrorKind::Interrupted,
            Unavailable::Permission => io::ErrorKind::PermissionDenied,
            Unavailable::Unsupported => io::ErrorKind::Unsupported,
            Unavailable::LimitExceeded => io::ErrorKind::FileTooLarge,
            Unavailable::NoProgress => io::ErrorKind::TimedOut,
            Unavailable::StorageFull => io::ErrorKind::StorageFull,
            _ => io::ErrorKind::InvalidData,
        },
        ContentError(reason),
    )
}
pub fn reason(error: &io::Error) -> Unavailable {
    if let Some(reason) = error.get_ref().and_then(|error| error.downcast_ref::<ContentError>()) {
        return reason.0;
    }
    match error.kind() {
        io::ErrorKind::NotFound => Unavailable::NoSuchItem,
        io::ErrorKind::PermissionDenied => Unavailable::Permission,
        io::ErrorKind::Unsupported => Unavailable::Unsupported,
        io::ErrorKind::FileTooLarge => Unavailable::LimitExceeded,
        io::ErrorKind::TimedOut => Unavailable::NoProgress,
        io::ErrorKind::StorageFull => Unavailable::StorageFull,
        io::ErrorKind::Interrupted => Unavailable::Superseded,
        _ => Unavailable::ReadFailed,
    }
}

pub fn find(inventory: &Inventory, id: RepresentationId) -> Option<&Representation> {
    inventory
        .items
        .iter()
        .flat_map(|i| &i.representations)
        .find(|r| r.id == id)
}

fn portable_representation(
    inventory: &Inventory,
    id: RepresentationId,
) -> Option<(
    oink_foundation::content::ItemId,
    &Representation,
    crate::model::StandardFormat,
)> {
    use crate::model::StandardFormat;
    inventory.items.iter().find_map(|item| {
        if item.file.is_some() && !item.virtual_files {
            return None;
        }
        let rep = item.representations.iter().find(|rep| rep.id == id)?;
        let format = rep.standard.filter(|format| *format != StandardFormat::Privacy)?;
        matches!(
            rep.data,
            RepresentationData::Inline(_) | RepresentationData::Reader { .. }
        )
        .then_some((item.id, rep, format))
    })
}

/// The representation a phone presents for `item`, by the phone's own rule
/// (oink-mobile-core `v2_session::portable_format`): the first portable
/// representation of the best kind, a bitmap before plain text, a URL, HTML
/// and RTF. A phone presents an offer with file roots by its roots instead.
pub(crate) fn phone_representation(item: &crate::model::ContentItem) -> Option<&Representation> {
    use crate::model::StandardFormat;
    let rank = |format: StandardFormat| match format {
        StandardFormat::Png => Some(0u8),
        StandardFormat::Text => Some(1),
        StandardFormat::Url => Some(2),
        StandardFormat::Html => Some(3),
        StandardFormat::Rtf => Some(4),
        StandardFormat::Privacy => None,
    };
    item.representations
        .iter()
        .filter(|rep| {
            matches!(
                rep.data,
                RepresentationData::Inline(_) | RepresentationData::Reader { .. }
            )
        })
        .filter_map(|rep| Some((rank(rep.standard?)?, rep)))
        // The first of equally ranked representations, like the phone.
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, rep)| rep)
}

/// Every representation a phone fetches for `item` (2026-11-12, phones keep
/// rich text): the one it presents (`phone_representation`) and, when that is
/// plain text, the item's rich text next to it, HTML before RTF (the first of
/// each, like the phone's `rich_format`). Hosts write both to the phone
/// clipboard and render the rich one in the conversation.
pub(crate) fn phone_representations(item: &crate::model::ContentItem) -> Vec<&Representation> {
    use crate::model::StandardFormat;
    let Some(primary) = phone_representation(item) else {
        return Vec::new();
    };
    let mut wanted = vec![primary];
    if primary.standard == Some(StandardFormat::Text) {
        let readable = |rep: &&Representation| {
            matches!(
                rep.data,
                RepresentationData::Inline(_) | RepresentationData::Reader { .. }
            )
        };
        let first = |format| {
            item.representations
                .iter()
                .filter(readable)
                .find(|rep| rep.standard == Some(format))
        };
        wanted.extend(first(StandardFormat::Html).or_else(|| first(StandardFormat::Rtf)));
    }
    wanted
}

/// Bound memory before any native HGLOBAL/NSData or conversion result is copied.
/// Streaming readers use the same limit and check EOF, including exact limits.
pub fn read_all(reader: &mut dyn Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(REPRESENTATION_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > REPRESENTATION_BYTES {
        return Err(unavailable(Unavailable::LimitExceeded));
    }
    Ok(bytes)
}

/// Same-system bytes remain untouched. Foreign payloads contain exactly the
/// declared portable format bytes, selected by the offered representation.
/// Canonical portable bytes regardless of source operating system.
pub fn open_portable(provider: &dyn Provider, id: RepresentationId) -> io::Result<FileReader> {
    open_for_demand(provider, id, None, false)
}
pub fn open_for(provider: &dyn Provider, id: RepresentationId, target: ClipPlatform) -> io::Result<FileReader> {
    open_for_demand(provider, id, Some(target), false)
}

fn open_for_demand(
    provider: &dyn Provider,
    id: RepresentationId,
    target: Option<ClipPlatform>,
    hover: bool,
) -> io::Result<FileReader> {
    use crate::copy_diagnostics::{self as diagnostic, Channel, Stage};
    let inventory = provider.inventory();
    let mut trace = diagnostic::Read::begin(
        inventory.id,
        Stage::SourceRead,
        id,
        diagnostic::format(find(&inventory, id).and_then(|rep| rep.standard)),
        Channel::Native,
    );
    let result = open_for_inventory(provider, &inventory, id, target, hover);
    if let Err(error) = &result {
        trace.failure(error);
    }
    // The reader reports actual bytes and final errors through EOF, including
    // same-platform streams which are intentionally not materialized here.
    result.map(|reader| {
        Box::new(DiagnosticReader {
            reader,
            trace,
            bytes: 0,
        }) as FileReader
    })
}
struct DiagnosticReader {
    reader: FileReader,
    trace: crate::copy_diagnostics::Read,
    bytes: u64,
}
impl Read for DiagnosticReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.reader.read(buffer) {
            Ok(size) => {
                self.bytes += size as u64;
                self.trace.success(self.bytes);
                Ok(size)
            }
            Err(error) => {
                self.trace.failure(&error);
                Err(error)
            }
        }
    }
}
fn native_png(format: &crate::model::NativeFormat) -> bool {
    use crate::model::NativeFormat;
    match format {
        NativeFormat::Mac(name) => name == "public.png",
        NativeFormat::WindowsRegistered(name) => {
            name.eq_ignore_ascii_case("PNG")
                || name.split(';').next().is_some_and(|mime| {
                    matches!(mime.trim().to_ascii_lowercase().as_str(), "image/png" | "image/x-png")
                })
        }
        _ => false,
    }
}
fn open_for_inventory(
    provider: &dyn Provider,
    inventory: &Inventory,
    id: RepresentationId,
    target: Option<ClipPlatform>,
    hover: bool,
) -> io::Result<FileReader> {
    let rep = match target {
        Some(target) => inventory.wanted(target).find(|rep| rep.id == id),
        None => portable_representation(inventory, id).map(|(_, rep, _)| rep),
    }
    .ok_or_else(|| unavailable(Unavailable::Unsupported))?;
    let same_platform = target == Some(inventory.platform);
    // A DIB/TIFF and an existing PNG on the same item describe the same image.
    // Prefer lossless native PNG only for the portable contract, retaining raw
    // same-platform order and identity guards. Invalid PNG can fall back; source
    // expiry or a timed-out native callback cannot restart through a sibling.
    if !same_platform
        && rep.standard == Some(crate::model::StandardFormat::Png)
        && !native_png(&rep.format)
        && let Some(item) = inventory
            .items
            .iter()
            .find(|item| item.representations.iter().any(|r| r.id == id))
    {
        for candidate in item
            .representations
            .iter()
            .filter(|r| r.standard == rep.standard && native_png(&r.format))
        {
            match open_for_demand(provider, candidate.id, target, hover) {
                Ok(reader) => return Ok(reader),
                Err(error)
                    if matches!(
                        reason(&error),
                        Unavailable::Superseded | Unavailable::Cancelled | Unavailable::NoProgress
                    ) =>
                {
                    return Err(error);
                }
                Err(_) => {}
            }
        }
    }
    let guard = provider.reader_guard(id)?;
    guard()?;
    let mut reader = match &rep.data {
        RepresentationData::Inline(bytes) => Box::new(io::Cursor::new(bytes.clone())) as FileReader,
        RepresentationData::Reader { .. } => {
            if hover {
                provider.open_hover(id)?
            } else {
                provider.open(id)?
            }
        }
        _ => return Err(unavailable(Unavailable::NoSuchItem)),
    };
    reader = guarded(reader, guard.clone())?;
    if same_platform {
        return Ok(reader);
    }
    if rep.standard == Some(crate::model::StandardFormat::Url) {
        let bytes = url::read(&mut reader)?;
        let encoded = url::capture(provider, inventory, rep, &bytes, hover)?.encode()?;
        return guarded(Box::new(io::Cursor::new(encoded)), guard);
    }
    let bytes = read_all(&mut reader)?;
    let mut standard = provider.translator().standard_with_source(rep, &bytes, provider)?;
    if standard.len() != 1 {
        return Err(unavailable(Unavailable::Unsupported));
    }
    // Portable contracts contain their declared bytes, never a serialised local
    // ClipData enum. The offered descriptor selects the decoder at the receiver.
    use crate::model::StandardFormat;
    let encoded = match (rep.standard, standard.pop().unwrap()) {
        (Some(StandardFormat::Text), ClipData::Text(text)) | (Some(StandardFormat::Html), ClipData::Html(text)) => {
            text.into_bytes()
        }
        (Some(StandardFormat::Rtf), ClipData::Rtf(bytes)) | (Some(StandardFormat::Png), ClipData::Png(bytes)) => bytes,
        (Some(StandardFormat::Privacy), ClipData::Custom { platform, name, bytes })
            if crate::formats::is_marker(platform, &name) =>
        {
            bytes
        }
        _ => return Err(unavailable(Unavailable::Unsupported)),
    };
    if encoded.len() as u64 > REPRESENTATION_BYTES {
        return Err(unavailable(Unavailable::LimitExceeded));
    }
    guarded(Box::new(io::Cursor::new(encoded)), guard)
}

/// The selected portable format supplies the exact byte contract. Foreign data
/// cannot inject a local enum variant, file path or private target format.
pub fn decode_standard(representation: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
    use crate::model::{NativeFormat, StandardFormat};
    if bytes.len() as u64 > REPRESENTATION_BYTES {
        return Err(unavailable(Unavailable::LimitExceeded));
    }
    let text = || String::from_utf8(bytes.to_vec()).map_err(|_| unavailable(Unavailable::InvalidData));
    let data = match representation.standard {
        Some(StandardFormat::Text) => ClipData::Text(text()?),
        Some(StandardFormat::Html) => ClipData::Html(text()?),
        Some(StandardFormat::Rtf) => ClipData::Rtf(bytes.to_vec()),
        Some(StandardFormat::Png) => ClipData::Png(bytes.to_vec()),
        Some(StandardFormat::Privacy) => {
            let (platform, name) = match &representation.format {
                NativeFormat::Mac(name) => (oink_foundation::clipboard::ClipPlatform::Mac, name),
                NativeFormat::WindowsRegistered(name) => (oink_foundation::clipboard::ClipPlatform::Windows, name),
                _ => return Err(unavailable(Unavailable::Unsupported)),
            };
            if !crate::formats::is_marker(platform, name) {
                return Err(unavailable(Unavailable::Unsupported));
            }
            crate::formats::custom(platform, name, bytes.to_vec())
        }
        Some(StandardFormat::Url) | None => return Err(unavailable(Unavailable::Unsupported)),
    };
    Ok(vec![data])
}

#[cfg(all(test, debug_assertions))]
pub(crate) mod tests;
