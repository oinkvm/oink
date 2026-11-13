use super::*;
use oink_mobile_core::MobileNetworkError;
use oink_network::{discovery::Advert, link::AddressProbeError, pairing as core};
use oink_protocol::v2::dto::{self, PairCode, PairStage};
use std::{
    net::{IpAddr, SocketAddr},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(super) struct PreparedAddress {
    pub id: String,
    pub generation: u64,
    peer: Option<DeviceId>,
    expires: Instant,
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PREPARED_TTL: Duration = Duration::from_secs(30);

fn decode_device(value: &str) -> Option<DeviceId> {
    if value.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(value.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(DeviceId(bytes))
}

pub(super) fn problem(code: PairCode, stage: PairStage, detail: impl Into<String>) -> PairProblem {
    PairProblem {
        code: code.number(),
        stage: stage.number(),
        detail: detail.into(),
        network: None,
    }
}

pub(super) fn network_problem(error: MobileNetworkError) -> PairProblem {
    let code = match &error {
        MobileNetworkError::Pairing(value) => return terminal(value),
        MobileNetworkError::NotReady(_) | MobileNetworkError::Dial(_) => PairCode::Unreachable,
        MobileNetworkError::Unauthorized(_) => PairCode::PermissionDenied,
        _ => PairCode::StateChanged,
    };
    problem(code, PairStage::Request, error.to_string())
}

pub(super) fn accepted(request: String, result: Result<(), MobileNetworkError>) -> PairAcceptedReply {
    match result {
        Ok(()) => PairAcceptedReply::Value { v1: request },
        Err(error) => PairAcceptedReply::Problem {
            v1: PairRejection::Problem {
                problem: network_problem(error),
            },
        },
    }
}

fn terminal(value: &dto::PairProblem) -> PairProblem {
    let code = value.code().unwrap_or(PairCode::ProtocolViolation);
    let stage = value.stage().unwrap_or(PairStage::Request);
    problem(code, stage, format!("{code:?}"))
}

/// The failure to report for an ended session, if the user should be told.
/// Silent: this device's own action (a local cancel, a local decline of an
/// incoming request including the dismissal by a saved block), and any end of
/// an incoming request the user never accepted: it only leaves the request
/// list (user 2026-11-11, e.g. the peer cancelled it). The initiator's session
/// ends with the peer's `Declined` and reports it at once.
fn reported_failure(view: &core::View) -> Option<PairProblem> {
    let value = view.terminal.as_ref()?;
    if view.direction == core::Direction::Incoming && !view.accepted {
        return None;
    }
    match (value.code().ok()?, view.direction) {
        (PairCode::CancelledLocally, _) | (PairCode::Declined, core::Direction::Incoming) => None,
        _ => Some(terminal(value)),
    }
}

fn peer_view(network: &MobileNetwork, device: DeviceId) -> PeerView {
    let advert = network.authenticated_advert(device);
    PeerView {
        device: device.to_hex(),
        name: advert
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_else(|| device.short()),
        os: advert.map(|a| a.os).unwrap_or_default(),
    }
}

fn group_ref(value: oink_protocol::v2::Result<oink_protocol::v2::schema::FixedBytes<16>>) -> GroupRef {
    let id = value
        .map(|id| oink_foundation::GroupId(id.0).to_hex())
        .unwrap_or_default();
    GroupRef {
        id,
        name: String::new(),
    }
}

fn phase(value: core::Phase) -> u32 {
    match value {
        core::Phase::Requested => 0,
        core::Phase::ShowCode => 1,
        core::Phase::EnterCode => 2,
        core::Phase::CheckingCode => 3,
        core::Phase::Preparing => 4,
        core::Phase::Committing => 5,
        core::Phase::Admitting => 6,
        core::Phase::Completed => 7,
        core::Phase::Ended => 8,
    }
}

impl MobileSession {
    pub(super) fn pairing_view(&self) -> Result<PairingSnapshot, MobileError> {
        let snapshot = self.network.pairing_snapshot().map_err(worker)?;
        let requests = snapshot
            .requests
            .into_iter()
            .map(|view| PendingRequest {
                id: core::key(view.id),
                revision: view.revision,
                peer: peer_view(&self.network, view.peer.device_id),
                source: group_ref(view.context.source_group()),
                target: group_ref(view.context.target_group()),
                source_count: view.context.source_count().unwrap_or_default(),
                target_count: view.context.target_count().unwrap_or_default(),
            })
            .collect();
        let sessions = snapshot
            .sessions
            .into_iter()
            .map(|view| {
                let peer = peer_view(&self.network, view.peer.device_id);
                PairView {
                    id: core::key(view.id),
                    revision: view.revision,
                    direction: match view.direction {
                        core::Direction::Incoming => Direction::Incoming,
                        core::Direction::Outgoing => Direction::Outgoing,
                    },
                    peer: PairEndpoint {
                        device: peer.device,
                        name: peer.name,
                        os: peer.os,
                    },
                    phase: phase(view.phase),
                    attempts_left: view.attempts_left,
                    feedback: match view.feedback {
                        dto::PairFeedbackKind::WrongCode => Feedback::Wrong,
                        dto::PairFeedbackKind::Verified => Feedback::Correct,
                        _ => Feedback::Waiting,
                    },
                    terminal: view.terminal.as_ref().map(terminal),
                }
            })
            .collect();
        Ok(PairingSnapshot {
            revision: snapshot.revision,
            requests,
            sessions,
        })
    }

    /// Cancel after the merge commit is not a failure: the pairing succeeded.
    pub(super) fn committed_pairing(&self, id: &str) -> Option<CancelResult> {
        let snapshot = self.network.pairing_snapshot().ok()?;
        let view = snapshot.sessions.into_iter().find(|view| core::key(view.id) == id)?;
        match view.phase {
            core::Phase::Completed => Some(CancelResult::AlreadyCommitted),
            core::Phase::Committing | core::Phase::Admitting => Some(CancelResult::CommitUnconfirmed),
            _ => None,
        }
    }

    /// Terminal pairing events after `after` (and the acknowledged id):
    /// failures, and successful completions (`PairingCompleted`, no undo).
    pub(super) fn pairing_errors(&self, after: u64) -> Result<Vec<PairingEvent>, MobileError> {
        let acknowledged = self.acknowledged_pairing_event.load(Ordering::Relaxed);
        let mut events: Vec<_> = self
            .network
            .pairing_snapshot()
            .map_err(worker)?
            .sessions
            .into_iter()
            .filter_map(|view| {
                if view.revision <= after.max(acknowledged) {
                    return None;
                }
                if view.phase == core::Phase::Completed && view.terminal.is_none() {
                    return Some((
                        view.revision,
                        PairingEvent::PairingCompleted {
                            event_id: view.revision,
                            peer: peer_view(&self.network, view.peer.device_id),
                        },
                    ));
                }
                let problem = reported_failure(&view)?;
                Some((
                    view.revision,
                    PairingEvent::PairingFailed {
                        event_id: view.revision,
                        problem,
                    },
                ))
            })
            .collect();
        events.sort_by_key(|(id, _)| *id);
        Ok(events.into_iter().map(|(_, event)| event).collect())
    }

    pub(super) async fn begin_pairing(&self, target: PairTarget) -> Result<String, PairProblem> {
        let peer = match target {
            PairTarget::NearbyDevice { device } => {
                let peer = decode_device(&device)
                    .ok_or_else(|| problem(PairCode::InvalidAddress, PairStage::Address, "invalid device identity"))?;
                if self.network.authenticated_advert(peer).is_none() {
                    let candidates = self
                        .nearby_addresses(peer)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            problem(
                                PairCode::Unreachable,
                                PairStage::Connect,
                                "no current address for nearby device",
                            )
                        })?;
                    // A native button can race an inbound authenticated link.
                    // Prefer the completed C0 result over a duplicate-dial error.
                    let _ = self.network.dial(peer, candidates);
                    self.await_advert(peer).await?;
                }
                peer
            }
            PairTarget::PreparedQuery { query_id, generation } => {
                let mut prepared = self.prepared_pairing.lock().unwrap();
                let peer = prepared
                    .as_ref()
                    .filter(|value| {
                        value.id == query_id && value.generation == generation && value.expires > Instant::now()
                    })
                    .and_then(|value| value.peer)
                    .ok_or_else(|| problem(PairCode::PreparedPeerLost, PairStage::Request, "address query expired"))?;
                prepared.take();
                peer
            }
        };
        self.network.start_pairing(peer).map_err(network_problem)
    }

    async fn await_advert(&self, peer: DeviceId) -> Result<Advert, PairProblem> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            loop {
                if let Some(advert) = self.network.authenticated_advert(peer) {
                    return advert;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| {
            let mut issue = problem(
                PairCode::ConnectTimeout,
                PairStage::Negotiate,
                "timed out waiting for the authenticated greeting",
            );
            issue.network = Some(AddressNetworkProblem::Timeout);
            issue
        })
    }

    pub(super) async fn query_address(&self, id: String, generation: u64, address: String, port: Option<u16>) {
        *self.prepared_pairing.lock().unwrap() = Some(PreparedAddress {
            id: id.clone(),
            generation,
            peer: None,
            expires: Instant::now() + PREPARED_TTL,
        });
        let result = self.probe_pairing_address(address, port).await;
        let event = {
            let mut state = self.prepared_pairing.lock().unwrap();
            let Some(prepared) = state
                .as_mut()
                .filter(|value| value.id == id && value.generation == generation)
            else {
                return;
            };
            match result {
                Ok(advert) => {
                    prepared.peer = Some(advert.id);
                    prepared.expires = Instant::now() + PREPARED_TTL;
                    AddressQueryResult::Found {
                        peer: FoundPeer {
                            query_id: id.clone(),
                            generation,
                            peer: PeerView {
                                device: advert.id.to_hex(),
                                name: advert.name,
                                os: advert.os,
                            },
                            prepared: true,
                        },
                    }
                }
                Err(problem) => AddressQueryResult::Problem { problem },
            }
        };
        self.emit_event(MobileEvent::Pairing {
            event: PairingEvent::AddressQueryFinished {
                operation_id: id,
                generation,
                result: event,
            },
        });
    }

    async fn probe_pairing_address(&self, address: String, port: Option<u16>) -> Result<Advert, PairProblem> {
        let ip = address.parse::<IpAddr>().map_err(|_| {
            problem(
                PairCode::InvalidAddress,
                PairStage::Address,
                "numeric IPv4 address required",
            )
        })?;
        let address = SocketAddr::new(ip, port.unwrap_or(47470));
        // A v1 (0.3.x) device fails the v2 ALPN/handshake. Discovery still
        // sees its v1 advert at this endpoint, which identifies the cause.
        let incompatible = || {
            let mut issue = problem(
                PairCode::UnsupportedProtocol,
                PairStage::Negotiate,
                "peer requires a compatible Oink version",
            );
            issue.network = Some(AddressNetworkProblem::Incompatible);
            issue
        };
        let peer = match self.network.probe_address(address).await {
            Ok(peer) => peer,
            Err(error)
                if !matches!(error, AddressProbeError::InvalidAddress | AddressProbeError::SelfDevice)
                    && self.incompatible_endpoint(address) =>
            {
                return Err(incompatible());
            }
            Err(error) => {
                let (code, reason) = probe_problem(error);
                let mut issue = problem(code, PairStage::Address, error.to_string());
                issue.network = Some(reason);
                return Err(issue);
            }
        };
        if self.nearby_incompatible(peer) {
            return Err(incompatible());
        }
        self.await_advert(peer).await
    }
}

pub(super) fn probe_problem(error: AddressProbeError) -> (PairCode, AddressNetworkProblem) {
    match error {
        AddressProbeError::InvalidAddress => (PairCode::InvalidAddress, AddressNetworkProblem::InvalidAddress),
        AddressProbeError::Timeout => (PairCode::ConnectTimeout, AddressNetworkProblem::Timeout),
        AddressProbeError::Refused => (PairCode::ConnectionRefused, AddressNetworkProblem::Refused),
        AddressProbeError::SelfDevice => (PairCode::SelfDevice, AddressNetworkProblem::SelfDevice),
        AddressProbeError::Authentication => (PairCode::NotOink, AddressNetworkProblem::NotOink),
        AddressProbeError::Busy => (PairCode::QueueFull, AddressNetworkProblem::Busy),
        AddressProbeError::NoRoute | AddressProbeError::NoNetworkInterface | AddressProbeError::Stopped => {
            (PairCode::Unreachable, AddressNetworkProblem::NoNetwork)
        }
        AddressProbeError::Unreachable | AddressProbeError::Socket => {
            (PairCode::Unreachable, AddressNetworkProblem::Unreachable)
        }
    }
}

fn worker(error: MobileNetworkError) -> MobileError {
    MobileError::Worker {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::session::{Boot, PairId};
    use oink_network::wire_identity as wire;
    use oink_protocol::v2::{
        Message, RawValue,
        schema::{WireDto, WireValue},
    };

    fn boot(n: u8) -> Boot {
        Boot {
            device_id: DeviceId([n; 32]),
            nonce: [n; 16],
        }
    }

    fn transcript() -> core::Transcript {
        core::Transcript {
            hello: vec![0, 13, 1, 0, 0, 0, 0xa0],
            info: vec![1, 13, 1, 0, 0, 0, 0xa0],
        }
    }

    fn context(initiator: Boot, receiver: Boot) -> dto::PairContext {
        let id = PairId {
            initiator_boot: initiator,
            random: [1; 16],
        };
        dto::PairContext::from_raw(
            [
                (1, wire::pair(id).write()),
                (2, wire::boot(initiator).write()),
                (3, wire::boot(receiver).write()),
                (4, RawValue::Bytes(vec![1; 16])),
                (5, RawValue::Bytes(vec![2; 16])),
                (6, 1u8.write()),
                (7, 1u8.write()),
                (8, RawValue::Bytes(vec![2; 16])),
                (9, RawValue::Bytes(vec![3; 32])),
                (10, RawValue::from(dto::PAIRING_SUITE)),
                (11, RawValue::Bytes(vec![4; 32])),
                (12, RawValue::Bytes(vec![5; 32])),
            ]
            .into(),
        )
        .unwrap()
    }

    fn sent(effects: Vec<core::Effect>) -> Message {
        effects
            .into_iter()
            .find_map(|effect| match effect {
                core::Effect::Send { message, .. } => Some(message),
                _ => None,
            })
            .unwrap()
    }

    fn failures(sessions: &core::Sessions) -> Vec<PairProblem> {
        sessions
            .snapshot()
            .sessions
            .iter()
            .filter_map(reported_failure)
            .collect()
    }

    /// The initiator sends a request; the receiver queues it, then declines it.
    fn declined_pairing() -> (core::Sessions, core::Sessions) {
        let now = Instant::now();
        let mut initiator = core::Sessions::new(boot(1));
        let mut receiver = core::Sessions::new(boot(2));
        let Message::ConfigPairRequest(request) = sent(
            initiator
                .start(context(boot(1), boot(2)), boot(2), transcript())
                .unwrap(),
        ) else {
            panic!("pair request");
        };
        let queued = sent(
            receiver
                .request(request, boot(1), transcript(), core::Admission::Open, now)
                .unwrap(),
        );
        initiator.receive(boot(2), queued, now).unwrap();
        let pending = receiver.snapshot().requests.remove(0);
        let declined = sent(receiver.decline(pending.id, pending.revision).unwrap());
        initiator.receive(boot(2), declined, now).unwrap();
        (initiator, receiver)
    }

    #[test]
    fn initiator_reports_the_peer_decline() {
        let (initiator, _) = declined_pairing();
        let reported = failures(&initiator);
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert_eq!(reported[0].code, PairCode::Declined.number());
        assert_eq!(reported[0].stage, PairStage::Request.number());
    }

    #[test]
    fn local_decline_is_not_a_pairing_failure() {
        let (_, receiver) = declined_pairing();
        assert!(
            receiver
                .snapshot()
                .sessions
                .iter()
                .any(|view| view.terminal.as_ref().and_then(|p| p.code().ok()) == Some(PairCode::Declined)),
            "the declined request stays as an ended session view"
        );
        assert!(failures(&receiver).is_empty());
    }

    /// The initiator sends a request the receiver lists; `accept` decides whether the
    /// receiver's user takes it before the initiator cancels.
    fn cancelled_by_initiator(accept: bool) -> core::Sessions {
        let now = Instant::now();
        let mut initiator = core::Sessions::new(boot(1));
        let mut receiver = core::Sessions::new(boot(2));
        let Message::ConfigPairRequest(request) = sent(
            initiator
                .start(context(boot(1), boot(2)), boot(2), transcript())
                .unwrap(),
        ) else {
            panic!("pair request");
        };
        let queued = sent(
            receiver
                .request(request, boot(1), transcript(), core::Admission::Open, now)
                .unwrap(),
        );
        initiator.receive(boot(2), queued, now).unwrap();
        let pending = receiver.snapshot().requests.remove(0);
        if accept {
            let accepted = sent(receiver.accept(pending.id, pending.revision).unwrap());
            initiator.receive(boot(2), accepted, now).unwrap();
        }
        let view = initiator.view(pending.id).unwrap().clone();
        let cancel = sent(initiator.cancel(pending.id, view.revision).unwrap());
        receiver.receive(boot(1), cancel, now).unwrap();
        assert!(receiver.snapshot().requests.is_empty());
        receiver
    }

    /// User 2026-11-11: a request the peer cancels before anyone accepted it just leaves
    /// the list; nothing is reported, so neither app shows a failure.
    #[test]
    fn a_request_cancelled_before_it_was_accepted_is_not_reported() {
        let receiver = cancelled_by_initiator(false);
        assert!(failures(&receiver).is_empty());
    }

    /// A pairing the user accepted and the peer then cancels still reports why, for the
    /// open pairing sheet.
    #[test]
    fn an_accepted_pairing_cancelled_by_the_peer_is_reported() {
        let receiver = cancelled_by_initiator(true);
        let reported = failures(&receiver);
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert_eq!(reported[0].code, PairCode::CancelledByPeer.number());
    }

    #[test]
    fn manual_add_failures_keep_their_typed_reason() {
        use AddressNetworkProblem as P;
        for (error, code, reason) in [
            (AddressProbeError::Authentication, PairCode::NotOink, P::NotOink),
            (AddressProbeError::Timeout, PairCode::ConnectTimeout, P::Timeout),
            (AddressProbeError::Refused, PairCode::ConnectionRefused, P::Refused),
            (AddressProbeError::Unreachable, PairCode::Unreachable, P::Unreachable),
            (AddressProbeError::NoRoute, PairCode::Unreachable, P::NoNetwork),
            (AddressProbeError::SelfDevice, PairCode::SelfDevice, P::SelfDevice),
            (AddressProbeError::Busy, PairCode::QueueFull, P::Busy),
        ] {
            assert_eq!(probe_problem(error), (code, reason), "{error:?}");
        }
    }
}
