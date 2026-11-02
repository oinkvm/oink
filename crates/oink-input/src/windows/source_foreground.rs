//! Dedicated foreground capture window, separate from the nonactivating cursor cover.
//! All activation is on this ordinary UI thread; the capture callback only queues intent.
use crate::source_foreground::{Lease, Native};
use std::{
    sync::{Mutex, mpsc},
    thread::JoinHandle,
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow, GetMessageW,
            IsIconic, IsWindow, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SW_HIDE, SW_RESTORE,
            SW_SHOWNOACTIVATE, SetForegroundWindow, ShowWindow, ShowWindowAsync, TranslateMessage, WM_APP, WM_CHAR,
            WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_SYSCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_EX_TOOLWINDOW,
            WS_POPUP,
        },
    },
    core::w,
};
const UPDATE: u32 = WM_APP + 1;
const STOP: u32 = WM_APP + 2;
struct Worker {
    window: isize,
    thread: Option<JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        // SAFETY: no payload; the UI thread owns the HWND until it acknowledges by exiting.
        unsafe {
            let _ = PostMessageW(Some(HWND(self.window as *mut _)), STOP, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
#[derive(Default)]
pub(super) struct Controller {
    worker: Mutex<Option<Worker>>,
}
impl std::fmt::Debug for Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceForeground").finish_non_exhaustive()
    }
}
impl Controller {
    pub fn set(&self, enabled: bool) {
        let mut slot = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() && enabled {
            *slot = start();
        }
        if let Some(worker) = slot.as_ref() {
            // SAFETY: scalar intent only, no window calls or waits on the hook thread.
            if unsafe {
                PostMessageW(
                    Some(HWND(worker.window as *mut _)),
                    UPDATE,
                    WPARAM(usize::from(enabled)),
                    LPARAM(0),
                )
            }
            .is_err()
            {
                tracing::warn!(enabled, "cannot dispatch source foreground transition");
            }
        }
    }
    pub fn stop(&self) {
        drop(self.worker.lock().unwrap_or_else(|e| e.into_inner()).take());
    }
}
struct Desktop {
    capture: HWND,
}
impl Native for Desktop {
    type Window = isize;
    fn foreground(&self) -> Option<isize> {
        // SAFETY: read-only window query, no identity is logged or serialized.
        let window = unsafe { GetForegroundWindow() };
        (!window.0.is_null()).then_some(window.0 as isize)
    }
    fn valid(&self, window: isize) -> bool {
        // SAFETY: IsWindow accepts an arbitrary HWND, including a closed window.
        unsafe { IsWindow(Some(HWND(window as *mut _))).as_bool() }
    }
    fn show_capture(&mut self) {
        // SAFETY: this thread owns the capture window.
        unsafe {
            let _ = ShowWindow(self.capture, SW_SHOWNOACTIVATE);
        }
    }
    fn hide_capture(&mut self) {
        // SAFETY: this thread owns the capture window.
        unsafe {
            let _ = ShowWindow(self.capture, SW_HIDE);
        }
    }
    fn minimized(&self, window: isize) -> bool {
        // SAFETY: read-only query of the remembered, validated top-level window.
        unsafe { IsIconic(HWND(window as *mut _)).as_bool() }
    }
    fn restore(&mut self, window: isize) {
        // SAFETY: the remembered HWND was validated; no pointers are passed.
        // Queue restoration to the other UI thread rather than blocking ours on an unresponsive game.
        unsafe {
            let _ = ShowWindowAsync(HWND(window as *mut _), SW_RESTORE);
        }
    }
    fn activate(&mut self, window: isize) {
        // SetForegroundWindow is a single documented request and may be refused.
        // Do not attach input queues, synthesize Alt, alter foreground-lock settings,
        // or retry: those change local input ownership beyond this preference.
        // SAFETY: either our live capture window or the validated remembered HWND.
        if !unsafe { SetForegroundWindow(HWND(window as *mut _)) }.as_bool() {
            tracing::warn!(
                taking = window == self.capture.0 as isize,
                "source foreground request refused"
            );
        }
    }
}
unsafe extern "system" fn procedure(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_DESTROY {
        // SAFETY: this window is the only window owned by the dedicated UI thread.
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    if matches!(
        message,
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR
    ) {
        return LRESULT(0);
    }
    // SAFETY: ordinary native message arguments passed unchanged.
    unsafe { DefWindowProcW(window, message, w, l) }
}
fn start() -> Option<Worker> {
    let (ready, received) = mpsc::sync_channel(0);
    let thread = std::thread::Builder::new()
        .name("oink-source-foreground".into())
        .spawn(move || {
            // SAFETY: this thread creates, pumps and destroys its own top-level tool window.
            unsafe {
                let Ok(module) = GetModuleHandleW(None) else {
                    return;
                };
                let class = w!("OinkSourceForegroundCapture");
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(procedure),
                    hInstance: module.into(),
                    lpszClassName: class,
                    ..Default::default()
                };
                RegisterClassW(&wc);
                let Ok(window) = CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    class,
                    w!("Oink input capture"),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(module.into()),
                    None,
                ) else {
                    return;
                };
                if ready.send(window.0 as isize).is_err() {
                    let _ = DestroyWindow(window);
                    return;
                }
                let mut lease = Lease::default();
                let mut native = Desktop { capture: window };
                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                    match message.message {
                        UPDATE => lease.update(message.wParam.0 != 0, window.0 as isize, &mut native),
                        STOP => break,
                        _ => {
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                }
                lease.update(false, window.0 as isize, &mut native);
                let _ = DestroyWindow(window);
            }
        })
        .ok()?;
    match received.recv_timeout(Duration::from_millis(200)) {
        Ok(window) => Some(Worker {
            window,
            thread: Some(thread),
        }),
        Err(_) => {
            tracing::warn!("source foreground window unavailable");
            None
        }
    }
}
