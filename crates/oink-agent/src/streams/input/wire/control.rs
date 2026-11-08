//! Explicit I0 control conversion. Unknown actions are refused at this boundary.
use oink_foundation::{DeviceId, Point, input::FeelProfile};
use oink_protocol::v1::{
    self, Message, RawMap, RawValue, dto as w,
    schema::{FixedBytes, List, Text, WireDto, WireValue},
};
use oink_routing::control::{ControlMsg, FocusLock, FocusScope, GameMode, TrailContinuation, TrailPoint};

fn map<const N: usize>(fields: [(u32, RawValue); N]) -> RawMap {
    fields.into()
}
use oink_network::wire_identity::{boot, device, read_boot};
fn read_device(value: w::DeviceId) -> v1::Result<DeviceId> {
    oink_network::wire_identity::read_device(&value)
}
fn point(value: Point) -> v1::Result<w::Point> {
    w::Point::from_raw(map([(1, value.x.write()), (2, value.y.write())]))
}
fn read_point(value: w::Point) -> v1::Result<Point> {
    Ok(Point::new(value.x()?, value.y()?))
}
fn mode(value: GameMode) -> w::GameMode {
    match value {
        GameMode::Auto => w::GameMode::Auto,
        GameMode::On => w::GameMode::On,
        GameMode::Off => w::GameMode::Off,
    }
}
fn read_mode(value: w::GameMode) -> v1::Result<GameMode> {
    match value {
        w::GameMode::Auto => Ok(GameMode::Auto),
        w::GameMode::On => Ok(GameMode::On),
        w::GameMode::Off => Ok(GameMode::Off),
        _ => Err(v1::ErrorCode::UnsupportedValue.into()),
    }
}
pub(super) fn scope(value: Option<&FocusScope>) -> v1::Result<w::FocusScope> {
    let members = value
        .map_or(&[][..], |scope| scope.members.as_slice())
        .iter()
        .map(|(id, nonce)| w::FocusMember::from_raw(map([(1, device(*id).write()), (2, FixedBytes(*nonce).write())])))
        .collect::<v1::Result<Vec<_>>>()?;
    w::FocusScope::from_raw(map([(1, List::<_, 16>(members).write())]))
}
pub(super) fn read_scope(value: w::FocusScope) -> v1::Result<FocusScope> {
    let mut members = Vec::new();
    for member in value.members()?.0 {
        let id = read_device(member.device()?)?;
        if members.iter().any(|(prior, _)| *prior == id) {
            return Err(v1::ErrorCode::MalformedFrame.into());
        }
        members.push((id, member.instance()?.0));
    }
    Ok(FocusScope { members })
}
fn lock(value: &FocusLock) -> v1::Result<w::FocusLockRecord> {
    w::FocusLockRecord::from_raw(map([
        (1, value.revision.write()),
        (2, device(value.author).write()),
        (3, value.epoch.write()),
        (4, device(value.holder).write()),
        (5, FixedBytes(value.session).write()),
        (6, scope(Some(&value.scope))?.write()),
        (7, value.locked.write()),
    ]))
}
fn read_lock(value: w::FocusLockRecord) -> v1::Result<FocusLock> {
    Ok(FocusLock {
        revision: value.revision()?,
        author: read_device(value.author()?)?,
        epoch: value.epoch()?,
        holder: read_device(value.holder()?)?,
        session: value.session()?.0,
        scope: read_scope(value.scope()?)?,
        locked: value.locked()?,
    })
}
fn feel(value: &FeelProfile) -> v1::Result<w::FeelProfile> {
    w::FeelProfile::from_raw(map([
        (1, value.key_repeat_delay_ms.write()),
        (2, value.key_repeat_interval_ms.write()),
        (3, value.wheel_lines_per_notch.write()),
        (4, value.natural_scroll_mouse.write()),
        (5, value.natural_scroll_touchpad.write()),
    ]))
}
fn read_feel(value: w::FeelProfile) -> v1::Result<FeelProfile> {
    Ok(FeelProfile {
        key_repeat_delay_ms: value.key_repeat_delay_ms()?,
        key_repeat_interval_ms: value.key_repeat_interval_ms()?,
        wheel_lines_per_notch: value.wheel_lines_per_notch()?,
        natural_scroll_mouse: value.natural_scroll_mouse()?,
        natural_scroll_touchpad: value.natural_scroll_touchpad()?,
    })
}
fn trail(value: &TrailContinuation) -> v1::Result<w::TrailContinuation> {
    let points = value
        .points
        .iter()
        .map(|p| w::TrailPoint::from_raw(map([(1, p.age_secs.write()), (2, point(p.pos)?.write())])))
        .collect::<v1::Result<Vec<_>>>()?;
    w::TrailContinuation::from_raw(map([
        (1, value.shown_secs.write()),
        (2, value.size_scale.write()),
        (3, value.native_units_per_reference.write()),
        (4, value.emit_carry.write()),
        (5, point(value.heading)?.write()),
        (6, List::<_, 64>(points).write()),
    ]))
}
fn read_trail(value: w::TrailContinuation) -> v1::Result<TrailContinuation> {
    Ok(TrailContinuation {
        shown_secs: value.shown_secs()?,
        size_scale: value.size_scale()?,
        native_units_per_reference: value.native_units_per_reference()?,
        emit_carry: value.emit_carry()?,
        heading: read_point(value.heading()?)?,
        points: value
            .points()?
            .0
            .into_iter()
            .map(|p| {
                Ok(TrailPoint {
                    age_secs: p.age_secs()?,
                    pos: read_point(p.pos()?)?,
                })
            })
            .collect::<v1::Result<_>>()?,
    })
}

