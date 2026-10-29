use super::*;
use crate::application_focus as a;
use oink_foundation::{
    application_focus::{Foreground, Observation, ObservationStamp, ParkResult},
    session::Boot,
};

const LETTER: KeyCode = KeyCode::keyboard(4);
fn boot(n: u8) -> Boot {
    Boot {
        device_id: DeviceId([n; 32]),
        nonce: [n; 16],
    }
}

struct Group {
    routers: Vec<Router>,
    foreground: Vec<Foreground>,
    sequence: Vec<u64>,
    observations: Vec<Option<Observation>>,
    injected: Vec<(usize, KeyCode, bool)>,
    park_result: Option<ParkResult>,
    held_parks: Vec<(usize, a::ParkRequest)>,
    skip_sample: Option<usize>,
    native_work: Vec<(usize, ApplicationFocusCommand)>,
}
impl Group {
    fn new() -> Self {
        Self::with_pointer(1)
    }
    fn with_pointer(pointer: u8) -> Self {
        Self::with_park_result(pointer, Some(ParkResult::Confirmed))
    }
    fn with_park_result(pointer: u8, park_result: Option<ParkResult>) -> Self {
        Self::with_feature(pointer, park_result, true)
    }
    fn with_feature(pointer: u8, park_result: Option<ParkResult>, enabled: bool) -> Self {
        let mut routers: Vec<_> = (1..=3)
            .map(|n| {
                Router::new(
                    RouterConfig::new(boot(n), "test", if n == 2 { Os::Windows } else { Os::MacOS }),
                    Arc::new(crate::SimpleLayout::new()),
                )
            })
            .collect();
        let sessions: Vec<_> = routers.iter().map(|r| r.session).collect();
        let scope = FocusScope {
            members: (1..=3).map(|n| (boot(n).device_id, sessions[n as usize - 1])).collect(),
        };
        for (i, r) in routers.iter_mut().enumerate() {
            for n in 1..=3 {
                if n as usize != i + 1 {
                    r.attach_input(
                        boot(n),
                        sessions[n as usize - 1],
                        true,
                        false,
                        FeelProfile::default(),
                        0,
                    );
                    r.peers.get_mut(&boot(n).device_id).unwrap().focus_ready = true;
                }
            }
            r.out.clear();
            r.focus_scope = Some(scope.clone());
            r.focus_joined = true;
            r.fresh_join = false;
            r.epoch = 1;
            r.holder = boot(pointer).device_id;
            r.claim = boot(pointer).device_id;
            for n in 1..=3 {
                r.set_application_focus_participation(boot(n), true, (InputMode::Shared, InputMode::Shared), 0);
                r.set_application_focus_enabled(enabled, 0);
            }
        }
        let mut group = Self {
            routers,
            foreground: vec![Foreground::Windowed; 3],
            sequence: vec![0; 3],
            observations: vec![None; 3],
            injected: vec![],
            park_result,
            held_parks: vec![],
            skip_sample: None,
            native_work: vec![],
        };
        group.pump();
        group
    }
    fn observe(&mut self, i: usize, operation: Option<u64>, hidden_lost: bool) {
        self.sequence[i] += 1;
        let claim = self.routers[i].application_focus().unwrap().claim.clone();
        let observation = Observation {
            stamp: ObservationStamp {
                generation: 1,
                sequence: self.sequence[i],
            },
            foreground: self.foreground[i],
            hidden_lost,
            park_operation: operation,
        };
        self.observations[i] = Some(observation);
        self.routers[i].application_focus_observed(&claim, observation, 10);
    }

    fn pump(&mut self) {
        for _ in 0..100 {
            let mut pending = Vec::new();
            for (i, r) in self.routers.iter_mut().enumerate() {
                pending.extend(r.drain_outputs().into_iter().map(|output| (i, output)));
            }
            if pending.is_empty() {
                return;
            }
            for (i, output) in pending {
                if let Output::ApplicationFocus(command) = &output {
                    self.native_work.push((i, command.clone()));
                }
                match output {
                    Output::Send { to, msg, .. } => {
                        self.routers[to.0[0] as usize - 1].input_message(boot(i as u8 + 1).device_id, msg, 10);
                    }
                    Output::Inject(InjectEvent::Key { code, pressed }) => self.injected.push((i, code, pressed)),
                    Output::ApplicationFocus(ApplicationFocusCommand::Sample(claim)) => {
                        if self.skip_sample == Some(i) {
                            continue;
                        }
                        if self.routers[i].application_focus().is_some_and(|s| s.claim == claim) {
                            // Sampling an unchanged native foreground preserves
                            // both its activation stamp and park-operation tag.
                            if let Some(observation) =
                                self.observations[i].filter(|o| o.foreground == self.foreground[i])
                            {
                                self.routers[i].application_focus_observed(&claim, observation, 10);
                            } else {
                                self.observe(i, None, false);
                            }
                        }
                    }
                    Output::ApplicationFocus(ApplicationFocusCommand::Park(request)) => {
                        if self.observations[i].is_none_or(|o| o.stamp != request.expected) {
                            self.routers[i].application_focus_park_completed(&request, ParkResult::Stale, 10);
                            continue;
                        }
                        let Some(result) = self.park_result else {
                            self.held_parks.push((i, request));
                            continue;
                        };
                        if result != ParkResult::Confirmed {
                            self.routers[i].application_focus_park_completed(&request, result, 10);
                            continue;
                        }
                        self.foreground[i] = Foreground::Hidden;
                        self.observe(i, Some(request.operation), false);
                        self.routers[i].application_focus_park_completed(&request, ParkResult::Confirmed, 10);
                    }
                    _ => {}
                }
            }
        }
        panic!("application-focus effects did not settle");
    }
    fn enter_hidden(&mut self) {
        assert_eq!(self.foreground[1], Foreground::Hidden);
        for r in &mut self.routers {
            r.epoch = 2;
            r.claim = boot(2).device_id;
            r.holder = boot(2).device_id;
            r.application_sync(20);
        }
        self.pump();
        for r in &self.routers {
            let state = r.application_focus().unwrap();
            assert_eq!(state.phase, a::Phase::Active);
            assert_eq!(state.keyboard, Some(boot(1)));
        }
        self.injected.clear();
    }
    fn source_key(&mut self, code: KeyCode, pressed: bool) {
        self.routers[1].input_message(boot(3).device_id, Message::Input(InputMsg::Key { code, pressed }), 30);
        self.pump();
    }
}

