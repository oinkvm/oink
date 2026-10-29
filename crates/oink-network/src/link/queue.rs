//! Bounded, lock-free handoff between a stream's writer and reader.
use super::{Closed, PostError, QueueSpec, Wake};
use arc_swap::ArcSwapOption;
use crossbeam_queue::ArrayQueue;
use oink_foundation::handoff_timing::{self, Event, Stage, Token};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use tokio::sync::Notify;

struct WakeSlot(Wake);

pub(super) struct Queued {
    pub bytes: Vec<u8>,
    pub pending: Option<oink_foundation::pending_age::Pending>,
    pub arrived: Option<std::time::Instant>,
    pub timing: Option<Token>,
}
impl Queued {
    fn len(&self) -> usize {
        self.bytes.len()
    }
}

pub(super) struct Queue {
    items: ArrayQueue<Queued>,
    pub(super) age: Arc<oink_foundation::pending_age::PendingAge>,
    spec: QueueSpec,
    bytes: AtomicUsize,
    closed: AtomicBool,
    wake: ArcSwapOption<WakeSlot>,
    producer_wake: ArcSwapOption<WakeSlot>,
    pub(super) notify: Notify,
    pub(super) dropped: AtomicU64,
    pub(super) full: AtomicU64,
    first_motion_epoch: AtomicU64,
}

impl std::fmt::Debug for Queue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queue")
            .field("spec", &self.spec)
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl Queue {
    pub(super) fn new(spec: QueueSpec) -> Arc<Self> {
        assert!(spec.messages > 0 && spec.bytes > 0);
        Arc::new(Self {
            items: ArrayQueue::new(spec.messages as usize),
            age: oink_foundation::pending_age::PendingAge::new(spec.messages as usize + 2),
            spec,
            bytes: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            wake: ArcSwapOption::empty(),
            producer_wake: ArcSwapOption::empty(),
            notify: Notify::new(),
            dropped: AtomicU64::new(0),
            full: AtomicU64::new(0),
            first_motion_epoch: AtomicU64::new(0),
        })
    }
    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    pub(super) fn len(&self) -> usize {
        self.items.len()
    }
    pub(super) fn fits(&self, len: usize) -> bool {
        self.len() < self.spec.messages as usize && self.bytes.load(Ordering::Acquire) + len <= self.spec.bytes as usize
    }
    pub(super) fn set_wake(&self, wake: Wake) {
        self.wake.store(Some(Arc::new(WakeSlot(wake))));
        if self.is_closed() || !self.items.is_empty() {
            self.signal();
        }
    }
    pub(super) fn set_producer_wake(&self, wake: Wake) {
        self.producer_wake.store(Some(Arc::new(WakeSlot(wake.clone()))));
        // Registration may race a drain; prime the owner once so it rechecks space.
        wake();
    }
    pub(super) fn signal(&self) {
        self.notify.notify_one();
        if let Some(wake) = self.wake.load().as_ref() {
            (wake.0)();
        }
    }
    pub(super) fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            while self.pop().is_some() {}
            self.signal();
            if let Some(wake) = self.producer_wake.load().as_ref() {
                (wake.0)();
            }
        }
    }
    pub(super) fn post(&self, encoded: Vec<u8>, limit: usize) -> Result<(), PostError> {
        // Also timestamps the in-memory stream test pair, where writer and reader
        // share a queue. Production senders never consume this local metadata.
        self.post_at(encoded, limit, Some(std::time::Instant::now()), Stage::Enqueue)
    }
    pub(super) fn post_received(&self, encoded: Vec<u8>, limit: usize) -> Result<(), PostError> {
        self.post_at(encoded, limit, Some(std::time::Instant::now()), Stage::QuicReceive)
    }
    fn post_at(
        &self,
        encoded: Vec<u8>,
        limit: usize,
        arrived: Option<std::time::Instant>,
        stage: Stage,
    ) -> Result<(), PostError> {
        if self.is_closed() {
            return Err(PostError::Closed);
        }
        if encoded.len() > limit {
            return Err(PostError::TooLarge);
        }
        if !self.fits(encoded.len()) {
            self.full.fetch_add(1, Ordering::Relaxed);
            return Err(PostError::Full);
        }
        let timing = if handoff_timing::enabled() {
            crossing_token(&encoded, &self.first_motion_epoch)
        } else {
            self.first_motion_epoch.store(0, Ordering::Relaxed);
            None
        };
        let pending = match arrived {
            Some(at) => Some(self.age.track(at).ok_or(PostError::Full)?),
            None => None,
        };
        let message = Queued {
            pending,
            bytes: encoded,
            arrived,
            timing,
        };
        self.bytes.fetch_add(message.len(), Ordering::AcqRel);
        // Stamp the insertion attempt before publishing to an already-running
        // consumer; its dequeue must not acquire an earlier capture timestamp.
        if let Some(token) = timing {
            handoff_timing::emit(Event::new(stage, token));
        }
        if let Err(message) = self.items.push(message) {
            self.bytes.fetch_sub(message.len(), Ordering::AcqRel);
            self.full.fetch_add(1, Ordering::Relaxed);
            return Err(PostError::Full);
        }
        self.signal();
        Ok(())
    }
    fn pop(&self) -> Option<Queued> {
        let message = self.items.pop()?;
        self.bytes.fetch_sub(message.len(), Ordering::AcqRel);
        Some(message)
    }
    pub(super) fn take(&self) -> Result<Option<Vec<u8>>, Closed> {
        self.take_scheduled().map(|item| item.map(|item| item.bytes))
    }
    pub(super) fn take_scheduled(&self) -> Result<Option<Queued>, Closed> {
        if self.is_closed() {
            return Err(Closed);
        }
        let message = self.pop();
        if message.is_some()
            && let Some(wake) = self.producer_wake.load().as_ref()
        {
            (wake.0)();
        }
        Ok(message)
    }
}

