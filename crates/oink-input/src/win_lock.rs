//! Windows lock chord over the target's already mapped, injected HID key stream.
use oink_foundation::input::KeyCode;
use std::collections::HashSet;

const L: KeyCode = KeyCode::keyboard(0x0f);

#[derive(Debug, Default)]
pub(crate) struct LockChord {
    /// Successfully injected downs; consumed keys never enter this ledger.
    held: HashSet<KeyCode>,
    /// Consume repeats and the matching up even if lock fails or a desktop changes.
    consumed: HashSet<KeyCode>,
}

impl LockChord {
    pub(crate) fn key<E>(
        &mut self,
        code: KeyCode,
        pressed: bool,
        mut send: impl FnMut(KeyCode, bool) -> Result<(), E>,
        mut lock: impl FnMut() -> Result<(), E>,
    ) -> Result<(), E> {
        if self.consumed.contains(&code) {
            if !pressed {
                self.consumed.remove(&code);
            }
            return Ok(());
        }
        let win_down = self.held.contains(&KeyCode::LEFT_GUI) || self.held.contains(&KeyCode::RIGHT_GUI);
        let other_modifier = self.held.iter().any(|key| key.is_modifier() && !is_win(*key));
        if code == L && pressed && !self.held.contains(&L) && win_down && !other_modifier {
            self.consumed.insert(L);
            // L is never submitted. Mask the otherwise bare Win up so it does not open Start.
            // Release before the asynchronous lock can switch desktops: the helper's held
            // ledger is cleared on Default, rather than depending on delivery on Winlogon.
            send(KeyCode::MENU_MASK, true)?;
            send(KeyCode::MENU_MASK, false)?;
            for win in [KeyCode::LEFT_GUI, KeyCode::RIGHT_GUI] {
                if self.held.contains(&win) {
                    send(win, false)?;
                    self.held.remove(&win);
                    self.consumed.insert(win);
                }
            }
            // Keep consumed keys through failures too: never retry lock on L autorepeat,
            // replay an L, or reintroduce Win while the source still holds it physically.
            return lock();
        }
        send(code, pressed)?;
        if pressed {
            self.held.insert(code);
        } else {
            self.held.remove(&code);
        }
        Ok(())
    }
}

