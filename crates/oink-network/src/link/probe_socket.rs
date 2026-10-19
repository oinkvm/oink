//! A manual lookup owns a connected UDP socket, so ICMP errors belong to exactly
//! that lookup. Shared sockets cannot attribute them safely to one remote peer.
use super::AddressProbeError;
use quinn::{AsyncUdpSocket, UdpPoller, udp};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

#[derive(Debug)]
pub(super) struct Socket {
    inner: Arc<dyn AsyncUdpSocket>,
    pub error: Mutex<Option<AddressProbeError>>,
    wake: Arc<tokio::sync::Notify>,
}
impl Socket {
    pub fn new(address: SocketAddr, wake: Arc<tokio::sync::Notify>) -> io::Result<Arc<Self>> {
        let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))?;
        socket.connect(address)?;
        socket.set_nonblocking(true)?;
        crate::qos::set_service_type(&socket, crate::qos::ServiceType::Voice);
        Ok(Arc::new(Self {
            inner: crate::qos::wrap_probe_socket(socket)?,
            error: Mutex::new(None),
            wake,
        }))
    }
    fn observe<T>(&self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result
            && !matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
        {
            *self.error.lock().expect("probe socket error") = Some(AddressProbeError::from_io(error));
            self.wake.notify_one();
        }
        result
    }
}
impl AsyncUdpSocket for Socket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }
    fn try_send(&self, transmit: &udp::Transmit<'_>) -> io::Result<()> {
        self.observe(self.inner.try_send(transmit))
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_recv(cx, bufs, meta).map(|result| self.observe(result))
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }
    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }
}
