//! One bounded, explicitly ended lease at a time on a reusable TLS socket.
use super::{LowBinding, LowMsg, lock};
use crate::link::control::LinkMsg;
use crate::record::{RecordRead, RecordWrite};
use oink_foundation::DeviceId;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use std::{
    io,
    net::Shutdown,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufStream, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::watch;

pub(crate) const BUFFER: usize = 256 * 1024;
const OPEN: u8 = 0;
const ABORTED: u8 = 1;
const COMMITTED: u8 = 2;
const CHUNK: usize = 16 * 1024;
const FRAME_LIMIT: usize = CHUNK + 1024;
pub(crate) type Stream = tokio_rustls::TlsStream<BufStream<TcpStream>>;
type Recycle = Box<dyn FnOnce(Stream, std::net::TcpStream) + Send>;
#[derive(Default)]
struct Completed {
    tx: Option<WriteHalf<Stream>>,
    rx: Option<ReadHalf<Stream>>,
    recycle: Option<Recycle>,
}

/// A fresh lifetime for every lease (and idle period). Old application handles
/// can never shut down a socket after ownership moves to its next lease.
pub(crate) struct Life {
    socket: Mutex<Option<std::net::TcpStream>>,
    pub done: watch::Sender<bool>,
    state: AtomicU8,
    recycling: AtomicBool,
    hold: AtomicBool,
    completed: Mutex<Completed>,
    runtime: tokio::runtime::Handle,
    timeout: Duration,
    binding: LowBinding,
    network: Arc<super::Port>,
    grant: super::Grant,
    forward: Option<super::Grant>,
    permit: Mutex<Option<tokio::sync::OwnedSemaphorePermit>>,
    pub peer: DeviceId,
    pub generation: u64,
    request: [u8; 16],
    cancel: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    tasks: Arc<watch::Sender<usize>>,
}
impl std::fmt::Debug for Life {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpLife")
            .field("peer", &self.peer)
            .field("state", &self.state)
            .finish()
    }
}
impl Life {
    pub(super) fn new(
        socket: std::net::TcpStream,
        runtime: tokio::runtime::Handle,
        timeout: Duration,
        request: &crate::link::control::LowRequest,
        authorization: &super::Authorization,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
        generation: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            socket: Mutex::new(Some(socket)),
            done: watch::channel(false).0,
            state: AtomicU8::new(OPEN),
            recycling: AtomicBool::new(false),
            hold: AtomicBool::new(false),
            completed: Mutex::new(Completed::default()),
            runtime,
            timeout,
            binding: request.binding,
            peer: authorization.grant.neighbor.device_id,
            network: authorization.network.clone(),
            grant: authorization.grant,
            forward: authorization.forward,
            request: request.request,
            cancel: Mutex::new(None),
            tasks: Arc::new(watch::channel(0).0),
            generation,
            permit: Mutex::new(permit),
        })
    }
    fn spawn(&self, work: impl std::future::Future<Output = ()> + Send + 'static) {
        let admitted = self.tasks.send_if_modified(|tasks| {
            if self.state.load(Ordering::Acquire) != OPEN {
                return false;
            }
            *tasks += 1;
            true
        });
        if !admitted {
            return;
        }
        let guard = super::Work(self.tasks.clone());
        self.runtime.spawn(async move {
            let _guard = guard;
            work.await;
        });
    }
    pub async fn drained(&self) {
        let mut tasks = self.tasks.subscribe();
        while *tasks.borrow_and_update() != 0 {
            if tasks.changed().await.is_err() {
                break;
            }
        }
    }
    pub fn cancel_via(&self, cancel: impl FnOnce() + Send + 'static) {
        *lock(&self.cancel) = Some(Box::new(cancel));
    }
    pub fn recycle(&self, callback: Recycle) {
        lock(&self.completed).recycle = Some(callback);
    }
    fn authorized(&self) -> bool {
        self.network.low_current(self.grant) && self.forward.is_none_or(|grant| self.network.low_current(grant))
    }
    pub fn effect_current(&self) -> bool {
        self.authorized() && self.state.load(Ordering::Acquire) != ABORTED
    }
    pub fn active(&self) -> bool {
        self.authorized()
            && match self.state.load(Ordering::Acquire) {
                OPEN => true,
                COMMITTED => self.recycling.load(Ordering::Acquire),
                _ => false,
            }
    }
    pub fn gracefully_finished(&self) -> bool {
        self.state.load(Ordering::Acquire) == COMMITTED
    }
    pub fn close(&self) {
        if self
            .state
            .compare_exchange(OPEN, ABORTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if let Some(ctrl) = lock(&self.cancel).take() {
                ctrl();
            }
            if let Some(socket) = lock(&self.socket).take() {
                let _ = socket.shutdown(Shutdown::Both);
            }
            *lock(&self.completed) = Completed::default();
            lock(&self.permit).take();
            self.done.send_replace(true);
        }
    }
    pub fn close_without_control(&self) {
        lock(&self.cancel).take();
        self.close();
    }
    /// Move an idle socket into a new lifetime; no shutdown on old-handle drop.
    pub fn take_socket(&self) -> Option<std::net::TcpStream> {
        if self
            .state
            .compare_exchange(OPEN, ABORTED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        let socket = lock(&self.socket).take();
        self.done.send_replace(true);
        socket
    }
    pub fn hold_reuse(&self) {
        self.hold.store(true, Ordering::Release);
    }
    pub fn release_reuse(self: &Arc<Self>) {
        self.hold.store(false, Ordering::Release);
        self.try_commit();
    }
    fn read_finished(self: &Arc<Self>, rx: ReadHalf<Stream>) {
        lock(&self.completed).rx = Some(rx);
        self.try_commit();
    }
    fn write_finished(self: &Arc<Self>, tx: WriteHalf<Stream>) {
        lock(&self.completed).tx = Some(tx);
        self.try_commit();
    }
    fn try_commit(self: &Arc<Self>) {
        if !self.active() || self.hold.load(Ordering::Acquire) {
            return;
        }
        let (rx, tx, recycle) = {
            let mut halves = lock(&self.completed);
            if halves.rx.is_none() || halves.tx.is_none() {
                return;
            }
            (
                halves.rx.take().unwrap(),
                halves.tx.take().unwrap(),
                halves.recycle.take(),
            )
        };
        let life = self.clone();
        let timeout = self.timeout;
        self.spawn(async move {
            let mut stream = rx.unsplit(tx);
            let mut done = life.done.subscribe();
            let work = async {
                write_message(
                    &mut Guarded::new(&mut stream, &life),
                    life.binding,
                    &LowMsg::EndAck { request: life.request },
                )
                .await?;
                let message = read_message(&mut Guarded::new(&mut stream, &life), life.binding).await?;
                if !matches!(message, LowMsg::EndAck { request } if request == life.request) {
                    return Err(invalid("missing matching END_ACK"));
                }
                Ok::<_, io::Error>(())
            };
            let success = tokio::select! { biased;
                _ = wait_done(&mut done) => false,
                result = tokio::time::timeout(timeout, work) => matches!(result, Ok(Ok(()))),
            };
            if !success {
                life.close();
                return;
            }
            // Keep the physical slot visible while moving it into the idle
            // pool. Otherwise another opener can claim a second keep_alive,
            // or reject a reuse arriving just before the idle entry is stored.
            life.recycling.store(true, Ordering::Release);
            // Commit atomically: deferred effects must never observe an aborted
            // interval between successful END_ACK and parking the socket.
            if life
                .state
                .compare_exchange(OPEN, COMMITTED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
                && let Some(socket) = lock(&life.socket).take()
            {
                if let Some(recycle) = recycle {
                    recycle(stream, socket);
                }
                life.recycling.store(false, Ordering::Release);
                // Park before admitting a waiter, so it can borrow this socket.
                lock(&life.permit).take();
                life.done.send_replace(true);
            }
            life.recycling.store(false, Ordering::Release);
        });
    }
}
impl Drop for Life {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) struct Tx {
    io: Option<WriteHalf<Stream>>,
    life: Arc<Life>,
    scratch: Vec<u8>,
    pending: Vec<u8>,
    written: usize,
    consumed: usize,
}
pub(crate) struct Rx {
    io: Option<ReadHalf<Stream>>,
    life: Arc<Life>,
    eof: bool,
    prefix: [u8; 6],
    prefix_read: usize,
    frame: Vec<u8>,
    frame_read: usize,
    chunk: Vec<u8>,
    chunk_read: usize,
}
pub(crate) fn split(stream: Stream, life: Arc<Life>) -> (Tx, Rx) {
    let (rx, tx) = tokio::io::split(stream);
    (
        Tx {
            io: Some(tx),
            life: life.clone(),
            scratch: Vec::with_capacity(65568),
            pending: Vec::new(),
            written: 0,
            consumed: 0,
        },
        Rx {
            io: Some(rx),
            life,
            eof: false,
            prefix: [0; 6],
            prefix_read: 0,
            frame: Vec::new(),
            frame_read: 0,
            chunk: Vec::new(),
            chunk_read: 0,
        },
    )
}
async fn wait_done(done: &mut watch::Receiver<bool>) {
    if !*done.borrow() {
        let _ = done.changed().await;
    }
}
fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "TCP Low lease closed")
}
fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}
fn encoded(binding: LowBinding, message: &LowMsg) -> Vec<u8> {
    crate::link::control_wire::encode(&LinkMsg::Low {
        binding,
        message: Box::new(message.clone()),
    })
    .expect("bounded Low control DTO")
}

