//! The clipboard follows this device's pause as its owner decided it, not the
//! later publication of that decision.
use super::*;
use crate::device_controls::{requests::wire_device, transactions::Requester};
use crate::node::lock;
use crate::snapshot_tests::{Displays, start_with_clip};
use oink_test_support::clipboard::FakeClipboard;

/// An emergency pauses the clipboard at once (`emit_emergency_notice`); its
/// publication follows only after the native safety fence. A group-state
/// change in between (another member's layout edit, a membership change)
/// re-read the still-unpaused publication and resumed clipboard sharing until
/// the publication landed.
#[test]
fn a_state_change_before_the_publication_keeps_an_emergency_clipboard_pause() {
    crate::test_watchdog::run(
        module_path!(),
        "a_state_change_before_the_publication_keeps_an_emergency_clipboard_pause",
        Some(4),
        async {
            let root = tempfile::tempdir().unwrap();
            let node = start_with_clip(
                &oink_test_support::network_config(root.path().join("a"), "a", 0),
                &Displays::new("a"),
                Some(Arc::new(FakeClipboard::new())),
            )
            .await;
            let clip = node.clipboard().unwrap().clone();
            assert!(!clip.shared.paused.load(Ordering::Acquire));
            // The owner's emergency decision and its direct clipboard pause, in
            // the order `originate_emergency` makes them. The publication is
            // written later, by the safety worker; here it is not written.
            let origin = Requester {
                device: node.id().into(),
                nonce: node.inner.boot.nonce,
            };
            lock(&node.inner.controls)
                .pause
                .emergency(origin, wire_device(origin.device), 1)
                .unwrap();
            clip.set_paused(true);
            assert!(
                !node.link_config().unwrap().applied().paused,
                "the emergency is not published yet"
            );
            // A group-state change handled before the publication.
            clip.config_state_changed(&node);
            assert!(
                clip.shared.paused.load(Ordering::Acquire),
                "a group-state change resumed the clipboard before the emergency was published"
            );
            node.shutdown().await;
        },
    );
}
