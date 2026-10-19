use super::*;
use crate::model::{ContentItem, ItemId, Medium, NativeFormat, NodeId, StandardFormat};
use crate::model::{ContentMsg, Demand};
use oink_foundation::DeviceId;
use oink_foundation::content::ClipId;

pub(crate) struct Fixture {
    inventory: Inventory,
    bytes: Mutex<HashMap<RepresentationId, Vec<u8>>>,
}
struct Text;
impl Translator for Text {
    fn standard(&self, _: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
        Ok(vec![ClipData::Text(
            String::from_utf8(bytes.to_vec()).map_err(|_| unavailable(Unavailable::InvalidData))?,
        )])
    }
}
impl Provider for Fixture {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<FileReader> {
        let bytes = self
            .bytes
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| unavailable(Unavailable::ReadFailed))?;
        Ok(Box::new(io::Cursor::new(bytes)))
    }
    fn translator(&self) -> Arc<dyn Translator> {
        Arc::new(Text)
    }
}
pub(crate) fn fixture() -> Fixture {
    Fixture {
        inventory: Inventory {
            roots: Vec::new(),
            id: OfferId {
                clip: ClipId {
                    origin: DeviceId([1; 32]),
                    seq: 1,
                },
                generation: 1,
            },
            platform: ClipPlatform::Mac,
            items: vec![ContentItem {
                virtual_files: false,
                id: ItemId(1),
                file: None,
                representations: vec![
                    Representation {
                        id: RepresentationId(2),
                        format: NativeFormat::Mac("example.private".into()),
                        medium: Medium::Bytes,
                        aspect: 1,
                        index: -1,
                        standard: None,
                        data: RepresentationData::Reader {
                            size: ContentSize::Unknown,
                            hash: None,
                        },
                    },
                    Representation {
                        id: RepresentationId(1),
                        format: NativeFormat::Mac("public.utf8-plain-text".into()),
                        medium: Medium::Bytes,
                        aspect: 1,
                        index: -1,
                        standard: Some(StandardFormat::Text),
                        data: RepresentationData::Reader {
                            size: ContentSize::Unknown,
                            hash: None,
                        },
                    },
                ],
            }],
        },
        bytes: Mutex::new(HashMap::from([
            (RepresentationId(2), vec![0, 255, 0]),
            (RepresentationId(1), b"original".to_vec()),
        ])),
    }
}
#[test]
fn promise_and_virtual_file_items_preserve_standard_content_companions_across_platforms() {
    for (source, target, format) in [
        (
            ClipPlatform::Mac,
            ClipPlatform::Windows,
            NativeFormat::Mac("public.png".into()),
        ),
        (
            ClipPlatform::Windows,
            ClipPlatform::Mac,
            NativeFormat::WindowsRegistered("PNG".into()),
        ),
    ] {
        let mut inventory = fixture().inventory();
        inventory.platform = source;
        inventory.items[0].file = Some(crate::model::NodeId(1));
        inventory.items[0].representations[0].format = format;
        inventory.items[0].representations[0].standard = Some(StandardFormat::Png);
        if source == ClipPlatform::Windows {
            inventory.items[0].representations[1].format = NativeFormat::WindowsPredefined(13);
        }
        assert_eq!(
            inventory.wanted(target).count(),
            0,
            "real-file icon/text remain filtered"
        );
        inventory.items[0].virtual_files = true;
        let wanted: Vec<_> = inventory.wanted(target).map(|rep| rep.standard).collect();
        assert_eq!(wanted, [Some(StandardFormat::Png), Some(StandardFormat::Text)]);
        assert_eq!(
            crate::wire::local_inventory(&crate::wire::inventory(&inventory).unwrap()).unwrap(),
            inventory
        );
    }
}
#[test]
fn native_drop_snapshot_preserves_order_bytes_and_survives_original_provider_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture();
    let snap = Snapshot::capture(dir.path(), BatchId([1; 16]), &source).unwrap();
    source.bytes.lock().unwrap().clear();
    let inv = snap.inventory();
    assert_eq!(
        inv.items[0].representations.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![RepresentationId(2), RepresentationId(1)]
    );
    let mut native = open_for(snap.as_ref(), RepresentationId(2), ClipPlatform::Mac).unwrap();
    assert_eq!(read_all(&mut native).unwrap(), vec![0, 255, 0]);
    assert!(open_for(snap.as_ref(), RepresentationId(2), ClipPlatform::Windows).is_err());
    let mut translated = open_for(snap.as_ref(), RepresentationId(1), ClipPlatform::Windows).unwrap();
    assert_eq!(
        decode_standard(
            find(&inv, RepresentationId(1)).unwrap(),
            &read_all(&mut translated).unwrap()
        )
        .unwrap(),
        vec![ClipData::Text("original".into())]
    );
    let mut late = snap.open(RepresentationId(1)).unwrap();
    let batch_path = snap.batch().path().to_owned();
    drop(native);
    drop(translated);
    drop(snap);
    crate::store::usage(dir.path(), StoreKind::SourceSnapshot, true);
    assert!(batch_path.exists(), "native reader still pins local bytes");
    assert_eq!(read_all(&mut late).unwrap(), b"original");
    drop(late);
    crate::store::usage(dir.path(), StoreKind::SourceSnapshot, true);
    assert!(!batch_path.exists());
}
#[test]
fn snapshot_exposes_unavailable_representations_without_dropping_the_other_items() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = fixture();
    source.inventory.items[0].representations[0].data = RepresentationData::Reader {
        size: ContentSize::Known(REPRESENTATION_BYTES + 1),
        hash: None,
    };
    let snapshot = Snapshot::capture(dir.path(), BatchId([2; 16]), &source).unwrap();
    assert_eq!(
        snapshot.inventory().items[0].representations[0].data,
        RepresentationData::Unavailable(Unavailable::LimitExceeded)
    );
    assert!(snapshot.open(RepresentationId(1)).is_ok());
    assert_eq!(
        snapshot.open(RepresentationId(2)).err().unwrap().kind(),
        io::ErrorKind::FileTooLarge
    );
}

#[test]
fn cancelled_native_source_rejects_late_drop_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture();
    let deferred = Deferred::new(source.inventory(), source.translator()).unwrap();
    let reader = deferred.clone();
    let (started, entered) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        started.send(()).unwrap();
        reader.open(RepresentationId(1)).err().unwrap().kind()
    });
    entered.recv().unwrap();
    assert!(deferred.fail(Unavailable::Cancelled));
    let snapshot = Snapshot::capture(dir.path(), BatchId([3; 16]), &source).unwrap();
    assert!(!deferred.complete(snapshot));
    assert_eq!(thread.join().unwrap(), io::ErrorKind::Interrupted);
}