#[test]
fn remote_source_delivers_once_to_p_and_k_then_retires_only_k_refs() {
    let mut g = Group::new();
    g.enter_hidden();
    g.source_key(LETTER, true);
    assert_eq!(g.injected.iter().filter(|(_, c, p)| *c == LETTER && *p).count(), 2);
    assert!(g.injected.contains(&(0, LETTER, true)));
    assert!(g.injected.contains(&(1, LETTER, true)));
    g.injected.clear();
    g.foreground[1] = Foreground::Windowed;
    g.observe(1, None, true);
    g.pump();
    assert!(g.injected.contains(&(0, LETTER, false)));
    assert!(!g.injected.contains(&(1, LETTER, false)));
    assert_eq!(g.routers[1].application_focus().unwrap().keyboard, Some(boot(2)));
    g.injected.clear();
    g.routers[1].input_message(
        boot(3).device_id,
        Message::Input(InputMsg::KeyRepeat { code: LETTER, n: 1 }),
        40,
    );
    g.pump();
    assert_eq!(g.injected, vec![(1, LETTER, true)]);
    g.injected.clear();
    g.source_key(LETTER, false);
    assert_eq!(g.injected, vec![(1, LETTER, false)]);
}

#[test]
fn local_p_passes_native_key_and_terminal_k_never_forwards_back() {
    let mut g = Group::new();
    g.enter_hidden();
    for pressed in [true, false] {
        assert_eq!(
            g.routers[1].on_local_event(
                &CapturedEvent {
                    time_us: 30,
                    physical: true,
                    source: SourceKind::Keyboard,
                    cursor: None,
                    event: InputEvent::Key { code: LETTER, pressed },
                },
                30
            ),
            Disposition::Pass
        );
        g.pump();
    }
    assert_eq!(g.injected, vec![(0, LETTER, true), (0, LETTER, false)]);
}

#[test]
fn group_disable_releases_application_refs_without_releasing_baseline_ref() {
    let mut g = Group::new();
    g.enter_hidden();
    g.routers[0].target_mapped_key(boot(3).device_id, LETTER, LETTER, true);
    g.source_key(LETTER, true);
    assert_eq!(g.routers[0].key_refs.get(&LETTER), Some(&2));
    g.injected.clear();
    g.routers[0].set_application_focus_enabled(false, 40);
    g.pump();
    assert_eq!(g.routers[0].key_refs.get(&LETTER), Some(&1));
    assert!(!g.injected.contains(&(0, LETTER, false)));
    g.routers[0].target_key(boot(3).device_id, LETTER, false);
    g.pump();
    assert!(g.injected.contains(&(0, LETTER, false)));
}

#[test]
fn terminal_authentication_hop_limit_and_retirement_fence_are_enforced() {
    let mut g = Group::new();
    g.enter_hidden();
    let state = g.routers[0].application_focus().unwrap().clone();
    let token = a::keys::KeyRoute {
        epoch: state.claim.epoch,
        pointer: boot(2),
        revision: state.revision,
        destination: boot(1),
        press_id: 1,
    };
    let message = |token, hops| {
        Message::Input(InputMsg::Forwarded {
            origin: boot(3),
            hops,
            msg: Box::new(InputMsg::ApplicationKey {
                route: token,
                code: LETTER,
                physical_code: None,
                pressed: true,
            }),
        })
    };
    for (sender, hops) in [(boot(3), 1), (boot(2), 3)] {
        g.routers[0].input_message(sender.device_id, message(token.clone(), hops), 30);
    }
    let mut wrong = token.clone();
    wrong.pointer.nonce = [99; 16];
    g.routers[0].input_message(boot(2).device_id, message(wrong, 1), 30);
    g.pump();
    assert!(g.injected.is_empty());
    g.routers[0].input_message(boot(2).device_id, message(token.clone(), 1), 30);
    g.pump();
    assert_eq!(g.injected, vec![(0, LETTER, true)]);
    let route = a::Route {
        claim: state.claim.clone(),
        revision: state.revision,
        destination: boot(1),
    };
    g.routers[0].input_message(
        boot(2).device_id,
        Message::Control(ControlMsg::ApplicationFocus(a::Message {
            claim: state.claim,
            action: a::Action::Retire(route),
        })),
        30,
    );
    g.pump();
    g.injected.clear();
    let mut next = token;
    next.press_id = 2;
    g.routers[0].input_message(boot(2).device_id, message(next, 1), 30);
    g.pump();
    assert!(g.injected.is_empty(), "retired revision cannot receive a fresh DOWN");
}

