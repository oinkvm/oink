//! Isolated loopback configurations for tests; never reads product overrides.
use oink_network::{MdnsMode, NetworkConfig};
use std::{
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
};

pub fn network_config(data_dir: PathBuf, name: &str, port: u16) -> NetworkConfig {
    let port = if port == 0 {
        super::ports::test_port_pair()
    } else {
        port
    };
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

pub use oink_network::link::testing::{datagram_pair, reliable_pair};
