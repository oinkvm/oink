//! Event-driven capture availability. Never switches desktops or reads secure input.
use crate::native_lifecycle::{self, Data, Kind};
use std::cell::Cell;
use windows::Win32::{
    Foundation::{HANDLE, HWND, LPARAM, WPARAM},
    System::{
        RemoteDesktop::{NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification},
        StationsAndDesktops::{
            CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop,
            UOI_NAME,
        },
    },
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        WindowsAndMessaging::{EVENT_SYSTEM_DESKTOPSWITCH, PostMessageW, WINEVENT_OUTOFCONTEXT, WM_APP},
    },
};
pub(super) const WM_CAPTURE_DESKTOP: u32 = WM_APP + 20;
thread_local! { static WINDOW: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) }; }
pub(super) struct Observer {
    hook: HWINEVENTHOOK,
    window: HWND,
    session: bool,
}
impl Observer {
    pub(super) fn new(window: HWND) -> Self {
        WINDOW.set(window);
        // SAFETY: both callbacks are delivered on this window's existing message loop.
        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_DESKTOPSWITCH,
                EVENT_SYSTEM_DESKTOPSWITCH,
                None,
                Some(changed),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        // SAFETY: owned live window, registrations removed before its destruction.
        let session = unsafe { WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION) }.is_ok();
        emit(
            Kind::InputDesktop,
            Data {
                reason: 1,
                success: Some(!hook.is_invalid()),
                ..Default::default()
            },
        );
        emit(
            Kind::InputDesktop,
            Data {
                reason: 3,
                success: Some(session),
                ..Default::default()
            },
        );
        Self { hook, window, session }
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        WINDOW.set(HWND(std::ptr::null_mut()));
        // SAFETY: own registrations, on the installing thread.
        unsafe {
            if !self.hook.is_invalid() {
                let _ = UnhookWinEvent(self.hook);
            }
            if self.session {
                let _ = WTSUnRegisterSessionNotification(self.window);
            }
        }
    }
}
unsafe extern "system" fn changed(
    _hook: HWINEVENTHOOK,
    event: u32,
    _window: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    native_ms: u32,
) {
    emit(
        Kind::InputDesktop,
        Data {
            message: event,
            extra: u64::from(native_ms),
            ..Default::default()
        },
    );
    // Defer state mutation: the event callback can be nested inside another native call.
    // SAFETY: own live window; only queues a private message.
    unsafe {
        let _ = PostMessageW(Some(WINDOW.get()), WM_CAPTURE_DESKTOP, WPARAM(0), LPARAM(0));
    }
}
/// An inaccessible desktop is unavailable, not evidence that every key is physically UP.
pub(super) fn default_desktop_active() -> bool {
    // SAFETY: read-only handle/query, bounded UTF-16 buffer, handle always closed.
    unsafe {
        let Ok(desktop) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) else {
            return false;
        };
        let mut name = [0u16; 64];
        let result = GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            std::mem::size_of_val(&name) as u32,
            None,
        )
        .is_ok();
        let _ = CloseDesktop(desktop);
        result && name[..8] == [68, 101, 102, 97, 117, 108, 116, 0] // Default; never recorded
    }
}

fn emit(kind: Kind, data: Data) {
    if native_lifecycle::lifecycle_enabled() {
        native_lifecycle::emit(kind, data);
    }
}

pub(super) fn session_changed(message: u32, locked: bool) {
    emit(
        Kind::InputDesktop,
        Data {
            reason: 4,
            message,
            success: Some(locked),
            ..Default::default()
        },
    );
}
