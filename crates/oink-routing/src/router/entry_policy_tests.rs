//! Reordered settings and input records from real, independently synchronised lanes.
use super::*;
use crate::control::FocusLock;
use crate::display_layout::TopologyLayout;
use oink_display::layout::Layout;
const ME: DeviceId = DeviceId([1; 32]);
const PEER: DeviceId = DeviceId([2; 32]);
const PIN: DeviceId = DeviceId([3; 32]);
const JOINER: DeviceId = DeviceId([4; 32]);

fn display() -> DisplayInfo {
    DisplayInfo {
        key: "screen".into(),
        name: "screen".into(),
        bounds: Rect::new(0., 0., 1000., 600.),
        pixel_width: 1000,
        pixel_height: 600,
        reference_extent: Some(oink_foundation::Size::new(250., 150.)),
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }
}
fn layout() -> Layout {
    Layout::auto(&[ME, PEER, PIN].map(|id| (id, vec![display()])))
}
fn scope(r: &Router) -> FocusScope {
    r.new_focus_scope()
}
fn state(r: &mut Router, from: DeviceId, epoch: u64, holder: DeviceId, proof: Option<FocusScope>) {
    let instance = r.peers[&from].session;
    r.input_message(
        from,
        Message::Control(ControlMsg::FocusState {
            lock_state: None,
            epoch,
            holder,
            scope: proof,
            session: instance,
        }),
        10,
    );
}
fn attach(r: &mut Router, peer: DeviceId, controllable: bool) {
    assert!(r.attach_input(
        oink_foundation::session::Boot {
            device_id: peer,
            nonce: [peer.0[0]; 16]
        },
        [peer.0[0]; 16],
        controllable,
        true,
        FeelProfile::default(),
        1
    ));
    state(r, peer, 0, peer, None);
    r.set_peer_displays(peer, vec![display()]);
}
fn router() -> Router {
    router_with_input(true)
}
fn router_with_input(controllable: bool) -> Router {
    let mut r = Router::new(
        RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: ME,
                nonce: [ME.0[0]; 16],
            },
            "source",
            Os::Other,
        ),
        Arc::new(TopologyLayout::from_layout(&layout(), &Default::default())),
    );
    r.set_local_displays(vec![display()]);
    attach(&mut r, PEER, controllable);
    attach(&mut r, PIN, controllable);
    r.drain_outputs();
    r
}
fn enter(r: &mut Router, from: DeviceId, epoch: u64) {
    let proof = scope(r);
    r.input_message(
        from,
        Message::Control(ControlMsg::FocusEnter {
            epoch,
            display: "screen".into(),
            pos: Point::new(30., 40.),
            scope: Some(proof),
            pointer_source: Some(from),
        }),
        20,
    );
}
fn pin(r: &Router, holder: DeviceId, epoch: u64, revision: u64, locked: bool) -> FocusLock {
    FocusLock {
        holder,
        epoch,
        revision,
        author: holder,
        session: if holder == ME {
            r.session
        } else {
            r.peers[&holder].session.unwrap()
        },
        scope: scope(r),
        locked,
    }
}
fn exclude(r: &mut Router, id: DeviceId) {
    let saved = layout();
    let owners = std::collections::BTreeMap::from([(
        id,
        oink_display::layout::OwnerLayoutPreferences {
            screen_shared: false,
            ..Default::default()
        },
    )]);
    r.set_layout(Arc::new(TopologyLayout::from_layout(&saved, &owners)));
}
fn key() -> CapturedEvent {
    CapturedEvent {
        physical: true,
        event: InputEvent::Key {
            code: KeyCode::keyboard(4),
            pressed: true,
        },
        source: SourceKind::Keyboard,
        time_us: 0,
        cursor: None,
    }
}

#[test]
fn excluded_enter_never_restores_local_cursor_or_bumps_the_focus_epoch() {
    for redirect in [false, true] {
        let mut r = router_with_input(redirect);
        if redirect {
            let proof = scope(&r);
            state(&mut r, PEER, 3, PEER, Some(proof));
        }
        exclude(&mut r, ME);
        let before = r.focus();
        r.drain_outputs();
        enter(&mut r, PEER, 4);
        let out = r.drain_outputs();
        assert!(
            !out.iter().any(|o| matches!(
                o,
                Output::RestoreLocal { .. }
                    | Output::SetMode(CaptureMode::Local)
                    | Output::Inject(InjectEvent::MoveTo { .. })
            )),
            "cursor became visible on an excluded screen: {out:?}"
        );
        assert_eq!(r.focus(), before, "rejected entry must preserve the focus and epoch");
    }
}

