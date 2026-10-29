//! Native identities never cross the application-focus wire boundary.
use oink_foundation::application_focus::{Foreground, Observation, ObservationStamp, ParkResult};
use oink_network::wire_identity::{boot, read_boot};
use oink_protocol::v1::{
    self, RawMap, RawValue, dto as w,
    schema::{List, Open, WireDto, WireValue},
};
use oink_routing::application_focus as a;

macro_rules! enums {
    ($to:ident, $from:ident, $domain:path, $wire:path, [$($variant:ident => $wire_variant:ident),+]) => {
        fn $to(value: $domain) -> $wire {
            use $domain as D;
            use $wire as W;
            match value { $(D::$variant => W::$wire_variant,)+ }
        }
        fn $from(value: $wire) -> v1::Result<$domain> {
            use $domain as D;
            use $wire as W;
            match value {
                $(W::$wire_variant => Ok(D::$variant),)+
                _ => Err(v1::ErrorCode::UnsupportedValue.into()),
            }
        }
    }
}
enums!(foreground, read_foreground, Foreground, w::ApplicationForeground,
    [Hidden => Hidden, Windowed => Windowed, Fullscreen => Fullscreen, OrdinaryUnknown => OrdinaryUnknown, Unknown => Unclassified, Unavailable => Unavailable]);
enums!(park_result, read_park_result, ParkResult, w::ApplicationParkResult,
    [Confirmed => Confirmed, FullscreenExempt => FullscreenExempt, UnknownExempt => UnknownExempt, Denied => Denied, Stale => Stale, Unavailable => Unavailable]);
enums!(phase, read_phase, a::Phase, w::ApplicationFocusPhase, [Off => Off, Preparing => Preparing, Active => Active, Suspended => Suspended]);
enums!(reason, read_reason, a::Reason, w::ApplicationFocusReason,
    [Disabled => Disabled, Ready => Ready, NoPointer => NoPointer, PointerNotParticipating => PointerNotParticipating, AwaitingApplication => AwaitingApplication, DesktopUnavailable => DesktopUnavailable, Drag => Drag, Game => Game, CounterExhausted => CounterExhausted]);

fn claim(value: &a::Claim) -> v1::Result<w::ApplicationFocusClaim> {
    w::ApplicationFocusClaim::from_raw(
        [
            (1, value.epoch.write()),
            (2, boot(value.pointer).write()),
            (3, super::control::scope(Some(&value.scope))?.write()),
        ]
        .into(),
    )
}
fn read_claim(value: w::ApplicationFocusClaim) -> v1::Result<a::Claim> {
    Ok(a::Claim {
        epoch: value.epoch()?,
        pointer: read_boot(&value.pointer()?)?,
        scope: super::control::read_scope(value.scope()?)?,
    })
}
fn prepare(value: &a::ParkRequest) -> v1::Result<w::ApplicationFocusPrepare> {
    w::ApplicationFocusPrepare::from_raw(
        [
            (1, value.revision.write()),
            (2, value.operation.write()),
            (3, boot(value.target).write()),
            (4, value.expected.generation.write()),
            (5, value.expected.sequence.write()),
        ]
        .into(),
    )
}
fn read_prepare(claim: &a::Claim, value: w::ApplicationFocusPrepare) -> v1::Result<a::ParkRequest> {
    Ok(a::ParkRequest {
        claim: claim.clone(),
        revision: value.revision()?,
        operation: value.operation()?,
        target: read_boot(&value.target()?)?,
        expected: ObservationStamp {
            generation: value.generation()?,
            sequence: value.sequence()?,
        },
    })
}
fn snapshot(value: &a::Snapshot) -> v1::Result<w::ApplicationFocusSnapshot> {
    w::ApplicationFocusSnapshot::from_raw(
        [
            (1, value.revision.write()),
            (2, value.keyboard.map(boot).write()),
            (3, phase(value.phase).write()),
            (4, reason(value.reason).write()),
            (
                5,
                List::<_, 16>(value.participants.iter().copied().map(boot).collect()).write(),
            ),
        ]
        .into(),
    )
}
fn read_snapshot(claim: &a::Claim, value: w::ApplicationFocusSnapshot) -> v1::Result<a::Snapshot> {
    let participants = value
        .participants()?
        .0
        .iter()
        .map(read_boot)
        .collect::<v1::Result<Vec<_>>>()?;
    let mut seen = std::collections::HashSet::new();
    if participants.iter().any(|boot| !seen.insert(boot.device_id)) {
        return Err(v1::ErrorCode::MalformedFrame.into());
    }
    Ok(a::Snapshot {
        claim: claim.clone(),
        revision: value.revision()?,
        keyboard: value.keyboard()?.as_ref().map(read_boot).transpose()?,
        phase: read_phase(value.phase()?)?,
        reason: read_reason(value.reason()?)?,
        participants,
    })
}

