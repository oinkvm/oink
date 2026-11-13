//! Source-device lease policy. The platform observer and process lock outlive
//! this lease. Storage/ownership rules are shared with Windows' recovery journal.
use crate::recovery::{EndpointBackend, EndpointState, Journal};

pub(crate) struct Lease<B: EndpointBackend> {
    pub(crate) backend: B,
    journal: Journal,
    /// Forwarding only began once the mute (or the user's own mute) was
    /// confirmed; a failed setup is no forwarding end and never pauses media.
    acquired: bool,
    finished: bool,
}
impl<B: EndpointBackend> Lease<B> {
    pub(crate) fn start(mut journal: Journal, backend: B, endpoint: &str) -> Result<Self, String> {
        let before = backend.read(endpoint)?.ok_or("source output unavailable before mute")?;
        journal.prepare(before.clone())?;
        let lease = Self {
            backend,
            journal,
            acquired: false,
            finished: false,
        };
        lease.acquire(before)
    }

    fn acquire(mut self, before: EndpointState) -> Result<Self, String> {
        if self.backend.ownership_lost() || self.backend.read(&before.endpoint)? != Some(before.clone()) {
            self.journal.revoke()?;
            return Err("source controls changed during mute setup; user state takes priority".into());
        }
        // An already-muted device belongs to the user. Never toggle it, even
        // transiently, and preserve that original bit on every release path.
        if !before.muted {
            self.backend.mute(&before.endpoint)?;
        }
        let mut expected = before;
        expected.muted = true;
        let observed = self.backend.read(&expected.endpoint)?;
        if self.backend.ownership_lost() || observed.as_ref().is_some_and(|state| state != &expected) {
            self.journal.revoke()?;
            return Err("source controls changed while muting; user state takes priority".into());
        }
        if observed.is_none() {
            // Disappearance is not a user override. Drop must resolve deletion
            // or retain our original mute for a reconnect during acquisition.
            return Err("source output disappeared while muting".into());
        }
        self.acquired = true;
        Ok(self)
    }

