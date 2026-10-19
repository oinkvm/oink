//! One replacement slot per notice kind and peer; an ordinary consumer cannot block
//! the input thread. Stream identities in faults prevent a late fault closing a replacement.
use super::input::{InputNotice, Which};
use arc_swap::ArcSwapOption;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
#[derive(Debug)]
struct Entry {
    sequence: u64,
    notice: InputNotice,
}
#[derive(Debug)]
pub(super) struct Notices {
    slots: Box<[ArcSwapOption<Entry>]>,
    sequence: AtomicU64,
    pub(super) changed: tokio::sync::Notify,
    pub(super) closed: AtomicBool,
}
fn key(notice: InputNotice) -> (Option<oink_foundation::DeviceId>, u8) {
    match notice {
        InputNotice::Fault {
            peer,
            which: Which::Outbound,
            ..
        } => (Some(peer), 0),
        InputNotice::Fault {
            peer,
            which: Which::Inbound,
            ..
        } => (Some(peer), 1),
        InputNotice::EmergencyPause => (None, 2),
        InputNotice::Controllable(_) => (None, 3),
        InputNotice::ProtocolViolation { peer, .. } => (Some(peer), 4),
    }
}
impl Notices {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: (0..(oink_protocol::v1::limits::MEMBERS * 3 + 2))
                .map(|_| ArcSwapOption::empty())
                .collect(),
            sequence: AtomicU64::new(0),
            changed: tokio::sync::Notify::new(),
            closed: AtomicBool::new(false),
        })
    }
    pub(super) fn post(&self, notice: InputNotice) {
        let slot = self
            .slots
            .iter()
            .find(|s| s.load().as_ref().is_some_and(|e| key(e.notice) == key(notice)))
            .or_else(|| self.slots.iter().find(|s| s.load().is_none()))
            .expect("reserved notice slot");
        slot.store(Some(Arc::new(Entry {
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            notice,
        })));
        self.changed.notify_one();
    }
    pub(super) fn take(&self) -> Option<InputNotice> {
        self.slots
            .iter()
            .filter_map(|s| s.load_full().map(|e| (s, e.sequence)))
            .min_by_key(|(_, sequence)| *sequence)
            .and_then(|(s, _)| s.swap(None))
            .map(|e| e.notice)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn stopped_ordinary_consumer_does_not_block_reserved_notices() {
        let notices = Notices::new();
        for _ in 0..1000 {
            notices.post(InputNotice::EmergencyPause);
        }
        assert_eq!(notices.take(), Some(InputNotice::EmergencyPause));
        assert_eq!(notices.take(), None);
    }
}
