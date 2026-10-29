//! Local capture only. This crate has no network caller or service/helper grant.
pub mod capsule;
#[cfg(windows)]
pub mod collector;
#[cfg(windows)]
pub mod dump;
pub mod local;
pub mod macos;
#[cfg(windows)]
pub mod staging;
#[cfg(windows)]
pub mod windows;
