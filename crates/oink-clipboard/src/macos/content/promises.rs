//! Clipboard promises remain capabilities until an authenticated content demand.
//! Real-file companions retain their independent local identity and lifetime.
use super::*;
use crate::content::namespace::Namespace;
use crate::content::native_metadata::{MacPromiseGroup, NativeFileMetadata};
use crate::model::{DirectoryCursor, DirectoryPage, FileNode};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

type Receive = Arc<dyn Fn() -> io::Result<Arc<dyn Provider>> + Send + Sync>;
type Cancel = Arc<dyn Fn() + Send + Sync>;
type ClipboardKey = Arc<dyn Fn() -> io::Result<Option<[u8; 32]>> + Send + Sync>;

#[derive(Clone)]
struct Current(Arc<Inner>);
struct Inner {
    raw: Arc<dyn Provider>,
    real: Option<Arc<dyn Namespace>>,
    real_nodes: Mutex<HashSet<NodeId>>,
    anchors: Vec<NodeId>,
    companions: NativeFileMetadata,
    received: OnceLock<Result<Arc<dyn Provider>, Unavailable>>,
    receive: Receive,
    cancel: Cancel,
    cancelled: Arc<AtomicBool>,
    key: ClipboardKey,
}

pub(super) fn attach(
    raw: Arc<dyn Provider>,
    name: Option<String>,
    count: u64,
    root: PathBuf,
    originals: Vec<(NodeId, PathBuf)>,
) -> Arc<dyn Provider> {
    use crate::macos::dnd::promise::Export;
    let cancelled = Arc::new(AtomicBool::new(false));
    let active = Arc::new(Mutex::new(None::<std::sync::Weak<Export>>));
    let receive_raw = raw.clone();
    let receive_cancelled = cancelled.clone();
    let receive_active = active.clone();
    let key_name = name.clone();
    let receive: Receive = Arc::new(move || {
        let raw = receive_raw.clone();
        let cancelled = receive_cancelled.clone();
        let active = receive_active.clone();
        let name = name.clone();
        let root = root.clone();
        let originals = originals.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        crate::macos::dnd::on_main(move |_| {
            let result = (|| {
                if cancelled.load(Ordering::Acquire) {
                    return Err(content::unavailable(Unavailable::Cancelled));
                }
                let pb = board(name.as_deref());
                if pb.changeCount() as u64 != count {
                    return Err(content::clipboard::changed());
                }
                let export = Export::clipboard(&root, raw, originals, &pb)?;
                *active.lock().unwrap() = Some(Arc::downgrade(&export));
                if cancelled.load(Ordering::Acquire) || pb.changeCount() as u64 != count {
                    export.cancel(false);
                    return Err(content::clipboard::changed());
                }
                Ok(export)
            })();
            if let Err(error) = tx.send(result)
                && let Ok(export) = error.0
            {
                export.cancel(false);
            }
        });
        let export = match rx.recv_timeout(std::time::Duration::from_millis(crate::model::NO_PROGRESS_MS)) {
            Ok(result) => result?,
            Err(_) => {
                receive_cancelled.store(true, Ordering::Release);
                if let Some(export) = receive_active
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                {
                    export.cancel(false);
                }
                return Err(content::unavailable(Unavailable::NoProgress));
            }
        };
        export.ready_provider()
    });
    let cancel: Cancel = Arc::new(move || {
        if let Some(export) = active.lock().unwrap().as_ref().and_then(std::sync::Weak::upgrade) {
            export.cancel(false);
        }
    });
    let key = Arc::new(move || {
        autoreleasepool(|_| {
            if board(key_name.as_deref()).changeCount() as u64 != count {
                return Ok(None);
            }
            crate::macos::clipboard_key::current(key_name.as_deref()).map(Some)
        })
    });
    Arc::new(Current::new(raw, receive, cancel, cancelled, key))
}

