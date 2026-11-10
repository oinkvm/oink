//! Clipboard duplicate suppression (`clipboard_dedupe`, oinkvm/oink#35) over
//! two real nodes. It is sending-side only; received offers are always
//! written. A synchronizer rewriting a device's clipboard is modelled by
//! `FakeClipboard::copy` (private formats dropped) or `copy_with_marker`
//! (private formats kept), both indistinguishable from a local copy.
use super::*;
use oink_clipboard::ClipboardBackend;

fn text(value: &str) -> Vec<ClipData> {
    vec![ClipData::Text(value.into())]
}

async fn quiet() {
    // Capture quiet window (75 ms), capture, offer delivery and write.
    tokio::time::sleep(Duration::from_millis(800)).await;
}

/// "Clipboard deduplication" (device-local setting, default on).
async fn set_dedupe(dev: &Dev, on: bool) {
    let mut settings = dev.node.clipboard_settings().unwrap();
    settings.dedupe = on;
    dev.node.set_clipboard_settings(settings.into()).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
}

fn offer_id(origin: u8) -> oink_clipboard::model::OfferId {
    oink_clipboard::model::OfferId {
        clip: oink_foundation::content::ClipId {
            origin: oink_foundation::DeviceId([origin; 32]),
            seq: 1,
        },
        generation: 1,
    }
}

/// A synchronizer that keeps private formats brings back the marker of the
/// clip Oink wrote: recognised by identity alone, also with deduplication off.
/// An unknown marker is no marker, and no marker reaches the peer.
#[test]
fn marker_kept_by_a_synchronizer_identifies_the_write_back() {
    crate::test_watchdog::run(
        module_path!(),
        "marker_kept_by_a_synchronizer_identifies_the_write_back",
        Some(4),
        async {
            let a = start("marker-a", 0).await;
            let b = start("marker-b", 0).await;
            a.clip.set_deferred_native(true);
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;

            a.clip.copy(text("marked"), Sensitivity::Normal);
            wait_for("text on B", 5.0, || b.clip.items() == text("marked")).await;
            quiet().await;
            let marker = b.clip.marker().expect("Oink writes carry the clip marker");
            let a_count = a.clip.change_count();
            // Other content and deduplication off: only the marker can
            // recognise this as B's write coming back.
            set_dedupe(&b, false).await;
            b.clip
                .copy_with_marker(text("rewritten by a synchronizer"), Some(marker), Sensitivity::Normal);
            quiet().await;
            assert_eq!(a.clip.change_count(), a_count, "a known marker is not announced");

            let mut bogus = oink_clipboard::dedupe::marker(offer_id(9)).unwrap();
            bogus[40] ^= 1;
            b.clip.copy_with_marker(text("fresh"), Some(bogus), Sensitivity::Normal);
            wait_for("fresh offer on A", 5.0, || a.clip.change_count() > a_count).await;
            let spec = a.clip.native_spec().expect("deferred write on A");
            assert!(
                spec.inventory
                    .items
                    .iter()
                    .flat_map(|item| &item.representations)
                    .all(|rep| !oink_clipboard::dedupe::is_marker(&rep.format)),
                "the marker is never announced"
            );
            assert_eq!(
                a.clip.marker(),
                oink_clipboard::dedupe::marker(spec.inventory.id),
                "A marks its own write with B's clip, not B's marker"
            );
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

/// The marker of our own announced offer coming back (Universal Clipboard
/// pushing another Mac's Oink write to this Mac, #35) is not announced and
/// rebinds the offer, so peers read the newest content.
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
            // Like AppKit/OLE capture: sizes unknown until read.
            a.clip.set_unknown_representation_sizes(true);
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            a.clip.copy(text("own offer"), Sensitivity::Normal);
            wait_for("lazy offer on B", 5.0, || b.clip.native_spec().is_some()).await;
            quiet().await;
            let spec = b.clip.native_spec().unwrap();
            let marker = b.clip.marker().expect("deferred writes carry the marker too");
            assert_eq!(Some(marker.clone()), oink_clipboard::dedupe::marker(spec.inventory.id));
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

/// A synchronizer that drops private formats: compared by content, but only
/// against this device's own previous copy (real local data).
#[test]
fn deduplication_compares_with_the_previous_local_copy() {
    crate::test_watchdog::run(
        module_path!(),
        "deduplication_compares_with_the_previous_local_copy",
        Some(4),
        async {
            let a = start("dedupe-a", 0).await;
            let b = start("dedupe-b", 0).await;
            a.clip.set_unknown_representation_sizes(true);
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            assert!(a.node.clipboard_settings().unwrap().dedupe, "on by default");

            a.clip.copy(text("echo me\n"), Sensitivity::Normal);
            wait_for("text on B", 5.0, || b.clip.items() == text("echo me\n")).await;
            quiet().await;
            let b_count = b.clip.change_count();
            // A synchronizer writes A's own copy back to A with CRLF and
            // without any private format: a subset of the previous copy.
            a.clip.copy(text("echo me\r\n"), Sensitivity::Normal);
            quiet().await;
            // The same content copied again: sent once only.
            a.clip.copy(text("echo me"), Sensitivity::Normal);
            quiet().await;
            assert_eq!(
                b.clip.change_count(),
                b_count,
                "duplicates of the local copy are not announced"
            );

            // A text prefix is a new copy.
            a.clip.copy(text("echo"), Sensitivity::Normal);
            wait_for("prefix on B", 5.0, || b.clip.items() == text("echo")).await;
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

/// The previous content is what Oink wrote from an offer: provided lazily by
/// the source device, so it is never read to compare (the source is gone
/// here; a read would wait for it). The same content copied over it is
/// announced at once.
#[test]
fn oink_written_previous_content_is_not_read_and_the_change_is_announced() {
    crate::test_watchdog::run(
        module_path!(),
        "oink_written_previous_content_is_not_read_and_the_change_is_announced",
        Some(4),
        async {
            let a = start("lazy-a", 0).await;
            let b = start("lazy-b", 0).await;
            b.clip.set_deferred_native(true);
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            a.clip.copy(text("lazily written"), Sensitivity::Normal);
            wait_for("lazy offer on B", 5.0, || b.clip.native_spec().is_some()).await;
            quiet().await;
            a.node.shutdown().await;
            let published = b.node.clipboard_activity().unwrap().len();
            b.clip.copy(text("lazily written"), Sensitivity::Normal);
            wait_for("same content announced", 2.0, || {
                b.node.clipboard_activity().unwrap().len() > published
            })
            .await;
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
            assert!(!a.node.clipboard_settings().unwrap().dedupe);
            a.clip.copy(text("twice"), Sensitivity::Normal);
            wait_for("first copy on B", 5.0, || b.clip.items() == text("twice")).await;
            quiet().await;
            let b_count = b.clip.change_count();
            a.clip.copy(text("twice"), Sensitivity::Normal);
            wait_for("second copy on B", 5.0, || b.clip.change_count() > b_count).await;
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
        // B announces its rewrite of A's content (deduplication off on B); A
        // already holds that content and writes it anyway.
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

/// Universal Clipboard (macOS) data is promised by the other Apple device: it
/// is never read to compare, so it is announced even when it repeats the
/// previous local copy.
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
