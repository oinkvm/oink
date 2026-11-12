//! Real C0/QUIC/relay and the real native owner with a memory backend. The test
//! drives I0 delivery and the opening clock explicitly; it never sleeps.
use super::*;
use crate::config_module::{ConfigDeps, ConfigModule, group_store, local_records::AddressStore};
use crate::threads::input::{InputSnapshots, InputThread};
use oink_foundation::{realtime::Tier, session::Boot};
use oink_network::{
    identity::Identity,
    link::{LinkConfig, ModulePort, testing},
};
use std::time::Duration;

struct Application;
impl crate::config_module::ConfigApplication for Application {
    fn apply_state(&self, _: &group_store::StoredGroupState) -> Result<(), String> {
        Ok(())
    }
}
struct Running {
    owner: InputStreams,
    port: ModulePort,
    changes: crate::config_module::ConfigChanges,
    notices: InputNotices,
    _directory: tempfile::TempDir,
    _main: crate::threads::MainReceiver,
}
impl Running {
    async fn step(&mut self) {
        let input = self.owner.input.clone();
        let retry = self.owner.peers.values().filter_map(|p| p.machine.deadline()).min();
        tokio::select! {
            biased;
            event = self.port.next() => match event.unwrap() {
                ModuleEvent::LinkReady { link, peer } => self.owner.link_ready(link, peer),
                ModuleEvent::LinkLost { link, peer, reason } => self.owner.link_lost(link, peer, reason),
                ModuleEvent::Inbound(stream) => self.owner.inbound(stream),
                ModuleEvent::StreamClosed { stream, reason } => self.owner.stream_closed(stream, reason),
                _ => {},
            },
            notice = self.notices.next() => self.owner.notice(notice.unwrap()),
            change = self.changes.next() => if let Some(crate::config_module::ConfigChange::Facts { device, capabilities_changed }) = change {
                self.owner.facts_changed(device, capabilities_changed);
            } else {
                self.owner.config_state_changed();
            },
            _ = input.status_changed() => {},
            // Production wakes at StreamMachine's retry deadline. The manually
            // driven owner must run that same transition after pause/reset too.
            // Deliver queued events first, then advance only this supplied clock.
            _ = async { if retry.is_none() { std::future::pending::<()>().await } } => {
                self.owner.poll_at(retry.unwrap());
            },
        }
    }
    fn attached(&self, peer: DeviceId) -> bool {
        self.owner
            .input
            .status()
            .peers
            .iter()
            .any(|p| p.peer == peer && p.attached)
    }
}
async fn step(nodes: &mut [Running]) {
    let [a, b, c] = nodes else { unreachable!() };
    tokio::select! { _ = a.step() => {}, _ = b.step() => {}, _ = c.step() => {} }
}
async fn start() -> Vec<Running> {
    start_with(false, true).await
}
/// Node 0 relays between nodes 1 and 2, whose direct path is blocked.
/// `old_relay` makes node 0 a peer that does not offer sync unlock.
async fn start_with(old_relay: bool, hold_headers: bool) -> Vec<Running> {
    let identities: Vec<_> = (1..=3).map(|seed| Arc::new(Identity::from_seed([seed; 32]))).collect();
    let states = crate::config_module::runtime_tests::same_group(3);
    let mut pending = Vec::new();
    for (n, (identity, state)) in identities.iter().zip(states).enumerate() {
        let directory = tempfile::tempdir().unwrap();
        group_store::save(&directory.path().join(group_store::STATE_FILE), &state).unwrap();
        let network = oink_test_support::network_config(directory.path().into(), "synthetic", 0);
        let mut profile = crate::node::Node::offer_profile();
        if old_relay && n == 0 {
            profile.capabilities.retain(|c| c != "oink.auto-unlock.sync/1");
        }
        let (links, port) = oink_network::link::LinkLayer::start(LinkConfig {
            profile,
            identity: identity.clone(),
            boot: Boot {
                device_id: identity.id(),
                nonce: [n as u8 + 1; 16],
            },
            bind: (network.bind_ip, network.port).into(),
            tier: Tier::Elevated,
        })
        .await
        .unwrap();
        if n > 0 {
            let peer = identities[3 - n].id();
            testing::set_direct_blocked(&links, peer, true);
            if hold_headers {
                testing::hold_input_header(&links, Some(peer));
            }
        }
        let input_port = links.register(KINDS);
        pending.push((directory, identity.clone(), network, links, port, input_port));
    }
    let addresses: Vec<_> = pending
        .iter()
        .map(|(_, identity, _, links, _, _)| (identity.id(), links.local_addr()))
        .collect();
    let mut nodes = Vec::new();
    for (directory, identity, network, links, port, input_port) in pending {
        let mut hints = AddressStore::load(directory.path()).unwrap();
        for (peer, address) in &addresses {
            if *peer != identity.id() {
                hints.remember(*peer, *address).unwrap();
            }
        }
        let config = Arc::new(std::sync::Mutex::new(
            crate::config::AgentConfig::load_from_with_device_name(directory.path(), "synthetic".into()).unwrap(),
        ));
        let config = ConfigModule::start_with_config(
            ConfigDeps {
                data_dir: directory.path().into(),
                links: links.clone(),
                port,
                identity,
                application: Arc::new(Application),
                config,
                owner_gate: Arc::new(std::sync::Mutex::new(())),
            },
            network,
        )
        .unwrap();
        let backend = Arc::new(oink_test_support::input::FakeInput::new());
        let inject = crate::threads::inject::start_with_backend(backend, Box::new(|_| {})).unwrap();
        let snapshots = InputSnapshots {
            mode_switch_in_callback: true,
            capture_reports_key_repeat: false,
            local_input_transform: oink_foundation::input_transform::InputTransform::initial(std::env::consts::OS),
            layout: config.router_layout(),
            settings: Arc::new(oink_foundation::snapshot::Snapshot::default()),
            environment: Arc::new(oink_foundation::snapshot::Snapshot::default()),
        };
        let (main, receive_main) = crate::threads::main_channel();
        let (input, notices) = InputThread::start(links.boot(), snapshots, inject, main).unwrap();
        let changes = config.subscribe();
        nodes.push(Running {
            owner: InputStreams::new(links, config, Arc::new(input), true),
            port: input_port,
            changes,
            notices,
            _directory: directory,
            _main: receive_main,
        });
    }
    nodes
}

