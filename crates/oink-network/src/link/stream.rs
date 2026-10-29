//! Stream handles: the queue ends used by a stream's one writer thread and one reader
//! thread. None of these calls goes through the command queue and none
//! of them blocks, takes a lock shared with an ordinary thread, or allocates beyond the
//! message itself; they are safe on realtime threads.
//!
//! A queue hands messages between two threads, reports "full" at once, and holds
//! messages during a cut-over. It guarantees no delivery: closing a stream discards what
//! is queued and in flight.
use super::queue::{DatagramQueue, Queue};
use super::types::{Closed, LinkId, PostError, QueueSpec, StreamId};
use oink_foundation::DeviceId;
use oink_protocol::frame::{Priority, StreamCode, StreamTag};
use std::sync::{Arc, atomic::Ordering};

/// Called by the network thread when a queue went from empty to non-empty or the handle
/// closed. Must return quickly and must not block (signal an event source, unpark, or
/// notify a task).
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Writer end of a reliable stream this device opened. Owned by exactly one thread.
/// The QUIC stream and its header appear on the wire at the first `post`.
#[derive(Debug)]
pub struct SendStream {
    pub(super) id: StreamId,
    pub(super) queue: Arc<Queue>,
}

impl SendStream {
    /// Includes a reliable writer's pending frame until QUIC accepts it.
    pub fn oldest_pending(&self) -> Option<std::time::Instant> {
        self.queue.age.oldest()
    }

    pub fn id(&self) -> StreamId {
        self.id
    }
    /// Wake the single writer when the queue frees space or closes. Replaces its previous wake.
    pub fn set_wake(&self, wake: Wake) {
        self.queue.set_producer_wake(wake);
    }
    /// Queue one encoded `Message` and wake the network thread. Returns at once.
    /// During a cut-over messages stay queued; `Full` is reported as usual.
    pub fn post(&self, encoded: impl Into<Vec<u8>>) -> Result<(), PostError> {
        self.queue.post(encoded.into(), self.id.tag.kind.max_message())
    }
}

/// Reader end of a reliable stream the peer opened. Owned by exactly one thread.
/// When its queue is full the network thread stops reading the QUIC stream and QUIC
/// blocks the peer; nothing is dropped.
#[derive(Debug)]
pub struct RecvStream {
    pub(super) id: StreamId,
    pub(super) queue: Arc<Queue>,
}

impl RecvStream {
    /// Includes a reliable writer's pending frame until QUIC accepts it.
    pub fn oldest_pending(&self) -> Option<std::time::Instant> {
        self.queue.age.oldest()
    }

    pub fn id(&self) -> StreamId {
        self.id
    }
    /// Next whole message in order, `Ok(None)` when the queue is empty, `Err(Closed)` once
    /// the handle is closed and drained of nothing: a close discards queued messages.
    pub fn take(&mut self) -> Result<Option<Vec<u8>>, Closed> {
        self.queue.take()
    }
    /// Complete-message arrival at the network receive queue, before the reader
    /// is scheduled. This measures no TCP/QUIC work before the network owner runs.
    pub fn take_timed(&mut self) -> Result<Option<(Vec<u8>, std::time::Instant)>, Closed> {
        self.queue.take_scheduled().map(|item| {
            item.map(|item| {
                if let Some(token) = item.timing {
                    use oink_foundation::handoff_timing::{self, Event, Stage};
                    let mut event = Event::new(Stage::InputDispatch, token);
                    event.link = self.id.link.0;
                    event.stream = self.id.tag.ordinal;
                    event.elapsed_us = item.arrived.map_or(0, |at| at.elapsed().as_micros() as u64);
                    handoff_timing::emit(event);
                }
                (
                    item.bytes,
                    item.arrived.expect("reliable receive messages are timestamped"),
                )
            })
        })
    }
    /// Install the wake-up for this reader, replacing an earlier one. Realtime readers
    /// pass their platform event source; ordinary tasks use [`RecvStream::next`] instead.
    pub fn set_wake(&mut self, wake: Wake) {
        self.queue.set_wake(wake)
    }
    /// Wait for the next message. For ordinary-thread tasks only.
    pub async fn next(&mut self) -> Result<Vec<u8>, Closed> {
        loop {
            let notified = self.queue.notify.notified();
            if let Some(message) = self.queue.take()? {
                return Ok(message);
            }
            notified.await;
        }
    }
}

