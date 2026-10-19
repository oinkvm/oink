use super::*;
const ME: DeviceId = DeviceId([1; 32]);
const PEER: DeviceId = DeviceId([2; 32]);
/// Geometry used by cases that move the pointer; session membership is independent.
pub(super) fn session_layout(me: DeviceId, peer: DeviceId) -> Arc<dyn LayoutQuery> {
    let mut layout = crate::SimpleLayout::new();
    let bounds = Rect::new(0., 0., 1000., 600.);
    layout.add_screen(ScreenId::new(me, "screen"), bounds, Rect::new(0., 0., 250., 150.));
    layout.add_screen(ScreenId::new(peer, "screen"), bounds, Rect::new(300., 0., 250., 150.));
    Arc::new(layout)
}
fn router() -> Router {
    Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: ME,
                nonce: [ME.0[0]; 16],
            },
            "local",
            Os::Other,
        ),
        session_layout(ME, PEER),
    )
}
fn initialize(r: &mut Router, input: bool) {
    assert!(r.attach_input(
        oink_foundation::session::Boot {
            device_id: PEER,
            nonce: [2; 16]
        },
        [2; 16],
        input,
        false,
        FeelProfile::default(),
        1
    ));
}
fn focus(epoch: u64, holder: DeviceId) -> Message {
    Message::Control(ControlMsg::FocusState {
        lock_state: None,
        epoch,
        holder,
        scope: None,
        session: Some([2; 16]),
    })
}
#[test]
fn duplicate_input_attachment_preserves_capabilities() {
    let mut r = router();
    initialize(&mut r, false);
    r.out.clear();
    assert!(!r.attach_input(
        oink_foundation::session::Boot {
            device_id: PEER,
            nonce: [2; 16]
        },
        [2; 16],
        true,
        false,
        FeelProfile::default(),
        100
    ));
    assert!(!r.peers[&PEER].sharing);
    assert!(r.out.is_empty());
    assert!(!r.input_ready(PEER));
}
#[test]
fn routing_never_infers_input_loss_from_missing_messages() {
    let mut r = router();
    initialize(&mut r, true);
    r.input_message(PEER, focus(0, ME), 2);
    r.tick(100_000_000);
    assert!(r.input_ready(PEER));
    assert!(
        !r.out
            .iter()
            .any(|o| matches!(o, Output::Status(StatusEvent::PeerTimedOut(_))))
    );
}

#[test]
fn display_change_remaps_saved_positions_and_refreshes_remote_park() {
    let mut r = router();
    let display = |key: &str, bounds: Rect, is_primary| DisplayInfo {
        key: key.into(),
        name: key.into(),
        bounds,
        pixel_width: bounds.w as u32,
        pixel_height: bounds.h as u32,
        reference_extent: None,
        rotation_deg: 0,
        is_primary,
        is_builtin: false,
        is_virtual: false,
    };
    r.set_local_displays(vec![
        display("primary", Rect::new(0., 0., 1728., 1117.), true),
        display("removed", Rect::new(1728., 0., 1920., 1080.), false),
    ]);
    let old = ScreenPos::new(ScreenId::new(ME, "removed"), Point::new(3000., 800.));
    r.pointer = Some(old.clone());
    r.exit_point = Some(old);
    r.ref_display = Some("removed".into());
    r.holder = PEER;
    r.out.clear();
    let resized = display("primary", Rect::new(0., 0., 1512., 982.), true);
    r.set_local_displays(vec![resized.clone()]);
    for pos in [&r.pointer, &r.exit_point] {
        let pos = pos.as_ref().unwrap();
        assert_eq!(pos.screen.display, "primary");
        assert!(resized.bounds.contains(pos.local), "{pos:?}");
    }
    assert_eq!(r.ref_display.as_deref(), Some("primary"));
    assert!(
        r.out
            .iter()
            .any(|o| matches!(o, Output::SetMode(CaptureMode::Remote { park }) if *park == resized.bounds.center()))
    );
    r.out.clear();
    let restored = display("primary", Rect::new(0., 0., 1728., 1117.), true);
    r.set_local_displays(vec![restored.clone()]);
    assert!(
        r.out
            .iter()
            .any(|o| matches!(o, Output::SetMode(CaptureMode::Remote { park }) if *park == restored.bounds.center()))
    );
}
#[test]
fn unknown_display_does_not_postpone_input_after_session_init() {
    let mut r = router();
    initialize(&mut r, true);
    r.out.clear();
    r.input_message(
        PEER,
        Message::Control(ControlMsg::FocusEnter {
            epoch: 2,
            display: "new-display".into(),
            pos: Point::new(20., 20.),
            scope: Some(r.new_focus_scope()),
            pointer_source: Some(PEER),
        }),
        2,
    );
    r.input_message(
        PEER,
        Message::Input(InputMsg::Key {
            code: KeyCode::keyboard(4),
            pressed: true,
        }),
        3,
    );
    assert!(
        r.out
            .iter()
            .any(|o| matches!(o, Output::Inject(InjectEvent::Key { pressed: true, .. })))
    );
    r.out.clear();
    r.detach_input(PEER);
    assert!(
        r.out
            .iter()
            .any(|o| matches!(o, Output::Inject(InjectEvent::Key { pressed: false, .. })))
    );
}