#[test]
fn lost_cutover_opening_recovers_via_a_fresh_link_at_the_opening_deadline() {
    crate::test_watchdog::run(
        module_path!(),
        "lost_cutover_opening_recovers_via_a_fresh_link_at_the_opening_deadline",
        Some(4),
        async {
            let mut nodes = start().await;
            let b = nodes[1].owner.links.clone();
            let c = nodes[2].owner.links.clone();
            // Receive both relay I0 headers but hold them before their reader IDs
            // exist. C0 and N0 keep running and can overtake these openings.
            let held_b = testing::input_header_held(&b);
            let held_c = testing::input_header_held(&c);
            tokio::pin!(held_b, held_c);
            let (mut seen_b, mut seen_c) = (false, false);
            while !seen_b || !seen_c {
                tokio::select! {
                    _ = &mut held_b, if !seen_b => seen_b = true,
                    _ = &mut held_c, if !seen_c => seen_c = true,
                    _ = step(&mut nodes) => {},
                }
            }
            let old_b = b.links().by_peer(c.id()).unwrap().link;
            let old_c = c.links().by_peer(b.id()).unwrap().link;
            assert!(matches!(
                b.links().by_peer(c.id()).unwrap().path,
                oink_network::link::LinkPath::Relayed { .. }
            ));
            testing::set_direct_blocked(&b, c.id(), false);
            testing::set_direct_blocked(&c, b.id(), false);
            b.update_addresses(c.id(), vec![c.local_addr()]);
            tokio::join!(testing::cutover_finished(&b), testing::cutover_finished(&c));
            // Releasing the old headers after cancellation deterministically loses
            // the openings: the new writers have already consumed their old queues.
            testing::hold_input_header(&b, None);
            testing::hold_input_header(&c, None);
            testing::flush_commands(&b);
            testing::flush_commands(&c);
            while !nodes[0].attached(b.id())
                || !nodes[0].attached(c.id())
                || !nodes[1].attached(nodes[0].owner.links.id())
                || !nodes[2].attached(nodes[0].owner.links.id())
            {
                step(&mut nodes).await;
            }
            for (node, peer) in [(&nodes[1], c.id()), (&nodes[2], b.id())] {
                let status = node.owner.input.status();
                let pair = status.peers.iter().find(|p| p.peer == peer).unwrap();
                assert!(!pair.attached);
                assert!(pair.outbound.is_some());
                assert!(pair.inbound.is_none());
                assert_eq!(pair.opening_received, 0);
            }
            let alpha_links: Vec<_> = nodes[0].owner.links.links().links.iter().map(|l| l.link).collect();
            // The ordinary owner is not spawned: advance only its supplied clock,
            // without any wall-clock sleep, periodic Facts or automatic time advance.
            let since = nodes[1].owner.peers[&c.id()].wait_since.unwrap();
            nodes[1]
                .owner
                .report_waits(since + Duration::from_secs(3) - Duration::from_nanos(1));
            testing::flush_commands(&b);
            assert!(b.links().links.iter().any(|l| l.link == old_b), "no early reset");
            nodes[1].owner.report_waits(since + Duration::from_secs(3));
            testing::flush_commands(&b);
            assert!(
                !b.links().links.iter().any(|l| l.link == old_b),
                "the opening deadline must retire the stalled link"
            );
            while !nodes[1].attached(c.id()) || !nodes[2].attached(b.id()) {
                step(&mut nodes).await;
            }
            assert_ne!(b.links().by_peer(c.id()).unwrap().link, old_b);
            assert_ne!(c.links().by_peer(b.id()).unwrap().link, old_c);
            for node in &nodes {
                for peer in &nodes {
                    if node.owner.links.id() != peer.owner.links.id() {
                        assert!(node.attached(peer.owner.links.id()));
                    }
                }
            }
            assert_eq!(
                nodes[0]
                    .owner
                    .links
                    .links()
                    .links
                    .iter()
                    .map(|l| l.link)
                    .collect::<Vec<_>>(),
                alpha_links,
                "the reset must leave the other two healthy links intact"
            );
            for node in &nodes {
                node.owner.input.stop();
                node.owner.config.stop().await;
                node.owner.links.stop().await;
            }
        },
    );
}

