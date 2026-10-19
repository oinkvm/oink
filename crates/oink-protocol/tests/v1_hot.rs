use oink_protocol::v1::{
    self, ErrorCode, RawValue,
    dto::*,
    frame::OpaqueFrame,
    hot::{self, Value as V},
    schema::WireDto,
};
use oink_test_support::protocol as support;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    hint::black_box,
};
struct Counting;
thread_local! {static ACTIVE:Cell<bool>=const{Cell::new(false)};static ALLOCS:Cell<usize>=const{Cell::new(0)};}
fn count() {
    let _ = ACTIVE.try_with(|active| {
        if active.get() {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
    });
}
// SAFETY: Unchanged layouts and pointers are delegated to System; counters only
// observe the calling test thread and do not participate in memory ownership.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: The allocator caller supplies a valid layout; forward unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: The allocator caller supplies a valid layout; forward unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count();
        // SAFETY: Preserve the caller's pointer, layout and requested size.
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: System allocated this pointer with the supplied layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;
fn zero_alloc<T>(f: impl FnOnce() -> T) -> T {
    ALLOCS.set(0);
    ACTIVE.set(true);
    let output = f();
    ACTIVE.set(false);
    assert_eq!(ALLOCS.get(), 0, "hot-path heap allocations");
    output
}
fn exercise(type_id: u16, fields: &[(u32, V<'_>)]) {
    let agreement = v1::admission::Agreement::new(v1::registry::CAPABILITIES.iter().map(|s| (*s).to_owned())).unwrap();
    let context = v1::admission::Context {
        kind: if type_id >= 0x0600 { 5 } else { 3 },
        transport: if matches!(type_id, 0x0209 | 0x0604 | 0x0606) {
            v1::registry::Transport::Datagram
        } else {
            v1::registry::Transport::Reliable
        },
        phase: v1::admission::Phase::Member,
        agreement: &agreement,
        direct: true,
        old_member_lane: false,
    };
    let mut send = [0; 4096];
    let mut forwarded = [0; 4096];
    let n = zero_alloc(|| {
        let n = hot::encode_into(type_id, black_box(fields), &mut send, 4096).unwrap();
        let frame = OpaqueFrame::parse_exact(black_box(&send[..n]), 4096).unwrap();
        let message = context.decode_hot(frame).unwrap();
        // Read actual typed fields; a borrowed frame that only copies bytes would
        // not test the runtime path used by input injection/media decode.
        match message {
            hot::Message::InputMotion(v) => {
                black_box((
                    v.seq().unwrap(),
                    v.dx().unwrap(),
                    v.dy().unwrap(),
                    v.forwarded_hops().unwrap(),
                    v.capture_time_us().unwrap(),
                ));
                if let Some(boot) = v.origin().unwrap() {
                    black_box((boot.device_id().unwrap().public_key().unwrap(), boot.nonce().unwrap()));
                }
            }
            hot::Message::InputKey(v) => {
                black_box((v.code().unwrap(), v.pressed().unwrap(), v.physical_code().unwrap()));
            }
            hot::Message::InputButton(v) => {
                black_box((v.button().unwrap().kind().unwrap(), v.pressed().unwrap()));
            }
            hot::Message::InputScroll(v) => {
                black_box((
                    v.source().unwrap(),
                    v.dx().unwrap(),
                    v.dy().unwrap(),
                    v.unit().unwrap(),
                    v.phase().unwrap(),
                ));
            }
            hot::Message::InputHeldSync(v) => {
                for key in v.keys().unwrap() {
                    black_box(key.unwrap());
                }
                for button in v.buttons().unwrap() {
                    black_box(button.unwrap().kind().unwrap());
                }
            }
            hot::Message::InputKeyRepeat(v) => {
                black_box((v.code().unwrap(), v.n().unwrap()));
            }
            hot::Message::InputGesture(v) => {
                black_box((v.source().unwrap(), v.gesture().unwrap().kind().unwrap()));
            }
            hot::Message::InputRelativeMotion(v) => {
                black_box((
                    v.epoch().unwrap(),
                    v.generation().unwrap(),
                    v.seq().unwrap(),
                    v.x().unwrap(),
                    v.y().unwrap(),
                ));
            }
            hot::Message::InputVolume(v) => {
                black_box((
                    v.code().unwrap(),
                    v.pressed().unwrap(),
                    v.repeat().unwrap(),
                    v.modifiers().unwrap(),
                    v.target_boot().unwrap().nonce().unwrap(),
                    v.session().unwrap().source_route_generation().unwrap(),
                    v.press_id().unwrap(),
                ));
                let session = v.session().unwrap();
                let output = session.receiver_output().unwrap();
                let claim = session.exit_claim().unwrap();
                black_box((claim.counter().unwrap(), claim.owner().unwrap().public_key().unwrap()));
                black_box((
                    session.source().unwrap().device_id().unwrap().public_key().unwrap(),
                    output.receiver().unwrap().nonce().unwrap(),
                    output.generation().unwrap(),
                    output.output_id().unwrap(),
                ));
            }
            hot::Message::InputTouchpadContacts(v) => {
                black_box((
                    v.device().unwrap(),
                    v.time_us().unwrap(),
                    v.phase().unwrap(),
                    v.surface().unwrap().width().unwrap(),
                ));
                for contact in v.changes().unwrap() {
                    black_box(contact.unwrap().contact().unwrap().id().unwrap());
                }
            }
            hot::Message::InputVolumeKeyOrigin(v) => {
                black_box((v.action().unwrap(), v.provenance().unwrap().physical().unwrap()));
            }
            hot::Message::AudioPacket(v) => {
                black_box((
                    v.stream_id().unwrap(),
                    v.sequence().unwrap(),
                    v.burst().unwrap(),
                    v.source_time_ns().unwrap(),
                    v.silent().unwrap(),
                    v.payload().unwrap(),
                ));
            }
            hot::Message::AudioSleep(v) => {
                black_box((v.stream_id().unwrap(), v.burst().unwrap(), v.sequence().unwrap()));
            }
        }
        assert_eq!(message.encode_into(&mut forwarded, 4096).unwrap(), n);
        n
    });
    assert_eq!(send[..n], forwarded[..n]);
    // Both codecs implement exactly the same schema and canonical bytes.
    let owned = OpaqueFrame::parse_exact(&send[..n], 4096).unwrap().decode().unwrap();
    assert_eq!(v1::frame::encode(&owned, 4096).unwrap(), send[..n]);
}
#[test]
fn every_input_and_media_capture_encode_decode_and_typed_access_is_allocation_free() {
    let identity = [(1, V::Text(ED25519)), (2, V::Bytes(&[7; 32]))];
    let boot = [(1, V::Map(&identity)), (2, V::Bytes(&[8; 16]))];
    let nested = [(1, V::Uint(400)), (2, V::Array(&[V::F64(1.25), V::Text("synthetic")]))];
    let output = [
        (1, V::Map(&boot)),
        (2, V::Uint(u64::MAX)),
        (3, V::Text("synthetic-output")),
        (900, V::Map(&nested)),
    ];
    let claim = [(1, V::Uint(u64::MAX)), (2, V::Map(&identity)), (900, V::Map(&nested))];
    let session = [
        (1, V::Map(&boot)),
        (2, V::Uint(u64::MAX)),
        (3, V::Map(&output)),
        (4, V::Map(&claim)),
        (900, V::Map(&nested)),
    ];
    let surface = [(1, V::Uint(10000)), (2, V::Uint(8000))];
    let contact_fields: [[(u32, V<'_>); 3]; 10] =
        std::array::from_fn(|i| [(1, V::Uint(i as u64 + 1)), (2, V::Uint(125)), (3, V::Uint(250))]);
    let change_fields: [[(u32, V<'_>); 2]; 10] =
        std::array::from_fn(|i| [(1, V::Map(&contact_fields[i])), (2, V::Uint(if i < 5 { 2 } else { 0 }))]);
    let contacts: [V<'_>; 10] = std::array::from_fn(|i| V::Map(&change_fields[i]));
    let envelope = [(1, V::Uint(400)), (2, V::Map(&nested))];
    let provenance = [(1, V::Bool(true)), (2, V::Uint(0))];
    exercise(0x0201, &[(1, V::Uint(700)), (2, V::F64(1.25)), (3, V::F64(-2.5))]);
    exercise(
        0x0201,
        &[
            (1, V::Uint(u64::MAX)),
            (2, V::F64(1.25)),
            (3, V::F64(-2.5)),
            (100, V::Map(&boot)),
            (101, V::Uint(3)),
            (102, V::Uint(u32::MAX.into())),
            (900, V::Map(&nested)),
        ],
    );
    exercise(0x0202, &[(1, V::Uint(4)), (2, V::Bool(true))]);
    exercise(
        0x0202,
        &[(1, V::Uint(0x0007_00e1)), (2, V::Bool(true)), (3, V::Uint(0x0007_0039))],
    );
    exercise(0x0203, &[(1, V::Map(&envelope)), (2, V::Bool(true))]);
    exercise(
        0x0204,
        &[
            (1, V::Uint(0)),
            (2, V::F64(1.0)),
            (3, V::F64(-2.0)),
            (4, V::Uint(0)),
            (5, V::Uint(400)),
        ],
    );
    exercise(
        0x0205,
        &[
            (1, V::Array(&[V::Uint(4); 256])),
            (2, V::Array(&[V::Map(&envelope); 32])),
        ],
    );
    exercise(0x0207, &[(1, V::Uint(4)), (2, V::Uint(10))]);
    exercise(0x0208, &[(1, V::Uint(0)), (2, V::Map(&envelope))]);
    exercise(
        0x0209,
        &[
            (1, V::Uint(1)),
            (2, V::Uint(1)),
            (3, V::Uint(700)),
            (4, V::F64(1.25)),
            (5, V::F64(-2.5)),
        ],
    );
    exercise(
        0x020a,
        &[
            (1, V::Uint(1)),
            (2, V::Bool(true)),
            (3, V::Bool(false)),
            (4, V::Uint(0)),
            (5, V::Map(&boot)),
            (6, V::Map(&session)),
            (7, V::Uint(1)),
        ],
    );
    exercise(
        0x020b,
        &[
            (1, V::Uint(1)),
            (2, V::Uint(u64::MAX)),
            (3, V::Uint(1)),
            (4, V::Map(&surface)),
            (6, V::Array(&contacts)),
        ],
    );
    exercise(0x020c, &[(1, V::Uint(0)), (2, V::Map(&provenance))]);
    exercise(
        0x0604,
        &[
            (1, V::Uint(u64::MAX)),
            (2, V::Uint(u64::MAX)),
            (3, V::Uint(u64::MAX)),
            (4, V::Uint(u64::MAX)),
            (5, V::Bool(false)),
            (6, V::Bytes(&[0; 960])),
        ],
    );
    exercise(
        0x0606,
        &[(1, V::Uint(u64::MAX)), (2, V::Uint(u64::MAX)), (3, V::Uint(u64::MAX))],
    );
}
#[test]
fn borrowed_profile_and_semantics_reject_malformed_before_dispatch() {
    let mut out = [0; 1024];
    let wrong = [(1, V::Uint(1)), (2, V::F32(1.0)), (3, V::F64(2.0))];
    assert_eq!(
        hot::encode_into(0x0201, &wrong, &mut out, 1024).unwrap_err().code,
        ErrorCode::MalformedFrame
    );
    let missing = [(1, V::Uint(1))];
    assert_eq!(
        hot::encode_into(0x0201, &missing, &mut out, 1024).unwrap_err().code,
        ErrorCode::MalformedFrame
    );
    assert!(hot::Map::parse(&[0xa1, 1, 0x18, 1], 1024).is_err());
    assert!(hot::Map::parse(&[0xa2, 1, 0, 1, 0], 1024).is_err());
    assert!(hot::Map::parse(&[0xa1, 1, 0x9f, 0xff], 1024).is_err());
    assert!(hot::encode_into(0x0201, &wrong, &mut [0; 6], 1024).is_err());
    let mut raw = support::sample_named("InputVolume");
    raw.insert(7, RawValue::Uint(0));
    assert!(InputVolume::from_raw(raw.clone()).is_err());
    let bytes = v1::cbor::encode_map(&raw, 4096).unwrap();
    assert!(hot::View::<InputVolume>::parse(&bytes, 4096).is_err());
    for path in [&[6, 2][..], &[6, 3, 2][..]] {
        let mut raw = support::sample_named("InputVolume");
        v1::retained::patch_path(&mut raw, path, Some(RawValue::Uint(0))).unwrap();
        assert!(InputVolume::from_raw(raw.clone()).is_err());
        let bytes = v1::cbor::encode_map(&raw, 4096).unwrap();
        // Rejection builds a cold diagnostic field path; the realtime allocation
        // contract above covers successful encode/decode/reads, including extras.
        assert!(hot::View::<InputVolume>::parse(&bytes, 4096).is_err());
    }
}
#[test]
fn every_hot_golden_including_nested_unknowns_uses_same_schema_and_zero_allocations() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1");
    let ids = [
        0x0201, 0x0202, 0x0203, 0x0204, 0x0205, 0x0207, 0x0208, 0x0209, 0x020a, 0x020b, 0x020c, 0x0604, 0x0606,
    ];
    for id in ids {
        for suffix in ["", "-future"] {
            let bytes = support::unhex(
                std::fs::read_to_string(root.join(format!("{id:04x}{suffix}.hex")))
                    .unwrap()
                    .trim(),
            );
            let mut out = vec![0; bytes.len()];
            zero_alloc(|| {
                let message = hot::Message::decode(OpaqueFrame::parse_exact(&bytes, 4096).unwrap()).unwrap();
                assert_eq!(message.encode_into(&mut out, 4096).unwrap(), bytes.len());
            });
            assert_eq!(out, bytes);
        }
    }
}

#[test]
fn diagnostic_kind_preflight_borrows_the_event_discriminator() {
    let raw = [
        (1, RawValue::from("trail/1")),
        (2, RawValue::Map(support::sample_named("DiagTrail"))),
    ]
    .into();
    let bytes = v1::cbor::encode_map(&raw, v1::limits::DIAGNOSTIC_SEGMENT).unwrap();
    zero_alloc(|| <DiagnosticEvent as WireDto>::preflight(&mut minicbor::Decoder::new(&bytes), 0)).unwrap();
}
