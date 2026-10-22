//! Explicit native acceptance: only the two windows owned by this test receive buttons.
use super::*;
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

thread_local! {
    static DOWNS: RefCell<usize> = const { RefCell::new(0) };
    static UPS: RefCell<Vec<(usize, i16, i16)>> = const { RefCell::new(Vec::new()) };
}
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if message == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    if message == WM_NCHITTEST {
        return LRESULT(HTCLIENT as isize);
    }
    if message == WM_LBUTTONDOWN {
        DOWNS.with_borrow_mut(|count| *count += 1);
        eprintln!("native fixture DOWN window={:?}", hwnd);
        // No capture: an unpositioned UP must reveal its actual hit recipient.
        return LRESULT(0);
    }
    if message == WM_LBUTTONUP {
        eprintln!("native fixture UP window={:?}", hwnd);
        UPS.with_borrow_mut(|events| events.push((hwnd.0 as usize, lp.0 as i16, (lp.0 >> 16) as i16)));
        return LRESULT(0);
    }
    // SAFETY: standard dispatch for this test's private window.
    unsafe { DefWindowProcW(hwnd, message, wp, lp) }
}
fn pump_until(ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let mut message = MSG::default();
        // SAFETY: valid output message; only this test thread's windows are dispatched.
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut cursor = POINT::default();
    let mut clip = windows::Win32::Foundation::RECT::default();
    // SAFETY: valid output storage for read-only native diagnostics.
    unsafe {
        let _ = GetCursorPos(&mut cursor);
        let _ = GetClipCursor(&mut clip);
    }
    panic!("private window input boundary not observed: cursor={cursor:?} clip={clip:?}");
}

#[test]
#[ignore = "requires device-test lock and an interactive Windows desktop"]
fn source_up_returns_to_recorded_start_after_cursor_moves_to_another_window() {
    assert!(
        std::env::var_os("OINK_OWNED_WINDOW_TEST").is_some(),
        "run through the device-test wrapper"
    );
    let _dpi = dpi::ThreadDpiGuard::per_monitor_v2();
    let input = WinInput::default();
    let before = input.cursor_position().unwrap();
    // SAFETY: module/geometry queries; all created windows belong to this thread.
    let windows = unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: w!("OinkSourceReleaseFixture"),
            ..Default::default()
        };
        assert_ne!(RegisterClassW(&class), 0);
        let x = GetSystemMetrics(SM_CXSCREEN) / 2 - 160;
        let y = GetSystemMetrics(SM_CYSCREEN) / 2 - 60;
        [x, x + 180].map(|x| {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class.lpszClassName,
                w!("Oink source release test"),
                WS_POPUP | WS_VISIBLE,
                x,
                y,
                140,
                140,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap()
        })
    };
    struct Cleanup<'a> {
        input: &'a WinInput,
        windows: [HWND; 2],
        before: Point,
        start: Point,
        armed: Cell<bool>,
    }
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            // Settle any test press before removing its only allowed recipient.
            if self.armed.get() {
                let _ = self.input.release_drag_at(self.start);
            }
            let until = Instant::now() + Duration::from_millis(50);
            while Instant::now() < until {
                let mut message = MSG::default();
                // SAFETY: drain this test thread's own messages before destroying its windows.
                unsafe {
                    while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // SAFETY: destroy only this thread's two self-owned windows.
            unsafe {
                for window in self.windows {
                    let _ = DestroyWindow(window);
                }
            }
            let _ = self.input.inject(&InjectEvent::MoveTo { pos: self.before });
        }
    }
    let positions = windows.map(|window| {
        let mut rect = windows::Win32::Foundation::RECT::default();
        // SAFETY: live self-owned window and valid output rectangle.
        unsafe { GetWindowRect(window, &mut rect) }.unwrap();
        Point::new(f64::from(rect.left + 50), f64::from(rect.top + 50))
    });
    let _cleanup = Cleanup {
        input: &input,
        windows,
        before,
        start: positions[0],
        armed: Cell::new(false),
    };
    let mut clip = windows::Win32::Foundation::RECT::default();
    // SAFETY: read-only query; never change another owner's pointer confinement.
    unsafe { GetClipCursor(&mut clip) }.unwrap();
    for (position, window) in positions.into_iter().zip(windows) {
        assert!(
            position.x >= f64::from(clip.left)
                && position.x < f64::from(clip.right)
                && position.y >= f64::from(clip.top)
                && position.y < f64::from(clip.bottom),
            "existing cursor confinement excludes the self-owned fixture"
        );
        // SAFETY: read-only hit test before any button is sent.
        let hit = unsafe {
            WindowFromPoint(POINT {
                x: position.x as i32,
                y: position.y as i32,
            })
        };
        assert_eq!(hit, window);
    }
    // SAFETY: foreground only our self-owned source window before synthetic input.
    unsafe {
        let _ = SetForegroundWindow(windows[0]);
    }
    UPS.with_borrow_mut(Vec::clear);
    DOWNS.with_borrow_mut(|count| *count = 0);
    for positioned in [false, true] {
        _cleanup.armed.set(true);
        input.press_drag_at(positions[0]).unwrap();
        pump_until(|| DOWNS.with_borrow(|count| *count > usize::from(positioned)));
        input.inject(&InjectEvent::MoveTo { pos: positions[1] }).unwrap();
        pump_until(|| input.cursor_position().unwrap() == positions[1]);
        eprintln!(
            "phase positioned={positioned} live={:?} start={:?}",
            input.cursor_position().unwrap(),
            positions[0]
        );
        if positioned {
            input.release_drag_at(positions[0]).unwrap();
        } else {
            input
                .inject(&InjectEvent::Button {
                    button: oink_foundation::input::MouseButton::Left,
                    pressed: false,
                })
                .unwrap();
        }
        _cleanup.armed.set(false);
        pump_until(|| UPS.with_borrow(|events| events.len() > usize::from(positioned)));
    }
    UPS.with_borrow(|events| {
        eprintln!("unpositioned recipient=live-window; positioned recipient=recorded-start; events={events:?}");
        assert_eq!(
            events.as_slice(),
            &[(windows[1].0 as usize, 50, 50), (windows[0].0 as usize, 50, 50)]
        );
    });
}