/// The device-control owner's order (`apply_pause_request`): the native owner
/// applies the decision first, then the owner publishes it in its local facts.
fn pause(owner: &InputStreams, paused: bool) {
    owner.input.apply_owner_pause_for_test(paused);
    publish_pause(&owner.config, paused);
}
fn publish_pause(config: &ConfigModule, paused: bool) {
    use oink_protocol::v1::{
        dto,
        schema::{WireDto, WireValue},
    };
    let facts = config.local_facts();
    let old = facts.pause().unwrap().unwrap();
    let state = dto::LocalPauseState::from_raw(
        [
            (1, paused.write()),
            (
                2,
                (if paused {
                    dto::PauseReason::LocalUser
                } else {
                    dto::PauseReason::None
                })
                .write(),
            ),
            (3, (old.revision().unwrap() + 1).write()),
        ]
        .into(),
    )
    .unwrap();
    config
        .publish_local_owner(facts.preferences().unwrap().unwrap().0, state)
        .unwrap();
}

/// Sync unlock is independent of the network path (user decision Q8): the
/// hint, the source challenge and its answer all travel on the one relayed
/// logical session, the source identity is the session peer (not the relay),
/// and the relay itself does not need the new capability.
#[test]
fn sync_unlock_round_trips_over_a_relay_that_lacks_the_capability() {
    crate::test_watchdog::run(
        module_path!(),
        "sync_unlock_round_trips_over_a_relay_that_lacks_the_capability",
        Some(6),
        sync_round_trip(false, false),
    );
}
/// The relay's own pause only closes its own input sessions; it keeps carrying
/// the B↔C session (user decision Q10 applies to the two endpoints).
#[test]
fn sync_unlock_round_trips_while_the_relay_itself_is_paused() {
    crate::test_watchdog::run(
        module_path!(),
        "sync_unlock_round_trips_while_the_relay_itself_is_paused",
        Some(6),
        sync_round_trip(false, true),
    );
}
/// A relay to direct migration keeps the logical session: a challenge sent on
/// the relayed path is answered on the direct one, never as a lost source.
#[test]
fn sync_unlock_challenge_survives_relay_to_direct_migration() {
    crate::test_watchdog::run(
        module_path!(),
        "sync_unlock_challenge_survives_relay_to_direct_migration",
        Some(6),
        sync_round_trip(true, false),
    );
}
async fn sync_round_trip(migrate: bool, relay_paused: bool) {
    use crate::auto_unlock::sync::SourceAnswer;
    use crate::threads::{MainCommand, input::InputCommand};
    let mut nodes = start_with(true, false).await;
    let relay = nodes[0].owner.links.id();
    let b = nodes[1].owner.links.clone();
    let c = nodes[2].owner.links.clone();
    crate::test_watchdog::phase("relayed input attachment");
    while !nodes[1].attached(c.id())
        || !nodes[2].attached(b.id())
        || !nodes[1].attached(relay)
        || !nodes[2].attached(relay)
    {
        step(&mut nodes).await;
    }
    assert!(matches!(
        b.links().by_peer(c.id()).unwrap().path,
        oink_network::link::LinkPath::Relayed { .. }
    ));
    if relay_paused {
        crate::test_watchdog::phase("relay pauses its own sharing");
        pause(&nodes[0].owner, true);
        while !nodes[0].owner.paused || nodes[1].attached(relay) || nodes[2].attached(relay) {
            step(&mut nodes).await;
        }
        assert!(nodes[1].attached(c.id()) && nodes[2].attached(b.id()));
    }
    for node in &nodes {
        while node._main.try_recv().is_ok() {}
    }
    // Drives the three owners until `found` matches a main command of
    // node `index`; the input threads run on their own threads.
    async fn until<T>(nodes: &mut [Running], index: usize, mut found: impl FnMut(MainCommand) -> Option<T>) -> T {
        loop {
            while let Ok(command) = nodes[index]._main.try_recv() {
                if let Some(value) = found(command) {
                    return value;
                }
            }
            let _ = tokio::time::timeout(Duration::from_millis(10), step(nodes)).await;
        }
    }
    crate::test_watchdog::phase("hint from B reaches C through the relay");
    nodes[1]
        .owner
        .input
        .send(InputCommand::SyncUnlockBroadcast { epoch: 41 });
    let (source, epoch) = until(&mut nodes, 2, |command| match command {
        MainCommand::SyncUnlock { source, epoch } => Some((source, epoch)),
        _ => None,
    })
    .await;
    assert_eq!(
        (source, epoch),
        (b.id(), 41),
        "source is the session peer, not the relay"
    );
    crate::test_watchdog::phase("C challenges B on the same relayed session");
    nodes[2].owner.input.send(InputCommand::SessionLocked(Some(true)));
    let (reply, answer) = std::sync::mpsc::sync_channel(1);
    let asked = Instant::now();
    nodes[2]
        .owner
        .input
        .send(InputCommand::SyncUnlockQuery { source, reply });
    let (peer, stream, query) = until(&mut nodes, 1, |command| match command {
        MainCommand::UnlockQuery { peer, stream, epoch } => Some((peer, stream, epoch)),
        _ => None,
    })
    .await;
    assert_eq!(peer, c.id());
    if migrate {
        crate::test_watchdog::phase("relay to direct migration during the challenge");
        testing::set_direct_blocked(&b, c.id(), false);
        testing::set_direct_blocked(&c, b.id(), false);
        b.update_addresses(c.id(), vec![c.local_addr()]);
        tokio::join!(testing::cutover_finished(&b), testing::cutover_finished(&c));
        assert!(!matches!(
            b.links().by_peer(c.id()).unwrap().path,
            oink_network::link::LinkPath::Relayed { .. }
        ));
    }
    // B's main thread answers for its own (here: unlocked) session.
    nodes[1].owner.input.send(InputCommand::UnlockReplyReady {
        peer,
        stream,
        epoch: query,
        unlocked: true,
    });
    let started = Instant::now();
    let answer = loop {
        if let Ok(answer) = answer.try_recv() {
            break answer;
        }
        let _ = tokio::time::timeout(Duration::from_millis(10), step(&mut nodes)).await;
    };
    // The reply crosses the migrated session at once. Migration never
    // cancels the challenge or reports a lost source; only the source's
    // own two-second window decides (this harness' cutover is slower).
    let expected = if asked.elapsed() <= crate::auto_unlock::sync::SOURCE_REPLY {
        SourceAnswer::Unlocked
    } else {
        SourceAnswer::Locked
    };
    assert!(migrate || expected == SourceAnswer::Unlocked);
    assert_eq!(
        answer,
        expected,
        "answered {:?} after the challenge, {:?} after the reply",
        asked.elapsed(),
        started.elapsed()
    );
    assert!(started.elapsed() < crate::auto_unlock::sync::SOURCE_REPLY);
    // The relay negotiated no sync capability with either endpoint and
    // never received the hint itself.
    assert!(
        !std::iter::from_fn(|| nodes[0]._main.try_recv().ok())
            .any(|command| matches!(command, MainCommand::SyncUnlock { .. }))
    );
    crate::test_watchdog::phase("input and network shutdown");
    for node in &nodes {
        node.owner.input.stop();
        node.owner.config.stop().await;
        node.owner.links.stop().await;
    }
}

