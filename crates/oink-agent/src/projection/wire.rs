//! P0 messages carry ratio-plane rectangles; conversion never writes persisted layout.
use super::model::{Action, ScreenRect};
use oink_foundation::Rect;
use oink_network::wire_identity::{device, read_device};
use oink_protocol::v2::{
    self, Message, RawValue as V, dto,
    schema::{WireDto, WireValue},
};
fn rect(rect: Rect) -> v2::Result<dto::Rect> {
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
pub(super) fn encode(action: &Action) -> v2::Result<Message> {
    if !action.valid() {
        return Err(v2::ErrorCode::UnsupportedValue.into());
    }
    Ok(match action {
        Action::Arrangement {
            sequence,
            lease_ms,
            expires_ms,
            snapshot,
        } => {
            let mut raw = [
                (1, V::Bool(true)),
                (2, sequence.write()),
                (3, lease_ms.write()),
                (4, expires_ms.write()),
            ]
            .into_iter()
            .collect::<v2::RawMap>();
            if let Some(s) = snapshot {
                raw.insert(5, crate::layout_profiles::wire::snapshot(s)?);
            }
            Message::ArrangementPreview(dto::ArrangementPreview::from_raw(raw)?)
        }
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
                .collect::<v2::Result<Vec<_>>>()?;
            let mut raw = [(1, sequence.write()), (3, V::Array(affected))]
                .into_iter()
                .collect::<v2::RawMap>();
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
                    .collect::<v2::Result<Vec<_>>>()?;
                raw.insert(2, V::Array(screens));
            }
            Message::ProjectionPreview(dto::ProjectionPreview::from_raw(raw)?)
        }
    })
}
pub(crate) fn decode(message: Message) -> v2::Result<Action> {
    let value = match message {
        Message::ArrangementPreview(m) => Action::Arrangement {
            sequence: m.sequence()?,
            lease_ms: m.lease_ms()?,
            expires_ms: m.expires_ms()?,
            snapshot: m
                .arrangement()?
                .map(|s| crate::layout_profiles::wire::read_snapshot(s, "").map(Box::new))
                .transpose()?,
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
                        .collect::<v2::Result<Vec<_>>>()
                })
                .transpose()?;
            let affected = m
                .affected()?
                .0
                .into_iter()
                .map(|s| Ok((read_device(&s.device()?)?, s.display()?.0)))
                .collect::<v2::Result<Vec<_>>>()?;
            Action::Preview {
                sequence: m.sequence()?,
                screens,
                affected,
            }
        }
        _ => return Err(v2::ErrorCode::InvalidChannel.into()),
    };
    if !value.valid() {
        return Err(v2::ErrorCode::UnsupportedValue.into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::DeviceId;

    #[test]
    fn projection_conversion_preserves_preview_and_clear_semantics() {
        let owner = DeviceId([1; 32]);
        for value in [
            Action::Preview {
                sequence: 3,
                screens: None,
                affected: vec![],
            },
            Action::Preview {
                sequence: 4,
                screens: Some(vec![ScreenRect {
                    device: owner,
                    display: "screen".into(),
                    rect: Rect::new(-0.2, 0.0, 0.5, 0.3),
                }]),
                affected: vec![(owner, "screen".into())],
            },
        ] {
            let bytes = v2::frame::encode(&encode(&value).unwrap(), v2::limits::RELIABLE_FRAME).unwrap();
            let wire = v2::frame::OpaqueFrame::parse_exact(&bytes, v2::limits::RELIABLE_FRAME)
                .unwrap()
                .decode()
                .unwrap();
            assert_eq!(decode(wire).unwrap(), value);
        }
        let value = Action::Preview {
            sequence: 5,
            screens: Some(vec![ScreenRect {
                device: owner,
                display: "screen".into(),
                rect: Rect::new(0.0, 0.0, -1.0, 0.3),
            }]),
            affected: vec![],
        };
        assert_eq!(encode(&value).unwrap_err().code, v2::ErrorCode::UnsupportedValue);
    }
}
