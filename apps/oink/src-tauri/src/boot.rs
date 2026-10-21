//! UI launch settings. All real backend access uses local IPC.
use serde::Serialize;

/// 前端启动时读取（`boot_info` 命令），字段与 `src/api/types.ts` 的 `BootInfo` 对应。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BootInfo {
    pub backend: &'static str,
    pub platform: &'static str,
    pub version: &'static str,
    pub build: Option<String>,
    pub open_update_failure: bool,
}

impl BootInfo {
    pub fn new(connected: bool) -> Self {
        Self {
            open_update_failure: false,
            backend: if connected { "agent" } else { "none" },
            platform: if cfg!(target_os = "macos") {
                "macos"
            } else if cfg!(windows) {
                "windows"
            } else {
                "linux"
            },
            version: option_env!("OINK_RELEASE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")),
            build: option_env!("OINK_RELEASE_BUILD")
                .and_then(|value| value.parse::<u64>().ok())
                .map(|value| value.to_string()),
        }
    }
}
