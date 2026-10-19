//! Keep the held OLE destination away from appbars, even while they are hidden.
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::UI::Shell::{ABM_GETAUTOHIDEBAREX, APPBARDATA, SHAppBarMessage};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, GetAncestor, GetClassNameW, GetWindowLongPtrW, GetWindowRect,
    IsWindowVisible, WS_DISABLED, WS_EX_LAYERED, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WindowFromPoint,
};
use windows::core::BOOL;

mod geometry;
use geometry::{Monitor, Rect};

fn rect(r: RECT) -> Rect {
    Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

pub(super) fn desktop(surface: usize) -> Option<(RECT, POINT)> {
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: EnumDisplayMonitors supplies a live HMONITOR. The callback is
        // synchronous and data is our Vec. All remaining calls only query geometry.
        unsafe {
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return BOOL(0);
            }
            let mut autohide = [0; 4];
            for (edge, thickness) in autohide.iter_mut().enumerate() {
                let mut bar = APPBARDATA {
                    cbSize: std::mem::size_of::<APPBARDATA>() as u32,
                    uEdge: edge as u32,
                    rc: info.rcMonitor,
                    ..Default::default()
                };
                let hwnd = HWND(SHAppBarMessage(ABM_GETAUTOHIDEBAREX, &mut bar) as _);
                if !hwnd.is_invalid() {
                    let mut bounds = RECT::default();
                    if GetWindowRect(hwnd, &mut bounds).is_err() {
                        return BOOL(0);
                    }
                    // rcWork excludes ordinary appbars, but not auto-hide bars.
                    // Their HWND can sit outside the display; reserve the full
                    // bar thickness at its registered edge, not its visible strip.
                    *thickness = if edge % 2 == 0 {
                        bounds.right - bounds.left
                    } else {
                        bounds.bottom - bounds.top
                    }
                    .max(1);
                }
            }
            (&mut *(data.0 as *mut Vec<Monitor>)).push(Monitor {
                bounds: rect(info.rcMonitor),
                work: rect(info.rcWork),
                autohide,
            });
        }
        BOOL(1)
    }
    struct Surfaces {
        ours: usize,
        obstacles: Vec<Rect>,
    }
    unsafe extern "system" fn occluder(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: EnumWindows invokes synchronously with a live callback context.
        // Query visible, input-bearing topmost rectangles, without changing z-order.
        unsafe {
            let surfaces = &mut *(data.0 as *mut Surfaces);
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            if hwnd.0 as usize == surfaces.ours
                || !IsWindowVisible(hwnd).as_bool()
                || GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_DISABLED.0 != 0
                || ex & WS_EX_TOPMOST.0 == 0
                || ex & (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0) == (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0)
            {
                return BOOL(1);
            }
            let mut cloaked = 0_u32;
            let _ = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                (&mut cloaked as *mut u32).cast(),
                std::mem::size_of::<u32>() as u32,
            );
            if cloaked == 0 {
                let mut bounds = RECT::default();
                if GetWindowRect(hwnd, &mut bounds).is_ok() {
                    surfaces.obstacles.push(rect(bounds));
                }
            }
        }
        BOOL(1)
    }
    let mut monitors = Vec::<Monitor>::new();
    let mut surfaces = Surfaces {
        ours: surface,
        obstacles: Vec::new(),
    };
    // SAFETY: both callbacks and their mutable contexts live for these synchronous calls.
    unsafe {
        EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut monitors as *mut _ as isize))
            .ok()
            .ok()?;
        EnumWindows(Some(occluder), LPARAM(&mut surfaces as *mut _ as isize)).ok()?;
    }
    let (bounds, (x, y)) = geometry::select(&monitors, &surfaces.obstacles)?;
    Some((
        RECT {
            left: bounds.left,
            top: bounds.top,
            right: bounds.right,
            bottom: bounds.bottom,
        },
        POINT { x, y },
    ))
}

/// Fixed diagnostics enum: 0=our catcher, 1=shell taskbar (including its children),
/// 2=other/no window. Never log arbitrary window classes, titles, or process names.
pub(super) fn window_kind_at(point: POINT, surface: usize) -> u64 {
    // SAFETY: read-only hit test/class queries, on the DPI-aware native drag STA.
    unsafe {
        let hit = WindowFromPoint(point);
        if hit.0 as usize == surface {
            return 0;
        }
        for hwnd in [hit, GetAncestor(hit, GA_ROOT)] {
            let mut class = [0_u16; 256];
            let len = GetClassNameW(hwnd, &mut class) as usize;
            if matches!(
                String::from_utf16_lossy(&class[..len]).as_str(),
                "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "TrayShowDesktopButtonWClass" | "TrayNotifyWnd"
            ) {
                return 1;
            }
        }
        2
    }
}
