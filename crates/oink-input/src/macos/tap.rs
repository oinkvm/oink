//! 截获：专用线程上的 active session event tap。
//!
//! 回调里只做：识别并放过自己注入的事件、翻译、调用处理函数、决定吞掉与否。所有发给 WindowServer 的
//! 同步调用（warp、解耦、隐藏光标）都推迟到回调返回之后，由本线程的循环执行（见 `Pending`）。
//! 循环每 250 ms 醒来一次：执行推迟的光标操作；每 5 s 检查 tap 是否仍启用；每 0.5 s 检查安全输入，
//! 开启时给仍按着的键补发抬起。
//!
//! **tap 被系统停用后的核对**：回调超时（或用户输入）使系统停用 tap 时，停用期间的事件（可能包括抬起）
//! 不会交给我们。重新启用后，把「截获到仍按着的键和按钮」与 HID 系统状态（`CGEventSourceKeyState` /
//! `CGEventSourceButtonState`，HID 层状态；HID tap 注入也可能更新它）比较，对已经松开的补报抬起，让路由
//! 及时在远端松开。Caps 的 Quartz 状态是锁定状态，不能作为物理按压快照；
//! 捕获中断或焦点交接时结束旧的 Caps 手势，下一枚明确方向的事件重新建立状态。
//!
//! **按着键期间的核对**（bug 9）：tap 没被停用时也会漏掉抬起——例如其他程序的 active HID tap（两台 Mac 上
//! 都有飞书的）处理超时，那个事件就到不了我们这里；源机以为还按着，目标端就一直按着、重复。所以有键按着时
//! 循环每 [`HELD_CHECK_SECS`] 醒来一次，同样按 HID 系统状态核对：连续两次（相隔至少 [`LOST_UP_CONFIRM`]）
//! 都已松开的键补报抬起。两次确认避开「抬起事件已在队列里、还没交给回调」的瞬间。
//!
//! **Caps Lock**：物理按下 / 抬起取自 `NX_SYSDEFINED / NX_KEYTYPE_CAPS_LOCK` 的 data1，
//! 不从 FlagsChanged 的数量推断。合成 FlagsChanged 使用调用者提供的 stateless 位。Remote 模式下转发后，
//! 由 `capslock::LocalGuard` 恢复本机被 HID 层改掉的锁定状态与输入法。

use oink_foundation::handoff_timing::{self, Stage as TimingStage, Token as TimingToken};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType, CGMouseButton,
};
use oink_foundation::Point;
use oink_foundation::input::{
    CaptureMode, CapturedEvent, Disposition, InputEvent, KeyCode, MouseButton, ScrollPhase, ScrollUnit, SourceKind,
};

use super::capslock::LocalGuard;
use super::inject::{MAGIC, button_from_number, button_number, current_location, device_bits, modifier_flags};
use super::{cursor, device, keymap, sysevent, system};
use crate::warp_delta::{WarpDeltaFix, clear_of_active_edge_zones, nudge_point, warp_moves_cursor};
use crate::{EventHandler, InputError};

/// 推迟到回调之外执行的操作。
const PENDING_APPLY_MODE: u8 = 1;
/// Remote 模式下有移动事件：核对隐藏的光标是否还在冻结的位置（只检测、记日志）。
const PENDING_FREEZE_CHECK: u8 = 2;
/// tap 刚被重新启用：核对按键与按钮的物理状态。
const PENDING_RECONCILE: u8 = 4;
/// 刚进入 Remote：记下本机输入法。
const PENDING_CAPS_ENTER: u8 = 8;
/// 刚回到 Local：恢复本机的大写锁定与输入法。
const PENDING_CAPS_LEAVE: u8 = 16;
/// Remote 模式下吞掉了 Caps Lock：核对本机的大写锁定与输入法。
const PENDING_CAPS_GUARD: u8 = 32;
/// AppKit has installed the blank glyph; move on the tap thread outside its callback.
const PENDING_OWNER_MOVE: u8 = 128;

/// `NX_ALPHASHIFT_STATELESS_MASK` / `NX_DEVICE_ALPHASHIFT_STATELESS_MASK`：Caps Lock 键此刻按着
/// （IOKit `IOLLEvent.h`）。
const ALPHASHIFT_STATELESS: u64 = 0x0100_0000 | 0x0000_0080;

/// Remote 模式下隐藏的光标。
#[derive(Clone, Copy, Debug, PartialEq)]
enum HiddenCursor {
    /// 不在 Remote 模式（光标显示）。
    Shown,
    /// 冻结（隐藏、解耦）在 `at`（`CGEventCreate` 读数），从 `since` 起；`seen`：上一次核对时光标所在处
    /// （只用于检测与日志，见 `check_frozen`）。
    Frozen { at: Point, since: Instant, seen: Point },
}

impl HiddenCursor {
    fn frozen(at: Point) -> Self {
        Self::Frozen {
            at,
            since: Instant::now(),
            seen: at,
        }
    }
}

/// Posted-event accounting is updated under a short lock, never across WindowServer calls.
/// Keep its epoch separately from owner_generation: stop invalidates UI work before draining.
#[derive(Clone, Copy, Default)]
struct OwnerPosts {
    generation: u64,
    count: u32,
    started_ns: u64,
}

mod button_evidence;
/// 与截获线程共享的状态。回调不持有 cursor_lock；事件账本和位移修正只用短锁，不跨 WindowServer 调用。
mod observations;

pub(crate) struct Shared {
    safety: Arc<crate::session_gate::SessionGate>,
    capture_epoch: AtomicU64,
    timing_epoch: AtomicU64,
    observations: Option<observations::State>,
    handler: EventHandler,
    gestures: Mutex<super::gesture_diagnostics::Tracker>,
    physical_buttons: Arc<super::physical_buttons::PhysicalButtons>,
    motion: Arc<super::cursor_reveal::Motion>,
    motion_generation: u64,
    remote: AtomicBool,
    keyboard_remote: AtomicBool,
    /// Remote 模式下隐藏的光标：冻结在哪里（只在截获线程的循环里、持有 `cursor_lock` 时修改）。
    hidden: Mutex<HiddenCursor>,
    /// 冻结期间放行了移动事件的次数（不变量：应当为 0；只供记录）。
    passed_while_frozen: AtomicU64,
    owner_generation: AtomicU64,
    drag_anchor: Mutex<Option<oink_foundation::dnd::DragAnchor>>,
    owner_move: Mutex<Option<(u64, Point)>>,
    owner_posts: Mutex<OwnerPosts>,
    /// 路由给的停靠点（参考屏中央）：macOS 不停到这里，只用作「朝屏幕内部」的方向（`nudge_point`）。
    park: Mutex<Option<Point>>,
    /// `restore_local` 的落点：回到 Local 时先 warp 到这里再显示（见 `apply_mode`）。
    restore_pending: AtomicBool,
    /// 已经离开 Remote、回到本机的 warp 还没做完：吞掉移动事件（见 `apply_mode` 的说明）。
    hold_motion: AtomicBool,
    restore_x: AtomicU64,
    restore_y: AtomicU64,
    stop: AtomicBool,
    pending: AtomicU8,
    port: AtomicPtr<CFMachPort>,
    run_loop: AtomicPtr<CFRunLoop>,
    /// 光标操作互斥（不在回调里使用）。
    cursor_lock: Mutex<()>,
    /// warp 之后第一个移动事件的位移修正（见 `crate::warp_delta`）。回调里只短暂持有，
    /// 从不在持有时调用 WindowServer。
    warp_fix: Mutex<WarpDeltaFix>,
    ordinary_buttons: Mutex<crate::ordinary_motion::ButtonGate>,
    pointer_transition: Mutex<crate::pointer_transition::Tracker>,
    /// 上一次安排冻结核对的时刻（`now_event_ns`）：回调据此限制核对的频率。
    freeze_check_ns: AtomicU64,
    /// Remote 模式下保护本机的大写锁定与输入法。
    caps_guard: LocalGuard,
}

pub(crate) struct Tap {
    shared: &'static Shared,
    observer: Option<JoinHandle<()>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Tap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tap").finish_non_exhaustive()
    }
}

thread_local! {
    static CONTACTS: RefCell<Option<super::contacts::Capture>> = const { RefCell::new(None) };
    /// 当前线程是否正在执行 tap 回调。
    static IN_CALLBACK: Cell<bool> = const { Cell::new(false) };
    /// 截获到的仍按着的键（安全输入开启、tap 恢复后核对时补发抬起）。含系统定义事件的媒体键。
    static PRESSED: RefCell<HashSet<KeyCode>> = RefCell::new(HashSet::new());
    /// 截获到的仍按着的鼠标按钮（tap 恢复后核对时补发抬起）。
    static BUTTONS: RefCell<HashSet<MouseButton>> = RefCell::new(HashSet::new());
    /// 每个按着的键在按下时的处理结果；没见过按下的键的自动重复沿用它（见 `Translated::repeat_of`）。
    /// 键是 [`disp_key`]：kVK，或系统定义事件的 `AUX_DISP | 键类型`。
    static KEY_DISPOSITION: RefCell<HashMap<u16, Disposition>> = RefCell::new(HashMap::new());
}

/// `KEY_DISPOSITION` 里系统定义事件的键：与 kVK（< 0x100）分开。
const AUX_DISP: u16 = 0x8000;

/// 键在 `KEY_DISPOSITION` 里的键：普通键为 kVK，媒体键为 `AUX_DISP | NX 键类型`。
fn disp_key(code: KeyCode) -> Option<u16> {
    keymap::hid_to_mac(code).or_else(|| keymap::hid_to_nx(code).map(|n| AUX_DISP | n as u16))
}

/// 有键按着时 run loop 的超时（核对漏掉的抬起的间隔）。
const HELD_CHECK_SECS: f64 = 0.05;
/// 看到「物理上已松开」之后至少再过这么久、再看到一次才补报抬起。
const LOST_UP_CONFIRM: Duration = Duration::from_millis(40);

/// 核对漏掉的抬起：记下第一次看到物理上已松开的时间，确认后才补报。
#[derive(Debug, Default)]
struct LostUps {
    suspect: HashMap<KeyCode, Instant>,
}

impl LostUps {
    /// `up`：这一轮物理上已松开、但截获仍记为按着的键。返回需要补报抬起的键（连续两次、相隔足够久）。
    fn confirm(&mut self, up: &[KeyCode], now: Instant) -> Vec<KeyCode> {
        self.suspect.retain(|k, _| up.contains(k));
        let mut lost = Vec::new();
        for &k in up {
            let first = *self.suspect.entry(k).or_insert(now);
            if now.duration_since(first) >= LOST_UP_CONFIRM {
                lost.push(k);
                self.suspect.remove(&k);
            }
        }
        lost
    }

    fn clear(&mut self) {
        self.suspect.clear();
    }
}

/// 进程内单调时钟的起点。
fn epoch() -> Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    *E.get_or_init(Instant::now)
}

fn now_us() -> u64 {
    epoch().elapsed().as_micros() as u64
}

unsafe extern "C" {
    fn clock_gettime_nsec_np(clock_id: libc::clockid_t) -> u64;
}

/// 与事件时间戳（`CGEventGetTimestamp`）同一时钟的当前时刻（纳秒，`CLOCK_UPTIME_RAW`；本机实测一致）。
pub(super) fn now_event_ns() -> u64 {
    // SAFETY: 无副作用的时钟查询。
    unsafe { clock_gettime_nsec_np(libc::CLOCK_UPTIME_RAW) }
}

pub(super) fn in_callback() -> bool {
    IN_CALLBACK.with(Cell::get)
}

fn mask_bit(t: u32) -> CGEventMask {
    1u64 << t
}

/// 截获的事件类型：键盘、鼠标、滚动、系统定义事件（14：媒体键、亮度键），以及触摸板手势（Remote 模式下一律
/// 吞掉；Dock 轻扫、捏合、旋转、智能缩放翻译成 `InputEvent::Gesture`，`sysevent`）。
fn event_mask() -> CGEventMask {
    [
        1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 14, 22, 25, 26, 27, 18, 19, 20, 29, 30, 31, 32,
    ]
    .into_iter()
    .map(mask_bit)
    .fold(0, |a, b| a | b)
}

impl Tap {
    pub(crate) fn active(&self) -> bool {
        self.shared.capture_permitted() && !self.shared.port.load(Ordering::Acquire).is_null()
    }
    pub(crate) fn start(
        handler: EventHandler,
        physical_buttons: Arc<super::physical_buttons::PhysicalButtons>,
        motion: Arc<super::cursor_reveal::Motion>,
        safety: Arc<crate::session_gate::SessionGate>,
    ) -> Result<Self, InputError> {
        Self::start_inner(handler, physical_buttons, motion, safety, None)
    }
    pub(crate) fn start_loop(
        owner: Box<dyn crate::capture_loop::CaptureLoop>,
        physical_buttons: Arc<super::physical_buttons::PhysicalButtons>,
        motion: Arc<super::cursor_reveal::Motion>,
        safety: Arc<crate::session_gate::SessionGate>,
    ) -> Result<Self, InputError> {
        Self::start_inner(
            Arc::new(crate::capture_loop::event),
            physical_buttons,
            motion,
            safety,
            Some(owner),
        )
    }
    fn start_inner(
        handler: EventHandler,
        physical_buttons: Arc<super::physical_buttons::PhysicalButtons>,
        motion: Arc<super::cursor_reveal::Motion>,
        safety: Arc<crate::session_gate::SessionGate>,
        owner: Option<Box<dyn crate::capture_loop::CaptureLoop>>,
    ) -> Result<Self, InputError> {
        epoch();
        physical_buttons.reset();
        // 截获线程与回调通过 'static 引用共享状态；Tap 结束时不释放（每次 start 泄漏一个小结构，
        // 换取回调里无需引用计数；start/stop 次数很少）。
        let shared: &'static Shared = Box::leak(Box::new(Shared {
            capture_epoch: AtomicU64::new(safety.snapshot().epoch),
            safety,
            timing_epoch: AtomicU64::new(0),
            observations: owner.as_ref().map(|_| observations::State::new()),
            physical_buttons,
            motion_generation: motion.new_generation(),
            motion,
            handler,
            gestures: Mutex::new(super::gesture_diagnostics::Tracker::default()),
            remote: AtomicBool::new(false),
            keyboard_remote: AtomicBool::new(false),
            hidden: Mutex::new(HiddenCursor::Shown),
            owner_generation: AtomicU64::new(0),
            drag_anchor: Mutex::new(None),
            owner_move: Mutex::new(None),
            owner_posts: Mutex::new(OwnerPosts::default()),
            passed_while_frozen: AtomicU64::new(0),
            park: Mutex::new(None),
            restore_pending: AtomicBool::new(false),
            hold_motion: AtomicBool::new(false),
            restore_x: AtomicU64::new(0),
            restore_y: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            pending: AtomicU8::new(0),
            port: AtomicPtr::new(std::ptr::null_mut()),
            run_loop: AtomicPtr::new(std::ptr::null_mut()),
            cursor_lock: Mutex::new(()),
            warp_fix: Mutex::new(WarpDeltaFix::new()),
            ordinary_buttons: Mutex::new(crate::ordinary_motion::ButtonGate::default()),
            pointer_transition: Mutex::new(crate::pointer_transition::Tracker::default()),
            freeze_check_ns: AtomicU64::new(0),
            caps_guard: LocalGuard::new(),
        }));
        let mut observer = shared
            .observations
            .as_ref()
            .map(|_| observations::State::spawn(shared))
            .transpose()
            .map_err(|e| InputError::Platform(format!("create input observer: {e}")))?;
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("oink-input-tap".into())
            .spawn(move || run(shared, tx, owner))
            .map_err(|e| {
                shared.stop.store(true, Ordering::Release);
                wake(shared);
                if let Some(observer) = observer.take() {
                    let _ = observer.join();
                }
                InputError::Platform(format!("创建线程失败：{e}"))
            })?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                shared,
                observer,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                shared.stop.store(true, Ordering::Release);
                if let Some(observer) = observer {
                    let _ = observer.join();
                }
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                shared.stop.store(true, Ordering::Release);
                wake(shared);
                if let Some(observer) = observer {
                    let _ = observer.join();
                }
                let _ = thread.join();
                Err(InputError::Platform("启动截获线程超时".into()))
            }
        }
    }

    pub(crate) fn observe_local_pointer<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        self.shared.observe_local_pointer(read)
    }

    pub(crate) fn set_mode(&self, mode: CaptureMode) {
        let s = self.shared;
        let mode = if s.capture_permitted() {
            mode
        } else {
            CaptureMode::Local
        };
        s.timing_epoch
            .store(handoff_timing::scope().map_or(0, |s| s.epoch), Ordering::Relaxed);
        s.gestures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .boundary("captured", "set_mode");
        tracing::debug!(?mode, "gesture lifecycle: capture mode requested");
        s.restore_pending.store(false, Ordering::Release);
        let changed = match mode {
            CaptureMode::Local => {
                s.hold_motion.store(true, Ordering::Release);
                let changed = s.remote.swap(false, Ordering::AcqRel);
                if !changed {
                    s.hold_motion.store(false, Ordering::Release);
                }
                changed
            }
            CaptureMode::Remote { park } => {
                *s.park.lock().unwrap_or_else(|e| e.into_inner()) = Some(park);
                s.hold_motion.store(false, Ordering::Release);
                let changed = !s.remote.swap(true, Ordering::AcqRel);
                if changed && s.owner_generation.load(Ordering::Acquire) == 0 {
                    s.owner_generation
                        .store(cursor::owner::next_generation(), Ordering::Release);
                }
                changed
            }
        };
        if changed {
            s.motion.reset_position_context();
        }
        self.schedule_apply();
    }

    pub(crate) fn set_keyboard_remote(&self, remote: bool) {
        let s = self.shared;
        let remote = remote && s.capture_permitted();
        if s.keyboard_remote.swap(remote, Ordering::AcqRel) == remote {
            return;
        }
        let pending = if remote {
            s.caps_guard.enter_remote();
            PENDING_CAPS_ENTER
        } else {
            PENDING_CAPS_LEAVE
        };
        if let Some(observations) = &s.observations {
            observations.reset_caps.store(true, Ordering::Release);
            if remote {
                s.caps_guard.snapshot_source();
            } else {
                s.caps_guard.leave_remote();
            }
        } else {
            s.pending.fetch_or(pending, Ordering::AcqRel);
        }
        wake(s);
    }

    pub(crate) fn set_drag_anchor(&self, anchor: Option<oink_foundation::dnd::DragAnchor>) {
        *self.shared.drag_anchor.lock().unwrap_or_else(|e| e.into_inner()) = anchor;
    }

    pub(crate) fn move_held_drag(
        &self,
        generation: u64,
        at: Point,
        inject: &dyn Fn() -> Result<(), InputError>,
    ) -> Result<(), InputError> {
        let s = self.shared;
        let _guard = s.cursor_lock.lock().unwrap_or_else(|e| e.into_inner());
        if !s.capture_permitted() {
            return Err(InputError::NotStarted);
        }
        if !s
            .drag_anchor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|a| a.generation == generation)
        {
            return Err(InputError::Platform("stale held drag move".into()));
        }
        // Physical movement stays frozen. MAGIC takes the earlier pass branch,
        // which registers the delivered coordinate in the shared HID ledger.
        // Even a zero-distance event is needed to update the native session.
        s.hold_motion.store(true, Ordering::Release);
        let result = inject();
        // A missing tap ACK does not mean CGEventPost was not delivered. Keep
        // the freeze bookkeeping at the submitted target; never replay/warp.
        s.motion.reset_position_context();
        if s.remote.load(Ordering::Acquire) {
            *s.hidden.lock().unwrap_or_else(|e| e.into_inner()) = HiddenCursor::frozen(at);
        }
        s.hold_motion.store(false, Ordering::Release);
        result
    }

    pub(crate) fn nudge_held_drag(
        &self,
        generation: u64,
        inject: &dyn Fn(Point) -> Result<(), InputError>,
    ) -> Result<(), InputError> {
        let s = self.shared;
        let _guard = s.cursor_lock.lock().unwrap_or_else(|e| e.into_inner());
        if !s.capture_permitted() {
            return Err(InputError::NotStarted);
        }
        let anchor = *s.drag_anchor.lock().unwrap_or_else(|e| e.into_inner());
        let current = current_location();
        let current = Point::new(current.x, current.y);
        let hidden = *s.hidden.lock().unwrap_or_else(|e| e.into_inner());
        let frozen = match hidden {
            HiddenCursor::Frozen { at, .. } => at,
            // HoldSource installs its anchor before the remote-mode transition.
            // During that setup, the same native catcher point is still current.
            HiddenCursor::Shown => current,
        };
        let at = image_nudge_location(anchor, generation, frozen, current)
            .ok_or_else(|| InputError::Platform("stale or relocated held drag image nudge".into()))?;
        // The zero-distance marked post goes through the ordinary tap ACK and
        // placement path. It cannot rebase motion, reveal, warp or refreeze.
        inject(at)
    }

    /// 回到 Local，光标出现在 `at`（`InputBackend::restore_local`）：与模式切换一起在 `apply_mode` 里执行。
    pub(crate) fn restore_local(&self, at: Point) {
        let s = self.shared;
        s.timing_epoch
            .store(handoff_timing::scope().map_or(0, |s| s.epoch), Ordering::Relaxed);
        s.motion.reset_position_context();
        s.gestures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .boundary("captured", "restore_local");
        tracing::debug!(?at, in_callback = IN_CALLBACK.with(Cell::get), "restore_local");
        s.restore_x.store(at.x.to_bits(), Ordering::Release);
        s.restore_y.store(at.y.to_bits(), Ordering::Release);
        s.restore_pending.store(true, Ordering::Release);
        // 先挡住移动事件，再离开 Remote（见 `apply_mode`）：warp 做完之前送到的事件一律吞掉。
        s.hold_motion.store(true, Ordering::Release);
        s.remote.store(false, Ordering::Release);
        self.schedule_apply();
    }

    fn schedule_apply(&self) {
        let s = self.shared;
        if IN_CALLBACK.with(Cell::get) {
            // 在回调里：推迟到回调返回后由截获线程执行。
            handoff_timing::native(TimingStage::NativeDeferred, 1); // capture callback, not a timer
            defer_mode_apply(&s.pending, || wake(s));
        } else {
            apply_mode(s);
        }
    }

    /// 停止截获。在回调里调用时只做标记，由截获线程自行退出并恢复光标。
    pub(crate) fn stop(mut self) {
        let s = self.shared;
        s.motion.set_active(s.motion_generation, false);
        s.owner_generation.store(0, Ordering::Release);
        s.gestures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .boundary("captured", "tap_stop_requested");
        s.stop.store(true, Ordering::Release);
        s.remote.store(false, Ordering::Release);
        if IN_CALLBACK.with(Cell::get) {
            // 不能在自己线程上 join；线程退出时会恢复光标。
            self.thread.take();
            return;
        }
        let rl = s.run_loop.load(Ordering::Acquire);
        if !rl.is_null() {
            // SAFETY: run_loop 指针在截获线程退出前有效（线程退出时先清空再释放）；CFRunLoopStop 可跨线程调用。
            unsafe { CFRunLoop::stop(&*rl) };
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(observer) = self.observer.take() {
            let _ = observer.join();
        }
        apply_mode(s);
    }
}

