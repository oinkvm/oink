//! Hold a transparent cursor in a nonactivating window while the source is frozen.
//!
//! A Dock-owned cursor ignores our background hide request. Moving that visible cursor out of
//! the Dock exposes the freeze point. A tiny alpha-backed panel takes pointer ownership at the
//! original position first; its cursor rectangle supplies an all-transparent bitmap. Only after
//! its marked same-position event is consumed and the main thread verifies ownership may the
//! tap move to the freeze point.
//! The panel stays until Local. No polling/repeated hide and no global cursor preference changes.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSBitmapImageRep, NSColor, NSCursor, NSDeviceRGBColorSpace, NSEvent, NSImage,
    NSPanel, NSPopUpMenuWindowLevel, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_graphics::{CGDisplayBounds, CGEvent, CGEventField, CGMainDisplayID};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize};
use oink_foundation::Point;
use oink_foundation::handoff_timing::{self, Stage as TimingStage};

use super::super::main_queue;
use super::super::tap::Shared;

const EVENT_PREFIX: u64 = 0x4F49_4E00_0000_0000;
const GENERATION_MASK: u64 = 0x0000_00FF_FFFF_FFFF;
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
static WINDOW: AtomicU32 = AtomicU32::new(0);
static WINDOW_GENERATION: AtomicU64 = AtomicU64::new(0);
static BLOCKED_WINDOW: AtomicU32 = AtomicU32::new(0);

pub(crate) fn blocks_local_mouse() -> bool {
    BLOCKED_WINDOW.load(Ordering::Acquire) != 0
}

static CONNECTION: AtomicI32 = AtomicI32::new(0);
// Last globally observed seed after setting our transparent glyph, under cursor_lock.
static BLANK_SEED: AtomicI32 = AtomicI32::new(-1);

pub(crate) fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::AcqRel) & GENERATION_MASK
}

pub(crate) fn event_tag(generation: u64) -> i64 {
    (EVENT_PREFIX | (generation & GENERATION_MASK)) as i64
}

pub(crate) fn event_generation(tag: i64) -> Option<u64> {
    let tag = tag as u64;
    (tag & !GENERATION_MASK == EVENT_PREFIX).then_some(tag & GENERATION_MASK)
}

fn active(generation: u64) -> bool {
    WINDOW.load(Ordering::Acquire) != 0 && WINDOW_GENERATION.load(Ordering::Acquire) == generation
}

type ConnectionFn = unsafe extern "C" fn() -> i32;
type OrderFn = unsafe extern "C" fn(i32, u32, i32, u32) -> i32;
type EventsFn = unsafe extern "C" fn(i32, u32, bool) -> i32;
type TagsFn = unsafe extern "C" fn(i32, u32, *const u32, usize) -> i32;
type CursorSeedFn = unsafe extern "C" fn() -> i32;
type SystemCursorFn = unsafe extern "C" fn(i32, isize, *mut i32);

struct WindowApi {
    connection: ConnectionFn,
    order: OrderFn,
    events: EventsFn,
    tags: TagsFn,
    cursor_seed: CursorSeedFn,
    system_cursor: SystemCursorFn,
}

fn symbol(name: &CStr) -> *mut c_void {
    // SAFETY: RTLD_DEFAULT and a NUL-terminated symbol name; no symbol is invoked here.
    unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) }
}

static API: OnceLock<Option<WindowApi>> = OnceLock::new();

fn window_api() -> Option<&'static WindowApi> {
    API.get_or_init(|| {
        let connection = symbol(c"_CGSDefaultConnection");
        let order = symbol(c"CGSOrderWindow");
        let events = symbol(c"CGSSetMouseEventEnableFlags");
        let tags = symbol(c"CGSSetWindowTags");
        let cursor_seed = symbol(c"CGSCurrentCursorSeed");
        let system_cursor = symbol(c"CGSSetSystemDefinedCursorWithSeed");
        if connection.is_null()
            || order.is_null()
            || events.is_null()
            || tags.is_null()
            || cursor_seed.is_null()
            || system_cursor.is_null()
        {
            return None;
        }
        // SAFETY: Signatures from NUIKit/CGSInternal CGSWindow.h and CGSCursor.h. Operations affect only our own
        // panel. The native probe checks that order-out's reply precedes loss of hit testing.
        Some(unsafe {
            WindowApi {
                connection: std::mem::transmute::<*mut c_void, ConnectionFn>(connection),
                order: std::mem::transmute::<*mut c_void, OrderFn>(order),
                events: std::mem::transmute::<*mut c_void, EventsFn>(events),
                tags: std::mem::transmute::<*mut c_void, TagsFn>(tags),
                cursor_seed: std::mem::transmute::<*mut c_void, CursorSeedFn>(cursor_seed),
                system_cursor: std::mem::transmute::<*mut c_void, SystemCursorFn>(system_cursor),
            }
        })
    })
    .as_ref()
}

