//! macOS native drag capture, handoff and destination publication.
//!
//! - 所有 AppKit 操作在主线程上做（经主 dispatch 队列）；调用方（本机动作线程）带超时等结果。主线程必须在跑
//!   `NSApplication` 的事件循环（界面程序），否则窗口收不到鼠标事件：[`MacDnd::start`] 检查 `NSApp` 在运行，
//!   不在时返回 `Unsupported`。
//! - 两个无边框、不激活应用的 `NSPanel`（`NonactivatingPanel`），层级在普通窗口之上，背景 alpha 0.01（完全透明的
//!   像素不接收鼠标）：
//!   - **接收窗口**：注册文件 URL、文本、图片类型。`draggingEntered:` 在后台线程读拖拽剪贴板（与剪贴板读取同一套
//!     规则），报告 `Detected`；`performDragOperation:` 只在 `release` 期间接受（交出源拖拽），其他时候拒绝
//!     （探测中在边缘松手 = 拖拽失败，图标飞回）。
//!   - **发起窗口**：注册类型只为识别取消落点，拒绝一切拖入。`begin` 把它放到光标下、注入左键按下，视图在 `mouseDown:`
//!     （`acceptsFirstMouse:` 为 YES，应用不需要激活）里 `beginDraggingSessionWithItems:event:source:`。
//!     `cancel` 把它挪到光标下再注入抬起：放在不接受拖入的窗口上 = 拖拽失败，不会落到任何程序里。
//! - 远端文件优先提供 File Provider NSURL；扩展不可用时回退文件承诺，保留沙盒目标容器写入的已知限制。

use oink_foundation::dnd::{CancelGeometry, CancelObservation, DestinationSnapshot, NativeHoldEnd, NativeHoldSummary};
use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
#[cfg(all(test, debug_assertions))]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSDragOperation, NSDraggingContext, NSDraggingDestination,
    NSDraggingInfo, NSDraggingItem, NSDraggingSession, NSDraggingSource, NSEvent, NSFilePromiseProvider,
    NSFilePromiseProviderDelegate, NSFilePromiseReceiver, NSPanel, NSPasteboard, NSPasteboardItem, NSPasteboardWriting,
    NSPasteboardWritingOptions, NSPopUpMenuWindowLevel, NSScreen, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSCocoaErrorDomain, NSData, NSError, NSObjectProtocol, NSOperationQueue, NSPoint, NSRect, NSSize,
    NSString, NSURL,
};
use oink_foundation::Point;
use oink_foundation::clipboard::{ClipData, FileEntry};
use oink_foundation::dnd::{ImageNudgeObservation, ImageNudgeReason};
use oink_foundation::input::{InjectEvent, MouseButton};

use super::file_provider::Registration;
use super::files;
use crate::dnd::{
    DndBackend, DndSink, DragOperation, DragOps, DragSpec, Injector, PlatformDndEvent, SourceImageNudge, SourcePress,
    SourceStart, SourceStartTrace,
};
use crate::{ClipboardError, FileContents};

/// 拖拽剪贴板（`NSPasteboardNameDrag`）。
const DRAG_PASTEBOARD: &str = "Apple CFPasteboard drag";
/// 窗口边长（pt）。
const SIZE: f64 = 48.0;
/// 等主线程、等系统拖拽的上限。
const WAIT: Duration = Duration::from_secs(1);
const IMAGE_NUDGE_DELAY: Duration = Duration::from_millis(100);
mod promise;
mod proxy;
mod representation_trace;

// ───────────── 主线程 ─────────────

#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}

unsafe extern "C" {
    static _dispatch_main_q: DispatchQueue;
    fn dispatch_async_f(queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_time(when: u64, delta: i64) -> u64;
    fn dispatch_after_f(when: u64, queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

type Job = Box<dyn FnOnce(MainThreadMarker) + Send + 'static>;

extern "C" fn trampoline(ctx: *mut c_void) {
    // SAFETY: ctx 是 `on_main` 里 Box::into_raw 得到的 Box<Job>，只执行一次。
    let job = unsafe { Box::from_raw(ctx as *mut Job) };
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::error!("drag and drop: main-queue job not on the main thread");
        return;
    };
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || job(mtm))).is_err() {
        tracing::error!("drag and drop: main-thread job panicked");
    }
}

/// One background decoder at a time. An overdue native ImageIO call cannot
/// be interrupted safely; it loses publication authority and prevents new jobs
/// until it returns. Neither AppKit nor input waits for the worker.
fn start_thumbnail(shared: Arc<Shared>, operation: DragOperation, name: String, change: isize, start: Instant) {
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if BUSY
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let budget = super::thumbnail::Budget::new(start);
    shared.lock().thumbnail_budget = Some(budget.clone());
    let spawned = std::thread::Builder::new()
        .name("oink-dnd-thumbnail".into())
        .spawn(move || {
            struct Release;
            impl Drop for Release {
                fn drop(&mut self) {
                    BUSY.store(false, Ordering::Release);
                }
            }
            let _release = Release;
            objc2::rc::autoreleasepool(|_| {
                let Some(image) = super::thumbnail::generate(&name, change, &budget) else {
                    return;
                };
                {
                    let mut st = shared.lock();
                    if st.source_operation != Some(operation) || !budget.current() {
                        return;
                    }
                    st.source_thumbnail = Some((operation, image.clone()));
                }
                shared.emit(PlatformDndEvent::SourceImage { operation, image });
            });
        });
    if spawned.is_err() {
        BUSY.store(false, Ordering::Release);
    }
}

fn apply_thumbnail(item: &NSDraggingItem, image: &oink_foundation::dnd::DragImage, bitmap: &objc2_app_kit::NSImage) {
    let old = item.draggingFrame();
    let frame = NSRect::new(
        NSPoint::new(
            old.origin.x + old.size.width / 2. - image.hotspot_x,
            old.origin.y + old.size.height / 2. - image.height + image.hotspot_y,
        ),
        NSSize::new(image.width, image.height),
    );
    // SAFETY: bitmap is an NSImage retained throughout this synchronous update.
    unsafe {
        item.setDraggingFrame_contents(frame, Some(bitmap.as_ref()));
    }
}

/// 在主线程上执行 `f`，不等待。
fn on_main(f: impl FnOnce(MainThreadMarker) + Send + 'static) {
    if let Some(mtm) = MainThreadMarker::new() {
        f(mtm);
        return;
    }
    queue_main(f);
}

/// Always defer, including callers already inside an AppKit drag callback.
fn queue_main(f: impl FnOnce(MainThreadMarker) + Send + 'static) {
    let job: Box<Job> = Box::new(Box::new(f));
    // SAFETY: 主队列对象在进程内一直有效；context 由 trampoline 接管并释放。
    unsafe {
        dispatch_async_f(
            &raw const _dispatch_main_q,
            Box::into_raw(job) as *mut c_void,
            trampoline,
        )
    };
}

fn queue_main_at(deadline: Instant, f: impl FnOnce(MainThreadMarker) + Send + 'static) {
    let delay_ns = deadline
        .saturating_duration_since(Instant::now())
        .as_nanos()
        .min(i64::MAX as u128) as i64;
    let job: Box<Job> = Box::new(Box::new(f));
    // SAFETY: public libdispatch retains the main queue; trampoline owns the
    // context, including when an overdue job is deferred to the next queue turn.
    unsafe {
        dispatch_after_f(
            dispatch_time(0, delay_ns),
            &raw const _dispatch_main_q,
            Box::into_raw(job) as *mut c_void,
            trampoline,
        )
    };
}

/// 在主线程上执行 `f` 并等结果（最多 `timeout`；主线程没在服务主队列时超时返回 `None`）。
fn on_main_wait<R: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce(MainThreadMarker) -> R + Send + 'static,
) -> Option<R> {
    let trace = oink_foundation::drag_trace::Trace::new(
        oink_foundation::drag_trace::stage::MAIN_QUEUE,
        oink_foundation::drag_trace::Context::current(),
    );
    let diagnostic = trace.context();
    let (tx, rx) = mpsc::channel();
    let pending = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let queued = pending.clone();
    on_main(move |mtm| {
        diagnostic.scope(|| {
            if queued.swap(false, Ordering::AcqRel) {
                let _ = tx.send(f(mtm));
            }
        })
    });
    let result = rx.recv_timeout(timeout).ok();
    // A job that has not started loses its authority on timeout. An already
    // running native call remains unconfirmed until its own keyed observation.
    pending.store(false, Ordering::Release);
    trace.result(if result.is_some() { 0 } else { -1 });
    result
}

// ───────────── 共享状态 ─────────────

#[derive(Default)]
struct St {
    content_root: Option<std::path::PathBuf>,
    source_data: Option<(DragOperation, Arc<crate::content::Deferred>)>,
    export: Option<(DragOperation, Arc<promise::Export>)>,
    exported_data: Option<(DragOperation, Arc<dyn crate::content::Provider>)>,
    source_present: bool,
    source_operation: Option<DragOperation>,
    thumbnail_budget: Option<super::thumbnail::Budget>,
    source_thumbnail: Option<(DragOperation, oink_foundation::dnd::DragImage)>,
    /// Catcher ownership outlives the native ended callback until cleanup.
    /// A new probe revokes it even before its first Detected callback.
    probe_operation: Option<DragOperation>,
    held_source: Option<DragOperation>,
    source_button: Option<MouseButton>,
    next_image_hold: u64,
    image_hold: Option<ImageHold>,
    cancel_move: Option<CancelMove>,
    cancel_observation: Option<(DragOperation, CancelObservation)>,
    /// `release` 进行中：接收窗口接受放下。
    releasing: bool,
    /// 接收窗口收到了放下（`release` 等它）。
    released: bool,
    /// 已报告过的源拖拽（`draggingSequenceNumber`），同一次拖拽只报告一次。
    reported_seq: Option<isize>,
    /// 接收窗口在显示。
    probing: bool,
    /// `begin` 等发起窗口的 `mouseDown:` 开始拖拽。
    began: bool,
    starting_operation: Option<DragOperation>,
    native_begin: Option<NativeBegin>,
    press_received: bool,
    /// 发起的拖拽在进行。
    active: bool,
    /// 发起的拖拽结束了（`cancel` 等它）。
    ended: bool,
    /// 本机的拖拽来源的操作（`draggingSession:sourceOperationMaskForDraggingContext:`）。
    ops: DragOps,
    /// 拖拽会话已经发现发起窗口在光标下（`cancel` 等它再抬起，否则抬起会落到之前的目标上）。
    over_source: bool,
    target_image: Option<oink_foundation::dnd::DragImage>,
    target_preview: Option<(DragOperation, Vec<ClipData>)>,
}

impl St {
    fn probe_current(&self, sequence: isize) -> bool {
        // A catcher exit retires the operation even when AppKit keeps the
        // original native sequence. Re-entry must publish fresh metadata.
        self.reported_seq == Some(sequence) && self.source_operation.is_some()
    }
}

/// Session acknowledgement is distinct from the input tap's delivery receipt.
/// Keep an exit during repositioning until Escape/release have been submitted.
#[derive(Clone, Copy)]
struct CancelMove {
    operation: DragOperation,
    at: Point,
    acknowledged: bool,
    exited: bool,
    cancel_submitted: bool,
    started: Instant,
}
impl CancelMove {
    fn observe(&mut self, at: Point, exited: bool) {
        self.exited |= exited;
        self.acknowledged |= (self.at.x - at.x).abs() <= 1.0 && (self.at.y - at.y).abs() <= 1.0;
    }
    fn cleanup_ready(&self) -> bool {
        self.exited && self.cancel_submitted
    }
}

impl St {
    fn retire_image_nudges(&mut self) {
        if let Some(hold) = &mut self.image_hold {
            hold.active = false;
            hold.pending.clear();
        }
    }
    fn observe_cancel_position(&mut self, at: Point, exited: bool) -> bool {
        let Some(movement) = self.cancel_move.as_mut() else {
            return false;
        };
        movement.observe(at, exited);
        let elapsed_us = movement.started.elapsed().as_micros() as u64;
        if let Some((_, observation)) = self
            .cancel_observation
            .as_mut()
            .filter(|(op, _)| *op == movement.operation)
        {
            observation.callback_count += 1;
            observation.last_callback = Some(at);
            if movement.acknowledged && observation.ack_latency_us.is_none() {
                observation.ack_position = Some(at);
                observation.ack_latency_us = Some(elapsed_us);
            }
        }
        movement.cleanup_ready()
    }
}

/// Separate from the native operation: one session can leave and be held again.
struct ImageHold {
    operation: DragOperation,
    generation: u64,
    native_sequence: i64,
    button: Option<MouseButton>,
    started: Instant,
    active: bool,
    ready: bool,
    pending: Vec<ImageNudgeReason>,
    posts: Vec<ImageNudgeObservation>,
}
impl ImageHold {
    fn matches(&self, operation: DragOperation, generation: u64) -> bool {
        self.active && self.operation == operation && self.generation == generation
    }
    fn observe_image_update(&mut self) {
        let elapsed = self.started.elapsed().as_micros() as u64;
        for post in &mut self.posts {
            post.image_update_us.get_or_insert(elapsed);
        }
    }
}

fn image_nudge_job(
    shared: Arc<Shared>,
    operation: DragOperation,
    generation: u64,
    reason: ImageNudgeReason,
    changed: usize,
    enumeration_returned: Instant,
) -> Option<(Instant, impl FnOnce() + Send + 'static)> {
    (changed != 0).then_some((enumeration_returned + IMAGE_NUDGE_DELAY, move || {
        shared.post_image_nudge(operation, generation, reason)
    }))
}

fn queue_image_nudge(
    shared: Arc<Shared>,
    operation: DragOperation,
    generation: u64,
    reason: ImageNudgeReason,
    changed: usize,
    enumeration_returned: Instant,
) {
    if let Some((deadline, job)) = image_nudge_job(shared, operation, generation, reason, changed, enumeration_returned)
    {
        // One deadline per actual enumeration batch, measured from its return,
        // never from anchor readiness or a later callback. See the posting-site
        // OS-workaround comment in oink-input's nudge_held_drag.
        queue_main_at(deadline, move |_| job());
    }
}

struct Shared {
    sink: OnceLock<DndSink>,
    inject: OnceLock<Injector>,
    source_press: OnceLock<SourcePress>,
    source_start: OnceLock<SourceStart>,
    image_nudge: OnceLock<SourceImageNudge>,
    st: Mutex<St>,
    cv: Condvar,
}

type NativeBeginCallback = Box<dyn FnOnce(Option<Instant>) + Send>;
struct NativeBegin {
    operation: DragOperation,
    at: Option<Instant>,
    finished: bool,
    callback: Option<NativeBeginCallback>,
    timing: Option<crate::dnd::NativeBeginTiming>,
}

impl Shared {
    fn post_image_nudge(&self, operation: DragOperation, generation: u64, reason: ImageNudgeReason) {
        // Keep validity and submission in one critical section. Cancel/Resume
        // cannot revoke then race a stale post behind their own dragged/UP event.
        // The native tap's ACK path never takes this clipboard-state mutex.
        let mut st = self.lock();
        if st.held_source != Some(operation) || !st.source_present || st.cancel_move.is_some() || st.releasing {
            return;
        }
        let Some(hold) = st.image_hold.as_mut().filter(|h| h.matches(operation, generation)) else {
            return;
        };
        if !hold.ready {
            hold.pending.push(reason);
            return;
        }
        let (Some(button), Some(inject)) = (hold.button, self.image_nudge.get()) else {
            return;
        };
        let observation = RefCell::new(None);
        let result = inject(operation, button, &|post| {
            *observation.borrow_mut() = Some(ImageNudgeObservation {
                generation,
                reason,
                posted_us: hold.started.elapsed().as_micros() as u64,
                post,
                image_update_us: None,
            });
        });
        if let Some(post) = observation.into_inner() {
            hold.posts.push(post);
        }
        if let Err(error) = result {
            tracing::warn!(%error, generation, "held drag image nudge was not acknowledged; never replaying");
        }
    }
    fn reset_native_begin(&self, operation: DragOperation) {
        let old = self.lock().native_begin.replace(NativeBegin {
            operation,
            at: None,
            finished: false,
            callback: None,
            timing: None,
        });
        if let Some(old) = old {
            if let Some(callback) = old.callback {
                callback(old.at);
            }
            if let Some(timing) = old.timing {
                timing.complete(old.at);
            }
        }
    }

    fn complete_native_begin(&self, operation: Option<DragOperation>, at: Option<Instant>) {
        let ready = {
            let mut st = self.lock();
            st.native_begin.as_mut().and_then(|observation| {
                if operation.is_some_and(|op| op != observation.operation) || observation.finished {
                    return None;
                }
                observation.at = at;
                observation.finished = true;
                Some((observation.callback.take(), observation.timing.take(), observation.at))
            })
        };
        if let Some((callback, timing, at)) = ready {
            if let Some(callback) = callback {
                callback(at);
            }
            if let Some(timing) = timing {
                timing.complete(at);
            }
        }
    }

    fn subscribe_native_begin(&self, operation: DragOperation, wait: bool, callback: NativeBeginCallback) {
        let ready = {
            let mut st = self.lock();
            match st
                .native_begin
                .as_mut()
                .filter(|observation| observation.operation == operation)
            {
                Some(observation) if wait && !observation.finished => {
                    debug_assert!(observation.callback.is_none());
                    observation.callback = Some(callback);
                    None
                }
                observation => Some((callback, observation.and_then(|observation| observation.at))),
            }
        };
        if let Some((callback, at)) = ready {
            callback(at);
        }
    }

