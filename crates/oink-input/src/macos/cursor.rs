//! 光标控制：Remote 模式下立即隐藏、解耦并冻结；回到 Local、停止或退出时恢复。
//!
//! 远程模式期间不 warp（不停到屏幕中央、不重新停靠）：每次 warp 都会被系统算进下一个硬件移动事件的
//! 位移，之后约 250 ms 事件位置冻结；远程模式下事件位置又不是光标位置，warp 量无从可靠地计算
//! (`crate::warp_delta`). When frozen at the exit edge, event positions remain
//! clamped to that edge; the displacement fields are unaffected.
//!
//! 这些调用都是发给 WindowServer 的同步 IPC，**不能在事件 tap 回调里执行**（WindowServer 正等着回调返回），
//! 由 `mod.rs` 保证在截获线程的回调之外调用。

use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use objc2_core_foundation::{CGPoint, CGRect};
#[allow(deprecated)]
use objc2_core_graphics::CGSetLocalEventsSuppressionInterval;
use objc2_core_graphics::{
    CGAssociateMouseAndMouseCursorPosition, CGDirectDisplayID, CGDisplayBounds, CGDisplayHideCursor,
    CGDisplayShowCursor, CGError, CGEvent, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGGetActiveDisplayList, CGMainDisplayID, CGMouseButton, CGWarpMouseCursorPosition,
};
use oink_foundation::Point;
use oink_foundation::handoff_timing::{self, Stage as TimingStage};

use super::system;

#[path = "cursor_owner.rs"]
pub(crate) mod owner;

#[path = "cursor_frame.rs"]
mod frame;

/// 进程级状态：退出钩子据此判断需要恢复什么。
static HIDDEN: AtomicBool = AtomicBool::new(false);
// Desired hidden state is separate from a successful native hide request. Never stack hides.
static HIDE_REQUESTED: AtomicBool = AtomicBool::new(false);
static DISSOCIATED: AtomicBool = AtomicBool::new(false);
static SOURCE_DISABLED: AtomicBool = AtomicBool::new(false);
static CONFINED: AtomicBool = AtomicBool::new(false);

/// Called under the capture cursor lock, outside native callbacks.
pub(crate) fn constrain(active: bool) {
    CONFINED.store(active, Ordering::Release);
    source_disabled(SOURCE_DISABLED.load(Ordering::Acquire));
}

/// Capture-thread reconciliation, outside event callbacks. Disable physical
/// motion without changing visibility/position or blocking tagged remote input.
pub(crate) fn source_disabled(disabled: bool) {
    SOURCE_DISABLED.store(disabled, Ordering::Release);
    let dissociate = disabled || CONFINED.load(Ordering::Acquire) || HIDDEN.load(Ordering::Acquire);
    if DISSOCIATED.load(Ordering::Acquire) != dissociate {
        install_exit_hooks();
        let result = CGAssociateMouseAndMouseCursorPosition(!dissociate);
        if result.0 == 0 {
            DISSOCIATED.store(dissociate, Ordering::Release);
        }
    }
}
/// 我们是否已解耦（本模块记录的状态；系统没有查询接口，别的程序改了关联我们不知道）。只用于日志。
pub(crate) fn dissociated() -> bool {
    DISSOCIATED.load(Ordering::Acquire)
}

/// 当前是否要求隐藏光标；不是 WindowServer 已落实的证明。只用于日志。
pub(crate) fn hidden() -> bool {
    HIDDEN.load(Ordering::Acquire)
}

