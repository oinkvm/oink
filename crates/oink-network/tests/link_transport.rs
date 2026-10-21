//! Real loopback QUIC endpoints. No native input, installed service, or device fixture.
use oink_foundation::{realtime::Tier, session::Boot};
use oink_network::negotiation as oink_network_profile;
use oink_network::{identity::Identity, link::*};
use oink_protocol::{
    frame::{Priority, StreamCode, StreamKind, StreamTag},
    v1::{
        self, Message, RawValue, dto,
        schema::{WireDto, WireValue},
    },
};
use std::{sync::Arc, time::Duration};
use tokio::time::{sleep, timeout};
#[path = "support/frames.rs"]
mod frames;
#[path = "../../../tests/support/src/ports.rs"]
mod ports;

struct Node {
    network: LinkLayer,
    config: ConfigPort,
    module: ModulePort,
}
impl Node {
    async fn start() -> Self {
        Self::start_tier(Tier::Elevated).await
    }
    async fn start_tier(tier: Tier) -> Self {
        Self::start_identity(Arc::new(Identity::generate()), tier).await
    }
    async fn start_identity(identity: Arc<Identity>, tier: Tier) -> Self {
        let boot = Boot {
            device_id: identity.id(),
            nonce: rand::random(),
        };
        let (network, config) = LinkLayer::start(LinkConfig {
            profile: frames::profile(),
            identity,
            boot,
            bind: (std::net::Ipv4Addr::LOCALHOST, ports::test_port_pair()).into(),
            tier,
        })
        .await
        .unwrap();
        let module = network.register(&[StreamKind::Diagnostics, StreamKind::Input, StreamKind::Clipboard]);
        Self {
            network,
            config,
            module,
        }
    }
    async fn connected(&mut self) -> Box<LinkConnected> {
        match timeout(Duration::from_secs(4), self.config.next())
            .await
            .expect("connection deadline")
            .unwrap()
        {
            ConfigEvent::Connected(link) => link,
            event => panic!("expected Connected, got {event:?}"),
        }
    }
    async fn inbound(&mut self) -> InboundStream {
        loop {
            match timeout(Duration::from_secs(2), self.module.next())
                .await
                .unwrap()
                .unwrap()
            {
                ModuleEvent::Inbound(stream) => return stream,
                ModuleEvent::LinkReady { .. }
                | ModuleEvent::PathChanged { .. }
                | ModuleEvent::StreamClosed {
                    reason: StreamClosed::Local,
                    ..
                } => {}
                event => panic!("expected inbound, got {event:?}"),
            }
        }
    }
    async fn ready(&mut self, link: LinkId) {
        self.network.set_agreement(
            link,
            v1::admission::Agreement::new(frames::profile().capabilities).unwrap(),
        );
        self.network.set_ready(link);
        loop {
            if matches!(timeout(Duration::from_secs(2),self.config.next()).await.unwrap().unwrap(),ConfigEvent::Ready{link:got,..} if got==link)
            {
                break;
            }
        }
    }
}
async fn connect(a: &mut Node, b: &mut Node) -> (Box<LinkConnected>, Box<LinkConnected>) {
    a.network
        .dial(b.network.id(), vec![b.network.local_addr()], DialMode::DirectOnly)
        .unwrap();
    tokio::join!(a.connected(), b.connected())
}
fn tag(n: u32) -> StreamTag {
    StreamTag {
        kind: StreamKind::Diagnostics,
        ordinal: n,
    }
}
fn queues() -> (QueueSpec, QueueSpec) {
    QueueSpec::reliable(StreamKind::Diagnostics)
}
#[tokio::test]
async fn parallel_candidates_skip_a_black_hole_without_spending_its_budget() {
    let mut a = Node::start().await;
    let mut b = Node::start().await;
    // A live UDP socket that never responds avoids OS-specific ICMP refusal.
    let black_hole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let began = std::time::Instant::now();
    a.network
        .dial(
            b.network.id(),
            vec![black_hole.local_addr().unwrap(), b.network.local_addr()],
            DialMode::DirectOnly,
        )
        .unwrap();
    let (ac, bc) = tokio::join!(a.connected(), b.connected());
    let elapsed = began.elapsed();
    eprintln!("black hole + reachable handshake: {elapsed:?}");
    assert!(elapsed < DIAL_BUDGET / 3, "waited for the black hole: {elapsed:?}");
    assert_eq!(ac.remote_address, Some(b.network.local_addr()));
    assert_ne!(ac.direction, bc.direction);
    tokio::join!(a.network.stop(), b.network.stop());
}

