//! Phone input sources (protocol §14). A privileged Android phone also has one
//! controllable screen (`android:built-in`) in the shared layout. Edge crossing
//! and explicit pointer moves use that same topology and virtual pointer.
//!
//! `holder == me` means "no target" here, unless focus was entered
//! on that Android screen (`screen_focus`). The member never claims focus,
//! never emits capture-mode or cursor-placement outputs for a cursor it does
//! not have, and joins the group pointer on its first input:
//! - a valid group focus exists (holder reachable, its instance in the scope):
//!   become its source and add this instance with `FocusResume`;
//! - otherwise move the pointer to the device chosen in the control page
//!   (`MovePointerEnter`, epoch + 1), landing at the centre of its primary
//!   screen from its DeviceFacts; with nothing chosen the input is dropped
//!   and `StatusEvent::NoTarget` tells the control page.
use super::*;

/// One input from the control page. Motion is in pointer units (1 = one logical
/// point on the pointer base computer's primary screen). The touchpad sends
/// what a Mac trackpad source sends: phased touchpad scroll in pixels (already
/// accelerated, momentum included), system gestures and raw contacts.
#[derive(Clone, Debug, PartialEq)]
pub enum MemberInput {
    Motion {
        dx: f64,
        dy: f64,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    Key {
        code: KeyCode,
        pressed: bool,
    },
    Scroll {
        dx: f64,
        dy: f64,
        phase: ScrollPhase,
    },
    Gesture(Gesture),
    /// Every finger on the pad after a touch event (also the empty lift).
    Contacts(oink_foundation::contacts::Sample),
    Text(String),
}

/// The pointer base computer's primary screen (protocol §14.3) and its system
/// scale: pointer units are logical points there.
#[derive(Clone, Debug, PartialEq)]
pub struct PointerBase {
    pub screen: ScreenId,
    pub system_scale: f64,
}

impl Router {
    pub fn screenless(&self) -> bool {
        self.cfg.screenless
    }

    /// The device chosen in the control page's device selector.
    pub fn set_selected_target(&mut self, target: Option<DeviceId>) {
        self.selected_target = target.filter(|t| *t != self.me());
    }

    pub fn selected_target(&self) -> Option<DeviceId> {
        self.selected_target
    }

    /// The focus holder this member's input currently goes to.
    pub fn member_target(&self) -> Option<DeviceId> {
        (self.holder != self.me() && self.input_ready(self.holder)).then_some(self.holder)
    }

    /// The speaker exit the control page's volume keys act on: the probe's
    /// context for this Boot, pinned to the exit's current Boot, either this
    /// phone itself (it plays the group's sound) or an admitted, reachable peer.
    pub(super) fn member_volume_exit(&self) -> Option<crate::input::VolumeRoute> {
        self.volume_exit.as_ref().and_then(|probe| probe()).filter(|route| {
            route.source == self.cfg.boot
                && (route.target == self.cfg.boot
                    || (route.target.device_id != self.me()
                        && self.admitted_boots.get(&route.target.device_id) == Some(&route.target)
                        && self.reachable(route.target.device_id)))
        })
    }

    /// This screenless member holds focus on its own (Android) screen.
    pub(super) fn screen_focus_active(&self) -> bool {
        self.screen_focus && self.holder == self.me()
    }