pub(super) fn encode(value: &a::Message) -> v1::Result<v1::Message> {
    use a::Action as A;
    use w::ApplicationFocusActionKind as K;
    // Redundant domain claims are deliberate conveniences for model effects;
    // refuse mismatches instead of silently relabelling a queued operation.
    let nested_claim = match &value.action {
        A::Prepare(r) | A::Cancel(r) | A::Applied { request: r, .. } => Some(&r.claim),
        A::Commit(s) | A::Snapshot(s) => Some(&s.claim),
        A::Retire(r) => Some(&r.claim),
        _ => None,
    };
    if nested_claim.is_some_and(|claim| *claim != value.claim) {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    let (kind, body): (_, RawMap) = match &value.action {
        A::Observe(o) => (
            K::Observe,
            w::ApplicationFocusObservation::from_raw(
                [
                    (1, o.stamp.generation.write()),
                    (2, o.stamp.sequence.write()),
                    (3, foreground(o.foreground).write()),
                    (4, o.hidden_lost.write()),
                    (5, o.park_operation.write()),
                ]
                .into(),
            )?
            .raw()
            .clone(),
        ),
        A::Prepare(r) => (K::Prepare, prepare(r)?.raw().clone()),
        A::Cancel(r) => (K::Cancel, prepare(r)?.raw().clone()),
        A::Commit(s) => (K::Commit, snapshot(s)?.raw().clone()),
        A::Snapshot(s) => (K::Snapshot, snapshot(s)?.raw().clone()),
        A::Applied { request, result } => (
            K::Applied,
            w::ApplicationFocusApplied::from_raw(
                [(1, prepare(request)?.write()), (2, park_result(*result).write())].into(),
            )?
            .raw()
            .clone(),
        ),
        A::Retire(r) => (
            K::Retire,
            w::ApplicationFocusRetire::from_raw([(1, r.revision.write()), (2, boot(r.destination).write())].into())?
                .raw()
                .clone(),
        ),
        A::Sample => (K::Sample, RawMap::new()),
    };
    let action = w::ApplicationFocusAction::from_raw([(1, kind.write()), (2, RawValue::Map(body))].into())?;
    Ok(v1::Message::ApplicationFocus(w::ApplicationFocusRecord::from_raw(
        [(1, claim(&value.claim)?.write()), (2, action.write())].into(),
    )?))
}

pub(super) fn decode(value: w::ApplicationFocusRecord) -> v1::Result<a::Message> {
    use a::Action as A;
    use w::ApplicationFocusActionKind as K;
    let claim = read_claim(value.claim()?)?;
    let envelope = value.action()?;
    let Open::Known(kind) = envelope.variant()? else {
        return Err(v1::ErrorCode::UnsupportedValue.into());
    };
    let body = envelope.body()?;
    let action = match kind {
        K::Observe => {
            let o = w::ApplicationFocusObservation::from_raw(body)?;
            A::Observe(Observation {
                stamp: ObservationStamp {
                    generation: o.generation()?,
                    sequence: o.sequence()?,
                },
                foreground: read_foreground(o.foreground()?)?,
                hidden_lost: o.hidden_lost()?,
                park_operation: o.park_operation()?,
            })
        }
        K::Prepare => A::Prepare(read_prepare(&claim, w::ApplicationFocusPrepare::from_raw(body)?)?),
        K::Cancel => A::Cancel(read_prepare(&claim, w::ApplicationFocusPrepare::from_raw(body)?)?),
        K::Commit => A::Commit(read_snapshot(&claim, w::ApplicationFocusSnapshot::from_raw(body)?)?),
        K::Snapshot => A::Snapshot(read_snapshot(&claim, w::ApplicationFocusSnapshot::from_raw(body)?)?),
        K::Applied => {
            let a = w::ApplicationFocusApplied::from_raw(body)?;
            A::Applied {
                request: read_prepare(&claim, a.request()?)?,
                result: read_park_result(a.result()?)?,
            }
        }
        K::Retire => {
            let r = w::ApplicationFocusRetire::from_raw(body)?;
            A::Retire(a::Route {
                claim: claim.clone(),
                revision: r.revision()?,
                destination: read_boot(&r.destination()?)?,
            })
        }
        K::Sample => A::Sample,
        _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
    };
    Ok(a::Message { claim, action })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::{DeviceId, session::Boot};

    fn identity(n: u8) -> Boot {
        Boot {
            device_id: DeviceId([n; 32]),
            nonce: [n; 16],
        }
    }
    #[test]
    fn every_control_action_round_trips_exact_claim_and_observation() {
        let claim = a::Claim {
            epoch: 9,
            pointer: identity(1),
            scope: oink_routing::control::FocusScope {
                members: vec![(identity(1).device_id, [1; 16]), (identity(2).device_id, [2; 16])],
            },
        };
        let request = a::ParkRequest {
            claim: claim.clone(),
            revision: 3,
            operation: 17,
            target: identity(2),
            expected: ObservationStamp {
                generation: 4,
                sequence: 5,
            },
        };
        let snapshot = a::Snapshot {
            claim: claim.clone(),
            revision: 3,
            keyboard: Some(identity(2)),
            phase: a::Phase::Active,
            reason: a::Reason::Ready,
            participants: vec![identity(1), identity(2)],
        };
        let actions = [
            a::Action::Observe(Observation {
                stamp: request.expected,
                foreground: Foreground::OrdinaryUnknown,
                hidden_lost: true,
                park_operation: Some(17),
            }),
            a::Action::Prepare(request.clone()),
            a::Action::Cancel(request.clone()),
            a::Action::Applied {
                request,
                result: ParkResult::Stale,
            },
            a::Action::Commit(snapshot.clone()),
            a::Action::Snapshot(snapshot),
            a::Action::Retire(a::Route {
                claim: claim.clone(),
                revision: 3,
                destination: identity(2),
            }),
            a::Action::Sample,
        ];
        for action in actions {
            let original = a::Message {
                claim: claim.clone(),
                action,
            };
            let bytes = v1::frame::encode(&encode(&original).unwrap(), v1::limits::RELIABLE_FRAME).unwrap();
            let v1::Message::ApplicationFocus(record) =
                v1::frame::OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME)
                    .unwrap()
                    .decode()
                    .unwrap()
            else {
                panic!()
            };
            assert_eq!(decode(record).unwrap(), original);
        }
    }

    #[test]
    fn future_action_is_not_executed_as_a_known_action() {
        let claim = w::ApplicationFocusClaim::from_raw(
            [
                (1, 1u64.write()),
                (2, boot(identity(1)).write()),
                (3, super::super::control::scope(None).unwrap().write()),
            ]
            .into(),
        )
        .unwrap();
        let action =
            w::ApplicationFocusAction::from_raw([(1, 999u32.write()), (2, RawValue::Map(RawMap::new()))].into())
                .unwrap();
        let record = w::ApplicationFocusRecord::from_raw([(1, claim.write()), (2, action.write())].into()).unwrap();
        assert_eq!(decode(record).unwrap_err().code, v1::ErrorCode::UnsupportedValue);
    }
}
