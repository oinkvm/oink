#![cfg(debug_assertions)]
//! PAKE, membership, and restart behavior moved from network join/upgrade tests.
//! Real nodes and QUIC/TLS; fake OS backends, no desktop validation claim.
use oink_agent as test_agent;
mod common;
use oink_agent::{
    api::{AgentApi, pairing},
    node::{Node, NodeOptions},
};
use oink_foundation::{Point, Rect, Size, display::DisplayInfo};
use oink_test_support::input::FakeInput;
use std::{path::Path, sync::Arc};

fn options(root: &Path, name: &str) -> NodeOptions {
    oink_agent::logs::init();
    let input = FakeInput::new();
    input.set_cursor(Point::new(400., 300.));
    NodeOptions {
        net: oink_test_support::network_config(root.join(name), name, 0),
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
fn session(node: &Node, id: &str) -> pairing::PairView {
    node.pairings()
        .unwrap()
        .sessions
        .into_iter()
        .find(|s| s.id == id)
        .unwrap()
}
async fn request(a: &Node, b: &Node) -> (String, String) {
    common::request(a, b).await
}
/// This node's own speaker "forward" switch (global audio configuration).
fn forwarding(node: &Node) -> bool {
    node.audio_status()
        .unwrap()
        .overview
        .switch(node.id(), oink_audio::route::Role::Speaker)
        == Some(true)
}
/// The forward switch persisted in a node's own audio configuration.
fn saved_forwarding(dir: &Path) -> bool {
    oink_agent::config::AgentConfig::load_from(dir)
        .unwrap()
        .owner()
        .audio(oink_foundation::DeviceId([0; 32]))
        .0
        .forward
        .on
}

#[test]
fn fresh_install_and_pairing_keep_audio_local() {
    common::watchdog::run(
        module_path!(),
        "fresh_install_and_pairing_keep_audio_local",
        Some(4),
        async {
            use oink_agent::device_cards::{SpeakerIcon, SpeakerMode};
            let temp = tempfile::tempdir().unwrap();
            let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
            let b = Node::start_with_fake_platform(options(temp.path(), "b")).await.unwrap();
            for node in [&a, &b] {
                assert!(!forwarding(node));
                assert!(!node.audio_status().unwrap().source_muted);
            }
            common::pair(&a, &b).await;
            // Owner facts can make both cards known before the audio actor has
            // consumed them. Wait for its published baseline independently.
            common::until("paired audio resolution and card baseline", || {
                [&a, &b].iter().all(|node| {
                    let cards = node.device_cards().unwrap();
                    !matches!(
                        node.audio_status().unwrap().resolution,
                        oink_audio::route::Resolution::Unknown
                    ) && cards.cards.len() == 2
                        && cards.cards.iter().all(|card| {
                            card.status.as_ref().is_some_and(|s| {
                                s.speaker.forwarding_enabled.is_some() && s.speaker.resource.configured_mode.is_some()
                            })
                        })
                })
            })
            .await;
            for node in [&a, &b] {
                let status = node.audio_status().unwrap();
                assert!(!forwarding(node), "pairing must not opt in a fresh source");
                assert!(
                    matches!(status.resolution, oink_audio::route::Resolution::None),
                    "{status:?}"
                );
                assert!(!status.source_muted);
                assert_eq!(status.sent, 0);
                for card in node.device_cards().unwrap().cards {
                    let speaker = card.status.unwrap().speaker;
                    assert_eq!(speaker.forwarding_enabled, Some(false));
                    assert_eq!(speaker.resource.configured_mode, Some(SpeakerMode::Unused));
                    assert_eq!(speaker.resource.icon, SpeakerIcon::Unused);
                }
                assert!(!saved_forwarding(node.data_dir()));
            }
            a.shutdown().await;
            b.shutdown().await;
        },
    );
}

#[test]
fn joining_a_group_with_an_explicit_exit_keeps_new_sources_local() {
    common::watchdog::run(
        module_path!(),
        "joining_a_group_with_an_explicit_exit_keeps_new_sources_local",
        Some(4),
        async {
            use oink_agent::{
                config::AgentConfig,
                device_cards::{CardAction, SpeakerMode},
            };
            let temp = tempfile::tempdir().unwrap();
            // A previous explicit forward choice (this device's own audio
            // configuration) remains authoritative.
            AgentConfig::load_from(&temp.path().join("a"))
                .unwrap()
                .update_owner(|owner| {
                    let (mut own, last) = owner.audio(oink_foundation::DeviceId([0; 32]));
                    own.forward.on = true;
                    owner.commit_audio(own, last)
                })
                .unwrap();
            let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
            let b = Node::start_with_fake_platform(options(temp.path(), "b")).await.unwrap();
            common::pair(&a, &b).await;
            // Input admission does not establish audio ordering readiness.
            let mut token = None;
            common::until("explicit exit command ready", || {
                let card = b.device_cards().unwrap().cards.into_iter().find(|c| c.is_self).unwrap();
                if let CardAction::Invoke { token: ready, .. } = card.status.unwrap().speaker.make_exit_action {
                    token = Some(ready);
                    true
                } else {
                    false
                }
            })
            .await;
            let token = token.unwrap();
            let exit = b.clone();
            let reply = tokio::task::spawn_blocking(move || {
                exit.invoke_device_card_action("19191919-1919-1919-1919-191919191919".into(), token)
            })
            .await
            .unwrap()
            .unwrap();
            assert!(
                !matches!(
                    reply,
                    oink_agent::device_cards::DeviceControlReply::Completed(
                        oink_agent::device_cards::DeviceControlCompleted {
                            result: oink_agent::device_cards::DeviceControlResult::Refused { .. },
                            ..
                        }
                    )
                ),
                "explicit exit choice refused: {reply:?}"
            );
            common::until("explicit exit resolved", || {
                a.audio_status().unwrap().resolution.target() == Some(b.id())
            })
            .await;
            let c = Node::start_with_fake_platform(options(temp.path(), "c")).await.unwrap();
            assert!(!forwarding(&c));
            common::pair(&c, &b).await;
            common::until("joined audio baseline", || {
                c.audio_status().unwrap().resolution.target() == Some(b.id())
            })
            .await;
            // The exit keeps forward on for when it loses the exit (Q1); the
            // joining source stays local.
            assert!(!forwarding(&c));
            for node in [&b, &c] {
                let status = node.audio_status().unwrap();
                assert!(!status.source_muted);
                assert_eq!(status.sent, 0);
            }
            assert!(forwarding(&a));
            // Audio resolution and the immutable UI cards publish separately.
            // Wait for the card's own baseline before asserting its mode.
            common::until("joined source local card", || {
                c.device_cards()
                    .unwrap()
                    .cards
                    .iter()
                    .find(|card| card.is_self)
                    .and_then(|card| card.status.as_ref())
                    .is_some_and(|s| s.speaker.resource.configured_mode == Some(SpeakerMode::Unused))
            })
            .await;
            for node in [&a, &b, &c] {
                node.shutdown().await;
            }
            let restarted = Node::start_with_fake_platform(options(temp.path(), "c")).await.unwrap();
            assert!(!forwarding(&restarted));
            restarted.shutdown().await;
            assert!(saved_forwarding(&temp.path().join("a")));
        },
    );
}

#[test]
fn peer_quit_ends_code_entry_promptly() {
    common::watchdog::run(module_path!(), "peer_quit_ends_code_entry_promptly", Some(4), async {
        let temp = tempfile::tempdir().unwrap();
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        let b = Node::start_with_fake_platform(options(temp.path(), "b")).await.unwrap();
        let (id, _) = request(&a, &b).await;
        a.shutdown().await;
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            common::until("peer quit closes code entry", || session(&b, &id).phase == 8),
        )
        .await
        .unwrap();
        assert!(matches!(session(&b, &id).terminal.unwrap().code, 16 | 17));
        b.shutdown().await;
    });
}

async fn start_request(a: &Node, b: &Node, operation: &str) -> String {
    let found = common::query(a, b).await;
    let reply = a
        .start_pairing(
            operation.into(),
            pairing::PairTarget::PreparedQuery {
                query_id: found.query_id,
                generation: found.generation,
            },
        )
        .unwrap();
    let pairing::PairReply::Value(pairing::Started::Started { pair_id }) = reply else {
        panic!("{reply:?}")
    };
    pair_id
}

#[test]
#[ignore = "slow: requires real network delay and loss qualification"]
fn slow_manual_peer_is_prepared_within_budget_and_real_network_loss_ends_pairing() {
    common::watchdog::run(
        module_path!(),
        "slow_manual_peer_is_prepared_within_budget_and_real_network_loss_ends_pairing",
        Some(4),
        async {
            use std::time::{Duration, Instant};
            let temp = tempfile::tempdir().unwrap();
            let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
            let b = Node::start_with_fake_platform(options(temp.path(), "b")).await.unwrap();
            let target: std::net::SocketAddr = ([127, 0, 0, 1], b.network_info().unwrap().port).into();
            let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let address = socket.local_addr().unwrap();
            let proxy = tokio::spawn(async move {
                let mut bytes = vec![0u8; 65536];
                let (len, source) = socket.recv_from(&mut bytes).await.unwrap();
                tokio::time::sleep(Duration::from_secs(2)).await;
                socket.send_to(&bytes[..len], target).await.unwrap();
                loop {
                    let (len, from) = socket.recv_from(&mut bytes).await.unwrap();
                    socket
                        .send_to(&bytes[..len], if from == target { source } else { target })
                        .await
                        .unwrap();
                }
            });
            let began = Instant::now();
            let found = common::query_at(&a, b.id(), address).await;
            let elapsed = began.elapsed();
            eprintln!("delayed manual TLS+C0 prepared in {} ms", elapsed.as_millis());
            assert!((Duration::from_secs(2)..Duration::from_secs(5)).contains(&elapsed));
            let pairing::PairReply::Value(pairing::Started::Started { pair_id }) = a
                .start_pairing(
                    "slow-peer".into(),
                    pairing::PairTarget::PreparedQuery {
                        query_id: found.query_id,
                        generation: found.generation,
                    },
                )
                .unwrap()
            else {
                panic!("start")
            };
            common::until("slow peer incoming", || {
                b.pairings().unwrap().requests.iter().any(|r| r.id == pair_id)
            })
            .await;
            let incoming = b
                .pairings()
                .unwrap()
                .requests
                .into_iter()
                .find(|r| r.id == pair_id)
                .unwrap();
            b.accept_pairing(incoming.id, incoming.revision).unwrap();
            common::until("slow peer code entry", || session(&b, &pair_id).phase == 2).await;
            proxy.abort();
            let _ = proxy.await;
            tokio::time::timeout(
                Duration::from_secs(3),
                common::until("real UDP path loss", || {
                    session(&a, &pair_id).phase == 8 && session(&b, &pair_id).phase == 8
                }),
            )
            .await
            .unwrap();
            a.shutdown().await;
            b.shutdown().await;
        },
    );
}

#[test]
#[ignore = "slow: requires a periodic group state clock injection"]
fn waiting_for_code_survives_periodic_group_state_and_still_commits() {
    common::watchdog::run(
        module_path!(),
        "waiting_for_code_survives_periodic_group_state_and_still_commits",
        Some(4),
        async {
            let temp = tempfile::tempdir().unwrap();
            let members = common::start([options(temp.path(), "a"), options(temp.path(), "b")]).await;
            let c = Node::start_with_fake_platform(options(temp.path(), "c")).await.unwrap();
            let b = &members[1];
            let (id, code) = request(&c, b).await;
            // A group member broadcasts its current roster every 60 s. An unknown
            // peer awaiting a human code is absent from that roster by definition.
            tokio::time::sleep(std::time::Duration::from_secs(62)).await;
            assert_eq!(session(&c, &id).phase, 1, "show-code must have no human deadline");
            assert_eq!(session(b, &id).phase, 2, "code-entry must have no human deadline");
            assert_eq!(c.pairing_code(id.clone()).unwrap().six_digits, code);
            common::submit(b, id.clone(), 1, code).await;
            common::until("pairing after human wait", || {
                session(&c, &id).phase == 7 && session(b, &id).phase == 7
            })
            .await;
            let source_listener = c.network_info().unwrap().port;
            assert!(
                b.member_addresses(c.id().into())
                    .unwrap()
                    .iter()
                    .any(|a| a.ends_with(&format!(":{source_listener}"))),
                "retain the advertised listener, not the manual query's ephemeral source port"
            );
            c.shutdown().await;
            for member in &members {
                member.shutdown().await;
            }
        },
    );
}

#[test]
fn onboarding_admits_one_pairing_and_regular_mode_keeps_the_pending_limit() {
    common::watchdog::run(
        module_path!(),
        "onboarding_admits_one_pairing_and_regular_mode_keeps_the_pending_limit",
        Some(4),
        async {
            let temp = tempfile::tempdir().unwrap();
            let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
            let b = Node::start_with_fake_platform(options(temp.path(), "b")).await.unwrap();
            let c = Node::start_with_fake_platform(options(temp.path(), "c")).await.unwrap();
            b.set_onboarding_connect(true).unwrap();
            let (first, _) = request(&a, &b).await;
            let refused = start_request(&c, &b, "onboarding-second").await;
            common::until("onboarding refuses second requester", || {
                c.pairings()
                    .unwrap()
                    .sessions
                    .iter()
                    .any(|s| s.id == refused && s.phase == 8)
            })
            .await;
            assert_eq!(session(&c, &refused).terminal.unwrap().code, 11); // QueueFull: nobody declined
            assert!(b.pairings().unwrap().requests.is_empty());
            assert!(!b.pairings().unwrap().sessions.iter().any(|s| s.id == refused));
            assert_eq!(session(&b, &first).phase, 2);
            a.cancel_pairing(first.clone(), session(&a, &first).revision).unwrap();
            common::until("cancel releases onboarding slot", || session(&b, &first).phase == 8).await;
            let replacement = start_request(&c, &b, "onboarding-replacement").await;
            common::until("failed session replaced without leaving connect stage", || {
                b.pairings().unwrap().requests.iter().any(|r| r.id == replacement)
            })
            .await;
            let pending = b
                .pairings()
                .unwrap()
                .requests
                .into_iter()
                .find(|r| r.id == replacement)
                .unwrap();
            b.accept_pairing(pending.id, pending.revision).unwrap();
            b.cancel_pairing(replacement.clone(), session(&b, &replacement).revision)
                .unwrap();
            common::until("replacement cancelled", || session(&c, &replacement).phase == 8).await;
            let (outgoing, _) = request(&b, &a).await;
            let refused_again = start_request(&c, &b, "onboarding-show-code-busy").await;
            common::until("show-code also refuses another requester", || {
                session(&c, &refused_again).phase == 8
            })
            .await;
            assert_eq!(session(&c, &refused_again).terminal.unwrap().code, 11);
            b.set_onboarding_connect(false).unwrap();
            let normal = start_request(&c, &b, "normal-after-onboarding").await;
            common::until("normal mode accepts another pending request", || {
                b.pairings().unwrap().requests.iter().any(|r| r.id == normal)
            })
            .await;
            // Existing request-list policy is three pending, even with an accepted session.
            for i in 0..2 {
                let id = start_request(&a, &b, &format!("normal-pending-{i}")).await;
                common::until("normal pending row", || {
                    b.pairings().unwrap().requests.iter().any(|r| r.id == id)
                })
                .await;
            }
            assert_eq!(b.pairings().unwrap().requests.len(), 3);
            let overflow = start_request(&a, &b, "normal-overflow").await;
            common::until("normal fourth pending still refused", || {
                session(&a, &overflow).phase == 8
            })
            .await;
            assert_eq!(session(&a, &overflow).terminal.unwrap().code, 46);
            assert_eq!(session(&b, &outgoing).phase, 1);
            for n in [&a, &b, &c] {
                n.shutdown().await;
            }
        },
    );
}
#[test]
fn wrong_codes_are_limited_and_closed_round_cannot_commit() {
    common::watchdog::run(
        module_path!(),
        "wrong_codes_are_limited_and_closed_round_cannot_commit",
        Some(4),
        wrong_codes_are_limited_and_closed_round_cannot_commit_scenario(),
    );
}

async fn wrong_codes_are_limited_and_closed_round_cannot_commit_scenario() {
    let temp = tempfile::tempdir().unwrap();
    let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
    let b = Node::start_with_fake_platform(options(temp.path(), "b")).await.unwrap();
    let (id, code) = request(&a, &b).await;
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for attempt in 1..=3 {
        common::submit(&b, id.clone(), attempt, wrong.into()).await;
        common::until("wrong code result", || {
            let view = session(&b, &id);
            if attempt < 3 {
                view.phase == 2 && view.attempts_left == 3 - attempt
            } else {
                view.phase == 8 && view.terminal.is_some()
            }
        })
        .await;
    }
    let view = session(&b, &id);
    assert!(matches!(
        b.submit_pairing_code(id.clone(), view.revision, 4, code).unwrap(),
        pairing::PairReply::Problem(_)
    ));
    assert_eq!(a.group_members().len(), 1);
    assert_eq!(b.group_members().len(), 1);
    assert!(
        b.subscribe_pairing_events(test_agent::device_cards::DecimalU64(0))
            .unwrap()
            .iter()
            .any(|e| matches!(e,pairing::PairingEvent::PairingFailed {pair_id,..} if pair_id == &id))
    );
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn either_party_cancels_without_granting_membership() {
    common::watchdog::run(
        module_path!(),
        "either_party_cancels_without_granting_membership",
        Some(4),
        either_party_cancels_without_granting_membership_scenario(),
    );
}

async fn either_party_cancels_without_granting_membership_scenario() {
    for initiator_cancels in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let a = Node::start_with_fake_platform(options(temp.path(), "a")).await.unwrap();
        let b = Node::start_with_fake_platform(options(temp.path(), "b")).await.unwrap();
        let (id, code) = request(&a, &b).await;
        let cancel = if initiator_cancels { &a } else { &b };
        let view = session(cancel, &id);
        assert!(matches!(
            cancel.cancel_pairing(id.clone(), view.revision).unwrap(),
            pairing::PairReply::Value(pairing::CancelResult::Cancelled)
        ));
        common::until("cancel closes both sessions", || {
            session(&a, &id).phase == 8 && session(&b, &id).phase == 8
        })
        .await;
        let view = session(&b, &id);
        assert!(matches!(
            b.submit_pairing_code(id, view.revision, 1, code).unwrap(),
            pairing::PairReply::Problem(_)
        ));
        assert_eq!(a.group_members().len(), 1);
        assert_eq!(b.group_members().len(), 1);
        a.shutdown().await;
        b.shutdown().await;
    }
}

#[test]
fn three_members_remove_and_last_member_becomes_independent() {
    common::watchdog::run(
        module_path!(),
        "three_members_remove_and_last_member_becomes_independent",
        Some(4),
        three_members_remove_and_last_member_becomes_independent_scenario(),
    );
}

async fn three_members_remove_and_last_member_becomes_independent_scenario() {
    let temp = tempfile::tempdir().unwrap();
    let nodes = common::start([
        options(temp.path(), "a"),
        options(temp.path(), "b"),
        options(temp.path(), "c"),
    ])
    .await;
    let (a, b, c) = (&nodes[0], &nodes[1], &nodes[2]);
    assert_eq!(a.group_members().len(), 3);
    a.group_remove(c.id()).await.unwrap();
    common::until("removed peer independent", || {
        c.group_members().len() == 1 && c.group().unwrap().members.len() == 1 && !b.group_members().contains(&c.id())
    })
    .await;
    assert!(c.group().unwrap().members.len() == 1);
    common::active(&[a, b]).await;
    assert!(
        !a.layout().0.screens.iter().any(|s| s.id.device == c.id()),
        "membership removal clears the removed device's screens"
    );
    b.group_leave().await.unwrap();
    common::until("last valid member independent", || {
        a.group_members().len() == 1 && b.group_members().len() == 1
    })
    .await;
    for node in &nodes {
        node.shutdown().await;
    }
}

#[test]
fn copied_target_identity_and_full_member_list_reconnect_without_pairing() {
    common::watchdog::run(
        module_path!(),
        "copied_target_identity_and_full_member_list_reconnect_without_pairing",
        Some(4),
        copied_target_identity_and_full_member_list_reconnect_without_pairing_scenario(),
    );
}

async fn copied_target_identity_and_full_member_list_reconnect_without_pairing_scenario() {
    let original = tempfile::tempdir().unwrap();
    let nodes = common::start([
        options(original.path(), "a"),
        options(original.path(), "b"),
        options(original.path(), "c"),
    ])
    .await;
    common::until("full roster propagated", || {
        nodes.iter().all(|n| n.group_members().len() == 3)
    })
    .await;
    let identities = nodes.iter().map(Node::id).collect::<Vec<_>>();
    let roster = nodes[0].link_config().unwrap().applied().members.clone();
    common::until("same member list", || {
        nodes
            .iter()
            .all(|n| n.link_config().unwrap().applied().members == roster)
    })
    .await;
    for node in &nodes {
        node.shutdown().await;
    }
    drop(nodes);
    let restored = tempfile::tempdir().unwrap();
    for name in ["a", "b", "c"] {
        let destination = restored.path().join(name);
        std::fs::create_dir_all(&destination).unwrap();
        for file in ["device.key", "group-state.json"] {
            std::fs::copy(original.path().join(name).join(file), destination.join(file)).unwrap();
        }
    }
    let a = Node::start_with_fake_platform(options(restored.path(), "a"))
        .await
        .unwrap();
    let b = Node::start_with_fake_platform(options(restored.path(), "b"))
        .await
        .unwrap();
    assert_eq!(common::query(&a, &b).await.peer.device.0, b.id());
    common::active(&[&a, &b]).await;
    assert_eq!(a.group_members().len(), 3, "offline members do not dissolve a group");
    let c = Node::start_with_fake_platform(options(restored.path(), "c"))
        .await
        .unwrap();
    assert_eq!(common::query(&a, &c).await.peer.device.0, c.id());
    common::active(&[&a, &b, &c]).await;
    assert_eq!([a.id(), b.id(), c.id()].as_slice(), identities);
    for (node, name) in [(&a, "a"), (&b, "b"), (&c, "c")] {
        assert!(node.pairings().unwrap().requests.is_empty() && node.pairings().unwrap().sessions.is_empty());
        assert_eq!(
            node.link_config().unwrap().applied().members.clone(),
            roster,
            "no fresh Add needed on restart"
        );
        assert_eq!(
            std::fs::read(node.data_dir().join("device.key")).unwrap(),
            std::fs::read(original.path().join(name).join("device.key")).unwrap()
        );
        node.shutdown().await;
    }
}

#[test]
fn pairing_keeps_pause_local_in_both_directions() {
    common::watchdog::run(
        module_path!(),
        "pairing_keeps_pause_local_in_both_directions",
        Some(4),
        pairing_keeps_pause_local_in_both_directions_scenario(),
    );
}

async fn pairing_keeps_pause_local_in_both_directions_scenario() {
    for group_initiates in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let nodes = common::start([options(temp.path(), "a"), options(temp.path(), "b")]).await;
        let (a, b) = (&nodes[0], &nodes[1]);
        let command = a.clone();
        tokio::task::spawn_blocking(move || {
            command.set_local_paused("00000000-0000-4000-8000-00000000ff01".into(), true)
        })
        .await
        .unwrap()
        .unwrap();
        common::until("source locally paused", || a.fully_paused()).await;
        assert!(!b.fully_paused());
        let c = Node::start_with_fake_platform(options(temp.path(), "c")).await.unwrap();
        assert!(!c.fully_paused());
        let (initiator, responder) = if group_initiates { (a, &c) } else { (&c, a) };
        let (id, code) = request(initiator, responder).await;
        common::submit(responder, id.clone(), 1, code).await;
        common::until("directional merge retains local pause", || {
            c.group_id() == a.group_id() && c.group_members().len() == 3 && a.fully_paused() && !c.fully_paused()
        })
        .await;
        assert!(
            common::input_id(&c, a.id()).is_none(),
            "paused pairing cannot open business"
        );
        let command = a.clone();
        tokio::task::spawn_blocking(move || {
            command.set_local_paused("00000000-0000-4000-8000-00000000ff02".into(), false)
        })
        .await
        .unwrap()
        .unwrap();
        common::active(&[a, b, &c]).await;
        for node in &nodes {
            node.shutdown().await;
        }
        c.shutdown().await;
    }
}