/// 冻结的第一步：隐藏，随后由 `freeze` 解耦。可重复调用。
///
/// The native hide changes WindowServer's logical state. Request a display update as well
/// so a quiet desktop commits the hidden hardware cursor before unrelated damage arrives.
pub(crate) fn hide() {
    install_exit_hooks();
    if !HIDDEN.swap(true, Ordering::AcqRel) {
        system::allow_background_cursor_hiding();
        handoff_timing::native(TimingStage::HideStarted, 0);
        let mut e1 = None;
        let e0 = hide_with_update(
            || {
                let e0 = CGDisplayHideCursor(CGMainDisplayID());
                handoff_timing::native(TimingStage::HideCompleted, e0.0 as u32);
                HIDE_REQUESTED.store(e0.0 == 0, Ordering::Release);
                crate::native_lifecycle::emit_lifecycle(
                    crate::native_lifecycle::Kind::Hide,
                    crate::native_lifecycle::Data {
                        hidden: Some(true),
                        success: Some(e0.0 == 0),
                        ..Default::default()
                    },
                );
                e1 = (!DISSOCIATED.load(Ordering::Acquire)).then(|| CGAssociateMouseAndMouseCursorPosition(true));
                e0
            },
            request_hidden_frame,
        );
        // SAFETY: 无参数的查询。
        let visible = unsafe { CGCursorIsVisible() } != 0;
        // This is WindowServer's visibility flag after submitting the display update,
        // not a rendered-frame observation or proof about a transparent glyph.
        handoff_timing::native(
            TimingStage::HideFlushed,
            u32::from(visible)
                | (u32::from(e0.0 != 0) << 1)
                | (u32::from(e1.as_ref().is_some_and(|error| error.0 != 0)) << 2),
        );
        if e0.0 != 0 || visible {
            // 只记录（CGCursorIsVisible 不一定可靠）。
            tracing::warn!(
                ?e0,
                ?e1,
                visible,
                "hide cursor: WindowServer still reports it visible or the call failed"
            );
        } else {
            tracing::debug!(?e0, ?e1, "hide cursor");
        }
    }
}

/// Re-establish the single hide request after our window has taken cursor ownership.
/// The old background hide may have been ignored by Dock. The transparent glyph covers the
/// balanced show/hide transition; this is an ownership handshake, never a timer/retry loop.
pub(crate) fn hide_owned() {
    if HIDDEN.load(Ordering::Acquire) {
        handoff_timing::native(TimingStage::HideStarted, 1); // renew after blank-glyph ownership
        let ok = renew_hide();
        handoff_timing::native(TimingStage::HideCompleted, u32::from(!ok));
    }
}

/// Balance only a successful request. A rejected show must not be followed by another hide.
fn renew_hide() -> bool {
    let requested = HIDE_REQUESTED.load(Ordering::Acquire);
    let (next, ok) = replace_request(
        requested,
        || CGDisplayShowCursor(CGMainDisplayID()).0 == 0,
        || hide_with_update(|| CGDisplayHideCursor(CGMainDisplayID()), request_hidden_frame).0 == 0,
    );
    HIDE_REQUESTED.store(next, Ordering::Release);
    ok
}

fn request_hidden_frame() {
    if let Err(reason) = frame::request_update() {
        tracing::warn!(reason, "cursor hide display update was not submitted");
    }
}

// A frame submission failure must not turn a successful hide into a failed request:
// the matching Show is still owed, and another Hide would stack the native hide count.
fn hide_with_update(hide: impl FnOnce() -> CGError, update: impl FnOnce()) -> CGError {
    let result = hide();
    if result.0 == 0 {
        update();
    }
    result
}

fn replace_request(requested: bool, show: impl FnOnce() -> bool, hide: impl FnOnce() -> bool) -> (bool, bool) {
    if requested && !show() {
        return (true, false);
    }
    let ok = hide();
    (ok, ok)
}

/// Called outside the tap callback, under cursor_lock. Only repairs visibility: never posts a
/// move, warps, changes focus, re-associates the mouse, or restarts the owner handshake.
#[derive(Default)]
pub(crate) struct Reconcile {
    next: Option<Instant>,
    failures: u32,
}

impl Reconcile {
    fn due(&mut self, hidden: bool, visible: bool, transition: bool, now: Instant) -> bool {
        if transition || !hidden || !visible {
            self.next = None;
            self.failures = 0;
        }
        hidden && visible && self.next.is_none_or(|next| now >= next)
    }

    fn attempted(&mut self, now: Instant, visible: bool) {
        if visible {
            self.failures = self.failures.saturating_add(1);
            self.next = Some(now + Duration::from_secs((1u64 << self.failures.min(6).saturating_sub(1)).min(30)));
        } else {
            self.failures = 0;
            self.next = None;
        }
    }

    fn visibility(&mut self, wanted: bool, transition: bool, query: impl FnOnce() -> bool) -> Option<bool> {
        if !wanted && !transition {
            // There is no hide request to repair in ordinary local mode. The
            // native visibility read can enter WindowServer; do not pay for it
            // on every idle observation. Preserve session/display diagnostics
            // and the existing remote-hide reconciliation cadence.
            self.next = None;
            self.failures = 0;
            return None;
        }
        Some(query())
    }

