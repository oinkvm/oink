//! Main-thread cross-process native receiver reads of a published linked virtual file. No general/drag
//! pasteboard, desktop window, synthetic input, or direct delegate invocation.
use super::*;
use crate::content::{
    Provider,
    url::tests::{Projected, source, wide_bytes},
};
use crate::model::{Inventory, RepresentationId};
use objc2_foundation::{NSDate, NSRunLoop};
use oink_foundation::clipboard::ClipPlatform;
use std::{
    collections::HashMap,
    process::{Command, Stdio},
};

const BOARD: &str = "OINK_TEST_LINKED_PROMISE_BOARD";
const DESTINATION: &str = "OINK_TEST_LINKED_PROMISE_DESTINATION";
const MODE: &str = "OINK_TEST_LINKED_PROMISE_MODE";
const LINK: &str = "https://example.invalid/linked-page";
const BYTES: &[u8] = b"original received image bytes";

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn PasteboardCreate(name: *const c_void, board: *mut *mut c_void) -> i32;
    fn PasteboardSetPasteLocation(board: *mut c_void, location: *const c_void) -> i32;
    fn PasteboardGetItemIdentifier(board: *mut c_void, index: isize, item: *mut *mut c_void) -> i32;
    fn PasteboardCopyItemFlavorData(
        board: *mut c_void,
        item: *mut c_void,
        ty: *const c_void,
        data: *mut *mut NSData,
    ) -> i32;
    fn CFRelease(value: *const c_void);
}

fn receive_url_promise(name: &str, destination: &Path) -> String {
    let name = NSString::from_str(name);
    let location = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&destination.to_string_lossy()), true);
    let ty = NSString::from_str("com.apple.pasteboard.promised-file-url");
    let mut board = std::ptr::null_mut();
    let mut item = std::ptr::null_mut();
    let mut bytes = std::ptr::null_mut();
    // SAFETY: documented Pasteboard Manager ABI; NSString/NSURL are toll-free
    // bridged to CFString/CFURL. Both operations must use this same local
    // reference: paste location is receiver context, not an ordinary flavor a
    // different process may add with NSPasteboard::setStringForType.
    unsafe {
        assert_eq!(PasteboardCreate(Retained::as_ptr(&name).cast(), &mut board), 0);
        assert_eq!(PasteboardSetPasteLocation(board, Retained::as_ptr(&location).cast()), 0);
        assert_eq!(PasteboardGetItemIdentifier(board, 1, &mut item), 0);
        let status = PasteboardCopyItemFlavorData(board, item, Retained::as_ptr(&ty).cast(), &mut bytes);
        CFRelease(board);
        assert_eq!(
            status, 0,
            "published file promise must materialize through the independent receiver"
        );
        let bytes = Retained::from_raw(bytes).expect("successful Copy returns owned data");
        String::from_utf8(bytes.to_vec()).unwrap()
    }
}

struct Received {
    local: Arc<dyn Namespace>,
    reads: Arc<AtomicUsize>,
}
impl Namespace for Received {
    fn roots(&self) -> Vec<FileNode> {
        self.local.roots()
    }
    fn requires_native_files(&self) -> bool {
        true
    }
    fn list(&self, _: NodeId, _: Option<crate::model::DirectoryCursor>) -> io::Result<crate::model::DirectoryPage> {
        panic!("promise must not enumerate at source")
    }
    fn select(&self, _: NodeId) -> io::Result<FileNode> {
        panic!("promise must not select at source")
    }
    fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<crate::FileReader> {
        panic!("promise must not read at source")
    }
    fn native_open(&self, node: NodeId, offset: u64) -> Option<io::Result<crate::FileReader>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Some(
            self.local
                .select(node)
                .and_then(|entry| self.local.open(node, entry.version, offset)),
        )
    }
    fn native_metadata(&self, node: NodeId) -> Option<io::Result<crate::FileMetadata>> {
        Some(self.local.select(node).map(|entry| crate::FileMetadata {
            size: match entry.size {
                crate::model::ContentSize::Known(n) => n,
                _ => unreachable!(),
            },
            modified_ms: entry.modified_ms,
            exec_bits: entry.exec_bits,
        }))
    }
}
struct Linked {
    contents: Projected,
    received: Arc<dyn Namespace>,
    reads: Arc<AtomicUsize>,
}
impl Provider for Linked {
    fn inventory(&self) -> Inventory {
        self.contents.inventory()
    }
    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        Some(self.received.clone())
    }
    fn translator(&self) -> Arc<dyn content::Translator> {
        self.contents.translator()
    }
    fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.contents.open(id)
    }
}

