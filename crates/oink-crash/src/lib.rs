//! Local capture only. This crate has no network caller or service/helper grant.
pub mod capsule;
#[cfg(windows)]
pub mod collector;
pub mod dump;
pub mod macos;
pub mod staging;
#[cfg(windows)]
pub mod windows;
