//! The native-loop owner with real bounded stream queues and a fake
//! injector, as in the sibling modules. No hook, tap or platform input call.
use super::*;

fn id(n: u8) -> DeviceId {
    DeviceId([n; 32])
}
fn remote_of(peer: DeviceId) -> Adapter {
    Adapter::new(Router::new(
        RouterConfig::new(fixture_boot(peer), "peer", Os::Windows),
        Arc::new(SimpleLayout::new()),
    ))
}
struct PeerEnds {
    read: oink_network::link::RecvStream,
    _datagrams: oink_network::link::DatagramRecv,
    _write: oink_network::link::SendStream,
    _motion: oink_network::link::DatagramSend,
}
/// Both directions of the input pair with `peer`, attached.
fn attach_peer(h: &mut Harness, peer: DeviceId, link: u64) -> PeerEnds {
    let (input, read) = reliable_pair(LinkId(link), tag(1), QueueSpec::reliable(StreamKind::Input).0);
    let (motion, datagrams) = datagram_pair(LinkId(link), tag(1), QueueSpec::datagram(StreamKind::Input));
    h.driver
        .command(InputCommand::PeerAdmission(fixture_admission(peer, LinkId(link))));
    h.driver.command(InputCommand::Outbound {
        peer,
        input,
        motion: Some(motion),
    });
    let (write, input) = reliable_pair(LinkId(link), tag(1), QueueSpec::reliable(StreamKind::Input).1);
    let (motion, read_motion) = datagram_pair(LinkId(link), tag(1), QueueSpec::datagram(StreamKind::Input));
    h.driver.command(InputCommand::Inbound {
        peer,
        input,
        motion: Some(read_motion),
        peer_os: Os::Windows,
    });
    opening(&write, &remote_of(peer), true);
    let until = Instant::now() + Duration::from_secs(2);
    while !h.driver.adapter.attached(peer) && Instant::now() < until {
        h.poll();
        std::thread::yield_now();
    }
    assert!(h.driver.adapter.attached(peer));
    PeerEnds {
        read,
        _datagrams: datagrams,
        _write: write,
        _motion: motion,
    }
}

/// A failed stream must not affect other streams. With three devices, the input
/// stream to one peer has just been closed by the network thread (its link dropped; the
/// "stream gone" command has not reached the input thread yet). The router then emits one
/// batch that addresses both peers. The post to the closed stream fails, that peer is
/// detached, and the rest of the batch is discarded, including what was addressed to the
/// healthy peer.
#[test]
fn failed_post_to_one_peer_does_not_drop_output_for_another_peer() {
    for closed in [2u8, 3] {
        let healthy = if closed == 2 { 3 } else { 2 };
        let mut h = Harness::new();
        let mut ends = std::collections::HashMap::new();
        ends.insert(2u8, attach_peer(&mut h, id(2), 1));
        ends.insert(3u8, attach_peer(&mut h, id(3), 2));
        for end in ends.values_mut() {
            drain(&mut end.read);
        }
        // The link to `closed` is lost: the network thread closes the queue.
        drop(ends.remove(&closed));
        let mut feel = oink_foundation::input::FeelProfile::default();
        feel.key_repeat_delay_ms += 125;
        // One router batch for both attached peers (the changed local feel is broadcast).
        h.driver.command(InputCommand::LocalFeel(feel.clone()));
        assert!(!h.driver.adapter.attached(id(closed)), "the broken peer is detached");
        assert!(
            h.driver.adapter.attached(id(healthy)),
            "the healthy peer stays attached"
        );
        let sent = drain(&mut ends.get_mut(&healthy).unwrap().read);
        assert!(
            sent.iter()
                .any(|m| matches!(m, Message::Control(ControlMsg::Feel { profile }) if *profile == feel)),
            "peer {healthy} never receives the new feel after the stream to peer {closed} failed in the same batch; it got {sent:?}"
        );
    }
}

/// A realtime thread never waits for ordinary threads or allocates unboundedly.
/// Threads communicate through queues, snapshots or small commands. The input thread hands
/// `MainCommand`s to one ordinary thread (`oink-main-commands`), which also performs
/// drag-and-drop preparation, media keys and the system activity declaration. While that
/// thread is busy the hand-over must stay bounded.
#[test]
fn main_command_backlog_is_bounded_while_its_ordinary_consumer_is_stalled() {
    let mut h = Harness::new();
    // Nobody drains `h._main`: the ordinary consumer is stalled.
    let focus = h.driver.adapter.router().focus();
    for _ in 0..50_000 {
        // What the platform observer sends 20 times a second, forever.
        h.driver.command(InputCommand::PointerSample {
            application: None,
            scope: Some((focus.epoch, focus.holder)),
            sample: None,
            hidden: false,
        });
    }
    let backlog = h._main.try_iter().count();
    assert!(
        backlog <= 8192,
        "{backlog} commands queued for the stalled ordinary thread by 50000 identical pointer samples"
    );
}
