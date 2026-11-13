use super::*;
use oink_audio::route::{Mode, Resolution, Role};
use oink_protocol::v2::dto;

#[test]
fn concurrent_exit_selections_converge_after_directional_group_merge() {
    crate::test_watchdog::run(
        module_path!(),
        "concurrent_exit_selections_converge_after_directional_group_merge",
        Some(4),
        async {
            let a = start("merge-exit-a", 0).await;
            let ap = start("merge-exit-ap", 0).await;
            let b = start("merge-exit-b", 0).await;
            let bp = start("merge-exit-bp", 0).await;
            common::pair(&ap.node, &a.node).await;
            common::pair(&bp.node, &b.node).await;
            tokio::join!(select_exit(&a.node), select_exit(&b.node));
            let source_group = b.node.group_id().unwrap();
            let target_group = a.node.group_id().unwrap();
            common::pair(&b.node, &a.node).await;
            let nodes = [&a.node, &ap.node, &b.node, &bp.node];
            common::until("signed merged rosters reach every member", || {
                nodes.iter().all(|node| {
                    let group = node.group().unwrap();
                    group.id == Some(target_group.into())
                        && nodes
                            .iter()
                            .all(|peer| group.members.iter().any(|member| member.device.id.0 == peer.id()))
                })
            })
            .await;
            // Discovery is disabled. Supply endpoint hints only after signed
            // membership converges, so this audio-ordering fixture does not
            // depend on cascading relay upgrades and their dial backoff. The
            // member API supplies addresses, never membership authority.
            for node in nodes {
                let endpoints = nodes
                    .iter()
                    .filter(|peer| peer.id() != node.id())
                    .map(|peer| {
                        let address = std::net::SocketAddr::from(([127, 0, 0, 1], peer.network_info().unwrap().port));
                        (peer.id(), address.to_string())
                    })
                    .collect::<Vec<_>>();
                api(node, move |node| {
                    for (device, address) in endpoints {
                        node.set_member_address(device.into(), address).unwrap();
                    }
                })
                .await;
            }
            common::active(&nodes).await;
            assert!(nodes.iter().all(|node| node.group_id() == Some(target_group)));
            // The joining group restarts its audio configuration and merges
            // with the target group: the target group's exit stays and the
            // joining exit falls back to forward (F6, user 2026-11-11 B).
            wait_for_audio(
                "the target group's exit survives the group merge",
                &nodes,
                |resolutions| {
                    let winner = resolutions[0];
                    winner.available()
                        && winner.target() == Some(a.node.id())
                        && resolutions.iter().all(|resolution| *resolution == winner)
                },
            )
            .await;
            assert_ne!(source_group, target_group);
            let b_own = b.node.audio_status().unwrap().overview;
            assert_eq!(b_own.mode(b.node.id(), Role::Speaker), Some(Mode::Shared));
            tokio::join!(select_exit(&ap.node), select_exit(&bp.node));
            wait_for_audio(
                "concurrent exit selections converge across merged members",
                &nodes,
                |resolutions| {
                    let resolution = resolutions[0];
                    resolution.available()
                        && matches!(resolution.target(), Some(owner) if owner == ap.node.id() || owner == bp.node.id())
                        && resolutions.iter().all(|current| *current == resolution)
                },
            )
            .await;
            for node in nodes {
                node.shutdown().await;
            }
        },
    );
}

