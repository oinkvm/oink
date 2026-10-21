//! Shared port allocation for isolated network fixtures. Uses no product configuration.
/// Keep isolated fixture ports below the OS ephemeral range. On Windows, two
/// consecutive `bind(:0)` calls can allocate N and N+1 before discovery binds
/// N+1, making one node permanently undiscoverable. Each fixture gets a distinct
/// pair. Low transport listens on TCP at the QUIC port too; probe that socket
/// as well so another fixture's TCP listener/TIME_WAIT cannot break Low startup.
/// Windows can also assign UDP `:0` a port excluded only for TCP, causing Low
/// bootstrap to fail with WSAEACCES on otherwise healthy loopback peers. The
/// shared, jointly probed range prevents that fixture assumption (Windows 11);
/// product binding and unavailable-listener behavior are unchanged.
pub fn test_port_pair() -> u16 {
    use std::{
        net::Ipv4Addr,
        sync::atomic::{AtomicU32, Ordering},
    };
    static NEXT: AtomicU32 = AtomicU32::new(0);
    for _ in 0..10_000 {
        let offset = NEXT.fetch_add(1, Ordering::Relaxed);
        let port = 10_000 + ((std::process::id().wrapping_mul(97).wrapping_add(offset) % 10_000) * 2) as u16;
        if let Ok(_quic) = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, port))
            && let Ok(_discovery) = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, port + 1))
            && let Ok(_low) = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        {
            return port;
        }
    }
    panic!("no isolated test QUIC/discovery port pair available");
}