    pub(crate) fn check_ownership(&mut self) -> Result<bool, String> {
        if self.backend.ownership_lost() {
            self.journal.revoke()?;
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn finish(&mut self) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        if let Some(uid) = self.journal.endpoint() {
            // Catch a level change whose HAL notification is still queued.
            // Platform readback may latch ownership loss; it never writes.
            let _ = self.backend.read(uid)?;
        }
        let report = if self.acquired {
            // A real forwarding end: pause (also for a pre-muted output or
            // after a user takeover), then restore only what Oink changed.
            self.journal.end_forwarding(&self.backend)?
        } else {
            // Failed setup: resolve the write-ahead entry without a pause.
            if self.backend.ownership_lost() {
                self.journal.release_after_volume_change(&self.backend)?;
            }
            self.journal.recover_or_defer(&self.backend, false)?
        };
        self.finished = true;
        tracing::info!(?report, "source output mute lease finished");
        Ok(())
    }
}
impl<B: EndpointBackend> Drop for Lease<B> {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            tracing::error!(%error, "source mute restoration incomplete; recovery journal retained");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::RecoveryOutcome;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    struct Fake {
        state: RefCell<Option<EndpointState>>,
        writes: Cell<usize>,
        disappear_on_mute: Cell<bool>,
        removed: Cell<bool>,
        changed: Cell<bool>,
        mute_changed: Cell<bool>,
        events: RefCell<Vec<&'static str>>,
        journal: std::path::PathBuf,
    }
    use crate::recovery::RestorePermit;
    impl EndpointBackend for Rc<Fake> {
        fn permanently_removed(&self, _: &str) -> Result<bool, String> {
            Ok(self.removed.get())
        }
        fn read(&self, id: &str) -> Result<Option<EndpointState>, String> {
            Ok(self.state.borrow().clone().filter(|s| s.endpoint == id))
        }
        fn mute(&self, id: &str) -> Result<(), String> {
            let mute = true;
            let entry: serde_json::Value = serde_json::from_slice(&std::fs::read(&self.journal).unwrap()).unwrap();
            assert_eq!(entry["entry"]["original"]["endpoint"], id);
            assert_eq!(entry["entry"]["owned"], true, "write-ahead before OS change");
            self.state.borrow_mut().as_mut().unwrap().muted = mute;
            self.writes.set(self.writes.get() + 1);
            if mute && self.disappear_on_mute.get() {
                self.state.borrow_mut().take();
            }
            Ok(())
        }
        fn ownership_lost(&self) -> bool {
            self.changed.get()
        }
        fn mute_ownership_lost(&self) -> bool {
            self.mute_changed.get()
        }
        fn pause_before_restore(&self) {
            self.events.borrow_mut().push("pause");
        }
        fn restore_mute(&self, _: &str, release_only: bool, _: &RestorePermit) -> Result<(), String> {
            self.events.borrow_mut().push("restore");
            assert!(!self.mute_changed.get());
            assert!(release_only || !self.changed.get());
            self.state.borrow_mut().as_mut().unwrap().muted = false;
            self.writes.set(self.writes.get() + 1);
            Ok(())
        }
    }
    fn setup(muted: bool) -> (tempfile::TempDir, Rc<Fake>) {
        let dir = tempfile::tempdir().unwrap();
        let backend = Rc::new(Fake {
            state: RefCell::new(Some(EndpointState {
                endpoint: "test-output".into(),
                muted,
                volume: 0.375,
            })),
            writes: Cell::new(0),
            disappear_on_mute: Cell::new(false),
            removed: Cell::new(false),
            changed: Cell::new(false),
            mute_changed: Cell::new(false),
            events: RefCell::new(Vec::new()),
            journal: dir.path().join("audio-mute-recovery.json"),
        });
        (dir, backend)
    }
    fn start(dir: &std::path::Path, backend: &Rc<Fake>) -> Lease<Rc<Fake>> {
        Lease::start(Journal::open(dir).unwrap(), backend.clone(), "test-output").unwrap()
    }
    #[test]
    fn explicit_finish_and_drop_restore_only_original_mute() {
        for explicit_finish in [false, true] {
            let (dir, b) = setup(false);
            let mut lease = start(dir.path(), &b);
            assert!(b.state.borrow().as_ref().unwrap().muted);
            assert!(Journal::open(dir.path()).is_err());
            if explicit_finish {
                lease.finish().unwrap();
            }
            drop(lease);
            assert_eq!(b.state.borrow().as_ref().unwrap().volume, 0.375);
            assert!(!b.state.borrow().as_ref().unwrap().muted);
            assert_eq!(b.writes.get(), 2);
        }
    }
    // Field case (a Windows test machine, 0.4.0): the output was muted by the user before
    // forwarding started, so Oink owned no mute and the end of forwarding
    // restored nothing and paused nothing; the playing app kept playing.
    #[test]
    fn forwarding_end_pauses_even_when_the_output_was_already_muted() {
        let (dir, b) = setup(true);
        drop(start(dir.path(), &b));
        assert_eq!(*b.events.borrow(), ["pause"]);
        assert!(b.state.borrow().as_ref().unwrap().muted, "the user's mute stays");
        assert_eq!(b.writes.get(), 0);
    }
    #[test]
    fn forwarding_end_pauses_once_before_restoring_our_mute() {
        let (dir, b) = setup(false);
        drop(start(dir.path(), &b));
        assert_eq!(*b.events.borrow(), ["pause", "restore"]);
    }
    #[test]
    fn user_control_change_during_forwarding_still_pauses_at_the_end() {
        for mute_toggled in [false, true] {
            let (dir, b) = setup(false);
            let mut lease = start(dir.path(), &b);
            b.state.borrow_mut().as_mut().unwrap().volume = 0.75;
            b.changed.set(true);
            b.mute_changed.set(mute_toggled);
            assert!(lease.check_ownership().unwrap());
            drop(lease);
            let restored: &[&str] = if mute_toggled { &[] } else { &["restore"] };
            let expected: Vec<_> = ["pause"].iter().chain(restored).copied().collect();
            assert_eq!(*b.events.borrow(), expected, "mute toggled {mute_toggled}");
        }
    }
    #[test]
    fn failed_mute_setup_is_not_a_forwarding_end_and_never_pauses() {
        let (dir, b) = setup(false);
        b.disappear_on_mute.set(true);
        assert!(Lease::start(Journal::open(dir.path()).unwrap(), b.clone(), "test-output").is_err());
        assert!(b.events.borrow().is_empty());
    }
    #[test]
    fn already_muted_is_never_written_or_unmuted() {
        let (dir, b) = setup(true);
        drop(start(dir.path(), &b));
        assert!(b.state.borrow().as_ref().unwrap().muted);
        assert_eq!(b.writes.get(), 0);
    }
    #[test]
    fn volume_takeover_releases_our_mute_without_restoring_volume() {
        let (dir, b) = setup(false);
        let mut lease = start(dir.path(), &b);
        b.state.borrow_mut().as_mut().unwrap().volume = 0.75;
        b.changed.set(true);
        assert!(lease.check_ownership().unwrap());
        lease.finish().unwrap();
        assert_eq!(b.state.borrow().as_ref().unwrap().volume, 0.75);
        assert!(!b.state.borrow().as_ref().unwrap().muted);
    }
    #[test]
    fn user_mute_toggle_back_still_revokes_restore() {
        let (dir, b) = setup(false);
        let lease = start(dir.path(), &b);
        b.changed.set(true);
        b.mute_changed.set(true);
        drop(lease);
        assert!(b.state.borrow().as_ref().unwrap().muted);
        assert_eq!(b.writes.get(), 1);
    }
    #[test]
    fn disappearance_during_mute_setup_is_not_mistaken_for_user_takeover() {
        for removed in [false, true] {
            let (dir, b) = setup(false);
            b.disappear_on_mute.set(true);
            b.removed.set(removed);
            let error = Lease::start(Journal::open(dir.path()).unwrap(), b.clone(), "test-output")
                .err()
                .unwrap();
            assert!(error.contains("disappeared"));
            let journal = Journal::open(dir.path()).unwrap();
            assert_eq!(journal.has_pending_restore(), !removed);
            assert!(
                !journal.has_active_pending_restore(),
                "unavailable endpoints are deferred rather than a cleanup fault"
            );
        }
    }
    #[test]
    fn removed_endpoint_finishes_lease_and_clears_durable_ownership() {
        let (dir, b) = setup(false);
        let mut lease = start(dir.path(), &b);
        b.state.borrow_mut().take();
        b.removed.set(true);
        lease.finish().unwrap();
        drop(lease);
        assert!(!Journal::open(dir.path()).unwrap().has_pending_restore());
        assert_eq!(b.writes.get(), 1);
    }
    #[test]
    fn missing_old_device_retains_recovery_and_never_changes_replacement() {
        let (dir, b) = setup(false);
        let mut lease = start(dir.path(), &b);
        let old = b.state.borrow_mut().take().unwrap();
        lease.finish().unwrap();
        drop(lease);
        let mut journal = Journal::open(dir.path()).unwrap();
        assert!(journal.has_pending_restore());
        assert_eq!(
            journal.recover(&b, true).unwrap().outcome,
            RecoveryOutcome::EndpointUnavailable
        );
        *b.state.borrow_mut() = Some(old);
        let report = journal.recover(&b, true).unwrap();
        assert_eq!(report.outcome, RecoveryOutcome::Restored);
        assert!(report.crash_intent_uncertain);
        assert!(!b.state.borrow().as_ref().unwrap().muted);
    }
}
