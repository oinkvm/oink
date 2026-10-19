//! Physical slot accounting is per next hop and requester, independent of the
//! logical session which currently authorizes an operation on that slot.
use super::*;
use std::sync::{
    Weak,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};

pub(super) const REGULAR_CAP: usize = 2;
pub(super) const EXTRA_CAP: usize = 1;
pub(super) const IDLE: Duration = Duration::from_secs(30);
pub(super) type RequestId = [u8; 16];
pub(super) type RaceChoice = Arc<watch::Sender<Option<bool>>>;
pub(super) fn short(options: Option<LowOptions>) -> bool {
    options.is_some_and(|o| o.valid() && o.operation_max < EXTRA_MAX)
}
pub(super) struct Queued(pub Arc<AtomicUsize>);
impl Queued {
    pub fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}
impl Drop for Queued {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
pub(super) struct Pending {
    pub request: LowRequest,
    pub authorization: Authorization,
    pub outgoing: bool,
    pub reverse: bool,
    pub race: Option<RaceChoice>,
    pub permit: OwnedSemaphorePermit,
    pub reply: Option<mpsc::Sender<ReplyMessage>>,
}
pub(super) enum ReplyMessage {
    Ack(Result<(), String>),
    Stream(Box<Hop>),
}
pub(super) struct Active {
    pub life: Weak<io::Life>,
    pub binding: LowBinding,
    pub purpose: LowTcpPurpose,
    pub connection: RequestId,
    pub outgoing: bool,
    pub extra: bool,
    pub idle: bool,
    pub keep_alive: bool,
}
impl Active {
    pub fn upgrade(&self) -> Option<Arc<io::Life>> {
        self.life.upgrade()
    }
}
pub(super) struct Idle {
    pub authorization: Authorization,
    pub stream: io::Stream,
    pub life: Arc<io::Life>,
    pub request: LowRequest,
    pub dialer: DeviceId,
}
#[derive(Clone)]
pub(super) struct Advert {
    pub paced: bool,
    pub port: Option<u16>,
    pub failure: Option<String>,
}
#[derive(Clone)]
pub(super) enum WarmState {
    Cold,
    Opening,
    Ready,
    Failed(String),
}
pub(super) struct PoolState {
    pub advert: Option<Advert>,
    pub pending: HashMap<RequestId, Pending>,
    pub incoming_race: Option<RequestId>,
    pub active: HashMap<RequestId, Active>,
    pub idle: Vec<Idle>,
    pub outgoing: Arc<Semaphore>,
    pub incoming: Arc<Semaphore>,
    pub extra_slots: Arc<Semaphore>,
    pub queued: Arc<AtomicUsize>,
    pub warm: WarmState,
    pub warm_binding: Option<LowBinding>,
    pub warm_waiters: Arc<AtomicUsize>,
    pub warm_time: Option<Duration>,
    pub prefer_reverse: bool,
    pub direction_known: bool,
    pub direction_addr: Option<std::net::SocketAddr>,
    pub generation: u64,
    pub opened: u64,
    pub reused: u64,
    pub dialer: Option<DeviceId>,
    pub first_failure: Option<String>,
}
impl Default for PoolState {
    fn default() -> Self {
        Self {
            advert: None,
            pending: HashMap::new(),
            incoming_race: None,
            active: HashMap::new(),
            idle: Vec::new(),
            outgoing: Arc::new(Semaphore::new(REGULAR_CAP)),
            incoming: Arc::new(Semaphore::new(256)),
            extra_slots: Arc::new(Semaphore::new(EXTRA_CAP)),
            queued: Arc::new(AtomicUsize::new(0)),
            warm: WarmState::Cold,
            warm_binding: None,
            warm_waiters: Arc::new(AtomicUsize::new(0)),
            warm_time: None,
            prefer_reverse: false,
            direction_known: false,
            direction_addr: None,
            generation: 0,
            opened: 0,
            reused: 0,
            dialer: None,
            first_failure: None,
        }
    }
}
impl PoolState {
    pub fn forget_direction(&mut self) {
        self.prefer_reverse = false;
        self.direction_known = false;
        self.direction_addr = None;
        self.first_failure = None;
    }
    pub fn close(&mut self) {
        self.generation = self.generation.checked_add(1).expect("pool generation exhausted");
        self.warm = WarmState::Cold;
        self.warm_binding = None;
        self.warm_time = None;
        self.forget_direction();
        self.pending.clear();
        self.incoming_race = None;
        self.idle.clear();
        for life in self.active.drain().filter_map(|(_, a)| a.upgrade()) {
            life.close();
        }
    }
    pub fn reset_bootstrap(&mut self) {
        self.generation = self.generation.checked_add(1).expect("pool generation exhausted");
        self.warm = WarmState::Cold;
        self.warm_binding = None;
        self.warm_time = None;
        self.forget_direction();
        self.pending.retain(|_, p| p.request.purpose == LowTcpPurpose::Transfer);
        self.active.retain(|_, a| {
            if a.idle || a.purpose != LowTcpPurpose::Transfer {
                if let Some(life) = a.upgrade() {
                    life.close();
                }
                false
            } else {
                true
            }
        });
        self.idle.clear();
    }
    pub fn cancel_scope(&mut self, binding: LowBinding) {
        self.pending.retain(|_, p| !same_route(p.request.binding, binding));
        self.active.retain(|_, a| {
            if same_route(a.binding, binding) {
                if let Some(life) = a.upgrade() {
                    life.close();
                }
                false
            } else {
                true
            }
        });
        self.idle.retain(|i| i.life.active());
        if self.warm_binding.is_some_and(|owner| same_route(owner, binding)) {
            // Only the route which owns this warm attempt may retire it.
            // A replacement can already be warming on this shared pool.
            self.generation = self.generation.checked_add(1).expect("pool generation exhausted");
            self.warm = WarmState::Cold;
            self.warm_binding = None;
            self.warm_time = None;
        }
    }
    pub fn incoming_room(&mut self, request: &LowRequest) -> bool {
        self.active.retain(|_, a| a.upgrade().is_some_and(|l| l.active()));
        if request.keep_alive
            && (self
                .active
                .values()
                .any(|a| !a.outgoing && a.keep_alive && a.connection != request.connection)
                || self
                    .pending
                    .values()
                    .any(|p| !p.outgoing && p.request.keep_alive && p.request.connection != request.connection))
        {
            return false;
        }
        if request.connection != request.request {
            return self
                .active
                .values()
                .any(|a| !a.outgoing && a.connection == request.connection && a.extra == request.extra)
                && !self
                    .pending
                    .values()
                    .any(|p| p.request.connection == request.connection);
        }
        let cap = if request.extra { EXTRA_CAP } else { REGULAR_CAP };
        let count = self
            .active
            .values()
            .filter(|a| !a.outgoing && a.extra == request.extra)
            .count()
            + self
                .pending
                .values()
                .filter(|p| {
                    !p.outgoing && p.request.connection == p.request.request && p.request.extra == request.extra
                })
                .count();
        if count < cap {
            return true;
        }
        let evict = self
            .active
            .iter()
            .find(|(_, a)| {
                !a.outgoing
                    && a.extra == request.extra
                    && a.idle
                    && !self.pending.values().any(|p| p.request.connection == a.connection)
            })
            .map(|(id, _)| *id);
        if let Some(id) = evict {
            if let Some(life) = self.active.remove(&id).and_then(|a| a.upgrade()) {
                life.close();
            }
            return true;
        }
        false
    }
    pub fn borrow_idle(&mut self, extra: bool) -> Option<Idle> {
        self.idle.retain(|i| i.life.active());
        self.idle
            .iter()
            .position(|i| i.request.extra == extra)
            .map(|index| self.idle.swap_remove(index))
    }
}
