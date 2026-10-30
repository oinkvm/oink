//! Native received-copy reads run in the signed manager without changing the
//! backend's dataless-I/O policy. Its stdout is a bounded pipe, never a backing file.
use super::*;
use crate::content::{
    ReaderGuard,
    linked::Descriptor,
    linked::completed,
    namespace::{NativeFiles, StoredFile},
};
use std::process::{Child, ChildStdout};

/// Match published URLs without opening/statting a dataless item in the agent.
/// The signed manager's received-read resolves the item and coordinates hydration.
/// No catalog/bridge lock can remain held while acquiring a grant or reading.
pub(crate) fn open_path(path: &std::path::Path) -> Option<io::Result<crate::FileReader>> {
    let bridge = BRIDGE.get()?.as_ref().ok()?;
    let batches: Vec<_> = bridge.batches.lock().unwrap().values().cloned().collect();
    for batch in batches {
        let node = node_for_path(&batch.state.lock().unwrap().catalog, path);
        if let Some(node) = node {
            let contents = batch.contents.lock().unwrap().clone();
            // The live namespace first commits the Drop grant, even when its
            // proactive download has not started yet. Restored/final catalogs
            // already own that authority and use the same native reader directly.
            return Some(contents.native_open(node, 0).unwrap_or_else(|| batch.open(node, 0)));
        }
    }
    None
}

fn node_for_path(catalog: &Catalog, path: &std::path::Path) -> Option<NodeId> {
    let root = &catalog.records.first()?.id;
    for (url, record) in catalog
        .urls
        .iter()
        .zip(catalog.records.iter().filter(|record| &record.parent == root))
    {
        let Ok(relative) = path.strip_prefix(url) else { continue };
        let mut record = record;
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return None;
            };
            record = catalog
                .records
                .iter()
                .find(|child| child.parent == record.id && name == child.name.as_str())?;
        }
        return (!record.directory && record.symlink_target.is_none())
            .then_some(record.node)
            .flatten()
            .map(NodeId);
    }
    None
}

