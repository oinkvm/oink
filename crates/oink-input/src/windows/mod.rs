//! Windows 实现：WH_KEYBOARD_LL / WH_MOUSE_LL 截获、Raw Input 区分设备、SendInput 注入。
//!
//! - 截获：专用线程 + 消息循环（[`hook`]）。自身事件用 `dwExtraInfo` 的 [`inject::INJECT_TAG`]
//!   或光标形状刷新专用 [`inject::SHAPE_REFRESH_TAG`] 识别并跳过；
//!   其他程序注入的事件（`LLMHF_INJECTED` / `LLKHF_INJECTED`）照常上报，来源为 `Unknown`。
//! - `Remote` 模式：鼠标移动一律吞掉，位移 = 钩子里的 `pt` − 移动前的光标位置（即停靠点，
//!   系统已按指针速度与「提高指针精确度」处理），事件不带光标位置；光标隐藏方案见 [`cursor`]。
//!   进入 `Remote` 先隐藏再停靠，`restore_local` 在钩子线程上原子地「放光标、切模式、显示」（见 [`hook`]）。
//! - 注入：见 [`inject`]。键码：HID ↔ Set 1 扫描码，见 [`keymap`]。
//! - 手感设置：见 [`settings`]。
//! - 坐标：物理像素（进程 PMv2；做不到时在本模块的线程与调用里临时切换线程 DPI 上下文）。
//! - 权限：产品后台由 OinkInput 复制交互用户 token 并设置 UIAccess；无需签名。
//!   普通 token 的低级 hook 无法稳定采集高完整性窗口。UIAccess 后台只在 Default 采集，
//!   安全桌面立即收回本机来源并释放已转发按压；现有 SYSTEM helper 的注入职责不变。

mod application_focus;
mod cursor;
mod dpi;
mod hook;
mod inject;
use crate::win_keymap as keymap;
mod cursor_clip;
mod loop_timer;
mod mouse_keys;
mod pointer_capture;
pub mod raw_capture;
mod rawinput;
mod resume_hotkey;
pub mod service;
mod settings;
#[cfg(all(test, debug_assertions))]
mod source_release_desktop_tests;
mod task_view;
mod touchpad;
pub(crate) mod trace;
mod warp;
mod wheel_origin;

use std::collections::HashSet;
use std::sync::Mutex;

use oink_foundation::Point;
use oink_foundation::input::{
    CaptureMode, FeelProfile, InjectEvent, InputPermissions, KeyCode, PermissionState, ScrollUnit,
};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint};
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::INPUT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::win_actions::{self, GestureMapper, WinAction};
use crate::{EventHandler, InputBackend, InputError};

pub(crate) fn backend() -> Result<Box<dyn InputBackend>, InputError> {
    if let Err(msg) = dpi::ensure_process_per_monitor_v2() {
        tracing::warn!("{msg}");
    }
    cursor::recover_if_needed();
    Ok(Box::new(WinInput::default()))
}

#[derive(Debug, Default)]
struct WinInput {
    application_focus: application_focus::Controller,
    safety: crate::session_gate::SessionGate,
    hook: Mutex<Option<hook::HookHandle>>,
    /// 滚轮小数部分的累积（水平、垂直），单位 1/120 格。
    scroll_acc: Mutex<(f64, f64)>,
    relative_acc: Mutex<(f64, f64)>,
    /// Mac 手势 → 快捷键（`win_actions`）。
    gestures: Mutex<GestureMapper>,
    /// 映射为快捷键、仍按着的 Mac 系统功能键：重复的按下不再触发。
    shortcut_keys: Mutex<HashSet<KeyCode>>,
    lock_chord: Mutex<crate::win_lock::LockChord>,
    confinement: Mutex<raw_capture::Confinement>,
}

