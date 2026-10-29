//! 注入：普通绝对移动与按钮用原生 HID；其它事件用 CG + `HIDSystemState` 事件源。
//! 本机物理按钮仍按住时，所有合成抬起投到 Session，保留 HID 按住状态。
//!
//! - 事件源的 `userData` 设为 [`MAGIC`]，截获回调据此识别并跳过自己注入的事件。
//! - 事件源的本地事件抑制间隔设为 0，避免注入后本机键鼠被屏蔽 250 ms。
//! - 移动一律用绝对位置，同时填 `DeltaX/Y`；有按钮按下时发拖动事件。`DeltaX/Y` 是整数字段，
//!   不足 1 pt 的部分累积到下次（慢速移动也有位移）。`MoveToWithDelta` 用给定的位移（被墙裁剪之前的），
//!   这样指针顶在屏幕边上时应用仍看到朝外的位移。
//! - 正常移动用一个绝对 HID 事件（朝下量化到 1/256 pt），session tap 只补 raw delta / flags / 标记，
//!   不改坐标，避免 CG 移动使 AppKit 打字隐藏持续。游戏相对位移与拖动仍用 CG。
//! - 普通按钮也用原生 HID down/up，让首次点击激活应用的键盘 / TSM 上下文；session tap 仅补语义
//!   修饰键与已计算点击计数，不改原生位置、时间、编号或注入标记。物理按钮保护及原生提交前的回退仍使用 CG。
//! - 位置先按 float32 **向下**取整（[`quartz_point`]）：WindowServer 用 float32 存光标位置，路由把被墙挡住的
//!   指针放在 `bottom - 1e-6`（如 1116.999999），直接交给 `CGEventPost` 会被舍入成 1117.0——落在屏幕外
//!   （右、下边界不属于屏幕）。光标于是画在屏幕外（底边闪烁），自动隐藏的 Dock、全屏时的菜单栏也认为指针
//!   不在边上而不出现或反复收起（bug 8 的后续）。向下取到 float32 后，同一个 `CGEventPost` 的推边在
//!   macOS 26 / 14 上都能让 Dock 约 0.3 s 出现，不再需要 IOHIDSystem 的第二条通道。
//! - 定位拖动事件另外向下取到整数 Quartz point，避免负边界的小数在原生目标查找时落到屏外；
//!   raw delta 和普通移动精度不变。原地刷新拖影保持当前坐标，不重新定位。
//! - 点击计数按系统双击间隔与 4 pt 距离自己计算（`kCGMouseEventClickState`）。
//! - 滚动：线上 `dy > 0` 表示视图向文档顶部移动，与 CGEvent 滚动增量的符号含义一致（应用把正增量理解为
//!   「向上滚」），所以原样注入。小数部分按轴累积，阶段结束时清零。
//! - 按键：修饰键发 `FlagsChanged`；所有事件都带上当前修饰键 flags（注入的 + 本机键盘按住的 + HID 系统的
//!   大写锁定）。
//! - 音量、播放控制键（Consumer 页，以及键盘页的音量键）发系统定义事件（`sysevent`），系统像实体键
//!   一样调节并弹出提示；按着时再次按下带重复标志。
//! - 手势（`InjectEvent::Gesture`）按 `sysevent` 合成系统的手势事件重放。
//! - 每次注入（限流到每秒一次）都声明一次用户活动，点亮熄灭的屏幕、推迟熄屏（`activity` 模块）。
//! - Caps Lock 交给 [`CapsInjector`]：普通模式用 IOKit 设置锁定，切源模式保留原生 Caps DOWN / UP；
//!   不能用普通 `FlagsChanged` 代替（见 `capslock` 模块）。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton, CGScrollEventUnit,
};
use oink_foundation::Point;
use oink_foundation::input::{InjectEvent, KeyCode, MouseButton, ScrollPhase, ScrollUnit};

use super::activity::UserActivity;
use super::capslock::{self, CapsInjector};
use super::cursor_reveal::NativeButton;
use super::keymap::{self, AuxKey, MacKeyCode, NxKey};
use super::{sysevent, system};
use crate::InputError;

/// 写在自己注入的事件的 `kCGEventSourceUserData` 字段里（"OINK" 的 ASCII）。
pub(crate) const MAGIC: i64 = 0x4F49_4E4B;

fn capture_keyboard_serial() -> u32 {
    CGEventSource::counter_for_event_type(CGEventSourceStateID::CombinedSessionState, CGEventType::KeyDown)
}

/// CGEventFlags 中与设备相关的左右修饰键位（IOKit `NX_DEVICE*KEYMASK`）。
pub(crate) mod device_bits {
    pub const LCTL: u64 = 0x0000_0001;
    pub const LSHIFT: u64 = 0x0000_0002;
    pub const RSHIFT: u64 = 0x0000_0004;
    pub const LCMD: u64 = 0x0000_0008;
    pub const RCMD: u64 = 0x0000_0010;
    pub const LALT: u64 = 0x0000_0020;
    pub const RALT: u64 = 0x0000_0040;
    pub const RCTL: u64 = 0x0000_2000;
}

/// mac 修饰键码 → (与设备无关的掩码, 左右位)。
pub(crate) fn modifier_flags(mac: MacKeyCode) -> Option<(CGEventFlags, u64)> {
    use device_bits::*;
    Some(match mac {
        0x3B => (CGEventFlags::MaskControl, LCTL),
        0x3E => (CGEventFlags::MaskControl, RCTL),
        0x38 => (CGEventFlags::MaskShift, LSHIFT),
        0x3C => (CGEventFlags::MaskShift, RSHIFT),
        0x3A => (CGEventFlags::MaskAlternate, LALT),
        0x3D => (CGEventFlags::MaskAlternate, RALT),
        0x37 => (CGEventFlags::MaskCommand, LCMD),
        0x36 => (CGEventFlags::MaskCommand, RCMD),
        0x3F => (CGEventFlags::MaskSecondaryFn, 0),
        0x39 => (CGEventFlags::MaskAlphaShift, 0),
        _ => return None,
    })
}

struct Source(CFRetained<CGEventSource>);
// SAFETY: CGEventSource 是不可变使用的 CF 对象（创建后只读），CoreGraphics 允许跨线程使用。
unsafe impl Send for Source {}
// SAFETY: 同上。
unsafe impl Sync for Source {}

/// Session acknowledgement precedes WindowServer's cursor-position commit.
/// Carry that one pending point to an immediately following click/scroll; never
/// retain a permanent injected-position override across physical motion or warp.
#[derive(Clone, Copy)]
struct PendingPosition {
    before: CGPoint,
    predecessor: CGPoint,
    at: CGPoint,
    epoch: u64,
}

/// The outer Result from button() fails only before submission. An accepted
/// post can still lose its tap ACK; callers which retry must keep that distinct.
struct ButtonSubmission {
    acknowledgement: Result<(), InputError>,
}

#[derive(Clone, Copy)]
enum ButtonModifiers {
    Ordinary,
    SourceRelease,
}
impl ButtonModifiers {
    fn for_button(held_source: bool, button: MouseButton, pressed: bool) -> Self {
        if held_source && button == MouseButton::Left && !pressed {
            Self::SourceRelease
        } else {
            Self::Ordinary
        }
    }
    fn resolve(self, flags: CGEventFlags) -> CGEventFlags {
        match self {
            Self::Ordinary => flags,
            Self::SourceRelease => CGEventFlags::empty(),
        }
    }
}
/// Keep the injector serialized across the ACK, while the caller releases its
/// cursor transition lock so capture can deliver that ACK.
pub(super) struct HeldSubmission<'a> {
    receipt: super::cursor_reveal::Receipt<'a>,
    _state: std::sync::MutexGuard<'a, State>,
}
impl HeldSubmission<'_> {
    pub fn wait_observed(self) -> Result<(), InputError> {
        self.receipt.wait_observed()
    }
}
impl ButtonSubmission {
    fn acknowledged(self) -> Result<(), InputError> {
        self.acknowledgement
    }
}
impl PendingPosition {
    fn committed(self, current: CGPoint, epoch: u64) -> bool {
        self.epoch != epoch || current == self.at
    }

    fn resolve(self, current: CGPoint, epoch: u64) -> CGPoint {
        if self.epoch == epoch && (current == self.before || current == self.predecessor) {
            self.at
        } else {
            current
        }
    }
}

#[derive(Default)]
struct State {
    held_source: Option<u64>,
    gesture_trace: crate::gesture_trace::Tracker,
    /// 按下的按钮（按钮编号的位）。
    buttons: u32,
    /// 注入的修饰键（mac 键码）。
    mods: HashSet<MacKeyCode>,
    /// 注入后仍按着的普通键（用于标记自动重复）。
    down: HashSet<MacKeyCode>,
    /// 注入后仍按着的系统定义事件键（媒体键、亮度键）。
    down_aux: HashSet<NxKey>,
    /// 每个按钮最近一次按下：时间、位置、点击计数。
    clicks: HashMap<u32, (Instant, CGPoint, i64)>,
    /// An accepted native DOWN owns its UP event number; zero means a CG pair.
    native_numbers: [i16; 32],
    last_native_number: i16,
    last_pos: Option<CGPoint>,
    pending_position: Option<PendingPosition>,
    last_move_native: bool,
    /// A failed absolute target cannot silently turn a subsequent DOWN into a
    /// click at an old cursor sample. Session changes abandon the old target.
    failed_position_context: Option<u64>,
    /// 移动位移不足 1 pt 的余数（`DeltaX/Y` 是整数字段）。
    delta_rem: (f64, f64),
    scroll_rem: (f64, f64),
}

impl State {
    fn click_count(&self, number: u32, pressed: bool, at: CGPoint, now: Instant, interval: f64) -> i64 {
        match self.clicks.get(&number) {
            Some((_, _, count)) if !pressed => *count,
            Some((t, last, count)) if now.duration_since(*t).as_secs_f64() <= interval && dist(*last, at) <= 4. => {
                count.saturating_add(1).min(i64::from(i32::MAX))
            }
            _ => 1,
        }
    }

