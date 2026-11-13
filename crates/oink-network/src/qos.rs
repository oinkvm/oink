//! Interactive QUIC uses a bounded send queue, fixed congestion window and
//! platform socket priority. Low TCP uses the platform default service class.

use std::sync::Arc;

mod fixed_window;

/// Sender-local policy; peers need not negotiate or upgrade together. This
/// only configures the interactive QUIC endpoint, without Low payloads.
pub(crate) fn interactive_congestion(t: &mut quinn::TransportConfig) {
    t.congestion_controller_factory(Arc::new(fixed_window::FixedWindowConfig));
}

/// datagram 发送缓冲（字节，含 quinn 每条约 32 字节的记账开销）。仅供游戏位移/媒体等
/// Instant 消息使用；桌面 Motion 的排队由 Ordered 容量和输入年龄约束。
pub(crate) const DATAGRAM_SEND_BUFFER: usize = 8 * 1024;

/// 连接的发送窗口（已发出未确认 + 缓冲待发的数据，字节）。quinn 默认 10 MiB。
pub(crate) const SEND_WINDOW: u64 = 2 * 1024 * 1024;

/// The service classes used by product sockets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceType {
    Voice,
    #[cfg(any(target_os = "ios", test))]
    BestEffort,
}
impl ServiceType {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn net_service_type(self) -> i32 {
        match self {
            // NET_SERVICE_TYPE_VO
            Self::Voice => 4,
            // NET_SERVICE_TYPE_BE, the default of a fresh socket
            #[cfg(any(target_os = "ios", test))]
            Self::BestEffort => 0,
        }
    }
}

/// iOS QUIC socket's service type: voice while this phone is the speaker exit
/// or sends its microphone (user 2026-11-11), best effort otherwise.
///
/// iOS has no public low-latency Wi-Fi API. A socket's network service type is
/// what Darwin maps to its traffic class, which picks the Wi-Fi access
/// category (VO → AC_VO) for the phone's uplink. Audio rides QUIC datagrams on
/// the one link socket, so the whole socket is marked: while audio flows that
/// socket carries only it plus small interactive traffic (input, control,
/// clipboard text, heartbeats), since bulk transfers use the separate Low TCP
/// sockets. Unlike the Mac (always voice, see [`set_service_type`]) it is
/// toggled with the role: an idle phone's heartbeats need no voice priority
/// and voice class is not free on battery or for the other stations sharing
/// the access point. No DSCP here: Quinn's per-packet TOS (ECN only)
/// overwrites a socket-level DSCP and the Darwin EF sendmsg adapter is
/// macOS-only (`udp.rs`); the service type is independent of that TOS.
#[cfg(any(target_os = "ios", test))]
pub(crate) fn audio_service_type(audio: bool) -> ServiceType {
    if audio {
        ServiceType::Voice
    } else {
        ServiceType::BestEffort
    }
}

/// macOS: set once while the socket is owned here. Low uses separate TCP
/// sockets. iOS: set and reset through a duplicate of the endpoint socket's
/// descriptor (`audio_service_type`); the option belongs to the socket, not
/// the descriptor. Elsewhere a no-op.
pub(crate) fn set_service_type(sock: &std::net::UdpSocket, service: ServiceType) {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        use std::os::fd::AsRawFd;
        let v: libc::c_int = service.net_service_type();
        // SAFETY: sock owns a live descriptor and v is an initialized c_int.
        let r = unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                0x1116,
                (&v as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if r != 0 {
            tracing::warn!(
                "设置 QUIC 套接字服务类型 {service:?} 失败：{}",
                std::io::Error::last_os_error()
            );
        }
    }
    // Windows needs qWAVE or group policy. Android: Quinn's per-packet TOS
    // overwrites a socket-level DSCP there too, so IP_TOS would not reach the
    // wire without a Linux sendmsg adapter like the Darwin one.
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let _ = (sock, service);
}