    /// Route one control-page input. Returns whether it reached a target
    /// (releases of presses that went nowhere also return false).
    pub fn member_input(&mut self, input: MemberInput, now: Micros) -> bool {
        debug_assert!(self.cfg.screenless);
        if self.paused || self.fully_paused {
            return false;
        }
        let activity = match &input {
            MemberInput::Motion { dx, dy } => dx.is_finite() && dy.is_finite() && (*dx != 0. || *dy != 0.),
            MemberInput::Scroll { dx, dy, phase } => {
                dx.is_finite() && dy.is_finite() && (*dx != 0. || *dy != 0. || *phase == ScrollPhase::Began)
            }
            MemberInput::Gesture(gesture) => gesture.starts(),
            // A finger resting on the pad chooses nothing.
            MemberInput::Contacts(_) => false,
            MemberInput::Button { pressed, .. } | MemberInput::Key { pressed, .. } => *pressed,
            MemberInput::Text(text) => !text.is_empty(),
        };
        // Volume and mute act on the group's speaker exit, as the same keys on a
        // forwarding desktop source do. A phone has no forwarding of its own,
        // so the focus computer's forwarding state never decides them: they go
        // to the exit directly, also without a focus, and neither join nor
        // move the group pointer. Without an exit they stay ordinary keys.
        if let MemberInput::Key { code, pressed } = input
            && code.is_volume()
            && (self.volume_targets.contains_key(&(self.me(), code))
                || (pressed && !self.keys.contains_key(&code) && self.member_volume_exit().is_some()))
        {
            self.local_key(code, pressed, now);
            return true;
        }
        let target = self.member_join(activity, now);
        match input {
            // As from a Mac's trackpad: each stream keeps its own route
            // (scroll and gesture sequences their start target, contacts the
            // current one), so ends still reach where their start went.
            MemberInput::Scroll { dx, dy, phase } => {
                if !(dx.is_finite() && dy.is_finite()) {
                    return false;
                }
                self.local_scroll(SourceKind::Touchpad, dx, dy, ScrollUnit::Pixel, phase, None) == Disposition::Swallow
                    && target.is_some()
            }
            MemberInput::Gesture(gesture) => {
                let sent =
                    self.local_gesture(SourceKind::Touchpad, gesture) == Disposition::Swallow && target.is_some();
                // Diagnostics: a gesture's begin and end (never its changes) and where it went.
                if gesture.starts() || gesture.ends() {
                    tracing::info!(?gesture, ?target, sent, "screenless member: touchpad gesture");
                }
                sent
            }
            MemberInput::Contacts(sample) => {
                self.contacts_route(&CapturedEvent {
                    physical: true,
                    event: InputEvent::TouchpadSample(sample),
                    source: SourceKind::Touchpad,
                    time_us: sample.time_us,
                    cursor: None,
                });
                target.is_some()
            }
            // Releases always go to the press's recorded target.
            MemberInput::Button { button, pressed: false } => {
                self.local_button(button, false);
                self.buttons.is_empty() && target.is_some()
            }
            MemberInput::Key { code, pressed: false } => {
                self.local_key(code, false, now);
                target.is_some()
            }
            _ if target.is_none() => false,
            MemberInput::Motion { dx, dy } => {
                if !activity {
                    return false;
                }
                if self.game_mode().active {
                    self.raw_motion(dx, dy);
                } else {
                    self.motion_seq += 1;
                    let seq = self.motion_seq;
                    self.send(self.holder, Message::Input(InputMsg::PointerMotion { seq, dx, dy }));
                }
                true
            }
            MemberInput::Button { button, pressed } => {
                self.local_button(button, pressed);
                true
            }
            MemberInput::Key { code, pressed } => {
                self.local_key(code, pressed, now);
                true
            }
            MemberInput::Text(text) => {
                for piece in split_text(&text, oink_protocol::v2::dto::INPUT_TEXT_MAX_BYTES) {
                    self.text_out_seq += 1;
                    let seq = self.text_out_seq;
                    self.send(
                        self.holder,
                        Message::Input(InputMsg::Text {
                            seq,
                            text: piece.to_owned(),
                        }),
                    );
                }
                true
            }
        }
    }

    /// The first-input rules of protocol §14.2. `activity` is a press, non-zero
    /// motion/scroll or text; anything else never chooses a target.
    fn member_join(&mut self, activity: bool, now: Micros) -> Option<DeviceId> {
        let me = self.me();
        if self.holder != me {
            if !self.input_ready(self.holder) {
                return None;
            }
            if activity && !self.focus_scope_has_me() {
                self.member_resume();
            }
            return Some(self.holder);
        }
        if !activity || self.screen_focus_active() {
            return None;
        }
        let claim = self.claim;
        let claim_valid = claim != me
            && self.epoch != 0
            && self.input_ready(claim)
            && self.peers.get(&claim).and_then(|p| p.session).is_some_and(|s| {
                self.focus_scope
                    .as_ref()
                    .is_some_and(|scope| scope.members.contains(&(claim, s)))
            });
        if claim_valid {
            tracing::info!(epoch = self.epoch, holder = ?claim, "screenless member: joining the group pointer");
            self.become_source(self.epoch, claim);
            self.member_resume();
            return Some(claim);
        }
        let Some(target) = self.selected_target else {
            tracing::info!("screenless member: no focus and no device selected; input dropped");
            self.out.push(Output::Status(StatusEvent::NoTarget));
            return None;
        };
        let _ = now;
        match self.move_pointer_to(target) {
            Ok(()) if self.holder == target => Some(target),
            result => {
                tracing::info!(?target, ?result, "screenless member: selected device cannot take focus");
                self.out.push(Output::Status(StatusEvent::NoTarget));
                None
            }
        }
    }

    fn focus_scope_has_me(&self) -> bool {
        self.focus_scope
            .as_ref()
            .is_some_and(|scope| scope.members.contains(&(self.me(), self.session)))
    }

    /// Add this instance to the current claim's participants (explicit input
    /// notice, as for a newly joined desktop's first input elsewhere).
    fn member_resume(&mut self) {
        let mut scope = self.focus_scope.clone().unwrap_or_else(|| self.new_focus_scope());
        if !scope.members.contains(&(self.me(), self.session)) {
            scope.members.push((self.me(), self.session));
            scope.members.sort_unstable_by_key(|(device, _)| *device);
        }
        self.fresh_join = false;
        self.focus_joined = true;
        self.focus_scope = Some(scope.clone());
        self.broadcast(Message::Control(ControlMsg::FocusResume {
            epoch: self.epoch,
            holder: self.claim,
            scope,
        }));
    }