fn defer_mode_apply(pending: &AtomicU8, notify: impl FnOnce()) {
    pending.fetch_or(PENDING_APPLY_MODE, Ordering::AcqRel);
    // A keyboard-mode wake can be consumed before this cursor request exists.
    // Without its own wake the ordinary observer can park until its 250 ms
    // visibility deadline, leaving the departed cursor visible. Signal only:
    // WindowServer calls still run outside the event-tap callback.
    notify();
}

/// 让截获线程的循环尽快处理推迟的操作。
fn wake(s: &Shared) {
    if let Some(observations) = &s.observations {
        observations.wake();
    }
    let rl = s.run_loop.load(Ordering::Acquire);
    if !rl.is_null() {
        // SAFETY: run_loop 指针在截获线程退出前有效；CFRunLoopStop 可跨线程调用。
        unsafe { CFRunLoop::stop(&*rl) };
    }
}

/// 按当前模式设置光标（不能在回调里调用）。
///
/// 进入 Remote：立即隐藏、解耦并挪出感应区。
fn apply_mode(s: &'static Shared) {
    let _scope = handoff_timing::enter_scope(s.timing_scope());
    handoff_timing::native(TimingStage::NativeApply, 0); // before the native cursor lock
    let _g = s.cursor_lock.lock().unwrap_or_else(|e| e.into_inner());
    handoff_timing::native(TimingStage::NativeApply, 1); // cursor lock acquired
    let mut hidden = s.hidden.lock().unwrap_or_else(|e| e.into_inner());
    apply_mode_locked(s, &mut hidden);
}

fn apply_mode_locked(s: &'static Shared, hidden: &mut HiddenCursor) {
    if !s.capture_permitted() {
        // After withdrawal, posted moves may never be acknowledged. Do not wait,
        // warp, or replay a pending owner move: return local cursor ownership once.
        s.remote.store(false, Ordering::Release);
        s.keyboard_remote.store(false, Ordering::Release);
        s.hold_motion.store(false, Ordering::Release);
        s.restore_pending.store(false, Ordering::Release);
        s.owner_generation.store(0, Ordering::Release);
        s.owner_posts.lock().unwrap_or_else(|e| e.into_inner()).count = 0;
        *hidden = HiddenCursor::Shown;
        cursor::restore();
        return;
    }
    let stop = s.stop.load(Ordering::Acquire);
    let posts = s.owner_posts_snapshot();
    if (!s.remote.load(Ordering::Acquire) || stop) && posts.count != 0 {
        // CGEventPost changes the global pointer before even an active HID tap can reject it.
        // Keep the owner alive until our already-posted events reach the session tap, then warp.
        // A disabled tap can lose acknowledgements; never leave Local or stop waiting forever.
        if now_event_ns().saturating_sub(posts.started_ns) < 250_000_000 {
            if let Some(token) = handoff_timing::scope() {
                let mut event = handoff_timing::Event::new(TimingStage::NativeDeferred, token);
                event.code = 2; // outstanding owner move ACKs
                event.elapsed_us = now_event_ns().saturating_sub(posts.started_ns) / 1000;
                event.count = u64::from(posts.count);
                handoff_timing::emit(event);
            }
            s.hold_motion.store(true, Ordering::Release);
            tracing::debug!(
                pending = posts.count,
                "cursor owner: waiting for posted moves before Local warp"
            );
            return;
        }
        s.owner_posts.lock().unwrap_or_else(|e| e.into_inner()).count = 0;
        tracing::warn!(
            pending = posts.count,
            "cursor owner: event acknowledgement timeout; restoring Local"
        );
    }
    if s.remote.load(Ordering::Acquire) && !stop {
        if *hidden == HiddenCursor::Shown {
            s.warp_fix.lock().unwrap_or_else(|e| e.into_inner()).enter_remote();
            let at = freeze_clear_of_zones(s);
            s.pointer_transition
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mode(at, now_event_ns());
            *hidden = HiddenCursor::frozen(at);
        } else {
            // A Local request may have suppressed queued AppKit work before a coalesced Remote.
            // Re-enter the idempotent preparation so no discarded stage leaves a stalled owner.
            freeze_clear_of_zones(s);
        }
    } else if s.restore_pending.swap(false, Ordering::AcqRel) && !stop {
        s.owner_generation.store(0, Ordering::Release);
        let at = Point::new(
            f64::from_bits(s.restore_x.load(Ordering::Acquire)),
            f64::from_bits(s.restore_y.load(Ordering::Acquire)),
        );
        *hidden = HiddenCursor::Shown;
        let cur = current_location();
        restore_to(s, at, Point::new(cur.x, cur.y));
    } else if *hidden != HiddenCursor::Shown && !stop {
        s.owner_generation.store(0, Ordering::Release);
        // 从远程模式直接回到 Local（关闭共享、紧急暂停等，没有进入点）：光标就在原处出现。冻结期间事件位置
        // 早已漂走，只关联的话下一个事件会把光标从漂走的位置带着走（实测跳了 37 pt）：同样经 warp 把事件
        // 位置拉回光标所在处。
        *hidden = HiddenCursor::Shown;
        let cur = current_location();
        let cur = Point::new(cur.x, cur.y);
        restore_to(s, cur, cur);
    } else {
        s.owner_generation.store(0, Ordering::Release);
        *hidden = HiddenCursor::Shown;
        cursor::restore();
        s.hold_motion.store(false, Ordering::Release);
    }
}

/// Freeze immediately. Relocate only after AppKit owns a transparent cursor at the source point.
fn freeze_clear_of_zones(s: &'static Shared) -> Point {
    cursor::freeze();
    let cur = current_location();
    let cur = Point::new(cur.x, cur.y);
    if s.drag_anchor.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        // AppKit must keep hitting the same native destination until real Drop
        // or return. The catcher already shields the active edge. Freeze in
        // place: the ordinary Dock/edge relocation would cause draggingExited.
        return cur;
    }
    let displays = cursor::display_bounds();
    let settings = system::edge_settings();
    let generation = s.owner_generation.load(Ordering::Acquire);
    let to = clear_of_active_edge_zones(cur, &displays, None, &settings.corners);
    // Only the configured Dock side can need AX. Ordinary edges never create an owner.
    let near_dock = settings.dock.is_some_and(|side| {
        displays.iter().any(|d| {
            let distance = match side {
                crate::warp_delta::DockSide::Bottom => d.bottom() - cur.y,
                crate::warp_delta::DockSide::Left => cur.x - d.x,
                crate::warp_delta::DockSide::Right => d.right() - cur.x,
            };
            d.contains(cur) && distance < settings.tile_pt + crate::warp_delta::DOCK_MARGIN_PT
        })
    });
    if !near_dock {
        if warp_moves_cursor(cur, to) {
            cursor::owner::prepare(s, generation, cur, to);
        }
        return cur;
    }
    // Cache per generation, including an in-flight marker. Repeated Remote requests can resume
    // AppKit preparation without launching duplicate AX queries. Stale results cannot move a cursor.
    static QUERY: Mutex<(u64, Option<(Point, Point)>)> = Mutex::new((0, None));
    let mut query = QUERY.lock().unwrap_or_else(|e| e.into_inner());
    if query.0 == generation {
        if let Some((from, to)) = query.1 {
            cursor::owner::prepare(s, generation, from, to);
        }
    } else {
        *query = (generation, None);
        let result = std::thread::Builder::new()
            .name("oink-dock-frame".into())
            .spawn(move || {
                // A thread spawned by the input tap can inherit its interactive QoS.
                // The slow AX query must not run at that priority.
                // SAFETY: affects only this helper, with a valid class and relative priority.
                let qos = unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_DEFAULT, 0) };
                if qos != 0 {
                    tracing::warn!(error = qos, "cannot restore Dock query default QoS");
                }
                let dock = super::dock_frame::read().map(|frame| crate::warp_delta::DockZone {
                    side: settings.dock.unwrap(),
                    frame,
                    tile_pt: settings.tile_pt,
                });
                let target = clear_of_active_edge_zones(cur, &displays, dock, &settings.corners);
                tracing::debug!(target: "oink_cursor_timing", ?dock, ?cur, ?target, generation, "freeze active zones");
                let mut query = QUERY.lock().unwrap_or_else(|e| e.into_inner());
                if query.0 == generation && warp_moves_cursor(cur, target) {
                    query.1 = Some((cur, target));
                    drop(query);
                    cursor::owner::prepare(s, generation, cur, target);
                }
            });
        if result.is_err() {
            *query = (0, None);
        }
    }
    cur
}

impl Shared {
    fn capture_permitted(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
            && self.safety.snapshot().accepts(crate::session_gate::InputSafety {
                epoch: self.capture_epoch.load(Ordering::Acquire),
                paused: false,
            })
    }
    pub(crate) fn timing_scope(&self) -> Option<TimingToken> {
        let epoch = self.timing_epoch.load(Ordering::Relaxed);
        (epoch != 0).then_some(TimingToken { epoch, sequence: 0 })
    }
    fn owner_current(&self, generation: u64) -> bool {
        generation != 0
            && self.capture_permitted()
            && self.remote.load(Ordering::Acquire)
            && !self.stop.load(Ordering::Acquire)
            && self.owner_generation.load(Ordering::Acquire) == generation
    }

    fn observe_local_pointer<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        // Router focus can return before Show/associate/blank-owner retirement
        // completes. HIDDEN alone is insufficient: restore_native clears it
        // before the synchronous Show call. Sample under the same cursor lock
        // as native transitions, or withhold immediately while it is busy.
        let _guard = self.cursor_lock.try_lock().ok()?;
        if self.remote.load(Ordering::Acquire)
            || self.hold_motion.load(Ordering::Acquire)
            || self.restore_pending.load(Ordering::Acquire)
            || !matches!(
                *self.hidden.lock().unwrap_or_else(|e| e.into_inner()),
                HiddenCursor::Shown
            )
        {
            return None;
        }
        Some(read())
    }

    pub(crate) fn owner_update<T>(&self, generation: u64, f: impl FnOnce() -> T) -> Option<T> {
        let _guard = self.cursor_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.owner_current(generation).then(f)
    }

    pub(crate) fn owner_try_update<T>(&self, generation: u64, f: impl FnOnce() -> T) -> Option<T> {
        let _guard = self.cursor_lock.try_lock().ok()?;
        self.owner_current(generation).then(f)
    }

    pub(crate) fn owner_cleanup(&self, f: impl FnOnce(bool)) {
        let _guard = self.cursor_lock.lock().unwrap_or_else(|e| e.into_inner());
        f(!self.remote.load(Ordering::Acquire) && self.owner_generation.load(Ordering::Acquire) == 0);
    }

    fn owner_posts_snapshot(&self) -> OwnerPosts {
        *self.owner_posts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Caller holds cursor_lock; Local cannot overtake the post registration.
    pub(crate) fn owner_post(&self, generation: u64, to: Point) -> bool {
        {
            let mut posts = self.owner_posts.lock().unwrap_or_else(|e| e.into_inner());
            if posts.count == 0 {
                *posts = OwnerPosts {
                    generation,
                    count: 0,
                    started_ns: now_event_ns(),
                };
            }
            if posts.generation != generation {
                return false;
            }
            posts.count += 1;
        }
        let posted = cursor::post_move(to, cursor::owner::event_tag(generation));
        if !posted {
            self.owner_ack(generation);
        }
        posted
    }

    fn owner_ack(&self, generation: u64) {
        let last = {
            let mut posts = self.owner_posts.lock().unwrap_or_else(|e| e.into_inner());
            if posts.generation != generation || posts.count == 0 {
                return;
            }
            posts.count -= 1;
            posts.count == 0
        };
        if last && !self.remote.load(Ordering::Acquire) {
            self.pending.fetch_or(PENDING_APPLY_MODE, Ordering::AcqRel);
            wake(self);
        }
    }

    pub(crate) fn owner_ready(&self, generation: u64, to: Point) {
        *self.owner_move.lock().unwrap_or_else(|e| e.into_inner()) = Some((generation, to));
        self.pending.fetch_or(PENDING_OWNER_MOVE, Ordering::AcqRel);
        wake(self);
    }
}

fn apply_owner_move(s: &Shared) {
    let Some((generation, to)) = s.owner_move.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    s.owner_update(generation, || {
        if s.owner_post(generation, to) {
            *s.hidden.lock().unwrap_or_else(|e| e.into_inner()) = HiddenCursor::frozen(to);
            tracing::debug!(generation, ?to, "cursor owner: posted hidden departure from edge zone");
        }
    });
}

/// 回到 Local，光标出现在 `at`（`cur`：现在光标所在处）。restore_at 用 warp（不产生事件）把光标放到
/// `at`：系统把它算进下一个移动事件，那个事件的位移另行修正（`crate::warp_delta`）。warp 到光标已经在
/// 的点会被系统忽略、事件位置也不重置，这时先 warp 到旁边再回来（`warp_delta::nudge_point`）。
///
/// **在 warp 与重新关联做完之前产生的移动事件一律吞掉**（`hold_motion` 与 `WarpDeltaFix::predates_warp`）。
/// 实测（macOS，硬件输入 4/21 次、合成输入 1/11 次）：回到本机后光标沿远程期间漂走的
/// 事件位置接着走。原因：远程模式下事件位置（HID 指针位置）按位移累加、早已离开光标；离开 Remote
/// （不再吞掉）到 warp 生效之间，系统按旧位置算好、还在路上的移动事件被放行，而「解耦但不吞掉」的
/// 事件会让光标跟着它走（真机：解耦只有与吞掉一起才能冻结光标）。这个事件若在 warp 之后才被系统
/// 处理，光标与 HID 指针位置就回到了旧位置 + 一步，warp 等于没做；之后的事件从那里接着累加。
/// 所以离开 Remote 时先挡住移动事件，warp 与关联做完后，时间戳早于那一刻的事件继续吞掉。
fn restore_to(s: &Shared, at: Point, cur: Point) {
    let via = (!warp_moves_cursor(cur, at)).then(|| {
        let toward = (*s.park.lock().unwrap_or_else(|e| e.into_inner())).unwrap_or(cur);
        nudge_point(at, toward)
    });
    let at_ns = now_event_ns();
    let done_ns = cursor::restore_at(at, via, &s.warp_fix);
    s.pointer_transition
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mode(at, done_ns);
    s.hold_motion.store(false, Ordering::Release);
    tracing::debug!(?at, ?cur, ?via, at_ns, done_ns, thread = ?std::thread::current().name(), "回到本机：warp 到落点");
}