/// 执行 Mac 手势 / 系统功能键映射出的动作（`win_actions`）。
fn run_actions(actions: &[WinAction]) -> Result<(), InputError> {
    for a in actions {
        let mut inputs = Vec::new();
        let keys: &[KeyCode] = match a {
            WinAction::Shortcut(keys) => keys,
            WinAction::CtrlWheel(_) => &[KeyCode::LEFT_CTRL],
        };
        for k in keys {
            inputs.push(inject::key(*k, true).ok_or(InputError::Unsupported)?);
        }
        if let WinAction::CtrlWheel(n) = a {
            inputs.push(inject::wheel(n.saturating_mul(inject::WHEEL_DELTA as i32), false));
        }
        for k in keys.iter().rev() {
            inputs.push(inject::key(*k, false).ok_or(InputError::Unsupported)?);
        }
        tracing::debug!("mac gesture / system key → shortcut");
        send(&inputs)?;
    }
    Ok(())
}

impl WinInput {
    fn take_hook(&self) -> Option<hook::HookHandle> {
        self.hook.lock().ok().and_then(|mut g| g.take())
    }
}

impl Drop for WinInput {
    fn drop(&mut self) {
        let _ = self.set_cursor_constraint(None);
        oink_winsvc::client::release_if_connected();
        if let Some(h) = self.take_hook() {
            h.stop();
        }
    }
}

fn send(inputs: &[INPUT]) -> Result<(), InputError> {
    if service::send(inputs)? {
        return Ok(());
    }
    let n = warp::send(inputs);
    if n as usize == inputs.len() {
        Ok(())
    } else {
        Err(InputError::Platform(format!(
            "SendInput 只注入了 {n}/{}：{}",
            inputs.len(),
            windows::core::Error::from_thread()
        )))
    }
}

fn cursor_dpi() -> u32 {
    // SAFETY: 查询光标所在显示器的有效 DPI。
    unsafe {
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        let m = MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST);
        let (mut x, mut y) = (96u32, 96u32);
        if GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_err() {
            return 96;
        }
        x
    }
}

impl InputBackend for WinInput {
    fn application_focus_supported(&self) -> bool {
        true
    }
    fn application_focus_watch(&self, observer: Option<crate::application_focus::Observer>) {
        self.application_focus.watch(observer);
    }
    fn application_focus_sample(&self) -> oink_foundation::application_focus::Observation {
        self.application_focus.sample()
    }
    fn application_focus_park(
        &self,
        expected: oink_foundation::application_focus::ObservationStamp,
        operation: u64,
        permit: crate::application_focus::Permit,
    ) -> oink_foundation::application_focus::ParkResult {
        let result = self.application_focus.park(expected, operation, permit);
        tracing::info!(target: "oink_application_focus", operation, generation = expected.generation,
            sequence = expected.sequence, ?result, "native park completed");
        result
    }
    fn application_focus_dismiss(&self) {
        self.application_focus.dismiss();
    }
    fn start_injection_session(&self) -> Result<(), InputError> {
        service::initialize()
    }
    fn stop_injection_session(&self) {
        let _ = self.set_cursor_constraint(None);
        oink_winsvc::client::suspend();
        *self.scroll_acc.lock().unwrap_or_else(|e| e.into_inner()) = (0., 0.);
        *self.relative_acc.lock().unwrap_or_else(|e| e.into_inner()) = (0., 0.);
        self.shortcut_keys.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *self.gestures.lock().unwrap_or_else(|e| e.into_inner()) = Default::default();
        *self.lock_chord.lock().unwrap_or_else(|e| e.into_inner()) = Default::default();
    }
    fn input_safety(&self) -> crate::session_gate::InputSafety {
        self.safety.snapshot()
    }
    fn watch_input_safety(&self, wake: &crate::capture_loop::Wake) {
        self.safety.watch(wake);
    }
    fn set_input_paused(&self, paused: bool) {
        self.safety.set_paused(paused);
    }
    fn touchpad_contacts(&self) -> Option<u8> {
        touchpad::contact_capacity()
    }
    fn contact_injection_status(&self) -> crate::ContactInjectionStatus {
        touchpad::contact_status()
    }
    fn helper_priority(&self) -> Option<i32> {
        oink_winsvc::client::helper_priority()
    }

    fn held_session_keys(&self) -> Vec<KeyCode> {
        if session_locked() == Some(true) {
            return Vec::new();
        }
        hook::physical_keys()
    }
    fn session_locked(&self) -> Option<bool> {
        session_locked()
    }

