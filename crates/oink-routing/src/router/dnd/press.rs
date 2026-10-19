//! The one original DOWN debt. Tokens cannot be cloned: transfer into native
//! submission or back to the ordinary ledger consumes the withheld token.
use super::*;

#[derive(Debug)]
pub(super) struct Withheld {
    physical_up: bool,
}
#[derive(Debug)]
pub(super) struct Submitted {
    physical_up: bool,
}
#[derive(Debug)]
pub(super) enum Owner {
    Ordinary { physical_up: bool },
    Withheld(Withheld),
    Submitted(Submitted),
    Settled { physical_up: bool },
    Unconfirmed { physical_up: bool },
}
#[derive(Debug)]
pub(super) struct OriginalPress {
    pub key: DragKey,
    pub input: DeviceId,
    pub revision: u64,
    pub owner: Owner,
    // The old input stream can disappear before its physical UP arrives. A
    // submitted native release may still await a receipt, but must never claim
    // an UP from the next stream/session's click.
    input_lost: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Return {
    Ordinary,
    Submit,
    None,
}
impl OriginalPress {
    pub fn new(key: DragKey, input: DeviceId) -> Self {
        Self {
            key,
            input,
            revision: 1,
            owner: Owner::Ordinary { physical_up: false },
            input_lost: false,
        }
    }
    pub fn withhold(&mut self) -> bool {
        if !matches!(self.owner, Owner::Ordinary { .. }) {
            return false;
        }
        self.revision += 1;
        self.owner = Owner::Withheld(Withheld { physical_up: false });
        true
    }
    /// Returns whether the normal ledger must consume this physical UP.
    pub fn physical_up(&mut self) -> bool {
        self.revision += 1;
        match &mut self.owner {
            Owner::Ordinary { physical_up } => {
                *physical_up = true;
                false
            }
            Owner::Withheld(p) => {
                p.physical_up = true;
                true
            }
            Owner::Submitted(p) => {
                p.physical_up = true;
                true
            }
            Owner::Settled { physical_up } | Owner::Unconfirmed { physical_up } => {
                *physical_up = true;
                true
            }
        }
    }
    pub fn submit(&mut self) -> bool {
        let Owner::Withheld(_) = self.owner else {
            return false;
        };
        let Owner::Withheld(p) = std::mem::replace(&mut self.owner, Owner::Ordinary { physical_up: false }) else {
            unreachable!()
        };
        self.revision += 1;
        self.owner = Owner::Submitted(Submitted {
            physical_up: p.physical_up,
        });
        true
    }
    pub fn receipt(&mut self, ok: bool) {
        if let Owner::Submitted(p) = &self.owner {
            self.revision += 1;
            let physical_up = p.physical_up;
            self.owner = if ok {
                Owner::Settled { physical_up }
            } else {
                Owner::Unconfirmed { physical_up }
            };
        }
    }
    pub fn return_route(&mut self) -> Return {
        match self.owner {
            Owner::Withheld(Withheld { physical_up: false }) => {
                self.revision += 1;
                self.owner = Owner::Ordinary { physical_up: false };
                Return::Ordinary
            }
            Owner::Withheld(Withheld { physical_up: true }) => Return::Submit,
            _ => Return::None,
        }
    }
    pub fn up_seen(&self) -> bool {
        match self.owner {
            Owner::Ordinary { physical_up } => physical_up,
            Owner::Withheld(ref p) => p.physical_up,
            Owner::Submitted(ref p) => p.physical_up,
            Owner::Settled { physical_up } | Owner::Unconfirmed { physical_up } => physical_up,
        }
    }
    pub fn retire_input_route(&mut self) {
        if matches!(
            self.owner,
            Owner::Submitted(_) | Owner::Settled { .. } | Owner::Unconfirmed { .. }
        ) {
            self.input_lost = true;
        }
    }
    pub fn retired(&self) -> bool {
        matches!(self.owner, Owner::Settled { .. } | Owner::Unconfirmed { .. }) && (self.up_seen() || self.input_lost)
    }
    pub fn disposition(&self) -> SourceButtonDisposition {
        match self.owner {
            Owner::Ordinary { .. } => SourceButtonDisposition::ReturnedToOriginalRoute,
            Owner::Withheld(_) => SourceButtonDisposition::OriginalSourceOwns,
            Owner::Submitted(_) => SourceButtonDisposition::ReleaseSubmitted,
            Owner::Settled { .. } => SourceButtonDisposition::Released,
            Owner::Unconfirmed { .. } => SourceButtonDisposition::Unconfirmed,
        }
    }
}
impl Router {
    pub(super) fn dnd_take_source_press(&mut self, offer: OfferId, input: DeviceId) {
        let Some(source) = self.dnd.sources.get(&offer) else {
            return;
        };
        let press = self
            .dnd
            .presses
            .entry(offer)
            .or_insert_with(|| OriginalPress::new(source.key(), input));
        if press.withhold() && input != self.me() {
            self.release_button_silently(input, MouseButton::Left);
        }
        self.dnd_button_notice(offer);
    }
    pub(super) fn dnd_button_notice(&mut self, offer: OfferId) {
        if let Some(press) = self.dnd.presses.get(&offer) {
            let input = press.input;
            let message = DndMsg::SourceButton {
                key: press.key,
                revision: press.revision,
                disposition: press.disposition(),
            };
            if input != self.me() {
                self.send(input, Message::Dnd(message));
            }
        }
    }
    pub(super) fn dnd_return_source_press(&mut self, offer: OfferId) {
        let Some(press) = self.dnd.presses.get_mut(&offer) else {
            return;
        };
        let input = press.input;
        match press.return_route() {
            Return::Ordinary => {
                if input == self.me() {
                    self.buttons.insert(MouseButton::Left, Target::Local);
                    self.button_diagnostics
                        .press(input, MouseButton::Left, self.dnd.now, None);
                } else {
                    self.hold_button_silently(input, MouseButton::Left);
                }
            }
            Return::Submit => self.dnd_release_source_button(offer),
            Return::None => {}
        }
        self.dnd_button_notice(offer);
    }
    pub(super) fn dnd_release_source_button(&mut self, offer: OfferId) {
        let Some(press) = self.dnd.presses.get_mut(&offer) else {
            return;
        };
        if press.submit() {
            let physical = press.input == self.cfg.device;
            self.out.push(Output::Dnd(DndCmd::ReleaseSourceButton {
                id: offer.clip,
                physical,
            }));
            self.dnd_button_notice(offer);
        }
    }
    pub(super) fn dnd_submit_source_finish(
        &mut self,
        offer: OfferId,
        accept: bool,
        cancel_geometry: Option<oink_foundation::dnd::CancelGeometry>,
    ) {
        let cancel_at = cancel_geometry.as_ref().and_then(|g| g.requested());
        let Some(press) = self.dnd.presses.get_mut(&offer) else {
            return;
        };
        if press.submit() {
            let physical = press.input == self.cfg.device;
            if let Some(source) = self.dnd.sources.get_mut(&offer) {
                source.native = if !accept && cancel_at.is_some() {
                    NativeObservation::CancelRepositioning
                } else {
                    NativeObservation::ReleaseSubmitted
                };
            }
            self.out.push(Output::Dnd(DndCmd::FinishSource {
                id: offer.clip,
                epoch: self.dnd.sources.get(&offer).map_or(self.epoch, |s| s.holder.epoch),
                accept,
                cancel_at,
                cancel_geometry,
                physical,
            }));
            self.dnd_button_notice(offer);
        }
    }
    /// Called at S before ordinary UP processing. I's original recorded route
    /// remains I→S throughout all holder changes; H/T never forward that UP.
    pub(in crate::router) fn dnd_original_up(&mut self, input: DeviceId) -> bool {
        let Some(offer) = self
            .dnd
            .presses
            .iter()
            .find_map(|(offer, p)| (p.input == input && !p.up_seen() && !p.input_lost).then_some(*offer))
        else {
            return false;
        };
        let press = self.dnd.presses.get_mut(&offer).unwrap();
        if !press.physical_up() {
            self.dnd_integrity_end(offer, IntegrityOutcome::OriginalRouteReleased);
            self.dnd.presses.remove(&offer);
            if let Some(source) = self.dnd.sources.get_mut(&offer) {
                source.advance();
            }
            self.dnd_publish_state(
                offer,
                DragState::Revoked {
                    scope: RevokeScope::Drag,
                    reason: RevokeReason::NativeEnded,
                },
            );
            self.dnd.sources.remove(&offer);
            if self.dnd.native_source == Some(offer) {
                self.dnd.native_source = None;
            }
            self.out.push(Output::Dnd(DndCmd::ForgetSource { id: offer.clip }));
            self.out.push(Output::Dnd(DndCmd::ReleaseData { id: offer.clip }));
            return false;
        }
        let mut commit = false;
        let mut released = None;
        if let Some(source) = self.dnd.sources.get_mut(&offer) {
            if source.phase == source::Phase::Local && !source.exported {
                if let State::Leaving { released, .. } = &mut self.dnd.state {
                    *released = true;
                }
            } else if source.phase == source::Phase::Held || (source.exported && source.phase == source::Phase::Local) {
                let holder = source.holder;
                let mode = source.mode();
                commit = source.released();
                released = Some(DragState::Released { holder, mode });
            }
        }
        if let Some(state) = released {
            self.dnd_publish_state(offer, state);
        }
        if commit {
            self.dnd_submit_source_finish(offer, true, None);
        }
        if self.dnd.presses.get(&offer).is_some_and(OriginalPress::retired) {
            self.dnd.presses.remove(&offer);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_loss_retires_submitted_receipts_without_inventing_physical_up() {
        let mut p = press();
        assert!(p.withhold());
        p.retire_input_route();
        assert!(!p.input_lost, "an unsent native UP remains owed");
        assert!(p.submit());
        p.retire_input_route();
        assert!(p.input_lost);
        assert!(!p.up_seen());
        assert!(!p.retired(), "keep the native submission receipt");
        p.receipt(true);
        assert!(p.retired());
        assert!(!p.up_seen(), "stream loss does not prove physical release");
    }

    fn press() -> OriginalPress {
        let id = ClipId {
            origin: DeviceId([1; 32]),
            seq: 1,
        };
        OriginalPress::new(
            DragKey {
                offer: OfferId {
                    clip: id,
                    generation: 1,
                },
                native_generation: 1,
            },
            DeviceId([2; 32]),
        )
    }
    #[test]
    fn every_exit_consumes_one_token_and_up_order_does_not_duplicate_submission() {
        for up_first in [false, true] {
            let mut p = press();
            assert!(p.withhold());
            assert!(!p.withhold());
            if up_first {
                assert!(p.physical_up());
            }
            assert!(p.submit());
            assert!(!p.submit());
            assert_eq!(p.return_route(), Return::None);
            p.receipt(true);
            if !up_first {
                assert!(p.physical_up());
            }
            assert!(p.retired());
            assert!(!p.submit());
        }
        let mut p = press();
        p.withhold();
        assert_eq!(p.return_route(), Return::Ordinary);
        assert!(!p.physical_up());
        assert!(!p.submit());
        let mut p = press();
        p.withhold();
        p.physical_up();
        assert_eq!(p.return_route(), Return::Submit);
        assert!(p.submit());
        p.receipt(false);
        assert_eq!(p.disposition(), SourceButtonDisposition::Unconfirmed);
        assert!(!p.submit());
        assert!(
            p.retired(),
            "unconfirmed disposal is recorded, but cannot consume the next press's UP"
        );
    }
}
