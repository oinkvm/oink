//! Windows 实现：EnumDisplayMonitors + QueryDisplayConfig（PMv2）、EDID 物理尺寸、WM_DISPLAYCHANGE。
//!
//! - 坐标：进程（或至少本模块的调用线程）为 Per-Monitor-V2 DPI 感知，`bounds` 是虚拟桌面上的物理像素。
//! - `key`：目标显示器的设备接口路径（`\\?\DISPLAY#GSM5B09#...#{e6f07b5f-...}`），重启、重新插拔到
//!   同一接口后不变；读不到时退回 GDI 名（`\\.\DISPLAY1`，不稳定）。
//! - 物理尺寸：EDID（SetupAPI → `Device Parameters\EDID`，失败时直接读注册表），优先详细时序描述符里的毫米值，
//!   按当前旋转交换宽高。
//! - 内建屏：输出技术为 INTERNAL / DISPLAYPORT_EMBEDDED / UDI_EMBEDDED / LVDS。
//! - 虚拟屏：输出技术为 INDIRECT_VIRTUAL，或设备/适配器路径里有已知虚拟显示驱动的标识，
//!   或是软件枚举（ROOT\、SWD\）适配器上的间接显示器。
//! - 镜像组视为一块屏幕（GDI 只有一个 HMONITOR）。

mod dpi;
mod edid;
mod enumerate;
mod wallpaper;
mod watch;

use std::sync::{Arc, Mutex};

use oink_foundation::display::DisplayInfo;

use crate::{ChangeHandler, DisplayBackend, DisplayError};

pub(crate) fn backend() -> Result<Box<dyn DisplayBackend>, DisplayError> {
    if let Err(msg) = dpi::ensure_process_per_monitor_v2() {
        tracing::warn!("{msg}");
    }
    Ok(Box::new(WinDisplay::default()))
}

#[derive(Default)]
struct WinDisplay {
    handlers: watch::Handlers,
    watcher: Mutex<Option<watch::Watcher>>,
}

impl std::fmt::Debug for WinDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinDisplay").finish_non_exhaustive()
    }
}

impl DisplayBackend for WinDisplay {
    fn wallpapers(&self) -> Option<Vec<crate::wallpaper::DisplayWallpaper>> {
        wallpaper::read_all()
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        enumerate::displays()
    }

    fn watch(&self, on_change: ChangeHandler) -> Result<(), DisplayError> {
        self.handlers
            .lock()
            .map_err(|_| DisplayError::Platform("回调列表锁已损坏".into()))?
            .push(on_change);
        let mut watcher = self
            .watcher
            .lock()
            .map_err(|_| DisplayError::Platform("监听线程锁已损坏".into()))?;
        if watcher.is_none() {
            *watcher = Some(watch::Watcher::spawn(Arc::clone(&self.handlers))?);
        }
        Ok(())
    }
}
