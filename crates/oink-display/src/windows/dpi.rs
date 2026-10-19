//! Per-Monitor-V2 DPI 感知。
//!
//! 进程级：backend 创建时调用 `SetProcessDpiAwarenessContext(PMv2)`。如果宿主程序已经通过清单或 API
//! 设成了别的模式，这次调用会失败（进程级感知只能设置一次），这时退而在本模块自己的线程和调用里用
//! `SetThreadDpiAwarenessContext` 切到 PMv2，结果仍是物理像素。

use windows::Win32::Foundation::HANDLE;
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    GetDpiAwarenessContextForProcess, SetProcessDpiAwarenessContext, SetThreadDpiAwarenessContext,
};

/// 把进程设为 PMv2。已经是 PMv2 时返回 `Ok`；设成了别的模式时返回说明。
pub(crate) fn ensure_process_per_monitor_v2() -> Result<(), String> {
    // SAFETY: 只传入系统预定义的 DPI 上下文常量。
    unsafe {
        let set = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let current = GetDpiAwarenessContextForProcess(HANDLE::default());
        if AreDpiAwarenessContextsEqual(current, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).as_bool() {
            return Ok(());
        }
        Err(match set {
            Err(e) => format!(
                "无法把进程设为 Per-Monitor-V2 DPI 感知（{e}）：进程的 DPI 感知已被清单或更早的调用设置为其他模式；\
                 本模块会在自己的调用中临时切换线程 DPI 上下文，坐标仍为物理像素，但宿主程序的窗口可能被系统缩放"
            ),
            Ok(()) => "SetProcessDpiAwarenessContext 成功但进程仍不是 PMv2".to_string(),
        })
    }
}

/// 作用域内把当前线程切到 PMv2，离开时恢复。
#[derive(Debug)]
pub(crate) struct ThreadDpiGuard(DPI_AWARENESS_CONTEXT);

impl ThreadDpiGuard {
    pub(crate) fn per_monitor_v2() -> Self {
        // SAFETY: 只传入系统预定义的 DPI 上下文常量；返回旧值，在 Drop 中恢复。
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}

impl Drop for ThreadDpiGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: 恢复构造时取得的旧上下文。
            unsafe {
                SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
}
