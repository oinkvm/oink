//! One small layered window per visible projection; no full-desktop input window.

use oink_foundation::Rect;
use std::{cell::RefCell, collections::HashMap, sync::atomic::Ordering};
use windows::Win32::Foundation::{
    COLORREF, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC, HGDIOBJ, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, HWND_TOPMOST,
    MA_NOACTIVATE, MSG, PM_REMOVE, PeekMessageW, RegisterClassExW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SetWindowPos, ShowWindow, TranslateMessage, ULW_ALPHA, UpdateLayeredWindow, WM_CAPTURECHANGED,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::w;

use super::{DRAGGING, DragPhase, Event, Frame, NativeHandle, emit};
thread_local! { static HANDLES: RefCell<HashMap<usize, (NativeHandle, Option<f64>)>> = RefCell::new(HashMap::new()); }

struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    w: usize,
    h: usize,
}
impl Dib {
    fn new(w: usize, h: usize) -> Option<Self> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: Standard memory DC and top-down premultiplied BGRA DIB. Failed
        // allocation releases the DC; successful ownership is paired in Drop.
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
                    return None;
                }
            };
            let old = SelectObject(dc, bitmap.into());
            Some(Self {
                dc,
                bitmap,
                old,
                bits: bits.cast(),
                w,
                h,
            })
        }
    }
}
impl Drop for Dib {
    fn drop(&mut self) {
        // SAFETY: Restore the original object before destroying our selected bitmap.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
struct Window {
    hwnd: HWND,
    dib: Option<Dib>,
}
impl Window {
    fn new() -> Option<Self> {
        Self::create(false)
    }
    fn create(handle: bool) -> Option<Self> {
        // SAFETY: Registered process-local class; this renderer owns the HWND on
        // its message-pumping thread. No focus, capture or cursor APIs are called.
        unsafe {
            let module = GetModuleHandleW(None).ok()?;
            let class = if handle {
                w!("OinkLayoutProjectionHandle")
            } else {
                w!("OinkLayoutProjection")
            };
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(if handle { handle_proc } else { proc_ }),
                hInstance: module.into(),
                lpszClassName: class,
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return None;
            }
            // TOOLWINDOW removes taskbar / Alt+Tab entries, NOACTIVATE prevents
            // focus changes, TRANSPARENT gives complete click-through outside
            // edit mode. Layered premultiplied alpha preserves the clipped ends.
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | if handle { Default::default() } else { WS_EX_TRANSPARENT },
                class,
                w!("Oink layout projection"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(module.into()),
                None,
            )
            .ok()?;
            Some(Self { hwnd, dib: None })
        }
    }
    fn present(&mut self, f: Frame) {
        if self.dib.as_ref().is_none_or(|d| d.w != f.width || d.h != f.height) {
            self.dib = Dib::new(f.width, f.height);
        }
        let Some(dib) = &self.dib else { return };
        let Rect { x, y, .. } = f.bounds;
        let pos = POINT {
            x: x.round() as i32,
            y: y.round() as i32,
        };
        let size = SIZE {
            cx: f.width as i32,
            cy: f.height as i32,
        };
        let src = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: This thread owns HWND/DC and the DIB has exactly width*height
        // initialized pixels. Show and reordering explicitly forbid activation.
        unsafe {
            std::ptr::copy_nonoverlapping(f.pixels.as_ptr(), dib.bits, f.pixels.len());
            if let Err(e) = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&pos),
                Some(&size),
                Some(dib.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            ) {
                tracing::warn!(%e, "layout projection upload failed");
            }
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            // Cursor parking relies on its cover being the top window at the
            // park point. Keep projection pixels below that window when present.
            let after = FindWindowW(w!("OinkCursorCover"), None)
                .ok()
                .filter(|h| !h.is_invalid())
                .unwrap_or(HWND_TOPMOST);
            let _ = SetWindowPos(
                self.hwnd,
                Some(after),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        HANDLES.with(|h| {
            h.borrow_mut().remove(&(self.hwnd.0 as usize));
        });
        // SAFETY: HWND created and destroyed on the same renderer thread.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
pub(super) struct Backend {
    windows: Vec<Window>,
    handles: Vec<Window>,
}
impl Backend {
    pub(super) fn new(_health: std::sync::Arc<crate::diagnostics::Health>) -> Option<Self> {
        // SAFETY: Only this renderer thread changes DPI context; layout bounds
        // and native layered positions are both PMv2 physical pixels.
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Some(Self {
            windows: Vec::new(),
            handles: Vec::new(),
        })
    }
    pub(super) fn present(&mut self, frames: Vec<Frame>, editable: bool) {
        // Losing the visible overlap during a genuine captured drag must not
        // destroy its capture HWND. Explicit hide/expiry cancels immediately.
        if !editable || !DRAGGING.load(Ordering::Acquire) {
            self.handles.clear();
            for handle in frames.iter().flat_map(|f| f.handles.iter()) {
                let Some(mut window) = Window::create(true) else {
                    continue;
                };
                HANDLES.with(|h| h.borrow_mut().insert(window.hwnd.0 as usize, (handle.clone(), None)));
                let r = handle.hit.rect;
                let (width, height) = (r.w.ceil() as usize, r.h.ceil() as usize);
                // Alpha=1 makes exactly the small handle hit-testable on layered
                // windows. It cannot create a full-screen invisible mouse barrier.
                window.present(Frame {
                    bounds: r,
                    width,
                    height,
                    pixels: vec![0x01000000; width * height],
                    handles: vec![],
                });
                self.handles.push(window);
            }
            if frames.is_empty() {
                DRAGGING.store(false, Ordering::Release);
            }
        }
        self.windows.truncate(frames.len());
        while self.windows.len() < frames.len() {
            let Some(w) = Window::new() else { return };
            self.windows.push(w);
        }
        for (w, f) in self.windows.iter_mut().zip(frames) {
            w.present(f);
        }
    }
    pub(super) fn hide(&mut self) {
        self.windows.clear();
        self.handles.clear();
        DRAGGING.store(false, Ordering::Release);
    }
    pub(super) fn pump(&mut self) {
        // SAFETY: Drain only this thread's native window queue.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    pub(super) fn escape_down(&self) -> bool {
        // SAFETY: Passive desktop key-state query, no focus or hook changes.
        unsafe { GetAsyncKeyState(i32::from(VK_ESCAPE.0)) < 0 }
    }
}
unsafe extern "system" fn handle_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    if matches!(msg, WM_LBUTTONDOWN | WM_MOUSEMOVE | WM_LBUTTONUP | WM_CAPTURECHANGED) {
        let mut p = POINT::default();
        // SAFETY: Initialized output point; read-only current native position.
        let ok = unsafe { GetCursorPos(&mut p) }.is_ok();
        if ok {
            let event = HANDLES.with(|h| {
                let mut h = h.borrow_mut();
                let (handle, start) = h.get_mut(&(hwnd.0 as usize))?;
                let at = if handle.projection.side.is_vertical() {
                    f64::from(p.y)
                } else {
                    f64::from(p.x)
                };
                let phase = match msg {
                    WM_LBUTTONDOWN => {
                        *start = Some(at);
                        Some(DragPhase::Begin)
                    }
                    WM_MOUSEMOVE if start.is_some() => Some(DragPhase::Update),
                    WM_LBUTTONUP | WM_CAPTURECHANGED if start.is_some() => Some(DragPhase::End),
                    _ => None,
                }?;
                let delta = at - (*start)?;
                if phase == DragPhase::End {
                    *start = None;
                }
                Some(Event::Drag {
                    projection: Box::new(handle.projection.clone()),
                    handle: handle.hit.handle,
                    delta,
                    phase,
                })
            });
            if let Some(event) = event {
                if let Event::Drag { phase, .. } = &event {
                    DRAGGING.store(*phase != DragPhase::End, Ordering::Release);
                }
                emit(event);
                // SAFETY: Capture is confined to a genuine handle down/drag/up;
                // no cursor visibility/position/park/cover state is touched.
                unsafe {
                    if msg == WM_LBUTTONDOWN {
                        SetCapture(hwnd);
                    }
                    if msg == WM_LBUTTONUP {
                        let _ = ReleaseCapture();
                    }
                }
            }
        }
        return LRESULT(0);
    }
    // SAFETY: All other messages use the normal non-activating window procedure.
    unsafe { proc_(hwnd, msg, wp, lp) }
}
unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    // SAFETY: Unhandled window messages preserve standard processing.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

// PMv2 desktop bounds are physical pixels. Convert the design's UI points with
// the host monitor's effective DPI, independently for each mixed-DPI display.
pub(super) fn annotation_unit(screen: Rect) -> f64 {
    use windows::Win32::{
        Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint},
        UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
    };
    let mut x = 96;
    let mut y = 96;
    // SAFETY: read-only monitor lookup and output parameters owned by this call.
    unsafe {
        let monitor = MonitorFromPoint(
            POINT {
                x: screen.center().x as i32,
                y: screen.center().y as i32,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y);
    }
    f64::from(x) / 96.0
}