    fn native_number(&self, number: u32, pressed: bool, held_drag_source: bool) -> Option<i16> {
        let previous = *self.native_numbers.get(number as usize)?;
        if pressed && held_drag_source {
            return None;
        }
        if pressed {
            Some(if self.last_native_number == i16::MAX {
                1
            } else {
                self.last_native_number + 1
            })
        } else {
            (previous != 0).then_some(previous)
        }
    }

    /// Called after OS acceptance, before waiting for a session ACK. Neither a
    /// failed submission nor an ACK timeout may create a second DOWN/UP.
    fn accept_button(&mut self, button: NativeButton, at: CGPoint, now: Instant, native: bool) {
        let mask = 1 << button.number.min(31);
        if button.pressed {
            self.buttons |= mask;
            self.clicks.insert(button.number, (now, at, i64::from(button.count)));
            if native {
                self.last_native_number = button.event_number;
            }
        } else {
            self.buttons &= !mask;
        }
        if let Some(number) = self.native_numbers.get_mut(button.number as usize) {
            *number = if native && button.pressed {
                button.event_number
            } else {
                0
            };
        }
        self.last_pos = Some(at);
    }

    fn position_result(&mut self, context: u64, succeeded: bool) {
        if succeeded {
            if self.failed_position_context.take().is_some() {
                tracing::debug!("button target: successful absolute input cleared failed target");
            }
        } else {
            self.pending_position = None;
            self.failed_position_context = Some(context);
        }
    }

    fn position_ready(&mut self, context: u64) -> bool {
        if let Some(failed) = self.failed_position_context {
            if failed == context {
                return false;
            }
            self.failed_position_context = None;
            tracing::debug!("button target: mode or capture change abandoned failed old target");
        }
        true
    }

    fn enter_relative_input(&mut self) {
        self.pending_position = None;
        // Game-mode changes do not necessarily change capture mode. Relative
        // input uses the current cursor, so abandon the old absolute target;
        // this does not claim that its failed post was eventually delivered.
        if self.failed_position_context.take().is_some() {
            tracing::debug!("button target: relative input abandoned failed absolute target");
        }
    }

    fn finish_drag_press(
        &mut self,
        context: u64,
        result: Result<ButtonSubmission, InputError>,
    ) -> Result<(), InputError> {
        match result {
            Ok(submission) => {
                self.position_result(context, submission.acknowledgement.is_ok());
                if let Err(error) = submission.acknowledgement {
                    tracing::warn!(%error,
                        "drag source DOWN was submitted without a tap ACK; awaiting AppKit start, never retrying DOWN");
                }
                // Clipboard verifies AppKit began separately and releases on
                // failure. Err here would retry an already submitted DOWN.
                Ok(())
            }
            Err(error) => {
                self.position_result(context, false);
                Err(error)
            }
        }
    }
}

fn source_motion_admitted(held: Option<u64>, admitted_epoch: u64, current_epoch: u64) -> bool {
    held.is_none() && admitted_epoch == current_epoch
}

pub(crate) struct Injector {
    safety: Option<Arc<crate::session_gate::SessionGate>>,
    gestures: Mutex<super::gesture_diagnostics::Tracker>,
    pub(crate) physical_buttons: Arc<super::physical_buttons::PhysicalButtons>,
    pub(super) motion: Arc<super::cursor_reveal::Motion>,
    source: Source,
    state: Mutex<State>,
    caps: CapsInjector,
    activity: UserActivity,
    capture_refresh: super::cursor_refresh::Observer,
    hold_epoch: AtomicU64,
}

