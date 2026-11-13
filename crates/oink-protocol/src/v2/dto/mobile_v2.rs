//! v2 mobile clipboard and endpoint DTOs.
//!
//! These are retaining wire views.  Paths, SQLite cursors and native handles
//! deliberately do not cross this boundary; operation/item/representation IDs
//! are correlation identities only.
use super::*;
use crate::v2::schema::{defaulted, no_validation, open_enum, required, wire_map};
use crate::v2::{ErrorCode, RawMap, RawValue, Result};

open_enum!(ContentKind { Text=0, Url=1, Image=2, File=3, Folder=4, Html=5, Rtf=6 });
open_enum!(ContentAvailability { Ready=0, SourceOffline=1, Evicted=2, NoSuchItem=3, Unsupported=4 });
open_enum!(CurrentResultStatus { Ready=0, Empty=1, SourceOffline=2, Expired=3, Unsupported=4 });
open_enum!(ShareKind { Text=0, Url=1, Image=2, File=3, Folder=4, Html=5, Rtf=6 });
open_enum!(ShareStatus { Accepted=0, Complete=1, Failed=2, Cancelled=3, SourceOffline=4, Evicted=5, Unsupported=6 });
// Why a receiver refused a mobile share before accepting it. ReceiveOff,
// Paused and Busy describe that receiver only: a directed share to a PC moves
// to the phone's next PC candidate.
open_enum!(ShareRejectReason { ReceiveOff=0, Paused=1, Busy=2, TooLarge=3, Duplicate=4, Invalid=5 });
// How a phone sends a share (protocol-v2 5.2). Directed: to the one device the
// user chose (a PC keeps it and announces it to the group; a phone keeps it).
// Broadcast: text the phone sends to every online device itself (each keeps it,
// nobody forwards it). The receiver acts on this and on its own kind only.
open_enum!(ShareDelivery { Directed=0, Broadcast=1 });
open_enum!(DeviceKind { Desktop=0, Mobile=1 });
// Physical shape of a mobile device, for presentation only (iPhone vs iPad badge).
open_enum!(FormFactor { Phone=0, Tablet=1 });
open_enum!(EndpointDirectionKind { MicrophoneSend=0, MicrophoneReceive=1, SpeakerSend=2, SpeakerReceive=3 });
open_enum!(EndpointPhase { Off=0, Starting=1, Running=2, Interrupted=3, Failed=4 });
// AwaitingDemand: a microphone send is on but no receiver demands it yet.
open_enum!(EndpointCloseReason { None=0, Local=1, Remote=2, Idle=3, Interrupted=4, Error=5, PermissionRequired=6, AwaitingDemand=7 });
// Why an endpoint refused or did not complete a Request/Release/Close (M22,
// pause audit 2). Expired and Declined answer an accepted Android activation
// the user did not confirm in time or turned down.
open_enum!(EndpointRefusal { None=0, Unsupported=1, HelperUnavailable=2, Busy=3, Duplicate=4, Stale=5, Expired=6, Declined=7, PermissionRequired=8, Invalid=9, Unavailable=10, Failed=11 });

// Keys 15+16 (0.4.0, no compatibility): the displayed pixel size of an image
// a phone sends (orientation applied), so a receiving phone shows it at its
// ratio before any byte arrives. Both or neither, positive, images only.
wire_map!(ContentOffer, validate_content_offer, {
    1 => offer: OfferId = required;
    2 => operation_id: Id16 = required;
    3 => message_id: Id16 = required;
    4 => item_id: u32 = required;
    5 => representation_id: u32 = required;
    6 => source: Boot = required;
    7 => real_sender: DeviceId = required;
    8 => proxy: Option<DeviceId> = optional;
    9 => kind: ContentKind = required;
    10 => mime: Text<128> = required;
    11 => display_name: Option<Text<512>> = optional;
    12 => bytes: Option<u64> = optional;
    13 => availability: ContentAvailability = required;
    14 => created_ms: u64 = required;
    15 => image_width: Option<u32> = optional;
    16 => image_height: Option<u32> = optional;
});

