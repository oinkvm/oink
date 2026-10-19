//! Source-side native promise receipt. Mac targets call in held promises at
//! real Drop. Windows targets export at crossing and publish names immediately;
//! read-ready copies and transfer are independent of the names deadline.
use super::*;
use crate::model::{
    ContentItem, Inventory, ItemId, Medium, NativeFormat, NodeId, Representation, RepresentationData, RepresentationId,
    Unavailable,
};
use crate::{
    content::{
        self, Provider,
        namespace::{Namespace, Pending},
        promise::{Gate, Phase},
    },
    store::{Batch, BatchId, StoreKind},
};
use oink_foundation::dnd::ExportFailure;
use std::{
    fs,
    io::{self, Read, Write},
    sync::atomic::AtomicBool,
};

pub(super) fn receivers(board: &NSPasteboard) -> Option<Vec<Retained<NSFilePromiseReceiver>>> {
    use objc2::ClassType;
    let count = board
        .pasteboardItems()?
        .iter()
        .filter(|item| {
            item.types()
                .iter()
                .any(|ty| super::super::content::promise_type(&ty.to_string()))
        })
        .count();
    if count == 0 || count > 4096 {
        return None;
    }
    let classes = NSArray::from_slice(&[NSFilePromiseReceiver::class()]);
    // SAFETY: the requested native class implements NSPasteboardReading.
    let objects = unsafe { board.readObjectsForClasses_options(&classes, None) }?;
    let receivers: Option<Vec<_>> = objects
        .iter()
        .map(|object| object.downcast::<NSFilePromiseReceiver>().ok())
        .collect();
    receivers.filter(|receivers| receivers.len() == count)
}
pub(super) fn names(receiver: &NSFilePromiseReceiver) -> Vec<String> {
    // AppKit returns nil before receive despite the SDK's nonnull annotation.
    // SAFETY: documented NSArray<NSString> result, with its nullable native ABI.
    let names: Option<Retained<NSArray<NSString>>> = unsafe { msg_send![receiver, fileNames] };
    names
        .map(|names| names.iter().map(|name| name.to_string()).collect())
        .unwrap_or_default()
}

