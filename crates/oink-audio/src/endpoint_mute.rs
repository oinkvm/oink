//! Source-device lease policy. The platform observer and process lock outlive
//! this lease. Storage/ownership rules are shared with Windows' recovery journal.
use crate::recovery::{EndpointBackend, EndpointState, Journal, RecoveryOutcome};

pub(crate) struct Lease<B: EndpointBackend> {
    pub(crate) backend: B,
    journal: Journal,
    finished: bool,
}
impl<B: EndpointBackend> Lease<B> {
    pub(crate) fn start(mut journal: Journal, backend: B, endpoint: &str) -> Result<Self, String> {
        let before = backend.read(endpoint)?.ok_or("source output unavailable before mute")?;
        journal.prepare(before.clone())?;
        let lease = Self {
            backend,
            journal,
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
            self.backend.set_mute(&before.endpoint, true)?;
        }
        let mut expected = before;
        expected.muted = true;
        if self.backend.ownership_lost() || self.backend.read(&expected.endpoint)? != Some(expected) {
            self.journal.revoke()?;
            return Err("source controls changed while muting; user state takes priority".into());
        }
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
        if self.backend.ownership_lost() {
            // Match Windows: a level-only change releases our mute without
            // writing the user's new level; a mute override is never undone.
            self.journal.release_after_volume_change(&self.backend)?;
        }
        let report = self.journal.recover(&self.backend, false)?;
        if report.outcome == RecoveryOutcome::UserOverride {
            // Also cover ownership discovered by recover's final readback.
            self.journal.release_after_volume_change(&self.backend)?;
        }
        if report.outcome == RecoveryOutcome::EndpointUnavailable {
            return Err("muted source output unavailable; recovery journal retained for next start".into());
        }
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
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    struct Fake {
        state: RefCell<Option<EndpointState>>,
        writes: Cell<usize>,
        changed: Cell<bool>,
        mute_changed: Cell<bool>,
        journal: std::path::PathBuf,
    }
    impl EndpointBackend for Rc<Fake> {
        fn read(&self, id: &str) -> Result<Option<EndpointState>, String> {
            Ok(self.state.borrow().clone().filter(|s| s.endpoint == id))
        }
        fn set_mute(&self, id: &str, mute: bool) -> Result<(), String> {
            let entry: serde_json::Value = serde_json::from_slice(&std::fs::read(&self.journal).unwrap()).unwrap();
            assert_eq!(entry["entry"]["original"]["endpoint"], id);
            assert_eq!(entry["entry"]["owned"], true, "write-ahead before OS change");
            self.state.borrow_mut().as_mut().unwrap().muted = mute;
            self.writes.set(self.writes.get() + 1);
            Ok(())
        }
        fn ownership_lost(&self) -> bool {
            self.changed.get()
        }
        fn mute_ownership_lost(&self) -> bool {
            self.mute_changed.get()
        }
        fn release_owned_mute(&self, _: &str) -> Result<(), String> {
            assert!(!self.mute_changed.get());
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
            changed: Cell::new(false),
            mute_changed: Cell::new(false),
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
    fn missing_old_device_retains_recovery_and_never_changes_replacement() {
        let (dir, b) = setup(false);
        let mut lease = start(dir.path(), &b);
        let old = b.state.borrow_mut().take().unwrap();
        assert!(lease.finish().is_err());
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
