//! Freeze tests for the v2 wire (protocol-v2 section 9, review M3).
//!
//! Changing a message ID, a field key, a route, a capability or the stream gate
//! table must fail here. An intended wire change is made in the contract first;
//! then `OINK_BLESS=1 cargo test -p oink-protocol --test registry` rewrites the
//! registry snapshot and the golden vectors, and the commit says why.
use oink_protocol::v2::{
    self, Message, RawMap, RawValue as V,
    admission::{Agreement, Context, Phase},
    frame::{self, FrameDecoder, OpaqueFrame, StreamHeader},
    registry::{self, Transport, type_id},
    schema::WireDto,
};
use std::path::PathBuf;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2")
}
fn bless() -> bool {
    std::env::var_os("OINK_BLESS").is_some()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---- registry snapshot ------------------------------------------------------

#[test]
fn registry_matches_the_frozen_snapshot() {
    let current = serde_json::to_string_pretty(&registry::schema_registry()).unwrap() + "\n";
    let path = fixtures().join("registry.json");
    if bless() {
        std::fs::write(&path, &current).unwrap();
        return;
    }
    let Ok(frozen) = std::fs::read_to_string(&path) else {
        std::fs::write(&path, &current).unwrap();
        panic!("created tests/fixtures/v2/registry.json; review it, commit it and run again");
    };
    if frozen != current {
        let line = frozen
            .lines()
            .zip(current.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| frozen.lines().count().min(current.lines().count()));
        panic!(
            "the v2 registry differs from tests/fixtures/v2/registry.json at line {}: {:?} became {:?}. \
             Change protocol-v2.md first, then re-bless (OINK_BLESS=1).",
            line + 1,
            frozen.lines().nth(line),
            current.lines().nth(line),
        );
    }
}

#[test]
fn registry_ids_are_unique_and_never_reuse_reserved_or_retired_numbers() {
    let mut ids: Vec<u16> = registry::MESSAGES.iter().map(|e| e.id).collect();
    let count = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), count, "duplicate message id");
    for (id, name) in registry::RESERVED_MESSAGES {
        assert!(
            registry::message(*id).is_none(),
            "reserved {id:#06x} {name} is registered"
        );
    }
    for id in registry::TOMBSTONES {
        assert!(registry::message(*id).is_none(), "tombstone {id:#06x} is registered");
    }
    // Every route names a stream in the gate table (or discovery) and only
    // capabilities from the capability table.
    for entry in registry::MESSAGES {
        for route in entry.routes {
            assert!(
                route.transport == Transport::Discovery || registry::stream(route.kind).is_some(),
                "{} routes to unknown stream {}",
                entry.name,
                route.kind
            );
            for capability in entry.capabilities.iter().chain(route.extra_capabilities) {
                assert!(
                    registry::CAPABILITIES.contains(capability),
                    "{}: {capability}",
                    entry.name
                );
            }
        }
    }
}

// ---- golden vectors for the 0.4.0 messages (protocol-v2 section 3) -----------

fn device(byte: u8) -> V {
    V::Map([(1, V::from("ed25519")), (2, V::Bytes(vec![byte; 32]))].into())
}
fn id16(byte: u8) -> V {
    V::Bytes(vec![byte; 16])
}
fn boot(byte: u8) -> V {
    V::Map([(1, device(byte)), (2, id16(0x10 | byte))].into())
}
fn offer_id(origin: u8) -> V {
    V::Map(
        [
            (1, V::Map([(1, device(origin)), (2, V::Uint(41))].into())),
            (2, V::Uint(3)),
        ]
        .into(),
    )
}
fn inventory(origin: u8) -> V {
    V::Map(
        [
            (1, offer_id(origin)),
            (2, V::from("macos")),
            (3, V::Array(vec![])),
            (4, V::Array(vec![])),
        ]
        .into(),
    )
}
/// A proxied ClipboardOffer with every field (the proxy announces it).
fn clipboard_offer() -> V {
    V::Map(
        [
            (1, offer_id(2)),
            (2, V::Uint(7)),
            (3, V::Uint(9)),
            (4, V::Uint(0)),
            (6, inventory(2)),
            (7, V::Uint(1_789_000_000_000)),
            (8, device(3)),
            (9, device(2)),
            (10, id16(4)),
            (11, id16(5)),
            (12, V::Uint(0)),
            (13, V::Array(vec![])),
        ]
        .into(),
    )
}
fn direction() -> V {
    // Key 2 (`enabled`) is retired: a direction's mode is the group audio
    // configuration (DeviceFacts key 33), never a second endpoint flag.
    V::Map([(1, V::Bool(true)), (3, V::Uint(2)), (4, V::Uint(6)), (5, V::Uint(0))].into())
}

