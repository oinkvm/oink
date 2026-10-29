//! Exercise source fulfilment ownership without native input or desktop access.
use super::*;
use crate::content::Provider;
use crate::model::{ContentSize, NodeKind};
use oink_foundation::content::Holder;
use std::sync::atomic::AtomicUsize;

#[derive(Clone, Copy, Debug)]
enum Kind {
    MacPromise,
    WindowsVirtual,
    RealFile,
}
struct Source {
    files: Arc<namespace::Local>,
    kind: Kind,
    prepared: AtomicUsize,
    released: AtomicBool,
}
impl Source {
    fn requested(&self) -> NodeId {
        if matches!(self.kind, Kind::MacPromise) {
            NodeId(90)
        } else {
            NodeId(1)
        }
    }
}
impl Namespace for Source {
    fn prepare(&self) -> io::Result<()> {
        if self.released.load(Ordering::Acquire) {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        self.prepared.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn roots(&self) -> Vec<FileNode> {
        if matches!(self.kind, Kind::MacPromise) && self.prepared.load(Ordering::Relaxed) == 0 {
            vec![FileNode {
                id: NodeId(90),
                parent: None,
                name: "_promise".into(),
                kind: NodeKind::Directory,
                size: ContentSize::Unknown,
                version: 0,
                modified_ms: None,
                exec_bits: None,
            }]
        } else {
            self.files.roots()
        }
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        if matches!(self.kind, Kind::RealFile) {
            vec![]
        } else {
            self.roots().iter().map(|n| n.id).collect()
        }
    }
    fn native_file_metadata(&self) -> NativeFileMetadata {
        use crate::content::native_metadata::{MacPromiseGroup, WindowsFileDescriptor};
        match self.kind {
            Kind::MacPromise if self.prepared.load(Ordering::Relaxed) != 0 => NativeFileMetadata {
                promises: vec![MacPromiseGroup {
                    anchor: NodeId(90),
                    roots: vec![NodeId(1)],
                    indices: vec![0],
                    controls: vec![],
                }],
                windows: vec![],
            },
            Kind::WindowsVirtual => NativeFileMetadata {
                windows: vec![WindowsFileDescriptor {
                    root: NodeId(1),
                    node: NodeId(1),
                    index: 0,
                    wide: Some(vec![0; 72]),
                    ansi: None,
                }],
                promises: vec![],
            },
            _ => NativeFileMetadata::default(),
        }
    }
    fn release_fulfilled(&self, roots: &[NodeId]) {
        self.files.release_fulfilled(roots);
        self.released.store(true, Ordering::Release);
    }
    fn source_roots(&self) -> io::Result<Vec<SourceRoot>> {
        self.files.source_roots()
    }
    fn list(&self, id: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.files.list(id, cursor)
    }
    fn select(&self, id: NodeId) -> io::Result<FileNode> {
        self.files.select(id)
    }
    fn open(&self, id: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        self.files.open(id, version, offset)
    }
}

#[test]
fn mac_promise_recrosses_source_and_third_host_without_drop() {
    recross(Kind::MacPromise, true);
}
#[test]
fn windows_virtual_recrosses_source_and_third_host_without_drop() {
    recross(Kind::WindowsVirtual, true);
}
#[test]
fn ordinary_file_recrosses_source_and_third_host_without_drop() {
    recross(Kind::RealFile, true);
}
#[test]
fn next_host_preparation_reuses_fulfilment_even_before_previous_host_leaves() {
    recross(Kind::MacPromise, false);
    recross(Kind::WindowsVirtual, false);
}
fn recross(kind: Kind, cancel_previous: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.png");
    fs::write(&path, b"promised image").unwrap();
    let source = Source {
        files: namespace::Local::new(vec![(NodeId(1), path)]).unwrap(),
        kind,
        prepared: AtomicUsize::new(0),
        released: AtomicBool::new(false),
    };
    let a = DeviceId([1; 32]);
    let b = DeviceId([2; 32]);
    let c = DeviceId([3; 32]);
    let store = SourceGrants::load(dir.path(), [(a, [1; 32]), (b, [2; 32]), (c, [3; 32])]).unwrap();
    let offer = crate::content::tests::fixture().inventory().id;
    let mut first = None;
    // A -> B, leave B without Drop; B -> A, leave A; A -> C.
    // Returning an exported source also needs a fresh local host grant.
    for (epoch, peer) in [b, a, c].into_iter().enumerate() {
        let holder = Holder {
            device: peer,
            epoch: epoch as u64 + 1,
        };
        let token = store
            .issue_for(peer, Some(offer), &source, &[source.requested()], Demand::Hover(holder))
            .unwrap_or_else(|e| panic!("{kind:?} crossing {epoch}: {e:?}"));
        let files = store.open(peer, token).unwrap();
        let selected = files.select(NodeId(1)).unwrap();
        assert!(
            files.open(selected.id, selected.version, 0).is_err(),
            "Hover cannot read bytes"
        );
        let fulfilled = store.fulfilled_roots(peer, token).unwrap();
        assert_eq!(files.native_file_metadata(), source.native_file_metadata());
        if let Some(first) = &first {
            assert_eq!(&fulfilled, first, "reuse the same fulfilment");
        } else {
            first = Some(fulfilled);
        }
        if peer == c {
            assert_eq!(
                token,
                store
                    .issue_for(peer, Some(offer), &source, &[source.requested()], Demand::Drop(holder))
                    .unwrap()
            );
            let mut bytes = Vec::new();
            files
                .open(selected.id, selected.version, 0)
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            assert_eq!(bytes, b"promised image");
            store.sent(peer, token, selected.id).unwrap();
            let state = store.0.state.lock().unwrap();
            for snapshot in state.journal.grants.iter().flat_map(|g| &g.snapshots) {
                assert!(
                    !snapshot.path(&store.0.path).exists(),
                    "completed send releases old hover references too"
                );
            }
        } else if cancel_previous {
            files.cancel();
            assert!(files.select(selected.id).is_err(), "old context remains cancelled");
        }
    }
    if !matches!(kind, Kind::RealFile) {
        assert_eq!(
            source.prepared.load(Ordering::Relaxed),
            1,
            "never ask the native one-shot owner twice"
        );
    }
}

#[test]
fn provisional_fulfilment_ends_with_source_but_committed_delivery_keeps_its_own_reference() {
    for outcome in 0..4 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        fs::write(&path, b"image").unwrap();
        let source = Source {
            files: namespace::Local::new(vec![(NodeId(1), path)]).unwrap(),
            kind: Kind::WindowsVirtual,
            prepared: AtomicUsize::new(0),
            released: AtomicBool::new(false),
        };
        let peer = DeviceId([1; 32]);
        let store = SourceGrants::load(dir.path(), [(peer, [1; 32])]).unwrap();
        let offer = crate::content::tests::fixture().inventory().id;
        let holder = Holder { device: peer, epoch: 1 };
        let token = store
            .issue_for(peer, Some(offer), &source, &[NodeId(1)], Demand::Hover(holder))
            .unwrap();
        let snapshot = store.0.state.lock().unwrap().journal.grants[0].snapshots[0].path(&store.0.path);
        store.open(peer, token).unwrap().cancel();
        {
            let mut state = store.0.state.lock().unwrap();
            // No amount of time spent hovering is a failed transfer.
            state.journal.grants[0].snapshots[0].deadline_ms = 0;
            store.expire_locked(&mut state, store.now_ms()).unwrap();
        }
        assert!(snapshot.exists());
        if outcome == 0 {
            store.discard_pending(offer).unwrap();
            assert!(store.open(peer, token).is_err());
        } else {
            let holder = Holder { epoch: 2, ..holder };
            let token = store
                .issue_for(peer, Some(offer), &source, &[NodeId(1)], Demand::Hover(holder))
                .unwrap();
            assert_eq!(
                token,
                store
                    .issue_for(peer, Some(offer), &source, &[NodeId(1)], Demand::Drop(holder))
                    .unwrap()
            );
            assert!(
                store
                    .0
                    .state
                    .lock()
                    .unwrap()
                    .journal
                    .grants
                    .iter()
                    .find(|g| g.token == token)
                    .unwrap()
                    .snapshots[0]
                    .deadline_ms
                    > store.now_ms()
            );
            let files = store.open(peer, token).unwrap();
            assert!(files.select(NodeId(1)).is_ok());
            assert!(snapshot.exists());
            if outcome == 1 {
                store.abandon(peer, token).unwrap();
            } else if outcome == 2 {
                // Native source end must not interrupt an already committed Drop.
                store.discard_pending(offer).unwrap();
                assert!(files.select(NodeId(1)).is_ok());
                assert!(snapshot.exists());
                store.sent(peer, token, NodeId(1)).unwrap();
            } else {
                let mut state = store.0.state.lock().unwrap();
                state
                    .journal
                    .grants
                    .iter_mut()
                    .find(|g| g.token == token)
                    .unwrap()
                    .snapshots[0]
                    .deadline_ms = 0;
                store.expire_locked(&mut state, store.now_ms()).unwrap();
                fulfilled::collect(&store.0.path, &state.journal).unwrap();
            }
        }
        assert!(
            !snapshot.exists(),
            "source cancel / actual send success or failure releases bytes"
        );
    }
}