async fn write_message<W: tokio::io::AsyncWrite + Unpin>(
    io: &mut W,
    binding: LowBinding,
    message: &LowMsg,
) -> io::Result<()> {
    io.write_all(&encoded(binding, message)).await?;
    io.flush().await
}
fn decode_message(bytes: &[u8], binding: LowBinding) -> io::Result<LowMsg> {
    match crate::link::control_wire::decode(bytes).map_err(io::Error::other)? {
        LinkMsg::Low { binding: got, message } if got == binding => Ok(*message),
        _ => Err(invalid("TCP Low binding mismatch")),
    }
}
async fn read_message<R: tokio::io::AsyncRead + Unpin>(io: &mut R, binding: LowBinding) -> io::Result<LowMsg> {
    decode_message(&super::sockets::read_control(io, FRAME_LIMIT).await?, binding)
}
// The network gate closes before the ordinary invalidation event. Every poll
// rechecks it, including a read/write resumed after Pending and END/ACK work.
enum Validity<'a> {
    Lease(&'a Life),
    Authorization(&'a super::Authorization),
}
impl Validity<'_> {
    fn current(&self) -> bool {
        match self {
            Self::Lease(life) => life.active(),
            Self::Authorization(auth) => auth.current(),
        }
    }
}
pub(super) struct Guarded<'a, T> {
    io: &'a mut T,
    validity: Validity<'a>,
}
impl<'a, T> Guarded<'a, T> {
    pub(super) fn new(io: &'a mut T, life: &'a Life) -> Self {
        Self {
            io,
            validity: Validity::Lease(life),
        }
    }
    pub(super) fn authorized(io: &'a mut T, auth: &'a super::Authorization) -> Self {
        Self {
            io,
            validity: Validity::Authorization(auth),
        }
    }
}
impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Guarded<'_, T> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.validity.current() {
            return Poll::Ready(Err(closed()));
        }
        let result = Pin::new(&mut *this.io).poll_read(cx, buf);
        if !this.validity.current() {
            return Poll::Ready(Err(closed()));
        }
        result
    }
}
impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Guarded<'_, T> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if !this.validity.current() {
            return Poll::Ready(Err(closed()));
        }
        Pin::new(&mut *this.io).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.validity.current() {
            return Poll::Ready(Err(closed()));
        }
        Pin::new(&mut *this.io).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.validity.current() {
            return Poll::Ready(Err(closed()));
        }
        Pin::new(&mut *this.io).poll_shutdown(cx)
    }
}