    fn subscribe_native_timing(&self, operation: DragOperation, wait: bool, timing: crate::dnd::NativeBeginTiming) {
        let ready = {
            let mut st = self.lock();
            match st.native_begin.as_mut().filter(|o| o.operation == operation) {
                Some(o) if wait && !o.finished => {
                    o.timing = Some(timing);
                    None
                }
                o => Some(o.and_then(|o| o.at)),
            }
        };
        if let Some(at) = ready {
            timing.complete(at);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 等 `done` 成立（最多 `timeout`），返回是否成立。
    fn wait(&self, timeout: Duration, done: impl Fn(&St) -> bool) -> bool {
        let until = Instant::now() + timeout;
        let mut st = self.lock();
        while !done(&st) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self.cv.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    fn inject(&self, ev: InjectEvent) -> bool {
        match self.inject.get() {
            Some(f) => match f(&ev) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(%e, ?ev, "drag and drop: inject failed");
                    false
                }
            },
            None => false,
        }
    }

    fn emit(&self, ev: PlatformDndEvent) {
        if let Some(s) = self.sink.get() {
            s(ev);
        }
    }
}

/// 主线程上的窗口与进行中的拖拽。
struct Ui {
    receivers: std::collections::BTreeMap<u64, Vec<Retained<NSFilePromiseReceiver>>>,
    source_info: Option<Retained<ProtocolObject<dyn NSDraggingInfo>>>,
    target_session: Option<(DragOperation, Retained<NSDraggingSession>)>,
    hold_visual: Option<HoldVisual>,
    operations: std::collections::BTreeMap<isize, DragOperation>,
    catcher: Retained<NSPanel>,
    source: Retained<NSPanel>,
    source_view: Retained<DndView>,
    /// `begin` 交给发起窗口、等 `mouseDown:` 取走的拖拽。
    pending: Option<PreparedDrag>,
    /// Retained until the system drag ends; provider retains successful drops beyond this.
    current: Option<Registration>,
    current_proxies: Vec<Arc<proxy::Proxy>>,
}

thread_local! {
    static CATCHER_SHOWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CATCHER_MOVES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
struct HoldVisual {
    generation: u64,
    started: Instant,
    shows: u64,
    moves: u64,
    summary: NativeHoldSummary,
}
fn destination_snapshot(panel: &NSPanel, sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) -> DestinationSnapshot {
    let mtm = panel.mtm();
    let mouse = NSEvent::mouseLocation();
    let frame = panel.frame();
    let corner = to_cocoa(mtm, Point::new(frame.origin.x, frame.origin.y + frame.size.height));
    let mut destination_ours = false;
    let session_location = sender.map(|sender| {
        let destination = sender.draggingDestinationWindow();
        destination_ours = destination
            .as_ref()
            .is_some_and(|w| w.windowNumber() == panel.windowNumber());
        let at = destination.map_or_else(
            || panel.convertPointToScreen(sender.draggingLocation()),
            |w| w.convertPointToScreen(sender.draggingLocation()),
        );
        let at = to_cocoa(mtm, Point::new(at.x, at.y));
        Point::new(at.x, at.y)
    });
    let cg_mouse = to_cocoa(mtm, Point::new(mouse.x, mouse.y));
    DestinationSnapshot {
        session_location,
        destination_ours,
        catcher: oink_foundation::Rect::new(corner.x, corner.y, frame.size.width, frame.size.height),
        mouse_location: Point::new(cg_mouse.x, cg_mouse.y),
        mouse_hit_ours: hit_at(mtm, panel, mouse),
        visible: panel.isVisible(),
        level: panel.level() as i64,
    }
}
impl HoldVisual {
    fn new(
        operation: DragOperation,
        sender: &ProtocolObject<dyn NSDraggingInfo>,
        panel: &NSPanel,
        initial_items: usize,
        started: Instant,
        generation: u64,
    ) -> Self {
        let start = destination_snapshot(panel, Some(sender));
        Self {
            generation,
            started,
            shows: CATCHER_SHOWS.get(),
            moves: CATCHER_MOVES.get(),
            summary: NativeHoldSummary {
                operation: operation.sequence,
                native_sequence: sender.draggingSequenceNumber() as i64,
                elapsed_us: 0,
                end: NativeHoldEnd::HoldUnconfirmed,
                entered: 0,
                updated: 0,
                image_updated: 0,
                exited: 0,
                ended: 0,
                first_exit_us: None,
                last_source_mask: sender.draggingSourceOperationMask().0 as u64,
                last_returned_mask: 0,
                last_is_own: false,
                returned_none: 0,
                first_none_us: None,
                initial_nil_items: initial_items as u64,
                update_nil_items: 0,
                providers_on_update: 0,
                show_calls: 0,
                move_calls: 0,
                start,
                first_image: None,
                finish: start,
                cancel: None,
                image_nudges_posted: 0,
                image_nudges: Vec::new(),
            },
        }
    }
    fn report(mut self, shared: &Shared, reason: NativeHoldEnd) {
        self.summary.end = reason;
        self.summary.elapsed_us = self.started.elapsed().as_micros() as u64;
        self.summary.show_calls = CATCHER_SHOWS.get().saturating_sub(self.shows);
        self.summary.move_calls = CATCHER_MOVES.get().saturating_sub(self.moves);
        {
            let mut st = shared.lock();
            if st.image_hold.as_ref().is_some_and(|h| h.generation == self.generation) {
                let hold = st.image_hold.take().unwrap();
                self.summary.image_nudges_posted = hold.posts.len() as u64;
                self.summary.image_nudges = hold.posts;
            }
            if st
                .cancel_observation
                .as_ref()
                .is_some_and(|(operation, _)| operation.sequence == self.summary.operation)
            {
                self.summary.cancel = st.cancel_observation.take().map(|(_, observation)| observation);
            }
        }
        shared.emit(PlatformDndEvent::NativeHoldSummary {
            operation: DragOperation {
                sequence: self.summary.operation,
            },
            summary: Box::new(self.summary),
        });
    }
}

/// AppKit can request fresh artwork after the initial destination enumeration.
/// The native probe handles this callback too; a one-shot hide misses that update.
fn hide_drag_images(sender: &ProtocolObject<dyn NSDraggingInfo>) -> (usize, usize) {
    use objc2::ClassType;
    let count = std::rc::Rc::new(std::cell::Cell::new(0));
    let restored = std::rc::Rc::new(std::cell::Cell::new(0));
    let counted = count.clone();
    let observed = restored.clone();
    let classes = NSArray::from_slice(&[NSPasteboardItem::class()]);
    let options = objc2_foundation::NSDictionary::new();
    let hide = block2::RcBlock::new(
        move |item: std::ptr::NonNull<NSDraggingItem>, _: isize, _: std::ptr::NonNull<objc2::runtime::Bool>| {
            counted.set(counted.get() + 1);
            // SAFETY: synchronous enumeration on AppKit's main thread; querying
            // the provider pointer does not invoke it or read source payloads.
            unsafe {
                let item = item.as_ref();
                if hide_drag_item(item) {
                    observed.set(observed.get() + 1);
                }
            }
        },
    );
    // SAFETY: retained arguments/block live through the synchronous enumeration.
    unsafe {
        sender.enumerateDraggingItemsWithOptions_forView_classes_searchOptions_usingBlock(
            objc2_app_kit::NSDraggingItemEnumerationOptions::empty(),
            None,
            &classes,
            &options,
            &hide,
        );
    }
    (count.get(), restored.get())
}

fn hide_drag_item(item: &NSDraggingItem) -> bool {
    // Nil is already hidden. Calling its setter again could generate another
    // image callback even though nothing changed; such callbacks must not post.
    if item.imageComponentsProvider().is_null() {
        return false;
    }
    // SAFETY: the synchronous main-thread enumeration owns this item.
    unsafe { item.setImageComponentsProvider(None) };
    true
}

fn native_drag_button(mtm: MainThreadMarker) -> Option<MouseButton> {
    use objc2_app_kit::NSEventType;
    let event = NSApplication::sharedApplication(mtm).currentEvent();
    let number = match event.as_ref().map(|event| event.r#type()) {
        Some(NSEventType::LeftMouseDragged) => Some(0),
        Some(NSEventType::RightMouseDragged) => Some(1),
        Some(NSEventType::OtherMouseDragged) => event.map(|event| event.buttonNumber()),
        _ => {
            // Some drag callbacks run on a non-mouse AppKit event. Only a
            // single held button is unambiguous; never guess among several.
            let buttons = NSEvent::pressedMouseButtons();
            (buttons.count_ones() == 1).then(|| buttons.trailing_zeros() as isize)
        }
    }?;
    Some(match number {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        n => MouseButton::Other(u8::try_from(n).ok()?),
    })
}

fn end_held_source(shared: &Shared, accepted: bool, reason: NativeHoldEnd) {
    let exporting = shared
        .lock()
        .export
        .as_ref()
        .is_some_and(|(_, export)| export.is_boundary() && export.started.load(Ordering::Acquire));
    let operation = {
        let mut st = shared.lock();
        if !exporting
            && !accepted
            && st.held_source.is_some()
            && let Some((_, data)) = &st.source_data
        {
            data.fail(crate::model::Unavailable::Cancelled);
        }
        st.cancel_move = None;
        st.retire_image_nudges();
        st.source_button = None;
        st.source_present = false;
        st.source_operation = None;
        if let Some(budget) = st.thumbnail_budget.take() {
            budget.cancel();
        }
        st.releasing = false;
        st.held_source.take()
    };
    shared.cv.notify_all();
    UI.with(|u| {
        if let Some(ui) = u.borrow_mut().as_mut() {
            if let Some(mut visual) = ui.hold_visual.take() {
                if visual.summary.exited == 0 {
                    visual.summary.finish = destination_snapshot(&ui.catcher, ui.source_info.as_deref());
                }
                visual.report(shared, reason);
            }
            ui.source_info = None;
        }
    });
    if let Some(operation) = operation
        && !exporting
    {
        shared.emit(match reason {
            NativeHoldEnd::DraggingExited => PlatformDndEvent::SourceCatcherLost { operation },
            NativeHoldEnd::CancelRepositioned => PlatformDndEvent::SourceCancelRetired { operation },
            _ => PlatformDndEvent::SourceEnded { operation, accepted },
        });
    }
}

struct PreparedDrag {
    spec: DragSpec,
    provider: Option<Registration>,
    image: Option<oink_foundation::dnd::DragImage>,
    preview: Vec<ClipData>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

// ───────────── 视图 ─────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Catcher,
    Source,
}

fn dragged_types(_role: Role) -> Retained<NSArray<NSString>> {
    let mut types = vec![
        NSString::from_str("public.file-url"),
        NSString::from_str("public.utf8-plain-text"),
        NSString::from_str("public.image"),
        NSString::from_str("public.jpeg"),
        NSString::from_str("public.heic"),
        NSString::from_str("public.png"),
        NSString::from_str("public.tiff"),
        NSString::from_str("public.html"),
        NSString::from_str("public.rtf"),
    ];
    // Both the catcher and cancel window recognize promises. Registration and
    // receiver discovery alone do not call them in; only the boundary Drop does.
    types.extend(NSFilePromiseReceiver::readableDraggedTypes());
    NSArray::from_retained_slice(&types)
}

struct ViewIvars {
    role: Role,
    shared: Arc<Shared>,
}

define_class!(
    /// 接收窗口与发起窗口的内容视图。
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkDndView"]
    #[ivars = ViewIvars]
    struct DndView;

    impl DndView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            tracing::debug!(role = ?self.ivars().role, "mouseDown on a drag-and-drop window");
            if self.ivars().role == Role::Source {
                self.ivars().shared.lock().press_received = true;
                self.ivars().shared.cv.notify_all();
                self.start_drag(event);
            }
        }
    }

    unsafe impl NSObjectProtocol for DndView {}

    unsafe impl NSDraggingDestination for DndView {
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            let returned = self.entered(sender);
            self.observe_hold(sender.into(), HoldCallback::Entered, returned, self.is_own(sender));
            returned
        }

        #[unsafe(method(draggingUpdated:))]
        fn dragging_updated(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            if self.ivars().role == Role::Source {
                self.mark_over_source(true);
                return NSDragOperation::None;
            }
            self.observe_cancel_move(sender, false);
            let own = self.is_own(sender);
            let returned = if !own && sender.draggingSourceOperationMask().contains(NSDragOperation::Copy) {
                NSDragOperation::Copy
            } else { NSDragOperation::None };
            self.observe_hold(Some(sender), HoldCallback::Updated, returned, own);
            returned
        }

        #[unsafe(method(updateDraggingItemsForDrag:))]
        fn update_dragging_items(&self, sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
            if self.ivars().role != Role::Catcher { return; }
            let Some(sender) = sender else { return; };
            let shared = &self.ivars().shared;
            let hold = {
                let mut st = shared.lock();
                let Some(hold) = st.image_hold.as_mut().filter(|h| h.active && h.native_sequence == sender.draggingSequenceNumber() as i64) else { return };
                hold.observe_image_update();
                (hold.operation, hold.generation)
            };
            let (items, providers) = hide_drag_images(sender);
            let enumeration_returned = Instant::now();
            self.observe_hold(Some(sender), HoldCallback::Image { items, providers }, NSDragOperation::None, false);
            queue_image_nudge(shared.clone(), hold.0, hold.1, ImageNudgeReason::ImageUpdate, providers, enumeration_returned);
        }

        #[unsafe(method(draggingExited:))]
        fn dragging_exited(&self, sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
            self.observe_hold(sender, HoldCallback::Exited, NSDragOperation::None, false);
            if self.ivars().role == Role::Source {
                self.mark_over_source(false);
            } else if !sender.is_some_and(|sender| self.observe_cancel_move(sender, true)) {
                end_held_source(&self.ivars().shared, false, NativeHoldEnd::DraggingExited);
            }
        }

        #[unsafe(method(draggingEnded:))]
        fn source_dragging_ended(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) {
            self.observe_hold(Some(sender), HoldCallback::Ended, NSDragOperation::None, false);
            if self.ivars().role == Role::Catcher { self.source_ended(false, NativeHoldEnd::DraggingEnded); }
        }

        #[unsafe(method(prepareForDragOperation:))]
        fn prepare(&self, _sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            self.ivars().role == Role::Catcher && self.ivars().shared.lock().releasing
        }

        #[unsafe(method(performDragOperation:))]
        fn perform(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            self.observe_hold(Some(sender), HoldCallback::Perform, NSDragOperation::None, false);
            let shared = &self.ivars().shared;
            let export = { let st = shared.lock(); st.releasing.then(|| st.export.as_ref().map(|(_, export)| export.clone())).flatten() };
            if self.ivars().role == Role::Catcher && let Some(export) = export {
                let accepted = UI.with(|ui| {
                    let receivers = ui.borrow().as_ref().and_then(|ui| ui.receivers.get(&export.operation.sequence).cloned());
                    receivers.is_some_and(|receivers| export.receive(&receivers, &sender.draggingPasteboard()))
                });
                if accepted { shared.lock().released = true; shared.cv.notify_all(); }
                self.source_ended(accepted, if accepted { NativeHoldEnd::DropAccepted } else { NativeHoldEnd::DropRejected });
                return accepted.into();
            }
            let (accept,source,root) = {
                let st = shared.lock();
                (self.ivars().role == Role::Catcher && st.releasing,st.source_data.as_ref().map(|(_,p)| p.clone()),st.content_root.clone())
            };
            let accepted = accept && match (source,root) {
                (Some(source),Some(root)) => {
                    let id = crate::content::Provider::inventory(source.as_ref()).id;
                    match super::content::snapshot_board(&sender.draggingPasteboard(),&root,id) {
                        Ok(snapshot) => source.complete(snapshot),
                        Err(error) => { source.fail(crate::content::reason(&error)); false }
                    }
                }
                // Standalone native probes can receive without the product store.
                (None,_) => true,
                (Some(source),None) => { source.fail(crate::model::Unavailable::StorageFull); false }
            };
            if accepted { shared.lock().released = true; shared.cv.notify_all(); }
            if self.ivars().role == Role::Catcher { self.source_ended(accepted, if accepted { NativeHoldEnd::DropAccepted } else { NativeHoldEnd::DropRejected }); }
            accepted
        }
    }

    impl DndView {
        // This legacy file-promise callback belongs to the view, not the
        // NSDraggingSource protocol. Native promise proxies still use it.
        #[unsafe(method_id(namesOfPromisedFilesDroppedAtDestination:))]
        fn promised_names(&self, destination: &NSURL) -> Option<Retained<NSArray<NSString>>> {
            (|| {
            let path = destination.path().map(|path| PathBuf::from(path.to_string()))?;
            let proxies = UI.with(|ui| ui.borrow().as_ref().map(|ui| ui.current_proxies.clone())).unwrap_or_default();
            let mut names = Vec::new();
            for proxy in proxies {
                match proxy.receive(&path) {
                    Ok(written) => names.extend(written.into_iter().map(|name| NSString::from_str(&name))),
                    Err(_) => { tracing::warn!("native legacy file promise failed"); return None; }
                }
            }
            Some(NSArray::from_retained_slice(&names))
            })()
        }
    }

    unsafe impl NSDraggingSource for DndView {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_mask(&self, _session: &NSDraggingSession, context: NSDraggingContext) -> NSDragOperation {
            let ops = self.ivars().shared.lock().ops;
            tracing::trace!(?ops, context = context.0, "drag source operation mask");
            ns_ops(ops)
        }

        #[unsafe(method(draggingSession:willBeginAtPoint:))]
        fn will_begin(&self, session: &NSDraggingSession, point: NSPoint) {
            // Numeric marker survives the diagnostic scalar filter. Its capture
            // timestamp is the native start boundary. AppKit can deliver it
            // after Begin returns; identify that session through the UI map.
            let at = Instant::now();
            tracing::debug!(dnd_phase = 28_u64, ?point, cursor = ?NSEvent::mouseLocation(), "AppKit drag will begin");
            let operation = UI.with(|u| u.borrow().as_ref()
                .and_then(|ui| ui.operations.get(&session.draggingSequenceNumber()).copied()))
                .or_else(|| self.ivars().shared.lock().starting_operation);
            if let Some(operation) = operation {
                self.ivars().shared.complete_native_begin(Some(operation), Some(at));
            }
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, session: &NSDraggingSession, _point: NSPoint, operation: NSDragOperation) {
            let shared = self.ivars().shared.clone();
            let dropped = operation != NSDragOperation::None;
            // Capture this AppKit session's identity before waking cancel().
            // That wake may let the resource owner prepare another network
            // session before emit runs; looking up the current peer there would
            // apply the old Ended to the new drag. This is a lifecycle fix, not
            // an OS behavior workaround; native acceptance remains separate.
            let operation = UI.with(|u| u.borrow_mut().as_mut()
                .and_then(|ui| ui.operations.remove(&session.draggingSequenceNumber())));
            {
                let mut st = shared.lock();
                st.active = false;
                st.ended = true;
                shared.cv.notify_all();
            }
            if let Some(operation) = operation {
                shared.complete_native_begin(Some(operation), None);
            }
            tracing::info!(dropped, ?operation, "drag started by Oink ended");
            UI.with(|u| {
                if let Some(ui) = u.borrow_mut().as_mut() {
                    if ui.target_session.as_ref().is_some_and(|(_, active)| active.draggingSequenceNumber() == session.draggingSequenceNumber()) {
                        ui.target_session = None;
                    }
                    ui.source.orderOut(None);
                    if let Some(cur) = ui.current.take() && !dropped {
                        cur.cancel();
                    }
                    ui.current_proxies.clear();
                }
            });
            if let Some(operation) = operation {
                shared.emit(PlatformDndEvent::Ended { operation, dropped });
            }
        }
    }
);

