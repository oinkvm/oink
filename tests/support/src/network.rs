//! Isolated loopback configurations for tests; never reads product overrides.
use oink_network::{MdnsMode, NetworkConfig};
use std::{
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
};

/// Reserve real loopback sockets when `port` is zero; explicit ports are caller-owned.
pub fn network_config(data_dir: PathBuf, name: &str, port: u16) -> NetworkConfig {
    network_config_with_bind(data_dir, name, port, Ipv4Addr::LOCALHOST.into())
}

/// Choose the bind address before reserving sockets, including wildcard fixtures.
/// Do not change `bind_ip` on an automatically allocated configuration afterwards.
pub fn network_config_with_bind(data_dir: PathBuf, name: &str, port: u16, bind_ip: IpAddr) -> NetworkConfig {
    let port = if port == 0 {
        oink_network::test_ports::reserve(bind_ip)
    } else {
        port
    };
    NetworkConfig {
        data_dir,
        device_name: name.to_string(),
        os: std::env::consts::OS.to_string(),
        bind_ip,
        port,
        bulk_port: 0,
        bulk_pacing: true,
        mdns: MdnsMode::Off,
        beacon: false,
        beacon_ports: Vec::new(),
    }
}

pub use oink_network::link::testing::{datagram_pair, reliable_pair};
