//! macOS：每块屏幕一个透明覆盖窗口，内容是一个按格子双线性放大的图层。
//!
//! - 窗口：`NSPanel`（无边框、不激活），透明、无阴影、鼠标穿透（`ignoresMouseEvents`），层级
//!   `NSScreenSaverWindowLevel`，出现在所有桌面空间与全屏应用之上（`CanJoinAllSpaces | FullScreenAuxiliary
//!   | Stationary | IgnoresCycle`）。不抢焦点、不影响光标与点击、能被截图。每块屏幕一个窗口，因为「显示器
//!   具有单独的空间」开着时，跨屏的窗口只显示在一块屏幕上。
//! - 内容：一个子图层的 `contents`
//!   是每格一个像素的 `CGImage`（BGRA 预乘），`magnificationFilter = linear` 平滑放大（格子约 0.2 mm = 1 pt，
//!   Retina 上放大 2 倍，不出现方块）；只有拖尾所在的
//!   那一小块有内容，图层随拖尾移动。
//! - 线程：AppKit / 图层只在主线程上改（经主 dispatch 队列）；绘制线程只做计算。主线程来不及时丢帧
//!   （最多两帧在路上），不排队。
//! - 帧率：按主屏 `maximumFramesPerSecond`（ProMotion 为 120）定时出帧。
//! - 不画时撤掉窗口（`orderOut`），不刷新、不占用。

use std::cell::RefCell;
use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSPanel, NSScreen, NSScreenSaverWindowLevel, NSView,
    NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFData, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayer, CATransaction, kCAFilterLinear};
use oink_foundation::display::DisplayInfo;

use crate::diagnostics::Health;
use crate::effect::CellRect;

pub(crate) fn available() -> bool {
    true
}

/// 一块屏幕本帧的内容。
pub(crate) struct Layer {
    pub surface: usize,
    pub rect: CellRect,
    pub pixels: Vec<u32>,
}

pub(crate) struct Backend {
    health: Arc<Health>,
    displays: Arc<Vec<DisplayInfo>>,
    generation: u64,
    /// 已交给主线程、还没执行的帧数。
    pending: Arc<AtomicU32>,
    /// 帧间隔（纳秒），由主线程按主屏刷新率写入。
    interval_ns: Arc<AtomicU64>,
    next_frame: Option<Instant>,
}

struct Surface {
    panel: Retained<NSPanel>,
    layer: Retained<CALayer>,
    /// 屏幕左上角在本机全局坐标里的位置与屏幕高度（点）。
    origin: (f64, f64),
    height: f64,
    shown: bool,
}

struct Ui {
    owner: u64,
    generation: u64,
    surfaces: Vec<Surface>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

impl Backend {
    pub(crate) fn new(health: Arc<Health>) -> Option<Self> {
        Some(Self {
            health,
            displays: Arc::new(Vec::new()),
            generation: 0,
            pending: Arc::new(AtomicU32::new(0)),
            interval_ns: Arc::new(AtomicU64::new(crate::DEFAULT_FRAME.as_nanos() as u64)),
            next_frame: None,
        })
    }

    pub(crate) fn set_displays(&mut self, displays: &[DisplayInfo]) {
        self.displays = Arc::new(displays.to_vec());
        self.generation += 1;
        let interval = self.interval_ns.clone();
        run_async(&self.health, move || {
            let Some(mtm) = MainThreadMarker::new() else { return };
            // 换了显示器：旧窗口全部撤掉，下一帧按新的显示器重建。
            UI.with(|ui| {
                if let Some(old) = ui.borrow_mut().take() {
                    for s in old.surfaces {
                        s.panel.orderOut(None);
                    }
                }
            });
            let fps = NSScreen::mainScreen(mtm).map_or(60, |s| s.maximumFramesPerSecond());
            let fps = if (30..=240).contains(&fps) { fps } else { 60 };
            interval.store(1_000_000_000 / fps as u64, Ordering::Relaxed);
        });
    }