impl Injector {
    pub(crate) fn freeze_source(
        &self,
        generation: u64,
        freeze: impl FnOnce() -> Result<(), InputError>,
        publish: impl FnOnce(oink_foundation::dnd::DragAnchor),
    ) -> Result<oink_foundation::dnd::DragAnchor, InputError> {
        let mut st = self.lock_state()?;
        self.check_session(false)?;
        if st.held_source.is_some() || !st.position_ready(self.motion.position_context()) {
            return Err(InputError::Platform(
                "source freeze has an active hold or unconfirmed input".into(),
            ));
        }
        self.hold_epoch.fetch_add(1, Ordering::AcqRel);
        st.held_source = Some(generation);
        let pending = st.pending_position;
        let epoch = self.motion.position_epoch();
        let result = super::drag_hold::freeze_anchor(generation, freeze, || {
            // A Session tap receipt precedes WindowServer's cursor commit.
            // Drain that already-submitted endpoint while physical motion is
            // dissociated and this ledger excludes further injected movement.
            // The deadline only fails the hold; it never establishes readiness
            // and does not post or retry an event.
            let deadline = Instant::now() + std::time::Duration::from_millis(250);
            loop {
                let at = checked_location()?;
                let cg = CGPoint::new(at.x, at.y);
                if pending.is_none_or(|p| p.committed(cg, epoch)) {
                    return Ok(at);
                }
                if Instant::now() >= deadline {
                    return Err(InputError::Platform(
                        "source freeze: prior injected position did not commit".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        match result {
            Ok(anchor) => {
                st.pending_position = None;
                st.last_pos = Some(CGPoint::new(anchor.at.x, anchor.at.y));
                publish(anchor);
                Ok(anchor)
            }
            Err(error) => {
                st.held_source = None;
                self.hold_epoch.fetch_add(1, Ordering::AcqRel);
                Err(error)
            }
        }
    }

    pub(crate) fn clear_source_hold(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.held_source = None;
        self.hold_epoch.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn new(safety: Arc<crate::session_gate::SessionGate>) -> Result<Self, InputError> {
        let mut injector = Self::with_activity(UserActivity::new()?)?;
        injector.safety = Some(safety);
        Ok(injector)
    }
    fn with_activity(activity: UserActivity) -> Result<Self, InputError> {
        let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok_or_else(|| InputError::Platform("CGEventSourceCreate 失败".into()))?;
        CGEventSource::set_user_data(Some(&src), MAGIC);
        CGEventSource::set_local_events_suppression_interval(Some(&src), 0.0);
        let capture_refresh = super::cursor_refresh::Observer::default();
        capture_refresh.initialize();
        Ok(Self {
            safety: None,
            capture_refresh,
            source: Source(src),
            physical_buttons: Arc::default(),
            motion: Arc::default(),
            state: Mutex::new(State::default()),
            gestures: Mutex::new(super::gesture_diagnostics::Tracker::default()),
            caps: CapsInjector::default(),
            activity,
            hold_epoch: AtomicU64::new(0),
        })
    }

    pub(crate) fn stop_session(&self) {
        // The injection owner has already attempted its finite held-input
        // releases. Never carry an unacknowledged position, click count or
        // fractional motion/scroll into a resumed input session.
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = State::default();
        self.hold_epoch.fetch_add(1, Ordering::AcqRel);
        *self.gestures.lock().unwrap_or_else(|e| e.into_inner()) = Default::default();
        self.physical_buttons.reset();
        self.capture_refresh.boundary();
    }

    pub(crate) fn declare_user_activity(&self) -> Result<bool, InputError> {
        self.activity.declare()
    }

    pub(crate) fn gesture_boundary(&self, reason: &'static str) {
        self.capture_refresh.boundary();
        // Boundary formatting is ordinary work; never hold the ledger mutex
        // while a logging subscriber formats or queues this snapshot.
        let tracker = *self.gestures.lock().unwrap_or_else(|e| e.into_inner());
        tracker.boundary("injected", reason);
    }

    pub(crate) fn reset_user_idle(&self) -> Result<bool, InputError> {
        self.activity.reset_idle(&self.source.0)
    }

    pub(crate) fn inject(&self, event: &InjectEvent) -> Result<(), InputError> {
        let hold_epoch = self.hold_epoch.load(Ordering::Acquire);
        self.check_session(matches!(
            event,
            InjectEvent::Key { pressed: false, .. } | InjectEvent::Button { pressed: false, .. }
        ))?;
        // Record activity before delivery; only the ordinary worker may wake
        // displays through powerd. Native input never waits for that declaration.
        self.activity.request();
        if let InjectEvent::Volume {
            code,
            pressed,
            repeat,
            modifiers,
        } = event
        {
            // Volume is dispatched on ordinary work and uses only the explicit
            // modifier payload. Its native system-event call must not hold the
            // mouse/key injector ledger while real-time motion is arriving.

            let key = keymap::hid_to_nx(*code)
                .filter(|_| code.is_volume())
                .ok_or(InputError::Unsupported)?;
            let flags = (0..8)
                .filter(|bit| modifiers & (1 << bit) != 0)
                .filter_map(|bit| keymap::hid_to_mac(KeyCode::keyboard(0xe0 + bit)))
                .filter_map(modifier_flags)
                .fold(0, |flags, (mask, _)| flags | mask.0);
            let result = if sysevent::post_aux_key_with_flags(
                AuxKey {
                    key,
                    pressed: *pressed,
                    repeat: *repeat,
                },
                flags,
            ) {
                Ok(())
            } else {
                Err(InputError::Platform("创建音量事件失败".into()))
            };
            if result.is_ok() {
                self.capture_refresh.changed();
            }
            return result;
        }
        let mut st = self.lock_state()?;
        if matches!(
            event,
            InjectEvent::MoveRelative { .. } | InjectEvent::MoveTo { .. } | InjectEvent::MoveToWithDelta { .. }
        ) && !source_motion_admitted(st.held_source, hold_epoch, self.hold_epoch.load(Ordering::Acquire))
        {
            // The ordinary queue may already contain motion from before the
            // handoff. It must not relocate a frozen third-party drag. Explicit
            // held-source refresh/move/return calls use their generation instead.
            return Ok(());
        }
        // Recheck after acquiring the ledger: another native call may have
        // blocked across a pause while holding this mutex.
        self.check_session(matches!(
            event,
            InjectEvent::Key { pressed: false, .. } | InjectEvent::Button { pressed: false, .. }
        ))?;
        let position_context = self.motion.position_context();
        let result = match event {
            InjectEvent::TouchpadContacts(_) => Err(InputError::Unsupported),
            InjectEvent::MoveRelative { dx, dy } => {
                st.enter_relative_input();
                let p = current_location();
                self.move_to(&mut st, Point::new(p.x, p.y), Some((*dx, *dy)), true)
            }
            InjectEvent::MoveTo { pos } => self.move_to(&mut st, *pos, None, false),
            InjectEvent::MoveToWithDelta { pos, dx, dy } => self.move_to(&mut st, *pos, Some((*dx, *dy)), false),
            InjectEvent::Button { button, pressed } => self
                .button(&mut st, *button, *pressed, CGEventTapLocation::HIDEventTap, None)
                .and_then(ButtonSubmission::acknowledged),
            InjectEvent::Scroll { dx, dy, unit, phase } => self.scroll(&mut st, *dx, *dy, *unit, *phase),
            // Only Windows routing emits this local variant; preserve wheel semantics if supplied directly.
            InjectEvent::TouchpadScroll { wheel, .. } => {
                self.scroll(&mut st, wheel.dx, wheel.dy, wheel.unit, wheel.phase)
            }
            InjectEvent::Key { code, pressed } => {
                let result = self.key(&mut st, *code, *pressed);
                st.pending_position = None;
                let mods = st
                    .mods
                    .iter()
                    .filter_map(|m| modifier_flags(*m))
                    .fold(0, |f, (mask, _)| f | mask.0);
                super::keyboard_diagnostics::injected(
                    result.is_ok(),
                    mods,
                    st.down.len() + st.mods.len() + st.down_aux.len(),
                );
                result
            }
            InjectEvent::Volume { .. } => unreachable!("volume is ordinary and handled before the injection ledger"),
            InjectEvent::Gesture(g) => {
                st.pending_position = None;
                let submitted = sysevent::post_gesture(g);
                if crate::native_lifecycle::lifecycle_enabled() {
                    use crate::gesture_trace::{Decision, Observation, Outcome, Stage};
                    let stamp = crate::native_lifecycle::capture_stamp();
                    let source = crate::gesture_trace::source();
                    let decision = if !submitted {
                        Decision::SubmissionFailed
                    } else if g.phase() == Some(oink_foundation::input::GesturePhase::Cancelled) {
                        Decision::NativeCancel
                    } else if matches!(g, oink_foundation::input::Gesture::Magnify { .. }) {
                        Decision::NativeMagnify
                    } else {
                        Decision::NativeDockControl
                    };
                    if let Some(summary) = st.gesture_trace.observe(
                        *g,
                        crate::native_lifecycle::gesture_time(stamp),
                        crate::native_lifecycle::lifecycle_generation(),
                        source,
                        Outcome {
                            decision,
                            threshold: None,
                            notches: 0,
                        },
                    ) {
                        crate::native_lifecycle::emit_gesture(
                            stamp,
                            Observation {
                                stage: Stage::MacSynth,
                                source,
                                summary,
                            },
                        );
                    }
                }
                if submitted {
                    self.gestures.lock().unwrap_or_else(|e| e.into_inner()).record(
                        *g,
                        "inject",
                        super::tap::now_event_ns(),
                    );
                }
                tracing::debug!(
                    ?g,
                    submitted,
                    "gesture injection: submission result (delivery unconfirmed)"
                );
                if submitted {
                    Ok(())
                } else {
                    Err(InputError::Platform("创建手势事件失败".into()))
                }
            }
        };
        if matches!(event, InjectEvent::MoveTo { .. } | InjectEvent::MoveToWithDelta { .. }) {
            st.position_result(position_context, result.is_ok());
        }
        if result.is_ok() {
            match event {
                InjectEvent::MoveToWithDelta { dx, dy, .. } | InjectEvent::MoveRelative { dx, dy }
                    if dx.is_finite() && dy.is_finite() && (*dx != 0. || *dy != 0.) =>
                {
                    self.capture_refresh.motion(Instant::now(), capture_keyboard_serial());
                }
                InjectEvent::Key { pressed: true, .. } => self.capture_refresh.key(),
                _ => self.capture_refresh.changed(),
            }
        }
        result
    }

    pub(super) fn refresh_capture_visibility(
        &self,
        sample: crate::pointer_capture::CaptureSample,
        eligible: bool,
        captured: bool,
        reread: impl FnOnce() -> Option<crate::pointer_capture::CaptureSample>,
    ) -> Option<crate::pointer_capture::CaptureSample> {
        if self.check_session(false).is_err() {
            return Some(sample);
        }
        self.observe_visibility(sample, eligible, captured, capture_keyboard_serial(), || {
            super::cursor_refresh::resynchronize().then(reread).flatten()
        })
    }
    fn observe_visibility(
        &self,
        sample: crate::pointer_capture::CaptureSample,
        eligible: bool,
        captured: bool,
        serial: u32,
        refresh: impl FnOnce() -> Option<crate::pointer_capture::CaptureSample>,
    ) -> Option<crate::pointer_capture::CaptureSample> {
        self.capture_refresh
            .observe(sample, eligible, captured, serial, refresh)
    }

    /// 物理左键仍按着：仅让 AppKit 源拖拽收尾，不清掉 HID 层的按钮状态。
    pub(crate) fn release_physical_drag(&self) -> Result<(), InputError> {
        self.activity.request();
        let mut st = self.lock_state()?;
        self.button_with_modifiers(
            &mut st,
            MouseButton::Left,
            false,
            CGEventTapLocation::SessionEventTap,
            None,
            ButtonModifiers::SourceRelease,
        )
        .and_then(ButtonSubmission::acknowledged)
    }

    /// Use the position whose source-window hit test just succeeded, not a second
    /// cursor sample after another thread has resumed.
    /// Ok means submitted, including a missing tap ACK; Err means no DOWN was
    /// submitted. The macOS clipboard caller may retry Err and checks AppKit began
    /// independently, with a paired release when the submitted press did not start.
    pub(crate) fn press_drag_at(&self, at: Point) -> Result<(), InputError> {
        let mut st = self.lock_state()?;
        self.check_session(false)?;
        let context = self.motion.position_context();
        let result = self.button(
            &mut st,
            MouseButton::Left,
            true,
            CGEventTapLocation::HIDEventTap,
            Some(at),
        );
        st.finish_drag_press(context, result)
    }
    /// The physical UP may already have been swallowed, so st.buttons can be
    /// zero. Force the event type without creating DOWN or changing ownership.
    pub(crate) fn submit_held_drag(&self, at: Point, posted: &dyn Fn(Point)) -> Result<HeldSubmission<'_>, InputError> {
        let mut st = self.lock_state()?;
        self.check_session(false)?;
        let receipt = self.submit_move_with_buttons(&mut st, at, None, false, 1, Some(posted))?;
        Ok(HeldSubmission { receipt, _state: st })
    }

    pub(crate) fn submit_held_nudge(
        &self,
        at: Point,
        button: MouseButton,
        posted: &dyn Fn(oink_foundation::dnd::ImageNudgePost),
    ) -> Result<HeldSubmission<'_>, InputError> {
        self.with_image_nudge(at, button, |event, st| {
            // macOS can leave destination image changes unapplied until source
            // drag tracking handles another mouse event; our frozen cursor
            // supplies none, so the old image remains visible. One
            // same-position, zero-delta dragged event through the public CGEvent
            // API makes the source apply the change without moving the pointer.
            // No public destination-side signal confirms the source's image
            // exchange is ready. A guarded AppKit source still failed at 20ms
            // after independent main-queue changes; delays from 50ms were 5/5
            // successful both there and with changes inside draggingUpdated.
            // The catcher waits 100ms after the changed enumeration returns:
            // heavier source processes can lag in IPC handling, a missed nudge
            // leaves the image visible, and 100ms more at the edge is negligible.
            // This is a measured scheduling margin, not a readiness guarantee.
            // Send on every macOS version: affected versions are not known; this
            // extra stationary event is harmless where no refresh is needed.
            // The catcher queues it only after an actual image change, outside
            // AppKit's callback; already-hidden items must not trigger another
            // post, otherwise post -> image callback -> post could loop forever.
            // No private interface, warp, association change or DOWN/UP is used.
            let receipt = self.motion.submit_cg(event, CGEventTapLocation::HIDEventTap)?;
            let p = CGEvent::location(Some(event));
            posted(oink_foundation::dnd::ImageNudgePost {
                at: Point::new(p.x, p.y),
                button,
                flags: CGEvent::flags(Some(event)).0,
            });
            // Capture accounts the marked absolute placement once. Do not touch
            // last_pos, pending_position, delta_rem, or the button/key ledger here.
            Ok(HeldSubmission { receipt, _state: st })
        })
    }

    fn with_image_nudge<'a, T>(
        &'a self,
        at: Point,
        button: MouseButton,
        submit: impl FnOnce(&CGEvent, std::sync::MutexGuard<'a, State>) -> Result<T, InputError>,
    ) -> Result<T, InputError> {
        let st = self.lock_state()?;
        self.check_session(false)?;
        let event = image_nudge_event(self.src(), at, button, self.flags(&st))?;
        submit(&event, st)
    }

    pub(crate) fn rebase_pointer_motion(&self) -> Result<Point, InputError> {
        let mut st = self.lock_state()?;
        let current = current_location();
        st.last_pos = Some(current);
        st.delta_rem = (0., 0.);
        st.pending_position = None;
        Ok(Point::new(current.x, current.y))
    }

    fn src(&self) -> Option<&CGEventSource> {
        Some(&self.source.0)
    }

    fn check_session(&self, cleanup: bool) -> Result<(), InputError> {
        // One finite cleanup Up is attempted after withdrawal. CGEventPost has
        // no acceptance result, so this cannot prove an Up reached the OS after
        // TCC loss. No Down/motion retry or delayed replay is allowed.
        if !cleanup && self.safety.as_ref().is_some_and(|p| !p.snapshot().active()) {
            Err(InputError::NotStarted)
        } else {
            Ok(())
        }
    }

    /// 当前应带的修饰键 flags：注入的修饰键 + 本机键盘正按着的修饰键 + 大写锁定。
    ///
    /// 大写锁定取 IOKit 的锁定状态，不取 `CGEventSourceFlagsState` 的 AlphaShift：「用大写锁定键切换输入法」
    /// 开启时，本机真实的 Caps Lock 短按会让后者停留在 AlphaShift（实测锁定状态为关、flags 状态仍为
    /// `0x2001_0100`），注入的字母因而全部变成大写。
    fn flags(&self, st: &State) -> CGEventFlags {
        let hw = CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState);
        compose_flags(&st.mods, hw.0, capslock::hid::caps_lock() == Some(true))
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, State>, InputError> {
        // A handler may inject directly. It must not wait for the worker whose
        // posted event is queued behind this very callback.
        if super::tap::in_callback() {
            self.state
                .try_lock()
                .map_err(|_| InputError::Platform("input injector is busy during a tap callback".into()))
        } else {
            Ok(self.state.lock().unwrap_or_else(|e| e.into_inner()))
        }
    }

    fn post(&self, ev: &CGEvent) -> Result<(), InputError> {
        self.motion.post_cg(ev, CGEventTapLocation::HIDEventTap)
    }

    /// 移到 `pos`。`delta` 为 `None` 时位移取与上次注入位置之差。
    fn move_to(&self, st: &mut State, pos: Point, delta: Option<(f64, f64)>, relative: bool) -> Result<(), InputError> {
        self.move_with_buttons(st, pos, delta, relative, st.buttons)
    }

    fn move_with_buttons(
        &self,
        st: &mut State,
        pos: Point,
        delta: Option<(f64, f64)>,
        relative: bool,
        buttons: u32,
    ) -> Result<(), InputError> {
        self.submit_move_with_buttons(st, pos, delta, relative, buttons, None)?
            .wait()
    }

    fn submit_move_with_buttons(
        &self,
        st: &mut State,
        pos: Point,
        delta: Option<(f64, f64)>,
        relative: bool,
        buttons: u32,
        posted: Option<&dyn Fn(Point)>,
    ) -> Result<super::cursor_reveal::Receipt<'_>, InputError> {
        let (ty, button, num) = drag_type(buttons);
        let ev = motion_event(self.src(), ty, quartz_point(pos), button)?;
        // Track the actual posted drag endpoint, including native lookup
        // normalization, in the ordinary delta and pending-position ledgers.
        let p = CGEvent::location(Some(&ev));
        let raw = delta.unwrap_or_else(|| {
            let last = st.last_pos.unwrap_or_else(current_location);
            (p.x - last.x, p.y - last.y)
        });
        let (ix, iy) = take_delta(&mut st.delta_rem, raw);
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventDeltaX, ix);
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventDeltaY, iy);
        if relative {
            CGEvent::set_double_value_field(Some(&ev), CGEventField::EventUnacceleratedPointerMovementX, raw.0);
            CGEvent::set_double_value_field(Some(&ev), CGEventField::EventUnacceleratedPointerMovementY, raw.1);
        }
        if ty == CGEventType::OtherMouseDragged {
            CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventButtonNumber, i64::from(num));
        }
        CGEvent::set_flags(Some(&ev), self.flags(st));
        let before = current_location();
        let epoch = self.motion.position_epoch();
        let predecessor = st.pending_position.take().map_or(before, |p| p.resolve(before, epoch));
        if !relative
            && ty == CGEventType::MouseMoved
            && let Some(receipt) = self.motion.post_mouse(&ev, p)?
        {
            st.last_move_native = true;
            st.pending_position = receipt.position.map(|at| PendingPosition {
                before,
                predecessor,
                at,
                epoch,
            });
            st.last_pos = Some(p);
            return Ok(receipt);
        }
        let receipt = self.motion.submit_cg(&ev, CGEventTapLocation::HIDEventTap)?;
        st.last_move_native = false;
        if let Some(posted) = posted {
            posted(Point::new(p.x, p.y));
        }
        // Absolute drag delivery has the same cursor-commit lag as a native
        // move. Its immediately following UP must retain the accepted endpoint.
        if !relative {
            st.pending_position = Some(PendingPosition {
                before,
                predecessor,
                at: p,
                epoch,
            });
        }
        st.last_pos = Some(p);
        Ok(receipt)
    }

    pub(super) fn last_cursor_target(&self) -> Option<(Point, f64)> {
        let st = self.state.lock().ok()?;
        let p = st.pending_position.map(|p| p.at).or(st.last_pos)?;
        let ulp = |v: f64| {
            let f = v as f32;
            f64::from(f.next_up() - f).abs()
        };
        let quantum: f64 = if st.last_move_native { 1. / 256. } else { 0. };
        Some((Point::new(p.x, p.y), quantum.max(ulp(p.x)).max(ulp(p.y))))
    }

    fn next_location(&self, st: &mut State) -> CGPoint {
        let current = current_location();
        st.pending_position
            .take()
            .map_or(current, |p| p.resolve(current, self.motion.position_epoch()))
    }

    fn button(
        &self,
        st: &mut State,
        button: MouseButton,
        pressed: bool,
        tap: CGEventTapLocation,
        at: Option<Point>,
    ) -> Result<ButtonSubmission, InputError> {
        let modifiers = ButtonModifiers::for_button(st.held_source.is_some(), button, pressed);
        self.button_with_modifiers(st, button, pressed, tap, at, modifiers)
    }

    fn button_with_modifiers(
        &self,
        st: &mut State,
        button: MouseButton,
        pressed: bool,
        tap: CGEventTapLocation,
        at: Option<Point>,
        modifiers: ButtonModifiers,
    ) -> Result<ButtonSubmission, InputError> {
        let num = button_number(button);
        if pressed && at.is_none() && !st.position_ready(self.motion.position_context()) {
            return Err(InputError::Platform(
                "button target depends on a failed absolute movement".into(),
            ));
        }
        // HIDSystemState also includes our injected down; it cannot distinguish a remote
        // drag from a held local device. Use provenance recorded before local routing.
        let tap = self.physical_buttons.tap(num, pressed, tap);
        let before = current_location();
        let epoch = self.motion.position_epoch();
        let predecessor = st.pending_position.map_or(before, |p| p.resolve(before, epoch));
        let p = if let Some(at) = at {
            quartz_drag_point(at)
        } else if tap == CGEventTapLocation::SessionEventTap {
            before
        } else {
            predecessor
        };
        let now = Instant::now();
        let count = st.click_count(num, pressed, p, now, system::double_click_interval());
        // This source UP commits a Copy into Oink's catcher, independently of
        // the remote target's COPY/MOVE modifier. HID flags still include a
        // physical Ctrl whose FlagsChanged event was swallowed and forwarded.
        // Reusing those flags on UP makes AppKit change Copy to None, bypassing
        // performDragOperation and cancelling the source before its authority
        // reply commits. Native macOS 26.6.2 file-drag A/B: Ctrl UP rejects,
        // neutral UP accepts. Do not release keys or change their ledger: the
        // target must keep its modifier until its real key-up. Ordinary pointer
        // events, held positioning, and return-to-source keep their modifiers.
        let flags = modifiers.resolve(self.flags(st));
        super::keyboard_diagnostics::button_flags(flags.0);
        // A drag returning to its input device needs a new AppKit DOWN while
        // the source button is still held at HID (its first DOWN was routed
        // away). On macOS 26.6.2 IOHIDPostEvent accepts this duplicate DOWN and
        // the HID observer sees it, but WindowServer drops it before Session
        // and AppKit. CG DOWN reaches both and starts the drag. Detect that
        // state before posting; never clear the physical hold with an extra UP
        // or retry an unacknowledged native DOWN. The explicit `at` identifies
        // our drag-source panel; ordinary application activation stays native.
        // Use HID state here, including synthetic input sources, not just our
        // physical-provenance ledger (which still protects physical UPs).
        let held_drag_source =
            at.is_some() && CGEventSource::button_state(CGEventSourceStateID::HIDSystemState, CGMouseButton::Left);
        let native_number = st.native_number(num, pressed, held_drag_source);
        let native = NativeButton {
            number: num,
            pressed,
            count: count as i32,
            event_number: native_number.unwrap_or(0),
            flags: semantic_button_flags(flags),
        };
        // Keep a fallback CG DOWN/UP pair together. A protected physical release
        // always wins, including after a native DOWN. No fallback after OS
        // acceptance: even a missing session ACK must not duplicate a click.
        if tap == CGEventTapLocation::HIDEventTap
            && native_number.is_some()
            && let Some(receipt) = self.motion.post_button(native, p)?
        {
            let accepted = receipt.position.unwrap_or(p);
            // Preserve the existing click-distance and movement-delta ledger;
            // only pending cursor delivery needs the native quantized position.
            st.accept_button(native, p, now, true);
            st.pending_position = Some(PendingPosition {
                before,
                predecessor,
                at: accepted,
                epoch,
            });
            return Ok(ButtonSubmission {
                acknowledgement: receipt.wait(),
            });
        }
        let (ty, cg_button) = match (num, pressed) {
            (0, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            (0, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            (1, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
            (1, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
            (_, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
            (_, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
        };
        let ev = CGEvent::new_mouse_event(self.src(), ty, p, cg_button)
            .ok_or_else(|| InputError::Platform("CGEventCreateMouseEvent 失败".into()))?;
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventClickState, count);
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventButtonNumber, i64::from(num));
        CGEvent::set_flags(Some(&ev), flags);
        tracing::debug!(
            ?button,
            pressed,
            ?ty,
            source_state = CGEvent::integer_value_field(Some(&ev), CGEventField::EventSourceStateID),
            source_pid = CGEvent::integer_value_field(Some(&ev), CGEventField::EventSourceUnixProcessID),
            user_data = CGEvent::integer_value_field(Some(&ev), CGEventField::EventSourceUserData),
            click_state = count,
            timestamp_ns = CGEvent::timestamp(Some(&ev)),
            x = p.x,
            y = p.y,
            tap = if tap == CGEventTapLocation::HIDEventTap {
                "HIDEventTap"
            } else {
                "SessionEventTap"
            },
            "button trace: posting synthetic button"
        );
        let receipt = self.motion.submit_cg(&ev, tap)?;
        st.accept_button(native, p, now, false);
        st.pending_position = (tap != CGEventTapLocation::SessionEventTap).then_some(PendingPosition {
            before,
            predecessor,
            at: p,
            epoch,
        });
        Ok(ButtonSubmission {
            acknowledgement: receipt.wait(),
        })
    }

    fn scroll(&self, st: &mut State, dx: f64, dy: f64, unit: ScrollUnit, phase: ScrollPhase) -> Result<(), InputError> {
        let (rx, ry) = st.scroll_rem;
        let (ix, rx) = split(rx + dx);
        let (iy, ry) = split(ry + dy);
        st.scroll_rem = (rx, ry);
        let cg_unit = match unit {
            ScrollUnit::Pixel => CGScrollEventUnit::Pixel,
            ScrollUnit::Line => CGScrollEventUnit::Line,
        };
        let ev = CGEvent::new_scroll_wheel_event2(self.src(), cg_unit, 2, iy, ix, 0)
            .ok_or_else(|| InputError::Platform("CGEventCreateScrollWheelEvent2 失败".into()))?;
        let (scroll_phase, momentum_phase) = cg_phases(phase);
        if scroll_phase != 0 || momentum_phase != 0 {
            CGEvent::set_integer_value_field(Some(&ev), CGEventField::ScrollWheelEventIsContinuous, 1);
            CGEvent::set_integer_value_field(Some(&ev), CGEventField::ScrollWheelEventScrollPhase, scroll_phase);
            CGEvent::set_integer_value_field(Some(&ev), CGEventField::ScrollWheelEventMomentumPhase, momentum_phase);
        }
        CGEvent::set_location(Some(&ev), self.next_location(st));
        CGEvent::set_flags(Some(&ev), self.flags(st));
        self.post(&ev)?;
        if matches!(
            phase,
            ScrollPhase::Ended | ScrollPhase::Cancelled | ScrollPhase::MomentumEnded
        ) {
            st.scroll_rem = (0.0, 0.0);
        }
        Ok(())
    }

    fn key(&self, st: &mut State, code: KeyCode, pressed: bool) -> Result<(), InputError> {
        if let Some(key) = keymap::hid_to_nx(code) {
            let repeat = if pressed {
                !st.down_aux.insert(key)
            } else {
                st.down_aux.remove(&key);
                false
            };
            return if sysevent::post_aux_key(AuxKey { key, pressed, repeat }) {
                Ok(())
            } else {
                Err(InputError::Platform("创建系统定义事件失败".into()))
            };
        }
        let mac = keymap::hid_to_mac(code)
            .ok_or_else(|| InputError::Platform("HID 没有对应的 mac 键码（不记录内容）".into()))?;
        if mac == keymap::MAC_CAPS_LOCK {
            // OS workaround: posted NX bypasses the HID filter that toggles a physical Caps key.
            // 2026-10-12, macOS 14.7.8, switch OFF: 100/700 ms NX presses left lock/AlphaShift OFF, letters lowercase.
            // CapsInjector keeps IOKit for ordinary lock mode and real-time native DOWN/UP for switch mode.
            self.caps.key(pressed);
            return Ok(());
        }
        let ev = CGEvent::new_keyboard_event(self.src(), mac, pressed)
            .ok_or_else(|| InputError::Platform("CGEventCreateKeyboardEvent 失败".into()))?;
        let mut flags;
        if modifier_flags(mac).is_some() {
            if pressed {
                st.mods.insert(mac);
            } else {
                st.mods.remove(&mac);
            }
            CGEvent::set_type(Some(&ev), CGEventType::FlagsChanged);
            flags = self.flags(st);
            if !pressed {
                // 抬起的键不再计入（本机键盘仍按着时由 hw 状态补回）。
                if let Some((mask, bit)) = modifier_flags(mac) {
                    let still = st
                        .mods
                        .iter()
                        .any(|m| modifier_flags(*m).is_some_and(|(mk, _)| mk == mask));
                    if !still {
                        flags = CGEventFlags(flags.0 & !mask.0);
                    }
                    flags = CGEventFlags(flags.0 & !bit);
                }
            }
        } else {
            flags = self.flags(st);
            if keymap::is_mac_keypad(mac) {
                flags = CGEventFlags(flags.0 | CGEventFlags::MaskNumericPad.0);
            }
            if keymap::is_mac_function(mac) {
                flags = CGEventFlags(flags.0 | CGEventFlags::MaskSecondaryFn.0);
            }
            if pressed {
                if !st.down.insert(mac) {
                    CGEvent::set_integer_value_field(Some(&ev), CGEventField::KeyboardEventAutorepeat, 1);
                }
            } else {
                st.down.remove(&mac);
            }
        }
        CGEvent::set_flags(Some(&ev), flags);
        if modifier_flags(mac).is_some() {
            tracing::info!(pressed, locked = ?super::session::locked(),
                ledger_modifiers = super::keyboard_diagnostics::modifiers(modifier_ledger_flags(&st.mods)),
                post_modifiers = super::keyboard_diagnostics::modifiers(flags.0),
                hid_modifiers_before_post = super::keyboard_diagnostics::modifiers(
                    CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState).0),
                "modifier injection submitted (no OS delivery acknowledgement)");
        }
        super::keyboard_diagnostics::posted_flags(flags.0, modifier_flags(mac).is_some());
        self.post(&ev)?;
        Ok(())
    }
}

/// Only public modifier semantics belong on a native button. Device-side bits,
/// NonCoalesced and reserved CG source bits are not keyboard modifiers.
fn semantic_button_flags(flags: CGEventFlags) -> CGEventFlags {
    CGEventFlags(
        flags.0
            & (CGEventFlags::MaskAlphaShift.0
                | CGEventFlags::MaskShift.0
                | CGEventFlags::MaskControl.0
                | CGEventFlags::MaskAlternate.0
                | CGEventFlags::MaskCommand.0
                | CGEventFlags::MaskSecondaryFn.0),
    )
}

fn modifier_ledger_flags(mods: &HashSet<MacKeyCode>) -> u64 {
    mods.iter()
        .filter_map(|m| modifier_flags(*m))
        .fold(0, |f, (mask, bit)| f | mask.0 | bit)
}

// Keep the existing behavior explicit for the lock-transition diagnostic tests. HIDSystemState
// mixes physical and injected state: a stale system modifier can outlive an empty ledger.
fn compose_flags(mods: &HashSet<MacKeyCode>, hid: u64, caps: bool) -> CGEventFlags {
    let keep = CGEventFlags::MaskShift.0
        | CGEventFlags::MaskControl.0
        | CGEventFlags::MaskAlternate.0
        | CGEventFlags::MaskCommand.0;
    CGEventFlags(
        modifier_ledger_flags(mods)
            | (hid & keep)
            | if caps { CGEventFlags::MaskAlphaShift.0 } else { 0 }
            | CGEventFlags::MaskNonCoalesced.0,
    )
}

/// 交给 WindowServer 的坐标：每个分量取不大于它的最大 float32。
///
/// WindowServer 以 float32 保存光标位置，`f64 → f32` 按最近舍入：屏幕的右、下边界不属于屏幕，
/// 贴着它们的坐标（路由裁剪到 `right - 1e-6`，如 1116.999999）会被舍入到边界上（1117.0），光标落到屏幕外。
/// 显示器的边界都是整数 point（float32 可以精确表示），向下取整后 `v < 边界` 仍然成立，左、上边界（含）
/// 同理不会越过。
pub(crate) fn quartz_point(p: Point) -> CGPoint {
    CGPoint {
        x: f32_floor(p.x),
        y: f32_floor(p.y),
    }
}

/// macOS 14 native capture misses the negative edge even with the corrected
/// catcher frame and a float32-safe cursor position. The explicit AppKit window
/// query truncates negative fractions toward zero; native drag lookup is opaque.
/// Submit the containing integer Quartz point, as the clipboard hit query
/// already does, rather than depending on fractional native destination lookup.
/// Ordinary motion's subpoint coordinates and routed/raw deltas stay unchanged.
/// Installed negative-edge acceptance is checked separately from geometry tests.
pub(crate) fn quartz_drag_point(p: Point) -> CGPoint {
    quartz_point(Point::new(p.x.floor(), p.y.floor()))
}

/// 不大于 `v` 的最大 float32（以 f64 表示）。非有限值原样返回。
fn f32_floor(v: f64) -> f64 {
    let f = v as f32;
    if f64::from(f) > v {
        f64::from(f.next_down())
    } else {
        f64::from(f)
    }
}

/// 当前光标位置（Quartz 全局坐标）。
pub(crate) fn checked_location() -> Result<Point, InputError> {
    let e = CGEvent::new(None).ok_or_else(|| InputError::Platform("cursor readback unavailable".into()))?;
    let p = CGEvent::location(Some(&e));
    if !p.x.is_finite() || !p.y.is_finite() {
        return Err(InputError::Platform("cursor readback invalid".into()));
    }
    Ok(Point::new(p.x, p.y))
}

pub(crate) fn current_location() -> CGPoint {
    CGEvent::new(None)
        .map(|e| CGEvent::location(Some(&e)))
        .unwrap_or(CGPoint { x: 0.0, y: 0.0 })
}

fn dist(a: CGPoint, b: CGPoint) -> f64 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// 位移累积到余数里，取出整数部分（四舍五入，余数保持在 ±0.5 以内）。非有限值当作 0。
fn take_delta(rem: &mut (f64, f64), raw: (f64, f64)) -> (i64, i64) {
    let add = |r: &mut f64, v: f64| {
        let v = if v.is_finite() { v } else { 0.0 };
        let total = *r + v;
        let whole = total.round();
        *r = total - whole;
        whole as i64
    };
    (add(&mut rem.0, raw.0), add(&mut rem.1, raw.1))
}

/// 取整数部分（向零）与余数。
fn split(v: f64) -> (i32, f64) {
    let i = v.trunc();
    (i as i32, v - i)
}

pub(crate) fn button_number(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
        MouseButton::Other(n) => u32::from(n),
    }
}

pub(crate) fn button_from_number(n: i64) -> MouseButton {
    match n {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        n => MouseButton::Other(n.clamp(0, 255) as u8),
    }
}

pub(super) fn image_nudge_event(
    source: Option<&CGEventSource>,
    at: Point,
    button: MouseButton,
    flags: CGEventFlags,
) -> Result<CFRetained<CGEvent>, InputError> {
    let number = button_number(button);
    if number >= 32 {
        return Err(InputError::Unsupported);
    }
    let (ty, cg_button) = match number {
        0 => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
        1 => (CGEventType::RightMouseDragged, CGMouseButton::Right),
        _ => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
    };
    // This refresh is stationary, including for a physical source whose native
    // anchor has subpoint precision. Only positioned drag motion is normalized;
    // do not move the anchor or its frozen-cursor ledger to refresh an image.
    let event = marked_mouse_event(source, ty, quartz_point(at), cg_button)?;
    CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventButtonNumber, i64::from(number));
    CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventDeltaX, 0);
    CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventDeltaY, 0);
    CGEvent::set_flags(Some(&event), flags);
    Ok(event)
}

