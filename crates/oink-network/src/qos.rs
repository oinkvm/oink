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
    #[cfg(all(test, target_os = "macos"))]
    BestEffort,
}
impl ServiceType {
    #[cfg(target_os = "macos")]
    fn net_service_type(self) -> i32 {
        match self {
            Self::Voice => 4,
            #[cfg(test)]
            Self::BestEffort => 0,
        }
    }
}

/// Set once while the socket is owned here. Low uses separate TCP sockets.
pub(crate) fn set_service_type(sock: &std::net::UdpSocket, service: ServiceType) {
    #[cfg(target_os = "macos")]
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
    #[cfg(not(target_os = "macos"))]
    let _ = (sock, service); // Windows needs qWAVE or group policy.
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
struct DestinationSocket(Arc<dyn quinn::AsyncUdpSocket>);
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
        self.0.clone().create_io_poller()
    }
    fn try_send(&self, transmit: &quinn::udp::Transmit<'_>) -> std::io::Result<()> {
        self.0
            .try_send(transmit)
            .map_err(|e| send_error(e, transmit.destination))
    }
    fn poll_recv(
        &self,
        cx: &mut std::task::Context<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0.poll_recv(cx, bufs, meta)
    }
    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.0.local_addr()
    }
    fn may_fragment(&self) -> bool {
        self.0.may_fragment()
    }
    fn max_transmit_segments(&self) -> usize {
        self.0.max_transmit_segments()
    }
    fn max_receive_segments(&self) -> usize {
        self.0.max_receive_segments()
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
pub(crate) fn wrap_udp_socket(socket: std::net::UdpSocket) -> std::io::Result<Arc<dyn quinn::AsyncUdpSocket>> {
    Ok(Arc::new(DestinationSocket(wrap_udp_socket_inner(socket)?)))
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
}
