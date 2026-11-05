//! Wake scheduling only. Session/process observations still go through the
//! existing pre-login and helper policies.
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

const RECOVERY_STEP: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug)]
pub(crate) enum SessionEvent {
    ConsoleConnect(u32),
    ConsoleDisconnect(u32),
    Logon(u32),
    Logoff(u32),
    Changed,
}

#[derive(Clone, Copy)]
pub(crate) struct SessionState {
    pub local: Option<bool>,
    pub user: bool,
    pub gone: bool,
}

#[derive(Default)]
pub(crate) struct Schedule {
    console: Option<u32>,
    logons: BTreeSet<u32>,
    transition: Option<Instant>,
    failure: Option<Instant>,
    delay: Option<Duration>,
    bootstrapping: bool,
}
impl Schedule {
    pub(crate) fn bootstrap(&mut self, candidate: u32, now: Instant) {
        self.session(SessionEvent::ConsoleConnect(candidate), now);
        self.bootstrapping = true;
    }
    pub(crate) fn bootstrapping(&self) -> bool {
        self.bootstrapping
    }
    pub(crate) fn bootstrap_candidate(&mut self, candidate: Option<u32>) {
        if self.bootstrapping {
            self.console = candidate;
            if candidate.is_none() {
                self.bootstrapping = false;
                if self.logons.is_empty() {
                    self.transition = None;
                    self.delay = None;
                }
            }
        }
    }
    pub(crate) fn session(&mut self, event: SessionEvent, now: Instant) {
        match event {
            SessionEvent::ConsoleConnect(id) => {
                self.console = Some(id);
                self.bootstrapping = false;
            }
            SessionEvent::ConsoleDisconnect(id) => {
                if self.console == Some(id) {
                    self.console = None;
                }
            }
            SessionEvent::Logon(id) => {
                self.logons.insert(id);
            }
            SessionEvent::Logoff(id) => {
                self.logons.remove(&id);
                if self.console == Some(id) {
                    self.console = None;
                }
            }
            SessionEvent::Changed => {}
        }
        if self.console.is_some() || !self.logons.is_empty() {
            self.transition = Some(now);
            self.delay = Some(Duration::from_millis(1));
        }
    }

    /// Session notifications can precede publication of the console/token.
    /// Probe only while an announced transition is unresolved, never at idle.
    pub(crate) fn reconcile(
        &mut self,
        now: Instant,
        console: Option<u32>,
        mut state: impl FnMut(u32) -> SessionState,
    ) -> bool {
        let mut resolved = false;
        if let Some(id) = self.console {
            let observed = state(id);
            if (self.bootstrapping && console.is_some())
                || console == Some(id)
                || observed.local == Some(false)
                || observed.gone
            {
                self.console = None;
                self.bootstrapping = false;
                resolved = true;
            }
        }
        self.logons.retain(|id| {
            let observed = state(*id);
            let pending = !observed.user && !observed.gone;
            resolved |= !pending;
            pending
        });
        if self.console.is_none() && self.logons.is_empty() {
            self.transition = None;
            self.delay = None;
        } else if self.transition.is_some_and(|deadline| now >= deadline) {
            let delay = self.delay.unwrap_or(Duration::from_millis(1));
            self.transition = Some(now + delay);
            self.delay = Some((delay * 2).min(RECOVERY_STEP));
        }
        resolved
    }

    pub(crate) fn helper_result(&mut self, now: Instant, failed: bool) {
        // Keep the old recovery opportunity only while helper creation/stop has
        // failed; it is not a periodic idle wake or a new operation timeout.
        self.failure = failed.then_some(now + RECOVERY_STEP);
    }

    pub(crate) fn deadline(&self, _now: Instant, policy: Option<Instant>) -> Option<Instant> {
        [policy, self.transition, self.failure].into_iter().flatten().min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pending(_: u32) -> SessionState {
        SessionState {
            local: Some(true),
            user: false,
            gone: false,
        }
    }
    #[test]
    fn stable_idle_has_no_periodic_deadline() {
        let now = Instant::now();
        let schedule = Schedule::default();
        assert_eq!(schedule.deadline(now, None), None);
        assert_eq!(
            schedule.deadline(now, Some(now + Duration::from_secs(60))),
            Some(now + Duration::from_secs(60))
        );
    }
    #[test]
    fn console_publication_after_the_notification_is_not_lost() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        schedule.session(SessionEvent::ConsoleConnect(2), now);
        assert!(!schedule.reconcile(now, None, pending));
        assert!(schedule.deadline(now, None).is_some());
        assert!(schedule.reconcile(now + Duration::from_millis(1), Some(2), pending));
        assert_eq!(schedule.deadline(now, None), None);
    }
    #[test]
    fn logon_before_token_or_hive_readiness_only_rechecks_the_transition() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        schedule.session(SessionEvent::Logon(3), now);
        assert!(!schedule.reconcile(now, Some(3), pending));
        assert!(schedule.reconcile(now, Some(3), |_| SessionState {
            user: true,
            ..pending(3)
        }));
        assert_eq!(schedule.deadline(now, None), None);
    }
    #[test]
    fn remote_console_notifications_and_cancelled_transitions_do_not_poll() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        schedule.session(SessionEvent::ConsoleConnect(9), now);
        schedule.reconcile(now, Some(1), |_| SessionState {
            local: Some(false),
            ..pending(9)
        });
        assert_eq!(schedule.deadline(now, None), None);
        schedule.session(SessionEvent::ConsoleConnect(2), now);
        schedule.session(SessionEvent::ConsoleDisconnect(2), now);
        schedule.session(SessionEvent::Logon(2), now);
        schedule.session(SessionEvent::Logoff(2), now);
        schedule.session(SessionEvent::Changed, now);
        schedule.reconcile(now, None, pending);
        assert_eq!(schedule.deadline(now, None), None);
    }
    #[test]
    fn helper_recovery_stops_waking_as_soon_as_the_error_is_resolved() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        schedule.helper_result(now, true);
        assert_eq!(schedule.deadline(now, None), Some(now + RECOVERY_STEP));
        schedule.helper_result(now, false);
        assert_eq!(schedule.deadline(now, None), None);
    }
    #[test]
    fn startup_during_attach_accepts_the_first_published_physical_console() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        schedule.bootstrap(1, now);
        assert!(!schedule.reconcile(now, None, pending));
        assert!(schedule.reconcile(now, Some(2), pending));
        assert_eq!(schedule.deadline(now, None), None);
    }
    #[test]
    fn losing_the_bootstrap_candidate_does_not_drop_an_announced_logon() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        schedule.bootstrap(1, now);
        schedule.session(SessionEvent::Logon(2), now);
        schedule.bootstrap_candidate(None);
        assert!(schedule.deadline(now, None).is_some());
        assert!(schedule.reconcile(now, Some(2), |_| SessionState {
            user: true,
            ..pending(2)
        }));
        assert_eq!(schedule.deadline(now, None), None);
    }
    #[test]
    fn cancelled_startup_hint_does_not_leave_an_idle_timer() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        schedule.bootstrap(1, now);
        assert!(schedule.bootstrapping());
        schedule.bootstrap_candidate(None);
        assert!(!schedule.bootstrapping());
        assert_eq!(schedule.deadline(now, None), None);
    }
}
