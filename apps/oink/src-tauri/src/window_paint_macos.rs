//! Reveal transparent normal windows only after WebKit has drawn their contents.
use block2::RcBlock;
use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{NSApplication, NSImage, NSWindow};
use objc2_foundation::NSError;
use objc2_web_kit::{WKSnapshotConfiguration, WKWebView};
use tauri::{AppHandle, Manager, WebviewWindow};

fn native(window: &WebviewWindow) -> tauri::Result<Retained<NSWindow>> {
    MainThreadMarker::new().expect("window presentation on AppKit thread");
    // SAFETY: Tauri owns this live NSWindow; all callers run on AppKit's thread.
    Ok(unsafe { Retained::retain(window.ns_window()?.cast::<NSWindow>()) }.expect("Tauri NSWindow"))
}

pub(crate) fn show(app: &AppHandle, window: &WebviewWindow) -> tauri::Result<()> {
    let native = native(window)?;
    let mtm = MainThreadMarker::new().expect("window presentation on AppKit thread");
    let application = NSApplication::sharedApplication(mtm);
    if native.isVisible() && !native.isMiniaturized() && !application.isHidden() && native.alphaValue() > 0.0 {
        if !window.is_focused()? {
            application.activate();
            window.set_focus()?;
        }
        return Ok(());
    }
    let Some(ticket) = app
        .state::<crate::shell::Shell>()
        .paint
        .lock()
        .unwrap()
        .begin(window.label())
    else {
        return Ok(());
    };
    // AppKit may restore a hidden/minimized window itself (Dock/unhide), before
    // the shell receives Reopen. Arm alpha zero on hide as well as here so that
    // native traffic lights cannot escape ahead of WebKit. We keep input off
    // until reveal. Alpha zero is NOT used to make hidden rAF run: on macOS 26
    // those callbacks can remain suspended even with throttling disabled.
    native.setAlphaValue(0.0);
    native.setIgnoresMouseEvents(true);
    let restoring_minimized = native.isMiniaturized();
    application.unhideWithoutActivation();
    let handle = app.clone();
    let owner = window.clone();
    let result = window.with_webview(move |webview| {
        // SAFETY: with_webview lends Wry's live WKWebView on AppKit's thread.
        let view = unsafe { &*webview.inner().cast::<WKWebView>() };
        view.setHidden(false);
        handle
            .state::<crate::shell::Shell>()
            .paint
            .lock()
            .unwrap()
            .unhidden(owner.label(), ticket);
        tracing::debug!(view = owner.label(), ticket, "window presentation: webview.show");
        // SAFETY: this callback runs on the WKWebView's AppKit thread.
        let configuration = unsafe { WKSnapshotConfiguration::new(MainThreadMarker::new().unwrap()) };
        // A DOM/font acknowledgement is not a paint acknowledgement. WebKit's
        // public snapshot API with afterScreenUpdates forces a rendering update
        // even in an ordered-out WKWebView, whereas double-rAF can deadlock.
        // The image is discarded: the barrier primes the actual page backing,
        // not a placeholder. Verified ordered-out and hide/unhide on macOS 26.6.2;
        // this API is available throughout our macOS 14.2+ support range.
        // Hidden live-view demand stays off; only this pending drawing barrier
        // temporarily unhides the WebView. Windows keeps its existing path.
        // SAFETY: configuration is owned on the WKWebView's main thread.
        unsafe { configuration.setAfterScreenUpdates(true) };
        let completion = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            if !handle
                .state::<crate::shell::Shell>()
                .paint
                .lock()
                .unwrap()
                .finish(owner.label(), ticket)
            {
                return;
            }
            let mtm = MainThreadMarker::new().expect("WebKit snapshot completion on AppKit thread");
            if !error.is_null() || image.is_null() {
                tracing::error!(view = owner.label(), ticket, "WebKit window drawing barrier failed");
                let webview: &tauri::Webview = owner.as_ref();
                let _ = webview.hide();
                crate::live_views::refresh(&handle);
                return;
            }
            tracing::debug!(view = owner.label(), ticket, "window presentation: WebKit frame drawn");
            // A user can hide/minimize while the rendering IPC is in flight.
            // Never let its late completion undo that user action.
            if NSApplication::sharedApplication(mtm).isHidden()
                || (!restoring_minimized && owner.is_minimized().unwrap_or(true))
            {
                let webview: &tauri::Webview = owner.as_ref();
                let _ = webview.hide();
                crate::live_views::refresh(&handle);
                return;
            }
            if let Err(error) = reveal(&handle, &owner) {
                tracing::error!(view = owner.label(), %error, "could not reveal drawn product window");
            }
        });
        // SAFETY: configuration and callback have the public WKWebView API's
        // exact types; WebKit copies the block until its main-thread completion.
        unsafe { view.takeSnapshotWithConfiguration_completionHandler(Some(&configuration), &completion) };
    });
    if result.is_err() {
        app.state::<crate::shell::Shell>()
            .paint
            .lock()
            .unwrap()
            .cancel(window.label());
    }
    result
}

fn reveal(app: &AppHandle, window: &WebviewWindow) -> tauri::Result<()> {
    let native = native(window)?;
    window.unminimize()?;
    native.setAlphaValue(1.0);
    native.setIgnoresMouseEvents(false);
    tracing::debug!(view = window.label(), "window presentation: window.show");
    window.show()?;
    let mtm = MainThreadMarker::new().expect("window presentation on AppKit thread");
    NSApplication::sharedApplication(mtm).activate();
    window.set_focus()?;
    tracing::info!(view = window.label(), "ready product window shown");
    crate::live_views::refresh(app);
    Ok(())
}

pub(crate) fn visibility(app: &AppHandle, window: &WebviewWindow, visible: bool) -> tauri::Result<()> {
    if app
        .state::<crate::shell::Shell>()
        .paint
        .lock()
        .unwrap()
        .pending(window.label())
    {
        // Parent remains ordered out during preparation. A reentrant refresh
        // must not hide the WKWebView again before the drawing IPC completes.
        return Ok(());
    }
    if visible {
        show(app, window)
    } else {
        let native = native(window)?;
        native.setAlphaValue(0.0);
        native.setIgnoresMouseEvents(true);
        tracing::debug!(view = window.label(), "window presentation: webview.hide");
        let webview: &tauri::Webview = window.as_ref();
        webview.hide()
    }
}
