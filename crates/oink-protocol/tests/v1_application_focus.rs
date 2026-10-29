use oink_protocol::v1::{
    self, ErrorCode, Message, RawValue,
    admission::{Agreement, Context, Phase},
    dto::*,
    frame::{self, OpaqueFrame},
    registry::Transport,
    schema::WireDto,
};
use oink_test_support::protocol as support;

const FEATURE: &str = "oink.input.application-focus/1";

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

#[test]
fn focus_control_requires_both_capabilities_and_reliable_input_lane() {
    let message = Message::ApplicationFocus(
        ApplicationFocusRecord::from_raw(support::sample_named("ApplicationFocusRecord")).unwrap(),
    );
    let bytes = frame::encode(&message, v1::limits::RELIABLE_FRAME).unwrap();
    let frame = OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME).unwrap();
    for capabilities in [vec!["oink.input/1"], vec![FEATURE]] {
        let agreement = Agreement::new(capabilities.into_iter().map(str::to_owned)).unwrap();
        assert_eq!(
            context(&agreement).decode(frame).unwrap_err().code,
            ErrorCode::UnnegotiatedCapability
        );
    }
    let agreement = Agreement::new(["oink.input/1", FEATURE].map(str::to_owned)).unwrap();
    let mut ctx = context(&agreement);
    assert!(ctx.decode(frame).is_ok());
    ctx.transport = Transport::Datagram;
    assert_eq!(ctx.decode(frame).unwrap_err().code, ErrorCode::InvalidChannel);
    ctx.transport = Transport::Reliable;
    ctx.kind = 2;
    assert_eq!(ctx.decode(frame).unwrap_err().code, ErrorCode::InvalidChannel);
}

#[test]
fn terminal_key_cannot_be_downgraded_to_baseline_input() {
    let baseline = Agreement::new(["oink.input/1".to_owned()]).unwrap();
    let enabled = Agreement::new(["oink.input/1", FEATURE].map(str::to_owned)).unwrap();
    for (id, name) in [
        (0x0202, "InputKey"),
        (0x0207, "InputKeyRepeat"),
        (0x0205, "InputHeldSync"),
    ] {
        let mut raw = support::sample_named(name);
        raw.remove(&103);
        raw.remove(&104);
        let baseline_message = Message::from_payload(id, raw.clone()).unwrap();
        let bytes = frame::encode(&baseline_message, v1::limits::RELIABLE_FRAME).unwrap();
        let frame = OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME).unwrap();
        assert!(context(&baseline).decode_hot(frame).is_ok());

        raw.insert(103, RawValue::Map(support::sample_named("ApplicationKeyRoute")));
        let message = Message::from_payload(id, raw).unwrap();
        let bytes = frame::encode(&message, v1::limits::RELIABLE_FRAME).unwrap();
        let frame = OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME).unwrap();
        let violation = context(&baseline).decode_hot(frame).unwrap_err();
        assert_eq!(violation.code, ErrorCode::UnnegotiatedCapability);
        assert_eq!(violation.capability, Some(FEATURE));
        assert!(violation.cleanup);
        assert_eq!(
            context(&baseline).decode(frame).unwrap_err().code,
            ErrorCode::UnnegotiatedCapability
        );
        assert!(context(&enabled).decode_hot(frame).is_ok());
        assert!(context(&enabled).decode(frame).is_ok());
    }
}

#[test]
fn unknown_focus_action_retains_opaque_body_without_becoming_a_known_action() {
    let mut raw = support::sample_named("ApplicationFocusRecord");
    raw.insert(
        2,
        RawValue::Map(
            [
                (1, RawValue::Uint(999)),
                (2, RawValue::Map([(900, support::synthetic_unknown())].into())),
            ]
            .into(),
        ),
    );
    let message = Message::ApplicationFocus(ApplicationFocusRecord::from_raw(raw).unwrap());
    let bytes = frame::encode(&message, v1::limits::RELIABLE_FRAME).unwrap();
    let decoded = OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME)
        .unwrap()
        .decode()
        .unwrap();
    assert_eq!(frame::encode(&decoded, v1::limits::RELIABLE_FRAME).unwrap(), bytes);
}