enum HoldCallback {
    Entered,
    Updated,
    Image { items: usize, providers: usize },
    Exited,
    Ended,
    Perform,
}
impl DndView {
    fn new(mtm: MainThreadMarker, role: Role, shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars { role, shared });
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SIZE, SIZE));
        // SAFETY: NSView 的指定初始化方法。
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn observe_hold(
        &self,
        sender: Option<&ProtocolObject<dyn NSDraggingInfo>>,
        callback: HoldCallback,
        returned: NSDragOperation,
        own: bool,
    ) {
        if self.ivars().role != Role::Catcher {
            return;
        }
        UI.with(|ui| {
            let mut ui = ui.borrow_mut();
            let Some(ui) = ui.as_mut() else { return };
            let Some(visual) = &mut ui.hold_visual else { return };
            if sender.is_some_and(|s| s.draggingSequenceNumber() as i64 != visual.summary.native_sequence) {
                return;
            }
            let elapsed = visual.started.elapsed().as_micros() as u64;
            let summary = &mut visual.summary;
            match callback {
                HoldCallback::Entered => summary.entered += 1,
                HoldCallback::Updated => {
                    summary.updated += 1;
                    summary.last_source_mask = sender.map_or(0, |s| s.draggingSourceOperationMask().0 as u64);
                    summary.last_returned_mask = returned.0 as u64;
                    summary.last_is_own = own;
                    if returned == NSDragOperation::None {
                        summary.returned_none += 1;
                        summary.first_none_us.get_or_insert(elapsed);
                    }
                }
                HoldCallback::Image { items, providers } => {
                    summary.image_updated += 1;
                    summary.update_nil_items += items as u64;
                    summary.providers_on_update += providers as u64;
                    if summary.first_image.is_none() {
                        summary.first_image = Some(destination_snapshot(&ui.catcher, sender));
                    }
                }
                HoldCallback::Exited => {
                    summary.exited += 1;
                    summary.first_exit_us.get_or_insert(elapsed);
                    summary.finish = destination_snapshot(&ui.catcher, sender);
                }
                HoldCallback::Ended => {
                    summary.ended += 1;
                    summary.finish = destination_snapshot(&ui.catcher, sender);
                }
                HoldCallback::Perform => {
                    summary.finish = destination_snapshot(&ui.catcher, sender);
                }
            }
        });
    }

    /// 拖拽来自 Oink 自己发起的拖拽（发起窗口）。
    fn is_own(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
        sender.draggingSource().is_some_and(|s| {
            let own = UI.with(|u| {
                u.borrow()
                    .as_ref()
                    .map(|ui| Retained::as_ptr(&ui.source_view) as *const AnyObject)
            });
            own == Some(Retained::as_ptr(&s))
        })
    }

    /// 发起窗口也注册了拖入类型（总是拒绝）：借拖入 / 离开知道拖拽会话此刻的目标是不是它。
    fn mark_over_source(&self, over: bool) {
        let shared = &self.ivars().shared;
        let mut st = shared.lock();
        st.over_source = over;
        shared.cv.notify_all();
    }

    /// Returns true when this exit belongs to cancel relocation, not an end.
    fn observe_cancel_move(&self, sender: &ProtocolObject<dyn NSDraggingInfo>, exited: bool) -> bool {
        let shared = &self.ivars().shared;
        {
            let st = shared.lock();
            if st.cancel_move.is_none() {
                return false;
            }
            if st.reported_seq != Some(sender.draggingSequenceNumber()) {
                return true;
            }
        }
        // NSDraggingInfo may omit the destination window in draggingExited.
        // Our retained catcher still defines the location's base coordinates.
        let location = sender.draggingLocation();
        let screen = sender
            .draggingDestinationWindow()
            .map(|window| window.convertPointToScreen(location))
            .or_else(|| UI.with(|ui| ui.borrow().as_ref().map(|ui| ui.catcher.convertPointToScreen(location))));
        let Some(screen) = screen else {
            return true;
        };
        let cg = to_cocoa(self.mtm(), Point::new(screen.x, screen.y));
        let cleanup = {
            let mut st = shared.lock();
            if st.cancel_move.is_none() {
                return false;
            }
            st.observe_cancel_position(Point::new(cg.x, cg.y), exited)
        };
        shared.cv.notify_all();
        if cleanup {
            self.source_ended(false, NativeHoldEnd::CancelRepositioned);
        }
        true
    }

    fn source_ended(&self, accepted: bool, reason: NativeHoldEnd) {
        end_held_source(&self.ivars().shared, accepted, reason);
    }

    fn entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
        if self.ivars().role == Role::Source {
            self.mark_over_source(true);
            return NSDragOperation::None;
        }
        if self.is_own(sender) {
            return NSDragOperation::None;
        }
        let shared = self.ivars().shared.clone();
        let seq = sender.draggingSequenceNumber();
        let ops = drag_ops(sender.draggingSourceOperationMask());
        if !ops.contains(DragOps::COPY) {
            return NSDragOperation::None;
        }
        // Retain only on AppKit's thread; never pass NSDraggingInfo to a worker.
        UI.with(|u| {
            if let Some(ui) = u.borrow_mut().as_mut() {
                // SAFETY: AppKit supplies a live Objective-C object to this
                // callback; the retained reference stays on this main thread.
                ui.source_info = unsafe { Retained::retain(sender as *const _ as *mut _) };
            }
        });
        let thumbnail_start = Instant::now();
        let operation = {
            let mut st = shared.lock();
            st.source_present = true;
            st.source_button = native_drag_button(self.mtm()).or(st.source_button);
            if st.probe_current(seq) {
                return NSDragOperation::Copy;
            }
            st.reported_seq = Some(seq);
            let operation = DragOperation::new();
            st.source_operation = Some(operation);
            if let Some(old) = st.thumbnail_budget.take() {
                old.cancel();
            }
            st.source_thumbnail = None;
            st.probe_operation = Some(operation);
            operation
        };
        tracing::debug!(seq, ?ops, "system drag entered the edge catcher: reading it");
        // Audit only fixed format-presence bits. Never export arbitrary UTI names
        // or read extra payloads just to diagnose a browser drag.
        let native_types = sender.draggingPasteboard().types();
        let dnd_formats = [
            "public.utf8-plain-text",
            "public.html",
            "public.rtf",
            "public.png",
            "public.tiff",
            "public.file-url",
            "public.url",
            "public.jpeg",
            "com.apple.pasteboard.promised-file-url",
            "com.apple.pasteboard.promised-file-content-type",
            "com.apple.pasteboard.promised-file-content",
            "com.compuserve.gif",
            "org.webmproject.webp",
        ]
        .iter()
        .enumerate()
        .fold(0_u64, |mask, (bit, name)| {
            mask | if native_types
                .as_ref()
                .is_some_and(|types| types.iter().any(|ty| ty.isEqualToString(&NSString::from_str(name))))
            {
                1 << bit
            } else {
                0
            }
        });
        tracing::debug!(
            dnd_formats,
            dnd_formats_complete = native_types.is_some(),
            "native drag offered formats"
        );
        let board = sender.draggingPasteboard();
        representation_trace::record(&board, operation, false);
        let promise_receivers = promise::receivers(&board);
        let promise_count = promise_receivers.as_ref().map(Vec::len);
        let single_promise = promise_receivers.as_ref().is_none_or(|receivers| {
            receivers.len() == 1
                && receivers[0].fileTypes().len() == 1
                && super::thumbnail::qualifies(1, &receivers[0].fileTypes().objectAtIndex(0).to_string())
        });
        if let Some(receivers) = promise_receivers {
            UI.with(|ui| {
                if let Some(ui) = ui.borrow_mut().as_mut() {
                    ui.receivers.insert(operation.sequence, receivers);
                }
            });
        }
        let inventory = super::content::drag_inventory(
            &board,
            oink_foundation::content::OfferId {
                clip: oink_foundation::content::ClipId {
                    origin: oink_foundation::DeviceId([0; 32]),
                    seq: 0,
                },
                generation: operation.sequence,
            },
        );
        if inventory.has_promises() && promise_count.is_none() {
            tracing::warn!("native promise receiver could not be constructed");
            return NSDragOperation::None;
        }
        let Ok(source) = crate::content::Deferred::new(inventory.clone(), Arc::new(super::content::MacTranslator))
        else {
            return NSDragOperation::None;
        };
        if single_promise && inventory.items.len() == 1 {
            start_thumbnail(
                shared.clone(),
                operation,
                board.name().to_string(),
                board.changeCount(),
                thumbnail_start,
            );
        }
        let board_name = board.name().to_string();
        let board_change = board.changeCount();
        let hover_shared = Arc::downgrade(&shared);
        let hover_inventory = inventory.clone();
        source.set_hover_reader(Arc::new(move |id| {
            let shared = hover_shared
                .upgrade()
                .ok_or_else(|| crate::content::unavailable(crate::model::Unavailable::Cancelled))?;
            let name = board_name.clone();
            let inventory = hover_inventory.clone();
            on_main_wait(Duration::from_secs(3), move |_| {
                let read = oink_foundation::drag_trace::Trace::new(
                    oink_foundation::drag_trace::stage::PASTEBOARD_READ,
                    oink_foundation::drag_trace::Context {
                        object: operation.sequence,
                        representation: id.0,
                        ..oink_foundation::drag_trace::Context::current()
                    },
                );
                let result = (|| {
                    use crate::model::{NativeFormat, Unavailable};
                    let current = shared.lock().source_operation == Some(operation);
                    let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&name));
                    if !current || board.changeCount() != board_change {
                        return Err(crate::content::unavailable(Unavailable::Superseded));
                    }
                    let (item, rep) = inventory
                        .items
                        .iter()
                        .find_map(|item| {
                            item.representations
                                .iter()
                                .find(|rep| rep.id == id)
                                .map(|rep| (item, rep))
                        })
                        .ok_or_else(|| crate::content::unavailable(Unavailable::NoSuchItem))?;
                    let NativeFormat::Mac(name) = &rep.format else {
                        return Err(crate::content::unavailable(Unavailable::Unsupported));
                    };
                    let raw = board
                        .pasteboardItems()
                        .and_then(|items| {
                            items
                                .iter()
                                .nth(item.id.0 as usize)
                                .and_then(|item| item.dataForType(&NSString::from_str(name)))
                        })
                        .ok_or_else(|| crate::content::unavailable(Unavailable::ReadFailed))?;
                    if raw.len() as u64 > crate::model::REPRESENTATION_BYTES {
                        return Err(crate::content::unavailable(Unavailable::LimitExceeded));
                    }
                    Ok(Box::new(std::io::Cursor::new(raw.to_vec())) as crate::FileReader)
                })();
                read.io(&result);
                result
            })
            .ok_or_else(|| crate::content::unavailable(crate::model::Unavailable::NoProgress))?
        }));
        {
            let mut st = shared.lock();
            if let Some((_, old)) = st.source_data.replace((operation, source.clone())) {
                old.fail(crate::model::Unavailable::Superseded);
            }
        }
        let roots: Vec<_> = board
            .pasteboardItems()
            .map(|items| {
                items
                    .iter()
                    .enumerate()
                    .filter_map(|(index, item)| {
                        super::content::file_path(&item).map(|path| (crate::model::NodeId(index as u64 + 1), path))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // File metadata work stays off AppKit. Representation bytes are read only
        // by the original session's real performDragOperation callback below.
        let spawned = std::thread::Builder::new().name("oink-dnd-read".into()).spawn(move || {
            if promise_count.is_some() {
                let root = shared.lock().content_root.clone();
                let export = root.and_then(|root| {
                    promise::Export::prepare(&root, operation, source.clone(), roots.clone(), &shared).ok()
                });
                let Some(export) = export else {
                    return;
                };
                let mut st = shared.lock();
                if st.source_operation != Some(operation) || !st.probing {
                    drop(st);
                    export.cancel(false);
                    return;
                }
                if let Some((_, old)) = st.export.replace((operation, export)) {
                    old.cancel(false);
                }
            }
            if promise_count.is_none() && !roots.is_empty() {
                let Ok(files) = crate::content::namespace::Local::new(roots) else {
                    return;
                };
                if !source.attach_files(files) {
                    return;
                }
            }
            use crate::content::Provider;
            let items = crate::content::items(&source.inventory());
            let current = {
                let st = shared.lock();
                st.source_operation == Some(operation) && st.probing
            };
            if current {
                shared.emit(PlatformDndEvent::Detected { operation, items, ops });
            }
        });
        if spawned.is_err() {
            tracing::warn!("drag metadata worker unavailable");
        }
        NSDragOperation::Copy
    }

    /// 发起窗口收到（注入的）左键按下：开始拖拽。
    fn start_drag(&self, event: &NSEvent) {
        let Some(_mtm) = MainThreadMarker::new() else {
            return;
        };
        let spec = UI.with(|u| u.borrow_mut().as_mut().and_then(|ui| ui.pending.take()));
        let Some(PreparedDrag {
            spec,
            provider,
            image,
            preview,
        }) = spec
        else {
            tracing::debug!("mouseDown on the drag source window without a pending drag");
            return;
        };
        let shared = self.ivars().shared.clone();
        let prepared_at = Instant::now();
        let items = dragging_items(
            &spec,
            image.as_ref(),
            provider.as_ref().map(Registration::top_level_urls),
            &preview,
        );
        let report_end = std::time::Instant::now();
        crate::dnd::preparation_timing(crate::dnd::PreparationStage::NativePreparation, prepared_at, report_end);
        tracing::debug!(
            dnd_phase = 23_u64,
            elapsed_us = report_end.saturating_duration_since(prepared_at).as_micros() as u64,
            "native drag writers and preview prepared"
        );
        if items.is_empty() {
            tracing::warn!("nothing to drag");
            return;
        }
        let frame = items[0].draggingFrame();
        tracing::debug!(
            dnd_preview_stage = 2_u64,
            dnd_preview_source_image = image.is_some(),
            dnd_preview_width = frame.size.width,
            dnd_preview_height = frame.size.height,
            dnd_preview_scale = self
                .window()
                .and_then(|window| window.screen())
                .map_or(0., |screen| screen.backingScaleFactor()),
            "target drag artwork geometry"
        );
        // 系统在发起拖拽的过程中就会问允许的操作（`source_mask`）：先记下。
        shared.lock().ops = spec.ops;
        let array = NSArray::from_retained_slice(&items);
        tracing::debug!(event = ?event.locationInWindow(), cursor = ?NSEvent::mouseLocation(),
            frame = ?self.window().map(|w| w.frame()), "starting AppKit dragging session");
        shared.lock().starting_operation = Some(spec.operation);
        let native_at = Instant::now();
        let session = self.beginDraggingSessionWithItems_event_source(&array, event, ProtocolObject::from_ref(self));
        // Observe AppKit's actual published board, including any native aliases;
        // inspecting the planned writers alone cannot prove what an app receives.
        representation_trace::record(&session.draggingPasteboard(), spec.operation, true);
        tracing::debug!(
            dnd_phase = 24_u64,
            elapsed_us = native_at.elapsed().as_micros() as u64,
            "AppKit drag session begin returned"
        );
        shared.lock().starting_operation = None;
        session.setAnimatesToStartingPositionsOnCancelOrFail(false);
        UI.with(|u| {
            if let Some(ui) = u.borrow_mut().as_mut() {
                ui.operations.insert(session.draggingSequenceNumber(), spec.operation);
                ui.target_session = Some((spec.operation, session.clone()));
                ui.current = provider;
            }
        });
        let mut st = shared.lock();
        st.began = true;
        st.active = true;
        shared.cv.notify_all();
        tracing::info!(items = items.len(), "drag started on this Mac");
    }
}

fn drag_ops(m: NSDragOperation) -> DragOps {
    let mut o = DragOps::default();
    if m.contains(NSDragOperation::Copy) || m.contains(NSDragOperation::Generic) {
        o = o.union(DragOps::COPY);
    }
    if m.contains(NSDragOperation::Move) {
        o = o.union(DragOps::MOVE);
    }
    if m.contains(NSDragOperation::Link) {
        o = o.union(DragOps::LINK);
    }
    o
}

fn ns_ops(o: DragOps) -> NSDragOperation {
    let mut m = NSDragOperation::None;
    if o.contains(DragOps::COPY) {
        m |= NSDragOperation::Copy | NSDragOperation::Generic;
    }
    if o.contains(DragOps::MOVE) {
        m |= NSDragOperation::Move;
    }
    if o.contains(DragOps::LINK) {
        m |= NSDragOperation::Link;
    }
    m
}

// ───────────── 拖拽内容 ─────────────

/// 拖拽的一项：写入剪贴板的对象与拖拽图像。
type Writer = (
    Retained<ProtocolObject<dyn NSPasteboardWriting>>,
    Option<Retained<objc2_app_kit::NSImage>>,
);

struct DragPromiseIvars {
    extra: Retained<NSPasteboardItem>,
    // NSFilePromiseProvider's delegate is weak. Tie it to the native writer,
    // which AppKit retains for queued writes, instead of retaining all delegates
    // in a session-wide set until even unrequested promises complete.
    _delegate: Retained<PromiseDelegate>,
}

// Preserve NSFilePromiseProvider identity/lazy delivery while adding other representations of the same item.
define_class!(
    #[unsafe(super(NSFilePromiseProvider))]
    #[name = "OinkDragFilePromise"]
    #[ivars = DragPromiseIvars]
    struct DragFilePromise;
    unsafe impl NSObjectProtocol for DragFilePromise {}
    unsafe impl NSPasteboardWriting for DragFilePromise {
        #[unsafe(method_id(writableTypesForPasteboard:))]
        fn types(&self, pb: &NSPasteboard) -> Retained<NSArray<NSString>> {
            // SAFETY: forward the exact NSPasteboardWriting signature to the superclass.
            let base: Retained<NSArray<NSString>> = unsafe { msg_send![super(self), writableTypesForPasteboard: pb] };
            let extras = self.ivars().extra.types();
            let placeholder = NSString::from_str("public.file-url");
            let mut types: Vec<_> = if extras.containsObject(&placeholder) {
                Vec::new()
            } else {
                base.iter().collect()
            };
            for ty in extras {
                if ty == placeholder {
                    for native in base.iter() {
                        if !types.contains(&native) {
                            types.push(native);
                        }
                    }
                } else if !types.contains(&ty) {
                    types.push(ty);
                }
            }
            NSArray::from_retained_slice(&types)
        }
        #[unsafe(method(writingOptionsForType:pasteboard:))]
        fn options(&self, ty: &NSString, pb: &NSPasteboard) -> NSPasteboardWritingOptions {
            if self.ivars().extra.types().containsObject(ty) {
                return NSPasteboardWritingOptions::empty();
            }
            // SAFETY: same signature; native file promises must retain their promised-writing option.
            unsafe { msg_send![super(self), writingOptionsForType: ty, pasteboard: pb] }
        }
        #[unsafe(method_id(pasteboardPropertyListForType:))]
        fn property(&self, ty: &NSString) -> Option<Retained<AnyObject>> {
            if self.ivars().extra.types().containsObject(ty) {
                self.ivars().extra.pasteboardPropertyListForType(ty)
            } else {
                // SAFETY: same NSPasteboardWriting signature.
                unsafe { msg_send![super(self), pasteboardPropertyListForType: ty] }
            }
        }
    }
);

fn representations(items: &[ClipData]) -> Retained<NSPasteboardItem> {
    let pi = NSPasteboardItem::new();
    for item in items {
        let (ty, bytes) = match item {
            ClipData::Text(t) => ("public.utf8-plain-text", t.as_bytes()),
            ClipData::Html(h) => ("public.html", h.as_bytes()),
            ClipData::Rtf(r) => ("public.rtf", r.as_slice()),
            ClipData::Png(p) => ("public.png", p.as_slice()),
            _ => continue,
        };
        pi.setData_forType(&NSData::with_bytes(bytes), &NSString::from_str(ty));
    }
    pi
}

/// File Provider and local files both offer real NSURL objects. PNG/text remain a single item.
fn dragging_items(
    spec: &DragSpec,
    source_image: Option<&oink_foundation::dnd::DragImage>,
    provider: Option<Vec<PathBuf>>,
    preview: &[ClipData],
) -> Vec<Retained<NSDraggingItem>> {
    UI.with(|ui| {
        if let Some(ui) = ui.borrow_mut().as_mut() {
            ui.current_proxies.clear();
        }
    });
    let mut writers: Vec<Writer> = Vec::new();
    if let Some(native) = &spec.native {
        let fallback = provider.is_none() && !native.inventory.roots.is_empty();
        let paths = provider.unwrap_or_default();
        let nodes: Vec<_> = native.inventory.roots.iter().map(|node| node.id).collect();
        let urls = nodes
            .into_iter()
            .zip(paths)
            .map(|(node, path)| (node, super::pasteboard::file_url_string(&path)))
            .collect();
        let native_writers = if fallback {
            let Some(contents) = &spec.files else {
                return Vec::new();
            };
            let Ok(items) = super::content::promise_items(native) else {
                return Vec::new();
            };
            let entries = Arc::new(native.files.clone());
            let queue = NSOperationQueue::new();
            queue.setMaxConcurrentOperationCount(4);
            let mut writers = Vec::new();
            for (node, extra) in items {
                if let Some(node) = node {
                    let Some(top) = native.inventory.roots.iter().position(|root| root.id == node) else {
                        return Vec::new();
                    };
                    let Some(entry) = entries.get(top) else {
                        return Vec::new();
                    };
                    let delegate = PromiseDelegate::new(top, entries.clone(), contents.clone(), queue.clone());
                    let ty = NSString::from_str(if entry.is_dir { "public.folder" } else { "public.data" });
                    let this = DragFilePromise::alloc().set_ivars(DragPromiseIvars {
                        extra,
                        _delegate: delegate.clone(),
                    });
                    // SAFETY: the native writer retains its delegate in its ivars.
                    let promise: Retained<DragFilePromise> = unsafe {
                        msg_send![super(this), initWithFileType: &*ty,
                            delegate: ProtocolObject::<dyn NSFilePromiseProviderDelegate>::from_ref(&*delegate)]
                    };
                    writers.push(ProtocolObject::from_retained(promise));
                } else {
                    writers.push(ProtocolObject::from_retained(extra));
                }
            }
            Ok(writers)
        } else {
            super::content::writers(native, &urls)
        };
        let Ok(native_writers) = native_writers else {
            return Vec::new();
        };
        let (native_writers, proxies) = if native.inventory.has_promises() {
            match proxy::wrap(native, native_writers) {
                Ok(wrapped) => wrapped,
                Err(_) => {
                    tracing::warn!("native promise proxy construction failed");
                    return Vec::new();
                }
            }
        } else {
            (native_writers, Vec::new())
        };
        UI.with(|ui| {
            if let Some(ui) = ui.borrow_mut().as_mut() {
                ui.current_proxies = proxies;
            }
        });
        let source = source_image.filter(|image| image.valid()).and_then(|image| {
            let image = crate::dnd::preview::fitted_image(image);
            let bitmap =
                objc2_app_kit::NSImage::initWithData(objc2_app_kit::NSImage::alloc(), &NSData::with_bytes(&image.png))?;
            // PNG resolution and AppKit point size are independent. The source
            // already converted its DPI; Retina must not divide this again.
            bitmap.setSize(NSSize::new(image.width, image.height));
            Some((image, bitmap))
        });
        let metadata = crate::dnd::preview::metadata(&crate::content::items(&native.inventory)).unwrap_or_default();
        let workspace = NSWorkspace::sharedWorkspace();
        let content_image = if native.inventory.roots.is_empty() && native.files.is_empty() {
            preview_image(preview)
        } else {
            None
        };
        let items = native_writers
            .into_iter()
            .enumerate()
            .map(|(index, writer)| {
                let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), &writer);
                let off = index.min(8) as f64 * 4.;
                let frame = source.as_ref().map_or_else(
                    || {
                        NSRect::new(
                            NSPoint::new(off, off),
                            content_image.as_ref().map_or(NSSize::new(SIZE, SIZE), |i| i.size()),
                        )
                    },
                    |(image, _)| {
                        NSRect::new(
                            NSPoint::new(SIZE / 2. - image.hotspot_x, SIZE / 2. - image.height + image.hotspot_y),
                            NSSize::new(image.width, image.height),
                        )
                    },
                );
                let icon = match source.as_ref() {
                    Some((_, bitmap)) if index == 0 => Some(bitmap.clone()),
                    Some(_) => None,
                    None if content_image.is_some() => (index == 0).then(|| content_image.as_ref().unwrap().clone()),
                    None => {
                        let entry = metadata.get(index);
                        let ty = if entry.is_some_and(|entry| entry.is_dir) {
                            "public.folder"
                        } else {
                            entry
                                .and_then(|entry| Path::new(&entry.path).extension())
                                .and_then(|s| s.to_str())
                                .unwrap_or("public.data")
                        };
                        // Files and unavailable hover content use type icons.
                        // Never ask a lazy writer for bytes inside native Begin.
                        #[allow(deprecated)]
                        Some(workspace.iconForFileType(&NSString::from_str(ty)))
                    }
                };
                let contents: Option<&AnyObject> = icon.as_deref().map(|image| image.as_ref());
                // SAFETY: NSImage is retained through this call and AppKit retains it.
                unsafe {
                    item.setDraggingFrame_contents(frame, contents);
                }
                item
            })
            .collect();
        return items;
    }
    let extra = representations(&spec.items);
    let queue = NSOperationQueue::new();
    queue.setMaxConcurrentOperationCount(4);
    let mut text: Option<&str> = None;
    let mut png: Option<&[u8]> = None;
    let mut html: Option<&str> = None;
    let mut rtf: Option<&[u8]> = None;
    let workspace = NSWorkspace::sharedWorkspace();
    for item in &spec.items {
        match item {
            ClipData::Files(entries) => {
                if provider.is_none()
                    && let Some(contents) = &spec.files
                {
                    let entries = Arc::new(entries.clone());
                    for (i, e) in entries.iter().enumerate().filter(|(_, e)| files::is_top_level(e)) {
                        let d = PromiseDelegate::new(i, entries.clone(), contents.clone(), queue.clone());
                        let uti = if e.is_dir { "public.folder" } else { "public.data" };
                        let extra = if writers.is_empty() {
                            extra.clone()
                        } else {
                            NSPasteboardItem::new()
                        };
                        let this = DragFilePromise::alloc().set_ivars(DragPromiseIvars {
                            extra,
                            _delegate: d.clone(),
                        });
                        // SAFETY: designated initializer; the writer retains the delegate.
                        let promise: Retained<DragFilePromise> = unsafe {
                            msg_send![super(this),
                            initWithFileType: &*NSString::from_str(uti), delegate: ProtocolObject::<dyn NSFilePromiseProviderDelegate>::from_ref(&*d)]
                        };

                        let ext = Path::new(&e.path).extension().map(|x| x.to_string_lossy().into_owned());
                        #[allow(deprecated)]
                        let icon = workspace.iconForFileType(&NSString::from_str(if e.is_dir {
                            "public.folder"
                        } else {
                            ext.as_deref().unwrap_or("public.data")
                        }));
                        writers.push((ProtocolObject::from_retained(promise), Some(icon)));
                    }
                } else {
                    let paths = provider.clone().unwrap_or_else(|| {
                        entries
                            .iter()
                            .filter(|e| files::is_top_level(e))
                            .filter_map(|e| e.local_path.clone())
                            .collect()
                    });
                    for path in paths {
                        let string = NSString::from_str(&path.to_string_lossy());
                        let url = NSURL::fileURLWithPath(&string);
                        let icon = workspace.iconForFile(&string);
                        if writers.is_empty() && !extra.types().is_empty() {
                            // URL stays the authoritative file representation; extra formats belong to this item.
                            let pb = NSPasteboard::pasteboardWithName(&NSString::from_str(DRAG_PASTEBOARD));
                            for ty in url.writableTypesForPasteboard(&pb) {
                                if let Some(value) = url.pasteboardPropertyListForType(&ty) {
                                    // NSPasteboardItem's property-list setter serializes strings as plist XML;
                                    // URL readers require the URL's UTF-8 string representation instead.
                                    if let Some(string) = value.downcast_ref::<NSString>() {
                                        extra.setString_forType(string, &ty);
                                    } else if let Some(data) = value.downcast_ref::<NSData>() {
                                        extra.setData_forType(data, &ty);
                                    } else {
                                        // SAFETY: NSURL supplied this property list for this exact pasteboard type.
                                        unsafe {
                                            extra.setPropertyList_forType(&value, &ty);
                                        }
                                    }
                                }
                            }
                            writers.push((ProtocolObject::from_retained(extra.clone()), Some(icon)));
                        } else {
                            writers.push((ProtocolObject::from_retained(url), Some(icon)));
                        }
                    }
                }
            }
            ClipData::Text(t) => text = Some(t),
            ClipData::Png(p) => png = Some(p),
            ClipData::Html(h) => html = Some(h),
            ClipData::Rtf(r) => rtf = Some(r),
            _ => {}
        }
    }
    if writers.is_empty() && (png.is_some() || text.is_some() || html.is_some() || rtf.is_some()) {
        let pi = NSPasteboardItem::new();
        let mut image = None;
        if let Some(png) = png {
            let data = NSData::with_bytes(png);
            pi.setData_forType(&data, &NSString::from_str("public.png"));
            image = objc2_app_kit::NSImage::initWithData(objc2_app_kit::NSImage::alloc(), &data);
        }
        if let Some(t) = text {
            pi.setString_forType(&NSString::from_str(t), &NSString::from_str("public.utf8-plain-text"));
        }
        if let Some(h) = html {
            pi.setData_forType(&NSData::with_bytes(h.as_bytes()), &NSString::from_str("public.html"));
        }
        if let Some(r) = rtf {
            pi.setData_forType(&NSData::with_bytes(r), &NSString::from_str("public.rtf"));
        }
        if image.is_none() && (text.is_some() || rtf.is_some()) {
            image = Some(text_drag_image(text.unwrap_or(""), rtf));
        }
        writers.push((ProtocolObject::from_retained(pi), image));
    }
    let source = source_image.filter(|image| image.valid()).and_then(|image| {
        let image = crate::dnd::preview::fitted_image(image);
        let bitmap =
            objc2_app_kit::NSImage::initWithData(objc2_app_kit::NSImage::alloc(), &NSData::with_bytes(&image.png))?;
        bitmap.setSize(NSSize::new(image.width, image.height));
        Some((image, bitmap))
    });
    writers
        .into_iter()
        .enumerate()
        .map(|(n, (w, icon))| {
            let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), &w);
            let off = (n.min(8) as f64) * 4.0;
            let size = icon.as_ref().map_or(NSSize::new(SIZE, SIZE), |icon| {
                let original = icon.size();
                let text_preview = source_image.is_none() && png.is_none() && writers_are_text(&spec.items);
                let limit = if text_preview { (480., 240.) } else { (160., 80.) };
                let scale = (limit.0 / original.width.max(1.0))
                    .min(limit.1 / original.height.max(1.0))
                    .min(1.0);
                NSSize::new(original.width * scale, original.height * scale)
            });
            let frame = source.as_ref().map_or_else(
                || NSRect::new(NSPoint::new(off, off), size),
                |(image, _)| {
                    NSRect::new(
                        NSPoint::new(SIZE / 2. - image.hotspot_x, SIZE / 2. - image.height + image.hotspot_y),
                        NSSize::new(image.width, image.height),
                    )
                },
            );
            let icon = match source.as_ref() {
                Some((_, bitmap)) if n == 0 => Some(bitmap.clone()),
                Some(_) => None,
                None => icon,
            };
            let contents: Option<&AnyObject> = icon.as_deref().map(|i| i.as_ref());
            // SAFETY: contents 是 NSImage（或 nil），frame 在视图坐标里。
            unsafe { item.setDraggingFrame_contents(frame, contents) };
            item
        })
        .collect()
}

