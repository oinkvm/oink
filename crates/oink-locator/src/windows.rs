//! Windows：一个跟着拖尾范围移动的分层窗口（逐像素 alpha）。
//!
//! - 窗口：`WS_POPUP`，`WS_EX_LAYERED | WS_EX_TRANSPARENT`（鼠标穿透，不参与命中测试）`| WS_EX_NOACTIVATE |
//!   WS_EX_TOOLWINDOW | WS_EX_TOPMOST`。每帧 `UpdateLayeredWindow` 一次给出位置、大小与 BGRA 预乘像素：
//!   窗口只有拖尾那么大，跨同一台设备的多块显示器时照常绘制（坐标是 PMv2 物理像素，与 `DisplayInfo` 一致）。
//! - 像素：效果模型每格一个像素；格子是 1 个物理像素时直接复制到 DIB，大于 1 个（高分屏）时双线性放大
//!   （[`crate::effect::upscale_bilinear`]，与 macOS 图层的 linear 放大一致，不出现方块）。
//! - 帧率：`DwmFlush` 等到下一次合成，与屏幕刷新同步。
//! - 不画时销毁窗口：空闲线程不处理消息，留着顶层窗口会让广播消息（`SendMessage(HWND_BROADCAST)`）等它。

use std::time::{Duration, Instant};

use oink_foundation::display::DisplayInfo;
use windows::Win32::Foundation::{
    COLORREF, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC, HGDIOBJ, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, HWND_TOPMOST, MSG, PM_REMOVE, PeekMessageW,
    RegisterClassExW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, ShowWindow,
    TranslateMessage, ULW_ALPHA, UpdateLayeredWindow, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::w;

use crate::effect::CellRect;

pub(crate) fn available() -> bool {
    true
}

/// 本帧的内容（Windows 只有一个窗口，`surface` 总是 0）。
pub(crate) struct Layer {
    #[allow(dead_code)]
    pub surface: usize,
    pub rect: CellRect,
    pub pixels: Vec<u32>,
}

struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    w: i32,
    h: i32,
}

pub(crate) struct Backend {
    displays: Vec<DisplayInfo>,
    hwnd: Option<HWND>,
    dib: Option<Dib>,
}

impl Backend {
    pub(crate) fn new(_health: std::sync::Arc<crate::diagnostics::Health>) -> Option<Self> {
        // SAFETY: 只传入系统预定义的 DPI 上下文常量；本线程只做拖尾窗口。
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Some(Self {
            displays: Vec::new(),
            hwnd: None,
            dib: None,
        })
    }

    pub(crate) fn set_displays(&mut self, displays: &[DisplayInfo]) {
        self.displays = displays.to_vec();
    }

    /// The shared renderer composites arriving and departing effects on one grid,
    /// including the continuously changing handoff size. Clip the imported
    /// off-screen history before allocating a DIB (never clamp its path points).
    /// 一个窗口覆盖整个拖尾，限制在所有显示器的外接矩形内（避免巨大的窗口）。
    pub(crate) fn regions(&self, bounds: CellRect, cell: f64) -> Vec<(usize, CellRect)> {
        let r = match self.desktop(cell) {
            Some(d) => bounds.intersect(&d),
            None => return Vec::new(),
        };
        if r.is_empty() { Vec::new() } else { vec![(0, r)] }
    }

    fn desktop(&self, cell: f64) -> Option<CellRect> {
        let mut it = self.displays.iter().map(|d| crate::effect::cells_of(d.bounds, cell));
        let first = it.next()?;
        Some(it.fold(first, |a, b| {
            let x0 = a.x.min(b.x);
            let y0 = a.y.min(b.y);
            let x1 = (a.x + i64::from(a.w)).max(b.x + i64::from(b.w));
            let y1 = (a.y + i64::from(a.h)).max(b.y + i64::from(b.h));
            CellRect {
                x: x0,
                y: y0,
                w: (x1 - x0) as u32,
                h: (y1 - y0) as u32,
            }
        }))
    }

