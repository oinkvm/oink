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

pub(super) fn showing() -> bool {
    // SAFETY: read-only queries on the user's desktop; no borrowed native memory.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return false;
    }
    let mut class = [0u16; 256];
    // SAFETY: valid fixed buffer; a disappearing window returns zero (not showing).
    let len = unsafe { GetClassNameW(foreground, &mut class) };
    len > 0 && is_task_view_host(&class[..len as usize])
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

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
