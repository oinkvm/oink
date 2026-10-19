//! Darwin QUIC socket: preserve EF in the per-packet traffic class alongside ECN.
use quinn::{AsyncUdpSocket, UdpPoller, udp};
use std::{
    future::Future,
    io,
    net::SocketAddr,
    os::fd::AsRawFd,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};
use tokio::io::Interest;

#[derive(Debug)]
pub(super) struct Socket {
    io: tokio::net::UdpSocket,
    state: udp::UdpSocketState,
    dscp: u8,
    fail_fast: bool,
}
impl Socket {
    pub(super) fn wrap(socket: std::net::UdpSocket, dscp: u8) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        Self::wrap_mode(socket, dscp, false)
    }
    pub(super) fn wrap_mode(
        socket: std::net::UdpSocket,
        dscp: u8,
        fail_fast: bool,
    ) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        let state = udp::UdpSocketState::new((&socket).into())?;
        Ok(Arc::new(Self {
            io: tokio::net::UdpSocket::from_std(socket)?,
            state,
            dscp,
            fail_fast,
        }))
    }
}
type Writable = Pin<Box<dyn Future<Output = io::Result<()>> + Send + Sync>>;
struct Poller {
    socket: Arc<Socket>,
    wait: Option<Writable>,
}
impl std::fmt::Debug for Poller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DscpPoller").finish()
    }
}
impl UdpPoller for Poller {
    fn poll_writable(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.wait.is_none() {
            let socket = self.socket.clone();
            self.wait = Some(Box::pin(async move { socket.io.writable().await }));
        }
        let result = ready!(self.wait.as_mut().expect("initialized").as_mut().poll(cx));
        self.wait = None;
        Poll::Ready(result)
    }
}
impl AsyncUdpSocket for Socket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(Poller {
            socket: self,
            wait: None,
        })
    }
    fn try_send(&self, transmit: &udp::Transmit<'_>) -> io::Result<()> {
        let result = self.io.try_io(Interest::WRITABLE, || {
            send(self.io.as_raw_fd(), transmit, self.dscp, self.fail_fast)
        });
        if self.fail_fast {
            result
        } else {
            super::datagram::send_result(result, transmit)
        }
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            ready!(self.io.poll_recv_ready(cx))?;
            match self
                .io
                .try_io(Interest::READABLE, || self.state.recv((&self.io).into(), bufs, meta))
            {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                result => return Poll::Ready(result),
            }
        }
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }
    fn may_fragment(&self) -> bool {
        self.state.may_fragment()
    }
    fn max_receive_segments(&self) -> usize {
        self.state.gro_segments()
    }
    // One datagram per transmit: no private Darwin sendmsg_x ABI or segmentation.
}

