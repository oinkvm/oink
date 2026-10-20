//! Real configuration owners and C0 links; no native desktop input or installed app.
use super::*;
use crate::config_module::runtime_tests::{display, ready, same_group, start, stop, until};
use oink_display::layout::SizeSource;
use oink_foundation::Size;
use oink_protocol::v1::dto::LayoutSource;

fn owner_settings(
    node: &crate::config_module::runtime_tests::Running,
    edit: impl FnOnce(&mut crate::config::Settings),
) {
    let _gate = lock(&node.gate);
    let preferences = {
        let mut config = lock(&node.config);
        config.update(edit).unwrap();
        config.owner().preferences()
    };
    node.group
        .publish_local_owner(preferences, node.group.local_facts().pause().unwrap().unwrap())
        .unwrap();
}
#[test]
fn layout_changes_cover_peer_facts_geometry_settings_membership_and_router_without_trace() {
    crate::test_watchdog::run(
        module_path!(),
        "layout_changes_cover_peer_facts_geometry_settings_membership_and_router_without_trace",
        Some(2),
        changes(),
    );
}
async fn changes() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    let mut recorders = Vec::new();
    let mut directories = Vec::new();
    for node in &nodes {
        let dir = tempfile::tempdir().unwrap();
        let recorder = super::tests::isolated(
            dir.path().into(),
            node.links.id(),
            Some(node.group.applied().group),
            true,
        )
        .unwrap();
        recorder.wait_recovery();
        node.group.observe_layout(&recorder).unwrap();
        recorders.push(recorder);
        directories.push(dir);
    }
    for (index, node) in nodes.iter().enumerate() {
        let mut info = display("PRIVATE-SCREEN");
        info.name = "PRIVATE-NAME".into();
        info.pixel_width = 200 + index as u32;
        info.reference_extent = Some(Size::new(300. / 686., 200. / 686.));
        node.group.initialize_displays(vec![info]).unwrap();
        assert!(
            until(|| nodes
                .iter()
                .all(|n| n.group.applied().layout.screens.len() == index + 1))
            .await
        );
    }
    let me = nodes[0].links.id();
    let peer = nodes[1].links.id();
    nodes[0]
        .group
        .edit_layout(
            Box::new(move |layout| {
                let screen = layout.screens.iter_mut().find(|s| s.id.device == peer).unwrap();
                screen.translate(31., 13.);
                screen.size_source = SizeSource::Manual;
            }),
            LayoutSource::Manual,
        )
        .unwrap();
    let target = nodes[0].group.applied().version.clone();
    assert!(until(|| nodes[1].group.applied().version == target).await);
    owner_settings(&nodes[1], |s| s.screen_preferences.device_gain = 1.25);
    assert!(
        until(|| nodes[0]
            .group
            .applied()
            .owners
            .get(&peer)
            .is_some_and(|p| p.device_gain == 1.25))
        .await
    );
    owner_settings(&nodes[1], |s| s.screen_shared = false);
    assert!(
        until(|| nodes[0]
            .group
            .applied()
            .owners
            .get(&peer)
            .is_some_and(|p| !p.screen_shared))
        .await
    );
    owner_settings(&nodes[1], |s| {
        s.screen_preferences.screens = vec![crate::config::ScreenPreference {
            remote_gain: 1.0,
            display: "PRIVATE-SCREEN".into(),
            enabled: false,
            gain: 1.0,
        }]
    });
    assert!(
        until(|| nodes[0]
            .group
            .applied()
            .owners
            .get(&peer)
            .is_some_and(|p| p.screens.get("PRIVATE-SCREEN").is_some_and(|s| !s.enabled)))
        .await
    );
    let config = nodes[0].group.clone();
    // Even an explicitly disabled InputTrace must publish actual router gates.
    let trace = input_trace::InputTrace::new(false);
    trace.config.set(config.clone()).unwrap();
    let router = config.router_layout().load();
    let actual = router.topology.with_online([me].into_iter().collect());
    trace.topology(
        &actual,
        router.group,
        router.layout.clone(),
        router.owners.clone(),
        router.version.clone(),
        Some([me].into_iter().collect()),
    );
    trace.drain(&recorders[0]);
    config.flush_layout_diagnostics();
    let anon = recorders[0].anonymizer();
    let current = config.diagnostic_layout(&anon).unwrap();
    assert_eq!(current.compiled.stage, layout::CompilationStage::Router);
    assert!(
        current
            .owner_preferences
            .iter()
            .any(|p| p.device_alias == anon.device(peer)
                && p.option == 6
                && p.effective.scalars.get("enabled") == Some(&layout::Scalar::Flag(false)))
    );
    let saved_peer = current.saved.iter().find(|s| s.device == anon.device(peer)).unwrap();
    assert!(!saved_peer.enabled);
    assert!(!current.compiled.screens.iter().any(|s| s.device == anon.device(peer)));
    assert!(
        current
            .compiled
            .sources
            .iter()
            .any(|s| s.device == anon.device(peer) && s.reference_per_native_unit.is_some())
    );
    assert!(
        current
            .compiled
            .source_devices
            .iter()
            .any(|d| d.device == anon.device(peer) && d.requires_target)
    );
    assert_eq!(current.compiled.input_online, Some(vec![anon.device(me)]));
    assert!(!current.compiled.edges.is_empty());
    // A repeat topology snapshot advances neither its sequence nor the layout record.
    let sequence = current.sequence;
    trace.disable_for_test();
    trace.topology(
        &actual,
        router.group,
        router.layout.clone(),
        router.owners.clone(),
        router.version.clone(),
        Some([me].into_iter().collect()),
    );
    trace.drain(&recorders[0]);
    config.flush_layout_diagnostics();
    assert_eq!(config.diagnostic_layout(&anon).unwrap().sequence, sequence);
    // Network/facts telemetry versions alone must not generate layout traffic.
    let facts = nodes[0].group.local_facts();
    nodes[0]
        .group
        .publish_local_owner(facts.preferences().unwrap().unwrap().0, facts.pause().unwrap().unwrap())
        .unwrap();
    assert_eq!(config.diagnostic_layout(&anon).unwrap().sequence, sequence);
    nodes[1].group.sleep();
    assert!(
        until(|| !nodes[0]
            .group
            .members()
            .members
            .iter()
            .find(|m| m.device == peer)
            .unwrap()
            .online)
        .await
    );
    nodes[0].group.remove_member(peer).unwrap();
    let pending_router = config.diagnostic_layout(&anon).unwrap();
    assert!(
        pending_router
            .compiled
            .sources
            .iter()
            .any(|s| s.device == anon.device(peer))
    );
    assert!(
        pending_router
            .devices
            .iter()
            .any(|d| d.device == anon.device(peer) && !d.displays.is_empty()),
        "facts for a still-installed compiled source must remain complete while membership catches up"
    );
    assert!(
        pending_router
            .devices
            .iter()
            .any(|d| d.device == anon.device(peer) && !d.saved_member && !d.applied_member)
    );
    let applied_router = config.router_layout().load();
    trace.topology(
        &applied_router.topology.with_online([me].into_iter().collect()),
        applied_router.group,
        applied_router.layout.clone(),
        applied_router.owners.clone(),
        applied_router.version.clone(),
        Some([me].into_iter().collect()),
    );
    trace.drain(&recorders[0]);
    config.flush_layout_diagnostics();
    assert!(
        !config
            .diagnostic_layout(&anon)
            .unwrap()
            .devices
            .iter()
            .any(|d| d.device == anon.device(peer)),
        "retained source facts end when neither saved/applied nor actual router references them"
    );
    for (index, recorder) in recorders.iter().enumerate() {
        nodes[index].group.flush_layout_diagnostics();
        recorder.configuration(&nodes[index].group);
        let saving = recorder.clone();
        let bundle = tokio::task::spawn_blocking(move || saving.save(Reason::Manual))
            .await
            .unwrap()
            .unwrap();
        let rows = super::tests::timeline(&bundle.path, "merged.ndjson");
        let changes: Vec<layout::LayoutChange> = rows
            .iter()
            .filter_map(|r| r["event"].get("Topology"))
            .map(|r| serde_json::from_value(r.clone()).unwrap())
            .collect();
        for kind in [
            layout::ChangeKind::Arrangement,
            layout::ChangeKind::Settings,
            layout::ChangeKind::DisplayFacts,
        ] {
            assert!(
                changes.iter().any(|c| c.before.is_some() && c.kinds.contains(&kind)),
                "device {index}: missing {kind:?}"
            );
        }
        if index == 0 {
            for kind in [
                layout::ChangeKind::Membership,
                layout::ChangeKind::Availability,
                layout::ChangeKind::CompiledTopology,
            ] {
                assert!(
                    changes.iter().any(|c| c.before.is_some() && c.kinds.contains(&kind)),
                    "missing {kind:?}"
                );
            }
        }
        for c in &changes {
            if let Some(before) = &c.before {
                assert!(c.after.sequence > before.sequence);
            }
        }
        let encoded = serde_json::to_string(&changes).unwrap();
        assert!(!encoded.contains("PRIVATE"));
        assert!(!encoded.contains(&me.to_hex()));
        assert!(!encoded.contains(&peer.to_hex()));
        assert!(
            !nodes[index]
                .group
                .diagnostic_excerpt()
                .to_string()
                .contains(&peer.to_hex())
        );
        recorder.shutdown();
    }
    stop(&nodes).await;
}

