//! macOS：每块屏幕一个不透明的全屏 `NSPanel`，图块各是一个 `CALayer`。
//!
//! - 窗口：无边框、不透明、背景色即游戏背景。层级 `NSScreenSaverWindowLevel - 1`：在 Dock、菜单栏、全屏
//!   应用之上，在光标定位拖尾的窗口（`NSScreenSaverWindowLevel`）之下。每块屏幕一个窗口（「显示器具有
//!   单独的空间」开着时跨屏的窗口只显示在一块屏幕上）。
//! - **键盘焦点**：打开时第一块屏幕的窗口成为 key 窗口，之后点哪一块哪一块成为 key——像普通的全屏游戏
//!   一样，按键由系统正常投递给它，视图收到后除 Esc 外什么也不做，所以进行期间键盘输入不会进到下面的
//!   应用。不挂事件 tap。做法见 [`AimPanel`]。
//! - 点击 / 按键：内容视图是 `NSView` 的子类，`mouseDown:` 把位置（换算成本机全局坐标）放进队列，
//!   `keyDown:` 里的 Esc 置一个标志，绘制线程每帧取走。收到的是系统投递给窗口的事件：本机键鼠的原生事件，
//!   或 Oink 在本机注入的事件，叠加层不区分、也不经过 Oink 的截获（暂停时截获已经摘掉，照样成立）。
//! - 图块：绘制线程把最新的一帧放进共享槽，主线程上的一个任务取出来，与已有图层按 `(id, version, 位置)`
//!   比较后增删改。主线程来不及时只保留最新的一帧，不排队。
//! - 线程：AppKit / 图层只在主线程上操作（经主 dispatch 队列）；关闭也是主线程上的一个任务，不设排队
//!   上限（它必须执行）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSEvent, NSPanel, NSScreen, NSScreenSaverWindowLevel, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFData, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayer, CATransaction};
use oink_foundation::Point;

use super::scene::{BACKGROUND, PxRect, SceneDisplay, Sprite};

pub(crate) fn available() -> bool {
    true
}

/// 叠加层收到的点击（本机全局坐标）。同一时刻只有一个叠加层。
static CLICKS: Mutex<Vec<Point>> = Mutex::new(Vec::new());
/// 叠加层的窗口收到了一次 Esc 按下（不是按住不放的重复）。
static ESCAPE: AtomicBool = AtomicBool::new(false);
/// 当前叠加层的代号：窗口属于哪一次打开（主线程上的任务据此丢弃过时的操作）。
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct Win {
    panel: Retained<AimPanel>,
    root: Retained<CALayer>,
    /// 屏幕高度（点）与每点的设备像素数。
    height: f64,
    ppu: f64,
    layers: HashMap<u64, (u64, PxRect, Retained<CALayer>)>,
}

struct Ui {
    generation: u64,
    windows: Vec<Win>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

define_class!(
    /// 能成为 key 窗口的无边框面板。
    ///
    /// 为什么需要：叠加层要像普通全屏游戏那样拿到键盘焦点，按键才由系统投递给它而不是下面的应用。
    /// 无边框窗口默认不能成为 key 窗口（`canBecomeKeyWindow` 为 NO），所以要子类化。
    ///
    /// 为什么是「不激活的面板成为 key」而不是把 Oink 激活成前台应用：`NonactivatingPanel` 成为 key 时
    /// 键盘事件交给它，但前台应用不变——游戏结束、窗口撤掉后，系统把 key 状态还给前台应用原来的 key 窗口，
    /// 输入框的焦点原样还在，不需要记住并重新激活之前的应用。激活 Oink 的话，结束后要自己把前一个应用
    /// 激活回来，而且 Oink 的主窗口会被带到前面。
    ///
    /// 不影响什么：系统级的组合键（Cmd+Tab、Mission Control、Spotlight、截图、媒体键等）在按键到达窗口
    /// 之前就由系统处理，照常可用；用户切到别的应用后键盘焦点跟着走，再点一下叠加层就回到叠加层。
    /// 不成为 main 窗口（不改变前台应用的主窗口状态）。
    ///
    /// 实测：macOS 26.6。
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkAimPanel"]
    struct AimPanel;

    impl AimPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            false
        }
    }
    unsafe impl NSObjectProtocol for AimPanel {}
);