fn send(fd: std::os::fd::RawFd, transmit: &udp::Transmit<'_>, dscp: u8, connected: bool) -> io::Result<()> {
    let address = socket2::SockAddr::from(transmit.destination);
    let mut iov = libc::iovec {
        iov_base: transmit.contents.as_ptr().cast_mut().cast(),
        iov_len: transmit.contents.len(),
    };
    // cmsghdr needs alignment, and Darwin CMSG_SPACE rounds payloads to four bytes.
    let mut control = [0u64; 16];
    // SAFETY: zero is a valid empty msghdr; all pointers below outlive sendmsg.
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    // Darwin rejects a destination on a connected UDP socket with EISCONN.
    // Dedicated manual probes use its connected peer, retaining DSCP/ECN.
    if !connected {
        header.msg_name = address.as_ptr().cast_mut().cast();
        header.msg_namelen = address.len();
    }
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    let ipv4 = transmit.destination.is_ipv4()
        || matches!(transmit.destination.ip(), std::net::IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some());
    let tos: libc::c_int = (i32::from(dscp) << 2) | transmit.ecn.map_or(0, |ecn| ecn as libc::c_int);
    append(
        &mut header,
        &mut control,
        if ipv4 { libc::IPPROTO_IP } else { libc::IPPROTO_IPV6 },
        if ipv4 { libc::IP_TOS } else { libc::IPV6_TCLASS },
        &tos,
    );
    if let Some(ip) = transmit.src_ip {
        match ip {
            std::net::IpAddr::V4(ip) => append(
                &mut header,
                &mut control,
                libc::IPPROTO_IP,
                libc::IP_RECVDSTADDR,
                &libc::in_addr {
                    s_addr: u32::from_ne_bytes(ip.octets()),
                },
            ),
            std::net::IpAddr::V6(ip) => append(
                &mut header,
                &mut control,
                libc::IPPROTO_IPV6,
                libc::IPV6_PKTINFO,
                &libc::in6_pktinfo {
                    ipi6_addr: libc::in6_addr { s6_addr: ip.octets() },
                    ipi6_ifindex: 0,
                },
            ),
        }
    }
    loop {
        // SAFETY: live fd, initialized header, bounded aligned control buffer and
        // borrowed payload/address; sendmsg does not mutate their contents.
        if unsafe { libc::sendmsg(fd, &header, 0) } >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
fn append<T: Copy>(header: &mut libc::msghdr, buffer: &mut [u64; 16], level: i32, kind: i32, value: &T) {
    let size = std::mem::size_of::<T>();
    // SAFETY: payload size is a small socket ancillary type.
    let space = unsafe { libc::CMSG_SPACE(size as u32) } as usize;
    let offset = header.msg_controllen as usize;
    assert!(offset + space <= std::mem::size_of_val(buffer));
    // SAFETY: aligned buffer and CMSG_SPACE-aligned offset; bounds checked above.
    // copy_nonoverlapping handles ancillary payloads with weaker alignment than T.
    unsafe {
        let cmsg = buffer.as_mut_ptr().cast::<u8>().add(offset).cast::<libc::cmsghdr>();
        (*cmsg).cmsg_level = level;
        (*cmsg).cmsg_type = kind;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size as u32);
        std::ptr::copy_nonoverlapping((value as *const T).cast::<u8>(), libc::CMSG_DATA(cmsg), size);
    }
    header.msg_controllen += space as libc::socklen_t;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn ipv4_and_ipv6_datagrams_preserve_chosen_mark_and_ecn() {
        for dscp in [0, 46] {
            for addr in ["127.0.0.1:0", "[::1]:0"] {
                let receiver = std::net::UdpSocket::bind(addr).unwrap();
                receiver
                    .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                    .unwrap();
                let sender = Socket::wrap(std::net::UdpSocket::bind(addr).unwrap(), dscp).unwrap();
                let destination = receiver.local_addr().unwrap();
                let (level, option, cmsg_kind) = if destination.is_ipv4() {
                    (libc::IPPROTO_IP, libc::IP_RECVTOS, libc::IP_RECVTOS)
                } else {
                    (libc::IPPROTO_IPV6, libc::IPV6_RECVTCLASS, libc::IPV6_TCLASS)
                };
                let on: libc::c_int = 1;
                assert_eq!(
                    // SAFETY: live socket, initialized option value with exact size.
                    unsafe {
                        libc::setsockopt(
                            receiver.as_raw_fd(),
                            level,
                            option,
                            (&on as *const libc::c_int).cast(),
                            4,
                        )
                    },
                    0
                );
                let mut poller = sender.clone().create_io_poller();
                std::future::poll_fn(|cx| poller.as_mut().poll_writable(cx))
                    .await
                    .unwrap();
                for ecn in [
                    None,
                    Some(udp::EcnCodepoint::Ect0),
                    Some(udp::EcnCodepoint::Ect1),
                    Some(udp::EcnCodepoint::Ce),
                ] {
                    sender
                        .try_send(&udp::Transmit {
                            destination,
                            ecn,
                            contents: b"mark",
                            segment_size: None,
                            src_ip: Some(destination.ip()),
                        })
                        .unwrap();
                    let mut bytes = [0u8; 32];
                    let mut iov = libc::iovec {
                        iov_base: bytes.as_mut_ptr().cast(),
                        iov_len: bytes.len(),
                    };
                    let mut control = [0u64; 16];
                    // SAFETY: empty header initialized with bounded buffers before recvmsg.
                    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
                    header.msg_iov = &mut iov;
                    header.msg_iovlen = 1;
                    header.msg_control = control.as_mut_ptr().cast();
                    header.msg_controllen = std::mem::size_of_val(&control) as _;
                    // SAFETY: all output buffers are valid for the lengths in header.
                    assert_eq!(unsafe { libc::recvmsg(receiver.as_raw_fd(), &mut header, 0) }, 4);
                    assert_eq!(&bytes[..4], b"mark");
                    let mut tos = None;
                    // SAFETY: CMSG traversal stays inside the returned ancillary buffer.
                    unsafe {
                        let mut item = libc::CMSG_FIRSTHDR(&header);
                        while !item.is_null() {
                            if (*item).cmsg_level == level && (*item).cmsg_type == cmsg_kind {
                                tos = Some(if destination.is_ipv4() {
                                    *libc::CMSG_DATA(item) as i32
                                } else {
                                    std::ptr::read_unaligned(libc::CMSG_DATA(item).cast::<i32>())
                                });
                            }
                            item = libc::CMSG_NXTHDR(&header, item);
                        }
                    }
                    assert_eq!(tos, Some((i32::from(dscp) << 2) | ecn.map_or(0, |e| e as i32)));
                }
            }
        }
    }
}