struct ReaderPin {
    readers: Arc<Mutex<usize>>,
    _consumer: std::fs::File,
}
impl ReaderPin {
    fn new(batch: &Batch) -> io::Result<Self> {
        let consumer = batch.consumer_lock()?;
        *batch.readers.lock().unwrap_or_else(|error| error.into_inner()) += 1;
        Ok(Self {
            readers: batch.readers.clone(),
            _consumer: consumer,
        })
    }
}
impl Drop for ReaderPin {
    fn drop(&mut self) {
        *self.readers.lock().unwrap_or_else(|error| error.into_inner()) -= 1;
    }
}
struct NativeReader {
    helper: Helper,
    _pin: ReaderPin,
}
impl Read for NativeReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.helper.read(bytes)
    }
}
pub(super) struct Helper {
    child: Arc<Mutex<Child>>,
    output: ChildStdout,
    done: Arc<AtomicBool>,
}
impl Helper {
    pub(super) fn start(args: &[String], check: Option<ReaderGuard>) -> io::Result<Self> {
        let path = std::env::current_exe()?
            .parent()
            .ok_or(io::ErrorKind::NotFound)?
            .join("oink-file-provider");
        let mut child = Command::new(path)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let output = child.stdout.take().ok_or(io::ErrorKind::BrokenPipe)?;
        let child = Arc::new(Mutex::new(child));
        let done = Arc::new(AtomicBool::new(false));
        let helper = Self { child, output, done };
        if let Some(check) = check {
            let child = helper.child.clone();
            let done = helper.done.clone();
            std::thread::Builder::new()
                .name("oink-native-download".into())
                .spawn(move || {
                    while !done.load(Ordering::Acquire) {
                        if check().is_err() {
                            let _ = child.lock().unwrap().kill();
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                })?;
        }
        Ok(helper)
    }
}
impl Read for Helper {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let count = self.output.read(bytes)?;
        if count == 0 && !self.child.lock().unwrap().wait()?.success() {
            return Err(io::ErrorKind::NotConnected.into());
        }
        Ok(count)
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}
impl Batch {
    fn restored_contents(&self, catalog: &Catalog) -> io::Result<Arc<dyn Namespace>> {
        let contents = catalog
            .settlement
            .restore(catalog.source.clone(), crate::content::linked::native_connector())?;
        // Grant commits and FinalLocal settlement replace the namespace, but
        // still read the same native received copy. Bind before publishing the
        // replacement: otherwise native_open fails before the manager can run.
        // A weak reference keeps Batch -> namespace -> Batch from forming a cycle.
        let native: Weak<dyn NativeFiles> = self.owner.clone();
        contents.bind_native(native)?;
        Ok(contents)
    }
    pub(super) fn native_args(&self, command: &str, node: Option<NodeId>) -> io::Result<Vec<String>> {
        let state = self.state.lock().unwrap();
        if state.retired || state.catalog.source.as_ref().is_some_and(|source| source.provisional) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        if node.is_some_and(|node| {
            !state.catalog.records.iter().any(|record| {
                record.node == Some(node.0)
                    && ((command == "received-link" && record.symlink_target.is_some())
                        || (!record.directory && record.symlink_target.is_none()))
            })
        }) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let key = self
            .catalog
            .file_stem()
            .and_then(|key| key.to_str())
            .ok_or(io::ErrorKind::InvalidData)?;
        let mut args = vec![command.into(), key.into()];
        if let Some(node) = node {
            args.push(node.0.to_string());
        }
        Ok(args)
    }
    /// The extension reports EOF/handoff once. This uses receive bookkeeping,
    /// never a second native read or a query of the OS pin/materialized sets.
    pub(super) fn received_complete(&self, node: NodeId, version: u64) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.source_available(node)?;
        if !state
            .catalog
            .records
            .iter()
            .any(|record| record.node == Some(node.0) && record.version == version)
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut next = state.catalog.clone();
        if !next.arrived.contains(&node) {
            next.arrived.push(node);
        }
        self.complete_catalog(&mut next)?;
        let contents = self.restored_contents(&next)?;
        save_catalog(&self.catalog, &next)?;
        state.catalog = next;
        *self.contents.lock().unwrap() = contents;
        drop(state);
        if CONTAINER.get().is_some() {
            signal_changes();
        }
        Ok(())
    }
    fn complete_catalog(&self, catalog: &mut Catalog) -> io::Result<()> {
        let Some(source) = catalog.source.as_ref().filter(|source| !source.provisional) else {
            return Ok(());
        };
        let roots = source.fulfilled.clone();
        let key = self
            .catalog
            .file_stem()
            .and_then(|key| key.to_str())
            .ok_or(io::ErrorKind::InvalidData)?;
        let manifest = linked_catalog_nodes(key, catalog)?;
        for root in roots {
            if catalog.failed.contains(&root.node) {
                continue;
            }
            let entries = completed::subtree(root.node, &manifest)?;
            if entries.iter().all(|entry| {
                catalog.arrived.contains(&entry.id)
                    && (entry.kind != NodeKind::Directory
                        || catalog
                            .records
                            .iter()
                            .find(|record| record.node == Some(entry.id.0))
                            .is_some_and(|record| {
                                record.children
                                    == Some(
                                        catalog.records.iter().filter(|child| child.parent == record.id).count() as u64
                                    )
                            }))
            }) {
                catalog.settlement.commit(&mut catalog.source, &root, &manifest)?;
                tracing::debug!(
                    items = entries.len(),
                    "one-shot receive complete; FinalLocal without verification"
                );
            }
        }
        catalog.file_policies();
        Ok(())
    }
    /// A failed F1 cannot resume. Withdraw its own subtree; real linked files
    /// and completed local roots retain their independent ownership.
    pub(super) fn received_failed(&self, node: NodeId) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let mut next = state.catalog.clone();
        let key = self
            .catalog
            .file_stem()
            .and_then(|key| key.to_str())
            .ok_or(io::ErrorKind::InvalidData)?;
        let manifest = linked_catalog_nodes(key, &next)?;
        let Some(source) = next.source.as_ref() else {
            return Ok(());
        };
        let root = source
            .fulfilled
            .iter()
            .find(|root| {
                completed::subtree(root.node, &manifest)
                    .is_ok_and(|entries| entries.iter().any(|entry| entry.id == node))
            })
            .cloned();
        let Some(root) = root else {
            return Ok(());
        };
        if !next.failed.contains(&root.node) {
            next.failed.push(root.node);
            if !has_local_edit(&self.catalog, &completed::subtree(root.node, &manifest)?) {
                fail_catalog_root(&mut next, &manifest, root.node)?;
            }
        }
        let contents = self.restored_contents(&next)?;
        save_catalog(&self.catalog, &next)?;
        state.catalog = next;
        *self.contents.lock().unwrap() = contents;
        drop(state);
        if CONTAINER.get().is_some() {
            signal_changes();
        }
        Ok(())
    }
}
impl NativeFiles for Batch {
    fn lease(&self) -> io::Result<crate::content::namespace::NativeLease> {
        let state = self.state.lock().unwrap();
        if state.retired {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(Box::new(ReaderPin::new(self)?))
    }
    fn persist_link(&self, descriptor: &Descriptor) -> io::Result<()> {
        descriptor.validate()?;
        let mut state = self.state.lock().unwrap();
        if state.retired
            || state
                .catalog
                .source
                .as_ref()
                .is_some_and(|old| old.token != descriptor.token)
        {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let mut next = state.catalog.clone();
        next.source = next.settlement.remaining_link(descriptor)?;
        self.complete_catalog(&mut next)?;
        let contents = self.restored_contents(&next)?;
        save_catalog(&self.catalog, &next)?;
        state.catalog = next;
        if !descriptor.provisional {
            *self.contents.lock().unwrap() = contents;
        }
        drop(state);
        if CONTAINER.get().is_some() {
            signal_changes();
        }
        Ok(())
    }
    fn list(
        &self,
        node: NodeId,
        cursor: Option<crate::model::DirectoryCursor>,
    ) -> io::Result<crate::model::DirectoryPage> {
        let _pin = ReaderPin::new(self)?;
        let contents = {
            let state = self.state.lock().unwrap();
            if state.retired || state.catalog.source.as_ref().is_some_and(|source| source.provisional) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            state.source_available(node)?;
            self.contents.lock().unwrap().clone()
        };
        let page = contents.list(node, cursor)?;
        persist_listing(self, node, cursor, &page)?;
        if CONTAINER.get().is_some() {
            signal_changes();
        }
        Ok(page)
    }
    fn open(&self, node: NodeId, offset: u64) -> io::Result<crate::FileReader> {
        let pin = ReaderPin::new(self)?;
        let mut args = self.native_args("received-read", Some(node))?;
        args.push(offset.to_string());
        Ok(Box::new(NativeReader {
            helper: Helper::start(&args, None)?,
            _pin: pin,
        }))
    }
    fn symlink_target(&self, node: NodeId) -> io::Result<String> {
        let _pin = ReaderPin::new(self)?;
        let args = self.native_args("received-link", Some(node))?;
        let mut bytes = Vec::new();
        Helper::start(&args, None)?
            .take(24 * 1024 + 3)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 24 * 1024 + 2 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        serde_json::from_slice(&bytes).map_err(Into::into)
    }
    fn metadata(&self, node: NodeId) -> io::Result<crate::FileMetadata> {
        let _pin = ReaderPin::new(self)?;
        let args = self.native_args("received-stat", Some(node))?;
        let mut bytes = Vec::new();
        Helper::start(&args, None)?.take(4096).read_to_end(&mut bytes)?;
        #[derive(Deserialize)]
        struct Metadata {
            size: u64,
            modified_ms: Option<i64>,
            exec_bits: Option<u16>,
        }
        let metadata: Metadata = serde_json::from_slice(&bytes)?;
        Ok(crate::FileMetadata {
            exec_bits: metadata.exec_bits,
            size: metadata.size,
            modified_ms: metadata.modified_ms,
        })
    }
    fn download(
        &self,
        check: &ReaderGuard,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> io::Result<Vec<StoredFile>> {
        check()?;
        let _pin = ReaderPin::new(self)?;
        let args = self.native_args("received-download", None)?;
        let mut input = io::BufReader::new(Helper::start(&args, Some(check.clone()))?);
        let mut files = Vec::new();
        let mut total = 0u64;
        loop {
            check()?;
            let mut line = String::new();
            let count = input.by_ref().take(4096).read_line(&mut line)?;
            if count == 0 {
                break;
            }
            if count == 4096 || !line.ends_with('\n') || files.len() >= crate::model::MAX_DISCOVERED_NODES {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let file: StoredFile = serde_json::from_str(&line)?;
            total = total.checked_add(file.size).ok_or(io::ErrorKind::InvalidData)?;
            files.push(file);
            progress(total, files.len() as u64);
        }
        check()?;
        Ok(files)
    }
}

fn fail_catalog_root(catalog: &mut Catalog, manifest: &[FileNode], root: NodeId) -> io::Result<()> {
    let entries = completed::subtree(root, manifest)?;
    let ids: HashSet<_> = entries.iter().map(|entry| entry.id).collect();
    catalog
        .records
        .retain(|record| record.node.is_none_or(|node| !ids.contains(&NodeId(node))));
    catalog.arrived.retain(|node| !ids.contains(node));
    if let Some(source) = &mut catalog.source {
        source.roots.retain(|entry| entry.id != root);
        source.fulfilled.retain(|entry| entry.node != root);
        source.native = source
            .native
            .restricted(&source.roots.iter().map(|root| root.id).collect::<Vec<_>>());
        if source.roots.is_empty() {
            catalog.source = None;
        }
    }
    catalog.settlement.forget(root);
    catalog.file_policies();
    tracing::debug!(items = ids.len(), "failed one-shot receive withdrawn");
    Ok(())
}
pub(super) fn recover_legacy_received(
    catalog: &mut Catalog,
    key: &str,
    path: &std::path::Path,
    roots: &HashSet<NodeId>,
) -> io::Result<()> {
    if roots.is_empty()
        || catalog.move_state.is_some()
        || catalog.source.as_ref().is_none_or(|source| source.provisional)
    {
        return Ok(());
    }
    // Swift adds local-only fields such as `created` to its sidecar Record.
    // Decode just content identity, rather than the strict Rust catalog Record.
    #[derive(serde::Deserialize)]
    struct HandoffContent {
        id: String,
        node: u64,
        version: u64,
        size: Option<u64>,
        directory: bool,
        symlink_target: Option<String>,
        exec_bits: Option<u16>,
    }
    let overlay = match std::fs::read(path.with_extension("received")) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) => value,
            Err(_) => return Ok(()),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => serde_json::Value::Null,
        Err(error) => return Err(error),
    };
    let manifest = linked_catalog_nodes(key, catalog)?;
    for root in roots {
        if catalog.failed.contains(root) {
            continue;
        }
        let entries = completed::subtree(*root, &manifest)?;
        let received = entries.iter().all(|entry| {
            let Some(record) = catalog.records.iter().find(|record| record.node == Some(entry.id.0)) else {
                return false;
            };
            let observed = &overlay[&record.id];
            if observed["deleted"] == true {
                return false;
            }
            if record.directory {
                // Old enumerators did not emit directory handoffs. A known
                // final child count plus all descendants' handoffs is enough,
                // including an empty directory; unknown lists are incomplete.
                return record.children
                    == Some(catalog.records.iter().filter(|child| child.parent == record.id).count() as u64);
            }
            let Ok(current) = serde_json::from_value::<HandoffContent>(observed["record"].clone()) else {
                return false;
            };
            observed["handed_off"] == true
                && record.size.is_some()
                && current.id == record.id
                && Some(current.node) == record.node
                && current.version == record.version
                && current.size == record.size
                && current.directory == record.directory
                && current.symlink_target == record.symlink_target
                && current.exec_bits == record.exec_bits
        });
        if received {
            // Handoff is the extension's durable record of successful byte
            // delivery. Do not open provider URLs, rehash, query the source or
            // revive received-check/pin just to upgrade build-72 receipts.
            catalog
                .settlement
                .recover_legacy_received(&mut catalog.source, *root, &manifest)?;
        }
    }
    catalog.file_policies();
    Ok(())
}

/// Startup never reconnects an unfinished F1. F_l placeholders remain linked.
pub(super) fn discard_incomplete(catalog: &mut Catalog, key: &str, path: &std::path::Path) -> io::Result<()> {
    let manifest = linked_catalog_nodes(key, catalog)?;
    let roots = catalog.source.as_ref().map_or_else(Vec::new, |source| {
        source.fulfilled.iter().map(|root| root.node).collect()
    });
    for root in roots {
        if !catalog.failed.contains(&root) {
            catalog.failed.push(root);
        }
        if !has_local_edit(path, &completed::subtree(root, &manifest)?) {
            fail_catalog_root(catalog, &manifest, root)?;
        }
    }
    Ok(())
}

// Native edits have independent user ownership. A failed transfer may remove
// its own partial bytes, never a file an application has already modified.
fn has_local_edit(path: &std::path::Path, entries: &[FileNode]) -> bool {
    match std::fs::read(path.with_extension("received")) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .is_none_or(|overlay| {
                overlay.values().any(|value| {
                    value["modified_locally"] == true
                        && entries
                            .iter()
                            .any(|entry| value["record"]["node"].as_u64() == Some(entry.id.0))
                })
            }),
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

pub(super) fn recover(batch: &Arc<Batch>) -> io::Result<()> {
    let namespace = batch.contents.lock().unwrap().clone();
    let native: Arc<dyn NativeFiles> = batch.clone();
    namespace.bind_native(Arc::downgrade(&native))?;
    Ok(())
}

pub(super) fn bind(batch: &Arc<Batch>, namespace: &Arc<dyn Namespace>) -> io::Result<()> {
    let native: Arc<dyn NativeFiles> = batch.clone();
    namespace.bind_native(Arc::downgrade(&native))?;
    recover(batch)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::content::{
        Provider,
        grants::{FulfilledRoot, LinkToken},
        linked::Connector,
    };

    fn assert_received_binding(batch: &Arc<Batch>) -> Arc<dyn Namespace> {
        let namespace = batch.contents.lock().unwrap().clone();
        // native_open, native_metadata and native_lease share LocalNamespace's
        // native() lookup. A real batch lease proves it reaches this received
        // store without starting a helper or touching a registered OS domain.
        let lease = namespace.native_lease().unwrap();
        assert_eq!(*batch.readers.lock().unwrap(), 1);
        drop(lease);
        assert_eq!(*batch.readers.lock().unwrap(), 0);
        namespace
    }

    #[test]
    fn committed_drop_replacement_keeps_native_received_binding() {
        let root = tempfile::tempdir().unwrap();
        let key = "f".repeat(64);
        let path = root.path().join(format!("{key}.json"));
        let mut descriptor = Descriptor {
            native: Default::default(),
            offer: crate::content::tests::fixture().inventory().id,
            token: LinkToken([7; 32]),
            provisional: true,
            roots: vec![FileNode {
                exec_bits: None,
                id: NodeId(1),
                parent: None,
                name: "invalid.dmg".into(),
                kind: NodeKind::File,
                size: ContentSize::Known(7),
                modified_ms: None,
                version: 1,
            }],
            fulfilled: Vec::new(),
        };
        // No connector transport: native binding must not depend on a network
        // connection, current clipboard owner, or an independent download.
        let namespace = Connector::default().restore(descriptor.clone()).unwrap();
        let (batch, _) = prepare_batch(namespace.clone(), &key, "token".into(), 1, path.clone()).unwrap();
        bind(&batch, &namespace).unwrap();
        batch.persist_link(&descriptor).unwrap();
        assert!(Arc::ptr_eq(&namespace, &batch.contents.lock().unwrap()));
        assert!(batch.native_args("received-read", Some(NodeId(1))).is_err());

        descriptor.provisional = false;
        // A failed save must not publish either the authority or its namespace.
        let previous = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(batch.persist_link(&descriptor).is_err());
        assert!(Arc::ptr_eq(&namespace, &batch.contents.lock().unwrap()));
        assert!(batch.state.lock().unwrap().catalog.source.as_ref().unwrap().provisional);
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, previous).unwrap();

        batch.persist_link(&descriptor).unwrap();
        let committed = assert_received_binding(&batch);
        assert!(!Arc::ptr_eq(&namespace, &committed));
        assert!(batch.native_args("received-read", Some(NodeId(1))).is_ok());
        let mut stale = descriptor.clone();
        stale.token = LinkToken([8; 32]);
        assert!(batch.persist_link(&stale).is_err());
        assert!(Arc::ptr_eq(&committed, &batch.contents.lock().unwrap()));

        batch.state.lock().unwrap().retired = true;
        assert!(committed.native_lease().is_err());
        let weak = Arc::downgrade(&batch);
        drop(batch);
        assert!(weak.upgrade().is_none(), "the replacement cannot retain its batch");
        assert_eq!(
            committed.native_lease().err().unwrap().kind(),
            io::ErrorKind::NotConnected
        );
    }

    #[test]
    fn received_package_paths_resolve_roots_and_directory_children_without_io() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("folder");
        std::fs::create_dir(&directory).unwrap();
        let package = root.path().join("Oink.dmg");
        std::fs::write(&package, b"placeholder").unwrap();
        let namespace =
            crate::content::namespace::Local::new(vec![(NodeId(1), package), (NodeId(2), directory)]).unwrap();
        let key = "d".repeat(64);
        let (batch, _) = prepare_batch(namespace, &key, "token".into(), 1, root.path().join("catalog.json")).unwrap();
        let mut state = batch.state.lock().unwrap();
        state.catalog.urls = vec!["/received/Oink.dmg".into(), "/received/folder".into()];
        let child = FileNode {
            exec_bits: None,
            id: NodeId(3),
            parent: Some(NodeId(2)),
            name: "nested.dmg".into(),
            kind: NodeKind::File,
            size: ContentSize::Unknown,
            modified_ms: None,
            version: 1,
        };
        let record = node_record(&key, &child, &mut state.names).unwrap();
        state.catalog.records.push(record);
        for (path, expected) in [
            ("/received/Oink.dmg", Some(NodeId(1))),
            ("/received/folder/nested.dmg", Some(NodeId(3))),
            ("/received/folder", None),
            ("/received/folder/../Oink.dmg", None),
            ("/received/Oink.dmg.other", None),
            ("/elsewhere/Oink.dmg", None),
        ] {
            assert_eq!(node_for_path(&state.catalog, path.as_ref()), expected, "{path}");
        }
    }
    #[test]
    fn native_consumer_excludes_received_maintenance_before_first_read() {
        use std::os::fd::AsRawFd;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source");
        std::fs::write(&path, b"received").unwrap();
        let namespace = crate::content::namespace::Local::new(vec![(NodeId(1), path)]).unwrap();
        let key = "d".repeat(64);
        let catalog = root.path().join(format!("{key}.json"));
        let (batch, _) = prepare_batch(namespace, &key, "token".into(), 1, catalog.clone()).unwrap();
        let maintenance = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(catalog.with_extension("consumers"))
            .unwrap();
        let consumer = NativeFiles::lease(batch.as_ref()).unwrap();
        // This is the same nonblocking cross-process lock used by the signed
        // manager; no registered domain or native file deletion is involved.
        // SAFETY: maintenance owns this descriptor throughout the call.
        let locked = unsafe { libc::flock(maintenance.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_ne!(locked, 0, "an unread native consumer must block received cleanup");
        drop(consumer);
        assert_eq!(
            // SAFETY: maintenance still owns the same open descriptor.
            unsafe { libc::flock(maintenance.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(
            NativeFiles::lease(batch.as_ref()).is_err(),
            "cleanup must exclude new consumers"
        );
        assert_eq!(*batch.readers.lock().unwrap(), 0);
        // SAFETY: unlock the descriptor whose exclusive lock we hold.
        assert_eq!(unsafe { libc::flock(maintenance.as_raw_fd(), libc::LOCK_UN) }, 0);
        let consumer = NativeFiles::lease(batch.as_ref()).unwrap();
        assert_eq!(*batch.readers.lock().unwrap(), 1);
        drop(consumer);
        assert_eq!(*batch.readers.lock().unwrap(), 0);
    }
    #[test]
    fn native_listing_registers_children_before_promise_read_admission() {
        struct UnknownChild(Arc<dyn Namespace>);
        impl Namespace for UnknownChild {
            fn roots(&self) -> Vec<FileNode> {
                self.0.roots()
            }
            fn list(
                &self,
                node: NodeId,
                cursor: Option<crate::model::DirectoryCursor>,
            ) -> io::Result<crate::model::DirectoryPage> {
                let mut page = self.0.list(node, cursor)?;
                for entry in &mut page.entries {
                    if entry.kind == NodeKind::File {
                        entry.size = ContentSize::Unknown;
                    }
                }
                Ok(page)
            }
            fn select(&self, _: NodeId) -> io::Result<FileNode> {
                panic!("listing must not select or read file bodies")
            }
            fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<crate::FileReader> {
                panic!("listing must not read file bodies")
            }
        }
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("first.bin"), vec![b'B'; 2048]).unwrap();
        let namespace = Arc::new(UnknownChild(
            crate::content::namespace::Local::new(vec![(NodeId(1), folder)]).unwrap(),
        ));
        let key = "d".repeat(64);
        let catalog = root.path().join(format!("{key}.json"));
        let (batch, _) = prepare_batch(namespace, &key, "token".into(), 1, catalog.clone()).unwrap();
        let page = NativeFiles::list(batch.as_ref(), NodeId(1), None).unwrap();
        assert_eq!(page.entries.len(), 1);
        let child = &page.entries[0];
        assert_eq!(child.size, ContentSize::Unknown);
        assert!(
            batch.native_args("received-read", Some(child.id)).is_ok(),
            "a promise must be able to read a child immediately after native_list, without a background download"
        );
        let saved: Catalog = serde_json::from_slice(&std::fs::read(catalog).unwrap()).unwrap();
        let record = saved
            .records
            .iter()
            .find(|record| record.node == Some(child.id.0))
            .unwrap();
        assert_eq!(record.name, "first.bin");
        assert_eq!(record.size, None, "unknown size is not a known zero");
        let parent = saved.records.iter().find(|record| record.node == Some(1)).unwrap();
        assert_eq!(parent.children, Some(1));
        assert_eq!(parent.version, page.version);
    }
    #[test]
    fn native_directory_listing_uses_final_catalog_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let key = "c".repeat(64);
        let path = root.path().join(format!("{key}.json"));
        let directory = FileNode {
            exec_bits: None,
            id: NodeId(4),
            parent: None,
            name: "directory".into(),
            kind: NodeKind::Directory,
            size: ContentSize::Known(0),
            modified_ms: None,
            version: 9,
        };
        let child = FileNode {
            exec_bits: None,
            id: NodeId(5),
            parent: Some(directory.id),
            name: "empty".into(),
            kind: NodeKind::Directory,
            size: ContentSize::Known(0),
            modified_ms: None,
            version: 2,
        };
        let receipt = FulfilledRoot {
            node: directory.id,
            id: [5; 16],
        };
        let descriptor = Descriptor {
            native: Default::default(),
            offer: crate::content::tests::fixture().inventory().id,
            token: LinkToken([9; 32]),
            provisional: false,
            roots: vec![directory.clone()],
            fulfilled: vec![receipt.clone()],
        };
        let namespace = Connector::default().restore(descriptor.clone()).unwrap();
        let (batch, _) = prepare_batch(namespace, &key, "token".into(), 1, path.clone()).unwrap();
        {
            let mut state = batch.state.lock().unwrap();
            let record = node_record(&key, &child, &mut state.names).unwrap();
            state.catalog.records.push(record);
        }
        {
            let mut state = batch.state.lock().unwrap();
            for record in &mut state.catalog.records {
                if record.node == Some(directory.id.0) {
                    record.children = Some(1);
                }
                if record.node == Some(child.id.0) {
                    record.children = Some(0);
                }
            }
        }
        batch.received_complete(directory.id, directory.version).unwrap();
        assert!(batch.state.lock().unwrap().catalog.source.is_some());
        batch.received_complete(child.id, child.version).unwrap();
        drop(batch);
        let batch = restore_catalog(data.path(), &path, &key, 2).unwrap();
        assert!(batch.contents.lock().unwrap().source_link().is_none());
        let page = NativeFiles::list(batch.as_ref(), directory.id, None).unwrap();
        assert_eq!(page.entries, vec![child.clone()]);
        assert!(
            NativeFiles::list(batch.as_ref(), child.id, None)
                .unwrap()
                .entries
                .is_empty()
        );
        batch.state.lock().unwrap().retired = true;
        assert!(NativeFiles::list(batch.as_ref(), directory.id, None).is_err());
    }
    fn one_shot_batch(root: &std::path::Path) -> (Arc<Batch>, Descriptor) {
        let descriptor = Descriptor {
            native: Default::default(),
            offer: crate::content::tests::fixture().inventory().id,
            token: LinkToken([4; 32]),
            provisional: false,
            roots: vec![FileNode {
                id: NodeId(1),
                parent: None,
                name: "received".into(),
                kind: NodeKind::File,
                size: ContentSize::Known(7),
                version: 1,
                modified_ms: None,
                exec_bits: None,
            }],
            fulfilled: vec![FulfilledRoot {
                node: NodeId(1),
                id: [3; 16],
            }],
        };
        let key = "a".repeat(64);
        let namespace = Connector::default().restore(descriptor.clone()).unwrap();
        let (batch, _) = prepare_batch(namespace, &key, "token".into(), 1, root.join(format!("{key}.json"))).unwrap();
        save_catalog(&batch.catalog, &batch.state.lock().unwrap().catalog).unwrap();
        (batch, descriptor)
    }
    #[test]
    fn build72_pending_receipt_recovery_keeps_complete_bytes_and_withdraws_missing_bytes() {
        for damage in [
            "none",
            "old-fetch",
            "missing-handoff",
            "stale-version",
            "stale-size",
            "deleted",
            "unlisted-directory",
        ] {
            let missing = !matches!(damage, "none" | "old-fetch");
            let root = tempfile::tempdir().unwrap();
            let key = "a".repeat(64);
            let path = root.path().join(format!("{key}.json"));
            let received = root.path().join("receive/tree/body");
            std::fs::create_dir_all(received.parent().unwrap()).unwrap();
            std::fs::write(&received, if missing { &b"a"[..] } else { &b"abc"[..] }).unwrap();
            let mut catalog: serde_json::Value =
                serde_json::from_str(include_str!("../../../tests/fixtures/build72-pending-mac.json")).unwrap();
            catalog["urls"] = serde_json::json!([received.parent().unwrap()]);
            let mut overlay: serde_json::Value = serde_json::from_str(include_str!(
                "../../../tests/fixtures/build72-pending-mac.received.json"
            ))
            .unwrap();
            match damage {
                "old-fetch" => overlay[format!("{key}-2")]["fetching"] = 1.into(),
                "missing-handoff" => overlay[format!("{key}-2")]["handed_off"] = false.into(),
                "stale-version" => overlay[format!("{key}-2")]["record"]["version"] = 4.into(),
                "stale-size" => overlay[format!("{key}-2")]["record"]["size"] = 1.into(),
                "deleted" => overlay[format!("{key}-2")]["deleted"] = true.into(),
                "unlisted-directory" => catalog["records"][3]["children"] = serde_json::Value::Null,
                _ => {}
            }
            std::fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
            std::fs::write(path.with_extension("received"), serde_json::to_vec(&overlay).unwrap()).unwrap();
            let restored = restore_catalog(root.path(), &path, &key, 2);
            if missing {
                assert!(restored.is_err());
                assert!(!path.exists(), "missing bytes must withdraw the old catalog");
            } else {
                let batch = restored.expect("old complete receive must become FinalLocal, not disappear");
                assert!(batch.state.lock().unwrap().catalog.source.is_none());
                assert!(batch.state.lock().unwrap().catalog.settlement.contains(NodeId(2)));
                assert!(batch.state.lock().unwrap().catalog.failed.is_empty());
                assert_eq!(std::fs::read(&received).unwrap(), b"abc");
                assert_eq!(
                    NativeFiles::list(batch.as_ref(), NodeId(1), None)
                        .unwrap()
                        .entries
                        .len(),
                    2
                );
                let saved = std::fs::read_to_string(&path).unwrap();
                assert!(!saved.contains("digest") && !saved.contains("pinning"));
                drop(batch);
                let again = restore_catalog(root.path(), &path, &key, 3).unwrap();
                assert!(again.state.lock().unwrap().catalog.settlement.contains(NodeId(2)));
            }
        }
    }

    #[test]
    fn arrival_is_final_without_a_helper_native_copy_or_sender_and_survives_restart() {
        let root = tempfile::tempdir().unwrap();
        let (batch, descriptor) = one_shot_batch(root.path());
        batch.received_complete(NodeId(1), 1).unwrap();
        assert!(batch.state.lock().unwrap().catalog.source.is_none());
        assert!(batch.state.lock().unwrap().catalog.settlement.contains(NodeId(1)));
        assert_received_binding(&batch);
        batch.persist_link(&descriptor).unwrap();
        assert!(batch.state.lock().unwrap().catalog.source.is_none());
        let path = batch.catalog.clone();
        drop(batch);
        let batch = restore_catalog(root.path(), &path, &"a".repeat(64), 2).unwrap();
        assert!(batch.contents.lock().unwrap().source_link().is_none());
        assert_received_binding(&batch);
        assert!(
            batch
                .state
                .lock()
                .unwrap()
                .catalog
                .records
                .iter()
                .filter(|record| record.node == Some(1))
                .all(|record| record.keep_local == Some(true) && record.refetchable != Some(true))
        );
    }
    #[test]
    fn failed_and_restarted_incomplete_receives_withdraw_their_placeholders() {
        for restart in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (batch, _) = one_shot_batch(root.path());
            let path = batch.catalog.clone();
            if restart {
                drop(batch);
                assert!(restore_catalog(root.path(), &path, &"a".repeat(64), 2).is_err());
                assert!(!path.exists());
            } else {
                batch.received_failed(NodeId(1)).unwrap();
                assert!(
                    batch
                        .state
                        .lock()
                        .unwrap()
                        .catalog
                        .records
                        .iter()
                        .all(|record| record.node != Some(1))
                );
                assert!(batch.state.lock().unwrap().catalog.source.is_none());
            }
        }
    }
    #[test]
    fn incomplete_receive_failure_preserves_an_independent_local_edit() {
        let root = tempfile::tempdir().unwrap();
        let (batch, _) = one_shot_batch(root.path());
        let record = batch
            .state
            .lock()
            .unwrap()
            .catalog
            .records
            .iter()
            .find(|record| record.node == Some(1))
            .unwrap()
            .clone();
        std::fs::write(batch.catalog.with_extension("received"),serde_json::to_vec(&serde_json::json!({record.id.clone(): {"record":record,"modified_locally":true,"handed_off":true,"materialized":true,"deleted":false}})).unwrap()).unwrap();
        batch.received_failed(NodeId(1)).unwrap();
        assert_eq!(batch.state.lock().unwrap().catalog.failed, [NodeId(1)]);
        assert!(batch.source_available(NodeId(1)).is_err());
        assert!(
            batch.received_complete(NodeId(1), 1).is_err(),
            "late EOF revived failed receive"
        );
        assert!(NativeFiles::list(batch.as_ref(), NodeId(1), None).is_err());
        assert!(
            batch
                .state
                .lock()
                .unwrap()
                .catalog
                .records
                .iter()
                .any(|record| record.node == Some(1))
        );
    }
}
