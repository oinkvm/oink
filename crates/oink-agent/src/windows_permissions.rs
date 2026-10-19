//! Read-only observations of installer-provisioned Windows access.
use crate::api::{PermissionItem, PermissionKind};
use oink_foundation::input::PermissionState;

#[cfg(windows)]
mod native;
#[cfg(windows)]
mod notifications;

#[cfg(all(test, windows))]
pub(crate) fn test_observations() -> usize {
    native::OBSERVATIONS.get()
}

// Only policy notifications authorize another expensive firewall query. Timer
// ticks, focus refreshes and failed/unknown query results do not authorize one.
pub(crate) struct Sampler {
    startup: bool,
    changed_at: Option<std::time::Instant>,
    last: Observation,
}
impl Default for Sampler {
    fn default() -> Self {
        Self {
            startup: true,
            changed_at: None,
            last: Observation::UNKNOWN,
        }
    }
}
impl Sampler {
    pub(crate) fn sample(
        &mut self,
        now: std::time::Instant,
        policy_changed: bool,
        firewall: impl FnOnce() -> PermissionState,
        service: impl FnOnce() -> PermissionState,
    ) -> Option<Observation> {
        if self.startup {
            self.startup = false;
            self.last = Observation {
                firewall: firewall(),
                system_service: service(),
            };
            return Some(self.last);
        }
        if policy_changed {
            // A single policy operation may write several values/stores. Fold
            // that notification burst into one query after the last signal.
            self.changed_at = Some(now + std::time::Duration::from_millis(250));
        }
        if self.changed_at.is_some_and(|at| now >= at) {
            self.changed_at = None;
            self.last.firewall = firewall();
            return Some(self.last);
        }
        None
    }
}

