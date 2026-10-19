//! Read-only evidence of the actual button ledger when an edge is refused.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ButtonSourcePress {
    pub physical: bool,
    pub event_time_us: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ButtonHold {
    pub source: DeviceId,
    pub button: MouseButton,
    /// Router clock at the ledger insertion; None for an unobserved insertion.
    pub since_us: Option<Micros>,
    /// Only available for a captured event on this holder, never inferred for a peer.
    pub captured: Option<ButtonSourcePress>,
}

#[derive(Clone, Copy, Debug)]
pub struct ButtonRefusal {
    pub at_us: Micros,
    pub epoch: u64,
    pub holder: DeviceId,
    pub holds: [Option<ButtonHold>; 32],
    pub omitted: u32,
    pub dnd_leaving: bool,
}

#[derive(Default)]
pub(super) struct State {
    presses: HashMap<(DeviceId, MouseButton), (Micros, Option<ButtonSourcePress>)>,
    last_refusal: std::cell::Cell<Option<Micros>>,
}
impl State {
    pub(super) fn press(
        &mut self,
        source: DeviceId,
        button: MouseButton,
        now: Micros,
        captured: Option<ButtonSourcePress>,
    ) {
        self.presses.insert((source, button), (now, captured));
        self.end_episode();
    }
    pub(super) fn release(&mut self, source: DeviceId, button: MouseButton) {
        if self.presses.remove(&(source, button)).is_some() {
            self.end_episode();
        }
    }
    pub(super) fn release_origin(&mut self, source: DeviceId) {
        self.presses.retain(|(id, _), _| *id != source);
        self.end_episode();
    }
    pub(super) fn end_episode(&self) {
        self.last_refusal.set(None);
    }
}
impl Router {
    pub(super) fn diagnostic_button_refusal(&self, sink: &dyn RoutingDiagnostics) {
        if !sink.enabled() {
            return;
        }
        // All input/control entry points already set this supplied monotonic clock.
        // No system clock, platform sampling or correction belongs in the router.
        let now = self.dnd.now;
        if self
            .button_diagnostics
            .last_refusal
            .get()
            .is_some_and(|last| now.saturating_sub(last) < 1_000_000)
        {
            return;
        }
        self.button_diagnostics.last_refusal.set(Some(now));
        let mut record = ButtonRefusal {
            at_us: now,
            epoch: self.epoch,
            holder: self.holder,
            holds: [None; 32],
            omitted: 0,
            dnd_leaving: self.dnd_leaving(),
        };
        let local = self
            .buttons
            .iter()
            .filter(|(_, target)| **target == Target::Local)
            .map(|(&button, _)| (self.me(), button));
        let injected = self
            .held
            .iter()
            .flat_map(|(&source, held)| held.buttons.iter().map(move |&button| (source, button)));
        for (count, (source, button)) in local.chain(injected).enumerate() {
            let press = self.button_diagnostics.presses.get(&(source, button));
            let hold = ButtonHold {
                source,
                button,
                since_us: press.map(|p| p.0),
                captured: press.and_then(|p| p.1),
            };
            if count < record.holds.len() {
                record.holds[count] = Some(hold);
            } else {
                record.omitted += 1;
            }
        }
        sink.held_buttons(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Evidence(std::sync::Mutex<Vec<ButtonRefusal>>);
    impl RoutingDiagnostics for Evidence {
        fn enabled(&self) -> bool {
            true
        }
        fn held_buttons(&self, record: ButtonRefusal) {
            self.0.lock().unwrap().push(record);
        }
        fn decision(&self, _: DecisionKind, _: DecisionReason, _: u64, _: DeviceId, _: Option<&ScreenPos>) {}
    }

    #[test]
    fn held_button_refusal_records_ledger_once_per_episode_and_second() {
        let me = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        for os in [Os::MacOS, Os::Windows] {
            for remote in [false, true] {
                for captured_kind in [Some(true), Some(false), None] {
                    let mut layout = crate::SimpleLayout::new();
                    let bounds = Rect::new(0., 0., 100., 100.);
                    layout.add_screen(
                        ScreenId::new(me, "s"),
                        bounds,
                        Rect::new(bounds.x / 686., bounds.y / 686., bounds.w / 686., bounds.h / 686.),
                    );
                    layout.add_screen(
                        ScreenId::new(peer, "s"),
                        bounds,
                        Rect::new(120. / 686.0, 0. / 686.0, 100. / 686.0, 100. / 686.0),
                    );
                    let mut r = Router::new(
                        RouterConfig::new(
                            oink_foundation::session::Boot {
                                device_id: me,
                                nonce: [me.0[0]; 16],
                            },
                            "local",
                            os,
                        ),
                        Arc::new(layout),
                    );
                    r.set_local_displays(vec![DisplayInfo {
                        key: "s".into(),
                        name: "s".into(),
                        bounds,
                        pixel_width: 100,
                        pixel_height: 100,
                        reference_extent: None,
                        rotation_deg: 0,
                        is_primary: true,
                        is_builtin: false,
                        is_virtual: false,
                    }]);
                    r.test_connect(peer, 0);
                    r.peers.get_mut(&peer).unwrap().focus_ready = true;
                    r.test_message(
                        peer,
                        Message::Control(ControlMsg::FocusEnter {
                            epoch: 1,
                            display: "s".into(),
                            pos: Point::new(99., 50.),
                            scope: Some(r.new_focus_scope()),
                            pointer_source: Some(peer),
                        }),
                        1,
                    );
                    let sink = Arc::new(Evidence::default());
                    r.set_diagnostics(sink.clone());
                    let button = |pressed| CapturedEvent {
                        physical: captured_kind.unwrap_or(true),
                        event: InputEvent::Button {
                            button: MouseButton::Right,
                            pressed,
                        },
                        source: SourceKind::Mouse,
                        time_us: 77,
                        cursor: Some(Point::new(99., 50.)),
                    };
                    let press = |r: &mut Router, pressed, now| {
                        if captured_kind.is_some() {
                            r.on_local_event(&button(pressed), now);
                        } else {
                            r.test_message(
                                peer,
                                Message::Input(InputMsg::Button {
                                    button: MouseButton::Right,
                                    pressed,
                                }),
                                now,
                            );
                        }
                    };
                    press(&mut r, true, 10);
                    assert!(r.handoff_blocked());
                    assert_eq!(
                        r.layout
                            .exit_through_edge(
                                &ScreenPos::new(ScreenId::new(me, "s"), Point::new(99.9, 50.)),
                                (10., 0.)
                            )
                            .unwrap()
                            .screen
                            .device,
                        peer
                    );
                    let motion = |r: &mut Router, now| {
                        r.pointer = Some(ScreenPos::new(ScreenId::new(me, "s"), Point::new(99., 50.)));
                        if remote {
                            r.test_message(
                                peer,
                                Message::Input(InputMsg::Motion {
                                    seq: now,
                                    dx: 10. / 686.,
                                    dy: 0.,
                                }),
                                now,
                            );
                        } else {
                            r.on_local_event(
                                &CapturedEvent {
                                    physical: true,
                                    event: InputEvent::PointerDelta { dx: 10., dy: 0. },
                                    source: SourceKind::Mouse,
                                    time_us: now,
                                    cursor: Some(Point::new(99.9, 50.)),
                                },
                                now,
                            );
                        }
                    };
                    for now in [100, 101, 999_999, 1_000_099, 1_000_100] {
                        motion(&mut r, now);
                    }
                    let records = sink.0.lock().unwrap();
                    assert_eq!(records.len(), 2, "first refusal and one-second boundary");
                    for (record, at) in records.iter().zip([100, 1_000_100]) {
                        assert_eq!(record.at_us, at);
                        assert_eq!(record.holder, me);
                        assert_eq!(
                            record.holds.iter().flatten().copied().collect::<Vec<_>>(),
                            vec![ButtonHold {
                                source: if captured_kind.is_some() { me } else { peer },
                                button: MouseButton::Right,
                                since_us: Some(10),
                                captured: captured_kind.map(|physical| ButtonSourcePress {
                                    physical,
                                    event_time_us: 77
                                }),
                            }]
                        );
                        assert_eq!(record.omitted, 0);
                        assert!(!record.dnd_leaving);
                    }
                    drop(records);
                    press(&mut r, false, 1_000_110);
                    press(&mut r, true, 1_000_120);
                    motion(&mut r, 1_000_130);
                    assert_eq!(
                        sink.0.lock().unwrap().len(),
                        3,
                        "new hold starts a new episode immediately"
                    );
                    assert_eq!(r.holder, me, "recording never bypasses the guard");
                }
            }
        }
    }
}