fn validate_content_offer(raw: &RawMap) -> Result<()> {
    let width = defaulted::<Option<u32>>(raw, 15, || None)?;
    let height = defaulted::<Option<u32>>(raw, 16, || None)?;
    match (width, height) {
        (None, None) => Ok(()),
        (Some(width), Some(height)) => {
            if width == 0 || height == 0 || required::<ContentKind>(raw, 9)? != ContentKind::Image {
                return Err(ErrorCode::UnsupportedValue.into());
            }
            Ok(())
        }
        _ => Err(ErrorCode::MalformedFrame.into()),
    }
}
/// Registry alias used by the mobile content stream.  Both names carry the
/// same schema; keeping the alias avoids a second DTO or wire framing type.
pub type ClipboardContentOffer = ContentOffer;

wire_map!(ClipboardCurrentQuery, no_validation, {
    1 => request_id: Id16 = required;
    2 => group: Id16 = required;
    3 => caller: Boot = required;
    4 => max_items: u16 = required;
});
wire_map!(ClipboardCurrentResult, no_validation, {
    1 => request_id: Id16 = required;
    2 => responder: Boot = required;
    3 => status: CurrentResultStatus = required;
    4 => offer: Option<ClipboardOffer> = optional;
    5 => source: Option<DeviceId> = optional;
    6 => reason_code: Option<u16> = optional;
    7 => clock: u64 = required;
});
wire_map!(ClipboardHeadUpdate, no_validation, {
    1 => group: Id16 = required;
    2 => responder: Boot = required;
    3 => offer: ClipboardOffer = required;
    4 => fresh: bool = required;
});

wire_map!(ClipboardShareBegin, no_validation, {
    1 => operation_id: Id16 = required;
    2 => message_id: Id16 = required;
    3 => origin_sender: DeviceId = required;
    4 => delivery: ShareDelivery = required;
    5 => kind: ShareKind = required;
    6 => item_count: u16 = required;
    7 => bytes: Option<u64> = optional;
    8 => inventory: Option<Inventory> = optional;
    9 => source: Boot = required;
    10 => target: Option<Boot> = optional;
    11 => sensitivity: Option<Sensitivity> = optional;
});
wire_map!(ClipboardShareAccept, no_validation, { 1 => operation_id: Id16 = required; });
wire_map!(ClipboardShareReject, no_validation, {
    1 => operation_id: Id16 = required;
    2 => status: ShareStatus = required;
    3 => reason: Option<ShareRejectReason> = optional;
});
wire_map!(ClipboardShareResult, no_validation, {
    1 => operation_id: Id16 = required;
    2 => status: ShareStatus = required;
    3 => reason_code: Option<u16> = optional;
});
wire_map!(ClipboardShareCancel, no_validation, { 1 => operation_id: Id16 = required; });
wire_map!(ClipboardShareTextChunk, no_validation, {
    1 => operation_id: Id16 = required;
    2 => offset: u64 = required;
    3 => bytes: Bytes<4096> = required;
});
wire_map!(ClipboardShareTextEnd, no_validation, { 1 => operation_id: Id16 = required; });
wire_map!(ClipboardContentRead, no_validation, {
    1 => operation_id: Id16 = required;
    2 => offer: OfferId = required;
    3 => item_id: u32 = required;
    4 => representation_id: u32 = required;
});
// Image preview for a phone that did not download the image itself
// (protocol-v2 §4.4). The target is exactly one of a bitmap representation
// (keys 3+4, the `ClipboardContentRead` slot) or a namespace file (keys 5+6,
// the `ContentReadFile` node/version). One request, one `ContentThumbnail`.
wire_map!(ContentThumbnailRead, validate_thumbnail_read, {
    1 => operation_id: Id16 = required;
    2 => offer: OfferId = required;
    3 => item_id: Option<u32> = optional;
    4 => representation_id: Option<u32> = optional;
    5 => node: Option<u64> = optional;
    6 => version: Option<u64> = optional;
    7 => max_edge: u16 = required;
    8 => max_bytes: u32 = required;
});
open_enum!(ThumbnailStatus { Ready=0, Expired=1, Unsupported=2, Failed=3 });
wire_map!(ContentThumbnail, validate_thumbnail, {
    1 => operation_id: Id16 = required;
    2 => status: ThumbnailStatus = required;
    3 => mime: Option<Text<32>> = optional;
    4 => width: Option<u32> = optional;
    5 => height: Option<u32> = optional;
    6 => bytes: Option<Bytes<THUMBNAIL_MAX_BYTES>> = optional;
    // Encoded size of the original the phone would download (the PC learns it
    // when it reads the image; Mac/Windows offers announce bitmaps with an
    // unknown size). Only with Ready.
    7 => source_bytes: Option<u64> = optional;
});
/// Bounds of a thumbnail request: the long edge in pixels and the encoded size.
pub const THUMBNAIL_EDGE: std::ops::RangeInclusive<u16> = 64..=1024;
pub const THUMBNAIL_MIN_BYTES: u32 = 4 * 1024;
pub const THUMBNAIL_MAX_BYTES: usize = 512 * 1024;
/// The only encodings a `Ready` thumbnail may carry: JPEG for opaque images,
/// PNG when the image has transparency.
pub const THUMBNAIL_MIMES: [&str; 2] = ["image/jpeg", "image/png"];