#[tokio::test]
async fn parallel_unreachable_candidates_each_get_one_full_budget() {
    let mut a = Node::start().await;
    let holes = [
        tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap(),
        tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap(),
    ];
    let peer = oink_foundation::DeviceId([77; 32]);
    let began = std::time::Instant::now();
    a.network
        .dial(
            peer,
            holes.iter().map(|s| s.local_addr().unwrap()).collect(),
            DialMode::DirectOnly,
        )
        .unwrap();
    let mut first = [0; 1500];
    let mut second = [0; 1500];
    timeout(Duration::from_millis(500), async {
        let (one, two) = tokio::join!(holes[0].recv_from(&mut first), holes[1].recv_from(&mut second));
        one.unwrap();
        two.unwrap();
    })
    .await
    .expect("both candidates must start together");
    assert!(matches!(
        timeout(DIAL_BUDGET + Duration::from_secs(1), a.config.next()).await.unwrap().unwrap(),
        ConfigEvent::DialFailed { peer: got, reason: DialFailure::Unreachable } if got == peer
    ));
    let elapsed = began.elapsed();
    eprintln!("two unreachable candidates: {elapsed:?}");
    assert!(elapsed >= DIAL_BUDGET && elapsed < DIAL_BUDGET + Duration::from_secs(1));
    a.network.stop().await;
}