/// 光标离开冻结的位置超过这个距离（pt）才算冻结失效。
const FREEZE_TOLERANCE_PT: f64 = 2.0;
/// Remote 模式下两次冻结核对之间至少相隔这么久（纳秒）：每次核对是一次发给 WindowServer 的同步调用。
const FREEZE_CHECK_INTERVAL_NS: u64 = 30_000_000;
/// 冻结失效的警告日志最多这么久记一条（其余计数）。
const FREEZE_WARN_EVERY: Duration = Duration::from_secs(5);

/// 光标（`CGEventCreate` 读到的位置）是否离开了冻结的位置。
fn left_frozen(cursor: Point, frozen: Point) -> bool {
    (cursor.x - frozen.x).hypot(cursor.y - frozen.y) > FREEZE_TOLERANCE_PT
}

/// 冻结失效的统计与限频日志。
#[derive(Debug, Default)]
struct FreezeWatch {
    /// 上一条警告之后又发现的次数。
    unreported: u64,
    last_warn: Option<Instant>,
}

impl FreezeWatch {
    /// 记一次冻结失效；需要记警告时返回这段时间内的次数。
    fn failed(&mut self, now: Instant) -> Option<u64> {
        self.unreported += 1;
        if self
            .last_warn
            .is_some_and(|t| now.duration_since(t) < FREEZE_WARN_EVERY)
        {
            return None;
        }
        self.last_warn = Some(now);
        Some(std::mem::take(&mut self.unreported))
    }
}

/// Remote 模式：核对隐藏的光标是否还在冻结的位置。**只检测、记日志，不改变行为**（不重新解耦、不放回）：
/// 冻结点为什么会漂还没查清，事后纠正会掩盖原因。每次发现光标离开了
/// 上一次核对时的位置就记一条警告（限频），带冻结点、上一次与这一次的位置、模式、我们记录的关联状态与
/// 冻结了多久。在截获线程上调用。
fn check_frozen(s: &Shared, watch: &mut FreezeWatch) {
    let _g = s.cursor_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut hidden = s.hidden.lock().unwrap_or_else(|e| e.into_inner());
    let HiddenCursor::Frozen { at, since, seen } = &mut *hidden else {
        return;
    };
    if s.owner_posts_snapshot().count != 0 {
        return;
    }
    let cur = current_location();
    let cur = Point::new(cur.x, cur.y);
    if !left_frozen(cur, *seen) {
        return;
    }
    let prev = std::mem::replace(seen, cur);
    if let Some(count) = watch.failed(Instant::now()) {
        tracing::warn!(
            frozen = ?at,
            previous = ?prev,
            found = ?cur,
            count,
            remote = s.remote.load(Ordering::Acquire),
            dissociated = cursor::dissociated(),
            hidden = cursor::hidden(),
            frozen_for_ms = since.elapsed().as_millis() as u64,
            "remote mode: the hidden cursor moved away from where it was frozen (not corrected)"
        );
    }
}

fn run(
    s: &'static Shared,
    ready: mpsc::Sender<Result<(), InputError>>,
    owner: Option<Box<dyn crate::capture_loop::CaptureLoop>>,
) {
    // Numeric scheduling/status records leave through snapshots; formatting must never
    // run on this realtime capture owner, including inside the unchanged router code.
    let _quiet = owner
        .as_ref()
        .map(|_| tracing::subscriber::set_default(tracing::subscriber::NoSubscriber::default()));
    let _ = oink_foundation::realtime::apply(
        oink_foundation::realtime::ThreadRole::Input,
        oink_foundation::realtime::Tier::Realtime,
    );

    if let Some(owner) = owner {
        run_owned(s, ready, owner);
        return;
    }
    if !s.capture_permitted() {
        let _ = ready.send(Err(InputError::NotStarted));
        return;
    }
    let safety_wake: crate::capture_loop::Wake = Arc::new(move || wake(s));
    s.safety.watch(&safety_wake);
    let user_info = s as *const Shared as *mut c_void;
    // SAFETY: 回调是本模块的 C ABI 函数；user_info 指向 'static 的 Shared。
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            event_mask(),
            Some(callback),
            user_info,
        )
    };
    let Some(port) = port else {
        let _ = ready.send(Err(InputError::PermissionDenied(
            "无法创建事件 tap：需要「辅助功能」与「输入监控」权限".into(),
        )));
        return;
    };
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        let _ = ready.send(Err(InputError::Platform("CFMachPortCreateRunLoopSource 失败".into())));
        return;
    };
    let Some(rl) = CFRunLoop::current() else {
        let _ = ready.send(Err(InputError::Platform("取不到 run loop".into())));
        return;
    };
    // SAFETY: 读取 CoreFoundation 导出的常量。
    let (common, default) = unsafe { (kCFRunLoopCommonModes, kCFRunLoopDefaultMode) };
    // Always retain button-only provenance while capture is installed. Recording
    // and system-state reads remain gated by the flight recorder's enabled flag.
    let hid_observer = button_evidence::install(&rl, s);
    rl.add_source(Some(&source), common);
    CGEvent::tap_enable(&port, true);
    s.port.store(CFRetained::as_ptr(&port).as_ptr(), Ordering::Release);
    s.run_loop.store(CFRetained::as_ptr(&rl).as_ptr(), Ordering::Release);
    s.motion.set_active(s.motion_generation, true);
    seed_ordinary_buttons(s);
    button_evidence::boundary(
        crate::button_diagnostics::Layer::SessionTap,
        CGEvent::tap_is_enabled(&port),
    );
    let pacing = if let Some(owner) = owner {
        match super::loop_timer::Timer::new(rl.clone()) {
            Ok(timer) => {
                crate::capture_loop::install(owner, timer.wake());
                Some(timer)
            }
            Err(error) => {
                CGEvent::tap_enable(&port, false);
                CFMachPort::invalidate(&port);
                s.port.store(std::ptr::null_mut(), Ordering::Release);
                s.run_loop.store(std::ptr::null_mut(), Ordering::Release);
                let _ = ready.send(Err(error));
                return;
            }
        }
    } else {
        None
    };
    if let Some(timer) = &pacing {
        CONTACTS.with(|slot| {
            let capture = super::contacts::Capture::start(&s.remote, timer.wake());
            tracing::info!(
                available = capture.is_some(),
                "built-in contact capture; unavailable uses recognized gestures"
            );
            *slot.borrow_mut() = capture;
        });
    }
    let _ = ready.send(Ok(()));
    tracing::debug!("事件 tap 已启动");

    if let Some(observations) = &s.observations {
        observations.set_port(Some(port.clone()));
    }
    let mut observed = observations::Reader::default();
    let mut last_health = Instant::now();
    let mut last_secure = Instant::now();
    let mut last_diagnostics = Instant::now();
    let mut session_state = super::session::snapshot();
    let mut display_power = crate::display_power::Monitor::new().ok();
    let mut visibility = cursor::Reconcile::default();
    let mut last_visibility = Instant::now();
    let mut lost_ups = LostUps::default();
    let mut freeze_watch = FreezeWatch::default();
    let mut last_freeze_check = Instant::now();
    while !s.stop.load(Ordering::Acquire) || s.owner_posts_snapshot().count != 0 {
        if !s.capture_permitted() {
            break;
        }
        if let Some(timer) = &pacing {
            timer.begin_turn();
            observed.poll(s);
            drain_contacts(s);
            let (due, stopped) = crate::capture_loop::poll(Instant::now());
            if stopped {
                break;
            }
            let contact_due = CONTACTS.with(|slot| slot.borrow().as_ref().and_then(super::contacts::Capture::deadline));
            timer.arm(match (due, contact_due) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            });
        }
        let held = PRESSED.with(|p| !p.borrow().is_empty());
        let timeout = if pacing.is_some() {
            1e20
        } else if held {
            HELD_CHECK_SECS
        } else {
            0.25
        };
        CFRunLoop::run_in_mode(default, timeout, false);
        if pacing.is_some() {
            continue;
        }
        if pacing.is_none() {
            release_lost_ups(s, &mut lost_ups);
        }
        if pacing.is_none() && last_visibility.elapsed() >= Duration::from_millis(250) {
            last_visibility = Instant::now();
            let current_session = super::session::snapshot();
            let session_changed = current_session != session_state;
            session_state = current_session;
            let power_change = display_power.as_mut().and_then(|monitor| monitor.poll());
            let transition = session_changed || power_change.is_some();
            if transition {
                tracing::info!(session_changed, asleep = ?power_change,
                    remote = s.remote.load(Ordering::Acquire), "input session/display transition");
            }
            {
                let _guard = s.cursor_lock.lock().unwrap_or_else(|e| e.into_inner());
                // Local restore may be waiting for owner acknowledgements. Do not renew its hide.
                visibility.check(
                    transition,
                    s.remote.load(Ordering::Acquire) && !s.stop.load(Ordering::Acquire),
                );
            }
            if transition || last_diagnostics.elapsed() >= Duration::from_secs(1) {
                last_diagnostics = Instant::now();
                super::keyboard_diagnostics::report(
                    s.remote.load(Ordering::Acquire),
                    PRESSED.with(|keys| keys.borrow().len()),
                    transition,
                    CGEvent::tap_is_enabled(&port),
                );
            }
        }
        let pending = s.pending.swap(0, Ordering::AcqRel);
        if pending & PENDING_APPLY_MODE != 0
            || (!s.remote.load(Ordering::Acquire) && s.owner_posts_snapshot().count != 0)
        {
            apply_mode(s);
        }
        if pending & PENDING_OWNER_MOVE != 0 {
            apply_owner_move(s);
        }
        // 冻结的光标：有移动事件时（限频）核对，没有事件时（被控的 Mac）也每 0.25 s 核对一次（只记日志）。
        if pacing.is_none()
            && s.remote.load(Ordering::Acquire)
            && (pending & PENDING_FREEZE_CHECK != 0 || last_freeze_check.elapsed() >= Duration::from_millis(250))
        {
            last_freeze_check = Instant::now();
            check_frozen(s, &mut freeze_watch);
        }
        if pending & PENDING_RECONCILE != 0 {
            reconcile_with_hardware(s);
        }
        if pending & (PENDING_CAPS_ENTER | PENDING_CAPS_LEAVE) != 0 {
            reset_caps_gesture(s, "focus handoff");
        }
        if pending & PENDING_CAPS_LEAVE != 0 && !s.keyboard_remote.load(Ordering::Acquire) {
            s.caps_guard.leave_remote();
        }
        if pending & PENDING_CAPS_ENTER != 0 && s.keyboard_remote.load(Ordering::Acquire) {
            s.caps_guard.snapshot_source();
        }
        if s.keyboard_remote.load(Ordering::Acquire) && (pending & PENDING_CAPS_GUARD != 0 || s.caps_guard.active()) {
            s.caps_guard.enforce();
        }
        if pacing.is_none() && last_health.elapsed() >= Duration::from_secs(5) {
            last_health = Instant::now();
            super::physical_motion::report();
            super::keyboard_diagnostics::report(
                s.remote.load(Ordering::Acquire),
                PRESSED.with(|keys| keys.borrow().len()),
                true,
                CGEvent::tap_is_enabled(&port),
            );
            if let Some((observer, _)) = &hid_observer
                && !CGEvent::tap_is_enabled(observer)
            {
                tracing::warn!("button trace: re-enabling diagnostic HID observer");
                CGEvent::tap_enable(observer, true);
                button_evidence::boundary(
                    crate::button_diagnostics::Layer::HidTap,
                    CGEvent::tap_is_enabled(observer),
                );
            }
            if !CGEvent::tap_is_enabled(&port) {
                s.ordinary_buttons
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .invalidate();
                tracing::warn!("事件 tap 被系统停用，重新启用");
                CGEvent::tap_enable(&port, true);
                button_evidence::boundary(
                    crate::button_diagnostics::Layer::SessionTap,
                    CGEvent::tap_is_enabled(&port),
                );
                tracing::info!(
                    enabled = CGEvent::tap_is_enabled(&port),
                    reason = "health check",
                    "event tap recovery attempted"
                );
                reconcile_with_hardware(s);
            }
        }
        if pacing.is_none() && last_secure.elapsed() >= Duration::from_millis(500) {
            last_secure = Instant::now();
            release_keys_if_secure_input(s);
        }
    }

    if let Some(observations) = &s.observations {
        s.stop.store(true, Ordering::Release);
        observations.wake();
        observations.set_port(None);
    }
    if pacing.is_some() {
        CONTACTS.with(|slot| slot.borrow_mut().take());
        crate::capture_loop::clear();
    }
    drop(pacing);
    if let Some(observer) = hid_observer {
        button_evidence::remove(&rl, observer);
    }
    super::physical_motion::report();
    s.gestures
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .boundary("captured", "tap_stopped");
    CGEvent::tap_enable(&port, false);
    s.motion.set_active(s.motion_generation, false);
    button_evidence::boundary(crate::button_diagnostics::Layer::SessionTap, false);
    // 在 Remote 模式下停止：恢复本机的大写锁定与输入法。
    s.caps_guard.leave_remote();
    s.port.store(std::ptr::null_mut(), Ordering::Release);
    s.run_loop.store(std::ptr::null_mut(), Ordering::Release);
    CFMachPort::invalidate(&port);
    let _g = s.cursor_lock.lock().unwrap_or_else(|e| e.into_inner());
    *s.hidden.lock().unwrap_or_else(|e| e.into_inner()) = HiddenCursor::Shown;
    cursor::restore();
    tracing::debug!("事件 tap 已停止");
}

type CapturePort = (CFRetained<CFMachPort>, CFRetained<CFRunLoopSource>);
struct InstalledCapture {
    session: CapturePort,
    observer: Option<CapturePort>,
}

// Keyboard filtering stays at kCGSessionEventTap, never a keyboard HID tap.
// Apple's TN2150 guarantees EnableSecureEventInput withholds keys from event
// interceptors, including event taps; the login/password field cannot be swallowed
// here. The other login session used by fast switching is outside this tap's scope.
// No guard-specific secure-input bypass is needed: the OS withholds those events.
fn install_capture(
    s: &'static Shared,
    run_loop: &CFRunLoop,
    wake: crate::capture_loop::Wake,
) -> Option<InstalledCapture> {
    let safety = s.safety.snapshot();
    if !safety.active() {
        return None;
    }
    s.capture_epoch.store(safety.epoch, Ordering::Release);
    // SAFETY: callback context outlives all taps and is accessed only on this run loop.
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            event_mask(),
            Some(callback),
            s as *const Shared as *mut c_void,
        )
    }?;
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        CFMachPort::invalidate(&port);
        return None;
    };
    // SAFETY: exported CoreFoundation mode constant.
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    CGEvent::tap_enable(&port, true);
    s.port.store(CFRetained::as_ptr(&port).as_ptr(), Ordering::Release);
    s.motion.set_active(s.motion_generation, true);
    seed_ordinary_buttons(s);
    button_evidence::boundary(
        crate::button_diagnostics::Layer::SessionTap,
        CGEvent::tap_is_enabled(&port),
    );
    if let Some(observations) = &s.observations {
        observations.set_port(Some(port.clone()));
    }
    let observer = button_evidence::install(run_loop, s);
    // MultitouchSupport is a separate subscription from the Quartz event tap.
    // macOS 26.6.2 can deliver recognized gestures with no native contact frames
    // unless we start that subscription on the actual capture owner as well.
    // Tie it to capture admission, so pause stops raw
    // sampling too. The run loop drains its wakeups even without a Quartz event;
    // native contacts and recognized gestures remain independent source streams.
    CONTACTS.with(|slot| {
        *slot.borrow_mut() = super::contacts::Capture::start(&s.remote, wake);
    });
    Some(InstalledCapture {
        session: (port, source),
        observer,
    })
}
fn withdraw_capture(s: &Shared, run_loop: &CFRunLoop, capture: InstalledCapture) {
    if let Some(observations) = &s.observations {
        observations.set_port(None);
    }
    s.port.store(std::ptr::null_mut(), Ordering::Release);
    CGEvent::tap_enable(&capture.session.0, false);
    // SAFETY: exported CoreFoundation mode constant; the source belongs to this loop.
    run_loop.remove_source(Some(&capture.session.1), unsafe { kCFRunLoopCommonModes });
    CFMachPort::invalidate(&capture.session.0);
    s.motion.set_active(s.motion_generation, false);
    if let Some(observer) = capture.observer {
        button_evidence::remove(run_loop, observer);
    }
    CONTACTS.with(|slot| slot.borrow_mut().take());
    button_evidence::boundary(crate::button_diagnostics::Layer::SessionTap, false);
    PRESSED.with(|keys| keys.borrow_mut().clear());
    BUTTONS.with(|buttons| buttons.borrow_mut().clear());
    KEY_DISPOSITION.with(|keys| keys.borrow_mut().clear());
    s.physical_buttons.reset();
    s.ordinary_buttons
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .invalidate();
    super::physical_motion::report();
    reset_caps_gesture(s, "capture withdrawn");
    s.remote.store(false, Ordering::Release);
    s.keyboard_remote.store(false, Ordering::Release);
    s.restore_pending.store(false, Ordering::Release);
    s.hold_motion.store(false, Ordering::Release);
    s.owner_generation.store(0, Ordering::Release);
    s.owner_posts.lock().unwrap_or_else(|e| e.into_inner()).count = 0;
    s.owner_move.lock().unwrap_or_else(|e| e.into_inner()).take();
    s.drag_anchor.lock().unwrap_or_else(|e| e.into_inner()).take();
    s.pending
        .fetch_or(PENDING_APPLY_MODE | PENDING_CAPS_LEAVE, Ordering::AcqRel);
    if let Some(observations) = &s.observations {
        observations.wake();
    }
}
fn run_owned(
    s: &'static Shared,
    ready: mpsc::Sender<Result<(), InputError>>,
    owner: Box<dyn crate::capture_loop::CaptureLoop>,
) {
    let Some(run_loop) = CFRunLoop::current() else {
        let _ = ready.send(Err(InputError::Platform("cannot obtain input run loop".into())));
        return;
    };
    let timer = match super::loop_timer::Timer::new(run_loop.clone()) {
        Ok(timer) => timer,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    crate::capture_loop::install(owner, timer.wake());
    let native_wake = timer.wake();
    let safety_wake: crate::capture_loop::Wake = Arc::new(move || {
        if !s.capture_permitted() {
            // Wake a pending 250 ms injection ACK immediately. This only takes
            // the delivery ledger lock; it makes no WindowServer call.
            s.motion.set_active(s.motion_generation, false);
        }
        native_wake();
    });
    s.safety.watch(&safety_wake);
    s.run_loop
        .store(CFRetained::as_ptr(&run_loop).as_ptr(), Ordering::Release);
    // Permission absence affects capture, never the inbound reader, pacing timer or startup.
    let _ = ready.send(Ok(()));
    let mut capture = None;
    let mut retry = Instant::now();
    let mut observed = observations::Reader::default();
    let mut resume_hotkey = super::resume_hotkey::Manager::default();
    while !s.stop.load(Ordering::Acquire) {
        timer.begin_turn();
        if !s.capture_permitted()
            && let Some(capture) = capture.take()
        {
            // Remove the active tap before router cleanup or any native posting.
            // A preflight on another thread can withdraw rights while this loop
            // is asleep; callbacks already pass through using the same atomic gate.
            withdraw_capture(s, &run_loop, capture);
            crate::capture_loop::capture_available(false);
        }
        observed.poll(s);
        drain_contacts(s);
        let now = Instant::now();
        let (mut due, stopped) = crate::capture_loop::poll(now);
        if stopped {
            break;
        }
        resume_hotkey.update(crate::capture_loop::paused_hotkey(), timer.wake());
        if resume_hotkey.take_fired() {
            crate::capture_loop::resume_hotkey();
        }
        if s.safety.snapshot().active() && crate::capture_loop::capture_requested() {
            if capture.is_none() && now >= retry {
                capture = install_capture(s, &run_loop, timer.wake());
                crate::capture_loop::capture_available(capture.is_some());
                retry = now + Duration::from_secs(2);
            }
            if capture.is_none() {
                due = Some(due.map_or(retry, |due| due.min(retry)));
            }
        } else {
            retry = now;
            if let Some(capture) = capture.take() {
                withdraw_capture(s, &run_loop, capture);
            }
        }
        let contact_due = CONTACTS.with(|slot| slot.borrow().as_ref().and_then(super::contacts::Capture::deadline));
        timer.arm(match (due, contact_due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        });
        // SAFETY: exported CoreFoundation mode constant.
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 1e20, false);
    }
    if let Some(capture) = capture {
        withdraw_capture(s, &run_loop, capture);
    }
    s.stop.store(true, Ordering::Release);
    if let Some(observations) = &s.observations {
        observations.wake();
    }
    crate::capture_loop::clear();
    s.run_loop.store(std::ptr::null_mut(), Ordering::Release);
    s.caps_guard.leave_remote();
    let _guard = s.cursor_lock.lock().unwrap_or_else(|e| e.into_inner());
    *s.hidden.lock().unwrap_or_else(|e| e.into_inner()) = HiddenCursor::Shown;
    cursor::restore();
}

