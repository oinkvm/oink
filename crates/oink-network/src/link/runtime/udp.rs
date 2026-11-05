//! Replace a defunct endpoint socket without replacing the QUIC endpoint.
use quinn::{AsyncUdpSocket, UdpPoller, udp};
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

#[derive(Debug)]
struct PollSlot {
    io: Option<Pin<Box<dyn UdpPoller>>>,
    wake: Option<Waker>,
}
#[derive(Debug)]
struct State {
    inner: Option<Arc<dyn AsyncUdpSocket>>,
    pollers: HashMap<usize, PollSlot>,
    next_poller: usize,
    recv_wake: Option<Waker>,
    error: Option<io::Error>,
    attempted: bool,
    failed: bool,
}
#[derive(Debug)]
pub(super) struct Socket {
    state: Mutex<State>,
    address: SocketAddr,
    wake: Arc<tokio::sync::Notify>,
    fragment: bool,
    transmit_segments: usize,
    receive_segments: usize,
}
impl Socket {
    pub(super) fn bind(address: SocketAddr, wake: Arc<tokio::sync::Notify>) -> io::Result<Arc<Self>> {
        #[cfg(any(test, feature = "test-support"))]
        let reserved = crate::test_ports::take_udp(address);
        #[cfg(not(any(test, feature = "test-support")))]
        let reserved: Option<std::net::UdpSocket> = None;
        let socket = match reserved {
            Some(socket) => socket,
            None => std::net::UdpSocket::bind(address)?,
        };
        Self::from_std(socket, wake)
    }
    pub(super) fn from_std(socket: std::net::UdpSocket, wake: Arc<tokio::sync::Notify>) -> io::Result<Arc<Self>> {
        crate::qos::set_service_type(&socket, crate::qos::ServiceType::Voice);
        Self::wrap(crate::qos::wrap_udp_socket(socket)?, wake)
    }
    fn wrap(inner: Arc<dyn AsyncUdpSocket>, wake: Arc<tokio::sync::Notify>) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            address: inner.local_addr()?,
            fragment: inner.may_fragment(),
            transmit_segments: inner.max_transmit_segments(),
            receive_segments: inner.max_receive_segments(),
            state: Mutex::new(State {
                inner: Some(inner),
                pollers: HashMap::new(),
                next_poller: 0,
                recv_wake: None,
                error: None,
                attempted: false,
                failed: false,
            }),
            wake,
        }))
    }
    fn observe<T>(&self, state: &mut State, result: io::Result<T>) -> io::Result<T> {
        // macOS can permanently mark UDP sockets defunct after NECP/network
        // extension policy changes: I/O then returns EPIPE for every destination.
        // Redialing on that fd cannot recover (incident on macOS 14.7.8); a fresh
        // socket did. Park Quinn until the owner rebinds, preserving its live
        // connections and migration machinery. Ordinary route errors and the
        // dedicated, short-lived manual-probe sockets retain their error paths.
        // The defunct transition itself has not been reproduced automatically.
        if result.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::BrokenPipe) {
            state.error = result.err();
            self.wake.notify_one();
            Err(io::ErrorKind::WouldBlock.into())
        } else {
            result
        }
    }
    /// One attempt per failed socket. All fd-owning pollers must be dropped too:
    /// Quinn retains its previous socket while migrating to the replacement.
    pub(super) fn rebind(self: &Arc<Self>, endpoint: &quinn::Endpoint) -> io::Result<Option<Arc<Self>>> {
        let mut state = self.state.lock().expect("endpoint socket");
        if state.error.is_none() || state.attempted {
            return Ok(None);
        }
        state.attempted = true;
        let error = copy_error(state.error.as_ref().expect("socket error"));
        state.inner = None;
        for slot in state.pollers.values_mut() {
            slot.io = None;
        }
        drop(state);
        let result = Self::bind(self.address, self.wake.clone()).and_then(|socket| {
            endpoint.rebind_abstract(socket.clone())?;
            Ok(socket)
        });
        tracing::warn!(
            %error, error_code = ?error.raw_os_error(),
            rebind_result = ?result.as_ref().map(|_| ()),
            "defunct UDP endpoint socket rebind"
        );
        if result.is_err() {
            let mut state = self.state.lock().expect("endpoint socket");
            state.failed = true;
            if let Some(wake) = state.recv_wake.take() {
                wake.wake();
            }
            for slot in state.pollers.values_mut() {
                if let Some(wake) = slot.wake.take() {
                    wake.wake();
                }
            }
        }
        result.map(Some)
    }
}
fn copy_error(error: &io::Error) -> io::Error {
    // The QoS destination decorator retains ErrorKind but wraps the OS error.
    let mut cause: &(dyn std::error::Error + 'static) = error;
    loop {
        if let Some(error) = cause.downcast_ref::<io::Error>() {
            if let Some(code) = error.raw_os_error() {
                return io::Error::from_raw_os_error(code);
            }
            if let Some(inner) = error.get_ref() {
                cause = inner;
                continue;
            }
        }
        match cause.source() {
            Some(source) => cause = source,
            None => return io::Error::new(error.kind(), error.to_string()),
        }
    }
}
#[derive(Debug)]
struct Poller {
    socket: Arc<Socket>,
    id: usize,
}
impl Drop for Poller {
    fn drop(&mut self) {
        self.socket
            .state
            .lock()
            .expect("endpoint socket")
            .pollers
            .remove(&self.id);
    }
}
impl UdpPoller for Poller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.socket.state.lock().expect("endpoint socket");
        if state.failed {
            return Poll::Ready(Err(copy_error(state.error.as_ref().expect("socket error"))));
        }
        let pending = state.error.is_some();
        let slot = state.pollers.get_mut(&self.id).expect("socket poller");
        slot.wake = Some(cx.waker().clone());
        if pending {
            return Poll::Pending;
        }
        let result = slot.io.as_mut().expect("live poller").as_mut().poll_writable(cx);
        match result {
            Poll::Ready(result) => match self.socket.observe(&mut state, result) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
                result => Poll::Ready(result),
            },
            Poll::Pending => Poll::Pending,
        }
    }
}
impl AsyncUdpSocket for Socket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        let mut state = self.state.lock().expect("endpoint socket");
        let id = state.next_poller;
        state.next_poller += 1;
        let io = state.inner.as_ref().map(|inner| inner.clone().create_io_poller());
        state.pollers.insert(id, PollSlot { io, wake: None });
        drop(state);
        Box::pin(Poller { socket: self, id })
    }
    fn try_send(&self, transmit: &udp::Transmit<'_>) -> io::Result<()> {
        let mut state = self.state.lock().expect("endpoint socket");
        if let Some(error) = &state.error {
            return Err(if state.failed {
                copy_error(error)
            } else {
                io::ErrorKind::WouldBlock.into()
            });
        }
        let result = state.inner.as_ref().expect("live socket").try_send(transmit);
        self.observe(&mut state, result)
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.state.lock().expect("endpoint socket");
        state.recv_wake = Some(cx.waker().clone());
        if let Some(error) = &state.error {
            return if state.failed {
                Poll::Ready(Err(copy_error(error)))
            } else {
                Poll::Pending
            };
        }
        match state.inner.as_ref().expect("live socket").poll_recv(cx, bufs, meta) {
            Poll::Ready(result) => match self.observe(&mut state, result) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
                result => Poll::Ready(result),
            },
            Poll::Pending => Poll::Pending,
        }
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.address)
    }
    fn may_fragment(&self) -> bool {
        self.fragment
    }
    fn max_transmit_segments(&self) -> usize {
        self.transmit_segments
    }
    fn max_receive_segments(&self) -> usize {
        self.receive_segments
    }
}

#[cfg(test)]
mod tests;
