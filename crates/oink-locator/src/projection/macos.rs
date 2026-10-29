//! Transparent, non-activating AppKit projection panels. Only AppKit's main
//! thread touches native objects; the render thread owns premultiplied bitmaps.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSEvent, NSPanel, NSPopUpMenuWindowLevel, NSScreen, NSView,
    NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFData, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayer, CATransaction};

use super::{DRAGGING, DragPhase, Event, Frame, NativeHandle, emit};
use crate::diagnostics::{Event as DiagnosticEvent, Health};

struct HandleIvars {
    handle: NativeHandle,
    start: Cell<f64>,
}
define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkLayoutProjectionHandle"]
    #[ivars = HandleIvars]
    struct HandleView;
    impl HandleView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool { true }
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.ivars().start.set(self.along(event));
            DRAGGING.store(true, Ordering::Release);
            self.send(event, DragPhase::Begin);
        }
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) { self.send(event, DragPhase::Update); }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.send(event, DragPhase::End);
            DRAGGING.store(false, Ordering::Release);
        }
    }
    unsafe impl NSObjectProtocol for HandleView {}
);
impl HandleView {
    fn along(&self, event: &NSEvent) -> f64 {
        let p = event.locationInWindow();
        if self.ivars().handle.projection.side.is_vertical() {
            -p.y
        } else {
            p.x
        }
    }
    fn send(&self, event: &NSEvent, phase: DragPhase) {
        let h = &self.ivars().handle;
        tracing::debug!(?phase, handle = ?h.hit.handle, delta = self.along(event) - self.ivars().start.get(), "native projection handle");
        emit(Event::Drag {
            projection: Box::new(h.projection.clone()),
            handle: h.hit.handle,
            delta: self.along(event) - self.ivars().start.get(),
            phase,
        });
    }
}

define_class!(
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkLayoutProjectionPanel"]
    struct ProjectionPanel;
    impl ProjectionPanel {
        // A nonactivating NSPanel can otherwise still take key status. Explicitly
        // refuse both key and main, including when future handle windows click.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool { false }
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool { false }
    }
    unsafe impl NSObjectProtocol for ProjectionPanel {}
);

struct Window {
    panel: Retained<ProjectionPanel>,
    layer: Retained<CALayer>,
}
struct Ui {
    reported_visibility: Option<(usize, usize)>,
    owner: u64,
    desired: usize,
    generation: u64,
    windows: Vec<Window>,
    handles: Vec<Retained<ProjectionPanel>>,
}
thread_local! { static UI: RefCell<Option<Ui>> = const { RefCell::new(None) }; }
static GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Update {
    frames: Vec<Frame>,
    editable: bool,
}
pub(super) struct Backend {
    health: Arc<Health>,
    generation: u64,
    slot: Arc<Mutex<Option<Update>>>,
    posted: Arc<AtomicBool>,
}

impl Backend {
    pub(super) fn new(health: Arc<Health>) -> Option<Self> {
        Some(Self {
            health,
            generation: GENERATION.fetch_add(1, Ordering::AcqRel) + 1,
            slot: Arc::new(Mutex::new(None)),
            posted: Arc::new(AtomicBool::new(false)),
        })
    }
    pub(super) fn present(&mut self, frames: Vec<Frame>, editable: bool) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Update { frames, editable });
        if self.posted.swap(true, Ordering::AcqRel) {
            return;
        }
        let (slot, posted, generation) = (self.slot.clone(), self.posted.clone(), self.generation);
        let health = self.health.clone();
        let ticket = health.ticket();
        on_main(move || {
            ticket.start();
            posted.store(false, Ordering::Release);
            let Some(mtm) = MainThreadMarker::new() else { return };
            if GENERATION.load(Ordering::Acquire) != generation {
                health.event(DiagnosticEvent::StaleGeneration, generation);
                return;
            }
            let update = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if update.is_none() {
                health.event(DiagnosticEvent::EmptyWake, 0);
            }
            // Observe the existing empty-wake behaviour; do not repair it here.
            let update = update.unwrap_or_default();
            apply(mtm, generation, health.id, update.frames, update.editable);
            health.applied();
        });
    }
    pub(super) fn hide(&mut self) {
        self.present(Vec::new(), false);
    }
    pub(super) fn pump(&mut self) {}
    pub(super) fn escape_down(&self) -> bool {
        unsafe extern "C" {
            fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
        }
        // SAFETY: Passive combined-session Escape state, no tap or focus change.
        unsafe { CGEventSourceKeyState(0, 53) }
    }
}

