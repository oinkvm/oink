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
                self.owner.pause_changed(self.owner.config.applied().paused);
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
    let identities: Vec<_> = (1..=3).map(|seed| Arc::new(Identity::from_seed([seed; 32]))).collect();
    let states = crate::config_module::runtime_tests::same_group(3);
    let mut pending = Vec::new();
    for (n, (identity, state)) in identities.iter().zip(states).enumerate() {
        let directory = tempfile::tempdir().unwrap();
        group_store::save(&directory.path().join(group_store::STATE_FILE), &state).unwrap();
        let network = oink_test_support::network_config(directory.path().into(), "synthetic", 0);
        let (links, port) = oink_network::link::LinkLayer::start(LinkConfig {
            profile: crate::node::Node::offer_profile(),
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
            testing::hold_input_header(&links, Some(peer));
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

fn pause(config: &ConfigModule, paused: bool) {
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
            pause(&nodes[2].owner.config, true);
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
            pause(&nodes[2].owner.config, false);
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