struct Vector {
    id: u16,
    min: RawMap,
    /// `None` when the message has no optional field.
    max: Option<RawMap>,
}

fn map(fields: &[(u32, V)]) -> RawMap {
    fields.iter().cloned().collect()
}
fn with(base: &RawMap, extra: &[(u32, V)]) -> RawMap {
    let mut raw = base.clone();
    raw.extend(extra.iter().cloned());
    raw
}

fn vectors() -> Vec<Vector> {
    let content_offer = map(&[
        (1, offer_id(1)),
        (2, id16(1)),
        (3, id16(2)),
        (4, V::Uint(0)),
        (5, V::Uint(1)),
        (6, boot(1)),
        (7, device(1)),
        (9, V::Uint(2)),
        (10, V::from("image/png")),
        (13, V::Uint(0)),
        (14, V::Uint(1_789_000_000_000)),
    ]);
    let content_offer_max = with(
        &content_offer,
        &[
            (8, device(4)),
            (11, V::from("photo.png")),
            (12, V::Uint(4096)),
            (15, V::Uint(3024)),
            (16, V::Uint(4032)),
        ],
    );
    let current_result = map(&[(1, id16(1)), (2, boot(2)), (3, V::Uint(1)), (7, V::Uint(42))]);
    let share_begin = map(&[
        (1, id16(1)),
        (2, id16(2)),
        (3, device(3)),
        (4, V::Uint(0)),
        (5, V::Uint(3)),
        (6, V::Uint(1)),
        (9, boot(3)),
    ]);
    let share_status = map(&[(1, id16(1)), (2, V::Uint(6))]);
    let endpoint = |kind: u64| {
        map(&[
            (1, id16(1)),
            (2, boot(1)),
            (3, boot(2)),
            (4, V::Uint(kind)),
            (5, V::Uint(6)),
        ])
    };
    let state = map(&[
        (1, boot(2)),
        (2, V::Uint(4)),
        (3, direction()),
        (4, direction()),
        (5, direction()),
        (6, direction()),
        (9, V::Uint(6)),
        (10, V::Uint(11)),
        (11, V::Uint(0)),
    ]);
    vec![
        Vector {
            id: type_id::ContentOffer,
            min: content_offer.clone(),
            max: Some(content_offer_max.clone()),
        },
        Vector {
            id: type_id::ClipboardCurrentQuery,
            min: map(&[(1, id16(1)), (2, id16(9)), (3, boot(3)), (4, V::Uint(8))]),
            max: None,
        },
        Vector {
            id: type_id::ClipboardCurrentResult,
            min: current_result.clone(),
            max: Some(with(
                &current_result,
                &[(4, clipboard_offer()), (5, device(2)), (6, V::Uint(17))],
            )),
        },
        Vector {
            id: type_id::ClipboardHeadUpdate,
            min: map(&[(1, id16(9)), (2, boot(2)), (3, clipboard_offer()), (4, V::Bool(true))]),
            max: None,
        },
        Vector {
            id: type_id::ClipboardShareBegin,
            min: share_begin.clone(),
            max: Some(with(
                &share_begin,
                &[
                    (7, V::Uint(1 << 20)),
                    (8, inventory(3)),
                    (10, boot(2)),
                    (11, V::Uint(1)),
                ],
            )),
        },
        Vector {
            id: type_id::ClipboardShareAccept,
            min: map(&[(1, id16(1))]),
            max: None,
        },
        Vector {
            id: type_id::ClipboardShareReject,
            min: share_status.clone(),
            max: Some(with(&share_status, &[(3, V::Uint(1))])),
        },
        Vector {
            id: type_id::ClipboardShareResult,
            min: share_status.clone(),
            max: Some(with(&share_status, &[(3, V::Uint(42))])),
        },
        Vector {
            id: type_id::ClipboardShareCancel,
            min: map(&[(1, id16(1))]),
            max: None,
        },
        Vector {
            id: type_id::ClipboardShareTextChunk,
            min: map(&[(1, id16(1)), (2, V::Uint(4096)), (3, V::Bytes(b"oink".to_vec()))]),
            max: None,
        },
        Vector {
            id: type_id::ClipboardShareTextEnd,
            min: map(&[(1, id16(1))]),
            max: None,
        },
        Vector {
            id: type_id::ClipboardContentRead,
            min: map(&[(1, id16(1)), (2, offer_id(2)), (3, V::Uint(0)), (4, V::Uint(2))]),
            max: None,
        },
        Vector {
            id: type_id::ClipboardContentOffer,
            min: content_offer,
            max: Some(content_offer_max),
        },
        Vector {
            id: type_id::ContentThumbnail,
            min: map(&[(1, id16(1)), (2, V::Uint(1))]),
            max: Some(map(&[
                (1, id16(1)),
                (2, V::Uint(0)),
                (3, V::from("image/jpeg")),
                (4, V::Uint(512)),
                (5, V::Uint(288)),
                (6, V::Bytes(vec![0xff, 0xd8, 0xff, 0xd9])),
                (7, V::Uint(6 << 20)),
            ])),
        },
        Vector {
            id: type_id::InputText,
            min: map(&[(1, V::Uint(1)), (2, V::from("你好"))]),
            max: Some(map(&[
                (1, V::Uint(1)),
                (2, V::from("你好")),
                (100, boot(2)),
                (101, V::Uint(1)),
            ])),
        },
        Vector {
            id: type_id::ControlEdgeReport,
            min: map(&[(1, V::Uint(3)), (2, V::Uint(1)), (3, V::Uint(1))]),
            max: Some(map(&[
                (1, V::Uint(3)),
                (2, V::Uint(2)),
                (3, V::Uint(3)),
                (4, device(4)),
            ])),
        },
        Vector {
            id: type_id::ControlTextRefused,
            min: map(&[(1, V::Uint(9)), (2, V::Uint(3))]),
            max: None,
        },
        Vector {
            id: type_id::ControlTextFocus,
            min: map(&[
                (1, V::Uint(3)),
                (2, V::Uint(7)),
                (3, V::Bool(true)),
                (4, V::Bool(false)),
                (5, V::Uint(9)),
            ]),
            max: None,
        },
        Vector {
            id: type_id::AudioEndpointState,
            min: state.clone(),
            max: Some(with(&state, &[(7, boot(1))])),
        },
        Vector {
            id: type_id::AudioEndpointRequest,
            min: map(&[(1, id16(1)), (2, boot(1)), (3, boot(2)), (4, V::Uint(3)), (6, boot(1))]),
            max: Some(map(&[
                (1, id16(1)),
                (2, boot(1)),
                (3, boot(2)),
                (4, V::Uint(3)),
                (5, id16(7)),
                (6, boot(1)),
            ])),
        },
        Vector {
            id: type_id::AudioEndpointRelease,
            min: endpoint(0),
            max: Some(with(&endpoint(0), &[(6, id16(7))])),
        },
    ]
}