#[test]
fn different_groups_merge_in_the_explicit_request_direction() {
    common::watchdog::run(
        module_path!(),
        "different_groups_merge_in_the_explicit_request_direction",
        Some(4),
        different_groups_merge_in_the_explicit_request_direction_scenario(),
    );
}

async fn different_groups_merge_in_the_explicit_request_direction_scenario() {
    let temp = tempfile::tempdir().unwrap();
    let first = common::start([options(temp.path(), "a"), options(temp.path(), "b")]).await;
    let second = common::start([options(temp.path(), "c"), options(temp.path(), "d")]).await;
    let target = second[0].group_id();
    common::pair(&first[0], &second[0]).await;
    common::until("four members adopt target group", || {
        first
            .iter()
            .chain(&second)
            .all(|node| node.group_id() == target && node.group_members().len() == 4)
    })
    .await;
    for node in first.iter().chain(&second) {
        node.shutdown().await;
    }
}

/// A desktop and a phone paired over loopback, both online to each other.
async fn paired_mobile(
    root: &Path,
) -> (
    Node,
    oink_mobile_core::MobileNetwork,
    oink_foundation::DeviceId,
    std::path::PathBuf,
) {
    use oink_foundation::session::Boot;
    use oink_mobile_core::{MobileNetwork, MobileNetworkConfig};
    use oink_network::{group_state::State, identity::Identity, pairing as network_pairing};
    use oink_protocol::v2::{limits, schema::WireDto};

    let desktop = Node::start_with_fake_platform(options(root, "desktop")).await.unwrap();
    let desktop_addr = ([127, 0, 0, 1], desktop.network_info().unwrap().port).into();
    let seed = [81; 32];
    let mobile_id = Identity::from_seed(seed);
    let mobile_group = State::create(&mobile_id, "test phone", "android", 1).unwrap();
    let mobile_dir = root.join("mobile");
    let state_path = mobile_dir.join("group-state.v2");
    let mobile = MobileNetwork::start(MobileNetworkConfig {
        identity_seed: seed,
        boot: Boot {
            device_id: mobile_id.id(),
            nonce: [91; 16],
        },
        bind: "127.0.0.1:0".parse().unwrap(),
        group_state: Some(mobile_group.wire().encode(limits::CONFIG_STATE).unwrap()),
        group_state_path: state_path.clone(),
        pairing_journal_path: mobile_dir.join("pairing-recovery.cbor"),
        device_name: "test phone".into(),
        os: "android".into(),
        form_factor: None,
        trusted_peers: Vec::new(),
        trust_path: None,
        process: Default::default(),
    })
    .await
    .unwrap();
    mobile.dial(desktop.id(), vec![desktop_addr]).unwrap();
    common::until("mobile authenticated desktop greeting", || {
        mobile.authenticated_advert(desktop.id()).is_some()
    })
    .await;
    let pair = mobile.start_pairing(desktop.id()).unwrap();
    common::converge(&[&desktop], "desktop receives the mobile pairing request", || {
        desktop
            .pairings()
            .unwrap()
            .requests
            .iter()
            .any(|request| request.id == pair)
    })
    .await;
    let pending = desktop
        .pairings()
        .unwrap()
        .requests
        .into_iter()
        .find(|request| request.id == pair)
        .unwrap();
    let client = desktop.clone();
    let accepted = tokio::task::spawn_blocking(move || client.accept_pairing(pending.id, pending.revision))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(accepted, pairing::PairReply::Value(_)));
    let code = mobile.pairing_code(&pair).unwrap();
    let submitted = common::submit(&desktop, pair.clone(), 1, code).await;
    assert!(matches!(submitted, pairing::PairReply::Value(_)));
    common::until("C0 pairing receipt and desktop facts on the phone", || {
        let state = mobile.pairing_snapshot().unwrap();
        let session = state
            .sessions
            .iter()
            .find(|session| network_pairing::key(session.id) == pair);
        assert!(
            !session.is_some_and(|session| session.phase == network_pairing::Phase::Ended),
            "{session:?}"
        );
        session.is_some_and(|session| session.phase == network_pairing::Phase::Completed)
            && mobile.peer_facts(desktop.id()).is_some()
            && desktop.device_cards().unwrap().cards.iter().any(|card| {
                card.device.0 == mobile_id.id()
                    && matches!(card.connection, oink_agent::device_cards::CardConnection::Online { .. })
            })
    })
    .await;
    (desktop, mobile, mobile_id.id(), state_path)
}

