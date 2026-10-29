//! Ordinary native-owner work. Routing revokes permits atomically before queued
//! OS work can execute; a delayed dispatch cannot steal foreground after timeout.
use oink_foundation::application_focus::Observation;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub type Observer = Arc<dyn Fn(Observation) + Send + Sync>;

#[derive(Clone, Debug)]
pub struct Permit {
    live: Arc<AtomicBool>,
    deadline: Instant,
    #[cfg(debug_assertions)]
    test_deadline: Option<(oink_foundation::application_focus::TestClock, u64)>,
}
impl Permit {
    pub fn new(lifetime: Duration) -> Self {
        Self {
            live: Arc::new(AtomicBool::new(true)),
            deadline: Instant::now() + lifetime,
            #[cfg(debug_assertions)]
            test_deadline: None,
        }
    }
    #[cfg(debug_assertions)]
    pub fn new_for_test(lifetime: Duration, clock: oink_foundation::application_focus::TestClock) -> Self {
        let deadline = clock.now().saturating_add(lifetime.as_micros() as u64);
        Self {
            test_deadline: Some((clock, deadline)),
            ..Self::new(lifetime)
        }
    }

    pub fn revoke(&self) {
        self.live.store(false, Ordering::Release);
    }
    /// Check on the native window owner immediately before activation, after
    /// observing foreground and validating its expected generation/sequence.
    pub fn valid(&self) -> bool {
        #[cfg(debug_assertions)]
        if let Some((clock, deadline)) = &self.test_deadline {
            return self.live.load(Ordering::Acquire) && clock.now() < *deadline;
        }
        self.live.load(Ordering::Acquire) && Instant::now() < self.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queued_copy_is_revoked_and_timeout_cannot_be_rearmed() {
        let permit = Permit::new(Duration::from_secs(1));
        let queued = permit.clone();
        permit.revoke();
        assert!(!queued.valid());
        assert!(!Permit::new(Duration::ZERO).valid());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn manual_deadline_does_not_weaken_expiration_or_revocation() {
        let clock = oink_foundation::application_focus::TestClock::default();
        let permit = Permit::new_for_test(Duration::from_millis(250), clock.clone());
        clock.advance(249_999);
        assert!(permit.valid());
        clock.advance(1);
        assert!(!permit.valid());
        let permit = Permit::new_for_test(Duration::from_millis(250), clock);
        let queued = permit.clone();
        permit.revoke();
        assert!(!queued.valid());
    }
}
