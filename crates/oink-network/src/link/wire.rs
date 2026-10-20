//! Poll-driven QUIC framing. Each visit does at most one message's work.
use super::*;
use oink_foundation::handoff_timing::{self, Event, Stage, Token};
use oink_protocol::{frame::StreamHeader, v1::frame};
use std::{
    collections::VecDeque,
    future::Future,
    pin::{Pin, pin},
    task::{Context, Poll},
};

/// Retain Notify registrations until completion. Dropping a Pending Quinn future
/// unregisters its waiter; a polling timer used to hide those missed wake-ups.
pub(super) struct Waiter<T>(Option<Pin<Box<dyn Future<Output = T>>>>);
impl<T> Default for Waiter<T> {
    fn default() -> Self {
        Self(None)
    }
}
impl<T> Waiter<T> {
    pub fn poll(&mut self, cx: &mut Context<'_>, make: impl FnOnce() -> Pin<Box<dyn Future<Output = T>>>) -> Poll<T> {
        let result = self.0.get_or_insert_with(make).as_mut().poll(cx);
        if result.is_ready() {
            self.0 = None;
        }
        result
    }
}
pub(super) type Opening = Waiter<Result<quinn::SendStream, quinn::ConnectionError>>;
pub(super) type Stopping = Waiter<Result<Option<quinn::VarInt>, quinn::StoppedError>>;
pub(super) fn poll<T>(future: impl Future<Output = T>, cx: &mut Context<'_>) -> Poll<T> {
    pin!(future).poll(cx)
}
pub(super) fn code(code: StreamCode) -> quinn::VarInt {
    quinn::VarInt::from_u64(code.to_wire()).unwrap_or_else(|_| 4u32.into())
}
pub(super) fn read_error(error: quinn::ReadError) -> StreamClosed {
    match error {
        quinn::ReadError::Reset(c) => StreamClosed::Peer(StreamCode::from_wire(c.into_inner())),
        _ => StreamClosed::LinkLost,
    }
}
pub(super) struct PoolSource {
    pub receiver: tokio::sync::mpsc::Receiver<super::tcp::port::ControlQueued>,
    pub advert: Arc<arc_swap::ArcSwapOption<super::tcp::port::ControlInfo>>,
    pub port: Arc<super::tcp::port::Port>,
    pub prefer_info: bool,
}
impl PoolSource {
    fn take_advert(&self) -> Option<Arc<super::tcp::port::ControlInfo>> {
        // Publishing also wakes the network owner, so an empty load can wait
        // for the next visit without an atomic swap on every idle poll.
        if self.advert.load().is_none() {
            return None;
        }
        self.advert.swap(None)
    }
    pub(super) fn take(&mut self) -> Option<queue::Queued> {
        let info = if self.prefer_info || self.receiver.is_empty() {
            self.take_advert()
        } else {
            None
        };
        let (grant, message, reply) = if let Some(info) = info {
            self.prefer_info = false;
            (info.grant, info.message.clone(), None)
        } else if let Ok(queued) = self.receiver.try_recv() {
            let item = queued.send;
            self.prefer_info = true;
            (item.grant, item.message, Some(item.reply))
        } else {
            let info = self.take_advert()?;
            self.prefer_info = false;
            (info.grant, info.message.clone(), None)
        };
        if !self.port.low_current(grant) {
            if let Some(reply) = reply {
                let _ = reply.send(Err(crate::NetError::Closed));
            }
            return None;
        }
        let bytes = super::control_wire::encode(&super::control::LinkMsg::Low {
            binding: grant.binding,
            message: Box::new(message),
        })
        .ok();
        if let Some(reply) = reply {
            let _ = reply.send(if bytes.is_some() {
                Ok(())
            } else {
                Err(crate::NetError::Closed)
            });
        }
        bytes.map(|bytes| queue::Queued {
            pending: None,
            bytes,
            arrived: None,
            timing: None,
        })
    }
}
pub(super) enum Source {
    User(Arc<queue::Queue>),
    Private(VecDeque<queue::Queued>),
    Pool(PoolSource),
}
pub(super) struct Writer {
    pub uid: usize,
    pub header: StreamHeader,
    pub id: Option<StreamId>,
    pub source: Source,
    pub datagram: Option<Arc<queue::DatagramQueue>>,
    pub stream: Option<quinn::SendStream>,
    pending: Vec<u8>,
    pending_age: Option<oink_foundation::pending_age::Pending>,
    timing: Option<(Token, std::time::Instant)>,
    timing_started: bool,
    timing_blocked: bool,
    opening: Opening,
    stopping: Stopping,
    offset: usize,
    pub finished: bool,
    config_drain: usize,
}
impl Writer {
    pub fn new(
        uid: usize,
        header: StreamHeader,
        id: Option<StreamId>,
        source: Source,
        datagram: Option<Arc<queue::DatagramQueue>>,
    ) -> Self {
        Self {
            uid,
            header,
            id,
            source,
            datagram,
            stream: None,
            pending: Vec::new(),
            pending_age: None,
            timing: None,
            timing_started: false,
            timing_blocked: false,
            opening: Opening::default(),
            stopping: Stopping::default(),
            offset: 0,
            finished: false,
            config_drain: 0,
        }
    }
    pub fn private_post(&mut self, bytes: queue::Queued) {
        if let Source::Private(q) = &mut self.source {
            q.push_back(bytes);
        }
    }
    pub fn freeze(&mut self) {
        // Ready may be queued immediately after ConfigDone. Deliver the C0
        // prefix already accepted before freeze so the peer can admit and
        // drain its held business readers. Later traffic stays for the new path.
        if self.header.tag.kind == StreamKind::Config
            && let Source::User(queue) = &self.source
        {
            self.config_drain = queue.len();
        }
    }
    pub fn abort(&mut self, why: StreamCode) {
        if let Some(stream) = &mut self.stream {
            let _ = stream.reset(code(why));
        }
        self.close_queues();
    }
    pub fn close_queues(&self) {
        if let Source::User(q) = &self.source {
            q.close();
        }
        if let Some(q) = &self.datagram {
            q.close();
        }
    }
    pub fn rebind(&mut self, uid: usize, route: u64, flow: u32) {
        self.uid = uid;
        self.header.route_tag = route;
        self.header.datagram_flow = flow;
        if let Some(queue) = &self.datagram {
            queue.datagram_prefix.store(4, std::sync::atomic::Ordering::Release);
        }
        self.stream = None;
        self.opening = Opening::default();
        self.stopping = Stopping::default();
        self.finished = false;
        self.config_drain = 0;
        self.pending.clear();
        self.pending_age = None;
        self.timing = None;
        self.offset = 0;
    }
    fn check_stopped(&mut self, cx: &mut Context<'_>) -> Result<(), StreamClosed> {
        if let Some(stream) = &mut self.stream {
            match self.stopping.poll(cx, || Box::pin(stream.stopped())) {
                Poll::Ready(Ok(Some(c))) => {
                    let _ = stream.reset(c);
                    return Err(StreamClosed::Peer(StreamCode::from_wire(c.into_inner())));
                }
                Poll::Ready(Err(_)) => return Err(StreamClosed::LinkLost),
                _ => {}
            }
        }
        Ok(())
    }
    pub fn step(&mut self, conn: &quinn::Connection, freeze: bool, cx: &mut Context<'_>) -> Result<bool, StreamClosed> {
        if let Source::User(q) = &self.source
            && q.is_closed()
        {
            return Err(StreamClosed::Local);
        }
        if self.finished {
            self.check_stopped(cx)?;
            return Ok(false);
        }
        if freeze && self.config_drain == 0 && self.pending.is_empty() {
            self.check_stopped(cx)?;
            if let Some(stream) = &mut self.stream {
                stream.finish().map_err(|_| StreamClosed::Local)?;
            }
            self.finished = true;
            return Ok(true);
        }
        if self.pending.is_empty() {
            let next = match &mut self.source {
                Source::User(q) => q.take_scheduled().map_err(|_| StreamClosed::Local)?,
                Source::Private(q) => q.pop_front(),
                Source::Pool(q) => q.take(),
            };
            let Some(next) = next else {
                self.check_stopped(cx)?;
                return Ok(false);
            };
            self.config_drain = self.config_drain.saturating_sub(1);
            self.pending_age = next.pending;
            self.timing = next.timing.zip(next.arrived);
            self.timing_started = false;
            self.timing_blocked = false;
            self.timing_record(Stage::NetworkDequeue, conn);
            if self.stream.is_none() {
                self.header.encode(&mut self.pending);
            }
            self.pending.extend_from_slice(&next.bytes);
        }
        if self.stream.is_none() {
            let stream = match self.opening.poll(cx, || {
                let conn = conn.clone();
                Box::pin(async move { conn.open_uni().await })
            }) {
                Poll::Ready(Ok(s)) => s,
                Poll::Ready(Err(_)) => return Err(StreamClosed::LinkLost),
                Poll::Pending => return Ok(false),
            };
            let _ = stream.set_priority(if self.header.priority == Priority::Interactive {
                10
            } else {
                0
            });
            self.stream = Some(stream);
        }
        if !self.timing_started {
            self.timing_started = true;
            self.timing_record(Stage::QuicWriteStarted, conn);
        }
        let stream = self.stream.as_mut().expect("opened");
        match poll(stream.write(&self.pending[self.offset..]), cx) {
            Poll::Ready(Ok(n)) => {
                self.offset += n;
                if self.offset == self.pending.len() {
                    self.timing_record(Stage::QuicWriteAccepted, conn);
                    self.pending.clear();
                    self.pending_age = None;
                    self.offset = 0;
                    self.timing = None;
                }
                Ok(true)
            }
            Poll::Ready(Err(quinn::WriteError::Stopped(c))) => {
                Err(StreamClosed::Peer(StreamCode::from_wire(c.into_inner())))
            }
            Poll::Ready(Err(_)) => Err(StreamClosed::LinkLost),
            Poll::Pending => {
                if !self.timing_blocked {
                    self.timing_blocked = true;
                    self.timing_record(Stage::QuicWriteBlocked, conn);
                }
                Ok(false)
            }
        }
    }
    fn timing_record(&self, stage: Stage, conn: &quinn::Connection) {
        let Some((token, queued)) = self.timing else { return };
        let mut event = Event::new(stage, token);
        if let Some(id) = self.id {
            event.link = id.link.0;
            event.stream = id.tag.ordinal;
        }
        event.elapsed_us = queued.elapsed().as_micros() as u64;
        let stats = conn.stats();
        event.sent_packets = stats.path.sent_packets;
        event.lost_packets = stats.path.lost_packets;
        event.rtt_us = conn.rtt().as_micros() as u64;
        handoff_timing::emit(event);
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        if !self.finished
            && let Some(s) = &mut self.stream
        {
            let _ = s.reset(0u32.into());
        }
    }
}

