//! Main-window AppKit geometry: visible-edge resizing and native toolbar alignment.
use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
    rc::Rc,
};

use block2::RcBlock;
use objc2::{
    MainThreadMarker,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
};
use objc2_app_kit::{
    NSApplication, NSCursor, NSEvent, NSEventMask, NSEventType, NSView, NSWindow, NSWindowButton,
    NSWindowCollectionBehavior, NSWindowDidResignKeyNotification, NSWindowDidResizeNotification, NSWindowStyleMask,
    NSWindowWillEnterFullScreenNotification, NSWindowWillMiniaturizeNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol, NSOperationQueue, NSPoint, NSRect};
use tauri::WebviewWindow;

type Observer = Retained<ProtocolObject<dyn NSObjectProtocol>>;

thread_local! {
    static MONITORS: RefCell<Vec<Retained<AnyObject>>> = const { RefCell::new(Vec::new()) };
    static TITLE_DRAG: RefCell<Option<Rc<TitleDrag>>> = const { RefCell::new(None) };
}

struct TitleDrag {
    native: Retained<NSWindow>,
    phase: Cell<DragPhase>,
    generation: Cell<u64>,
    observers: RefCell<Vec<Observer>>,
}

#[derive(Clone, Copy, PartialEq)]
enum DragPhase {
    Idle,
    Moving,
    Restoring(u64),
}

impl TitleDrag {
    fn finish(&self) {
        if self.phase.replace(DragPhase::Idle) != DragPhase::Idle {
            self.native
                .setStyleMask(self.native.styleMask() & !NSWindowStyleMask::Resizable);
        }
    }
}