    fn declare_user_activity(&self) -> Result<bool, InputError> {
        use windows::Win32::System::Power::{ES_DISPLAY_REQUIRED, SetThreadExecutionState};
        // SAFETY: one-shot idle timer reset, no persistent execution state assertion.
        if unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED) }.0 == 0 {
            return Err(InputError::Platform(
                "SetThreadExecutionState(ES_DISPLAY_REQUIRED) failed".into(),
            ));
        }
        Ok(true)
    }

    fn reset_user_idle(&self) -> Result<bool, InputError> {
        use windows::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_MOVE;
        self.declare_user_activity()?;
        // Use the same protected-install OinkInput service path as routed input.
        // Tagged zero displacement; idle/lock behavior awaits Windows hardware verification.
        send(&[inject::mouse(0, 0, 0, MOUSEEVENTF_MOVE)]).map(|()| true)
    }

    fn start(&self, handler: EventHandler) -> Result<(), InputError> {
        // Create the protected pipe owner before installing the realtime hook.
        service::initialize()?;
        if let Some(old) = self.take_hook() {
            old.stop();
        }
        let h = hook::HookHandle::spawn(handler)?;
        *self.hook.lock().map_err(|_| InputError::Platform("锁已损坏".into()))? = Some(h);
        Ok(())
    }

    fn start_loop(&self, owner: Box<dyn crate::capture_loop::CaptureLoop>) -> Result<(), InputError> {
        service::initialize()?;
        if let Some(old) = self.take_hook() {
            old.stop();
        }
        let h = hook::HookHandle::spawn_loop(owner)?;
        *self
            .hook
            .lock()
            .map_err(|_| InputError::Platform("capture lock poisoned".into()))? = Some(h);
        Ok(())
    }

    fn capture_active(&self) -> Option<bool> {
        Some(hook::capture_active())
    }

    fn stop(&self) {
        let _ = self.set_cursor_constraint(None);
        oink_winsvc::client::release_if_connected();
        // 先取出句柄再等待线程退出：回调里若调用 set_mode，不会因为这把锁死锁
        if let Some(h) = self.take_hook() {
            h.stop();
        }
    }

    fn set_cursor_constraint(&self, bounds: Option<oink_foundation::Rect>) -> Result<(), InputError> {
        let hook = self
            .hook
            .lock()
            .map_err(|_| InputError::Platform("capture lock poisoned".into()))?;
        self.confinement
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set(bounds, hook.as_ref())
    }
    fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError> {
        let mode = if self.input_safety().active() {
            mode
        } else {
            CaptureMode::Local
        };
        if hook::on_hook_thread() {
            return hook::apply_mode_here(mode);
        }
        let g = self.hook.lock().map_err(|_| InputError::Platform("锁已损坏".into()))?;
        g.as_ref().ok_or(InputError::NotStarted)?.set_mode(mode)
    }

    fn set_drag_anchor(&self, anchor: Option<oink_foundation::dnd::DragAnchor>) -> Result<(), InputError> {
        if anchor.is_some() && !self.input_safety().active() {
            return Err(InputError::NotStarted);
        }
        if hook::on_hook_thread() {
            return if hook::apply_drag_anchor_here(anchor) {
                Ok(())
            } else {
                Err(InputError::Unsupported)
            };
        }
        self.hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .ok_or(InputError::NotStarted)?
            .set_drag_anchor(anchor)
    }

    fn set_relative_capture(&self, enabled: bool) -> Result<(), InputError> {
        let enabled = enabled && self.input_safety().active();
        let g = self.hook.lock().map_err(|_| InputError::Platform("锁已损坏".into()))?;
        g.as_ref().ok_or(InputError::NotStarted)?.set_relative_capture(enabled)
    }

    fn restore_local(&self, at: Point) -> Result<(), InputError> {
        if !self.input_safety().active() {
            return self.set_mode(CaptureMode::Local);
        }
        if hook::on_hook_thread() {
            return hook::restore_local_here(at, None);
        }
        let secure = service::optional(oink_winsvc::wire::Command::Ping)?.is_some_and(|s| s.desktop != "Default");
        let g = self.hook.lock().map_err(|_| InputError::Platform("锁已损坏".into()))?;
        let hook = g.as_ref().ok_or(InputError::NotStarted)?;
        if secure {
            hook.set_mode(CaptureMode::Local)?;
        } else {
            hook.restore_local(at, None)?;
        }
        drop(g);
        if secure {
            self.inject(&InjectEvent::MoveTo { pos: at })?;
        }
        Ok(())
    }

    fn restore_local_with_completion(
        &self,
        at: Point,
        complete: Box<dyn FnOnce(bool) + Send>,
    ) -> Result<(), InputError> {
        if !self.input_safety().active() {
            self.set_mode(CaptureMode::Local)?;
            complete(false);
            return Ok(());
        }
        if hook::on_hook_thread() {
            return hook::restore_local_here(at, Some(complete));
        }
        let secure = service::optional(oink_winsvc::wire::Command::Ping)?.is_some_and(|s| s.desktop != "Default");
        if secure {
            self.restore_local(at)?;
            complete(false); // This desktop cannot attest secure-desktop visibility.
            return Ok(());
        }
        let hook = self
            .hook
            .lock()
            .map_err(|_| InputError::Platform("capture lock poisoned".into()))?;
        hook.as_ref()
            .ok_or(InputError::NotStarted)?
            .restore_local(at, Some(complete))
    }

    fn press_drag_at(&self, at: Point) -> Result<(), InputError> {
        if !self.input_safety().active() {
            return Err(InputError::NotStarted);
        }
        let _dpi = dpi::ThreadDpiGuard::per_monitor_v2();
        // SAFETY: virtual-desktop metrics are read-only under the DPI guard.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        send(&[inject::drag_press_at(at, (vx, vy), (vw, vh))])
    }
    fn release_drag_at(&self, at: Point) -> Result<(), InputError> {
        if !self.input_safety().active() {
            return Err(InputError::NotStarted);
        }
        let _dpi = dpi::ThreadDpiGuard::per_monitor_v2();
        // SAFETY: virtual-desktop metrics are read-only under the DPI guard.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        if !at.x.is_finite()
            || !at.y.is_finite()
            || vw <= 0
            || vh <= 0
            || at.x < f64::from(vx)
            || at.y < f64::from(vy)
            || at.x >= f64::from(vx) + f64::from(vw)
            || at.y >= f64::from(vy) + f64::from(vh)
        {
            return Err(InputError::Unsupported);
        }
        let mut clip = windows::Win32::Foundation::RECT::default();
        // A lost native hold may leave remote-mode confinement behind. Never
        // let Windows clamp a positioned source UP onto that unrelated park.
        // SAFETY: read-only clipping query with valid output storage.
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetClipCursor(&mut clip) }
            .map_err(|error| InputError::Platform(error.to_string()))?;
        if at.x < f64::from(clip.left)
            || at.y < f64::from(clip.top)
            || at.x >= f64::from(clip.right)
            || at.y >= f64::from(clip.bottom)
        {
            return Err(InputError::Unsupported);
        }
        send(&[inject::drag_release_at(at, (vx, vy), (vw, vh))])
    }
    fn rebase_pointer_motion(&self) -> Result<Point, InputError> {
        *self
            .relative_acc
            .lock()
            .map_err(|_| InputError::Platform("relative accumulator poisoned".into()))? = (0., 0.);
        self.cursor_position()
    }
    fn inject(&self, event: &InjectEvent) -> Result<(), InputError> {
        let cleanup = matches!(
            event,
            InjectEvent::Key { pressed: false, .. } | InjectEvent::Button { pressed: false, .. }
        );
        if !cleanup && !self.input_safety().active() {
            return Err(InputError::NotStarted);
        }
        let _dpi = dpi::ThreadDpiGuard::per_monitor_v2();
        match event {
            InjectEvent::MoveRelative { dx, dy } => {
                if !dx.is_finite() || !dy.is_finite() {
                    return Ok(());
                }
                let mut acc = self
                    .relative_acc
                    .lock()
                    .map_err(|_| InputError::Platform("relative accumulator poisoned".into()))?;
                let x = inject::take_whole(&mut acc.0, *dx);
                let y = inject::take_whole(&mut acc.1, *dy);
                drop(acc); // Only fractional accounting is shared, never native/pipe latency.
                send(&[inject::move_relative(x, y)])?;
                pointer_capture::record_motion(x != 0 || y != 0);
                Ok(())
            }
            // Windows still injects absolute position; the supplied delta is read-only capture evidence.
            InjectEvent::MoveTo { pos } | InjectEvent::MoveToWithDelta { pos, .. } => {
                // SAFETY: 读取虚拟桌面范围（本线程 PMv2，物理像素）。
                let (vx, vy, vw, vh) = unsafe {
                    (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN),
                    )
                };
                let x = pos.x.floor() as i32;
                let y = pos.y.floor() as i32;
                // Count routed nonzero deltas, including movement against a clip/edge.
                // Plain MoveTo is also used for handoff/restore; it is not motion evidence.
                let nonzero = matches!(event, InjectEvent::MoveToWithDelta { dx, dy, .. }
                    if crate::pointer_capture::windows::nonzero_motion(*dx, *dy));
                send(&[inject::move_abs(
                    inject::normalize(x, vx, vw),
                    inject::normalize(y, vy, vh),
                )])?;
                pointer_capture::record_motion(nonzero);
                Ok(())
            }
            InjectEvent::Button { button, pressed } => {
                send(&[inject::button(*button, *pressed).ok_or(InputError::Unsupported)?])
            }
            InjectEvent::TouchpadContacts(frame) => {
                if hook::on_hook_thread() {
                    return if hook::contacts_here(frame) {
                        Ok(())
                    } else {
                        Err(InputError::Unsupported)
                    };
                }
                let secure =
                    service::optional(oink_winsvc::wire::Command::Ping)?.is_some_and(|s| s.desktop != "Default");
                let guard = self
                    .hook
                    .lock()
                    .map_err(|_| InputError::Platform("hook lock poisoned".into()))?;
                let hook = guard.as_ref().ok_or(InputError::NotStarted)?;
                if secure {
                    hook.cancel_touchpad()?;
                    return Err(InputError::Unsupported);
                }
                if hook.contacts(*frame)? {
                    Ok(())
                } else {
                    Err(InputError::Unsupported)
                }
            }
            InjectEvent::TouchpadScroll {
                dx,
                dy,
                pixels_per_reference,
                wheel,
            } => {
                // A hook callback must never wait for helper IPC. Remote input
                // normally arrives on the network thread; the reentrant path is
                // already on the active input desktop.
                let secure = !hook::on_hook_thread()
                    && service::optional(oink_winsvc::wire::Command::Ping)?.is_some_and(|s| s.desktop != "Default");
                let pixels = (dx * pixels_per_reference, dy * pixels_per_reference);
                let remaining = if hook::on_hook_thread() {
                    if secure {
                        hook::cancel_touchpad_here();
                        (1.0, 1.0)
                    } else {
                        hook::touchpad_here(pixels, cursor_dpi(), wheel.phase)?
                    }
                } else {
                    let hook = self
                        .hook
                        .lock()
                        .map_err(|_| InputError::Platform("hook lock poisoned".into()))?;
                    if let Some(hook) = hook.as_ref() {
                        if secure {
                            hook.cancel_touchpad()?;
                            (1.0, 1.0)
                        } else {
                            hook.touchpad(
                                (dx * pixels_per_reference, dy * pixels_per_reference),
                                cursor_dpi(),
                                wheel.phase,
                            )?
                        }
                    } else {
                        (1.0, 1.0)
                    }
                };
                if remaining == (0.0, 0.0) {
                    return Ok(());
                }
                self.inject(&InjectEvent::Scroll {
                    dx: wheel.dx * remaining.0,
                    dy: wheel.dy * remaining.1,
                    unit: wheel.unit,
                    phase: wheel.phase,
                })
            }
            InjectEvent::Scroll { dx, dy, unit, .. } => {
                let lines = settings::wheel_lines_per_notch();
                let (ux, uy) = match unit {
                    ScrollUnit::Line => (inject::notch_to_wheel(*dx), inject::notch_to_wheel(*dy)),
                    ScrollUnit::Pixel => {
                        let dpi = cursor_dpi();
                        (
                            inject::pixel_to_wheel(*dx, dpi, lines),
                            inject::pixel_to_wheel(*dy, dpi, lines),
                        )
                    }
                };
                let (wx, wy) = {
                    let mut acc = self
                        .scroll_acc
                        .lock()
                        .map_err(|_| InputError::Platform("锁已损坏".into()))?;
                    // dx > 0 = 视图向左 = 负的 HWHEEL
                    (inject::take_whole(&mut acc.0, -ux), inject::take_whole(&mut acc.1, uy))
                };
                let mut inputs = Vec::with_capacity(2);
                if wy != 0 {
                    inputs.push(inject::wheel(wy, false));
                }
                if wx != 0 {
                    inputs.push(inject::wheel(wx, true));
                }
                if inputs.is_empty() { Ok(()) } else { send(&inputs) }
            }
            InjectEvent::Key { code, pressed } => {
                if win_actions::key_dropped(*code) {
                    tracing::debug!("mac-only key: dropped");
                    return Ok(());
                }
                if let Some(keys) = win_actions::key_shortcut(*code) {
                    let mut held = self
                        .shortcut_keys
                        .lock()
                        .map_err(|_| InputError::Platform("锁已损坏".into()))?;
                    if !*pressed {
                        held.remove(code);
                        return Ok(());
                    }
                    if !held.insert(*code) {
                        return Ok(()); // 按住的重复：只触发一次
                    }
                    drop(held);
                    return run_actions(&[WinAction::Shortcut(keys)]);
                }
                self.lock_chord
                    .lock()
                    .map_err(|_| InputError::Platform("lock chord mutex poisoned".into()))?
                    .key(
                        *code,
                        *pressed,
                        |code, pressed| send(&[inject::key(code, pressed).ok_or(InputError::Unsupported)?]),
                        || {
                            // OS workaround: Windows reserves Win+L below the injected-key
                            // lock-hotkey path (like SAS); SendInput cannot lock this way.
                            // Use the documented LockWorkStation API from this interactive
                            // user's backend on WinSta0\Default, not the session-0 broker.
                            // UIAccess is retained with elevated foreground windows; this API
                            // locks the session, not that window, so foreground UIPI is irrelevant.
                            // Detection follows routing's mapping/hotkey suppression. L is
                            // consumed and Win released via the existing SYSTEM helper (or
                            // direct SendInput) BEFORE the async lock, keeping both ledgers clear.
                            // https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-lockworkstation
                            // https://learn.microsoft.com/windows/apps/dev-tools/winapp-cli/ui-automation
                            // SAFETY: production injection thread on the interactive desktop;
                            // tests exercise LockChord with fake send/lock functions only.
                            unsafe { LockWorkStation() }
                                .map_err(|e| InputError::Platform(format!("LockWorkStation: {e}")))
                        },
                    )
            }
            InjectEvent::Volume {
                code,
                pressed,
                modifiers,
                ..
            } => {
                if !code.is_volume() {
                    return Err(InputError::Unsupported);
                }
                let added: Vec<_> = (0..8)
                    .filter(|bit| modifiers & (1 << bit) != 0)
                    .map(|bit| KeyCode::keyboard(0xe0 + bit))
                    .filter(|key| {
                        let vk = [0xA2, 0xA0, 0xA4, 0x5B, 0xA3, 0xA1, 0xA5, 0x5C][(key.usage() - 0xe0) as usize];
                        // SAFETY: vk is a valid modifier virtual-key code; this read-only API takes no pointers.
                        unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk) >= 0 }
                    })
                    .collect();
                let mut inputs = Vec::new();
                for key in &added {
                    inputs.push(inject::key(*key, true).ok_or(InputError::Unsupported)?);
                }
                inputs.push(inject::key(*code, *pressed).ok_or(InputError::Unsupported)?);
                for key in added.iter().rev() {
                    inputs.push(inject::key(*key, false).ok_or(InputError::Unsupported)?);
                }
                send(&inputs)
            }
            InjectEvent::Gesture(g) => {
                let actions = self.gestures.lock().map_err(|_| InputError::Platform("锁已损坏".into()))?
                    // Only native injection uses this mapper. No ordinary observer
                    // owns its mutex, including during the foreground Shell query.
                    .map_with_foreground_query(g, task_view::sample);
                if actions.is_empty() {
                    tracing::debug!("mac gesture: no Windows action");
                }
                run_actions(&actions)
            }
        }
    }

    fn system_cursor_clip(&self) -> Result<Option<oink_foundation::Rect>, InputError> {
        cursor_clip::application().map_err(|e| InputError::Platform(e.to_string()))
    }
    fn pointer_capture_sample(&self) -> Option<crate::pointer_capture::CaptureSample> {
        pointer_capture::sample()
    }

    fn cursor_injection_target(&self, event: &InjectEvent) -> Option<(Point, f64)> {
        let (InjectEvent::MoveTo { pos } | InjectEvent::MoveToWithDelta { pos, .. }) = event else {
            return None;
        };
        let _dpi = dpi::ThreadDpiGuard::per_monitor_v2();
        // SAFETY: read-only virtual-desktop metrics, no handles or borrowed memory.
        let (x, y, w, h) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        Some((inject::absolute_target(*pos, (x, y), (w, h)), 0.))
    }

    fn cursor_position(&self) -> Result<Point, InputError> {
        // Hook callbacks must never wait on IPC. They already run on their desktop.
        if !hook::on_hook_thread()
            && let Some(s) = service::optional(oink_winsvc::wire::Command::Position)?
        {
            return Ok(Point::new(f64::from(s.x), f64::from(s.y)));
        }
        let _dpi = dpi::ThreadDpiGuard::per_monitor_v2();
        let mut p = POINT::default();
        // SAFETY: 输出参数指向本地变量。
        unsafe { GetCursorPos(&mut p) }.map_err(|e| InputError::Platform(format!("GetCursorPos: {e}")))?;
        Ok(Point::new(f64::from(p.x), f64::from(p.y)))
    }

    fn feel_profile(&self) -> FeelProfile {
        settings::feel_profile()
    }

    fn permissions(&self) -> InputPermissions {
        InputPermissions {
            accessibility: PermissionState::NotApplicable,
            input_monitoring: PermissionState::NotApplicable,
            secure_input_active: false,
        }
    }
}

