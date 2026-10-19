//! Keep unused Tao root-window cursor notifications away from its global clip state.

fn dispatch<T: Default>(message: u32, next: impl FnOnce() -> T) -> T {
    // Tao 0.37.1 refreshes global cursor confinement on these root notifications,
    // even though Oink never grabs the UI cursor. That can release a clip owned
    // by our backend or another app. This shell has no root CursorMoved/Left
    // consumers; WebView2 receives input on its own child HWND. Consume only
    // the unused root notifications, not child input or nonclient Snap/resize.
    // This bypass follows the pinned Tao call graph; desktop UI regression
    // remains unverified while desktop tests are on hold.
    if matches!(message, 0x0200 | 0x02a3) {
        // WM_MOUSEMOVE, WM_MOUSELEAVE
        T::default()
    } else {
        next()
    }
}

#[cfg(windows)]
mod native {
    use super::dispatch;
    use windows::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        UI::{
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::WM_NCDESTROY,
        },
    };

    const SUBCLASS: usize = 0x4f49_4350;

    pub fn prepare(window: &tauri::WebviewWindow) -> tauri::Result<()> {
        let hwnd = HWND(window.hwnd()?.0);
        // SAFETY: called on the owning UI thread, while the root window is hidden.
        unsafe { SetWindowSubclass(hwnd, Some(window_proc), SUBCLASS, 0) }
            .ok()
            .map_err(|e| std::io::Error::other(e).into())
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        id: usize,
        _data: usize,
    ) -> LRESULT {
        if message == WM_NCDESTROY {
            // SAFETY: remove only this subclass; the destruction continues below.
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(window_proc), id);
            }
        }
        dispatch(message, || {
            // SAFETY: unchanged message continues down this window's subclass chain.
            unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
        })
    }
}

#[cfg(windows)]
pub use native::prepare;

#[cfg(test)]
mod tests {
    use super::dispatch;

    #[test]
    fn root_mouse_notifications_never_reach_tao_clip_refresh() {
        for message in [0x0200, 0x02a3] {
            // WM_MOUSEMOVE, WM_MOUSELEAVE
            let result: isize = dispatch(message, || panic!("Tao would refresh the global clip"));
            assert_eq!(result, 0);
        }
    }

    #[test]
    fn webview_input_nonclient_controls_and_window_lifecycle_are_preserved() {
        // Only the root HWND is subclassed. WebView2 children keep their original
        // procedures; root button, wheel, cursor, Snap and lifetime messages pass.
        for message in [0x0201, 0x0202, 0x020a, 0x0020, 0x00a0, 0x0084, 0x0082, 0x0005] {
            assert_eq!(dispatch(message, || 123), 123);
        }
    }
}
