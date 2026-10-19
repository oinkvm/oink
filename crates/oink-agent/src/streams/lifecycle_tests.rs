//! Real business owners over two isolated loopback LinkLayers and ConfigModules.
//! No native input/audio/clipboard backends or installed application are started.
use super::{StreamModule, machine::State};
use crate::config_module::{ConfigDeps, ConfigModule, group_store, local_records::AddressStore};
use oink_foundation::{DeviceId, realtime::Tier, session::Boot};
use oink_network::{group_state::State as GroupState, identity::Identity, link::*, negotiation::OfferProfile};
use oink_protocol::{
    frame::{StreamCode, StreamKind, StreamTag},
    v1::{
        self, Message, RawValue as V, dto,
        schema::{WireDto, WireValue},
    },
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct Application {
    fail: AtomicBool,
    failures: AtomicUsize,
}
impl crate::config_module::ConfigApplication for Application {
    fn apply_state(&self, _: &group_store::StoredGroupState) -> Result<(), String> {
        if self.fail.load(Ordering::Acquire) {
            self.failures.fetch_add(1, Ordering::Relaxed);
            Err("synthetic application failure".into())
        } else {
            Ok(())
        }
    }
}
struct Node {
    _directory: tempfile::TempDir,
    links: LinkLayer,
    group: ConfigModule,
    config: Arc<std::sync::Mutex<crate::config::AgentConfig>>,
    owner_gate: Arc<std::sync::Mutex<()>>,
    application: Arc<Application>,
    port: ModulePort,
}
pub(crate) trait Owner: StreamModule {
    const KIND: StreamKind;
    fn create(links: LinkLayer, config: ConfigModule) -> Self;
    fn current(&self, peer: DeviceId) -> Option<StreamId>;
    fn incoming(&self, peer: DeviceId) -> Option<StreamId>;
    fn state(&self, peer: DeviceId) -> Option<State>;
    fn replace(&mut self, peer: DeviceId, message: Message);
    fn tick(&mut self, now: Instant) -> Vec<(DeviceId, Message)>;
    fn post(&mut self, peer: DeviceId, message: &Message, now: Instant) -> Result<(), PostError>;
    fn retry(&self) -> Option<Instant>;
}
pub(crate) struct Fixture<O> {
    nodes: Vec<Node>,
    owner: O,
    peer: DeviceId,
    link: LinkId,
    remote_link: LinkId,
}
const KINDS: &[StreamKind] = &[
    StreamKind::Clipboard,
    StreamKind::Audio,
    StreamKind::Aim,
    StreamKind::Diagnostics,
    StreamKind::Input,
    StreamKind::Wallpaper,
    StreamKind::Dnd,
];
impl<O: Owner> Fixture<O> {
    pub(crate) async fn start() -> Self {
        Self::start_members(2).await
    }
    pub(crate) async fn start_members(count: u8) -> Self {
        Self::start_profile(count, None).await
    }
    async fn start_profile(count: u8, omitted_capability: Option<&str>) -> Self {
        Self::start_topology(count, omitted_capability, false).await
    }
    pub(crate) async fn start_relayed() -> Self {
        Self::start_topology(3, None, true).await
    }
    async fn start_topology(count: u8, omitted_capability: Option<&str>, relayed: bool) -> Self {
        let identities = (41..41 + count)
            .map(|n| Arc::new(Identity::from_seed([n; 32])))
            .collect::<Vec<_>>();
        let mut state = GroupState::create(&identities[0], "synthetic", "test-os", 100).unwrap();
        for identity in identities.iter().skip(1) {
            let roster = state
                .roster()
                .add(&identities[0], identity.id(), "synthetic", "test-os", 100)
                .unwrap();
            state = state.with_roster(&roster).unwrap();
        }
        let mut pending = Vec::new();
        for (i, identity) in identities.into_iter().enumerate() {
            let directory = tempfile::tempdir().unwrap();
            let stored = group_store::StoredGroupState::new(identity.id(), state.wire().clone()).unwrap();
            group_store::save(&directory.path().join(group_store::STATE_FILE), &stored).unwrap();
            // QUIC and Low TCP share a port. UDP :0 can pick a Windows TCP
            // ephemeral/TIME_WAIT port, leaving this fixture without Low.
            let network = oink_test_support::network_config(directory.path().into(), "synthetic", 0);
            let profile = OfferProfile {
                capabilities: v1::registry::CAPABILITIES
                    .iter()
                    .filter(|cap| i == 0 || Some(**cap) != omitted_capability)
                    .map(|s| (*s).to_owned())
                    .collect(),
                formats: oink_clipboard::negotiation::formats(),
                audio: oink_audio::negotiation::audio(),
                ..Default::default()
            };
            let (links, config_port) = LinkLayer::start(LinkConfig {
                profile,
                identity: identity.clone(),
                boot: Boot {
                    device_id: identity.id(),
                    nonce: [41 + i as u8; 16],
                },
                bind: (network.bind_ip, network.port).into(),
                tier: Tier::Elevated,
            })
            .await
            .unwrap();
            let port = links.register(KINDS);
            pending.push((directory, identity, network, links, config_port, port));
        }
        if relayed {
            oink_network::link::testing::set_direct_blocked(&pending[0].3, pending[1].1.id(), true);
            oink_network::link::testing::set_direct_blocked(&pending[1].3, pending[0].1.id(), true);
        }
        let addresses = pending
            .iter()
            .map(|(_, id, _, links, _, _)| (id.id(), vec![links.local_addr()]))
            .collect::<Vec<_>>();
        let mut nodes = Vec::new();
        for (directory, identity, network, links, config_port, port) in pending {
            let mut hints = AddressStore::load(directory.path()).unwrap();
            for (peer, addresses) in &addresses {
                for address in addresses {
                    hints.remember(*peer, *address).unwrap();
                }
            }
            let application = Arc::new(Application::default());
            let config = Arc::new(std::sync::Mutex::new(
                crate::config::AgentConfig::load_from_with_device_name(directory.path(), "synthetic".into()).unwrap(),
            ));
            let owner_gate = Arc::new(std::sync::Mutex::new(()));
            let group = ConfigModule::start_with_config(
                ConfigDeps {
                    config: config.clone(),
                    owner_gate: owner_gate.clone(),
                    data_dir: directory.path().into(),
                    links: links.clone(),
                    port: config_port,
                    identity,
                    application: application.clone(),
                },
                network,
            )
            .unwrap();
            nodes.push(Node {
                _directory: directory,
                links,
                group,
                config,
                owner_gate,
                application,
                port,
            });
        }
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                if nodes.iter().all(|n| {
                    n.group
                        .members()
                        .members
                        .iter()
                        .filter(|m| !m.is_self)
                        .all(|m| m.online && m.facts.is_some() && n.group.peer_agreement(m.device).is_some())
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("both real ConfigModules should admit the signed roster and exchange Facts");
        let peer = nodes[1].links.boot().device_id;
        let link = nodes[0].links.links().by_peer(peer).unwrap().link;
        let remote_link = nodes[1]
            .links
            .links()
            .by_peer(nodes[0].links.boot().device_id)
            .unwrap()
            .link;
        let owner = O::create(nodes[0].links.clone(), nodes[0].group.clone());
        let mut this = Self {
            nodes,
            owner,
            peer,
            link,
            remote_link,
        };
        this.pump().await;
        assert_eq!(
            this.owner.current(peer),
            None,
            "first use, not readiness, opens business streams"
        );
        this
    }
    async fn pump(&mut self) {
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(1), self.nodes[0].port.next()).await {
            match event {
                ModuleEvent::LinkReady { link, peer } => self.owner.link_ready(link, peer),
                ModuleEvent::LinkLost { link, peer, reason } => self.owner.link_lost(link, peer, reason),
                ModuleEvent::Inbound(stream) => self.owner.inbound(stream),
                ModuleEvent::InboundLow(low) => self.owner.inbound_low(*low),
                ModuleEvent::StreamClosed { stream, reason } => self.owner.stream_closed(stream, reason),
                ModuleEvent::PathChanged { .. } => {}
                ModuleEvent::ProtocolViolation { violation, .. } => panic!("fixture protocol violation: {violation:?}"),
            }
        }
    }
    async fn remote_inbound(&mut self, kind: StreamKind) -> InboundStream {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(ModuleEvent::Inbound(stream)) = self.nodes[1].port.next().await {
                    assert_eq!(stream.tag.kind, kind);
                    return stream;
                }
            }
        })
        .await
        .expect("outbound owner stream reaches the peer")
    }
    async fn opened(&mut self, message: Message) -> RecvStream {
        self.owner.replace(self.peer, message.clone());
        self.owner.tick(Instant::now());
        let stream = self.remote_inbound(O::KIND).await;
        let (mut rx, _) = stream.accept(QueueSpec::reliable(O::KIND).1, QueueSpec::datagram(O::KIND));
        assert_eq!(read(&mut rx).await, message, "opening carries current state");
        rx
    }
    async fn inbound(&mut self, ordinal: u32, message: &Message) -> SendStream {
        let (tx, _) = self.nodes[1]
            .links
            .open_stream(
                self.remote_link,
                O::KIND.priority(),
                StreamTag { kind: O::KIND, ordinal },
                QueueSpec::reliable(O::KIND).0,
                None,
            )
            .unwrap();
        tx.post(encode(message).unwrap()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                self.pump().await;
                if self
                    .owner
                    .incoming(self.peer)
                    .is_some_and(|id| id.tag.ordinal == ordinal)
                {
                    break;
                }
            }
        })
        .await
        .expect("owner accepts incoming stream");
        tx
    }
    async fn deliveries(&mut self) -> Vec<(DeviceId, Message)> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                self.pump().await;
                let messages = self.owner.tick(Instant::now());
                if !messages.is_empty() {
                    return messages;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("owner delivers business message")
    }
    async fn closed(&mut self, old: StreamId) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.owner.current(self.peer) == Some(old) {
                self.pump().await;
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("owner observes peer close");
    }
    pub(crate) fn owner_store(
        &self,
        index: usize,
    ) -> (
        Arc<std::sync::Mutex<crate::config::AgentConfig>>,
        Arc<std::sync::Mutex<()>>,
    ) {
        (self.nodes[index].config.clone(), self.nodes[index].owner_gate.clone())
    }
    pub(crate) fn context(&self, index: usize) -> (LinkLayer, ConfigModule) {
        (self.nodes[index].links.clone(), self.nodes[index].group.clone())
    }
    pub(crate) fn port(&mut self, index: usize) -> &mut ModulePort {
        &mut self.nodes[index].port
    }
    pub(crate) async fn stop(self) {
        drop(self.owner);
        for node in &self.nodes {
            node.group.stop().await;
            node.links.stop().await;
        }
    }
}
fn encode(message: &Message) -> v1::Result<Vec<u8>> {
    v1::frame::encode(message, v1::limits::RELIABLE_FRAME)
}
async fn read(rx: &mut RecvStream) -> Message {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(bytes) = rx.take().expect("stream remains open") {
                return v1::frame::OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME)
                    .unwrap()
                    .decode()
                    .unwrap();
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("opening arrives")
}