    /// 拖尾范围与每块屏幕的交集（格子）。
    pub(crate) fn regions(&self, bounds: CellRect, cell: f64) -> Vec<(usize, CellRect)> {
        self.displays
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                let r = bounds.intersect(&crate::effect::cells_of(d.bounds, cell));
                (!r.is_empty()).then_some((i, r))
            })
            .collect()
    }

    pub(crate) fn present(&mut self, cell: f64, layers: Vec<Layer>) {
        if self.pending.load(Ordering::Acquire) >= 2 {
            // 主线程来不及：丢掉这一帧。
            self.health.dropped();
            return;
        }
        self.pending.fetch_add(1, Ordering::AcqRel);
        let pending = self.pending.clone();
        let displays = self.displays.clone();
        let generation = self.generation;
        let health = self.health.clone();
        let ok = run_async(&self.health, move || {
            if let Some(mtm) = MainThreadMarker::new()
                && let Err(e) = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    show_frame(mtm, &displays, generation, health.id, cell, &layers);
                    health.applied();
                }))
            {
                health.panic();
                tracing::error!(?e, "cursor trail frame panicked");
            }
            pending.fetch_sub(1, Ordering::AcqRel);
        });
        if !ok {
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }

    pub(crate) fn hide(&mut self) {
        self.next_frame = None;
        run_async(&self.health, || {
            UI.with(|ui| {
                if let Some(ui) = ui.borrow_mut().as_mut() {
                    for s in &mut ui.surfaces {
                        if s.shown {
                            s.panel.orderOut(None);
                            // SAFETY: nil contents 合法。
                            unsafe { s.layer.setContents(None) };
                            s.shown = false;
                        }
                    }
                }
            });
        });
    }

    /// 按固定节拍出帧（下一帧的时刻 = 上一帧的时刻 + 间隔），睡眠的误差不累积；落后时从现在重新开始。
    pub(crate) fn wait_frame(&mut self, frame_start: Instant) {
        let interval = Duration::from_nanos(self.interval_ns.load(Ordering::Relaxed));
        let next = match self.next_frame {
            Some(n) if n + interval > frame_start => n + interval,
            _ => frame_start + interval,
        };
        self.next_frame = Some(next);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }
}

fn make_surface(mtm: MainThreadMarker, d: &DisplayInfo, primary_h: f64) -> Surface {
    let b = d.bounds;
    // CoreGraphics 全局坐标（左上原点、y 向下）→ Cocoa（主屏左下原点、y 向上）。
    let frame = NSRect::new(NSPoint::new(b.x, primary_h - (b.y + b.h)), NSSize::new(b.w, b.h));
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(false);
    // Avoid AppKit _NSWindowTransformAnimation workers on every trail show/hide.
    panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
    panel.setIgnoresMouseEvents(true);
    let policy = oink_foundation::panel_policy::PanelPolicy::BACKEND_OVERLAY;
    panel.setHidesOnDeactivate(policy.hides_on_deactivate);
    // Hide Others may hide the backend, but this input-feedback surface must
    // remain available independently of the settings application's visibility.
    panel.setCanHide(policy.can_hide);
    panel.setBecomesKeyOnlyIfNeeded(true);
    // SAFETY: 窗口由我们一直持有，关闭时不释放。
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setLevel(NSScreenSaverWindowLevel);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(b.w, b.h)),
    );
    let root = CALayer::new();
    // 图层坐标是左下原点、y 向上（视图不翻转；根图层的 geometryFlipped 对窗口内容视图不生效，实测 y 颠倒），
    // 帧的位置在 show_frame 里换算。
    view.setLayer(Some(&root));
    view.setWantsLayer(true);
    let layer = CALayer::new();
    // SAFETY: 系统常量。
    layer.setMagnificationFilter(unsafe { kCAFilterLinear });
    layer.setHidden(true);
    root.addSublayer(&layer);
    panel.setContentView(Some(&view));
    Surface {
        panel,
        layer,
        origin: (b.x, b.y),
        height: b.h,
        shown: false,
    }
}