#[test]
fn suspended_layout_changes_are_recorded_but_explicit_diagnostics_off_is_respected() {
    crate::test_watchdog::run(
        module_path!(),
        "suspended_layout_changes_are_recorded_but_explicit_diagnostics_off_is_respected",
        Some(2),
        async {
            let nodes = start(same_group(1)).await;
            let config = &nodes[0].group;
            config.initialize_displays(vec![display("PRIVATE")]).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let recorder = super::tests::isolated_suspended(
                dir.path().into(),
                nodes[0].links.id(),
                Some(config.applied().group),
                true,
                true,
            )
            .unwrap();
            recorder.wait_recovery();
            config.observe_layout(&recorder).unwrap();
            config
                .edit_layout(Box::new(|l| l.screens[0].translate(1., 0.)), LayoutSource::Manual)
                .unwrap();
            let saving = recorder.clone();
            let bundle = tokio::task::spawn_blocking(move || saving.save(Reason::Manual))
                .await
                .unwrap()
                .unwrap();
            let changes: Vec<_> = super::tests::timeline(&bundle.path, "merged.ndjson")
                .into_iter()
                .filter(|r| r["event"].get("Topology").is_some())
                .collect();
            assert_eq!(changes.len(), 3);
            assert!(
                changes.iter().any(|r| r["event"]["Topology"]["after"]["version"]
                    != r["event"]["Topology"]["after"]["applied_version"]),
                "durable acceptance is visible before application"
            );
            recorder.set_enabled(false).unwrap();
            config
                .edit_layout(Box::new(|l| l.screens[0].translate(1., 0.)), LayoutSource::Manual)
                .unwrap();
            assert!(recorder.inner.queue.load().is_empty());
            recorder.shutdown();
            stop(&nodes).await;
        },
    );
}