#[test]
fn diagnostic_hello_saves_only_the_observed_unclean_restart() {
    crate::test_watchdog::run(
        module_path!(),
        "diagnostic_hello_saves_only_the_observed_unclean_restart",
        None,
        diagnostic_hello_saves_only_the_observed_unclean_restart_scenario(),
    );
}

async fn diagnostic_hello_saves_only_the_observed_unclean_restart_scenario() {
    use super::diagnostics::DiagnosticsStreams;
    use crate::diagnostics::{Reason, Recorder};
    let mut f = Fixture::<DiagnosticsStreams>::start().await;
    let directory = tempfile::tempdir().unwrap();
    let recorder = Recorder::start(directory.path().into(), f.nodes[0].links.id(), None, true).unwrap();
    f.owner.set_recorder(recorder.clone());
    let mut saved = recorder.saved_bundles();
    let hello = |instance: &str, previous_unclean: bool| {
        Message::DiagnosticsHello(
            dto::DiagnosticsHello::from_raw(
                [
                    (1, V::Text(instance.repeat(24))),
                    (2, true.write()),
                    (3, false.write()),
                    (4, previous_unclean.write()),
                ]
                .into(),
            )
            .unwrap(),
        )
    };
    let first = hello("a", false);
    let _first = f.inbound(1, &first).await;
    assert_eq!(f.deliveries().await, vec![(f.peer, first)]);
    let clean = hello("b", false);
    let _clean = f.inbound(2, &clean).await;
    assert_eq!(f.deliveries().await, vec![(f.peer, clean)]);
    assert!(!saved.has_changed().unwrap(), "clean restart does not export a bundle");
    let unclean = hello("c", true);
    let current = f.inbound(3, &unclean).await;
    assert_eq!(f.deliveries().await, vec![(f.peer, unclean.clone())]);
    tokio::time::timeout(Duration::from_secs(5), saved.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(recorder.list().unwrap().iter().any(|b| b.reason == Reason::PeerRestart));
    saved.borrow_and_update();
    current.post(encode(&unclean).unwrap()).unwrap();
    assert_eq!(f.deliveries().await, vec![(f.peer, unclean)]);
    assert!(!saved.has_changed().unwrap(), "repeated Hello does not export again");
    recorder.shutdown();
    f.stop().await;
}
pub(super) fn message(kind: StreamKind, sequence: u64) -> Message {
    match kind {
        StreamKind::Clipboard => {
            let clip = dto::ClipId::from_raw(
                [
                    (
                        1,
                        oink_network::wire_identity::device(Identity::from_seed([41; 32]).id()).write(),
                    ),
                    (2, sequence.write()),
                ]
                .into(),
            )
            .unwrap();
            let offer = dto::OfferId::from_raw([(1, clip.write()), (2, 1u64.write())].into()).unwrap();
            let inventory = dto::Inventory::from_raw(
                [
                    (1, offer.write()),
                    (2, V::from("macos")),
                    (3, V::Array(vec![])),
                    (4, V::Array(vec![])),
                ]
                .into(),
            )
            .unwrap();
            let digest = oink_clipboard::wire::digest(
                *blake3::hash(&inventory.encode(v1::limits::INVENTORY).unwrap()).as_bytes(),
            );
            Message::ClipboardOffer(
                dto::ClipboardOffer::from_raw(
                    [
                        (1, offer.write()),
                        (2, 0u64.write()),
                        (3, sequence.write()),
                        (4, dto::Sensitivity::Normal.write()),
                        (5, digest.write()),
                        (6, inventory.write()),
                        (7, sequence.write()),
                    ]
                    .into(),
                )
                .unwrap(),
            )
        }
        StreamKind::Audio => Message::AudioState(
            dto::AudioState::from_raw(
                [
                    (3, 0u32.write()),
                    (4, false.write()),
                    (5, false.write()),
                    (7, false.write()),
                    (8, 100u16.write()),
                    (9, false.write()),
                    (10, sequence.write()),
                    (11, 0u64.write()),
                    (12, V::Array(vec![])),
                    (13, V::Array(vec![])),
                    (14, dto::AudioPhase::Idle.write()),
                ]
                .into(),
            )
            .unwrap(),
        ),
        StreamKind::Aim => {
            let boot = Boot {
                device_id: Identity::from_seed([41; 32]).id(),
                nonce: [41; 16],
            };
            let boot = oink_network::wire_identity::boot(boot);
            let game = dto::GameInstance::from_raw([(1, boot.write()), (2, 1u64.write())].into()).unwrap();
            let event =
                dto::GameEvent::from_raw([(1, game.write()), (2, boot.write()), (3, sequence.write())].into()).unwrap();
            Message::AimClose(dto::AimClose::from_raw([(1, event.write())].into()).unwrap())
        }
        StreamKind::Diagnostics => Message::DiagnosticsHello(
            dto::DiagnosticsHello::from_raw(
                [
                    (1, V::Text(format!("synthetic-{sequence}"))),
                    (2, true.write()),
                    (3, false.write()),
                    (4, false.write()),
                ]
                .into(),
            )
            .unwrap(),
        ),
        _ => unreachable!(),
    }
}

fn input_message(sequence: u32) -> Message {
    Message::InputKey(
        dto::InputKey::from_raw([(1, 4u32.write()), (2, false.write()), (102, sequence.write())].into()).unwrap(),
    )
}

pub(super) async fn failures<O: Owner>() {
    let mut f = Fixture::<O>::start().await;
    let mut rx = f.opened(message(O::KIND, 1)).await;
    // The peer's independently written direction must stay attached throughout.
    let reverse = f.inbound(1, &message(O::KIND, 50)).await;
    let incoming = f.owner.incoming(f.peer).unwrap();
    f.deliveries().await;
    // Keep a different kind (I0) alive as well; no native backend is involved.
    let (sentinel, _) = f.nodes[0]
        .links
        .open_stream(
            f.link,
            StreamKind::Input.priority(),
            StreamTag {
                kind: StreamKind::Input,
                ordinal: 1,
            },
            QueueSpec::reliable(StreamKind::Input).0,
            None,
        )
        .unwrap();
    sentinel.post(encode(&input_message(7)).unwrap()).unwrap();
    let (mut sentinel_rx, _) = f.remote_inbound(StreamKind::Input).await.accept(
        QueueSpec::reliable(StreamKind::Input).1,
        QueueSpec::datagram(StreamKind::Input),
    );
    assert_eq!(read(&mut sentinel_rx).await, input_message(7));
    for (index, reason) in [
        StreamClosed::Peer(StreamCode::Resend),
        StreamClosed::Peer(StreamCode::Invalid),
    ]
    .into_iter()
    .enumerate()
    {
        let old = f.owner.current(f.peer).unwrap();
        let StreamClosed::Peer(code) = reason else {
            unreachable!()
        };
        f.nodes[1].links.close_stream(rx.id(), code);
        f.closed(old).await;
        assert_eq!(f.owner.state(f.peer), Some(State::Backoff));
        assert_eq!(f.owner.incoming(f.peer), Some(incoming));
        let due = f.owner.retry().unwrap();
        if index > 0 {
            f.owner.tick(due - Duration::from_nanos(1));
            assert_eq!(f.owner.current(f.peer), None, "does not reopen before backoff deadline");
        }
        let current = message(O::KIND, 10 + index as u64);
        f.owner.replace(f.peer, current.clone());
        f.owner.tick(due);
        let new = f.owner.current(f.peer).unwrap();
        assert_eq!(new.tag.ordinal, old.tag.ordinal + 1);
        let (mut next, _) = f
            .remote_inbound(O::KIND)
            .await
            .accept(QueueSpec::reliable(O::KIND).1, QueueSpec::datagram(O::KIND));
        assert_eq!(read(&mut next).await, current);
        f.owner.stream_closed(old, StreamClosed::Peer(StreamCode::Unsupported));
        assert_eq!(
            f.owner.current(f.peer),
            Some(new),
            "late old close cannot affect replacement"
        );
        reverse
            .post(encode(&message(O::KIND, 70 + index as u64)).unwrap())
            .unwrap();
        assert!(!f.deliveries().await.is_empty());
        sentinel.post(encode(&input_message(8)).unwrap()).unwrap();
        assert_eq!(
            read(&mut sentinel_rx).await,
            input_message(8),
            "other kind survives every owner fault"
        );
        rx = next;
    }
    let old = f.owner.current(f.peer).unwrap();
    f.nodes[1].links.close_stream(rx.id(), StreamCode::Unsupported);
    f.closed(old).await;
    assert_eq!(f.owner.state(f.peer), Some(State::Closed));
    f.owner.tick(Instant::now() + Duration::from_secs(60));
    assert_eq!(f.owner.current(f.peer), None);
    f.owner.facts_changed(f.peer, false);
    assert_eq!(
        f.owner.current(f.peer),
        None,
        "unchanged periodic facts do not clear unsupported"
    );
    f.owner.facts_changed(f.peer, true);
    assert!(
        f.owner.current(f.peer).is_some(),
        "changed capabilities permit another first opening"
    );
    f.stop().await;
}

pub(super) async fn replacement_and_pause<O: Owner>() {
    let mut f = Fixture::<O>::start().await;
    let _rx = f.opened(message(O::KIND, 1)).await;
    let outgoing = f.owner.current(f.peer).unwrap();
    let old_tx = f.inbound(1, &message(O::KIND, 2)).await;
    let old = f.owner.incoming(f.peer).unwrap();
    f.deliveries().await;
    let new_tx = f.inbound(2, &message(O::KIND, 3)).await;
    let new = f.owner.incoming(f.peer).unwrap();
    assert_eq!(f.deliveries().await.last().unwrap().1, message(O::KIND, 3));
    f.owner.stream_closed(old, StreamClosed::Local);
    assert_eq!(f.owner.incoming(f.peer), Some(new));
    assert_eq!(
        f.owner.current(f.peer),
        Some(outgoing),
        "inbound replacement never replaces outbound"
    );
    let _ = old_tx.post(encode(&message(O::KIND, 99)).unwrap());
    f.owner.pause_changed(true);
    let obeys = !matches!(O::KIND, StreamKind::Audio | StreamKind::Aim | StreamKind::Diagnostics);
    assert_eq!(f.owner.current(f.peer).is_none(), obeys);
    assert_eq!(f.owner.incoming(f.peer), Some(new));
    new_tx.post(encode(&message(O::KIND, 4)).unwrap()).unwrap();
    // Clipboard retains state while paused; audio continues control telemetry.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let paused = f.owner.tick(Instant::now());
    if matches!(O::KIND, StreamKind::Clipboard) {
        assert!(paused.is_empty());
    } else {
        assert_eq!(paused.last().unwrap().1, message(O::KIND, 4));
    }
    f.owner.pause_changed(false);
    if matches!(O::KIND, StreamKind::Clipboard) {
        assert_eq!(
            f.deliveries().await.last().unwrap().1,
            message(O::KIND, 4),
            "resume uses retained state without peer retransmission"
        );
    }
    assert_eq!(f.owner.incoming(f.peer), Some(new));
    f.owner.link_lost(f.link, f.peer, LinkLost::Stopped);
    assert_eq!(f.owner.current(f.peer), None);
    assert_eq!(f.owner.incoming(f.peer), None);
    f.stop().await;
}

pub(super) async fn full<O: Owner>() {
    let mut f = Fixture::<O>::start().await;
    let _rx = f.opened(message(O::KIND, 1)).await;
    let old = f.owner.current(f.peer).unwrap();
    let msg = message(O::KIND, 2);
    // No await: saturate this owner's actual bounded SendStream queue.
    let mut full = false;
    for _ in 0..100_000 {
        if f.owner.post(f.peer, &msg, Instant::now()) == Err(PostError::Full) {
            full = true;
            break;
        }
    }
    assert!(full, "real bounded queue must reject at capacity");
    assert_eq!(f.owner.state(f.peer), Some(State::Backoff));
    let current = message(O::KIND, 3);
    f.owner.replace(f.peer, current.clone());
    f.owner.tick(f.owner.retry().unwrap_or_else(Instant::now));
    let new = f.owner.current(f.peer).unwrap();
    assert_eq!(new.tag.ordinal, old.tag.ordinal + 1);
    let (mut rx, _) = f
        .remote_inbound(O::KIND)
        .await
        .accept(QueueSpec::reliable(O::KIND).1, QueueSpec::datagram(O::KIND));
    assert_eq!(read(&mut rx).await, current);
    f.stop().await;
}

pub(super) async fn failed_application<O: Owner>() {
    let mut f = Fixture::<O>::start().await;
    let _rx = f.opened(message(O::KIND, 1)).await;
    let _reverse = f.inbound(1, &message(O::KIND, 2)).await;
    let old = f.owner.current(f.peer);
    let incoming = f.owner.incoming(f.peer);
    f.nodes[0].application.fail.store(true, Ordering::Release);
    let old_version = f.nodes[0].group.applied().version.clone();
    f.nodes[1]
        .group
        .edit_settings(Box::new(|settings| settings.walls = !settings.walls))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while f.nodes[0].application.failures.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("real config owner attempts and fails incoming group application");
    f.pump().await;
    assert_eq!(f.nodes[0].group.applied().version, old_version);
    assert_eq!(f.owner.current(f.peer), old);
    assert_eq!(f.owner.incoming(f.peer), incoming);
    assert!(f.nodes[0].links.links().ready(f.peer));
    f.owner.post(f.peer, &message(O::KIND, 3), Instant::now()).unwrap();
    f.nodes[0].application.fail.store(false, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(3), async {
        while f.nodes[0].group.applied().version == old_version {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("config retry succeeds without replacing any links");
    f.owner.pause_changed(f.nodes[0].group.applied().paused);
    assert_eq!(f.owner.incoming(f.peer), incoming);
    f.stop().await;
}

macro_rules! owner_lifecycle_tests {
    ($owner:ident, $kind:ident, $replace:expr) => {
        crate::streams::lifecycle_tests::owner_lifecycle_tests!($owner, $kind, $replace, |message| message);
    };
    ($owner:ident, $kind:ident, $replace:expr, $project:expr) => {
        #[cfg(all(test, debug_assertions))]
        mod lifecycle {
            use super::*;
            use crate::streams::lifecycle_tests::{self as cases, Owner};
            impl Owner for $owner {
                const KIND: StreamKind = StreamKind::$kind;
                fn create(links: LinkLayer, config: ConfigModule) -> Self {
                    Self::new(links, config)
                }
                fn current(&self, peer: DeviceId) -> Option<StreamId> {
                    self.peers
                        .get(&peer)
                        .and_then(|p| p.tx.as_ref())
                        .map(SendStream::id)
                }
                fn incoming(&self, peer: DeviceId) -> Option<StreamId> {
                    self.peers
                        .get(&peer)
                        .and_then(|p| p.rx.as_ref())
                        .map(RecvStream::id)
                }
                fn state(&self, peer: DeviceId) -> Option<crate::streams::machine::State> {
                    self.peers.get(&peer).map(|p| p.machine.state())
                }
                fn replace(&mut self, peer: DeviceId, message: Message) {
                    ($replace)(self, peer, message);
                }
                fn tick(&mut self, now: Instant) -> Vec<(DeviceId, Message)> {
                    self.poll(now)
                        .into_iter()
                        .map(|(peer, message)| (peer, ($project)(message)))
                        .collect()
                }
                fn post(&mut self, peer: DeviceId, message: &Message, now: Instant) -> Result<(), PostError> {
                    self.send(peer, message, now)
                }
                fn retry(&self) -> Option<Instant> {
                    self.deadline()
                }
            }
            #[test]
            fn owner_failures_reopen_current_state_without_disturbing_other_streams() {
                crate::test_watchdog::run(
                    module_path!(),
                    "owner_failures_reopen_current_state_without_disturbing_other_streams",
                    None,
                    owner_failures_reopen_current_state_without_disturbing_other_streams_scenario(),
                );
            }

            async fn owner_failures_reopen_current_state_without_disturbing_other_streams_scenario() {
                cases::failures::<$owner>().await;
            }
            #[test]
            fn owner_inbound_replacement_pause_resume_and_link_loss() {
                crate::test_watchdog::run(
                    module_path!(),
                    "owner_inbound_replacement_pause_resume_and_link_loss",
                    None,
                    owner_inbound_replacement_pause_resume_and_link_loss_scenario(),
                );
            }

            async fn owner_inbound_replacement_pause_resume_and_link_loss_scenario() {
                cases::replacement_and_pause::<$owner>().await;
            }
            #[test]
            fn owner_real_queue_full_reopens_with_current_state() {
                crate::test_watchdog::run(
                    module_path!(),
                    "owner_real_queue_full_reopens_with_current_state",
                    None,
                    owner_real_queue_full_reopens_with_current_state_scenario(),
                );
            }

            async fn owner_real_queue_full_reopens_with_current_state_scenario() {
                cases::full::<$owner>().await;
            }
            #[test]
            fn owner_config_application_failure_preserves_both_directions() {
                crate::test_watchdog::run(
                    module_path!(),
                    "owner_config_application_failure_preserves_both_directions",
                    None,
                    owner_config_application_failure_preserves_both_directions_scenario(),
                );
            }

            async fn owner_config_application_failure_preserves_both_directions_scenario() {
                cases::failed_application::<$owner>().await;
            }
        }
    };
}
pub(super) use owner_lifecycle_tests;

async fn next_low(port: &mut ModulePort) -> InboundLow {
    // These scenarios verify operation contents, not a three-second scheduling
    // budget. Wait for the real delivery or link failure; their process watchdog
    // still bounds a missing completion.
    loop {
        match port.next().await.expect("module port remains open") {
            ModuleEvent::InboundLow(low) => return *low,
            ModuleEvent::LinkLost { reason, .. } => panic!("link lost before Low delivery: {reason:?}"),
            _ => {}
        }
    }
}
fn wallpaper(fill: u8) -> Vec<crate::wallpaper::model::ScreenWallpaper> {
    vec![crate::wallpaper::model::ScreenWallpaper {
        display: "synthetic-screen".into(),
        width: 1,
        height: 1,
        jpeg: vec![0xff, 0xd8, fill],
    }]
}
async fn no_low(port: &mut ModulePort, duration: Duration) {
    assert!(
        tokio::time::timeout(duration, async {
            loop {
                if let Some(ModuleEvent::InboundLow(_)) = port.next().await {
                    return;
                }
            }
        })
        .await
        .is_err(),
        "no unscheduled low-priority send"
    );
}
#[test]
fn wallpaper_latest_during_open_in_both_roles_and_receiving_while_paused() {
    crate::test_watchdog::run(
        module_path!(),
        "wallpaper_latest_during_open_in_both_roles_and_receiving_while_paused",
        None,
        wallpaper_latest_during_open_in_both_roles_and_receiving_while_paused_scenario(),
    );
}

async fn wallpaper_latest_during_open_in_both_roles_and_receiving_while_paused_scenario() {
    let mut f = Fixture::<super::diagnostics::DiagnosticsStreams>::start().await;
    let local = f.nodes[0].links.id();
    let mut owners = f
        .nodes
        .iter()
        .map(|n| super::wallpaper::WallpaperStreams::new(n.links.clone(), n.group.clone()))
        .collect::<Vec<_>>();
    owners[0].link_ready(f.link, f.peer);
    owners[1].link_ready(f.remote_link, local);
    no_low(&mut f.nodes[1].port, Duration::from_millis(30)).await;
    for owner in &mut owners {
        owner.set_wallpapers(wallpaper(1));
        // The pending send is superseded before opening and must read the latest state.
        owner.set_wallpapers(wallpaper(2));
    }
    let mut received = [Vec::new(), Vec::new()];
    for (index, operations) in received.iter_mut().enumerate() {
        operations.push(next_low(&mut f.nodes[index].port).await);
    }
    for (index, operations) in received.into_iter().enumerate() {
        owners[index].pause_changed(true);
        for low in operations {
            assert_eq!(
                low.first,
                Message::WallpaperSet(crate::wallpaper::model::encode(2, &wallpaper(2)).unwrap())
            );
            owners[index].inbound_low(low);
        }
        let peer = if index == 0 { f.peer } else { local };
        assert_eq!(owners[index].received(peer), Some(wallpaper(2).as_slice()));
    }
    // Confirmed empty differs from not read: paused change sends nothing, resume sends empty.
    owners[0].set_wallpapers(vec![]);
    no_low(&mut f.nodes[1].port, Duration::from_millis(30)).await;
    owners[0].pause_changed(false);
    let mut low = next_low(&mut f.nodes[1].port).await;
    assert_eq!(
        low.first,
        Message::WallpaperSet(crate::wallpaper::model::encode(3, &[]).unwrap())
    );
    low.stream.finish();
    no_low(&mut f.nodes[1].port, Duration::from_millis(50)).await;
    drop(owners);
    f.stop().await;
}

#[test]
fn dnd_pause_blocks_own_low_but_keeps_serving_peer_and_input_replacement_cancels() {
    crate::test_watchdog::run(
        module_path!(),
        "dnd_pause_blocks_own_low_but_keeps_serving_peer_and_input_replacement_cancels",
        None,
        dnd_pause_blocks_own_low_but_keeps_serving_peer_and_input_replacement_cancels_scenario(),
    );
}

async fn dnd_pause_blocks_own_low_but_keeps_serving_peer_and_input_replacement_cancels_scenario() {
    let mut f = Fixture::<super::diagnostics::DiagnosticsStreams>::start().await;
    let mut owner = super::dnd::DndStreams::new(f.nodes[0].links.clone(), f.nodes[0].group.clone());
    owner.link_ready(f.link, f.peer);
    let id = oink_foundation::content::ClipId { origin: f.peer, seq: 1 };
    let request = Message::DndPreview(
        crate::dnd::wire::preview(
            id,
            2,
            oink_routing::dnd::PreviewBinding {
                key: oink_routing::dnd::DragKey {
                    offer: oink_foundation::content::OfferId {
                        clip: id,
                        generation: 1,
                    },
                    native_generation: 1,
                },
                metadata_revision: 1,
                preview_revision: 1,
            },
            None,
        )
        .unwrap(),
    );
    assert!(!owner.send(f.peer, request.clone()), "DnD requires attached I0");
    let pair = (
        StreamId {
            link: f.link,
            dir: StreamDir::Out,
            tag: StreamTag {
                kind: StreamKind::Input,
                ordinal: 1,
            },
        },
        StreamId {
            link: f.link,
            dir: StreamDir::In,
            tag: StreamTag {
                kind: StreamKind::Input,
                ordinal: 1,
            },
        },
    );
    owner.input_streams(f.peer, Some(pair));
    assert!(owner.send(f.peer, request.clone()));
    // Replacement is synchronous and occurs before the just-spawned operation can run.
    owner.input_streams(f.peer, None);
    no_low(&mut f.nodes[1].port, Duration::from_millis(30)).await;
    owner.input_streams(f.peer, Some(pair));
    owner.pause_changed(true);
    assert!(!owner.send(f.peer, request.clone()));
    let links = f.nodes[1].links.clone();
    let link = f.remote_link;
    let send = tokio::spawn(async move {
        let mut low = links
            .open_low(
                link,
                StreamKind::Dnd,
                oink_network::LowOptions {
                    send_max: 4096,
                    receive_max: 4096,
                    operation_max: 8192,
                },
            )
            .await
            .unwrap();
        low.send(&request).await.unwrap();
        low
    });
    let low = next_low(&mut f.nodes[0].port).await;
    owner.inbound_low(low);
    let mut accepted = owner.take_low();
    assert_eq!(accepted.len(), 1, "pause preserves inbound service");
    accepted[0].stream.finish();
    send.await.unwrap().finish();
    drop(owner);
    f.stop().await;
}

async fn capability_changes<O: Owner>() {
    let capability = match O::KIND {
        StreamKind::Audio => "oink.audio/1",
        StreamKind::Aim => "oink.aim/1",
        _ => unreachable!(),
    };
    // Optional capabilities belong to a connection's negotiated agreement.
    // Fresh links are required to change them; periodic Facts cannot grant one.
    for enabled in [false, true] {
        let mut f = Fixture::<O>::start_profile(2, (!enabled).then_some(capability)).await;
        f.owner.replace(f.peer, message(O::KIND, 1));
        f.owner.tick(Instant::now());
        assert_eq!(f.owner.current(f.peer).is_some(), enabled);
        let current = f.owner.current(f.peer);
        f.owner.facts_changed(f.peer, false);
        f.owner.facts_changed(f.peer, true);
        assert_eq!(
            f.owner.current(f.peer),
            current,
            "Facts never changes negotiated support"
        );
        f.stop().await;
    }
}
#[test]
fn audio_obeys_negotiated_capabilities_across_new_links() {
    crate::test_watchdog::run(
        module_path!(),
        "audio_obeys_negotiated_capabilities_across_new_links",
        None,
        audio_obeys_negotiated_capabilities_across_new_links_scenario(),
    );
}

async fn audio_obeys_negotiated_capabilities_across_new_links_scenario() {
    capability_changes::<super::audio::AudioStreams>().await;
}
#[test]
fn aim_obeys_negotiated_capabilities_across_new_links() {
    crate::test_watchdog::run(
        module_path!(),
        "aim_obeys_negotiated_capabilities_across_new_links",
        None,
        aim_obeys_negotiated_capabilities_across_new_links_scenario(),
    );
}

async fn aim_obeys_negotiated_capabilities_across_new_links_scenario() {
    capability_changes::<super::aim::AimStreams>().await;
}

#[test]
fn clipboard_file_operation_interruption_retains_binding_and_reopens_at_requested_offset() {
    crate::test_watchdog::run(
        module_path!(),
        "clipboard_file_operation_interruption_retains_binding_and_reopens_at_requested_offset",
        None,
        clipboard_file_operation_interruption_retains_binding_and_reopens_at_requested_offset_scenario(),
    );
}

async fn clipboard_file_operation_interruption_retains_binding_and_reopens_at_requested_offset_scenario() {
    use oink_clipboard::{
        model::{ContentMsg, Demand, NodeId},
        sync::files::BulkLink,
    };
    let mut f = Fixture::<super::clipboard::ClipboardStreams>::start().await;
    use oink_clipboard::model::{ClipId, ContentSize, FileNode, Inventory, NodeKind, OfferId};
    use oink_foundation::clipboard::{ClipPlatform, Sensitivity};
    const OFFSET: u64 = 1024 * 1024;
    let inventory = Inventory {
        id: OfferId {
            clip: ClipId { origin: f.peer, seq: 1 },
            generation: 1,
        },
        platform: if cfg!(target_os = "macos") {
            ClipPlatform::Mac
        } else {
            ClipPlatform::Windows
        },
        items: vec![],
        roots: vec![FileNode {
            id: NodeId(11),
            parent: None,
            name: "native-file".into(),
            kind: NodeKind::File,
            size: ContentSize::Known(2 * OFFSET),
            version: 17,
            modified_ms: None,
        }],
    };
    let offer = oink_clipboard::sync::Ordering::new(f.peer, 0)
        .capture(
            oink_clipboard::wire::inventory(&inventory).unwrap(),
            1,
            Sensitivity::Normal,
            0,
        )
        .unwrap()
        .unwrap();
    let offer_id = inventory.id;
    let id = offer_id.clip;
    let mut writer = f.inbound(1, &Message::ClipboardOffer(offer.clone())).await;
    f.deliveries().await;
    let bound = f.owner.file_links().bind(f.peer, id);
    let request = ContentMsg::ReadFile {
        offer: offer_id,
        node: NodeId(11),
        version: 17,
        offset: OFFSET,
        check: Some([7; 32]),
        demand: Demand::Clipboard,
    };
    for reason in 0..3 {
        let mut channel = bound
            .open_with_options(
                f.peer,
                oink_foundation::transfer::LowOptions {
                    send_max: 4096,
                    receive_max: 4096,
                    operation_max: 8192,
                },
            )
            .await
            .unwrap();
        channel.send(&request).await.unwrap();
        let mut remote = next_low(&mut f.nodes[1].port).await;
        assert_eq!(
            oink_clipboard::wire::local_content(remote.first.clone()).unwrap(),
            request,
            "adapter preserves the file layer's verified-offset request"
        );
        let valid = channel.effect_validity();
        assert!(valid());
        match reason {
            0 => f.owner.pause_changed(true),
            1 => {
                let old = f.owner.incoming(f.peer).unwrap();
                f.nodes[1].links.close_stream(writer.id(), StreamCode::None);
                f.owner.stream_closed(old, StreamClosed::Peer(StreamCode::None));
            }
            _ => remote.stream.cancel(),
        }
        let result = tokio::time::timeout(Duration::from_secs(2), channel.recv())
            .await
            .unwrap();
        assert!(
            result.is_err(),
            "in-flight read is cancelled by its exact operation/stream/pause"
        );
        assert!(!valid());
        assert!(
            !bound.is_cancelled(),
            "a transport interruption does not discard the accepted download"
        );
        drop(remote);
        drop(channel);
        match reason {
            0 => f.owner.pause_changed(false),
            1 => {
                writer = f.inbound(2, &Message::ClipboardOffer(offer.clone())).await;
                f.deliveries().await;
            }
            _ => {}
        }
    }
    let mut resumed = bound
        .open_with_options(
            f.peer,
            oink_foundation::transfer::LowOptions {
                send_max: 4096,
                receive_max: 4096,
                operation_max: 8192,
            },
        )
        .await
        .unwrap();
    resumed.send(&request).await.unwrap();
    let mut remote = next_low(&mut f.nodes[1].port).await;
    let reply = ContentMsg::FileEnd {
        offer: offer_id,
        node: NodeId(11),
        version: 17,
        size: OFFSET,
    };
    remote
        .stream
        .send(&oink_clipboard::wire::content(&reply).unwrap())
        .await
        .unwrap();
    remote.stream.finish();
    assert_eq!(resumed.recv().await.unwrap(), Some(reply));
    assert_eq!(resumed.recv().await.unwrap(), None);
    let valid = resumed.effect_validity();
    resumed.finish();
    assert!(valid(), "successful Low completion permits the final file-layer commit");
    f.stop().await;
}

#[test]
fn clipboard_current_six_hundred_items_during_open_in_both_roles() {
    crate::test_watchdog::run(
        module_path!(),
        "clipboard_current_six_hundred_items_during_open_in_both_roles",
        Some(4),
        clipboard_current_six_hundred_items_during_open_in_both_roles_scenario(),
    );
}

fn large_clipboard_offer(origin: DeviceId, sequence: u64) -> dto::ClipboardOffer {
    use oink_clipboard::model::*;
    use oink_foundation::clipboard::{ClipPlatform, Sensitivity};
    let inventory = Inventory {
        id: OfferId {
            clip: ClipId { origin, seq: sequence },
            generation: sequence,
        },
        platform: if cfg!(target_os = "macos") {
            ClipPlatform::Mac
        } else {
            ClipPlatform::Windows
        },
        roots: vec![],
        items: (0..600)
            .map(|n| ContentItem {
                virtual_files: false,
                id: ItemId(n + 1),
                file: None,
                representations: vec![Representation {
                    id: RepresentationId(n + 1),
                    format: if cfg!(target_os = "macos") {
                        NativeFormat::Mac(format!("org.oink.test.item-{n}"))
                    } else {
                        NativeFormat::WindowsRegistered(format!("Oink Test Item {n}"))
                    },
                    medium: Medium::Bytes,
                    aspect: 1,
                    index: -1,
                    standard: None,
                    data: RepresentationData::Reader {
                        size: ContentSize::Known(u64::from(n)),
                        hash: Some([sequence as u8; 32]),
                    },
                }],
            })
            .collect(),
    };
    oink_clipboard::sync::Ordering::new(origin, sequence - 1)
        .capture(
            oink_clipboard::wire::inventory(&inventory).unwrap(),
            sequence,
            Sensitivity::Normal,
            0,
        )
        .unwrap()
        .unwrap()
}

async fn clipboard_current_six_hundred_items_during_open_in_both_roles_scenario() {
    use oink_clipboard::model::*;
    let mut f = Fixture::<super::clipboard::ClipboardStreams>::start().await;
    let local = f.nodes[0].links.id();
    let mut remote = super::clipboard::ClipboardStreams::new(f.nodes[1].links.clone(), f.nodes[1].group.clone());
    remote.link_ready(f.remote_link, local);
    for (index, owner) in [&mut f.owner, &mut remote].into_iter().enumerate() {
        let origin = if index == 0 { local } else { f.peer };
        for sequence in [1, 2] {
            let offer = large_clipboard_offer(origin, sequence);
            owner.set_offer(Some(offer));
            assert_eq!(
                owner.attachment_count(),
                sequence,
                "each update splits once, including first open"
            );
        }
    }
    let mut complete = [false; 2];
    let mut held_inbound = None;
    let mut tail_preceded_inbound = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while complete != [true, true] {
            f.pump().await;
            while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(1), f.nodes[1].port.next()).await {
                match event {
                    ModuleEvent::Inbound(stream) if !tail_preceded_inbound => held_inbound = Some(stream),
                    ModuleEvent::Inbound(stream) => remote.inbound(stream),
                    ModuleEvent::InboundLow(low) => {
                        remote.inbound_low(*low);
                        if remote.inbound_id(local).is_none() {
                            assert!(remote.deadline().is_some(), "unadmitted tails have an expiry");
                        }
                        if let Some(stream) = held_inbound.take() {
                            remote.inbound(stream);
                        }
                        tail_preceded_inbound = true;
                    },
                    ModuleEvent::StreamClosed { stream, reason } => remote.stream_closed(stream, reason),
                    _ => {}
                }
            }
            for (index, owner) in [&mut f.owner, &mut remote].into_iter().enumerate() {
                for (_, message) in owner.poll(Instant::now()) {
                    if let Message::ClipboardOffer(offer) = message {
                        let sequence = oink_clipboard::wire::local_offer_id(offer.id().unwrap()).unwrap().clip.seq;
                        assert!((1..=2).contains(&sequence));
                        let inventory = oink_clipboard::wire::local_inventory(&offer.inventory().unwrap()).unwrap();
                        assert_eq!(inventory.items.len(), 600);
                        assert!(inventory.items.iter().flat_map(|item| &item.representations).all(
                            |rep| matches!(rep.data,RepresentationData::Reader{hash:Some(hash),..} if hash==[sequence as u8;32])
                        ));
                        complete[index] |= sequence == 2;
                    }
                }
            }
        }
    })
    .await
    .unwrap_or_else(|error| panic!("both real B0 owners assemble latest: {error}; completed={complete:?}, early={tail_preceded_inbound}"));
    assert!(tail_preceded_inbound, "exercise TCP tail before QUIC admission");
    drop(remote);
    f.stop().await;
}

#[test]
fn clipboard_unadmitted_tail_expires_and_retires_with_its_link() {
    crate::test_watchdog::run(
        module_path!(),
        "clipboard_unadmitted_tail_expires_and_retires_with_its_link",
        Some(4),
        async {
            use oink_clipboard::model::*;
            use oink_foundation::clipboard::{ClipPlatform, Sensitivity};
            for retire in [false, true] {
                let mut f = Fixture::<super::clipboard::ClipboardStreams>::start().await;
                let source = f.nodes[0].links.id();
                let mut remote =
                    super::clipboard::ClipboardStreams::new(f.nodes[1].links.clone(), f.nodes[1].group.clone());
                remote.link_ready(f.remote_link, source);
                let inventory = Inventory {
                    id: OfferId {
                        clip: ClipId { origin: source, seq: 1 },
                        generation: 1,
                    },
                    platform: ClipPlatform::Mac,
                    roots: vec![],
                    items: (1..=6)
                        .map(|id| ContentItem {
                            virtual_files: false,
                            id: ItemId(id),
                            file: None,
                            representations: vec![Representation {
                                id: RepresentationId(id),
                                format: NativeFormat::Mac(format!("example.{}", "x".repeat(3900))),
                                medium: Medium::Bytes,
                                aspect: 1,
                                index: -1,
                                standard: None,
                                data: RepresentationData::Reader {
                                    size: ContentSize::Unknown,
                                    hash: None,
                                },
                            }],
                        })
                        .collect(),
                };
                let offer = oink_clipboard::sync::Ordering::new(source, 0)
                    .capture(
                        oink_clipboard::wire::inventory(&inventory).unwrap(),
                        1,
                        Sensitivity::Normal,
                        0,
                    )
                    .unwrap()
                    .unwrap();
                f.owner.set_offer(Some(offer));
                let mut held = None;
                let valid = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        match f.nodes[1].port.next().await.unwrap() {
                            ModuleEvent::Inbound(stream) => held = Some(stream),
                            ModuleEvent::InboundLow(low) => {
                                let valid = low.stream.effect_validity();
                                remote.inbound_low(*low);
                                break valid;
                            }
                            _ => {}
                        }
                    }
                })
                .await
                .unwrap();
                assert!(remote.inbound_id(source).is_none());
                let expiry = remote.deadline().expect("bounded pending descriptor tail");
                assert!(valid());
                if retire {
                    remote.link_lost(f.remote_link, source, LinkLost::Stopped);
                } else {
                    assert!(remote.poll(expiry).is_empty(), "a tail alone never publishes an offer");
                }
                assert!(!valid(), "expiry and link retirement cancel the exact Low capability");
                assert!(remote.deadline().is_none());
                drop(held);
                drop(remote);
                f.stop().await;
            }
        },
    );
}