/// 按着键期间：给物理上已经松开、但抬起没有交给 tap 的键补报抬起（见模块文档）。在截获线程上调用。
fn release_lost_ups(s: &Shared, check: &mut LostUps) {
    if PRESSED.with(|p| p.borrow().is_empty()) {
        check.clear();
        return;
    }
    let up = PRESSED.with(|p| released(p.borrow().iter().copied(), key_physically_down));
    let lost = check.confirm(&up, Instant::now());
    if lost.is_empty() {
        return;
    }
    tracing::info!(count = lost.len(), "按键抬起没有交给 tap（物理上已松开）：补报抬起");
    for code in lost {
        PRESSED.with(|p| p.borrow_mut().remove(&code));
        if let Some(k) = disp_key(code) {
            KEY_DISPOSITION.with(|d| d.borrow_mut().remove(&k));
        }
        release_key(s, code);
    }
}

/// 安全输入开启后 tap 收不到按键：给截获到仍按着的键补发抬起。
fn release_keys_if_secure_input(s: &Shared) {
    let has_pressed = PRESSED.with(|p| !p.borrow().is_empty());
    if !has_pressed || !(super::session::locked() == Some(true) || system::secure_input_active()) {
        return;
    }
    let keys: Vec<KeyCode> = PRESSED.with(|p| p.borrow_mut().drain().collect());
    KEY_DISPOSITION.with(|d| d.borrow_mut().clear());
    tracing::info!(count = keys.len(), "安全输入已开启，补发抬键");
    for code in keys {
        release_key(s, code);
    }
}

fn release_key(s: &Shared, code: KeyCode) {
    let ev = CapturedEvent {
        physical: false,
        event: InputEvent::Key { code, pressed: false },
        source: SourceKind::Keyboard,
        time_us: now_us(),
        cursor: None,
    };
    crate::diagnostics::with_capture(
        crate::diagnostics::CaptureMetadata {
            backend_generated: Some(true),
            repeat: Some(false),
            ..Default::default()
        },
        || call_handler(s, &ev),
    );
}

/// Quartz exposes the Caps *lock*, not a reliable physical hold snapshot. A stream gap
/// therefore ends the old gesture instead of seeding its phase from that lock bit.
/// The next NX Caps event carries its own direction, including an orphan UP. This
/// runs on the tap thread, as do PRESSED/KEY_DISPOSITION and ordinary lost-UP cleanup.
fn reset_caps_gesture(s: &Shared, reason: &str) {
    if forget_caps_gesture() {
        tracing::debug!(reason, "Caps Lock capture discontinuity: releasing old gesture");
        release_key(s, KeyCode::keyboard(0x39));
    }
}

fn forget_caps_gesture() -> bool {
    KEY_DISPOSITION.with(|d| {
        let mut d = d.borrow_mut();
        d.remove(&keymap::MAC_CAPS_LOCK);
        d.remove(&(AUX_DISP | 4));
    });
    PRESSED.with(|p| p.borrow_mut().remove(&KeyCode::keyboard(0x39)))
}

/// 已记录为按下、但 `is_down` 明确说已经松开（`Some(false)`）的那些。`None`（查不到、或不适合核对）保留。
fn released<T: Copy>(held: impl IntoIterator<Item = T>, is_down: impl Fn(T) -> Option<bool>) -> Vec<T> {
    held.into_iter().filter(|&x| is_down(x) == Some(false)).collect()
}

/// 键的物理状态（HID 系统状态）。Caps Lock 按锁定状态上报，不核对；媒体键与新款键盘的功能键（Consumer
/// 等页，系统不按 kVK 记录状态）不核对。
pub(super) fn held_session_keys() -> Vec<KeyCode> {
    // This bitmap can include injected or stale ordinary keys; it is exclusively
    // release cleanup, never a source of physical presses or chord identities.
    (0..128u16)
        .filter_map(keymap::mac_to_hid)
        .filter(|code| key_physically_down(*code) == Some(true))
        .collect()
}

fn key_physically_down(code: KeyCode) -> Option<bool> {
    if code.page() != KeyCode::PAGE_KEYBOARD && code != keymap::APPLE_FN {
        return None;
    }
    let mac = keymap::hid_to_mac(code)?;
    mac_key_physically_down(
        mac,
        || CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState),
        || CGEventSource::key_state(CGEventSourceStateID::HIDSystemState, mac),
    )
}

fn mac_key_physically_down(
    mac: u16,
    flags: impl FnOnce() -> CGEventFlags,
    key_state: impl FnOnce() -> bool,
) -> Option<bool> {
    if mac == keymap::MAC_CAPS_LOCK {
        return None;
    }
    if modifier_flags(mac).is_some() {
        // CGEventSourceKeyState(HID, kVK_RightShift) stays false while HID flags
        // carry both Shift and its right-device bit (macOS 26.6.2, physical failure
        // logs and native FlagsChanged reproduction). Treating that bitmap as
        // physical state sent an early UP after the first letter. Read the HID
        // modifier flags instead, preserving left/right distinctions; Session
        // flags are intentionally cleared when our source tap swallows input.
        // Ordinary keys retain the bitmap check, Caps retains its explicit NX
        // phases, and target key submission/secure-input ACK policy is unchanged.
        return modifier_pressed(mac, flags());
    }
    Some(key_state())
}

/// Once per installation, with the tap already installed but before its run
/// loop dispatches. No WindowServer queries on the motion callback. Replaying
/// queued transitions from `start` closes the non-atomic 32-button scan window;
/// pre-`end` motion stays legacy. A later capture gap stays unknown until install.
fn seed_ordinary_buttons(s: &Shared) {
    super::physical_motion::initialize();
    let start = now_event_ns();
    let mut mask = 0u32;
    for button in 0..32 {
        if CGEventSource::button_state(CGEventSourceStateID::HIDSystemState, CGMouseButton(button)) {
            mask |= 1u32 << button;
        }
    }
    let end = now_event_ns();
    s.ordinary_buttons
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .seed(mask, start, end);
}

fn button_physically_down(b: MouseButton) -> Option<bool> {
    Some(CGEventSource::button_state(
        CGEventSourceStateID::HIDSystemState,
        CGMouseButton(button_number(b)),
    ))
}

/// tap 重新启用后：给停用期间已经松开的键和按钮补报抬起（见模块文档）。在截获线程上调用。
fn reconcile_with_hardware(s: &Shared) {
    reset_caps_gesture(s, "tap recovery");
    let keys = PRESSED.with(|p| released(p.borrow().iter().copied(), key_physically_down));
    let buttons = BUTTONS.with(|b| released(b.borrow().iter().copied(), button_physically_down));
    if keys.is_empty() && buttons.is_empty() {
        tracing::debug!("tap 恢复：按键与按钮状态一致");
        return;
    }
    tracing::info!(
        key_count = keys.len(),
        button_count = buttons.len(),
        "tap 恢复：补报停用期间丢失的抬起"
    );
    for code in keys {
        PRESSED.with(|p| p.borrow_mut().remove(&code));
        if let Some(k) = disp_key(code) {
            KEY_DISPOSITION.with(|d| d.borrow_mut().remove(&k));
        }
        release_key(s, code);
    }
    let p = current_location();
    for button in buttons {
        BUTTONS.with(|b| b.borrow_mut().remove(&button));
        s.physical_buttons.record(button_number(button), false);
        let ev = CapturedEvent {
            physical: false,
            event: InputEvent::Button { button, pressed: false },
            source: SourceKind::Unknown,
            time_us: now_us(),
            cursor: Some(Point::new(p.x, p.y)),
        };
        call_handler(s, &ev);
    }
}

fn call_handler(s: &Shared, ev: &CapturedEvent) -> Disposition {
    IN_CALLBACK.with(|c| c.set(true));
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| (s.handler)(ev))).unwrap_or_else(|_| {
        tracing::error!("截获回调 panic，事件放行");
        Disposition::Pass
    });
    IN_CALLBACK.with(|c| c.set(false));
    r
}

// Keyboard side effects must never infer their destination from cursor hiding:
// mouse-local/keyboard-shared and mouse-shared/keyboard-local are both valid.
fn native_swallow(blocked: bool, routed: bool, native_caps: bool, caps_flags: bool, keyboard_remote: bool) -> bool {
    blocked || (routed && !native_caps) || (keyboard_remote && caps_flags)
}

/// 翻译结果。
struct Translated {
    event: Option<CapturedEvent>,
    /// 指针类事件（移动、按钮、滚动、手势）：Remote 模式下一律吞掉。
    pointer: bool,
    /// 没见过按下的键的自动重复：不上报，沿用按下时的处理结果（没有记录时放行）。
    repeat_of: Option<u16>,
    /// 非自动重复的按下，而这个键仍记为按着：上一次的抬起没交给我们，先补报抬起。
    release_first: Option<KeyCode>,
    /// 按键事件在 `KEY_DISPOSITION` 里的键（[`disp_key`]）。
    disp_key: Option<u16>,
}

fn image_nudge_location(
    anchor: Option<oink_foundation::dnd::DragAnchor>,
    generation: u64,
    frozen: Point,
    current: Point,
) -> Option<Point> {
    let anchor = anchor.filter(|a| a.generation == generation)?;
    let q = super::inject::quartz_point(current);
    let expected = super::inject::quartz_point(frozen);
    let held = super::inject::quartz_point(anchor.at);
    (q == expected && q == held).then_some(Point::new(q.x, q.y))
}

fn without_blocked_pointer_modifiers(flags: CGEventFlags, blocked: &[KeyCode]) -> CGEventFlags {
    let mask = blocked
        .iter()
        .filter_map(|code| keymap::hid_to_mac(*code))
        .filter_map(modifier_flags)
        .fold(0, |mask, (aggregate, side)| mask | aggregate.0 | side);
    CGEventFlags(flags.0 & !mask)
}

fn marked_own_event(ev: &CGEvent) -> bool {
    let tag = CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData);
    tag == MAGIC || cursor::owner::event_generation(tag).is_some()
}

fn physical_keyboard_event(ty: CGEventType, ev: &CGEvent) -> bool {
    !marked_own_event(ev)
        && CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID) == 0
        && (matches!(
            ty,
            CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged
        ) || (ty.0 == sysevent::TYPE_SYSDEFINED && sysevent::read_aux_key(ev).is_some()))
}