#[test]
fn resolved_caps_and_physical_shift_are_independent_application_refs() {
    let mut g = Group::new();
    g.enter_hidden();
    let shift = KeyCode::LEFT_SHIFT;
    g.routers[1].input_message(
        boot(3).device_id,
        Message::Input(InputMsg::CapsKey {
            code: shift,
            pressed: true,
        }),
        30,
    );
    g.pump();
    g.source_key(shift, true);
    assert_eq!(g.routers[0].key_refs.get(&shift), Some(&2));
    assert_eq!(g.routers[1].key_refs.get(&shift), Some(&2));
    g.injected.clear();
    g.routers[1].input_message(
        boot(3).device_id,
        Message::Input(InputMsg::CapsKey {
            code: shift,
            pressed: false,
        }),
        30,
    );
    g.pump();
    assert!(g.injected.is_empty());
    g.source_key(shift, false);
    assert_eq!(
        g.injected
            .iter()
            .filter(|(_, code, pressed)| *code == shift && !pressed)
            .count(),
        2
    );
}

#[test]
fn old_keyboard_owner_never_gets_extra_input_and_p_keeps_normal_delivery() {
    let mut g = Group::new();
    for r in &mut g.routers {
        r.set_application_focus_capability(boot(1), false, 20);
    }
    g.pump();
    for r in &mut g.routers {
        r.epoch = 2;
        r.claim = boot(2).device_id;
        r.holder = boot(2).device_id;
        r.application_sync(20);
    }
    g.pump();
    assert_eq!(g.routers[1].application_focus().unwrap().keyboard, Some(boot(1)));
    g.injected.clear();
    g.source_key(LETTER, true);
    g.source_key(LETTER, false);
    assert_eq!(g.injected, vec![(1, LETTER, true), (1, LETTER, false)]);
}

#[test]
fn lost_application_release_heals_without_touching_baseline_or_replaying_down() {
    let mut g = Group::new();
    g.enter_hidden();
    g.source_key(LETTER, true);
    // Lose the entire outgoing UP batch. P still releases its own native ref.
    g.routers[1].input_message(
        boot(3).device_id,
        Message::Input(InputMsg::Key {
            code: LETTER,
            pressed: false,
        }),
        40,
    );
    g.routers[1].out.clear();
    assert_eq!(g.routers[0].key_refs.get(&LETTER), Some(&1));
    assert!(!g.routers[1].key_refs.contains_key(&LETTER));
    g.injected.clear();
    g.routers[1].application_state_sync(true);
    g.pump();
    assert_eq!(g.injected, vec![(0, LETTER, false)]);
    g.injected.clear();
    g.routers[1].application_state_sync(true);
    g.pump();
    assert!(g.injected.is_empty());
}

#[test]
fn gossip_repairs_lost_commit_without_parking_or_migrating_held_keys() {
    let mut g = Group::new();
    g.enter_hidden();
    g.foreground[1] = Foreground::Windowed;
    g.observe(1, None, true);
    g.routers[1].out.clear();
    assert_eq!(g.routers[0].application_focus().unwrap().keyboard, Some(boot(1)));
    g.routers[1].application_state_sync(true);
    g.pump();
    assert_eq!(g.routers[0].application_focus().unwrap().keyboard, Some(boot(2)));
    assert!(g.injected.is_empty());
}

#[test]
fn pointer_input_stream_loss_releases_application_keys_even_while_boot_is_admitted() {
    let mut g = Group::new();
    g.enter_hidden();
    g.source_key(LETTER, true);
    g.injected.clear();
    g.routers[0].detach_input(boot(2).device_id);
    assert_eq!(g.routers[0].admitted_boots.get(&boot(2).device_id), Some(&boot(2)));
    g.pump();
    assert!(!g.routers[0].key_refs.contains_key(&LETTER));
    assert!(g.injected.contains(&(0, LETTER, false)));
}

#[test]
fn capture_unavailable_retires_extra_delivery_and_preserves_no_stale_repeat() {
    let mut g = Group::new();
    g.enter_hidden();
    g.source_key(LETTER, true);
    g.injected.clear();
    g.routers[1].set_capture_available(false);
    g.pump();
    assert!(!g.routers[0].key_refs.contains_key(&LETTER));
    assert!(g.injected.contains(&(0, LETTER, false)));
}

