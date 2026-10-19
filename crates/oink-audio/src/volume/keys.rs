//! Physical volume keys retain their first press context. A held repeat cannot
//! migrate to another claim/output, and duplicate downs do not adjust twice.
use super::*;
use oink_protocol::v1::dto::AudioSessionContext;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    Up,
    Down,
    Mute,
}
impl KeyAction {
    pub fn from_code(code: oink_foundation::input::KeyCode) -> Option<Self> {
        use oink_foundation::input::KeyCode;
        if code == KeyCode::new(12, 0xe9) || code == KeyCode::keyboard(0x80) {
            Some(Self::Up)
        } else if code == KeyCode::new(12, 0xea) || code == KeyCode::keyboard(0x81) {
            Some(Self::Down)
        } else if code == KeyCode::new(12, 0xe2) || code == KeyCode::keyboard(0x7f) {
            Some(Self::Mute)
        } else {
            None
        }
    }
}
#[derive(Clone, Debug)]
pub struct KeyEvent {
    pub source: Boot,
    pub session: AudioSessionContext,
    pub press_id: u64,
    pub action: KeyAction,
    pub pressed: bool,
    pub repeat: bool,
    /// HID modifier bitmap (left/right Ctrl, Shift, Alt, GUI).
    pub modifiers: u8,
}
#[derive(Debug)]
pub(super) struct Press {
    source: Boot,
    session: AudioSessionContext,
    action: KeyAction,
    id: u64,
    held: bool,
}
impl Owner {
    /// Retire press identities when their authenticated group/Boot authority
    /// ends. Otherwise departed members would occupy the bounded key table
    /// until the output changes, and a reconnect could continue an old repeat.
    pub fn retain_sources(&mut self, mut current: impl FnMut(&Boot) -> bool) {
        self.presses.retain(|press| current(&press.source));
    }
    /// The caller verifies the authenticated source and current group membership
    /// before delivery; this method verifies its complete physical press context.
    pub fn key(&mut self, event: KeyEvent) -> Result<bool, ControlFailure> {
        if event.press_id == 0 {
            return Err(ControlFailure::InvalidValue);
        }
        if event.session.source().as_ref() != Ok(&event.source) {
            return Err(ControlFailure::StaleBoot);
        }
        let current = self.state.as_ref().ok_or(ControlFailure::NotExit)?;
        let version = current.version().map_err(|_| ControlFailure::InvalidValue)?;
        let output = event
            .session
            .receiver_output()
            .map_err(|_| ControlFailure::InvalidValue)?;
        if output.receiver() != version.exit() {
            return Err(ControlFailure::StaleBoot);
        }
        if event.session.exit_claim() != version.exit_claim() {
            return Err(ControlFailure::RouteChanged);
        }
        if output.generation() != version.output_generation() || output.output_id() != version.output_id() {
            return Err(ControlFailure::OutputChanged);
        }
        // The authenticated current Boot retires this device's prior process.
        self.presses
            .retain(|press| press.source.device_id() != event.source.device_id() || press.source == event.source);
        if let Some(press) = self
            .presses
            .iter_mut()
            .find(|press| press.source == event.source && press.action == event.action)
        {
            if event.press_id < press.id {
                return Err(ControlFailure::StateConflict);
            }
            if event.press_id == press.id {
                if press.session != event.session {
                    return Err(ControlFailure::RouteChanged);
                }
                if !event.pressed {
                    press.held = false;
                    return Ok(false);
                }
                if !press.held || !event.repeat {
                    return Ok(false);
                }
                // A held mute key toggles once, never on platform auto-repeat.
                if event.action == KeyAction::Mute {
                    return Ok(false);
                }
            } else {
                if event.repeat || !event.pressed {
                    return Err(ControlFailure::StateConflict);
                }
                *press = Press {
                    source: event.source,
                    session: event.session,
                    action: event.action,
                    id: event.press_id,
                    held: true,
                };
            }
        } else {
            if event.repeat || !event.pressed {
                return Err(ControlFailure::StateConflict);
            }
            if self.presses.len() == 48 {
                return Err(ControlFailure::Busy);
            }
            self.presses.push(Press {
                source: event.source,
                session: event.session,
                action: event.action,
                id: event.press_id,
                held: true,
            });
        }
        self.refresh(true)?;
        let status = self
            .state
            .as_ref()
            .ok_or(ControlFailure::NotExit)?
            .status()
            .map_err(|_| ControlFailure::InvalidValue)?;
        if status != ExitVolumeStatus::Ready {
            return Err(Fault {
                status,
                native_code: None,
            }
            .reason());
        }
        let endpoint = self.endpoint.as_mut().ok_or(ControlFailure::OutputUnavailable)?;
        let written = endpoint.key_with_modifiers(event.action, event.modifiers);
        let reading = endpoint.read();
        let changed = self.apply_reading(reading)?;
        written.map_err(Fault::reason)?;
        reading.map_err(Fault::reason)?;
        Ok(changed)
    }
}
