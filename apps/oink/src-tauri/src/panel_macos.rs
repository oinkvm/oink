//! A genuine nonactivating NSPanel hosts Tauri's WKWebView content view. The
//! hidden Tauri window remains its lifetime owner; no NSWindow class is mutated.
use std::cell::RefCell;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSEventMask, NSEventType, NSPanel, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use tauri::{AppHandle, Manager};

use oink_agent::backend::{PanelAnchor, PanelRect};

define_class!(
    // SAFETY: NSPanel adds no subclass invariants. Every access stays on AppKit's
    // main thread, including the WKWebView and event monitor destructors.
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkWebPanel"]
    struct WebPanel;
    impl WebPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool { true }
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool { false }
    }
);

struct Panel {
    native: Retained<WebPanel>,
    owner: Retained<NSWindow>,
    content: Retained<NSView>,
    monitors: Vec<Retained<AnyObject>>,
    measured: bool,
}

impl Drop for Panel {
    fn drop(&mut self) {
        for monitor in self.monitors.drain(..) {
            // SAFETY: these tokens came from NSEvent's monitor registration.
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
        self.native.orderOut(None);
        self.native.setContentView(None);
        // Restore Tauri's content before destroying its owning window. Wry keeps
        // that NSWindow for focus/dialog bookkeeping and tears its WebView down.
        self.owner.setContentView(Some(&self.content));
        self.native.close();
    }
}

thread_local! { static PANEL: RefCell<Option<Panel>> = const { RefCell::new(None) }; }

fn native_frame(rect: PanelRect, mtm: MainThreadMarker) -> NSRect {
    let primary = objc2_app_kit::NSScreen::screens(mtm)
        .firstObject()
        .expect("desktop screen");
    let top = primary.frame().origin.y + primary.frame().size.height;
    NSRect::new(
        NSPoint::new(rect.x, top - rect.y - rect.height),
        NSSize::new(rect.width, rect.height),
    )
}

pub fn prepare(app: &AppHandle, rect: PanelRect, anchor: PanelAnchor) -> tauri::Result<()> {
    let mtm = MainThreadMarker::new().expect("panel on main thread");
    let window = app.get_webview_window("panel").expect("panel WebView created first");
    // SAFETY: Tauri's live NSWindow is borrowed on its own event-loop thread.
    let owner = unsafe { Retained::retain(window.ns_window()?.cast::<NSWindow>()) }.expect("Tauri NSWindow");
    let content = owner.contentView().expect("Tauri content view");
    let policy = oink_foundation::panel_policy::PanelPolicy::WEB_POPOVER;
    // SAFETY: initialize our NSPanel subclass with the designated initializer.
    let native: Retained<WebPanel> = unsafe {
        msg_send![WebPanel::alloc(mtm),
            initWithContentRect: native_frame(rect, mtm),
            styleMask: NSWindowStyleMask::Borderless | if policy.nonactivating { NSWindowStyleMask::NonactivatingPanel } else { NSWindowStyleMask::empty() },
            backing: NSBackingStoreType::Buffered,
            defer: false
        ]
    };
    // SAFETY: the Retained owner, rather than AppKit close, releases this panel.
    unsafe { native.setReleasedWhenClosed(false) };
    // The host NSPanel is separate from Tauri's transparent lifetime owner, so
    // it must also opt out of AppKit's opaque white backing. The web menu draws
    // its rounded silhouette and inset border; oink-ui has no outer shadow.
    native.setOpaque(false);
    native.setBackgroundColor(Some(&NSColor::clearColor()));
    native.setHasShadow(false);
    native.setAlphaValue(0.0);
    native.setIgnoresMouseEvents(true);
    native.setHidesOnDeactivate(policy.hides_on_deactivate);
    native.setCanHide(policy.can_hide);
    native.setBecomesKeyOnlyIfNeeded(false);
    native.setFloatingPanel(true);
    native.setLevel(3);
    native.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    // Tauri resolves its NSWindow through its content view even after the
    // WKWebView has moved. Keep an empty view attached so raw-handle requests
    // (including late command ownership checks) retain that invariant.
    let placeholder = NSView::initWithFrame(NSView::alloc(mtm), content.frame());
    owner.setContentView(Some(&placeholder));
    native.setContentView(Some(&content));
    content.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(rect.width, rect.height),
    ));
    // WebKit suspends animation frames in an ordered-out window on macOS 26.
    // Order at alpha zero without taking keys so it can paint and acknowledge
    // readiness. Ignore mouse events until reveal; the placeholder never paints
    // onscreen or intercepts clicks. The transparent backing is required too.
    native.orderFront(None);
    let handle = app.clone();
    let status_rect = native_frame(anchor.rect, mtm);
    let global = RcBlock::new(move |_: NonNull<NSEvent>| {
        let point = NSEvent::mouseLocation();
        // The backend status item toggles the panel itself. Closing here first
        // would race its IPC event and turn the same click into a reopen.
        if point.x >= status_rect.origin.x
            && point.x < status_rect.origin.x + status_rect.size.width
            && point.y >= status_rect.origin.y
            && point.y < status_rect.origin.y + status_rect.size.height
        {
            return;
        }
        dismiss(&handle);
    });
    // SAFETY: monitor callbacks use sendable AppHandles and dispatch to main.
    let global = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
        NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown | NSEventMask::OtherMouseDown,
        &global,
    );
    let handle = app.clone();
    let number = native.windowNumber();
    let local = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit supplies a live event for the duration of this callback.
        let event_ref = unsafe { event.as_ref() };
        let escape = event_ref.r#type() == NSEventType::KeyDown && event_ref.keyCode() == 53;
        let outside = event_ref.r#type() != NSEventType::KeyDown && event_ref.windowNumber() != number;
        if escape || outside {
            dismiss(&handle);
        }
        if escape { std::ptr::null_mut() } else { event.as_ptr() }
    });
    // SAFETY: same as global monitor; return null only to consume panel Escape.
    let local = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(
            NSEventMask::KeyDown
                | NSEventMask::LeftMouseDown
                | NSEventMask::RightMouseDown
                | NSEventMask::OtherMouseDown,
            &local,
        )
    };
    PANEL.with(|slot| {
        *slot.borrow_mut() = Some(Panel {
            native,
            owner,
            content,
            monitors: global.into_iter().chain(local).collect(),
            measured: false,
        })
    });
    Ok(())
}

