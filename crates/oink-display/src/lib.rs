//! 屏幕与布局。
//!
//! - [`DisplayBackend`]：枚举本机显示器、监听变化。平台实现在 `src/macos`、`src/windows`。
//! - [`layout`]: native display groups expanded into a shared reference canvas.
//!
//! Native writes are explicit transactions gated by the owner preference.
//! Windows also applies the current layout once at application startup.
//! Enumeration and ordinary layout synchronization never apply.

use std::sync::Arc;

pub use oink_foundation::display::DisplayInfo;

pub mod arrangement;

pub mod layout;
#[cfg(target_os = "macos")]
mod macos;
pub mod projection;
pub mod wallpaper;
#[cfg(windows)]
mod windows;

#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    #[error("本平台不支持")]
    Unsupported,
    #[error("系统调用失败：{0}")]
    Platform(String),
    #[error(transparent)]
    Arrangement(#[from] arrangement::ArrangementError),
}

/// 显示器配置变化（插拔、分辨率、缩放、排列、旋转）时的回调，可能在任意线程调用。
pub type ChangeHandler = Arc<dyn Fn() + Send + Sync>;

pub trait DisplayBackend: Send + Sync {
    /// Full baseline including mirror followers and mode identity for stale-write fencing.
    fn native_configuration(&self) -> Result<arrangement::NativeConfiguration, DisplayError> {
        Err(DisplayError::Unsupported)
    }
    /// Origin-only, session-scoped write. The implementation revalidates expected
    /// against a fresh native read, then returns the actual observed result.
    fn apply_native_origins(
        &self,
        _expected: &arrangement::NativeConfiguration,
        _origins: &[arrangement::NativeOrigin],
    ) -> Result<arrangement::NativeConfiguration, DisplayError> {
        Err(DisplayError::Unsupported)
    }
    /// Initialize owner-local Windows disabled-target storage. Other platforms
    /// leave this unused; it never changes native configuration by itself.
    fn initialize_visibility(&self, _root: &std::path::Path) -> Result<(), DisplayError> {
        Ok(())
    }
    /// Explicit layout action. Windows also removes/reconnects hidden targets;
    /// other platforms retain their existing Oink-only visibility behavior.
    fn apply_native_layout(
        &self,
        expected: &arrangement::NativeConfiguration,
        origins: &[arrangement::NativeOrigin],
        _disabled: &[(String, bool)],
    ) -> Result<arrangement::NativeConfiguration, DisplayError> {
        if origins.is_empty() || expected.with_origins(origins)? == *expected {
            Ok(expected.clone())
        } else {
            self.apply_native_origins(expected, origins)
        }
    }
    /// Re-enable only targets disabled by Oink, preserving active source origins.
    /// This is an Oink release, so its observation must keep hidden preferences.
    fn release_disabled_displays(&self) -> Result<(), DisplayError> {
        Ok(())
    }
    /// Oink-disabled targets now active in the OS. No display writes; the caller persists
    /// enabled owner preferences before acknowledging/removing these records.
    fn reconnected_displays(&self) -> Result<Vec<String>, DisplayError> {
        Ok(Vec::new())
    }
    fn acknowledge_reconnected(&self, _keys: &[String]) -> Result<(), DisplayError> {
        Ok(())
    }
    /// 当前所有显示器（含虚拟显示器，由 `is_virtual` 标出）。Windows 也保留仍连接、
    /// 但被 Oink 禁用的目标，让布局里仍可取消隐藏；原生活动状态由后端单独查询。
    fn displays(&self) -> Result<Vec<DisplayInfo>, DisplayError>;
    /// Active native desktop geometry for cursor routing, confinement and overlays.
    /// Unlike the layout catalog, this never includes an OS-disabled target.
    fn active_displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        self.displays()
    }
    /// 注册变化回调；实现可以合并短时间内的多次变化。
    fn watch(&self, on_change: ChangeHandler) -> Result<(), DisplayError>;
    /// 各屏幕当前桌面壁纸的缩略图（[`wallpaper`]）。拿不到静态图片的屏幕不在列表里；`None`：本平台
    /// 没有实现，或这一次没能向系统查询（调用方保留上一次的结果）。会解码图片，不要在异步任务里直接调用。
    fn wallpapers(&self) -> Option<Vec<wallpaper::DisplayWallpaper>> {
        None
    }
    /// 注册「壁纸可能变了」的回调（系统只在切换桌面空间时通知；显示器变化用 [`Self::watch`]）。
    fn watch_wallpaper(&self, _on_change: ChangeHandler) -> Result<(), DisplayError> {
        Ok(())
    }
}

/// 创建本机平台的实现。
pub fn platform_backend() -> Result<Box<dyn DisplayBackend>, DisplayError> {
    #[cfg(target_os = "macos")]
    return macos::backend();
    #[cfg(windows)]
    return windows::backend();
    #[allow(unreachable_code)]
    Err(DisplayError::Unsupported)
}