#[test]
fn two_business_hops_preserve_source_and_terminal_ref() {
    let mut g = Group::new();
    g.enter_hidden();
    // An old pointer has already forwarded S's key once; the P->K copy is hop 2.
    for pressed in [true, false] {
        g.routers[1].input_message(
            boot(1).device_id,
            Message::Input(InputMsg::Forwarded {
                origin: boot(3),
                hops: 1,
                msg: Box::new(InputMsg::Key { code: LETTER, pressed }),
            }),
            40,
        );
        g.pump();
    }
    assert_eq!(g.injected.iter().filter(|(i, _, p)| *i == 0 && *p).count(), 1);
    assert_eq!(g.injected.iter().filter(|(i, _, p)| *i == 1 && *p).count(), 1);
    assert!(!g.routers[0].key_refs.contains_key(&LETTER));
}

#[test]
fn each_destination_maps_original_source_once_and_releases_frozen_mapping() {
    use oink_foundation::input_transform::{InputTransform, KeyMapping, KeyPreset, KeyboardMapping, TargetOverride};
    let mut g = Group::new();
    g.enter_hidden();
    let on_p = KeyCode::keyboard(5);
    let on_k = KeyCode::keyboard(6);
    let transform = InputTransform {
        overrides: [(boot(1).device_id, on_k), (boot(2).device_id, on_p)]
            .into_iter()
            .map(|(target, to)| TargetOverride {
                target,
                reverse_wheel: None,
                keyboard: Some(KeyboardMapping {
                    saved_custom: None,
                    preset: KeyPreset::Custom,
                    caps_tap_hold: None,
                    table: vec![KeyMapping { from: LETTER, to }],
                }),
            })
            .collect(),
        ..InputTransform::default()
    };
    for r in &mut g.routers {
        r.set_input_transforms([(boot(3).device_id, transform.clone())].into());
    }
    g.source_key(LETTER, true);
    assert!(g.injected.contains(&(0, on_k, true)));
    assert!(g.injected.contains(&(1, on_p, true)));
    g.injected.clear();
    for r in &mut g.routers {
        r.set_input_transforms(HashMap::new());
    }
    g.source_key(LETTER, false);
    assert!(g.injected.contains(&(0, on_k, false)));
    assert!(g.injected.contains(&(1, on_p, false)));
}

#[test]
fn game_mode_transition_retires_extra_application_keys_and_keeps_pointer_delivery() {
    let mut g = Group::new();
    g.enter_hidden();
    g.source_key(LETTER, true);
    g.injected.clear();
    g.routers[1].set_game_mode(GameMode::On);
    g.pump();
    let state = g.routers[1].application_focus().unwrap();
    assert_eq!(state.phase, a::Phase::Suspended);
    assert_eq!(state.reason, a::Reason::Game);
    assert!(!g.routers[0].key_refs.contains_key(&LETTER));
    assert_eq!(g.routers[1].key_refs.get(&LETTER), Some(&1));
    assert!(g.injected.contains(&(0, LETTER, false)));
    assert!(!g.injected.contains(&(1, LETTER, false)));
    g.source_key(LETTER, false);
    g.injected.clear();
    g.source_key(LETTER, true);
    g.source_key(LETTER, false);
    assert!(!g.injected.iter().any(|(device, _, _)| *device == 0));
    assert_eq!(g.injected, vec![(1, LETTER, true), (1, LETTER, false)]);
}

#[test]
fn duplicate_readiness_and_authority_samples_allow_initial_parking() {
    let g = Group::new();
    assert_eq!(g.foreground[1], Foreground::Hidden, "non-K Windows must park");
    assert_eq!(g.foreground[2], Foreground::Hidden, "non-K Mac must park");
}

#[test]
fn windows_physical_mouse_enters_mac_without_click_and_both_keyboards_keep_windows_k() {
    for keyboard in [0, 1] {
        let mut g = Group::with_pointer(2);
        let bounds = Rect::new(0., 0., 1000., 600.);
        let mut layout = crate::SimpleLayout::new();
        layout.add_screen(
            ScreenId::new(boot(2).device_id, "screen"),
            bounds,
            Rect::new(0., 0., 1., 0.6),
        );
        layout.add_screen(
            ScreenId::new(boot(1).device_id, "screen"),
            bounds,
            Rect::new(1., 0., 1., 0.6),
        );
        let layout = Arc::new(layout);
        for r in &mut g.routers {
            r.set_layout(layout.clone());
            r.set_local_displays(vec![DisplayInfo {
                key: "screen".into(),
                name: "screen".into(),
                bounds,
                pixel_width: 1000,
                pixel_height: 600,
                reference_extent: None,
                rotation_deg: 0,
                is_primary: true,
                is_builtin: false,
                is_virtual: false,
            }]);
        }
        g.pump();
        // The previous P uses its own physical mouse to focus its browser.
        for pressed in [true, false] {
            g.routers[1].on_local_event(
                &CapturedEvent {
                    time_us: 20,
                    physical: true,
                    source: SourceKind::Mouse,
                    cursor: None,
                    event: InputEvent::Button {
                        button: MouseButton::Left,
                        pressed,
                    },
                },
                20,
            );
            g.pump();
        }
        g.routers[1].set_cursor(Point::new(999., 300.));
        g.routers[1].on_local_event(
            &CapturedEvent {
                time_us: 30,
                physical: true,
                source: SourceKind::Mouse,
                cursor: Some(Point::new(999., 300.)),
                event: InputEvent::PointerDelta { dx: 100., dy: 0. },
            },
            30,
        );
        g.pump();
        assert_eq!(g.routers[0].focus().holder, boot(1).device_id);
        assert_eq!(g.routers[0].application_focus().unwrap().keyboard, Some(boot(2)));
        assert_eq!(g.foreground[0], Foreground::Hidden);
        g.injected.clear();
        for pressed in [true, false] {
            let disposition = g.routers[keyboard].on_local_event(
                &CapturedEvent {
                    time_us: 40,
                    physical: true,
                    source: SourceKind::Keyboard,
                    cursor: None,
                    event: InputEvent::Key { code: LETTER, pressed },
                },
                40,
            );
            assert_eq!(
                disposition,
                if keyboard == 0 {
                    Disposition::Pass
                } else {
                    Disposition::Swallow
                }
            );
            g.pump();
        }
        // P keeps baseline delivery (its native sink consumes it); K receives
        // exactly one application-forward press/release for either source.
        assert_eq!(
            g.injected
                .iter()
                .filter(|(i, _, _)| *i == 1)
                .copied()
                .collect::<Vec<_>>(),
            vec![(1, LETTER, true), (1, LETTER, false)]
        );
    }
}

