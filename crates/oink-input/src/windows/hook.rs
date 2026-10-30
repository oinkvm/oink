//! 截获线程：WH_MOUSE_LL + WH_KEYBOARD_LL + Raw Input + 光标遮罩，全部在一个专用线程的消息循环里。
//!
//! 钩子回调由系统在本线程取消息时同步调用，必须在 `LowLevelHooksTimeout`（默认约 300 ms，上限 1 s）内返回，
//! 否则系统会跳过、多次超时后**静默卸载**钩子。所以回调里只做：读结构体、查表、`GetCursorPos`、调用
//! `EventHandler`（契约要求 < 0.5 ms）；线程优先级设为 HIGHEST；看门狗每秒检查一次，鼠标、键盘分开：
//! 可产生 LL 回调的设备 Raw Input 在动而对应的钩子超过 1 s 没被调用 → 重装那个钩子（[`hook_lost`]）。系统卸载钩子
//! 是一个一个卸的：键盘钩子丢了、鼠标钩子还在时，共用一个「最近调用」时间就发现不了（评审 W02；2026-09-30
//! Windows 上 Windows 控制 Mac 时按键全部留在本机，鼠标照常移交）。看门狗先核对 Raw Input 注册没被本进程的界面
//! 改走（见 [`super::rawinput`]，评审 W01）。
//!
//! 状态都在本线程的 thread_local 里，钩子回调不加锁。其他线程通过 `PostThreadMessage` + 通道发命令；
//! 在回调里（本线程）调用 `set_mode` 时直接生效，不经过消息。
//!
//! **模式切换与光标落点**（[`Transition`]）：
//! - 进入 `Remote`：立即采集、异步请求遮罩隐藏。普通停靠由隐藏 ACK 授权一个带代次的绝对移动；
//!   钩子校验并放行该输入，不另做 SetCursorPos。发送完成且读回后才缩罩并允许 raw clip。
//!   已有移动仍按对应的位移账本与旧坐标在途规则计算（[`crate::stale_step`]）。
//! - 从其他线程请求切换时，由调用方注入带序号的绝对移动标记。回调更新模式；回到指定本机落点的
//!   标记交给系统提交位置；普通 Remote 模式标记仍吞掉，等待后续 hide ACK 的停车输入。
//!   调用方在 `SendInput` 返回后通知钩子线程完成对应停车或撤罩。
//! - Windows 11 实测：即使在标记回调里，`SetCursorPos` 成功、立即读到落点之后，同一回调内再次读取
//!   也可能已回到停靠点，其间没有 Oink 的重新停靠调用。因此不能假定标记回调阻止所有系统输入更新。
//!   用标记本身提交位置，避免这次回调中的独立 warp；只放行标记却在回调里发撤罩消息也不够，会在系统
//!   提交位置之前露出停靠点。撤罩由调用方完成通知驱动，不重复定位、不加延时。
//!   The underlying Windows input update that restores the parked position is not identified.
//! - 标记必须由其他线程注入（本线程注入不经过自己的钩子）。迟到或被取代的标记必须吞掉，避免它
//!   提交旧落点；完成通知也核对序号。标记未到达时仍由定时器在 [`TRANSITION_FALLBACK_MS`] 后兜底。
//!
//! **回调里不做任何要等其他线程应答的调用**（跨线程的同步窗口调用、`SendMessage` 等）：等待期间系统会把
//! 新到的低级钩子消息交给本线程处理，钩子回调**重入**；外层还占着线程状态，重入的回调拿不到状态，只能把
//! 事件原样交给系统（没经过截获、路由看不到）。Windows 实测（2026-09-29，Windows 的鼠标在 Mac 与 Windows
//! 之间快速往返）：回到本机时撤掉遮罩窗口用的是同步的 `ShowWindow`，等待期间重入的那一步是系统按停靠点
//! 算的（`pt` = 停靠点 + 一步），被原样放行后光标跳回屏幕中央、之后从那里走，路由也以它为指针位置——用户
//! 看到的「光标从中心开始移动」；当时记录的「撤掉遮罩时光标被放回停靠点」也是它。所以遮罩窗口只用异步调用
//! （见 [`super::cursor`]）。万一仍然重入，记一条检测日志（`REENTERED`）。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

mod desktop_trace;
mod external;
#[cfg(all(test, debug_assertions))]
mod external_tests;
mod observations;
mod restore;

pub(crate) use external::physical_keys;

static CAPTURE_THREADS: AtomicUsize = AtomicUsize::new(0);
static CAPTURE_HOOKS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn capture_active() -> bool {
    CAPTURE_HOOKS.load(Ordering::Acquire) >= 2
}
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use oink_foundation::Point;
use oink_foundation::input::{
    CaptureMode, CapturedEvent, Disposition, InputEvent, KeyCode, MouseButton, ScrollPhase, ScrollUnit, SourceKind,
};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, SetLastError, WIN32_ERROR, WPARAM,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC_EX, MapVirtualKeyW};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCursorInfo, GetCursorPos, GetMessageW, GetSystemMetrics, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT, KillTimer,
    LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PostMessageW, PostQuitMessage,
    PostThreadMessageW, RegisterClassExW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_APP, WM_INPUT, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_TIMER, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSEXW,
};
use windows::core::w;

use super::cursor::{Hider, Strategy};
use super::dpi::ThreadDpiGuard;
use super::inject::{self, INJECT_TAG};
use super::keymap;
use super::rawinput::{self, RawTracker};
use crate::{EventHandler, InputError};

const WM_APP_CMD: u32 = WM_APP + 1;
/// 回到本机之后撤掉遮罩（`wparam` = 发出时的切换序号；见 [`State::execute`]）。
const WM_APP_UNCOVER: u32 = WM_APP + 2;
pub(super) const WM_APP_COVER_READY: u32 = WM_APP + 3;
pub(super) const WM_APP_COVER_HIDDEN: u32 = WM_APP + 4;
pub(super) const WM_APP_COVER_PLACED: u32 = WM_APP + 5;
const WM_APP_PARK_COMPLETE: u32 = WM_APP + 6;
/// 鼠标钩子回调重入、事件没经过截获就放行的次数（检测日志）。
static REENTERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const WATCHDOG_TIMER: usize = 1;
const MOUSE_KEYS_TIMER: usize = 5;
const TOUCHPAD_END_TIMER: usize = 3;
const WATCHDOG_MS: u32 = 1000;
/// Raw Input 有输入、而钩子超过这么久没被调用 → 认为钩子被卸载了。
const HOOK_SILENT: Duration = Duration::from_millis(1000);
/// 回调耗时超过这个值时记录警告（系统超时约 300 ms）。
const SLOW_HANDLER: Duration = Duration::from_millis(50);

pub(crate) type RestoreCompletion = Box<dyn FnOnce(bool) + Send>;

pub(crate) enum Command {
    DragAnchor(Option<oink_foundation::dnd::DragAnchor>, mpsc::SyncSender<bool>),
    RelativeCapture(bool),
    CursorConstraint {
        owned: windows::Win32::Foundation::RECT,
        previous: windows::Win32::Foundation::RECT,
        acquire: bool,
        reply: mpsc::SyncSender<Result<(), InputError>>,
    },
    SetMode(CaptureMode, mpsc::SyncSender<Option<Marker>>),
    RestoreLocal(Point, Option<RestoreCompletion>, mpsc::SyncSender<Option<Marker>>),
    Touchpad(
        (f64, f64),
        u32,
        oink_foundation::input::ScrollPhase,
        mpsc::SyncSender<(f64, f64)>,
    ),
    CancelTouchpad(mpsc::SyncSender<()>),
    Contacts(oink_foundation::contacts::Frame, mpsc::SyncSender<bool>),
    Stop,
}

pub(crate) fn apply_drag_anchor_here(anchor: Option<oink_foundation::dnd::DragAnchor>) -> bool {
    let valid = anchor.is_none_or(super::cursor::drag_anchor_ready);
    valid
        && with_state(|st| {
            st.drag_anchor = anchor;
            true
        })
        .unwrap_or(false)
}

/// 标记类型取 "ACRW" 的低 16 位；高 16 位是本次请求的票号（见模块说明）。
const TRANSITION_TAG: usize = 0x4F49_4E57;
// Windows 11 10.0.26200 delivered only the low 32 bits of SendInput's extra
// info to this hook, even in a 64-bit process. Keep both identity and ticket
// in those bits; a high-word ticket made every restore fall back to the timer.
const MARKER_TAG_BITS: u32 = 16;
const MARKER_TAG_MASK: usize = 0xffff;
static NEXT_MARKER: AtomicUsize = AtomicUsize::new(1);

pub(super) fn is_marker(extra: usize) -> bool {
    extra & MARKER_TAG_MASK == TRANSITION_TAG & MARKER_TAG_MASK
}

#[derive(Clone, Copy, Debug)]
struct PendingTransition {
    transition: Transition,
    marker: Marker,
}
const TRANSITION_TIMER: usize = 2;
/// 标记事件没有到达时，这么久之后直接执行切换。
const TRANSITION_FALLBACK_MS: u32 = 50;

/// 模式切换在哪里执行（检测日志）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Via {
    /// 在截获回调里请求，立即执行。
    InHook,
    /// 标记事件的钩子回调。
    Marker,
    /// 标记没有到达，兜底定时器（不在钩子回调里）。
    Timer,
}

/// 回到本机后观察的时长（检测日志）。
const RESTORE_WATCH: Duration = Duration::from_millis(1000);

/// 最近一次回到本机：之后的移动是否从落点出发（检测日志，不做纠正）。
#[derive(Debug)]
struct RestoreWatch {
    at: POINT,
    park: POINT,
    since: Instant,
    via: Via,
    /// 上一个经过钩子的本进程注入移动的位置（下一个移动之前光标应在这里）。
    last: POINT,
    moves: u32,
    reported: bool,
}

fn to_point(p: POINT) -> Point {
    Point::new(f64::from(p.x), f64::from(p.y))
}

/// 回到本机后观察到的异常（每次回到本机最多报告一次）。
#[derive(Clone, Copy, Debug, PartialEq)]
enum RestoreAnomaly {
    /// 一个移动生效之前，光标离上一次放置的位置（落点，或上一个移动的目标）很远：跳走了。
    /// （差一两步不算：上一步可能被钩子链里的其他程序或路由吞掉。）
    NotWherePlaced { expected: POINT, at_park: bool },
    /// 本机鼠标的一步（相对移动）的目标离光标很远：系统不是从光标所在处算这一步的。
    StepFromElsewhere { at_park: bool },
}

/// 本机鼠标一步的目标离光标超过这么远（物理像素）算不是从光标处出发（真实的一步远小于它）。
const STEP_FAR_PX: i32 = 300;

impl RestoreWatch {
    fn new(at: POINT, park: POINT, via: Via) -> Self {
        Self {
            at,
            park,
            since: Instant::now(),
            via,
            last: at,
            moves: 0,
            reported: false,
        }
    }

    /// 一个经过钩子的移动（本机模式下都会生效）：`cur` 它生效之前的光标位置，`pt` 它的目标位置，
    /// `relative` 是本机鼠标的相对移动（本进程注入的是绝对位置）。
    fn check(&mut self, cur: POINT, pt: POINT, relative: bool) -> Option<RestoreAnomaly> {
        let near = |a: POINT, b: POINT, d: i32| (a.x - b.x).abs() <= d && (a.y - b.y).abs() <= d;
        self.moves += 1;
        let expected = std::mem::replace(&mut self.last, pt);
        if self.reported {
            return None;
        }
        let anomaly = if !near(cur, expected, STEP_FAR_PX) {
            RestoreAnomaly::NotWherePlaced {
                expected,
                at_park: near(cur, self.park, 2),
            }
        } else if relative && !near(pt, cur, STEP_FAR_PX) {
            RestoreAnomaly::StepFromElsewhere {
                at_park: near(pt, self.park, STEP_FAR_PX),
            }
        } else {
            return None;
        };
        self.reported = true;
        Some(anomaly)
    }
}

/// 一次模式切换（坐标已裁剪到显示器内，物理像素）。
#[derive(Clone, Copy, Debug)]
enum Transition {
    /// 隐藏并停靠在这里。
    Remote(POINT),
    /// 回到 `Local`；带落点时先（隐藏状态下）把光标放过去。
    Local(Option<POINT>),
}

impl Transition {
    fn of_mode(mode: CaptureMode) -> Self {
        match mode {
            CaptureMode::Remote { park } => Self::Remote(clamp_to_monitor(to_pixel(park))),
            CaptureMode::Local => Self::Local(None),
        }
    }

    fn restore(at: Point) -> Self {
        Self::Local(Some(clamp_to_monitor(to_pixel(at))))
    }

    /// 标记事件移向的位置（事件会被吞掉，只影响钩子里看到的 `pt`）。
    fn marker_target(self) -> Option<POINT> {
        match self {
            Self::Remote(p) | Self::Local(Some(p)) => Some(p),
            Self::Local(None) => None,
        }
    }
}

/// 截获线程的句柄（在其他线程持有）。
#[derive(Debug)]
pub(crate) struct HookHandle {
    thread_id: u32,
    observer: Option<observations::Observer>,
    cmd: mpsc::Sender<Command>,
    done: mpsc::Receiver<()>,
    join: Option<JoinHandle<()>>,
}

/// 线程内状态的对外快照（诊断）。
#[derive(Clone, Debug, Default)]
pub(crate) struct Stats {
    pub hook_reinstalls: u64,
    pub slow_handler_calls: u64,
    pub touch_reports: u64,
    pub mouse_kbd_raw_reports: u64,
    pub cursor_hidden: bool,
}

struct State {
    drag_anchor: Option<oink_foundation::dnd::DragAnchor>,
    touchpad: super::touchpad::Touchpad,
    raw_capture: super::raw_capture::RawCapture,
    relative_requested: bool,
    handler: EventHandler,
    mode: CaptureMode,
    park: POINT,
    mouse_hook: Option<HHOOK>,
    kbd_hook: Option<HHOOK>,
    hider: Hider,
    park_commit: crate::park_commit::ParkCommit,
    park_marker: Option<(u64, Marker)>,
    park_acceptance: Option<super::warp::Acceptance>,
    /// 等标记事件到达后执行的切换。
    pending: Option<PendingTransition>,
    /// 最近一次回到本机（检测日志）。
    restore_watch: Option<RestoreWatch>,
    /// The native restore event has not yet returned from SendInput.
    restoring_from_marker: Option<POINT>,
    /// 模式切换的序号（推迟撤掉遮罩时核对：其间又切换过就不撤）。
    transitions: u64,
    local_restore: Option<restore::LocalRestore>,
    restore_complete: Option<(u64, RestoreCompletion)>,
    /// 本线程的消息窗口（兜底定时器）。
    hwnd: HWND,
    raw: RawTracker,
    /// 按下中的键 → 按下事件是否被吞掉（识别自动重复、保证本机不卡键）。
    keys: HashMap<u32, bool>,
    continuity: crate::capture_continuity::CaptureContinuity,
    /// 鼠标、键盘钩子最近一次被调用（看门狗分别核对）。
    last_mouse_hook: Instant,
    last_kbd_hook: Instant,
    /// 上一次核对 Raw Input 注册的结果（只在变化时记日志）。
    raw_registration_ok: bool,
    ordinary_observer: bool,
    capture_enabled: bool,
    /// 已经记过「看门狗收到了鼠标 / 键盘的 Raw Input」（各记一次，证明看门狗有数据可用）。
    raw_seen_logged: (bool, bool),
    cmd_rx: mpsc::Receiver<Command>,
    stats: Stats,
    pipeline: PipelineCounts,
    diagnostic_ticks: u64,
}

#[derive(Default)]
struct PipelineCounts {
    mouse_callbacks: u64,
    keyboard_callbacks: u64,
    key_captured: u64,
    key_passed: u64,
    key_swallowed: u64,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static IS_HOOK_THREAD: Cell<bool> = const { Cell::new(false) };
    /// 正在执行钩子回调（RIT 在等本线程）。
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// 看门狗分别核对的钩子。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookKind {
    Mouse,
    Keyboard,
}

/// 这类设备的钩子是否已被系统卸载：Raw Input 最近（[`WATCHDOG_MS`] 的两倍以内）收到了输入，而钩子在那之前
/// 已经超过 [`HOOK_SILENT`] 没被调用。钩子吞掉的事件 Raw Input 收不到、放行的事件钩子先于 Raw Input 看到，
/// 调用方先排除无设备、无动作以及 clip 下纯移动等不证明 LL 回调应当发生的报告。
/// 这仍是恢复启发式，Windows 不提供“钩子已被摘除”的查询。
fn hook_lost(now: Instant, raw: Option<Instant>, last_hook: Instant) -> bool {
    raw.is_some_and(|raw| {
        now.saturating_duration_since(raw) < Duration::from_millis(2 * u64::from(WATCHDOG_MS))
            && raw.saturating_duration_since(last_hook) > HOOK_SILENT
    })
}

pub(super) fn time_us(now: Instant) -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    now.saturating_duration_since(*BASE.get_or_init(Instant::now))
        .as_micros() as u64
}

pub(crate) fn on_hook_thread() -> bool {
    IS_HOOK_THREAD.with(Cell::get)
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| {
        let mut s = s.try_borrow_mut().ok()?;
        s.as_mut().map(f)
    })
}

