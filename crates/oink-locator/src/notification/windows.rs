//! Small per-monitor layered toast windows. Never activate or capture input.
use super::{
    Click, Feedback, Frame, Surface,
    interaction::{HitState, Input},
};
use oink_foundation::{Point, Rect};
use std::{cell::RefCell, collections::HashMap};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::WM_MOUSELEAVE,
            HiDpi::*,
            Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
            WindowsAndMessaging::*,
        },
    },
    core::{BOOL, w},
};

thread_local! {
    static HITS: RefCell<HashMap<usize, HitState>> = RefCell::new(HashMap::new());
    static CLICKS: RefCell<Vec<Click>> = const { RefCell::new(Vec::new()) };
    static SCALES: RefCell<Vec<(u64, f64)>> = const { RefCell::new(Vec::new()) };
    static FEEDBACK: RefCell<Vec<Feedback>> = const { RefCell::new(Vec::new()) };
}
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
fn window_style(interactive: bool) -> WINDOW_EX_STYLE {
    WS_EX_LAYERED
        | WS_EX_NOACTIVATE
        | WS_EX_TOOLWINDOW
        | WS_EX_TOPMOST
        | if interactive {
            WINDOW_EX_STYLE(0)
        } else {
            WS_EX_TRANSPARENT
        }
}

impl Window {
    fn new() -> Option<Self> {
        Self::create()
    }
    fn create() -> Option<Self> {
        let _dpi = DpiContext::enter()?;
        // SAFETY: Registered process-local class; this renderer owns the HWND on
        // its message-pumping thread. The shared system cursor is not owned here.
        unsafe {
            let module = GetModuleHandleW(None).ok()?;
            let class = w!("OinkNotification");
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(proc_),
                hInstance: module.into(),
                lpszClassName: class,
                // This toast is clickable (body dismissal / Undo), so it must
                // supply its own cursor. A null class cursor leaves this input
                // queue's cursor unset and can retain the busy/startup cursor;
                // NOACTIVATE alone does not make a window hit-test transparent.
                hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return None;
            }
            // TOOLWINDOW removes taskbar / Alt+Tab entries, NOACTIVATE prevents
            // focus changes. Start click-through until the first frame installs
            // its hit data. Layered alpha keeps empty corners click-through.
            let hwnd = CreateWindowExW(
                window_style(false),
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
            h.borrow_mut().entry(self.hwnd.0 as usize).or_default().update_frame(&f);
        });
        self.set_interactive(f.interactive);
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
            // ULW_ALPHA's native per-pixel hit shape lets alpha-zero padding
            // pass through across process boundaries; HTTRANSPARENT alone only
            // searches this thread's windows.
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