unsafe extern "C-unwind" fn callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    let _timing = crate::timing::CaptureTimer::new();
    // SAFETY: user_info 指向 'static 的 Shared；event 在回调期间有效。
    let (s, ev) = unsafe { (&*(user_info as *const Shared), event.as_ref()) };
    let entry = now_event_ns() / 1000;
    let pass = event.as_ptr();
    if !s.capture_permitted() {
        wake(s);
        return pass;
    }
    button_evidence::observe(crate::button_diagnostics::Layer::SessionTap, ty, ev);

    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        s.ordinary_buttons
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate();
        button_evidence::boundary(crate::button_diagnostics::Layer::SessionTap, false);
        s.gestures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .boundary("captured", "tap_disabled");
        let port = s.port.load(Ordering::Acquire);
        if !port.is_null() {
            tracing::warn!(?ty, "事件 tap 被停用，立即重新启用");
            // SAFETY: port 在截获线程退出前有效。
            CGEvent::tap_enable(unsafe { &*port }, true);
            // SAFETY: same live port as the enable call above, owned by this thread.
            let enabled = CGEvent::tap_is_enabled(unsafe { &*port });
            button_evidence::boundary(crate::button_diagnostics::Layer::SessionTap, enabled);
            tracing::info!(enabled, reason = ?ty, "event tap recovery attempted");
            // 核对放到回调之外（截获线程的循环里）做。
            s.pending.fetch_or(PENDING_RECONCILE, Ordering::AcqRel);
            if let Some(rl) = CFRunLoop::current() {
                rl.stop();
            }
        }
        return pass;
    }
    let stamp = CGEvent::timestamp(Some(ev)) / 1000;
    let physical = CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID) == 0;
    // A posting process can set its CGEvent timestamp. Only native timestamps
    // use the OS capture instant; synthetic/invalid ones use tap entry in the
    // same uptime clock. CapturedEvent.time_us remains unchanged for contacts.
    let _source_time = crate::source_time::CaptureScope::new(crate::source_time::CaptureTime::from_micros(
        if physical && stamp != 0 && stamp <= entry {
            stamp
        } else {
            entry
        },
    ));
    // 在注入标记过滤之前记录，才能区分系统没送到 tap 与收到后被当作自己注入而跳过。
    let left_button = matches!(ty, CGEventType::LeftMouseDown | CGEventType::LeftMouseUp);
    if left_button {
        let user_data = CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData);
        let at = CGEvent::location(Some(ev));
        tracing::debug!(
            ?ty,
            source_state = CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceStateID),
            source_pid = CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID),
            user_data,
            own = user_data == MAGIC,
            click_state = CGEvent::integer_value_field(Some(ev), CGEventField::MouseEventClickState),
            timestamp_ns = CGEvent::timestamp(Some(ev)),
            x = at.x,
            y = at.y,
            remote = s.remote.load(Ordering::Acquire),
            "button trace: raw left event at session tap"
        );
    }
    let user_data = CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData);
    if matches!(
        ty,
        CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged
    ) {
        super::keyboard_diagnostics::captured(user_data == MAGIC, s.remote.load(Ordering::Acquire));
        if ty == CGEventType::FlagsChanged {
            tracing::info!(
                own = user_data == MAGIC,
                physical = CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID) == 0,
                remote = s.remote.load(Ordering::Acquire),
                modifiers = super::keyboard_diagnostics::modifiers(CGEvent::flags(Some(ev)).0),
                "modifier event observed at session tap"
            );
        }
    }
    if let Some(generation) = cursor::owner::event_generation(user_data) {
        s.owner_ack(generation);
        if !s.owner_current(generation) {
            tracing::debug!(generation, "cursor owner: discarded stale move");
            return std::ptr::null_mut();
        }
        if is_motion(ty) {
            cursor::owner::event_consumed(s, generation);
            account_cursor_placement(s, ev, crate::pointer_transition::PlacementKind::Owner, None);
        }
        return pass;
    }
    // 普通注入事件。
    if marked_own_event(ev) {
        // Injected events already have their destination keycode. Only replace
        // this Mac's still-held physical modifier contribution in their flags;
        // preserve the injected contributions recorded by the router.
        // System media keys carry their source modifier bitmap explicitly.
        // Replacing it with this Mac's modifier ledger loses Option+Shift fine
        // steps when the exit has no keyboard focus. Tagged events still pass
        // before capture/routing, so owner-applied volume cannot loop back.
        let (mapping, injected) = crate::capture_loop::local_modifier_mapping();
        CGEvent::set_flags(Some(ev), transformed_injected_flags(ev, &mapping, &injected));
        trace_gesture_delivery(ty, ev, "injected_echo_pass", None, s.remote.load(Ordering::Acquire));
        s.motion.acknowledge_cg(s.motion_generation, ty, ev);
        if is_motion(ty) || is_mouse_button(ty) {
            account_cursor_placement(s, ev, crate::pointer_transition::PlacementKind::Tagged, None);
        }
        return pass;
    }

    if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID) == 0
        && (is_motion(ty) || is_mouse_button(ty) || ty == CGEventType::ScrollWheel)
    {
        // Quartz copies hardware modifier flags onto physical pointer events even
        // when the keyboard tap swallowed FlagsChanged. Strip only blocked own
        // modifiers; the router excludes modifiers still held by injected sources.
        let blocked = crate::capture_loop::keyboard_pointer_modifiers();
        if !blocked.is_empty() {
            CGEvent::set_flags(
                Some(ev),
                without_blocked_pointer_modifiers(CGEvent::flags(Some(ev)), &blocked),
            );
        }
    }

    // Native buttons need the OS activation path (keyboard focus / TSM), so do
    // not tag or rewrite their location. Only a matching submission receives
    // semantic flags. Own late UPs still pass: dropping one can latch a drag.
    // This precedes physical-button bookkeeping, routing and motion warp repair.
    if s.motion.finish_button(s.motion_generation, ty, ev) {
        account_cursor_placement(s, ev, crate::pointer_transition::PlacementKind::NativeButton, None);
        return pass;
    }

    // HID is the actual remote motion, not a second visibility companion. Keep
    // its location: even a tiny correction can latch typing obscure. Populate
    // raw delta/flags/tag, acknowledge through a separate short lock and bypass
    // physical routing. One in-flight post + its timestamp rejects late echoes;
    // hardware PID=0 and other processes cannot consume pending metadata.
    // No unhide/show call: games retain NSCursor.hide counts. macOS 14.7.8 /
    // 26.6.2 native typing tests verified this path.
    let native_delta = is_motion(ty).then(|| {
        (
            CGEvent::double_value_field(Some(ev), CGEventField::MouseEventDeltaX),
            CGEvent::double_value_field(Some(ev), CGEventField::MouseEventDeltaY),
        )
    });
    if let Some(accepted) = s.motion.finish_mouse(s.motion_generation, ty, ev) {
        if !accepted {
            return std::ptr::null_mut();
        }
        account_cursor_placement(
            s,
            ev,
            crate::pointer_transition::PlacementKind::NativeMotion,
            native_delta,
        );
        return pass;
    }

    if is_motion(ty)
        || matches!(
            ty,
            CGEventType::LeftMouseDown
                | CGEventType::LeftMouseUp
                | CGEventType::RightMouseDown
                | CGEventType::RightMouseUp
                | CGEventType::OtherMouseDown
                | CGEventType::OtherMouseUp
        )
    {
        s.motion.invalidate_position();
    }

    // Record genuine device transitions before routing can synchronously request a
    // release (e.g. repeated-down recovery). Own and other-process injection never
    // changes this ledger, even when it changes WindowServer's HID state.
    if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID) == 0 {
        let transition = match ty {
            CGEventType::LeftMouseDown => Some((0, true)),
            CGEventType::LeftMouseUp => Some((0, false)),
            CGEventType::RightMouseDown => Some((1, true)),
            CGEventType::RightMouseUp => Some((1, false)),
            CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => Some((
                CGEvent::integer_value_field(Some(ev), CGEventField::MouseEventButtonNumber) as u32,
                ty == CGEventType::OtherMouseDown,
            )),
            _ => None,
        };
        if let Some((button, pressed)) = transition {
            s.physical_buttons.record(button, pressed);
            let mut gate = s.ordinary_buttons.lock().unwrap_or_else(|e| e.into_inner());
            if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceStateID)
                == CGEventSourceStateID::HIDSystemState.0 as i64
            {
                gate.transition(button, pressed, CGEvent::timestamp(Some(ev)));
            } else {
                gate.invalidate();
            }
        }
    }
    let remote = s.remote.load(Ordering::Acquire);
    if !remote
        && cursor::owner::blocks_local_mouse()
        && (is_motion(ty)
            || matches!(
                ty,
                CGEventType::LeftMouseDown
                    | CGEventType::LeftMouseUp
                    | CGEventType::RightMouseDown
                    | CGEventType::RightMouseUp
                    | CGEventType::OtherMouseDown
                    | CGEventType::OtherMouseUp
            )
            || ty == CGEventType::ScrollWheel)
    {
        return std::ptr::null_mut();
    }
    if remote && is_motion(ty) && s.hold_motion.load(Ordering::Acquire) {
        return std::ptr::null_mut();
    }
    if !remote && is_motion(ty) {
        // 回到本机的 warp 与关联做完之前产生的移动：吞掉，不交给路由（见 `restore_to`）。
        let ts = CGEvent::timestamp(Some(ev));
        if s.hold_motion.load(Ordering::Acquire)
            || s.warp_fix.lock().unwrap_or_else(|e| e.into_inner()).predates_warp(ts)
        {
            let loc = CGEvent::location(Some(ev));
            tracing::debug!(ts, x = loc.x, y = loc.y, "回到本机的 warp 之前产生的移动：吞掉");
            return std::ptr::null_mut();
        }
    }
    let mut t = std::panic::catch_unwind(AssertUnwindSafe(|| translate(ty, ev))).unwrap_or(Translated {
        event: None,
        pointer: false,
        repeat_of: None,
        release_first: None,
        disp_key: None,
    });
    if let Some(CapturedEvent {
        event: InputEvent::PointerDelta { dx, dy },
        cursor: Some(at),
        ..
    }) = &mut t.event
    {
        // Keep the complete legacy stream, including diagnostics and pending
        // consumption. Skipping its ordinary calls creates a new press/drag seam.
        // Only routing output for a proven ordinary frame is replaced below.
        let ts = match CGEvent::timestamp(Some(ev)) {
            0 => u64::MAX,
            t => t,
        };
        let mut fix = s.warp_fix.lock().unwrap_or_else(|e| e.into_inner());
        let raw = (*dx, *dy);
        // 其他程序用 CGEventPost 投递的移动（事件源为会话状态）自带位移字段，系统不把 warp / 投递的移动算进去
        // （实测 loopback 的合成移动）：冻结时那次移动只从 HID 层的事件里减——硬件的，以及 `IOHIDPostEvent`
        // 投递的（来源进程号不是 0，但与硬件一样经 HID 系统，实测同样含这次移动）。
        let source_state = CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceStateID);
        let hardware = source_state == CGEventSourceStateID::HIDSystemState.0 as i64;
        let before = fix.snapshot();
        let frame = crate::ordinary_motion::Frame {
            physical,
            ordinary: ty == CGEventType::MouseMoved,
            buttons: s
                .ordinary_buttons
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .state(CGEvent::timestamp(Some(ev))),
        };
        let decision = crate::ordinary_motion::motion(&mut fix, raw, *at, ts, hardware, frame, || {
            super::physical_motion::read(ev)
        });
        // This observation still describes the shadow ledger, just as before A.
        // PointerAuthority interval counters separately describe routed selection.
        let (fx, fy) = decision.ledger_delta;
        super::physical_motion::count(decision.reason);
        if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID) == 0
            && let Some(observation) = s.pointer_transition.lock().unwrap_or_else(|e| e.into_inner()).resumed(
                crate::pointer_transition::Observation {
                    remote,
                    mode_cursor: None,
                    mode_timestamp_ns: 0,
                    placements: 0,
                    last_placement: None,
                    event_position: *at,
                    event_timestamp_ns: ts,
                    source_state,
                    raw_delta: raw,
                    corrected_delta: (fx, fy),
                    before,
                    after: fix.snapshot(),
                },
            )
        {
            crate::native_lifecycle::emit_pointer_transition(observation);
        }
        if (fx, fy) != raw {
            tracing::debug!(?raw, fixed = ?(fx, fy), "修正 warp 之后的位移");
        }
        if let Some(leak) = fix.take_leak() {
            // 只记录：冻结时投递的移动没有被干净地减掉，发给远端的位移可能含它（不变量：不能含）。
            tracing::warn!(
                ?leak,
                "remote mode: the freeze move may have leaked into the delta sent to the remote"
            );
        }
        if let Some(entry) = fix.take_mismatch() {
            // 只记录，不改变行为：回到本机后事件位置没有从进入点出发。
            tracing::warn!(?entry, loc = ?*at, ?raw, ts, "回到本机后光标没有从进入点出发");
        }
        (*dx, *dy) = decision.routed;
    }
    if remote && let Some(e) = &mut t.event {
        // Remote：光标冻结不动、事件位置也不是光标位置，不上报（接口约定）。
        e.cursor = None;
    }

    if remote && is_motion(ty) {
        // 解耦后光标应冻结不动。事件位置不能说明这一点（它按位移累加、会卡住，见 `crate::warp_delta`）：
        // 限频安排截获线程读取光标位置核对（回调里不能调用 WindowServer）。
        let now = now_event_ns();
        let last = s.freeze_check_ns.load(Ordering::Acquire);
        if now.saturating_sub(last) >= FREEZE_CHECK_INTERVAL_NS {
            s.freeze_check_ns.store(now, Ordering::Release);
            s.pending.fetch_or(PENDING_FREEZE_CHECK, Ordering::AcqRel);
        }
    }

    drain_contacts(s);
    // CG can carry contact Ended and MomentumBegan in the same event. Preserve
    // the contact end before map_phase gives priority to momentum; delta goes once.
    if ty == CGEventType::ScrollWheel {
        let contact = CGEvent::integer_value_field(Some(ev), CGEventField::ScrollWheelEventScrollPhase);
        let momentum = CGEvent::integer_value_field(Some(ev), CGEventField::ScrollWheelEventMomentumPhase);
        if momentum != 0 && matches!(contact, 4 | 8) {
            call_handler(
                s,
                &CapturedEvent {
                    physical: false,
                    source: SourceKind::Touchpad,
                    cursor: None,
                    time_us: CGEvent::timestamp(Some(ev)) / 1_000,
                    event: InputEvent::Scroll {
                        dx: 0.,
                        dy: 0.,
                        unit: ScrollUnit::Pixel,
                        phase: if contact == 4 {
                            ScrollPhase::Ended
                        } else {
                            ScrollPhase::Cancelled
                        },
                    },
                },
            );
        }
    }
    if remote && is_motion(ty) {
        let raw = CapturedEvent {
            physical: CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID) == 0,
            event: InputEvent::RawPointerDelta {
                dx: CGEvent::double_value_field(Some(ev), CGEventField::EventUnacceleratedPointerMovementX),
                dy: CGEvent::double_value_field(Some(ev), CGEventField::EventUnacceleratedPointerMovementY),
            },
            source: pointer_source(ev),
            time_us: now_us(),
            cursor: None,
        };
        call_handler(s, &raw);
    }
    if t.event.is_none() && crate::capture_loop::keyboard_disabled() {
        let key = if matches!(ty, CGEventType::KeyDown | CGEventType::KeyUp) {
            let mac = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode) as u16;
            Some((
                keymap::mac_to_hid(mac).unwrap_or_else(|| KeyCode::new(0xFFAB, mac)),
                ty == CGEventType::KeyDown,
            ))
        } else if ty.0 == sysevent::TYPE_SYSDEFINED {
            sysevent::read_aux_key(ev).map(|k| {
                (
                    keymap::nx_to_hid(k.key).unwrap_or_else(|| KeyCode::new(0xFFAA, k.key as u16)),
                    k.pressed,
                )
            })
        } else {
            None
        };
        if let Some((code, pressed)) = key {
            t.repeat_of = None;
            t.event = Some(CapturedEvent {
                physical: CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID) == 0,
                source: SourceKind::Keyboard,
                cursor: None,
                time_us: now_us(),
                event: InputEvent::Key { code, pressed },
            });
        }
    }
    let key = t.event.as_ref().and_then(|e| {
        if let InputEvent::Key { code, .. } = e.event {
            Some(code)
        } else {
            None
        }
    });
    let keyboard_blocked = crate::capture_loop::keyboard_key_blocked(key) && physical_keyboard_event(ty, ev);
    let disposition = if let Some(code) = t.repeat_of {
        KEY_DISPOSITION.with(|d| d.borrow().get(&code).copied().unwrap_or(Disposition::Pass))
    } else if let Some(captured) = &t.event {
        if let Some(code) = t.release_first {
            tracing::warn!("key pressed again without release: reporting the lost key-up first");
            PRESSED.with(|p| p.borrow_mut().remove(&code));
            release_key(s, code);
        }
        track_keys(captured, t.disp_key);
        // Carry only diagnostic context through the synchronous callback. This
        // does not alter the routed event or the backend's recovery ordering.
        let metadata = key_capture_metadata(ty, ev, t.release_first.is_some());
        let d = crate::diagnostics::with_capture(metadata, || call_handler(s, captured));
        if let (InputEvent::Key { pressed: true, .. }, Some(k)) = (&captured.event, t.disp_key) {
            KEY_DISPOSITION.with(|m| m.borrow_mut().insert(k, d));
        }
        d
    } else {
        Disposition::Pass
    };

    let routed_swallow = (remote && t.pointer) || disposition == Disposition::Swallow;
    // Keep the existing local HIToolbox path and LocalGuard behavior. Only the
    // capture source changes: forwarding NX Caps must not also forward FlagsChanged.
    let native_caps = ty.0 == sysevent::TYPE_SYSDEFINED && sysevent::read_aux_key(ev).is_some_and(|key| key.key == 4);
    let caps_flags = ty == CGEventType::FlagsChanged
        && CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode)
            == i64::from(keymap::MAC_CAPS_LOCK);
    // Caps/NX events normally retain the local HIToolbox path. The keyboard guard
    // must override that exception, including untranslated keyboard/media keys.
    let keyboard_remote = s.keyboard_remote.load(Ordering::Acquire);
    let swallow = native_swallow(
        keyboard_blocked,
        routed_swallow,
        native_caps,
        caps_flags,
        keyboard_remote,
    );
    if left_button {
        tracing::debug!(
            ?ty,
            ?disposition,
            swallow,
            remote,
            translated = t.event.is_some(),
            timestamp_ns = CGEvent::timestamp(Some(ev)),
            "button trace: left event after routing"
        );
    }
    if remote && is_motion(ty) && !swallow {
        // 不变量：冻结期间（远程模式）绝不把移动事件放行给系统。只记录。
        let n = s.passed_while_frozen.fetch_add(1, Ordering::AcqRel) + 1;
        if n == 1 || n.is_power_of_two() {
            tracing::warn!(
                n,
                ?ty,
                "remote mode: a motion event was passed to the system while frozen"
            );
        }
    }
    if keyboard_remote && ((routed_swallow && native_caps) || (swallow && caps_flags)) {
        // HID 层可能已经改了本机的锁定状态（事件到达 tap 之前）：推迟到回调之外核对并恢复。
        s.caps_guard.caps_swallowed();
        s.pending.fetch_or(PENDING_CAPS_GUARD, Ordering::AcqRel);
    }

    if s.pending.load(Ordering::Acquire) != 0 || s.stop.load(Ordering::Acquire) {
        // 让 run_in_mode 尽快返回，去执行推迟的操作。
        if let Some(rl) = CFRunLoop::current() {
            rl.stop();
        }
    }

    if matches!(
        ty,
        CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged
    ) {
        super::keyboard_diagnostics::disposition(swallow);
    }
    let owner = if swallow { "swallow" } else { "pass" };
    trace_gesture_delivery(ty, ev, owner, Some(disposition), remote);
    if let Some(CapturedEvent {
        event: InputEvent::Gesture(g),
        ..
    }) = &t.event
    {
        s.gestures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(*g, owner, CGEvent::timestamp(Some(ev)));
    }
    if !swallow {
        // The session tap is the local transformation boundary. Rewriting here
        // preserves the native delivery path and never changes system settings.
        if t.event.is_some() && disposition == Disposition::Pass {
            match crate::capture_loop::local_rewrite() {
                Some(oink_foundation::input::LocalRewrite::Key { code, pressed }) => {
                    if let Some(mac) = keymap::hid_to_mac(code) {
                        CGEvent::set_integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode, i64::from(mac));
                        CGEvent::set_type(
                            Some(ev),
                            if code.is_modifier() {
                                CGEventType::FlagsChanged
                            } else if pressed {
                                CGEventType::KeyDown
                            } else {
                                CGEventType::KeyUp
                            },
                        );
                        let mut flags = CGEvent::flags(Some(ev));
                        flags.0 &= !(CGEventFlags::MaskNumericPad.0 | CGEventFlags::MaskSecondaryFn.0);
                        if keymap::is_mac_keypad(mac) {
                            flags.0 |= CGEventFlags::MaskNumericPad.0;
                        }
                        if keymap::is_mac_function(mac) {
                            flags.0 |= CGEventFlags::MaskSecondaryFn.0;
                        }
                        CGEvent::set_flags(Some(ev), flags);
                        // Cached Unicode belongs to the physical key. Let the
                        // current input source derive text for the rewritten key.
                        // SAFETY: length is zero, so the pointer is never read.
                        unsafe { CGEvent::keyboard_set_unicode_string(Some(ev), 0, std::ptr::null()) };
                    }
                }
                Some(oink_foundation::input::LocalRewrite::ReverseWheel) => reverse_wheel_event(ev),
                None => {}
            }
        }
        if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID) == 0 {
            let (mapping, injected) = crate::capture_loop::local_modifier_mapping();
            CGEvent::set_flags(
                Some(ev),
                transformed_modifier_flags(CGEvent::flags(Some(ev)), &mapping, &injected),
            );
        }
    }
    if swallow { std::ptr::null_mut() } else { pass }
}

