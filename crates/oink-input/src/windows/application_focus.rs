//! An ordinary UI-owner thread owns the hidden keyboard sink and WinEvent hook.
//! Capture hooks never query foreground windows or perform activation.
use crate::application_focus::{Observer, Permit};
use oink_foundation::application_focus::{Foreground, Observation, ObservationStamp, ParkResult};
use std::{
    cell::Cell,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
        System::{
            LibraryLoader::GetModuleHandleW,
            StationsAndDesktops::{
                CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop,
                UOI_NAME,
            },
        },
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            Input::Ime::{HIMC, ImmAssociateContext},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

const WAKE: u32 = WM_APP + 1;
const OBSERVE: u32 = WM_APP + 2;
const WAIT: Duration = Duration::from_millis(100);
static GENERATION: AtomicU64 = AtomicU64::new(1);
thread_local! { static WINDOW: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) }; }

enum Command {
    Watch(Option<Observer>),
    Sample(mpsc::SyncSender<Observation>),
    Park(ObservationStamp, u64, Permit, mpsc::SyncSender<ParkResult>),
    Dismiss,
    Stop,
}
struct Worker {
    sender: mpsc::SyncSender<Command>,
    window: isize,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    fn send(&self, command: Command) -> bool {
        if self.sender.try_send(command).is_err() {
            return false;
        }
        // SAFETY: worker owns this HWND until joined; message carries no pointers.
        unsafe { PostMessageW(Some(HWND(self.window as *mut _)), WAKE, WPARAM(0), LPARAM(0)).is_ok() }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        // Stop is sent only during ordinary backend teardown. If the bounded
        // queue is full, drain finishes before this finite shutdown request.
        let _ = self.sender.send(Command::Stop);
        // SAFETY: worker is still owned; a destroyed HWND simply rejects the post.
        unsafe {
            let _ = PostMessageW(Some(HWND(self.window as *mut _)), WAKE, WPARAM(0), LPARAM(0));
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
        f.debug_struct("ApplicationFocus").finish_non_exhaustive()
    }
}
impl Controller {
    fn send(&self, command: Command) -> bool {
        let mut slot = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = start();
        }
        slot.as_ref().is_some_and(|worker| worker.send(command))
    }
    pub fn watch(&self, observer: Option<Observer>) {
        if observer.is_none() {
            let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
            drop(worker); // Stop/join also destroys the HWND, WinEvent hook and timer.
        } else {
            self.send(Command::Watch(observer));
        }
    }
    pub fn sample(&self) -> Observation {
        let (tx, rx) = mpsc::sync_channel(1);
        if !self.send(Command::Sample(tx)) {
            return unavailable();
        }
        rx.recv_timeout(WAIT).unwrap_or_else(|_| unavailable())
    }
    pub fn park(&self, expected: ObservationStamp, operation: u64, permit: Permit) -> ParkResult {
        let (tx, rx) = mpsc::sync_channel(1);
        if !self.send(Command::Park(expected, operation, permit.clone(), tx)) {
            permit.revoke();
            return ParkResult::Unavailable;
        }
        match rx.recv_timeout(WAIT) {
            Ok(result) => result,
            Err(_) => {
                permit.revoke();
                ParkResult::Unavailable
            }
        }
    }
    pub fn dismiss(&self) {
        if self.worker.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            self.send(Command::Dismiss);
        }
    }
}
fn unavailable() -> Observation {
    // Timeouts replace old native evidence. A recovered owner adopts the new
    // generation so its next real sample can resume ordinary desktop handling.
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    Observation {
        stamp: ObservationStamp {
            generation,
            sequence: 1,
        },
        foreground: Foreground::Unavailable,
        hidden_lost: false,
        park_operation: None,
    }
}