pub fn start_title_drag(window: &WebviewWindow) -> tauri::Result<()> {
    let mtm = MainThreadMarker::new().expect("main window on AppKit thread");
    let drag = TITLE_DRAG.with(|slot| slot.borrow().clone());
    if let Some(drag) = drag
        && drag.phase.get() == DragPhase::Idle
        && drag.native.isVisible()
        && !drag.native.styleMask().contains(NSWindowStyleMask::FullScreen)
        && NSEvent::pressedMouseButtons() & 1 != 0
        && let Some(event) = NSApplication::sharedApplication(mtm).currentEvent()
        && event.windowNumber() == drag.native.windowNumber()
    {
        // Keep Tao's configured mask non-resizable for fullscreen restore.
        // WindowManager needs the native capability from before the move
        // starts through its resize commit after release. Clearing it on UP
        // (even on the next main-queue turn) cancelled corner tiles on 26.6.2.
        drag.phase.set(DragPhase::Moving);
        drag.generation.set(drag.generation.get().wrapping_add(1));
        drag.native
            .setStyleMask(drag.native.styleMask() | NSWindowStyleMask::Resizable);
        if let Err(error) = window.start_dragging() {
            drag.finish();
            return Err(error);
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Resize {
    origin: NSPoint,
    frame: NSRect,
    horizontal: i8,
    vertical: i8,
}

// Match the 24-point CSS shadow gutter and its 4-point resize overhang.
const SHADOW_GUTTER: f64 = 24.0;
const RESIZE_OVERHANG: f64 = 4.0;
const MANUAL_MIN_WIDTH: f64 = 820.0 + SHADOW_GUTTER * 2.0;
const MANUAL_MIN_HEIGHT: f64 = 560.0 + SHADOW_GUTTER * 2.0;

fn accepts_pointer(frame: NSRect, point: NSPoint, fullscreen: bool) -> bool {
    let inset = if fullscreen {
        0.0
    } else {
        SHADOW_GUTTER - RESIZE_OVERHANG
    };
    point.x >= frame.origin.x + inset
        && point.x <= frame.origin.x + frame.size.width - inset
        && point.y >= frame.origin.y + inset
        && point.y <= frame.origin.y + frame.size.height - inset
}

fn sync_mouse_region(native: &NSWindow, captured: &Cell<bool>) {
    // A normal window stays ordered out while its DOM is prepared. Mouse
    // tracking must not make that hidden window interactive.
    if !native.isVisible() || native.alphaValue() == 0.0 {
        native.setIgnoresMouseEvents(true);
        return;
    }
    if NSEvent::pressedMouseButtons() == 0 {
        captured.set(false);
    }
    // Keep capture during a gesture begun in this window, even outside its
    // frame. A drag arriving from another app must still enter the content.
    if !captured.get() {
        let ignore = !accepts_pointer(
            native.frame(),
            NSEvent::mouseLocation(),
            native.styleMask().contains(NSWindowStyleMask::FullScreen),
        );
        if native.ignoresMouseEvents() != ignore {
            native.setIgnoresMouseEvents(ignore);
            if ignore {
                // Release our last WebKit edge cursor when the window gives up
                // mouse ownership; no native exterior cursor region remains.
                NSCursor::arrowCursor().set();
            }
        }
    }
}

pub fn prepare(window: &WebviewWindow) -> tauri::Result<()> {
    let _mtm = MainThreadMarker::new().expect("main window on AppKit thread");
    // SAFETY: retain Tauri's live NSWindow on its owning event-loop thread.
    let native = unsafe { Retained::retain(window.ns_window()?.cast::<NSWindow>()) }.expect("Tauri NSWindow");
    // NSView hitTest and CSS pointer-events run after WindowServer/AppKit's
    // native frame hit test. They cannot veto its exterior resize margin or
    // pass a nonzero-alpha CSS shadow through to another app. Gate the whole
    // NSWindow at the visible edge instead so the shadow gutter passes through
    // without changing the backing/frame or CSS shadow.
    // Both mouse monitors are needed: a local monitor cannot see re-entry once
    // the window ignores mouse events. Mouse-only global monitoring needs no
    // Accessibility/Input Monitoring grant. Do not use a global event tap.
    // Verified on macOS 26; these public APIs also exist on macOS 14.2+.
    // AppKit's exterior resize cursor regions can remain active even when
    // ignoresMouseEvents makes clicks pass through. Disabling cursor rects or
    // consuming local moves does not remove those system-owned regions. Give
    // our visible-edge tracker sole ownership of resizing instead. Use Tauri's
    // setter so Tao also restores this style after fullscreen; changing only
    // the NSWindow mask would bring the outer targets back on fullscreen exit.
    // FullScreenPrimary and the enabled green button retain native fullscreen
    // outside the titlebar move's temporary native resizing capability.
    window.set_resizable(false)?;
    native.setCollectionBehavior(native.collectionBehavior() | NSWindowCollectionBehavior::FullScreenPrimary);
    window.set_maximizable(true)?;
    let title_drag = Rc::new(TitleDrag {
        native: native.clone(),
        phase: Cell::new(DragPhase::Idle),
        generation: Cell::new(0),
        observers: RefCell::new(Vec::new()),
    });
    let center = NSNotificationCenter::defaultCenter();
    // These window-scoped notifications are posted by AppKit on this thread.
    // The first resize after release means WindowManager accepted the tile.
    // A click following an ordinary move ends the lease before custom resizing.
    // SAFETY: these immutable names are exported by AppKit.
    let notifications = unsafe {
        [
            (NSWindowDidResizeNotification, true),
            (NSWindowDidResignKeyNotification, false),
            (NSWindowWillEnterFullScreenNotification, false),
            (NSWindowWillMiniaturizeNotification, false),
        ]
    };
    for (name, resize) in notifications {
        let owner = Rc::downgrade(&title_drag);
        let block = RcBlock::new(move |_: NonNull<NSNotification>| {
            let Some(owner) = owner.upgrade() else {
                return;
            };
            if !resize {
                owner.finish();
            } else if owner.phase.get() == DragPhase::Moving && NSEvent::pressedMouseButtons() & 1 == 0 {
                let restoring = DragPhase::Restoring(owner.generation.get());
                owner.phase.set(restoring);
                let weak = Rc::downgrade(&owner);
                let restore = RcBlock::new(move || {
                    if let Some(owner) = weak.upgrade().filter(|owner| owner.phase.get() == restoring) {
                        owner.finish();
                    }
                });
                // SAFETY: the queue and the resize observer run on the AppKit
                // thread, and the block only holds a weak reference to its window.
                unsafe { NSOperationQueue::mainQueue().addOperationWithBlock(&restore) };
            }
        });
        // SAFETY: this NSWindow and its notifications stay on the AppKit thread.
        let observer =
            unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), Some(&native), None, &block) };
        title_drag.observers.borrow_mut().push(observer);
    }
    TITLE_DRAG.with(|slot| *slot.borrow_mut() = Some(title_drag.clone()));
    native.setAcceptsMouseMovedEvents(true);
    let captured = Rc::new(Cell::new(false));
    sync_mouse_region(&native, &captured);
    let global_native = native.clone();
    let global_captured = captured.clone();
    let global_drag = title_drag.clone();
    let global = RcBlock::new(move |event: NonNull<NSEvent>| {
        // SAFETY: AppKit supplies a live event during this callback.
        if mouse_down(unsafe { event.as_ref() }.r#type()) {
            global_drag.finish();
        }
        sync_mouse_region(&global_native, &global_captured);
    });
    let mouse_updates = NSEventMask::MouseMoved
        | NSEventMask::LeftMouseDragged
        | NSEventMask::RightMouseDragged
        | NSEventMask::OtherMouseDragged
        | NSEventMask::LeftMouseUp
        | NSEventMask::RightMouseUp
        | NSEventMask::OtherMouseUp;
    let mouse_buttons = NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown | NSEventMask::OtherMouseDown;
    let global_monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mouse_updates | mouse_buttons, &global);
    let tracking = Rc::new(RefCell::new(None::<Resize>));
    let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit supplies a live event during this local callback.
        let event = unsafe { event.as_ref() };
        let kind = event.r#type();
        let point = NSEvent::mouseLocation();
        if mouse_down(kind) {
            title_drag.finish();
            if event.windowNumber() == native.windowNumber()
                && accepts_pointer(
                    native.frame(),
                    point,
                    native.styleMask().contains(NSWindowStyleMask::FullScreen),
                )
            {
                captured.set(true);
            }
        }
        sync_mouse_region(&native, &captured);
        if kind == NSEventType::LeftMouseDown {
            if event.windowNumber() != native.windowNumber()
                || native.styleMask().contains(NSWindowStyleMask::FullScreen)
            {
                return event as *const _ as *mut _;
            }
            let frame = native.frame();
            let left = frame.origin.x + SHADOW_GUTTER;
            let right = frame.origin.x + frame.size.width - SHADOW_GUTTER;
            let bottom = frame.origin.y + SHADOW_GUTTER;
            let top = frame.origin.y + frame.size.height - SHADOW_GUTTER;
            let near_x = (point.x - left).abs() <= RESIZE_OVERHANG || (point.x - right).abs() <= RESIZE_OVERHANG;
            let near_y = (point.y - bottom).abs() <= RESIZE_OVERHANG || (point.y - top).abs() <= RESIZE_OVERHANG;
            // The 16-point corner targets also cover the transparent part of
            // the superellipse. Interior toolbar/buttons never start a resize.
            let corner =
                (point.x < left + 12.0 || point.x > right - 12.0) && (point.y < bottom + 12.0 || point.y > top - 12.0);
            if !accepts_pointer(frame, point, false) || !(near_x || near_y || corner) {
                return event as *const _ as *mut _;
            }
            *tracking.borrow_mut() = Some(Resize {
                origin: point,
                frame,
                horizontal: if near_x || corner {
                    if point.x < (left + right) / 2.0 { -1 } else { 1 }
                } else {
                    0
                },
                vertical: if near_y || corner {
                    if point.y < (bottom + top) / 2.0 { -1 } else { 1 }
                } else {
                    0
                },
            });
            return std::ptr::null_mut();
        }
        if let Some(resize) = *tracking.borrow()
            && kind == NSEventType::LeftMouseDragged
        {
            let mut frame = resize.frame;
            let dx = point.x - resize.origin.x;
            let dy = point.y - resize.origin.y;
            if resize.horizontal != 0 {
                frame.size.width = (resize.frame.size.width + dx * resize.horizontal as f64)
                    .max(MANUAL_MIN_WIDTH.min(resize.frame.size.width));
                if resize.horizontal < 0 {
                    frame.origin.x += resize.frame.size.width - frame.size.width;
                }
            }
            if resize.vertical != 0 {
                frame.size.height = (resize.frame.size.height + dy * resize.vertical as f64)
                    .max(MANUAL_MIN_HEIGHT.min(resize.frame.size.height));
                if resize.vertical < 0 {
                    frame.origin.y += resize.frame.size.height - frame.size.height;
                }
            }
            native.setFrame_display(frame, true);
            return std::ptr::null_mut();
        }
        if kind == NSEventType::LeftMouseUp && tracking.borrow_mut().take().is_some() {
            return std::ptr::null_mut();
        }
        event as *const _ as *mut _
    });
    // Tao's startResizeDragging is unsupported on macOS. Keep the standard
    // titled NSWindow for traffic lights/fullscreen/Mission Control,
    // and track only the web shell's visible edges here. The native outer edges
    // are mouse-transparent. AppKit event tracking avoids asynchronous WebView IPC
    // jitter and preserves the opposite edge/minimum size during a resize.
    // Verified on macOS 26; uses public AppKit APIs available on macOS 14.2+.
    // SAFETY: block and retained NSWindow stay on AppKit's main thread.
    let monitor =
        unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mouse_updates | mouse_buttons, &block) };
    MONITORS.with(|slot| *slot.borrow_mut() = global_monitor.into_iter().chain(monitor).collect());
    Ok(())
}

