//! Read-only button provenance. This never releases, injects, filters or repairs input.
use oink_foundation::input::MouseButton;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layer {
    HidTap,
    SessionTap,
    WindowsHook,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimestampUnit {
    Nanoseconds,
    Milliseconds,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemState {
    /// native_lifecycle's process-monotonic clock; the enclosing record also has recorder time.
    pub sampled_at_us: u64,
    pub hid: Option<bool>,
    pub combined_session: Option<bool>,
    pub asynchronous: Option<bool>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub layer: Layer,
    pub button: MouseButton,
    pub pressed: bool,
    pub event_timestamp: u64,
    pub timestamp_unit: TimestampUnit,
    pub source_state: Option<i64>,
    pub physical: bool,
    pub own: bool,
    pub system: SystemState,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapUp {
    pub available: bool,
    pub generation: u64,
    pub observing_since_ns: u64,
    pub last_physical_up_ns: Option<u64>,
    pub last_external_up_ns: Option<u64>,
    /// The down corresponding to the router's captured press, retained even if
    /// the original transition has aged out of the requested package window.
    pub matching_down: Option<TapDown>,
    /// None: no local captured press to match, or an observation gap since that press.
    /// Own injected ups never count as releases of a captured physical/external press.
    pub matching_up: Option<bool>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapDown {
    pub event_timestamp_ns: u64,
    pub source_state: Option<i64>,
    pub system: SystemState,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    pub system: SystemState,
    pub hid_tap: Option<TapUp>,
    pub session_tap: Option<TapUp>,
}

pub fn enabled() -> bool {
    crate::native_lifecycle::lifecycle_enabled()
}

pub fn system_state(button: MouseButton) -> SystemState {
    let mut state = SystemState {
        sampled_at_us: crate::native_lifecycle::timestamp(),
        ..Default::default()
    };
    #[cfg(target_os = "macos")]
    {
        use objc2_core_graphics::{CGEventSource, CGEventSourceStateID, CGMouseButton};
        let button = CGMouseButton(number(button));
        state.hid = Some(CGEventSource::button_state(
            CGEventSourceStateID::HIDSystemState,
            button,
        ));
        state.combined_session = Some(CGEventSource::button_state(
            CGEventSourceStateID::CombinedSessionState,
            button,
        ));
    }
    #[cfg(windows)]
    {
        // GetAsyncKeyState is a read-only observation of this desktop, not proof of
        // device provenance. A low-level hook sees the state before its event updates it.
        if let Some(vk) = windows_vk(button) {
            // SAFETY: scalar virtual-key query; no pointers or state mutation.
            state.asynchronous =
                Some(unsafe { ::windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk) } < 0);
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = button;
    state
}

pub fn sample(button: MouseButton, captured: Option<(bool, u64)>) -> Sample {
    let result = Sample {
        system: system_state(button),
        ..Default::default()
    };
    #[cfg(target_os = "macos")]
    {
        let mut result = result;
        HISTORY.with(|history| {
            let history = history.borrow();
            result.hid_tap = Some(history[0].sample(number(button) as usize, captured));
            result.session_tap = Some(history[1].sample(number(button) as usize, captured));
        });
        result
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = captured;
        result
    }
}

#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
fn number(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
        MouseButton::Other(n) => n.into(),
    }
}
#[cfg(any(windows, all(test, debug_assertions)))]
fn windows_vk(button: MouseButton) -> Option<i32> {
    // VK_*BUTTON identify physical left/right; do not swap them using the primary-button setting.
    match button {
        MouseButton::Left => Some(0x01),
        MouseButton::Right => Some(0x02),
        MouseButton::Middle => Some(0x04),
        MouseButton::Back => Some(0x05),
        MouseButton::Forward => Some(0x06),
        MouseButton::Other(_) => None,
    }
}

#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
struct History {
    available: bool,
    generation: u64,
    since_ns: u64,
    ups: [[Option<u64>; 2]; 256],
    downs: [[Option<TapDown>; 2]; 256],
}
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
impl Default for History {
    fn default() -> Self {
        Self {
            available: false,
            generation: 0,
            since_ns: 0,
            ups: [[None; 2]; 256],
            downs: [[None; 2]; 256],
        }
    }
}
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
impl History {
    fn boundary(&mut self, available: bool, at_ns: u64) {
        self.available = available;
        self.generation += 1;
        self.since_ns = at_ns;
        // Keep positive evidence across gaps; only a negative "no up" claim
        // requires uninterrupted observation since the corresponding down.
    }
    fn observe(&mut self, event: &Transition) {
        if !event.own {
            let button = number(event.button) as usize;
            let source = usize::from(event.physical);
            if event.pressed {
                // A new down begins a new pair. External producers may supply
                // non-monotonic timestamps; never reuse an earlier pair's up.
                self.ups[button][source] = None;
                self.downs[button][source] = Some(TapDown {
                    event_timestamp_ns: event.event_timestamp,
                    source_state: event.source_state,
                    system: event.system,
                });
            } else {
                self.ups[button][source] = Some(event.event_timestamp);
            }
        }
    }
    fn sample(&self, button: usize, captured: Option<(bool, u64)>) -> TapUp {
        let [external, physical] = self.ups[button];
        let matching_up = captured.and_then(|(is_physical, event_us)| {
            let since = event_us.saturating_mul(1000);
            if self.ups[button][usize::from(is_physical)].is_some_and(|up| up >= since) {
                Some(true)
            } else if self.available && since >= self.since_ns {
                Some(false)
            } else {
                None
            }
        });
        TapUp {
            available: self.available,
            generation: self.generation,
            observing_since_ns: self.since_ns,
            last_physical_up_ns: physical,
            last_external_up_ns: external,
            matching_down: captured.and_then(|(physical, us)| {
                self.downs[button][usize::from(physical)].filter(|down| down.event_timestamp_ns / 1000 == us)
            }),
            matching_up,
        }
    }
}
// Both taps and routing run on the capture owner. A different thread has unavailable
// histories, rather than borrowing an unrelated backend or attributing another source's up.
#[cfg(target_os = "macos")]
thread_local! { static HISTORY: std::cell::RefCell<[History; 2]> = std::cell::RefCell::new(Default::default()); }
#[cfg(target_os = "macos")]
pub(crate) fn tap_boundary(layer: Layer, available: bool, at_ns: u64) {
    let index = match layer {
        Layer::HidTap => 0,
        Layer::SessionTap => 1,
        Layer::WindowsHook => return,
    };
    HISTORY.with(|h| h.borrow_mut()[index].boundary(available, at_ns));
}
pub(crate) fn transition(event: Transition) {
    #[cfg(target_os = "macos")]
    {
        let index = match event.layer {
            Layer::HidTap => 0,
            Layer::SessionTap => 1,
            Layer::WindowsHook => return,
        };
        HISTORY.with(|h| h.borrow_mut()[index].observe(&event));
    }
    if enabled() {
        crate::native_lifecycle::emit_button(event);
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn button_up_evidence_distinguishes_layers_provenance_and_capture_gaps() {
        let mut hid = History::default();
        let mut session = History::default();
        hid.boundary(true, 1000);
        session.boundary(true, 1000);
        let press = Some((true, 2));
        assert_eq!(hid.sample(1, press).matching_up, Some(false));
        let mut up = Transition {
            layer: Layer::HidTap,
            button: MouseButton::Right,
            pressed: false,
            event_timestamp: 3000,
            timestamp_unit: TimestampUnit::Nanoseconds,
            source_state: Some(1),
            physical: false,
            own: true,
            system: SystemState::default(),
        };
        let down = Transition {
            pressed: true,
            physical: true,
            own: false,
            event_timestamp: 2000,
            source_state: Some(1),
            system: SystemState {
                sampled_at_us: 17,
                hid: Some(true),
                combined_session: Some(false),
                asynchronous: None,
            },
            ..up
        };
        hid.observe(&down);
        session.observe(&down);
        assert_eq!(session.sample(1, press).matching_down.unwrap().source_state, Some(1));
        assert_eq!(session.sample(1, press).matching_down.unwrap().system, down.system);
        hid.observe(&up);
        assert_eq!(
            hid.sample(1, press).matching_up,
            Some(false),
            "own injected release cannot repair physical evidence"
        );
        up.own = false;
        hid.observe(&up);
        assert_eq!(hid.sample(1, press).matching_up, Some(false));
        assert_eq!(hid.sample(1, Some((false, 2))).matching_up, Some(true));
        up.physical = true;
        hid.observe(&up);
        assert_eq!(hid.sample(1, press).matching_up, Some(true));
        assert_eq!(
            session.sample(1, press).matching_up,
            Some(false),
            "HID up absent at session remains visible"
        );
        session.boundary(true, 4000);
        assert_eq!(
            session.sample(1, press).matching_down.unwrap().event_timestamp_ns,
            2000,
            "the known down survives a gap or a package window that excludes it"
        );
        assert_eq!(
            session.sample(1, press).matching_up,
            None,
            "re-enable gap is not proof of a missing up"
        );
        assert_eq!(session.sample(1, Some((true, 5))).matching_up, Some(false));
        assert_eq!(
            hid.sample(1, None).matching_up,
            None,
            "remote-source presses cannot match holder hardware"
        );
    }
    #[test]
    fn windows_button_observation_uses_physical_vk_and_unknown_stays_unknown() {
        assert_eq!(windows_vk(MouseButton::Left), Some(1));
        assert_eq!(windows_vk(MouseButton::Right), Some(2));
        assert_eq!(windows_vk(MouseButton::Middle), Some(4));
        assert_eq!(windows_vk(MouseButton::Back), Some(5));
        assert_eq!(windows_vk(MouseButton::Forward), Some(6));
        assert_eq!(windows_vk(MouseButton::Other(7)), None);
    }
}