    pub(crate) fn check(&mut self, transition: bool, remote: bool) {
        let wanted = hidden() && remote;
        let Some(before) = self.visibility(wanted, transition, || {
            // SAFETY: argument-free read, outside input callback.
            unsafe { CGCursorIsVisible() != 0 }
        }) else {
            return;
        };
        if self.due(wanted, before, transition, Instant::now()) {
            let ok = renew_hide();
            // CG visibility is a diagnostic heuristic, not proof of the displayed bitmap.
            // SAFETY: argument-free query outside the input callback.
            let visible = unsafe { CGCursorIsVisible() } != 0;
            self.attempted(Instant::now(), !ok || visible);
            tracing::info!(wanted, before, visible, native_ok = ok,
                retry = self.failures, transition, locked = ?super::session::locked(),
                "cursor hidden state reconciled");
        } else if transition {
            tracing::info!(wanted, visible = before, locked = ?super::session::locked(),
                "cursor hidden state checked after session/display change");
        }
    }
}

unsafe extern "C" {
    /// 已标为废弃，但仍可用；只用于日志。
    fn CGCursorIsVisible() -> u32;
}

/// 冻结：先隐藏再解耦，不 warp（bug 3：光标不会在别处露出来）。可重复调用。
pub(crate) fn freeze() {
    hide();
    if !DISSOCIATED.swap(true, Ordering::AcqRel) {
        let _ = CGAssociateMouseAndMouseCursorPosition(false);
    }
}

pub(crate) fn freeze_checked() -> Result<(), crate::InputError> {
    hide();
    let result = CGAssociateMouseAndMouseCursorPosition(false);
    if result.0 != 0 {
        return Err(crate::InputError::Platform(format!(
            "source cursor freeze failed: {}",
            result.0
        )));
    }
    DISSOCIATED.store(true, Ordering::Release);
    Ok(())
}

/// 光标隐藏、解耦时把它放到 `to`：投递一个带注入标记的鼠标移动事件（不是 warp）。系统照常处理这个事件——
/// 放行的事件在解耦时照样移动光标——Dock 等看到光标离开了它们的区域就收起（实测 warp 不行：Dock 只在收到
/// 事件时重新判断，warp 到 Dock 上方之后 Dock 一直不收起）。截获回调认出
/// 注入标记、原样放行。
pub(crate) fn post_move(to: Point, tag: i64) -> bool {
    let Some(src) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        return false;
    };
    CGEventSource::set_user_data(Some(&src), tag);
    let p = super::inject::quartz_point(to);
    let Some(ev) = CGEvent::new_mouse_event(Some(&src), CGEventType::MouseMoved, p, CGMouseButton::Left) else {
        return false;
    };
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
    true
}

/// 本机所有活动显示器的范围（全局坐标，pt）。CoreGraphics 的显示器查询，可以在任意线程调用。
pub(crate) fn display_bounds() -> Vec<oink_foundation::Rect> {
    let mut ids: [CGDirectDisplayID; 16] = [0; 16];
    let mut n: u32 = 0;
    // SAFETY: 缓冲区与计数指针有效，最多写入 16 个。
    let err = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n) };
    if err.0 != 0 {
        return Vec::new();
    }
    ids[..(n as usize).min(ids.len())]
        .iter()
        .map(|id| {
            let r: CGRect = CGDisplayBounds(*id);
            oink_foundation::Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
        })
        .collect()
}

/// 焦点回到本机：光标仍隐藏（且解耦）时 warp 到 `at`，再重新关联、显示——光标直接出现在 `at`，
/// 不会先在冻结的位置露出来。warp 前把本地事件抑制间隔设为约 0：否则 warp 之后约 250 ms 光标
/// 不跟手（实测 F4；Deskflow 同样做法）。
///
/// `via`：先 warp 到这里（进入点就是光标冻结的位置时，原地 warp 会被系统忽略，见 `crate::warp_delta`）。
pub(super) fn restore_at(
    at: Point,
    via: Option<Point>,
    fix: &std::sync::Mutex<crate::warp_delta::WarpDeltaFix>,
) -> u64 {
    tracing::debug!(target: "oink_cursor_timing", ?at, ?via, "cursor latency: warp begin");
    let (e0, e1, e2) = warp(at, via);
    tracing::debug!(target: "oink_cursor_timing", ?at, "cursor latency: warp end");
    tracing::debug!(target: "oink_cursor_timing", "cursor latency: retire glyph begin");
    handoff_timing::native(TimingStage::GlyphRetireStarted, 0);
    let retired = owner::retire_blank();
    // Distinguish no owner/no blank from a changed destination cursor. A
    // skipped retirement is not evidence that our transparent glyph survived.
    handoff_timing::native(TimingStage::GlyphRetireCompleted, retired as u32);
    tracing::debug!(target: "oink_cursor_timing", "cursor latency: retire glyph end");
    let was = DISSOCIATED.load(Ordering::Acquire);
    restore();
    // Keep the only CG warp primitive and its accounting inseparable. As with
    // native absolute injection (50d073df), the next hardware delta can contain
    // this displacement. hold_motion remains set until this registration ends.
    let done_ns = super::tap::now_event_ns();
    fix.lock().unwrap_or_else(|e| e.into_inner()).warped(at, done_ns);
    tracing::debug!(?e0, ?e1, ?e2, dissociated_before = was, "restore_at: CG results");
    done_ns
}

