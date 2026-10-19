//! Main-window chrome only: WebView2 draws the silhouette; Win32 owns Snap.
use std::{cell::Cell, sync::Once};

use tauri::WebviewWindow;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::{
            Dwm::{
                DWMNCRP_DISABLED, DWMWA_BORDER_COLOR, DWMWA_NCRENDERING_POLICY, DWMWA_WINDOW_CORNER_PREFERENCE,
                DWMWCP_DONOTROUND, DwmSetWindowAttribute,
            },
            Gdi::HBRUSH,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent},
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

const SUBCLASS: usize = 0x4f494e4b;
static REGISTER: Once = Once::new();
struct SnapState {
    window: WebviewWindow,
    hovered: Cell<bool>,
    pressed: Cell<bool>,
}

pub fn prepare(window: &WebviewWindow) -> tauri::Result<()> {
    let hwnd = HWND(window.hwnd()?.0);
    // SAFETY: live HWND on its owning UI thread.
    unsafe {
        REGISTER.call_once(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(snap_proc),
                hInstance: GetModuleHandleW(None).unwrap().into(),
                lpszClassName: w!("OinkMainSnapButton"),
                hbrBackground: HBRUSH::default(),
                ..Default::default()
            };
            RegisterClassW(&class);
        });
        // WebView2's rendering child consumes hit tests before the Tauri HWND.
        // A nonpainting sibling over ONLY the maximize button supplies the
        // HTMAXBUTTON required by Windows 11 Snap. WS_CLIPSIBLINGS keeps WebView2
        // from painting over it; no layered/transparent extended style (those
        // would pass the hit test back into WebView2). The HTML stays visible.
        let overlay = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("OinkMainSnapButton"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            0,
            0,
            0,
            0,
            Some(hwnd),
            None,
            Some(GetModuleHandleW(None).map_err(native_error)?.into()),
            None,
        )
        .map_err(native_error)?;
        let state = Box::into_raw(Box::new(SnapState {
            window: window.clone(),
            hovered: Cell::new(false),
            pressed: Cell::new(false),
        }));
        SetWindowLongPtrW(overlay, GWLP_USERDATA, state as isize);
        if !SetWindowSubclass(hwnd, Some(parent_proc), SUBCLASS, overlay.0 as usize).as_bool() {
            let _ = DestroyWindow(overlay);
            return Err(native_error(windows::core::Error::from_thread()));
        }
        remove_native_chrome(hwnd);
        // Windows 11 otherwise adds native rounded corners and a one-pixel
        // border. Windows 10 rejects these two Win11 attributes (best effort).
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            std::ptr::from_ref(&DWMWCP_DONOTROUND).cast(),
            4,
        );
        let none = 0xfffffffe_u32; // DWMWA_COLOR_NONE
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, (&none as *const u32).cast(), 4);
        // Disabling DWM's nonclient renderer does not disable classic Win32
        // frame painting. Install the subclass and remove the caption style
        // BEFORE changing this policy; the resize/minimize/maximize/system-menu
        // styles remain for native window behaviour. DWM composition stays on
        // so WebView2's alpha backing remains transparent. Verified on Windows
        // 11 at 200% DPI through welcome/main focus transitions, native Snap,
        // drag/resize and maximize/restore. Windows 10 remains unverified.
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY,
            std::ptr::from_ref(&DWMNCRP_DISABLED).cast(),
            4,
        );
        // Recalculate the client area before first show; changing DWM policy
        // does not itself discard the previous frame metrics.
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
        .map_err(native_error)?;
        position_overlay(hwnd, overlay);
    }
    Ok(())
}

unsafe fn remove_native_chrome(hwnd: HWND) {
    // SAFETY: the main HWND belongs to this thread; the subclass is installed
    // first so subsequent Tao style rewrites receive the same filtering.
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & !WS_CAPTION.0;
        SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
    }
}

