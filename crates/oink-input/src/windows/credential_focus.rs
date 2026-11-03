//! Pre-login only: whether the logon UI's focused element is a password field.
//!
//! Why: on the logon desktop a low-level keyboard hook does not receive the keys
//! typed into the password field (user-tested on the Windows sign-in screen,
//! 2026-11-03); every other key and all mouse input are delivered. Those keys go
//! to the local credential UI and can never be forwarded, so while a password
//! field has focus the local source is withdrawn exactly like a desktop switch:
//! focus returns to this machine and nothing pretends to forward.
//!
//! Why this shape: LogonUI's fields are not Win32 edit controls, so there is no
//! `ES_PASSWORD` style to read; UI Automation's `IsPassword` on the focused
//! element is the documented signal (it is what screen readers use on that
//! screen). UIA calls are cross-process COM and may block, so they never run on
//! the low-level hook thread (whose callbacks have a system timeout); a separate
//! thread polls every 100 ms and posts a message to the hook window on change.
//! Until the first successful read the field is treated as focused (fail closed);
//! a failed read keeps the last observation.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize},
    UI::{
        Accessibility::{CUIAutomation, IUIAutomation},
        WindowsAndMessaging::PostMessageW,
    },
};

static FOCUSED: AtomicBool = AtomicBool::new(false);

/// True while the source must be withdrawn for a focused password field.
pub(super) fn password_focused() -> bool {
    super::profile::current().credential_gate && FOCUSED.load(Ordering::Acquire)
}

pub(super) struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start observing for the hook window (pre-login profile only).
pub(super) fn start(window: HWND, message: u32) -> Option<Watcher> {
    if !super::profile::current().credential_gate {
        return None;
    }
    FOCUSED.store(true, Ordering::Release);
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let window = window.0 as isize;
    let thread = std::thread::Builder::new()
        .name("oink-credential-focus".into())
        .spawn(move || observe(HWND(window as *mut _), message, &stopping))
        .map_err(|error| tracing::error!(%error, "credential focus observer unavailable; local source withdrawn"))
        .ok()?;
    Some(Watcher {
        stop,
        thread: Some(thread),
    })
}

fn observe(window: HWND, message: u32, stop: &AtomicBool) {
    // SAFETY: this thread's own COM apartment, balanced below.
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    // SAFETY: in-process UIA client object; released before CoUninitialize.
    let automation: windows::core::Result<IUIAutomation> =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) };
    match automation {
        Ok(automation) => {
            let mut reported = false;
            while !stop.load(Ordering::Acquire) {
                // SAFETY: COM calls on this thread's live UIA client.
                let read = unsafe { automation.GetFocusedElement().and_then(|e| e.CurrentIsPassword()) };
                match read {
                    Ok(password) => {
                        let password = password.as_bool();
                        if FOCUSED.swap(password, Ordering::AcqRel) != password {
                            tracing::info!(password, "logon focus changed");
                            // SAFETY: scalar notification to the live hook window.
                            let _ = unsafe { PostMessageW(Some(window), message, WPARAM(0), LPARAM(0)) };
                        }
                    }
                    Err(error) if !reported => {
                        reported = true;
                        tracing::warn!(
                            code = error.code().0,
                            "logon focus read failed; keeping last observation"
                        );
                    }
                    Err(_) => {}
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Err(error) => {
            tracing::error!(
                code = error.code().0,
                "UI Automation unavailable; local source stays withdrawn"
            );
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    if initialized {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}