/// 虚拟键码：Esc。
const KEY_ESCAPE: u16 = 53;

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkAimView"]
    struct AimView;

    impl AimView {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            // 窗口坐标（左下原点、y 向上）→ 本机全局坐标（主屏左上原点、y 向下）。窗口正好盖住一块屏幕。
            let Some(window) = self.window() else { return };
            let Some(mtm) = MainThreadMarker::new() else { return };
            let primary_h = NSScreen::screens(mtm).firstObject().map_or(0.0, |s| s.frame().size.height);
            let frame = window.frame();
            let p = event.locationInWindow();
            let pos = Point::new(frame.origin.x + p.x, primary_h - (frame.origin.y + p.y));
            CLICKS.lock().unwrap_or_else(|e| e.into_inner()).push(pos);
        }

        /// 窗口还不是 key 窗口时（用户切走后点回来），这第一次点击也要交给视图（同时窗口成为 key）。
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// 视图是窗口的第一响应者：按键事件交到这里。
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        /// 按键到此为止：Esc 结束游戏，其它键什么也不做（不交给父类——没人处理的按键系统会响一声）。
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if event.keyCode() == KEY_ESCAPE && !event.isARepeat() {
                ESCAPE.store(true, Ordering::Release);
            }
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, _event: &NSEvent) {}

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, _event: &NSEvent) {}

        /// 带 Cmd 等修饰键的按键先走「快捷键」这条路：不在这里吞掉的话，会落到 Oink 自己的菜单
        /// （Cmd+Q 会退出 Oink、Cmd+W 会关它的主窗口）。Esc 也可能从这条路来。
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            if event.keyCode() == KEY_ESCAPE && !event.isARepeat() {
                ESCAPE.store(true, Ordering::Release);
            }
            true
        }
    }
    unsafe impl NSObjectProtocol for AimView {}
);

pub(crate) struct Backend {
    generation: u64,
    /// 最新的一帧；主线程上的任务取走。
    slot: Arc<Mutex<Option<Vec<Vec<Sprite>>>>>,
    /// 主线程上是否已有一个还没执行的出帧任务。
    posted: Arc<AtomicBool>,
    interval_ns: Arc<AtomicU64>,
    next_frame: Option<Instant>,
}

impl Backend {
    pub(crate) fn open(displays: &[SceneDisplay]) -> Option<Self> {
        let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
        CLICKS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        ESCAPE.store(false, Ordering::Release);
        let interval_ns = Arc::new(AtomicU64::new(super::DEFAULT_FRAME.as_nanos() as u64));
        let interval = interval_ns.clone();
        let displays = displays.to_vec();
        run_on_main(move || {
            let Some(mtm) = MainThreadMarker::new() else { return };
            // 打开的任务排到主线程时这一次已经被关掉了：不再创建窗口。
            if GENERATION.load(Ordering::Acquire) != generation {
                return;
            }
            close_windows();
            let _ = NSApplication::sharedApplication(mtm);
            let primary_h = NSScreen::screens(mtm)
                .firstObject()
                .map_or(0.0, |s| s.frame().size.height);
            let windows: Vec<Win> = displays.iter().map(|d| make_window(mtm, d, primary_h)).collect();
            // 第一块屏幕的窗口拿键盘焦点；之后用户点哪一块，哪一块成为 key。
            if let Some(first) = windows.first() {
                first.panel.makeKeyWindow();
                // 只记日志：没拿到焦点时按键会进下面的应用（Esc 也到不了），用户点一下叠加层即可拿到。
                tracing::info!(key = first.panel.isKeyWindow(), "aim overlay keyboard focus");
            }
            UI.with(|ui| *ui.borrow_mut() = Some(Ui { generation, windows }));
            let fps = NSScreen::mainScreen(mtm).map_or(60, |s| s.maximumFramesPerSecond());
            let fps = if (30..=240).contains(&fps) { fps } else { 60 };
            interval.store(1_000_000_000 / fps as u64, Ordering::Relaxed);
        });
        Some(Self {
            generation,
            slot: Arc::new(Mutex::new(None)),
            posted: Arc::new(AtomicBool::new(false)),
            interval_ns,
            next_frame: None,
        })
    }

