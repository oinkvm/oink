//! Native UI address shared by the normal-user shell and its input backend.
pub const WINDOW_CLASS: &str = "OinkForegroundOwner";
pub const OPEN_MESSAGE: u32 = 0x8000 + 71; // WM_APP + 71

pub fn window_title(data_dir: &std::path::Path) -> String {
    format!("{}-foreground", crate::instance::lock_name(data_dir))
}

/// Preserve grant-before-signal ordering for both native handoffs.
pub fn handoff<E>(
    mut grant: impl FnMut() -> Result<(), E>,
    mut signal: impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    grant()?;
    signal()
}

/// Local emergency input is a user action; remote emergencies and generic
/// backend show/notification events must never borrow the UIAccess grant.
pub fn user_event(event: &crate::api::ApiEvent) -> bool {
    matches!(event, crate::api::ApiEvent::EmergencyStop(stop) if stop.local)
}

/// A hook callback alone owns no foreground permission. Our installed backend
/// has a real UIAccess token, for which Windows documents foreground control.
/// Delegate only for an explicit local open shortcut, never for background IPC.
/// Check both the real token and ASFW's result; do not infer permission from a
/// manifest, injection, or receipt of a hook event. Windows 10/11 documented
/// mechanism; interactive verification is pending.
#[cfg(windows)]
pub(crate) fn open_from_hotkey() -> std::io::Result<()> {
    use windows::{
        Win32::{
            Foundation::{LPARAM, WPARAM},
            UI::WindowsAndMessaging::{AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, PostMessageW},
        },
        core::HSTRING,
    };
    if !oink_winsvc::backend::uiaccess()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "open shortcut requires backend UIAccess",
        ));
    }
    let title = HSTRING::from(window_title(&oink_foundation::paths::data_dir()));
    // SAFETY: all strings remain live; these calls inspect a native HWND and
    // post scalar values. No UI object or borrowed pointer crosses processes.
    unsafe {
        let hwnd = FindWindowW(&HSTRING::from(WINDOW_CLASS), &title)?;
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return Err(std::io::Error::other("UI foreground owner unavailable"));
        }
        handoff(
            || AllowSetForegroundWindow(pid),
            || PostMessageW(Some(hwnd), OPEN_MESSAGE, WPARAM(0), LPARAM(0)),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_local_emergency_event_can_use_backend_permission() {
        assert!(!user_event(&crate::api::ApiEvent::ShowWindow));
        for local in [false, true] {
            assert_eq!(
                user_event(&crate::api::ApiEvent::EmergencyStop(crate::api::EmergencyStop {
                    device: oink_foundation::DeviceId([1; 32]).into(),
                    device_name: "test".into(),
                    local,
                    at_ms: 0,
                })),
                local
            );
        }
    }
    #[test]
    fn grant_precedes_signal_and_failure_never_signals() {
        let calls = std::cell::RefCell::new(Vec::new());
        handoff(
            || {
                calls.borrow_mut().push("grant");
                Ok::<_, ()>(())
            },
            || {
                calls.borrow_mut().push("signal");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["grant", "signal"]);
        assert!(handoff(|| Err(()), || panic!("ungranted signal")).is_err());
    }
}