fn encode(id: u16, raw: &RawMap) -> Vec<u8> {
    let message = Message::from_payload(id, raw.clone()).unwrap_or_else(|e| panic!("{id:#06x}: {e:?}"));
    assert_eq!(message.type_id(), id);
    frame::encode(&message, v2::limits::RELIABLE_FRAME).unwrap()
}

/// Compares with the golden file; a missing one is written and reported.
fn check_golden(name: &str, bytes: &[u8], created: &mut Vec<String>) {
    let path = fixtures().join(format!("{name}.hex"));
    if bless() {
        std::fs::write(&path, hex(bytes) + "\n").unwrap();
        return;
    }
    let Ok(golden) = std::fs::read_to_string(&path) else {
        std::fs::write(&path, hex(bytes) + "\n").unwrap();
        created.push(name.to_owned());
        return;
    };
    assert_eq!(hex(bytes), golden.trim(), "{name} changed on the wire");
}

#[test]
fn proxy_share_sensitivity_is_optional_and_roundtrips() {
    use oink_protocol::v2::dto::{ClipboardShareBegin, Sensitivity};
    let vector = vectors()
        .into_iter()
        .find(|v| v.id == type_id::ClipboardShareBegin)
        .unwrap();
    let absent = ClipboardShareBegin::from_raw(vector.min.clone()).unwrap();
    assert_eq!(
        absent.sensitivity().unwrap().unwrap_or(Sensitivity::Normal),
        Sensitivity::Normal
    );
    for value in [Sensitivity::Normal, Sensitivity::Concealed, Sensitivity::Transient] {
        let mut raw = vector.min.clone();
        raw.insert(11, V::Uint(value.number().into()));
        let bytes = encode(vector.id, &raw);
        let Message::ClipboardShareBegin(decoded) = OpaqueFrame::parse_exact(&bytes, v2::limits::RELIABLE_FRAME)
            .unwrap()
            .decode()
            .unwrap()
        else {
            panic!("share begin");
        };
        assert_eq!(decoded.sensitivity().unwrap(), Some(value));
    }
}

