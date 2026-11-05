//! Connection lease clock. Only an authenticated heartbeat renews it.
use std::time::{Duration, Instant};
const LEASE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(crate) struct Lease {
    expires: Option<Instant>,
}
impl Lease {
    pub(crate) fn connect(&mut self, now: Instant) {
        self.expires = Some(now + LEASE);
    }
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.expires
    }
    pub(crate) fn expired(&self, now: Instant) -> bool {
        self.expires.is_some_and(|at| now >= at)
    }
    pub(crate) fn frame(&mut self, now: Instant, heartbeat: bool) -> bool {
        if self.expires.is_none() || self.expired(now) {
            return false;
        }
        if heartbeat {
            self.expires = Some(now + LEASE);
        }
        true
    }
    pub(crate) fn disconnect(&mut self) {
        self.expires = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disconnected_has_no_timer_and_connection_has_only_its_expiry() {
        let now = Instant::now();
        let mut lease = Lease::default();
        assert_eq!(lease.deadline(), None);
        lease.connect(now);
        assert_eq!(lease.deadline(), Some(now + LEASE));
        lease.disconnect();
        assert_eq!(lease.deadline(), None);
    }
    #[test]
    fn heartbeat_moves_the_one_shot_expiry_but_queries_do_not() {
        let now = Instant::now();
        let mut lease = Lease::default();
        lease.connect(now);
        assert!(lease.frame(now + Duration::from_millis(400), true));
        assert_eq!(lease.deadline(), Some(now + Duration::from_millis(2400)));
        for ms in [800, 1200, 1600, 2000, 2399] {
            assert!(lease.frame(now + Duration::from_millis(ms), false));
        }
        assert!(!lease.expired(now + Duration::from_millis(2399)));
        assert!(lease.expired(now + Duration::from_millis(2400)));
        assert!(!lease.frame(now + Duration::from_millis(2400), true));
        assert_eq!(lease.deadline(), Some(now + Duration::from_millis(2400)));
    }
    #[test]
    fn an_expired_connection_cannot_be_revived_by_a_late_heartbeat() {
        let now = Instant::now();
        let mut lease = Lease::default();
        lease.connect(now);
        assert!(!lease.frame(now + LEASE, true));
        lease.disconnect();
        assert!(!lease.frame(now + LEASE, true));
        lease.connect(now + LEASE);
        assert_eq!(lease.deadline(), Some(now + LEASE * 2));
    }
}
