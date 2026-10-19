use oink_protocol::v1::{
    self, ErrorCode, Message, RawValue as V,
    admission::*,
    dto::*,
    frame::*,
    registry::{self, Transport},
    schema::WireDto,
};
use oink_test_support::protocol as support;
fn all() -> Agreement {
    Agreement::new(registry::CAPABILITIES.iter().map(|s| s.to_string())).unwrap()
}
fn context(agreement: &Agreement) -> Context<'_> {
    Context {
        kind: 3,
        transport: Transport::Reliable,
        phase: Phase::Member,
        agreement,
        direct: true,
        old_member_lane: false,
    }
}
fn identify() -> Message {
    Message::ControlIdentifyScreens(ControlIdentifyScreens::from_raw([(1, V::Uint(7))].into()).unwrap())
}
#[test]
fn documented_frame_and_every_split_and_coalescing() {
    let bytes = encode(&identify(), 16384).unwrap();
    assert_eq!(bytes, support::unhex("050103000000a10107"));
    for split in 0..=bytes.len() {
        let mut decoder = FrameDecoder::new(16384);
        let agreement = all();
        let ctx = context(&agreement);
        let mut frames = vec![];
        for chunk in [&bytes[..split], &bytes[split..]] {
            let mut input = chunk;
            if let Some(frame) = decoder
                .next(&mut input, |h| ctx.admit(h).map_err(|v| v.code.into()))
                .unwrap()
            {
                frames.push(frame);
            }
            assert!(input.is_empty());
        }
        assert_eq!(frames, vec![bytes.clone()]);
        decoder.finish().unwrap();
    }
    let joined = [bytes.clone(), bytes.clone()].concat();
    let mut input = joined.as_slice();
    let mut decoder = FrameDecoder::new(16384);
    for _ in 0..2 {
        assert_eq!(decoder.next(&mut input, |_| Ok(())).unwrap().unwrap(), bytes);
    }
    assert!(input.is_empty());
    decoder.finish().unwrap();
}
#[test]
fn unknown_type_rejected_at_header_before_body_and_poisoned_stream_never_continues() {
    let agreement = all();
    let ctx = context(&agreement);
    for id in [0x1234, 0xf000, 0x0200, 0x0a08] {
        let header = FrameHeader {
            type_id: id,
            payload_length: 9000,
        };
        let mut decoder = FrameDecoder::new(16384);
        let bytes = header.bytes();
        let mut input = bytes.as_slice();
        let error = decoder
            .next(&mut input, |h| ctx.admit(h).map_err(|v| v.code.into()))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::UnknownMessage);
        let known = encode(&identify(), 16384).unwrap();
        let mut input = known.as_slice();
        assert_eq!(
            decoder.next(&mut input, |_| Ok(())).unwrap_err().code,
            ErrorCode::UnknownMessage
        );
        assert_eq!(input, known);
    }
    let header = FrameHeader {
        type_id: 0x1234,
        payload_length: 1,
    };
    let fault = ctx.admit(header).unwrap_err();
    assert_eq!(fault.isolation, Isolation::Stream);
    assert!(fault.cleanup && fault.suppress_reopen && fault.reply);
    assert!(
        context(&all())
            .admit(FrameHeader {
                type_id: 0x105,
                payload_length: 3
            })
            .is_ok()
    );
}
#[test]
fn invalid_lengths_fin_reserved_and_extensions_have_typed_errors() {
    for bytes in [
        support::unhex("000001000000a0"),
        support::unhex("ffff01000000a0"),
        support::unhex("050100000000"),
        support::unhex("050103000000a1"),
        support::unhex("050103000000a10107ff"),
    ] {
        assert!(OpaqueFrame::parse_exact(&bytes, 16384).is_err());
    }
    assert_eq!(
        FrameHeader::parse(
            &FrameHeader {
                type_id: 0x105,
                payload_length: 16379
            }
            .bytes(),
            16384
        )
        .unwrap_err()
        .code,
        ErrorCode::LimitExceeded
    );
    assert!(
        FrameHeader::parse(
            &FrameHeader {
                type_id: 0x105,
                payload_length: 16378
            }
            .bytes(),
            16384
        )
        .is_ok()
    );
    let mut decoder = FrameDecoder::new(100);
    let mut input = &[5u8][..];
    decoder.next(&mut input, |_| Ok(())).unwrap();
    assert_eq!(decoder.finish().unwrap_err().code, ErrorCode::MalformedFrame);
    let base = StreamHeader::bootstrap().encode().unwrap();
    assert_eq!(&base[..6], b"OINK\x20\x00");
    for position in [8, 9, 11, 28, 29, 30, 31] {
        let mut bad = base;
        bad[position] = 1;
        assert_eq!(StreamHeader::parse(&bad).unwrap_err().code, ErrorCode::InvalidHeader);
    }
    let mut bad = base;
    bad[10] = 2;
    assert_eq!(StreamHeader::parse(&bad).unwrap_err().code, ErrorCode::UnsupportedValue);
    let mut extension = base.to_vec();
    extension[4] = 36;
    extension.extend([1, 0, 0, 0]);
    assert_eq!(
        StreamHeader::parse(&extension).unwrap_err().code,
        ErrorCode::UnsupportedHeaderExtension
    );
    extension[32] = 0;
    assert_eq!(
        StreamHeader::parse(&extension).unwrap_err().code,
        ErrorCode::InvalidHeader
    );
}
#[test]
fn release_destination_has_no_experimental_decoder_but_opaque_forwarder_keeps_bytes() {
    let bytes = support::unhex("00f003000000ffffff");
    let frame = OpaqueFrame::parse_exact(&bytes, 16384).unwrap();
    assert_eq!(frame.bytes(), bytes);
    assert_eq!(frame.decode().unwrap_err().code, ErrorCode::UnknownMessage);
    let agreement = all();
    let ctx = context(&agreement);
    let header = StreamHeader {
        kind: 0xf0,
        priority: 1,
        route_tag: 9,
        ordinal: 1,
        datagram_flow: 77,
    };
    let raw = header.encode().unwrap();
    assert_eq!(StreamHeader::parse(&raw).unwrap().unwrap().0, header);
    let err = ctx.admit_stream(&header).unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsupportedKind);
    assert!(err.experimental);
    let err = ctx.admit_low_open(0xf0).unwrap_err();
    assert_eq!(err.isolation, Isolation::LowOperation);
}
#[test]
fn registered_datagram_errors_retire_owner_unregistered_flow_is_only_dropped() {
    let bytes = support::unhex("07000000341201000000a0");
    assert_eq!(datagram(&bytes, |_| false).unwrap(), Datagram::Unregistered { flow: 7 });
    let Datagram::Active { flow, frame } = datagram(&bytes, |f| f == 7).unwrap() else {
        panic!()
    };
    assert_eq!(flow, 7);
    let agreement = all();
    let mut ctx = context(&agreement);
    ctx.transport = Transport::Datagram;
    assert_eq!(ctx.admit(frame.header).unwrap_err().isolation, Isolation::Stream);
    assert!(datagram(&[0; 4], |_| true).is_err());
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(datagram(&extra, |_| true).is_err());
    let known = encode_datagram(
        8,
        &Message::InputRelativeMotion(
            InputRelativeMotion::from_raw(support::sample_named("InputRelativeMotion")).unwrap(),
        ),
    )
    .unwrap();
    assert!(matches!(datagram(&known, |f| f == 8).unwrap(), Datagram::Active { .. }));
}
#[test]
fn known_unnegotiated_wrong_channel_and_error_reply_rules() {
    let agreement = Agreement::new(registry::BASELINE.iter().map(|s| s.to_string())).unwrap();
    let mut ctx = context(&agreement);
    ctx.kind = 12;
    let err = ctx
        .admit(FrameHeader {
            type_id: 0x1100,
            payload_length: 1,
        })
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::UnnegotiatedCapability);
    assert_eq!(err.capability, Some("oink.layout.projection/1"));
    ctx.kind = 3;
    assert_eq!(
        ctx.admit(FrameHeader {
            type_id: 0x1100,
            payload_length: 1
        })
        .unwrap_err()
        .code,
        ErrorCode::InvalidChannel
    );
    ctx.kind = 0;
    ctx.phase = Phase::PhysicalBootstrap;
    assert!(
        ctx.admit(FrameHeader {
            type_id: 0xc00,
            payload_length: 1
        })
        .is_ok()
    );
    assert_eq!(
        ctx.admit(FrameHeader {
            type_id: 0xc01,
            payload_length: 1
        })
        .unwrap_err()
        .code,
        ErrorCode::NotReady
    );
    let malformed = ctx
        .admit(FrameHeader {
            type_id: 1,
            payload_length: 0,
        })
        .unwrap_err();
    assert!(!malformed.reply);
    assert_eq!(malformed.isolation, Isolation::PhysicalConnection);
}
#[test]
fn pairing_is_direct_and_low_business_waits_for_membership() {
    let agreement = all();
    let mut ctx = context(&agreement);
    ctx.kind = 2;
    ctx.phase = Phase::Admission;
    ctx.direct = false;
    assert_eq!(
        ctx.admit(FrameHeader {
            type_id: 0xd03,
            payload_length: 1
        })
        .unwrap_err()
        .code,
        ErrorCode::InvalidChannel
    );
    ctx.old_member_lane = true;
    assert!(
        ctx.admit(FrameHeader {
            type_id: 0xd1b,
            payload_length: 1
        })
        .is_ok()
    );
    assert_eq!(
        ctx.admit(FrameHeader {
            type_id: 0xd08,
            payload_length: 1
        })
        .unwrap_err()
        .code,
        ErrorCode::NotReady
    );
    assert_eq!(ctx.admit_low_open(4).unwrap_err().code, ErrorCode::NotReady);
    ctx.phase = Phase::Member;
    assert!(ctx.admit_low_open(4).is_ok());
}
#[test]
fn nested_low_frames_span_record_chunks_without_new_outer_prefix() {
    let inner = encode(&identify(), 16384).unwrap();
    let mut decoder = FrameDecoder::new(v1::limits::LOW_FRAME);
    let mut completed = vec![];
    for piece in inner.chunks(2) {
        let mut record = support::sample_named("LowRecord");
        record.insert(1, V::Bytes(piece.to_vec()));
        let bytes = encode(
            &Message::LowRecord(LowRecord::from_raw(record).unwrap()),
            v1::limits::LOW_OUTER_FRAME,
        )
        .unwrap();
        let Message::LowRecord(record) = OpaqueFrame::parse_exact(&bytes, v1::limits::LOW_OUTER_FRAME)
            .unwrap()
            .decode()
            .unwrap()
        else {
            panic!()
        };
        let data = record.bytes().unwrap();
        let mut input = data.0.as_slice();
        if let Some(frame) = decoder.next(&mut input, |_| Ok(())).unwrap() {
            completed.push(frame);
        }
    }
    assert_eq!(completed, vec![inner]);
    decoder.finish().unwrap();
}
#[test]
fn a_malformed_protocol_error_is_recorded_without_reciprocal_reply() {
    let agreement = all();
    let mut ctx = context(&agreement);
    ctx.kind = 0;
    ctx.phase = Phase::PhysicalBootstrap;
    let bytes = support::unhex("010001000000a0");
    let frame = OpaqueFrame::parse_exact(&bytes, 16384).unwrap();
    let fault = ctx.decode(frame).unwrap_err();
    assert_eq!(fault.code, ErrorCode::MalformedFrame);
    assert!(!fault.reply);
    assert_eq!(registry::message(0xd08).unwrap().capabilities, &["oink.group/1"]);
}