#[test]
fn empty_geometry_accepts_focus_claims_and_keeps_the_native_keyboard_as_a_source() {
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: ME,
                nonce: [ME.0[0]; 16],
            },
            "local",
            Os::Windows,
        ),
        Arc::new(crate::SimpleLayout::new()),
    );
    initialize(&mut r, true);
    let scope = FocusScope {
        members: vec![(ME, r.session), (PEER, [2; 16])],
    };
    r.input_message(
        PEER,
        Message::Control(ControlMsg::FocusState {
            lock_state: None,
            epoch: 2,
            holder: PEER,
            scope: Some(scope),
            session: Some([2; 16]),
        }),
        2,
    );
    assert_eq!(r.focus().holder, PEER);
    assert!(r.input_ready(PEER));
    assert!(!r.screen_local_input());
    r.out.clear();
    let code = KeyCode::keyboard(4);
    let event = CapturedEvent {
        physical: true,
        event: InputEvent::Key { code, pressed: true },
        source: SourceKind::Keyboard,
        time_us: 0,
        cursor: None,
    };
    assert_eq!(r.on_local_event(&event, 3), Disposition::Swallow);
    assert!(r.out.iter().any(
        |o| matches!(o, Output::Send { to, msg: Message::Input(InputMsg::Key { pressed: true, .. }), .. } if *to == PEER)
    ));
    r.set_locked(true);
    assert!(r.out.iter().any(
        |o| matches!(o, Output::Send { to, msg: Message::Control(ControlMsg::SetFocusLock { .. }), .. } if *to == PEER)
    ));
    r.return_pointer_home();
    assert_eq!(r.focus().holder, ME);
}

#[test]
fn an_empty_layout_is_not_the_screen_local_input_exception() {
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: ME,
                nonce: [ME.0[0]; 16],
            },
            "local",
            Os::Other,
        ),
        Arc::new(crate::SimpleLayout::new()),
    );
    initialize(&mut r, true);
    r.input_message(PEER, focus(0, ME), 2);
    r.tick(10_000);
    assert_eq!(r.focus().holder, ME);
    assert!(!r.screen_local_input());
    assert!(
        !r.out
            .iter()
            .any(|o| matches!(o, Output::Status(StatusEvent::ScreenLocalInput(true))))
    );
}

