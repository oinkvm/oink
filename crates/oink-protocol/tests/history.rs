use oink_foundation::{
    DeviceId, GroupId,
    clipboard::Sensitivity,
    clipboard_history as h,
    content::{ClipId, OfferId},
};
use oink_protocol::v2::{
    self, Message, RawValue as V,
    admission::{Agreement, Context, Phase},
    dto::*,
    frame::{self, OpaqueFrame},
    registry::{self, Transport},
    schema::{WireDto, WireValue},
};
fn record() -> h::ClipRecord {
    h::ClipRecord {
        id: OfferId {
            clip: ClipId {
                origin: DeviceId([1; 32]),
                seq: 7,
            },
            generation: 0,
        },
        clock: 8,
        time_ms: 1234,
        origin: DeviceId([1; 32]),
        proxy: None,
        sensitivity: Sensitivity::Normal,
        items: vec![h::HistoryItem {
            id: 0,
            kind: h::ItemKind::Image,
            mime: "image/png".into(),
            name: None,
            bytes: Some(70000),
            dimensions: Some((640, 480)),
            child_count: None,
            children_bytes: None,
            file_node: None,
            file_version: None,
        }],
        formats: vec![h::HistoryFormat {
            item: 0,
            representation: 0,
            name: "image/png".into(),
            portable: Some(h::PortableFormat::Png),
            bytes: Some(70000),
            file_node: None,
        }],
        digests: vec![h::HistoryDigest {
            item: 0,
            class: 4,
            name: None,
            size: None,
            digest: Some([9; 32]),
            dimensions: Some((640, 480)),
            modified_ms: None,
            directory: false,
        }],
        inline: vec![],
        preview: None,
        rev: 0,
    }
}
fn window() -> h::HistoryWindow {
    let r = record();
    let holder = h::HolderEntry {
        record_id: r.id,
        holder: r.origin,
        rev: 1,
        state: h::HolderState::Holding,
        scope: h::HolderScope::PNG,
    };
    let mut w = h::HistoryWindow::new();
    w.merge([r], [holder]).unwrap();
    w
}
fn messages() -> Vec<(&'static str, Message)> {
    let w = window();
    let group = GroupId([2; 16]);
    let request = [3; 16];
    let mut rich = record();
    rich.inline = vec![h::InlinePayload {
        item: 0,
        representation: 0,
        format: h::PortableFormat::Png,
        bytes: vec![0; 32],
    }];
    rich.preview = h::encode_inline_preview(1, 1, vec![255, 0, 0, 128]);
    rich.proxy = Some(rich.id.clip.origin);
    rich.origin = DeviceId([4; 32]);
    rich.rev = 1;
    let mut rich_window = h::HistoryWindow::new();
    rich_window.merge([rich.clone()], w.holders().iter().cloned()).unwrap();
    let mut file = record();
    file.items[0].kind = h::ItemKind::Directory;
    file.items[0].mime = "application/octet-stream".into();
    file.items[0].name = Some("folder".into());
    file.items[0].dimensions = None;
    file.items[0].file_node = Some(u64::MAX - 3);
    file.items[0].file_version = Some(42);
    file.formats[0].name = "oink:file-namespace/1".into();
    file.formats[0].portable = None;
    file.formats[0].file_node = Some(u64::MAX - 3);
    file.digests.clear();
    vec![
        (
            "035b-file",
            Message::HistoryDelta(HistoryDelta::from_domain(&h::HistoryChange::Record(file), w.digest()).unwrap()),
        ),
        (
            "035b-preview",
            Message::HistoryDelta(
                HistoryDelta::from_domain(&h::HistoryChange::Record(rich), rich_window.digest()).unwrap(),
            ),
        ),
        (
            "0356-empty",
            Message::HistorySummary(HistorySummary::from_window(group, &h::HistoryWindow::new(), false).unwrap()),
        ),
        (
            "0356-window",
            Message::HistorySummary(HistorySummary::from_window(group, &w, true).unwrap()),
        ),
        (
            "0357-request",
            Message::HistoryIndexRequest(
                HistoryIndexRequest::from_raw([(1, V::Bytes(request.to_vec()))].into()).unwrap(),
            ),
        ),
        (
            "0358-index",
            Message::HistoryIndex(HistoryIndex::from_window(request, &w).unwrap()),
        ),
        (
            "0359-page",
            Message::HistoryPull(
                HistoryPull::from_query(request, HistoryQuery::Page { before: None, max: 20 }, true).unwrap(),
            ),
        ),
        (
            "0359-before",
            Message::HistoryPull(
                HistoryPull::from_query(
                    request,
                    HistoryQuery::Page {
                        before: Some(record().key()),
                        max: 20,
                    },
                    false,
                )
                .unwrap(),
            ),
        ),
        (
            "0359-ids",
            Message::HistoryPull(HistoryPull::from_query(request, HistoryQuery::Ids(vec![record().id]), true).unwrap()),
        ),
        (
            "035a-records",
            Message::HistoryRecords(HistoryRecords::from_page(request, &w.page(None, 20, true), w.digest()).unwrap()),
        ),
        (
            "035b-record",
            Message::HistoryDelta(HistoryDelta::from_domain(&h::HistoryChange::Record(record()), w.digest()).unwrap()),
        ),
        (
            "035b-holder",
            Message::HistoryDelta(
                HistoryDelta::from_domain(&h::HistoryChange::Holder(w.holders()[0].clone()), w.digest()).unwrap(),
            ),
        ),
    ]
}
#[test]
fn history_wire_goldens_and_admission() {
    let agreement = Agreement::new(
        [
            "oink.core/2",
            "oink.group/2",
            "oink.clipboard/2",
            "oink.clipboard.history/1",
            "oink.low/1",
        ]
        .into_iter()
        .map(str::to_owned),
    )
    .unwrap();
    let without = Agreement::new(
        ["oink.core/2", "oink.group/2", "oink.clipboard/2", "oink.low/1"]
            .into_iter()
            .map(str::to_owned),
    )
    .unwrap();
    for (name, message) in messages() {
        let encoded = frame::encode(&message, v2::limits::LOW_FRAME).unwrap();
        let hex = encoded.iter().map(|b| format!("{b:02x}")).collect::<String>() + "\n";
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/v2")
            .join(format!("{name}.hex"));
        if std::env::var_os("OINK_BLESS").is_some() {
            std::fs::write(&path, &hex).unwrap();
        }
        assert_eq!(std::fs::read_to_string(path).unwrap(), hex, "{name}");
        let opaque = OpaqueFrame::parse_exact(&encoded, v2::limits::LOW_FRAME).unwrap();
        let context = |transport, agreement| Context {
            kind: 4,
            transport,
            phase: Phase::Member,
            agreement,
            direct: true,
            old_member_lane: false,
        };
        assert_eq!(
            context(Transport::Reliable, &agreement).decode(opaque).unwrap(),
            message
        );
        assert!(context(Transport::Reliable, &without).decode(opaque).is_err());
        assert!(context(Transport::Datagram, &agreement).decode(opaque).is_err());
        assert_eq!(
            registry::message(message.type_id()).unwrap().pause,
            registry::PausePolicy::Suspend
        );
    }
    assert!(registry::message(0x0352).is_none() && registry::message(0x0353).is_none());
}
#[test]
fn domain_bridges_payload_preview_and_holder_roundtrip() {
    let mut r = record();
    r.inline = vec![h::InlinePayload {
        item: 0,
        representation: 0,
        format: h::PortableFormat::Png,
        bytes: vec![0x42; 32768],
    }];
    r.preview = h::encode_inline_preview(2, 1, vec![0, 0, 0, 0, 0, 0, 0, 255]);
    r.rev = 1;
    let dto = HistoryRecord::from_domain(&r).unwrap();
    assert_eq!(dto.to_domain().unwrap(), r);
    let encoded = dto.encode(v2::limits::LOW_FRAME).unwrap();
    assert_eq!(HistoryRecord::decode(&encoded, v2::limits::LOW_FRAME).unwrap(), dto);
    let msg = Message::HistoryDelta(HistoryDelta::from_domain(&h::HistoryChange::Record(r.clone()), [0; 32]).unwrap());
    assert!(frame::encode(&msg, v2::limits::RELIABLE_FRAME).is_err());
    let a = Agreement::new(
        [
            "oink.core/2",
            "oink.group/2",
            "oink.clipboard/2",
            "oink.clipboard.history/1",
            "oink.low/1",
        ]
        .into_iter()
        .map(str::to_owned),
    )
    .unwrap();
    assert!(a.permits_on(msg.type_id(), 4, Transport::Low));
    let bytes = frame::encode(&msg, v2::limits::LOW_FRAME).unwrap();
    let context = Context {
        kind: 4,
        transport: Transport::Low,
        phase: Phase::Member,
        agreement: &a,
        direct: true,
        old_member_lane: false,
    };
    assert_eq!(
        context
            .decode(OpaqueFrame::parse_exact(&bytes, v2::limits::LOW_FRAME).unwrap())
            .unwrap(),
        msg
    );
    for sensitivity in [Sensitivity::Concealed, Sensitivity::Transient] {
        let mut r = dto.to_domain().unwrap();
        r.sensitivity = sensitivity;
        assert!(HistoryRecord::from_domain(&r).is_err());
        r.redact();
        assert_eq!(HistoryRecord::from_domain(&r).unwrap().to_domain().unwrap(), r);
    }
    let w = window();
    assert_eq!(
        HistoryIndex::from_window([0; 16], &w)
            .unwrap()
            .domain_entries()
            .unwrap(),
        w.index()
    );
    assert_eq!(
        HistoryRecords::from_page([0; 16], &w.page(None, 20, true), w.digest())
            .unwrap()
            .to_page()
            .unwrap(),
        w.page(None, 20, true)
    );
    let e = w.holders()[0].clone();
    assert_eq!(HistoryHolder::from_domain(&e).unwrap().to_domain().unwrap(), e);
}
#[test]
fn reject_invalid_queries_records_and_delta_shapes() {
    for max in [0, 21, 255] {
        assert!(HistoryPull::from_query([0; 16], HistoryQuery::Page { before: None, max }, true).is_err());
    }
    assert!(HistoryPull::from_query([0; 16], HistoryQuery::Ids(vec![]), true).is_err());
    let query = HistoryQuery::Ids(vec![record().id]);
    assert_eq!(
        HistoryPull::from_query([0; 16], query.clone(), false)
            .unwrap()
            .query()
            .unwrap(),
        query
    );
    let mut raw = HistoryPull::from_query([0; 16], query, true).unwrap().into_raw();
    raw.insert(4, V::Uint(20));
    assert!(HistoryPull::from_raw(raw).is_err());
    let mut r = record();
    r.preview = Some(h::InlinePreview {
        width: 161,
        height: 1,
        jpeg: vec![255, 216, 255, 217],
    });
    assert!(HistoryRecord::from_domain(&r).is_err());
    let mut mismatch = record();
    mismatch.preview = h::encode_inline_preview(320, 160, vec![255; 320 * 160 * 4]);
    mismatch.preview.as_mut().unwrap().width = 1;
    assert!(HistoryRecord::from_domain(&mismatch).is_err());
    let mut malformed = record();
    malformed.preview = Some(h::InlinePreview {
        width: 1,
        height: 1,
        jpeg: vec![255, 216, 255, 217],
    });
    assert!(HistoryRecord::from_domain(&malformed).is_err());
    let r = HistoryRecord::from_domain(&record()).unwrap();
    let mut raw = r.into_raw();
    raw.insert(99, V::from("future-field"));
    let retained = HistoryRecord::from_raw(raw.clone()).unwrap();
    assert_eq!(retained.raw(), &raw);
    let delta = HistoryDelta::from_domain(&h::HistoryChange::Record(record()), [0; 32]).unwrap();
    let mut raw = delta.into_raw();
    raw.insert(2, HistoryHolder::from_domain(&window().holders()[0]).unwrap().write());
    assert!(HistoryDelta::from_raw(raw).is_err());
}

