//! 假的输入实现：由测试「喂」截获事件，记录注入的事件。多设备模拟器用它代替真实系统。
//!
//! 模式切换默认立即生效。[`FakeInput::set_deferred_modes`] 打开后，`set_mode` / `restore_local` 只排队，
//! 直到测试调用 [`FakeInput::apply_pending_modes`] 才生效——模拟真实平台上模式切换在另一个线程上
//! 稍后执行（Windows 钩子线程、macOS tap 线程），用来复现「路由已经换了焦点、截获还在旧模式」这段空档。
//! 生效时：`Remote { park }` 把光标放到停靠点；`restore_local(at)` 把光标放到 `at` 再切回 `Local`。
//!
use std::sync::{Arc, Mutex};

use oink_foundation::Point;
use oink_foundation::input::{
    CaptureMode, CapturedEvent, Disposition, FeelProfile, InjectEvent, InputPermissions, PermissionState,
};

use oink_input::{EventHandler, InputBackend, InputError};

#[derive(Default)]
struct State {
    touchpad_contacts: Option<u8>,
    injection_active: bool,
    injection_sessions: u64,
    injection_start: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    resume_hotkey: Option<oink_input::capture_loop::ResumeHotkey>,
    resume_requested: bool,
    capture_sessions: u64,
    handler: Option<EventHandler>,
    loop_started: bool,
    capture_tier: Option<oink_foundation::realtime::Tier>,
    loop_turns: u64,
    loop_capturing: bool,
    mode: Option<CaptureMode>,
    keyboard_remote: bool,
    drag_anchor: Option<oink_foundation::dnd::DragAnchor>,
    cursor: Point,
    injected: Vec<InjectEvent>,
    timed_injections: Option<Vec<(std::time::Instant, InjectEvent)>>,
    injection_sources: Option<Vec<Option<oink_input::source_time::InjectionSource>>>,
    feel: FeelProfile,
    deferred: bool,
    pending: Vec<PendingMode>,
    idle_resets: u64,
    activity_declarations: u64,
    displays_asleep: bool,
    held_session_keys: Vec<oink_input::KeyCode>,
}

/// 排队中的模式切换。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PendingMode {
    Set(CaptureMode),
    RestoreLocal(Point),
}

impl State {
    fn apply(&mut self, m: PendingMode) {
        match m {
            PendingMode::Set(mode) => {
                if let CaptureMode::Remote { park } = mode {
                    self.cursor = self.drag_anchor.map_or(park, |anchor| anchor.at);
                }
                self.mode = (self.handler.is_some() || self.loop_capturing).then_some(mode);
            }
            PendingMode::RestoreLocal(at) => {
                self.cursor = at;
                self.injected.push(InjectEvent::MoveTo { pos: at });
                self.mode = (self.handler.is_some() || self.loop_capturing).then_some(CaptureMode::Local);
            }
        }
    }

    fn switch(&mut self, m: PendingMode) -> Result<(), InputError> {
        if self.handler.is_none() && !self.loop_started {
            return Err(InputError::NotStarted);
        }
        if self.deferred {
            self.pending.push(m);
        } else {
            self.apply(m);
        }
        Ok(())
    }
}

/// 可克隆的句柄：一份交给被测代码当 `InputBackend`，另一份留在测试里喂事件、查结果。
#[derive(Clone, Default)]
pub struct FakeInput {
    safety: Arc<oink_input::session_gate::SessionGate>,
    state: Arc<Mutex<State>>,
    event_loop: Arc<Mutex<Option<MemoryLoop>>>,
}

impl std::fmt::Debug for FakeInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeInput").finish_non_exhaustive()
    }
}