fn warp(
    at: Point,
    via: Option<Point>,
) -> (
    objc2_core_graphics::CGError,
    Option<objc2_core_graphics::CGError>,
    objc2_core_graphics::CGError,
) {
    handoff_timing::native(TimingStage::WarpStarted, u32::from(via.is_some()));
    #[allow(deprecated)]
    let e0 = CGSetLocalEventsSuppressionInterval(0.0001);
    let e1 = via.map(|v| CGWarpMouseCursorPosition(CGPoint { x: v.x, y: v.y }));
    let e2 = CGWarpMouseCursorPosition(CGPoint { x: at.x, y: at.y });
    handoff_timing::native(TimingStage::WarpCompleted, e2.0 as u32);
    crate::native_lifecycle::emit_lifecycle(
        crate::native_lifecycle::Kind::Park,
        crate::native_lifecycle::Data {
            point: Some([at.x as i32, at.y as i32]),
            success: Some(e2.0 == 0),
            ..Default::default()
        },
    );
    (e0, e1, e2)
}

/// 恢复光标：重新关联、显示。可重复调用。
pub(crate) fn restore() {
    tracing::debug!(target: "oink_cursor_timing", "cursor latency: owner release begin");
    owner::release();
    tracing::debug!(target: "oink_cursor_timing", "cursor latency: owner release end");
    handoff_timing::native(TimingStage::ShowStarted, 0);
    let (associate, show, flush) =
        restore_native_guarded(SOURCE_DISABLED.load(Ordering::Acquire) || CONFINED.load(Ordering::Acquire));
    handoff_timing::native(
        TimingStage::ShowCompleted,
        (associate.unwrap_or(0) | show.unwrap_or(0) | flush.unwrap_or(0)) as u32 | (u32::from(show.is_none()) << 31),
    );
    // Keep the existing post-Show visibility query in native typed evidence too:
    // the realtime owner suppresses tracing, and API success is not visibility.
    // SAFETY: Read-only query; visibility does not establish a nontransparent glyph.
    let visible = unsafe { CGCursorIsVisible() } != 0;
    if associate.is_some() || show.is_some() {
        crate::native_lifecycle::emit_lifecycle(
            crate::native_lifecycle::Kind::Show,
            crate::native_lifecycle::Data {
                hidden: Some(false),
                cursor_showing: Some(visible),
                success: Some(
                    associate.is_none_or(|v| v == 0) && show.is_none_or(|v| v == 0) && flush.is_none_or(|v| v == 0),
                ),
                ..Default::default()
            },
        );
    }
    tracing::debug!(
        associate_called = associate.is_some(),
        associate_code = associate.unwrap_or(0),
        show_called = show.is_some(),
        show_code = show.unwrap_or(0),
        flush_called = flush.is_some(),
        flush_code = flush.unwrap_or(0),
        "restore cursor: native results"
    );
    tracing::debug!(target: "oink_cursor_timing", visible, "cursor latency: show end");
}

fn restore_native() -> (Option<i32>, Option<i32>, Option<i32>) {
    CONFINED.store(false, Ordering::Release);
    SOURCE_DISABLED.store(false, Ordering::Release);
    restore_native_guarded(false)
}
fn restore_native_guarded(disabled: bool) -> (Option<i32>, Option<i32>, Option<i32>) {
    let associate = (DISSOCIATED.swap(disabled, Ordering::AcqRel) != disabled)
        .then(|| CGAssociateMouseAndMouseCursorPosition(!disabled).0);
    HIDDEN.store(false, Ordering::Release);
    let (show, flush) = if HIDE_REQUESTED.swap(false, Ordering::AcqRel) {
        let show = CGDisplayShowCursor(CGMainDisplayID()).0;
        // As with hide(), flush the visibility change at the already-warped entry point.
        // A successful ShowCursor alone can leave the cursor hidden until the next move.
        let flush = CGAssociateMouseAndMouseCursorPosition(!disabled).0;
        (Some(show), Some(flush))
    } else {
        (None, None)
    };
    // No logging/allocation here: signal and exit hooks also call this function.
    (associate, show, flush)
}