/// `DatagramSend::post` did not take the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DatagramError {
    /// With its flow number it would exceed `frame::MAX_DATAGRAM`. Refused and counted.
    #[error("datagram too large")]
    TooLarge,
    #[error("stream closed")]
    Closed,
}

/// Writer end of the datagram flow that follows a reliable stream; closed with it.
/// A full queue drops its oldest message and counts it; `post` does not report that.
/// Quinn may also silently evict its oldest buffered datagram; those drops are not counted
/// here. During a cut-over datagrams are dropped.
// Clones only share ownership; each flow still has one writer and one reader.
// Publishing a new audio snapshot must not close the old snapshot's live flow.
#[derive(Debug)]
pub(super) struct DatagramOwner(pub(super) Arc<DatagramQueue>);
impl Drop for DatagramOwner {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[derive(Clone, Debug)]
pub struct DatagramSend {
    pub(super) follows: StreamId,
    pub(super) _owner: Arc<DatagramOwner>,
    pub(super) queue: Arc<DatagramQueue>,
    pub(super) parent: Option<Arc<Queue>>,
}

impl DatagramSend {
    /// Includes a reliable writer's pending frame until QUIC accepts it.
    pub fn oldest_pending(&self) -> Option<std::time::Instant> {
        self.queue.age.oldest()
    }

    /// Wake the single writer when queued datagrams are consumed or the flow closes.
    pub fn set_wake(&self, wake: Wake) {
        self.queue.set_producer_wake(wake);
    }
    /// The reliable stream this flow follows.
    pub fn follows(&self) -> StreamId {
        self.follows
    }
    pub fn post(&self, encoded: impl AsRef<[u8]>) -> Result<(), DatagramError> {
        let encoded = encoded.as_ref();
        if self.queue.is_closed() || self.parent.as_ref().is_some_and(|q| q.is_closed()) {
            return Err(DatagramError::Closed);
        }
        if encoded.len() + self.queue.datagram_prefix.load(Ordering::Acquire) > oink_protocol::frame::MAX_DATAGRAM {
            self.queue.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(DatagramError::TooLarge);
        }
        self.queue.post_lossy(encoded)
    }
}

/// Reader end of a datagram flow. Unordered, lossy; a full queue drops its oldest.
/// Datagrams that arrive before the declaring stream header are dropped.
/// Like the writer end it is used through a shared reference (it is handed to the audio
/// threads inside a snapshot), by exactly one reader thread.
#[derive(Clone, Debug)]
pub struct DatagramRecv {
    pub(super) partition: Option<usize>,
    pub(super) follows: StreamId,
    pub(super) _owner: Arc<DatagramOwner>,
    pub(super) queue: Arc<DatagramQueue>,
    pub(super) parent: Option<Arc<Queue>>,
}

impl DatagramRecv {
    /// Includes a reliable writer's pending frame until QUIC accepts it.
    pub fn oldest_pending(&self) -> Option<std::time::Instant> {
        self.queue.oldest()
    }

