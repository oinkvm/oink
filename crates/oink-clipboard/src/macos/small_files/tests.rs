use super::*;
use crate::model::{AUTO_PULL_BYTES, DirectoryCursor, DirectoryPage, NodeKind};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Files {
    roots: Vec<FileNode>,
    opened: AtomicUsize,
    bytes: Vec<u8>,
    fail: Option<Unavailable>,
    gate: Option<Arc<(Mutex<bool>, Condvar)>>,
}
impl Namespace for Files {
    fn roots(&self) -> Vec<FileNode> {
        self.roots.clone()
    }
    fn list(&self, _: NodeId, _: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        panic!("no directories")
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        if let Some(gate) = &self.gate {
            let lock = gate.0.lock().unwrap();
            drop(gate.1.wait_while(lock, |open| !*open).unwrap());
        }
        if let Some(reason) = self.fail {
            return Err(content::unavailable(reason));
        }
        Ok(self.roots.iter().find(|root| root.id == node).unwrap().clone())
    }
    fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<crate::FileReader> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(io::Cursor::new(self.bytes.clone())))
    }
}
fn file(id: u64, name: &str, size: u64) -> FileNode {
    FileNode {
        id: NodeId(id),
        parent: None,
        name: name.into(),
        kind: NodeKind::File,
        size: ContentSize::Known(size),
        version: 1,
        modified_ms: Some(1_700_000_000_000),
        exec_bits: None,
    }
}
fn files(roots: Vec<FileNode>, bytes: &[u8]) -> Arc<Files> {
    Arc::new(Files {
        roots,
        opened: AtomicUsize::new(0),
        bytes: bytes.into(),
        fail: None,
        gate: None,
    })
}
fn policy(root: &Path, files: Arc<Files>) -> Arc<SmallFiles> {
    SmallFiles::new(root.into(), files.clone(), files.roots(), Arc::new(|| Ok(())))
}
fn path(url: &str) -> PathBuf {
    let url = objc2_foundation::NSURL::URLWithString(&objc2_foundation::NSString::from_str(url)).unwrap();
    PathBuf::from(url.path().unwrap().to_string())
}
fn inventory(roots: Vec<FileNode>) -> crate::model::Inventory {
    use content::Provider;
    let mut inventory = content::tests::fixture().inventory();
    inventory.items.clear();
    inventory.roots = roots;
    inventory
}
#[test]
fn selection_uses_the_shared_auto_fetch_budget() {
    assert!(!eligible(&inventory(vec![])));
    assert!(eligible(&inventory(vec![file(1, "empty", 0)])));
    assert!(eligible(&inventory(vec![file(1, "a", AUTO_PULL_BYTES)])));
    assert!(!eligible(&inventory(vec![file(1, "a", AUTO_PULL_BYTES + 1)])));
    assert!(eligible(&inventory(vec![
        file(1, "a", 1),
        file(2, "b", AUTO_PULL_BYTES - 1)
    ])));
    assert!(!eligible(&inventory(vec![
        file(1, "a", 1),
        file(2, "b", AUTO_PULL_BYTES)
    ])));
    assert!(!eligible(&inventory(vec![file(1, "a", u64::MAX)])));
    let mut node = file(1, "dir", 0);
    node.kind = NodeKind::Directory;
    assert!(!eligible(&inventory(vec![file(2, "a", 1), node.clone()])));
    node.kind = NodeKind::File;
    node.size = ContentSize::Unknown;
    assert!(!eligible(&inventory(vec![node])));
}

