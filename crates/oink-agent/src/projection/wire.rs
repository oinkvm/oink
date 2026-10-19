//! P0 messages carry ratio-plane rectangles; conversion never writes persisted layout.
use super::model::{Action, ModeId, ScreenRect};
use oink_foundation::Rect;
use oink_network::wire_identity::{device, read_device};
use oink_protocol::v1::{
    self, Message, RawValue as V, dto,
    schema::{WireDto, WireValue},
};
fn mode(id: ModeId) -> v1::Result<dto::ModeId> {
    dto::ModeId::from_raw(
        [
            (1, device(id.owner).write()),
            (2, id.serial.write()),
            (3, V::Bytes(id.boot_nonce.to_vec())),
        ]
        .into(),
    )
}
fn read_mode(id: dto::ModeId) -> v1::Result<ModeId> {
    Ok(ModeId {
        owner: read_device(&id.owner()?)?,
        serial: id.serial()?,
        boot_nonce: id.boot_nonce()?.0,
    })
}
fn rect(rect: Rect) -> v1::Result<dto::Rect> {
    dto::Rect::from_raw(
        [
            (1, rect.x.write()),
            (2, rect.y.write()),
            (3, rect.w.write()),
            (4, rect.h.write()),
        ]
        .into(),
    )
}
pub(super) fn encode(action: &Action) -> v1::Result<Message> {
    if !action.valid() {
        return Err(v1::ErrorCode::UnsupportedValue.into());
    }
    Ok(match action {
        Action::Mode { id, enabled } => Message::ProjectionMode(dto::ProjectionMode::from_raw(
            [(1, mode(*id)?.write()), (2, enabled.write())].into(),
        )?),
        Action::Gesture { id, active } => Message::ProjectionGesture(dto::ProjectionGesture::from_raw(
            [(1, mode(*id)?.write()), (2, active.write())].into(),
        )?),
        Action::Preview {
            sequence,
            screens,
            affected,
        } => {
            let affected = affected
                .iter()
                .map(|(id, display)| {
                    dto::ScreenRef::from_raw([(1, device(*id).write()), (2, V::Text(display.clone()))].into())
                        .map(|value| value.write())
                })
                .collect::<v1::Result<Vec<_>>>()?;
            let mut raw = [(1, sequence.write()), (3, V::Array(affected))]
                .into_iter()
                .collect::<v1::RawMap>();
            if let Some(screens) = screens {
                let screens = screens
                    .iter()
                    .map(|s| {
                        dto::ScreenRect::from_raw(
                            [
                                (1, device(s.device).write()),
                                (2, V::Text(s.display.clone())),
                                (3, rect(s.rect)?.write()),
                            ]
                            .into(),
                        )
                        .map(|value| value.write())
                    })
                    .collect::<v1::Result<Vec<_>>>()?;
                raw.insert(2, V::Array(screens));
            }
            Message::ProjectionPreview(dto::ProjectionPreview::from_raw(raw)?)
        }
    })
}
pub(crate) fn decode(message: Message) -> v1::Result<Action> {
    let value = match message {
        Message::ProjectionMode(m) => Action::Mode {
            id: read_mode(m.id()?)?,
            enabled: m.enabled()?,
        },
        Message::ProjectionGesture(m) => Action::Gesture {
            id: read_mode(m.id()?)?,
            active: m.active()?,
        },
        Message::ProjectionPreview(m) => {
            let screens = m
                .screens()?
                .map(|list| {
                    list.0
                        .into_iter()
                        .map(|s| {
                            let r = s.rect()?;
                            Ok(ScreenRect {
                                device: read_device(&s.device()?)?,
                                display: s.display()?.0,
                                rect: Rect::new(r.x()?, r.y()?, r.w()?, r.h()?),
                            })
                        })
                        .collect::<v1::Result<Vec<_>>>()
                })
                .transpose()?;
            let affected = m
                .affected()?
                .0
                .into_iter()
                .map(|s| Ok((read_device(&s.device()?)?, s.display()?.0)))
                .collect::<v1::Result<Vec<_>>>()?;
            Action::Preview {
                sequence: m.sequence()?,
                screens,
                affected,
            }
        }
        _ => return Err(v1::ErrorCode::InvalidChannel.into()),
    };
    if !value.valid() {
        return Err(v1::ErrorCode::UnsupportedValue.into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::DeviceId;

    #[test]
    fn projection_conversion_preserves_mode_and_clear_semantics() {
        let id = ModeId {
            owner: DeviceId([1; 32]),
            serial: 7,
            boot_nonce: [3; 16],
        };
        for value in [
            Action::Mode { id, enabled: true },
            Action::Gesture { id, active: false },
            Action::Preview {
                sequence: 3,
                screens: None,
                affected: vec![],
            },
            Action::Preview {
                sequence: 4,
                screens: Some(vec![ScreenRect {
                    device: id.owner,
                    display: "screen".into(),
                    rect: Rect::new(-0.2, 0.0, 0.5, 0.3),
                }]),
                affected: vec![(id.owner, "screen".into())],
            },
        ] {
            let bytes = v1::frame::encode(&encode(&value).unwrap(), v1::limits::RELIABLE_FRAME).unwrap();
            let wire = v1::frame::OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME)
                .unwrap()
                .decode()
                .unwrap();
            assert_eq!(decode(wire).unwrap(), value);
        }
        let value = Action::Preview {
            sequence: 5,
            screens: Some(vec![ScreenRect {
                device: id.owner,
                display: "screen".into(),
                rect: Rect::new(0.0, 0.0, -1.0, 0.3),
            }]),
            affected: vec![],
        };
        assert_eq!(encode(&value).unwrap_err().code, v1::ErrorCode::UnsupportedValue);
    }
}
