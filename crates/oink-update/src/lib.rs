//! Update policy and verification shared by the backend, SYSTEM service and executor.
//! No production trust-root override, installer commands or user-provided paths.
pub mod barrier;
pub mod feed;
pub mod ipc;
pub mod package;
pub mod policy;
pub mod transaction;
pub mod trust;

pub const PRODUCT: &str = "com.hachiwii.oink";
pub const WINDOWS_EPOCH: &str = "win-2026-01";
pub const MAC_EPOCH: &str = "mac-2026-01";
/// Signed document/IPC schema. Independent of package inspection capabilities.
pub const SCHEMA: u32 = 1;
/// Highest minimumUpdaterSchema admitted by this Windows updater. Capability 2
/// inspects packages containing the exact bundled optional audio driver archive.
pub const UPDATER_CAPABILITY: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum Error {
    #[error("invalid update metadata")]
    Metadata,
    #[error("update signature rejected")]
    Signature,
    #[error("update information expired")]
    ExpiredFeed,
    #[error("update replay rejected")]
    Replay,
    #[error("clock is not trustworthy")]
    Clock,
    #[error("update URL rejected")]
    Url,
    #[error("update exceeds resource limit")]
    Limit,
    #[error("update policy mismatch")]
    Policy,
    #[error("update requires a compatible operating system")]
    OsUnsupported,
    #[error("update preparation or installer is unavailable")]
    Busy,
    #[error("transaction state rejected")]
    Transaction,
    #[error("repair required")]
    RepairRequired,
    #[error("protected state could not be persisted")]
    Storage,
    #[error("release trust is not configured")]
    NotConfigured,
    #[error("network request failed")]
    Network,
}

/// Official packages inject one display version for every executable and receipt.
pub const RELEASE_VERSION: &str = match option_env!("OINK_RELEASE_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
