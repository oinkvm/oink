//! 组网配置。

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

pub use crate::discovery::MdnsMode;

/// 正式 QUIC 端口（交互连接；发现端口 = 47471，大块数据连接 = 47472）。
pub const DEFAULT_PORT: u16 = 47470;

/// 组网配置。
#[derive(Clone, Debug)]
pub struct NetworkConfig {
    /// 数据目录：设备密钥、组名单、历史地址。
    pub data_dir: PathBuf,
    /// 设备名（首次启动时使用；之后以组名单里的名字为准，改名用 `rename_device`）。
    pub device_name: String,
    /// `"macos"` / `"windows"` / …
    pub os: String,
    /// 监听地址。默认 `0.0.0.0`；测试用 `127.0.0.1`。
    pub bind_ip: IpAddr,
    /// QUIC 端口（交互连接）；发现端口 = 端口 + 1。
    pub port: u16,
    /// Low TCP/TLS 监听端口（默认 = 端口 + 2；0 = 随机），直连/中转每跳共用。
    pub bulk_port: u16,
    /// Enables activity-gated TCP Low pacing. Both neighbors
    /// must agree to disable this policy. This is not the current activity state.
    pub bulk_pacing: bool,
    pub mdns: MdnsMode,
    /// UDP 广播信标。
    pub beacon: bool,
    /// 信标广播到哪些端口（除了本机的发现端口）。默认 47471；同机多实例时可列出其他实例的发现端口。
    pub beacon_ports: Vec<u16>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            data_dir: oink_foundation::paths::data_dir(),
            device_name: default_device_name(),
            os: std::env::consts::OS.to_string(),
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: DEFAULT_PORT,
            bulk_port: DEFAULT_PORT + 2,
            bulk_pacing: true,
            mdns: MdnsMode::System,
            beacon: true,
            beacon_ports: vec![DEFAULT_PORT + 1],
        }
    }
}

impl NetworkConfig {
    pub fn discovery_port(&self) -> u16 {
        // Port 0 requests an ephemeral socket for every listener, including
        // isolated backend launch checks. Adding one would bind privileged port 1.
        if self.port == 0 { 0 } else { self.port.wrapping_add(1) }
    }
}

/// 主机名（去掉 `.local` 等后缀）。
pub fn default_device_name() -> String {
    let h = gethostname::gethostname().to_string_lossy().into_owned();
    let h = h.strip_suffix(".local").unwrap_or(&h).to_string();
    if h.is_empty() { "Oink device".into() } else { h }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ephemeral_network_port_also_isolates_discovery() {
        let mut config = super::NetworkConfig {
            port: 0,
            ..Default::default()
        };
        assert_eq!(config.discovery_port(), 0);
        config.port = super::DEFAULT_PORT;
        assert_eq!(config.discovery_port(), super::DEFAULT_PORT + 1);
    }
}
