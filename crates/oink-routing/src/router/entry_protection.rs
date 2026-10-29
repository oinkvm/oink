//! Owner policy survives focus visits; only the active focus pin is exclusive.
use super::*;
use crate::control::EntryProtection;

#[derive(Default)]
pub(super) struct EntryProtectionState {
    last_sent: Option<EntryProtection>,
    peers: HashMap<DeviceId, EntryProtection>,
}

impl Router {
    pub(super) fn local_entry_protection(&self) -> EntryProtection {
        EntryProtection {
            locked: self.owner_pointer_locked && !self.paused,
            game: self.owner_game_active() && !self.paused,
            passive_displays: if self.paused || self.cfg.os != Os::Windows {
                Vec::new()
            } else {
                self.displays
                    .iter()
                    .filter(|d| {
                        self.cursor_clip
                            .rect
                            .is_some_and(|clip| cursor_clip::passive_clip(clip, d.bounds))
                    })
                    .map(|d| d.key.clone())
                    .collect()
            },
        }
    }
    pub(super) fn sync_entry_protection(&mut self) {
        let state = self.local_entry_protection();
        if self.entry_protection.last_sent.as_ref() != Some(&state) {
            self.entry_protection.last_sent = Some(state.clone());
            self.resistance = None;
            self.broadcast(Message::Control(ControlMsg::EntryProtection { state }));
        }
    }
    pub(super) fn send_entry_protection(&mut self, peer: DeviceId) {
        self.send(
            peer,
            Message::Control(ControlMsg::EntryProtection {
                state: self.local_entry_protection(),
            }),
        );
    }
    pub(super) fn receive_entry_protection(&mut self, from: DeviceId, state: EntryProtection) {
        if self.entry_protection.peers.get(&from) != Some(&state) {
            self.entry_protection.peers.insert(from, state);
            self.resistance = None;
        }
    }
    pub(super) fn forget_entry_protection(&mut self, peer: DeviceId) {
        if self.entry_protection.peers.remove(&peer).is_some() {
            self.resistance = None;
        }
    }
    pub(super) fn protected_entry(&self, screen: &ScreenId) -> bool {
        let local;
        let state = if screen.device == self.me() {
            local = self.local_entry_protection();
            &local
        } else {
            let Some(state) = self.entry_protection.peers.get(&screen.device) else {
                return false;
            };
            state
        };
        state.locked || state.game || state.passive_displays.contains(&screen.display)
    }
}