    fn set_interactive(&self, interactive: bool) {
        // SAFETY: This thread owns the HWND. Only the extended input style is
        // changed; layered rendering, z-order and activation policy stay intact.
        unsafe {
            let style = window_style(interactive).0 as isize;
            if GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) != style {
                // HTTRANSPARENT only searches windows on the same thread.
                // LAYERED + TRANSPARENT also passes through to other processes,
                // letting their cursor and clicks survive the exit animation.
                SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, style);
            }
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
        if unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
            .0
            .is_null()
        {
            return None;
        }
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
    fn scales(&mut self) -> Vec<(u64, f64)> {
        SCALES.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }
    fn feedback(&mut self) -> Vec<Feedback> {
        FEEDBACK.with(|f| std::mem::take(&mut *f.borrow_mut()))
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
    if msg == WM_DPICHANGED {
        // A scale-only change need not change DisplayInfo's physical bounds.
        // Repaint through the queue even when the display watcher sees no change.
        // Do not accept the suggested rect: the queue owns toast stack placement.
        let dpi = (w.0 & 0xffff) as u32;
        if dpi > 0 {
            HITS.with(|h| {
                if let Some(hit) = h.borrow().get(&(hwnd.0 as usize)) {
                    SCALES.with(|s| s.borrow_mut().push((hit.id, f64::from(dpi) / 96.)));
                }
            });
        }
        return LRESULT(0);
    }
    if msg == WM_NCHITTEST {
        let mut point = POINT {
            x: (l.0 as u16 as i16) as i32,
            y: ((l.0 >> 16) as u16 as i16) as i32,
        };
        // SAFETY: owned HWND and initialized screen point from WM_NCHITTEST.
        unsafe {
            let _ = ScreenToClient(hwnd, &mut point);
        }
        let hit = HITS.with(|h| {
            h.borrow()
                .get(&(hwnd.0 as usize))
                .is_some_and(|h| h.contains(Point::new(point.x.into(), point.y.into())))
        });
        return LRESULT(if hit { HTCLIENT as isize } else { HTTRANSPARENT as isize });
    }
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    if matches!(msg, WM_MOUSEMOVE | WM_MOUSELEAVE | WM_LBUTTONDOWN | WM_LBUTTONUP) {
        if msg == WM_MOUSEMOVE {
            // Track leave without taking capture or activating the toast. Without
            // this, the last hovered button stays highlighted outside the HWND.
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: Valid initialized structure and our current HWND.
            unsafe {
                let _ = TrackMouseEvent(&mut track);
            }
        }
        let p = Point::new((l.0 as u16 as i16) as f64, ((l.0 >> 16) as u16 as i16) as f64);
        let input = match msg {
            WM_LBUTTONDOWN => Input::Down(p),
            WM_LBUTTONUP => Input::Up(p),
            WM_MOUSELEAVE => Input::Leave,
            _ => Input::Move(p),
        };
        HITS.with(|h| {
            if let Some(hit) = h.borrow_mut().get_mut(&(hwnd.0 as usize)) {
                let (feedback, click) = hit.input(input);
                FEEDBACK.with(|f| f.borrow_mut().push(feedback));
                if let Some(click) = click {
                    CLICKS.with(|c| c.borrow_mut().push(click));
                }
            }
        });
        return LRESULT(0);
    }
    // SAFETY: unhandled message and its original parameters are passed to Windows.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

/// Scope awareness to the calling thread; never change the host process policy.
struct DpiContext(DPI_AWARENESS_CONTEXT);
impl DpiContext {
    fn enter() -> Option<Self> {
        // SAFETY: only this thread's context is changed and restored in Drop.
        let old = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        (!old.0.is_null()).then_some(Self(old))
    }
}
impl Drop for DpiContext {
    fn drop(&mut self) {
        // SAFETY: previously returned valid thread context on the same thread.
        unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}

pub(super) fn scale_for(display: &oink_foundation::display::DisplayInfo) -> f64 {
    monitor_scale(display).unwrap_or_else(|| {
        tracing::warn!("notification monitor DPI query failed");
        1.
    })
}

fn monitor_scale(display: &oink_foundation::display::DisplayInfo) -> Option<f64> {
    let _dpi = DpiContext::enter()?;
    // GetDpiForMonitor depends on the HOST PROCESS awareness (96/system DPI in
    // unaware/system-aware hosts), even though our toast worker is PMv2. Query
    // a PMv2 HWND on the target monitor instead. STATIC is a system class; this
    // 1px probe is never shown, activated, or added to the desktop input path.
    // https://learn.microsoft.com/windows/win32/api/shellscalingapi/nf-shellscalingapi-getdpiformonitor
    // SAFETY: owned hidden top-level HWND, created/destroyed on this thread.
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("STATIC"),
            w!(""),
            WS_POPUP,
            (display.bounds.x + display.bounds.w / 2.).floor() as i32,
            (display.bounds.y + display.bounds.h / 2.).floor() as i32,
            1,
            1,
            None,
            None,
            None,
            None,
        )
        .ok()?;
        let dpi = GetDpiForWindow(hwnd);
        let _ = DestroyWindow(hwnd);
        (dpi > 0).then_some(f64::from(dpi) / 96.)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_monitor_query_restores_an_unaware_caller() {
        // SAFETY: scoped context on this test thread; no process policy changes.
        let previous = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE) };
        let restore = DpiContext(previous);
        let window = Window::new().expect("hidden DPI reference");
        let display = oink_foundation::display::DisplayInfo {
            key: "test".into(),
            name: String::new(),
            bounds: Rect::new(0., 0., 1., 1.),
            pixel_width: 1,
            pixel_height: 1,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        };
        let scale = monitor_scale(&display).expect("target monitor DPI");
        // SAFETY: read-only queries of this test's hidden PMv2 HWND and thread.
        unsafe {
            assert_eq!(scale, f64::from(GetDpiForWindow(window.hwnd)) / 96.);
            assert!(
                AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), DPI_AWARENESS_CONTEXT_UNAWARE).as_bool()
            );
        }
        drop(window);
        drop(restore);
    }

    #[test]
    fn dpi_window_context_and_scale_change_are_independent_of_host_awareness() {
        // SAFETY: changes only this test thread; restore guard resets it below.
        // Hidden windows only: never call present/ShowWindow or inject input.
        let previous = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE) };
        let restore = DpiContext(previous);
        let window = Window::new().expect("hidden PMv2 toast");
        // SAFETY: read-only queries on our owned hidden window and test thread.
        unsafe {
            assert!(
                AreDpiAwarenessContextsEqual(
                    GetWindowDpiAwarenessContext(window.hwnd),
                    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
                )
                .as_bool()
            );
            assert!(
                AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), DPI_AWARENESS_CONTEXT_UNAWARE).as_bool(),
                "creating a toast must restore its caller context"
            );
        }
        HITS.with(|h| {
            h.borrow_mut()
                .entry(window.hwnd.0 as usize)
                .or_default()
                .update(42, None, true)
        });
        for dpi in [96usize, 120, 144, 192] {
            // SAFETY: synthetic DPI notification to our hidden window only.
            unsafe {
                SendMessageW(window.hwnd, WM_DPICHANGED, Some(WPARAM(dpi | (dpi << 16))), None);
            }
            assert_eq!(
                SCALES.with(|s| std::mem::take(&mut *s.borrow_mut())),
                vec![(42, dpi as f64 / 96.)]
            );
        }
        drop(window);
        drop(restore);
    }

    #[test]
    fn native_hit_messages_exclude_transparent_corners() {
        let window = Window::new().expect("hidden toast HWND");
        let mut pixels = vec![0; 100];
        // The alpha mask, rather than the bounding rectangle, owns input.
        for y in 4..10 {
            for x in 0..8 {
                pixels[y * 10 + x] = 0xff_ff_ff_ff;
            }
        }
        let frame = Frame {
            id: 9,
            bounds: Rect::new(0., 0., 10., 10.),
            card: Rect::new(0., 4., 8., 6.),
            width: 10,
            height: 10,
            pixels,
            undo: Some(Rect::new(5., 5., 2., 2.)),
            interactive: true,
        };
        HITS.with(|h| {
            h.borrow_mut()
                .entry(window.hwnd.0 as usize)
                .or_default()
                .update_frame(&frame)
        });
        let message_point = |point: Point, screen: bool| {
            let mut point = POINT {
                x: point.x as i32,
                y: point.y as i32,
            };
            if screen {
                // SAFETY: hidden owned HWND and initialized client point.
                unsafe {
                    let _ = ClientToScreen(window.hwnd, &mut point);
                }
            }
            LPARAM(((point.y as u16 as u32) << 16 | point.x as u16 as u32) as isize)
        };
        for (point, expected, undo) in [
            (Point::new(1., 1.), HTTRANSPARENT as isize, false),
            (Point::new(2., 7.), HTCLIENT as isize, false),
            (Point::new(8., 2.), HTTRANSPARENT as isize, false),
            (Point::new(6., 6.), HTCLIENT as isize, true),
        ] {
            // SAFETY: direct messages to our hidden HWND; no desktop input.
            unsafe {
                assert_eq!(
                    SendMessageW(window.hwnd, WM_NCHITTEST, None, Some(message_point(point, true))).0,
                    expected
                );
                SendMessageW(window.hwnd, WM_LBUTTONDOWN, None, Some(message_point(point, false)));
                SendMessageW(window.hwnd, WM_LBUTTONUP, None, Some(message_point(point, false)));
            }
            let clicks = CLICKS.with(|c| std::mem::take(&mut *c.borrow_mut()));
            assert_eq!(
                clicks,
                if expected == HTCLIENT as isize {
                    vec![Click { id: 9, undo }]
                } else {
                    vec![]
                }
            );
        }
    }

    #[test]
    fn toast_cursor_and_hit_testing_follow_interactivity_without_activation() {
        let window = Window::new().expect("create hidden toast");
        // SAFETY: All messages target our hidden HWND on its owning test thread;
        // no pointer movement, visible window or input injection is involved.
        unsafe {
            let arrow = LoadCursorW(None, IDC_ARROW).unwrap();
            let wait = LoadCursorW(None, IDC_WAIT).unwrap();
            let previous = SetCursor(Some(wait));
            SendMessageW(
                window.hwnd,
                WM_SETCURSOR,
                Some(WPARAM(window.hwnd.0 as usize)),
                Some(LPARAM(HTCLIENT as isize)),
            );
            let actual = GetCursor();
            SetCursor(Some(previous));
            assert_eq!(actual, arrow, "toast must replace a stale busy cursor");
            assert_eq!(GetClassLongPtrW(window.hwnd, GCLP_HCURSOR), arrow.0 as usize);

            for interactive in [false, true, false, true] {
                HITS.with(|h| {
                    h.borrow_mut()
                        .entry(window.hwnd.0 as usize)
                        .or_default()
                        .update(7, None, interactive)
                });
                window.set_interactive(interactive);
                let style = WINDOW_EX_STYLE(GetWindowLongPtrW(window.hwnd, GWL_EXSTYLE) as u32);
                assert!(style.contains(WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST));
                assert_eq!(style.contains(WS_EX_TRANSPARENT), !interactive);
                assert_eq!(
                    SendMessageW(window.hwnd, WM_NCHITTEST, None, Some(LPARAM(0))).0,
                    if interactive {
                        HTCLIENT as isize
                    } else {
                        HTTRANSPARENT as isize
                    },
                );
                assert_eq!(
                    SendMessageW(window.hwnd, WM_MOUSEACTIVATE, None, None).0,
                    MA_NOACTIVATE as isize,
                );
                SendMessageW(window.hwnd, WM_LBUTTONDOWN, None, Some(LPARAM(0)));
                SendMessageW(window.hwnd, WM_LBUTTONUP, None, Some(LPARAM(0)));
                let clicks = CLICKS.with(|c| std::mem::take(&mut *c.borrow_mut()));
                assert_eq!(
                    clicks,
                    if interactive {
                        vec![Click { id: 7, undo: false }]
                    } else {
                        vec![]
                    }
                );
            }
            HITS.with(|h| {
                h.borrow_mut().entry(window.hwnd.0 as usize).or_default().update(
                    7,
                    Some(Rect::new(0., 0., 10., 10.)),
                    true,
                )
            });
            FEEDBACK.with(|f| f.borrow_mut().clear());
            SendMessageW(window.hwnd, WM_MOUSEMOVE, None, Some(LPARAM(0)));
            assert_eq!(
                FEEDBACK.with(|f| f.borrow_mut().pop()).unwrap(),
                Feedback {
                    id: 7,
                    toast_hovered: true,
                    hovered: true,
                    pressed: false
                }
            );
            SendMessageW(window.hwnd, WM_LBUTTONDOWN, None, Some(LPARAM(0)));
            assert_eq!(
                FEEDBACK.with(|f| f.borrow_mut().pop()).unwrap(),
                Feedback {
                    id: 7,
                    toast_hovered: true,
                    hovered: true,
                    pressed: true
                }
            );
            SendMessageW(window.hwnd, WM_MOUSELEAVE, None, None);
            assert_eq!(
                FEEDBACK.with(|f| f.borrow_mut().pop()).unwrap(),
                Feedback {
                    id: 7,
                    toast_hovered: false,
                    hovered: false,
                    pressed: false
                }
            );
            SendMessageW(window.hwnd, WM_LBUTTONUP, None, Some(LPARAM(0)));
            assert!(
                CLICKS.with(|c| c.borrow().is_empty()),
                "leaving cancels press without capture"
            );
            SendMessageW(window.hwnd, WM_LBUTTONDOWN, None, Some(LPARAM(0)));
            SendMessageW(window.hwnd, WM_LBUTTONUP, None, Some(LPARAM(0)));
            assert_eq!(
                CLICKS.with(|c| std::mem::take(&mut *c.borrow_mut())),
                vec![Click { id: 7, undo: true }],
            );
        }
    }
}
