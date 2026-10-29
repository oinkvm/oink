//! The real configuration owner, input stream manager and QUIC endpoint run together.
//! The native owner runs on the memory backend thread; no desktop input is touched.
use super::*;
use crate::config_module::{ConfigDeps, ConfigModule, group_store};
use crate::streams::input::InputStreams;
use oink_foundation::{realtime::Tier, session::Boot};
use oink_network::{
    identity::Identity,
    link::{LinkConfig, LinkLayer, ModulePort},
};
use oink_protocol::v1::{
    dto,
    schema::{WireDto, WireValue},
};

struct Application;
impl crate::config_module::ConfigApplication for Application {
    fn apply_state(&self, _: &group_store::StoredGroupState) -> Result<(), String> {
        Ok(())
    }
}

struct Running {
    input: Arc<InputThread>,
    links: LinkLayer,
    config: ConfigModule,
    task: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
    _main: crate::threads::MainReceiver,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn start() -> Vec<Running> {
    let identities: Vec<_> = (1..=2).map(|seed| Arc::new(Identity::from_seed([seed; 32]))).collect();
    let states = crate::config_module::runtime_tests::same_group(2);
    let mut pending = Vec::new();
    for (n, (identity, state)) in identities.into_iter().zip(states).enumerate() {
        let directory = tempfile::tempdir().unwrap();
        group_store::save(&directory.path().join(group_store::STATE_FILE), &state).unwrap();
        let (links, port) = LinkLayer::start(LinkConfig {
            profile: crate::node::Node::offer_profile(),
            identity: identity.clone(),
            boot: Boot {
                device_id: identity.id(),
                nonce: [n as u8 + 1; 16],
            },
            bind: "127.0.0.1:0".parse().unwrap(),
            tier: Tier::Elevated,
        })
        .await
        .unwrap();
        let input_port: ModulePort = links.register(&[StreamKind::Input]);
        pending.push((directory, identity, links, port, input_port));
    }
    let addresses: Vec<_> = pending
        .iter()
        .map(|(_, identity, links, _, _)| (identity.id(), links.local_addr()))
        .collect();
    let mut nodes = Vec::new();
    for (directory, identity, links, port, input_port) in pending {
        let mut addresses_store = crate::config_module::local_records::AddressStore::load(directory.path()).unwrap();
        for (peer, address) in &addresses {
            if *peer != identity.id() {
                addresses_store.remember(*peer, *address).unwrap();
            }
        }
        let local_config = Arc::new(std::sync::Mutex::new(
            crate::config::AgentConfig::load_from_with_device_name(directory.path(), "input-test".into()).unwrap(),
        ));
        let config = ConfigModule::start_with_config(
            ConfigDeps {
                data_dir: directory.path().to_path_buf(),
                links: links.clone(),
                port,
                identity: identity.clone(),
                application: Arc::new(Application),
                config: local_config,
                owner_gate: Arc::new(std::sync::Mutex::new(())),
            },
            oink_test_support::network_config(directory.path().to_path_buf(), "input-test", 0),
        )
        .unwrap();
        let backend = Arc::new(oink_test_support::input::FakeInput::new());
        let inject = super::super::super::super::inject::start_with_backend(backend, Box::new(|_| {})).unwrap();
        let snapshots = InputSnapshots {
            mode_switch_in_callback: true,
            capture_reports_key_repeat: false,
            local_input_transform: oink_foundation::input_transform::InputTransform::initial(std::env::consts::OS),
            layout: config.router_layout(),
            settings: Arc::new(Snapshot::default()),
            environment: Arc::new(Snapshot::default()),
        };
        let (main, receive_main) = crate::threads::main_channel();
        let (input, notices) = InputThread::start(links.boot(), snapshots, inject, main).unwrap();
        let input = Arc::new(input);
        let task =
            InputStreams::spawn_with_port(links.clone(), config.clone(), input.clone(), notices, true, input_port);
        nodes.push(Running {
            input,
            links,
            config,
            task,
            _directory: directory,
            _main: receive_main,
        });
    }
    nodes
}
fn pause(config: &ConfigModule, paused: bool) {
    let facts = config.local_facts();
    let old = facts.pause().unwrap().unwrap();
    let pause = dto::LocalPauseState::from_raw(
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
        .publish_local_owner(facts.preferences().unwrap().unwrap().0, pause)
        .unwrap();
}
async fn until(nodes: &mut [Running], predicate: impl Fn(&[Running]) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if predicate(nodes) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "{error}: {:?}",
            nodes.iter().map(|node| node.input.status()).collect::<Vec<_>>()
        )
    });
}
fn peer_status(nodes: &[Running], n: usize) -> PeerInput {
    nodes[n]
        .input
        .status()
        .peers
        .iter()
        .find(|peer| peer.peer == nodes[1 - n].links.id())
        .unwrap()
        .clone()
}
#[test]
fn config_capability_changes_and_reader_resend_drive_the_real_input_manager() {
    crate::test_watchdog::run(
        module_path!(),
        "config_capability_changes_and_reader_resend_drive_the_real_input_manager",
        Some(2),
        config_capability_changes_and_reader_resend_drive_the_real_input_manager_scenario(),
    );
}