/// Reentrant injection must use the owning hook state, never wait for its queue.
pub(crate) fn touchpad_here(
    pixels: (f64, f64),
    dpi: u32,
    phase: oink_foundation::input::ScrollPhase,
) -> Result<(f64, f64), InputError> {
    with_state(|st| {
        let remaining = if matches!(st.mode, CaptureMode::Local) && st.pending.is_none() {
            st.touchpad.scroll(pixels, dpi, phase)
        } else {
            st.touchpad.cancel_all();
            (0.0, 0.0)
        };
        // Only arm the short bridge timer while Ended is waiting for momentum.
        // No recurring high-frequency timer is needed during ordinary scrolling.
        // SAFETY: the window and its timers belong to this hook thread.
        unsafe {
            let _ = KillTimer(Some(st.hwnd), TOUCHPAD_END_TIMER);
            if st.touchpad.awaiting_momentum()
                && SetTimer(
                    Some(st.hwnd),
                    TOUCHPAD_END_TIMER,
                    crate::touchpad::MOMENTUM_GRACE.as_millis() as u32,
                    None,
                ) == 0
            {
                st.touchpad.finish_scroll();
            }
        }
        remaining
    })
    .ok_or(InputError::NotStarted)
}
pub(crate) fn contacts_here(frame: &oink_foundation::contacts::Frame) -> bool {
    with_state(|st| {
        if matches!(st.mode, CaptureMode::Local) && st.pending.is_none() && st.continuity.available() {
            st.touchpad.contacts(frame)
        } else {
            st.touchpad.cancel_all();
            if frame.phase == oink_foundation::contacts::Phase::Begin {
                // A rejected Begin never reaches the native receiver's summary.
                // Keep the admission result observable without recording fingers.
                let reason = if !matches!(st.mode, CaptureMode::Local) {
                    1
                } else if st.pending.is_some() {
                    2
                } else {
                    3
                };
                crate::native_lifecycle::emit_lifecycle(
                    crate::native_lifecycle::Kind::ContactInjection,
                    crate::native_lifecycle::Data {
                        reason,
                        success: Some(false),
                        ..Default::default()
                    },
                );
            }
            false
        }
    })
    .unwrap_or(false)
}

pub(crate) fn cancel_touchpad_here() {
    with_state(|st| st.touchpad.cancel_all());
}

/// 在截获线程上切换模式：钩子回调里立即执行；否则等标记事件（见模块说明），标记由临时线程注入。
pub(crate) fn apply_mode_here(mode: CaptureMode) -> Result<(), InputError> {
    request_here(Transition::of_mode(mode))
}

/// 在截获线程上回到 `Local` 并把光标放到 `at`（执行时机同 [`apply_mode_here`]）。
pub(crate) fn restore_local_here(at: Point, complete: Option<RestoreCompletion>) -> Result<(), InputError> {
    let marker = with_state(|st| st.request_restore(at, complete)).ok_or(InputError::NotStarted)?;
    if let Some(marker) = marker {
        dispatch_marker(marker);
    }
    Ok(())
}

fn request_here(t: Transition) -> Result<(), InputError> {
    let marker = with_state(|st| st.request(t)).ok_or(InputError::NotStarted)?;
    if let Some(target) = marker {
        dispatch_marker(target);
    }
    Ok(())
}

// Windows 11 does not call this thread's own LL hook for its SendInput.
// Use the existing marker sender thread; never wait for that hook from its owner.
fn dispatch_marker(marker: Marker) {
    if let Err(error) = std::thread::Builder::new()
        .name("oink-input-marker".into())
        .spawn(move || send_marker(marker))
    {
        park_failure(
            marker,
            crate::native_lifecycle::ParkInputFailure::SenderUnavailable,
            error.raw_os_error().unwrap_or(0) as u32,
            None,
        );
        marker_completed(marker, false);
        tracing::warn!(%error, "native input marker could not be scheduled");
    }
}

/// 钩子回调期间置位。
struct InHook;

impl InHook {
    fn enter() -> Self {
        IN_HOOK.with(|c| c.set(true));
        Self
    }
}

impl Drop for InHook {
    fn drop(&mut self) {
        IN_HOOK.with(|c| c.set(false));
    }
}

impl HookHandle {
    pub(crate) fn contacts(&self, frame: oink_foundation::contacts::Frame) -> Result<bool, InputError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.post(Command::Contacts(frame, tx))?;
        rx.recv().map_err(|_| InputError::NotStarted)
    }
    pub(crate) fn touchpad(
        &self,
        pixels: (f64, f64),
        dpi: u32,
        phase: oink_foundation::input::ScrollPhase,
    ) -> Result<(f64, f64), InputError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.post(Command::Touchpad(pixels, dpi, phase, tx))?;
        rx.recv().map_err(|_| InputError::NotStarted)
    }

    pub(crate) fn cancel_touchpad(&self) -> Result<(), InputError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.post(Command::CancelTouchpad(tx))?;
        rx.recv().map_err(|_| InputError::NotStarted)
    }

    pub(crate) fn set_relative_capture(&self, enabled: bool) -> Result<(), InputError> {
        self.post(Command::RelativeCapture(enabled))
    }
    pub(crate) fn spawn(handler: EventHandler) -> Result<Self, InputError> {
        Self::spawn_inner(handler, None)
    }
    pub(crate) fn spawn_loop(owner: Box<dyn crate::capture_loop::CaptureLoop>) -> Result<Self, InputError> {
        Self::spawn_inner(std::sync::Arc::new(crate::capture_loop::event), Some(owner))
    }
    fn spawn_inner(
        handler: EventHandler,
        owner: Option<Box<dyn crate::capture_loop::CaptureLoop>>,
    ) -> Result<Self, InputError> {
        let _ = time_us(Instant::now());
        // Initialise QPC frequency before callbacks can run; a hook must never
        // wait behind another thread's first clock initialisation.
        let _ = crate::source_time::capture_time();
        let observer = owner
            .as_ref()
            .map(|_| observations::Observer::start())
            .transpose()
            .map_err(|e| InputError::Platform(format!("create input observer: {e}")))?;
        let observations = observer.as_ref().map(|o| o.state.clone());
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
        let (done_tx, done_rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("oink-input-hook".into())
            .spawn(move || {
                CAPTURE_THREADS.fetch_add(1, Ordering::AcqRel);
                run(handler, cmd_rx, ready_tx, owner, observations);
                // run removes both LL hooks, Raw Input and owner windows before
                // acknowledging stop. A timed-out stop must not claim detachment.
                CAPTURE_THREADS.fetch_sub(1, Ordering::AcqRel);
                let _ = done_tx.send(());
            })
            .map_err(|e| InputError::Platform(format!("创建截获线程失败：{e}")))?;
        let thread_id = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| InputError::Platform("截获线程启动超时".into()))?
            .map_err(InputError::Platform)?;
        Ok(Self {
            thread_id,
            observer,
            cmd: cmd_tx,
            done: done_rx,
            join: Some(join),
        })
    }

    fn post(&self, cmd: Command) -> Result<(), InputError> {
        self.cmd.send(cmd).map_err(|_| InputError::NotStarted)?;
        // SAFETY: 向截获线程投递一条线程消息，唤醒它处理命令。
        unsafe { PostThreadMessageW(self.thread_id, WM_APP_CMD, WPARAM(0), LPARAM(0)) }
            .map_err(|e| InputError::Platform(format!("PostThreadMessageW: {e}")))
    }

    /// Confinement's helper handshake runs on the injection worker, while the
    /// cursor mutation and thread-local warp accounting stay on the hook owner.
    pub(crate) fn cursor_constraint(
        &self,
        owned: windows::Win32::Foundation::RECT,
        previous: windows::Win32::Foundation::RECT,
        acquire: bool,
    ) -> Result<(), InputError> {
        let (reply, received) = mpsc::sync_channel(1);
        self.post(Command::CursorConstraint {
            owned,
            previous,
            acquire,
            reply,
        })?;
        received
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| InputError::Platform("cursor confinement owner did not acknowledge".into()))?
    }

    pub(crate) fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.post(Command::SetMode(mode, tx))?;
        Self::finish_request(&rx)
    }

    pub(crate) fn set_drag_anchor(&self, anchor: Option<oink_foundation::dnd::DragAnchor>) -> Result<(), InputError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.post(Command::DragAnchor(anchor, tx))?;
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(true) => Ok(()),
            _ => Err(InputError::Platform(
                "native drag catcher is not current or input owner did not acknowledge".into(),
            )),
        }
    }

    pub(crate) fn restore_local(&self, at: Point, complete: Option<RestoreCompletion>) -> Result<(), InputError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.post(Command::RestoreLocal(at, complete, tx))?;
        Self::finish_request(&rx)
    }

    /// 等截获线程记下切换，然后在**调用方线程**注入标记事件：同一线程之后的注入（例如 dispatcher 随后的
    /// `MoveTo`）在系统输入队列里排在标记之后，切换与注入的先后与路由的输出顺序一致。
    fn finish_request(rx: &mpsc::Receiver<Option<Marker>>) -> Result<(), InputError> {
        let marker = rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| InputError::Platform("截获线程无响应".into()))?;
        if let Some(target) = marker {
            send_marker(target);
        }
        Ok(())
    }

    /// 停止截获线程（卸载钩子、恢复光标），等待其退出（最多 2 秒）。
    pub(crate) fn stop(mut self) {
        let _ = self.post(Command::Stop);
        if on_hook_thread() {
            return; // 在回调里停止：线程在回调返回后自行退出
        }
        if self.done.recv_timeout(Duration::from_secs(2)).is_ok() {
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        } else {
            tracing::warn!("截获线程 2 秒内没有退出");
        }
        self.observer.take();
    }
}

fn report_capture_install_result(installed: bool, desktop_available: bool) {
    if installed && !desktop_available {
        crate::capture_loop::capture_detached();
    } else {
        crate::capture_loop::capture_available(installed);
    }
}

fn report_capture_desktop(available: bool, desktop_changed: bool) {
    // UAC's secure desktop intentionally withdraws this desktop's source.
    // Keep the routing/key-release boundary without reporting a failed hook
    // installation. Hook installation failures use capture_available(false).
    if desktop_changed || !available {
        crate::capture_loop::capture_detached();
    }
    if available {
        crate::capture_loop::capture_available(true);
    }
}

fn run(
    handler: EventHandler,
    cmd_rx: mpsc::Receiver<Command>,
    ready: mpsc::Sender<Result<u32, String>>,
    owner: Option<Box<dyn crate::capture_loop::CaptureLoop>>,
    observations: Option<std::sync::Arc<observations::State>>,
) {
    IS_HOOK_THREAD.with(|c| c.set(true));
    let mouse_keys = super::mouse_keys::Worker::new();
    let _dpi = ThreadDpiGuard::per_monitor_v2();
    // Numeric scheduling/status records leave through snapshots; formatting must never
    // run on this realtime capture owner, including inside the unchanged router code.
    let _quiet = owner
        .as_ref()
        .map(|_| tracing::subscriber::set_default(tracing::subscriber::NoSubscriber::default()));
    let _ = oink_foundation::realtime::apply(
        oink_foundation::realtime::ThreadRole::Input,
        oink_foundation::realtime::Tier::Realtime,
    );
    let pacing = if let Some(owner) = owner {
        match super::loop_timer::Timer::new() {
            Ok(timer) => {
                crate::capture_loop::install(owner, timer.wake());
                Some(timer)
            }
            Err(error) => {
                let _ = ready.send(Err(error.to_string()));
                return;
            }
        }
    } else {
        None
    };
    // SAFETY: 以下均为本线程内的标准 Win32 调用；窗口过程与钩子回调只访问本线程的 thread_local。
    unsafe {
        let hinstance = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready.send(Err(format!("GetModuleHandleW: {e}")));
                return;
            }
        };
        let class = w!("OinkInputWindow");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            let _ = ready.send(Err(format!("RegisterClassExW: {:?}", GetLastError())));
            return;
        }
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Oink input"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready.send(Err(format!("CreateWindowExW: {e}")));
                return;
            }
        };
        if pacing.is_none()
            && let Err(e) = rawinput::register(hwnd)
        {
            tracing::warn!("注册 Raw Input 失败（无法区分触摸板、看门狗失效）：{e}");
        }
        let hider = Hider::new(Strategy::from_env());
        let mut cur = POINT::default();
        let _ = GetCursorPos(&mut cur);
        STATE.with(|s| {
            *s.borrow_mut() = Some(State {
                touchpad: super::touchpad::Touchpad::new(),
                raw_capture: super::raw_capture::RawCapture::default(),
                relative_requested: false,
                drag_anchor: None,
                handler,
                mode: CaptureMode::Local,
                park: cur,
                mouse_hook: None,
                kbd_hook: None,
                hider,
                park_commit: Default::default(),
                park_marker: None,
                park_acceptance: None,
                pending: None,
                restore_watch: None,
                restoring_from_marker: None,
                transitions: 0,
                local_restore: None,
                restore_complete: None,
                hwnd,
                raw: RawTracker::default(),
                keys: HashMap::new(),
                continuity: Default::default(),
                last_mouse_hook: Instant::now(),
                last_kbd_hook: Instant::now(),
                raw_registration_ok: true,
                ordinary_observer: observations.is_some(),
                capture_enabled: pacing.is_none(),
                raw_seen_logged: (false, false),
                cmd_rx,
                stats: Stats::default(),
                pipeline: PipelineCounts::default(),
                diagnostic_ticks: 0,
            })
        });
        let _desktop_trace = desktop_trace::Observer::new(hwnd);
        capture_availability_changed(false);
        if pacing.is_none()
            && let Some(Err(e)) = with_state(|st| st.install_hooks())
        {
            STATE.with(|s| s.borrow_mut().take());
            rawinput::unregister();
            let _ = DestroyWindow(hwnd);
            let _ = ready.send(Err(e));
            return;
        }
        if let Some(observations) = &observations {
            observations.set_window(Some(hwnd));
        }
        SetTimer(Some(hwnd), WATCHDOG_TIMER, WATCHDOG_MS, None);
        mouse_keys.sharing(pacing.is_none());
        let _ = ready.send(Ok(GetCurrentThreadId()));

        let mut msg = MSG::default();
        let mut capture_retry = Instant::now();
        let mut resume_hotkey = super::resume_hotkey::Manager::default();
        loop {
            if let Some(timer) = &pacing {
                timer.begin_turn();
                let now = Instant::now();
                let (mut due, stopped) = crate::capture_loop::poll(now);
                if stopped {
                    break;
                }
                resume_hotkey.update(crate::capture_loop::paused_hotkey());
                let requested = crate::capture_loop::capture_requested();
                mouse_keys.sharing(requested);
                let mut capture_attempt = None;
                let (active, desktop_available) = with_state(|st| {
                    if requested && !st.capture_enabled && now >= capture_retry {
                        st.capture_enabled = st.install_hooks().is_ok();
                        capture_attempt = Some(st.capture_enabled);
                        capture_retry = now + Duration::from_secs(2);
                        if st.capture_enabled {
                            let _ = rawinput::register(hwnd);
                            if let Some(observations) = &observations {
                                observations.set_capturing(true);
                            }
                        }
                    } else if !requested {
                        capture_retry = now;
                        if st.capture_enabled {
                            if let Some(observations) = &observations {
                                observations.set_capturing(false);
                            }
                            st.capture_enabled = false;
                            st.cancel_restore();
                            st.transitions += 1;
                            st.pending = None;
                            st.park_marker = None;
                            st.park_acceptance = None;
                            st.park_commit.cancel();
                            st.hider.cancel_park();
                            st.hider.show();
                            st.mode = CaptureMode::Local;
                            st.relative_requested = false;
                            st.drag_anchor = None;
                            st.restore_watch = None;
                            st.restoring_from_marker = None;
                            st.raw_capture.update(false, st.park);
                            st.touchpad.cancel_all();
                            st.uninstall_hooks();
                            rawinput::unregister();
                            st.keys.clear();
                            external::gap();
                        }
                    }
                    (st.capture_enabled, st.continuity.available())
                })
                .unwrap_or((false, false));
                if let Some(installed) = capture_attempt {
                    // Publish initial attachment and every retry, as the macOS
                    // capture owner does. An unchanged Default desktop emits no
                    // desktop-change callback; without this the keyboard health
                    // snapshot stays unknown even while its hook works. Notify
                    // outside STATE's borrow because the owner can call back into
                    // native input. Intentional withdrawal is not a failure.
                    report_capture_install_result(installed, desktop_available);
                }
                if requested && !active {
                    due = Some(due.map_or(capture_retry, |due| due.min(capture_retry)));
                }
                if timer.arm(due).and_then(|()| timer.wait()).is_err() {
                    break;
                }
                use windows::Win32::UI::WindowsAndMessaging::{PM_REMOVE, PeekMessageW, WM_QUIT};
                if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    continue;
                }
                if msg.message == WM_QUIT {
                    break;
                }
            } else if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                break;
            }
            let t0 = Instant::now();
            if msg.message == windows::Win32::UI::WindowsAndMessaging::WM_HOTKEY
                && msg.wParam.0 == super::resume_hotkey::ID as usize
            {
                crate::capture_loop::resume_hotkey();
            } else if msg.hwnd.is_invalid() && msg.message == WM_APP_CMD {
                process_commands();
            } else {
                DispatchMessageW(&msg);
            }
            report_busy("message", msg.message, t0.elapsed());
        }

        if let Some(observations) = &observations {
            observations.set_window(None);
        }
        if pacing.is_some() {
            crate::capture_loop::clear();
        }
        drop(_desktop_trace);
        // 清理：卸载钩子、恢复光标、注销 Raw Input
        let _ = KillTimer(Some(hwnd), WATCHDOG_TIMER);
        let _ = KillTimer(Some(hwnd), TOUCHPAD_END_TIMER);
        if let Some(mut st) = STATE.with(|s| s.borrow_mut().take()) {
            st.cancel_restore();
            st.raw_capture.update(false, st.park);
            st.uninstall_hooks();
            st.hider.show();
            let stats = st.snapshot();
            tracing::debug!(
                hook_reinstalls = stats.hook_reinstalls,
                slow_handler_calls = stats.slow_handler_calls,
                touch_reports = stats.touch_reports,
                mouse_kbd_raw_reports = stats.mouse_kbd_raw_reports,
                cursor_hidden = stats.cursor_hidden,
                hide = ?st.hider.strategy(),
                "截获线程退出"
            );
            drop(st);
        }
        external::gap();
        rawinput::unregister();
        let _ = DestroyWindow(hwnd);
    }
}