pub fn closed() {
    TITLE_DRAG.with(|slot| {
        if let Some(drag) = slot.borrow_mut().take() {
            let center = NSNotificationCenter::defaultCenter();
            for observer in drag.observers.borrow_mut().drain(..) {
                // SAFETY: token from the current AppKit thread's registration.
                unsafe { center.removeObserver(ProtocolObject::as_ref(&observer)) };
            }
        }
    });
    MONITORS.with(|slot| {
        for monitor in slot.borrow_mut().drain(..) {
            // SAFETY: token from this thread's AppKit local monitor registration.
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
    });
}

fn mouse_down(kind: NSEventType) -> bool {
    matches!(
        kind,
        NSEventType::LeftMouseDown | NSEventType::RightMouseDown | NSEventType::OtherMouseDown
    )
}

/// The standard control's centre in top-origin WebView points (CSS pixels).
pub fn traffic_light_center(native: &NSWindow, webview: &NSView) -> Option<f64> {
    if native.styleMask().contains(NSWindowStyleMask::FullScreen) {
        return None;
    }
    let button = native.standardWindowButton(NSWindowButton::CloseButton)?;
    let rect = button.convertRect_toView(button.bounds(), Some(webview));
    let center = rect.origin.y + rect.size.height / 2.0;
    // Tao's traffic-light inset resizes the titlebar container; it is not the
    // visible button centre. AppKit 14 and 26 have different button frames and
    // origins (measured: 14 has a 16-point button at y=6; 26 has 14 at y=9).
    // Convert the real bounds into WKWebView coordinates rather than
    // assuming that an inset calibrated on one OS also centres the web toolbar.
    // Both normal/flipped views use logical points; no backing-scale multiplier.
    Some(if webview.isFlipped() {
        center - webview.bounds().origin.y
    } else {
        webview.bounds().origin.y + webview.bounds().size.height - center
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::NSSize;

    fn frame() -> NSRect {
        NSRect::new(NSPoint::new(-900.0, 100.0), NSSize::new(1088.0, 728.0))
    }

    #[test]
    fn shadow_gutter_and_native_outer_resize_regions_pass_through() {
        let f = frame();
        for offset in [-4.0, 0.0, 4.0, 12.0, 19.0] {
            for p in [
                NSPoint::new(f.origin.x + offset, 400.0),
                NSPoint::new(f.origin.x + f.size.width - offset, 400.0),
                NSPoint::new(-400.0, f.origin.y + offset),
                NSPoint::new(-400.0, f.origin.y + f.size.height - offset),
            ] {
                assert!(!accepts_pointer(f, p, false), "gutter point {p:?}");
            }
        }
    }

    #[test]
    fn visible_edges_straddle_the_silhouette_by_four_points() {
        let f = frame();
        for offset in [20.0, 24.0, 28.0] {
            for p in [
                NSPoint::new(f.origin.x + offset, 400.0),
                NSPoint::new(f.origin.x + f.size.width - offset, 400.0),
                NSPoint::new(-400.0, f.origin.y + offset),
                NSPoint::new(-400.0, f.origin.y + f.size.height - offset),
            ] {
                assert!(accepts_pointer(f, p, false), "visible edge {p:?}");
            }
        }
        assert!(accepts_pointer(f, NSPoint::new(-400.0, 400.0), false));
    }

    #[test]
    fn fullscreen_has_no_shadow_gutter() {
        let f = frame();
        assert!(accepts_pointer(f, f.origin, true));
        assert!(accepts_pointer(
            f,
            NSPoint::new(f.origin.x + f.size.width, f.origin.y + f.size.height),
            true
        ));
    }
}
