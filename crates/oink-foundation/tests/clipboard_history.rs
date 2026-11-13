//! Deterministic multi-device simulation: only domain state is simulated. These
//! tests make no claim about native clipboards, device networking or UI behavior.
use oink_foundation::{
    DeviceId,
    clipboard::Sensitivity,
    clipboard_dedupe::{self as d, Identity, Rules, Source},
    clipboard_history::*,
    content::{ClipId, OfferId},
};
fn device(n: u8) -> DeviceId {
    DeviceId([n; 32])
}
fn record(origin: u8, seq: u64, clock: u64) -> ClipRecord {
    let fp = d::Fingerprint::text(&format!("copy {seq}"));
    ClipRecord {
        id: OfferId {
            clip: ClipId {
                origin: device(origin),
                seq,
            },
            generation: 0,
        },
        clock,
        time_ms: clock as i64,
        origin: device(origin),
        proxy: None,
        sensitivity: Sensitivity::Normal,
        items: vec![HistoryItem {
            id: 0,
            kind: ItemKind::Text,
            mime: "text/plain".into(),
            name: None,
            bytes: None,
            dimensions: None,
            child_count: None,
            children_bytes: None,
            file_node: None,
            file_version: None,
        }],
        formats: vec![HistoryFormat {
            item: 0,
            representation: 0,
            name: "text/plain".into(),
            portable: Some(PortableFormat::Text),
            bytes: None,
            file_node: None,
        }],
        digests: fp
            .entries
            .into_iter()
            .map(|e| HistoryDigest {
                item: e.item,
                class: 0,
                name: e.name,
                size: e.size,
                digest: e.digest,
                dimensions: e.dimensions,
                modified_ms: e.modified_ms,
                directory: e.directory,
            })
            .collect(),
        inline: vec![],
        preview: None,
        rev: 0,
    }
}
fn holder(r: &ClipRecord, n: u8, rev: u64, state: HolderState) -> HolderEntry {
    HolderEntry {
        record_id: r.id,
        holder: device(n),
        rev,
        state,
        scope: HolderScope::TEXT,
    }
}
#[derive(Default)]
struct Mesh {
    nodes: Vec<HistoryWindow>,
}
impl Mesh {
    fn new() -> Self {
        Self {
            nodes: vec![HistoryWindow::new(); 4],
        }
    }
    fn reconcile(&mut self, a: usize, b: usize) {
        let left = self.nodes[a].clone();
        let right = self.nodes[b].clone();
        let plan = left.reconcile(&right.index());
        let pull = right
            .records()
            .iter()
            .filter(|r| plan.pull.contains(&r.id))
            .cloned()
            .collect::<Vec<_>>();
        let push = left
            .records()
            .iter()
            .filter(|r| plan.send.contains(&r.id))
            .cloned()
            .collect::<Vec<_>>();
        self.nodes[a].merge(pull, right.holders().iter().cloned()).unwrap();
        self.nodes[b].merge(push, left.holders().iter().cloned()).unwrap();
    }
}
#[test]
fn partitions_merge_eviction_lww_and_missed_delta_reconcile() {
    let mut mesh = Mesh::new();
    // Two disjoint partitions produce 80 records each, clocks overlap.
    for (a, b, author) in [(0, 1, 1), (2, 3, 3)] {
        let records: Vec<_> = (1..=80).map(|i| record(author, i, i)).collect();
        let holders: Vec<_> = records
            .iter()
            .map(|r| holder(r, author, 1, HolderState::Holding))
            .collect();
        mesh.nodes[a].merge(records, holders).unwrap();
        mesh.reconcile(a, b);
    }
    assert_ne!(mesh.nodes[0].digest(), mesh.nodes[2].digest());
    mesh.reconcile(1, 2);
    mesh.reconcile(0, 1);
    mesh.reconcile(2, 3);
    for node in &mesh.nodes {
        assert_eq!(node.records().len(), 100);
        assert_eq!(node.digest(), mesh.nodes[0].digest());
        assert_eq!(node.records().last().unwrap().clock, 31);
    }
    // Holder 2 releases; an old Holding arrives after release on another link.
    let latest = mesh.nodes[0].records()[0].clone();
    mesh.nodes[0]
        .merge([], [holder(&latest, 2, 8, HolderState::Released)])
        .unwrap();
    mesh.nodes[1]
        .merge([], [holder(&latest, 2, 7, HolderState::Holding)])
        .unwrap();
    mesh.reconcile(0, 1);
    mesh.reconcile(1, 2);
    mesh.reconcile(2, 3);
    for node in &mesh.nodes {
        assert_eq!(node.digest(), mesh.nodes[0].digest());
        assert!(
            node.holders()
                .iter()
                .any(|h| h.holder == device(2) && h.state == HolderState::Released)
        );
    }
    // Miss an entire new record on every other node; summaries detect it.
    let new = record(1, 81, 81);
    mesh.nodes[0]
        .merge([new.clone()], [holder(&new, 1, 1, HolderState::Holding)])
        .unwrap();
    assert_ne!(mesh.nodes[0].digest(), mesh.nodes[3].digest());
    mesh.reconcile(3, 0);
    mesh.reconcile(2, 3);
    mesh.reconcile(1, 2);
    for node in &mesh.nodes {
        assert_eq!(node.digest(), mesh.nodes[0].digest());
        assert_eq!(node.records()[0].id, new.id);
        assert!(node.reconcile(&mesh.nodes[0].index()).pull.is_empty());
    }
}
#[test]
fn merge_is_associative_commutative_idempotent_and_order_is_not_wall_time() {
    let records: Vec<_> = (0..220)
        .map(|i| {
            let mut r = record((i % 3 + 1) as u8, i, i / 3);
            r.time_ms = 1000 - i as i64;
            r
        })
        .collect();
    let mut a = HistoryWindow::new();
    a.merge(records.clone(), []).unwrap();
    let mut b = HistoryWindow::new();
    for chunk in records.rchunks(13) {
        b.merge(chunk.to_vec(), []).unwrap();
    }
    assert_eq!(a.records(), b.records());
    assert_eq!(a.digest(), b.digest());
    let old = a.digest();
    a.merge(a.records().to_vec(), []).unwrap();
    assert_eq!(a.digest(), old);
    let times = a.display_times();
    assert!(times.windows(2).all(|w| w[0].1 <= w[1].1));
    assert_eq!(a.records()[0].clock, 73);
    assert_eq!(a.records()[0].origin, device(1));
    assert_eq!(
        a.records()
            .iter()
            .filter(|r| r.clock == 72)
            .map(|r| r.origin)
            .collect::<Vec<_>>(),
        vec![device(3), device(2), device(1)]
    );
}
#[test]
fn old_remote_entries_do_not_cause_reconcile_livelock() {
    let mut a = HistoryWindow::new();
    let mut b = HistoryWindow::new();
    a.merge((101..=200).map(|i| record(1, i, i)), []).unwrap();
    b.merge((1..=100).map(|i| record(1, i, i)), []).unwrap();
    let plan = a.reconcile(&b.index());
    assert!(plan.pull.is_empty());
    assert_eq!(plan.send.len(), 100);
    b.merge(a.records().to_vec(), []).unwrap();
    assert_eq!(a.digest(), b.digest());
}
#[test]
fn local_deletion_does_not_change_shared_window_and_survives_resync_restart() {
    let mut window = HistoryWindow::new();
    window.merge((1..=100).map(|i| record(1, i, i)), []).unwrap();
    let hidden = window.records()[99].id;
    let digest = window.digest();
    assert!(window.hide_local(hidden));
    window.merge([record(1, 1, 1)], []).unwrap();
    assert_eq!(window.digest(), digest);
    let json = serde_json::to_vec(&window).unwrap();
    let mut restored: HistoryWindow = serde_json::from_slice(&json).unwrap();
    assert!(restored.tombstones().contains(&hidden));
    let mut before = None;
    let mut ids = vec![];
    loop {
        let page = restored.page(before, 99, false);
        assert!(page.records.len() <= 20);
        ids.extend(page.records.iter().map(|r| r.id));
        before = page.next;
        if !page.more {
            break;
        }
    }
    assert_eq!(ids.len(), 99);
    assert!(!ids.contains(&hidden));
    restored.merge([record(1, 101, 101)], []).unwrap();
    assert!(!restored.tombstones().contains(&hidden));
}
#[test]
fn content_rules_keep_reselected_old_copies_and_settings() {
    let r = record(1, 1, 1);
    let held = Identity {
        fingerprint: r.fingerprint(),
        source: Source::Device(device(2)),
    };
    let on = Rules {
        dedupe: true,
        receive: true,
    };
    assert_eq!(r.duplicate_of_current(&held, on, true), Ok(()));
    assert!(r.duplicate_of_current(&held, on, false).is_err());
    assert!(
        r.duplicate_of_current(&held, Rules { dedupe: false, ..on }, true)
            .is_err()
    );
    assert!(
        r.duplicate_of_current(&held, Rules { receive: false, ..on }, true)
            .is_err()
    );
    let mut second = r.clone();
    second.id.clip.seq = 2;
    second.clock = 2;
    let mut window = HistoryWindow::new();
    window.merge([r, second], []).unwrap();
    assert_eq!(window.records().len(), 2);
}
#[test]
fn immutable_conflicts_are_atomic_and_enrichment_is_supported() {
    let r = record(1, 1, 1);
    let h = holder(&r, 1, 1, HolderState::Holding);
    let mut w = HistoryWindow::new();
    w.merge([r.clone()], [h.clone()]).unwrap();
    let digest = w.digest();
    let mut conflict = r.clone();
    conflict.clock = 2;
    assert_eq!(w.merge([record(2, 2, 3), conflict], []), Err(HistoryError::Conflict));
    assert_eq!(w.digest(), digest);
    assert_eq!(w.records().len(), 1);
    let mut enriched = r.clone();
    enriched.inline = vec![InlinePayload {
        item: 0,
        representation: 0,
        format: PortableFormat::Text,
        bytes: b"hello".to_vec(),
    }];
    w.merge([enriched.clone()], []).unwrap();
    w.merge([r.clone()], []).unwrap();
    assert_eq!(w.get(r.id).unwrap().inline, enriched.inline);
    let mut supplement = enriched;
    supplement.preview = encode_inline_preview(1, 1, vec![0, 0, 0, 255]);
    supplement.rev = 1;
    w.merge([supplement], []).unwrap();
    assert_ne!(w.digest(), digest);
    let mut conflict = h;
    conflict.state = HolderState::Gone;
    assert_eq!(w.merge([], [conflict]), Err(HistoryError::Conflict));
}
#[test]
fn metadata_budget_sensitivity_and_priority() {
    let payload = |rep, format, n| InlinePayload {
        item: 0,
        representation: rep,
        format,
        bytes: vec![0; n],
    };
    let selected = select_inline(vec![
        payload(1, PortableFormat::Html, 10_000),
        payload(0, PortableFormat::Text, 30_000),
        payload(2, PortableFormat::Rtf, 2000),
    ]);
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].format, PortableFormat::Text);
    assert_eq!(selected[1].format, PortableFormat::Rtf);
    let mut r = record(1, 1, 1);
    r.inline = vec![payload(0, PortableFormat::Text, INLINE_MAX)];
    assert!(r.validate().is_ok());
    r.inline[0].bytes.push(0);
    assert!(r.validate().is_err());
    r.inline[0].bytes.truncate(INLINE_MAX);
    for sensitivity in [Sensitivity::Concealed, Sensitivity::Transient] {
        r.sensitivity = sensitivity;
        r.preview = encode_inline_preview(1, 1, vec![0; 4]);
        r.redact();
        assert_eq!(r.inline[0].bytes.len(), INLINE_MAX);
        assert!(r.preview.is_none() && r.digests.is_empty());
        assert!(r.validate().is_ok());
    }
}
#[test]
fn thumbnail_is_jpeg_bounded_and_alpha_gets_white_background() {
    for (width, height) in [(1024, 512), (512, 1024), (1, 1)] {
        let rgba = vec![0; (width * height * 4) as usize];
        let p = encode_inline_preview(width, height, rgba).unwrap();
        assert!(p.width <= 160 && p.height <= 160 && p.jpeg.len() <= 8192);
        let decoded = image::load_from_memory(&p.jpeg).unwrap().to_rgb8();
        assert_eq!((decoded.width(), decoded.height()), (p.width, p.height));
        assert!(decoded.pixels().all(|p| p.0.iter().all(|c| *c > 250)));
    }
    let mut seed = 7u32;
    let noisy = (0..640 * 480 * 4)
        .map(|i| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            if i % 4 == 3 { 255 } else { (seed >> 24) as u8 }
        })
        .collect();
    let p = encode_inline_preview(640, 480, noisy).unwrap();
    assert!(p.jpeg.len() <= 8192);
    assert!(encode_inline_preview(0, 5, vec![]).is_none());
    assert!(encode_inline_preview(5, 5, vec![0; 3]).is_none());
}
#[test]
fn mesh_forwarding_author_graph_phone_leaf_and_revision_dedupe() {
    let neighbour = |n, direct, author_link, phone, subscribed| HistoryNeighbour {
        id: device(n),
        direct,
        capable: true,
        subscribed,
        phone,
        connected_to_author: author_link,
    };
    // A-C-B: C forwards to B; a fourth peer already directly linked to A is skipped.
    let graph = [
        neighbour(1, true, true, false, false),
        neighbour(2, true, false, false, false),
        neighbour(4, true, true, false, false),
        neighbour(5, false, false, false, false),
        neighbour(6, true, false, true, false),
    ];
    assert_eq!(
        forward_targets(device(3), false, device(1), Some(device(1)), &graph),
        [device(2)].into()
    );
    assert!(forward_targets(device(3), true, device(1), Some(device(1)), &graph).is_empty());
    let full = [
        neighbour(2, true, true, false, false),
        neighbour(3, true, true, false, false),
    ];
    assert!(forward_targets(device(4), false, device(1), None, &full).is_empty());
    assert_eq!(forward_targets(device(1), false, device(1), None, &full).len(), 2);
    let r = record(1, 1, 1);
    let mut seen = ForwardSeen::default();
    assert!(seen.observe(&HistoryChange::Record(r.clone())));
    assert!(!seen.observe(&HistoryChange::Record(r.clone())));
    let e = holder(&r, 1, 0, HolderState::Holding);
    assert!(seen.observe(&HistoryChange::Holder(e.clone())));
    assert!(!seen.observe(&HistoryChange::Holder(e)));
    let newer = holder(&r, 1, 1, HolderState::Released);
    assert!(seen.observe(&HistoryChange::Holder(newer)));
    seen.retain_window(&HistoryWindow::new());
    assert!(seen.observe(&HistoryChange::Record(r.clone())));
    assert!(HistoryChange::Record(r.clone()).validate_author(device(2)).is_err());
    let mut proxied = r;
    proxied.origin = device(9);
    proxied.proxy = Some(device(1));
    let proxied = HistoryChange::Record(proxied);
    assert_eq!(proxied.author(), device(1));
    assert!(proxied.validate_author(device(1)).is_ok());
    let HistoryChange::Record(one) = proxied else { panic!() };
    let mut two = one.clone();
    two.id.clip.origin = device(2);
    two.proxy = Some(device(2));
    let mut window = HistoryWindow::new();
    window.merge([one, two], []).unwrap();
    assert_eq!(window.records().len(), 2);
    assert_eq!(window.records()[0].key().origin, device(2));
}
#[test]
fn digest_is_canonical_and_tracks_every_holder_revision() {
    let r = record(1, 1, 1);
    let mut a = HistoryWindow::new();
    let mut b = HistoryWindow::new();
    let hs = vec![
        holder(&r, 1, 1, HolderState::Holding),
        holder(&r, 2, 5, HolderState::Released),
    ];
    a.merge([r.clone()], hs.clone()).unwrap();
    b.merge([r.clone()], hs.into_iter().rev()).unwrap();
    assert_eq!(a.digest(), b.digest());
    let mut shuffled = a.index();
    shuffled[0].holders.reverse();
    assert_eq!(window_digest(&shuffled), a.digest());
    b.merge([], [holder(&r, 2, 6, HolderState::Gone)]).unwrap();
    assert_ne!(a.digest(), b.digest());
    assert_eq!(a.reconcile(&b.index()).pull, vec![r.id]);
    b.retain_members(&[device(1)].into());
    assert_eq!(b.holders().len(), 1);
}

