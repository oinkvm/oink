//! Immutable link authority and one bounded N1 channel per physical next hop.
use super::*;
use crate::link::control::{NextHopBinding, PathInstall};
use arc_swap::{ArcSwap, ArcSwapOption};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub link: super::super::LinkId,
    pub peer: Boot,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Grant {
    pub endpoint: Option<Endpoint>,
    pub neighbor: Boot,
    pub binding: LowBinding,
    pub next_hop_address: SocketAddr,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Route {
    pub agreement: Option<oink_protocol::v1::admission::Agreement>,
    pub endpoint: Option<Endpoint>,
    pub wire: PathInstall,
    pub source: Boot,
    pub target: Boot,
    pub hops: Vec<ConnectionId>,
    pub ready: bool,
}
#[derive(Default)]
pub(crate) struct View {
    pub routes: Vec<Route>,
    pub hops: Vec<Arc<N1>>,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Status {
    pub connection: ConnectionId,
    pub available: bool,
    pub failure: Option<crate::LowUnavailable>,
}
pub(crate) struct Hub {
    pub view: ArcSwap<View>,
    pub changed: Notify,
    pub status: ArcSwap<Vec<Status>>,
    pub failures: ArcSwap<HashMap<LowBinding, crate::LowUnavailable>>,
    pub stopped: AtomicBool,
    pub observed: ArcSwap<HashMap<ConnectionId, Option<Duration>>>,
    pub wake: Arc<Notify>,
}
impl Hub {
    pub fn new(wake: Arc<Notify>) -> Self {
        Self {
            view: ArcSwap::from_pointee(View::default()),
            changed: Notify::new(),
            status: ArcSwap::from_pointee(vec![]),
            failures: ArcSwap::from_pointee(HashMap::new()),
            stopped: AtomicBool::new(false),
            observed: ArcSwap::from_pointee(HashMap::new()),
            wake,
        }
    }
}
pub(crate) struct ControlInfo {
    pub grant: Grant,
    pub message: LowMsg,
}
pub(crate) struct ControlQueued {
    pub send: ControlSend,
}
pub(crate) struct ControlSend {
    pub grant: Grant,
    pub message: LowMsg,
    pub reply: tokio::sync::oneshot::Sender<Result<(), NetError>>,
}
pub(crate) struct Delivery {
    pub grant: Grant,
    pub forward: Option<Grant>,
    pub message: LowMsg,
    pub refusal: Option<String>,
}
pub(crate) struct N1 {
    pub connection: ConnectionId,
    pub neighbor: Boot,
    pub address: SocketAddr,
    pub controls: mpsc::Sender<ControlQueued>,
    pub advert: Arc<ArcSwapOption<ControlInfo>>,
    pub input: mpsc::Sender<Delivery>,
    pub receiver: Mutex<Option<mpsc::Receiver<Delivery>>>,
}
impl N1 {
    pub fn new(
        connection: ConnectionId,
        neighbor: Boot,
        address: SocketAddr,
    ) -> (Arc<Self>, mpsc::Receiver<ControlQueued>) {
        let (controls, output) = mpsc::channel(64);
        let (input, receiver) = mpsc::channel(64);
        (
            Arc::new(Self {
                connection,
                neighbor,
                address,
                controls,
                advert: Arc::new(ArcSwapOption::empty()),
                input,
                receiver: Mutex::new(Some(receiver)),
            }),
            output,
        )
    }
}
pub(crate) struct Slot {
    pub slot: mpsc::OwnedPermit<ControlQueued>,
    pub wake: Arc<Notify>,
}
impl Slot {
    pub fn send(self, send: ControlSend) {
        self.slot.send(ControlQueued { send });
        self.wake.notify_one();
    }
}
pub(crate) struct Port {
    pub boot: Boot,
    pub hub: Arc<Hub>,
    pub commands: mpsc::UnboundedSender<super::super::handle::Command>,
}
impl Port {
    pub fn agreement(&self, binding: LowBinding) -> Option<oink_protocol::v1::admission::Agreement> {
        self.hub
            .view
            .load()
            .routes
            .iter()
            .find(|r| r.wire.path_id == binding.path_id && r.wire.route_revision == binding.route_revision)?
            .agreement
            .clone()
    }
    pub fn admit_kind(&self, grant: Grant, kind: StreamKind) -> bool {
        use oink_protocol::v1::{
            admission::{Context, Phase},
            registry::Transport,
        };
        let Some(agreement) = self.agreement(grant.binding) else {
            return false;
        };
        Context {
            kind: kind as u16,
            transport: Transport::Low,
            phase: Phase::Member,
            agreement: &agreement,
            direct: grant.endpoint.is_some_and(|e| e.peer == grant.neighbor),
            old_member_lane: false,
        }
        .admit_low_open(kind as u16)
        .is_ok()
    }

    pub fn grant(&self, binding: LowBinding) -> Option<Grant> {
        let view = self.hub.view.load();
        let hop = view.hops.iter().find(|h| h.connection == binding.hop.connection_id)?;
        if binding.hop.peer != self.boot.device_id && binding.hop.peer != hop.neighbor.device_id {
            return None;
        }
        let route = view.routes.iter().find(|r| {
            r.wire.path_id == binding.path_id
                && r.wire.route_revision == binding.route_revision
                && r.wire.next_hop_binding.route_tag == binding.hop.route_tag
                && r.hops.contains(&binding.hop.connection_id)
                && (r.source == binding.source && r.target == binding.target
                    || r.source == binding.target && r.target == binding.source)
        })?;
        Some(Grant {
            endpoint: route.endpoint,
            neighbor: hop.neighbor,
            binding,
            next_hop_address: hop.address,
        })
    }
    pub fn outgoing(&self, link: super::super::LinkId) -> Option<Grant> {
        let view = self.hub.view.load();
        let route = view
            .routes
            .iter()
            .find(|r| r.ready && r.endpoint.is_some_and(|e| e.link == link))?;
        let endpoint = route.endpoint?;
        let connection = *route.hops.first()?;
        let hop = view.hops.iter().find(|h| h.connection == connection)?;
        self.grant(LowBinding {
            path_id: route.wire.path_id,
            route_revision: route.wire.route_revision,
            source: self.boot,
            target: endpoint.peer,
            hop: NextHopBinding {
                peer: hop.neighbor.device_id,
                connection_id: connection,
                route_tag: route.wire.next_hop_binding.route_tag,
            },
        })
    }
    pub fn low_current(&self, grant: Grant) -> bool {
        !self.hub.stopped.load(Ordering::Acquire) && self.grant(grant.binding) == Some(grant)
    }
    pub async fn validate_low(&self, grant: Grant) -> bool {
        self.low_current(grant)
    }
    pub async fn cancelled(&self, grant: Grant) {
        loop {
            let changed = self.hub.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.low_current(grant) {
                return;
            }
            changed.await;
        }
    }
    pub fn hop(&self, grant: Grant) -> Result<Arc<N1>, NetError> {
        self.hub
            .view
            .load()
            .hops
            .iter()
            .find(|h| h.connection == grant.binding.hop.connection_id)
            .cloned()
            .ok_or(NetError::Closed)
    }
    pub async fn set_low_probe_active(&self, connection: ConnectionId, active: bool) {
        let _ = self
            .commands
            .send(super::super::handle::Command::Probe(connection, active));
        self.hub.wake.notify_one();
    }
}