#[cfg(windows)]
pub(crate) struct Observer {
    notifications: notifications::Notifications,
    sampler: Sampler,
}
#[cfg(windows)]
impl Observer {
    pub(crate) fn new() -> Self {
        Self {
            notifications: notifications::Notifications::new(),
            sampler: Sampler::default(),
        }
    }
    pub(crate) fn sample(&mut self, now: std::time::Instant) -> Option<Observation> {
        self.sampler.sample(
            now,
            self.notifications.changed(),
            native::firewall_state,
            native::service_state,
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Observation {
    pub firewall: PermissionState,
    pub system_service: PermissionState,
}

impl Observation {
    pub(crate) const UNKNOWN: Self = Self {
        firewall: PermissionState::NotDetermined,
        system_service: PermissionState::NotDetermined,
    };

    pub(crate) fn items(self, inbound_verified: bool) -> Vec<PermissionItem> {
        // Authenticated inbound TCP remains useful evidence for development
        // executables without installer rules. An explicit policy block wins.
        let firewall = match self.firewall {
            PermissionState::NotDetermined | PermissionState::NotGranted if inbound_verified => {
                PermissionState::Granted
            }
            state => state,
        };
        vec![
            PermissionItem {
                kind: PermissionKind::Firewall,
                state: firewall,
                required: true,
            },
            PermissionItem {
                kind: PermissionKind::SystemService,
                state: self.system_service,
                required: false,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    #[ignore = "requires OINK_FIREWALL_CHANGE_PROBE and an external disposable-rule edit"]
    fn native_policy_notification_requeries_once() {
        // This acceptance probe never mutates policy. The external controller
        // changes one disposable rule only after this observer is registered.
        std::thread::Builder::new().name("oink-platform-observer".into()).spawn(|| {
            use std::time::{Duration, Instant};
            let dir = std::path::PathBuf::from(std::env::var_os("OINK_FIREWALL_CHANGE_PROBE").expect("probe handshake directory"));
            let mut observer = Observer::new();
            assert_eq!(observer.notifications.registered_count(), 2, "local and GPO notifications must both register");
            observer.sample(Instant::now()).unwrap();
            assert_eq!(native::FIREWALL_QUERIES.get(), 1);
            assert_eq!(native::SERVICE_QUERIES.get(), 1);
            std::fs::write(dir.join("ready"), b"startup=1 service=1").unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            while native::FIREWALL_QUERIES.get() < 2 && Instant::now() < deadline {
                observer.sample(Instant::now());
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(native::FIREWALL_QUERIES.get(), 2, "rule edit must generate one additional query");
            let quiet_until = Instant::now() + Duration::from_secs(1);
            while Instant::now() < quiet_until {
                observer.sample(Instant::now());
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(native::FIREWALL_QUERIES.get(), 2, "no timer/focus polling or duplicate query after a rule edit");
            assert_eq!(native::SERVICE_QUERIES.get(), 1, "firewall changes must not query SCM");
            println!("native notification probe: stores=2 startup_firewall=1 rule_edit_requeries=1 firewall_total=2 service_total=1 thread={}", std::thread::current().name().unwrap());
        }).unwrap().join().unwrap();
    }

    #[test]
    fn policy_notifications_coalesce_and_only_requery_firewall() {
        use std::{
            cell::Cell,
            time::{Duration, Instant},
        };
        let mut sampler = Sampler::default();
        let firewall = Cell::new(0);
        let service = Cell::new(0);
        let start = Instant::now();
        let mut sample = |millis, changed| {
            sampler.sample(
                start + Duration::from_millis(millis),
                changed,
                || {
                    firewall.set(firewall.get() + 1);
                    if firewall.get() == 1 {
                        PermissionState::Granted
                    } else {
                        PermissionState::Denied
                    }
                },
                || {
                    service.set(service.get() + 1);
                    PermissionState::Granted
                },
            )
        };
        assert_eq!(sample(0, false).unwrap().firewall, PermissionState::Granted);
        assert!(sample(1000, true).is_none());
        assert!(sample(1100, true).is_none());
        assert!(sample(1349, false).is_none());
        let updated = sample(1350, false).unwrap();
        assert_eq!(updated.firewall, PermissionState::Denied);
        assert_eq!(updated.system_service, PermissionState::Granted);
        assert!(sample(60000, false).is_none());
        assert_eq!(firewall.get(), 2);
        assert_eq!(service.get(), 1, "policy writes must not issue SCM queries");
    }

    #[test]
    fn startup_queries_once_without_polling_or_focus_refresh_retries() {
        use std::{
            cell::Cell,
            time::{Duration, Instant},
        };
        let mut sampler = Sampler::default();
        let firewall = Cell::new(0);
        let service = Cell::new(0);
        let start = Instant::now();
        for elapsed in [0, 1, 2, 60, 3600] {
            sampler.sample(
                start + Duration::from_secs(elapsed),
                false,
                || {
                    firewall.set(firewall.get() + 1);
                    PermissionState::NotDetermined
                },
                || {
                    service.set(service.get() + 1);
                    PermissionState::Granted
                },
            );
        }
        assert_eq!(
            firewall.get(),
            1,
            "elapsed time/focus must not retry a firewall query, even after an unknown result"
        );
        assert_eq!(service.get(), 1, "service status is sampled once per backend lifetime");
    }

    #[test]
    fn inbound_proof_does_not_manufacture_a_service_grant_or_override_a_policy_block() {
        assert_eq!(Observation::UNKNOWN.items(true)[0].state, PermissionState::Granted);
        assert_eq!(
            Observation::UNKNOWN.items(true)[1].state,
            PermissionState::NotDetermined
        );
        let blocked = Observation {
            firewall: PermissionState::Denied,
            ..Observation::UNKNOWN
        };
        assert_eq!(blocked.items(true)[0].state, PermissionState::Denied);
        assert_eq!(
            Observation::UNKNOWN.items(false)[0].state,
            PermissionState::NotDetermined
        );
    }

    #[test]
    fn installer_access_survives_the_last_peer_disconnect() {
        let installed = Observation {
            firewall: PermissionState::Granted,
            system_service: PermissionState::Granted,
        };
        assert_eq!(installed.items(true), installed.items(false));
        let stopped = Observation {
            system_service: PermissionState::NotGranted,
            ..installed
        };
        assert_eq!(stopped.items(false)[1].state, PermissionState::NotGranted);
        assert_eq!(stopped.items(false)[0].state, PermissionState::Granted);
    }

    #[test]
    fn installed_access_is_reported_without_a_peer_connection() {
        let items = Observation {
            firewall: PermissionState::Granted,
            system_service: PermissionState::Granted,
        }
        .items(false);
        assert_eq!(items[0].state, PermissionState::Granted);
        assert!(items[0].required);
        assert_eq!(items[1].state, PermissionState::Granted);
        assert!(!items[1].required);
    }
}