// ---- 退出时恢复 ----

const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
static mut OLD_ACTIONS: [Option<libc::sigaction>; 3] = [None, None, None];

extern "C" fn restore_at_exit() {
    owner::release_emergency();
    restore_native();
}

extern "C" fn on_signal(sig: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    owner::release_emergency();
    restore_native();
    let idx = SIGNALS.iter().position(|s| *s == sig);
    // SAFETY: Each slot is initialized before its handler becomes visible to the OS,
    // then never changed. Read only this slot, not other slots still being installed.
    let old = idx.and_then(|i| unsafe {
        std::ptr::addr_of!(OLD_ACTIONS)
            .cast::<Option<libc::sigaction>>()
            .add(i)
            .read()
    });
    match old {
        Some(a) if a.sa_sigaction != libc::SIG_DFL && a.sa_sigaction != libc::SIG_IGN => {
            // 把信号交给原来的处理函数（如 tokio 的优雅退出）。
            // SAFETY: 原处理函数由 sigaction 登记，签名为 fn(c_int)（未设置 SA_SIGINFO 时）。
            unsafe {
                if a.sa_flags & libc::SA_SIGINFO != 0 {
                    let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                        std::mem::transmute(a.sa_sigaction);
                    // Preserve the kernel's signal metadata. Tokio's signal-hook-registry
                    // aborts on a null siginfo, so a one-argument wrapper that fabricates
                    // null pointers cannot chain its graceful-exit handler. This changes
                    // only forwarding; cursor restoration and default/ignored signals keep
                    // their behavior. Verified by an idle child process on macOS 26.6.2.
                    f(sig, info, context);
                } else {
                    let f: extern "C" fn(libc::c_int) = std::mem::transmute(a.sa_sigaction);
                    f(sig);
                }
            }
        }
        Some(a) if a.sa_sigaction == libc::SIG_IGN => {}
        _ => {
            // 默认行为：恢复默认处理并重新发出信号，让进程按原样结束。
            // SAFETY: 标准的信号处理重置与重发。
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                libc::raise(sig);
            }
        }
    }
}

fn install_exit_hooks() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: atexit 登记一个无参数的 C 函数；sigaction 登记处理函数并保存原处理函数。
        unsafe {
            libc::atexit(restore_at_exit);
            for (i, sig) in SIGNALS.iter().enumerate() {
                let mut new: libc::sigaction = std::mem::zeroed();
                new.sa_sigaction =
                    on_signal as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
                new.sa_flags = libc::SA_RESTART | libc::SA_SIGINFO;
                libc::sigemptyset(&mut new.sa_mask);
                let mut old: libc::sigaction = std::mem::zeroed();
                // Publish the previous action BEFORE exposing our wrapper. Capturing
                // it in the installing sigaction call leaves a window where SIGTERM
                // sees an empty slot and re-raises the default action instead of exiting
                // cleanly. AgentHost registers its handler before input initialization.
                if libc::sigaction(*sig, std::ptr::null(), &mut old) == 0 {
                    std::ptr::addr_of_mut!(OLD_ACTIONS)
                        .cast::<Option<libc::sigaction>>()
                        .add(i)
                        .write(Some(old));
                    libc::sigaction(*sig, &new, std::ptr::null_mut());
                }
            }
        }
    });
}

#[cfg(all(test, debug_assertions))]
mod reconcile_tests {
    use super::*;

    static SIGNAL_METADATA_OK: AtomicBool = AtomicBool::new(false);

    extern "C" fn previous_signal(sig: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
        // SAFETY: The OS supplies this pointer while delivering the signal; the null
        // check also makes the test safely detect the former broken forwarding wrapper.
        let valid = !info.is_null() && !context.is_null() && unsafe { (*info).si_signo == sig };
        SIGNAL_METADATA_OK.store(valid, Ordering::SeqCst);
    }

