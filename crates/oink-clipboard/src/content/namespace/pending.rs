//! A promise container is a file-service capability, never a native filename.
//! Its directory cannot be enumerated until the OS receiver has made every
//! promised file read-ready. Ordinary file roots keep their existing namespace.
use super::*;
use std::{
    collections::HashSet,
    sync::{Condvar, Mutex},
};

pub(crate) struct Pending {
    inner: Arc<dyn Namespace>,
    containers: HashSet<NodeId>,
    ready: Mutex<Option<Result<(), Unavailable>>>,
    changed: Condvar,
    _batch: crate::store::Batch,
}
impl Pending {
    pub fn new(
        inner: Arc<dyn Namespace>,
        containers: impl IntoIterator<Item = NodeId>,
        batch: crate::store::Batch,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            containers: containers.into_iter().collect(),
            ready: Mutex::new(None),
            changed: Condvar::new(),
            _batch: batch,
        })
    }
    pub fn finish(&self, result: Result<(), Unavailable>) -> bool {
        let mut ready = self.ready.lock().unwrap();
        if ready.is_some() {
            return false;
        }
        *ready = Some(result);
        self.changed.notify_all();
        true
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn failed(&self) -> bool {
        matches!(*self.ready.lock().unwrap(), Some(Err(_)))
    }
    pub(crate) fn wait(&self) -> io::Result<()> {
        let mut ready = self.ready.lock().unwrap();
        while ready.is_none() {
            ready = self.changed.wait(ready).unwrap();
        }
        ready.unwrap().map_err(unavailable)
    }
}
impl Namespace for Pending {
    fn origin(&self) -> Option<oink_foundation::DeviceId> {
        self.inner.origin()
    }
    fn roots(&self) -> Vec<FileNode> {
        self.inner
            .roots()
            .into_iter()
            .filter(|node| !self.containers.contains(&node.id))
            .collect()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        if self.containers.contains(&node) {
            self.wait()?;
        }
        self.inner.list(node, cursor)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        self.inner.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        self.inner.open(node, version, offset)
    }
    fn cancel(&self) {
        self.finish(Err(Unavailable::Cancelled));
        self.inner.cancel();
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn deferred_names_are_hidden_until_os_completion_and_cancel_is_terminal() {
        let data = tempfile::tempdir().unwrap();
        let batch = crate::store::Batch::create(
            data.path(),
            crate::store::StoreKind::PromiseExport,
            crate::store::BatchId::random().unwrap(),
        )
        .unwrap();
        let path = batch.path().join("_promise");
        std::fs::create_dir(&path).unwrap();
        let inner = super::Local::new(vec![(NodeId(100), path.clone())]).unwrap();
        let pending = Pending::new(inner, [NodeId(100)], batch);
        assert!(pending.roots().is_empty());
        // Only the OS completion authorizes freezing a directory view; an
        // initially empty directory must not be mistaken for an empty promise.
        std::fs::write(path.join("original.jpg"), b"complete bytes").unwrap();
        assert!(pending.finish(Ok(())));
        let page = pending.list(NodeId(100), None).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].name, "original.jpg");
        assert!(!pending.finish(Err(Unavailable::Cancelled)));
        pending.cancel();
        assert!(pending.list(NodeId(100), None).is_err());
    }
}
