//! Clipboard copy ordering. Content is always an Inventory; payload operations
//! belong to the native content engine and never use positional item indices.
use oink_foundation::{DeviceId, clipboard::Sensitivity};
use oink_protocol::v1::{
    self, RawValue as V, dto,
    schema::{WireDto, WireValue},
};
use std::time::{Duration, Instant};

pub struct Ordering {
    device: DeviceId,
    clock: u64,
    current: Option<(DeviceId, dto::ClipboardOffer)>,
    // Withdrawing publishable content must not forget which copy won ordering.
    // An empty or fully filtered local copy still supersedes older peer offers.
    ordering_fence: Option<(u64, DeviceId)>,
    written: u64,
    pending: Option<(u64, Instant)>,
}
impl std::fmt::Debug for Ordering {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardOrdering")
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}
impl Ordering {
    pub fn new(device: DeviceId, clock: u64) -> Self {
        Self {
            device,
            clock,
            current: None,
            ordering_fence: None,
            written: 0,
            pending: None,
        }
    }
    pub fn clock(&self) -> u64 {
        self.clock
    }
    pub fn current(&self) -> Option<&dto::ClipboardOffer> {
        self.current.as_ref().map(|(_, offer)| offer)
    }
    pub fn source(&self) -> Option<DeviceId> {
        self.current.as_ref().map(|(peer, _)| *peer)
    }
    pub fn local_change(&mut self, count: u64, now: Instant) -> bool {
        if count <= self.written {
            return false;
        }
        self.pending = Some((count, now + Duration::from_millis(300)));
        self.current = None;
        true
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.map(|(_, deadline)| deadline)
    }
    pub fn capture_due(&mut self, now: Instant) -> bool {
        if self.pending.is_some_and(|(_, deadline)| now >= deadline) {
            self.pending = None;
            true
        } else {
            false
        }
    }
    pub fn revoke_local(&mut self) {
        if self.ordering_fence.is_some_and(|(_, origin)| origin == self.device) {
            self.ordering_fence = None;
        }
        if self.source() == Some(self.device) {
            self.current = None;
        }
        self.pending = None;
    }
    pub fn written(&mut self, count: u64) {
        self.written = count;
        self.pending = None;
    }
    pub fn forget(&mut self, peer: DeviceId) {
        if self.ordering_fence.is_some_and(|(_, origin)| origin == peer) {
            self.ordering_fence = None;
        }
        if self.source() == Some(peer) {
            self.current = None;
        }
    }
    pub fn capture(
        &mut self,
        mut inventory: dto::Inventory,
        count: u64,
        sensitivity: Sensitivity,
        time: u64,
    ) -> v1::Result<Option<dto::ClipboardOffer>> {
        if count <= self.written && count != 0 {
            return Ok(None);
        }
        if inventory.items()?.0.is_empty() && inventory.roots()?.0.is_empty() {
            self.current = None;
            self.pending = None;
            return Ok(None);
        }
        inventory = crate::wire::reliable_inventory(&inventory)?;
        let clock = self.clock.checked_add(1).ok_or(v1::ErrorCode::StateConflict)?;
        let id = crate::model::OfferId {
            clip: oink_foundation::content::ClipId {
                origin: self.device,
                seq: clock,
            },
            generation: clock,
        };
        let mut raw = inventory.into_raw();
        raw.insert(1, crate::wire::offer_id(id).write());
        inventory = dto::Inventory::from_raw(raw)?;
        let build = |inventory: &dto::Inventory| -> v1::Result<dto::ClipboardOffer> {
            let digest = blake3::hash(&inventory.encode(crate::model::INVENTORY_BYTES)?);
            dto::ClipboardOffer::from_raw(
                [
                    (1, crate::wire::offer_id(id).write()),
                    (2, V::Uint(count)),
                    (3, V::Uint(clock)),
                    (4, crate::wire::sensitivity(sensitivity).write()),
                    (5, crate::wire::digest(digest.into()).write()),
                    (6, inventory.write()),
                    (7, V::Uint(time)),
                ]
                .into(),
            )
        };
        let mut offer = build(&inventory)?;
        if inventory.inline_bytes()? > 0
            && v1::frame::encode(&v1::Message::ClipboardOffer(offer.clone()), v1::limits::RELIABLE_FRAME).is_err()
        {
            inventory = crate::wire::descriptor_inventory(&inventory)?;
            offer = build(&inventory)?;
        }
        self.clock = clock;
        self.ordering_fence = Some((clock, self.device));
        self.pending = None;
        self.current = Some((self.device, offer.clone()));
        Ok(Some(offer))
    }
    pub fn receive(&mut self, peer: DeviceId, offer: dto::ClipboardOffer) -> v1::Result<bool> {
        let id = crate::wire::local_offer_id(offer.id()?)?;
        if id.clip.origin != peer || peer == self.device {
            return Ok(false);
        }
        let inventory = offer.inventory()?;
        let digest = crate::wire::local_digest(offer.content_digest()?)?;
        if blake3::hash(&inventory.encode(crate::model::INVENTORY_BYTES)?).as_bytes() != &digest {
            return Err(v1::ErrorCode::StateConflict.into());
        }
        let clock = offer.clock()?;
        self.clock = self.clock.max(clock);
        if self.pending.is_some() {
            return Ok(false);
        }
        if self.ordering_fence.is_some_and(|old| (clock, id.clip.origin) <= old) {
            return Ok(false);
        }
        self.ordering_fence = Some((clock, id.clip.origin));
        self.current = Some((peer, offer));
        Ok(true)
    }
}

