use oink_protocol::v1::{
    self, ErrorCode, Message, RawMap, RawValue as V,
    admission::{Agreement, Context, Phase},
    dto::*,
    frame::OpaqueFrame,
    registry::{self, PausePolicy, Transport},
    schema::{WireDto, WireValue},
};
use oink_test_support::protocol as support;

fn roundtrip(name: &str, raw: RawMap) {
    registry::validate_schema(name, &raw).unwrap();
    let bytes = v1::cbor::encode_map(&raw, v1::limits::CONFIG_FRAME).unwrap();
    assert_eq!(v1::cbor::decode_map(&bytes, v1::limits::CONFIG_FRAME).unwrap(), raw);
}
#[test]
fn caps_tap_hold_is_custom_only_and_conflicts_with_plain_caps_mapping() {
    let mut raw = support::sample_named("KeyboardMapping");
    let mut caps = support::sample_named("CapsTapHold");
    caps.insert(900, support::synthetic_unknown());
    raw.insert(3, V::Map(caps.clone()));
    let mapping = KeyboardMapping::from_raw(raw.clone()).unwrap();
    assert_eq!(mapping.caps_tap_hold().unwrap().unwrap().raw(), &caps);
    roundtrip("KeyboardMapping", raw.clone());
    for preset in [0, 1, 3, 400] {
        let mut bad = raw.clone();
        bad.insert(1, V::Uint(preset));
        assert!(KeyboardMapping::from_raw(bad).is_err());
    }
    raw.insert(
        2,
        V::Array(vec![V::Map(
            [(1, V::Uint(0x0007_0039)), (2, V::Uint(0x0007_00e1))].into(),
        )]),
    );
    assert!(KeyboardMapping::from_raw(raw.clone()).is_err());
    let mut invalid = support::sample_named("CapsTapHold");
    invalid.insert(1, V::Uint(0));
    assert!(CapsTapHold::from_raw(invalid).is_err());
    raw.remove(&3);
    assert!(
        KeyboardMapping::from_raw(raw)
            .unwrap()
            .caps_tap_hold()
            .unwrap()
            .is_none()
    );
}
#[test]
fn focus_target_game_and_device_gain_are_typed_and_retaining() {
    let lock = ControlSetFocusLock::from_raw(support::sample_named("ControlSetFocusLock")).unwrap();
    assert!(lock.target().unwrap().is_some());
    let mut raw = lock.into_raw();
    raw.remove(&3);
    assert!(ControlSetFocusLock::from_raw(raw).unwrap().target().unwrap().is_none());
    let mut gain = support::sample_named("ScreenPreferencesValue");
    gain.remove(&2);
    assert_eq!(
        ScreenPreferencesValue::from_raw(gain.clone())
            .unwrap()
            .device_gain()
            .unwrap(),
        1.0
    );
    for n in [0.1, 5.0] {
        gain.insert(2, V::F64(n));
        roundtrip("ScreenPreferencesValue", gain.clone());
    }
    for n in [0.0, 0.099, 5.001, -1.0] {
        gain.insert(2, V::F64(n));
        assert!(ScreenPreferencesValue::from_raw(gain.clone()).is_err());
    }
    let mut screen = support::sample_named("ScreenPreference");
    screen.insert(2, V::F64(5.001));
    assert!(ScreenPreference::from_raw(screen).is_err());
    let mut invalid_target = support::sample_named("ControlSetFocusLock");
    invalid_target.insert(3, V::Uint(1));
    assert!(ControlSetFocusLock::from_raw(invalid_target).is_err());
    let resources = ResourceFacts::from_raw(support::sample_named("ResourceFacts")).unwrap();
    assert!(resources.game().unwrap().is_some());
    assert_eq!(LocalOption::from_number(18), LocalOption::GameModePreference);
    assert_eq!(LocalOption::from_number(19), LocalOption::AcceptContacts);
    assert_eq!(LocalOption::from_number(20), LocalOption::AudioExitClaim);
    for n in [9, 17] {
        assert_eq!(LocalOption::from_number(n), LocalOption::Unknown(n));
    }
}
#[test]
fn claims_bind_media_and_volume_and_cannot_impersonate_another_owner() {
    let mut facts = support::sample_named("DeviceFacts");
    let mut claim = support::sample_named("AudioExitClaim");
    claim.insert(900, support::synthetic_unknown());
    facts.insert(20, V::Map(claim.clone()));
    let decoded = DeviceFacts::from_raw(facts.clone()).unwrap();
    assert_eq!(decoded.audio_exit_claim().unwrap().unwrap().raw(), &claim);
    roundtrip("DeviceFacts", facts.clone());
    let mut stamp = support::sample_named("ExitClaimStamp");
    stamp.insert(1, V::Uint(0));
    assert!(ExitClaimStamp::from_raw(stamp.clone()).is_err());
    let bytes = v1::cbor::encode_map(&stamp, 4096).unwrap();
    assert!(v1::hot::View::<ExitClaimStamp>::parse(&bytes, 4096).is_err());
    stamp.insert(1, V::Uint(1));
    let mut owner = support::sample_named("DeviceId");
    owner.insert(2, V::Bytes(vec![1; 32]));
    stamp.insert(2, V::Map(owner));
    claim.insert(1, V::Map(stamp));
    facts.insert(20, V::Map(claim));
    assert!(DeviceFacts::from_raw(facts).is_err());
    // Both fences are required: exit ABA must invalidate a stale command even
    // if the source route and native output happened not to change.
    for (name, key) in [
        ("AudioSessionContext", 4),
        ("SourceRouteOffer", 4),
        ("ExitVolumeVersion", 6),
    ] {
        let mut raw = support::sample_named(name);
        roundtrip(name, raw.clone());
        raw.remove(&key);
        assert!(registry::validate_schema(name, &raw).is_err());
    }
    assert_eq!(ControlFailure::from_number(22), ControlFailure::Unknown(22));
}

