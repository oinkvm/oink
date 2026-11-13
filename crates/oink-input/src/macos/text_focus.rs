//! macOS text-focus watcher (`crate::text_focus`, protocol §14.7).
//!
//! Why this shape: macOS has no system-wide "focused element changed"
//! notification; Accessibility delivers `AXFocusedUIElementChanged` only per
//! application (`AXObserverCreate(pid)`). So this thread follows the focused
//! application (the system-wide element's `AXFocusedApplication`, sampled
//! every [`APP_POLL`]), keeps one observer on it, and runs its own CFRunLoop
//! for the observer's source. A notification, or a change of focused
//! application, starts a settle window (`Settle`); then the system-wide
//! `AXFocusedUIElement` is read once and classified. Every AX call is a
//! cross-process request, so all of them run on this thread with a short
//! messaging timeout and never on the event-tap or router threads.
//!
//! Without Accessibility permission every read fails, nothing is focused, and
//! the watcher stays silent. Oink's own windows are not observed (their AX
//! requests would be served by this process's main thread). Electron apps that
//! keep their accessibility tree off until a screen reader asks are not
//! reported (known gap; Oink does not switch other apps' accessibility on).
use crate::text_focus::{FocusEdges, Report, Running, Settle, mac_editable, mac_secure, mac_text_role};
use objc2_core_foundation::CFString;
use std::{
    cell::Cell,
    ffi::c_void,
    time::{Duration, Instant},
};

type CfRef = *const c_void;
type ObserverCallback = extern "C" fn(observer: CfRef, element: CfRef, notification: CfRef, refcon: *mut c_void);

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> CfRef;
    fn AXUIElementCreateApplication(pid: i32) -> CfRef;
    fn AXUIElementCopyAttributeValue(el: CfRef, attr: CfRef, value: *mut CfRef) -> i32;
    fn AXUIElementIsAttributeSettable(el: CfRef, attr: CfRef, settable: *mut u8) -> i32;
    fn AXUIElementSetMessagingTimeout(el: CfRef, timeout: f32) -> i32;
    fn AXUIElementGetPid(el: CfRef, pid: *mut i32) -> i32;
    fn AXUIElementGetTypeID() -> usize;
    fn AXObserverCreate(pid: i32, callback: ObserverCallback, out: *mut CfRef) -> i32;
    fn AXObserverAddNotification(observer: CfRef, element: CfRef, notification: CfRef, refcon: *mut c_void) -> i32;
    fn AXObserverRemoveNotification(observer: CfRef, element: CfRef, notification: CfRef) -> i32;
    fn AXObserverGetRunLoopSource(observer: CfRef) -> CfRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    #[allow(non_upper_case_globals)]
    static kCFRunLoopDefaultMode: CfRef;
    fn CFRunLoopGetCurrent() -> CfRef;
    fn CFRunLoopAddSource(run_loop: CfRef, source: CfRef, mode: CfRef);
    fn CFRunLoopRemoveSource(run_loop: CfRef, source: CfRef, mode: CfRef);
    fn CFRunLoopRunInMode(mode: CfRef, seconds: f64, return_after_source_handled: u8) -> i32;
    fn CFRelease(value: CfRef);
    fn CFEqual(a: CfRef, b: CfRef) -> u8;
    fn CFGetTypeID(value: CfRef) -> usize;
    fn CFStringGetTypeID() -> usize;
}

/// `kCFRunLoopRunFinished`: the mode has no sources (no observer yet).
const RUN_FINISHED: i32 = 1;
/// How often the focused application is sampled.
const APP_POLL: Duration = Duration::from_millis(250);
/// Per-request AX timeout: a hung application must not stall the watcher.
const MESSAGING_TIMEOUT: f32 = 0.25;
const FOCUS_CHANGED: &str = "AXFocusedUIElementChanged";

thread_local! {
    /// Set by the observer callback, which runs inside this thread's run loop.
    static NOTIFIED: Cell<bool> = const { Cell::new(false) };
}

extern "C" fn focus_changed(_observer: CfRef, _element: CfRef, _notification: CfRef, _refcon: *mut c_void) {
    NOTIFIED.set(true);
}

/// An owned (Create/Copy) CoreFoundation reference, never null: build it only
/// through `Owned::new`. CFRelease(NULL) traps, so a wrapper must not even be
/// created (and dropped) for a null result: a failed AX read leaves `out` null
/// (integration 2026-11-12: `then_some(Self(value))` built and dropped one).
struct Owned(CfRef);
impl Owned {
    fn new(value: CfRef) -> Option<Self> {
        if value.is_null() { None } else { Some(Self(value)) }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        debug_assert!(!self.0.is_null(), "Owned must never hold NULL");
        // SAFETY: only constructed by `new` from a non-null Create/Copy result, released once.
        unsafe { CFRelease(self.0) }
    }
}