fn process_commands() {
    loop {
        let cmd = with_state(|st| st.cmd_rx.try_recv().ok()).flatten();
        match cmd {
            Some(Command::Contacts(frame, reply)) => {
                let _ = reply.send(contacts_here(&frame));
            }
            Some(Command::Touchpad(pixels, dpi, phase, reply)) => {
                let remaining = touchpad_here(pixels, dpi, phase).unwrap_or((1.0, 1.0));
                let _ = reply.send(remaining);
            }
            Some(Command::CancelTouchpad(reply)) => {
                with_state(|st| st.touchpad.cancel_all());
                let _ = reply.send(());
            }
            Some(Command::DragAnchor(anchor, reply)) => {
                let _ = reply.send(apply_drag_anchor_here(anchor));
            }
            Some(Command::RelativeCapture(enabled)) => {
                with_state(|st| {
                    st.relative_requested = enabled;
                    st.update_raw_capture();
                });
            }
            Some(Command::CursorConstraint {
                owned,
                previous,
                acquire,
                reply,
            }) => {
                let result = if acquire {
                    super::cursor_clip::acquire_confinement(owned, previous)
                        .map_err(|e| InputError::Platform(e.to_string()))
                } else if super::cursor_clip::restore(previous, owned) {
                    Ok(())
                } else {
                    Err(InputError::Platform("cursor confinement restore failed".into()))
                };
                let _ = reply.send(result);
            }
            Some(Command::SetMode(mode, reply)) => {
                let marker = with_state(|st| st.request(Transition::of_mode(mode))).flatten();
                let _ = reply.send(marker);
            }
            Some(Command::RestoreLocal(at, complete, reply)) => {
                let marker = with_state(|st| st.request_restore(at, complete)).flatten();
                let _ = reply.send(marker);
            }
            Some(Command::Stop) => {
                with_state(|st| {
                    st.cancel_restore();
                    st.touchpad.cancel_all();
                    st.raw_capture.update(false, st.park);
                    st.pending = None;
                    st.mode = CaptureMode::Local;
                    st.hider.show();
                });
                // SAFETY: 结束本线程的消息循环。
                unsafe { PostQuitMessage(0) };
            }
            None => break,
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        windows::Win32::UI::WindowsAndMessaging::WM_DEVICECHANGE
        | windows::Win32::UI::WindowsAndMessaging::WM_INPUT_DEVICE_CHANGE => {
            super::mouse_keys::edge(crate::mouse_keys::Reason::DeviceChange);
            // Device removal can finish asynchronously after the notification.
            // SAFETY: timer belongs to this message window.
            unsafe {
                SetTimer(Some(hwnd), MOUSE_KEYS_TIMER, 50, None);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == MOUSE_KEYS_TIMER => {
            // SAFETY: this window owns the one-shot device timer.
            unsafe {
                let _ = KillTimer(Some(hwnd), MOUSE_KEYS_TIMER);
            }
            super::mouse_keys::edge(crate::mouse_keys::Reason::DeviceChange);
            LRESULT(0)
        }
        desktop_trace::WM_CAPTURE_DESKTOP => {
            capture_availability_changed(true);
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_WTSSESSION_CHANGE => {
            use windows::Win32::UI::WindowsAndMessaging::{WTS_SESSION_LOCK, WTS_SESSION_UNLOCK};
            if wparam.0 as u32 == WTS_SESSION_LOCK {
                desktop_trace::session_changed(WTS_SESSION_LOCK, true);
                capture_availability_changed(false);
            }
            if wparam.0 as u32 == WTS_SESSION_UNLOCK {
                desktop_trace::session_changed(WTS_SESSION_UNLOCK, false);
                capture_availability_changed(false);
            }
            LRESULT(0)
        }
        WM_INPUT => {
            let _source_time = crate::source_time::CaptureScope::entry();
            let now = Instant::now();
            let report = with_state(|st| {
                if !st.capture_enabled || !st.continuity.available() {
                    return None;
                }
                st.raw.on_input(
                    lparam,
                    now,
                    st.raw_capture.allows_move(),
                    matches!(st.mode, CaptureMode::Remote { .. }),
                );
                st.raw.motion.take().map(|(dx, dy)| {
                    (
                        st.handler.clone(),
                        CapturedEvent {
                            physical: true,
                            event: InputEvent::RawPointerDelta { dx, dy },
                            source: SourceKind::Mouse,
                            time_us: time_us(now),
                            cursor: None,
                        },
                    )
                })
            })
            .flatten();
            if let Some((handler, ev)) = report {
                handler(&ev);
            }
            // SAFETY: 文档要求 WM_INPUT 交给 DefWindowProc 做清理。
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_APP_COVER_PLACED => {
            with_state(|st| {
                if matches!(st.mode, CaptureMode::Remote { .. }) && st.pending.is_none() {
                    st.hider.refresh_shape(wparam.0 as u64);
                }
            });
            LRESULT(0)
        }
        WM_APP_COVER_READY => {
            if let Some(marker) = with_state(|st| st.cover_acknowledged(wparam.0 as u64)).flatten() {
                dispatch_marker(marker);
            }
            LRESULT(0)
        }
        WM_APP_PARK_COMPLETE => {
            with_state(|st| st.finish_park_input(wparam.0, lparam.0 != 0));
            LRESULT(0)
        }
        WM_APP_COVER_HIDDEN => {
            with_state(|st| {
                st.hider.finish_show(wparam.0 as u64);
                st.check_restore_completion();
            });
            LRESULT(0)
        }
        restore::MESSAGE => {
            with_state(|st| st.reset_restore(wparam.0 as u64, 1));
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == restore::TIMER => {
            with_state(State::restore_deadline);
            LRESULT(0)
        }
        WM_APP_UNCOVER => {
            with_state(|st| {
                st.uncover(wparam.0 as u64);
            });
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TOUCHPAD_END_TIMER => {
            with_state(|st| {
                st.touchpad.expire();
                if !st.touchpad.awaiting_momentum() {
                    // SAFETY: timer belongs to this hook thread/window.
                    unsafe {
                        let _ = KillTimer(Some(hwnd), TOUCHPAD_END_TIMER);
                    }
                }
            });
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == WATCHDOG_TIMER => {
            with_state(|st| st.touchpad.expire());
            with_state(State::watchdog);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TRANSITION_TIMER => {
            // SAFETY: 本线程的窗口与定时器。
            unsafe {
                let _ = KillTimer(Some(hwnd), TRANSITION_TIMER);
            }
            with_state(|st| {
                if let Some(pending) = st.pending.take() {
                    let t = pending.transition;
                    tracing::info!(
                        ?t,
                        "mode-switch marker did not arrive; switching from the fallback timer"
                    );
                    st.execute(t, Via::Timer);
                }
            });
            LRESULT(0)
        }
        // SAFETY: 其余消息交给默认处理。
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 钩子回调准备好的结果。
enum Prepared {
    Pass,
    Swallow,
    Event {
        event: CapturedEvent,
        handler: EventHandler,
        force_swallow: bool,
        force_pass: bool,
        /// 按键按下：回调后记录是否被吞掉。
        track_key: Option<u32>,
    },
}

impl State {
    fn snapshot(&self) -> Stats {
        Stats {
            touch_reports: self.raw.touch_reports,
            mouse_kbd_raw_reports: self.raw.mouse_kbd_reports,
            cursor_hidden: self.hider.is_hidden(),
            ..self.stats.clone()
        }
    }

    fn install_hooks(&mut self) -> Result<(), String> {
        self.install_hook(HookKind::Mouse)?;
        if let Err(e) = self.install_hook(HookKind::Keyboard) {
            self.uninstall_hook(HookKind::Mouse);
            return Err(e);
        }
        Ok(())
    }

    fn install_hook(&mut self, kind: HookKind) -> Result<(), String> {
        // WH_KEYBOARD_LL is desktop-scoped (Microsoft's Desktops/SetWindowsHookEx
        // contract). UIAccess lifts UIPI on Default, not Winlogon's desktop ACL.
        // This thread never switches desktops; the SYSTEM helper injects only and
        // installs no hook. Secure credential keys therefore cannot reach this hook.

        // SAFETY: 全局低级钩子：hmod 为本模块，线程 id 为 0；回调是本模块里的 extern "system" 函数。
        unsafe {
            let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            match kind {
                HookKind::Mouse => {
                    let h = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(hinstance.into()), 0)
                        .map_err(|e| format!("SetWindowsHookExW(WH_MOUSE_LL): {e}"))?;
                    self.mouse_hook = Some(h);
                    CAPTURE_HOOKS.fetch_add(1, Ordering::AcqRel);
                    self.last_mouse_hook = Instant::now();
                }
                HookKind::Keyboard => {
                    let h = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), Some(hinstance.into()), 0)
                        .map_err(|e| format!("SetWindowsHookExW(WH_KEYBOARD_LL): {e}"))?;
                    self.kbd_hook = Some(h);
                    CAPTURE_HOOKS.fetch_add(1, Ordering::AcqRel);
                    self.last_kbd_hook = Instant::now();
                }
            }
        }
        super::trace::action(
            crate::native_lifecycle::Kind::HookInstall,
            None,
            kind as u32,
            Some(true),
        );
        Ok(())
    }

    fn uninstall_hook(&mut self, kind: HookKind) {
        let h = match kind {
            HookKind::Mouse => self.mouse_hook.take(),
            HookKind::Keyboard => self.kbd_hook.take(),
        };
        if let Some(h) = h {
            CAPTURE_HOOKS.fetch_sub(1, Ordering::AcqRel);
            // SAFETY: 句柄由 install_hook 取得，只卸载一次（系统已经静默卸载时这里返回失败，无害）。
            let result = unsafe { UnhookWindowsHookEx(h) };
            super::trace::action(
                crate::native_lifecycle::Kind::HookRemove,
                None,
                kind as u32,
                Some(result.is_ok()),
            );
        }
    }

    fn uninstall_hooks(&mut self) {
        self.uninstall_hook(HookKind::Mouse);
        self.uninstall_hook(HookKind::Keyboard);
    }

    fn watchdog(&mut self) {
        external::report(
            matches!(self.mode, CaptureMode::Remote { .. }),
            self.raw_capture.allows_move(),
        );
        if !self.capture_enabled {
            return;
        }
        let now = Instant::now();
        // Raw Input 注册被本进程的其他代码（界面的 tao 事件循环）改走时，看门狗收不到任何设备输入（W01）。
        if !self.ordinary_observer {
            let reg = rawinput::registration_ok(self.hwnd);
            if reg.is_ok() != self.raw_registration_ok {
                super::trace::action(crate::native_lifecycle::Kind::Registration, None, 0, Some(reg.is_ok()));
                self.raw_registration_ok = reg.is_ok();
                if let Err(found) = &reg {
                    tracing::warn!(
                        ours = self.hwnd.0 as isize,
                        ?found,
                        "Raw Input registration for mouse/keyboard was taken over by other code in this process; re-registering"
                    );
                }
            }
            if reg.is_err() {
                if let Err(e) = rawinput::register(self.hwnd) {
                    tracing::warn!("重新注册 Raw Input 失败：{e}");
                } else {
                    self.raw_registration_ok = true;
                }
            }
        }
        if !self.raw_seen_logged.0 && self.raw.last_mouse.is_some() {
            self.raw_seen_logged.0 = true;
            tracing::info!("watchdog: receiving mouse Raw Input");
        }
        if !self.raw_seen_logged.1 && self.raw.last_kbd.is_some() {
            self.raw_seen_logged.1 = true;
            tracing::info!("watchdog: receiving keyboard Raw Input");
        }
        for (kind, raw, last) in [
            (HookKind::Mouse, self.raw.last_mouse_hook_evidence, self.last_mouse_hook),
            (HookKind::Keyboard, self.raw.last_kbd, self.last_kbd_hook),
        ] {
            if hook_lost(now, raw, last) {
                tracing::warn!(
                    ?kind,
                    hook_silent_ms = now.saturating_duration_since(last).as_millis() as u64,
                    "low-level hook silent despite eligible device input; reinstalling suspected lost hook"
                );
                super::trace::action(crate::native_lifecycle::Kind::HookReinstall, None, kind as u32, None);
                self.uninstall_hook(kind);
                if let Err(e) = self.install_hook(kind) {
                    tracing::error!("重新安装钩子失败：{e}");
                }
                self.stats.hook_reinstalls += 1;
            }
        }
        self.diagnostic_ticks += 1;
        let counts = std::mem::take(&mut self.pipeline);
        if counts.mouse_callbacks + counts.keyboard_callbacks != 0 || self.diagnostic_ticks.is_multiple_of(5) {
            tracing::debug!(
                remote = matches!(self.mode, CaptureMode::Remote { .. }),
                clipped = self.raw_capture.allows_move(),
                mouse_callbacks = counts.mouse_callbacks,
                keyboard_callbacks = counts.keyboard_callbacks,
                key_captured = counts.key_captured,
                key_passed = counts.key_passed,
                key_swallowed = counts.key_swallowed,
                captured_held = self.keys.len(),
                raw_mouse_reports = self.raw.mouse_reports,
                raw_without_device = self.raw.mouse_without_device,
                raw_keyboard_reports = self.raw.keyboard_reports,
                hook_reinstalls = self.stats.hook_reinstalls,
                "Windows input pipeline (callbacks per interval; raw reports cumulative)"
            );
        }
    }

    /// 请求一次模式切换：钩子回调里立即执行，返回 `None`；否则记下，返回应注入的标记事件，
    /// 在标记的钩子回调里执行（见模块说明）。标记必须由其他线程注入。
    fn request(&mut self, t: Transition) -> Option<Marker> {
        self.cancel_restore();
        if !self.capture_enabled {
            self.pending = None;
            self.execute(Transition::Local(None), Via::Timer);
            return None;
        }
        self.transitions += 1;
        if crate::native_lifecycle::lifecycle_enabled() {
            crate::native_lifecycle::emit(
                crate::native_lifecycle::Kind::ModeRequest,
                crate::native_lifecycle::Data {
                    extra: self.transitions,
                    remote: Some(matches!(t, Transition::Remote(_))),
                    point: t.marker_target().map(|p| [p.x, p.y]),
                    reason: u32::from(!IN_HOOK.with(Cell::get)),
                    ..crate::native_lifecycle::Data::default()
                },
            );
        }
        if matches!(t, Transition::Local(_)) {
            self.hider.cancel_park();
            self.park_marker = None;
            self.park_acceptance = None;
            self.park_commit.cancel();
        }

        if matches!(t, Transition::Remote(_)) {
            self.touchpad.cancel_all();
        }
        if IN_HOOK.with(Cell::get) {
            self.pending = None;
            self.execute(t, Via::InHook);
            return None;
        }
        // Release our game clip before Windows calculates the absolute marker
        // target. Releasing it in the callback would leave info.pt clipped to park.
        if matches!(t, Transition::Local(_)) {
            self.raw_capture.update(false, self.park);
        }
        let marker = Marker {
            target: match (t, self.drag_anchor) {
                (Transition::Remote(_), Some(anchor)) => to_pixel(anchor.at),
                _ => t.marker_target().unwrap_or_else(cursor_pos),
            },
            sequence: self.transitions,
            extra: ((NEXT_MARKER.fetch_add(1, Ordering::Relaxed) & MARKER_TAG_MASK) << MARKER_TAG_BITS)
                | (TRANSITION_TAG & MARKER_TAG_MASK),
            window: self.hwnd.0 as isize,
            park_generation: None,
        };
        self.pending = Some(PendingTransition { transition: t, marker });
        if !self.hwnd.is_invalid() {
            // SAFETY: 本线程的窗口；同 id 的定时器重复设置时重新计时。
            unsafe {
                SetTimer(Some(self.hwnd), TRANSITION_TIMER, TRANSITION_FALLBACK_MS, None);
            }
        }
        Some(marker)
    }

    /// 执行模式切换（在钩子回调里调用；兜底定时器除外）。
    fn execute(&mut self, t: Transition, via: Via) {
        if matches!(t, Transition::Local(_)) {
            super::mouse_keys::edge(crate::mouse_keys::Reason::Local);
        }
        if crate::native_lifecycle::lifecycle_enabled() {
            crate::native_lifecycle::emit(
                crate::native_lifecycle::Kind::Mode,
                crate::native_lifecycle::Data {
                    remote: Some(matches!(t, Transition::Remote(_))),
                    point: t.marker_target().map(|p| [p.x, p.y]),
                    extra: self.transitions,
                    reason: via as u32,
                    ..crate::native_lifecycle::Data::default()
                },
            );
        }
        self.restoring_from_marker = match (t, via) {
            (Transition::Local(at), Via::Marker) => at,
            _ => None,
        };
        match t {
            Transition::Remote(p) => {
                self.cancel_restore();
                let from = cursor_pos();
                self.raw_capture.update(false, self.park);
                // GiveFeedback can replace SetCursor(NULL). The live catcher
                // spans departure, outer bottom corner, and re-entry, so one
                // accounted move hides the arrow without changing OLE targets.
                // Never interpose the ordinary cursor cover on that catcher.
                let parked = if let Some(anchor) = self.drag_anchor {
                    self.hider.cancel_park();
                    let target = to_pixel(anchor.at);
                    self.park = if via == Via::Marker || via == Via::InHook && place_cursor(target).is_ok() {
                        target
                    } else {
                        from
                    };
                    self.mode = CaptureMode::Remote {
                        park: to_point(self.park),
                    };
                    self.restore_watch = None;
                    self.update_raw_capture();
                    return;
                } else {
                    self.hider.hide(from, p, self.hwnd)
                };
                self.restore_watch = None;
                self.park = if parked { p } else { from };
                self.mode = CaptureMode::Remote {
                    park: to_point(self.park),
                };
                self.update_raw_capture();
            }
            Transition::Local(at) => {
                self.hider.cancel_park();
                self.raw_capture.update(false, self.park);
                if let Some(p) = at.filter(|_| via != Via::Marker) {
                    // 光标仍隐藏（遮罩在停靠点）时放到落点，再撤掉遮罩。
                    let placed = place_cursor(p).is_ok();
                    super::trace::action(crate::native_lifecycle::Kind::ParkCommitted, Some(p), 1, Some(placed));
                }
                let was_hidden = self.hider.is_hidden();
                self.mode = CaptureMode::Local;
                self.begin_restore(at);
                if at.is_some() && self.hider.is_hidden() {
                    if via != Via::Marker {
                        // 光标已经放到落点（遮罩之外）。遮罩等回调返回之后再撤：光标位置在回调返回后才对其他程序
                        // 生效，在回调里撤掉遮罩时光标会在停靠点露出约 1 ms（Windows 11 实测）。
                        // SAFETY: 向本线程的窗口投递消息。
                        unsafe {
                            let _ = PostMessageW(
                                Some(self.hwnd),
                                WM_APP_UNCOVER,
                                WPARAM(self.transitions as usize),
                                LPARAM(0),
                            );
                        }
                    }
                } else {
                    self.hider.show_at(at);
                }
                self.restore_watch = at.map(|p| {
                    let now = cursor_pos();
                    tracing::debug!(
                        at = ?(p.x, p.y),
                        park = ?(self.park.x, self.park.y),
                        cursor = ?(now.x, now.y),
                        was_hidden,
                        ?via,
                        "returned to local"
                    );
                    RestoreWatch::new(p, self.park, via)
                });
            }
        }
    }

    fn wants_raw_capture(&self) -> bool {
        let restoring = self
            .pending
            .is_some_and(|p| matches!(p.transition, Transition::Local(_)));
        self.relative_requested
            && self.drag_anchor.is_none()
            && !restoring
            && !self.hider.park_pending()
            && matches!(self.mode, CaptureMode::Remote { .. })
    }

    fn update_raw_capture(&mut self) {
        self.raw_capture.update(self.wants_raw_capture(), self.park);
    }

    fn cover_acknowledged(&mut self, generation: u64) -> Option<Marker> {
        if !matches!(self.mode, CaptureMode::Remote { .. })
            || self.pending.is_some()
            || self.drag_anchor.is_some()
            || self.hider.park_generation() != Some(generation)
        {
            return None;
        }
        let target = self.hider.park_target(generation)?;
        if !self.park_commit.request(generation) {
            return None;
        }
        // The input itself changes position. A source-coordinate marker followed
        // by SetCursorPos, or a same-point input after that warp, leaves the
        // Shell's synthetic hover state behind. Never place from the ACK message
        // or from an unrelated physical callback, regardless of input source.
        let marker = Marker {
            target,
            sequence: self.transitions,
            extra: ((NEXT_MARKER.fetch_add(1, Ordering::Relaxed) & MARKER_TAG_MASK) << MARKER_TAG_BITS)
                | (TRANSITION_TAG & MARKER_TAG_MASK),
            window: self.hwnd.0 as isize,
            park_generation: Some(generation),
        };
        self.park_marker = Some((generation, marker));
        self.park_acceptance = None;
        Some(marker)
    }

    fn current_park_marker(&self, extra: usize) -> Option<(u64, Marker)> {
        self.park_marker.filter(|(generation, marker)| {
            marker.extra == extra
                && marker.sequence == self.transitions
                && matches!(self.mode, CaptureMode::Remote { .. })
                && self.pending.is_none()
                && self.drag_anchor.is_none()
                && self.hider.park_generation() == Some(*generation)
        })
    }

    fn fail_park(
        &mut self,
        marker: Marker,
        reason: crate::native_lifecycle::ParkInputFailure,
        observed: Option<POINT>,
    ) {
        if let Some(generation) = marker.park_generation
            && self.park_commit.fail(generation)
        {
            park_failure(marker, reason, 0, observed);
        }
    }

    fn finish_park_input(&mut self, extra: usize, inserted: bool) {
        use crate::native_lifecycle::ParkInputFailure;
        let Some((generation, marker)) = self.current_park_marker(extra) else {
            return;
        };
        if !inserted {
            // The sender already recorded its precise failure and native error.
            self.park_commit.fail(generation);
            return;
        }
        if !self.park_commit.accepted(generation) {
            self.fail_park(marker, ParkInputFailure::HookNotObserved, None);
            return;
        }
        let actual = cursor_pos();
        if actual != marker.target {
            self.fail_park(marker, ParkInputFailure::ReadbackMismatch, Some(actual));
            return;
        }
        if self.hider.finish_park(generation).is_none() {
            self.fail_park(marker, ParkInputFailure::HideLost, Some(actual));
            return;
        }
        self.park_commit.complete(generation);
        self.park_acceptance = None;
        self.park = marker.target;
        self.mode = CaptureMode::Remote {
            park: to_point(marker.target),
        };
        super::trace::action(
            crate::native_lifecycle::Kind::ParkCommitted,
            Some(marker.target),
            0,
            Some(true),
        );
        // SendInput has returned AND placement is visible. Only now may a
        // one-pixel raw/game clip be acquired; no wait for Shell hover closure.
        self.update_raw_capture();
    }

    fn take_park_acceptance(&mut self, extra: usize) -> Option<super::warp::Acceptance> {
        self.park_marker.filter(|(_, marker)| marker.extra == extra)?;
        self.park_acceptance.take()
    }

    fn park_chain_result(&mut self, extra: usize, passed: bool) {
        if !passed && let Some((_, marker)) = self.current_park_marker(extra) {
            if let Some(acceptance) = self.park_acceptance.take() {
                super::warp::rejected(acceptance);
            }
            self.fail_park(
                marker,
                crate::native_lifecycle::ParkInputFailure::HookChainRejected,
                None,
            );
        }
    }

    fn finish_ready_park(&mut self) {
        // The native held-source fallback retains its anchored placement in a
        // swallowed MOVE. Ordinary cover parking only uses its own allowed input.
        if matches!(self.mode, CaptureMode::Remote { .. })
            && self.pending.is_none()
            && let Some(anchor) = self.drag_anchor
        {
            let target = to_pixel(anchor.at);
            if self.park != target && place_cursor(target).is_ok() {
                self.park = target;
                self.mode = CaptureMode::Remote { park: to_point(target) };
            }
        }
    }

    fn prepare_marker(&mut self, pt: POINT, extra: usize) -> Prepared {
        if self.park_marker.is_some_and(|(_, marker)| marker.extra == extra) {
            let Some((generation, marker)) = self.current_park_marker(extra) else {
                return Prepared::Swallow;
            };
            if marker.target != pt {
                // Includes a clip owned by another app. Do not undo its clip or
                // account an unaccepted target as our displacement.
                self.fail_park(
                    marker,
                    crate::native_lifecycle::ParkInputFailure::TargetMismatch,
                    Some(pt),
                );
                return Prepared::Swallow;
            }
            if self.hider.park_target(generation) != Some(pt) {
                self.fail_park(marker, crate::native_lifecycle::ParkInputFailure::HideLost, Some(pt));
                return Prepared::Swallow;
            }
            if !self.park_commit.accept(generation) {
                return Prepared::Swallow;
            }
            // Same exact displacement + StaleSteps ledger as accepted restores.
            // Register before native readback catches up, never as user motion.
            // A downstream hook rejection rolls back only this registration.
            self.park_acceptance = Some(super::warp::accepted(cursor_pos(), pt));
            return Prepared::Pass;
        }
        // Match the request, not just its coordinates: two quick restores may
        // have the same target while the first marker is still in flight.
        let Some(pending) = self.pending else {
            return Prepared::Swallow;
        };
        if pending.marker.extra != extra || pending.marker.target != pt {
            tracing::debug!(?pt, extra, expected = ?pending.marker, "mode-switch marker does not match pending transition");
            return Prepared::Swallow;
        }
        self.pending = None;
        let t = pending.transition;
        if !self.hwnd.is_invalid() {
            // SAFETY: the hook thread owns this window and timer.
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TRANSITION_TIMER);
            }
        }
        self.execute(t, Via::Marker);
        if matches!(t, Transition::Local(Some(_))) || matches!(t, Transition::Remote(_)) && self.drag_anchor.is_some() {
            super::warp::accepted(cursor_pos(), pt);
            Prepared::Pass // The restore is committed by this native absolute move.
        } else {
            Prepared::Swallow
        }
    }

    fn uncover(&mut self, sequence: u64) -> bool {
        if self.transitions != sequence || self.pending.is_some() || !matches!(self.mode, CaptureMode::Local) {
            return false;
        }
        // SendInput completion and a hook return are not permission to uncover the
        // old park point. Read back placement before handing visibility to the cover.
        if let Some(watch) = &self.restore_watch
            && watch.at != self.park
        {
            let actual = cursor_pos_checked().ok();
            if actual == Some(self.park) {
                crate::native_lifecycle::emit_lifecycle(
                    crate::native_lifecycle::Kind::UncoverRequest,
                    crate::native_lifecycle::Data {
                        point: Some([watch.at.x, watch.at.y]),
                        cursor: actual.map(|p| [p.x, p.y]),
                        park: Some([self.park.x, self.park.y]),
                        extra: sequence,
                        reason: 2,
                        hidden: Some(self.hider.is_hidden()),
                        success: Some(false),
                        ..Default::default()
                    },
                );
                if IN_HOOK.with(Cell::get) {
                    // SAFETY: wake our owner window; generation is checked again
                    // outside the low-level callback before any synchronous placement.
                    unsafe {
                        let _ = PostMessageW(Some(self.hwnd), restore::MESSAGE, WPARAM(sequence as usize), LPARAM(0));
                    }
                } else {
                    self.reset_restore(sequence, 1);
                }
                return false;
            }
        }
        self.restoring_from_marker = None;
        self.hider.show_moved_away();
        self.check_restore_completion();
        true
    }

    /// 程序定位后，这一步若仍按旧位置计算，使用统一边界登记的旧基准（见 [`crate::stale_step`]）。
    fn stale_base_for(&mut self, pt: POINT, cur: POINT) -> Option<POINT> {
        super::warp::stale_base_for(pt, cur)
    }

    /// 回到本机后的第一秒：经过钩子的移动是否从落点出发（检测日志，不改变事件的处理）。
    fn watch_after_restore(&mut self, info: &MSLLHOOKSTRUCT) {
        if matches!(self.mode, CaptureMode::Local) && self.hider.is_hidden() {
            self.uncover(self.transitions);
        }
        if self.restoring_from_marker.is_some() {
            return; // The marker callback precedes the native cursor commit.
        }
        let Some(w) = self.restore_watch.as_mut() else {
            return;
        };
        if w.since.elapsed() > RESTORE_WATCH || !matches!(self.mode, CaptureMode::Local) {
            self.restore_watch = None;
            return;
        }
        let cur = cursor_pos();
        let ours = inject::own_mouse_tag(info.dwExtraInfo);
        let Some(anomaly) = w.check(cur, info.pt, !ours) else {
            return;
        };
        tracing::info!(
            ?anomaly,
            injected_by_us = ours,
            n = w.moves,
            cursor = ?(cur.x, cur.y),
            pt = ?(info.pt.x, info.pt.y),
            at = ?(w.at.x, w.at.y),
            park = ?(w.park.x, w.park.y),
            via = ?w.via,
            ms = w.since.elapsed().as_millis() as u64,
            "after returning to local the cursor does not move on from where it was placed"
        );
    }

    fn prepare_own_mouse(&mut self, msg: u32, info: &MSLLHOOKSTRUCT) -> Prepared {
        if msg == WM_MOUSEMOVE {
            self.watch_after_restore(info);
        }
        Prepared::Pass
    }

    fn prepare_mouse(&mut self, msg: u32, info: &MSLLHOOKSTRUCT, now: Instant) -> Prepared {
        if inject::own_mouse_tag(info.dwExtraInfo) {
            return self.prepare_own_mouse(msg, info);
        }
        // One snapshot must feed both stale-step classification and subtraction.
        // On Windows 11 the accepted absolute marker can precede native readback:
        // using entry for classification but park for subtraction leaks the entire
        // unpark displacement (2026-10-12). Keep visibility checks on real readback.
        let native = if msg == WM_MOUSEMOVE { cursor_pos() } else { info.pt };
        let cur = if msg == WM_MOUSEMOVE {
            super::warp::sample(native)
        } else {
            native
        };
        let stale = if msg == WM_MOUSEMOVE {
            self.stale_base_for(info.pt, cur)
        } else {
            None
        };
        if msg == WM_MOUSEMOVE && stale.is_none() {
            self.watch_after_restore(info);
        }
        let remote = matches!(self.mode, CaptureMode::Remote { .. });
        // Swallowing here also suppresses WM_INPUT. Raw game capture instead
        // confines the pointer under our cursor-cover window and allows only
        // physical MOVE. External synthetic input was classified before this path;
        // only the explicit forwarding opt-in reaches routed translation.
        if remote
            && msg == WM_MOUSEMOVE
            && external::mouse_origin(info) == crate::windows_external_input::Origin::Physical
            && self.raw_capture.allows_move()
        {
            return Prepared::Pass;
        }
        let source = if external::mouse_origin(info) == crate::windows_external_input::Origin::External {
            SourceKind::Unknown
        } else if self.raw.touch_active(now) {
            SourceKind::Touchpad
        } else {
            SourceKind::Mouse
        };
        let pt = info.pt;
        // Remote：光标停在停靠点，不是用户指针的位置，不上报（接口约定）。
        let cursor = (!remote).then(|| Point::new(f64::from(pt.x), f64::from(pt.y)));
        // 滚轮格数（`ScrollUnit::Line` 的约定）：高精度滚轮为小数；换算成行由输入路由按每格行数负责。
        let wheel = || f64::from((info.mouseData >> 16) as u16 as i16) / 120.0;
        let (event, force_swallow) = match msg {
            WM_MOUSEMOVE => {
                if let Some(base) = stale {
                    // 按切换之前的位置算的一步（见 crate::stale_step）：位移按那个位置算；吞掉，不能让它把光标带回去。
                    // 本机模式下不上报它的位置（不是光标会在的位置），这一步只是没有生效。
                    tracing::debug!(pt = ?(pt.x, pt.y), base = ?(base.x, base.y), cur = ?(cur.x, cur.y), remote, "mouse step computed from the position before the mode switch");
                    return Prepared::Event {
                        event: CapturedEvent {
                            physical: external::mouse_origin(info) == crate::windows_external_input::Origin::Physical,
                            event: InputEvent::PointerDelta {
                                dx: f64::from(pt.x - base.x),
                                dy: f64::from(pt.y - base.y),
                            },
                            source,
                            time_us: time_us(now),
                            cursor: None,
                        },
                        handler: self.handler.clone(),
                        force_swallow: true,
                        force_pass: false,
                        track_key: None,
                    };
                }
                let (dx, dy) = (pt.x - cur.x, pt.y - cur.y);
                if remote {
                    if self.drag_anchor.is_none() && (native.x != self.park.x || native.y != self.park.y) {
                        // A foreign warp can also leave a visible cursor outside our cover.
                        // Reacquire hiding before moving it back; never bypass the same handshake.
                        // A held OLE catcher owns its hit-test surface. Never put
                        // a new cover over it: a foreign departure invalidates
                        // that native source through DragLeave instead.
                        if !self.hider.park_pending() {
                            let target = self.park;
                            self.raw_capture.update(false, self.park);
                            if !self.hider.hide(native, target, self.hwnd) {
                                self.park = native;
                                self.mode = CaptureMode::Remote { park: to_point(native) };
                            }
                        }
                    }
                    if dx == 0 && dy == 0 {
                        return Prepared::Swallow;
                    }
                }
                (
                    InputEvent::PointerDelta {
                        dx: f64::from(dx),
                        dy: f64::from(dy),
                    },
                    remote,
                )
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP => (btn(MouseButton::Left, msg == WM_LBUTTONDOWN), false),
            WM_RBUTTONDOWN | WM_RBUTTONUP => (btn(MouseButton::Right, msg == WM_RBUTTONDOWN), false),
            WM_MBUTTONDOWN | WM_MBUTTONUP => (btn(MouseButton::Middle, msg == WM_MBUTTONDOWN), false),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let b = match info.mouseData >> 16 {
                    1 => MouseButton::Back,
                    2 => MouseButton::Forward,
                    n => MouseButton::Other((n + 2).min(255) as u8),
                };
                (btn(b, msg == WM_XBUTTONDOWN), false)
            }
            // WM_MOUSEWHEEL 正值 = 向前滚 = 视图向文档顶部：与 dy > 0 的约定一致。
            // 钩子看到的增量已经过 FlipFlopWheel（鼠标驱动）与触摸板方向设置，正是本机会产生的视图移动方向。
            WM_MOUSEWHEEL => (scroll(0.0, wheel()), false),
            // WM_MOUSEHWHEEL 正值 = 向右滚 = 视图向右移动，与 dx > 0（视图向左）相反。
            WM_MOUSEHWHEEL => (scroll(-wheel(), 0.0), false),
            _ => return Prepared::Pass,
        };
        Prepared::Event {
            event: CapturedEvent {
                physical: external::mouse_origin(info) == crate::windows_external_input::Origin::Physical,
                event,
                source,
                time_us: time_us(now),
                cursor,
            },
            handler: self.handler.clone(),
            force_swallow,
            force_pass: false,
            track_key: None,
        }
    }

    fn prepare_key(&mut self, msg: u32, info: &KBDLLHOOKSTRUCT, now: Instant) -> Prepared {
        if info.dwExtraInfo == INJECT_TAG {
            return Prepared::Pass;
        }
        let remote = matches!(self.mode, CaptureMode::Remote { .. });
        let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let mut sc = (info.scanCode & 0xFF) as u16;
        if info.flags.0 & LLKHF_EXTENDED.0 != 0 {
            sc |= 0xE000;
        }
        if info.scanCode == 0 {
            // 某些软件合成的按键没有扫描码：按虚拟键反查
            // SAFETY: 纯查表函数。
            sc = unsafe { MapVirtualKeyW(info.vkCode, MAPVK_VK_TO_VSC_EX) } as u16;
        }
        let Some(code) = keymap::scancode_to_hid(sc).or_else(|| {
            (physical_keyboard(info) && crate::capture_loop::keyboard_disabled()).then_some(KeyCode::new(0xFFAB, sc))
        }) else {
            tracing::debug!(remote, pressed, mapped = false, "keyboard hook translation");
            return if remote { Prepared::Swallow } else { Prepared::Pass };
        };
        self.pipeline.key_captured += 1;
        tracing::debug!(
            remote,
            pressed,
            mapped = true,
            modifier = code.is_modifier(),
            injected = info.flags.0 & LLKHF_INJECTED.0 != 0,
            repeat = pressed && self.keys.contains_key(&code.0),
            "keyboard hook translation"
        );
        let source = if external::key_origin(info) == crate::windows_external_input::Origin::External {
            SourceKind::Unknown
        } else {
            SourceKind::Keyboard
        };
        let event = CapturedEvent {
            physical: physical_keyboard(info),
            event: InputEvent::Key { code, pressed },
            source,
            time_us: time_us(now),
            cursor: None,
        };
        // Caps uses these exact hook DOWN/UP transitions too. Gesture timing and
        // repeat suppression belong to the source router, never GetKeyState's
        // toggle bit (which describes lock state rather than a held key).
        let (track_key, force_pass) = if pressed {
            if let Some(&swallowed) = self.keys.get(&code.0) {
                // 系统的自动重复（同一个键没有抬起又按下）：上报给路由，发往远端的键逐个转发
                // （重复只在源机产生）；吞掉还是放行与首次按下一致。
                return Prepared::Event {
                    event,
                    handler: self.handler.clone(),
                    force_swallow: swallowed,
                    force_pass: !swallowed,
                    track_key: None,
                };
            }
            (Some(code.0), false)
        } else {
            // 按下时交给了本机的键，抬起也必须交给本机，否则本机卡键
            (None, self.keys.remove(&code.0) == Some(false))
        };
        Prepared::Event {
            event,
            handler: self.handler.clone(),
            force_swallow: false,
            force_pass,
            track_key,
        }
    }
}

fn btn(button: MouseButton, pressed: bool) -> InputEvent {
    InputEvent::Button { button, pressed }
}

fn scroll(dx: f64, dy: f64) -> InputEvent {
    InputEvent::Scroll {
        dx,
        dy,
        unit: ScrollUnit::Line,
        phase: ScrollPhase::None,
    }
}

/// A single transition's native input and completion notification.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Marker {
    target: POINT,
    sequence: u64,
    extra: usize,
    window: isize,
    park_generation: Option<u64>,
}

fn park_failure(
    marker: Marker,
    failure: crate::native_lifecycle::ParkInputFailure,
    error: u32,
    observed: Option<POINT>,
) {
    let Some(generation) = marker.park_generation else {
        return;
    };
    if crate::native_lifecycle::lifecycle_enabled() {
        crate::native_lifecycle::emit(
            failure.kind(),
            crate::native_lifecycle::Data {
                extra: generation,
                flags: error,
                point: Some([marker.target.x, marker.target.y]),
                cursor: observed.map(|p| [p.x, p.y]),
                success: Some(false),
                ..Default::default()
            },
        );
    }
    tracing::warn!(
        ?failure,
        error,
        generation,
        "ordinary cursor park failed; no retry for this generation"
    );
}

fn marker_completed(marker: Marker, inserted: bool) {
    let (message, wparam, lparam) = if marker.park_generation.is_some() {
        (WM_APP_PARK_COMPLETE, marker.extra, isize::from(inserted))
    } else if inserted {
        (WM_APP_UNCOVER, marker.sequence as usize, 0)
    } else {
        return;
    };
    // SAFETY: fixed notification to the original hook owner; the ticket is
    // revalidated there. No window call waits for that thread or moves input.
    if let Err(error) = unsafe {
        PostMessageW(
            Some(HWND(marker.window as *mut _)),
            message,
            WPARAM(wparam),
            LPARAM(lparam),
        )
    } {
        park_failure(
            marker,
            crate::native_lifecycle::ParkInputFailure::CompletionUnavailable,
            error.code().0 as u32,
            None,
        );
    }
}

// Boundary-only observations share the bounded lifecycle queue with Mode and
// UncoverRequest. No text formatting, input payload or window identity is retained.
fn marker_data(marker: Option<Marker>, extra: usize, target: POINT) -> crate::native_lifecycle::Data {
    crate::native_lifecycle::Data {
        extra: marker.map_or(0, |marker| marker.sequence),
        marker: Some(extra as u64),
        reason: marker.map_or(2, |marker| u32::from(marker.park_generation.is_some())),
        point: Some([target.x, target.y]),
        cursor: cursor_pos_checked().ok().map(|p| [p.x, p.y]),
        ..Default::default()
    }
}

fn trace_marker_hook(
    kind: crate::native_lifecycle::Kind,
    marker: Option<Marker>,
    info: &MSLLHOOKSTRUCT,
    passed: bool,
    chain_result: Option<i64>,
) {
    if crate::native_lifecycle::lifecycle_enabled() {
        crate::native_lifecycle::emit(
            kind,
            crate::native_lifecycle::Data {
                success: Some(passed),
                chain_result,
                ..marker_data(marker, info.dwExtraInfo, info.pt)
            },
        );
    }
}

/// Inject from the caller, never the hook thread; see the module comment.
fn send_marker(marker: Marker) {
    let p = marker.target;
    let _dpi = ThreadDpiGuard::per_monitor_v2();
    // SAFETY: 读取虚拟桌面范围（本线程 PMv2，物理像素）；注入一个完整初始化的 INPUT。
    unsafe {
        let (vx, vy, vw, vh) = (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        );
        let mut input = inject::move_abs(inject::normalize(p.x, vx, vw), inject::normalize(p.y, vy, vh));
        input.Anonymous.mi.dwExtraInfo = marker.extra;
        if crate::native_lifecycle::lifecycle_enabled() {
            let data = marker_data(Some(marker), marker.extra, p);
            crate::native_lifecycle::emit(
                crate::native_lifecycle::Kind::MarkerSubmission,
                crate::native_lifecycle::Data {
                    normalized: Some([input.Anonymous.mi.dx, input.Anonymous.mi.dy]),
                    desktop: Some([vx, vy, vx.saturating_add(vw), vy.saturating_add(vh)]),
                    ..data
                },
            );
        }
        SetLastError(WIN32_ERROR(0));
        let inserted = super::warp::send(&[input]) == 1;
        let error = if inserted { 0 } else { GetLastError().0 };
        if crate::native_lifecycle::lifecycle_enabled() {
            let mut info = CURSORINFO {
                cbSize: size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            let showing = GetCursorInfo(&mut info)
                .ok()
                .map(|()| info.flags.0 & CURSOR_SHOWING.0 != 0);
            let data = marker_data(Some(marker), marker.extra, p);
            crate::native_lifecycle::emit(
                crate::native_lifecycle::Kind::MarkerReadback,
                crate::native_lifecycle::Data {
                    flags: error,
                    success: Some(inserted),
                    hidden: showing.map(|showing| !showing),
                    cursor_showing: showing,
                    ..data
                },
            );
        }
        if !inserted {
            park_failure(
                marker,
                crate::native_lifecycle::ParkInputFailure::SendRejected,
                error,
                None,
            );
            tracing::warn!(error, "native input marker was not delivered");
            marker_completed(marker, false);
            return;
        }
        marker_completed(marker, true);
    }
}

fn to_pixel(p: Point) -> POINT {
    POINT {
        x: p.x.floor() as i32,
        y: p.y.floor() as i32,
    }
}

#[cfg(all(test, debug_assertions))]
thread_local! {
    static TEST_CURSOR: Cell<Option<POINT>> = const { Cell::new(None) };
}

fn place_cursor(p: POINT) -> windows::core::Result<()> {
    super::warp::place(p)
}

#[cfg(all(test, debug_assertions))]
pub(super) fn test_place_cursor(p: POINT) -> bool {
    if super::cursor::test_native::warp(p) {
        return true;
    }
    if TEST_CURSOR.with(|c| c.get().is_some()) {
        TEST_CURSOR.with(|c| c.set(Some(p)));
        return true;
    }
    false
}

pub(super) fn cursor_pos() -> POINT {
    cursor_pos_checked().unwrap_or_default()
}

pub(super) fn cursor_pos_checked() -> windows::core::Result<POINT> {
    #[cfg(all(test, debug_assertions))]
    if let Some(p) = super::cursor::test_native::position() {
        return Ok(p);
    }
    #[cfg(all(test, debug_assertions))]
    if let Some(p) = TEST_CURSOR.with(Cell::get) {
        return Ok(p);
    }
    let mut p = POINT::default();
    // SAFETY: 输出参数指向本地变量。
    unsafe {
        GetCursorPos(&mut p)?;
    }
    Ok(p)
}

fn clamp_to_monitor(p: POINT) -> POINT {
    clamp_to_monitor_checked(p).unwrap_or(p)
}

fn clamp_to_monitor_checked(p: POINT) -> Option<POINT> {
    #[cfg(all(test, debug_assertions))]
    if super::cursor::test_native::active() {
        return Some(p);
    }
    // SAFETY: read-only nearest-monitor query under the hook owner's PMv2 context.
    unsafe {
        let monitor = MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        let rect = info.rcMonitor;
        (rect.left < rect.right && rect.top < rect.bottom).then(|| POINT {
            x: p.x.clamp(rect.left, rect.right - 1),
            y: p.y.clamp(rect.top, rect.bottom - 1),
        })
    }
}

/// 截获线程被一件事占住多久就可能让系统卸载钩子：钩子回调排在本线程的消息之后，超过 `LowLevelHooksTimeout`
/// （Windows 10 起最多 1 s）没返回，系统就静默卸载那个钩子。超过这个时间记一条警告（只检测，说明是哪一类工作）。
const BUSY_WARN: Duration = Duration::from_millis(300);

fn report_busy(what: &str, msg: u32, dt: Duration) {
    if dt > BUSY_WARN {
        tracing::warn!(
            what,
            msg = format_args!("0x{msg:04X}"),
            ms = dt.as_millis() as u64,
            "capture thread was busy; a low-level hook callback waiting behind it may time out and be removed"
        );
    }
}

/// 调用上层回调：捕获 panic（不能跨 FFI 边界展开），统计慢调用。
fn call_handler(handler: &EventHandler, event: &CapturedEvent) -> Disposition {
    let t0 = Instant::now();
    let d = catch_unwind(AssertUnwindSafe(|| handler(event))).unwrap_or_else(|_| {
        tracing::error!("截获回调 panic，事件交还系统");
        Disposition::Pass
    });
    let dt = t0.elapsed();
    if dt > SLOW_HANDLER {
        with_state(|st| {
            st.stats.slow_handler_calls += 1;
            if st.stats.slow_handler_calls.is_power_of_two() {
                tracing::warn!(
                    "截获回调耗时 {dt:?}（累计 {} 次过慢），可能导致系统卸载钩子",
                    st.stats.slow_handler_calls
                );
            }
        });
    }
    d
}

/// The Default lock curtain remains capturable with UIAccess. Only inaccessible
/// desktops gate the source; WTS lock is a notification to re-query, not a gate.
fn capture_availability_changed(desktop_changed: bool) {
    let desktop_available = desktop_trace::default_desktop_active();
    super::touchpad::contact_desktop(desktop_available);
    let Some((handler, events, available)) = with_state(|st| {
        let available = desktop_available;
        if available == st.continuity.available() && !desktop_changed {
            return None;
        }
        st.touchpad.cancel_all();
        st.keys.clear();
        external::gap();
        let now = time_us(Instant::now());
        let events = if desktop_changed {
            st.continuity.desktop_changed(available, |_| false, now)
        } else {
            st.continuity.transition(available, |_| false, now)
        };
        Some((st.handler.clone(), events, available))
    })
    .flatten() else {
        return;
    };
    if crate::native_lifecycle::lifecycle_enabled() {
        crate::native_lifecycle::emit(
            crate::native_lifecycle::Kind::InputDesktop,
            crate::native_lifecycle::Data {
                reason: 2,
                success: Some(available),
                message: events.len() as u32,
                ..Default::default()
            },
        );
    }
    // A queued switch can arrive after a rapid Winlogon -> Default round trip.
    // Reclaim/release on that discontinuity too, before rebuilding any modifiers.
    // No event from the credential desktop is read or forwarded.
    report_capture_desktop(available, desktop_changed);
    for event in events {
        let disposition = crate::diagnostics::with_capture(
            crate::diagnostics::CaptureMetadata {
                backend_generated: Some(true),
                repeat: Some(false),
                ..Default::default()
            },
            || call_handler(&handler, &event),
        );
        if let InputEvent::Key { code, pressed: true } = event.event {
            with_state(|st| st.keys.insert(code.0, disposition == Disposition::Swallow));
        }
    }
}

/// 返回 `Some(true)` 吞掉，`Some(false)` 或 `None` 交给下一个钩子。
fn finish(prepared: Option<Prepared>) -> bool {
    match prepared {
        None | Some(Prepared::Pass) => false,
        Some(Prepared::Swallow) => true,
        Some(Prepared::Event {
            event,
            handler,
            force_swallow,
            force_pass,
            track_key,
        }) => {
            if !with_state(|st| {
                if !st.continuity.available() {
                    return false;
                }
                st.continuity.observe(&event.event);
                true
            })
            .unwrap_or(true)
            {
                return false;
            }
            let d = if let InputEvent::Key { pressed, .. } = event.event {
                // The low-level hook provides DOWN/UP but no OS repeat bit.
                // A duplicate DOWN in our ledger cannot distinguish auto-repeat
                // from a missing UP, so repeat remains unknown. This context is
                // observation-only and contains no serialized key identity.
                let metadata = crate::diagnostics::CaptureMetadata {
                    raw: if pressed {
                        crate::diagnostics::RawKeyEvent::Down
                    } else {
                        crate::diagnostics::RawKeyEvent::Up
                    },
                    backend_generated: Some(false),
                    repeat: None,
                    pressed_while_held: false,
                };
                crate::diagnostics::with_capture(metadata, || call_handler(&handler, &event))
            } else {
                call_handler(&handler, &event)
            };
            let swallow = !force_pass && (force_swallow || d == Disposition::Swallow);
            if matches!(event.event, InputEvent::Key { .. }) {
                with_state(|st| {
                    if swallow {
                        st.pipeline.key_swallowed += 1;
                    } else {
                        st.pipeline.key_passed += 1;
                    }
                });
            }
            if let Some(code) = track_key {
                with_state(|st| st.keys.insert(code, swallow));
            }
            swallow
        }
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let _source_time = crate::source_time::CaptureScope::entry();
    let _timing = crate::timing::CaptureTimer::new();
    let t0 = Instant::now();
    let r = mouse_proc_inner(code, wparam, lparam);
    report_busy("mouse hook", 0, t0.elapsed());
    r
}

pub(super) fn suppress_native_wheel(
    msg: u32,
    info: &MSLLHOOKSTRUCT,
    owns_native: bool,
    origin: super::wheel_origin::Origin,
) -> bool {
    // On Windows 11 build 26200 native PTP promotion loses extra-info and the
    // injected bit; GetCurrentInputMessageSource was Unknown at the hook. After
    // consuming own markers, use raw mouse identity first and bounded native
    // ownership only for the remainder. A blanket time/INJECTED filter would
    // suppress real mouse or third-party remap input.
    matches!(msg, WM_MOUSEWHEEL | WM_MOUSEHWHEEL)
        && owns_native
        // Only non-injected promotion participates in native wheel ownership.
        // External SendInput either bypasses capture or uses opt-in routing.
        && external::mouse_origin(info) == crate::windows_external_input::Origin::Physical
        && origin != super::wheel_origin::Origin::PhysicalMouse
}

fn mouse_proc_inner(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    mouse_proc_with_next(code, wparam, lparam, || {
        // SAFETY: forward the unchanged native hook arguments.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    })
}

fn mouse_proc_with_next(code: i32, wparam: WPARAM, lparam: LPARAM, next: impl FnOnce() -> LRESULT) -> LRESULT {
    let mut marker_context = None;
    if code >= 0 {
        // SAFETY: HC_ACTION 时 lparam 指向系统提供的 MSLLHOOKSTRUCT，回调期间有效。
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let origin_kind = external::mouse_origin(info);
        if origin_kind == crate::windows_external_input::Origin::External
            && external::mouse_local(wparam.0 as u32, info)
        {
            return external::pass_mouse(wparam.0 as u32, info, next);
        }
        if origin_kind == crate::windows_external_input::Origin::Physical {
            // OS event times are wrapping milliseconds; this is callback queue delay, not hardware latency.
            // SAFETY: read-only system millisecond counter, no handles or pointers.
            crate::timing::CaptureTimer::os_wait(
                unsafe { windows::Win32::System::SystemInformation::GetTickCount() }.wrapping_sub(info.time),
            );
        }
        let now = Instant::now();
        let owns_wheel = with_state(|st| st.touchpad.owns_ambiguous_wheel(now)).unwrap_or(false);
        // Read before borrowing State: PeekMessage can dispatch sent messages.
        let origin = super::wheel_origin::hook(wparam.0 as u32, info, owns_wheel);
        let _in_hook = InHook::enter();
        if origin_kind != crate::windows_external_input::Origin::External {
            observe_button(wparam.0 as u32, info);
        }
        account_own_mouse(wparam.0 as u32, info);
        // Check before raw-capture MOVE and native touchpad wheel bypasses.
        // Own injections/markers and external applications retain their paths.
        if origin_kind == crate::windows_external_input::Origin::Physical && crate::capture_loop::mouse_disabled() {
            return LRESULT(1);
        }
        let prepared = with_state(|st| {
            st.last_mouse_hook = now;
            st.pipeline.mouse_callbacks += 1;
            if is_marker(info.dwExtraInfo) {
                marker_context = st
                    .pending
                    .map(|pending| pending.marker)
                    .filter(|marker| marker.extra == info.dwExtraInfo)
                    .or_else(|| {
                        st.park_marker
                            .map(|(_, marker)| marker)
                            .filter(|marker| marker.extra == info.dwExtraInfo)
                    });
                let prepared = st.prepare_marker(info.pt, info.dwExtraInfo);
                trace_marker_hook(
                    crate::native_lifecycle::Kind::MarkerHookDecision,
                    marker_context,
                    info,
                    matches!(prepared, Prepared::Pass),
                    None,
                );
                return prepared;
            }
            let msg = wparam.0 as u32;
            let ours = inject::own_mouse_tag(info.dwExtraInfo);
            if ours {
                return st.prepare_own_mouse(msg, info);
            }
            if suppress_native_wheel(msg, info, st.touchpad.owns_ambiguous_wheel(now), origin) {
                if origin == super::wheel_origin::Origin::Unknown {
                    tracing::warn!(
                        ?origin,
                        "non-injected wheel origin unavailable during native gesture ownership; capture withheld"
                    );
                } else {
                    tracing::debug!(
                        ?origin,
                        tail_us = st.touchpad.wheel_tail_age(now).map(|age| age.as_micros() as u64),
                        "native gesture owns device-less non-injected wheel; capture withheld"
                    );
                }
                return if matches!(st.mode, CaptureMode::Remote { .. }) {
                    Prepared::Swallow
                } else {
                    Prepared::Pass
                };
            }
            st.prepare_mouse(msg, info, now)
        });
        if prepared.is_none() {
            if is_marker(info.dwExtraInfo) {
                // A reentrant callback could not validate the pending sequence.
                // It must not commit an unvalidated absolute marker. The normal
                // transition fallback remains responsible for that request.
                trace_marker_hook(
                    crate::native_lifecycle::Kind::MarkerHookDecision,
                    None,
                    info,
                    false,
                    None,
                );
                return LRESULT(1);
            }
            // 状态正被外层使用：钩子回调重入了（外层在等跨线程的窗口调用时，本线程处理了新的钩子消息）。
            // 这个事件没经过截获就交给了系统（检测日志）。
            let n = REENTERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::info!(
                    n,
                    pt = ?(info.pt.x, info.pt.y),
                    extra = info.dwExtraInfo,
                    flags = info.flags,
                    "mouse hook re-entered while its state is in use; the event passed unprocessed"
                );
            }
        }
        let swallowed = finish(prepared);
        // The target of this swallowed move was computed before any pending park.
        // Route it first, then position while Windows is waiting for this callback.
        if swallowed
            && wparam.0 as u32 == WM_MOUSEMOVE
            && !is_marker(info.dwExtraInfo)
            && !inject::own_mouse_tag(info.dwExtraInfo)
        {
            with_state(State::finish_ready_park);
        }
        if swallowed {
            return LRESULT(1);
        }
    }
    // Keep rollback outside State across CallNextHookEx: a downstream hook can
    // pump messages and cancel/replace the mode before rejecting this input.
    // A newer placement is protected by WarpBase's registration sequence.
    let acceptance = if code >= 0 && wparam.0 as u32 == WM_MOUSEMOVE {
        // SAFETY: hook input is valid for this callback.
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        with_state(|st| st.take_park_acceptance(info.dwExtraInfo)).flatten()
    } else {
        None
    };
    // SAFETY: 交给钩子链中的下一个。
    let result = next();
    if result.0 != 0
        && let Some(acceptance) = acceptance
    {
        super::warp::rejected(acceptance);
    }
    if code >= 0 && wparam.0 as u32 == WM_MOUSEMOVE {
        // SAFETY: the hook input remains valid until this callback returns.
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if is_marker(info.dwExtraInfo) {
            trace_marker_hook(
                crate::native_lifecycle::Kind::MarkerChainResult,
                marker_context,
                info,
                result.0 == 0,
                Some(result.0 as i64),
            );
            with_state(|st| st.park_chain_result(info.dwExtraInfo, result.0 == 0));
        }
    }
    if result.0 == 0 && code >= 0 && wparam.0 as u32 == WM_MOUSEMOVE {
        // SAFETY: the hook's input remains valid until this callback returns.
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if external::mouse_origin(info) == crate::windows_external_input::Origin::Physical {
            super::warp::passed(info.pt);
        }
    }
    result
}

fn account_own_mouse(msg: u32, info: &MSLLHOOKSTRUCT) {
    // Register outside the State borrow: a tagged placement can reenter while
    // State is busy, and is still passed to Windows. MOVE|DOWN has a movement
    // callback as well as a button callback; only the movement changes the base.
    if msg == WM_MOUSEMOVE && inject::own_mouse_tag(info.dwExtraInfo) {
        if info.dwExtraInfo == inject::SHAPE_REFRESH_TAG {
            super::warp::accepted_refresh(cursor_pos(), info.pt);
        } else {
            super::warp::accepted(cursor_pos(), info.pt);
        }
    }
}

fn observe_button(msg: u32, info: &MSLLHOOKSTRUCT) {
    use crate::button_diagnostics::{self as evidence, Layer, TimestampUnit, Transition};
    if !evidence::enabled() {
        return;
    }
    let (button, pressed) = match msg {
        WM_LBUTTONDOWN => (MouseButton::Left, true),
        WM_LBUTTONUP => (MouseButton::Left, false),
        WM_RBUTTONDOWN => (MouseButton::Right, true),
        WM_RBUTTONUP => (MouseButton::Right, false),
        WM_MBUTTONDOWN => (MouseButton::Middle, true),
        WM_MBUTTONUP => (MouseButton::Middle, false),
        WM_XBUTTONDOWN | WM_XBUTTONUP => (
            match info.mouseData >> 16 {
                1 => MouseButton::Back,
                2 => MouseButton::Forward,
                n => MouseButton::Other((n + 2).min(255) as u8),
            },
            msg == WM_XBUTTONDOWN,
        ),
        _ => return,
    };
    evidence::transition(Transition {
        layer: Layer::WindowsHook,
        button,
        pressed,
        event_timestamp: u64::from(info.time),
        timestamp_unit: TimestampUnit::Milliseconds,
        source_state: None,
        physical: external::mouse_origin(info) == crate::windows_external_input::Origin::Physical,
        own: inject::own_mouse_tag(info.dwExtraInfo) || is_marker(info.dwExtraInfo),
        system: evidence::system_state(button),
    });
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let _source_time = crate::source_time::CaptureScope::entry();
    let _timing = crate::timing::CaptureTimer::new();
    let t0 = Instant::now();
    let r = keyboard_proc_inner(code, wparam, lparam);
    report_busy("keyboard hook", 0, t0.elapsed());
    r
}

fn physical_keyboard(info: &KBDLLHOOKSTRUCT) -> bool {
    external::key_origin(info) == crate::windows_external_input::Origin::Physical
}

fn guard_passes_lock_input(info: &KBDLLHOOKSTRUCT, disabled: bool, locked: bool) -> bool {
    physical_keyboard(info) && disabled && locked
}

fn keyboard_proc_inner(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    keyboard_proc_with_next(code, wparam, lparam, || {
        // SAFETY: forward the unchanged native hook arguments.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    })
}

fn keyboard_proc_with_next(code: i32, wparam: WPARAM, lparam: LPARAM, next: impl FnOnce() -> LRESULT) -> LRESULT {
    if code >= 0 {
        // SAFETY: HC_ACTION 时 lparam 指向系统提供的 KBDLLHOOKSTRUCT，回调期间有效。
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        use crate::windows_external_input::Origin;
        match external::key_origin(info) {
            Origin::Own | Origin::Marker => return next(),
            Origin::External if external::key_local(wparam.0 as u32, info) => return external::pass_key(next),
            Origin::Physical => external::observe_physical_key(wparam.0 as u32, info),
            _ => {}
        }
        if physical_keyboard(info) {
            // SAFETY: read-only system millisecond counter, no handles or pointers.
            crate::timing::CaptureTimer::os_wait(
                unsafe { windows::Win32::System::SystemInformation::GetTickCount() }.wrapping_sub(info.time),
            );
        }
        // Windows' Default lock curtain can still reach UIAccess hooks, unlike
        // Winlogon credentials. Reuse the existing WTS lock observation to pass
        // physical keys straight to the OS while guarded+locked: no forwarding,
        // hotkeys or chord tracking, and no change to the saved disabled setting.
        if guard_passes_lock_input(
            info,
            crate::capture_loop::keyboard_disabled(),
            crate::capture_loop::keyboard_guard_suspended(),
        ) {
            // SAFETY: unchanged native event is passed to the next desktop-local hook.
            return next();
        }
        let sc = (info.scanCode & 0xFF) as u16
            | if info.flags.0 & LLKHF_EXTENDED.0 != 0 {
                0xE000
            } else {
                0
            };
        if crate::capture_loop::keyboard_disabled() && keymap::local_brightness_key(sc).is_some() {
            // Before both routing and the native guard override: down/repeat/up
            // remain local even when keyboard focus is on another device.
            return next();
        }
        let now = Instant::now();
        let _in_hook = InHook::enter();
        let mut remote = false;
        let prepared = with_state(|st| {
            st.last_kbd_hook = now;
            st.pipeline.keyboard_callbacks += 1;
            remote = matches!(st.mode, CaptureMode::Remote { .. });
            st.prepare_key(wparam.0 as u32, info, now)
        });
        if prepared.is_none() {
            // 同鼠标钩子：重入时按键没经过截获就交给了系统（检测日志，不记键码）。
            let n = REENTERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::info!(
                    n,
                    "keyboard hook re-entered while its state is in use; the key passed unprocessed"
                );
            }
        }
        // The native guard also covers unmapped scan/media keys and overrides
        // the old press disposition cached before disabling. Tagged remote input passes.
        let key = match &prepared {
            Some(Prepared::Event {
                event:
                    CapturedEvent {
                        event: InputEvent::Key { code, .. },
                        ..
                    },
                ..
            }) => Some(*code),
            _ => None,
        };
        let guarded = physical_keyboard(info) && crate::capture_loop::keyboard_key_blocked(key);
        let swallowed = finish(prepared) || guarded;
        if swallowed {
            return LRESULT(1);
        }
        let msg = wparam.0 as u32;
        if crate::capture_loop::keyboard_remote().unwrap_or(remote) && (msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN) {
            // 焦点在其他设备上，按下却交给了本机：只检测、记日志。
            let n = REMOTE_KEY_PASSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::warn!(
                    n,
                    injected = info.flags.0 & LLKHF_INJECTED.0 != 0,
                    "remote mode: a key press was passed to this computer instead of the focused device"
                );
            }
        }
    }
    // SAFETY: 交给钩子链中的下一个。
    next()
}

/// Remote 模式下交给了本机的按下（检测计数）。
static REMOTE_KEY_PASSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn initial_hook_install_and_retry_publish_keyboard_capture_health() {
        use std::sync::{Arc, Mutex};
        struct Owner(Arc<Mutex<Vec<Option<bool>>>>);
        impl crate::capture_loop::CaptureLoop for Owner {
            fn set_wake(&mut self, _: crate::capture_loop::Wake) {}
            fn event(&mut self, _: &CapturedEvent) -> Disposition {
                Disposition::Pass
            }
            fn capture_available(&mut self, available: bool) {
                self.0.lock().unwrap().push(Some(available));
            }
            fn capture_detached(&mut self) {
                self.0.lock().unwrap().push(None);
            }
            fn poll(&mut self, _: Instant) -> Option<Instant> {
                None
            }
            fn stopped(&self) -> bool {
                false
            }
        }
        let observed = Arc::new(Mutex::new(Vec::new()));
        crate::capture_loop::install(Box::new(Owner(observed.clone())), Arc::new(|| {}));
        // A successful first installation, failed retry, inaccessible desktop,
        // and successful retry must each publish an authoritative outcome.
        for (installed, desktop_available) in [(true, true), (false, true), (true, false), (false, false), (true, true)]
        {
            report_capture_install_result(installed, desktop_available);
        }
        report_capture_desktop(false, true);
        report_capture_desktop(true, true);
        // A coalesced round trip still withdraws before restoring the source.
        report_capture_desktop(true, true);
        crate::capture_loop::clear();
        assert_eq!(
            *observed.lock().unwrap(),
            [
                Some(true),
                Some(false),
                None,
                Some(false),
                Some(true),
                None,
                None,
                Some(true),
                None,
                Some(true)
            ]
        );
    }

    #[test]
    fn disabled_brightness_hook_passes_down_repeat_up_without_routing() {
        use windows::Win32::UI::WindowsAndMessaging::WM_KEYUP;
        struct Owner;
        impl crate::capture_loop::CaptureLoop for Owner {
            fn set_wake(&mut self, _: crate::capture_loop::Wake) {}
            fn event(&mut self, _: &CapturedEvent) -> Disposition {
                panic!("brightness must stay native")
            }
            fn poll(&mut self, _: Instant) -> Option<Instant> {
                None
            }
            fn stopped(&self) -> bool {
                false
            }
            fn keyboard_disabled(&self) -> bool {
                true
            }
            fn keyboard_remote(&self) -> Option<bool> {
                Some(true)
            }
        }
        crate::capture_loop::install(Box::new(Owner), std::sync::Arc::new(|| {}));
        for scan in [0x54, 0x4C] {
            let info = KBDLLHOOKSTRUCT {
                scanCode: scan,
                flags: LLKHF_EXTENDED,
                ..Default::default()
            };
            for msg in [WM_KEYDOWN, WM_KEYDOWN, WM_KEYUP] {
                assert_eq!(
                    keyboard_proc_with_next(
                        0,
                        WPARAM(msg as usize),
                        LPARAM((&info as *const KBDLLHOOKSTRUCT) as isize),
                        || LRESULT(42)
                    ),
                    LRESULT(42)
                );
            }
        }
        crate::capture_loop::clear();
    }

    #[test]
    fn caps_capture_preserves_physical_down_repeat_up_for_source_gesture() {
        use windows::Win32::UI::WindowsAndMessaging::WM_KEYUP;
        let mut state = remote_state();
        let info = KBDLLHOOKSTRUCT {
            scanCode: 0x3a,
            vkCode: 0x14,
            ..Default::default()
        };
        for (msg, pressed) in [(WM_KEYDOWN, true), (WM_KEYDOWN, true), (WM_KEYUP, false)] {
            let Prepared::Event { event, force_pass, .. } = state.prepare_key(msg, &info, Instant::now()) else {
                panic!("Caps must reach the source router");
            };
            assert!(event.physical);
            assert!(!force_pass);
            assert_eq!(
                event.event,
                InputEvent::Key {
                    code: KeyCode::CAPS_LOCK,
                    pressed
                }
            );
            if pressed {
                state.keys.insert(KeyCode::CAPS_LOCK.0, true);
            }
        }
        assert!(!state.keys.contains_key(&KeyCode::CAPS_LOCK.0));
    }

    #[test]
    fn local_cursor_never_requests_a_raw_clip_even_if_remote_game_request_is_stale() {
        let mut state = remote_state();
        state.relative_requested = true;
        assert!(state.wants_raw_capture());
        state.mode = CaptureMode::Local;
        assert!(!state.wants_raw_capture());
        assert!(!state.hider.is_hidden());
    }

    #[test]
    fn third_party_injected_wheel_is_forwardable_during_native_touchpad_tail() {
        for msg in [WM_MOUSEWHEEL, WM_MOUSEHWHEEL] {
            for origin in [
                super::super::wheel_origin::Origin::Unknown,
                super::super::wheel_origin::Origin::DeviceLess,
            ] {
                let info = MSLLHOOKSTRUCT {
                    flags: LLMHF_INJECTED,
                    dwExtraInfo: 0x1234,
                    ..Default::default()
                };
                assert!(!suppress_native_wheel(msg, &info, true, origin));
            }
        }
    }

    #[test]
    fn opt_in_translation_preserves_legacy_external_source_routing() {
        use windows::Win32::UI::WindowsAndMessaging::{KBDLLHOOKSTRUCT_FLAGS, WM_KEYUP};
        let mut state = remote_state();
        for extra in [0, 0x1234, INJECT_TAG] {
            for injected in [false, true] {
                for (msg, pressed) in [(WM_KEYDOWN, true), (WM_KEYUP, false)] {
                    let info = KBDLLHOOKSTRUCT {
                        scanCode: 0x1e,
                        vkCode: 0x41,
                        dwExtraInfo: extra,
                        flags: KBDLLHOOKSTRUCT_FLAGS(if injected { LLKHF_INJECTED.0 | 2 } else { 0 }),
                        ..Default::default()
                    };
                    let prepared = state.prepare_key(msg, &info, Instant::now());
                    if extra == INJECT_TAG {
                        assert!(matches!(prepared, Prepared::Pass));
                    } else {
                        let Prepared::Event { event, .. } = prepared else {
                            panic!("foreign key must reach routing")
                        };
                        assert_eq!(event.physical, !injected);
                        assert_eq!(
                            event.event,
                            InputEvent::Key {
                                code: KeyCode::keyboard(4),
                                pressed
                            }
                        );
                    }
                }
                let info = MSLLHOOKSTRUCT {
                    dwExtraInfo: extra,
                    flags: if injected { LLMHF_INJECTED | 2 } else { 0 },
                    mouseData: 120 << 16,
                    ..Default::default()
                };
                for msg in [
                    WM_MOUSEWHEEL,
                    WM_MOUSEHWHEEL,
                    WM_LBUTTONDOWN,
                    WM_LBUTTONUP,
                    WM_RBUTTONDOWN,
                    WM_RBUTTONUP,
                ] {
                    let prepared = state.prepare_mouse(msg, &info, Instant::now());
                    if extra == INJECT_TAG {
                        assert!(matches!(prepared, Prepared::Pass));
                    } else {
                        let Prepared::Event { event, .. } = prepared else {
                            panic!("foreign pointer input must reach routing")
                        };
                        assert_eq!(event.physical, !injected);
                        assert!(matches!(
                            event.event,
                            InputEvent::Scroll { .. } | InputEvent::Button { .. }
                        ));
                    }
                }
            }
        }
    }

    /// No hook, native window or cursor cover is installed by these state-machine tests.
    pub(super) fn remote_state() -> State {
        let now = Instant::now();
        State {
            touchpad: super::super::touchpad::Touchpad::unavailable(),
            handler: std::sync::Arc::new(|_| Disposition::Pass),
            mode: CaptureMode::Remote {
                park: to_point(p(1920, 1080)),
            },
            park: p(1920, 1080),
            mouse_hook: None,
            kbd_hook: None,
            hider: Hider::new(Strategy::None),
            park_commit: Default::default(),
            park_marker: None,
            park_acceptance: None,
            pending: None,
            restore_watch: None,
            restoring_from_marker: None,
            transitions: 0,
            local_restore: None,
            restore_complete: None,
            hwnd: HWND::default(),
            raw: RawTracker::default(),
            raw_capture: super::super::raw_capture::RawCapture::default(),
            relative_requested: false,
            drag_anchor: None,
            keys: HashMap::new(),
            continuity: Default::default(),
            last_mouse_hook: now,
            last_kbd_hook: now,
            raw_registration_ok: true,
            ordinary_observer: false,
            capture_enabled: true,
            raw_seen_logged: (false, false),
            cmd_rx: mpsc::channel().1,
            stats: Stats::default(),
            pipeline: PipelineCounts::default(),
            diagnostic_ticks: 0,
        }
    }

    #[test]
    fn keyboard_guard_passes_physical_lock_curtain_input_without_treating_marked_injection_as_physical() {
        let injected = KBDLLHOOKSTRUCT {
            dwExtraInfo: INJECT_TAG,
            flags: LLKHF_INJECTED,
            ..Default::default()
        };
        assert!(!guard_passes_lock_input(&injected, true, true));
        let physical = KBDLLHOOKSTRUCT::default();
        assert!(guard_passes_lock_input(&physical, true, true));
        assert!(!guard_passes_lock_input(&physical, true, false));
        assert!(!guard_passes_lock_input(&physical, false, true));
    }

    #[test]
    fn keyboard_guard_never_treats_marked_synthetic_input_as_physical() {
        // Classification only: no hook installation or SendInput call.
        let tagged = KBDLLHOOKSTRUCT {
            dwExtraInfo: INJECT_TAG,
            flags: LLKHF_INJECTED,
            ..Default::default()
        };
        assert!(!physical_keyboard(&tagged));
        let tagged_without_flag = KBDLLHOOKSTRUCT {
            flags: Default::default(),
            ..tagged
        };
        assert!(!physical_keyboard(&tagged_without_flag));
    }

    pub(super) struct TestCursor;
    impl TestCursor {
        pub(super) fn at(p: POINT) -> Self {
            super::super::warp::reset();
            TEST_CURSOR.with(|c| c.set(Some(p)));
            Self
        }
    }
    impl Drop for TestCursor {
        fn drop(&mut self) {
            TEST_CURSOR.with(|c| c.set(None));
        }
    }

    #[test]
    fn held_catcher_is_never_replaced_by_the_regular_cover() {
        use super::super::cursor::test_native as native;
        let from = p(2178, 2156);
        let _native = native::Guard::at(from);
        let mut st = remote_state();
        st.hider = native::hider();
        st.drag_anchor = Some(oink_foundation::dnd::DragAnchor {
            generation: 1,
            at: to_point(from),
            surface: 1,
        });
        assert!(
            super::super::cursor::drag_anchor_ready(st.drag_anchor.unwrap()),
            "a visible OLE source cursor does not reject a stationary live catcher"
        );
        let stale = oink_foundation::dnd::DragAnchor {
            surface: 2,
            ..st.drag_anchor.unwrap()
        };
        assert!(!super::super::cursor::drag_anchor_ready(stale));
        st.execute(Transition::Remote(p(1920, 1080)), Via::InHook);
        assert_eq!(cursor_pos(), from);
        assert_eq!(st.park, from);
        assert!(!st.hider.park_pending());
        assert!(!st.hider.is_hidden(), "the catcher, not Hider, owns hiding");

        // Model native readback leaving the catcher. Its DragLeave callback
        // cancels source authority; pointer recovery must not cover that window.
        let moved = p(from.x + 20, from.y);
        native::warp(moved);
        let info = MSLLHOOKSTRUCT {
            pt: p(moved.x + 3, moved.y),
            ..Default::default()
        };
        let _ = st.prepare_mouse(WM_MOUSEMOVE, &info, Instant::now());
        assert!(!st.hider.park_pending());
        assert!(!st.hider.is_hidden());
        assert_eq!(cursor_pos(), moved);
    }

    #[test]
    fn held_source_parks_once_on_its_catcher_and_returns_to_the_new_entry() {
        let departure = p(0, 500);
        let corner = p(3839, 2159);
        let entry = p(2000, 0);
        let _cursor = TestCursor::at(departure);
        let mut st = remote_state();
        st.mode = CaptureMode::Local;
        st.drag_anchor = Some(oink_foundation::dnd::DragAnchor {
            generation: 17,
            at: to_point(corner),
            surface: 1,
        });
        // Synthetic mode markers carry the real transition tag. No SendInput,
        // hook installation, or user-desktop cursor is used by this test.
        let marker = st.request(Transition::Remote(p(1920, 1080))).unwrap();
        assert_eq!(marker.target, corner);
        assert!(is_marker(marker.extra));
        assert!(matches!(st.prepare_marker(corner, marker.extra), Prepared::Pass));
        assert_eq!(super::super::warp::sample(departure), corner);
        assert_eq!(st.park, corner);
        assert!(!st.hider.park_pending());
        TEST_CURSOR.with(|c| c.set(Some(corner))); // OS commits the accepted marker.
        let marker = st.request(Transition::Local(Some(entry))).unwrap();
        assert_eq!(marker.target, entry);
        assert!(matches!(st.prepare_marker(entry, marker.extra), Prepared::Pass));
        assert_eq!(super::super::warp::sample(corner), entry);
        assert!(matches!(st.mode, CaptureMode::Local));
    }

    #[test]
    fn held_source_timer_waits_for_swallowed_motion_and_uses_the_existing_warp_ledger() {
        let departure = p(0, 500);
        let corner = p(3839, 2159);
        let _cursor = TestCursor::at(departure);
        let mut st = remote_state();
        st.mode = CaptureMode::Local;
        st.drag_anchor = Some(oink_foundation::dnd::DragAnchor {
            generation: 18,
            at: to_point(corner),
            surface: 1,
        });
        st.execute(Transition::Remote(p(1920, 1080)), Via::Timer);
        assert_eq!(
            cursor_pos(),
            departure,
            "ordinary-message placement is not a park commit"
        );
        assert_eq!(st.park, departure);
        let _callback = InHook::enter();
        st.finish_ready_park();
        assert_eq!(cursor_pos(), corner);
        assert_eq!(st.park, corner);
        assert_eq!(super::super::warp::sample(departure), corner);
        assert!(!st.hider.park_pending());
    }

    #[test]
    fn ordinary_park_is_a_passed_absolute_input_for_every_departure_source() {
        use super::super::cursor::test_native as native;
        for via in [Via::InHook, Via::Marker, Via::Timer] {
            let from = p(1870, 2156);
            let park = p(1920, 1080);
            let _native = native::Guard::at(from);
            let mut st = remote_state();
            st.hider = native::hider();
            st.park = from;
            st.execute(Transition::Remote(park), via);
            native::set_showing(false);
            let marker = st.cover_acknowledged(1).expect("ACK queues the park input");
            assert_eq!(
                marker.target, park,
                "the input itself must move to park; a source marker plus warp leaves Shell hover behind"
            );
            assert_eq!(cursor_pos(), from, "no SetCursorPos before the input arrives");
            let _callback = InHook::enter();
            assert!(
                matches!(st.prepare_marker(park, marker.extra), Prepared::Pass),
                "the park input must reach Windows for every source"
            );
            assert_eq!(
                cursor_pos(),
                from,
                "the hook must not pre-warp before Windows applies the input"
            );
            assert_eq!(
                super::super::warp::sample(from),
                park,
                "accepted native target updates the exact displacement base before readback"
            );
        }
    }

    fn native_park_applies(st: &mut State, marker: Marker) {
        use super::super::cursor::test_native as native;
        {
            let _callback = InHook::enter();
            assert!(matches!(st.prepare_marker(marker.target, marker.extra), Prepared::Pass));
            st.park_chain_result(marker.extra, true);
        }
        assert!(
            st.hider.park_pending(),
            "cover cannot shrink before SendInput completes"
        );
        native::warp(marker.target); // Windows applies the allowed INPUT after the hook returns.
        st.finish_park_input(marker.extra, true);
        assert!(!st.hider.park_pending());
        assert_eq!(st.park, marker.target);
    }

    #[test]
    fn stationary_remote_departure_commits_cover_ack_without_a_physical_move() {
        use super::super::cursor::test_native as native;
        let from = p(1870, 2156);
        let park = p(1920, 1080);
        let _native = native::Guard::at(from);
        let mut st = remote_state();
        st.hider = native::hider();
        st.park = from;
        st.execute(Transition::Remote(park), Via::Marker);
        native::set_showing(false);
        let marker = st
            .cover_acknowledged(1)
            .expect("ACK schedules its own input even when the remote source stops");
        assert!(is_marker(marker.extra));
        assert_eq!(cursor_pos(), from, "ordinary window messages must not warp");
        native_park_applies(&mut st, marker);
        assert_eq!(cursor_pos(), park);
        assert!(st.cover_acknowledged(1).is_none());
    }

    #[test]
    fn cover_commit_marker_cannot_park_after_return_or_a_newer_hide_generation() {
        use super::super::cursor::test_native as native;
        let from = p(1870, 2156);
        let park = p(1920, 1080);
        let entry = p(1300, 2158);
        let _native = native::Guard::at(from);
        let mut st = remote_state();
        st.hider = native::hider();
        st.execute(Transition::Remote(park), Via::Marker);
        native::set_showing(false);
        let old = st.cover_acknowledged(1).unwrap();
        assert!(st.cover_acknowledged(1).is_none());
        let restore = st.request(Transition::Local(Some(entry))).unwrap();
        {
            let _callback = InHook::enter();
            assert!(matches!(st.prepare_marker(entry, restore.extra), Prepared::Pass));
        }
        native::warp(entry);
        assert!(st.uncover(restore.sequence));
        {
            let _callback = InHook::enter();
            assert!(matches!(st.prepare_marker(old.target, old.extra), Prepared::Swallow));
        }
        st.finish_park_input(old.extra, true);
        assert_eq!(cursor_pos(), entry);
        st.execute(Transition::Remote(park), Via::Marker);
        native::set_showing(false);
        let newer = st.cover_acknowledged(st.hider.park_generation().unwrap()).unwrap();
        {
            let _callback = InHook::enter();
            assert!(matches!(st.prepare_marker(old.target, old.extra), Prepared::Swallow));
        }
        st.finish_park_input(old.extra, true);
        assert!(st.hider.park_pending(), "old completion cannot shrink a new cover");
        assert_eq!(cursor_pos(), entry);
        native_park_applies(&mut st, newer);
        assert_eq!(cursor_pos(), park);
    }

    #[test]
    fn stuck_park_restore_resets_our_cover_to_a_visible_cursor_once() {
        use super::super::cursor::test_native as native;
        let from = p(3700, 900);
        let park = p(1920, 1080);
        let entry = p(3839, 900);
        let _native = native::Guard::at(from);
        let mut st = remote_state();
        st.hider = native::hider();
        st.execute(Transition::Remote(park), Via::Marker);
        native::set_showing(false);
        let departure = st.cover_acknowledged(1).unwrap();
        native_park_applies(&mut st, departure);
        let marker = st.request(Transition::Local(Some(entry))).unwrap();
        assert!(matches!(st.prepare_marker(entry, marker.extra), Prepared::Pass));
        // Windows accepted the matching hook target, but its position never applied.
        assert_eq!(cursor_pos(), park);
        st.uncover(marker.sequence);
        st.hider.finish_show(2);
        assert!(
            !st.hider.is_hidden(),
            "a refused uncover must reset our own hidden state"
        );
        assert_eq!(cursor_pos(), entry);
        assert_eq!(native::hidden(), Some(false));
        st.uncover(marker.sequence);
        assert_eq!(cursor_pos(), entry);
    }

    #[test]
    fn every_departure_path_waits_for_hide_and_return_commits_before_uncover() {
        use super::super::cursor::test_native as native;
        for via in [Via::InHook, Via::Marker, Via::Timer] {
            let from = p(2094, 2156);
            let park = p(1920, 1080);
            let entry = p(1809, 2158);
            let _native = native::Guard::at(from);
            let mut st = remote_state();
            st.hider = native::hider();
            st.park = from;
            st.execute(Transition::Remote(park), via);
            assert!(
                st.cover_acknowledged(1).is_none(),
                "visible cursor cannot authorize input"
            );
            assert_eq!(cursor_pos(), from);
            native::set_showing(false);
            let park_marker = st.cover_acknowledged(1).unwrap();
            {
                let _callback = InHook::enter();
                st.finish_ready_park();
            }
            assert_eq!(cursor_pos(), from, "a physical callback cannot pre-warp the park input");
            native_park_applies(&mut st, park_marker);
            let marker = st.request(Transition::Local(Some(entry))).unwrap();
            assert!(matches!(st.prepare_marker(entry, marker.extra), Prepared::Pass));
            {
                let _callback = InHook::enter();
                assert!(!st.uncover(marker.sequence), "restore input has not applied yet");
            }
            assert!(st.hider.is_hidden());
            native::warp(entry);
            assert!(st.uncover(marker.sequence));
            assert!(!st.hider.is_hidden());
            native::set_showing(true);
            assert!(st.cover_acknowledged(1).is_none());
            st.finish_park_input(park_marker.extra, true);
            assert_eq!(cursor_pos(), entry);
        }
    }

    #[test]
    fn ordinary_park_routes_inflight_steps_without_booking_its_native_displacement() {
        use super::super::cursor::test_native as native;
        for from in [p(2178, 2156), p(2224, 2156)] {
            let park = p(1920, 1080);
            let _native = native::Guard::at(from);
            let mut st = remote_state();
            st.hider = native::hider();
            st.park = from;
            let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = events.clone();
            st.handler = std::sync::Arc::new(move |event| {
                recorded.lock().unwrap().push(event.clone());
                Disposition::Swallow
            });
            st.execute(Transition::Remote(park), Via::InHook);
            native::set_showing(false);
            let marker = st.cover_acknowledged(1).unwrap();
            let info = MSLLHOOKSTRUCT {
                pt: p(from.x - 3, from.y + 5),
                ..Default::default()
            };
            {
                let _callback = InHook::enter();
                assert!(finish(Some(st.prepare_mouse(WM_MOUSEMOVE, &info, Instant::now()))));
                st.finish_ready_park();
                assert_eq!(cursor_pos(), from);
                assert!(matches!(st.prepare_marker(park, marker.extra), Prepared::Pass));
                assert_eq!(st.stale_base_for(info.pt, park), Some(from));
                assert!(finish(Some(st.prepare_mouse(WM_MOUSEMOVE, &info, Instant::now()))));
                // Native readback is deliberately still SOURCE. The next
                // hardware target is already based on the accepted park.
                let next = MSLLHOOKSTRUCT {
                    pt: p(park.x - 1, park.y + 1),
                    ..Default::default()
                };
                assert!(finish(Some(st.prepare_mouse(WM_MOUSEMOVE, &next, Instant::now()))));
            }
            let recorded = events.lock().unwrap();
            assert_eq!(recorded.len(), 3, "park input itself never reaches the router");
            assert_eq!(recorded[0].event, InputEvent::PointerDelta { dx: -3.0, dy: 5.0 });
            assert_eq!(recorded[1].event, InputEvent::PointerDelta { dx: -3.0, dy: 5.0 });
            assert_eq!(recorded[2].event, InputEvent::PointerDelta { dx: -1.0, dy: 1.0 });
            drop(recorded);
            assert_eq!(
                st.stale_base_for(info.pt, park),
                Some(from),
                "a swallowed fresh target does not commit or retire the displaced native base"
            );
            super::super::warp::passed(from);
            assert_eq!(
                st.stale_base_for(info.pt, park),
                None,
                "a passed target retires its old alias"
            );
            native::warp(park);
            st.finish_park_input(marker.extra, true);
            assert_eq!(st.park, park);
        }
    }

    #[test]
    fn ordinary_park_failure_is_terminal_and_never_enables_raw_capture() {
        use super::super::cursor::test_native as native;
        // Failed SendInput, successful send without a hook, clipped target,
        // lost hide, downstream rejection, and mismatched native readback.
        for failure in 0..6 {
            let from = p(1870, 2156);
            let park = p(1920, 1080);
            let _native = native::Guard::at(from);
            let mut st = remote_state();
            st.hider = native::hider();
            st.park = from;
            st.execute(Transition::Remote(park), Via::Marker);
            native::set_showing(false);
            let marker = st.cover_acknowledged(1).unwrap();
            st.relative_requested = true;
            assert!(!st.wants_raw_capture());
            match failure {
                0 => st.finish_park_input(marker.extra, false),
                1 => st.finish_park_input(marker.extra, true),
                2 => assert!(matches!(st.prepare_marker(from, marker.extra), Prepared::Swallow)),
                3 => {
                    native::set_showing(true);
                    assert!(matches!(st.prepare_marker(park, marker.extra), Prepared::Swallow));
                    native::set_showing(false);
                }
                4 => {
                    assert!(matches!(st.prepare_marker(park, marker.extra), Prepared::Pass));
                    assert_eq!(super::super::warp::sample(from), park);
                    st.park_chain_result(marker.extra, false);
                    assert_eq!(
                        super::super::warp::sample(from),
                        from,
                        "rejected native input must not poison D3's base"
                    );
                    st.finish_park_input(marker.extra, true);
                }
                5 => {
                    assert!(matches!(st.prepare_marker(park, marker.extra), Prepared::Pass));
                    st.finish_park_input(marker.extra, true); // readback still source
                }
                _ => unreachable!(),
            }
            assert_eq!(cursor_pos(), from);
            assert!(st.hider.park_pending(), "keep expanded cover on failure");
            assert!(!st.wants_raw_capture());
            assert!(st.cover_acknowledged(1).is_none(), "failure never silently resends");
            assert!(matches!(st.prepare_marker(park, marker.extra), Prepared::Swallow));
        }
    }

    #[test]
    fn rejected_park_rolls_back_even_if_a_reentrant_return_cancelled_its_state() {
        use super::super::cursor::test_native as native;
        let from = p(1870, 2156);
        let park = p(1920, 1080);
        let _native = native::Guard::at(from);
        let mut st = remote_state();
        st.hider = native::hider();
        st.execute(Transition::Remote(park), Via::InHook);
        native::set_showing(false);
        let marker = st.cover_acknowledged(1).unwrap();
        assert!(matches!(st.prepare_marker(park, marker.extra), Prepared::Pass));
        let acceptance = st.take_park_acceptance(marker.extra).unwrap();
        {
            let _callback = InHook::enter();
            st.request(Transition::Local(None)); // reentry while the downstream hook runs
        }
        super::super::warp::rejected(acceptance);
        st.park_chain_result(marker.extra, false);
        st.finish_park_input(marker.extra, true);
        assert_eq!(cursor_pos(), from);
        assert_eq!(super::super::warp::sample(from), from);
    }

    #[test]
    fn ordinary_game_clip_waits_until_the_input_has_applied_and_completed() {
        use super::super::cursor::test_native as native;
        let from = p(1870, 2156);
        let park = p(1920, 1080);
        let _native = native::Guard::at(from);
        let mut st = remote_state();
        st.hider = native::hider();
        st.execute(Transition::Remote(park), Via::InHook);
        native::set_showing(false);
        let marker = st.cover_acknowledged(1).unwrap();
        st.relative_requested = true;
        assert!(!st.wants_raw_capture());
        assert!(matches!(st.prepare_marker(park, marker.extra), Prepared::Pass));
        assert!(!st.wants_raw_capture(), "do not clip before Windows applies this input");
        native::warp(park);
        assert!(!st.wants_raw_capture(), "the cover must complete first");
        st.relative_requested = false; // Test the policy without launching a real clip guard.
        st.finish_park_input(marker.extra, true);
        st.relative_requested = true;
        assert!(st.wants_raw_capture());
    }

    #[test]
    fn reentrant_own_move_is_accounted_and_unvalidated_marker_is_swallowed() {
        let park = p(1920, 1080);
        let entry = p(2170, 2159);
        let _cursor = TestCursor::at(park);
        let mut info = MSLLHOOKSTRUCT {
            pt: entry,
            dwExtraInfo: INJECT_TAG,
            ..Default::default()
        };
        STATE.with(|state| {
            let _borrow = state.borrow_mut();
            account_own_mouse(WM_MOUSEMOVE, &info);
            assert_eq!(super::super::warp::sample(park), entry);
            info.dwExtraInfo = TRANSITION_TAG;
            let result = mouse_proc_inner(
                0,
                WPARAM(WM_MOUSEMOVE as usize),
                LPARAM((&info as *const MSLLHOOKSTRUCT) as isize),
            );
            assert_eq!(result.0, 1, "a marker needs a validated request even on reentry");
        });
    }

    #[test]
    fn direct_local_entry_and_all_tagged_move_builders_share_the_accounted_base() {
        let park = p(1920, 1080);
        let entry = p(2170, 2159);
        let physical = MSLLHOOKSTRUCT {
            pt: p(2169, 2159),
            ..Default::default()
        };
        let assert_step = |st: &mut State| {
            // The next hook already uses entry although native readback lags.
            TEST_CURSOR.with(|c| c.set(Some(park)));
            let Prepared::Event { event, .. } = st.prepare_mouse(WM_MOUSEMOVE, &physical, Instant::now()) else {
                panic!("physical movement must be routed");
            };
            assert_eq!(event.event, InputEvent::PointerDelta { dx: -1.0, dy: 0.0 });
        };
        for via in [Via::InHook, Via::Timer] {
            let _cursor = TestCursor::at(park);
            let mut st = remote_state();
            st.execute(Transition::Local(Some(entry)), via);
            assert_step(&mut st);
        }
        use windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE;
        for input in [
            inject::move_abs(1, 2),
            inject::drag_press(1, 2),
            inject::mouse(1, 2, 0, MOUSEEVENTF_MOVE),
        ] {
            let _cursor = TestCursor::at(park);
            let mut st = remote_state();
            st.mode = CaptureMode::Local;
            // Both direct SendInput and the helper preserve this tag. Windows
            // resolves normalization/clipping before invoking this same branch.
            let own = MSLLHOOKSTRUCT {
                pt: entry,
                flags: LLMHF_INJECTED,
                // SAFETY: each builder above constructs INPUT_MOUSE.
                dwExtraInfo: unsafe { input.Anonymous.mi.dwExtraInfo },
                ..Default::default()
            };
            assert_eq!(own.dwExtraInfo, INJECT_TAG);
            account_own_mouse(WM_MOUSEMOVE, &own);
            assert!(matches!(st.prepare_own_mouse(WM_MOUSEMOVE, &own), Prepared::Pass));
            account_own_mouse(WM_LBUTTONDOWN, &own);
            assert!(matches!(st.prepare_own_mouse(WM_LBUTTONDOWN, &own), Prepared::Pass));
            assert_step(&mut st);
        }
    }

    #[test]
    fn tagged_refresh_echoes_are_own_input_and_keep_real_motion_origins() {
        let entry = p(1200, 2100);
        let park = p(1920, 1080);
        let _cursor = TestCursor::at(park);
        super::super::warp::accepted(entry, park);
        for target in [p(park.x + 1, park.y), park] {
            let echo = MSLLHOOKSTRUCT {
                pt: target,
                dwExtraInfo: inject::SHAPE_REFRESH_TAG,
                ..Default::default()
            };
            assert!(inject::own_mouse_tag(echo.dwExtraInfo));
            assert!(!is_marker(echo.dwExtraInfo));
            assert_eq!(
                external::mouse_origin(&echo),
                crate::windows_external_input::Origin::Own
            );
            // Account before borrowing State, including a reentrant own echo.
            STATE.with(|state| {
                let _borrow = state.borrow_mut();
                account_own_mouse(WM_MOUSEMOVE, &echo);
            });
            TEST_CURSOR.with(|c| c.set(Some(target)));
        }
        let mut st = remote_state();
        let next = MSLLHOOKSTRUCT {
            pt: p(park.x + 4, park.y - 2),
            ..Default::default()
        };
        let Prepared::Event { event, .. } = st.prepare_mouse(WM_MOUSEMOVE, &next, Instant::now()) else {
            panic!("physical motion must reach routing");
        };
        assert_eq!(event.event, InputEvent::PointerDelta { dx: 4., dy: -2. });
        assert_eq!(super::super::warp::sample(entry), park);
        assert_eq!(st.stale_base_for(p(entry.x + 4, entry.y - 2), park), Some(entry));
    }

    #[test]
    fn remote_shape_refresh_preserves_the_forwarded_motion_sum() {
        let park = p(1920, 1080);
        let _cursor = TestCursor::at(park);
        let mut st = remote_state();
        // Same accounting as cursor::nudge, with no native input or placement.
        let nudge = p(park.x + 1, park.y);
        super::super::warp::accepted_refresh(park, nudge);
        super::super::warp::accepted_refresh(nudge, park);
        let mut forwarded = Point::new(0., 0.);
        for _ in 0..100 {
            let info = MSLLHOOKSTRUCT {
                pt: p(park.x + 4, park.y - 2),
                ..Default::default()
            };
            let Prepared::Event {
                event, force_swallow, ..
            } = st.prepare_mouse(WM_MOUSEMOVE, &info, Instant::now())
            else {
                panic!("physical movement must be forwarded");
            };
            let InputEvent::PointerDelta { dx, dy } = event.event else {
                panic!("expected ordinary movement");
            };
            assert_eq!(event.cursor, None);
            assert!(force_swallow);
            forwarded.x += dx;
            forwarded.y += dy;
        }
        assert_eq!(forwarded, Point::new(400., -200.));
    }

    #[test]
    fn local_shape_refresh_does_not_swallow_the_rightward_arc() {
        let entry = p(1200, 2100);
        let _cursor = TestCursor::at(entry);
        let mut st = remote_state();
        st.mode = CaptureMode::Local;
        // Same accounting as cursor::nudge, without native placement or input.
        let nudge = p(entry.x + 1, entry.y);
        super::super::warp::accepted_refresh(entry, nudge);
        super::super::warp::accepted_refresh(nudge, entry);
        let mut native = entry;
        for _ in 0..100 {
            let info = MSLLHOOKSTRUCT {
                pt: p(native.x + 4, native.y - 2),
                ..Default::default()
            };
            let Prepared::Event {
                event, force_swallow, ..
            } = st.prepare_mouse(WM_MOUSEMOVE, &info, Instant::now())
            else {
                panic!("physical movement must be routed");
            };
            assert_eq!(event.event, InputEvent::PointerDelta { dx: 4., dy: -2. });
            assert_eq!(event.cursor, Some(to_point(info.pt)));
            assert!(
                !force_swallow,
                "a Local shape-refresh alias cannot reject physical motion"
            );
            native = info.pt;
            super::super::warp::passed(native);
            TEST_CURSOR.with(|c| c.set(Some(native)));
        }
        assert_eq!(native, p(1600, 1900));
    }

    #[test]
    fn park_readback_reversal_cannot_forward_the_program_displacement() {
        let from = p(2331, 2148);
        let park = p(1920, 1080);
        let _cursor = TestCursor::at(from);
        let mut st = remote_state();
        st.park = park;
        st.mode = CaptureMode::Remote { park: to_point(park) };
        super::super::warp::place(park).unwrap();
        for observed in [park, from, from, park, from] {
            TEST_CURSOR.with(|c| c.set(Some(observed)));
            let info = MSLLHOOKSTRUCT {
                pt: p(1920, 1084),
                ..Default::default()
            };
            let Prepared::Event {
                event, force_swallow, ..
            } = st.prepare_mouse(WM_MOUSEMOVE, &info, Instant::now())
            else {
                panic!("physical movement must be routed");
            };
            assert_eq!(event.event, InputEvent::PointerDelta { dx: 0., dy: 4. });
            assert_eq!(event.cursor, None);
            assert!(force_swallow);
        }
    }

    #[test]
    fn entry_marker_fresh_step_uses_committed_base_even_before_native_readback() {
        let mut st = remote_state();
        let park = st.park;
        let entry = p(2170, 2159);
        let _cursor = TestCursor::at(park);
        let marker = st.request(Transition::Local(Some(entry))).unwrap();
        assert!(matches!(st.prepare_marker(entry, marker.extra), Prepared::Pass));
        let info = MSLLHOOKSTRUCT {
            pt: p(2169, 2159),
            ..Default::default()
        };
        let stale = st.stale_base_for(info.pt, super::super::warp::sample(cursor_pos()));
        assert!(stale.is_none());
        let Prepared::Event { event, .. } = st.prepare_mouse(WM_MOUSEMOVE, &info, Instant::now()) else {
            panic!("physical step must still be routed");
        };
        assert_eq!(event.event, InputEvent::PointerDelta { dx: -1.0, dy: 0.0 });
        assert_eq!(event.cursor, Some(to_point(info.pt)));
    }

    #[test]
    fn restore_marker_commits_after_cursor_state_changes_during_callback() {
        let mut st = remote_state();
        let park = st.park;
        let entry = p(451, 2156);
        let _cursor = TestCursor::at(park);
        let old_step = MSLLHOOKSTRUCT {
            pt: p(1920, 1079),
            ..Default::default()
        };

        // A remote move is swallowed before the restore request arrives.
        assert!(finish(Some(st.prepare_mouse(WM_MOUSEMOVE, &old_step, Instant::now()))));
        let marker = st.request(Transition::Local(Some(entry))).unwrap();
        // The actual desktop hook delivered only the low 32 bits.
        let delivered_extra = marker.extra as u32 as usize;
        assert_eq!(marker.extra, delivered_extra);
        assert!(is_marker(delivered_extra));
        let decision = st.prepare_marker(entry, delivered_extra);
        assert_eq!(st.mode, CaptureMode::Local);
        assert_eq!(cursor_pos(), park, "the callback must not place the cursor out of band");

        // Model the observed desktop interleaving: cursor state can be park
        // during the restore callback, despite an earlier SetCursorPos(entry).
        // The experiment does not identify which Windows internal operation
        // writes park; this test deliberately makes no claim about that source.
        TEST_CURSOR.with(|c| c.set(Some(park)));
        let stale = st.stale_base_for(old_step.pt, super::super::warp::sample(cursor_pos()));
        assert_eq!(
            stale,
            Some(park),
            "recognize old coordinates even before the marker commits"
        );
        assert!(finish(Some(st.prepare_mouse(WM_MOUSEMOVE, &old_step, Instant::now()))));

        // The marker itself commits its absolute position. SendInput's return,
        // rather than a posted message from inside its hook, authorizes uncover.
        assert!(
            !finish(Some(decision)),
            "swallowing the marker loses the native placement"
        );
        TEST_CURSOR.with(|c| c.set(Some(entry)));
        assert!(st.uncover(marker.sequence));
        assert_eq!(cursor_pos(), entry);
        assert!(st.restoring_from_marker.is_none());
        assert_eq!(st.stale_base_for(p(entry.x, entry.y - 1), entry), None);
        assert_eq!(
            st.restore_watch
                .as_mut()
                .unwrap()
                .check(entry, p(entry.x, entry.y - 1), true),
            None
        );
    }

    #[test]
    fn stale_markers_and_completions_do_not_apply_to_a_new_restore() {
        let mut st = remote_state();
        let _cursor = TestCursor::at(st.park);
        let first = p(451, 2156);
        let second = p(869, 2158);
        let old = st.request(Transition::Local(Some(first))).unwrap();
        assert!(matches!(st.prepare_marker(first, old.extra), Prepared::Pass));
        let new = st.request(Transition::Local(Some(second))).unwrap();
        assert!(!st.uncover(old.sequence));
        assert!(matches!(st.prepare_marker(first, old.extra), Prepared::Swallow));
        assert!(st.pending.is_some());
        assert!(matches!(st.prepare_marker(second, new.extra), Prepared::Pass));
        assert!(!st.uncover(old.sequence));
        assert_eq!(st.restoring_from_marker, Some(second));
        TEST_CURSOR.with(|c| c.set(Some(second)));
        assert!(st.uncover(new.sequence));
        assert!(matches!(st.prepare_marker(second, new.extra), Prepared::Swallow));

        let same_old = st.request(Transition::Local(Some(second))).unwrap();
        let same_new = st.request(Transition::Local(Some(second))).unwrap();
        assert!(matches!(st.prepare_marker(second, same_old.extra), Prepared::Swallow));
        assert!(st.pending.is_some());
        assert!(matches!(st.prepare_marker(second, same_new.extra), Prepared::Pass));
        assert!(!st.uncover(same_old.sequence));
        assert!(st.uncover(same_new.sequence));

        // A late marker after the timer fallback must remain inert, too.
        let late = st.request(Transition::Local(Some(first))).unwrap();
        let fallback = st.pending.take().unwrap().transition;
        st.execute(fallback, Via::Timer);
        assert_eq!(cursor_pos(), first);
        assert!(matches!(st.prepare_marker(first, late.extra), Prepared::Swallow));
        assert!(st.uncover(late.sequence));
    }

    #[test]
    fn key_capture_metadata_reports_kind_without_guessing_repeat() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let output = seen.clone();
        let handler: EventHandler = std::sync::Arc::new(move |_| {
            output.lock().unwrap().push(crate::diagnostics::capture_metadata());
            Disposition::Pass
        });
        let code = crate::KeyCode::keyboard(0x6e);
        for (pressed, track_key) in [(true, Some(code.0)), (true, None), (false, None)] {
            assert!(!finish(Some(Prepared::Event {
                event: CapturedEvent {
                    physical: false,
                    event: InputEvent::Key { code, pressed },
                    source: SourceKind::Unknown,
                    time_us: 0,
                    cursor: None,
                },
                handler: handler.clone(),
                force_swallow: false,
                force_pass: false,
                track_key,
            })));
        }
        let rows = seen.lock().unwrap();
        use crate::diagnostics::RawKeyEvent::{Down, Up};
        assert_eq!(rows.iter().map(|r| r.raw).collect::<Vec<_>>(), [Down, Down, Up]);
        assert_eq!(rows.iter().map(|r| r.repeat).collect::<Vec<_>>(), [None, None, None]);
        assert!(rows.iter().all(|r| r.backend_generated == Some(false)));
        assert_eq!(
            crate::diagnostics::capture_metadata(),
            crate::diagnostics::CaptureMetadata::default()
        );
    }

    fn p(x: i32, y: i32) -> POINT {
        POINT { x, y }
    }

    /// 回到 Windows（4K，停靠点在屏幕中央）：进入点在底边。
    fn watch() -> RestoreWatch {
        RestoreWatch::new(p(2988, 2159), p(1920, 1080), Via::Marker)
    }

    #[test]
    fn a_lost_hook_is_found_per_device_kind() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        // 键盘钩子最后一次被调用在 0 ms；Raw Input 在 1500 ms 看到键盘输入；1800 ms 核对：丢了。
        assert!(hook_lost(ms(1800), Some(ms(1500)), t0));
        // 钩子在 Raw Input 之前刚被调用过（同一个事件）：没丢。
        assert!(!hook_lost(ms(1800), Some(ms(1500)), ms(1499)));
        // 没有这类设备的输入：不判断（例如只动了鼠标，键盘钩子自然没被调用）。
        assert!(!hook_lost(ms(5000), None, t0));
        // Raw Input 的输入太久以前（之后钩子也没被调用、设备也没再动）：不重复判断。
        assert!(!hook_lost(ms(9000), Some(ms(1500)), t0));
    }

    #[test]
    fn moves_that_start_from_the_entry_point_are_quiet() {
        let mut w = watch();
        assert_eq!(w.check(p(2988, 2159), p(2988, 2131), false), None);
        assert_eq!(w.check(p(2988, 2131), p(2988, 2125), true), None);
        // 系统原地「移动」时可能差一个像素；上一步被吞掉时差一步
        assert_eq!(w.check(p(2989, 2125), p(2988, 2074), false), None);
        assert_eq!(w.check(p(2988, 2080), p(2988, 2068), true), None);
    }

    #[test]
    fn a_cursor_back_at_the_park_point_is_reported_once() {
        let mut w = watch();
        assert_eq!(w.check(p(2988, 2159), p(2988, 2131), false), None);
        assert_eq!(
            w.check(p(1920, 1080), p(1922, 1078), true),
            Some(RestoreAnomaly::NotWherePlaced {
                expected: p(2988, 2131),
                at_park: true
            })
        );
        assert_eq!(w.check(p(1922, 1078), p(1924, 1076), true), None);
        assert_eq!(w.check(p(1920, 1080), p(1922, 1078), true), None);
    }

    #[test]
    fn the_first_move_is_checked_against_the_entry_point() {
        let mut w = watch();
        assert_eq!(
            w.check(p(100, 100), p(101, 100), true),
            Some(RestoreAnomaly::NotWherePlaced {
                expected: p(2988, 2159),
                at_park: false
            })
        );
    }

    /// Windows 实测（2026-09-29）：光标已放到进入点，本机鼠标的下一步却是从停靠点算的。
    #[test]
    fn a_mouse_step_computed_from_the_park_point_is_reported() {
        let mut w = watch();
        assert_eq!(
            w.check(p(2988, 2159), p(1920, 1074), true),
            Some(RestoreAnomaly::StepFromElsewhere { at_park: true })
        );
        assert_eq!(w.check(p(1920, 1074), p(1920, 1068), true), None);
    }

    #[test]
    fn our_absolute_moves_may_jump() {
        let mut w = watch();
        assert_eq!(w.check(p(2988, 2159), p(1500, 900), false), None);
    }
}