#[test]
#[ignore = "device: requires injectable small-file backend completion"]
fn native_backend_selects_small_cache_without_a_file_provider_manager() {
    use crate::{
        ClipboardBackend,
        content::Provider,
        model::{Medium, NativeFormat, Representation, RepresentationData, RepresentationId},
    };
    const CHILD: &str = "OINK_TEST_SMALL_FILE_BACKEND_CHILD";
    const NAME: &str = "macos::small_files::tests::native_backend_selects_small_cache_without_a_file_provider_manager";
    if std::env::var_os(CHILD).is_none() {
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME])
                .env(CHILD, "1")
                .status()
                .unwrap()
                .success()
        );
        return;
    }
    objc2::rc::autoreleasepool(|_| {
        let directory = tempfile::tempdir().unwrap();
        ROOT.set(directory.path().join("cache")).unwrap();
        let data = files(vec![file(1, "actual.txt", 3)], b"abc");
        let contents = content::namespace::Files::new(data.clone(), crate::sync::files::TargetOs::current()).unwrap();
        let source = content::with_files(Arc::new(content::tests::fixture()), data.clone());
        let mut inventory = source.inventory();
        for representation in &mut inventory.items[0].representations {
            representation.data = crate::model::RepresentationData::Reader {
                size: ContentSize::Known(8),
            };
        }
        inventory.items[0].file = Some(NodeId(1));
        inventory.items[0].representations.push(Representation {
            id: RepresentationId(10),
            format: NativeFormat::Mac("public.file-url".into()),
            standard: None,
            medium: Medium::Bytes,
            aspect: 1,
            index: -1,
            data: RepresentationData::File(NodeId(1)),
        });
        let board = crate::macos::test_pasteboard::new();
        let backend = crate::macos::MacClipboard {
            name: Some(board.name().to_string()),
            watcher: Mutex::new(None),
            registration: Arc::new(Mutex::new(None)),
            local: Arc::new(Mutex::new(None)),
        };
        backend
            .write_native(NativeSpec {
                inventory,
                contents: source,
                files: contents.roots().to_vec(),
                file_contents: Some(contents),
                sensitivity: crate::Sensitivity::Normal,
            })
            .unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while data.opened.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        let url = board
            .stringForType(&objc2_foundation::NSString::from_str("public.file-url"))
            .unwrap();
        assert_eq!(fs::read(path(&url.to_string())).unwrap(), b"abc");
        board.clearContents();
    });
}
#[test]
fn prefetch_writes_once_preserves_names_metadata_and_expires_after_24_hours() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let files = files(vec![file(1, "猫 report.txt", 3), file(2, "猫 REPORT.txt", 3)], b"abc");
    let policy = policy(&cache, files.clone());
    assert!(!cache.exists(), "constructing an offer cannot create or fetch contents");
    policy.start();
    let deadline = Instant::now() + Duration::from_secs(2);
    let first = path(&policy.resolve(NodeId(1), deadline).unwrap());
    let second = path(&policy.resolve(NodeId(2), deadline).unwrap());
    assert_eq!(first.file_name().unwrap(), "猫 report.txt");
    assert_ne!(
        first.file_name().unwrap().to_str().unwrap().to_lowercase(),
        second.file_name().unwrap().to_str().unwrap().to_lowercase()
    );
    assert_eq!(second.extension().unwrap(), "txt");
    assert_eq!(fs::read(&first).unwrap(), b"abc");
    assert_eq!(fs::read(&second).unwrap(), b"abc");
    assert_eq!(files.opened.load(Ordering::SeqCst), 2);
    assert_eq!(fs::metadata(&first).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(&cache).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(
        fs::metadata(&first).unwrap().modified().unwrap(),
        UNIX_EPOCH + Duration::from_millis(1_700_000_000_000)
    );
    drop(policy);
    let record = cleanup::load(first.parent().unwrap().parent().unwrap()).unwrap();
    maintain(&cache, false, record.created_ms + RETENTION_MS).unwrap();
    assert!(first.exists(), "exactly 24h has not exceeded retention");
    let scan = maintain(&cache, false, record.created_ms + RETENTION_MS + 1).unwrap();
    assert_reclaimed(&first, &scan);
}
#[test]
fn failure_is_terminal_without_url_or_partial_delivery() {
    let root = tempfile::tempdir().unwrap();
    let files = Arc::new(Files {
        roots: vec![file(1, "a", 3)],
        opened: AtomicUsize::new(0),
        bytes: vec![],
        fail: Some(Unavailable::Permission),
        gate: None,
    });
    let policy = policy(root.path(), files.clone());
    for _ in 0..2 {
        assert_eq!(
            content::reason(
                &policy
                    .resolve(NodeId(1), Instant::now() + Duration::from_secs(1))
                    .unwrap_err()
            ),
            Unavailable::Permission
        );
    }
    assert_eq!(files.opened.load(Ordering::SeqCst), 0);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1); // lock only
}
#[test]
#[ignore = "device: requires injectable small-file clock and worker completion"]
fn timeout_bounds_callback_and_late_worker_cannot_publish() {
    let root = tempfile::tempdir().unwrap();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let files = Arc::new(Files {
        roots: vec![file(1, "a", 3)],
        opened: AtomicUsize::new(0),
        bytes: b"abc".into(),
        fail: None,
        gate: Some(gate.clone()),
    });
    let policy = policy(root.path(), files);
    let start = Instant::now();
    assert_eq!(
        content::reason(
            &policy
                .resolve(NodeId(1), start + Duration::from_millis(40))
                .unwrap_err()
        ),
        Unavailable::NoProgress
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if Arc::strong_count(&policy) == 1 {
            break;
        }
        assert!(Instant::now() < until);
        std::thread::yield_now();
    }
    assert_eq!(
        content::reason(
            &policy
                .resolve(NodeId(1), Instant::now() + Duration::from_secs(1))
                .unwrap_err()
        ),
        Unavailable::NoProgress
    );
    assert!(
        fs::read_dir(root.path())
            .unwrap()
            .all(|entry| entry.unwrap().file_name() == ".lock")
    );
}

