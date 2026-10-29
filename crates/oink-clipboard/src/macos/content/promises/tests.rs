use super::*;
use std::sync::atomic::AtomicUsize;

struct Static {
    inventory: Inventory,
    files: Arc<dyn Namespace>,
    reads: Arc<AtomicUsize>,
}
impl Provider for Static {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        Some(self.files.clone())
    }
    fn translator(&self) -> Arc<dyn Translator> {
        Arc::new(MacTranslator)
    }
    fn open(&self, _: RepresentationId) -> io::Result<crate::FileReader> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(io::Cursor::new(b"ordinary representation")))
    }
}
struct Fulfilled {
    files: Arc<dyn Namespace>,
    native: NativeFileMetadata,
    adopted: Arc<AtomicUsize>,
}
impl Namespace for Fulfilled {
    fn roots(&self) -> Vec<FileNode> {
        self.files.roots()
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        vec![NodeId(101)]
    }
    fn native_file_metadata(&self) -> NativeFileMetadata {
        self.native.clone()
    }
    fn release_fulfilled(&self, roots: &[NodeId]) {
        assert_eq!(roots, [NodeId(101)]);
        self.adopted.fetch_add(1, Ordering::Relaxed);
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.files.list(node, cursor)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        self.files.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
        self.files.open(node, version, offset)
    }
}

#[test]
fn clipboard_fulfils_once_on_demand_and_keeps_real_files_independent() {
    let dir = tempfile::tempdir().unwrap();
    let real_path = dir.path().join("real");
    let promised_path = dir.path().join("promised");
    std::fs::write(&real_path, b"persistent real file").unwrap();
    std::fs::write(&promised_path, b"promised file").unwrap();
    let real = content::namespace::Local::clipboard(vec![(NodeId(10), real_path.clone())]).unwrap();
    let files = content::namespace::Local::new(vec![(NodeId(10), real_path), (NodeId(101), promised_path)]).unwrap();
    let native = NativeFileMetadata {
        windows: vec![],
        promises: vec![MacPromiseGroup {
            controls: Vec::new(),
            anchor: NodeId(90),
            roots: vec![NodeId(101)],
            indices: vec![0],
        }],
    };
    let mut inventory = content::tests::fixture().inventory();
    inventory.platform = ClipPlatform::Mac;
    inventory.roots = real.roots();
    inventory.items = vec![ContentItem {
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
    }];
    let reads = Arc::new(AtomicUsize::new(0));
    let adopted = Arc::new(AtomicUsize::new(0));
    let fulfilled: Arc<dyn Provider> = Arc::new(Static {
        inventory: content::native_metadata::project_inventory(inventory.clone(), files.roots(), &native).unwrap(),
        files: Arc::new(Fulfilled {
            files,
            native: native.clone(),
            adopted: adopted.clone(),
        }),
        reads: reads.clone(),
    });
    let raw: Arc<dyn Provider> = Arc::new(Static {
        inventory: inventory.clone(),
        files: real,
        reads: reads.clone(),
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let receive_calls = calls.clone();
    let current = Current::new(
        raw,
        Arc::new(move || {
            receive_calls.fetch_add(1, Ordering::Relaxed);
            Ok(fulfilled.clone())
        }),
        Arc::new(|| {}),
        Arc::new(AtomicBool::new(false)),
        Arc::new(|| Ok(Some([7; 32]))),
    );
    assert_eq!(Provider::inventory(&current), inventory);
    assert_eq!(current.one_shot_roots(), [NodeId(90)]);
    assert_eq!(current.source_roots().unwrap().len(), 1);
    assert!(current.select(NodeId(90)).is_err());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let other = current.clone();
    let worker = std::thread::spawn(move || other.prepare().unwrap());
    current.prepare().unwrap();
    worker.join().unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        reads.load(Ordering::Relaxed),
        0,
        "fulfilment must not snapshot unrelated representations"
    );
    assert_eq!(current.one_shot_roots(), [NodeId(101)]);
    assert_eq!(current.native_file_metadata(), native);
    assert_eq!(Provider::inventory(&current).items[0].file, Some(NodeId(101)));
    current.release_fulfilled(&[NodeId(101)]);
    assert_eq!(adopted.load(Ordering::Relaxed), 1);
    Namespace::cancel(&current);
    assert!(current.prepare().is_err());
    assert!(current.select(NodeId(101)).is_err());
    let real = current.select(NodeId(10)).unwrap();
    assert_eq!(
        content::read_all(&mut Namespace::open(&current, real.id, real.version, 0).unwrap()).unwrap(),
        b"persistent real file"
    );
    assert_eq!(current.source_roots().unwrap().len(), 1);
}
