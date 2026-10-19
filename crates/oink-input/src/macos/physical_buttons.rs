//! Local device provenance, independent of WindowServer's mixed physical/injected HID state.
use objc2_core_graphics::CGEventTapLocation;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Default)]
pub(crate) struct PhysicalButtons(AtomicU32);

impl PhysicalButtons {
    pub(crate) fn tap(&self, button: u32, pressed: bool, requested: CGEventTapLocation) -> CGEventTapLocation {
        if !pressed && self.is_down(button) {
            CGEventTapLocation::SessionEventTap
        } else {
            requested
        }
    }

    pub(crate) fn reset(&self) {
        self.0.store(0, Ordering::Release);
    }

    pub(crate) fn record(&self, button: u32, pressed: bool) {
        let Some(bit) = 1u32.checked_shl(button) else { return };
        if pressed {
            self.0.fetch_or(bit, Ordering::AcqRel);
        } else {
            self.0.fetch_and(!bit, Ordering::AcqRel);
        }
    }

    pub(crate) fn is_down(&self, button: u32) -> bool {
        1u32.checked_shl(button)
            .is_some_and(|bit| self.0.load(Ordering::Acquire) & bit != 0)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn failed_start_and_repeated_down_keep_physical_hold_until_real_up() {
        let buttons = PhysicalButtons::default();
        buttons.record(0, true);
        let hid = CGEventTapLocation::HIDEventTap;
        let session = CGEventTapLocation::SessionEventTap;
        // Both failed-start cleanup and repeated-down recovery request an ordinary up.
        assert_eq!(buttons.tap(0, false, hid), session);
        assert_eq!(buttons.tap(0, true, hid), hid);
        buttons.record(0, true);
        assert_eq!(buttons.tap(0, false, hid), session);
        buttons.record(0, false);
        assert_eq!(buttons.tap(0, false, hid), hid);
        // A later remote-only drag keeps HID cleanup; explicit physical handoff stays Session.
        assert_eq!(buttons.tap(0, true, hid), hid);
        assert_eq!(buttons.tap(0, false, hid), hid);
        assert_eq!(buttons.tap(0, false, session), session);
    }

    #[test]
    fn independent_buttons_and_out_of_range_numbers() {
        let buttons = PhysicalButtons::default();
        buttons.record(0, true);
        buttons.record(1, true);
        buttons.record(0, false);
        buttons.record(32, true);
        assert!(!buttons.is_down(0));
        assert!(buttons.is_down(1));
        assert!(!buttons.is_down(32));
    }
}
