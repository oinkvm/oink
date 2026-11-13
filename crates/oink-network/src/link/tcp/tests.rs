use super::*;
use crate::link::control::{NextHopBinding, PathInstall, PathRequestId};
use oink_foundation::session::{BootPair, PathId, RouteRevision};
use tokio::time::timeout;

struct Rig {
    core: Arc<Core>,
    grants: Vec<Grant>,
    receivers: Vec<mpsc::Receiver<super::port::ControlQueued>>,
}
impl Rig {
    fn new(count: u8) -> Self {
        let boot = Boot {
            device_id: DeviceId([1; 32]),
            nonce: [1; 16],
        };
        let wake = Arc::new(Notify::new());
        let hub = Arc::new(Hub::new(wake));
        let (commands, _) = mpsc::unbounded_channel();
        let network = Arc::new(Port {
            boot,
            hub: hub.clone(),
            commands,
        });
        let mut view = View::default();
        let mut receivers = vec![];
        for n in 2..count + 2 {
            let peer = Boot {
                device_id: DeviceId([n; 32]),
                nonce: [n; 16],
            };
            let connection = ConnectionId {
                initiator_boot: boot,
                random: [n; 16],
            };
            let (hop, receiver) = N1::new(
                connection,
                peer,
                "127.0.0.1:9".parse().unwrap(),
                Arc::new(arc_swap::ArcSwapOption::empty()),
            );
            view.hops.push(hop);
            receivers.push(receiver);
            view.routes.push(Route {
                agreement: Some(
                    oink_protocol::v2::admission::Agreement::new(
                        oink_protocol::v2::registry::BASELINE
                            .iter()
                            .chain(["oink.diagnostics/2", "oink.low/1"].iter())
                            .map(|s| s.to_string()),
                    )
                    .unwrap(),
                ),
                endpoint: Some(Endpoint {
                    link: LinkId(n as u64),
                    peer,
                }),
                wire: PathInstall {
                    request_id: PathRequestId {
                        source_boot: boot,
                        random: [n; 16],
                    },
                    path_id: PathId {
                        boot_pair: BootPair::new(boot, peer).unwrap(),
                        coordinator_counter: 1,
                    },
                    route_revision: RouteRevision(0),
                    reservation: None,
                    next_hop_binding: NextHopBinding {
                        peer: peer.device_id,
                        connection_id: connection,
                        route_tag: n as u64,
                    },
                },
                source: boot,
                target: peer,
                hops: vec![connection],
                ready: true,
            });
        }
        hub.view.store(Arc::new(view));
        let grants = (2..count + 2)
            .map(|n| network.outgoing(LinkId(n as u64)).unwrap())
            .collect();
        let core = Arc::new_cyclic(|me| Core {
            cfg: Config {
                boot,
                creds: Arc::new(tls::Credentials::new(&crate::identity::Identity::generate()).unwrap()),
                bind_ip: "127.0.0.1".parse().unwrap(),
                port: 0,
                pacing: true,
                activity: Arc::new(crate::activity::Activity::default()),
                dial_timeout: Duration::from_secs(1),
            },
            network,
            pools: Mutex::new(HashMap::new()),
            admission: Mutex::new(0),
            inbound: Mutex::new(HashSet::new()),
            pacers: Mutex::new(HashMap::new()),
            listener: Mutex::new(None),
            listener_failure: Mutex::new(None),
            runtime: tokio::runtime::Handle::current(),
            stopped: watch::channel(false).0,
            shutdown_done: watch::channel(false).0,
            background: Arc::new(watch::channel(0).0),
            me: me.clone(),
        });
        Self {
            core,
            grants,
            receivers,
        }
    }
    async fn fill(&self, index: usize) -> (Arc<Pool>, Authorization) {
        let grant = self.grants[index];
        let (pool, auth) = self.core.admit(grant, None).await.unwrap();
        for _ in 0..64 {
            drop(self.core.queue_control(grant, LowMsg::Refresh).await.unwrap());
        }
        (pool, auth)
    }
}
#[tokio::test]
async fn queued_peer_info_survives_route_retirement_but_lease_controls_do_not() {
    let mut rig = Rig::new(1);
    let grant = rig.grants[0];
    rig.fill(0).await;
    rig.core.publish_advert(grant).unwrap();
    let view = rig.core.network.hub.view.load_full();
    let mut source = super::super::wire::PoolSource {
        receiver: rig.receivers.remove(0),
        advert: view.hops[0].advert.clone(),
        port: rig.core.network.clone(),
        prefer_info: true,
    };
    rig.core.network.hub.view.store(Arc::new(View {
        routes: vec![],
        hops: view.hops.clone(),
    }));
    assert!(!rig.core.network.low_current(grant));
    let info = source.take().expect("physical Info must survive its old logical route");
    assert!(
        matches!(crate::link::control_wire::decode(&info.bytes).unwrap(), crate::link::control::LinkMsg::Low { message, .. } if matches!(message.as_ref(), LowMsg::Info { .. }))
    );
    assert!(
        source.take().is_none(),
        "a stale lease control still requires its grant"
    );
    rig.core.stop().await;
}

