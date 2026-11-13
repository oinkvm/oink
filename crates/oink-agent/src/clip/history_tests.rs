use super::*;
use oink_foundation::{
    GroupId,
    clipboard::Sensitivity,
    content::{ClipId, OfferId},
};

fn record(author: u8, seq: u64) -> h::ClipRecord {
    let origin = DeviceId([author; 32]);
    h::ClipRecord {
        id: OfferId {
            clip: ClipId { origin, seq },
            generation: seq,
        },
        clock: seq,
        time_ms: seq as i64,
        origin,
        proxy: None,
        sensitivity: Sensitivity::Normal,
        items: vec![h::HistoryItem {
            id: 0,
            kind: h::ItemKind::Text,
            mime: "text/plain".into(),
            name: None,
            bytes: Some(1),
            dimensions: None,
            child_count: None,
            children_bytes: None,
            file_node: None,
            file_version: None,
        }],
        formats: vec![h::HistoryFormat {
            item: 0,
            representation: 0,
            name: "mime:text/plain;charset=utf-8".into(),
            portable: Some(h::PortableFormat::Text),
            bytes: Some(1),
            file_node: None,
        }],
        digests: vec![],
        inline: vec![h::InlinePayload {
            item: 0,
            representation: 0,
            format: h::PortableFormat::Text,
            bytes: vec![b'a'],
        }],
        preview: None,
        rev: 0,
    }
}
struct Peer {
    _root: tempfile::TempDir,
    history: History,
    context: Context,
}
fn peer(id: u8) -> Peer {
    let root = tempfile::tempdir().unwrap();
    let group = GroupId([1; 16]);
    let me = DeviceId([id; 32]);
    let store = oink_clipboard::history::Store::load(
        root.path(),
        &root.path().join("cache"),
        group,
        me,
        oink_clipboard::policy::SharingPolicy::new(true),
    )
    .unwrap();
    let context = Context {
        me,
        group,
        members: (1..=4).map(|n| DeviceId([n; 32])).collect(),
        phones: [DeviceId([4; 32])].into(),
        neighbours: (1..=4)
            .filter(|n| *n != id)
            .map(|n| h::HistoryNeighbour {
                id: DeviceId([n; 32]),
                direct: true,
                capable: true,
                subscribed: false,
                phone: n == 4,
                connected_to_author: false,
            })
            .collect(),
    };
    Peer {
        _root: root,
        history: History::new(store),
        context,
    }
}
fn pump(peers: &mut [Peer]) -> Vec<Message> {
    let mut phone = vec![];
    for _ in 0..200 {
        let messages: Vec<_> = peers
            .iter_mut()
            .flat_map(|p| {
                let sender = p.context.me;
                std::mem::take(&mut p.history.pending)
                    .into_iter()
                    .flat_map(move |(to, messages)| messages.into_iter().map(move |m| (sender, to, m)))
            })
            .collect();
        if messages.is_empty() {
            return phone;
        }
        for (from, to, message) in messages {
            if to == DeviceId([4; 32]) {
                phone.push(message);
                continue;
            }
            let p = peers.iter_mut().find(|p| p.context.me == to).unwrap();
            p.history.receive_in(&p.context, from, message).unwrap();
        }
    }
    panic!("history flood failed to quiesce")
}
#[test]
fn three_pcs_and_foreground_phone_converge_without_reflooding_duplicates() {
    let mut peers = vec![peer(1), peer(2), peer(3)];
    let phone = DeviceId([4; 32]);
    let summary = dto::HistorySummary::from_window(GroupId([1; 16]), &h::HistoryWindow::new(), true).unwrap();
    {
        let p = &mut peers[1];
        p.history
            .receive_in(&p.context, phone, Message::HistorySummary(summary))
            .unwrap();
    }
    let mut r = record(1, 1);
    r.origin = phone;
    r.proxy = Some(r.id.clip.origin);
    peers[0].history.store.merge(vec![r.clone()], vec![]).unwrap();
    let p = &mut peers[0];
    p.history
        .forward_in(&p.context, h::HistoryChange::Record(r.clone()), None);
    let phone_messages = pump(&mut peers);
    assert_eq!(
        phone_messages
            .iter()
            .filter(|m| matches!(m, Message::HistoryDelta(_)))
            .count(),
        1
    );
    let digest = peers[0].history.store.window().digest();
    assert!(peers.iter().all(|p| p.history.store.window().digest() == digest));
    for p in &mut peers {
        p.history
            .forward_in(&p.context, h::HistoryChange::Record(r.clone()), Some(DeviceId([2; 32])));
    }
    assert!(pump(&mut peers).is_empty());
    let own = peers[1]
        .history
        .store
        .holder(r.id, h::HolderState::Holding, h::HolderScope::TEXT)
        .unwrap()
        .unwrap();
    {
        let p = &mut peers[1];
        p.history
            .forward_in(&p.context, h::HistoryChange::Holder(own.clone()), None);
    }
    pump(&mut peers);
    assert!(peers.iter().all(|p| p.history.store.window().holders().contains(&own)));
    // A phone cannot author somebody else's holder state.
    let forged = h::HolderEntry {
        rev: own.rev + 1,
        state: h::HolderState::Gone,
        ..own.clone()
    };
    let forged = dto::HistoryDelta::from_domain(&h::HistoryChange::Holder(forged), digest).unwrap();
    {
        let p = &mut peers[1];
        assert!(
            p.history
                .receive_in(&p.context, phone, Message::HistoryDelta(forged))
                .is_err()
        );
    }
    assert!(peers[1].history.store.window().holders().contains(&own));
}
#[test]
fn cache_budget_release_converges_with_three_pcs_and_phone_subscription() {
    let mut peers = vec![peer(1), peer(2), peer(3)];
    let phone = DeviceId([4; 32]);
    let p = &mut peers[0];
    p.history
        .receive_in(
            &p.context,
            phone,
            Message::HistorySummary(
                dto::HistorySummary::from_window(p.context.group, &h::HistoryWindow::new(), true).unwrap(),
            ),
        )
        .unwrap();
    for seq in 1..=12 {
        let mut r = record(1, seq);
        let size = match seq {
            1 => 100,
            12 => 300,
            _ => 10,
        };
        r.inline[0].bytes = vec![b'x'; size];
        r.formats[0].bytes = Some(size as u64);
        r.items[0].bytes = Some(size as u64);
        let p = &mut peers[0];
        p.history.store.merge(vec![r.clone()], vec![]).unwrap();
        p.history.store.ingest_inline(&r).unwrap();
        p.history
            .forward_in(&p.context, h::HistoryChange::Record(r.clone()), None);
        let holder = p
            .history
            .store
            .holder(r.id, h::HolderState::Holding, h::HolderScope::TEXT)
            .unwrap()
            .unwrap();
        p.history.forward_in(&p.context, h::HistoryChange::Holder(holder), None);
    }
    pump(&mut peers);
    let p = &mut peers[0];
    let total: u64 = (1..=12).map(|seq| p.history.store.bytes.bytes(record(1, seq).id)).sum();
    let cap = total - p.history.store.bytes.bytes(record(1, 1).id);
    let released = p.history.store.enforce_cap(cap).unwrap();
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].record_id, record(1, 1).id);
    assert_eq!(released[0].state, h::HolderState::Released);
    assert_eq!(released[0].rev, 2);
    assert!(
        p.history.store.bytes.bytes(record(1, 12).id) > 0,
        "newest ten are protected even if larger"
    );
    p.history
        .forward_in(&p.context, h::HistoryChange::Holder(released[0].clone()), None);
    let phone_messages = pump(&mut peers);
    assert!(
        phone_messages
            .iter()
            .any(|message| matches!(message, Message::HistoryDelta(delta) if
        delta.to_domain().is_ok_and(|change| change == h::HistoryChange::Holder(released[0].clone()))))
    );
    let digest = peers[0].history.store.window().digest();
    for p in peers {
        assert_eq!(p.history.store.window().records().len(), 12);
        assert_eq!(p.history.store.window().digest(), digest);
        assert!(p.history.store.window().holders().contains(&released[0]));
    }
}