fn dismiss(app: &AppHandle) {
    let app = app.clone();
    // Tauri executes run_on_main_thread inline on the main thread. Defer through
    // a worker so unregistering the current NSEvent monitor cannot release its
    // callback while that callback is still returning to AppKit.
    tauri::async_runtime::spawn(async move {
        crate::shell::close_panel(&app);
    });
}

pub fn close() {
    PANEL.with(|slot| {
        slot.borrow_mut().take();
    });
}

pub fn resize(window: &tauri::WebviewWindow, rect: PanelRect) {
    let mtm = MainThreadMarker::new().expect("panel on main thread");
    PANEL.with(|slot| {
        if let Some(panel) = slot.borrow_mut().as_mut().filter(|p| owns(p, window)) {
            panel
                .native
                .setFrame_display(native_frame(rect, mtm), panel.native.isVisible());
            panel.measured = true;
        }
    });
}

pub fn show(window: &tauri::WebviewWindow) {
    PANEL.with(|slot| {
        if let Some(panel) = slot.borrow().as_ref().filter(|p| owns(p, window))
            && panel.measured
            && panel.native.alphaValue() == 0.0
        {
            // React acknowledges a paint at the measured size before this
            // first visible frame. NonactivatingPanel was set at construction, so
            // WKWebView may take keys without activating this application.
            panel.native.makeKeyAndOrderFront(None);
            panel.native.setIgnoresMouseEvents(false);
            panel.native.setAlphaValue(1.0);
        }
    });
}

fn owns(panel: &Panel, window: &tauri::WebviewWindow) -> bool {
    // A late IPC callback from a destroyed WebView must not resize or reveal a
    // new panel that happens to reuse its label after another status-item click.
    window
        .ns_window()
        .is_ok_and(|owner| owner == Retained::as_ptr(&panel.owner).cast_mut().cast())
}
