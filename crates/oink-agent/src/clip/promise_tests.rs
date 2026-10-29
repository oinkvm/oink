//! Promise capabilities without announced names still reach native publication.
use super::*;
use crate::snapshot_tests::{Displays, common, start_with_clip};
use oink_clipboard::{ChangeHandler, ClipData, ClipSnapshot, ClipboardError, Sensitivity};
use oink_test_support::clipboard::FakeClipboard;

struct PromiseClipboard {
    clipboard: Arc<FakeClipboard>,
}
impl ClipboardBackend for PromiseClipboard {
    fn change_count(&self) -> u64 {
        self.clipboard.change_count()
    }
    fn read(&self) -> Result<ClipSnapshot, ClipboardError> {
        self.clipboard.read()
    }
    fn capture(&self, data: &std::path::Path) -> Result<oink_clipboard::CapturedClipboard, ClipboardError> {
        if self.change_count() == 0 {
            return self.clipboard.capture(data);
        }
        use oink_clipboard::model::*;
        use oink_foundation::clipboard::ClipPlatform;
        let inventory = Inventory {
            id: OfferId {
                clip: ClipId {
                    origin: DeviceId([0; 32]),
                    seq: 0,
                },
                generation: self.change_count(),
            },
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
        let source = content::Deferred::new(inventory.clone(), Arc::new(NoRepresentation)).unwrap();
        Ok(oink_clipboard::CapturedClipboard {
            snapshot: ClipSnapshot {
                items: content::items(&inventory),
                sensitivity: Sensitivity::Normal,
                change_count: self.change_count(),
            },
            content: Some(source),
        })
    }
    fn write(&self, items: &[ClipData], sensitivity: Sensitivity) -> Result<u64, ClipboardError> {
        self.clipboard.write(items, sensitivity)
    }
    fn watch(&self, handler: ChangeHandler) -> Result<(), ClipboardError> {
        self.clipboard.watch(handler)
    }
}
struct NoRepresentation;
impl content::Translator for NoRepresentation {
    fn standard(&self, _: &oink_clipboard::model::Representation, _: &[u8]) -> std::io::Result<Vec<ClipData>> {
        panic!("promise publication must not read ordinary representation data")
    }
}
struct NativeBoundary {
    clipboard: Arc<FakeClipboard>,
    published: Mutex<Option<oneshot::Sender<oink_clipboard::NativeSpec>>>,
}
impl ClipboardBackend for NativeBoundary {
    fn change_count(&self) -> u64 {
        self.clipboard.change_count()
    }
    fn read(&self) -> Result<ClipSnapshot, ClipboardError> {
        self.clipboard.read()
    }
    fn capture(&self, data: &std::path::Path) -> Result<oink_clipboard::CapturedClipboard, ClipboardError> {
        self.clipboard.capture(data)
    }
    fn write(&self, items: &[ClipData], sensitivity: Sensitivity) -> Result<u64, ClipboardError> {
        self.clipboard.write(items, sensitivity)
    }
    fn watch(&self, handler: ChangeHandler) -> Result<(), ClipboardError> {
        self.clipboard.watch(handler)
    }
    fn write_native_guarded(
        &self,
        spec: oink_clipboard::NativeSpec,
        _: content::ReaderGuard,
    ) -> Result<u64, ClipboardError> {
        if spec.inventory.has_promises()
            && let Some(tx) = self.published.lock().unwrap().take()
        {
            let _ = tx.send(spec);
        }
        Err(ClipboardError::Unsupported)
    }
}
#[test]
fn promise_clipboard_reaches_native_file_publication_without_announced_names() {
    crate::test_watchdog::run(
        module_path!(),
        "promise_clipboard_reaches_native_file_publication_without_announced_names",
        Some(4),
        async {
            let root = tempfile::tempdir().unwrap();
            let source = Arc::new(FakeClipboard::new());
            let (tx, rx) = oneshot::channel();
            let a = start_with_clip(
                &oink_test_support::network_config(root.path().join("a"), "a", 0),
                &Displays::new("a"),
                Some(Arc::new(PromiseClipboard {
                    clipboard: source.clone(),
                })),
            )
            .await;
            let b = start_with_clip(
                &oink_test_support::network_config(root.path().join("b"), "b", 0),
                &Displays::new("b"),
                Some(Arc::new(NativeBoundary {
                    clipboard: Arc::new(FakeClipboard::new()),
                    published: Mutex::new(Some(tx)),
                })),
            )
            .await;
            common::pair(&a, &b).await;
            common::active(&[&a, &b]).await;
            source.copy(
                vec![ClipData::Text("trigger source observation".into())],
                Sensitivity::Normal,
            );
            let result = tokio::time::timeout(Duration::from_secs(5), rx).await;
            let namespace_available = result
                .as_ref()
                .ok()
                .and_then(|result| result.as_ref().ok())
                .is_some_and(|spec| spec.contents.namespace().is_some());
            a.shutdown().await;
            b.shutdown().await;
            let spec = result
                .expect("promise capability was rejected before the native publication boundary")
                .unwrap();
            assert!(
                spec.inventory.roots.is_empty(),
                "private promise containers are not native file roots"
            );
            assert!(
                spec.file_contents.is_some(),
                "native publication needs the unresolved file capability"
            );
            assert!(namespace_available);
        },
    );
}
