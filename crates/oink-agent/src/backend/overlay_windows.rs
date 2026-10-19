//! Non-activating HWNDs on the backend main thread. Keyboard interception continues
//! on the input thread; Esc never relies on these windows taking focus.
use super::{Frame, OverlayShown};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{OnceLock, mpsc};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW,
    DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW,
    EndPaint, FW_BOLD, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, SelectObject, SetBkMode, SetTextColor,
    StretchDIBits, TRANSPARENT,
};
use windows::Win32::System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, MA_NOACTIVATE, MSG, PM_NOREMOVE,
    PeekMessageW, PostQuitMessage, PostThreadMessageW, RegisterClassW, SW_SHOWNOACTIVATE, ShowWindow, TranslateMessage,
    WM_APP, WM_MOUSEACTIVATE, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{HSTRING, w};

type Job = Box<dyn FnOnce() + Send>;
static MAIN: OnceLock<(u32, mpsc::Sender<Job>)> = OnceLock::new();
const WORK: u32 = WM_APP + 31;
struct Window {
    hwnd: HWND,
    pixels: Vec<u8>,
    width: i32,
    height: i32,
    bounds: RECT,
    caption: RECT,
    title: Vec<u16>,
    font: HFONT,
}
thread_local! { static WINDOWS: RefCell<HashMap<isize, Window>> = RefCell::new(HashMap::new()); }

pub(super) fn on_main(f: impl FnOnce() + Send + 'static) {
    if let Some((thread, queue)) = MAIN.get() {
        let _ = queue.send(Box::new(f));
        // SAFETY: The queue was created by run_main before publishing its thread ID.
        unsafe {
            let _ = PostThreadMessageW(*thread, WORK, WPARAM(0), LPARAM(0));
        }
    }
}

pub(super) fn run_main<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel::<Job>();
    // SAFETY: MSG storage is valid for these calls and no borrowed pointers escape.
    // Win32 only creates a thread message queue on its first user32 message call.
    unsafe {
        let mut message = MSG::default();
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
        MAIN.set((GetCurrentThreadId(), tx))
            .expect("backend main loop initialized once");
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let worker = std::thread::spawn(move || {
        let result = work();
        on_main(|| {
            close();
            // SAFETY: this callback runs on the owning message-loop thread.
            unsafe {
                PostQuitMessage(0);
            }
        });
        result
    });
    // SAFETY: MSG is initialized storage; all messages belong to this thread.
    unsafe {
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            if message.message == WORK {
                for job in rx.try_iter() {
                    job();
                }
            } else {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    worker.join().expect("backend host panicked")
}

pub(super) fn hide() {
    super::super::on_main(close);
}
fn close() {
    let windows = WINDOWS.with(|windows| std::mem::take(&mut *windows.borrow_mut()));
    for (_, window) in windows {
        // SAFETY: windows and fonts are created and destroyed on this same thread,
        // and the font has been deselected before the paint callback returns.
        unsafe {
            let _ = DestroyWindow(window.hwnd);
            let _ = DeleteObject(window.font.into());
        }
    }
    super::SHOWN.store(false, std::sync::atomic::Ordering::Release);
}

pub(super) fn show(id: u64, frames: Vec<Frame>) {
    on_main(move || {
        close();
        let mut reports = Vec::new();
        for frame in frames {
            let mut report = OverlayShown {
                display: frame.display.clone(),
                pixel_width: frame.pixels.width(),
                pixel_height: frame.pixels.height(),
                error: None,
            };
            if let Err(error) = make_window(frame) {
                report.error = Some(error.to_string());
            }
            reports.push(report);
        }
        super::report(id, reports);
    });
}

fn make_window(frame: Frame) -> windows::core::Result<()> {
    // SAFETY: constant UTF-16 class name, synchronous title copy, correct window
    // callback ABI, and all GDI/HWND handles remain owned by this main thread.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: w!("OinkIdentifyOverlay"),
            ..Default::default()
        };
        // RegisterClass returns zero once this process has already registered this class.
        let _ = RegisterClassW(&class);
        let b = frame.bounds;
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_TRANSPARENT,
            w!("OinkIdentifyOverlay"),
            &HSTRING::from("Oink screen identification"),
            WS_POPUP,
            b.x as i32,
            b.y as i32,
            b.w as i32,
            b.h as i32,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let width = frame.pixels.width() as i32;
        let height = frame.pixels.height() as i32;
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for pixel in frame.pixels.pixels() {
            pixels.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
        let [x, y, w, h] = frame.caption.map(f64::from);
        let sx = b.w / f64::from(width);
        let sy = b.h / f64::from(height);
        let font = CreateFontW(
            -((h * sy / 4.0).clamp(16.0, 54.0) as i32),
            0,
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        );
        let data = Window {
            hwnd,
            font,
            pixels,
            width,
            height,
            bounds: RECT {
                left: 0,
                top: 0,
                right: b.w as i32,
                bottom: b.h as i32,
            },
            caption: RECT {
                left: (x * sx) as i32,
                top: (y * sy) as i32,
                right: ((x + w) * sx) as i32,
                bottom: ((y + h) * sy) as i32,
            },
            title: frame.title.encode_utf16().collect(),
        };
        WINDOWS.with(|windows| windows.borrow_mut().insert(hwnd.0 as isize, data));
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        Ok(())
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            // SAFETY: The window exists on this thread; BeginPaint/EndPaint bracket this HDC.
            unsafe {
                let dc = BeginPaint(hwnd, &mut paint);
                WINDOWS.with(|windows| {
                    if let Some(window) = windows.borrow_mut().get_mut(&(hwnd.0 as isize)) {
                        let bitmap = BITMAPINFO {
                            bmiHeader: BITMAPINFOHEADER {
                                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                                biWidth: window.width,
                                biHeight: -window.height,
                                biPlanes: 1,
                                biBitCount: 32,
                                biCompression: BI_RGB.0,
                                ..Default::default()
                            },
                            ..Default::default()
                        };
                        StretchDIBits(
                            dc,
                            0,
                            0,
                            window.bounds.right,
                            window.bounds.bottom,
                            0,
                            0,
                            window.width,
                            window.height,
                            Some(window.pixels.as_ptr().cast()),
                            &bitmap,
                            DIB_RGB_COLORS,
                            SRCCOPY,
                        );
                        SetBkMode(dc, TRANSPARENT);
                        SetTextColor(dc, COLORREF(0x00ff_ffff));
                        let previous_font = SelectObject(dc, window.font.into());
                        DrawTextW(
                            dc,
                            &mut window.title,
                            &mut window.caption,
                            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                        );
                        SelectObject(dc, previous_font);
                    }
                });
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        // SAFETY: unhandled message arguments are forwarded unchanged to Win32.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