    #[test]
    fn idle_local_reconciliation_never_queries_native_visibility() {
        let mut reconcile = Reconcile {
            next: Some(Instant::now()),
            failures: 3,
        };
        for _ in 0..240 {
            assert_eq!(
                reconcile.visibility(false, false, || panic!("idle native visibility query")),
                None
            );
        }
        assert_eq!(reconcile.failures, 0);
        assert_eq!(reconcile.next, None);
        assert_eq!(reconcile.visibility(true, false, || true), Some(true));
        assert_eq!(reconcile.visibility(false, true, || false), Some(false));
    }

    #[test]
    fn exit_hook_metadata_child() {
        if std::env::var_os("OINK_CURSOR_SIGNAL_TEST").is_none() {
            return;
        }
        // This child never creates an input backend or owner window, hides, dissociates
        // or injects. All restoration flags stay false, so the hook makes no cursor calls.
        assert!(!HIDDEN.load(Ordering::SeqCst));
        assert!(!HIDE_REQUESTED.load(Ordering::SeqCst));
        assert!(!DISSOCIATED.load(Ordering::SeqCst));
        // SAFETY: Register a C handler in this isolated test child, then deliver a real
        // signal to this process only. The alarm bounds a broken signal-chain hang.
        unsafe {
            libc::alarm(10);
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction =
                previous_signal as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
            action.sa_flags = libc::SA_SIGINFO;
            libc::sigemptyset(&mut action.sa_mask);
            assert_eq!(libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()), 0);
        }
        install_exit_hooks();
        // SAFETY: Only this child receives SIGTERM; the registered handler returns.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        assert!(SIGNAL_METADATA_OK.load(Ordering::SeqCst));
    }

    #[test]
    fn exit_hook_preserves_kernel_signal_metadata() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "macos::cursor::reconcile_tests::exit_hook_metadata_child",
                "--exact",
                "--nocapture",
            ])
            .env("OINK_CURSOR_SIGNAL_TEST", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn failed_hide_is_retried_and_session_or_display_transition_resets_backoff() {
        let mut retry = Reconcile::default();
        let now = Instant::now();
        assert!(retry.due(true, true, false, now));
        retry.attempted(now, true);
        assert!(!retry.due(true, true, false, now + Duration::from_millis(250)));
        assert!(retry.due(true, true, false, now + Duration::from_secs(1)));
        retry.attempted(now + Duration::from_secs(1), true);
        assert!(retry.due(true, true, true, now + Duration::from_millis(1250)));
        retry.attempted(now, false);
        assert!(!retry.due(true, false, false, now));
        assert!(!retry.due(false, true, true, now));
        assert!(retry.due(true, true, false, now));
    }

    #[test]
    fn native_requests_are_balanced_even_when_hide_or_show_fails() {
        assert_eq!(
            replace_request(false, || panic!("no request to show"), || true),
            (true, true)
        );
        assert_eq!(
            replace_request(false, || panic!("no request to show"), || false),
            (false, false)
        );
        assert_eq!(
            replace_request(true, || false, || panic!("must not stack hide")),
            (true, false)
        );
        assert_eq!(replace_request(true, || true, || false), (false, false));
        assert_eq!(replace_request(true, || true, || true), (true, true));
    }

    #[test]
    fn successful_native_hide_submits_one_update_after_return() {
        let calls = std::cell::RefCell::new(Vec::new());
        let result = hide_with_update(
            || {
                calls.borrow_mut().push("hide");
                CGError(0)
            },
            || calls.borrow_mut().push("update"),
        );
        assert_eq!(result.0, 0);
        assert_eq!(*calls.borrow(), ["hide", "update"]);
        assert_eq!(
            hide_with_update(|| CGError(1000), || panic!("hide was rejected")).0,
            1000
        );
    }

    #[test]
    fn renewal_balances_show_then_hide_then_update_without_stacking_on_failure() {
        let calls = std::cell::RefCell::new(Vec::new());
        let result = replace_request(
            true,
            || {
                calls.borrow_mut().push("show");
                true
            },
            || {
                hide_with_update(
                    || {
                        calls.borrow_mut().push("hide");
                        CGError(0)
                    },
                    || {
                        // A missing private API is logged by request_hidden_frame; it must
                        // not undo the successful native request's accounting.
                        calls.borrow_mut().push("update unavailable");
                    },
                )
                .0 == 0
            },
        );
        assert_eq!(result, (true, true));
        assert_eq!(*calls.borrow(), ["show", "hide", "update unavailable"]);
        assert_eq!(
            replace_request(
                true,
                || false,
                || { hide_with_update(|| panic!("must not stack hide"), || panic!("no new hide")).0 == 0 }
            ),
            (true, false)
        );
    }
}
