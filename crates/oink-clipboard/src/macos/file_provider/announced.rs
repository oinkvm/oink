//! Metadata-only publications become readable only after a matching source grant.
//! This module never creates a Remote, starts delivery, or begins a native drag.
use super::*;
use oink_foundation::content::{Holder, OfferId};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PreparationKey {
    pub offer: OfferId,
    pub native_generation: u64,
    /// Target-local admission generation, not a wire identity or a clock.
    pub source_session: u64,
    pub metadata_revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activation {
    pub key: PreparationKey,
    pub holder: Holder,
    pub grant_revision: u64,
}

type Current = Arc<dyn Fn() -> bool + Send + Sync>;

/// Non-cloneable owner of an announcement's local namespace. Wrap it in an Arc
/// at the lifecycle owner if worker completion and cancellation need to share it.
pub struct PreparedDrag {
    key: PreparationKey,
    registration: Registration,
    admission: Arc<Admission>,
    started: Instant,
    timing: PreparationTiming,
    trace: PublicationTrace,
}
impl std::fmt::Debug for PreparedDrag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedDrag")
            .field("items", &self.registration.entries.len())
            .finish_non_exhaustive()
    }
}
impl PreparedDrag {
    pub fn generation(&self) -> u64 {
        self.trace.generation
    }
    pub fn key(&self) -> PreparationKey {
        self.key
    }
    /// All preparation offsets still refer to this instant, even when Enter is later.
    pub fn started(&self) -> Instant {
        self.started
    }
    pub fn timing(&self) -> PreparationTiming {
        self.timing.clone()
    }
}
impl Drop for PreparedDrag {
    fn drop(&mut self) {
        if self.admission.abandon_prepared() {
            self.registration.cancel();
        }
        // Activated URLs belong to Registration/native readers. Dropping this
        // preparation handle must not cancel an independently admitted delivery.
    }
}

/// Blocking: call on a preparation worker, never on an input/AppKit thread.
/// `current` covers this preparation key and admitted source session. Pending
/// metadata must not call this API with invented names or placeholder nodes.
pub fn prepare_announced(
    key: PreparationKey,
    roots: Vec<FileNode>,
    current: &dyn Fn() -> bool,
) -> Result<PreparedDrag, ClipboardError> {
    prepare_announced_with_cancel(key, roots, &PreparationCancellation::new(), current)
}

/// Ordinary-only Announced registration with independently cancellable helper work.
pub fn prepare_announced_with_cancel(
    key: PreparationKey,
    roots: Vec<FileNode>,
    cancellation: &PreparationCancellation,
    current: &dyn Fn() -> bool,
) -> Result<PreparedDrag, ClipboardError> {
    let _running = cancellation.enter()?;
    prepare_metadata(
        key,
        roots,
        None,
        PublicationTrace::new(Some(PublicationOwner::Announced(key))),
        Instant::now(),
        &|| !cancellation.is_cancelled() && current(),
    )
}

/// Blocking Enter worker only. None leaves the ordinary attempt entirely intact.
/// Taking happens before any storage lock; cancellation never revokes a content grant.
pub fn try_prepare_enter_placeholder(
    activation: Activation,
    roots: Vec<FileNode>,
    current: &dyn Fn() -> bool,
) -> Result<Option<PreparedDrag>, ClipboardError> {
    let started = Instant::now();
    check_publication(started + MANAGER_TIMEOUT, current)?;
    // Validate before spending the one-shot identity or cancelling useful work.
    Admission::new(roots.clone()).map_err(error)?;
    let _foreground = placeholders::Foreground::enter();
    let Some(taken) = placeholders::take(&roots, &mut Default::default()) else {
        return Ok(None);
    };
    // Enter competes with the ordinary metadata publication. Cancelling it
    // here discards useful OS reconciliation and serializes native Begin on a
    // fresh typed rename. The lifecycle owner cancels only the losing worker.
    prepare_metadata(
        activation.key,
        roots,
        Some(taken),
        PublicationTrace::new(Some(PublicationOwner::Entered(activation.key, activation.holder.epoch))),
        started,
        current,
    )
    .map(Some)
}

