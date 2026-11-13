//! Clipboard duplicate suppression (`clipboard_dedupe`) over two real nodes.
//! It is sending-side only; received offers are always written. A synchronizer
//! rewriting a device's clipboard is modelled by `FakeClipboard::copy`, which
//! looks exactly like a local copy.
use super::*;
use oink_clipboard::ClipboardBackend;

fn text(value: &str) -> Vec<ClipData> {
    vec![ClipData::Text(value.into())]
}

async fn quiet() {
    // Capture quiet window (250 ms), capture, offer delivery and write.
    tokio::time::sleep(Duration::from_millis(800)).await;
}

/// "Clipboard deduplication" (device-local setting, default on).
async fn set_dedupe(dev: &Dev, on: bool) {
    let mut settings = dev.node.clipboard_settings().unwrap();
    settings.dedupe = on;
    dev.node.set_clipboard_settings(settings.into()).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[test]
fn deduplication_on_suppresses_duplicates_on_the_sending_side() {
    crate::test_watchdog::run(
        module_path!(),
        "deduplication_on_suppresses_duplicates_on_the_sending_side",
        Some(4),
        async {
            let a = start("dedupe-a", 0).await;
            let b = start("dedupe-b", 0).await;
            // Like AppKit/OLE capture: sizes unknown until read, so A's own
            // offer can be rebound to a rewritten clipboard (see below).
            a.clip.set_unknown_representation_sizes(true);
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            assert!(b.node.clipboard_settings().unwrap().dedupe, "on by default");

            a.clip.copy(text("echo me\n"), Sensitivity::Normal);
            wait_for("text on B", 5.0, || b.clip.items() == text("echo me\n")).await;
            quiet().await;
            let a_count = a.clip.change_count();
            // A synchronizer on B writes the same text back with CRLF: a
            // format subset of what Oink wrote, not announced.
            b.clip.copy(text("echo me\r\n"), Sensitivity::Normal);
            quiet().await;
            assert_eq!(a.clip.change_count(), a_count, "B must not announce the duplicate");

            // A text prefix is a new copy.
            b.clip.copy(text("echo"), Sensitivity::Normal);
            wait_for("prefix on A", 5.0, || a.clip.items() == text("echo")).await;
            quiet().await;
            // The same device copying the same content again: sent once only.
            let a_count = a.clip.change_count();
            b.clip.copy(text("echo"), Sensitivity::Normal);
            quiet().await;
            assert_eq!(a.clip.change_count(), a_count, "a repeated copy is not announced");

            // A synchronizer writing A's own copy back to A (two Windows
            // devices with a vendor sync tool): not announced.
            a.clip.copy(text("from A"), Sensitivity::Normal);
            wait_for("A's copy on B", 5.0, || b.clip.items() == text("from A")).await;
            quiet().await;
            let b_count = b.clip.change_count();
            a.clip.copy(text("from A\r\n"), Sensitivity::Normal);
            quiet().await;
            assert_eq!(b.clip.change_count(), b_count, "the write-back to A is not announced");
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

#[test]
fn deduplication_off_announces_every_change() {
    crate::test_watchdog::run(
        module_path!(),
        "deduplication_off_announces_every_change",
        Some(4),
        async {
            let a = start("nodedupe-a", 0).await;
            let b = start("nodedupe-b", 0).await;
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            set_dedupe(&a, false).await;
            set_dedupe(&b, false).await;
            a.clip.copy(text("twice"), Sensitivity::Normal);
            wait_for("first copy on B", 5.0, || b.clip.items() == text("twice")).await;
            quiet().await;
            let b_count = b.clip.change_count();
            a.clip.copy(text("twice"), Sensitivity::Normal);
            wait_for("second copy on B", 5.0, || b.clip.change_count() > b_count).await;
            quiet().await;
            let a_count = a.clip.change_count();
            b.clip.copy(text("twice\r\n"), Sensitivity::Normal);
            wait_for("B's rewrite on A", 5.0, || a.clip.change_count() > a_count).await;
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

#[test]
fn received_offers_are_always_written() {
    crate::test_watchdog::run(module_path!(), "received_offers_are_always_written", Some(4), async {
        let a = start("held-a", 0).await;
        let b = start("held-b", 0).await;
        common::pair(&a.node, &b.node).await;
        common::active(&[&a.node, &b.node]).await;
        // B announces its rewrite of A's content (deduplication off on B);
        // A already holds that content and writes it anyway.
        set_dedupe(&b, false).await;
        a.clip.copy(text("both have this"), Sensitivity::Normal);
        wait_for("text on B", 5.0, || b.clip.items() == text("both have this")).await;
        quiet().await;
        let a_count = a.clip.change_count();
        b.clip.copy(text("both have this\r\n"), Sensitivity::Normal);
        wait_for("B's copy written on A", 5.0, || a.clip.change_count() > a_count).await;
        a.node.shutdown().await;
        b.node.shutdown().await;
    });
}

/// The previous content Oink wrote is known from the offer's digests: deciding
/// a duplicate never reads the lazily written clipboard (that would fetch from
/// the source device, which is gone here).
#[test]
fn oink_written_content_is_compared_by_offer_digests() {
    crate::test_watchdog::run(
        module_path!(),
        "oink_written_content_is_compared_by_offer_digests",
        Some(4),
        async {
            let a = start("digest-a", 0).await;
            let b = start("digest-b", 0).await;
            b.clip.set_deferred_native(true);
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            a.clip.copy(text("lazily written"), Sensitivity::Normal);
            wait_for("lazy offer on B", 5.0, || b.clip.native_spec().is_some()).await;
            quiet().await;
            a.node.shutdown().await;
            let published = b.node.clipboard_activity().unwrap().len();
            b.clip.copy(text("lazily written\r\n"), Sensitivity::Normal);
            quiet().await;
            assert_eq!(
                b.node.clipboard_activity().unwrap().len(),
                published,
                "a duplicate of the offer's content is not announced"
            );
            b.clip.copy(text("something else"), Sensitivity::Normal);
            wait_for("new content announced", 5.0, || {
                b.node.clipboard_activity().unwrap().len() > published
            })
            .await;
            b.node.shutdown().await;
        },
    );
}

/// Universal Clipboard (macOS) data is promised by the other Apple device:
/// it is never read to compare, so it is not a duplicate by content (only the
/// Oink marker, below, recognises a write-back).
#[cfg(target_os = "macos")]
#[test]
fn universal_clipboard_content_is_never_read_to_compare() {
    crate::test_watchdog::run(
        module_path!(),
        "universal_clipboard_content_is_never_read_to_compare",
        Some(4),
        async {
            let a = start("uc-a", 0).await;
            let b = start("uc-b", 0).await;
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            a.clip.copy(text("from this mac"), Sensitivity::Normal);
            wait_for("text on B", 5.0, || b.clip.items() == text("from this mac")).await;
            quiet().await;
            let b_count = b.clip.change_count();
            a.clip.copy(
                vec![
                    ClipData::Text("from this mac".into()),
                    ClipData::Custom {
                        platform: oink_foundation::clipboard::ClipPlatform::Mac,
                        name: oink_clipboard::dedupe::UNIVERSAL_CLIPBOARD.into(),
                        bytes: vec![],
                    },
                ],
                Sensitivity::Normal,
            );
            wait_for("unread UC content announced", 5.0, || b.clip.change_count() > b_count).await;
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

/// The Oink clip marker: a synchronizer that keeps private formats brings back
/// the marker of the clip Oink wrote. That identifies the write-back without
/// any content comparison or time window; a synchronizer that drops it falls
/// back to the content rules. The marker never appears in an announced offer.
#[test]
fn oink_clip_marker_identifies_a_write_back() {
    crate::test_watchdog::run(
        module_path!(),
        "oink_clip_marker_identifies_a_write_back",
        Some(4),
        async {
            let a = start("marker-a", 0).await;
            let b = start("marker-b", 0).await;
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;

            a.clip.copy(text("marked"), Sensitivity::Normal);
            wait_for("text on B", 5.0, || b.clip.items() == text("marked")).await;
            quiet().await;
            let marker = b.clip.marker().expect("Oink writes carry the clip marker");
            let a_count = a.clip.change_count();
            // Other content, and deduplication off: only the marker can
            // recognise this as B's write coming back.
            set_dedupe(&b, false).await;
            b.clip
                .copy_with_marker(text("rewritten by a synchronizer"), Some(marker), Sensitivity::Normal);
            quiet().await;
            assert_eq!(a.clip.change_count(), a_count, "a known marker is not announced");

            // An unknown marker is ignored: content rules apply, and the
            // marker is not part of what B announces.
            let mut bogus = oink_clipboard::dedupe::marker(oink_clipboard::model::OfferId {
                clip: oink_foundation::content::ClipId {
                    origin: oink_foundation::DeviceId([9; 32]),
                    seq: 1,
                },
                generation: 1,
            })
            .unwrap();
            bogus[40] ^= 1;
            b.clip.copy_with_marker(text("fresh"), Some(bogus), Sensitivity::Normal);
            wait_for("fresh text on A", 5.0, || a.clip.change_count() > a_count).await;
            assert_eq!(a.clip.items(), text("fresh"), "no marker format reaches the peer");
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

/// The marker of our own announced offer coming back (Universal Clipboard
/// pushing another Mac's Oink write to this Mac) rebinds the offer, so peers
/// read the newest content.
#[test]
fn own_offer_marker_coming_back_rebinds_the_offer() {
    crate::test_watchdog::run(
        module_path!(),
        "own_offer_marker_coming_back_rebinds_the_offer",
        Some(4),
        async {
            use std::io::Read;
            let a = start("rebind-a", 0).await;
            let b = start("rebind-b", 0).await;
            b.clip.set_deferred_native(true);
            a.clip.set_unknown_representation_sizes(true);
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            a.clip.copy(text("own offer"), Sensitivity::Normal);
            wait_for("lazy offer on B", 5.0, || b.clip.native_spec().is_some()).await;
            quiet().await;
            let spec = b.clip.native_spec().unwrap();
            let marker = oink_clipboard::dedupe::marker(spec.inventory.id).unwrap();
            let b_count = b.clip.change_count();
            a.clip
                .copy_with_marker(text("newer content"), Some(marker), Sensitivity::Normal);
            quiet().await;
            assert_eq!(b.clip.change_count(), b_count, "the write-back is not announced");
            let rep = spec.inventory.items[0].representations[0].id;
            let bytes = tokio::task::spawn_blocking(move || {
                let mut bytes = Vec::new();
                spec.contents.open(rep)?.read_to_end(&mut bytes)?;
                Ok::<_, std::io::Error>(bytes)
            })
            .await
            .unwrap()
            .expect("the announced offer is rebound");
            assert_eq!(bytes, b"newer content");
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

async fn set_paused(dev: &Dev, paused: bool) {
    use crate::api::AgentApi;
    api(&dev.node, move |node| {
        node.set_local_paused(crate::device_controls::runtime::fresh_request_id(), paused)
            .unwrap()
    })
    .await;
    wait_for("local pause applied", 5.0, || dev.node.fully_paused() == paused).await;
}

/// Pausing and resuming sharing (or turning sending off and on) does not make
/// the unchanged clipboard a new copy: the same offer is published again and
/// peers already hold it.
#[test]
fn pause_and_resume_republish_the_unchanged_offer() {
    crate::test_watchdog::run(
        module_path!(),
        "pause_and_resume_republish_the_unchanged_offer",
        Some(4),
        async {
            let a = start("resume-a", 0).await;
            let b = start("resume-b", 0).await;
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            a.clip.copy(text("copied once"), Sensitivity::Normal);
            wait_for("text on B", 5.0, || b.clip.items() == text("copied once")).await;
            quiet().await;
            let b_count = b.clip.change_count();
            let published = a.node.clipboard_activity().unwrap().len();
            for _ in 0..3 {
                set_paused(&a, true).await;
                set_paused(&a, false).await;
                quiet().await;
            }
            let mut settings = a.node.clipboard_settings().unwrap();
            settings.send = false;
            a.node.set_clipboard_settings(settings.into()).unwrap();
            quiet().await;
            settings.send = true;
            a.node.set_clipboard_settings(settings.into()).unwrap();
            quiet().await;
            assert_eq!(b.clip.change_count(), b_count, "B received the same copy again");
            assert_eq!(
                a.node.clipboard_activity().unwrap().len(),
                published,
                "A announced the unchanged clipboard as a new copy"
            );
            // Both directions still work after resuming.
            b.clip.copy(text("from B"), Sensitivity::Normal);
            wait_for("B's copy on A", 5.0, || a.clip.items() == text("from B")).await;
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

/// A peer's copy made while this device was paused stays the group's current
/// content when this device resumes with its older, unchanged clipboard.
#[test]
fn resuming_keeps_a_newer_copy_made_while_paused() {
    crate::test_watchdog::run(
        module_path!(),
        "resuming_keeps_a_newer_copy_made_while_paused",
        Some(4),
        async {
            let first = start("newer-1", 0).await;
            let second = start("newer-2", 0).await;
            // Ordering clock ties go to the larger device id: let the resuming
            // device be that one, so a re-announcement with a fresh clock
            // would always win against the copy made during the pause.
            let (a, b) = if first.node.id() > second.node.id() {
                (first, second)
            } else {
                (second, first)
            };
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            a.clip.copy(text("older"), Sensitivity::Normal);
            wait_for("older on B", 5.0, || b.clip.items() == text("older")).await;
            quiet().await;
            set_paused(&a, true).await;
            b.clip.copy(text("newer"), Sensitivity::Normal);
            quiet().await;
            set_paused(&a, false).await;
            // B's B0 to A was refused while A was paused; it reopens on its
            // retry schedule (at most 5 s) and brings B's current offer.
            wait_for("newer on A", 10.0, || a.clip.items() == text("newer")).await;
            quiet().await;
            assert_eq!(
                b.clip.items(),
                text("newer"),
                "the resumed device overwrote a newer copy"
            );
            assert_eq!(a.clip.items(), text("newer"));
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

/// Changing a setting that is read where it is used ("clipboard
/// deduplication") does not withdraw the clipboard or cancel transfers.
#[test]
fn deduplication_toggle_keeps_the_pending_file_offer() {
    crate::test_watchdog::run(
        module_path!(),
        "deduplication_toggle_keeps_the_pending_file_offer",
        Some(4),
        async {
            let a = start("toggle-a", 0).await;
            let b = start("toggle-b", 0).await;
            b.clip.set_virtual_files(true);
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            let source = a.node.data_dir().join("pending.bin");
            let bytes = vec![0x5a; oink_clipboard::model::AUTO_PULL_BYTES as usize + 1];
            std::fs::write(&source, &bytes).unwrap();
            a.clip.copy(
                vec![ClipData::Files(vec![oink_clipboard::FileEntry {
                    path: "pending.bin".into(),
                    size: bytes.len() as u64,
                    is_dir: false,
                    modified_ms: None,
                    local_path: Some(source),
                }])],
                Sensitivity::Normal,
            );
            wait_for("pending file offer on B", 5.0, || {
                b.node
                    .clipboard_transfer()
                    .unwrap()
                    .is_some_and(|p| p.phase == crate::api::FileTransferPhase::Pending)
            })
            .await;
            let pending = b.node.clipboard_transfer().unwrap().unwrap();
            let b_count = b.clip.change_count();
            set_dedupe(&b, false).await;
            set_dedupe(&a, false).await;
            quiet().await;
            assert_eq!(
                b.node.clipboard_transfer().unwrap().map(|p| p.id),
                Some(pending.id),
                "a deduplication toggle withdrew the file offer"
            );
            assert_eq!(b.clip.change_count(), b_count, "the offer was announced again");
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

/// Re-selecting an older entry in a clipboard manager brings back the Oink
/// marker of an earlier clip. Without Universal Clipboard content that is a
/// new copy and is announced; only a real UC write-back is recognised.
#[test]
fn earlier_clip_marker_without_universal_clipboard_is_a_new_copy() {
    crate::test_watchdog::run(
        module_path!(),
        "earlier_clip_marker_without_universal_clipboard_is_a_new_copy",
        Some(4),
        async {
            let a = start("history-a", 0).await;
            let b = start("history-b", 0).await;
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            b.clip.copy(text("old"), Sensitivity::Normal);
            wait_for("old on A", 5.0, || a.clip.items() == text("old")).await;
            quiet().await;
            let old = a.clip.marker().expect("Oink writes carry the clip marker");
            a.clip.copy(text("new"), Sensitivity::Normal);
            wait_for("new on B", 5.0, || b.clip.items() == text("new")).await;
            quiet().await;
            #[cfg(target_os = "macos")]
            {
                // Universal Clipboard bringing back the earlier write: echo.
                let b_count = b.clip.change_count();
                a.clip.copy_with_marker(
                    vec![
                        ClipData::Text("old".into()),
                        ClipData::Custom {
                            platform: oink_foundation::clipboard::ClipPlatform::Mac,
                            name: oink_clipboard::dedupe::UNIVERSAL_CLIPBOARD.into(),
                            bytes: vec![],
                        },
                    ],
                    Some(old.clone()),
                    Sensitivity::Normal,
                );
                quiet().await;
                assert_eq!(b.clip.change_count(), b_count, "a UC write-back is not announced");
                a.clip.copy(text("new again"), Sensitivity::Normal);
                wait_for("new again on B", 5.0, || b.clip.items() == text("new again")).await;
                quiet().await;
            }
            // The clipboard manager re-selects "old" with its private formats.
            a.clip.copy_with_marker(text("old"), Some(old), Sensitivity::Normal);
            wait_for("re-selected entry on B", 5.0, || b.clip.items() == text("old")).await;
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

/// After the group moved on to a newer offer, copying the earlier content
/// again is a new copy even though the native clipboard still holds it.
#[test]
fn repeated_copy_after_a_newer_group_offer_is_announced() {
    crate::test_watchdog::run(
        module_path!(),
        "repeated_copy_after_a_newer_group_offer_is_announced",
        Some(4),
        async {
            let a = start("rebase-a", 0).await;
            let b = start("rebase-b", 0).await;
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            // (b) B accepts A's newer offer, but writing it fails (files and
            // no file provider): B's clipboard still holds X.
            b.clip.copy(text("X"), Sensitivity::Normal);
            wait_for("X on A", 5.0, || a.clip.items() == text("X")).await;
            quiet().await;
            let source = a.node.data_dir().join("newer.bin");
            std::fs::write(&source, b"newer").unwrap();
            a.clip.copy(
                vec![ClipData::Files(vec![oink_clipboard::FileEntry {
                    path: "newer.bin".into(),
                    size: 5,
                    is_dir: false,
                    modified_ms: None,
                    local_path: Some(source),
                }])],
                Sensitivity::Normal,
            );
            quiet().await;
            assert_eq!(b.clip.items(), text("X"), "the file offer could not be written");
            b.clip.copy(text("X"), Sensitivity::Normal);
            wait_for("X again on A", 5.0, || a.clip.items() == text("X")).await;
            quiet().await;

            // (a) B only sends: A's newer copy is dropped by B.
            let mut settings = b.node.clipboard_settings().unwrap();
            settings.receive = false;
            b.node.set_clipboard_settings(settings.into()).unwrap();
            b.clip.copy(text("Z"), Sensitivity::Normal);
            wait_for("Z on A", 5.0, || a.clip.items() == text("Z")).await;
            quiet().await;
            a.clip.copy(text("Y"), Sensitivity::Normal);
            quiet().await;
            assert_eq!(b.clip.items(), text("Z"));
            b.clip.copy(text("Z"), Sensitivity::Normal);
            wait_for("Z again on A", 5.0, || a.clip.items() == text("Z")).await;
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}
