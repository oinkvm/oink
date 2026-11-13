//! The control page (protocol §14.6): this phone as a screenless input
//! source in the group's focus session, sending what a Mac trackpad source
//! sends. The host keeps one-finger input and taps; the multi-finger
//! recognition (`recognize`, shared by iOS and Android), pointer and scroll
//! acceleration and scroll momentum (the Mac trackpad's) are done here.
use super::*;
use oink_foundation::input::{Gesture, GesturePhase, SwipeAxis};
use oink_mobile_core::{
    ControlError, ControlObserver, ControlSettings, ControlState, FingerPhase, InputRouter, PadTouch, PadTouches,
    trackpad::gestures::{Claim, Finger, Recognized, Recognizer},
};
use oink_routing::{
    GameMode,
    control::{EdgeSide, EdgeState},
};

#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct MobileControlSettings {
    /// 1..=10: the Mac trackpad's tracking-speed step, default 4.
    pub pointer_speed: u8,
    /// Host coordinate units (iOS points, Android dp) per millimetre.
    pub points_per_mm: f32,
}

/// The phase of a touchpad scroll of the fingers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ControlScrollPhase {
    /// The fingers start scrolling (first movement past the slop).
    Began,
    Changed,
    /// The fingers lifted (or fewer fingers remain): momentum may follow.
    Ended,
    /// The scroll became something else (more fingers): no momentum.
    Cancelled,
}

/// One finger on the pad, millimetres from its top left.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct ControlTouch {
    /// The host's touch id, stable while the finger touches.
    pub id: u64,
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ControlGesturePhase {
    Began,
    Changed,
    Ended,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ControlSwipeAxis {
    Horizontal,
    Vertical,
    Pinch,
}

/// A recognised touchpad gesture, with the Mac trackpad's semantics
/// (`oink_foundation::input::Gesture`).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Enum)]
pub enum ControlGesture {
    /// Three / four finger swipe, four / five finger pinch. `progress`:
    /// accumulated since Began (1.0 ≈ one full switch); horizontal positive =
    /// fingers move left, vertical negative = fingers move up, pinch negative =
    /// fingers close. `velocity`: progress per second, at the end.
    DockSwipe {
        axis: ControlSwipeAxis,
        phase: ControlGesturePhase,
        progress: f64,
        velocity: f64,
    },
    /// Two finger pinch: this step's magnification (0.1 = 10 % larger).
    Magnify { phase: ControlGesturePhase, amount: f64 },
    /// Two finger rotation: this step's degrees, counter-clockwise positive.
    Rotate { phase: ControlGesturePhase, degrees: f64 },
    /// Two finger double tap.
    SmartMagnify,
}

/// What the core's touchpad recognizer does with the current touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ControlTouchClaim {
    /// Nothing (yet): the touch is the page's (pointer, taps, drag).
    Unclaimed,
    /// A two-finger scroll (the page shows the press under the fingers).
    Scroll,
    /// A gesture, or a touch whose gesture is over: the page does nothing
    /// more with this touch (its remaining fingers move no pointer, it is no
    /// tap).
    Gesture,
}

/// One output of the core's touchpad recognizer. The page does not read it:
/// it passes it to `send_recognized` as it is.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Enum)]
pub enum ControlRecognized {
    Gesture {
        gesture: ControlGesture,
    },
    /// A two-finger scroll of the fingers, millimetres, finger direction.
    Scroll {
        phase: ControlScrollPhase,
        dx: f64,
        dy: f64,
        time_us: u64,
        since_us: u64,
    },
}

/// The core's touchpad recognizer on one touch frame.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ControlRecognition {
    pub claim: ControlTouchClaim,
    /// To `send_recognized`, after what the page sends itself for the same
    /// touch event.
    pub outputs: Vec<ControlRecognized>,
}

fn control_phase(phase: GesturePhase) -> ControlGesturePhase {
    match phase {
        GesturePhase::Began => ControlGesturePhase::Began,
        GesturePhase::Changed => ControlGesturePhase::Changed,
        GesturePhase::Ended => ControlGesturePhase::Ended,
        GesturePhase::Cancelled => ControlGesturePhase::Cancelled,
    }
}