    /// `become_holder` for a screenless member: "no target". Holder becomes
    /// this device without claiming, placing or revealing anything.
    pub(super) fn become_untargeted(&mut self, epoch: u64) {
        let was_screen = self.screen_focus_active();
        if self.holder != self.me() && !self.keyboard_local() {
            self.cancel_caps();
        }
        if self.holder != self.me() {
            self.drop_scroll_remainder();
        }
        self.gap = None;
        self.epoch = epoch;
        self.holder = self.me();
        self.screen_focus = false;
        self.publish_lock_escape();
        self.last_pointer_source = None;
        self.unconfirmed_since = None;
        self.rejoin = false;
        self.enter_due = None;
        self.resistance = None;
        self.exit_point = None;
        if was_screen {
            // Android: the helper releases what remote sources hold and hides.
            self.out
                .push(Output::SetMode(CaptureMode::Remote { park: Point::default() }));
        }
        self.retarget_modifiers();
        self.push_focus_status();
    }

    /// Pointer units → canvas units on the focus holder (protocol §14.3).
    pub(super) fn pointer_unit_scale(&self) -> f64 {
        let Some(base) = &self.pointer_base else {
            return 1.0;
        };
        let canvas = self.layout.source_reference_per_native_unit(&base.screen);
        match canvas {
            Some(c) if c > 0.0 && base.system_scale > 0.0 => c * base.system_scale,
            _ => 1.0,
        }
    }

    /// The control page's lock button: pin or release the current focus
    /// (`SetFocusLock` to the holder; the result arrives in `FocusState`).
    pub fn member_set_locked(&mut self, locked: bool) -> Result<(), FocusLockError> {
        if self.paused || self.fully_paused {
            return Err(FocusLockError::Paused);
        }
        let holder = self.locked_device().unwrap_or(self.holder);
        if holder == self.me() && !self.screen_focus_active() {
            return Err(FocusLockError::Unavailable);
        }
        self.request_focus_lock(Some(locked));
        Ok(())
    }

    /// The control page closed: release every key and button it holds,
    /// at the targets they were pressed on.
    pub fn member_release_all(&mut self, now: Micros) {
        let keys: Vec<KeyCode> = self.keys.keys().copied().collect();
        for code in keys {
            self.local_key(code, false, now);
        }
        let buttons: Vec<MouseButton> = self.buttons.keys().copied().collect();
        for button in buttons {
            self.local_button(button, false);
        }
    }

    /// A member's emergency pause reached this phone, which has no pause state
    /// of its own: text not injected yet is dropped, and everything this phone
    /// holds elsewhere or injected here for another device is released.
    pub fn member_emergency(&mut self, now: Micros) {
        self.discard_text("emergency stop");
        self.member_release_all(now);
        let origins: Vec<DeviceId> = self.held.keys().copied().collect();
        for origin in origins {
            self.release_origin(origin);
        }
    }

    /// The control page's game-mode switch: a request to the current holder.
    pub fn member_set_game_mode(&mut self, mode: GameMode) -> Result<(), FocusLockError> {
        if self.paused || self.fully_paused {
            return Err(FocusLockError::Paused);
        }
        if self.member_target().is_none() {
            return Err(FocusLockError::Unavailable);
        }
        self.set_game_mode(mode);
        Ok(())
    }

    pub fn set_pointer_base(&mut self, base: Option<PointerBase>) {
        self.pointer_base = base;
    }

    /// Holder side: report this source's edge state when it changes.
    pub(super) fn report_edge(&mut self, origin: DeviceId, state: EdgeState, side: EdgeSide, target: Option<DeviceId>) {
        if !self.pointer_sources.contains(&origin) || origin == self.me() {
            return;
        }
        let report = (self.epoch, state, side, target);
        if self.edge_reports.get(&origin) == Some(&report) {
            return;
        }
        if state == EdgeState::Clear && self.edge_reports.get(&origin).is_none_or(|r| r.1 == EdgeState::Clear) {
            return;
        }
        self.edge_reports.insert(origin, report);
        self.send(
            origin,
            Message::Control(ControlMsg::EdgeReport {
                epoch: self.epoch,
                state,
                edge: side,
                target,
            }),
        );
    }
}

/// The edge a delta pushes against: the dominant axis and its sign.
pub(super) fn edge_side(dx: f64, dy: f64) -> EdgeSide {
    if dx.abs() >= dy.abs() {
        if dx < 0. { EdgeSide::Left } else { EdgeSide::Right }
    } else if dy < 0. {
        EdgeSide::Top
    } else {
        EdgeSide::Bottom
    }
}

/// Split on character boundaries into pieces of at most `max` UTF-8 bytes.
pub(super) fn split_text(text: &str, max: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(max);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        pieces.push(&rest[..end]);
        rest = &rest[end..];
    }
    pieces
}