#[test]
fn partition_reconcile_eviction_and_phone_exclusive_pages() {
    let mut peers = vec![peer(1), peer(2), peer(3)];
    peers[0]
        .history
        .store
        .merge((1..=70).map(|s| record(1, s)).collect(), vec![])
        .unwrap();
    peers[1]
        .history
        .store
        .merge((71..=140).map(|s| record(2, s)).collect(), vec![])
        .unwrap();
    for (to, from) in [(0, 1), (1, 0), (2, 0)] {
        let summary = peers[from].history.summary(GroupId([1; 16]));
        let sender = peers[from].context.me;
        let p = &mut peers[to];
        p.history
            .receive_in(&p.context, sender, Message::HistorySummary(summary))
            .unwrap();
    }
    pump(&mut peers);
    let window = peers[0].history.store.window();
    assert_eq!(window.records().len(), 100);
    assert!(
        peers
            .iter()
            .all(|p| p.history.store.window().digest() == window.digest())
    );
    assert!(window.get(record(1, 1).id).is_none());
    let old = record(1, 1);
    let p = &mut peers[0];
    p.history.forward_in(&p.context, h::HistoryChange::Record(old), None);
    assert!(p.history.pending.is_empty());
    let phone = DeviceId([4; 32]);
    {
        let p = &mut peers[0];
        let summary = dto::HistorySummary::from_window(p.context.group, &h::HistoryWindow::new(), true).unwrap();
        p.history
            .receive_in(&p.context, phone, Message::HistorySummary(summary))
            .unwrap();
        p.history.pending.remove(&phone);
    }
    let mut before = None;
    let mut ids = vec![];
    loop {
        let p = &mut peers[0];
        let request = dto::HistoryPull::from_query([7; 16], dto::HistoryQuery::Page { before, max: 20 }, true).unwrap();
        p.history
            .receive_in(&p.context, phone, Message::HistoryPull(request))
            .unwrap();
        let messages = p.history.pending.remove(&phone).unwrap();
        let Message::HistoryRecords(reply) = &messages[0] else {
            panic!("records reply")
        };
        let page = reply.to_page().unwrap();
        assert_eq!(page.records.len(), 20);
        assert_eq!(reply.digest().unwrap().0, window.digest());
        ids.extend(page.records.iter().map(|r| r.id));
        if !page.more {
            break;
        }
        before = page.next;
    }
    assert_eq!(ids.len(), 100);
    assert_eq!(ids.into_iter().collect::<HashSet<_>>().len(), 100);
}