fn prepare_metadata(
    key: PreparationKey,
    roots: Vec<FileNode>,
    selected: Option<placeholders::Taken>,
    mut trace: PublicationTrace,
    started: Instant,
    current: &dyn Fn() -> bool,
) -> Result<PreparedDrag, ClipboardError> {
    let mut timing = PreparationTiming::default();
    if let Some(taken) = &selected {
        timing.placeholder = taken.timing.clone();
        trace.stock_generation = taken.generation();
    }
    let metadata = (|| {
        check_publication(started + MANAGER_TIMEOUT, current)?;
        let admission = Admission::new(roots).map_err(error)?;
        *admission.origin.lock().unwrap() = Some(key.offer.clip.origin);
        let mut names = Names::new(crate::sync::files::TargetOs::MacOs);
        let entries = admission
            .roots
            .iter()
            .map(|node| names.entry(node))
            .collect::<io::Result<Vec<_>>>()
            .map_err(error)?;
        Ok((admission, entries))
    })();
    let (admission, entries) = match metadata {
        Ok(metadata) => metadata,
        Err(error) => {
            timing.registration_us = started.elapsed().as_micros() as u64;
            record_placeholder_path(&timing, &trace);
            return Err(error);
        }
    };
    let result = publish_inner(
        &entries,
        Arc::new(MetadataContents(admission.clone())),
        started + MANAGER_TIMEOUT,
        current,
        &mut trace,
        selected,
        &mut timing,
        monotonic_ns().saturating_sub(started.elapsed().as_nanos() as u64),
    );
    timing.registration_us = started.elapsed().as_micros() as u64;
    record_result(&result);
    let mut registration = result?;
    registration.admission = Some(admission.clone());
    if !current() {
        registration.cancel();
        return Err(ClipboardError::FileProvider(FileProviderReason::Cancelled));
    }
    Ok(PreparedDrag {
        key,
        registration,
        admission,
        started,
        timing,
        trace,
    })
}

/// Installs an already-authorized namespace without another OS URL lookup.
/// `current` is CONTENT grant validity, including legitimate Released/Drop and
/// independent delivery; the glue separately checks its stricter begin_current.
/// The namespace must enforce Hover/Drop permissions itself. This method grants
/// neither Drop nor permission to inject a native DOWN.
pub fn activate_prepared(
    prepared: &PreparedDrag,
    activation: Activation,
    namespace: Arc<dyn Namespace>,
    current: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<Registration, ClipboardError> {
    let _foreground = placeholders::Foreground::enter();
    if activation.key != prepared.key {
        tracing::warn!(cause = 36, "File Provider prepared item identity rejected");
        return Err(ClipboardError::FileProvider(FileProviderReason::ItemIdentityMismatch));
    }
    namespace.prepare().map_err(error)?;
    let roots = namespace.roots();
    // FILEGROUPDESCRIPTOR announcements can have unknown sizes and version zero.
    // Fulfilling their grant supplies sizes/versions (also for directories).
    // The prepared URLs bind ordered node identities and names, not those early
    // content observations. Validate after prepare, which can complete the grant.
    if !same_root_identities(&prepared.admission.roots, &roots) {
        tracing::warn!(cause = 36, "File Provider prepared item identity rejected");
        return Err(ClipboardError::FileProvider(FileProviderReason::ItemIdentityMismatch));
    }
    let pin = namespace.local_batch();
    let result = {
        // The prepared Registration and local pin already protect cache lifetime.
        // Do not acquire STORAGE here: cache cleanup may hold it during an OS
        // wait. Only this batch's catalog/admission locks are needed.
        prepared.admission.activate(activation, namespace.clone(), current, || {
            let mut state = prepared.registration.batch.state.lock().unwrap();
            if state.retired {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let mut next = state.catalog.clone();
            for root in &roots {
                let record = next
                    .records
                    .iter_mut()
                    .find(|record| record.node == Some(root.id.0))
                    .ok_or(io::ErrorKind::InvalidData)?;
                record.size = match root.size {
                    ContentSize::Known(size) => Some(size),
                    ContentSize::Unknown => None,
                };
                record.modified = root.modified_ms;
                record.version = root.version;
            }
            next.source = namespace.source_link();
            save_catalog(&prepared.registration.batch.catalog, &next)?;
            state.catalog = next;
            Ok(())
        })
    };
    if let Err(e) = result {
        // A conflicting/stale call cannot replace a previous activation, but it
        // also must not revoke the previously admitted delivery.
        if prepared.admission.is_prepared_or_retired() {
            prepared.registration.cancel();
        }
        return Err(
            if matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::Interrupted) {
                ClipboardError::FileProvider(FileProviderReason::Cancelled)
            } else {
                error(e)
            },
        );
    }
    native::bind(&prepared.registration.batch, &namespace).map_err(error)?;
    if CONTAINER.get().is_some() {
        signal_changes();
    }
    let mut registration = prepared.registration.clone();
    for (entry, root) in registration.entries.iter_mut().zip(&roots) {
        entry.size = match root.size {
            ContentSize::Known(size) => size,
            ContentSize::Unknown => 0,
        };
        entry.modified_ms = root.modified_ms;
    }
    registration._content_pin = pin;
    prepared.trace.activated(activation.holder.epoch);
    Ok(registration)
}

fn same_root_identities(announced: &[FileNode], granted: &[FileNode]) -> bool {
    announced.len() == granted.len()
        && announced.iter().zip(granted).all(|(old, new)| {
            new.valid() && old.id == new.id && old.parent == new.parent && old.name == new.name && old.kind == new.kind
        })
}

/// Stop new admissions and retire the catalog/working-set identity. Readers
/// already admitted hold their own namespace and batch pins; their content
/// grant and the source still decide whether they can finish.
pub fn revoke_prepared(prepared: &PreparedDrag) {
    prepared.registration.cancel();
}

struct MetadataContents(Arc<Admission>);
impl FileContents for MetadataContents {
    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        Some(self.0.clone())
    }
    fn open(&self, _: usize) -> io::Result<crate::FileReader> {
        Err(io::ErrorKind::PermissionDenied.into())
    }
}