impl FakeInput {
    pub fn set_touchpad_contacts(&self, capacity: Option<u8>) {
        self.state.lock().unwrap().touchpad_contacts = capacity;
    }
    pub fn injection_active(&self) -> bool {
        self.state.lock().unwrap().injection_active
    }
    pub fn injection_sessions(&self) -> u64 {
        self.state.lock().unwrap().injection_sessions
    }
    /// Hold the next fake worker before its native-start observation. Dropping
    /// the release sender also unblocks it during test cleanup/unwind.
    pub fn block_next_injection_start(&self) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, entering) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let mut state = self.state.lock().unwrap();
        assert!(state.injection_start.is_none());
        state.injection_start = Some((entered, released));
        (entering, release)
    }
    pub fn paused_hotkey(&self) -> Option<oink_input::capture_loop::ResumeHotkey> {
        self.state.lock().unwrap().resume_hotkey
    }
    pub fn capture_sessions(&self) -> u64 {
        self.state.lock().unwrap().capture_sessions
    }
    /// A system hotkey notification, independent of captured keyboard events.
    pub fn fire_resume_hotkey(&self) {
        self.state.lock().unwrap().resume_requested = true;
        if let Some(control) = self.event_loop.lock().unwrap().as_ref() {
            control.thread.unpark();
        }
    }
    pub fn keyboard_remote(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).keyboard_remote
    }
    /// Unobserved OS-held keys for guard-start cleanup tests; no desktop input.
    pub fn set_held_session_keys(&self, keys: Vec<oink_input::KeyCode>) {
        self.state.lock().unwrap().held_session_keys = keys;
    }
    pub fn set_displays_asleep(&self, asleep: bool) {
        self.state.lock().unwrap().displays_asleep = asleep;
    }
    pub fn idle_reset_count(&self) -> u64 {
        self.state.lock().unwrap().idle_resets
    }
    pub fn activity_declaration_count(&self) -> u64 {
        self.state.lock().unwrap().activity_declarations
    }
    /// Opt into the native capture scheduling policy for standalone cost fixtures.
    pub fn set_capture_tier(&self, tier: oink_foundation::realtime::Tier) {
        self.state.lock().unwrap().capture_tier = Some(tier);
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// 模拟系统截获到一个事件；未启动时返回 `Pass`。
    pub fn feed(&self, event: &CapturedEvent) -> Disposition {
        let pending = self.event_loop.lock().unwrap().as_ref().map(|control| {
            let (reply, result) = std::sync::mpsc::sync_channel(1);
            let sent = control.events.send((event.clone(), reply)).is_ok();
            control.thread.unpark();
            (sent, result)
        });
        if let Some((sent, result)) = pending {
            return if sent {
                result.recv().unwrap_or(Disposition::Pass)
            } else {
                Disposition::Pass
            };
        }
        let handler = self.state.lock().unwrap().handler.clone();
        match handler {
            Some(h) => h(event),
            None => Disposition::Pass,
        }
    }

    /// Read the currently retained injection evidence without consuming it.
    pub fn injected(&self) -> Vec<InjectEvent> {
        self.state.lock().unwrap().injected.clone()
    }

    /// 取出并清空已注入的事件。
    pub fn take_injected(&self) -> Vec<InjectEvent> {
        std::mem::take(&mut self.state.lock().unwrap().injected)
    }

    /// Opt-in timestamps for standalone end-to-end measurements; native injection
    /// costs are deliberately absent from this fake backend.
    pub fn record_injection_times(&self) {
        self.state.lock().unwrap().timed_injections = Some(Vec::new());
    }

    pub fn take_injection_times(&self) -> Vec<(std::time::Instant, InjectEvent)> {
        self.state.lock().unwrap().timed_injections.take().unwrap_or_default()
    }
    /// Opt-in interval provenance for native-owner tests; no real desktop input.
    pub fn record_injection_sources(&self) {
        self.state.lock().unwrap().injection_sources = Some(Vec::new());
    }
    pub fn take_injection_sources(&self) -> Vec<Option<oink_input::source_time::InjectionSource>> {
        self.state.lock().unwrap().injection_sources.take().unwrap_or_default()
    }

    pub fn mode(&self) -> Option<CaptureMode> {
        self.state.lock().unwrap().mode
    }

    pub fn set_cursor(&self, p: Point) {
        self.state.lock().unwrap().cursor = p;
    }

    /// 模式切换是否延迟生效（见模块说明）。关闭时立即执行已排队的切换。
    pub fn set_deferred_modes(&self, deferred: bool) {
        let mut s = self.state.lock().unwrap();
        s.deferred = deferred;
        if !deferred {
            for m in std::mem::take(&mut s.pending) {
                s.apply(m);
            }
        }
    }

    /// 排队中、尚未生效的模式切换。
    pub fn pending_modes(&self) -> Vec<PendingMode> {
        self.state.lock().unwrap().pending.clone()
    }

    /// Owner turns in the in-memory backend, for isolated idle measurements.
    pub fn loop_turns(&self) -> u64 {
        self.state.lock().unwrap().loop_turns
    }

    /// 让排队的模式切换按顺序生效。
    pub fn apply_pending_modes(&self) {
        let mut s = self.state.lock().unwrap();
        for m in std::mem::take(&mut s.pending) {
            s.apply(m);
        }
    }
}

