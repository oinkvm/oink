//! Text-input focus for the phones' keyboard (protocol §14.7; behaviour in
//! the control-page spec §9: a text field focused on the focus computer pops
//! the phone keyboard up, leaving it hides a keyboard that popped up that way,
//! a password field gets the Oink keyboard).
//!
//! Holder side (a desktop): the platform watcher reports `TextFocusState`:
//! `seq` counts keyboard focuses into an editable element (input-focus
//! episodes), `editable` / `secure` describe the focused element now. Every
//! change bumps `revision`. While this device holds the group focus, each
//! change goes at once to every connected peer that is not a desktop
//! (`ControlMsg::TextFocus`, leaving a field included), the current state goes
//! with the game state when focus arrives here or a phone attaches, and it is
//! repeated on the 2 s heartbeat, so a lost record is recovered. Nothing about
//! the element (text, label, application) leaves the device.
//!
//! Member side (a phone's screenless router): a record counts only when it
//! comes from the current holder in the current focus epoch, and only if its
//! revision is newer than the last one from that holder instance (heartbeat
//! copies and records overtaken on the stream are dropped). The same revision
//! counts again after focus went to another device and came back, so the page
//! always has the current holder's state. Pop-up / hide decisions (episodes,
//! manual hiding, keyboard kind) are the control page's.
use super::*;
use crate::control::TextFocusState;

/// This device's own text-input focus as last reported by its watcher.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct LocalTextFocus {
    state: TextFocusState,
    /// Increases on every change of `state`; 0 = never reported.
    revision: u64,
}

impl Router {
    /// Holder side: the platform watcher's latest observation.
    pub fn set_text_focus(&mut self, state: TextFocusState) {
        if self.cfg.screenless || (self.text_focus.revision != 0 && self.text_focus.state == state) {
            return;
        }
        self.text_focus = LocalTextFocus {
            state,
            revision: self.text_focus.revision + 1,
        };
        let peers: Vec<DeviceId> = self
            .peers
            .iter()
            .filter(|(id, peer)| peer.connected && Self::text_focus_peer(peer) && **id != self.me())
            .map(|(id, _)| *id)
            .collect();
        let Some(message) = self.text_focus_message() else {
            tracing::debug!(?state, "text focus changed (not the focus holder)");
            return;
        };
        tracing::info!(
            seq = state.seq,
            editable = state.editable,
            secure = state.secure,
            "text focus changed, telling phone sources"
        );
        for peer in peers {
            self.send(peer, message.clone());
        }
    }

    /// The record this device sends while it holds the group focus.
    fn text_focus_message(&self) -> Option<Message> {
        let me = self.me();
        let focus = self.text_focus;
        (focus.revision != 0 && self.holder == me && self.claim == me && !self.paused).then_some(Message::Control(
            ControlMsg::TextFocus {
                epoch: self.epoch,
                seq: focus.state.seq,
                editable: focus.state.editable,
                secure: focus.state.secure,
                revision: focus.revision,
            },
        ))
    }

    /// Only phones act on text focus; desktops are left out to keep their
    /// streams unchanged. A peer whose system is not known yet is included
    /// (it ignores the record if it is not screenless).
    fn text_focus_peer(peer: &Peer) -> bool {
        !matches!(peer.os, Os::MacOS | Os::Windows)
    }

    /// The current record for `peer` (heartbeat, focus arrival, attach).
    pub(super) fn text_focus_record(&self, peer: DeviceId) -> Option<Message> {
        self.peers
            .get(&peer)
            .is_some_and(Self::text_focus_peer)
            .then(|| self.text_focus_message())
            .flatten()
    }

    /// Member side: a holder's record (see the module comment).
    pub(super) fn receive_text_focus(&mut self, from: DeviceId, epoch: u64, revision: u64, state: TextFocusState) {
        if !self.cfg.screenless || from == self.me() || from != self.holder || epoch != self.epoch {
            return;
        }
        let instance = self.peers.get(&from).and_then(|peer| peer.session);
        let last_from = self.text_focus_last.map(|(device, _)| device);
        if let Some((seen_instance, seen)) = self.text_focus_revisions.get(&from)
            && *seen_instance == instance
            && (revision < *seen || (revision == *seen && last_from == Some(from)))
        {
            return;
        }
        self.text_focus_revisions.insert(from, (instance, revision));
        self.text_focus_last = Some((from, state));
        tracing::info!(
            ?from,
            seq = state.seq,
            editable = state.editable,
            secure = state.secure,
            revision,
            "text focus: focus holder's text-input focus"
        );
        self.out
            .push(Output::Status(StatusEvent::TextFocus { device: from, state }));
    }

