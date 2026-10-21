//! All C0 state mutations and durable decisions run on this ordinary-thread actor.
use super::*;
use super::{facts::Facts, handshake::Link};
use crate::{
    api::{ApiError, pairing as api},
    device_cards::DecimalU64,
};
use oink_foundation::session::PairId;
use oink_network::{
    admission,
    discovery::{Advert, Discovery, DiscoveryConfig, Sighting},
    group_state::State,
    link::{ConfigEvent, DialFailure, DialMode, Direction, Disconnect, LinkId, LinkPath, PostError},
    pair_journal::Journal,
    pair_merge::Transactions,
    pairing::{self, Sessions},
    roster::Anchor,
    wire_identity as wire,
};
use oink_protocol::v1::{
    Message, RawValue,
    schema::{WireDto, WireValue},
};
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, watch};
#[path = "runtime_diagnostics.rs"]
mod diagnostics;
#[path = "pairing_api.rs"]
mod pairing_api;
#[path = "recovery.rs"]
mod recovery;
#[path = "runtime_state.rs"]
mod state_owner;
/// Signed state exchange after a same-group C0 greeting succeeds.
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn random_id() -> ApiResult<[u8; 16]> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(internal)?;
    Ok(bytes)
}
pub(super) fn internal(error: impl ToString) -> ApiError {
    ApiError::Internal {
        message: error.to_string(),
    }
}
pub(super) fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::InvalidArgument {
        message: message.into(),
    }
}
fn stopped() -> ApiError {
    ApiError::NetworkStopped
}
pub(super) struct Shared {
    pub members: Snapshot<MemberTable>,
    pub applied: Snapshot<AppliedState>,
    pub displays: Snapshot<LocalDisplays>,
    pub router: Arc<Snapshot<RouterLayout>>,
    pub nearby: Snapshot<api::NearbySnapshot>,
    pub pairings: Snapshot<api::PairingSnapshot>,
    pub migration: Snapshot<Option<api::GroupMigrationStatus>>,
    pub addresses: Snapshot<Vec<std::net::IpAddr>>,
    pub diagnostic: Snapshot<serde_json::Value>,
    pub diagnostic_layout: Snapshot<Option<crate::diagnostics::layout::Inputs>>,
    pub local_facts: Snapshot<dto::DeviceFacts>,
    subscribers: Mutex<Vec<mpsc::UnboundedSender<ConfigChange>>>,
    pub stopped: watch::Sender<bool>,
}
impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfigSnapshots(..)")
    }
}
impl Shared {
    fn emit_pairing(&self, event: api::PairingEvent) {
        self.emit(ConfigChange::PairingEvent(Box::new(event)));
    }
    fn emit(&self, change: ConfigChange) {
        self.subscribers
            .lock()
            .expect("subscribers")
            .retain(|s| s.send(change.clone()).is_ok());
    }
    pub fn subscribe(&self) -> ConfigChanges {
        let (tx, rx) = mpsc::unbounded_channel();
        if !*self.stopped.borrow() {
            self.subscribers.lock().expect("subscribers").push(tx);
        }
        ConfigChanges(rx)
    }
}
pub(super) enum Command {
    Run(Box<dyn FnOnce(&mut Owner) + Send>),
    Stop,
}
enum Incoming {
    Control(LinkId, Result<Vec<u8>, ()>, tokio::sync::oneshot::Sender<()>),
    Writable(LinkId),
    Probe {
        operation: String,
        generation: u64,
        result: Result<DeviceId, oink_network::link::AddressProbeError>,
    },
}
struct Seen {
    sighting: Sighting,
    at: Instant,
}
struct Query {
    client: [u8; 16],
    operation: String,
    generation: u64,
    address: SocketAddr,
    peer: Option<DeviceId>,
    boot: Option<Boot>,
    deadline: Option<Instant>,
    released: bool,
}
struct Connecting {
    id: PairId,
    device: DeviceId,
    endpoint: api::PairEndpoint,
    revision: u64,
    deadline: Instant,
}
#[derive(Default)]
struct Dial {
    released_query: bool,
    after: Option<Instant>,
    failures: usize,
    active: bool,
    last_relayed: bool,
    started: bool,
}
impl Dial {
    fn failed(&mut self, reason: DialFailure, now: Instant) {
        self.active = false;
        self.started = true;
        self.failures = self.failures.saturating_add(1);
        // LinkLayer forgets failed dials; it has no second reconnect schedule.
        // Timeouts/no route must keep probing even when the other side cannot
        // originate a connection (for example through the local router's NAT).
        let backoff = if reason == DialFailure::Unreachable {
            &RECONNECT_BACKOFF[..]
        } else {
            &HANDSHAKE_BACKOFF[..]
        };
        self.after = Some(now + backoff[(self.failures - 1).min(backoff.len() - 1)]);
    }
    fn reset(&mut self, now: Instant) {
        self.failures = 0;
        self.after = Some(now);
        self.active = false;
        // The larger-ID delay only applies to the initial round, never to an
        // explicit recovery signal such as Lost, interface change or wake.
        self.started = true;
    }
    fn due(&mut self, local: DeviceId, peer: DeviceId, now: Instant) -> bool {
        if self.active {
            return false;
        }
        if !self.started {
            self.started = true;
            if local > peer {
                self.after = Some(now + RECONNECT_BACKOFF[0] * 2);
            }
        }
        self.after.is_none_or(|at| now >= at)
    }
}
pub(super) struct Owner {
    config: Arc<Mutex<crate::config::AgentConfig>>,
    owner_gate: Arc<Mutex<()>>,
    pending_effects: VecDeque<oink_network::pair_merge::Effect>,
    projection_retry: Option<Instant>,
    pending_revocation: Option<State>,
    projection_announced: Option<[u8; 32]>,
    recovery_projected: Vec<(LinkId, Option<Message>, bool)>,
    state: State,
    needs_apply: bool,
    stored: group_store::StoredGroupState,
    data_dir: PathBuf,
    identity: Arc<oink_network::identity::Identity>,
    links: LinkLayer,
    port: ConfigPort,
    application: Arc<dyn ConfigApplication>,
    shared: Arc<Shared>,
    commands: mpsc::UnboundedReceiver<Command>,
    incoming_tx: mpsc::UnboundedSender<Incoming>,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    sessions: BTreeMap<LinkId, Link>,
    pairs: Sessions,
    onboarding_connect: bool,
    transactions: Transactions,
    journal: Journal,
    blocks: Option<crate::config::records::BlockList>,
    block_stream: String,
    block_revision: u64,
    presence: crate::config::records::PresenceStore,
    address_book: super::local_records::AddressStore,
    cache: super::local_records::FactsCache,
    nearby: BTreeMap<DeviceId, Seen>,
    incompatible: BTreeSet<DeviceId>,
    nearby_revision: u64,
    dials: BTreeMap<DeviceId, Dial>,
    queries: HashMap<String, Query>,
    connecting: HashMap<PairId, Connecting>,
    endpoints: HashMap<PairId, api::PairEndpoint>,
    local_terminal: VecDeque<api::PairView>,
    operations: VecDeque<(String, String, serde_json::Value)>,
    proof_objects: oink_network::pair_objects::Objects,
    proof_expected: HashMap<(LinkId, [u8; 32]), recovery::Expected>,
    lookups: HashMap<LinkId, (Anchor, Instant)>,
    recovery_due: HashMap<PairId, Instant>,
    pub(super) local: Facts,
    facts: BTreeMap<DeviceId, Arc<dto::DeviceFacts>>,
    last_facts: Instant,
    network_checked: Instant,
    #[cfg(test)]
    pub(super) hold_facts: bool,
    last_state: Instant,
    displays: Vec<DisplayInfo>,
    detached: detached::DetachedScreens,
    discovery: Option<Arc<Discovery>>,
    advert: watch::Sender<Advert>,
    sleeping: bool,
    pub(super) layout_sink: Option<crate::diagnostics::layout::Sink>,
    router_compilation: Option<crate::diagnostics::layout::RouterCompilation>,
    diagnostic_emitted: Option<crate::diagnostics::layout::Inputs>,
}
impl ConfigModule {
    pub(super) fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Owner) -> ApiResult<R> + Send + 'static,
    ) -> ApiResult<R> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.commands
            .send(Command::Run(Box::new(move |owner| {
                let _ = tx.send(f(owner));
            })))
            .map_err(|_| stopped())?;
        rx.recv().map_err(|_| stopped())?
    }
    pub(super) fn send(&self, f: impl FnOnce(&mut Owner) + Send + 'static) {
        let _ = self.commands.send(Command::Run(Box::new(f)));
    }
}
impl Owner {
    pub fn start(deps: ConfigDeps, config: oink_network::NetworkConfig) -> Result<ConfigModule, StartError> {
        let fail = |e: &dyn std::fmt::Display| StartError::Unreadable(e.to_string());
        let device = deps.identity.id();
        let path = deps.data_dir.join(group_store::STATE_FILE);
        let mut stored = match group_store::load(&path, device).map_err(|e| fail(&e))? {
            Some(value) => value,
            None => {
                let value =
                    State::create(&deps.identity, &config.device_name, &config.os, now_ms()).map_err(|e| fail(&e))?;
                let value = group_store::StoredGroupState::new(device, value.wire().clone()).map_err(|e| fail(&e))?;
                group_store::save(&path, &value).map_err(|e| fail(&e))?;
                value
            }
        };
        let mut journal =
            Journal::load(&deps.data_dir.join(oink_network::pair_journal::FILE), device).map_err(|e| fail(&e))?;
        journal.recover().map_err(|e| fail(&e))?;
        if let Some(projected) = journal.pending_projection().map_err(|e| fail(&e))? {
            stored.state = projected;
            group_store::save(&path, &stored).map_err(|e| fail(&e))?;
        }
        let roster = stored.state.roster().map_err(|e| fail(&e))?;
        let anchor = Anchor {
            group: stored.group(),
            root: roster.root_id().map_err(|e| fail(&e))?.0,
        };
        let mut state = State::verify(stored.to_wire(), anchor).map_err(|e| fail(&e))?;
        if !state.roster().contains(device) {
            state = State::create(&deps.identity, &config.device_name, &config.os, now_ms()).map_err(|e| fail(&e))?;
            stored.state = state.wire().clone();
            group_store::save(&path, &stored).map_err(|e| fail(&e))?;
        }
        let anchor = state.roster().anchor();
        let value = dto::LayoutValue::from_raw(state.layout().map_err(|e| fail(&e))?.value().map_err(|e| fail(&e))?)
            .map_err(|e| fail(&e))?;
        let layout = Arc::new(layout_wire::from_wire(&value).map_err(|e| fail(&e))?);
        let owners = Arc::new(BTreeMap::new());
        let devices = BTreeSet::from([device]);
        let version = state.layout().map_err(|e| fail(&e))?.version().map_err(|e| fail(&e))?;
        let mut local = Facts::new(
            deps.links.boot(),
            anchor.group,
            &config.device_name,
            &config.os,
            deps.links.profile(),
        )
        .map_err(|e| fail(&e))?;
        {
            let _gate = deps.owner_gate.lock().map_err(|e| fail(&e))?;
            let mut owner_config = deps.config.lock().map_err(|e| fail(&e))?;
            let mut next_owner = owner_config.owner().clone();
            next_owner.switch_group(anchor.group, device).map_err(|e| fail(&e))?;
            let pause = dto::LocalPauseState::from_raw(
                [
                    (1, false.write()),
                    (2, dto::PauseReason::None.write()),
                    (3, 1u64.write()),
                ]
                .into(),
            )
            .map_err(|e| fail(&e))?;
            local
                .owner(
                    next_owner.preferences(),
                    pause,
                    next_owner.claims(anchor.group).watermark(),
                )
                .map_err(|e| fail(&e))?;
            local.group(anchor.group, &next_owner, device).map_err(|e| fail(&e))?;
            owner_config
                .update_owner(|owner| owner.switch_group(anchor.group, device))
                .map_err(|e| fail(&e))?;
        }
        let applied = AppliedState {
            group: anchor.group,
            roster_digest: state.roster().digest().map_err(|e| fail(&e))?,
            members: state.roster().members().map(|m| m.device).collect(),
            layout: layout.clone(),
            owners: owners.clone(),
            layout_source: value.source().map_err(|e| fail(&e))?,
            version: version.clone(),
            paused: false,
            emergency: false,
        };
        let (stopped, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            members: Snapshot::default(),
            applied: Snapshot::new(applied),
            displays: Snapshot::new(LocalDisplays {
                displays: vec![],
                layout: layout.clone(),
                owners: owners.clone(),
            }),
            router: Arc::new(Snapshot::new(RouterLayout {
                group: anchor.group,
                version,
                topology: TopologyLayout::from_effective(&layout.effective(&owners), |d| devices.contains(d)),
                layout,
                owners,
                devices,
            })),
            nearby: Snapshot::new(api::NearbySnapshot {
                revision: DecimalU64(0),
                rows: vec![],
            }),
            pairings: Snapshot::new(api::PairingSnapshot {
                revision: DecimalU64(0),
                requests: vec![],
                sessions: vec![],
            }),
            migration: Snapshot::new(None),
            addresses: Snapshot::default(),
            diagnostic: Snapshot::new(serde_json::Value::Null),
            diagnostic_layout: Snapshot::new(None),
            local_facts: Snapshot::new(local.value().clone()),
            subscribers: Mutex::new(vec![]),
            stopped,
        });
        let (advert, _) = watch::channel(Advert::local(
            device,
            config.device_name.clone(),
            anchor.group,
            state.roster().members().count() as u8,
            deps.links.local_addr().port(),
            config.os.clone(),
        ));
        let (commands_tx, commands) = mpsc::unbounded_channel();
        let (incoming_tx, incoming) = mpsc::unbounded_channel();
        let blocks = crate::config::records::BlockList::load(&deps.data_dir).ok();
        let presence = crate::config::records::PresenceStore::load(&deps.data_dir).map_err(|e| fail(&e))?;
        let address_book = super::local_records::AddressStore::load(&deps.data_dir).map_err(|e| fail(&e))?;
        let cache = super::local_records::FactsCache::load(&deps.data_dir).map_err(|e| fail(&e))?;
        let detached = detached::load(&deps.data_dir.join(detached::DETACHED_FILE)).map_err(|e| fail(&e))?;
        let links = deps.links.clone();
        let mut owner = Self {
            config: deps.config,
            owner_gate: deps.owner_gate,
            pending_effects: VecDeque::new(),
            projection_retry: None,
            pending_revocation: None,
            projection_announced: None,
            recovery_projected: Vec::new(),
            state,
            needs_apply: false,
            stored,
            data_dir: deps.data_dir,
            identity: deps.identity,
            links: deps.links,
            port: deps.port,
            application: deps.application,
            shared: shared.clone(),
            commands,
            incoming_tx,
            incoming,
            sessions: BTreeMap::new(),
            pairs: Sessions::new(links.boot()),
            onboarding_connect: false,
            transactions: Transactions::default(),
            journal,
            blocks,
            block_stream: format!("{:032x}", u128::from_le_bytes(random_id().map_err(|e| fail(&e))?)),
            block_revision: 0,
            presence,
            address_book,
            cache,
            nearby: BTreeMap::new(),
            incompatible: BTreeSet::new(),
            nearby_revision: 0,
            dials: BTreeMap::new(),
            queries: HashMap::new(),
            connecting: HashMap::new(),
            endpoints: HashMap::new(),
            local_terminal: VecDeque::new(),
            operations: VecDeque::new(),
            proof_objects: Default::default(),
            proof_expected: HashMap::new(),
            lookups: HashMap::new(),
            recovery_due: HashMap::new(),
            local,
            facts: BTreeMap::new(),
            last_facts: Instant::now(),
            network_checked: Instant::now() - Duration::from_secs(2),
            #[cfg(test)]
            hold_facts: false,
            last_state: Instant::now(),
            displays: vec![],
            detached,
            discovery: None,
            advert,
            sleeping: false,
            layout_sink: None,
            router_compilation: None,
            diagnostic_emitted: None,
        };
        owner
            .application
            .apply_state(&owner.stored)
            .map_err(StartError::Unreadable)?;
        if owner.journal.pending_projection().map_err(|e| fail(&e))?.is_some() {
            owner
                .journal
                .projected(owner.state.digest().map_err(|e| fail(&e))?)
                .map_err(|e| fail(&e))?;
        }
        owner.publish().map_err(|e| fail(&e))?;
        std::thread::Builder::new().name("oink-config".into()).spawn(move || {
            match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime.block_on(owner.run(config)),
                Err(error) => {
                    tracing::error!(%error,"configuration executor failed");
                    owner.shared.stopped.send_replace(true);
                }
            }
        })?;
        Ok(ConfigModule {
            shared,
            commands: commands_tx,
            links,
        })
    }
    async fn run(&mut self, config: oink_network::NetworkConfig) {
        let (sightings, mut sightings_rx) = mpsc::channel(256);
        match Discovery::start(
            DiscoveryConfig {
                bind_ip: config.bind_ip,
                port: config.discovery_port(),
                beacon: config.beacon,
                beacon_interval: Duration::from_secs(2),
                beacon_ports: config.beacon_ports,
                mdns: config.mdns,
            },
            self.advert.subscribe(),
            sightings,
        )
        .await
        {
            Ok(discovery) => self.discovery = Some(Arc::new(discovery)),
            Err(error) => tracing::warn!(%error,"discovery unavailable"),
        }
        self.tick();
        loop {
            let deadline = self.next_deadline();
            tokio::select! {
                command=self.commands.recv()=>match command{Some(Command::Run(f))=>f(self),Some(Command::Stop)|None=>break},
                event=self.port.next()=>match event{Some(event)=>self.network(event),None=>break},
                event=self.incoming.recv()=>if let Some(event)=event{self.incoming(event)},
                sighting=sightings_rx.recv(),if self.discovery.is_some()=>if let Some(sighting)=sighting{self.sighting(sighting)},
                _=tokio::time::sleep_until(deadline.into())=>self.tick(),
            }
        }
        let effects = self.pairs.shutdown();
        self.pair_effects(effects);
        // Give queued terminal controls one bounded drain opportunity before stop.
        for link in self.sessions.keys().copied().collect::<Vec<_>>() {
            self.flush(link);
        }
        self.disconnect_all(Disconnect::Stopping);
        if let Some(discovery) = self.discovery.take() {
            discovery.stop().await;
        }
        let _ = self.presence.flush();
        let _ = self.cache.flush();
        self.shared.subscribers.lock().expect("subscribers").clear();
        self.shared.stopped.send_replace(true);
    }
    fn next_deadline(&self) -> Instant {
        let mut deadline = (self.last_facts + FACTS_INTERVAL).min(self.last_state + STATE_BROADCAST);
        #[cfg(test)]
        if self.hold_facts {
            deadline = self.last_state + STATE_BROADCAST;
        }
        if let Some(at) = self.projection_retry {
            deadline = deadline.min(at);
        }
        if self.pending_revocation.is_some() || self.needs_apply {
            deadline = deadline.min(Instant::now() + Duration::from_millis(50));
        }
        for at in self.recovery_due.values() {
            deadline = deadline.min((*at).max(Instant::now() + Duration::from_millis(100)));
        }
        for link in self.sessions.values() {
            if let Some(at) = link.deadline {
                deadline = deadline.min(at);
            }
        }
        deadline = deadline.min(self.network_checked + Duration::from_secs(2));
        // Only work that tick can perform may wake the actor. A disconnected
        // peer can retain its retry deadline through sleep or inbound reconnect.
        if !self.sleeping {
            for (peer, dial) in &self.dials {
                if !dial.active
                    && self.state.roster().contains(*peer)
                    && !self
                        .sessions
                        .values()
                        .any(|session| session.peer_boot.device_id == *peer)
                    && let Some(at) = dial.after
                {
                    deadline = deadline.min(at);
                }
            }
        }
        for query in self.queries.values() {
            if let Some(at) = query.deadline {
                deadline = deadline.min(at);
            }
        }
        if let Some(at) = self.transactions.next_deadline() {
            deadline = deadline.min(at);
        }
        if let Some(at) = self.proof_objects.next_deadline() {
            deadline = deadline.min(at);
        }
        for expected in self.proof_expected.values() {
            deadline = deadline.min(expected.deadline);
        }
        for (_, at) in self.lookups.values() {
            deadline = deadline.min(*at);
        }
        for seen in self.nearby.values() {
            deadline = deadline.min(seen.at + Duration::from_secs(90));
        }
        for start in self.connecting.values() {
            deadline = deadline.min(start.deadline);
        }
        if let Some(at) = self.pairs.next_deadline() {
            deadline = deadline.min(at);
        }
        deadline
    }
    fn network(&mut self, event: ConfigEvent) {
        match event {
            ConfigEvent::Connected(event) => {
                if self.sleeping {
                    self.links.disconnect(event.link, Disconnect::Sleeping);
                    return;
                }
                let event = *event;
                let id = event.link;
                let peer = event.peer_boot;
                let outbound = event.direction == Direction::Outbound;
                self.dials.entry(peer.device_id).or_default().active = false;
                // A probe reports TLS identity before N0 selects its C0. A
                // repeated query can resolve on the existing C0 and release it
                // before this replacement arrives. Do not retain an outgoing
                // prepared link after its last consumer has gone away.
                if outbound
                    && self.dials[&peer.device_id].released_query
                    && !self.prepared_peer_in_use(peer.device_id, event.remote_address)
                {
                    self.links.disconnect(id, Disconnect::LeavingGroup);
                    return;
                }
                let relay = if event.path != LinkPath::Direct {
                    Some(dto::RelayContext::from_raw(
                        [
                            (1, wire::path(event.path_id).write()),
                            (2, event.route_revision.write()),
                            (3, wire::boot(if outbound { self.links.boot() } else { peer }).write()),
                            (4, wire::boot(if outbound { peer } else { self.links.boot() }).write()),
                        ]
                        .into(),
                    ))
                } else {
                    None
                };
                let result = relay.transpose().and_then(|relay| {
                    admission::GreetingSession::new(
                        self.links.boot(),
                        peer,
                        outbound,
                        self.links.profile().clone(),
                        event.negotiation,
                        relay,
                        self.advert.borrow().clone(),
                        self.state.roster().anchor(),
                        self.state.roster().digest()?,
                        Instant::now(),
                    )
                });
                let Ok((greeting, hello)) = result else {
                    self.links.disconnect(id, Disconnect::HandshakeFailed);
                    return;
                };
                let mut rx = event.control_rx;
                let incoming = self.incoming_tx.clone();
                let reader = tokio::spawn(async move {
                    loop {
                        let value = rx.next().await.map_err(|_| ());
                        let ended = value.is_err();
                        let (tx, ack) = tokio::sync::oneshot::channel();
                        if incoming.send(Incoming::Control(id, value, tx)).is_err() {
                            break;
                        }
                        if ack.await.is_err() || ended {
                            break;
                        }
                    }
                });
                let wake = self.incoming_tx.clone();
                event.control_tx.set_wake(Arc::new(move || {
                    let _ = wake.send(Incoming::Writable(id));
                }));
                self.sessions.insert(
                    id,
                    Link {
                        peer_boot: peer,
                        direction: event.direction,
                        path: event.path,
                        remote: event.remote_address,
                        greeting,
                        peer: None,
                        sent: None,
                        accepted: None,
                        ready: false,
                        admitted_anchor: None,
                        departing: false,
                        tx: event.control_tx,
                        reader,
                        outbox: VecDeque::new(),
                        deadline: Some(Instant::now() + admission::GREETING_TIMEOUT),
                    },
                );
                if let Some(hello) = hello {
                    self.send_bytes(id, hello);
                }
            }
            ConfigEvent::Lost { link, peer, path, .. } => {
                self.drop_link(link);
                let dial = self.dials.entry(peer).or_default();
                dial.last_relayed = matches!(path, LinkPath::Relayed { .. });
                dial.reset(Instant::now());
            }
            ConfigEvent::Ready { link, .. } => {
                if self.sessions.get(&link).is_some_and(|s| s.ready) {
                    let _ = self.publish();
                }
            }
            ConfigEvent::DialFailed { peer, reason } => {
                let dial = self.dials.entry(peer).or_default();
                dial.failed(reason, Instant::now());
                self.fail_connecting(
                    peer,
                    match reason {
                        oink_network::link::DialFailure::IdentityChanged => dto::PairCode::IdentityChanged,
                        oink_network::link::DialFailure::Refused => dto::PairCode::ConnectionRefused,
                        oink_network::link::DialFailure::Stopped => dto::PairCode::LinkLost,
                        _ => dto::PairCode::Unreachable,
                    },
                );
            }
            ConfigEvent::ControlClosed { stream, .. } => self.close(stream.link, Disconnect::HandshakeFailed),
            ConfigEvent::PathChanged { link, path } => {
                if let Some(session) = self.sessions.get_mut(&link) {
                    session.path = path;
                    session.remote = self
                        .links
                        .links()
                        .links
                        .iter()
                        .find(|entry| entry.link == link)
                        .and_then(|entry| entry.remote_address);
                    let peer = session.peer_boot.device_id;
                    // The selected remote socket is authenticated by this direct
                    // path; discovery candidates need not be the address that won.
                    if path == LinkPath::Direct
                        && let Some(address) = session.remote
                        && let Err(error) = self.address_book.remember(peer, address)
                    {
                        tracing::warn!(?error, "could not retain successful direct endpoint");
                    }
                    self.finish_queries(peer);
                }
                let _ = self.publish();
            }
        }
    }
    fn incoming(&mut self, event: Incoming) {
        match event {
            Incoming::Writable(link) => self.flush(link),
            Incoming::Control(link, value, ack) => {
                match value {
                    Ok(bytes) => {
                        if let Err(error) = self.control(link, &bytes) {
                            tracing::debug!(code=?error.code, link=?link,
                                type_id=?v1::frame::OpaqueFrame::parse_exact(&bytes, v1::limits::CONFIG_FRAME).ok().map(|frame| frame.header.type_id),
                                ready=self.sessions.get(&link).is_some_and(|session| session.ready),
                                departing=self.sessions.get(&link).is_some_and(|session| session.departing),
                                "C0 validation failed");
                            self.close(link, Disconnect::HandshakeFailed);
                        }
                    }
                    Err(()) => self.close(link, Disconnect::HandshakeFailed),
                }
                let _ = ack.send(());
            }
            Incoming::Probe {
                operation,
                generation,
                result,
            } => self.probe_result(operation, generation, result),
        }
    }
    fn send_bytes(&mut self, id: LinkId, bytes: Vec<u8>) {
        let Some(link) = self.sessions.get_mut(&id) else { return };
        if link.outbox.len() >= 256 || link.outbox.iter().map(Vec::len).sum::<usize>() + bytes.len() > 4 * 1024 * 1024 {
            self.close(id, Disconnect::HandshakeFailed);
            return;
        }
        link.outbox.push_back(bytes);
        self.flush(id);
    }
    fn send(&mut self, id: LinkId, message: Message) -> v1::Result<()> {
        let bytes = v1::frame::encode(&message, v1::limits::CONFIG_FRAME)?;
        self.send_bytes(id, bytes);
        // Posting can synchronously retire a closed or over-budget C0. Its
        // caller must stop the transition before reading that session again.
        if !self.sessions.contains_key(&id) {
            return Err(v1::ErrorCode::NotReady.into());
        }
        Ok(())
    }
    fn flush(&mut self, id: LinkId) {
        let Some(link) = self.sessions.get_mut(&id) else { return };
        while let Some(bytes) = link.outbox.front() {
            match link.tx.post(bytes.clone()) {
                Ok(()) => {
                    link.outbox.pop_front();
                }
                Err(PostError::Full) => break,
                Err(_) => {
                    self.close(id, Disconnect::HandshakeFailed);
                    break;
                }
            }
        }
    }
    fn peer_link(&self, peer: Boot) -> Option<LinkId> {
        self.selected_session(peer.device_id)
            .filter(|(_, session)| session.peer_boot == peer)
            .map(|(id, _)| id)
    }
    fn selected_session(&self, device: DeviceId) -> Option<(LinkId, &Link)> {
        // Connected/Lost events are asynchronous. An older C0 can still be in
        // this map after N0 has selected its replacement; never send a new
        // pairing step or receipt through that retired lifetime.
        let links = self.links.links();
        let selected = links.by_peer(device)?;
        let session = self.sessions.get(&selected.link)?;
        (session.peer_boot == selected.peer_boot && !session.departing).then_some((selected.link, session))
    }
    fn close(&mut self, id: LinkId, reason: Disconnect) {
        self.links.disconnect(id, reason);
        self.drop_link(id);
    }
    fn drop_link(&mut self, id: LinkId) {
        if let Some(link) = self.sessions.remove(&id) {
            link.reader.abort();
            self.transactions.drop_link(id.0);
            self.proof_objects.drop_link(id.0);
            self.proof_expected.retain(|(link, _), _| *link != id);
            self.lookups.remove(&id);
            let links = self.links.links();
            let replacement = links
                .by_peer(link.peer_boot.device_id)
                .filter(|selected| selected.link != id);
            if !replacement.is_some_and(|selected| selected.peer_boot == link.peer_boot) {
                let effects = self.pairs.lost(link.peer_boot);
                self.pair_effects(effects);
                if self.facts.get(&link.peer_boot.device_id).is_some_and(|facts| {
                    facts.boot().ok().and_then(|boot| wire::read_boot(&boot).ok()) == Some(link.peer_boot)
                }) {
                    self.facts.remove(&link.peer_boot.device_id);
                }
                // A retired boot must not cancel a query already completed on
                // a replacement. Pending queries can use its new greeting too.
                self.fail_queries_matching(
                    link.peer_boot.device_id,
                    replacement.map(|_| link.peer_boot),
                    dto::PairCode::PreparedPeerLost,
                );
            }
            let _ = self.publish();
        }
    }
    fn disconnect_all(&mut self, reason: Disconnect) {
        for link in self.sessions.keys().copied().collect::<Vec<_>>() {
            self.close(link, reason);
        }
    }
    fn control(&mut self, id: LinkId, bytes: &[u8]) -> v1::Result<()> {
        if self.sessions.get(&id).is_some_and(|session| session.departing) {
            return Ok(());
        }
        let peer = self.sessions.get(&id).ok_or(v1::ErrorCode::NotReady)?.peer_boot;
        if self.sessions[&id].peer.is_none() {
            let response = self
                .sessions
                .get_mut(&id)
                .expect("link")
                .greeting
                .receive(bytes, Instant::now());
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    if matches!(
                        error.code,
                        v1::ErrorCode::UnsupportedMajor | v1::ErrorCode::MissingCapability
                    ) {
                        self.incompatible.insert(peer.device_id);
                        let _ = self.publish();
                    }
                    return Err(error);
                }
            };
            self.incompatible.remove(&peer.device_id);
            if let Some(response) = response {
                self.send_bytes(id, response);
            }
            // Posting the reply can synchronously retire a full or closed C0
            // queue. The actor must stop this handshake on that lost lifetime.
            let result = self
                .sessions
                .get(&id)
                .ok_or(v1::ErrorCode::NotReady)?
                .greeting
                .result()
                .cloned()
                .ok_or(v1::ErrorCode::NotReady)?;
            self.links.set_agreement(id, result.agreement.clone());
            self.links.set_peer_formats(id, result.formats.clone());
            self.links.set_peer_audio(id, result.audio.clone());
            let same = result.anchor()? == self.state.roster().anchor();
            let session = self.sessions.get_mut(&id).expect("link");
            session.peer = Some(result);
            session.deadline = None;
            if let Some(address) = session.remote {
                // An incoming manual lookup now uses a dedicated ephemeral
                // source port. Reconnect to its advertised listener, while an
                // outgoing lookup retains the exact port the user queried.
                let listen = if session.direction == Direction::Inbound {
                    SocketAddr::new(
                        address.ip(),
                        session.peer.as_ref().expect("greeting").greeting.advert()?.port()?,
                    )
                } else {
                    address
                };
                let _ = self.address_book.remember(peer.device_id, listen);
            }
            self.finish_queries(peer.device_id);
            self.start_connecting(peer.device_id);
            self.reconcile_peer(id)?;
            let session = self.sessions.get(&id).ok_or(v1::ErrorCode::NotReady)?;
            if same && session.direction == Direction::Outbound {
                self.begin_admission(id)?;
            } else if !same && self.state.roster().contains(peer.device_id) {
                self.lookup(id)?;
            }
            return Ok(());
        }
        let message = v1::frame::OpaqueFrame::parse_exact(bytes, v1::limits::CONFIG_FRAME)?.decode()?;
        let hash = self.sessions[&id].peer.as_ref().expect("greeting").hash()?;
        if let Message::ConfigPairDecisionReply(reply) = &message {
            let pair = wire::read_pair(&reply.pair_id()?)?;
            if self.transactions.contains(pair)
                && reply.outcome()?.variant()? == v1::schema::Open::Known(dto::DecisionKind::Aborted)
                && dto::DecisionAborted::from_raw(reply.outcome()?.body()?)?
                    .problem()?
                    .code()?
                    == dto::PairCode::StateChanged
            {
                let effects = self
                    .transactions
                    .receive(
                        id.0,
                        peer,
                        message,
                        &self.identity,
                        &self.state,
                        &mut self.journal,
                        Instant::now(),
                        now_ms(),
                    )
                    .map_err(|_| v1::ErrorCode::StateConflict)?;
                return self
                    .merge_effects(effects)
                    .map_err(|_| v1::ErrorCode::StateConflict.into());
            }
        }
        let recovery = match &message {
            Message::ConfigPairEnded(value) => value.lookup()?.is_some(),
            Message::ConfigPairReceipt(value) => !self.transactions.contains(wire::read_pair(&value.pair_id()?)?),
            Message::ConfigPairObjectBegin(value) => {
                self.proof_expected.contains_key(&(id, value.object()?.digest()?.0))
            }
            Message::ConfigPairObjectChunk(value) => self.proof_objects.contains(id.0, value.digest()?.0),
            Message::ConfigPairObjectEnd(value) => self.proof_objects.contains(id.0, value.digest()?.0),
            _ => false,
        };
        if let Message::ConfigPairObjectBegin(value) = &message {
            let object = value.object()?;
            let existing = self.proof_objects.contains(id.0, object.digest()?.0)
                || self.transactions.has_object(id.0, object.digest()?.0);
            let bytes = if existing { 0 } else { object.bytes()? as usize };
            if !existing
                && (self.proof_objects.staging_bytes() + self.transactions.staging_bytes() + bytes > 4 * 1024 * 1024
                    || self.proof_objects.staging_count(id.0) + self.transactions.staging_count(id.0) >= 2)
            {
                return Err(v1::ErrorCode::LimitExceeded.into());
            }
        }
        if recovery {
            return self.recovery_message(id, peer, message);
        }
        match message {
            Message::ConfigRequest(request) => {
                if !self.state.roster().contains(peer.device_id) {
                    self.send(
                        id,
                        admission::reply(
                            &self.state,
                            self.identity.id(),
                            hash,
                            self.links.links().ready_count() as u8,
                        )?,
                    )?;
                    self.sessions.get_mut(&id).unwrap().deadline = Some(Instant::now() + Duration::from_millis(200));
                    return Ok(());
                }
                if let Some(candidate) =
                    admission::request_revocation(&self.state, &request, self.identity.id(), peer.device_id, hash)?
                {
                    self.receive_revocation(candidate)?;
                    return Ok(());
                }

                let candidate =
                    admission::receive_request(&self.state, &request, self.identity.id(), peer.device_id, hash)?;
                self.commit(candidate.clone())
                    .map_err(|_| v1::ErrorCode::StateConflict)?;
                self.sessions.get_mut(&id).expect("link").accepted = Some(candidate.clone());
                self.links.set_half_open(id);
                self.sessions.get_mut(&id).expect("link").deadline = Some(Instant::now() + Duration::from_secs(10));
                self.send(
                    id,
                    admission::reply(
                        &candidate,
                        self.identity.id(),
                        hash,
                        self.links.links().ready_count() as u8,
                    )?,
                )?;
            }
            Message::ConfigReply(reply) => {
                let sent = self.sessions[&id].sent.as_ref().ok_or(v1::ErrorCode::StateConflict)?;
                if let Some(candidate) =
                    admission::reply_revocation(&self.state, sent, &reply, self.identity.id(), peer.device_id, hash)?
                {
                    self.receive_revocation(candidate)?;
                    return Ok(());
                }
                let candidate =
                    admission::receive_reply(&self.state, sent, &reply, self.identity.id(), peer.device_id, hash)?;
                self.commit(candidate.clone())
                    .map_err(|_| v1::ErrorCode::StateConflict)?;
                // Done can reach the peer before this owner gets to ready().
                // Its first Facts may then reach N0 immediately. Fence the
                // verified reply's half-open phase before exposing Done, just
                // as the responder fences this phase before exposing Reply.
                self.links.set_half_open(id);
                self.send(id, admission::done(&candidate, hash)?)?;
                self.ready(id)?;
            }
            Message::ConfigDone(done) => {
                admission::receive_done(
                    self.sessions[&id]
                        .accepted
                        .as_ref()
                        .ok_or(v1::ErrorCode::StateConflict)?,
                    &done,
                    hash,
                )?;
                self.ready(id)?;
            }
            Message::ConfigFacts(facts) => {
                if wire::read_boot(&facts.boot()?)? != peer {
                    return Err(v1::ErrorCode::AuthenticationFailed.into());
                }
                if self
                    .retired_anchor(id)
                    .is_some_and(|old| facts.group().is_ok_and(|group| group.0 == old.group.0))
                {
                    // The peer may have queued these before receiving our merge
                    // proof. Drain the old group's facts without publishing them
                    // or closing the C0 needed for signed readmission.
                    return Ok(());
                }
                if !self.sessions[&id].ready || facts.group()?.0 != self.state.roster().anchor().group.0 {
                    return Err(v1::ErrorCode::AuthenticationFailed.into());
                }
                let old = self.facts.get(&peer.device_id);
                if old.is_some_and(|old| old.fact_sequence().unwrap_or(u64::MAX) >= facts.fact_sequence().unwrap_or(0))
                {
                    return Ok(());
                }
                let changed = old.is_none_or(|old| old.capabilities() != facts.capabilities());
                self.cache
                    .observe(facts.clone())
                    .map_err(|_| v1::ErrorCode::StateConflict)?;
                self.facts.insert(peer.device_id, Arc::new(facts));
                self.links
                    .update_addresses(peer.device_id, self.member_candidates(peer.device_id));
                if !self.needs_apply {
                    self.publish().map_err(|_| v1::ErrorCode::StateConflict)?;
                }
                self.shared.emit(ConfigChange::Facts {
                    device: peer.device_id,
                    capabilities_changed: changed,
                });
            }
            Message::ConfigState(state) => {
                if !self.sessions[&id].ready {
                    if let Some(old) = self.retired_anchor(id)
                        && state.group()?.0 == old.group.0
                    {
                        // As with Facts, a valid old-group update can cross our
                        // local retirement. Verify it, but never apply it here.
                        State::verify(state, old)?;
                        return Ok(());
                    }
                    return Err(v1::ErrorCode::NotReady.into());
                }
                let candidate = self.state.receive_update(&state, peer.device_id, self.identity.id())?;
                let local_removed = !candidate.roster().contains(self.identity.id());
                let removed = self
                    .sessions
                    .iter()
                    .filter_map(|(link, s)| {
                        // Absence is a revocation only for a previous member.
                        // A separate direct pairing/prepared-query peer is not
                        // in this roster yet and may wait for human input forever.
                        ((self.state.roster().contains(s.peer_boot.device_id)
                            && !candidate.roster().contains(s.peer_boot.device_id))
                            || local_removed)
                            .then_some(*link)
                    })
                    .collect::<Vec<_>>();
                let digest = candidate.digest()?;
                if let Err(error) = self.commit(candidate) {
                    if !self.needs_apply || self.state.digest()? != digest {
                        return Err(v1::ErrorCode::StateConflict.into());
                    }
                    // The authenticated update is durable. A local native failure
                    // keeps the previous applied view and streams until tick retries;
                    // it does not invalidate the peer's C0 admission.
                    tracing::warn!(%error, "saved group update awaits native application");
                }
                for link in removed {
                    self.close(link, Disconnect::MemberRemoved);
                }
                if local_removed {
                    self.fresh_after_revocation()
                        .map_err(|_| v1::ErrorCode::StateConflict)?;
                }
            }
            Message::ConfigPairRequest(request) => {
                if self.sessions[&id].path != LinkPath::Direct {
                    return Err(v1::ErrorCode::InvalidChannel.into());
                }
                let greeting = self.sessions[&id].peer.as_ref().expect("greeting");
                let pair = wire::read_pair(&request.pair_id()?)?;
                if !self.pairs.knows(pair) {
                    let context = request.context()?;
                    if context.negotiation_hash()?.0 != greeting.hash()? {
                        return Err(v1::ErrorCode::AuthenticationFailed.into());
                    }
                    let source = greeting.anchor()?;
                    let target = self.state.roster().anchor();
                    if context.source_group()?.0 != source.group.0
                        || context.source_root_id()?.0 != source.root
                        || context.target_group()?.0 != target.group.0
                        || context.target_root_id()?.0 != target.root
                        || context.destination_group()?.0 != target.group.0
                        || context.source_count()? != greeting.greeting.advert()?.count()?
                        || context.target_count()? != self.state.roster().members().count() as u8
                    {
                        let effects = self.pairs.refuse(
                            pair,
                            peer,
                            pairing::problem(dto::PairCode::GroupChanged, dto::PairStage::Request),
                        );
                        self.pair_effects(effects);
                        return Ok(());
                    }
                    if source == target {
                        let effects = self.pairs.refuse(
                            pair,
                            peer,
                            pairing::problem(dto::PairCode::AlreadyConnected, dto::PairStage::Request),
                        );
                        self.pair_effects(effects);
                        return Ok(());
                    }
                }
                let blocked =
                    self.blocks.as_ref().is_none_or(|b| b.contains(peer.device_id)) || self.onboarding_pairing_busy();
                let effects = self
                    .pairs
                    .request(request, peer, greeting.transcript.clone(), blocked, Instant::now())
                    .map_err(|_| v1::ErrorCode::AuthenticationFailed)?;
                self.pair_effects(effects);
            }
            Message::ConfigPairPrepare(_)
            | Message::ConfigPairReady(_)
            | Message::ConfigPairCommitPermit(_)
            | Message::ConfigPairCommit(_)
            | Message::ConfigPairCommitted(_)
            | Message::ConfigPairObjectBegin(_)
            | Message::ConfigPairObjectChunk(_)
            | Message::ConfigPairObjectEnd(_)
            | Message::ConfigPairReceipt(_) => {
                if self.sessions[&id].path != LinkPath::Direct {
                    return Err(v1::ErrorCode::InvalidChannel.into());
                }
                let pair = self
                    .transactions
                    .associated_pair(id.0, &message)
                    .ok_or(v1::ErrorCode::StateConflict)?;
                if self.pairs.view(pair).is_some_and(|view| view.peer != peer) {
                    return Err(v1::ErrorCode::AuthenticationFailed.into());
                }
                match self.transactions.receive(
                    id.0,
                    peer,
                    message,
                    &self.identity,
                    &self.state,
                    &mut self.journal,
                    Instant::now(),
                    now_ms(),
                ) {
                    Ok(effects) => {
                        if let Some(view) = self.pairs.view(pair) {
                            let phase = view.phase;
                            if let Ok(effects) = self.pairs.progress(pair, phase, Instant::now()) {
                                self.pair_effects(effects);
                            }
                        }
                        self.merge_effects(effects).map_err(|_| v1::ErrorCode::StateConflict)?;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "pairing merge message failed");
                        let code = match error {
                            oink_network::pair_merge::Error::Journal(oink_network::pair_journal::Error::Io(_)) => {
                                dto::PairCode::LocalStorageFailed
                            }
                            oink_network::pair_merge::Error::Wire(error) => match error.code {
                                v1::ErrorCode::UnsupportedValue => dto::PairCode::UnsupportedLayout,
                                v1::ErrorCode::LimitExceeded => dto::PairCode::MergeTooLarge,
                                v1::ErrorCode::AuthenticationFailed => dto::PairCode::AuthenticationFailed,
                                _ => dto::PairCode::StateChanged,
                            },
                            _ => dto::PairCode::MergeConflict,
                        };
                        self.pair_problem(id, peer, pair, pairing::problem(code, dto::PairStage::Prepare))?;
                    }
                }
            }
            Message::ConfigPairQueued(_)
            | Message::ConfigPairAccept(_)
            | Message::ConfigPairDeclined(_)
            | Message::ConfigPairFeedback(_)
            | Message::ConfigPairPake(_)
            | Message::ConfigPairCancel(_)
            | Message::ConfigPairEnded(_) => {
                let pair = self
                    .transactions
                    .associated_pair(id.0, &message)
                    .ok_or(v1::ErrorCode::StateConflict)?;
                if self.pairs.view(pair).is_some_and(|view| view.peer != peer) {
                    return Err(v1::ErrorCode::AuthenticationFailed.into());
                }
                match self.pairs.receive(peer, message, Instant::now()) {
                    Ok(effects) => self.pair_effects(effects),
                    Err(problem) => self.pair_problem(id, peer, pair, problem)?,
                }
            }
            Message::ConfigMergeLookup(_)
            | Message::ConfigMergeTransfer(_)
            | Message::ConfigMergeReceipt(_)
            | Message::ConfigPairDecisionQuery(_)
            | Message::ConfigPairDecisionReply(_) => self.recovery_message(id, peer, message)?,
            Message::ConfigSetLocalOption(_)
            | Message::ConfigLocalEditResult(_)
            | Message::ConfigSetExitVolume(_)
            | Message::ConfigExitVolumeResult(_)
            | Message::ConfigExitVolumeChanged(_)
            | Message::ConfigPauseDevice(_)
            | Message::ConfigPauseDeviceResult(_)
            | Message::ConfigEmergencyPause(_)
            | Message::ConfigEmergencyPauseAck(_)
            | Message::NotifyShow(_)
            | Message::NotifyAction(_)
            | Message::NotifyDismiss(_) => {
                if !self.sessions[&id].ready || !self.state.roster().contains(peer.device_id) {
                    return Err(v1::ErrorCode::NotReady.into());
                }
                self.shared.emit(ConfigChange::DeviceControl {
                    peer: peer.device_id,
                    message,
                });
            }
            Message::ProtocolError(error) => {
                if self.sessions[&id].ready && self.state.roster().contains(peer.device_id) {
                    self.shared.emit(ConfigChange::ProtocolError {
                        link: id,
                        peer: peer.device_id,
                        error,
                    });
                }
            }
            _ => return Err(v1::ErrorCode::InvalidChannel.into()),
        }
        Ok(())
    }
    fn pair_problem(
        &mut self,
        link: LinkId,
        peer: oink_foundation::session::Boot,
        pair: PairId,
        problem: dto::PairProblem,
    ) -> v1::Result<()> {
        if self.pairs.view(pair).is_some_and(|view| {
            view.peer == peer && !matches!(view.phase, pairing::Phase::Ended | pairing::Phase::Completed)
        }) {
            let effects = self.pairs.fail(pair, problem);
            self.pair_effects(effects);
            Ok(())
        } else {
            self.send(
                link,
                Message::ConfigPairEnded(dto::PairEnded::from_raw(
                    [(1, wire::pair(pair).write()), (2, problem.write())].into(),
                )?),
            )
        }
    }
    fn retired_anchor(&self, id: LinkId) -> Option<Anchor> {
        let session = self.sessions.get(&id)?;
        let old = session.admitted_anchor?;
        (!session.ready
            && old != self.state.roster().anchor()
            && self.state.roster().contains(session.peer_boot.device_id))
        .then_some(old)
    }
    fn begin_admission(&mut self, id: LinkId) -> v1::Result<()> {
        let hash = self.sessions[&id]
            .peer
            .as_ref()
            .ok_or(v1::ErrorCode::NotReady)?
            .hash()?;
        let request = admission::request(
            &self.state,
            self.identity.id(),
            hash,
            self.links.links().ready_count() as u8,
        )?;
        let session = self.sessions.get_mut(&id).expect("link");
        session.sent = Some(self.state.clone());
        session.deadline = Some(Instant::now() + ADMISSION_TIMEOUT);
        self.send(id, request)
    }
    fn ready(&mut self, id: LinkId) -> v1::Result<()> {
        let session = self.sessions.get_mut(&id).ok_or(v1::ErrorCode::NotReady)?;
        session.ready = true;
        session.admitted_anchor = Some(self.state.roster().anchor());
        session.deadline = None;
        let peer = session.peer_boot;
        self.links.set_ready(id);
        self.dials.entry(peer.device_id).or_default().failures = 0;
        self.presence
            .observe(self.state.roster().anchor().group, peer.device_id, now_ms())
            .map_err(|_| v1::ErrorCode::StateConflict)?;
        self.send(id, Message::ConfigFacts(self.local.value().clone()))?;
        let pairs = self
            .pairs
            .snapshot()
            .sessions
            .into_iter()
            .filter(|p| p.peer == peer && self.transactions.bundle(p.id).is_some())
            .map(|p| p.id)
            .collect::<Vec<_>>();
        for pair in pairs {
            let effects = self
                .transactions
                .admitted(pair)
                .map_err(|_| v1::ErrorCode::StateConflict)?;
            self.merge_effects(effects).map_err(|_| v1::ErrorCode::StateConflict)?;
        }
        let current = self.state.roster().anchor();
        if self.lookups.get(&id).is_some_and(|(source, _)| *source == current) {
            self.lookups.remove(&id);
            self.migration(current, Some(current), api::MigrationPhase::Adopted, None);
        }
        self.recovery_ready(id)?;
        self.publish().map_err(|_| v1::ErrorCode::StateConflict)?;
        Ok(())
    }
    fn tick(&mut self) {
        let now = Instant::now();
        if now >= self.network_checked + Duration::from_secs(2) {
            self.network_checked = now;
            match self.observe_endpoint_facts() {
                Ok(true) => {
                    for dial in self.dials.values_mut().filter(|dial| !dial.active) {
                        dial.reset(now);
                    }
                    if let Some(discovery) = &self.discovery {
                        discovery.refresh();
                    }
                    if let Err(error) = self.publish_facts() {
                        tracing::debug!(?error, "endpoint facts publication deferred");
                    }
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(?error, "could not refresh local endpoint facts"),
            }
        }
        if let Some(candidate) = self.pending_revocation.take() {
            if self.commit(candidate.clone()).is_err() {
                self.pending_revocation = Some(candidate);
            } else {
                let _ = self.displays_changed(self.displays.clone());
            }
        }
        if self.projection_retry.is_some_and(|at| now >= at) {
            let _ = self.merge_effects(Vec::new());
        }
        if self.needs_apply && self.projection_retry.is_none() {
            let _ = self.commit(self.state.clone());
        }
        if self
            .nearby
            .values()
            .any(|seen| now >= seen.at + Duration::from_secs(90))
        {
            self.publish_nearby();
        }
        let recovery_peers = self
            .sessions
            .iter()
            .filter_map(|(id, s)| (!s.departing && s.peer.is_some()).then_some(*id))
            .collect::<Vec<_>>();
        for id in recovery_peers {
            let _ = self.reconcile_peer(id);
        }
        self.expire_recovery(now);
        for pair in self.transactions.expire(now) {
            let effects = self.pairs.fail(
                pair,
                pairing::problem(dto::PairCode::NetworkTimeout, dto::PairStage::Prepare),
            );
            self.pair_effects(effects);
        }
        if let Ok(effects) = self
            .transactions
            .retry_permits(&self.identity, &self.state, &mut self.journal)
        {
            let _ = self.merge_effects(effects);
        }
        let effects = self.pairs.tick(now);
        self.pair_effects(effects);
        for id in self
            .sessions
            .iter()
            .filter_map(|(id, l)| l.deadline.filter(|at| now >= *at).map(|_| *id))
            .collect::<Vec<_>>()
        {
            self.close(id, Disconnect::HandshakeFailed);
        }
        for peer in self
            .connecting
            .values()
            .filter(|c| now >= c.deadline)
            .map(|c| c.device)
            .collect::<Vec<_>>()
        {
            self.fail_connecting(peer, dto::PairCode::ConnectTimeout);
        }
        self.expire_queries(now);
        let publish_periodic = now >= self.last_facts + FACTS_INTERVAL;
        #[cfg(test)]
        let publish_periodic = publish_periodic && !self.hold_facts;
        if publish_periodic {
            self.last_facts = now;
            let _ = self.publish_facts();
            let _ = self.presence.flush();
            let _ = self.cache.flush();
        }
        if now >= self.last_state + STATE_BROADCAST {
            self.last_state = now;
            self.broadcast(Message::ConfigState(self.state.wire().clone()));
        }
        if !self.sleeping {
            let peers = self
                .state
                .roster()
                .members()
                .filter(|m| m.device != self.identity.id())
                .map(|m| m.device)
                .collect::<Vec<_>>();
            for peer in peers {
                if self.sessions.values().any(|l| l.peer_boot.device_id == peer) {
                    continue;
                }
                let addresses = self.member_candidates(peer);
                let dial = self.dials.entry(peer).or_default();
                if !dial.due(self.identity.id(), peer, now) {
                    continue;
                }
                let mode = if dial.last_relayed {
                    DialMode::All
                } else {
                    DialMode::PreferDirect
                };
                match self.links.dial(peer, addresses, mode) {
                    Ok(()) => {
                        dial.active = true;
                        dial.after = None;
                    }
                    Err(_) => dial.after = Some(now + REDIAL_INTERVAL),
                }
            }
        }
    }
    fn broadcast(&mut self, message: Message) {
        for id in self
            .sessions
            .iter()
            .filter_map(|(id, l)| l.ready.then_some(*id))
            .collect::<Vec<_>>()
        {
            let _ = self.send(id, message.clone());
        }
    }
    fn member_candidates(&self, peer: DeviceId) -> Vec<SocketAddr> {
        // Keep successful/manual endpoints first. Facts arrive over authenticated
        // relayed links too, so discovery need not cross a subnet to upgrade one.
        // Cached facts remain address hints only; TLS still verifies the target.
        let mut addresses = self.address_book.candidates(peer);
        let cached = self.cache.get(self.state.roster().anchor().group, peer);
        if let Some(facts) = self.facts.get(&peer).map(AsRef::as_ref).or(cached.as_ref())
            && let Ok(endpoints) = facts.endpoints()
        {
            for endpoint in endpoints.0 {
                if endpoint.family() != Ok(dto::AddressFamily::Ipv4) {
                    continue;
                }
                let (Ok(bytes), Ok(port)) = (endpoint.address(), endpoint.port()) else {
                    continue;
                };
                let Ok(octets) = <[u8; 4]>::try_from(bytes.0.as_slice()) else {
                    continue;
                };
                let ip = std::net::Ipv4Addr::from(octets);
                if !ip.is_unspecified() && !ip.is_multicast() && ip != std::net::Ipv4Addr::BROADCAST {
                    addresses.push(SocketAddr::from((ip, port)));
                }
            }
        }
        if let Some(seen) = self.nearby.get(&peer) {
            addresses.extend(&seen.sighting.addrs);
        }
        let mut seen = BTreeSet::new();
        addresses.retain(|address| seen.insert(*address));
        addresses
    }
    fn sighting(&mut self, sighting: Sighting) {
        let peer = sighting.advert.id;
        if peer == self.identity.id() {
            return;
        }
        if self.nearby.len() >= 4096 && !self.nearby.contains_key(&peer) {
            return;
        }
        let changed = self
            .nearby
            .get(&peer)
            .is_none_or(|seen| seen.sighting.advert != sighting.advert || seen.sighting.addrs != sighting.addrs);
        self.nearby.insert(
            peer,
            Seen {
                sighting,
                at: Instant::now(),
            },
        );
        if self.state.roster().contains(peer) {
            self.links.update_addresses(peer, self.member_candidates(peer));
            // Repeated beacons must not defeat admission-failure backoff.
            let dial = self.dials.entry(peer).or_default();
            if changed && !dial.active && dial.failures > 0 {
                dial.reset(Instant::now());
            }
        }
        self.publish_nearby();
    }
    pub(super) fn sleep(&mut self) {
        self.sleeping = true;
        self.disconnect_all(Disconnect::Sleeping);
    }
    pub(super) fn wake(&mut self) {
        self.sleeping = false;
        self.network_checked = Instant::now() - Duration::from_secs(2);
        for dial in self.dials.values_mut() {
            dial.reset(Instant::now());
        }
        if let Some(discovery) = &self.discovery {
            discovery.refresh();
        }
        self.tick();
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
pub(crate) mod tests;
