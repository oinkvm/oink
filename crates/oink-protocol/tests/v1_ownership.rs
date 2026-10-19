use oink_protocol::v1::{
    self, ErrorCode, Message, RawMap, RawValue as V,
    admission::{Agreement, Context, Phase},
    dto::*,
    frame::{FrameHeader, OpaqueFrame},
    registry::{self, PausePolicy, Transport},
    retained::{register_digest, version_cmp},
    schema::{WireDto, WireValue},
};
use oink_test_support::protocol as support;

#[test]
fn layout_is_the_only_known_register_and_stamp_has_no_clock_authority() {
    for kind in [1, 3, 4, 5, 6, 7] {
        let mut raw = support::sample_named("Register");
        raw.insert(1, V::Uint(kind));
        assert_eq!(Register::from_raw(raw).unwrap_err().code, ErrorCode::UnsupportedValue);
    }
    let mut raw = support::sample_named("Register");
    raw.insert(5, V::Bool(true));
    raw.insert(4, V::Map(RawMap::new()));
    assert_eq!(
        Register::from_raw(raw.clone()).unwrap_err().code,
        ErrorCode::UnsupportedValue
    );
    // Unknown future register tombstones still retain their anti-resurrection data.
    raw.insert(1, V::Uint(400));
    raw.insert(900, support::synthetic_unknown());
    raw.insert(6, register_digest(&raw).unwrap().write());
    let future = Register::from_raw(raw).unwrap();
    v1::retained::verify_register(&future).unwrap();

    let a = ConfigVersion::from_raw(support::sample_named("ConfigVersion")).unwrap();
    let mut b = a.raw().clone();
    let mut later_writer = support::sample_named("DeviceId");
    later_writer.insert(2, V::Bytes(vec![1; 32]));
    b.insert(3, V::Map(later_writer));
    let mut old_clock = a.raw().clone();
    // Reserved data is retained, but must never break the (counter, writer) tie.
    old_clock.insert(2, V::Uint(u64::MAX));
    let clock = ConfigVersion::from_raw(old_clock).unwrap();
    assert_eq!(version_cmp(&a, &clock).unwrap(), std::cmp::Ordering::Equal);
    let b = ConfigVersion::from_raw(b).unwrap();
    assert!(version_cmp(&clock, &b).unwrap().is_lt());
    let mut counter = a.into_raw();
    counter.insert(1, V::Uint(2));
    assert!(
        version_cmp(&ConfigVersion::from_raw(counter).unwrap(), &b)
            .unwrap()
            .is_gt()
    );
}

#[test]
fn owner_catalogue_keeps_durable_revisions_and_nested_unknown_values() {
    let mut raw = support::sample_named("DeviceFacts");
    raw.insert(18, support::future_preferences());
    raw.insert(19, support::future_pause());
    let mut facts = DeviceFacts::from_raw(raw).unwrap();
    let original = facts.preferences().unwrap().unwrap();
    assert_eq!(original.0.len(), 19);
    assert_eq!(original.0[6].option().unwrap(), LocalOption::ScreenPreferences);
    assert_eq!(original.0[6].revision().unwrap(), u64::MAX);
    assert_eq!(original.0.last().unwrap().option().unwrap(), LocalOption::Unknown(400));
    // New Boot/sequence affects live publication, never the durable item revision.
    facts
        .edit(|raw| {
            let V::Map(boot) = raw.get_mut(&1).unwrap() else {
                panic!()
            };
            boot.insert(2, V::Bytes(vec![9; 16]));
            raw.insert(2, V::Uint(1));
        })
        .unwrap();
    assert_eq!(facts.preferences().unwrap().unwrap(), original);
    let bytes = facts.encode(v1::limits::CONFIG_FRAME).unwrap();
    let saved = DeviceFacts::from_raw(v1::cbor::decode_map(&bytes, v1::limits::CONFIG_FRAME).unwrap()).unwrap();
    assert_eq!(saved.encode(v1::limits::CONFIG_FRAME).unwrap(), bytes);
    let screen = ScreenPreferencesValue::read(&original.0[6].value().unwrap()).unwrap();
    assert_eq!(screen.screens().unwrap().0[0].raw()[&13], support::synthetic_unknown());
    assert_eq!(
        saved.pause().unwrap().unwrap().reason().unwrap(),
        PauseReason::Emergency
    );

    for options in [vec![1, 1], vec![2, 1], (1..=33).collect()] {
        let items = options
            .into_iter()
            .map(|option| V::Map([(1, V::Uint(option)), (2, V::Uint(1)), (3, V::Bool(true))].into()))
            .collect();
        let mut raw = support::sample_named("DeviceFacts");
        raw.insert(18, V::Array(items));
        assert!(DeviceFacts::from_raw(raw).is_err());
    }
    let mut item = support::sample_named("DevicePreference");
    item.insert(2, V::Uint(0));
    assert!(DevicePreference::from_raw(item).is_err());
    let absent = DeviceFacts::from_raw(support::sample_named("DeviceFacts")).unwrap();
    assert!(absent.preferences().unwrap().is_none());
    assert!(absent.pause().unwrap().is_none());
}

