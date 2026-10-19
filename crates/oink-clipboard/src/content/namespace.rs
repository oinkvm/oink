//! The file namespace shared by clipboard, File Provider and OLE. Creating an
//! offer stats its roots only. Each first directory request freezes one direct
//! child view; file versions are selected on first content demand.
use super::{Unavailable, unavailable};
use crate::FileReader;
use crate::model::{DirectoryCursor, DirectoryPage, FileNode, NodeId};
use std::{io, sync::Arc};

#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
mod pending;
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
pub(crate) use pending::Pending;
pub(crate) mod local;
pub use local::Local;
mod files;
pub use files::Files;
#[cfg(target_os = "macos")]
pub(crate) use files::Flat;
pub(crate) use files::Names;

pub trait Namespace: Send + Sync + 'static {
    /// Real clipboard paths survive replacement of the system slot. Promise,
    /// virtual and Drop namespaces retain their existing authority rules.
    fn real_clipboard_files(&self) -> bool {
        false
    }
    fn retain_files(&self, _retention: super::transfer::FileRetention) {}
    fn roots(&self) -> Vec<FileNode>;
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage>;
    /// Select the current file version. Real clipboard files may publish a new
    /// version between independent reads; every open/read validates its version.
    fn select(&self, node: NodeId) -> io::Result<FileNode>;
    /// Every reader checks the selected version, including after the last byte.
    /// Offset is a verified block boundary owned by the target's cache.
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader>;
    fn cancel(&self) {}
    fn preparing(&self) {}
    fn in_use(&self) -> bool {
        false
    }
    fn local_batch(&self) -> Option<crate::store::Batch> {
        None
    }
    /// Replace a published remote namespace with a dormant local catalog after
    /// the same observable whole-delivery operation reaches Stored.
    fn on_stored(&self, _ready: Box<dyn FnOnce(super::Completed) + Send>) {}
}

/// Attach files to ordered raw representations without making native format
/// readers responsible for namespace traversal or transfer scheduling.
pub(super) struct WithFiles {
    pub provider: Arc<dyn super::Provider>,
    pub files: Arc<dyn Namespace>,
}
impl super::Provider for WithFiles {
    fn inventory(&self) -> super::Inventory {
        let mut inventory = self.provider.inventory();
        inventory.roots = self.files.roots();
        inventory
    }
    fn open(&self, id: super::RepresentationId) -> io::Result<FileReader> {
        self.provider.open(id)
    }
    fn open_hover(&self, id: super::RepresentationId) -> io::Result<FileReader> {
        self.provider.open_hover(id)
    }
    fn translator(&self) -> Arc<dyn super::Translator> {
        self.provider.translator()
    }
    fn reader_guard(&self, id: super::RepresentationId) -> io::Result<super::ReaderGuard> {
        self.provider.reader_guard(id)
    }
    fn local_batch(&self) -> Option<crate::store::Batch> {
        self.provider.local_batch()
    }
    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        Some(self.files.clone())
    }
    fn cancel(&self) {
        self.files.cancel();
        self.provider.cancel();
    }
}

pub(super) struct Restricted {
    inner: Arc<dyn Namespace>,
    roots: Vec<FileNode>,
    allowed: std::sync::Mutex<std::collections::HashSet<NodeId>>,
}
impl Restricted {
    pub fn new(inner: Arc<dyn Namespace>, roots: Vec<FileNode>) -> Arc<Self> {
        let allowed = roots.iter().map(|root| root.id).collect();
        Arc::new(Self {
            inner,
            roots,
            allowed: std::sync::Mutex::new(allowed),
        })
    }
    fn check(&self, id: NodeId) -> io::Result<()> {
        if self.allowed.lock().unwrap().contains(&id) {
            Ok(())
        } else {
            Err(unavailable(Unavailable::NoSuchItem))
        }
    }
}
impl Namespace for Restricted {
    fn real_clipboard_files(&self) -> bool {
        self.inner.real_clipboard_files()
    }
    fn retain_files(&self, retention: super::transfer::FileRetention) {
        self.inner.retain_files(retention);
    }
    fn roots(&self) -> Vec<FileNode> {
        self.roots.clone()
    }
    fn list(&self, id: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.check(id)?;
        let page = self.inner.list(id, cursor)?;
        page.validate(cursor).map_err(unavailable)?;
        self.allowed
            .lock()
            .unwrap()
            .extend(page.entries.iter().map(|entry| entry.id));
        Ok(page)
    }
    fn select(&self, id: NodeId) -> io::Result<FileNode> {
        self.check(id)?;
        self.inner.select(id)
    }
    fn open(&self, id: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        self.check(id)?;
        self.inner.open(id, version, offset)
    }
    fn cancel(&self) {
        self.inner.cancel();
    }
}

/// Stable, bounded direct-child page, shared by disk and owned-file namespaces.
pub(crate) fn page(
    directory: NodeId,
    version: u64,
    entries: &[FileNode],
    cursor: Option<DirectoryCursor>,
) -> io::Result<DirectoryPage> {
    use crate::model::{DIRECTORY_PAGE_BYTES, DIRECTORY_PAGE_ENTRIES};
    if cursor.is_some_and(|c| c.version != version) {
        return Err(unavailable(Unavailable::Superseded));
    }
    let start = cursor.map_or(0, |c| c.offset as usize);
    if start > entries.len() {
        return Err(unavailable(Unavailable::InvalidData));
    }
    let mut bytes = 64;
    let mut selected = Vec::new();
    for entry in entries.iter().skip(start).take(DIRECTORY_PAGE_ENTRIES) {
        let length = crate::wire::node(entry)
            .and_then(|value| oink_protocol::v1::schema::WireDto::encode(&value, DIRECTORY_PAGE_BYTES))
            .map_err(|_| unavailable(Unavailable::InvalidData))?
            .len();
        if bytes + length > DIRECTORY_PAGE_BYTES {
            break;
        }
        bytes += length;
        selected.push(entry.clone());
    }
    let end = start + selected.len();
    let page = DirectoryPage {
        directory,
        version,
        entries: selected,
        next: (end < entries.len()).then_some(DirectoryCursor {
            version,
            offset: end as u32,
        }),
    };
    page.validate(cursor).map_err(unavailable)?;
    Ok(page)
}
