//! Actor health must reach both local and remote cards, including idle clipboards.
use super::*;
use crate::api::AgentApi;
use crate::device_cards::{CardAction, CardCondition, ResourceCard, ResourceFaultCode, SharingIcon, SharingMode};
use crate::node::Node;
use crate::snapshot_tests::{Displays, common, start_with_clip};
use oink_clipboard::{ChangeHandler, ClipData, ClipSnapshot, ClipboardError, Sensitivity};
use oink_test_support::clipboard::FakeClipboard;

#[derive(Default)]
struct CaptureBackend {
    clipboard: FakeClipboard,
    fail: AtomicBool,
}
impl ClipboardBackend for CaptureBackend {
    fn change_count(&self) -> u64 {
        self.clipboard.change_count()
    }
    fn read(&self) -> Result<ClipSnapshot, ClipboardError> {
        self.clipboard.read()
    }
    fn capture(&self, data: &std::path::Path) -> Result<oink_clipboard::CapturedClipboard, ClipboardError> {
        if self.fail.load(Ordering::Acquire) {
            Err(ClipboardError::Platform("synthetic capture failure".into()))
        } else {
            self.clipboard.capture(data)
        }
    }
    fn write(&self, items: &[ClipData], sensitivity: Sensitivity) -> Result<u64, ClipboardError> {
        self.clipboard.write(items, sensitivity)
    }
    fn watch(&self, handler: ChangeHandler) -> Result<(), ClipboardError> {
        self.clipboard.watch(handler)
    }
}

fn clipboard_card(observer: &Node, owner: &Node) -> Option<ResourceCard> {
    observer
        .device_cards()
        .unwrap()
        .cards
        .into_iter()
        .find(|card| card.device.0 == owner.id())?
        .status
        .map(|status| status.clipboard)
}
fn healthy(observer: &Node, owner: &Node, shared: bool) -> bool {
    clipboard_card(observer, owner).is_some_and(|card| {
        card.fault.is_some_and(|fault| fault.code == ResourceFaultCode::None)
            && card.condition == CardCondition::Ready
            && card.icon
                == if shared {
                    SharingIcon::Shared
                } else {
                    SharingIcon::Local
                }
            && card.configured_mode
                == Some(if shared {
                    SharingMode::Shared
                } else {
                    SharingMode::Local
                })
            && matches!(card.action, CardAction::Invoke { .. })
    })
}

#[test]
fn clipboard_health_reaches_cards_before_offers_and_after_capture_recovery() {
    crate::test_watchdog::run(
        module_path!(),
        "clipboard_health_reaches_cards_before_offers_and_after_capture_recovery",
        Some(4),
        async {
            let root = tempfile::tempdir().unwrap();
            let source = Arc::new(CaptureBackend::default());
            let a = start_with_clip(
                &oink_test_support::network_config(root.path().join("a"), "a", 0),
                &Displays::new("a"),
                Some(source.clone()),
            )
            .await;
            common::until("healthy idle clipboard without a peer or content", || {
                healthy(&a, &a, true)
            })
            .await;
            assert!(a.clipboard_activity().unwrap().is_empty());

            let b = start_with_clip(
                &oink_test_support::network_config(root.path().join("b"), "b", 0),
                &Displays::new("b"),
                Some(Arc::new(FakeClipboard::new())),
            )
            .await;
            common::pair(&a, &b).await;
            common::until("healthy clipboard cards on both observers", || {
                [&a, &b]
                    .iter()
                    .all(|observer| [&a, &b].iter().all(|owner| healthy(observer, owner, true)))
            })
            .await;

            // Exercise the real owner action unlocked by the healthy observation.
            for enabled in [false, true] {
                let CardAction::Invoke { token, .. } = clipboard_card(&b, &a).unwrap().action else {
                    panic!("healthy remote clipboard must be controllable");
                };
                b.invoke_device_card_action(crate::device_controls::runtime::fresh_request_id(), token)
                    .unwrap();
                common::until("clipboard owner option and health propagated", || {
                    a.settings().clipboard_enabled == enabled && healthy(&a, &a, enabled) && healthy(&b, &a, enabled)
                })
                .await;
            }

            source.fail.store(true, Ordering::Release);
            source
                .clipboard
                .copy(vec![ClipData::Text("failed copy".into())], Sensitivity::Normal);
            common::until("capture failure remains a typed fault on both observers", || {
                [&a, &b].iter().all(|observer| {
                    clipboard_card(observer, &a).is_some_and(|card| {
                        card.fault
                            .is_some_and(|fault| fault.code == ResourceFaultCode::BackendFailed)
                            && card.icon == SharingIcon::Fault
                    })
                })
            })
            .await;
            source.fail.store(false, Ordering::Release);
            source.clipboard.copy(Vec::new(), Sensitivity::Normal);
            common::until("empty successful capture restores explicit health", || {
                healthy(&a, &a, true) && healthy(&b, &a, true)
            })
            .await;
            a.shutdown().await;
            b.shutdown().await;
        },
    );
}

#[test]
fn content_and_superseded_capture_errors_leave_clipboard_usable() {
    crate::test_watchdog::run(
        module_path!(),
        "content_and_superseded_capture_errors_leave_clipboard_usable",
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
            source.copy(vec![ClipData::Text("first copy".into())], Sensitivity::Normal);
            common::until("initial copy published", || target.items() == source.items()).await;
            let handle = a.clipboard().unwrap();

            for error in [
                ClipboardError::ContentUnavailable(Unavailable::NoSuchItem),
                ClipboardError::ContentUnavailable(Unavailable::Permission),
                ClipboardError::ContentUnavailable(Unavailable::InvalidData),
            ] {
                handle
                    .tx
                    .send(Input::Captured(source.change_count(), Err(error)))
                    .unwrap();
                handle.settle().await;
                assert!(
                    healthy(&a, &a, true),
                    "unreadable copy must not fault the native service"
                );
            }
            // A failed worker for the previous slot cannot poison the current
            // one, including when our own remote write replaced that slot.
            handle
                .tx
                .send(Input::Captured(
                    source.change_count() - 1,
                    Err(ClipboardError::Platform("superseded capture".into())),
                ))
                .unwrap();
            handle.settle().await;
            assert!(healthy(&a, &a, true));
            common::until("healthy clipboard propagated to the peer", || healthy(&b, &a, true)).await;

            let received = vec![ClipData::Text("remote copy still works".into())];
            target.copy(received.clone(), Sensitivity::Normal);
            common::until("remote clipboard received after unreadable content", || {
                source.items() == received
            })
            .await;
            assert!(healthy(&a, &a, true));
            source.copy(vec![ClipData::Text("next local copy".into())], Sensitivity::Normal);
            common::until("next local clipboard still propagates", || {
                target.items() == source.items()
            })
            .await;
            assert!(healthy(&a, &a, true));
            assert!(healthy(&b, &a, true));
            a.shutdown().await;
            b.shutdown().await;
        },
    );
}