    pub fn follows(&self) -> StreamId {
        self.follows
    }
    pub fn take(&self) -> Result<Option<Vec<u8>>, Closed> {
        self.take_timed().map(|item| item.map(|(bytes, _)| bytes))
    }
    /// Copy into caller-owned storage without allocating. A short output buffer
    /// drops the datagram and returns TooLarge; a 1100-byte buffer always fits.
    pub fn take_into(&self, output: &mut [u8]) -> Result<Option<usize>, DatagramError> {
        if self.parent.as_ref().is_some_and(|q| q.is_closed()) {
            return Err(DatagramError::Closed);
        }
        self.take_timed_into(output).map(|item| item.map(|(len, _)| len))
    }
    /// Copy into caller storage and retain the network insertion timestamp.
    pub fn take_timed_into(&self, output: &mut [u8]) -> Result<Option<(usize, std::time::Instant)>, DatagramError> {
        if self.parent.as_ref().is_some_and(|q| q.is_closed()) {
            return Err(DatagramError::Closed);
        }
        match self.partition {
            Some(lane) => self.queue.take_partition(lane, output),
            None => self.queue.take_timed_into(output),
        }
    }
    /// Arrival is stamped at queue insertion, not when a restarting device reads it.
    pub fn take_timed(&self) -> Result<Option<(Vec<u8>, std::time::Instant)>, Closed> {
        if self.parent.as_ref().is_some_and(|q| q.is_closed()) {
            return Err(Closed);
        }
        if self.partition.is_some() {
            let mut bytes = [0; oink_protocol::v1::limits::DATAGRAM];
            return self
                .take_timed_into(&mut bytes)
                .map_err(|_| Closed)
                .map(|item| item.map(|(len, at)| (bytes[..len].to_vec(), at)));
        }
        self.queue.take_scheduled().map(|item| {
            item.map(|item| {
                (
                    item.bytes,
                    item.arrived.expect("datagrams are timestamped at insertion"),
                )
            })
        })
    }
    pub fn dropped(&self) -> u64 {
        self.partition.map_or_else(
            || self.queue.dropped.load(Ordering::Acquire),
            |lane| self.queue.partition_dropped(lane),
        )
    }
    pub fn set_wake(&self, wake: Wake) {
        self.queue.set_wake(wake)
    }
}

/// A stream the peer opened on a ready link, delivered to the module registered for its
/// kind. The network thread has read only its header. The module answers from an
/// ordinary thread with `accept` or `reject`; until it does, the stream's content is not
/// read, and a cut-over of this link cannot finish (it times out after 500 ms).
/// Dropping it without an answer rejects it with `StreamCode::None`.
#[derive(Debug)]
pub struct InboundStream {
    pub link: LinkId,
    pub peer: DeviceId,
    pub tag: StreamTag,
    pub priority: Priority,
    /// The header declared a datagram flow following this stream.
    pub has_datagram: bool,
    pub(super) command: Option<tokio::sync::mpsc::UnboundedSender<super::handle::Command>>,
    pub(super) wake: Arc<tokio::sync::Notify>,
}

impl InboundStream {
    /// Allocate the receive queue(s) on the calling thread and tell the network thread to
    /// start reading. `datagram` is used only when `has_datagram`.
    pub fn accept(self, spec: QueueSpec, datagram: QueueSpec) -> (RecvStream, Option<DatagramRecv>) {
        self.accept_inner(spec, datagram, None)
    }
    /// Fixed-copy M1 lanes selected by registered message IDs on the network
    /// producer. No business task or competing audio callback demultiplexes them.
    pub fn accept_partitioned(
        self,
        spec: QueueSpec,
        datagram: QueueSpec,
        types: [[u16; 2]; 2],
    ) -> (RecvStream, Option<[DatagramRecv; 2]>) {
        let (stream, packets) = self.accept_inner(spec, datagram, Some(types));
        (
            stream,
            packets.map(|first| {
                let mut second = first.clone();
                second.partition = Some(1);
                [first, second]
            }),
        )
    }
    fn accept_inner(
        mut self,
        spec: QueueSpec,
        datagram: QueueSpec,
        types: Option<[[u16; 2]; 2]>,
    ) -> (RecvStream, Option<DatagramRecv>) {
        let id = StreamId {
            link: self.link,
            dir: super::StreamDir::In,
            tag: self.tag,
        };
        let queue = Queue::new(spec);
        let datagram = self.has_datagram.then(|| {
            types.map_or_else(
                || DatagramQueue::new(datagram),
                |types| DatagramQueue::new_partitioned(datagram, types),
            )
        });
        if let Some(command) = self.command.take() {
            let _ = command.send(super::handle::Command::Accept {
                id,
                queue: queue.clone(),
                datagram: datagram.clone(),
            });
            self.wake.notify_one();
        }
        let parent = queue.clone();
        (
            RecvStream { id, queue },
            datagram.map(|queue| DatagramRecv {
                partition: types.map(|_| 0),
                follows: id,
                _owner: Arc::new(DatagramOwner(queue.clone())),
                queue,
                parent: Some(parent),
            }),
        )
    }
    /// Refuse the stream; the writer sees `StreamClosed::Peer(code)`.
    pub fn reject(mut self, code: StreamCode) {
        if let Some(command) = self.command.take() {
            let _ = command.send(super::handle::Command::Close(
                StreamId {
                    link: self.link,
                    dir: super::StreamDir::In,
                    tag: self.tag,
                },
                code,
            ));
            self.wake.notify_one();
        }
    }
}

impl Drop for InboundStream {
    fn drop(&mut self) {
        if let Some(command) = self.command.take() {
            let _ = command.send(super::handle::Command::Close(
                StreamId {
                    link: self.link,
                    dir: super::StreamDir::In,
                    tag: self.tag,
                },
                StreamCode::None,
            ));
            self.wake.notify_one();
        }
    }
}
macro_rules! close_on_drop {
    ($($kind:ty),* $(,)?) => { $(impl Drop for $kind { fn drop(&mut self) { self.queue.close(); } })* };
}
close_on_drop!(SendStream, RecvStream);
