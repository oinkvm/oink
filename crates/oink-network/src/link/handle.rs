//! Ordinary-thread command handle and whole-snapshot publication.
use super::*;
use arc_swap::ArcSwap;
#[cfg(any(test, feature = "test-support"))]
use arc_swap::ArcSwapOption;
use std::{
    collections::HashSet,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
};
use tokio::sync::{Notify, mpsc, oneshot};

pub(super) enum Command {
    Dial(DeviceId, Vec<SocketAddr>, DialMode),
    Disconnect(LinkId, Disconnect),
    HalfOpen(LinkId),
    Ready(LinkId),
    ConfigFence(std::sync::mpsc::SyncSender<()>),
    Agreement(LinkId, oink_protocol::v1::admission::Agreement),
    Retire(LinkId),
    OldMemberLane(LinkId, bool),
    Formats(LinkId, oink_protocol::v1::dto::FormatSupport),
    Audio(LinkId, Vec<oink_protocol::v1::dto::AudioSupport>),
    Violation(LinkId, oink_protocol::v1::admission::Violation),
    Addresses(DeviceId, Vec<SocketAddr>),
    Register(Vec<StreamKind>, mpsc::UnboundedSender<ModuleEvent>),
    Open {
        id: StreamId,
        priority: Priority,
        queue: Arc<queue::Queue>,
        datagram: Option<Arc<queue::DatagramQueue>>,
    },
    Accept {
        id: StreamId,
        queue: Arc<queue::Queue>,
        datagram: Option<Arc<queue::DatagramQueue>>,
    },
    Close(StreamId, StreamCode),
    ProbeAddress(SocketAddr, oneshot::Sender<Result<DeviceId, AddressProbeError>>),
    #[cfg(any(test, feature = "test-support"))]
    BlockDirect(DeviceId, bool),
    Stop,
    Probe(oink_foundation::session::ConnectionId, bool),
    LowInbound(Box<InboundLow>),
    #[cfg(test)]
    ResetN1(LinkId),
    #[cfg(test)]
    InspectConnections(oneshot::Sender<(usize, usize, usize)>),
}
pub(super) struct Shared {
    pub boot: Boot,
    pub profile: crate::negotiation::OfferProfile,
    pub address: OnceLock<SocketAddr>,
    pub commands: mpsc::UnboundedSender<Command>,
    pub wake: Arc<Notify>,
    pub done: Notify,
    pub stopped: AtomicBool,
    #[cfg(test)]
    pub expired_service_budget: AtomicBool,
    #[cfg(any(test, feature = "test-support"))]
    pub low_open_attempts: std::sync::Mutex<std::collections::BTreeMap<u8, u64>>,
    #[cfg(any(test, feature = "test-support"))]
    pub hold_input_peer: ArcSwapOption<DeviceId>,
    #[cfg(any(test, feature = "test-support"))]
    pub input_header_held: Notify,
    #[cfg(any(test, feature = "test-support"))]
    pub cutover_finished: Notify,
    pub links: ArcSwap<LinkTable>,
    pub scheduler: ArcSwap<SchedulerStatus>,
    pub counters: ArcSwap<Vec<StreamCounters>>,
    pub activity: Arc<crate::activity::Activity>,
    pub hub: Arc<super::tcp::Hub>,
    pub pool: OnceLock<Arc<super::tcp::LowPool>>,
    dialing: ArcSwap<HashSet<DeviceId>>,
    registered: AtomicU16,
}
impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkLayer")
            .field("boot", &self.boot)
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}
impl Shared {
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
        self.wake.notify_one();
    }
    pub fn track_dial(&self, peer: DeviceId) {
        self.dialing.rcu(|old| {
            let mut next = (**old).clone();
            next.insert(peer);
            next
        });
    }
    pub fn release_dial(&self, peer: DeviceId) {
        self.dialing.rcu(|old| {
            let mut next = (**old).clone();
            next.remove(&peer);
            next
        });
    }
    pub fn wake_fn(&self) -> Wake {
        let notify = self.wake.clone();
        Arc::new(move || notify.notify_one())
    }
}
impl LinkLayer {
    pub(super) async fn launch(config: LinkConfig) -> Result<(Self, ConfigPort), crate::NetError> {
        if !config.bind.is_ipv4() {
            return Err(crate::NetError::BadAddress(config.bind.to_string()));
        }
        let credentials = Arc::new(
            crate::tls::Credentials::new(&config.identity).map_err(|e| crate::NetError::Endpoint(e.to_string()))?,
        );
        let ordinary_config = config.clone();
        let ordinary_credentials = credentials.clone();
        let (commands, rx) = mpsc::unbounded_channel();
        let (events, port) = mpsc::unbounded_channel();
        let (start, started) = oneshot::channel();
        let wake = Arc::new(Notify::new());
        let shared = Arc::new(Shared {
            boot: config.boot,
            profile: config.profile.clone(),
            address: OnceLock::new(),
            commands,
            wake: wake.clone(),
            hub: Arc::new(super::tcp::Hub::new(wake)),
            pool: OnceLock::new(),
            done: Notify::new(),
            stopped: AtomicBool::new(false),
            #[cfg(test)]
            expired_service_budget: AtomicBool::new(false),
            #[cfg(any(test, feature = "test-support"))]
            low_open_attempts: Default::default(),
            #[cfg(any(test, feature = "test-support"))]
            hold_input_peer: ArcSwapOption::empty(),
            #[cfg(any(test, feature = "test-support"))]
            input_header_held: Notify::new(),
            #[cfg(any(test, feature = "test-support"))]
            cutover_finished: Notify::new(),
            links: ArcSwap::from_pointee(LinkTable::default()),
            scheduler: ArcSwap::from_pointee(SchedulerStatus::default()),
            counters: ArcSwap::from_pointee(Vec::new()),
            activity: Arc::new(crate::activity::Activity::default()),
            dialing: ArcSwap::from_pointee(HashSet::new()),
            registered: AtomicU16::new(0),
        });
        let owner = shared.clone();
        std::thread::Builder::new()
            .name("oink-network".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
                match runtime {
                    Ok(runtime) => runtime.block_on(super::runtime::run(
                        config,
                        credentials,
                        owner.clone(),
                        rx,
                        events,
                        start,
                    )),
                    Err(e) => {
                        let _ = start.send(Err(crate::NetError::Io(e)));
                    }
                }
                owner.stopped.store(true, Ordering::Release);
                owner.done.notify_waiters();
            })
            .map_err(crate::NetError::Io)?;
        started
            .await
            .map_err(|_| crate::NetError::Endpoint("network thread stopped during startup".into()))??;
        let pool = super::tcp::LowPool::new(
            &ordinary_config,
            *shared.address.get().expect("bound endpoint"),
            ordinary_credentials,
            shared.hub.clone(),
            shared.commands.clone(),
            shared.activity.clone(),
        );
        shared.pool.set(pool).expect("pool initialized once");
        Ok((Self(shared), ConfigPort(port)))
    }
    pub(super) fn reserve_dial(&self, peer: DeviceId) -> Result<(), DialRefused> {
        if self.0.stopped.load(Ordering::Acquire) {
            return Err(DialRefused::Stopped);
        }
        if self.links().by_peer(peer).is_some() {
            return Err(DialRefused::AlreadyLinked);
        }
        loop {
            let old = self.0.dialing.load_full();
            if old.len() >= MAX_DIALS || old.contains(&peer) {
                return Err(DialRefused::Busy);
            }
            let mut next = (*old).clone();
            next.insert(peer);
            let previous = self.0.dialing.compare_and_swap(&old, Arc::new(next));
            if Arc::ptr_eq(&previous, &old) {
                return Ok(());
            }
        }
    }
    pub(super) fn register_port(&self, kinds: &[StreamKind]) -> ModulePort {
        let mut bits = 0u16;
        for kind in kinds {
            assert!(kind.is_business(), "private stream kind");
            let bit = 1u16 << (*kind as u8);
            assert_eq!(bits & bit, 0, "duplicate stream kind");
            bits |= bit;
        }
        loop {
            let old = self.0.registered.load(Ordering::Acquire);
            assert_eq!(old & bits, 0, "stream kind already registered");
            if self
                .0
                .registered
                .compare_exchange(old, old | bits, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        self.0.send(Command::Register(kinds.to_vec(), sender));
        ModulePort(receiver)
    }
    pub(super) fn allocate_stream(
        &self,
        link: LinkId,
        priority: Priority,
        tag: StreamTag,
        spec: QueueSpec,
        datagram: Option<QueueSpec>,
    ) -> Result<(SendStream, Option<DatagramSend>), LinkGone> {
        assert!(tag.kind.is_business(), "private stream kind");
        if self.0.stopped.load(Ordering::Acquire) || !self.links().links.iter().any(|l| l.link == link) {
            return Err(LinkGone);
        }
        let id = StreamId {
            link,
            dir: StreamDir::Out,
            tag,
        };
        let queue = queue::Queue::new(spec);
        queue.set_wake(self.0.wake_fn());
        let datagram = datagram.map(queue::DatagramQueue::new);
        if let Some(q) = &datagram {
            q.set_wake(self.0.wake_fn());
        }
        if let Some(q) = &datagram {
            q.datagram_prefix.store(4, Ordering::Release);
        }
        self.0.send(Command::Open {
            id,
            priority,
            queue: queue.clone(),
            datagram: datagram.clone(),
        });
        let parent = queue.clone();
        Ok((
            SendStream { id, queue },
            datagram.map(|queue| DatagramSend {
                follows: id,
                _owner: Arc::new(super::stream::DatagramOwner(queue.clone())),
                queue,
                parent: Some(parent),
            }),
        ))
    }
}

#[cfg(test)]
mod config_order_tests {
    use super::*;

    fn handle() -> (LinkLayer, mpsc::UnboundedReceiver<Command>) {
        let (commands, receive) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());
        let shared = Shared {
            boot: oink_foundation::session::Boot {
                device_id: DeviceId([1; 32]),
                nonce: [1; 16],
            },
            profile: Default::default(),
            address: OnceLock::new(),
            commands,
            wake: wake.clone(),
            done: Notify::new(),
            stopped: AtomicBool::new(false),
            #[cfg(test)]
            expired_service_budget: AtomicBool::new(false),
            #[cfg(any(test, feature = "test-support"))]
            low_open_attempts: Default::default(),
            #[cfg(any(test, feature = "test-support"))]
            hold_input_peer: ArcSwapOption::empty(),
            #[cfg(any(test, feature = "test-support"))]
            input_header_held: Notify::new(),
            #[cfg(any(test, feature = "test-support"))]
            cutover_finished: Notify::new(),
            links: ArcSwap::from_pointee(LinkTable::default()),
            scheduler: ArcSwap::from_pointee(SchedulerStatus::default()),
            counters: ArcSwap::from_pointee(Vec::new()),
            activity: Arc::default(),
            hub: Arc::new(super::super::tcp::Hub::new(wake)),
            pool: OnceLock::new(),
            dialing: ArcSwap::from_pointee(HashSet::new()),
            registered: AtomicU16::new(0),
        };
        (LinkLayer(Arc::new(shared)), receive)
    }

    #[tokio::test]
    async fn config_phase_is_applied_before_caller_can_post_its_next_frame() {
        for ready in [false, true] {
            let (network, mut commands) = handle();
            let (returned, mut completion) = oneshot::channel();
            let caller = std::thread::spawn(move || {
                if ready {
                    network.set_ready(LinkId(1));
                } else {
                    network.set_half_open(LinkId(1));
                }
                let _ = returned.send(());
            });
            let command = commands.recv().await.unwrap();
            assert!(matches!(
                command,
                Command::HalfOpen(LinkId(1)) | Command::Ready(LinkId(1))
            ));
            // Hold the networking owner before applying its queued phase. The
            // caller must not yet be allowed to publish a reply/Done/Facts.
            tokio::time::timeout(Duration::from_secs(4), async {
                tokio::select! {
                    _ = &mut completion => panic!("config caller returned before networking applied its phase"),
                    fence = commands.recv() => {
                        let Some(Command::ConfigFence(applied)) = fence else { panic!("missing config phase fence") };
                        applied.send(()).unwrap();
                    }
                }
                completion.await.unwrap();
            })
            .await
            .expect("config phase acknowledgement completes");
            caller.join().unwrap();
        }
    }
}
