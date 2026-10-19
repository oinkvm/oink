//! File diagnostics must describe file offers, never ordinary clipboard readers.
use super::*;
use crate::api::{AgentApi, ApiEvent, LiveViews};
use crate::snapshot_tests::{Displays, common, start_with_clip};
use oink_clipboard::{ChangeHandler, ClipData, ClipSnapshot, ClipboardError, Sensitivity};
use oink_test_support::clipboard::FakeClipboard;

#[test]
fn rich_text_readers_do_not_create_a_waiting_file_transfer() {
    crate::test_watchdog::run(
        module_path!(),
        "rich_text_readers_do_not_create_a_waiting_file_transfer",
        Some(4),
        async {
            let root = tempfile::tempdir().unwrap();
            let source = Arc::new(FakeClipboard::new());
            let target = Arc::new(FakeClipboard::new());
            let a = start_with_clip(
                &oink_test_support::network_config(root.path().join("a"), "a", 0),
                &Displays::new("a"),
                Some(source.clone()),
            )
            .await;
            let b = start_with_clip(
                &oink_test_support::network_config(root.path().join("b"), "b", 0),
                &Displays::new("b"),
                Some(target.clone()),
            )
            .await;
            common::pair(&a, &b).await;
            common::active(&[&a, &b]).await;
            let statuses = Arc::new(Mutex::new(Vec::new()));
            let events = statuses.clone();
            let subscription = b.subscribe_views(
                Arc::new(move |event| {
                    if let ApiEvent::FileTransfer(Some(status)) = event {
                        events.lock().unwrap().push(status.clone());
                    }
                }),
                LiveViews::default(),
            );
            let items = vec![
                ClipData::Html("<p>clipboard reader</p>".repeat(40)),
                ClipData::Text("clipboard reader".into()),
                ClipData::Custom {
                    platform: if cfg!(target_os = "macos") {
                        oink_foundation::clipboard::ClipPlatform::Mac
                    } else {
                        oink_foundation::clipboard::ClipPlatform::Windows
                    },
                    name: "org.oink.test.native".into(),
                    bytes: vec![0x5a; 1903],
                },
            ];
            source.copy(items.clone(), Sensitivity::Normal);
            common::until("rich text published and recorded", || {
                target.items() == items && !b.clipboard_activity().unwrap().is_empty()
            })
            .await;
            let handle = b.inner.clip.get().unwrap();
            handle.settle().await;
            assert!(
                b.clipboard_transfer().unwrap().is_none(),
                "native representation reads are not a pending file offer: {:?}",
                b.clipboard_transfer().unwrap()
            );
            assert!(statuses.lock().unwrap().is_empty());
            b.fetch_clipboard_files().unwrap();
            handle.settle().await;
            assert!(b.clipboard_transfer().unwrap().is_none());
            assert_eq!(
                handle
                    .shared
                    .native_target
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .1
                    .progress()
                    .borrow()
                    .phase,
                content::TransferPhase::Offered,
                "the file-only CLI must not start an empty delivery for rich text"
            );
            b.unsubscribe(subscription);
            a.shutdown().await;
            b.shutdown().await;
        },
    );
}