#[test]
fn new_messages_match_min_and_max_golden_vectors() {
    let mut created = Vec::new();
    for vector in vectors() {
        let entry = registry::message(vector.id).unwrap();
        let required: Vec<u32> = entry
            .schema
            .fields
            .iter()
            .filter(|f| f.absence == "required")
            .map(|f| f.key)
            .collect();
        let all: Vec<u32> = entry.schema.fields.iter().map(|f| f.key).collect();
        let keys = |raw: &RawMap| raw.keys().copied().collect::<Vec<_>>();
        assert_eq!(
            keys(&vector.min),
            required,
            "{} min must hold exactly the required keys",
            entry.name
        );
        match &vector.max {
            Some(max) => assert_eq!(keys(max), all, "{} max must hold every key", entry.name),
            None => assert_eq!(required, all, "{} has optional keys; add a max vector", entry.name),
        }
        for (suffix, raw) in [("min", Some(&vector.min)), ("max", vector.max.as_ref())] {
            let Some(raw) = raw else { continue };
            let bytes = encode(vector.id, raw);
            check_golden(&format!("{:04x}-{suffix}", vector.id), &bytes, &mut created);
            let decoded = OpaqueFrame::parse_exact(&bytes, v2::limits::RELIABLE_FRAME)
                .unwrap()
                .decode()
                .unwrap();
            assert_eq!(decoded.raw(), raw, "{} round trip", entry.name);
        }
    }
    assert!(
        created.is_empty(),
        "created goldens {created:?}; review, commit and run again"
    );
}

/// `ContentThumbnailRead` names exactly one target (protocol-v2 §4.4), so it
/// has neither a required-keys-only nor an every-key vector; each target form
/// has its own golden.
#[test]
fn thumbnail_read_matches_its_golden_vectors() {
    let mut created = Vec::new();
    let base = [
        (1, id16(1)),
        (2, offer_id(2)),
        (7, V::Uint(512)),
        (8, V::Uint(256 * 1024)),
    ];
    for (suffix, target) in [
        ("representation", [(3, V::Uint(0)), (4, V::Uint(2))]),
        ("file", [(5, V::Uint(7)), (6, V::Uint(3))]),
    ] {
        let raw: RawMap = base.iter().cloned().chain(target).collect();
        let bytes = encode(type_id::ContentThumbnailRead, &raw);
        check_golden(&format!("0354-{suffix}"), &bytes, &mut created);
        let decoded = OpaqueFrame::parse_exact(&bytes, v2::limits::RELIABLE_FRAME)
            .unwrap()
            .decode()
            .unwrap();
        assert_eq!(decoded.raw(), &raw, "ContentThumbnailRead round trip");
    }
    assert!(
        created.is_empty(),
        "created goldens {created:?}; review, commit and run again"
    );
}

// ---- canonical CBOR -----------------------------------------------------------

