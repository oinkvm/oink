//! Passive AppKit/WindowServer observations. Never activates, captures pixels,
//! asks for TCC access, or repairs a panel. Only aggregate counts leave here.
use super::{Event, Health, Kind};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationDidChangeScreenParametersNotification, NSPanel, NSScreen, NSWorkspace,
    NSWorkspaceActiveSpaceDidChangeNotification, NSWorkspaceSessionDidBecomeActiveNotification,
    NSWorkspaceSessionDidResignActiveNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol};
use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static SPACE: AtomicU64 = AtomicU64::new(0);
static DISPLAY: AtomicU64 = AtomicU64::new(0);
static SESSION: AtomicU64 = AtomicU64::new(0);
static SESSION_STATE: AtomicU64 = AtomicU64::new(0); // unknown / active / inactive

fn changed(reason: u8) {
    tracing::info!(target: "oink_overlay_diagnostics", overlay_event=Event::Environment as u8,
        overlay_reason=reason, overlay_space_epoch=SPACE.load(Ordering::Relaxed),
        overlay_display_epoch=DISPLAY.load(Ordering::Relaxed), overlay_session_epoch=SESSION.load(Ordering::Relaxed),
        overlay_session=SESSION_STATE.load(Ordering::Relaxed));
}
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkVisualDiagnosticsObserver"]
    struct Observer;
    unsafe impl NSObjectProtocol for Observer {}
    impl Observer {
        #[unsafe(method(spaceChanged:))]
        fn space(&self, _: &NSNotification) { SPACE.fetch_add(1, Ordering::Relaxed); changed(1); }
        #[unsafe(method(displayChanged:))]
        fn display(&self, _: &NSNotification) { DISPLAY.fetch_add(1, Ordering::Relaxed); changed(2); }
        #[unsafe(method(sessionActive:))]
        fn active(&self, _: &NSNotification) {
            SESSION.fetch_add(1, Ordering::Relaxed); SESSION_STATE.store(1, Ordering::Relaxed); changed(3);
        }
        #[unsafe(method(sessionInactive:))]
        fn inactive(&self, _: &NSNotification) {
            SESSION.fetch_add(1, Ordering::Relaxed); SESSION_STATE.store(2, Ordering::Relaxed); changed(4);
        }
    }
);
thread_local! { static OBSERVER: RefCell<Option<Retained<Observer>>> = const { RefCell::new(None) }; }
pub(crate) fn observe(mtm: MainThreadMarker) {
    OBSERVER.with(|slot| {
        if slot.borrow().is_some() {
            return;
        }
        // SAFETY: NSObject initialization; retained for the process lifetime.
        let observer: Retained<Observer> = unsafe { msg_send![Observer::alloc(mtm), init] };
        let workspace = NSWorkspace::sharedWorkspace().notificationCenter();
        // SAFETY: Valid selectors on the retained observer. Read only notifications
        // cover Space switches / fast user switching without examining any app.
        unsafe {
            NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
                &observer,
                sel!(displayChanged:),
                Some(NSApplicationDidChangeScreenParametersNotification),
                None,
            );
            for (name, selector) in [
                (NSWorkspaceActiveSpaceDidChangeNotification, sel!(spaceChanged:)),
                (NSWorkspaceSessionDidBecomeActiveNotification, sel!(sessionActive:)),
                (NSWorkspaceSessionDidResignActiveNotification, sel!(sessionInactive:)),
            ] {
                workspace.addObserver_selector_name_object(&observer, selector, Some(name), None);
            }
        }
        *slot.borrow_mut() = Some(observer);
        changed(0);
    });
}

