//! Input pair attachment. Exact stream identities stay in the input owner.
use super::*;
impl Router {
    /// Called only with current C0-admitted identities, including indirect sources.
    pub fn admit_boot(&mut self, boot: oink_foundation::session::Boot) {
        if boot.device_id == self.me() {
            return;
        }
        if self.admitted_boots.get(&boot.device_id).is_some_and(|old| *old != boot) {
            let old = self.admitted_boots[&boot.device_id];
            self.retire_boot(old);
        }
        self.admitted_boots.insert(boot.device_id, boot);
    }
    pub fn retire_boot(&mut self, boot: oink_foundation::session::Boot) {
        if boot.device_id == self.me() || self.admitted_boots.get(&boot.device_id) != Some(&boot) {
            return;
        }
        if self.peers.get(&boot.device_id).is_some_and(|peer| peer.connected) {
            self.detach_input(boot.device_id);
        } else {
            self.peer_lost(boot.device_id);
        }
        self.input_transforms.remove(&boot.device_id);
        // Release held input and drag ownership while the old peer and Boot are
        // still available above. Then discard its sessions and cached facts so
        // future focus scopes contain no retired members. A replacement Boot
        // starts with a fresh peer record; stale retirement is fenced above.
        self.peers.remove(&boot.device_id);
        self.admitted_boots.remove(&boot.device_id);
    }

    pub fn attach_input(
        &mut self,
        boot: oink_foundation::session::Boot,
        instance: [u8; 16],
        controllable: bool,
        dnd: bool,
        feel: FeelProfile,
        now: Micros,
    ) -> bool {
        let peer = boot.device_id;
        if peer == self.me() {
            return false;
        }
        self.admit_boot(boot);
        let state = self.peers.entry(peer).or_insert_with(|| Peer::new(now));
        if state.connected || state.retired_sessions.contains(&instance) {
            return false;
        }
        if let Some(old) = state.session.filter(|old| *old != instance) {
            state.retired_sessions.insert(old);
        }
        state.session = Some(instance);
        state.sharing = controllable;
        state.feel = feel;
        state.connected = true;
        state.focus_ready = false;
        state.held_sync_nonempty = false;
        self.validate_focus_lock_session();
        self.dnd_hello(peer);
        self.on_dnd_msg(peer, crate::dnd::DndMsg::Caps { enabled: dnd });
        self.dnd_member_attached(peer);
        self.send_focus_state(peer);
        self.send_game_state(peer);
        let interval = self.interval_us();
        self.next_sync = Some(self.next_sync.map_or(now + interval, |t| t.min(now + interval)));
        true
    }
    pub fn detach_input(&mut self, peer: DeviceId) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        if !state.connected {
            return;
        }
        state.connected = false;
        state.touchpad_contacts = None;
        state.focus_ready = false;
        state.held_sync_nonempty = false;
        if self.contacts_target == Some(peer) {
            self.cancel_source_contacts();
        }
        self.peer_lost(peer);
        if !self.peers.values().any(|p| p.connected) {
            self.next_sync = None;
        }
        self.dnd_hello(peer);
        self.restart_game_stream();
    }
    pub fn input_message(&mut self, peer: DeviceId, message: Message, now: Micros) -> bool {
        if self.peers.get(&peer).is_none_or(|p| !p.connected) {
            return false;
        }
        self.receive_message(peer, message, now);
        true
    }
}
impl Router {
    /// The five records every newly opened input stream starts with. This only reads
    /// state, so it is also valid while neither direction has been attached yet.
    pub fn input_opening(&self, controllable: bool) -> [Message; 5] {
        [
            Message::Control(ControlMsg::InputCapability {
                controllable,
                touchpad_contacts: self.touchpad_contacts,
            }),
            Message::Control(ControlMsg::Feel {
                profile: self.feel.clone(),
            }),
            Message::Control(ControlMsg::FocusState {
                lock_state: self.focus_lock.clone(),
                epoch: self.epoch,
                holder: self.claim,
                scope: self.focus_scope.clone(),
                session: Some(self.session),
            }),
            self.input_game_state(),
            Message::Dnd(crate::dnd::DndMsg::Caps {
                enabled: self.dnd_enabled(),
            }),
        ]
    }
    /// Handshake OS precedes device facts; later facts may overwrite it without
    /// replacing either input stream or changing the router's held-key ledger.
    pub fn set_peer_os(&mut self, peer: DeviceId, os: Os, now: Micros) {
        self.peers.entry(peer).or_insert_with(|| Peer::new(now)).os = os;
    }
    /// Display facts are metadata only; topology arrives already compiled by config.
    pub fn set_peer_displays(&mut self, peer: DeviceId, displays: Vec<DisplayInfo>) {
        let state = self.peers.entry(peer).or_insert_with(|| Peer::new(0));
        if state.displays != displays {
            state.displays = displays;
            self.out.push(Output::Status(StatusEvent::PeerDisplaysChanged(peer)));
        }
    }
    pub fn resend_input_preferences(&mut self, peer: DeviceId) {
        self.send(
            peer,
            Message::Control(ControlMsg::Feel {
                profile: self.feel.clone(),
            }),
        );
        self.send(
            peer,
            Message::Dnd(crate::dnd::DndMsg::Caps {
                enabled: self.dnd_enabled(),
            }),
        );
    }
}
