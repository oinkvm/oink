//! Hidden keyboard sink and foreground observation, confined to AppKit's owner.
//! No AX query, activation, or window operation runs on the capture thread.
use super::main_queue;
use crate::application_focus::{Observer, Permit};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEvent,
    NSRunningApplication, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::CFString;
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize};
use oink_foundation::application_focus::{Foreground, Observation, ObservationStamp, ParkResult};
use std::{
    cell::RefCell,
    ffi::c_void,
    sync::atomic::{AtomicU64, Ordering},
    sync::mpsc,
    time::Duration,
};

const WAIT: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(50);
static GENERATION: AtomicU64 = AtomicU64::new(1);

// This sink intentionally is not NSTextInputClient. Its responder chain consumes
// keyboard equivalents and ordinary keys instead of falling through to AppKit's
// noResponderFor:/NSBeep. The prototype was checked on macOS 26 with ordinary and
// Cmd-key events and instrumented beep/noResponder counters. OS-level shortcuts
// and input-source behavior remain the system's responsibility.
define_class!(
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkApplicationFocusWindow"]
    struct SinkWindow;
    impl SinkWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool { true }
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool { true }
        #[unsafe(method(resignKeyWindow))]
        fn resign_key(&self) {
            // AppKit can invoke this synchronously inside makeKey/orderOut. Queue
            // observation to avoid borrowing UI recursively during that operation.
            // SAFETY: NSWindow superclass implements this selector on the main thread.
            unsafe { let _: () = msg_send![super(self), resignKeyWindow]; }
            main_queue::run_after(Duration::ZERO, observe_notification);
        }
        #[unsafe(method(performKeyEquivalent:))]
        fn key_equivalent(&self, _event: &NSEvent) -> bool { true }
        #[unsafe(method(keyDown:))]
        fn key_down(&self, _event: &NSEvent) {}
        #[unsafe(method(keyUp:))]
        fn key_up(&self, _event: &NSEvent) {}
        #[unsafe(method(flagsChanged:))]
        fn flags(&self, _event: &NSEvent) {}
    }
    unsafe impl NSObjectProtocol for SinkWindow {}
);
define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkApplicationFocusSink"]
    struct SinkView;
    impl SinkView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first(&self) -> bool { true }
        #[unsafe(method(performKeyEquivalent:))]
        fn key_equivalent(&self, _event: &NSEvent) -> bool { true }
        #[unsafe(method(keyDown:))]
        fn key_down(&self, _event: &NSEvent) {}
        #[unsafe(method(keyUp:))]
        fn key_up(&self, _event: &NSEvent) {}
        #[unsafe(method(flagsChanged:))]
        fn flags(&self, _event: &NSEvent) {}
    }
    unsafe impl NSObjectProtocol for SinkView {}
);

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn AXUIElementSetMessagingTimeout(element: *const c_void, timeout: f32) -> i32;
    fn AXUIElementCopyAttributeValue(element: *const c_void, attribute: *const c_void, out: *mut *const c_void) -> i32;
    fn AXUIElementGetTypeID() -> usize;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
    fn CFEqual(a: *const c_void, b: *const c_void) -> u8;
    fn CFGetTypeID(value: *const c_void) -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(value: *const c_void) -> u8;
}
struct Owned(*const c_void);
impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: Owned holds one non-null Create/Copy CF reference.
        unsafe { CFRelease(self.0) };
    }
}
fn attribute(element: &Owned, name: &str) -> Option<Owned> {
    let key = CFString::from_str(name);
    let mut out = std::ptr::null();
    // SAFETY: live AX element and CFString; out receives an owned CF reference.
    let error = unsafe { AXUIElementCopyAttributeValue(element.0, (&*key as *const CFString).cast(), &mut out) };
    let value = (!out.is_null()).then(|| Owned(out));
    if error == 0 { value } else { None }
}

struct Pending {
    operation: u64,
    expected: ObservationStamp,
    permit: Permit,
    reply: mpsc::SyncSender<ParkResult>,
}

struct Ui {
    pending: Option<Pending>,
    window: Option<Retained<SinkWindow>>,
    observer: Option<Observer>,
    polling: bool,
    current: Observation,
    pid: i32,
    focused_window: Option<Owned>,
}
impl Default for Ui {
    fn default() -> Self {
        Self {
            pending: None,
            window: None,
            observer: None,
            polling: false,
            current: Observation {
                stamp: ObservationStamp {
                    generation: 1,
                    sequence: 1,
                },
                ..Observation::default()
            },
            pid: 0,
            focused_window: None,
        }
    }
}
thread_local! { static UI: RefCell<Ui> = RefCell::new(Ui::default()); }

fn unavailable() -> Observation {
    // A timed-out native owner must retire prior evidence, not return an older
    // sequence that routing would discard. The owner adopts this generation on recovery.
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    Observation {
        stamp: ObservationStamp {
            generation,
            sequence: 1,
        },
        foreground: Foreground::Unavailable,
        hidden_lost: false,
        park_operation: None,
    }
}

