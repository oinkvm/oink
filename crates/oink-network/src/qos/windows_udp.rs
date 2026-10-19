//! Tokio/quinn-udp adapter with a single source-hint rejection fallback.
use quinn::{AsyncUdpSocket, UdpPoller, udp};
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, ready},
    time::{Duration, Instant},
};
use tokio::io::Interest;

#[derive(Debug)]
pub(super) struct Socket {
    io: tokio::net::UdpSocket,
    state: udp::UdpSocketState,
    last_send_error: Mutex<Option<Instant>>,
    fail_fast: bool,
}
impl Socket {
    pub(super) fn wrap(socket: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        Self::wrap_mode(socket, false)
    }
    pub(super) fn wrap_mode(socket: std::net::UdpSocket, fail_fast: bool) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        let state = udp::UdpSocketState::new((&socket).into())?;
        Ok(Arc::new(Self {
            io: tokio::net::UdpSocket::from_std(socket)?,
            state,
            last_send_error: Mutex::new(None),
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
        f.debug_struct("WindowsUdpPoller").finish()
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
        // The dedicated connected probe has one peer and no source hint/GSO.
        // Use connected send so Winsock reports that peer's ICMP failures.
        if self.fail_fast {
            return self.io.try_send(transmit.contents).map(|_| ());
        }
        // Both attempts share Tokio's readiness guard: WouldBlock on either
        // attempt clears write readiness and wakes Quinn via its own poller.
        self.io.try_io(Interest::WRITABLE, || {
            let mut resent = false;
            let mut segmented_host_unreachable = false;
            // Native tests reproduced 10065 with source hint + UDP
            // segmentation; removing only the hint preserved segmentation and
            // arrived with the correct source. The helper retries that exact
            // case, alongside the previously approved hinted 10022 workaround.
            let result = super::source_hint::send(
                transmit,
                |tx| {
                    let result = self.state.try_send((&self.io).into(), tx);
                    if tx.src_ip.is_some() {
                        segmented_host_unreachable =
                            result.as_ref().err().is_some_and(|e| e.raw_os_error() == Some(10065))
                                && tx.segment_size.is_some();
                    }
                    result
                },
                || {
                    resent = true;
                },
            );
            if resent {
                super::source_hint::record(transmit.destination, segmented_host_unreachable);
            }
            match result {
                Err(error) if !self.fail_fast && !resent && error.kind() != io::ErrorKind::WouldBlock => {
                    // Preserve quinn-udp::send's log-and-drop policy for all
                    // other errors, without making a second system send.
                    let now = Instant::now();
                    let mut last = self.last_send_error.lock().expect("send error log");
                    if last.is_none_or(|last| now.saturating_duration_since(last) > Duration::from_secs(60)) {
                        *last = Some(now);
                        tracing::warn!(
                            ?error,
                            destination = %transmit.destination,
                            src_ip = ?transmit.src_ip,
                            ecn = ?transmit.ecn,
                            len = transmit.contents.len(),
                            segment_size = ?transmit.segment_size,
                            "sendmsg error"
                        );
                    }
                    Ok(())
                }
                result => result,
            }
        })
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // Identical to Quinn's Tokio receive path.
        loop {
            ready!(self.io.poll_recv_ready(cx))?;
            match self
                .io
                .try_io(Interest::READABLE, || self.state.recv((&self.io).into(), bufs, meta))
            {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(_) if !self.fail_fast => continue,
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
    fn max_transmit_segments(&self) -> usize {
        if self.fail_fast {
            1
        } else {
            self.state.max_gso_segments()
        }
    }
    fn max_receive_segments(&self) -> usize {
        self.state.gro_segments()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // This is a normal-path native socket test, not a reproduction of the
    // transient destination-cache state that rejects valid hints with WSAEINVAL.
    #[tokio::test]
    async fn loopback_hinted_and_unhinted_datagrams_and_receive_metadata() {
        for addr in ["127.0.0.1:0", "[::1]:0"] {
            let sender = Socket::wrap(std::net::UdpSocket::bind(addr).unwrap()).unwrap();
            let receiver = Socket::wrap(std::net::UdpSocket::bind(addr).unwrap()).unwrap();
            let destination = receiver.local_addr().unwrap();
            let mut first = sender.clone().create_io_poller();
            let mut second = sender.clone().create_io_poller();
            for poller in [&mut first, &mut second] {
                std::future::poll_fn(|cx| poller.as_mut().poll_writable(cx))
                    .await
                    .unwrap();
            }
            for src_ip in [None, Some(destination.ip())] {
                sender
                    .try_send(&udp::Transmit {
                        destination,
                        ecn: Some(udp::EcnCodepoint::Ect0),
                        contents: b"normal",
                        segment_size: None,
                        src_ip,
                    })
                    .unwrap();
                let mut bytes = [0; 64];
                let mut bufs = [io::IoSliceMut::new(&mut bytes)];
                let mut meta = [udp::RecvMeta::default()];
                assert_eq!(
                    tokio::time::timeout(
                        Duration::from_secs(2),
                        std::future::poll_fn(|cx| receiver.poll_recv(cx, &mut bufs, &mut meta))
                    )
                    .await
                    .unwrap()
                    .unwrap(),
                    1
                );
                assert_eq!(&bytes[..meta[0].len], b"normal");
                assert_eq!(meta[0].dst_ip, Some(destination.ip()));
                assert_eq!(meta[0].ecn, Some(udp::EcnCodepoint::Ect0));
            }
        }
    }
}
