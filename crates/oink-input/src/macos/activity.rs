//! 本机物理输入和远端操作都算作本机的用户活动：点亮熄灭的屏幕、推迟熄屏。
//!
//! 注入远端事件时调用 `IOPMAssertionDeclareUserActivity(kIOPMUserActiveLocal)`（`caffeinate -u` 用的接口），
//! 最多每秒一次（熄屏时间以分钟计，更密没有意义；这是一次到 powerd 的 IPC）。
//!
//! 为什么不只靠注入的事件本身：`CGEventPost(kCGHIDEventTap)` 在屏幕亮着时确实会重置 `HIDIdleTime`、刷新
//! WindowServer 的 `UserIsActive` 断言（macOS 26 实测），但用户
//! 报告被控 Mac 熄屏后远端移动鼠标屏幕不亮；声明用户活动是系统文档化的点亮屏幕方式（`caffeinate(8)` -u：
//! 「If the display is off, this option turns the display on」）。
//!
//! 返回的断言 ID 在下次调用时传回（系统据此合并），[`UserActivity`] 释放时一并释放。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::InputError;
use objc2_core_foundation::CFString;
use objc2_core_graphics::{CGEvent, CGEventField, CGEventSource, CGEventTapLocation, CGEventType};

/// 两次声明之间的最短间隔。
pub(crate) const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// `kIOPMUserActiveLocal`：本机用户活动（点亮屏幕、推迟熄屏）。
const USER_ACTIVE_LOCAL: u32 = 0;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionDeclareUserActivity(name: *const c_void, user_type: u32, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

/// 距上次声明够久了（或还没声明过）。
pub(crate) fn due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|t| now.saturating_duration_since(t) >= MIN_INTERVAL)
}

#[derive(Default)]
struct State {
    last: Option<Instant>,
    last_idle: Option<Instant>,
    /// 上次返回的断言 ID（0 = 没有）。
    id: u32,
}

type Declaration = dyn Fn(&mut u32) -> Result<(), InputError> + Send + Sync;
struct OrdinaryActivity {
    state: Mutex<State>,
    /// 只报告第一次失败（只检测，不改变行为）。
    warned: AtomicBool,
    declare: Box<Declaration>,
}

pub(crate) struct UserActivity {
    ordinary: Arc<OrdinaryActivity>,
    deferred: crate::deferred_activity::DeferredActivity,
}
impl UserActivity {
    pub fn new() -> Result<Self, InputError> {
        Self::with_declaration(|id| {
            let name = CFString::from_static_str("Oink user input");
            // SAFETY: valid CFString and writable assertion ID for this call.
            let r =
                unsafe { IOPMAssertionDeclareUserActivity((&*name as *const CFString).cast(), USER_ACTIVE_LOCAL, id) };
            if r == 0 {
                Ok(())
            } else {
                Err(InputError::Platform(format!(
                    "IOPMAssertionDeclareUserActivity: {r:#x}"
                )))
            }
        })
    }
    pub(super) fn with_declaration(
        declare: impl Fn(&mut u32) -> Result<(), InputError> + Send + Sync + 'static,
    ) -> Result<Self, InputError> {
        let ordinary = Arc::new(OrdinaryActivity {
            state: Mutex::new(State::default()),
            warned: AtomicBool::new(false),
            declare: Box::new(declare),
        });
        let worker = ordinary.clone();
        let deferred = crate::deferred_activity::DeferredActivity::new(move || {
            let _ = worker.declare();
        })
        .map_err(|e| InputError::Platform(format!("user activity worker: {e}")))?;
        Ok(Self { ordinary, deferred })
    }
    /// Native input only records an occurrence. powerd and HID-idle work can
    /// block under load, so they must never share a lock with input delivery.
    pub fn request(&self) {
        self.deferred.request();
    }
    pub fn declare(&self) -> Result<bool, InputError> {
        self.ordinary.declare()
    }
    pub fn reset_idle(&self, source: &CGEventSource) -> Result<bool, InputError> {
        self.ordinary.reset_idle(source)
    }
}

