//! The existing TCP/TLS pool rebound to links. All locks and tasks here live on
//! the ordinary runtime; the network thread exchanges N1 bytes and immutable views.
use super::{LinkId, LowError, LowStream};
use crate::link::control::{LowBinding, LowMsg, LowRequest};
use crate::{LowOptions, LowTcpPurpose};
use crate::{NetError, tls};
use oink_foundation::{
    DeviceId,
    session::{Boot, ConnectionId},
};
use oink_protocol::frame::StreamKind;
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore, mpsc, watch};
pub(super) mod port;
use port::{ControlInfo, ControlSend, Endpoint, Grant, Port, Slot, Status};
pub(super) use port::{Hub, N1, Route, View};
mod physical;
mod scope;
use physical::*;
mod sockets;
use sockets::{Hop, read_setup, write_setup};
mod bootstrap;
pub(super) mod bulk;
mod io;
use bootstrap::choice;
mod leases;
mod recycle;
mod relay;
const EXTRA_MAX: u64 = 4 * 1024 * 1024;
type Reply<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
async fn wait_closed(done: &mut watch::Receiver<bool>) {
    if !*done.borrow() {
        let _ = done.changed().await;
    }
}
struct Config {
    boot: Boot,
    creds: Arc<tls::Credentials>,
    bind_ip: IpAddr,
    port: u16,
    pacing: bool,
    activity: Arc<crate::activity::Activity>,
    dial_timeout: Duration,
}
#[derive(Clone)]
struct Authorization {
    network: Arc<Port>,
    grant: Grant,
    forward: Option<Grant>,
    gate: Arc<scope::Gate>,
    tasks: Arc<watch::Sender<usize>>,
    lives: Arc<Mutex<Vec<Weak<io::Life>>>>,
}
impl Authorization {
    fn current(&self) -> bool {
        self.network.low_current(self.grant)
            && self.forward.is_none_or(|grant| self.network.low_current(grant))
            && !*self.gate.closed.borrow()
    }
    fn with_current<T>(&self, action: impl FnOnce() -> Option<T>) -> Option<T> {
        if !self.current() {
            return None;
        }
        self.gate.with_current(action)
    }
    async fn cancelled(&self) {
        let mut closed = self.gate.closed.subscribe();
        let forward = self.forward;
        tokio::select! {
            _=wait_closed(&mut closed)=>{},
            _=self.network.cancelled(self.grant)=>{},
            _=async {if let Some(forward)=forward {self.network.cancelled(forward).await;}else{std::future::pending::<()>().await;}}=>{},
        }
    }
    fn bind(&self, life: Arc<io::Life>) -> Result<(), NetError> {
        {
            let mut lives = lock(&self.lives);
            lives.retain(|life| life.strong_count() != 0);
            lives.push(Arc::downgrade(&life));
        }
        self.gate.bind(life.clone())?;
        Ok(())
    }
    fn work(&self) -> Option<Work> {
        self.tasks
            .send_if_modified(|n| {
                if !self.current() {
                    return false;
                }
                *n += 1;
                true
            })
            .then(|| Work(self.tasks.clone()))
    }
    async fn drained(&self) {
        let mut tasks = self.tasks.subscribe();
        while *tasks.borrow_and_update() != 0 {
            if tasks.changed().await.is_err() {
                break;
            }
        }
        let lives: Vec<_> = lock(&self.lives).iter().filter_map(Weak::upgrade).collect();
        for life in lives {
            life.drained().await;
        }
    }
}
struct Work(Arc<watch::Sender<usize>>);
impl Drop for Work {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n -= 1);
    }
}
struct Pool {
    key: PoolKey,
    state: Mutex<PoolState>,
    grants: Mutex<HashMap<LowBinding, Authorization>>,
    pacer: Arc<crate::pace::Pacer>,
    closed: watch::Sender<bool>,
    notify: Notify,
    failure: Mutex<Option<crate::LowUnavailable>>,
}
impl Pool {
    fn current(&self) -> bool {
        !*self.closed.borrow()
    }
    fn close(&self) {
        self.closed.send_replace(true);
        for auth in lock(&self.grants).drain().map(|(_, a)| a) {
            auth.gate.cancel();
        }
        lock(&self.state).close();
        self.notify.notify_waiters();
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.close();
    }
}
use scope::PoolKey;
struct Core {
    cfg: Config,
    network: Arc<Port>,
    pools: Mutex<HashMap<PoolKey, Arc<Pool>>>,
    admission: Mutex<u64>,
    inbound: Mutex<HashSet<ConnectionId>>,
    pacers: Mutex<HashMap<ConnectionId, Arc<crate::pace::Pacer>>>,
    listener: Mutex<Option<SocketAddr>>,
    listener_failure: Mutex<Option<String>>,
    runtime: tokio::runtime::Handle,
    stopped: watch::Sender<bool>,
    shutdown_done: watch::Sender<bool>,
    background: Arc<watch::Sender<usize>>,
    me: Weak<Core>,
}
pub(super) struct LowPool {
    core: Arc<Core>,
}
impl std::fmt::Debug for LowPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LowPool")
    }
}
impl LowPool {
    /// Ordinary-runtime diagnostics only; this snapshot grants no permission.
    pub fn status(&self, link: super::LinkId) -> Option<crate::TcpBulkStatus> {
        let pool = lock(&self.core.pools)
            .values()
            .find(|p| {
                lock(&p.grants)
                    .values()
                    .any(|a| a.grant.endpoint.is_some_and(|e| e.link == link))
            })?
            .clone();
        let st = lock(&pool.state);
        let live: Vec<_> = st
            .active
            .values()
            .filter(|a| a.upgrade().is_some_and(|l| l.active()))
            .collect();
        let active = live.iter().filter(|a| !a.idle).count();
        let idle = live.len() - active;
        let failure = lock(&pool.failure).clone();
        let phase = if active + idle > 0 {
            crate::TcpBulkPhase::Ready
        } else if failure.is_some() {
            crate::TcpBulkPhase::Unavailable
        } else if st.pending.is_empty() {
            crate::TcpBulkPhase::OnDemand
        } else if st.pending.values().any(|p| p.reverse) {
            crate::TcpBulkPhase::ReverseDialing
        } else {
            crate::TcpBulkPhase::Dialing
        };
        Some(crate::TcpBulkStatus {
            phase,
            active_connections: active,
            idle_connections: idle,
            opening_connections: st.pending.len(),
            queued_requests: st.queued.load(std::sync::atomic::Ordering::Relaxed),
            regular_cap: REGULAR_CAP as u8,
            extra_cap: EXTRA_CAP as u8,
            connections_opened: st.opened,
            connections_reused: st.reused,
            prefer_reverse: st.prefer_reverse,
            warm_ready: matches!(st.warm, WarmState::Ready),
            warm_time: st.warm_time,
            dialer: st.dialer,
            cipher: (active + idle > 0).then_some("TLS_AES_128_GCM_SHA256"),
            last_failure: failure,
            first_dial_failure: st.first_failure.clone(),
            pacing: (self.core.cfg.pacing || st.advert.as_ref().is_some_and(|a| a.paced))
                .then(|| pool.pacer.snapshot()),
        })
    }
    pub(super) fn tcp_inbound_verified(&self) -> bool {
        !*self.core.stopped.borrow() && lock(&self.core.listener).is_some() && !lock(&self.core.inbound).is_empty()
    }
    pub fn new(
        config: &super::LinkConfig,
        address: SocketAddr,
        creds: Arc<tls::Credentials>,
        hub: Arc<Hub>,
        commands: mpsc::UnboundedSender<super::handle::Command>,
        activity: Arc<crate::activity::Activity>,
    ) -> Arc<Self> {
        let network = Arc::new(Port {
            boot: config.boot,
            hub,
            commands,
        });
        let core = Arc::new_cyclic(|me| Core {
            cfg: Config {
                boot: config.boot,
                creds,
                bind_ip: address.ip(),
                port: address.port(),
                pacing: true,
                activity,
                dial_timeout: Duration::from_secs(3),
            },
            network,
            pools: Mutex::new(HashMap::new()),
            admission: Mutex::new(0),
            inbound: Mutex::new(HashSet::new()),
            pacers: Mutex::new(HashMap::new()),
            listener: Mutex::new(None),
            listener_failure: Mutex::new(None),
            runtime: tokio::runtime::Handle::current(),
            stopped: watch::channel(false).0,
            shutdown_done: watch::channel(false).0,
            background: Arc::new(watch::channel(0).0),
            me: me.clone(),
        });
        let worker = core.clone();
        core.runtime.spawn(async move {
            loop {
                let changed = worker.network.hub.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if worker.network.hub.stopped.load(std::sync::atomic::Ordering::Acquire) {
                    worker.stop().await;
                    worker.shutdown_done.send_replace(true);
                    break;
                }
                worker.reconcile().await;
                tokio::select! {
                    _ = changed => {},
                    _ = worker.cfg.activity.changed.notified() => {
                        for pacer in lock(&worker.pacers).values() { pacer.activity_changed(); pacer.report(); }
                    }
                }
            }
        });
        Arc::new(Self { core })
    }
    pub async fn wait_stopped(&self) {
        wait_closed(&mut self.core.shutdown_done.subscribe()).await;
    }
    pub async fn open(&self, link: LinkId, kind: StreamKind, options: LowOptions) -> Result<LowStream, LowError> {
        let grant = self.core.network.outgoing(link).ok_or_else(|| {
            let view = self.core.network.hub.view.load();
            let endpoint = view.routes.iter().find_map(|r| r.endpoint.filter(|e| e.link == link));
            match endpoint {
                Some(endpoint) => LowError::Unavailable(crate::LowUnavailable {
                    from: self.core.cfg.boot.device_id,
                    to: endpoint.peer.device_id,
                    reason: crate::LowFailureReason::Tcp {
                        stage: "admission".into(),
                        detail: "low-priority admission is unavailable".into(),
                    },
                }),
                None => LowError::Interrupted,
            }
        })?;
        if !self.core.network.admit_kind(grant, kind) {
            return Err(LowError::Interrupted);
        }
        if !options.valid() {
            return Err(LowError::Budget);
        }
        let (pool, auth) = self.core.admit(grant, None).await.map_err(LowError::from)?;
        self.core
            .wait_warm(pool.clone(), auth.clone())
            .await
            .map_err(LowError::from)?;
        let hop = self
            .core
            .open_lease(
                pool,
                auth,
                rand::random::<u64>().max(1),
                Some(options),
                LowTcpPurpose::Transfer,
                None,
                kind as u16,
                None,
            )
            .await
            .map_err(LowError::from)?;
        let stream = self
            .core
            .endpoint_after_relay(hop, grant.endpoint.expect("endpoint").peer.device_id)
            .await
            .map_err(LowError::from)?;
        Ok(LowStream::new(stream))
    }
}
fn same_route(a: LowBinding, b: LowBinding) -> bool {
    a.path_id == b.path_id && a.route_revision == b.route_revision
}
impl Core {
    fn arc(&self) -> Arc<Self> {
        self.me.upgrade().expect("live pool")
    }
    async fn reconcile(self: &Arc<Self>) {
        let view = self.network.hub.view.load_full();
        for hop in &view.hops {
            if let Some(mut input) = lock(&hop.receiver).take() {
                let core = self.clone();
                self.spawn_background(async move{let mut stopped=core.stopped.subscribe();loop{let delivery=tokio::select!{biased;_=wait_closed(&mut stopped)=>break,item=input.recv()=>item};let Some(delivery)=delivery else{break;};core.network.hub.wake.notify_one();
                // Register the exact incoming grant before handing control to its
                // handler. Route cleanup cannot mistake queued replacement work
                // for an unreferenced pool (rollback fixes A3/T1).
                let Ok((pool,auth))=core.admit(delivery.grant,delivery.forward).await else{continue;};if let Some(reason)=delivery.refusal { if let LowMsg::Open(request)=delivery.message {let _=core.send(delivery.grant,LowMsg::Ready{request:request.request,result:Err(reason)}).await;}continue;}let cancelled=auth.clone();tokio::select!{biased;_=cancelled.cancelled()=>{},_=core.receive_authorized(pool,auth,delivery.message)=>{}}
            }});
            }
        }
        // Acquire replacement route grants before cancelling old ones. Pools are
        // keyed by the physical neighbor, never by the end-to-end route.
        for route in &view.routes {
            if route.ready
                && let Some(endpoint) = route.endpoint
                && let Some(grant) = self.network.outgoing(endpoint.link)
            {
                let new = !lock(&self.pools)
                    .values()
                    .any(|p| lock(&p.grants).contains_key(&grant.binding));
                if new && let Ok((pool, auth)) = self.admit(grant, None).await {
                    self.ensure_listener();
                    let _ = self.publish_advert(grant);
                    self.start_warm(pool, auth);
                }
            }
        }
        self.revoke_stale();
        for (connection, rtt) in self.network.hub.observed.swap(Arc::new(HashMap::new())).iter() {
            if let Some(pacer) = lock(&self.pacers).get(connection) {
                match rtt {
                    Some(rtt) => pacer.observe(*rtt),
                    None => pacer.probe_timeout(),
                }
            }
        }
        self.status_changed();
    }
    fn pool(&self, grant: Grant) -> Result<Arc<Pool>, NetError> {
        let key = PoolKey {
            peer: grant.neighbor.device_id,
            connection: grant.binding.hop.connection_id,
        };
        let mut pools = lock(&self.pools);
        if let Some(pool) = pools.get(&key) {
            return Ok(pool.clone());
        }
        let pacer = lock(&self.pacers)
            .entry(key.connection)
            .or_insert_with(|| {
                let pacer = Arc::new(crate::pace::Pacer::new(self.cfg.activity.clone()));
                self.watch_probe_demand(key.connection, pacer.clone());
                pacer
            })
            .clone();
        let pool = Arc::new(Pool {
            key,
            state: Mutex::new(PoolState::default()),
            grants: Mutex::new(HashMap::new()),
            pacer,
            closed: watch::channel(false).0,
            notify: Notify::new(),
            failure: Mutex::new(None),
        });
        pools.insert(key, pool.clone());
        Ok(pool)
    }
    fn authorization(&self, pool: &Arc<Pool>, grant: Grant, forward: Option<Grant>) -> Authorization {
        lock(&pool.grants)
            .entry(grant.binding)
            .or_insert_with(|| Authorization {
                network: self.network.clone(),
                grant,
                forward,
                gate: Arc::new(scope::Gate::new()),
                tasks: Arc::new(watch::channel(0).0),
                lives: Arc::new(Mutex::new(vec![])),
            })
            .clone()
    }
    async fn admit(&self, grant: Grant, forward: Option<Grant>) -> Result<(Arc<Pool>, Authorization), NetError> {
        let _admission = lock(&self.admission);
        if !self.network.low_current(grant) || *self.stopped.borrow() {
            return Err(NetError::Closed);
        }
        let pool = self.pool(grant)?;
        let auth = self.authorization(&pool, grant, forward);
        if !auth.current() {
            return Err(NetError::Closed);
        }
        Ok((pool, auth))
    }
    fn control_authorization(&self, grant: Grant) -> Result<Authorization, NetError> {
        let key = PoolKey {
            peer: grant.neighbor.device_id,
            connection: grant.binding.hop.connection_id,
        };
        let pool = lock(&self.pools).get(&key).cloned().ok_or(NetError::Closed)?;
        let auth = lock(&pool.grants)
            .get(&grant.binding)
            .cloned()
            .ok_or(NetError::Closed)?;
        if auth.current() {
            Ok(auth)
        } else {
            Err(NetError::Closed)
        }
    }
    async fn reserve_control(&self, grant: Grant) -> Result<Slot, NetError> {
        let auth = self.control_authorization(grant)?;
        let hop = self.network.hop(grant)?;
        let mut stopped = self.stopped.subscribe();
        let slot = tokio::select! {biased;_=wait_closed(&mut stopped)=>return Err(NetError::Closed),_=auth.cancelled()=>return Err(NetError::Closed),slot=hop.controls.clone().reserve_owned()=>slot.map_err(|_|NetError::Closed)?};
        if !auth.current() {
            return Err(NetError::Closed);
        }
        Ok(Slot {
            slot,
            wake: self.network.hub.wake.clone(),
        })
    }
    async fn queue_control(
        &self,
        grant: Grant,
        message: LowMsg,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<(), NetError>>, NetError> {
        let slot = self.reserve_control(grant).await?;
        let (reply, result) = tokio::sync::oneshot::channel();
        slot.send(ControlSend { grant, message, reply });
        Ok(result)
    }
    async fn send(&self, grant: Grant, message: LowMsg) -> Result<(), NetError> {
        let auth = self.control_authorization(grant)?;
        let receive = self.queue_control(grant, message).await?;
        tokio::select! {biased;_=auth.cancelled()=>Err(NetError::Closed),result=receive=>result.unwrap_or(Err(NetError::Closed))}
    }
    fn cancel_hint(&self, grant: Grant, request: RequestId) -> Result<(), NetError> {
        let hop = self.network.hop(grant)?;
        let slot = hop.controls.clone().try_reserve_owned().map_err(|_| NetError::Full)?;
        let (reply, _) = tokio::sync::oneshot::channel();
        Slot {
            slot,
            wake: self.network.hub.wake.clone(),
        }
        .send(ControlSend {
            grant,
            message: LowMsg::Cancel { request },
            reply,
        });
        Ok(())
    }
    fn publish_advert(&self, grant: Grant) -> Result<(), NetError> {
        if !self.network.low_current(grant) {
            return Err(NetError::Closed);
        }
        let hop = self.network.hop(grant)?;
        hop.advert.store(Some(Arc::new(ControlInfo {
            grant,
            message: self.advert(),
        })));
        self.network.hub.wake.notify_one();
        Ok(())
    }
    fn spawn_background(&self, work: impl std::future::Future<Output = ()> + Send + 'static) {
        let mut stopped = self.stopped.subscribe();
        if !self.background.send_if_modified(|count| {
            if *stopped.borrow() {
                return false;
            }
            *count += 1;
            true
        }) {
            return;
        }
        let guard = Work(self.background.clone());
        self.runtime.spawn(async move {
            let _guard = guard;
            tokio::select! { biased; _ = wait_closed(&mut stopped) => {}, _ = work => {} }
        });
    }
    fn spawn(&self, auth: Authorization, work: impl std::future::Future<Output = ()> + Send + 'static) {
        let Some(guard) = auth.work() else {
            return;
        };
        self.runtime.spawn(async move {
            let _guard = guard;
            tokio::select! {biased;_=auth.cancelled()=>{},_=work=>{}}
        });
    }
    fn revoke_stale(&self) {
        let admission = lock(&self.admission);
        let pools: Vec<_> = lock(&self.pools).values().cloned().collect();
        let mut restart = vec![];
        for pool in pools {
            let removed = {
                let mut grants = lock(&pool.grants);
                let stale: Vec<_> = grants.iter().filter(|(_, a)| !a.current()).map(|(b, _)| *b).collect();
                stale.into_iter().filter_map(|b| grants.remove(&b)).collect::<Vec<_>>()
            };
            let retired = !removed.is_empty();
            for auth in removed {
                auth.gate.cancel();
                lock(&pool.state).cancel_scope(auth.grant.binding);
            }
            if lock(&pool.grants).is_empty() {
                pool.close();
                lock(&self.pools).remove(&pool.key);
            } else if retired && let Some(auth) = lock(&pool.grants).values().find(|a| a.current()).cloned() {
                restart.push((pool.clone(), auth));
            }
            pool.notify.notify_waiters();
        }
        drop(admission);
        for (pool, auth) in restart {
            self.arc().start_warm(pool, auth);
        }
        self.network.hub.failures.rcu(|old| {
            let mut next = (**old).clone();
            next.retain(|binding, _| self.network.grant(*binding).is_some());
            next
        });
        let live: HashSet<_> = self.network.hub.view.load().hops.iter().map(|h| h.connection).collect();
        lock(&self.inbound).retain(|c| live.contains(c));
        lock(&self.pacers).retain(|c, p| {
            if live.contains(c) {
                true
            } else {
                p.close();
                false
            }
        });
    }
    fn watch_probe_demand(&self, connection: ConnectionId, pacer: Arc<crate::pace::Pacer>) {
        let core = self.arc();
        self.spawn_background(async move{let mut stopped=core.stopped.subscribe();let mut reported=false;loop{let changed=pacer.changed.notified();tokio::pin!(changed);changed.as_mut().enable();if *stopped.borrow()||!lock(&core.pacers).contains_key(&connection){break;}pacer.report();let active=pacer.active();if active!=reported{core.network.set_low_probe_active(connection,active).await;reported=active;}tokio::select!{biased;_=wait_closed(&mut stopped)=>break,_=changed=>{},_=tokio::time::sleep(crate::pace::PERIOD),if reported=>{}}}if reported{core.network.set_low_probe_active(connection,false).await;}});
    }
    fn status_changed(&self) {
        let status = lock(&self.pools)
            .values()
            .map(|pool| Status {
                connection: pool.key.connection,
                available: matches!(lock(&pool.state).warm, WarmState::Ready),
                failure: lock(&pool.failure).clone(),
            })
            .collect();
        let current = self.network.hub.status.load();
        if **current == status {
            return;
        }
        self.network.hub.status.store(Arc::new(status));
        self.network.hub.wake.notify_one();
    }
    async fn stop(&self) {
        self.stopped.send_replace(true);
        let pools: Vec<_> = lock(&self.pools).drain().map(|(_, p)| p).collect();
        let auths: Vec<_> = pools
            .iter()
            .flat_map(|p| lock(&p.grants).values().cloned().collect::<Vec<_>>())
            .collect();
        for pool in pools {
            pool.close();
        }
        for pacer in lock(&self.pacers).drain().map(|(_, p)| p) {
            pacer.close();
        }
        for auth in auths {
            auth.drained().await;
        }
        let mut background = self.background.subscribe();
        while *background.borrow_and_update() != 0 {
            if background.changed().await.is_err() {
                break;
            }
        }
        self.status_changed();
    }
}

#[cfg(test)]
mod tests;