use crate::sync::files::{BoxFuture, BulkChannel, BulkLink};
struct Pipe {
    out: Option<tokio::sync::mpsc::UnboundedSender<ContentMsg>>,
    input: tokio::sync::mpsc::UnboundedReceiver<ContentMsg>,
}
impl BulkChannel for Pipe {
    fn send<'a>(&'a mut self, message: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.out
                .as_ref()
                .unwrap()
                .send(message.clone())
                .map_err(|_| io::ErrorKind::BrokenPipe.into())
        })
    }
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
        Box::pin(async { Ok(self.input.recv().await) })
    }
    fn finish(&mut self) {
        self.out = None;
    }
}
struct TestLink {
    registry: Arc<Registry>,
    opened: std::sync::atomic::AtomicUsize,
    requested: tokio::sync::mpsc::UnboundedSender<Demand>,
}
impl BulkLink for TestLink {
    fn bind(self: Arc<Self>, _: DeviceId, _: ClipId) -> Arc<dyn BulkLink> {
        self
    }
    fn open(&self, _: DeviceId) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>> {
        Box::pin(async move {
            self.opened.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (a_tx, a_rx) = tokio::sync::mpsc::unbounded_channel();
            let (b_tx, b_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut source = Pipe {
                out: Some(b_tx),
                input: a_rx,
            };
            let registry = self.registry.clone();
            let requested = self.requested.clone();
            tokio::spawn(async move {
                let request = source.recv().await.unwrap().unwrap();
                if let ContentMsg::ReadRepresentation { demand, .. } = request {
                    requested.send(demand).unwrap();
                }
                match request {
                    ContentMsg::TargetStored { delivery, receipt } => {
                        registry.clipboard_receipt(DeviceId([2; 32]), delivery, Some(receipt));
                        source
                            .send(&ContentMsg::TerminalAck {
                                delivery,
                                receipt: Some(receipt),
                            })
                            .await
                            .unwrap()
                    }
                    ContentMsg::Failed { delivery, .. } => {
                        registry.clipboard_receipt(DeviceId([2; 32]), delivery, None);
                        source
                            .send(&ContentMsg::TerminalAck {
                                delivery,
                                receipt: None,
                            })
                            .await
                            .unwrap()
                    }
                    request => {
                        if let Err(error) = registry.serve(DeviceId([2; 32]), request, &mut source).await {
                            assert_eq!(
                                error.kind(),
                                io::ErrorKind::BrokenPipe,
                                "only the explicit interrupted-pipe scenario closes a live server"
                            );
                        }
                    }
                }
                source.finish();
                assert!(source.recv().await.unwrap().is_none());
            });
            Ok(Box::new(Pipe {
                out: Some(a_tx),
                input: b_rx,
            }) as Box<dyn BulkChannel>)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_drop_demand_unblocks_retained_source_and_native_reader_uses_local_verified_bytes() {
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let original = fixture();
    let inventory = original.inventory();
    let deferred = Deferred::new(inventory.clone(), original.translator()).unwrap();
    let registry = Arc::new(Registry::default());
    let lease = registry.register(deferred.clone()).unwrap();
    let (requested, mut demand) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(TestLink {
        registry,
        opened: std::sync::atomic::AtomicUsize::new(0),
        requested,
    });
    let remote = Remote::new(
        target_dir.path(),
        BatchId([4; 16]),
        inventory,
        ClipPlatform::Mac,
        link.clone(),
        Demand::Hover(oink_foundation::content::Holder {
            device: DeviceId([2; 32]),
            epoch: 1,
        }),
    )
    .unwrap();
    assert!(
        remote.open(RepresentationId(1)).is_err(),
        "hover cannot block a native modal loop"
    );
    assert_eq!(link.opened.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(matches!(demand.recv().await, Some(Demand::Hover(_))));
    let holder = oink_foundation::content::Holder {
        device: DeviceId([2; 32]),
        epoch: 7,
    };
    remote.set_demand(Demand::Drop(holder));
    let reader = remote.clone();
    let reading = tokio::task::spawn_blocking(move || {
        let mut reader = reader.open(RepresentationId(1)).unwrap();
        read_all(&mut reader).unwrap()
    });
    assert_eq!(demand.recv().await, Some(Demand::Drop(holder)));
    // The native source performs its one real Drop while GetData is outstanding.
    let snapshot = Snapshot::capture(source_dir.path(), BatchId([5; 16]), &original).unwrap();
    assert!(deferred.complete(snapshot));
    assert_eq!(reading.await.unwrap(), b"original");
    let mut progress = remote.progress();
    progress
        .wait_for(|p| p.phase == TransferPhase::Stored && p.source_acknowledged)
        .await
        .unwrap();
    assert!(remote.receipt().is_some());
    lease.cancel();
    drop(lease);
    drop(deferred);
    original.bytes.lock().unwrap().clear();
    let mut reader = remote.open(RepresentationId(1)).unwrap();
    assert_eq!(read_all(&mut reader).unwrap(), b"original");
    assert_eq!(
        link.opened.load(std::sync::atomic::Ordering::SeqCst),
        5,
        "whole drop also receives the unrequested representation; later reads remain local"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_and_aggregate_over_budget_clipboard_never_open_a_bulk_channel_automatically() {
    let dir = tempfile::tempdir().unwrap();
    let (requested, _rx) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(TestLink {
        registry: Arc::default(),
        opened: std::sync::atomic::AtomicUsize::new(0),
        requested,
    });
    let mut inventory = fixture().inventory();
    let remote = Remote::new(
        dir.path(),
        BatchId([6; 16]),
        inventory.clone(),
        ClipPlatform::Mac,
        link.clone(),
        Demand::Clipboard,
    )
    .unwrap();
    assert!(!remote.automatic_pull());
    inventory.items[0].representations[0].data = RepresentationData::Inline(vec![0; 65536]);
    inventory.items[0].representations[1].data = RepresentationData::Reader {
        size: ContentSize::Known(crate::model::AUTO_PULL_BYTES),
        hash: None,
    };
    let remote = Remote::new(
        dir.path(),
        BatchId([7; 16]),
        inventory,
        ClipPlatform::Mac,
        link.clone(),
        Demand::Clipboard,
    )
    .unwrap();
    assert!(!remote.automatic_pull());
    assert_eq!(link.opened.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn source_settings_projection_denies_unannounced_representation_ids() {
    let source = Arc::new(fixture());
    let mut inventory = source.inventory();
    inventory.items[0].representations.remove(0);
    let permitted = projected(source, inventory).unwrap();
    assert_eq!(
        permitted.open(RepresentationId(2)).err().unwrap().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(
        read_all(&mut permitted.open(RepresentationId(1)).unwrap()).unwrap(),
        b"original"
    );
}

#[test]
fn changed_native_inventory_cannot_supply_a_previous_hover_identity_at_drop() {
    let original = fixture();
    let deferred = Deferred::new(original.inventory(), original.translator()).unwrap();
    let mut replacement = fixture();
    replacement.inventory.items[0].representations.swap(0, 1);
    assert!(!deferred.complete(Arc::new(replacement)));
    assert_eq!(
        deferred.open(RepresentationId(1)).err().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_drop_receives_unread_tree_and_all_representations_into_durable_local_storage() {
    use crate::model::NodeId;
    use namespace::Namespace;
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let root = source_dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("one"), vec![37; 1024 * 1024 + 19]).unwrap();
    std::fs::write(root.join("nested/two"), b"unrequested second file").unwrap();
    let files = namespace::Local::new(vec![(NodeId(9), root)]).unwrap();
    let raw = Snapshot::capture(source_dir.path(), BatchId([31; 16]), &fixture()).unwrap();
    let source = with_files(raw, files);
    let registry = Arc::new(Registry::default());
    let lease = registry.register(source.clone()).unwrap();
    let (requested, _requests) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(TestLink {
        registry,
        opened: std::sync::atomic::AtomicUsize::new(0),
        requested,
    });
    let remote = Remote::new(
        target_dir.path(),
        BatchId([32; 16]),
        source.inventory(),
        ClipPlatform::Mac,
        link.clone(),
        Demand::Drop(oink_foundation::content::Holder {
            device: DeviceId([2; 32]),
            epoch: 1,
        }),
    )
    .unwrap();
    // No application reader exists. A native accepted callback alone is enough.
    assert!(remote.start_delivery());
    let mut progress = remote.progress();
    progress
        .wait_for(|p| matches!(p.phase, TransferPhase::Stored | TransferPhase::Failed))
        .await
        .unwrap();
    assert_eq!(progress.borrow().phase, TransferPhase::Stored);
    assert_eq!(progress.borrow().files_done, 2);
    assert_eq!(progress.borrow().total_files, Some(2));
    assert_eq!(progress.borrow().total_bytes, Some(1024 * 1024 + 19 + 23 + 11));
    assert!(remote.receipt().is_some());
    assert!(Batch::restore(target_dir.path(), BatchId([32; 16])).is_ok());
    progress.wait_for(|p| p.source_acknowledged).await.unwrap();
    let requests = link.opened.load(std::sync::atomic::Ordering::SeqCst);
    lease.cancel();
    drop((lease, source));
    drop(remote);
    let local = Completed::restore(target_dir.path(), BatchId([32; 16])).unwrap();
    tokio::task::spawn_blocking(move || {
        let page = Namespace::list(&local, NodeId(9), None).unwrap();
        let folder = page
            .entries
            .iter()
            .find(|node| node.kind == crate::model::NodeKind::Directory)
            .unwrap();
        let nested = Namespace::list(&local, folder.id, None).unwrap();
        let selected = Namespace::select(&local, nested.entries[0].id).unwrap();
        let mut reader = Namespace::open(&local, selected.id, selected.version, 0).unwrap();
        assert_eq!(read_all(&mut reader).unwrap(), b"unrequested second file");
        assert_eq!(
            read_all(&mut Provider::open(&local, RepresentationId(2)).unwrap()).unwrap(),
            [0, 255, 0]
        );
    })
    .await
    .unwrap();
    assert_eq!(
        link.opened.load(std::sync::atomic::Ordering::SeqCst),
        requests,
        "stored readers never reopen the source"
    );
}

#[test]
fn native_file_descriptor_table_expands_once_and_node_authority_excludes_hidden_roots() {
    use crate::model::NodeId;
    let source_dir = tempfile::tempdir().unwrap();
    let first = source_dir.path().join("allowed");
    let hidden = source_dir.path().join("hidden");
    std::fs::create_dir(&first).unwrap();
    std::fs::write(first.join("child"), b"kept").unwrap();
    std::fs::write(&hidden, b"denied").unwrap();
    let files = namespace::Local::new(vec![(NodeId(1), first), (NodeId(2), hidden)]).unwrap();
    let source = with_files(Arc::new(fixture()), files);
    let mut inventory = source.inventory();
    inventory.roots.truncate(1);
    let allowed = projected(source, inventory).unwrap().namespace().unwrap();
    assert!(allowed.select(NodeId(2)).is_err());
    let descriptor = namespace::Files::new(allowed, crate::sync::files::TargetOs::Windows).unwrap();
    assert_eq!(descriptor.roots().len(), 1);
    use crate::FileContents;
    let entries = descriptor.descriptor_entries().unwrap().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].path, "allowed/child");
    assert_eq!(descriptor.descriptor_entries().unwrap().unwrap(), entries);
    assert_eq!(read_all(&mut descriptor.open(1).unwrap()).unwrap(), b"kept");
}

struct InterruptedFileLink {
    inner: Arc<TestLink>,
    interrupted: Arc<std::sync::atomic::AtomicBool>,
    requests: tokio::sync::mpsc::UnboundedSender<(u64, Option<[u8; 32]>)>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_prefetch_checks_current_known_total_before_any_file_reader() {
    use namespace::Namespace;
    use std::sync::atomic::Ordering;
    struct Selection {
        inner: Arc<dyn Namespace>,
        unknown: bool,
        opens: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Namespace for Selection {
        fn roots(&self) -> Vec<crate::model::FileNode> {
            self.inner.roots()
        }
        fn list(&self, n: NodeId, c: Option<crate::model::DirectoryCursor>) -> io::Result<crate::model::DirectoryPage> {
            self.inner.list(n, c)
        }
        fn select(&self, n: NodeId) -> io::Result<crate::model::FileNode> {
            let mut selected = self.inner.select(n)?;
            if self.unknown {
                selected.size = ContentSize::Unknown;
            }
            Ok(selected)
        }
        fn open(&self, n: NodeId, v: u64, o: u64) -> io::Result<FileReader> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            self.inner.open(n, v, o)
        }
    }
    for unknown in [false, true] {
        let source_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let path = source_dir.path().join("file");
        std::fs::write(&path, b"a").unwrap();
        let local = namespace::Local::new(vec![(NodeId(1), path.clone())]).unwrap();
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut raw = fixture();
        raw.inventory.items.clear();
        let source = with_files(
            Arc::new(raw),
            Arc::new(Selection {
                inner: local,
                unknown,
                opens: opens.clone(),
            }),
        );
        let inventory = source.inventory();
        // The offer's old one-byte length cannot authorize a now-grown file.
        std::fs::write(path, b"larger than the budget").unwrap();
        let registry = Arc::new(Registry::default());
        let _lease = registry.register(source).unwrap();
        let (requested, _requests) = tokio::sync::mpsc::unbounded_channel();
        let link = Arc::new(TestLink {
            registry,
            opened: Default::default(),
            requested,
        });
        let remote = Remote::new(
            target_dir.path(),
            BatchId([81; 16]),
            inventory,
            ClipPlatform::Mac,
            link,
            Demand::Clipboard,
        )
        .unwrap();
        remote.set_receiver(DeviceId([2; 32]));
        assert!(remote.prefetch_bounded(4).await.is_err());
        assert_eq!(opens.load(Ordering::SeqCst), 0);
        assert_eq!(remote.progress().borrow().phase, TransferPhase::Failed);
        assert!(remote.receipt().is_none());
    }
}
impl BulkLink for InterruptedFileLink {
    fn bind(self: Arc<Self>, _: DeviceId, _: ClipId) -> Arc<dyn BulkLink> {
        self
    }
    fn open(&self, peer: DeviceId) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>> {
        Box::pin(async move {
            Ok(Box::new(InterruptedFileChannel {
                channel: self.inner.open(peer).await?,
                interrupted: self.interrupted.clone(),
                fail: false,
                requests: self.requests.clone(),
            }) as Box<dyn BulkChannel>)
        })
    }
}
struct InterruptedFileChannel {
    channel: Box<dyn BulkChannel>,
    interrupted: Arc<std::sync::atomic::AtomicBool>,
    fail: bool,
    requests: tokio::sync::mpsc::UnboundedSender<(u64, Option<[u8; 32]>)>,
}
impl BulkChannel for InterruptedFileChannel {
    fn send<'a>(&'a mut self, message: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            if let ContentMsg::ReadFile { offset, check, .. } = message {
                self.requests.send((*offset, *check)).unwrap();
            }
            self.channel.send(message).await
        })
    }
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
        Box::pin(async move {
            if self.fail {
                self.channel.finish();
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            let message = self.channel.recv().await?;
            if matches!(message, Some(ContentMsg::FileBlock { .. }))
                && !self.interrupted.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                self.fail = true;
            }
            Ok(message)
        })
    }
    fn finish(&mut self) {
        self.channel.finish();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_delivery_resumes_verified_block_and_outlives_a_partial_native_reader() {
    use namespace::Namespace;
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let bytes = vec![73; 2 * 1024 * 1024 + 117];
    let path = source_dir.path().join("file");
    std::fs::write(&path, &bytes).unwrap();
    let mut raw = fixture();
    raw.inventory.items.clear();
    let source = with_files(Arc::new(raw), namespace::Local::new(vec![(NodeId(1), path)]).unwrap());
    let registry = Arc::new(Registry::default());
    let lease = registry.register(source.clone()).unwrap();
    let (requested, _raw) = tokio::sync::mpsc::unbounded_channel();
    let (requests, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(InterruptedFileLink {
        inner: Arc::new(TestLink {
            registry,
            opened: Default::default(),
            requested,
        }),
        interrupted: Arc::default(),
        requests,
    });
    let remote = Remote::new(
        target_dir.path(),
        BatchId([49; 16]),
        source.inventory(),
        ClipPlatform::Mac,
        link,
        Demand::Clipboard,
    )
    .unwrap();
    remote.set_receiver(DeviceId([2; 32]));
    let reading = remote.clone();
    let partial = tokio::task::spawn_blocking(move || {
        let file = Namespace::select(&reading, NodeId(1)).unwrap();
        let mut reader = Namespace::open(&reading, NodeId(1), file.version, 0).unwrap();
        let mut bytes = [0; 19];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, [73; 19]);
        // Dropping this native reader does not cancel its independently owned batch.
    });
    assert_eq!(observed.recv().await.unwrap(), (0, None));
    partial.await.unwrap();
    assert_eq!(
        observed.recv().await.unwrap(),
        (1024 * 1024, Some(*blake3::hash(&bytes[..1024 * 1024]).as_bytes()))
    );
    let mut progress = remote.progress();
    progress
        .wait_for(|p| matches!(p.phase, TransferPhase::Stored | TransferPhase::Failed))
        .await
        .unwrap();
    assert_eq!(progress.borrow().phase, TransferPhase::Stored);
    assert_eq!(
        progress.borrow().bytes,
        bytes.len() as u64,
        "retries cannot count a verified prefix twice"
    );
    lease.cancel();
    let read = tokio::task::spawn_blocking(move || {
        let selected = Namespace::select(&remote, NodeId(1)).unwrap();
        let mut reader = Namespace::open(&remote, NodeId(1), selected.version, 0).unwrap();
        read_all(&mut reader).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(read, bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn target_manifest_commit_failure_never_reports_stored() {
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let source = Snapshot::capture(source_dir.path(), BatchId([61; 16]), &fixture()).unwrap();
    let registry = Arc::new(Registry::default());
    let _lease = registry.register(source.clone()).unwrap();
    let (requested, _requests) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(TestLink {
        registry,
        opened: Default::default(),
        requested,
    });
    let remote = Remote::new(
        target_dir.path(),
        BatchId([62; 16]),
        source.inventory(),
        ClipPlatform::Mac,
        link,
        Demand::Drop(oink_foundation::content::Holder {
            device: DeviceId([2; 32]),
            epoch: 1,
        }),
    )
    .unwrap();
    // A real filesystem failure at the commit boundary, after payload verification.
    std::fs::create_dir(remote.batch().path().join("manifest.part")).unwrap();
    assert!(remote.start_delivery());
    let mut progress = remote.progress();
    progress.wait_for(|p| p.source_acknowledged).await.unwrap();
    assert_eq!(progress.borrow().phase, TransferPhase::Failed);
    assert!(remote.receipt().is_none());
    assert!(Batch::restore(target_dir.path(), BatchId([62; 16])).is_err());
    assert!(remote.open(RepresentationId(1)).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hover_reads_active_provider_without_starting_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let original = Arc::new(fixture());
    let registry = Arc::new(Registry::default());
    let deferred = Deferred::new(original.inventory(), original.translator()).unwrap();
    let live = original.clone();
    deferred.set_hover_reader(Arc::new(move |id| live.open(id)));
    let _lease = registry
        .register(identified(deferred.clone(), original.inventory().id))
        .unwrap();
    let (requested, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(TestLink {
        registry,
        opened: Default::default(),
        requested,
    });
    let hover = Demand::Hover(oink_foundation::content::Holder {
        device: DeviceId([2; 32]),
        epoch: 1,
    });
    let remote = Remote::new(
        dir.path(),
        BatchId([14; 16]),
        original.inventory(),
        ClipPlatform::Mac,
        link.clone(),
        hover,
    )
    .unwrap();
    let read = remote.clone();
    let bytes = tokio::task::spawn_blocking(move || read_all(&mut read.open(RepresentationId(1)).unwrap()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, b"original");
    assert_eq!(requests.recv().await, Some(hover));
    assert_eq!(link.opened.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(remote.receipt().is_none(), "hover is not a completed delivery");
    assert!(
        deferred.complete(original),
        "hover did not commit or finish the native source"
    );
    remote.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_preview_reads_cross_platform_hover_without_committing_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let original = Arc::new(fixture());
    let registry = Arc::new(Registry::default());
    let deferred = Deferred::new(original.inventory(), original.translator()).unwrap();
    let live = original.clone();
    deferred.set_hover_reader(Arc::new(move |id| live.open(id)));
    let _lease = registry.register(deferred.clone()).unwrap();
    let (requested, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(TestLink {
        registry,
        opened: Default::default(),
        requested,
    });
    let holder = oink_foundation::content::Holder {
        device: DeviceId([2; 32]),
        epoch: 1,
    };
    let remote = Remote::new(
        dir.path(),
        BatchId([24; 16]),
        original.inventory(),
        ClipPlatform::Windows,
        link,
        Demand::Hover(holder),
    )
    .unwrap();
    let native = crate::NativeSpec {
        inventory: original.inventory(),
        contents: Arc::new(remote.clone()),
        files: vec![],
        file_contents: None,
        sensitivity: crate::Sensitivity::Normal,
    };
    let preview =
        tokio::task::spawn_blocking(move || crate::dnd::preview::content(&native, ClipPlatform::Windows, &Text))
            .await
            .unwrap();
    assert_eq!(preview, [ClipData::Text("original".into())]);
    assert_eq!(requests.recv().await, Some(Demand::Hover(holder)));
    assert!(requests.try_recv().is_err());
    assert!(remote.receipt().is_none());
    assert!(
        deferred.complete(original),
        "preview must leave the native source uncommitted"
    );
    remote.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preview_timeout_leaves_hover_cache_usable_and_does_not_start_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let original = Arc::new(fixture());
    let deferred = Deferred::new(original.inventory(), original.translator()).unwrap();
    let (entered, received) = std::sync::mpsc::sync_channel(1);
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let released = Mutex::new(released);
    deferred.set_hover_reader(Arc::new(move |_| {
        entered.send(()).unwrap();
        released.lock().unwrap().recv().unwrap();
        Ok(Box::new(io::Cursor::new(b"late hover".to_vec())) as FileReader)
    }));
    let registry = Arc::new(Registry::default());
    let _lease = registry.register(deferred.clone()).unwrap();
    let (requested, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(TestLink {
        registry,
        opened: Default::default(),
        requested,
    });
    let holder = oink_foundation::content::Holder {
        device: DeviceId([2; 32]),
        epoch: 1,
    };
    let remote = Remote::new(
        dir.path(),
        BatchId([25; 16]),
        original.inventory(),
        ClipPlatform::Mac,
        link,
        Demand::Hover(holder),
    )
    .unwrap();
    let reader = remote.clone();
    let pending = tokio::task::spawn_blocking(move || {
        reader.open_preview(RepresentationId(1), std::time::Duration::from_millis(20))
    });
    assert_eq!(requests.recv().await, Some(Demand::Hover(holder)));
    tokio::task::spawn_blocking(move || received.recv().unwrap())
        .await
        .unwrap();
    let error = pending
        .await
        .unwrap()
        .err()
        .expect("blocked hover must not delay Begin until the transfer watchdog");
    release.send(()).unwrap();
    assert_eq!(reason(&error), Unavailable::NoProgress);
    assert!(remote.receipt().is_none());
    let reader = remote.clone();
    let bytes = tokio::task::spawn_blocking(move || read_all(&mut reader.open(RepresentationId(1)).unwrap()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, b"late hover");
    assert!(
        requests.try_recv().is_err(),
        "cached preview hover is not reissued as Drop"
    );
    assert!(deferred.complete(original));
    remote.set_demand(Demand::Drop(holder));
    assert_eq!(
        reason(
            &remote
                .open_preview(RepresentationId(1), std::time::Duration::from_secs(1))
                .err()
                .unwrap()
        ),
        Unavailable::Permission
    );
    assert!(remote.receipt().is_none());
    remote.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_intent_alone_does_not_upgrade_an_outstanding_hover_to_drop() {
    let dir = tempfile::tempdir().unwrap();
    let original = Arc::new(fixture());
    let deferred = Deferred::new(original.inventory(), original.translator()).unwrap();
    let (entered, received) = std::sync::mpsc::sync_channel(1);
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let released = Mutex::new(released);
    deferred.set_hover_reader(Arc::new(move |_| {
        entered.send(()).unwrap();
        released.lock().unwrap().recv().unwrap();
        Err(unavailable(Unavailable::Unsupported))
    }));
    let registry = Arc::new(Registry::default());
    let _lease = registry.register(deferred.clone()).unwrap();
    let (requested, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(TestLink {
        registry,
        opened: Default::default(),
        requested,
    });
    let holder = oink_foundation::content::Holder {
        device: DeviceId([2; 32]),
        epoch: 1,
    };
    let remote = Remote::new(
        dir.path(),
        BatchId([15; 16]),
        original.inventory(),
        ClipPlatform::Mac,
        link,
        Demand::Hover(holder),
    )
    .unwrap();
    let reader = remote.clone();
    let pending = tokio::task::spawn_blocking(move || reader.open(RepresentationId(1)));
    assert_eq!(requests.recv().await, Some(Demand::Hover(holder)));
    tokio::task::spawn_blocking(move || received.recv().unwrap())
        .await
        .unwrap();
    remote.set_demand(Demand::Drop(holder));
    assert!(deferred.complete(original));
    release.send(()).unwrap();
    assert!(pending.await.unwrap().is_err(), "the old hover is not a new drop read");
    assert!(
        requests.try_recv().is_err(),
        "release intent alone authorizes no source drop"
    );
    assert!(remote.receipt().is_none());
    let reader = remote.clone();
    let pending = tokio::task::spawn_blocking(move || read_all(&mut reader.open(RepresentationId(1)).unwrap()));
    assert_eq!(requests.recv().await, Some(Demand::Drop(holder)));
    assert_eq!(pending.await.unwrap().unwrap(), b"original");
    remote.cancel();
}

#[tokio::test]
async fn unknown_clipboard_representation_over_limit_fails_without_a_source_store() {
    struct Oversized(Fixture);
    impl Provider for Oversized {
        fn inventory(&self) -> Inventory {
            self.0.inventory()
        }
        fn translator(&self) -> Arc<dyn Translator> {
            self.0.translator()
        }
        fn open(&self, _: RepresentationId) -> io::Result<FileReader> {
            Ok(Box::new(io::repeat(0).take(REPRESENTATION_BYTES + 1)))
        }
    }
    #[derive(Default)]
    struct Sink {
        bytes: u64,
        failure: Option<Unavailable>,
        ended: bool,
    }
    impl BulkChannel for Sink {
        fn send<'a>(&'a mut self, message: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
            Box::pin(async move {
                match message {
                    ContentMsg::RepresentationBytes { bytes, .. } => self.bytes += bytes.len() as u64,
                    ContentMsg::RepresentationFailed { reason, .. } => self.failure = Some(*reason),
                    ContentMsg::RepresentationEnd { .. } => self.ended = true,
                    _ => panic!("unexpected response"),
                }
                Ok(())
            })
        }
        fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
            Box::pin(async { Ok(None) })
        }
        fn finish(&mut self) {}
    }
    let provider = Arc::new(Oversized(fixture()));
    let inventory = provider.inventory();
    assert!(!inventory.automatic_pull(ClipPlatform::Mac));
    assert!(provider.local_batch().is_none());
    let registry = Arc::new(Registry::default());
    let _lease = registry.register(provider).unwrap();
    let mut sink = Sink::default();
    registry
        .serve(
            DeviceId([2; 32]),
            ContentMsg::ReadRepresentation {
                offer: inventory.id,
                representation: RepresentationId(2),
                item: ItemId(1),
                accepted_format: "mac-uti:example.private".into(),
                target: ClipPlatform::Mac,
                demand: Demand::Clipboard,
            },
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(sink.bytes, REPRESENTATION_BYTES);
    assert_eq!(sink.failure, Some(Unavailable::LimitExceeded));
    assert!(!sink.ended, "a truncated representation is never marked complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promise_names_are_drop_only_and_the_resolved_files_survive_source_release() {
    use namespace::Namespace;
    use std::{sync::atomic::Ordering, time::Duration};
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let mut original = fixture();
    let anchor = NodeId(1u64 << 32);
    original.inventory.items[0].representations.push(Representation {
        id: RepresentationId(3),
        format: NativeFormat::Mac("com.apple.pasteboard.promised-file-url".into()),
        medium: Medium::Bytes,
        aspect: 1,
        index: -1,
        standard: None,
        data: RepresentationData::Promise(anchor, vec![]),
    });
    let snapshot = Snapshot::capture(source_dir.path(), BatchId([71; 16]), &original).unwrap();
    let batch = Batch::create(source_dir.path(), StoreKind::PromiseExport, BatchId([72; 16])).unwrap();
    let ready = batch.path().join("ready");
    std::fs::create_dir(&ready).unwrap();
    let pending = namespace::Pending::new(
        namespace::Local::new(vec![(anchor, ready.clone())]).unwrap(),
        [anchor],
        batch,
    );
    let provider = with_files(snapshot, pending.clone());
    let registry = Arc::new(Registry::default());
    let lease = registry.register(provider.clone()).unwrap();
    let (requested, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(TestLink {
        registry,
        opened: Default::default(),
        requested,
    });
    let holder = oink_foundation::content::Holder {
        device: DeviceId([2; 32]),
        epoch: 1,
    };
    let remote = Remote::new(
        target_dir.path(),
        BatchId([73; 16]),
        provider.inventory(),
        ClipPlatform::Mac,
        link.clone(),
        Demand::Hover(holder),
    )
    .unwrap();
    assert!(remote.inventory().roots.is_empty());
    let hover = remote.clone();
    assert!(
        tokio::task::spawn_blocking(move || Namespace::list(&hover, anchor, None))
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(link.opened.load(Ordering::SeqCst), 0);
    remote.set_demand(Demand::Drop(holder));
    let reading = remote.clone();
    let reader = tokio::task::spawn_blocking(move || Namespace::list(&reading, anchor, None));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(30), requests.recv())
            .await
            .unwrap(),
        Some(Demand::Drop(holder))
    );
    // Model only the OS read-ready completion boundary, not a native drag.
    std::fs::write(ready.join("actual.jpg"), b"original promise bytes").unwrap();
    assert!(pending.finish(Ok(())));
    let page = tokio::time::timeout(Duration::from_secs(30), reader)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].name, "actual.jpg");
    let mut progress = remote.progress();
    tokio::time::timeout(
        Duration::from_secs(30),
        progress.wait_for(|p| matches!(p.phase, TransferPhase::Stored | TransferPhase::Failed)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(progress.borrow().phase, TransferPhase::Stored);
    lease.cancel();
    let completed = Completed::restore(target_dir.path(), BatchId([73; 16])).unwrap();
    assert!(Provider::inventory(&completed).roots.is_empty());
    let files = Provider::namespace(&completed).unwrap();
    let entry = files.list(anchor, None).unwrap().entries.remove(0);
    let selected = files.select(entry.id).unwrap();
    assert_eq!(
        read_all(&mut files.open(entry.id, selected.version, 0).unwrap()).unwrap(),
        b"original promise bytes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_published_inventory_rejects_size_and_version_changes_even_without_a_reader() {
    for (before, after) in [(0, 31), (50, 0), (9, 57), (57, 9), (9, 9)] {
        let source_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let path = source_dir.path().join("changed");
        std::fs::write(&path, vec![1; before]).unwrap();
        let mut raw = fixture();
        raw.inventory.items.clear();
        let source = with_files(
            Arc::new(raw),
            namespace::Local::new(vec![(NodeId(1), path.clone())]).unwrap(),
        );
        let registry = Arc::new(Registry::default());
        let _lease = registry.register(source.clone()).unwrap();
        let (requested, _requests) = tokio::sync::mpsc::unbounded_channel();
        let link = Arc::new(TestLink {
            registry,
            opened: Default::default(),
            requested,
        });
        let remote = Remote::new(
            target_dir.path(),
            BatchId([111; 16]),
            source.inventory(),
            ClipPlatform::Windows,
            link,
            Demand::Drop(oink_foundation::content::Holder {
                device: DeviceId([2; 32]),
                epoch: 1,
            }),
        )
        .unwrap();
        std::fs::write(&path, vec![2; after]).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_001_234))
            .unwrap();
        assert!(remote.start_delivery());
        let mut progress = remote.progress();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            progress.wait_for(|p| matches!(p.phase, TransferPhase::Stored | TransferPhase::Failed)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(progress.borrow().phase, TransferPhase::Failed, "{before} -> {after}");
        assert_eq!(progress.borrow().failure, Some(Unavailable::Superseded));
        assert!(remote.receipt().is_none());
        assert_eq!(progress.borrow().bytes, 0);
    }
}

#[test]
fn windows_descriptor_table_rejects_changes_before_first_stream_metadata_or_open() {
    use crate::FileContents;
    for (before, after) in [(0, 31), (50, 0), (9, 57), (57, 9), (9, 9)] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("changed");
        std::fs::write(&path, vec![1; before]).unwrap();
        let local = namespace::Local::new(vec![(NodeId(1), path.clone())]).unwrap();
        let files = namespace::Files::new(local, crate::sync::files::TargetOs::Windows).unwrap();
        assert_eq!(files.descriptor_entries().unwrap().unwrap()[0].size, before as u64);
        std::fs::write(&path, vec![2; after]).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_001_234))
            .unwrap();
        assert_eq!(reason(&files.metadata(0).unwrap_err()), Unavailable::Superseded);
        assert_eq!(reason(&files.open(0).err().unwrap()), Unavailable::Superseded);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_provider_refetches_evicted_content_from_live_offer() {
    use namespace::Namespace;
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let file = source_dir.path().join("file");
    std::fs::write(&file, b"refetched bytes").unwrap();
    let source = with_files(
        Arc::new(fixture()),
        namespace::Local::new(vec![(NodeId(9), file)]).unwrap(),
    );
    let registry = Arc::new(Registry::default());
    let lease = registry.register(source.clone()).unwrap();
    let (requested, _requests) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(TestLink {
        registry,
        opened: Default::default(),
        requested,
    });
    let remote = Remote::new(
        target_dir.path(),
        BatchId([85; 16]),
        source.inventory(),
        ClipPlatform::Mac,
        link.clone(),
        Demand::Clipboard,
    )
    .unwrap();
    remote.set_receiver(DeviceId([2; 32]));
    let (send, receive) = tokio::sync::oneshot::channel();
    remote.on_stored(Box::new(move |local| {
        send.send(local).unwrap();
    }));
    assert!(remote.start_delivery());
    let local = receive.await.unwrap();
    let path = remote.batch().path().to_owned();
    crate::store::sweep_at(target_dir.path(), u64::MAX);
    assert!(!path.exists());
    let opened = link.opened.load(std::sync::atomic::Ordering::SeqCst);
    let data = target_dir.path().to_owned();
    tokio::task::spawn_blocking(move || {
        let selected = Namespace::select(&local, NodeId(9)).unwrap();
        let mut read = Namespace::open(&local, selected.id, selected.version, 0).unwrap();
        assert_eq!(read_all(&mut read).unwrap(), b"refetched bytes");
        crate::store::sweep_at(&data, u64::MAX);
        assert!(
            Namespace::local_batch(&local).is_some(),
            "active re-fetch read protects its new batch"
        );
        drop(read);
        crate::store::sweep_at(&data, u64::MAX);
        // Repeated eviction must permit another source fetch as well.
        let selected = Namespace::select(&local, NodeId(9)).unwrap();
        assert_eq!(
            read_all(&mut Namespace::open(&local, selected.id, selected.version, 0).unwrap()).unwrap(),
            b"refetched bytes"
        );
        lease.cancel();
        crate::store::sweep_at(&data, u64::MAX);
        let error = Namespace::select(&local, NodeId(9)).unwrap_err();
        assert_eq!(reason(&error), Unavailable::Superseded);
    })
    .await
    .unwrap();
    assert!(link.opened.load(std::sync::atomic::Ordering::SeqCst) > opened);
}

#[tokio::test]
async fn cancellation_during_source_read_does_not_send_the_finished_block() {
    struct RevokingSource {
        registry: std::sync::Weak<Registry>,
    }
    struct RevokingReader {
        registry: std::sync::Weak<Registry>,
        reader: io::Cursor<Vec<u8>>,
    }
    impl Read for RevokingReader {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.registry.upgrade().unwrap().cancel_all();
            self.reader.read(bytes)
        }
    }
    impl Provider for RevokingSource {
        fn inventory(&self) -> Inventory {
            fixture().inventory()
        }
        fn open(&self, _: RepresentationId) -> io::Result<FileReader> {
            Ok(Box::new(RevokingReader {
                registry: self.registry.clone(),
                reader: io::Cursor::new(b"private".to_vec()),
            }))
        }
        fn translator(&self) -> Arc<dyn Translator> {
            Arc::new(Text)
        }
    }
    let registry = Arc::new(Registry::default());
    let source = Arc::new(RevokingSource {
        registry: Arc::downgrade(&registry),
    });
    let offer = source.inventory().id;
    let _source = registry.register(source).unwrap();
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (_unused, input) = tokio::sync::mpsc::unbounded_channel();
    let mut pipe = Pipe { out: Some(sent), input };
    registry
        .serve(
            DeviceId([2; 32]),
            ContentMsg::ReadRepresentation {
                offer,
                representation: RepresentationId(2),
                item: ItemId(1),
                accepted_format: "mac-uti:example.private".into(),
                target: ClipPlatform::Mac,
                demand: Demand::Clipboard,
            },
            &mut pipe,
        )
        .await
        .unwrap();
    let mut failures = 0;
    while let Ok(message) = received.try_recv() {
        match message {
            ContentMsg::RepresentationFailed {
                reason: Unavailable::Cancelled,
                ..
            } => failures += 1,
            _ => panic!("source sent content after permission was revoked"),
        }
    }
    assert_eq!(failures, 1);
}

#[tokio::test]
async fn mismatched_item_or_accepted_format_never_opens_the_source() {
    struct NeverRead;
    impl Provider for NeverRead {
        fn inventory(&self) -> Inventory {
            fixture().inventory()
        }
        fn open(&self, _: RepresentationId) -> io::Result<FileReader> {
            panic!("unselected native source was opened")
        }
        fn translator(&self) -> Arc<dyn Translator> {
            Arc::new(Text)
        }
    }
    let registry = Arc::new(Registry::default());
    let source = registry.register(Arc::new(NeverRead)).unwrap();
    for (item, format) in [
        (ItemId(99), "mac-uti:example.private"),
        (ItemId(1), "mime:text/plain;charset=utf-8"),
    ] {
        let (out, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (_unused, input) = tokio::sync::mpsc::unbounded_channel();
        registry
            .serve(
                DeviceId([2; 32]),
                ContentMsg::ReadRepresentation {
                    offer: source.inventory().id,
                    representation: RepresentationId(2),
                    item,
                    accepted_format: format.into(),
                    target: ClipPlatform::Mac,
                    demand: Demand::Clipboard,
                },
                &mut Pipe { out: Some(out), input },
            )
            .await
            .unwrap();
        assert!(matches!(
            received.recv().await,
            Some(ContentMsg::RepresentationFailed {
                reason: Unavailable::Unsupported,
                ..
            })
        ));
        assert!(received.try_recv().is_err());
    }
}

#[tokio::test]
async fn network_reconstruction_profile_is_checked_before_a_source_read() {
    struct NoIo;
    impl BulkChannel for NoIo {
        fn send<'a>(&'a mut self, _: &'a ContentMsg) -> crate::sync::files::BoxFuture<'a, io::Result<()>> {
            Box::pin(async { panic!("unsupported contract reached IO") })
        }
        fn recv(&mut self) -> crate::sync::files::BoxFuture<'_, io::Result<Option<ContentMsg>>> {
            Box::pin(async { panic!("unsupported contract reached IO") })
        }
        fn finish(&mut self) {}
    }
    let registry = Arc::new(Registry::default());
    let lease = registry.register(Arc::new(fixture())).unwrap();
    let inventory = lease.inventory();
    let mut channel = NoIo;
    use oink_protocol::v1::{RawValue as V, dto, schema::WireDto};
    let empty = dto::FormatSupport::from_raw(
        [
            (1, V::Array(vec![])),
            (2, V::Array(vec![])),
            (3, V::Array(vec![])),
            (4, V::Array(vec![])),
        ]
        .into(),
    )
    .unwrap();
    let error = registry
        .serve_negotiated(
            DeviceId([2; 32]),
            ContentMsg::ReadRepresentation {
                offer: inventory.id,
                representation: RepresentationId(2),
                item: ItemId(1),
                accepted_format: "mac-uti:example.private".into(),
                target: ClipPlatform::Mac,
                demand: Demand::Clipboard,
            },
            &mut channel,
            &empty,
            &empty,
        )
        .await
        .unwrap_err();
    assert_eq!(reason(&error), Unavailable::Unsupported);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_clipboard_publishes_latest_version_before_serving_to_mac_and_windows() {
    use namespace::Namespace;
    for target in [ClipPlatform::Mac, ClipPlatform::Windows] {
        let source_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let path = source_dir.path().join("file");
        std::fs::write(&path, b"old").unwrap();
        let files = namespace::Local::clipboard(vec![(NodeId(1), path.clone())]).unwrap();
        let source = with_files(Arc::new(fixture()), files);
        let offered = source.inventory().roots[0].clone();
        let registry = Arc::new(Registry::default());
        drop(registry.register_clipboard(source.clone()).unwrap());
        let (requested, _requests) = tokio::sync::mpsc::unbounded_channel();
        let link = Arc::new(TestLink {
            registry,
            opened: Default::default(),
            requested,
        });
        let remote = Remote::new(
            target_dir.path(),
            BatchId([122; 16]),
            source.inventory(),
            target,
            link,
            Demand::Clipboard,
        )
        .unwrap();
        remote.set_receiver(DeviceId([2; 32]));
        drop(source);
        std::fs::write(&path, b"latest file content").unwrap();
        let reading = remote.clone();
        let current = tokio::task::spawn_blocking(move || {
            let selected = Namespace::select(&reading, NodeId(1)).unwrap();
            let mut reader = Namespace::open(&reading, selected.id, selected.version, 0).unwrap();
            assert_eq!(read_all(&mut reader).unwrap(), b"latest file content");
            selected
        })
        .await
        .unwrap();
        assert_eq!(current.size, ContentSize::Known(19));
        assert_ne!(offered.version, current.version);
        let same = remote.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || Namespace::select(&same, NodeId(1)))
                .await
                .unwrap()
                .unwrap(),
            current
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_file_delivery_does_not_depend_on_superseded_native_byte_formats() {
    use namespace::Namespace;
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let path = source_dir.path().join("file");
    std::fs::write(&path, b"retained file").unwrap();
    let files = namespace::Local::clipboard(vec![(NodeId(1), path)]).unwrap();
    let mut inventory = fixture().inventory();
    inventory.roots = files.roots();
    let provider = clipboard::Clipboard::new(
        inventory,
        Box::new(|_| Err(clipboard::changed())),
        Arc::new(|| Err(clipboard::changed())),
        Arc::new(Text),
        Some(files),
    )
    .unwrap();
    let registry = Arc::new(Registry::default());
    let inventory = provider.inventory();
    drop(registry.register_clipboard(provider).unwrap());
    let (requested, _requests) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(TestLink {
        registry,
        opened: Default::default(),
        requested,
    });
    let remote = Remote::new(
        target_dir.path(),
        BatchId([123; 16]),
        inventory,
        ClipPlatform::Mac,
        link,
        Demand::Clipboard,
    )
    .unwrap();
    remote.set_receiver(DeviceId([2; 32]));
    let mut progress = remote.progress();
    let reading = remote.clone();
    tokio::task::spawn_blocking(move || {
        let current = Namespace::select(&reading, NodeId(1)).unwrap();
        assert_eq!(
            read_all(&mut Namespace::open(&reading, current.id, current.version, 0).unwrap()).unwrap(),
            b"retained file"
        );
    })
    .await
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        progress.wait_for(|p| matches!(p.phase, TransferPhase::Stored | TransferPhase::Failed)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(progress.borrow().phase, TransferPhase::Stored);
    let completed = Completed::restore(target_dir.path(), remote.batch().id()).unwrap();
    assert!(
        completed.inventory().items[0]
            .representations
            .iter()
            .all(|r| matches!(r.data, RepresentationData::Unavailable(Unavailable::Superseded)))
    );
}

#[test]
fn terminal_file_failures_are_not_retried_as_operating_system_interrupts() {
    struct Once(Option<Unavailable>);
    impl Read for Once {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if let Some(failure) = self.0.take() {
                return Err(unavailable(failure));
            }
            bytes.fill(7);
            Ok(bytes.len())
        }
    }
    for failure in [Unavailable::Superseded, Unavailable::Cancelled] {
        let mut reader = terminal_reader(Box::new(Once(Some(failure))));
        assert_eq!(reason(&reader.read_exact(&mut [0; 3]).unwrap_err()), failure);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutation_between_real_file_blocks_fails_without_retrying_or_committing_a_mixed_version() {
    use namespace::Namespace;
    struct Mutating {
        inner: Arc<namespace::Local>,
        path: std::path::PathBuf,
    }
    impl Namespace for Mutating {
        fn real_clipboard_files(&self) -> bool {
            true
        }
        fn retain_files(&self, retention: FileRetention) {
            self.inner.retain_files(retention);
        }
        fn roots(&self) -> Vec<crate::model::FileNode> {
            self.inner.roots()
        }
        fn list(
            &self,
            id: NodeId,
            cursor: Option<crate::model::DirectoryCursor>,
        ) -> io::Result<crate::model::DirectoryPage> {
            self.inner.list(id, cursor)
        }
        fn select(&self, id: NodeId) -> io::Result<crate::model::FileNode> {
            self.inner.select(id)
        }
        fn open(&self, id: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
            struct Change {
                inner: FileReader,
                path: Option<std::path::PathBuf>,
            }
            impl Read for Change {
                fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                    let size = self.inner.read(bytes)?;
                    if let Some(path) = self.path.take() {
                        std::fs::write(path, b"modified during the read")?;
                    }
                    Ok(size)
                }
            }
            Ok(Box::new(Change {
                inner: self.inner.open(id, version, offset)?,
                path: Some(self.path.clone()),
            }))
        }
    }
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let path = source_dir.path().join("file");
    std::fs::write(&path, vec![3; 2 * 1024 * 1024]).unwrap();
    let files = namespace::Local::clipboard(vec![(NodeId(1), path.clone())]).unwrap();
    let provider = with_files(Arc::new(fixture()), Arc::new(Mutating { inner: files, path }));
    let inventory = provider.inventory();
    let registry = Arc::new(Registry::default());
    drop(registry.register_clipboard(provider).unwrap());
    let (requested, _requests) = tokio::sync::mpsc::unbounded_channel();
    let link = Arc::new(TestLink {
        registry,
        opened: Default::default(),
        requested,
    });
    let remote = Remote::new(
        target_dir.path(),
        BatchId([124; 16]),
        inventory,
        ClipPlatform::Windows,
        link,
        Demand::Clipboard,
    )
    .unwrap();
    remote.set_receiver(DeviceId([2; 32]));
    let mut progress = remote.progress();
    remote.start_delivery();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        progress.wait_for(|p| matches!(p.phase, TransferPhase::Stored | TransferPhase::Failed)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(progress.borrow().phase, TransferPhase::Failed);
    assert_eq!(progress.borrow().failure, Some(Unavailable::Superseded));
    assert!(remote.receipt().is_none());
    assert!(!remote.batch().path().join("f-1").exists());
}

/// Already projected bytes, as supplied by a remote provider to native targets.
pub(crate) struct Portable(pub Inventory);
impl Provider for Portable {
    fn inventory(&self) -> Inventory {
        self.0.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<FileReader> {
        match &find(&self.0, id).unwrap().data {
            RepresentationData::Inline(bytes) => Ok(Box::new(io::Cursor::new(bytes.clone()))),
            _ => Err(unavailable(Unavailable::InvalidData)),
        }
    }
    fn translator(&self) -> Arc<dyn Translator> {
        unreachable!("target bytes are already projected")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversion_failure_is_committed_per_representation_for_clipboard_and_drop() {
    struct OneFailure(Unavailable);
    impl Translator for OneFailure {
        fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
            if bytes.first() == Some(&0) {
                Err(unavailable(self.0))
            } else {
                Text.standard(rep, bytes)
            }
        }
    }
    for failure in [
        Unavailable::InvalidData,
        Unavailable::Unsupported,
        Unavailable::ReadFailed,
        Unavailable::Permission,
        Unavailable::NoSuchItem,
        Unavailable::LimitExceeded,
        Unavailable::StorageFull,
    ] {
        for demand in [
            Demand::Clipboard,
            Demand::Drop(oink_foundation::content::Holder {
                device: DeviceId([2; 32]),
                epoch: 1,
            }),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut source = fixture();
            source.inventory.items[0].representations[0].standard = Some(StandardFormat::Text);
            // First representation cannot decode as UTF-8; second is valid text.
            let inventory = source.inventory();
            let registry = Arc::new(Registry::default());
            let provider = clipboard::Clipboard::new(
                inventory.clone(),
                Box::new(move |id| source.open(id)),
                Arc::new(|| Ok(())),
                Arc::new(OneFailure(failure)),
                None,
            )
            .unwrap();
            let lease = registry.register(provider).unwrap();
            let (requested, _requests) = tokio::sync::mpsc::unbounded_channel();
            let remote = Remote::new(
                dir.path(),
                BatchId([124; 16]),
                inventory,
                ClipPlatform::Windows,
                Arc::new(TestLink {
                    registry,
                    opened: Default::default(),
                    requested,
                }),
                demand,
            )
            .unwrap();
            remote.set_receiver(DeviceId([2; 32]));
            assert!(remote.start_delivery());
            let mut progress = remote.progress();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                progress.wait_for(|p| matches!(p.phase, TransferPhase::Stored | TransferPhase::Failed)),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(progress.borrow().phase, TransferPhase::Stored);
            let completed = Completed::restore(dir.path(), remote.batch().id()).unwrap();
            assert_eq!(
                find(&completed.inventory(), RepresentationId(2)).unwrap().data,
                RepresentationData::Unavailable(failure)
            );
            assert_eq!(
                read_all(&mut completed.open(RepresentationId(1)).unwrap()).unwrap(),
                b"original"
            );
            drop(lease);
        }
    }
}
