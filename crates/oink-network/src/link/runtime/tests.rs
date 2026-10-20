use super::*;
use crate::negotiation as oink_network_profile;
use crate::{LowOptions, identity::Identity};
#[path = "../../../tests/support/frames.rs"]
mod frames;
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn manual_query_closed_udp_port_fails_without_waiting_for_quic_idle() {
    let (a, _, _) = node().await;
    let vacant = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = vacant.local_addr().unwrap();
    drop(vacant);
    let result = timeout(Duration::from_secs(1), a.probe_address(address)).await;
    a.stop().await;
    assert_eq!(
        result.expect("ICMP port unreachable must fail immediately"),
        Err(AddressProbeError::Refused)
    );
}

#[tokio::test]
async fn manual_query_silent_udp_peer_has_five_second_budget() {
    let (a, _, _) = node().await;
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let started = Instant::now();
    let result = timeout(
        Duration::from_millis(5500),
        a.probe_address(silent.local_addr().unwrap()),
    )
    .await;
    a.stop().await;
    assert_eq!(
        result.expect("silent target must finish in five seconds"),
        Err(AddressProbeError::Timeout)
    );
    assert!(started.elapsed() >= Duration::from_millis(4500));
}

async fn node() -> (LinkLayer, ConfigPort, ModulePort) {
    let identity = Arc::new(Identity::generate());
    let boot = Boot {
        device_id: identity.id(),
        nonce: rand::random(),
    };
    let (network, config) = LinkLayer::start(LinkConfig {
        profile: frames::profile(),
        identity,
        boot,
        bind: "127.0.0.1:0".parse().unwrap(),
        tier: Tier::Elevated,
    })
    .await
    .unwrap();
    let module = network.register(&[StreamKind::Diagnostics]);
    (network, config, module)
}
async fn connected(config: &mut ConfigPort) -> Box<LinkConnected> {
    match timeout(Duration::from_secs(4), config.next()).await.unwrap().unwrap() {
        ConfigEvent::Connected(link) => link,
        event => panic!("expected connection, got {event:?}"),
    }
}
async fn incoming(module: &mut ModulePort) -> Box<InboundLow> {
    timeout(Duration::from_secs(4), async {
        loop {
            if let Some(ModuleEvent::InboundLow(incoming)) = module.next().await {
                break incoming;
            }
        }
    })
    .await
    .unwrap()
}

async fn connection_counts(network: &LinkLayer) -> (usize, usize, usize) {
    let (send, receive) = oneshot::channel();
    network.0.send(Command::InspectConnections(send));
    receive.await.unwrap()
}

#[path = "../../../../oink-agent/tests/common/watchdog.rs"]
mod watchdog;

