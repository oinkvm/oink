use super::*;
use oink_audio::claims::Resolution;
use oink_protocol::v1::{
    dto,
    schema::{WireDto, WireValue},
};

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
            wait_for_audio("one selected exit survives group merge", &nodes, |resolutions| {
                let winner = resolutions[0];
                winner.available()
                    && matches!(winner.owner(),Some(owner) if owner==a.node.id() || owner==b.node.id())
                    && resolutions.iter().all(|resolution| *resolution == winner)
            })
            .await;
            assert!(
                crate::node::lock(&b.node.inner.config)
                    .owner()
                    .claims(source_group)
                    .records
                    .iter()
                    .any(|record| record.owner == b.node.id() && record.claim.is_some())
            );
            tokio::join!(select_exit(&ap.node), select_exit(&bp.node));
            wait_for_audio(
                "concurrent exit selections converge across merged members",
                &nodes,
                |resolutions| {
                    let resolution = resolutions[0];
                    resolution.available()
                        && matches!(resolution.owner(), Some(owner) if owner == ap.node.id() || owner == bp.node.id())
                        && resolutions.iter().all(|current| *current == resolution)
                },
            )
            .await;
            for node in nodes {
                assert!(!node.audio_status().unwrap().settings.forwarding_enabled);
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
        let started = Instant::now();
        let end = started + Duration::from_secs(5);
        let now = std::cell::Cell::new(started);
        let ready = std::cell::Cell::new(false);
        let mut waiting = Box::pin(observe_until(end, || now.get(), || ready.get()));
        let pending = std::future::poll_fn(|context| {
            std::task::Poll::Ready(std::future::Future::poll(waiting.as_mut(), context).is_pending())
        })
        .await;
        assert!(pending, "the first false observation waits for a notification");
        ready.set(becomes_ready);
        now.set(end + Duration::from_millis(1));
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
        let (active_group, claims, local) = {
            let config = crate::node::lock(&node.inner.config);
            let owner = config.owner();
            (
                owner.active_group(),
                owner.claims(applied.group),
                owner.item(dto::LocalOption::AudioExitClaim),
            )
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
        eprintln!("node[{index}] local_option20={local:?} retained={claims:?}");
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
                "node[{index}]->node[{peer_index}] facts_boot={:?} facts_group={:?} fact_sequence={:?} preference20={:?} claim20={:?} watermark21={:?} observed={:?} pause={:?}",
                facts.as_ref().map(|facts| facts.boot()),
                facts.as_ref().map(|facts| facts.group()),
                facts.as_ref().map(|facts| facts.fact_sequence()),
                facts
                    .as_ref()
                    .map(|facts| facts.preferences().map(|items| items.map(|items| items
                        .0
                        .into_iter()
                        .filter(|item| item.option() == Ok(dto::LocalOption::AudioExitClaim))
                        .collect::<Vec<_>>()))),
                facts.as_ref().map(|facts| facts.audio_exit_claim()),
                facts.as_ref().map(|facts| facts.audio_claim_watermark()),
                facts.as_ref().map(|facts| facts.observed()),
                facts.as_ref().map(|facts| facts.pause())
            );
        }
    }
}