unsafe extern "system" fn procedure(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // Prototype verified on Windows 11 with a zero-size top-level tool window.
    // These are only keyboard messages: lifecycle/paint/activation still use the
    // default procedure. No IME context is associated with this sink, and no
    // synthetic Alt/menu-mask key or foreground-lock workaround is generated.
    if (WM_KEYFIRST..=WM_KEYLAST).contains(&message) {
        return LRESULT(isize::from(message == WM_UNICHAR && w.0 == UNICODE_NOCHAR as usize));
    }
    if matches!(message, WM_ACTIVATE | WM_ACTIVATEAPP | WM_SETFOCUS | WM_KILLFOCUS) {
        // SAFETY: callback HWND is supplied by Windows; no borrowed payload is sent.
        unsafe {
            let _ = PostMessageW(Some(window), OBSERVE, WPARAM(0), LPARAM(0));
        }
    }
    // SAFETY: forward the callback's original arguments to its default procedure.
    unsafe { DefWindowProcW(window, message, w, l) }
}
unsafe extern "system" fn changed(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    // Out-of-context delivery uses this owner's message loop. Do not reenter
    // state while SetForegroundWindow synchronously dispatches notifications.
    // SAFETY: thread-local HWND is owned by this hook's thread until unhooking.
    unsafe {
        let _ = PostMessageW(Some(WINDOW.get()), OBSERVE, WPARAM(0), LPARAM(0));
    }
}
fn start() -> Option<Worker> {
    let (sender, receiver) = mpsc::sync_channel(32);
    let (ready, receive_ready) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("oink-application-focus".into())
        .spawn(move || run(receiver, ready))
        .ok()?;
    match receive_ready.recv_timeout(Duration::from_secs(1)) {
        Ok(Some(window)) => Some(Worker {
            sender,
            window,
            thread: Some(thread),
        }),
        _ => {
            // Closing the channel also makes a late startup retire its window.
            drop(sender);
            // Don't join a delayed UI startup on the ordinary dispatcher.
            drop(thread);
            None
        }
    }
}
struct Ui {
    window: HWND,
    current: Observation,
    foreground: HWND,
    observer: Option<Observer>,
}
impl Ui {
    fn refresh(&mut self, operation: Option<u64>, notify: bool) -> Observation {
        let generation = GENERATION.load(Ordering::Acquire);
        let recovered = self.current.stamp.generation != generation;
        if recovered {
            self.current.stamp = ObservationStamp {
                generation,
                sequence: 1,
            };
        }
        let (foreground, state) = foreground(self.window);
        if recovered || foreground != self.foreground || state != self.current.foreground || operation.is_some() {
            self.current.stamp.sequence = self.current.stamp.sequence.saturating_add(1);
            self.current.hidden_lost |= self.current.foreground == Foreground::Hidden && state != Foreground::Hidden;
            if state == Foreground::Hidden {
                self.current.hidden_lost = false;
            }
            self.current.foreground = state;
            self.current.park_operation = operation;
            self.foreground = foreground;
            if notify && let Some(observer) = &self.observer {
                observer(self.current);
            }
        }
        self.current
    }
    fn park(&mut self, expected: ObservationStamp, operation: u64, permit: Permit) -> ParkResult {
        let current = self.refresh(None, true);
        if !permit.valid() || current.stamp != expected {
            return ParkResult::Stale;
        }
        match current.foreground {
            Foreground::Windowed => {}
            Foreground::Fullscreen => return ParkResult::FullscreenExempt,
            Foreground::Unavailable => return ParkResult::Unavailable,
            _ => return ParkResult::UnknownExempt,
        }
        if !permit.valid() {
            return ParkResult::Stale;
        }
        // SAFETY: this UI thread owns the live sink HWND; activation has no pointer payload.
        unsafe {
            let _ = ShowWindow(self.window, SW_SHOWNOACTIVATE);
            let accepted = SetForegroundWindow(self.window).as_bool();
            let current = self.refresh(Some(operation), true);
            if accepted && current.foreground == Foreground::Hidden {
                ParkResult::Confirmed
            } else {
                ParkResult::Denied
            }
        }
    }
}
fn run(receiver: mpsc::Receiver<Command>, ready: mpsc::SyncSender<Option<isize>>) {
    // SAFETY: all window/hook ownership stays on this thread. Callbacks have the
    // required ABI; stack buffers live through calls and resources are destroyed last.
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            let _ = ready.send(None);
            return;
        };
        let class = w!("OinkApplicationFocusSink");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: module.into(),
            lpszClassName: class,
            ..Default::default()
        };
        // One controller per native backend; an already registered class is OK.
        RegisterClassW(&wc);
        let Ok(window) = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class,
            w!("Oink application focus"),
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
            let _ = ready.send(None);
            return;
        };
        ImmAssociateContext(window, HIMC::default());
        WINDOW.set(window);
        let hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(changed),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        if ready.send(Some(window.0 as isize)).is_err() {
            if !hook.is_invalid() {
                let _ = UnhookWinEvent(hook);
            }
            let _ = DestroyWindow(window);
            return;
        }
        let mut ui = Ui {
            window,
            current: Observation {
                stamp: ObservationStamp {
                    generation: GENERATION.fetch_add(1, Ordering::AcqRel) + 1,
                    sequence: 1,
                },
                ..Observation::default()
            },
            foreground: HWND(std::ptr::null_mut()),
            observer: None,
        };
        let mut message = MSG::default();
        'messages: while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            match message.message {
                WAKE => loop {
                    match receiver.try_recv() {
                        Ok(Command::Watch(observer)) => {
                            ui.observer = observer;
                            if ui.observer.is_some() {
                                SetTimer(Some(window), 1, 50, None);
                            } else {
                                let _ = KillTimer(Some(window), 1);
                            }
                        }
                        Ok(Command::Sample(reply)) => {
                            // Sampling does not activate a window. Keep the stamp
                            // stable so a concurrent readiness/authority sample
                            // cannot invalidate an otherwise valid fenced park.
                            let _ = reply.send(ui.refresh(None, false));
                        }
                        Ok(Command::Park(stamp, operation, permit, reply)) => {
                            let _ = reply.send(ui.park(stamp, operation, permit));
                        }
                        Ok(Command::Dismiss) => {
                            let _ = ShowWindow(window, SW_HIDE);
                        }
                        Ok(Command::Stop) | Err(mpsc::TryRecvError::Disconnected) => break 'messages,
                        Err(mpsc::TryRecvError::Empty) => break,
                    }
                },
                OBSERVE | WM_TIMER => {
                    if ui.observer.is_some() {
                        ui.refresh(None, true);
                    }
                }
                _ => {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
        let _ = KillTimer(Some(window), 1);
        if !hook.is_invalid() {
            let _ = UnhookWinEvent(hook);
        }
        WINDOW.set(HWND(std::ptr::null_mut()));
        let _ = DestroyWindow(window);
    }
}
fn desktop_available() -> bool {
    if super::session_locked() != Some(false) {
        return false;
    }
    // SAFETY: read-only desktop handle is closed after querying a correctly sized buffer.
    unsafe {
        let Ok(desktop) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) else {
            return false;
        };
        let mut name = [0u16; 64];
        let ok = GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            std::mem::size_of_val(&name) as u32,
            None,
        )
        .is_ok();
        let _ = CloseDesktop(desktop);
        ok && name[..8] == [68, 101, 102, 97, 117, 108, 116, 0]
    }
}
fn foreground(hidden: HWND) -> (HWND, Foreground) {
    // SAFETY: these read-only HWND queries tolerate concurrent window destruction;
    // output structures are initialized and their sizes supplied to Windows.
    unsafe {
        let window = GetForegroundWindow();
        if !desktop_available() {
            return (window, Foreground::Unavailable);
        }
        if window == hidden {
            return (window, Foreground::Hidden);
        }
        if window.is_invalid() || window == GetDesktopWindow() || window == GetShellWindow() {
            return (window, Foreground::Unknown);
        }
        let mut class = [0u16; 128];
        let len = GetClassNameW(window, &mut class).max(0) as usize;
        if matches!(
            String::from_utf16_lossy(&class[..len]).as_str(),
            "Shell_TrayWnd"
                | "Shell_SecondaryTrayWnd"
                | "MultitaskingViewFrame"
                | "TaskSwitcherWnd"
                | "XamlExplorerHostIslandWindow"
        ) {
            return (window, Foreground::Unknown);
        }
        let mut thread = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(0, &mut thread).is_err() {
            return (window, Foreground::OrdinaryUnknown);
        }
        if thread.flags.0 & (GUI_INMENUMODE.0 | GUI_POPUPMENUMODE.0 | GUI_SYSTEMMENUMODE.0) != 0 {
            return (window, Foreground::Unknown);
        }
        if !IsWindowVisible(window).as_bool() || IsIconic(window).as_bool() {
            return (window, Foreground::Unknown);
        }
        let mut rect = RECT::default();
        let mut monitor = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetWindowRect(window, &mut rect).is_err()
            || !GetMonitorInfoW(MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST), &mut monitor).as_bool()
        {
            return (window, Foreground::OrdinaryUnknown);
        }
        let bounds = monitor.rcMonitor;
        let fullscreen = rect.left <= bounds.left
            && rect.top <= bounds.top
            && rect.right >= bounds.right
            && rect.bottom >= bounds.bottom;
        (
            window,
            if fullscreen {
                Foreground::Fullscreen
            } else {
                Foreground::Windowed
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_controller_cleanup_does_not_start_a_worker_or_window() {
        let controller = Controller::default();
        controller.watch(None);
        controller.dismiss();
        assert!(controller.worker.lock().unwrap().is_none());
    }
}
