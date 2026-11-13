#![cfg(debug_assertions)]
//! The pre-login profile on real nodes and QUIC/TLS with fake OS backends: a
//! working copy of a member's exported files rejoins the group as the same
//! device, and nobody can pair with it. No desktop validation claim.
use oink_agent as test_agent;
mod common;
use oink_agent::{
    api::{AgentApi, pairing},
    node::{Node, NodeOptions, Profile},
};
use oink_foundation::{Point, Rect, Size, display::DisplayInfo};
use oink_test_support::input::FakeInput;
use std::{path::Path, sync::Arc};

fn options(root: &Path, name: &str, port: u16) -> NodeOptions {
    oink_agent::logs::init();
    let input = FakeInput::new();
    input.set_cursor(Point::new(400., 300.));
    NodeOptions {
        net: oink_test_support::network_config(root.join(name), name, port),
        input: Arc::new(input),
        displays: vec![DisplayInfo {
            key: "main".into(),
            name: "main".into(),
            bounds: Rect::new(0., 0., 800., 600.),
            pixel_width: 800,
            pixel_height: 600,
            reference_extent: Some(Size::new(300., 200.)),
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        }],
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        cursor_trail: false,
    }
}

/// The files the Windows user backend exports (oink_winsvc::SnapshotFile).
const SNAPSHOT: [&str; 5] = [
    "device.key",
    "group-state.json",
    "settings.json",
    "addresses.json",
    "detached-screens.json",
];

#[test]
fn a_prelogin_working_copy_rejoins_as_the_same_device_and_refuses_pairing() {
    common::watchdog::run(
        module_path!(),
        "a_prelogin_working_copy_rejoins_as_the_same_device_and_refuses_pairing",
        Some(4),
        async {
            let root = tempfile::tempdir().unwrap();
            // A full member with clipboard and audio owners: they must leave the
            // pre-login peer alone rather than fail its links.
            let mut full = options(root.path(), "a", 0);
            full.clipboard = Some(Arc::new(oink_test_support::clipboard::FakeClipboard::new()));
            let a = Node::start_with_fake_platform(full).await.unwrap();
            let b = Node::start_with_fake_platform(options(root.path(), "b", 0))
                .await
                .unwrap();
            common::pair(&a, &b).await;
            let device = b.id();
            let port = b.links().unwrap().local_addr().port();
            common::phase("member exits; its files are copied like the service's snapshot");
            b.shutdown().await;
            drop(b);
            let work = root.path().join("work");
            std::fs::create_dir_all(&work).unwrap();
            for name in SNAPSHOT {
                let source = root.path().join("b").join(name);
                if source.exists() {
                    std::fs::copy(&source, work.join(name)).unwrap();
                }
            }
            let prelogin = Node::start_prelogin_with_fake_platform(options(root.path(), "work", port))
                .await
                .unwrap();
            assert_eq!(prelogin.profile(), Profile::Prelogin);
            assert_eq!(prelogin.id(), device, "the copy must keep the member's DeviceId");
            common::phase("pre-login copy is an active member again");
            common::active(&[&a, &prelogin]).await;
            // The link survives the member's content/audio owners for a while.
            common::until("pre-login copy remains active", || {
                common::linked(&a, device) && prelogin.links().is_ok()
            })
            .await;
            assert!(common::linked(&a, device));
            // Device cards, remote options, system arrangement and diagnostics
            // of the logon desktop stay available to the member (review M4).
            let agreement = a.link_config().unwrap().peer_agreement(device).unwrap();
            for capability in ["oink.device/2", "oink.layout/2", "oink.diagnostics/2"] {
                assert!(agreement.contains(capability), "{capability}");
            }
            common::phase("an outsider cannot pair with the pre-login backend");
            let outsider = Node::start_with_fake_platform(options(root.path(), "outsider", 0))
                .await
                .unwrap();
            let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            let found = common::query_at(&outsider, device, address).await;
            let client = outsider.clone();
            let started = tokio::task::spawn_blocking(move || {
                client.start_pairing(
                    "00000000-0000-4000-8000-0000000000f1".into(),
                    pairing::PairTarget::PreparedQuery {
                        query_id: found.query_id,
                        generation: found.generation,
                    },
                )
            })
            .await
            .unwrap();
            if let Ok(pairing::PairReply::Value(pairing::Started::Started { pair_id })) = started {
                // Pairing is not negotiated with this backend: the attempt ends
                // without ever reaching it as an incoming request.
                common::until("outsider pairing attempt ended", || {
                    assert!(prelogin.pairings().unwrap().requests.is_empty());
                    outsider
                        .pairings()
                        .unwrap()
                        .sessions
                        .iter()
                        .any(|s| s.id == pair_id && s.phase == 8)
                })
                .await;
            }
            assert!(prelogin.pairings().unwrap().requests.is_empty());
            assert!(
                !prelogin
                    .group()
                    .unwrap()
                    .members
                    .iter()
                    .any(|m| m.device.id.0 == outsider.id())
            );
            outsider.shutdown().await;
            prelogin.shutdown().await;
            a.shutdown().await;
        },
    );
}