#[tokio::test]
async fn parallel_candidates_beyond_eight_run_in_full_budget_rounds() {
    let mut a = Node::start().await;
    let mut b = Node::start().await;
    let mut holes = Vec::new();
    for _ in 0..MAX_DIALS {
        holes.push(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    }
    let mut addresses: Vec<_> = holes.iter().map(|s| s.local_addr().unwrap()).collect();
    addresses.push(b.network.local_addr());
    let began = std::time::Instant::now();
    a.network.dial(b.network.id(), addresses, DialMode::DirectOnly).unwrap();
    for hole in &holes {
        let mut packet = [0; 1500];
        timeout(Duration::from_millis(500), hole.recv_from(&mut packet))
            .await
            .expect("first eight candidates start in one round")
            .unwrap();
    }
    assert!(a.network.links().links.is_empty());
    let (ac, bc) = tokio::join!(a.connected(), b.connected());
    let elapsed = began.elapsed();
    eprintln!("ninth candidate after one round: {elapsed:?}");
    assert!(elapsed >= DIAL_BUDGET && elapsed < DIAL_BUDGET + Duration::from_secs(1));
    assert_ne!(ac.direction, bc.direction);
    tokio::join!(a.network.stop(), b.network.stop());
}

#[tokio::test]
async fn parallel_upgrade_candidates_skip_a_black_hole() {
    let mut r = Relayed::start().await;
    r.a.ready(r.ac.link).await;
    r.b.ready(r.bc.link).await;
    let hole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let began = std::time::Instant::now();
    r.a.network.update_addresses(
        r.b.network.id(),
        vec![hole.local_addr().unwrap(), r.b.network.local_addr()],
    );
    timeout(DIAL_BUDGET / 3, async {
        loop {
            if r.a.network.links().by_peer(r.b.network.id()).unwrap().path == LinkPath::Direct
                && r.b.network.links().by_peer(r.a.network.id()).unwrap().path == LinkPath::Direct
            {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("upgrade must not wait for the first candidate's timeout");
    eprintln!("relay upgrade with black hole: {:?}", began.elapsed());
    assert_eq!(r.a.network.links().by_peer(r.b.network.id()).unwrap().link, r.ac.link);
    assert_eq!(r.b.network.links().by_peer(r.a.network.id()).unwrap().link, r.bc.link);
    r.stop().await;
}

#[tokio::test]
async fn simultaneous_dials_and_all_direct_modes_have_complementary_roles() {
    for mode in [DialMode::DirectOnly, DialMode::PreferDirect, DialMode::All] {
        let mut a = Node::start().await;
        let mut b = Node::start().await;
        a.network
            .dial(b.network.id(), vec![b.network.local_addr()], mode)
            .unwrap();
        b.network
            .dial(a.network.id(), vec![a.network.local_addr()], mode)
            .unwrap();
        let (ac, bc) = tokio::join!(a.connected(), b.connected());
        assert_ne!(ac.direction, bc.direction);
        assert_eq!(ac.path, LinkPath::Direct);
        assert_eq!(bc.path, LinkPath::Direct);
        sleep(Duration::from_millis(150)).await;
        assert_eq!(a.network.links().links.len(), 1);
        assert_eq!(b.network.links().links.len(), 1);
        assert!(timeout(Duration::from_millis(40), a.config.next()).await.is_err());
        assert!(timeout(Duration::from_millis(40), b.config.next()).await.is_err());
        tokio::join!(a.network.stop(), b.network.stop());
    }
}

struct Relayed {
    a: Node,
    relay: Node,
    b: Node,
    ac: Box<LinkConnected>,
    bc: Box<LinkConnected>,
    _neighbors: [Box<LinkConnected>; 4],
    _blocked_tcp: Vec<std::net::TcpListener>,
}
impl Relayed {
    async fn start() -> Self {
        Self::start_with_blocked_second_hop(false).await
    }
    async fn start_with_blocked_second_hop(blocked: bool) -> Self {
        let mut a = Node::start().await;
        let mut relay = Node::start().await;
        let mut b = Node::start().await;
        let blocked_tcp = if blocked {
            vec![
                std::net::TcpListener::bind(relay.network.local_addr()).unwrap(),
                std::net::TcpListener::bind(b.network.local_addr()).unwrap(),
            ]
        } else {
            vec![]
        };
        let (ar, ra) = connect(&mut a, &mut relay).await;
        let (br, rb) = connect(&mut b, &mut relay).await;
        a.ready(ar.link).await;
        relay.ready(ra.link).await;
        b.ready(br.link).await;
        relay.ready(rb.link).await;
        // With no addresses PreferDirect must start relay discovery immediately.
        a.network.dial(b.network.id(), vec![], DialMode::PreferDirect).unwrap();
        let (ac, bc) = tokio::join!(a.connected(), b.connected());
        assert!(matches!(ac.path, LinkPath::Relayed { .. }));
        assert_eq!(ac.remote_address, None);
        assert_eq!(bc.remote_address, None);
        Self {
            a,
            relay,
            b,
            ac,
            bc,
            _neighbors: [ar, ra, br, rb],
            _blocked_tcp: blocked_tcp,
        }
    }
    fn direct(&self) {
        self.a
            .network
            .update_addresses(self.b.network.id(), vec![self.b.network.local_addr()]);
    }
    async fn stop(&self) {
        tokio::join!(self.a.network.stop(), self.b.network.stop(), self.relay.network.stop());
    }
}

#[tokio::test]
async fn negotiated_config_and_reliable_frames_preserve_unknown_fields() {
    let mut a = Node::start().await;
    let mut b = Node::start().await;
    let (ac, mut bc) = connect(&mut a, &mut b).await;
    let selected = ac.negotiation.as_ref().unwrap();
    assert_eq!(selected.peer_boot.device_id, b.network.id());
    assert!(selected.agreement.contains("oink.input/1"));
    assert!(!selected.agreement.contains("oink.calibration/1"));
    assert_eq!(selected.negotiated, bc.negotiation.as_ref().unwrap().negotiated);
    let config = frames::control(100 * 1024);
    ac.control_tx.post(config.as_slice()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), bc.control_rx.next())
            .await
            .unwrap()
            .unwrap(),
        config
    );
    a.ready(ac.link).await;
    b.ready(bc.link).await;
    let (tx, _) = a
        .network
        .open_stream(ac.link, Priority::Normal, tag(1), queues().0, None)
        .unwrap();
    let payload = reliable(15000);
    tx.post(payload.as_slice()).unwrap();
    let (mut rx, _) = b
        .inbound()
        .await
        .accept(queues().1, QueueSpec::datagram(StreamKind::Diagnostics));
    assert_eq!(
        timeout(Duration::from_secs(2), rx.next()).await.unwrap().unwrap(),
        payload
    );
    b.network.close_stream(rx.id(), StreamCode::Resend);
    timeout(Duration::from_secs(2), async {
        while tx.post(payload.as_slice()) != Err(PostError::Closed) {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    tokio::join!(a.network.stop(), b.network.stop());
}

#[tokio::test]
async fn invalid_business_channel_isolated_without_losing_control() {
    let mut a = Node::start().await;
    let mut b = Node::start().await;
    let (ac, mut bc) = connect(&mut a, &mut b).await;
    a.ready(ac.link).await;
    b.ready(bc.link).await;
    let (tx, _) = a
        .network
        .open_stream(ac.link, Priority::Normal, tag(1), queues().0, None)
        .unwrap();
    // A valid C0 frame is forbidden on D0 before its large payload is allocated.
    tx.post(frames::control(300)).unwrap();
    let (mut rx, _) = b
        .inbound()
        .await
        .accept(queues().1, QueueSpec::datagram(StreamKind::Diagnostics));
    assert_eq!(timeout(Duration::from_secs(2), rx.next()).await.unwrap(), Err(Closed));
    let valid = frames::control(1);
    ac.control_tx.post(valid.as_slice()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), bc.control_rx.next())
            .await
            .unwrap()
            .unwrap(),
        valid
    );
    assert!(a.network.links().ready(b.network.id()));
    tokio::join!(a.network.stop(), b.network.stop());
}

#[tokio::test]
async fn reliable_backpressure_preserves_frame_order() {
    let mut a = Node::start().await;
    let mut b = Node::start().await;
    let (ac, bc) = connect(&mut a, &mut b).await;
    a.ready(ac.link).await;
    b.ready(bc.link).await;
    let spec = QueueSpec {
        messages: 2,
        bytes: 32768,
    };
    let (tx, _) = a
        .network
        .open_stream(ac.link, Priority::Normal, tag(1), spec, None)
        .unwrap();
    tx.post(reliable(1)).unwrap();
    let (mut rx, _) = b
        .inbound()
        .await
        .accept(spec, QueueSpec::datagram(StreamKind::Diagnostics));
    assert_eq!(rx.next().await.unwrap(), reliable(1));
    let frames: Vec<_> = (1000..1020).map(reliable).collect();
    let writer = tokio::spawn({
        let frames = frames.clone();
        async move {
            for payload in frames {
                loop {
                    match tx.post(payload.as_slice()) {
                        Ok(()) => break,
                        Err(PostError::Full) => sleep(Duration::from_millis(1)).await,
                        error => panic!("{error:?}"),
                    }
                }
            }
            tx
        }
    });
    for expected in frames {
        assert_eq!(
            timeout(Duration::from_secs(2), rx.next()).await.unwrap().unwrap(),
            expected
        );
    }
    let tx = writer.await.unwrap();
    drop(rx);
    timeout(Duration::from_secs(2), async {
        while tx.post(reliable(1)) != Err(PostError::Closed) {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    tokio::join!(a.network.stop(), b.network.stop());
}

fn input_open() -> Vec<u8> {
    frames::encode(&Message::ControlUserActivity(
        dto::ControlUserActivity::from_raw(Default::default()).unwrap(),
    ))
}
fn datagram(seq: u64, padding: usize) -> Vec<u8> {
    frames::encode(&Message::InputRelativeMotion(
        dto::InputRelativeMotion::from_raw(
            [
                (1, 1u64.write()),
                (2, 1u64.write()),
                (3, seq.write()),
                (4, 1f64.write()),
                (5, 2f64.write()),
                (42, RawValue::Bytes(vec![5; padding])),
            ]
            .into(),
        )
        .unwrap(),
    ))
}
async fn datagram_stream(
    a: &mut Node,
    b: &mut Node,
    link: LinkId,
    ordinal: u32,
) -> (SendStream, DatagramSend, RecvStream, DatagramRecv) {
    let tag = StreamTag {
        kind: StreamKind::Input,
        ordinal,
    };
    let (send, recv) = QueueSpec::reliable(StreamKind::Input);
    let (tx, dg) = a
        .network
        .open_stream(
            link,
            Priority::Interactive,
            tag,
            send,
            Some(QueueSpec::datagram(StreamKind::Input)),
        )
        .unwrap();
    tx.post(input_open()).unwrap();
    let incoming = b.inbound().await;
    let (mut rx, rd) = incoming.accept(recv, QueueSpec::datagram(StreamKind::Input));
    assert_eq!(rx.next().await.unwrap(), input_open());
    (tx, dg.unwrap(), rx, rd.unwrap())
}
async fn next_datagram(rx: &DatagramRecv) -> Vec<u8> {
    timeout(Duration::from_secs(2), async {
        loop {
            if let Some(bytes) = rx.take().unwrap() {
                break bytes;
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn datagram_flows_are_bounded_and_timestamped() {
    let mut a = Node::start().await;
    let mut b = Node::start().await;
    let (ac, bc) = connect(&mut a, &mut b).await;
    a.ready(ac.link).await;
    b.ready(bc.link).await;
    let mut flows = Vec::new();
    for ordinal in 1..=3 {
        let flow = datagram_stream(&mut a, &mut b, ac.link, ordinal).await;
        assert_eq!(flow.1.post([0; 1100]), Err(DatagramError::TooLarge));
        let frame = datagram(ordinal as u64, 900);
        flow.1.post(&frame).unwrap();
        assert_eq!(next_datagram(&flow.3).await, frame);
        flows.push(flow);
    }
    let before = std::time::Instant::now();
    flows[0].1.post(datagram(4, 0)).unwrap();
    let mut buffer = [0; 1100];
    let (_, at) = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(item) = flows[0].3.take_timed_into(&mut buffer).unwrap() {
                break item;
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(at >= before && at <= std::time::Instant::now());
    a.network.disconnect(ac.link, Disconnect::Stopping);
    timeout(Duration::from_secs(2), async {
        loop {
            if flows[0].1.post(datagram(5, 0)) == Err(DatagramError::Closed) {
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    tokio::join!(a.network.stop(), b.network.stop());
}

#[tokio::test]
async fn relay_preserves_frames_datagrams_and_stream_handles_across_cutover() {
    let mut r = Relayed::start().await;
    r.a.ready(r.ac.link).await;
    r.b.ready(r.bc.link).await;
    let (tx, dg, mut rx, rd) = datagram_stream(&mut r.a, &mut r.b, r.ac.link, 1).await;
    dg.post(datagram(1, 900)).unwrap();
    assert_eq!(next_datagram(&rd).await, datagram(1, 900));
    r.direct();
    timeout(Duration::from_secs(3), async {
        while r.a.network.links().by_peer(r.b.network.id()).unwrap().path != LinkPath::Direct
            || r.b.network.links().by_peer(r.a.network.id()).unwrap().path != LinkPath::Direct
        {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tx.post(input_open()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), rx.next()).await.unwrap().unwrap(),
        input_open()
    );
    dg.post(datagram(2, 900)).unwrap();
    assert_eq!(next_datagram(&rd).await, datagram(2, 900));
    r.stop().await;
}

fn low_options() -> oink_network::LowOptions {
    oink_network::LowOptions {
        send_max: 2 * 1024 * 1024,
        receive_max: 2 * 1024 * 1024,
        operation_max: 4 * 1024 * 1024,
    }
}
async fn next_low(node: &mut Node) -> Box<InboundLow> {
    timeout(Duration::from_secs(4), async {
        loop {
            if let Some(ModuleEvent::InboundLow(inbound)) = node.module.next().await {
                break inbound;
            }
        }
    })
    .await
    .unwrap()
}
async fn low_waits_for_done(a: &mut Node, b: &mut Node, outgoing: LinkId, incoming: LinkId) {
    b.network.set_half_open(incoming);
    a.ready(outgoing).await;
    let opening = a.network.open_low(outgoing, StreamKind::Diagnostics, low_options());
    tokio::pin!(opening);
    assert!(
        timeout(Duration::from_millis(150), &mut opening).await.is_err(),
        "early Low must wait for Done, without refusal or promotion"
    );
    assert_eq!(
        b.network.links().by_peer(a.network.id()).unwrap().state,
        LinkState::HalfOpen
    );
    b.ready(incoming).await;
    let mut stream = timeout(Duration::from_secs(4), opening).await.unwrap().unwrap();
    let data = frames::low(0, 64);
    stream.send(&data).await.unwrap();
    let mut inbound = next_low(b).await;
    assert_eq!(inbound.first, data);
    stream.finish();
    assert!(inbound.stream.recv().await.unwrap().is_none());
    inbound.stream.finish();
    assert!(stream.recv().await.unwrap().is_none());
}

#[tokio::test]
async fn departing_link_drains_c0_while_business_authority_is_revoked() {
    retired_member_drains_c0(true).await;
}

async fn retired_member_drains_c0(departing: bool) {
    let mut a = Node::start().await;
    let mut b = Node::start().await;
    let (mut ac, mut bc) = connect(&mut a, &mut b).await;
    a.ready(ac.link).await;
    b.ready(bc.link).await;
    if departing {
        a.network.retire_for_departure(ac.link);
    } else {
        a.network.retire(ac.link);
    }
    timeout(Duration::from_secs(2), async {
        while a.network.links().ready(b.network.id()) {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        a.network
            .open_low(ac.link, StreamKind::Diagnostics, low_options())
            .await
            .is_err()
    );
    let state = oink_network::group_state::State::create(&Identity::generate(), "fixture", "test", 1).unwrap();
    let bytes = frames::encode(&Message::ConfigState(state.wire().clone()));
    // Peer traffic already in flight must not close the control stream before
    // our departure can be delivered. ConfigModule discards this inbound value.
    bc.control_tx.post(bytes.clone()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), ac.control_rx.next())
            .await
            .unwrap()
            .unwrap(),
        bytes
    );
    ac.control_tx.post(bytes.clone()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), bc.control_rx.next())
            .await
            .unwrap()
            .unwrap(),
        bytes
    );
    assert!(!a.network.links().ready(b.network.id()));
    tokio::join!(a.network.stop(), b.network.stop());
}

#[tokio::test]
async fn migrating_link_drains_c0_while_business_authority_is_revoked() {
    retired_member_drains_c0(false).await;
}

#[tokio::test]
async fn early_low_waits_for_exact_direct_and_relay_admission_done() {
    let mut a = Node::start().await;
    let mut b = Node::start().await;
    let (ac, bc) = connect(&mut a, &mut b).await;
    low_waits_for_done(&mut a, &mut b, ac.link, bc.link).await;
    tokio::join!(a.network.stop(), b.network.stop());
    let mut relay = Relayed::start().await;
    low_waits_for_done(&mut relay.a, &mut relay.b, relay.ac.link, relay.bc.link).await;
    relay.stop().await;
}

#[tokio::test]
async fn low_v1_chunks_reuse_tls_and_abort_with_link() {
    let mut a = Node::start().await;
    let mut b = Node::start().await;
    let (ac, bc) = connect(&mut a, &mut b).await;
    a.ready(ac.link).await;
    b.ready(bc.link).await;
    for round in 0..2 {
        let mut stream = timeout(
            Duration::from_secs(4),
            a.network.open_low(ac.link, StreamKind::Diagnostics, low_options()),
        )
        .await
        .unwrap()
        .unwrap();
        let first = frames::low(round, 16000);
        stream.send(&first).await.unwrap();
        let mut incoming = next_low(&mut b).await;
        assert_eq!(incoming.first, first);
        for n in 1..8 {
            let data = frames::low(n, 16000);
            stream.send(&data).await.unwrap();
            assert_eq!(incoming.stream.recv().await.unwrap(), Some(data));
        }
        incoming.stream.send(&first).await.unwrap();
        assert_eq!(stream.recv().await.unwrap(), Some(first));
        stream.finish();
        assert!(incoming.stream.recv().await.unwrap().is_none());
        incoming.stream.finish();
        assert!(stream.recv().await.unwrap().is_none());
    }
    let mut stream = a
        .network
        .open_low(ac.link, StreamKind::Diagnostics, low_options())
        .await
        .unwrap();
    stream.send(&frames::low(1, 1)).await.unwrap();
    let mut incoming = next_low(&mut b).await;
    a.network.disconnect(ac.link, Disconnect::Stopping);
    assert!(
        timeout(Duration::from_secs(2), incoming.stream.recv())
            .await
            .unwrap()
            .is_err()
    );
    tokio::join!(a.network.stop(), b.network.stop());
}
#[tokio::test]
async fn relay_low_keeps_end_to_end_capability_and_cancels_on_cutover() {
    let mut r = Relayed::start().await;
    r.a.ready(r.ac.link).await;
    r.b.ready(r.bc.link).await;
    let mut stream = timeout(
        Duration::from_secs(4),
        r.a.network.open_low(r.ac.link, StreamKind::Diagnostics, low_options()),
    )
    .await
    .unwrap()
    .unwrap();
    let data = frames::low(4, 16000);
    stream.send(&data).await.unwrap();
    let mut incoming = next_low(&mut r.b).await;
    assert_eq!(incoming.first, data);
    r.direct();
    assert!(
        timeout(Duration::from_secs(4), incoming.stream.recv())
            .await
            .unwrap()
            .is_err()
    );
    r.stop().await;
}
#[tokio::test]
async fn ipv6_candidates_are_rejected_before_dial() {
    let a = Node::start().await;
    assert_eq!(
        a.network.dial(
            oink_foundation::DeviceId([3; 32]),
            vec!["[::1]:1234".parse().unwrap()],
            DialMode::DirectOnly
        ),
        Err(DialRefused::InvalidAddress)
    );
    a.network.stop().await;
}

fn reliable(size: usize) -> Vec<u8> {
    frames::encode(&Message::DiagnosticsHello(
        dto::DiagnosticsHello::from_raw(
            [
                (1, RawValue::from("test")),
                (2, true.write()),
                (3, false.write()),
                (4, false.write()),
                (42, RawValue::Bytes(vec![7; size])),
            ]
            .into(),
        )
        .unwrap(),
    ))
}

#[tokio::test]
async fn retiring_unadmitted_link_does_not_grant_member_frame_admission() {
    let mut a = Node::start().await;
    let mut b = Node::start().await;
    let (mut ac, bc) = connect(&mut a, &mut b).await;
    a.network.retire(ac.link);
    // An old-root proof lane alone must not admit ordinary member frames.
    a.network.set_old_member_lane(ac.link, true);
    let state = oink_network::group_state::State::create(&Identity::generate(), "fixture", "test", 1).unwrap();
    bc.control_tx
        .post(frames::encode(&Message::ConfigState(state.wire().clone())))
        .unwrap();
    let result = timeout(Duration::from_secs(2), ac.control_rx.next()).await.unwrap();
    assert!(
        result.is_err(),
        "a bare old-root proof lane is not prior member admission"
    );
    tokio::join!(a.network.stop(), b.network.stop());
}

#[tokio::test]
async fn verified_reply_half_open_admits_peer_state_before_local_ready() {
    let mut a = Node::start().await;
    let mut b = Node::start().await;
    let (mut ac, bc) = connect(&mut a, &mut b).await;
    // The initiator verified Reply. It fences HalfOpen before posting Done;
    // the peer can process Done and publish its first state immediately.
    a.network.set_half_open(ac.link);
    b.ready(bc.link).await;
    let state = oink_network::group_state::State::create(&Identity::generate(), "fixture", "test", 1).unwrap();
    let bytes = frames::encode(&Message::ConfigState(state.wire().clone()));
    bc.control_tx.post(bytes.clone()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), ac.control_rx.next())
            .await
            .unwrap()
            .unwrap(),
        bytes
    );
    assert_eq!(
        a.network.links().by_peer(b.network.id()).unwrap().state,
        LinkState::HalfOpen
    );
    assert!(!a.network.links().ready(b.network.id()));
    a.ready(ac.link).await;
    assert!(a.network.links().ready(b.network.id()));
    tokio::join!(a.network.stop(), b.network.stop());
}
