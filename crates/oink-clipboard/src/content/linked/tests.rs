use super::*;
use crate::sync::files::BoxFuture;
use oink_foundation::{DeviceId, content::ClipId};
use std::collections::VecDeque;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Link {
    replies: Mutex<VecDeque<Vec<ContentMsg>>>,
    requests: Arc<Mutex<Vec<ContentMsg>>>,
    opened: AtomicUsize,
    body_cut: Mutex<Option<Arc<AtomicBool>>>,
}
struct Channel {
    cut: Option<Arc<AtomicBool>>,
    replies: VecDeque<ContentMsg>,
    requests: Arc<Mutex<Vec<ContentMsg>>>,
}
impl BulkLink for Link {
    fn bind(self: Arc<Self>, _: DeviceId, _: ClipId) -> Arc<dyn BulkLink> {
        self
    }
    fn bind_persistent(self: Arc<Self>, _: DeviceId) -> io::Result<Arc<dyn BulkLink>> {
        Ok(self)
    }
    fn supports_cloud_links(&self) -> bool {
        true
    }
    fn open(&self, _: DeviceId) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>> {
        Box::pin(async {
            let opened = self.opened.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Channel {
                cut: if opened == 1 {
                    self.body_cut.lock().unwrap().take()
                } else {
                    None
                },
                replies: self
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or(io::ErrorKind::NotConnected)?
                    .into(),
                requests: self.requests.clone(),
            }) as Box<dyn BulkChannel>)
        })
    }
}
impl BulkChannel for Channel {
    fn effect_validity(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let cut = self.cut.clone();
        Arc::new(move || cut.as_ref().is_none_or(|live| live.load(Ordering::Acquire)))
    }
    fn send<'a>(&'a mut self, message: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(message.clone());
            Ok(())
        })
    }
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
        Box::pin(async {
            if let Some(cut) = &self.cut {
                if self.replies.is_empty() {
                    cut.store(false, Ordering::Release);
                }
                if !cut.load(Ordering::Acquire) {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
            }
            Ok(self.replies.pop_front())
        })
    }
    fn finish(&mut self) {}
}
fn descriptor(size: u64) -> Descriptor {
    Descriptor {
        native: Default::default(),
        provisional: false,
        fulfilled: vec![],
        offer: OfferId {
            clip: ClipId {
                origin: DeviceId([1; 32]),
                seq: 1,
            },
            generation: 0,
        },
        token: super::super::grants::LinkToken([7; 32]),
        roots: vec![FileNode {
            exec_bits: None,
            id: NodeId(1),
            parent: None,
            name: "file".into(),
            kind: NodeKind::File,
            size: ContentSize::Known(size),
            modified_ms: None,
            version: 1,
        }],
    }
}
fn link(replies: Vec<Vec<ContentMsg>>) -> Arc<Link> {
    Arc::new(Link {
        replies: Mutex::new(replies.into()),
        requests: Arc::default(),
        opened: AtomicUsize::new(0),
        body_cut: Mutex::default(),
    })
}