#[test]
fn application_focus_evidence_uses_numeric_diagnostic_sink() {
    #[derive(Default)]
    struct Evidence(std::sync::Mutex<Vec<a::Diagnostic>>);
    impl RoutingDiagnostics for Evidence {
        fn enabled(&self) -> bool {
            true
        }
        fn decision(&self, _: DecisionKind, _: DecisionReason, _: u64, _: DeviceId, _: Option<&ScreenPos>) {}
        fn application_focus(&self, record: a::Diagnostic) {
            self.0.lock().unwrap().push(record);
        }
    }
    let evidence = Arc::new(Evidence::default());
    let mut g = Group::new();
    for r in &mut g.routers {
        r.set_diagnostics(evidence.clone());
    }
    g.enter_hidden();
    g.source_key(LETTER, true);
    let records = evidence.0.lock().unwrap();
    assert!(records.iter().any(|r| matches!(
        r,
        a::Diagnostic::State {
            phase: a::Phase::Active,
            ..
        }
    )));
    assert!(
        records
            .iter()
            .any(|r| matches!(r, a::Diagnostic::Observation { accepted: true, .. }))
    );
    for expected in [a::KeyStage::Sent, a::KeyStage::Received, a::KeyStage::Injected] {
        assert!(
            records
                .iter()
                .any(|r| matches!(r, a::Diagnostic::Key { stage, .. } if *stage == expected)),
            "{expected:?}"
        );
    }
}

#[test]
fn clicking_new_keyboard_owner_parks_previous_owner_from_fresh_evidence() {
    let mut g = Group::new();
    g.enter_hidden();
    for pressed in [true, false] {
        g.routers[1].on_local_event(
            &CapturedEvent {
                time_us: 30,
                physical: true,
                source: SourceKind::Mouse,
                cursor: None,
                event: InputEvent::Button {
                    button: MouseButton::Left,
                    pressed,
                },
            },
            30,
        );
        if pressed {
            g.pump();
            g.foreground[1] = Foreground::Windowed;
            g.observe(1, None, true);
            g.pump();
            // The old K has switched ordinary windows since the authority's
            // cached observation. Native refresh sees it before its next poll
            // notifies routing; the resume Sample must supply this new stamp.
            let observation = g.observations[0].as_mut().unwrap();
            g.sequence[0] += 1;
            observation.stamp.sequence = g.sequence[0];
        }
    }
    g.pump();
    assert_eq!(g.routers[1].application_focus().unwrap().keyboard, Some(boot(2)));
    assert_eq!(
        g.foreground[0],
        Foreground::Hidden,
        "park used cached rather than fresh native evidence"
    );
}

// Entry is decided from confirmed foreground evidence, never a pending park.
fn claim_second(g: &mut Group) {
    for r in &mut g.routers {
        r.epoch = 2;
        r.claim = boot(2).device_id;
        r.holder = boot(2).device_id;
        r.application_sync(20);
    }
}
fn assert_second_gets_keys_only(g: &mut Group) {
    g.injected.clear();
    g.source_key(LETTER, true);
    g.source_key(LETTER, false);
    assert_eq!(g.injected, vec![(1, LETTER, true), (1, LETTER, false)]);
    g.injected.clear();
    for pressed in [true, false] {
        assert_eq!(
            g.routers[1].on_local_event(
                &CapturedEvent {
                    time_us: 40,
                    physical: true,
                    source: SourceKind::Keyboard,
                    cursor: None,
                    event: InputEvent::Key { code: LETTER, pressed },
                },
                40
            ),
            Disposition::Pass
        );
        g.pump();
    }
    assert!(g.injected.is_empty());
}

