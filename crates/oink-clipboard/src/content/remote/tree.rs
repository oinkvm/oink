use super::*;
use crate::content::namespace::{Namespace, NativeFiles};
use crate::model::{DirectoryCursor, DirectoryPage, FileNode, NodeId};

impl Remote {
    fn files(&self) -> io::Result<&super::super::linked::Publication> {
        self.0
            .files
            .as_ref()
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))
    }
    fn active_files(&self) -> io::Result<super::super::linked::Linked> {
        let files = self.files()?.ready()?;
        if files.descriptor().provisional {
            return Err(unavailable(Unavailable::Permission));
        }
        Ok(files)
    }
}
impl Namespace for Remote {
    fn native_lease(&self) -> io::Result<crate::content::namespace::NativeLease> {
        self.files()?.native()?.lease()
    }
    fn native_consumed(&self, nodes: &[crate::model::NodeId]) -> io::Result<()> {
        self.files()?.native()?.consumed(nodes)
    }

    fn native_file_metadata(&self) -> crate::content::native_metadata::NativeFileMetadata {
        self.source_link()
            .map(|descriptor| descriptor.native)
            .unwrap_or_default()
    }
    fn requires_native_files(&self) -> bool {
        true
    }
    fn prepare(&self) -> io::Result<()> {
        self.files()?.ready().map(|_| ())
    }
    fn source_link(&self) -> Option<super::super::linked::Descriptor> {
        self.0.files.as_ref()?.descriptor()
    }
    fn bind_native(&self, native: Weak<dyn NativeFiles>) -> io::Result<()> {
        self.files()?.bind_native(native)
    }
    fn native_list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> Option<io::Result<DirectoryPage>> {
        match self.files().and_then(|files| files.native()) {
            Ok(native) => Some((|| {
                self.active_files()?;
                native.list(node, cursor)
            })()),
            // Descriptor preparation can precede native publication.
            Err(_) => None,
        }
    }
    fn native_open(&self, node: NodeId, offset: u64) -> Option<io::Result<FileReader>> {
        Some((|| {
            self.active_files()?;
            if matches!(*self.0.demand.lock().unwrap(), Demand::Drop(_)) {
                self.start_delivery();
            }
            self.files()?.native()?.open(node, offset)
        })())
    }
    fn native_metadata(&self, node: NodeId) -> Option<io::Result<crate::FileMetadata>> {
        match self.files().and_then(|files| files.native()) {
            Ok(native) => Some(native.metadata(node)),
            // Metadata preparation precedes creation of the native copy.
            Err(_) => None,
        }
    }
    fn origin(&self) -> Option<oink_foundation::DeviceId> {
        Some(self.0.inventory.id.clip.origin)
    }
    fn roots(&self) -> Vec<FileNode> {
        self.0
            .files
            .as_ref()
            .map_or_else(|| self.0.inventory.roots.clone(), |files| files.roots())
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.files()?.ready()?.list(node, cursor)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        self.files()?.ready()?.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        self.active_files()?.open(node, version, offset)
    }
    fn cancel(&self) {
        Remote::cancel(self);
    }
    fn preparing(&self) {
        if self.0.terminal.load(Ordering::Acquire) == 0 {
            self.update_progress(false, |p| {
                p.phase = super::super::TransferPhase::Preparing;
            });
        }
    }
    fn in_use(&self) -> bool {
        self.0.delivery_started.load(Ordering::Acquire)
            && self.receipt().is_none()
            && !self.0.cancelled.load(Ordering::Acquire)
    }
}