#[tokio::test]
async fn promise_offer_expands_only_the_requested_native_groups() {
    use crate::content::native_metadata::{MacPromiseGroup, NativeFileMetadata};
    let descriptor = descriptor(11);
    let mut second = descriptor.roots[0].clone();
    second.id = NodeId(2);
    second.name = "second".into();
    let received = vec![second, descriptor.roots[0].clone()];
    let native = NativeFileMetadata {
        windows: vec![],
        promises: vec![MacPromiseGroup {
            controls: Vec::new(),
            anchor: NodeId(90),
            roots: vec![NodeId(2), NodeId(1)],
            indices: vec![0, 1],
        }],
    };
    let response = ContentMsg::CloudLinkOffer {
        offer: descriptor.offer,
        token: descriptor.token,
        roots: received.clone(),
        fulfilled: received
            .iter()
            .map(|node| super::super::grants::FulfilledRoot {
                node: node.id,
                id: [4; 16],
            })
            .collect(),
        native,
    };
    let files = Linked::acquire(
        descriptor.offer,
        vec![NodeId(90)],
        Demand::Clipboard,
        link(vec![vec![response.clone()]]),
    )
    .await
    .unwrap();
    assert_eq!(files.descriptor().roots, received);
    for requested in [vec![NodeId(1)], vec![NodeId(91)], vec![NodeId(90), NodeId(1)]] {
        assert!(
            Linked::acquire(
                descriptor.offer,
                requested,
                Demand::Clipboard,
                link(vec![vec![response.clone()]])
            )
            .await
            .is_err()
        );
    }
    let mut reversed = response.clone();
    if let ContentMsg::CloudLinkOffer { roots, .. } = &mut reversed {
        roots.reverse();
    }
    assert!(
        Linked::acquire(
            descriptor.offer,
            vec![NodeId(90)],
            Demand::Clipboard,
            link(vec![vec![reversed]])
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn publication_uses_fulfilled_root_metadata_after_preparation() {
    let descriptor = descriptor(11);
    let mut announced = descriptor.roots.clone();
    announced[0].size = ContentSize::Unknown;
    announced[0].version = 0;
    let link = link(vec![vec![ContentMsg::CloudLinkOffer {
        native: Default::default(),
        offer: descriptor.offer,
        token: descriptor.token,
        roots: descriptor.roots.clone(),
        fulfilled: vec![super::super::grants::FulfilledRoot {
            node: NodeId(1),
            id: [4; 16],
        }],
    }]]);
    let publication = Publication::new(
        descriptor.offer,
        announced.clone(),
        link,
        Demand::Clipboard,
        tokio::runtime::Handle::current(),
    );
    assert_eq!(publication.roots(), announced);
    publication.prepare().await.unwrap();
    assert_eq!(publication.roots(), descriptor.roots);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restricted_native_publication_uses_committed_metadata() {
    use crate::FileContents;
    use crate::content::{
        Provider, Remote,
        namespace::{Files, Restricted},
    };
    let descriptor = descriptor(11);
    let mut announced = descriptor.roots.clone();
    announced[0].size = ContentSize::Unknown;
    announced[0].version = 0;
    let receipt = super::super::grants::FulfilledRoot {
        node: NodeId(1),
        id: [4; 16],
    };
    let link = link(vec![vec![ContentMsg::CloudLinkOffer {
        native: Default::default(),
        offer: descriptor.offer,
        token: descriptor.token,
        roots: descriptor.roots.clone(),
        fulfilled: vec![receipt.clone()],
    }]]);
    let directory = tempfile::tempdir().unwrap();
    let mut inventory = super::super::tests::fixture().inventory();
    inventory.id = descriptor.offer;
    inventory.roots = announced.clone();
    let remote = Remote::new(
        directory.path(),
        crate::store::BatchId([202; 16]),
        inventory,
        oink_foundation::clipboard::ClipPlatform::Windows,
        link,
        Demand::Clipboard,
    )
    .unwrap();
    let restricted = Restricted::new(Arc::new(remote), announced);
    let files = Files::new(restricted.clone(), crate::sync::files::TargetOs::Windows).unwrap();
    tokio::task::spawn_blocking(move || {
        restricted.prepare().unwrap();
        assert_eq!(restricted.roots(), descriptor.roots);
        let committed = restricted.source_link().unwrap();
        assert_eq!(committed.roots, descriptor.roots);
        assert_eq!(committed.fulfilled, vec![receipt]);
        assert_eq!(files.descriptor_entries().unwrap().unwrap()[0].size, 11);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_promise_publication_replaces_private_containers_with_actual_roots() {
    use crate::content::native_metadata::{MacPromiseGroup, NativeFileMetadata};
    use crate::content::{Provider, Remote, namespace::Restricted};
    use crate::model::{
        ContentItem, Inventory, ItemId, Medium, NativeFormat, Representation, RepresentationData, RepresentationId,
    };
    use oink_foundation::clipboard::ClipPlatform;
    let descriptor = descriptor(11);
    let inventory = Inventory {
        id: descriptor.offer,
        platform: ClipPlatform::Mac,
        roots: vec![],
        items: vec![ContentItem {
            id: ItemId(0),
            file: None,
            virtual_files: true,
            representations: vec![Representation {
                id: RepresentationId(1),
                format: NativeFormat::Mac("com.apple.pasteboard.promised-file-url".into()),
                standard: None,
                medium: Medium::Bytes,
                aspect: 1,
                index: -1,
                data: RepresentationData::Promise(NodeId(90), vec![]),
            }],
        }],
    };
    let requested = inventory.namespace_roots();
    let receipt = super::super::grants::FulfilledRoot {
        node: NodeId(1),
        id: [4; 16],
    };
    let native = NativeFileMetadata {
        windows: vec![],
        promises: vec![MacPromiseGroup {
            controls: Vec::new(),
            anchor: NodeId(90),
            roots: vec![NodeId(1)],
            indices: vec![0],
        }],
    };
    let transport = link(vec![vec![ContentMsg::CloudLinkOffer {
        offer: descriptor.offer,
        token: descriptor.token,
        roots: descriptor.roots.clone(),
        native: native.clone(),
        fulfilled: vec![receipt.clone()],
    }]]);
    let directory = tempfile::tempdir().unwrap();
    let remote = Remote::new(
        directory.path(),
        crate::store::BatchId([207; 16]),
        inventory,
        ClipPlatform::Windows,
        transport.clone(),
        Demand::Clipboard,
    )
    .unwrap();
    assert!(
        remote.inventory().roots.is_empty(),
        "never advertise the private container as a received file"
    );
    let restricted = Restricted::new(
        remote
            .namespace()
            .expect("foreign promises still carry a file capability"),
        requested,
    );
    let files =
        crate::content::namespace::Files::new(restricted.clone(), crate::sync::files::TargetOs::current()).unwrap();
    let mut spec = crate::NativeSpec {
        inventory: remote.inventory(),
        contents: Arc::new(remote),
        files: files.roots().to_vec(),
        file_contents: Some(files),
        sensitivity: crate::Sensitivity::Normal,
    };
    tokio::task::spawn_blocking(move || {
        spec.prepare_promises().unwrap();
        assert_eq!(spec.files.len(), 1);
        assert_eq!(spec.files[0].path, "file");
        assert_eq!(spec.files[0].size, 11);
        assert_eq!(spec.inventory.roots, descriptor.roots);
        assert_eq!(spec.inventory.items[0].file, Some(NodeId(1)));
        assert!(matches!(
            spec.inventory.items[0].representations[0].data,
            RepresentationData::Promise(NodeId(90), _)
        ));
        assert!(matches!(
            spec.inventory.items[0].representations[1].data,
            RepresentationData::File(NodeId(1))
        ));
        spec.prepare_promises().unwrap();
        assert_eq!(restricted.roots(), descriptor.roots);
        let committed = restricted.source_link().unwrap();
        assert_eq!(committed.roots, descriptor.roots);
        assert_eq!(committed.fulfilled, [receipt]);
        assert_eq!(committed.native, native);
        assert!(restricted.select(NodeId(90)).is_err());
        assert_eq!(
            transport.opened.load(Ordering::Relaxed),
            1,
            "the private anchor must not escape into a later RPC"
        );
    })
    .await
    .unwrap();
}
fn selected(d: &Descriptor) -> Vec<ContentMsg> {
    vec![ContentMsg::FileSelected {
        offer: d.offer,
        entry: d.roots[0].clone(),
    }]
}
fn block(d: &Descriptor, offset: u64, bytes: Vec<u8>) -> ContentMsg {
    ContentMsg::FileBlock {
        offer: d.offer,
        node: NodeId(1),
        version: 1,
        offset,
        bytes,
    }
}
fn end(d: &Descriptor, size: u64) -> ContentMsg {
    ContentMsg::FileEnd {
        offer: d.offer,
        node: NodeId(1),
        version: 1,
        size,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_is_metadata_only_and_open_streams_without_a_receiver_batch() {
    let d = descriptor(3);
    let encoded = serde_json::to_vec(&d).unwrap();
    let restored = serde_json::from_slice(&encoded).unwrap();
    let link = link(vec![selected(&d), vec![block(&d, 0, b"abc".to_vec()), end(&d, 3)]]);
    let files = Linked::restore(restored, link.clone(), tokio::runtime::Handle::current()).unwrap();
    assert_eq!(link.opened.load(Ordering::SeqCst), 0);
    assert!(files.local_batch().is_none());
    assert_eq!(files.roots(), d.roots);
    let bytes = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        files.open(NodeId(1), 1, 0).unwrap().read_to_end(&mut bytes).unwrap();
        bytes
    })
    .await
    .unwrap();
    assert_eq!(bytes, b"abc");
    assert!(link.requests.lock().unwrap().iter().all(ContentMsg::persistent_request));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_block_is_not_visible_without_a_valid_terminal_response() {
    for tail in [
        vec![],
        vec![ContentMsg::FileFailed {
            offer: descriptor(3).offer,
            node: NodeId(1),
            reason: Unavailable::Superseded,
        }],
        vec![end(&descriptor(3), 4)],
    ] {
        let d = descriptor(3);
        let mut replies = vec![block(&d, 0, b"abc".to_vec())];
        replies.extend(tail);
        let files = Linked::restore(
            d.clone(),
            link(vec![selected(&d), replies]),
            tokio::runtime::Handle::current(),
        )
        .unwrap();
        tokio::task::spawn_blocking(move || {
            let mut bytes = Vec::new();
            assert!(files.open(NodeId(1), 1, 0).unwrap().read_to_end(&mut bytes).is_err());
            assert!(bytes.is_empty());
        })
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_version_must_match_before_read_and_invalid_offset_is_terminal() {
    let d = descriptor(3);
    let mut corrupted = block(&d, 0, b"abc".to_vec());
    if let ContentMsg::FileBlock { offset, .. } = &mut corrupted {
        *offset = 1;
    }
    let files = Linked::restore(
        d.clone(),
        link(vec![selected(&d), selected(&d), vec![corrupted, end(&d, 3)]]),
        tokio::runtime::Handle::current(),
    )
    .unwrap();
    tokio::task::spawn_blocking(move || {
        assert_eq!(
            super::super::reason(&files.open(NodeId(1), 2, 0).err().unwrap()),
            Unavailable::Superseded
        );
        let mut reader = files.open(NodeId(1), 1, 0).unwrap();
        assert!(reader.read(&mut [0; 3]).is_err());
        assert!(reader.read(&mut [0; 3]).is_err());
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_seek_requests_only_the_aligned_suffix() {
    let d = descriptor(BLOCK + 3);
    let link = link(vec![
        selected(&d),
        vec![block(&d, BLOCK, b"abc".to_vec()), end(&d, BLOCK + 3)],
    ]);
    let files = Linked::restore(d, link.clone(), tokio::runtime::Handle::current()).unwrap();
    tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        files
            .open(NodeId(1), 1, BLOCK + 1)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"bc");
    })
    .await
    .unwrap();
    assert!(
        matches!(link.requests.lock().unwrap().last(), Some(ContentMsg::CloudLinkRead { offset, .. }) if *offset == BLOCK)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_rejects_already_buffered_bytes_and_zero_length_requires_eof() {
    let d = descriptor(BLOCK + 1);
    let files = Linked::restore(
        d.clone(),
        link(vec![selected(&d), vec![block(&d, 0, vec![4; BLOCK as usize])]]),
        tokio::runtime::Handle::current(),
    )
    .unwrap();
    tokio::task::spawn_blocking(move || {
        let mut reader = files.open(NodeId(1), 1, 0).unwrap();
        assert_eq!(reader.read(&mut [0; 1]).unwrap(), 1);
        files.cancel();
        assert_eq!(
            super::super::reason(&reader.read(&mut [0; 1]).unwrap_err()),
            Unavailable::Cancelled
        );
    })
    .await
    .unwrap();
    for eof in [false, true] {
        let d = descriptor(0);
        let replies = if eof { vec![end(&d, 0)] } else { vec![] };
        let files = Linked::restore(
            d.clone(),
            link(vec![selected(&d), replies]),
            tokio::runtime::Handle::current(),
        )
        .unwrap();
        tokio::task::spawn_blocking(move || {
            assert_eq!(files.open(NodeId(1), 1, 0).unwrap().read(&mut [0; 1]).is_ok(), eof);
        })
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_catalog_is_available_before_backend_and_old_owner_cannot_disconnect_replacement() {
    let connector = Connector::default();
    let d = descriptor(BLOCK + 1);
    let namespace = connector.restore(d.clone()).unwrap();
    assert_eq!(namespace.roots(), d.roots);
    assert_eq!(
        namespace.select(NodeId(1)).unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
    let old = connector.connect(
        link(vec![selected(&d), vec![block(&d, 0, vec![4; BLOCK as usize])]]),
        tokio::runtime::Handle::current(),
    );
    let files = namespace.clone();
    let mut reader = tokio::task::spawn_blocking(move || {
        let mut reader = files.open(NodeId(1), 1, 0).unwrap();
        assert_eq!(reader.read(&mut [0; 1]).unwrap(), 1);
        reader
    })
    .await
    .unwrap();
    let current = connector.connect(link(vec![selected(&d)]), tokio::runtime::Handle::current());
    drop(old);
    let files = namespace.clone();
    tokio::task::spawn_blocking(move || {
        assert!(
            reader.read(&mut [0; 1]).is_err(),
            "replacement retires buffered old operations"
        );
        assert!(files.select(NodeId(1)).is_ok(), "old guard cannot clear new backend");
    })
    .await
    .unwrap();
    drop(current);
    assert_eq!(
        namespace.select(NodeId(1)).unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
    assert_eq!(namespace.roots(), d.roots, "offline catalog preserves paths");
}

#[test]
fn restored_native_index_never_enumerates_offline_and_rejects_path_escape() {
    use crate::{FileContents, content::namespace::Files, sync::files::TargetOs};
    let d = descriptor(3);
    let namespace = Connector::default().restore(d.clone()).unwrap();
    let files = Files::new(namespace.clone(), TargetOs::Windows).unwrap();
    let entries = files.descriptor_entries().unwrap().unwrap();
    let restored =
        Files::restore_table(namespace.clone(), TargetOs::Windows, d.roots.clone(), entries.clone()).unwrap();
    assert_eq!(restored.descriptor_nodes().unwrap().unwrap(), d.roots);
    assert_eq!(restored.descriptor_entries().unwrap().unwrap(), entries);
    let mut invalid = entries;
    invalid[0].path = "C:/escaped".into();
    assert!(Files::restore_table(namespace, TargetOs::Windows, d.roots, invalid).is_err());
}

// Real registry and source identity store, with bounded transport in both
// directions. No collector can quietly download the file behind a slow reader.
struct RegistryLink(Arc<super::super::Registry>);
struct Pipe {
    send: Option<tokio::sync::mpsc::Sender<ContentMsg>>,
    receive: tokio::sync::mpsc::Receiver<ContentMsg>,
}
impl BulkChannel for Pipe {
    fn send<'a>(&'a mut self, message: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.send
                .as_ref()
                .unwrap()
                .send(
                    crate::wire::local_content(crate::wire::content(message).map_err(io::Error::other)?)
                        .map_err(io::Error::other)?,
                )
                .await
                .map_err(|_| io::ErrorKind::BrokenPipe.into())
        })
    }
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
        Box::pin(async { Ok(self.receive.recv().await) })
    }
    fn finish(&mut self) {
        self.send = None;
    }
}

impl BulkLink for RegistryLink {
    fn bind(self: Arc<Self>, _: DeviceId, _: ClipId) -> Arc<dyn BulkLink> {
        self
    }
    fn bind_persistent(self: Arc<Self>, _: DeviceId) -> io::Result<Arc<dyn BulkLink>> {
        Ok(self)
    }
    fn supports_cloud_links(&self) -> bool {
        true
    }
    fn open(&self, _: DeviceId) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>> {
        Box::pin(async {
            let (a, ar) = tokio::sync::mpsc::channel(1);
            let (b, br) = tokio::sync::mpsc::channel(1);
            let mut server = Pipe {
                send: Some(a),
                receive: br,
            };
            let registry = self.0.clone();
            tokio::spawn(async move {
                if let Ok(Some(request)) = server.recv().await {
                    let _ = registry.serve(DeviceId([2; 32]), request, &mut server).await;
                    server.finish();
                    let _ = server.recv().await;
                }
            });
            Ok(Box::new(Pipe {
                send: Some(b),
                receive: ar,
            }) as Box<dyn BulkChannel>)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acquired_files_survive_offer_loss_source_rename_and_both_namespace_restarts() {
    use super::super::{Registry, grants::SourceGrants, namespace::Local};
    let data = tempfile::tempdir().unwrap();
    let source = data.path().join("source");
    let bytes = vec![5; BLOCK as usize * 2 + 3];
    std::fs::write(&source, &bytes).unwrap();
    let members = [(DeviceId([2; 32]), [8; 32])];
    let grants = SourceGrants::load(&data.path().join("links"), members).unwrap();
    let registry = Arc::new(Registry::with_source_grants(grants));
    let provider = super::super::with_files(
        Arc::new(super::super::tests::fixture()),
        Local::new(vec![(NodeId(1), source.clone())]).unwrap(),
    );
    let offer = provider.inventory().id;
    let lease = registry.register(provider).unwrap();
    let files = Linked::acquire(
        offer,
        vec![NodeId(1)],
        Demand::Clipboard,
        Arc::new(RegistryLink(registry.clone())),
    )
    .await
    .unwrap();
    let saved = serde_json::to_vec(files.descriptor()).unwrap();
    drop(files);
    drop(lease);
    drop(registry);
    std::fs::rename(&source, data.path().join("renamed")).unwrap();
    std::fs::write(&source, b"replacement is not the offered file").unwrap();
    let grants = SourceGrants::load(&data.path().join("links"), members).unwrap();
    let registry = Arc::new(Registry::with_source_grants(grants.clone()));
    let files = Linked::restore(
        serde_json::from_slice(&saved).unwrap(),
        Arc::new(RegistryLink(registry)),
        tokio::runtime::Handle::current(),
    )
    .unwrap();
    let files = tokio::task::spawn_blocking(move || {
        let selected = files.select(NodeId(1)).unwrap();
        let mut reader = files.open(NodeId(1), selected.version, BLOCK + 1).unwrap();
        let mut actual = Vec::new();
        reader.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, bytes[BLOCK as usize + 1..]);
        files
    })
    .await
    .unwrap();
    grants.reconcile_members([]).unwrap();
    tokio::task::spawn_blocking(move || {
        assert!(files.select(NodeId(1)).is_err());
    })
    .await
    .unwrap();
    assert!(!data.path().join("completed-deliveries").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hover_acquires_only_metadata_and_drop_reuses_and_activates_the_same_token() {
    use super::super::{Registry, grants::SourceGrants, namespace::Local};
    let data = tempfile::tempdir().unwrap();
    let source = data.path().join("source");
    std::fs::write(&source, b"payload").unwrap();
    let peer = DeviceId([2; 32]);
    let grants = SourceGrants::load(&data.path().join("links"), [(peer, [8; 32])]).unwrap();
    let registry = Arc::new(Registry::with_source_grants(grants));
    let provider = super::super::with_files(
        Arc::new(super::super::tests::fixture()),
        Local::new(vec![(NodeId(1), source)]).unwrap(),
    );
    let offer = provider.inventory().id;
    let lease = registry.register(provider).unwrap();
    let holder = oink_foundation::content::Holder { device: peer, epoch: 1 };
    let link: Arc<dyn BulkLink> = Arc::new(RegistryLink(registry));
    let files = Linked::acquire(offer, vec![NodeId(1)], Demand::Hover(holder), link.clone())
        .await
        .unwrap();
    assert!(files.descriptor().provisional);
    let token = files.descriptor().token;
    tokio::task::spawn_blocking(move || {
        let selected = files.select(NodeId(1)).unwrap();
        assert!(files.open(NodeId(1), selected.version, 0).is_err());
    })
    .await
    .unwrap();
    let files = Linked::acquire(offer, vec![NodeId(1)], Demand::Drop(holder), link)
        .await
        .unwrap();
    assert!(!files.descriptor().provisional);
    assert_eq!(files.descriptor().token, token);
    lease.cancel();
    tokio::task::spawn_blocking(move || {
        let selected = files.select(NodeId(1)).unwrap();
        let mut bytes = Vec::new();
        files
            .open(NodeId(1), selected.version, 0)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"payload");
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_remote_publishes_persistent_credentials_and_never_creates_file_backing() {
    use super::super::{Provider, Registry, Remote, grants::SourceGrants, namespace::Local};
    let source_dir = tempfile::tempdir().unwrap();
    let receiver_dir = tempfile::tempdir().unwrap();
    let path = source_dir.path().join("source");
    std::fs::write(&path, b"persistent bytes").unwrap();
    let grants = SourceGrants::load(&source_dir.path().join("grants"), [(DeviceId([2; 32]), [8; 32])]).unwrap();
    let registry = Arc::new(Registry::with_source_grants(grants));
    let provider = super::super::with_files(
        Arc::new(super::super::tests::fixture()),
        Local::new(vec![(NodeId(1), path)]).unwrap(),
    );
    let inventory = provider.inventory();
    let lease = registry.register(provider).unwrap();
    let remote = Remote::new(
        receiver_dir.path(),
        crate::store::BatchId([200; 16]),
        inventory,
        oink_foundation::clipboard::ClipPlatform::Mac,
        Arc::new(RegistryLink(registry)),
        Demand::Clipboard,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_dir(receiver_dir.path()).unwrap().count(),
        0,
        "a file offer must not create the removed receiver backing cache"
    );
    let files = remote.namespace().unwrap();
    let files = tokio::task::spawn_blocking(move || {
        files.prepare().unwrap();
        assert!(files.source_link().is_some());
        assert!(files.local_batch().is_none());
        files
    })
    .await
    .unwrap();
    lease.cancel();
    remote.cancel();
    tokio::task::spawn_blocking(move || {
        let selected = files.select(NodeId(1)).unwrap();
        let mut bytes = Vec::new();
        files
            .open(selected.id, selected.version, 0)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"persistent bytes");
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read_dir(receiver_dir.path()).unwrap().count(), 0);
}

struct NativeCatalog {
    path: std::path::PathBuf,
    fail: AtomicBool,
}
impl crate::content::namespace::NativeFiles for NativeCatalog {
    fn persist_link(&self, descriptor: &Descriptor) -> io::Result<()> {
        if self.fail.load(Ordering::Acquire) {
            return Err(io::ErrorKind::WriteZero.into());
        }
        oink_foundation::storage::atomic_write(&self.path, &serde_json::to_vec(descriptor)?)
    }
    fn open(&self, _: NodeId, offset: u64) -> io::Result<FileReader> {
        use std::io::Seek;
        let mut bytes = io::Cursor::new(b"native copy");
        bytes.seek(io::SeekFrom::Start(offset))?;
        Ok(Box::new(bytes))
    }
    fn metadata(&self, _: NodeId) -> io::Result<crate::FileMetadata> {
        Ok(crate::FileMetadata {
            exec_bits: None,
            size: 11,
            modified_ms: None,
        })
    }
    fn download(
        &self,
        _: &crate::content::ReaderGuard,
        _: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> io::Result<Vec<crate::content::namespace::StoredFile>> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_activation_waits_for_native_catalog_commit_and_retry_keeps_the_same_token() {
    use crate::content::namespace::NativeFiles;
    let d = descriptor(11);
    let reply = vec![ContentMsg::CloudLinkOffer {
        native: Default::default(),
        offer: d.offer,
        token: d.token,
        roots: d.roots.clone(),
        fulfilled: vec![],
    }];
    let link = link(vec![reply.clone(), reply.clone(), reply]);
    let holder = oink_foundation::content::Holder {
        device: DeviceId([2; 32]),
        epoch: 1,
    };
    let publication = Publication::new(
        d.offer,
        d.roots,
        link,
        Demand::Hover(holder),
        tokio::runtime::Handle::current(),
    );
    assert!(publication.prepare().await.unwrap().descriptor().provisional);
    let directory = tempfile::tempdir().unwrap();
    let native = Arc::new(NativeCatalog {
        path: directory.path().join("catalog.json"),
        fail: AtomicBool::new(false),
    });
    let owner: Arc<dyn NativeFiles> = native.clone();
    publication.bind_native(Arc::downgrade(&owner)).unwrap();
    native.fail.store(true, Ordering::Release);
    publication.set_demand(Demand::Drop(holder));
    assert!(publication.prepare().await.is_err());
    assert!(publication.descriptor().unwrap().provisional);
    let saved: Descriptor = serde_json::from_slice(&std::fs::read(&native.path).unwrap()).unwrap();
    assert!(saved.provisional);
    native.fail.store(false, Ordering::Release);
    let active = publication.prepare().await.unwrap();
    assert_eq!(active.descriptor().token, d.token);
    let saved: Descriptor = serde_json::from_slice(&std::fs::read(&native.path).unwrap()).unwrap();
    assert!(!saved.provisional);
    publication.cancel();
    assert!(
        !publication.prepare().await.unwrap().descriptor().provisional,
        "closing the offer cannot revoke an activated received link"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_streams_and_metadata_use_the_received_copy_without_rechecking_the_source() {
    use crate::FileContents;
    use crate::content::{
        Provider, Remote,
        namespace::{Files, NativeFiles},
    };
    let d = descriptor(11);
    let link = link(vec![vec![ContentMsg::CloudLinkOffer {
        native: Default::default(),
        offer: d.offer,
        token: d.token,
        roots: d.roots.clone(),
        fulfilled: vec![],
    }]]);
    let mut inventory = super::super::tests::fixture().inventory();
    inventory.id = d.offer;
    inventory.roots = d.roots;
    let directory = tempfile::tempdir().unwrap();
    let remote = Remote::new(
        directory.path(),
        crate::store::BatchId([201; 16]),
        inventory,
        oink_foundation::clipboard::ClipPlatform::Mac,
        link.clone(),
        Demand::Clipboard,
    )
    .unwrap();
    let namespace = remote.namespace().unwrap();
    tokio::task::spawn_blocking(move || {
        namespace.prepare().unwrap();
        let owner: Arc<dyn NativeFiles> = Arc::new(NativeCatalog {
            path: directory.path().join("catalog.json"),
            fail: AtomicBool::new(false),
        });
        namespace.bind_native(Arc::downgrade(&owner)).unwrap();
        let files = Files::new(namespace, crate::sync::files::TargetOs::Windows).unwrap();
        assert_eq!(files.metadata(0).unwrap().unwrap().size, 11);
        let mut actual = Vec::new();
        files.open(0).unwrap().read_to_end(&mut actual).unwrap();
        assert_eq!(actual, b"native copy");
        assert_eq!(
            link.opened.load(Ordering::Acquire),
            1,
            "only initial grant acquisition used the source"
        );
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_reader_reopens_after_transport_loss_without_mixing_a_prefix() {
    for consumed in [64 * 1024, BLOCK as usize] {
        let d = descriptor(BLOCK + 3);
        let first = vec![0x5a; BLOCK as usize];
        let resumed = if consumed == BLOCK as usize {
            vec![block(&d, BLOCK, b"end".to_vec()), end(&d, BLOCK + 3)]
        } else {
            vec![
                block(&d, 0, first.clone()),
                block(&d, BLOCK, b"end".to_vec()),
                end(&d, BLOCK + 3),
            ]
        };
        let link = link(vec![selected(&d), vec![block(&d, 0, first.clone())], resumed]);
        let live = Arc::new(AtomicBool::new(true));
        *link.body_cut.lock().unwrap() = Some(live.clone());
        let files = Linked::restore(d, link.clone(), tokio::runtime::Handle::current()).unwrap();
        let bytes = tokio::task::spawn_blocking(move || {
            let mut reader = files.open(NodeId(1), 1, 0).unwrap();
            let mut bytes = vec![0; consumed];
            reader.read_exact(&mut bytes).unwrap();
            live.store(false, Ordering::Release);
            reader.read_to_end(&mut bytes).unwrap();
            bytes
        })
        .await
        .unwrap();
        assert_eq!(bytes, [first.clone(), b"end".to_vec()].concat());
        let requests = link.requests.lock().unwrap();
        let Some(ContentMsg::CloudLinkRead { offset, version, .. }) = requests.last() else {
            panic!("missing resume")
        };
        assert_eq!(*version, 1);
        assert_eq!(*offset, if consumed == BLOCK as usize { BLOCK } else { 0 });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_reader_reopens_a_final_block_when_transport_loses_eof() {
    let d = descriptor(3);
    let link = link(vec![
        selected(&d),
        vec![block(&d, 0, b"end".to_vec())],
        vec![block(&d, 0, b"end".to_vec()), end(&d, 3)],
    ]);
    *link.body_cut.lock().unwrap() = Some(Arc::new(AtomicBool::new(true)));
    let files = Linked::restore(d, link.clone(), tokio::runtime::Handle::current()).unwrap();
    let bytes = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        files.open(NodeId(1), 1, 0).unwrap().read_to_end(&mut bytes).unwrap();
        bytes
    })
    .await
    .unwrap();
    assert_eq!(bytes, b"end");
    assert!(matches!(
        link.requests.lock().unwrap().last(),
        Some(ContentMsg::CloudLinkRead { offset: 0, .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_reader_reconnect_relies_on_metadata_version_without_rehashing_buffered_bytes() {
    let d = descriptor(BLOCK + 3);
    let link = link(vec![
        selected(&d),
        vec![block(&d, 0, vec![1; BLOCK as usize])],
        vec![
            block(&d, 0, vec![2; BLOCK as usize]),
            block(&d, BLOCK, b"end".to_vec()),
            end(&d, BLOCK + 3),
        ],
    ]);
    let live = Arc::new(AtomicBool::new(true));
    *link.body_cut.lock().unwrap() = Some(live.clone());
    let files = Linked::restore(d, link, tokio::runtime::Handle::current()).unwrap();
    tokio::task::spawn_blocking(move || {
        let mut reader = files.open(NodeId(1), 1, 0).unwrap();
        reader.read_exact(&mut [0; 64]).unwrap();
        live.store(false, Ordering::Release);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, [vec![2; BLOCK as usize - 64], b"end".to_vec()].concat());
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_reader_retry_is_cancelled_while_the_peer_is_absent() {
    let d = descriptor(BLOCK + 3);
    let link = link(vec![selected(&d), vec![block(&d, 0, vec![1; BLOCK as usize])]]);
    *link.body_cut.lock().unwrap() = Some(Arc::new(AtomicBool::new(true)));
    let files = Linked::restore(d, link.clone(), tokio::runtime::Handle::current()).unwrap();
    let reading = files.clone();
    let task = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        let error = reading
            .open(NodeId(1), 1, 0)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap_err();
        assert_eq!(bytes.len(), BLOCK as usize);
        assert_eq!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<super::super::ContentError>()
                .unwrap()
                .0,
            Unavailable::Cancelled
        );
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while link.opened.load(Ordering::Acquire) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    files.cancel();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_reader_reconnect_denial_is_terminal() {
    let d = descriptor(BLOCK + 3);
    let link = link(vec![
        selected(&d),
        vec![block(&d, 0, vec![1; BLOCK as usize])],
        vec![ContentMsg::FileFailed {
            offer: d.offer,
            node: NodeId(1),
            reason: Unavailable::Permission,
        }],
    ]);
    *link.body_cut.lock().unwrap() = Some(Arc::new(AtomicBool::new(true)));
    let files = Linked::restore(d, link.clone(), tokio::runtime::Handle::current()).unwrap();
    tokio::task::spawn_blocking(move || {
        let mut reader = files.open(NodeId(1), 1, 0).unwrap();
        let mut bytes = Vec::new();
        assert!(reader.read_to_end(&mut bytes).is_err());
        assert_eq!(bytes.len(), BLOCK as usize);
        assert!(reader.read(&mut [0]).is_err());
    })
    .await
    .unwrap();
    assert_eq!(link.opened.load(Ordering::Acquire), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_performance_records_verified_boundaries_without_per_chunk_events() {
    use crate::timing::{Measurement as M, Outcome as O};
    for corrupt in [false, true] {
        let d = descriptor(BLOCK + 3);
        let mut first = block(&d, 0, vec![7; BLOCK as usize]);
        if corrupt && let ContentMsg::FileBlock { offset, .. } = &mut first {
            *offset = 1;
        }
        let files = Linked::restore(
            d.clone(),
            link(vec![
                selected(&d),
                vec![first, block(&d, BLOCK, b"abc".to_vec()), end(&d, BLOCK + 3)],
            ]),
            tokio::runtime::Handle::current(),
        )
        .unwrap();
        let records = tokio::task::spawn_blocking(move || {
            crate::timing::tests::capture(|| {
                let mut reader = files.open(NodeId(1), 1, 0).unwrap();
                let mut buffer = [0; 97];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(_) => assert!(!corrupt),
                        Err(_) => {
                            assert!(corrupt);
                            break;
                        }
                    }
                }
            })
        })
        .await
        .unwrap();
        if corrupt {
            assert!(matches!(
                records.as_slice(),
                [M::FileComplete {
                    outcome: O::Failed(Unavailable::InvalidData),
                    ..
                }]
            ));
        } else {
            assert!(matches!(
                records.as_slice(),
                [
                    M::FileFirstByte { .. },
                    M::FileComplete {
                        outcome: O::Completed,
                        ..
                    }
                ]
            ));
        }
    }
    let d = descriptor(3);
    let files = Linked::restore(
        d.clone(),
        link(vec![selected(&d), vec![block(&d, 0, b"abc".to_vec()), end(&d, 3)]]),
        tokio::runtime::Handle::current(),
    )
    .unwrap();
    let records = tokio::task::spawn_blocking(move || {
        crate::timing::tests::capture(|| {
            let mut bytes = Vec::new();
            files.open(NodeId(1), 1, 1).unwrap().read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"bc");
        })
    })
    .await
    .unwrap();
    assert!(records.is_empty(), "range reads must not report whole-file throughput");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_target_projects_a_filtered_link_root_out_of_native_inventory() {
    use crate::content::{Provider, Remote};
    use crate::model::{
        ContentItem, ItemId, Medium, NativeFormat, Representation, RepresentationData, RepresentationId,
    };
    let dir = tempfile::tempdir().unwrap();
    let mut inventory = crate::content::tests::fixture().inventory();
    let mut file = descriptor(3).roots.remove(0);
    file.id = NodeId(2);
    file.name = "file".into();
    let mut alias = file.clone();
    alias.id = NodeId(1);
    alias.name = "alias".into();
    alias.kind = crate::model::NodeKind::Symlink("file".into());
    alias.size = crate::model::ContentSize::Known(4);
    inventory.roots = vec![alias, file.clone()];
    inventory.items = [1u32, 2]
        .into_iter()
        .map(|id| ContentItem {
            id: ItemId(id),
            file: Some(NodeId(u64::from(id))),
            virtual_files: false,
            representations: vec![Representation {
                id: RepresentationId(id),
                format: NativeFormat::Mac("public.file-url".into()),
                medium: Medium::Bytes,
                aspect: 1,
                index: -1,
                standard: None,
                data: RepresentationData::File(NodeId(u64::from(id))),
            }],
        })
        .collect();
    let remote = Remote::new(
        dir.path(),
        crate::store::BatchId([8; 16]),
        inventory,
        oink_foundation::clipboard::ClipPlatform::Windows,
        Arc::new(Link {
            replies: Mutex::default(),
            requests: Arc::default(),
            opened: AtomicUsize::new(0),
            body_cut: Mutex::default(),
        }),
        crate::model::Demand::Clipboard,
    )
    .unwrap();
    let projected = remote.inventory();
    assert_eq!(projected.roots, [file]);
    assert_eq!(
        projected.items.iter().map(|item| item.id).collect::<Vec<_>>(),
        [ItemId(2)]
    );
    projected.validate().unwrap();
    assert_eq!(remote.namespace().unwrap().roots(), projected.roots);
    let files =
        crate::content::namespace::Files::new(remote.namespace().unwrap(), crate::sync::files::TargetOs::Windows)
            .unwrap();
    assert_eq!(files.roots().len(), 1);
    assert_eq!(files.roots()[0].path, "file");
}