/// 读回套接字当前的服务类型（测试用）。
#[cfg(all(test, target_os = "macos"))]
fn read_service_type(sock: &std::net::UdpSocket) -> i32 {
    use std::os::fd::AsRawFd;
    let mut v: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: 有效的套接字描述符；`len` 是输出缓冲的长度。
    unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            0x1116,
            (&mut v as *mut libc::c_int).cast(),
            &mut len,
        );
    }
    v
}

#[cfg(any(target_os = "macos", test))]
mod datagram;
#[cfg(any(windows, test))]
mod source_hint;
#[cfg(target_os = "macos")]
mod udp;
#[cfg(windows)]
mod windows_udp;

pub(crate) fn udp_send_status() -> crate::UdpSendStatus {
    crate::UdpSendStatus {
        #[cfg(windows)]
        windows_source_hint_resends: source_hint::resends(),
        #[cfg(not(windows))]
        windows_source_hint_resends: 0,
        #[cfg(windows)]
        windows_source_hint_segmented_resends: source_hint::segmented_resends(),
        #[cfg(not(windows))]
        windows_source_hint_segmented_resends: 0,
    }
}

// Quinn logs terminal send errors when its connection driver exits. Carry the
// destination in that existing line, also with QoS disabled. No extra send/log.
#[derive(Debug)]
struct DestinationSocket {
    inner: Arc<dyn quinn::AsyncUdpSocket>,
    send_errors: Option<SendErrors>,
    /// `Some` on the shared endpoint socket, whose receive keeps going past a
    /// per-destination unreachability (see `poll_recv`); `None` on a manual
    /// probe's dedicated socket, which must see that error.
    recv_errors: Option<std::sync::Mutex<RecvErrorBurst>>,
}
impl DestinationSocket {
    fn endpoint(
        inner: Arc<dyn quinn::AsyncUdpSocket>,
        send_errors: Option<SendErrors>,
    ) -> Arc<dyn quinn::AsyncUdpSocket> {
        Arc::new(Self {
            inner,
            send_errors,
            recv_errors: Some(Default::default()),
        })
    }
}

/// EHOSTDOWN has no `io::ErrorKind`; libc is a Darwin-only dependency here.
#[cfg(any(target_os = "macos", target_os = "ios"))]
const HOST_DOWN: Option<i32> = Some(libc::EHOSTDOWN);
#[cfg(any(target_os = "linux", target_os = "android"))]
const HOST_DOWN: Option<i32> = Some(112);
// WSAEHOSTDOWN
#[cfg(windows)]
const HOST_DOWN: Option<i32> = Some(10064);
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android",
    windows
)))]
const HOST_DOWN: Option<i32> = None;

/// A receive error that names one unreachable destination, not this socket.
/// The kinds also cover Winsock's WSAEHOSTUNREACH / WSAENETUNREACH.
fn unreachable_recv(error: &std::io::Error) -> Option<&'static str> {
    match error.kind() {
        std::io::ErrorKind::HostUnreachable => Some("host_unreachable"),
        std::io::ErrorKind::NetworkUnreachable => Some("network_unreachable"),
        _ if HOST_DOWN.is_some_and(|code| error.raw_os_error() == Some(code)) => Some("host_down"),
        _ => None,
    }
}

/// Receive errors closer together than this belong to one burst: one log line.
const RECV_ERROR_BURST_GAP: std::time::Duration = std::time::Duration::from_secs(10);
/// Like Quinn's own I/O loops, yield after this many back-to-back errors.
const RECV_ERROR_LOOP_BOUND: usize = 32;