/// Test-only backend loop: all callbacks and timer turns use one memory thread.
struct MemoryLoop {
    events: std::sync::mpsc::Sender<(CapturedEvent, std::sync::mpsc::SyncSender<Disposition>)>,
    thread: std::thread::Thread,
    stop: Arc<std::sync::atomic::AtomicBool>,
    join: std::thread::JoinHandle<()>,
}
impl MemoryLoop {
    fn start(
        state: Arc<Mutex<State>>,
        mut owner: Box<dyn oink_input::capture_loop::CaptureLoop>,
    ) -> Result<Self, InputError> {
        let (events, received) =
            std::sync::mpsc::channel::<(CapturedEvent, std::sync::mpsc::SyncSender<Disposition>)>();
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let join = std::thread::Builder::new()
            .name("oink-fake-input".into())
            .spawn(move || {
                if let Some(tier) = state.lock().unwrap().capture_tier {
                    oink_foundation::realtime::apply(oink_foundation::realtime::ThreadRole::Input, tier).unwrap();
                }
                let thread = std::thread::current();
                let wake_thread = thread.clone();
                owner.set_wake(Arc::new(move || wake_thread.unpark()));
                {
                    let mut state = state.lock().unwrap();
                    state.loop_started = true;
                    state.mode = Some(CaptureMode::Local);
                }
                let mut ready = Some((ready, thread));
                while !stopping.load(std::sync::atomic::Ordering::Acquire) {
                    let resume = {
                        let mut state = state.lock().unwrap();
                        std::mem::take(&mut state.resume_requested) && state.resume_hotkey.is_some()
                    };
                    if resume {
                        owner.resume_hotkey();
                    }
                    let due = owner.poll(std::time::Instant::now());
                    if owner.stopped() {
                        break;
                    }
                    let capturing = owner.capture_requested();
                    let attached = {
                        let mut state = state.lock().unwrap();
                        let attached = capturing && !state.loop_capturing;
                        state.resume_hotkey = owner.paused_hotkey();
                        if attached {
                            state.capture_sessions += 1;
                        }
                        if !capturing {
                            state.mode = None;
                        } else if !state.loop_capturing {
                            state.mode = Some(CaptureMode::Local);
                        }
                        state.loop_capturing = capturing;
                        state.loop_turns += 1;
                        attached
                    };
                    if attached {
                        // Match the native owner's successful capture attachment.
                        // Intentional detachment is not a capture failure.
                        owner.capture_available(true);
                    }
                    // Match native startup: returning from start_loop means the
                    // initial capture request has been applied, not just spawned.
                    if let Some((ready, thread)) = ready.take() {
                        let _ = ready.send(thread);
                    }
                    let mut handled = false;
                    for (event, reply) in received.try_iter() {
                        handled = true;
                        let disposition = if capturing {
                            owner.event(&event)
                        } else {
                            Disposition::Pass
                        };
                        let _ = reply.send(disposition);
                    }
                    if handled {
                        continue;
                    }
                    match due {
                        Some(due) => {
                            std::thread::park_timeout(due.saturating_duration_since(std::time::Instant::now()))
                        }
                        None => std::thread::park(),
                    }
                }
                let mut state = state.lock().unwrap();
                state.loop_started = false;
                state.resume_hotkey = None;
                state.loop_capturing = false;
                state.mode = None;
                state.pending.clear();
            })
            .map_err(|error| InputError::Platform(error.to_string()))?;
        let thread = started
            .recv()
            .map_err(|_| InputError::Platform("memory input loop stopped during startup".into()))?;
        Ok(Self {
            events,
            thread,
            stop,
            join,
        })
    }
}

