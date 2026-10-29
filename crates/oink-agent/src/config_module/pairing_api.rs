use super::*;
use crate::device_cards::{BlockEdited, BlockListError, BlockReply, BlockedDevice, BlockedDevicesSnapshot};
use oink_network::{group_merge, pair_merge, roster::Anchor};
use serde::{Serialize, de::DeserializeOwned};
fn failure(code: dto::PairCode, stage: dto::PairStage) -> api::PairProblem {
    api::PairProblem::try_from(&pairing::problem(code, stage)).expect("fixed problem")
}
fn reject<T>(problem: api::PairProblem) -> api::PairReply<T> {
    api::PairReply::Problem(api::PairRejection::Problem { problem })
}
fn wire_reject<T>(problem: dto::PairProblem) -> api::PairReply<T> {
    reject(api::PairProblem::try_from(&problem).expect("validated problem"))
}
fn reference(anchor: Anchor) -> api::GroupRef {
    api::GroupRef {
        group: anchor.group.into(),
        root: DeviceId(anchor.root).to_hex(),
    }
}
fn endpoint(address: SocketAddr, expected: Option<DeviceId>, verified: Option<api::PeerView>) -> api::PairEndpoint {
    api::PairEndpoint {
        address: address.into(),
        expected_id: expected.map(Into::into),
        verified,
    }
}
fn phase(value: pairing::Phase) -> u32 {
    match value {
        pairing::Phase::Requested | pairing::Phase::ShowCode => 1,
        pairing::Phase::EnterCode => 2,
        pairing::Phase::CheckingCode => 3,
        pairing::Phase::Preparing => 4,
        pairing::Phase::Committing => 5,
        pairing::Phase::Admitting => 6,
        pairing::Phase::Completed => 7,
        pairing::Phase::Ended => 8,
    }
}
impl Owner {
    pub(crate) fn onboarding_pairing_busy(&self) -> bool {
        let snapshot = self.pairs.snapshot();
        self.onboarding_connect
            && (!self.connecting.is_empty()
                || !snapshot.requests.is_empty()
                || snapshot
                    .sessions
                    .iter()
                    .any(|s| !matches!(s.phase, pairing::Phase::Completed | pairing::Phase::Ended)))
    }
    pub(crate) fn set_onboarding_connect(&mut self, active: bool) {
        self.onboarding_connect = active;
        if !active {
            return;
        }
        // Requests can have arrived during earlier welcome pages. Keep one only
        // when no accepted/outgoing session already owns the connect screen.
        let snapshot = self.pairs.snapshot();
        let busy = !self.connecting.is_empty()
            || snapshot
                .sessions
                .iter()
                .any(|s| !matches!(s.phase, pairing::Phase::Completed | pairing::Phase::Ended));
        for request in snapshot.requests.into_iter().skip(usize::from(!busy)) {
            if let Ok(effects) = self.pairs.decline(request.id, request.revision) {
                self.pair_effects(effects);
            }
        }
    }
    fn cached<T: DeserializeOwned>(&self, id: &str, key: &str) -> Result<Option<T>, api::PairProblem> {
        if id.is_empty() || id.len() > 256 {
            return Err(failure(dto::PairCode::StateChanged, dto::PairStage::Request));
        }
        match self.operations.iter().find(|(old, _, _)| old == id) {
            None => Ok(None),
            Some((_, prior, value)) if prior == key => serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|_| failure(dto::PairCode::ProtocolViolation, dto::PairStage::Request)),
            Some(_) => Err(failure(dto::PairCode::StateChanged, dto::PairStage::Request)),
        }
    }
    fn remember<T: Serialize>(&mut self, id: String, key: String, value: &T) {
        if self.operations.len() >= 256 {
            self.operations.pop_front();
        }
        if let Ok(value) = serde_json::to_value(value) {
            self.operations.push_back((id, key, value));
        }
    }
    fn peer_view(&self, device: DeviceId) -> Option<api::PeerView> {
        let (_, link) = self.selected_session(device)?;
        let greeting = &link.peer.as_ref()?.greeting;
        let advert = greeting.advert().ok()?;
        let remote = link.remote?;
        let listen = SocketAddr::new(remote.ip(), advert.port().ok()?);
        Some(api::PeerView {
            device: device.into(),
            name: advert.name().ok()?.0,
            os: greeting.os().ok()?.0,
            remote: remote.into(),
            listen: Some(listen.into()),
        })
    }
    fn pair_view(&self, view: &pairing::View) -> Option<api::PairView> {
        let endpoint = self.endpoints.get(&view.id).cloned().or_else(|| {
            let peer = self.peer_view(view.peer.device_id)?;
            let address = peer.remote.clone();
            Some(api::PairEndpoint {
                address,
                expected_id: Some(view.peer.device_id.into()),
                verified: Some(peer),
            })
        })?;
        Some(api::PairView {
            id: pairing::key(view.id),
            revision: DecimalU64(view.revision),
            direction: if view.direction == pairing::Direction::Incoming {
                api::Direction::Incoming
            } else {
                api::Direction::Outgoing
            },
            peer: endpoint,
            phase: phase(view.phase),
            attempts_left: view.attempts_left,
            feedback: match view.feedback {
                dto::PairFeedbackKind::Verified => api::Feedback::Correct,
                dto::PairFeedbackKind::WrongCode => api::Feedback::Wrong,
                _ => api::Feedback::Waiting,
            },
            terminal: view.terminal.as_ref().and_then(|p| api::PairProblem::try_from(p).ok()),
        })
    }
    pub(crate) fn publish_pairings(&mut self) {
        let current = self.pairs.snapshot();
        for view in current.requests.iter().chain(&current.sessions) {
            if !self.endpoints.contains_key(&view.id)
                && let Some(peer) = self.peer_view(view.peer.device_id)
            {
                self.endpoints.insert(
                    view.id,
                    api::PairEndpoint {
                        address: peer.remote.clone(),
                        expected_id: Some(view.peer.device_id.into()),
                        verified: Some(peer),
                    },
                );
            }
        }
        let requests = current
            .requests
            .iter()
            .filter_map(|v| {
                Some(api::PendingRequest {
                    id: pairing::key(v.id),
                    revision: DecimalU64(v.revision),
                    peer: self.endpoints.get(&v.id)?.verified.clone()?,
                    source: api::GroupRef {
                        group: GroupId(v.context.source_group().ok()?.0).into(),
                        root: DeviceId(v.context.source_root_id().ok()?.0).to_hex(),
                    },
                    target: api::GroupRef {
                        group: GroupId(v.context.target_group().ok()?.0).into(),
                        root: DeviceId(v.context.target_root_id().ok()?.0).to_hex(),
                    },
                    source_count: v.context.source_count().ok()?,
                    target_count: v.context.target_count().ok()?,
                })
            })
            .collect::<Vec<_>>();
        let mut sessions = current
            .sessions
            .iter()
            .filter_map(|v| self.pair_view(v))
            .collect::<Vec<_>>();
        sessions.extend(self.connecting.values().map(|v| api::PairView {
            id: pairing::key(v.id),
            revision: DecimalU64(v.revision),
            direction: api::Direction::Outgoing,
            peer: v.endpoint.clone(),
            phase: 0,
            attempts_left: 3,
            feedback: api::Feedback::Waiting,
            terminal: None,
        }));
        sessions.extend(self.local_terminal.iter().cloned());
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        let previous = self.shared.pairings.load();
        let revision = previous.revision.0.saturating_add(1).max(current.revision);
        let snapshot = api::PairingSnapshot {
            revision: DecimalU64(revision),
            requests: requests.clone(),
            sessions: sessions.clone(),
        };
        self.shared.pairings.publish(snapshot);
        if previous.requests != requests {
            self.shared.emit_pairing(api::PairingEvent::PairingRequestsChanged {
                revision: DecimalU64(revision),
                requests,
            });
        }
        for view in sessions {
            if !previous.sessions.iter().any(|old| old == &view) {
                self.shared.emit_pairing(api::PairingEvent::PairingChanged {
                    revision: DecimalU64(revision),
                    view,
                });
            }
        }
    }
    pub(crate) fn publish_nearby(&mut self) {
        let now = Instant::now();
        self.nearby
            .retain(|_, seen| now.duration_since(seen.at) < Duration::from_secs(90));
        let rows = self
            .nearby
            .values()
            .filter(|seen| !self.state.roster().contains(seen.sighting.advert.id))
            .filter(|seen| {
                self.blocks
                    .as_ref()
                    .is_some_and(|b| !b.contains(seen.sighting.advert.id))
            })
            .filter_map(|seen| {
                let advert = &seen.sighting.advert;
                let addresses = seen
                    .sighting
                    .addrs
                    .iter()
                    .copied()
                    .filter(|a| a.is_ipv4())
                    .collect::<Vec<_>>();
                let selected = *addresses.first()?;
                let compatible = advert.protocols.iter().any(|p| p.major == v1::MAJOR)
                    && advert.identity_algorithms.iter().any(|a| a == dto::ED25519);
                Some(api::NearbyRow {
                    device: advert.id.into(),
                    name: advert.name.clone(),
                    os: advert.os.clone(),
                    group: api::DiscoveredGroupRef {
                        group: advert.group.into(),
                        root: None,
                    },
                    count: compatible.then_some(advert.count),
                    addresses: addresses.into_iter().map(Into::into).collect(),
                    selected: selected.into(),
                    source: match seen.sighting.source {
                        oink_network::discovery::Source::Beacon => 0,
                        oink_network::discovery::Source::Mdns => 1,
                        oink_network::discovery::Source::Manual => 2,
                    },
                    identity: api::IdentityEvidence::Advertised,
                    reachable: true,
                    compatible,
                    problem: (!compatible)
                        .then(|| failure(dto::PairCode::UnsupportedProtocol, dto::PairStage::Negotiate)),
                })
            })
            .collect::<Vec<_>>();
        let old = self.shared.nearby.load();
        if old.rows == rows {
            return;
        }
        self.nearby_revision += 1;
        let revision = DecimalU64(self.nearby_revision);
        let removed = old
            .rows
            .iter()
            .filter(|r| !rows.iter().any(|n| n.device == r.device))
            .map(|r| r.device)
            .collect();
        let upserts = rows.iter().filter(|r| !old.rows.contains(r)).cloned().collect();
        self.shared.nearby.publish(api::NearbySnapshot { revision, rows });
        self.shared.emit_pairing(api::PairingEvent::NearbyChanged {
            revision,
            upserts,
            removed,
        });
    }
    pub(crate) fn refresh_nearby(&mut self, operation: String) -> ApiResult<api::PairReply<api::Accepted>> {
        match self.cached(&operation, "refresh") {
            Ok(Some(reply)) => return Ok(reply),
            Err(error) => return Ok(reject(error)),
            _ => {}
        }
        let reply = if let Some(discovery) = &self.discovery {
            discovery.refresh();
            api::PairReply::Value(api::Accepted::Accepted)
        } else {
            let mut problem = failure(dto::PairCode::Unreachable, dto::PairStage::Address);
            problem.network_problem = Some(api::AddressNetworkProblem::NetworkStopped);
            reject(problem)
        };
        let result = match &reply {
            api::PairReply::Value(_) => api::TypedResult::Ok,
            api::PairReply::Problem(api::PairRejection::Problem { problem }) => api::TypedResult::Problem {
                problem: problem.clone(),
            },
        };
        self.remember(operation.clone(), "refresh".into(), &reply);
        self.publish_nearby();
        self.shared.emit_pairing(api::PairingEvent::NearbyRefreshFinished {
            operation_id: operation,
            result,
        });
        Ok(reply)
    }
    pub(crate) fn query_address(
        &mut self,
        client: [u8; 16],
        operation: String,
        generation: u64,
        ip: String,
        port: Option<u16>,
    ) -> ApiResult<api::PairReply<api::Accepted>> {
        let operation_key = format!("{:032x}/{operation}", u128::from_le_bytes(client));
        let key = format!("query/{generation}/{ip}/{port:?}");
        match self.cached(&operation_key, &key) {
            Ok(Some(reply)) => return Ok(reply),
            Err(error) => return Ok(reject(error)),
            _ => {}
        }
        let error = if port == Some(0) {
            Some(dto::PairCode::InvalidPort)
        } else {
            None
        };
        let address = ip
            .parse::<std::net::Ipv4Addr>()
            .ok()
            .filter(|a| !a.is_unspecified() && !a.is_multicast() && !a.is_broadcast())
            .map(|a| SocketAddr::new(a.into(), port.unwrap_or(oink_network::NetworkConfig::default().port)));
        if error.is_some() || address.is_none() {
            let reply = reject(failure(
                error.unwrap_or(dto::PairCode::InvalidAddress),
                dto::PairStage::Address,
            ));
            self.remember(operation_key, key, &reply);
            return Ok(reply);
        }
        let address = address.expect("checked");
        if self
            .queries
            .values()
            .filter(|q| !q.released && q.client != client)
            .count()
            >= 16
        {
            let reply = reject(failure(dto::PairCode::QueueFull, dto::PairStage::Address));
            self.remember(operation_key, key, &reply);
            return Ok(reply);
        }
        let replaced_peers = self
            .queries
            .values()
            .filter(|q| q.client == client)
            .filter_map(|q| q.peer)
            .collect::<Vec<_>>();
        self.queries.retain(|_, q| q.client != client);
        // Replacing a client's result releases its prepared-peer reference just
        // like explicit ReleaseQuery. Otherwise an unused nonmember C0 survives
        // the replacement and a fresh query can reuse stale group/count metadata.
        for peer in replaced_peers {
            self.release_prepared_peer(peer);
        }
        let query_id = format!("{:032x}", u128::from_le_bytes(random_id()?));
        self.queries.insert(
            query_id.clone(),
            Query {
                client,
                operation: operation.clone(),
                generation,
                address,
                peer: None,
                boot: None,
                deadline: Some(Instant::now() + oink_network::link::ADDRESS_QUERY_BUDGET),
                released: false,
            },
        );
        let links = self.links.clone();
        let incoming = self.incoming_tx.clone();
        let query = query_id;
        tokio::spawn(async move {
            let result = links.probe_address(address).await;
            let _ = incoming.send(Incoming::Probe {
                operation: query,
                generation,
                result,
            });
        });
        let reply = api::PairReply::Value(api::Accepted::Accepted);
        self.remember(operation_key, key, &reply);
        Ok(reply)
    }
    pub(crate) fn probe_result(
        &mut self,
        operation: String,
        generation: u64,
        result: Result<DeviceId, oink_network::link::AddressProbeError>,
    ) {
        let Some(query) = self
            .queries
            .get_mut(&operation)
            .filter(|q| q.generation == generation && !q.released)
        else {
            if let Ok(peer) = result {
                self.release_prepared_peer(peer);
            }
            return;
        };
        match result {
            Ok(peer) => {
                query.peer = Some(peer);
                self.finish_queries(peer);
            }
            Err(error) => {
                let code = match error {
                    oink_network::link::AddressProbeError::InvalidAddress => dto::PairCode::InvalidAddress,
                    oink_network::link::AddressProbeError::Busy => dto::PairCode::QueueFull,
                    oink_network::link::AddressProbeError::Stopped
                    | oink_network::link::AddressProbeError::NoRoute
                    | oink_network::link::AddressProbeError::NoNetworkInterface
                    | oink_network::link::AddressProbeError::Unreachable
                    | oink_network::link::AddressProbeError::Socket => dto::PairCode::Unreachable,
                    oink_network::link::AddressProbeError::Timeout => dto::PairCode::ConnectTimeout,
                    oink_network::link::AddressProbeError::Refused => dto::PairCode::ConnectionRefused,
                    oink_network::link::AddressProbeError::Authentication => dto::PairCode::NotOink,
                    oink_network::link::AddressProbeError::SelfDevice => dto::PairCode::SelfDevice,
                };
                let mut problem = failure(code, dto::PairStage::Connect);
                use api::AddressNetworkProblem as P;
                use oink_network::link::AddressProbeError as E;
                problem.network_problem = match error {
                    E::NoRoute => Some(P::NoRoute),
                    E::NoNetworkInterface => Some(P::NoNetworkInterface),
                    E::Unreachable => Some(P::PeerUnreachable),
                    E::Socket => Some(P::SocketError),
                    E::Stopped => Some(P::NetworkStopped),
                    _ => None,
                };
                query.deadline = None;
                query.released = true;
                self.shared.emit_pairing(api::PairingEvent::AddressQueryFinished {
                    operation_id: query.operation.clone(),
                    generation: DecimalU64(generation),
                    result: api::AddressQueryResult::Problem { problem },
                });
            }
        }
    }
    pub(crate) fn finish_queries(&mut self, device: DeviceId) {
        let Some(peer) = self.peer_view(device) else { return };
        let Some(boot) = self.selected_session(device).map(|(_, session)| session.peer_boot) else {
            return;
        };
        for (id, query) in &mut self.queries {
            if query.peer == Some(device) && query.boot.is_none() && !query.released {
                query.boot = Some(boot);
                query.deadline = None;
                let mut view = peer.clone();
                view.remote = query.address.into();
                self.shared.emit_pairing(api::PairingEvent::AddressQueryFinished {
                    operation_id: query.operation.clone(),
                    generation: DecimalU64(query.generation),
                    result: api::AddressQueryResult::Found {
                        peer: api::FoundPeer {
                            query_id: id.clone(),
                            generation: DecimalU64(query.generation),
                            peer: view,
                            prepared: true,
                        },
                    },
                });
            }
        }
    }
    pub(crate) fn release_query(&mut self, client: [u8; 16], id: &str, generation: u64) {
        if self
            .queries
            .get(id)
            .is_some_and(|q| q.generation == generation && q.client == client)
            && let Some(query) = self.queries.remove(id)
            && let Some(peer) = query.peer
        {
            self.release_prepared_peer(peer);
        }
    }
    pub(super) fn prepared_peer_in_use(&self, peer: DeviceId, address: Option<SocketAddr>) -> bool {
        let pairs = self.pairs.snapshot();
        self.state.roster().contains(peer)
            || self
                .queries
                .values()
                .any(|q| !q.released && (q.peer == Some(peer) || address == Some(q.address)))
            || self.connecting.values().any(|c| c.device == peer)
            || pairs.requests.iter().chain(&pairs.sessions).any(|s| {
                s.peer.device_id == peer && !matches!(s.phase, pairing::Phase::Completed | pairing::Phase::Ended)
            })
    }
    fn release_prepared_peer(&mut self, peer: DeviceId) {
        if self.prepared_peer_in_use(peer, None) {
            return;
        }
        self.dials.entry(peer).or_default().released_query = true;
        let links = self
            .sessions
            .iter()
            // Connection selection may keep the inbound physical instance even
            // when our address query created the prepared-peer reference. Its
            // C0 role does not extend that reference after the last consumer leaves.
            .filter_map(|(id, s)| (s.peer_boot.device_id == peer && !s.ready).then_some(*id))
            .collect::<Vec<_>>();
        for id in links {
            self.close(id, Disconnect::LeavingGroup);
        }
    }
    pub(super) fn fail_queries_matching(&mut self, device: DeviceId, boot: Option<Boot>, code: dto::PairCode) {
        for query in self.queries.values_mut() {
            if query.peer == Some(device) && !query.released && boot.is_none_or(|boot| query.boot == Some(boot)) {
                query.released = true;
                query.deadline = None;
                self.shared.emit_pairing(api::PairingEvent::AddressQueryFinished {
                    operation_id: query.operation.clone(),
                    generation: DecimalU64(query.generation),
                    result: api::AddressQueryResult::Problem {
                        problem: failure(code, dto::PairStage::Connect),
                    },
                });
            }
        }
    }
    pub(crate) fn expire_queries(&mut self, now: Instant) {
        for query in self.queries.values_mut() {
            if query.deadline.is_some_and(|at| now >= at) && !query.released {
                query.released = true;
                query.deadline = None;
                self.shared.emit_pairing(api::PairingEvent::AddressQueryFinished {
                    operation_id: query.operation.clone(),
                    generation: DecimalU64(query.generation),
                    result: api::AddressQueryResult::Problem {
                        problem: failure(dto::PairCode::ConnectTimeout, dto::PairStage::Connect),
                    },
                });
            }
        }
    }
    pub(crate) fn start_pairing(
        &mut self,
        client: [u8; 16],
        operation: String,
        target: api::PairTarget,
    ) -> ApiResult<api::PairReply<api::Started>> {
        let operation_key = format!("{:032x}/{operation}", u128::from_le_bytes(client));
        let key = serde_json::to_string(&target).map_err(internal)?;
        match self.cached(&operation_key, &key) {
            Ok(Some(reply)) => return Ok(reply),
            Err(error) => return Ok(reject(error)),
            _ => {}
        }
        if self.onboarding_pairing_busy() {
            let reply = reject(failure(dto::PairCode::QueueFull, dto::PairStage::Request));
            self.remember(operation_key, key, &reply);
            return Ok(reply);
        }
        let selected = match &target {
            api::PairTarget::NearbyDevice { device } => {
                let visible = self.shared.nearby.load();
                match visible
                    .rows
                    .iter()
                    .find(|r| r.device == *device && r.compatible && r.reachable)
                {
                    Some(_) => self
                        .nearby
                        .get(&device.0)
                        .and_then(|s| s.sighting.addrs.first())
                        .copied()
                        .map(|a| (device.0, a, None)),
                    None => None,
                }
            }
            api::PairTarget::PreparedQuery { query_id, generation } => self
                .queries
                .get(query_id)
                .filter(|q| q.client == client && q.generation == generation.0 && !q.released)
                .and_then(|q| Some((q.peer?, q.address, q.boot))),
        };
        let Some((device, address, prepared)) = selected else {
            let code = if matches!(target, api::PairTarget::PreparedQuery { .. }) {
                dto::PairCode::PreparedPeerLost
            } else {
                dto::PairCode::StateChanged
            };
            let reply = reject(failure(code, dto::PairStage::Request));
            self.remember(operation_key, key, &reply);
            return Ok(reply);
        };
        if self.state.roster().contains(device)
            && self
                .sessions
                .values()
                .any(|s| s.ready && s.peer_boot.device_id == device)
        {
            let reply = reject(failure(dto::PairCode::AlreadyConnected, dto::PairStage::Request));
            self.remember(operation_key, key, &reply);
            return Ok(reply);
        }
        let current = self.pairs.snapshot();
        if current.requests.len()
            + current
                .sessions
                .iter()
                .filter(|s| !matches!(s.phase, pairing::Phase::Completed | pairing::Phase::Ended))
                .count()
            + self.connecting.len()
            >= pairing::MAX_ACTIVE
            || current
                .requests
                .iter()
                .chain(&current.sessions)
                .filter(|s| {
                    s.peer.device_id == device && !matches!(s.phase, pairing::Phase::Completed | pairing::Phase::Ended)
                })
                .count()
                + self.connecting.values().filter(|s| s.device == device).count()
                >= pairing::MAX_PER_PEER
        {
            let reply = reject(failure(dto::PairCode::QueueFull, dto::PairStage::Request));
            self.remember(operation_key, key, &reply);
            return Ok(reply);
        }
        let id = PairId {
            initiator_boot: self.links.boot(),
            random: random_id()?,
        };
        let endpoint = endpoint(address, Some(device), prepared.and_then(|_| self.peer_view(device)));
        self.endpoints.insert(id, endpoint.clone());
        self.connecting.insert(
            id,
            Connecting {
                id,
                device,
                endpoint,
                revision: self.shared.pairings.load().revision.0 + 1,
                deadline: Instant::now() + Duration::from_secs(10),
            },
        );
        let reply = api::PairReply::Value(api::Started::Started {
            pair_id: pairing::key(id),
        });
        self.remember(operation_key, key, &reply);
        self.publish_pairings();
        self.start_connecting(device);
        if self.connecting.contains_key(&id) {
            if prepared.is_some() {
                self.fail_connecting(device, dto::PairCode::PreparedPeerLost);
            } else {
                match self.links.dial(device, vec![address], DialMode::DirectOnly) {
                    Ok(()) => {}
                    Err(oink_network::link::DialRefused::AlreadyLinked) => {}
                    Err(_) => self.fail_connecting(device, dto::PairCode::Unreachable),
                }
            }
        }
        if let api::PairTarget::PreparedQuery { query_id, generation } = target {
            self.release_query(client, &query_id, generation.0);
        }
        Ok(reply)
    }
    pub(crate) fn start_connecting(&mut self, device: DeviceId) {
        let Some((link, session)) = self
            .selected_session(device)
            .filter(|(_, session)| session.path == LinkPath::Direct && session.peer.is_some())
        else {
            return;
        };
        let peer = session.peer_boot;
        let greeting = self.sessions[&link].peer.as_ref().expect("greeting").clone();
        for id in self
            .connecting
            .values()
            .filter(|s| s.device == device)
            .map(|s| s.id)
            .collect::<Vec<_>>()
        {
            let result = (|| -> v1::Result<dto::PairContext> {
                let anchor = greeting.anchor()?;
                if anchor == self.state.roster().anchor() {
                    return Err(v1::ErrorCode::StateConflict.into());
                }
                dto::PairContext::from_raw(
                    [
                        (1, wire::pair(id).write()),
                        (2, wire::boot(self.links.boot()).write()),
                        (3, wire::boot(peer).write()),
                        (4, RawValue::Bytes(self.state.roster().anchor().group.0.to_vec())),
                        (5, RawValue::Bytes(anchor.group.0.to_vec())),
                        (6, (self.state.roster().members().count() as u8).write()),
                        (7, greeting.greeting.advert()?.count()?.write()),
                        (8, RawValue::Bytes(anchor.group.0.to_vec())),
                        (9, RawValue::Bytes(greeting.hash()?.to_vec())),
                        (10, RawValue::from(dto::PAIRING_SUITE)),
                        (11, RawValue::Bytes(anchor.root.to_vec())),
                        (12, RawValue::Bytes(self.state.roster().anchor().root.to_vec())),
                    ]
                    .into(),
                )
            })();
            let Ok(context) = result else {
                self.fail_connecting(device, dto::PairCode::AlreadyConnected);
                continue;
            };
            match self.pairs.start(context, peer, greeting.transcript.clone()) {
                Ok(effects) => {
                    let verified = self.peer_view(device);
                    if let Some(endpoint) = self.endpoints.get_mut(&id) {
                        endpoint.verified = verified;
                    }
                    self.connecting.remove(&id);
                    self.pair_effects(effects);
                }
                Err(problem) => {
                    self.fail_connecting(device, problem.code().unwrap_or(dto::PairCode::ProtocolViolation))
                }
            }
        }
    }
    pub(crate) fn fail_connecting(&mut self, device: DeviceId, code: dto::PairCode) {
        let ids = self
            .connecting
            .values()
            .filter(|s| s.device == device)
            .map(|s| s.id)
            .collect::<Vec<_>>();
        for id in ids {
            self.fail_connecting_pair(id, code);
        }
    }
    pub(crate) fn fail_connecting_pair(&mut self, id: PairId, code: dto::PairCode) {
        if let Some(value) = self.connecting.remove(&id) {
            let problem = failure(code, dto::PairStage::Connect);
            let view = api::PairView {
                id: pairing::key(id),
                revision: DecimalU64(value.revision + 1),
                direction: api::Direction::Outgoing,
                peer: value.endpoint.clone(),
                phase: 8,
                attempts_left: 3,
                feedback: api::Feedback::Waiting,
                terminal: Some(problem.clone()),
            };
            self.local_terminal.push_back(view);
            if self.local_terminal.len() > 256 {
                self.local_terminal.pop_front();
            }
            self.publish_pairings();
            let _ = self.durable_event(
                api::PairingEvent::PairingFailed {
                    event_id: DecimalU64(0),
                    pair_id: pairing::key(id),
                    peer: value.endpoint,
                    problem,
                },
                None,
            );
        }
    }
    pub(crate) fn accept_pairing(&mut self, id: String, revision: u64) -> ApiResult<api::PairReply<api::Accepted>> {
        self.pair_command(id, revision, "accept", |pairs, id, revision| pairs.accept(id, revision))
    }
    pub(crate) fn decline_pairing(&mut self, id: String, revision: u64) -> ApiResult<api::PairReply<api::Accepted>> {
        self.pair_command(id, revision, "decline", |pairs, id, revision| {
            pairs.decline(id, revision)
        })
    }
    fn pair_command(
        &mut self,
        id: String,
        revision: u64,
        kind: &str,
        run: impl FnOnce(&mut Sessions, PairId, u64) -> pairing::Result<Vec<pairing::Effect>>,
    ) -> ApiResult<api::PairReply<api::Accepted>> {
        let op = format!("{kind}/{id}/{revision}");
        match self.cached(&op, kind) {
            Ok(Some(reply)) => return Ok(reply),
            Err(error) => return Ok(reject(error)),
            _ => {}
        }
        let reply = if let Some(id) = pairing::parse_key(&id) {
            match run(&mut self.pairs, id, revision) {
                Ok(effects) => {
                    self.pair_effects(effects);
                    api::PairReply::Value(api::Accepted::Accepted)
                }
                Err(problem) => wire_reject(problem),
            }
        } else {
            reject(failure(dto::PairCode::PairClosed, dto::PairStage::Code))
        };
        self.remember(op, kind.into(), &reply);
        Ok(reply)
    }
    pub(crate) fn submit_pairing(
        &mut self,
        id: String,
        revision: u64,
        attempt: u8,
        code: String,
    ) -> ApiResult<api::PairReply<api::Accepted>> {
        self.pair_command(
            id,
            revision,
            &format!("submit/{attempt}"),
            move |pairs, id, revision| pairs.submit(id, revision, attempt, &code, Instant::now()),
        )
    }
    pub(crate) fn pairing_code(&self, id: String) -> ApiResult<api::CodeView> {
        let id = pairing::parse_key(&id).ok_or_else(stopped)?;
        let code = self.pairs.code(id).map_err(|_| stopped())?;
        Ok(api::CodeView {
            six_digits: code.to_string(),
        })
    }
    pub(crate) fn cancel_pairing(&mut self, id: String, revision: u64) -> ApiResult<api::PairReply<api::CancelResult>> {
        let op = format!("cancel/{id}");
        match self.cached(&op, "cancel") {
            Ok(Some(reply)) => return Ok(reply),
            Err(error) => return Ok(reject(error)),
            _ => {}
        }
        let Some(pair) = pairing::parse_key(&id) else {
            return Ok(reject(failure(dto::PairCode::PairClosed, dto::PairStage::Code)));
        };
        let reply = if let Some(connecting) = self.connecting.get(&pair) {
            if connecting.revision != revision {
                reject(failure(dto::PairCode::StaleRevision, dto::PairStage::Code))
            } else {
                self.fail_connecting_pair(pair, dto::PairCode::CancelledLocally);
                api::PairReply::Value(api::CancelResult::Cancelled)
            }
        } else if self.pairs.view(pair).is_some_and(|v| v.revision != revision) {
            reject(failure(dto::PairCode::StaleRevision, dto::PairStage::Code))
        } else {
            match self.transactions.cancel(pair, &mut self.journal).map_err(internal)? {
                pair_merge::Cancel::Committed(bundle) => api::PairReply::Value(api::CancelResult::AlreadyCommitted {
                    undo: self.undo_handle(&bundle)?,
                }),
                pair_merge::Cancel::Unconfirmed => {
                    let effects = self.pairs.fail(
                        pair,
                        pairing::problem(dto::PairCode::CommitUnconfirmed, dto::PairStage::Commit),
                    );
                    self.pair_effects(effects);
                    api::PairReply::Value(api::CancelResult::CommitUnconfirmed)
                }
                pair_merge::Cancel::Cancelled => match self.pairs.cancel(pair, revision) {
                    Ok(effects) => {
                        self.pair_effects(effects);
                        api::PairReply::Value(api::CancelResult::Cancelled)
                    }
                    Err(problem) => wire_reject(problem),
                },
            }
        };
        self.remember(op, "cancel".into(), &reply);
        Ok(reply)
    }
    pub(crate) fn pair_effects(&mut self, effects: Vec<pairing::Effect>) {
        for effect in effects {
            match effect {
                pairing::Effect::Send { peer, message } => {
                    if let Some(link) = self.peer_link(peer) {
                        let _ = self.send(link, message);
                    }
                }
                pairing::Effect::Changed => self.publish_pairings(),
                pairing::Effect::Verified(pair) => {
                    if let Some(view) = self.pairs.view(pair).cloned() {
                        match self.transactions.verified(view, &self.state, &mut self.journal) {
                            Ok(effects) => {
                                let _ = self.merge_effects(effects);
                            }
                            Err(error) => {
                                tracing::warn!(%error, "pairing verification could not start merge");
                                let effects = self.pairs.fail(
                                    pair,
                                    pairing::problem(dto::PairCode::LocalStorageFailed, dto::PairStage::Prepare),
                                );
                                self.pair_effects(effects);
                            }
                        }
                    }
                }
                pairing::Effect::Completed(_) => self.publish_pairings(),
                pairing::Effect::Ended { id, mut problem } => {
                    match self.transactions.cancel(id, &mut self.journal) {
                        Ok(pair_merge::Cancel::Committed(_)) => {
                            self.publish_pairings();
                            continue;
                        }
                        Ok(pair_merge::Cancel::Unconfirmed) => {
                            problem = pairing::problem(dto::PairCode::CommitUnconfirmed, dto::PairStage::Commit)
                        }
                        Ok(pair_merge::Cancel::Cancelled) => {}
                        Err(error) => {
                            tracing::warn!(%error, "pairing cancellation could not persist");
                            problem = pairing::problem(dto::PairCode::LocalStorageFailed, dto::PairStage::Commit);
                        }
                    }
                    self.pairs.terminal_problem(id, problem.clone());
                    self.publish_pairings();
                    if let Some(peer) = self.endpoints.get(&id).cloned() {
                        let event = api::PairingEvent::PairingFailed {
                            event_id: DecimalU64(0),
                            pair_id: pairing::key(id),
                            peer,
                            problem: api::PairProblem::try_from(&problem).expect("problem"),
                        };
                        let _ = self.durable_event(event, None);
                    }
                }
            }
        }
    }
    pub(crate) fn merge_effects(&mut self, effects: Vec<pair_merge::Effect>) -> ApiResult<()> {
        self.pending_effects.extend(effects);
        while let Some(effect) = self.pending_effects.pop_front() {
            match effect {
                pair_merge::Effect::Send { peer, message } => {
                    if let Some(raw) = message.raw().get(&1)
                        && let Ok(id) = dto::PairId::read(raw).and_then(|p| wire::read_pair(&p))
                        && matches!(
                            message,
                            Message::ConfigPairCommit(_) | Message::ConfigPairCommitPermit(_)
                        )
                        && let Ok(effects) = self.pairs.progress(id, pairing::Phase::Committing, Instant::now())
                    {
                        self.pair_effects(effects);
                    }
                    if let Some(link) = self.peer_link(peer) {
                        self.send(link, message).map_err(internal)?;
                    }
                }
                pair_merge::Effect::Project => match self.project_decision() {
                    Ok(true) => {
                        self.finish_recovery_projection().map_err(internal)?;
                    }
                    result => {
                        self.pending_effects.push_front(pair_merge::Effect::Project);
                        self.projection_retry = Some(Instant::now() + Duration::from_millis(50));
                        return result.map(|_| ());
                    }
                },
                pair_merge::Effect::Admit { pair, peer } => {
                    if let Ok(effects) = self.pairs.progress(pair, pairing::Phase::Committing, Instant::now()) {
                        self.pair_effects(effects);
                    }
                    if let Ok(effects) = self.pairs.progress(pair, pairing::Phase::Admitting, Instant::now()) {
                        self.pair_effects(effects);
                    }
                    if let Some(link) = self.peer_link(peer) {
                        self.begin_admission(link).map_err(internal)?;
                    }
                }
                pair_merge::Effect::Completed {
                    pair,
                    bundle,
                    reconciled,
                } => {
                    let intent = bundle.certificate().map_err(internal)?.intent().map_err(internal)?;
                    let peer = if self.identity.id()
                        == wire::read_device(&intent.source_issuer().map_err(internal)?).map_err(internal)?
                    {
                        wire::read_device(&intent.target_issuer().map_err(internal)?).map_err(internal)?
                    } else {
                        wire::read_device(&intent.source_issuer().map_err(internal)?).map_err(internal)?
                    };
                    let peer = self.peer_view(peer).ok_or_else(stopped)?;
                    let undo = self.undo_handle(&bundle)?;
                    let event = api::PairingEvent::PairingCompleted {
                        event_id: DecimalU64(0),
                        pair_id: pairing::key(pair),
                        peer,
                        merge_id: DeviceId(intent.merge_id().map_err(internal)?.0).to_hex(),
                        target_group: reference(
                            group_merge::anchor(&intent.target().map_err(internal)?).map_err(internal)?,
                        ),
                        imported_devices: intent
                            .members()
                            .map_err(internal)?
                            .0
                            .into_iter()
                            .map(|m| wire::read_device(&m.device()?).map(Into::into))
                            .collect::<v1::Result<Vec<_>>>()
                            .map_err(internal)?,
                        undo,
                        reconciled,
                    };
                    self.durable_event(
                        event,
                        Some((
                            pair,
                            bundle
                                .certificate()
                                .map_err(internal)?
                                .permit_decision()
                                .map_err(internal)?,
                        )),
                    )?;
                    if let Ok(effects) = self.pairs.progress(pair, pairing::Phase::Committing, Instant::now()) {
                        self.pair_effects(effects);
                    }
                    if let Ok(effects) = self.pairs.progress(pair, pairing::Phase::Admitting, Instant::now()) {
                        self.pair_effects(effects);
                    }
                    if let Ok(effects) = self.pairs.complete(pair) {
                        self.pair_effects(effects);
                    }
                    self.supersede(pair, &intent);
                    self.transactions.completed(pair);
                }
            }
        }
        Ok(())
    }
    pub(crate) fn supersede(&mut self, winner: PairId, intent: &dto::MergeIntent) {
        let source = intent.source().ok();
        let target = intent.target().ok();
        for view in self
            .pairs
            .snapshot()
            .requests
            .into_iter()
            .chain(self.pairs.snapshot().sessions)
        {
            if view.id == winner || matches!(view.phase, pairing::Phase::Completed | pairing::Phase::Ended) {
                continue;
            }
            let same = view.context.source_group().ok().map(|g| g.0)
                == source.as_ref().and_then(|s| s.group().ok()).map(|g| g.0)
                || view.context.target_group().ok().map(|g| g.0)
                    == source.as_ref().and_then(|s| s.group().ok()).map(|g| g.0);
            if same {
                let same_endpoints = [intent.source_issuer().ok(), intent.target_issuer().ok()]
                    .into_iter()
                    .flatten()
                    .any(|d| wire::read_device(&d) == Ok(view.peer.device_id));
                let mut reason = pairing::problem(
                    if same_endpoints {
                        dto::PairCode::SupersededByPairing
                    } else {
                        dto::PairCode::GroupChanged
                    },
                    dto::PairStage::Commit,
                )
                .into_raw();
                if same_endpoints {
                    reason.insert(5, wire::pair(winner).write());
                }
                let effects = self
                    .pairs
                    .fail(view.id, dto::PairProblem::from_raw(reason).expect("problem"));
                self.pair_effects(effects);
            }
        }
        let _ = target;
    }
    fn undo_handle(&self, bundle: &dto::MergeTransferBundle) -> ApiResult<api::UndoHandle> {
        let intent = bundle.certificate().map_err(internal)?.intent().map_err(internal)?;
        let source = wire::read_device(&intent.source_issuer().map_err(internal)?).map_err(internal)?;
        let target = wire::read_device(&intent.target_issuer().map_err(internal)?).map_err(internal)?;
        let peer = if source == self.identity.id() { target } else { source };
        let anchor = group_merge::anchor(&intent.target().map_err(internal)?).map_err(internal)?;
        let roster = oink_network::roster::Roster::verify(
            bundle.target_state().map_err(internal)?.roster().map_err(internal)?,
            anchor,
        )
        .map_err(internal)?;
        let add = roster.member(peer).ok_or_else(stopped)?.add;
        Ok(api::UndoHandle {
            group: reference(anchor),
            merge_id: DeviceId(intent.merge_id().map_err(internal)?.0).to_hex(),
            peer: peer.into(),
            observed_add: DeviceId(add).to_hex(),
        })
    }
    pub(crate) fn undo_pairing(
        &mut self,
        operation: String,
        undo: api::UndoHandle,
    ) -> ApiResult<api::PairReply<api::Accepted>> {
        let key = serde_json::to_string(&undo).map_err(internal)?;
        match self.cached(&operation, &key) {
            Ok(Some(reply)) => return Ok(reply),
            Err(error) => return Ok(reject(error)),
            _ => {}
        }
        let result = if undo.group != reference(self.state.roster().anchor()) {
            Err(failure(dto::PairCode::UndoStale, dto::PairStage::Undo))
        } else {
            match parse_hash(&undo.observed_add).map(DeviceId) {
                Some(add) => self
                    .remove_member(undo.peer.0, Some(add.0))
                    .map_err(|_| failure(dto::PairCode::UndoStale, dto::PairStage::Undo)),
                None => Err(failure(dto::PairCode::UndoStale, dto::PairStage::Undo)),
            }
        };
        let reply = match &result {
            Ok(()) => api::PairReply::Value(api::Accepted::Accepted),
            Err(problem) => reject(problem.clone()),
        };
        self.remember(operation.clone(), key, &reply);
        self.shared.emit_pairing(api::PairingEvent::MembershipEdited {
            operation_id: operation,
            result: match result {
                Ok(()) => api::TypedResult::Ok,
                Err(problem) => api::TypedResult::Problem { problem },
            },
        });
        Ok(reply)
    }
    fn durable_event(&mut self, event: api::PairingEvent, receipt: Option<(PairId, u64)>) -> ApiResult<u64> {
        let raw = [
            (1, 1u32.write()),
            (2, RawValue::Bytes(serde_json::to_vec(&event).map_err(internal)?)),
        ]
        .into();
        let before = self.journal.last_event_id().map_err(internal)?;
        let id = if let Some((pair, decision)) = receipt {
            self.journal.receipt(pair, decision, raw)
        } else {
            self.journal.event(raw)
        }
        .map_err(internal)?;
        if id > before {
            let mut event = event;
            set_event_id(&mut event, id);
            self.shared.emit_pairing(event);
        }
        Ok(id)
    }
    pub(crate) fn recovered_failure(
        &mut self,
        recovery: &oink_network::pair_journal::Recovery,
        problem: dto::PairProblem,
    ) -> ApiResult<()> {
        let peer = self.peer_view(recovery.target).ok_or_else(stopped)?;
        let event = api::PairingEvent::PairingFailed {
            event_id: DecimalU64(0),
            pair_id: pairing::key(recovery.pair),
            peer: api::PairEndpoint {
                address: peer.remote.clone(),
                expected_id: Some(recovery.target.into()),
                verified: Some(peer),
            },
            problem: api::PairProblem::try_from(&problem).map_err(internal)?,
        };
        let before = self.journal.last_event_id().map_err(internal)?;
        let raw = [
            (1, 1u32.write()),
            (2, RawValue::Bytes(serde_json::to_vec(&event).map_err(internal)?)),
        ]
        .into();
        let id = self
            .journal
            .aborted_event(recovery.pair, recovery.decision, raw)
            .map_err(internal)?;
        if id > before {
            let mut event = event;
            set_event_id(&mut event, id);
            self.shared.emit_pairing(event);
        }
        Ok(())
    }
    pub(crate) fn pairing_events(&self, after: u64) -> ApiResult<Vec<api::PairingEvent>> {
        self.journal
            .events(after)
            .map_err(internal)?
            .into_iter()
            .map(|entry| {
                if entry.body.get(&1) != Some(&1u32.write()) {
                    return Err(internal("unsupported outcome schema"));
                }
                let RawValue::Bytes(bytes) = entry.body.get(&2).ok_or_else(stopped)? else {
                    return Err(stopped());
                };
                let mut event = serde_json::from_slice(bytes).map_err(internal)?;
                set_event_id(&mut event, entry.id);
                Ok(event)
            })
            .collect()
    }
    pub(crate) fn ack_pairing_event(&mut self, id: u64) -> ApiResult<()> {
        self.journal.acknowledge(id).map_err(internal)
    }
    pub(crate) fn blocked_devices(&self) -> ApiResult<BlockedDevicesSnapshot> {
        let blocks = self.blocks.as_ref().ok_or_else(stopped)?;
        Ok(BlockedDevicesSnapshot {
            stream: self.block_stream.clone(),
            revision: DecimalU64(self.block_revision),
            entries: blocks
                .entries()
                .into_iter()
                .map(|e| BlockedDevice {
                    device: e.device.into(),
                    name: e.name,
                    os: e.os,
                    blocked_at_ms: e.blocked_at_ms,
                })
                .collect(),
        })
    }
    pub(crate) fn block_edit(&mut self, operation: String, device: DeviceId, install: bool) -> ApiResult<BlockReply> {
        if device == self.identity.id() {
            return Ok(BlockReply::Error(BlockListError { code: 0, limit: None }));
        }
        let key = format!("block/{device}/{install}");
        match self.cached(&operation, &key) {
            Ok(Some(reply)) => return Ok(reply),
            Err(_) => return Ok(BlockReply::Error(BlockListError { code: 0, limit: None })),
            _ => {}
        }
        let hint = self
            .peer_view(device)
            .map(|v| (Some(v.name), Some(v.os)))
            .or_else(|| {
                self.nearby
                    .get(&device)
                    .map(|s| (Some(s.sighting.advert.name.clone()), Some(s.sighting.advert.os.clone())))
            })
            .unwrap_or_default();
        let reply = if let Some(blocks) = self.blocks.as_mut() {
            let before = blocks.contains(device);
            let result = if install {
                blocks
                    .block(crate::config::records::BlockedDevice {
                        device,
                        name: hint.0,
                        os: hint.1,
                        blocked_at_ms: now_ms(),
                    })
                    .map(|_| !before)
            } else {
                blocks.unblock(device)
            };
            match result {
                Ok(changed) => {
                    if changed {
                        self.block_revision += 1;
                        if install {
                            let effects = self.pairs.block_pending(device);
                            self.pair_effects(effects);
                        }
                        self.publish_nearby();
                        self.shared.emit(ConfigChange::BlockedDevices);
                    }
                    BlockReply::Edited(BlockEdited {
                        operation_id: operation.clone(),
                        changed,
                        snapshot: self.blocked_devices()?,
                    })
                }
                Err(error) => BlockReply::Error(BlockListError {
                    code: match error {
                        crate::config::store::StoreError::LimitExceeded => 2,
                        _ => 1,
                    },
                    limit: None,
                }),
            }
        } else {
            BlockReply::Error(BlockListError { code: 3, limit: None })
        };
        self.remember(operation, key, &reply);
        Ok(reply)
    }
}
fn set_event_id(event: &mut api::PairingEvent, id: u64) {
    match event {
        api::PairingEvent::PairingFailed { event_id, .. } | api::PairingEvent::PairingCompleted { event_id, .. } => {
            *event_id = DecimalU64(id)
        }
        _ => {}
    }
}

fn parse_hash(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let mut bytes = [0; 32];
    for (i, value) in bytes.iter_mut().enumerate() {
        *value = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}