fn key(name: &str) -> objc2_core_foundation::CFRetained<CFString> {
    CFString::from_str(name)
}

fn key_ref(key: &CFString) -> CfRef {
    (key as *const CFString).cast()
}

fn copy_attr(element: CfRef, name: &str) -> Option<Owned> {
    let name = key(name);
    let mut out = std::ptr::null();
    // SAFETY: element is a live AX element; the key and out pointer outlive the call.
    let code = unsafe { AXUIElementCopyAttributeValue(element, key_ref(&name), &mut out) };
    // A non-null result is ours to release even if the call reported an error.
    let value = Owned::new(out);
    if code == 0 { value } else { None }
}

fn string_attr(element: CfRef, name: &str) -> Option<String> {
    let value = copy_attr(element, name)?;
    // SAFETY: the owned value's type is checked before it is read as a CFString.
    unsafe {
        if CFGetTypeID(value.0) != CFStringGetTypeID() {
            return None;
        }
        Some((*value.0.cast::<CFString>()).to_string())
    }
}

fn settable(element: CfRef, name: &str) -> bool {
    let name = key(name);
    let mut settable = 0u8;
    // SAFETY: element is a live AX element; key and out pointer outlive the call.
    let code = unsafe { AXUIElementIsAttributeSettable(element, key_ref(&name), &mut settable) };
    code == 0 && settable != 0
}

fn pid_of(element: CfRef) -> Option<i32> {
    let mut pid = 0;
    // SAFETY: element is a live AX element; pid is writable.
    (unsafe { AXUIElementGetPid(element, &mut pid) } == 0 && pid > 0).then_some(pid)
}

/// The system-wide focused element, unless it is in this process.
fn focused_element(system: &Owned) -> Option<Owned> {
    let element = copy_attr(system.0, "AXFocusedUIElement")?;
    // SAFETY: CF type check of an owned value.
    if unsafe { CFGetTypeID(element.0) != AXUIElementGetTypeID() } {
        return None;
    }
    if pid_of(element.0) == Some(std::process::id() as i32) {
        return None;
    }
    Some(element)
}

fn focused_application(system: &Owned) -> Option<i32> {
    let app = copy_attr(system.0, "AXFocusedApplication")?;
    // SAFETY: CF type check of an owned value.
    if unsafe { CFGetTypeID(app.0) != AXUIElementGetTypeID() } {
        return None;
    }
    pid_of(app.0).filter(|pid| *pid != std::process::id() as i32)
}

/// (editable, password field) of the focused element.
fn classify(element: &Owned) -> (bool, bool) {
    let role = string_attr(element.0, "AXRole");
    let subrole = string_attr(element.0, "AXSubrole");
    let secure = mac_secure(subrole.as_deref());
    if mac_text_role(role.as_deref(), subrole.as_deref()) {
        return (true, secure);
    }
    let editable = mac_editable(
        role.as_deref(),
        subrole.as_deref(),
        settable(element.0, "AXValue"),
        copy_attr(element.0, "AXSelectedTextRange").is_some(),
    );
    (editable, secure)
}

fn same(a: Option<&Owned>, b: Option<&Owned>) -> bool {
    match (a, b) {
        (None, None) => true,
        // SAFETY: both are live AX elements.
        (Some(a), Some(b)) => unsafe { CFEqual(a.0, b.0) != 0 },
        _ => false,
    }
}

/// `AXFocusedUIElementChanged` on one application, its source on this thread's run loop.
struct AppObserver {
    pid: i32,
    app: Owned,
    observer: Owned,
    /// Borrowed from `observer` (Get rule).
    source: CfRef,
}