/// Fixed slots keep the realtime datagram handoff free of per-packet allocation.
struct Datagram {
    bytes: [u8; oink_protocol::v1::limits::DATAGRAM],
    len: usize,
    arrived: std::time::Instant,
    pending: oink_foundation::pending_age::Pending,
}
pub(super) struct DatagramQueue {
    partitions: Option<super::realtime_datagram::Partitions>,
    items: ArrayQueue<Datagram>,
    pub(super) age: Arc<oink_foundation::pending_age::PendingAge>,
    spec: QueueSpec,
    bytes: AtomicUsize,
    closed: AtomicBool,
    wake: ArcSwapOption<WakeSlot>,
    producer_wake: ArcSwapOption<WakeSlot>,
    pub(super) dropped: AtomicU64,
    pub(super) datagram_prefix: AtomicUsize,
}
impl std::fmt::Debug for DatagramQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatagramQueue")
            .field("spec", &self.spec)
            .field("closed", &self.is_closed())
            .finish()
    }
}
impl DatagramQueue {
    pub(super) fn new(spec: QueueSpec) -> Arc<Self> {
        Self::create(spec, None)
    }
    pub(super) fn new_partitioned(spec: QueueSpec, types: [[u16; 2]; 2]) -> Arc<Self> {
        Self::create(spec, Some(types))
    }
    fn create(spec: QueueSpec, types: Option<[[u16; 2]; 2]>) -> Arc<Self> {
        assert!(spec.messages > 0 && spec.bytes > 0);
        Arc::new(Self {
            partitions: types.map(|types| super::realtime_datagram::Partitions::new(spec, types)),
            items: ArrayQueue::new(if types.is_some() { 1 } else { spec.messages as usize }),
            age: oink_foundation::pending_age::PendingAge::new(if types.is_some() {
                1
            } else {
                spec.messages as usize + 2
            }),
            spec,
            bytes: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            wake: ArcSwapOption::empty(),
            producer_wake: ArcSwapOption::empty(),
            dropped: AtomicU64::new(0),
            datagram_prefix: AtomicUsize::new(4),
        })
    }
    pub(super) fn take_partition(
        &self,
        lane: usize,
        output: &mut [u8],
    ) -> Result<Option<(usize, std::time::Instant)>, super::DatagramError> {
        if self.is_closed() {
            return Err(super::DatagramError::Closed);
        }
        self.partitions
            .as_ref()
            .ok_or(super::DatagramError::Closed)?
            .take(lane, output)
    }
    pub(super) fn partition_dropped(&self, lane: usize) -> u64 {
        self.partitions.as_ref().map_or(0, |p| p.dropped(lane))
    }
    pub(super) fn oldest(&self) -> Option<std::time::Instant> {
        if self.is_closed() {
            return None;
        }
        self.partitions
            .as_ref()
            .map_or_else(|| self.age.oldest(), |p| p.oldest())
    }
    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    pub(super) fn len(&self) -> usize {
        self.partitions.as_ref().map_or_else(|| self.items.len(), |p| p.len())
    }
    pub(super) fn set_wake(&self, wake: Wake) {
        self.wake.store(Some(Arc::new(WakeSlot(wake))));
        if self.is_closed() || self.len() != 0 {
            self.signal();
        }
    }
    pub(super) fn set_producer_wake(&self, wake: Wake) {
        self.producer_wake.store(Some(Arc::new(WakeSlot(wake.clone()))));
        wake();
    }
    fn signal(&self) {
        if let Some(wake) = self.wake.load().as_ref() {
            (wake.0)();
        }
    }
    fn producer_signal(&self) {
        if let Some(wake) = self.producer_wake.load().as_ref() {
            (wake.0)();
        }
    }
    fn pop(&self) -> Option<Datagram> {
        let item = self.items.pop()?;
        self.bytes.fetch_sub(item.len, Ordering::AcqRel);
        self.producer_signal();
        Some(item)
    }
    pub(super) fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            while self.pop().is_some() {}
            self.signal();
            self.producer_signal();
        }
    }
    pub(super) fn post_lossy(&self, encoded: &[u8]) -> Result<(), super::DatagramError> {
        self.post_lossy_at(encoded, std::time::Instant::now())
    }
    pub(super) fn post_lossy_at(
        &self,
        encoded: &[u8],
        arrived: std::time::Instant,
    ) -> Result<(), super::DatagramError> {
        if self.is_closed() {
            return Err(super::DatagramError::Closed);
        }
        if encoded.len() + self.datagram_prefix.load(Ordering::Acquire) > oink_protocol::v1::limits::DATAGRAM
            || encoded.len() > self.spec.bytes as usize
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(super::DatagramError::TooLarge);
        }
        if let Some(partitions) = &self.partitions {
            if !partitions.post(encoded, arrived) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            self.signal(); // network producer only; the RT consumer never wakes it
            return Ok(());
        }
        while self.items.len() >= self.spec.messages as usize
            || self.bytes.load(Ordering::Acquire) + encoded.len() > self.spec.bytes as usize
        {
            // The consumer may be between removing the last slot and accounting
            // its bytes. Never wait for that consumer to be scheduled.
            if self.pop().is_none() {
                break;
            }
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        let arrived = std::time::Instant::now();
        let Some(pending) = self.age.track(arrived) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        let mut item = Datagram {
            pending,
            bytes: [0; oink_protocol::v1::limits::DATAGRAM],
            len: encoded.len(),
            arrived,
        };
        item.bytes[..encoded.len()].copy_from_slice(encoded);
        self.bytes.fetch_add(encoded.len(), Ordering::AcqRel);
        if let Err(item) = self.items.push(item) {
            self.bytes.fetch_sub(item.len, Ordering::AcqRel);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.signal();
        Ok(())
    }
    pub(super) fn take_into(&self, output: &mut [u8]) -> Result<Option<usize>, super::DatagramError> {
        self.take_timed_into(output).map(|item| item.map(|(len, _)| len))
    }
    pub(super) fn take_timed_into(
        &self,
        output: &mut [u8],
    ) -> Result<Option<(usize, std::time::Instant)>, super::DatagramError> {
        if self.is_closed() {
            return Err(super::DatagramError::Closed);
        }
        let Some(item) = self.pop() else {
            return Ok(None);
        };
        let Some(buffer) = output.get_mut(..item.len) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(super::DatagramError::TooLarge);
        };
        buffer.copy_from_slice(&item.bytes[..item.len]);
        Ok(Some((item.len, item.arrived)))
    }
    pub(super) fn take(&self) -> Result<Option<Vec<u8>>, Closed> {
        self.take_scheduled().map(|item| item.map(|item| item.bytes))
    }
    pub(super) fn take_scheduled(&self) -> Result<Option<Queued>, Closed> {
        if self.is_closed() {
            return Err(Closed);
        }
        Ok(self.pop().map(|item| Queued {
            pending: Some(item.pending),
            bytes: item.bytes[..item.len].to_vec(),
            arrived: Some(item.arrived),
            timing: None,
        }))
    }
}