#[test]
fn cyclic_mesh_forwards_once_and_never_refloods_evicted_records() {
    use std::collections::VecDeque;
    // Author A only connects to C. C-B-D form a cycle. D has a phone leaf E.
    let edges = [(0usize, 2usize), (2, 1), (1, 3), (3, 2), (3, 4)];
    let mut windows = vec![HistoryWindow::new(); 5];
    let mut seen = vec![ForwardSeen::default(); 5];
    let r = record(1, 1, 1);
    let change = HistoryChange::Record(r.clone());
    let mut pending = VecDeque::from([(0usize, None)]);
    let mut forwards = 0;
    while let Some((at, sender)) = pending.pop_front() {
        windows[at].merge([r.clone()], []).unwrap();
        seen[at].retain_window(&windows[at]);
        if !seen[at].observe_in_window(&windows[at], &change) {
            continue;
        }
        let neighbours: Vec<_> = edges
            .iter()
            .filter_map(|(a, b)| {
                if *a == at {
                    Some(*b)
                } else if *b == at {
                    Some(*a)
                } else {
                    None
                }
            })
            .map(|n| HistoryNeighbour {
                id: device(n as u8 + 1),
                direct: true,
                capable: true,
                phone: n == 4,
                subscribed: true,
                connected_to_author: edges.contains(&(0, n)) || edges.contains(&(n, 0)),
            })
            .collect();
        for to in forward_targets(
            device(at as u8 + 1),
            at == 4,
            r.origin,
            sender.map(|s| device(s as u8 + 1)),
            &neighbours,
        ) {
            pending.push_back((usize::from(to.0[0] - 1), Some(at)));
            forwards += 1;
        }
        assert!(forwards <= 10, "seen revision must break cycles");
    }
    for w in &windows {
        assert_eq!(w.digest(), windows[0].digest());
    }
    for at in 0..5 {
        windows[at].merge((2..=101).map(|i| record(1, i, i)), []).unwrap();
        seen[at].retain_window(&windows[at]);
        assert!(!seen[at].observe_in_window(&windows[at], &change));
    }
}

