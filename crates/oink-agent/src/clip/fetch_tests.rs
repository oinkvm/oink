//! Provider refusal remains explicit, even when download was requested during publication.
use super::*;
use crate::snapshot_tests::{Displays, common, start_with_clip};
use oink_clipboard::{ChangeHandler, ClipData, ClipSnapshot, ClipboardError};
use oink_test_support::clipboard::FakeClipboard;

struct GatedProvider {
    clipboard: Arc<FakeClipboard>,
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    after_publish: bool,
}
impl ClipboardBackend for GatedProvider {
    fn change_count(&self) -> u64 {
        self.clipboard.change_count()
    }
    fn read(&self) -> Result<ClipSnapshot, ClipboardError> {
        self.clipboard.read()
    }
    fn capture(&self, data: &std::path::Path) -> Result<oink_clipboard::CapturedClipboard, ClipboardError> {
        self.clipboard.capture(data)
    }
    fn write(&self, items: &[ClipData], sensitivity: oink_clipboard::Sensitivity) -> Result<u64, ClipboardError> {
        self.clipboard.write(items, sensitivity)
    }
    fn watch(&self, handler: ChangeHandler) -> Result<(), ClipboardError> {
        self.clipboard.watch(handler)
    }
    fn write_native_guarded(
        &self,
        spec: oink_clipboard::NativeSpec,
        guard: content::ReaderGuard,
    ) -> Result<u64, ClipboardError> {
        if !self.after_publish && spec.file_contents.is_some() {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            self.release.lock().unwrap().recv().unwrap();
            return Err(ClipboardError::FileProviderUnavailable);
        }
        let result = self.clipboard.write_native_guarded(spec, guard);
        if self.after_publish
            && let Some(entered) = self.entered.lock().unwrap().take()
        {
            let _ = entered.send(());
            self.release.lock().unwrap().recv().unwrap();
        }
        result
    }
}
#[test]
fn file_download_during_preparation_does_not_bypass_provider_refusal() {
    crate::test_watchdog::run(
        module_path!(),
        "file_download_during_preparation_does_not_bypass_provider_refusal",
        Some(4),
        file_fetch_scenario(false, false),
    );
}

#[test]
fn file_download_behind_previous_publication_does_not_bypass_provider_refusal() {
    crate::test_watchdog::run(
        module_path!(),
        "file_download_behind_previous_publication_does_not_bypass_provider_refusal",
        Some(4),
        file_fetch_scenario(true, false),
    );
}

#[test]
fn provider_refusal_notifies_and_preserves_mixed_text() {
    crate::test_watchdog::run(
        module_path!(),
        "provider_refusal_notifies_and_preserves_mixed_text",
        Some(4),
        file_fetch_scenario(false, true),
    );
}

async fn file_fetch_scenario(after_publish: bool, mixed: bool) {
    use crate::api::AgentApi;
    let root = tempfile::tempdir().unwrap();
    let source = Arc::new(FakeClipboard::new());
    let target = Arc::new(FakeClipboard::new());
    let (entered, preparing) = oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let a = start_with_clip(
        &oink_test_support::network_config(root.path().join("a"), "a", 0),
        &Displays::new("a"),
        Some(source.clone()),
    )
    .await;
    let b = start_with_clip(
        &oink_test_support::network_config(root.path().join("b"), "b", 0),
        &Displays::new("b"),
        Some(Arc::new(GatedProvider {
            clipboard: target.clone(),
            entered: Mutex::new(Some(entered)),
            release: Mutex::new(gate),
            after_publish,
        })),
    )
    .await;
    let notices = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = notices.clone();
    let subscription = b.subscribe(Arc::new(move |event| {
        if matches!(event, crate::api::ApiEvent::FileProviderRequired) {
            observed.fetch_add(1, Ordering::SeqCst);
        }
    }));
    common::pair(&a, &b).await;
    common::active(&[&a, &b]).await;
    let mut preparing = Some(preparing);
    if after_publish {
        // The previous offer is already visible to the consumer, but
        // its native callback has not returned to the clipboard owner.
        source.copy(
            vec![ClipData::Text("previous offer".into())],
            oink_clipboard::Sensitivity::Normal,
        );
        tokio::time::timeout(Duration::from_secs(5), preparing.take().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(target.items(), vec![ClipData::Text("previous offer".into())]);
    }
    let file = a.data_dir().join("source.bin");
    let expected = vec![0x5a; 2_000_000];
    std::fs::write(&file, &expected).unwrap();
    let mut items = vec![ClipData::Files(vec![oink_clipboard::FileEntry {
        path: "传输.bin".into(),
        is_dir: false,
        size: expected.len() as u64,
        modified_ms: None,
        local_path: Some(file),
    }])];
    if mixed {
        items.push(ClipData::Text("unaffected text".into()));
    }
    source.copy(items, oink_clipboard::Sensitivity::Normal);
    if let Some(preparing) = preparing {
        tokio::time::timeout(Duration::from_secs(5), preparing)
            .await
            .unwrap()
            .unwrap();
    } else {
        common::until("new file offer queued behind previous publication", || {
            b.clipboard_transfer()
                .unwrap()
                .is_some_and(|transfer| transfer.phase == FileTransferPhase::Pending)
        })
        .await;
    }
    b.fetch_clipboard_files().unwrap();
    b.inner.clip.get().unwrap().settle().await;
    release.send(()).unwrap();
    common::until("native provider refusal reported without cache fallback", || {
        b.clipboard_transfer()
            .unwrap()
            .is_some_and(|transfer| transfer.phase == FileTransferPhase::Failed)
    })
    .await;
    assert!(!b.data_dir().join("completed-deliveries").exists());
    assert!(!matches!(target.items().as_slice(), [ClipData::Files(_)]));
    if cfg!(target_os = "macos") {
        common::until("enablement notice", || notices.load(Ordering::SeqCst) > 0).await;
        assert_eq!(notices.load(Ordering::SeqCst), 1);
    } else {
        assert_eq!(notices.load(Ordering::SeqCst), 0);
    }
    if mixed {
        assert!(target.items().contains(&ClipData::Text("unaffected text".into())));
    }
    b.unsubscribe(subscription);
    a.shutdown().await;
    b.shutdown().await;
}
