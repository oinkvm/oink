//! AppKit URL/promise adaptation for the main webview's package drop.
use std::{
    ffi::{CStr, c_char},
    sync::OnceLock,
};
use tauri::{Emitter, Manager};
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
extern "C" fn received(path: *const c_char) {
    if path.is_null() {
        return;
    }
    // SAFETY: AppKit receiver lends this UTF-8 string for the callback only.
    let path = unsafe { CStr::from_ptr(path) }.to_string_lossy().into_owned();
    if let Some(app) = APP.get() {
        let _ = app.emit_to("main", "local-package-drop", path);
    }
}
extern "C" fn dragged(active: u8) {
    if let Some(app) = APP.get() {
        let _ = app.emit_to("main", "local-package-drag", active != 0);
    }
}
pub fn attach(window: &tauri::WebviewWindow) -> tauri::Result<()> {
    let _ = APP.set(window.app_handle().clone());
    window.with_webview(|view| {
        // SAFETY: with_webview runs on the UI thread with a live WKWebView.
        unsafe {
            oink_agent::update::local_mac::attach_drop_view(view.inner(), received, dragged);
        }
    })
}
