//! Low-frequency foreground check for the recognized-gesture shortcut fallback.
use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow};

// OS workaround: Task View has no documented visibility API. These undocumented
// foreground host classes cover Windows 11 / Windows 10 without localized titles
// or a cached "we opened it" flag (it can also open/close locally). Windows 11's
// class is shared with Alt+Tab and snap-assist: deliberately accept those too,
// since the reverse swipe's Esc safely dismisses all three without selecting a
// different app/layout. Alt state would only exclude Alt+Tab, and process/styles
// do not reliably distinguish these Shell surfaces across OS builds. Up still
// sends the existing Win+Tab toggle regardless of this sample.
const TASK_VIEW_HOST_CLASSES: [&str; 2] = ["XamlExplorerHostIslandWindow", "MultitaskingViewFrame"];

fn is_task_view_host(class: &[u16]) -> bool {
    TASK_VIEW_HOST_CLASSES
        .iter()
        .any(|known| class.iter().copied().eq(known.encode_utf16()))
}

// OS workaround: the Explorer desktop has no documented "shown" predicate.
// Progman and WorkerW are its foreground hosts across Windows 10/11 Shell
// layouts (also recognized by Microsoft's PowerToys system-window filter).
// Looking for an existing desktop HWND is insufficient: the desktop always
// exists behind apps. Match the *foreground* class, never a localized title or
// a cached Win+D toggle; local input can change the desktop independently.
const DESKTOP_HOST_CLASSES: [&str; 2] = ["Progman", "WorkerW"];
fn is_desktop_host(class: &[u16]) -> bool {
    DESKTOP_HOST_CLASSES
        .iter()
        .any(|known| class.iter().copied().eq(known.encode_utf16()))
}

// OS workaround: Start's XAML class is not a unique, documented visibility API.
// Its owning image is normally StartMenuExperienceHost.exe. The authorized
// Windows 11 build 26200 check found SearchHost.exe in all three samples while
// Start was shown with focus in its search box. Accept that foreground search
// surface too: pinch-open Esc safely dismisses either; pinch-close must not
// toggle Win and reopen/close it. These are undocumented host identities.
// Query only the image basename with limited process rights: no localized title,
// UIAutomation traversal, cached "we opened it" flag, or process-wide search
// (the Start process can remain running while its menu is closed).
fn is_start_host(image: &[u16]) -> bool {
    let name = image
        .rsplit(|c| *c == u16::from(b'\\') || *c == u16::from(b'/'))
        .next()
        .unwrap_or(image);
    [b"StartMenuExperienceHost.exe".as_slice(), b"SearchHost.exe".as_slice()]
        .iter()
        .any(|expected| {
            name.len() == expected.len()
                && name
                    .iter()
                    .zip(*expected)
                    .all(|(&unit, byte)| unit < 128 && (unit as u8).eq_ignore_ascii_case(byte))
        })
}

fn start_open(foreground: windows::Win32::Foundation::HWND) -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::{
        Win32::{
            Foundation::HANDLE,
            System::Threading::{
                OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
            },
            UI::WindowsAndMessaging::GetWindowThreadProcessId,
        },
        core::PWSTR,
    };
    let mut pid = 0;
    // SAFETY: valid foreground HWND sample and writable process-ID storage.
    if unsafe { GetWindowThreadProcessId(foreground, Some(&mut pid)) } == 0 || pid == 0 {
        return false;
    }
    // SAFETY: read-only, non-inheritable process handle; a disappearing process
    // or denied access is simply not-open. OwnedHandle closes on every path.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return false;
    };
    // SAFETY: OpenProcess returned a newly owned valid handle.
    let process = unsafe { OwnedHandle::from_raw_handle(process.0) };
    let mut image = [0u16; 32768];
    let mut len = image.len() as u32;
    // SAFETY: owned process handle, writable buffer and in/out length.
    if unsafe {
        QueryFullProcessImageNameW(
            HANDLE(process.as_raw_handle()),
            PROCESS_NAME_WIN32,
            PWSTR(image.as_mut_ptr()),
            &mut len,
        )
    }
    .is_err()
    {
        return false;
    }
    is_start_host(&image[..len as usize])
}