#[test]
fn group_transition_reprojects_both_sides_and_survives_recorder_rekey() {
    crate::test_watchdog::run(
        module_path!(),
        "group_transition_reprojects_both_sides_and_survives_recorder_rekey",
        Some(2),
        async {
            let nodes = start(same_group(2)).await;
            assert!(until(|| ready(&nodes)).await);
            let config = &nodes[0].group;
            config.initialize_displays(vec![display("PRIVATE")]).unwrap();
            let old_group = config.applied().group;
            let dir = tempfile::tempdir().unwrap();
            let recorder =
                super::tests::isolated(dir.path().into(), nodes[0].links.id(), Some(old_group), true).unwrap();
            recorder.wait_recovery();
            config.observe_layout(&recorder).unwrap();
            let trace = input_trace::InputTrace::new(false);
            trace.config.set(config.clone()).unwrap();
            let old_router = config.router_layout().load();
            trace.topology(
                &old_router.topology,
                old_router.group,
                old_router.layout.clone(),
                old_router.owners.clone(),
                old_router.version.clone(),
                None,
            );
            trace.drain(&recorder);
            config.flush_layout_diagnostics();
            config.leave_group().unwrap();
            let new_group = config.applied().group;
            assert_ne!(new_group, old_group);
            // Production Collector::group_changed performs this same ordered handoff.
            recorder.rekey(nodes[0].links.id(), Some(new_group)).unwrap();
            config.observe_layout(&recorder).unwrap();
            recorder.configuration(config);
            let saving = recorder.clone();
            let bundle = tokio::task::spawn_blocking(move || saving.save(Reason::Manual))
                .await
                .unwrap()
                .unwrap();
            let rows = super::tests::timeline(&bundle.path, "merged.ndjson");
            let changes: Vec<layout::LayoutChange> = rows
                .iter()
                .filter_map(|r| r["event"].get("Topology"))
                .map(|v| serde_json::from_value(v.clone()).unwrap())
                .collect();
            let transition = changes
                .iter()
                .find(|c| c.kinds.contains(&layout::ChangeKind::GroupChanged))
                .unwrap();
            // The signed departure is persisted in the old domain first. The
            // installed old router still retains the departing source's facts.
            assert_eq!(transition.before.as_ref().unwrap().saved_members.len(), 1);
            assert_eq!(transition.after.saved_members.len(), 1);
            assert_eq!(transition.after.devices.len(), 1);
            let new_id = recorder.anonymizer().device(nodes[0].links.id());
            let before = transition.before.as_ref().unwrap();
            assert!(
                before
                    .devices
                    .iter()
                    .any(|d| d.device == new_id && !d.saved_member && !d.applied_member && !d.displays.is_empty()),
                "old installed router source retains its facts during the membership transition"
            );
            assert!(
                before
                    .devices
                    .iter()
                    .any(|d| d.device == recorder.anonymizer().device(nodes[1].links.id()) && d.saved_member)
            );
            assert_eq!(transition.after.devices[0].device, new_id);
            assert!(
                !transition.after.compiled.current_group,
                "actual router still runs its old domain until its next installation"
            );
            let router = config.router_layout().load();
            trace.topology(
                &router.topology,
                router.group,
                router.layout.clone(),
                router.owners.clone(),
                router.version.clone(),
                None,
            );
            trace.drain(&recorder);
            config.flush_layout_diagnostics();
            assert!(
                config
                    .diagnostic_layout(&recorder.anonymizer())
                    .unwrap()
                    .compiled
                    .current_group
            );
            let encoded = serde_json::to_string(&changes).unwrap();
            assert!(!encoded.contains(&Anonymizer::new(old_group).device(nodes[0].links.id())));
            assert!(!encoded.contains("PRIVATE"));
            recorder.shutdown();
            stop(&nodes).await;
        },
    );
}

