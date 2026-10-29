//! macOS 实现：CGEventTap 截获、CGEventPost 注入。
//!
//! - `tap`：截获线程与事件翻译；
//! - `activity`：注入远端事件时声明用户活动（点亮熄灭的屏幕、推迟熄屏）；
//! - `inject`：注入（坐标按 float32 向下取整，贴着右、下边界的位置不会落到屏幕外）；
//! - `cursor`：Remote 模式的光标隐藏、解耦（原地冻结）与恢复；
//! - `capslock`：Caps Lock 的锁定状态与切换输入法（目标端），Remote 模式下保护本机状态（源端），
//!   焦点移交时读取与设置大写锁定和输入法；
//! - `main_queue`：把 TIS（输入法）调用交给主线程（TIS 只能在主线程调用）；
//! - `keymap`：HID Usage ↔ kVK、媒体键的 NX 键类型；
//! - `sysevent`：系统定义事件（媒体键、亮度键）与触摸板手势事件的读取和合成；
//! - `device`：按事件的发送者判断鼠标 / 触摸板；
//! - `system`：手感设置、权限、私有函数查找。

mod activity;
mod application_focus;
pub mod capslock;
mod contacts;
mod cursor;
mod cursor_refresh;
mod cursor_reveal;
mod cursor_visibility;
mod device;
mod dock_frame;
mod drag_hold;
mod gesture_diagnostics;
mod idle_pulse;
mod inject;
mod keyboard_diagnostics;
use crate::mac_keymap as keymap;
mod loop_timer;
mod main_queue;
mod physical_buttons;
mod physical_motion;
mod resume_hotkey;
mod session;
mod sysevent;
mod system;
mod tap;

use std::sync::Mutex;

use oink_foundation::Point;
use oink_foundation::input::{CaptureMode, FeelProfile, InjectEvent, InputPermissions};

use crate::{EventHandler, InputBackend, InputError};

