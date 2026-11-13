use super::*;
use crate::negotiation as oink_network_profile;
use crate::{LowOptions, identity::Identity};
#[path = "../../../tests/support/frames.rs"]
mod frames;
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn freezing_config_delivers_accepted_prefix_before_fin() {
    let credentials = crate::tls::Credentials::new(&Identity::generate()).unwrap();
    let (server, client) = transport(&credentials).unwrap();
    let endpoint = quinn::Endpoint::server(server, "127.0.0.1:0".parse().unwrap()).unwrap();
    let mut sender = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    sender.set_default_client_config(client);
    let (outbound, inbound) = tokio::join!(sender.connect(endpoint.local_addr().unwrap(), "oink").unwrap(), async {
        endpoint.accept().await.unwrap().await
    });
    let outbound = outbound.unwrap();
    let inbound = inbound.unwrap();
    let queue = queue::Queue::new(QueueSpec::reliable(StreamKind::Config).0);
    let accepted = frames::control(7);
    let later = frames::control(11);
    queue.post(accepted.clone(), 65536).unwrap();
    let header = StreamHeader {
        route_tag: 0,
        priority: Priority::Normal,
        tag: StreamTag {
            kind: StreamKind::Config,
            ordinal: 1,
        },
        datagram_flow: 0,
    };
    let mut writer = wire::Writer::new(0, header, None, wire::Source::User(queue.clone()), None);
    writer.freeze();
    queue.post(later.clone(), 65536).unwrap();
    let received = timeout(Duration::from_secs(2), async {
        let send = std::future::poll_fn(|cx| {
            while !writer.finished {
                if !writer.step(&outbound, true, cx).unwrap() {
                    return Poll::Pending;
                }
            }
            Poll::Ready(())
        });
        let read = async { inbound.accept_uni().await.unwrap().read_to_end(65536).await.unwrap() };
        tokio::join!(send, read).1
    })
    .await;
    sender.close(0u32.into(), b"done");
    endpoint.close(0u32.into(), b"done");
    let mut expected = Vec::new();
    header.encode(&mut expected);
    expected.extend(accepted);
    assert_eq!(
        received.expect("accepted C0 prefix must precede migration FIN"),
        expected
    );
    assert_eq!(
        queue.take().unwrap(),
        Some(later),
        "post-freeze traffic belongs to the new path"
    );
}

#[tokio::test]
async fn a_v1_only_server_is_recognized_by_its_alpn_refusal() {
    let credentials = crate::tls::Credentials::new(&Identity::generate()).unwrap();
    let (_, client) = transport(&credentials).unwrap();
    // 0.3.x accepts only the v1 ALPN.
    let mut v1 = credentials.server_config().unwrap();
    v1.alpn_protocols = vec![b"oink-wire/1".to_vec()];
    let server =
        quinn::ServerConfig::with_crypto(Arc::new(quinn::crypto::rustls::QuicServerConfig::try_from(v1).unwrap()));
    let endpoint = quinn::Endpoint::server(server, "127.0.0.1:0".parse().unwrap()).unwrap();
    let mut sender = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    sender.set_default_client_config(client);
    let (outbound, _) = tokio::join!(sender.connect(endpoint.local_addr().unwrap(), "oink").unwrap(), async {
        if let Some(incoming) = endpoint.accept().await {
            let _ = incoming.await;
        }
    });
    let error = outbound.expect_err("ALPN mismatch must fail the handshake");
    assert!(alpn_rejected(&error), "{error:?}");
    assert!(!alpn_rejected(&quinn::ConnectionError::TimedOut));
}

#[tokio::test]
async fn manual_query_closed_udp_port_returns_a_terminal_probe_error() {
    let (a, _, _) = node().await;
    let vacant = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = vacant.local_addr().unwrap();
    drop(vacant);
    let result = timeout(Duration::from_secs(1), a.probe_address(address)).await;
    a.stop().await;
    assert!(matches!(
        result.expect("closed target must finish"),
        Err(AddressProbeError::Refused | AddressProbeError::Timeout)
    ));
}

#[tokio::test]
async fn expired_service_budgets_still_process_queued_commands() {
    let (a, _, _) = node().await;
    let vacant = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = vacant.local_addr().unwrap();
    drop(vacant);
    // Model a descheduled owner resuming after all three absolute deadlines.
    a.0.expired_service_budget.store(true, Ordering::Release);
    let result = timeout(Duration::from_secs(1), a.probe_address(address)).await;
    a.0.expired_service_budget.store(false, Ordering::Release);
    a.stop().await;
    assert_eq!(
        result.expect("expired slices must still service queued commands"),
        Err(AddressProbeError::Refused)
    );
}