    /// Member side: the last text-input focus reported by the device this
    /// member's input goes to, for a control page that opens now.
    pub fn member_text_focus(&self) -> Option<(DeviceId, TextFocusState)> {
        let target = self.member_target()?;
        self.text_focus_last.filter(|(device, _)| *device == target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PC: DeviceId = DeviceId([1; 32]);
    const MAC: DeviceId = DeviceId([2; 32]);
    const PHONE: DeviceId = DeviceId([9; 32]);

    fn boot(device: DeviceId) -> oink_foundation::session::Boot {
        oink_foundation::session::Boot {
            device_id: device,
            nonce: [device.0[0]; 16],
        }
    }

    fn field(seq: u64, editable: bool, secure: bool) -> TextFocusState {
        TextFocusState { seq, editable, secure }
    }

    /// A desktop that holds the group focus at epoch 3, with a phone and a
    /// Mac attached.
    fn holder() -> Router {
        let mut router = Router::new(
            RouterConfig::new(boot(PC), "pc", Os::Windows),
            Arc::new(crate::SimpleLayout::new()),
        );
        router.session = [1; 16];
        for peer in [PHONE, MAC] {
            assert!(router.attach_input(boot(peer), [peer.0[0]; 16], true, true, FeelProfile::default(), 0));
        }
        router.set_peer_os(MAC, Os::MacOS, 0);
        router.set_peer_os(PHONE, Os::Android, 0);
        router.epoch = 3;
        router.holder = PC;
        router.claim = PC;
        router.drain_outputs();
        router
    }

    /// A phone whose input goes to PC at epoch 3; MAC is attached too.
    fn phone() -> Router {
        let mut config = RouterConfig::new(boot(PHONE), "phone", Os::Android);
        config.screenless = true;
        let mut router = Router::new(config, Arc::new(crate::SimpleLayout::new()));
        router.session = [9; 16];
        assert!(router.attach_input(boot(PC), [1; 16], true, true, FeelProfile::default(), 0));
        assert!(router.attach_input(boot(MAC), [2; 16], true, true, FeelProfile::default(), 0));
        for peer in [PC, MAC] {
            router.peers.get_mut(&peer).unwrap().focus_ready = true;
        }
        router.epoch = 3;
        router.holder = PC;
        router.claim = PC;
        router.drain_outputs();
        router
    }

    fn sent(router: &mut Router) -> Vec<(DeviceId, ControlMsg)> {
        router
            .drain_outputs()
            .into_iter()
            .filter_map(|output| match output {
                Output::Send {
                    to,
                    msg: Message::Control(msg @ ControlMsg::TextFocus { .. }),
                    ..
                } => Some((to, msg)),
                _ => None,
            })
            .collect()
    }

    fn record(epoch: u64, revision: u64, state: TextFocusState) -> Message {
        Message::Control(ControlMsg::TextFocus {
            epoch,
            seq: state.seq,
            editable: state.editable,
            secure: state.secure,
            revision,
        })
    }

    /// The phone router's reports for one delivered record.
    fn reports(router: &mut Router, from: DeviceId, message: Message) -> Vec<(DeviceId, TextFocusState)> {
        router.input_message(from, message, 1);
        router
            .drain_outputs()
            .into_iter()
            .filter_map(|output| match output {
                Output::Status(StatusEvent::TextFocus { device, state }) => Some((device, state)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_change_goes_to_phones_at_once_and_not_to_desktops() {
        let mut pc = holder();
        pc.set_text_focus(field(1, true, false));
        assert_eq!(
            sent(&mut pc),
            [(
                PHONE,
                ControlMsg::TextFocus {
                    epoch: 3,
                    seq: 1,
                    editable: true,
                    secure: false,
                    revision: 1
                }
            )]
        );
        // Leaving the field goes at once too (the phone hides a popped-up keyboard).
        pc.set_text_focus(field(1, false, false));
        assert_eq!(sent(&mut pc).len(), 1);
        // A password field.
        pc.set_text_focus(field(2, true, true));
        assert_eq!(
            sent(&mut pc),
            [(
                PHONE,
                ControlMsg::TextFocus {
                    epoch: 3,
                    seq: 2,
                    editable: true,
                    secure: true,
                    revision: 3
                }
            )]
        );
        // The same report again changes nothing.
        pc.set_text_focus(field(2, true, true));
        assert!(sent(&mut pc).is_empty());
    }

    #[test]
    fn the_first_report_is_sent_even_without_a_field() {
        let mut pc = holder();
        assert!(pc.text_focus_record(PHONE).is_none(), "nothing reported yet");
        pc.set_text_focus(TextFocusState::default());
        assert_eq!(sent(&mut pc).len(), 1);
        assert_eq!(
            pc.text_focus_record(PHONE),
            Some(record(3, 1, TextFocusState::default()))
        );
    }

    #[test]
    fn heartbeat_and_game_state_carry_the_current_state_to_phones_only() {
        let mut pc = holder();
        pc.set_text_focus(field(2, true, false));
        pc.set_text_focus(field(2, false, false));
        pc.drain_outputs();
        assert_eq!(pc.text_focus_record(PHONE), Some(record(3, 2, field(2, false, false))));
        assert!(pc.text_focus_record(MAC).is_none());
        assert!(
            pc.heartbeat_messages(PHONE)
                .contains(&record(3, 2, field(2, false, false)))
        );
        assert!(
            !pc.heartbeat_messages(MAC)
                .iter()
                .any(|m| matches!(m, Message::Control(ControlMsg::TextFocus { .. })))
        );
        // Focus arriving here (or a phone attaching) sends the game state; the
        // text-input focus follows it.
        pc.send_game_state(PHONE);
        pc.send_game_state(MAC);
        assert_eq!(
            sent(&mut pc),
            [(
                PHONE,
                ControlMsg::TextFocus {
                    epoch: 3,
                    seq: 2,
                    editable: false,
                    secure: false,
                    revision: 2
                }
            )]
        );
    }

    #[test]
    fn a_device_that_is_not_the_holder_or_is_paused_sends_nothing() {
        let mut pc = holder();
        pc.holder = MAC;
        pc.claim = MAC;
        pc.set_text_focus(field(1, true, false));
        assert!(sent(&mut pc).is_empty());
        assert!(pc.text_focus_record(PHONE).is_none());
        // Focus comes here later: the current state (with the current epoch) is sent.
        pc.epoch = 4;
        pc.holder = PC;
        pc.claim = PC;
        assert_eq!(pc.text_focus_record(PHONE), Some(record(4, 1, field(1, true, false))));
        pc.set_paused(true);
        pc.drain_outputs();
        pc.set_text_focus(field(2, true, false));
        assert!(sent(&mut pc).is_empty());
        assert!(pc.text_focus_record(PHONE).is_none());
    }

    #[test]
    fn the_phone_reports_each_change_once() {
        let mut phone = phone();
        assert_eq!(
            reports(&mut phone, PC, record(3, 1, field(1, true, false))),
            [(PC, field(1, true, false))]
        );
        // Heartbeat copies: nothing.
        assert!(reports(&mut phone, PC, record(3, 1, field(1, true, false))).is_empty());
        // Leaving the field.
        assert_eq!(
            reports(&mut phone, PC, record(3, 2, field(1, false, false))),
            [(PC, field(1, false, false))]
        );
        // An older record overtaken on the stream (a heartbeat copy) is dropped.
        assert!(reports(&mut phone, PC, record(3, 1, field(1, true, false))).is_empty());
        // A password field.
        assert_eq!(
            reports(&mut phone, PC, record(3, 3, field(2, true, true))),
            [(PC, field(2, true, true))]
        );
        assert_eq!(phone.member_text_focus(), Some((PC, field(2, true, true))));
    }

    #[test]
    fn records_from_another_device_or_another_epoch_are_ignored_until_they_count() {
        let mut phone = phone();
        assert!(
            reports(&mut phone, MAC, record(3, 1, field(1, true, false))).is_empty(),
            "not the holder"
        );
        assert!(
            reports(&mut phone, PC, record(2, 1, field(1, true, false))).is_empty(),
            "another epoch"
        );
        // The phone learns epoch 4 later; the heartbeat's copy then counts.
        phone.epoch = 4;
        assert!(reports(&mut phone, PC, record(3, 1, field(1, true, false))).is_empty());
        assert_eq!(
            reports(&mut phone, PC, record(4, 1, field(1, true, false))),
            [(PC, field(1, true, false))]
        );
    }

    #[test]
    fn focus_moving_away_and_back_reports_the_holders_state_again() {
        let mut phone = phone();
        reports(&mut phone, PC, record(3, 5, field(2, true, false)));
        // Focus moves to MAC (epoch 4), which reports no field.
        phone.epoch = 4;
        phone.holder = MAC;
        assert_eq!(phone.member_text_focus(), None);
        assert_eq!(
            reports(&mut phone, MAC, record(4, 1, TextFocusState::default())),
            [(MAC, TextFocusState::default())]
        );
        // Back to PC (epoch 5): the same revision counts again.
        phone.epoch = 5;
        phone.holder = PC;
        assert_eq!(
            reports(&mut phone, PC, record(5, 5, field(2, true, false))),
            [(PC, field(2, true, false))]
        );
    }

    #[test]
    fn a_restarted_holder_starts_its_revisions_again() {
        let mut phone = phone();
        reports(&mut phone, PC, record(3, 9, field(4, true, false)));
        // PC restarted: a new router instance counting from 1.
        phone.peers.get_mut(&PC).unwrap().session = Some([7; 16]);
        assert_eq!(
            reports(&mut phone, PC, record(3, 1, TextFocusState::default())),
            [(PC, TextFocusState::default())]
        );
    }

    #[test]
    fn a_desktop_never_reports_text_focus_status() {
        let mut pc = holder();
        pc.holder = MAC;
        assert!(reports(&mut pc, MAC, record(3, 1, field(1, true, false))).is_empty());
    }
}
