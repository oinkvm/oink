//! Local Low operation values. Display text never travels on the wire.
use oink_foundation::DeviceId;
pub use oink_foundation::transfer::LowOptions;
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LowTcpPurpose {
    Transfer,
    Warm,
    Race,
}

/// Failure of a specific directed Low hop, also forwarded through relay setup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LowUnavailable {
    pub from: DeviceId,
    pub to: DeviceId,
    pub reason: LowFailureReason,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LowFailureReason {
    NoDirectNeighbor,
    Tcp { stage: String, detail: String },
}
impl std::fmt::Display for LowUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "文件与大内容传输不可用（{} → {}）：", self.from, self.to)?;
        match &self.reason {
            LowFailureReason::Tcp { stage, detail } => write!(f, "TCP/TLS {stage}: {detail}"),
            LowFailureReason::NoDirectNeighbor => write!(f, "中继邻居不可直连，请检查中继连接"),
        }
    }
}