#[derive(Clone)]
struct Lease {
    namespace: Arc<dyn Namespace>,
    current: Current,
}
impl Lease {
    fn check(&self) -> io::Result<()> {
        if self.namespace.source_link().is_some_and(|source| !source.provisional) || (self.current)() {
            Ok(())
        } else {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }
}
enum State {
    Prepared,
    Active { lease: Lease },
    Retired,
}
pub(super) struct Admission {
    origin: Mutex<Option<oink_foundation::DeviceId>>,
    roots: Vec<FileNode>,
    state: Mutex<State>,
}
impl Admission {
    fn new(roots: Vec<FileNode>) -> io::Result<Arc<Self>> {
        let mut ids = HashSet::new();
        if roots.is_empty()
            || roots.len() > crate::model::MAX_DISCOVERED_NODES
            || roots
                .iter()
                .any(|n| !n.valid() || n.parent.is_some() || !ids.insert(n.id))
            || serde_json::to_vec(&roots)?.len() > crate::model::INVENTORY_BYTES
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(Arc::new(Self {
            origin: Mutex::new(None),
            roots,
            state: Mutex::new(State::Prepared),
        }))
    }
    fn activate(
        &self,
        _activation: Activation,
        namespace: Arc<dyn Namespace>,
        current: Current,
        persist: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        match *state {
            State::Prepared => {}
            State::Active { .. } => return Err(io::ErrorKind::AlreadyExists.into()),
            State::Retired => return Err(io::ErrorKind::Interrupted.into()),
        }
        if !current() {
            *state = State::Retired;
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        persist()?;
        if !current() {
            *state = State::Retired;
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        *state = State::Active {
            lease: Lease { namespace, current },
        };
        Ok(())
    }
    fn admit(&self) -> io::Result<Lease> {
        let mut state = self.state.lock().unwrap();
        let State::Active { lease, .. } = &*state else {
            return Err(io::ErrorKind::PermissionDenied.into());
        };
        if let Err(e) = lease.check() {
            *state = State::Retired;
            return Err(e);
        }
        Ok(lease.clone())
    }
    pub(super) fn revoke(&self) {
        // Never call Namespace::cancel here: a real Drop can own an independent
        // delivery even after this host's URLs leave the working set.
        *self.state.lock().unwrap() = State::Retired;
    }
    fn abandon_prepared(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if !matches!(*state, State::Active { .. }) {
            *state = State::Retired;
            true
        } else {
            false
        }
    }
    fn is_prepared_or_retired(&self) -> bool {
        !matches!(*self.state.lock().unwrap(), State::Active { .. })
    }
}
impl Namespace for Admission {
    fn native_open(&self, node: NodeId, offset: u64) -> Option<io::Result<crate::FileReader>> {
        let lease = match self.admit() {
            Ok(lease) => lease,
            Err(error) => return Some(Err(error)),
        };
        lease.namespace.native_open(node, offset).map(|reader| {
            let reader = reader?;
            lease.check()?;
            Ok(Box::new(AuthorizedReader { lease, reader }) as crate::FileReader)
        })
    }
    fn source_link(&self) -> Option<crate::content::linked::Descriptor> {
        let State::Active { lease, .. } = &*self.state.lock().unwrap() else {
            return None;
        };
        lease.namespace.source_link()
    }
    fn origin(&self) -> Option<oink_foundation::DeviceId> {
        *self.origin.lock().unwrap()
    }
    fn roots(&self) -> Vec<FileNode> {
        match &*self.state.lock().unwrap() {
            State::Active { lease } => lease.namespace.roots(),
            _ => self.roots.clone(),
        }
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<crate::model::DirectoryPage> {
        let lease = self.admit()?;
        let page = lease.namespace.list(node, cursor)?;
        lease.check()?;
        Ok(page)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        let lease = self.admit()?;
        let entry = lease.namespace.select(node)?;
        lease.check()?;
        Ok(entry)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
        let lease = self.admit()?;
        let reader = lease.namespace.open(node, version, offset)?;
        lease.check()?;
        Ok(Box::new(AuthorizedReader { lease, reader }))
    }
    fn in_use(&self) -> bool {
        let State::Active { lease, .. } = &*self.state.lock().unwrap() else {
            return false;
        };
        lease.namespace.in_use()
    }
    fn local_batch(&self) -> Option<crate::store::Batch> {
        let State::Active { lease, .. } = &*self.state.lock().unwrap() else {
            return None;
        };
        lease.namespace.local_batch()
    }
    fn cache_batch(&self) -> Option<crate::store::Batch> {
        let State::Active { lease, .. } = &*self.state.lock().unwrap() else {
            return None;
        };
        lease.namespace.cache_batch()
    }
}
struct AuthorizedReader {
    lease: Lease,
    reader: crate::FileReader,
}
impl Read for AuthorizedReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.lease.check()?;
        let count = self.reader.read(out)?;
        self.lease.check()?;
        Ok(count)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::model::DirectoryPage;
    use oink_foundation::DeviceId;
    use std::sync::atomic::AtomicUsize;

    fn key() -> PreparationKey {
        PreparationKey {
            offer: OfferId {
                clip: ClipId {
                    origin: DeviceId([3; 32]),
                    seq: 7,
                },
                generation: 8,
            },
            native_generation: 9,
            source_session: 10,
            metadata_revision: 11,
        }
    }
    fn activation(key: PreparationKey) -> Activation {
        Activation {
            key,
            holder: Holder {
                device: DeviceId([4; 32]),
                epoch: 12,
            },
            grant_revision: 13,
        }
    }
    fn roots() -> Vec<FileNode> {
        vec![
            FileNode {
                exec_bits: None,
                id: NodeId(1),
                parent: None,
                name: "actual-name.txt".into(),
                kind: NodeKind::File,
                size: ContentSize::Unknown,
                version: 3,
                modified_ms: None,
            },
            FileNode {
                exec_bits: None,
                id: NodeId(2),
                parent: None,
                name: "actual-directory".into(),
                kind: NodeKind::Directory,
                size: ContentSize::Unknown,
                version: 4,
                modified_ms: None,
            },
        ]
    }
    struct Contents {
        roots: Vec<FileNode>,
        calls: AtomicUsize,
        cancelled: AtomicBool,
    }
    impl Contents {
        fn new(roots: Vec<FileNode>) -> Arc<Self> {
            Arc::new(Self {
                roots,
                calls: AtomicUsize::new(0),
                cancelled: AtomicBool::new(false),
            })
        }
    }
    impl Namespace for Contents {
        fn native_open(&self, _: NodeId, _: u64) -> Option<io::Result<crate::FileReader>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Some(Ok(Box::new(io::Cursor::new(b"received-copy".to_vec()))))
        }
        fn roots(&self) -> Vec<FileNode> {
            self.roots.clone()
        }
        fn list(&self, node: NodeId, _: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(DirectoryPage {
                directory: node,
                version: 4,
                entries: vec![],
                next: None,
            })
        }
        fn select(&self, node: NodeId) -> io::Result<FileNode> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut entry = self.roots.iter().find(|n| n.id == node).unwrap().clone();
            entry.size = ContentSize::Known(7);
            Ok(entry)
        }
        fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<crate::FileReader> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(io::Cursor::new(b"payload".to_vec())))
        }
        fn cancel(&self) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn received_reader_delegates_to_committed_native_namespace_without_source_open() {
        let admission = Admission::new(roots()).unwrap();
        assert!(admission.native_open(NodeId(1), 0).unwrap().is_err());
        let contents = Contents::new(roots());
        admission
            .activate(activation(key()), contents.clone(), Arc::new(|| true), || Ok(()))
            .unwrap();
        let mut reader = admission.native_open(NodeId(1), 0).unwrap().unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"received-copy");
        assert_eq!(contents.calls.load(Ordering::SeqCst), 1);
        admission.revoke();
        assert!(admission.native_open(NodeId(1), 0).unwrap().is_err());
    }
    #[test]
    fn returning_to_source_and_reannouncing_preserves_second_typed_parent() {
        fn take(pool: &mut placeholders::Pool, owner: PublicationOwner) -> Option<placeholders::Ready> {
            owner
                .permits_placeholder()
                .then(|| pool.checkout(1, 0, true, &mut Default::default()))
                .flatten()
        }
        fn finish(pool: &mut placeholders::Pool, generation: u64, name: &str, stock: u64) {
            assert!(pool.preparing(generation, name.into()));
            pool.finish(
                generation,
                Ok(placeholders::Ready {
                    generation: stock,
                    key: name.into(),
                    preparation_us: 2_000_000,
                }),
            );
        }
        let mut pool = placeholders::Pool::new();
        pool.configure(true, true);
        let generation = pool.start().unwrap();
        finish(&mut pool, generation, "first", 1);
        finish(&mut pool, generation, "second", 2);
        assert!(
            take(&mut pool, PublicationOwner::Announced(key())).is_none(),
            "broadcast announcement must not spend typed stock"
        );
        assert_eq!(
            take(&mut pool, PublicationOwner::Entered(key(), 1)).unwrap().generation,
            1
        );
        let refill = pool.start().unwrap();
        // The lifecycle owner permits hidden background work while remote/dragging.
        // Returning and repeated Announced publications do not change that permission.
        for _ in 0..5 {
            pool.configure(true, true);
            assert!(take(&mut pool, PublicationOwner::Announced(key())).is_none());
            assert!(pool.current(refill));
        }
        finish(&mut pool, refill, "refilled", 3);
        assert_eq!(
            take(&mut pool, PublicationOwner::Entered(key(), 2)).unwrap().generation,
            2
        );
        assert_eq!(
            take(&mut pool, PublicationOwner::Entered(key(), 3)).unwrap().generation,
            3
        );
    }

    #[test]
    fn no_stock_leaves_selection_to_ordinary_preparation() {
        assert!(
            try_prepare_enter_placeholder(activation(key()), roots(), &|| true)
                .unwrap()
                .is_none()
        );
    }

    // Local catalog/bridge fixture only: these tests do not claim to resolve an
    // OS provider URL or measure readiness on a real File Provider domain.
    fn prepared(roots: Vec<FileNode>) -> (tempfile::TempDir, Arc<Bridge>, PreparedDrag) {
        let root = tempfile::tempdir().unwrap();
        let admission = Admission::new(roots).unwrap();
        let path = root.path().join("fixture.json");
        let (batch, mut entries) = stage_publication(
            admission.clone(),
            "fixture",
            "secret".into(),
            0,
            &path,
            &|| Ok(()),
            &|| {},
        )
        .unwrap();
        let urls: Vec<_> = entries.iter().map(|e| root.path().join(&e.path)).collect();
        for (entry, url) in entries.iter_mut().zip(&urls) {
            entry.local_path = Some(url.clone());
        }
        {
            let mut state = batch.state.lock().unwrap();
            state.catalog.urls = urls.clone();
            state.persist(&path).unwrap();
        }
        *batch.urls.lock().unwrap() = urls;
        let bridge = Arc::new(Bridge {
            port: 0,
            batches: Mutex::new(HashMap::from([("fixture".into(), batch.clone())])),
        });
        let registration = Registration {
            diagnostic_publication: (Instant::now(), false),
            key: "fixture".into(),
            bridge: Arc::downgrade(&bridge),
            batch,
            entries,
            _content_pin: None,
            admission: Some(admission.clone()),
        };
        (
            root,
            bridge,
            PreparedDrag {
                key: key(),
                registration,
                admission,
                started: Instant::now(),
                timing: PreparationTiming::default(),
                trace: PublicationTrace::new(Some(PublicationOwner::Announced(key()))),
            },
        )
    }
    fn exchange(bridge: Arc<Bridge>, operation: &str, node: u64) -> Vec<u8> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = serve(stream, &bridge);
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        writeln!(
            client,
            "{}",
            serde_json::json!({"operation":operation,"token":"secret","batch":"fixture","node":node})
        )
        .unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).unwrap();
        worker.join().unwrap();
        bytes
    }

    #[test]
    fn preparation_keeps_true_metadata_and_refuses_system_readers_without_a_remote() {
        let (_root, bridge, prepared) = prepared(roots());
        let state = prepared.registration.batch.state.lock().unwrap();
        assert_eq!(state.catalog.records.len(), 3);
        assert_eq!(state.catalog.records[1].name, "actual-name.txt");
        assert_eq!(state.catalog.records[1].size, None); // unknown is not a known zero
        assert_eq!(state.catalog.records[2].children, None); // no invented empty listing
        drop(state);
        assert!(prepared.admission.select(NodeId(1)).is_err());
        assert!(prepared.admission.open(NodeId(1), 3, 0).is_err());
        assert!(prepared.admission.list(NodeId(2), None).is_err());
        assert!(batch_needed(&prepared.registration.batch, &[]));
        for (op, node) in [("read", 1), ("list", 2)] {
            let bytes = exchange(bridge.clone(), op, node);
            assert_eq!(u32::from_be_bytes(bytes[..4].try_into().unwrap()), u32::MAX);
        }
        assert_eq!(*prepared.registration.batch.readers.lock().unwrap(), 0);
    }

    #[test]
    fn activation_checks_every_identity_and_metadata_then_reuses_urls_once() {
        let (_root, _bridge, prepared) = prepared(roots());
        let contents = Contents::new(roots());
        let mut changed_keys = vec![key(); 4];
        changed_keys[0].source_session += 1;
        changed_keys[1].native_generation += 1;
        changed_keys[2].metadata_revision += 1;
        changed_keys[3].offer.generation += 1;
        for key in changed_keys {
            assert!(activate_prepared(&prepared, activation(key), contents.clone(), Arc::new(|| true)).is_err());
        }
        let mut wrong_roots = roots();
        wrong_roots[0].name = "different.txt".into();
        assert!(
            activate_prepared(
                &prepared,
                activation(key()),
                Contents::new(wrong_roots),
                Arc::new(|| true)
            )
            .is_err()
        );
        let urls = prepared.registration.top_level_urls();
        let registration =
            activate_prepared(&prepared, activation(key()), contents.clone(), Arc::new(|| true)).unwrap();
        assert_eq!(registration.top_level_urls(), urls);
        assert_eq!(contents.calls.load(Ordering::SeqCst), 0); // activation reads no content/list
        let mut another_epoch = activation(key());
        another_epoch.holder.epoch += 1;
        assert!(activate_prepared(&prepared, another_epoch, contents.clone(), Arc::new(|| true)).is_err());
        assert!(prepared.admission.select(NodeId(1)).is_ok()); // conflict did not revoke valid owner
        drop(prepared);
        assert!(registration.batch.contents.lock().unwrap().select(NodeId(1)).is_ok());
        registration.cancel();
    }

    #[test]
    fn virtual_descriptor_grants_refine_metadata_without_replacing_prepared_urls() {
        // tools/dnd-held-source/fixture-windows.cpp FILEGROUPDESCRIPTORW:
        // folder, folder\\first.bin (no FD_FILESIZE), second.bin (3072), empty.
        // No FD_WRITESTIME. The nested file is discovered after activation.
        let harness = vec![
            FileNode {
                exec_bits: None,
                id: NodeId(1),
                parent: None,
                name: "folder".into(),
                kind: NodeKind::Directory,
                size: ContentSize::Unknown,
                version: 0,
                modified_ms: None,
            },
            FileNode {
                exec_bits: None,
                id: NodeId(3),
                parent: None,
                name: "second.bin".into(),
                kind: NodeKind::File,
                size: ContentSize::Known(3072),
                version: 0,
                modified_ms: None,
            },
            FileNode {
                exec_bits: None,
                id: NodeId(4),
                parent: None,
                name: "empty".into(),
                kind: NodeKind::Directory,
                size: ContentSize::Unknown,
                version: 0,
                modified_ms: None,
            },
        ];
        let image = FileNode {
            exec_bits: None,
            id: NodeId(1),
            parent: None,
            name: "image.png".into(),
            kind: NodeKind::File,
            size: ContentSize::Unknown,
            version: 0,
            modified_ms: None,
        };
        for announced in [harness, vec![image]] {
            let (_root, _bridge, prepared) = prepared(announced.clone());
            let urls = prepared.registration.top_level_urls();
            let mut granted = announced;
            for node in &mut granted {
                node.version = 1;
                if node.kind == NodeKind::File {
                    node.size = ContentSize::Known(3072);
                }
            }
            let registration = activate_prepared(
                &prepared,
                activation(key()),
                Contents::new(granted.clone()),
                Arc::new(|| true),
            )
            .unwrap();
            assert_eq!(registration.top_level_urls(), urls);
            assert_eq!(prepared.admission.roots(), granted);
            let catalog: Catalog =
                serde_json::from_slice(&std::fs::read(&registration.batch.catalog).unwrap()).unwrap();
            for node in &granted {
                let record = catalog
                    .records
                    .iter()
                    .find(|record| record.node == Some(node.id.0))
                    .unwrap();
                assert_eq!(record.version, node.version);
                assert_eq!(record.size, if node.kind == NodeKind::File { Some(3072) } else { None });
            }
            for (entry, node) in registration.entries.iter().zip(granted) {
                assert_eq!(entry.size, if node.kind == NodeKind::File { 3072 } else { 0 });
            }
            registration.cancel();
        }
    }

    #[test]
    fn mac_promise_and_readonly_app_metadata_refine_existing_urls() {
        use crate::content::namespace::Local;
        struct Completing {
            early: Vec<FileNode>,
            granted: Vec<FileNode>,
            ready: AtomicBool,
        }
        impl Namespace for Completing {
            fn prepare(&self) -> io::Result<()> {
                self.ready.store(true, Ordering::Release);
                Ok(())
            }
            fn roots(&self) -> Vec<FileNode> {
                if self.ready.load(Ordering::Acquire) {
                    self.granted.clone()
                } else {
                    self.early.clone()
                }
            }
            fn list(&self, _: NodeId, _: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
                unreachable!()
            }
            fn select(&self, _: NodeId) -> io::Result<FileNode> {
                unreachable!()
            }
            fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<crate::FileReader> {
                unreachable!()
            }
        }
        let files = tempfile::tempdir().unwrap();
        let image = files.path().join("promised.png");
        std::fs::write(&image, b"synthetic promised bytes").unwrap();
        let app = files.path().join("Fixture.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), b"synthetic bundle metadata").unwrap();
        let mut permissions = std::fs::metadata(&app).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&app, permissions).unwrap();
        for (path, promise) in [(image, true), (app.clone(), false)] {
            let local = Local::new(vec![(NodeId(1), path)]).unwrap();
            let mut granted = local.roots();
            let mut early = granted.clone();
            if promise {
                // Named NSFilePromiseProvider inventory before its write callback.
                early[0].size = ContentSize::Unknown;
                early[0].version = 0;
                early[0].modified_ms = None;
            } else {
                // Native readonly package roots initially have version zero;
                // a fulfilled directory uses its actual enumeration version.
                assert_eq!(early[0].kind, NodeKind::Directory);
                assert_eq!(early[0].version, 0);
                granted[0].version = local.list(NodeId(1), None).unwrap().version;
            }
            assert_ne!(early, granted);
            let (_root, _bridge, prepared) = prepared(early.clone());
            let urls = prepared.registration.top_level_urls();
            let contents = Arc::new(Completing {
                early,
                granted: granted.clone(),
                ready: AtomicBool::new(false),
            });
            let registration = activate_prepared(&prepared, activation(key()), contents, Arc::new(|| true)).unwrap();
            assert_eq!(registration.top_level_urls(), urls);
            assert_eq!(prepared.admission.roots(), granted);
            let catalog: Catalog =
                serde_json::from_slice(&std::fs::read(&registration.batch.catalog).unwrap()).unwrap();
            let root = catalog.records.iter().find(|r| r.node == Some(1)).unwrap();
            assert_eq!(root.modified, granted[0].modified_ms);
            assert_eq!(root.version, granted[0].version);
            registration.cancel();
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(app, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn refined_metadata_cannot_replace_the_announced_tree() {
        let original = roots();
        let mut cases = vec![original.clone(); 6];
        cases[0][0].id = NodeId(20);
        cases[1][0].parent = Some(NodeId(2));
        cases[2][0].name = "renamed".into();
        cases[3][0].kind = NodeKind::Directory;
        cases[4].reverse();
        cases[5].pop();
        for changed in cases {
            assert!(!same_root_identities(&original, &changed));
        }
    }

    #[test]
    fn cancelling_ordinary_attempt_does_not_revoke_an_activated_delivery() {
        let cancel = PreparationCancellation::new();
        let running = cancel.enter().unwrap();
        let (_root, bridge, prepared) = prepared(roots());
        drop(running);
        let registration =
            activate_prepared(&prepared, activation(key()), Contents::new(roots()), Arc::new(|| true)).unwrap();
        cancel.cancel_and_wait();
        drop(prepared);
        let bytes = exchange(bridge, "read", 1);
        assert_eq!(u32::from_be_bytes(bytes[..4].try_into().unwrap()), u32::MAX - 1);
        assert!(bytes.windows(7).any(|chunk| chunk == b"payload"));
        assert!(registration.batch.catalog.exists());
        registration.cancel();
    }

    #[test]
    fn revoked_preparation_and_late_completion_cannot_activate_or_leave_a_catalog() {
        let (_root, bridge, prepared) = prepared(roots());
        let path = prepared.registration.batch.catalog.clone();
        revoke_prepared(&prepared);
        revoke_prepared(&prepared);
        assert!(!path.exists());
        assert!(bridge.batches.lock().unwrap().is_empty());
        assert!(activate_prepared(&prepared, activation(key()), Contents::new(roots()), Arc::new(|| true)).is_err());
        assert!(!path.exists());
        let (_root2, bridge2, prepared2) = self::prepared(roots());
        let path2 = prepared2.registration.batch.catalog.clone();
        drop(prepared2); // completion arrived after owner cancellation, simply discarded
        assert!(!path2.exists());
        assert!(bridge2.batches.lock().unwrap().is_empty());
    }

    #[test]
    fn cancelled_at_activation_or_during_persistence_never_opens_the_gate() {
        let (_root, bridge, prepared) = prepared(roots());
        assert!(activate_prepared(&prepared, activation(key()), Contents::new(roots()), Arc::new(|| false)).is_err());
        assert!(bridge.batches.lock().unwrap().is_empty());
        let gate = Admission::new(roots()).unwrap();
        let live = Arc::new(AtomicBool::new(true));
        let current = live.clone();
        assert!(
            gate.activate(
                activation(key()),
                Contents::new(roots()),
                Arc::new(move || current.load(Ordering::Acquire)),
                || {
                    live.store(false, Ordering::Release);
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(gate.select(NodeId(1)).is_err());
        live.store(true, Ordering::Release); // even a buggy owner cannot revive a terminal gate
        assert!(
            gate.activate(activation(key()), Contents::new(roots()), Arc::new(|| true), || Ok(()))
                .is_err()
        );
    }

    #[test]
    fn retirement_keeps_an_admitted_reader_but_content_revocation_stops_it() {
        let (_root, bridge, prepared) = prepared(roots());
        let live = Arc::new(AtomicBool::new(true));
        let current = live.clone();
        let contents = Contents::new(roots());
        let registration = activate_prepared(
            &prepared,
            activation(key()),
            contents.clone(),
            Arc::new(move || current.load(Ordering::Acquire)),
        )
        .unwrap();
        let namespace = registration.batch.contents.lock().unwrap().clone();
        let mut reader = namespace.open(NodeId(1), 3, 0).unwrap();
        let mut first = [0u8; 1];
        reader.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"p");
        registration.cancel(); // explicit URL retirement preserves this already admitted reader
        assert!(bridge.batches.lock().unwrap().is_empty());
        assert!(!contents.cancelled.load(Ordering::SeqCst));
        assert!(namespace.open(NodeId(1), 3, 0).is_err());
        reader.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"a");
        live.store(false, Ordering::Release); // explicit content-grant revocation
        assert!(reader.read(&mut first).is_err());
    }

    #[test]
    fn grant_is_checked_again_on_each_ipc_and_reader_operation() {
        let (_root, bridge, prepared) = prepared(roots());
        let contents = Contents::new(roots());
        let live = Arc::new(AtomicBool::new(true));
        let current = live.clone();
        let _registration = activate_prepared(
            &prepared,
            activation(key()),
            contents.clone(),
            Arc::new(move || current.load(Ordering::Acquire)),
        )
        .unwrap();
        let bytes = exchange(bridge.clone(), "read", 1);
        assert_eq!(u32::from_be_bytes(bytes[..4].try_into().unwrap()), u32::MAX - 1);
        assert!(bytes.windows(7).any(|bytes| bytes == b"payload"));
        let count = contents.calls.load(Ordering::SeqCst);
        live.store(false, Ordering::Release);
        let bytes = exchange(bridge.clone(), "read", 1);
        assert_eq!(u32::from_be_bytes(bytes[..4].try_into().unwrap()), u32::MAX);
        assert_eq!(contents.calls.load(Ordering::SeqCst), count);
        live.store(true, Ordering::Release);
        assert!(prepared.admission.select(NodeId(1)).is_err());
        let path = prepared.registration.batch.catalog.clone();
        drop(prepared);
        assert!(!path.exists());
    }

    #[test]
    fn activation_never_waits_for_another_publications_os_wait() {
        let (_root, _bridge, prepared) = prepared(roots());
        // A real publication holds STORAGE while waiting for getUserVisibleURL.
        // Activation must only use this prepared batch's admission/catalog locks.
        let storage = STORAGE.lock().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = activate_prepared(&prepared, activation(key()), Contents::new(roots()), Arc::new(|| true));
            send.send(result.is_ok()).unwrap();
            (prepared, result)
        });
        let activated = receive.recv_timeout(Duration::from_secs(3));
        drop(storage); // unlock even on failure so the worker can join
        let (_prepared, registration) = worker.join().unwrap();
        assert!(activated.unwrap());
        registration.unwrap().cancel();
    }

    #[test]
    fn pending_or_invalid_metadata_is_rejected_before_contacting_file_provider() {
        assert!(prepare_announced(key(), roots(), &|| false).is_err());
        assert!(prepare_announced(key(), vec![], &|| true).is_err());
        let mut duplicate = roots();
        duplicate[1].id = duplicate[0].id;
        assert!(prepare_announced(key(), duplicate, &|| true).is_err());
        let mut nested = roots();
        nested[1].parent = Some(NodeId(1));
        assert!(prepare_announced(key(), nested, &|| true).is_err());
        fn send<T: Send>() {}
        send::<PreparedDrag>();
    }
}