fn make_window(mtm: MainThreadMarker) -> Window {
    // SAFETY: The designated NSPanel initializer on a main-thread-only subclass.
    let panel: Retained<ProjectionPanel> = unsafe {
        msg_send![ProjectionPanel::alloc(mtm),
        initWithContentRect: NSRect::new(NSPoint::ZERO, NSSize::new(1.0, 1.0)),
        styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        backing: NSBackingStoreType::Buffered, defer: false]
    };
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(false);
    panel.setIgnoresMouseEvents(true);
    let policy = oink_foundation::panel_policy::PanelPolicy::BACKEND_OVERLAY;
    panel.setHidesOnDeactivate(policy.hides_on_deactivate);
    // Hide Others can hide the accessory backend; projection bodies and handles
    // belong to the active layout session, not the settings window's visibility.
    panel.setCanHide(policy.can_hide);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
    // Stay below Oink's cursor-owner panel (NSPopUpMenuWindowLevel), so an
    // invisible cursor-owner surface keeps its existing cursor/hit ownership.
    panel.setLevel(NSPopUpMenuWindowLevel - 1);
    // Stationary omits Exposé/Mission Control, IgnoresCycle omits window cycling.
    // Auxiliary + all Spaces covers fullscreen Spaces without switching desktop;
    // a panel creates no Dock/task-switcher entry. Never activate NSApplication.
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    // SAFETY: UI retains the panel, including across close/orderOut.
    unsafe { panel.setReleasedWhenClosed(false) };
    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::ZERO, NSSize::new(1.0, 1.0)));
    let layer = CALayer::new();
    view.setLayer(Some(&layer));
    view.setWantsLayer(true);
    panel.setContentView(Some(&view));
    Window { panel, layer }
}