#[test]
fn wire_actors_cache_copies_and_read_another_holder_after_origin_shutdown() {
    use crate::snapshot_tests::{Displays, common, start_with_clip};
    use oink_clipboard::{ClipData, content::Provider, model::*, sync::files::BulkLink};
    use oink_test_support::clipboard::FakeClipboard;
    crate::test_watchdog::run(module_path!(), "history_holder_failover", Some(20), async {
        let root = tempfile::tempdir().unwrap();
        let ca = Arc::new(FakeClipboard::new());
        let a = start_with_clip(
            &oink_test_support::network_config(root.path().join("a"), "a", 0),
            &Displays::new("a"),
            Some(ca.clone()),
        )
        .await;
        let b = start_with_clip(
            &oink_test_support::network_config(root.path().join("b"), "b", 0),
            &Displays::new("b"),
            Some(Arc::new(FakeClipboard::new())),
        )
        .await;
        let c = start_with_clip(
            &oink_test_support::network_config(root.path().join("c"), "c", 0),
            &Displays::new("c"),
            Some(Arc::new(FakeClipboard::new())),
        )
        .await;
        common::pair(&a, &b).await;
        common::pair(&a, &c).await;
        common::active(&[&a, &b, &c]).await;
        ca.copy(vec![ClipData::Text("history holder data".into())], Sensitivity::Normal);
        common::until("all PCs keep a record and a second holder", || {
            let stores = [
                a.clipboard().unwrap().shared.history.clone(),
                b.clipboard().unwrap().shared.history.clone(),
                c.clipboard().unwrap().shared.history.clone(),
            ];
            stores.iter().all(|s| {
                s.window().records().iter().any(|r| r.id.clip.origin == a.id())
                    && s.window()
                        .holders()
                        .iter()
                        .any(|h| h.holder == b.id() && h.state == h::HolderState::Holding)
            })
        })
        .await;
        let store = b.clipboard().unwrap().shared.history.clone();
        let record = store
            .window()
            .records()
            .iter()
            .find(|r| r.id.clip.origin == a.id())
            .unwrap()
            .clone();
        let inventory = store.provider(record.id, None).unwrap().inventory();
        let rep = inventory.items[0].representations[0].id;
        ca.copy(
            vec![ClipData::Text("temporary sensitive head".into())],
            Sensitivity::Concealed,
        );
        common::until("sensitive inline head reaches all PC caches", || {
            [&a, &b, &c].iter().all(|node| {
                let store = &node.clipboard().unwrap().shared.history;
                store
                    .window()
                    .records()
                    .first()
                    .is_some_and(|r| r.sensitive() && !r.inline.is_empty() && store.bytes.bytes(r.id) > 0)
            })
        })
        .await;
        let sensitive_id = store.window().records()[0].id;
        ca.copy(vec![ClipData::Text("after sensitive head".into())], Sensitivity::Normal);
        common::until("superseded sensitive data and native source retire on all PCs", || {
            [&a, &b, &c].iter().all(|node| {
                let clip = node.clipboard().unwrap();
                clip.shared.history.window().get(sensitive_id).is_none()
                    && clip.shared.history.bytes.bytes(sensitive_id) == 0
                    && clip
                        .shared
                        .native_target
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_none_or(|(id, _)| *id != sensitive_id.clip)
            })
        })
        .await;
        let cclip = c.clipboard().unwrap();
        let files = cclip.shared.links.clone();
        let origin = files.clone().bind(a.id(), record.id.clip);
        let link = Arc::new(crate::clip::history_link::Link {
            files,
            network: c.links().unwrap(),
            store: cclip.shared.history.clone(),
            offer: record.id,
            origin,
            unavailable: Default::default(),
        });
        a.shutdown().await;
        common::until("origin offline, holder still directly ready", || {
            !c.links().unwrap().links().ready(a.id()) && c.links().unwrap().links().ready(b.id())
        })
        .await;
        let remote = content::Remote::on_runtime(
            c.cache_dir().as_path(),
            oink_clipboard::store::BatchId::random().unwrap(),
            inventory,
            oink_foundation::clipboard::ClipPlatform::Mac,
            link,
            Demand::Clipboard,
            tokio::runtime::Handle::current(),
        )
        .unwrap();
        let bytes = tokio::task::spawn_blocking(move || content::read_all(&mut remote.open(rep).unwrap()).unwrap())
            .await
            .unwrap();
        assert_eq!(bytes, b"history holder data");
        b.shutdown().await;
        c.shutdown().await;
    });
}