#[test]
fn canonical_cbor_rejects_non_canonical_encodings() {
    let good = encode(type_id::ClipboardShareAccept, &map(&[(1, id16(1))]));
    let payload = &good[6..];
    let decode = |payload: &[u8]| {
        let mut bytes = (type_id::ClipboardShareAccept).to_le_bytes().to_vec();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        OpaqueFrame::parse_exact(&bytes, v2::limits::RELIABLE_FRAME).and_then(|f| f.decode())
    };
    assert!(decode(payload).is_ok());
    // a1 01 50 <16 bytes>: the same key with a non-minimal (two-byte) integer head.
    let mut wide_key = vec![0xa1, 0x18, 0x01];
    wide_key.extend_from_slice(&payload[2..]);
    assert!(decode(&wide_key).is_err(), "non-minimal key accepted");
    // An indefinite-length map.
    let mut indefinite = vec![0xbf];
    indefinite.extend_from_slice(&payload[1..]);
    indefinite.push(0xff);
    assert!(decode(&indefinite).is_err(), "indefinite map accepted");
    // Keys out of order: {2: 6, 1: id}.
    let reject = encode(type_id::ClipboardShareReject, &map(&[(1, id16(1)), (2, V::Uint(6))]));
    let mut swapped = vec![0xa2, 0x02, 0x06];
    swapped.extend_from_slice(&reject[6 + 1..reject.len() - 2]);
    let mut bytes = type_id::ClipboardShareReject.to_le_bytes().to_vec();
    bytes.extend_from_slice(&(swapped.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&swapped);
    assert!(
        OpaqueFrame::parse_exact(&bytes, v2::limits::RELIABLE_FRAME)
            .and_then(|f| f.decode())
            .is_err(),
        "unordered keys accepted"
    );
    // Trailing bytes after the payload map.
    let mut trailing = payload.to_vec();
    trailing.push(0x00);
    assert!(decode(&trailing).is_err(), "trailing data accepted");
}

// ---- frame decoding -----------------------------------------------------------

#[test]
fn frame_decoder_splits_concatenated_frames_and_rejects_bad_headers() {
    let a = encode(type_id::ClipboardShareAccept, &map(&[(1, id16(1))]));
    let b = encode(type_id::ClipboardShareCancel, &map(&[(1, id16(2))]));
    let mut stream = a.clone();
    stream.extend_from_slice(&b);
    let mut decoder = FrameDecoder::new(v2::limits::RELIABLE_FRAME);
    let mut seen = Vec::new();
    // Feed one byte at a time: a frame is complete only with its whole payload.
    for byte in &stream {
        let mut input: &[u8] = std::slice::from_ref(byte);
        if let Some(frame) = decoder.next(&mut input, |_| Ok(())).unwrap() {
            seen.push(frame);
        }
        assert!(input.is_empty());
    }
    decoder.finish().unwrap();
    assert_eq!(seen, vec![a.clone(), b]);
    // The admission hook sees each header before the payload is buffered.
    let mut refusing = FrameDecoder::new(v2::limits::RELIABLE_FRAME);
    let mut input = &a[..];
    assert!(
        refusing
            .next(&mut input, |_| Err(v2::ErrorCode::UnknownMessage.into()))
            .is_err()
    );
    // A failed decoder stays failed.
    assert!(refusing.next(&mut &a[..], |_| Ok(())).is_err());
    // Reserved type ids 0 and 0xffff, an empty payload and an oversize length.
    for header in [
        [0u8, 0, 1, 0, 0, 0],
        [0xff, 0xff, 1, 0, 0, 0],
        [0x4a, 0x03, 0, 0, 0, 0],
        [0x4a, 0x03, 0xff, 0xff, 0xff, 0x00],
    ] {
        let mut decoder = FrameDecoder::new(v2::limits::RELIABLE_FRAME);
        assert!(
            decoder.next(&mut &header[..], |_| Ok(())).is_err(),
            "{header:02x?} accepted"
        );
    }
    // A truncated frame is not complete at the end of the stream.
    let mut partial = FrameDecoder::new(v2::limits::RELIABLE_FRAME);
    assert_eq!(partial.next(&mut &a[..a.len() - 1], |_| Ok(())).unwrap(), None);
    assert!(partial.finish().is_err());
    assert!(OpaqueFrame::parse_exact(&a[..a.len() - 1], v2::limits::RELIABLE_FRAME).is_err());
}

// ---- negotiation --------------------------------------------------------------

fn offer(boot_byte: u8, capabilities: &[&str], required: &[&str]) -> Vec<u8> {
    let caps = |list: &[&str]| {
        let mut list = list.to_vec();
        list.sort_unstable();
        V::Array(list.into_iter().map(V::from).collect())
    };
    let strings = |list: &[&str]| V::Array(list.iter().map(|c| V::from(*c)).collect());
    let connection = V::Map([(1, boot(1)), (2, id16(0x40))].into());
    let limits = V::Map(
        [16384u64, 131072, 16777216, 1100, 122880, 16384]
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index as u32 + 1, V::Uint(value)))
            .collect(),
    );
    let crypto = V::Map(
        [
            (1, strings(&[v2::dto::PAIRING_SUITE])),
            (2, strings(&[v2::dto::ADMISSION_SUITE])),
            (3, strings(&[v2::dto::ED25519])),
        ]
        .into(),
    );
    let formats = V::Map((1..=4).map(|key| (key, V::Array(vec![]))).collect());
    let raw = map(&[
        (1, boot(boot_byte)),
        (2, connection),
        (3, V::Array(vec![V::Map([(1, V::Uint(2)), (2, V::Uint(0))].into())])),
        (4, caps(capabilities)),
        (5, caps(required)),
        (6, limits),
        (7, crypto),
        (8, V::Bytes(vec![boot_byte; 32])),
        (9, V::from("0.4.0")),
        (10, formats),
        (11, V::Array(vec![])),
    ]);
    encode(type_id::ConnectionOffer, &raw)
}