#[test]
fn low_ready_acknowledges_control_and_tcp_setup_only() {
    let agreement = all();
    let header = FrameHeader {
        type_id: 0x0c22,
        payload_length: 4,
    };
    for transport in [Transport::Reliable, Transport::Tcp] {
        let ctx = Context {
            kind: 1,
            transport,
            ..context(&agreement)
        };
        assert!(ctx.admit(header).is_ok());
    }
    for (kind, transport) in [(1, Transport::Low), (4, Transport::Tcp), (0, Transport::Reliable)] {
        let ctx = Context {
            kind,
            transport,
            ..context(&agreement)
        };
        assert_eq!(ctx.admit(header).unwrap_err().code, ErrorCode::InvalidChannel);
    }
}

#[test]
fn owner_payload_failure_preserves_operation_scope_and_error_loop_guard() {
    let agreement = all();
    let ctx = Context {
        kind: 9,
        transport: Transport::Low,
        ..context(&agreement)
    };
    let failure = ctx.payload_violation(0x0800, ErrorCode::UnsupportedValue);
    assert_eq!(failure.isolation, Isolation::LowOperation);
    assert_eq!(failure.type_id, Some(0x0800));
    assert!(failure.reply && failure.cleanup && failure.suppress_reopen);
    assert!(!ctx.payload_violation(1, ErrorCode::MalformedFrame).reply);
}