fn writers_are_text(items: &[ClipData]) -> bool {
    !items.iter().any(|item| matches!(item, ClipData::Files(_)))
}

/// The native representation path uses the same content renderers as the
/// legacy path. Bytes were prepared off AppKit; no provider call is made here.
fn preview_image(items: &[ClipData]) -> Option<Retained<objc2_app_kit::NSImage>> {
    if let Some(png) = items.iter().find_map(|item| match item {
        ClipData::Png(png) => Some(png),
        _ => None,
    }) && let Some(image) =
        objc2_app_kit::NSImage::initWithData(objc2_app_kit::NSImage::alloc(), &NSData::with_bytes(png))
    {
        let size = image.size();
        let scale = (160. / size.width.max(1.)).min(80. / size.height.max(1.)).min(1.);
        image.setSize(NSSize::new(size.width * scale, size.height * scale));
        return Some(image);
    }
    let text = items.iter().find_map(|item| match item {
        ClipData::Text(text) => Some(text.as_str()),
        _ => None,
    });
    let rtf = items.iter().find_map(|item| match item {
        ClipData::Rtf(rtf) => Some(rtf.as_slice()),
        _ => None,
    });
    (text.is_some() || rtf.is_some()).then(|| text_drag_image(text.unwrap_or(""), rtf))
}

/// 在目标端生成文字预览，不传输源应用的截图或增加协议字段。
fn text_drag_image(text: &str, rtf: Option<&[u8]>) -> Retained<objc2_app_kit::NSImage> {
    use objc2_app_kit::{
        NSAttributedStringAppKitDocumentFormats, NSAttributedStringNSExtendedStringDrawing, NSImage,
        NSStringDrawingOptions,
    };
    use objc2_foundation::{NSAttributedString, NSRange};
    // RTF preserves fonts, sizes, colors and explicit line breaks. HTML's AppKit importer can
    // synchronously load external resources/run WebKit, so it is deliberately not used on the drag path.
    let attributed = rtf
        .filter(|data| data.len() <= 256 * 1024)
        .and_then(|data| {
            // SAFETY: no document-attributes out pointer; AppKit validates the RTF data.
            unsafe {
                NSAttributedString::initWithRTF_documentAttributes(
                    NSAttributedString::alloc(),
                    &NSData::with_bytes(data),
                    None,
                )
            }
        })
        .unwrap_or_else(|| NSAttributedString::initWithString(NSAttributedString::alloc(), &NSString::from_str(text)));
    // Bound rendering work, without flattening whitespace or splitting UTF-16 surrogate pairs.
    let string = attributed.string().to_string();
    let length = string.chars().take(512).map(char::len_utf16).sum();
    let preview = attributed.attributedSubstringFromRange(NSRange::new(0, length));
    let options = NSStringDrawingOptions::UsesLineFragmentOrigin | NSStringDrawingOptions::UsesFontLeading;
    let natural = preview
        .boundingRectWithSize_options_context(NSSize::new(480., 240.), options, None)
        .size;
    let size = NSSize::new(
        natural.width.ceil().clamp(1., 480.),
        natural.height.ceil().clamp(1., 240.),
    );
    let image = NSImage::initWithSize(NSImage::alloc(), size);
    #[allow(deprecated)]
    image.lockFocus();
    // A fresh NSImage is transparent; do not paint the previous white rectangle.
    preview.drawWithRect_options_context(NSRect::new(NSPoint::new(0., 0.), size), options, None);
    #[allow(deprecated)]
    image.unlockFocus();
    image
}

struct PromiseIvars {
    /// 这个承诺对应清单里的哪个顶层条目。
    top: usize,
    entries: Arc<Vec<FileEntry>>,
    contents: Arc<dyn FileContents>,
    queue: Retained<NSOperationQueue>,
}

define_class!(
    /// 一个顶层条目的文件承诺：放下后把它（文件夹递归）写到目标位置。
    #[unsafe(super(NSObject))]
    #[name = "OinkDndPromise"]
    #[ivars = PromiseIvars]
    struct PromiseDelegate;

    unsafe impl NSObjectProtocol for PromiseDelegate {}

    unsafe impl NSFilePromiseProviderDelegate for PromiseDelegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(&self, _p: &NSFilePromiseProvider, _ty: &NSString) -> Retained<NSString> {
            let iv = self.ivars();
            NSString::from_str(&iv.entries[iv.top].path)
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write(&self, _p: &NSFilePromiseProvider, url: &NSURL, done: &block2::DynBlock<dyn Fn(*mut NSError)>) {
            let iv = self.ivars();
            let dest = url.path().map(|p| PathBuf::from(p.to_string()));
            let started = Instant::now();
            let result = match dest {
                Some(d) => write_top(&iv.entries, iv.top, &d, &iv.contents),
                None => Err(std::io::Error::other("promise destination is not a file URL")),
            };
            match result {
                Ok(_) => {
                    tracing::info!(took_ms = started.elapsed().as_millis() as u64, "dropped files written");
                    done.call((std::ptr::null_mut(),));
                }
                Err(_) => {
                    tracing::warn!("writing dropped files failed");
                    // SAFETY: 标准的 NSError 构造；userInfo 为空。
                    let err = unsafe { NSError::errorWithDomain_code_userInfo(NSCocoaErrorDomain, 512, None) };
                    done.call((Retained::as_ptr(&err) as *mut NSError,));
                }
            }
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn queue(&self, _p: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }
    }
);

impl PromiseDelegate {
    fn new(
        top: usize,
        entries: Arc<Vec<FileEntry>>,
        contents: Arc<dyn FileContents>,
        queue: Retained<NSOperationQueue>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(PromiseIvars {
            top,
            entries,
            contents,
            queue,
        });
        // SAFETY: NSObject 的 init。
        unsafe { msg_send![super(this), init] }
    }
}

/// 清单下标的映射：把子清单的下标换成完整清单的下标（`materialize` 按子清单编号打开）。
struct Subset {
    inner: Arc<dyn FileContents>,
    map: Vec<usize>,
}

impl FileContents for Subset {
    fn open(&self, index: usize) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        let i = *self
            .map
            .get(index)
            .ok_or_else(|| std::io::Error::other("index out of range"))?;
        self.inner.open(i)
    }

    fn metadata(&self, index: usize) -> std::io::Result<Option<crate::FileMetadata>> {
        let i = *self
            .map
            .get(index)
            .ok_or_else(|| std::io::Error::other("index out of range"))?;
        self.inner.metadata(i)
    }
    fn cancel(&self) {
        self.inner.cancel();
    }
}

/// 把第 `top` 个顶层条目（文件夹递归）写到 `dest`。`dest` 已存在时换成「名字 2」之类，不覆盖已有的东西。
/// 返回实际写到的路径。
fn write_top(
    entries: &[FileEntry],
    top: usize,
    dest: &Path,
    contents: &Arc<dyn FileContents>,
) -> std::io::Result<PathBuf> {
    // Native namespaces initially contain roots only. Expand the promise's
    // immutable descriptor table on its actual write request, then map to that
    // table's indices; otherwise a dropped folder becomes an empty directory.
    let root = entries.get(top).ok_or(std::io::ErrorKind::InvalidInput)?.path.clone();
    let expanded = contents.descriptor_entries()?;
    let entries = expanded.as_deref().unwrap_or(entries);
    let top = entries
        .iter()
        .position(|entry| entry.path == root)
        .ok_or(std::io::ErrorKind::InvalidData)?;
    let dest = unique_path(dest);
    let root = &entries[top].path;
    let prefix = format!("{root}/");
    let mut sub = Vec::new();
    let mut dests = Vec::new();
    let mut map = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let rest = if i == top {
            Some("")
        } else {
            e.path.strip_prefix(&prefix)
        };
        let Some(rest) = rest else { continue };
        let mut d = dest.clone();
        for c in rest.split('/').filter(|c| !c.is_empty()) {
            d.push(c);
        }
        sub.push(FileEntry {
            local_path: None,
            ..e.clone()
        });
        dests.push(d);
        map.push(i);
    }
    let subset: Arc<dyn FileContents> = Arc::new(Subset {
        inner: contents.clone(),
        map,
    });
    files::materialize(&sub, &dests, &subset)?;
    Ok(dest)
}

fn unique_path(p: &Path) -> PathBuf {
    if std::fs::symlink_metadata(p).is_err() {
        return p.to_path_buf();
    }
    let parent = p.parent().unwrap_or(Path::new("/"));
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name.as_str(), ""),
    };
    (2..)
        .map(|n| parent.join(format!("{stem} {n}{ext}")))
        .find(|c| std::fs::symlink_metadata(c).is_err())
        .expect("总能找到不重复的名字")
}

// ───────────── 窗口 ─────────────

fn make_panel(mtm: MainThreadMarker, view: &NSView) -> Retained<NSPanel> {
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SIZE, SIZE));
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        rect,
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setOpaque(false);
    // AppKit excludes fully transparent pixels from hit-testing. A 1% alpha
    // surface receives native drags without a visible catcher (macOS 14/26).
    panel.setBackgroundColor(Some(&NSColor::colorWithRed_green_blue_alpha(1.0, 0.0, 0.0, 0.01)));
    panel.setHasShadow(false);
    panel.setIgnoresMouseEvents(false);
    let policy = oink_foundation::panel_policy::PanelPolicy::BACKEND_OVERLAY;
    panel.setHidesOnDeactivate(policy.hides_on_deactivate);
    // Hide Others can hide the accessory backend while sharing stays active.
    // Deactivation policy alone does not keep catcher/source panels available.
    panel.setCanHide(policy.can_hide);
    panel.setFloatingPanel(true);
    // 在 setFloatingPanel 之后设（它会把层级改回浮动窗口层级）。
    panel.setLevel(NSPopUpMenuWindowLevel);
    panel.setBecomesKeyOnlyIfNeeded(true);
    // SAFETY: 窗口由我们一直持有，关闭时不释放。
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel.setContentView(Some(view));
    panel
}

/// 本机全局坐标（CoreGraphics：左上原点、y 向下）→ Cocoa 屏幕坐标（主屏左下原点、y 向上）。
fn to_cocoa(mtm: MainThreadMarker, p: Point) -> NSPoint {
    let h = NSScreen::screens(mtm)
        .firstObject()
        .map_or(0.0, |s| s.frame().size.height);
    NSPoint::new(p.x, h - p.y)
}

fn catcher_frame(at: NSPoint, screens: &[NSRect]) -> NSRect {
    // Keep one stationary strip while metadata is read asynchronously. Moving
    // a cursor-sized panel can cause draggingExited before HoldSource arrives.
    // Use the full screen frame, including the menu bar/notch band: visibleFrame
    // excludes the top pixels where crossing is requested. Native drag probes
    // on macOS 26 enter this inset strip before the Mission Control hot edge;
    // the old half-offscreen square can receive no Enter until mouse-up.
    for b in screens {
        if at.x < b.origin.x - 1.0
            || at.x > b.origin.x + b.size.width + 1.0
            || at.y < b.origin.y - 1.0
            || at.y > b.origin.y + b.size.height + 1.0
        {
            continue;
        }
        let edges = [
            ((at.y - b.origin.y - b.size.height).abs(), 0),
            ((at.y - b.origin.y).abs(), 1),
            ((at.x - b.origin.x).abs(), 2),
            ((at.x - b.origin.x - b.size.width).abs(), 3),
        ];
        let (distance, edge) = edges.into_iter().min_by(|a, b| a.0.total_cmp(&b.0)).unwrap();
        if distance <= 1.0 {
            let w = SIZE.min(b.size.width);
            let h = SIZE.min(b.size.height);
            return match edge {
                0 => NSRect::new(
                    NSPoint::new(b.origin.x, b.origin.y + b.size.height - h),
                    NSSize::new(b.size.width, h),
                ),
                1 => NSRect::new(b.origin, NSSize::new(b.size.width, h)),
                2 => NSRect::new(b.origin, NSSize::new(w, b.size.height)),
                _ => NSRect::new(
                    NSPoint::new(b.origin.x + b.size.width - w, b.origin.y),
                    NSSize::new(w, b.size.height),
                ),
            };
        }
    }
    NSRect::new(
        NSPoint::new(at.x - SIZE / 2.0, at.y - SIZE / 2.0),
        NSSize::new(SIZE, SIZE),
    )
}

