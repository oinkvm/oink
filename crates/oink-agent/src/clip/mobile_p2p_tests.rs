//! Phone-to-phone transfers (protocol-v2 5.2) between two real phones
//! (MobileNetwork + MobileV2Session) and a desktop node of the same group,
//! over QUIC/TLS and Low TCP on loopback. Text is still broadcast to every
//! device; attachments directed to a phone reach that phone alone.
use super::*;
use crate::snapshot_tests::{Displays, common, start_with_clip};
use oink_mobile_core::{
    DeliveryFailure, DeliveryState, DownloadState, MessageRecord, MobileNetwork, MobileNetworkConfig,
    MobileNetworkLink, MobileProcess, MobileV2Identity, MobileV2Session, Peer, PeerKind, PeerState, ProxyChoice,
    SendItem, SendOrigin, SharePayload, TransferEvent, TransferStore,
};
use oink_network::{group_state::State, identity::Identity};
use oink_test_support::clipboard::FakeClipboard;
use std::path::PathBuf;

type EventLog = Arc<std::sync::Mutex<Vec<TransferEvent>>>;

struct Phone {
    net: MobileNetwork,
    session: Arc<MobileV2Session<MobileNetworkLink>>,
    events: EventLog,
    id: String,
    dir: PathBuf,
}

struct Group {
    _dir: tempfile::TempDir,
    desktop: crate::node::Node,
    clipboard: Arc<FakeClipboard>,
    a: Phone,
    b: Phone,
}

async fn phone(dir: PathBuf, seed: [u8; 32], nonce: u8, state: &State, os: &str) -> Phone {
    fs::create_dir_all(&dir).unwrap();
    let identity = Identity::from_seed(seed);
    let wire = MobileV2Identity::from_parts(identity.id().0, state.wire().group().unwrap().0, [nonce; 16]);
    let net = MobileNetwork::start(MobileNetworkConfig {
        identity_seed: seed,
        boot: wire.boot,
        bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
        group_state: Some(state.wire().encode(v2::limits::CONFIG_STATE).unwrap()),
        group_state_path: dir.join("group-state.v2"),
        pairing_journal_path: dir.join("pairing-recovery.cbor"),
        device_name: format!("phone {nonce}"),
        os: os.into(),
        form_factor: None,
        trusted_peers: Vec::new(),
        trust_path: None,
        process: MobileProcess::App,
    })
    .await
    .unwrap();
    let session = Arc::new(MobileV2Session::new(
        TransferStore::open(dir.join("history.sqlite"), dir.join("files")).unwrap(),
        wire,
        net.link.clone(),
    ));
    net.link.attach_clipboard(&session);
    session.set_history_foreground(true).unwrap();
    let events = EventLog::default();
    let sink = events.clone();
    session.set_event_sink(Arc::new(move |event| sink.lock().unwrap().push(event)));
    Phone {
        net,
        session,
        events,
        id: identity.id().to_hex(),
        dir,
    }
}

