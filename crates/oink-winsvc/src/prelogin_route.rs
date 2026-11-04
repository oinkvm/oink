//! Broker-issued identity for the separate broker-authorized input endpoint.
//! None of these fields are accepted from an input client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Identity {
    pub pid: u32,
    pub session: u32,
    pub created: u64,
    pub parent: u32,
    pub parent_created: u64,
}
impl Identity {
    pub(crate) fn accepts(self, actual: Self, system: bool, protected: bool, alive: bool) -> bool {
        system
            && protected
            && alive
            && self == actual
            && self.pid != 0
            && self.session != 0
            && self.session != u32::MAX
            && self.parent != 0
            && self.parent_created < self.created
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_broker_pinned_system_child_can_use_the_route() {
        let expected = Identity {
            pid: 42,
            session: 1,
            created: 20,
            parent: 7,
            parent_created: 10,
        };
        assert!(expected.accepts(expected, true, true, true));
        for (system, protected, alive) in [(false, true, true), (true, false, true), (true, true, false)] {
            assert!(!expected.accepts(expected, system, protected, alive));
        }
        for other in [
            Identity { pid: 43, ..expected }, // another SYSTEM process
            Identity { session: 2, ..expected },
            Identity {
                created: 21,
                ..expected
            }, // reused PID
            Identity { parent: 8, ..expected },
            Identity {
                parent_created: 11,
                ..expected
            }, // restarted service
        ] {
            assert!(!expected.accepts(other, true, true, true));
        }
    }
}

/// Minted only from a service-owned process handle. User sources additionally
/// retain their original logon authentication ID (session reuse is not authority).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Source {
    pub identity: Identity,
    pub user_sid: Option<Vec<u8>>,
    pub logon: Option<u64>,
    pub owner_sid: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub session: u32,
    pub source: Option<Source>,
    pub user_sid: Option<Vec<u8>>,
}
impl Target {
    pub(crate) fn valid(&self) -> bool {
        self.session != 0 && self.session != u32::MAX && (self.source.is_some() || self.user_sid.is_some())
    }
}
pub(crate) trait Helper {
    fn target(&self) -> &Target;
    fn alive(&self) -> bool;
    fn stop(&mut self) -> std::io::Result<()>;
    /// Positive OS proof that both the process and its entire session are gone.
    fn session_ended(&self) -> bool {
        false
    }
}
#[derive(Default)]
pub(crate) struct Route<C> {
    child: Option<C>,
    failed: bool,
}
impl<C: Helper> Route<C> {
    pub(crate) fn new() -> Self {
        Self {
            child: None,
            failed: false,
        }
    }
    pub(crate) fn update(
        &mut self,
        desired: Option<Target>,
        start: impl FnOnce(Target) -> std::io::Result<C>,
    ) -> std::io::Result<()> {
        if self.failed {
            // Logoff may terminate a SYSTEM helper with the whole session,
            // before it can ACK cleanup. The failure belongs to that session,
            // not every future login. A console switch or process exit alone
            // is insufficient: disconnected sessions still own desktops/holds.
            if !self.child.as_ref().is_some_and(Helper::session_ended) {
                return Ok(());
            }
            self.child = None;
            self.failed = false;
        }
        let desired = desired.filter(Target::valid);
        if self
            .child
            .as_ref()
            .is_some_and(|c| c.alive() && Some(c.target()) == desired.as_ref())
        {
            return Ok(());
        }
        if let Some(old) = &mut self.child
            && let Err(error) = old.stop()
        {
            self.failed = true;
            return Err(error);
        }
        self.child = None;
        if let Some(target) = desired {
            self.child = Some(start(target)?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    struct Fake {
        target: Target,
        log: Rc<RefCell<Vec<String>>>,
        fail: bool,
    }
    impl Helper for Fake {
        fn target(&self) -> &Target {
            &self.target
        }
        fn alive(&self) -> bool {
            true
        }
        fn stop(&mut self) -> std::io::Result<()> {
            self.log.borrow_mut().push(format!("release {}", self.target.session));
            if self.fail {
                Err(std::io::Error::other("release failed"))
            } else {
                Ok(())
            }
        }
    }
    fn target(session: u32) -> Target {
        Target {
            session,
            source: Some(Source {
                identity: Identity {
                    pid: 42,
                    session: 1,
                    created: 20,
                    parent: 7,
                    parent_created: 10,
                },
                user_sid: None,
                logon: None,
                owner_sid: vec![],
            }),
            user_sid: None,
        }
    }
    #[test]
    fn new_console_helper_is_created_only_after_old_release_confirmation() {
        for fail in [false, true] {
            let log = Rc::new(RefCell::new(Vec::new()));
            let create = |target: Target| {
                log.borrow_mut().push(format!("start {}", target.session));
                Ok(Fake {
                    target,
                    log: log.clone(),
                    fail,
                })
            };
            let mut route = Route::new();
            route.update(Some(target(1)), create).unwrap();
            assert_eq!(route.update(Some(target(2)), create).is_err(), fail);
            if fail {
                route.update(Some(target(2)), create).unwrap();
                assert_eq!(*log.borrow(), ["start 1", "release 1"]);
            } else {
                assert_eq!(*log.borrow(), ["start 1", "release 1", "start 2"]);
                route.update(None, create).unwrap();
                assert_eq!(*log.borrow(), ["start 1", "release 1", "start 2", "release 2"]);
            }
        }
    }
    #[test]
    fn missing_or_invalid_console_never_creates_a_target() {
        let mut route: Route<Fake> = Route::new();
        for session in [0, u32::MAX] {
            route
                .update(Some(target(session)), |_| panic!("invalid console"))
                .unwrap();
        }
        route.update(None, |_| panic!("no active console")).unwrap();
    }
    #[test]
    fn user_console_temporary_console_and_return_all_release_before_replacement() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut route = Route::new();
        let create = |target: Target| {
            log.borrow_mut().push(format!("start {}", target.session));
            Ok(Fake {
                target,
                log: log.clone(),
                fail: false,
            })
        };
        let own = Target {
            session: 1,
            source: None,
            user_sid: Some(vec![1]),
        };
        let mut temporary = target(2);
        let source = temporary.source.as_mut().unwrap();
        source.user_sid = Some(vec![1]);
        source.owner_sid = vec![1];
        source.logon = Some(99);
        route.update(Some(own.clone()), create).unwrap();
        route.update(Some(temporary), create).unwrap();
        route.update(Some(own), create).unwrap();
        assert_eq!(
            *log.borrow(),
            ["start 1", "release 1", "start 2", "release 2", "start 1"]
        );
    }

    struct LoggedOff {
        target: Target,
        process_exited: Rc<std::cell::Cell<bool>>,
        session_absent: Rc<std::cell::Cell<Option<bool>>>,
    }
    impl Helper for LoggedOff {
        fn target(&self) -> &Target {
            &self.target
        }
        fn alive(&self) -> bool {
            !self.process_exited.get()
        }
        fn stop(&mut self) -> std::io::Result<()> {
            if self.target.session == 1 {
                Err(std::io::Error::other("session teardown terminated helper without ACK"))
            } else {
                Ok(())
            }
        }
        fn session_ended(&self) -> bool {
            self.process_exited.get() && self.session_absent.get() == Some(true)
        }
    }

    #[test]
    fn logout_into_new_tokenless_console_does_not_latch_the_previous_session_failure() {
        let exited = Rc::new(std::cell::Cell::new(false));
        let absent = Rc::new(std::cell::Cell::new(Some(false)));
        let started = Rc::new(RefCell::new(Vec::new()));
        let create = |target: Target| {
            started.borrow_mut().push(target.session);
            Ok(LoggedOff {
                target,
                process_exited: exited.clone(),
                session_absent: absent.clone(),
            })
        };
        let mut route = Route::new();
        let user = Target {
            session: 1,
            source: None,
            user_sid: Some(vec![1]),
        };
        route.update(Some(user), create).unwrap();
        let mut login = target(2);
        login.source.as_mut().unwrap().identity.session = 2;
        assert!(login.user_sid.is_none());
        assert!(route.update(Some(login.clone()), create).is_err());

        // A disconnected/FUS session, a failed WTS query, or a live helper is
        // never evidence of cleanup. Even a different target must stay blocked.
        for (process_exited, session_absent) in [(false, Some(true)), (true, None), (true, Some(false))] {
            exited.set(process_exited);
            absent.set(session_absent);
            route.update(Some(login.clone()), create).unwrap();
            assert_eq!(*started.borrow(), [1]);
        }
        exited.set(true);
        absent.set(Some(true));
        route.update(Some(login), create).unwrap();
        assert_eq!(
            *started.borrow(),
            [1, 2],
            "the destroyed session must not disable the new login screen"
        );

        // The next login still uses normal release-before-replace. Neither the
        // new prelogin final ACK nor the user's own-session helper is latched
        // behind the dead session's failed cleanup.
        let mut waiting_start = target(2);
        waiting_start.source.as_mut().unwrap().identity.session = 2;
        waiting_start.user_sid = Some(vec![1]);
        route.update(Some(waiting_start), create).unwrap();
        route.update(None, create).unwrap();
        let user = Target {
            session: 2,
            source: None,
            user_sid: Some(vec![1]),
        };
        route.update(Some(user), create).unwrap();
        assert_eq!(*started.borrow(), [1, 2, 2, 2]);
    }
}
