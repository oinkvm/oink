//! Source-side native promise receipt. Both targets export at crossing and
//! publish real names alongside the original promise formats. Read-ready copies
//! and transfer are independent of the names deadline.
use super::*;
mod capture;
mod copy;
pub(super) mod timing;
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
use capture::Capture;
use copy::{CopyError, CopyReason, copy_tree};
use oink_foundation::dnd::ExportFailure;
use std::{fs, io, sync::atomic::AtomicBool};

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

pub(in crate::macos) struct Export {
    _update_activity: oink_foundation::maintenance::Lease,
    pub(super) operation: Option<DragOperation>,
    root: PathBuf,
    created: Instant,
    batch: Capture,
    gate: Mutex<Gate>,
    /// None is a Drop-time receipt; Some bounds crossing-time name discovery.
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
thread_local! {
    static RECEIVERS: RefCell<std::collections::HashMap<PathBuf, Vec<Retained<NSFilePromiseReceiver>>>> = RefCell::default();
}
impl Export {
    fn stage(&self, stage: u8, index: usize, status: i64) {
        tracing::info!(target: "oink_file_diagnostics", file_stage = stage,
            file_request = self.operation.map_or(0, |operation| operation.sequence),
            file_index = index, file_status = status,
            elapsed_us = self.created.elapsed().as_micros() as u64,
            "native promise export stage");
    }
    pub(super) fn prepare(
        root: &Path,
        operation: DragOperation,
        source: Arc<content::Deferred>,
        originals: Vec<(NodeId, PathBuf)>,
        shared: &Arc<Shared>,
    ) -> io::Result<Arc<Self>> {
        Self::prepare_inner(root, Some(operation), source, originals, Arc::downgrade(shared))
    }
    pub(in crate::macos) fn clipboard(
        root: &Path,
        raw: Arc<dyn Provider>,
        originals: Vec<(NodeId, PathBuf)>,
        board: &NSPasteboard,
    ) -> io::Result<Arc<Self>> {
        let receivers = receivers(board).ok_or_else(|| content::unavailable(Unavailable::Unsupported))?;
        let source = content::Deferred::new(raw.inventory(), raw.translator())?;
        let export = Self::prepare_inner(root, None, source, originals, std::sync::Weak::new())?;
        if !export.receive_snapshot(&receivers, raw) {
            export.cancel(false);
            return Err(content::unavailable(Unavailable::ReadFailed));
        }
        Ok(export)
    }
    pub(in crate::macos) fn ready_provider(self: &Arc<Self>) -> io::Result<Arc<dyn Provider>> {
        if let Err(error) = self
            .pending
            .wait_until(Instant::now() + Duration::from_millis(crate::model::NO_PROGRESS_MS))
        {
            self.cancel(false);
            return Err(error);
        }
        let provider = self.provider()?;
        provider.namespace().ok_or(io::ErrorKind::InvalidData)?.prepare()?;
        Ok(provider)
    }
    fn prepare_inner(
        root: &Path,
        operation: Option<DragOperation>,
        source: Arc<content::Deferred>,
        originals: Vec<(NodeId, PathBuf)>,
        shared: std::sync::Weak<Shared>,
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
        fs::create_dir_all(batch.path().join("incoming"))?;
        for (i, node) in anchors.iter().enumerate() {
            let path = batch.path().join("ready").join(i.to_string());
            fs::create_dir_all(&path)?;
            roots.push((*node, path));
        }
        let files = content::namespace::Local::new(roots)?;
        let pending = Pending::new(files, anchors.iter().copied(), batch.clone());
        let batch = Capture::new(batch);
        if !source.attach_files(pending.clone()) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let queue = NSOperationQueue::new();
        queue.setMaxConcurrentOperationCount(4);
        Ok(Arc::new(Self {
            _update_activity: update_activity,
            created: Instant::now(),
            operation,
            root: root.to_owned(),
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
            shared,
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
        self.stage(60, 0, 0);
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
                export.release_receivers();
            }
            export.check();
        });
    }
    // SourceExported completes the crossing's name publication exactly once.
    // Later read-ready failures belong to Pending/CloudLinkUnavailable; sending
    // a second SourceExported cannot complete an already-consumed export epoch.
    fn notify(&self, result: Result<(), ExportFailure>) {
        if self.deadline.lock().unwrap().is_none() || self.notified.swap(true, Ordering::AcqRel) {
            return;
        }
        if let (Some(shared), Some(operation)) = (self.shared.upgrade(), self.operation) {
            shared.emit(PlatformDndEvent::SourceExported { operation, result });
        }
    }
    pub fn receive(self: &Arc<Self>, receivers: &[Retained<NSFilePromiseReceiver>], board: &NSPasteboard) -> bool {
        if receivers.len() != self.anchors.len() {
            return false;
        }
        // Snapshot ordinary bytes while the original native session still owns
        // them. Promise controls are capabilities, so this never fulfils them.
        let snapshot = match super::super::content::snapshot_board(board, &self.root, self.source.inventory().id) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.source.fail(content::reason(&error));
                return false;
            }
        };
        self.receive_snapshot(receivers, snapshot)
    }
    fn receive_snapshot(
        self: &Arc<Self>,
        receivers: &[Retained<NSFilePromiseReceiver>],
        snapshot: Arc<dyn Provider>,
    ) -> bool {
        if receivers.len() != self.anchors.len() {
            return false;
        }
        RECEIVERS.with(|owned| {
            owned
                .borrow_mut()
                .insert(self.batch.path().to_owned(), receivers.to_vec());
        });
        *self.snapshot.lock().unwrap() = Some(snapshot.clone());
        if !self.source.complete(snapshot) {
            return false;
        }
        self.stage(61, receivers.len(), 0);
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
        let timing = timing::Trace::new(self.operation.map_or(0, |operation| operation.sequence), self.created);
        timing::observe(self, timing.clone());
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
        // Use one native fulfilment destination for this drag. On macOS 14.7.8,
        // changing it between receivers throws "Could not get filename from
        // URL:" on the second receive and AppKit cancels the original drag.
        // Rebuilding receivers at Drop and postponing fileNames do not help.
        // Share only this batch's incoming directory; each read-ready copy
        // remains isolated in ready/<receiver>. The source is never moved.
        // Native shared-destination probes passed on macOS 14.7.8 and 26.6.2.
        for (index, receiver) in receivers.iter().enumerate() {
            if !active[index] {
                self.gate.lock().unwrap().skip(index);
                self.check();
                continue;
            }
            let destination = NSURL::fileURLWithPath_isDirectory(
                &NSString::from_str(&self.batch.path().join("incoming").to_string_lossy()),
                true,
            );
            let export = self.clone();
            let reader_timing = timing.clone();
            let reader = block2::RcBlock::new(move |url: std::ptr::NonNull<NSURL>, error: *mut NSError| {
                reader_timing.event(timing::Event::Reader, index, Duration::ZERO, 0);
                // NSError details may contain source paths; retain only its code.
                let status = if error.is_null() {
                    0
                } else {
                    // SAFETY: AppKit owns this non-null NSError for this callback.
                    unsafe { &*error }.code() as i64
                };
                export.stage(63, index, status);
                let path = error
                    .is_null()
                    .then(|| {
                        // SAFETY: AppKit owns this URL throughout a successful reader callback.
                        unsafe { url.as_ref() }.path()
                    })
                    .flatten()
                    .map(|path| PathBuf::from(path.to_string()));
                let metadata = path.as_ref().and_then(|path| fs::symlink_metadata(path).ok());
                let result = if !error.is_null() {
                    Err(CopyError::new(
                        CopyReason::NativeReader,
                        content::unavailable(Unavailable::ReadFailed),
                    ))
                } else {
                    path.as_deref()
                        .ok_or_else(|| CopyError::invalid(CopyReason::InvalidUrl))
                        .and_then(|path| export.copy(index, path))
                };
                let (name, reason, os_code) = match result {
                    Ok(name) => (Some(name), CopyReason::Success, 0),
                    Err(error) => {
                        // Publish the definitive content error before waking namespace
                        // requests. Names may already have been announced successfully.
                        export.pending.finish(Err(error.unavailable()));
                        (None, error.reason, error.error.raw_os_error().unwrap_or(0))
                    }
                };
                tracing::info!(target: "oink_file_diagnostics", file_stage = 64_u8,
                    file_request = export.operation.map_or(0, |operation| operation.sequence),
                    file_index = index, file_status = i64::from(name.is_none()),
                    file_copy_reason = reason as u8, file_os_code = os_code,
                    file_size = metadata.as_ref().map(|meta| meta.len()),
                    file_kind = metadata.as_ref().map(|meta| if meta.is_file() { 1_u8 } else if meta.is_dir() { 2 } else if meta.is_symlink() { 3 } else { 4 }),
                    file_name_matches = path.as_ref().and_then(|path| path.file_name()).and_then(|name| name.to_str())
                        .map(|name| export.gate.lock().unwrap().receiver_names()[index].iter().any(|expected| expected == name)),
                    elapsed_us = export.created.elapsed().as_micros() as u64,
                    "native promise copy completed");
                export.gate.lock().unwrap().completed(index, name);
                export.check();
                // fileNames may not have been populated when receive returned.
                // Re-read the OS's whole list after the callback; never infer
                // an expected count from the number of callbacks seen so far.
                let names_ready = export.clone();
                on_main(move |_| {
                    let declared = RECEIVERS.with(|owned| {
                        owned
                            .borrow()
                            .get(names_ready.batch.path())
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
            // Modern readers are coordinated; the SDK documents a readiness
            // heuristic for legacy sources. Check the copy's identity/size/mtime
            // and report any mutation; an arbitrary quiet timer proves no EOF.
            // macOS 14.7.8 can assert while registering a receiver. Keep the
            // Objective-C unwind inside this call so it cannot bypass our gate
            // and surface later as a misleading names timeout. A submission can
            // already have started a writer: cancel preserves its quarantine.
            timing.event(timing::Event::ReceiveEnter, index, Duration::ZERO, 0);
            let returned = std::cell::Cell::new(false);
            let submitted = Instant::now();
            // SAFETY: AppKit retains the owned destination, queue and reader block.
            let received = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
                receiver.receivePromisedFilesAtDestination_options_operationQueue_reader(
                    &destination,
                    &options,
                    &self.queue,
                    &reader,
                );
                timing.event(timing::Event::ReceiveReturn, index, submitted.elapsed(), 0);
                returned.set(true);
                names(receiver)
            }));
            if !returned.get() {
                timing.event(timing::Event::ReceiveReturn, index, submitted.elapsed(), 1);
            }
            timing.sentinel(&self.queue, index);
            let declared = match received {
                Ok(names) => names,
                Err(_) => {
                    // Exception text may contain a source path or filename.
                    tracing::warn!(receiver = index, "native promise receiver raised an exception");
                    self.notify(Err(ExportFailure::Failed));
                    self.cancel(false);
                    return false;
                }
            };
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
                        let names = RECEIVERS.with(|owned| {
                            owned
                                .borrow()
                                .get(current.batch.path())
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
    fn copy(&self, index: usize, path: &Path) -> Result<String, CopyError> {
        let incoming = self.batch.path().join("incoming");
        if path.parent() != Some(incoming.as_path()) {
            return Err(CopyError::invalid(CopyReason::OutsideDestination));
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| CopyError::invalid(CopyReason::InvalidName))?
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
        self.stage(62, 0, 0);
        match self.provider() {
            Ok(provider) => {
                if let (Some(shared), Some(operation)) = (self.shared.upgrade(), self.operation) {
                    let mut state = shared.lock();
                    if state.export.as_ref().is_some_and(|(op, _)| *op == operation) {
                        state.exported_data = Some((operation, provider));
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
        self.stage(65, 0, 0);
        let export = self.clone();
        let barrier = block2::RcBlock::new(move || {
            export.stage(66, 0, 0);
            // Modern callbacks run inside a coordinated read; legacy callbacks
            // use AppKit's documented readiness heuristic. Both have returned and
            // our independent ready copies are complete. No native reader of
            // incoming remains, so neither kind needs a permanent quarantine.
            let cleared = export.batch.external_writers_finished().is_ok();
            let ready = finalize && cleared && export.gate.lock().unwrap().commit(Instant::now());
            export.stage(67, 0, i64::from(!ready));
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
            export.release_receivers();
        });
        // SAFETY: the barrier runs after every receiver's callback operation and
        // its surrounding coordinated-read scope have returned.
        unsafe {
            self.queue.addBarrierBlock(&barrier);
        }
    }
    fn release_receivers(&self) {
        let path = self.batch.path().to_owned();
        let operation = self.operation;
        on_main(move |_| {
            RECEIVERS.with(|owned| {
                owned.borrow_mut().remove(&path);
            });
            if let Some(operation) = operation {
                UI.with(|ui| {
                    if let Some(ui) = ui.borrow_mut().as_mut() {
                        ui.receivers.remove(&operation.sequence);
                    }
                });
            }
        });
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
        let real = content::namespace::Local::new(self.originals.clone())?;
        let mut roots = real.roots();
        let mut native = content::native_metadata::NativeFileMetadata::default();
        let mut one_shot = Vec::new();
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
            let anchor = item.representations.iter().find_map(|rep| match rep.data {
                RepresentationData::Promise(node, _) => Some(node),
                _ => None,
            });
            if let Some(root) = item.file {
                if let Some(anchor) = anchor {
                    native.promises.push(content::native_metadata::MacPromiseGroup {
                        controls: content::native_metadata::MacPromiseGroup::controls(&item),
                        anchor,
                        roots: vec![root],
                        indices: vec![0],
                    });
                }
                output.push(item);
                continue;
            }
            if let Some(anchor) = anchor {
                let index = self
                    .anchors
                    .iter()
                    .position(|id| *id == anchor)
                    .ok_or(io::ErrorKind::InvalidData)?;
                let mut extras = Vec::new();
                let mut first = None;
                let mut promised = Vec::new();
                for (position, name) in names[index].iter().enumerate() {
                    let node = NodeId(next_node);
                    next_node += 1;
                    promised.push(node);
                    one_shot.push(node);
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
                        exec_bits: None,
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
                native.promises.push(content::native_metadata::MacPromiseGroup {
                    controls: content::native_metadata::MacPromiseGroup::controls(&item),
                    anchor,
                    indices: (0..promised.len() as u32).collect(),
                    roots: promised,
                });
                next_rep += 1;
                item.representations
                    .extend(file_item(first, RepresentationId(next_rep)).representations);
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
            real,
            native,
            promised: Mutex::new(one_shot.iter().copied().collect()),
            one_shot,
            capture: self.batch.clone(),
        });
        inventory.items = output;
        inventory.roots = roots;
        inventory.validate().map_err(content::unavailable)?;
        Ok(Arc::new(Exported {
            inventory,
            snapshot,
            files,
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
    real: Arc<dyn Namespace>,
    native: content::native_metadata::NativeFileMetadata,
    one_shot: Vec<NodeId>,
    capture: Capture,
    promised: Mutex<std::collections::HashSet<NodeId>>,
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
    fn native_file_metadata(&self) -> content::native_metadata::NativeFileMetadata {
        self.native.clone()
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        self.one_shot.clone()
    }
    fn release_fulfilled(&self, roots: &[NodeId]) {
        if self.capture.adopted(roots, &self.one_shot) {
            if let Some(Ok(files)) = self.local.get() {
                files.release_fulfilled(&self.one_shot);
            }
            self.pending.release_capture();
            self.capture.release();
        }
    }
    fn prepare(&self) -> io::Result<()> {
        self.files().map(|_| ())
    }
    fn source_roots(&self) -> io::Result<Vec<content::grants::SourceRoot>> {
        self.real.source_roots()
    }
    fn roots(&self) -> Vec<crate::model::FileNode> {
        match self.local.get() {
            Some(Ok(files)) => files.roots(),
            _ => self.roots.clone(),
        }
    }
    fn list(
        &self,
        node: NodeId,
        cursor: Option<crate::model::DirectoryCursor>,
    ) -> io::Result<crate::model::DirectoryPage> {
        let page = self.files()?.list(node, cursor)?;
        let mut promised = self.promised.lock().unwrap();
        if promised.contains(&node) {
            promised.extend(page.entries.iter().map(|entry| entry.id));
        }
        Ok(page)
    }
    fn select(&self, node: NodeId) -> io::Result<crate::model::FileNode> {
        self.files()?.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
        // Real file companions are independent of the temporary promise batch.
        if !self.promised.lock().unwrap().contains(&node) {
            return self.files()?.open(node, version, offset);
        }
        let batch = self.capture.pin()?;
        let inner = self.files()?.open(node, version, offset)?;
        Ok(Box::new(capture::Reader { inner, _batch: batch }))
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

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    mod timing_tests;
    define_class!(
        #[unsafe(super(NSFilePromiseReceiver))]
        #[name = "OinkThrowingPromiseReceiver"]
        struct ThrowingReceiver;
        unsafe impl NSObjectProtocol for ThrowingReceiver {}
        impl ThrowingReceiver {
            #[unsafe(method_id(fileTypes))]
            fn types(&self) -> Retained<NSArray<NSString>> {
                NSArray::from_retained_slice(&[NSString::from_str("public.data")])
            }
            #[unsafe(method(receivePromisedFilesAtDestination:options:operationQueue:reader:))]
            fn receive(
                &self,
                _destination: &NSURL,
                _options: &objc2_foundation::NSDictionary,
                _queue: &NSOperationQueue,
                _reader: &block2::DynBlock<dyn Fn(std::ptr::NonNull<NSURL>, *mut NSError)>,
            ) {
                // Real Objective-C exception, matching the native failure class;
                // no AppKit drag, native writer or desktop input is simulated.
                // SAFETY: deliberately raised inside the production exception boundary.
                unsafe {
                    objc2_foundation::NSException::exceptionWithName_reason_userInfo(
                        &NSString::from_str("NSInternalInconsistencyException"),
                        Some(&NSString::from_str("test receiver rejection")),
                        None,
                    ).raise();
                }
            }
        }
    );
    struct ReceiverDestinations {
        index: usize,
        paths: Arc<Mutex<Vec<PathBuf>>>,
    }
    define_class!(
        #[unsafe(super(NSFilePromiseReceiver))]
        #[name = "OinkRecordingPromiseReceiver"]
        #[ivars = ReceiverDestinations]
        struct RecordingReceiver;
        unsafe impl NSObjectProtocol for RecordingReceiver {}
        impl RecordingReceiver {
            #[unsafe(method_id(fileTypes))]
            fn types(&self) -> Retained<NSArray<NSString>> {
                NSArray::from_retained_slice(&[NSString::from_str("public.data")])
            }
            #[unsafe(method_id(fileNames))]
            fn names(&self) -> Retained<NSArray<NSString>> {
                NSArray::from_retained_slice(&[NSString::from_str(&format!("promise-{}.bin", self.ivars().index))])
            }
            #[unsafe(method(receivePromisedFilesAtDestination:options:operationQueue:reader:))]
            fn receive(
                &self,
                destination: &NSURL,
                _options: &objc2_foundation::NSDictionary,
                _queue: &NSOperationQueue,
                _reader: &block2::DynBlock<dyn Fn(std::ptr::NonNull<NSURL>, *mut NSError)>,
            ) {
                // Observe the real Objective-C API arguments, without pretending
                // to create an OS writer or invoke a read-ready callback.
                self.ivars().paths.lock().unwrap().push(PathBuf::from(destination.path().unwrap().to_string()));
            }
        }
    );
    #[test]
    fn all_receivers_share_one_native_destination_and_keep_separate_ready_copies() {
        objc2::rc::autoreleasepool(|_| {
            let root = tempfile::tempdir().unwrap();
            let backend = MacDnd::new();
            let mut inventory = deferred_promise().inventory();
            let mut second = inventory.items[0].clone();
            second.id = ItemId(1);
            second.representations[0].id = RepresentationId(2);
            second.representations[0].data = RepresentationData::Promise(NodeId(11), vec![]);
            inventory.items.push(second);
            let source =
                content::Deferred::new(inventory, Arc::new(super::super::super::content::MacTranslator)).unwrap();
            let export = Export::prepare(
                root.path(),
                DragOperation::new(),
                source.clone(),
                vec![],
                &backend.shared,
            )
            .unwrap();
            let batch = Batch::create(root.path(), StoreKind::SourceSnapshot, BatchId::random().unwrap()).unwrap();
            let snapshot = content::Snapshot::from_parts(source.inventory(), batch, source.translator()).unwrap();
            let paths = Arc::new(Mutex::new(Vec::new()));
            let receivers: Vec<_> = (0..2)
                .map(|index| {
                    let receiver = RecordingReceiver::alloc().set_ivars(ReceiverDestinations {
                        index,
                        paths: paths.clone(),
                    });
                    // SAFETY: inherited NSObject initializer of the native API spy.
                    let receiver: Retained<RecordingReceiver> = unsafe { msg_send![super(receiver), init] };
                    receiver.into_super()
                })
                .collect();
            assert!(export.receive_snapshot(&receivers, snapshot));
            let incoming = export.batch.path().join("incoming");
            assert_eq!(*paths.lock().unwrap(), vec![incoming.clone(), incoming.clone()]);
            for (index, bytes) in [b"first".as_slice(), b"second".as_slice()].into_iter().enumerate() {
                let file = incoming.join(format!("promise-{index}.bin"));
                fs::write(&file, bytes).unwrap();
                let name = export.copy(index, &file).unwrap();
                assert_eq!(
                    fs::read(export.batch.path().join("ready").join(index.to_string()).join(name)).unwrap(),
                    bytes
                );
            }
            export.cancel(false);
            // The spies created no external writers, so their test obligation can end.
            export.batch.external_writers_finished().unwrap();
        });
    }
    #[test]
    fn native_receiver_exception_fails_receipt_without_waiting_for_names_timeout() {
        objc2::rc::autoreleasepool(|_| {
            let root = tempfile::tempdir().unwrap();
            let backend = MacDnd::new();
            let source = deferred_promise();
            let operation = DragOperation::new();
            let export = Export::prepare(root.path(), operation, source.clone(), vec![], &backend.shared).unwrap();
            *export.deadline.lock().unwrap() = Some(Instant::now() + content::promise::LIMIT);
            let (send, events) = std::sync::mpsc::channel();
            assert!(
                backend
                    .shared
                    .sink
                    .set(Arc::new(move |event| {
                        let _ = send.send(event);
                    }))
                    .is_ok()
            );
            let batch = Batch::create(root.path(), StoreKind::SourceSnapshot, BatchId::random().unwrap()).unwrap();
            let snapshot = content::Snapshot::from_parts(source.inventory(), batch, source.translator()).unwrap();
            // SAFETY: inherited NSObject initializer of this test receiver subclass.
            let receiver: Retained<ThrowingReceiver> = unsafe { msg_send![ThrowingReceiver::alloc(), init] };
            assert!(!export.receive_snapshot(&[receiver.into_super()], snapshot));
            assert_eq!(
                events.try_recv().unwrap(),
                PlatformDndEvent::SourceExported {
                    operation,
                    result: Err(ExportFailure::Failed),
                }
            );
            assert_eq!(export.gate.lock().unwrap().phase, Phase::Abandoned);
            assert!(export.pending.failed());
            // A late name cannot turn the failed receipt into a delayed success.
            export.gate.lock().unwrap().names(0, vec!["late.bin".into()]);
            export.check();
            assert!(events.try_recv().is_err());
            // This stub never submitted an OS writer; release its test obligation.
            export.batch.external_writers_finished().unwrap();
        });
    }

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
    fn promise_names_precede_owned_bytes_and_native_publication_waits_for_receipt() {
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
        assert!(!export.batch.path().join("ready/0/attachment.txt").exists());
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            // Publishing a native path waits for the immutable source; name
            // discovery itself above remains metadata-only and nonblocking.
            assert_eq!(files.descriptor_entries().unwrap().unwrap()[0].path, "attachment.txt");
            assert!(files.descriptor_size_known(0));
            let result = files.open(0).and_then(|mut reader| content::read_all(&mut reader));
            send.send(result).unwrap();
        });
        assert!(receive.recv_timeout(Duration::from_millis(30)).is_err());
        let incoming = export.batch.path().join("incoming/attachment.txt");
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
    fn native_capture_retires_after_adoption_without_releasing_active_readers() {
        let root = tempfile::tempdir().unwrap();
        let backend = MacDnd::new();
        let real_path = root.path().join("real-directory");
        fs::create_dir(&real_path).unwrap();
        fs::write(real_path.join("child.txt"), b"real file remains").unwrap();
        let real = content::namespace::Local::new(vec![(NodeId(1000), real_path.clone())]).unwrap();
        let mut inventory = deferred_promise().inventory();
        inventory.roots = real.roots();
        let mut item = file_item(NodeId(1000), RepresentationId(2));
        item.id = ItemId(1);
        item.virtual_files = false;
        inventory.items.push(item);
        let source = content::Deferred::new(inventory, Arc::new(super::super::super::content::MacTranslator)).unwrap();
        let operation = DragOperation::new();
        let export = Export::prepare(
            root.path(),
            operation,
            source.clone(),
            vec![(NodeId(1000), real_path.clone())],
            &backend.shared,
        )
        .unwrap();
        backend.shared.lock().export = Some((operation, export.clone()));
        *export.snapshot.lock().unwrap() = Some(source);
        export.batch.external_writers_started().unwrap();
        assert!(export.gate.lock().unwrap().begin_until(None));
        export
            .gate
            .lock()
            .unwrap()
            .names(0, vec!["first.txt".into(), "second.txt".into()]);
        for name in ["first.txt", "second.txt"] {
            let incoming = export.batch.path().join("incoming").join(name);
            fs::write(&incoming, b"complete source bytes").unwrap();
            let name = export.copy(0, &incoming).unwrap();
            export.gate.lock().unwrap().completed(0, Some(name));
        }
        export.check();
        let provider = export.provider().unwrap();
        let files = provider.namespace().unwrap();
        files.prepare().unwrap();
        let roots = files.roots();
        let node = roots.iter().find(|node| node.name == "first.txt").unwrap().clone();
        let second = roots.iter().find(|node| node.name == "second.txt").unwrap().clone();
        let child = files.list(NodeId(1000), None).unwrap().entries.remove(0);
        let metadata = files.native_file_metadata();
        let path = export.batch.path().to_owned();
        let mut reader = files.open(node.id, node.version, 0).unwrap();
        files.release_fulfilled(&[NodeId(999)]);
        assert!(files.open(node.id, node.version, 0).is_ok());
        files.release_fulfilled(&[node.id]);
        assert!(
            files.open(node.id, node.version, 0).is_ok(),
            "unadopted roots still own the batch"
        );
        files.release_fulfilled(&[second.id]);
        assert!(files.open(node.id, node.version, 0).is_err());
        assert_eq!(files.roots(), roots);
        assert_eq!(files.native_file_metadata(), metadata);
        crate::store::usage(root.path(), StoreKind::PromiseExport, true);
        assert!(path.exists());
        assert_eq!(content::read_all(&mut reader).unwrap(), b"complete source bytes");
        drop(reader);
        crate::store::usage(root.path(), StoreKind::PromiseExport, true);
        assert!(
            !path.exists(),
            "source session and provider no longer retain redundant bytes"
        );
        let child = files.select(child.id).unwrap();
        assert_eq!(
            content::read_all(&mut files.open(child.id, child.version, 0).unwrap()).unwrap(),
            b"real file remains"
        );
        assert!(real_path.join("child.txt").exists());
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
        let incoming = export.batch.path().join("incoming/cancelled.txt");
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
            let result = Export::prepare(root.path(), DragOperation::new(), source, originals, &backend.shared);
            if missing_root {
                // A bad selected root no longer kills an otherwise valid promise.
                result.unwrap().cancel(false);
                continue;
            } else {
                assert!(result.is_err());
            }
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
            let board = crate::macos::test_pasteboard::new();
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
                let native = crate::macos::test_pasteboard::new();
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
                assert!(backend.source_requires_export(operation, false));
                assert!(backend.source_requires_export(operation, true));
                assert!(!backend.source_requires_export(DragOperation::new(), false));
                let snapshot = super::super::super::content::snapshot_board(&native, root.path(), id).unwrap();
                *export.snapshot.lock().unwrap() = Some(snapshot.clone());
                assert!(source.complete(snapshot));
                // Exercise the production copy/gate/provider boundary after a native
                // read-ready callback, without pretending this is an AppKit drag.
                let incoming = export.batch.path().join("incoming").join(filename);
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
                assert!(projected.has_promises());
                assert_eq!(projected.roots[0].name, filename);
                assert!(projected.items[0].file.is_some());
                assert_eq!(projected.namespace_roots(), projected.roots);
                let published = exported.namespace().unwrap();
                assert_eq!(published.one_shot_roots(), vec![projected.roots[0].id]);
                let metadata = published.native_file_metadata();
                assert_eq!(metadata.promises.len(), 1);
                assert_eq!(metadata.promises[0].anchor, export.anchors[0]);
                assert_eq!(metadata.promises[0].roots, published.one_shot_roots());
                // An adopted snapshot owns its original control metadata. A
                // later inventory must not pair immutable bytes with newly
                // rendered promise metadata after restart.
                let restored = serde_json::from_slice(&serde_json::to_vec(&metadata).unwrap()).unwrap();
                let mut refreshed = inventory.clone();
                for rep in &mut refreshed.items[0].representations {
                    if let RepresentationData::Promise(_, bytes) = &mut rep.data {
                        *bytes = b"changed after adoption".to_vec();
                    }
                }
                let republished =
                    content::native_metadata::project_inventory(refreshed, published.roots(), &restored).unwrap();
                for rep in &inventory.items[0].representations {
                    if matches!(rep.data, RepresentationData::Promise(..)) {
                        assert_eq!(
                            republished.items[0]
                                .representations
                                .iter()
                                .find(|value| value.id == rep.id)
                                .unwrap()
                                .data,
                            rep.data,
                            "durable promise metadata must match the adopted bytes"
                        );
                    }
                }
                assert!(published.source_roots().unwrap().is_empty());
                published.prepare().unwrap();
                assert_eq!(published.roots()[0].size, crate::model::ContentSize::Known(14));
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
                let board = crate::macos::test_pasteboard::new();
                assert!(board.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(writer)])));
                let target = board.pasteboardItems().unwrap().objectAtIndex(0);
                assert_eq!(super::super::super::content::file_path(&target), Some(provider_path));
                assert!(
                    target
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
                    if image_type == "public.tiff"
                        || super::super::super::content::standard(image_type) != Some(crate::model::StandardFormat::Png)
                    {
                        vec![
                            image_type,
                            "com.apple.pasteboard.promised-file-url",
                            "com.apple.pasteboard.promised-file-content-type",
                            "example.private",
                            "public.file-url",
                        ]
                    } else {
                        vec![
                            image_type,
                            "com.apple.pasteboard.promised-file-url",
                            "com.apple.pasteboard.promised-file-content-type",
                            "example.private",
                            "public.file-url",
                            "public.tiff",
                        ]
                    }
                );
                board.clearContents();
                native.clearContents();
            }
        });
    }
    define_class!(
        #[unsafe(super(NSFilePromiseReceiver))]
        #[name = "OinkMissingFilePromiseReceiver"]
        struct MissingFileReceiver;
        unsafe impl NSObjectProtocol for MissingFileReceiver {}
        impl MissingFileReceiver {
            #[unsafe(method_id(fileTypes))]
            fn types(&self) -> Retained<NSArray<NSString>> {
                NSArray::from_retained_slice(&[NSString::from_str("public.png")])
            }
            #[unsafe(method_id(fileNames))]
            fn names(&self) -> Retained<NSArray<NSString>> {
                NSArray::from_retained_slice(&[NSString::from_str("missing.png")])
            }
            #[unsafe(method(receivePromisedFilesAtDestination:options:operationQueue:reader:))]
            fn receive(&self, destination: &NSURL, _options: &objc2_foundation::NSDictionary,
                _queue: &NSOperationQueue,
                reader: &block2::DynBlock<dyn Fn(std::ptr::NonNull<NSURL>, *mut NSError)>) {
                let path = PathBuf::from(destination.path().unwrap().to_string()).join("missing.png");
                let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
                // Test only a read-ready callback with a missing file, not browser behaviour.
                reader.call((std::ptr::NonNull::from(&*url), std::ptr::null_mut()));
            }
        }
    );
    #[test]
    fn missing_native_file_wakes_namespace_waiters_with_specific_reason() {
        objc2::rc::autoreleasepool(|_| {
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
            let batch = Batch::create(root.path(), StoreKind::SourceSnapshot, BatchId::random().unwrap()).unwrap();
            let snapshot = content::Snapshot::from_parts(source.inventory(), batch, source.translator()).unwrap();
            // SAFETY: inherited NSObject initializer for the callback-only test double.
            let receiver: Retained<MissingFileReceiver> = unsafe { msg_send![MissingFileReceiver::alloc(), init] };
            assert!(export.receive_snapshot(&[receiver.into_super()], snapshot));
            assert_eq!(
                content::reason(&export.pending.wait().unwrap_err()),
                Unavailable::NoSuchItem
            );
            assert_eq!(export.gate.lock().unwrap().phase, Phase::Abandoned);
            export.batch.external_writers_finished().unwrap();
        });
    }
    #[test]
    fn promise_copy_accepts_empty_files_and_reports_mutation_separately() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.unfamiliar");
        fs::write(&source, []).unwrap();
        copy_tree(&source, &root.path().join("empty"), 0, &mut 0, &|| true).unwrap();
        fs::write(&source, b"initial").unwrap();
        let calls = AtomicUsize::new(0);
        let error = copy_tree(&source, &root.path().join("growing"), 0, &mut 0, &|| {
            if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                fs::write(&source, b"growing file").unwrap();
            }
            true
        })
        .unwrap_err();
        assert_eq!(error.reason, CopyReason::SizeMismatch);
        assert_eq!(error.unavailable(), Unavailable::InvalidData);
        let calls = AtomicUsize::new(0);
        let error = copy_tree(&source, &root.path().join("changed"), 0, &mut 0, &|| {
            if calls.fetch_add(1, Ordering::SeqCst) == 2 {
                fs::File::options()
                    .write(true)
                    .open(&source)
                    .unwrap()
                    .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
                    .unwrap();
            }
            true
        })
        .unwrap_err();
        assert_eq!(error.reason, CopyReason::SourceChanged);
    }
    #[test]
    fn promise_copy_rejections_keep_typed_content_reason() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("link");
        std::os::unix::fs::symlink("absent", &source).unwrap();
        let error = copy_tree(&source, &root.path().join("copy"), 0, &mut 0, &|| true).unwrap_err();
        assert_eq!(error.unavailable(), Unavailable::InvalidData);
        let error = copy_tree(&source, &root.path().join("cancelled"), 0, &mut 0, &|| false).unwrap_err();
        assert_eq!(error.unavailable(), Unavailable::Cancelled);
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
        assert_eq!(result.unwrap_err().unavailable(), Unavailable::Cancelled);
        assert!(fs::metadata(&target).unwrap().len() < fs::metadata(&source).unwrap().len());
        assert_eq!(fs::read(&source).unwrap(), vec![29; 3 * 1024 * 1024]);
    }
}