#[test]
fn accepted_layout_is_recorded_before_failed_application_and_after_retry() {
    crate::test_watchdog::run(
        module_path!(),
        "accepted_layout_is_recorded_before_failed_application_and_after_retry",
        Some(2),
        async {
            let nodes = start(same_group(1)).await;
            let config = &nodes[0].group;
            config.initialize_displays(vec![display("PRIVATE")]).unwrap();
            assert!(until(|| !config.applied().layout.screens.is_empty()).await);
            let dir = tempfile::tempdir().unwrap();
            let recorder = super::tests::isolated(
                dir.path().into(),
                nodes[0].links.id(),
                Some(config.applied().group),
                true,
            )
            .unwrap();
            recorder.wait_recovery();
            config.observe_layout(&recorder).unwrap();
            crate::config_module::runtime_tests::set_application_failure(&nodes[0], true);
            assert!(
                config
                    .edit_layout(Box::new(|l| l.screens[0].translate(19., 0.)), LayoutSource::Manual)
                    .is_err()
            );
            let pending = config.diagnostic_layout(&recorder.anonymizer()).unwrap();
            assert!(pending.version.counter > pending.applied_version.counter);
            crate::config_module::runtime_tests::set_application_failure(&nodes[0], false);
            assert!(until(|| config.applied().version.counter() == Ok(pending.version.counter)).await);
            config.flush_layout_diagnostics();
            let saving = recorder.clone();
            let bundle = tokio::task::spawn_blocking(move || saving.save(Reason::Manual))
                .await
                .unwrap()
                .unwrap();
            let rows = super::tests::timeline(&bundle.path, "merged.ndjson");
            let changes: Vec<layout::LayoutChange> = rows
                .iter()
                .filter_map(|r| r["event"].get("Topology"))
                .map(|v| serde_json::from_value(v.clone()).unwrap())
                .collect();
            assert!(
                changes
                    .iter()
                    .any(|c| c.kinds.contains(&layout::ChangeKind::Arrangement)
                        && c.after.version.counter > c.after.applied_version.counter)
            );
            assert!(changes.iter().any(|c| {
                c.kinds.contains(&layout::ChangeKind::Applied)
                    && c.before
                        .as_ref()
                        .is_some_and(|b| b.version.counter > b.applied_version.counter)
                    && c.after.version == c.after.applied_version
            }));
            recorder.shutdown();
            stop(&nodes).await;
        },
    );
}