/// `crate::run_with_main_run_loop` 的 macOS 实现：主线程以 0.1 s 为一段运行 run loop，直到工作线程结束。
pub(crate) fn run_with_main_run_loop<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let worker = std::thread::Builder::new()
        .name("oink-main-work".into())
        .spawn(f)
        .expect("创建线程失败");
    // SAFETY: 读取 CoreFoundation 导出的常量。
    let mode = unsafe { objc2_core_foundation::kCFRunLoopDefaultMode };
    while !worker.is_finished() {
        let r = objc2_core_foundation::CFRunLoop::run_in_mode(mode, 0.1, false);
        if r == objc2_core_foundation::CFRunLoopRunResult::Finished {
            // run loop 里还没有任何来源（例如还没用过输入法 API）时立即返回：不要空转。
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    match worker.join() {
        Ok(v) => v,
        Err(p) => std::panic::resume_unwind(p),
    }
}

pub(crate) fn backend() -> Result<Box<dyn InputBackend>, InputError> {
    // 主线程读取 Caps 本机行为设置并订阅变化（TIS 只能在主线程调用）。
    capslock::init();
    session::init();
    // Prime the main-thread Carbon status cache before the tap needs stuck-key cleanup.
    let _ = system::secure_input_active();
    let safety = std::sync::Arc::new(crate::session_gate::SessionGate::default());
    let injector = inject::Injector::new(safety.clone())?;
    Ok(Box::new(MacInput {
        safety,
        tap: Mutex::new(None),
        injector,
    }))
}

struct MacInput {
    safety: std::sync::Arc<crate::session_gate::SessionGate>,
    tap: Mutex<Option<tap::Tap>>,
    injector: inject::Injector,
}

impl InputBackend for MacInput {
    fn application_focus_supported(&self) -> bool {
        true
    }
    fn application_focus_watch(&self, observer: Option<crate::application_focus::Observer>) {
        application_focus::watch(observer);
    }
    fn application_focus_sample(&self) -> oink_foundation::application_focus::Observation {
        application_focus::sample()
    }
    fn application_focus_park(
        &self,
        expected: oink_foundation::application_focus::ObservationStamp,
        operation: u64,
        permit: crate::application_focus::Permit,
    ) -> oink_foundation::application_focus::ParkResult {
        let result = application_focus::park(expected, operation, permit);
        tracing::info!(target: "oink_application_focus", operation, generation = expected.generation,
            sequence = expected.sequence, ?result, "native park completed");
        result
    }
    fn application_focus_dismiss(&self) {
        application_focus::dismiss();
    }
    fn stop_injection_session(&self) {
        let _ = self.set_cursor_constraint(None);
        self.injector.stop_session();
    }
    fn set_input_paused(&self, paused: bool) {
        self.safety.set_paused(paused);
    }
    fn input_safety(&self) -> crate::session_gate::InputSafety {
        self.safety.snapshot()
    }
    fn watch_input_safety(&self, wake: &crate::capture_loop::Wake) {
        self.safety.watch(wake);
    }
    fn held_session_keys(&self) -> Vec<crate::KeyCode> {
        tap::held_session_keys()
    }
    fn session_locked(&self) -> Option<bool> {
        session::locked()
    }

    fn start(&self, handler: EventHandler) -> Result<(), InputError> {
        let mut t = self.tap.lock().unwrap_or_else(|e| e.into_inner());
        if t.is_some() {
            return Err(InputError::Platform("截获已经启动".into()));
        }
        *t = Some(tap::Tap::start(
            handler,
            self.injector.physical_buttons.clone(),
            self.injector.motion.clone(),
            self.safety.clone(),
        )?);
        Ok(())
    }

    fn start_loop(&self, owner: Box<dyn crate::capture_loop::CaptureLoop>) -> Result<(), InputError> {
        let mut t = self.tap.lock().unwrap_or_else(|e| e.into_inner());
        if t.is_some() {
            return Err(InputError::Platform("capture already started".into()));
        }
        *t = Some(tap::Tap::start_loop(
            owner,
            self.injector.physical_buttons.clone(),
            self.injector.motion.clone(),
            self.safety.clone(),
        )?);
        Ok(())
    }

    fn capture_active(&self) -> Option<bool> {
        Some(
            self.tap
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(tap::Tap::active),
        )
    }

    fn stop(&self) {
        self.injector.gesture_boundary("capture_stop");
        let tap = self.tap.lock().unwrap_or_else(|e| e.into_inner()).take();
        match tap {
            Some(t) => t.stop(),
            None => cursor::restore(),
        }
    }

    fn set_keyboard_remote(&self, remote: bool) -> Result<(), InputError> {
        let tap = self.tap.lock().unwrap_or_else(|e| e.into_inner());
        tap.as_ref().ok_or(InputError::NotStarted)?.set_keyboard_remote(remote);
        Ok(())
    }

    fn set_cursor_constraint(&self, bounds: Option<oink_foundation::Rect>) -> Result<(), InputError> {
        let tap = self.tap.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tap) = tap.as_ref() {
            tap.set_cursor_constraint(bounds.is_some());
        } else if bounds.is_none() {
            cursor::constrain(false);
        } else {
            return Err(InputError::NotStarted);
        }
        Ok(())
    }
    fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError> {
        self.injector.gesture_boundary("set_mode");
        let t = self.tap.lock().unwrap_or_else(|e| e.into_inner());
        let tap = t.as_ref().ok_or(InputError::NotStarted)?;
        self.injector.motion.invalidate_position();
        tap.set_mode(mode);
        Ok(())
    }

    fn set_drag_anchor(&self, anchor: Option<oink_foundation::dnd::DragAnchor>) -> Result<(), InputError> {
        self.tap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .ok_or(InputError::NotStarted)?
            .set_drag_anchor(anchor, &self.injector)
    }

    fn freeze_drag_anchor(&self, generation: u64) -> Result<oink_foundation::dnd::DragAnchor, InputError> {
        self.tap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .ok_or(InputError::NotStarted)?
            .freeze_drag_anchor(generation, &self.injector)
    }

    fn relocate_drag_anchor(&self, generation: u64, at: Point) -> Result<(), InputError> {
        let at = inject::quartz_drag_point(at);
        self.tap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .ok_or(InputError::NotStarted)?
            .relocate_drag_anchor(generation, Point::new(at.x, at.y), &self.injector)
    }

    fn move_held_drag(&self, generation: u64, at: Point) -> Result<(), InputError> {
        self.move_held_drag_observed(generation, at, &|_| {})
    }

    fn move_held_drag_observed(&self, generation: u64, at: Point, posted: &dyn Fn(Point)) -> Result<(), InputError> {
        let q = inject::quartz_drag_point(at);
        let at = Point::new(q.x, q.y);
        let tap = self.tap.lock().unwrap_or_else(|e| e.into_inner());
        tap.as_ref()
            .ok_or(InputError::NotStarted)?
            .move_held_drag(generation, at, &self.injector, posted)
    }

    fn nudge_held_drag(
        &self,
        generation: u64,
        button: oink_foundation::input::MouseButton,
        posted: &dyn Fn(oink_foundation::dnd::ImageNudgePost),
    ) -> Result<(), InputError> {
        let tap = self.tap.lock().unwrap_or_else(|e| e.into_inner());
        tap.as_ref()
            .ok_or(InputError::NotStarted)?
            .nudge_held_drag(generation, &self.injector, button, posted)
    }

    fn restore_local(&self, at: Point) -> Result<(), InputError> {
        self.injector.gesture_boundary("restore_local");
        let t = self.tap.lock().unwrap_or_else(|e| e.into_inner());
        let tap = t.as_ref().ok_or(InputError::NotStarted)?;
        // 落点常在进入边上（如 `right - 1e-6`）：与注入一样按 float32 向下取整，warp 后光标不会落在屏幕外。
        let q = inject::quartz_point(at);
        self.injector.motion.invalidate_position();
        tap.restore_local(Point::new(q.x, q.y));
        Ok(())
    }

    fn press_drag_at(&self, at: Point) -> Result<(), InputError> {
        if !self.input_safety().active() {
            return Err(InputError::NotStarted);
        }
        self.injector.press_drag_at(at)
    }

    fn release_physical_drag(&self) -> Result<(), InputError> {
        self.injector.release_physical_drag()
    }

    fn declare_user_activity(&self) -> Result<bool, InputError> {
        self.injector.declare_user_activity()
    }

    fn reset_user_idle(&self) -> Result<bool, InputError> {
        self.injector.reset_user_idle()
    }

    fn inject(&self, event: &InjectEvent) -> Result<(), InputError> {
        self.injector.inject(event)
    }
    fn rebase_pointer_motion(&self) -> Result<Point, InputError> {
        self.injector.rebase_pointer_motion()
    }

    fn system_button_state(&self, button: oink_foundation::input::MouseButton) -> Option<bool> {
        use objc2_core_graphics::{CGEventSource, CGEventSourceStateID, CGMouseButton};
        Some(CGEventSource::button_state(
            CGEventSourceStateID::HIDSystemState,
            CGMouseButton(inject::button_number(button)),
        ))
    }

    fn pointer_capture_sample(&self) -> Option<crate::pointer_capture::CaptureSample> {
        self.observe_local_pointer(capture_sample).flatten()
    }

    fn foreground_bundle_id(&self, foreground: u64) -> Option<String> {
        let app = objc2_app_kit::NSWorkspace::sharedWorkspace().frontmostApplication()?;
        if app.processIdentifier() as u64 != foreground {
            return None;
        }
        app.bundleIdentifier().map(|id| id.to_string())
    }

    fn cursor_position(&self) -> Result<Point, InputError> {
        inject::checked_location()
    }

    fn cursor_injection_target(&self, event: &InjectEvent) -> Option<(Point, f64)> {
        if !matches!(event, InjectEvent::MoveTo { .. } | InjectEvent::MoveToWithDelta { .. }) {
            return None;
        }
        self.injector.last_cursor_target()
    }

    fn refresh_pointer_capture_sample(
        &self,
        sample: crate::pointer_capture::CaptureSample,
        eligible: bool,
        captured: bool,
    ) -> Option<crate::pointer_capture::CaptureSample> {
        self.observe_local_pointer(|| {
            self.injector
                .refresh_capture_visibility(sample, eligible, captured, capture_sample)
        })
        .flatten()
    }

    fn edge_resistance_regions(
        &self,
        displays: &[oink_foundation::display::DisplayInfo],
    ) -> Vec<oink_foundation::input::EdgeResistanceRegion> {
        system::edge_resistance_regions(displays)
    }

    fn feel_profile(&self) -> FeelProfile {
        // Reuse the ordinary preference observer's 2 s cadence for checkbox-only
        // edits too: selected/enabled-source notifications need not describe such
        // edits. All TIS work stays on the main queue; capture reads one atomic.
        capslock::request_refresh();
        system::feel_profile()
    }

    fn caps_lock_switches_input_source(&self) -> Option<bool> {
        capslock::source_switches_input()
    }

    fn permissions(&self) -> InputPermissions {
        system::permissions()
    }
}

