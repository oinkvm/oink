use super::*;
use crate::RoutingSettings;
const ME: DeviceId = DeviceId([1; 32]);
const PEER: DeviceId = DeviceId([2; 32]);
fn router() -> Router {
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: ME,
                nonce: [1; 16],
            },
            "local",
            Os::Other,
        ),
        connection_tests::session_layout(ME, PEER),
    );
    r.test_connect(PEER, 0);
    r.out.clear();
    r
}
fn broadcasts(r: &Router) -> usize {
    r.out
        .iter()
        .filter(|o| {
            matches!(
                o,
                Output::Send {
                    msg: Message::Control(ControlMsg::UserActivity),
                    ..
                }
            )
        })
        .count()
}
#[test]
fn group_activity_is_consumed_even_when_peer_keyboard_and_mouse_sharing_are_off() {
    let mut r = router();
    r.peers.get_mut(&PEER).unwrap().sharing = false;
    assert!(r.accepts_group_user_activity(PEER));
    r.input_message(PEER, Message::Control(ControlMsg::UserActivity), 0);
    assert!(r.out.iter().any(|o| matches!(o, Output::ResetUserIdle)));
    assert_eq!(broadcasts(&r), 0, "receipt never rebroadcasts");
    r.out.clear();
    r.set_settings(RoutingSettings {
        wake_displays_on_input: false,
        ..Default::default()
    });
    r.input_message(PEER, Message::Control(ControlMsg::UserActivity), 1_000_000);
    assert!(!r.out.iter().any(|o| matches!(o, Output::ResetUserIdle)));
    r.test_disconnect(PEER, 2_000_000);
    assert!(!r.accepts_group_user_activity(PEER));
}

#[test]
fn physical_input_still_broadcasts_without_periodic_renewal() {
    let mut r = router();
    let event = oink_foundation::input::CapturedEvent {
        physical: true,
        event: oink_foundation::input::InputEvent::PointerDelta { dx: 1., dy: 0. },
        source: SourceKind::Mouse,
        time_us: 0,
        cursor: None,
    };
    r.on_local_event(&event, 0);
    r.on_local_event(&event, 999_999);
    assert_eq!(broadcasts(&r), 1);
    r.on_local_event(&event, 1_000_000);
    assert_eq!(broadcasts(&r), 2);
    r.tick(10_000_000);
    assert_eq!(broadcasts(&r), 2);
}

#[test]
fn captured_physical_input_and_peer_activity_refresh_but_idle_time_does_not() {
    let mut r = router();
    r.cfg.os = Os::MacOS;
    let event = CapturedEvent {
        physical: true,
        event: InputEvent::Key {
            code: KeyCode::keyboard(4),
            pressed: true,
        },
        source: SourceKind::Keyboard,
        time_us: 0,
        cursor: None,
    };
    r.on_local_event(&event, 0);
    assert!(
        !r.out.iter().any(|o| matches!(o, Output::ResetUserIdle)),
        "passed local input already reaches CGS"
    );
    r.holder = PEER;
    r.out.clear();
    r.on_local_event(&event, 1_000_000);
    assert!(r.out.iter().any(|o| matches!(o, Output::ResetUserIdle)));
    r.out.clear();
    r.input_message(PEER, Message::Control(ControlMsg::UserActivity), 2_000_000);
    assert!(r.out.iter().any(|o| matches!(o, Output::ResetUserIdle)));
    r.out.clear();
    for second in 3..1200 {
        r.tick(second * 1_000_000);
    }
    assert!(!r.out.iter().any(|o| matches!(o, Output::ResetUserIdle)));
}

#[test]
fn peer_activity_while_paused_or_disconnected_never_refreshes() {
    let mut r = router();
    r.paused = true;
    r.input_message(PEER, Message::Control(ControlMsg::UserActivity), 0);
    assert!(!r.out.iter().any(|o| matches!(o, Output::ResetUserIdle)));
    r.paused = false;
    r.test_disconnect(PEER, 1);
    r.out.clear();
    r.input_message(PEER, Message::Control(ControlMsg::UserActivity), 2);
    assert!(!r.out.iter().any(|o| matches!(o, Output::ResetUserIdle)));
}

fn contact_event(len: u8, physical: bool) -> CapturedEvent {
    let mut sample = oink_foundation::contacts::Sample {
        device: 1,
        surface: oink_foundation::contacts::Surface {
            width: 15000,
            height: 9000,
        },
        len,
        ..Default::default()
    };
    sample.contacts[0] = oink_foundation::contacts::Contact {
        id: 1,
        x: 5000,
        y: 4000,
    };
    CapturedEvent {
        physical,
        event: InputEvent::TouchpadSample(sample),
        source: SourceKind::Touchpad,
        time_us: 0,
        cursor: None,
    }
}

#[test]
fn contact_only_input_refreshes_local_and_group_idle_before_early_return() {
    let mut r = router();
    r.cfg.os = Os::MacOS;
    r.holder = PEER;
    let contact = contact_event(1, true);
    r.on_local_event(&contact, 0);
    assert!(r.out.iter().any(|o| matches!(o, Output::LocalUserActivity)));
    assert!(r.out.iter().any(|o| matches!(o, Output::ResetUserIdle)));
    assert_eq!(broadcasts(&r), 1);
    r.out.clear();
    r.on_local_event(&contact, 999_999);
    assert_eq!(broadcasts(&r), 0);
    r.on_local_event(&contact, 1_000_000);
    assert_eq!(broadcasts(&r), 1);
    r.out.clear();
    r.tick(3_000_000);
    assert_eq!(broadcasts(&r), 0, "no timer-driven renewal after contacts stop");
}

#[test]
fn contact_idle_activity_rejects_faults_boundaries_empty_and_synthetic_frames() {
    for event in [
        contact_event(255, true),
        contact_event(0, true),
        contact_event(1, false),
        CapturedEvent {
            event: InputEvent::TouchpadBoundary,
            ..contact_event(1, true)
        },
    ] {
        let mut r = router();
        r.on_local_event(&event, 0);
        assert!(
            !r.out
                .iter()
                .any(|o| matches!(o, Output::LocalUserActivity | Output::ResetUserIdle))
        );
        assert_eq!(broadcasts(&r), 0);
    }
}

#[test]
fn contact_activity_respects_pause_capture_and_wake_gates() {
    for (paused, fully_paused, capture_available, wake) in [
        (false, false, true, false),
        (true, false, true, true),
        (true, true, true, true),
        (false, false, false, true),
    ] {
        let mut r = router();
        r.paused = paused;
        r.fully_paused = fully_paused;
        r.capture_available = capture_available;
        r.cfg.settings.wake_displays_on_input = wake;
        r.on_local_event(&contact_event(1, true), 0);
        let local = !fully_paused && capture_available;
        assert_eq!(r.out.iter().any(|o| matches!(o, Output::LocalUserActivity)), local);
        assert!(!r.out.iter().any(|o| matches!(o, Output::ResetUserIdle)));
        assert_eq!(broadcasts(&r), usize::from(local && !paused));
    }
}
