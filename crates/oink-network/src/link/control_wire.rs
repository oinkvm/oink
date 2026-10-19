//! Explicit network-domain conversion. Raw extension maps are validated by the
//! protocol DTO before these endpoint semantics are used.
use super::control::*;
use crate::{LowOptions, LowTcpPurpose, wire_identity as id};
use oink_foundation::session::RouteRevision;
use oink_protocol::v1::{
    self, Message, RawMap, RawValue, dto,
    schema::{WireDto, WireValue},
};

type Result<T> = v1::Result<T>;
fn get<T: WireValue>(map: &RawMap, key: u32) -> Result<T> {
    T::read(map.get(&key).ok_or(v1::ErrorCode::MalformedFrame)?)
}
fn optional<T>(map: &RawMap, key: u32, read: impl FnOnce(&RawMap) -> Result<T>) -> Result<Option<T>> {
    map.get(&key).map(|value| read(&RawMap::read(value)?)).transpose()
}
fn map<const N: usize>(entries: [(u32, RawValue); N]) -> RawMap {
    entries.into()
}
fn bytes(value: &[u8]) -> RawValue {
    RawValue::Bytes(value.to_vec())
}
fn random(map: &RawMap, key: u32) -> Result<[u8; 16]> {
    Ok(get::<dto::Id16>(map, key)?.0)
}
fn req(value: PathRequestId) -> RawValue {
    RawValue::Map(map([
        (1, id::boot(value.source_boot).write()),
        (2, bytes(&value.random)),
    ]))
}
fn read_req(value: &RawMap) -> Result<PathRequestId> {
    Ok(PathRequestId {
        source_boot: id::read_boot(&get(value, 1)?)?,
        random: random(value, 2)?,
    })
}
fn reservation(value: RelayReservation) -> RawValue {
    RawValue::Map(map([
        (1, id::boot(value.relay).write()),
        (2, bytes(&value.random)),
        (3, id::connection(value.source_connection).write()),
        (4, id::connection(value.target_connection).write()),
    ]))
}
fn read_reservation(value: &RawMap) -> Result<RelayReservation> {
    Ok(RelayReservation {
        relay: id::read_boot(&get(value, 1)?)?,
        random: random(value, 2)?,
        source_connection: id::read_connection(&get(value, 3)?)?,
        target_connection: id::read_connection(&get(value, 4)?)?,
    })
}
fn hop(value: NextHopBinding) -> RawValue {
    RawValue::Map(map([
        (1, id::device(value.peer).write()),
        (2, id::connection(value.connection_id).write()),
        (3, value.route_tag.write()),
    ]))
}
fn read_hop(value: &RawMap) -> Result<NextHopBinding> {
    Ok(NextHopBinding {
        peer: id::read_device(&get(value, 1)?)?,
        connection_id: id::read_connection(&get(value, 2)?)?,
        route_tag: get(value, 3)?,
    })
}
fn binding(value: LowBinding) -> RawValue {
    RawValue::Map(map([
        (1, id::path(value.path_id).write()),
        (2, value.route_revision.0.write()),
        (3, id::boot(value.source).write()),
        (4, id::boot(value.target).write()),
        (5, hop(value.hop)),
    ]))
}
fn read_binding(value: &RawMap) -> Result<LowBinding> {
    Ok(LowBinding {
        path_id: id::read_path(&get(value, 1)?)?,
        route_revision: RouteRevision(get(value, 2)?),
        source: id::read_boot(&get(value, 3)?)?,
        target: id::read_boot(&get(value, 4)?)?,
        hop: read_hop(&get(value, 5)?)?,
    })
}
fn migration(value: MigrationId) -> RawValue {
    RawValue::Map(map([
        (1, id::path(value.path_id).write()),
        (2, value.to_revision.0.write()),
    ]))
}
fn read_migration(value: &RawMap) -> Result<MigrationId> {
    Ok(MigrationId {
        path_id: id::read_path(&get(value, 1)?)?,
        to_revision: RouteRevision(get(value, 2)?),
    })
}
fn reference(value: PathReference) -> RawValue {
    let (kind, body) = match value {
        PathReference::Request(value) => (0u32, req(value)),
        PathReference::Installed(value) => (1, id::path(value).write()),
    };
    RawValue::Map(map([(1, kind.write()), (2, RawValue::Map(map([(1, body)])))]))
}
fn read_reference(value: &RawMap) -> Result<PathReference> {
    let body: RawMap = get(value, 2)?;
    match get::<u32>(value, 1)? {
        0 => Ok(PathReference::Request(read_req(&get(&body, 1)?)?)),
        1 => Ok(PathReference::Installed(id::read_path(&get(&body, 1)?)?)),
        _ => Err(v1::ErrorCode::UnsupportedValue.into()),
    }
}
fn options(value: LowOptions) -> RawValue {
    RawValue::Map(map([
        (1, value.send_max.write()),
        (2, value.receive_max.write()),
        (3, value.operation_max.write()),
    ]))
}
fn read_options(value: &RawMap) -> Result<LowOptions> {
    let options = LowOptions {
        send_max: get(value, 1)?,
        receive_max: get(value, 2)?,
        operation_max: get(value, 3)?,
    };
    if !options.valid() {
        return Err(v1::ErrorCode::LimitExceeded.into());
    }
    Ok(options)
}
fn request(value: &LowRequest) -> RawMap {
    let purpose = match value.purpose {
        LowTcpPurpose::Transfer => 0u32,
        LowTcpPurpose::Warm => 1,
        LowTcpPurpose::Race => 2,
    };
    let mut raw = map([
        (1, bytes(&value.request)),
        (2, bytes(&value.connection)),
        (4, value.extra.write()),
        (5, value.keep_alive.write()),
        (6, purpose.write()),
        (7, value.paced.write()),
        (8, binding(value.binding)),
        (9, value.route_tag.write()),
        (10, value.kind.write()),
    ]);
    if let Some(value) = value.options {
        raw.insert(3, options(value));
    }
    raw
}
fn read_request(raw: &RawMap) -> Result<LowRequest> {
    let purpose = match get::<u32>(raw, 6)? {
        0 => LowTcpPurpose::Transfer,
        1 => LowTcpPurpose::Warm,
        2 => LowTcpPurpose::Race,
        _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
    };
    let kind = get(raw, 10)?;
    Ok(LowRequest {
        request: random(raw, 1)?,
        connection: random(raw, 2)?,
        options: optional(raw, 3, read_options)?,
        extra: get(raw, 4)?,
        keep_alive: get(raw, 5)?,
        purpose,
        paced: get(raw, 7)?,
        binding: read_binding(&get(raw, 8)?)?,
        route_tag: get(raw, 9)?,
        kind,
    })
}
fn install(value: PathInstall) -> RawMap {
    let mut raw = map([
        (1, req(value.request_id)),
        (2, id::path(value.path_id).write()),
        (3, value.route_revision.0.write()),
        (5, hop(value.next_hop_binding)),
    ]);
    if let Some(value) = value.reservation {
        raw.insert(4, reservation(value));
    }
    raw
}
fn read_install(raw: &RawMap) -> Result<PathInstall> {
    Ok(PathInstall {
        request_id: read_req(&get(raw, 1)?)?,
        path_id: id::read_path(&get(raw, 2)?)?,
        route_revision: RouteRevision(get(raw, 3)?),
        reservation: optional(raw, 4, read_reservation)?,
        next_hop_binding: read_hop(&get(raw, 5)?)?,
    })
}
fn pulse(value: Pulse) -> RawMap {
    map([
        (1, id::path(value.path_id).write()),
        (2, value.route_revision.0.write()),
        (3, value.nonce.write()),
    ])
}
fn read_pulse(raw: &RawMap) -> Result<Pulse> {
    Ok(Pulse {
        path_id: id::read_path(&get(raw, 1)?)?,
        route_revision: RouteRevision(get(raw, 2)?),
        nonce: get(raw, 3)?,
    })
}
fn moving(value: Migration) -> RawMap {
    map([
        (1, migration(value.migration_id)),
        (2, id::path(value.old_path).write()),
        (3, id::connection(value.new_connection).write()),
    ])
}
fn read_moving(raw: &RawMap) -> Result<Migration> {
    Ok(Migration {
        migration_id: read_migration(&get(raw, 1)?)?,
        old_path: id::read_path(&get(raw, 2)?)?,
        new_connection: id::read_connection(&get(raw, 3)?)?,
    })
}
fn refusal(value: Refusal) -> RawValue {
    (value as u32).write()
}
fn read_refusal(raw: &RawMap, key: u32) -> Result<Refusal> {
    Ok(match get::<u32>(raw, key)? {
        0 => Refusal::NotReady,
        1 => Refusal::Paused,
        2 => Refusal::Closed,
        3 => Refusal::Busy,
        4 => Refusal::Invalid,
        5 => Refusal::NotMember,
        6 => Refusal::DifferentGroups,
        7 => Refusal::Superseded,
        8 => Refusal::Expired,
        9 => Refusal::Cancelled,
        _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
    })
}
fn protocol_error(code: v1::ErrorCode) -> RawValue {
    RawValue::Map(map([
        (1, (code.number() as u16).write()),
        (2, dto::ErrorScope::LowOperation.write()),
    ]))
}
fn failure(value: &crate::LowUnavailable) -> RawValue {
    let (code, stage) = match &value.reason {
        crate::LowFailureReason::NoDirectNeighbor => (dto::LowFailureCode::NoDirectNeighbor, None),
        crate::LowFailureReason::Tcp { stage, .. } => match stage.as_str() {
            "connect" => (dto::LowFailureCode::ConnectFailed, Some(dto::TcpStage::Connect)),
            "tls" => (dto::LowFailureCode::TlsFailed, Some(dto::TcpStage::Tls)),
            "read" => (dto::LowFailureCode::PeerClosed, Some(dto::TcpStage::Read)),
            "write" => (dto::LowFailureCode::PeerClosed, Some(dto::TcpStage::Write)),
            _ => (dto::LowFailureCode::PeerClosed, Some(dto::TcpStage::Open)),
        },
    };
    let mut raw = map([
        (1, id::device(value.from).write()),
        (2, id::device(value.to).write()),
        (4, code.write()),
    ]);
    if let Some(stage) = stage {
        raw.insert(5, stage.write());
    }
    RawValue::Map(raw)
}
fn read_failure(raw: &RawMap) -> Result<crate::LowUnavailable> {
    let value = dto::LowUnavailable::from_raw(raw.clone())?;
    let reason = match value.code()? {
        dto::LowFailureCode::NoDirectNeighbor => crate::LowFailureReason::NoDirectNeighbor,
        code @ (dto::LowFailureCode::ConnectFailed
        | dto::LowFailureCode::TlsFailed
        | dto::LowFailureCode::PeerClosed
        | dto::LowFailureCode::Timeout
        | dto::LowFailureCode::Unsupported) => crate::LowFailureReason::Tcp {
            stage: format!("{:?}", value.stage()?),
            detail: format!("{code:?}"),
        },
        _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
    };
    Ok(crate::LowUnavailable {
        from: id::read_device(&value.from()?)?,
        to: id::read_device(&value.to()?)?,
        reason,
    })
}