#[test]
fn entry_without_confirmed_hidden_claims_keyboard_before_sampling() {
    for foreground in [
        Foreground::Windowed,
        Foreground::Fullscreen,
        Foreground::OrdinaryUnknown,
        Foreground::Unknown,
        Foreground::Unavailable,
    ] {
        let mut g = Group::new();
        g.foreground[1] = foreground;
        g.observe(1, None, true);
        g.pump();
        claim_second(&mut g);
        assert_eq!(
            g.routers[1].application_focus().unwrap().keyboard,
            Some(boot(2)),
            "{foreground:?}"
        );
        g.pump();
        assert_second_gets_keys_only(&mut g);
    }
}

#[test]
fn denied_or_exempt_windows_park_never_keeps_keyboard_on_previous_device() {
    for result in [
        ParkResult::Denied,
        ParkResult::Stale,
        ParkResult::UnknownExempt,
        ParkResult::FullscreenExempt,
        ParkResult::Unavailable,
    ] {
        let mut g = Group::with_park_result(1, Some(result));
        claim_second(&mut g);
        assert_eq!(
            g.routers[1].application_focus().unwrap().keyboard,
            Some(boot(2)),
            "{result:?}"
        );
        g.pump();
        assert_second_gets_keys_only(&mut g);
    }
}

#[test]
fn unconfirmed_park_and_late_completion_cannot_reclaim_keyboard_after_entry() {
    let mut g = Group::with_park_result(1, None);
    let (_, request) = g.held_parks.iter().find(|(i, _)| *i == 1).unwrap().clone();
    claim_second(&mut g);
    assert_eq!(g.routers[1].application_focus().unwrap().keyboard, Some(boot(2)));
    // Physical keys arriving before native Sample/confirmation are passed
    // locally without waiting; a pending park never authorizes forwarding.
    g.injected.clear();
    for pressed in [true, false] {
        assert_eq!(
            g.routers[1].on_local_event(
                &CapturedEvent {
                    time_us: 21,
                    physical: true,
                    source: SourceKind::Keyboard,
                    cursor: None,
                    event: InputEvent::Key { code: LETTER, pressed },
                },
                21
            ),
            Disposition::Pass
        );
    }
    g.pump();
    assert!(g.injected.is_empty(), "unconfirmed park forwarded an entry key");
    assert_second_gets_keys_only(&mut g);
    // Old-claim success is fenced. Even a later Hidden observation cannot
    // restore the keyboard owner which entry already replaced.
    g.routers[1].application_focus_park_completed(&request, ParkResult::Confirmed, 50);
    g.foreground[1] = Foreground::Hidden;
    g.observe(1, Some(request.operation), false);
    g.pump();
    assert_eq!(g.routers[1].application_focus().unwrap().keyboard, Some(boot(2)));
    assert_second_gets_keys_only(&mut g);
}

#[test]
fn unknown_on_pointer_retires_gap_forwarding_immediately() {
    let mut g = Group::new();
    g.enter_hidden();
    // OS has changed foreground but its notification has not arrived yet.
    g.foreground[1] = Foreground::Unknown;
    g.source_key(LETTER, true);
    assert!(g.injected.contains(&(0, LETTER, true)));
    assert!(g.injected.contains(&(1, LETTER, true)));
    g.source_key(LETTER, false);
    g.observe(1, None, true);
    assert_eq!(g.routers[1].application_focus().unwrap().keyboard, Some(boot(2)));
    g.pump();
    assert_second_gets_keys_only(&mut g);
}

#[test]
fn missing_entry_sample_times_out_without_retaining_previous_keyboard() {
    let mut g = Group::new();
    g.skip_sample = Some(1);
    claim_second(&mut g);
    g.pump();
    g.routers[1].application.model.tick(u64::MAX);
    g.routers[1].application_effects();
    assert_eq!(g.routers[1].application_focus().unwrap().keyboard, Some(boot(2)));
    g.pump();
    assert_second_gets_keys_only(&mut g);
}

#[test]
fn timed_out_windows_park_is_not_retried_on_entry() {
    let mut g = Group::with_park_result(1, None);
    assert_eq!(g.held_parks.iter().filter(|(i, _)| *i == 1).count(), 1);
    for r in &mut g.routers {
        r.application.model.tick(a::PARK_TIMEOUT_US + 20);
        r.application_effects();
    }
    g.pump();
    claim_second(&mut g);
    assert_eq!(g.routers[1].application_focus().unwrap().keyboard, Some(boot(2)));
    g.pump();
    assert_second_gets_keys_only(&mut g);
    assert_eq!(g.held_parks.iter().filter(|(i, _)| *i == 1).count(), 1);
}

#[test]
fn disabled_after_use_is_quiet_across_context_changes_and_late_messages() {
    let mut g = Group::new();
    g.enter_hidden();
    let old = g.routers[1].application_focus().unwrap().clone();
    for r in &mut g.routers {
        r.set_application_focus_enabled(false, 40);
    }
    g.pump(); // Only the disable transition may release keys/cancel/dismiss.
    for r in &g.routers {
        assert!(r.application_focus().is_none());
    }
    for r in &mut g.routers {
        r.epoch += 1;
        r.application_sync(50);
        r.application_state_sync(true);
        r.application_focus_observed(
            &old.claim,
            Observation {
                stamp: ObservationStamp {
                    generation: 1,
                    sequence: 999,
                },
                foreground: Foreground::Hidden,
                hidden_lost: false,
                park_operation: None,
            },
            50,
        );
        r.application.model.tick(u64::MAX);
        r.application_effects();
        assert!(r.drain_outputs().is_empty());
    }
    g.source_key(LETTER, true);
    g.source_key(LETTER, false);
    assert_eq!(g.injected, vec![(1, LETTER, true), (1, LETTER, false)]);
}