#[test]
fn focus_proof_requires_current_local_and_holder_incarnations() {
    let mut r = router();
    initialize(&mut r, true);
    r.focus_joined = true;
    assert!(!r.scope_allows(None, PEER));
    let proof = r.new_focus_scope();
    assert!(r.scope_allows(Some(&proof), PEER));
    r.peers.get_mut(&PEER).unwrap().session = None;
    assert!(!r.scope_allows(Some(&proof), PEER));
    r.peers.get_mut(&PEER).unwrap().session = Some([3; 16]);
    assert!(!r.scope_allows(Some(&proof), PEER));
    r.session = [9; 16];
    assert!(!r.scope_allows(Some(&proof), ME));
}

#[test]
fn unscoped_handoff_cannot_reposition_or_admit_an_existing_participant() {
    let mut r = router();
    initialize(&mut r, true);
    r.reclaim_focus(None);
    let before = r.focus();
    r.out.clear();
    r.input_message(
        PEER,
        Message::Control(ControlMsg::FocusEnter {
            epoch: before.epoch + 1,
            display: "screen".into(),
            pos: Point::new(25., 30.),
            scope: None,
            pointer_source: Some(PEER),
        }),
        2,
    );
    assert_eq!(r.focus().holder, before.holder);
    assert_eq!(r.focus().pointer, before.pointer);
    assert!(!r.group_focus_status().valid);
    assert!(!r.out.iter().any(|out| matches!(out, Output::Inject(_))));
}

#[test]
fn group_focus_status_distinguishes_bootstrap_claim_and_local_island() {
    let mut r = router();
    initialize(&mut r, true);
    let initial = r.group_focus_status();
    assert_eq!(initial.holder, None);
    assert!(!initial.valid && !initial.participating && initial.independent);
    r.focus_scope = Some(r.new_focus_scope());
    r.peers.get_mut(&PEER).unwrap().focus_ready = true;
    r.adopt_claim(1, PEER);
    let joined = r.group_focus_status();
    assert_eq!(joined.holder, Some(PEER));
    assert!(joined.valid && joined.participating && !joined.independent);
    r.detach_input(PEER);
    let island = r.group_focus_status();
    assert_eq!(island.holder, Some(PEER));
    assert!(!island.valid && !island.participating && island.independent);
    assert_eq!(r.focus().holder, ME);
}

#[test]
fn forwarded_input_cannot_cross_source_boot_retirement() {
    use oink_foundation::session::Boot;
    let mut r = router();
    initialize(&mut r, true);
    let old = Boot {
        device_id: DeviceId([3; 32]),
        nonce: [3; 16],
    };
    let new = Boot { nonce: [4; 16], ..old };
    r.admit_boot(old);
    r.input_message(
        PEER,
        Message::Control(ControlMsg::FocusEnter {
            epoch: 1,
            display: "screen".into(),
            pos: Point::new(20., 20.),
            scope: Some(r.new_focus_scope()),
            pointer_source: Some(PEER),
        }),
        1,
    );
    let key = KeyCode::keyboard(4);
    let forwarded = |origin| {
        Message::Input(InputMsg::Forwarded {
            origin,
            hops: 1,
            msg: Box::new(InputMsg::Key {
                code: key,
                pressed: true,
            }),
        })
    };
    r.input_message(PEER, forwarded(old), 2);
    assert!(r.is_held(old.device_id, HeldItem::Key(key)));
    r.admit_boot(new);
    assert!(!r.is_held(old.device_id, HeldItem::Key(key)));
    r.input_message(PEER, forwarded(old), 3);
    assert!(!r.is_held(old.device_id, HeldItem::Key(key)));
    r.input_message(PEER, forwarded(new), 4);
    assert!(r.is_held(new.device_id, HeldItem::Key(key)));
    r.retire_boot(old);
    assert!(
        r.is_held(new.device_id, HeldItem::Key(key)),
        "retiring an old Boot cannot retire its successor"
    );
}