    /// 自上次调用以来窗口是否收到过 Esc。
    pub(crate) fn take_escape(&mut self) -> bool {
        ESCAPE.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn poll_clicks(&mut self) -> Vec<Point> {
        std::mem::take(&mut *CLICKS.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub(crate) fn present(&mut self, frame: &[Vec<Sprite>], _dirty: &[Vec<PxRect>]) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame.to_vec());
        if self.posted.swap(true, Ordering::AcqRel) {
            // 主线程还没取走上一帧：它执行时拿到的就是刚放进去的这一帧。
            return;
        }
        let (slot, posted, generation) = (self.slot.clone(), self.posted.clone(), self.generation);
        run_on_main(move || {
            posted.store(false, Ordering::Release);
            let frame = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(frame) = frame {
                apply(generation, &frame);
            }
        });
    }

    /// 按固定节拍出帧（误差不累积；落后时从现在重新开始）。
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

impl Drop for Backend {
    fn drop(&mut self) {
        let generation = self.generation;
        // 让还没执行的打开任务作废（它看到代号变了就不创建窗口）。
        let _ = GENERATION.compare_exchange(generation, generation + 1, Ordering::AcqRel, Ordering::Acquire);
        run_on_main(move || {
            let ours = UI.with(|ui| ui.borrow().as_ref().is_some_and(|u| u.generation == generation));
            if ours {
                close_windows();
            }
        });
    }
}

fn close_windows() {
    UI.with(|ui| {
        if let Some(old) = ui.borrow_mut().take() {
            for w in old.windows {
                w.panel.orderOut(None);
                w.panel.close();
            }
        }
    });
}

fn make_window(mtm: MainThreadMarker, d: &SceneDisplay, primary_h: f64) -> Win {
    let b = d.bounds;
    // CoreGraphics 全局坐标（左上原点、y 向下）→ Cocoa（主屏左下原点、y 向上）。
    let frame = NSRect::new(NSPoint::new(b.x, primary_h - (b.y + b.h)), NSSize::new(b.w, b.h));
    // SAFETY: NSPanel 的指定初始化方法，用在我们只在主线程上使用的子类上。
    let panel: Retained<AimPanel> = unsafe {
        msg_send![AimPanel::alloc(mtm), initWithContentRect: frame,
            styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            backing: NSBackingStoreType::Buffered, defer: false]
    };
    let bg = |shift: u32| f64::from((BACKGROUND >> shift) & 0xFF) / 255.0;
    panel.setOpaque(true);
    panel.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        bg(16),
        bg(8),
        bg(0),
        1.0,
    )));
    panel.setHasShadow(false);
    panel.setIgnoresMouseEvents(false);
    let policy = oink_foundation::panel_policy::PanelPolicy::BACKEND_OVERLAY;
    panel.setHidesOnDeactivate(policy.hides_on_deactivate);
    // Keep the active locator session visible if Hide Others hides the backend;
    // deactivation and application hiding are distinct AppKit policies.
    panel.setCanHide(policy.can_hide);
    // 点击任何一块叠加层都让它成为 key（默认「只在需要时」：视图不是文本输入就不成为 key）。
    panel.setBecomesKeyOnlyIfNeeded(false);
    // SAFETY: 窗口由我们一直持有，关闭时不释放。
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setLevel(NSScreenSaverWindowLevel - 1);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    let this = AimView::alloc(mtm).set_ivars(());
    // SAFETY: NSView 的指定初始化方法；子类没有实例变量。
    let view: Retained<AimView> =
        unsafe { msg_send![super(this), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(b.w, b.h))] };
    let root = CALayer::new();
    view.setLayer(Some(&root));
    view.setWantsLayer(true);
    panel.setContentView(Some(&view));
    panel.makeFirstResponder(Some(&view));
    panel.orderFrontRegardless();
    Win {
        panel,
        root,
        height: b.h,
        ppu: d.pixels_per_unit.max(0.5),
        layers: HashMap::new(),
    }
}

