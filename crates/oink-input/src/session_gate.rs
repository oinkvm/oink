//! Nonblocking pause/resume boundary shared by capture, routing and injection.
use crate::capture_loop::Wake;
use std::sync::{
    Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputSafety {
    /// Advances on every pause/resume transition, including changes between reads.
    pub epoch: u64,
    pub paused: bool,
}
impl InputSafety {
    pub fn active(self) -> bool {
        !self.paused
    }
    pub fn accepts(self, queued: Self) -> bool {
        self.active() && queued.active() && self.epoch == queued.epoch
    }
}

pub struct SessionGate {
    state: AtomicU64,
    wakes: Mutex<Vec<Weak<dyn Fn() + Send + Sync>>>,
}
impl std::fmt::Debug for SessionGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionGate").field(&self.snapshot()).finish()
    }
}
impl Default for SessionGate {
    fn default() -> Self {
        Self {
            state: AtomicU64::new(0),
            wakes: Mutex::new(Vec::new()),
        }
    }
}
impl SessionGate {
    pub fn snapshot(&self) -> InputSafety {
        let state = self.state.load(Ordering::Acquire);
        InputSafety {
            epoch: state >> 1,
            paused: state & 1 != 0,
        }
    }
    /// Concurrent pause requests publish only actual state transitions.
    pub fn set_paused(&self, paused: bool) {
        let bits = u64::from(paused);
        if self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |before| {
                if before & 1 == bits {
                    return None;
                }
                Some((((before >> 1) + 1) << 1) | bits)
            })
            .is_err()
        {
            return;
        }
        self.wakes.lock().unwrap_or_else(|e| e.into_inner()).retain(|weak| {
            if let Some(wake) = weak.upgrade() {
                wake();
                true
            } else {
                false
            }
        });
    }
    /// The consumer retains the wake for its lifetime; stopping it needs no unsubscribe race.
    pub fn watch(&self, wake: &Wake) {
        self.wakes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(std::sync::Arc::downgrade(wake));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_pause_requests_advance_once_and_resume_rejects_old_work() {
        let gate = std::sync::Arc::new(SessionGate::default());
        let old = gate.snapshot();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| gate.set_paused(true));
            }
        });
        let paused = gate.snapshot();
        assert_eq!(paused.epoch, old.epoch + 1);
        assert!(!paused.active());
        gate.set_paused(false);
        let fresh = gate.snapshot();
        assert_eq!(fresh.epoch, paused.epoch + 1);
        assert!(fresh.active());
        assert!(!fresh.accepts(old));
        gate.set_paused(false);
        assert_eq!(gate.snapshot(), fresh);
    }
}