#[test]
fn excluded_focus_gossip_does_not_adopt_or_evacuate_the_excluded_claim() {
    for redirect in [false, true] {
        let mut r = router_with_input(redirect);
        if redirect {
            let proof = scope(&r);
            state(&mut r, PEER, 3, PEER, Some(proof));
        }
        exclude(&mut r, ME);
        let before = r.focus();
        r.drain_outputs();
        let proof = scope(&r);
        state(&mut r, PEER, 4, ME, Some(proof));
        assert_eq!(
            r.focus(),
            before,
            "stale sender turned an excluded device into a group focus"
        );
    }
}

#[test]
fn excluded_lock_record_and_local_pin_request_never_publish_a_locked_ui_state() {
    for remote in [false, true] {
        let mut r = router_with_input(false);
        exclude(&mut r, ME);
        let before = r.focus();
        r.drain_outputs();
        if remote {
            let record = pin(&r, ME, 4, 1, true);
            r.input_message(PEER, Message::Control(ControlMsg::FocusLock { state: record }), 20);
        } else {
            r.set_locked(true);
        }
        let out = r.drain_outputs();
        assert!(
            !out.iter().any(
                |o| matches!(o,Output::Send {msg:Message::Control(ControlMsg::FocusLock {state}),..} if state.locked)
            ),
            "UI receives an invalid lock then an unlock: {out:?}"
        );
        assert_eq!(r.focus(), before);
        assert!(!r.locked());
    }
}

#[test]
fn excluded_owner_refuses_a_remote_pin_request_without_publishing_a_lock() {
    let mut r = router_with_input(false);
    exclude(&mut r, ME);
    let before = r.focus();
    r.drain_outputs();
    r.input_message(
        PEER,
        Message::Control(ControlMsg::SetFocusLock {
            target: None,
            epoch: before.epoch,
            locked: Some(true),
        }),
        20,
    );
    assert_eq!(r.focus(), before);
    assert!(!r.drain_outputs().iter().any(|o| matches!(o,
        Output::Send {msg:Message::Control(ControlMsg::FocusLock {state}),..} if state.locked)));
}

#[test]
fn remote_resume_cannot_reenter_an_excluded_historical_claim() {
    let mut r = router();
    let proof = scope(&r);
    state(&mut r, PEER, 5, PEER, Some(proof));
    let locked = pin(&r, PIN, 4, 1, true);
    r.input_message(PIN, Message::Control(ControlMsg::FocusLock { state: locked }), 20);
    exclude(&mut r, PEER);
    let unlocked = pin(&r, PIN, 4, 2, false);
    r.input_message(PIN, Message::Control(ControlMsg::FocusLock { state: unlocked }), 21);
    let before = r.focus();
    let proof = scope(&r);
    r.drain_outputs();
    r.input_message(
        PIN,
        Message::Control(ControlMsg::FocusResume {
            epoch: 5,
            holder: PEER,
            scope: proof,
        }),
        22,
    );
    assert_eq!(r.focus(), before, "resume of an excluded claim moved the group pointer");
    assert!(!r.drain_outputs().iter().any(|o| matches!(
        o,
        Output::Send {
            msg: Message::Control(ControlMsg::FocusEnter { .. }),
            ..
        }
    )));
}

#[test]
fn local_resume_does_not_send_the_current_key_to_an_excluded_remembered_claim() {
    let mut r = router();
    let proof = scope(&r);
    state(&mut r, PEER, 5, PEER, Some(proof));
    // A lagging pin owner has not yet seen the conflicting epoch. Its existing
    // pin wins the local holder while the historical group claim stays newer.
    let locked = pin(&r, PIN, 4, 1, true);
    r.input_message(PIN, Message::Control(ControlMsg::FocusLock { state: locked }), 20);
    assert_eq!(r.focus().holder, PIN);
    exclude(&mut r, PEER);
    // Owner unlocks before receiving the conflict report; then a new input
    // member needs admitting to the existing group pointer on first activity.
    let unlocked = pin(&r, PIN, 4, 2, false);
    r.input_message(PIN, Message::Control(ControlMsg::FocusLock { state: unlocked }), 21);
    attach(&mut r, JOINER, true);
    r.drain_outputs();
    r.on_local_event(&key(), 30);
    let out = r.drain_outputs();
    assert!(
        !out.iter()
            .any(|o| matches!(o,Output::Send {to,msg:Message::Input(InputMsg::Key {pressed:true,..}),..} if *to==PEER)),
        "one physical key leaked to the excluded target: {out:?}"
    );
    assert_eq!(r.focus().holder, PIN);
}

