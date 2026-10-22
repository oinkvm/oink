//! 采集与注入：截获本机键盘、鼠标、触摸板事件，把其他电脑发来的事件注入本机。
//!
//! - 对外只暴露 [`InputBackend`]；平台实现在 `src/macos`、`src/windows`，只做「翻译」，不做路由决策。
//! - Shared input event definitions live in `oink_foundation::input`.

use std::sync::Arc;

use oink_foundation::Point;
pub use oink_foundation::input::{
    CaptureMode, CapturedEvent, Disposition, FeelProfile, Gesture, GesturePhase, InjectEvent, InputEvent,
    InputPermissions, KeyCode, MouseButton, PermissionState, ScrollPhase, ScrollUnit, SourceKind, SwipeAxis,
};

pub mod capture_continuity;
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
mod contact_capture;
mod contact_status;
pub mod contact_trace;
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
mod deferred_activity;
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
mod mt_contacts;
pub mod mt_trace;
pub use contact_status::{ContactInitError, ContactInjectionStatus};
pub mod button_diagnostics;
pub mod capture_loop;
#[cfg(any(windows, all(test, debug_assertions)))]
mod contacts;
#[cfg(any(windows, all(test, debug_assertions)))]
mod cursor_clip;
pub mod cursor_reconciliation;
pub mod diagnostics;
pub mod display_power;
pub mod gesture_trace;
pub mod jitter;
#[cfg(target_os = "macos")]
mod macos;
pub mod native_lifecycle;
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
mod ordinary_motion;
#[cfg(any(windows, all(test, debug_assertions)))]
mod park_commit;
pub mod pointer_capture;
pub mod pointer_transition;
pub mod session_gate;
pub mod source_time;
pub mod stale_step;
pub mod timing;
pub mod warp_base;
pub mod warp_delta;
pub mod win_actions;
#[cfg(any(windows, all(test, debug_assertions)))]
mod win_lock;
#[cfg(windows)]
mod windows;
#[cfg(any(windows, all(test, debug_assertions)))]
mod windows_external_input;

#[cfg(windows)]
pub use windows::raw_capture::dispatch_guard as dispatch_raw_clip_guard;
/// Windows input service diagnostics (always require the authenticated service).
#[cfg(windows)]
pub use windows::service as windows_service;

/// Pure native key tables are available on both OSes so an editor on either
/// device can validate a different source/target without guessing its key set.
#[path = "macos/keymap.rs"]
pub mod mac_keymap;
#[path = "windows/keymap.rs"]
mod win_keymap;

pub fn native_key_supported(os: &str, code: KeyCode) -> bool {
    match os {
        "macos" => mac_keymap::hid_to_mac(code).is_some() || mac_keymap::hid_to_nx(code).is_some(),
        "windows" => win_keymap::lookup_hid(code).is_some(),
        _ => false,
    }
}
pub fn supported_key_codes(os: &str) -> Vec<KeyCode> {
    oink_foundation::input_transform::candidate_keys()
        .into_iter()
        .filter(|&k| native_key_supported(os, k))
        .collect()
}

/// macOS 的 Caps Lock 状态与输入法（诊断、实机测试程序使用）。
#[cfg(target_os = "macos")]
pub use macos::capslock::diag as mac_caps;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("缺少系统权限：{0}")]
    PermissionDenied(String),
    #[error("本平台不支持")]
    Unsupported,
    #[error("未启动截获")]
    NotStarted,
    #[error("系统调用失败：{0}")]
    Platform(String),
}

/// 截获回调。在平台的截获线程里**同步**调用，必须尽快返回（目标 < 0.5 ms，不做 I/O、不加可能阻塞的锁），
/// 返回值决定这个事件是否被吞掉。
pub type EventHandler = Arc<dyn Fn(&CapturedEvent) -> Disposition + Send + Sync>;