/// A desktop and two phones; each phone is linked to the desktop and to
/// the other phone.
async fn group() -> Group {
    let dir = tempfile::tempdir().unwrap();
    let pc_dir = dir.path().join("desktop");
    fs::create_dir_all(&pc_dir).unwrap();
    let pc = Identity::load_or_create(&pc_dir).unwrap();
    let (seed_a, seed_b) = ([51; 32], [52; 32]);
    let initial = State::create(&pc, "desktop", std::env::consts::OS, 1).unwrap();
    let roster = initial
        .roster()
        .add(&pc, Identity::from_seed(seed_a).id(), "phone a", "android", 2)
        .unwrap()
        .add(&pc, Identity::from_seed(seed_b).id(), "phone b", "ios", 3)
        .unwrap();
    let state = initial.with_roster(&roster).unwrap();
    {
        use crate::config_module::group_store::{self, StoredGroupState};
        group_store::save(
            &pc_dir.join(group_store::STATE_FILE),
            &StoredGroupState::new(pc.id(), state.wire().clone()).unwrap(),
        )
        .unwrap();
    }
    let clipboard = Arc::new(FakeClipboard::new());
    clipboard.copy(vec![ClipData::Text("desktop clipboard".into())], Sensitivity::Normal);
    let desktop = start_with_clip(
        &oink_test_support::network_config(pc_dir, "desktop", 0),
        &Displays::new("desktop"),
        Some(clipboard.clone()),
    )
    .await;
    let a = phone(dir.path().join("a"), seed_a, 1, &state, "android").await;
    let b = phone(dir.path().join("b"), seed_b, 2, &state, "ios").await;
    let desktop_addr = desktop.links().unwrap().local_addr();
    a.net.dial(desktop.id(), vec![desktop_addr]).unwrap();
    b.net.dial(desktop.id(), vec![desktop_addr]).unwrap();
    a.net
        .dial(Identity::from_seed(seed_b).id(), vec![b.net.local_addr()])
        .unwrap();
    let (id_a, id_b) = (Identity::from_seed(seed_a).id(), Identity::from_seed(seed_b).id());
    common::until("both phones admitted by the desktop and by each other", || {
        let admitted = |net: &MobileNetwork, peer| net.admitted_peers().unwrap().contains(&peer);
        admitted(&a.net, desktop.id())
            && admitted(&b.net, desktop.id())
            && admitted(&a.net, id_b)
            && admitted(&b.net, id_a)
    })
    .await;
    common::until("each phone's clipboard stream to every peer is open", || {
        let open = |net: &MobileNetwork, peer| {
            net.outbound_stream(peer, oink_protocol::frame::StreamKind::Clipboard)
                .is_some()
        };
        open(&a.net, desktop.id()) && open(&b.net, desktop.id()) && open(&a.net, id_b) && open(&b.net, id_a)
    })
    .await;
    let peer = |id: String, kind, order| Peer {
        id,
        kind,
        state: PeerState::Ready,
        accepts_clipboard: true,
        connection_order: order,
        relayed: false,
        clipboard_pending: false,
    };
    let desktop_id = desktop.id().to_hex();
    a.session.set_peers(vec![
        peer(desktop_id.clone(), PeerKind::Pc, 1),
        peer(b.id.clone(), PeerKind::Mobile, 2),
    ]);
    b.session.set_peers(vec![
        peer(desktop_id, PeerKind::Pc, 1),
        peer(a.id.clone(), PeerKind::Mobile, 2),
    ]);
    Group {
        _dir: dir,
        desktop,
        clipboard,
        a,
        b,
    }
}

impl Group {
    async fn stop(self) {
        self.a.net.stop().await;
        self.b.net.stop().await;
        self.desktop.shutdown().await;
    }
}

/// Takes the first logged event `pick` accepts; other events stay.
async fn next(events: &EventLog, what: &str, mut pick: impl FnMut(&TransferEvent) -> bool) -> TransferEvent {
    let mut found = None;
    common::until(what, || {
        let mut log = events.lock().unwrap();
        if let Some(index) = log.iter().position(&mut pick) {
            found = Some(log.remove(index));
        }
        found.is_some()
    })
    .await;
    found.unwrap()
}

/// The single delivery outcome of own share `operation`.
async fn delivery(phone: &Phone, operation: [u8; 16]) -> (Vec<i64>, DeliveryState, Option<DeliveryFailure>) {
    match next(
        &phone.events,
        "the share settled",
        |event| matches!(event, TransferEvent::DeliveryEvent { operation_id, .. } if *operation_id == operation),
    )
    .await
    {
        TransferEvent::DeliveryEvent {
            record_ids,
            delivery,
            failure,
            ..
        } => (record_ids, delivery, failure),
        _ => unreachable!(),
    }
}

/// The records of the receive that just started on `phone`.
async fn incoming(phone: &Phone) -> Vec<i64> {
    match next(&phone.events, "a phone-to-phone receive started", |event| {
        matches!(event, TransferEvent::DirectIncoming { .. })
    })
    .await
    {
        TransferEvent::DirectIncoming { record_ids, .. } => record_ids,
        _ => unreachable!(),
    }
}

async fn direct_ended(phone: &Phone) -> (Vec<i64>, Option<DeliveryFailure>) {
    match next(&phone.events, "the phone-to-phone receive ended", |event| {
        matches!(event, TransferEvent::DirectEnded { .. })
    })
    .await
    {
        TransferEvent::DirectEnded {
            record_ids, failure, ..
        } => (record_ids, failure),
        _ => unreachable!(),
    }
}

fn record(phone: &Phone, id: i64) -> Option<MessageRecord> {
    phone.session.with_store(|store| store.by_id(id)).unwrap()
}

fn to_phone(phone: &Phone) -> ProxyChoice {
    ProxyChoice::Direct {
        device: phone.id.clone(),
        name: "phone b".into(),
    }
}

fn share(operation: u8, item: SendItem) -> SharePayload {
    SharePayload {
        sensitivity: Sensitivity::Normal,
        items: vec![item],
        origin: SendOrigin::TransferPage,
        operation_id: [operation; 16],
        message_id: [operation + 1; 16],
    }
}