    pub(crate) fn present(&mut self, cell: f64, layers: Vec<Layer>) {
        let Some(l) = layers.into_iter().next() else {
            self.hide();
            return;
        };
        let k = cell.round().max(1.0) as i32;
        let (w, h) = (l.rect.w as i32 * k, l.rect.h as i32 * k);
        if w <= 0 || h <= 0 {
            return;
        }
        let Some(hwnd) = self.window() else { return };
        let Some(dib) = self.dib(w, h) else { return };
        // DIB 自上而下，行宽 dib.w。
        let stride = dib.w as usize;
        // SAFETY: bits 指向 dib.w × dib.h 个 u32，下面只写 w ≤ dib.w、h ≤ dib.h 的范围。
        let out = unsafe { std::slice::from_raw_parts_mut(dib.bits, stride * dib.h as usize) };
        let k = k as usize;
        crate::effect::upscale_bilinear(&l.pixels, l.rect.w as usize, l.rect.h as usize, k, out, stride);
        let pos = POINT {
            x: (l.rect.x * k as i64) as i32,
            y: (l.rect.y * k as i64) as i32,
        };
        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: 窗口与内存 DC 都是本线程创建、有效的；DIB 已选入 DC。
        unsafe {
            if let Err(e) = UpdateLayeredWindow(
                hwnd,
                None,
                Some(&pos),
                Some(&size),
                Some(dib.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            ) {
                tracing::debug!(%e, "UpdateLayeredWindow failed");
            }
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        pump();
    }

    /// 拖尾窗口（没有就创建，并放到最上层）。
    fn window(&mut self) -> Option<HWND> {
        if let Some(h) = self.hwnd {
            return Some(h);
        }
        // SAFETY: 标准的窗口类注册与窗口创建。
        let hwnd = unsafe {
            let hinstance = GetModuleHandleW(None).ok()?;
            let class = w!("OinkCursorTrail");
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(proc_),
                hInstance: hinstance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                tracing::warn!("cursor trail: RegisterClassExW failed");
                return None;
            }
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                class,
                w!("Oink cursor trail"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
            .ok()?;
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            hwnd
        };
        self.hwnd = Some(hwnd);
        Some(hwnd)
    }

    /// 至少 `w × h` 的 DIB（只增不减，按 256 取整）。
    fn dib(&mut self, w: i32, h: i32) -> Option<&Dib> {
        if self.dib.as_ref().is_some_and(|d| d.w >= w && d.h >= h) {
            return self.dib.as_ref();
        }
        let (nw, nh) = {
            let (ow, oh) = self.dib.as_ref().map_or((0, 0), |d| (d.w, d.h));
            (((w.max(ow) + 255) / 256) * 256, ((h.max(oh) + 255) / 256) * 256)
        };
        self.free_dib();
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: nw,
                biHeight: -nh,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: 标准的内存 DC 与 DIB 创建；失败时释放已创建的对象。
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let mut bits = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(b) if !bits.is_null() => b,
                _ => {
                    let _ = DeleteDC(dc);
                    tracing::warn!(nw, nh, "cursor trail: CreateDIBSection failed");
                    return None;
                }
            };
            let old = SelectObject(dc, bitmap.into());
            self.dib = Some(Dib {
                dc,
                bitmap,
                old,
                bits: bits.cast(),
                w: nw,
                h: nh,
            });
        }
        self.dib.as_ref()
    }

    fn free_dib(&mut self) {
        if let Some(d) = self.dib.take() {
            // SAFETY: 本线程创建的对象，先把原来的位图选回再删除。
            unsafe {
                SelectObject(d.dc, d.old);
                let _ = DeleteObject(d.bitmap.into());
                let _ = DeleteDC(d.dc);
            }
        }
    }

    pub(crate) fn hide(&mut self) {
        if let Some(h) = self.hwnd.take() {
            // SAFETY: 本线程创建的窗口。
            unsafe {
                let _ = DestroyWindow(h);
            }
            pump();
        }
        // 大的 DIB 不留着。
        self.free_dib();
    }

    pub(crate) fn wait_frame(&mut self, frame_start: Instant) {
        // DwmFlush 等到下一次桌面合成（与刷新率同步）；合成关闭或失败时按 60 Hz。
        // SAFETY: 无参数调用。
        let ok = unsafe { DwmFlush() }.is_ok();
        let min = Duration::from_millis(2);
        let elapsed = frame_start.elapsed();
        if !ok || elapsed < min {
            let target = crate::DEFAULT_FRAME;
            if elapsed < target {
                std::thread::sleep(target - elapsed);
            }
        }
        pump();
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.hide();
    }
}

/// 处理本线程的窗口消息（不阻塞）。
fn pump() {
    let mut msg = MSG::default();
    // SAFETY: 标准消息循环。
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: 系统调用的窗口过程，参数原样交给默认处理。
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