#[test]
fn partial_payload_merge_is_canonical_and_preview_has_no_alpha_fringe() {
    let mut left = record(1, 1, 1);
    left.formats.push(HistoryFormat {
        item: 0,
        representation: 1,
        name: "text/html".into(),
        portable: Some(PortableFormat::Html),
        bytes: None,
        file_node: None,
    });
    let mut right = left.clone();
    left.inline = vec![InlinePayload {
        item: 0,
        representation: 1,
        format: PortableFormat::Html,
        bytes: b"<b>x</b>".to_vec(),
    }];
    right.inline = vec![InlinePayload {
        item: 0,
        representation: 0,
        format: PortableFormat::Text,
        bytes: b"x".to_vec(),
    }];
    let mut a = HistoryWindow::new();
    a.merge([left.clone(), right.clone()], []).unwrap();
    let mut b = HistoryWindow::new();
    b.merge([right, left], []).unwrap();
    assert_eq!(a.records(), b.records());
    // Invisible red beside opaque blue must mix white+blue, never red+blue.
    let rgba = (0..320)
        .flat_map(|i| if i % 2 == 0 { [255, 0, 0, 0] } else { [0, 0, 255, 255] })
        .collect();
    let p = encode_inline_preview(320, 1, rgba).unwrap();
    let rgb = image::load_from_memory(&p.jpeg).unwrap().to_rgb8();
    assert!(rgb.pixels().all(|p| p.0[0].abs_diff(p.0[1]) < 10));
}