impl Drop for MacInput {
    fn drop(&mut self) {
        self.stop();
    }
}

impl MacInput {
    fn observe_local_pointer<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        // Serialize mode requests with the sample as well as native cursor work.
        let tap = self.tap.lock().unwrap_or_else(|e| e.into_inner());
        tap.as_ref()?.observe_local_pointer(read)
    }
}

#[allow(deprecated)] // CGCursorIsVisible has no public replacement; heuristic evidence only.
fn capture_sample() -> Option<crate::pointer_capture::CaptureSample> {
    use objc2_core_graphics::{CGCursorIsVisible, CGEventSource, CGEventSourceStateID, CGEventType};
    let p = inject::current_location();
    let position = Point::new(p.x, p.y);
    let motion_serial = [
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
    ]
    .into_iter()
    .fold(0u32, |n, t| {
        n.wrapping_add(CGEventSource::counter_for_event_type(
            CGEventSourceStateID::CombinedSessionState,
            t,
        ))
    });
    // NSWorkspace's frontmostApplication is safe to read without activating anything.
    let foreground = objc2_app_kit::NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map_or(0, |a| a.processIdentifier() as u64);
    let hidden = !CGCursorIsVisible();
    Some(crate::pointer_capture::CaptureSample {
        windows: None,
        hidden,
        application_hidden: cursor_visibility::application_hidden(),
        confined: false,
        position,
        motion_serial,
        motion_observable: true,
        foreground,
    })
}