#[test]
fn phone_incomplete_record_is_rejected_and_pause_keeps_foreground_subscription() {
    let mut pc = peer(1);
    let phone = DeviceId([4; 32]);
    let summary = dto::HistorySummary::from_window(pc.context.group, &h::HistoryWindow::new(), true).unwrap();
    pc.history
        .receive_in(&pc.context, phone, Message::HistorySummary(summary))
        .unwrap();
    pc.history.pending.clear();
    pc.history.suspend();
    assert!(pc.history.subscriptions.contains(&phone));
    assert!(pc.history.pending.is_empty());
    let mut incomplete = record(4, 1);
    incomplete.inline.clear();
    let delta = dto::HistoryDelta::from_domain(&h::HistoryChange::Record(incomplete.clone()), [0; 32]).unwrap();
    assert!(
        pc.history
            .receive_in(&pc.context, phone, Message::HistoryDelta(delta))
            .is_err()
    );
    assert!(pc.history.store.window().get(incomplete.id).is_none());
    assert!(pc.history.pending.is_empty());
    let unknown = h::HolderEntry {
        record_id: record(4, 99).id,
        holder: DeviceId([3; 32]),
        rev: 1,
        state: h::HolderState::Holding,
        scope: h::HolderScope::TEXT,
    };
    let unknown = dto::HistoryDelta::from_domain(&h::HistoryChange::Holder(unknown), [0; 32]).unwrap();
    pc.history
        .receive_in(&pc.context, DeviceId([3; 32]), Message::HistoryDelta(unknown))
        .unwrap();
    assert!(pc.history.store.window().holders().is_empty());
    assert!(
        pc.history
            .pending
            .values()
            .flatten()
            .all(|m| matches!(m, Message::HistoryIndexRequest(_)))
    );
    let complete = record(4, 2);
    let delta = dto::HistoryDelta::from_domain(&h::HistoryChange::Record(complete.clone()), [0; 32]).unwrap();
    pc.history
        .receive_in(&pc.context, phone, Message::HistoryDelta(delta))
        .unwrap();
    assert_eq!(pc.history.store.scope(complete.id), h::HolderScope::TEXT);
    assert!(phone_offer(&complete).is_some());
    let phone_holder = h::HolderEntry {
        record_id: complete.id,
        holder: phone,
        rev: 1,
        state: h::HolderState::Holding,
        scope: h::HolderScope::TEXT,
    };
    let delta = dto::HistoryDelta::from_domain(&h::HistoryChange::Holder(phone_holder), [0; 32]).unwrap();
    assert!(
        pc.history
            .receive_in(&pc.context, phone, Message::HistoryDelta(delta))
            .is_err()
    );
    assert!(pc.history.store.window().holders().is_empty());
}