fn publish(path: &Path, board: &NSPasteboard) -> (Writers, Vec<Arc<Proxy>>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let image_reads = Arc::new(AtomicUsize::new(0));
    let received: Arc<dyn Namespace> = Arc::new(Received {
        local: content::namespace::Local::new(vec![(NodeId(1), path.to_owned())]).unwrap(),
        reads: image_reads.clone(),
    });
    let mut source = source(
        ClipPlatform::Windows,
        &[
            ("UniformResourceLocatorW", wide_bytes(LINK)),
            ("text/x-moz-url", wide_bytes(&format!("{LINK}\nLinked image"))),
        ],
    );
    source.inventory.roots = received.roots();
    source.inventory.items[0].file = Some(NodeId(1));
    source.inventory.items[0].virtual_files = true;
    let reads = Arc::new(AtomicUsize::new(0));
    let spec = crate::NativeSpec {
        inventory: source.inventory.clone(),
        contents: Arc::new(Linked {
            contents: Projected(source, ClipPlatform::Mac),
            received,
            reads: reads.clone(),
        }),
        files: vec![],
        file_contents: None,
        sensitivity: crate::Sensitivity::Normal,
    };
    let urls = HashMap::from([(NodeId(1), super::super::super::pasteboard::file_url_string(path))]);
    let writers = super::super::super::content::writers(&spec, &urls).unwrap();
    let (writers, proxies) = wrap_drag(&spec, writers).unwrap();
    assert!(board.writeObjects(&NSArray::from_retained_slice(&writers)));
    assert_eq!(reads.load(Ordering::SeqCst), 0, "publication must not fetch companions");
    assert_eq!(
        image_reads.load(Ordering::SeqCst),
        0,
        "publication must not materialize the file"
    );
    (writers, proxies, reads, image_reads)
}

pub(in crate::macos) fn run() {
    let mtm = MainThreadMarker::new().expect("native promise harness must own the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    if let Ok(name) = std::env::var(BOARD) {
        read_published_link(
            &name,
            &PathBuf::from(std::env::var_os(DESTINATION).unwrap()),
            &std::env::var(MODE).unwrap(),
        );
        return;
    }
    objc2::rc::autoreleasepool(|_| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        fs::write(&path, BYTES).unwrap();
        for mode in ["new-file", "existing-file"] {
            let destination = dir.path().join(mode);
            fs::create_dir(&destination).unwrap();
            if mode == "existing-file" {
                fs::write(destination.join("image.png"), b"existing file").unwrap();
            }
            let board = crate::macos::test_pasteboard::new();
            let (_writers, _proxies, reads, image_reads) = publish(&path, &board);
            run_reader(&board, &destination, mode);
            assert!(reads.load(Ordering::SeqCst) > 0, "reader must resolve the lazy URL");
            assert_eq!(
                image_reads.load(Ordering::SeqCst),
                1,
                "only the received copy supplies the file"
            );
            let name = if mode == "existing-file" {
                "image 2.png"
            } else {
                "image.png"
            };
            assert_eq!(fs::read(destination.join(name)).unwrap(), BYTES);
            if mode == "existing-file" {
                assert_eq!(fs::read(destination.join("image.png")).unwrap(), b"existing file");
            }
            board.clearContents();
        }
    });
}

fn run_reader(board: &NSPasteboard, destination: &Path, mode: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .env(BOARD, board.name().to_string())
        .env(DESTINATION, destination)
        .env(MODE, mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "independent native reader timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.01));
    }
    let output = child.wait_with_output().unwrap();
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "independent native reader failed: {}",
        output.status
    );
}

fn read_published_link(name: &str, destination: &Path, mode: &str) {
    objc2::rc::autoreleasepool(|_| {
        let board = NSPasteboard::pasteboardWithName(&NSString::from_str(name));
        let item = board.pasteboardItems().unwrap().objectAtIndex(0);
        let types: Vec<_> = item.types().iter().map(|ty| ty.to_string()).collect();
        eprintln!("native reader mode={mode} types={types:?}");
        assert!(!types.iter().any(|ty| ty == "public.file-url"));
        assert_eq!(
            item.stringForType(&NSString::from_str("public.url"))
                .unwrap()
                .to_string(),
            LINK
        );
        assert_eq!(
            item.stringForType(&NSString::from_str("public.url-name"))
                .unwrap()
                .to_string(),
            "Linked image"
        );
        assert!(
            item.stringForType(&NSString::from_str("public.html"))
                .unwrap()
                .to_string()
                .contains(LINK)
        );
        let name = if mode == "existing-file" {
            "image 2.png"
        } else {
            "image.png"
        };
        assert!(
            !destination.join(name).exists(),
            "URL reads must not materialize the image"
        );
        let url = receive_url_promise(&board.name().to_string(), destination);
        let path = NSURL::URLWithString(&NSString::from_str(&url)).unwrap().path().unwrap();
        assert_eq!(Path::new(&path.to_string()), destination.join(name));
        assert_eq!(
            fs::read(destination.join(name)).unwrap(),
            BYTES,
            "the independent consumer must materialize the original file"
        );
        assert_eq!(
            item.stringForType(&NSString::from_str(FILE_PROMISE_CONTENT))
                .unwrap()
                .to_string(),
            "public.png"
        );
        assert_eq!(
            types,
            [
                "public.url",
                "public.url-name",
                "public.html",
                "public.utf8-plain-text",
                FILE_URL_PROMISE,
                FILE_PROMISE_CONTENT,
            ],
            "linked drags replace modern controls rather than advertise two promise protocols"
        );
    });
}
