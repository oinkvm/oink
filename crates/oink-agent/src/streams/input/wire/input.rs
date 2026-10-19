//! Borrowed I0/I1 input conversion. Native motion/contact encoding uses caller storage.
use oink_foundation::{DeviceId, contacts as c, input as n, session::Boot};
use oink_protocol::v1::{
    self, dto as w,
    frame::OpaqueFrame,
    hot::{self, Value as V, View},
    schema::{WireDto, WireValue},
};
use oink_routing::{
    Packet,
    input::{InputMsg, ScrollAmount, VolumeKey, VolumeKeyOrigin},
};

type Fields<'a> = &'a [(u32, V<'a>)];
fn frame(
    name: &str,
    body: Fields<'_>,
    origin: Option<Boot>,
    hops: u8,
    time: Option<n::CaptureTime>,
    out: &mut [u8],
) -> v1::Result<usize> {
    if origin.is_some() != (hops > 0) {
        return Err(v1::ErrorCode::MalformedFrame.into());
    }
    let id = v1::registry::MESSAGES
        .iter()
        .find(|entry| entry.name == name)
        .ok_or(v1::ErrorCode::UnknownMessage)?
        .id;
    let nonce = origin.map(|boot| boot.nonce).unwrap_or_default();
    let device = origin.map(|boot| boot.device_id.0).unwrap_or_default();
    let identity = [(1, V::Text(w::ED25519)), (2, V::Bytes(&device))];
    let boot = [(1, V::Map(&identity)), (2, V::Bytes(&nonce))];
    let mut fields = [(0, V::Null); 10];
    fields[..body.len()].copy_from_slice(body);
    let mut count = body.len();
    if origin.is_some() {
        fields[count] = (100, V::Map(&boot));
        fields[count + 1] = (101, V::Uint(u64::from(hops)));
        count += 2;
    }
    if let Some(time) = time {
        fields[count] = (102, V::Uint(u64::from(time.0)));
        count += 1;
    }
    hot::encode_into(id, &fields[..count], out, out.len())
}
macro_rules! enum_values {
    ($to:ident,$from:ident,$native:path,$wire:path,{$($name:ident=>$value:literal),+})=>{
        fn $to(value:$native)->u64 {use $native as N; match value {$(N::$name=>$value),+}}
        fn $from(value:$wire)->v1::Result<$native> {use $native as N; use $wire as W; match value {$(W::$name=>Ok(N::$name)),+, _=>Err(v1::ErrorCode::UnsupportedValue.into())}}
    }
}
fn source(value: n::SourceKind) -> u64 {
    match value {
        n::SourceKind::Mouse => 0,
        n::SourceKind::Touchpad => 1,
        n::SourceKind::Keyboard => 2,
        n::SourceKind::Unknown => 3,
    }
}
fn read_source(value: w::SourceKind) -> v1::Result<n::SourceKind> {
    Ok(match value {
        w::SourceKind::Mouse => n::SourceKind::Mouse,
        w::SourceKind::Touchpad => n::SourceKind::Touchpad,
        w::SourceKind::Keyboard => n::SourceKind::Keyboard,
        w::SourceKind::Indeterminate => n::SourceKind::Unknown,
        _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
    })
}
enum_values!(phase,read_phase,n::ScrollPhase,w::ScrollPhase,{None=>0,Began=>1,Changed=>2,Ended=>3,Cancelled=>4,MomentumBegan=>5,Momentum=>6,MomentumEnded=>7});
enum_values!(gesture_phase,read_gesture_phase,n::GesturePhase,w::GesturePhase,{Began=>0,Changed=>1,Ended=>2,Cancelled=>3});
enum_values!(axis,read_axis,n::SwipeAxis,w::SwipeAxis,{Horizontal=>0,Vertical=>1,Pinch=>2});
enum_values!(contact_phase,read_contact_phase,c::Phase,w::ContactPhase,{Begin=>0,Move=>1,End=>2,Cancel=>3});
enum_values!(amount,read_amount,ScrollAmount,w::ScrollAmount,{Reference=>0,Lines=>1});
enum_values!(volume_action,read_volume_action,VolumeKey,w::VolumeKey,{Up=>0,Down=>1,Mute=>2});
fn button_kind(button: n::MouseButton) -> u64 {
    match button {
        n::MouseButton::Left => 0,
        n::MouseButton::Right => 1,
        n::MouseButton::Middle => 2,
        n::MouseButton::Back => 3,
        n::MouseButton::Forward => 4,
        n::MouseButton::Other(_) => 5,
    }
}
fn other_button(button: n::MouseButton) -> u64 {
    if let n::MouseButton::Other(number) = button {
        u64::from(number)
    } else {
        0
    }
}
fn read_button(button: View<'_, w::MouseButton>) -> v1::Result<n::MouseButton> {
    Ok(match button.kind()? {
        w::MouseButtonKind::Left => n::MouseButton::Left,
        w::MouseButtonKind::Right => n::MouseButton::Right,
        w::MouseButtonKind::Middle => n::MouseButton::Middle,
        w::MouseButtonKind::Back => n::MouseButton::Back,
        w::MouseButtonKind::Forward => n::MouseButton::Forward,
        w::MouseButtonKind::Other => {
            n::MouseButton::Other(View::<w::OtherMouseButton>::from_map(button.body()?)?.number()?)
        }
        _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
    })
}
fn read_gesture(gesture: View<'_, w::Gesture>) -> v1::Result<n::Gesture> {
    Ok(match gesture.kind()? {
        w::GestureKind::DockSwipe => {
            let v = View::<w::DockSwipe>::from_map(gesture.body()?)?;
            n::Gesture::DockSwipe {
                axis: read_axis(v.axis()?)?,
                phase: read_gesture_phase(v.phase()?)?,
                progress: v.progress()?,
                velocity: v.velocity()?,
            }
        }
        w::GestureKind::Magnify => {
            let v = View::<w::Magnify>::from_map(gesture.body()?)?;
            n::Gesture::Magnify {
                phase: read_gesture_phase(v.phase()?)?,
                amount: v.amount()?,
            }
        }
        w::GestureKind::Rotate => {
            let v = View::<w::Rotate>::from_map(gesture.body()?)?;
            n::Gesture::Rotate {
                phase: read_gesture_phase(v.phase()?)?,
                degrees: v.degrees()?,
            }
        }
        w::GestureKind::SmartMagnify => n::Gesture::SmartMagnify,
        _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
    })
}
fn read_boot(boot: View<'_, w::Boot>) -> v1::Result<Boot> {
    let device = boot.device_id()?;
    if device.algorithm()? != w::ED25519 {
        return Err(v1::ErrorCode::UnsupportedValue.into());
    }
    Ok(Boot {
        device_id: DeviceId(
            device
                .public_key()?
                .try_into()
                .map_err(|_| v1::ErrorCode::MalformedFrame)?,
        ),
        nonce: *boot.nonce()?,
    })
}

pub(super) fn encode(input: &InputMsg, time: Option<n::CaptureTime>, out: &mut [u8]) -> v1::Result<usize> {
    let (input, origin, hops) = match input {
        InputMsg::Forwarded { origin, hops, msg } if !matches!(**msg, InputMsg::Forwarded { .. }) => {
            (msg.as_ref(), Some(*origin), *hops)
        }
        input => (input, None, 0),
    };
    let emit = |name, body: Fields<'_>, out: &mut [u8]| frame(name, body, origin, hops, time, out);
    match input {
        InputMsg::Motion { seq, dx, dy } => emit(
            "InputMotion",
            &[(1, V::Uint(*seq)), (2, V::F64(*dx)), (3, V::F64(*dy))],
            out,
        ),
        InputMsg::RelativeMotion {
            epoch,
            generation,
            seq,
            x,
            y,
        } => emit(
            "InputRelativeMotion",
            &[
                (1, V::Uint(*epoch)),
                (2, V::Uint(*generation)),
                (3, V::Uint(*seq)),
                (4, V::F64(*x)),
                (5, V::F64(*y)),
            ],
            out,
        ),
        InputMsg::Key { code, pressed } => emit(
            "InputKey",
            &[(1, V::Uint(u64::from(code.0))), (2, V::Bool(*pressed))],
            out,
        ),
        InputMsg::CapsKey { code, pressed } => emit(
            "InputKey",
            &[
                (1, V::Uint(u64::from(code.0))),
                (2, V::Bool(*pressed)),
                (3, V::Uint(u64::from(n::KeyCode::CAPS_LOCK.0))),
            ],
            out,
        ),
        InputMsg::KeyRepeat { code, n } => emit(
            "InputKeyRepeat",
            &[(1, V::Uint(u64::from(code.0))), (2, V::Uint(u64::from(*n)))],
            out,
        ),
        InputMsg::Button { button, pressed } => {
            let other = [(1, V::Uint(other_button(*button)))];
            let envelope = [
                (1, V::Uint(button_kind(*button))),
                (
                    2,
                    V::Map(if matches!(button, n::MouseButton::Other(_)) {
                        &other
                    } else {
                        &[]
                    }),
                ),
            ];
            emit("InputButton", &[(1, V::Map(&envelope)), (2, V::Bool(*pressed))], out)
        }
        InputMsg::Scroll {
            source: kind,
            dx,
            dy,
            unit,
            phase: scroll_phase,
        } => emit(
            "InputScroll",
            &[
                (1, V::Uint(source(*kind))),
                (2, V::F64(*dx)),
                (3, V::F64(*dy)),
                (4, V::Uint(amount(*unit))),
                (5, V::Uint(phase(*scroll_phase))),
            ],
            out,
        ),
        InputMsg::HeldSync { keys, buttons } => {
            if keys.len() > 256 || buttons.len() > 32 {
                return Err(v1::ErrorCode::LimitExceeded.into());
            }
            let mut key_values = [V::Null; 256];
            for (output, key) in key_values.iter_mut().zip(keys) {
                *output = V::Uint(u64::from(key.0));
            }
            let others: [[(u32, V<'_>); 1]; 32] = std::array::from_fn(|index| {
                [(1, V::Uint(buttons.get(index).map_or(0, |button| other_button(*button))))]
            });
            let envelopes: [[(u32, V<'_>); 2]; 32] = std::array::from_fn(|index| {
                let button = buttons.get(index).copied().unwrap_or(n::MouseButton::Left);
                [
                    (1, V::Uint(button_kind(button))),
                    (
                        2,
                        V::Map(if matches!(button, n::MouseButton::Other(_)) {
                            &others[index]
                        } else {
                            &[]
                        }),
                    ),
                ]
            });
            let values: [V<'_>; 32] = std::array::from_fn(|index| V::Map(&envelopes[index]));
            emit(
                "InputHeldSync",
                &[
                    (1, V::Array(&key_values[..keys.len()])),
                    (2, V::Array(&values[..buttons.len()])),
                ],
                out,
            )
        }
        InputMsg::Gesture { source: kind, gesture } => {
            let mut body = [(0, V::Null); 4];
            let (tag, len) = match gesture {
                n::Gesture::DockSwipe {
                    axis: value,
                    phase,
                    progress,
                    velocity,
                } => {
                    body = [
                        (1, V::Uint(axis(*value))),
                        (2, V::Uint(gesture_phase(*phase))),
                        (3, V::F64(*progress)),
                        (4, V::F64(*velocity)),
                    ];
                    (0, 4)
                }
                n::Gesture::Magnify { phase, amount } => {
                    body[0] = (1, V::Uint(gesture_phase(*phase)));
                    body[1] = (2, V::F64(*amount));
                    (1, 2)
                }
                n::Gesture::Rotate { phase, degrees } => {
                    body[0] = (1, V::Uint(gesture_phase(*phase)));
                    body[1] = (2, V::F64(*degrees));
                    (2, 2)
                }
                n::Gesture::SmartMagnify => (3, 0),
            };
            let envelope = [(1, V::Uint(tag)), (2, V::Map(&body[..len]))];
            emit(
                "InputGesture",
                &[(1, V::Uint(source(*kind))), (2, V::Map(&envelope))],
                out,
            )
        }
        InputMsg::TouchpadContacts(frame) => {
            if usize::from(frame.len) > c::MAX_CHANGES
                || frame
                    .changes()
                    .iter()
                    .any(|change| change.contact.x < 0 || change.contact.y < 0)
            {
                return Err(v1::ErrorCode::UnsupportedValue.into());
            }
            let points: [[(u32, V<'_>); 3]; c::MAX_CHANGES] = std::array::from_fn(|i| {
                let p = frame.changes[i].contact;
                [
                    (1, V::Uint(u64::from(p.id))),
                    (2, V::Uint(p.x as u64)),
                    (3, V::Uint(p.y as u64)),
                ]
            });
            let changes: [[(u32, V<'_>); 2]; c::MAX_CHANGES] = std::array::from_fn(|i| {
                [
                    (1, V::Map(&points[i])),
                    (2, V::Uint(contact_phase(frame.changes[i].phase))),
                ]
            });
            let values: [V<'_>; c::MAX_CHANGES] = std::array::from_fn(|i| V::Map(&changes[i]));
            let surface = [
                (1, V::Uint(u64::from(frame.surface.width))),
                (2, V::Uint(u64::from(frame.surface.height))),
            ];
            emit(
                "InputTouchpadContacts",
                &[
                    (1, V::Uint(u64::from(frame.device))),
                    (2, V::Uint(frame.time_us)),
                    (3, V::Uint(contact_phase(frame.phase))),
                    (4, V::Map(&surface)),
                    (6, V::Array(&values[..usize::from(frame.len)])),
                ],
                out,
            )
        }
        InputMsg::VolumeKeyOrigin {
            action,
            origin: provenance,
        } => {
            let notifications = provenance.windows_notifications.unwrap_or_default();
            let notification_fields = [
                (
                    1,
                    notifications
                        .default_endpoint_age_ms
                        .map_or(V::Null, |v| V::Uint(u64::from(v))),
                ),
                (
                    2,
                    notifications
                        .arrival_removal_age_ms
                        .map_or(V::Null, |v| V::Uint(u64::from(v))),
                ),
            ];
            let fields = [
                (1, provenance.physical.map_or(V::Null, V::Bool)),
                (2, V::Uint(source(provenance.source_kind))),
                (3, provenance.keep_local_playback.map_or(V::Null, V::Bool)),
                (
                    4,
                    if provenance.windows_notifications.is_some() {
                        V::Map(&notification_fields)
                    } else {
                        V::Null
                    },
                ),
            ];
            emit(
                "InputVolumeKeyOrigin",
                &[(1, V::Uint(volume_action(*action))), (2, V::Map(&fields))],
                out,
            )
        }
        InputMsg::Volume {
            route,
            press_id,
            code,
            pressed,
            repeat,
            modifiers,
        } => {
            if origin.is_some() {
                return Err(v1::ErrorCode::UnsupportedValue.into());
            }
            let mut fields: v1::RawMap = [
                (1, code.0.write()),
                (2, pressed.write()),
                (3, repeat.write()),
                (4, modifiers.write()),
                (5, oink_network::wire_identity::boot(route.target).write()),
                (6, route.session.write()),
                (7, press_id.write()),
            ]
            .into();
            if let Some(time) = time {
                fields.insert(102, time.0.write());
            }
            let message = v1::Message::InputVolume(w::InputVolume::from_raw(fields)?);
            let bytes = v1::frame::encode(&message, out.len())?;
            out[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
        InputMsg::Forwarded { .. } => Err(v1::ErrorCode::UnsupportedValue.into()),
    }
}

pub(super) fn decode(frame: OpaqueFrame<'_>) -> v1::Result<Packet> {
    let value = hot::Message::decode(frame)?;
    macro_rules! decoded {
        ($view:expr,$input:expr) => {{
            let view = $view;
            let origin = view.origin()?.map(read_boot).transpose()?;
            let hops = view.forwarded_hops()?;
            if origin.is_some() != (hops > 0) {
                return Err(v1::ErrorCode::MalformedFrame.into());
            }
            let input = $input;
            let input = if let Some(origin) = origin {
                InputMsg::Forwarded {
                    origin,
                    hops,
                    msg: Box::new(input),
                }
            } else {
                input
            };
            match view.capture_time_us()? {
                Some(time) => Packet::TimedInput {
                    time: n::CaptureTime(time),
                    input,
                },
                None => Packet::Input(input),
            }
        }};
    }
    Ok(match value {
        hot::Message::InputMotion(v) => decoded!(
            v,
            InputMsg::Motion {
                seq: v.seq()?,
                dx: v.dx()?,
                dy: v.dy()?
            }
        ),
        hot::Message::InputRelativeMotion(v) => decoded!(
            v,
            InputMsg::RelativeMotion {
                epoch: v.epoch()?,
                generation: v.generation()?,
                seq: v.seq()?,
                x: v.x()?,
                y: v.y()?
            }
        ),
        hot::Message::InputKey(v) => decoded!(
            v,
            match v.physical_code()? {
                None => InputMsg::Key {
                    code: n::KeyCode(v.code()?),
                    pressed: v.pressed()?
                },
                Some(code) if code == n::KeyCode::CAPS_LOCK.0 => InputMsg::CapsKey {
                    code: n::KeyCode(v.code()?),
                    pressed: v.pressed()?
                },
                _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
            }
        ),
        hot::Message::InputButton(v) => decoded!(
            v,
            InputMsg::Button {
                button: read_button(v.button()?)?,
                pressed: v.pressed()?
            }
        ),
        hot::Message::InputScroll(v) => decoded!(
            v,
            InputMsg::Scroll {
                source: read_source(v.source()?)?,
                dx: v.dx()?,
                dy: v.dy()?,
                unit: read_amount(v.unit()?)?,
                phase: read_phase(v.phase()?)?
            }
        ),
        hot::Message::InputHeldSync(v) => decoded!(
            v,
            InputMsg::HeldSync {
                keys: v.keys()?.map(|v| v.map(n::KeyCode)).collect::<v1::Result<_>>()?,
                buttons: v.buttons()?.map(|v| read_button(v?)).collect::<v1::Result<_>>()?
            }
        ),
        hot::Message::InputKeyRepeat(v) => decoded!(
            v,
            InputMsg::KeyRepeat {
                code: n::KeyCode(v.code()?),
                n: v.n()?
            }
        ),
        hot::Message::InputGesture(v) => decoded!(
            v,
            InputMsg::Gesture {
                source: read_source(v.source()?)?,
                gesture: read_gesture(v.gesture()?)?
            }
        ),
        hot::Message::InputTouchpadContacts(v) => decoded!(v, {
            let surface = v.surface()?;
            let mut frame = c::Frame {
                device: v.device()?,
                time_us: v.time_us()?,
                phase: read_contact_phase(v.phase()?)?,
                surface: c::Surface {
                    width: surface.width()?,
                    height: surface.height()?,
                },
                ..Default::default()
            };
            for (index, change) in v.changes()?.enumerate() {
                let change = change?;
                let p = change.contact()?;
                frame.changes[index] = c::Change {
                    phase: read_contact_phase(change.phase()?)?,
                    contact: c::Contact {
                        id: p.id()?,
                        x: p.x()?,
                        y: p.y()?,
                    },
                };
                frame.len += 1;
            }
            InputMsg::TouchpadContacts(frame)
        }),
        hot::Message::InputVolume(v) => decoded!(v, {
            let session = std::sync::Arc::new(w::AudioSessionContext::from_raw(v.session()?.raw().to_owned()?)?);
            let route =
                oink_routing::input::VolumeRoute::from_session(session).ok_or(v1::ErrorCode::UnsupportedValue)?;
            if route.target != read_boot(v.target_boot()?)? {
                return Err(v1::ErrorCode::AuthenticationFailed.into());
            }
            InputMsg::Volume {
                route,
                press_id: v.press_id()?,
                code: n::KeyCode(v.code()?),
                pressed: v.pressed()?,
                repeat: v.repeat()?,
                modifiers: v.modifiers()?,
            }
        }),
        hot::Message::InputVolumeKeyOrigin(v) => decoded!(v, {
            let p = v.provenance()?;
            InputMsg::VolumeKeyOrigin {
                action: read_volume_action(v.action()?)?,
                origin: VolumeKeyOrigin {
                    physical: p.physical()?,
                    source_kind: read_source(p.source_kind()?)?,
                    keep_local_playback: p.keep_local_playback()?,
                    windows_notifications: p
                        .windows_notifications()?
                        .map(|n| {
                            Ok::<_, v1::Error>(n::RecentAudioNotifications {
                                default_endpoint_age_ms: n.default_endpoint_age_ms()?,
                                arrival_removal_age_ms: n.arrival_removal_age_ms()?,
                            })
                        })
                        .transpose()?,
                },
            }
        }),
        _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn roundtrip(value: InputMsg) -> InputMsg {
        let mut bytes = [0; v1::limits::RELIABLE_FRAME];
        let len = encode(&value, None, &mut bytes).unwrap();
        let Packet::Input(found) = decode(OpaqueFrame::parse_exact(&bytes[..len], bytes.len()).unwrap()).unwrap()
        else {
            panic!("input")
        };
        assert_eq!(found, value);
        found
    }
    #[test]
    fn source_resolved_caps_preserves_physical_identity_and_forwarded_boot() {
        let origin = Boot {
            device_id: DeviceId([9; 32]),
            nonce: [7; 16],
        };
        let value = InputMsg::Forwarded {
            origin,
            hops: 2,
            msg: Box::new(InputMsg::CapsKey {
                code: n::KeyCode::LEFT_SHIFT,
                pressed: false,
            }),
        };
        let mut bytes = [0; v1::limits::RELIABLE_FRAME];
        let len = encode(&value, Some(n::CaptureTime(123)), &mut bytes).unwrap();
        let frame = OpaqueFrame::parse_exact(&bytes[..len], bytes.len()).unwrap();
        let hot::Message::InputKey(key) = hot::Message::decode(frame).unwrap() else {
            panic!("key")
        };
        assert_eq!(key.physical_code().unwrap(), Some(n::KeyCode::CAPS_LOCK.0));
        assert_eq!(
            decode(frame).unwrap(),
            Packet::TimedInput {
                time: n::CaptureTime(123),
                input: value
            }
        );
    }
    #[test]
    fn typed_mouse_gestures_and_integer_contact_geometry_roundtrip() {
        for button in [n::MouseButton::Left, n::MouseButton::Other(31)] {
            roundtrip(InputMsg::Button { button, pressed: true });
        }
        for gesture in [
            n::Gesture::SmartMagnify,
            n::Gesture::DockSwipe {
                axis: n::SwipeAxis::Pinch,
                phase: n::GesturePhase::Changed,
                progress: 0.375,
                velocity: -0.5,
            },
            n::Gesture::Rotate {
                phase: n::GesturePhase::Ended,
                degrees: -24.,
            },
        ] {
            roundtrip(InputMsg::Gesture {
                source: n::SourceKind::Touchpad,
                gesture,
            });
        }
        let mut contacts = c::Frame {
            device: 7,
            time_us: 456,
            surface: c::Surface {
                width: 6860,
                height: 4116,
            },
            len: 1,
            ..Default::default()
        };
        contacts.changes[0] = c::Change {
            contact: c::Contact {
                id: 12,
                x: 3430,
                y: 2058,
            },
            phase: c::Phase::Begin,
        };
        roundtrip(InputMsg::TouchpadContacts(contacts));
        roundtrip(InputMsg::TouchpadContacts(c::Frame::cancel(7)));
    }
    #[test]
    fn malformed_forwarding_and_unknown_native_actions_are_refused() {
        use v1::{
            RawValue as R,
            schema::{WireDto, WireValue},
        };
        let base = [
            (1, 1u64.write()),
            (2, 1f64.write()),
            (3, 2f64.write()),
            (101, 1u8.write()),
        ];
        let value = w::InputMotion::from_raw(base.into()).unwrap();
        let bytes = v1::frame::encode(&v1::Message::InputMotion(value), 1024).unwrap();
        assert!(decode(OpaqueFrame::parse_exact(&bytes, 1024).unwrap()).is_err());
        let button =
            w::InputButton::from_raw([(1, R::Map([(1, R::Uint(99))].into())), (2, true.write())].into()).unwrap();
        let bytes = v1::frame::encode(&v1::Message::InputButton(button), 1024).unwrap();
        assert!(decode(OpaqueFrame::parse_exact(&bytes, 1024).unwrap()).is_err());
    }
}