#[tokio::test]
#[ignore = "slow: silent UDP probe budget requires wall-clock timeout"]
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
    node_profile(frames::profile()).await
}
async fn node_profile(profile: crate::negotiation::OfferProfile) -> (LinkLayer, ConfigPort, ModulePort) {
    let identity = Arc::new(Identity::generate());
    let boot = Boot {
        device_id: identity.id(),
        nonce: rand::random(),
    };
    let (network, config) = LinkLayer::start(LinkConfig {
        profile,
        identity,
        boot,
        bind: (
            std::net::Ipv4Addr::LOCALHOST,
            crate::test_ports::reserve(std::net::Ipv4Addr::LOCALHOST.into()),
        )
            .into(),
        tier: Tier::Elevated,
    })
    .await
    .unwrap();
    let module = network.register(&[StreamKind::Diagnostics]);
    (network, config, module)
}
#[tokio::test]
async fn loopback_fixture_starts_low_and_keeps_discovery_reserved() {
    let (network, _, _) = node().await;
    let address = network.local_addr();
    assert_eq!(network.low_local_addr(), Some(address));
    let tcp = std::net::TcpStream::connect(address);
    let discovery = std::net::UdpSocket::bind((address.ip(), address.port() + 1));
    network.stop().await;
    // Shutdown releases the eagerly started Low listener.
    let _low_after_stop = std::net::TcpListener::bind(address).unwrap();
    // The fixtures share a QUIC/TCP port and the next UDP port is discovery.
    // OS-assigned UDP ports can be excluded for TCP on Windows. Use the
    // reserved fixture range, below the default OS ephemeral ranges.
    assert!(
        address.port() < 32768,
        "fixture selected an OS ephemeral UDP port: {address}"
    );
    assert!(tcp.is_ok(), "reserved fixture TCP listener unavailable: {tcp:?}");
    assert!(
        discovery.is_err(),
        "discovery socket must stay reserved until discovery starts"
    );
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
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
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
    let focus = hex(include_str!("../../../../oink-protocol/tests/fixtures/v2/0101.hex"));
    let motion = hex(include_str!("../../../../oink-protocol/tests/fixtures/v2/0201.hex"));
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
    a.dial(b.id(), addresses, DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
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
            DialOrigin::Initial,
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
/// Review H17: the owner reopens the endpoint socket after the OS moved the
/// process to another network. Same port, the live link keeps carrying C0
/// and a new peer can still dial in.
#[tokio::test]
async fn requested_socket_rebind_keeps_the_port_and_the_links() {
    let (a, mut a_config, _a_module) = node().await;
    let (b, mut b_config, _b_module) = node().await;
    b.dial(a.id(), vec![a.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
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
    let address = a.local_addr();
    timeout(Duration::from_secs(2), a.rebind_socket())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.local_addr(), address);
    let payload = frames::control(1000);
    ac.control_tx.post(payload.as_slice()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), bc.control_rx.next())
            .await
            .unwrap()
            .unwrap(),
        payload
    );
    let (c, mut c_config, _c_module) = node().await;
    c.dial(a.id(), vec![address], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
    connected(&mut c_config).await;
    assert!(a.links().ready(b.id()));
    tokio::join!(a.stop(), b.stop(), c.stop());
}

/// User 2026-11-12: when the shared endpoint's driver dies (2026-11-12 a
/// receive error ended it) the layer exits with a fatal error instead of
/// failing every dial in place; its owner restarts it with a new Boot.
#[tokio::test]
async fn a_dead_endpoint_driver_ends_the_layer_with_a_fatal_error() {
    let (a, mut a_config, _a_module) = node().await;
    let (b, mut b_config, _b_module) = node().await;
    b.dial(a.id(), vec![a.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
    let (ac, _bc) = tokio::join!(connected(&mut a_config), connected(&mut b_config));
    let address = a.local_addr();
    a.fail_endpoint_socket();
    let exited = timeout(Duration::from_secs(5), a.exited())
        .await
        .expect("the layer ends on its own");
    match &exited {
        Err(NetworkFatal::EndpointDriverStopped {
            detected_by,
            local_address,
        }) => {
            assert!(["accept", "dial"].contains(detected_by), "{detected_by}");
            assert_eq!(*local_address, Some(address));
        }
        other => panic!("expected a dead endpoint driver, got {other:?}"),
    }
    assert_eq!(a.fatal(), exited.clone().err());
    // Its link is reported lost before the config port ends. The dead driver
    // drops every connection's channel, so a link whose connection already
    // ended is lost as ConnectionLost (its real cause); one still up when the
    // thread exits is lost as Stopped. Which comes first is a race.
    timeout(Duration::from_secs(2), async {
        loop {
            match a_config.next().await {
                Some(ConfigEvent::Lost { link, reason, .. }) if link == ac.link => {
                    assert!(
                        matches!(reason, LinkLost::ConnectionLost | LinkLost::Stopped),
                        "{reason:?}"
                    );
                    break;
                }
                Some(_) => continue,
                None => panic!("config port ended before the link was reported lost"),
            }
        }
    })
    .await
    .unwrap();
    assert!(a.links().links.is_empty());
    // Stopping the ended layer returns at once and keeps the reason.
    timeout(Duration::from_secs(2), a.stop()).await.unwrap();
    assert_eq!(a.exited().await, exited);
    // A requested stop is not a failure.
    b.stop().await;
    assert_eq!(b.exited().await, Ok(()));
    assert_eq!(b.fatal(), None);
}

#[tokio::test]
async fn fresh_half_open_replaces_the_old_member_low_fence_before_ready() {
    let mut left = node().await;
    let mut right = node().await;
    // The smaller identity starts TCP Race. Keep its peer half-open until the
    // race has completed, so N1 necessarily arrives before that peer's Ready.
    if left.0.id() > right.0.id() {
        std::mem::swap(&mut left, &mut right);
    }
    let (a, mut a_config, _a_module) = left;
    let (b, mut b_config, mut b_module) = right;
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
    let (ac, bc) = tokio::join!(connected(&mut a_config), connected(&mut b_config));
    // Retain listener facts from the old membership, as a real group merge
    // does on the same authenticated physical connection.
    b.set_ready(bc.link);
    timeout(Duration::from_secs(4), async {
        while !a
            .0
            .hub
            .view
            .load()
            .hops
            .iter()
            .any(|hop| hop.neighbor.device_id == b.id() && hop.peer_info.load().is_some())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    b.retire(bc.link);
    b.config_phase_applied();
    let has_low_route = || {
        b.0.hub
            .view
            .load()
            .routes
            .iter()
            .any(|route| route.endpoint.is_some_and(|endpoint| endpoint.link == bc.link))
    };
    assert!(!has_low_route(), "proof exchange alone must not grant Low authority");
    b.set_half_open(bc.link);
    assert!(
        has_low_route(),
        "freshly accepted membership must admit TCP bootstrap before Done"
    );
    a.set_ready(ac.link);
    timeout(Duration::from_secs(4), async {
        while !a.low_status(ac.link).is_some_and(|status| status.warm_ready) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("TCP Race must not be lost while the peer waits for Done");
    assert_eq!(b.links().by_peer(a.id()).unwrap().state, LinkState::HalfOpen);
    b.set_ready(bc.link);
    let options = LowOptions {
        send_max: 100000,
        receive_max: 100000,
        operation_max: 200000,
    };
    let mut stream = a.open_low(ac.link, StreamKind::Diagnostics, options).await.unwrap();
    let first = frames::low(7, 1000);
    stream.send(&first).await.unwrap();
    let inbound = incoming(&mut b_module).await;
    assert_eq!(inbound.first, first);
    b.retire(bc.link);
    b.config_phase_applied();
    assert!(
        !has_low_route(),
        "retirement must withdraw the accepted membership again"
    );
    assert!(matches!(
        timeout(Duration::from_secs(2), stream.recv()).await.unwrap(),
        Err(LowError::Interrupted)
    ));
    tokio::join!(a.stop(), b.stop());
}

#[tokio::test]
async fn failed_n1_cancels_old_low_and_reopens_without_losing_c0() {
    let (a, mut a_config, _a_module) = node().await;
    let (b, mut b_config, mut b_module) = node().await;
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
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
async fn link_without_n1_reports_low_admission_as_the_bulk_reason() {
    let (a, mut a_config, _a_module) = node().await;
    let (b, mut b_config, _b_module) = node().await;
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
    let (ac, bc) = tokio::join!(connected(&mut a_config), connected(&mut b_config));
    a.set_ready(ac.link);
    b.set_ready(bc.link);
    timeout(Duration::from_secs(4), async {
        while !a.links().by_peer(b.id()).is_some_and(|e| e.low_available) {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(a.links().by_peer(b.id()).unwrap().low_failure, None);
    a.0.send(Command::ResetN1(ac.link));
    // N1 is retried after one second; the reason is published at once.
    let failure = timeout(Duration::from_millis(900), async {
        loop {
            if let Some(failure) = a.links().by_peer(b.id()).and_then(|e| e.low_failure.clone()) {
                break failure;
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(!a.links().by_peer(b.id()).unwrap().low_available);
    assert_eq!((failure.from, failure.to), (a.id(), b.id()));
    assert!(
        matches!(&failure.reason, crate::LowFailureReason::Tcp { stage, detail }
            if stage == "admission" && detail.starts_with("low-priority admission is unavailable")),
        "{failure:?}"
    );
    tokio::join!(a.stop(), b.stop());
}

#[tokio::test]
async fn exact_identity_dial_reports_certificate_replacement_without_pairing() {
    let (a, mut events, _) = node().await;
    let (b, _, _) = node().await;
    let expected = DeviceId([87; 32]);
    a.dial(
        expected,
        vec![b.local_addr()],
        DialMode::DirectOnly,
        DialOrigin::Initial,
    )
    .unwrap();
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
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
    let ab = connected(&mut ac).await;
    let _ba = connected(&mut bc).await;
    a.dial(c.id(), vec![c.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
    let ac_link = connected(&mut ac).await;
    let _ca = connected(&mut cc).await;
    let violation = v2::admission::Violation {
        code: v2::ErrorCode::MalformedFrame,
        kind: 3,
        type_id: Some(0x100),
        capability: None,
        experimental: false,
        isolation: v2::dto::Isolation::Stream,
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
    a.dial(b.id(), vec![], DialMode::PreferDirect, DialOrigin::Initial)
        .unwrap();
    // FIFO commands guarantee the address arrives after the empty search starts.
    a.update_addresses(b.id(), vec![b.local_addr()]);
    let (al, bl) = tokio::join!(connected(&mut ac), connected(&mut bc));
    assert_ne!(al.direction, bl.direction);
    assert_eq!(a.links().links.len(), 1);
    assert_eq!(b.links().links.len(), 1);
    tokio::join!(a.stop(), b.stop());
}

#[tokio::test]
#[ignore = "slow: heartbeat period requires wall-clock scheduling"]
async fn fourth_heartbeat_writes_n0_i0_m0_in_one_poll_turn_and_repeats_after_two_seconds() {
    let mut profile = frames::profile();
    profile
        .capabilities
        .extend(["oink.audio/2".into(), "oink.audio/2".into()]);
    let (a, mut ac, _) = node_profile(profile.clone()).await;
    let (b, mut bc, _) = node_profile(profile).await;
    let mut input = b.register(&[StreamKind::Input]);
    let mut audio = b.register(&[StreamKind::Audio]);
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
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
        (0..text.trim().len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text.trim()[i..i + 2], 16).unwrap())
            .collect()
    }
    let mut sends = Vec::new();
    for (kind, encoded) in [
        (
            StreamKind::Input,
            hex(include_str!("../../../../oink-protocol/tests/fixtures/v2/0102.hex")),
        ),
        (
            StreamKind::Audio,
            hex(include_str!("../../../../oink-protocol/tests/fixtures/v2/0602.hex")),
        ),
    ] {
        let (tx, _) = a
            .open_stream(
                al.link,
                if kind == StreamKind::Input {
                    Priority::Interactive
                } else {
                    Priority::Normal
                },
                StreamTag { kind, ordinal: 1 },
                QueueSpec::reliable(kind).0,
                None,
            )
            .unwrap();
        // Only registration: the first records must open their original streams on the heartbeat.
        tx.set_heartbeat_messages(vec![encoded.clone()]).unwrap();
        sends.push((tx, encoded));
    }
    let mut receives = Vec::new();
    for (port, kind) in [(&mut input, StreamKind::Input), (&mut audio, StreamKind::Audio)] {
        let inbound = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(ModuleEvent::Inbound(stream)) = port.next().await {
                    break stream;
                }
            }
        })
        .await
        .unwrap();
        let (rx, _) = inbound.accept(QueueSpec::reliable(kind).1, QueueSpec::datagram(kind));
        receives.push(rx);
    }
    for _ in 0..2 {
        for (index, rx) in receives.iter_mut().enumerate() {
            assert_eq!(
                timeout(Duration::from_secs(4), rx.next()).await.unwrap().unwrap(),
                sends[index].1
            );
        }
    }
    let batches = a.0.heartbeat_batches.lock().unwrap().clone();
    tokio::join!(a.stop(), b.stop());
    let periodic: Vec<_> = batches
        .iter()
        .filter(|(_, _, kinds)| kinds.contains(&StreamKind::Input) || kinds.contains(&StreamKind::Audio))
        .collect();
    assert!(periodic.len() >= 2);
    for (nonce, _, kinds) in &periodic {
        assert!(nonce.is_multiple_of(4));
        assert!(kinds.contains(&StreamKind::Internal));
        assert!(kinds.contains(&StreamKind::Input));
        assert!(kinds.contains(&StreamKind::Audio));
    }
    assert_eq!(periodic[1].0 - periodic[0].0, 4);
}

#[tokio::test]
async fn occupied_ports_still_allow_probe_quic_and_low_with_announced_ports() {
    for (hold_udp, hold_tcp) in [(true, false), (false, true), (true, true)] {
        let preferred = SocketAddr::from((
            std::net::Ipv4Addr::LOCALHOST,
            crate::test_ports::reserve(std::net::Ipv4Addr::LOCALHOST.into()),
        ));
        let udp = crate::test_ports::take_udp(preferred).unwrap();
        let tcp = crate::test_ports::take_tcp(preferred).unwrap();
        let _udp = hold_udp.then_some(udp);
        let _tcp = hold_tcp.then_some(tcp);
        let identity = Arc::new(Identity::generate());
        let (b, mut bc) = LinkLayer::start(LinkConfig {
            profile: frames::profile(),
            boot: Boot {
                device_id: identity.id(),
                nonce: rand::random(),
            },
            identity,
            bind: preferred,
            tier: Tier::Elevated,
        })
        .await
        .unwrap();
        if hold_udp {
            assert_ne!(b.local_addr(), preferred);
        }
        if hold_tcp {
            assert_ne!(b.low_local_addr(), Some(preferred));
        }
        let mut b_module = b.register(&[StreamKind::Diagnostics]);
        let (a, mut ac, _) = node().await;
        // A manual probe opens the actual announced QUIC service.
        let found = a.probe_address(b.local_addr()).await.unwrap();
        assert_eq!(found, b.id());
        let (ac, bc) = tokio::join!(connected(&mut ac), connected(&mut bc));
        a.set_ready(ac.link);
        b.set_ready(bc.link);
        timeout(Duration::from_secs(2), async {
            while !a.links().ready(b.id()) || !b.links().ready(a.id()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut low = a
            .open_low(
                ac.link,
                StreamKind::Diagnostics,
                LowOptions {
                    send_max: 100000,
                    receive_max: 100000,
                    operation_max: 200000,
                },
            )
            .await
            .unwrap();
        let first = frames::low(7, 1000);
        low.send(&first).await.unwrap();
        // TCP can differ from QUIC: only the authenticated frozen Low::Info
        // contains the dial destination; no port arithmetic can pass this case.
        assert_eq!(incoming(&mut b_module).await.first, first);
        tokio::join!(a.stop(), b.stop());
    }
}

/// One-client UDP relay on loopback with a fixed one-way delay: a second
/// "route" to the same peer whose RTT the test controls.
fn delayed_route(target: SocketAddr, delay: Duration) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let address = socket.local_addr().unwrap();
    let socket = Arc::new(tokio::net::UdpSocket::from_std(socket).unwrap());
    let task = tokio::spawn(async move {
        let mut client = None;
        let mut packet = [0; 65536];
        loop {
            let Ok((size, source)) = socket.recv_from(&mut packet).await else {
                return;
            };
            let destination = if source == target {
                let Some(client) = client else { continue };
                client
            } else {
                client = Some(source);
                target
            };
            let bytes = packet[..size].to_vec();
            let socket = socket.clone();
            tokio::spawn(async move {
                if !delay.is_zero() {
                    sleep(delay).await;
                }
                let _ = socket.send_to(&bytes, destination).await;
            });
        }
    });
    (address, task)
}
async fn take_message(receive: &mut RecvStream) -> Vec<u8> {
    timeout(Duration::from_secs(4), async {
        loop {
            if let Some((bytes, _)) = receive.take_timed().unwrap() {
                break bytes;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[test]
fn route_switches_to_a_clearly_faster_direct_connection_and_keeps_streams() {
    watchdog::run(
        module_path!(),
        "route_switches_to_a_clearly_faster_direct_connection_and_keeps_streams",
        None,
        route_switch_scenario(),
    );
}
async fn route_switch_scenario() {
    let (x, xc, _) = node().await;
    let (y, yc, _) = node().await;
    // The arbiter (smaller id) dials, so its own standby is the faster route.
    let ((m, mut mc), (f, mut fc)) = if x.id() < y.id() {
        ((x, xc), (y, yc))
    } else {
        ((y, yc), (x, xc))
    };
    let mut port = f.register(&[StreamKind::Input]);
    let (slow, slow_task) = delayed_route(f.local_addr(), Duration::from_millis(8));
    let (fast, fast_task) = delayed_route(f.local_addr(), Duration::ZERO);
    let vacant = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let dead = vacant.local_addr().unwrap();
    m.dial(f.id(), vec![slow], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
    let (ml, fl) = tokio::join!(connected(&mut mc), connected(&mut fc));
    assert_eq!(ml.remote_address, Some(slow));
    m.update_route_addresses(f.id(), vec![slow, fast, dead]);
    m.set_ready(ml.link);
    f.set_ready(fl.link);
    timeout(Duration::from_secs(4), async {
        while !m.links().ready(f.id()) || !f.links().ready(m.id()) {
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
    let focus = hex(include_str!("../../../../oink-protocol/tests/fixtures/v2/0101.hex"));
    let motion = hex(include_str!("../../../../oink-protocol/tests/fixtures/v2/0201.hex"));
    let (send, _) = m
        .open_stream(
            ml.link,
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
    assert_eq!(take_message(&mut receive).await, focus);
    watchdog::phase("idle link moves to the faster standby connection");
    let switched = timeout(Duration::from_secs(60), async {
        loop {
            if let Some(entry) = m.links().by_peer(f.id())
                && entry.remote_address == Some(fast)
            {
                break entry.clone();
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("automatic switch to the faster route");
    assert_eq!(switched.link, ml.link, "the logical link survives the switch");
    let route = switched.route.clone().expect("route view");
    assert_eq!(route.direction, Direction::Outbound);
    let row = |address| route.rows.iter().find(|r| r.address == address).copied().unwrap();
    assert!(row(fast).current);
    assert_eq!(row(dead).state, RouteRowState::Unreachable);
    assert_ne!(
        row(slow).state,
        RouteRowState::Unreachable,
        "the old route stays a standby"
    );
    let peer_view = f.links().by_peer(m.id()).unwrap().route.clone().unwrap();
    assert_eq!(peer_view.direction, Direction::Inbound);
    // The same stream handles keep working across the switch, in order.
    send.post(motion.clone()).unwrap();
    assert_eq!(take_message(&mut receive).await, motion);
    let mut changed = false;
    while let Ok(event) = mc.0.try_recv() {
        changed |= matches!(event, ConfigEvent::PathChanged { link, path: LinkPath::Direct } if link == ml.link);
    }
    assert!(changed, "the owner sees the direct-to-direct path change");
    tokio::join!(m.stop(), f.stop());
    slow_task.abort();
    fast_task.abort();
    drop(vacant);
}

/// The next Connected event, past the events of links already up.
async fn connected_later(config: &mut ConfigPort) -> Box<LinkConnected> {
    timeout(Duration::from_secs(4), async {
        loop {
            if let Some(ConfigEvent::Connected(link)) = config.next().await {
                break link;
            }
        }
    })
    .await
    .unwrap()
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    timeout(Duration::from_secs(10), async {
        while !done() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

/// Review L3 (protocol-v2 11.2): a stream of a link opened on a standby
/// connection is refused as InvalidChannel, not as a generic Invalid.
#[tokio::test]
async fn a_link_stream_on_a_standby_connection_is_refused_as_invalid_channel() {
    let (a, mut a_config, mut a_module) = node().await;
    let (b, mut b_config, _b_module) = node().await;
    let (second, second_task) = delayed_route(b.local_addr(), Duration::ZERO);
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
    let (ac, bc) = tokio::join!(connected(&mut a_config), connected(&mut b_config));
    a.update_route_addresses(b.id(), vec![b.local_addr(), second]);
    // Streams open only on what C0 negotiated (the send gate).
    for (network, link) in [(&a, ac.link), (&b, bc.link)] {
        network.set_agreement(
            link,
            v2::admission::Agreement::new(frames::profile().capabilities).unwrap(),
        );
    }
    a.set_ready(ac.link);
    b.set_ready(bc.link);
    until("standby to the second address", || {
        a.links()
            .by_peer(b.id())
            .and_then(|e| e.route.clone())
            .is_some_and(|route| {
                route
                    .rows
                    .iter()
                    .any(|r| r.address == second && r.state == RouteRowState::Reachable)
            })
    })
    .await;
    a.0.send(Command::StreamsOn(Some(second)));
    let (send, _) = a
        .open_stream(
            ac.link,
            Priority::Normal,
            oink_protocol::frame::StreamTag {
                kind: StreamKind::Diagnostics,
                ordinal: 1,
            },
            QueueSpec::reliable(StreamKind::Diagnostics).0,
            None,
        )
        .unwrap();
    // A D0 frame the send gate admits (DiagnosticsHello, 0x0a00, empty body):
    // the peer refuses the stream at its header, before reading any frame.
    send.post(vec![0x00, 0x0a, 0, 0, 0, 0]).unwrap();
    let reason = timeout(Duration::from_secs(4), async {
        loop {
            if let Some(ModuleEvent::StreamClosed { reason, .. }) = a_module.next().await {
                break reason;
            }
        }
    })
    .await
    .expect("the peer refuses the stream");
    assert_eq!(reason, StreamClosed::Peer(StreamCode::InvalidChannel));
    tokio::join!(a.stop(), b.stop());
    second_task.abort();
}

/// Review M7: the realtime network thread never enumerates interfaces (it did
/// every 2 s while any link existed); the owner hands it the local addresses,
/// and an address of this device is not dialed as a route.
#[tokio::test]
async fn the_network_thread_takes_local_addresses_and_never_enumerates() {
    let (a, mut a_config, _a_module) = node().await;
    let (b, mut b_config, _b_module) = node().await;
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
    let (ac, bc) = tokio::join!(connected(&mut a_config), connected(&mut b_config));
    a.set_ready(ac.link);
    b.set_ready(bc.link);
    let own = SocketAddr::from(([198, 51, 100, 7], b.local_addr().port()));
    a.set_local_addresses(vec![std::net::Ipv4Addr::new(198, 51, 100, 7)]);
    a.update_route_addresses(b.id(), vec![b.local_addr(), own]);
    until("ready", || a.links().ready(b.id()) && b.links().ready(a.id())).await;
    // Past the old 2 s poll: the network threads are alive (a debug build
    // asserts against enumeration on them).
    sleep(Duration::from_millis(2500)).await;
    for network in [&a, &b] {
        connection_counts(network).await;
    }
    let route = a.links().by_peer(b.id()).unwrap().route.clone().unwrap();
    let row = route.rows.iter().find(|r| r.address == own).unwrap();
    assert_eq!(row.state, RouteRowState::Unreachable, "own address is not dialed");
    tokio::join!(a.stop(), b.stop());
}

/// Phone-like leaf capabilities: no transit role. This is a transport test on
/// real loopback TCP/TLS, without a mobile app or native clipboard backend.
#[test]
fn a_phone_endpoint_transfers_low_in_both_directions_through_a_desktop() {
    watchdog::run(
        module_path!(),
        "a_phone_endpoint_transfers_low_in_both_directions_through_a_desktop",
        None,
        phone_relay_low_scenario(),
    );
}

async fn phone_relay_low_scenario() {
    let mut phone = frames::profile();
    phone.capabilities.retain(|capability| {
        matches!(
            capability.as_str(),
            "oink.core/2"
                | "oink.group/2"
                | "oink.device/2"
                | "oink.clipboard/2"
                | "oink.low/1"
                | "oink.relay/1"
                | "oink.diagnostics/2"
        )
    });
    assert!(
        !phone
            .capabilities
            .iter()
            .any(|capability| capability == "oink.relay.transit/1")
    );
    let agreement = v2::admission::Agreement::new(phone.capabilities.clone()).unwrap();
    let (a, mut ac, mut am) = node().await;
    let (r, mut rc, _) = node().await;
    let (b, mut bc, mut bm) = node_profile(phone).await;
    for (endpoint, config) in [(&a, &mut ac), (&b, &mut bc)] {
        endpoint
            .dial(r.id(), vec![r.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
            .unwrap();
        let (er, re) = tokio::join!(connected_later(config), connected_later(&mut rc));
        endpoint.set_ready(er.link);
        r.set_ready(re.link);
    }
    until("phone and desktop are direct neighbors of the relay", || {
        a.links().ready(r.id()) && b.links().ready(r.id()) && r.links().ready(a.id()) && r.links().ready(b.id())
    })
    .await;
    a.dial(b.id(), vec![], DialMode::PreferDirect, DialOrigin::Initial)
        .unwrap();
    let (ab, ba) = tokio::join!(connected_later(&mut ac), connected_later(&mut bc));
    assert!(matches!(ab.path, LinkPath::Relayed { .. }));
    assert!(matches!(ba.path, LinkPath::Relayed { .. }));
    for (network, link) in [(&a, ab.link), (&b, ba.link)] {
        network.set_agreement(link, agreement.clone());
        network.set_ready(link);
    }
    until("phone relayed path ready", || {
        a.links().ready(b.id()) && b.links().ready(a.id())
    })
    .await;
    let options = LowOptions {
        send_max: 500_000,
        receive_max: 500_000,
        operation_max: 1_000_000,
    };
    for (source, link, destination) in [(&a, ab.link, &mut bm), (&b, ba.link, &mut am)] {
        let mut stream = timeout(
            Duration::from_secs(5),
            source.open_low(link, StreamKind::Diagnostics, options),
        )
        .await
        .unwrap()
        .unwrap();
        let payload = frames::low(1, 16_000);
        stream.send(&payload).await.unwrap();
        let mut inbound = incoming(destination).await;
        assert_eq!(inbound.first, payload);
        for _ in 0..19 {
            stream.send(&payload).await.unwrap();
            assert_eq!(inbound.stream.recv().await.unwrap(), Some(payload.clone()));
        }
        let reply = frames::low(2, 100);
        inbound.stream.send(&reply).await.unwrap();
        inbound.stream.finish();
        assert_eq!(stream.recv().await.unwrap(), Some(reply));
        stream.finish();
        assert!(inbound.stream.recv().await.unwrap().is_none());
        assert!(stream.recv().await.unwrap().is_none());
    }
    tokio::join!(a.stop(), b.stop(), r.stop());
}

/// Review H3, change plan 6.1: a device reaches a peer through a relay. Its
/// pair with the relay has a second direct connection, but the current one
/// carries the relayed path (and, at the relay, its transit): no switch moves
/// away from it, and Low over the relayed link keeps working. (Unfixed, the
/// switch went ahead and the relay closed that connection's Low hop.)
#[test]
fn a_direct_switch_with_the_relay_keeps_low_on_the_relayed_path() {
    watchdog::run(
        module_path!(),
        "a_direct_switch_with_the_relay_keeps_low_on_the_relayed_path",
        None,
        transit_switch_scenario(),
    );
}
async fn transit_switch_scenario() {
    let mut nodes = vec![node().await, node().await, node().await];
    // The source has the smallest id: it is the arbiter of its pair with the
    // relay and runs that pair's switch.
    nodes.sort_by_key(|(network, _, _)| network.id());
    let mut nodes = nodes.into_iter();
    let (a, mut a_config, _a_module) = nodes.next().unwrap();
    let (r, mut r_config, _r_module) = nodes.next().unwrap();
    let (b, mut b_config, mut b_module) = nodes.next().unwrap();
    let options = LowOptions {
        send_max: 100000,
        receive_max: 100000,
        operation_max: 200000,
    };
    let (second, second_task) = delayed_route(r.local_addr(), Duration::ZERO);
    a.dial(r.id(), vec![r.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
    let (ar, ra) = tokio::join!(connected(&mut a_config), connected(&mut r_config));
    a.set_ready(ar.link);
    r.set_ready(ra.link);
    b.dial(r.id(), vec![r.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
    let (br, rb) = tokio::join!(connected(&mut b_config), connected_later(&mut r_config));
    b.set_ready(br.link);
    r.set_ready(rb.link);
    until("neighbors ready", || {
        a.links().ready(r.id()) && r.links().ready(a.id()) && b.links().ready(r.id()) && r.links().ready(b.id())
    })
    .await;
    a.dial(b.id(), vec![], DialMode::PreferDirect, DialOrigin::Initial)
        .unwrap();
    let (ab, ba) = tokio::join!(connected_later(&mut a_config), connected_later(&mut b_config));
    assert!(matches!(ab.path, LinkPath::Relayed { .. }));
    // A relayed link's Low admission uses the agreement its C0 negotiated.
    for (network, link) in [(&a, ab.link), (&b, ba.link)] {
        network.set_agreement(
            link,
            v2::admission::Agreement::new(frames::profile().capabilities).unwrap(),
        );
    }
    a.set_ready(ab.link);
    b.set_ready(ba.link);
    until("relayed link ready", || {
        a.links().ready(b.id()) && b.links().ready(a.id())
    })
    .await;
    let mut low = timeout(
        Duration::from_secs(5),
        a.open_low(ab.link, StreamKind::Diagnostics, options),
    )
    .await
    .unwrap()
    .expect("Low over the relay before the switch");
    low.send(&frames::low(1, 100)).await.unwrap();
    assert_eq!(incoming(&mut b_module).await.first, frames::low(1, 100));
    low.finish();
    // A second address of the relay becomes a standby; the pair switches to it.
    a.update_route_addresses(r.id(), vec![r.local_addr(), second]);
    until("standby to the relay's second address", || {
        a.links()
            .by_peer(r.id())
            .and_then(|e| e.route.clone())
            .is_some_and(|route| {
                route
                    .rows
                    .iter()
                    .any(|row| row.address == second && row.state == RouteRowState::Reachable)
            })
    })
    .await;
    let (reply, switched) = std::sync::mpsc::sync_channel(1);
    a.0.send(Command::SwitchRoute(r.id(), second, reply));
    assert!(
        !switched.recv().unwrap(),
        "the relayed path still rides the current connection"
    );
    assert_eq!(a.links().by_peer(r.id()).unwrap().remote_address, Some(r.local_addr()));
    // Past the one-second N1 retry: a closed hop would be retried and refused.
    sleep(Duration::from_millis(1500)).await;
    let mut low = timeout(
        Duration::from_secs(5),
        a.open_low(ab.link, StreamKind::Diagnostics, options),
    )
    .await
    .expect("Low open deadline")
    .expect("Low over the relay after the switch");
    low.send(&frames::low(2, 100)).await.unwrap();
    assert_eq!(incoming(&mut b_module).await.first, frames::low(2, 100));
    low.finish();
    assert!(a.links().ready(b.id()) && b.links().ready(a.id()));
    tokio::join!(a.stop(), r.stop(), b.stop());
    second_task.abort();
}

/// Real QUIC loopback: either endpoint's click owns the pair, independent of
/// the initial dialer. No input backend, desktop, or system clipboard is used.
#[tokio::test]
async fn manual_route_is_pair_owned_and_automatic_from_acceptor_clears_it() {
    let (a, mut ac, _) = node().await;
    let (b, mut bc, _) = node().await;
    a.update_route_addresses(b.id(), vec![b.local_addr()]);
    b.update_route_addresses(a.id(), vec![a.local_addr()]);
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly, DialOrigin::Initial)
        .unwrap();
    let (al, bl) = tokio::join!(connected(&mut ac), connected(&mut bc));
    a.set_ready(al.link);
    b.set_ready(bl.link);
    until("both endpoints ready", || {
        a.links().ready(b.id()) && b.links().ready(a.id())
    })
    .await;
    let ip_a = match a.local_addr().ip() {
        std::net::IpAddr::V4(ip) => ip,
        _ => unreachable!(),
    };
    let ip_b = match b.local_addr().ip() {
        std::net::IpAddr::V4(ip) => ip,
        _ => unreachable!(),
    };
    assert!(a.set_route(b.id(), RouteChoice::Pin(ip_b)));
    until("initial dialer owns manual route", || {
        a.links()
            .by_peer(b.id())
            .and_then(|e| e.route.clone())
            .is_some_and(|r| r.pin == Some(RouteRecord::Dial(ip_b)) && !r.pin_unavailable)
            && b.links()
                .by_peer(a.id())
                .and_then(|e| e.route.clone())
                .is_some_and(|r| r.pin == Some(RouteRecord::Incoming(ip_b)))
    })
    .await;
    assert!(b.set_route(a.id(), RouteChoice::Pin(ip_a)));
    until("acceptor establishes its own new connection", || {
        b.links()
            .by_peer(a.id())
            .and_then(|e| e.route.clone())
            .is_some_and(|r| {
                r.pin == Some(RouteRecord::Dial(ip_a)) && !r.pin_unavailable && r.direction == Direction::Outbound
            })
            && a.links()
                .by_peer(b.id())
                .and_then(|e| e.route.clone())
                .is_some_and(|r| r.pin == Some(RouteRecord::Incoming(ip_a)) && r.direction == Direction::Inbound)
    })
    .await;
    let revision = a.links().by_peer(b.id()).unwrap().route_revision;
    assert!(a.set_route(b.id(), RouteChoice::Auto));
    until("automatic clears both records", || {
        [(&a, b.id()), (&b, a.id())].iter().all(|(n, peer)| {
            n.links()
                .by_peer(*peer)
                .and_then(|e| e.route.clone())
                .is_some_and(|r| r.pin.is_none())
        })
    })
    .await;
    assert_eq!(a.links().by_peer(b.id()).unwrap().route_revision, revision);
    tokio::join!(a.stop(), b.stop());
}
