use super::*;

impl Router {
    /// Observe hardware keys before owner pause, keyboard guard, or overlay gates.
    /// Repeated downs cannot rearm until the real Escape release is observed.
    pub fn observe_emergency_event(&mut self, event: &CapturedEvent, now: Micros) -> bool {
        if !event.physical {
            return false;
        }
        let InputEvent::Key { code, pressed } = event.event else {
            return false;
        };
        if !pressed {
            self.physical_keys.remove(&code);
            if code == KeyCode::ESCAPE {
                self.esc_down_at = None;
                return std::mem::take(&mut self.escape_consumed);
            }
            return self.pause_all_consumed.remove(&code);
        }
        let fresh = self.physical_keys.insert(code);
        if self.pause_all_consumed.contains(&code) {
            return true;
        }
        if fresh
            && self
                .hotkeys
                .matching(code, Mods::held(self.physical_keys.iter().copied()))
                == Some("pause_all")
        {
            self.pause_all_consumed.insert(code);
            self.trigger_emergency();
            return true;
        }
        if code != KeyCode::ESCAPE {
            return false;
        }
        if !fresh {
            return self.escape_consumed;
        }
        let held = |a, b| self.physical_keys.contains(&a) || self.physical_keys.contains(&b);
        if self.cfg.settings.emergency_combo
            && held(KeyCode::LEFT_CTRL, KeyCode::RIGHT_CTRL)
            && held(KeyCode::LEFT_ALT, KeyCode::RIGHT_ALT)
            && held(KeyCode::LEFT_SHIFT, KeyCode::RIGHT_SHIFT)
        {
            self.escape_consumed = true;
            self.trigger_emergency();
            return true;
        }
        if self.cfg.settings.emergency_hold_ms > 0 {
            self.esc_down_at = Some(now);
        }
        false
    }

    pub(super) fn emergency_deadline(&self) -> Option<Micros> {
        (self.cfg.settings.emergency_hold_ms > 0)
            .then_some(self.esc_down_at)
            .flatten()
            .map(|since| since.saturating_add(self.emergency_hold_us()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(router: &mut Router, code: KeyCode, pressed: bool, physical: bool, now: Micros) {
        router.on_local_event(
            &CapturedEvent {
                time_us: now,
                physical,
                source: SourceKind::Keyboard,
                cursor: None,
                event: InputEvent::Key { code, pressed },
            },
            now,
        );
    }
    fn stops(router: &mut Router) -> usize {
        router
            .drain_outputs()
            .iter()
            .filter(|out| matches!(out, Output::Status(StatusEvent::EmergencyStop { .. })))
            .count()
    }
    fn router() -> Router {
        Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: DeviceId([1; 32]),
                    nonce: [DeviceId([1; 32]).0[0]; 16],
                },
                "local",
                Os::Other,
            ),
            Arc::new(crate::SimpleLayout::new()),
        )
    }

    #[test]
    fn physical_escape_pauses_once_per_press_even_while_paused_or_guarded() {
        for paused in [false, true] {
            for guarded in [false, true] {
                let mut r = router();
                r.set_fully_paused(paused);
                r.set_keyboard_disabled(guarded);
                r.drain_outputs();
                key(&mut r, KeyCode::ESCAPE, true, true, 10);
                r.tick(5_000_009);
                assert_eq!(stops(&mut r), 0);
                r.tick(5_000_010);
                assert_eq!(stops(&mut r), 1);
                key(&mut r, KeyCode::ESCAPE, true, true, 6_000_000);
                r.tick(12_000_000);
                assert_eq!(stops(&mut r), 0);
                key(&mut r, KeyCode::ESCAPE, false, true, 12_000_001);
                key(&mut r, KeyCode::ESCAPE, true, true, 12_000_002);
                r.tick(17_000_002);
                assert_eq!(stops(&mut r), 1);
            }
        }
    }

    #[test]
    fn synthetic_escape_and_disabled_hold_never_arm_emergency() {
        let mut r = router();
        key(&mut r, KeyCode::ESCAPE, true, false, 0);
        r.tick(10_000_000);
        assert_eq!(stops(&mut r), 0);
        r.cfg.settings.emergency_hold_ms = 0;
        key(&mut r, KeyCode::ESCAPE, true, true, 10_000_001);
        r.tick(20_000_000);
        assert_eq!(stops(&mut r), 0);
        assert!(r.emergency_deadline().is_none());
    }

    #[test]
    fn physical_chord_works_while_guarded_and_synthetic_modifiers_do_not_count() {
        for physical in [false, true] {
            let mut r = router();
            r.set_fully_paused(true);
            r.set_keyboard_disabled(true);
            for code in [KeyCode::LEFT_CTRL, KeyCode::RIGHT_ALT, KeyCode::LEFT_SHIFT] {
                key(&mut r, code, true, physical, 0);
            }
            key(&mut r, KeyCode::ESCAPE, true, true, 1);
            assert_eq!(stops(&mut r), usize::from(physical));
        }
    }
}

#[cfg(test)]
mod pause_all_tests {
    use super::*;

    #[test]
    fn configurable_pause_all_is_physical_once_per_press_before_guards() {
        for guarded in [false, true] {
            let mut r = Router::new(
                RouterConfig::new(
                    oink_foundation::session::Boot {
                        device_id: DeviceId([1; 32]),
                        nonce: [DeviceId([1; 32]).0[0]; 16],
                    },
                    "local",
                    Os::Other,
                ),
                Arc::new(crate::SimpleLayout::new()),
            );
            r.set_hotkeys(vec![HotkeyBinding {
                chord: crate::hotkey::Chord::parse(&["Ctrl", "Alt", "Shift", "P"]).unwrap(),
                action: "pause_all".into(),
            }]);
            r.set_fully_paused(guarded);
            r.set_keyboard_disabled(guarded);
            r.drain_outputs();
            let mut send = |code, pressed, physical| {
                r.on_local_event(
                    &CapturedEvent {
                        time_us: 1,
                        physical,
                        source: SourceKind::Keyboard,
                        cursor: None,
                        event: InputEvent::Key { code, pressed },
                    },
                    1,
                )
            };
            for code in [KeyCode::LEFT_CTRL, KeyCode::RIGHT_ALT, KeyCode::LEFT_SHIFT] {
                send(code, true, true);
            }
            let p = KeyCode::keyboard(0x13);
            assert_eq!(send(p, true, true), Disposition::Swallow);
            assert_eq!(send(p, true, true), Disposition::Swallow);
            assert_eq!(send(p, false, true), Disposition::Swallow);
            let outputs = r.drain_outputs();
            assert_eq!(
                outputs
                    .iter()
                    .filter(|o| matches!(o, Output::Status(StatusEvent::EmergencyStop { .. })))
                    .count(),
                1
            );
            assert!(
                !outputs
                    .iter()
                    .any(|o| matches!(o, Output::Status(StatusEvent::Hotkey { .. })))
            );
        }
    }
}