#[test]
fn wire_rejects_aggregate_inline_overflow_sensitive_bytes_and_corrupt_index() {
    let mut r = record();
    r.formats.push(h::HistoryFormat {
        item: 0,
        representation: 1,
        name: "text/html".into(),
        portable: Some(h::PortableFormat::Html),
        bytes: None,
        file_node: None,
    });
    r.inline = vec![
        h::InlinePayload {
            item: 0,
            representation: 0,
            format: h::PortableFormat::Png,
            bytes: vec![0; 16384],
        },
        h::InlinePayload {
            item: 0,
            representation: 1,
            format: h::PortableFormat::Html,
            bytes: vec![0; 16384],
        },
    ];
    let dto = HistoryRecord::from_domain(&r).unwrap();
    let mut raw = dto.raw().clone();
    let V::Array(inlines) = raw.get_mut(&10).unwrap() else {
        panic!()
    };
    let V::Map(second) = &mut inlines[1] else { panic!() };
    second.insert(4, V::Bytes(vec![0; 16385]));
    assert!(HistoryRecord::from_raw(raw).is_err());
    let mut raw = dto.into_raw();
    raw.insert(6, V::Uint(1));
    assert!(HistoryRecord::from_raw(raw).is_err());
    let mut index = HistoryIndex::from_window([0; 16], &window()).unwrap().into_raw();
    index.insert(3, V::Bytes(vec![0; 32]));
    assert!(HistoryIndex::from_raw(index).is_err());
    let query = HistoryQuery::Ids(vec![record().id; 101]);
    assert!(HistoryPull::from_query([0; 16], query, true).is_err());
}

