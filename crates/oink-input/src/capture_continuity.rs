//! Shared by the Windows hook and its recording test backend. A capture gap ends
//! every observed gesture; recovery consults hardware, never the stale ledger.
use crate::{CapturedEvent, InputEvent, KeyCode, MouseButton, SourceKind};
use std::collections::HashSet;

#[derive(Debug)]
pub struct CaptureContinuity {
    available: bool,
    keys: HashSet<KeyCode>,
    buttons: HashSet<MouseButton>,
    resume_modifiers: Vec<KeyCode>,
}
impl Default for CaptureContinuity {
    fn default() -> Self {
        Self {
            available: true,
            keys: HashSet::new(),
            buttons: HashSet::new(),
            resume_modifiers: Vec::new(),
        }
    }
}
impl CaptureContinuity {
    pub fn available(&self) -> bool {
        self.available
    }
    pub fn observe(&mut self, event: &InputEvent) {
        match *event {
            InputEvent::Key { code, pressed } => {
                if pressed {
                    self.keys.insert(code);
                } else {
                    self.keys.remove(&code);
                }
            }
            InputEvent::Button { button, pressed } => {
                if pressed {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
            }
            _ => {}
        }
    }
    /// A switch notification proves a discontinuity even if a fast secure-desktop
    /// round trip finished before its queued callback could inspect the desktop.
    pub fn desktop_changed(
        &mut self,
        available: bool,
        is_down: impl FnMut(KeyCode) -> bool,
        time_us: u64,
    ) -> Vec<CapturedEvent> {
        let mut events = self.transition(false, |_| false, time_us);
        events.extend(self.transition(available, is_down, time_us));
        events
    }

    /// Loss emits UP through the same callback as a physical release. It also
    /// forgets local-only presses, which must not be retargeted after unlocking.
    /// Resume queries only modifiers observed before the gap. Ordinary keys and
    /// buttons start new gestures; never replay password text or clicks from a
    /// desktop on which we were not capturing.
    pub fn transition(
        &mut self,
        available: bool,
        mut is_down: impl FnMut(KeyCode) -> bool,
        time_us: u64,
    ) -> Vec<CapturedEvent> {
        if self.available == available {
            return Vec::new();
        }
        self.available = available;
        let mut events = Vec::new();
        if available {
            for code in self.resume_modifiers.drain(..) {
                if is_down(code) {
                    self.keys.insert(code);
                    events.push(InputEvent::Key { code, pressed: true });
                }
            }
        } else {
            for code in self.keys.drain() {
                if code.is_modifier() {
                    self.resume_modifiers.push(code);
                }
                events.push(InputEvent::Key { code, pressed: false });
            }
            for button in self.buttons.drain() {
                events.push(InputEvent::Button { button, pressed: false });
            }
        }
        events
            .into_iter()
            .map(|event| CapturedEvent {
                event,
                physical: false,
                source: SourceKind::Unknown,
                time_us,
                cursor: None,
            })
            .collect()
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn delayed_switch_notification_still_repairs_a_completed_desktop_round_trip() {
        let mut capture = CaptureContinuity::default();
        capture.observe(&InputEvent::Key {
            code: KeyCode::LEFT_GUI,
            pressed: true,
        });
        let releases = capture.desktop_changed(true, |_| false, 10);
        assert_eq!(releases.len(), 1);
        assert_eq!(
            releases[0].event,
            InputEvent::Key {
                code: KeyCode::LEFT_GUI,
                pressed: false
            }
        );
        assert!(capture.available());
        assert!(capture.desktop_changed(true, |_| false, 11).is_empty());
    }

    #[test]
    fn loss_releases_buttons_and_keys_once_resume_reads_real_modifiers_only() {
        let mut capture = CaptureContinuity::default();
        for code in [KeyCode::LEFT_GUI, KeyCode::RIGHT_SHIFT, KeyCode::keyboard(4)] {
            capture.observe(&InputEvent::Key { code, pressed: true });
        }
        capture.observe(&InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        });
        let releases = capture.transition(false, |_| panic!("unobservable keyboard"), 10);
        assert_eq!(releases.len(), 4);
        assert!(releases.iter().all(|e| !e.physical));
        assert!(capture.transition(false, |_| panic!("duplicate loss"), 11).is_empty());
        let mut queried = Vec::new();
        let resumed = capture.transition(
            true,
            |key| {
                queried.push(key);
                key == KeyCode::RIGHT_SHIFT
            },
            12,
        );
        assert_eq!(queried.len(), 2);
        assert_eq!(resumed.len(), 1);
        assert_eq!(
            resumed[0].event,
            InputEvent::Key {
                code: KeyCode::RIGHT_SHIFT,
                pressed: true
            }
        );
        assert!(capture.transition(true, |_| panic!("duplicate resume"), 13).is_empty());
    }
}
