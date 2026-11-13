//! Native helper state, and the privileged Android screen as the router's output.
use oink_foundation::{
    Point,
    display::{ANDROID_INPUT_DISPLAY, DisplayInfo},
    input::{InjectEvent, KeyCode, MouseButton, ScrollUnit},
};
use oink_mobile_core::android_gestures::{AndroidGestures, SystemGesture};
use oink_mobile_core::android_input::{AndroidDisplay, AndroidKeyboard, KeyboardError, PointerServo};
use oink_mobile_core::{InputRouter, ScreenSink};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum AndroidPointerPath {
    VirtualMouse,
    SelfDrawnAbsolute,
}
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum AndroidSystemGesture {
    Home,
    Recents,
    SwitchNext,
    SwitchPrevious,
    Notifications,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum AndroidInputAction {
    Ready { ready: bool },
    Focus { active: bool },
    Move { x: f64, y: f64, generation: u64 },
    Key { page: u16, usage: u16, pressed: bool },
    Button { button: u8, pressed: bool },
    Scroll { horizontal: f64, vertical: f64 },
    ReleaseAll,
    SystemGesture { gesture: AndroidSystemGesture },
    GroupActivity { active: bool },
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct AndroidInputCommand {
    pub sequence: u64,
    pub action: AndroidInputAction,
    pub helper_incarnation: Vec<u8>,
    pub generation: u64,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct AndroidDisplayState {
    pub width: u32,
    pub height: u32,
    pub rotation: u16,
    pub generation: u64,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct AndroidPointerPoint {
    pub x: f64,
    pub y: f64,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct AndroidKeyboardState {
    pub connected: bool,
    pub generation: u64,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct AndroidPointerReport {
    pub x: f64,
    pub y: f64,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct AndroidKeyReport {
    pub pending: bool,
    pub unsupported: bool,
    pub modifiers: u8,
    pub usages: Vec<u8>,
    pub rollover: bool,
}
#[derive(Debug, uniffi::Object)]
pub struct AndroidInputController {
    pointer: Mutex<Option<PointerServo>>,
    keyboard: Mutex<AndroidKeyboard>,
}

#[uniffi::export]
impl AndroidInputController {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pointer: Mutex::new(None),
            keyboard: Mutex::new(AndroidKeyboard::default()),
        })
    }

    pub fn display(&self, value: Option<AndroidDisplayState>) {
        self.keyboard.lock().unwrap().focus(false);
        *self.pointer.lock().unwrap() = value.and_then(|display| PointerServo::new(display.into()));
    }

    pub fn target(&self, point: AndroidPointerPoint, generation: u64) -> bool {
        self.pointer
            .lock()
            .unwrap()
            .as_mut()
            .is_some_and(|pointer| pointer.target(Point::new(point.x, point.y), generation))
    }

    pub fn feedback(&self, point: AndroidPointerPoint, generation: u64) -> Option<AndroidPointerReport> {
        let delta = self
            .pointer
            .lock()
            .unwrap()
            .as_mut()?
            .step(Point::new(point.x, point.y), generation)?;
        Some(AndroidPointerReport { x: delta.x, y: delta.y })
    }

    pub fn cancel(&self) {
        if let Some(pointer) = self.pointer.lock().unwrap().as_mut() {
            pointer.cancel();
        }
        self.keyboard.lock().unwrap().focus(false);
    }

    pub fn focus(&self, active: bool) {
        self.keyboard.lock().unwrap().focus(active);
    }

    pub fn keyboard_ready(&self, ready: bool) {
        let mut keyboard = self.keyboard.lock().unwrap();
        if ready {
            keyboard.device_ready();
        } else {
            keyboard.device_removed();
        }
    }

    pub fn key(&self, page: u16, usage: u16, pressed: bool) -> AndroidKeyReport {
        let result = self.keyboard.lock().unwrap().key(page, usage, pressed);
        let report = result.unwrap_or_default();
        AndroidKeyReport {
            pending: result == Err(KeyboardError::Pending),
            unsupported: result == Err(KeyboardError::Unsupported),
            modifiers: report.modifiers,
            usages: report.usages.to_vec(),
            rollover: result == Err(KeyboardError::Rollover),
        }
    }
}

impl From<AndroidDisplayState> for AndroidDisplay {
    fn from(value: AndroidDisplayState) -> Self {
        Self {
            width: value.width,
            height: value.height,
            rotation: value.rotation,
            generation: value.generation,
        }
    }
}

#[uniffi::export(callback_interface)]
pub trait AndroidInputListener: Send + Sync {
    fn on_input(&self, command: AndroidInputCommand);
}

#[derive(uniffi::Object)]
pub struct MobileAndroidInput {
    publish_display: Box<dyn Fn(Option<AndroidDisplay>) -> bool + Send + Sync>,
    router: Arc<InputRouter>,
    screen: Arc<Screen>,
}

/// The helper's side of the router: focus on this phone's own screen and the
/// router's injections become helper commands, dispatched here in Rust
/// (protocol §14.6) instead of through host polling.
struct Screen {
    listener: Arc<dyn AndroidInputListener>,
    state: Mutex<HelperState>,
}

#[derive(Default)]
struct HelperState {
    helper: Option<Helper>,
    sequence: u64,
    configured_at: u64,
    focused: bool,
    gestures: AndroidGestures,
}

#[derive(Clone, Copy)]
struct Helper {
    display: AndroidDisplay,
    incarnation: [u8; 16],
}

impl std::fmt::Debug for MobileAndroidInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("MobileAndroidInput").finish_non_exhaustive()
    }
}

impl Screen {
    fn emit(&self, state: &mut HelperState, action: AndroidInputAction) {
        let Some(helper) = state.helper else { return };
        state.sequence = state.sequence.saturating_add(1);
        self.listener.on_input(AndroidInputCommand {
            sequence: state.sequence,
            helper_incarnation: helper.incarnation.to_vec(),
            generation: helper.display.generation,
            action,
        });
    }
}

impl ScreenSink for Screen {
    fn group_activity(&self, active: bool) {
        let mut state = self.state.lock().unwrap();
        self.emit(&mut state, AndroidInputAction::GroupActivity { active });
    }

    fn focus(&self, active: bool) {
        let mut state = self.state.lock().unwrap();
        if state.focused == active {
            return;
        }
        state.focused = active;
        state.gestures.cancel();
        if !active {
            self.emit(&mut state, AndroidInputAction::ReleaseAll);
        }
        self.emit(&mut state, AndroidInputAction::Focus { active });
    }

    fn inject(&self, event: InjectEvent) {
        let mut state = self.state.lock().unwrap();
        let Some(helper) = state.helper else { return };
        let action = match event {
            InjectEvent::MoveTo { pos } | InjectEvent::MoveToWithDelta { pos, .. } => {
                bounded(helper.display, pos.x, pos.y).map(|point| motion(point, helper.display.generation))
            }
            InjectEvent::Key { code, pressed } | InjectEvent::KeyWithoutLock { code, pressed } => {
                (code != KeyCode::NONE).then_some(key(code.0, pressed))
            }
            InjectEvent::Button { button, pressed } => match button {
                MouseButton::Left => Some(0),
                MouseButton::Right => Some(1),
                MouseButton::Middle => Some(2),
                _ => None,
            }
            .map(|button| AndroidInputAction::Button { button, pressed }),
            InjectEvent::Scroll { dx, dy, unit, .. } => {
                // The helper emits one report per wheel unit; pixels go 32 to a unit.
                let divisor = if unit == ScrollUnit::Pixel { 32.0 } else { 1.0 };
                let (horizontal, vertical) = (-dx / divisor, dy / divisor);
                (horizontal.is_finite() && vertical.is_finite()).then_some(AndroidInputAction::Scroll {
                    horizontal: horizontal.clamp(-128.0, 128.0),
                    vertical: vertical.clamp(-128.0, 128.0),
                })
            }
            InjectEvent::Gesture(gesture) if state.focused => {
                state
                    .gestures
                    .apply(gesture)
                    .map(|gesture| AndroidInputAction::SystemGesture {
                        gesture: match gesture {
                            SystemGesture::Home => AndroidSystemGesture::Home,
                            SystemGesture::Recents => AndroidSystemGesture::Recents,
                            SystemGesture::SwitchNext => AndroidSystemGesture::SwitchNext,
                            SystemGesture::SwitchPrevious => AndroidSystemGesture::SwitchPrevious,
                            SystemGesture::Notifications => AndroidSystemGesture::Notifications,
                        },
                    })
            }
            // No helper channel for text.
            _ => None,
        };
        if let Some(action) = action {
            self.emit(&mut state, action);
        }
    }

    fn release_all(&self) {
        let mut state = self.state.lock().unwrap();
        state.gestures.cancel();
        self.emit(&mut state, AndroidInputAction::ReleaseAll);
    }
}

#[uniffi::export]
impl MobileAndroidInput {
    pub fn acknowledge(&self, sequence: u64, success: bool) {
        let mut state = self.screen.state.lock().unwrap();
        if !success && sequence > state.configured_at && sequence <= state.sequence {
            // The helper dropped a command: nothing it holds may stay pressed.
            state.gestures.cancel();
            self.screen.emit(&mut state, AndroidInputAction::ReleaseAll);
        }
    }

    pub fn configure(&self, display: Option<AndroidDisplayState>, _path: AndroidPointerPath, incarnation: Vec<u8>) {
        let helper = display
            .map(AndroidDisplay::from)
            .filter(|display| display.valid())
            .zip(incarnation.try_into().ok())
            .map(|(display, incarnation)| Helper { display, incarnation });
        let published = (self.publish_display)(helper.map(|helper| helper.display));
        let mut state = self.screen.state.lock().unwrap();
        if let Some(helper) = helper.filter(|_| published) {
            state.helper = Some(helper);
            state.configured_at = state.sequence;
            state.focused = false;
            state.gestures.cancel();
            self.screen.emit(&mut state, AndroidInputAction::Ready { ready: true });
            drop(state);
            let display = DisplayInfo {
                key: ANDROID_INPUT_DISPLAY.into(),
                name: "Android".into(),
                bounds: oink_foundation::Rect::new(
                    0.0,
                    0.0,
                    f64::from(helper.display.width),
                    f64::from(helper.display.height),
                ),
                pixel_width: helper.display.width,
                pixel_height: helper.display.height,
                reference_extent: None,
                rotation_deg: helper.display.rotation,
                is_primary: true,
                is_builtin: true,
                is_virtual: false,
            };
            self.router.set_screen(Some((display, self.screen.clone())));
        } else if state.helper.is_some() {
            self.screen.emit(&mut state, AndroidInputAction::Ready { ready: false });
            state.helper = None;
            state.focused = false;
            state.gestures.cancel();
            drop(state);
            self.router.set_screen(None);
        }
    }

    pub fn actual_position(&self, _point: AndroidPointerPoint, _generation: u64) {
        // Native feedback belongs to AndroidInputController's servo. Routed
        // deltas accumulate at the target, independent of helper convergence.
    }

    pub fn shutdown(&self) {
        self.configure(None, AndroidPointerPath::VirtualMouse, Vec::new());
    }
}

impl MobileAndroidInput {
    pub(crate) fn new(
        listener: Box<dyn AndroidInputListener>,
        router: Arc<InputRouter>,
        publish_display: impl Fn(Option<AndroidDisplay>) -> bool + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            publish_display: Box::new(publish_display),
            router,
            screen: Arc::new(Screen {
                listener: Arc::from(listener),
                state: Mutex::new(HelperState::default()),
            }),
        })
    }
}

