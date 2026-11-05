//! 基础设施：各模块共用的数据类型，以及配置目录与日志。
//!
//! Shared types define the contracts between modules; changes must remain consistent
//! across their producers and consumers.

pub mod clipboard;
pub mod contacts;
pub mod content;
pub mod display;
pub mod dnd;
pub mod drag_trace;
pub mod geom;
pub mod handoff_timing;
pub mod id;
pub mod input;
pub mod input_transform;
pub mod logging;
pub mod panel_policy;
pub mod paths;
pub mod process;
pub mod realtime;
pub mod session;
pub mod snapshot;
pub mod storage;

pub use geom::{Point, Rect, Size};
pub use id::{DeviceId, GroupId};

pub mod wake;

/// Read a development override only in debug builds. The name and environment
/// lookup are removed by conditional compilation in production callers.
#[macro_export]
macro_rules! debug_env {
    ($name:literal) => {{
        #[cfg(debug_assertions)]
        {
            ::std::env::var($name)
        }
        #[cfg(not(debug_assertions))]
        {
            ::std::result::Result::<String, ::std::env::VarError>::Err(::std::env::VarError::NotPresent)
        }
    }};
    (os $name:literal) => {{
        #[cfg(debug_assertions)]
        {
            ::std::env::var_os($name)
        }
        #[cfg(not(debug_assertions))]
        {
            ::std::option::Option::<::std::ffi::OsString>::None
        }
    }};
}

pub mod maintenance;

pub mod pending_age;
pub mod transfer;
