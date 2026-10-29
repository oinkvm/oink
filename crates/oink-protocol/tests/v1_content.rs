use oink_protocol::v1::{
    self, ErrorCode, Message, RawValue as V,
    dto::*,
    schema::{Open, WireDto, WireValue},
};
use oink_test_support::protocol as support;

fn inline(bytes: usize) -> Representation {
    let mut raw = support::sample_named("Representation");
    raw.insert(
        7,
        V::Map([(1, V::Uint(1)), (2, V::Map([(1, V::Bytes(vec![42; bytes]))].into()))].into()),
    );
    Representation::from_raw(raw).unwrap()
}

fn inventory(representations: Vec<V>) -> Inventory {
    let mut item = support::sample_named("ContentItem");
    item.insert(3, V::Array(representations));
    let mut raw = support::sample_named("Inventory");
    raw.insert(3, V::Array(vec![V::Map(item)]));
    Inventory::from_raw(raw).unwrap()
}

#[test]
fn nested_inventory_is_retained_without_reconstructing_native_formats() {
    let mut rep = inline(8);
    rep.edit(|raw| {
        raw.insert(900, support::synthetic_unknown());
    })
    .unwrap();
    let mut object = inventory(vec![rep.write()]);
    object
        .edit(|raw| {
            raw.insert(900, V::Null);
        })
        .unwrap();
    let encoded = object.encode(8 * 1024 * 1024).unwrap();
    let decoded = Inventory::from_raw(v1::cbor::decode_map(&encoded, 8 * 1024 * 1024).unwrap()).unwrap();
    assert_eq!(decoded, object);
    assert_eq!(
        decoded.items().unwrap().0[0].representations().unwrap().0[0].raw()[&900],
        support::synthetic_unknown()
    );
    assert_eq!(decoded.inline_bytes().unwrap(), 8);
    let future = RepresentationData::from_raw([(1, V::Uint(400)), (2, support::synthetic_unknown())].into()).unwrap();
    assert!(matches!(future.variant().unwrap(), Open::Unknown { id: 400, .. }));
    rep.edit(|raw| {
        raw.insert(7, future.write());
    })
    .unwrap();
    assert!(matches!(
        rep.data().unwrap().variant().unwrap(),
        Open::Unknown { id: 400, .. }
    ));
}

