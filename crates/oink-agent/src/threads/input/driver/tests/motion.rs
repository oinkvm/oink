use super::*;
use crate::api::EventFrequency;
use oink_foundation::input::{KeyCode, MouseButton, ScrollPhase, SourceKind};

fn motion(seq: u64, dx: f64, dy: f64) -> Message {
    Message::Input(InputMsg::Motion { seq, dx, dy })
}

#[test]
fn first_crossing_motion_has_one_budget_deadline_while_focus_is_unpaced() {
    let mut h = Harness::new();
    let (mut read, _) = h.outbound(1);
    drain(&mut read);
    let now = h.driver.now;
    h.driver.budget.consume(now);
    h.driver.budget.consume(now);
    let focus = Message::Control(ControlMsg::FocusEnter {
        epoch: 37,
        display: "screen".into(),
        pos: oink_foundation::Point::new(5., 5.),
        scope: Some(oink_routing::control::FocusScope {
            members: vec![(h.driver.local, h.driver.adapter.routing_session()), (peer(), [2; 16])],
        }),
        pointer_source: Some(h.driver.local),
    });
    h.driver.send(peer(), focus.clone());
    assert_eq!(drain(&mut read), vec![focus]);
    h.driver.now = now + Duration::from_millis(1);
    h.driver.send(peer(), motion(7, 1., 2.));
    assert_eq!(
        h.driver.motion.as_ref().unwrap().crossing.unwrap().0,
        TimingToken { epoch: 37, sequence: 7 }
    );
    h.driver.now = now + Duration::from_millis(7);
    h.driver.send(peer(), motion(8, 3., 4.));
    assert!(drain(&mut read).is_empty());
    assert_eq!(h.driver.input_deadline(), Some(now + Duration::from_millis(8)));
    h.driver.poll(now + Duration::from_millis(8));
    assert_eq!(drain(&mut read), vec![motion(8, 4., 6.)]);
    assert!(h.driver.motion.is_none());
}

#[test]
fn default_budget_coalesces_distance_and_flushes_the_last_move_at_eight_ms() {
    let mut h = Harness::new();
    let (mut read, _datagrams) = h.outbound(1);
    drain(&mut read); // Opening records are not motion.
    let now = h.driver.now;
    assert_eq!(h.driver.budget.frequency(), EventFrequency::Hz125);
    for seq in 0..2 {
        h.driver.send(peer(), motion(seq, 1., -2.));
    }
    assert_eq!(drain(&mut read), vec![motion(0, 1., -2.), motion(1, 1., -2.)]);
    h.driver.now = now + Duration::from_millis(1);
    h.driver.send(peer(), motion(2, 3., -4.));
    h.driver.now = now + Duration::from_millis(7);
    h.driver.send(peer(), motion(3, -1., 1.));
    assert!(drain(&mut read).is_empty());
    assert_eq!(h.driver.input_deadline(), Some(now + Duration::from_millis(8)));
    h.driver.poll(now + Duration::from_millis(8));
    assert_eq!(drain(&mut read), vec![motion(3, 2., -3.)]);
    assert!(h.driver.motion.is_none());
}

#[test]
fn pending_motion_precedes_buttons_keys_scroll_and_focus_handoff_without_waiting() {
    let barriers = [
        Message::Input(InputMsg::Button {
            button: MouseButton::Left,
            pressed: true,
        }),
        Message::Input(InputMsg::Key {
            code: KeyCode::keyboard(4),
            pressed: true,
        }),
        Message::Input(InputMsg::Scroll {
            source: SourceKind::Mouse,
            dx: 0.,
            dy: 1.,
            unit: oink_routing::input::ScrollAmount::Lines,
            phase: ScrollPhase::None,
        }),
        Message::Control(ControlMsg::FocusEnter {
            epoch: 2,
            display: "remote".into(),
            pos: oink_foundation::Point::new(0., 0.),
            scope: Some(oink_routing::control::FocusScope {
                members: vec![(DeviceId([1; 32]), [1; 16]), (peer(), [2; 16])],
            }),
            pointer_source: None,
        }),
    ];
    for barrier in barriers {
        let mut h = Harness::new();
        let (mut read, _datagrams) = h.outbound(1);
        drain(&mut read);
        for seq in 0..4 {
            h.driver.send(peer(), motion(seq, 1., -2.));
        }
        assert_eq!(drain(&mut read).len(), 2);
        assert!(h.driver.motion.is_some());
        h.driver.send(peer(), barrier.clone());
        assert_eq!(drain(&mut read), vec![motion(3, 2., -4.), barrier]);
        assert!(h.driver.motion.is_none());
    }
}

#[test]
fn explicit_unlimited_sends_every_move_and_switching_flushes_the_old_tail() {
    let mut h = Harness::new();
    let (mut read, _datagrams) = h.outbound(1);
    drain(&mut read);
    for seq in 0..4 {
        h.driver.send(peer(), motion(seq, 1., 2.));
    }
    assert_eq!(drain(&mut read).len(), 2);
    h.driver
        .command(InputCommand::EventFrequency(EventFrequency::Unlimited));
    assert_eq!(drain(&mut read), vec![motion(3, 2., 4.)]);
    let expected: Vec<_> = (4..24).map(|seq| motion(seq, 1., 2.)).collect();
    for message in &expected {
        h.driver.send(peer(), message.clone());
    }
    assert_eq!(drain(&mut read), expected);
    assert!(h.driver.motion.is_none());
    assert!(h.driver.input_deadline().is_none());
}

#[test]
fn contact_frames_are_reliable_and_unpaced_at_both_motion_settings() {
    for frequency in [EventFrequency::Hz125, EventFrequency::Unlimited] {
        let mut h = Harness::new();
        let (mut read, _) = h.outbound(1);
        drain(&mut read);
        h.driver.budget.reset(frequency, h.driver.now);
        let messages: Vec<_> = (0..80)
            .map(|time_us| {
                use oink_foundation::contacts as c;
                let mut frame = c::Frame {
                    device: 1,
                    time_us,
                    surface: c::Surface {
                        width: 6000,
                        height: 4000,
                    },
                    phase: if time_us == 0 { c::Phase::Begin } else { c::Phase::Move },
                    len: 1,
                    ..Default::default()
                };
                frame.changes[0] = c::Change {
                    contact: c::Contact {
                        id: 1,
                        x: 1000 + time_us as i32,
                        y: 2000,
                    },
                    phase: frame.phase,
                };
                Message::Input(InputMsg::TouchpadContacts(frame))
            })
            .collect();
        for msg in &messages {
            h.driver.send(peer(), msg.clone());
        }
        assert_eq!(drain(&mut read), messages);
        assert!(h.driver.motion.is_none());
    }
}