unsafe fn position_overlay(parent: HWND, overlay: HWND) {
    // SAFETY: both windows belong to this UI thread. CSS uses 46px buttons and
    // a 48px toolbar; the floating shadow gutter is 24px, zero when maximized.
    unsafe {
        let scale = GetDpiForWindow(parent) as f64 / 96.0;
        let px = |n: f64| (n * scale).round() as i32;
        let mut rect = RECT::default();
        if GetClientRect(parent, &mut rect).is_ok() {
            let gutter = if IsZoomed(parent).as_bool() { 0 } else { px(24.0) };
            let _ = SetWindowPos(
                overlay,
                Some(HWND_TOP),
                rect.right - gutter - px(92.0),
                gutter,
                px(46.0),
                px(48.0),
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }
}

unsafe extern "system" fn parent_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _: usize, data: usize) -> LRESULT {
    // SAFETY: common-controls forwards the owning window's messages and our
    // overlay remains a child until its parent's normal destruction.
    unsafe {
        match msg {
            // Tao keeps WS_CAPTION even for undecorated top-level windows,
            // then restores it on show/restore. Suppressing
            // WM_NCPAINT/NCACTIVATE alone cannot cover direct DefWindowProc
            // painting (Tao's theme refresh calls it directly). A client repaint
            // can temporarily erase those pixels; refocusing does not remove
            // their caption style. WS_CAPTION
            // includes WS_BORDER and WS_DLGFRAME: retaining it also leaves an
            // extra native rim outside the web shell. Remove these visual
            // style bits at their source, including each later rewrite.
            // WS_THICKFRAME/MINIMIZEBOX/MAXIMIZEBOX/SYSMENU remain for native
            // resize, caption drag, taskbar commands and Windows 11 Snap.
            WM_STYLECHANGING if wp.0 as i32 == GWL_STYLE.0 => {
                let styles = &mut *(lp.0 as *mut STYLESTRUCT);
                styles.styleNew &= !WS_CAPTION.0;
            }
            // Tao handles the NCCALCSIZE_PARAMS form, including maximized work
            // area and auto-hide taskbar insets. Its plain RECT form falls
            // through to DefWindowProc, which would restore caption insets.
            WM_NCCALCSIZE if wp.0 == 0 => return LRESULT(0),
            // DWMNCRP_DISABLED does not disable classic Win32 frame painting.
            // WebView2 owns every pixel; there is no native frame to repaint.
            WM_NCPAINT => return LRESULT(0),
            // Still notify Tao of activation/focus changes. The documented -1
            // lParam tells DefWindowProc to skip only its nonclient repaint.
            WM_NCACTIVATE => return DefSubclassProc(hwnd, msg, wp, LPARAM(-1)),
            _ => {}
        }
        let result = DefSubclassProc(hwnd, msg, wp, lp);
        // Tao hit-tests the native frame, including the 24-point web shadow
        // gutter. The HTML targets already start native resize gestures at the
        // visible edge; veto only these extra outer-frame targets. Keep the
        // style bits for Snap/maximize and preserve all other nonclient hits.
        // HTTRANSPARENT only passes through windows on this thread, so it does
        // not make the WebView2 shadow click-through to another application.
        if msg == WM_NCHITTEST
            && matches!(
                result.0 as u32,
                HTLEFT | HTRIGHT | HTTOP | HTBOTTOM | HTTOPLEFT | HTTOPRIGHT | HTBOTTOMLEFT | HTBOTTOMRIGHT
            )
        {
            return LRESULT(HTNOWHERE as isize);
        }
        if msg == WM_SIZE || msg == WM_DPICHANGED {
            position_overlay(hwnd, HWND(data as *mut _));
        }
        result
    }
}

unsafe fn hover(hwnd: HWND, value: bool) {
    // SAFETY: GWLP_USERDATA owns one Box<SnapState>, released at NCDESTROY.
    unsafe {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SnapState;
        if let Some(state) = state.as_ref() {
            if state.hovered.replace(value) == value {
                return;
            }
            let script = if value {
                "document.querySelector('[data-window-maximize]')?.setAttribute('data-snap-hover','')"
            } else {
                "document.querySelector('[data-window-maximize]')?.removeAttribute('data-snap-hover')"
            };
            let _ = state.window.eval(script);
        }
    }
}

unsafe extern "system" fn snap_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: Win32 supplies live handles, and all calls stay on their UI thread.
    unsafe {
        match msg {
            WM_NCHITTEST => return LRESULT(HTMAXBUTTON as isize),
            WM_ERASEBKGND => return LRESULT(1),
            WM_NCMOUSEMOVE => {
                hover(hwnd, true);
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE | TME_NONCLIENT,
                    hwndTrack: hwnd,
                    ..Default::default()
                };
                let _ = TrackMouseEvent(&mut track);
            }
            WM_NCMOUSELEAVE => hover(hwnd, false),
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK => {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SnapState;
                if let Some(state) = state.as_ref() {
                    state.pressed.set(true);
                }
                return LRESULT(0);
            }
            WM_NCLBUTTONUP => {
                // Only complete a click released on the original button. Use
                // WM_SYSCOMMAND so maximize/restore follows the system path.
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SnapState;
                if !state.as_ref().is_some_and(|state| state.pressed.replace(false)) {
                    return LRESULT(0);
                }
                let mut rect = RECT::default();
                let _ = GetWindowRect(hwnd, &mut rect);
                let x = lp.0 as i16 as i32;
                let y = (lp.0 >> 16) as i16 as i32;
                if x >= rect.left
                    && x < rect.right
                    && y >= rect.top
                    && y < rect.bottom
                    && let Ok(parent) = GetParent(hwnd)
                {
                    let command = if IsZoomed(parent).as_bool() {
                        SC_RESTORE
                    } else {
                        SC_MAXIMIZE
                    };
                    let _ = PostMessageW(Some(parent), WM_SYSCOMMAND, WPARAM(command as usize), LPARAM(0));
                }
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SnapState;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if !state.is_null() {
                    drop(Box::from_raw(state));
                }
            }
            _ => {}
        }
        // DefWindowProc handles native nonclient hover (the actual Snap menu).
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

fn native_error(error: windows::core::Error) -> tauri::Error {
    std::io::Error::other(error).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Forwarded {
        paint: bool,
        activate: Option<isize>,
        calculate: Option<usize>,
        hit: Option<i32>,
    }

    unsafe extern "system" fn observe(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _: usize, data: usize) -> LRESULT {
        // SAFETY: the test keeps Forwarded alive until this hidden HWND dies;
        // SendMessage and both subclasses run synchronously on this thread.
        unsafe {
            let forwarded = &mut *(data as *mut Forwarded);
            match msg {
                WM_NCHITTEST => {
                    if let Some(hit) = forwarded.hit {
                        return LRESULT(hit as isize);
                    }
                }
                WM_NCPAINT => forwarded.paint = true,
                WM_NCACTIVATE => {
                    forwarded.activate = Some(lp.0);
                    return LRESULT(1);
                }
                WM_NCCALCSIZE => {
                    forwarded.calculate = Some(wp.0);
                    // Stand in for Tao's structured maximize/work-area sizing.
                    if wp.0 != 0 {
                        let params = &mut *(lp.0 as *mut NCCALCSIZE_PARAMS);
                        params.rgrc[0].bottom -= 1;
                    }
                    return LRESULT(0);
                }
                _ => {}
            }
            DefSubclassProc(hwnd, msg, wp, lp)
        }
    }

    #[test]
    fn frameless_messages_skip_paint_but_preserve_activation_and_work_area_layout() {
        struct HiddenWindow(HWND);
        impl Drop for HiddenWindow {
            fn drop(&mut self) {
                // SAFETY: the hidden window belongs to this test thread.
                unsafe { DestroyWindow(self.0).unwrap() };
            }
        }

        let mut forwarded = Forwarded::default();
        // SAFETY: create an invisible Win32 window, never focus it or send input.
        // Forwarded outlives the window and the observers only borrow it during
        // synchronous SendMessage calls on this thread.
        unsafe {
            let window = HiddenWindow(
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    w!(""),
                    WS_OVERLAPPEDWINDOW,
                    0,
                    0,
                    1000,
                    700,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap(),
            );
            let hwnd = window.0;
            assert!(
                SetWindowSubclass(
                    hwnd,
                    Some(observe),
                    SUBCLASS + 1,
                    std::ptr::from_mut(&mut forwarded) as usize
                )
                .as_bool()
            );
            assert!(SetWindowSubclass(hwnd, Some(parent_proc), SUBCLASS, 0).as_bool());
            remove_native_chrome(hwnd);
            let native_behaviour = WS_THICKFRAME | WS_MAXIMIZEBOX | WS_MINIMIZEBOX | WS_SYSMENU;
            assert_eq!(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_CAPTION.0, 0);
            // A visibility/restore style rewrite must not bring the caption
            // back. Other behaviour bits survive intact.
            SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_OVERLAPPEDWINDOW | WS_DISABLED).0 as isize);
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            assert_eq!(style & WS_CAPTION.0, 0);
            assert_eq!(style & native_behaviour.0, native_behaviour.0);
            assert_ne!(style & WS_DISABLED.0, 0);
            // Tao still reports resize hits at the native frame, 24 CSS points
            // outside the silhouette. Only the inner HTML targets may resize.
            for hit in [
                HTLEFT,
                HTRIGHT,
                HTTOP,
                HTBOTTOM,
                HTTOPLEFT,
                HTTOPRIGHT,
                HTBOTTOMLEFT,
                HTBOTTOMRIGHT,
            ] {
                forwarded.hit = Some(hit as i32);
                assert_eq!(SendMessageW(hwnd, WM_NCHITTEST, None, None).0, HTNOWHERE as isize);
            }
            for hit in [HTCLIENT, HTCAPTION, HTMAXBUTTON] {
                forwarded.hit = Some(hit as i32);
                assert_eq!(SendMessageW(hwnd, WM_NCHITTEST, None, None).0, hit as isize);
            }
            forwarded.hit = None;
            SendMessageW(hwnd, WM_NCPAINT, Some(WPARAM(1)), Some(LPARAM(0)));
            for active in [0, 1] {
                SendMessageW(hwnd, WM_NCACTIVATE, Some(WPARAM(active)), Some(LPARAM(0)));
                assert_eq!(forwarded.activate.take(), Some(-1));
            }
            let mut rect = RECT {
                left: -1200,
                top: 20,
                right: -200,
                bottom: 720,
            };
            SendMessageW(
                hwnd,
                WM_NCCALCSIZE,
                Some(WPARAM(0)),
                Some(LPARAM(std::ptr::from_mut(&mut rect) as isize)),
            );
            assert_eq!((rect.left, rect.top, rect.right, rect.bottom), (-1200, 20, -200, 720));
            assert!(!forwarded.paint);
            assert_eq!(forwarded.calculate, None);
            let mut params = NCCALCSIZE_PARAMS::default();
            params.rgrc[0] = rect;
            SendMessageW(
                hwnd,
                WM_NCCALCSIZE,
                Some(WPARAM(1)),
                Some(LPARAM(std::ptr::from_mut(&mut params) as isize)),
            );
            assert_eq!(forwarded.calculate, Some(1));
            assert_eq!(params.rgrc[0].bottom, 719);
        }
    }
}