#[test]
fn file_offer_has_initial_items_and_late_progress_cannot_restore_a_retired_offer() {
    crate::test_watchdog::run(
        module_path!(),
        "file_offer_has_initial_items_and_late_progress_cannot_restore_a_retired_offer",
        Some(4),
        async {
            let root = tempfile::tempdir().unwrap();
            let source = Arc::new(FakeClipboard::new());
            let target = Arc::new(FakeClipboard::new());
            let a = start_with_clip(
                &oink_test_support::network_config(root.path().join("a"), "a", 0),
                &Displays::new("a"),
                Some(source.clone()),
            )
            .await;
            let b = start_with_clip(
                &oink_test_support::network_config(root.path().join("b"), "b", 0),
                &Displays::new("b"),
                Some(target.clone()),
            )
            .await;
            common::pair(&a, &b).await;
            common::active(&[&a, &b]).await;
            let statuses = Arc::new(Mutex::new(Vec::new()));
            let events = statuses.clone();
            let subscription = b.subscribe_views(
                Arc::new(move |event| {
                    if let ApiEvent::FileTransfer(Some(status)) = event {
                        events.lock().unwrap().push(status.clone());
                    }
                }),
                LiveViews::default(),
            );
            let file = a.data_dir().join("source.bin");
            std::fs::write(&file, [0x5a; 1024]).unwrap();
            let files = vec![ClipData::Files(vec![oink_clipboard::FileEntry {
                path: "source.bin".into(),
                is_dir: false,
                size: 1024,
                modified_ms: None,
                local_path: Some(file),
            }])];
            source.copy(files.clone(), Sensitivity::Normal);
            common::until("file provider refusal recorded", || {
                statuses.lock().unwrap().iter().any(|s| s.message.is_some())
            })
            .await;
            let handle = b.inner.clip.get().unwrap();
            handle.settle().await;
            let status = b.clipboard_transfer().unwrap().unwrap();
            assert_eq!(status.phase, FileTransferPhase::Pending);
            assert_eq!(status.files, 1);
            assert!(statuses.lock().unwrap().iter().all(|s| s.files > 0));
            let (id, batch, progress) = {
                let current = handle.shared.native_target.lock().unwrap();
                let (id, remote) = current.as_ref().unwrap();
                (*id, remote.batch().id(), remote.progress().borrow().clone())
            };
            target.copy(vec![ClipData::Text("local replacement".into())], Sensitivity::Normal);
            common::until("local copy retires file offer", || {
                b.clipboard_transfer().unwrap().is_none()
            })
            .await;
            handle.tx.send(Input::Progress(id, batch, progress)).unwrap();
            handle.settle().await;
            assert!(
                b.clipboard_transfer().unwrap().is_none(),
                "queued progress re-admitted a retired offer"
            );

            // A new remote ordinary copy must also replace the previous file
            // status even though its own native readers report no file progress.
            common::until("local copy reaches source", || {
                source.items() == vec![ClipData::Text("local replacement".into())]
            })
            .await;
            source.copy(files, Sensitivity::Normal);
            common::until("second file offer", || b.clipboard_transfer().unwrap().is_some()).await;
            source.copy(vec![ClipData::Text("remote replacement".into())], Sensitivity::Normal);
            common::until("remote ordinary copy replaces file offer", || {
                target.items() == vec![ClipData::Text("remote replacement".into())]
            })
            .await;
            handle.settle().await;
            assert!(b.clipboard_transfer().unwrap().is_none());
            b.unsubscribe(subscription);
            a.shutdown().await;
            b.shutdown().await;
        },
    );
}

struct GatedFilePublication {
    clipboard: Arc<FakeClipboard>,
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl ClipboardBackend for GatedFilePublication {
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
        guard: content::ReaderGuard,
    ) -> Result<u64, ClipboardError> {
        if !spec.files.is_empty() {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            self.release.lock().unwrap().recv().unwrap();
        }
        self.clipboard.write_native_guarded(spec, guard)
    }
}

#[test]
fn delayed_native_publication_cannot_reset_completed_delivery_to_waiting() {
    crate::test_watchdog::run(
        module_path!(),
        "delayed_native_publication_cannot_reset_completed_delivery_to_waiting",
        Some(4),
        async {
            let root = tempfile::tempdir().unwrap();
            let source = Arc::new(FakeClipboard::new());
            let target = Arc::new(FakeClipboard::new());
            target.set_virtual_files(true);
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
                Some(Arc::new(GatedFilePublication {
                    clipboard: target,
                    entered: Mutex::new(Some(entered)),
                    release: Mutex::new(gate),
                })),
            )
            .await;
            common::pair(&a, &b).await;
            common::active(&[&a, &b]).await;
            let file = a.data_dir().join("source.bin");
            std::fs::write(&file, [0x5a; 1024]).unwrap();
            source.copy(
                vec![ClipData::Files(vec![oink_clipboard::FileEntry {
                    path: "source.bin".into(),
                    is_dir: false,
                    size: 1024,
                    modified_ms: None,
                    local_path: Some(file),
                }])],
                Sensitivity::Normal,
            );
            tokio::time::timeout(Duration::from_secs(5), preparing)
                .await
                .unwrap()
                .unwrap();
            let handle = b.inner.clip.get().unwrap();
            let remote = handle.shared.native_target.lock().unwrap().as_ref().unwrap().1.clone();
            assert!(remote.start_delivery());
            common::until("delivery completes before native publication returns", || {
                b.clipboard_transfer()
                    .unwrap()
                    .is_some_and(|s| s.phase == FileTransferPhase::Ready)
            })
            .await;
            release.send(()).unwrap();
            common::until("native publication result applied", || {
                !b.clipboard_activity().unwrap().is_empty()
            })
            .await;
            handle.settle().await;
            let status = b.clipboard_transfer().unwrap().unwrap();
            assert_eq!(status.phase, FileTransferPhase::Ready);
            assert_eq!((status.files_done, status.files), (1, 1));
            assert_eq!((status.done, status.total), (1024, 1024));
            a.shutdown().await;
            b.shutdown().await;
        },
    );
}