/// Retire our transparent connection cursor at the restored entry point, before showing it.
/// Ordering the panel out alone does not reset its glyph: macOS 14 can leave it globally
/// transparent for 130 ms even though ShowCursor succeeded. Warp FIRST: the blank glyph also
/// protects against Dock overriding our hide, so retiring it at the freeze point could flash.
/// The seed check leaves a destination application's already-selected glyph alone. No AppKit
/// cleanup handshake is needed. Normal calls hold cursor_lock after invalidating owner work;
/// the exit path uses only these already-resolved native APIs and atomics.
pub(crate) fn retire_blank() -> bool {
    let expected = BLANK_SEED.swap(-1, Ordering::AcqRel);
    if WINDOW.load(Ordering::Acquire) == 0 || expected == -1 {
        return false;
    }
    if let Some(Some(api)) = API.get() {
        // SAFETY: CGSCursor.h signatures. Only our live connection's cursor is changed, and only
        // if the global seed still identifies our blank glyph. Cursor ID 0 is the system arrow.
        unsafe {
            if (api.cursor_seed)() == expected {
                let mut seed = 0;
                (api.system_cursor)(CONNECTION.load(Ordering::Acquire), 0, &mut seed);
                return true;
            }
        }
    }
    false
}

fn set_blank(cursor: &NSCursor) {
    cursor.set();
    if let Some(Some(api)) = API.get() {
        // SAFETY: Read-only query following the synchronous set, under cursor_lock.
        BLANK_SEED.store(unsafe { (api.cursor_seed)() }, Ordering::Release);
    }
}

/// Signal/exit path: use only already-resolved native functions, no AppKit, locks or allocation.
pub(crate) fn release_emergency() {
    retire_blank();
    let window = WINDOW.load(Ordering::Acquire);
    if window != 0
        && let Some(Some(api)) = API.get()
    {
        let connection = CONNECTION.load(Ordering::Acquire);
        // SAFETY: IDs belong to the retained owner window; these are synchronous native calls.
        unsafe {
            (api.events)(connection, window, false);
            (api.order)(connection, window, 0, 0);
        }
    }
}

/// Called outside the event-tap callback, after the Local warp and before input is released.
/// WindowServer stops routing to the panel synchronously; AppKit object cleanup is asynchronous.
pub(crate) fn release() {
    retire_blank();
    let window = WINDOW.swap(0, Ordering::AcqRel);
    if window == 0 {
        return;
    }
    if let Some(api) = window_api() {
        let connection = CONNECTION.load(Ordering::Acquire);
        // SAFETY: Our connection/window IDs remain live until the main-thread cleanup below.
        let (events, order) = unsafe {
            (
                (api.events)(connection, window, false),
                (api.order)(connection, window, 0, 0),
            )
        };
        if events != 0 && order != 0 {
            BLOCKED_WINDOW.store(window, Ordering::Release);
            tracing::warn!(window, events, order, "cursor owner: native order-out failed");
        }
    }
    let timing_scope = handoff_timing::scope();
    handoff_timing::native(TimingStage::CleanupQueued, 0);
    main_queue::run_after(Duration::ZERO, move || {
        let _scope = handoff_timing::enter_scope(timing_scope);
        handoff_timing::native(TimingStage::CleanupStarted, 0);
        tracing::debug!(target: "oink_cursor_timing", window, "cursor latency: AppKit cleanup begin");
        UI.with(|ui| {
            let mut ui = ui.borrow_mut();
            if ui.as_ref().is_some_and(|u| u.panel.windowNumber() as u32 == window)
                && let Some(old) = ui.take()
            {
                old.view.ivars().shared.owner_cleanup(|local| {
                    old.panel.orderOut(None);
                    // Reset only our own stale application cursor over our own window. Never
                    // set a global arrow over another application's I-beam/resize cursor.
                    let mtm = old.panel.mtm();
                    let hit =
                        NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(NSEvent::mouseLocation(), 0, mtm);
                    if local
                        && WINDOW.load(Ordering::Acquire) == 0
                        && std::ptr::eq(&*NSCursor::currentCursor(), &*old.view.ivars().cursor)
                        && let Some(window) = NSApplication::sharedApplication(mtm).windowWithWindowNumber(hit)
                    {
                        NSCursor::arrowCursor().set();
                        window.resetCursorRects();
                    }
                });
            }
            tracing::debug!(target: "oink_cursor_timing", window, "cursor latency: AppKit cleanup end");
            let _ = BLOCKED_WINDOW.compare_exchange(window, 0, Ordering::AcqRel, Ordering::Acquire);
        });
        handoff_timing::native(TimingStage::CleanupCompleted, 0);
    });
}