pub(crate) fn visibility<'a>(
    h: &Health,
    owner: u64,
    generation: u64,
    panels: impl IntoIterator<Item = (&'a NSPanel, bool, bool)>,
    mtm: MainThreadMarker,
) {
    let panels: Vec<_> = panels.into_iter().collect();
    let ids: Vec<_> = panels.iter().map(|(p, _, _)| p.windowNumber() as i32).collect();
    let onscreen = onscreen_count(&ids);
    tracing::info!(target: "oink_overlay_diagnostics", overlay_object=h.id, overlay_kind=h.kind as u8,
        overlay_event=Event::Visibility as u8, overlay_native_owner=owner, overlay_generation=generation,
        overlay_windows=panels.len() as u64, overlay_cached_shown=panels.iter().filter(|p| p.1).count() as u64,
        overlay_content=panels.iter().filter(|p| p.2).count() as u64,
        overlay_visible=panels.iter().filter(|p| p.0.isVisible()).count() as u64,
        overlay_on_space=panels.iter().filter(|p| p.0.isOnActiveSpace()).count() as u64,
        overlay_onscreen_known=onscreen.is_some(), overlay_onscreen=onscreen.unwrap_or(0) as u64,
        overlay_application_hidden=NSApplication::sharedApplication(mtm).isHidden(),
        overlay_screens=NSScreen::screens(mtm).len() as u64,
        overlay_space_epoch=SPACE.load(Ordering::Relaxed), overlay_display_epoch=DISPLAY.load(Ordering::Relaxed),
        overlay_session_epoch=SESSION.load(Ordering::Relaxed), overlay_session=SESSION_STATE.load(Ordering::Relaxed));
}

// CGWindowListCopyWindowInfo is a metadata query with no permission prompt.
// Read ONLY window numbers, match our own panels in memory, then discard the
// dictionary. No owner name, title, PID, ID, desktop content or Space ID is logged.
fn onscreen_count(ids: &[i32]) -> Option<usize> {
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative: u32) -> *const c_void;
        static kCGWindowNumber: *const c_void;
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFDictionaryGetValue(dictionary: *const c_void, key: *const c_void) -> *const c_void;
        fn CFNumberGetValue(number: *const c_void, kind: isize, value: *mut i32) -> bool;
        fn CFRelease(value: *const c_void);
    }
    // SAFETY: Copy returns a retained CFArray of documented CFDictionary rows;
    // kCGWindowNumber is a CFNumber, converted to signed 32 bits. Release once.
    unsafe {
        let array = CGWindowListCopyWindowInfo(1, 0); // onscreen only
        if array.is_null() {
            return None;
        }
        let mut count = 0;
        for index in 0..CFArrayGetCount(array) {
            let row = CFArrayGetValueAtIndex(array, index);
            let number = CFDictionaryGetValue(row, kCGWindowNumber);
            let mut id = 0;
            if !number.is_null() && CFNumberGetValue(number, 3, &mut id) && ids.contains(&id) {
                count += 1;
            }
        }
        CFRelease(array);
        Some(count)
    }
}

pub(super) fn sample(h: Arc<Health>) {
    if h.sampling.swap(true, Ordering::AcqRel) {
        return;
    }
    let ticket = h.ticket();
    struct Sample(Arc<Health>);
    impl Drop for Sample {
        fn drop(&mut self) {
            self.0.sampling.store(false, Ordering::Release);
        }
    }
    let job: Box<Box<dyn FnOnce() + Send>> = Box::new(Box::new(move || {
        let _sample = Sample(h.clone());
        ticket.start();
        if let Some(mtm) = MainThreadMarker::new() {
            observe(mtm);
            match h.kind {
                Kind::Trail => crate::macos::diagnose(&h, mtm),
                Kind::Projection => crate::projection::diagnose(&h, mtm),
            }
        }
    }));
    #[repr(C)]
    struct DispatchQueue {
        _private: [u8; 0],
    }
    unsafe extern "C" {
        static _dispatch_main_q: DispatchQueue;
        fn dispatch_async_f(queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
    }
    extern "C" fn run(context: *mut c_void) {
        // SAFETY: Transferred once below; the closure owns its queue ticket.
        let job = unsafe { Box::from_raw(context.cast::<Box<dyn FnOnce() + Send>>()) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
    }
    // SAFETY: Process-lifetime main queue. Never wait on it during collection.
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, Box::into_raw(job).cast(), run) };
}
