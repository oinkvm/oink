//! Reserve listeners before any discovery, platform capture or peer dialing.
use std::{
    io,
    net::{SocketAddr, TcpListener, UdpSocket},
};

fn bind<T>(address: SocketAddr, transport: &str, create: impl Fn(SocketAddr) -> io::Result<T>) -> io::Result<T> {
    match create(address) {
        Ok(socket) => {
            tracing::info!(%address, transport, "network listener bound");
            Ok(socket)
        }
        Err(error)
            if address.port() != 0
                && matches!(error.kind(), io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied) =>
        {
            // WSAEACCES includes Windows excluded port ranges. Do not wait/retry
            // that port: ask the OS once, and preserve the native failure in logs.
            tracing::warn!(%address, transport, %error, os_error = ?error.raw_os_error(), "network port unavailable; requesting an OS-assigned port");
            create(SocketAddr::new(address.ip(), 0))
        }
        Err(error) => Err(error),
    }
}
pub(crate) fn udp(address: SocketAddr) -> io::Result<UdpSocket> {
    let socket = bind(address, "quic_udp", |address| {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(socket) = crate::test_ports::take_udp(address) {
            return Ok(socket);
        }
        UdpSocket::bind(address)
    })?;
    tracing::info!(address = %socket.local_addr()?, "QUIC UDP listening");
    Ok(socket)
}
pub(crate) fn tcp(address: SocketAddr) -> io::Result<TcpListener> {
    let socket = bind(address, "low_tcp", |address| {
        // Test reservations transfer at the same eager bind point as production.
        #[cfg(any(test, feature = "test-support"))]
        if let Some(socket) = crate::test_ports::take_tcp(address) {
            #[cfg(not(windows))]
            socket2::SockRef::from(&socket).set_reuse_address(true)?;
            socket.set_nonblocking(true)?;
            return Ok(socket);
        }
        let socket = socket2::Socket::new(
            socket2::Domain::for_address(address),
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;
        #[cfg(not(windows))]
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&address.into())?;
        socket.listen(128)?;
        Ok(TcpListener::from(socket))
    })?;
    tracing::info!(address = %socket.local_addr()?, "Low TCP listening");
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn denied_port_asks_os_once_and_other_errors_remain_fatal() {
        let address = "127.0.0.1:47470".parse().unwrap();
        let calls = std::cell::RefCell::new(Vec::new());
        let result = bind(address, "test", |next| {
            calls.borrow_mut().push(next);
            if next.port() == 0 {
                Ok(())
            } else {
                Err(io::ErrorKind::PermissionDenied.into())
            }
        });
        assert!(result.is_ok());
        assert_eq!(calls.borrow().as_slice(), &[address, "127.0.0.1:0".parse().unwrap()]);
        calls.borrow_mut().clear();
        assert!(
            bind(address, "test", |next| {
                calls.borrow_mut().push(next);
                Err::<(), _>(io::ErrorKind::InvalidInput.into())
            })
            .is_err()
        );
        assert_eq!(calls.borrow().as_slice(), &[address]);
    }
    #[test]
    fn occupied_udp_and_tcp_fall_back_without_waiting() {
        for (hold_udp, hold_tcp) in [(true, false), (false, true), (true, true)] {
            let address = SocketAddr::from((
                std::net::Ipv4Addr::LOCALHOST,
                crate::test_ports::reserve(std::net::Ipv4Addr::LOCALHOST.into()),
            ));
            let occupied_udp = crate::test_ports::take_udp(address).unwrap();
            let occupied_tcp = crate::test_ports::take_tcp(address).unwrap();
            let udp_guard = hold_udp.then_some(occupied_udp);
            let tcp_guard = hold_tcp.then_some(occupied_tcp);
            let actual_udp = udp(address).unwrap();
            let actual_tcp = tcp(address).unwrap();
            assert_eq!(actual_udp.local_addr().unwrap() == address, !hold_udp);
            assert_eq!(actual_tcp.local_addr().unwrap() == address, !hold_tcp);
            drop((udp_guard, tcp_guard));
        }
    }
}