#[test]
fn remote_pause_cancels_the_opening_deadline_and_resume_keeps_the_link() {
    crate::test_watchdog::run(
        module_path!(),
        "remote_pause_cancels_the_opening_deadline_and_resume_keeps_the_link",
        Some(4),
        async {
            let mut nodes = start().await;
            let b = nodes[1].owner.links.clone();
            let c = nodes[2].owner.links.clone();
            testing::hold_input_header(&b, None);
            testing::hold_input_header(&c, None);
            crate::test_watchdog::phase("initial relay input attachment");
            while !nodes[1].attached(c.id()) || !nodes[2].attached(b.id()) {
                step(&mut nodes).await;
            }
            let old = b.links().by_peer(c.id()).unwrap().link;
            pause(&nodes[2].owner, true);
            crate::test_watchdog::phase("remote pause and input detachment");
            loop {
                let remote_paused = nodes[1]
                    .owner
                    .config
                    .members()
                    .members
                    .iter()
                    .find(|m| m.device == c.id())
                    .and_then(|m| m.facts.as_ref())
                    .and_then(|f| f.pause().ok().flatten())
                    .is_some_and(|p| p.paused() == Ok(true));
                let detached = nodes[1]
                    .owner
                    .input
                    .status()
                    .peers
                    .iter()
                    .any(|p| p.peer == c.id() && p.inbound.is_none() && !p.attached);
                if remote_paused && detached && nodes[2].owner.paused {
                    break;
                }
                step(&mut nodes).await;
            }
            assert!(!nodes[1].owner.paused, "only the remote endpoint is paused");
            // A remote pause is intentional detachment, however long it lasts.
            nodes[1].owner.report_waits(Instant::now() + Duration::from_secs(60));
            testing::flush_commands(&b);
            assert!(
                b.links().links.iter().any(|l| l.link == old),
                "remote pause must retain the authenticated link past the opening deadline"
            );
            assert!(nodes[1].owner.peers[&c.id()].wait_since.is_none());
            pause(&nodes[2].owner, false);
            crate::test_watchdog::phase("resume input on the retained relay link");
            while !nodes[1].attached(c.id()) || !nodes[2].attached(b.id()) {
                step(&mut nodes).await;
            }
            assert_eq!(b.links().by_peer(c.id()).unwrap().link, old);
            crate::test_watchdog::phase("input and network shutdown");
            for node in &nodes {
                node.owner.input.stop();
                node.owner.config.stop().await;
                node.owner.links.stop().await;
            }
        },
    );
}