#[test]
fn oversized_low_history_sender_waits_for_peer_eof_and_keeps_effect_valid() {
    use crate::streams::{StreamModule, clipboard::ClipboardStreams, lifecycle_tests::Fixture};
    use oink_network::link::ModuleEvent;
    crate::test_watchdog::run(module_path!(), "history_low_half_close", Some(6), async {
        let mut fixture = Fixture::<ClipboardStreams>::start().await;
        let (source, config) = fixture.context(0);
        let (target, _) = fixture.context(1);
        let peer = target.id();
        let link = source.links().by_peer(peer).unwrap().link;
        let mut owner = ClipboardStreams::new(source.clone(), config);
        owner.link_ready(link, peer);
        let mut r = record(1, 1);
        r.origin = source.id();
        r.id.clip.origin = source.id();
        r.inline[0].bytes = vec![b'x'; 32 * 1024];
        r.formats[0].bytes = Some(32 * 1024);
        let id = r.id;
        let message =
            Message::HistoryDelta(dto::HistoryDelta::from_domain(&h::HistoryChange::Record(r), [0; 32]).unwrap());
        assert!(oink_protocol::v2::frame::encode(&message, oink_protocol::v2::limits::RELIABLE_FRAME).is_err());
        owner.send_history(peer, &message).unwrap();
        let mut received = loop {
            if let Some(ModuleEvent::InboundLow(operation)) = fixture.port(1).next().await {
                break *operation;
            }
        };
        assert_eq!(received.first, message);
        assert!(
            received.stream.recv().await.unwrap().is_none(),
            "sender half-close arrives first"
        );
        // An early drop finishes the sender task here and revokes this effect.
        // The production future must remain alive until we send our own EOF.
        assert_eq!(owner.history_operations_for_test(peer), 1);
        let holder = h::HolderEntry {
            record_id: id,
            holder: source.id(),
            rev: 1,
            state: h::HolderState::Holding,
            scope: h::HolderScope::TEXT,
        };
        let holder =
            Message::HistoryDelta(dto::HistoryDelta::from_domain(&h::HistoryChange::Holder(holder), [0; 32]).unwrap());
        assert!(
            matches!(
                owner.send_history(peer, &holder),
                Err(oink_network::link::PostError::Full)
            ),
            "Reliable holder waits behind Low record"
        );
        let valid = received.stream.effect_validity();
        assert!(valid());
        received.stream.finish();
        crate::snapshot_tests::common::until("history sender consumed peer EOF", || {
            owner.history_operations_for_test(peer) == 0
        })
        .await;
        assert!(valid(), "orderly two-sided end retains the received effect");
        drop(owner);
        fixture.stop().await;
    });
}

