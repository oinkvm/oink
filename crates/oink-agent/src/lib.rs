//! Background agent composition and authenticated local UI/CLI interfaces.
//! Resource owners apply state; device cards expose coherent presentation snapshots.

mod agent_api;
mod aim;
pub mod api;
pub mod appearance;
mod attachment;
mod audio_quality;
mod cache;
pub mod clip;
pub mod config;
pub mod config_module;
mod connection_quality;
pub mod device_cards;
pub mod device_controls;
mod display_overlay;
mod dnd;
mod event_delivery;
#[cfg(target_os = "macos")]
mod file_provider;
pub mod host;
mod hotkeys;
pub mod instance;
pub mod ipc;
pub mod layout_profiles;
pub mod layout_sync;
pub mod logs;

pub mod node;
mod projection;
pub mod remote;
pub mod telemetry;
mod wallpaper;
#[cfg(any(windows, test))]
mod windows_permissions;
#[cfg(windows)]
pub use oink_input::dispatch_raw_clip_guard;

mod audio;
pub mod backend;
pub mod diagnostics;
pub mod streams;
pub mod threads;

#[cfg(all(test, debug_assertions))]
mod snapshot_tests;

#[cfg(all(test, debug_assertions))]
mod two_nodes_tests;

#[cfg(all(test, debug_assertions))]
use snapshot_tests::common::watchdog as test_watchdog;

pub mod update;

pub mod native_controls;
pub mod tray;