#[test]
fn crossing_timing_covers_real_quic_write_receive_and_first_motion_only() {
    watchdog::run(
        module_path!(),
        "crossing_timing_covers_real_quic_write_receive_and_first_motion_only",
        None,
        crossing_timing_scenario(),
    );
}
async fn crossing_timing_scenario() {
    use oink_foundation::handoff_timing::{self, Event, Observer, Stage};
    struct Measurements(crossbeam_queue::ArrayQueue<Event>);
    impl Observer for Measurements {
        fn enabled(&self) -> bool {
            true
        }
        fn record(&self, event: Event) {
            self.0.push(event).unwrap();
        }
    }
    let measurements = Arc::new(Measurements(crossbeam_queue::ArrayQueue::new(128)));
    let observer: Arc<dyn Observer> = measurements.clone();
    handoff_timing::set_observer(Arc::downgrade(&observer));
    let (a, mut ac, _) = node().await;
    let (b, mut bc, _) = node().await;
    let mut port = b.register(&[StreamKind::Input]);
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly).unwrap();
    let (al, bl) = tokio::join!(connected(&mut ac), connected(&mut bc));
    a.set_ready(al.link);
    b.set_ready(bl.link);
    timeout(Duration::from_secs(4), async {
        while !a.links().ready(b.id()) || !b.links().ready(a.id()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fn hex(text: &str) -> Vec<u8> {
        let text = text.trim();
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }
    let focus = hex(include_str!("../../../../oink-protocol/tests/fixtures/v1/0101.hex"));
    let motion = hex(include_str!("../../../../oink-protocol/tests/fixtures/v1/0201.hex"));
    let (send, _) = a
        .open_stream(
            al.link,
            Priority::Interactive,
            oink_protocol::frame::StreamTag {
                kind: StreamKind::Input,
                ordinal: 1,
            },
            QueueSpec::reliable(StreamKind::Input).0,
            None,
        )
        .unwrap();
    send.post(focus.clone()).unwrap();
    let inbound = timeout(Duration::from_secs(4), async {
        loop {
            if let Some(ModuleEvent::Inbound(inbound)) = port.next().await {
                break inbound;
            }
        }
    })
    .await
    .unwrap();
    let (mut receive, _) = inbound.accept(
        QueueSpec::reliable(StreamKind::Input).1,
        QueueSpec::datagram(StreamKind::Input),
    );
    for expected in [focus, motion.clone(), motion] {
        if expected[0] == 1 && expected[1] == 2 {
            send.post(expected.clone()).unwrap();
        }
        let (bytes, _) = timeout(Duration::from_secs(4), async {
            loop {
                if let Some(message) = receive.take_timed().unwrap() {
                    break message;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(bytes, expected);
    }
    let mut events = Vec::new();
    while let Some(event) = measurements.0.pop() {
        events.push(event);
    }
    for sequence in [0, 1] {
        let stages: Vec<_> = events
            .iter()
            .filter(|e| e.token.epoch == 1 && e.token.sequence == sequence)
            .map(|e| e.stage)
            .collect();
        let expected = [
            Stage::Enqueue,
            Stage::NetworkDequeue,
            Stage::QuicWriteStarted,
            Stage::QuicWriteAccepted,
            Stage::QuicReceive,
            Stage::InputDispatch,
        ];
        let observed: Vec<_> = stages.into_iter().filter(|s| *s != Stage::QuicWriteBlocked).collect();
        assert_eq!(observed, expected);
    }
    assert_eq!(events.iter().filter(|e| e.stage == Stage::QuicWriteAccepted).count(), 2);
    tokio::join!(a.stop(), b.stop());
}

#[test]
fn parallel_reachable_candidates_install_once_and_close_the_loser() {
    watchdog::run(
        module_path!(),
        "parallel_reachable_candidates_install_once_and_close_the_loser",
        None,
        parallel_reachable_candidates_scenario(),
    );
}
async fn parallel_reachable_candidates_scenario() {
    let (a, mut ac, _) = node().await;
    let (b, mut bc, _) = node().await;
    let mut addresses = Vec::new();
    let mut proxies = Vec::new();
    let mut first_packets = Vec::new();
    for _ in 0..2 {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        addresses.push(socket.local_addr().unwrap());
        let target = b.local_addr();
        let (first, observed) = oneshot::channel();
        first_packets.push(observed);
        proxies.push(tokio::spawn(async move {
            let mut first = Some(first);
            let mut client = None;
            let mut packet = [0; 65536];
            loop {
                let (size, source) = socket.recv_from(&mut packet).await.unwrap();
                if source == target {
                    socket.send_to(&packet[..size], client.unwrap()).await.unwrap();
                } else {
                    client = Some(source);
                    if let Some(first) = first.take() {
                        let _ = first.send(());
                    }
                    socket.send_to(&packet[..size], target).await.unwrap();
                }
            }
        }));
    }
    a.dial(b.id(), addresses, DialMode::DirectOnly).unwrap();
    watchdog::phase("both reachable candidates attempted");
    for first in first_packets {
        first.await.unwrap();
    }
    let (al, bl) = tokio::join!(connected(&mut ac), connected(&mut bc));
    assert_ne!(al.direction, bl.direction);
    // This is eventual loser cleanup, not a one-second latency contract.
    // The process watchdog bounds a lost close with the phase in its failure.
    watchdog::phase("losing QUIC connection closes on both endpoints");
    loop {
        if connection_counts(&a).await == (1, 0, 1) && connection_counts(&b).await == (1, 0, 1) {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(a.links().links.len(), 1);
    assert_eq!(b.links().links.len(), 1);
    assert!(timeout(Duration::from_millis(100), ac.next()).await.is_err());
    assert!(timeout(Duration::from_millis(100), bc.next()).await.is_err());
    tokio::join!(a.stop(), b.stop());
    for proxy in proxies {
        proxy.abort();
    }
}

#[tokio::test]
async fn candidate_slots_are_shared_between_peer_dials() {
    let (a, mut config, _) = node().await;
    let mut holes = Vec::new();
    for _ in 0..12 {
        holes.push(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    }
    for (peer, sockets) in [(DeviceId([70; 32]), &holes[..4]), (DeviceId([71; 32]), &holes[4..])] {
        a.dial(
            peer,
            sockets.iter().map(|s| s.local_addr().unwrap()).collect(),
            DialMode::DirectOnly,
        )
        .unwrap();
    }
    for hole in &holes[..MAX_DIALS] {
        let mut packet = [0; 1500];
        timeout(Duration::from_millis(500), hole.recv_from(&mut packet))
            .await
            .unwrap()
            .unwrap();
    }
    let counts = connection_counts(&a).await;
    assert_eq!(counts, (0, MAX_DIALS, MAX_DIALS));
    for hole in &holes[MAX_DIALS..] {
        let mut packet = [0; 1500];
        assert!(
            timeout(Duration::from_millis(20), hole.recv_from(&mut packet))
                .await
                .is_err()
        );
    }
    assert!(matches!(
        timeout(DIAL_BUDGET, config.next()).await.unwrap().unwrap(),
        ConfigEvent::DialFailed { peer, .. } if peer == DeviceId([70; 32])
    ));
    for hole in &holes[MAX_DIALS..] {
        let mut packet = [0; 1500];
        timeout(Duration::from_millis(500), hole.recv_from(&mut packet))
            .await
            .expect("remaining addresses get their own round")
            .unwrap();
    }
    assert_eq!(connection_counts(&a).await.1, 4);
    a.stop().await;
}
#[tokio::test]
async fn failed_n1_cancels_old_low_and_reopens_without_losing_c0() {
    let (a, mut a_config, _a_module) = node().await;
    let (b, mut b_config, mut b_module) = node().await;
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly).unwrap();
    let (ac, mut bc) = tokio::join!(connected(&mut a_config), connected(&mut b_config));
    a.set_ready(ac.link);
    b.set_ready(bc.link);
    timeout(Duration::from_secs(2), async {
        while !a.links().ready(b.id()) || !b.links().ready(a.id()) {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let options = LowOptions {
        send_max: 100000,
        receive_max: 100000,
        operation_max: 200000,
    };
    let mut old = a
        .open_low(ac.link, StreamKind::Diagnostics, options)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "initial Low open failed: {e:?}; local endpoints {} and {}",
                a.local_addr(),
                b.local_addr()
            )
        });
    let first = frames::low(7, 1000);
    old.send(&first).await.unwrap();
    let mut inbound = incoming(&mut b_module).await;
    a.0.send(Command::ResetN1(ac.link));
    assert!(matches!(
        timeout(Duration::from_secs(2), old.recv()).await.unwrap(),
        Err(LowError::Interrupted)
    ));
    assert!(matches!(
        timeout(Duration::from_secs(2), inbound.stream.recv()).await.unwrap(),
        Err(LowError::Interrupted)
    ));
    assert!(a.links().ready(b.id()));
    let payload = frames::control(1000);
    ac.control_tx.post(payload.as_slice()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), bc.control_rx.next())
            .await
            .unwrap()
            .unwrap(),
        payload
    );
    sleep(Duration::from_millis(1300)).await;
    let mut replacement = timeout(
        Duration::from_secs(5),
        a.open_low(ac.link, StreamKind::Diagnostics, options),
    )
    .await
    .unwrap()
    .unwrap();
    replacement.send(&first).await.unwrap();
    assert_eq!(incoming(&mut b_module).await.first, first);
    assert!(a.links().ready(b.id()));
    tokio::join!(a.stop(), b.stop());
}

#[tokio::test]
async fn exact_identity_dial_reports_certificate_replacement_without_pairing() {
    let (a, mut events, _) = node().await;
    let (b, _, _) = node().await;
    let expected = DeviceId([87; 32]);
    a.dial(expected, vec![b.local_addr()], DialMode::DirectOnly).unwrap();
    let event = timeout(Duration::from_secs(4), events.next()).await.unwrap().unwrap();
    assert!(matches!(event,ConfigEvent::DialFailed {peer,reason:DialFailure::IdentityChanged} if peer==expected));
    assert!(a.links().links.is_empty());
    tokio::join!(a.stop(), b.stop());
}

#[tokio::test]
async fn protocol_violation_budget_is_per_logical_link_and_reports_suppressed_count_once() {
    let (a, mut ac, mut diagnostics) = node().await;
    let (b, mut bc, _) = node().await;
    let (c, mut cc, _) = node().await;
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly).unwrap();
    let ab = connected(&mut ac).await;
    let _ba = connected(&mut bc).await;
    a.dial(c.id(), vec![c.local_addr()], DialMode::DirectOnly).unwrap();
    let ac_link = connected(&mut ac).await;
    let _ca = connected(&mut cc).await;
    let violation = v1::admission::Violation {
        code: v1::ErrorCode::MalformedFrame,
        kind: 3,
        type_id: Some(0x100),
        capability: None,
        experimental: false,
        isolation: v1::dto::Isolation::Stream,
        reply: true,
        cleanup: true,
        suppress_reopen: false,
    };
    for _ in 0..10 {
        a.report_protocol_violation(ab.link, violation.clone());
        a.report_protocol_violation(ac_link.link, violation.clone());
    }
    let mut peers = HashMap::new();
    timeout(Duration::from_secs(2), async {
        while peers.values().sum::<usize>() < 16 {
            if let Some(ModuleEvent::ProtocolViolation { peer, violation }) = diagnostics.next().await {
                assert_eq!(violation.suppressed, 0);
                *peers.entry(peer).or_insert(0) += 1;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(peers.get(&b.id()), Some(&8));
    assert_eq!(peers.get(&c.id()), Some(&8));
    sleep(Duration::from_millis(1050)).await;
    a.report_protocol_violation(ab.link, violation);
    let next = timeout(Duration::from_secs(2), diagnostics.next())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(next,ModuleEvent::ProtocolViolation{peer,violation} if peer==b.id()&&violation.suppressed==2));
    tokio::join!(a.stop(), b.stop(), c.stop());
}

#[test]
fn address_probe_failures_keep_local_network_causes() {
    use std::io::{Error, ErrorKind};
    for (kind, expected) in [
        (ErrorKind::NetworkUnreachable, AddressProbeError::NoRoute),
        (ErrorKind::AddrNotAvailable, AddressProbeError::NoNetworkInterface),
        (ErrorKind::HostUnreachable, AddressProbeError::Unreachable),
        (ErrorKind::PermissionDenied, AddressProbeError::Socket),
        (ErrorKind::ConnectionRefused, AddressProbeError::Refused),
        (ErrorKind::TimedOut, AddressProbeError::Timeout),
    ] {
        assert_eq!(AddressProbeError::from_io(&Error::from(kind)), expected);
    }
    assert_eq!(
        AddressProbeError::check_route("127.0.0.1:47470".parse().unwrap()),
        Ok(())
    );
    assert_eq!(
        AddressProbeError::from_connect(quinn::ConnectError::EndpointStopping),
        AddressProbeError::Stopped
    );
    assert_eq!(
        AddressProbeError::from_connect(quinn::ConnectError::InvalidRemoteAddress(
            "127.0.0.1:0".parse().unwrap()
        )),
        AddressProbeError::InvalidAddress
    );
    assert_eq!(
        AddressProbeError::from_connection(quinn::ConnectionError::TimedOut),
        AddressProbeError::Timeout
    );
}

#[tokio::test]
async fn a_new_address_connects_during_a_pending_relay_search() {
    let (a, mut ac, _) = node().await;
    let (b, mut bc, _) = node().await;
    a.dial(b.id(), vec![], DialMode::PreferDirect).unwrap();
    // FIFO commands guarantee the address arrives after the empty search starts.
    a.update_addresses(b.id(), vec![b.local_addr()]);
    let (al, bl) = tokio::join!(connected(&mut ac), connected(&mut bc));
    assert_ne!(al.direction, bl.direction);
    assert_eq!(a.links().links.len(), 1);
    assert_eq!(b.links().links.len(), 1);
    tokio::join!(a.stop(), b.stop());
}
