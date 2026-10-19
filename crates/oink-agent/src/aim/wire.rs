//! Explicit A0 conversion. Unknown execution phases are refused before game dispatch.
use oink_foundation::Point;
use oink_locator::aim::model::*;
use oink_network::wire_identity::{boot, device, read_boot, read_device};
use oink_protocol::v1::{
    self, Message, RawValue as V, dto,
    schema::{WireDto, WireValue},
};

fn game(value: GameInstance) -> v1::Result<dto::GameInstance> {
    dto::GameInstance::from_raw(
        [
            (1, boot(value.authority_boot).write()),
            (2, V::Uint(value.game_counter)),
        ]
        .into(),
    )
}
fn read_game(value: dto::GameInstance) -> v1::Result<GameInstance> {
    Ok(GameInstance {
        authority_boot: read_boot(&value.authority_boot()?)?,
        game_counter: value.game_counter()?,
    })
}
fn event(value: GameEvent) -> v1::Result<dto::GameEvent> {
    dto::GameEvent::from_raw(
        [
            (1, game(value.game_instance)?.write()),
            (2, boot(value.sender_boot).write()),
            (3, V::Uint(value.event_seq)),
        ]
        .into(),
    )
}
fn read_event(value: dto::GameEvent) -> v1::Result<GameEvent> {
    Ok(GameEvent {
        game_instance: read_game(value.game_instance()?)?,
        sender_boot: read_boot(&value.sender_boot()?)?,
        event_seq: value.event_seq()?,
    })
}
fn point(value: Point) -> v1::Result<dto::Point> {
    dto::Point::from_raw([(1, value.x.write()), (2, value.y.write())].into())
}
fn read_point(value: dto::Point) -> v1::Result<Point> {
    Ok(Point::new(value.x()?, value.y()?))
}

pub(crate) fn encode(value: &AimMsg) -> v1::Result<Message> {
    if !value.valid() {
        return Err(v1::ErrorCode::UnsupportedValue.into());
    }
    Ok(match value {
        AimMsg::Open { event: e } => Message::AimOpen(dto::AimOpen::from_raw([(1, event(*e)?.write())].into())?),
        AimMsg::Joined { event: e } => Message::AimJoined(dto::AimJoined::from_raw([(1, event(*e)?.write())].into())?),
        AimMsg::Declined { event: e } => {
            Message::AimDeclined(dto::AimDeclined::from_raw([(1, event(*e)?.write())].into())?)
        }
        AimMsg::Close { event: e } => Message::AimClose(dto::AimClose::from_raw([(1, event(*e)?.write())].into())?),
        AimMsg::Click { event: e, display, pos } => Message::AimClick(dto::AimClick::from_raw(
            [
                (1, event(*e)?.write()),
                (2, V::Text(display.clone())),
                (3, point(*pos)?.write()),
            ]
            .into(),
        )?),
        AimMsg::State(s) => {
            let targets = s
                .targets
                .iter()
                .map(|t| {
                    dto::AimTarget::from_raw(
                        [
                            (1, t.id.write()),
                            (2, device(t.device).write()),
                            (3, V::Text(t.display.clone())),
                            (4, point(t.pos)?.write()),
                        ]
                        .into(),
                    )
                    .map(|t| t.write())
                })
                .collect::<v1::Result<Vec<_>>>()?;
            let phase = match s.phase {
                AimPhase::Countdown => dto::AimPhase::Countdown,
                AimPhase::Running => dto::AimPhase::Running,
                AimPhase::Finished => dto::AimPhase::Finished,
            };
            Message::AimState(dto::AimState::from_raw(
                [
                    (1, game(s.game)?.write()),
                    (2, s.seq.write()),
                    (3, phase.write()),
                    (4, s.remaining_ms.write()),
                    (5, s.score.write()),
                    (6, s.clicks.write()),
                    (7, s.radius.write()),
                    (8, s.native_input.write()),
                    (9, V::Array(targets)),
                ]
                .into(),
            )?)
        }
    })
}

pub(crate) fn decode(value: Message) -> v1::Result<AimMsg> {
    let value = match value {
        Message::AimOpen(m) => AimMsg::Open {
            event: read_event(m.event()?)?,
        },
        Message::AimJoined(m) => AimMsg::Joined {
            event: read_event(m.event()?)?,
        },
        Message::AimDeclined(m) => AimMsg::Declined {
            event: read_event(m.event()?)?,
        },
        Message::AimClose(m) => AimMsg::Close {
            event: read_event(m.event()?)?,
        },
        Message::AimClick(m) => AimMsg::Click {
            event: read_event(m.event()?)?,
            display: m.display()?.0,
            pos: read_point(m.pos()?)?,
        },
        Message::AimState(s) => {
            let phase = match s.phase()? {
                dto::AimPhase::Countdown => AimPhase::Countdown,
                dto::AimPhase::Running => AimPhase::Running,
                dto::AimPhase::Finished => AimPhase::Finished,
                dto::AimPhase::Unknown(_) => return Err(v1::ErrorCode::UnsupportedValue.into()),
            };
            let targets = s
                .targets()?
                .0
                .into_iter()
                .map(|t| {
                    Ok(AimTarget {
                        id: t.id()?,
                        device: read_device(&t.device()?)?,
                        display: t.display()?.0,
                        pos: read_point(t.pos()?)?,
                    })
                })
                .collect::<v1::Result<Vec<_>>>()?;
            AimMsg::State(AimState {
                game: read_game(s.game()?)?,
                seq: s.seq()?,
                phase,
                remaining_ms: s.remaining_ms()?,
                score: s.score()?,
                clicks: s.clicks()?,
                radius: s.radius()?,
                native_input: s.native_input()?,
                targets,
            })
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
    use oink_foundation::{DeviceId, session::Boot};

    #[test]
    fn game_identity_and_reference_coordinates_survive_framing() {
        let sender = Boot {
            device_id: DeviceId([1; 32]),
            nonce: [2; 16],
        };
        let game = GameInstance {
            authority_boot: sender,
            game_counter: 7,
        };
        let event = GameEvent {
            game_instance: game,
            sender_boot: sender,
            event_seq: 9,
        };
        let state = AimMsg::State(AimState {
            game,
            seq: 12,
            phase: AimPhase::Running,
            remaining_ms: 1000,
            score: 2,
            clicks: 3,
            radius: 7.0 / 686.0,
            native_input: false,
            targets: vec![AimTarget {
                id: 4,
                device: sender.device_id,
                display: "screen".into(),
                pos: Point::new(0.2, -0.1),
            }],
        });
        for value in [
            AimMsg::Open { event },
            AimMsg::Joined { event },
            AimMsg::Declined { event },
            AimMsg::Click {
                event,
                display: "screen".into(),
                pos: Point::new(0.2, -0.1),
            },
            AimMsg::Close { event },
            state.clone(),
        ] {
            let bytes = v1::frame::encode(&encode(&value).unwrap(), v1::limits::RELIABLE_FRAME).unwrap();
            let wire = v1::frame::OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME)
                .unwrap()
                .decode()
                .unwrap();
            assert_eq!(decode(wire).unwrap(), value);
        }
        let Message::AimState(mut future) = encode(&state).unwrap() else {
            panic!()
        };
        future
            .edit(|raw| {
                raw.insert(3, V::Uint(99));
            })
            .unwrap();
        assert_eq!(
            decode(Message::AimState(future)).unwrap_err().code,
            v1::ErrorCode::UnsupportedValue
        );
    }
}
