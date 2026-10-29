//! The current system clipboard, never a source-side content snapshot.
use super::*;
use crate::model::{DirectoryCursor, DirectoryPage, FileNode, NodeId};
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) type Check = Arc<dyn Fn() -> io::Result<()> + Send + Sync>;
type ReadRepresentation = Box<dyn Fn(RepresentationId) -> io::Result<FileReader> + Send + Sync>;

pub(crate) struct Clipboard {
    inventory: Inventory,
    read: ReadRepresentation,
    translator: Arc<dyn Translator>,
    files: Option<Arc<dyn namespace::Namespace>>,
    failed: AtomicBool,
}
impl Clipboard {
    pub fn new(
        inventory: Inventory,
        read: ReadRepresentation,
        check: Check,
        translator: Arc<dyn Translator>,
        files: Option<Arc<dyn namespace::Namespace>>,
    ) -> io::Result<Arc<Self>> {
        inventory.validate().map_err(unavailable)?;
        Ok(Arc::new(Self {
            inventory,
            read,
            translator,
            files: files.map(|inner| {
                // Real paths are identity-checked on disk, independently of the
                // current clipboard slot. Virtual files still need that slot.
                if inner.real_clipboard_files() {
                    inner
                } else {
                    Arc::new(CurrentFiles { inner, check }) as Arc<dyn namespace::Namespace>
                }
            }),
            failed: AtomicBool::new(false),
        }))
    }
}
impl Provider for Clipboard {
    fn inventory(&self) -> Inventory {
        let mut inventory = self.inventory.clone();
        if let Some(files) = &self.files {
            inventory = inventory.with_roots(files.roots());
        }
        inventory
    }
    fn open(&self, id: RepresentationId) -> io::Result<FileReader> {
        let rep = find(&self.inventory, id).ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        if let RepresentationData::Unavailable(reason) = rep.data {
            return Err(unavailable(reason));
        }
        let result = (self.read)(id);
        if let Err(error) = &result
            && !self.failed.swap(true, Ordering::AcqRel)
        {
            let reason = if reason(error) == Unavailable::Superseded {
                "source_clipboard_changed"
            } else {
                "source_clipboard_unavailable"
            };
            tracing::warn!(reason, "system clipboard read failed");
        }
        result
    }
    fn translator(&self) -> Arc<dyn Translator> {
        self.translator.clone()
    }
    fn namespace(&self) -> Option<Arc<dyn namespace::Namespace>> {
        self.files.clone()
    }
}

pub(crate) fn changed() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "source clipboard changed")
}

struct CurrentFiles {
    inner: Arc<dyn namespace::Namespace>,
    check: Check,
}
impl namespace::Namespace for CurrentFiles {
    fn native_file_metadata(&self) -> super::native_metadata::NativeFileMetadata {
        if (self.check)().is_ok() {
            self.inner.native_file_metadata()
        } else {
            Default::default()
        }
    }
    fn clipboard_key(&self) -> io::Result<Option<[u8; 32]>> {
        (self.check)()?;
        let key = self.inner.clipboard_key()?;
        (self.check)()?;
        Ok(key)
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        self.inner.one_shot_roots()
    }
    fn release_fulfilled(&self, roots: &[NodeId]) {
        self.inner.release_fulfilled(roots);
    }
    fn prepare(&self) -> io::Result<()> {
        (self.check)()?;
        self.inner.prepare()?;
        (self.check)()
    }
    fn cancel(&self) {
        self.inner.cancel();
    }
    fn roots(&self) -> Vec<FileNode> {
        self.inner.roots()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        (self.check)()?;
        let page = self.inner.list(node, cursor)?;
        (self.check)()?;
        Ok(page)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        (self.check)()?;
        let node = self.inner.select(node)?;
        (self.check)()?;
        Ok(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        (self.check)()?;
        let reader = self.inner.open(node, version, offset)?;
        (self.check)()?;
        Ok(Box::new(CurrentReader {
            reader,
            check: self.check.clone(),
        }))
    }
}
struct CurrentReader {
    reader: FileReader,
    check: Check,
}
impl Read for CurrentReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        (self.check)()?;
        let n = self.reader.read(bytes)?;
        (self.check)()?;
        Ok(n)
    }
}