#[test]
fn full_index_with_sixteen_holders_requires_low_and_roundtrips_every_revision() {
    let mut window = h::HistoryWindow::new();
    for seq in 1..=100 {
        let mut r = record();
        r.id.clip.seq = seq;
        r.clock = seq;
        let holders = (1..=16)
            .map(|holder| h::HolderEntry {
                record_id: r.id,
                holder: DeviceId([holder; 32]),
                rev: u64::MAX - u64::from(holder),
                state: h::HolderState::Holding,
                scope: h::HolderScope::PNG,
            })
            .collect::<Vec<_>>();
        window.merge([r], holders).unwrap();
    }
    let index = HistoryIndex::from_window([3; 16], &window).unwrap();
    let message = Message::HistoryIndex(index.clone());
    assert!(frame::encode(&message, v2::limits::RELIABLE_FRAME).is_err());
    let bytes = frame::encode(&message, v2::limits::LOW_FRAME).unwrap();
    assert!(bytes.len() > v2::limits::RELIABLE_FRAME);
    // Freeze the complete 100 x 16 wire vector without a 200 KiB hex fixture.
    // This includes maximal u64 revisions and every required head-only flag.
    use sha2::{Digest, Sha256};
    assert_eq!(bytes.len(), 102_994);
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        "04f60c6903aa85ae96597bab82a0669c61e9cc4a36737fbea5a2dcf3edcf7add"
    );
    let agreement = Agreement::new(
        [
            "oink.core/2",
            "oink.group/2",
            "oink.clipboard/2",
            "oink.clipboard.history/1",
            "oink.low/1",
        ]
        .into_iter()
        .map(str::to_owned),
    )
    .unwrap();
    let without_low = Agreement::new(
        [
            "oink.core/2",
            "oink.group/2",
            "oink.clipboard/2",
            "oink.clipboard.history/1",
        ]
        .into_iter()
        .map(str::to_owned),
    )
    .unwrap();
    assert!(agreement.permits_on(message.type_id(), 4, Transport::Low));
    assert!(!without_low.permits_on(message.type_id(), 4, Transport::Low));
    let context = Context {
        kind: 4,
        transport: Transport::Low,
        phase: Phase::Member,
        agreement: &agreement,
        direct: true,
        old_member_lane: false,
    };
    assert_eq!(
        context
            .decode(OpaqueFrame::parse_exact(&bytes, v2::limits::LOW_FRAME).unwrap())
            .unwrap(),
        message
    );
    let entries = index.domain_entries().unwrap();
    assert_eq!(entries.len(), 100);
    assert!(entries.iter().all(|entry| entry.holders.len() == 16));
    assert_eq!(entries, window.index());
}

#[test]
fn history_file_namespace_identity_and_head_only_index_roundtrip() {
    let mut r = record();
    r.items[0].kind = h::ItemKind::Directory;
    r.items[0].dimensions = None;
    r.items[0].file_node = Some(u64::MAX - 3);
    r.items[0].file_version = Some(42);
    r.formats[0].file_node = Some(u64::MAX - 3);
    assert_eq!(HistoryRecord::from_domain(&r).unwrap().to_domain().unwrap(), r);
    r.items[0].file_version = None;
    assert!(HistoryRecord::from_domain(&r).is_err());
    let mut r = record();
    r.sensitivity = Sensitivity::Concealed;
    r.redact();
    r.inline = vec![h::InlinePayload {
        item: 0,
        representation: 0,
        format: h::PortableFormat::Png,
        bytes: vec![1; 32],
    }];
    let mut w = h::HistoryWindow::new();
    w.merge([r.clone()], []).unwrap();
    let index = HistoryIndex::from_window([1; 16], &w).unwrap();
    assert!(index.domain_entries().unwrap()[0].head_only);
    assert_eq!(
        HistoryRecord::from_domain(&r).unwrap().to_domain().unwrap().inline,
        r.inline
    );
}