#[test]
fn display_sleep_returns_to_the_physical_source_while_its_geometry_is_pending() {
    let mut r = router();
    enter(&mut r, PEER, 3);
    r.drain_outputs();
    let mut pending = layout();
    pending.screens.retain(|p| p.id.device != PEER);
    r.set_layout(Arc::new(TopologyLayout::from_layout(&pending, &Default::default())));
    r.drain_outputs();
    r.on_displays_asleep();
    assert_eq!(
        r.focus().holder,
        PEER,
        "the pointer stayed on a sleeping display instead of returning to its source"
    );
    assert!(r.drain_outputs().iter().any(|o| matches!(o,Output::Send {to,msg:Message::Control(ControlMsg::FocusEnter {pos,..}),..} if *to==PEER && *pos==display().bounds.center())));
}

#[test]
fn pending_geometry_does_not_discard_a_drag_accompanying_an_accepted_focus_enter() {
    use crate::dnd::{DND_SEQ_BIT, DndMsg};
    use oink_foundation::content::ClipId;
    let mut r = router();
    r.set_dnd_available(true);
    let mut pending = layout();
    pending.screens.retain(|p| p.id.device != ME);
    r.set_layout(Arc::new(TopologyLayout::from_layout(&pending, &Default::default())));
    r.drain_outputs();
    let id = ClipId {
        origin: PEER,
        seq: DND_SEQ_BIT | 1,
    };
    r.input_message(
        PEER,
        Message::Dnd(DndMsg::Notice(crate::dnd::DragNotice {
            key: crate::dnd::DragKey {
                offer: oink_foundation::content::OfferId {
                    clip: id,
                    generation: id.seq,
                },
                native_generation: id.seq,
            },
            revision: 1,
            input: PEER,
            state: crate::dnd::DragState::Offered {
                holder: oink_foundation::content::Holder { device: ME, epoch: 3 },
                mode: crate::dnd::SourceMode::Held,
            },
            metadata: crate::dnd::MetadataVersion {
                revision: 1,
                readiness: crate::dnd::MetadataReadiness::Ready,
            },
            preview: crate::dnd::PreviewAvailability::Absent,
            retained_inventory: None,
            items: vec![oink_foundation::clipboard::ClipData::Text("drag".into())],
        })),
        20,
    );
    r.input_message(PEER, Message::Dnd(DndMsg::Enter { id, epoch: 3 }), 20);
    enter(&mut r, PEER, 3);
    r.input_message(
        PEER,
        Message::Dnd(DndMsg::Preview {
            binding: crate::dnd::PreviewBinding {
                key: crate::dnd::DragKey {
                    offer: oink_foundation::content::OfferId {
                        clip: id,
                        generation: id.seq,
                    },
                    native_generation: id.seq,
                },
                metadata_revision: 1,
                preview_revision: 0,
            },
            id,
            epoch: 3,
            image: None,
        }),
        21,
    );
    assert!(
        r.drain_outputs()
            .iter()
            .any(|o| matches!(o,Output::Dnd(DndCmd::Begin {id:found,..}) if *found==id)),
        "cursor entered but the dragged content was silently lost"
    );
}

#[test]
fn known_display_facts_before_layout_sync_do_not_clear_the_owners_focus_lock() {
    let mut r = router();
    let mut pending = layout();
    pending.screens.retain(|p| p.id.device != PEER);
    r.set_layout(Arc::new(TopologyLayout::from_layout(&pending, &Default::default())));
    let proof = scope(&r);
    state(&mut r, PEER, 3, PEER, Some(proof));
    r.drain_outputs();
    let record = pin(&r, PEER, 3, 1, true);
    r.input_message(PEER, Message::Control(ControlMsg::FocusLock { state: record }), 20);
    assert_eq!(
        r.locked_device(),
        Some(PEER),
        "known facts with pending geometry erased the remote lock from the UI"
    );
    assert!(
        !r.drain_outputs().iter().any(
            |o| matches!(o,Output::Send {msg:Message::Control(ControlMsg::FocusLock {state}),..} if !state.locked)
        )
    );
}