#[tokio::test]
async fn peer_info_before_route_admission_is_available_to_the_first_open() {
    let rig = Rig::new(1);
    let grant = rig.grants[0];
    let view = rig.core.network.hub.view.load_full();
    // The authenticated physical connection receives Info before N0 registers
    // N1 or commits its logical route. No operation is authorized yet.
    rig.core.network.hub.view.store(Arc::new(View::default()));
    assert!(!rig.core.network.low_current(grant));
    let peer_info = Arc::new(arc_swap::ArcSwapOption::empty());
    peer_info.store(Some(Arc::new(LowMsg::Info {
        paced: true,
        port: Some(9),
        failure: None,
    })));
    assert!(rig.core.admit(grant, None).await.is_err());
    let original = &view.hops[0];
    let (hop, _controls) = N1::new(original.connection, original.neighbor, original.address, peer_info);
    rig.core.network.hub.view.store(Arc::new(View {
        routes: view.routes.clone(),
        hops: vec![hop.clone()],
    }));
    // Keep this unit test at the admission boundary, without dialing TCP.
    let pool = rig.core.pool(grant).unwrap();
    lock(&pool.state).warm = WarmState::Opening;
    let (pool, auth) = rig.core.admit(grant, None).await.unwrap();
    let request = LowRequest {
        request: [9; 16],
        connection: [9; 16],
        options: Some(LowOptions {
            send_max: 0,
            receive_max: 0,
            operation_max: 0,
        }),
        extra: false,
        keep_alive: true,
        purpose: LowTcpPurpose::Warm,
        paced: true,
        binding: grant.binding,
        route_tag: 9,
        kind: StreamKind::LowControl as u16,
    };
    rig.core.accept_request(pool.clone(), auth, request).await;
    assert!(
        lock(&pool.state).pending.contains_key(&[9; 16]),
        "the first Open must be admitted"
    );
    assert!(
        matches!(
            hop.advert.load().as_deref().map(|info| &info.message),
            Some(LowMsg::Info { .. })
        ),
        "reply with our physical listener facts too"
    );
    rig.core.stop().await;
}

#[tokio::test]
async fn retiring_last_route_preserves_the_physical_peer_info() {
    let rig = Rig::new(1);
    let grant = rig.grants[0];
    let view = rig.core.network.hub.view.load_full();
    view.hops[0].peer_info.store(Some(Arc::new(LowMsg::Info {
        paced: true,
        port: Some(9),
        failure: None,
    })));
    let old = rig.core.pool(grant).unwrap();
    lock(&old.state).warm = WarmState::Opening;
    rig.core.admit(grant, None).await.unwrap();
    rig.core.network.hub.view.store(Arc::new(View {
        routes: vec![],
        hops: view.hops.clone(),
    }));
    rig.core.revoke_stale();
    assert!(lock(&rig.core.pools).is_empty());
    // Reopening N1 on the same physical connection must inherit its facts too.
    let original = &view.hops[0];
    let (hop, _controls) = N1::new(
        original.connection,
        original.neighbor,
        original.address,
        original.peer_info.clone(),
    );
    rig.core.network.hub.view.store(Arc::new(View {
        routes: view.routes.clone(),
        hops: vec![hop],
    }));
    let replacement = rig.core.pool(grant).unwrap();
    lock(&replacement.state).warm = WarmState::Opening;
    rig.core.admit(grant, None).await.unwrap();
    assert!(!Arc::ptr_eq(&old, &replacement));
    assert_eq!(
        lock(&replacement.state).advert.as_ref().map(|info| info.port),
        Some(Some(9))
    );
    rig.core.stop().await;
}

#[tokio::test]
async fn peer_info_worker_does_not_keep_a_retired_n1_alive() {
    let rig = Rig::new(1);
    let view = rig.core.network.hub.view.load_full();
    let hop = Arc::downgrade(&view.hops[0]);
    rig.core.reconcile().await;
    rig.core.network.hub.view.store(Arc::new(View::default()));
    drop(view);
    rig.core.reconcile().await;
    assert!(
        hop.upgrade().is_none(),
        "the receive worker must release the retired hop and its sender"
    );
    rig.core.stop().await;
}

