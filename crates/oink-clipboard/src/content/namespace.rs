//! The file namespace shared by clipboard, File Provider and OLE. Creating an
//! offer stats its roots only. Each first directory request freezes one direct
//! child view; file versions are selected on first content demand.
use super::{Unavailable, unavailable};
use crate::FileReader;
use crate::model::{DirectoryCursor, DirectoryPage, FileNode, NodeId};
use std::{io, sync::Arc};

#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
mod pending;
#[cfg(target_os = "macos")]
pub(crate) use pending::Pending;
pub(crate) mod local;
pub use local::Local;
mod files;
pub use files::Files;
#[cfg(target_os = "macos")]
pub(crate) use files::Flat;
pub(crate) use files::Names;

/// The OS-owned received copy. Source reads use Namespace::open; native
/// stream/promise readers use this copy, which may trigger OS hydration.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoredFile {
    pub node: NodeId,
    pub version: u64,
    pub size: u64,
}
/// Keeps a native consumer alive before its first read and between file reads.
/// Dropping the lease never cancels other consumers or a whole-drop download.
pub type NativeLease = Box<dyn Send + Sync>;
pub trait NativeFiles: Send + Sync + 'static {
    /// Adapter-observed complete target consumption, distinct from hydration or Stored.
    /// A directory means its entire subtree was successfully delivered.
    fn consumed(&self, _nodes: &[NodeId]) -> io::Result<()> {
        Ok(())
    }

    fn lease(&self) -> io::Result<NativeLease> {
        Ok(Box::new(()))
    }
    /// Atomically persist credentials before allowing body reads. Implementors
    /// must not call back into the namespace while holding their catalog lock.
    fn persist_link(&self, descriptor: &super::linked::Descriptor) -> io::Result<()>;
    fn open(&self, node: NodeId, offset: u64) -> io::Result<FileReader>;
    /// Enumerate the current received catalog, including FinalLocal roots.
    fn list(&self, _node: NodeId, _cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        Err(io::ErrorKind::Unsupported.into())
    }
    /// Read the resident link itself without opening its target.
    fn symlink_target(&self, _node: NodeId) -> io::Result<String> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn metadata(&self, node: NodeId) -> io::Result<crate::FileMetadata>;
    fn download(
        &self,
        check: &super::ReaderGuard,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> io::Result<Vec<StoredFile>>;
}

pub trait Namespace: Send + Sync + 'static {
    fn native_consumed(&self, _nodes: &[NodeId]) -> io::Result<()> {
        Ok(())
    }

    /// Native promise/stream lifetime, including a callback that has not started.
    fn native_lease(&self) -> io::Result<NativeLease> {
        Ok(Box::new(()))
    }
    /// Original native file-format metadata, independent of source credentials.
    /// This never reads a payload; prepare may be needed to resolve promise names.
    fn native_file_metadata(&self) -> super::native_metadata::NativeFileMetadata {
        self.source_link()
            .map(|descriptor| descriptor.native)
            .unwrap_or_default()
    }
    /// Current source clipboard identity after any native one-shot rendering.
    /// Drag and ordinary file namespaces have no clipboard ownership.
    fn clipboard_key(&self) -> io::Result<Option<[u8; 32]>> {
        Ok(None)
    }
    /// Remote application streams must read the native received copy.
    fn requires_native_files(&self) -> bool {
        self.source_link().is_some()
    }
    /// Blocking preparation runs on a native/background worker, never a route
    /// executor. No native path may be exposed before this succeeds.
    fn prepare(&self) -> io::Result<()> {
        Ok(())
    }
    fn bind_native(&self, _native: std::sync::Weak<dyn NativeFiles>) -> io::Result<()> {
        Ok(())
    }
    /// None is a local/source namespace; remote publications return Some even
    /// while their native copy is not ready, so streams cannot bypass it.
    fn native_open(&self, _node: NodeId, _offset: u64) -> Option<io::Result<FileReader>> {
        None
    }
    fn native_list(&self, _node: NodeId, _cursor: Option<DirectoryCursor>) -> Option<io::Result<DirectoryPage>> {
        None
    }
    fn native_metadata(&self, _node: NodeId) -> Option<io::Result<crate::FileMetadata>> {
        None
    }
    /// Durable credentials for this native received namespace. This is metadata
    /// only: asking for it must not enumerate or read the remote source.
    fn source_link(&self) -> Option<super::linked::Descriptor> {
        None
    }
    fn source_roots(&self) -> io::Result<Vec<super::grants::SourceRoot>> {
        Err(unavailable(Unavailable::Unsupported))
    }
    /// Roots fulfilled by a native one-shot source. The grant service retains
    /// their complete, immutable bytes independently of the clipboard owner.
    /// This query is metadata-only; select/open perform native fulfilment.
    fn one_shot_roots(&self) -> Vec<NodeId> {
        Vec::new()
    }
    /// The grant journal now owns complete immutable copies of these roots.
    /// Release redundant native capture storage, while preserving metadata and
    /// leases held by readers that were already opened. Never render again.
    fn release_fulfilled(&self, _roots: &[NodeId]) {}
    fn origin(&self) -> Option<oink_foundation::DeviceId> {
        None
    }
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
    /// Temporary accounting protection; must not invalidate the accounting snapshot itself.
    fn cache_batch(&self) -> Option<crate::store::Batch> {
        self.local_batch()
    }
}