#[test]
fn disabled_feature_matches_never_enabled_routing_with_capable_peers() {
    let mut used = Group::new();
    used.enter_hidden();
    for r in &mut used.routers {
        r.set_application_focus_enabled(false, 40);
    }
    used.pump();
    let mut baseline = Group::with_feature(1, Some(ParkResult::Confirmed), false);
    for g in [&mut used, &mut baseline] {
        claim_second(g);
        g.pump();
        g.injected.clear();
        for r in &g.routers {
            assert!(r.application.model.deadline().is_none());
            assert!(r.application_focus().is_none());
        }
        g.source_key(LETTER, true);
        g.source_key(LETTER, false);
    }
    assert_eq!(used.injected, baseline.injected);
    assert_eq!(baseline.injected, vec![(1, LETTER, true), (1, LETTER, false)]);
    // Repeated state/heartbeat work cannot schedule native or feature traffic.
    for g in [&mut used, &mut baseline] {
        for r in &mut g.routers {
            r.application_state_sync(true);
            r.application_sync(60);
            r.application_effects();
            assert!(r.drain_outputs().is_empty());
        }
    }
}

#[test]
fn disabled_ingress_produces_no_application_diagnostics_or_outputs() {
    struct RejectEvidence;
    impl RoutingDiagnostics for RejectEvidence {
        fn enabled(&self) -> bool {
            true
        }
        fn decision(&self, _: DecisionKind, _: DecisionReason, _: u64, _: DeviceId, _: Option<&ScreenPos>) {}
        fn application_focus(&self, _: a::Diagnostic) {
            panic!("off emitted focus evidence");
        }
    }
    let mut g = Group::new();
    g.enter_hidden();
    let old = g.routers[1].application_focus().unwrap().clone();
    for r in &mut g.routers {
        r.set_diagnostics(Arc::new(RejectEvidence));
        r.set_application_focus_enabled(false, 40);
    }
    g.pump();
    let r = &mut g.routers[1];
    r.input_message(
        boot(1).device_id,
        Message::Control(ControlMsg::ApplicationFocus(a::Message {
            claim: old.claim.clone(),
            action: a::Action::Snapshot(old.clone()),
        })),
        50,
    );
    r.application_input(
        boot(1).device_id,
        boot(1).device_id,
        1,
        InputMsg::ApplicationKey {
            route: a::keys::KeyRoute {
                epoch: old.claim.epoch,
                pointer: boot(1),
                revision: old.revision,
                destination: boot(2),
                press_id: 1,
            },
            code: LETTER,
            physical_code: None,
            pressed: true,
        },
    );
    assert!(r.drain_outputs().is_empty());
}

