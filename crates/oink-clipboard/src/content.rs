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

pub(crate) mod clipboard;
mod completed;
mod deferred;
mod manifest;
pub use completed::Completed;
pub(crate) use completed::batch_delivered;
pub use deferred::Deferred;
mod progress;
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
pub(crate) mod promise;
mod remote;
pub use progress::{Phase as TransferPhase, Progress as TransferProgress};
pub mod namespace;
mod snapshot;
mod transfer;
pub(crate) mod url;
pub use remote::Remote;
pub use snapshot::Snapshot;
pub use transfer::{FileRetention, Registry, SourceLease};

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
    Ok(Box::new(Checked { reader, check }))
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
pub fn open_for(provider: &dyn Provider, id: RepresentationId, target: ClipPlatform) -> io::Result<FileReader> {
    open_for_demand(provider, id, target, false)
}

fn open_for_demand(
    provider: &dyn Provider,
    id: RepresentationId,
    target: ClipPlatform,
    hover: bool,
) -> io::Result<FileReader> {
    let inventory = provider.inventory();
    let rep = inventory
        .wanted(target)
        .find(|r| r.id == id)
        .ok_or_else(|| unavailable(Unavailable::Unsupported))?;
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
    if target == inventory.platform {
        return Ok(reader);
    }
    if rep.standard == Some(crate::model::StandardFormat::Url) {
        let bytes = url::read(&mut reader)?;
        let encoded = url::capture(provider, &inventory, rep, &bytes, hover)?.encode()?;
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
