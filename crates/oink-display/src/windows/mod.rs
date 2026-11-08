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

mod arrangement;
mod dpi;
mod edid;
mod enumerate;
mod visibility;
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
    visibility_root: Mutex<Option<std::path::PathBuf>>,
    handlers: watch::Handlers,
    watcher: Mutex<Option<watch::Watcher>>,
}

impl std::fmt::Debug for WinDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinDisplay").finish_non_exhaustive()
    }
}

impl DisplayBackend for WinDisplay {
    fn native_configuration(&self) -> Result<crate::arrangement::NativeConfiguration, DisplayError> {
        match self.visibility_root.lock().unwrap().as_deref() {
            Some(root) => visibility::configuration(root),
            None => arrangement::configuration(),
        }
    }
    fn apply_native_origins(
        &self,
        expected: &crate::arrangement::NativeConfiguration,
        origins: &[crate::arrangement::NativeOrigin],
    ) -> Result<crate::arrangement::NativeConfiguration, DisplayError> {
        match self.visibility_root.lock().unwrap().as_deref() {
            Some(root) => visibility::apply(root, expected, origins, &[]),
            None => arrangement::apply(expected, origins),
        }
    }
    fn initialize_visibility(&self, root: &std::path::Path) -> Result<(), DisplayError> {
        *self.visibility_root.lock().unwrap() = Some(root.to_owned());
        Ok(())
    }
    fn apply_native_layout(
        &self,
        expected: &crate::arrangement::NativeConfiguration,
        origins: &[crate::arrangement::NativeOrigin],
        disabled: &[(String, bool)],
    ) -> Result<crate::arrangement::NativeConfiguration, DisplayError> {
        let root = self.visibility_root.lock().unwrap();
        let root = root
            .as_deref()
            .ok_or_else(|| DisplayError::Platform("Visibility storage not initialized".into()))?;
        visibility::apply(root, expected, origins, disabled)
    }
    fn release_disabled_displays(&self) -> Result<(), DisplayError> {
        match self.visibility_root.lock().unwrap().as_deref() {
            Some(root) => visibility::release(root),
            None => Ok(()),
        }
    }
    fn reconnected_displays(&self) -> Result<Vec<String>, DisplayError> {
        match self.visibility_root.lock().unwrap().as_deref() {
            Some(root) => visibility::returned(root),
            None => Ok(Vec::new()),
        }
    }
    fn acknowledge_reconnected(&self, keys: &[String]) -> Result<(), DisplayError> {
        match self.visibility_root.lock().unwrap().as_deref() {
            Some(root) => visibility::acknowledge(root, keys),
            None => Ok(()),
        }
    }
    fn wallpapers(&self) -> Option<Vec<crate::wallpaper::DisplayWallpaper>> {
        wallpaper::read_all()
    }

    fn active_displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        enumerate::displays()
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        match self.visibility_root.lock().unwrap().as_deref() {
            Some(root) => visibility::displays(root),
            None => enumerate::displays(),
        }
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