impl AppObserver {
    fn new(pid: i32) -> Option<Self> {
        let mut observer = std::ptr::null();
        // SAFETY: plain C callback without captured state; out pointer is writable.
        let code = unsafe { AXObserverCreate(pid, focus_changed, &mut observer) };
        if code != 0 {
            return None;
        }
        let observer = Owned::new(observer)?;
        // SAFETY: creates an owned element for a process id.
        let app = Owned::new(unsafe { AXUIElementCreateApplication(pid) })?;
        // SAFETY: live element owned above.
        unsafe { AXUIElementSetMessagingTimeout(app.0, MESSAGING_TIMEOUT) };
        let note = key(FOCUS_CHANGED);
        // SAFETY: live observer and element; the notification name outlives the call.
        let code = unsafe { AXObserverAddNotification(observer.0, app.0, key_ref(&note), std::ptr::null_mut()) };
        if code != 0 {
            tracing::debug!(code, "text focus: application does not post focus notifications");
            return None;
        }
        // SAFETY: the observer owns the source; it is removed before the observer is released.
        let source = unsafe { AXObserverGetRunLoopSource(observer.0) };
        if source.is_null() {
            return None;
        }
        // SAFETY: this thread's run loop, a live source, the exported default mode.
        unsafe { CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode) };
        Some(Self {
            pid,
            app,
            observer,
            source,
        })
    }
}

impl Drop for AppObserver {
    fn drop(&mut self) {
        let note = key(FOCUS_CHANGED);
        // SAFETY: on the thread that added the source; observer and element still owned.
        unsafe {
            CFRunLoopRemoveSource(CFRunLoopGetCurrent(), self.source, kCFRunLoopDefaultMode);
            AXObserverRemoveNotification(self.observer.0, self.app.0, key_ref(&note));
        }
    }
}

pub(crate) fn spawn(report: Report, running: Running) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("oink-text-focus".into())
        .spawn(move || watch(&*report, &*running))
        .map_err(|error| tracing::warn!(%error, "text focus watcher unavailable"))
        .ok()
}

fn watch(report: &dyn Fn(crate::text_focus::TextFocusReport), running: &dyn Fn() -> bool) {
    // SAFETY: creates an owned system-wide element.
    let Some(system) = Owned::new(unsafe { AXUIElementCreateSystemWide() }) else {
        tracing::warn!("text focus: no system-wide accessibility element");
        return;
    };
    // SAFETY: live element; on the system-wide element this is the default for this process's requests.
    unsafe { AXUIElementSetMessagingTimeout(system.0, MESSAGING_TIMEOUT) };
    tracing::info!("text focus watcher started");
    let mut observed: Option<AppObserver> = None;
    let mut last: Option<Owned> = None;
    let mut edges = FocusEdges::default();
    let mut settle = Settle::default();
    let mut next_app = Instant::now();
    // The element focused at start is the first report.
    settle.event(Instant::now());
    while running() {
        let now = Instant::now();
        if now >= next_app {
            next_app = now + APP_POLL;
            let pid = focused_application(&system);
            if pid != observed.as_ref().map(|o| o.pid) {
                // The old observer's source leaves the run loop before the new one joins.
                drop(observed.take());
                observed = pid.and_then(AppObserver::new);
                settle.event(now);
            }
        }
        if settle.take_due(now) {
            let element = focused_element(&system);
            let changed = !same(element.as_ref(), last.as_ref());
            let (editable, secure) = element.as_ref().map_or((false, false), classify);
            if let Some(next) = edges.observe(changed, editable, secure) {
                tracing::debug!(
                    seq = next.seq,
                    editable = next.editable,
                    secure = next.secure,
                    "text focus changed"
                );
                report(next);
            }
            last = element;
        }
        let wait = settle
            .wait(now, next_app.saturating_duration_since(now))
            .max(Duration::from_millis(1));
        // SAFETY: runs this thread's own run loop in the exported default mode;
        // returns after the observer callback or the timeout.
        let result = unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, wait.as_secs_f64(), 1) };
        if result == RUN_FINISHED {
            // No observer source yet: the run loop returns at once, so wait here.
            std::thread::sleep(wait);
        }
        if NOTIFIED.replace(false) {
            settle.event(Instant::now());
        }
    }
    drop(observed);
    tracing::info!("text focus watcher stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_reference_is_never_wrapped_or_released() {
        // Dropping a wrapper around NULL would call CFRelease(NULL) and trap.
        assert!(Owned::new(std::ptr::null()).is_none());
    }

    #[test]
    fn a_failed_attribute_read_returns_none_without_releasing() {
        // No such process: every AX read fails (or is refused without
        // Accessibility permission) and leaves the out pointer null.
        // SAFETY: creates an owned element for a process id.
        let app = Owned::new(unsafe { AXUIElementCreateApplication(i32::MAX) }).expect("element");
        // SAFETY: live element owned above.
        unsafe { AXUIElementSetMessagingTimeout(app.0, MESSAGING_TIMEOUT) };
        assert!(copy_attr(app.0, "AXFocusedUIElement").is_none());
        assert!(string_attr(app.0, "AXRole").is_none());
        assert!(!settable(app.0, "AXValue"));
    }
}