#[test]
fn local_display_facts_are_recorded_at_observation_before_network_publication() {
    crate::test_watchdog::run(
        module_path!(),
        "local_display_facts_are_recorded_at_observation_before_network_publication",
        Some(2),
        async {
            let nodes = start(same_group(1)).await;
            let config = &nodes[0].group;
            let dir = tempfile::tempdir().unwrap();
            let recorder = super::tests::isolated(
                dir.path().into(),
                nodes[0].links.id(),
                Some(config.applied().group),
                true,
            )
            .unwrap();
            recorder.wait_recovery();
            config.observe_layout(&recorder).unwrap();
            let mut info = display("PRIVATE");
            info.pixel_width = 1600;
            info.pixel_height = 1200;
            let current = crate::config_module::runtime_tests::observed_display_layout_before_fact_publication(
                config,
                vec![info],
            );
            assert_eq!(
                current.devices[0].displays.first().map(|d| d.scale),
                Some(2.),
                "current layout must include locally observed facts without waiting for the next C0 publication"
            );
            let saving = recorder.clone();
            let bundle = tokio::task::spawn_blocking(move || saving.save(Reason::Manual))
                .await
                .unwrap()
                .unwrap();
            let changes: Vec<layout::LayoutChange> = super::tests::timeline(&bundle.path, "merged.ndjson")
                .into_iter()
                .filter_map(|r| {
                    r["event"]
                        .get("Topology")
                        .map(|v| serde_json::from_value(v.clone()).unwrap())
                })
                .collect();
            assert!(
                changes
                    .iter()
                    .any(|c| c.kinds.contains(&layout::ChangeKind::DisplayFacts)
                        && c.after.devices[0].displays.first().is_some_and(|d| d.scale == 2.))
            );
            recorder.shutdown();
            stop(&nodes).await;
        },
    );
}
