//! Committed input-method text (`InputMsg::Text`, wire `InputText` on I0,
//! protocol §12) at the focus holder. It is routed like a key press: the
//! source sends it to its focus holder, a stale receiver forwards it, and the
//! ordinary remote-input gate in `on_input` (admitted Boot, not paused, joined
//! routing session) applies before this point. Here only the per-Boot
//! sequence is checked; dropped text is logged without its content.
//!
//! A backend that cannot take a long text at once (Windows, `TextPacing`)
//! gets admitted text in slices, one per pause, released from `tick`. Other
//! input is injected between slices; pausing discards what is left.
use super::*;
use oink_foundation::input::InjectedText;
use oink_foundation::session::Boot;

impl Router {
    /// The injection backend's limit for one text insertion (Windows); `None`
    /// inserts each admitted text at once.
    pub fn set_text_pacing(&mut self, pacing: Option<oink_foundation::input::TextPacing>) {
        self.text_pacing = pacing;
    }

    /// Holder side: inject `text` from `origin` unless its `seq` is not newer.
    /// Returns whether it was accepted.
    pub(super) fn remote_text(&mut self, origin: Boot, seq: u64, text: String, now: Micros) -> bool {
        let units = text.encode_utf16().count();
        if self
            .text_seq
            .get(&origin.device_id)
            .is_some_and(|(boot, last)| *boot == origin && seq <= *last)
        {
            tracing::info!(origin = ?origin.device_id, seq, units, "remote text: duplicate dropped");
            self.refuse_text(origin.device_id, seq, crate::control::TextRefusal::Duplicate);
            return false;
        }
        self.text_seq.insert(origin.device_id, (origin, seq));
        if self.cfg.screenless {
            // The Android helper has no text channel.
            tracing::info!(origin = ?origin.device_id, seq, units, "remote text: no text injection on this device");
            self.refuse_text(origin.device_id, seq, crate::control::TextRefusal::Unsupported);
            return false;
        }
        tracing::debug!(origin = ?origin.device_id, seq, units, "remote text: injecting");
        // Pacing happens here, not by sleeping in the injection thread:
        // other members' keys and motion go between the slices instead of
        // queueing behind the whole text (and aging into the 5 s backlog).
        match self.text_pacing {
            Some(pacing) => self.text_slices.extend(
                oink_foundation::text::utf16_chunks(&text, pacing.units)
                    .into_iter()
                    .map(|slice| String::from_utf16(&slice).expect("slices keep surrogate pairs whole")),
            ),
            None => self.text_slices.push_back(text),
        }
        self.release_text(now);
        true
    }

    fn refuse_text(&mut self, origin: DeviceId, seq: u64, reason: crate::control::TextRefusal) {
        self.send(origin, Message::Control(ControlMsg::TextRefused { seq, reason }));
    }

    /// Inject the admitted slices that are due: one per `TextPacing::pause`,
    /// the pause also separating consecutive texts.
    pub(super) fn release_text(&mut self, now: Micros) {
        while now >= self.text_next
            && let Some(slice) = self.text_slices.pop_front()
        {
            if let Some(pacing) = self.text_pacing {
                self.text_next = now.saturating_add(pacing.pause.as_micros() as Micros);
            }
            self.inject_text(slice, now);
        }
    }

    /// Admitted text is dropped with the rest of remote input when this
    /// device pauses; only its length is logged.
    pub(super) fn discard_text(&mut self, reason: &'static str) {
        if !self.text_slices.is_empty() {
            let units: usize = self
                .text_slices
                .drain(..)
                .map(|slice| slice.encode_utf16().count())
                .sum();
            tracing::info!(units, reason, "remote text: discarded before injection");
        }
    }

