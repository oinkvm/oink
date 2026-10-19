use super::*;
use crate::model::{ContentMsg, Demand, NO_PROGRESS_MS};
use crate::sync::files::{BulkChannel, BulkLink};
use oink_foundation::drag_trace::{Context as ReadContext, Trace as ReadTrace, stage as read_stage};
use std::sync::{
    Condvar,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub(super) mod delivery;
pub(super) mod tree;

#[derive(Clone)]
pub struct Remote(Arc<Inner>);
struct Inner {
    inventory: Inventory,
    wire_inventory: oink_protocol::v1::dto::Inventory,
    target: ClipPlatform,
    batch: Batch,
    link: Arc<dyn BulkLink>,
    runtime: tokio::runtime::Handle,
    slots: Mutex<HashMap<RepresentationId, Arc<Slot>>>,
    demand: Mutex<Demand>,
    receiver: Mutex<Option<oink_foundation::DeviceId>>,
    cancelled: AtomicBool,
    terminal: std::sync::atomic::AtomicU8,
    changed: tokio::sync::Notify,
    parallel: Arc<tokio::sync::Semaphore>,
    tree: Mutex<tree::Tree>,
    progress: tokio::sync::watch::Sender<super::TransferProgress>,
    last_progress: Mutex<tokio::time::Instant>,
    delivery_started: AtomicBool,
    receipt: Mutex<Option<[u8; 32]>>,
    representations: Mutex<HashMap<RepresentationId, (u64, [u8; 32])>>,
    received: Mutex<HashMap<RepresentationId, u64>>,
    max_files: std::sync::atomic::AtomicU64,
}
struct Slot {
    diagnostic: ReadContext,
    hover: bool,
    drop_requested: AtomicBool,
    result: Mutex<Option<Result<(), Unavailable>>>,
    ready: Condvar,
    changed: tokio::sync::Notify,
}
// Aborting a runtime future must release synchronous native readers as well.
// The guard is constructed before spawning, so an unpolled future is covered.
struct SlotCompletion(Arc<Slot>);
impl Drop for SlotCompletion {
    fn drop(&mut self) {
        let mut result = self.0.result.lock().unwrap();
        if result.is_none() {
            *result = Some(Err(Unavailable::Cancelled));
        }
        self.0.ready.notify_all();
        self.0.changed.notify_waiters();
    }
}
impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteContent")
            .field("items", &self.0.inventory.items.len())
            .finish()
    }
}
pub(super) struct NoTranslation;
impl Translator for NoTranslation {
    fn standard(&self, _: &Representation, _: &[u8]) -> io::Result<Vec<ClipData>> {
        Err(unavailable(Unavailable::Unsupported))
    }
}
impl Remote {
    pub fn new(
        data: &Path,
        batch: BatchId,
        inventory: Inventory,
        target: ClipPlatform,
        link: Arc<dyn BulkLink>,
        demand: Demand,
    ) -> io::Result<Self> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| unavailable(Unavailable::Cancelled))?;
        Self::on_runtime(data, batch, inventory, target, link, demand, runtime)
    }
    pub fn on_runtime(
        data: &Path,
        batch: BatchId,
        inventory: Inventory,
        target: ClipPlatform,
        link: Arc<dyn BulkLink>,
        demand: Demand,
        runtime: tokio::runtime::Handle,
    ) -> io::Result<Self> {
        let inventory = crate::wire::inventory(&inventory).map_err(|_| unavailable(Unavailable::InvalidData))?;
        Self::on_wire_runtime(data, batch, inventory, target, link, demand, runtime)
    }
    /// Keep the authenticated wire tree as the durable representation. Native
    /// adapters inspect a supported view without discarding future fields.
    pub fn on_wire_runtime(
        data: &Path,
        batch: BatchId,
        wire_inventory: oink_protocol::v1::dto::Inventory,
        target: ClipPlatform,
        link: Arc<dyn BulkLink>,
        demand: Demand,
        runtime: tokio::runtime::Handle,
    ) -> io::Result<Self> {
        let inventory =
            crate::wire::local_inventory(&wire_inventory).map_err(|_| unavailable(Unavailable::InvalidData))?;
        inventory.validate().map_err(unavailable)?;
        let link = link.bind(inventory.id.clip.origin, inventory.id.clip);
        if !inventory.namespace_roots_for(target).is_empty() && !link.supports_namespace() {
            return Err(unavailable(Unavailable::Unsupported));
        }
        let tree = Mutex::new(tree::Tree::new(&inventory.namespace_roots_for(target)));
        Ok(Self(Arc::new(Inner {
            inventory,
            wire_inventory,
            target,
            batch: Batch::create(data, StoreKind::CompletedDelivery, batch)?,
            link,
            runtime,
            slots: Mutex::default(),
            demand: Mutex::new(demand),
            receiver: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            terminal: std::sync::atomic::AtomicU8::new(0),
            changed: tokio::sync::Notify::new(),
            parallel: Arc::new(tokio::sync::Semaphore::new(4)),
            tree,
            progress: tokio::sync::watch::channel(super::TransferProgress::new()).0,
            last_progress: Mutex::new(tokio::time::Instant::now()),
            delivery_started: AtomicBool::new(false),
            receipt: Mutex::new(None),
            representations: Mutex::new(HashMap::new()),
            received: Mutex::default(),
            max_files: std::sync::atomic::AtomicU64::new(u64::MAX),
        })))
    }
    pub fn set_demand(&self, demand: Demand) {
        *self.0.demand.lock().unwrap() = demand;
    }
    pub fn set_receiver(&self, receiver: oink_foundation::DeviceId) {
        let mut current = self.0.receiver.lock().unwrap();
        assert!(
            current.is_none_or(|current| current == receiver),
            "delivery receiver is immutable"
        );
        *current = Some(receiver);
    }
    fn delivery_id(&self) -> io::Result<oink_foundation::content::DeliveryId> {
        let (receiver, sequence) = match *self.0.demand.lock().unwrap() {
            Demand::Clipboard => (
                self.0
                    .receiver
                    .lock()
                    .unwrap()
                    .ok_or_else(|| unavailable(Unavailable::InvalidData))?,
                u64::from_le_bytes(self.0.batch.id().0[..8].try_into().unwrap()),
            ),
            Demand::Drop(holder) | Demand::Hover(holder) => (holder.device, holder.epoch),
        };
        Ok(oink_foundation::content::DeliveryId {
            offer: self.0.inventory.id,
            receiver,
            sequence,
        })
    }
    pub fn batch(&self) -> &Batch {
        &self.0.batch
    }
    pub fn automatic_pull(&self) -> bool {
        if !self.0.inventory.automatic_pull(self.0.target) {
            return false;
        }
        for representation in self.0.inventory.wanted(self.0.target) {
            if representation.data.size().is_some()
                && !(self.0.target == self.0.inventory.platform
                    && matches!(representation.data, RepresentationData::Inline(_)))
            {
                self.request(representation.id);
            }
        }
        true
    }
    pub fn cancel(&self) {
        if self
            .0
            .terminal
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.0.cancelled.store(true, Ordering::Release);
        self.0.changed.notify_waiters();
        for slot in self.0.slots.lock().unwrap().values() {
            let mut result = slot.result.lock().unwrap();
            if result.is_none() {
                *result = Some(Err(Unavailable::Cancelled));
            }
            slot.ready.notify_all();
            slot.changed.notify_waiters();
        }
        self.cancel_files(Unavailable::Cancelled);
        self.update_progress(false, |p| {
            p.phase = super::TransferPhase::Cancelled;
            p.failure = Some(Unavailable::Cancelled);
        });
        let batch = self.0.batch.clone();
        self.0.runtime.spawn_blocking(move || {
            let _ = batch.abandon();
        });
    }
    fn request(&self, id: RepresentationId) -> Arc<Slot> {
        let demand = *self.0.demand.lock().unwrap();
        let mut slots = self.0.slots.lock().unwrap();
        if let Some(slot) = slots.get(&id) {
            let result = slot.result.lock().unwrap();
            if slot.hover && matches!(demand, Demand::Drop(_)) && result.is_some_and(|r| r.is_err()) {
                drop(result);
                slots.remove(&id);
            } else {
                // Only a new real content request (or accepted-drop downloader)
                // upgrades this slot. ReleaseIntent/set_demand alone cannot.
                if matches!(demand, Demand::Drop(_)) {
                    slot.drop_requested.store(true, Ordering::Release);
                }
                return slot.clone();
            }
        }
        let slot_trace = ReadTrace::new(
            read_stage::FETCH,
            ReadContext {
                offer: self.0.inventory.id.clip.seq,
                representation: id.0,
                ..ReadContext::current()
            },
        );
        let slot = Arc::new(Slot {
            diagnostic: slot_trace.context(),
            hover: matches!(demand, Demand::Hover(_)),
            drop_requested: AtomicBool::new(false),
            result: Mutex::new(None),
            ready: Condvar::new(),
            changed: tokio::sync::Notify::new(),
        });
        slot_trace.result(0);
        slots.insert(id, slot.clone());
        let content = self.clone();
        let ready = slot.clone();
        let activity =
            oink_foundation::maintenance::process().enter(oink_foundation::maintenance::ActivityKind::FileTransfer);
        let completion = SlotCompletion(ready.clone());
        self.0.runtime.spawn(async move {
            let fetch_trace = ReadTrace::new(read_stage::FETCH, ready.diagnostic);
            let _completion = completion;
            let Some(_update_activity) = activity else {
                *ready.result.lock().unwrap() = Some(Err(Unavailable::Cancelled));
                ready.ready.notify_all();
                ready.changed.notify_waiters();
                return;
            };
            let mut demand = demand;
            loop {
                let result = content
                    .fetch(id, demand, fetch_trace.context())
                    .await
                    .map_err(|e| reason(&e));
                // Serialize with request(), not just the physical release. An
                // outstanding hover refusal is retried only if a drop reader
                // joined it while it was running.
                fetch_trace.result(result.as_ref().err().map_or(0, |e| -1 - *e as i32));
                let current = content.0.demand.lock().unwrap();
                let mut done = ready.result.lock().unwrap();
                if result.is_err()
                    && matches!(demand, Demand::Hover(_))
                    && ready.drop_requested.load(Ordering::Acquire)
                    && matches!(*current, Demand::Drop(_))
                {
                    demand = *current;
                    drop(done);
                    drop(current);
                    continue;
                }
                if done.is_none() {
                    *done = Some(result);
                }
                ready.ready.notify_all();
                ready.changed.notify_waiters();
                break;
            }
        });
        slot
    }
    async fn fetch(&self, id: RepresentationId, demand: Demand, diagnostic: ReadContext) -> io::Result<()> {
        let queued = ReadTrace::new(read_stage::PARALLEL_WAIT, diagnostic);
        let permit = self.0.parallel.clone().acquire_owned().await;
        queued.result(if permit.is_ok() { 0 } else { -1 });
        drop(queued);
        let _permit = permit.map_err(|_| unavailable(Unavailable::Cancelled))?;
        let mut deadline = self.deadline();
        let mut backoff = Duration::from_millis(500);
        loop {
            if self.0.cancelled.load(Ordering::Acquire) || self.0.link.is_cancelled() {
                return Err(unavailable(Unavailable::Cancelled));
            }
            let result = tokio::select! {
                _ = self.0.changed.notified() => return Err(unavailable(Unavailable::Cancelled)),
                _ = self.0.link.cancelled() => return Err(unavailable(Unavailable::Cancelled)),
                result = self.attempt(id, demand, &mut deadline, diagnostic) => result,
            };
            match result {
                Ok(()) => return Ok(()),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::NotConnected
                            | io::ErrorKind::BrokenPipe
                            | io::ErrorKind::UnexpectedEof
                    ) => {}
                Err(error) => return Err(error),
            }
            tokio::select! {
                _ = self.0.changed.notified() => return Err(unavailable(Unavailable::Cancelled)),
                _ = self.0.link.cancelled() => return Err(unavailable(Unavailable::Cancelled)),
                _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now()+backoff)) => {},
            }
            deadline = self.deadline();
            if tokio::time::Instant::now() >= deadline {
                return Err(unavailable(Unavailable::NoProgress));
            }
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }
    async fn attempt(
        &self,
        id: RepresentationId,
        demand: Demand,
        deadline: &mut tokio::time::Instant,
        diagnostic: ReadContext,
    ) -> io::Result<()> {
        let attempt = ReadTrace::new(read_stage::LOW_OPEN, diagnostic);
        let offer = self.0.inventory.id;
        // The link owner admits every bulk operation against explicit wire
        // budgets. Include bounded frame metadata in addition to payload bytes.
        let (item, accepted_format) = self
            .0
            .link
            .select_representation(&self.0.inventory, id, self.0.target)?;
        let request = ContentMsg::ReadRepresentation {
            offer,
            representation: id,
            item,
            accepted_format,
            target: self.0.target,
            demand,
        };
        let send_max = oink_protocol::v1::frame::encode(
            &crate::wire::content(&request).map_err(|_| unavailable(Unavailable::InvalidData))?,
            oink_protocol::v1::limits::LOW_FRAME,
        )
        .map_err(|_| unavailable(Unavailable::InvalidData))?
        .len() as u64;
        let receive_max = REPRESENTATION_BYTES + (REPRESENTATION_BYTES / (1024 * 1024) + 2) * 512;
        let options = oink_foundation::transfer::LowOptions {
            send_max,
            receive_max,
            operation_max: send_max + receive_max,
        };
        let opened = tokio::time::timeout_at(
            *deadline,
            attempt.scope(|| self.0.link.open_with_options(offer.clip.origin, options)),
        )
        .await
        .map_err(|_| unavailable(Unavailable::NoProgress))
        .and_then(|result| result);
        attempt.io(&opened);
        let context = attempt.context();
        drop(attempt);
        let mut channel = opened?;
        let context = context.with_request(channel.diagnostic_id());
        let sending = ReadTrace::new(read_stage::REQUEST_SEND, context);
        let sent = channel.send(&request).await;
        sending.io(&sent);
        drop(sending);
        sent?;
        let response = ReadTrace::new(read_stage::RESPONSE_WAIT, context);
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let batch = self.0.batch.clone();
        let writer_trace = ReadTrace::new(read_stage::CACHE_WRITE, context);
        let writer_context = writer_trace.context();
        let writer = tokio::task::spawn_blocking(move || {
            let writing = ReadTrace::new(
                read_stage::CACHE_WRITE,
                ReadContext {
                    aux: 1,
                    ..writer_context
                },
            );
            let result = batch.snapshot(
                id.0,
                &mut Incoming {
                    rx,
                    bytes: io::Cursor::new(Vec::new()),
                    failed: false,
                    finished: false,
                },
            );
            writing.io(&result);
            result
        });
        let result = self.receive(id, channel.as_mut(), &tx, deadline).await;
        response.io(&result);
        drop(response);
        if let Err(error) = &result {
            let _ = tx.send(Err(io::Error::new(error.kind(), "content read failed"))).await;
        } else {
            let _ = tx.send(Ok(Vec::new())).await;
        }
        drop(tx);
        let stored = writer.await.map_err(|_| unavailable(Unavailable::ReadFailed))?;
        writer_trace.io(&stored);
        drop(writer_trace);
        result?;
        let stored = stored?;
        self.0.representations.lock().unwrap().insert(id, stored);
        Ok(())
    }
    async fn receive(
        &self,
        id: RepresentationId,
        channel: &mut dyn BulkChannel,
        tx: &tokio::sync::mpsc::Sender<io::Result<Vec<u8>>>,
        deadline: &mut tokio::time::Instant,
    ) -> io::Result<()> {
        let offer = self.0.inventory.id;
        let mut offset = 0_u64;
        let mut hash = blake3::Hasher::new();
        let validity = channel.effect_validity();
        loop {
            let message = self.receive_content(channel).await?;
            if !validity() || self.0.cancelled.load(Ordering::Acquire) {
                return Err(unavailable(Unavailable::Cancelled));
            }
            match message {
                ContentMsg::RepresentationBytes {
                    offer: got,
                    representation,
                    offset: at,
                    bytes,
                } if got == offer && representation == id && at == offset && !bytes.is_empty() => {
                    offset = offset
                        .checked_add(bytes.len() as u64)
                        .filter(|n| *n <= REPRESENTATION_BYTES)
                        .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
                    hash.update(&bytes);

                    tx.send(Ok(bytes))
                        .await
                        .map_err(|_| unavailable(Unavailable::StorageFull))?;
                    let fresh = {
                        let mut received = self.0.received.lock().unwrap();
                        let previous = received.entry(id).or_default();
                        let fresh = offset.saturating_sub(*previous);
                        *previous = (*previous).max(offset);
                        fresh
                    };
                    self.update_progress(fresh > 0, |p| p.bytes = p.bytes.saturating_add(fresh));
                    *deadline = self.deadline();
                }
                ContentMsg::RepresentationEnd {
                    offer: got,
                    representation,
                    size,
                    hash: digest,
                } if got == offer
                    && representation == id
                    && size == offset
                    && digest == *hash.finalize().as_bytes() =>
                {
                    let rep = find(&self.0.inventory, id).ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
                    if self.0.target == self.0.inventory.platform {
                        match rep.data {
                            RepresentationData::Reader {
                                size: ContentSize::Known(n),
                                ..
                            } if n != size => return Err(unavailable(Unavailable::Superseded)),
                            RepresentationData::Reader {
                                hash: Some(expected), ..
                            } if expected != digest => return Err(unavailable(Unavailable::Superseded)),
                            _ => {}
                        }
                    }
                    self.end_channel(channel).await?;
                    return Ok(());
                }
                ContentMsg::RepresentationFailed {
                    offer: got,
                    representation,
                    reason,
                } if got == offer && representation == id => return Err(unavailable(reason)),
                _ => return Err(unavailable(Unavailable::InvalidData)),
            }
        }
    }
    fn open_until(&self, id: RepresentationId, deadline: Option<std::time::Instant>) -> io::Result<FileReader> {
        let trace = ReadTrace::new(
            read_stage::REMOTE_OPEN,
            ReadContext {
                offer: self.0.inventory.id.clip.seq,
                representation: id.0,
                ..ReadContext::current()
            },
        );
        let result = trace.scope(|| -> io::Result<FileReader> {
            if self.0.cancelled.load(Ordering::Acquire) {
                return Err(unavailable(Unavailable::Cancelled));
            }
            let rep = self
                .0
                .inventory
                .wanted(self.0.target)
                .find(|r| r.id == id)
                .ok_or_else(|| unavailable(Unavailable::Unsupported))?;
            if matches!(*self.0.demand.lock().unwrap(), Demand::Drop(_)) {
                self.start_delivery();
            }
            if self.0.target == self.0.inventory.platform
                && let RepresentationData::Inline(bytes) = &rep.data
            {
                return Ok(Box::new(io::Cursor::new(bytes.clone())));
            }
            if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                return Err(unavailable(Unavailable::NoProgress));
            }
            let slot = self.request(id);
            let wait = ReadTrace::new(
                read_stage::SLOT_WAIT,
                ReadContext {
                    object: slot.diagnostic.call,
                    aux: u32::from(slot.hover),
                    ..ReadContext::current()
                },
            );
            let mut result = slot.result.lock().unwrap();
            while result.is_none() {
                result = if let Some(deadline) = deadline {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return Err(unavailable(Unavailable::NoProgress));
                    }
                    slot.ready.wait_timeout(result, left).unwrap().0
                } else {
                    slot.ready.wait(result).unwrap()
                };
            }
            wait.result(result.as_ref().unwrap().as_ref().err().map_or(0, |e| -1 - *e as i32));
            drop(wait);
            result.as_ref().unwrap().map_err(unavailable)?;
            let read = self.0.batch.read_lease()?;
            let file = std::fs::File::open(self.0.batch.path().join(format!("r-{}", id.0)))?;
            Ok(Box::new(LocalReader {
                file,
                _batch: self.0.batch.clone(),
                _read: read,
            }))
        });
        trace.io(&result);
        result
    }
}
impl Provider for Remote {
    fn inventory(&self) -> Inventory {
        self.0.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<FileReader> {
        self.open_until(id, None)
    }
    fn open_preview(&self, id: RepresentationId, timeout: Duration) -> io::Result<FileReader> {
        if !matches!(*self.0.demand.lock().unwrap(), Demand::Hover(_)) {
            return Err(unavailable(Unavailable::Permission));
        }
        self.open_until(id, Some(std::time::Instant::now() + timeout))
    }
    fn local_batch(&self) -> Option<Batch> {
        Some(self.0.batch.clone())
    }
    fn translator(&self) -> Arc<dyn Translator> {
        Arc::new(NoTranslation)
    }
    fn namespace(&self) -> Option<Arc<dyn namespace::Namespace>> {
        (!self.0.inventory.namespace_roots_for(self.0.target).is_empty())
            .then(|| Arc::new(self.clone()) as Arc<dyn namespace::Namespace>)
    }
}
struct LocalReader {
    file: std::fs::File,
    _read: crate::cache::SourceLease,
    _batch: Batch,
}
impl Read for LocalReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}
struct Incoming {
    rx: tokio::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
    bytes: io::Cursor<Vec<u8>>,
    failed: bool,
    finished: bool,
}
impl Read for Incoming {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.failed {
            return Err(unavailable(Unavailable::ReadFailed));
        }
        if self.finished {
            return Ok(0);
        }
        loop {
            let n = self.bytes.read(buffer)?;
            if n != 0 {
                return Ok(n);
            }
            match self.rx.blocking_recv() {
                Some(Ok(bytes)) if bytes.is_empty() => {
                    self.finished = true;
                    return Ok(0);
                }
                Some(Ok(bytes)) => self.bytes = io::Cursor::new(bytes),
                Some(Err(error)) => {
                    self.failed = true;
                    return Err(error);
                }
                None => {
                    self.failed = true;
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
            }
        }
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    #[tokio::test]
    async fn aborting_an_unpolled_download_wakes_its_native_reader() {
        let slot = Arc::new(Slot {
            diagnostic: ReadContext::default(),
            hover: false,
            drop_requested: AtomicBool::new(false),
            result: Mutex::new(None),
            ready: Condvar::new(),
            changed: tokio::sync::Notify::new(),
        });
        let waiting = slot.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut result = waiting.result.lock().unwrap();
            while result.is_none() {
                result = waiting.ready.wait(result).unwrap();
            }
            send.send(result.unwrap()).unwrap();
        });
        let completion = SlotCompletion(slot);
        let task = tokio::spawn(async move {
            let _completion = completion;
            std::future::pending::<()>().await;
        });
        task.abort();
        let _ = task.await;
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(Unavailable::Cancelled)
        );
        reader.join().unwrap();
    }
}
