//! Windows external-input ownership. Kept independent of Win32 for model tests.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Own,
    Marker,
    Physical,
    External,
}

pub(crate) fn classify(own: bool, marker: bool, injected: bool, lower: bool) -> Origin {
    if own {
        Origin::Own
    } else if marker {
        Origin::Marker
    } else if injected || lower {
        Origin::External
    } else {
        Origin::Physical
    }
}

// No Debug/Serialize: key/button identities exist only for the finite native
// press ledger, never in a diagnostic payload. Win32 virtual keys are 0..255.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Move,
    Button(u8, bool),
    VerticalWheel,
    HorizontalWheel,
    Key(u8, bool),
    Other,
}

pub(crate) struct ExternalInput {
    forward: bool,
    keys: [bool; 256],
    buttons: [bool; 256],
    held: usize,
}

impl Default for ExternalInput {
    fn default() -> Self {
        Self {
            forward: false,
            keys: [false; 256],
            buttons: [false; 256],
            held: 0,
        }
    }
}

impl ExternalInput {
    /// Native callback order is the policy boundary. Preserve an entire chord /
    /// drag (including motion, wheel and repeats) until all external holds end.
    /// Physical and Oink input never enter this ledger.
    pub fn local(&mut self, kind: Kind, requested_forward: bool) -> bool {
        if self.held == 0 {
            self.forward = requested_forward;
        }
        let slot = match kind {
            Kind::Button(button, down) => Some((&mut self.buttons[usize::from(button)], down)),
            Kind::Key(key, down) => Some((&mut self.keys[usize::from(key)], down)),
            _ => None,
        };
        if let Some((held, down)) = slot
            && *held != down
        {
            if down {
                self.held += 1;
            } else {
                self.held -= 1;
            }
            *held = down;
        }
        !self.forward
    }

    /// Oink's ordinary capture-gap cleanup retires routed holds separately.
    /// Never synthesize a release for an external local press or infer a new
    /// DOWN from the merged OS keyboard bitmap.
    pub fn gap(&mut self) {
        self.keys.fill(false);
        self.buttons.fill(false);
        self.held = 0;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counters {
    pub move_count: u64,
    pub button_down: u64,
    pub button_up: u64,
    pub wheel_vertical: u64,
    pub wheel_horizontal: u64,
    pub key_down: u64,
    pub key_up: u64,
    pub other: u64,
    pub lower_il: u64,
    pub local: u64,
    pub downstream_rejected: u64,
    pub base_registered: u64,
    pub base_rollback: u64,
    pub position_read_failed: u64,
}

impl Counters {
    pub fn observe(&mut self, kind: Kind, lower: bool, local: bool) {
        let count = match kind {
            Kind::Move => &mut self.move_count,
            Kind::Button(_, true) => &mut self.button_down,
            Kind::Button(_, false) => &mut self.button_up,
            Kind::VerticalWheel => &mut self.wheel_vertical,
            Kind::HorizontalWheel => &mut self.wheel_horizontal,
            Kind::Key(_, true) => &mut self.key_down,
            Kind::Key(_, false) => &mut self.key_up,
            Kind::Other => &mut self.other,
        };
        *count = count.saturating_add(1);
        self.lower_il = self.lower_il.saturating_add(u64::from(lower));
        self.local = self.local.saturating_add(u64::from(local));
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn own_marks_precede_both_injected_flags() {
        for injected in [false, true] {
            for lower in [false, true] {
                assert_eq!(classify(true, false, injected, lower), Origin::Own);
                assert_eq!(classify(false, true, injected, lower), Origin::Marker);
                assert_eq!(
                    classify(false, false, injected, lower),
                    if injected || lower {
                        Origin::External
                    } else {
                        Origin::Physical
                    }
                );
            }
        }
    }

    #[test]
    fn neutral_switch_is_live_and_every_external_kind_obeys_it() {
        let mut policy = ExternalInput::default();
        for forward in [false, true, false] {
            for kind in [
                Kind::Move,
                Kind::VerticalWheel,
                Kind::HorizontalWheel,
                Kind::Other,
                Kind::Button(1, true),
                Kind::Button(1, false),
                Kind::Key(0x41, true),
                Kind::Key(0x41, false),
            ] {
                assert_eq!(policy.local(kind, forward), !forward);
            }
        }
    }

    #[test]
    fn toggling_during_a_drag_or_chord_preserves_the_whole_external_sequence() {
        for before in [false, true] {
            let mut policy = ExternalInput::default();
            assert_eq!(policy.local(Kind::Button(0, true), before), !before);
            for kind in [
                Kind::Move,
                Kind::Key(0x10, true),
                Kind::Key(0x10, true),
                Kind::Button(0, true),
                Kind::Button(1, true),
                Kind::Button(0, false),
                Kind::VerticalWheel,
                Kind::Key(0x10, false),
                Kind::Button(1, false),
            ] {
                assert_eq!(policy.local(kind, !before), !before);
            }
            assert_eq!(policy.local(Kind::Move, !before), before);
        }
    }

    #[test]
    fn latest_toggle_wins_and_capture_gap_retires_unknown_holds_without_replaying() {
        let mut policy = ExternalInput::default();
        assert!(policy.local(Kind::Key(0x10, true), false));
        assert!(policy.local(Kind::Move, true));
        assert!(policy.local(Kind::Key(0x10, false), false));
        assert!(policy.local(Kind::Move, false));
        assert!(!policy.local(Kind::Button(0, true), true));
        policy.gap();
        assert!(policy.local(Kind::Move, false));
        // No synthetic cleanup is generated; an orphan UP follows the current policy.
        assert!(policy.local(Kind::Button(0, false), false));
    }

    #[test]
    fn counters_carry_only_kind_counts_and_not_button_or_key_identity() {
        let mut a = Counters::default();
        let mut b = Counters::default();
        for (key, button, counters) in [(0x41, 0, &mut a), (0x42, 4, &mut b)] {
            for kind in [
                Kind::Move,
                Kind::Button(button, true),
                Kind::Button(button, false),
                Kind::VerticalWheel,
                Kind::HorizontalWheel,
                Kind::Key(key, true),
                Kind::Key(key, false),
                Kind::Other,
            ] {
                counters.observe(kind, true, true);
            }
        }
        assert_eq!(a, b);
        assert_eq!(a.move_count, 1);
        assert_eq!(a.button_down, 1);
        assert_eq!(a.button_up, 1);
        assert_eq!(a.key_down, 1);
        assert_eq!(a.key_up, 1);
        assert_eq!(a.wheel_vertical, 1);
        assert_eq!(a.wheel_horizontal, 1);
        assert_eq!(a.other, 1);
        assert_eq!(a.lower_il, 8);
        assert_eq!(a.local, 8);
    }
}
