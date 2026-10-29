//! Keep package drops registered on WebView2's asynchronously created HWND tree.
use std::sync::OnceLock;
use tauri::{Emitter, Manager};
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
unsafe extern "C" {
    fn oink_package_drop_attach(
        root: *mut std::ffi::c_void,
        hover: extern "C" fn(bool),
        received: extern "C" fn(*const u16),
        failure: extern "C" fn(i32),
    ) -> u32;
}
extern "C" fn hover(active: bool) {
    if let Some(app) = APP.get() {
        let _ = app.emit_to("main", "local-package-drag", active);
    }
}
extern "C" fn received(path: *const u16) {
    // SAFETY: the native receiver lends a bounded NUL-terminated UTF-16 path.
    let path = unsafe {
        let mut length = 0;
        while length < 32768 && *path.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(path, length))
    };
    if let Some(app) = APP.get() {
        let _ = app.emit_to("main", "local-package-drop", path);
    }
}
extern "C" fn failure(code: i32) {
    tracing::warn!(code, "could not register a package drop target");
}
pub fn attach(window: &tauri::WebviewWindow) -> tauri::Result<()> {
    let _ = APP.set(window.app_handle().clone());
    let root = window.hwnd()?.0 as usize;
    window.with_webview(move |_| {
        // SAFETY: UI/STA thread with the live main HWND. Native subclass teardown
        // revokes registrations and stops its audit before the window is destroyed.
        let error = unsafe { oink_package_drop_attach(root as _, hover, received, failure) };
        if error != 0 {
            tracing::error!(error, "could not attach the main package drop receiver");
        } else {
            tracing::info!("main package drop receiver attached");
        }
    })
}
