//! The input owner's view of each peer's opening records and the attach
//! of a completed I0 pair, shared by the desktop input owner and the phone's
//! embedded router. Stream identities stay with the owner.
use crate::{Packet as Message, control::ControlMsg, dnd::DndMsg};
use oink_foundation::{DeviceId, input::FeelProfile, session::Boot};

/// The records every newly opened input stream starts with: 1
/// `ControlMsg::InputCapability`, 2 `ControlMsg::Feel`, 3 `ControlMsg::FocusState`,
/// 4 `ControlMsg::GameMode`, 5 `DndMsg::Caps`.
pub const OPENING_MESSAGES: usize = 5;

/// What the negotiated capabilities of a peer's link allow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerCaps {
    pub relative: bool,
    pub drag: bool,
    /// `oink.input/2`.
    pub input: bool,
    pub windows_lock: bool,
}

/// What the inbound I0 has told about its writer. Lives and dies with that stream: cleared
/// when it closes or is replaced, kept while only the outbound stream is reopened. Between
/// attaches (and while this device is paused) state messages update it without reaching
/// the router; at every attach the router gets the newest values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReceiveState {
    /// From `InputCapability`. The router's `sharing` flag for the peer.
    pub controllable: Option<bool>,
    pub touchpad_contacts: Option<u8>,
    pub feel: Option<FeelProfile>,
    /// Newest `ControlMsg::FocusState` (carries the peer's router instance).
    pub focus: Option<ControlMsg>,
    /// Newest `ControlMsg::GameMode`.
    pub game_mode: Option<ControlMsg>,
    /// Newest `DndMsg::Caps`.
    pub dnd_caps: Option<DndMsg>,
}

impl ReceiveState {
    /// All five opening messages have been read.
    pub fn opening_read(&self, caps: PeerCaps) -> bool {
        self.controllable.is_some()
            && self.feel.is_some()
            && self.focus.is_some()
            && (!caps.relative || self.game_mode.is_some())
            && (!caps.drag || self.dnd_caps.is_some())
    }
}

impl ReceiveState {
    /// State messages keep their newest value even while the pair is detached.
    pub fn record(&mut self, message: &Message) {
        match message {
            Message::Control(ControlMsg::InputCapability {
                controllable,
                touchpad_contacts,
            }) => {
                self.controllable = Some(*controllable);
                self.touchpad_contacts = *touchpad_contacts;
            }
            Message::Control(ControlMsg::Feel { profile }) => self.feel = Some(profile.clone()),
            Message::Control(m @ ControlMsg::FocusState { .. }) => self.focus = Some(m.clone()),
            Message::Control(m @ ControlMsg::GameMode { .. }) => self.game_mode = Some(m.clone()),
            Message::Dnd(m @ DndMsg::Caps { .. }) => self.dnd_caps = Some(m.clone()),
            _ => {}
        }
    }
}

/// Concrete adapter owned by the native input loop. It has no system backend and can
/// also drive the real router in the simulator. Input stream identities stay outside it.
#[derive(Debug)]
pub struct Adapter {
    router: crate::Router,
    received: std::collections::HashMap<DeviceId, ReceiveState>,
    active: std::collections::HashSet<DeviceId>,
    now: crate::Micros,
}
impl Adapter {
    pub fn new(router: crate::Router) -> Self {
        Self {
            router,
            received: Default::default(),
            active: Default::default(),
            now: 0,
        }
    }
    pub fn router(&self) -> &crate::Router {
        &self.router
    }
    pub fn router_mut(&mut self) -> &mut crate::Router {
        &mut self.router
    }
    pub fn at(&mut self, now: crate::Micros) {
        self.now = now;
    }
    pub fn receive_state(&self, peer: DeviceId) -> Option<&ReceiveState> {
        self.received.get(&peer)
    }
    pub fn clear_received(&mut self, peer: DeviceId) {
        self.received.remove(&peer);
    }
}
impl std::ops::Deref for Adapter {
    type Target = crate::Router;
    fn deref(&self) -> &Self::Target {
        &self.router
    }
}
impl std::ops::DerefMut for Adapter {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.router
    }
}
impl Adapter {
    pub fn attach_with(&mut self, boot: Boot, caps: PeerCaps, state: &ReceiveState, peer_os: crate::Os) {
        let peer = boot.device_id;
        if self.attached(peer) || !state.opening_read(caps) {
            return;
        }
        let Some(ControlMsg::FocusState {
            session: Some(instance),
            ..
        }) = state.focus.as_ref()
        else {
            return;
        };
        let enabled = caps.drag && matches!(state.dnd_caps, Some(DndMsg::Caps { enabled: true }));
        self.router.set_peer_os(peer, peer_os, self.now);
        if !self.router.attach_input(
            boot,
            *instance,
            state.controllable == Some(true),
            enabled,
            state.feel.clone().expect("complete opening"),
            self.now,
        ) {
            return;
        }
        self.router.set_peer_windows_lock(peer, caps.windows_lock);
        self.router.set_peer_touchpad_contacts(peer, state.touchpad_contacts);
        self.router.set_peer_scroll_pixels(peer, caps.input);
        self.router.set_peer_touchpad_options(peer, caps.input);
        self.router.set_peer_pointer_move_hidden(peer, caps.input);
        self.router.set_peer_local_click(peer, caps.input);
        self.active.insert(peer);
        self.received.insert(peer, state.clone());
        for message in [
            state.focus.clone().map(Message::Control),
            state.game_mode.clone().map(Message::Control),
        ]
        .into_iter()
        .flatten()
        {
            self.router.input_message(peer, message, self.now);
        }
        self.router.resend_input_preferences(peer);
    }
    pub fn detach(&mut self, peer: DeviceId) {
        if self.active.remove(&peer) {
            self.router.detach_input(peer);
        }
    }
    pub fn attached(&self, peer: DeviceId) -> bool {
        self.active.contains(&peer)
    }
    pub fn inbound(&mut self, peer: DeviceId, message: Message) {
        self.received.entry(peer).or_default().record(&message);
        if self.active.contains(&peer) {
            self.router.input_message(peer, message, self.now);
        }
    }
    pub fn opening_state(&self, controllable: bool) -> [Message; OPENING_MESSAGES] {
        self.router.input_opening(controllable)
    }
}
