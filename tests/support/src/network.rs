//! Isolated loopback configurations for tests; never reads product overrides.
use oink_network::{MdnsMode, NetworkConfig};
use std::{
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
};

pub fn network_config(data_dir: PathBuf, name: &str, port: u16) -> NetworkConfig {
    let port = if port == 0 { test_port_pair() } else { port };
    NetworkConfig {
        data_dir,
        device_name: name.to_string(),
        os: std::env::consts::OS.to_string(),
        bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port,
        bulk_port: 0,
        bulk_pacing: true,
        mdns: MdnsMode::Off,
        beacon: false,
        beacon_ports: Vec::new(),
    }
}

/// Keep isolated fixture ports below the OS ephemeral range. On Windows, two
/// consecutive `bind(:0)` calls can allocate N and N+1 before discovery binds
/// N+1, making one node permanently undiscoverable. Each fixture gets a distinct
/// pair. Low transport listens on TCP at the QUIC port too; probe that socket
/// as well so another fixture's TCP listener/TIME_WAIT cannot break Low startup.
fn test_port_pair() -> u16 {
    use std::sync::atomic::{AtomicU32, Ordering};
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

pub use oink_network::link::testing::{datagram_pair, reliable_pair};