/// 主线程：把窗口里的图层变成 `frame` 描述的样子。
fn apply(generation: u64, frame: &[Vec<Sprite>]) {
    UI.with(|ui| {
        let mut ui = ui.borrow_mut();
        let Some(ui) = ui.as_mut().filter(|u| u.generation == generation) else {
            return;
        };
        let Some(space) = CGColorSpace::new_device_rgb() else {
            return;
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for (win, sprites) in ui.windows.iter_mut().zip(frame) {
            win.layers.retain(|id, (_, _, layer)| {
                let keep = sprites.iter().any(|s| s.id == *id);
                if !keep {
                    layer.removeFromSuperlayer();
                }
                keep
            });
            for (z, s) in sprites.iter().enumerate() {
                let rect = s.rect();
                let entry = win.layers.entry(s.id).or_insert_with(|| {
                    let layer = CALayer::new();
                    win.root.addSublayer(&layer);
                    (0, PxRect { x: 0, y: 0, w: 0, h: 0 }, layer)
                });
                entry.2.setZPosition(z as f64);
                if entry.0 != s.version {
                    let Some(image) = image_of(s, &space) else { continue };
                    // SAFETY: CGImage 是图层合法的 contents。
                    unsafe { entry.2.setContents(Some(image_object(&image))) };
                    entry.0 = s.version;
                }
                if entry.1 != rect {
                    // 屏幕内的设备像素（左上原点）→ 图层坐标（点，左下原点、y 向上）。图的像素数正好是
                    // 图层大小 × 每点像素数，不缩放。
                    let (w, h) = (f64::from(s.w) / win.ppu, f64::from(s.h) / win.ppu);
                    entry.2.setFrame(CGRect::new(
                        CGPoint::new(f64::from(s.x) / win.ppu, win.height - f64::from(s.y) / win.ppu - h),
                        CGSize::new(w, h),
                    ));
                    entry.1 = rect;
                }
            }
        }
        CATransaction::commit();
    });
}

fn image_of(s: &Sprite, space: &CGColorSpace) -> Option<objc2_core_foundation::CFRetained<CGImage>> {
    let (w, h) = (s.w as usize, s.h as usize);
    // SAFETY: u32 像素按字节看待，长度为 w*h*4。
    let bytes = unsafe { std::slice::from_raw_parts(s.pixels.as_ptr().cast::<u8>(), s.pixels.len() * 4) };
    let data = CFData::from_bytes(bytes);
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    // SAFETY: 尺寸与数据一致；decode 为空。BGRA 预乘（小端 32 位、alpha 在前）。
    unsafe {
        CGImage::new(
            w,
            h,
            8,
            32,
            w * 4,
            Some(space),
            CGBitmapInfo(CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0),
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
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

extern "C" fn trampoline(ctx: *mut c_void) {
    // SAFETY: ctx 是 run_on_main 里 Box::into_raw 得到的 Box<Job>，只执行一次。
    let job = unsafe { Box::from_raw(ctx as *mut Job) };
    if std::panic::catch_unwind(AssertUnwindSafe(job)).is_err() {
        tracing::error!("aim overlay main-thread job panicked");
    }
}

/// 在主线程上执行 `f`，不等待。不设排队上限：每个叠加层只排打开、关闭各一次，出帧任务同一时刻至多一个。
fn run_on_main(f: impl FnOnce() + Send + 'static) {
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