fn reverse_wheel_event(ev: &CGEvent) {
    let integers = [
        CGEventField::ScrollWheelEventDeltaAxis1,
        CGEventField::ScrollWheelEventDeltaAxis2,
        CGEventField::ScrollWheelEventPointDeltaAxis1,
        CGEventField::ScrollWheelEventPointDeltaAxis2,
    ]
    .map(|field| (field, CGEvent::integer_value_field(Some(ev), field)));
    let fixed = [
        CGEventField::ScrollWheelEventFixedPtDeltaAxis1,
        CGEventField::ScrollWheelEventFixedPtDeltaAxis2,
    ]
    .map(|field| (field, CGEvent::double_value_field(Some(ev), field)));
    // Quartz setters update related scroll representations. Read the complete
    // native delta snapshot before writing either axis so values are reversed once.
    for (field, value) in integers {
        CGEvent::set_integer_value_field(Some(ev), field, -value);
    }
    for (field, value) in fixed {
        CGEvent::set_double_value_field(Some(ev), field, -value);
    }
}
fn transformed_injected_flags(ev: &CGEvent, mapping: &[(KeyCode, KeyCode)], injected: &[KeyCode]) -> CGEventFlags {
    let flags = CGEvent::flags(Some(ev));
    if super::sysevent::read_aux_key(ev).is_some_and(|key| matches!(key.key, 0 | 1 | 7)) {
        flags
    } else {
        transformed_modifier_flags(flags, mapping, injected)
    }
}
fn transformed_modifier_flags(
    flags: CGEventFlags,
    mapping: &[(KeyCode, KeyCode)],
    injected: &[KeyCode],
) -> CGEventFlags {
    let changed = mapping
        .iter()
        .any(|(a, b)| a != b && (a.is_modifier() || b.is_modifier()));
    if !changed {
        return flags;
    }
    let mut held = Vec::new();
    let mut clear = 0;
    for usage in 0xe0..=0xe7 {
        let key = KeyCode::keyboard(usage);
        let Some(mac) = keymap::hid_to_mac(key) else { continue };
        let Some((mask, bit)) = modifier_flags(mac) else {
            continue;
        };
        clear |= mask.0 | bit;
        if modifier_pressed(mac, flags) == Some(true) {
            let mapped = mapping.iter().find(|(a, _)| *a == key).map_or(key, |(_, b)| *b);
            if mapped.is_modifier() {
                held.push(mapped);
            }
        }
    }
    held.extend(
        mapping
            .iter()
            .filter(|(a, b)| !a.is_modifier() && b.is_modifier())
            .map(|(_, b)| *b),
    );
    held.extend(injected.iter().copied());
    let mut value = flags.0 & !clear;
    for key in held {
        if let Some(mac) = keymap::hid_to_mac(key)
            && let Some((mask, bit)) = modifier_flags(mac)
        {
            value |= mask.0 | bit;
        }
    }
    CGEventFlags(value)
}

/// Log the actual tap result, including unparsed companion/touch events. Reading
/// raw fields and keeping a diagnostic ledger must never change the native event,
/// disposition or lifetime. A pass/echo is not evidence that Dock consumed it.
fn trace_gesture_delivery(
    ty: CGEventType,
    ev: &CGEvent,
    owner: &'static str,
    disposition: Option<Disposition>,
    remote: bool,
) {
    if !matches!(ty.0, 18..=20 | 29..=32) {
        return;
    }
    let field = |f| CGEvent::integer_value_field(Some(ev), CGEventField(f));
    tracing::debug!(
        event_type = ty.0,
        owner,
        ?disposition,
        remote,
        timestamp_ns = CGEvent::timestamp(Some(ev)),
        source_pid = CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID),
        user_data = CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData),
        hid_type = field(110),
        phase = field(132),
        swipe_phase = field(134),
        motion = field(123),
        raw_progress = CGEvent::double_value_field(Some(ev), CGEventField(124)),
        inverted_from_device = field(136),
        "gesture tap: return (delivery unconfirmed)"
    );
}

fn track_keys(captured: &CapturedEvent, disp: Option<u16>) {
    if let InputEvent::Button { button, pressed } = captured.event {
        BUTTONS.with(|b| {
            if pressed {
                b.borrow_mut().insert(button);
            } else {
                b.borrow_mut().remove(&button);
            }
        });
    }
    if let InputEvent::Key { code, pressed } = captured.event {
        PRESSED.with(|p| {
            if pressed {
                p.borrow_mut().insert(code);
            } else {
                p.borrow_mut().remove(&code);
                if let Some(k) = disp {
                    KEY_DISPOSITION.with(|d| d.borrow_mut().remove(&k));
                }
            }
        });
    }
}

fn is_motion(ty: CGEventType) -> bool {
    ty == CGEventType::MouseMoved
        || ty == CGEventType::LeftMouseDragged
        || ty == CGEventType::RightMouseDragged
        || ty == CGEventType::OtherMouseDragged
}

fn is_mouse_button(ty: CGEventType) -> bool {
    matches!(
        ty,
        CGEventType::LeftMouseDown
            | CGEventType::LeftMouseUp
            | CGEventType::RightMouseDown
            | CGEventType::RightMouseUp
            | CGEventType::OtherMouseDown
            | CGEventType::OtherMouseUp
    )
}

/// Native HID movement advances the HID base; CG placement can leave a fold
/// for the next hardware delta. Keep their accounting distinct. Use the delivered
/// coordinate without rewriting the event (which can latch typing-obscure).
/// Actual warps use cursor::restore_at's completion ledger.
fn account_cursor_placement(
    s: &Shared,
    ev: &CGEvent,
    kind: crate::pointer_transition::PlacementKind,
    native_delta: Option<(f64, f64)>,
) {
    let at = CGEvent::location(Some(ev));
    let at = Point::new(at.x, at.y);
    let timestamp_ns = CGEvent::timestamp(Some(ev));
    let mut fix = s.warp_fix.lock().unwrap_or_else(|e| e.into_inner());
    if kind == crate::pointer_transition::PlacementKind::NativeMotion {
        fix.native_absolute_motion(at, timestamp_ns);
    } else {
        fix.posted_absolute_motion(at, timestamp_ns);
    }
    s.pointer_transition
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .placed(crate::pointer_transition::Placement {
            kind,
            at,
            delta: native_delta.unwrap_or_else(|| {
                (
                    CGEvent::double_value_field(Some(ev), CGEventField::MouseEventDeltaX),
                    CGEvent::double_value_field(Some(ev), CGEventField::MouseEventDeltaY),
                )
            }),
            timestamp_ns,
            source_state: CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceStateID),
        });
}

fn drain_contacts(s: &Shared) {
    loop {
        let event = CONTACTS.with(|slot| slot.borrow_mut().as_mut().and_then(|c| c.pop(now_event_ns() / 1_000)));
        let Some(event) = event else {
            break;
        };
        // Provider sample times already use uptime raw; a pending sample drained
        // inside another CG callback must not borrow that event's capture time.
        let _source_time =
            crate::source_time::CaptureScope::new(crate::source_time::CaptureTime::from_micros(event.time_us));
        call_handler(s, &event);
    }
}

fn pointer_source(ev: &CGEvent) -> SourceKind {
    match device::sender_id(ev) {
        Some(id) => match device::describe(id).kind {
            SourceKind::Keyboard => SourceKind::Unknown,
            k => k,
        },
        None => SourceKind::Unknown,
    }
}

fn key_capture_metadata(
    ty: CGEventType,
    ev: &CGEvent,
    pressed_while_held: bool,
) -> crate::diagnostics::CaptureMetadata {
    crate::diagnostics::CaptureMetadata {
        raw: if ty == CGEventType::KeyDown {
            crate::diagnostics::RawKeyEvent::Down
        } else if ty == CGEventType::KeyUp {
            crate::diagnostics::RawKeyEvent::Up
        } else if ty == CGEventType::FlagsChanged {
            crate::diagnostics::RawKeyEvent::ModifierFlags
        } else if ty.0 == sysevent::TYPE_SYSDEFINED {
            crate::diagnostics::RawKeyEvent::SystemFunction
        } else {
            crate::diagnostics::RawKeyEvent::Unknown
        },
        backend_generated: Some(false),
        repeat: if ty.0 == sysevent::TYPE_SYSDEFINED {
            sysevent::read_aux_key(ev).map(|key| key.repeat)
        } else if matches!(
            ty,
            CGEventType::KeyDown | CGEventType::KeyUp | CGEventType::FlagsChanged
        ) {
            Some(
                ty == CGEventType::KeyDown
                    && CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventAutorepeat) != 0,
            )
        } else {
            None
        },
        pressed_while_held,
    }
}

fn translate(ty: CGEventType, ev: &CGEvent) -> Translated {
    let int = |f: CGEventField| CGEvent::integer_value_field(Some(ev), f);
    let dbl = |f: CGEventField| CGEvent::double_value_field(Some(ev), f);
    let loc = CGEvent::location(Some(ev));
    let cursor = Some(Point::new(loc.x, loc.y));
    let captured = |event: InputEvent, source: SourceKind, cursor: Option<Point>| CapturedEvent {
        physical: CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID) == 0,
        event,
        source,
        time_us: CGEvent::timestamp(Some(ev)) / 1_000,
        cursor,
    };
    let none = |pointer: bool| Translated {
        event: None,
        pointer,
        repeat_of: None,
        release_first: None,
        disp_key: None,
    };

    if is_motion(ty) {
        // 位移取双精度值：慢速移动时的小数部分不丢失。
        let dx = dbl(CGEventField::MouseEventDeltaX);
        let dy = dbl(CGEventField::MouseEventDeltaY);
        tracing::trace!(
            dx,
            dy,
            pid = int(CGEventField::EventSourceUnixProcessID),
            ts = CGEvent::timestamp(Some(ev)),
            x = loc.x,
            y = loc.y,
            "move"
        );
        return Translated {
            event: Some(captured(
                InputEvent::PointerDelta { dx, dy },
                pointer_source(ev),
                cursor,
            )),
            pointer: true,
            repeat_of: None,
            release_first: None,
            disp_key: None,
        };
    }

    let button_event = |num: i64, pressed: bool| Translated {
        event: Some(captured(
            InputEvent::Button {
                button: button_from_number(num),
                pressed,
            },
            pointer_source(ev),
            cursor,
        )),
        pointer: true,
        repeat_of: None,
        release_first: None,
        disp_key: None,
    };
    match ty {
        CGEventType::LeftMouseDown => return button_event(0, true),
        CGEventType::LeftMouseUp => return button_event(0, false),
        CGEventType::RightMouseDown => return button_event(1, true),
        CGEventType::RightMouseUp => return button_event(1, false),
        CGEventType::OtherMouseDown => return button_event(int(CGEventField::MouseEventButtonNumber), true),
        CGEventType::OtherMouseUp => return button_event(int(CGEventField::MouseEventButtonNumber), false),
        _ => {}
    }

    if ty == CGEventType::ScrollWheel {
        let continuous = int(CGEventField::ScrollWheelEventIsContinuous) != 0;
        let phase = int(CGEventField::ScrollWheelEventScrollPhase);
        let momentum = int(CGEventField::ScrollWheelEventMomentumPhase);
        // 像素增量（已含系统滚动加速）。CGEvent 的滚动增量本身就表示「视图移动方向」：
        // 正值 = 视图向文档顶部移动，本机的「自然滚动」设置已由系统体现在符号里，与线上约定一致。
        let dy = dbl(CGEventField::ScrollWheelEventPointDeltaAxis1);
        let dx = dbl(CGEventField::ScrollWheelEventPointDeltaAxis2);
        tracing::trace!(
            dx,
            dy,
            line_dy = dbl(CGEventField::ScrollWheelEventFixedPtDeltaAxis1),
            continuous,
            phase,
            momentum,
            inverted = int(CGEventField(137)),
            "scroll"
        );
        let Some(phase) = map_phase(phase, momentum) else {
            return none(true); // MayBegin：不上报
        };
        let source = if continuous {
            SourceKind::Touchpad
        } else {
            SourceKind::Mouse
        };
        return Translated {
            event: Some(captured(
                InputEvent::Scroll {
                    dx,
                    dy,
                    unit: ScrollUnit::Pixel,
                    phase,
                },
                source,
                cursor,
            )),
            pointer: true,
            repeat_of: None,
            release_first: None,
            disp_key: None,
        };
    }

    if ty == CGEventType::KeyDown || ty == CGEventType::KeyUp {
        let mac = int(CGEventField::KeyboardEventKeycode) as u16;
        let pressed = ty == CGEventType::KeyDown;
        let autorepeat = pressed && int(CGEventField::KeyboardEventAutorepeat) != 0;
        let Some(code) = keymap::mac_to_hid(mac) else {
            tracing::debug!("未知的 mac 键码（不记录内容）");
            return none(false);
        };
        let held = pressed && PRESSED.with(|p| p.borrow().contains(&code));
        if autorepeat && !held {
            // 没见过按下（截获启动前按下、已补报过抬起）：不上报，沿用原来的处理。
            return Translated {
                event: None,
                pointer: false,
                repeat_of: Some(mac),
                release_first: None,
                disp_key: None,
            };
        }
        // 自动重复作为再一次按下上报：重复只在源机产生，路由把发往远端的逐个转发。
        return Translated {
            event: Some(captured(InputEvent::Key { code, pressed }, SourceKind::Keyboard, None)),
            pointer: false,
            repeat_of: None,
            release_first: (held && !autorepeat).then_some(code),
            disp_key: Some(mac),
        };
    }

    if ty == CGEventType::FlagsChanged {
        if int(CGEventField::EventSourceUnixProcessID) == 0 {
            crate::diagnostics::caps(CGEvent::flags(Some(ev)).contains(CGEventFlags::MaskAlphaShift));
        }
        let mac = int(CGEventField::KeyboardEventKeycode) as u16;
        if mac != keymap::MAC_CAPS_LOCK && modifier_flags(mac).is_none() {
            crate::diagnostics::nonmodifier_flags();
        }
        let Some(code) = keymap::mac_to_hid(mac) else {
            return none(false);
        };
        let flags = CGEvent::flags(Some(ev));
        let held = PRESSED.with(|p| p.borrow().contains(&code));
        let pressed = if mac == keymap::MAC_CAPS_LOCK {
            let physical = int(CGEventField::EventSourceUnixProcessID) == 0;
            tracing::debug!(physical, flags = flags.0, held, "Caps Lock flags companion");
            // Real Caps FlagsChanged is a lock notification, not a reliable paired
            // key stream: macOS 26.6.2 logs contain a DOWN with no matching UP,
            // despite two NX auxiliary events. The lock guard also generates flags
            // notifications (kVK=255). Counting these permanently reverses phase.
            // NX Caps data1 has explicit down/up, already decoded without AppKit by
            // sysevent (field encoding verified on 14.7.8/26.6.2). Ignore physical
            // companions; synthetic FlagsChanged retains its explicit stateless bit
            // for existing probes. Never learn a process-global flag capability or
            // derive phase from PRESSED, AlphaShift, or CGEventSourceKeyState.
            if physical {
                return none(false);
            }
            flags.0 & ALPHASHIFT_STATELESS != 0
        } else {
            // A lock-state change can accompany the next ordinary physical key:
            // macOS 26.6.2 emitted FlagsChanged(Tab) and KeyDown(Tab) with the
            // same HID timestamp. FlagsChanged is a modifier notification, not a
            // second key press. The old !held fallback invented a DOWN, so the
            // real DOWN triggered lost-UP recovery and typed the first letter
            // twice when forwarded (replayed on a 14.7.8 target). Only known
            // modifiers have a direction here; ordinary keys keep their normal
            // KeyDown/KeyUp and autorepeat path. Caps is handled above.
            let Some(pressed) = modifier_pressed(mac, flags) else {
                return none(false);
            };
            pressed
        };
        return Translated {
            event: Some(captured(InputEvent::Key { code, pressed }, SourceKind::Keyboard, None)),
            pointer: false,
            repeat_of: None,
            release_first: None,
            disp_key: Some(mac),
        };
    }

    if ty.0 == sysevent::TYPE_SYSDEFINED {
        return translate_aux(ev);
    }

    // 手势：Dock 轻扫、捏合、旋转、智能缩放翻译成 `Gesture`；其余（只携带触点的手势事件、
    // Dock 轻扫的伴随事件、轻扫、开始 / 结束手势）不上报。Remote 模式下一律吞掉（`pointer`）。
    let gesture = sysevent::parse_gesture(ty.0, |f| int(CGEventField(f)), |f| dbl(CGEventField(f)));
    if let Some(g) = gesture {
        tracing::trace!(
            ?g,
            raw_progress = dbl(CGEventField(124)),
            inverted_from_device = int(CGEventField(136)),
            "gesture"
        );
    }
    Translated {
        event: gesture.map(|g| captured(InputEvent::Gesture(g), SourceKind::Touchpad, None)),
        pointer: true,
        repeat_of: None,
        release_first: None,
        disp_key: None,
    }
}

/// 系统定义事件：发往正在操作的那台电脑的媒体键（`keymap::nx_to_hid`）翻译成 Consumer 页的按键，
/// 按下 / 抬起 / 重复与普通键一样处理（重复只在源机产生、逐个转发）。其余（键盘背光、电源、
/// 弹出等、其他 subtype）不上报、原样放行。Caps 的明确方向也在这里采集，
/// 但原生事件仍放行给本机，保持 LocalGuard 的既有行为。
fn translate_aux(ev: &CGEvent) -> Translated {
    let pass = Translated {
        event: None,
        pointer: false,
        repeat_of: None,
        release_first: None,
        disp_key: None,
    };
    let Some(aux) = sysevent::read_aux_key(ev) else {
        return pass;
    };
    let Some(code) = (if aux.key == 4 {
        Some(KeyCode::keyboard(0x39))
    } else {
        keymap::nx_to_hid(aux.key)
    }) else {
        tracing::trace!("system-defined key stays local");
        return pass;
    };
    if aux.key == 4 {
        tracing::debug!(
            pressed = aux.pressed,
            repeat = aux.repeat,
            data1 = aux.data1(),
            timestamp_ns = CGEvent::timestamp(Some(ev)),
            physical = CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID) == 0,
            held = PRESSED.with(|p| p.borrow().contains(&code)),
            "Caps Lock explicit transition"
        );
    }
    let disp = AUX_DISP | aux.key as u16;
    let held = aux.pressed && PRESSED.with(|p| p.borrow().contains(&code));
    if aux.pressed && aux.repeat && !held {
        // 没见过按下的重复：不上报，沿用按下时的处理结果。
        return Translated {
            repeat_of: Some(disp),
            ..pass
        };
    }
    Translated {
        event: Some(CapturedEvent {
            physical: CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID) == 0,
            event: InputEvent::Key {
                code,
                pressed: aux.pressed,
            },
            source: SourceKind::Keyboard,
            time_us: now_us(),
            cursor: None,
        }),
        pointer: false,
        repeat_of: None,
        release_first: (held && !aux.repeat).then_some(code),
        disp_key: Some(disp),
    }
}