fn show_frame(
    mtm: MainThreadMarker,
    displays: &[DisplayInfo],
    generation: u64,
    owner: u64,
    cell: f64,
    layers: &[Layer],
) {
    UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        if ui.as_ref().is_none_or(|u| u.generation != generation) {
            if let Some(old) = ui.take() {
                for s in old.surfaces {
                    s.panel.orderOut(None);
                }
            }
            let _ = NSApplication::sharedApplication(mtm);
            let primary_h = NSScreen::screens(mtm)
                .firstObject()
                .map_or(0.0, |s| s.frame().size.height);
            *ui = Some(Ui {
                owner,
                generation,
                surfaces: displays.iter().map(|d| make_surface(mtm, d, primary_h)).collect(),
            });
        }
        let Some(ui) = ui.as_mut() else { return };
        // OS workaround: untagged DeviceRGB layer contents force CoreAnimation
        // to colour-match the entire image on the CPU at every transaction
        // (copy_image/create_image_by_rendering/vImage, including alpha conversion).
        // Tag our RGB palette as sRGB so the compositor can handle colour matching.
        // SAFETY: Apple's immutable named colour-space constant.
        let Some(space) = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })) else {
            return;
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for (i, s) in ui.surfaces.iter_mut().enumerate() {
            let Some(l) = layers.iter().find(|l| l.surface == i) else {
                s.layer.setHidden(true);
                continue;
            };
            let (w, h) = (l.rect.w as usize, l.rect.h as usize);
            // SAFETY: u32 像素按字节看待，长度为 w*h*4。
            let bytes = unsafe { std::slice::from_raw_parts(l.pixels.as_ptr().cast::<u8>(), l.pixels.len() * 4) };
            let data = CFData::from_bytes(bytes);
            let Some(provider) = CGDataProvider::with_cf_data(Some(&data)) else {
                continue;
            };
            // SAFETY: 尺寸与数据一致；decode 为空。BGRA 预乘（小端 32 位、alpha 在前）。
            let image = unsafe {
                CGImage::new(
                    w,
                    h,
                    8,
                    32,
                    w * 4,
                    Some(&space),
                    CGBitmapInfo(CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0),
                    Some(&provider),
                    std::ptr::null(),
                    false,
                    CGColorRenderingIntent::RenderingIntentDefault,
                )
            };
            let Some(image) = image else { continue };
            // 全局坐标（y 向下）→ 屏幕内的图层坐标（左下原点、y 向上）。
            let top = l.rect.y as f64 * cell - s.origin.1;
            let frame = CGRect::new(
                CGPoint::new(l.rect.x as f64 * cell - s.origin.0, s.height - top - h as f64 * cell),
                CGSize::new(w as f64 * cell, h as f64 * cell),
            );
            // SAFETY: CGImage 是图层合法的 contents。
            unsafe { s.layer.setContents(Some(image_object(&image))) };
            s.layer.setFrame(frame);
            s.layer.setHidden(false);
            if !s.shown {
                s.panel.orderFrontRegardless();
                s.shown = true;
            }
        }
        CATransaction::commit();
    });
}

fn image_object(image: &CGImage) -> &objc2::runtime::AnyObject {
    // SAFETY: CGImageRef 是 CF 对象，可以当作 Objective-C 对象（toll-free bridged 的 id）传给 `contents`。
    unsafe { &*(image as *const CGImage).cast::<objc2::runtime::AnyObject>() }
}

// ───────────── 主队列 ─────────────

#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}

unsafe extern "C" {
    static _dispatch_main_q: DispatchQueue;
    fn dispatch_async_f(queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// 主队列里最多排多少个还没执行的工作（主线程不服务主队列时防止堆积）。
const MAX_PENDING: u32 = 16;
static QUEUED: AtomicU32 = AtomicU32::new(0);

extern "C" fn trampoline(ctx: *mut c_void) {
    // SAFETY: ctx 是 run_async 里 Box::into_raw 得到的 Box<Job>，只执行一次。
    let job = unsafe { Box::from_raw(ctx as *mut Job) };
    QUEUED.fetch_sub(1, Ordering::AcqRel);
    if std::panic::catch_unwind(AssertUnwindSafe(job)).is_err() {
        tracing::error!("cursor trail main-thread job panicked");
    }
}

/// 在主线程上执行 `f`，不等待。返回是否已投递。
fn run_async(health: &Arc<Health>, f: impl FnOnce() + Send + 'static) -> bool {
    if QUEUED.fetch_add(1, Ordering::AcqRel) >= MAX_PENDING {
        QUEUED.fetch_sub(1, Ordering::AcqRel);
        health.dropped();
        return false;
    }
    let ticket = health.ticket();
    let job: Box<Job> = Box::new(Box::new(move || {
        ticket.start();
        f();
    }));
    // SAFETY: 主队列对象在进程内一直有效；context 由 trampoline 接管并释放。
    unsafe {
        dispatch_async_f(
            &raw const _dispatch_main_q,
            Box::into_raw(job) as *mut c_void,
            trampoline,
        )
    };
    true
}

// Observe AppKit and WindowServer independently of the cached `shown` flag.
pub(crate) fn diagnose(health: &Health, mtm: MainThreadMarker) {
    UI.with(|slot| {
        let ui = slot.borrow();
        let panels: Vec<_> = ui
            .as_ref()
            .into_iter()
            .flat_map(|u| u.surfaces.iter().map(|s| (&*s.panel, s.shown, !s.layer.isHidden())))
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