#[tokio::test]
async fn cached_listener_changes_wait_on_their_hop_not_global_reconciliation() {
    let rig = Rig::new(2);
    let (pool, _) = rig.fill(0).await;
    lock(&pool.state).advert = Some(Advert {
        paced: true,
        port: Some(9),
        failure: None,
    });
    let view = rig.core.network.hub.view.load_full();
    view.hops[0].peer_info.store(Some(Arc::new(LowMsg::Info {
        paced: true,
        port: Some(10),
        failure: None,
    })));
    view.hops[0].peer_info_changed.notify_one();
    timeout(Duration::from_secs(1), rig.core.reconcile())
        .await
        .expect("a saturated hop must not block other routes");
    // Hop zero's mandatory Refresh has no capacity yet. Hop one's newly
    // registered grant can still publish control work immediately.
    assert_eq!(lock(&pool.state).advert.as_ref().unwrap().port, Some(9));
    drop(rig.core.queue_control(rig.grants[1], LowMsg::Refresh).await.unwrap());
    rig.core.stop().await;
}

#[tokio::test]
async fn mandatory_capacity_waits_are_per_hop_and_cancel_with_authority() {
    let mut rig = Rig::new(2);
    rig.fill(0).await;
    let grant = rig.grants[0];
    assert!(
        timeout(Duration::from_millis(30), rig.core.reserve_control(grant))
            .await
            .is_err()
    );
    let other = rig.grants[1];
    rig.core.admit(other, None).await.unwrap();
    rig.core.queue_control(other, LowMsg::Refresh).await.unwrap();
    assert!(matches!(
        rig.receivers[1].try_recv().unwrap().send.message,
        LowMsg::Refresh
    ));
    let waiting = rig.core.reserve_control(grant);
    tokio::pin!(waiting);
    assert!(timeout(Duration::from_millis(10), &mut waiting).await.is_err());
    rig.core.network.hub.view.store(Arc::new(View::default()));
    rig.core.network.hub.changed.notify_waiters();
    assert!(matches!(
        timeout(Duration::from_secs(1), waiting).await.unwrap(),
        Err(NetError::Closed)
    ));
    rig.core.stop().await;
}
#[tokio::test]
async fn open_reserves_before_registering_pending_and_info_does_not_take_capacity() {
    let rig = Rig::new(1);
    let (pool, auth) = rig.fill(0).await;
    let grant = rig.grants[0];
    lock(&pool.state).advert = Some(Advert {
        paced: true,
        port: Some(9),
        failure: None,
    });
    for _ in 0..200 {
        rig.core.publish_advert(grant).unwrap();
    }
    assert_eq!(rig.receivers[0].len(), 64);
    assert!(rig.core.network.hop(grant).unwrap().advert.load().is_some());
    let open = rig.core.open_lease(
        pool.clone(),
        auth,
        1,
        Some(LowOptions {
            send_max: 100,
            receive_max: 100,
            operation_max: 200,
        }),
        LowTcpPurpose::Transfer,
        None,
        StreamKind::Diagnostics as u16,
        None,
    );
    assert!(timeout(Duration::from_millis(30), open).await.is_err());
    assert!(lock(&pool.state).pending.is_empty());
    rig.core.stop().await;
}
#[tokio::test]
async fn ready_reserves_before_incoming_lease_changes() {
    let mut rig = Rig::new(1);
    let (pool, auth) = rig.fill(0).await;
    let grant = rig.grants[0];
    lock(&pool.state).advert = Some(Advert {
        paced: true,
        port: Some(9),
        failure: None,
    });
    let request = LowRequest {
        request: [9; 16],
        connection: [9; 16],
        options: Some(LowOptions {
            send_max: 100,
            receive_max: 100,
            operation_max: 200,
        }),
        extra: false,
        keep_alive: false,
        purpose: LowTcpPurpose::Transfer,
        paced: true,
        binding: grant.binding,
        route_tag: 9,
        kind: StreamKind::Clipboard as u16,
    };
    let accept = rig.core.accept_request(pool.clone(), auth, request);
    tokio::pin!(accept);
    assert!(timeout(Duration::from_millis(30), &mut accept).await.is_err());
    assert!(lock(&pool.state).pending.is_empty());
    rig.receivers[0].recv().await.unwrap();
    timeout(Duration::from_secs(1), accept).await.unwrap();
    assert!(lock(&pool.state).pending.contains_key(&[9; 16]));
    rig.core.stop().await;
}
#[tokio::test]
async fn old_route_cleanup_preserves_replacement_warm_binding() {
    let rig = Rig::new(1);
    let old = rig.grants[0].binding;
    let new = LowBinding {
        route_revision: RouteRevision(1),
        ..old
    };
    let mut state = PoolState {
        warm: WarmState::Opening,
        warm_binding: Some(new),
        ..Default::default()
    };
    state.cancel_scope(old);
    assert!(matches!(state.warm, WarmState::Opening));
    assert_eq!(state.warm_binding, Some(new));
    state.cancel_scope(new);
    assert!(matches!(state.warm, WarmState::Cold));
    rig.core.stop().await;
}

