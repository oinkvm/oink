//! Clipboard copy ordering. Content is always an Inventory; payload operations
//! belong to the native content engine and never use positional item indices.
use oink_foundation::clipboard_burst::Burst;
use oink_foundation::{DeviceId, clipboard::Sensitivity};
use oink_protocol::v2::{
    self, RawValue as V, dto,
    schema::{WireDto, WireValue},
};
use std::time::Instant;

/// The quiet period aggregates separately notified writes, not
/// delayed-rendered bytes. Native capture and every later read still
/// validate the source identity. Like the former 300 ms heuristic, this
/// cannot detect an owner filling an already declared type without changing
/// its native generation.
///
/// It is also the announcement debounce (clipboard.md 2026-11-12), shared
/// with the phone's privileged capture (`oink_foundation::clipboard_burst`):
/// local changes become offers only after it, so a clipboard written several
/// times in quick succession is announced once, as its last state.
pub use oink_foundation::clipboard_burst::{SOURCE_BURST_CAP, SOURCE_QUIET_PERIOD};

pub struct Ordering {
    device: DeviceId,
    clock: u64,
    current: Option<(DeviceId, dto::ClipboardOffer)>,
    // Withdrawing publishable content must not forget which copy won ordering.
    // An empty or fully filtered local copy still supersedes older peer offers.
    ordering_fence: Option<(u64, DeviceId)>,
    written: u64,
    pending: Burst,
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
            pending: Burst::default(),
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
    /// A local change turned out to be an echo of the current content
    /// (`crate::dedupe`): the clipboard keeps its previous Oink identity.
    /// The ordering fence was never moved by the local change.
    ///
    /// Restoring our own withdrawn offer (resume after a pause) also restores
    /// its ordering fence: an older peer offer must not replace it.
    pub fn restore(&mut self, current: Option<(DeviceId, dto::ClipboardOffer)>) {
        self.pending.clear();
        let order = current.as_ref().and_then(|(_, offer)| {
            let id = crate::wire::local_offer_id(offer.id().ok()?).ok()?;
            Some((offer.clock().ok()?, id.clip.origin))
        });
        if let Some(order) = order
            && self.ordering_fence.is_none_or(|fence| fence < order)
        {
            self.ordering_fence = Some(order);
        }
        self.current = current;
    }
    /// A group offer this device does not take (receiving is off) still
    /// advances its clock. True when the clock changed.
    pub fn witness_clock(&mut self, clock: u64) -> bool {
        if clock <= self.clock {
            return false;
        }
        self.clock = clock;
        true
    }
    /// Allocate a clip sequence for content written without an announced
    /// offer (a direct mobile share). The caller persists the clock.
    pub fn reserve(&mut self) -> Option<u64> {
        self.clock = self.clock.checked_add(1)?;
        Some(self.clock)
    }
    pub fn current_with_source(&self) -> Option<(DeviceId, dto::ClipboardOffer)> {
        self.current.clone()
    }
    pub fn local_change(&mut self, count: u64, now: Instant) -> bool {
        if count <= self.written {
            return false;
        }
        // Each change restarts the quiet period, but never past the cap
        // counted from the burst's first change.
        self.pending.change(now);
        self.current = None;
        true
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.deadline()
    }
    /// The capture this allows ends the burst: a change seen after it starts
    /// a new one (a capture overtaken by a change is discarded, and the new
    /// burst announces the newer state).
    pub fn capture_due(&mut self, now: Instant) -> bool {
        self.pending.due(now)
    }
    pub fn revoke_local(&mut self) {
        if self.ordering_fence.is_some_and(|(_, origin)| origin == self.device) {
            self.ordering_fence = None;
        }
        if self.source() == Some(self.device) {
            self.current = None;
        }
        self.pending.clear();
    }
    /// Last native write accepted by this ordering state (diagnostics only).
    pub fn written_count(&self) -> u64 {
        self.written
    }
    pub fn written(&mut self, count: u64) {
        self.written = count;
        self.pending.clear();
    }
    /// Retire content without lowering the order fence or losing a pending
    /// physical copy. Used when a head-only sensitive record is superseded.
    pub fn withdraw(&mut self, clip: crate::model::ClipId) {
        if self
            .current()
            .and_then(|offer| offer.id().ok())
            .and_then(|id| crate::wire::local_offer_id(id).ok())
            .is_some_and(|id| id.clip == clip)
        {
            self.current = None;
        }
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
        digests: Vec<dto::ContentDigest>,
        count: u64,
        sensitivity: Sensitivity,
        time: u64,
    ) -> v2::Result<Option<dto::ClipboardOffer>> {
        if count <= self.written && count != 0 {
            return Ok(None);
        }
        if inventory.items()?.0.is_empty() && inventory.roots()?.0.is_empty() {
            self.current = None;
            self.pending.clear();
            return Ok(None);
        }
        inventory = crate::wire::reliable_inventory(&inventory)?;
        let clock = self.clock.checked_add(1).ok_or(v2::ErrorCode::StateConflict)?;
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
        let build = |inventory: &dto::Inventory| -> v2::Result<dto::ClipboardOffer> {
            let mut operation = blake3::Hasher::new();
            operation.update(b"oink/clipboard/operation/2");
            operation.update(&id.clip.origin.0);
            operation.update(&id.clip.seq.to_le_bytes());
            operation.update(&id.generation.to_le_bytes());
            let operation = operation.finalize();
            let mut message = blake3::Hasher::new();
            message.update(b"oink/clipboard/message/2");
            message.update(&id.clip.origin.0);
            message.update(&id.clip.seq.to_le_bytes());
            message.update(&id.generation.to_le_bytes());
            let message = message.finalize();
            dto::ClipboardOffer::from_raw(
                [
                    (1, crate::wire::offer_id(id).write()),
                    (2, V::Uint(count)),
                    (3, V::Uint(clock)),
                    (4, crate::wire::sensitivity(sensitivity).write()),
                    (6, inventory.write()),
                    (7, V::Uint(time)),
                    (8, crate::wire::device(self.device).write()),
                    (
                        10,
                        v2::schema::FixedBytes::<16>(operation.as_bytes()[..16].try_into().unwrap()).write(),
                    ),
                    (
                        11,
                        v2::schema::FixedBytes::<16>(message.as_bytes()[..16].try_into().unwrap()).write(),
                    ),
                    (12, V::Uint(inventory.items()?.0.len() as u64)),
                    (13, crate::dedupe::digests_value(digests.clone())),
                ]
                .into(),
            )
        };
        let mut offer = build(&inventory)?;
        if inventory.inline_bytes()? > 0
            && v2::frame::encode(&v2::Message::ClipboardOffer(offer.clone()), v2::limits::RELIABLE_FRAME).is_err()
        {
            inventory = crate::wire::descriptor_inventory(&inventory)?;
            offer = build(&inventory)?;
        }
        self.clock = clock;
        self.ordering_fence = Some((clock, self.device));
        self.pending.clear();
        self.current = Some((self.device, offer.clone()));
        Ok(Some(offer))
    }
    pub fn receive(&mut self, peer: DeviceId, offer: dto::ClipboardOffer) -> v2::Result<bool> {
        let id = crate::wire::local_offer_id(offer.id()?)?;
        if id.clip.origin != peer || peer == self.device {
            return Ok(false);
        }
        let clock = offer.clock()?;
        self.clock = self.clock.max(clock);
        if self.pending.deadline().is_some() {
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
    use std::time::Duration;
    fn inventory() -> dto::Inventory {
        crate::wire::inventory(&crate::content::tests::fixture().inventory()).unwrap()
    }
    #[test]
    fn migration_empty_local_copy_keeps_ordering_fence_against_older_peer_offer() {
        let mut state = Ordering::new(DeviceId([1; 32]), 100);
        let local = state
            .capture(inventory(), vec![], 1, Sensitivity::Normal, 0)
            .unwrap()
            .unwrap();
        assert_eq!(local.clock().unwrap(), 101);
        let now = Instant::now();
        assert!(state.local_change(2, now));
        assert!(state.capture_due(now + Duration::from_millis(300)));
        let mut empty = inventory().into_raw();
        empty.insert(3, V::Array(vec![]));
        empty.insert(4, V::Array(vec![]));
        assert!(
            state
                .capture(
                    dto::Inventory::from_raw(empty).unwrap(),
                    vec![],
                    2,
                    Sensitivity::Normal,
                    0
                )
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
        state.capture(inventory(), vec![], 1, Sensitivity::Normal, 0).unwrap();
        let mut empty = inventory().into_raw();
        empty.insert(3, V::Array(vec![]));
        empty.insert(4, V::Array(vec![]));
        let empty = dto::Inventory::from_raw(empty).unwrap();
        state.capture(empty.clone(), vec![], 2, Sensitivity::Normal, 0).unwrap();
        assert!(!state.receive(DeviceId([1; 32]), offer(1, 100)).unwrap());
        assert!(state.receive(DeviceId([3; 32]), offer(3, 100)).unwrap());

        let mut fresh = Ordering::new(DeviceId([2; 32]), 100);
        fresh.capture(empty, vec![], 1, Sensitivity::Normal, 0).unwrap();
        assert!(fresh.receive(DeviceId([1; 32]), offer(1, 50)).unwrap());
    }
    #[test]
    fn offer_accepts_only_direct_or_proxy_announced_sender_combinations() {
        let raw = offer(1, 10).into_raw();
        let with = |sender: u8, proxy: Option<u8>| {
            let mut raw = raw.clone();
            raw.insert(8, crate::wire::device(DeviceId([sender; 32])).write());
            match proxy {
                Some(proxy) => raw.insert(9, crate::wire::device(DeviceId([proxy; 32])).write()),
                None => raw.remove(&9),
            };
            dto::ClipboardOffer::from_raw(raw)
        };
        // Direct: the origin is the real sender and there is no proxy.
        assert!(with(1, None).is_ok());
        // Proxied: the origin announces as proxy for a different real sender.
        assert!(with(2, Some(1)).is_ok());
        // Contradictory: another sender without a proxy, a proxy that is not
        // the origin, or a proxy naming itself as the real sender.
        assert!(with(2, None).is_err());
        assert!(with(2, Some(3)).is_err());
        assert!(with(1, Some(1)).is_err());
        assert!(with(3, Some(2)).is_err());
    }
    fn offer(device: u8, clock: u64) -> dto::ClipboardOffer {
        Ordering::new(DeviceId([device; 32]), clock)
            .capture(inventory(), vec![], 1, Sensitivity::Normal, 0)
            .unwrap()
            .unwrap()
    }
    #[test]
    fn quiet_window_tracks_the_last_os_transaction_and_never_guesses_completion() {
        let mut state = Ordering::new(DeviceId([1; 32]), 0);
        let at = Instant::now();
        let ms = Duration::from_millis;
        assert!(state.local_change(1, at));
        assert!(!state.capture_due(at + ms(49)));
        assert!(state.local_change(2, at + ms(50)));
        assert!(!state.capture_due(at + ms(50) + SOURCE_QUIET_PERIOD - ms(1)));
        assert!(state.capture_due(at + ms(50) + SOURCE_QUIET_PERIOD));
        assert!(!state.capture_due(at + Duration::from_secs(10)));
    }

    // Announcement debounce (clipboard.md 2026-11-12), with an injected clock:
    // `local_change`/`capture_due` are the only way a local change becomes an
    // offer (the agent captures exactly when `capture_due` says so).

    /// Drives one burst: returns the offsets (ms) at which a capture became
    /// due while `changes` (ms offsets) arrive, checking every millisecond.
    fn due_while(state: &mut Ordering, at: Instant, changes: &[u64], until: u64) -> Vec<u64> {
        let mut due = vec![];
        let mut count = 0;
        for t in 0..=until {
            let now = at + Duration::from_millis(t);
            if state.capture_due(now) {
                due.push(t);
            }
            if changes.contains(&t) {
                count += 1;
                assert!(state.local_change(count, now));
            }
        }
        due
    }

    #[test]
    fn debounce_a_double_write_is_announced_once_as_its_last_state() {
        // The 2026-11-12 case: an application wrote the clipboard twice
        // ~180 ms apart; the first state was announced and was gone 8 ms later.
        let mut state = Ordering::new(DeviceId([1; 32]), 1309);
        let at = Instant::now();
        assert_eq!(due_while(&mut state, at, &[0, 180], 3000), vec![430]);
        assert!(state.current().is_none(), "nothing is current while the burst settles");
        // The single capture reads the clipboard as it is now (count 2).
        let offer = state
            .capture(inventory(), vec![], 2, Sensitivity::Normal, 0)
            .unwrap()
            .unwrap();
        // The dropped intermediate state consumed no clip sequence.
        assert_eq!(crate::wire::local_offer_id(offer.id().unwrap()).unwrap().clip.seq, 1310);
        assert_eq!(offer.source_change_count().unwrap(), 2);
        assert_eq!(state.clock(), 1310);
    }

    #[test]
    fn debounce_a_single_change_is_announced_after_the_quiet_period() {
        let mut state = Ordering::new(DeviceId([1; 32]), 0);
        let at = Instant::now();
        assert_eq!(SOURCE_QUIET_PERIOD, Duration::from_millis(250));
        assert_eq!(due_while(&mut state, at, &[0], 3000), vec![250]);
    }

    #[test]
    fn debounce_a_continuously_changing_clipboard_is_announced_at_the_cap() {
        let mut state = Ordering::new(DeviceId([1; 32]), 0);
        let at = Instant::now();
        assert_eq!(SOURCE_BURST_CAP, Duration::from_secs(1));
        // A change every 100 ms for 2.5 s never leaves 250 ms of quiet: each
        // burst is captured 1 s after its first change, then the next starts.
        let changes: Vec<u64> = (0..=2500).step_by(100).collect();
        assert_eq!(due_while(&mut state, at, &changes, 4000), vec![1000, 2000, 2750]);
    }

    #[test]
    fn debounce_a_pause_inside_the_window_announces_nothing() {
        let mut state = Ordering::new(DeviceId([1; 32]), 7);
        let at = Instant::now();
        assert!(state.local_change(1, at));
        assert!(state.local_change(2, at + Duration::from_millis(100)));
        // Pausing sharing (or turning sending off) withdraws local content:
        // the agent's reconfiguration calls `revoke_local`.
        state.revoke_local();
        assert_eq!(state.deadline(), None);
        assert!(!state.capture_due(at + Duration::from_secs(10)));
        assert_eq!(state.clock(), 7, "no clip sequence was consumed");
        assert!(state.current().is_none());
    }

    #[test]
    fn debounce_oink_own_writes_never_open_a_burst() {
        let mut state = Ordering::new(DeviceId([1; 32]), 0);
        let at = Instant::now();
        state.written(5);
        assert!(
            !state.local_change(5, at),
            "a remote clip Oink wrote is not a local copy"
        );
        assert_eq!(state.deadline(), None);
        // A write landing inside a local burst settles it: the clipboard now
        // holds the written remote content, which is never announced.
        assert!(state.local_change(6, at));
        state.written(7);
        assert_eq!(state.deadline(), None);
        assert!(!state.capture_due(at + Duration::from_secs(10)));
    }
    #[test]
    fn every_new_copy_and_reconfiguration_gets_a_new_highwater_identity() {
        let mut state = Ordering::new(DeviceId([1; 32]), 99);
        let first = state
            .capture(inventory(), vec![], 1, Sensitivity::Normal, 0)
            .unwrap()
            .unwrap();
        state.revoke_local();
        let second = state
            .capture(inventory(), vec![], 1, Sensitivity::Normal, 0)
            .unwrap()
            .unwrap();
        assert_eq!(crate::wire::local_offer_id(first.id().unwrap()).unwrap().clip.seq, 100);
        assert_eq!(crate::wire::local_offer_id(second.id().unwrap()).unwrap().clip.seq, 101);
        assert_eq!(state.clock(), 101);
        assert_ne!(first.id().unwrap(), second.id().unwrap());
        state.written(2);
        assert!(!state.local_change(2, Instant::now()));
        assert!(
            state
                .capture(inventory(), vec![], 2, Sensitivity::Normal, 0)
                .unwrap()
                .is_none()
        );
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
        let local = state
            .capture(inventory(), vec![], 1, Sensitivity::Normal, 0)
            .unwrap()
            .unwrap();
        assert_eq!(local.clock().unwrap(), 102);
    }
    #[test]
    fn resume_publication_and_a_peer_copy_are_concurrent_until_they_converge() {
        let at = Instant::now();
        let mut a = Ordering::new(DeviceId([1; 32]), 100);
        let mut b = Ordering::new(DeviceId([2; 32]), 100);
        // Input is ready, but B's resume has only just observed its paused copy.
        b.local_change(1, at);
        let early = a
            .capture(inventory(), vec![], 1, Sensitivity::Normal, 0)
            .unwrap()
            .unwrap();
        assert_eq!(early.clock().unwrap(), 101);
        assert!(!b.receive(DeviceId([1; 32]), early.clone()).unwrap());
        assert!(b.capture_due(at + Duration::from_millis(300)));
        let resumed = b
            .capture(inventory(), vec![], 1, Sensitivity::Normal, 0)
            .unwrap()
            .unwrap();
        assert_eq!(resumed.clock().unwrap(), 102);
        assert!(a.receive(DeviceId([2; 32]), resumed).unwrap());
        a.written(2);
        // Waiting longer cannot make the losing offer win. Observe restoration
        // first, then issue the sequential delivery check's new physical copy.
        assert!(!b.capture_due(at + Duration::from_secs(60)));
        assert!(!b.receive(DeviceId([1; 32]), early).unwrap());
        assert!(a.local_change(3, at + Duration::from_secs(60)));
        assert!(a.capture_due(at + Duration::from_secs(61)));
        let after = a
            .capture(inventory(), vec![], 3, Sensitivity::Normal, 0)
            .unwrap()
            .unwrap();
        assert_eq!(after.clock().unwrap(), 103);
        assert!(b.receive(DeviceId([1; 32]), after).unwrap());
    }
    #[test]
    fn empty_copies_revoke_and_clock_exhaustion_never_wraps() {
        let mut state = Ordering::new(DeviceId([1; 32]), u64::MAX);
        assert!(state.capture(inventory(), vec![], 1, Sensitivity::Normal, 0).is_err());
        assert_eq!(state.clock(), u64::MAX);
        assert!(state.current().is_none());
        let mut raw = inventory().into_raw();
        raw.insert(3, V::Array(vec![]));
        raw.insert(4, V::Array(vec![]));
        assert!(
            state
                .capture(
                    dto::Inventory::from_raw(raw).unwrap(),
                    vec![],
                    2,
                    Sensitivity::Normal,
                    0
                )
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn reliable_offer_demotes_large_inline_data_to_the_same_stable_reader() {
        let mut local = crate::content::tests::fixture().inventory();
        local.items[0].representations[0].data = crate::model::RepresentationData::Inline(vec![7; 65536]);
        let offer = Ordering::new(DeviceId([1; 32]), 0)
            .capture(
                crate::wire::inventory(&local).unwrap(),
                vec![],
                1,
                Sensitivity::Normal,
                0,
            )
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