/// 一台电脑的输入能力。
///
/// 平台实现的约定：
/// - 必须过滤掉自己注入的事件，不交给回调（用事件上的来源标记识别，如 macOS
///   `kCGEventSourceUserData`、Windows `dwExtraInfo`）。
/// - Windows keeps other software's flagged injected input local by default;
///   the local forward_injected_input preference opts into legacy source routing.
///   macOS retains its existing source-input policy.
/// - `Remote` 模式下本机光标隐藏且不动，位移以 `PointerDelta` 上报（已含系统加速），
///   事件的 `cursor` 为 `None`（光标停在停靠点或原地冻结，那不是用户指针的位置）。
///   macOS 不停到 `park`：远程模式期间的 warp 会被系统算进下一个移动事件（`warp_delta`）。macOS 交出焦点时
///   立即隐藏、解耦并挪出感应区冻结。
/// - 切到 `Remote` 时先隐藏再停靠 / 冻结：光标不能在别处露出来。
/// - 所有方法可以从任意线程调用。
pub trait InputBackend: Send + Sync {
    /// Atomic only: no native permission query on capture/injection threads.
    fn input_safety(&self) -> session_gate::InputSafety {
        Default::default()
    }
    fn watch_input_safety(&self, _wake: &capture_loop::Wake) {}
    /// Advances the pause/resume generation; native owners retire old work before
    /// tearing down, and resume creates a fresh input session.
    fn set_input_paused(&self, _paused: bool) {}
    /// Runs on a fresh injection worker, before accepting session work.
    fn start_injection_session(&self) -> Result<(), InputError> {
        Ok(())
    }
    /// Runs after finite held-input cleanup, before that worker exits.
    fn stop_injection_session(&self) {}
    /// Cached source preference only: safe on the capture owner, no TIS/OS calls.
    /// None means the preference has not been observed or is not applicable.
    fn caps_lock_switches_input_source(&self) -> Option<bool> {
        None
    }
    /// Atomic snapshot of the live gesture-only injector; macOS has no injector.
    fn touchpad_contacts(&self) -> Option<u8> {
        None
    }
    /// Read-only runtime probe result, independent of the group contacts switch.
    fn contact_injection_status(&self) -> ContactInjectionStatus {
        ContactInjectionStatus::from_capacity(self.touchpad_contacts())
    }
    /// Last priority read back in a protected input helper reply. Snapshot only.
    fn helper_priority(&self) -> Option<i32> {
        None
    }
    fn display_power_observation(&self) -> Result<Box<dyn display_power::Observation>, InputError> {
        Ok(Box::new(display_power::Monitor::new()?))
    }
    /// Keys eligible for local cleanup when enabling the physical-key guard.
    /// Windows returns only observed physical holds, never the mixed OS bitmap.
    fn held_session_keys(&self) -> Vec<KeyCode> {
        Vec::new()
    }
    /// Read-only session lock observation. Unknown is not treated as unlocked.
    fn session_locked(&self) -> Option<bool> {
        None
    }

