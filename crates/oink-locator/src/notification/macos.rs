//! One nonactivating panel per toast, confined to its own hit rectangle. AppKit
//! operations are dispatched to the main queue; backend activation policy stays
//! Accessory. No normal window, key status, application activation or TCC APIs.
use super::{
    Click, Feedback, Frame, Surface,
    interaction::{HitState, Input},
};
use objc2::{
    AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSEvent, NSPanel, NSPopUpMenuWindowLevel, NSScreen, NSTrackingArea,
    NSTrackingAreaOptions, NSView, NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWindowStyleMask,
    NSWorkspace,
};
use objc2_core_foundation::{CFData, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayer, CATransaction};
use oink_foundation::Point;
use std::{
    cell::RefCell,
    ffi::c_void,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
type Clicks = Arc<Mutex<Vec<Click>>>;
type Feedbacks = Arc<Mutex<Vec<Feedback>>>;
struct Hit {
    state: RefCell<HitState>,
    clicks: Clicks,
    feedback: Feedbacks,
}
define_class!(
    #[unsafe(super(NSView))] #[thread_kind=MainThreadOnly] #[name="OinkNotificationView"] #[ivars=Hit]
    struct ToastView;
    impl ToastView {
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<&NSView> {
            let point = Point::new(point.x, self.bounds().size.height - point.y);
            self.ivars().state.borrow().contains(point).then(|| self.as_super())
        }
        #[unsafe(method(acceptsFirstMouse:))] fn first_mouse(&self,_event:Option<&NSEvent>)->bool {true}
        #[unsafe(method(mouseEntered:))] fn entered(&self,event:&NSEvent) { self.pointer(event, Input::Move); }
        #[unsafe(method(mouseMoved:))] fn moved(&self,event:&NSEvent) { self.pointer(event, Input::Move); }
        #[unsafe(method(mouseDragged:))] fn dragged(&self,event:&NSEvent) { self.pointer(event, Input::Move); }
        #[unsafe(method(mouseExited:))] fn exited(&self,_event:&NSEvent) {
            self.input(Input::Leave);
            if let Some(window)=self.window() { set_passthrough(&window, true); }
        }
        #[unsafe(method(mouseDown:))] fn down(&self,event:&NSEvent) { self.pointer(event, Input::Down); }
        #[unsafe(method(mouseUp:))] fn up(&self,event:&NSEvent) { self.pointer(event, Input::Up); }
    }
    unsafe impl NSObjectProtocol for ToastView {}
);
impl ToastView {
    fn input(&self, input: Input) {
        let mut state = self.ivars().state.borrow_mut();
        let before = state.feedback();
        let (feedback, click) = state.input(input);
        drop(state);
        if feedback != before {
            self.ivars()
                .feedback
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(feedback);
        }
        if let Some(click) = click {
            self.ivars()
                .clicks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(click);
        }
    }
    fn pointer(&self, event: &NSEvent, input: fn(Point) -> Input) {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        if p.x < 0. || p.y < 0. || p.x >= self.bounds().size.width || p.y >= self.bounds().size.height {
            self.input(Input::Leave);
            return;
        }
        let p = Point::new(p.x, self.bounds().size.height - p.y);
        self.input(input(p));
        if let Some(window) = self.window() {
            set_passthrough(&window, !self.ivars().state.borrow().contains(p));
        }
    }
}
define_class!(
    #[unsafe(super(NSPanel))] #[thread_kind=MainThreadOnly] #[name="OinkNotificationPanel"]
    struct Panel;
    impl Panel {
        #[unsafe(method(canBecomeKeyWindow))] fn key(&self)->bool {false}
        #[unsafe(method(canBecomeMainWindow))] fn main(&self)->bool {false}
    }
    unsafe impl NSObjectProtocol for Panel {}
);
struct Window {
    panel: Retained<Panel>,
    layer: Retained<CALayer>,
    view: Retained<ToastView>,
}
impl Window {
    fn pointer(&self, screen: NSPoint) {
        let local = self.panel.convertPointFromScreen(screen);
        let local = self.view.convertPoint_fromView(local, None);
        let point = Point::new(local.x, self.view.bounds().size.height - local.y);
        self.view.input(Input::Move(point));
        set_passthrough(&self.panel, !self.view.ivars().state.borrow().contains(point));
    }
}
fn set_passthrough(window: &objc2_app_kit::NSWindow, ignore: bool) {
    // Repeating this setter dirties WindowServer/CA state even when unchanged
    // (macOS 14.7.8 profile, macOS 26 native probe). Keep re-entry polling, but
    // only change native input routing when the hit rectangle actually changes.
    if window.ignoresMouseEvents() != ignore {
        window.setIgnoresMouseEvents(ignore);
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        self.panel.orderOut(None);
        self.panel.close();
    }
}
thread_local! {static WINDOWS:RefCell<Vec<Window>>=const{RefCell::new(Vec::new())};}
pub(super) struct Backend {
    clicks: Clicks,
    feedback: Feedbacks,
    reduced: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    pending: Arc<Mutex<Option<Vec<Frame>>>>,
    posted: Arc<AtomicBool>,
}
impl Backend {
    pub fn new() -> Option<Self> {
        let reduced = Arc::new(AtomicBool::new(true));
        let ready = Arc::new(AtomicBool::new(false));
        Some(Self {
            clicks: Arc::default(),
            feedback: Arc::default(),
            reduced,
            ready,
            pending: Arc::default(),
            posted: Arc::default(),
        })
    }
}
impl Surface for Backend {
    fn refresh_preferences(&mut self) {
        self.ready.store(false, Ordering::Release);
        let (reduced, ready) = (self.reduced.clone(), self.ready.clone());
        on_main(move || {
            // SAFETY: NSWorkspace's documented boolean property, queried on
            // AppKit's main thread before rendering a newly requested toast.
            let value: bool =
                unsafe { msg_send![&*NSWorkspace::sharedWorkspace(), accessibilityDisplayShouldReduceMotion] };
            reduced.store(value, Ordering::Release);
            ready.store(true, Ordering::Release);
        });
    }

    fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    fn present(&mut self, frames: Vec<Frame>) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(frames);
        if self.posted.swap(true, Ordering::AcqRel) {
            return;
        }
        let (pending, posted, clicks, feedback, reduced) = (
            self.pending.clone(),
            self.posted.clone(),
            self.clicks.clone(),
            self.feedback.clone(),
            self.reduced.clone(),
        );
        on_main(move || {
            posted.store(false, Ordering::Release);
            let Some(frames) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                return;
            };
            let Some(mtm) = MainThreadMarker::new() else { return };
            // SAFETY: documented boolean property read only on AppKit main.
            let value: bool =
                unsafe { msg_send![&*NSWorkspace::sharedWorkspace(), accessibilityDisplayShouldReduceMotion] };
            reduced.store(value, Ordering::Release);
            apply(mtm, frames, clicks, feedback);
        });
    }
    fn clicks(&mut self) -> Vec<Click> {
        std::mem::take(&mut *self.clicks.lock().unwrap_or_else(|e| e.into_inner()))
    }
    fn feedback(&mut self) -> Vec<Feedback> {
        // AppKit's CALayer alpha and hitTest:nil do not exclude transparent
        // padding from WindowServer's input rectangle (macOS 26 native probe).
        // Use the documented whole-window pass-through switch at the pointer,
        // updated by local moves and this worker's 16ms poll for re-entry. This
        // avoids private shape APIs/global input hooks and does not activate,
        // capture, warp or inject input. Reduced-motion/idle frames still poll.
        on_main(|| {
            WINDOWS.with(|slot| {
                let windows = slot.borrow();
                if windows.is_empty() {
                    return;
                }
                let point = NSEvent::mouseLocation();
                for window in windows.iter() {
                    window.pointer(point);
                }
            })
        });
        std::mem::take(&mut *self.feedback.lock().unwrap_or_else(|e| e.into_inner()))
    }
    fn reduced_motion(&self) -> bool {
        self.reduced.load(Ordering::Acquire)
    }
}
fn make_window(mtm: MainThreadMarker, clicks: Clicks, feedback: Feedbacks) -> Window {
    // SAFETY: designated NSPanel initializer on its main-thread-only subclass.
    let panel: Retained<Panel> = unsafe {
        msg_send![Panel::alloc(mtm),initWithContentRect:NSRect::new(NSPoint::ZERO,NSSize::new(1.,1.)),styleMask:NSWindowStyleMask::Borderless|NSWindowStyleMask::NonactivatingPanel,backing:NSBackingStoreType::Buffered,defer:false]
    };
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(false);
    panel.setIgnoresMouseEvents(false);
    panel.setHidesOnDeactivate(false);
    panel.setCanHide(false);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
    panel.setLevel(NSPopUpMenuWindowLevel - 1);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    // SAFETY: Window retains the panel until after close/orderOut in Drop.
    unsafe { panel.setReleasedWhenClosed(false) };
    let view = ToastView::alloc(mtm).set_ivars(Hit {
        state: RefCell::new(HitState::default()),
        clicks,
        feedback,
    });
    // SAFETY: designated NSView initializer; ivars were installed on allocation.
    let view: Retained<ToastView> =
        unsafe { msg_send![super(view),initWithFrame:NSRect::new(NSPoint::ZERO,NSSize::new(1.,1.))] };
    // ActiveAlways is required: these panels deliberately never become key and
    // the backend is usually inactive. InVisibleRect tracks animated resizing;
    // no global monitor, input capture, activation or TCC permission is needed.
    // SAFETY: The view owns the tracking area and implements its owner callbacks.
    let tracking = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            NSRect::ZERO,
            NSTrackingAreaOptions::MouseEnteredAndExited
                | NSTrackingAreaOptions::MouseMoved
                | NSTrackingAreaOptions::ActiveAlways
                | NSTrackingAreaOptions::InVisibleRect
                | NSTrackingAreaOptions::EnabledDuringMouseDrag,
            Some(&view),
            None,
        )
    };
    view.addTrackingArea(&tracking);
    let layer = CALayer::new();
    view.setLayer(Some(&layer));
    view.setWantsLayer(true);
    panel.setContentView(Some(&view));
    Window { panel, layer, view }
}
fn apply(mtm: MainThreadMarker, frames: Vec<Frame>, clicks: Clicks, feedback: Feedbacks) {
    let _ = NSApplication::sharedApplication(mtm);
    let primary_h = NSScreen::screens(mtm)
        .firstObject()
        .map_or(0., |s| s.frame().size.height);
    WINDOWS.with(|slot| {
        let mut windows = slot.borrow_mut();
        windows.truncate(frames.len());
        while windows.len() < frames.len() {
            windows.push(make_window(mtm, clicks.clone(), feedback.clone()));
        }
        // SAFETY: process-lifetime immutable CoreGraphics color-space constant.
        let Some(space) = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })) else {
            return;
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for (w, f) in windows.iter_mut().zip(frames) {
            let b = f.bounds;
            w.panel.setFrame_display(
                NSRect::new(NSPoint::new(b.x, primary_h - b.bottom()), NSSize::new(b.w, b.h)),
                false,
            );
            // SAFETY: initialized u32 pixels, four bytes each, copied by CFData.
            let data = CFData::from_bytes(unsafe {
                std::slice::from_raw_parts(f.pixels.as_ptr().cast::<u8>(), f.pixels.len() * 4)
            });
            let Some(provider) = CGDataProvider::with_cf_data(Some(&data)) else {
                continue;
            };
            // SAFETY: dimensions/stride match the owned BGRA byte provider.
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
            // SAFETY: CGImage is an accepted CALayer content object retained by the layer.
            unsafe {
                w.layer.setContents(Some(
                    &*(image.as_ref() as *const CGImage).cast::<objc2::runtime::AnyObject>(),
                ))
            };
            w.layer
                .setFrame(CGRect::new(CGPoint::new(0., 0.), CGSize::new(b.w, b.h)));
            w.view.ivars().state.borrow_mut().update_frame(&f);
            w.pointer(NSEvent::mouseLocation());
            w.panel.orderFrontRegardless();
        }
        CATransaction::commit();
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
        tracing::error!("notification main-thread operation panicked");
    }
}
fn on_main(f: impl FnOnce() + Send + 'static) {
    let job: Box<Job> = Box::new(Box::new(f));
    // SAFETY: main queue outlives this operation; trampoline consumes the Box once.
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, Box::into_raw(job).cast(), trampoline) };
}
