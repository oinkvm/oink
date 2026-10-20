use oink_protocol::v1::{
    ErrorCode, RawValue as V,
    dto::*,
    schema::{Open, WireDto, WireValue},
};
use oink_test_support::protocol as support;

#[test]
fn unknown_transition_is_retained_without_becoming_a_release() {
    let state = DragState::from_raw([(1, V::Uint(400)), (2, support::synthetic_unknown())].into()).unwrap();
    assert!(matches!(state.variant().unwrap(), Open::Unknown { id: 400, .. }));
    let mut raw = support::sample_named("DndNotice");
    raw.insert(4, state.write());
    let notice = DndNotice::from_raw(raw).unwrap();
    assert_eq!(notice.state().unwrap(), state);
}
#[test]
fn drag_notice_binds_native_generation_and_inventory() {
    let mut raw = support::sample_named("DndNotice");
    let V::Map(key) = raw.get_mut(&1).unwrap() else {
        panic!()
    };
    key.insert(2, V::Uint(0));
    assert_eq!(DndNotice::from_raw(raw).unwrap_err().code, ErrorCode::StateConflict);
    let mut raw = support::sample_named("DndNotice");
    let V::Map(inventory) = raw.get_mut(&7).unwrap() else {
        panic!()
    };
    let V::Map(id) = inventory.get_mut(&1).unwrap() else {
        panic!()
    };
    id.insert(2, V::Uint(9));
    assert_eq!(DndNotice::from_raw(raw).unwrap_err().code, ErrorCode::StateConflict);
}
#[test]
fn attachment_remainder_is_metadata_only_and_counted() {
    for kind in [1, 400] {
        let mut rep = support::sample_named("Representation");
        rep.insert(
            7,
            V::Map([(1, V::Uint(kind)), (2, V::Map([(1, V::Bytes(vec![1]))].into()))].into()),
        );
        let mut item = support::sample_named("ContentItem");
        item.insert(3, V::Array(vec![V::Map(rep)]));
        let mut raw = support::sample_named("InventoryRemainder");
        raw.insert(1, V::Array(vec![V::Map(item)]));
        assert_eq!(
            InventoryRemainder::from_raw(raw).unwrap_err().code,
            ErrorCode::UnsupportedValue
        );
    }
    let mut raw = support::sample_named("AttachmentItems");
    raw.insert(5, V::Uint(1));
    assert_eq!(
        AttachmentItems::from_raw(raw).unwrap_err().code,
        ErrorCode::StateConflict
    );
    let mut raw = support::sample_named("AttachmentResult");
    raw.insert(2, V::Uint(1));
    assert!(AttachmentResult::from_raw(raw.clone()).is_err());
    raw.insert(4, V::Map(support::sample_named("DragKey")));
    assert!(AttachmentResult::from_raw(raw).is_ok());
}
#[test]
fn preview_limits_cover_png_pixels_and_logical_hotspot() {
    let mut raw = support::sample_named("DragImage");
    assert!(DragImage::from_raw(raw.clone()).is_ok());
    raw.insert(4, V::F64(513.0));
    assert!(DragImage::from_raw(raw).is_err());
    let mut raw = support::sample_named("DragImage");
    let V::Bytes(png) = raw.get_mut(&1).unwrap() else {
        panic!()
    };
    png[16..20].copy_from_slice(&513_u32.to_be_bytes());
    assert!(DragImage::from_raw(raw).is_err());
}

#[test]
fn large_promise_metadata_can_use_the_attachment_remainder() {
    let mut representation = support::sample_named("Representation");
    let metadata = vec![3; 65536];
    let promise = RepresentationData::from_raw(
        [
            (1, V::Uint(4)),
            (2, V::Map([(1, V::Uint(7)), (2, V::Bytes(metadata.clone()))].into())),
        ]
        .into(),
    )
    .unwrap();
    representation.insert(7, promise.write());
    let mut item = support::sample_named("ContentItem");
    item.insert(3, V::Array(vec![V::Map(representation)]));
    let remainder =
        InventoryRemainder::from_raw([(1, V::Array(vec![V::Map(item)])), (2, V::Array(vec![]))].into()).unwrap();
    let mut items = support::sample_named("AttachmentItems");
    items.insert(2, remainder.write());
    items.insert(5, V::Uint(1));
    let message = oink_protocol::v1::Message::AttachmentItems(AttachmentItems::from_raw(items).unwrap());
    let bytes = oink_protocol::v1::frame::encode(&message, oink_protocol::v1::limits::LOW_FRAME).unwrap();
    assert!(bytes.len() > oink_protocol::v1::limits::RELIABLE_FRAME);
    assert_eq!(
        oink_protocol::v1::frame::OpaqueFrame::parse_exact(&bytes, oink_protocol::v1::limits::LOW_FRAME)
            .unwrap()
            .decode()
            .unwrap(),
        message
    );
    let decoded = RepresentationPromise::from_raw(promise.body().unwrap()).unwrap();
    assert_eq!(decoded.bytes().unwrap().0, metadata);
}
