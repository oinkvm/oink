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
    if activation.key != prepared.key || namespace.roots() != prepared.admission.roots {
        return Err(error("prepared metadata or identity changed"));
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
            state.catalog.batch = pin.as_ref().map(crate::store::Batch::id);
            state.persist(&prepared.registration.batch.catalog)
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
    let weak = Arc::downgrade(&prepared.admission);
    namespace.on_stored(Box::new(move |local| {
        let Some(admission) = weak.upgrade() else { return };
        admission.stored(activation, local);
        if let Some(root) = CONTAINER.get() {
            crate::cache::changed(root);
        }
    }));
    let mut registration = prepared.registration.clone();
    registration._content_pin = pin;
    prepared.trace.activated(activation.holder.epoch);
    Ok(registration)
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
    stored: Arc<AtomicBool>,
}
impl Lease {
    fn check(&self) -> io::Result<()> {
        if self.stored.load(Ordering::Acquire) || (self.current)() {
            Ok(())
        } else {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }
}
enum State {
    Prepared,
    Active { activation: Activation, lease: Lease },
    Retired,
}
pub(super) struct Admission {
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
            roots,
            state: Mutex::new(State::Prepared),
        }))
    }
    fn activate(
        &self,
        activation: Activation,
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
            activation,
            lease: Lease {
                namespace,
                current,
                stored: Arc::new(AtomicBool::new(false)),
            },
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
    fn stored(&self, expected: Activation, local: crate::content::Completed) {
        let mut state = self.state.lock().unwrap();
        if let State::Active { activation, lease } = &mut *state
            && *activation == expected
        {
            lease.namespace = Arc::new(local);
            // Verified owned bytes no longer need a live source lease. Existing
            // readers share this flag while retaining their independently pinned
            // Remote, whose Stored state also permits completed local reads.
            lease.stored.store(true, Ordering::Release);
        }
    }
}
impl Namespace for Admission {
    fn roots(&self) -> Vec<FileNode> {
        self.roots.clone()
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
                id: NodeId(1),
                parent: None,
                name: "actual-name.txt".into(),
                kind: NodeKind::File,
                size: ContentSize::Unknown,
                version: 3,
                modified_ms: None,
            },
            FileNode {
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
        assert!(state.catalog.batch.is_none());
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

    struct Stores(crate::content::Completed);
    impl Namespace for Stores {
        fn roots(&self) -> Vec<FileNode> {
            self.0.roots()
        }
        fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
            self.0.list(node, cursor)
        }
        fn select(&self, node: NodeId) -> io::Result<FileNode> {
            self.0.select(node)
        }
        fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
            self.0.open(node, version, offset)
        }
        fn local_batch(&self) -> Option<crate::store::Batch> {
            Namespace::local_batch(&self.0)
        }
        fn on_stored(&self, ready: Box<dyn FnOnce(crate::content::Completed) + Send>) {
            ready(self.0.clone());
        }
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
    fn completed_bytes_shed_remote_grant_but_explicit_retirement_still_blocks_new_readers() {
        let data = tempfile::tempdir().unwrap();
        let owned = crate::content::Completed::test_single_file(data.path());
        let local = crate::content::Completed::restore(data.path(), owned.id()).unwrap();
        let (_root, _bridge, prepared) = prepared(local.roots());
        let contents = Arc::new(Stores(local));
        let live = Arc::new(AtomicBool::new(true));
        let current = live.clone();
        let registration = activate_prepared(
            &prepared,
            activation(key()),
            contents,
            Arc::new(move || current.load(Ordering::Acquire)),
        )
        .unwrap();
        assert_eq!(
            prepared.registration.batch.state.lock().unwrap().catalog.batch,
            Some(owned.id())
        );
        live.store(false, Ordering::Release);
        let mut reader = prepared.admission.open(NodeId(1), 7, 0).unwrap();
        drop(prepared);
        registration.cancel();
        // A delayed completion notification after retirement cannot republish
        // this namespace or open a fresh capability.
        registration.admission.as_ref().unwrap().stored(
            activation(key()),
            crate::content::Completed::restore(data.path(), owned.id()).unwrap(),
        );
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"cached");
        assert!(
            registration
                .batch
                .contents
                .lock()
                .unwrap()
                .open(NodeId(1), 7, 0)
                .is_err()
        );
        assert!(!registration.batch.catalog.exists());
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