struct DisplayPower {
    state: Arc<Mutex<State>>,
    last: Option<bool>,
}
impl oink_input::display_power::Observation for DisplayPower {
    fn poll(&mut self) -> Option<bool> {
        let asleep = self.state.lock().unwrap().displays_asleep;
        if self.last == Some(asleep) {
            return None;
        }
        self.last = Some(asleep);
        Some(asleep)
    }
}

impl InputBackend for FakeInput {
    fn touchpad_contacts(&self) -> Option<u8> {
        self.state.lock().unwrap().touchpad_contacts
    }
    fn start_injection_session(&self) -> Result<(), InputError> {
        let blocked = self.state.lock().unwrap().injection_start.take();
        if let Some((entered, released)) = blocked {
            let _ = entered.send(());
            let _ = released.recv();
        }
        let mut state = self.state.lock().unwrap();
        state.injection_active = true;
        state.injection_sessions += 1;
        Ok(())
    }
    fn stop_injection_session(&self) {
        self.state.lock().unwrap().injection_active = false;
    }
    fn set_input_paused(&self, paused: bool) {
        self.safety.set_paused(paused);
    }
    fn input_safety(&self) -> oink_input::session_gate::InputSafety {
        self.safety.snapshot()
    }
    fn watch_input_safety(&self, wake: &oink_input::capture_loop::Wake) {
        self.safety.watch(wake);
    }
    fn set_drag_anchor(&self, anchor: Option<oink_foundation::dnd::DragAnchor>) -> Result<(), InputError> {
        self.state.lock().unwrap().drag_anchor = anchor;
        Ok(())
    }
    fn held_session_keys(&self) -> Vec<oink_input::KeyCode> {
        self.state.lock().unwrap().held_session_keys.clone()
    }
    fn declare_user_activity(&self) -> Result<bool, InputError> {
        self.state.lock().unwrap().activity_declarations += 1;
        Ok(true)
    }
    fn display_power_observation(&self) -> Result<Box<dyn oink_input::display_power::Observation>, InputError> {
        Ok(Box::new(DisplayPower {
            state: self.state.clone(),
            last: None,
        }))
    }
    fn reset_user_idle(&self) -> Result<bool, InputError> {
        self.state.lock().unwrap().idle_resets += 1;
        Ok(true)
    }

    fn diagnostic_key_details(&self) -> bool {
        true
    }
    fn start(&self, handler: EventHandler) -> Result<(), InputError> {
        let mut s = self.state.lock().unwrap();
        s.handler = Some(handler);
        s.mode = Some(CaptureMode::Local);
        Ok(())
    }

    fn start_loop(&self, owner: Box<dyn oink_input::capture_loop::CaptureLoop>) -> Result<(), InputError> {
        let mut control = self.event_loop.lock().unwrap();
        if control.is_some() || self.state.lock().unwrap().handler.is_some() {
            return Err(InputError::Platform("capture already started".into()));
        }
        *control = Some(MemoryLoop::start(self.state.clone(), owner)?);
        Ok(())
    }