pub mod files;

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::content::Provider;
    fn inventory() -> dto::Inventory {
        crate::wire::inventory(&crate::content::tests::fixture().inventory()).unwrap()
    }
    #[test]
    fn migration_empty_local_copy_keeps_ordering_fence_against_older_peer_offer() {
        let mut state = Ordering::new(DeviceId([1; 32]), 100);
        let local = state.capture(inventory(), 1, Sensitivity::Normal, 0).unwrap().unwrap();
        assert_eq!(local.clock().unwrap(), 101);
        let now = Instant::now();
        assert!(state.local_change(2, now));
        assert!(state.capture_due(now + Duration::from_millis(300)));
        let mut empty = inventory().into_raw();
        empty.insert(3, V::Array(vec![]));
        empty.insert(4, V::Array(vec![]));
        assert!(
            state
                .capture(dto::Inventory::from_raw(empty).unwrap(), 2, Sensitivity::Normal, 0)
                .unwrap()
                .is_none()
        );
        // A delayed offer from another peer predates the local clipboard that
        // the user just cleared (also models a fully filtered local copy).
        assert!(
            !state.receive(DeviceId([2; 32]), offer(2, 50)).unwrap(),
            "an older peer offer must not overwrite a newer cleared local clipboard"
        );
        assert!(state.current().is_none(), "cleared content must not remain publishable");
        assert!(state.receive(DeviceId([2; 32]), offer(2, 101)).unwrap());
    }
    #[test]
    fn empty_copy_preserves_origin_tie_break_without_fencing_a_fresh_session() {
        let mut state = Ordering::new(DeviceId([2; 32]), 100);
        state.capture(inventory(), 1, Sensitivity::Normal, 0).unwrap();
        let mut empty = inventory().into_raw();
        empty.insert(3, V::Array(vec![]));
        empty.insert(4, V::Array(vec![]));
        let empty = dto::Inventory::from_raw(empty).unwrap();
        state.capture(empty.clone(), 2, Sensitivity::Normal, 0).unwrap();
        assert!(!state.receive(DeviceId([1; 32]), offer(1, 100)).unwrap());
        assert!(state.receive(DeviceId([3; 32]), offer(3, 100)).unwrap());

        let mut fresh = Ordering::new(DeviceId([2; 32]), 100);
        fresh.capture(empty, 1, Sensitivity::Normal, 0).unwrap();
        assert!(fresh.receive(DeviceId([1; 32]), offer(1, 50)).unwrap());
    }
    fn offer(device: u8, clock: u64) -> dto::ClipboardOffer {
        Ordering::new(DeviceId([device; 32]), clock)
            .capture(inventory(), 1, Sensitivity::Normal, 0)
            .unwrap()
            .unwrap()
    }
    #[test]
    fn quiet_window_tracks_the_last_os_transaction_and_never_guesses_completion() {
        let mut state = Ordering::new(DeviceId([1; 32]), 0);
        let at = Instant::now();
        assert!(state.local_change(1, at));
        assert!(!state.capture_due(at + Duration::from_millis(299)));
        assert!(state.local_change(2, at + Duration::from_millis(200)));
        assert!(!state.capture_due(at + Duration::from_millis(499)));
        assert!(state.capture_due(at + Duration::from_millis(500)));
        assert!(!state.capture_due(at + Duration::from_secs(1)));
    }
    #[test]
    fn every_new_copy_and_reconfiguration_gets_a_new_highwater_identity() {
        let mut state = Ordering::new(DeviceId([1; 32]), 99);
        let first = state.capture(inventory(), 1, Sensitivity::Normal, 0).unwrap().unwrap();
        state.revoke_local();
        let second = state.capture(inventory(), 1, Sensitivity::Normal, 0).unwrap().unwrap();
        assert_eq!(crate::wire::local_offer_id(first.id().unwrap()).unwrap().clip.seq, 100);
        assert_eq!(crate::wire::local_offer_id(second.id().unwrap()).unwrap().clip.seq, 101);
        assert_eq!(state.clock(), 101);
        assert_ne!(first.content_digest().unwrap(), second.content_digest().unwrap());
        state.written(2);
        assert!(!state.local_change(2, Instant::now()));
        assert!(state.capture(inventory(), 2, Sensitivity::Normal, 0).unwrap().is_none());
        assert!(state.local_change(3, Instant::now()));
    }
    #[test]
    fn lamport_and_complete_origin_order_are_independent_of_arrival_order() {
        let mut state = Ordering::new(DeviceId([2; 32]), 0);
        let low = offer(1, 10);
        let high = offer(3, 10);
        assert!(
            !state.receive(DeviceId([4; 32]), high.clone()).unwrap(),
            "transport peer must own the offer"
        );
        assert!(state.receive(DeviceId([1; 32]), low.clone()).unwrap());
        assert!(!state.receive(DeviceId([1; 32]), low.clone()).unwrap());
        assert!(state.receive(DeviceId([3; 32]), high.clone()).unwrap());
        assert!(!state.receive(DeviceId([1; 32]), low).unwrap());
        assert_eq!(state.source(), Some(DeviceId([3; 32])));
        state.forget(DeviceId([1; 32]));
        assert!(state.current().is_some());
        state.forget(DeviceId([3; 32]));
        assert!(state.current().is_none());
    }
    #[test]
    fn pending_local_copy_observes_remote_clock_before_its_own_publication() {
        let mut state = Ordering::new(DeviceId([1; 32]), 1);
        let at = Instant::now();
        state.local_change(1, at);
        assert!(!state.receive(DeviceId([3; 32]), offer(3, 100)).unwrap());
        assert_eq!(state.clock(), 101);
        assert!(state.capture_due(at + Duration::from_millis(300)));
        let local = state.capture(inventory(), 1, Sensitivity::Normal, 0).unwrap().unwrap();
        assert_eq!(local.clock().unwrap(), 102);
    }
    #[test]
    fn empty_copies_revoke_and_clock_exhaustion_never_wraps() {
        let mut state = Ordering::new(DeviceId([1; 32]), u64::MAX);
        assert!(state.capture(inventory(), 1, Sensitivity::Normal, 0).is_err());
        assert_eq!(state.clock(), u64::MAX);
        assert!(state.current().is_none());
        let mut raw = inventory().into_raw();
        raw.insert(3, V::Array(vec![]));
        raw.insert(4, V::Array(vec![]));
        assert!(
            state
                .capture(dto::Inventory::from_raw(raw).unwrap(), 2, Sensitivity::Normal, 0)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn reliable_offer_demotes_large_inline_data_to_the_same_stable_reader() {
        let mut local = crate::content::tests::fixture().inventory();
        local.items[0].representations[0].data = crate::model::RepresentationData::Inline(vec![7; 65536]);
        let offer = Ordering::new(DeviceId([1; 32]), 0)
            .capture(crate::wire::inventory(&local).unwrap(), 1, Sensitivity::Normal, 0)
            .unwrap()
            .unwrap();
        let inventory = crate::wire::local_inventory(&offer.inventory().unwrap()).unwrap();
        assert_eq!(
            inventory.items[0].representations[0].id,
            local.items[0].representations[0].id
        );
        assert!(matches!(
            inventory.items[0].representations[0].data,
            crate::model::RepresentationData::Reader {
                size: crate::model::ContentSize::Known(65536),
                ..
            }
        ));
        assert_eq!(offer.inventory().unwrap().inline_bytes().unwrap(), 0);
    }
}
