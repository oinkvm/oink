//! Focus-scoped relative input. No layout/damping calls occur on this path.
use super::*;
pub use crate::control::GameMode;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GameModeStatus {
    pub holder: DeviceId,
    pub epoch: u64,
    pub mode: GameMode,
    pub detected: bool,
    pub active: bool,
    /// Current holder: None until sampled; false when the hiding cause is unavailable.
    /// Informational only: Auto then uses visibility/motion evidence, not this flag.
    pub cursor_hiding_cause_available: Option<bool>,
}

#[derive(Debug, Default)]
pub(super) struct GameState {
    scope: Option<(u64, DeviceId)>,
    pub(super) mode: GameMode,
    passive_detected: bool,
    pub(super) remote_capture: super::game_capture::RemoteCapture,
    detected: bool,
    active: bool,
    cursor_hiding_cause_available: Option<bool>,
    generation: u64,
    authority_generation: u64,
    tx_scope: Option<(u64, DeviceId, u64)>,
    tx: (u64, f64, f64),
    rx: HashMap<DeviceId, (u64, f64, f64)>,
}

impl GameState {
    pub(super) fn for_owner(mode: GameMode) -> Self {
        Self {
            mode,
            active: mode == GameMode::On,
            generation: 1,
            authority_generation: 1,
            ..Default::default()
        }
    }
}

impl Router {
    pub fn game_mode(&self) -> GameModeStatus {
        GameModeStatus {
            holder: self.holder,
            epoch: self.epoch,
            mode: self.game.mode,
            detected: self.game.detected,
            active: self.game.active && !self.paused,
            cursor_hiding_cause_available: self.game.cursor_hiding_cause_available,
        }
    }

    /// A manual override applies to the current focus device, never pulls focus home.
    /// Remote requests complete asynchronously through the holder's status event.
    pub fn set_game_mode(&mut self, mode: GameMode) {
        if self.paused {
            return;
        }
        if self.holder != self.me() {
            self.send(
                self.holder,
                Message::Control(ControlMsg::SetGameMode {
                    epoch: self.epoch,
                    mode,
                }),
            );
            return;
        }
        self.game.scope = Some((self.epoch, self.holder));
        self.game.mode = mode;
        if mode != GameMode::Auto {
            self.game.remote_capture.reset("manual_mode");
            self.game.detected = self.game.passive_detected;
        }
        self.refresh_game();
    }

    pub(super) fn apply_game_preference(&mut self) {
        self.game.mode = self.cfg.settings.game_mode_preference;
        self.game.remote_capture.reset("owner_preference");
        self.game.detected = self.game.passive_detected;
        self.refresh_game();
    }

    pub fn toggle_game_mode(&mut self) {
        self.set_game_mode(if self.game_mode().active {
            GameMode::Off
        } else {
            GameMode::On
        });
    }

    /// Debounced local platform observation, never the source's Oink-owned freeze.
    pub fn set_pointer_captured(&mut self, detected: bool) {
        if self.has_pointer_focus() {
            self.game.passive_detected = detected;
            let detected = detected || self.game.remote_capture.detected;
            if self.game.detected != detected {
                self.game.detected = detected;
                self.refresh_game();
            }
        }
    }

    /// Combine the passive platform detector with macOS-only remote movement
    /// evidence. Call with read-only samples every 50 ms, even while not eligible.
    /// Manual Off suppresses the remote-hidden rule until Auto is selected again.
    pub fn observe_pointer_capture(
        &mut self,
        sample: Option<PointerCaptureObservation>,
        passive_detected: bool,
        now: Micros,
    ) {
        let availability = sample.map(|s| s.application_hidden.is_some());
        let availability_changed = self.has_pointer_focus() && self.game.cursor_hiding_cause_available != availability;
        if availability_changed {
            self.game.cursor_hiding_cause_available = availability;
        }
        let previously_detected = self.game.detected;
        if self.cfg.os == Os::MacOS && self.has_pointer_focus() && !self.paused && self.game.mode == GameMode::Auto {
            self.game.remote_capture.observe(sample, now);
        } else {
            self.game.remote_capture.reset("not_auto_focus_or_paused");
        }
        self.set_pointer_captured(passive_detected);
        if availability_changed && previously_detected == self.game.detected {
            self.refresh_game();
        }
    }