async fn config_capability_changes_and_reader_resend_drive_the_real_input_manager_scenario() {
    let mut nodes = start().await;
    until(&mut nodes, |nodes| {
        nodes
            .iter()
            .all(|node| node.input.status().peers.iter().any(|peer| peer.online))
    })
    .await;
    let before_a = peer_status(&nodes, 0);
    let before_b = peer_status(&nodes, 1);
    nodes[0].input.send(InputCommand::Controllable(false));
    until(&mut nodes, |nodes| {
        let a = peer_status(nodes, 0);
        let b = peer_status(nodes, 1);
        a.attached && b.attached && a.outbound != before_a.outbound && !b.online
    })
    .await;
    let changed_a = peer_status(&nodes, 0);
    let changed_b = peer_status(&nodes, 1);
    assert_eq!(changed_a.inbound, before_a.inbound);
    assert_eq!(changed_b.outbound, before_b.outbound);
    assert_eq!(
        changed_a.outbound.unwrap().tag.ordinal,
        before_a.outbound.unwrap().tag.ordinal + 1
    );
    // The native reader's Resend is delivered through ModulePort to the actual manager.
    nodes[1]
        .links
        .close_stream(changed_b.inbound.unwrap(), StreamCode::Resend);
    until(&mut nodes, |nodes| {
        let a = peer_status(nodes, 0);
        let b = peer_status(nodes, 1);
        a.attached && b.attached && a.outbound != changed_a.outbound && b.inbound != changed_b.inbound
    })
    .await;
    let resent = peer_status(&nodes, 0);
    assert_eq!(
        resent.outbound.unwrap().tag.ordinal,
        changed_a.outbound.unwrap().tag.ordinal + 1
    );
    assert_eq!(resent.inbound, before_a.inbound);
    assert_eq!(peer_status(&nodes, 1).outbound, before_b.outbound);
    nodes[0].input.send(InputCommand::Controllable(true));
    until(&mut nodes, |nodes| {
        let a = peer_status(nodes, 0);
        a.attached && a.outbound != resent.outbound && peer_status(nodes, 1).online
    })
    .await;
    assert_eq!(
        peer_status(&nodes, 0).outbound.unwrap().tag.ordinal,
        resent.outbound.unwrap().tag.ordinal + 1
    );
    // A local pause closes both input directions; the peer remains unpaused.
    let before_pause: Vec<_> = (0..2).map(|n| peer_status(&nodes, n)).collect();
    let instance = |input: &InputThread| match &input.opening_state(true)[2] {
        Message::Control(ControlMsg::FocusState { session, .. }) => *session,
        other => panic!("focus opening expected, got {other:?}"),
    };
    let old_instances: Vec<_> = nodes.iter().map(|node| instance(&node.input)).collect();
    pause(&nodes[0].config, true);
    until(&mut nodes, |nodes| {
        nodes[0].input.status().paused
            && peer_status(nodes, 0).outbound.is_none()
            && !nodes[1].input.status().paused
            && !peer_status(nodes, 1).online
    })
    .await;
    assert!(nodes.iter().all(|node| node.links.links().ready_count() == 1));
    assert_ne!(instance(&nodes[0].input), old_instances[0]);
    assert_eq!(instance(&nodes[1].input), old_instances[1]);
    pause(&nodes[0].config, false);
    until(&mut nodes, |nodes| (0..2).all(|n| peer_status(nodes, n).online)).await;
    for (n, before) in before_pause.iter().enumerate() {
        assert!(
            peer_status(&nodes, n).outbound.unwrap().tag.ordinal > before.outbound.unwrap().tag.ordinal,
            "both directions must use a fresh opening after pause/resume"
        );
    }
    for node in &nodes {
        node.task.abort();
    }
    for node in &mut nodes {
        node.input.stop();
        node.config.stop().await;
        node.links.stop().await;
    }
}
