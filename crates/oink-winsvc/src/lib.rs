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