fn bounded(display: AndroidDisplay, x: f64, y: f64) -> Option<Point> {
    (x.is_finite() && y.is_finite()).then(|| {
        Point::new(
            x.clamp(0.0, f64::from(display.width - 1)),
            y.clamp(0.0, f64::from(display.height - 1)),
        )
    })
}

fn motion(point: Point, generation: u64) -> AndroidInputAction {
    AndroidInputAction::Move {
        x: point.x,
        y: point.y,
        generation,
    }
}

fn key(code: u32, pressed: bool) -> AndroidInputAction {
    let code = KeyCode(code);
    AndroidInputAction::Key {
        page: code.page(),
        usage: code.usage(),
        pressed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default, Clone)]
    struct Commands(Arc<Mutex<Vec<AndroidInputCommand>>>);

    impl AndroidInputListener for Commands {
        fn on_input(&self, command: AndroidInputCommand) {
            self.0.lock().unwrap().push(command);
        }
    }

    fn screen(commands: &Commands) -> Screen {
        let screen = Screen {
            listener: Arc::new(commands.clone()),
            state: Mutex::new(HelperState::default()),
        };
        screen.state.lock().unwrap().helper = Some(Helper {
            display: AndroidDisplay {
                width: 200,
                height: 400,
                rotation: 0,
                generation: 7,
            },
            incarnation: [3; 16],
        });
        screen
    }

    #[test]
    fn group_activity_reaches_helper_without_taking_focus() {
        let commands = Commands::default();
        let screen = screen(&commands);
        screen.group_activity(true);
        screen.group_activity(false);
        let commands = commands.0.lock().unwrap();
        assert!(matches!(
            commands[0].action,
            AndroidInputAction::GroupActivity { active: true }
        ));
        assert!(matches!(
            commands[1].action,
            AndroidInputAction::GroupActivity { active: false }
        ));
        assert!(!screen.state.lock().unwrap().focused);
    }

    #[test]
    fn router_injections_become_helper_commands() {
        let commands = Commands::default();
        let screen = screen(&commands);
        screen.focus(true);
        screen.inject(InjectEvent::MoveTo {
            pos: Point::new(500.0, 10.0),
        });
        screen.inject(InjectEvent::Key {
            code: KeyCode::keyboard(0x04),
            pressed: true,
        });
        screen.inject(InjectEvent::Scroll {
            dx: 0.0,
            dy: 64.0,
            unit: ScrollUnit::Pixel,
            phase: oink_foundation::input::ScrollPhase::None,
        });
        screen.focus(false);
        let commands = commands.0.lock().unwrap();
        let actions: Vec<_> = commands.iter().map(|c| c.action).collect();
        assert!(matches!(actions[0], AndroidInputAction::Focus { active: true }));
        // Clamped to the helper display.
        assert!(matches!(actions[1], AndroidInputAction::Move { x, y, generation: 7 } if x == 199.0 && y == 10.0));
        assert!(matches!(
            actions[2],
            AndroidInputAction::Key {
                page: 7,
                usage: 4,
                pressed: true
            }
        ));
        assert!(matches!(actions[3], AndroidInputAction::Scroll { vertical, .. } if vertical == 2.0));
        assert!(matches!(actions[4], AndroidInputAction::ReleaseAll));
        assert!(matches!(actions[5], AndroidInputAction::Focus { active: false }));
        assert!(commands.iter().all(|c| c.helper_incarnation == vec![3; 16]));
        assert!(commands.windows(2).all(|w| w[1].sequence == w[0].sequence + 1));
    }
    #[test]
    fn completed_gestures_are_fenced_and_cancelled_with_focus() {
        use oink_foundation::input::{Gesture, GesturePhase, SwipeAxis};
        let commands = Commands::default();
        let screen = screen(&commands);
        screen.focus(true);
        let gesture = |phase| {
            InjectEvent::Gesture(Gesture::DockSwipe {
                axis: SwipeAxis::Pinch,
                phase,
                progress: -0.7,
                velocity: 0.0,
            })
        };
        screen.inject(gesture(GesturePhase::Began));
        screen.release_all();
        screen.inject(gesture(GesturePhase::Ended));
        screen.inject(gesture(GesturePhase::Began));
        screen.inject(gesture(GesturePhase::Ended));
        screen.inject(gesture(GesturePhase::Ended));
        screen.focus(false);
        screen.inject(gesture(GesturePhase::Began));
        screen.inject(gesture(GesturePhase::Ended));
        let actions = commands.0.lock().unwrap();
        assert_eq!(
            actions
                .iter()
                .filter(|c| matches!(
                    c.action,
                    AndroidInputAction::SystemGesture {
                        gesture: AndroidSystemGesture::Home
                    }
                ))
                .count(),
            1
        );
        assert!(
            actions
                .iter()
                .all(|c| c.generation == 7 && c.helper_incarnation == vec![3; 16])
        );
        assert!(matches!(
            actions.last().unwrap().action,
            AndroidInputAction::Focus { active: false }
        ));
    }
}