#[test]
fn negotiation_intersects_implemented_capabilities_and_enforces_requirements() {
    let all: Vec<&str> = registry::CAPABILITIES.to_vec();
    let phone = [
        "oink.core/2",
        "oink.group/2",
        "oink.clipboard/2",
        "oink.input/2",
        "made-up/1",
    ];
    let pc = offer(1, &all, &[]);
    let negotiated = v2::admission::negotiate(&pc, &offer(2, &phone, &[]), &all, false).unwrap();
    let enabled: Vec<String> = negotiated.enabled().unwrap().0.into_iter().map(|c| c.0).collect();
    assert_eq!(
        enabled,
        ["oink.clipboard/2", "oink.core/2", "oink.group/2", "oink.input/2"]
    );
    // A capability this build does not implement is never enabled by an echo.
    let negotiated =
        v2::admission::negotiate(&pc, &offer(2, &phone, &[]), &["oink.core/2", "oink.group/2"], false).unwrap();
    assert_eq!(negotiated.enabled().unwrap().0.len(), 2);
    // A required capability the other side lacks refuses admission.
    assert_eq!(
        v2::admission::negotiate(&offer(1, &all, &["oink.drag/1"]), &offer(2, &phone, &[]), &all, false)
            .unwrap_err()
            .code,
        v2::ErrorCode::MissingCapability
    );
    // So does a missing baseline capability.
    assert_eq!(
        v2::admission::negotiate(&pc, &offer(2, &["oink.core/2"], &[]), &all, false)
            .unwrap_err()
            .code,
        v2::ErrorCode::MissingCapability
    );
}

// ---- stream gate and admission -----------------------------------------------

#[test]
fn stream_gate_table_is_frozen() {
    let table: Vec<(u16, &str, Vec<&str>)> = registry::STREAMS
        .iter()
        .map(|s| (s.kind, s.name, s.capabilities.to_vec()))
        .collect();
    assert_eq!(
        table,
        vec![
            (0, "N0", vec!["oink.core/2"]),
            (1, "N1", vec!["oink.low/1"]),
            (2, "C0", vec!["oink.group/2"]),
            (3, "I0", vec!["oink.input/2"]),
            (4, "B0", vec!["oink.clipboard/2"]),
            (5, "M0", vec!["oink.audio/2"]),
            (8, "D0", vec!["oink.diagnostics/2"]),
            (9, "Wallpaper", vec!["oink.device/2"]),
            (10, "Dnd", vec!["oink.drag/1"]),
            (11, "Cache", vec!["oink.clipboard/2"]),
            (12, "P0", vec!["oink.layout/2"]),
            (13, "LayoutProfiles", vec!["oink.group/2", "oink.layout/2"]),
        ]
    );
}

#[test]
fn receive_admission_and_send_permission_follow_the_same_registry_rule() {
    let agreement = Agreement::new(["oink.core/2", "oink.group/2", "oink.device/2"].map(String::from)).unwrap();
    let context = |kind: u16| Context {
        kind,
        transport: Transport::Reliable,
        phase: Phase::Member,
        agreement: &agreement,
        direct: true,
        old_member_lane: false,
    };
    for entry in registry::MESSAGES {
        for route in entry.routes.iter().filter(|r| r.transport == Transport::Reliable) {
            // N0/N1 belong to the link layer; only business and C0 streams here.
            let Some(stream) = registry::stream(route.kind).filter(|s| s.reliable && route.kind > 1) else {
                continue;
            };
            let header = frame::FrameHeader {
                type_id: entry.id,
                payload_length: 1,
            };
            let admitted = context(route.kind).admit(header).is_ok();
            let stream_open = context(route.kind)
                .admit_stream(&StreamHeader {
                    kind: route.kind,
                    priority: stream.priority,
                    route_tag: 0,
                    ordinal: 1,
                    datagram_flow: 0,
                })
                .is_ok();
            let permitted = agreement.permits_on(entry.id, route.kind, Transport::Reliable);
            assert_eq!(
                admitted && stream_open,
                permitted,
                "{} on kind {}",
                entry.name,
                route.kind
            );
        }
    }
    assert!(agreement.permits(type_id::ConfigSetLocalOption));
    assert!(!agreement.permits(type_id::ConfigSetExitVolume));
    assert!(!agreement.permits(type_id::ClipboardOffer));
    // Unregistered numbers are refused by both sides.
    assert!(!agreement.permits(0x0343));
    assert_eq!(
        context(4)
            .admit(frame::FrameHeader {
                type_id: 0x0343,
                payload_length: 1
            })
            .unwrap_err()
            .code,
        v2::ErrorCode::UnknownMessage
    );
}