fn session_locked() -> Option<bool> {
    use windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTS_SESSIONSTATE_UNLOCK, WTSFreeMemory, WTSINFOEXW,
        WTSQuerySessionInformationW, WTSSessionInfoEx,
    };
    let mut buffer = windows::core::PWSTR::null();
    let mut len = 0;
    // SAFETY: outputs are writable; queries only the current session, with no change to its state.
    if unsafe { WTSQuerySessionInformationW(None, WTS_CURRENT_SESSION, WTSSessionInfoEx, &mut buffer, &mut len) }
        .is_err()
    {
        return None;
    }
    if buffer.is_null() {
        return None;
    }
    let result = if len as usize >= std::mem::size_of::<WTSINFOEXW>() {
        // SAFETY: successful WTS query allocated this aligned structure; length checked above.
        let info = unsafe { &*buffer.0.cast::<WTSINFOEXW>() };
        if info.Level == 1 {
            // SAFETY: Level==1 identifies the active union field. Never retain names or other fields.
            match unsafe { info.Data.WTSInfoExLevel1.SessionFlags } as u32 {
                WTS_SESSIONSTATE_LOCK => Some(true),
                WTS_SESSIONSTATE_UNLOCK => Some(false),
                _ => None,
            }
        } else {
            None
        }
    } else {
        None
    };
    // SAFETY: buffer was allocated by the successful WTS query and is freed exactly once.
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    result
}