fn motion_event(
    source: Option<&CGEventSource>,
    ty: CGEventType,
    at: CGPoint,
    button: CGMouseButton,
) -> Result<objc2_core_foundation::CFRetained<CGEvent>, InputError> {
    let at = if matches!(
        ty,
        CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged | CGEventType::OtherMouseDragged
    ) {
        quartz_drag_point(Point::new(at.x, at.y))
    } else {
        at
    };
    marked_mouse_event(source, ty, at, button)
}

fn marked_mouse_event(
    source: Option<&CGEventSource>,
    ty: CGEventType,
    at: CGPoint,
    button: CGMouseButton,
) -> Result<objc2_core_foundation::CFRetained<CGEvent>, InputError> {
    let event = CGEvent::new_mouse_event(source, ty, at, button)
        .ok_or_else(|| InputError::Platform("CGEventCreateMouseEvent 失败".into()))?;
    // Explicit even when the HID source already supplies it. The frozen tap's
    // own-event bypass and absolute-placement accounting require this marker.
    CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUserData, MAGIC);
    Ok(event)
}

/// 按下的按钮 → 移动事件的类型（左键优先，其次右键，其次编号最小的其他键）。
fn drag_type(buttons: u32) -> (CGEventType, CGMouseButton, u32) {
    if buttons & 1 != 0 {
        (CGEventType::LeftMouseDragged, CGMouseButton::Left, 0)
    } else if buttons & 2 != 0 {
        (CGEventType::RightMouseDragged, CGMouseButton::Right, 1)
    } else if buttons != 0 {
        (
            CGEventType::OtherMouseDragged,
            CGMouseButton::Center,
            buttons.trailing_zeros(),
        )
    } else {
        (CGEventType::MouseMoved, CGMouseButton::Left, 0)
    }
}