/// Registered numeric fields only; never retain or decode a payload into an owning tree.
/// The fixed IDs are frozen protocol allocations. Tests tie them to the registry.
fn crossing_token(bytes: &[u8], first_motion_epoch: &AtomicU64) -> Option<Token> {
    use oink_protocol::v1::{frame::OpaqueFrame, hot::Map, limits};
    let kind = u16::from_le_bytes(bytes.get(..2)?.try_into().ok()?);
    if kind != 0x0101 && (kind != 0x0201 || first_motion_epoch.load(Ordering::Relaxed) == 0) {
        return None;
    }
    let frame = OpaqueFrame::parse_exact(bytes, limits::RELIABLE_FRAME).ok()?;
    let value = Map::parse(frame.payload(), limits::RELIABLE_FRAME)
        .ok()?
        .required::<u64>(1)
        .ok()?;
    if kind == 0x0101 {
        first_motion_epoch.store(value, Ordering::Relaxed);
        Some(Token {
            epoch: value,
            sequence: 0,
        })
    } else {
        let epoch = first_motion_epoch.swap(0, Ordering::Relaxed);
        (epoch != 0).then_some(Token { epoch, sequence: value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realtime_datagram_read_never_notifies_producer() {
        let queue = DatagramQueue::new_partitioned(
            QueueSpec {
                messages: 8,
                bytes: 8800,
            },
            [[0x0604, 0x0606], [0x0618, 0x0619]],
        );
        let wakes = Arc::new(AtomicUsize::new(0));
        let observe = wakes.clone();
        queue.set_producer_wake(Arc::new(move || {
            observe.fetch_add(1, Ordering::Relaxed);
        }));
        let baseline = wakes.load(Ordering::Relaxed);
        let mut bytes = [0; 1100];
        let size = oink_protocol::v1::hot::encode_into(
            0x0606,
            &[
                (1, oink_protocol::v1::hot::Value::Uint(1)),
                (2, oink_protocol::v1::hot::Value::Uint(1)),
                (3, oink_protocol::v1::hot::Value::Uint(1)),
            ],
            &mut bytes,
            1100,
        )
        .unwrap();
        queue.post_lossy(&bytes[..size]).unwrap();
        assert!(queue.take_partition(0, &mut bytes).unwrap().is_some());
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            baseline,
            "RT reader woke ordinary producer"
        );
    }

    #[test]
    fn reliable_and_datagram_ages_include_claimed_work_and_clear_on_discard() {
        let queue = Queue::new(QueueSpec {
            messages: 2,
            bytes: 100,
        });
        let at = std::time::Instant::now();
        queue.post_at(vec![1], 100, Some(at), Stage::Enqueue).unwrap();
        let flight = queue.take_scheduled().unwrap().unwrap();
        assert_eq!(queue.age.oldest(), Some(at));
        queue.post(vec![2], 100).unwrap();
        queue.close();
        assert_eq!(queue.age.oldest(), Some(at));
        drop(flight);
        assert_eq!(queue.age.oldest(), None);
        let queue = DatagramQueue::new(QueueSpec {
            messages: 1,
            bytes: 100,
        });
        queue.post_lossy(&[1]).unwrap();
        let flight = queue.take_scheduled().unwrap().unwrap();
        let oldest = queue.age.oldest().unwrap();
        for _ in 0..5 {
            queue.post_lossy(&[2]).unwrap();
        }
        assert_eq!(queue.age.oldest(), Some(oldest));
        queue.close();
        drop(flight);
        assert_eq!(queue.age.oldest(), None);
    }

    #[test]
    fn crossing_timing_keeps_only_focus_and_first_motion_numeric_identity() {
        fn fixture(hex: &str) -> Vec<u8> {
            let hex = hex.trim();
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect()
        }
        assert_eq!(
            oink_protocol::v1::registry::message(0x0101).unwrap().name,
            "ControlFocusEnter"
        );
        assert_eq!(
            oink_protocol::v1::registry::message(0x0201).unwrap().name,
            "InputMotion"
        );
        let focus = fixture(include_str!("../../../oink-protocol/tests/fixtures/v1/0101-future.hex"));
        let motion = fixture(include_str!("../../../oink-protocol/tests/fixtures/v1/0201-future.hex"));
        let pending = AtomicU64::new(0);
        assert_eq!(crossing_token(&motion, &pending), None);
        assert_eq!(crossing_token(&focus, &pending), Some(Token { epoch: 1, sequence: 0 }));
        // Invalid numeric identity must not consume the first-motion marker.
        assert_eq!(crossing_token(&[1, 2, 4, 0, 0, 0, 0xa1, 1, 0x61, b'x'], &pending), None);
        assert_eq!(pending.load(Ordering::Relaxed), 1);
        assert_eq!(crossing_token(&motion, &pending), Some(Token { epoch: 1, sequence: 1 }));
        assert_eq!(crossing_token(&motion, &pending), None);
    }

    #[test]
    fn receive_timestamp_is_preserved_through_queue_wait() {
        let queue = Queue::new(QueueSpec {
            messages: 2,
            bytes: 100,
        });
        let at = std::time::Instant::now();
        queue.post_at(vec![1, 2], 100, Some(at), Stage::QuicReceive).unwrap();
        let queued = queue.take_scheduled().unwrap().unwrap();
        assert_eq!(queued.arrived, Some(at));
        assert_eq!(queued.bytes, [1, 2]);
    }

    #[test]
    fn lossy_post_does_not_wait_for_a_consumer_paused_after_removing_the_last_item() {
        let queue = DatagramQueue::new(QueueSpec {
            messages: 1,
            bytes: 100,
        });
        queue.post_lossy(&[1; 100]).unwrap();
        // Pause a consumer between ArrayQueue::pop and byte accounting.
        let claimed = queue.items.pop().unwrap();
        let producer = queue.clone();
        let (done, completed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = done.send(producer.post_lossy(&[2; 100]));
        });
        let result = completed.recv_timeout(std::time::Duration::from_secs(1));
        // Always release the paused consumer, even for the old spinning behavior.
        queue.bytes.fetch_sub(claimed.len, Ordering::AcqRel);
        worker.join().unwrap();
        assert_eq!(result.unwrap(), Ok(()));
        assert_eq!(queue.take().unwrap(), Some(vec![2; 100]));
        assert_eq!(queue.bytes.load(Ordering::Acquire), 0);
    }
    #[test]
    fn borrowed_datagrams_obey_limits_eviction_and_closure() {
        let queue = DatagramQueue::new(QueueSpec { messages: 2, bytes: 6 });
        let mut output = [0; 1100];
        queue.post_lossy(&[1, 2, 3]).unwrap();
        queue.post_lossy(&[4, 5, 6]).unwrap();
        queue.post_lossy(&[7, 8, 9]).unwrap();
        assert_eq!(queue.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(queue.take_into(&mut output).unwrap(), Some(3));
        assert_eq!(&output[..3], &[4, 5, 6]);
        assert_eq!(
            queue.take_into(&mut output[..2]),
            Err(super::super::DatagramError::TooLarge)
        );
        assert_eq!(queue.take_into(&mut output).unwrap(), None);
        assert_eq!(queue.post_lossy(&[0; 7]), Err(super::super::DatagramError::TooLarge));
        queue.close();
        assert_eq!(queue.post_lossy(&[1]), Err(super::super::DatagramError::Closed));
        assert_eq!(queue.take_into(&mut output), Err(super::super::DatagramError::Closed));
    }
}
