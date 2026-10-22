use oink_protocol::v1::{
    self, ErrorCode, Message, RawMap, RawValue as V,
    admission::*,
    dto::*,
    frame::*,
    registry::{self, Transport},
    schema::{WireDto, WireValue},
};
use oink_test_support::protocol as support;
fn context(agreement: &Agreement) -> Context<'_> {
    Context {
        kind: 2,
        transport: Transport::Reliable,
        phase: Phase::Member,
        agreement,
        direct: false,
        old_member_lane: false,
    }
}
fn ready() -> RawMap {
    let mut version = support::sample_named("ExitVolumeVersion");
    version.insert(3, V::from("synthetic-output"));
    [
        (1, V::Map(version)),
        (2, V::Uint(0)),
        (3, V::Bool(false)),
        (4, V::Uint(0)),
    ]
    .into()
}
#[test]
fn controls_are_c0_capability_gated_and_keyboard_wire_id_is_tombstoned() {
    let baseline = Agreement::new(["oink.group/1".into()]).unwrap();
    let options = Agreement::new(["oink.group/1".into(), "oink.device-options/1".into()]).unwrap();
    // No media decoder, input capability or audio.state subscription is required.
    let volume = Agreement::new(["oink.group/1".into(), "oink.audio.exit-volume/1".into()]).unwrap();
    for id in 0x0d23..=0x0d27 {
        let entry = registry::message(id).unwrap();
        let m = Message::from_payload(id, support::sample(entry.schema)).unwrap();
        let bytes = v1::frame::encode(&m, 4096).unwrap();
        let frame = OpaqueFrame::parse_exact(&bytes, 4096).unwrap();
        assert_eq!(
            context(&baseline).decode(frame).unwrap_err().code,
            ErrorCode::UnnegotiatedCapability
        );
        let c = context(if id < 0x0d25 { &options } else { &volume });
        assert!(c.decode(frame).is_ok());
        assert!(matches!(entry.pause, registry::PausePolicy::Allow));
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
    }
    assert!(registry::message(0x0d0b).is_none());
    assert!(registry::TOMBSTONES.contains(&0x0d0b));
}
#[test]
fn facts_distinguish_missing_none_false_zero_and_preserve_all_five_faults_and_future_values() {
    let absent = ResourceFacts::from_raw(RawMap::new()).unwrap();
    assert_eq!(absent.clipboard_mode().unwrap(), None);
    assert_eq!(absent.faults().unwrap(), None);
    assert!(absent.option_revisions().unwrap().0.is_empty());
    let faults = (1..=5)
        .map(|k| (k, V::Map([(1, V::Uint(0)), (900, support::synthetic_unknown())].into())))
        .collect();
    let resources = ResourceFacts::from_raw(
        [
            (1, V::Uint(0)),
            (2, V::Uint(400)),
            (3, V::Null),
            (4, V::Map(faults)),
            (
                5,
                V::Map([(2, V::Uint(0)), (3, V::Uint(0)), (4, V::Bool(false))].into()),
            ),
            (6, V::Array(vec![V::Map([(1, V::Uint(3)), (2, V::Uint(7))].into())])),
            (900, support::synthetic_unknown()),
        ]
        .into(),
    )
    .unwrap();
    assert_eq!(resources.clipboard_mode().unwrap(), Some(SharingMode::Shared));
    assert_eq!(resources.mouse_mode().unwrap(), Some(SharingMode::Unknown(400)));
    assert_eq!(resources.keyboard_mode().unwrap(), None);
    assert_eq!(
        resources
            .faults()
            .unwrap()
            .unwrap()
            .clipboard()
            .unwrap()
            .unwrap()
            .code()
            .unwrap(),
        ResourceFaultCode::None
    );
    let speaker = resources.speaker().unwrap().unwrap();
    assert_eq!(speaker.forwarding_volume_percent().unwrap(), Some(0));
    assert_eq!(speaker.forwarding_muted().unwrap(), Some(false));
    let mut raw = support::sample_named("DeviceFacts");
    raw.insert(16, resources.write());
    raw.insert(17, V::Map(ready()));
    let message = Message::ConfigFacts(DeviceFacts::from_raw(raw).unwrap());
    let bytes = v1::frame::encode(&message, v1::limits::CONFIG_FRAME).unwrap();
    // Reporting fields of the known Facts message require no reporting/control cap.
    let baseline = Agreement::new(["oink.group/1".into()]).unwrap();
    let decoded = context(&baseline)
        .decode(OpaqueFrame::parse_exact(&bytes, v1::limits::CONFIG_FRAME).unwrap())
        .unwrap();
    assert_eq!(v1::frame::encode(&decoded, v1::limits::CONFIG_FRAME).unwrap(), bytes);
}
#[test]
fn option_and_volume_bounds_are_structural_while_request_value_refusals_belong_to_owner() {
    let mut resources = RawMap::from([(6, V::Array(vec![V::Map([(1, V::Uint(3)), (2, V::Uint(0))].into())]))]);
    assert!(ResourceFacts::from_raw(resources.clone()).is_err());
    resources.insert(6, V::Array(vec![V::Map([(1, V::Uint(3)), (2, V::Uint(7))].into()); 2]));
    assert!(ResourceFacts::from_raw(resources).is_err());
    let mut state = ready();
    state.remove(&3);
    assert!(ExitVolumeState::from_raw(state).is_err());
    let mut state = ready();
    state.insert(2, V::Uint(101));
    assert!(ExitVolumeState::from_raw(state).is_err());
    let mut state = ready();
    state.insert(900, V::Bytes(vec![0; 2048]));
    assert_eq!(
        ExitVolumeState::from_raw(state).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    let future = ExitVolumeState::from_raw(
        [
            (1, V::Map(support::sample_named("ExitVolumeVersion"))),
            (4, V::Uint(400)),
            (900, support::synthetic_unknown()),
        ]
        .into(),
    )
    .unwrap();
    assert_eq!(future.status().unwrap(), ExitVolumeStatus::Unknown(400));
    assert!(future.volume_percent().unwrap().is_none());
    let mut request = support::sample_named("SetLocalOption");
    request.insert(3, V::Uint(400));
    request.insert(4, support::synthetic_unknown());
    let unknown = SetLocalOption::from_raw(request).unwrap();
    assert_eq!(unknown.option().unwrap(), LocalOption::Unknown(400));
    // Even known-option values stay raw until the owner returns InvalidValue;
    // decoding a request is never permission to apply it or a successful no-op.
    let mut request = unknown.into_raw();
    request.insert(3, V::Uint(3));
    assert!(SetLocalOption::from_raw(request).is_ok());
    let mut request = support::sample_named("SetExitVolume");
    request.insert(3, V::Uint(101));
    assert!(SetExitVolume::from_raw(request).is_ok());
    let mut request = support::sample_named("SetLocalOption");
    request.insert(900, V::Bytes(vec![0; 4096]));
    assert_eq!(
        SetLocalOption::from_raw(request).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
}
#[test]
fn exit_and_option_subtree_limits_are_checked_in_preflight_before_retention() {
    let raw = V::Map([(17, V::Map([(900, V::Bytes(vec![0; 2048]))].into()))].into());
    let bytes = raw.encode(16384).unwrap();
    assert_eq!(
        Message::preflight(0x0d08, &bytes).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    let raw = V::Map([(16, V::Map([(6, V::Array(vec![V::Map(RawMap::new()); 33]))].into()))].into());
    let bytes = raw.encode(16384).unwrap();
    assert_eq!(
        Message::preflight(0x0d08, &bytes).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
}

#[test]
fn deferred_forwarding_persistence_is_an_optional_retained_request_extension() {
    let mut old = support::sample_named("SetLocalOption");
    old.remove(&6);
    let legacy = SetLocalOption::from_raw(old.clone()).unwrap();
    assert!(!legacy.defer_persistence().unwrap());
    assert_eq!(legacy.raw(), &old); // no new bytes in an old-style request
    let mut opted_in = old.clone();
    opted_in.insert(6, true.write());
    let request = SetLocalOption::from_raw(opted_in.clone()).unwrap();
    assert!(request.defer_persistence().unwrap());
    let bytes = encode(&Message::ConfigSetLocalOption(request), v1::limits::CONFIG_FRAME).unwrap();
    let frame = OpaqueFrame::parse_exact(&bytes, v1::limits::CONFIG_FRAME).unwrap();
    // Existing v1 retaining decoders preserve unknown fields and can ignore key
    // 6, applying the original synchronous contract. False/missing stays old.
    let Message::ConfigSetLocalOption(roundtrip) = frame.decode().unwrap() else {
        panic!("wrong frame")
    };
    assert_eq!(roundtrip.raw(), &opted_in);
    let mut invalid = old;
    invalid.insert(6, V::Text("true".into()));
    assert!(SetLocalOption::from_raw(invalid).is_err());
}

#[test]
fn local_output_volume_is_optional_bounded_and_retained_in_existing_facts() {
    for percent in [0u8, 100] {
        let volume = LocalOutputVolume::from_raw([(1, percent.write()), (2, true.write())].into()).unwrap();
        assert_eq!(volume.volume_percent().unwrap(), percent);
        assert!(volume.muted().unwrap());
    }
    assert!(LocalOutputVolume::from_raw([(1, 101u8.write()), (2, false.write())].into()).is_err());
    assert!(LocalOutputVolume::from_raw([(1, 50u8.write())].into()).is_err());
    assert_eq!(
        SpeakerObservation::from_raw(RawMap::new())
            .unwrap()
            .local_output()
            .unwrap(),
        None
    );
    let mut raw = support::sample_named("DeviceFacts");
    raw.insert(
        16,
        V::Map(
            [(
                5,
                V::Map([(9, V::Map([(1, 42u8.write()), (2, true.write())].into()))].into()),
            )]
            .into(),
        ),
    );
    let message = Message::ConfigFacts(DeviceFacts::from_raw(raw).unwrap());
    let bytes = encode(&message, v1::limits::CONFIG_FRAME).unwrap();
    let decoded = OpaqueFrame::parse_exact(&bytes, v1::limits::CONFIG_FRAME)
        .unwrap()
        .decode()
        .unwrap();
    assert_eq!(decoded, message);
    assert!(registry::CAPABILITIES.contains(&"oink.audio.local-output/1"));
}