#[test]
fn retiring_forwarded_origin_emits_old_boot_release_and_drops_sync_route() {
    use oink_foundation::session::Boot;
    let mut r = router();
    initialize(&mut r, true);
    let old = Boot {
        device_id: DeviceId([3; 32]),
        nonce: [3; 16],
    };
    r.admit_boot(old);
    let key = HeldItem::Key(KeyCode::keyboard(4));
    r.forwarded.insert((old.device_id, key), PEER);
    r.forwarded_sync.insert((old.device_id, PEER));
    r.out.clear();
    r.admit_boot(Boot { nonce: [4; 16], ..old });
    assert!(!r.forwarded_sync.contains(&(old.device_id, PEER)));
    assert!(r.out.iter().any(|out| matches!(out, Output::Send {
        msg: Message::Input(InputMsg::Forwarded { origin, msg, .. }), ..
    } if *origin == old && matches!(**msg, InputMsg::Key { pressed: false, .. }))));
}

#[test]
fn migration_focus_scope_remains_encodable_after_member_replacement() {
    use oink_foundation::session::Boot;
    use oink_protocol::v1::{
        RawMap, RawValue, dto,
        schema::{FixedBytes, List, WireDto, WireValue},
    };

    let mut r = router();
    // Replace a member repeatedly; there are never more than two admitted Boots.
    // RetirePeer is also how the input owner handles a removed group member.
    for n in 3..=18 {
        let boot = Boot {
            device_id: DeviceId([n; 32]),
            nonce: [n; 16],
        };
        assert!(r.attach_input(boot, [n; 16], true, false, FeelProfile::default(), 0));
        r.retire_boot(boot);
        assert!(!r.peers.contains_key(&boot.device_id));
    }
    initialize(&mut r, true);
    assert_eq!(r.admitted_boots.len(), 2);
    assert_eq!(r.peers.len(), 1);
    r.reclaim_focus(None);
    let opening = r.input_opening(true);
    let Message::Control(ControlMsg::FocusState { scope: Some(scope), .. }) = &opening[2] else {
        panic!("reclaimed focus must have a scope");
    };

    // Use the same DTO projection as the agent's I0 control encoder, then the
    // actual v1 validator. A valid two-member group must keep opening I0 streams.
    let members = scope
        .members
        .iter()
        .map(|(id, nonce)| {
            let device = dto::DeviceId::from_raw(RawMap::from([
                (1, RawValue::Text(dto::ED25519.into())),
                (2, RawValue::Bytes(id.0.to_vec())),
            ]))
            .unwrap();
            dto::FocusMember::from_raw(RawMap::from([(1, device.write()), (2, FixedBytes(*nonce).write())])).unwrap()
        })
        .collect::<Vec<_>>();
    let encoded_scope = dto::FocusScope::from_raw(RawMap::from([(1, List::<_, 16>(members).write())]));
    assert!(
        encoded_scope.is_ok(),
        "two-member input opening contains {} historical scope members: {encoded_scope:?}",
        scope.members.len()
    );
}

#[test]
fn replacing_member_boot_discards_old_sessions_and_stale_retirement_keeps_successor() {
    use oink_foundation::session::Boot;
    let mut r = router();
    initialize(&mut r, true);
    let old = Boot {
        device_id: PEER,
        nonce: [2; 16],
    };
    let replacement = Boot { nonce: [3; 16], ..old };
    r.peers.get_mut(&PEER).unwrap().retired_sessions.insert([9; 16]);
    r.admit_boot(replacement);
    assert!(!r.peers.contains_key(&PEER));
    assert!(r.attach_input(replacement, [3; 16], true, false, FeelProfile::default(), 2));
    r.retire_boot(old);
    let peer = &r.peers[&PEER];
    assert!(peer.connected);
    assert_eq!(peer.session, Some([3; 16]));
    assert!(peer.retired_sessions.is_empty());
    assert_eq!(r.admitted_boots[&PEER], replacement);
    let scope = r.new_focus_scope();
    assert_eq!(scope.members.len(), 2);
    assert!(scope.members.contains(&(PEER, [3; 16])));
    assert!(!scope.members.contains(&(PEER, [2; 16])));
}