/// 把窗口放到以 `at`（Cocoa 坐标）为中心的位置并显示（不激活应用）。
fn show_at(panel: &NSPanel, at: NSPoint, role: Role) {
    let frame = if role == Role::Catcher {
        let screens: Vec<_> = NSScreen::screens(panel.mtm()).iter().map(|s| s.frame()).collect();
        catcher_frame(at, &screens)
    } else {
        NSRect::new(
            NSPoint::new(at.x - SIZE / 2.0, at.y - SIZE / 2.0),
            NSSize::new(SIZE, SIZE),
        )
    };
    if role == Role::Catcher {
        CATCHER_SHOWS.set(CATCHER_SHOWS.get().saturating_add(1));
        if panel.frame() != frame {
            CATCHER_MOVES.set(CATCHER_MOVES.get().saturating_add(1));
        }
    }
    panel.setFrame_display(frame, false);
    panel.orderFrontRegardless();
    tracing::debug!(
        x = frame.origin.x,
        y = frame.origin.y,
        visible = panel.isVisible(),
        "drag-and-drop window shown"
    );
}

/// 当前光标（本机全局坐标，左上原点）。
fn cursor_cg() -> Option<Point> {
    on_main_wait(WAIT, |mtm| {
        let p = NSEvent::mouseLocation();
        let h = NSScreen::screens(mtm)
            .firstObject()
            .map_or(0.0, |s| s.frame().size.height);
        Point::new(p.x, h - p.y)
    })
}

/// 窗口服务器是否已经把 `panel` 当作 `at`（Cocoa 坐标）处的点击目标（刚显示或挪动的窗口要过几毫秒才生效，
/// 在那之前注入的点击会落到下面的窗口上）。
fn hit_at(mtm: MainThreadMarker, panel: &NSPanel, at: NSPoint) -> bool {
    NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(at, 0, mtm) == panel.windowNumber()
}

/// 把 `which` 窗口放到光标下，直到窗口服务器把它当作光标处的点击目标（最多 `WAIT`）。光标可能还在动
/// （用户的手没停、远端的位移还在注入），所以每次都按当前的光标位置放，没对上就重放。
fn place_under_cursor(which: Role) -> bool {
    place_and_press(which, None).is_some()
}

#[derive(Clone, Copy)]
enum PressOutcome {
    Submitted,
    NotSubmitted,
    /// A generic injector error does not say whether it posted before failing.
    Unconfirmed,
}
#[derive(Default)]
enum PressState {
    #[default]
    Waiting,
    InFlight,
    Finished(PressOutcome),
    Cancelled,
    CleanupIssued,
}
impl PressState {
    fn may_have_posted(&self) -> bool {
        matches!(
            self,
            Self::InFlight | Self::Finished(PressOutcome::Submitted | PressOutcome::Unconfirmed)
        )
    }
}

/// Coordinates the main-queue deadline with the first possible DOWN. A timeout
/// before try_begin cancels the closure; one during the press retains its cleanup
/// obligation. The cleanup runs later on the same queue, never ahead of that DOWN.
#[derive(Default)]
struct PressAttempt {
    state: Mutex<PressState>,
    offset: Mutex<Point>,
    dropped: std::sync::atomic::AtomicU64,
    critical_us: std::sync::atomic::AtomicU64,
}
impl PressAttempt {
    fn try_begin(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(*state, PressState::Waiting) {
            return false;
        }
        *state = PressState::InFlight;
        true
    }

    fn finish(&self, outcome: PressOutcome) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *state = PressState::Finished(outcome);
        state.may_have_posted()
    }

    fn stop_waiting(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, PressState::Waiting) {
            *state = PressState::Cancelled;
        }
        state.may_have_posted()
    }

    fn take_release(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.may_have_posted() {
            return false;
        }
        *state = PressState::CleanupIssued;
        true
    }
}

/// Keep the hit-test sample and button position together on the main thread.
/// NSEvent::mouseLocation can lag CGEvent's current location during continuous
/// physical motion; mixing the two allowed a down outside the 48 pt panel.
fn place_and_press(which: Role, press: Option<Arc<Shared>>) -> Option<Arc<PressAttempt>> {
    let started = Instant::now();
    while started.elapsed() < WAIT {
        let press = press.clone();
        let attempt = Arc::new(PressAttempt::default());
        let pending = attempt.clone();
        let hit = on_main_wait(WAIT, move |mtm| {
            // A stalled main queue may run this after the caller timed out. Never
            // post a late down after failed-start cleanup has already run.
            if started.elapsed() >= WAIT {
                return None;
            }
            UI.with(|u| {
                u.borrow().as_ref().map(|ui| {
                    let panel = if which == Role::Catcher {
                        &ui.catcher
                    } else {
                        &ui.source
                    };
                    let Some(event) = objc2_core_graphics::CGEvent::new(None) else {
                        return false;
                    };
                    let cg = objc2_core_graphics::CGEvent::location(Some(&event));
                    let at = Point::new(cg.x, cg.y);
                    let p = to_cocoa(mtm, at);
                    let frame = panel.frame();
                    let inside = p.x >= frame.origin.x
                        && p.x < frame.origin.x + frame.size.width
                        && p.y >= frame.origin.y
                        && p.y < frame.origin.y + frame.size.height;
                    if inside && hit_at(mtm, panel, p) {
                        if started.elapsed() >= WAIT {
                            return false;
                        }
                        tracing::debug!(?which, ?at, ?frame, "validated drag window hit position");
                        return press.as_ref().is_none_or(|shared| {
                            *pending.offset.lock().unwrap() = Point::new(
                                p.x - frame.origin.x - frame.size.width / 2.,
                                -(p.y - frame.origin.y - frame.size.height / 2.),
                            );
                            if !pending.try_begin() {
                                return false;
                            }
                            let mut outcome = PressOutcome::NotSubmitted;
                            let mut submit = || {
                                let submitted_at = Instant::now();
                                outcome = if let Some(press) = shared.source_press.get() {
                                    // macOS press_drag_at reports submission separately from
                                    // its tap ACK: Err means no DOWN was submitted, so retry
                                    // is safe. AppKit began is still checked by begin_prepared.
                                    match ui
                                        .pending
                                        .as_ref()
                                        .ok_or_else(|| "drag no longer pending".to_string())
                                        .and_then(|p| press(p.spec.operation, at))
                                    {
                                        Ok(()) => PressOutcome::Submitted,
                                        Err(error) => {
                                            tracing::warn!(%error, "drag source DOWN was not submitted");
                                            PressOutcome::NotSubmitted
                                        }
                                    }
                                } else if shared.inject.get().is_none() {
                                    PressOutcome::NotSubmitted
                                } else {
                                    let accepted = shared.inject(InjectEvent::Button {
                                        button: MouseButton::Left,
                                        pressed: true,
                                    });
                                    if accepted {
                                        PressOutcome::Submitted
                                    } else {
                                        PressOutcome::Unconfirmed
                                    }
                                };
                                pending
                                    .critical_us
                                    .store(submitted_at.elapsed().as_micros() as u64, Ordering::Relaxed);
                            };
                            if let Some(start) = shared.source_start.get() {
                                if let Ok(dropped) = start(&mut submit) {
                                    pending.dropped.store(dropped, Ordering::Relaxed);
                                }
                            } else {
                                submit();
                            }
                            pending.finish(outcome)
                        });
                    }
                    // Repeatedly repositioning/ordering a window already covering the pointer
                    // restarts WindowServer hit testing while it is catching up with the first show.
                    if !panel.isVisible()
                        || p.x < frame.origin.x
                        || p.x >= frame.origin.x + frame.size.width
                        || p.y < frame.origin.y
                        || p.y >= frame.origin.y + frame.size.height
                    {
                        show_at(panel, p, which);
                    }
                    false
                })
            })
        });
        match hit {
            Some(Some(true)) => {
                tracing::debug!(
                    ?which,
                    waited_ms = started.elapsed().as_millis() as u64,
                    "window is under the cursor"
                );
                return Some(attempt);
            }
            Some(Some(false)) => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                // Atomically disarm a not-yet-running press. If it already won
                // try_begin, never replay; retain it for AppKit start/cleanup.
                let may_have_posted = attempt.stop_waiting();
                if may_have_posted {
                    tracing::warn!(
                        ?which,
                        "drag source press outlived the main-queue wait; retaining cleanup"
                    );
                }
                return may_have_posted.then_some(attempt);
            }
        }
    }
    tracing::warn!(?which, "window did not become the click target in time");
    None
}

// ───────────── 后端 ─────────────

pub(crate) struct MacDnd {
    shared: Arc<Shared>,
}

impl MacDnd {
    fn cancel_using(&self, release: &dyn Fn() -> Result<(), String>) -> bool {
        self.shared.complete_native_begin(None, None);
        {
            let mut st = self.shared.lock();
            if !st.active {
                return true;
            }
            st.ended = false;
        }
        // 把发起窗口挪到光标下再抬起：放在不接受拖入的窗口上，拖拽失败，不会落到任何程序里。
        self.shared.lock().over_source = false;
        let placed_at = Instant::now();
        let placed = place_under_cursor(Role::Source);
        tracing::debug!(
            dnd_phase = 25_u64,
            elapsed_us = placed_at.elapsed().as_micros() as u64,
            success = placed,
            "drag cancel window placement finished"
        );
        if !placed {
            tracing::warn!("cancelling the drag: the source window is not under the cursor");
        }
        // 拖拽会话只在收到拖动事件时重新判断光标下的目标：光标不动时它仍以为目标是之前那个窗口，
        // 这时抬起会放到那里（Mac peer 实测：Esc 取消后文件落进了 Finder）。原地轻微拖动，直到它认发起窗口为目标。
        let noticed_at = Instant::now();
        let mut count = 0_u64;
        for i in 0..20 {
            if self.shared.lock().over_source {
                break;
            }
            if let Some(c) = cursor_cg() {
                count += 1;
                let d = if i % 2 == 0 { 1.0 } else { -1.0 };
                self.shared.inject(InjectEvent::MoveTo {
                    pos: Point::new(c.x + d, c.y),
                });
            }
            self.shared.wait(Duration::from_millis(25), |st| st.over_source);
        }
        let noticed = self.shared.lock().over_source;
        tracing::debug!(
            dnd_phase = 26_u64,
            elapsed_us = noticed_at.elapsed().as_micros() as u64,
            count,
            success = noticed,
            "drag cancel window recognition finished"
        );
        if !noticed {
            tracing::warn!("cancelling the drag: the drag session did not notice the source window");
        }
        let ended_at = Instant::now();
        let up = match release() {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "hosted drag cancel injection failed");
                false
            }
        };
        let ok = up && self.shared.wait(WAIT, |st| st.ended);
        tracing::debug!(
            dnd_phase = 27_u64,
            elapsed_us = ended_at.elapsed().as_micros() as u64,
            success = ok,
            "drag cancel release and native-end acknowledgement finished"
        );
        if !ok {
            tracing::warn!(up, "the drag did not end after cancelling");
        }
        ok
    }

    fn release_using(&self, release: &dyn Fn() -> Result<(), String>) -> bool {
        {
            let mut st = self.shared.lock();
            st.releasing = true;
            st.released = false;
        }
        let injected = match release() {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "source drag release injection failed");
                false
            }
        };
        let ok = injected && self.shared.wait(WAIT, |st| st.released);
        {
            let mut st = self.shared.lock();
            st.releasing = false;
            st.probing = false;
            st.reported_seq = None;
        }
        on_main(|_| {
            UI.with(|u| {
                if let Some(ui) = u.borrow().as_ref() {
                    ui.catcher.orderOut(None);
                }
            });
        });
        if !ok {
            tracing::warn!(injected, "source drag did not end on the catcher");
        }
        ok
    }

    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                sink: OnceLock::new(),
                inject: OnceLock::new(),
                source_press: OnceLock::new(),
                source_start: OnceLock::new(),
                image_nudge: OnceLock::new(),
                st: Mutex::new(St::default()),
                cv: Condvar::new(),
            }),
        }
    }
}

impl MacDnd {
    fn begin_prepared(&self, spec: DragSpec, provider: Option<Registration>) -> Result<(), ClipboardError> {
        self.shared.reset_native_begin(spec.operation);
        let mut trace = SourceStartTrace::default();
        tracing::debug!(?spec, "starting a drag");
        {
            let mut st = self.shared.lock();
            st.began = false;
            st.press_received = false;
            st.ended = false;
        }
        let (image, preview) = {
            let mut st = self.shared.lock();
            let preview = st
                .target_preview
                .take()
                .filter(|(op, _)| *op == spec.operation)
                .map(|(_, preview)| preview)
                .unwrap_or_default();
            (st.target_image.take(), preview)
        };
        let prepared_at = Instant::now();
        // 按当前光标放（`spec.at` 是焦点移交时的落点，光标之后可能已经动了）。
        let placed = on_main_wait(WAIT, move |_| {
            UI.with(|u| {
                let mut u = u.borrow_mut();
                let Some(ui) = u.as_mut() else { return false };
                ui.pending = Some(PreparedDrag {
                    spec,
                    provider,
                    image,
                    preview,
                });
                true
            })
        });
        tracing::debug!(
            dnd_phase = 20_u64,
            elapsed_us = prepared_at.elapsed().as_micros() as u64,
            success = placed == Some(true),
            "drag pending-window main-queue round trip finished"
        );
        if placed != Some(true) {
            return Err(ClipboardError::Platform("drag source window unavailable".into()));
        }
        let pressed_at = Instant::now();
        // WindowServer hit-test polling can take hundreds of milliseconds.
        // It runs with motion enabled. place_and_press gates only its final
        // positioned DOWN, on the same main-queue turn as the hit-test sample.
        let down = place_and_press(Role::Source, Some(self.shared.clone()));
        let receipt_at = Instant::now();
        let received = down.is_some() && self.shared.wait(WAIT, |st| st.press_received);
        trace.offset = down.as_ref().map(|attempt| *attempt.offset.lock().unwrap());
        trace.received = received;
        trace.dropped = down
            .as_ref()
            .map_or(0, |attempt| attempt.dropped.load(Ordering::Relaxed));
        trace.critical_us = down
            .as_ref()
            .map_or(0, |attempt| attempt.critical_us.load(Ordering::Relaxed));
        tracing::debug!(
            dnd_phase = 21_u64,
            elapsed_us = pressed_at.elapsed().as_micros() as u64,
            success = down.is_some(),
            "drag source window placement and press finished"
        );
        let native_at = Instant::now();
        let remaining = WAIT.saturating_sub(receipt_at.elapsed());
        let began = received && self.shared.wait(remaining, |st| st.began);
        tracing::debug!(
            dnd_phase = 22_u64,
            elapsed_us = native_at.elapsed().as_micros() as u64,
            success = began,
            "drag native begin acknowledgement finished"
        );
        if began {
            return Ok(());
        }
        tracing::warn!(
            down = down.is_some(),
            "the drag did not start: scheduling button cleanup"
        );
        let shared = self.shared.clone();
        on_main(move |_| {
            // A main-queue deadline can expire while press_drag_at is still
            // running. Queue its UP after that closure, so cleanup cannot race
            // ahead of a late DOWN. A definitive pre-submit rejection needs no UP.
            // Scheduling this closure is not an OS/application release receipt.
            if down.as_ref().is_some_and(|attempt| attempt.take_release()) {
                shared.inject(InjectEvent::Button {
                    button: MouseButton::Left,
                    pressed: false,
                });
            }
            UI.with(|u| {
                if let Some(ui) = u.borrow_mut().as_mut() {
                    if let Some(pending) = ui.pending.take()
                        && let Some(provider) = pending.provider
                    {
                        provider.cancel();
                    }
                    ui.source.orderOut(None);
                }
            });
        });
        Err(ClipboardError::Platform("drag did not start".into()))
    }
}

impl DndBackend for MacDnd {
    fn observe_native_timing(&self, operation: DragOperation, wait: bool, timing: crate::dnd::NativeBeginTiming) {
        self.shared.subscribe_native_timing(operation, wait, timing);
    }
    fn observe_native_begin(&self, operation: DragOperation, wait: bool, callback: NativeBeginCallback) {
        self.shared.subscribe_native_begin(operation, wait, callback);
    }

    fn uses_file_provider(&self) -> bool {
        true
    }

    fn set_source_press(&self, press: SourcePress) {
        let _ = self.shared.source_press.set(press);
    }
    fn set_source_start(&self, start: SourceStart) {
        let _ = self.shared.source_start.set(start);
    }
    fn set_source_image_nudge(&self, nudge: SourceImageNudge) {
        let _ = self.shared.image_nudge.set(nudge);
    }
    fn source_hold_ready(&self, operation: DragOperation) {
        let pending = {
            let mut st = self.shared.lock();
            let Some(hold) = st.image_hold.as_mut().filter(|h| h.active && h.operation == operation) else {
                return;
            };
            hold.ready = true;
            (hold.generation, std::mem::take(&mut hold.pending))
        };
        for reason in pending.1 {
            let shared = self.shared.clone();
            // Pending means its 100ms deadline already fired before the input
            // anchor was armed. Do not start a second delay or post inline.
            queue_main(move |_| shared.post_image_nudge(operation, pending.0, reason));
        }
    }
    fn retire_source_image_nudges(&self, operation: DragOperation) {
        let mut st = self.shared.lock();
        if st.held_source == Some(operation) {
            st.retire_image_nudges();
        }
    }

    fn start(&self, sink: DndSink, inject: Injector) -> Result<(), ClipboardError> {
        let _ = self.shared.sink.set(sink);
        let _ = self.shared.inject.set(inject);
        let shared = self.shared.clone();
        let ok = on_main_wait(Duration::from_secs(3), move |mtm| {
            if !NSApplication::sharedApplication(mtm).isRunning() {
                return false;
            }
            UI.with(|u| {
                if u.borrow().is_some() {
                    return;
                }
                let catcher_view = DndView::new(mtm, Role::Catcher, shared.clone());
                catcher_view.registerForDraggedTypes(&dragged_types(Role::Catcher));
                let catcher = make_panel(mtm, &catcher_view);
                let source_view = DndView::new(mtm, Role::Source, shared.clone());
                // 发起窗口拒绝一切拖入，注册类型只是为了知道拖拽会话什么时候认它为目标（见 `cancel`）。
                source_view.registerForDraggedTypes(&dragged_types(Role::Source));
                let source = make_panel(mtm, &source_view);
                *u.borrow_mut() = Some(Ui {
                    receivers: Default::default(),
                    source_info: None,
                    target_session: None,
                    hold_visual: None,
                    catcher,
                    source,
                    source_view,
                    pending: None,
                    operations: Default::default(),
                    current: None,
                    current_proxies: Vec::new(),
                });
            });
            true
        });
        match ok {
            Some(true) => Ok(()),
            Some(false) => Err(ClipboardError::Platform(
                "NSApplication is not running (drag and drop needs the app, not the command-line agent)".into(),
            )),
            None => Err(ClipboardError::Platform(
                "the main thread is not serving the main queue".into(),
            )),
        }
    }

    fn probe(&self, at: Point) {
        {
            let mut st = self.shared.lock();
            if st.held_source.is_some() {
                return;
            }
            if !st.probing {
                st.probe_operation = None;
            }
            st.probing = true;
        }
        let shared = self.shared.clone();
        on_main(move |mtm| {
            let st = shared.lock();
            if !st.probing || st.held_source.is_some() {
                return;
            }
            drop(st);
            let p = to_cocoa(mtm, at);
            UI.with(|u| {
                if let Some(ui) = u.borrow().as_ref() {
                    show_at(&ui.catcher, p, Role::Catcher);
                }
            });
        });
    }