/// Detection only: which receive errors were skipped, one line per burst.
#[derive(Debug, Default)]
struct RecvErrorBurst {
    last: Option<(Option<i32>, &'static str, std::time::Instant)>,
    repeats: u64,
}
impl RecvErrorBurst {
    /// `Some(repeats of the previous burst)` when this error starts a burst.
    fn observe(&mut self, code: Option<i32>, reason: &'static str, now: std::time::Instant) -> Option<u64> {
        match &mut self.last {
            Some((last_code, last_reason, at))
                if *last_code == code
                    && *last_reason == reason
                    && now.saturating_duration_since(*at) < RECV_ERROR_BURST_GAP =>
            {
                *at = now;
                self.repeats += 1;
                None
            }
            _ => {
                self.last = Some((code, reason, now));
                Some(std::mem::take(&mut self.repeats))
            }
        }
    }
}

/// Error-only bounded mailbox. Never changes the socket result or sends packets.
#[derive(Debug, Default)]
pub(crate) struct SendFailures {
    pub events: std::collections::VecDeque<(std::time::Instant, std::net::SocketAddr, String)>,
    pub dropped: u64,
}
pub(crate) type SendErrors = Arc<std::sync::Mutex<SendFailures>>;
impl SendFailures {
    fn observe(&mut self, destination: std::net::SocketAddr, error: &std::io::Error) {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return;
        }
        if self.events.len() == 32 {
            self.events.pop_front();
            self.dropped += 1;
        }
        self.events
            .push_back((std::time::Instant::now(), destination, error.to_string()));
    }
}
#[derive(Debug)]
struct SendError {
    destination: std::net::SocketAddr,
    error: std::io::Error,
}
impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; destination={}", self.error, self.destination)
    }
}
impl std::error::Error for SendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}
fn send_error(error: std::io::Error, destination: std::net::SocketAddr) -> std::io::Error {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        error
    } else {
        std::io::Error::new(error.kind(), SendError { destination, error })
    }
}
impl quinn::AsyncUdpSocket for DestinationSocket {
    fn create_io_poller(self: Arc<Self>) -> std::pin::Pin<Box<dyn quinn::UdpPoller>> {
        self.inner.clone().create_io_poller()
    }
    fn try_send(&self, transmit: &quinn::udp::Transmit<'_>) -> std::io::Result<()> {
        self.inner.try_send(transmit).map_err(|e| {
            if let Some(errors) = &self.send_errors {
                errors
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .observe(transmit.destination, &e);
            }
            send_error(e, transmit.destination)
        })
    }
    fn poll_recv(
        &self,
        cx: &mut std::task::Context<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let Some(burst) = &self.recv_errors else {
            return self.inner.poll_recv(cx, bufs, meta);
        };
        // macOS 27.2.1 (2026-11-12), seconds after launch while the Local
        // Network privacy policy was still being installed: one recv on the
        // shared endpoint socket returned EHOSTUNREACH (os error 65), the same
        // rejection its sends got. An unconnected UDP socket has no single
        // peer, so this names some destination, not the socket. Quinn 0.11's
        // endpoint driver ignores only ConnectionReset on recv and exits on any
        // other error, after which every connect() is EndpointStopping until
        // the process restarts. Keep receiving instead, the way the send path
        // already turns EHOSTUNREACH into packet loss (`datagram.rs`): the
        // affected connections' loss recovery and dial deadlines still apply.
        // Here, not in the Darwin adapter, so the Android/Linux endpoint gets
        // it too (Windows' adapter already skips every recv error), while
        // manual probes (`wrap_probe_socket`, no `recv_errors`) keep failing
        // fast on it. Re-poll the inner socket rather than return Pending, so
        // that it registers the waker; yield after a bounded run of errors.
        // Which kernel path queued the error on recv was not reproduced.
        for _ in 0..RECV_ERROR_LOOP_BOUND {
            let error = match self.inner.poll_recv(cx, bufs, meta) {
                std::task::Poll::Ready(Err(error)) => error,
                other => return other,
            };
            let Some(reason) = unreachable_recv(&error) else {
                return std::task::Poll::Ready(Err(error));
            };
            let started = burst.lock().unwrap_or_else(|e| e.into_inner()).observe(
                error.raw_os_error(),
                reason,
                std::time::Instant::now(),
            );
            if let Some(previous_repeats) = started {
                tracing::warn!(target: "oink_link_diagnostics", kind = "udp_recv", stage = "kernel",
                    result = "dropped", reason, os_error = error.raw_os_error().unwrap_or_default(),
                    os_error_known = error.raw_os_error().is_some(), %error, previous_repeats,
                    "UDP receive error on the shared endpoint socket; receiving continues");
            }
        }
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    }
    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
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

/// EF protects interactive traffic under TCP bulk load. Unmarked TCP roughly
/// doubled p99; BK protected input but cost 35% single-stream throughput.
/// AP queue mapping is not inferred from DSCP.
/// Verified unprivileged on macOS 14.7.8 / 26.6.2, both directions in IP captures.
/// AP WMM mapping and other routers are NOT proven. Service types stay VO / BE.
/// Quinn's per-packet TOS contains only ECN and overwrites socket-level DSCP;
/// therefore the Darwin sendmsg adapter combines EF with ECN on every datagram.
/// Receive processing, ECN, TLS and the AP input/audio route stay unchanged.
/// Windows documents QoS APIs, not IP_TOS, as the supported marking interface.
/// Windows 11 build 26200 does retain socket TOS/TCLASS on loopback, even with a
/// limited token; that does not establish NIC egress or Quinn per-packet marking.
/// No Windows marking is configured here. Arbitrary QOSSetOutgoingDSCPValue needs
/// Administrators/Network Configuration Operators.
// Android: the endpoint socket wrapper also rebinds on request (review H17).
#[cfg(any(target_os = "macos", target_os = "android", test))]
pub(crate) fn wrap_udp_socket(socket: std::net::UdpSocket) -> std::io::Result<Arc<dyn quinn::AsyncUdpSocket>> {
    Ok(DestinationSocket::endpoint(wrap_udp_socket_inner(socket)?, None))
}
/// A manual probe's socket: send failures are recorded, receive errors pass
/// through unchanged.
pub(crate) fn observe_udp_socket(
    inner: Arc<dyn quinn::AsyncUdpSocket>,
    errors: SendErrors,
) -> Arc<dyn quinn::AsyncUdpSocket> {
    Arc::new(DestinationSocket {
        inner,
        send_errors: Some(errors),
        recv_errors: None,
    })
}
pub(crate) fn wrap_udp_socket_observed(
    socket: std::net::UdpSocket,
    errors: SendErrors,
) -> std::io::Result<Arc<dyn quinn::AsyncUdpSocket>> {
    Ok(DestinationSocket::endpoint(
        wrap_udp_socket_inner(socket)?,
        Some(errors),
    ))
}
/// Manual lookups must surface native unreachability instead of treating it as
/// packet loss until Quinn's idle timeout. This socket is dedicated/connected;
/// the shared endpoint retains its existing transient-error recovery policy.
pub(crate) fn wrap_probe_socket(socket: std::net::UdpSocket) -> std::io::Result<Arc<dyn quinn::AsyncUdpSocket>> {
    #[cfg(target_os = "macos")]
    {
        udp::Socket::wrap_mode(socket, 46, true)
    }
    #[cfg(windows)]
    {
        windows_udp::Socket::wrap_mode(socket, true)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        quinn::Runtime::wrap_udp_socket(&quinn::TokioRuntime, socket)
    }
}
fn wrap_udp_socket_inner(socket: std::net::UdpSocket) -> std::io::Result<Arc<dyn quinn::AsyncUdpSocket>> {
    #[cfg(target_os = "macos")]
    {
        udp::Socket::wrap(socket, 46)
    }
    #[cfg(windows)]
    {
        windows_udp::Socket::wrap(socket)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        quinn::Runtime::wrap_udp_socket(&quinn::TokioRuntime, socket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_udp_error_has_destination_and_would_block_is_unchanged() {
        let destination = "127.0.0.1:47470".parse().unwrap();
        let error = send_error(std::io::Error::other("No route to host"), destination);
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(
            error
                .to_string()
                .contains("No route to host; destination=127.0.0.1:47470")
        );
        let error = send_error(
            std::io::Error::new(std::io::ErrorKind::WouldBlock, "would block"),
            destination,
        );
        assert_eq!(error.to_string(), "would block");
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn separate_sockets_keep_their_service_types() {
        let interactive = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let bulk = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        set_service_type(&interactive, ServiceType::Voice);
        set_service_type(&bulk, ServiceType::BestEffort);
        assert_eq!(read_service_type(&interactive), 4);
        assert_eq!(read_service_type(&bulk), 0);
    }
    #[test]
    fn audio_role_picks_voice_or_best_effort() {
        assert_eq!(audio_service_type(true), ServiceType::Voice);
        assert_eq!(audio_service_type(false), ServiceType::BestEffort);
    }
    /// The iOS path (Darwin, same option) on the Mac: marked and unmarked
    /// through a duplicate descriptor, read back on the endpoint's own.
    #[cfg(target_os = "macos")]
    #[test]
    fn service_type_toggles_through_a_duplicate_descriptor() {
        let endpoint = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let marker = endpoint.try_clone().unwrap();
        for (audio, expected) in [(true, 4), (false, 0), (true, 4), (false, 0)] {
            set_service_type(&marker, audio_service_type(audio));
            assert_eq!(read_service_type(&endpoint), expected, "audio {audio}");
        }
    }
}

#[cfg(test)]
mod recv_tests;

#[cfg(test)]
mod send_failure_tests {
    use super::*;
    #[derive(Debug)]
    struct FailingSend;
    impl quinn::AsyncUdpSocket for FailingSend {
        fn create_io_poller(self: Arc<Self>) -> std::pin::Pin<Box<dyn quinn::UdpPoller>> {
            unreachable!("send-only fixture")
        }
        fn try_send(&self, _: &quinn::udp::Transmit<'_>) -> std::io::Result<()> {
            Err(std::io::ErrorKind::NetworkUnreachable.into())
        }
        fn poll_recv(
            &self,
            _: &mut std::task::Context<'_>,
            _: &mut [std::io::IoSliceMut<'_>],
            _: &mut [quinn::udp::RecvMeta],
        ) -> std::task::Poll<std::io::Result<usize>> {
            unreachable!("send-only fixture")
        }
        fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
            unreachable!("send-only fixture")
        }
    }
    #[test]
    fn observed_socket_preserves_terminal_error_and_records_destination() {
        let failures = SendErrors::default();
        let socket = observe_udp_socket(Arc::new(FailingSend), failures.clone());
        let destination = "127.0.0.1:47470".parse().unwrap();
        let error = socket
            .try_send(&quinn::udp::Transmit {
                destination,
                contents: b"packet",
                ecn: None,
                segment_size: None,
                src_ip: None,
            })
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NetworkUnreachable);
        assert!(error.to_string().contains(&destination.to_string()));
        let failures = failures.lock().unwrap();
        assert_eq!(failures.events.len(), 1);
        assert_eq!(failures.events[0].1, destination);
        assert_eq!(failures.dropped, 0);
    }
    #[test]
    fn error_observations_are_bounded_and_ignore_would_block() {
        let mut failures = SendFailures::default();
        let destination = "127.0.0.1:47470".parse().unwrap();
        failures.observe(destination, &std::io::ErrorKind::WouldBlock.into());
        assert!(failures.events.is_empty());
        for _ in 0..35 {
            failures.observe(destination, &std::io::ErrorKind::NetworkUnreachable.into());
        }
        assert_eq!(failures.events.len(), 32);
        assert_eq!(failures.dropped, 3);
        assert!(
            failures
                .events
                .iter()
                .all(|(_, address, error)| *address == destination && !error.is_empty())
        );
    }
}