impl Tx {
    pub fn finish(&mut self) {
        if self.io.is_none() {
            return;
        }
        if !self.life.active() {
            self.abort();
            return;
        }
        if !self.pending.is_empty() {
            self.abort();
            return;
        }
        let mut io = self.io.take().unwrap();
        let life = self.life.clone();
        let timeout = life.timeout;
        self.life.spawn(async move {
            let mut done = life.done.subscribe();
            let end = LowMsg::End { request: life.request };
            let success = tokio::select! { biased;
                _ = wait_done(&mut done) => false,
                result = tokio::time::timeout(timeout,
                    async { write_message(&mut Guarded::new(&mut io, &life), life.binding, &end).await }) => matches!(result, Ok(Ok(()))),
            };
            if success {
                life.write_finished(io);
            } else {
                life.close();
            }
        });
    }
    pub fn abort(&mut self) {
        self.life.close();
        self.io.take();
    }
}
impl Drop for Tx {
    fn drop(&mut self) {
        if self.io.is_some() {
            self.abort();
        }
    }
}
impl Drop for Rx {
    fn drop(&mut self) {
        // The new operation contract makes dropping an unread side an abort.
        // Only an explicitly consumed END can return this side to the pool.
        if self.io.is_some() {
            self.life.close();
        }
    }
}