#[test]
fn game_observes_new_local_displays_from_config_owner() {
    crate::test_watchdog::run(
        module_path!(),
        "game_observes_new_local_displays_from_config_owner",
        None,
        game_observes_new_local_displays_from_config_owner_scenario(),
    );
}

async fn game_observes_new_local_displays_from_config_owner_scenario() {
    let f = Fixture::<super::aim::AimStreams>::start().await;
    let (links, group) = f.context(0);
    let game = super::aim::AimStreams::new(links, group.clone());
    for key in ["synthetic-first", "synthetic-replacement"] {
        group.local_displays_changed(vec![oink_display::DisplayInfo {
            key: key.into(),
            name: key.into(),
            bounds: oink_foundation::Rect::new(0.0, 0.0, 100.0, 100.0),
            pixel_width: 100,
            pixel_height: 100,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        }]);
        tokio::time::timeout(Duration::from_secs(2), async {
            while f
                .owner
                .local_displays()
                .displays
                .first()
                .is_none_or(|display| display.key != key)
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(game.local_displays().displays, f.owner.local_displays().displays);
        assert_eq!(game.local_displays().displays[0].key, key);
    }
    drop(game);
    f.stop().await;
}

#[test]
fn clipboard_consumer_blocked_beyond_heartbeat_does_not_stall_link_or_input() {
    crate::test_watchdog::run(
        module_path!(),
        "clipboard_consumer_blocked_beyond_heartbeat_does_not_stall_link_or_input",
        None,
        clipboard_consumer_blocked_beyond_heartbeat_does_not_stall_link_or_input_scenario(),
    );
}

async fn clipboard_consumer_blocked_beyond_heartbeat_does_not_stall_link_or_input_scenario() {
    let mut f = Fixture::<super::clipboard::ClipboardStreams>::start().await;
    let _outbound = f.opened(message(StreamKind::Clipboard, 1)).await;
    let remote_clipboard = f.inbound(1, &message(StreamKind::Clipboard, 2)).await;
    f.deliveries().await;
    let incoming = f.owner.incoming(f.peer);
    let (input, _) = f.nodes[0]
        .links
        .open_stream(
            f.link,
            StreamKind::Input.priority(),
            StreamTag {
                kind: StreamKind::Input,
                ordinal: 1,
            },
            QueueSpec::reliable(StreamKind::Input).0,
            None,
        )
        .unwrap();
    input.post(encode(&input_message(0)).unwrap()).unwrap();
    let (mut input_rx, _) = f.remote_inbound(StreamKind::Input).await.accept(
        QueueSpec::reliable(StreamKind::Input).1,
        QueueSpec::datagram(StreamKind::Input),
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while input_rx.take().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();

    // Park the real B0 consumer, including its ModulePort processing, on an
    // ordinary thread. Only the network thread and the I0 consumer keep running.
    // This exercises queue/thread isolation, not native clipboard or key effects.
    let (resume, blocked) = std::sync::mpsc::channel();
    let (started, entered) = tokio::sync::oneshot::channel();
    let owner = f.owner;
    let worker = tokio::task::spawn_blocking(move || {
        let _ = started.send(());
        blocked.recv_timeout(Duration::from_secs(15)).unwrap();
        owner
    });
    entered.await.unwrap();
    let pending = message(StreamKind::Clipboard, 3);
    remote_clipboard.post(encode(&pending).unwrap()).unwrap();
    let stalled_at = Instant::now();
    let mut count = 0u32;
    while stalled_at.elapsed() < HEARTBEAT_TIMEOUT * 2 {
        count += 1;
        let bytes = encode(&input_message(count)).unwrap();
        input.post(bytes.as_slice()).unwrap();
        tokio::time::timeout(HEARTBEAT_TIMEOUT / 2, async {
            loop {
                if let Some(received) = input_rx.take().unwrap() {
                    assert_eq!(received.as_slice(), bytes.as_slice());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("I0 keeps delivering while the B0 consumer is blocked");
        let snapshot = f.nodes[0].links.links();
        let link = snapshot.by_peer(f.peer).expect("heartbeat keeps the link alive");
        assert_eq!(link.link, f.link, "no reconnect masks a heartbeat failure");
        assert_eq!(link.state, LinkState::Ready);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(count >= 10, "input made repeated progress during the blocked interval");
    resume.send(()).unwrap();
    f.owner = worker.await.unwrap();
    assert_eq!(f.owner.incoming(f.peer), incoming);
    assert_eq!(f.deliveries().await, [(f.peer, pending)]);
    f.stop().await;
}

#[test]
fn clipboard_new_tail_before_replacement_stream_keeps_its_matching_head() {
    crate::test_watchdog::run(
        module_path!(),
        "clipboard_new_tail_before_replacement_stream_keeps_its_matching_head",
        Some(4),
        async {
            let mut f = Fixture::<super::clipboard::ClipboardStreams>::start().await;
            let old = f.inbound(1, &message(StreamKind::Clipboard, 1)).await;
            assert_eq!(f.deliveries().await.len(), 1);
            let offer = Message::ClipboardOffer(large_clipboard_offer(f.peer, 2));
            let (head, tail) = crate::attachment::split(7, offer.clone()).unwrap();
            let tail = tail.unwrap();
            let bytes = v1::frame::encode(&tail, v1::limits::LOW_FRAME).unwrap().len();
            let max = oink_foundation::transfer::LowOptions::message_bytes(bytes);
            let mut transfer = f.nodes[1]
                .links
                .open_low(
                    f.remote_link,
                    StreamKind::Clipboard,
                    oink_foundation::transfer::LowOptions {
                        send_max: max,
                        receive_max: 0,
                        operation_max: max,
                    },
                )
                .await
                .unwrap();
            transfer.send(&tail).await.unwrap();
            transfer.finish();
            // TCP has delivered the NEW tail while the OLD QUIC stream still
            // exists. Its matching head will arrive on a newer ordinal.
            let early = next_low(&mut f.nodes[0].port).await;
            f.owner.inbound_low(early);
            assert!(f.owner.poll(Instant::now()).is_empty());
            let next = f.inbound(2, &head).await;
            let received = f.deliveries().await;
            assert_eq!(received, vec![(f.peer, offer)]);
            drop((old, next, transfer));
            f.stop().await;
        },
    );
}
