//! Dedicated foreground capture window, separate from the nonactivating cursor cover.
//! All activation is on this ordinary UI thread; the capture callback only queues intent.
use crate::source_foreground::{Lease, Native, ReleaseDecision};
use std::{
    collections::HashSet,
    sync::{Mutex, mpsc},
    thread::JoinHandle,
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentProcessId},
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, GUITHREADINFO, GW_HWNDNEXT,
            GWL_EXSTYLE, GWL_STYLE, GetForegroundWindow, GetGUIThreadInfo, GetMessageW, GetTopWindow, GetWindow,
            GetWindowLongPtrW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, MSG, PostMessageW,
            PostQuitMessage, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SetForegroundWindow, ShowWindow,
            ShowWindowAsync, TranslateMessage, WM_APP, WM_CHAR, WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_SYSCHAR,
            WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_DISABLED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
        },
    },
    core::{BOOL, w},
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
    visible_at_departure: Option<HashSet<isize>>,
    new_target: Option<isize>,
}

fn windows() -> Option<Vec<HWND>> {
    unsafe extern "system" fn collect(window: HWND, context: LPARAM) -> BOOL {
        // SAFETY: EnumWindows invokes this synchronously with our live Vec.
        unsafe { (&mut *(context.0 as *mut Vec<HWND>)).push(window) };
        BOOL(1)
    }
    let mut windows = Vec::new();
    // SAFETY: synchronous enumeration; no HWND ownership or pointers escape it.
    unsafe { EnumWindows(Some(collect), LPARAM((&mut windows as *mut Vec<HWND>) as isize)) }.ok()?;
    Some(windows)
}

fn application_window(window: HWND) -> bool {
    // SAFETY: read-only queries tolerate windows being destroyed during enumeration.
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(window, Some(&mut pid));
        if pid == 0
            || pid == GetCurrentProcessId()
            || !IsWindowVisible(window).as_bool()
            || GetWindowLongPtrW(window, GWL_STYLE) as u32 & WS_DISABLED.0 != 0
            || IsIconic(window).as_bool()
            || GetWindowLongPtrW(window, GWL_EXSTYLE) as u32 & (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW).0 != 0
        {
            return false;
        }
        let mut cloaked = 0u32;
        DwmGetWindowAttribute(
            window,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked == 0
    }
}