/// 由 FlagsChanged 事件的 flags 判断修饰键是按下还是抬起。取不到左右位时返回 `None`。
fn modifier_pressed(mac: u16, flags: CGEventFlags) -> Option<bool> {
    let (mask, bit) = modifier_flags(mac)?;
    if mac == 0x39 {
        // Caps Lock：AlphaShift 是锁定状态，物理按压另从 NX Caps 的 data1 读取。
        return None;
    }
    if bit != 0 {
        let all_bits = device_bits::LCTL
            | device_bits::LSHIFT
            | device_bits::RSHIFT
            | device_bits::LCMD
            | device_bits::RCMD
            | device_bits::LALT
            | device_bits::RALT
            | device_bits::RCTL;
        if flags.0 & all_bits != 0 || flags.0 & mask.0 == 0 {
            return Some(flags.0 & bit != 0);
        }
        // 只有与设备无关的位（合成事件）：退回到按掩码判断。
    }
    Some(flags.0 & mask.0 != 0)
}

/// CGEvent 的滚动阶段字段 → 线上阶段。`MayBegin`（手指放上但还没动）返回 `None`。
fn map_phase(phase: i64, momentum: i64) -> Option<ScrollPhase> {
    Some(match (phase, momentum) {
        (_, 1) => ScrollPhase::MomentumBegan,
        (_, 2) => ScrollPhase::Momentum,
        (_, 3) => ScrollPhase::MomentumEnded,
        (1, _) => ScrollPhase::Began,
        (2, _) => ScrollPhase::Changed,
        (4, _) => ScrollPhase::Ended,
        (8, _) => ScrollPhase::Cancelled,
        (128, _) => return None,
        _ => ScrollPhase::None,
    })
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn tagged_volume_keeps_explicit_fine_step_flags_across_local_remapping() {
        let ev = CGEvent::new(None).unwrap();
        CGEvent::set_type(Some(&ev), CGEventType(14));
        CGEvent::set_integer_value_field(Some(&ev), CGEventField(83), 8);
        let flags = CGEventFlags::MaskShift | CGEventFlags::MaskAlternate;
        CGEvent::set_flags(Some(&ev), flags);
        let mapping = [(KeyCode::keyboard(0xe2), KeyCode::keyboard(0xe3))];
        for key in [0, 1, 7] {
            CGEvent::set_integer_value_field(Some(&ev), CGEventField(149), (key << 16) | 0xa00);
            assert_eq!(transformed_injected_flags(&ev, &mapping, &[]), flags);
        }
        CGEvent::set_type(Some(&ev), CGEventType::KeyDown);
        assert_ne!(transformed_injected_flags(&ev, &mapping, &[]), flags);
    }
    #[test]
    fn deferred_cursor_mode_wakes_consumer_after_publishing_request() {
        let pending = AtomicU8::new(0);
        let notified = Cell::new(false);
        // A previous keyboard-mode wake may already have been consumed. The
        // cursor request needs its own notification, with the flag visible first.
        defer_mode_apply(&pending, || {
            assert_ne!(
                pending.fetch_and(!PENDING_APPLY_MODE, Ordering::AcqRel) & PENDING_APPLY_MODE,
                0
            );
            notified.set(true);
        });
        assert!(notified.get(), "cursor mode waited for the observer's periodic timeout");
        assert_eq!(pending.load(Ordering::Acquire), 0);
    }

    #[test]
    fn image_nudge_location_is_the_current_frozen_anchor_and_cannot_undo_cancel() {
        let frozen = Point::new(0., 55.5);
        let anchor = oink_foundation::dnd::DragAnchor {
            generation: 9,
            at: frozen,
            surface: 0,
        };
        assert_eq!(image_nudge_location(Some(anchor), 9, frozen, frozen), Some(frozen));
        assert_eq!(image_nudge_location(Some(anchor), 8, frozen, frozen), None);
        assert_eq!(image_nudge_location(None, 9, frozen, frozen), None);
        let flyback = Point::new(63., 0.);
        assert_eq!(image_nudge_location(Some(anchor), 9, flyback, flyback), None);
        assert_eq!(image_nudge_location(Some(anchor), 9, frozen, flyback), None);
    }

    fn fake_shared(handler: EventHandler) -> Shared {
        let at = Point::new(12., 34.);
        let motion = Arc::new(super::super::cursor_reveal::Motion::default());
        Shared {
            safety: Arc::new(crate::session_gate::SessionGate::default()),
            capture_epoch: AtomicU64::new(0),
            timing_epoch: AtomicU64::new(0),
            observations: None,
            handler,
            gestures: Mutex::new(super::super::gesture_diagnostics::Tracker::default()),
            physical_buttons: Arc::default(),
            motion_generation: motion.new_generation(),
            motion,
            remote: AtomicBool::new(true),
            keyboard_remote: AtomicBool::new(false),
            hidden: Mutex::new(HiddenCursor::frozen(at)),
            passed_while_frozen: AtomicU64::new(0),
            owner_generation: AtomicU64::new(0),
            drag_anchor: Mutex::new(None),
            owner_move: Mutex::new(None),
            owner_posts: Mutex::new(OwnerPosts::default()),
            park: Mutex::new(None),
            restore_pending: AtomicBool::new(false),
            hold_motion: AtomicBool::new(false),
            restore_x: AtomicU64::new(0),
            restore_y: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            pending: AtomicU8::new(0),
            port: AtomicPtr::new(std::ptr::null_mut()),
            run_loop: AtomicPtr::new(std::ptr::null_mut()),
            cursor_lock: Mutex::new(()),
            warp_fix: Mutex::new(WarpDeltaFix::new()),
            ordinary_buttons: Mutex::new(crate::ordinary_motion::ButtonGate::default()),
            pointer_transition: Mutex::new(crate::pointer_transition::Tracker::default()),
            freeze_check_ns: AtomicU64::new(0),
            caps_guard: LocalGuard::new(),
        }
    }

    #[test]
    fn capture_observation_excludes_our_freeze_and_incomplete_local_restore() {
        let shared = fake_shared(Arc::new(|_| Disposition::Pass));
        // A logical focus return can precede the native Show/associate completion.
        assert_eq!(shared.observe_local_pointer(|| true), None);
        shared.remote.store(false, Ordering::Release);
        shared.hold_motion.store(true, Ordering::Release);
        assert_eq!(shared.observe_local_pointer(|| true), None);
        shared.hold_motion.store(false, Ordering::Release);
        assert_eq!(shared.observe_local_pointer(|| true), None);
        *shared.hidden.lock().unwrap() = HiddenCursor::Shown;
        shared.restore_pending.store(true, Ordering::Release);
        assert_eq!(shared.observe_local_pointer(|| true), None);
        shared.restore_pending.store(false, Ordering::Release);
        assert_eq!(shared.observe_local_pointer(|| true), Some(true));
    }

    #[test]
    fn capture_observation_cannot_sample_half_a_native_transition() {
        let shared = fake_shared(Arc::new(|_| Disposition::Pass));
        shared.remote.store(false, Ordering::Release);
        *shared.hidden.lock().unwrap() = HiddenCursor::Shown;
        // Native restore may already have cleared HIDDEN before Show returns.
        // The cursor lock, not that early Boolean, delimits native completion.
        let transition = shared.cursor_lock.lock().unwrap();
        assert_eq!(shared.observe_local_pointer(|| true), None);
        drop(transition);
        assert_eq!(
            shared.observe_local_pointer(|| {
                assert!(shared.cursor_lock.try_lock().is_err(), "sample must remain coherent");
                true
            }),
            Some(true)
        );
    }

    #[test]
    fn paused_capture_passes_every_native_event_and_cannot_reenable_old_tap() {
        for remote in [false, true] {
            for resumed in [false, true] {
                let calls = Arc::new(AtomicU64::new(0));
                let seen = calls.clone();
                let shared = fake_shared(Arc::new(move |_| {
                    seen.fetch_add(1, Ordering::Relaxed);
                    Disposition::Swallow
                }));
                shared.remote.store(remote, Ordering::Release);
                shared.keyboard_remote.store(remote, Ordering::Release);
                shared.hold_motion.store(true, Ordering::Release);
                shared.owner_generation.store(42, Ordering::Release);
                shared.safety.set_paused(true);
                if resumed {
                    shared.safety.set_paused(false);
                }
                // Unposted event, null native ports: exercise the production
                // callback without a real tap, authorization query or input.
                let event = CGEvent::new(None).unwrap();
                let pointer = CFRetained::as_ptr(&event);
                for ty in [
                    CGEventType::MouseMoved,
                    CGEventType::LeftMouseDragged,
                    CGEventType::LeftMouseDown,
                    CGEventType::LeftMouseUp,
                    CGEventType::KeyDown,
                    CGEventType::KeyUp,
                    CGEventType::FlagsChanged,
                    CGEventType::TapDisabledByTimeout,
                    CGEventType::TapDisabledByUserInput,
                ] {
                    // SAFETY: callback borrows this local context and unposted event.
                    let result = unsafe {
                        callback(
                            std::ptr::null_mut(),
                            ty,
                            pointer,
                            &shared as *const Shared as *mut c_void,
                        )
                    };
                    assert_eq!(result, pointer.as_ptr());
                }
                assert_eq!(calls.load(Ordering::Relaxed), 0);
                assert!(!shared.capture_permitted());
                assert!(!shared.owner_current(42));
                assert_eq!(shared.pending.load(Ordering::Acquire), 0);
            }
        }
    }

    #[test]
    fn marked_image_nudge_passes_capture_once_without_physical_routing_or_authority() {
        // Invoke the real tap callback on an unposted CGEvent and an isolated
        // shared ledger. No event tap, cursor operation or desktop input is used.
        let calls = Arc::new(AtomicU64::new(0));
        let observed = calls.clone();
        let at = Point::new(12., 34.);
        let shared = fake_shared(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            Disposition::Swallow
        }));
        shared.physical_buttons.record(0, true);
        shared.ordinary_buttons.lock().unwrap().transition(0, true, 1);
        shared
            .warp_fix
            .lock()
            .unwrap()
            .native_absolute_motion(Point::new(5., 7.), 1);
        shared.warp_fix.lock().unwrap().posted_move(at);
        let before = shared.warp_fix.lock().unwrap().snapshot();
        let epoch = shared.motion.position_epoch();
        let event = super::super::inject::image_nudge_event(None, at, MouseButton::Right, CGEventFlags(0)).unwrap();
        // Even a PID of zero must not reach physical bookkeeping with MAGIC.
        CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUnixProcessID, 0);
        CGEvent::set_timestamp(Some(&event), 2);
        // SAFETY: shared and the unposted event remain alive through the call;
        // the own-event branch does not use its null event-tap proxy.
        let result = unsafe {
            callback(
                std::ptr::null_mut(),
                CGEventType::RightMouseDragged,
                NonNull::from(&*event),
                (&shared as *const Shared).cast_mut().cast(),
            )
        };
        assert_eq!(result, (&*event as *const CGEvent).cast_mut());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(shared.physical_buttons.is_down(0));
        assert!(!shared.physical_buttons.is_down(1));
        assert_eq!(
            shared.motion.position_epoch(),
            epoch,
            "physical-motion authority was bypassed"
        );
        let after = shared.warp_fix.lock().unwrap().snapshot();
        assert_eq!(
            after, before,
            "same-position accounting must not double-book prior injected displacement"
        );
        let observation = shared
            .pointer_transition
            .lock()
            .unwrap()
            .resumed(crate::pointer_transition::Observation::default())
            .unwrap();
        assert_eq!(observation.placements, 1);
        assert_eq!(observation.last_placement.unwrap().delta, (0., 0.));
        assert_eq!(observation.last_placement.unwrap().at, at);
        assert!(matches!(*shared.hidden.lock().unwrap(), HiddenCursor::Frozen { at: frozen, .. } if frozen == at));
    }

    #[test]
    fn local_mapping_rebuilds_both_modifier_sides_and_following_pointer_flags() {
        let flags = CGEventFlags(
            CGEventFlags::MaskControl.0
                | device_bits::RCTL
                | CGEventFlags::MaskCommand.0
                | device_bits::LCMD
                | CGEventFlags::MaskShift.0
                | device_bits::LSHIFT
                | CGEventFlags::MaskAlphaShift.0,
        );
        let mapping = [
            (KeyCode::RIGHT_CTRL, KeyCode::RIGHT_GUI),
            (KeyCode::LEFT_GUI, KeyCode::LEFT_ALT),
        ];
        let rewritten = transformed_modifier_flags(flags, &mapping, &[KeyCode::LEFT_CTRL]);
        assert_eq!(modifier_pressed(0x36, rewritten), Some(true));
        assert_eq!(modifier_pressed(0x37, rewritten), Some(false));
        assert_eq!(modifier_pressed(0x3a, rewritten), Some(true));
        assert_eq!(modifier_pressed(0x3b, rewritten), Some(true));
        assert_eq!(modifier_pressed(0x3e, rewritten), Some(false));
        assert_eq!(modifier_pressed(0x38, rewritten), Some(true));
        assert!(rewritten.contains(CGEventFlags::MaskAlphaShift));
        let from_letter =
            transformed_modifier_flags(CGEventFlags(0), &[(KeyCode::keyboard(4), KeyCode::RIGHT_SHIFT)], &[]);
        assert_eq!(modifier_pressed(0x3c, from_letter), Some(true));
        assert_eq!(modifier_pressed(0x38, from_letter), Some(false));
    }

    #[test]
    fn local_wheel_rewrite_changes_all_native_axis_representations() {
        let event =
            CGEvent::new_scroll_wheel_event2(None, objc2_core_graphics::CGScrollEventUnit::Line, 2, 3, 4, 0).unwrap();
        CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUserData, MAGIC);
        let before = [
            CGEventField::ScrollWheelEventDeltaAxis1,
            CGEventField::ScrollWheelEventDeltaAxis2,
            CGEventField::ScrollWheelEventPointDeltaAxis1,
            CGEventField::ScrollWheelEventPointDeltaAxis2,
        ]
        .map(|field| (field, CGEvent::integer_value_field(Some(&event), field)));
        let fixed = [
            CGEventField::ScrollWheelEventFixedPtDeltaAxis1,
            CGEventField::ScrollWheelEventFixedPtDeltaAxis2,
        ]
        .map(|field| (field, CGEvent::double_value_field(Some(&event), field)));
        reverse_wheel_event(&event);
        for (field, value) in before {
            assert_eq!(CGEvent::integer_value_field(Some(&event), field), -value);
        }
        for (field, value) in fixed {
            assert_eq!(CGEvent::double_value_field(Some(&event), field), -value);
        }
    }

    #[test]
    fn keyboard_guard_pointer_flags_strip_blocked_shift_and_preserve_other_modifiers() {
        let flags = CGEventFlags(CGEventFlags::MaskShift.0 | device_bits::LSHIFT | CGEventFlags::MaskCommand.0);
        let result = without_blocked_pointer_modifiers(flags, &[KeyCode::LEFT_SHIFT, KeyCode::RIGHT_SHIFT]);
        assert_eq!(result, CGEventFlags::MaskCommand);
    }

    #[test]
    fn tap_classification_forwards_foreign_posts_regardless_of_source_state() {
        for source in [-1, 0, 1] {
            for pid in [0, i64::from(std::process::id()), i64::from(std::process::id()) + 1] {
                for tag in [0, 0x1234, MAGIC, cursor::owner::event_tag(123)] {
                    let event = CGEvent::new(None).unwrap();
                    for (field, value) in [
                        (CGEventField::EventSourceStateID, source),
                        (CGEventField::EventSourceUnixProcessID, pid),
                        (CGEventField::EventSourceUserData, tag),
                    ] {
                        CGEvent::set_integer_value_field(Some(&event), field, value);
                    }
                    assert_eq!(
                        marked_own_event(&event),
                        tag == MAGIC || cursor::owner::event_generation(tag).is_some()
                    );
                }
            }
        }
    }

    #[test]
    fn keyboard_guard_never_treats_marked_synthetic_input_as_physical() {
        // Decoder evidence only: this marked event is never posted to a desktop.
        let event = CGEvent::new_keyboard_event(None, 0, true).unwrap();
        CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUserData, MAGIC);
        assert!(!physical_keyboard_event(CGEventType::KeyDown, &event));
        assert!(!physical_keyboard_event(CGEventType::FlagsChanged, &event));
    }

    #[test]
    fn frozen_left_only_beyond_tolerance() {
        let at = Point::new(0.0, 558.5);
        assert!(!left_frozen(Point::new(0.0, 558.0), at));
        assert!(!left_frozen(Point::new(1.5, 559.5), at));
        assert!(left_frozen(Point::new(6.0, 558.0), at));
        assert!(left_frozen(Point::new(864.0, 558.5), at));
    }

    #[test]
    fn freeze_failure_warnings_are_rate_limited() {
        let t0 = Instant::now();
        let mut w = FreezeWatch::default();
        assert_eq!(w.failed(t0), Some(1));
        assert_eq!(w.failed(t0 + Duration::from_millis(30)), None);
        assert_eq!(w.failed(t0 + Duration::from_millis(60)), None);
        // 过了限频间隔：记一条，带上这段时间的次数。
        assert_eq!(w.failed(t0 + FREEZE_WARN_EVERY), Some(3));
        assert_eq!(w.failed(t0 + FREEZE_WARN_EVERY + Duration::from_millis(1)), None);
    }

    #[test]
    fn event_clock_is_monotonic_nanoseconds() {
        let before = now_event_ns();
        std::thread::sleep(Duration::from_millis(5));
        let after = now_event_ns();
        assert!(before > 0);
        let d = after - before;
        assert!((4_000_000..1_000_000_000).contains(&d), "{d}");
    }

    #[test]
    fn lost_up_needs_two_sightings_apart() {
        let a = KeyCode::keyboard(0x04);
        let b = KeyCode::keyboard(0x05);
        let t0 = Instant::now();
        let mut c = LostUps::default();
        // 第一次看到松开：只记下。
        assert!(c.confirm(&[a], t0).is_empty());
        // 太快再看一次（run loop 被提前唤醒）：仍不补报。
        assert!(c.confirm(&[a], t0 + Duration::from_millis(5)).is_empty());
        // 相隔足够久仍松开：补报，之后不再重复补报。
        assert_eq!(c.confirm(&[a], t0 + LOST_UP_CONFIRM), vec![a]);
        assert!(c.confirm(&[], t0 + LOST_UP_CONFIRM * 2).is_empty());
        // 中途又按着了（抬起事件其实只是还在队列里）：重新计时。
        assert!(c.confirm(&[b], t0).is_empty());
        assert!(c.confirm(&[], t0 + Duration::from_millis(20)).is_empty());
        assert!(c.confirm(&[b], t0 + LOST_UP_CONFIRM).is_empty());
        assert_eq!(c.confirm(&[b], t0 + LOST_UP_CONFIRM * 2), vec![b]);
    }

    #[test]
    fn phases() {
        assert_eq!(map_phase(0, 0), Some(ScrollPhase::None));
        assert_eq!(map_phase(1, 0), Some(ScrollPhase::Began));
        assert_eq!(map_phase(0, 2), Some(ScrollPhase::Momentum));
        assert_eq!(map_phase(128, 0), None);
    }

    #[test]
    fn modifier_state_from_flags() {
        let shift_l = CGEventFlags(CGEventFlags::MaskShift.0 | device_bits::LSHIFT);
        assert_eq!(modifier_pressed(0x38, shift_l), Some(true));
        // 右 Shift 抬起、左 Shift 仍按着：右 Shift 判为抬起。
        assert_eq!(modifier_pressed(0x3C, shift_l), Some(false));
        assert_eq!(modifier_pressed(0x37, CGEventFlags(0)), Some(false));
        // 合成事件只有与设备无关的位。
        assert_eq!(modifier_pressed(0x37, CGEventFlags::MaskCommand), Some(true));
        assert_eq!(modifier_pressed(0x39, CGEventFlags::MaskAlphaShift), None);
    }

    #[test]
    fn held_modifiers_survive_stale_key_bitmap_and_still_reconcile_lost_up() {
        for mac in [0x38, 0x3c, 0x3b, 0x3e, 0x3a, 0x3d, 0x37, 0x36] {
            let code = keymap::mac_to_hid(mac).unwrap();
            let (mask, bit) = modifier_flags(mac).unwrap();
            let held = CGEventFlags(mask.0 | bit);
            let t0 = Instant::now();
            let mut check = LostUps::default();
            // Multiple 50 ms sweeps during a hold: the right Shift HID bitmap
            // can remain false throughout, including between ordinary keys.
            for tick in 0..20 {
                let up = released([code], |_| mac_key_physically_down(mac, || held, || false));
                assert!(check.confirm(&up, t0 + Duration::from_millis(tick * 50)).is_empty());
            }
            // A genuinely lost modifier UP must still be recovered, with the
            // existing two-sighting delay, even if the bitmap is stale true.
            let up = released([code], |_| mac_key_physically_down(mac, || CGEventFlags(0), || true));
            assert_eq!(up, vec![code]);
            let released_at = t0 + Duration::from_secs(1);
            assert!(check.confirm(&up, released_at).is_empty());
            assert_eq!(check.confirm(&up, released_at + LOST_UP_CONFIRM), vec![code]);
        }
    }

    #[test]
    fn modifier_reconciliation_distinguishes_sides_and_keeps_other_key_policies() {
        for (left, right) in [(0x38, 0x3c), (0x3b, 0x3e), (0x3a, 0x3d), (0x37, 0x36)] {
            let (mask, left_bit) = modifier_flags(left).unwrap();
            let (_, right_bit) = modifier_flags(right).unwrap();
            for (bits, left_down, right_down) in [
                (left_bit | right_bit, true, true),
                (left_bit, true, false),
                (right_bit, false, true),
            ] {
                let flags = CGEventFlags(mask.0 | bits);
                for (mac, down) in [(left, left_down), (right, right_down)] {
                    assert_eq!(
                        mac_key_physically_down(mac, || flags, || panic!("modifier bitmap")),
                        Some(down)
                    );
                }
            }
        }
        assert_eq!(
            mac_key_physically_down(keymap::MAC_CAPS_LOCK, || panic!("Caps flags"), || panic!("Caps bitmap")),
            None
        );
        for down in [true, false] {
            assert_eq!(
                mac_key_physically_down(0, || panic!("ordinary flags"), || down),
                Some(down)
            );
        }
    }

    #[test]
    fn reconcile_reports_only_keys_known_to_be_up() {
        let a = KeyCode::keyboard(0x04);
        let b = KeyCode::keyboard(0x05);
        let c = KeyCode::keyboard(0x06);
        // a 已松开、b 仍按着、c 查不到：只补报 a。
        let is_down = |k: KeyCode| match k {
            k if k == a => Some(false),
            k if k == b => Some(true),
            _ => None,
        };
        assert_eq!(released([c, b, a], is_down), vec![a]);
        let buttons = released([MouseButton::Right, MouseButton::Left], |b| {
            Some(b == MouseButton::Left)
        });
        assert_eq!(buttons, vec![MouseButton::Right]);
    }

    #[test]
    fn diagnostic_metadata_preserves_native_kind_repeat_and_duplicate_down() {
        use crate::diagnostics::RawKeyEvent;
        let event = CGEvent::new_keyboard_event(None, 0, true).unwrap();
        for (ty, raw) in [
            (CGEventType::KeyDown, RawKeyEvent::Down),
            (CGEventType::KeyUp, RawKeyEvent::Up),
            (CGEventType::FlagsChanged, RawKeyEvent::ModifierFlags),
        ] {
            let metadata = key_capture_metadata(ty, &event, false);
            assert_eq!(metadata.raw, raw);
            assert_eq!(metadata.repeat, Some(false));
            assert_eq!(metadata.backend_generated, Some(false));
        }
        CGEvent::set_integer_value_field(Some(&event), CGEventField::KeyboardEventAutorepeat, 1);
        assert_eq!(
            key_capture_metadata(CGEventType::KeyDown, &event, false).repeat,
            Some(true)
        );
        assert!(key_capture_metadata(CGEventType::KeyDown, &event, true).pressed_while_held);
        let aux = caps_aux(true);
        let metadata = key_capture_metadata(CGEventType(sysevent::TYPE_SYSDEFINED), &aux, false);
        assert_eq!(metadata.raw, RawKeyEvent::SystemFunction);
        assert_eq!(metadata.repeat, Some(false));
    }

    #[test]
    fn ignored_ordinary_flags_emit_only_an_identity_free_anomaly() {
        use crate::diagnostics::OBSERVED_NONMODIFIER_FLAGS;
        for mac in [0, 11] {
            OBSERVED_NONMODIFIER_FLAGS.with(|records| *records.borrow_mut() = Some(Vec::new()));
            let event = CGEvent::new_keyboard_event(None, mac, true).unwrap();
            assert!(translate(CGEventType::FlagsChanged, &event).event.is_none());
            // Each ordinary key submits exactly one identical boolean payload;
            // there is no key identity slot and no tracing subscriber to race.
            let records = OBSERVED_NONMODIFIER_FLAGS.with(|records| records.borrow_mut().take());
            assert_eq!(records, Some(vec![true]));
        }
    }

    #[test]
    fn ordinary_key_flags_companion_cannot_create_a_second_press_or_release() {
        // A, B, Tab, Return and F1 cover letters and the physical Tab reference.
        for mac in [0, 11, 48, 36, 122] {
            let code = keymap::mac_to_hid(mac).unwrap();
            PRESSED.with(|p| p.borrow_mut().remove(&code));
            let event = CGEvent::new_keyboard_event(None, mac, true).unwrap();
            CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUnixProcessID, 0);
            for held in [false, true] {
                if held {
                    PRESSED.with(|p| p.borrow_mut().insert(code));
                }
                for flags in [0, CGEventFlags::MaskAlphaShift.0, CGEventFlags::MaskShift.0] {
                    CGEvent::set_type(Some(&event), CGEventType::FlagsChanged);
                    CGEvent::set_flags(Some(&event), CGEventFlags(flags));
                    let companion = translate(CGEventType::FlagsChanged, &event);
                    assert!(companion.event.is_none());
                    assert!(companion.release_first.is_none());
                    assert!(companion.disp_key.is_none());
                    assert_eq!(PRESSED.with(|p| p.borrow().contains(&code)), held);
                }
            }
            PRESSED.with(|p| p.borrow_mut().remove(&code));
            CGEvent::set_type(Some(&event), CGEventType::KeyDown);
            CGEvent::set_integer_value_field(Some(&event), CGEventField::KeyboardEventAutorepeat, 0);
            let down = translate(CGEventType::KeyDown, &event);
            assert!(matches!(down.event.as_ref().map(|e| &e.event),
                Some(InputEvent::Key { code: k, pressed: true }) if *k == code));
            assert!(down.release_first.is_none());
            track_keys(down.event.as_ref().unwrap(), down.disp_key);
            CGEvent::set_integer_value_field(Some(&event), CGEventField::KeyboardEventAutorepeat, 1);
            let repeat = translate(CGEventType::KeyDown, &event);
            assert!(repeat.event.is_some());
            assert!(repeat.release_first.is_none());
            CGEvent::set_type(Some(&event), CGEventType::KeyUp);
            let up = translate(CGEventType::KeyUp, &event);
            assert!(matches!(up.event.as_ref().map(|e| &e.event),
                Some(InputEvent::Key { code: k, pressed: false }) if *k == code));
            track_keys(up.event.as_ref().unwrap(), up.disp_key);
            assert!(!PRESSED.with(|p| p.borrow().contains(&code)));
        }
    }

    fn caps_aux(pressed: bool) -> CFRetained<CGEvent> {
        let event = CGEvent::new(None).unwrap();
        CGEvent::set_type(Some(&event), CGEventType(sysevent::TYPE_SYSDEFINED));
        // Same NX encoding as physical Caps, with no event posting or OS state change.
        for (field, value) in [(83, 8), (149, if pressed { 0x40A00 } else { 0x40B00 })] {
            CGEvent::set_integer_value_field(Some(&event), CGEventField(field), value);
        }
        CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUnixProcessID, 0);
        event
    }

    fn capture_caps(pressed: bool) -> Translated {
        let translated = translate(CGEventType(sysevent::TYPE_SYSDEFINED), &caps_aux(pressed));
        assert!(matches!(
            translated.event.as_ref().map(|e| &e.event),
            Some(InputEvent::Key { code, pressed: p }) if *code == KeyCode::keyboard(0x39) && *p == pressed
        ));
        track_keys(translated.event.as_ref().unwrap(), translated.disp_key);
        translated
    }

    #[test]
    fn caps_next_short_press_recovers_after_missing_down_or_up() {
        for missing_down in [true, false] {
            forget_caps_gesture();
            // Deliver only one half of the preceding gesture. The old !held
            // fallback inverted every subsequent gesture after either loss.
            capture_caps(!missing_down);
            let down = capture_caps(true);
            assert_eq!(down.release_first.is_some(), !missing_down);
            let up = capture_caps(false);
            assert_eq!(up.release_first, None);
            assert!(!forget_caps_gesture());
            // Repeat the short press: no lasting phase error.
            assert_eq!(capture_caps(true).release_first, None);
            capture_caps(false);
        }
    }

    #[test]
    fn caps_flags_companions_do_not_duplicate_or_invert_native_transitions() {
        forget_caps_gesture();
        capture_caps(true);
        for flags in [0, CGEventFlags::MaskAlphaShift.0, ALPHASHIFT_STATELESS] {
            let event = CGEvent::new_keyboard_event(None, keymap::MAC_CAPS_LOCK, true).unwrap();
            CGEvent::set_type(Some(&event), CGEventType::FlagsChanged);
            CGEvent::set_flags(Some(&event), CGEventFlags(flags));
            CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUnixProcessID, 0);
            assert!(translate(CGEventType::FlagsChanged, &event).event.is_none());
        }
        capture_caps(false);
        assert!(!forget_caps_gesture());
    }

    #[test]
    fn caps_nx_repeat_keeps_the_same_source_gesture_without_a_synthetic_up() {
        forget_caps_gesture();
        capture_caps(true);
        let event = caps_aux(true);
        let data1 = CGEvent::integer_value_field(Some(&event), CGEventField(149));
        CGEvent::set_integer_value_field(Some(&event), CGEventField(149), data1 | 1);
        let translated = translate(CGEventType(sysevent::TYPE_SYSDEFINED), &event);
        assert_eq!(translated.release_first, None);
        let captured = translated.event.unwrap();
        assert!(captured.physical);
        assert_eq!(
            captured.event,
            InputEvent::Key {
                code: KeyCode::CAPS_LOCK,
                pressed: true
            }
        );
        capture_caps(false);
        assert!(!forget_caps_gesture());
    }

    #[test]
    fn caps_handoff_and_tap_restart_discard_old_gesture() {
        for _ in ["focus handoff", "tap recovery", "capture restart"] {
            capture_caps(true);
            KEY_DISPOSITION.with(|d| d.borrow_mut().insert(AUX_DISP | 4, Disposition::Swallow));
            assert!(forget_caps_gesture());
            assert!(!forget_caps_gesture());
            KEY_DISPOSITION.with(|d| assert!(!d.borrow().contains_key(&(AUX_DISP | 4))));
            // An UP from before the boundary remains an UP; it cannot arm a hold.
            assert_eq!(capture_caps(false).release_first, None);
            assert_eq!(capture_caps(true).release_first, None);
            capture_caps(false);
        }
    }

    #[test]
    fn synthetic_caps_flags_have_explicit_direction_without_learned_capability() {
        forget_caps_gesture();
        for pressed in [false, true, true, false] {
            let event = CGEvent::new_keyboard_event(None, keymap::MAC_CAPS_LOCK, pressed).unwrap();
            CGEvent::set_type(Some(&event), CGEventType::FlagsChanged);
            CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUnixProcessID, 1);
            CGEvent::set_flags(
                Some(&event),
                CGEventFlags(CGEventFlags::MaskAlphaShift.0 | if pressed { ALPHASHIFT_STATELESS } else { 0 }),
            );
            let translated = translate(CGEventType::FlagsChanged, &event);
            assert!(matches!(
                translated.event.as_ref().map(|e| &e.event),
                Some(InputEvent::Key { pressed: p, .. }) if *p == pressed
            ));
            track_keys(translated.event.as_ref().unwrap(), translated.disp_key);
        }
        assert!(!forget_caps_gesture());
    }

    #[test]
    fn caps_lock_is_not_reconciled() {
        assert_eq!(key_physically_down(KeyCode::keyboard(0x39)), None);
        // 其他键能查询（结果取决于当时的键盘，只检查有值）。
        assert!(key_physically_down(KeyCode::keyboard(0x6E)).is_some());
        assert!(button_physically_down(MouseButton::Other(9)).is_some());
    }

    #[test]
    fn mask_contains_key_and_mouse_types() {
        let m = event_mask();
        for t in [1u32, 5, 10, 11, 12, 22] {
            assert!(m & mask_bit(t) != 0);
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod brightness_tests {
    use super::*;
    #[test]
    fn brightness_down_up_and_repeat_are_never_reported() {
        for key in [2, 3] {
            for (pressed, repeat) in [(true, false), (true, true), (false, false)] {
                let ev = CGEvent::new(None).unwrap();
                CGEvent::set_type(Some(&ev), CGEventType(sysevent::TYPE_SYSDEFINED));
                CGEvent::set_integer_value_field(Some(&ev), CGEventField(83), 8);
                CGEvent::set_integer_value_field(
                    Some(&ev),
                    CGEventField(149),
                    keymap::AuxKey { key, pressed, repeat }.data1(),
                );
                let translated = translate_aux(&ev);
                assert!(translated.event.is_none());
                assert!(translated.disp_key.is_none());
                assert!(translated.repeat_of.is_none());
                assert!(translated.release_first.is_none());
                assert!(keymap::hid_to_nx(KeyCode::new(12, if key == 2 { 0x6f } else { 0x70 })).is_none());
            }
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod local_source_modes_tests {
    use super::*;
    #[test]
    fn caps_native_path_follows_keyboard_route_instead_of_cursor_capture() {
        // The hidden/shared mouse must not swallow a local keyboard's Caps flags.
        assert!(!native_swallow(false, false, false, true, false));
        // A local/visible mouse must still guard Caps forwarded by a shared keyboard.
        assert!(native_swallow(false, false, false, true, true));
        assert!(!native_swallow(false, true, true, false, true)); // NX preserves native path, guard restores state
        assert!(native_swallow(true, false, true, false, false)); // disabled remains independent
        assert!(native_swallow(false, true, false, false, false)); // hotkey/held-tail interception still works
    }
}

#[cfg(all(test, debug_assertions))]
mod owned_contact_capture_tests {
    use super::*;

    #[test]
    #[ignore = "requires macOS built-in trackpad and event-tap permission; run under the device lock"]
    fn owned_capture_installs_and_withdraws_native_contacts() {
        let shared: &'static Shared = Box::leak(Box::new(Shared {
            safety: Arc::new(crate::session_gate::SessionGate::default()),
            capture_epoch: AtomicU64::new(0),
            timing_epoch: AtomicU64::new(0),
            observations: None,
            physical_buttons: Arc::default(),
            motion_generation: 0,
            motion: Arc::default(),
            handler: Arc::new(|_| Disposition::Pass),
            gestures: Mutex::new(super::super::gesture_diagnostics::Tracker::default()),
            remote: AtomicBool::new(false),
            keyboard_remote: AtomicBool::new(false),
            hidden: Mutex::new(HiddenCursor::Shown),
            owner_generation: AtomicU64::new(0),
            drag_anchor: Mutex::new(None),
            owner_move: Mutex::new(None),
            owner_posts: Mutex::new(OwnerPosts::default()),
            passed_while_frozen: AtomicU64::new(0),
            park: Mutex::new(None),
            restore_pending: AtomicBool::new(false),
            hold_motion: AtomicBool::new(false),
            restore_x: AtomicU64::new(0),
            restore_y: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            pending: AtomicU8::new(0),
            port: AtomicPtr::new(std::ptr::null_mut()),
            run_loop: AtomicPtr::new(std::ptr::null_mut()),
            cursor_lock: Mutex::new(()),
            warp_fix: Mutex::new(WarpDeltaFix::new()),
            ordinary_buttons: Mutex::new(crate::ordinary_motion::ButtonGate::default()),
            pointer_transition: Mutex::new(crate::pointer_transition::Tracker::default()),
            freeze_check_ns: AtomicU64::new(0),
            caps_guard: LocalGuard::new(),
        }));
        let run_loop = CFRunLoop::current().unwrap();
        let timer = super::super::loop_timer::Timer::new(run_loop.clone()).unwrap();
        for _ in 0..2 {
            let capture = install_capture(shared, &run_loop, timer.wake()).expect("native session capture available");
            let contacts_installed = CONTACTS.with(|slot| slot.borrow().is_some());
            // Always withdraw even on the baseline failure; the assertion must
            // not leave an extra tap or native contact subscription installed.
            withdraw_capture(shared, &run_loop, capture);
            assert!(
                contacts_installed,
                "the product capture owner must subscribe to native contacts"
            );
            assert!(CONTACTS.with(|slot| slot.borrow().is_none()));
        }
        drop(timer);
    }
}
