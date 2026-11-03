use super::*;
use crate::{identity::Identity, tls::Credentials};
use std::{
    sync::atomic::{AtomicU8, Ordering},
    time::Duration,
};

#[derive(Debug)]
struct FailingSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    failure: Arc<AtomicU8>,
}
impl AsyncUdpSocket for FailingSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }
    fn try_send(&self, transmit: &udp::Transmit<'_>) -> io::Result<()> {
        match self.failure.load(Ordering::Relaxed) {
            1 => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                io::Error::from_raw_os_error(32),
            )),
            3 => Err(io::ErrorKind::NetworkUnreachable.into()),
            4 => Err(io::ErrorKind::HostUnreachable.into()),
            _ => self.inner.try_send(transmit),
        }
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        if self.failure.load(Ordering::Relaxed) == 2 {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                io::Error::from_raw_os_error(32),
            )))
        } else {
            self.inner.poll_recv(cx, bufs, meta)
        }
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

fn injected(wake: Arc<tokio::sync::Notify>) -> (Arc<Socket>, Arc<AtomicU8>) {
    let failure = Arc::new(AtomicU8::new(0));
    let inner = crate::qos::wrap_udp_socket(std::net::UdpSocket::bind("127.0.0.1:0").unwrap()).unwrap();
    let socket = Socket::wrap(
        Arc::new(FailingSocket {
            inner,
            failure: failure.clone(),
        }),
        wake,
    )
    .unwrap();
    (socket, failure)
}

async fn recovers(failure_kind: u8) {
    let credentials = Credentials::new(&Identity::generate()).unwrap();
    let (server_config, client_config) = super::super::transport(&credentials).unwrap();
    let wake = Arc::new(tokio::sync::Notify::new());
    let (socket, failure) = injected(wake.clone());
    let address = socket.local_addr().unwrap();
    let recovering = quinn::Endpoint::new_with_abstract_socket(
        Default::default(),
        Some(server_config.clone()),
        socket.clone(),
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap();
    let mut remote = quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
    remote.set_default_client_config(client_config);
    let (outbound, inbound) = tokio::try_join!(remote.connect(address, crate::tls::SERVER_NAME).unwrap(), async {
        recovering.accept().await.unwrap().await
    })
    .unwrap();
    let identity = inbound.stable_id();
    failure.store(failure_kind, Ordering::Relaxed);
    if failure_kind == 1 {
        inbound
            .send_datagram(bytes::Bytes::from_static(b"trigger send"))
            .unwrap();
    } else {
        outbound
            .send_datagram(bytes::Bytes::from_static(b"trigger recv"))
            .unwrap();
    }
    wake.notified().await;
    let replacement = socket
        .rebind(&recovering)
        .unwrap()
        .expect("EPIPE must rebind the endpoint");
    assert_eq!(replacement.local_addr().unwrap(), address);
    assert_eq!(recovering.local_addr().unwrap(), address);
    assert_eq!(inbound.stable_id(), identity);
    // Existing connections survive, including the transmit parked by EPIPE.
    inbound
        .send_datagram(bytes::Bytes::from_static(b"after rebind"))
        .unwrap();
    loop {
        if outbound.read_datagram().await.unwrap() == b"after rebind"[..] {
            break;
        }
    }
    outbound.send_datagram(bytes::Bytes::from_static(b"reply")).unwrap();
    assert_eq!(inbound.read_datagram().await.unwrap(), b"reply"[..]);
    // The same endpoint and listening port also accept a fresh handshake.
    let (fresh, accepted) = tokio::try_join!(remote.connect(address, crate::tls::SERVER_NAME).unwrap(), async {
        recovering.accept().await.unwrap().await
    })
    .unwrap();
    fresh.send_datagram(bytes::Bytes::from_static(b"fresh")).unwrap();
    assert_eq!(accepted.read_datagram().await.unwrap(), b"fresh"[..]);
    assert!(socket.rebind(&recovering).unwrap().is_none());
    recovering.close(0u32.into(), b"done");
    remote.close(0u32.into(), b"done");
}

#[tokio::test]
async fn defunct_send_rebinds_same_port_and_preserves_connections() {
    tokio::time::timeout(Duration::from_secs(3), recovers(1))
        .await
        .expect("send EPIPE recovery");
}
#[tokio::test]
async fn defunct_receive_rebinds_same_port_and_preserves_connections() {
    tokio::time::timeout(Duration::from_secs(3), recovers(2))
        .await
        .expect("receive EPIPE recovery");
}

#[tokio::test]
async fn ordinary_network_error_does_not_request_rebind() {
    let (socket, failure) = injected(Arc::new(tokio::sync::Notify::new()));
    for (code, kind) in [
        (3, io::ErrorKind::NetworkUnreachable),
        (4, io::ErrorKind::HostUnreachable),
    ] {
        failure.store(code, Ordering::Relaxed);
        let result = socket.try_send(&udp::Transmit {
            destination: "127.0.0.1:47470".parse().unwrap(),
            ecn: None,
            contents: b"test",
            segment_size: None,
            src_ip: None,
        });
        assert_eq!(result.unwrap_err().kind(), kind);
        assert!(socket.state.lock().unwrap().error.is_none());
    }
}

#[tokio::test]
async fn failed_rebind_surfaces_error_without_retrying() {
    let bound = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    // An extra owner keeps the port occupied after the adapter releases its fd.
    let occupied = bound.try_clone().unwrap();
    let failure = Arc::new(AtomicU8::new(1));
    let socket = Socket::wrap(
        Arc::new(FailingSocket {
            inner: crate::qos::wrap_udp_socket(bound).unwrap(),
            failure,
        }),
        Arc::new(tokio::sync::Notify::new()),
    )
    .unwrap();
    let endpoint = quinn::Endpoint::new_with_abstract_socket(
        Default::default(),
        None,
        socket.clone(),
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap();
    let transmit = udp::Transmit {
        destination: "127.0.0.1:47470".parse().unwrap(),
        ecn: None,
        contents: b"test",
        segment_size: None,
        src_ip: None,
    };
    assert_eq!(
        socket.try_send(&transmit).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    let mut poller = socket.clone().create_io_poller();
    assert_eq!(socket.rebind(&endpoint).unwrap_err().kind(), io::ErrorKind::AddrInUse);
    assert_eq!(socket.try_send(&transmit).unwrap_err().raw_os_error(), Some(32));
    assert_eq!(
        std::future::poll_fn(|cx| poller.as_mut().poll_writable(cx))
            .await
            .unwrap_err()
            .raw_os_error(),
        Some(32)
    );
    let mut data = [0; 32];
    let mut bufs = [io::IoSliceMut::new(&mut data)];
    let mut meta = [udp::RecvMeta::default()];
    assert_eq!(
        std::future::poll_fn(|cx| socket.poll_recv(cx, &mut bufs, &mut meta))
            .await
            .unwrap_err()
            .raw_os_error(),
        Some(32)
    );
    drop(occupied);
    assert!(
        socket.rebind(&endpoint).unwrap().is_none(),
        "no retry after port becomes free"
    );
    endpoint.close(0u32.into(), b"done");
}

#[test]
fn decorated_error_retains_os_code() {
    let error = io::Error::new(io::ErrorKind::BrokenPipe, io::Error::from_raw_os_error(32));
    assert_eq!(copy_error(&error).raw_os_error(), Some(32));
}
