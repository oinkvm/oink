//! Content-source authority survives host changes, but never source-session loss.
//! Native closure and receipt of the complete target batch are separate facts.
use super::NativeObservation;
use crate::dnd::*;
use oink_foundation::DeviceId;
use oink_foundation::clipboard::ClipData;
use oink_foundation::content::Holder;
use oink_foundation::content::OfferId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Local,
    Held,
    Released,
    Committing,
    Receiving,
    Cancelling,
    Closed,
}

#[derive(Clone, Debug)]
pub(super) struct Source {
    pub offer: OfferId,
    pub input: DeviceId,
    pub holder: Holder,
    pub phase: Phase,
    pub exported: bool,
    pub confirmed: bool,
    pub native: NativeObservation,
    pub ever_held: bool,
    pub integrity_emitted: bool,
    pub revision: u64,
    pub metadata_revision: u64,
    pub items: Vec<ClipData>,
    pub origin: Option<crate::layout::ScreenPos>,
    pub exit: Option<oink_foundation::Point>,
    /// Drop-phase demand can outrun ReleaseIntent on the bulk stream. It is
    /// remembered as intent only; it never grants release authority by itself.
    demand: bool,
    native_closed: bool,
    stored: bool,
}

impl Source {
    pub fn new(offer: OfferId, input: DeviceId, holder: Holder) -> Self {
        Self {
            offer,
            input,
            holder,
            phase: Phase::Local,
            exported: false,
            confirmed: false,
            native: NativeObservation::Observed,
            ever_held: false,
            integrity_emitted: false,
            revision: 1,
            metadata_revision: 1,
            items: Vec::new(),
            origin: None,
            exit: None,
            demand: false,
            native_closed: false,
            stored: false,
        }
    }
    pub fn key(&self) -> DragKey {
        DragKey {
            offer: self.offer,
            native_generation: self.offer.clip.seq,
        }
    }
    pub fn mode(&self) -> SourceMode {
        if self.exported {
            SourceMode::Exported
        } else {
            SourceMode::Held
        }
    }
    pub fn advance(&mut self) {
        self.revision = self.revision.checked_add(1).expect("drag state revision exhausted");
    }
    pub fn metadata(&mut self, items: Vec<ClipData>) {
        if self.items != items {
            self.items = items;
            self.metadata_revision = self
                .metadata_revision
                .checked_add(1)
                .expect("drag metadata revision exhausted");
            self.advance();
        }
    }
    pub fn notice(&self, state: DragState, preview: PreviewAvailability) -> DragNotice {
        let items = if state == DragState::Announced {
            self.items
                .iter()
                .filter(|i| matches!(i, ClipData::FileRoots(_)))
                .cloned()
                .collect()
        } else if matches!(state, DragState::Offered { .. }) {
            self.items.clone()
        } else {
            Vec::new()
        };
        let pending = self
            .items
            .iter()
            .any(|i| matches!(i, ClipData::Native { item, .. } if item.file.is_some()))
            && !self.items.iter().any(|i| matches!(i, ClipData::FileRoots(_)));
        DragNotice {
            key: self.key(),
            revision: self.revision,
            input: self.input,
            state,
            metadata: MetadataVersion {
                revision: self.metadata_revision,
                readiness: if pending {
                    MetadataReadiness::Pending
                } else {
                    MetadataReadiness::Ready
                },
            },
            preview,
            retained_inventory: None,
            items,
        }
    }
    pub fn matches(&self, offer: OfferId, holder: Holder, from: DeviceId) -> bool {
        self.offer == offer && self.holder == holder && holder.device == from && self.phase != Phase::Closed
    }
    /// Only the current holder may pass authority onward, with an increasing
    /// focus epoch. An already-released drag can never be returned or rehosted.
    pub fn handoff(&mut self, from: Holder, to: Holder) -> bool {
        if self.holder != from || to.epoch <= from.epoch || !matches!(self.phase, Phase::Local | Phase::Held) {
            return false;
        }
        self.holder = to;
        self.advance();
        self.phase = if to.device == self.offer.clip.origin {
            Phase::Local
        } else {
            Phase::Held
        };
        self.demand = false;
        true
    }
    /// Return true exactly once when this event authorizes source native Drop.
    pub fn released(&mut self) -> bool {
        if self.phase != Phase::Held && !(self.exported && self.phase == Phase::Local) {
            return false;
        }
        self.phase = Phase::Released;
        self.advance();
        self.maybe_commit()
    }
    pub fn demanded(&mut self) -> bool {
        if !matches!(self.phase, Phase::Held | Phase::Released) && !(self.exported && self.phase == Phase::Local) {
            return false;
        }
        self.demand = true;
        self.maybe_commit()
    }
    fn maybe_commit(&mut self) -> bool {
        if self.phase == Phase::Released && self.demand {
            self.phase = if self.exported {
                Phase::Receiving
            } else {
                Phase::Committing
            };
            true
        } else {
            false
        }
    }
    pub fn cancel(&mut self) -> bool {
        if matches!(self.phase, Phase::Closed | Phase::Cancelling) {
            return false;
        }
        self.advance();
        self.phase = if self.native_closed {
            Phase::Closed
        } else {
            Phase::Cancelling
        };
        !self.native_closed
    }
    pub fn catcher_lost(&mut self) {
        self.native = NativeObservation::CatcherLost;
        self.advance();
        self.phase = Phase::Closed;
    }
    pub fn cancel_retired(&mut self) {
        self.native = NativeObservation::CancelRetired;
        self.advance();
        self.phase = Phase::Closed;
        // We retired the cancelled operation, not observed native completion.
    }
    pub fn native_ended(&mut self, accepted: bool) {
        self.native = if accepted {
            NativeObservation::EndedAccepted
        } else {
            NativeObservation::EndedRejected
        };
        self.native_closed = true;
        self.advance();
        self.phase = if accepted && self.phase == Phase::Committing && !self.stored {
            Phase::Receiving
        } else {
            Phase::Closed
        };
    }
    pub fn export_ready(&mut self) -> bool {
        if self.phase != Phase::Local || self.native_closed {
            return false;
        }
        self.exported = true;
        self.native = NativeObservation::Exported;
        self.confirmed = true;
        self.advance();
        self.native_closed = true;
        true
    }
    /// Record durable target storage independently of native completion. A
    /// still-running native Drop may need its provider to finish snapshotting;
    /// terminal publication and provider cleanup must wait for both facts.
    pub fn target_stored(&mut self) -> bool {
        if !matches!(self.phase, Phase::Committing | Phase::Receiving) || self.stored {
            return false;
        }
        self.stored = true;
        if self.native_closed {
            self.phase = Phase::Closed;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::content::ClipId;
    fn source() -> Source {
        let origin = DeviceId([1; 32]);
        let mut s = Source::new(
            OfferId {
                clip: ClipId { origin, seq: 1 },
                generation: 1,
            },
            origin,
            Holder {
                device: origin,
                epoch: 1,
            },
        );
        assert!(s.handoff(
            s.holder,
            Holder {
                device: DeviceId([2; 32]),
                epoch: 2
            }
        ));
        s
    }
    #[test]
    fn drop_demand_and_release_commute_and_commit_once() {
        for demand_first in [false, true] {
            let mut s = source();
            if demand_first {
                assert!(!s.demanded());
                assert!(s.released());
            } else {
                assert!(!s.released());
                assert!(s.demanded());
            }
            assert_eq!(s.phase, Phase::Committing);
            assert!(!s.released());
            assert!(!s.demanded());
            assert!(!s.handoff(
                s.holder,
                Holder {
                    device: s.offer.clip.origin,
                    epoch: 3
                }
            ));
        }
    }
    #[test]
    fn native_end_is_not_target_stored_and_late_receipts_cannot_revive_cancel() {
        let mut s = source();
        assert!(!s.target_stored());
        s.released();
        s.demanded();
        s.native_ended(true);
        assert_eq!(s.phase, Phase::Receiving);
        assert!(s.target_stored());
        assert!(!s.target_stored());
        assert_eq!(s.phase, Phase::Closed);
        let mut s = source();
        s.demanded();
        assert!(s.cancel());
        assert!(!s.released());
        s.native_ended(true);
        assert_eq!(s.phase, Phase::Closed);
        assert!(!s.target_stored());
    }
    #[test]
    fn exported_copy_returns_as_a_host_and_receipt_ends_its_source_obligation() {
        let mut s = source();
        let remote = s.holder;
        assert!(s.handoff(
            remote,
            Holder {
                device: s.offer.clip.origin,
                epoch: 3
            }
        ));
        assert!(s.export_ready());
        assert!(!s.export_ready());
        assert!(s.handoff(
            s.holder,
            Holder {
                device: remote.device,
                epoch: 4
            }
        ));
        assert!(s.handoff(
            s.holder,
            Holder {
                device: s.offer.clip.origin,
                epoch: 5
            }
        ));
        assert!(!s.released());
        assert!(s.demanded());
        assert_eq!(s.phase, Phase::Receiving, "there is no second native source Drop");
        assert!(s.target_stored());
        assert_eq!(s.phase, Phase::Closed);
    }
    #[test]
    fn return_reuses_source_and_rejects_old_holder_generations() {
        let mut s = source();
        let old = s.holder;
        let local = Holder {
            device: s.offer.clip.origin,
            epoch: 3,
        };
        assert!(s.handoff(old, local));
        assert_eq!(s.phase, Phase::Local);
        assert!(!s.handoff(
            old,
            Holder {
                device: DeviceId([3; 32]),
                epoch: 4
            }
        ));
        assert!(!s.matches(s.offer, old, old.device));
        let mut stale = s.offer;
        stale.generation += 1;
        assert!(!s.matches(stale, local, local.device));
        assert!(!s.released());
        assert!(s.handoff(
            local,
            Holder {
                device: DeviceId([3; 32]),
                epoch: 4
            }
        ));
        assert!(s.cancel());
        assert!(!s.cancel());
    }
}
