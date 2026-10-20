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
