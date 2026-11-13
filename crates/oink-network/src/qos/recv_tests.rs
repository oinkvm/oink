//! Receive errors on the shared endpoint socket versus a manual probe's socket.
use super::*;
use crate::{identity::Identity, tls::Credentials};
use quinn::{AsyncUdpSocket, UdpPoller, udp};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
    time::{Duration, Instant},
};

fn host_unreachable() -> io::Error {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let error = io::Error::from_raw_os_error(libc::EHOSTUNREACH);
    // WSAEHOSTUNREACH
    #[cfg(windows)]
    let error = io::Error::from_raw_os_error(10065);
    #[cfg(not(any(target_os = "macos", target_os = "ios", windows)))]
    let error = io::Error::from(io::ErrorKind::HostUnreachable);
    error
}

/// A real socket whose first receive reports EHOSTUNREACH, as the shared
/// endpoint socket's did on macOS 27.2.1 (2026-11-12). Only the error is
/// injected; datagrams after it come from the real socket.
#[derive(Debug)]
struct UnreachableOnce {
    inner: Arc<dyn AsyncUdpSocket>,
    armed: AtomicBool,
    fired: tokio::sync::Notify,
}
impl UnreachableOnce {
    fn new(inner: Arc<dyn AsyncUdpSocket>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            armed: AtomicBool::new(true),
            fired: tokio::sync::Notify::new(),
        })
    }
}
impl AsyncUdpSocket for UnreachableOnce {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }
    fn try_send(&self, transmit: &udp::Transmit<'_>) -> io::Result<()> {
        self.inner.try_send(transmit)
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.fired.notify_one();
            return Poll::Ready(Err(host_unreachable()));
        }
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

fn loopback() -> std::net::UdpSocket {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    socket
}

#[tokio::test]
async fn unreachable_receive_keeps_the_shared_endpoint_driver_running() {
    let server_identity = Identity::generate();
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(
            Credentials::new(&server_identity).unwrap().server_config().unwrap(),
        )
        .unwrap(),
    ));
    let server = quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
    // The production endpoint stack: platform adapter inside DestinationSocket.
    let injected = UnreachableOnce::new(wrap_udp_socket_inner(loopback()).unwrap());
    let mut client = quinn::Endpoint::new_with_abstract_socket(
        Default::default(),
        None,
        DestinationSocket::endpoint(injected.clone(), Some(SendErrors::default())),
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap();
    client.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(
            Credentials::new(&Identity::generate())
                .unwrap()
                .client_config(Some(server_identity.id()))
                .unwrap(),
        )
        .unwrap(),
    )));
    // The endpoint driver's first receive gets the error. On this
    // single-threaded runtime that driver poll has finished, or its task has
    // ended, before this test runs again.
    tokio::time::timeout(Duration::from_secs(2), injected.fired.notified())
        .await
        .expect("the endpoint driver polls its socket");
    let connecting = client
        .connect(server.local_addr().unwrap(), crate::tls::SERVER_NAME)
        .expect("a receive-side EHOSTUNREACH must not stop the shared endpoint");
    let (outbound, inbound) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::try_join!(connecting, async { server.accept().await.unwrap().await })
    })
    .await
    .expect("the handshake completes on datagrams received after the error")
    .unwrap();
    assert!(
        !matches!(
            client.connect(server.local_addr().unwrap(), crate::tls::SERVER_NAME),
            Err(quinn::ConnectError::EndpointStopping)
        ),
        "the endpoint still accepts dials"
    );
    outbound.close(0u32.into(), b"test complete");
    inbound.close(0u32.into(), b"test complete");
    client.close(0u32.into(), b"test complete");
    server.close(0u32.into(), b"test complete");
}

#[tokio::test]
async fn probe_socket_still_reports_an_unreachable_receive() {
    let injected = UnreachableOnce::new(wrap_probe_socket(loopback()).unwrap());
    let probe = observe_udp_socket(injected, SendErrors::default());
    let mut bytes = [0; 64];
    let mut bufs = [io::IoSliceMut::new(&mut bytes)];
    let mut meta = [udp::RecvMeta::default()];
    let error = std::future::poll_fn(|cx| probe.poll_recv(cx, &mut bufs, &mut meta))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::HostUnreachable);
}

#[test]
fn only_destination_unreachability_is_skipped() {
    assert_eq!(unreachable_recv(&host_unreachable()), Some("host_unreachable"));
    assert_eq!(
        unreachable_recv(&io::ErrorKind::NetworkUnreachable.into()),
        Some("network_unreachable")
    );
    if let Some(code) = HOST_DOWN {
        assert_eq!(unreachable_recv(&io::Error::from_raw_os_error(code)), Some("host_down"));
    }
    for kind in [
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::Other,
    ] {
        assert_eq!(unreachable_recv(&kind.into()), None, "{kind:?}");
    }
}

#[test]
fn one_log_line_per_receive_error_burst() {
    let now = Instant::now();
    let mut burst = RecvErrorBurst::default();
    assert_eq!(burst.observe(Some(65), "host_unreachable", now), Some(0));
    assert_eq!(
        burst.observe(Some(65), "host_unreachable", now + Duration::from_secs(9)),
        None
    );
    assert_eq!(
        burst.observe(Some(65), "host_unreachable", now + Duration::from_secs(18)),
        None
    );
    // Another errno starts its own burst and reports the previous one's repeats.
    assert_eq!(
        burst.observe(Some(51), "network_unreachable", now + Duration::from_secs(19)),
        Some(2)
    );
    assert_eq!(
        burst.observe(Some(65), "host_unreachable", now + Duration::from_secs(20)),
        Some(0)
    );
    // A quiet gap ends a burst.
    assert_eq!(
        burst.observe(Some(65), "host_unreachable", now + Duration::from_secs(30)),
        Some(0)
    );
}

#[test]
fn a_run_of_receive_errors_yields_with_a_wake() {
    #[derive(Debug)]
    struct AlwaysUnreachable(Arc<dyn AsyncUdpSocket>);
    impl AsyncUdpSocket for AlwaysUnreachable {
        fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
            self.0.clone().create_io_poller()
        }
        fn try_send(&self, transmit: &udp::Transmit<'_>) -> io::Result<()> {
            self.0.try_send(transmit)
        }
        fn poll_recv(
            &self,
            _: &mut Context<'_>,
            _: &mut [io::IoSliceMut<'_>],
            _: &mut [udp::RecvMeta],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(host_unreachable()))
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            self.0.local_addr()
        }
    }
    struct CountWakes(std::sync::atomic::AtomicUsize);
    impl std::task::Wake for CountWakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let inner = quinn::Runtime::wrap_udp_socket(&quinn::TokioRuntime, loopback()).unwrap();
    let socket = DestinationSocket::endpoint(Arc::new(AlwaysUnreachable(inner)), None);
    let wakes = Arc::new(CountWakes(Default::default()));
    let waker = std::task::Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    let mut bytes = [0; 64];
    let mut bufs = [io::IoSliceMut::new(&mut bytes)];
    let mut meta = [udp::RecvMeta::default()];
    assert!(socket.poll_recv(&mut cx, &mut bufs, &mut meta).is_pending());
    assert_eq!(
        wakes.0.load(Ordering::SeqCst),
        1,
        "Pending without a waker would stall the driver"
    );
}
