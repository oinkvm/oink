use super::*;
use crate::{identity::Identity, link::DIAL_BUDGET, tls::Credentials};
use quinn::{AsyncUdpSocket, UdpPoller, udp};
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

#[test]
fn only_host_unreachable_is_a_dropped_datagram() {
    let transmit = Transmit {
        destination: "127.0.0.1:47470".parse().unwrap(),
        ecn: None,
        contents: b"packet",
        segment_size: None,
        src_ip: None,
    };
    for kind in [
        io::ErrorKind::HostUnreachable,
        io::ErrorKind::NetworkUnreachable,
        io::ErrorKind::WouldBlock,
        io::ErrorKind::InvalidInput,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::AddrNotAvailable,
    ] {
        let result = send_result(Err(io::Error::new(kind, "injected UDP send error")), &transmit);
        if kind == io::ErrorKind::HostUnreachable {
            assert!(
                result.is_ok(),
                "a single host-unreachable send must remain QUIC packet loss"
            );
        } else {
            assert_eq!(result.unwrap_err().kind(), kind);
        }
    }
    assert!(send_result(Ok(()), &transmit).is_ok());
}

#[derive(Debug)]
struct FailingSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    target: SocketAddr,
    failures: usize,
    sends: AtomicUsize,
}
impl AsyncUdpSocket for FailingSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }
    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        let result =
            if transmit.destination == self.target && self.sends.fetch_add(1, Ordering::Relaxed) < self.failures {
                Err(io::Error::new(
                    io::ErrorKind::HostUnreachable,
                    "injected startup send failure",
                ))
            } else {
                self.inner.try_send(transmit)
            };
        send_result(result, transmit)
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_recv(cx, bufs, meta)
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

async fn endpoints(failures: usize) -> (quinn::Endpoint, quinn::Endpoint, Arc<FailingSocket>) {
    let server_identity = Identity::generate();
    let server_credentials = Credentials::new(&server_identity).unwrap();
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_credentials.server_config().unwrap()).unwrap(),
    ));
    let server = quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
    let client_credentials = Credentials::new(&Identity::generate()).unwrap();
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let socket = Arc::new(FailingSocket {
        inner: quinn::Runtime::wrap_udp_socket(&quinn::TokioRuntime, socket).unwrap(),
        target: server.local_addr().unwrap(),
        failures,
        sends: AtomicUsize::new(0),
    });
    let mut client = quinn::Endpoint::new_with_abstract_socket(
        Default::default(),
        None,
        socket.clone(),
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap();
    client.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(
            client_credentials.client_config(Some(server_identity.id())).unwrap(),
        )
        .unwrap(),
    )));
    (client, server, socket)
}

#[tokio::test]
async fn startup_host_unreachable_retransmits_while_stale_candidate_is_pending() {
    // Real QUIC/TLS, with only the first kernel send result injected. This
    // reproduces the adapter failure, not macOS's network-privacy/ARP state.
    let (client, server, socket) = endpoints(1).await;
    let stale = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let direct = client
        .connect(server.local_addr().unwrap(), crate::tls::SERVER_NAME)
        .unwrap();
    let stale_attempt = client
        .connect(stale.local_addr().unwrap(), crate::tls::SERVER_NAME)
        .unwrap();
    let accept = async { server.accept().await.unwrap().await };
    let (direct, inbound) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::try_join!(direct, accept).expect("send failure must not close the correct candidate")
    })
    .await
    .expect("correct candidate must recover before the stale candidate's 3 s deadline");
    assert!(socket.sends.load(Ordering::Relaxed) >= 2);
    drop(stale_attempt);
    direct.close(0u32.into(), b"test complete");
    inbound.close(0u32.into(), b"test complete");
    client.close(0u32.into(), b"test complete");
    server.close(0u32.into(), b"test complete");
}

#[tokio::test]
async fn persistent_host_unreachable_remains_bounded_by_dial_deadline() {
    let (client, server, socket) = endpoints(usize::MAX).await;
    let attempt = client
        .connect(server.local_addr().unwrap(), crate::tls::SERVER_NAME)
        .unwrap();
    assert!(tokio::time::timeout(DIAL_BUDGET, attempt).await.is_err());
    assert!(socket.sends.load(Ordering::Relaxed) >= 2);
    client.close(0u32.into(), b"test complete");
    server.close(0u32.into(), b"test complete");
}