fn control_gesture(gesture: Gesture) -> ControlGesture {
    match gesture {
        Gesture::DockSwipe {
            axis,
            phase,
            progress,
            velocity,
        } => ControlGesture::DockSwipe {
            axis: match axis {
                SwipeAxis::Horizontal => ControlSwipeAxis::Horizontal,
                SwipeAxis::Vertical => ControlSwipeAxis::Vertical,
                SwipeAxis::Pinch => ControlSwipeAxis::Pinch,
            },
            phase: control_phase(phase),
            progress,
            velocity,
        },
        Gesture::Magnify { phase, amount } => ControlGesture::Magnify {
            phase: control_phase(phase),
            amount,
        },
        Gesture::Rotate { phase, degrees } => ControlGesture::Rotate {
            phase: control_phase(phase),
            degrees,
        },
        Gesture::SmartMagnify => ControlGesture::SmartMagnify,
    }
}

fn recognized(output: Recognized) -> ControlRecognized {
    match output {
        Recognized::Gesture(gesture) => ControlRecognized::Gesture {
            gesture: control_gesture(gesture),
        },
        Recognized::Scroll {
            phase,
            dx,
            dy,
            time_us,
            since_us,
        } => ControlRecognized::Scroll {
            phase: match phase {
                FingerPhase::Began => ControlScrollPhase::Began,
                FingerPhase::Changed => ControlScrollPhase::Changed,
                FingerPhase::Ended => ControlScrollPhase::Ended,
                FingerPhase::Cancelled => ControlScrollPhase::Cancelled,
            },
            dx,
            dy,
            time_us,
            since_us,
        },
    }
}

fn finger_phase(phase: ControlScrollPhase) -> FingerPhase {
    match phase {
        ControlScrollPhase::Began => FingerPhase::Began,
        ControlScrollPhase::Changed => FingerPhase::Changed,
        ControlScrollPhase::Ended => FingerPhase::Ended,
        ControlScrollPhase::Cancelled => FingerPhase::Cancelled,
    }
}

fn gesture_phase(phase: ControlGesturePhase) -> GesturePhase {
    match phase {
        ControlGesturePhase::Began => GesturePhase::Began,
        ControlGesturePhase::Changed => GesturePhase::Changed,
        ControlGesturePhase::Ended => GesturePhase::Ended,
        ControlGesturePhase::Cancelled => GesturePhase::Cancelled,
    }
}