    fn unprobe(&self) {
        let unused = {
            let mut st = self.shared.lock();
            st.probing = false;
            if st.held_source.is_none()
                && let Some(budget) = st.thumbnail_budget.take()
            {
                budget.cancel();
            }
            st.probe_operation = None;
            st.reported_seq = None;
            if st.held_source.is_none()
                && st
                    .export
                    .as_ref()
                    .is_some_and(|(_, export)| !export.started.load(Ordering::Acquire))
            {
                st.export.take()
            } else {
                None
            }
        };
        if let Some((operation, export)) = unused {
            export.cancel(false);
            on_main(move |_| {
                UI.with(|ui| {
                    if let Some(ui) = ui.borrow_mut().as_mut() {
                        ui.receivers.remove(&operation.sequence);
                    }
                })
            });
        }
        on_main(|_| {
            UI.with(|u| {
                if let Some(ui) = u.borrow().as_ref() {
                    ui.catcher.orderOut(None);
                }
            });
        });
    }

    fn unprobe_source(&self, operation: DragOperation) {
        let shared = self.shared.clone();
        on_main(move |_| {
            let unused = {
                let mut st = shared.lock();
                if st.probe_operation != Some(operation) {
                    return;
                }
                st.probing = false;
                if st.held_source.is_none()
                    && let Some(budget) = st.thumbnail_budget.take()
                {
                    budget.cancel();
                }
                st.probe_operation = None;
                st.reported_seq = None;
                if st.held_source.is_none()
                    && st
                        .export
                        .as_ref()
                        .is_some_and(|(_, export)| !export.started.load(Ordering::Acquire))
                {
                    st.export.take()
                } else {
                    None
                }
            };
            if let Some((_, export)) = &unused {
                export.cancel(false);
            }
            // Re-entry after Resume gets a new capture of the same AppKit drag.
            // Check its operation and hide on the same main-queue turn; merely
            // checking on the action thread could still hide a newer catcher.
            UI.with(|u| {
                if let Some(ui) = u.borrow_mut().as_mut() {
                    if let Some((operation, _)) = unused {
                        ui.receivers.remove(&operation.sequence);
                    }
                    ui.catcher.orderOut(None);
                }
            });
        });
    }

    fn set_content_store(&self, root: std::path::PathBuf) {
        super::file_provider::initialize(root.clone());
        self.shared.lock().content_root = Some(root);
    }
    fn source_content(&self, operation: DragOperation) -> Option<Arc<dyn crate::content::Provider>> {
        let st = self.shared.lock();
        if let Some((_, provider)) = st.exported_data.as_ref().filter(|(op, _)| *op == operation) {
            return Some(provider.clone());
        }
        st.source_data
            .as_ref()
            .filter(|(op, _)| *op == operation)
            .map(|(_, source)| source.clone() as Arc<dyn crate::content::Provider>)
    }
    fn source_image(&self) -> Option<oink_foundation::dnd::DragImage> {
        let st = self.shared.lock();
        st.source_thumbnail
            .as_ref()
            .filter(|(op, _)| Some(*op) == st.source_operation)
            .map(|(_, image)| image.clone())
    }
    fn forget_content(&self, operation: DragOperation) {
        let mut st = self.shared.lock();
        if st.source_data.as_ref().is_some_and(|(op, _)| *op == operation) {
            // A new catcher operation can already have installed its artwork
            // job while the previous content owner is being retired.
            if st.source_operation == Some(operation) {
                if let Some(budget) = st.thumbnail_budget.take() {
                    budget.cancel();
                }
                st.source_thumbnail = None;
            }
            st.source_data = None;
        }
        if st.exported_data.as_ref().is_some_and(|(op, _)| *op == operation) {
            st.exported_data = None;
        }
        let export = if st.export.as_ref().is_some_and(|(op, _)| *op == operation) {
            st.export.take()
        } else {
            None
        };
        drop(st);
        if let Some((_, export)) = export {
            export.cancel(false);
        }
    }
    fn source_requires_export(&self, operation: DragOperation, windows_target: bool) -> bool {
        // Mac targets retain the original source session and publish a promise
        // proxy. Only its real drop requests receipt. Windows needs filenames
        // for FileGroupDescriptor, so only that target calls in promises here.
        windows_target
            && self
                .shared
                .lock()
                .export
                .as_ref()
                .is_some_and(|(op, export)| *op == operation && export.requires_export())
    }
    fn export_source(
        &self,
        operation: DragOperation,
        crossing: Instant,
        release: &dyn Fn() -> Result<(), String>,
    ) -> bool {
        {
            let mut st = self.shared.lock();
            if st.held_source != Some(operation) || !st.export.as_ref().is_some_and(|(op, _)| *op == operation) {
                return false;
            }
            if !st.export.as_ref().unwrap().1.at_boundary(crossing) {
                return false;
            }
            st.releasing = true;
            st.released = false;
            st.retire_image_nudges();
        }
        release().is_ok()
    }
    fn hold_source(&self, operation: DragOperation) -> Result<oink_foundation::dnd::DragAnchor, ClipboardError> {
        let shared = self.shared.clone();
        let hidden = on_main_wait(WAIT, move |_| {
            {
                let st = shared.lock();
                if !st.probing
                    || !st.source_present
                    || st.held_source.is_some()
                    || st.source_operation != Some(operation)
                {
                    return false;
                }
            }
            let sender = UI.with(|u| u.borrow().as_ref().and_then(|ui| ui.source_info.clone()));
            let Some(sender) = sender else {
                return false;
            };
            let started = Instant::now();
            let (initial_items, changed) = hide_drag_images(&sender);
            let enumeration_returned = Instant::now();
            if initial_items == 0 {
                tracing::warn!(
                    sequence = operation.sequence,
                    "held source has no enumerable artwork items"
                );
                return false;
            }
            UI.with(|u| {
                let mut ui = u.borrow_mut();
                let Some(ui) = ui.as_mut() else {
                    return false;
                };
                let mut st = shared.lock();
                st.next_image_hold += 1;
                let generation = st.next_image_hold;
                ui.hold_visual = Some(HoldVisual::new(
                    operation,
                    &sender,
                    &ui.catcher,
                    initial_items,
                    started,
                    generation,
                ));
                st.held_source = Some(operation);
                st.image_hold = Some(ImageHold {
                    operation,
                    generation,
                    native_sequence: sender.draggingSequenceNumber() as i64,
                    button: st.source_button,
                    started,
                    active: true,
                    ready: false,
                    pending: Vec::new(),
                    posts: Vec::new(),
                });
                queue_image_nudge(
                    shared.clone(),
                    operation,
                    generation,
                    ImageNudgeReason::InitialHide,
                    changed,
                    enumeration_returned,
                );
                true
            })
        })
        .unwrap_or(false);
        if !hidden {
            return Err(ClipboardError::Unsupported);
        }
        let at = cursor_cg().ok_or(ClipboardError::Unsupported)?;
        let st = self.shared.lock();
        if !st.probing
            || !st.source_present
            || st.held_source != Some(operation)
            || st.source_operation != Some(operation)
        {
            return Err(ClipboardError::Unsupported);
        }
        Ok(oink_foundation::dnd::DragAnchor {
            generation: operation.sequence,
            at,
            surface: 0,
        })
    }

    fn resume_source(&self, operation: DragOperation) -> bool {
        {
            let mut st = self.shared.lock();
            if st.held_source != Some(operation) || !st.source_present {
                return false;
            }
            st.held_source = None;
            st.retire_image_nudges();
        }
        let shared = self.shared.clone();
        on_main(move |_| {
            UI.with(|u| {
                if let Some(ui) = u.borrow_mut().as_mut()
                    && let Some(mut visual) = ui.hold_visual.take()
                {
                    visual.summary.finish = destination_snapshot(&ui.catcher, ui.source_info.as_deref());
                    visual.report(&shared, NativeHoldEnd::Resume);
                }
            })
        });
        self.unprobe();
        true
    }

    fn observe_source_cancel_geometry(&self, operation: DragOperation, geometry: CancelGeometry) {
        let mut st = self.shared.lock();
        if st.held_source == Some(operation) {
            st.cancel_observation = Some((
                operation,
                CancelObservation {
                    geometry,
                    ..Default::default()
                },
            ));
        }
    }

    fn observe_source_cancel_posted(&self, operation: DragOperation, at: Point) {
        let mut st = self.shared.lock();
        if let Some((_, observation)) = st.cancel_observation.as_mut().filter(|(op, _)| *op == operation) {
            observation.posted = Some(at);
        }
    }

    fn prepare_source_cancel(
        &self,
        operation: DragOperation,
        at: Point,
        move_source: &dyn Fn(Point) -> Result<(), String>,
    ) -> bool {
        let started = Instant::now();
        {
            let mut st = self.shared.lock();
            if st.held_source != Some(operation)
                || !st.source_present
                || st
                    .export
                    .as_ref()
                    .is_some_and(|(_, export)| export.started.load(Ordering::Acquire))
            {
                return false;
            }
            st.cancel_move = Some(CancelMove {
                operation,
                at,
                acknowledged: false,
                exited: false,
                cancel_submitted: false,
                started,
            });
            st.retire_image_nudges();
        }
        // Never run this on AppKit's thread: the native callback must be able to
        // acknowledge the tagged event. No polling, path, extra warp or retry.
        let delivered = move_source(at).is_ok();
        if let Some((_, observation)) = self
            .shared
            .lock()
            .cancel_observation
            .as_mut()
            .filter(|(op, _)| *op == operation)
        {
            observation.delivery_ack = Some(delivered);
        }
        tracing::info!(dnd_cancel_delivery = delivered, "held source dragged delivery receipt");
        self.shared.wait(WAIT.saturating_sub(started.elapsed()), |st| {
            st.held_source != Some(operation) || st.cancel_move.as_ref().is_none_or(|movement| movement.acknowledged)
        });
        let mut st = self.shared.lock();
        if let Some((_, observation)) = st.cancel_observation.as_mut().filter(|(op, _)| *op == operation) {
            observation.wait_us = Some(started.elapsed().as_micros() as u64);
        }
        st.cancel_move
            .as_ref()
            .is_some_and(|movement| movement.operation == operation && movement.acknowledged)
    }

    fn source_completion_is_async(&self) -> bool {
        true
    }
    fn finish_source(&self, operation: DragOperation, accept: bool, release: &dyn Fn() -> Result<(), String>) -> bool {
        let export = self
            .shared
            .lock()
            .export
            .as_ref()
            .filter(|(op, _)| *op == operation)
            .map(|(_, export)| export.clone());
        if let Some(export) = export {
            let started = export.started.load(Ordering::Acquire);
            if !accept {
                export.cancel(started);
            }
            if started {
                return true;
            }
        }
        {
            let mut st = self.shared.lock();
            if st.held_source != Some(operation) {
                return false;
            }
            st.releasing = accept;
            st.released = false;
            st.retire_image_nudges();
        }
        if !accept {
            // Third-party drag sources do not observe Oink's own cancel flag.
            // Tagged Escape reaches their native modal loop on both platforms.
            for pressed in [true, false] {
                if !self.shared.inject(InjectEvent::Key {
                    code: oink_foundation::input::KeyCode::ESCAPE,
                    pressed,
                }) {
                    return false;
                }
            }
        }
        // Drop can synchronously snapshot a large provider. Do not block the
        // input action queue or invent completion from a short acknowledgement.
        let released = release().is_ok();
        if !accept && released {
            let cleanup = {
                let mut st = self.shared.lock();
                st.cancel_move
                    .as_mut()
                    .filter(|movement| movement.operation == operation)
                    .is_some_and(|movement| {
                        movement.cancel_submitted = true;
                        movement.cleanup_ready()
                    })
            };
            if cleanup {
                let shared = self.shared.clone();
                on_main(move |_| {
                    // Moving out of a destination is not native completion. Only
                    // retire its deferred exit after the explicit cancel/release.
                    if shared.lock().held_source == Some(operation) {
                        end_held_source(&shared, false, NativeHoldEnd::CancelRepositioned);
                    }
                });
            }
        }
        released
    }

    fn release(&self) -> bool {
        self.release_using(&|| {
            if self.shared.inject(InjectEvent::Button {
                button: MouseButton::Left,
                pressed: false,
            }) {
                Ok(())
            } else {
                Err("button injection failed".into())
            }
        })
    }

    fn release_physical(&self, release: &dyn Fn() -> Result<(), String>) -> bool {
        self.release_using(release)
    }

    fn set_target_image(&self, image: Option<oink_foundation::dnd::DragImage>) {
        let mut st = self.shared.lock();
        st.target_image = image.filter(|i| i.valid());
        st.target_preview = None;
    }
    fn update_target_image(&self, operation: DragOperation, image: oink_foundation::dnd::DragImage) {
        if !image.valid() {
            return;
        }
        let image = crate::dnd::preview::fitted_image(&image);
        let Some(decoded) = super::thumbnail::decode_target(&image.png) else {
            return;
        };
        on_main(move |_| {
            use objc2::ClassType;
            UI.with(|ui| {
                let ui = ui.borrow();
                let Some((_, session)) = ui
                    .as_ref()
                    .and_then(|ui| ui.target_session.as_ref())
                    .filter(|(op, _)| *op == operation)
                else {
                    return;
                };
                let bitmap = decoded.appkit(NSSize::new(image.width, image.height));
                let classes = NSArray::from_slice(&[NSPasteboardItem::class()]);
                let options = objc2_foundation::NSDictionary::new();
                let update = block2::RcBlock::new(
                    move |item: std::ptr::NonNull<NSDraggingItem>,
                          index: isize,
                          _: std::ptr::NonNull<objc2::runtime::Bool>| {
                        if index == 0 {
                            // SAFETY: the enumerated item is valid only during this callback.
                            apply_thumbnail(unsafe { item.as_ref() }, &image, &bitmap);
                        }
                    },
                );
                // AppKit supports changing session images by enumeration. The
                // destination may override them; no new drag or input is sent.
                // SAFETY: synchronous enumeration on main; all arguments stay retained.
                unsafe {
                    session.enumerateDraggingItemsWithOptions_forView_classes_searchOptions_usingBlock(
                        objc2_app_kit::NSDraggingItemEnumerationOptions::empty(),
                        None,
                        &classes,
                        &options,
                        &update,
                    );
                }
            });
        });
    }
    fn prepare_preview(&self, spec: &DragSpec) -> bool {
        let Some(native) = spec
            .native
            .as_ref()
            .filter(|native| native.inventory.roots.is_empty() && native.files.is_empty())
        else {
            return false;
        };
        if self.shared.lock().target_image.is_some() {
            return false;
        }
        let preview = crate::dnd::preview::content(
            native,
            oink_foundation::clipboard::ClipPlatform::Mac,
            &super::content::MacTranslator,
        );
        self.shared.lock().target_preview = Some((spec.operation, preview));
        true
    }
    fn begin(&self, spec: DragSpec) -> Result<(), ClipboardError> {
        self.begin_prepared(spec, None)
    }

    fn begin_file_provider(&self, spec: DragSpec, registration: Registration) -> Result<(), ClipboardError> {
        let cleanup = registration.clone();
        let result = self.begin_prepared(spec, Some(registration));
        if result.is_err() {
            cleanup.cancel();
        }
        result
    }

    fn cancel(&self) -> bool {
        self.cancel_using(&|| {
            if self.shared.inject(InjectEvent::Button {
                button: MouseButton::Left,
                pressed: false,
            }) {
                Ok(())
            } else {
                Err("button injection failed".into())
            }
        })
    }