#[test]
fn phone_keeps_full_index_digest_without_downloading_all_bodies() {
    let mut pc = HistoryWindow::new();
    pc.merge((1..=100).map(|i| record(1, i, i)), []).unwrap();
    let phone_index = merge_indexes(&[], &pc.index()).unwrap();
    let mut phone_bodies = HistoryWindow::new();
    phone_bodies.merge(pc.page(None, 20, true).records, []).unwrap();
    assert_eq!(phone_bodies.records().len(), 20);
    assert_ne!(phone_bodies.digest(), pc.digest());
    assert_eq!(window_digest(&phone_index), pc.digest());
    assert!(reconcile_indexes(&phone_index, &pc.index()).pull.is_empty());
    let next = record(1, 101, 101);
    pc.merge([next.clone()], [holder(&next, 1, 1, HolderState::Holding)])
        .unwrap();
    assert_eq!(reconcile_indexes(&phone_index, &pc.index()).pull, vec![next.id]);
    let merged = merge_indexes(&phone_index, &pc.index()).unwrap();
    assert_eq!(window_digest(&merged), pc.digest());
    assert_eq!(merged.len(), 100);
    assert_eq!(merged.last().unwrap().id.clip.seq, 2);
}

#[test]
fn concealed_heads_converge_then_purge_inline_holders_and_late_records() {
    for sensitivity in [Sensitivity::Concealed, Sensitivity::Transient] {
        let mut secret = record(1, 2, 2);
        secret.sensitivity = sensitivity;
        secret.redact();
        secret.inline = vec![InlinePayload {
            item: 0,
            representation: 0,
            format: PortableFormat::Text,
            bytes: b"sensitive head".to_vec(),
        }];
        let old = record(2, 1, 1);
        let newer = record(3, 3, 3);
        let mut a = HistoryWindow::new();
        let mut b = HistoryWindow::new();
        let mut c = HistoryWindow::new();
        a.merge([old.clone(), secret.clone()], []).unwrap();
        b.merge([secret.clone(), old.clone()], []).unwrap();
        assert!(a.get(secret.id).is_some());
        assert_eq!(a.digest(), b.digest());
        assert!(a.index()[0].head_only);
        let prior = a.index();
        assert!(a.merge([newer.clone()], []).unwrap().contains(&secret.id));
        b.merge([newer.clone()], []).unwrap();
        c.merge([newer.clone(), old.clone()], []).unwrap();
        assert!(c.merge([secret.clone()], []).unwrap().contains(&secret.id));
        assert!(a.get(secret.id).is_none() && b.get(secret.id).is_none() && c.get(secret.id).is_none());
        assert_eq!(a.digest(), b.digest());
        assert_eq!(b.digest(), c.digest());
        let index = merge_indexes(&prior, &c.index()).unwrap();
        assert!(index.iter().all(|e| e.id != secret.id));
        assert_eq!(window_digest(&index), c.digest());
        assert!(!reconcile_indexes(&prior, &c.index()).send.contains(&secret.id));
        let mixed = prior.into_iter().chain(c.index()).collect::<Vec<_>>();
        // Compare unique canonical entries: an old concealed body cannot change
        // the digest after the index head proves it superseded.
        let unique = merge_indexes(&[], &mixed).unwrap();
        assert_eq!(window_digest(&unique), c.digest());
    }
}
#[test]
fn holder_role_filter_removes_phone_entries_without_forging_revisions() {
    let r = record(1, 1, 1);
    let pc = holder(&r, 1, 1, HolderState::Holding);
    let phone = holder(&r, 2, 9, HolderState::Holding);
    let mut w = HistoryWindow::new();
    w.merge([r], [pc.clone(), phone]).unwrap();
    w.retain_pc_holders(&[device(1)].into());
    assert_eq!(w.holders(), &[pc]);
}
