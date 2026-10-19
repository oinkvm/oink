//! Actual QUIC streams and the actual input owner; all native effects use FakeInput.
use super::*;

use oink_foundation::{realtime::Tier, session::Boot};
use oink_network::{identity::Identity, link::*};
use oink_protocol::frame::Priority;
use tokio::time::timeout;

async fn node(seed: u8) -> (LinkLayer, ConfigPort, ModulePort) {
    let identity = Arc::new(Identity::from_seed([seed; 32]));
    let (network, config) = LinkLayer::start(LinkConfig {
        profile: crate::node::Node::offer_profile(),
        boot: Boot {
            device_id: identity.id(),
            nonce: [seed; 16],
        },
        identity,
        bind: "127.0.0.1:0".parse().unwrap(),
        tier: Tier::Elevated,
    })
    .await
    .unwrap();
    let module = network.register(&[StreamKind::Input]);
    (network, config, module)
}
async fn connected(port: &mut ConfigPort) -> Box<LinkConnected> {
    match timeout(Duration::from_secs(4), port.next()).await.unwrap().unwrap() {
        ConfigEvent::Connected(link) => link,
        other => panic!("expected connected, got {other:?}"),
    }
}
async fn inbound(port: &mut ModulePort) -> InboundStream {
    timeout(Duration::from_secs(4), async {
        loop {
            match port.next().await.unwrap() {
                ModuleEvent::Inbound(stream) => return stream,
                ModuleEvent::LinkReady { .. }
                | ModuleEvent::StreamClosed {
                    reason: StreamClosed::Local,
                    ..
                } => {}
                other => panic!("expected inbound, got {other:?}"),
            }
        }
    })
    .await
    .unwrap()
}
async fn attach(h: &mut Harness, remote: DeviceId) {
    timeout(Duration::from_secs(4), async {
        while !h.driver.adapter.attached(remote) {
            h.poll();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
fn outbound(h: &mut Harness, network: &LinkLayer, link: LinkId, peer: DeviceId, ordinal: u32) {
    let (input, motion) = network
        .open_stream(
            link,
            Priority::Interactive,
            tag(ordinal),
            QueueSpec::reliable(StreamKind::Input).0,
            Some(QueueSpec::datagram(StreamKind::Input)),
        )
        .unwrap();
    h.driver.command(InputCommand::Outbound { peer, input, motion });
}

async fn stall_clipboard(a: &LinkLayer, b: &LinkLayer, link: LinkId) -> (SendStream, RecvStream, ModulePort) {
    use oink_protocol::v1::{
        dto,
        schema::{WireDto, WireValue},
    };
    let mut port = b.register(&[StreamKind::Clipboard]);
    let (send, _) = a
        .open_stream(
            link,
            Priority::Normal,
            StreamTag {
                kind: StreamKind::Clipboard,
                ordinal: 1,
            },
            QueueSpec::reliable(StreamKind::Clipboard).0,
            None,
        )
        .unwrap();
    let offer = oink_protocol::v1::Message::ClipboardOffer(
        dto::ClipboardOffer::from_raw(
            [
                (
                    1,
                    oink_clipboard::wire::offer_id(oink_foundation::content::OfferId {
                        clip: oink_foundation::content::ClipId { origin: a.id(), seq: 1 },
                        generation: 1,
                    })
                    .write(),
                ),
                (2, 0u64.write()),
                (3, 1u64.write()),
                (4, dto::Sensitivity::Normal.write()),
                (5, oink_clipboard::wire::digest([0; 32]).write()),
                (
                    6,
                    oink_clipboard::wire::inventory(&oink_clipboard::model::Inventory {
                        id: oink_foundation::content::OfferId {
                            clip: oink_foundation::content::ClipId { origin: a.id(), seq: 1 },
                            generation: 1,
                        },
                        platform: oink_foundation::clipboard::ClipPlatform::Mac,
                        items: vec![],
                        roots: vec![],
                    })
                    .unwrap()
                    .write(),
                ),
                (7, 0u64.write()),
            ]
            .into(),
        )
        .unwrap(),
    );
    let offer = oink_protocol::v1::frame::encode(&offer, oink_protocol::v1::limits::RELIABLE_FRAME).unwrap();
    send.post(offer.as_slice()).unwrap();
    let (receive, _) = inbound(&mut port).await.accept(
        QueueSpec::reliable(StreamKind::Clipboard).1,
        QueueSpec::datagram(StreamKind::Clipboard),
    );
    timeout(Duration::from_secs(4), async {
        loop {
            loop {
                match send.post(offer.as_slice()) {
                    Ok(()) => {}
                    Err(PostError::Full) => break,
                    error => panic!("clipboard stalled unexpectedly: {error:?}"),
                }
            }
            let receive_full = b.stream_counters().iter().any(|counter| {
                counter.stream == receive.id()
                    && counter.depth == u32::from(QueueSpec::reliable(StreamKind::Clipboard).1.messages)
            });
            if receive_full {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (send, receive, port)
}

#[test]
fn quic_reader_resend_reopens_only_the_writer_and_reuses_the_retained_opening() {
    crate::test_watchdog::run(
        module_path!(),
        "quic_reader_resend_reopens_only_the_writer_and_reuses_the_retained_opening",
        None,
        quic_reader_resend_reopens_only_the_writer_and_reuses_the_retained_opening_scenario(),
    );
}

async fn quic_reader_resend_reopens_only_the_writer_and_reuses_the_retained_opening_scenario() {
    let (a, mut acfg, mut aport) = node(41).await;
    let (b, mut bcfg, mut bport) = node(42).await;
    a.dial(b.id(), vec![b.local_addr()], DialMode::DirectOnly).unwrap();
    let (ac, bc) = tokio::join!(connected(&mut acfg), connected(&mut bcfg));
    let agreement =
        oink_protocol::v1::admission::Agreement::new(crate::node::Node::offer_profile().capabilities).unwrap();
    a.set_agreement(ac.link, agreement.clone());
    b.set_agreement(bc.link, agreement.clone());
    a.set_ready(ac.link);
    b.set_ready(bc.link);
    timeout(Duration::from_secs(4), async {
        while !a.links().ready(b.id()) || !b.links().ready(a.id()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut h = Harness::with_boot(a.boot(), Arc::new(oink_test_support::input::FakeInput::new()));
    h.driver.command(InputCommand::PeerAdmission(wire::PeerAdmission {
        boot: b.boot(),
        link: ac.link,
        agreement: Arc::new(agreement),
    }));
    outbound(&mut h, &a, ac.link, b.id(), 1);
    let incoming = inbound(&mut bport).await;
    let (mut b_read, _) = incoming.accept(
        QueueSpec::reliable(StreamKind::Input).1,
        QueueSpec::datagram(StreamKind::Input),
    );
    for index in 0..5 {
        let bytes = timeout(Duration::from_secs(4), b_read.next()).await.unwrap().unwrap();
        assert!(opening_at(index, &decode(&bytes).unwrap()));
    }
    assert!(!h.driver.adapter.attached(b.id()));
    let (b_write, _motion) = b
        .open_stream(
            bc.link,
            Priority::Interactive,
            tag(1),
            QueueSpec::reliable(StreamKind::Input).0,
            Some(QueueSpec::datagram(StreamKind::Input)),
        )
        .unwrap();
    let remote = Adapter::new(Router::new(
        RouterConfig::new(b.boot(), "remote", Os::Windows),
        Arc::new(SimpleLayout::new()),
    ));
    opening(&b_write, &remote, true);
    let incoming = inbound(&mut aport).await;
    let (input, motion) = incoming.accept(
        QueueSpec::reliable(StreamKind::Input).1,
        QueueSpec::datagram(StreamKind::Input),
    );
    let retained = input.id();
    h.driver.command(InputCommand::Inbound {
        peer: b.id(),
        input,
        motion,
        peer_os: Os::Windows,
    });
    attach(&mut h, b.id()).await;
    // This accepted B0 has a full receive queue and no ordinary consumer. Input
    // detach/reopen/reattach must progress on the same real QUIC connection.
    let (_blocked_send, blocked_receive, _blocked_port) = stall_clipboard(&a, &b, ac.link).await;
    b.close_stream(b_read.id(), StreamCode::Resend);
    let closed = timeout(Duration::from_secs(4), async {
        loop {
            if let ModuleEvent::StreamClosed { stream, reason } = aport.next().await.unwrap() {
                assert_eq!(reason, StreamClosed::Peer(StreamCode::Resend));
                break stream;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(closed.tag.ordinal, 1);
    h.driver.command(InputCommand::OutboundGone { peer: b.id() });
    assert!(!h.driver.adapter.attached(b.id()));
    outbound(&mut h, &a, ac.link, b.id(), 2);
    let incoming = inbound(&mut bport).await;
    assert_eq!(incoming.tag.ordinal, 2);
    let (_b_read, _motion) = incoming.accept(
        QueueSpec::reliable(StreamKind::Input).1,
        QueueSpec::datagram(StreamKind::Input),
    );
    attach(&mut h, b.id()).await;
    assert_eq!(h.driver.pairs[&b.id()].inbound.as_ref().unwrap().0.id(), retained);
    assert_eq!(b_write.id().tag.ordinal, 1);
    assert!(h.driver.shared.status.load().peers[0].online);
    assert!(b.stream_counters().iter().any(|counter| {
        counter.stream == blocked_receive.id()
            && counter.depth == u32::from(QueueSpec::reliable(StreamKind::Clipboard).1.messages)
    }));
    drop(h);
    tokio::join!(a.stop(), b.stop());
}
