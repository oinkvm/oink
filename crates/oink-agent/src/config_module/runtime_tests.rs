//! Real loopback TLS/C0 owners with durable stores. No native desktop backend.
use super::*;
use oink_foundation::{realtime::Tier, session::Boot};
use oink_network::{identity::Identity, link::LinkConfig};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Default)]
struct Application {
    fail: AtomicBool,
    state: Mutex<Option<group_store::StoredGroupState>>,
}
impl ConfigApplication for Application {
    fn apply_state(&self, state: &group_store::StoredGroupState) -> Result<(), String> {
        if self.fail.load(Ordering::Acquire) {
            return Err("synthetic native apply failure".into());
        }
        *self.state.lock().unwrap() = Some(state.clone());
        Ok(())
    }
}
pub(crate) struct Running {
    directory: tempfile::TempDir,
    identity: Arc<Identity>,
    pub(crate) group: ConfigModule,
    pub(crate) links: LinkLayer,
    application: Arc<Application>,
    pub(crate) config: Arc<Mutex<crate::config::AgentConfig>>,
    pub(crate) gate: Arc<Mutex<()>>,
}
pub(crate) fn set_application_failure(node: &Running, fail: bool) {
    node.application.fail.store(fail, Ordering::Release);
}
pub(crate) fn display(key: &str) -> DisplayInfo {
    DisplayInfo {
        key: key.into(),
        name: key.into(),
        bounds: oink_foundation::Rect::new(0., 0., 800., 600.),
        pixel_width: 800,
        pixel_height: 600,
        reference_extent: Some(oink_foundation::Size::new(300., 225.)),
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }
}
fn identities(count: u8) -> Vec<Arc<Identity>> {
    (1..=count).map(|n| Arc::new(Identity::from_seed([n; 32]))).collect()
}
fn fresh(count: u8) -> Vec<group_store::StoredGroupState> {
    identities(count)
        .iter()
        .map(|identity| {
            group_store::StoredGroupState::new(
                identity.id(),
                State::create(identity, "synthetic", "test", 1).unwrap().wire().clone(),
            )
            .unwrap()
        })
        .collect()
}
pub(crate) fn same_group(count: u8) -> Vec<group_store::StoredGroupState> {
    let keys = identities(count);
    let mut state = State::create(&keys[0], "synthetic", "test", 1).unwrap();
    for key in &keys[1..] {
        state = state
            .with_roster(&state.roster().add(&keys[0], key.id(), "synthetic", "test", 2).unwrap())
            .unwrap();
    }
    keys.iter()
        .map(|key| group_store::StoredGroupState::new(key.id(), state.wire().clone()).unwrap())
        .collect()
}
pub(crate) async fn start(states: Vec<group_store::StoredGroupState>) -> Vec<Running> {
    start_with_hints(states, None).await
}
async fn start_with_hints(
    states: Vec<group_store::StoredGroupState>,
    topology: Option<&[(usize, usize)]>,
) -> Vec<Running> {
    start_with_bind(states, topology, std::net::Ipv4Addr::LOCALHOST.into()).await
}
async fn start_with_bind(
    states: Vec<group_store::StoredGroupState>,
    topology: Option<&[(usize, usize)]>,
    bind_ip: std::net::IpAddr,
) -> Vec<Running> {
    let keys = identities(states.len() as u8);
    let mut pending = Vec::new();
    for (state, identity) in states.into_iter().zip(keys) {
        assert_eq!(state.device, identity.id());
        let directory = tempfile::tempdir().unwrap();
        group_store::save(&directory.path().join(group_store::STATE_FILE), &state).unwrap();
        let mut network = oink_test_support::network_config(directory.path().into(), "synthetic", 0);
        network.bind_ip = bind_ip;
        let (links, port) = LinkLayer::start(LinkConfig {
            profile: crate::node::Node::offer_profile(),
            identity: identity.clone(),
            boot: Boot {
                device_id: identity.id(),
                nonce: random_id().unwrap(),
            },
            bind: (network.bind_ip, network.port).into(),
            tier: Tier::Elevated,
        })
        .await
        .unwrap();
        pending.push((directory, identity, network, links, port));
    }
    let addresses = pending
        .iter()
        .map(|(_, identity, _, links, _)| (identity.id(), links.local_addr()))
        .collect::<Vec<_>>();
    let mut nodes = Vec::new();
    for (local, (directory, identity, network, links, port)) in pending.into_iter().enumerate() {
        let mut hints = super::super::local_records::AddressStore::load(directory.path()).unwrap();
        for (remote, (peer, address)) in addresses.iter().enumerate() {
            if *peer != identity.id() && topology.is_none_or(|hints| hints.contains(&(local, remote))) {
                hints.remember(*peer, *address).unwrap();
            }
        }
        let application = Arc::new(Application::default());
        let config = Arc::new(Mutex::new(
            crate::config::AgentConfig::load_from_with_device_name(directory.path(), "synthetic".into()).unwrap(),
        ));
        let gate = Arc::new(Mutex::new(()));
        let group = ConfigModule::start_with_config(
            ConfigDeps {
                data_dir: directory.path().into(),
                identity: identity.clone(),
                links: links.clone(),
                port,
                application: application.clone(),
                config: config.clone(),
                owner_gate: gate.clone(),
            },
            network,
        )
        .unwrap();
        if topology.is_some() {
            // Partial-topology fixtures model interfaces that cannot discover
            // each other. Keep host interface polling from replacing the
            // endpoint facts these loopback scenarios explicitly publish.
            group
                .call(|owner| {
                    owner.network_checked = Instant::now() + Duration::from_secs(60);
                    owner
                        .local
                        .edit(|raw| {
                            raw.insert(10, RawValue::Array(vec![]));
                            Ok(())
                        })
                        .unwrap();
                    owner.publish_facts()?;
                    Ok(())
                })
                .unwrap();
        }
        nodes.push(Running {
            directory,
            identity,
            group,
            links,
            application,
            config,
            gate,
        });
    }
    nodes
}
async fn restart(node: &mut Running) {
    node.group.stop().await;
    node.links.stop().await;
    let network = oink_test_support::network_config(node.directory.path().into(), "synthetic", 0);
    let (links, port) = LinkLayer::start(LinkConfig {
        profile: crate::node::Node::offer_profile(),
        identity: node.identity.clone(),
        boot: Boot {
            device_id: node.identity.id(),
            nonce: random_id().unwrap(),
        },
        bind: (network.bind_ip, network.port).into(),
        tier: Tier::Elevated,
    })
    .await
    .unwrap();
    let group = ConfigModule::start_with_config(
        ConfigDeps {
            data_dir: node.directory.path().into(),
            identity: node.identity.clone(),
            links: links.clone(),
            port,
            application: node.application.clone(),
            config: node.config.clone(),
            owner_gate: node.gate.clone(),
        },
        network,
    )
    .unwrap();
    node.links = links;
    node.group = group;
}
pub(crate) async fn stop(nodes: &[Running]) {
    for node in nodes {
        node.group.stop().await;
        node.links.stop().await;
    }
}
pub(crate) async fn until(mut predicate: impl FnMut() -> bool) -> bool {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}
pub(crate) fn ready(nodes: &[Running]) -> bool {
    nodes.iter().all(|node| {
        nodes.iter().all(|peer| {
            peer.identity.id() == node.identity.id()
                || node
                    .group
                    .members()
                    .members
                    .iter()
                    .any(|m| m.device == peer.identity.id() && m.online)
        })
    })
}
pub(crate) fn observed_display_layout_before_fact_publication(
    group: &ConfigModule,
    displays: Vec<DisplayInfo>,
) -> crate::diagnostics::layout::CompleteLayout {
    let read = group.clone();
    group
        .call(move |owner| {
            owner.displays_changed(displays)?;
            Ok(read
                .diagnostic_layout(&crate::diagnostics::Anonymizer::new(read.applied().group))
                .unwrap())
        })
        .unwrap()
}
static OPS: AtomicU64 = AtomicU64::new(1);
fn operation() -> (String, u64) {
    let n = OPS.fetch_add(1, Ordering::Relaxed);
    (format!("00000000-0000-4000-8000-{n:012x}"), n)
}
async fn query(source: &Running, target: &Running, client: [u8; 16]) -> api::FoundPeer {
    let mut events = source.group.subscribe();
    let (op, generation) = operation();
    let result = source
        .group
        .query_pairing_address(
            client,
            op.clone(),
            generation,
            "127.0.0.1".into(),
            Some(target.links.local_addr().port()),
        )
        .unwrap();
    assert!(matches!(result, api::PairReply::Value(_)), "{result:?}");
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(ConfigChange::PairingEvent(event)) = events.next().await
                && let api::PairingEvent::AddressQueryFinished {
                    operation_id, result, ..
                } = *event
                && operation_id == op
            {
                match result {
                    api::AddressQueryResult::Found { peer } => break peer,
                    other => panic!("query failed: {other:?}"),
                }
            }
        }
    })
    .await
    .unwrap()
}
async fn request(source: &Running, target: &Running, client: [u8; 16]) -> String {
    let peer = query(source, target, client).await;
    assert_eq!(peer.peer.device.0, target.identity.id());
    assert!(peer.prepared);
    let (op, _) = operation();
    let result = source
        .group
        .start_pairing(
            client,
            op,
            api::PairTarget::PreparedQuery {
                query_id: peer.query_id,
                generation: peer.generation,
            },
        )
        .unwrap();
    let api::PairReply::Value(api::Started::Started { pair_id }) = result else {
        panic!("start: {result:?}")
    };
    pair_id
}
async fn accept(source: &Running, target: &Running, id: &str) -> String {
    assert!(until(|| target.group.pairings().requests.iter().any(|r| r.id == id)).await);
    let request = target
        .group
        .pairings()
        .requests
        .iter()
        .find(|r| r.id == id)
        .unwrap()
        .clone();
    assert!(matches!(
        target.group.accept_pairing(id.into(), request.revision.0).unwrap(),
        api::PairReply::Value(_)
    ));
    assert!(
        until(|| source
            .group
            .pairings()
            .sessions
            .iter()
            .any(|s| s.id == id && s.phase == 1)
            && target
                .group
                .pairings()
                .sessions
                .iter()
                .any(|s| s.id == id && s.phase == 2))
        .await
    );
    source.group.pairing_code(id.into()).unwrap().six_digits
}
fn submit(target: &Running, id: &str, attempt: u8, code: String) {
    let session = target
        .group
        .pairings()
        .sessions
        .iter()
        .find(|s| s.id == id)
        .unwrap()
        .clone();
    let result = target
        .group
        .submit_pairing_code(id.into(), session.revision.0, attempt, code)
        .unwrap();
    assert!(matches!(result, api::PairReply::Value(_)), "submit: {result:?}");
}
async fn complete(source: &Running, target: &Running, id: &str) {
    assert!(
        until(|| [source, target].iter().all(|node| node
            .group
            .subscribe_pairing_events(0)
            .unwrap()
            .iter()
            .any(|event| matches!(event,api::PairingEvent::PairingCompleted{pair_id,..} if pair_id==id))))
        .await,
        "completion snapshots: {:?} / {:?}",
        source.group.pairings(),
        target.group.pairings()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_members_use_retained_address_store_and_publish_current_boot_facts() {
    let mut nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    assert_eq!(
        nodes[0].group.peer_boot(nodes[1].identity.id()),
        Some(nodes[1].links.boot())
    );
    for node in &nodes {
        let facts = node.group.local_facts();
        assert_eq!(facts.group().unwrap().0, node.group.applied().group.0);
        assert_eq!(
            node.config.lock().unwrap().owner().active_group(),
            Some(node.group.applied().group)
        );
        assert!(facts.preferences().unwrap().is_some());
        assert!(facts.pause().unwrap().is_some());
    }
    let old = nodes[1].links.boot();
    restart(&mut nodes[1]).await;
    nodes[0]
        .group
        .set_member_address(nodes[1].identity.id(), nodes[1].links.local_addr().to_string())
        .unwrap();
    assert!(until(|| ready(&nodes)).await);
    assert_ne!(nodes[1].links.boot(), old);
    assert_eq!(
        nodes[0].group.peer_boot(nodes[1].identity.id()),
        Some(nodes[1].links.boot())
    );
    stop(&nodes).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_pairing_is_explicit_directional_and_undo_removes_only_the_peer() {
    // Keep the same untrusted discovery hints through the merge and undo. A
    // stale advertised group must never override authoritative membership.

    let nodes = start(fresh(2)).await;
    for (local, remote) in [(0, 1), (1, 0)] {
        let peer = nodes[remote].identity.id();
        let address = nodes[remote].links.local_addr();
        nodes[local]
            .group
            .call(move |owner| {
                owner.sighting(Sighting {
                    advert: Advert::local(
                        peer,
                        "synthetic".into(),
                        GroupId([99; 16]),
                        1,
                        address.port(),
                        "test".into(),
                    ),
                    addrs: vec![address],
                    source: oink_network::discovery::Source::Manual,
                });
                Ok(())
            })
            .unwrap();
        assert_eq!(nodes[local].group.nearby().rows.len(), 1);
    }
    let target = nodes[1].group.applied().group;
    let id = request(&nodes[0], &nodes[1], [1; 16]).await;
    assert_ne!(nodes[0].group.applied().group, target);
    let code = accept(&nodes[0], &nodes[1], &id).await;
    submit(&nodes[1], &id, 1, code);
    complete(&nodes[0], &nodes[1], &id).await;
    assert_eq!(nodes[0].group.applied().group, target);
    assert_eq!(nodes[1].group.applied().group, target);
    assert!(nodes.iter().all(|node| node.group.nearby().rows.is_empty()));
    let event = nodes[0]
        .group
        .subscribe_pairing_events(0)
        .unwrap()
        .into_iter()
        .find(|e| matches!(e, api::PairingEvent::PairingCompleted { .. }))
        .unwrap();
    let api::PairingEvent::PairingCompleted { event_id, undo, .. } = event else {
        unreachable!()
    };
    let session = nodes[0]
        .group
        .pairings()
        .sessions
        .iter()
        .find(|s| s.id == id)
        .unwrap()
        .clone();
    assert!(matches!(
        nodes[0].group.cancel_pairing(id, session.revision.0).unwrap(),
        api::PairReply::Value(api::CancelResult::AlreadyCommitted { .. })
    ));
    nodes[0].group.ack_pairing_event(event_id.0).unwrap();
    assert!(nodes[0].group.subscribe_pairing_events(0).unwrap().is_empty());
    assert!(matches!(
        nodes[0].group.undo_pairing(operation().0, undo).unwrap(),
        api::PairReply::Value(_)
    ));
    assert!(
        until(|| nodes[0].group.members().members.len() == 1
            && nodes[1].group.members().members.len() == 1
            && nodes[1].group.applied().group != target)
        .await
    );
    assert!(nodes.iter().all(|node| node.group.nearby().rows.len() == 1));
    stop(&nodes).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_capacity_wrong_code_and_cancel_are_independent_on_one_c0() {
    let nodes = start(fresh(2)).await;
    let mut ids = Vec::new();
    for client in 1..=4 {
        ids.push(request(&nodes[0], &nodes[1], [client; 16]).await);
    }
    assert!(
        until(|| nodes[1].group.pairings().requests.len() == 3
            && nodes[0]
                .group
                .pairings()
                .sessions
                .iter()
                .any(|s| s.id == ids[3] && s.terminal.as_ref().is_some_and(|p| p.code == 46)))
        .await
    );
    assert!(nodes[1].group.subscribe_pairing_events(0).unwrap().is_empty());
    let code = accept(&nodes[0], &nodes[1], &ids[0]).await;
    let wrong = if code == "000000" { "111111" } else { "000000" };
    submit(&nodes[1], &ids[0], 1, wrong.into());
    assert!(
        until(|| nodes[1]
            .group
            .pairings()
            .sessions
            .iter()
            .any(|s| s.id == ids[0] && s.attempts_left == 2 && s.phase == 2))
        .await
    );
    let second = nodes[0]
        .group
        .pairings()
        .sessions
        .iter()
        .find(|s| s.id == ids[1])
        .unwrap()
        .clone();
    assert!(matches!(
        nodes[0].group.cancel_pairing(second.id, second.revision.0).unwrap(),
        api::PairReply::Value(api::CancelResult::Cancelled)
    ));
    assert!(until(|| nodes[1].group.pairings().requests.len() == 1).await);
    assert_eq!(nodes[1].group.pairings().requests[0].id, ids[2]);
    submit(&nodes[1], &ids[0], 2, code);
    complete(&nodes[0], &nodes[1], &ids[0]).await;
    stop(&nodes).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_identity_is_hidden_automatically_but_manual_queries_remain_usable() {
    let nodes = start(fresh(2)).await;
    assert!(matches!(
        nodes[0].group.block(operation().0, nodes[1].identity.id()).unwrap(),
        crate::device_cards::BlockReply::Edited(_)
    ));
    let peer = query(&nodes[0], &nodes[1], [1; 16]).await;
    assert!(peer.prepared);
    let id = request(&nodes[0], &nodes[1], [2; 16]).await;
    let code = accept(&nodes[0], &nodes[1], &id).await;
    submit(&nodes[1], &id, 1, code);
    complete(&nodes[0], &nodes[1], &id).await;
    assert!(
        nodes[0]
            .group
            .blocked_devices()
            .unwrap()
            .entries
            .iter()
            .any(|e| e.device.0 == nodes[1].identity.id())
    );
    stop(&nodes).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_projection_waits_for_owner_gate_and_native_application_before_publication() {
    for application_failure in [false, true] {
        let nodes = start(fresh(2)).await;
        let source = nodes[0].group.applied().group;
        let target = nodes[1].group.applied().group;
        let id = request(&nodes[0], &nodes[1], [1; 16]).await;
        let code = accept(&nodes[0], &nodes[1], &id).await;
        let held = if application_failure {
            set_application_failure(&nodes[0], true);
            None
        } else {
            let gate = nodes[0].gate.clone();
            let (entered, entering) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let _guard = gate.lock().unwrap();
                entered.send(()).unwrap();
                let _ = released.recv();
            });
            entering.recv().unwrap();
            Some((release, holder))
        };
        submit(&nodes[1], &id, 1, code);
        assert!(
            until(|| Journal::load(
                &nodes[0].directory.path().join(oink_network::pair_journal::FILE),
                nodes[0].identity.id()
            )
            .unwrap()
            .pending_projection()
            .unwrap()
            .is_some())
            .await,
            "durable projection missing (native failure={application_failure}); source={:?}; target={:?}",
            nodes[0].group.pairings(),
            nodes[1].group.pairings(),
        );
        assert_eq!(nodes[0].group.applied().group, source);
        assert_eq!(nodes[0].group.local_facts().group().unwrap().0, source.0);
        let facts = nodes[0].group.local_facts();
        let result = nodes[0]
            .group
            .publish_local_owner(facts.preferences().unwrap().unwrap().0, facts.pause().unwrap().unwrap());
        assert!(result.is_err());
        if let Some((release, holder)) = held {
            release.send(()).unwrap();
            holder.join().unwrap();
        }
        set_application_failure(&nodes[0], false);
        complete(&nodes[0], &nodes[1], &id).await;
        assert_eq!(nodes[0].group.applied().group, target);
        assert_eq!(nodes[0].config.lock().unwrap().owner().active_group(), Some(target));
        stop(&nodes).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn display_geometry_and_owner_facts_converge_without_replicating_pause() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    nodes[0].group.initialize_displays(vec![display("first")]).unwrap();
    assert!(until(|| nodes[1].group.applied().layout.screens.len() == 1).await);
    let before = nodes[0].group.applied().layout.clone();
    nodes[0].group.local_displays_changed(Vec::new());
    assert!(until(|| nodes[1].group.applied().layout.screens.is_empty()).await);
    nodes[0].group.local_displays_changed(vec![display("first")]);
    assert!(
        until(|| nodes[0].group.applied().layout.screens == before.screens
            && nodes[1].group.applied().layout.screens == before.screens)
        .await
    );
    let pause = dto::LocalPauseState::from_raw(
        [
            (1, true.write()),
            (2, dto::PauseReason::LocalUser.write()),
            (3, 2u64.write()),
        ]
        .into(),
    )
    .unwrap();
    let mut changes = nodes[0].group.subscribe();
    nodes[0]
        .group
        .publish_local_owner(nodes[0].group.local_facts().preferences().unwrap().unwrap().0, pause)
        .unwrap();
    assert!(
        until(|| nodes[1]
            .group
            .members()
            .members
            .iter()
            .find(|m| m.device == nodes[0].identity.id())
            .and_then(|m| m.facts.as_ref())
            .is_some_and(|f| f.pause().unwrap().unwrap().paused() == Ok(true)))
        .await
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(change) = changes.next().await {
            if matches!(change, ConfigChange::State { items } if items.contains(&Item::Pause)) {
                return;
            }
        }
        panic!("pause change stream closed");
    })
    .await
    .unwrap();
    assert!(nodes[0].group.applied().paused);
    assert!(!nodes[1].group.applied().paused);
    stop(&nodes).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_submit_after_link_loss_is_closed_and_no_code_state_resumes() {
    let nodes = start(fresh(2)).await;
    let id = request(&nodes[0], &nodes[1], [1; 16]).await;
    let code = accept(&nodes[0], &nodes[1], &id).await;
    let session = nodes[1]
        .group
        .pairings()
        .sessions
        .iter()
        .find(|s| s.id == id)
        .unwrap()
        .clone();
    nodes[0].links.disconnect(
        nodes[0].links.links().by_peer(nodes[1].identity.id()).unwrap().link,
        Disconnect::Stopping,
    );
    assert!(
        until(|| nodes[1]
            .group
            .pairings()
            .sessions
            .iter()
            .any(|s| s.id == id && s.phase == 8))
        .await
    );
    assert!(matches!(
        nodes[1]
            .group
            .submit_pairing_code(id, session.revision.0, 1, code)
            .unwrap(),
        api::PairReply::Problem(_)
    ));
    stop(&nodes).await;
}

#[path = "runtime_control_tests.rs"]
mod control;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn member_addresses_are_durable_while_disconnected_and_fail_with_typed_errors() {
    // Only the local owner runs: its roster member has no endpoint or live path.
    let nodes = start(same_group(2).into_iter().take(1).collect()).await;
    let peer = identities(2)[1].id();
    let group = &nodes[0].group;
    group.sleep();
    assert!(group.member_addresses(peer).unwrap().is_empty());
    group.set_member_address(peer, "192.0.2.4:47470".into()).unwrap();
    assert_eq!(group.member_addresses(peer).unwrap()[0], "192.0.2.4:47470");
    assert_eq!(
        super::super::local_records::AddressStore::load(nodes[0].directory.path())
            .unwrap()
            .candidates(peer)[0]
            .to_string(),
        "192.0.2.4:47470"
    );
    for invalid in [
        "invalid",
        "[::1]:47470",
        "0.0.0.0:47470",
        "224.0.0.1:47470",
        "255.255.255.255:47470",
        "192.0.2.4:0",
    ] {
        assert_eq!(
            group.set_member_address(peer, invalid.into()),
            Err(ApiError::InvalidAddress)
        );
    }
    assert_eq!(group.member_addresses(DeviceId([99; 32])), Err(ApiError::NotMember));
    assert_eq!(
        group.set_member_address(DeviceId([99; 32]), "192.0.2.4:47470".into()),
        Err(ApiError::NotMember)
    );
    let path = nodes[0].directory.path().join("addresses.json");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert_eq!(
        group.set_member_address(peer, "192.0.2.5:47470".into()),
        Err(ApiError::PersistenceFailed)
    );
    assert_eq!(group.member_addresses(peer).unwrap()[0], "192.0.2.4:47470");
    tokio::time::timeout(Duration::from_secs(3), stop(&nodes))
        .await
        .expect("local fixture stops");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn address_query_events_preserve_network_causes_without_claiming_ipc_loss() {
    use api::AddressNetworkProblem as P;
    use oink_network::link::AddressProbeError as E;
    let nodes = start(fresh(1)).await;
    let group = &nodes[0].group;
    let mut events = group.subscribe();
    for (error, expected) in [
        (E::NoRoute, P::NoRoute),
        (E::NoNetworkInterface, P::NoNetworkInterface),
        (E::Unreachable, P::PeerUnreachable),
        (E::Socket, P::SocketError),
        (E::Stopped, P::NetworkStopped),
    ] {
        group
            .call(move |owner| {
                owner.queries.insert(
                    "synthetic".into(),
                    Query {
                        client: [0; 16],
                        operation: "operation".into(),
                        generation: 1,
                        address: "192.0.2.4:47470".parse().unwrap(),
                        peer: None,
                        boot: None,
                        deadline: None,
                        released: false,
                    },
                );
                owner.probe_result("synthetic".into(), 1, Err(error));
                Ok(())
            })
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(ConfigChange::PairingEvent(event)) = events.next().await
                    && let api::PairingEvent::AddressQueryFinished { result, .. } = *event
                {
                    break result;
                }
            }
        })
        .await
        .unwrap();
        let api::AddressQueryResult::Problem { problem } = event else {
            panic!("expected problem")
        };
        assert_eq!(problem.code, dto::PairCode::Unreachable.number());
        assert_eq!(problem.network_problem, Some(expected));
        let json = serde_json::to_value(&problem).unwrap();
        assert_eq!(serde_json::from_value::<api::PairProblem>(json).unwrap(), problem);
    }
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relayed_members_upgrade_using_authenticated_endpoint_facts_without_discovery() {
    let nodes = start_with_hints(same_group(3), Some(&[(0, 1), (1, 0), (0, 2), (2, 0)])).await;
    assert!(until(|| ready(&nodes)).await);
    let a = nodes[1].identity.id();
    let b = nodes[2].identity.id();
    assert!(matches!(
        nodes[1].links.links().by_peer(b).unwrap().path,
        LinkPath::Relayed { .. }
    ));
    assert!(matches!(
        nodes[2].links.links().by_peer(a).unwrap().path,
        LinkPath::Relayed { .. }
    ));
    // The relay carries authenticated facts across a discovery boundary. Only
    // these endpoint facts reveal the missing direct addresses to the members.
    for node in &nodes[1..] {
        let address = node.links.local_addr();
        node.group
            .call(move |owner| {
                let std::net::IpAddr::V4(ip) = address.ip() else {
                    unreachable!()
                };
                let endpoint = dto::EndpointFact::from_raw(
                    [
                        (1, dto::AddressFamily::Ipv4.write()),
                        (2, RawValue::Bytes(ip.octets().to_vec())),
                        (3, address.port().write()),
                        (4, 0u32.write()),
                    ]
                    .into(),
                )
                .unwrap();
                owner
                    .local
                    .edit(|raw| {
                        raw.insert(10, RawValue::Array(vec![endpoint.write()]));
                        Ok(())
                    })
                    .unwrap();
                owner.publish_facts()?;
                Ok(())
            })
            .unwrap();
    }
    let upgraded =
        until(|| {
            [(1, b), (2, a)].iter().all(|(local, peer)| {
                nodes[*local].links.links().by_peer(*peer).is_some_and(|link| {
                    link.state == oink_network::link::LinkState::Ready && link.path == LinkPath::Direct
                })
            })
        })
        .await;
    stop(&nodes).await;
    assert!(
        upgraded,
        "authenticated endpoints never reached the relay upgrade dialer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn member_candidates_merge_retained_cached_facts_and_discovery_without_replacement() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    let peer = nodes[1].identity.id();
    assert!(
        until(|| nodes[0]
            .group
            .call(move |owner| Ok(owner.facts.contains_key(&peer)))
            .unwrap())
        .await
    );
    let retained = nodes[1].links.local_addr();
    nodes[0]
        .group
        .call(move |owner| {
            let announced: SocketAddr = "192.0.2.30:47470".parse().unwrap();
            let discovered: SocketAddr = "192.0.2.31:47470".parse().unwrap();
            let mut raw = owner.facts[&peer].raw().clone();
            raw.insert(
                10,
                RawValue::Array(
                    [
                        announced,
                        retained,
                        "0.0.0.0:47470".parse().unwrap(),
                        "224.0.0.1:47470".parse().unwrap(),
                        "255.255.255.255:47470".parse().unwrap(),
                    ]
                    .into_iter()
                    .map(|address| {
                        let std::net::IpAddr::V4(ip) = address.ip() else {
                            unreachable!()
                        };
                        dto::EndpointFact::from_raw(
                            [
                                (1, dto::AddressFamily::Ipv4.write()),
                                (2, RawValue::Bytes(ip.octets().to_vec())),
                                (3, address.port().write()),
                                (4, 0u32.write()),
                            ]
                            .into(),
                        )
                        .unwrap()
                        .write()
                    })
                    .collect(),
                ),
            );
            let facts = dto::DeviceFacts::from_raw(raw).unwrap();
            owner.cache.observe(facts.clone()).unwrap();
            owner.facts.insert(peer, Arc::new(facts));
            owner.sighting(Sighting {
                advert: Advert::local(
                    peer,
                    "synthetic".into(),
                    owner.state.roster().anchor().group,
                    2,
                    discovered.port(),
                    "test".into(),
                ),
                addrs: vec![discovered, retained],
                source: oink_network::discovery::Source::Mdns,
            });
            assert_eq!(owner.member_candidates(peer), vec![retained, announced, discovered]);
            owner.facts.remove(&peer);
            assert_eq!(owner.member_candidates(peer), vec![retained, announced, discovered]);
            Ok(())
        })
        .unwrap();
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn greeting_reply_closing_its_link_returns_an_error_without_panicking_the_owner() {
    use oink_network::negotiation::Selected;
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    let peer = nodes[1].identity.id();
    let link = nodes[0].links.links().by_peer(peer).unwrap().link;
    let local_id = nodes[0].identity.id();
    let mine = nodes[1]
        .group
        .call(move |owner| {
            Ok(owner
                .sessions
                .values()
                .find(|s| s.peer_boot.device_id == local_id)
                .unwrap()
                .peer
                .clone()
                .unwrap())
        })
        .unwrap();
    let survived = nodes[0]
        .group
        .call(move |owner| {
            let local = owner.links.boot();
            let peer = owner.sessions[&link].peer_boot;
            let profile = owner.links.profile().clone();
            let now = Instant::now();
            let prior = owner.sessions[&link].peer.clone().unwrap();
            let local_offer = mine.greeting.offer().unwrap();
            let peer_offer = prior.greeting.offer().unwrap();
            let connection = wire::read_connection(&local_offer.connection_id().unwrap()).unwrap();
            let selected = Selected {
                peer_boot: peer,
                connection,
                agreement: prior.agreement.clone(),
                negotiated: prior.negotiated.clone(),
                local_offer: v1::frame::encode(&Message::ConnectionOffer(local_offer), v1::limits::CONFIG_FRAME)
                    .unwrap(),
                peer_offer: v1::frame::encode(&Message::ConnectionOffer(peer_offer), v1::limits::CONFIG_FRAME).unwrap(),
            };
            let reverse = Selected {
                peer_boot: local,
                local_offer: selected.peer_offer.clone(),
                peer_offer: selected.local_offer.clone(),
                ..selected.clone()
            };
            let anchor = owner.state.roster().anchor();
            let digest = owner.state.roster().digest().unwrap();
            let (greeting, _) = admission::GreetingSession::new(
                local,
                peer,
                false,
                profile.clone(),
                Some(selected),
                None,
                owner.advert.borrow().clone(),
                anchor,
                digest,
                now,
            )
            .unwrap();
            let (_, hello) = admission::GreetingSession::new(
                peer,
                local,
                true,
                profile,
                Some(reverse),
                None,
                Advert::local(
                    peer.device_id,
                    "synthetic".into(),
                    anchor.group,
                    2,
                    47470,
                    "test".into(),
                ),
                anchor,
                digest,
                now,
            )
            .unwrap();
            let session = owner.sessions.get_mut(&link).unwrap();
            session.greeting = greeting;
            session.peer = None;
            // A mandatory reply must close a saturated link. This is synchronous:
            // control must not index the session again after sending that reply.
            session.outbox = std::iter::repeat_n(vec![0], 256).collect();
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.control(link, &hello.unwrap())));
            assert!(!owner.sessions.contains_key(&link));
            Ok(matches!(result, Ok(Err(error)) if error.code == v1::ErrorCode::NotReady))
        })
        .unwrap();
    // The actor must still answer commands after dropping only the failed link.
    nodes[0].group.call(|_| Ok(())).unwrap();
    stop(&nodes).await;
    assert!(
        survived,
        "greeting reply teardown panicked instead of returning a link error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_sleeping_owner_rejects_incoming_member_connection() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    for node in &nodes {
        node.group.sleep();
        node.group.call(|_| Ok(())).unwrap();
    }
    assert!(until(|| nodes.iter().all(|node| node.links.links().links.is_empty())).await);
    // Only the remote owner wakes; it reconnects using its retained address.
    nodes[1].group.wake();
    let admitted_while_asleep = tokio::time::timeout(Duration::from_secs(3), async {
        while !ready(&nodes) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    stop(&nodes).await;
    assert!(
        !admitted_while_asleep,
        "sleeping config owner admitted an incoming member and restored Ready before wake"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_sleeping_owner_does_not_reschedule_expired_redial_forever() {
    let nodes = start(fresh(1)).await;
    let (before, after) = nodes[0]
        .group
        .call(|owner| {
            owner.sleep();
            // A Lost event after sleep leaves the normal reconnect deadline behind.
            owner.dials.insert(
                DeviceId([99; 32]),
                Dial {
                    after: Some(Instant::now() - Duration::from_secs(1)),
                    ..Default::default()
                },
            );
            owner.tick();
            let before = owner.next_deadline() > Instant::now();
            owner.tick();
            let after = owner.next_deadline() > Instant::now();
            Ok((before, after))
        })
        .unwrap();
    stop(&nodes).await;
    assert!(
        before && after,
        "sleeping owner repeatedly returns an expired redial deadline, making sleep_until immediately ready"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_facts_advertise_the_explicit_ipv4_listener_address() {
    let nodes = start(same_group(1)).await;
    let port = nodes[0].links.local_addr().port();
    let endpoints = nodes[0]
        .group
        .call(|owner| {
            owner.observe_endpoint_facts()?;
            Ok(owner
                .local
                .value()
                .endpoints()
                .unwrap()
                .0
                .into_iter()
                .map(|endpoint| (endpoint.address().unwrap().0, endpoint.port().unwrap()))
                .collect::<Vec<_>>())
        })
        .unwrap();
    stop(&nodes).await;
    assert_eq!(
        endpoints,
        vec![(std::net::Ipv4Addr::LOCALHOST.octets().to_vec(), port)],
        "a loopback-only listener cannot answer its host's LAN addresses"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_wake_refreshes_endpoint_facts_without_display_change() {
    use oink_protocol::v1::schema::WireValue;
    let nodes = start_with_bind(fresh(1), None, std::net::Ipv4Addr::UNSPECIFIED.into()).await;
    let current = oink_network::discovery::local_ipv4()
        .into_iter()
        .map(|(ip, _)| ip.octets().to_vec())
        .collect::<Vec<_>>();
    assert!(!current.is_empty(), "this test needs an IPv4 interface to observe");
    let endpoints = nodes[0]
        .group
        .call(|owner| {
            owner.sleep();
            // Retained facts describe the pre-sleep interface. The OS observer now
            // sees current interfaces; no display event occurs on a network change.
            let stale = dto::EndpointFact::from_raw(
                [
                    (1, dto::AddressFamily::Ipv4.write()),
                    (2, RawValue::Bytes(vec![192, 0, 2, 254])),
                    (3, owner.links.local_addr().port().write()),
                    (4, 0u32.write()),
                ]
                .into(),
            )
            .unwrap();
            owner
                .local
                .edit(|raw| {
                    raw.insert(10, RawValue::Array(vec![stale.write()]));
                    Ok(())
                })
                .unwrap();
            owner.last_facts = Instant::now() - FACTS_INTERVAL;
            owner.wake();
            Ok(owner
                .local
                .value()
                .endpoints()
                .unwrap()
                .0
                .into_iter()
                .map(|endpoint| endpoint.address().unwrap().0)
                .collect::<Vec<_>>())
        })
        .unwrap();
    stop(&nodes).await;
    assert_eq!(
        endpoints, current,
        "wake must refresh interface endpoints independently of display changes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn migration_direct_upgrade_remembers_successful_endpoint_durably() {
    let nodes = start_with_hints(same_group(3), Some(&[(0, 1), (1, 0), (0, 2), (2, 0)])).await;
    assert!(until(|| ready(&nodes)).await);
    let peer = nodes[2].identity.id();
    let endpoint = nodes[2].links.local_addr();
    assert!(matches!(
        nodes[1].links.links().by_peer(peer).unwrap().path,
        LinkPath::Relayed { .. }
    ));
    let group = nodes[2].group.applied().group;
    nodes[1]
        .group
        .call(move |owner| {
            owner.sighting(Sighting {
                advert: Advert::local(peer, "synthetic".into(), group, 3, endpoint.port(), "test".into()),
                addrs: vec![endpoint],
                source: oink_network::discovery::Source::Manual,
            });
            Ok(())
        })
        .unwrap();
    assert!(
        until(|| nodes[1]
            .group
            .call(move |owner| Ok(owner
                .sessions
                .values()
                .any(|session| session.peer_boot.device_id == peer
                    && session.path == LinkPath::Direct
                    && session.remote == Some(endpoint))))
            .unwrap())
        .await,
        "the owner must first consume the successful PathChanged event"
    );
    let remembered = super::super::local_records::AddressStore::load(nodes[1].directory.path())
        .unwrap()
        .candidates(peer);
    stop(&nodes).await;
    assert!(
        remembered.contains(&endpoint),
        "successful direct upgrade was not retained in addresses.json: {remembered:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_endpoint_poll_refreshes_without_wake_or_display_events() {
    let nodes = start_with_bind(fresh(1), None, std::net::Ipv4Addr::UNSPECIFIED.into()).await;
    let current = oink_network::discovery::local_ipv4()
        .into_iter()
        .map(|(ip, _)| ip.octets().to_vec())
        .collect::<Vec<_>>();
    assert!(!current.is_empty(), "this test needs an IPv4 interface to observe");
    let (endpoints, changed, unchanged) = nodes[0]
        .group
        .call(|owner| {
            owner
                .local
                .edit(|raw| {
                    raw.insert(10, RawValue::Array(vec![]));
                    Ok(())
                })
                .unwrap();
            let before = owner.local.value().fact_sequence().unwrap();
            owner.network_checked = Instant::now() - Duration::from_secs(2);
            owner.tick();
            let refreshed = owner.local.value().fact_sequence().unwrap();
            let endpoints = owner
                .local
                .value()
                .endpoints()
                .unwrap()
                .0
                .into_iter()
                .map(|endpoint| endpoint.address().unwrap().0)
                .collect::<Vec<_>>();
            owner.network_checked = Instant::now() - Duration::from_secs(2);
            owner.tick();
            Ok((
                endpoints,
                refreshed > before,
                owner.local.value().fact_sequence().unwrap() == refreshed,
            ))
        })
        .unwrap();
    stop(&nodes).await;
    assert_eq!(endpoints, current);
    assert!(changed, "a network change must advance the facts sequence");
    assert!(unchanged, "unchanged interfaces must not churn facts sequences");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_presence_records_local_disconnect_time_and_survives_reload() {
    for retire in [false, true] {
        let nodes = start(same_group(2)).await;
        assert!(until(|| ready(&nodes)).await);
        let peer = nodes[1].identity.id();
        let (before, observed, after, persisted) = nodes[0]
            .group
            .call(move |owner| {
                let group = owner.state.roster().anchor().group;
                // Model a long-lived quiet session without waiting or using peer clocks.
                owner.presence.observe(group, peer, 1).unwrap();
                let link = owner
                    .sessions
                    .iter()
                    .find(|(_, s)| s.ready && s.peer_boot.device_id == peer)
                    .unwrap()
                    .0;
                let before = now_ms();
                let link = *link;
                if retire {
                    // A live transport returning to admission is offline too.
                    owner.sessions.get_mut(&link).unwrap().ready = false;
                    owner.links.retire(link);
                    owner.publish()?;
                } else {
                    owner.close(link, Disconnect::HandshakeFailed);
                }
                let after = now_ms();
                let observed = owner
                    .shared
                    .members
                    .load()
                    .members
                    .iter()
                    .find(|m| m.device == peer)
                    .unwrap()
                    .last_connected_ms;
                let persisted = crate::config::records::PresenceStore::load(&owner.data_dir)
                    .unwrap()
                    .last_connected(group, peer);
                owner.publish()?;
                assert_eq!(
                    owner.presence.last_connected(group, peer),
                    observed,
                    "publishing an already offline peer must not refresh last seen"
                );
                Ok((before, observed, after, persisted))
            })
            .unwrap();
        stop(&nodes).await;
        assert!(
            observed.is_some_and(|time| (before..=after).contains(&time)),
            "disconnect time: {observed:?}, expected {before}..={after}"
        );
        assert_eq!(persisted, observed);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_layout_retains_cached_visibility_without_routing_authority() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    let peer = nodes[1].identity.id();
    nodes[1].group.initialize_displays(vec![display("retained")]).unwrap();
    assert!(
        until(|| nodes[0]
            .group
            .call(move |owner| Ok(owner.facts.contains_key(&peer)
                && owner
                    .shared
                    .applied
                    .load()
                    .layout
                    .screens
                    .iter()
                    .any(|s| s.id.device == peer)))
            .unwrap())
        .await
    );
    let (hidden, disabled, router_has_owner, routable, reloaded_hidden) = nodes[0]
        .group
        .call(move |owner| {
            let mut raw = owner.facts[&peer].raw().clone();
            let mut preferences = owner.facts[&peer].preferences().unwrap().unwrap().0;
            for preference in &mut preferences {
                let mut fields = preference.raw().clone();
                match preference.option().unwrap() {
                    dto::LocalOption::ScreenShared => {
                        fields.insert(3, false.write());
                    }
                    dto::LocalOption::ScreenPreferences => {
                        fields.insert(
                            3,
                            dto::ScreenPreferencesValue::from_raw(
                                [
                                    (2, 1.0f64.write()),
                                    (
                                        1,
                                        RawValue::Array(vec![
                                            dto::ScreenPreference::from_raw(
                                                [
                                                    (1, RawValue::Text("retained".into())),
                                                    (2, 1.0f64.write()),
                                                    (3, false.write()),
                                                ]
                                                .into(),
                                            )
                                            .unwrap()
                                            .write(),
                                        ]),
                                    ),
                                ]
                                .into(),
                            )
                            .unwrap()
                            .write(),
                        );
                    }
                    _ => {}
                }
                *preference = dto::DevicePreference::from_raw(fields).unwrap();
            }
            raw.insert(
                18,
                RawValue::Array(preferences.into_iter().map(|p| p.write()).collect()),
            );
            let facts = dto::DeviceFacts::from_raw(raw).unwrap();
            owner.cache.observe(facts).unwrap();
            owner.cache.flush().unwrap();
            let links = owner
                .sessions
                .iter()
                .filter(|(_, s)| s.peer_boot.device_id == peer)
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            for link in links {
                owner.close(link, Disconnect::HandshakeFailed);
            }
            let applied = owner.shared.applied.load();
            let geometry = crate::api::layout_geometry(
                &applied.layout,
                &applied.owners,
                |_| false,
                |_| {
                    Some(crate::api::ScreenMeta {
                        device_name: "Remote".into(),
                        os: crate::api::Os::Windows,
                        display: None,
                    })
                },
            );
            let screen = geometry.screens.iter().find(|s| s.key.device == peer.into()).unwrap();
            assert!(screen.hidden && screen.device_disabled && !screen.online);
            let retained = applied.owners.get(&peer);
            let hidden = retained
                .and_then(|o| o.screens.get("retained"))
                .is_some_and(|s| !s.enabled);
            let disabled = retained.is_some_and(|o| !o.screen_shared);
            let router = owner.shared.router.load();
            let router_has_owner = router.owners.contains_key(&peer);
            let routable = router.devices.contains(&peer);
            // Reload the existing schema-1 cache, as on a backend restart.
            owner.cache = super::super::local_records::FactsCache::load(&owner.data_dir).unwrap();
            owner.publish()?;
            let reloaded_hidden = owner
                .shared
                .applied
                .load()
                .owners
                .get(&peer)
                .and_then(|o| o.screens.get("retained"))
                .is_some_and(|s| !s.enabled);
            Ok((hidden, disabled, router_has_owner, routable, reloaded_hidden))
        })
        .unwrap();
    stop(&nodes).await;
    assert!(
        hidden,
        "offline projection discarded the cached hidden screen preference"
    );
    assert!(disabled, "offline projection discarded screen sharing disabled");
    assert!(
        !router_has_owner && !routable,
        "cached preferences granted live routing authority"
    );
    assert!(reloaded_hidden);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn member_timeouts_keep_next_attempt_within_five_seconds() {
    let nodes = start(fresh(1)).await;
    nodes[0]
        .group
        .call(|owner| {
            let peer = DeviceId([99; 32]);
            for attempt in 1..=20 {
                owner.network(ConfigEvent::DialFailed {
                    peer,
                    reason: oink_network::link::DialFailure::Unreachable,
                });
                let wait = owner.dials[&peer]
                    .after
                    .unwrap()
                    .saturating_duration_since(Instant::now());
                assert!(
                    wait <= Duration::from_secs(5),
                    "timeout {attempt}: next attempt is {wait:?} away"
                );
            }
            Ok(())
        })
        .unwrap();
    stop(&nodes).await;
}

#[test]
fn member_retry_clock_picks_up_reachability_within_five_seconds_after_many_timeouts() {
    let mut dial = Dial::default();
    let mut now = Instant::now();
    let local = DeviceId([2; 32]);
    let peer = DeviceId([1; 32]);
    let expected = [500, 1000, 2000, 4000, 5000];
    for attempt in 0..100 {
        dial.failed(DialFailure::Unreachable, now);
        let next = dial.after.unwrap();
        assert_eq!(next - now, Duration::from_millis(expected[attempt.min(4)]));
        // The peer returns just after the failed attempt. Drive the same gate
        // used by tick with explicit time, without relying on wall-clock sleeps.
        let reachable = now + Duration::from_nanos(1);
        assert!(!dial.due(local, peer, reachable));
        assert!(dial.due(local, peer, next));
        assert!(next - reachable <= Duration::from_secs(5));
        dial.active = true;
        assert!(!dial.due(local, peer, next));
        now = next + oink_network::link::DIAL_BUDGET;
    }
}

#[test]
fn larger_id_waits_only_on_its_initial_round_and_rejections_keep_slow_backoff() {
    let now = Instant::now();
    let smaller = DeviceId([1; 32]);
    let larger = DeviceId([2; 32]);
    assert!(Dial::default().due(smaller, larger, now));
    let mut dial = Dial::default();
    assert!(!dial.due(larger, smaller, now));
    assert!(dial.due(larger, smaller, now + Duration::from_secs(1)));
    dial.failed(DialFailure::Unreachable, now);
    assert!(dial.due(larger, smaller, now + Duration::from_millis(500)));
    for reason in [DialFailure::IdentityChanged, DialFailure::Refused, DialFailure::Stopped] {
        let mut dial = Dial::default();
        for expected in [5, 15, 60, 300, 300] {
            dial.failed(reason, now);
            assert_eq!(dial.after.unwrap() - now, Duration::from_secs(expected));
        }
        dial.reset(now);
        assert_eq!(dial.failures, 0);
        assert!(dial.due(larger, smaller, now));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_network_change_and_wake_reset_member_retry_immediately() {
    let nodes = start(fresh(1)).await;
    nodes[0]
        .group
        .call(|owner| {
            let peer = DeviceId([99; 32]);
            let fail = |owner: &mut Owner| {
                for _ in 0..8 {
                    owner.network(ConfigEvent::DialFailed {
                        peer,
                        reason: DialFailure::Refused,
                    });
                }
                assert!(owner.dials[&peer].after.unwrap() > Instant::now() + Duration::from_secs(60));
            };
            fail(owner);
            owner.network(ConfigEvent::Lost {
                link: LinkId(u64::MAX),
                peer,
                path: LinkPath::Direct,
                reason: oink_network::link::LinkLost::ConnectionLost,
            });
            assert_eq!(owner.dials[&peer].failures, 0);
            assert!(
                owner
                    .dials
                    .get_mut(&peer)
                    .unwrap()
                    .due(owner.identity.id(), peer, Instant::now())
            );

            fail(owner);
            // Model changed interface facts; tick observes the actual loopback
            // listener and must clear the old retry delay in this same turn.
            owner
                .local
                .edit(|raw| {
                    raw.insert(10, RawValue::Array(vec![]));
                    Ok(())
                })
                .unwrap();
            owner.network_checked = Instant::now() - Duration::from_secs(2);
            owner.tick();
            assert_eq!(owner.dials[&peer].failures, 0);
            assert!(
                owner
                    .dials
                    .get_mut(&peer)
                    .unwrap()
                    .due(owner.identity.id(), peer, Instant::now())
            );

            fail(owner);
            owner.sleep();
            owner.wake();
            assert_eq!(owner.dials[&peer].failures, 0);
            assert!(
                owner
                    .dials
                    .get_mut(&peer)
                    .unwrap()
                    .due(owner.identity.id(), peer, Instant::now())
            );
            Ok(())
        })
        .unwrap();
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_discovery_does_not_bypass_member_rejection_backoff() {
    let nodes = start(same_group(2).into_iter().take(1).collect()).await;
    nodes[0]
        .group
        .call(|owner| {
            let peer = identities(2)[1].id();
            let sighting = Sighting {
                advert: Advert::local(
                    peer,
                    "synthetic".into(),
                    owner.state.roster().anchor().group,
                    2,
                    47470,
                    "test".into(),
                ),
                addrs: vec!["192.0.2.30:47470".parse().unwrap()],
                source: oink_network::discovery::Source::Mdns,
            };
            owner.sighting(sighting.clone());
            for _ in 0..8 {
                owner.network(ConfigEvent::DialFailed {
                    peer,
                    reason: DialFailure::IdentityChanged,
                });
            }
            let deadline = owner.dials[&peer].after;
            owner.sighting(sighting.clone());
            assert_eq!(owner.dials[&peer].after, deadline);
            let mut changed = sighting;
            changed.addrs = vec!["192.0.2.31:47470".parse().unwrap()];
            owner.sighting(changed);
            assert_eq!(owner.dials[&peer].failures, 0);
            assert!(owner.dials[&peer].after.unwrap() <= Instant::now());
            Ok(())
        })
        .unwrap();
    stop(&nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redial_fixture_preserves_retry_when_loss_precedes_sleep() {
    let nodes = start(same_group(2).into_iter().take(1).collect()).await;
    let peer = identities(2)[1].id();
    nodes[0]
        .group
        .call(move |owner| {
            // The other member's sleep may deliver Lost before this member's
            // setup command. Keep both steps in one owner turn to force that
            // ordering without depending on network or executor scheduling.
            owner.network(ConfigEvent::Lost {
                link: LinkId(u64::MAX),
                peer,
                path: LinkPath::Direct,
                reason: oink_network::link::LinkLost::ConnectionLost,
            });
            let retry = owner.dials[&peer].after;
            assert!(retry.is_some());
            sleep_for_redial_test(owner);
            assert_eq!(
                owner.dials[&peer].after, retry,
                "fixture must preserve the processed loss"
            );
            Ok(())
        })
        .unwrap();
    stop(&nodes).await;
}

fn sleep_for_redial_test(owner: &mut Owner) {
    // Lost may precede this member's setup command after the other member
    // sleeps. Preserve its retry: an already retired link produces no further
    // Lost to restore an erased deadline, and sleeping suppresses redial ticks.
    owner.sleep();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reachable_member_recovers_on_its_own_retry_after_many_timeouts() {
    let nodes = start(same_group(2)).await;
    assert!(until(|| ready(&nodes)).await);
    for node in &nodes {
        node.group
            .call(|owner| {
                sleep_for_redial_test(owner);
                Ok(())
            })
            .unwrap();
    }
    assert!(
        until(|| nodes.iter().all(|node| node.links.links().links.is_empty()
            && node
                .group
                .call(|owner| Ok(
                    owner.sessions.is_empty() && owner.dials.values().all(|dial| dial.after.is_some() && !dial.active)
                ))
                .unwrap()))
        .await
    );
    let source = nodes[0].identity.id();
    let target = nodes[1].identity.id();
    nodes[1]
        .group
        .call(move |owner| {
            // The returned peer can accept incoming connections, but cannot
            // originate one. Recovery must come from the source's own retry.
            owner.sleeping = false;
            owner.dials.entry(source).or_default().active = true;
            owner.network_checked = Instant::now() + Duration::from_secs(60);
            Ok(())
        })
        .unwrap();
    nodes[0]
        .group
        .call(move |owner| {
            owner.network_checked = Instant::now() + Duration::from_secs(60);
            for _ in 0..20 {
                owner.network(ConfigEvent::DialFailed {
                    peer: target,
                    reason: DialFailure::Unreachable,
                });
            }
            owner.sleeping = false;
            let now = Instant::now();
            assert!(owner.dials[&target].after.unwrap() <= now + Duration::from_secs(5));
            assert!(owner.next_deadline() <= now + Duration::from_secs(5));
            Ok(())
        })
        .unwrap();
    let began = Instant::now();
    // Five seconds for the idle retry, plus one second of loopback TLS/C0
    // test allowance. The exact scheduling bound is checked without sleeps above.
    let recovered = tokio::time::timeout(Duration::from_secs(6), async {
        while !ready(&nodes) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok();
    let elapsed = began.elapsed();
    let outbound = nodes[0]
        .group
        .call(move |owner| {
            Ok(owner
                .sessions
                .values()
                .find(|link| link.peer_boot.device_id == target)
                .map(|link| link.direction))
        })
        .unwrap();
    stop(&nodes).await;
    assert!(recovered, "the source's own retry did not recover in {elapsed:?}");
    assert_eq!(outbound, Some(Direction::Outbound));
    eprintln!("own retry after 20 timeouts reached Ready in {elapsed:?}");
}