    fn inject_text(&mut self, text: String, now: Micros) {
        // Remote control is busy, exactly as for a remote key press.
        self.update_remote_until = now.saturating_add(60_000_000);
        // Held modifiers are read per slice: other input may change them in between.
        let lifted = self.text_lifted_modifiers();
        if !lifted.is_empty() {
            self.inject_menu_mask();
            for &code in &lifted {
                self.out.push(Output::Inject(InjectEvent::Key { code, pressed: false }));
            }
        }
        self.out.push(Output::Inject(InjectEvent::Text(InjectedText(text))));
        if !lifted.is_empty() {
            for &code in lifted.iter().rev() {
                self.out.push(Output::Inject(InjectEvent::Key { code, pressed: true }));
            }
            self.inject_menu_mask();
        }
    }

    /// Windows only: Ctrl/Alt/Win that Oink itself injected for a remote
    /// source and still holds (`key_refs`, the ledger HeldSync and link loss
    /// release). OS behaviour (a Windows test machine, 2026-11-09): with Alt down a
    /// `KEYEVENTF_UNICODE` space became Alt+Space and opened the system menu;
    /// Ctrl/Win make the press a chord. Why this shape: the text is wrapped in
    /// releases and re-presses of exactly these keys, so the ledger is the
    /// same afterwards; a menu-mask tap before each change keeps an Alt/Win
    /// release from opening the menu bar or Start. Keys held by anything else
    /// (the physical keyboard, other programs) are never touched or inferred
    /// from the system key state. macOS posts the text with cleared flags and
    /// needs no change (probe 2026-11-09: held Command/Control/Option ignored).
    fn text_lifted_modifiers(&self) -> Vec<KeyCode> {
        if self.cfg.os != Os::Windows {
            return Vec::new();
        }
        [0xE0, 0xE4, 0xE2, 0xE6, 0xE3, 0xE7]
            .map(KeyCode::keyboard)
            .into_iter()
            .filter(|code| self.key_refs.contains_key(code))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PC: DeviceId = DeviceId([1; 32]);
    const PHONE: DeviceId = DeviceId([9; 32]);

    fn boot(device: DeviceId, nonce: u8) -> Boot {
        Boot {
            device_id: device,
            nonce: [nonce; 16],
        }
    }

    fn joined() -> Router {
        joined_on(Os::Windows)
    }

    fn joined_on(os: Os) -> Router {
        let mut router = Router::new(
            RouterConfig::new(boot(PC, 1), "test", os),
            Arc::new(crate::SimpleLayout::new()),
        );
        router.session = [1; 16];
        router.focus_joined = true;
        router.fresh_join = false;
        router.epoch = 1;
        router.focus_scope = Some(router.new_focus_scope());
        router.admit_boot(boot(PHONE, 9));
        router.drain_outputs();
        router
    }

    fn injected(router: &mut Router) -> Vec<String> {
        router
            .drain_outputs()
            .into_iter()
            .filter_map(|output| match output {
                Output::Inject(InjectEvent::Text(text)) => Some(text.0),
                _ => None,
            })
            .collect()
    }

    /// `InputText` from the phone over I0, through the ordinary remote gate.
    fn deliver(router: &mut Router, seq: u64, text: impl Into<String>, now: Micros) -> bool {
        let before = router.text_seq.get(&PHONE).copied();
        router.test_message(PHONE, Message::Input(InputMsg::Text { seq, text: text.into() }), now);
        router.text_seq.get(&PHONE).copied() != before
    }

    #[test]
    fn admitted_member_text_is_injected_once_per_sequence() {
        let mut router = joined();
        assert!(deliver(&mut router, 1, "中文", 10));
        assert!(!deliver(&mut router, 1, "中文", 11));
        assert!(deliver(&mut router, 2, "😀", 12));
        assert_eq!(injected(&mut router), ["中文", "😀"]);
        assert!(router.update_remote_until > 12);
    }

    #[test]
    fn text_follows_focus_like_a_key_press() {
        let mut router = joined();
        let other = DeviceId([2; 32]);
        router.test_connect(other, 0);
        router.holder = other;
        router.drain_outputs();
        assert!(!deliver(&mut router, 3, "a", 13));
        let outputs = router.drain_outputs();
        assert!(outputs.iter().any(|output| matches!(
            output,
            Output::Send {
                to,
                msg: Message::Input(InputMsg::Forwarded { msg, .. }),
                ..
            } if *to == other && matches!(**msg, InputMsg::Text { seq: 3, .. })
        )));
        assert!(
            !outputs
                .iter()
                .any(|output| matches!(output, Output::Inject(InjectEvent::Text(_))))
        );
    }

    #[test]
    fn a_new_boot_restarts_its_sequence() {
        let mut router = joined();
        assert!(deliver(&mut router, 5, "a", 0));
        // The phone restarted: a new Boot attaches (and retires the old one).
        assert!(router.attach_input(boot(PHONE, 10), [10; 16], true, true, FeelProfile::default(), 0));
        router.input_message(
            PHONE,
            Message::Input(InputMsg::Text {
                seq: 1,
                text: "b".into(),
            }),
            0,
        );
        assert_eq!(injected(&mut router), ["a", "b"]);
    }

    #[test]
    fn text_is_dropped_while_paused_or_before_joining() {
        let mut router = joined();
        router.set_paused(true);
        router.drain_outputs();
        assert!(!deliver(&mut router, 1, "x", 0));
        router.set_paused(false);
        router.drain_outputs();
        router.epoch = 0;
        assert!(!deliver(&mut router, 1, "x", 0));
        router.epoch = 1;
        router.focus_scope = None;
        assert!(!deliver(&mut router, 1, "x", 0));
        assert!(injected(&mut router).is_empty());
        // Dropped text never consumed the sequence number.
        router.focus_scope = Some(router.new_focus_scope());
        assert!(deliver(&mut router, 1, "x", 0));
    }

    const KEYBOARD: DeviceId = DeviceId([5; 32]);

    /// A remote keyboard source holds Ctrl at this target: Oink injected it.
    fn remote_ctrl_held(router: &mut Router) -> KeyCode {
        router.test_message(
            KEYBOARD,
            Message::Input(InputMsg::Key {
                code: KeyCode::LEFT_CTRL,
                pressed: true,
            }),
            1,
        );
        let mapped: Vec<KeyCode> = router
            .drain_outputs()
            .into_iter()
            .filter_map(|output| match output {
                Output::Inject(InjectEvent::Key { code, pressed: true }) => Some(code),
                _ => None,
            })
            .collect();
        assert_eq!(mapped.len(), 1, "{mapped:?}");
        assert!(router.key_refs.contains_key(&mapped[0]));
        mapped[0]
    }

    fn keys_and_text(router: &mut Router) -> Vec<(KeyCode, bool)> {
        router
            .drain_outputs()
            .into_iter()
            .filter_map(|output| match output {
                Output::Inject(InjectEvent::Key { code, pressed }) => Some((code, pressed)),
                Output::Inject(InjectEvent::Text(_)) => Some((KeyCode::NONE, true)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn without_oink_held_modifiers_no_modifier_event_is_sent() {
        let mut router = joined();
        assert!(deliver(&mut router, 1, "a", 0));
        assert_eq!(keys_and_text(&mut router), [(KeyCode::NONE, true)]);
    }

    #[test]
    fn windows_lifts_and_restores_only_oinks_own_held_ctrl() {
        let mut router = joined();
        let ctrl = remote_ctrl_held(&mut router);
        let refs = router.key_refs.clone();
        assert!(deliver(&mut router, 1, "a b", 2));
        assert_eq!(
            keys_and_text(&mut router),
            [
                (KeyCode::MENU_MASK, true),
                (KeyCode::MENU_MASK, false),
                (ctrl, false),
                (KeyCode::NONE, true),
                (ctrl, true),
                (KeyCode::MENU_MASK, true),
                (KeyCode::MENU_MASK, false),
            ]
        );
        // The ledger is unchanged, so the source's own release still ends it.
        assert_eq!(router.key_refs, refs);
        router.test_message(
            KEYBOARD,
            Message::Input(InputMsg::Key {
                code: KeyCode::LEFT_CTRL,
                pressed: false,
            }),
            3,
        );
        assert_eq!(keys_and_text(&mut router), [(ctrl, false)]);
        assert!(router.key_refs.is_empty());
    }

    const PACING: oink_foundation::input::TextPacing = oink_foundation::input::TextPacing {
        units: 512,
        pause: std::time::Duration::from_millis(30),
    };

    #[test]
    fn paced_text_is_released_one_slice_per_pause_without_holding_other_input() {
        let mut router = joined();
        router.set_text_pacing(Some(PACING));
        let text = format!("{}中文😀", "a".repeat(1300));
        assert!(deliver(&mut router, 1, text.clone(), 1_000));
        let mut slices = injected(&mut router);
        assert_eq!(slices, ["a".repeat(512)]);
        assert!(router.next_deadline().is_some_and(|due| due <= 31_000));
        // A remote key press between slices is injected at once, not behind
        // the rest of the text; the next slice lifts the Ctrl it now holds.
        let ctrl = remote_ctrl_held(&mut router);
        router.tick(30_999);
        assert!(injected(&mut router).is_empty());
        router.tick(31_000);
        let wrapped = router.drain_outputs();
        let keys: Vec<_> = wrapped
            .iter()
            .filter_map(|output| match output {
                Output::Inject(InjectEvent::Key { code, pressed }) => Some((*code, *pressed)),
                Output::Inject(InjectEvent::Text(text)) => {
                    slices.push(text.0.clone());
                    Some((KeyCode::NONE, true))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            keys,
            [
                (KeyCode::MENU_MASK, true),
                (KeyCode::MENU_MASK, false),
                (ctrl, false),
                (KeyCode::NONE, true),
                (ctrl, true),
                (KeyCode::MENU_MASK, true),
                (KeyCode::MENU_MASK, false),
            ]
        );
        router.tick(61_000);
        slices.extend(injected(&mut router));
        assert_eq!(slices.len(), 3);
        assert!(slices.iter().all(|slice| slice.encode_utf16().count() <= 512));
        assert_eq!(slices.concat(), text);
        router.tick(1_000_000);
        assert!(injected(&mut router).is_empty());
    }

    #[test]
    fn consecutive_texts_share_the_pause() {
        let mut router = joined();
        router.set_text_pacing(Some(PACING));
        deliver(&mut router, 1, "a".repeat(512), 0);
        deliver(&mut router, 2, "b", 10);
        assert_eq!(injected(&mut router), ["a".repeat(512)]);
        router.tick(30_000);
        assert_eq!(injected(&mut router), ["b"]);
    }

    #[test]
    fn pausing_discards_text_not_injected_yet() {
        let mut router = joined();
        router.set_text_pacing(Some(PACING));
        deliver(&mut router, 1, "a".repeat(2000), 0);
        assert_eq!(injected(&mut router).len(), 1);
        router.set_paused(true);
        router.set_paused(false);
        router.tick(1_000_000);
        assert!(injected(&mut router).is_empty());
    }

    #[test]
    fn without_pacing_a_text_is_one_insertion() {
        let mut router = joined();
        let text = "a".repeat(4000);
        deliver(&mut router, 1, text.clone(), 0);
        assert_eq!(injected(&mut router), [text]);
    }

    #[test]
    fn macos_never_touches_modifiers_for_text() {
        let mut router = joined_on(Os::MacOS);
        remote_ctrl_held(&mut router);
        assert!(deliver(&mut router, 1, "a", 2));
        assert_eq!(keys_and_text(&mut router), [(KeyCode::NONE, true)]);
    }
}