fn local_mode(g: &mut Group, mouse: bool) {
    let mut settings = g.routers[1].config().settings.clone();
    if mouse {
        settings.mouse_mode = oink_foundation::input::InputMode::Local;
    } else {
        settings.keyboard_mode = oink_foundation::input::InputMode::Local;
    }
    g.routers[1].set_settings(settings);
    for r in &mut g.routers {
        r.set_application_focus_participation(
            boot(2),
            true,
            if mouse {
                (InputMode::Local, InputMode::Shared)
            } else {
                (InputMode::Shared, InputMode::Local)
            },
            15,
        );
    }
    // Dismiss is a native withdrawal, not a new foreground claim.
    g.foreground[1] = Foreground::Windowed;
    g.pump();
    g.native_work.clear();
}
fn claim_device(g: &mut Group, pointer: u8, epoch: u64) {
    for r in &mut g.routers {
        r.epoch = epoch;
        r.claim = boot(pointer).device_id;
        r.holder = boot(pointer).device_id;
        r.application_sync(20);
    }
    g.pump();
}
fn type_on_mac(g: &mut Group) {
    g.injected.clear();
    for pressed in [true, false] {
        assert_eq!(
            g.routers[0].on_local_event(
                &CapturedEvent {
                    time_us: 40,
                    physical: true,
                    source: SourceKind::Keyboard,
                    cursor: None,
                    event: InputEvent::Key { code: LETTER, pressed },
                },
                40
            ),
            Disposition::Pass
        );
        g.pump();
    }
}
fn assert_no_local_mode_park_work(g: &Group) {
    assert!(!g.native_work.iter().any(|(i, command)| *i == 1
        && matches!(
            command,
            ApplicationFocusCommand::Sample(_) | ApplicationFocusCommand::Park(_)
        )));
}
#[test]
fn local_mode_windows_keeps_k_when_pointer_enters_shared_mac() {
    for mouse in [false, true] {
        let mut g = Group::with_pointer(2);
        local_mode(&mut g, mouse);
        assert_eq!(g.routers[1].application_focus().unwrap().keyboard, Some(boot(2)));
        assert_eq!(g.foreground[0], Foreground::Hidden);
        claim_device(&mut g, 1, 2);
        assert_eq!(g.routers[0].application_focus().unwrap().keyboard, Some(boot(2)));
        assert!(
            g.routers[1]
                .application_focus()
                .unwrap()
                .participants
                .contains(&boot(2))
        );
        type_on_mac(&mut g);
        assert_eq!(g.injected, vec![(1, LETTER, true), (1, LETTER, false)]);
        assert_no_local_mode_park_work(&g);
    }
}
#[test]
fn entering_local_mode_windows_claims_k_and_leaving_it_never_parks_it() {
    for mouse in [false, true] {
        let mut g = Group::new();
        local_mode(&mut g, mouse);
        // A delayed pre-mode-change Hidden sample is not permission to keep K.
        g.foreground[1] = Foreground::Hidden;
        g.observe(1, None, false);
        claim_device(&mut g, 2, 2);
        assert_eq!(g.routers[1].application_focus().unwrap().keyboard, Some(boot(2)));
        assert_eq!(g.routers[1].application_focus().unwrap().phase, a::Phase::Active);
        assert_eq!(g.foreground[0], Foreground::Hidden, "the shared previous K still parks");
        g.foreground[1] = Foreground::Windowed;
        claim_device(&mut g, 1, 3);
        // A click on Mac takes K from Windows, but may never park local Windows.
        g.foreground[0] = Foreground::Windowed;
        g.observe(0, None, true);
        g.pump();
        assert_eq!(g.routers[0].application_focus().unwrap().keyboard, Some(boot(1)));
        assert_no_local_mode_park_work(&g);
    }
}
#[test]
fn disabled_keyboard_still_receives_application_keys_and_keeps_shared_parking_policy() {
    let mut g = Group::with_pointer(2);
    g.routers[1].set_keyboard_disabled(true);
    g.routers[1].set_application_focus_participation(boot(2), true, (InputMode::Shared, InputMode::Disabled), 0);
    g.pump();
    claim_device(&mut g, 1, 2);
    type_on_mac(&mut g);
    assert_eq!(g.injected, vec![(1, LETTER, true), (1, LETTER, false)]);
    assert_eq!(
        g.routers[1].on_local_event(
            &CapturedEvent {
                time_us: 50,
                physical: true,
                source: SourceKind::Keyboard,
                cursor: None,
                event: InputEvent::Key {
                    code: LETTER,
                    pressed: true
                },
            },
            50
        ),
        Disposition::Swallow
    );
    g.foreground[0] = Foreground::Windowed;
    g.observe(0, None, true);
    g.pump();
    assert_eq!(g.foreground[1], Foreground::Hidden);
}

#[test]
fn local_mode_ignores_delayed_authority_sample_and_prepare() {
    for mouse in [false, true] {
        let mut g = Group::new();
        local_mode(&mut g, mouse);
        let state = g.routers[1].application_focus().unwrap().clone();
        let request = a::ParkRequest {
            claim: state.claim.clone(),
            revision: state.revision,
            operation: 100,
            target: boot(2),
            expected: g.observations[1].unwrap().stamp,
        };
        for action in [a::Action::Sample, a::Action::Prepare(request)] {
            g.routers[1].application_message(
                boot(1).device_id,
                a::Message {
                    claim: state.claim.clone(),
                    action,
                },
                50,
            );
        }
        g.pump();
        assert_no_local_mode_park_work(&g);
    }
}

#[test]
fn pointer_lock_arriving_before_handoff_gossip_preserves_application_claim() {
    let mut g = Group::new();
    local_mode(&mut g, false);
    // A hands off to local-mode B, then asks B to lock. C can receive B's
    // ordered stream before A's independent stream announces that handoff.
    g.routers[0].hand_off(
        ScreenPos::new(ScreenId::new(boot(2).device_id, "test"), Point::new(10., 10.)),
        None,
    );
    let epoch = g.routers[0].epoch;
    g.routers[0].send(
        boot(2).device_id,
        Message::Control(ControlMsg::SetFocusLock {
            epoch,
            locked: Some(true),
            target: None,
        }),
    );
    let mut delayed = Vec::new();
    for output in g.routers[0].drain_outputs() {
        if let Output::Send { to, msg, .. } = output {
            if to == boot(2).device_id {
                g.routers[1].input_message(boot(1).device_id, msg, 20);
            } else {
                delayed.push((to, msg));
            }
        }
    }
    g.pump();
    assert_eq!(g.routers[2].claim, boot(2).device_id);
    assert_eq!(g.routers[2].epoch, epoch);
    for (to, msg) in delayed {
        g.routers[to.0[0] as usize - 1].input_message(boot(1).device_id, msg, 30);
    }
    g.pump();
    // Existing heartbeat repair must suffice, with no native timeout or new
    // pointer claim. Both streams carry the same routing incarnations.
    for _ in 0..3 {
        g.routers[1].send_focus_state(boot(3).device_id);
        g.routers[1].application_state_sync(true);
        g.pump();
    }
    let authority = g.routers[1].application_focus().unwrap();
    let follower = g.routers[2].application_focus().unwrap();
    assert_eq!(authority.keyboard, Some(boot(2)));
    assert_eq!(follower, authority);
    assert_eq!(authority.phase, a::Phase::Active);
}
