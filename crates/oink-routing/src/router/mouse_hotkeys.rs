use super::*;

impl Router {
    /// A short UI lease prevents a disconnected recorder from intercepting future
    /// clicks forever. Cancellation is scoped so an old recorder cannot stop a new one.
    pub fn record_mouse_shortcut(&mut self, token: String, active: bool, now: Micros) {
        if active {
            self.mouse_shortcut_recording = Some((token, now.saturating_add(60_000_000)));
        } else if self
            .mouse_shortcut_recording
            .as_ref()
            .is_some_and(|(id, _)| *id == token)
        {
            self.mouse_shortcut_recording = None;
        }
    }

    pub(super) fn mouse_shortcut_event(&mut self, event: &CapturedEvent, now: Micros) -> Option<Disposition> {
        let InputEvent::Button { button, pressed } = event.event else {
            return None;
        };
        let code = crate::hotkey::mouse_key(button)?;
        if !pressed {
            self.lock_escape_keys.remove(&code);
            self.publish_lock_escape();
            if self.consumed_mouse_shortcuts.remove(&button) {
                if let Some(action) = self.mouse_release_actions.remove(&button) {
                    if action == "pause_all" {
                        self.trigger_emergency();
                    } else {
                        self.out.push(Output::Status(StatusEvent::Hotkey { action }));
                    }
                }
                return Some(Disposition::Swallow);
            }
            return None;
        }
        if self.consumed_mouse_shortcuts.contains(&button) {
            return Some(Disposition::Swallow);
        }
        if !self.capture_available {
            return None;
        }
        let held = Mods::held(self.keys.keys().copied());
        if self
            .mouse_shortcut_recording
            .as_ref()
            .is_some_and(|(_, deadline)| now < *deadline)
        {
            let (token, _) = self.mouse_shortcut_recording.take().unwrap();
            let mut keys: Vec<String> = [
                (Mods::CTRL, "Ctrl"),
                (Mods::ALT, "Alt"),
                (Mods::SHIFT, "Shift"),
                (Mods::META, "Meta"),
            ]
            .into_iter()
            .filter(|(modifier, _)| held.contains(*modifier))
            .map(|(_, name)| name.into())
            .collect();
            keys.push(crate::hotkey::mouse_name(button).unwrap().into());
            self.consumed_mouse_shortcuts.insert(button);
            self.out
                .push(Output::Status(StatusEvent::MouseShortcutRecorded { token, keys }));
            return Some(Disposition::Swallow);
        }
        self.mouse_shortcut_recording = None;
        if self.fully_paused {
            return None;
        }
        let escape = self
            .hotkeys
            .bindings()
            .iter()
            .any(|b| b.action == "lock_escape" && b.chord.key == code);
        let action = if escape {
            "lock_escape"
        } else {
            self.hotkeys.matching(code, held)?
        }
        .to_owned();
        if action == "lock_remote_windows" && self.windows_lock_target().is_none() {
            return None;
        }
        self.consumed_mouse_shortcuts.insert(button);
        tracing::info!(%action, "mouse hotkey pressed");
        if escape {
            self.lock_escape_keys.insert(code);
            self.publish_lock_escape();
        } else if action == "pause_all" || action == "toggle_sharing" {
            // Pause withdraws every native tap/hook. Dispatch only after consuming
            // this UP, otherwise native browser navigation can see an orphan UP.
            // Physical Escape emergency handling still withdraws immediately.
            self.mask_menu_keys();
            self.mouse_release_actions.insert(button, action);
        } else {
            if action == "lock_remote_windows" {
                self.request_windows_lock(self.windows_lock_target().unwrap());
            } else {
                self.mask_menu_keys();
            }
            if action == "lock_focus" {
                self.toggle_locked();
            }
            self.out.push(Output::Status(StatusEvent::Hotkey { action }));
        }
        Some(Disposition::Swallow)
    }
}