fn active_in_own_queue(window: HWND) -> bool {
    // A denied SetForegroundWindow can still activate the application's own
    // input queue. Query that queue, not thread 0 (the system foreground queue).
    // SAFETY: initialized output structure; a vanished thread returns an error.
    unsafe {
        let thread = GetWindowThreadProcessId(window, None);
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        thread != 0 && GetGUIThreadInfo(thread, &mut info).is_ok() && info.hwndActive == window
    }
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
        self.new_target = None;
        self.visible_at_departure = windows().map(|windows| {
            windows
                .into_iter()
                // Remember every already-visible window, including disabled or
                // tool windows. A later style change must not make an existing
                // window look like an application that appeared during departure.
                // SAFETY: read-only visibility query; destroyed windows return false.
                .filter(|window| unsafe { IsWindowVisible(*window) }.as_bool())
                .map(|window| window.0 as isize)
                .collect()
        });
        tracing::info!(target: "oink_source_foreground", transition = 1u64,
            known = self.visible_at_departure.is_some(),
            "source foreground capture beginning");
        // SAFETY: this thread owns the capture window.
        unsafe {
            let _ = ShowWindow(self.capture, SW_SHOWNOACTIVATE);
        }
    }
    fn hide_capture(&mut self) {
        let before = self.foreground() == Some(self.capture.0 as isize);
        // SAFETY: this thread owns the capture window.
        unsafe {
            let _ = ShowWindow(self.capture, SW_HIDE);
        }
        tracing::info!(target: "oink_source_foreground", transition = 8u64, before,
            active = self.foreground() == Some(self.capture.0 as isize), "source foreground capture hidden");
    }
    fn minimized(&self, window: isize) -> bool {
        // SAFETY: read-only query of the remembered, validated top-level window.
        unsafe { IsIconic(HWND(window as *mut _)).as_bool() }
    }
    fn new_activation_target(&mut self) -> Option<isize> {
        self.new_target = None;
        let before = self.visible_at_departure.as_ref()?;
        let windows = windows()?;
        let candidates: HashSet<_> = windows
            .iter()
            .copied()
            .filter(|window| !before.contains(&(window.0 as isize)))
            .filter(|window| application_window(*window) && active_in_own_queue(*window))
            .map(|window| window.0 as isize)
            .collect();
        // Windows can show a new fullscreen application and activate its local
        // queue while denying system foreground because our capture owns it.
        // Hand foreground to the highest new visible, active application on
        // return; don't override it with the saved application. Existing windows,
        // nonactivating tools, minimized notifications and cloaked desktops are
        // excluded. No global ASFW grant, attached queues or synthetic input.
        // Use documented Z order, not an assumed EnumWindows ordering. Bound the
        // walk by the snapshot size: destruction/reordering must not cause a loop.
        // Verified with a denied fullscreen window on Windows 11 build 26200.
        // SAFETY: read-only top-level Z-order query on this UI thread's desktop.
        let mut window = unsafe { GetTopWindow(None) }.ok()?;
        for _ in 0..windows.len() {
            if candidates.contains(&(window.0 as isize)) {
                self.new_target = Some(window.0 as isize);
                break;
            }
            // SAFETY: read-only Z-order query; destruction ends the bounded walk.
            window = unsafe { GetWindow(window, GW_HWNDNEXT) }.ok()?;
        }
        self.new_target
    }
    fn restore_without_activation(&mut self, window: isize) {
        // SAFETY: the remembered HWND was validated; no pointers are passed.
        // Queue restoration to the other UI thread rather than blocking ours on an unresponsive game.
        // SW_RESTORE also activates when that thread finally handles the request,
        // potentially stealing foreground from an application started in the meantime.
        // SW_SHOWNOACTIVATE restores without that deferred activation; the lease's
        // separate SetForegroundWindow remains a single immediate request. No input
        // queues, synthetic keys or foreground-lock settings are changed. Verified
        // with a stalled UI thread and a newly foreground fullscreen window on
        // Windows 11 build 26200; merely checking GetForegroundWindow before queueing
        // SW_RESTORE does not protect against its later activation.
        let success = unsafe { ShowWindowAsync(HWND(window as *mut _), SW_SHOWNOACTIVATE) }.as_bool();
        tracing::info!(target: "oink_source_foreground", transition = 7u64, success,
            "source foreground previous window restore queued");
    }
    fn activate(&mut self, window: isize) {
        // SetForegroundWindow is a single documented request and may be refused.
        // Do not attach input queues, synthesize Alt, alter foreground-lock settings,
        // or retry: those change local input ownership beyond this preference.
        let taking = window == self.capture.0 as isize;
        let transition = if taking {
            3u64
        } else if self.new_target == Some(window) {
            5
        } else {
            4
        };
        // SAFETY: our live capture window or a validated saved/new application HWND.
        let success = unsafe { SetForegroundWindow(HWND(window as *mut _)) }.as_bool();
        tracing::info!(target: "oink_source_foreground", transition, success,
            active = self.foreground() == Some(window), "source foreground activation completed");
        if !success {
            tracing::warn!(taking, "source foreground request refused");
        }
    }
    fn released(&mut self, decision: ReleaseDecision) {
        // Existing reviewed diagnostic scalar names keep window/process identity
        // out of both ordinary logs and group diagnostic bundles. transition=2:
        // before=saved HWND, active=capture owned foreground, known=saved valid,
        // detected=new app, hidden=saved minimized, wanted=activation requested.
        tracing::info!(target: "oink_source_foreground", transition = 2u64,
            before = decision.previous_available, active = decision.capture_foreground,
            known = decision.previous_valid, detected = decision.new_application,
            hidden = decision.previous_minimized, wanted = decision.activation_requested,
            "source foreground release decision");
        self.visible_at_departure = None;
        self.new_target = None;
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
                let mut native = Desktop {
                    capture: window,
                    visible_at_departure: None,
                    new_target: None,
                };
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