/// AX is consulted only for an ordinary regular app. Failed fullscreen
/// classification exempts that app from parking; it does not prevent K=P entry.
fn foreground(ui: &Ui, mtm: MainThreadMarker) -> (Foreground, i32, Option<Owned>) {
    if !super::session::idle_refresh_allowed() || super::system::secure_input_active() {
        return (Foreground::Unavailable, 0, None);
    }
    let Some(front) = NSWorkspace::sharedWorkspace().frontmostApplication() else {
        return (Foreground::Unknown, 0, None);
    };
    let pid = front.processIdentifier();
    let app = NSApplication::sharedApplication(mtm);
    if pid == std::process::id() as i32 && app.isActive() && ui.window.as_ref().is_some_and(|w| w.isKeyWindow()) {
        return (Foreground::Hidden, pid, None);
    }
    if front.activationPolicy() != NSApplicationActivationPolicy::Regular {
        // Dock, switchers and transient system UI do not constitute an ordinary
        // foreground application. No bundle IDs or app names leave this owner.
        return (Foreground::Unknown, pid, None);
    }
    // SAFETY: AX objects are checked for null/type and held by Owned for every call.
    unsafe {
        if AXIsProcessTrusted() == 0 {
            return (Foreground::OrdinaryUnknown, pid, None);
        }
        let raw = AXUIElementCreateApplication(pid);
        if raw.is_null() {
            return (Foreground::OrdinaryUnknown, pid, None);
        }
        let application = Owned(raw);
        AXUIElementSetMessagingTimeout(application.0, 0.025);
        let Some(window) = attribute(&application, "AXFocusedWindow") else {
            return (Foreground::OrdinaryUnknown, pid, None);
        };
        if CFGetTypeID(window.0) != AXUIElementGetTypeID() {
            return (Foreground::OrdinaryUnknown, pid, None);
        }
        AXUIElementSetMessagingTimeout(window.0, 0.025);
        let state = attribute(&window, "AXFullScreen")
            .and_then(|v| (CFGetTypeID(v.0) == CFBooleanGetTypeID()).then(|| CFBooleanGetValue(v.0) != 0));
        (
            match state {
                Some(true) => Foreground::Fullscreen,
                Some(false) => Foreground::Windowed,
                None => Foreground::OrdinaryUnknown,
            },
            pid,
            Some(window),
        )
    }
}
fn refresh(ui: &mut Ui, mtm: MainThreadMarker, operation: Option<u64>) -> Observation {
    let generation = GENERATION.load(Ordering::Acquire);
    let recovered = ui.current.stamp.generation != generation;
    if recovered {
        ui.current.stamp = ObservationStamp {
            generation,
            sequence: 1,
        };
    }
    let (foreground, pid, window) = foreground(ui, mtm);
    let operation = operation.or_else(|| {
        ui.pending
            .as_ref()
            .filter(|p| p.permit.valid() && foreground == Foreground::Hidden)
            .map(|p| p.operation)
    });
    let same_window = match (&ui.focused_window, &window) {
        (None, None) => true,
        // SAFETY: both references remain retained throughout this comparison.
        (Some(a), Some(b)) => unsafe { CFEqual(a.0, b.0) != 0 },
        _ => false,
    };
    if recovered
        || foreground != ui.current.foreground
        || pid != ui.pid
        || !same_window
        || (operation.is_some() && operation != ui.current.park_operation)
    {
        ui.current.stamp.sequence = ui.current.stamp.sequence.saturating_add(1);
        ui.current.hidden_lost |= ui.current.foreground == Foreground::Hidden && foreground != Foreground::Hidden;
        if foreground == Foreground::Hidden {
            ui.current.hidden_lost = false;
        }
        ui.current.foreground = foreground;
        ui.current.park_operation = operation;
        ui.pid = pid;
        ui.focused_window = window;
    }
    ui.current
}
fn observe_notification() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let notify = UI.with(|slot| {
        let mut ui = slot.borrow_mut();
        let old = ui.current;
        let next = refresh(&mut ui, mtm, None);
        (next != old)
            .then(|| ui.observer.clone().map(|observer| (observer, next)))
            .flatten()
    });
    if let Some((observer, value)) = notify {
        observer(value);
    }
}
fn poll() {
    let again = UI.with(|slot| {
        let mut ui = slot.borrow_mut();
        ui.polling = ui.observer.is_some();
        ui.polling
    });
    if again {
        observe_notification();
        main_queue::run_after(POLL, poll);
    }
}
pub(super) fn watch(observer: Option<Observer>) {
    main_queue::run_async(move || {
        let start = UI.with(|slot| {
            let mut ui = slot.borrow_mut();
            ui.observer = observer;
            let start = ui.observer.is_some() && !ui.polling;
            if start {
                ui.polling = true;
            }
            start
        });
        if start {
            main_queue::run_after(POLL, poll);
        }
    });
}
pub(super) fn sample() -> Observation {
    main_queue::run_blocking(WAIT, || {
        let mtm = MainThreadMarker::new().expect("AppKit main queue");
        UI.with(|slot| {
            let mut ui = slot.borrow_mut();
            // The claim travels separately from the native activation stamp.
            // Readiness and authority can both sample an unchanged foreground;
            // incrementing here invalidates the first sample's fenced park.
            refresh(&mut ui, mtm, None)
        })
    })
    .unwrap_or_else(unavailable)
}
fn window(mtm: MainThreadMarker) -> Retained<SinkWindow> {
    let frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(1., 1.));
    // SAFETY: inherited NSWindow initializer and its exact signature on AppKit's owner.
    let window: Retained<SinkWindow> = unsafe {
        msg_send![SinkWindow::alloc(mtm), initWithContentRect: frame,
        styleMask: NSWindowStyleMask::Borderless, backing: NSBackingStoreType::Buffered, defer: false]
    };
    // SAFETY: inherited NSView initializer; MainThreadMarker confines allocation.
    let view: Retained<SinkView> = unsafe { msg_send![SinkView::alloc(mtm), initWithFrame: frame] };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setHasShadow(false);
    window.setIgnoresMouseEvents(true);
    // SAFETY: Retained exclusively manages ownership; close must not release it.
    unsafe {
        window.setReleasedWhenClosed(false);
    }
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    window.setContentView(Some(&view));
    window
}
// AppKit activation completes on a later run-loop turn. Never block that owner
// while waiting, and tag observer evidence with the in-flight operation.
fn finish_pending(ui: &mut Ui, result: ParkResult) {
    if let Some(pending) = ui.pending.take() {
        if result != ParkResult::Confirmed {
            pending.permit.revoke();
            if let Some(window) = &ui.window {
                window.orderOut(None);
            }
        }
        let _ = pending.reply.send(result);
    }
}
fn confirm_park() {
    let mtm = MainThreadMarker::new().expect("AppKit main queue");
    let again = UI.with(|slot| {
        let mut ui = slot.borrow_mut();
        let Some(pending) = &ui.pending else { return false };
        if !pending.permit.valid() {
            finish_pending(&mut ui, ParkResult::Stale);
            return false;
        }
        let expected = pending.expected;
        let current = refresh(&mut ui, mtm, None);
        let result = if current.foreground == Foreground::Hidden {
            Some(ParkResult::Confirmed)
        } else if current.foreground == Foreground::Unavailable {
            Some(ParkResult::Unavailable)
        } else if current.stamp != expected && ui.pid != std::process::id() as i32 {
            Some(ParkResult::Stale)
        } else {
            None
        };
        if let Some(result) = result {
            finish_pending(&mut ui, result);
            false
        } else {
            true
        }
    });
    if again && !main_queue::run_after(Duration::from_millis(5), confirm_park) {
        UI.with(|slot| finish_pending(&mut slot.borrow_mut(), ParkResult::Unavailable));
    }
}
pub(super) fn park(expected: ObservationStamp, operation: u64, permit: Permit) -> ParkResult {
    if main_queue::is_main_thread() {
        return ParkResult::Unavailable;
    }
    let queued = permit.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    main_queue::run_async(move || {
        let mtm = MainThreadMarker::new().expect("AppKit main queue");
        UI.with(|slot| {
            let mut ui = slot.borrow_mut();
            let current = refresh(&mut ui, mtm, None);
            let rejected = if !queued.valid() || current.stamp != expected || ui.pending.is_some() {
                Some(ParkResult::Stale)
            } else {
                match current.foreground {
                    Foreground::Fullscreen => Some(ParkResult::FullscreenExempt),
                    Foreground::Windowed => None,
                    Foreground::Unavailable => Some(ParkResult::Unavailable),
                    _ => Some(ParkResult::UnknownExempt),
                }
            };
            if let Some(result) = rejected {
                let _ = tx.send(result);
                return;
            }
            let window = ui.window.get_or_insert_with(|| window(mtm)).clone();
            if !queued.valid() {
                let _ = tx.send(ParkResult::Stale);
                return;
            }
            ui.pending = Some(Pending {
                operation,
                expected,
                permit: queued,
                reply: tx,
            });
            // One activation only; no saved-app restoration or fullscreen exit.
            window.makeKeyAndOrderFront(None);
            if let Some(view) = window.contentView() {
                window.makeFirstResponder(Some(&view));
            }
            #[allow(deprecated)]
            let accepted = NSRunningApplication::currentApplication()
                .activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
            if !accepted {
                finish_pending(&mut ui, ParkResult::Denied);
            }
        });
        confirm_park();
    });
    match rx.recv_timeout(WAIT) {
        Ok(result) => result,
        Err(_) => {
            permit.revoke();
            ParkResult::Unavailable
        }
    }
}
pub(super) fn dismiss() {
    main_queue::run_async(|| {
        UI.with(|slot| {
            let mut ui = slot.borrow_mut();
            finish_pending(&mut ui, ParkResult::Stale);
            if let Some(window) = ui.window.take() {
                window.orderOut(None);
            }
        })
    });
}
