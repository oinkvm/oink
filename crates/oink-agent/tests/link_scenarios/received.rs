//! Transport/lifecycle model at the NativeFiles boundary. These private ordinary
//! files do not exercise File Provider, Cloud Files, a desktop or OS hydration.
use oink_clipboard::{
    FileMetadata, FileReader,
    content::{
        ReaderGuard,
        linked::Descriptor,
        namespace::{Namespace, NativeFiles, StoredFile},
    },
    model::{ContentSize, DirectoryCursor, DirectoryPage, FileNode, NodeId, NodeKind},
};
use std::{
    collections::HashSet,
    fs::File,
    io::{self, Seek, SeekFrom},
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub struct Received {
    namespace: Arc<dyn Namespace>,
    directory: tempfile::TempDir,
    serial: Mutex<()>,
    nodes: Mutex<Vec<FileNode>>,
    stored: Mutex<Option<Vec<StoredFile>>>,
    publication: Mutex<Option<(oink_foundation::content::OfferId, bool, usize, usize)>>,
    consumers: Arc<std::sync::atomic::AtomicUsize>,
    gate: Option<Arc<ReadGate>>,
}
impl Received {
    pub fn bind(namespace: Arc<dyn Namespace>) -> io::Result<Arc<Self>> {
        Self::bind_with_gate(namespace, None)
    }
    fn bind_with_gate(namespace: Arc<dyn Namespace>, gate: Option<Arc<ReadGate>>) -> io::Result<Arc<Self>> {
        namespace.prepare()?;
        let received = Arc::new(Self {
            nodes: Mutex::new(namespace.roots()),
            namespace: namespace.clone(),
            directory: tempfile::tempdir()?,
            serial: Mutex::new(()),
            stored: Mutex::new(None),
            publication: Mutex::default(),
            consumers: Arc::default(),
            gate,
        });
        let native: Arc<dyn NativeFiles> = received.clone();
        namespace.bind_native(Arc::downgrade(&native))?;
        Ok(received)
    }
    pub fn path(&self, node: NodeId) -> PathBuf {
        self.directory.path().join(node.0.to_string())
    }
    pub fn watchdog_snapshot(&self) -> String {
        format!(
            "native_received publication={:?} nodes={:?} stored={:?} download_lock={:?} consumer_leases={}",
            self.publication.try_lock(),
            self.nodes.try_lock().map(|nodes| nodes.len()),
            self.stored.try_lock().map(|stored| stored.as_ref().map(Vec::len)),
            self.serial.try_lock().map(|_| ()),
            self.consumers.load(std::sync::atomic::Ordering::Acquire)
        )
    }
}
struct ConsumerLease(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for ConsumerLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
impl NativeFiles for Received {
    fn lease(&self) -> io::Result<oink_clipboard::content::namespace::NativeLease> {
        self.consumers.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(Box::new(ConsumerLease(self.consumers.clone())))
    }
    fn persist_link(&self, descriptor: &Descriptor) -> io::Result<()> {
        oink_foundation::storage::atomic_write(
            &self.directory.path().join("source.json"),
            &serde_json::to_vec(descriptor)?,
        )?;
        *self.publication.lock().unwrap() = Some((
            descriptor.offer,
            descriptor.provisional,
            descriptor.roots.len(),
            descriptor.fulfilled.len(),
        ));
        Ok(())
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        if self.stored.lock().unwrap().is_none() {
            return self.namespace.list(node, cursor);
        }
        let nodes = self.nodes.lock().unwrap();
        let version = nodes
            .iter()
            .find(|entry| entry.id == node)
            .ok_or(io::ErrorKind::NotFound)?
            .version;
        if cursor.is_some_and(|cursor| cursor.version != version) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let start = cursor.map_or(0, |cursor| cursor.offset as usize);
        let entries: Vec<_> = nodes
            .iter()
            .filter(|entry| entry.parent == Some(node))
            .cloned()
            .collect();
        if start > entries.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(DirectoryPage {
            directory: node,
            version,
            entries: entries[start..].to_vec(),
            next: None,
        })
    }
    fn open(&self, node: NodeId, offset: u64) -> io::Result<FileReader> {
        let check: ReaderGuard = Arc::new(|| Ok(()));
        self.download(&check, &|_, _| {})?;
        let mut file = File::open(self.path(node))?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(Box::new(file))
    }
    fn metadata(&self, node: NodeId) -> io::Result<FileMetadata> {
        if let Ok(metadata) = std::fs::metadata(self.path(node)) {
            return Ok(FileMetadata {
                exec_bits: None,
                size: metadata.len(),
                modified_ms: None,
            });
        }
        let selected = self.namespace.select(node)?;
        let ContentSize::Known(size) = selected.size else {
            return Err(io::ErrorKind::InvalidData.into());
        };
        Ok(FileMetadata {
            exec_bits: None,
            size,
            modified_ms: selected.modified_ms,
        })
    }
    fn download(
        &self,
        check: &ReaderGuard,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> io::Result<Vec<StoredFile>> {
        let _serial = self.serial.lock().unwrap();
        if let Some(stored) = self.stored.lock().unwrap().clone() {
            return Ok(stored);
        }
        let mut pending = self.namespace.roots();
        let mut nodes = Vec::new();
        let mut stored = Vec::new();
        let mut seen = HashSet::new();
        let mut bytes = 0;
        while let Some(mut node) = pending.pop() {
            check()?;
            if !seen.insert(node.id) || seen.len() > 100_000 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            if node.kind == NodeKind::Directory {
                let mut cursor = None;
                loop {
                    let page = self.namespace.list(node.id, cursor)?;
                    node.version = page.version;
                    pending.extend(page.entries);
                    cursor = page.next;
                    if cursor.is_none() {
                        break;
                    }
                }
            } else {
                node = self.namespace.select(node.id)?;
                let mut source = self.namespace.open(node.id, node.version, 0)?;
                let path = self.path(node.id);
                let partial = path.with_extension("partial");
                let mut output = File::create(&partial)?;
                let mut size = 0;
                let mut buffer = vec![0; 64 * 1024];
                loop {
                    check()?;
                    if let Some(gate) = &self.gate {
                        gate.before_read(size, &path);
                    }
                    let count = source.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    std::io::Write::write_all(&mut output, &buffer[..count])?;
                    size += count as u64;
                    progress(bytes + size, stored.len() as u64);
                }
                output.sync_all()?;
                drop(output);
                if node.size != ContentSize::Known(size) {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                std::fs::rename(partial, path)?;
                bytes += size;
                stored.push(StoredFile {
                    node: node.id,
                    version: node.version,
                    size,
                });
                progress(bytes, stored.len() as u64);
            }
            nodes.push(node);
        }
        oink_foundation::storage::atomic_write(
            &self.directory.path().join("stored.json"),
            &serde_json::to_vec(&stored)?,
        )?;
        *self.nodes.lock().unwrap() = nodes;
        *self.stored.lock().unwrap() = Some(stored.clone());
        Ok(stored)
    }
}

pub struct Clipboard {
    inner: Arc<oink_test_support::clipboard::FakeClipboard>,
    received: Mutex<Vec<Arc<Received>>>,
    gate: Arc<ReadGate>,
}
impl Clipboard {
    pub fn new(inner: Arc<oink_test_support::clipboard::FakeClipboard>, gate: Arc<ReadGate>) -> Self {
        inner.set_virtual_files(true);
        Self {
            inner,
            received: Mutex::default(),
            gate,
        }
    }
}
impl oink_clipboard::ClipboardBackend for Clipboard {
    fn change_count(&self) -> u64 {
        self.inner.change_count()
    }
    fn read(&self) -> Result<oink_clipboard::ClipSnapshot, oink_clipboard::ClipboardError> {
        self.inner.read()
    }
    fn capture(
        &self,
        data: &std::path::Path,
    ) -> Result<oink_clipboard::CapturedClipboard, oink_clipboard::ClipboardError> {
        self.inner.capture(data)
    }
    fn write(
        &self,
        items: &[oink_clipboard::ClipData],
        sensitivity: oink_clipboard::Sensitivity,
    ) -> Result<u64, oink_clipboard::ClipboardError> {
        self.inner.write(items, sensitivity)
    }
    fn watch(&self, handler: oink_clipboard::ChangeHandler) -> Result<(), oink_clipboard::ClipboardError> {
        self.inner.watch(handler)
    }
    fn write_native_guarded(
        &self,
        mut spec: oink_clipboard::NativeSpec,
        check: ReaderGuard,
    ) -> Result<u64, oink_clipboard::ClipboardError> {
        use oink_clipboard::ClipboardError;
        if !spec.files.is_empty() {
            let files = spec.file_contents.as_ref().ok_or(ClipboardError::Unsupported)?;
            let namespace = files.namespace().ok_or(ClipboardError::Unsupported)?;
            let received = Received::bind_with_gate(namespace, Some(self.gate.clone()))
                .map_err(|e| ClipboardError::Platform(e.to_string()))?;
            let nodes = files
                .descriptor_nodes()
                .map_err(|e| ClipboardError::Platform(e.to_string()))?
                .ok_or(ClipboardError::Unsupported)?;
            if nodes.len() != spec.files.len() {
                return Err(ClipboardError::Unsupported);
            }
            for (entry, node) in spec.files.iter_mut().zip(nodes) {
                entry.local_path = Some(received.path(node.id));
            }
            self.received.lock().unwrap().push(received);
        }
        self.inner.write_native_guarded(spec, check)
    }
}

/// Stop between verified linked-file blocks and identify that reader's received
/// copy, independently of later native publications. The persistent grant
/// deliberately bypasses the source clipboard object's transient reader hooks.
pub struct ReadGate {
    after: u64,
    entered: Mutex<Option<std::sync::mpsc::Sender<(u64, PathBuf)>>>,
    resume: Mutex<std::sync::mpsc::Receiver<()>>,
    offsets: Mutex<Vec<u64>>,
}
impl ReadGate {
    pub fn new(
        after: u64,
    ) -> (
        Arc<Self>,
        std::sync::mpsc::Receiver<(u64, PathBuf)>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered, ready) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                after,
                entered: Mutex::new(Some(entered)),
                resume: Mutex::new(resume),
                offsets: Mutex::default(),
            }),
            ready,
            release,
        )
    }
    fn before_read(&self, offset: u64, path: &std::path::Path) {
        self.offsets.lock().unwrap().push(offset);
        let entered = if offset >= self.after {
            self.entered.lock().unwrap().take()
        } else {
            None
        };
        if let Some(entered) = entered {
            let _ = entered.send((offset, path.to_path_buf()));
            // Dropping the sender also unblocks on a test failure.
            let _ = self.resume.lock().unwrap().recv();
        }
    }
    pub fn offsets(&self) -> Vec<u64> {
        self.offsets.lock().unwrap().clone()
    }
}