pub(super) fn sample(axis: oink_foundation::input::SwipeAxis) -> crate::win_actions::ForegroundState {
    use crate::win_actions::ForegroundState;
    // Called only at a committed swipe on across-inject, never capture/router.
    // A single HWND/class sample makes the two class predicates consistent.
    // SAFETY: read-only queries on the user's desktop; no borrowed native memory.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return ForegroundState::default();
    }
    if axis == oink_foundation::input::SwipeAxis::Pinch {
        return ForegroundState {
            start_open: start_open(foreground),
            ..Default::default()
        };
    }
    let mut class = [0u16; 256];
    // SAFETY: valid fixed buffer; a disappearing window returns zero (not showing).
    let len = unsafe { GetClassNameW(foreground, &mut class) };
    if len <= 0 {
        return ForegroundState::default();
    }
    let class = &class[..len as usize];
    ForegroundState {
        task_view_showing: is_task_view_host(class),
        desktop_showing: is_desktop_host(class),
        ..Default::default()
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn desktop_classes_are_exact_and_do_not_match_other_shell_surfaces() {
        for name in ["Progman", "WorkerW"] {
            let wide: Vec<_> = name.encode_utf16().collect();
            assert!(is_desktop_host(&wide));
            assert!(!is_desktop_host(&wide[..wide.len() - 1]));
            let mut suffixed = wide;
            suffixed.push(0);
            assert!(!is_desktop_host(&suffixed));
        }
        for name in [
            "",
            "progman",
            "workerw",
            "Shell_TrayWnd",
            "SHELLDLL_DefView",
            "Notepad",
            "XamlExplorerHostIslandWindow",
        ] {
            assert!(!is_desktop_host(&name.encode_utf16().collect::<Vec<_>>()));
        }
    }

    #[test]
    fn start_host_matches_only_the_complete_process_basename_case_insensitively() {
        for name in [
            "StartMenuExperienceHost.exe",
            "SearchHost.exe",
            "C:\\Windows\\SystemApps\\SearchHost.EXE",
            "C:\\Windows\\SystemApps\\Microsoft.Windows.StartMenuExperienceHost_cw5n1h2txyewy\\StartMenuExperienceHost.exe",
            "c:\\windows\\STARTMENUEXPERIENCEHOST.EXE",
        ] {
            assert!(is_start_host(&name.encode_utf16().collect::<Vec<_>>()));
        }
        for name in [
            "",
            "StartMenuExperienceHost",
            "OtherStartMenuExperienceHost.exe",
            "StartMenuExperienceHost.exe.old",
            "C:\\StartMenuExperienceHost.exe\\other.exe",
            "SearchHost.exe.old",
            "OtherSearchHost.exe",
            "SearchApp.exe",
            "explorer.exe",
            "ShellExperienceHost.exe",
        ] {
            assert!(!is_start_host(&name.encode_utf16().collect::<Vec<_>>()));
        }
    }

    #[test]
    fn recognizes_only_complete_host_classes_without_titles() {
        for class in TASK_VIEW_HOST_CLASSES {
            let wide: Vec<_> = class.encode_utf16().collect();
            assert!(is_task_view_host(&wide));
            assert!(!is_task_view_host(&wide[..wide.len() - 1]));
            let mut suffixed = wide;
            suffixed.push(u16::from(b'X'));
            assert!(!is_task_view_host(&suffixed));
        }
        for class in [
            "",
            "Shell_TrayWnd",
            "Progman",
            "Notepad",
            "xamlexplorerhostislandwindow",
        ] {
            assert!(!is_task_view_host(&class.encode_utf16().collect::<Vec<_>>()));
        }
    }
}
