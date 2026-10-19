//! A terminal record is emitted once, after any submitted original-UP receipt.
//! These fields contain classifications only; device IDs stay in the envelope.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DndSourceEffect {
    Hold,
    Anchor,
    Resume,
    Finish,
    Release,
    CancelReposition,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeObservation {
    Observed,
    HoldRequested,
    Held,
    Resumed,
    CancelRepositioning,
    ReleaseSubmitted,
    EndedAccepted,
    EndedRejected,
    CatcherLost,
    Exported,
    Unconfirmed(DndSourceEffect),
    /// Explicit cancel/release followed by destination exit, without observing
    /// native completion. This is distinct from an unexpected catcher loss.
    CancelRetired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RolePartition {
    AllSame,
    SourceInput,
    SourceTarget,
    InputTarget,
    Distinct,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhysicalObservation {
    Down,
    Up,
    Unobserved,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntegrityOutcome {
    Completed,
    Cancelled,
    CatcherLost,
    HoldRejected,
    EffectUnconfirmed,
    OriginalRouteReleased,
    ExportFailed,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DndIntegrity {
    pub roles: RolePartition,
    pub source_os: crate::Os,
    pub input_os: crate::Os,
    pub target_os: crate::Os,
    pub holder_epoch: u64,
    pub native: NativeObservation,
    pub physical: PhysicalObservation,
    pub disposition: SourceButtonDisposition,
    pub route: source::Phase,
    pub outcome: IntegrityOutcome,
}
impl Router {
    pub(super) fn dnd_integrity_end(&mut self, offer: OfferId, outcome: IntegrityOutcome) {
        let Some(source) = self
            .dnd
            .sources
            .get_mut(&offer)
            .filter(|s| s.ever_held && !s.integrity_emitted)
        else {
            return;
        };
        source.integrity_emitted = true;
        let (input, target, epoch, native, route) = (
            source.input,
            source.holder.device,
            source.holder.epoch,
            source.native,
            source.phase,
        );
        let me = self.me();
        let os = |device| {
            if device == me {
                self.cfg.os
            } else {
                self.peers.get(&device).map_or(crate::Os::Other, |p| p.os)
            }
        };
        let press = self.dnd.presses.get(&offer);
        let record = DndIntegrity {
            roles: if input == me && target == me {
                RolePartition::AllSame
            } else if input == me {
                RolePartition::SourceInput
            } else if target == me {
                RolePartition::SourceTarget
            } else if input == target {
                RolePartition::InputTarget
            } else {
                RolePartition::Distinct
            },
            source_os: self.cfg.os,
            input_os: os(input),
            target_os: os(target),
            holder_epoch: epoch,
            native,
            route,
            outcome,
            physical: press.map_or(PhysicalObservation::Unobserved, |p| {
                if p.up_seen() {
                    PhysicalObservation::Up
                } else {
                    PhysicalObservation::Down
                }
            }),
            disposition: press.map_or(SourceButtonDisposition::Unconfirmed, |p| p.disposition()),
        };
        let returned = press.is_some_and(|p| matches!(p.owner, press::Owner::Ordinary { .. }));
        if press.is_some_and(|p| matches!(p.owner, press::Owner::Submitted(_))) {
            self.dnd.pending_integrity.insert(offer, record);
        } else {
            self.out.push(Output::Dnd(DndCmd::Integrity { id: offer.clip, record }));
        }
        if returned {
            // The ordinary ledger now owns this DOWN. Once this capture is
            // retired its observation must not compete with a subsequent
            // capture of the still-live native drag for the one physical UP.
            self.dnd.presses.remove(&offer);
        }
    }
    pub(super) fn dnd_integrity_receipt(&mut self, offer: OfferId) {
        if let Some(mut record) = self.dnd.pending_integrity.remove(&offer) {
            if let Some(press) = self.dnd.presses.get(&offer) {
                record.disposition = press.disposition();
                record.physical = if press.up_seen() {
                    PhysicalObservation::Up
                } else {
                    PhysicalObservation::Down
                };
            }
            self.out.push(Output::Dnd(DndCmd::Integrity { id: offer.clip, record }));
        }
    }
}