/// 线上阶段 → (kCGScrollWheelEventScrollPhase, kCGScrollWheelEventMomentumPhase)。
pub(crate) fn cg_phases(p: ScrollPhase) -> (i64, i64) {
    match p {
        ScrollPhase::None => (0, 0),
        ScrollPhase::Began => (1, 0),
        ScrollPhase::Changed => (2, 0),
        ScrollPhase::Ended => (4, 0),
        ScrollPhase::Cancelled => (8, 0),
        ScrollPhase::MomentumBegan => (0, 1),
        ScrollPhase::Momentum => (0, 2),
        ScrollPhase::MomentumEnded => (0, 3),
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    #[test]
    fn source_freeze_waits_for_exact_posted_endpoint_or_a_newer_physical_epoch() {
        use super::*;
        let pending = PendingPosition {
            before: CGPoint::new(100., 4.),
            predecessor: CGPoint::new(100., 2.),
            at: CGPoint::new(100., 0.),
            epoch: 7,
        };
        assert!(!pending.committed(pending.before, 7));
        assert!(!pending.committed(pending.predecessor, 7));
        assert!(!pending.committed(CGPoint::new(101., 1.), 7));
        assert!(pending.committed(pending.at, 7));
        assert!(pending.committed(CGPoint::new(101., 1.), 8));
    }

    #[test]
    fn source_hold_fences_delayed_injection_through_return_without_blocking_fresh_motion() {
        // Admission is sampled before waiting for the injector ledger. A move
        // already waiting when freeze commits must not cross either boundary.
        let before_hold = 0;
        let after_freeze = 1;
        let after_return = 2;
        assert!(!super::source_motion_admitted(Some(42), before_hold, after_freeze));
        assert!(!super::source_motion_admitted(Some(42), after_freeze, after_freeze));
        assert!(!super::source_motion_admitted(None, before_hold, after_return));
        assert!(!super::source_motion_admitted(None, after_freeze, after_return));
        assert!(super::source_motion_admitted(None, after_return, after_return));
    }

    use super::*;

    #[test]
    fn stopped_session_discards_native_position_and_fractional_input_state() {
        let injector = Injector::with_activity(UserActivity::with_declaration(|_| Ok(())).unwrap()).unwrap();
        {
            let mut state = injector.state.lock().unwrap();
            state.buttons = 1;
            state.last_pos = Some(CGPoint::new(40., 50.));
            state.failed_position_context = Some(5);
            state.delta_rem = (0.75, -0.5);
            state.scroll_rem = (0.25, 0.5);
        }
        injector.stop_session();
        let state = injector.state.lock().unwrap();
        assert_eq!(state.buttons, 0);
        assert!(state.last_pos.is_none());
        assert!(state.pending_position.is_none());
        assert!(state.failed_position_context.is_none());
        assert_eq!(state.delta_rem, (0., 0.));
        assert_eq!(state.scroll_rem, (0., 0.));
    }

    #[test]
    fn image_nudge_is_one_marked_zero_delta_drag_for_the_source_button() {
        let at = Point::new(1727.999999, 1116.999999);
        let flags = CGEventFlags(CGEventFlags::MaskCommand.0 | device_bits::LCMD | CGEventFlags::MaskShift.0);
        for (button, ty, number) in [
            (MouseButton::Left, CGEventType::LeftMouseDragged, 0),
            (MouseButton::Right, CGEventType::RightMouseDragged, 1),
            (MouseButton::Middle, CGEventType::OtherMouseDragged, 2),
            (MouseButton::Other(7), CGEventType::OtherMouseDragged, 7),
            (MouseButton::Other(31), CGEventType::OtherMouseDragged, 31),
        ] {
            let event = image_nudge_event(None, at, button, flags).unwrap();
            assert_eq!(CGEvent::r#type(Some(&event)), ty);
            assert_eq!(CGEvent::location(Some(&event)), quartz_point(at));
            assert_eq!(CGEvent::flags(Some(&event)), flags);
            for (field, expected) in [
                (CGEventField::EventSourceUserData, MAGIC),
                (CGEventField::MouseEventDeltaX, 0),
                (CGEventField::MouseEventDeltaY, 0),
                (CGEventField::MouseEventButtonNumber, number),
            ] {
                assert_eq!(CGEvent::integer_value_field(Some(&event), field), expected);
            }
        }
        assert!(matches!(
            image_nudge_event(None, at, MouseButton::Other(255), flags),
            Err(InputError::Unsupported)
        ));
    }

    #[test]
    fn image_nudge_submission_does_not_move_or_consume_the_injection_ledger() {
        let injector = Injector::with_activity(UserActivity::with_declaration(|_| Ok(())).unwrap()).unwrap();
        let at = CGPoint { x: 12., y: 34. };
        {
            let mut st = injector.state.lock().unwrap();
            st.last_pos = Some(CGPoint { x: -20., y: -30. });
            st.delta_rem = (0.75, -0.5);
            st.buttons = 1; // remote route holds left; source drag belongs to right
            st.mods.insert(0x37);
            st.pending_position = Some(PendingPosition {
                before: at,
                predecessor: at,
                at,
                epoch: 2,
            });
        }
        let mut count = 0;
        let result: Result<(), InputError> =
            injector.with_image_nudge(Point::new(at.x, at.y), MouseButton::Right, |event, _state| {
                count += 1;
                assert_eq!(CGEvent::r#type(Some(event)), CGEventType::RightMouseDragged);
                assert_eq!(
                    CGEvent::integer_value_field(Some(event), CGEventField::MouseEventDeltaX),
                    0
                );
                assert_eq!(
                    CGEvent::integer_value_field(Some(event), CGEventField::MouseEventDeltaY),
                    0
                );
                assert!(CGEvent::flags(Some(event)).contains(CGEventFlags::MaskCommand));
                Err(InputError::Platform("fake lost ACK".into()))
            });
        assert!(result.is_err());
        assert_eq!(count, 1);
        let st = injector.state.lock().unwrap();
        assert_eq!(st.last_pos, Some(CGPoint { x: -20., y: -30. }));
        assert_eq!(st.delta_rem, (0.75, -0.5));
        assert_eq!(st.buttons, 1);
        assert_eq!(st.mods, HashSet::from([0x37]));
        assert_eq!(st.pending_position.unwrap().epoch, 2);
        assert!(st.clicks.is_empty());
        assert!(st.down.is_empty());
    }

    #[test]
    fn cancel_dragged_event_has_marker_and_edge_displacement_is_folded_once() {
        let at = quartz_point(Point::new(1.0, 558.5));
        let (ty, button, _) = drag_type(1);
        let event = motion_event(None, ty, at, button).unwrap();
        // Construct/inspect only; this test never posts input to the desktop.
        assert_eq!(CGEvent::r#type(Some(&event)), CGEventType::LeftMouseDragged);
        assert_eq!(
            CGEvent::integer_value_field(Some(&event), CGEventField::EventSourceUserData),
            MAGIC
        );
        let delivered = CGEvent::location(Some(&event));
        assert_eq!(delivered, quartz_drag_point(Point::new(at.x, at.y)));
        let mut ledger = crate::warp_delta::WarpDeltaFix::new();
        ledger.motion((0., 0.), Point::new(720., 596.), 10, true);
        ledger.enter_remote();
        // Same registration used by the tap's MAGIC branch, with the delivered
        // coordinate (not the unquantized requested coordinate).
        ledger.posted_absolute_motion(Point::new(delivered.x, delivered.y), 20);
        assert_eq!(ledger.motion((-719., -37.), Point::new(1., 559.), 30, true), (0., 1.));
        assert_eq!(ledger.motion((0., 2.), Point::new(1., 561.), 40, true), (0., 2.));
        assert_eq!(ledger.take_leak(), None);
    }

    #[test]
    fn held_drag_restart_selects_a_cg_pair_without_clearing_physical_hold() {
        let mut state = State::default();
        let physical = super::super::physical_buttons::PhysicalButtons::default();
        physical.record(0, true);
        assert_eq!(state.native_number(0, true, true), None);
        let down = NativeButton {
            number: 0,
            pressed: true,
            count: 1,
            event_number: 0,
            flags: CGEventFlags(0),
        };
        let at = CGPoint { x: 20., y: 30. };
        state.accept_button(down, at, Instant::now(), false);
        assert_eq!(drag_type(state.buttons).0, CGEventType::LeftMouseDragged);
        assert_eq!(state.native_number(0, false, false), None);
        assert!(physical.is_down(0));
        assert_eq!(
            physical.tap(0, false, CGEventTapLocation::HIDEventTap),
            CGEventTapLocation::SessionEventTap
        );
        state.accept_button(NativeButton { pressed: false, ..down }, at, Instant::now(), false);
        assert_eq!(state.buttons, 0);
        assert!(physical.is_down(0));
        physical.record(0, false);
        // A later drag from another device, and ordinary clicks, stay native.
        assert_eq!(state.native_number(0, true, false), Some(1));
    }

    #[test]
    fn accepted_drag_press_ack_timeout_is_not_retried_and_keeps_cleanup_pair() {
        let mut state = State::default();
        let at = CGPoint { x: 20., y: 30. };
        let down = NativeButton {
            number: 0,
            pressed: true,
            count: 1,
            event_number: 1,
            flags: CGEventFlags(0),
        };
        let mut posts = 0;
        // Model the clipboard retry decision, not OS delivery or AppKit start.
        for _ in 0..3 {
            posts += 1;
            state.accept_button(down, at, Instant::now(), true);
            let submission = ButtonSubmission {
                acknowledgement: Err(InputError::Platform("tap timeout".into())),
            };
            if state.finish_drag_press(7, Ok(submission)).is_ok() {
                break;
            }
        }
        assert_eq!(posts, 1);
        assert_eq!(state.native_number(0, false, false), Some(1));
        assert!(!state.position_ready(7), "accepted is not an acknowledged position");
        // AppKit did not begin: the caller submits exactly the paired release.
        state.accept_button(NativeButton { pressed: false, ..down }, at, Instant::now(), true);
        assert_eq!(state.buttons, 0);
        assert_eq!(state.native_number(0, false, false), None);
        assert!(
            ButtonSubmission {
                acknowledgement: Err(InputError::Platform("tap timeout".into()))
            }
            .acknowledged()
            .is_err(),
            "ordinary injection must retain its ACK failure result"
        );
    }

    #[test]
    fn unsubmitted_drag_press_remains_retryable_without_a_button_to_release() {
        let mut state = State::default();
        assert!(
            state
                .finish_drag_press(7, Err(InputError::Platform("submit failed".into())))
                .is_err()
        );
        assert_eq!(state.buttons, 0);
        assert!(state.clicks.is_empty());
        assert_eq!(state.native_number(0, false, false), None);
        assert!(!state.position_ready(7));
        assert!(
            state
                .finish_drag_press(
                    7,
                    Ok(ButtonSubmission {
                        acknowledgement: Ok(())
                    })
                )
                .is_ok()
        );
        assert!(state.position_ready(7));
    }

    #[test]
    fn relative_input_abandons_failed_absolute_target_without_changing_delta_ledger() {
        let mut state = State::default();
        let at = CGPoint { x: 20., y: 30. };
        state.last_pos = Some(at);
        state.delta_rem = (0.25, -0.25);
        state.position_result(7, false);
        assert!(!state.position_ready(7));
        state.enter_relative_input();
        assert!(state.position_ready(7));
        assert!(state.pending_position.is_none());
        assert_eq!(state.last_pos, Some(at));
        assert_eq!(state.delta_rem, (0.25, -0.25));
    }

    #[test]
    fn click_count_and_native_pair_commit_only_after_submission() {
        let mut state = State::default();
        let now = Instant::now();
        let at = CGPoint { x: 20., y: 30. };
        // Preparing a rejected submission must not change multi-click/drag state.
        for _ in 0..2 {
            assert_eq!(state.click_count(0, true, at, now, 0.5), 1);
            assert_eq!(state.native_number(0, true, false), Some(1));
        }
        assert_eq!(state.buttons, 0);
        assert!(state.clicks.is_empty());
        for count in 1..=3 {
            assert_eq!(state.click_count(0, true, at, now, 0.5), count);
            let down = NativeButton {
                number: 0,
                pressed: true,
                count: count as i32,
                event_number: count as i16,
                flags: CGEventFlags(0),
            };
            state.accept_button(down, at, now, true);
            // ACK failure does not undo an accepted DOWN; the next move is a drag
            // and the eventual UP closes the same native pair.
            assert_eq!(drag_type(state.buttons).0, CGEventType::LeftMouseDragged);
            assert_eq!(state.native_number(0, false, false), Some(count as i16));
            assert_eq!(state.click_count(0, false, at, now, 0.5), count);
            state.accept_button(NativeButton { pressed: false, ..down }, at, now, true);
            assert_eq!(state.buttons, 0);
            assert_eq!(state.native_number(0, false, false), None);
        }
        assert_eq!(state.click_count(1, true, at, now, 0.5), 1);
        assert_eq!(state.click_count(0, true, CGPoint { x: 25., ..at }, now, 0.5), 1);
        assert_eq!(
            state.click_count(0, true, at, now + std::time::Duration::from_secs(1), 0.5),
            1
        );
        let cg = NativeButton {
            number: 1,
            pressed: true,
            count: 1,
            event_number: 4,
            flags: CGEventFlags(0),
        };
        state.accept_button(cg, at, now, false);
        assert_eq!(state.native_number(1, false, false), None); // CG DOWN retains a CG UP
        assert_eq!(state.last_native_number, 3);
        state.last_native_number = i16::MAX;
        assert_eq!(state.native_number(2, true, false), Some(1));
        assert_eq!(state.native_number(32, true, false), None);
        state.clicks.insert(0, (now, at, i64::from(i32::MAX)));
        assert_eq!(state.click_count(0, true, at, now, 0.5), i64::from(i32::MAX));
    }

    #[test]
    fn failed_absolute_target_requires_success_or_a_new_context() {
        let motion = super::super::cursor_reveal::Motion::default();
        let mut state = State::default();
        let at = CGPoint { x: 10., y: 20. };
        state.pending_position = Some(PendingPosition {
            before: at,
            predecessor: at,
            at,
            epoch: 0,
        });
        state.position_result(motion.position_context(), false);
        assert!(state.pending_position.is_none());
        assert!(!state.position_ready(motion.position_context()));
        motion.invalidate_position(); // physical motion does not deliver a failed target
        assert!(!state.position_ready(motion.position_context()));
        state.position_result(motion.position_context(), true);
        assert!(state.position_ready(motion.position_context()));
        state.position_result(motion.position_context(), false);
        motion.reset_position_context(); // mode/capture transition abandons old target
        assert!(state.position_ready(motion.position_context()));
        assert!(state.failed_position_context.is_none());
    }

    #[test]
    fn native_button_flags_keep_public_modifier_semantics_only() {
        let semantic = CGEventFlags::MaskAlphaShift.0
            | CGEventFlags::MaskShift.0
            | CGEventFlags::MaskControl.0
            | CGEventFlags::MaskAlternate.0
            | CGEventFlags::MaskCommand.0
            | CGEventFlags::MaskSecondaryFn.0;
        assert_eq!(semantic_button_flags(CGEventFlags(u64::MAX)).0, semantic);
        assert_eq!(semantic_button_flags(CGEventFlags(0)).0, 0);
        for bit in [
            CGEventFlags::MaskShift,
            CGEventFlags::MaskControl,
            CGEventFlags::MaskAlternate,
            CGEventFlags::MaskCommand,
        ] {
            assert_eq!(
                semantic_button_flags(CGEventFlags(bit.0 | device_bits::LSHIFT | 0x2000_0100)),
                bit
            );
        }
    }

    #[test]
    fn held_source_up_does_not_reintroduce_forwarded_modifiers() {
        // HID may still contain Ctrl after capture forwards its FlagsChanged;
        // injected modifiers can also remain while another device drives this
        // source. Neither is the Copy destination's keyboard state.
        let injected = HashSet::from([0x3A, 0x37]); // Option and Command
        let flags = compose_flags(&injected, CGEventFlags::MaskControl.0, true);
        assert!(flags.contains(CGEventFlags::MaskControl));
        assert!(flags.contains(CGEventFlags::MaskAlternate));
        assert!(flags.contains(CGEventFlags::MaskCommand));
        for held in [false, true] {
            for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
                for pressed in [false, true] {
                    let actual = ButtonModifiers::for_button(held, button, pressed).resolve(flags);
                    if held && button == MouseButton::Left && !pressed {
                        assert_eq!(actual, CGEventFlags::empty());
                    } else {
                        assert_eq!(actual, flags, "ordinary/return events retain their modifiers");
                    }
                }
            }
        }
        // A dedicated source-release command stays neutral after a lost hold;
        // neither route releases keys or mutates the injected key ledger.
        assert_eq!(ButtonModifiers::SourceRelease.resolve(flags), CGEventFlags::empty());
        assert_eq!(injected, HashSet::from([0x3A, 0x37]));
    }

    #[test]
    fn pending_position_only_bridges_the_uncommitted_point() {
        let before = CGPoint { x: 10., y: 20. };
        let at = CGPoint { x: 30.25, y: 40.5 };
        let pending = || PendingPosition {
            before,
            predecessor: before,
            at,
            epoch: 7,
        };
        assert_eq!(pending().resolve(before, 7), at);
        assert_eq!(pending().resolve(at, 7), at);
        assert_eq!(pending().resolve(before, 8), before);
        let warped = CGPoint { x: 99., y: 100. };
        assert_eq!(pending().resolve(warped, 7), warped);
        let next = CGPoint { x: 50., y: 60. };
        let chained = || PendingPosition {
            before,
            predecessor: pending().resolve(before, 7),
            at: next,
            epoch: 7,
        };
        assert_eq!(chained().resolve(before, 7), next);
        assert_eq!(chained().resolve(at, 7), next);
        assert_eq!(chained().resolve(warped, 7), warped);
    }

    #[test]
    fn split_keeps_remainder() {
        assert_eq!(split(2.75), (2, 0.75));
        let (i, r) = split(-1.5);
        assert_eq!(i, -1);
        assert!((r + 0.5).abs() < 1e-9);
    }

    #[test]
    fn take_delta_accumulates_fractions() {
        let mut rem = (0.0, 0.0);
        // 每次 0.4 pt：第 2 次累计 0.8 → 发出 1，余 -0.2
        assert_eq!(take_delta(&mut rem, (0.4, -0.4)), (0, 0));
        assert_eq!(take_delta(&mut rem, (0.4, -0.4)), (1, -1));
        let (a, b) = rem;
        assert!((a + 0.2).abs() < 1e-9 && (b - 0.2).abs() < 1e-9);
        // 总和守恒
        let mut rem = (0.0, 0.0);
        let sum: i64 = (0..100).map(|_| take_delta(&mut rem, (0.3, 0.0)).0).sum();
        assert_eq!(sum, 30);
        // 顶在墙上的推动：位置不变，位移照样发出
        let mut rem = (0.0, 0.0);
        assert_eq!(take_delta(&mut rem, (0.0, 3.2)), (0, 3));
        assert_eq!(take_delta(&mut rem, (f64::NAN, 0.0)).0, 0);
    }

    #[test]
    fn drag_event_endpoint_stays_inside_its_display_after_native_integer_lookup() {
        // AppKit's negative-edge lookup must see the containing point, even
        // when routing parks just inside an exclusive right/bottom boundary.
        for (bounds, at) in [
            (
                oink_foundation::Rect::new(1600., -1080., 1920., 1080.),
                Point::new(2500.25, -0.000_005),
            ),
            (
                oink_foundation::Rect::new(-1920., 0., 1920., 1080.),
                Point::new(-0.000_005, 600.75),
            ),
            (
                oink_foundation::Rect::new(-1920., -1080., 1920., 1080.),
                Point::new(-0.000_005, -0.000_005),
            ),
            (
                oink_foundation::Rect::new(0., 0., 1728., 1117.),
                Point::new(1_727.999_999, 1_116.999_999),
            ),
        ] {
            assert!(bounds.contains(at));
            for ty in [
                CGEventType::LeftMouseDragged,
                CGEventType::RightMouseDragged,
                CGEventType::OtherMouseDragged,
            ] {
                let event = motion_event(None, ty, quartz_point(at), CGMouseButton::Left).unwrap();
                let posted = CGEvent::location(Some(&event));
                let lookup = Point::new(posted.x.trunc(), posted.y.trunc());
                assert!(
                    bounds.contains(lookup),
                    "{at:?} posted as {posted:?}, native lookup {lookup:?}"
                );
            }
        }
        // Ordinary pointer motion retains its existing subpoint precision.
        let at = CGPoint::new(-12.25, -0.25);
        let event = motion_event(None, CGEventType::MouseMoved, at, CGMouseButton::Left).unwrap();
        assert_eq!(CGEvent::location(Some(&event)), at);
    }

    #[test]
    fn quartz_point_never_rounds_onto_an_exclusive_edge() {
        // 路由把被墙挡住的指针放在 bottom - 1e-6：最近舍入到 float32 会变成 1117.0（屏幕外）。
        let p = quartz_point(Point::new(1727.999999, 1116.999999));
        assert!(p.x < 1728.0 && p.y < 1117.0);
        assert_eq!(
            f64::from(p.y as f32),
            p.y,
            "结果本身就是 float32，WindowServer 不会再舍入"
        );
        assert!(1117.0 - p.y < 1e-3, "仍在最后一行：{}", p.y);
        // 已经是 float32 的值不变；左、上边界（含）不会越过。
        for v in [0.0, 1116.0, 558.5, -1728.0, 2160.0] {
            assert_eq!(f32_floor(v), v);
        }
        assert!(f32_floor(-1727.999999) >= -1728.0);
        assert!(f32_floor(1e-9) >= 0.0);
        // 负坐标（主屏左边、上边的显示器）：同样不大于原值。
        let v = -0.000_000_1;
        assert!(f32_floor(v) <= v && f32_floor(v) > -1e-6);
        assert!(f32_floor(f64::NAN).is_nan());
    }

    #[test]
    fn drag_type_prefers_left() {
        assert_eq!(drag_type(0).0, CGEventType::MouseMoved);
        assert_eq!(drag_type(0b11).0, CGEventType::LeftMouseDragged);
        assert_eq!(drag_type(0b10).0, CGEventType::RightMouseDragged);
        let (t, _, n) = drag_type(1 << 3);
        assert_eq!((t, n), (CGEventType::OtherMouseDragged, 3));
    }

    #[test]
    fn button_numbers_round_trip() {
        for b in [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Back,
            MouseButton::Forward,
            MouseButton::Other(7),
        ] {
            assert_eq!(button_from_number(i64::from(button_number(b))), b);
        }
    }

    #[test]
    fn phases_map_to_cg_fields() {
        assert_eq!(cg_phases(ScrollPhase::Began), (1, 0));
        assert_eq!(cg_phases(ScrollPhase::Ended), (4, 0));
        assert_eq!(cg_phases(ScrollPhase::Momentum), (0, 2));
    }
}

#[cfg(all(test, debug_assertions))]
mod session_transition_tests {
    use super::*;

    #[test]
    fn lock_shortcut_releases_ledger_but_stale_hid_can_reintroduce_modifiers() {
        let mut st = State::default();
        // Ctrl + Cmd + Q; no native events or system session changes in this simulation.
        st.mods.insert(0x3b);
        st.mods.insert(0x37);
        st.down.insert(0x0c);
        let locked_flags = CGEventFlags::MaskControl.0 | CGEventFlags::MaskCommand.0;
        assert_eq!(
            super::super::keyboard_diagnostics::modifiers(compose_flags(&st.mods, 0, false).0),
            locked_flags
        );
        // Q, Cmd, Ctrl releases received across the session transition.
        st.down.remove(&0x0c);
        st.mods.remove(&0x37);
        st.mods.remove(&0x3b);
        assert!(st.down.is_empty() && st.mods.is_empty());
        assert_eq!(
            super::super::keyboard_diagnostics::modifiers(compose_flags(&st.mods, 0, false).0),
            0
        );
        // This documents the candidate failure, not evidence that macOS actually drops those ups.
        assert_eq!(
            super::super::keyboard_diagnostics::modifiers(compose_flags(&st.mods, locked_flags, false).0),
            locked_flags
        );
    }
}

#[cfg(all(test, debug_assertions))]
mod ordinary_work_regressions {
    use super::*;
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    // Unsupported input reaches the exact production entry/locking code but
    // posts no native event, requests no permissions, and calls no real powerd IPC.
    fn unsupported() -> InjectEvent {
        InjectEvent::Volume {
            code: KeyCode::keyboard(4),
            pressed: false,
            repeat: false,
            modifiers: 0,
        }
    }
    #[test]
    fn blocked_activity_ipc_and_ordinary_lock_do_not_delay_injection() {
        let (entered, ready) = mpsc::channel();
        let (release, proceed) = mpsc::channel();
        let proceed = Mutex::new(proceed);
        let ordinary = Arc::new(Mutex::new(()));
        let held = ordinary.clone();
        let first = std::sync::atomic::AtomicBool::new(true);
        let activity = UserActivity::with_declaration(move |_| {
            if first.swap(false, std::sync::atomic::Ordering::Relaxed) {
                let _ordinary = held.lock().unwrap();
                entered.send(()).unwrap();
                proceed.lock().unwrap().recv().unwrap();
            }
            Ok(())
        })
        .unwrap();
        let injector = Arc::new(Injector::with_activity(activity).unwrap());
        injector.activity.request();
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(ordinary.try_lock().is_err(), "fake ordinary IPC is holding its lock");
        let native = injector.clone();
        let (done, completed) = mpsc::channel();
        let input = std::thread::spawn(move || done.send(native.inject(&unsupported())).unwrap());
        // The timeout is a deadlock watchdog, not a latency assertion. Success
        // requires completion while the worker's release gate is still closed.
        let result = completed.recv_timeout(Duration::from_secs(3));
        release.send(()).unwrap();
        input.join().unwrap();
        assert!(
            matches!(result, Ok(Err(InputError::Unsupported))),
            "injection waited for the ordinary declaration lock: {result:?}"
        );
    }
    #[test]
    fn injection_still_requests_an_ordinary_activity_declaration() {
        let (called, observed) = mpsc::channel();
        let caller = std::thread::current().id();
        let activity = UserActivity::with_declaration(move |_| {
            called.send(std::thread::current().id()).unwrap();
            Ok(())
        })
        .unwrap();
        let injector = Injector::with_activity(activity).unwrap();
        assert!(matches!(injector.inject(&unsupported()), Err(InputError::Unsupported)));
        let worker = observed.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_ne!(worker, caller, "inject performed the activity IPC itself");
    }
    #[test]
    fn blocked_visibility_query_does_not_hold_injector_state_or_authorize_stale_capture() {
        let injector = Arc::new(Injector::with_activity(UserActivity::with_declaration(|_| Ok(())).unwrap()).unwrap());
        let sample = crate::pointer_capture::CaptureSample {
            hidden: true,
            foreground: 1,
            windows: None,
            application_hidden: None,
            confined: false,
            position: Point::default(),
            motion_serial: 0,
            motion_observable: true,
        };
        let visible = crate::pointer_capture::CaptureSample {
            hidden: false,
            ..sample
        };
        assert_eq!(
            injector.observe_visibility(visible, true, false, 3, || panic!("visible samples do not refresh")),
            Some(visible)
        );
        let native = injector.clone();
        let (entered, ready) = mpsc::channel();
        let (release, proceed) = mpsc::channel();
        let observer = std::thread::spawn(move || {
            native.observe_visibility(sample, true, true, 3, || {
                entered.send(()).unwrap();
                proceed.recv().unwrap();
                Some(sample)
            })
        });
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        // State acquisition is deterministic: the query is parked at an explicit
        // barrier. No native input or wall-clock speed is used as evidence.
        let state_free = injector.state.try_lock().is_ok();
        injector.capture_refresh.key(); // invalidate the delayed ordinary reread
        release.send(()).unwrap();
        let refreshed = observer.join().unwrap();
        assert!(state_free, "ordinary visibility query held the injection mutex");
        assert!(refreshed.is_none(), "stale refresh entered automatic capture");
    }
}
