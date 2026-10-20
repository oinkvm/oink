//! Small per-monitor layered toast windows. Never activate or capture input.
use super::{Click, Frame, Surface};
use oink_foundation::{Point, Rect};
use std::{cell::RefCell, collections::HashMap};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{HiDpi::*, WindowsAndMessaging::*},
    },
    core::{BOOL, w},
};
type Hit = (u64, Option<Rect>, bool);
thread_local! {static HITS:RefCell<HashMap<usize,Hit>>=RefCell::new(HashMap::new());static CLICKS:RefCell<Vec<Click>>=const{RefCell::new(Vec::new())};}
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
        Self::create()
    }
    fn create() -> Option<Self> {
        // SAFETY: Registered process-local class; this renderer owns the HWND on
        // its message-pumping thread. No focus, capture or cursor APIs are called.
        unsafe {
            let module = GetModuleHandleW(None).ok()?;
            let class = w!("OinkNotification");
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(proc_),
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
                WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                class,
                w!("Oink notification"),
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
        HITS.with(|h| {
            h.borrow_mut()
                .insert(self.hwnd.0 as usize, (f.id, f.undo, f.interactive));
        });
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
        HITS.with(|h| {
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
}
impl Backend {
    pub fn new() -> Option<Self> {
        // SAFETY: Only the notification worker changes its own DPI context. Bounds and
        // uploads remain physical pixels; sizes use each target monitor's scale.
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Some(Self { windows: Vec::new() })
    }
}
impl Surface for Backend {
    fn present(&mut self, frames: Vec<Frame>) {
        self.windows.truncate(frames.len());
        while self.windows.len() < frames.len() {
            let Some(w) = Window::new() else { return };
            self.windows.push(w);
        }
        for (w, f) in self.windows.iter_mut().zip(frames) {
            w.present(f);
        }
    }
    fn clicks(&mut self) -> Vec<Click> {
        // SAFETY: this worker owns and dispatches only its own HWND message queue.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        CLICKS.with(|c| std::mem::take(&mut *c.borrow_mut()))
    }
    fn reduced_motion(&self) -> bool {
        let mut animation = BOOL(0);
        // SAFETY: read-only SPI query writes one initialized BOOL; no setting is changed.
        unsafe {
            let _ = SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                Some((&mut animation as *mut BOOL).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
        }
        !animation.as_bool()
    }
}
unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    if msg == WM_LBUTTONUP {
        HITS.with(|h| {
            if let Some((id, undo, true)) = h.borrow().get(&(hwnd.0 as usize)) {
                let x = (l.0 as u16 as i16) as f64;
                let y = ((l.0 >> 16) as u16 as i16) as f64;
                CLICKS.with(|c| {
                    c.borrow_mut().push(Click {
                        id: *id,
                        undo: undo.is_some_and(|r| r.contains(Point::new(x, y))),
                    })
                });
            }
        });
        return LRESULT(0);
    }
    // SAFETY: unhandled message and its original parameters are passed to Windows.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

pub(super) fn scale_for(display: &oink_foundation::display::DisplayInfo) -> f64 {
    let point = POINT {
        x: (display.bounds.x + display.bounds.w / 2.).floor() as i32,
        y: (display.bounds.y + display.bounds.h / 2.).floor() as i32,
    };
    let mut x = 96;
    let mut y = 96;
    // SAFETY: passive monitor lookup and two valid DPI outputs, without input changes.
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y);
    }
    f64::from(x) / 96.
}