#[test]
fn concealed_inline_heads_are_purged_on_all_pcs_and_never_resurrected() {
    let mut peers = vec![peer(1), peer(2), peer(3)];
    let phone = DeviceId([4; 32]);
    let mut secret = record(4, 2);
    secret.sensitivity = Sensitivity::Concealed;
    secret.redact();
    let delta = dto::HistoryDelta::from_domain(&h::HistoryChange::Record(secret.clone()), [0; 32]).unwrap();
    {
        let p = &mut peers[0];
        p.history
            .receive_in(&p.context, phone, Message::HistoryDelta(delta.clone()))
            .unwrap();
    }
    pump(&mut peers);
    for p in &peers {
        assert!(p.history.store.window().get(secret.id).is_some());
        assert!(p.history.store.bytes.bytes(secret.id) > 0);
    }
    let normal = record(2, 3);
    {
        let p = &mut peers[1];
        p.history.store.merge(vec![normal.clone()], vec![]).unwrap();
        p.history.forward_in(&p.context, h::HistoryChange::Record(normal), None);
    }
    pump(&mut peers);
    let digest = peers[0].history.store.window().digest();
    for p in &mut peers {
        assert!(p.history.store.window().get(secret.id).is_none());
        assert_eq!(p.history.store.bytes.bytes(secret.id), 0);
        assert_eq!(p.history.store.window().digest(), digest);
        p.history
            .receive_in(&p.context, phone, Message::HistoryDelta(delta.clone()))
            .unwrap();
        assert!(p.history.store.window().get(secret.id).is_none());
        assert_eq!(p.history.store.bytes.bytes(secret.id), 0);
    }
}

#[test]
fn queued_pages_and_deltas_drop_superseded_sensitive_bodies() {
    let mut p = peer(1);
    let mut secret = record(1, 1);
    secret.sensitivity = Sensitivity::Concealed;
    p.history.store.merge(vec![secret.clone()], vec![]).unwrap();
    p.history
        .forward_in(&p.context, h::HistoryChange::Record(secret.clone()), None);
    let request =
        dto::HistoryPull::from_query([7; 16], dto::HistoryQuery::Page { before: None, max: 20 }, true).unwrap();
    p.history
        .receive_in(&p.context, DeviceId([2; 32]), Message::HistoryPull(request))
        .unwrap();
    p.history.store.merge(vec![record(1, 2)], vec![]).unwrap();
    p.history.prune_pending();
    for message in p.history.pending.values().flatten() {
        match message {
            Message::HistoryDelta(delta) => assert_ne!(delta.to_domain().unwrap().record_id(), secret.id),
            Message::HistoryRecords(records) => assert!(records.to_page().unwrap().records.is_empty()),
            _ => {}
        }
    }
}

#[test]
fn windows_file_selection_cards_keep_real_roots_and_versions() {
    use oink_clipboard::model::*;
    use oink_foundation::clipboard::ClipPlatform;
    let pc = peer(1);
    let me = pc.context.me;
    let root_a = u64::MAX - 3;
    let root_b = u64::MAX - 2;
    let inventory = Inventory {
        id: OfferId {
            clip: ClipId { origin: me, seq: 1 },
            generation: 1,
        },
        platform: ClipPlatform::Windows,
        items: vec![ContentItem {
            id: ItemId(0),
            file: Some(NodeId(0)),
            virtual_files: false,
            representations: vec![Representation {
                id: RepresentationId(0),
                format: NativeFormat::WindowsPredefined(15),
                medium: Medium::HGlobal,
                aspect: 1,
                index: -1,
                standard: None,
                data: RepresentationData::File(NodeId(0)),
            }],
        }],
        roots: vec![
            FileNode {
                id: NodeId(root_a),
                parent: None,
                name: "file.bin".into(),
                kind: NodeKind::File,
                size: ContentSize::Known(10),
                version: 41,
                modified_ms: None,
                exec_bits: None,
            },
            FileNode {
                id: NodeId(root_b),
                parent: None,
                name: "folder".into(),
                kind: NodeKind::Directory,
                size: ContentSize::Unknown,
                version: 42,
                modified_ms: None,
                exec_bits: None,
            },
        ],
    };
    let offer = oink_clipboard::sync::Ordering::new(me, 0)
        .capture(
            oink_clipboard::wire::inventory(&inventory).unwrap(),
            vec![],
            1,
            Sensitivity::Normal,
            0,
        )
        .unwrap()
        .unwrap();
    let r = super::record(&offer, None, None, &pc.history.store).unwrap();
    assert_eq!(r.items.len(), 2);
    assert_eq!(
        (r.items[0].file_node, r.items[0].file_version),
        (Some(root_a), Some(41))
    );
    assert_eq!(
        (r.items[1].file_node, r.items[1].file_version),
        (Some(root_b), Some(42))
    );
    assert_eq!(r.formats[0].file_node, Some(0));
}