impl OrdinaryActivity {
    /// Declare first to wake the display, then refresh HID idle with a Null event.
    /// Unlike MouseMoved, Null has no pointer/hover/drag semantics. A display-sleep
    /// assertion alone does not refresh the HID idle timer.
    pub(crate) fn reset_idle(&self, source: &CGEventSource) -> Result<bool, InputError> {
        self.declare()?;
        let now = Instant::now();
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !due(st.last_idle, now) || !super::session::idle_refresh_allowed() {
            return Ok(false);
        }
        let event = CGEvent::new(Some(source))
            .ok_or_else(|| InputError::Platform("CGEventCreate idle activity failed".into()))?;
        CGEvent::set_type(Some(&event), CGEventType::Null);
        CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUserData, super::inject::MAGIC);
        // DeclareUserActivity 点亮屏幕，但不重置 HIDIdleTime，MDM 屏保仍会按 HID 空闲到点触发。
        // 因此声明后在 HID 层补一个 Null：不移动光标，也没有 hover/按键/按钮语义；来源 PID
        // 和 MAGIC 使它不算物理输入、不回流成组内活动。上面的会话检查禁止在锁屏/屏保期间投递。
        // 不持续持有 PreventUserIdleDisplaySleep：它只压住触发，HID 空闲仍累积；托管 Mac 实测
        // 释放后约 12 秒便进入屏保。按真实活动补 Null 后，停止操作就从最后一次活动自然计时。
        // macOS 26 探针验证 HID 重置、坐标/隐藏光标不变；macOS 14.7.8 已签名服务验证 HID 重置。
        // The observed lock-screen behavior is not guaranteed across all OS versions.
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        st.last_idle = Some(now);
        tracing::debug!("posted group idle activity (tagged HID Null event)");
        Ok(true)
    }

    /// 物理输入、组内通知或远端事件注入时调用：共享限流与断言。
    pub(crate) fn declare(&self) -> Result<bool, InputError> {
        self.declare_at(Instant::now())
    }
    fn declare_at(&self, now: Instant) -> Result<bool, InputError> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !due(st.last, now) {
            return Ok(false);
        }
        st.last = Some(now);
        let mut id = st.id;
        tracing::debug!(target: "oink_cursor_timing", "cursor latency: activity IPC begin");
        let result = (self.declare)(&mut id);
        tracing::debug!(target: "oink_cursor_timing", "cursor latency: activity IPC end");
        if result.is_ok() {
            st.id = id;
            tracing::debug!(assertion_id = id, "IOPMAssertionDeclareUserActivity succeeded");
            Ok(true)
        } else {
            if !self.warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(error = ?result.as_ref().err(), "IOPMAssertionDeclareUserActivity failed");
            }
            result.map(|()| false)
        }
    }
}

impl Drop for OrdinaryActivity {
    fn drop(&mut self) {
        let st = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        if st.id != 0 {
            // SAFETY: 释放自己持有的断言。
            unsafe { IOPMAssertionRelease(st.id) };
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn ordinary_declaration_preserves_event_driven_one_second_throttle() {
        let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let count = calls.clone();
        let activity = UserActivity::with_declaration(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        let now = Instant::now();
        assert!(activity.ordinary.declare_at(now).unwrap());
        assert!(!activity.ordinary.declare_at(now + Duration::from_millis(999)).unwrap());
        assert!(activity.ordinary.declare_at(now + Duration::from_secs(1)).unwrap());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        // Advancing time alone does not ask the worker to renew an assertion.
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn throttled_to_once_per_interval() {
        let t = Instant::now();
        assert!(due(None, t));
        assert!(!due(Some(t), t));
        assert!(!due(Some(t), t + Duration::from_millis(999)));
        assert!(due(Some(t), t + MIN_INTERVAL));
        // 时钟回退不会 panic，也不会一直声明
        assert!(!due(Some(t + Duration::from_secs(5)), t));
    }
}