fn apply(mtm: MainThreadMarker, generation: u64, owner: u64, frames: Vec<Frame>, editable: bool) {
    let _ = NSApplication::sharedApplication(mtm);
    let primary_h = NSScreen::screens(mtm)
        .firstObject()
        .map_or(0.0, |s| s.frame().size.height);
    UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        if ui.as_ref().is_none_or(|u| u.generation != generation) {
            if let Some(old) = ui.take() {
                for panel in old.handles {
                    panel.orderOut(None);
                    panel.close();
                }
                for w in old.windows {
                    w.panel.orderOut(None);
                    w.panel.close();
                }
            }
            *ui = Some(Ui {
                reported_visibility: None,
                owner,
                desired: 0,
                generation,
                windows: Vec::new(),
                handles: Vec::new(),
            });
        }
        let ui = ui.as_mut().expect("initialized");
        ui.desired = frames.len();
        // AppKit retains a down/drag/up tracking sequence outside the small
        // handle. Keep that window stationary until the gesture ends.
        // A preview can leave this monitor or lose its neighbour mid-drag.
        // Keep AppKit's tracking view alive until up; an explicit hide/expiry
        // passes editable=false and cancels it immediately.
        if !editable || !DRAGGING.load(Ordering::Acquire) {
            for panel in ui.handles.drain(..) {
                panel.orderOut(None);
                panel.close();
            }
            for handle in frames.iter().filter(|_| editable).flat_map(|f| f.handles.iter()) {
                let panel = make_window(mtm).panel;
                let b = handle.hit.rect;
                panel.setFrame_display(
                    NSRect::new(NSPoint::new(b.x, primary_h - b.bottom()), NSSize::new(b.w, b.h)),
                    false,
                );
                panel.setIgnoresMouseEvents(false);
                // Only these small bounds take mouse events. All projection
                // pixels and the outer crossing gutter remain click-through.
                panel.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.0, 0.01)));
                let this = HandleView::alloc(mtm).set_ivars(HandleIvars {
                    handle: handle.clone(),
                    start: Cell::new(0.0),
                });
                // SAFETY: NSView's designated initializer on our subclass.
                let view: Retained<HandleView> =
                    unsafe { msg_send![super(this), initWithFrame: NSRect::new(NSPoint::ZERO, NSSize::new(b.w, b.h))] };
                panel.setContentView(Some(&view));
                panel.orderFrontRegardless();
                ui.handles.push(panel);
            }
            if frames.is_empty() {
                DRAGGING.store(false, Ordering::Release);
            }
        }
        while ui.windows.len() > frames.len() {
            let w = ui.windows.pop().expect("nonempty");
            w.panel.orderOut(None);
            w.panel.close();
        }
        while ui.windows.len() < frames.len() {
            ui.windows.push(make_window(mtm));
        }
        // SAFETY: immutable system constant; tagged sRGB avoids CPU colour matching.
        let Some(space) = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })) else {
            return;
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for (w, f) in ui.windows.iter_mut().zip(frames) {
            let b = f.bounds;
            w.panel.setFrame_display(
                NSRect::new(NSPoint::new(b.x, primary_h - b.bottom()), NSSize::new(b.w, b.h)),
                false,
            );
            // SAFETY: width*height initialized u32 pixels, interpreted as BGRA bytes.
            let bytes = unsafe { std::slice::from_raw_parts(f.pixels.as_ptr().cast::<u8>(), f.pixels.len() * 4) };
            let data = CFData::from_bytes(bytes);
            let Some(provider) = CGDataProvider::with_cf_data(Some(&data)) else {
                continue;
            };
            // SAFETY: Matching dimensions / row stride; provider owns its copied bytes.
            let image = unsafe {
                CGImage::new(
                    f.width,
                    f.height,
                    8,
                    32,
                    f.width * 4,
                    Some(&space),
                    CGBitmapInfo(CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0),
                    Some(&provider),
                    std::ptr::null(),
                    false,
                    CGColorRenderingIntent::RenderingIntentDefault,
                )
            };
            let Some(image) = image else { continue };
            // SAFETY: CoreGraphics images are valid Objective-C layer contents.
            unsafe {
                w.layer.setContents(Some(
                    &*(image.as_ref() as *const CGImage).cast::<objc2::runtime::AnyObject>(),
                ))
            };
            w.layer
                .setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(b.w, b.h)));
            w.panel.orderFrontRegardless();
            tracing::debug!(window = w.panel.windowNumber(), visible = w.panel.isVisible(), key = w.panel.isKeyWindow(),
                level = w.panel.level(), frame = ?w.panel.frame(), "projection panel presented");
        }
        CATransaction::commit();
        let visible = ui.windows.iter().filter(|w| w.panel.isVisible()).count();
        let visibility = (ui.desired, visible);
        if ui.reported_visibility != Some(visibility) {
            tracing::info!(requested = ui.desired, visible, "projection native panel visibility");
            ui.reported_visibility = Some(visibility);
        }
    });
}

#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}
unsafe extern "C" {
    static _dispatch_main_q: DispatchQueue;
    fn dispatch_async_f(queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
}
type Job = Box<dyn FnOnce() + Send + 'static>;
extern "C" fn trampoline(context: *mut c_void) {
    // SAFETY: on_main transfers this Box exactly once to the main dispatch queue.
    let job = unsafe { Box::from_raw(context.cast::<Job>()) };
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
        tracing::error!("projection main-thread operation panicked");
    }
}
fn on_main(f: impl FnOnce() + Send + 'static) {
    let job: Box<Job> = Box::new(Box::new(f));
    // SAFETY: process-lifetime main queue; trampoline consumes the context.
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, Box::into_raw(job).cast(), trampoline) };
}

// Passive diagnostic sampling must also run when identical views skip present.
pub(crate) fn diagnose(health: &Health, mtm: MainThreadMarker) {
    UI.with(|slot| {
        let ui = slot.borrow();
        let panels: Vec<_> = ui
            .as_ref()
            .into_iter()
            .flat_map(|u| {
                u.windows
                    .iter()
                    .enumerate()
                    .map(|(i, w)| (&*w.panel as &NSPanel, i < u.desired, !w.layer.isHidden()))
                    .chain(u.handles.iter().map(|w| (&**w as &NSPanel, true, true)))
            })
            .collect();
        crate::diagnostics::macos::visibility(
            health,
            ui.as_ref().map_or(0, |u| u.owner),
            ui.as_ref().map_or(0, |u| u.generation),
            panels,
            mtm,
        );
    });
}

// AppKit desktop coordinates are UI points; Retina density is applied separately.
pub(super) fn annotation_unit(_screen: oink_foundation::Rect) -> f64 {
    1.0
}