/// A file of `size` bytes the phone hands over (staged: moved, not copied).
fn file(phone: &Phone, name: &str, size: u64, pattern: bool) -> SendItem {
    let path = phone.dir.join(name);
    let file = fs::File::create(&path).unwrap();
    if pattern {
        let bytes: Vec<u8> = (0..size).map(|index| (index * 7 % 251) as u8).collect();
        fs::write(&path, bytes).unwrap();
    } else {
        // Sparse: a large transfer that stays in flight long enough.
        file.set_len(size).unwrap();
    }
    SendItem::File {
        path: path.to_string_lossy().into_owned(),
        name: name.into(),
        size,
        mime: Some("application/octet-stream".into()),
        staged: true,
    }
}

#[test]
fn text_is_still_broadcast_when_a_phone_is_chosen() {
    crate::test_watchdog::run(module_path!(), "p2p_text_broadcast", Some(8), async {
        let group = group().await;
        let (a, b) = (&group.a, &group.b);
        a.session
            .send_share(
                share(
                    1,
                    SendItem::Text {
                        text: "hello everyone".into(),
                    },
                ),
                to_phone(b),
            )
            .unwrap();
        let (_, state, failure) = delivery(a, [1; 16]).await;
        assert_eq!((state, failure), (DeliveryState::Sent, None));
        common::until("the desktop took the broadcast text", || {
            group
                .clipboard
                .items()
                .contains(&ClipData::Text("hello everyone".into()))
        })
        .await;
        // Phone b may also learn the desktop's clipboard (now this text)
        // from the desktop; the broadcast is the record phone a sent.
        let received = match next(&b.events, "phone b recorded the text from phone a", |event| {
            matches!(event, TransferEvent::Announcement { record_id, .. }
                if record(b, *record_id).is_some_and(|record| record.sender_id == a.id && record.proxy_id.is_none()))
        })
        .await
        {
            TransferEvent::Announcement { record_id, .. } => record(b, record_id).unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(received.text.as_deref(), Some("hello everyone"));
        assert_eq!(received.direct_peer, None);
        group.stop().await;
    });
}

#[test]
fn a_large_file_reaches_the_chosen_phone_alone() {
    crate::test_watchdog::run(module_path!(), "p2p_large_file", Some(8), async {
        let group = group().await;
        let (a, b) = (&group.a, &group.b);
        let size = 6 * 1024 * 1024 + 17;
        let sent = a
            .session
            .send_share(share(3, file(a, "report.bin", size, true)), to_phone(b))
            .unwrap();
        let received_ids = incoming(b).await;
        let (_, state, failure) = delivery(a, [3; 16]).await;
        assert_eq!((state, failure), (DeliveryState::Sent, None));
        let (ended, failure) = direct_ended(b).await;
        assert_eq!((ended.clone(), failure), (received_ids.clone(), None));

        let own = record(a, sent[0]).unwrap();
        assert_eq!(own.direct_peer.as_deref(), Some(b.id.as_str()));
        assert_eq!(own.direct_peer_name.as_deref(), Some("phone b"));
        let received = record(b, received_ids[0]).unwrap();
        assert_eq!(received.sender_id, a.id);
        assert_eq!(received.direct_peer.as_deref(), Some(a.id.as_str()));
        assert_eq!(received.download, DownloadState::Complete);
        assert_eq!(received.display_name, "report.bin");
        let body = fs::read(received.local_path.unwrap()).unwrap();
        let expected: Vec<u8> = (0..size).map(|index| (index * 7 % 251) as u8).collect();
        assert!(body == expected, "received body differs");
        // The desktop never saw it.
        assert_eq!(
            group.clipboard.items(),
            vec![ClipData::Text("desktop clipboard".into())]
        );
        group.stop().await;
    });
}

#[test]
fn a_folder_is_unpacked_on_the_receiving_phone() {
    crate::test_watchdog::run(module_path!(), "p2p_folder", Some(8), async {
        let group = group().await;
        let (a, b) = (&group.a, &group.b);
        let folder = a.dir.join("Holiday");
        fs::create_dir_all(folder.join("day 1/empty")).unwrap();
        fs::write(folder.join("notes.txt"), b"beach").unwrap();
        fs::write(folder.join("day 1/photo.raw"), vec![9u8; 300_000]).unwrap();
        a.session
            .send_share(
                share(
                    5,
                    SendItem::Folder {
                        path: folder.to_string_lossy().into_owned(),
                        name: "Holiday".into(),
                    },
                ),
                to_phone(b),
            )
            .unwrap();
        let (_, state, failure) = delivery(a, [5; 16]).await;
        assert_eq!((state, failure), (DeliveryState::Sent, None));
        let (ids, failure) = direct_ended(b).await;
        assert_eq!(failure, None);
        let received = record(b, ids[0]).unwrap();
        assert_eq!(received.download, DownloadState::Complete);
        assert_eq!(received.item_count, Some(2));
        let root = PathBuf::from(received.local_path.unwrap());
        assert_eq!(root.file_name().unwrap(), "Holiday");
        assert_eq!(fs::read(root.join("notes.txt")).unwrap(), b"beach");
        assert_eq!(fs::read(root.join("day 1/photo.raw")).unwrap(), vec![9u8; 300_000]);
        assert!(root.join("day 1/empty").is_dir());
        group.stop().await;
    });
}

#[test]
fn losing_the_receiving_phone_fails_the_transfer_as_disconnected() {
    crate::test_watchdog::run(module_path!(), "p2p_interrupted", Some(8), async {
        let group = group().await;
        let (a, b) = (&group.a, &group.b);
        let sent = a
            .session
            .send_share(share(7, file(a, "big.bin", 1 << 30, false)), to_phone(b))
            .unwrap();
        let ids = incoming(b).await;
        // The receiving phone goes away (its app closed) mid-transfer.
        b.net.stop().await;
        let (_, state, failure) = delivery(a, [7; 16]).await;
        assert_eq!(
            (state, failure),
            (DeliveryState::Failed, Some(DeliveryFailure::PeerDisconnected))
        );
        assert_eq!(
            record(a, sent[0]).unwrap().delivery_failure,
            Some(DeliveryFailure::PeerDisconnected)
        );
        let (_, failure) = direct_ended(b).await;
        assert_eq!(failure, Some(DeliveryFailure::PeerDisconnected));
        let received = record(b, ids[0]).unwrap();
        assert_eq!(received.download, DownloadState::Failed);
        assert_eq!(received.delivery_failure, Some(DeliveryFailure::PeerDisconnected));
        assert_eq!(received.local_path, None);
        group.stop().await;
    });
}

#[test]
fn a_sender_cancel_removes_the_message_on_both_phones() {
    crate::test_watchdog::run(module_path!(), "p2p_sender_cancel", Some(8), async {
        let group = group().await;
        let (a, b) = (&group.a, &group.b);
        let sent = a
            .session
            .send_share(share(9, file(a, "big.bin", 1 << 30, false)), to_phone(b))
            .unwrap();
        let ids = incoming(b).await;
        a.session.cancel_share([9; 16]).unwrap();
        let (_, state, failure) = delivery(a, [9; 16]).await;
        assert_eq!(
            (state, failure),
            (DeliveryState::Failed, Some(DeliveryFailure::Cancelled))
        );
        for (phone, records) in [(a, &sent), (b, &ids)] {
            let removed = next(&phone.events, "the records were removed", |event| {
                matches!(event, TransferEvent::Removed { .. })
            })
            .await;
            assert_eq!(
                removed,
                TransferEvent::Removed {
                    record_ids: records.clone()
                }
            );
            assert!(records.iter().all(|id| record(phone, *id).is_none()));
        }
        group.stop().await;
    });
}

#[test]
fn a_receiver_cancel_ends_both_sides_cancelled() {
    crate::test_watchdog::run(module_path!(), "p2p_receiver_cancel", Some(8), async {
        let group = group().await;
        let (a, b) = (&group.a, &group.b);
        let sent = a
            .session
            .send_share(share(11, file(a, "big.bin", 1 << 30, false)), to_phone(b))
            .unwrap();
        let ids = incoming(b).await;
        b.session.cancel_receive(ids[0]).unwrap();
        let (_, failure) = direct_ended(b).await;
        assert_eq!(failure, Some(DeliveryFailure::Cancelled));
        let (_, state, failure) = delivery(a, [11; 16]).await;
        assert_eq!(
            (state, failure),
            (DeliveryState::Failed, Some(DeliveryFailure::Cancelled))
        );
        // Both keep the message, shown as cancelled, not retried.
        assert_eq!(
            record(a, sent[0]).unwrap().delivery_failure,
            Some(DeliveryFailure::Cancelled)
        );
        let received = record(b, ids[0]).unwrap();
        assert_eq!(received.download, DownloadState::Failed);
        assert_eq!(received.delivery_failure, Some(DeliveryFailure::Cancelled));
        assert!(b.session.request_download(ids[0], false).is_err());
        group.stop().await;
    });
}
