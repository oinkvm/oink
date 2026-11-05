//! Local physical clicks use a distributed drag barrier before publishing a
//! focus claim. Every online participant can propose; no holder arbitrates.
use super::*;

const PREPARE: u8 = 0;
const ACCEPT: u8 = 1;
const REJECT: u8 = 2;
const COMMIT: u8 = 3;
const ABORT: u8 = 4;
const SNAPSHOT: u8 = 5;

#[derive(Debug, Default)]
pub(super) struct State {
    clock: u64,
    committed: Option<(u64, DeviceId, FocusScope)>,
    pending: BTreeMap<(u64, DeviceId), Proposal>,
    retired: BTreeMap<DeviceId, u64>,
    cancelled: BTreeMap<DeviceId, (u64, FocusScope, u8)>,
}
#[derive(Debug)]
struct Proposal {
    scope: FocusScope,
    accepted: HashSet<DeviceId>,
}
impl Router {
    pub(super) fn local_click_reset_peer(&mut self, peer: DeviceId) {
        self.local_click.retired.remove(&peer);
        self.local_click.cancelled.remove(&peer);
    }
    pub fn set_peer_local_click(&mut self, peer: DeviceId, supported: bool) {
        if !supported && self.peers.get(&peer).is_some_and(|p| p.connected && p.sharing) {
            self.local_click_cancel();
        }
        if let Some(p) = self.peers.get_mut(&peer) {
            p.local_click = supported;
        }
    }
    // Controllability is advertised by the agent as InputCapability and maps
    // locally to paused. Capture readiness is separate: a capture-less source
    // cannot click, but can still vote on a drag it is receiving/hosting.
    fn local_click_voter(&self) -> bool {
        !self.paused && !self.fully_paused
    }
    fn local_click_group_ready(&self) -> bool {
        self.local_click_scope().members.len() <= 16
            && self
                .peers
                .values()
                .filter(|p| p.connected && p.sharing)
                .all(|p| p.local_click && p.focus_ready && p.session.is_some())
    }
    fn local_click_available(&self) -> bool {
        self.local_click_voter() && self.capture_available && self.local_click_group_ready()
    }
    fn local_click_scope(&self) -> FocusScope {
        let mut scope = self.new_focus_scope();
        scope.members.retain(|(d, _)| {
            if *d == self.me() {
                self.local_click_voter()
            } else {
                self.peers.get(d).is_some_and(|p| p.connected && p.sharing)
            }
        });
        scope
    }
    pub(super) fn local_click_peer_lost(&mut self, peer: DeviceId) {
        // An observer's link/session churn does not alter the vote set.
        if self
            .local_click
            .pending
            .values()
            .any(|p| p.scope.members.iter().any(|(d, _)| *d == peer))
        {
            self.local_click_cancel();
        }
    }
    pub(super) fn local_click_cancel(&mut self) {
        self.local_click_cancel_selected(None);
    }
    pub(super) fn local_click_cancel_own(&mut self) {
        self.local_click_cancel_selected(Some(self.me()));
    }
    fn local_click_cancel_selected(&mut self, origin: Option<DeviceId>) {
        let keys: Vec<_> = self
            .local_click
            .pending
            .keys()
            .copied()
            .filter(|(_, h)| origin.is_none_or(|o| o == *h))
            .collect();
        for (epoch, holder) in keys {
            let p = self
                .local_click
                .pending
                .remove(&(epoch, holder))
                .expect("selected proposal exists");
            self.local_click_retire(epoch, holder);
            self.local_click.cancelled.insert(
                holder,
                (epoch, p.scope.clone(), if holder == self.me() { ABORT } else { REJECT }),
            );
            if holder != self.me() {
                self.local_click_send(holder, epoch, holder, p.scope, REJECT);
            } else {
                self.broadcast(Message::Control(ControlMsg::LocalClick {
                    epoch,
                    holder,
                    scope: p.scope,
                    phase: ABORT,
                }));
            }
        }
    }
    pub(super) fn local_click_obsoletes_lock(&self, state: &crate::control::FocusLock) -> bool {
        state.locked
            && self
                .local_click
                .committed
                .as_ref()
                .is_some_and(|(epoch, holder, scope)| {
                    (*epoch, *holder) > (state.epoch, state.holder) && self.scope_allows(Some(scope), *holder)
                })
    }
    pub(super) fn local_click_preparing(&self, epoch: u64, holder: DeviceId) -> bool {
        self.local_click.pending.contains_key(&(epoch, holder))
            || (self.dnd_pointer_active()
                && self
                    .local_click
                    .committed
                    .as_ref()
                    .is_some_and(|(e, h, _)| (*e, *h) == (epoch, holder)))
    }
    fn local_click_scope_valid(&self, scope: &FocusScope) -> bool {
        let current = self.local_click_scope();
        scope.members.len() == current.members.len()
            && scope.members.iter().all(|member| current.members.contains(member))
            && scope.members.iter().map(|m| m.0).collect::<HashSet<_>>().len() == scope.members.len()
    }
    pub(super) fn local_click_reserved(&self) -> bool {
        !self.local_click.pending.is_empty()
    }
    pub(super) fn local_click_event(&mut self, ev: &CapturedEvent, woke: bool) {
        if !self.mouse_local()
            || !ev.physical
            || !matches!(ev.event, InputEvent::Button { pressed: true, .. })
            || (self.holder == self.me()
                && self.claim == self.me()
                && !self.fresh_join
                && self.local_click.pending.is_empty())
            || !self.local_click_available()
            || self.dnd_pointer_active()
            || self.local_click.pending.keys().any(|(_, holder)| *holder == self.me())
        {
            return;
        }
        if !woke
            && let Some(at) = ev.cursor
            && let Some(display) = self.displays.iter().find(|d| d.bounds.contains(at))
        {
            self.local_cursor = Some(ScreenPos::new(ScreenId::new(self.me(), &display.key), at));
        }
        let Some(at) = self.local_cursor.as_ref() else {
            return;
        };
        if self.hidden_entry(self.me(), &at.screen.display) || self.layout.source_requires_target(self.me()) {
            return;
        }
        let Some(epoch) = self.local_click.clock.max(self.epoch).checked_add(1) else {
            return;
        };
        self.local_click.clock = epoch;
        let holder = self.me();
        let scope = self.local_click_scope();
        self.local_click.pending.insert(
            (epoch, holder),
            Proposal {
                scope: scope.clone(),
                accepted: HashSet::from([holder]),
            },
        );
        self.broadcast(Message::Control(ControlMsg::LocalClick {
            epoch,
            holder,
            scope,
            phase: PREPARE,
        }));
        self.local_click_finish(epoch, holder);
    }
    fn local_click_send(&mut self, to: DeviceId, epoch: u64, holder: DeviceId, scope: FocusScope, phase: u8) {
        self.send(
            to,
            Message::Control(ControlMsg::LocalClick {
                epoch,
                holder,
                scope,
                phase,
            }),
        );
    }
    pub(super) fn local_click_receive(
        &mut self,
        from: DeviceId,
        epoch: u64,
        holder: DeviceId,
        scope: FocusScope,
        phase: u8,
    ) {
        if (!matches!(phase, SNAPSHOT | REJECT) && !scope.members.iter().any(|(d, _)| *d == from))
            || !scope.members.iter().any(|(d, _)| *d == holder)
            || epoch == 0
        {
            return;
        }
        if !self.local_click_scope_valid(&scope) {
            // This member sees a different online set (for example a link the
            // proposer lacks), so it can never accept. Refuse now: silence
            // would hold the proposer's and every voter's drag barrier.
            if phase == PREPARE
                && from == holder
                && (self.local_click_voter() || scope.members.iter().any(|(d, _)| *d == self.me()))
            {
                self.local_click_send(from, epoch, holder, scope, REJECT);
            }
            return;
        }
        self.local_click.clock = self.local_click.clock.max(epoch);
        let key = (epoch, holder);
        if phase == PREPARE && !self.local_click_voter() {
            return;
        }
        if !self.local_click_group_ready() {
            // Availability can change before its capability update reaches the
            // requester. Refuse now; never turn an old click into a new request
            // when permission or input readiness later returns.
            if phase == PREPARE && from == holder && self.peers.get(&from).is_some_and(|p| p.local_click) {
                self.local_click_retire(epoch, holder);
                self.local_click
                    .cancelled
                    .insert(holder, (epoch, scope.clone(), REJECT));
                self.local_click_send(from, epoch, holder, scope, REJECT);
            } else if phase == ABORT && from == holder {
                self.local_click.pending.remove(&key);
                self.local_click_retire(epoch, holder);
            }
            return;
        }
        match phase {
            PREPARE if from == holder => {
                if self
                    .local_click
                    .retired
                    .get(&holder)
                    .is_some_and(|retired| *retired >= epoch)
                {
                    self.local_click_send(from, epoch, holder, scope, REJECT);
                    return;
                }
                if !self.local_click.pending.contains_key(&key) {
                    if self.dnd_pointer_active() || self.local_click.pending.len() >= 16 {
                        self.local_click_retire(epoch, holder);
                        self.local_click
                            .cancelled
                            .insert(holder, (epoch, scope.clone(), REJECT));
                        self.local_click_send(from, epoch, holder, scope, REJECT);
                        return;
                    }
                    self.local_click.pending.insert(
                        key,
                        Proposal {
                            scope: scope.clone(),
                            accepted: HashSet::new(),
                        },
                    );
                }
                self.local_click_send(from, epoch, holder, scope, ACCEPT);
            }
            ACCEPT if holder == self.me() => {
                if let Some(p) = self.local_click.pending.get_mut(&key) {
                    p.accepted.insert(from);
                    self.local_click_finish(epoch, holder);
                }
            }
            REJECT if holder == self.me() => {
                // A newly controllable member may be missing from our stale
                // vote set. Its rejection must retire this request, never add
                // an affirmative vote or wait for the capability update.
                if self.local_click.pending.remove(&key).is_some() {
                    self.local_click_retire(epoch, holder);
                    self.local_click.cancelled.insert(holder, (epoch, scope.clone(), ABORT));
                    self.broadcast(Message::Control(ControlMsg::LocalClick {
                        epoch,
                        holder,
                        scope,
                        phase: ABORT,
                    }));
                }
            }
            COMMIT if from == holder => {
                if self.local_click.pending.remove(&key).is_some() || !self.local_click_voter() {
                    self.local_click_retire(epoch, holder);
                    self.local_click_apply(epoch, holder, scope);
                }
            }
            SNAPSHOT => {
                if self
                    .local_click
                    .committed
                    .as_ref()
                    .is_none_or(|(e, h, _)| (epoch, holder) > (*e, *h))
                {
                    self.local_click.committed = Some((epoch, holder, scope.clone()));
                }
                if !self.dnd_pointer_active() {
                    self.local_click.pending.remove(&key);
                    self.local_click_retire(epoch, holder);
                    self.local_click_apply(epoch, holder, scope);
                }
            }
            ABORT if from == holder => {
                self.local_click.pending.remove(&key);
                self.local_click_retire(epoch, holder);
            }
            _ => {}
        }
    }
    fn local_click_retire(&mut self, epoch: u64, holder: DeviceId) {
        let retired = self.local_click.retired.entry(holder).or_default();
        *retired = (*retired).max(epoch);
    }
    fn local_click_finish(&mut self, epoch: u64, holder: DeviceId) {
        let key = (epoch, holder);
        let Some(p) = self.local_click.pending.get(&key) else {
            return;
        };
        if !p.scope.members.iter().all(|(d, _)| p.accepted.contains(d)) {
            return;
        }
        let p = self.local_click.pending.remove(&key).expect("proposal exists");
        self.local_click_retire(epoch, holder);
        self.broadcast(Message::Control(ControlMsg::LocalClick {
            epoch,
            holder,
            scope: p.scope.clone(),
            phase: COMMIT,
        }));
        self.local_click_apply(epoch, holder, p.scope);
    }
    /// Reuse the existing reliable state repair envelope, without a new timer.
    pub(super) fn local_click_repair(&self, peer: DeviceId) -> Vec<Message> {
        if !self.peers.get(&peer).is_some_and(|p| p.connected && p.local_click) {
            return vec![];
        }
        let mut result = Vec::new();
        if let Some((epoch, holder, scope)) = &self.local_click.committed {
            result.push(Message::Control(ControlMsg::LocalClick {
                epoch: *epoch,
                holder: *holder,
                scope: scope.clone(),
                phase: SNAPSHOT,
            }));
        }
        for (&holder, (epoch, scope, phase)) in &self.local_click.cancelled {
            if holder == self.me() || holder == peer {
                result.push(Message::Control(ControlMsg::LocalClick {
                    epoch: *epoch,
                    holder,
                    scope: scope.clone(),
                    phase: *phase,
                }));
            }
        }
        for (&(epoch, holder), p) in &self.local_click.pending {
            if (holder == self.me() && p.scope.members.iter().any(|(d, _)| *d == peer)) || holder == peer {
                result.push(Message::Control(ControlMsg::LocalClick {
                    epoch,
                    holder,
                    scope: p.scope.clone(),
                    phase: if holder == self.me() { PREPARE } else { ACCEPT },
                }));
            }
        }
        result
    }
    fn local_click_apply(&mut self, epoch: u64, holder: DeviceId, scope: FocusScope) {
        // A certified greater claim also retires losing reservations whose own
        // commit was lost. Retaining only the winner must not leak a drag gate.
        let obsolete: Vec<_> = self
            .local_click
            .pending
            .keys()
            .copied()
            .filter(|key| *key <= (epoch, holder))
            .collect();
        for (e, h) in obsolete {
            self.local_click.pending.remove(&(e, h));
            self.local_click_retire(e, h);
        }
        if !claim_newer((epoch, holder), (self.epoch, self.claim)) {
            return;
        }
        // All members reserved against starting a drag before acknowledging.
        // The ordinary focus transition owns modifier retargeting and release.
        if self
            .local_click
            .committed
            .as_ref()
            .is_none_or(|(e, h, _)| (epoch, holder) > (*e, *h))
        {
            self.local_click.committed = Some((epoch, holder, scope.clone()));
        }
        self.suspend_focus_lock();
        // `scope` is the online vote set. Admission uses the ordinary proof, which
        // keeps retained instances of offline members, as an explicit recall does:
        // a member returning with the same instance then rejoins as a source.
        self.focus_scope = Some(self.new_focus_scope());
        if holder == self.me() {
            self.focus_joined = true;
            self.fresh_join = false;
            self.claim = holder;
            self.become_holder(epoch, self.local_cursor.clone());
        } else {
            self.adopt_claim(epoch, holder);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::input::InputMode;
    fn group() -> Vec<Router> {
        group_for(&[DeviceId([1; 32]), DeviceId([2; 32]), DeviceId([3; 32])])
    }
    fn group_for(ids: &[DeviceId]) -> Vec<Router> {
        let mut layout = crate::SimpleLayout::new();
        for (i, id) in ids.iter().enumerate() {
            layout.add_screen(
                ScreenId::new(*id, "screen"),
                Rect::new(0., 0., 1000., 600.),
                Rect::new(i as f64 * 300., 0., 250., 150.),
            );
        }
        ids.iter()
            .map(|id| {
                let mut r = Router::new(
                    RouterConfig::new(
                        oink_foundation::session::Boot {
                            device_id: *id,
                            nonce: [id.0[0]; 16],
                        },
                        "test",
                        Os::Windows,
                    ),
                    Arc::new(layout.clone()),
                );
                r.session = [id.0[0]; 16];
                r.set_local_displays(vec![DisplayInfo {
                    key: "screen".into(),
                    name: "screen".into(),
                    bounds: Rect::new(0., 0., 1000., 600.),
                    pixel_width: 1000,
                    pixel_height: 600,
                    reference_extent: None,
                    rotation_deg: 0,
                    is_primary: true,
                    is_builtin: false,
                    is_virtual: false,
                }]);
                r.set_cursor(Point::new(123., 234.));
                for peer in ids.iter().filter(|d| *d != id) {
                    r.test_connect(*peer, 0);
                    let p = r.peers.get_mut(peer).unwrap();
                    p.local_click = true;
                    p.focus_ready = true;
                }
                r.focus_scope = Some(r.local_click_scope());
                r.focus_joined = true;
                r.fresh_join = false;
                r.claim = ids[0];
                if *id != ids[0] {
                    r.become_source(10, ids[0]);
                } else {
                    r.epoch = 10;
                }
                let mut settings = r.cfg.settings.clone();
                settings.mouse_mode = InputMode::Local;
                r.set_settings(settings);
                r.out.clear();
                r
            })
            .collect()
    }
    fn click(r: &mut Router) -> Disposition {
        r.on_local_event(
            &CapturedEvent {
                event: InputEvent::Button {
                    button: MouseButton::Right,
                    pressed: true,
                },
                source: SourceKind::Mouse,
                physical: true,
                time_us: 1,
                cursor: Some(Point::new(333., 222.)),
            },
            1,
        )
    }
    fn pump(group: &mut [Router], reverse: bool) {
        for _ in 0..50 {
            let mut messages = vec![];
            for r in group.iter_mut() {
                let from = r.me();
                for o in std::mem::take(&mut r.out) {
                    if let Output::Send { to, msg, .. } = o {
                        if matches!(&msg, Message::Control(ControlMsg::LocalClick { .. }))
                            && !r.peers.get(&to).is_some_and(|p| p.local_click)
                        {
                            continue;
                        }
                        messages.push((from, to, msg));
                    }
                }
            }
            if messages.is_empty() {
                return;
            }
            if reverse {
                messages.reverse();
            }
            for (from, to, msg) in messages {
                let r = group.iter_mut().find(|r| r.me() == to).unwrap();
                r.input_message(from, msg.clone(), 2);
                // Duplicate delivery cannot manufacture another claim.
                r.input_message(from, msg, 2);
            }
        }
        panic!("message loop");
    }
    #[test]
    fn click_recalls_without_moving_cursor_and_breaks_lock() {
        let mut g = group();
        g[0].set_locked(true);
        pump(&mut g, false);
        assert!(g[0].locked());
        let target = g[1].me();
        assert_eq!(click(&mut g[1]), Disposition::Pass);
        pump(&mut g, false);
        assert!(g.iter().all(|r| r.holder == target));
        assert_eq!(g[1].pointer.as_ref().unwrap().local, Point::new(333., 222.));
    }
    #[test]
    fn concurrent_clicks_converge_under_reorder_and_duplicates() {
        for reverse in [false, true] {
            let mut g = group();
            click(&mut g[1]);
            click(&mut g[2]);
            pump(&mut g, reverse);
            let expected = (11, g[2].me());
            assert!(
                g.iter().all(|r| r.focus_claim() == expected),
                "{:?}",
                g.iter().map(Router::focus_claim).collect::<Vec<_>>()
            );
            assert!(g.iter().all(|r| !r.local_click_reserved()));
        }
    }
    #[test]
    fn old_peer_disables_recall() {
        let mut g = group();
        g[1].peers.get_mut(&DeviceId([3; 32])).unwrap().local_click = false;
        click(&mut g[1]);
        pump(&mut g, false);
        assert!(g.iter().all(|r| r.claim == DeviceId([1; 32])));
    }
    #[test]
    fn disconnect_retires_pending_reservations() {
        let mut g = group();
        click(&mut g[1]);
        assert!(g[1].local_click_reserved());
        g[1].test_disconnect(DeviceId([3; 32]), 2);
        assert!(!g[1].local_click_reserved());
    }
    #[test]
    fn game_capture_is_overridden_without_changing_owner_preference() {
        let mut g = group();
        g[0].set_game_mode(GameMode::On);
        pump(&mut g, false);
        assert!(g[0].game_mode().active);
        click(&mut g[1]);
        pump(&mut g, true);
        assert!(g.iter().all(|r| r.claim == DeviceId([2; 32])));
    }
    #[test]
    fn movement_wheel_gesture_up_and_synthetic_down_do_not_claim() {
        let mut g = group();
        for event in [
            InputEvent::PointerDelta { dx: 1., dy: 2. },
            InputEvent::RawPointerDelta { dx: 1., dy: 2. },
            InputEvent::Button {
                button: MouseButton::Left,
                pressed: false,
            },
            InputEvent::Gesture(Gesture::SmartMagnify),
            InputEvent::Scroll {
                dx: 0.,
                dy: 1.,
                unit: ScrollUnit::Line,
                phase: ScrollPhase::None,
                wheel_steps: None,
            },
        ] {
            g[1].on_local_event(
                &CapturedEvent {
                    event,
                    source: SourceKind::Mouse,
                    physical: true,
                    time_us: 2,
                    cursor: Some(Point::new(300., 200.)),
                },
                2,
            );
        }
        g[1].on_local_event(
            &CapturedEvent {
                event: InputEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                },
                source: SourceKind::Mouse,
                physical: false,
                time_us: 2,
                cursor: None,
            },
            2,
        );
        pump(&mut g, true);
        assert!(g.iter().all(|r| r.claim == DeviceId([1; 32])));
    }
    #[test]
    fn remote_drag_vetoes_click_and_is_not_cancelled() {
        let mut g = group();
        let source = g[0].me();
        g[0].cfg.settings.mouse_mode = InputMode::Shared;
        g[0].set_dnd_available(true);
        g[0].cfg.settings.dnd = true;
        g[0].dnd_source_learned(
            oink_foundation::content::ClipId { origin: source, seq: 1 },
            source,
            vec![],
        );
        assert!(g[0].dnd_pointer_active());
        // Drop the announcement: a third machine cannot know this drag yet.
        g[0].out.clear();
        click(&mut g[1]);
        pump(&mut g, true);
        assert!(g.iter().all(|r| r.claim == source));
        assert!(g[0].dnd_pointer_active());
        assert!(g.iter().all(|r| !r.local_click_reserved()));
    }
    #[test]
    fn lost_prepare_and_commit_repair_from_existing_state_envelope() {
        let mut g = group();
        click(&mut g[1]);
        // Entire first transmission lost. Repaired records use the already
        // existing reliable network heartbeat envelope, not a new timer.
        g[1].out.clear();
        for i in 0..g.len() {
            for j in 0..g.len() {
                if i != j {
                    let from = g[i].me();
                    let msgs = g[i].local_click_repair(g[j].me());
                    for msg in msgs {
                        g[j].input_message(from, msg, 3);
                    }
                }
            }
        }
        pump(&mut g, true);
        assert!(g.iter().all(|r| r.claim == DeviceId([2; 32])));
        // A lagging member receives only the committed record, with no replay
        // of pointer position or ordinary key downs.
        g[2].claim = DeviceId([1; 32]);
        g[2].epoch = 10;
        for msg in g[1].local_click_repair(g[2].me()) {
            g[2].input_message(DeviceId([2; 32]), msg, 4);
        }
        assert_eq!(g[2].claim, DeviceId([2; 32]));
    }

    /// Delivers queued sends; sends to devices outside the group are lost.
    fn pump_known(group: &mut [Router]) {
        for _ in 0..50 {
            let mut messages = vec![];
            for r in group.iter_mut() {
                let from = r.me();
                for o in std::mem::take(&mut r.out) {
                    if let Output::Send { to, msg, .. } = o {
                        messages.push((from, to, msg));
                    }
                }
            }
            if messages.is_empty() {
                return;
            }
            for (from, to, msg) in messages {
                if let Some(r) = group.iter_mut().find(|r| r.me() == to) {
                    r.input_message(from, msg, 2);
                }
            }
        }
        panic!("message loop");
    }
    fn repair_rounds(group: &mut [Router]) {
        for _ in 0..3 {
            for i in 0..group.len() {
                for j in 0..group.len() {
                    if i != j {
                        let from = group[i].me();
                        for msg in group[i].heartbeat_messages(group[j].me()) {
                            group[j].input_message(from, msg, 3);
                        }
                    }
                }
            }
            pump_known(group);
        }
    }
    #[test]
    fn member_with_a_different_online_scope_refuses_instead_of_holding_the_barrier() {
        // D is online only for C (partial mesh). B cannot poll D, so C must refuse
        // B's proposal at once; silence would hold A's and B's drag barrier forever.
        let mut g = group();
        let d = DeviceId([4; 32]);
        g[2].test_connect(d, 0);
        let p = g[2].peers.get_mut(&d).unwrap();
        p.local_click = true;
        p.focus_ready = true;
        g[2].out.clear();
        click(&mut g[1]);
        pump_known(&mut g);
        repair_rounds(&mut g);
        assert!(g.iter().all(|r| !r.local_click_reserved()));
        assert!(g.iter().all(|r| r.claim == DeviceId([1; 32])));
    }
    #[test]
    fn member_offline_during_click_rejoins_the_claim_like_an_ordinary_recall() {
        let mut g = group();
        let (a, b, c) = (g[0].me(), g[1].me(), g[2].me());
        for (i, peer) in [(2, a), (2, b), (0, c), (1, c)] {
            g[i].test_disconnect(peer, 2);
        }
        for r in g.iter_mut() {
            r.out.clear();
        }
        click(&mut g[1]);
        pump_known(&mut g);
        assert_eq!(g[0].focus_claim(), (11, b));
        // Same routing instance comes back (network blip, not a restart).
        for (i, peer) in [(2, a), (2, b), (0, c), (1, c)] {
            g[i].test_connect(peer, 3);
            g[i].set_peer_local_click(peer, true);
        }
        pump_known(&mut g);
        repair_rounds(&mut g);
        assert!(g.iter().all(|r| r.focus_claim() == (11, b)));
        assert_eq!(g[2].holder, b, "the returning member must act as a source of the claim");
    }
    include!("local_click_sim_tests.rs");

    #[test]
    fn click_keeps_existing_modifier_and_ordinary_key_ownership() {
        let mut g = group();
        for code in [KeyCode::LEFT_SHIFT, KeyCode::keyboard(4)] {
            g[1].on_local_event(
                &CapturedEvent {
                    event: InputEvent::Key { code, pressed: true },
                    source: SourceKind::Keyboard,
                    physical: true,
                    time_us: 1,
                    cursor: None,
                },
                1,
            );
        }
        pump(&mut g, false);
        click(&mut g[1]);
        pump(&mut g, false);
        assert!(
            g[1].keys[&KeyCode::LEFT_SHIFT]
                .iter()
                .any(|t| matches!(t, Target::LocalInjected | Target::Local))
        );
        assert!(
            g[1].keys[&KeyCode::keyboard(4)]
                .iter()
                .all(|t| matches!(t,Target::Remote(d) if *d==DeviceId([1;32])))
        );
        for code in [KeyCode::LEFT_SHIFT, KeyCode::keyboard(4)] {
            g[1].on_local_event(
                &CapturedEvent {
                    event: InputEvent::Key { code, pressed: false },
                    source: SourceKind::Keyboard,
                    physical: true,
                    time_us: 2,
                    cursor: None,
                },
                2,
            );
        }
        pump(&mut g, false);
        assert!(g.iter().all(|r| r.key_refs.is_empty()));
    }
    #[test]
    fn reservation_blocks_new_drag_until_decision() {
        let mut g = group();
        click(&mut g[1]);
        let scope = g[1].local_click_scope();
        g[0].local_click_receive(DeviceId([2; 32]), 11, DeviceId([2; 32]), scope, PREPARE);
        assert!(g[0].local_click_reserved());
        g[0].cfg.settings.mouse_mode = InputMode::Shared;
        g[0].cfg.settings.dnd = true;
        g[0].set_dnd_available(true);
        g[0].dnd_source_learned(
            oink_foundation::content::ClipId {
                origin: DeviceId([1; 32]),
                seq: 1,
            },
            DeviceId([1; 32]),
            vec![],
        );
        assert!(!g[0].dnd_pointer_active());
        pump(&mut g, false);
        assert!(g.iter().all(|r| !r.local_click_reserved()));
    }
    #[test]
    fn stale_scope_cannot_reclaim_after_process_replacement() {
        let mut g = group();
        click(&mut g[1]);
        pump(&mut g, false);
        let record = g[1].local_click_repair(g[2].me());
        g[2].session = [99; 16];
        g[2].epoch = 10;
        g[2].claim = DeviceId([1; 32]);
        for msg in record {
            g[2].input_message(DeviceId([2; 32]), msg, 4);
        }
        assert_eq!(g[2].claim, DeviceId([1; 32]));
    }
    #[test]
    fn concurrent_clicks_with_retained_owner_locks_converge() {
        for reverse in [false, true] {
            let mut g = group();
            g[1].owner_pointer_locked = true;
            g[2].owner_pointer_locked = true;
            click(&mut g[1]);
            click(&mut g[2]);
            pump(&mut g, reverse);
            assert!(
                g.iter().all(|r| r.holder == DeviceId([3; 32])),
                "{:?}",
                g.iter().map(Router::focus_claim).collect::<Vec<_>>()
            );
        }
    }
    #[test]
    fn island_click_can_publish_group_claim_and_both_platforms_keep_first_click() {
        for os in [Os::MacOS, Os::Windows] {
            let mut g = group();
            g[1].cfg.os = os;
            g[1].island();
            assert_ne!(g[1].claim, g[1].me());
            assert_eq!(click(&mut g[1]), Disposition::Pass);
            pump(&mut g, false);
            assert!(g.iter().all(|r| r.claim == DeviceId([2; 32])));
        }
    }
    #[test]
    fn parked_first_click_replays_once_before_async_claim() {
        let mut g = group();
        g[1].local_mouse_hidden = true;
        assert_eq!(click(&mut g[1]), Disposition::Swallow);
        assert_eq!(
            g[1].out
                .iter()
                .filter(|o| matches!(
                    o,
                    Output::Inject(InjectEvent::Button {
                        button: MouseButton::Right,
                        pressed: true
                    })
                ))
                .count(),
            1
        );
        pump(&mut g, false);
        assert_eq!(g[1].pointer.as_ref().unwrap().local, Point::new(123., 234.));
    }
    #[test]
    fn same_instance_reconnect_reconciles_and_old_peer_join_disables_new_clicks() {
        let mut g = group();
        click(&mut g[1]);
        pump(&mut g, true);
        g[2].test_disconnect(DeviceId([2; 32]), 3);
        g[2].test_connect(DeviceId([2; 32]), 4);
        g[2].set_peer_local_click(DeviceId([2; 32]), true);
        g[1].send_focus_state(DeviceId([3; 32]));
        pump(&mut g, true);
        // Reattach bootstrap plus the retained snapshot converge without a new click.
        for msg in g[1].heartbeat_messages(DeviceId([3; 32])) {
            g[2].input_message(DeviceId([2; 32]), msg, 5);
        }
        pump(&mut g, false);
        assert!(g.iter().all(|r| r.focus_claim() == (11, DeviceId([2; 32]))));
        g[2].set_peer_local_click(DeviceId([1; 32]), false);
        click(&mut g[2]);
        pump(&mut g, true);
        assert!(g.iter().all(|r| r.focus_claim() == (11, DeviceId([2; 32]))));
    }
    #[test]
    fn capture_unavailable_member_still_votes_without_claiming_for_itself() {
        let mut g = group();
        g[0].set_capture_available(false);
        g[0].out.clear();
        click(&mut g[1]);
        pump(&mut g, false);
        assert!(g.iter().all(|r| !r.local_click_reserved()));
        assert!(g.iter().all(|r| r.claim == DeviceId([2; 32])));
        click(&mut g[0]);
        pump(&mut g, false);
        assert!(g.iter().all(|r| r.claim == DeviceId([2; 32])));
        g[0].set_capture_available(true);
        click(&mut g[0]);
        pump(&mut g, false);
        assert!(g.iter().all(|r| r.claim == DeviceId([1; 32])));
    }
    fn controllable(g: &mut [Router], index: usize, enabled: bool) {
        let id = g[index].me();
        // This is the router state used by Driver::update_controllable; the
        // remote InputCapability arrives on a replacement input attachment.
        g[index].set_paused(!enabled);
        for (i, r) in g.iter_mut().enumerate() {
            if i != index {
                r.local_click_cancel();
                r.peers.get_mut(&id).unwrap().sharing = enabled;
            }
        }
    }
    #[test]
    fn uncontrollable_online_observer_does_not_vote_or_block_other_clicks() {
        for reverse in [false, true] {
            let mut g = group();
            controllable(&mut g, 2, false);
            // Even a non-controllable older member need not support the new
            // vote protocol. It still learns the ordinary focus register.
            for r in &mut g[..2] {
                r.set_peer_local_click(DeviceId([3; 32]), false);
            }
            assert_eq!(g[1].local_click_scope().members.len(), 2);
            click(&mut g[1]);
            pump(&mut g, reverse);
            // Older non-voters get no extension packets; ordinary state repair
            // still carries the winning focus and retained admission proof.
            for msg in g[1].heartbeat_messages(DeviceId([3; 32])) {
                g[2].input_message(DeviceId([2; 32]), msg, 3);
            }
            assert!(g.iter().all(|r| r.focus_claim() == (11, DeviceId([2; 32]))));
            assert!(g.iter().all(|r| !r.local_click_reserved()));
            click(&mut g[2]);
            pump(&mut g, reverse);
            assert!(g.iter().all(|r| r.focus_claim() == (11, DeviceId([2; 32]))));
        }
    }
    #[test]
    fn recovery_requires_new_click_and_restores_voting_and_target_eligibility() {
        let mut g = group();
        controllable(&mut g, 2, false);
        click(&mut g[2]);
        click(&mut g[1]);
        pump(&mut g, false);
        controllable(&mut g, 2, true);
        pump(&mut g, false);
        assert!(g.iter().all(|r| r.claim == DeviceId([2; 32])));
        assert_eq!(g[1].local_click_scope().members.len(), 3);
        click(&mut g[2]);
        pump(&mut g, true);
        assert!(g.iter().all(|r| r.claim == DeviceId([3; 32])));
        assert!(g.iter().all(|r| !r.local_click_reserved()));
    }
    #[test]
    fn excluded_target_cannot_be_claimed_by_old_commit_or_snapshot() {
        let mut g = group();
        let scope = g[2].local_click_scope();
        controllable(&mut g, 2, false);
        for phase in [PREPARE, COMMIT, SNAPSHOT] {
            g[0].local_click_receive(DeviceId([3; 32]), 99, DeviceId([3; 32]), scope.clone(), phase);
        }
        assert_eq!(g[0].claim, DeviceId([1; 32]));
        assert!(!g[0].local_click_reserved());
    }
    #[test]
    fn inconsistent_controllability_rejects_without_leaking_reservations() {
        let mut g = group();
        g[2].set_paused(true);
        // C has lost permission, but A/B still have its old I0 capability.
        click(&mut g[1]);
        pump(&mut g, true);
        assert!(g.iter().all(|r| !r.local_click_reserved()));
        assert!(g.iter().all(|r| r.claim == DeviceId([1; 32])));
        controllable(&mut g, 2, false);
        click(&mut g[1]);
        pump(&mut g, false);
        assert!(g.iter().all(|r| r.claim == DeviceId([2; 32])));
    }
    #[test]
    fn eligible_drag_still_vetoes_with_an_uncontrollable_observer_online() {
        let mut g = group();
        controllable(&mut g, 2, false);
        let source = g[0].me();
        g[0].cfg.settings.mouse_mode = InputMode::Shared;
        g[0].set_dnd_available(true);
        g[0].cfg.settings.dnd = true;
        g[0].dnd_source_learned(
            oink_foundation::content::ClipId { origin: source, seq: 1 },
            source,
            vec![],
        );
        assert!(g[0].dnd_pointer_active());
        g[0].out.clear();
        click(&mut g[1]);
        pump(&mut g, true);
        assert!(g.iter().all(|r| r.claim == source));
        assert!(g[0].dnd_pointer_active());
        assert!(g.iter().all(|r| !r.local_click_reserved()));
    }
    #[test]
    fn observer_can_repair_a_committed_claim() {
        let mut g = group();
        controllable(&mut g, 2, false);
        click(&mut g[1]);
        pump(&mut g, false);
        // The observer retains the same maximum, even though it did not vote.
        g[0].epoch = 10;
        g[0].claim = DeviceId([1; 32]);
        for msg in g[2].local_click_repair(DeviceId([1; 32])) {
            g[0].input_message(DeviceId([3; 32]), msg, 3);
        }
        assert_eq!(g[0].focus_claim(), (11, DeviceId([2; 32])));
        assert!(!g[2].local_click_reserved());
    }
    #[test]
    fn concurrent_clicks_converge_with_nonvoter_and_lost_first_transmissions() {
        let mut g = group_for(&[
            DeviceId([1; 32]),
            DeviceId([2; 32]),
            DeviceId([3; 32]),
            DeviceId([4; 32]),
        ]);
        controllable(&mut g, 3, false);
        click(&mut g[1]);
        click(&mut g[2]);
        g[1].out.clear();
        g[2].out.clear();
        repair_rounds(&mut g);
        pump(&mut g, true);
        assert!(g.iter().all(|r| r.focus_claim() == (11, DeviceId([3; 32]))));
        assert!(g.iter().all(|r| !r.local_click_reserved()));
    }
    #[test]
    fn recovering_member_missing_from_proposers_vote_set_can_refuse() {
        let mut g = group();
        controllable(&mut g, 2, false);
        g[2].set_paused(false); // A/B have not received its replacement I0 yet.
        click(&mut g[1]);
        pump(&mut g, true);
        assert!(g.iter().all(|r| !r.local_click_reserved()));
        assert!(g.iter().all(|r| r.claim == DeviceId([1; 32])));
        controllable(&mut g, 2, true);
        click(&mut g[1]);
        pump(&mut g, false);
        assert!(g.iter().all(|r| r.claim == DeviceId([2; 32])));
    }
    #[test]
    fn observer_disconnect_and_noncontrollable_reattachment_do_not_abort_votes() {
        let mut g = group();
        controllable(&mut g, 2, false);
        click(&mut g[1]);
        let observer = DeviceId([3; 32]);
        for r in &mut g[..2] {
            r.test_disconnect(observer, 2);
            assert!(r.attach_input(
                oink_foundation::session::Boot {
                    device_id: observer,
                    nonce: [3; 16]
                },
                [3; 16],
                false,
                true,
                FeelProfile::default(),
                3
            ));
            r.set_peer_local_click(observer, true);
            r.peers.get_mut(&observer).unwrap().focus_ready = true;
        }
        assert!(g[1].local_click_reserved());
        pump(&mut g, true);
        repair_rounds(&mut g);
        assert!(g.iter().all(|r| r.claim == DeviceId([2; 32])));
        assert!(g.iter().all(|r| !r.local_click_reserved()));
    }
}
