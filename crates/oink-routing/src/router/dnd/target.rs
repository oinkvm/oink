//! Shared, irreversible authority checked again at asynchronous execution points.
use crate::dnd::PreparationKey;
use oink_foundation::content::Holder;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

#[derive(Clone, Debug)]
pub struct DndPreparation {
    pub key: PreparationKey,
    pub source_session: u64,
    current: Arc<AtomicBool>,
}
impl PartialEq for DndPreparation {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.current, &other.current)
    }
}
impl DndPreparation {
    pub fn new(key: PreparationKey, source_session: u64) -> Self {
        Self {
            key,
            source_session,
            current: Arc::new(AtomicBool::new(true)),
        }
    }
    pub fn current(&self) -> bool {
        self.current.load(Ordering::Acquire)
    }
    pub fn revoke(&self) {
        self.current.store(false, Ordering::Release);
    }
}

const RELEASED: u8 = 1;
const REVOKED: u8 = 2;
const CLAIMED: u8 = 4;
const DOWN: u8 = 8;
const UP: u8 = 16;
const DELIVERING: u8 = 32;

#[derive(Clone, Debug)]
pub struct DndTarget {
    pub preparation: DndPreparation,
    pub holder: Holder,
    pub input: oink_foundation::DeviceId,
    pub revision: u64,
    state: Arc<AtomicU8>,
    preparation_deadline: std::time::Instant,
    entered_at: Option<std::time::Instant>,
}
impl PartialEq for DndTarget {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}
impl DndTarget {
    pub fn new(preparation: DndPreparation, holder: Holder, revision: u64, input: oink_foundation::DeviceId) -> Self {
        Self {
            preparation,
            holder,
            input,
            revision,
            state: Arc::new(AtomicU8::new(0)),
            entered_at: None,
            preparation_deadline: std::time::Instant::now() + std::time::Duration::from_secs(3),
        }
    }
    /// Local accepted FocusEnter, preserved across deferred metadata and Begin retries.
    /// No timestamp is serialized or used to change routing/deadlines.
    pub fn with_entered_at(mut self, at: std::time::Instant) -> Self {
        self.entered_at = Some(at);
        self
    }
    pub fn entered_at(&self) -> Option<std::time::Instant> {
        self.entered_at
    }
    /// Target-local execution deadline, never a wire clock or a transfer timeout.
    pub fn with_preparation_remaining(mut self, remaining: std::time::Duration) -> Self {
        self.preparation_deadline = std::time::Instant::now() + remaining;
        self
    }
    pub fn preparation_deadline(&self) -> std::time::Instant {
        self.preparation_deadline
    }
    pub fn begin_current(&self) -> bool {
        self.preparation.current() && self.state.load(Ordering::Acquire) & (RELEASED | REVOKED) == 0
    }
    pub fn content_current(&self) -> bool {
        self.state.load(Ordering::Acquire) & REVOKED == 0
    }
    pub fn released(&self) -> bool {
        self.state.load(Ordering::Acquire) & RELEASED != 0
    }
    pub(super) fn drop_pending(&self) -> bool {
        self.state.load(Ordering::Acquire) & (DOWN | RELEASED | REVOKED) == DOWN | RELEASED
    }
    pub fn release(&self) {
        self.state.fetch_or(RELEASED, Ordering::AcqRel);
    }
    pub fn revoke(&self) {
        self.state.fetch_or(REVOKED, Ordering::AcqRel);
    }
    pub fn leave(&self) {
        if self.state.load(Ordering::Acquire) & DELIVERING == 0 {
            self.revoke();
        }
    }
    pub fn delivered(&self) {
        self.state.fetch_or(DELIVERING | RELEASED, Ordering::AcqRel);
    }
    /// Linearization point for DOWN: no input-thread lock or native wait.
    pub fn claim_down(&self) -> bool {
        self.preparation.current()
            && self
                .state
                .compare_exchange(0, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }
    pub fn down_submitted(&self, success: bool) {
        if success {
            self.state.fetch_or(DOWN, Ordering::AcqRel);
        } else {
            self.revoke();
        }
    }
    /// Backend failure cleanup, native cancellation and keyed Released share this debt.
    pub fn claim_up(&self) -> bool {
        self.state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & DOWN != 0 && state & UP == 0).then_some(state | UP)
            })
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dnd::DragKey;
    use oink_foundation::DeviceId;
    use oink_foundation::content::{ClipId, OfferId};
    fn target() -> DndTarget {
        let id = ClipId {
            origin: DeviceId([1; 32]),
            seq: 1,
        };
        DndTarget::new(
            DndPreparation::new(
                PreparationKey {
                    drag: DragKey {
                        offer: OfferId {
                            clip: id,
                            generation: 1,
                        },
                        native_generation: 1,
                    },
                    metadata_revision: 1,
                },
                1,
            ),
            Holder {
                device: DeviceId([2; 32]),
                epoch: 2,
            },
            1,
            DeviceId([1; 32]),
        )
    }
    #[test]
    fn target_entry_survives_clone_and_preparation_rescheduling() {
        let entered = std::time::Instant::now() - std::time::Duration::from_millis(25);
        let grant = target().with_entered_at(entered);
        let resumed = grant
            .clone()
            .with_preparation_remaining(std::time::Duration::from_secs(1));
        assert_eq!(resumed.entered_at(), Some(entered));
        assert_eq!(target().entered_at(), None);
    }
    #[test]
    fn release_before_down_and_release_during_down_have_distinct_debts() {
        let t = target();
        t.release();
        assert!(!t.claim_down());
        assert!(!t.claim_up());
        let t = target();
        assert!(t.claim_down());
        t.release();
        assert!(!t.claim_up());
        t.down_submitted(true);
        assert!(t.claim_up());
        assert!(!t.claim_up());
        assert!(t.content_current());
        assert!(!t.begin_current());
        t.delivered();
        t.leave();
        assert!(t.content_current());
        t.revoke();
        assert!(!t.content_current());
    }
    #[test]
    fn old_preparation_and_failed_down_never_activate_or_emit_up() {
        let t = target();
        t.preparation.revoke();
        assert!(!t.claim_down());
        let t = target();
        assert!(t.claim_down());
        t.down_submitted(false);
        assert!(!t.claim_up());
        assert!(!t.claim_down());
    }
}