    fn capture_active(&self) -> Option<bool> {
        let state = self.state.lock().unwrap();
        Some(state.handler.is_some() || state.loop_capturing)
    }

    fn stop(&self) {
        let control = self.event_loop.lock().unwrap().take();
        if let Some(control) = control {
            control.stop.store(true, std::sync::atomic::Ordering::Release);
            control.thread.unpark();
            let _ = control.join.join();
        }
        let mut s = self.state.lock().unwrap();
        s.handler = None;
        s.loop_started = false;
        s.loop_capturing = false;
        s.mode = None;
        s.pending.clear();
    }

    fn set_keyboard_remote(&self, remote: bool) -> Result<(), InputError> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).keyboard_remote = remote;
        Ok(())
    }
    fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError> {
        self.state.lock().unwrap().switch(PendingMode::Set(mode))
    }

    fn restore_local(&self, at: Point) -> Result<(), InputError> {
        self.state.lock().unwrap().switch(PendingMode::RestoreLocal(at))
    }

    fn inject(&self, event: &InjectEvent) -> Result<(), InputError> {
        let mut s = self.state.lock().unwrap();
        if let Some(pos) = event.move_target() {
            s.cursor = pos;
        }
        if let Some(timed) = &mut s.timed_injections {
            timed.push((std::time::Instant::now(), event.clone()));
        }
        if let Some(sources) = &mut s.injection_sources {
            sources.push(oink_input::source_time::injection_source());
        }
        s.injected.push(event.clone());
        Ok(())
    }

    fn cursor_position(&self) -> Result<Point, InputError> {
        Ok(self.state.lock().unwrap().cursor)
    }

    fn feel_profile(&self) -> FeelProfile {
        self.state.lock().unwrap().feel.clone()
    }

    fn permissions(&self) -> InputPermissions {
        InputPermissions {
            accessibility: PermissionState::NotApplicable,
            input_monitoring: PermissionState::NotApplicable,
            secure_input_active: false,
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_foundation::input::{InputEvent, SourceKind};
    use std::sync::Arc;

    #[test]
    fn feed_reaches_handler_and_inject_is_recorded() {
        let fake = FakeInput::new();
        let backend: Box<dyn InputBackend> = Box::new(fake.clone());
        backend.start(Arc::new(|_| Disposition::Swallow)).unwrap();
        let ev = CapturedEvent {
            physical: true,
            event: InputEvent::PointerDelta { dx: 1.0, dy: 0.0 },
            source: SourceKind::Mouse,
            time_us: 0,
            cursor: None,
        };
        assert_eq!(fake.feed(&ev), Disposition::Swallow);
        backend
            .inject(&InjectEvent::MoveTo {
                pos: Point::new(3.0, 4.0),
            })
            .unwrap();
        assert_eq!(fake.take_injected().len(), 1);
        assert_eq!(backend.cursor_position().unwrap(), Point::new(3.0, 4.0));
    }

    #[test]
    fn deferred_modes_apply_in_order() {
        let fake = FakeInput::new();
        let backend: Box<dyn InputBackend> = Box::new(fake.clone());
        backend.start(Arc::new(|_| Disposition::Pass)).unwrap();
        fake.set_cursor(Point::new(990.0, 300.0));
        fake.set_deferred_modes(true);
        let park = Point::new(500.0, 400.0);
        backend.set_mode(CaptureMode::Remote { park }).unwrap();
        assert_eq!(fake.mode(), Some(CaptureMode::Local), "not applied yet");
        assert_eq!(fake.cursor_position().unwrap(), Point::new(990.0, 300.0));
        fake.apply_pending_modes();
        assert_eq!(fake.mode(), Some(CaptureMode::Remote { park }));
        assert_eq!(fake.cursor_position().unwrap(), park);

        let at = Point::new(999.0, 310.0);
        backend.restore_local(at).unwrap();
        assert_eq!(fake.pending_modes(), vec![PendingMode::RestoreLocal(at)]);
        assert_eq!(fake.cursor_position().unwrap(), park);
        fake.apply_pending_modes();
        assert_eq!(fake.mode(), Some(CaptureMode::Local));
        assert_eq!(fake.cursor_position().unwrap(), at);
        assert!(fake.pending_modes().is_empty());
    }
}
#[test]
fn display_observer_follows_simulated_device_and_reports_changes_once() {
    let input = FakeInput::new();
    let mut observer = input.display_power_observation().unwrap();
    assert_eq!(observer.poll(), Some(false));
    assert_eq!(observer.poll(), None);
    input.set_displays_asleep(true);
    assert_eq!(observer.poll(), Some(true));
    assert_eq!(observer.poll(), None);
    input.set_displays_asleep(false);
    assert_eq!(observer.poll(), Some(false));
}

#[cfg(all(test, debug_assertions))]
mod loop_tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        time::{Duration, Instant},
    };
    struct Owner {
        capture: Arc<AtomicBool>,
        turns: Arc<AtomicUsize>,
        calls: Arc<Mutex<Vec<std::thread::ThreadId>>>,
        wake: std::sync::mpsc::Sender<oink_input::capture_loop::Wake>,
    }
    impl oink_input::capture_loop::CaptureLoop for Owner {
        fn set_wake(&mut self, wake: oink_input::capture_loop::Wake) {
            self.wake.send(wake).unwrap();
        }
        fn event(&mut self, _: &CapturedEvent) -> Disposition {
            self.calls.lock().unwrap().push(std::thread::current().id());
            Disposition::Swallow
        }
        fn poll(&mut self, now: Instant) -> Option<Instant> {
            self.turns.fetch_add(1, Ordering::Relaxed);
            Some(now + Duration::from_millis(2))
        }
        fn stopped(&self) -> bool {
            false
        }
        fn capture_requested(&self) -> bool {
            self.capture.load(Ordering::Acquire)
        }
    }
    fn wait(mut predicate: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(2);
        while !predicate() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn memory_loop_keeps_timer_and_owner_across_capture_withdrawal_and_resume() {
        let fake = FakeInput::new();
        let capture = Arc::new(AtomicBool::new(false));
        let turns = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (wake, rx) = std::sync::mpsc::channel();
        fake.start_loop(Box::new(Owner {
            capture: capture.clone(),
            turns: turns.clone(),
            calls: calls.clone(),
            wake,
        }))
        .unwrap();
        let wake = rx.recv().unwrap();
        let event = CapturedEvent {
            physical: true,
            event: oink_foundation::input::InputEvent::PointerDelta { dx: 1., dy: 0. },
            source: oink_foundation::input::SourceKind::Mouse,
            time_us: 1,
            cursor: None,
        };
        assert_eq!(fake.feed(&event), Disposition::Pass);
        capture.store(true, Ordering::Release);
        wake();
        wait(|| fake.capture_active() == Some(true));
        assert_eq!(fake.feed(&event), Disposition::Swallow);
        capture.store(false, Ordering::Release);
        wake();
        wait(|| fake.capture_active() == Some(false));
        let before = turns.load(Ordering::Relaxed);
        wait(|| turns.load(Ordering::Relaxed) > before + 2);
        assert_eq!(fake.feed(&event), Disposition::Pass);
        capture.store(true, Ordering::Release);
        wake();
        wait(|| fake.capture_active() == Some(true));
        assert_eq!(fake.feed(&event), Disposition::Swallow);
        fake.stop();
        assert_eq!(fake.capture_active(), Some(false));
        assert_eq!(fake.feed(&event), Disposition::Pass);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
        assert_ne!(calls[0], std::thread::current().id());
    }
}

/// Recording seam for the Windows desktop-loss state machine. No native APIs.
pub use oink_input::capture_continuity::CaptureContinuity as WindowsCapture;