#[test]
fn mobile_pairing_converges_with_desktop_owner() {
    common::watchdog::run(
        module_path!(),
        "mobile_pairing_converges_with_desktop_owner",
        Some(4),
        async {
            use oink_network::group_state::State;
            use oink_protocol::v2::{dto, limits, schema::WireDto};

            let temp = tempfile::tempdir().unwrap();
            let (desktop, mobile, mobile_id, state_path) = paired_mobile(temp.path()).await;
            let bytes = std::fs::read(state_path).unwrap();
            let wire =
                dto::GroupState::from_raw(oink_protocol::v2::cbor::decode_map(&bytes, limits::CONFIG_STATE).unwrap())
                    .unwrap();
            let persisted = State::from_wire(wire).unwrap();
            assert!(persisted.roster().contains(desktop.id()));
            assert!(persisted.roster().contains(mobile_id));
            mobile.stop().await;
            desktop.shutdown().await;
        },
    );
}

/// Route selection is baseline core/2 on phones too (route design D10,
/// protocol-v2 11.3): the desktop's route menu for a phone has data, and a
/// choice made on the desktop moves the link and pins it on the phone.
#[test]
fn desktop_measures_and_chooses_a_phone_route() {
    common::watchdog::run(
        module_path!(),
        "desktop_measures_and_chooses_a_phone_route",
        Some(4),
        async {
            use oink_agent::api::{DeviceKey, RouteRowState, RouteSelection};
            use oink_network::link::{Direction, RouteRecord};

            let temp = tempfile::tempdir().unwrap();
            let (desktop, mobile, mobile_id, _) = paired_mobile(temp.path()).await;
            let phone_addr = mobile.local_addr();
            let phone_ip = phone_addr.ip().to_string();
            // The phone dialed, so the desktop is the passive end; it keeps a
            // standby to the phone's published endpoint and measures it.
            common::until("desktop measures the phone's published endpoint", || {
                desktop.member_routes(DeviceKey(mobile_id)).is_ok_and(|routes| {
                    routes.available
                        && !routes.relayed
                        && routes.passive
                        && routes.can_replace
                        && routes.rows.iter().any(|row| {
                            row.ip == phone_ip && row.port == phone_addr.port() && row.state == RouteRowState::Reachable
                        })
                })
            })
            .await;
            let client = desktop.clone();
            let ip = phone_ip.clone();
            let applied = tokio::task::spawn_blocking(move || {
                client.set_member_route(DeviceKey(mobile_id), RouteSelection::Address { ip })
            })
            .await
            .unwrap()
            .unwrap();
            assert!(applied);
            let desktop_id = desktop.id();
            common::until("the phone follows the desktop's choice and keeps its pin", || {
                mobile
                    .links()
                    .by_peer(desktop_id)
                    .and_then(|entry| entry.route.clone())
                    .is_some_and(|route| {
                        route.direction == Direction::Inbound
                            && route.pin == Some(RouteRecord::Incoming(std::net::Ipv4Addr::LOCALHOST))
                            && route.pin_set_by == Some(desktop_id)
                    })
            })
            .await;
            common::until("the desktop is the active end on the chosen address", || {
                desktop.member_routes(DeviceKey(mobile_id)).is_ok_and(|routes| {
                    !routes.passive
                        && matches!(&routes.selection, RouteSelection::Address { ip } if *ip == phone_ip)
                        && routes.rows.iter().any(|row| row.current && row.ip == phone_ip)
                })
            })
            .await;
            mobile.stop().await;
            desktop.shutdown().await;
        },
    );
}