    /// Only synthetic backends may retain key details in diagnostics.
    fn diagnostic_key_details(&self) -> bool {
        false
    }
    /// 开始截获，之后每个事件调用一次 `handler`。
    fn start(&self, handler: EventHandler) -> Result<(), InputError>;
    /// Start capture and transfer the routing owner onto its native event loop.
    fn start_loop(&self, _owner: Box<dyn capture_loop::CaptureLoop>) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    /// 停止截获并恢复本机光标（显示、解除停靠）。
    fn stop(&self);
    /// Platform-owned capture handle presence; stop joins the OS thread before returning.
    fn capture_active(&self) -> Option<bool> {
        None
    }
    /// Cursor visibility/coupling only; it need not follow group focus.
    fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError>;
    /// Ordered, visible local-pointer confinement. Never moves the pointer on acquisition.
    fn set_cursor_constraint(&self, _bounds: Option<oink_foundation::Rect>) -> Result<(), InputError> {
        Ok(())
    }
    /// Native keyboard side effects follow the keyboard source route independently
    /// of the mouse cursor mode (notably macOS Caps Lock and input-source guarding).
    fn set_keyboard_remote(&self, _remote: bool) -> Result<(), InputError> {
        Ok(())
    }
    /// Keep the original native drag on its catcher while input is remote. The
    /// catcher is also the cursor-hiding surface; an unrelated cover must never
    /// displace it. Clear only after native return/end for the same generation.
    fn set_drag_anchor(&self, _anchor: Option<oink_foundation::dnd::DragAnchor>) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    /// Send one tagged left-dragged event to the held Mac source before cancel.
    /// Preserve button ownership, frozen cursor state and the motion ledger.
    fn move_held_drag(&self, _generation: u64, _at: Point) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    /// Same move, with an optional exact submission observation. Implementations
    /// without a submission hook leave the point unknown; never infer it from
    /// a requested position or a successful delivery receipt.
    fn move_held_drag_observed(&self, generation: u64, at: Point, _posted: &dyn Fn(Point)) -> Result<(), InputError> {
        self.move_held_drag(generation, at)
    }
    /// Refresh held Mac-source artwork with one marked same-position dragged
    /// event. No movement delta or injection-position ledger change.
    fn nudge_held_drag(
        &self,
        _generation: u64,
        _button: MouseButton,
        _posted: &dyn Fn(oink_foundation::dnd::ImageNudgePost),
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    /// Permit raw mouse reports while parked on a remote game (Windows needs a
    /// guarded ClipCursor instead of swallowing MOVE). Called outside input callbacks.
    fn set_relative_capture(&self, _enabled: bool) -> Result<(), InputError> {
        Ok(())
    }
    /// 焦点回到本机：切回 `Local`，光标出现在 `at`（本机全局坐标，通常是进入边缘上的点）。
    ///
    /// 平台实现应在截获线程上**原子地**完成「光标仍隐藏时放到 `at` → 切到 `Local` → 显示」，保证：
    /// 光标不会先在停靠点露出来；这期间的物理移动不会把光标拉回停靠点；之后的第一个移动从 `at` 出发。
    /// 与 `set_mode` 一样可以在截获回调里调用。
    ///
    /// 默认实现（不原子）：注入 `MoveTo`、切到 `Local`、再注入一次 `MoveTo`。
    fn restore_local(&self, at: Point) -> Result<(), InputError> {
        self.inject(&InjectEvent::MoveTo { pos: at })?;
        self.set_mode(CaptureMode::Local)?;
        self.inject(&InjectEvent::MoveTo { pos: at })
    }
    /// 注入一个事件到本机。
    ///
    /// Injected events wake displays and reset idle time. Windows SendInput already
    /// counts as activity; macOS separately declares activity, at most once per second.
    fn inject(&self, event: &InjectEvent) -> Result<(), InputError>;
    /// One-shot display wake / idle timer reset; does not change routing focus.
    /// Returns true when declared, false when coalesced with a recent declaration.
    fn declare_user_activity(&self) -> Result<bool, InputError> {
        Err(InputError::Unsupported)
    }
    /// Refresh OS input-idle accounting for an actual group/local activity pulse.
    /// Caller gates this with sharing and `wake_displays_on_input`. No persistent
    /// sleep assertion or periodic renewal: stopping input stops these calls.
    /// Returns false when throttled or when a session shield prevents delivery.
    fn reset_user_idle(&self) -> Result<bool, InputError> {
        Err(InputError::Unsupported)
    }
    /// Press the hosted-drag source at its validated hit-test position.
    /// macOS retains the explicit CGEvent position; Windows combines absolute
    /// position and Down in one INPUT so physical movement cannot displace it.
    fn press_drag_at(&self, _at: Point) -> Result<(), InputError> {
        self.inject(&InjectEvent::Button {
            button: oink_foundation::input::MouseButton::Left,
            pressed: true,
        })
    }

    /// Release an original source press at its recorded start, atomically with
    /// positioning. Unsupported backends must not fall back to an unpositioned UP.
    fn release_drag_at(&self, _at: Point) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }

    /// 结束物理左键发起的拖拽，macOS 保留 HID 按住状态，其他平台保持原注入行为。
    fn release_physical_drag(&self) -> Result<(), InputError> {
        self.inject(&InjectEvent::Button {
            button: oink_foundation::input::MouseButton::Left,
            pressed: false,
        })
    }
    /// 当前光标位置（本机全局坐标）。
    fn cursor_position(&self) -> Result<Point, InputError>;
    /// Expected native endpoint of the just-successful absolute injection.
    /// Read-only ledger/quantization, not cursor readback or proof of OS commit.
    /// Relative motion has no absolute endpoint. Tolerance is native units.
    fn cursor_injection_target(&self, event: &InjectEvent) -> Option<(Point, f64)> {
        match event {
            InjectEvent::MoveTo { pos } | InjectEvent::MoveToWithDelta { pos, .. } => Some((*pos, 0.)),
            _ => None,
        }
    }
    /// After a hosted-drag press discarded motion, abandon fractional/cached
    /// displacement and sample the real cursor before the next movement.
    fn rebase_pointer_motion(&self) -> Result<Point, InputError> {
        self.cursor_position()
    }
    /// Current application cursor confinement in physical desktop pixels.
    /// Query on the ordinary input event loop, never inside a native input callback.
    /// None means unconstrained/unsupported; errors must not masquerade as release.
    /// Windows excludes its own raw-capture clip by explicit lease ownership.
    fn system_cursor_clip(&self) -> Result<Option<oink_foundation::Rect>, InputError> {
        Ok(None)
    }
    /// Read-only OS button state for diagnostics. May include injected state (macOS HID).
    /// None means the backend does not expose this observation; never repairs input state.
    fn system_button_state(&self, _button: oink_foundation::input::MouseButton) -> Option<bool> {
        None
    }
    /// Read-only pointer capture evidence. Never asks for permissions or changes cursor state.
    fn pointer_capture_sample(&self) -> Option<pointer_capture::CaptureSample> {
        None
    }
    /// Bundle identifier for this still-foreground process, for transition diagnostics only.
    /// Called on the ordinary observer; never returns a window title or executable path.
    fn foreground_bundle_id(&self, _foreground: u64) -> Option<String> {
        None
    }
    /// Reconcile an ambiguous platform snapshot before automatic detection.
    /// Call outside the input callback/router lock, after the read-only sample.
    /// `eligible` means local focus, Auto and not paused; `captured` is the current
    /// automatic capture. macOS may refresh hide-until-move state after accepted
    /// remote motion, or while maintaining a capture. Known causes stay read-only.
    /// Returns None while ambiguous entry evidence awaits a rate-limited refresh;
    /// use the original read-only snapshot for display-only visibility consumers.
    fn refresh_pointer_capture_sample(
        &self,
        sample: pointer_capture::CaptureSample,
        _eligible: bool,
        _captured: bool,
    ) -> Option<pointer_capture::CaptureSample> {
        Some(sample)
    }
    /// System edge reservations for these displays. Called outside the capture callback;
    /// implementations may read preferences. Unsupported backends need no reservations.
    fn edge_resistance_regions(
        &self,
        _displays: &[oink_foundation::display::DisplayInfo],
    ) -> Vec<oink_foundation::input::EdgeResistanceRegion> {
        Vec::new()
    }
    /// 本机的手感设置（按键重复、滚动等）。
    fn feel_profile(&self) -> FeelProfile;
    fn permissions(&self) -> InputPermissions;
}

/// 在工作线程上运行 `f`，调用线程（应当是主线程）同时运行系统的主 run loop，直到 `f` 返回。
///
/// macOS：「当前输入法」（Text Input Sources）在进程内有缓存，只有**主线程的 run loop 在跑**时才会随其他
/// 程序（菜单栏、快捷键）的切换而更新；主线程被别的东西占住（例如阻塞在 tokio 的 `block_on`）时，
/// Caps Lock 本机行为设置和源端守卫读到的输入法会一直是旧的（2026-09-28 实测）。有界面的程序（Tauri）主线程
/// 本来就在跑 run loop；没有界面的命令行服务进程用这个函数运行。其他系统：直接运行 `f`。
pub fn run_with_main_run_loop<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    #[cfg(target_os = "macos")]
    return macos::run_with_main_run_loop(f);
    #[allow(unreachable_code)]
    f()
}

/// 创建本机平台的实现。
pub fn platform_backend() -> Result<Box<dyn InputBackend>, InputError> {
    #[cfg(target_os = "macos")]
    return macos::backend();
    #[cfg(windows)]
    return windows::backend();
    #[allow(unreachable_code)]
    Err(InputError::Unsupported)
}

#[cfg(any(windows, all(test, debug_assertions)))]
mod touchpad;