pub(super) fn encode(value: &ControlMsg) -> v1::Result<Message> {
    Ok(match value {
        ControlMsg::UnlockQuery { epoch } => {
            Message::ControlUnlockQuery(w::ControlUnlockQuery::from_raw(map([(1, epoch.write())]))?)
        }
        ControlMsg::UnlockReply { epoch, unlocked } => {
            Message::ControlUnlockReply(w::ControlUnlockReply::from_raw(map([
                (1, epoch.write()),
                (2, unlocked.write()),
            ]))?)
        }
        ControlMsg::LockRemoteWindows(request) => {
            Message::ControlLockRemoteWindows(w::ControlLockRemoteWindows::from_raw(map([
                (1, boot(request.source).write()),
                (2, boot(request.target).write()),
                (3, request.epoch.write()),
                (4, request.id.write()),
            ]))?)
        }
        ControlMsg::LocalClick {
            epoch,
            holder,
            scope: members,
            phase,
        } => Message::ControlLocalClick(w::ControlLocalClick::from_raw(map([
            (1, epoch.write()),
            (2, device(*holder).write()),
            (3, scope(Some(members))?.write()),
            (4, phase.write()),
        ]))?),
        ControlMsg::MovePointer { epoch, target } => {
            Message::ControlMovePointer(w::ControlMovePointer::from_raw(map([
                (1, epoch.write()),
                (2, device(*target).write()),
            ]))?)
        }
        ControlMsg::Feel { profile } => {
            Message::ControlFeel(w::ControlFeel::from_raw(map([(1, feel(profile)?.write())]))?)
        }
        ControlMsg::MovePointerEnter {
            epoch,
            display,
            pos,
            scope: members,
            pointer_source,
        }
        | ControlMsg::FocusEnter {
            epoch,
            display,
            pos,
            scope: members,
            pointer_source,
        } => {
            let members = members.as_ref().ok_or(v1::ErrorCode::StateConflict)?;
            let enter = w::ControlFocusEnter::from_raw(map([
                (1, epoch.write()),
                (2, Text::<512>(display.clone()).write()),
                (3, point(*pos)?.write()),
                (4, scope(Some(members))?.write()),
                (5, pointer_source.map(device).write()),
            ]))?;
            if matches!(value, ControlMsg::MovePointerEnter { .. }) {
                Message::ControlMovePointerEnter(enter)
            } else {
                Message::ControlFocusEnter(enter)
            }
        }
        ControlMsg::FocusState {
            epoch,
            holder,
            scope: members,
            session,
            lock_state,
        } => Message::ControlFocusState(w::ControlFocusState::from_raw(map([
            (1, epoch.write()),
            (2, device(*holder).write()),
            (3, scope(members.as_ref())?.write()),
            (4, FixedBytes(session.ok_or(v1::ErrorCode::StateConflict)?).write()),
            (5, lock_state.as_ref().map(lock).transpose()?.write()),
        ]))?),
        ControlMsg::SetFocusLock { epoch, locked, target } => {
            Message::ControlSetFocusLock(w::ControlSetFocusLock::from_raw(map([
                (1, epoch.write()),
                (2, locked.write()),
                (3, target.map(device).write()),
            ]))?)
        }
        ControlMsg::EntryProtection { state } => {
            Message::ControlEntryProtection(w::ControlEntryProtection::from_raw(map([
                (1, state.locked.write()),
                (2, state.game.write()),
                (
                    3,
                    List::<_, 64>(state.passive_displays.iter().cloned().map(Text::<512>).collect()).write(),
                ),
            ]))?)
        }
        ControlMsg::LockEscape { held } => {
            Message::ControlLockEscape(w::ControlLockEscape::from_raw(map([(1, held.write())]))?)
        }
        ControlMsg::FocusLock { state } => {
            Message::ControlFocusLock(w::ControlFocusLock::from_raw(map([(1, lock(state)?.write())]))?)
        }
        ControlMsg::GameMode {
            epoch,
            generation,
            mode: value,
            detected,
            active,
            cursor_hiding_cause_available,
        } => Message::ControlGameMode(w::ControlGameMode::from_raw(map([
            (1, epoch.write()),
            (2, generation.write()),
            (3, mode(*value).write()),
            (4, detected.write()),
            (5, active.write()),
            (6, cursor_hiding_cause_available.write()),
        ]))?),
        ControlMsg::SetGameMode { epoch, mode: value } => {
            Message::ControlSetGameMode(w::ControlSetGameMode::from_raw(map([
                (1, epoch.write()),
                (2, mode(*value).write()),
            ]))?)
        }
        ControlMsg::FocusResume {
            epoch,
            holder,
            scope: members,
        } => Message::ControlFocusResume(w::ControlFocusResume::from_raw(map([
            (1, epoch.write()),
            (2, device(*holder).write()),
            (3, scope(Some(members))?.write()),
        ]))?),
        ControlMsg::CursorTrail { active } => {
            Message::ControlCursorTrail(w::ControlCursorTrail::from_raw(map([(1, active.write())]))?)
        }
        ControlMsg::UserActivity => Message::ControlUserActivity(w::ControlUserActivity::from_raw(RawMap::new())?),
        ControlMsg::CursorTrailHandoff { epoch, sources, state } => {
            Message::ControlCursorTrailHandoff(w::ControlCursorTrailHandoff::from_raw(map([
                (1, epoch.write()),
                (2, List::<_, 16>(sources.iter().copied().map(device).collect()).write()),
                (3, trail(state)?.write()),
            ]))?)
        }
        ControlMsg::InputCapability {
            controllable,
            touchpad_contacts,
        } => Message::InputCapability(w::InputCapability::from_raw(map([
            (1, controllable.write()),
            (2, touchpad_contacts.unwrap_or(0).write()),
        ]))?),
    })
}
pub(super) fn decode(value: Message) -> v1::Result<ControlMsg> {
    Ok(match value {
        Message::ControlUnlockQuery(v) => ControlMsg::UnlockQuery { epoch: v.epoch()? },
        Message::ControlUnlockReply(v) => ControlMsg::UnlockReply {
            epoch: v.epoch()?,
            unlocked: v.unlocked()?,
        },
        Message::ControlLockRemoteWindows(v) => {
            ControlMsg::LockRemoteWindows(oink_routing::control::WindowsLockRequest {
                source: read_boot(&v.source()?)?,
                target: read_boot(&v.target()?)?,
                epoch: v.epoch()?,
                id: v.request_id()?,
            })
        }
        Message::ControlLocalClick(v) => ControlMsg::LocalClick {
            epoch: v.epoch()?,
            holder: read_device(v.holder()?)?,
            scope: read_scope(v.scope()?)?,
            phase: v.phase()?,
        },
        Message::ControlMovePointer(v) => ControlMsg::MovePointer {
            epoch: v.epoch()?,
            target: read_device(v.target()?)?,
        },
        Message::ControlFeel(v) => ControlMsg::Feel {
            profile: read_feel(v.profile()?)?,
        },
        Message::ControlFocusEnter(v) => ControlMsg::FocusEnter {
            epoch: v.epoch()?,
            display: v.display()?.0,
            pos: read_point(v.pos()?)?,
            scope: Some(read_scope(v.scope()?)?),
            pointer_source: v.pointer_source()?.map(read_device).transpose()?,
        },
        Message::ControlMovePointerEnter(v) => ControlMsg::MovePointerEnter {
            epoch: v.epoch()?,
            display: v.display()?.0,
            pos: read_point(v.pos()?)?,
            scope: Some(read_scope(v.scope()?)?),
            pointer_source: v.pointer_source()?.map(read_device).transpose()?,
        },
        Message::ControlFocusState(v) => {
            let scope = read_scope(v.scope()?)?;
            ControlMsg::FocusState {
                epoch: v.epoch()?,
                holder: read_device(v.holder()?)?,
                scope: (!scope.members.is_empty()).then_some(scope),
                session: Some(v.session()?.0),
                lock_state: v.lock_state()?.map(read_lock).transpose()?,
            }
        }
        Message::ControlSetFocusLock(v) => ControlMsg::SetFocusLock {
            epoch: v.epoch()?,
            locked: v.locked()?,
            target: v.target()?.map(read_device).transpose()?,
        },
        Message::ControlEntryProtection(v) => ControlMsg::EntryProtection {
            state: oink_routing::control::EntryProtection {
                locked: v.locked()?,
                game: v.game()?,
                passive_displays: v.passive_displays()?.0.into_iter().map(|v| v.0).collect(),
            },
        },
        Message::ControlLockEscape(v) => ControlMsg::LockEscape { held: v.held()? },
        Message::ControlFocusLock(v) => ControlMsg::FocusLock {
            state: read_lock(v.state()?)?,
        },
        Message::ControlGameMode(v) => ControlMsg::GameMode {
            epoch: v.epoch()?,
            generation: v.generation()?,
            mode: read_mode(v.mode()?)?,
            detected: v.detected()?,
            active: v.active()?,
            cursor_hiding_cause_available: v.cursor_hiding_cause_available()?,
        },
        Message::ControlSetGameMode(v) => ControlMsg::SetGameMode {
            epoch: v.epoch()?,
            mode: read_mode(v.mode()?)?,
        },
        Message::ControlFocusResume(v) => ControlMsg::FocusResume {
            epoch: v.epoch()?,
            holder: read_device(v.holder()?)?,
            scope: read_scope(v.scope()?)?,
        },
        Message::ControlCursorTrail(v) => ControlMsg::CursorTrail { active: v.active()? },
        Message::ControlUserActivity(_) => ControlMsg::UserActivity,
        Message::ControlCursorTrailHandoff(v) => ControlMsg::CursorTrailHandoff {
            epoch: v.epoch()?,
            sources: v.sources()?.0.into_iter().map(read_device).collect::<v1::Result<_>>()?,
            state: read_trail(v.state()?)?,
        },
        Message::InputCapability(v) => ControlMsg::InputCapability {
            controllable: v.controllable()?,
            touchpad_contacts: Some(v.max_contacts()?).filter(|count| *count > 0),
        },
        _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absent_focus_claim_stays_absent_and_session_is_never_fabricated() {
        let state = ControlMsg::FocusState {
            epoch: 0,
            holder: DeviceId([1; 32]),
            scope: None,
            session: Some([2; 16]),
            lock_state: None,
        };
        assert_eq!(decode(encode(&state).unwrap()).unwrap(), state);
        let ControlMsg::FocusState {
            epoch,
            holder,
            scope,
            lock_state,
            ..
        } = state
        else {
            unreachable!()
        };
        assert!(
            encode(&ControlMsg::FocusState {
                epoch,
                holder,
                scope,
                session: None,
                lock_state
            })
            .is_err()
        );
        assert!(
            encode(&ControlMsg::FocusEnter {
                epoch: 1,
                display: "screen".into(),
                pos: Point::new(0.5, 0.5),
                scope: None,
                pointer_source: None
            })
            .is_err()
        );
    }
    #[test]
    fn handoff_preserves_original_process_scope_and_reference_geometry() {
        let scope = FocusScope {
            members: vec![(DeviceId([1; 32]), [3; 16]), (DeviceId([2; 32]), [4; 16])],
        };
        let enter = ControlMsg::FocusEnter {
            epoch: 19,
            display: "screen".into(),
            pos: Point::new(0.25, 0.125),
            scope: Some(scope),
            pointer_source: Some(DeviceId([2; 32])),
        };
        assert_eq!(decode(encode(&enter).unwrap()).unwrap(), enter);
    }
    #[test]
    fn pointer_shortcut_entry_roundtrips_without_becoming_an_edge_entry() {
        let entry = ControlMsg::MovePointerEnter {
            epoch: 19,
            display: "hidden".into(),
            pos: Point::new(-200., 350.),
            scope: Some(FocusScope {
                members: vec![(DeviceId([1; 32]), [3; 16]), (DeviceId([2; 32]), [4; 16])],
            }),
            pointer_source: Some(DeviceId([1; 32])),
        };
        let encoded = encode(&entry).unwrap();
        assert_eq!(encoded.type_id(), 0x1301);
        assert_eq!(decode(encoded).unwrap(), entry);
    }
    #[test]
    fn local_click_phases_roundtrip() {
        for phase in 0..=5 {
            let value = ControlMsg::LocalClick {
                epoch: 42,
                holder: DeviceId([1; 32]),
                scope: FocusScope {
                    members: vec![(DeviceId([1; 32]), [2; 16])],
                },
                phase,
            };
            let wire = encode(&value).unwrap();
            assert_eq!(wire.type_id(), 0x1302);
            assert_eq!(decode(wire).unwrap(), value);
        }
    }
}
