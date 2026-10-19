//! A disabled device remains a source. Full pause/emergency and unreachable peers use local input.
use super::*;
impl Router {
    pub fn screen_local_input(&self) -> bool {
        self.screen_local
    }
    pub(super) fn redirect_target(&self, from: DeviceId) -> Option<ScreenPos> {
        if !self.layout.source_requires_target(from) {
            return None;
        }
        let candidates = self.layout.focus_candidates(from);
        let eligible = |p: &&ScreenPos| self.input_ready(p.screen.device);
        let preferred = if self.claim != from && self.input_ready(self.claim) {
            Some(self.claim)
        } else {
            self.last_enabled_focus
        };
        candidates
            .iter()
            .filter(eligible)
            .find(|p| Some(p.screen.device) == preferred)
            .or_else(|| candidates.iter().find(eligible))
            .cloned()
    }
    pub(super) fn enforce_screen_policy(&mut self) {
        let from = self.holder;
        if self
            .locked_device()
            .is_some_and(|target| self.layout.source_requires_target(target))
        {
            self.end_focus_lock();
        }
        let target = (!self.paused && !self.fully_paused)
            .then(|| self.redirect_target(from))
            .flatten();
        let local = self.layout.source_requires_target(self.me())
            && (self.paused || self.fully_paused || self.redirect_target(self.me()).is_none());
        if self.screen_local != local {
            self.screen_local = local;
            self.out.push(Output::Status(StatusEvent::ScreenLocalInput(local)));
        }
        let Some(target) = target else {
            return;
        };
        self.end_focus_lock();
        self.dnd_abort("focus screen disabled");
        self.cancel_source_contacts();
        self.cancel_received_contacts();
        let keys: Vec<_> = self.keys.drain().collect();
        for (code, targets) in keys {
            self.keyboard.suppress(code);
            if self.release_local_key(code, targets) && !self.key_refs.contains_key(&code) {
                self.out.push(Output::Inject(InjectEvent::Key { code, pressed: false }));
            }
        }
        let buttons: Vec<_> = self.buttons.drain().collect();
        for (button, target) in buttons {
            if self.release_local_button(button, target) && !self.button_refs.contains_key(&button) {
                self.out
                    .push(Output::Inject(InjectEvent::Button { button, pressed: false }));
            }
            self.buttons.insert(button, Target::Dropped);
        }
        let forwarded: Vec<_> = self.forwarded.drain().collect();
        for ((origin, item), target) in forwarded {
            self.press_routes.remove(&(origin, item));
            self.send(target, forwarded_release(self.admitted_boots[&origin], item));
        }
        let origins: Vec<_> = self.held.keys().copied().collect();
        for origin in origins {
            self.release_origin(origin);
        }
        self.key_repeats.clear();
        self.scroll_target = None;
        self.gesture_lock = None;
        if target.screen.device == self.me() {
            self.reclaim_focus(Some(target));
        } else {
            self.hand_off(target, None);
        }
    }
}
