//! One observable state for native file/representation delivery. UI, diagnostics
//! and later transfer controls observe this record rather than infer state from
//! an application's stream count or from which device owns keyboard focus.
use super::Unavailable;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Offered,
    Preparing,
    Receiving,
    Reconnecting,
    Stored,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub phase: Phase,
    pub discovered: u64,
    pub files_done: u64,
    pub bytes: u64,
    pub total_files: Option<u64>,
    pub total_bytes: Option<u64>,
    pub failure: Option<Unavailable>,
    pub source_acknowledged: bool,
    /// Content-derived receipt is carried separately, never in this observable
    /// diagnostic record. Stored means durable local readers are available.
    pub enumerating: bool,
}
impl Progress {
    pub(super) fn new() -> Self {
        Self {
            phase: Phase::Offered,
            discovered: 0,
            files_done: 0,
            bytes: 0,
            total_files: None,
            total_bytes: None,
            failure: None,
            source_acknowledged: false,
            enumerating: false,
        }
    }
}
