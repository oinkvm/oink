//! First input after joining chooses between the independent local cursor and
//! the existing group pointer. Passive gossip must never make that choice.
use super::*;

impl Router {
    /// Whether first-use arbitration is still pending for this group incarnation.
    pub fn fresh_join_pending(&self) -> bool {
        self.fresh_join
    }

    /// Explicit group membership replacement (not transport reconnection).
    pub fn begin_group_join(&mut self) {
        self.dnd_abort("joining a device group");
        self.island();
        getrandom::fill(&mut self.session).expect("routing session entropy");
        self.focus_joined = false;
        self.fresh_join = true;
        self.focus_scope = None;
        self.epoch = 0;
        self.claim = self.me();
        self.rejoin = false;
        self.focus_lock = None;
        self.peers.clear();
    }

    pub(super) fn fresh_join_local_input(&mut self, ev: &CapturedEvent) {
        if self.paused || !Self::is_join_activity(&ev.event) {
            return;
        }
        // A raw sample precedes its cooked event on some backends. It counts,
        // but never substitutes a relative delta for the actual local position.
        if self.fresh_join && self.locked() {
            let holder = self.focus_lock.as_ref().unwrap().holder;
            if self.input_ready(holder) {
                let scope = self.new_focus_scope();
                self.receive_focus_resume(self.me(), self.epoch, holder, scope.clone());
                self.broadcast(Message::Control(ControlMsg::FocusResume {
                    epoch: self.epoch,
                    holder,
                    scope,
                }));
            }
            return;
        }
        if self.fresh_join {
            if !self.peers.values().any(|p| p.connected && p.focus_ready) {
                return;
            }
            if !matches!(ev.event, InputEvent::PointerDelta { .. })
                && let Some(c) = ev.cursor
            {
                self.set_cursor(c);
            }
            self.diagnostic_log(
                line!(),
                "fresh join: local input claims current local cursor",
                self.epoch,
            );
            tracing::info!(
                epoch = self.epoch,
                "fresh join: local input claims current local cursor"
            );
            self.reclaim_focus(None); // Already Local: no warp, including a key-only claim.
            return;
        }
        let missing = self.peers.iter().any(|(id, p)| {
            p.connected
                && p.focus_ready
                && p.sharing
                && p.session.is_some_and(|s| {
                    self.focus_scope
                        .as_ref()
                        .is_none_or(|scope| !scope.members.contains(&(*id, s)))
                })
        });
        if !missing
            || self.epoch == 0
            || !self.input_ready(self.claim)
            || self.layout.source_requires_target(self.claim)
            || (self.claim != self.holder && self.hidden_device(self.claim))
        {
            return;
        }
        // A restarted holder has already caused the old source to island,
        // either on disconnect or on receiving its new session. Preserve that
        // recovery position; input must not send focus back to the old holder.
        if self.holder == self.me() && self.claim != self.me() {
            self.fresh_join = true;
            self.reclaim_focus(None);
            return;
        }
        let scope = self.new_focus_scope();
        let msg = ControlMsg::FocusResume {
            epoch: self.epoch,
            holder: self.claim,
            scope: scope.clone(),
        };
        self.diagnostic_log(
            line!(),
            "fresh join: other input resumes existing group pointer",
            self.epoch,
        );
        tracing::info!(epoch = self.epoch, holder = ?self.claim, "fresh join: other input resumes existing group pointer");
        self.broadcast(Message::Control(msg));
        self.receive_focus_resume(self.me(), self.epoch, self.claim, scope);
    }

    fn is_join_activity(event: &InputEvent) -> bool {
        match event {
            InputEvent::PointerDelta { dx, dy } | InputEvent::RawPointerDelta { dx, dy } => *dx != 0.0 || *dy != 0.0,
            InputEvent::Key { pressed, .. } | InputEvent::Button { pressed, .. } => *pressed,
            InputEvent::Scroll { dx, dy, .. } => *dx != 0.0 || *dy != 0.0,
            InputEvent::Gesture(g) => !g.ends(),
            InputEvent::TouchpadSample(_) | InputEvent::TouchpadBoundary => false,
        }
    }

    pub(super) fn receive_focus_resume(&mut self, from: DeviceId, epoch: u64, holder: DeviceId, scope: FocusScope) {
        let sender = if from == self.me() {
            Some(self.session)
        } else {
            self.peers.get(&from).and_then(|p| p.session)
        };
        if self.paused || self.layout.source_requires_target(holder)
            || (holder != self.holder && self.hidden_device(holder))
            || self.locked_device().is_some_and(|d| d != holder) || (epoch, holder) != (self.epoch, self.claim)
            || !sender.is_some_and(|s| scope.members.contains(&(from, s)))
            || !self.scope_allows(Some(&scope), holder)
            // A fresh holder cannot be resurrected by sources still naming its
            // previous incarnation. They must use normal disconnect recovery.
            || (self.fresh_join && holder == self.me())
        {
            return;
        }
        self.fresh_join = false;
        self.focus_joined = true;
        self.focus_scope = Some(scope);
        if holder != self.me() && self.input_ready(holder) {
            self.become_source(epoch, holder);
        }
        if holder == self.me() {
            self.send_game_state(from);
        }
    }
}