pub(super) enum Delivery {
    Header(frame::StreamHeader),
    Message(Vec<u8>),
    Finished,
    Progress,
    Pending,
}
pub(super) struct Reader {
    pub uid: usize,
    pub stream: quinn::RecvStream,
    pub header: Option<StreamHeader>,
    pub id: Option<StreamId>,
    pub queue: Option<Arc<queue::Queue>>,
    pub datagram: Option<Arc<queue::DatagramQueue>>,
    pub finished: bool,
    pub pending_low: Option<super::tcp::port::Delivery>,
    pub pending_admission: bool,
    prefix: Vec<u8>,
    body: Vec<u8>,
    length: Option<usize>,
}
impl Reader {
    pub fn new(uid: usize, stream: quinn::RecvStream) -> Self {
        Self {
            uid,
            stream,
            header: None,
            id: None,
            queue: None,
            datagram: None,
            finished: false,
            pending_low: None,
            pending_admission: false,
            prefix: Vec::new(),
            body: Vec::new(),
            length: None,
        }
    }
    pub fn stop(&mut self, why: StreamCode) {
        let _ = self.stream.stop(code(why));
        if let Some(q) = &self.queue {
            q.close();
        }
        if let Some(q) = &self.datagram {
            q.close();
        }
    }
    fn check_reset(&mut self, cx: &mut Context<'_>) -> Result<(), StreamClosed> {
        // Observe RESET without consuming payload while full or unanswered. Quinn
        // 0.11's reset-only waiter can survive a subsequent buffered FIN read and
        // panic on drop. A zero-length ordered chunk uses the normal read wakeup
        // rules and consumes no bytes, including while payload is already buffered.
        if let Poll::Ready(Err(error)) = poll(self.stream.read_chunk(0, true), cx) {
            return Err(read_error(error));
        }
        Ok(())
    }
    pub fn step(
        &mut self,
        cx: &mut Context<'_>,
        mut admit: impl FnMut(StreamHeader, frame::FrameHeader) -> Result<(), StreamClosed>,
    ) -> Result<Delivery, StreamClosed> {
        if let Some(q) = &self.queue
            && q.is_closed()
        {
            return Err(StreamClosed::Local);
        }
        if self.finished {
            return Ok(Delivery::Pending);
        }
        // Finish the bounded header before reading any payload.
        while self.header.is_none() {
            let mut byte = [0];
            match self.stream.poll_read(cx, &mut byte) {
                Poll::Ready(Ok(0)) => return Err(StreamClosed::Peer(StreamCode::Invalid)),
                Poll::Ready(Ok(_)) => self.prefix.push(byte[0]),
                Poll::Ready(Err(e)) => return Err(read_error(e)),
                Poll::Pending => return Ok(Delivery::Pending),
            }
            if let Some((header, _)) =
                frame::StreamHeader::parse(&self.prefix).map_err(|_| StreamClosed::Peer(StreamCode::Invalid))?
            {
                self.prefix.clear();
                return Ok(Delivery::Header(header));
            }
        }
        let header = self.header.expect("header");
        if header.tag.kind.is_business() && self.queue.is_none() {
            self.check_reset(cx)?;
            return Ok(Delivery::Pending);
        }
        while self.length.is_none() {
            let mut byte = [0];
            match self.stream.poll_read(cx, &mut byte) {
                Poll::Ready(Ok(0)) => {
                    if !self.prefix.is_empty() {
                        return Err(StreamClosed::Peer(StreamCode::Invalid));
                    }
                    self.finished = true;
                    return Ok(Delivery::Finished);
                }
                Poll::Ready(Ok(_)) => self.prefix.push(byte[0]),
                Poll::Ready(Err(e)) => return Err(read_error(e)),
                Poll::Pending => return Ok(Delivery::Pending),
            }
            if let Some(frame) = frame::FrameHeader::parse(&self.prefix, header.tag.kind.max_message())
                .map_err(|_| StreamClosed::Peer(StreamCode::Invalid))?
            {
                admit(header, frame)?;
                self.length = Some(frame.total_length());
                self.body.extend_from_slice(&self.prefix);
                self.prefix.clear();
            }
        }
        let length = self.length.expect("length");
        if let Some(q) = &self.queue
            && !q.fits(length)
        {
            self.check_reset(cx)?;
            return Ok(Delivery::Pending);
        }
        if self.body.len() < length {
            let offset = self.body.len();
            self.body.resize(length.min(offset + 16 * 1024), 0);
            match self.stream.poll_read(cx, &mut self.body[offset..]) {
                Poll::Ready(Ok(0)) => return Err(StreamClosed::Peer(StreamCode::Invalid)),
                Poll::Ready(Ok(n)) => self.body.truncate(offset + n),
                Poll::Ready(Err(e)) => return Err(read_error(e)),
                Poll::Pending => {
                    self.body.truncate(offset);
                    return Ok(Delivery::Pending);
                }
            }
        }
        if self.body.len() != length {
            return Ok(Delivery::Progress);
        }
        self.length = None;
        let bytes = std::mem::take(&mut self.body);
        if let Some(q) = &self.queue {
            q.post_received(bytes, header.tag.kind.max_message())
                .map_err(|_| StreamClosed::Local)?;
            Ok(Delivery::Progress)
        } else {
            Ok(Delivery::Message(bytes))
        }
    }
}