#[tokio::test]
async fn listener_info_change_reserves_refresh_before_resetting_direction() {
    let rig = Rig::new(1);
    let (pool, auth) = rig.fill(0).await;
    let generation = {
        let mut state = lock(&pool.state);
        state.advert = Some(Advert {
            paced: true,
            port: Some(9),
            failure: None,
        });
        state.direction_known = true;
        state.generation
    };
    let change = rig.core.receive_authorized(
        pool.clone(),
        auth,
        LowMsg::Info {
            paced: true,
            port: Some(10),
            failure: None,
        },
    );
    assert!(timeout(Duration::from_millis(30), change).await.is_err());
    {
        let state = lock(&pool.state);
        assert_eq!(state.generation, generation);
        assert!(state.direction_known);
        assert_eq!(state.advert.as_ref().unwrap().port, Some(9));
    }
    rig.core.stop().await;
}

#[tokio::test]
async fn stop_closes_listener_and_unfinished_tls_handshakes_before_returning() {
    use tokio::io::AsyncReadExt;
    let rig = Rig::new(1);
    rig.core.ensure_listener();
    let address = lock(&rig.core.listener).unwrap();
    let mut unfinished = tokio::net::TcpStream::connect(address).await.unwrap();
    rig.core.stop().await;
    assert_eq!(*rig.core.background.borrow(), 0);
    let mut byte = [0];
    let end = timeout(Duration::from_secs(1), unfinished.read(&mut byte))
        .await
        .unwrap();
    assert!(matches!(end, Ok(0) | Err(_)));
    drop(unfinished);
    let rebound = std::net::TcpListener::bind(address).unwrap();
    drop(rebound);
}

#[tokio::test]
async fn repeated_latest_info_and_mandatory_controls_both_make_progress() {
    let mut rig = Rig::new(1);
    rig.fill(0).await;
    let grant = rig.grants[0];
    let hop = rig.core.network.hop(grant).unwrap();
    let mut source = super::super::wire::PoolSource {
        receiver: rig.receivers.remove(0),
        advert: hop.advert.clone(),
        port: rig.core.network.clone(),
        prefer_info: true,
    };
    for _ in 0..4 {
        rig.core.publish_advert(grant).unwrap();
        let bytes = source.take().unwrap();
        assert!(
            matches!(crate::link::control_wire::decode(&bytes.bytes).unwrap(), crate::link::control::LinkMsg::Low { message, .. } if matches!(message.as_ref(), LowMsg::Info { .. }))
        );
        rig.core.publish_advert(grant).unwrap();
        let bytes = source.take().unwrap();
        assert!(
            matches!(crate::link::control_wire::decode(&bytes.bytes).unwrap(), crate::link::control::LinkMsg::Low { message, .. } if matches!(message.as_ref(), LowMsg::Refresh))
        );
    }
    rig.core.stop().await;
}

#[tokio::test]
async fn consuming_first_inbound_and_recovered_leases_publishes_without_relocking_state() {
    for outgoing in [false, true] {
        let rig = Rig::new(1);
        let grant = rig.grants[0];
        let (pool, auth) = rig.core.admit(grant, None).await.unwrap();
        let request = LowRequest {
            request: [7; 16],
            connection: [7; 16],
            options: Some(LowOptions {
                send_max: 100,
                receive_max: 100,
                operation_max: 200,
            }),
            extra: false,
            keep_alive: false,
            purpose: LowTcpPurpose::Transfer,
            paced: true,
            binding: grant.binding,
            route_tag: 7,
            kind: StreamKind::Diagnostics as u16,
        };
        lock(&pool.state).pending.insert(
            request.request,
            Pending {
                request: request.clone(),
                authorization: auth,
                outgoing,
                reverse: false,
                race: None,
                permit: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
                reply: None,
            },
        );
        if outgoing {
            let _ = rig.core.error(&pool, "test direction", "previously unavailable");
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_other, _) = listener.accept().unwrap();
        let core = rig.core.clone();
        let (done, completed) = tokio::sync::oneshot::channel();
        // A mutex deadlock blocks its OS thread, so the watchdog lives elsewhere.
        // This is a state/real-socket regression, not TLS/QUIC acceptance evidence.
        let worker = std::thread::spawn(move || {
            let result = core.consume(&pool, &request, outgoing, false, socket);
            let _ = done.send(result);
        });
        let (life, _, _) = timeout(Duration::from_secs(2), completed)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        assert!(
            rig.core
                .network
                .hub
                .status
                .load()
                .iter()
                .any(|s| s.connection == grant.binding.hop.connection_id && s.failure.is_none())
        );
        life.close();
        rig.core.stop().await;
    }
}
