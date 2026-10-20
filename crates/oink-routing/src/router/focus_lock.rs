//! The holder serializes physical requests. Every member gossips the resulting
//! record and its unlock tombstone; loss of the target also publishes an unlock.
use super::*;
use crate::control::FocusLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusLockError {
    Paused,
    Unavailable,
}

impl Router {
    pub fn locked_device(&self) -> Option<DeviceId> {
        self.focus_lock.as_ref().filter(|s| s.locked).map(|s| s.holder)
    }
    pub fn locked(&self) -> bool {
        self.locked_device().is_some()
    }
    /// Manual focus pin and game capture use the same edge/handoff gate. Game
    /// capture additionally selects relative input; manual unlock doesn't turn it off.
    pub(super) fn focus_pinned(&self) -> bool {
        self.locked() || self.game_mode().active
    }
    pub fn set_locked(&mut self, locked: bool) {
        self.request_focus_lock(Some(locked));
    }
    /// Screen menu commands execute on the screen owner, which can release its
    /// own pin regardless of which member requested or authored the lock.
    pub fn release_owned_focus_lock(&mut self) {
        if self.locked_device() == Some(self.me()) {
            self.end_focus_lock();
        }
    }
    /// Request a normal handoff followed by a lock at the accepted target epoch.
    /// Success means admitted for delivery; only the target's lock state confirms it.
    pub fn lock_focus_to(&mut self, target: DeviceId) -> Result<(), FocusLockError> {
        if self.paused {
            return Err(FocusLockError::Paused);
        }
        if !self.capture_available
            || !self.input_ready(target)
            || !self.layout.contains_device(target)
            || self.layout.source_requires_target(target)
        {
            return Err(FocusLockError::Unavailable);
        }
        if self.holder != self.me() {
            if !self.group_focus_status().participating {
                return Err(FocusLockError::Unavailable);
            }
            self.send(
                self.holder,
                Message::Control(ControlMsg::SetFocusLock {
                    epoch: self.epoch,
                    locked: Some(true),
                    target: Some(target),
                }),
            );
            return Ok(());
        }
        if target == self.me() {
            self.change_focus_lock(Some(true));
            return Ok(());
        }
        let Some(destination) = self
            .layout
            .focus_candidates(self.me())
            .into_iter()
            .find(|position| position.screen.device == target)
        else {
            return Err(FocusLockError::Unavailable);
        };
        self.end_focus_lock();
        self.hand_off(destination, None);
        self.send(
            target,
            Message::Control(ControlMsg::SetFocusLock {
                epoch: self.epoch,
                locked: Some(true),
                target: None,
            }),
        );
        Ok(())
    }