fn is_win(code: KeyCode) -> bool {
    matches!(code, KeyCode::LEFT_GUI | KeyCode::RIGHT_GUI)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        chord: LockChord,
        native_held: std::cell::RefCell<HashSet<KeyCode>>,
        events: Vec<(KeyCode, bool)>,
        locks: usize,
    }
    impl Fake {
        fn key(&mut self, code: KeyCode, pressed: bool) {
            self.chord
                .key(
                    code,
                    pressed,
                    |code, pressed| {
                        self.events.push((code, pressed));
                        if pressed {
                            self.native_held.borrow_mut().insert(code);
                        } else {
                            self.native_held.borrow_mut().remove(&code);
                        }
                        Ok::<_, ()>(())
                    },
                    || {
                        // Fake only: neither SendInput nor LockWorkStation is called in tests.
                        assert!(!self.native_held.borrow().contains(&L));
                        assert!(!self.native_held.borrow().iter().any(|key| is_win(*key)));
                        self.locks += 1;
                        Ok(())
                    },
                )
                .unwrap();
        }
    }

    #[test]
    fn either_win_l_locks_once_and_clears_both_injection_ledgers_before_lock() {
        for wins in [
            vec![KeyCode::LEFT_GUI],
            vec![KeyCode::RIGHT_GUI],
            vec![KeyCode::LEFT_GUI, KeyCode::RIGHT_GUI],
        ] {
            let mut f = Fake::default();
            for &win in &wins {
                f.key(win, true);
            }
            f.key(L, true);
            assert_eq!(f.locks, 1);
            assert!(f.chord.held.is_empty());
            assert!(f.native_held.borrow().is_empty());
            let after_lock = f.events.clone();
            // Source still holds Win across lock/unlock. Repeats or resumed snapshots must
            // not reassert it; physical/reconciled ups close the consumed pairs harmlessly.
            for &win in &wins {
                f.key(win, true);
            }
            f.key(L, true);
            for &win in &wins {
                f.key(win, false);
            }
            f.key(L, true);
            f.key(L, false);
            assert_eq!(f.locks, 1);
            assert_eq!(f.events, after_lock);
            assert!(f.chord.held.is_empty() && f.chord.consumed.is_empty());
            assert!(f.native_held.borrow().is_empty());
            // Ordinary typing and a subsequent deliberate lock chord still work.
            f.key(L, true);
            f.key(L, false);
            f.key(wins[0], true);
            f.key(L, true);
            assert_eq!(f.locks, 2);
        }
    }

    #[test]
    fn extra_modifier_passes_every_key_through() {
        for modifier in [
            KeyCode::LEFT_SHIFT,
            KeyCode::RIGHT_SHIFT,
            KeyCode::LEFT_CTRL,
            KeyCode::RIGHT_CTRL,
            KeyCode::LEFT_ALT,
            KeyCode::RIGHT_ALT,
        ] {
            let mut f = Fake::default();
            let events = [
                (KeyCode::LEFT_GUI, true),
                (modifier, true),
                (L, true),
                (L, true),
                (L, false),
                (modifier, false),
                (KeyCode::LEFT_GUI, false),
            ];
            for (code, pressed) in events {
                f.key(code, pressed);
            }
            assert_eq!(f.locks, 0);
            assert_eq!(f.events, events);
            assert!(f.chord.held.is_empty() && f.native_held.borrow().is_empty());
        }
    }

    #[test]
    fn l_first_then_win_including_l_repeat_does_not_lock() {
        let mut f = Fake::default();
        let events = [
            (L, true),
            (KeyCode::LEFT_GUI, true),
            (L, true),
            (L, false),
            (KeyCode::LEFT_GUI, false),
        ];
        for (code, pressed) in events {
            f.key(code, pressed);
        }
        assert_eq!(f.locks, 0);
        assert_eq!(f.events, events);
        assert!(f.chord.held.is_empty() && f.native_held.borrow().is_empty());
    }

    #[test]
    fn failed_lock_consumes_chord_until_up_without_retry_or_l_injection() {
        let mut chord = LockChord::default();
        chord
            .key(KeyCode::LEFT_GUI, true, |_, _| Ok::<_, ()>(()), || panic!())
            .unwrap();
        assert!(chord.key(L, true, |_, _| Ok(()), || Err(())).is_err());
        assert!(chord.held.is_empty());
        for (code, pressed) in [(L, true), (KeyCode::LEFT_GUI, false), (L, false)] {
            chord
                .key::<()>(code, pressed, |_, _| panic!("consumed key"), || panic!("repeat lock"))
                .unwrap();
        }
        assert!(chord.consumed.is_empty());
    }

    #[test]
    fn failed_win_release_does_not_lock_or_forget_the_native_hold() {
        let mut chord = LockChord::default();
        chord
            .key(KeyCode::LEFT_GUI, true, |_, _| Ok::<_, ()>(()), || panic!())
            .unwrap();
        assert!(
            chord
                .key(
                    L,
                    true,
                    |code, pressed| {
                        assert_ne!(code, L);
                        if code == KeyCode::LEFT_GUI && !pressed {
                            Err(())
                        } else {
                            Ok(())
                        }
                    },
                    || panic!("cannot lock until releases succeed"),
                )
                .is_err()
        );
        assert!(chord.held.contains(&KeyCode::LEFT_GUI));
        chord
            .key::<()>(KeyCode::LEFT_GUI, false, |_, _| Ok(()), || panic!())
            .unwrap();
        chord
            .key::<()>(L, false, |_, _| panic!("L was never injected"), || panic!())
            .unwrap();
        assert!(chord.held.is_empty() && chord.consumed.is_empty());
    }

    #[test]
    fn remapped_ctrl_l_and_suppressed_l_do_not_lock() {
        let mut f = Fake::default();
        // The backend only sees routing's mapped result, never the original source Meta.
        f.key(KeyCode::LEFT_CTRL, true);
        f.key(L, true);
        f.key(L, false);
        f.key(KeyCode::LEFT_CTRL, false);
        // A source hotkey has swallowed L; only its modifier pair reaches the backend.
        f.key(KeyCode::LEFT_GUI, true);
        f.key(KeyCode::MENU_MASK, true);
        f.key(KeyCode::MENU_MASK, false);
        f.key(KeyCode::LEFT_GUI, false);
        assert_eq!(f.locks, 0);
        assert!(f.chord.held.is_empty() && f.native_held.borrow().is_empty());
    }
}