#[test]
#[ignore = "device: requires injectable small-file clock and worker completion"]
fn shared_file_url_budget_timeout_is_terminal_for_cache_without_provider_fallback() {
    use crate::macos::clipboard_files::FileUrls;
    let root = tempfile::tempdir().unwrap();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let data = Arc::new(Files {
        roots: vec![file(1, "a", 3)],
        opened: AtomicUsize::new(0),
        bytes: b"abc".into(),
        fail: None,
        gate: Some(gate.clone()),
    });
    let policy = policy(root.path(), data);
    let spec = NativeSpec {
        inventory: inventory(policy.roots.clone()),
        contents: Arc::new(content::tests::fixture()),
        files: vec![],
        file_contents: None,
        sensitivity: crate::Sensitivity::Normal,
    };
    let urls = FileUrls::with_source(spec, Arc::new(Source(policy.clone()))).unwrap();
    let clock = Arc::new(content::native_read::TestClock::default());
    clock.scope(|| {
        let _callback = content::native_read::Callback::begin();
        clock.expire();
        assert_eq!(
            content::reason(&urls.read(NodeId(1)).unwrap_err()),
            Unavailable::NoProgress
        );
    });
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let until = Instant::now() + Duration::from_secs(2);
    while Arc::strong_count(&policy) > 2 {
        assert!(Instant::now() < until);
        std::thread::yield_now();
    }
    let _callback = content::native_read::Callback::begin();
    assert_eq!(
        content::reason(&urls.read(NodeId(1)).unwrap_err()),
        Unavailable::NoProgress
    );
    assert!(!fs::read_dir(root.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|extension| extension == "ready")
    }));
}

#[test]
fn shared_file_url_source_observes_current_cancellation_during_prefetch() {
    use crate::macos::clipboard_files::FileUrls;
    let root = tempfile::tempdir().unwrap();
    let current = Arc::new(AtomicBool::new(true));
    let live = current.clone();
    let guard: ReaderGuard = Arc::new(move || {
        if live.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(content::unavailable(Unavailable::Cancelled))
        }
    });
    let data = files(vec![file(1, "a", 3)], b"abc");
    let policy = SmallFiles::new(root.path().into(), data.clone(), data.roots(), guard.clone());
    // Pause final publication so cancellation is exercised while the source is
    // pending, even on a host where the three-byte prefetch finishes instantly.
    policy.await_publication.store(true, Ordering::Release);
    let spec = NativeSpec {
        inventory: inventory(policy.roots.clone()),
        contents: Arc::new(content::tests::fixture()),
        files: vec![],
        file_contents: None,
        sensitivity: crate::Sensitivity::Normal,
    };
    let urls = FileUrls::with_source_current(spec, Arc::new(Source(policy)), guard).unwrap();
    current.store(false, Ordering::Release);
    let _callback = content::native_read::Callback::begin();
    assert_eq!(
        content::reason(&urls.read(NodeId(1)).unwrap_err()),
        Unavailable::Cancelled
    );
    assert!(!fs::read_dir(root.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|extension| extension == "ready")
    }));
}
#[test]
fn restart_cleanup_skips_current_clipboard() {
    objc2::rc::autoreleasepool(|_| {
        let root = tempfile::tempdir().unwrap();
        let policy = policy(root.path(), files(vec![file(1, "a", 3)], b"abc"));
        let board = crate::macos::test_pasteboard::new();
        board.clearContents();
        policy.await_publication.store(true, Ordering::Release);
        policy.published(Some(&board.name().to_string()), board.changeCount() as u64);
        let file = path(
            &policy
                .resolve(NodeId(1), Instant::now() + Duration::from_secs(2))
                .unwrap(),
        );
        maintain(root.path(), true, u64::MAX).unwrap();
        assert!(file.exists(), "clear/expiry cannot delete the current clipboard");
        drop(policy); // protection is on disk, not a live native owner.
        maintain(root.path(), false, u64::MAX).unwrap();
        assert!(file.exists());
        board.clearContents();
        let scan = maintain(root.path(), false, u64::MAX).unwrap();
        assert_reclaimed(&file, &scan);
    });
}
#[test]
fn cleanup_removes_orphan_partials_and_skips_active_ones() {
    let root = tempfile::tempdir().unwrap();
    let partial = root.path().join(format!("{}.partial", "a".repeat(32)));
    private_dir(&partial).unwrap();
    let lock = lock_root(&partial).unwrap().unwrap();
    maintain(root.path(), true, u64::MAX).unwrap();
    assert!(partial.exists());
    drop(lock);
    maintain(root.path(), false, 0).unwrap();
    assert!(!partial.exists());
}