/// The stream task follows the published pause later than the native owner
/// applies it. A config change it handles after the owner resumed native input,
/// but before that resume is published, still reads the old pause. The stale
/// value must not pause native input again (two extra input-safety epochs).
#[test]
fn a_stale_published_pause_does_not_override_the_owner_resume() {
    crate::test_watchdog::run(
        module_path!(),
        "a_stale_published_pause_does_not_override_the_owner_resume",
        Some(4),
        async {
            let mut nodes = start().await;
            let owner = &mut nodes[0].owner;
            let before = owner.input.input_safety_for_test();
            assert!(!before.paused);
            // Pause: native owner first, then its publication. The stream task
            // has not handled that change yet.
            pause(owner, true);
            let paused = owner.input.input_safety_for_test();
            assert_eq!((paused.epoch, paused.paused), (before.epoch + 1, true));
            // Resume: the native owner applies it; its publication is pending.
            owner.input.apply_owner_pause_for_test(false);
            let resumed = owner.input.input_safety_for_test();
            assert_eq!((resumed.epoch, resumed.paused), (before.epoch + 2, false));
            // Now the stream task handles the pause publication's change.
            assert!(owner.config.applied().paused);
            owner.config_state_changed();
            owner.input.flush_commands_for_test();
            assert_eq!(
                owner.input.input_safety_for_test(),
                resumed,
                "a stale published pause paused native input again"
            );
            assert!(!owner.paused, "a stale published pause closed I0 again");
            // The resume publication then changes nothing in native input either.
            publish_pause(&owner.config, false);
            owner.config_state_changed();
            owner.input.flush_commands_for_test();
            assert_eq!(owner.input.input_safety_for_test(), resumed);
            assert!(!owner.paused);
            assert!(!owner.input.status().paused);
            for node in &nodes {
                node.owner.input.stop();
                node.owner.config.stop().await;
                node.owner.links.stop().await;
            }
        },
    );
}