    fn emit_game_status(&mut self) {
        let status = self.game_mode();
        if self.last_game_status.as_ref() != Some(&status) {
            self.last_game_status = Some(status.clone());
            self.out.push(Output::Status(StatusEvent::GameMode(status)));
        }
    }

    fn refresh_game(&mut self) {
        self.game.scope = Some((self.epoch, self.holder));
        let active = !self.paused
            && match self.game.mode {
                GameMode::Auto => self.game.detected,
                GameMode::On => true,
                GameMode::Off => false,
            };
        if self.game.active != active {
            self.game.active = active;
            self.game.authority_generation += 1;
            self.game.generation = self.game.authority_generation;
            self.game.tx = (0, 0., 0.);
            self.game.rx.clear();
            self.reset_resistance();
            if active {
                self.gap_cancel();
                self.dnd_abort("game mode");
            }
        }
        self.emit_game_status();
        let peers: Vec<_> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected)
            .map(|(d, _)| *d)
            .collect();
        for peer in peers {
            self.send_game_state(peer);
        }
    }

    pub(super) fn keep_game_focus_epoch(&mut self) {
        self.game.scope = Some((self.epoch, self.holder));
        self.emit_game_status();
    }

    pub(super) fn reset_game_focus(&mut self) {
        let scope = (self.epoch, self.holder);
        if self.game.scope != Some(scope) || self.paused {
            self.game.remote_capture.reset("focus_changed_or_paused");
            let authority_generation = self.game.authority_generation + 1;
            let mode = if self.holder == self.me() {
                self.cfg.settings.game_mode_preference
            } else {
                GameMode::Auto
            };
            self.game = GameState {
                scope: Some(scope),
                mode,
                active: !self.paused && mode == GameMode::On,
                authority_generation,
                generation: if self.holder == self.me() {
                    authority_generation
                } else {
                    0
                },
                tx_scope: self.game.tx_scope,
                tx: self.game.tx,
                ..Default::default()
            };
            if self.holder == self.me() {
                self.refresh_game();
            } else {
                self.emit_game_status();
            }
        }
    }

    pub(super) fn restart_game_stream(&mut self) {
        if self.holder == self.me() && self.game.active {
            self.game.authority_generation += 1;
            self.game.generation = self.game.authority_generation;
            self.game.rx.clear();
            self.refresh_game();
        }
    }

    pub(super) fn input_game_state(&self) -> Message {
        Message::Control(ControlMsg::GameMode {
            epoch: self.epoch,
            generation: self.game.generation,
            mode: self.game.mode,
            detected: self.game.detected,
            active: self.game.active,
            cursor_hiding_cause_available: self.game.cursor_hiding_cause_available,
        })
    }

    pub(super) fn send_game_state(&mut self, peer: DeviceId) {
        if self.holder == self.me() && self.claim == self.me() {
            self.send(
                peer,
                Message::Control(ControlMsg::GameMode {
                    epoch: self.epoch,
                    generation: self.game.generation,
                    mode: self.game.mode,
                    detected: self.game.detected,
                    active: self.game.active,
                    cursor_hiding_cause_available: self.game.cursor_hiding_cause_available,
                }),
            );
        }
    }

    #[allow(clippy::too_many_arguments)] // Fields of the holder-authoritative wire message.
    pub(super) fn receive_game_state(
        &mut self,
        from: DeviceId,
        epoch: u64,
        generation: u64,
        mode: GameMode,
        detected: bool,
        active: bool,
        cursor_hiding_cause_available: Option<bool>,
    ) {
        if from != self.holder || from == self.me() || epoch != self.epoch || generation < self.game.generation {
            return;
        }
        let scope = (epoch, from, generation);
        if self.game.tx_scope != Some(scope) {
            self.game.tx_scope = Some(scope);
            self.game.tx = (0, 0., 0.);
        }
        self.game.generation = generation;
        self.game.mode = mode;
        self.game.detected = detected;
        self.game.active = active;
        self.game.cursor_hiding_cause_available = cursor_hiding_cause_available;
        self.emit_game_status();
    }

    pub(super) fn raw_motion(&mut self, dx: f64, dy: f64) -> Disposition {
        if !self.mouse_local()
            && self.holder != self.me()
            && self.game_mode().active
            && dx.is_finite()
            && dy.is_finite()
            && (dx != 0. || dy != 0.)
        {
            self.game.tx.0 += 1;
            self.game.tx.1 += dx;
            self.game.tx.2 += dy;
            self.send_relative_total();
        }
        Disposition::Pass // observational companion stream; normal motion decides swallow
    }

    pub(super) fn send_relative_total(&mut self) {
        if self.mouse_local() || self.holder == self.me() || !self.game_mode().active || self.game.tx.0 == 0 {
            return;
        }
        self.send(
            self.holder,
            Message::Input(InputMsg::RelativeMotion {
                epoch: self.epoch,
                generation: self.game.generation,
                seq: self.game.tx.0,
                x: self.game.tx.1,
                y: self.game.tx.2,
            }),
        );
    }

    pub(super) fn receive_relative(&mut self, origin: DeviceId, epoch: u64, generation: u64, seq: u64, x: f64, y: f64) {
        // Relative packets never migrate to another focus/generation or get forwarded.
        if self.holder != self.me()
            || self.claim != self.me()
            || !self.game_mode().active
            || epoch != self.epoch
            || generation != self.game.generation
            || !x.is_finite()
            || !y.is_finite()
        {
            return;
        }
        let prev = self.game.rx.entry(origin).or_insert((0, 0., 0.));
        if seq <= prev.0 {
            return;
        }
        let (dx, dy) = (x - prev.1, y - prev.2);
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        *prev = (seq, x, y);
        if dx != 0. || dy != 0. {
            self.last_pointer_source = Some(origin);
            self.out.push(Output::Inject(InjectEvent::MoveRelative { dx, dy }));
        }
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;

    #[test]
    fn status_changes_once_but_wire_gossip_remains_periodic() {
        let me = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let mut r = Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: me,
                    nonce: [me.0[0]; 16],
                },
                "local",
                Os::Other,
            ),
            crate::router::connection_tests::session_layout(me, peer),
        );
        r.test_connect(peer, 0);
        r.out.clear();
        r.set_game_mode(GameMode::On);
        assert_eq!(
            r.out
                .iter()
                .filter(|o| matches!(o, Output::Status(StatusEvent::GameMode(_))))
                .count(),
            1
        );
        r.out.clear();
        r.set_game_mode(GameMode::On);
        assert!(
            !r.out
                .iter()
                .any(|o| matches!(o, Output::Status(StatusEvent::GameMode(_))))
        );
        r.out.clear();
        for heartbeat in 1..=6 {
            r.send_state_sync();
            let wire: Vec<_> = r
                .out
                .iter()
                .filter_map(|o| match o {
                    Output::Send {
                        msg: Message::Control(ControlMsg::GameMode { mode, active, .. }),
                        ..
                    } => Some((*mode, *active)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                wire,
                if heartbeat % 2 == 0 {
                    vec![(GameMode::On, true)]
                } else {
                    vec![]
                }
            );
            assert!(
                !r.out
                    .iter()
                    .any(|o| matches!(o, Output::Status(StatusEvent::GameMode(_))))
            );
            r.out.clear();
        }
        r.set_game_mode(GameMode::Off);
        assert_eq!(
            r.out
                .iter()
                .filter(|o| matches!(o, Output::Status(StatusEvent::GameMode(_))))
                .count(),
            1
        );
    }

    #[test]
    fn owner_preference_survives_focus_epochs_and_pause_but_override_does_not() {
        let me = DeviceId([1; 32]);
        let mut config = RouterConfig::new(
            oink_foundation::session::Boot {
                device_id: me,
                nonce: [me.0[0]; 16],
            },
            "local",
            Os::Other,
        );
        config.settings.game_mode_preference = GameMode::On;
        let mut r = Router::new(config, Arc::new(crate::SimpleLayout::new()));
        assert!(r.game_mode().active);
        r.set_game_mode(GameMode::Off);
        assert!(!r.game_mode().active);
        assert_eq!(r.config().settings.game_mode_preference, GameMode::On);
        r.become_holder(2, None);
        assert_eq!(r.game_mode().mode, GameMode::On);
        assert!(r.game_mode().active);
        r.set_fully_paused(true);
        assert!(!r.game_mode().active);
        r.set_fully_paused(false);
        assert!(r.game_mode().active);
    }

    #[test]
    fn changing_local_owner_preference_does_not_override_the_remote_holder() {
        let me = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let mut r = Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: me,
                    nonce: [me.0[0]; 16],
                },
                "local",
                Os::Other,
            ),
            Arc::new(crate::SimpleLayout::new()),
        );
        r.test_connect(peer, 0);
        r.become_source(2, peer);
        r.game.mode = GameMode::Off;
        let mut settings = r.config().settings.clone();
        settings.game_mode_preference = GameMode::On;
        r.set_settings(settings);
        assert_eq!(r.game_mode().mode, GameMode::Off);
        r.become_holder(3, None);
        assert_eq!(r.game_mode().mode, GameMode::On);
        assert!(r.game_mode().active);
    }

    #[test]
    fn late_connection_keeps_relative_totals_and_real_disconnect_changes_generation() {
        let me = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let mut r = Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: me,
                    nonce: [me.0[0]; 16],
                },
                "local",
                Os::Other,
            ),
            crate::router::connection_tests::session_layout(me, peer),
        );
        r.test_message(
            peer,
            Message::Control(ControlMsg::FocusEnter {
                epoch: 1,
                display: "screen".into(),
                pos: Point::new(0., 0.),
                scope: Some(r.new_focus_scope()),
                pointer_source: Some(peer),
            }),
            0,
        );
        r.set_game_mode(GameMode::On);
        let generation = r.game.generation;
        let motion = |seq, x| {
            Message::Input(InputMsg::RelativeMotion {
                epoch: 1,
                generation,
                seq,
                x,
                y: 0.,
            })
        };
        r.test_message(peer, motion(1, 5.), 1);
        r.test_connect(peer, 2);
        assert_eq!(r.game.generation, generation);
        r.out.clear();
        r.test_message(peer, motion(1, 5.), 3);
        assert!(
            r.out.is_empty(),
            "the same cumulative movement cannot be injected twice"
        );
        r.test_message(peer, motion(2, 7.), 4);
        assert!(
            r.out
                .iter()
                .any(|o| matches!(o, Output::Inject(InjectEvent::MoveRelative { dx: 2., dy: 0. })))
        );
        r.test_disconnect(peer, 5);
        assert!(r.game.generation > generation);
        assert!(r.game.rx.is_empty());
        r.out.clear();
        r.test_message(peer, motion(3, 99.), 6);
        assert!(
            !r.out
                .iter()
                .any(|o| matches!(o, Output::Inject(InjectEvent::MoveRelative { .. })))
        );
    }

    #[test]
    fn remote_game_announcement_survives_connected_and_resets_on_loss() {
        let me = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let mut r = Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: me,
                    nonce: [me.0[0]; 16],
                },
                "local",
                Os::Other,
            ),
            crate::router::connection_tests::session_layout(me, peer),
        );
        r.test_message(
            peer,
            Message::Control(ControlMsg::FocusState {
                lock_state: None,
                epoch: 3,
                holder: peer,
                scope: Some(FocusScope {
                    members: vec![(me, r.routing_session()), (peer, [1; 16])],
                }),
                session: Some([1; 16]),
            }),
            1,
        );
        r.test_message(
            peer,
            Message::Control(ControlMsg::GameMode {
                epoch: 3,
                generation: 7,
                mode: GameMode::On,
                active: true,
                detected: true,
                cursor_hiding_cause_available: Some(true),
            }),
            2,
        );
        let game = r.game_mode();
        r.test_connect(peer, 3);
        assert_eq!(r.game_mode(), game);
        assert!(r.game_mode().active);
        r.test_disconnect(peer, 4);
        assert_eq!(r.holder, me);
        assert!(!r.game_mode().active);
        assert_eq!(r.game_mode().mode, GameMode::Auto);
    }
}