#[test]
fn native_consumer_lease_protects_a_batch_between_file_reads() {
    let root = tempfile::tempdir().unwrap();
    let policy = policy(root.path(), files(vec![file(1, "a", 3), file(2, "b", 3)], b"abc"));
    let file = path(
        &policy
            .resolve(NodeId(1), Instant::now() + Duration::from_secs(2))
            .unwrap(),
    );
    let lease = policy.lease().unwrap();
    maintain(root.path(), true, u64::MAX).unwrap();
    assert!(file.exists());
    drop(lease);
    let scan = maintain(root.path(), true, u64::MAX).unwrap();
    assert_reclaimed(&file, &scan);
}

#[test]
fn cleanup_refuses_a_redirected_cache_root() {
    let root = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let partial = external.path().join(format!("{}.partial", "a".repeat(32)));
    private_dir(&partial).unwrap();
    let cache = root.path().join("ClipboardFiles");
    std::os::unix::fs::symlink(external.path(), &cache).unwrap();
    assert!(maintain(&cache, true, u64::MAX).is_err());
    assert!(partial.exists());
}

#[test]
fn all_or_nothing_short_read_and_metadata_filename_are_safe() {
    let root = tempfile::tempdir().unwrap();
    let policy = policy(
        root.path(),
        files(vec![file(1, "first", 3), file(2, "second", 4)], b"abc"),
    );
    assert_eq!(
        content::reason(
            &policy
                .resolve(NodeId(1), Instant::now() + Duration::from_secs(2))
                .unwrap_err()
        ),
        Unavailable::ReadFailed
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    let policy = SmallFiles::new(
        root.path().into(),
        files(vec![file(1, RECORD, 3)], b"abc"),
        vec![file(1, RECORD, 3)],
        Arc::new(|| Ok(())),
    );
    let url = policy
        .resolve(NodeId(1), Instant::now() + Duration::from_secs(2))
        .unwrap();
    assert_eq!(fs::read(path(&url)).unwrap(), b"abc");
}

#[test]
fn retirement_prevents_late_publication_and_new_requests() {
    let root = tempfile::tempdir().unwrap();
    let policy = policy(root.path(), files(vec![file(1, "a", 3)], b"abc"));
    policy.retire();
    assert_eq!(
        content::reason(
            &policy
                .resolve(NodeId(1), Instant::now() + Duration::from_secs(2))
                .unwrap_err()
        ),
        Unavailable::Cancelled
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn cache_usage_and_clear_use_clipboard_files_category_without_quota_rejection() {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("ClipboardFiles");
    let policy = policy(&cache, files(vec![file(1, "a", 3)], b"abc"));
    let file = path(
        &policy
            .resolve(NodeId(1), Instant::now() + Duration::from_secs(2))
            .unwrap(),
    );
    let (usage, removed, incomplete) = cache_usage(root.path(), false);
    assert_eq!(usage.category, crate::cache::CacheCategory::ClipboardFiles);
    assert!(usage.bytes > 0);
    assert_eq!(removed, 0);
    assert!(!incomplete);
    let (usage, removed, incomplete) = cache_usage(root.path(), true);
    assert!(!incomplete);
    assert_eq!(usage.bytes, 0);
    assert!(removed > 0);
    assert!(!file.exists());
}

#[test]
#[ignore = "device: requires injectable small-file publication completion"]
fn native_item_declares_file_type_before_demand_and_preserves_companions() {
    use crate::content::Provider;
    use crate::model::{
        ContentItem, Inventory, ItemId, Medium, NativeFormat, Representation, RepresentationData, RepresentationId,
    };
    use objc2::rc::autoreleasepool;
    use objc2_foundation::NSString;
    use oink_foundation::clipboard::ClipPlatform;
    autoreleasepool(|_| {
        for platform in [ClipPlatform::Mac, ClipPlatform::Windows] {
            let root = tempfile::tempdir().unwrap();
            let data = files(vec![file(1, "a.txt", 3), file(2, "b.txt", 3)], b"abc");
            let policy = policy(root.path(), data.clone());
            policy.await_publication.store(true, Ordering::Release);
            let source = Arc::new(content::tests::fixture());
            let mut inventory: Inventory = source.inventory();
            inventory.platform = platform;
            inventory.roots = data.roots();
            inventory.items = if platform == ClipPlatform::Mac {
                inventory
                    .roots
                    .iter()
                    .enumerate()
                    .map(|(index, node)| ContentItem {
                        id: ItemId(index as u32),
                        virtual_files: false,
                        file: Some(node.id),
                        representations: vec![Representation {
                            id: RepresentationId(index as u32 + 10),
                            format: NativeFormat::Mac("public.file-url".into()),
                            standard: None,
                            medium: Medium::Bytes,
                            aspect: 1,
                            index: -1,
                            data: RepresentationData::File(node.id),
                        }],
                    })
                    .collect()
            } else {
                let mut item = source.inventory().items.remove(0);
                item.representations
                    .retain(|rep| rep.standard == Some(crate::model::StandardFormat::Text));
                vec![item]
            };
            if platform == ClipPlatform::Mac {
                inventory.items[0]
                    .representations
                    .push(source.inventory().items[0].representations[1].clone());
            }
            let spec = NativeSpec {
                inventory,
                contents: source,
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            };
            let board = crate::macos::test_pasteboard::new();
            let urls =
                crate::macos::clipboard_files::FileUrls::with_source(spec.clone(), Arc::new(Source(policy.clone())))
                    .unwrap();
            let count = crate::macos::content::write_lazy(
                Some(&board.name().to_string()),
                &spec,
                &HashMap::new(),
                Some(urls.clone()),
                &|| Ok(()),
            )
            .unwrap();
            urls.published(Some(&board.name().to_string()), count);
            let until = Instant::now() + Duration::from_secs(2);
            while !matches!(*policy.state.lock().unwrap(), State::Ready(_)) {
                assert!(Instant::now() < until, "prefetch must complete before a URL request");
                std::thread::yield_now();
            }
            let items = board.pasteboardItems().unwrap();
            assert_eq!(items.len(), 2);
            assert!(
                items
                    .iter()
                    .all(|item| item.types().containsObject(&NSString::from_str("public.file-url")))
            );
            assert_eq!(
                data.opened.load(Ordering::SeqCst),
                2,
                "automatic prefetch does not wait for file URL demand"
            );
            assert_eq!(
                items
                    .objectAtIndex(0)
                    .dataForType(&NSString::from_str("public.utf8-plain-text"))
                    .unwrap()
                    .to_vec(),
                b"original"
            );
            assert_eq!(
                data.opened.load(Ordering::SeqCst),
                2,
                "companion reads reuse the same completed file prefetch"
            );
            let url = items
                .objectAtIndex(0)
                .stringForType(&NSString::from_str("public.file-url"))
                .unwrap();
            assert_eq!(fs::read(path(&url.to_string())).unwrap(), b"abc");
            assert_eq!(data.opened.load(Ordering::SeqCst), 2);
            board.clearContents();
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_remote_and_manual_receive_share_cache_and_reach_stored() {
    use crate::content::{Provider, Registry, Remote, grants::SourceGrants, linked::RegistryLink, namespace::Local};
    use crate::model::Demand;
    use oink_foundation::{DeviceId, clipboard::ClipPlatform};
    for manual in [false, true] {
        let source_dir = tempfile::tempdir().unwrap();
        let receiver = tempfile::tempdir().unwrap();
        let source_path = source_dir.path().join("a.txt");
        fs::write(&source_path, b"actual remote file").unwrap();
        let grants = SourceGrants::load(&source_dir.path().join("grants"), [(DeviceId([2; 32]), [8; 32])]).unwrap();
        let registry = Arc::new(Registry::with_source_grants(grants));
        let source = content::with_files(
            Arc::new(content::tests::fixture()),
            Local::new(vec![(NodeId(1), source_path)]).unwrap(),
        );
        let mut inventory = source.inventory();
        for representation in &mut inventory.items[0].representations {
            representation.data = crate::model::RepresentationData::Reader {
                size: ContentSize::Known(8),
            };
        }
        // This transport scenario isolates file prefetch/admission. The model
        // tests separately cover the shared file/companion/inline budget.
        inventory.items.clear();
        let _lease = registry.register(source).unwrap();
        let remote = Remote::new(
            receiver.path(),
            crate::store::BatchId::random().unwrap(),
            inventory.clone(),
            ClipPlatform::Mac,
            Arc::new(RegistryLink(registry)),
            Demand::Clipboard,
        )
        .unwrap();
        let namespace = remote.namespace().unwrap();
        let cache = receiver.path().join("small");
        let policy = SmallFiles::new(cache.clone(), namespace.clone(), inventory.roots, Arc::new(|| Ok(())));
        assert!(namespace.source_link().is_none());
        // Match agent order: automatic_pull runs on Offer, before the platform
        // publication worker has registered the native cache receiver.
        if manual {
            assert!(remote.start_delivery());
        } else {
            assert!(remote.automatic_pull());
            assert_eq!(remote.progress().borrow().phase, content::TransferPhase::Offered);
            assert!(
                namespace.source_link().is_none(),
                "unbound receivers cannot acquire file authority"
            );
        }
        assert!(!cache.exists());
        let native: Arc<dyn NativeFiles> = policy.clone();
        namespace.bind_native(Arc::downgrade(&native)).unwrap();
        let mut prefetched = remote.progress();
        tokio::time::timeout(
            Duration::from_secs(2),
            prefetched.wait_for(|progress| progress.phase == content::TransferPhase::Stored),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            cache.exists(),
            "Offer automatic fetch must finish before the first URL read"
        );
        let owner = policy.clone();
        let url =
            tokio::task::spawn_blocking(move || owner.resolve(NodeId(1), Instant::now() + Duration::from_secs(2)))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(fs::read(path(&url)).unwrap(), b"actual remote file");
        let mut progress = remote.progress();
        tokio::time::timeout(
            Duration::from_secs(2),
            progress.wait_for(|progress| progress.phase == content::TransferPhase::Stored),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            fs::read_dir(&cache).unwrap().count(),
            2,
            "one complete copy plus the cache lock"
        );
    }
}

#[test]
fn cleanup_retains_recent_batches_until_expiry() {
    let root = tempfile::tempdir().unwrap();
    let now = received::now();
    let batch = root.path().join(format!("{:032x}.ready", 1));
    private_dir(&batch.join("files")).unwrap();
    drop(lock_root(&batch).unwrap());
    let file = batch.join("files/payload");
    fs::write(&file, b"reader").unwrap();
    fs::write(
        batch.join(RECORD),
        serde_json::to_vec(&Record {
            created_ms: now,
            clipboard: None,
            bytes: 6,
        })
        .unwrap(),
    )
    .unwrap();
    let recent = maintain(root.path(), false, now).unwrap();
    assert_eq!(recent.next_expiry, Some(now + RETENTION_MS + 1));
    maintain(root.path(), false, now + RETENTION_MS).unwrap();
    assert!(file.exists(), "exactly 24 hours is still retained");
    let expired = maintain(root.path(), false, now + RETENTION_MS + 1).unwrap();
    assert_reclaimed(&file, &expired);
    assert_eq!(expired.next_expiry, None);
}

#[test]
fn accounting_without_root_lock_never_deletes() {
    let root = tempfile::tempdir().unwrap();
    let batch = root.path().join(format!("{:032x}.ready", 2));
    private_dir(&batch.join("files")).unwrap();
    drop(lock_root(&batch).unwrap());
    fs::write(batch.join("files/payload"), b"reader").unwrap();
    fs::write(
        batch.join(RECORD),
        serde_json::to_vec(&Record {
            created_ms: 0,
            clipboard: None,
            bytes: 6,
        })
        .unwrap(),
    )
    .unwrap();
    let _root_lock = lock_root(root.path()).unwrap().unwrap();
    let result = maintain(root.path(), true, u64::MAX).unwrap();
    assert!(result.incomplete);
    assert!(batch.exists());
}

#[test]
#[ignore = "device: requires injectable cleanup worker clock and completion"]
fn cleanup_worker_wakes_at_expiry_and_on_cache_event() {
    const CHILD: &str = "OINK_CLEANUP_WAKE_TEST";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "macos::small_files::tests::cleanup_worker_wakes_at_expiry_and_on_cache_event",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let cache = tempfile::tempdir().unwrap();
    let root = cache.path().join("ClipboardFiles");
    let create = |id: u32, created_ms| {
        let batch = root.join(format!("{id:032x}.ready"));
        private_dir(&batch.join("files")).unwrap();
        drop(lock_root(&batch).unwrap());
        fs::write(batch.join("files/payload"), b"expiry").unwrap();
        fs::write(
            batch.join(RECORD),
            serde_json::to_vec(&Record {
                created_ms,
                clipboard: None,
                bytes: 6,
            })
            .unwrap(),
        )
        .unwrap();
        batch
    };
    let wait_deleted = |batch: &Path| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while batch.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!batch.exists(), "cleanup worker did not reclaim batch");
    };
    let first = create(1, received::now() - RETENTION_MS + 200);
    initialize(cache.path());
    wait_deleted(&first);
    // The empty cache has no expiry deadline. A cache event must wake it.
    let second = create(2, 0);
    crate::cache::changed(&root);
    wait_deleted(&second);
}

fn assert_reclaimed(file: &Path, scan: &cleanup::Scan) {
    assert!(!scan.incomplete, "ordinary-user cleanup must complete");
    assert!(!file.exists(), "unreferenced file must be reclaimed");
    assert!(scan.removed > 0);
}

#[test]
fn expiry_unlinks_cache_while_posix_reader_and_mapping_remain_usable() {
    let root = tempfile::tempdir().unwrap();
    let policy = policy(root.path(), files(vec![file(1, "a", 3)], b"abc"));
    let file = path(
        &policy
            .resolve(NodeId(1), Instant::now() + Duration::from_secs(2))
            .unwrap(),
    );
    drop(policy);
    let mut reader = File::open(&file).unwrap();
    // SAFETY: maps the three bytes of the live private cache file, read-only.
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            3,
            libc::PROT_READ,
            libc::MAP_SHARED,
            reader.as_raw_fd(),
            0,
        )
    };
    assert_ne!(mapping, libc::MAP_FAILED);
    let scan = maintain(root.path(), false, u64::MAX).unwrap();
    let mut bytes = Vec::new();
    let read = reader.read_to_end(&mut bytes);
    // SAFETY: mapping remains live after unlink and contains exactly three bytes.
    let mapped = unsafe { std::slice::from_raw_parts(mapping.cast::<u8>(), 3).to_vec() };
    // SAFETY: releases the sole mapping with the original length.
    unsafe {
        libc::munmap(mapping, 3);
    }
    assert_reclaimed(&file, &scan);
    read.unwrap();
    assert_eq!(bytes, b"abc");
    assert_eq!(mapped, b"abc");
    assert_eq!(File::open(&file).unwrap_err().kind(), io::ErrorKind::NotFound);
}

#[test]
fn source_lease_still_protects_expired_cache() {
    let root = tempfile::tempdir().unwrap();
    let policy = policy(root.path(), files(vec![file(1, "a", 3)], b"abc"));
    let file = path(
        &policy
            .resolve(NodeId(1), Instant::now() + Duration::from_secs(2))
            .unwrap(),
    );
    drop(policy);
    let source = crate::cache::SourceLease::reading(file.clone());
    let protected = maintain(root.path(), true, u64::MAX).unwrap();
    assert!(file.exists());
    assert!(!protected.incomplete);
    assert_eq!(protected.kept, protected.bytes);
    drop(source);
    let unprotected = maintain(root.path(), false, u64::MAX).unwrap();
    assert_reclaimed(&file, &unprotected);
}