impl Current {
    fn new(
        raw: Arc<dyn Provider>,
        receive: Receive,
        cancel: Cancel,
        cancelled: Arc<AtomicBool>,
        key: ClipboardKey,
    ) -> Self {
        let inventory = raw.inventory();
        let mut anchors = Vec::new();
        let mut companions = NativeFileMetadata::default();
        for item in &inventory.items {
            let anchor = item.representations.iter().find_map(|rep| match rep.data {
                RepresentationData::Promise(anchor, _) => Some(anchor),
                _ => None,
            });
            if let Some(anchor) = anchor {
                if let Some(root) = item.file {
                    companions.promises.push(MacPromiseGroup {
                        controls: content::native_metadata::MacPromiseGroup::controls(item),
                        anchor,
                        roots: vec![root],
                        indices: vec![0],
                    });
                } else {
                    anchors.push(anchor);
                }
            }
        }
        Self(Arc::new(Inner {
            real: raw.namespace(),
            real_nodes: Mutex::new(inventory.roots.iter().map(|node| node.id).collect()),
            raw,
            anchors,
            companions,
            received: OnceLock::new(),
            receive,
            cancel,
            cancelled,
            key,
        }))
    }
    fn received(&self) -> Option<&Arc<dyn Provider>> {
        self.0.received.get().and_then(|result| result.as_ref().ok())
    }
    fn files(&self, node: NodeId) -> io::Result<Arc<dyn Namespace>> {
        if self.0.real_nodes.lock().unwrap().contains(&node) {
            return self
                .0
                .real
                .clone()
                .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem));
        }
        if self.0.cancelled.load(Ordering::Acquire) {
            return Err(content::unavailable(Unavailable::Cancelled));
        }
        self.received()
            .and_then(|provider| provider.namespace())
            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))
    }
}
impl Provider for Current {
    fn inventory(&self) -> Inventory {
        self.received().unwrap_or(&self.0.raw).inventory()
    }
    fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
        self.0.raw.open(id)
    }
    fn translator(&self) -> Arc<dyn Translator> {
        self.0.raw.translator()
    }
    fn reader_guard(&self, id: RepresentationId) -> io::Result<content::ReaderGuard> {
        self.0.raw.reader_guard(id)
    }
    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        Some(Arc::new(self.clone()))
    }
    fn cancel(&self) {
        Namespace::cancel(self);
    }
}
impl Namespace for Current {
    fn prepare(&self) -> io::Result<()> {
        if self.0.anchors.is_empty() {
            return Ok(());
        }
        if self.0.cancelled.load(Ordering::Acquire) {
            return Err(content::unavailable(Unavailable::Cancelled));
        }
        self.0
            .received
            .get_or_init(|| {
                if self.0.cancelled.load(Ordering::Acquire) {
                    return Err(Unavailable::Cancelled);
                }
                let provider = (self.0.receive)().map_err(|error| content::reason(&error))?;
                if self.0.cancelled.load(Ordering::Acquire) {
                    return Err(Unavailable::Cancelled);
                }
                Ok(provider)
            })
            .as_ref()
            .map(|_| ())
            .map_err(|reason| content::unavailable(*reason))
    }
    fn roots(&self) -> Vec<FileNode> {
        self.received()
            .and_then(|provider| provider.namespace())
            .or_else(|| self.0.real.clone())
            .map(|files| files.roots())
            .unwrap_or_default()
    }
    fn native_file_metadata(&self) -> NativeFileMetadata {
        self.received()
            .and_then(|provider| provider.namespace())
            .map(|files| files.native_file_metadata())
            .unwrap_or_else(|| self.0.companions.clone())
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        self.received()
            .and_then(|provider| provider.namespace())
            .map(|files| files.one_shot_roots())
            .unwrap_or_else(|| self.0.anchors.clone())
    }
    fn release_fulfilled(&self, roots: &[NodeId]) {
        if let Some(files) = self.received().and_then(|provider| provider.namespace()) {
            files.release_fulfilled(roots);
        }
    }
    fn source_roots(&self) -> io::Result<Vec<content::grants::SourceRoot>> {
        self.0
            .real
            .as_ref()
            .map(|files| files.source_roots())
            .unwrap_or_else(|| Ok(Vec::new()))
    }
    fn clipboard_key(&self) -> io::Result<Option<[u8; 32]>> {
        (self.0.key)()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        let page = self.files(node)?.list(node, cursor)?;
        let mut real = self.0.real_nodes.lock().unwrap();
        if real.contains(&node) {
            real.extend(page.entries.iter().map(|node| node.id));
        }
        Ok(page)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        self.files(node)?.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
        self.files(node)?.open(node, version, offset)
    }
    fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        (self.0.cancel)();
    }
}

#[cfg(all(test, debug_assertions))]
mod tests;