/// Attach files to ordered raw representations without making native format
/// readers responsible for namespace traversal or transfer scheduling.
pub(super) struct WithFiles {
    pub provider: Arc<dyn super::Provider>,
    pub files: Arc<dyn Namespace>,
}
impl super::Provider for WithFiles {
    fn inventory(&self) -> super::Inventory {
        self.provider.inventory().with_roots(self.files.roots())
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
    fn resolved_roots(&self) -> io::Result<Vec<FileNode>> {
        let current = self.inner.roots();
        let ids = self.inner.native_file_metadata().resolve_roots(
            &self.roots.iter().map(|root| root.id).collect::<Vec<_>>(),
            &current.iter().map(|root| root.id).collect::<Vec<_>>(),
        )?;
        ids.iter()
            .map(|id| {
                current
                    .iter()
                    .find(|root| root.id == *id)
                    .cloned()
                    .ok_or_else(|| unavailable(Unavailable::InvalidData))
            })
            .collect()
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
    fn release_fulfilled(&self, roots: &[NodeId]) {
        let roots: Vec<_> = roots
            .iter()
            .copied()
            .filter(|id| self.roots().iter().any(|root| root.id == *id))
            .collect();
        self.inner.release_fulfilled(&roots);
    }
    fn native_file_metadata(&self) -> super::native_metadata::NativeFileMetadata {
        self.inner
            .native_file_metadata()
            .restricted(&self.roots().iter().map(|root| root.id).collect::<Vec<_>>())
    }
    fn clipboard_key(&self) -> io::Result<Option<[u8; 32]>> {
        self.inner.clipboard_key()
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        self.inner
            .one_shot_roots()
            .into_iter()
            .filter(|id| self.roots().iter().any(|root| root.id == *id))
            .collect()
    }
    fn requires_native_files(&self) -> bool {
        self.inner.requires_native_files()
    }
    fn prepare(&self) -> io::Result<()> {
        self.inner.prepare()?;
        let roots = self.resolved_roots()?;
        let mut allowed = self.allowed.lock().unwrap();
        for previous in &self.roots {
            if !roots.iter().any(|root| root.id == previous.id) {
                allowed.remove(&previous.id);
            }
        }
        allowed.extend(roots.iter().map(|root| root.id));
        Ok(())
    }
    fn native_lease(&self) -> io::Result<NativeLease> {
        self.inner.native_lease()
    }
    fn native_consumed(&self, nodes: &[NodeId]) -> io::Result<()> {
        self.inner.native_consumed(nodes)
    }
    fn bind_native(&self, native: std::sync::Weak<dyn NativeFiles>) -> io::Result<()> {
        self.inner.bind_native(native)
    }
    fn native_list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> Option<io::Result<DirectoryPage>> {
        if let Err(error) = self.check(node) {
            return Some(Err(error));
        }
        self.inner.native_list(node, cursor).map(|page| {
            let page = page?;
            page.validate(cursor).map_err(unavailable)?;
            self.allowed
                .lock()
                .unwrap()
                .extend(page.entries.iter().map(|entry| entry.id));
            Ok(page)
        })
    }
    fn native_open(&self, node: NodeId, offset: u64) -> Option<io::Result<FileReader>> {
        if let Err(error) = self.check(node) {
            return Some(Err(error));
        }
        self.inner.native_open(node, offset)
    }
    fn native_metadata(&self, node: NodeId) -> Option<io::Result<crate::FileMetadata>> {
        if let Err(error) = self.check(node) {
            return Some(Err(error));
        }
        self.inner.native_metadata(node)
    }
    fn source_link(&self) -> Option<super::linked::Descriptor> {
        self.inner.source_link().map(|mut descriptor| {
            descriptor
                .roots
                .retain(|root| self.roots().iter().any(|entry| entry.id == root.id));
            descriptor.native = self.native_file_metadata();
            descriptor
                .fulfilled
                .retain(|root| self.roots().iter().any(|entry| entry.id == root.node));
            descriptor
        })
    }
    fn source_roots(&self) -> io::Result<Vec<super::grants::SourceRoot>> {
        self.inner.source_roots().map(|roots| {
            roots
                .into_iter()
                .filter(|root| self.roots().iter().any(|node| node.id == root.id()))
                .collect()
        })
    }
    fn origin(&self) -> Option<oink_foundation::DeviceId> {
        self.inner.origin()
    }
    fn real_clipboard_files(&self) -> bool {
        self.inner.real_clipboard_files()
    }
    fn retain_files(&self, retention: super::transfer::FileRetention) {
        self.inner.retain_files(retention);
    }
    fn roots(&self) -> Vec<FileNode> {
        self.resolved_roots().unwrap_or_else(|_| self.roots.clone())
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