struct Ui {
    panel: Retained<OwnerPanel>,
    view: Retained<OwnerView>,
    from: Point,
    generation: u64,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkCursorOwnerPanel"]
    struct OwnerPanel;

    impl OwnerPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool { false }
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool { false }
    }
    unsafe impl NSObjectProtocol for OwnerPanel {}
);

struct ViewIvars {
    shared: &'static Shared,
    generation: u64,
    cursor: Retained<NSCursor>,
    ready: Cell<bool>,
    to: Point,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkCursorOwnerView"]
    #[ivars = ViewIvars]
    struct OwnerView;

    impl OwnerView {
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            // Registering the rectangle does not globally set the cursor. AppKit may call this
            // synchronously from orderFront, while prepare holds the cursor lock.
            self.addCursorRect_cursor(self.bounds(), &self.ivars().cursor);
        }
        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _event: &NSEvent) { self.set_blank(); }
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) { self.set_blank(); }
        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.set_blank();
            let ivars = self.ivars();
            if let Some(event) = event.CGEvent()
                && event_generation(CGEvent::integer_value_field(Some(&event), CGEventField::EventSourceUserData)) == Some(ivars.generation)
            {
                // Always enqueue: no nested cursor-lock acquisition from an AppKit call made
                // by prepare. The tap has already registered this same-position HID correction.
                let shared = ivars.shared;
                let generation = ivars.generation;
                main_queue::run_after(Duration::ZERO, move || finish_marked_move(shared, generation));
            }
        }
    }
    unsafe impl NSObjectProtocol for OwnerView {}
);

impl OwnerView {
    fn set_blank(&self) {
        let ivars = self.ivars();
        ivars.shared.owner_try_update(ivars.generation, || {
            if active(ivars.generation) {
                set_blank(&ivars.cursor);
            }
        });
    }
}

/// Regular background NSApplications need not deliver mouseMoved to a non-key panel. The
/// session tap acknowledgement is the reliable event boundary; finish on the main queue after
/// rechecking WindowServer ownership. Native AppKit callbacks still refresh the blank glyph.
pub(crate) fn event_consumed(shared: &'static Shared, generation: u64) {
    handoff_timing::with_scope(shared.timing_scope(), || {
        handoff_timing::native(TimingStage::OwnerMarkerConsumed, 0);
    });
    main_queue::run_after(Duration::ZERO, move || finish_marked_move(shared, generation));
}