    pub fn toggle_locked(&mut self) {
        self.request_focus_lock(None);
    }
    fn request_focus_lock(&mut self, locked: Option<bool>) {
        if self.paused {
            return;
        }
        let holder = self.locked_device().unwrap_or(self.holder);
        if self.layout.source_requires_target(holder) {
            return;
        }
        let epoch = self
            .focus_lock
            .as_ref()
            .filter(|s| s.locked)
            .map_or(self.epoch, |s| s.epoch);
        if holder != self.me() {
            self.send(
                holder,
                Message::Control(ControlMsg::SetFocusLock {
                    epoch,
                    locked,
                    target: None,
                }),
            );
        } else {
            self.change_focus_lock(locked);
        }
    }
    pub(super) fn change_focus_lock(&mut self, locked: Option<bool>) {
        if self.paused || !self.capture_available {
            return;
        }
        let locked = locked.unwrap_or(!self.locked());
        if locked && self.layout.source_requires_target(self.holder) {
            return;
        }
        if locked == self.locked() {
            return;
        }
        // A previously independent local cursor must become a real group focus,
        // at its existing position. An established focus does not move or change epoch.
        if locked && (self.fresh_join || self.epoch == 0 || self.claim != self.holder) {
            self.fresh_join = true;
            self.reclaim_focus(None);
        }
        let state = FocusLock {
            revision: self.focus_lock.as_ref().map_or(1, |s| s.revision + 1),
            author: self.me(),
            epoch: self.epoch,
            holder: self.holder,
            session: self.session,
            scope: self.new_focus_scope(),
            locked,
        };
        self.apply_focus_lock(state.clone());
        self.broadcast(Message::Control(ControlMsg::FocusLock { state }));
        self.validate_focus_lock_layout();
    }
    pub(super) fn end_focus_lock(&mut self) {
        let Some(mut state) = self.focus_lock.clone().filter(|s| s.locked) else {
            return;
        };
        state.revision += 1;
        state.author = self.me();
        state.locked = false;
        self.apply_focus_lock(state.clone());
        self.broadcast(Message::Control(ControlMsg::FocusLock { state }));
    }
    fn apply_focus_lock(&mut self, state: FocusLock) {
        let locked = state.locked;
        tracing::info!(locked, holder = ?state.holder, revision = state.revision, "group focus lock changed");
        self.focus_lock = Some(state);
        self.reset_resistance();
        if locked {
            self.gap_cancel();
            self.enter_due = None;
            // Cancel cross-device file drag without dropping it; ordinary button
            // ownership stays on its original target and its physical up still works.
            self.dnd_abort("focus locked");
        }
        self.diagnostic(
            if locked {
                DecisionKind::Lock
            } else {
                DecisionKind::Unlock
            },
            DecisionReason::UserRequest,
            None,
        );
        self.out.push(Output::Status(StatusEvent::FocusLock {
            holder: self.locked_device(),
        }));
        self.push_focus_status();
    }
    pub(super) fn receive_focus_lock(&mut self, state: FocusLock) {
        if self
            .focus_lock
            .as_ref()
            .is_some_and(|s| (s.revision, s.author) >= (state.revision, state.author))
        {
            return;
        }
        let active = state.locked;
        let holder = state.holder;
        if active {
            let session = if holder == self.me() {
                Some(self.session)
            } else {
                self.peers.get(&holder).and_then(|peer| peer.session)
            };
            if session != Some(state.session) || !self.scope_allows(Some(&state.scope), holder) {
                return;
            }
        }
        if active && self.layout.source_requires_target(holder) {
            return;
        }
        let epoch = state.epoch;
        let scope = state.scope.clone();
        self.apply_focus_lock(state);
        if active {
            // This record carries the same incarnation proof as focus gossip.
            if !self.paused
                && (epoch, holder) >= (self.epoch, self.claim)
                && ((epoch, holder) != (self.epoch, self.holder)
                    || !self.scope_allows(self.focus_scope.as_ref(), holder))
            {
                self.focus_scope = Some(scope);
                self.adopt_claim(epoch, holder);
            }
            if (epoch, holder) < (self.epoch, self.claim) && holder != self.me() {
                // An uninformed fresh member may already have claimed its own
                // cursor. Ask the pin's authority to fence that claim; never
                // turn the group lock off merely because this claim is newer.
                self.send_focus_state(holder);
                if !self.paused
                    && self.input_ready(holder)
                    && self.scope_allows(self.focus_scope.as_ref(), holder)
                    && self.holder != holder
                {
                    self.become_source(self.epoch, holder);
                }
            }
            self.validate_focus_lock_session();
            self.validate_focus_lock_layout();
            if holder == self.me() && (self.paused || !self.capture_available) {
                self.end_focus_lock();
            }
        }
    }
    /// Fence an uninformed member's new claim, without moving the held pointer
    /// or resetting game capture. The lock's creation epoch is a stable request
    /// token; current focus epoch is carried alongside it in FocusState.
    pub(super) fn renew_focus_lock(&mut self, seen_epoch: u64) {
        let Some(mut state) = self.focus_lock.clone().filter(|s| s.locked && s.holder == self.me()) else {
            return;
        };
        self.epoch = seen_epoch + 1;
        self.claim = self.me();
        self.focus_scope = Some(self.new_focus_scope());
        self.focus_joined = true;
        self.fresh_join = false;
        self.unconfirmed_since = None;
        self.rejoin = false;
        self.keep_game_focus_epoch();
        state.revision += 1;
        state.author = self.me();
        state.scope = self.focus_scope.clone().unwrap();
        self.apply_focus_lock(state);
        let peers: Vec<_> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected)
            .map(|(d, _)| *d)
            .collect();
        for peer in &peers {
            self.send_focus_state(*peer);
        }
        // Focus precedes game state on each reliable stream. Preserve active
        // mode but restart cumulative raw totals at the new focus epoch.
        if self.game_mode().active {
            self.restart_game_stream();
        } else {
            for peer in peers {
                self.send_game_state(peer);
            }
        }
    }

    pub(super) fn validate_focus_lock_session(&mut self) {
        let Some(state) = self.focus_lock.as_ref().filter(|s| s.locked) else {
            return;
        };
        let session = if state.holder == self.me() {
            Some(self.session)
        } else {
            self.peers.get(&state.holder).and_then(|p| p.session)
        };
        if session != Some(state.session) {
            self.end_focus_lock();
        }
    }
    pub(super) fn validate_focus_lock_layout(&mut self) {
        let Some(device) = self.locked_device() else {
            return;
        };
        // Facts and layout travel independently. Missing geometry cannot tell
        // us that a known physical display or the owner's valid lock is gone.
        if self.layout.source_requires_target(device) {
            self.end_focus_lock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: DeviceId = DeviceId([1; 32]);
    const B: DeviceId = DeviceId([2; 32]);
    fn pair() -> (Router, Router) {
        let mut a = Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: A,
                    nonce: [A.0[0]; 16],
                },
                "source",
                Os::Other,
            ),
            super::super::connection_tests::session_layout(A, B),
        );
        let mut b = Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: B,
                    nonce: [B.0[0]; 16],
                },
                "target",
                Os::Other,
            ),
            super::super::connection_tests::session_layout(B, A),
        );
        a.attach_input(
            oink_foundation::session::Boot {
                device_id: B,
                nonce: b.session,
            },
            b.session,
            true,
            true,
            FeelProfile::default(),
            0,
        );
        b.attach_input(
            oink_foundation::session::Boot {
                device_id: A,
                nonce: a.session,
            },
            a.session,
            true,
            true,
            FeelProfile::default(),
            0,
        );
        a.peers.get_mut(&B).unwrap().focus_ready = true;
        b.peers.get_mut(&A).unwrap().focus_ready = true;
        a.reclaim_focus(None);
        a.drain_outputs();
        b.drain_outputs();
        (a, b)
    }
    fn deliver(from: &mut Router, to: &mut Router) -> Vec<Message> {
        let messages: Vec<_> = from
            .drain_outputs()
            .into_iter()
            .filter_map(|output| match output {
                Output::Send { to: target, msg, .. } if target == to.me() => Some(msg),
                _ => None,
            })
            .collect();
        for message in &messages {
            to.input_message(from.me(), message.clone(), 1);
        }
        messages
    }
    #[test]
    fn target_lock_follows_handoff_and_waits_for_target_authority() {
        let (mut a, mut b) = pair();
        a.lock_focus_to(B).unwrap();
        assert!(!a.locked());
        let messages = deliver(&mut a, &mut b);
        let enter = messages
            .iter()
            .position(|message| matches!(message, Message::Control(ControlMsg::FocusEnter { .. })))
            .unwrap();
        let lock = messages
            .iter()
            .position(|message| matches!(message, Message::Control(ControlMsg::SetFocusLock { target: None, .. })))
            .unwrap();
        assert!(enter < lock);
        assert_eq!(b.locked_device(), Some(B));
        assert_eq!(b.focus().holder, B);
        deliver(&mut b, &mut a);
        assert_eq!(a.group_focus_status().locked_device, Some(B));
    }
    #[test]
    fn screen_owner_release_gossips_a_tombstone_and_preserves_other_owners_pin() {
        let (mut a, mut b) = pair();
        a.lock_focus_to(B).unwrap();
        deliver(&mut a, &mut b);
        deliver(&mut b, &mut a);
        let stale = b.focus_lock.clone().unwrap();
        a.release_owned_focus_lock();
        assert_eq!(a.locked_device(), Some(B));
        b.release_owned_focus_lock();
        assert!(!b.locked());
        deliver(&mut b, &mut a);
        assert!(!a.locked());
        a.receive_focus_lock(stale.clone());
        b.receive_focus_lock(stale);
        assert!(!a.locked());
        assert!(!b.locked());
    }
    #[test]
    fn a_non_owner_can_request_release_from_the_current_focus_holder() {
        let (mut a, mut b) = pair();
        a.lock_focus_to(B).unwrap();
        deliver(&mut a, &mut b);
        deliver(&mut b, &mut a);
        a.set_locked(false);
        let messages = deliver(&mut a, &mut b);
        assert!(messages.iter().any(|message| matches!(
            message,
            Message::Control(ControlMsg::SetFocusLock {
                locked: Some(false),
                target: None,
                ..
            })
        )));
        deliver(&mut b, &mut a);
        assert!(!a.locked());
        assert!(!b.locked());
    }
    #[test]
    fn unavailable_or_rejected_target_never_locks_original_holder() {
        let (mut a, mut b) = pair();
        assert_eq!(a.lock_focus_to(DeviceId([3; 32])), Err(FocusLockError::Unavailable));
        assert!(!a.locked());
        b.set_fully_paused(true);
        a.lock_focus_to(B).unwrap();
        deliver(&mut a, &mut b);
        assert!(!b.locked());
        deliver(&mut b, &mut a);
        assert!(!a.locked());
    }
    #[test]
    fn invalid_scope_cannot_publish_an_active_lock() {
        let (mut a, mut b) = pair();
        b.reclaim_focus(None);
        b.set_locked(true);
        let mut lock = b.focus_lock.clone().unwrap();
        lock.scope.members.retain(|(device, _)| *device != A);
        a.receive_focus_lock(lock);
        assert!(!a.locked());
    }
}
