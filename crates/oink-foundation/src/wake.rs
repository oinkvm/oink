//! Coalesced notification around an owner's native wait. Producers publish work
//! first, then notify; only a transition out of Waiting needs a kernel wake.
use std::sync::atomic::{AtomicU8, Ordering};
const AWAKE: u8 = 0;
const NOTIFIED: u8 = 1;
const WAITING: u8 = 2;
#[derive(Debug, Default)]
pub struct WakeGate(AtomicU8);
impl WakeGate {
    /// Start a turn before inspecting any queues.
    pub fn begin_turn(&self) {
        self.0.store(AWAKE, Ordering::Release);
    }
    /// Called after the last empty queue check. False requires another turn.
    pub fn prepare_wait(&self) -> bool {
        self.0
            .compare_exchange(AWAKE, WAITING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    /// Native input/timer arrival makes the owner awake without consuming a queue notification.
    pub fn awake(&self) {
        let _ = self
            .0
            .compare_exchange(WAITING, AWAKE, Ordering::AcqRel, Ordering::Acquire);
    }
    /// True asks the caller to wake its native wait. Further posts coalesce.
    pub fn notify(&self) -> bool {
        self.0.swap(NOTIFIED, Ordering::AcqRel) == WAITING
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn awake_posts_and_racing_posts_need_no_syscall_and_cannot_park() {
        let gate = WakeGate::default();
        for _ in 0..1000 {
            assert!(!gate.notify());
        }
        assert!(!gate.prepare_wait());
        gate.begin_turn();
        assert!(gate.prepare_wait());
        assert!(gate.notify());
        assert!(!gate.notify());
        gate.awake();
        assert!(!gate.prepare_wait());
        gate.begin_turn();
        assert!(gate.prepare_wait());
        gate.awake();
        assert!(!gate.notify());
        assert!(!gate.prepare_wait());
    }
}