fn finish_marked_move(shared: &'static Shared, generation: u64) {
    let _scope = handoff_timing::enter_scope(shared.timing_scope());
    shared.owner_update(generation, || {
        UI.with(|ui| {
            let ui = ui.borrow();
            let Some(ui) = ui.as_ref().filter(|u| u.generation == generation && active(generation)) else {
                return;
            };
            let hit =
                NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(NSEvent::mouseLocation(), 0, ui.panel.mtm());
            if hit != ui.panel.windowNumber() {
                tracing::debug!(
                    generation,
                    hit,
                    "cursor owner: acknowledged event no longer hits owner; frozen in place"
                );
                return;
            }
            let ivars = ui.view.ivars();
            set_blank(&ivars.cursor);
            if ivars.ready.replace(true) {
                return;
            }
            handoff_timing::native(TimingStage::OwnerReady, 0);
            super::hide_owned();
            tracing::debug!(generation, from = ?ui.from, to = ?ivars.to, "cursor owner: blank glyph ready");
            shared.owner_ready(generation, ivars.to);
        });
    });
}

fn blank_cursor(_mtm: MainThreadMarker) -> Option<Retained<NSCursor>> {
    // SAFETY: Null planes requests AppKit-owned storage, exactly 16x16 RGBA bytes below.
    let bitmap = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(), std::ptr::null_mut(), 16, 16, 8, 4, true, false,
            NSDeviceRGBColorSpace, 64, 32,
        )?
    };
    let data = bitmap.bitmapData();
    if data.is_null() {
        return None;
    }
    // SAFETY: The representation above allocates 16 rows of 64 bytes. Keep it alive while writing.
    unsafe { std::ptr::write_bytes(data, 0, 16 * 64) };
    let image = NSImage::initWithSize(NSImage::alloc(), NSSize::new(16.0, 16.0));
    image.addRepresentation(&bitmap);
    Some(NSCursor::initWithImage_hotSpot(
        NSCursor::alloc(),
        &image,
        NSPoint::ZERO,
    ))
}

/// No NSApplication event loop (plain CFRunLoop CLI), no window APIs, or a timed-out hit test:
/// keep the cursor frozen at its original point. Never relocate before ownership is established.
pub(crate) fn prepare(shared: &'static Shared, generation: u64, from: Point, to: Point) {
    let scope = shared.timing_scope();
    handoff_timing::with_scope(scope, || handoff_timing::native(TimingStage::OwnerPrepareQueued, 0));
    main_queue::run_after(Duration::ZERO, move || {
        let _scope = handoff_timing::enter_scope(scope);
        handoff_timing::native(TimingStage::OwnerPrepareStarted, 0);
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        unsafe extern "C" {
            static NSApp: *mut NSApplication;
        }
        // SAFETY: NSApp is only read on the main thread; don't initialize AppKit in a CLI.
        let app = unsafe { NSApp.as_ref() };
        let Some(api) = window_api().filter(|_| app.is_some_and(NSApplication::isRunning)) else {
            tracing::warn!(
                generation,
                "cursor owner: AppKit event loop unavailable; frozen in place"
            );
            return;
        };
        let existing = shared.owner_update(generation, || {
            UI.with(|ui| {
                let ui = ui.borrow();
                if let Some(ui) = ui.as_ref().filter(|u| u.generation == generation && active(generation)) {
                    if ui.view.ivars().ready.get() {
                        shared.owner_ready(generation, ui.view.ivars().to);
                    }
                    true
                } else {
                    false
                }
            })
        });
        if existing == Some(true) {
            wait_for_hit(shared, generation, Instant::now());
            return;
        }
        if existing.is_none() {
            return;
        }
        let Some(cursor) = blank_cursor(mtm) else {
            tracing::warn!(
                generation,
                "cursor owner: cannot create transparent glyph; frozen in place"
            );
            return;
        };
        let height = CGDisplayBounds(CGMainDisplayID()).size.height;
        let frame = NSRect::new(
            NSPoint::new(from.x.min(to.x) - 8.0, height - from.y.max(to.y) - 8.0),
            NSSize::new((from.x - to.x).abs() + 16.0, (from.y - to.y).abs() + 16.0),
        );
        let this = OwnerView::alloc(mtm).set_ivars(ViewIvars {
            shared,
            generation,
            cursor,
            ready: Cell::new(false),
            to,
        });
        // SAFETY: NSView designated initializer, supplied ivars already initialized.
        let view: Retained<OwnerView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::new(NSPoint::ZERO, frame.size)] };
        // SAFETY: The owner view outlives its tracking area. userInfo is unused.
        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::CursorUpdate
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&tracking);
        // SAFETY: NSPanel designated initializer for our main-thread-only subclass.
        let panel: Retained<OwnerPanel> = unsafe {
            msg_send![OwnerPanel::alloc(mtm), initWithContentRect: frame,
                styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                backing: NSBackingStoreType::Buffered, defer: false]
        };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.0, 0.01)));
        panel.setHasShadow(false);
        panel.setIgnoresMouseEvents(false);
        let policy = oink_foundation::panel_policy::PanelPolicy::BACKEND_OVERLAY;
        panel.setHidesOnDeactivate(policy.hides_on_deactivate);
        // Cursor ownership must survive Hide Others hiding the accessory backend;
        // hidesOnDeactivate only handles losing foreground activation.
        panel.setCanHide(policy.can_hide);
        panel.setFloatingPanel(true);
        panel.setLevel(NSPopUpMenuWindowLevel);
        panel.setBecomesKeyOnlyIfNeeded(true);
        // SAFETY: We retain it in UI; orderOut must not release the Objective-C instance.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        panel.setAcceptsMouseMovedEvents(true);
        panel.setContentView(Some(&view));
        shared.owner_update(generation, || {
            release();
            // SAFETY: Native functions loaded above. The tag affects only this retained panel:
            // kCGSSetsCursorInBackgroundTagBit, CGSWindow.h; maxTagSize is bits (64).
            let connection = unsafe { (api.connection)() };
            let tags = [1u32 << 5, 0];
            // SAFETY: two initialized u32 words, with a valid native connection/window ID.
            let tag_result = unsafe { (api.tags)(connection, panel.windowNumber() as u32, tags.as_ptr(), 64) };
            tracing::debug!(generation, tag_result, "cursor owner: enabled background window cursor");
            if tag_result != 0 {
                return;
            }
            panel.orderFrontRegardless();
            CONNECTION.store(connection, Ordering::Release);
            WINDOW_GENERATION.store(generation, Ordering::Release);
            WINDOW.store(panel.windowNumber() as u32, Ordering::Release);
            UI.with(|ui| {
                *ui.borrow_mut() = Some(Ui {
                    panel,
                    view,
                    from,
                    generation,
                })
            });
        });
        wait_for_hit(shared, generation, Instant::now());
    });
}

