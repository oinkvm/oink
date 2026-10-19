//! Resource bounds for a logical bulk operation.
/// Conservative wire-byte bounds (encoded Messages plus record headers).
/// `operation_max` covers the entire paste/drag, including all files and streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LowOptions {
    pub send_max: u64,
    pub receive_max: u64,
    pub operation_max: u64,
}
impl LowOptions {
    pub fn valid(self) -> bool {
        self.send_max
            .checked_add(self.receive_max)
            .is_some_and(|n| n <= self.operation_max)
    }
    /// Cost of one encoded Message, including all fixed transport headers.
    pub fn message_bytes(encoded: usize) -> u64 {
        encoded as u64
    }
}