async fn wait_for_audio(what: &str, nodes: &[&Node], mut ready: impl FnMut(&[Resolution]) -> bool) {
    let started = Instant::now();
    let end = started + Duration::from_secs(5);
    let mut observations = std::collections::VecDeque::new();
    let observed = observe_until(end, Instant::now, || {
        let at = started.elapsed();
        let resolutions = audio_resolutions(nodes);
        let matches = ready(&resolutions);
        if observations.len() == 8 {
            observations.pop_front();
        }
        observations.push_back((at, started.elapsed(), resolutions));
        matches
    })
    .await;
    if observed {
        return;
    }
    eprintln!(
        "AUDIO_CONVERGENCE_TIMING stage={what} elapsed={:?} last_observations={observations:?} final_observation={:?}",
        started.elapsed(),
        audio_resolutions(nodes)
    );
    dump_audio_convergence(what, nodes);
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn readiness_is_observed_after_a_delayed_deadline_wakeup() {
    // The producer becomes ready inside the deadline, then the waiter cannot
    // run until after it. A controllable clock forces that order without a
    // machine-speed assertion or a long blocked executor.
    for becomes_ready in [true, false] {
        let fake_clock = std::cell::Cell::new(Instant::now());
        let deadline = fake_clock.get() + Duration::from_secs(5);
        let ready = std::cell::Cell::new(false);
        let mut waiting = Box::pin(observe_until(deadline, || fake_clock.get(), || ready.get()));
        let pending = std::future::poll_fn(|context| {
            std::task::Poll::Ready(std::future::Future::poll(waiting.as_mut(), context).is_pending())
        })
        .await;
        assert!(pending, "the first false observation waits for a notification");
        ready.set(becomes_ready);
        fake_clock.set(deadline + Duration::from_millis(1));
        assert_eq!(waiting.await, becomes_ready, "ready={becomes_ready} at wakeup");
    }
}

fn audio_resolutions(nodes: &[&Node]) -> Vec<Resolution> {
    nodes
        .iter()
        .map(|node| node.audio_status().unwrap().resolution)
        .collect()
}

fn dump_audio_convergence(what: &str, nodes: &[&Node]) {
    eprintln!("AUDIO_CONVERGENCE_TIMEOUT stage={what}");
    for (index, node) in nodes.iter().enumerate() {
        let group = node.inner.group.get().unwrap();
        let applied = group.applied();
        let (active_group, own) = {
            let config = crate::node::lock(&node.inner.config);
            let owner = config.owner();
            (owner.active_group(), owner.audio(node.id()))
        };
        eprintln!(
            "node[{index}] id={:?} boot={:?} group={:?} active_group={active_group:?} paused={} members={:?} audio={:?}",
            node.id(),
            node.boot(),
            applied.group,
            applied.paused,
            applied.members,
            node.audio_status()
                .map(|status| (status.resolution, status.phase, status.problem))
        );
        eprintln!(
            "node[{index}] own_audio={own:?} overview={:?}",
            node.audio_status().map(|status| status.overview)
        );
        let links = node.links().unwrap().links();
        for (peer_index, peer) in nodes.iter().enumerate() {
            let link = links
                .by_peer(peer.id())
                .map(|link| (link.link, link.state, link.path, link.peer_boot, link.remote_address));
            let facts = node.device_facts(peer.id());
            eprintln!(
                "node[{index}]->node[{peer_index}] link={link:?} current_boot={:?}",
                if node.id() == peer.id() {
                    Some(node.boot())
                } else {
                    group.peer_boot(peer.id())
                }
            );
            eprintln!(
                "node[{index}]->node[{peer_index}] facts_boot={:?} facts_group={:?} fact_sequence={:?} audio_config={:?} observed={:?} pause={:?}",
                facts.as_ref().map(|facts| facts.boot()),
                facts.as_ref().map(|facts| facts.group()),
                facts.as_ref().map(|facts| facts.fact_sequence()),
                facts.as_ref().map(|facts| facts.audio_config()),
                facts.as_ref().map(|facts| facts.observed()),
                facts.as_ref().map(|facts| facts.pause())
            );
        }
    }
}

#[test]
fn removed_exit_drops_out_and_a_new_member_replaces_it_from_its_card() {
    crate::test_watchdog::run(
        module_path!(),
        "removed_exit_drops_out_and_a_new_member_replaces_it_from_its_card",
        Some(4),
        async {
            let old = start("exit-old", 0).await;
            let a = start("exit-peer-a", 0).await;
            let b = start("exit-peer-b", 0).await;
            common::pair(&a.node, &old.node).await;
            common::pair(&a.node, &b.node).await;
            common::active(&[&old.node, &a.node, &b.node]).await;
            // Peer a chooses old as the exit; old executes a's decision.
            choose_exit(&a.node, Some(old.node.id())).await;
            let chosen = a.node.audio_status().unwrap().resolution;
            assert_eq!(chosen.target(), Some(old.node.id()));
            wait_for_audio(
                "a's choice reaches every member",
                &[&old.node, &a.node, &b.node],
                |all| all.iter().all(|resolution| *resolution == chosen),
            )
            .await;
            old.node.shutdown().await;
            let removed = old.node.id();
            api(&a.node, move |node| node.remove_member(removed.into()))
                .await
                .unwrap();
            // An offline exit drops out of the overview: no exit at all, and
            // sources play locally again (no "selected but unavailable").
            wait_for_audio("removed exit drops out", &[&a.node, &b.node], |all| {
                all.iter().all(|resolution| *resolution == Resolution::None)
            })
            .await;
            // A newly joined member merges with the group (it holds no role).
            let fresh = start("exit-new", 0).await;
            common::pair(&fresh.node, &a.node).await;
            common::active(&[&a.node, &b.node, &fresh.node]).await;
            wait_for("new member merges with the group", 5., || {
                let status = fresh.node.audio_status().unwrap();
                status.overview.joined && status.resolution == Resolution::None
            })
            .await;
            assert_eq!(
                make_exit_via_card(&fresh.node).await,
                crate::device_cards::DeviceControlResult::Applied
            );
            wait_for_audio("new exit converges", &[&a.node, &b.node, &fresh.node], |all| {
                all.iter()
                    .all(|resolution| resolution.target() == Some(fresh.node.id()) && resolution.available())
            })
            .await;
            for node in [&a.node, &b.node, &fresh.node] {
                let version = node.audio_status().unwrap().resolution.version().unwrap();
                // Chosen after seeing the old choice: a newer HLC stamp.
                assert!(version > chosen.version().unwrap());
                assert_eq!(version.author, fresh.node.id());
                node.shutdown().await;
            }
        },
    );
}

/// The user on `node` chooses `exit` for the group, or turns the current
/// exit back to forward (a PC that leaves the exit forwards).
async fn choose_exit(node: &Node, exit: Option<oink_foundation::DeviceId>) {
    let audio = node.inner.audio.get().unwrap().clone();
    let (device, mode) = match exit {
        Some(exit) => (exit, Mode::Holder),
        None => (
            audio.status().resolution.target().expect("an exit to clear"),
            Mode::Shared,
        ),
    };
    let result = tokio::task::spawn_blocking(move || audio.set_audio_mode(device, Role::Speaker, mode))
        .await
        .unwrap();
    eprintln!(
        "AUDIO_EXIT_SELECTION author={:?} group={:?} device={device:?} mode={mode:?} result={result:?}",
        node.id(),
        node.group_id()
    );
    assert_eq!(result, Ok(dto::ControlOutcome::Applied));
}

/// Select `node` as the audio exit through its own device card, as the UI does.
/// The choice completes on this device; no other member is asked.
async fn make_exit_via_card(node: &Node) -> crate::device_cards::DeviceControlResult {
    let mut token = None;
    wait_for("make-exit action on this device's card", 5., || {
        node.refresh_device_cards();
        let cards = node.device_cards().unwrap();
        let card = cards.cards.iter().find(|card| card.is_self).unwrap();
        let Some(status) = &card.status else { return false };
        let crate::device_cards::CardAction::Invoke { token: current, .. } = &status.speaker.make_exit_action else {
            return false;
        };
        token = Some(current.clone());
        true
    })
    .await;
    let token = token.unwrap();
    let request = crate::device_controls::runtime::fresh_request_id();
    let reply = api(node, move |node| node.invoke_device_card_action(request, token))
        .await
        .unwrap();
    let crate::device_cards::DeviceControlReply::Completed(completed) = reply else {
        panic!("a route choice completes on the choosing device: {reply:?}")
    };
    completed.result
}

/// oinkvm/oink#32 (main 0.3.5) in the global audio configuration: a member
/// that is offline and that the clicking device never met blocks nothing; it
/// dropped out of the overview, so its old exit is no exit. When it restarts
/// holding the exit in its own configuration, it yields to the group's
/// online exit (F5, user 2026-11-11 A).
#[test]
fn offline_member_without_audio_record_does_not_block_exit_selection() {
    crate::test_watchdog::run(
        module_path!(),
        "offline_member_without_audio_record_does_not_block_exit_selection",
        Some(4),
        async {
            let a = start("issue32-a", 0).await;
            let d = start("issue32-d", 0).await;
            common::pair(&a.node, &d.node).await;
            // The member that later goes offline is the exit before it leaves.
            select_exit(&d.node).await;
            let old_exit = d.node.audio_status().unwrap().resolution;
            wait_for("old exit reaches the other member", 5., || {
                a.node.audio_status().unwrap().resolution == old_exit
            })
            .await;
            let offline = d.node.id();
            let port = d.node.network_info().unwrap().port;
            let Dev { node, _dir: dir, .. } = d;
            node.shutdown().await;
            // The clicking device joins while that member is offline.
            let w = start("issue32-w", 0).await;
            common::pair(&w.node, &a.node).await;
            let group = a.node.group_id().unwrap();
            assert_eq!(w.node.group_id(), Some(group));
            assert!(w.node.group().unwrap().members.iter().any(|m| m.device.id.0 == offline));
            // Both online devices show the real state: no exit (the offline
            // one dropped out). Neither waits on the offline member.
            wait_for("both online devices have no exit", 5., || {
                [&a.node, &w.node].iter().all(|node| {
                    let status = node.audio_status().unwrap();
                    status.overview.joined && status.resolution == Resolution::None
                })
            })
            .await;
            assert_eq!(
                make_exit_via_card(&w.node).await,
                crate::device_cards::DeviceControlResult::Applied
            );
            let selected = w.node.audio_status().unwrap().resolution;
            assert_eq!(selected.target(), Some(w.node.id()));
            assert!(selected.version().unwrap() > old_exit.version().unwrap());
            wait_for("both online devices agree on the new exit", 5., || {
                [&a.node, &w.node].iter().all(|node| {
                    let resolution = node.audio_status().unwrap().resolution;
                    resolution == selected && resolution.available()
                })
            })
            .await;
            // The offline member restarts with its persisted exit and yields
            // to the group's online exit (it falls back to forward).
            let d = start_in(dir, "issue32-d", port).await;
            let nodes = [&a.node, &w.node, &d.node];
            wait_for("returning member learns the current roster", 5., || {
                d.node
                    .group()
                    .unwrap()
                    .members
                    .iter()
                    .any(|m| m.device.id.0 == w.node.id())
            })
            .await;
            // Discovery is disabled in these fixtures; supply endpoint hints.
            for node in nodes {
                let endpoints = nodes
                    .iter()
                    .filter(|peer| peer.id() != node.id())
                    .map(|peer| {
                        let address = std::net::SocketAddr::from(([127, 0, 0, 1], peer.network_info().unwrap().port));
                        (peer.id(), address.to_string())
                    })
                    .collect::<Vec<_>>();
                api(node, move |node| {
                    for (device, address) in endpoints {
                        node.set_member_address(device.into(), address).unwrap();
                    }
                })
                .await;
            }
            common::active(&nodes).await;
            wait_for_audio(
                "returning member yields to the group's selection",
                &nodes,
                |resolutions| {
                    resolutions
                        .iter()
                        .all(|resolution| *resolution == selected && resolution.available())
                },
            )
            .await;
            let returned = d.node.audio_status().unwrap().overview;
            assert_eq!(returned.mode(d.node.id(), Role::Speaker), Some(Mode::Shared));
            for node in nodes {
                node.shutdown().await;
            }
        },
    );
}

/// Q1 / F7: the setter decides every mode. When the exit moves to another PC
/// the previous one forwards (its own configuration says so, no option 4);
/// a setter turning another PC's forward off is adopted and persisted there.
#[test]
fn a_pc_that_loses_the_exit_forwards_and_a_setter_changes_its_forward() {
    crate::test_watchdog::run(
        module_path!(),
        "a_pc_that_loses_the_exit_forwards_and_a_setter_changes_its_forward",
        Some(4),
        async {
            let a = start("q1-a", 0).await;
            let b = start("q1-b", 0).await;
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            let mode = |node: &Node, device| node.audio_status().unwrap().overview.mode(device, Role::Speaker);
            choose_exit(&a.node, Some(b.node.id())).await;
            wait_for("b is the exit everywhere", 5., || {
                [&a.node, &b.node]
                    .iter()
                    .all(|node| mode(node, b.node.id()) == Some(Mode::Holder))
            })
            .await;
            // a picks itself: b falls back to forward, seen the same everywhere.
            choose_exit(&a.node, Some(a.node.id())).await;
            wait_for("b forwards to the new exit", 5., || {
                [&a.node, &b.node].iter().all(|node| {
                    mode(node, b.node.id()) == Some(Mode::Shared) && mode(node, a.node.id()) == Some(Mode::Holder)
                })
            })
            .await;
            // a turns b's forward off; b adopts it and persists it.
            let audio = a.node.inner.audio.get().unwrap().clone();
            let device = b.node.id();
            let result = tokio::task::spawn_blocking(move || audio.set_audio_mode(device, Role::Speaker, Mode::Local))
                .await
                .unwrap();
            assert_eq!(result, Ok(dto::ControlOutcome::Applied));
            wait_for("b adopts local", 5., || {
                [&a.node, &b.node]
                    .iter()
                    .all(|node| mode(node, b.node.id()) == Some(Mode::Local))
            })
            .await;
            let (own, _) = crate::node::lock(&b.node.inner.config).owner().audio(b.node.id());
            assert!(!own.forward.on && !own.speaker_exit, "persisted on b");
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

async fn select_exit(node: &Node) {
    choose_exit(node, Some(node.id())).await;
}

#[test]
fn gain_command_publishes_authoritative_ui_event_without_observer_tick() {
    crate::test_watchdog::run(
        module_path!(),
        "gain_command_publishes_authoritative_ui_event_without_observer_tick",
        Some(4),
        gain_command_publishes_authoritative_ui_event_without_observer_tick_scenario(),
    );
}

async fn gain_command_publishes_authoritative_ui_event_without_observer_tick_scenario() {
    use crate::api::{ApiEvent, LiveViews};
    let a = start("gn-ui", 0).await;
    // Exercise the gain key's settings command, writer, owner and API subscriber
    // without native streams or desktop input. Active key eligibility is covered
    // by audio::link's synthetic capture and the router's focus tests.
    let audio = a.node.inner.audio.get().unwrap();
    audio.hold_periodic_reports().await;
    let initial = audio.status().settings.forwarding_volume_percent;
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let subscription = a.node.subscribe_views(
        Arc::new(move |event| {
            if let ApiEvent::Audio(status) = event
                && status.settings.forwarding_volume_percent != initial
            {
                let _ = send.send((Instant::now(), status.clone()));
            }
        }),
        LiveViews::default(),
    );
    let mut settings = audio.status().settings;
    settings.forwarding_volume_percent += 10;
    let configure = audio.clone();
    let key_at = Instant::now();
    let completion = tokio::task::spawn_blocking(move || configure.configure(settings.into()));
    completion.await.unwrap().unwrap();
    let (event_at, status) = receive
        .try_recv()
        .expect("gain event must be delivered before configuration replies");
    let elapsed = event_at.duration_since(key_at);
    eprintln!(
        "GAIN_TRACE gain_command_to_ui_event_ms={:.3}",
        elapsed.as_secs_f64() * 1000.0
    );
    assert_eq!(
        audio.status().settings,
        status.settings,
        "event is the installed backend value"
    );
    assert_eq!(
        crate::config::AgentConfig::load_from(a.node.data_dir())
            .unwrap()
            .settings
            .audio,
        status.settings
    );
    a.node.unsubscribe(subscription);
    a.node.shutdown().await;
}

#[test]
fn gain_command_reaches_another_devices_ui_in_one_m0_hop_without_an_exit() {
    crate::test_watchdog::run(
        module_path!(),
        "gain_command_reaches_another_devices_ui_in_one_m0_hop_without_an_exit",
        Some(4),
        gain_command_reaches_another_devices_ui_in_one_m0_hop_without_an_exit_scenario(),
    );
}

async fn gain_command_reaches_another_devices_ui_in_one_m0_hop_without_an_exit_scenario() {
    use crate::api::{ApiEvent, LiveViews};
    let a = start("gn-a", 0).await;
    let b = start("gn-b", 0).await;
    common::pair(&a.node, &b.node).await;
    let source = a.node.id();
    crate::test_watchdog::until("direct M0 state support", || {
        b.node
            .audio_status()
            .unwrap()
            .device_states
            .iter()
            .any(|r| r.device == source)
            && a.node
                .audio_status()
                .unwrap()
                .device_states
                .iter()
                .any(|r| r.device == b.node.id())
    })
    .await;
    a.node.inner.audio.get().unwrap().hold_periodic_reports().await;
    b.node.inner.audio.get().unwrap().hold_periodic_reports().await;
    let local_before = b.node.audio_status().unwrap().settings;
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let subscription = b.node.subscribe_views(
        Arc::new(move |event| {
            if let ApiEvent::Audio(status) = event
                && status
                    .device_states
                    .iter()
                    .any(|r| r.device == source && r.state.forwarding_volume_percent == 110)
            {
                let _ = send.send((Instant::now(), status.clone()));
            }
        }),
        LiveViews::default(),
    );
    let audio = a.node.inner.audio.get().unwrap();
    let mut settings = audio.status().settings;
    settings.forwarding_volume_percent = 110;
    let configure = audio.clone();
    let key_at = Instant::now();
    let completion = tokio::task::spawn_blocking(move || configure.configure(settings.into()));
    let (event_at, status) = receive
        .recv()
        .await
        .expect("direct remote gain event with both periodic publishers held");
    eprintln!(
        "GAIN_TRACE gain_command_to_remote_ui_event_ms={:.3}",
        event_at.duration_since(key_at).as_secs_f64() * 1000.0
    );
    completion.await.unwrap().unwrap();
    assert_eq!(
        status.settings, local_before,
        "a peer report does not change local preferences"
    );
    assert_eq!(audio.status().settings.forwarding_volume_percent, 110);
    let current = b.node.audio_status().unwrap();
    assert_eq!(
        current
            .device_states
            .iter()
            .map(|row| (row.device, &row.state))
            .collect::<Vec<_>>(),
        status
            .device_states
            .iter()
            .map(|row| (row.device, &row.state))
            .collect::<Vec<_>>()
    );
    b.node.unsubscribe(subscription);
    a.node.shutdown().await;
    b.node.shutdown().await;
}

#[test]
fn lifecycle_changes_reach_both_uis_without_periodic_report() {
    crate::test_watchdog::run(
        module_path!(),
        "lifecycle_changes_reach_both_uis_without_periodic_report",
        Some(4),
        lifecycle_changes_reach_both_uis_without_periodic_report_scenario(),
    );
}

async fn lifecycle_changes_reach_both_uis_without_periodic_report_scenario() {
    use crate::api::{ApiEvent, LiveViews};
    let a = start("phase-a", 0).await;
    let b = start("phase-b", 0).await;
    common::pair(&a.node, &b.node).await;
    let source = a.node.id();
    wait_for("audio state support", 5., || {
        b.node
            .audio_status()
            .unwrap()
            .device_states
            .iter()
            .any(|r| r.device == source)
    })
    .await;
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let subscriptions = [&a.node, &b.node]
        .into_iter()
        .enumerate()
        .map(|(index, node)| {
            let send = send.clone();
            node.subscribe_views(
                Arc::new(move |event| {
                    if let ApiEvent::Audio(status) = event
                        && status
                            .device_states
                            .iter()
                            .any(|r| r.device == source && r.state.phase == oink_audio::AudioPhase::Paused)
                    {
                        let _ = send.send((index, Instant::now()));
                    }
                }),
                LiveViews::default(),
            )
        })
        .collect::<Vec<_>>();
    let audio = a.node.inner.audio.get().unwrap().clone();
    audio.hold_periodic_reports().await;
    b.node.inner.audio.get().unwrap().hold_periodic_reports().await;
    let at = Instant::now();
    api(&a.node, |node| {
        node.set_local_paused(crate::device_controls::runtime::fresh_request_id(), true)
    })
    .await
    .unwrap();
    let mut latencies = [None, None];
    tokio::time::timeout(Duration::from_secs(5), async {
        while latencies.iter().any(Option::is_none) {
            let (index, when) = receive.recv().await.unwrap();
            latencies[index].get_or_insert(when.duration_since(at));
        }
    })
    .await
    .expect("both lifecycle updates arrive while periodic reports are held");
    for (index, node) in [&a.node, &b.node].into_iter().enumerate() {
        node.unsubscribe(subscriptions[index]);
    }
    a.node.shutdown().await;
    b.node.shutdown().await;
    eprintln!(
        "UI_LATENCY lifecycle_local_ms={:.3} lifecycle_remote_ms={:.3}",
        latencies[0].unwrap().as_secs_f64() * 1000.,
        latencies[1].unwrap().as_secs_f64() * 1000.
    );
}

/// Choose this node as the exit and clear it again; returns the speaker
/// register it then broadcasts.
pub(super) async fn withdraw_exit(node: &Node) -> dto::AudioRoute {
    select_exit(node).await;
    choose_exit(node, None).await;
    node.device_facts(node.id())
        .and_then(|facts| facts.audio_config().ok().flatten())
        .and_then(|config| config.speaker_exit().ok().flatten())
        .expect("broadcast speaker register")
}

#[test]
fn audio_settings_api_updates_owner_and_ui_before_reply() {
    crate::test_watchdog::run(
        module_path!(),
        "audio_settings_api_updates_owner_and_ui_before_reply",
        Some(4),
        async {
            use crate::api::{ApiEvent, LiveViews, Request};
            use std::sync::Mutex;
            let a = start("aud-a", 0).await;
            let b = start("aud-b", 0).await;
            common::pair(&a.node, &b.node).await;
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = events.clone();
            let subscription = a.node.subscribe_views(
                Arc::new(move |event| {
                    if let ApiEvent::Audio(status) = event {
                        sink.lock().unwrap().push(status.clone());
                    }
                }),
                LiveViews::default(),
            );
            let peer_local = b.node.audio_status().unwrap().settings;
            let mut settings = a.node.audio_status().unwrap().settings;
            // The forward switch is not a local setting (global audio
            // configuration): settings edits never change key 33. Pairing
            // returns before the groups merge and each side lists the other's
            // switches (one hop), so compare only against the converged
            // configuration, where both nodes list the same two devices.
            let devices = |node: &Node| {
                node.device_facts(node.id())
                    .and_then(|facts| facts.audio_config().ok().flatten())
                    .and_then(|config| config.devices().ok())
                    .map(|devices| devices.0)
                    .unwrap_or_default()
            };
            wait_for("both nodes publish the merged audio configuration", 5., || {
                let (mine, peer) = (devices(&a.node), devices(&b.node));
                mine.len() == 2 && peer.len() == 2 && mine.iter().all(|entry| peer.contains(entry))
            })
            .await;
            let audio_config = a.node.device_facts(a.node.id()).unwrap().audio_config().unwrap();
            for edit in 0..7 {
                match edit {
                    0 => settings.receive_output = Some("synthetic-output".into()),
                    1 => settings.keep_local_playback = true,
                    2 => settings.forwarding_volume_percent = 135,
                    3 => settings.capture_scope = oink_audio::CaptureScope::Global,
                    4 => settings.quality = oink_audio::negotiation::Quality::Lossless,
                    5 => settings.forwarding_muted = true,
                    6 => settings.forwarding_muted = false,
                    _ => unreachable!(),
                }
                events.lock().unwrap().clear();
                let requested = settings.clone();
                api(&a.node, move |node| {
                    crate::api::dispatch(
                        node,
                        Request::SetAudioSettings {
                            settings: requested.into(),
                        },
                    )
                })
                .await
                .unwrap();
                assert_eq!(a.node.audio_status().unwrap().settings, settings);
                assert!(events.lock().unwrap().iter().any(|status| status.settings == settings));
                let saved = crate::config::AgentConfig::load_from(a.node.data_dir()).unwrap();
                assert_eq!(saved.settings.audio, settings);
                let local = a.node.device_facts(a.node.id()).unwrap();
                assert_eq!(local.audio_config().unwrap(), audio_config);
                let preferences = local.preferences().unwrap();
                wait_for("peer receives owner audio preferences", 5., || {
                    b.node
                        .device_facts(a.node.id())
                        .is_some_and(|facts| facts.preferences().unwrap() == preferences)
                })
                .await;
                assert_eq!(b.node.audio_status().unwrap().settings, peer_local);
            }
            a.node.unsubscribe(subscription);
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}