#[test]
fn audio_watermark_can_describe_removed_owner_without_impersonating_its_claim() {
    let mut facts = support::sample_named("DeviceFacts");
    let mut witness = support::sample_named("AudioExitClaim");
    let mut stamp = support::sample_named("ExitClaimStamp");
    let mut owner = support::sample_named("DeviceId");
    owner.insert(2, V::Bytes(vec![9; 32]));
    stamp.insert(2, V::Map(owner));
    witness.insert(1, V::Map(stamp));
    witness.insert(900, support::synthetic_unknown());
    facts.insert(21, V::Map(witness.clone()));
    let decoded = DeviceFacts::from_raw(facts.clone()).unwrap();
    assert_eq!(decoded.audio_claim_watermark().unwrap().unwrap().raw(), &witness);
    roundtrip("DeviceFacts", facts.clone());
    facts.insert(20, V::Map(witness));
    assert!(DeviceFacts::from_raw(facts).is_err(), "only Facts20 is an owner claim");
}
#[test]
fn pause_request_and_result_are_single_c0_messages_allowed_while_paused() {
    let baseline = Agreement::new(["oink.group/1".into()]).unwrap();
    let controls = Agreement::new(["oink.group/1".into(), "oink.device-options/1".into()]).unwrap();
    let context = Context {
        kind: 2,
        transport: Transport::Reliable,
        phase: Phase::Member,
        agreement: &controls,
        direct: false,
        old_member_lane: false,
    };
    for id in [0x0d28, 0x0d29] {
        let entry = registry::message(id).unwrap();
        assert!(matches!(entry.pause, PausePolicy::Allow));
        assert!(!registry::TOMBSTONES.contains(&id));
        let mut raw = support::sample(entry.schema);
        raw.insert(900, support::synthetic_unknown());
        let m = Message::from_payload(id, raw).unwrap();
        let bytes = v1::frame::encode(&m, 4096).unwrap();
        let f = OpaqueFrame::parse_exact(&bytes, 4096).unwrap();
        assert_eq!(v1::frame::encode(&context.decode(f).unwrap(), 4096).unwrap(), bytes);
        assert_eq!(
            Context {
                agreement: &baseline,
                ..context
            }
            .decode(f)
            .unwrap_err()
            .code,
            ErrorCode::UnnegotiatedCapability
        );
        assert_eq!(
            Context { kind: 3, ..context }.decode(f).unwrap_err().code,
            ErrorCode::InvalidChannel
        );
        let mut big = support::sample(entry.schema);
        big.insert(900, V::Bytes(vec![0; 4096]));
        let payload = v1::cbor::encode_map(&big, 8192).unwrap();
        assert_eq!(
            Message::preflight(id, &payload).unwrap_err().code,
            ErrorCode::LimitExceeded
        );
        assert!(Message::from_payload(id, big).is_err());
    }
    assert_eq!(PauseReason::from_number(4), PauseReason::RemoteUser);
}
#[test]
fn backend_presentation_uses_open_codes_and_retains_retired_text_as_opaque() {
    let mut observed = support::sample_named("ObservedState");
    observed.insert(5, V::from("synthetic retired text"));
    let state = ObservedState::from_raw(observed.clone()).unwrap();
    assert_eq!(
        state.problems().unwrap().0[0].code().unwrap(),
        UserFacingCode::Unspecified
    );
    roundtrip("ObservedState", observed);
    let mut fault = support::sample_named("ResourceFault");
    fault.insert(2, V::from("synthetic retired text"));
    assert!(ResourceFault::from_raw(fault).unwrap().problem().unwrap().is_some());
    let state = AudioState::from_raw(support::sample_named("AudioState")).unwrap();
    assert_eq!(state.phase().unwrap(), AudioPhase::Idle);
    let mut raw = support::sample_named("AudioRouteFault");
    let problem = UserFacingProblem::from_raw([(1, V::Uint(400)), (900, support::synthetic_unknown())].into()).unwrap();
    raw.insert(3, problem.write());
    assert_eq!(
        AudioRouteFault::from_raw(raw)
            .unwrap()
            .problem()
            .unwrap()
            .code()
            .unwrap(),
        UserFacingCode::Unknown(400)
    );
    for (name, keys) in [
        ("ObservedState", vec![5]),
        ("ResourceFault", vec![2]),
        ("AudioState", vec![1, 2]),
        ("AudioRouteFault", vec![2]),
        ("ProtocolError", vec![10]),
        ("ScreenReady", vec![4]),
    ] {
        assert!(keys.iter().all(|k| {
            registry::RESERVED_FIELDS
                .iter()
                .any(|(n, ks)| *n == name && ks.contains(k))
        }));
    }
}

#[test]
fn low_failure_strings_are_replaced_by_open_codes_in_the_existing_message() {
    let mut raw = support::sample_named("LowUnavailable");
    raw.insert(3, V::from("synthetic retired reason"));
    raw.insert(4, V::Uint(400));
    raw.insert(5, V::Uint(400));
    let failure = LowUnavailable::from_raw(raw.clone()).unwrap();
    assert_eq!(failure.code().unwrap(), LowFailureCode::Unknown(400));
    assert_eq!(failure.stage().unwrap(), Some(TcpStage::Unknown(400)));
    roundtrip("LowUnavailable", raw.clone());
    let mut message = support::sample_named("LowRelayUnavailable");
    message.insert(2, failure.write());
    assert!(matches!(
        Message::from_payload(0x0c2b, message).unwrap(),
        Message::LowRelayUnavailable(_)
    ));
    raw.remove(&4);
    assert!(LowUnavailable::from_raw(raw).is_err());
}
