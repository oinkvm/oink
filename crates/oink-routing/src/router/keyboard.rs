//! Runtime-only source keyboard guard. Remote injection never enters this gate.
use super::*;
use std::collections::BTreeSet;

/// Exact physical chord, either modifier side, held for one second.
pub const KEYBOARD_RELEASE_CHORD: &str = "Ctrl+Option/Alt+Shift+U";
pub const KEYBOARD_RELEASE_HOLD_US: Micros = 1_000_000;
const RELEASE_KEY: KeyCode = KeyCode::keyboard(0x18);

#[derive(Debug, Default)]
pub(super) struct Keyboard {
    disabled: bool,
    down: BTreeSet<KeyCode>,
    local_passed: BTreeSet<KeyCode>,
    pointer_seed: BTreeSet<KeyCode>,
    suppressed: BTreeSet<KeyCode>,
    since: Option<Micros>,
}
impl Keyboard {
    pub(super) fn suppress(&mut self, code: KeyCode) {
        self.suppressed.insert(code);
        self.local_passed.remove(&code);
    }
    pub(super) fn record_pass(&mut self, ev: &CapturedEvent, disposition: Disposition) {
        if let InputEvent::Key { code, pressed } = ev.event {
            if !pressed {
                self.local_passed.remove(&code);
            } else if ev.physical && disposition == Disposition::Pass {
                self.local_passed.insert(code);
            }
        }
    }
    fn exact(&self) -> bool {
        self.down.len() == 4
            && self.down.contains(&RELEASE_KEY)
            && [KeyCode::LEFT_CTRL, KeyCode::RIGHT_CTRL]
                .iter()
                .any(|k| self.down.contains(k))
            && [KeyCode::LEFT_ALT, KeyCode::RIGHT_ALT]
                .iter()
                .any(|k| self.down.contains(k))
            && [KeyCode::LEFT_SHIFT, KeyCode::RIGHT_SHIFT]
                .iter()
                .any(|k| self.down.contains(k))
    }
    pub(super) fn deadline(&self) -> Option<Micros> {
        self.since
            .filter(|_| self.disabled && self.exact())
            .map(|t| t + KEYBOARD_RELEASE_HOLD_US)
    }
}
impl Router {
    /// Modifier flags on local physical pointer events must exclude blocked hardware
    /// modifiers, while preserving any modifier held by a remote injected source.
    pub fn keyboard_pointer_modifiers(&self) -> Vec<KeyCode> {
        let groups = [
            [KeyCode::LEFT_CTRL, KeyCode::RIGHT_CTRL],
            [KeyCode::LEFT_SHIFT, KeyCode::RIGHT_SHIFT],
            [KeyCode::LEFT_ALT, KeyCode::RIGHT_ALT],
            [KeyCode::LEFT_GUI, KeyCode::RIGHT_GUI],
            [KeyCode::new(0xff, 3), KeyCode::new(0xff, 3)],
        ];
        groups
            .into_iter()
            .filter(|g| {
                g.iter().any(|k| {
                    (self.keyboard.down.contains(k)
                        || (self.keyboard.disabled && self.keyboard.pointer_seed.contains(k)))
                        && (self.keyboard_key_blocked(Some(*k))
                            || self.keys.get(k).is_some_and(|targets| {
                                !targets.contains(&Target::Local) && !targets.contains(&Target::LocalInjected)
                            }))
                }) && !g.iter().any(|k| self.key_refs.contains_key(k))
            })
            .flatten()
            .collect()
    }
    pub fn keyboard_disabled(&self) -> bool {
        self.keyboard.disabled
    }
    pub fn keyboard_key_blocked(&self, code: Option<KeyCode>) -> bool {
        if code.is_some_and(KeyCode::is_brightness) {
            return false;
        }
        (!self.fully_paused && self.keyboard.disabled) || code.is_some_and(|k| self.keyboard.suppressed.contains(&k))
    }

