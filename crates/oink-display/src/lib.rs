//! 屏幕与布局。
//!
//! - [`DisplayBackend`]：枚举本机显示器、监听变化。平台实现在 `src/macos`、`src/windows`。
//! - [`layout`]: native display groups expanded into a shared reference canvas.
//!
//! The backend only reads OS display arrangements; it has no display-write API.

use std::sync::Arc;

pub use oink_foundation::display::DisplayInfo;

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
}

/// 显示器配置变化（插拔、分辨率、缩放、排列、旋转）时的回调，可能在任意线程调用。
pub type ChangeHandler = Arc<dyn Fn() + Send + Sync>;

pub trait DisplayBackend: Send + Sync {
    /// 当前所有显示器（含虚拟显示器，由 `is_virtual` 标出）。
    fn displays(&self) -> Result<Vec<DisplayInfo>, DisplayError>;
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
