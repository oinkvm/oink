//! Existing SYSTEM input service plus its fixed user/UIAccess backend launch entry.
pub mod helper;
pub mod owner;
#[cfg(windows)]
mod platform;
pub mod power;
pub mod wire;
#[cfg(windows)]
pub use platform::{backend, client, prelogin, run, update};
#[cfg(any(windows, test))]
mod backend_launch;
#[cfg(any(windows, test))]
mod launch_recipe;
#[cfg(any(windows, test))]
mod prelogin_policy;
#[cfg(windows)]
pub use prelogin_policy::{OwnerView, SnapshotFile};

#[cfg(test)]
mod isolation_tests;

#[cfg(any(windows, test))]
mod token_policy;

#[cfg(any(windows, test))]
mod console_policy;
#[cfg(any(windows, test))]
mod prelogin_route;
#[cfg(any(windows, test))]
mod supervision;

#[cfg(any(windows, test))]
mod lease;

mod auto_unlock;
pub use auto_unlock::{
    AUTO_UNLOCK_SYNC_WINDOW, AutoUnlockFailure, AutoUnlockResult, AutoUnlockStatus, AutoUnlockSync,
    auto_unlock_attempt, auto_unlock_disable, auto_unlock_remote_input, auto_unlock_save, auto_unlock_status,
    auto_unlock_sync_attempt,
};