fn gesture(gesture: ControlGesture) -> Option<Gesture> {
    let finite = |v: f64| v.is_finite().then_some(v);
    Some(match gesture {
        ControlGesture::DockSwipe {
            axis,
            phase,
            progress,
            velocity,
        } => Gesture::DockSwipe {
            axis: match axis {
                ControlSwipeAxis::Horizontal => SwipeAxis::Horizontal,
                ControlSwipeAxis::Vertical => SwipeAxis::Vertical,
                ControlSwipeAxis::Pinch => SwipeAxis::Pinch,
            },
            phase: gesture_phase(phase),
            progress: finite(progress)?,
            velocity: finite(velocity)?,
        },
        ControlGesture::Magnify { phase, amount } => Gesture::Magnify {
            phase: gesture_phase(phase),
            amount: finite(amount)?,
        },
        ControlGesture::Rotate { phase, degrees } => Gesture::Rotate {
            phase: gesture_phase(phase),
            degrees: finite(degrees)?,
        },
        ControlGesture::SmartMagnify => Gesture::SmartMagnify,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ControlButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ControlGameMode {
    Off,
    On,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ControlTarget {
    /// 64-hex device id.
    pub device: String,
    pub name: String,
    /// "macos", "windows", "android" (the peer's DeviceFacts OS).
    pub platform: String,
    pub online: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MobileControlState {
    /// The current focus holder this phone's input goes to; None = no target.
    pub focus_device: Option<String>,
    pub selected_device: Option<String>,
    /// The latest input was dropped: no focus and nothing selected.
    pub no_target: bool,
    pub locked: bool,
    pub game_mode: ControlGameMode,
    pub game_active: bool,
    pub targets: Vec<ControlTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ControlEdgeState {
    Clear,
    Blocked,
    Approaching,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ControlEdgeSide {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ControlEdge {
    pub state: ControlEdgeState,
    pub edge: ControlEdgeSide,
    pub to_device: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ControlTextRefusal {
    /// The target paused sharing.
    Paused,
    /// The target has not joined the focus session (its own pointer is local).
    NotJoined,
    Duplicate,
    /// The target cannot insert text (an Android phone).
    Unsupported,
}

/// A focus computer's text-input focus (protocol §14.7). Never any text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ControlTextFocus {
    /// Input-focus episode: increases once per keyboard focus into an editable element.
    pub seq: u64,
    /// An editable text element has keyboard focus.
    pub editable: bool,
    /// That element is a password field.
    pub secure: bool,
}

/// The modifiers the user holds down (held or locked) on the control page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Record)]
pub struct ControlModifiersDown {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// ⌘ / Windows key.
    pub meta: bool,
}

/// What one character key of the emulated keyboard sends (docs/control-page.md
/// §3.1.2, 调节 › 键盘布局).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ControlTypedChar {
    /// `code` ((HID page << 16) | usage) goes down, and up when the finger
    /// lifts. `auto_shift`: left Shift goes down before it and up after it,
    /// around just this key. `auto_alt`: that key (right Alt = AltGr on
    /// Windows, left Alt = Option on a Mac) goes around it the same way, inside
    /// the Shift. `dead`: the key is tapped at once (down + up inside its
    /// Shift / Alt) followed by a Space tap, and is neither held nor repeated.
    Key {
        code: u32,
        auto_shift: bool,
        auto_alt: Option<u32>,
        dead: bool,
    },
    /// No certain key on the layout: sent with `input_text` instead.
    Text { text: String },
}

/// The key a character key of the emulated keyboard sends. `layout`: the
/// 键盘布局 setting ("auto", "us", "uk", "de", "fr", "es", "jis", "ko"; anything
/// else is US). `target_mac`: the focus computer is a Mac (its control target's
/// platform is "macos"). `ch`: the key's character without Shift (one
/// character; anything else goes as text). `modifiers`: the page's modifiers
/// that are down.
#[uniffi::export]
pub fn control_typed_char(
    layout: String,
    target_mac: bool,
    ch: String,
    modifiers: ControlModifiersDown,
) -> ControlTypedChar {
    use oink_mobile_core::key_layouts::{ModifiersDown, TypedChar, typed_char};
    let mut chars = ch.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return ControlTypedChar::Text { text: ch };
    };
    let mods = ModifiersDown {
        shift: modifiers.shift,
        ctrl: modifiers.ctrl,
        alt: modifiers.alt,
        meta: modifiers.meta,
    };
    match typed_char(&layout, target_mac, c, mods) {
        TypedChar::Key {
            code,
            auto_shift,
            auto_alt,
            dead,
        } => ControlTypedChar::Key {
            code,
            auto_shift,
            auto_alt,
            dead,
        },
        TypedChar::Text(text) => ControlTypedChar::Text { text },
    }
}

/// Called on the router thread; hosts move to their UI thread themselves.
#[uniffi::export(callback_interface)]
pub trait MobileControlListener: Send + Sync {
    fn on_state(&self, state: MobileControlState);
    fn on_edge(&self, edge: ControlEdge);
    /// A text sent with `input_text` was not inserted by the focus holder.
    fn on_text_refused(&self, reason: ControlTextRefusal);
    /// The text-input focus of `device` (64-hex), the focus holder this
    /// phone's input goes to (protocol §14.7): on every change while this
    /// handle is open, and the current state right after it opens. The page
    /// pops its keyboard up or hides it (control-page spec §9).
    fn on_text_focus(&self, device: String, focus: ControlTextFocus);
}

struct Observer(Box<dyn MobileControlListener>);

impl ControlObserver for Observer {
    fn state(&self, state: ControlState) {
        self.0.on_state(MobileControlState {
            focus_device: state.focus_device.map(|d| d.to_hex()),
            selected_device: state.selected_device.map(|d| d.to_hex()),
            no_target: state.no_target,
            locked: state.locked,
            game_mode: match state.game_mode {
                GameMode::Off => ControlGameMode::Off,
                GameMode::On => ControlGameMode::On,
                GameMode::Auto => ControlGameMode::Auto,
            },
            game_active: state.game_active,
            targets: state
                .targets
                .into_iter()
                .map(|t| ControlTarget {
                    device: t.device.to_hex(),
                    name: t.name,
                    platform: t.platform,
                    online: t.online,
                })
                .collect(),
        });
    }
    fn text_refused(&self, reason: oink_routing::control::TextRefusal) {
        use oink_routing::control::TextRefusal as R;
        self.0.on_text_refused(match reason {
            R::Paused => ControlTextRefusal::Paused,
            R::NotJoined => ControlTextRefusal::NotJoined,
            R::Duplicate => ControlTextRefusal::Duplicate,
            R::Unsupported => ControlTextRefusal::Unsupported,
        });
    }
    fn text_focus(&self, device: oink_foundation::DeviceId, state: oink_routing::control::TextFocusState) {
        self.0.on_text_focus(
            device.to_hex(),
            ControlTextFocus {
                seq: state.seq,
                editable: state.editable,
                secure: state.secure,
            },
        );
    }
    fn edge(&self, report: oink_mobile_core::ControlEdge) {
        self.0.on_edge(ControlEdge {
            state: match report.state {
                EdgeState::Clear => ControlEdgeState::Clear,
                EdgeState::Blocked => ControlEdgeState::Blocked,
                EdgeState::Approaching => ControlEdgeState::Approaching,
            },
            edge: match report.edge {
                EdgeSide::Left => ControlEdgeSide::Left,
                EdgeSide::Right => ControlEdgeSide::Right,
                EdgeSide::Top => ControlEdgeSide::Top,
                EdgeSide::Bottom => ControlEdgeSide::Bottom,
            },
            to_device: report.to_device.map(|d| d.to_hex()),
        });
    }
}

#[derive(uniffi::Object)]
pub struct MobileControl {
    input: Arc<InputRouter>,
    /// The page's multi-finger recognizer (`recognize`).
    gestures: std::sync::Mutex<Recognizer>,
    /// Identifies this handle: a newer `control()` replaces the observer.
    generation: u64,
    current: Arc<std::sync::atomic::AtomicU64>,
}

impl std::fmt::Debug for MobileControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobileControl").finish_non_exhaustive()
    }
}

fn control_error(error: ControlError) -> MobileError {
    match error {
        ControlError::InvalidText => MobileError::InvalidArgument {
            detail: "text is empty, longer than 4096 bytes, or contains control characters".into(),
        },
        ControlError::SendFailed => MobileError::Command {
            code: UserFacingCode::LinkLost,
            detail: "the input stream did not take the text; nothing was sent".into(),
        },
        ControlError::NoTarget => MobileError::Command {
            code: UserFacingCode::DeviceUnavailable,
            detail: "no focus to send to".into(),
        },
        ControlError::Paused => MobileError::Command {
            code: UserFacingCode::StateConflict,
            detail: "input is paused".into(),
        },
        ControlError::Stopped => MobileError::Stopped,
    }
}

/// A 64-hex device id from the host.
pub(super) fn parse_device_id(device: &str) -> Result<oink_foundation::DeviceId, MobileError> {
    let bytes = (device.len() == 64)
        .then(|| {
            (0..32)
                .map(|index| u8::from_str_radix(device.get(index * 2..index * 2 + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()
        })
        .flatten()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| MobileError::InvalidArgument {
            detail: "device must be a 32-byte hex id".into(),
        })?;
    Ok(oink_foundation::DeviceId(bytes))
}

impl MobileControl {
    fn live(&self) -> bool {
        self.current.load(std::sync::atomic::Ordering::Acquire) == self.generation
    }
}

#[uniffi::export]
impl MobileControl {
    pub fn set_settings(&self, settings: MobileControlSettings) {
        self.input.set_settings(ControlSettings {
            pointer_speed: settings.pointer_speed.clamp(1, 10),
            points_per_mm: settings.points_per_mm,
        });
    }
    /// One combined pointer movement per touch event, in host units. `time_us` is the touch
    /// timestamp of this event and `since_us` the one the movement is measured from (the
    /// previous pointer movement of this touch, or the touch-down for its first), both on the
    /// phone's uptime clock in microseconds; the finger speed for acceleration comes from them.
    pub fn pointer_move(&self, dx: f32, dy: f32, time_us: u64, since_us: u64) {
        if self.live() && dx.is_finite() && dy.is_finite() {
            self.input.pointer_move(dx, dy, time_us, since_us);
        }
    }
    pub fn button(&self, button: ControlButton, down: bool) {
        if self.live() {
            self.input.button(
                match button {
                    ControlButton::Left => oink_foundation::input::MouseButton::Left,
                    ControlButton::Right => oink_foundation::input::MouseButton::Right,
                    ControlButton::Middle => oink_foundation::input::MouseButton::Middle,
                },
                down,
            );
        }
    }
    /// One touchpad scroll event of the fingers in host units, finger
    /// direction; `time_us` / `since_us` as for `pointer_move`. Ended / Cancelled
    /// carry the last movement (usually none). Momentum is generated here.
    pub fn scroll(&self, phase: ControlScrollPhase, dx: f32, dy: f32, time_us: u64, since_us: u64) {
        if self.live() && dx.is_finite() && dy.is_finite() {
            self.input.scroll(finger_phase(phase), dx, dy, time_us, since_us);
        }
    }
    /// Every finger on the pad after each touch event (down, move, up; an
    /// empty list when the last finger lifts), the pad's size in millimetres
    /// and the touch time (uptime, µs). `cancelled`: the system took the
    /// touches away. Sent as the trackpad's raw contacts; a new touch also
    /// stops scroll momentum.
    pub fn touches(&self, time_us: u64, width: f32, height: f32, touches: Vec<ControlTouch>, cancelled: bool) {
        let finite =
            width.is_finite() && height.is_finite() && touches.iter().all(|t| t.x.is_finite() && t.y.is_finite());
        if self.live() && finite {
            self.input.touches(PadTouches {
                time_us,
                width,
                height,
                touches: touches
                    .into_iter()
                    .map(|t| PadTouch {
                        id: t.id,
                        x: t.x,
                        y: t.y,
                    })
                    .collect(),
                cancelled,
            });
        }
    }
    /// The shared multi-finger recognizer (the same for iOS and Android) on
    /// one touch frame: every finger on the pad after a touch event,
    /// millimetres from its top left (none when the last one lifted), at the
    /// touch time (uptime, µs) — the same list as `touches`. `cancelled`: the
    /// system took the touches away. `multi_finger`: the page leaves this
    /// touch's fingers to the core; false while it drags, scrolls with one
    /// finger or moves the pointer with two (单指划动 = 滚动), or ignores the
    /// touch. Two or more fingers become a two-finger scroll, magnify, rotate
    /// or a Dock gesture (three / four finger swipe, four / five finger pinch)
    /// as a Mac trackpad's driver decides them. Answers at once on the
    /// calling thread and sends nothing: the page acts on `claim` and passes
    /// `outputs` to `send_recognized` after what it sends itself for the same
    /// event (a held-back right click goes first).
    pub fn recognize(
        &self,
        time_us: u64,
        touches: Vec<ControlTouch>,
        cancelled: bool,
        multi_finger: bool,
    ) -> ControlRecognition {
        if !self.live() || !touches.iter().all(|t| t.x.is_finite() && t.y.is_finite()) {
            return ControlRecognition {
                claim: ControlTouchClaim::Unclaimed,
                outputs: Vec::new(),
            };
        }
        let fingers: Vec<Finger> = touches
            .iter()
            .map(|t| Finger {
                id: t.id,
                x: f64::from(t.x),
                y: f64::from(t.y),
            })
            .collect();
        let mut out = Vec::new();
        let claim = self.gestures.lock().unwrap_or_else(|e| e.into_inner()).frame(
            time_us,
            &fingers,
            cancelled,
            multi_finger,
            &mut out,
        );
        ControlRecognition {
            claim: match claim {
                Claim::Unclaimed => ControlTouchClaim::Unclaimed,
                Claim::Scroll => ControlTouchClaim::Scroll,
                Claim::Gesture => ControlTouchClaim::Gesture,
            },
            outputs: out.into_iter().map(recognized).collect(),
        }
    }
    /// Sends `recognize`'s outputs in order, as a Mac trackpad source sends
    /// them: the gestures as system gestures, the two-finger scroll
    /// accelerated with the Mac's momentum after it ends.
    pub fn send_recognized(&self, outputs: Vec<ControlRecognized>) {
        if !self.live() {
            return;
        }
        for output in outputs {
            match output {
                ControlRecognized::Gesture { gesture } => {
                    if let Some(gesture) = self::gesture(gesture) {
                        self.input.gesture(gesture);
                    }
                }
                ControlRecognized::Scroll {
                    phase,
                    dx,
                    dy,
                    time_us,
                    since_us,
                } => {
                    if dx.is_finite() && dy.is_finite() {
                        self.input.scroll_mm(finger_phase(phase), dx, dy, time_us, since_us);
                    }
                }
            }
        }
    }
    /// A gesture the page recognised itself: the two-finger double tap's
    /// smart zoom (the rest come from `recognize`).
    pub fn gesture(&self, gesture: ControlGesture) {
        if self.live()
            && let Some(gesture) = self::gesture(gesture)
        {
            self.input.gesture(gesture);
        }
    }
    /// `code` = (HID page << 16) | usage.
    pub fn key(&self, code: u32, down: bool) {
        if self.live() {
            self.input.key(code, down);
        }
    }
    pub fn input_text(&self, text: String) -> Result<(), MobileError> {
        self.input.input_text(text).map_err(control_error)
    }
    pub fn move_pointer_to(&self, device: String) -> Result<(), MobileError> {
        let device = parse_device_id(&device)?;
        self.input.move_pointer_to(device).map_err(control_error)
    }
    pub fn set_focus_lock(&self, locked: bool) -> Result<(), MobileError> {
        self.input.set_focus_lock(locked).map_err(control_error)
    }
    pub fn set_game_mode(&self, mode: ControlGameMode) -> Result<(), MobileError> {
        self.input
            .set_game_mode(match mode {
                ControlGameMode::Off => GameMode::Off,
                ControlGameMode::On => GameMode::On,
                ControlGameMode::Auto => GameMode::Auto,
            })
            .map_err(control_error)
    }
    /// Leaving the control page: everything this phone holds is released.
    /// Not named `close`: UniFFI's Kotlin objects already have `close()`
    /// (AutoCloseable, frees the handle), and a second one does not compile.
    pub fn leave(&self) {
        if self.live() {
            self.input.set_observer(None);
        }
    }
}

impl MobileSession {
    pub(super) fn open_control(&self, listener: Box<dyn MobileControlListener>) -> Arc<MobileControl> {
        let generation = self
            .control_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1;
        self.input.set_observer(Some(Arc::new(Observer(listener))));
        Arc::new(MobileControl {
            input: self.input.clone(),
            gestures: Default::default(),
            generation,
            current: self.control_generation.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ControlModifiersDown = ControlModifiersDown {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
    };

    #[test]
    fn typed_char_maps_the_cores_answer() {
        // German on Windows: @ = AltGr (right Alt, 0xE6) + Q.
        assert_eq!(
            ControlTypedChar::Key {
                code: 0x07_0014,
                auto_shift: false,
                auto_alt: Some(0x07_00E6),
                dead: false
            },
            control_typed_char("de".into(), false, "@".into(), NONE)
        );
        // French on a Mac has no certain key for [: it goes as text.
        assert_eq!(
            ControlTypedChar::Text { text: "[".into() },
            control_typed_char("fr".into(), true, "[".into(), NONE)
        );
        // The modifiers reach the core: with Shift down the t key types T on its own key.
        assert_eq!(
            ControlTypedChar::Key {
                code: 0x07_0017,
                auto_shift: false,
                auto_alt: None,
                dead: false
            },
            control_typed_char(
                "us".into(),
                false,
                "t".into(),
                ControlModifiersDown { shift: true, ..NONE }
            )
        );
    }

    /// The recognizer's gestures cross the FFI and come back to the wire
    /// unchanged (the page passes them on without reading them).
    #[test]
    fn recognised_gestures_round_trip_through_the_ffi() {
        for g in [
            Gesture::DockSwipe {
                axis: SwipeAxis::Pinch,
                phase: GesturePhase::Ended,
                progress: -0.4,
                velocity: -2.5,
            },
            Gesture::DockSwipe {
                axis: SwipeAxis::Horizontal,
                phase: GesturePhase::Cancelled,
                progress: 0.3,
                velocity: 0.1,
            },
            Gesture::Magnify {
                phase: GesturePhase::Changed,
                amount: 0.05,
            },
            Gesture::Rotate {
                phase: GesturePhase::Began,
                degrees: -4.0,
            },
        ] {
            let ControlRecognized::Gesture { gesture: control } = recognized(Recognized::Gesture(g)) else {
                panic!("{g:?}");
            };
            assert_eq!(gesture(control), Some(g));
        }
        let scroll = recognized(Recognized::Scroll {
            phase: FingerPhase::Ended,
            dx: 1.5,
            dy: -2.0,
            time_us: 20,
            since_us: 10,
        });
        assert_eq!(
            scroll,
            ControlRecognized::Scroll {
                phase: ControlScrollPhase::Ended,
                dx: 1.5,
                dy: -2.0,
                time_us: 20,
                since_us: 10
            }
        );
        let ControlRecognized::Scroll { phase, .. } = scroll else {
            unreachable!()
        };
        assert_eq!(finger_phase(phase), FingerPhase::Ended);
    }

    #[test]
    fn anything_but_one_character_goes_as_text() {
        assert_eq!(
            ControlTypedChar::Text { text: "ab".into() },
            control_typed_char("us".into(), false, "ab".into(), NONE)
        );
        assert_eq!(
            ControlTypedChar::Text { text: String::new() },
            control_typed_char("us".into(), false, String::new(), NONE)
        );
    }
}