fn wait_for_hit(shared: &'static Shared, generation: u64, started: Instant) {
    let _scope = handoff_timing::enter_scope(shared.timing_scope());
    let done = shared.owner_update(generation, || {
        UI.with(|ui| {
            let ui = ui.borrow();
            let Some(ui) = ui.as_ref().filter(|u| u.generation == generation && active(generation)) else {
                return true;
            };
            if ui.view.ivars().ready.get() {
                return true;
            }
            let height = CGDisplayBounds(CGMainDisplayID()).size.height;
            let at = NSPoint::new(ui.from.x, height - ui.from.y);
            let hit = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(at, 0, ui.panel.mtm());
            if hit == ui.panel.windowNumber() {
                let posted = shared.owner_post(generation, ui.from);
                if let Some(token) = handoff_timing::scope() {
                    let mut event = handoff_timing::Event::new(TimingStage::OwnerHitResolved, token);
                    event.elapsed_us = started.elapsed().as_micros() as u64;
                    event.code = if posted { 0 } else { 2 };
                    handoff_timing::emit(event);
                }
                tracing::debug!(
                    generation,
                    posted,
                    wait_us = started.elapsed().as_micros() as u64,
                    "cursor owner: window hit, posting same-position move"
                );
                true
            } else if started.elapsed() >= Duration::from_millis(250) {
                handoff_timing::native(TimingStage::OwnerHitResolved, 1);
                tracing::warn!(generation, hit, "cursor owner: window not hit; frozen in place");
                true
            } else {
                false
            }
        })
    });
    if done == Some(false) {
        main_queue::run_after(Duration::from_millis(2), move || {
            wait_for_hit(shared, generation, started)
        });
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn owner_event_tag_is_distinct_and_round_trips() {
        assert_eq!(event_generation(super::super::super::inject::MAGIC), None);
        for generation in [0, 1, 999, GENERATION_MASK] {
            assert_eq!(event_generation(event_tag(generation)), Some(generation));
        }
    }
}