pub(super) struct Export {
    _update_activity: oink_foundation::maintenance::Lease,
    pub(super) operation: DragOperation,
    batch: Batch,
    gate: Mutex<Gate>,
    /// None is a Drop-time proxy; Some bounds Windows name discovery only.
    deadline: Mutex<Option<Instant>>,
    anchors: Vec<NodeId>,
    types: Mutex<Vec<Vec<String>>>,
    announced: AtomicBool,
    originals: Vec<(NodeId, PathBuf)>,
    pending: Arc<Pending>,
    source: Arc<content::Deferred>,
    snapshot: Mutex<Option<Arc<dyn Provider>>>,
    queue: Retained<NSOperationQueue>,
    shared: std::sync::Weak<Shared>,
    pub started: AtomicBool,
    notified: AtomicBool,
    retired: AtomicBool,
}
impl Export {
    pub fn prepare(
        root: &Path,
        operation: DragOperation,
        source: Arc<content::Deferred>,
        originals: Vec<(NodeId, PathBuf)>,
        shared: &Arc<Shared>,
    ) -> io::Result<Arc<Self>> {
        let update_activity = oink_foundation::maintenance::process()
            .enter(oink_foundation::maintenance::ActivityKind::Promise)
            .ok_or(io::ErrorKind::WouldBlock)?;
        let inventory = source.inventory();
        let anchors: Vec<_> = inventory
            .items
            .iter()
            .filter_map(|item| {
                item.representations.iter().find_map(|rep| match rep.data {
                    RepresentationData::Promise(id, _) => Some(id),
                    _ => None,
                })
            })
            .collect();
        if anchors.is_empty() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let batch = Batch::create(root, StoreKind::PromiseExport, BatchId::random()?)?;
        let mut roots = originals.clone();
        for (i, node) in anchors.iter().enumerate() {
            fs::create_dir_all(batch.path().join("incoming").join(i.to_string()))?;
            let path = batch.path().join("ready").join(i.to_string());
            fs::create_dir_all(&path)?;
            roots.push((*node, path));
        }
        let files = content::namespace::Local::new(roots)?;
        let pending = Pending::new(files, anchors.iter().copied(), batch.clone());
        if !source.attach_files(pending.clone()) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let queue = NSOperationQueue::new();
        queue.setMaxConcurrentOperationCount(4);
        Ok(Arc::new(Self {
            _update_activity: update_activity,
            operation,
            batch,
            gate: Mutex::new(Gate::new(anchors.len())),
            deadline: Mutex::new(None),
            anchors,
            types: Mutex::new(Vec::new()),
            announced: AtomicBool::new(false),
            originals,
            pending,
            source,
            snapshot: Mutex::new(None),
            queue,
            shared: Arc::downgrade(shared),
            started: AtomicBool::new(false),
            notified: AtomicBool::new(false),
            retired: AtomicBool::new(false),
        }))
    }
    pub fn requires_export(&self) -> bool {
        self.source.inventory().items.iter().any(|item| {
            item.file.is_none()
                && item
                    .representations
                    .iter()
                    .any(|rep| matches!(rep.data, RepresentationData::Promise(..)))
        })
    }
    pub fn is_boundary(&self) -> bool {
        self.deadline.lock().unwrap().is_some()
    }
    pub fn at_boundary(self: &Arc<Self>, crossing: Instant) -> bool {
        let deadline = crossing + content::promise::LIMIT;
        *self.deadline.lock().unwrap() = Some(deadline);
        if Instant::now() >= deadline {
            self.notify(Err(ExportFailure::Timeout));
            return false;
        }
        let export = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            let expired = {
                let mut gate = export.gate.lock().unwrap();
                (gate.phase == Phase::Prepared || gate.expired(Instant::now())) && gate.abandon()
            };
            if expired {
                export.pending.finish(Err(Unavailable::NoProgress));
                export.notify(Err(ExportFailure::Timeout));
                let _ = export.batch.abandon();
            }
        });
        true
    }
    pub fn cancel(self: &Arc<Self>, notify: bool) {
        if !self.gate.lock().unwrap().abandon() {
            return;
        }
        self.pending.cancel();
        if notify {
            self.notify(Err(ExportFailure::Cancelled));
        }
        let export = self.clone();
        std::thread::spawn(move || {
            let _ = export.batch.abandon();
            if !export.started.load(Ordering::Acquire) {
                let _ = export.batch.external_writers_finished();
                let operation = export.operation;
                on_main(move |_| {
                    UI.with(|ui| {
                        if let Some(ui) = ui.borrow_mut().as_mut() {
                            ui.receivers.remove(&operation.sequence);
                        }
                    })
                });
            }
            export.check();
        });
    }
    fn notify(&self, result: Result<(), ExportFailure>) {
        if self.deadline.lock().unwrap().is_none() || self.notified.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(shared) = self.shared.upgrade() {
            shared.emit(PlatformDndEvent::SourceExported {
                operation: self.operation,
                result,
            });
        }
    }
    pub fn receive(self: &Arc<Self>, receivers: &[Retained<NSFilePromiseReceiver>], board: &NSPasteboard) -> bool {
        if receivers.len() != self.anchors.len() {
            return false;
        }
        let Some(root) = self
            .shared
            .upgrade()
            .and_then(|shared| shared.lock().content_root.clone())
        else {
            return false;
        };
        // Snapshot ordinary bytes while the original native session still owns
        // them. Promise controls are capabilities, so this never fulfils them.
        let snapshot = match super::super::content::snapshot_board(board, &root, self.source.inventory().id) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.source.fail(content::reason(&error));
                return false;
            }
        };
        *self.snapshot.lock().unwrap() = Some(snapshot.clone());
        if !self.source.complete(snapshot) {
            return false;
        }
        let deadline = *self.deadline.lock().unwrap();
        if !self.gate.lock().unwrap().begin_until(deadline) {
            return false;
        }
        if !self.gate.lock().unwrap().live(Instant::now()) {
            self.cancel(true);
            return false;
        }
        // No native writer exists during prepare or snapshotting. Persist the
        // obligation only when we are about to pass a destination to AppKit.
        if self.batch.external_writers_started().is_err() {
            self.cancel(true);
            return false;
        }
        self.started.store(true, Ordering::Release);
        let active: Vec<_> = self
            .source
            .inventory()
            .items
            .iter()
            .filter(|item| {
                item.representations
                    .iter()
                    .any(|rep| matches!(rep.data, RepresentationData::Promise(..)))
            })
            .map(|item| deadline.is_none() || item.file.is_none())
            .collect();
        *self.types.lock().unwrap() = receivers
            .iter()
            .map(|receiver| receiver.fileTypes().iter().map(|ty| ty.to_string()).collect())
            .collect();
        let options = objc2_foundation::NSDictionary::new();
        for (index, receiver) in receivers.iter().enumerate() {
            if !active[index] {
                self.gate.lock().unwrap().skip(index);
                self.check();
                continue;
            }
            let destination = NSURL::fileURLWithPath_isDirectory(
                &NSString::from_str(
                    &self
                        .batch
                        .path()
                        .join("incoming")
                        .join(index.to_string())
                        .to_string_lossy(),
                ),
                true,
            );
            let export = self.clone();
            let reader = block2::RcBlock::new(move |url: std::ptr::NonNull<NSURL>, error: *mut NSError| {
                let name = if error.is_null() {
                    // SAFETY: AppKit owns this URL for the callback duration.
                    unsafe { url.as_ref() }
                        .path()
                        .map(|path| PathBuf::from(path.to_string()))
                        .and_then(|path| export.copy(index, &path).ok())
                } else {
                    None
                };
                export.gate.lock().unwrap().completed(index, name);
                export.check();
                // fileNames may not have been populated when receive returned.
                // Re-read the OS's whole list after the callback; never infer
                // an expected count from the number of callbacks seen so far.
                let names_ready = export.clone();
                on_main(move |_| {
                    let declared = UI.with(|ui| {
                        ui.borrow()
                            .as_ref()
                            .and_then(|ui| ui.receivers.get(&names_ready.operation.sequence))
                            .and_then(|receivers| receivers.get(index))
                            .map(|receiver| names(receiver))
                    });
                    if let Some(declared) = declared.filter(|names| !names.is_empty()) {
                        names_ready.gate.lock().unwrap().names(index, declared);
                        names_ready.check();
                    }
                });
            });
            // SAFETY: AppKit retains the destination, queue, and reader block.
            // Modern readers are coordinated; legacy readers give only the OS's
            // read-ready guarantee. Neither needs a homemade quiescence timer.
            unsafe {
                receiver.receivePromisedFilesAtDestination_options_operationQueue_reader(
                    &destination,
                    &options,
                    &self.queue,
                    &reader,
                );
            }
            let declared = names(receiver);
            if !declared.is_empty() {
                self.gate.lock().unwrap().names(index, declared);
            }
            self.check();
        }
        // fileNames is normally populated when receive returns. Poll AppKit
        // while names are pending because a modern receiver can publish them
        // before its coordinated read-ready callback. Never wait for a writer
        // inside performDragOperation: legacy sources may start after it ends.
        if self.is_boundary() && !self.announced.load(Ordering::Acquire) {
            let export = self.clone();
            std::thread::spawn(move || {
                while !export.announced.load(Ordering::Acquire) && export.gate.lock().unwrap().live(Instant::now()) {
                    let current = export.clone();
                    let active = active.clone();
                    on_main(move |_| {
                        let names = UI.with(|ui| {
                            ui.borrow()
                                .as_ref()
                                .and_then(|ui| ui.receivers.get(&current.operation.sequence))
                                .map(|receivers| receivers.iter().map(|receiver| names(receiver)).collect::<Vec<_>>())
                        });
                        if let Some(names) = names {
                            let mut gate = current.gate.lock().unwrap();
                            for (index, names) in names.into_iter().enumerate() {
                                if active[index] && !names.is_empty() {
                                    gate.names(index, names);
                                }
                            }
                            drop(gate);
                            current.check();
                        }
                    });
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
        }
        true
    }
    fn copy(&self, index: usize, path: &Path) -> io::Result<String> {
        let incoming = self.batch.path().join("incoming").join(index.to_string());
        if path.parent() != Some(incoming.as_path()) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(io::ErrorKind::InvalidData)?
            .to_owned();
        let target = self.batch.path().join("ready").join(index.to_string()).join(&name);
        copy_tree(path, &target, 0, &mut 0, &|| {
            self.gate.lock().unwrap().live(Instant::now()) && !self.pending.failed()
        })?;
        Ok(name)
    }
    fn publish_names(&self) {
        if !self.is_boundary() || self.announced.load(Ordering::Acquire) {
            return;
        }
        if !self.gate.lock().unwrap().names_ready(Instant::now()) || self.announced.swap(true, Ordering::AcqRel) {
            return;
        }
        match self.provider() {
            Ok(provider) => {
                if let Some(shared) = self.shared.upgrade() {
                    let mut state = shared.lock();
                    if state.export.as_ref().is_some_and(|(op, _)| *op == self.operation) {
                        state.exported_data = Some((self.operation, provider));
                    }
                }
                self.notify(Ok(()));
            }
            Err(_) => {
                self.gate.lock().unwrap().abandon();
                self.pending.finish(Err(Unavailable::ReadFailed));
                self.notify(Err(ExportFailure::Failed));
            }
        }
    }
    fn check(self: &Arc<Self>) {
        self.publish_names();
        let (abandoned, finalize, drained) = {
            let mut gate = self.gate.lock().unwrap();
            let finalize = gate.finalize(Instant::now());
            (gate.phase == Phase::Abandoned, finalize, gate.drained())
        };
        if abandoned {
            self.pending.finish(Err(Unavailable::ReadFailed));
            self.notify(Err(ExportFailure::Failed));
        }
        if !drained || self.retired.swap(true, Ordering::AcqRel) {
            return;
        }
        let export = self.clone();
        let barrier = block2::RcBlock::new(move || {
            // Modern callbacks run inside a coordinated read; legacy callbacks
            // carry AppKit's read-ready guarantee. Both have now returned and
            // our independent ready copies are complete. No native reader of
            // incoming remains, so neither kind needs a permanent quarantine.
            let cleared = export.batch.external_writers_finished().is_ok();
            let ready = finalize && cleared && export.gate.lock().unwrap().commit(Instant::now());
            if ready {
                export.pending.finish(Ok(()));
            } else {
                export.pending.finish(Err(Unavailable::ReadFailed));
                export.notify(Err(if export.gate.lock().unwrap().expired(Instant::now()) {
                    ExportFailure::Timeout
                } else {
                    ExportFailure::Failed
                }));
                let _ = export.batch.abandon();
            }
            let operation = export.operation;
            on_main(move |_| {
                UI.with(|ui| {
                    if let Some(ui) = ui.borrow_mut().as_mut() {
                        ui.receivers.remove(&operation.sequence);
                    }
                })
            });
        });
        // SAFETY: the barrier runs after every receiver's callback operation and
        // its surrounding coordinated-read scope have returned.
        unsafe {
            self.queue.addBarrierBlock(&barrier);
        }
    }
    fn provider(&self) -> io::Result<Arc<dyn Provider>> {
        let snapshot = self
            .snapshot
            .lock()
            .unwrap()
            .clone()
            .ok_or(io::ErrorKind::InvalidData)?;
        let mut inventory = snapshot.inventory();
        let names = self.gate.lock().unwrap().receiver_names();
        let mut paths = self.originals.clone();
        let mut roots = content::namespace::Local::new(self.originals.clone())?.roots();
        let types = self.types.lock().unwrap();
        let mut next_node = inventory
            .namespace_roots()
            .iter()
            .map(|node| node.id.0)
            .max()
            .unwrap_or(0)
            + 1;
        let mut next_rep = inventory
            .items
            .iter()
            .flat_map(|item| &item.representations)
            .map(|rep| rep.id.0)
            .max()
            .unwrap_or(0);
        let mut output = Vec::new();
        for mut item in inventory.items {
            if item.file.is_some() {
                item.representations.retain(
                    |rep| !matches!(&rep.format, NativeFormat::Mac(name) if super::super::content::promise_type(name)),
                );
                output.push(item);
                continue;
            }
            let anchor = item.representations.iter().find_map(|rep| match rep.data {
                RepresentationData::Promise(node, _) => Some(node),
                _ => None,
            });
            if let Some(anchor) = anchor {
                let index = self
                    .anchors
                    .iter()
                    .position(|id| *id == anchor)
                    .ok_or(io::ErrorKind::InvalidData)?;
                let mut extras = Vec::new();
                let mut first = None;
                for (position, name) in names[index].iter().enumerate() {
                    let node = NodeId(next_node);
                    next_node += 1;
                    paths.push((node, self.batch.path().join("ready").join(index.to_string()).join(name)));
                    let ty = types.get(index).and_then(|types| types.get(position));
                    let directory = ty.is_some_and(|ty| {
                        use objc2_core_foundation::CFString;
                        // SAFETY: registry-only UTI metadata; no content read.
                        #[allow(deprecated)]
                        unsafe {
                            objc2_core_services::UTTypeConformsTo(
                                &CFString::from_str(ty),
                                &CFString::from_str("public.directory"),
                            )
                        }
                    });
                    roots.push(crate::model::FileNode {
                        id: node,
                        parent: None,
                        name: name.clone(),
                        kind: if directory {
                            crate::model::NodeKind::Directory
                        } else {
                            crate::model::NodeKind::File
                        },
                        size: crate::model::ContentSize::Unknown,
                        version: 0,
                        modified_ms: None,
                    });
                    if first.is_none() {
                        first = Some(node);
                    } else {
                        next_rep += 1;
                        extras.push(file_item(node, RepresentationId(next_rep)));
                    }
                }
                let first = first.ok_or(io::ErrorKind::InvalidData)?;
                item.file = Some(first);
                item.virtual_files = true;
                let mut replaced = false;
                item.representations.retain_mut(|rep| {
                    if super::super::content::promise_type(match &rep.format {
                        NativeFormat::Mac(name) => name,
                        _ => return true,
                    }) {
                        if replaced {
                            return false;
                        }
                        replaced = true;
                        rep.format = NativeFormat::Mac("public.file-url".into());
                        rep.standard = None;
                        rep.data = RepresentationData::File(first);
                    }
                    true
                });
                output.push(item);
                output.extend(extras);
            } else {
                output.push(item);
            }
        }
        for (index, item) in output.iter_mut().enumerate() {
            item.id = ItemId(index as u32);
        }
        let files = Arc::new(PublishedFiles {
            roots: roots.clone(),
            paths,
            pending: self.pending.clone(),
            local: OnceLock::new(),
        });
        inventory.items = output;
        inventory.roots = roots;
        inventory.validate().map_err(content::unavailable)?;
        Ok(Arc::new(Exported {
            inventory,
            snapshot,
            files,
            _batch: self.batch.clone(),
        }))
    }
}
/// Names/unknown sizes are public before AppKit grants read-ready access. The
/// first content or directory request waits for owned copies; it never exposes
/// a growing incoming file or mistakes its current length for the final size.
struct PublishedFiles {
    roots: Vec<crate::model::FileNode>,
    paths: Vec<(NodeId, PathBuf)>,
    pending: Arc<Pending>,
    local: OnceLock<Result<Arc<dyn Namespace>, Unavailable>>,
}
impl PublishedFiles {
    fn files(&self) -> io::Result<&Arc<dyn Namespace>> {
        self.pending.wait()?;
        self.local
            .get_or_init(|| {
                content::namespace::Local::new(self.paths.clone())
                    .map(|files| files as Arc<dyn Namespace>)
                    .map_err(|error| content::reason(&error))
            })
            .as_ref()
            .map_err(|reason| content::unavailable(*reason))
    }
}
impl Namespace for PublishedFiles {
    fn roots(&self) -> Vec<crate::model::FileNode> {
        self.roots.clone()
    }
    fn list(
        &self,
        node: NodeId,
        cursor: Option<crate::model::DirectoryCursor>,
    ) -> io::Result<crate::model::DirectoryPage> {
        self.files()?.list(node, cursor)
    }
    fn select(&self, node: NodeId) -> io::Result<crate::model::FileNode> {
        self.files()?.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
        self.files()?.open(node, version, offset)
    }
    fn cancel(&self) {
        self.pending.cancel();
        if let Some(Ok(files)) = self.local.get() {
            files.cancel();
        }
    }
}
struct Exported {
    inventory: Inventory,
    snapshot: Arc<dyn Provider>,
    files: Arc<dyn Namespace>,
    _batch: Batch,
}
impl Provider for Exported {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
        self.snapshot.open(id)
    }
    fn translator(&self) -> Arc<dyn content::Translator> {
        self.snapshot.translator()
    }
    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        Some(self.files.clone())
    }
    fn cancel(&self) {
        self.snapshot.cancel();
        self.files.cancel();
    }
}
fn file_item(node: NodeId, id: RepresentationId) -> ContentItem {
    ContentItem {
        virtual_files: true,
        id: ItemId(0),
        file: Some(node),
        representations: vec![Representation {
            id,
            format: NativeFormat::Mac("public.file-url".into()),
            medium: Medium::Bytes,
            aspect: 1,
            index: -1,
            standard: None,
            data: RepresentationData::File(node),
        }],
    }
}
fn copy_tree(source: &Path, target: &Path, depth: usize, count: &mut usize, live: &dyn Fn() -> bool) -> io::Result<()> {
    *count += 1;
    if !live() || depth > 256 || *count > crate::model::MAX_DISCOVERED_NODES {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let before = fs::symlink_metadata(source)?;
    if crate::cache::is_link(&before) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    if before.is_dir() {
        fs::create_dir(target)?;
        for child in fs::read_dir(source)? {
            let child = child?;
            copy_tree(&child.path(), &target.join(child.file_name()), depth + 1, count, live)?;
        }
        crate::store::sync_directory(target)?;
    } else if before.is_file() {
        let mut input = fs::File::open(source)?;
        let mut output = fs::OpenOptions::new().write(true).create_new(true).open(target)?;
        let mut buffer = vec![0; 1024 * 1024];
        let mut copied = 0u64;
        loop {
            if !live() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            output.write_all(&buffer[..n])?;
            copied += n as u64;
        }
        output.set_times(fs::FileTimes::new().set_modified(before.modified()?))?;
        output.sync_all()?;
        if copied != before.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
    } else {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let after = fs::symlink_metadata(source)?;
    use std::os::unix::fs::MetadataExt;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.modified()? != after.modified()?
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    fn deferred_promise() -> Arc<content::Deferred> {
        content::Deferred::new(
            Inventory {
                id: oink_foundation::content::OfferId {
                    clip: oink_foundation::content::ClipId {
                        origin: oink_foundation::DeviceId([1; 32]),
                        seq: 1,
                    },
                    generation: 1,
                },
                platform: oink_foundation::clipboard::ClipPlatform::Mac,
                items: vec![ContentItem {
                    virtual_files: false,
                    id: ItemId(0),
                    file: None,
                    representations: vec![Representation {
                        id: RepresentationId(1),
                        format: NativeFormat::Mac("com.apple.pasteboard.promised-file-url".into()),
                        medium: Medium::Bytes,
                        aspect: 1,
                        index: -1,
                        standard: None,
                        data: RepresentationData::Promise(NodeId(10), vec![]),
                    }],
                }],
                roots: vec![],
            },
            Arc::new(super::super::super::content::MacTranslator),
        )
        .unwrap()
    }
    #[test]
    fn windows_names_publish_before_owned_bytes_and_content_reads_wait_for_receipt() {
        let root = tempfile::tempdir().unwrap();
        let backend = MacDnd::new();
        let source = deferred_promise();
        let operation = DragOperation::new();
        let export = Export::prepare(root.path(), operation, source.clone(), vec![], &backend.shared).unwrap();
        backend.shared.lock().export = Some((operation, export.clone()));
        *export.snapshot.lock().unwrap() = Some(source);
        let deadline = Instant::now() + content::promise::LIMIT;
        *export.deadline.lock().unwrap() = Some(deadline);
        export.batch.external_writers_started().unwrap();
        assert!(export.gate.lock().unwrap().begin_until(Some(deadline)));
        export.gate.lock().unwrap().names(0, vec!["attachment.txt".into()]);
        export.check();
        let provider = backend.source_content(operation).unwrap();
        assert!(export.notified.load(Ordering::Acquire));
        assert_eq!(export.gate.lock().unwrap().phase, Phase::Receiving);
        let inventory = provider.inventory();
        assert_eq!(inventory.roots[0].name, "attachment.txt");
        assert_eq!(inventory.roots[0].size, crate::model::ContentSize::Unknown);
        assert_eq!(inventory.roots[0].version, 0);
        let files =
            content::namespace::Files::new(provider.namespace().unwrap(), crate::sync::files::TargetOs::Windows)
                .unwrap();
        assert_eq!(files.descriptor_entries().unwrap().unwrap()[0].path, "attachment.txt");
        assert!(!files.descriptor_size_known(0));
        assert!(!export.batch.path().join("ready/0/attachment.txt").exists());
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let result = files.open(0).and_then(|mut reader| content::read_all(&mut reader));
            send.send(result).unwrap();
        });
        assert!(receive.recv_timeout(Duration::from_millis(30)).is_err());
        let incoming = export.batch.path().join("incoming/0/attachment.txt");
        fs::write(&incoming, b"complete source bytes").unwrap();
        let name = export.copy(0, &incoming).unwrap();
        export.gate.lock().unwrap().completed(0, Some(name));
        export.check();
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(2)).unwrap().unwrap(),
            b"complete source bytes"
        );
        reader.join().unwrap();
    }
    #[test]
    fn cancelling_an_announced_windows_export_wakes_content_readers_and_stops_own_copy() {
        let root = tempfile::tempdir().unwrap();
        let backend = MacDnd::new();
        let source = deferred_promise();
        let export = Export::prepare(
            root.path(),
            DragOperation::new(),
            source.clone(),
            vec![],
            &backend.shared,
        )
        .unwrap();
        *export.snapshot.lock().unwrap() = Some(source);
        assert!(export.gate.lock().unwrap().begin_until(None));
        export.gate.lock().unwrap().names(0, vec!["cancelled.txt".into()]);
        let provider = export.provider().unwrap();
        let files = provider.namespace().unwrap();
        let node = files.roots()[0].id;
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || send.send(files.select(node).err().unwrap().kind()).unwrap());
        provider.cancel();
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            io::ErrorKind::Interrupted
        );
        reader.join().unwrap();
        let incoming = export.batch.path().join("incoming/0/cancelled.txt");
        fs::write(&incoming, b"late native callback").unwrap();
        assert!(export.copy(0, &incoming).is_err());
        assert!(!export.batch.path().join("ready/0/cancelled.txt").exists());
    }
    #[test]
    fn d3_prepare_failure_never_quarantines_an_unstarted_writer() {
        let root = tempfile::tempdir().unwrap();
        let backend = MacDnd::new();
        for missing_root in [false, true] {
            let source = deferred_promise();
            if !missing_root {
                source.fail(Unavailable::Cancelled);
            }
            let originals = if missing_root {
                vec![(NodeId(1), root.path().join("absent"))]
            } else {
                vec![]
            };
            assert!(Export::prepare(root.path(), DragOperation::new(), source, originals, &backend.shared).is_err());
            let (size, _, incomplete) = crate::store::usage(root.path(), StoreKind::PromiseExport, true);
            assert!(!incomplete);
            assert_eq!(size.kept_bytes, 0, "prepare never handed a path to AppKit");
        }
    }
    #[test]
    fn d2_legacy_read_ready_receipt_releases_external_writer_obligation() {
        let root = tempfile::tempdir().unwrap();
        let backend = MacDnd::new();
        let source = deferred_promise();
        let export = Export::prepare(
            root.path(),
            DragOperation::new(),
            source.clone(),
            vec![],
            &backend.shared,
        )
        .unwrap();
        export.batch.external_writers_started().unwrap();
        let path = export.batch.path().to_owned();
        fs::write(path.join("ready/0/attachment.txt"), b"read ready bytes").unwrap();
        assert!(export.gate.lock().unwrap().begin_until(None));
        export.gate.lock().unwrap().names(0, vec!["attachment.txt".into()]);
        export.gate.lock().unwrap().completed(0, Some("attachment.txt".into()));
        export.check();
        source.namespace().unwrap().list(export.anchors[0], None).unwrap();
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(path.join(".oink-content-v1")).unwrap()).unwrap();
        assert_eq!(
            record["external_writers"], false,
            "a legacy success must be reclaimable"
        );
    }
    #[test]
    fn real_file_with_promise_companion_keeps_held_source_on_both_targets() {
        objc2::rc::autoreleasepool(|_| {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("original.png");
            fs::write(&path, b"original image").unwrap();
            let board = NSPasteboard::pasteboardWithUniqueName();
            let item = NSPasteboardItem::new();
            item.setString_forType(
                &NSString::from_str(&super::super::super::pasteboard::file_url_string(&path)),
                &NSString::from_str("public.file-url"),
            );
            item.setString_forType(
                &NSString::from_str("public.png"),
                &NSString::from_str("com.apple.pasteboard.promised-file-url"),
            );
            assert!(board.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)])));
            let operation = DragOperation::new();
            let id = oink_foundation::content::OfferId {
                clip: oink_foundation::content::ClipId {
                    origin: oink_foundation::DeviceId([1; 32]),
                    seq: 1,
                },
                generation: operation.sequence,
            };
            let inventory = super::super::super::content::drag_inventory(&board, id);
            assert!(inventory.has_promises());
            let source =
                content::Deferred::new(inventory, Arc::new(super::super::super::content::MacTranslator)).unwrap();
            let backend = MacDnd::new();
            let export = Export::prepare(
                root.path(),
                operation,
                source,
                vec![(NodeId(1), path.clone())],
                &backend.shared,
            )
            .unwrap();
            backend.shared.lock().export = Some((operation, export));
            for windows_target in [false, true] {
                assert!(!backend.source_requires_export(operation, windows_target));
            }
            assert_eq!(fs::read(&path).unwrap(), b"original image");
            board.clearContents();
        });
    }

    #[test]
    fn source_drop_receipt_preserves_mixed_bytes_and_boundary_projection_uses_real_names() {
        objc2::rc::autoreleasepool(|_| {
            for (image_type, filename) in [
                ("public.jpeg", "photo.jpg"),
                ("public.png", "photo.png"),
                ("public.tiff", "photo.tiff"),
                ("public.svg-image", "photo.svg"),
            ] {
                let root = tempfile::tempdir().unwrap();
                let native = NSPasteboard::pasteboardWithUniqueName();
                let item = NSPasteboardItem::new();
                for (ty, bytes) in [
                    (image_type, b"original image".as_slice()),
                    ("com.apple.pasteboard.promised-file-url", b"source identity".as_slice()),
                    ("com.apple.pasteboard.promised-file-content-type", image_type.as_bytes()),
                    ("example.private", b"unmodified companion".as_slice()),
                ] {
                    item.setData_forType(&NSData::with_bytes(bytes), &NSString::from_str(ty));
                }
                assert!(native.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)])));
                let operation = DragOperation::new();
                let id = oink_foundation::content::OfferId {
                    clip: oink_foundation::content::ClipId {
                        origin: oink_foundation::DeviceId([1; 32]),
                        seq: 1,
                    },
                    generation: operation.sequence,
                };
                let inventory = super::super::super::content::drag_inventory(&native, id);
                let source =
                    content::Deferred::new(inventory.clone(), Arc::new(super::super::super::content::MacTranslator))
                        .unwrap();
                let backend = MacDnd::new();
                backend.shared.lock().content_root = Some(root.path().to_owned());
                let export = Export::prepare(root.path(), operation, source.clone(), vec![], &backend.shared).unwrap();
                assert!(source.inventory().roots.is_empty());
                assert!(!export.is_boundary());
                assert!(!backend.source_requires_export(operation, false));
                backend.shared.lock().export = Some((operation, export.clone()));
                assert!(!backend.source_requires_export(operation, false));
                assert!(backend.source_requires_export(operation, true));
                assert!(!backend.source_requires_export(DragOperation::new(), false));
                let snapshot = super::super::super::content::snapshot_board(&native, root.path(), id).unwrap();
                *export.snapshot.lock().unwrap() = Some(snapshot.clone());
                assert!(source.complete(snapshot));
                // Exercise the production copy/gate/provider boundary after a native
                // read-ready callback, without pretending this is an AppKit drag.
                let incoming = export.batch.path().join("incoming/0").join(filename);
                fs::write(&incoming, b"original image").unwrap();
                assert!(export.gate.lock().unwrap().begin_until(None));
                export.gate.lock().unwrap().names(0, vec![filename.into()]);
                let name = export.copy(0, &incoming).unwrap();
                export.gate.lock().unwrap().completed(0, Some(name));
                export.check();
                // Queue barriers are not NSOperations: wait on the actual consumer
                // readiness condition rather than assuming the queue wait covers it.
                let page = source.namespace().unwrap().list(export.anchors[0], None).unwrap();
                let phase = export.gate.lock().unwrap().phase;
                assert_eq!(phase, Phase::Ready);
                assert_eq!(page.entries[0].name, filename);
                let exported = export.provider().unwrap();
                let projected = exported.inventory();
                assert!(!projected.has_promises());
                assert_eq!(projected.roots[0].name, filename);
                assert!(projected.items[0].file.is_some());
                let image = projected.items[0]
                    .representations
                    .iter()
                    .find(|rep| rep.format == NativeFormat::Mac(image_type.into()))
                    .unwrap();
                assert_eq!(
                    projected
                        .wanted(oink_foundation::clipboard::ClipPlatform::Windows)
                        .any(|rep| rep.id == image.id),
                    image.standard.is_some()
                );
                let raw = projected.items[0]
                    .representations
                    .iter()
                    .find(|rep| rep.format == NativeFormat::Mac("example.private".into()))
                    .unwrap();
                assert_eq!(
                    content::read_all(&mut exported.open(raw.id).unwrap()).unwrap(),
                    b"unmodified companion"
                );
                // Exercise the same native target writer path after registration
                // returns its domain URL; no installed domain or drag is involved.
                let provider_path = root.path().join("provider-domain").join(filename);
                let files =
                    content::namespace::Files::new(exported.namespace().unwrap(), crate::sync::files::TargetOs::MacOs)
                        .unwrap();
                let entries = files.roots().to_vec();
                let spec = DragSpec {
                    operation: DragOperation::new(),
                    at: Point::new(0., 0.),
                    items: content::items(&projected),
                    files: Some(files.clone()),
                    ops: DragOps::COPY,
                    native: Some(crate::NativeSpec {
                        inventory: projected,
                        contents: exported,
                        files: entries,
                        file_contents: Some(files),
                        sensitivity: crate::Sensitivity::Normal,
                    }),
                };
                let items = dragging_items(&spec, None, Some(vec![provider_path.clone()]), &[]);
                assert_eq!(items.len(), 1);
                let writer = items[0].item().downcast::<NSPasteboardItem>().unwrap();
                let board = NSPasteboard::pasteboardWithUniqueName();
                assert!(board.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(writer)])));
                let target = board.pasteboardItems().unwrap().objectAtIndex(0);
                assert_eq!(super::super::super::content::file_path(&target), Some(provider_path));
                assert!(
                    !target
                        .types()
                        .iter()
                        .any(|ty| super::super::super::content::promise_type(&ty.to_string()))
                );
                assert_eq!(
                    target.dataForType(&NSString::from_str(image_type)).unwrap().to_vec(),
                    b"original image"
                );
                assert_eq!(
                    target.types().iter().map(|ty| ty.to_string()).collect::<Vec<_>>(),
                    [image_type, "public.file-url", "example.private"]
                );
                board.clearContents();
                native.clearContents();
            }
        });
    }
    #[test]
    fn coordinated_export_snapshot_copies_whole_tree_without_links_or_source_mutation() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir_all(source.join("empty")).unwrap();
        fs::write(source.join("file"), b"unchanged original").unwrap();
        let target = root.path().join("copy");
        copy_tree(&source, &target, 0, &mut 0, &|| true).unwrap();
        assert!(target.join("empty").is_dir());
        fs::write(target.join("file"), b"edited copy").unwrap();
        assert_eq!(fs::read(source.join("file")).unwrap(), b"unchanged original");
        std::os::unix::fs::symlink(source.join("file"), source.join("link")).unwrap();
        assert!(copy_tree(&source, &root.path().join("rejected"), 0, &mut 0, &|| true).is_err());
    }
    #[test]
    fn cancellation_during_snapshot_stops_copying_and_cannot_publish_partial_file() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("copy");
        fs::write(&source, vec![29; 3 * 1024 * 1024]).unwrap();
        let calls = AtomicUsize::new(0);
        let result = copy_tree(&source, &target, 0, &mut 0, &|| {
            calls.fetch_add(1, Ordering::SeqCst) < 2
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(fs::metadata(&target).unwrap().len() < fs::metadata(&source).unwrap().len());
        assert_eq!(fs::read(&source).unwrap(), vec![29; 3 * 1024 * 1024]);
    }
}
