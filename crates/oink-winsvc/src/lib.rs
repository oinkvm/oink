//! Existing SYSTEM input service plus its fixed user/UIAccess backend launch entry.
pub mod helper;
pub mod owner;
#[cfg(windows)]
mod platform;
pub mod power;
pub mod wire;
#[cfg(windows)]
pub use platform::{backend, client, run, update};
#[cfg(any(windows, test))]
mod backend_launch;
#[cfg(any(windows, test))]
mod launch_recipe;

#[cfg(test)]
mod isolation_tests;

#[cfg(any(windows, test))]
mod token_policy;
