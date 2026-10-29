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
mod memory;
mod network;
mod tree;

#[derive(Clone)]
pub struct Remote(Arc<Inner>);
struct Inner {
    inventory: Inventory,
    target: ClipPlatform,
    batch_id: BatchId,
    link: Arc<dyn BulkLink>,
    runtime: tokio::runtime::Handle,
    slots: Mutex<HashMap<RepresentationId, Arc<Slot>>>,
    demand: Mutex<Demand>,
    receiver: Mutex<Option<oink_foundation::DeviceId>>,
    cancelled: AtomicBool,
    terminal: std::sync::atomic::AtomicU8,
    changed: tokio::sync::Notify,
    parallel: Arc<tokio::sync::Semaphore>,
    files: Option<super::linked::Publication>,
    progress: tokio::sync::watch::Sender<super::TransferProgress>,
    last_progress: Mutex<tokio::time::Instant>,
    delivery_started: AtomicBool,
    delivery_report: std::sync::OnceLock<std::time::Instant>,
    report_drag: bool,
    drop_report: std::sync::OnceLock<Option<std::time::Instant>>,
    drop_report_ended: AtomicBool,
    receipt: Mutex<Option<[u8; 32]>>,
    representations: Mutex<HashMap<RepresentationId, Arc<memory::Bytes>>>,
    memory: Arc<memory::Budget>,
    received: Mutex<HashMap<RepresentationId, u64>>,
}
struct Slot {
    diagnostic: ReadContext,
    hover: bool,
    abort: std::sync::OnceLock<tokio::task::AbortHandle>,
    received: std::sync::atomic::AtomicU64,
    deadline_floor: Option<tokio::time::Instant>,
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
    /// Read-only, nonblocking failure diagnostics. Do not acquire the progress
    /// watch's read lock: a stuck writer is one reason a watchdog may be firing.
    #[cfg(debug_assertions)]
    pub fn watchdog_snapshot_for_test(&self) -> String {
        let slots = self.0.slots.try_lock().map(|slots| {
            slots
                .iter()
                .map(|(id, slot)| {
                    (
                        *id,
                        slot.hover,
                        slot.drop_requested.load(Ordering::Acquire),
                        format!("{:?}", slot.result.try_lock()),
                    )
                })
                .collect::<Vec<_>>()
        });
        format!(
            "remote terminal={} cancelled={} delivery_started={} demand={:?} receiver={:?} receipt={:?} slots={slots:?}\n{}",
            self.0.terminal.load(Ordering::Acquire),
            self.0.cancelled.load(Ordering::Acquire),
            self.0.delivery_started.load(Ordering::Acquire),
            self.0.demand.try_lock(),
            self.0.receiver.try_lock(),
            self.0.receipt.try_lock().map(|r| r.is_some()),
            self.0
                .files
                .as_ref()
                .map_or_else(|| "publication=none".into(), |files| files.watchdog_snapshot_for_test()),
        )
    }

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
        let mut inventory =
            crate::wire::local_inventory(&wire_inventory).map_err(|_| unavailable(Unavailable::InvalidData))?;
        inventory.validate().map_err(unavailable)?;
        if target == ClipPlatform::Windows {
            let roots = inventory
                .roots
                .iter()
                .filter(|root| root.kind.symlink_target().is_none())
                .cloned()
                .collect();
            inventory = inventory.with_roots(roots);
        }
        let link = link.bind(inventory.id.clip.origin, inventory.id.clip);
        if !link.supports_cloud_links() {
            // Capability absence silently removes only file delivery. Mixed
            // offers retain their text, image and other non-file representations.
            inventory = crate::policy::without_files(inventory);
        }
        if !inventory.namespace_roots_for(target).is_empty() && !link.supports_namespace() {
            return Err(unavailable(Unavailable::Unsupported));
        }
        let roots = inventory.namespace_roots_for(target);
        let _ = data;
        let files = (!roots.is_empty())
            .then(|| super::linked::Publication::new(inventory.id, roots, link.clone(), demand, runtime.clone()));
        let inline = inventory
            .wanted(target)
            .filter_map(|rep| match &rep.data {
                RepresentationData::Inline(bytes) => Some(bytes.len() as u64),
                _ => None,
            })
            .sum::<u64>();
        Ok(Self(Arc::new(Inner {
            inventory,
            target,
            batch_id: batch,
            memory: memory::Budget::new(crate::model::SNAPSHOT_BYTES.saturating_sub(inline)),
            link,
            runtime,
            slots: Mutex::default(),
            demand: Mutex::new(demand),
            receiver: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            terminal: std::sync::atomic::AtomicU8::new(0),
            changed: tokio::sync::Notify::new(),
            parallel: Arc::new(tokio::sync::Semaphore::new(4)),
            files,
            progress: tokio::sync::watch::channel(super::TransferProgress::new()).0,
            last_progress: Mutex::new(tokio::time::Instant::now()),
            delivery_started: AtomicBool::new(false),
            delivery_report: std::sync::OnceLock::new(),
            report_drag: !matches!(demand, Demand::Clipboard),
            drop_report: std::sync::OnceLock::new(),
            drop_report_ended: AtomicBool::new(false),
            receipt: Mutex::new(None),
            representations: Mutex::new(HashMap::new()),
            received: Mutex::default(),
        })))
    }
    pub fn set_demand(&self, demand: Demand) {
        *self.0.demand.lock().unwrap() = demand;
        if let Some(files) = &self.0.files {
            files.set_demand(demand);
        }
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
                u64::from_le_bytes(self.0.batch_id.0[..8].try_into().unwrap()),
            ),
            Demand::Drop(holder) | Demand::Hover(holder) => (holder.device, holder.epoch),
        };
        Ok(oink_foundation::content::DeliveryId {
            offer: self.0.inventory.id,
            receiver,
            sequence,
        })
    }
    pub fn batch_id(&self) -> BatchId {
        self.0.batch_id
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
        self.record_delivery(crate::timing::Outcome::Cancelled);
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
        if let Some(files) = &self.0.files {
            files.cancel();
        }
        self.update_progress(false, |p| {
            p.phase = super::TransferPhase::Cancelled;
            p.failure = Some(Unavailable::Cancelled);
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
            abort: std::sync::OnceLock::new(),
            received: Default::default(),
            // A later native read gets its own callback budget even when the
            // earlier transfer watchdog expired. Do not reset file progress.
            deadline_floor: super::native_read::remote_read()
                .map(|read| tokio::time::Instant::now() + read.remaining()),
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
        let task = self.0.runtime.spawn(async move {
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
                    .fetch(id, demand, fetch_trace.context(), &ready)
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
                    *done = Some(result.map(|bytes| {
                        content.0.representations.lock().unwrap().insert(id, Arc::new(bytes));
                    }));
                }
                ready.ready.notify_all();
                ready.changed.notify_waiters();
                break;
            }
        });
        slot.abort.set(task.abort_handle()).unwrap();
        slot
    }
    async fn fetch(
        &self,
        id: RepresentationId,
        demand: Demand,
        diagnostic: ReadContext,
        slot: &Slot,
    ) -> io::Result<memory::Bytes> {
        let queued = ReadTrace::new(read_stage::PARALLEL_WAIT, diagnostic);
        let permit = self.0.parallel.clone().acquire_owned().await;
        queued.result(if permit.is_ok() { 0 } else { -1 });
        drop(queued);
        let _permit = permit.map_err(|_| unavailable(Unavailable::Cancelled))?;
        let mut deadline = self.deadline_with_floor(slot.deadline_floor);
        let mut backoff = Duration::from_millis(500);
        loop {
            if self.0.cancelled.load(Ordering::Acquire) || self.0.link.is_cancelled() {
                return Err(unavailable(Unavailable::Cancelled));
            }
            let result = tokio::select! {
                _ = self.0.changed.notified() => return Err(unavailable(Unavailable::Cancelled)),
                _ = self.0.link.cancelled() => return Err(unavailable(Unavailable::Cancelled)),
                result = self.attempt(id, demand, &mut deadline, diagnostic, slot) => result,
            };
            match result {
                Ok(bytes) => return Ok(bytes),
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
            deadline = self.deadline_with_floor(slot.deadline_floor);
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
        slot: &Slot,
    ) -> io::Result<memory::Bytes> {
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
        let result = self.receive(id, channel.as_mut(), deadline, slot).await;
        response.io(&result);
        result
    }
    async fn receive(
        &self,
        id: RepresentationId,
        channel: &mut dyn BulkChannel,
        deadline: &mut tokio::time::Instant,
        slot: &Slot,
    ) -> io::Result<memory::Bytes> {
        let offer = self.0.inventory.id;
        let mut stored = memory::Bytes::new(self.0.memory.clone());
        let mut offset = 0_u64;
        let validity = channel.effect_validity();
        loop {
            let message = self.receive_content_with_floor(channel, slot.deadline_floor).await?;
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

                    stored.push(bytes)?;
                    slot.received.store(offset, Ordering::Release);
                    let fresh = {
                        let mut received = self.0.received.lock().unwrap();
                        let previous = received.entry(id).or_default();
                        let fresh = offset.saturating_sub(*previous);
                        *previous = (*previous).max(offset);
                        fresh
                    };
                    self.update_progress(fresh > 0, |p| p.bytes = p.bytes.saturating_add(fresh));
                    *deadline = self.deadline_with_floor(slot.deadline_floor);
                }
                ContentMsg::RepresentationEnd {
                    offer: got,
                    representation,
                    size,
                } if got == offer && representation == id && size == offset => {
                    self.end_channel_with_floor(channel, slot.deadline_floor).await?;
                    return Ok(stored);
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
        let native = super::native_read::remote_read();
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
            if native.as_ref().is_some_and(|read| read.remaining().is_zero()) {
                self.native_timeout(id, None, native.as_ref().unwrap());
                return Err(unavailable(Unavailable::NoProgress));
            }
            let slot = self.request(id);
            #[cfg(test)]
            if let Some(read) = &native {
                let weak = Arc::downgrade(&slot);
                read.wake_on_advance(move || {
                    if let Some(slot) = weak.upgrade() {
                        let _result = slot.result.lock().unwrap();
                        slot.ready.notify_all();
                    }
                });
            }
            let wait = ReadTrace::new(
                read_stage::SLOT_WAIT,
                ReadContext {
                    object: slot.diagnostic.call,
                    aux: u32::from(slot.hover),
                    ..ReadContext::current()
                },
            );
            let mut result = slot.result.lock().unwrap();
            loop {
                if let Some(read) = &native
                    && read.remaining().is_zero()
                {
                    drop(result);
                    self.native_timeout(id, Some(&slot), read);
                    return Err(unavailable(Unavailable::NoProgress));
                }
                if result.is_some() {
                    break;
                }
                let remaining = deadline.map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()));
                let remaining = match (&native, remaining) {
                    (Some(read), Some(left)) => Some(left.min(read.remaining())),
                    (Some(read), None) => Some(read.remaining()),
                    (None, left) => left,
                };
                result = if let Some(left) = remaining {
                    if left.is_zero() {
                        drop(result);
                        if let Some(read) = &native
                            && read.remaining().is_zero()
                        {
                            self.native_timeout(id, Some(&slot), read);
                        }
                        return Err(unavailable(Unavailable::NoProgress));
                    }
                    slot.ready.wait_timeout(result, left).unwrap().0
                } else {
                    slot.ready.wait(result).unwrap()
                };
            }
            wait.result(result.as_ref().unwrap().as_ref().err().map_or(0, |e| -1 - *e as i32));
            drop(wait);
            let completed = result.unwrap();
            // The async completion guard also locks this slot. Native file
            // acquisition must not keep an executor worker waiting on disk.
            drop(result);
            completed.map_err(unavailable)?;
            self.0
                .representations
                .lock()
                .unwrap()
                .get(&id)
                .map(memory::Bytes::reader)
                .ok_or_else(|| unavailable(Unavailable::ReadFailed))
        });
        if result
            .as_ref()
            .err()
            .is_some_and(|error| reason(error) == Unavailable::Cancelled)
            && let Some(read) = &native
        {
            read.cancelled();
        }
        trace.io(&result);
        result
    }
    fn native_timeout(&self, id: RepresentationId, slot: Option<&Arc<Slot>>, read: &super::native_read::Read) {
        if let Some(slot) = slot {
            // Serialize detachment and completion with request(): a late task
            // cannot publish bytes into a replacement request's cache entry.
            let mut slots = self.0.slots.lock().unwrap();
            let mut result = slot.result.lock().unwrap();
            // The async watchdog can complete with NoProgress just before
            // the native waiter wakes at the same deadline. That failed slot
            // must also be detached so the next paste can retry.
            if result.is_none() || *result == Some(Err(Unavailable::NoProgress)) {
                *result = Some(Err(Unavailable::NoProgress));
                if let Some(abort) = slot.abort.get() {
                    // Dropping the unfinished BulkChannel cancels its Low stream
                    // at the source, including a pending source read.
                    abort.abort();
                }
                if slots.get(&id).is_some_and(|current| Arc::ptr_eq(current, slot)) {
                    slots.remove(&id);
                }
                slot.ready.notify_all();
                slot.changed.notify_waiters();
            }
        }
        if !super::native_read::timed_out() {
            read.timed_out();
            tracing::warn!(
                clipboard_offer = self.0.inventory.id.clip.seq,
                clipboard_generation = self.0.inventory.id.generation,
                clipboard_representation = id.0,
                bytes_received = slot.map_or(0, |slot| slot.received.load(Ordering::Acquire)),
                elapsed_ms = read.elapsed().as_millis() as u64,
                "native clipboard read timed out"
            );
        }
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
    fn translator(&self) -> Arc<dyn Translator> {
        Arc::new(NoTranslation)
    }
    fn namespace(&self) -> Option<Arc<dyn namespace::Namespace>> {
        (!self.0.inventory.namespace_roots_for(self.0.target).is_empty())
            .then(|| Arc::new(self.clone()) as Arc<dyn namespace::Namespace>)
    }
}
#[cfg(test)]
mod cancellation_tests {
    use super::*;
    #[test]
    fn native_retry_has_budget_without_resetting_file_watchdog() {
        use super::super::native_read::{Callback, TestClock, tests::Harness};
        let mut harness = Harness::new(ClipPlatform::Mac, false);
        let expired = tokio::time::Instant::now() - Duration::from_millis(NO_PROGRESS_MS + 1);
        *harness.remote.0.last_progress.lock().unwrap() = expired;
        let clock = Arc::new(TestClock::default());
        let expiry = harness.expire_when_waiting(clock.clone());
        clock.scope(|| {
            let _callback = Callback::begin();
            assert!(harness.remote.open(RepresentationId(1)).is_err());
        });
        expiry.join().unwrap();
        assert_eq!(*harness.remote.0.last_progress.lock().unwrap(), expired);
        harness.allow_retry();
        // Model a later paste after the prior transfer watchdog has expired,
        // without spending wall time or changing the offer's identity.
        *harness.remote.0.last_progress.lock().unwrap() =
            tokio::time::Instant::now() - Duration::from_millis(NO_PROGRESS_MS + 1);
        let callback = Callback::begin();
        assert_eq!(
            read_all(&mut harness.remote.open(RepresentationId(1)).unwrap()).unwrap(),
            harness.bytes
        );
        callback.served();
    }
    #[test]
    fn timeout_detaches_a_slot_even_when_the_async_watchdog_completed_first() {
        use super::super::native_read::{self, Callback, tests::Harness};
        let harness = Harness::new(ClipPlatform::Mac, false);
        let _callback = Callback::begin();
        let read = native_read::remote_read().unwrap();
        let slot = harness.remote.request(RepresentationId(1));
        // Reproduce the ordering where the async timer wins just before the
        // synchronous callback wakes. This must not cache a permanent failure.
        *slot.result.lock().unwrap() = Some(Err(Unavailable::NoProgress));
        harness.remote.native_timeout(RepresentationId(1), Some(&slot), &read);
        assert!(
            !harness
                .remote
                .0
                .slots
                .lock()
                .unwrap()
                .contains_key(&RepresentationId(1))
        );
        assert!(!harness.remote.0.cancelled.load(Ordering::Acquire));
    }
    #[tokio::test]
    async fn aborting_an_unpolled_download_wakes_its_native_reader() {
        let slot = Arc::new(Slot {
            diagnostic: ReadContext::default(),
            hover: false,
            abort: std::sync::OnceLock::new(),
            received: Default::default(),
            deadline_floor: None,
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
