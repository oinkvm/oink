//! Source-owned Caps gesture. The output and destination are frozen at DOWN;
//! Caps remains the physical identity in target refcounts and HeldSync.
use super::*;
use oink_foundation::input_transform::{CAPS_HOLD_US, CapsTapHold, InputTransform};

#[derive(Clone, Copy, Debug)]
pub(super) struct CapsPress {
    target: Target,
    mapping: CapsTapHold,
    since: Micros,
    output: Option<KeyCode>,
}
impl CapsPress {
    pub(super) fn deadline(&self) -> Option<Micros> {
        self.output.is_none().then_some(self.since.saturating_add(CAPS_HOLD_US))
    }
    pub(super) fn held_on(&self, peer: DeviceId) -> bool {
        self.target == Target::Remote(peer) && self.output.is_some()
    }
}
impl Router {
    /// A cached platform observation, never an OS query on the input owner.
    /// Changes affect the next physical press, not a gesture already in flight.
    pub fn set_mac_caps_switch(&mut self, enabled: bool) {
        self.mac_caps_switch = enabled;
    }
    fn source_caps_mapping(&self, target: Target) -> Option<CapsTapHold> {
        let (device, os) = match target {
            Target::Remote(d) => (d, self.peers.get(&d)?.os),
            Target::Local | Target::LocalInjected if self.cfg.os == Os::Windows => (self.me(), self.cfg.os),
            // Local Mac Caps owns native HID/TSM state; leave it to macOS.
            _ => return None,
        };
        let source_os = self.cfg.os.as_str();
        self.input_transforms
            .get(&self.me())
            .cloned()
            .unwrap_or_else(|| InputTransform::initial(source_os))
            .resolve(device, os.as_str())
            .keyboard
            .caps_tap_hold(source_os, os.as_str(), self.mac_caps_switch)
    }
    pub(super) fn local_caps_mapping(&self) -> bool {
        // A settings edit must not let a native letter overtake the Shift DOWN
        // already queued by the in-flight Windows-local Caps gesture.
        self.caps_press
            .is_some_and(|c| matches!(c.target, Target::Local | Target::LocalInjected))
            || self.source_caps_mapping(Target::Local).is_some()
    }
    pub(super) fn caps_event(
        &mut self,
        code: KeyCode,
        pressed: bool,
        physical: bool,
        now: Micros,
    ) -> Option<Disposition> {
        if code != KeyCode::CAPS_LOCK {
            if physical && pressed && !self.keys.contains_key(&code) {
                // Process the elapsed threshold even if the event beat a delayed timer.
                self.caps_tick(now);
                self.commit_caps(false);
            }
            return None;
        }
        if self.caps_press.is_some() {
            if !pressed {
                if physical {
                    self.caps_tick(now);
                    self.commit_caps(false);
                }
                // A synthetic lost-UP or capture reset cancels a pending tap.
                self.release_caps();
                self.keys.remove(&code);
            }
            // Repeated DOWN is never a second gesture, including Windows repeat.
            return Some(Disposition::Swallow);
        }
        if !pressed || !physical || self.paused || self.keys.contains_key(&code) {
            return None;
        }
        let target = if self.keyboard_local() {
            Target::Local
        } else {
            self.current_target()
        };
        let mapping = self.source_caps_mapping(target)?;
        self.caps_press = Some(CapsPress {
            target,
            mapping,
            since: now,
            output: None,
        });
        // No remote DOWN exists yet. HeldSync adds Caps only after commitment.
        self.keys.insert(code, vec![Target::Dropped]);
        Some(Disposition::Swallow)
    }
    pub(super) fn caps_tick(&mut self, now: Micros) {
        if self
            .caps_press
            .as_ref()
            .and_then(CapsPress::deadline)
            .is_some_and(|due| now >= due)
        {
            self.commit_caps(true);
        }
    }
    fn commit_caps(&mut self, hold: bool) {
        let Some(caps) = self.caps_press.as_mut().filter(|c| c.output.is_none()) else {
            return;
        };
        let code = if hold { caps.mapping.hold } else { caps.mapping.tap };
        caps.output = Some(code);
        let target = caps.target;
        self.send_caps(target, code, true);
    }
    fn send_caps(&mut self, target: Target, code: KeyCode, pressed: bool) {
        if code == KeyCode::NONE {
            return;
        }
        match target {
            Target::Remote(d) => self.send(d, Message::Input(InputMsg::CapsKey { code, pressed })),
            Target::Local | Target::LocalInjected => {
                if !self.keyboard_local() {
                    self.application_key(self.me(), 0, code, Some(KeyCode::CAPS_LOCK), pressed);
                }
                self.target_mapped_key(self.me(), KeyCode::CAPS_LOCK, code, pressed)
            }
            Target::Dropped => {}
        }
    }
    fn release_caps(&mut self) {
        if let Some(caps) = self.caps_press.take()
            && let Some(code) = caps.output
        {
            self.send_caps(caps.target, code, false);
        }
    }
    pub(super) fn cancel_caps(&mut self) {
        if self.caps_press.is_some() {
            self.release_caps();
            self.keys.remove(&KeyCode::CAPS_LOCK);
            self.keyboard.suppress(KeyCode::CAPS_LOCK);
        }
    }
    pub(super) fn cancel_caps_peer(&mut self, peer: DeviceId) {
        if self.caps_press.is_some_and(|c| c.target == Target::Remote(peer)) {
            self.cancel_caps();
        }
    }
}
