use super::*;
use std::sync::atomic::AtomicUsize;

struct Promise {
    files: OneShot,
    prepared: AtomicBool,
    released: AtomicBool,
    calls: AtomicUsize,
}
impl Namespace for Promise {
    fn prepare(&self) -> io::Result<()> {
        assert!(
            !self.released.load(Ordering::Acquire),
            "never fulfil an adopted promise again"
        );
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.prepared.store(true, Ordering::Release);
        Ok(())
    }
    fn roots(&self) -> Vec<FileNode> {
        if self.prepared.load(Ordering::Acquire) {
            self.files.roots()
        } else {
            vec![FileNode {
                exec_bits: None,
                id: NodeId(90),
                parent: None,
                name: "_promise".into(),
                kind: crate::model::NodeKind::Directory,
                size: crate::model::ContentSize::Unknown,
                version: 0,
                modified_ms: None,
            }]
        }
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        self.roots().iter().map(|node| node.id).collect()
    }
    fn native_file_metadata(&self) -> NativeFileMetadata {
        if !self.prepared.load(Ordering::Acquire) {
            return NativeFileMetadata::default();
        }
        NativeFileMetadata {
            windows: vec![],
            promises: vec![crate::content::native_metadata::MacPromiseGroup {
                controls: Vec::new(),
                anchor: NodeId(90),
                roots: vec![NodeId(2), NodeId(1)],
                indices: vec![0, 1],
            }],
        }
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.files.list(node, cursor)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        assert!(!self.released.load(Ordering::Acquire));
        self.files.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        assert!(!self.released.load(Ordering::Acquire));
        self.files.open(node, version, offset)
    }
    fn release_fulfilled(&self, roots: &[NodeId]) {
        assert!(roots.contains(&NodeId(1)) && roots.contains(&NodeId(2)));
        self.released.store(true, Ordering::Release);
    }
}

#[test]
fn promise_anchor_expands_once_and_current_clipboard_reuses_for_other_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<_> = ["first", "second"].iter().map(|name| dir.path().join(name)).collect();
    for path in &paths {
        fs::write(path, b"promised bytes").unwrap();
    }
    let source = Promise {
        files: OneShot(Local::new(vec![(NodeId(1), paths[0].clone()), (NodeId(2), paths[1].clone())]).unwrap()),
        prepared: AtomicBool::new(false),
        released: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
    };
    let journal = dir.path().join("journal");
    let other = DeviceId([2; 32]);
    let members = [(PEER, [1; 32]), (other, [2; 32])];
    let store = SourceGrants::load(&journal, members).unwrap();
    let offer = crate::content::tests::fixture().inventory().id;
    store.observe_clipboard(Some((offer, Some([7; 32])))).unwrap();
    assert!(
        store
            .issue_for(
                DeviceId([9; 32]),
                Some(offer),
                &source,
                &[NodeId(90)],
                Demand::Clipboard
            )
            .is_err()
    );
    assert!(
        store
            .issue_for(PEER, Some(offer), &source, &[NodeId(99)], Demand::Clipboard)
            .is_err()
    );
    assert_eq!(source.calls.load(Ordering::Relaxed), 0);
    let token = store
        .issue_for(PEER, Some(offer), &source, &[NodeId(90)], Demand::Clipboard)
        .unwrap();
    assert_eq!(source.calls.load(Ordering::Relaxed), 1);
    let files = store.open(PEER, token).unwrap();
    assert_eq!(
        files.roots().iter().map(|node| node.id).collect::<Vec<_>>(),
        [NodeId(2), NodeId(1)]
    );
    assert!(files.select(NodeId(90)).is_err());
    assert!(
        store
            .issue_for(PEER, Some(offer), &source, &[NodeId(90), NodeId(1)], Demand::Clipboard)
            .is_err()
    );
    assert_eq!(
        store
            .issue_for(PEER, Some(offer), &source, &[NodeId(90)], Demand::Clipboard)
            .unwrap()
            .0,
        token.0
    );
    let receipts = store.fulfilled_roots(PEER, token).unwrap();
    assert_eq!(receipts.len(), 2);
    for receipt in &receipts {
        store.sent(PEER, token, receipt.node).unwrap();
    }
    drop(files);
    for path in paths {
        fs::remove_file(path).unwrap();
    }
    let token = store
        .issue_for(other, Some(offer), &source, &[NodeId(90)], Demand::Clipboard)
        .unwrap();
    assert_eq!(source.calls.load(Ordering::Relaxed), 1);
    assert_eq!(store.fulfilled_roots(other, token).unwrap(), receipts);
    let files = store.open(other, token).unwrap();
    assert_eq!(
        files.roots().iter().map(|node| node.id).collect::<Vec<_>>(),
        [NodeId(2), NodeId(1)]
    );
    for root in files.roots() {
        let selected = files.select(root.id).unwrap();
        let mut bytes = Vec::new();
        files
            .open(root.id, selected.version, 0)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"promised bytes");
    }
}