fn validate_thumbnail_read(raw: &RawMap) -> Result<()> {
    let present = |key| raw.get(&key).is_some_and(|value| !matches!(value, RawValue::Null));
    let representation = present(3) && present(4);
    let file = present(5) && present(6);
    if representation == file || (representation && (present(5) || present(6))) || (file && (present(3) || present(4)))
    {
        return Err(ErrorCode::MalformedFrame.into());
    }
    let edge: u16 = required(raw, 7)?;
    let bytes: u32 = required(raw, 8)?;
    if !THUMBNAIL_EDGE.contains(&edge) || !(THUMBNAIL_MIN_BYTES..=THUMBNAIL_MAX_BYTES as u32).contains(&bytes) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

fn validate_thumbnail(raw: &RawMap) -> Result<()> {
    let present = |key| raw.get(&key).is_some_and(|value| !matches!(value, RawValue::Null));
    match required::<ThumbnailStatus>(raw, 2)? {
        ThumbnailStatus::Ready => {
            let mime: Text<32> = required(raw, 3)?;
            let width: u32 = required(raw, 4)?;
            let height: u32 = required(raw, 5)?;
            let bytes: Bytes<THUMBNAIL_MAX_BYTES> = required(raw, 6)?;
            if !THUMBNAIL_MIMES.contains(&mime.0.as_str())
                || !(1..=u32::from(*THUMBNAIL_EDGE.end())).contains(&width)
                || !(1..=u32::from(*THUMBNAIL_EDGE.end())).contains(&height)
                || bytes.0.is_empty()
            {
                return Err(ErrorCode::UnsupportedValue.into());
            }
        }
        // Every other status, including a future one, carries no image.
        _ if present(3) || present(4) || present(5) || present(6) || present(7) => {
            return Err(ErrorCode::MalformedFrame.into());
        }
        _ => {}
    }
    Ok(())
}

wire_map!(EndpointDirection, no_validation, {
    1 => supported: bool = required;
    // Key 2 (`enabled`) is retired and not reused: whether a phone is the
    // speaker exit or microphone sender is read from the group audio
    // configuration (DeviceFacts key 33) only. This keeps runtime facts.
    3 => phase: EndpointPhase = required;
    4 => generation: u64 = required;
    5 => reason: EndpointCloseReason = required;
    // The remote activation that owns this direction only (M22).
    6 => activation_id: Option<Id16> = optional;
});
wire_map!(AudioEndpointState, no_validation, {
    1 => boot: Boot = required;
    2 => revision: u64 = required;
    3 => microphone_send: EndpointDirection = required;
    4 => microphone_receive: EndpointDirection = required;
    5 => speaker_send: EndpointDirection = required;
    6 => speaker_receive: EndpointDirection = required;
    7 => owner: Option<Boot> = optional;
    // Key 8 (one activation_id for both directions) moved into EndpointDirection.
    9 => generation: u64 = required;
    10 => activity_seq: u64 = required;
    11 => close_reason: EndpointCloseReason = required;
});
wire_map!(AudioEndpointRequest, no_validation, {
    1 => request_id: Id16 = required;
    2 => caller: Boot = required;
    3 => target: Boot = required;
    4 => direction: EndpointDirectionKind = required;
    5 => activation_id: Option<Id16> = optional;
    6 => owner: Boot = required;
});
wire_map!(AudioEndpointRelease, no_validation, {
    1 => request_id: Id16 = required;
    2 => caller: Boot = required;
    3 => target: Boot = required;
    4 => direction: EndpointDirectionKind = required;
    5 => generation: u64 = required;
    6 => activation_id: Option<Id16> = optional;
});
// The endpoint answers every Request/Release once (M22); a refusal is
// explicit instead of an endpoint that stays Starting. 0x061f
// AudioEndpointClose is retired: a PC turns a phone back to local by
// authoring the group audio configuration (Clear(phone)).
wire_map!(AudioEndpointResult, no_validation, {
    1 => request_id: Id16 = required;
    2 => target: Boot = required;
    3 => direction: EndpointDirectionKind = required;
    4 => outcome: ControlOutcome = required;
    5 => reason: EndpointRefusal = required;
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::{RawValue as V, schema::WireDto};

    fn id() -> V {
        V::Bytes(vec![7; 16])
    }
    fn boot(byte: u8) -> V {
        V::Map(
            [
                (1, V::Map([(1, V::from(ED25519)), (2, V::Bytes(vec![byte; 32]))].into())),
                (2, id()),
            ]
            .into(),
        )
    }
    #[test]
    fn current_query_has_bounded_identity_fields() {
        let raw = [(1, id()), (2, id()), (3, boot(1)), (4, V::Uint(8))].into();
        assert!(ClipboardCurrentQuery::from_raw(raw).is_ok());
    }
    #[test]
    fn current_result_requires_a_clock_even_when_empty() {
        use crate::v2::{Message, frame};
        let mut raw: RawMap = [(1, id()), (2, boot(1)), (3, V::Uint(1))].into();
        assert!(ClipboardCurrentResult::from_raw(raw.clone()).is_err());
        raw.insert(7, V::Uint(42));
        let result = ClipboardCurrentResult::from_raw(raw).unwrap();
        assert_eq!(result.clock().unwrap(), 42);
        let bytes = frame::encode(
            &Message::ClipboardCurrentResult(result),
            crate::v2::limits::RELIABLE_FRAME,
        )
        .unwrap();
        // Empty has no offer, but key 7 is still a mandatory publication barrier.
        assert_eq!(&bytes[bytes.len() - 3..], &[7, 0x18, 42]);
        let Message::ClipboardCurrentResult(decoded) =
            frame::OpaqueFrame::parse_exact(&bytes, crate::v2::limits::RELIABLE_FRAME)
                .unwrap()
                .decode()
                .unwrap()
        else {
            panic!("wrong message")
        };
        assert_eq!(decoded.status().unwrap(), CurrentResultStatus::Empty);
        assert_eq!(decoded.clock().unwrap(), 42);
    }
    fn offer() -> V {
        let device = V::Map([(1, V::from(ED25519)), (2, V::Bytes(vec![3; 32]))].into());
        V::Map([(1, V::Map([(1, device), (2, V::Uint(9))].into())), (2, V::Uint(4))].into())
    }
    fn read(target: &[(u32, u64)], edge: u64, bytes: u64) -> crate::v2::Result<ContentThumbnailRead> {
        let mut raw: RawMap = [(1, id()), (2, offer()), (7, V::Uint(edge)), (8, V::Uint(bytes))].into();
        raw.extend(target.iter().map(|(key, value)| (*key, V::Uint(*value))));
        ContentThumbnailRead::from_raw(raw)
    }
    fn content_offer(kind: u64, size: &[(u32, u64)]) -> crate::v2::Result<ContentOffer> {
        let device = V::Map([(1, V::from(ED25519)), (2, V::Bytes(vec![3; 32]))].into());
        let mut raw: RawMap = [
            (1, offer()),
            (2, id()),
            (3, id()),
            (4, V::Uint(0)),
            (5, V::Uint(0)),
            (6, boot(3)),
            (7, device),
            (9, V::Uint(kind)),
            (10, V::from("image/heic")),
            (13, V::Uint(0)),
            (14, V::Uint(1)),
        ]
        .into();
        raw.extend(size.iter().map(|(key, value)| (*key, V::Uint(*value))));
        ContentOffer::from_raw(raw)
    }
    /// A phone's image offer may carry the original's displayed size (keys
    /// 15/16): both or neither, positive, on images only.
    #[test]
    fn content_offer_carries_an_image_size_as_a_pair() {
        let sized = content_offer(2, &[(15, 3024), (16, 4032)]).unwrap();
        assert_eq!(
            (sized.image_width().unwrap(), sized.image_height().unwrap()),
            (Some(3024), Some(4032))
        );
        let plain = content_offer(2, &[]).unwrap();
        assert_eq!(
            (plain.image_width().unwrap(), plain.image_height().unwrap()),
            (None, None)
        );
        for half in [&[(15, 3024)][..], &[(16, 4032)]] {
            assert_eq!(
                content_offer(2, half).unwrap_err().code,
                ErrorCode::MalformedFrame,
                "{half:?}"
            );
        }
        for bad in [&[(15, 0), (16, 4032)][..], &[(15, 3024), (16, 0)]] {
            assert_eq!(
                content_offer(2, bad).unwrap_err().code,
                ErrorCode::UnsupportedValue,
                "{bad:?}"
            );
        }
        // A file has no image size.
        assert_eq!(
            content_offer(3, &[(15, 3024), (16, 4032)]).unwrap_err().code,
            ErrorCode::UnsupportedValue
        );
    }
    #[test]
    fn thumbnail_read_names_exactly_one_target_within_bounds() {
        let rep = read(&[(3, 0), (4, 1)], 512, 256 * 1024).unwrap();
        assert_eq!(
            (rep.item_id().unwrap(), rep.representation_id().unwrap()),
            (Some(0), Some(1))
        );
        assert_eq!(rep.node().unwrap(), None);
        let file = read(&[(5, 2), (6, 7)], 512, 256 * 1024).unwrap();
        assert_eq!((file.node().unwrap(), file.version().unwrap()), (Some(2), Some(7)));
        for target in [
            &[][..],
            &[(3, 0)],
            &[(5, 2)],
            &[(3, 0), (4, 1), (5, 2), (6, 7)],
            &[(3, 0), (4, 1), (6, 7)],
        ] {
            assert_eq!(
                read(target, 512, 4096).unwrap_err().code,
                ErrorCode::MalformedFrame,
                "{target:?}"
            );
        }
        for (edge, bytes) in [(63, 4096), (1025, 4096), (512, 4095), (512, 512 * 1024 + 1)] {
            assert_eq!(
                read(&[(3, 0), (4, 1)], edge, bytes).unwrap_err().code,
                ErrorCode::UnsupportedValue
            );
        }
    }
    #[test]
    fn thumbnail_reply_carries_an_image_only_when_ready() {
        let ready = |mime: &str, bytes: usize| {
            ContentThumbnail::from_raw(
                [
                    (1, id()),
                    (2, V::Uint(0)),
                    (3, V::Text(mime.into())),
                    (4, V::Uint(512)),
                    (5, V::Uint(288)),
                    (6, V::Bytes(vec![1; bytes])),
                ]
                .into(),
            )
        };
        let reply = ready("image/jpeg", 1000).unwrap();
        assert_eq!(reply.status().unwrap(), ThumbnailStatus::Ready);
        assert_eq!(
            (reply.width().unwrap(), reply.height().unwrap()),
            (Some(512), Some(288))
        );
        assert!(ready("image/png", THUMBNAIL_MAX_BYTES).is_ok());
        assert_eq!(ready("image/gif", 10).unwrap_err().code, ErrorCode::UnsupportedValue);
        assert_eq!(ready("image/png", 0).unwrap_err().code, ErrorCode::UnsupportedValue);
        assert_eq!(
            ready("image/png", THUMBNAIL_MAX_BYTES + 1).unwrap_err().code,
            ErrorCode::LimitExceeded
        );
        let expired = ContentThumbnail::from_raw([(1, id()), (2, V::Uint(1))].into()).unwrap();
        assert_eq!(expired.status().unwrap(), ThumbnailStatus::Expired);
        assert_eq!(
            ContentThumbnail::from_raw([(1, id()), (2, V::Uint(1)), (6, V::Bytes(vec![1]))].into())
                .unwrap_err()
                .code,
            ErrorCode::MalformedFrame
        );
        assert_eq!(
            ContentThumbnail::from_raw([(1, id()), (2, V::Uint(2)), (7, V::Uint(9))].into())
                .unwrap_err()
                .code,
            ErrorCode::MalformedFrame,
            "the original's size only accompanies a thumbnail"
        );
        // Ready without an image is malformed.
        assert!(ContentThumbnail::from_raw([(1, id()), (2, V::Uint(0))].into()).is_err());
    }
    #[test]
    fn thumbnail_messages_are_b0_low_variants_of_the_one_message_enum() {
        use crate::v2::{
            Message,
            admission::{Agreement, Context, Phase},
            frame::OpaqueFrame,
            registry::Transport,
        };
        let message = Message::ContentThumbnailRead(read(&[(5, 2), (6, 7)], 512, 65536).unwrap());
        assert_eq!(message.type_id(), 0x0354);
        let reply = Message::ContentThumbnail(ContentThumbnail::from_raw([(1, id()), (2, V::Uint(1))].into()).unwrap());
        assert_eq!(reply.type_id(), 0x0355);
        let agreement = Agreement::new(
            ["oink.core/2", "oink.group/2", "oink.clipboard/2", "oink.low/1"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        for message in [message, reply] {
            let bytes = crate::v2::frame::encode(&message, crate::v2::limits::LOW_FRAME).unwrap();
            let frame = OpaqueFrame::parse_exact(&bytes, crate::v2::limits::LOW_FRAME).unwrap();
            let context = |transport| Context {
                kind: 4,
                transport,
                phase: Phase::Member,
                agreement: &agreement,
                direct: true,
                old_member_lane: false,
            };
            assert_eq!(context(Transport::Low).decode(frame).unwrap(), message);
            assert_eq!(
                context(Transport::Reliable).decode(frame).unwrap_err().code,
                ErrorCode::InvalidChannel
            );
        }
    }
    #[test]
    fn endpoint_direction_no_longer_carries_an_enabled_flag() {
        let raw: RawMap = [(1, V::Bool(true)), (3, V::Uint(2)), (4, V::Uint(6)), (5, V::Uint(0))].into();
        let direction = EndpointDirection::from_raw(raw).unwrap();
        assert!(direction.supported().unwrap());
        assert!(!EndpointDirection::SCHEMA.fields.iter().any(|field| field.key == 2));
        // An earlier 0.4.0 build's key 2 is retained as an unknown key, never read.
        let mut old: RawMap = direction.raw().clone();
        old.insert(2, V::Bool(true));
        assert!(EndpointDirection::from_raw(old).is_ok());
    }
}
