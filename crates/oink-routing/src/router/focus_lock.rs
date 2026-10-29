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
    pub(super) fn set_owner_pointer_locked(&mut self, locked: bool) {
        if self.owner_pointer_locked != locked {
            self.owner_pointer_locked = locked;
            self.out.push(Output::Status(StatusEvent::LocalPointerLock(locked)));
        }
    }
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
        self.set_owner_pointer_locked(false);
        if self.locked_device() == Some(self.me()) {
            self.end_focus_lock();
        }
    }
    /// Move focus without pinning it. Uses the same landing candidates as an
    /// explicit focus lock, and the normal handoff path (held-key/contact cleanup).
    pub fn move_pointer_to(&mut self, target: DeviceId) -> Result<(), FocusLockError> {
        if self.paused || self.fully_paused {
            return Err(FocusLockError::Paused);
        }
        if !self.capture_available
            || !self.input_ready(target)
            || !self.layout.contains_device(target)
            || self.layout.source_requires_target(target)
        {
            return Err(FocusLockError::Unavailable);
        }
        if target == self.me() {
            self.return_pointer_home();
            return Ok(());
        }
        if self.holder != self.me() {
            if !self.group_focus_status().participating {
                return Err(FocusLockError::Unavailable);
            }
            self.send(
                self.holder,
                Message::Control(ControlMsg::MovePointer {
                    epoch: self.epoch,
                    target,
                }),
            );
            return Ok(());
        }
        let destination = self
            .layout
            .focus_candidates(self.me())
            .into_iter()
            .find(|p| p.screen.device == target)
            .ok_or(FocusLockError::Unavailable)?;
        self.dnd_abort("pointer moved by shortcut");
        self.hand_off(destination, None);
        Ok(())
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
        let locked = locked.unwrap_or(!self.owner_pointer_locked);
        if locked && self.layout.source_requires_target(self.holder) {
            return;
        }
        self.set_owner_pointer_locked(locked);
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
    /// Departure retires the active group pin without editing its owner's policy.
    /// An unlock tombstone travels before the focus claim, so old peers never
    /// fence an intentional departure. Returning owners publish a fresh pin.
    pub(super) fn suspend_focus_lock(&mut self) {
        self.retire_active_focus_lock();
    }
    pub(super) fn restore_owner_pointer_lock(&mut self) {
        if self.owner_pointer_locked
            && self.claim == self.me()
            && self.focus_joined
            && !self.paused
            && self.capture_available
        {
            self.change_focus_lock(Some(true));
        }
    }
    pub(super) fn observe_lock_escape_event(&mut self, event: &CapturedEvent) {
        if let InputEvent::Key { code, pressed } = event.event {
            // Backend reconciliation can synthesize a missed release. It must
            // close this barrier too, without making synthetic downs escape keys.
            if !pressed {
                self.lock_escape_keys.remove(&code);
            } else if event.physical {
                self.lock_escape_keys.insert(code);
            }
            self.publish_lock_escape();
        }
    }
    pub(super) fn local_lock_escape_held(&self) -> bool {
        self.capture_available
            && !self.paused
            && !self.keyboard_disabled()
            && self
                .hotkeys
                .bindings()
                .iter()
                .filter(|b| b.action == "lock_escape")
                .any(|b| {
                    self.lock_escape_keys.iter().any(|key| {
                        if let Some(modifier) = Mods::of(b.chord.key) {
                            Mods::of(*key) == Some(modifier)
                        } else {
                            *key == b.chord.key
                        }
                    })
                })
    }
    /// The keyboard source interprets its binding. Only its current shared focus
    /// receives the boolean; injected key events never contribute to this state.
    pub(super) fn publish_lock_escape(&mut self) {
        let held = self.local_lock_escape_held();
        let target = (self.holder != self.me()
            && !self.keyboard_local()
            && !self.keyboard_disabled()
            && !self.paused
            && self.input_ready(self.holder))
        .then_some(self.holder);
        if held != self.last_lock_escape || target != self.lock_escape_target {
            self.lock_escape_changed();
            if let Some(old) = self.lock_escape_target.filter(|old| Some(*old) != target) {
                self.send(old, Message::Control(ControlMsg::LockEscape { held: false }));
            }
            self.lock_escape_target = target;
            self.last_lock_escape = held;
            if let Some(target) = target {
                self.send(target, Message::Control(ControlMsg::LockEscape { held }));
            }
        }
    }
    pub(super) fn sync_lock_escape(&mut self) {
        self.publish_lock_escape();
        if let Some(target) = self.lock_escape_target {
            self.send(
                target,
                Message::Control(ControlMsg::LockEscape {
                    held: self.local_lock_escape_held(),
                }),
            );
        }
    }
    pub(super) fn lock_escape_changed(&mut self) {
        // Every mouse on the focus device uses the same escape gate. A keyboard
        // transition invalidates readback and damping from any motion source.
        self.game.escape_cursor = None;
        self.resistance = None;
    }
    pub(super) fn pointer_lock_escape_held(&self) -> bool {
        self.local_lock_escape_held()
            || self
                .lock_escape_sources
                .iter()
                .any(|source| self.source_lock_escape_held(*source))
    }
    pub(super) fn source_lock_escape_held(&self, source: DeviceId) -> bool {
        if source == self.me() {
            self.local_lock_escape_held()
        } else {
            self.lock_escape_sources.contains(&source) && self.input_ready(source)
        }
    }
    pub(super) fn end_focus_lock(&mut self) {
        if self.locked_device() == Some(self.me()) {
            self.set_owner_pointer_locked(false);
        }
        self.retire_active_focus_lock();
    }
    fn retire_active_focus_lock(&mut self) {
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
        self.resistance = None;
        if locked {
            self.gap_cancel();
            // A pin closes departures, not an admitted incoming drag. In
            // particular, restoring an owner's retained pin during FocusEnter
            // must preserve both its pending host and the original held source
            // on other members. Retire edge pressure only; an in-flight native
            // hold rechecks the departure gate before handing off.
            self.dnd_off_edge();
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
        if self.layout.source_requires_target(self.me()) {
            self.set_owner_pointer_locked(false);
        }
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
    fn lock_survives_shortcut_departure_and_home_return() {
        for home in [false, true] {
            let (mut a, mut b) = pair();
            a.set_locked(true);
            deliver(&mut a, &mut b);
            a.move_pointer_to(B).unwrap();
            deliver(&mut a, &mut b);
            deliver(&mut b, &mut a);
            assert_eq!(b.focus().holder, B);
            assert_eq!(b.group_focus_status().locked_device, None);
            if home {
                a.return_pointer_home();
            } else {
                b.move_pointer_to(A).unwrap();
            }
            for _ in 0..4 {
                deliver(&mut a, &mut b);
                deliver(&mut b, &mut a);
            }
            assert_eq!(a.focus().holder, A);
            assert_eq!(a.group_focus_status().locked_device, Some(A));
            assert_eq!(b.group_focus_status().locked_device, Some(A));
            assert!(a.leave_blocked());
        }
    }
    fn owner_events(router: &mut Router) -> Vec<bool> {
        let events = router
            .out
            .iter()
            .filter_map(|output| match output {
                Output::Status(StatusEvent::LocalPointerLock(locked)) => Some(*locked),
                _ => None,
            })
            .collect();
        router
            .out
            .retain(|output| !matches!(output, Output::Status(StatusEvent::LocalPointerLock(_))));
        events
    }
    #[test]
    fn owner_lock_events_ignore_visits_and_report_dormant_release() {
        let (mut a, mut b) = pair();
        a.set_locked(true);
        assert_eq!(owner_events(&mut a), vec![true]);
        a.move_pointer_to(B).unwrap();
        assert!(owner_events(&mut a).is_empty());
        a.return_pointer_home();
        assert!(owner_events(&mut a).is_empty());
        a.move_pointer_to(B).unwrap();
        a.release_owned_focus_lock();
        assert_eq!(owner_events(&mut a), vec![false]);
        a.return_pointer_home();
        assert!(owner_events(&mut a).is_empty());
        a.set_locked(true);
        a.set_locked(false);
        a.set_locked(true);
        assert_eq!(owner_events(&mut a), vec![true, false, true]);
        deliver(&mut a, &mut b);
        assert!(
            owner_events(&mut b).is_empty(),
            "remote lock observations are not owner transitions"
        );
    }

    #[test]
    fn escape_gate_uses_held_key_and_preserves_lock() {
        let (mut a, _) = pair();
        a.set_hotkeys(vec![HotkeyBinding {
            action: "lock_escape".into(),
            chord: crate::hotkey::Chord {
                mods: Mods::NONE,
                key: KeyCode::LEFT_GUI,
            },
        }]);
        a.set_locked(true);
        assert!(a.leave_blocked());
        a.lock_escape_keys.insert(KeyCode::RIGHT_GUI);
        assert!(!a.leave_blocked());
        assert!(a.locked());
        a.lock_escape_keys.clear();
        assert!(a.leave_blocked());
    }
    fn physical(r: &mut Router, event: InputEvent) {
        let source = if matches!(event, InputEvent::Key { .. }) {
            SourceKind::Keyboard
        } else {
            SourceKind::Mouse
        };
        r.on_local_event(
            &CapturedEvent {
                cursor: r.pointer.as_ref().map(|p| p.local),
                event,
                physical: true,
                source,
                time_us: 0,
            },
            10,
        );
    }
    fn escape(key: KeyCode) -> Vec<HotkeyBinding> {
        vec![HotkeyBinding {
            action: "lock_escape".into(),
            chord: crate::hotkey::Chord { mods: Mods::NONE, key },
        }]
    }
    #[test]
    fn default_alt_escape_crosses_from_a_remote_locked_owner() {
        for source_os in [Os::MacOS, Os::Windows] {
            let (mut source, mut owner) = pair();
            source.cfg.os = source_os;
            let mut layout = crate::SimpleLayout::new();
            for (index, id) in [A, B].into_iter().enumerate() {
                layout.add_screen(
                    ScreenId::new(id, "screen"),
                    Rect::new(0., 0., 1000., 600.),
                    Rect::new(index as f64, 0., 1., 0.6),
                );
            }
            source.set_layout(Arc::new(layout));
            owner.set_layout(source.layout.clone());
            for router in [&mut source, &mut owner] {
                router.set_local_displays(vec![DisplayInfo {
                    key: "screen".into(),
                    name: String::new(),
                    bounds: Rect::new(0., 0., 1000., 600.),
                    pixel_width: 1000,
                    pixel_height: 600,
                    reference_extent: None,
                    rotation_deg: 0,
                    is_primary: true,
                    is_builtin: false,
                    is_virtual: false,
                }]);
            }
            source.set_hotkeys(escape(crate::hotkey::single_key(&["Alt"]).unwrap()));
            source.lock_focus_to(B).unwrap();
            for _ in 0..3 {
                deliver(&mut source, &mut owner);
                deliver(&mut owner, &mut source);
            }
            owner.set_cursor(Point::new(0., 300.));
            physical(&mut source, InputEvent::PointerDelta { dx: -5000., dy: 0. });
            deliver(&mut source, &mut owner);
            assert_eq!(owner.focus().holder, B);
            physical(
                &mut source,
                InputEvent::Key {
                    code: KeyCode::RIGHT_ALT,
                    pressed: true,
                },
            );
            physical(&mut source, InputEvent::PointerDelta { dx: -5000., dy: 0. });
            for _ in 0..3 {
                deliver(&mut source, &mut owner);
                deliver(&mut owner, &mut source);
            }
            assert_eq!(source.focus().holder, A);
            assert_eq!(owner.focus().holder, A);
            assert!(owner.owner_pointer_locked, "escaping retains the owner's policy");
            physical(
                &mut source,
                InputEvent::Key {
                    code: KeyCode::RIGHT_ALT,
                    pressed: false,
                },
            );
            deliver(&mut source, &mut owner);
            assert!(!owner.source_lock_escape_held(A));
            assert!(owner.key_refs.is_empty());
        }
    }

    #[test]
    fn remote_option_allows_windows_local_mouse_to_leave_downward() {
        for key in [KeyCode::LEFT_ALT, KeyCode::RIGHT_ALT] {
            let (mut source, mut owner) = pair();
            source.cfg.os = Os::MacOS;
            owner.cfg.os = Os::Windows;
            let mut layout = crate::SimpleLayout::new();
            for (index, id) in [B, A].into_iter().enumerate() {
                layout.add_screen(
                    ScreenId::new(id, "screen"),
                    Rect::new(0., 0., 1000., 600.),
                    Rect::new(0., index as f64 * 0.6, 1., 0.6),
                );
            }
            source.set_layout(Arc::new(layout));
            owner.set_layout(source.layout.clone());
            for router in [&mut source, &mut owner] {
                router.set_local_displays(vec![DisplayInfo {
                    key: "screen".into(),
                    name: String::new(),
                    bounds: Rect::new(0., 0., 1000., 600.),
                    pixel_width: 1000,
                    pixel_height: 600,
                    reference_extent: None,
                    rotation_deg: 0,
                    is_primary: true,
                    is_builtin: false,
                    is_virtual: false,
                }]);
            }
            source.set_hotkeys(escape(crate::hotkey::single_key(&["Alt"]).unwrap()));
            source.lock_focus_to(B).unwrap();
            for _ in 0..3 {
                deliver(&mut source, &mut owner);
                deliver(&mut owner, &mut source);
            }
            owner.set_cursor(Point::new(500., 599.));
            physical(&mut owner, InputEvent::PointerDelta { dx: 0., dy: 5000. });
            deliver(&mut source, &mut owner);
            assert_eq!(owner.focus().holder, B);
            physical(
                &mut source,
                InputEvent::Key {
                    code: key,
                    pressed: true,
                },
            );
            deliver(&mut source, &mut owner);
            assert!(owner.lock_escape_sources.contains(&A), "remote held state arrived");
            assert!(
                owner.held.get(&A).is_some_and(|held| held.keys.contains_key(&key)),
                "Option is forwarded into Windows"
            );
            physical(&mut owner, InputEvent::PointerDelta { dx: 0., dy: 5000. });
            for _ in 0..3 {
                deliver(&mut source, &mut owner);
                deliver(&mut owner, &mut source);
            }
            assert_eq!(source.focus().holder, A);
            assert_eq!(owner.focus().holder, A);
            assert!(owner.owner_pointer_locked, "escaping retains the owner's policy");
            physical(
                &mut source,
                InputEvent::Key {
                    code: key,
                    pressed: false,
                },
            );
            deliver(&mut source, &mut owner);
            assert!(!owner.source_lock_escape_held(A));
            assert!(owner.key_refs.is_empty());
        }
    }

    fn escape_messages(router: &mut Router) -> Vec<(DeviceId, bool)> {
        router
            .drain_outputs()
            .into_iter()
            .filter_map(|output| match output {
                Output::Send {
                    to,
                    msg: Message::Control(ControlMsg::LockEscape { held }),
                    ..
                } => Some((to, held)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn escape_state_is_retargeted_only_to_the_current_keyboard_focus() {
        let (mut source, mut owner) = pair();
        source.set_hotkeys(escape(KeyCode::LEFT_ALT));
        source.lock_focus_to(B).unwrap();
        for _ in 0..3 {
            deliver(&mut source, &mut owner);
            deliver(&mut owner, &mut source);
        }
        let third = DeviceId([3; 32]);
        source.attach_input(
            oink_foundation::session::Boot {
                device_id: third,
                nonce: [3; 16],
            },
            [3; 16],
            true,
            true,
            FeelProfile::default(),
            0,
        );
        source.peers.get_mut(&third).unwrap().focus_ready = true;
        source.drain_outputs();
        physical(
            &mut source,
            InputEvent::Key {
                code: KeyCode::LEFT_ALT,
                pressed: true,
            },
        );
        assert_eq!(escape_messages(&mut source), vec![(B, true)]);
        source.become_source(source.epoch + 1, third);
        assert_eq!(escape_messages(&mut source), vec![(B, false), (third, true)]);
        source.send_state_sync();
        assert_eq!(escape_messages(&mut source), vec![(third, true)]);
        source.become_holder(source.epoch + 1, None);
        assert_eq!(escape_messages(&mut source), vec![(third, false)]);
        assert!(source.local_lock_escape_held());
        source.become_source(source.epoch + 1, B);
        assert_eq!(escape_messages(&mut source), vec![(B, true)]);
        physical(
            &mut source,
            InputEvent::Key {
                code: KeyCode::LEFT_ALT,
                pressed: false,
            },
        );
        assert_eq!(escape_messages(&mut source), vec![(B, false)]);
    }

    #[test]
    fn remote_escape_is_cleared_on_focus_departure_and_link_loss() {
        let (mut source, mut owner) = pair();
        source.set_hotkeys(escape(KeyCode::LEFT_ALT));
        source.lock_focus_to(B).unwrap();
        for _ in 0..3 {
            deliver(&mut source, &mut owner);
            deliver(&mut owner, &mut source);
        }
        physical(
            &mut source,
            InputEvent::Key {
                code: KeyCode::LEFT_ALT,
                pressed: true,
            },
        );
        deliver(&mut source, &mut owner);
        assert!(!owner.leave_blocked());
        source.return_pointer_home();
        for _ in 0..3 {
            deliver(&mut source, &mut owner);
            deliver(&mut owner, &mut source);
        }
        assert!(owner.lock_escape_sources.is_empty());
        source.move_pointer_to(B).unwrap();
        for _ in 0..3 {
            deliver(&mut source, &mut owner);
            deliver(&mut owner, &mut source);
        }
        assert!(owner.locked());
        assert!(!owner.leave_blocked(), "still-held key is resent without a new press");
        owner.detach_input(A);
        assert!(owner.lock_escape_sources.is_empty());
        assert!(owner.leave_blocked(), "link loss cannot leave an escape latch");
    }

    #[test]
    fn local_only_keyboard_never_opens_a_remote_pointer_lock() {
        let (mut source, mut owner) = pair();
        source.set_hotkeys(escape(KeyCode::LEFT_ALT));
        source.lock_focus_to(B).unwrap();
        for _ in 0..3 {
            deliver(&mut source, &mut owner);
            deliver(&mut owner, &mut source);
        }
        physical(
            &mut source,
            InputEvent::Key {
                code: KeyCode::LEFT_ALT,
                pressed: true,
            },
        );
        deliver(&mut source, &mut owner);
        assert!(!owner.leave_blocked());
        let mut settings = source.config().settings.clone();
        settings.keyboard_mode = oink_foundation::input::InputMode::Local;
        source.set_settings(settings);
        deliver(&mut source, &mut owner);
        assert!(owner.leave_blocked());
        source.send_state_sync();
        assert!(escape_messages(&mut source).is_empty());
    }

    #[test]
    fn local_and_remote_keyboards_both_open_the_gate_using_their_own_bindings() {
        let (mut a, mut b) = pair();
        a.set_hotkeys(escape(KeyCode::keyboard(0x1d))); // Z on the physical source.
        b.set_hotkeys(escape(KeyCode::LEFT_GUI));
        a.lock_focus_to(B).unwrap();
        for _ in 0..3 {
            deliver(&mut a, &mut b);
            deliver(&mut b, &mut a);
        }
        b.last_pointer_source = Some(A);
        assert!(b.leave_blocked());
        // The holder binds Meta, but the source binds Z. Forwarded Meta is just
        // application input and must not be interpreted again by the holder.
        physical(
            &mut a,
            InputEvent::Key {
                code: KeyCode::LEFT_GUI,
                pressed: true,
            },
        );
        deliver(&mut a, &mut b);
        assert!(b.lock_escape_keys.is_empty());
        assert!(b.leave_blocked());
        physical(
            &mut a,
            InputEvent::Key {
                code: KeyCode::LEFT_GUI,
                pressed: false,
            },
        );
        deliver(&mut a, &mut b);
        physical(
            &mut b,
            InputEvent::Key {
                code: KeyCode::LEFT_GUI,
                pressed: true,
            },
        );
        b.last_pointer_source = Some(A);
        assert!(
            !b.leave_blocked(),
            "the local keyboard also opens a remote mouse's gate"
        );
        physical(
            &mut b,
            InputEvent::Key {
                code: KeyCode::LEFT_GUI,
                pressed: false,
            },
        );
        assert!(b.leave_blocked());
        physical(
            &mut a,
            InputEvent::Key {
                code: KeyCode::keyboard(0x1d),
                pressed: true,
            },
        );
        deliver(&mut a, &mut b);
        assert!(!b.leave_blocked());
        assert!(b.locked());
        physical(
            &mut a,
            InputEvent::Key {
                code: KeyCode::keyboard(0x1d),
                pressed: false,
            },
        );
        deliver(&mut a, &mut b);
        assert!(b.leave_blocked());
        physical(
            &mut a,
            InputEvent::Key {
                code: KeyCode::keyboard(0x1d),
                pressed: true,
            },
        );
        deliver(&mut a, &mut b);
        b.detach_input(A);
        assert!(b.lock_escape_sources.is_empty());
    }
    #[test]
    fn escape_crossing_preserves_owner_lock_and_balances_target_modifiers() {
        let (mut a, mut b) = pair();
        a.cfg.os = Os::Windows;
        b.cfg.os = Os::Windows;
        // Adjacent screens on both routers. The generic shortcut fixture has a
        // large wall gap, which deliberately has no ordinary edge portal.
        let mut layout = crate::SimpleLayout::new();
        for (index, id) in [A, B].into_iter().enumerate() {
            layout.add_screen(
                ScreenId::new(id, "screen"),
                Rect::new(0., 0., 1000., 600.),
                Rect::new(index as f64, 0., 1., 0.6),
            );
        }
        a.set_layout(Arc::new(layout));
        b.set_layout(a.layout.clone());
        a.set_hotkeys(escape(KeyCode::LEFT_GUI));
        a.set_local_displays(vec![DisplayInfo {
            key: "screen".into(),
            name: String::new(),
            bounds: Rect::new(0., 0., 1000., 600.),
            pixel_width: 1000,
            pixel_height: 600,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        }]);
        a.set_cursor(Point::new(999., 300.));
        a.set_locked(true);
        deliver(&mut a, &mut b);
        physical(&mut a, InputEvent::PointerDelta { dx: 500., dy: 0. });
        assert_eq!(a.focus().holder, A);
        physical(
            &mut a,
            InputEvent::Key {
                code: KeyCode::RIGHT_GUI,
                pressed: true,
            },
        );
        physical(&mut a, InputEvent::PointerDelta { dx: 500., dy: 0. });
        assert!(
            a.out.iter().any(|out| matches!(
                out,
                Output::Inject(InjectEvent::Key {
                    code: KeyCode::MENU_MASK,
                    pressed: true
                })
            )),
            "escape crossing must mask the old Windows source as well as the destination"
        );
        for _ in 0..3 {
            deliver(&mut a, &mut b);
            deliver(&mut b, &mut a);
        }
        assert_eq!(b.focus().holder, B);
        assert!(a.owner_pointer_locked);
        assert_eq!(b.group_focus_status().locked_device, None);
        assert!(b.held.get(&A).is_some_and(|h| h.keys.contains_key(&KeyCode::RIGHT_GUI)));
        physical(
            &mut a,
            InputEvent::Key {
                code: KeyCode::RIGHT_GUI,
                pressed: false,
            },
        );
        deliver(&mut a, &mut b);
        assert!(
            b.key_refs.is_empty(),
            "physical release must clear the synced target modifier"
        );
        a.return_pointer_home();
        for _ in 0..3 {
            deliver(&mut a, &mut b);
            deliver(&mut b, &mut a);
        }
        assert!(a.locked());
        assert!(a.leave_blocked());
    }
    #[test]
    fn reconciled_escape_release_closes_barrier_without_rearming_on_synthetic_down() {
        let (mut a, _) = pair();
        a.set_hotkeys(escape(KeyCode::LEFT_GUI));
        a.set_locked(true);
        physical(
            &mut a,
            InputEvent::Key {
                code: KeyCode::LEFT_GUI,
                pressed: true,
            },
        );
        assert!(!a.leave_blocked());
        for pressed in [false, true] {
            a.on_local_event(
                &CapturedEvent {
                    cursor: None,
                    event: InputEvent::Key {
                        code: KeyCode::LEFT_GUI,
                        pressed,
                    },
                    source: SourceKind::Keyboard,
                    physical: false,
                    time_us: 20,
                },
                20,
            );
            assert!(a.leave_blocked());
        }
    }
    #[test]
    fn two_owners_retain_independent_locks_across_visits_and_explicit_unlock() {
        let (mut a, mut b) = pair();
        a.set_locked(true);
        deliver(&mut a, &mut b);
        a.lock_focus_to(B).unwrap();
        for _ in 0..3 {
            deliver(&mut a, &mut b);
            deliver(&mut b, &mut a);
        }
        assert!(a.owner_pointer_locked && b.owner_pointer_locked);
        a.return_pointer_home();
        for _ in 0..3 {
            deliver(&mut a, &mut b);
            deliver(&mut b, &mut a);
        }
        assert_eq!(a.locked_device(), Some(A));
        a.set_locked(false);
        a.move_pointer_to(B).unwrap();
        for _ in 0..3 {
            deliver(&mut a, &mut b);
            deliver(&mut b, &mut a);
        }
        assert_eq!(a.locked_device(), Some(B));
        b.release_owned_focus_lock();
        deliver(&mut b, &mut a);
        assert!(!a.locked() && !b.owner_pointer_locked);
        a.return_pointer_home();
        for _ in 0..3 {
            deliver(&mut a, &mut b);
            deliver(&mut b, &mut a);
        }
        assert!(!a.locked() && !a.owner_pointer_locked);
    }
    #[test]
    fn pointer_shortcut_hands_off_at_existing_landing_without_locking() {
        let (mut a, mut b) = pair();
        let landing = a
            .layout
            .focus_candidates(A)
            .into_iter()
            .find(|p| p.screen.device == B)
            .unwrap();
        a.move_pointer_to(B).unwrap();
        let messages = deliver(&mut a, &mut b);
        assert!(messages.iter().any(|m|matches!(m,Message::Control(ControlMsg::FocusEnter { display,pos,.. }) if *display == landing.screen.display && *pos == landing.local)));
        assert_eq!(b.focus().holder, B);
        assert!(!a.locked());
        assert!(!b.locked());
        // The separate digit for this device recalls using the same rule as H.
        a.move_pointer_to(A).unwrap();
        deliver(&mut a, &mut b);
        assert_eq!(a.focus().holder, A);
    }
    #[test]
    fn pointer_shortcut_remote_holder_request_is_epoch_fenced() {
        let (mut a, mut b) = pair();
        a.move_pointer_to(B).unwrap();
        deliver(&mut a, &mut b);
        deliver(&mut b, &mut a);
        a.move_pointer_to(B).unwrap();
        assert!(
            deliver(&mut a, &mut b)
                .iter()
                .any(|m| matches!(m,Message::Control(ControlMsg::MovePointer{target,..}) if *target == B))
        );
        let epoch = b.epoch;
        b.input_message(
            A,
            Message::Control(ControlMsg::MovePointer {
                epoch: epoch.saturating_sub(1),
                target: A,
            }),
            2,
        );
        assert_eq!(b.focus().holder, B);
        assert_eq!(b.epoch, epoch);
    }
    #[test]
    fn pointer_shortcut_can_move_from_a_remote_holder_to_a_third_device() {
        let ids = [A, B, DeviceId([3; 32])];
        let mut layout = crate::SimpleLayout::new();
        for (index, id) in ids.iter().enumerate() {
            layout.add_screen(
                ScreenId::new(*id, "screen"),
                Rect::new(0., 0., 1000., 600.),
                Rect::new(index as f64 * 300., 0., 250., 150.),
            );
        }
        let layout: Arc<dyn LayoutQuery> = Arc::new(layout);
        let mut routers: Vec<_> = ids
            .iter()
            .map(|id| {
                Router::new(
                    RouterConfig::new(
                        oink_foundation::session::Boot {
                            device_id: *id,
                            nonce: [id.0[0]; 16],
                        },
                        "test",
                        Os::Other,
                    ),
                    layout.clone(),
                )
            })
            .collect();
        let sessions: Vec<_> = routers.iter().map(|r| r.session).collect();
        for (i, router) in routers.iter_mut().enumerate() {
            for j in 0..3 {
                if i != j {
                    router.attach_input(
                        oink_foundation::session::Boot {
                            device_id: ids[j],
                            nonce: sessions[j],
                        },
                        sessions[j],
                        true,
                        true,
                        FeelProfile::default(),
                        0,
                    );
                    router.peers.get_mut(&ids[j]).unwrap().focus_ready = true;
                }
            }
        }
        let pump = |routers: &mut Vec<Router>| {
            for _ in 0..8 {
                let mut messages = Vec::new();
                for router in routers.iter_mut() {
                    let from = router.me();
                    for output in router.drain_outputs() {
                        if let Output::Send { to, msg, .. } = output {
                            messages.push((from, to, msg));
                        }
                    }
                }
                if messages.is_empty() {
                    break;
                }
                for (from, to, msg) in messages {
                    routers[ids.iter().position(|id| *id == to).unwrap()].input_message(from, msg, 1);
                }
            }
        };
        routers[0].reclaim_focus(None);
        pump(&mut routers);
        routers[0].move_pointer_to(B).unwrap();
        pump(&mut routers);
        assert!(routers.iter().all(|r| r.focus().holder == B));
        routers[0].move_pointer_to(ids[2]).unwrap();
        pump(&mut routers);
        assert!(routers.iter().all(|r| r.focus().holder == ids[2]));
        assert!(routers.iter().all(|r| !r.locked()));
        routers[0].move_pointer_to(A).unwrap();
        pump(&mut routers);
        assert!(routers.iter().all(|r| r.focus().holder == A));
    }
    #[test]
    fn pointer_shortcut_refuses_paused_or_unavailable_targets() {
        let (mut a, _) = pair();
        let before = a.focus();
        assert_eq!(a.move_pointer_to(DeviceId([99; 32])), Err(FocusLockError::Unavailable));
        a.set_fully_paused(true);
        assert_eq!(a.move_pointer_to(B), Err(FocusLockError::Paused));
        assert_eq!(a.focus().holder, before.holder);
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