    fn cancel_physical(&self, release: &dyn Fn() -> Result<(), String>) -> bool {
        self.cancel_using(release)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn reentering_a_native_drag_after_catcher_exit_starts_a_new_probe_operation() {
        let operation = DragOperation::new();
        let mut st = St {
            reported_seq: Some(42),
            source_operation: Some(operation),
            ..Default::default()
        };
        assert!(st.probe_current(42));
        // draggingExited retires source_operation but the native sequence stays
        // the same when the user pushes the same held drag against the edge.
        st.source_operation = None;
        assert!(
            !st.probe_current(42),
            "the retired operation cannot suppress a new Detected callback"
        );
    }

    #[test]
    fn edge_catcher_covers_the_menu_bar_and_tangent_while_detection_is_pending() {
        let screen = NSRect::new(NSPoint::ZERO, NSSize::new(1728.0, 1117.0));
        for x in [0.0, 600.0, 864.0, 1727.0] {
            let frame = catcher_frame(NSPoint::new(x, 1117.0), &[screen]);
            assert_eq!(frame, NSRect::new(NSPoint::new(0.0, 1069.0), NSSize::new(1728.0, 48.0)));
            assert_eq!(catcher_frame(NSPoint::new(x + 1.0, 1117.0), &[screen]), frame);
        }
    }

    #[test]
    fn edge_catcher_uses_full_native_screen_including_negative_origin_and_other_edges() {
        let screen = NSRect::new(NSPoint::new(-1200.0, -300.0), NSSize::new(1200.0, 900.0));
        for (at, expected) in [
            (
                NSPoint::new(-1200.0, 100.0),
                NSRect::new(NSPoint::new(-1200.0, -300.0), NSSize::new(48.0, 900.0)),
            ),
            (
                NSPoint::new(-1.0, 100.0),
                NSRect::new(NSPoint::new(-48.0, -300.0), NSSize::new(48.0, 900.0)),
            ),
            (
                NSPoint::new(-600.0, -299.0),
                NSRect::new(NSPoint::new(-1200.0, -300.0), NSSize::new(1200.0, 48.0)),
            ),
        ] {
            assert_eq!(catcher_frame(at, &[screen]), expected);
        }
        let middle = NSPoint::new(-600.0, 100.0);
        assert_eq!(catcher_frame(middle, &[screen]), catcher_frame(middle, &[]));
    }

    #[test]
    fn dragging_view_registers_source_and_legacy_promise_methods() {
        use objc2::{ClassType, sel};

        let class = DndView::class();
        for selector in [
            sel!(namesOfPromisedFilesDroppedAtDestination:),
            sel!(draggingSession:sourceOperationMaskForDraggingContext:),
            sel!(draggingSession:willBeginAtPoint:),
            sel!(draggingSession:endedAtPoint:operation:),
        ] {
            assert!(class.instance_method(selector).is_some(), "{selector}");
        }
    }

    #[test]
    fn image_nudge_requires_an_actual_image_change() {
        objc2::rc::autoreleasepool(|_| {
            let items = dragging_items(&remote_spec(), None, None, &[]);
            let item = &items[0];
            assert!(hide_drag_item(item));
            assert!(
                !hide_drag_item(item),
                "already hidden: no setter and no nudge, or callbacks can loop"
            );
        });
    }

    fn fake_image_hold(backend: &MacDnd, operation: DragOperation, generation: u64, ready: bool) {
        let mut st = backend.shared.lock();
        st.source_present = true;
        st.held_source = Some(operation);
        st.image_hold = Some(ImageHold {
            operation,
            generation,
            native_sequence: 7,
            button: Some(MouseButton::Right),
            started: Instant::now(),
            active: true,
            ready,
            pending: Vec::new(),
            posts: Vec::new(),
        });
    }

    #[test]
    fn image_nudge_scheduler_waits_100ms_per_changed_batch_and_revokes_stale_hold() {
        // Fake clock/queue and backend: no main run loop or desktop input.
        let backend = MacDnd::new();
        let operation = DragOperation::new();
        fake_image_hold(&backend, operation, 1, true);
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        backend.set_source_image_nudge(Arc::new(move |_, button, posted| {
            observed.fetch_add(1, Ordering::Relaxed);
            posted(oink_foundation::dnd::ImageNudgePost {
                at: Point::new(12., 34.),
                button,
                flags: 256,
            });
            Ok(())
        }));
        let returned = Instant::now();
        let job = |changed, returned| {
            image_nudge_job(
                backend.shared.clone(),
                operation,
                1,
                ImageNudgeReason::InitialHide,
                changed,
                returned,
            )
        };
        assert!(job(0, returned).is_none(), "no actual change, no timer or post");
        let mut queue = vec![job(2, returned).unwrap()];
        assert_eq!(queue[0].0, returned + Duration::from_millis(100));
        for now in [
            returned,
            returned + Duration::from_millis(99),
            returned + Duration::from_millis(100),
        ] {
            let expected = usize::from(now == returned + Duration::from_millis(100));
            if queue.first().is_some_and(|(deadline, _)| *deadline <= now) {
                let (_, post) = queue.remove(0);
                post();
            }
            assert_eq!(count.load(Ordering::Relaxed), expected);
        }
        assert!(queue.is_empty(), "one batch, one post, no retry");
        let st = backend.shared.lock();
        assert_eq!(st.image_hold.as_ref().unwrap().posts.len(), 1);
        assert_eq!(st.image_hold.as_ref().unwrap().posts[0].post.at, Point::new(12., 34.));
        assert_eq!(st.image_hold.as_ref().unwrap().posts[0].post.button, MouseButton::Right);
        drop(st);
        let later_return = returned + Duration::from_millis(120);
        let (deadline, stale) = job(1, later_return).unwrap();
        assert_eq!(
            deadline,
            later_return + Duration::from_millis(100),
            "each real batch has its own deadline"
        );
        fake_image_hold(&backend, operation, 2, true);
        stale();
        assert_eq!(count.load(Ordering::Relaxed), 1, "replacement retired the old timer");
    }

    #[test]
    fn image_change_batch_posts_once_and_unchanged_callback_posts_nothing() {
        objc2::rc::autoreleasepool(|_| {
            let backend = MacDnd::new();
            let operation = DragOperation::new();
            fake_image_hold(&backend, operation, 1, true);
            let count = Arc::new(AtomicUsize::new(0));
            let observed = count.clone();
            backend.set_source_image_nudge(Arc::new(move |_, button, posted| {
                observed.fetch_add(1, Ordering::Relaxed);
                posted(oink_foundation::dnd::ImageNudgePost {
                    at: Point::new(12., 34.),
                    button,
                    flags: 256,
                });
                Ok(())
            }));
            let mut spec = remote_spec();
            let ClipData::Files(entries) = &mut spec.items[0] else {
                panic!()
            };
            entries.push(FileEntry {
                path: "second.txt".into(),
                ..entries[0].clone()
            });
            let items = dragging_items(&spec, None, None, &[]);
            assert_eq!(items.len(), 2);
            for (reason, expected_changes) in [(ImageNudgeReason::InitialHide, 2), (ImageNudgeReason::ImageUpdate, 0)] {
                let changed = items.iter().filter(|item| hide_drag_item(item)).count();
                assert_eq!(changed, expected_changes);
                if let Some((_, job)) =
                    image_nudge_job(backend.shared.clone(), operation, 1, reason, changed, Instant::now())
                {
                    job();
                }
                assert_eq!(
                    count.load(Ordering::Relaxed),
                    1,
                    "one per changed batch, none per no-change callback"
                );
            }
        });
    }

    #[test]
    fn image_nudge_batches_wait_for_anchor_and_have_no_per_hold_budget() {
        use oink_foundation::dnd::ImageNudgePost;
        let backend = MacDnd::new();
        let operation = DragOperation::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        backend.set_source_image_nudge(Arc::new(move |op, button, posted| {
            observed.lock().unwrap().push((op, button));
            posted(ImageNudgePost {
                at: Point::new(12., 34.),
                button,
                flags: 256,
            });
            // OS accepted; a lost tap ACK must not lose the post count or retry.
            Err("fake lost ACK".into())
        }));
        fake_image_hold(&backend, operation, 1, false);
        backend
            .shared
            .post_image_nudge(operation, 1, ImageNudgeReason::InitialHide);
        assert!(seen.lock().unwrap().is_empty());
        {
            let mut st = backend.shared.lock();
            let hold = st.image_hold.as_mut().unwrap();
            assert_eq!(hold.pending, [ImageNudgeReason::InitialHide]);
            hold.ready = true;
            hold.pending.clear();
        }
        backend
            .shared
            .post_image_nudge(operation, 1, ImageNudgeReason::InitialHide);
        for _ in 0..4 {
            backend
                .shared
                .lock()
                .image_hold
                .as_mut()
                .unwrap()
                .observe_image_update();
            backend
                .shared
                .post_image_nudge(operation, 1, ImageNudgeReason::ImageUpdate);
        }
        assert_eq!(*seen.lock().unwrap(), vec![(operation, MouseButton::Right); 5]);
        let st = backend.shared.lock();
        let posts = &st.image_hold.as_ref().unwrap().posts;
        assert_eq!(posts.len(), 5);
        assert!(posts[..4].iter().all(|p| p.image_update_us.is_some()));
        assert_eq!(posts[4].image_update_us, None);
    }

    #[test]
    fn stale_image_nudges_cannot_post_after_return_rehold_cancel_or_release() {
        let backend = MacDnd::new();
        let operation = DragOperation::new();
        backend.set_source_image_nudge(Arc::new(|_, _, _| panic!("stale nudge submitted")));
        fake_image_hold(&backend, operation, 2, true);
        backend
            .shared
            .post_image_nudge(operation, 1, ImageNudgeReason::InitialHide);
        backend
            .shared
            .post_image_nudge(DragOperation::new(), 2, ImageNudgeReason::ImageUpdate);
        backend.shared.lock().retire_image_nudges();
        backend
            .shared
            .post_image_nudge(operation, 2, ImageNudgeReason::ImageUpdate);
        fake_image_hold(&backend, operation, 3, true);
        backend.retire_source_image_nudges(operation);
        backend
            .shared
            .post_image_nudge(operation, 3, ImageNudgeReason::InitialHide);
        fake_image_hold(&backend, operation, 4, true);
        assert!(backend.resume_source(operation));
        backend
            .shared
            .post_image_nudge(operation, 4, ImageNudgeReason::ImageUpdate);
        fake_image_hold(&backend, operation, 5, true);
        assert!(!backend.prepare_source_cancel(operation, Point::new(0., 0.), &|_| {
            backend
                .shared
                .post_image_nudge(operation, 5, ImageNudgeReason::ImageUpdate);
            Err("fake relocation failed".into())
        }));
    }

    #[test]
    fn cancel_diagnostics_distinguish_post_receipt_callback_and_ack() {
        let backend = MacDnd::new();
        let operation = DragOperation::new();
        {
            let mut st = backend.shared.lock();
            st.held_source = Some(operation);
            st.source_present = true;
        }
        let geometry = CancelGeometry {
            down: Some(Point::new(50., 50.)),
            target_plane: Some(Point::new(25., -20.)),
            projected: Some(Point::new(75., -50.)),
            edge: Some(Point::new(62.5, 0.)),
            fallback: None,
        };
        backend.observe_source_cancel_geometry(operation, geometry);
        let posted = Point::new(62.499996, 0.);
        assert!(
            backend.prepare_source_cancel(operation, geometry.requested().unwrap(), &|_| {
                backend.observe_source_cancel_posted(DragOperation::new(), Point::new(999., 999.));
                assert!(backend.shared.lock().cancel_observation.unwrap().1.posted.is_none());
                backend.observe_source_cancel_posted(operation, posted);
                let mut st = backend.shared.lock();
                st.observe_cancel_position(Point::new(0., 50.), false);
                assert!(st.cancel_observation.unwrap().1.ack_latency_us.is_none());
                st.observe_cancel_position(posted, true);
                let ack = st.cancel_observation.unwrap().1.ack_latency_us;
                st.observe_cancel_position(Point::new(64., 0.), false);
                assert_eq!(st.cancel_observation.unwrap().1.ack_latency_us, ack);
                Ok(())
            })
        );
        let observation = backend.shared.lock().cancel_observation.unwrap().1;
        assert_eq!(observation.geometry, geometry);
        assert_eq!(observation.posted, Some(posted));
        assert_eq!(observation.delivery_ack, Some(true));
        assert_eq!(observation.callback_count, 3);
        assert_eq!(observation.ack_position, Some(posted));
        assert_eq!(observation.last_callback, Some(Point::new(64., 0.)));
        assert!(observation.ack_latency_us.unwrap() <= observation.wait_us.unwrap());
    }

    #[test]
    fn cancel_delivery_without_a_session_callback_stays_unconfirmed_at_any_point() {
        // State/diagnostic contract only, not a claim about when AppKit calls
        // back. A zero-distance post and a displaced post both need observation.
        for at in [Point::new(0., 50.), Point::new(75., 0.)] {
            let backend = MacDnd::new();
            let operation = DragOperation::new();
            {
                let mut st = backend.shared.lock();
                st.held_source = Some(operation);
                st.source_present = true;
            }
            backend.observe_source_cancel_geometry(
                operation,
                CancelGeometry {
                    edge: Some(at),
                    ..Default::default()
                },
            );
            assert!(!backend.prepare_source_cancel(operation, at, &|point| {
                backend.observe_source_cancel_posted(operation, point);
                Ok(())
            }));
            let observation = backend.shared.lock().cancel_observation.unwrap().1;
            assert_eq!(observation.posted, Some(at));
            assert_eq!(observation.delivery_ack, Some(true));
            assert_eq!(observation.callback_count, 0);
            assert_eq!(observation.ack_latency_us, None);
            assert_eq!(observation.ack_position, None);
            assert!(observation.wait_us.is_some());
        }
    }

    #[test]
    fn expected_cancel_exit_is_not_reported_as_unexpected_catcher_loss() {
        // This happens for both the old frozen point and a real displacement:
        // classification must follow explicit cancel/release, not coordinates.
        for at in [Point::new(0., 50.), Point::new(75., 0.)] {
            let backend = MacDnd::new();
            let operation = DragOperation::new();
            let (tx, rx) = mpsc::channel();
            assert!(
                backend
                    .shared
                    .sink
                    .set(Arc::new(move |event| {
                        tx.send(event).unwrap();
                    }))
                    .is_ok()
            );
            let mut movement = CancelMove {
                operation,
                at,
                acknowledged: false,
                exited: false,
                cancel_submitted: false,
                started: Instant::now(),
            };
            movement.observe(at, true);
            assert!(!movement.cleanup_ready());
            movement.cancel_submitted = true;
            assert!(movement.cleanup_ready());
            backend.shared.lock().held_source = Some(operation);
            end_held_source(&backend.shared, false, NativeHoldEnd::CancelRepositioned);
            let event = rx.try_recv().unwrap();
            assert_eq!(event, PlatformDndEvent::SourceCancelRetired { operation });
            assert!(
                !matches!(event, PlatformDndEvent::SourceCatcherLost { .. }),
                "{event:?}"
            );
            assert!(
                !matches!(event, PlatformDndEvent::SourceEnded { .. }),
                "an exit is not native completion"
            );
        }
    }

    #[test]
    fn unrequested_exit_remains_catcher_loss_and_native_end_remains_observed() {
        for ended in [false, true] {
            let backend = MacDnd::new();
            let operation = DragOperation::new();
            let (tx, rx) = mpsc::channel();
            assert!(
                backend
                    .shared
                    .sink
                    .set(Arc::new(move |event| {
                        tx.send(event).unwrap();
                    }))
                    .is_ok()
            );
            backend.shared.lock().held_source = Some(operation);
            end_held_source(
                &backend.shared,
                false,
                if ended {
                    NativeHoldEnd::DraggingEnded
                } else {
                    NativeHoldEnd::DraggingExited
                },
            );
            assert_eq!(
                rx.try_recv().unwrap(),
                if ended {
                    PlatformDndEvent::SourceEnded {
                        operation,
                        accepted: false,
                    }
                } else {
                    PlatformDndEvent::SourceCatcherLost { operation }
                }
            );
        }
    }

    #[test]
    fn cancel_move_acknowledges_only_target_and_defers_exit_until_cancel_submission() {
        let mut movement = CancelMove {
            operation: DragOperation::new(),
            at: Point::new(1., 558.5),
            acknowledged: false,
            exited: false,
            cancel_submitted: false,
            started: Instant::now(),
        };
        movement.observe(Point::new(720., 596.), false);
        assert!(!movement.acknowledged);
        movement.observe(Point::new(1., 558.5), true);
        assert!(movement.acknowledged);
        assert!(
            !movement.cleanup_ready(),
            "destination exit alone must not clear held source before Escape"
        );
        movement.cancel_submitted = true;
        assert!(movement.cleanup_ready());
        movement.exited = false;
        assert!(
            !movement.cleanup_ready(),
            "inside-catcher cancellation still uses the native end callback"
        );
    }

    #[test]
    fn native_begin_numeric_handoff_is_once_only_and_cannot_cross_operations() {
        static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        fn record(g: u64, start: Instant, end: Instant) {
            assert_eq!(g, 7);
            assert!(end >= start);
            CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let backend = MacDnd::new();
        let start = Instant::now();
        let timing = crate::dnd::NativeBeginTiming {
            generation: 7,
            queued_at: start,
            record,
        };
        for completed_first in [false, true] {
            let operation = DragOperation::new();
            backend.shared.reset_native_begin(operation);
            if completed_first {
                backend.shared.complete_native_begin(Some(operation), Some(start));
            }
            backend.observe_native_timing(operation, true, timing);
            if !completed_first {
                backend.shared.complete_native_begin(Some(operation), Some(start));
            }
            backend.shared.complete_native_begin(Some(operation), Some(start));
        }
        assert_eq!(CALLS.load(std::sync::atomic::Ordering::Relaxed), 2);
        let old = DragOperation::new();
        let current = DragOperation::new();
        backend.shared.reset_native_begin(current);
        backend.observe_native_timing(current, true, timing);
        backend.shared.complete_native_begin(Some(old), Some(start));
        backend.shared.complete_native_begin(Some(current), None);
        assert_eq!(CALLS.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn native_begin_observation_handles_callback_before_and_after_begin_return() {
        for callback_first in [false, true] {
            let backend = MacDnd::new();
            let operation = DragOperation::new();
            let at = Instant::now();
            let (tx, rx) = mpsc::channel();
            backend.shared.reset_native_begin(operation);
            if callback_first {
                backend.shared.complete_native_begin(Some(operation), Some(at));
            }
            backend.observe_native_begin(operation, true, Box::new(move |at| tx.send(at).unwrap()));
            if !callback_first {
                assert!(rx.try_recv().is_err());
                backend.shared.complete_native_begin(Some(operation), Some(at));
            }
            assert_eq!(rx.try_recv().unwrap(), Some(at));
            backend.shared.complete_native_begin(Some(operation), None);
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn native_begin_cancel_and_old_callback_cannot_label_the_next_operation() {
        let backend = MacDnd::new();
        let first = DragOperation::new();
        let next = DragOperation::new();
        let (tx, rx) = mpsc::channel();
        backend.shared.reset_native_begin(first);
        backend.observe_native_begin(first, true, Box::new(move |at| tx.send(at).unwrap()));
        backend.shared.complete_native_begin(None, None);
        assert_eq!(rx.try_recv().unwrap(), None);
        backend.shared.reset_native_begin(next);
        let (tx, rx) = mpsc::channel();
        backend.observe_native_begin(next, true, Box::new(move |at| tx.send(at).unwrap()));
        backend.shared.complete_native_begin(Some(first), Some(Instant::now()));
        assert!(rx.try_recv().is_err());
        backend.shared.complete_native_begin(Some(next), None);
        assert_eq!(rx.try_recv().unwrap(), None);
    }

    #[test]
    fn main_queue_timeout_before_execution_disarms_the_late_press() {
        let attempt = PressAttempt::default();
        assert!(!attempt.stop_waiting());
        assert!(!attempt.try_begin(), "a delayed main-queue closure must not post DOWN");
        assert!(!attempt.take_release(), "nothing was submitted");
    }

    #[test]
    fn inflight_timeout_retains_one_cleanup_after_press_completion() {
        let attempt = PressAttempt::default();
        assert!(attempt.try_begin());
        assert!(attempt.stop_waiting(), "an in-flight press must not be replayed");
        assert!(!attempt.try_begin());
        // The press finishes after the caller's deadline. Its queued cleanup
        // executes afterwards, so there cannot be a DOWN after this UP.
        assert!(attempt.finish(PressOutcome::Submitted));
        assert!(attempt.take_release());
        assert!(!attempt.take_release());
        assert!(!attempt.try_begin());
    }

    #[test]
    fn known_rejection_never_requires_a_release_even_after_wait_timeout() {
        for timeout in [false, true] {
            let attempt = PressAttempt::default();
            assert!(attempt.try_begin());
            if timeout {
                assert!(attempt.stop_waiting());
            }
            assert!(!attempt.finish(PressOutcome::NotSubmitted));
            assert!(!attempt.stop_waiting());
            assert!(!attempt.take_release());
        }
        // A new hit-test attempt is allowed after a definitive synchronous
        // rejection; the old attempt cannot be armed a second time.
        assert!(PressAttempt::default().try_begin());
    }

    #[test]
    fn generic_injector_unknown_result_is_not_replayed_and_is_cleaned_up() {
        let attempt = PressAttempt::default();
        assert!(attempt.try_begin());
        assert!(attempt.finish(PressOutcome::Unconfirmed));
        assert!(!attempt.try_begin());
        assert!(attempt.take_release());
        assert!(!attempt.take_release());
    }

    struct CurrentBytes;
    impl FileContents for CurrentBytes {
        fn open(&self, _: usize) -> std::io::Result<crate::FileReader> {
            Ok(Box::new(std::io::Cursor::new(b"current bytes".to_vec())))
        }
        fn metadata(&self, _: usize) -> std::io::Result<Option<crate::FileMetadata>> {
            Ok(Some(crate::FileMetadata {
                size: 13,
                modified_ms: Some(1700000001234),
            }))
        }
    }

    fn remote_spec() -> DragSpec {
        DragSpec {
            native: None,
            operation: crate::dnd::DragOperation::new(),
            at: Point::new(0.0, 0.0),
            items: vec![ClipData::Files(vec![FileEntry {
                path: "中文.txt".into(),
                is_dir: false,
                size: 1,
                modified_ms: None,
                local_path: Some(PathBuf::from("/untrusted/source/path")),
            }])],
            files: Some(Arc::new(CurrentBytes)),
            ops: DragOps::COPY,
        }
    }

    fn native_content_spec(name: &str, standard: oink_foundation::content::StandardFormat, bytes: Vec<u8>) -> DragSpec {
        use crate::model::Inventory;
        use oink_foundation::{clipboard::ClipPlatform, content::*};
        struct Bytes(Inventory);
        impl crate::content::Provider for Bytes {
            fn inventory(&self) -> Inventory {
                self.0.clone()
            }
            fn open(&self, _: RepresentationId) -> std::io::Result<crate::FileReader> {
                panic!("artwork rendering uses prepared preview bytes")
            }
            fn translator(&self) -> Arc<dyn crate::content::Translator> {
                Arc::new(super::super::content::MacTranslator)
            }
        }
        let inventory = Inventory {
            id: OfferId {
                clip: oink_foundation::content::ClipId {
                    origin: oink_foundation::DeviceId([1; 32]),
                    seq: 1,
                },
                generation: 1,
            },
            platform: ClipPlatform::Mac,
            items: vec![ContentItem {
                virtual_files: false,
                id: ItemId(0),
                file: None,
                representations: vec![Representation {
                    id: RepresentationId(0),
                    format: NativeFormat::Mac(name.into()),
                    medium: Medium::Bytes,
                    aspect: 1,
                    index: -1,
                    standard: Some(standard),
                    data: RepresentationData::Inline(bytes),
                }],
            }],
            roots: vec![],
        };
        let mut spec = remote_spec();
        spec.files = None;
        spec.items = crate::content::items(&inventory);
        spec.native = Some(crate::NativeSpec {
            contents: Arc::new(Bytes(inventory.clone())),
            inventory,
            files: vec![],
            file_contents: None,
            sensitivity: crate::Sensitivity::Normal,
        });
        spec
    }

    #[test]
    fn native_text_preview_renders_words_instead_of_a_file_icon() {
        objc2::rc::autoreleasepool(|_| {
            let text = "Native words\nsecond line";
            let spec = native_content_spec(
                "public.utf8-plain-text",
                oink_foundation::content::StandardFormat::Text,
                text.as_bytes().to_vec(),
            );
            let backend = MacDnd::new();
            assert!(backend.prepare_preview(&spec));
            let (_, preview) = backend.shared.lock().target_preview.take().unwrap();
            assert_eq!(preview, [ClipData::Text(text.into())]);
            assert!(spec.items.iter().all(|item| matches!(item, ClipData::Native { .. })));
            let items = dragging_items(&spec, None, None, &preview);
            let components = items[0].imageComponents().unwrap();
            let contents = components.objectAtIndex(0).contents().unwrap();
            let image = contents.downcast_ref::<objc2_app_kit::NSImage>().unwrap();
            let expected = text_drag_image(text, None);
            assert_eq!(
                image.size(),
                expected.size(),
                "text dimensions must follow the words, not a type icon"
            );
            assert_eq!(
                image.TIFFRepresentation().unwrap().to_vec(),
                expected.TIFFRepresentation().unwrap().to_vec()
            );
            assert_eq!(items[0].draggingFrame().size, expected.size());
            let raster =
                objc2_app_kit::NSBitmapImageRep::imageRepWithData(&image.TIFFRepresentation().unwrap()).unwrap();
            let mut transparent = false;
            let mut ink = false;
            for y in 0..raster.pixelsHigh() {
                for x in 0..raster.pixelsWide() {
                    let alpha = raster.colorAtX_y(x, y).unwrap().alphaComponent();
                    transparent |= alpha == 0.;
                    ink |= alpha > 0.;
                }
            }
            assert!(
                transparent && ink,
                "text artwork must contain glyphs on a transparent background"
            );
        });
    }

    #[test]
    fn native_png_and_tiff_preview_show_the_picture() {
        use objc2_app_kit::{NSBitmapImageRep, NSImage};
        use oink_foundation::content::StandardFormat;
        objc2::rc::autoreleasepool(|_| {
            let png = crate::dnd::preview::asymmetric_png();
            let rep = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(&png)).unwrap();
            for (format, bytes) in [
                ("public.png", png.clone()),
                ("public.tiff", rep.TIFFRepresentation().unwrap().to_vec()),
            ] {
                let spec = native_content_spec(format, StandardFormat::Png, bytes);
                let backend = MacDnd::new();
                assert!(backend.prepare_preview(&spec));
                let (_, preview) = backend.shared.lock().target_preview.take().unwrap();
                let items = dragging_items(&spec, None, None, &preview);
                let components = items[0].imageComponents().unwrap();
                let contents = components.objectAtIndex(0).contents().unwrap();
                let image = contents.downcast_ref::<NSImage>().unwrap();
                assert_eq!(image.size(), NSSize::new(3., 2.));
                let output = NSBitmapImageRep::imageRepWithData(&image.TIFFRepresentation().unwrap()).unwrap();
                for y in 0..2 {
                    for x in 0..3 {
                        let got = output.colorAtX_y(x, y).unwrap();
                        let expected = rep.colorAtX_y(x, y).unwrap();
                        for (a, b) in [
                            (got.redComponent(), expected.redComponent()),
                            (got.greenComponent(), expected.greenComponent()),
                            (got.blueComponent(), expected.blueComponent()),
                            (got.alphaComponent(), expected.alphaComponent()),
                        ] {
                            assert!((a - b).abs() < 0.01, "{format}: pixel {x},{y}");
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn forgetting_an_old_source_does_not_cancel_a_new_thumbnail_job() {
        let backend = MacDnd::new();
        let old = DragOperation::new();
        let current = DragOperation::new();
        let budget = super::super::thumbnail::Budget::new(Instant::now());
        let inventory = native_content_spec(
            "public.png",
            oink_foundation::content::StandardFormat::Png,
            crate::dnd::preview::asymmetric_png(),
        )
        .native
        .unwrap()
        .inventory;
        let source = crate::content::Deferred::new(inventory, Arc::new(super::super::content::MacTranslator)).unwrap();
        {
            let mut st = backend.shared.lock();
            st.source_data = Some((old, source));
            st.source_operation = Some(current);
            st.thumbnail_budget = Some(budget.clone());
        }
        backend.forget_content(old);
        assert!(budget.current());
    }

    #[test]
    fn late_thumbnail_updates_the_existing_dragging_item_image_and_frame() {
        objc2::rc::autoreleasepool(|_| {
            let spec = native_content_spec(
                "public.utf8-plain-text",
                oink_foundation::content::StandardFormat::Text,
                b"fixture".to_vec(),
            );
            let items = dragging_items(&spec, None, None, &[]);
            let image = oink_foundation::dnd::DragImage {
                png: crate::dnd::preview::asymmetric_png().into(),
                width: 256.,
                height: 192.,
                hotspot_x: 128.,
                hotspot_y: 96.,
            };
            let image = crate::dnd::preview::fitted_image(&image);
            let decoded = super::super::thumbnail::decode_target(&image.png).unwrap();
            let bitmap = decoded.appkit(NSSize::new(image.width, image.height));
            apply_thumbnail(&items[0], &image, &bitmap);
            assert_eq!(items[0].draggingFrame().size, NSSize::new(128., 96.));
            let components = items[0].imageComponents().unwrap();
            let contents = components.objectAtIndex(0).contents().unwrap();
            let actual = contents.downcast_ref::<objc2_app_kit::NSImage>().unwrap();
            assert_eq!(actual.TIFFRepresentation(), bitmap.TIFFRepresentation());
        });
    }

    #[test]
    fn source_png_is_upright_at_its_point_size_on_a_two_x_target() {
        use objc2_app_kit::{NSBitmapImageRep, NSGraphicsContext, NSImage, NSImageInterpolation};
        objc2::rc::autoreleasepool(|_| {
            let source = oink_foundation::dnd::DragImage {
                png: crate::dnd::preview::asymmetric_png().into(),
                width: 3.,
                height: 2.,
                hotspot_x: 1.,
                hotspot_y: 0.5,
            };
            let spec = native_content_spec(
                "public.utf8-plain-text",
                oink_foundation::content::StandardFormat::Text,
                b"fixture".to_vec(),
            );
            let backend = MacDnd::new();
            backend.set_target_image(Some(source.clone()));
            assert!(
                !backend.prepare_preview(&spec),
                "source artwork needs no payload pre-read"
            );
            let items = dragging_items(&spec, Some(&source), None, &[]);
            let frame = items[0].draggingFrame();
            assert_eq!(frame.size, NSSize::new(3., 2.));
            assert_eq!(frame.origin, NSPoint::new(SIZE / 2. - 1., SIZE / 2. - 2. + 0.5));
            let components = items[0].imageComponents().unwrap();
            let contents = components.objectAtIndex(0).contents().unwrap();
            let image = contents.downcast_ref::<NSImage>().unwrap();
            assert_eq!(image.size(), frame.size);
            // Explicit 2x backing raster, independent of the machine's display.
            // No visible window or desktop input is used by this test.
            // SAFETY: null planes ask AppKit to allocate storage; the 6x4 RGBA8
            // layout uses four samples, 32 bits per pixel and a 24-byte row.
            let backing = unsafe {
                NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                    NSBitmapImageRep::alloc(), std::ptr::null_mut(), 6, 4, 8, 4, true, false,
                    objc2_app_kit::NSDeviceRGBColorSpace, 24, 32,
                ).unwrap()
            };
            backing.setSize(frame.size);
            let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&backing).unwrap();
            NSGraphicsContext::saveGraphicsState_class();
            NSGraphicsContext::setCurrentContext(Some(&context));
            context.setImageInterpolation(NSImageInterpolation::None);
            image.drawInRect(NSRect::new(NSPoint::new(0., 0.), frame.size));
            NSGraphicsContext::restoreGraphicsState_class();
            for (x, y, rgb, alpha) in [
                (0, 0, [1., 0., 0.], 128. / 255.),
                (0, 3, [0., 0., 1.], 1.),
                (3, 0, [0., 1., 0.], 1.),
                (3, 3, [1., 1., 0.], 1.),
            ] {
                let color = backing.colorAtX_y(x, y).unwrap();
                for (actual, expected) in [
                    (color.redComponent(), rgb[0]),
                    (color.greenComponent(), rgb[1]),
                    (color.blueComponent(), rgb[2]),
                    (color.alphaComponent(), alpha),
                ] {
                    assert!(
                        (actual - expected).abs() < 0.01,
                        "2x pixel {x},{y}: {actual} != {expected}"
                    );
                }
            }
        });
    }

    #[test]
    fn native_offer_draws_type_icon_without_source_artwork_or_payload_reads() {
        use crate::model::Inventory;
        use oink_foundation::content::*;
        struct MetadataOnly(Inventory);
        impl crate::content::Provider for MetadataOnly {
            fn inventory(&self) -> Inventory {
                self.0.clone()
            }
            fn open(&self, _: RepresentationId) -> std::io::Result<crate::FileReader> {
                panic!("drawing a drag icon must not read payload");
            }
            fn translator(&self) -> Arc<dyn crate::content::Translator> {
                Arc::new(super::super::content::MacTranslator)
            }
        }
        objc2::rc::autoreleasepool(|_| {
            let mut spec = remote_spec();
            let root = FileNode {
                id: NodeId(1),
                parent: None,
                name: "example.txt".into(),
                kind: NodeKind::File,
                size: ContentSize::Unknown,
                version: 1,
                modified_ms: None,
            };
            let inventory = Inventory {
                id: OfferId {
                    clip: oink_foundation::content::ClipId {
                        origin: oink_foundation::DeviceId([1; 32]),
                        seq: 1,
                    },
                    generation: 1,
                },
                platform: oink_foundation::clipboard::ClipPlatform::Mac,
                items: vec![ContentItem {
                    virtual_files: false,
                    id: ItemId(0),
                    file: Some(root.id),
                    representations: vec![Representation {
                        id: RepresentationId(0),
                        format: NativeFormat::Mac("public.file-url".into()),
                        medium: Medium::Bytes,
                        aspect: 1,
                        index: -1,
                        standard: None,
                        data: RepresentationData::File(root.id),
                    }],
                }],
                roots: vec![root],
            };
            spec.items = crate::content::items(&inventory);
            spec.native = Some(crate::NativeSpec {
                contents: Arc::new(MetadataOnly(inventory.clone())),
                inventory,
                files: match &remote_spec().items[0] {
                    ClipData::Files(entries) => entries.clone(),
                    _ => unreachable!(),
                },
                file_contents: spec.files.clone(),
                sensitivity: crate::Sensitivity::Normal,
            });
            for urls in [None, Some(vec![PathBuf::from("/tmp/drag-icon-test.txt")])] {
                assert!(
                    !MacDnd::new().prepare_preview(&spec),
                    "file companions must never be read for preview"
                );
                let items = dragging_items(&spec, None, urls, &[]);
                assert_eq!(items.len(), 1);
                let components = items[0]
                    .imageComponents()
                    .expect("native item needs visible drag artwork");
                assert!(!components.is_empty());
                assert!(components.objectAtIndex(0).contents().is_some());
                let frame = items[0].draggingFrame();
                assert!(frame.size.width > 0. && frame.size.height > 0.);
            }
        });
    }

    #[test]
    fn d4_unrequested_promises_release_content_when_native_writer_is_released() {
        objc2::rc::autoreleasepool(|_| {
            let spec = remote_spec();
            let contents = spec.files.as_ref().unwrap().clone();
            let weak = Arc::downgrade(&contents);
            let items = dragging_items(&spec, None, None, &[]);
            drop(spec);
            drop(contents);
            assert!(weak.upgrade().is_some(), "the native writer still owns its delegate");
            drop(items);
            assert!(
                weak.upgrade().is_none(),
                "an unrequested promise must not be pinned across later drags"
            );
        });
    }

    #[test]
    fn unavailable_provider_uses_real_file_promise_writer() {
        objc2::rc::autoreleasepool(|_| {
            let items = dragging_items(&remote_spec(), None, None, &[]);
            assert_eq!(items.len(), 1);
            assert!(items[0].item().downcast_ref::<NSFilePromiseProvider>().is_some());
        });
    }

    #[test]
    fn catcher_and_cancel_window_recognize_promises_without_calling_them_in() {
        objc2::rc::autoreleasepool(|_| {
            let items = dragging_items(&remote_spec(), None, None, &[]);
            let provider = items[0].item().downcast::<NSFilePromiseProvider>().unwrap();
            let pasteboard = NSPasteboard::pasteboardWithUniqueName();
            let offered = provider.writableTypesForPasteboard(&pasteboard);
            let intersects = |role| {
                let registered = dragged_types(role);
                offered
                    .iter()
                    .any(|ty| registered.iter().any(|r| r.isEqualToString(&ty)))
            };
            assert!(
                intersects(Role::Source),
                "AppKit cannot notify the cancel window for this writer"
            );
            assert!(intersects(Role::Catcher));
            let writers = [ProtocolObject::from_retained(provider)];
            assert!(pasteboard.writeObjects(&NSArray::from_retained_slice(&writers)));

            let receivers = promise::receivers(&pasteboard).unwrap();
            assert_eq!(receivers.len(), 1);
            assert!(
                promise::names(&receivers[0]).is_empty(),
                "probing does not receive the promise"
            );
            pasteboard.clearContents();
        });
    }

    #[test]
    fn available_provider_uses_registered_url_without_promises() {
        objc2::rc::autoreleasepool(|_| {
            let path = PathBuf::from("/tmp/provider-unit-test/中文.txt");
            let items = dragging_items(&remote_spec(), None, Some(vec![path.clone()]), &[]);
            assert_eq!(items.len(), 1);
            let object = items[0].item();
            let url = object.downcast_ref::<NSURL>().unwrap();
            assert_eq!(url.path().unwrap().to_string(), path.to_string_lossy());
        });
    }

    #[test]
    fn file_representations_are_one_item_for_promises_urls_and_provider() {
        objc2::rc::autoreleasepool(|_| {
            let mut spec = remote_spec();
            spec.items.push(ClipData::Text("file:///source/image.jpg".into()));
            spec.items.push(ClipData::Html("<b>image</b>".into()));
            spec.items.push(ClipData::Rtf(b"{\\rtf1 image}".to_vec()));
            for mode in 0..3 {
                let mut spec = DragSpec {
                    native: None,
                    operation: crate::dnd::DragOperation::new(),
                    at: spec.at,
                    items: spec.items.clone(),
                    files: spec.files.clone(),
                    ops: spec.ops,
                };
                if mode == 1 {
                    spec.files = None;
                }
                let urls = (mode == 2).then(|| vec![PathBuf::from("/tmp/provider/image.jpg")]);
                let items = dragging_items(&spec, None, urls, &[]);
                assert_eq!(items.len(), 1, "mode={mode}");
                let writer = items[0].item();
                let ty = NSString::from_str("public.utf8-plain-text");
                // SAFETY: every object created by dragging_items implements NSPasteboardWriting.
                let value: Option<Retained<AnyObject>> =
                    unsafe { msg_send![&*writer, pasteboardPropertyListForType: &*ty] };
                assert!(value.is_some());
                if mode == 0 {
                    assert!(writer.downcast_ref::<NSFilePromiseProvider>().is_some());
                } else {
                    let pi = writer.downcast_ref::<NSPasteboardItem>().unwrap();
                    let url = pi
                        .stringForType(&NSString::from_str("public.file-url"))
                        .unwrap()
                        .to_string();
                    let expected = if mode == 1 {
                        "file:///untrusted/source/path"
                    } else {
                        "file:///tmp/provider/image.jpg"
                    };
                    assert_eq!(url, expected);
                    assert!(pi.dataForType(&NSString::from_str("public.rtf")).is_some());
                }
            }
        });
    }

    #[test]
    fn extra_formats_do_not_add_an_item_to_multiple_files() {
        objc2::rc::autoreleasepool(|_| {
            let mut spec = remote_spec();
            let ClipData::Files(entries) = &mut spec.items[0] else {
                unreachable!()
            };
            let mut second = entries[0].clone();
            second.path = "second.txt".into();
            entries.push(second);
            spec.items.push(ClipData::Text("two files".into()));
            let items = dragging_items(&spec, None, None, &spec.items);
            assert_eq!(items.len(), 2);
        });
    }

    #[test]
    fn promise_delivery_uses_current_version_and_preserves_existing_destination() {
        use std::os::macos::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("中文.txt");
        std::fs::write(&dest, b"existing").unwrap();
        let spec = remote_spec();
        let ClipData::Files(entries) = &spec.items[0] else {
            panic!()
        };
        let path = write_top(entries, 0, &dest, spec.files.as_ref().unwrap()).unwrap();
        assert_ne!(path, dest);
        assert_eq!(std::fs::read(dest).unwrap(), b"existing");
        assert_eq!(std::fs::read(&path).unwrap(), b"current bytes");
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o600, 0o600);
        assert_eq!(metadata.st_flags() & 0x2, 0); // UF_IMMUTABLE (Finder's Locked checkbox).
        assert_eq!(metadata.st_uid(), std::fs::metadata(temp.path()).unwrap().st_uid());
        assert_eq!(
            metadata
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            1700000001234
        );
    }

    #[test]
    fn promise_directory_and_children_are_writable_unlocked_and_deletable() {
        use std::os::macos::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let entries = vec![
            FileEntry {
                path: "folder".into(),
                is_dir: true,
                size: 0,
                modified_ms: None,
                local_path: None,
            },
            FileEntry {
                path: "folder/nested".into(),
                is_dir: true,
                size: 0,
                modified_ms: None,
                local_path: None,
            },
            FileEntry {
                path: "folder/nested/file.txt".into(),
                is_dir: false,
                size: 13,
                modified_ms: None,
                local_path: None,
            },
        ];
        let contents: Arc<dyn FileContents> = Arc::new(CurrentBytes);
        let path = write_top(&entries, 0, &temp.path().join("folder"), &contents).unwrap();
        for (target, owner_mode) in [
            (path.clone(), 0o700),
            (path.join("nested"), 0o700),
            (path.join("nested/file.txt"), 0o600),
        ] {
            let metadata = std::fs::metadata(&target).unwrap();
            assert_eq!(metadata.permissions().mode() & owner_mode, owner_mode);
            assert_eq!(metadata.st_flags() & 0x2, 0); // UF_IMMUTABLE.
            assert_eq!(metadata.st_uid(), std::fs::metadata(temp.path()).unwrap().st_uid());
        }
        let file = path.join("nested/file.txt");
        std::fs::write(&file, b"edited copy").unwrap();
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn native_promise_fallback_keeps_raw_order_and_expands_folder_only_at_write() {
        use crate::content::namespace::{Files, Local};
        use crate::model::NodeId;
        objc2::rc::autoreleasepool(|_| {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source");
            std::fs::create_dir(&source).unwrap();
            let board = NSPasteboard::pasteboardWithUniqueName();
            let item = NSPasteboardItem::new();
            item.setData_forType(&NSData::with_bytes(b"before"), &NSString::from_str("example.before"));
            item.setString_forType(
                &NSString::from_str(&super::super::pasteboard::file_url_string(&source)),
                &NSString::from_str("public.file-url"),
            );
            item.setData_forType(&NSData::with_bytes(b"after"), &NSString::from_str("example.after"));
            assert!(board.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)])));
            let captured = super::super::content::capture(Some(&board.name().to_string()), temp.path()).unwrap();
            let provider = captured.content.unwrap();
            let inventory = provider.inventory();
            let files = Files::new(
                Local::new(vec![(NodeId(1), source.clone())]).unwrap(),
                crate::sync::files::TargetOs::MacOs,
            )
            .unwrap();
            let entries = files.roots().to_vec();
            let mut spec = remote_spec();
            spec.files = Some(files.clone());
            spec.native = Some(crate::NativeSpec {
                inventory,
                contents: provider,
                files: entries.clone(),
                file_contents: Some(files.clone()),
                sensitivity: crate::Sensitivity::Normal,
            });
            let items = dragging_items(&spec, None, None, &spec.items);
            assert_eq!(items.len(), 1);
            let writer = items[0].item();
            let promise = writer.downcast_ref::<DragFilePromise>().unwrap();
            let types = promise.writableTypesForPasteboard(&board);
            let types: Vec<_> = types.iter().map(|ty| ty.to_string()).collect();
            assert_eq!(types.first().unwrap(), "example.before");
            assert_eq!(types.last().unwrap(), "example.after");
            assert!(!types.iter().any(|ty| ty == "public.file-url"));
            // A child created after hover is discovered by the actual write.
            std::fs::write(source.join("late-child"), b"whole folder").unwrap();
            let contents: Arc<dyn FileContents> = files;
            let output = write_top(&entries, 0, &temp.path().join("output"), &contents).unwrap();
            assert_eq!(std::fs::read(output.join("late-child")).unwrap(), b"whole folder");
            board.clearContents();
        });
    }

    #[test]
    fn drag_operation_bits_round_trip() {
        for o in [DragOps::COPY, DragOps::COPY.union(DragOps::MOVE), DragOps::LINK] {
            assert_eq!(drag_ops(ns_ops(o)), o);
        }
        assert_eq!(drag_ops(NSDragOperation::Generic), DragOps::COPY);
    }
}