/// The emergency hot key pauses native input at once; the configuration owner
/// publishes that pause only after the native safety fence and the C0 event.
/// A config change unrelated to pause (here Members, which follows every config
/// publish) handled in between still reads the old published pause. It must
/// not reopen I0 before the emergency is published.
#[test]
fn an_unrelated_config_change_keeps_an_unpublished_emergency_pause() {
    crate::test_watchdog::run(
        module_path!(),
        "an_unrelated_config_change_keeps_an_unpublished_emergency_pause",
        Some(4),
        async {
            let mut nodes = start().await;
            let a = nodes[0].owner.links.id();
            let b = nodes[1].owner.links.clone();
            let c = nodes[2].owner.links.clone();
            testing::hold_input_header(&b, None);
            testing::hold_input_header(&c, None);
            crate::test_watchdog::phase("initial input attachment");
            while !nodes[0].attached(b.id()) || !nodes[1].attached(a) {
                step(&mut nodes).await;
            }
            crate::test_watchdog::phase("native emergency on A");
            // The hot key's native path. This harness has no configuration
            // owner, so nothing publishes the emergency until the test does.
            let (reply, _applied) = std::sync::mpsc::sync_channel(1);
            nodes[0].owner.input.send(InputCommand::TriggerEmergency { reply });
            loop {
                let notice = nodes[0].notices.next().await.unwrap();
                nodes[0].owner.notice(notice);
                if notice == InputNotice::EmergencyPause {
                    break;
                }
            }
            assert!(nodes[0].owner.paused);
            assert!(nodes[0].owner.peers.values().all(|p| p.outbound.is_none()));
            assert!(
                !nodes[0].owner.config.applied().paused,
                "the emergency is not published yet"
            );
            crate::test_watchdog::phase("unrelated config change before the publication");
            let safety = nodes[0].owner.input.input_safety_for_test();
            assert!(safety.paused);
            nodes[0].owner.config_state_changed();
            // Native input stays paused (the stream task never sends a pause
            // back there); this test is about I0 following it.
            nodes[0].owner.input.flush_commands_for_test();
            assert_eq!(nodes[0].owner.input.input_safety_for_test(), safety);
            assert!(nodes[0].owner.input.status().local_paused);
            assert!(
                nodes[0].owner.paused,
                "an unrelated config change resumed I0 before the emergency was published"
            );
            assert!(
                nodes[0].owner.peers.values().all(|p| p.outbound.is_none()),
                "an unrelated config change reopened I0 before the emergency was published"
            );
            // The publication lands; the device stays paused until it resumes.
            publish_pause(&nodes[0].owner.config, true);
            nodes[0].owner.config_state_changed();
            assert!(nodes[0].owner.paused);
            crate::test_watchdog::phase("resume after the emergency");
            pause(&nodes[0].owner, false);
            while !nodes[0].attached(b.id()) || !nodes[1].attached(a) {
                step(&mut nodes).await;
            }
            crate::test_watchdog::phase("input and network shutdown");
            for node in &nodes {
                node.owner.input.stop();
                node.owner.config.stop().await;
                node.owner.links.stop().await;
            }
        },
    );
}