#[test]
fn emergency_is_bounded_baseline_c0_traffic_available_during_local_pause() {
    let agreement = Agreement::new(["oink.group/1".into()]).unwrap();
    let c = Context {
        kind: 2,
        transport: Transport::Reliable,
        phase: Phase::Member,
        agreement: &agreement,
        direct: false,
        old_member_lane: false,
    };
    for id in [0x0d2a, 0x0d2b] {
        let entry = registry::message(id).unwrap();
        assert!(matches!(entry.pause, PausePolicy::Allow));
        let m = Message::from_payload(id, support::sample(entry.schema)).unwrap();
        let bytes = v1::frame::encode(&m, 4096).unwrap();
        let frame = OpaqueFrame::parse_exact(&bytes, 4096).unwrap();
        assert!(c.decode(frame).is_ok());
        assert_eq!(
            Context { kind: 5, ..c }.decode(frame).unwrap_err().code,
            ErrorCode::InvalidChannel
        );
        assert_eq!(
            Context {
                phase: Phase::Admission,
                ..c
            }
            .decode(frame)
            .unwrap_err()
            .code,
            ErrorCode::NotReady
        );
        assert_eq!(
            c.admit(FrameHeader {
                type_id: id,
                payload_length: 4091
            })
            .unwrap_err()
            .code,
            ErrorCode::LimitExceeded
        );
        let mut oversized = support::sample(entry.schema);
        oversized.insert(900, V::Bytes(vec![0; 4096]));
        assert_eq!(
            Message::from_payload(id, oversized.clone()).unwrap_err().code,
            ErrorCode::LimitExceeded
        );
        let payload = v1::cbor::encode_map(&oversized, 8192).unwrap();
        assert_eq!(
            Message::preflight(id, &payload).unwrap_err().code,
            ErrorCode::LimitExceeded
        );
    }
    for id in [0x0d09, 0x0d0a, 0x0d0b] {
        assert!(registry::TOMBSTONES.contains(&id));
        assert_eq!(
            Message::from_payload(id, RawMap::new()).unwrap_err().code,
            ErrorCode::UnknownMessage
        );
    }
    let mut event = support::sample_named("EmergencyEventId");
    event.insert(2, V::Uint(0));
    assert!(EmergencyEventId::from_raw(event).is_err());
    let mut ack = support::sample_named("EmergencyPauseAck");
    ack.insert(3, V::Uint(400));
    assert_eq!(
        EmergencyPauseAck::from_raw(ack).unwrap().outcome().unwrap(),
        EmergencyOutcome::Unknown(400)
    );
    let mut pause = support::sample_named("LocalPauseState");
    pause.insert(900, V::Bytes(vec![0; 1024]));
    assert_eq!(
        LocalPauseState::from_raw(pause.clone()).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    let bytes = v1::cbor::encode_map(&[(19, V::Map(pause))].into(), 4096).unwrap();
    assert_eq!(
        Message::preflight(0x0d08, &bytes).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
}

#[test]
fn receiver_volume_has_no_source_route_stamp_and_media_uses_both_generations() {
    let mut raw = support::sample_named("ExitVolumeVersion");
    assert!(!raw.contains_key(&2));
    raw.insert(3, V::from("synthetic-output"));
    let token = ExitVolumeVersion::from_raw(raw).unwrap();
    assert_eq!(token.output_generation().unwrap(), 1);
    let session = AudioSessionContext::from_raw(support::sample_named("AudioSessionContext")).unwrap();
    assert_eq!(session.receiver_output().unwrap().generation().unwrap(), 1);
    let mut changed = session.raw().clone();
    changed.insert(2, V::Uint(2));
    let changed = AudioSessionContext::from_raw(changed).unwrap();
    assert_eq!(changed.receiver_output().unwrap(), session.receiver_output().unwrap());
    for id in [0x0603, 0x0605, 0x020a] {
        let mut raw = support::sample(registry::message(id).unwrap().schema);
        raw.insert(if id == 0x020a { 6 } else { 1 }, changed.write());
        assert!(Message::from_payload(id, raw).is_ok());
        let mut old = support::sample(registry::message(id).unwrap().schema);
        old.insert(
            if id == 0x020a { 6 } else { 1 },
            V::Map(support::sample_named("ConfigVersion")),
        );
        assert!(Message::from_payload(id, old).is_err());
    }
    let audio = Agreement::new(["oink.audio/1".into()]).unwrap();
    let none = Agreement::new([]).unwrap();
    let c = Context {
        kind: 5,
        transport: Transport::Reliable,
        phase: Phase::Member,
        agreement: &audio,
        direct: true,
        old_member_lane: false,
    };
    for id in [0x0611, 0x0612] {
        let message = Message::from_payload(id, support::sample(registry::message(id).unwrap().schema)).unwrap();
        let bytes = v1::frame::encode(&message, 16384).unwrap();
        let frame = OpaqueFrame::parse_exact(&bytes, 16384).unwrap();
        assert!(c.decode(frame).is_ok());
        assert_eq!(
            Context { agreement: &none, ..c }.decode(frame).unwrap_err().code,
            ErrorCode::UnnegotiatedCapability
        );
    }
    assert_eq!(ControlFailure::from_number(22), ControlFailure::Unknown(22));
    for name in ["ReceiverOutputBinding", "AudioSessionContext", "SourceRouteOffer"] {
        let mut raw = support::sample_named(name);
        raw.insert(2, V::Uint(0));
        assert!(registry::validate_schema(name, &raw).is_err());
    }
}

#[test]
fn reference_units_defaults_and_projection_bounds_match_the_design() {
    let topology = TopologySettings::from_raw(RawMap::new()).unwrap();
    assert_eq!(topology.endpoint_reach().unwrap().to_bits(), 0.0_f64.to_bits());
    assert_eq!(topology.overlap_tolerance().unwrap(), 5.0 / 686.0);
    assert_eq!(topology.minimum_portal().unwrap(), 10.0 / 686.0);
    assert_eq!(topology.portal_snap().unwrap(), 8.0 / 686.0);
    // The default's exact binary64 representation is part of the wire contract.
    assert_eq!(
        TopologySettings::from_raw([(12, V::F64(0.0))].into())
            .unwrap()
            .encode(32)
            .unwrap(),
        support::unhex("a10cfb0000000000000000")
    );
    assert_eq!(ScrollAmount::from_number(0), ScrollAmount::Reference);
    assert_eq!(v1::CONTACT_REFERENCE_QUANTUM, 1.0 / 68600.0);
    for n in [-1.0, 8.0, 50.0 / 686.0 + f64::EPSILON] {
        assert!(TopologySettings::from_raw([(12, V::F64(n))].into()).is_err());
        assert!(EffectiveSettings::from_raw([(12, V::F64(n))].into()).is_err());
    }
    assert!(TopologySettings::from_raw([(12, V::F64(50.0 / 686.0))].into()).is_ok());
    let mut preview = support::sample_named("ProjectionPreview");
    let mut screen = support::sample_named("ScreenRect");
    let rect = [
        (1, V::F64(1_000_000.0 / 686.0)),
        (2, V::F64(-1_000_000.0 / 686.0)),
        (3, V::F64(4000.0 / 686.0)),
        (4, V::F64(1.0)),
    ];
    screen.insert(3, V::Map(rect.into()));
    preview.insert(2, V::Array(vec![V::Map(screen.clone())]));
    assert!(ProjectionPreview::from_raw(preview.clone()).is_ok());
    let V::Map(rect) = screen.get_mut(&3).unwrap() else {
        panic!()
    };
    rect.insert(3, V::F64(4000.0));
    preview.insert(2, V::Array(vec![V::Map(screen)]));
    assert!(ProjectionPreview::from_raw(preview).is_err());
    for schema in registry::SCHEMAS {
        assert!(schema.fields.iter().all(|f| !f.name.contains("_mm")));
    }
}