    pub fn release_session_keys(&mut self, keys: &[KeyCode]) {
        if self.cfg.os == Os::Windows
            && keys.iter().any(|k| {
                matches!(
                    *k,
                    KeyCode::LEFT_ALT | KeyCode::RIGHT_ALT | KeyCode::LEFT_GUI | KeyCode::RIGHT_GUI
                ) && !self.key_refs.contains_key(k)
                    && !self.keyboard.suppressed.contains(k)
            })
        {
            self.inject_menu_mask();
        }
        for &code in keys {
            if self.key_refs.contains_key(&code) {
                continue;
            }
            if code.is_modifier() || code == KeyCode::new(0xff, 3) {
                self.keyboard.pointer_seed.insert(code);
            }
            if !self.keyboard.suppressed.contains(&code) {
                self.out.push(Output::Inject(InjectEvent::Key { code, pressed: false }));
            }
        }
    }
    pub fn release_keyboard(&mut self) {
        if self.keyboard.disabled {
            // Persist the release intent before publishing the enabled observation.
            self.out.push(Output::Status(StatusEvent::KeyboardReleased));
            self.set_keyboard_disabled(false);
        }
    }
    pub fn set_keyboard_disabled(&mut self, disabled: bool) {
        if self.keyboard.disabled == disabled {
            return;
        }
        self.keyboard.disabled = disabled;
        self.keyboard.since = None;
        self.keyboard.pointer_seed.clear();
        // Presses made under the guard (including its release chord) must be lifted
        // before they can start a new gesture. Auto-repeat cannot revive them.
        self.keyboard.suppressed.extend(self.keyboard.down.iter().copied());
        if disabled {
            self.cancel_caps();
            self.release_local_transformed();
            // Alt/Win released for cleanup must not activate a menu or Start.
            self.mask_menu_keys();
            let keys: Vec<_> = self.keys.drain().collect();
            for (code, targets) in keys {
                self.keyboard.suppressed.insert(code);
                if self.release_local_key(code, targets) {
                    self.keyboard.local_passed.insert(code);
                }
            }
            let passed = std::mem::take(&mut self.keyboard.local_passed);
            for code in passed {
                self.keyboard.suppressed.insert(code);
                if !self.key_refs.contains_key(&code) {
                    self.out.push(Output::Inject(InjectEvent::Key { code, pressed: false }));
                }
            }
        }
        self.out.push(Output::Status(StatusEvent::KeyboardDisabled(disabled)));
    }

    pub(super) fn retire_source_keyboard(&mut self) {
        self.cancel_caps();
        self.mask_menu_keys();
        let keys: Vec<_> = self.keys.drain().collect();
        let mut native = std::mem::take(&mut self.keyboard.local_passed);
        for (code, targets) in keys {
            self.keyboard.suppressed.insert(code);
            if self.release_local_key(code, targets) {
                native.insert(code);
            }
        }
        self.keyboard.suppressed.extend(self.keyboard.down.iter().copied());
        let mut released = HashSet::new();
        for code in native {
            self.keyboard.suppressed.insert(code);
            let mapped = self.local_mapped.get(&code).copied().unwrap_or(code);
            if !self.key_refs.contains_key(&mapped) && released.insert(mapped) {
                self.out.push(Output::Inject(InjectEvent::Key {
                    code: mapped,
                    pressed: false,
                }));
            }
        }
        self.local_mapped.clear();
        self.local_rewrite = None;
        self.key_repeats.clear();
    }

    /// Session transitions retire physical chord/tail observations, never the
    /// saved guard. Credential input is not a source of presses for this tracker.
    pub fn keyboard_session_locked(&mut self, locked: Option<bool>) {
        if self.keyboard.disabled && locked.is_some() {
            self.keyboard.since = None;
            self.keyboard.down.clear();
            self.keyboard.suppressed.clear();
            self.keyboard.pointer_seed.clear();
        }
    }

    pub(super) fn keyboard_event(&mut self, ev: &CapturedEvent, now: Micros) -> Option<Disposition> {
        let InputEvent::Key { code, pressed } = ev.event else {
            return None;
        };
        if self.keyboard.disabled && ev.physical && code.is_brightness() {
            // Return Pass here, before suppression, transforms and forwarding.
            return Some(Disposition::Pass);
        }
        if ev.physical {
            if pressed {
                self.keyboard.down.insert(code);
            } else {
                self.keyboard.down.remove(&code);
            }
        } else if !pressed {
            // Native lost-UP reconciliation only retires identities; it cannot arm release.
            self.keyboard.down.remove(&code);
        }
        let suppressed = self.keyboard.suppressed.contains(&code);
        if !pressed {
            self.keyboard.suppressed.remove(&code);
        }
        if self.keyboard.disabled && !self.fully_paused && ev.physical {
            if pressed {
                self.keyboard.suppressed.insert(code);
            }
            if !self.keyboard.exact() {
                self.keyboard.since = None;
            } else if self.keyboard.since.is_none() {
                self.keyboard.since = Some(now);
            }
            return Some(Disposition::Swallow);
        }
        if !self.keyboard.exact() {
            self.keyboard.since = None;
        }
        suppressed.then_some(Disposition::Swallow)
    }
}