impl Rx {
    pub fn stop(&mut self) {
        self.life.close();
        self.io.take();
    }
}
impl RecordWrite for Tx {
    async fn record(&mut self, header: &[u8], body: &[u8]) -> io::Result<()> {
        let mut bytes = std::mem::take(&mut self.scratch);
        bytes.clear();
        bytes.extend_from_slice(header);
        bytes.extend_from_slice(body);
        let mut offset = 0;
        while offset < bytes.len() {
            let n = std::future::poll_fn(|cx| self.poll_write_bytes(cx, &bytes[offset..])).await?;
            offset += n;
        }
        self.scratch = bytes;
        Ok(())
    }
    async fn flush_message(&mut self) -> io::Result<()> {
        std::future::poll_fn(|cx| self.poll_flush_bytes(cx)).await
    }
    fn poll_write_bytes(&mut self, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        if !self.life.active() {
            return Poll::Ready(Err(closed()));
        }
        let Some(io) = self.io.as_mut() else {
            return Poll::Ready(Err(closed()));
        };
        if self.pending.is_empty() {
            self.consumed = data.len().min(CHUNK);
            self.pending = encoded(
                self.life.binding,
                &LowMsg::Record {
                    bytes: data[..self.consumed].to_vec(),
                },
            );
            self.written = 0;
        }
        while self.written < self.pending.len() {
            let mut guarded = Guarded::new(&mut *io, &self.life);
            match tokio::io::AsyncWrite::poll_write(Pin::new(&mut guarded), cx, &self.pending[self.written..]) {
                Poll::Ready(Ok(0)) => return Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                Poll::Ready(Ok(n)) => self.written += n,
                other => return other,
            }
        }
        self.pending.clear();
        Poll::Ready(Ok(self.consumed))
    }
    fn poll_flush_bytes(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.life.active() {
            return Poll::Ready(Err(closed()));
        }
        let Some(io) = self.io.as_mut() else {
            return Poll::Ready(Err(closed()));
        };
        tokio::io::AsyncWrite::poll_flush(Pin::new(&mut Guarded::new(io, &self.life)), cx)
    }
    async fn finish_records(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            self.abort();
            return Err(invalid("unfinished TCP record"));
        }
        let Some(mut io) = self.io.take() else {
            return Ok(());
        };
        let mut done = self.life.done.subscribe();
        let timeout = self.life.timeout;
        let end = LowMsg::End {
            request: self.life.request,
        };
        let success = tokio::select! { biased;
            _ = wait_done(&mut done) => false,
            result = tokio::time::timeout(timeout, async { write_message(&mut Guarded::new(&mut io, &self.life), self.life.binding, &end).await }) => matches!(result, Ok(Ok(()))),
        };
        if success {
            self.life.write_finished(io);
            Ok(())
        } else {
            self.life.close();
            Err(closed())
        }
    }
}
impl RecordRead for Rx {
    async fn read_record_bytes(&mut self, data: &mut [u8]) -> io::Result<usize> {
        if self.eof {
            return Ok(0);
        }
        if !self.life.active() {
            return Err(closed());
        }
        if self.chunk_read < self.chunk.len() {
            let n = data.len().min(self.chunk.len() - self.chunk_read);
            data[..n].copy_from_slice(&self.chunk[self.chunk_read..self.chunk_read + n]);
            self.chunk_read += n;
            return Ok(n);
        }
        let mut done = self.life.done.subscribe();
        let work = async {
            let mut io = Guarded::new(self.io.as_mut().ok_or_else(closed)?, &self.life);
            while self.prefix_read < 6 {
                let n = io.read(&mut self.prefix[self.prefix_read..]).await?;
                if n == 0 {
                    return Err(closed());
                }
                self.prefix_read += n;
            }
            if self.frame.is_empty() {
                let header = super::sockets::control_header(&self.prefix, FRAME_LIMIT)?;
                self.frame.resize(header.total_length(), 0);
                self.frame[..6].copy_from_slice(&self.prefix);
                self.frame_read = 6;
            }
            while self.frame_read < self.frame.len() {
                let n = io.read(&mut self.frame[self.frame_read..]).await?;
                if n == 0 {
                    return Err(closed());
                }
                self.frame_read += n;
            }
            if !self.life.active() {
                return Err(closed());
            }
            let message = decode_message(&self.frame, self.life.binding)?;
            self.frame.clear();
            self.frame_read = 0;
            self.prefix_read = 0;
            match message {
                LowMsg::Record { bytes } if !bytes.is_empty() && bytes.len() <= CHUNK => {
                    self.chunk = bytes;
                    self.chunk_read = data.len().min(self.chunk.len());
                    data[..self.chunk_read].copy_from_slice(&self.chunk[..self.chunk_read]);
                    Ok(self.chunk_read)
                }
                LowMsg::End { request } if request == self.life.request => {
                    self.eof = true;
                    Ok(0)
                }
                _ => Err(invalid("unexpected TCP lease frame")),
            }
        };
        let result = tokio::select! { biased; _ = wait_done(&mut done) => Err(closed()), result = work => result };
        if result.is_err() {
            self.life.close();
        }
        result
    }
    fn finish_read(&mut self) {
        if self.eof
            && let Some(io) = self.io.take()
        {
            self.life.read_finished(io);
        }
    }
}