#[test]
fn reinstalled_member_can_replace_the_removed_audio_exit() {
    crate::test_watchdog::run(
        module_path!(),
        "reinstalled_member_can_replace_the_removed_audio_exit",
        Some(4),
        async {
            let old = start("exit-old", 0).await;
            let a = start("exit-peer-a", 0).await;
            let b = start("exit-peer-b", 0).await;
            common::pair(&a.node, &old.node).await;
            common::pair(&a.node, &b.node).await;
            common::active(&[&old.node, &a.node, &b.node]).await;
            wait_for("all audio owner baselines", 5., || {
                [&old.node, &a.node, &b.node]
                    .iter()
                    .all(|node| node.audio_status().unwrap().resolution != oink_audio::claims::Resolution::Unknown)
            })
            .await;
            // A counter above one makes the lost-history case independent of
            // the random identities' byte ordering.
            select_exit(&old.node).await;
            let first_exit = old.node.audio_status().unwrap().resolution;
            wait_for("first selection reaches both peers", 5., || {
                [&a.node, &b.node]
                    .iter()
                    .all(|node| node.audio_status().unwrap().resolution == first_exit)
            })
            .await;
            // Delay both ordinary owners after the first selection. A later
            // selection has the same owner ID but must not reuse that readiness.
            let gates = [a.node.inner.owner_gate.clone(), b.node.inner.owner_gate.clone()];
            let (held, holding) = tokio::sync::oneshot::channel();
            let (resume, resumed) = std::sync::mpsc::channel();
            let delayed = tokio::task::spawn_blocking(move || {
                let _guards = gates.iter().map(|gate| gate.lock().unwrap()).collect::<Vec<_>>();
                let _ = held.send(());
                let _ = resumed.recv();
            });
            holding.await.unwrap();
            let revision = crate::node::lock(&old.node.inner.config)
                .owner()
                .revision(dto::LocalOption::AudioExitClaim)
                .unwrap();
            let cleared = old
                .node
                .inner
                .audio
                .get()
                .unwrap()
                .set_claim(
                    oink_network::wire_identity::boot(old.node.boot()),
                    old.node.group_id().unwrap(),
                    false,
                    revision,
                )
                .await
                .unwrap();
            assert_eq!(cleared.outcome, dto::ControlOutcome::Applied);
            assert!(cleared.persisted);
            assert!(cleared.fact_sequence.is_some());
            select_exit(&old.node).await;
            let selected_exit = old.node.audio_status().unwrap().resolution;
            assert!(selected_exit.stamp().unwrap().counter >= 3);
            // Both consumers must apply this exact selection before its author
            // goes offline; the earlier selection cannot establish its history.
            let applied = wait_for("final exit selection reaches both peers", 5., || {
                [&a.node, &b.node]
                    .iter()
                    .all(|node| node.audio_status().unwrap().resolution == selected_exit)
            });
            tokio::pin!(applied);
            let readiness = std::future::poll_fn(|context| {
                std::task::Poll::Ready(std::future::Future::poll(applied.as_mut(), context))
            })
            .await;
            assert!(
                readiness.is_pending(),
                "an older selection must not satisfy the final selection barrier"
            );
            resume.send(()).unwrap();
            delayed.await.unwrap();
            applied.await;
            old.node.shutdown().await;
            let removed = old.node.id();
            api(&a.node, move |node| node.remove_member(removed.into()))
                .await
                .unwrap();
            wait_for("removed exit leaves both rosters", 5., || {
                [&a.node, &b.node].iter().all(|node| {
                    !node
                        .group()
                        .unwrap()
                        .members
                        .iter()
                        .any(|member| member.device.id.0 == removed)
                })
            })
            .await;
            // Force the legal observation order: the new member receives the
            // historical watermark and its own withdrawal before the last
            // current member's AudioExitClaim baseline. A matching local owner
            // revision alone must not advertise a selection that returns Busy.
            let gate = b.node.inner.owner_gate.clone();
            let (held, holding) = tokio::sync::oneshot::channel();
            let (resume_baseline, resumed) = std::sync::mpsc::channel::<()>();
            let baseline_worker = tokio::task::spawn_blocking(move || {
                let _guard = gate.lock().unwrap();
                let _ = held.send(());
                let _ = resumed.recv();
            });
            holding.await.unwrap();
            api(&b.node, |node| {
                let mut preferences = crate::node::lock(&node.inner.config).owner().preferences();
                preferences.retain(|item| item.option() != Ok(dto::LocalOption::AudioExitClaim));
                let group = node.inner.group.get().unwrap();
                let pause = group.local_facts().pause().unwrap().unwrap();
                group.publish_local_owner(preferences, pause).unwrap();
            })
            .await;
            let fresh = start("exit-new", 0).await;
            let group = a.node.group_id().unwrap();
            common::pair(&fresh.node, &a.node).await;
            assert_eq!(fresh.node.group_id(), Some(group));
            common::active(&[&a.node, &b.node, &fresh.node]).await;
            wait_for("historical ordering witness reaches new member", 5., || {
                fresh.node.audio_status().unwrap().resolution == oink_audio::claims::Resolution::Unknown
                    && crate::node::lock(&fresh.node.inner.config)
                        .owner()
                        .claims(group)
                        .seen_counter
                        >= 3
            })
            .await;
            // Recreate the already-installed app's durable colliding claim. The
            // new owner must fence this old selection, then let the user select
            // again above history that exists only on the surviving peers.
            {
                let mut config = crate::node::lock(&fresh.node.inner.config);
                config
                    .update_owner(|owner| {
                        let mut records = owner.claims(group);
                        let local = records.records.iter_mut().find(|r| r.owner == fresh.node.id()).unwrap();
                        local.revision += 1;
                        local.claim = Some(
                            dto::AudioExitClaim::from_raw(
                                [
                                    (
                                        1,
                                        oink_audio::claims::Stamp {
                                            counter: 1,
                                            owner: fresh.node.id(),
                                        }
                                        .to_wire()
                                        .write(),
                                    ),
                                    (2, true.write()),
                                ]
                                .into(),
                            )
                            .unwrap(),
                        );
                        owner.commit_claims(group, fresh.node.id(), records)
                    })
                    .unwrap();
            }
            let audio = fresh.node.inner.audio.get().unwrap().clone();
            let settings = audio.status().settings;
            tokio::task::spawn_blocking(move || audio.configure(settings.into()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                fresh.node.audio_status().unwrap().resolution,
                oink_audio::claims::Resolution::Unknown
            );
            wait_for("obsolete installed selection yields before user action", 5., || {
                fresh
                    .node
                    .device_facts(fresh.node.id())
                    .and_then(|facts| facts.audio_exit_claim().ok().flatten())
                    .is_some_and(|claim| claim.claimed() == Ok(false))
            })
            .await;
            let owner_revision = crate::node::lock(&fresh.node.inner.config)
                .owner()
                .revision(dto::LocalOption::AudioExitClaim)
                .unwrap();
            fresh.node.refresh_device_cards();
            let cards = fresh.node.device_cards().unwrap();
            let card = cards.cards.iter().find(|card| card.is_self).unwrap();
            assert_eq!(
                card.status.as_ref().unwrap().speaker.make_exit_action,
                crate::device_cards::CardAction::unavailable(crate::device_cards::CardActionUnavailable::StateUnknown),
                "selection must wait for the last current member's baseline"
            );
            resume_baseline.send(()).unwrap();
            baseline_worker.await.unwrap();
            // Facts and device cards are separate asynchronous observations.
            // The withdrawal's Facts20 can precede its card; selecting an older
            // cached token correctly refuses StaleRevision instead of applying.
            // Baseline completion must also enable the action at this revision.
            let mut token = None;
            wait_for("audio selection ready at yielded claim revision", 5., || {
                let cards = fresh.node.device_cards().unwrap();
                let card = cards.cards.iter().find(|card| card.is_self).unwrap();
                let Some(status) = &card.status else { return false };
                let crate::device_cards::CardAction::Invoke { token: current, .. } = &status.speaker.make_exit_action
                else {
                    return false;
                };
                let binding = crate::node::lock(&fresh.node.inner.cards)
                    .tokens()
                    .read(current)
                    .unwrap();
                if binding.owner.revision != owner_revision {
                    return false;
                }
                token = Some(current.clone());
                true
            })
            .await;
            let token = token.unwrap();
            let request = crate::device_controls::runtime::fresh_request_id();
            let expected = request.clone();
            let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
            let subscription = fresh.node.subscribe(Arc::new(move |event| {
                if let crate::api::ApiEvent::DeviceControlCompleted(result) = event
                    && result.request_id == expected
                {
                    let _ = send.send(result.clone());
                }
            }));
            let reply = api(&fresh.node, move |node| node.invoke_device_card_action(request, token))
                .await
                .unwrap();
            let completed = match reply {
                crate::device_cards::DeviceControlReply::Completed(completed) => completed,
                crate::device_cards::DeviceControlReply::Accepted(_) => {
                    tokio::time::timeout(Duration::from_secs(5), receive.recv())
                        .await
                        .expect("exit command completion")
                        .unwrap()
                }
            };
            fresh.node.unsubscribe(subscription);
            assert_eq!(
                completed.result,
                crate::device_cards::DeviceControlResult::Applied,
                "exit selection completion at owner revision {owner_revision}: {completed:?}"
            );
            wait_for("new exit claim converges after reinstall and join", 5., || {
                [&a.node, &b.node, &fresh.node]
                    .iter()
                    .all(|node| node.audio_status().unwrap().resolution.owner() == Some(fresh.node.id()))
            })
            .await;
            for node in [&a.node, &b.node, &fresh.node] {
                assert!(!node.audio_status().unwrap().settings.forwarding_enabled);
                assert!(node.audio_status().unwrap().resolution.stamp().unwrap().counter > 3);
                node.shutdown().await;
            }
        },
    );
}

async fn select_exit(node: &Node) {
    wait_for("complete audio baseline before selecting exit", 5., || {
        let records = crate::node::lock(&node.inner.config)
            .owner()
            .claims(node.group_id().unwrap())
            .records;
        node.group()
            .unwrap()
            .members
            .iter()
            .all(|member| records.iter().any(|record| record.owner == member.device.id.0))
    })
    .await;
    let revision = crate::node::lock(&node.inner.config)
        .owner()
        .revision(dto::LocalOption::AudioExitClaim)
        .unwrap();
    let result = node
        .inner
        .audio
        .get()
        .unwrap()
        .set_claim(
            oink_network::wire_identity::boot(node.boot()),
            node.group_id().unwrap(),
            true,
            revision,
        )
        .await;
    eprintln!(
        "AUDIO_EXIT_SELECTION owner={:?} group={:?} expected_revision={revision} result={result:?}",
        node.id(),
        node.group_id()
    );
    let applied = result.unwrap();
    assert_eq!(applied.outcome, dto::ControlOutcome::Applied);
    assert!(applied.persisted);
    assert!(applied.fact_sequence.is_some());
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

pub(super) async fn withdraw_exit(node: &Node) -> dto::AudioExitClaim {
    let audio = node.inner.audio.get().unwrap();
    let revision = crate::node::lock(&node.inner.config)
        .owner()
        .revision(dto::LocalOption::AudioExitClaim)
        .unwrap();
    let acquired = audio
        .set_claim(
            oink_network::wire_identity::boot(node.boot()),
            node.group_id().unwrap(),
            true,
            revision,
        )
        .await
        .unwrap();
    assert_eq!(acquired.outcome, dto::ControlOutcome::Applied);
    let revision = acquired.current.revision().unwrap();
    let applied = audio
        .set_claim(
            oink_network::wire_identity::boot(node.boot()),
            node.group_id().unwrap(),
            false,
            revision,
        )
        .await
        .unwrap();
    assert_eq!(applied.outcome, dto::ControlOutcome::Applied);
    assert!(applied.persisted);
    assert!(applied.fact_sequence.is_some());
    dto::AudioExitClaim::read(&applied.current.value().unwrap()).unwrap()
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
            // This scenario exercises disabling then re-enabling an opted-in
            // source; fresh installations now start with forwarding disabled.
            api(&a.node, |node| {
                node.set_audio_settings(
                    serde_json::from_value(serde_json::json!({"forwarding_enabled": true})).unwrap(),
                )
                .unwrap();
            })
            .await;
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
            let claim = a.node.device_facts(a.node.id()).unwrap().audio_exit_claim().unwrap();
            for edit in 0..8 {
                match edit {
                    0 => settings.receive_output = Some("synthetic-output".into()),
                    1 => settings.keep_local_playback = true,
                    2 => settings.forwarding_enabled = false,
                    3 => settings.forwarding_volume_percent = 135,
                    4 => settings.capture_scope = oink_audio::CaptureScope::Global,
                    5 => settings.preferred_format = oink_audio::StreamFormat::Pcm16_2p5ms,
                    6 => settings.forwarding_muted = true,
                    7 => {
                        settings.forwarding_enabled = true;
                        // Enabling forwarding atomically clears its previous mute.
                        settings.forwarding_muted = false;
                    }
                    _ => unreachable!(),
                }
                events.lock().unwrap().clear();
                let mut requested = settings.clone();
                if edit == 7 {
                    requested.forwarding_muted = true;
                }
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
                assert_eq!(local.audio_exit_claim().unwrap(), claim);
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