#[test]
fn reliable_offer_has_one_inline_budget_and_bound_identity() {
    let mut offer = support::sample_named("ClipboardOffer");
    offer.insert(6, inventory(vec![inline(4096).write()]).write());
    assert!(ClipboardOffer::from_raw(offer.clone()).is_ok());
    offer.insert(6, inventory(vec![inline(4097).write()]).write());
    assert_eq!(
        ClipboardOffer::from_raw(offer.clone()).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    offer.insert(6, support::sample_type("Inventory"));
    let V::Map(id) = offer.get_mut(&1).unwrap() else {
        panic!()
    };
    id.insert(2, V::Uint(500));
    assert_eq!(
        ClipboardOffer::from_raw(offer).unwrap_err().code,
        ErrorCode::StateConflict
    );
}

#[test]
fn file_names_duplicates_and_representation_limits_are_rejected() {
    for name in ["", ".", "..", "one/two", "one\\two", "zero\0byte"] {
        let mut raw = support::sample_named("FileNode");
        raw.insert(3, V::from(name));
        assert!(FileNode::from_raw(raw).is_err(), "{name:?}");
    }
    let mut object = support::sample_named("Inventory");
    let item = support::sample_type("ContentItem");
    object.insert(3, V::Array(vec![item.clone(), item]));
    assert!(Inventory::from_raw(object).is_err());
    let raw = [(
        1,
        V::Map(
            [
                (1, V::Uint(1)),
                (2, V::Map([(1, V::Uint(256 * 1024 * 1024 + 1))].into())),
            ]
            .into(),
        ),
    )]
    .into();
    assert_eq!(
        RepresentationReader::from_raw(raw).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    assert!(RepresentationInline::from_raw([(1, V::Bytes(vec![0; 65537]))].into()).is_err());
}

#[test]
fn preflight_applies_nested_data_variant_bounds_before_materialization() {
    let mut object = inventory(vec![inline(1).write()]).into_raw();
    let V::Array(items) = object.get_mut(&3).unwrap() else {
        panic!()
    };
    let V::Map(item) = &mut items[0] else { panic!() };
    let V::Array(reps) = item.get_mut(&3).unwrap() else {
        panic!()
    };
    let V::Map(rep) = &mut reps[0] else { panic!() };
    rep.insert(
        7,
        V::Map([(1, V::Uint(1)), (2, V::Map([(1, V::Bytes(vec![0; 65537]))].into()))].into()),
    );
    let mut offer = support::sample_named("ClipboardOffer");
    offer.insert(6, V::Map(object));
    assert_eq!(
        ClipboardOffer::from_raw(offer.clone()).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    let bytes = v1::cbor::encode_map(&offer, 128 * 1024).unwrap();
    assert_eq!(
        Message::preflight(0x0300, &bytes).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
}

#[test]
fn current_file_version_is_one_bounded_clipboard_low_message() {
    let value = ContentFileVersion::from_raw(support::sample_named("ContentFileVersion")).unwrap();
    let message = Message::ContentFileVersion(value.clone());
    let bytes = v1::frame::encode(&message, 65536).unwrap();
    let decoded = v1::frame::OpaqueFrame::parse_exact(&bytes, 65536)
        .unwrap()
        .decode()
        .unwrap();
    assert_eq!(decoded, message);
    assert_eq!(message.type_id(), 0x0333);
    let entry = v1::registry::MESSAGES
        .iter()
        .find(|entry| entry.id == message.type_id())
        .unwrap();
    assert_eq!(entry.routes.len(), 1);
    assert_eq!(entry.routes[0].kind, 4);
    assert_eq!(
        value.entry().unwrap().version().unwrap(),
        FileNode::from_raw(support::sample_named("FileNode"))
            .unwrap()
            .version()
            .unwrap()
    );
}

#[test]
fn inventory_checks_cross_item_representation_ids_and_root_identity() {
    let mut first = support::sample_named("ContentItem");
    first.insert(1, V::Uint(1));
    first.insert(3, V::Array(vec![inline(1).write()]));
    let mut second = first.clone();
    second.insert(1, V::Uint(2));
    let mut raw = support::sample_named("Inventory");
    raw.insert(3, V::Array(vec![V::Map(first.clone()), V::Map(second.clone())]));
    assert_eq!(
        Inventory::from_raw(raw.clone()).unwrap_err().code,
        ErrorCode::MalformedFrame
    );
    let mut rep = inline(1).into_raw();
    rep.insert(1, V::Uint(2));
    second.insert(3, V::Array(vec![V::Map(rep)]));
    raw.insert(3, V::Array(vec![V::Map(first), V::Map(second)]));
    assert!(Inventory::from_raw(raw.clone()).is_ok());
    let mut root = support::sample_named("FileNode");
    root.insert(1, V::Uint(1));
    root.remove(&2);
    raw.insert(4, V::Array(vec![V::Map(root.clone()), V::Map(root.clone())]));
    assert_eq!(
        Inventory::from_raw(raw.clone()).unwrap_err().code,
        ErrorCode::MalformedFrame
    );
    root.insert(2, V::Uint(2));
    raw.insert(4, V::Array(vec![V::Map(root)]));
    assert_eq!(Inventory::from_raw(raw).unwrap_err().code, ErrorCode::MalformedFrame);
}

#[test]
fn persistent_file_operations_require_cloud_capability_on_both_low_routes() {
    let names = [
        "CloudLinkRequest",
        "CloudLinkOffer",
        "CloudLinkUnavailable",
        "CloudLinkList",
        "CloudLinkSelect",
        "CloudLinkRead",
    ];
    for (offset, name) in names.into_iter().enumerate() {
        let entry = v1::registry::MESSAGES
            .iter()
            .find(|e| e.name == name)
            .expect("persistent cloud file message is registered");
        assert_eq!(entry.id, 0x0334 + offset as u16);
        assert!(entry.capabilities.contains(&"oink.cloud-file-links/1"));
        assert_eq!(entry.routes.iter().map(|r| r.kind).collect::<Vec<_>>(), vec![4, 10]);
        assert!(entry.routes.iter().all(|r| r.transport == v1::registry::Transport::Low));
    }
}

#[test]
fn persistent_grants_reject_bad_tokens_and_ambiguous_roots() {
    let mut request = support::sample_named("CloudLinkRequest");
    for roots in [vec![], vec![V::Uint(1), V::Uint(1)]] {
        request.insert(2, V::Array(roots));
        assert_eq!(
            CloudLinkRequest::from_raw(request.clone()).unwrap_err().code,
            ErrorCode::MalformedFrame
        );
    }
    for length in [0, 31, 33] {
        let mut raw = support::sample_named("CloudLinkRead");
        raw.insert(2, V::Bytes(vec![0; length]));
        assert!(CloudLinkRead::from_raw(raw).is_err());
    }
    let root = V::Map(support::sample_named("FileNode"));
    let mut offer = support::sample_named("CloudLinkOffer");
    for roots in [vec![], vec![root.clone(), root]] {
        offer.insert(3, V::Array(roots));
        assert_eq!(
            CloudLinkOffer::from_raw(offer.clone()).unwrap_err().code,
            ErrorCode::MalformedFrame
        );
    }
}

#[test]
fn fulfilled_roots_must_be_unique_offered_roots_with_snapshot_identities() {
    let mut offer = support::sample_named("CloudLinkOffer");
    // The old form remains a real-file offer when the extension is absent.
    assert!(CloudLinkOffer::from_raw(offer.clone()).is_ok());
    let root = V::Map([(1, V::Uint(1)), (2, V::Bytes(vec![3; 16]))].into());
    for roots in [
        vec![root.clone(), root],
        vec![V::Map([(1, V::Uint(999)), (2, V::Bytes(vec![3; 16]))].into())],
        vec![V::Map([(1, V::Uint(1)), (2, V::Bytes(vec![3; 15]))].into())],
    ] {
        offer.insert(4, V::Array(roots));
        assert!(CloudLinkOffer::from_raw(offer.clone()).is_err());
    }
}

#[test]
fn persistent_read_retains_future_fields_and_is_denied_to_baseline_peers() {
    use v1::{
        admission::{Agreement, Context, Phase},
        registry::Transport,
    };
    let mut raw = support::sample_named("CloudLinkRead");
    raw.insert(900, support::synthetic_unknown());
    let message = Message::CloudLinkRead(CloudLinkRead::from_raw(raw).unwrap());
    let bytes = v1::frame::encode(&message, 65536).unwrap();
    let parse = || v1::frame::OpaqueFrame::parse_exact(&bytes, 65536).unwrap();
    assert_eq!(parse().decode().unwrap(), message);
    let baseline = Agreement::new(v1::registry::BASELINE.iter().map(|s| s.to_string())).unwrap();
    let ctx = Context {
        kind: 4,
        transport: Transport::Low,
        phase: Phase::Member,
        agreement: &baseline,
        direct: true,
        old_member_lane: false,
    };
    assert_eq!(
        ctx.admit_frame(parse()).unwrap_err().code,
        ErrorCode::UnnegotiatedCapability
    );
    let enabled = Agreement::new(v1::registry::CAPABILITIES.iter().map(|s| s.to_string())).unwrap();
    for kind in [4, 10] {
        let ctx = Context {
            kind,
            agreement: &enabled,
            ..ctx
        };
        ctx.admit_frame(parse()).unwrap();
    }
}

#[test]
fn native_file_metadata_is_bounded_scoped_and_retains_original_indices() {
    let mut offer = support::sample_named("CloudLinkOffer");
    let metadata = support::sample_named("NativeFileMetadata");
    offer.insert(5, V::Map(metadata.clone()));
    let parsed = CloudLinkOffer::from_raw(offer.clone()).unwrap();
    assert_eq!(parsed.native().unwrap().unwrap().windows().unwrap().0.len(), 1);
    for length in [0, 71, 73] {
        let mut entry = support::sample_named("WindowsFileDescriptor");
        entry.insert(4, V::Bytes(vec![0; length]));
        assert!(WindowsFileDescriptor::from_raw(entry).is_err());
    }
    for (key, value) in [(1, 999), (3, 1)] {
        let mut entry = support::sample_named("WindowsFileDescriptor");
        entry.insert(key, V::Uint(value));
        let entries = if key == 3 {
            vec![V::Map(entry.clone()), V::Map(entry)]
        } else {
            vec![V::Map(entry)]
        };
        let mut bad = metadata.clone();
        bad.insert(1, V::Array(entries));
        offer.insert(5, V::Map(bad));
        assert!(CloudLinkOffer::from_raw(offer.clone()).is_err());
    }
    let mut group = support::sample_named("MacPromiseGroup");
    for (key, value) in [
        (1, V::Uint(1)), // promise anchor is not an actual file root
        (2, V::Array(vec![V::Uint(999)])),
        (3, V::Array(vec![])),
    ] {
        let old = group.insert(key, value);
        let mut bad = metadata.clone();
        bad.insert(2, V::Array(vec![V::Map(group.clone())]));
        offer.insert(5, V::Map(bad));
        assert!(CloudLinkOffer::from_raw(offer.clone()).is_err());
        group.insert(key, old.unwrap());
    }
}

#[test]
fn promise_controls_are_optional_bounded_and_unique() {
    let mut group = support::sample_named("MacPromiseGroup");
    group.remove(&4);
    assert!(
        MacPromiseGroup::from_raw(group.clone())
            .unwrap()
            .controls()
            .unwrap()
            .is_none()
    );
    let control = V::Map(
        [
            (1, V::Text("com.apple.NSFilePromiseItemMetaData".into())),
            (2, V::Bytes(b"public.data".to_vec())),
        ]
        .into(),
    );
    group.insert(4, V::Array(vec![control.clone()]));
    let decoded = MacPromiseGroup::from_raw(group.clone()).unwrap();
    assert_eq!(
        decoded.controls().unwrap().unwrap().0[0].bytes().unwrap().0,
        b"public.data"
    );
    group.insert(4, V::Array(vec![control.clone(), control]));
    assert!(MacPromiseGroup::from_raw(group.clone()).is_err());
    for bytes in [V::Bytes(vec![0; 65537]), V::Text("wrong medium".into())] {
        group.insert(
            4,
            V::Array(vec![V::Map([(1, V::Text("type".into())), (2, bytes)].into())]),
        );
        assert!(MacPromiseGroup::from_raw(group.clone()).is_err());
    }
}

#[test]
fn symlink_node_roundtrips_only_with_negotiated_capability() {
    use v1::{
        admission::{Agreement, Context, Phase},
        registry::Transport,
    };
    let mut node = support::sample_named("FileNode");
    node.insert(1, V::Uint(2));
    node.insert(2, V::Uint(1));
    node.insert(4, V::Uint(2));
    node.insert(
        5,
        V::Map([(1, V::Uint(1)), (2, V::Map([(1, V::Uint(18))].into()))].into()),
    );
    node.insert(8, V::Text("nested/content.txt".into()));
    let link = FileNode::from_raw(node.clone()).unwrap();
    assert_eq!(link.symlink_target().unwrap().unwrap().0, "nested/content.txt");
    let mut page = support::sample_named("DirectoryPage");
    page.insert(1, V::Uint(1));
    page.insert(3, V::Array(vec![V::Map(node.clone())]));
    page.remove(&4);
    let mut directory = support::sample_named("ContentDirectory");
    directory.insert(2, V::Map(page));
    let message = Message::ContentDirectory(ContentDirectory::from_raw(directory).unwrap());
    let bytes = v1::frame::encode(&message, 65536).unwrap();
    let parse = || v1::frame::OpaqueFrame::parse_exact(&bytes, 65536).unwrap();
    let old = Agreement::new(["oink.clipboard/1", "oink.low/1"].map(str::to_owned)).unwrap();
    let enabled =
        Agreement::new(["oink.clipboard/1", "oink.low/1", "oink.files.safe-symlinks/1"].map(str::to_owned)).unwrap();
    let context = Context {
        kind: 4,
        transport: Transport::Low,
        phase: Phase::Member,
        agreement: &old,
        direct: true,
        old_member_lane: false,
    };
    assert_eq!(
        context.decode(parse()).unwrap_err().code,
        ErrorCode::UnnegotiatedCapability
    );
    assert_eq!(
        Context {
            agreement: &enabled,
            ..context
        }
        .decode(parse())
        .unwrap(),
        message
    );
    for target in ["", "/absolute", "bad\0target"] {
        node.insert(8, V::Text(target.into()));
        assert!(FileNode::from_raw(node.clone()).is_err());
    }
    node.insert(4, V::Uint(0));
    node.insert(8, V::Text("target".into()));
    assert!(FileNode::from_raw(node).is_err());
}

#[test]
fn executable_bits_are_additive_and_require_their_own_capability() {
    use v1::{
        admission::{Agreement, Context, Phase},
        registry::Transport,
    };
    let mut raw = support::sample_named("FileNode");
    raw.insert(9, V::Uint(0o111));
    let entry = FileNode::from_raw(raw.clone()).unwrap();
    let mut selected = support::sample_named("ContentFileSelected");
    selected.insert(2, entry.write());
    let message = Message::ContentFileSelected(ContentFileSelected::from_raw(selected).unwrap());
    let bytes = v1::frame::encode(&message, 65536).unwrap();
    for (caps, enabled) in [
        (
            vec!["oink.clipboard/1", "oink.low/1", "oink.files.safe-symlinks/1"],
            false,
        ),
        (vec!["oink.clipboard/1", "oink.low/1", "oink.files.exec-bits/1"], true),
    ] {
        let agreement = Agreement::new(caps.into_iter().map(str::to_owned)).unwrap();
        let context = Context {
            kind: 4,
            transport: Transport::Low,
            phase: Phase::Member,
            agreement: &agreement,
            direct: true,
            old_member_lane: false,
        };
        let parsed = v1::frame::OpaqueFrame::parse_exact(&bytes, 65536).unwrap();
        if enabled {
            assert_eq!(context.decode(parsed).unwrap(), message);
        } else {
            assert_eq!(
                context.decode(parsed).unwrap_err().code,
                ErrorCode::UnnegotiatedCapability
            );
        }
    }
    for bits in [0o002, 0o755, 0o4755, 0xffff] {
        raw.insert(9, V::Uint(bits));
        assert!(FileNode::from_raw(raw.clone()).is_err());
    }
    raw.insert(9, V::Uint(0));
    raw.insert(4, V::Uint(1));
    assert!(FileNode::from_raw(raw).is_err());
}

#[test]
fn batch_root_links_roundtrip_declarations_but_grants_require_capability() {
    use v1::{
        admission::{Agreement, Context, Phase},
        registry::Transport,
    };
    let mut node = support::sample_named("FileNode");
    node.remove(&2);
    node.insert(4, V::Uint(2));
    node.insert(
        5,
        V::Map([(1, V::Uint(1)), (2, V::Map([(1, V::Uint(10))].into()))].into()),
    );
    node.insert(8, V::Text("Other/file".into()));
    let link = FileNode::from_raw(node.clone()).unwrap();
    assert_eq!(link.parent().unwrap(), None);
    let mut inventory = support::sample_named("Inventory");
    inventory.insert(4, V::Array(vec![V::Map(node.clone())]));
    let mut offer = support::sample_named("ClipboardOffer");
    offer.insert(6, V::Map(inventory));
    let message = Message::ClipboardOffer(ClipboardOffer::from_raw(offer).unwrap());
    let old = Agreement::new(["oink.clipboard/1", "oink.low/1", "oink.cloud-file-links/1"].map(str::to_owned)).unwrap();
    let enabled = Agreement::new(
        [
            "oink.clipboard/1",
            "oink.low/1",
            "oink.cloud-file-links/1",
            "oink.files.safe-symlinks/1",
        ]
        .map(str::to_owned),
    )
    .unwrap();
    let context = Context {
        kind: 4,
        transport: Transport::Reliable,
        phase: Phase::Member,
        agreement: &old,
        direct: true,
        old_member_lane: false,
    };
    let bytes = v1::frame::encode(&message, 65536).unwrap();
    assert_eq!(
        context
            .decode(v1::frame::OpaqueFrame::parse_exact(&bytes, 65536).unwrap())
            .unwrap(),
        message
    );
    let mut grant = support::sample_named("CloudLinkOffer");
    grant.insert(3, V::Array(vec![V::Map(node)]));
    let message = Message::CloudLinkOffer(CloudLinkOffer::from_raw(grant).unwrap());
    let bytes = v1::frame::encode(&message, 65536).unwrap();
    let context = Context {
        transport: Transport::Low,
        ..context
    };
    assert_eq!(
        context
            .decode(v1::frame::OpaqueFrame::parse_exact(&bytes, 65536).unwrap())
            .unwrap_err()
            .code,
        ErrorCode::UnnegotiatedCapability
    );
    assert_eq!(
        Context {
            agreement: &enabled,
            ..context
        }
        .decode(v1::frame::OpaqueFrame::parse_exact(&bytes, 65536).unwrap())
        .unwrap(),
        message
    );
}

#[test]
fn aead_protected_content_and_receipts_do_not_require_content_hash_fields() {
    // These keys were removed for 0.3.0, not made optional or replaced by zeros.
    for (name, removed) in [
        ("ContentFileBlock", 5),
        ("ContentRepresentationEnd", 4),
        ("ContentReadFile", 5),
        ("CloudLinkRead", 6),
        ("RepresentationReader", 2),
        ("ClipboardOffer", 5),
        ("DiagnosticsSegmentEnd", 5),
    ] {
        let schema = support::schema(name);
        assert!(schema.fields.iter().all(|f| f.key != removed), "{name}");
        let raw = support::sample_named(name);
        assert!(!raw.contains_key(&removed), "{name}");
        v1::registry::validate_schema(name, &raw).unwrap();
        let encoded = v1::cbor::encode_map(&raw, v1::limits::LOW_FRAME).unwrap();
        v1::registry::validate_schema(name, &v1::cbor::decode_map(&encoded, v1::limits::LOW_FRAME).unwrap()).unwrap();
    }
}

#[test]
fn layout_receipt_only_echoes_group_and_acceptance() {
    assert!(LayoutProfilesReceipt::SCHEMA.fields.iter().all(|f| f.key != 2));
    for accepted in [false, true] {
        let receipt =
            LayoutProfilesReceipt::from_raw([(1, V::Bytes(vec![1; 16])), (3, accepted.write())].into()).unwrap();
        assert_eq!(receipt.accepted().unwrap(), accepted);
        assert!(!receipt.raw().contains_key(&2));
    }
}