#[test]
fn register_kinds_shipped_in_0_3_x_keep_their_meaning() {
    // Retained registers live in every upgraded data directory and group: a
    // kind that a public release wrote is never given another value shape.
    let kind = |name: &str| {
        registry::REGISTER_KINDS
            .iter()
            .find(|(_, n)| *n == name)
            .map(|(kind, _)| *kind)
    };
    assert_eq!(kind("LayoutValue"), Some(2));
    assert_eq!(kind("PointerShortcutSlots"), Some(0x1301));
    assert_eq!(kind("DeviceOrder"), Some(0x1302));
    // 0x1303 was DeviceOrder in the packaged 0.4.0-inhouse.2; it stays free.
    assert_eq!(kind("PointerBase"), Some(0x1304));
    assert!(registry::REGISTER_KINDS.iter().all(|(kind, _)| *kind != 0x1303));
    let mut kinds: Vec<u32> = registry::REGISTER_KINDS.iter().map(|(kind, _)| *kind).collect();
    let count = kinds.len();
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(kinds.len(), count, "duplicate register kind");
    assert!(kinds.iter().all(|kind| !registry::RESERVED_REGISTERS.contains(kind)));
}

#[test]
fn removed_admission_digests_and_device_facts_keys_stay_reserved() {
    let reserved = |name: &str| {
        registry::RESERVED_FIELDS
            .iter()
            .filter(|(n, _)| *n == name)
            .flat_map(|(_, keys)| keys.iter().copied())
            .collect::<Vec<u32>>()
    };
    assert_eq!(reserved("Admission"), [3, 4, 5]);
    for key in [28, 29, 30] {
        assert!(reserved("DeviceFacts").contains(&key), "DeviceFacts {key}");
    }
    let admission = v2::dto::Admission::SCHEMA
        .fields
        .iter()
        .map(|f| f.key)
        .collect::<Vec<_>>();
    assert_eq!(admission, [1, 2]);
    assert!(
        !v2::dto::DeviceFacts::SCHEMA
            .fields
            .iter()
            .any(|f| (28..=30).contains(&f.key))
    );
}

#[test]
fn global_audio_configuration_retires_close_enabled_and_the_forward_receive_options() {
    // 0x061f AudioEndpointClose: a PC authors Clear(phone) in the group audio
    // configuration instead; the number is never reused.
    assert!(registry::message(0x061f).is_none());
    assert!(registry::TOMBSTONES.contains(&0x061f));
    assert!(registry::RESERVED_MESSAGES.iter().any(|(id, _)| *id == 0x061f));
    let reserved = |table: &[(&str, &[u32])], name: &str| {
        table
            .iter()
            .filter(|(n, _)| *n == name)
            .flat_map(|(_, keys)| keys.iter().copied())
            .collect::<Vec<u32>>()
    };
    assert!(reserved(registry::RESERVED_FIELDS, "EndpointDirection").contains(&2));
    assert!(reserved(registry::RESERVED_FIELDS, "LocalOptionsSummary").contains(&2));
    let options = reserved(registry::RESERVED_ENUM_VALUES, "LocalOption");
    assert!(options.contains(&4) && options.contains(&24));
    assert!(!v2::dto::EndpointDirection::SCHEMA.fields.iter().any(|f| f.key == 2));
    let config = v2::dto::AudioConfig::SCHEMA
        .fields
        .iter()
        .map(|f| f.key)
        .collect::<Vec<_>>();
    assert_eq!(config, [1, 2, 3]);
}