pub(super) fn encode(message: &LinkMsg) -> Result<Vec<u8>> {
    v1::frame::encode(&to_wire(message)?, v1::limits::LOW_OUTER_FRAME)
}
pub(super) fn decode(bytes: &[u8]) -> Result<LinkMsg> {
    from_wire(v1::frame::OpaqueFrame::parse_exact(bytes, v1::limits::LOW_OUTER_FRAME)?.decode()?)
}
fn to_wire(message: &LinkMsg) -> Result<Message> {
    macro_rules! dto {
        ($kind:ident, $raw:expr) => {
            Message::$kind(dto::$kind::from_raw($raw)?)
        };
    }
    Ok(match message {
        LinkMsg::ConnectionSelect {
            boot_pair,
            connection_id,
            choice_counter,
        } => dto!(
            LinkConnectionSelect,
            map([
                (1, id::boot_pair(*boot_pair).write()),
                (2, id::connection(*connection_id).write()),
                (3, choice_counter.write())
            ])
        ),
        LinkMsg::ConnectionSelected {
            boot_pair,
            connection_id,
            choice_counter,
        } => dto!(
            LinkConnectionSelected,
            map([
                (1, id::boot_pair(*boot_pair).write()),
                (2, id::connection(*connection_id).write()),
                (3, choice_counter.write())
            ])
        ),
        LinkMsg::ConnectionReject { id: connection, reason } => dto!(
            LinkConnectionReject,
            map([(1, id::connection(*connection).write()), (2, refusal(*reason))])
        ),
        LinkMsg::PathRequest {
            reservation: reserved,
            request_id,
            expected_target_boot,
            target,
            round,
        } => {
            let mut raw = map([
                (2, req(*request_id)),
                (4, id::device(*target).write()),
                (5, round.write()),
            ]);
            if let Some(value) = reserved {
                raw.insert(1, reservation(*value));
            }
            if let Some(value) = expected_target_boot {
                raw.insert(3, id::boot(*value).write());
            }
            dto!(LinkPathRequest, raw)
        }
        LinkMsg::PathNominate {
            request_id,
            reservation: reserved,
            endpoint_boot,
        } => dto!(
            LinkPathNominate,
            map([
                (1, req(*request_id)),
                (2, reservation(*reserved)),
                (3, id::boot(*endpoint_boot).write())
            ])
        ),
        LinkMsg::PathInstall(value) => dto!(LinkPathInstall, install(*value)),
        LinkMsg::PathInstalled(value) => dto!(LinkPathInstalled, install(*value)),
        LinkMsg::PathCancel {
            reference: value,
            reason,
        } => dto!(LinkPathCancel, map([(1, reference(*value)), (2, refusal(*reason))])),
        LinkMsg::PathDown { path_id } => dto!(LinkPathDown, map([(1, id::path(*path_id).write())])),
        LinkMsg::Pulse(value) => dto!(LinkPulse, pulse(*value)),
        LinkMsg::Pong(value) => dto!(LinkPong, pulse(*value)),
        LinkMsg::HopProbe { nonce } => dto!(LinkHopProbe, map([(1, nonce.write())])),
        LinkMsg::HopPong { nonce } => dto!(LinkHopPong, map([(1, nonce.write())])),
        LinkMsg::MigrateBegin {
            path_id,
            from_revision,
            to_revision,
            connection_id,
            migration_id,
        } => dto!(
            LinkMigrateBegin,
            map([
                (1, id::path(*path_id).write()),
                (2, from_revision.0.write()),
                (3, to_revision.0.write()),
                (4, id::connection(*connection_id).write()),
                (5, migration(*migration_id))
            ])
        ),
        LinkMsg::MigrateDrained(value) => dto!(LinkMigrateDrained, moving(*value)),
        LinkMsg::MigrateCommit(value) => dto!(LinkMigrateCommit, moving(*value)),
        LinkMsg::MigrateCommitted(value) => dto!(LinkMigrateCommitted, moving(*value)),
        LinkMsg::Low {
            binding: lease,
            message,
        } => {
            let (type_id, mut raw) = match message.as_ref() {
                LowMsg::Info { paced, port, failure } => {
                    let mut raw = map([(1, paced.write())]);
                    if let Some(port) = port {
                        raw.insert(2, port.write());
                    }
                    if failure.is_some() {
                        raw.insert(3, protocol_error(v1::ErrorCode::NotReady));
                    }
                    (0xc20, raw)
                }
                LowMsg::Open(value) => (0xc21, request(value)),
                LowMsg::Ready { request, result } => (
                    0xc22,
                    map([
                        (1, bytes(request)),
                        (
                            2,
                            RawValue::Map(if result.is_ok() {
                                map([(1, 0u32.write())])
                            } else {
                                map([(1, 1u32.write()), (2, protocol_error(v1::ErrorCode::NotReady))])
                            }),
                        ),
                    ]),
                ),
                LowMsg::Reverse { request } => (0xc23, map([(1, bytes(request))])),
                LowMsg::Cancel { request } => (0xc24, map([(1, bytes(request))])),
                LowMsg::Bind {
                    request: value,
                    reverse,
                } => (0xc25, map([(1, RawValue::Map(request(value))), (2, reverse.write())])),
                LowMsg::Record { bytes: value } => (0xc26, map([(1, bytes(value))])),
                LowMsg::End { request } => (0xc27, map([(1, bytes(request))])),
                LowMsg::EndAck { request } => (0xc28, map([(1, bytes(request))])),
                LowMsg::RaceWinner { request, reverse } => (0xc29, map([(1, bytes(request)), (2, reverse.write())])),
                LowMsg::Refresh => (0xc2a, RawMap::new()),
                LowMsg::RelayUnavailable {
                    request,
                    failure: value,
                } => (0xc2b, map([(1, bytes(request)), (2, failure(value))])),
            };
            raw.insert(100, binding(*lease));
            Message::from_payload(type_id, raw)?
        }
    })
}
fn from_wire(message: Message) -> Result<LinkMsg> {
    let raw = message.raw();
    Ok(match &message {
        Message::LinkConnectionSelect(_) => LinkMsg::ConnectionSelect {
            boot_pair: id::read_boot_pair(&get(raw, 1)?)?,
            connection_id: id::read_connection(&get(raw, 2)?)?,
            choice_counter: get(raw, 3)?,
        },
        Message::LinkConnectionSelected(_) => LinkMsg::ConnectionSelected {
            boot_pair: id::read_boot_pair(&get(raw, 1)?)?,
            connection_id: id::read_connection(&get(raw, 2)?)?,
            choice_counter: get(raw, 3)?,
        },
        Message::LinkConnectionReject(_) => LinkMsg::ConnectionReject {
            id: id::read_connection(&get(raw, 1)?)?,
            reason: read_refusal(raw, 2)?,
        },
        Message::LinkPathRequest(_) => LinkMsg::PathRequest {
            reservation: optional(raw, 1, read_reservation)?,
            request_id: read_req(&get(raw, 2)?)?,
            expected_target_boot: raw
                .get(&3)
                .map(|value| id::read_boot(&dto::Boot::read(value)?))
                .transpose()?,
            target: id::read_device(&get(raw, 4)?)?,
            round: get(raw, 5)?,
        },
        Message::LinkPathNominate(_) => LinkMsg::PathNominate {
            request_id: read_req(&get(raw, 1)?)?,
            reservation: read_reservation(&get(raw, 2)?)?,
            endpoint_boot: id::read_boot(&get(raw, 3)?)?,
        },
        Message::LinkPathInstall(_) => LinkMsg::PathInstall(read_install(raw)?),
        Message::LinkPathInstalled(_) => LinkMsg::PathInstalled(read_install(raw)?),
        Message::LinkPathCancel(_) => LinkMsg::PathCancel {
            reference: read_reference(&get(raw, 1)?)?,
            reason: read_refusal(raw, 2)?,
        },
        Message::LinkPathDown(_) => LinkMsg::PathDown {
            path_id: id::read_path(&get(raw, 1)?)?,
        },
        Message::LinkPulse(_) => LinkMsg::Pulse(read_pulse(raw)?),
        Message::LinkPong(_) => LinkMsg::Pong(read_pulse(raw)?),
        Message::LinkHopProbe(_) => LinkMsg::HopProbe { nonce: get(raw, 1)? },
        Message::LinkHopPong(_) => LinkMsg::HopPong { nonce: get(raw, 1)? },
        Message::LinkMigrateBegin(_) => LinkMsg::MigrateBegin {
            path_id: id::read_path(&get(raw, 1)?)?,
            from_revision: RouteRevision(get(raw, 2)?),
            to_revision: RouteRevision(get(raw, 3)?),
            connection_id: id::read_connection(&get(raw, 4)?)?,
            migration_id: read_migration(&get(raw, 5)?)?,
        },
        Message::LinkMigrateDrained(_) => LinkMsg::MigrateDrained(read_moving(raw)?),
        Message::LinkMigrateCommit(_) => LinkMsg::MigrateCommit(read_moving(raw)?),
        Message::LinkMigrateCommitted(_) => LinkMsg::MigrateCommitted(read_moving(raw)?),
        Message::LowInfo(_)
        | Message::LowOpen(_)
        | Message::LowReady(_)
        | Message::LowReverse(_)
        | Message::LowCancel(_)
        | Message::LowBind(_)
        | Message::LowRecord(_)
        | Message::LowEnd(_)
        | Message::LowEndAck(_)
        | Message::LowRaceWinner(_)
        | Message::LowRefresh(_)
        | Message::LowRelayUnavailable(_) => {
            let lease = read_binding(&get(raw, 100)?)?;
            let value = match &message {
                Message::LowInfo(value) => LowMsg::Info {
                    paced: value.paced()?,
                    port: value.port()?,
                    failure: value
                        .failure()?
                        .map(|error| {
                            error
                                .code()
                                .map(|code| format!("{:?}", v1::ErrorCode::from_number(code.into())))
                        })
                        .transpose()?,
                },
                Message::LowOpen(_) => LowMsg::Open(read_request(raw)?),
                Message::LowReady(value) => {
                    let result = value.result()?;
                    let result = match result.variant()? {
                        v1::schema::Open::Known(dto::ResultKind::Success) => Ok(()),
                        v1::schema::Open::Known(dto::ResultKind::Failure) => Err(format!(
                            "{:?}",
                            v1::ErrorCode::from_number(get::<u16>(&result.body()?, 1)?.into())
                        )),
                        _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
                    };
                    LowMsg::Ready {
                        request: value.request()?.0,
                        result,
                    }
                }
                Message::LowReverse(_) => LowMsg::Reverse {
                    request: random(raw, 1)?,
                },
                Message::LowCancel(_) => LowMsg::Cancel {
                    request: random(raw, 1)?,
                },
                Message::LowBind(_) => LowMsg::Bind {
                    request: read_request(&get(raw, 1)?)?,
                    reverse: get(raw, 2)?,
                },
                Message::LowRecord(value) => LowMsg::Record {
                    bytes: value.bytes()?.0,
                },
                Message::LowEnd(_) => LowMsg::End {
                    request: random(raw, 1)?,
                },
                Message::LowEndAck(_) => LowMsg::EndAck {
                    request: random(raw, 1)?,
                },
                Message::LowRaceWinner(_) => LowMsg::RaceWinner {
                    request: random(raw, 1)?,
                    reverse: get(raw, 2)?,
                },
                Message::LowRefresh(_) => LowMsg::Refresh,
                Message::LowRelayUnavailable(_) => LowMsg::RelayUnavailable {
                    request: random(raw, 1)?,
                    failure: read_failure(&get(raw, 2)?)?,
                },
                _ => unreachable!(),
            };
            LinkMsg::Low {
                binding: lease,
                message: Box::new(value),
            }
        }
        _ => return Err(v1::ErrorCode::InvalidChannel.into()),
    })
}
