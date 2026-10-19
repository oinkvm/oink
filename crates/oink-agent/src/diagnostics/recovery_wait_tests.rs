//! Device-free recovery collection tests over isolated loopback links.
use super::*;
use std::path::Path;
fn context(links: LinkLayer, config: crate::config_module::ConfigModule) -> Context {
    Context {
        links,
        config,
        runtime: tokio::runtime::Handle::current(),
        timeout: TIMEOUT,
        deadline_gate: None,
        recovery_wait_entered: None,
        node: None,
    }
}
fn start_recorder(
    dir: PathBuf,
    links: &LinkLayer,
    config: &crate::config_module::ConfigModule,
    collection: Option<Context>,
) -> anyhow::Result<Recorder> {
    // These fixtures own their rows; production Node tests exercise process trace registration.
    Recorder::start_inner(
        dir,
        links.id(),
        Some(config.applied().group),
        true,
        false,
        clock::Clock::system(),
        collection,
        None,
        false,
    )
}
fn manifest(bundle: &BundleInfo) -> serde_json::Value {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
    serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap()
}
async fn incoming(port: &mut ModulePort) -> oink_network::link::InboundLow {
    loop {
        if let Some(ModuleEvent::InboundLow(operation)) = port.next().await {
            return *operation;
        }
    }
}
// Isolated real LinkLayers/ConfigModules with no remembered addresses. Only
// the test explicitly connects them; no installed backend or desktop is used.
struct RecoveryPeer {
    _dir: tempfile::TempDir,
    links: LinkLayer,
    config: crate::config_module::ConfigModule,
}
async fn recovery_peers(count: u8) -> Vec<RecoveryPeer> {
    use crate::config_module::{ConfigDeps, ConfigModule, group_store};
    use oink_foundation::{realtime::Tier, session::Boot};
    use oink_network::group_state::State;
    use oink_network::{identity::Identity, link::LinkConfig};
    struct Application;
    impl crate::config_module::ConfigApplication for Application {
        fn apply_state(&self, _: &group_store::StoredGroupState) -> Result<(), String> {
            Ok(())
        }
    }
    let identities = (71..71 + count)
        .map(|seed| Arc::new(Identity::from_seed([seed; 32])))
        .collect::<Vec<_>>();
    let mut shared = State::create(&identities[0], "synthetic", "test-os", 100).unwrap();
    for id in &identities[1..] {
        shared = shared
            .with_roster(
                &shared
                    .roster()
                    .add(&identities[0], id.id(), "synthetic", "test-os", 100)
                    .unwrap(),
            )
            .unwrap();
    }
    let mut peers = Vec::new();
    for (index, identity) in identities.into_iter().enumerate() {
        let state = group_store::StoredGroupState::new(identity.id(), shared.wire().clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        group_store::save(&dir.path().join(group_store::STATE_FILE), &state).unwrap();
        let network = oink_test_support::network_config(dir.path().into(), "synthetic", 0);
        let (links, port) = LinkLayer::start(LinkConfig {
            profile: Node::offer_profile(),
            identity: identity.clone(),
            boot: Boot {
                device_id: identity.id(),
                nonce: [index as u8; 16],
            },
            bind: (network.bind_ip, network.port).into(),
            tier: Tier::Elevated,
        })
        .await
        .unwrap();
        let config = ConfigModule::start_with_config(
            ConfigDeps {
                config: Arc::new(Mutex::new(
                    crate::config::AgentConfig::load_from_with_device_name(dir.path(), "synthetic".into()).unwrap(),
                )),
                owner_gate: Default::default(),
                data_dir: dir.path().into(),
                links: links.clone(),
                port,
                identity,
                application: Arc::new(Application),
            },
            network,
        )
        .unwrap();
        peers.push(RecoveryPeer {
            _dir: dir,
            links,
            config,
        });
    }
    peers
}
async fn stop_recovery_peers(peers: Vec<RecoveryPeer>) {
    for peer in peers {
        peer.config.stop().await;
        peer.links.stop().await;
    }
}
fn crash_source(dir: &Path, links: &LinkLayer, config: &crate::config_module::ConfigModule) -> store::Store {
    let mut store = store::Store::open(
        dir.into(),
        Anonymizer::new(config.applied().group).device(links.id()),
        true,
    )
    .unwrap();
    store.recover_for_test().unwrap();
    let now = store.current_stamp();
    store
        .append(
            vec![Row {
                mono_us: 0,
                clock_id: now.clock_id,
                utc_us: now.utc_us - 10_000_000,
                event: Event::State {
                    kind: StateKind::RecorderTick,
                    active: true,
                    count: 73,
                    value: 0.,
                },
            }],
            0,
        )
        .unwrap();
    store
}
#[test]
fn recovery_wait_collects_all_members_as_soon_as_links_are_ready() {
    crate::test_watchdog::run(
        module_path!(),
        "recovery_wait_collects_all_members_as_soon_as_links_are_ready",
        Some(4),
        recovery_wait_collects_all_members_as_soon_as_links_are_ready_scenario(),
    );
}
async fn recovery_wait_collects_all_members_as_soon_as_links_are_ready_scenario() {
    let mut fixture = crate::streams::lifecycle_tests::Fixture::<DiagnosticsStreams>::start_members(3).await;
    let peers = (0..3).map(|index| fixture.context(index)).collect::<Vec<_>>();
    let mut changes = peers[0].1.subscribe();
    for (_, config) in &peers {
        config.sleep();
    }
    while peers[0]
        .0
        .links()
        .links
        .iter()
        .any(|link| link.state == LinkState::Ready)
    {
        changes.next().await.unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    drop(crash_source(dir.path(), &peers[0].0, &peers[0].1));
    let (_deadline, held) = tokio::sync::watch::channel(false);
    let (entered, mut waiting) = tokio::sync::watch::channel(None);
    let mut recovery = context(peers[0].0.clone(), peers[0].1.clone());
    recovery.deadline_gate = Some(held);
    recovery.recovery_wait_entered = Some(entered);
    let local = start_recorder(dir.path().into(), &peers[0].0, &peers[0].1, Some(recovery)).unwrap();
    while waiting.borrow().is_none() {
        waiting.changed().await.unwrap();
    }
    let cutoff_before_ready = waiting.borrow().unwrap();
    assert!(
        local.list().unwrap().is_empty(),
        "recovery must wait for initially offline members"
    );
    // Startup/recording remain available while just the recovery export waits.
    local.record(Event::State {
        kind: StateKind::RecorderTick,
        active: true,
        count: 99,
        value: 0.,
    });
    let mut remote_recorders = Vec::new();
    let mut remote_dirs = Vec::new();
    let mut serving = Vec::new();
    let mut requests = Vec::new();
    for (index, peer) in peers.iter().enumerate().skip(1) {
        let remote_dir = tempfile::tempdir().unwrap();
        let recorder = start_recorder(remote_dir.path().into(), &peer.0, &peer.1, None).unwrap();
        recorder.wait_recovery();
        {
            let mut store = lock(&recorder.inner.store);
            let stamp = store.current_stamp();
            store
                .append(
                    vec![Row {
                        mono_us: 0,
                        clock_id: stamp.clock_id,
                        utc_us: stamp.utc_us - 10_000_000,
                        event: Event::State {
                            kind: StateKind::RecorderTick,
                            active: true,
                            count: 74,
                            value: 0.,
                        },
                    }],
                    0,
                )
                .unwrap();
        }
        let collector = Collector {
            recorder: recorder.clone(),
            context: context(peer.0.clone(), peer.1.clone()),
        };
        let mut port = std::mem::replace(fixture.port(index), peer.0.register(&[]));
        let (tx, rx) = tokio::sync::oneshot::channel();
        serving.push(tokio::spawn(async move {
            let operation = incoming(&mut port).await;
            let Message::DiagnosticsRequest(request) = &operation.first else {
                panic!("expected request")
            };
            tx.send(request.clone()).unwrap();
            collector.serve(operation.stream, operation.first).await.unwrap();
        }));
        requests.push(rx);
        remote_recorders.push(recorder);
        remote_dirs.push(remote_dir);
    }
    for (_, config) in &peers {
        config.wake();
    }
    let mut saved = local.saved_bundles();
    while local.list().unwrap().is_empty() {
        saved.changed().await.unwrap();
    }
    assert!(!*_deadline.borrow(), "readiness, not the deadline, released recovery");
    let bundle = local
        .list()
        .unwrap()
        .into_iter()
        .find(|bundle| bundle.reason == Reason::RecoveredCrash)
        .unwrap();
    assert_eq!(
        (
            bundle.included_device_count,
            bundle.missing_device_count,
            bundle.cutoff_complete
        ),
        (Some(3), Some(0), Some(true)),
        "{}",
        manifest(&bundle)
    );
    let manifest = manifest(&bundle);
    let range = manifest["requested_range_us"].as_array().unwrap();
    let end = range[1].as_i64().unwrap();
    assert_eq!(end - range[0].as_i64().unwrap(), AUTOMATIC_RANGE_US);
    assert_eq!(manifest["export_clock"]["utc_us"], end);
    for request in requests {
        let request = request.await.unwrap();
        assert_eq!(
            (request.start_us().unwrap(), request.end_us().unwrap()),
            (end - AUTOMATIC_RANGE_US, end)
        );
        assert_eq!(
            end, cutoff_before_ready,
            "readiness must not shift the recovered cutoff"
        );
    }
    for task in serving {
        task.await.unwrap();
    }
    local.shutdown();
    for recorder in remote_recorders {
        recorder.shutdown();
    }
    fixture.stop().await;
}
#[test]
fn recovery_wait_deadline_exports_with_missing_member_at_fifteen_seconds() {
    let _watchdog = crate::test_watchdog::Scenario::new(
        module_path!(),
        "recovery_wait_deadline_exports_with_missing_member_at_fifteen_seconds",
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    runtime.block_on(recovery_wait_deadline_exports_with_missing_member_at_fifteen_seconds_scenario());
    drop(runtime);
}
async fn recovery_wait_deadline_exports_with_missing_member_at_fifteen_seconds_scenario() {
    use std::{future::Future, task::Poll};
    let peers = recovery_peers(2).await;
    let dir = tempfile::tempdir().unwrap();
    drop(crash_source(dir.path(), &peers[0].links, &peers[0].config));
    let source = store::Store::open(
        dir.path().into(),
        Anonymizer::new(peers[0].config.applied().group).device(peers[0].links.id()),
        true,
    )
    .unwrap();
    let snapshot = source.recover(|_, _, _| Ok(()), |_| {}).unwrap();
    let context = context(peers[0].links.clone(), peers[0].config.clone());
    let wake = tokio::sync::Notify::new();
    {
        let mut waiting = std::pin::pin!(context.wait_recovery_ready(&snapshot, &wake));
        async fn pending<F: Future>(waiting: &mut std::pin::Pin<&mut F>) -> bool {
            std::future::poll_fn(|cx| Poll::Ready(matches!(waiting.as_mut().poll(cx), Poll::Pending))).await
        }
        assert!(pending(&mut waiting).await);
        tokio::time::advance(Duration::from_millis(14_999)).await;
        assert!(pending(&mut waiting).await, "deadline must not expire early");
        tokio::time::advance(Duration::from_millis(1)).await;
        waiting.await.unwrap();
    }
    let bundle = tokio::task::spawn_blocking(move || context.export(snapshot, Reason::RecoveredCrash, 0, 0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            bundle.included_device_count,
            bundle.missing_device_count,
            bundle.cutoff_complete
        ),
        (Some(1), Some(1), Some(false))
    );
    assert!(
        manifest(&bundle)["missing_members"]
            .as_object()
            .unwrap()
            .values()
            .all(|value| value == "offline")
    );
    stop_recovery_peers(peers).await;
}
#[test]
fn recovery_wait_manual_save_keeps_immediate_offline_behavior() {
    crate::test_watchdog::run(
        module_path!(),
        "recovery_wait_manual_save_keeps_immediate_offline_behavior",
        Some(2),
        recovery_wait_manual_save_keeps_immediate_offline_behavior_scenario(),
    );
}
async fn recovery_wait_manual_save_keeps_immediate_offline_behavior_scenario() {
    let peers = recovery_peers(2).await;
    let dir = tempfile::tempdir().unwrap();
    let recorder = start_recorder(dir.path().into(), &peers[0].links, &peers[0].config, None).unwrap();
    recorder.wait_recovery();
    let (_deadline, held) = tokio::sync::watch::channel(false);
    let (entered, waiting) = tokio::sync::watch::channel(None);
    let mut collection = context(peers[0].links.clone(), peers[0].config.clone());
    collection.deadline_gate = Some(held);
    collection.recovery_wait_entered = Some(entered);
    recorder.inner.collection.set(collection).ok().unwrap();
    let save = recorder.clone();
    let bundle = tokio::task::spawn_blocking(move || save.save(Reason::Manual))
        .await
        .unwrap()
        .unwrap();
    assert!(
        waiting.borrow().is_none(),
        "manual save never entered recovery readiness"
    );
    assert_eq!(bundle.missing_device_count, Some(1));
    assert_eq!(bundle.cutoff_complete, Some(false));
    recorder.shutdown();
    stop_recovery_peers(peers).await;
}

#[test]
fn recovery_wait_pause_preserves_export_and_shutdown_cancels_durably() {
    crate::test_watchdog::run(
        module_path!(),
        "recovery_wait_pause_preserves_export_and_shutdown_cancels_durably",
        Some(2),
        recovery_wait_pause_preserves_export_and_shutdown_cancels_durably_scenario(),
    );
}
async fn recovery_wait_pause_preserves_export_and_shutdown_cancels_durably_scenario() {
    let peers = recovery_peers(2).await;
    let dir = tempfile::tempdir().unwrap();
    let previous = crash_source(dir.path(), &peers[0].links, &peers[0].config)
        .instance
        .clone();
    let (_deadline, held) = tokio::sync::watch::channel(false);
    let (entered, mut waiting) = tokio::sync::watch::channel(None);
    let mut recovery = context(peers[0].links.clone(), peers[0].config.clone());
    recovery.deadline_gate = Some(held);
    recovery.recovery_wait_entered = Some(entered);
    let recorder = start_recorder(dir.path().into(), &peers[0].links, &peers[0].config, Some(recovery)).unwrap();
    while waiting.borrow().is_none() {
        waiting.changed().await.unwrap();
    }
    recorder.set_suspended(true);
    super::tests::pause(&peers[0].config);
    assert!(recorder.status().recovering, "pause preserves pending crash recovery");
    assert!(recorder.list().unwrap().is_empty());
    let shutdown = recorder.clone();
    tokio::task::spawn_blocking(move || shutdown.shutdown()).await.unwrap();
    assert!(
        !*_deadline.borrow(),
        "shutdown cancelled recovery while deadline remained held"
    );
    assert!(!recorder.status().recovering);
    assert!(recorder.list().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(dir.path().join("running")).unwrap(), previous);
    assert!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with("recover-"))
    );
    stop_recovery_peers(peers).await;
}
