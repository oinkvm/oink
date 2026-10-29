//! Journal-before-effect transaction engine. Platform operations must hold the
//! installer maintenance lock and operate only on protected, verified handles.
use crate::{Error, feed::strict_json};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Prepared,
    Stopping,
    Replacing,
    Starting,
    AwaitingReboot,
    RollbackAwaitingReboot,
    HealthCheck,
    Committed,
    Recovering,
    RolledBack,
    RepairRequired,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Journal {
    pub schema: u32,
    pub transaction: String,
    pub old_build: u64,
    pub new_build: u64,
    pub package_sha256: String,
    pub stage: Stage,
    pub rollback_attempted: bool,
    pub failure: Option<Error>,
}
impl Journal {
    pub fn new(transaction: String, old_build: u64, new_build: u64, package_sha256: String) -> Result<Self, Error> {
        let journal = Self {
            schema: 1,
            transaction,
            old_build,
            new_build,
            package_sha256,
            stage: Stage::Prepared,
            rollback_attempted: false,
            failure: None,
        };
        journal.validate()?;
        Ok(journal)
    }
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > 65536 {
            return Err(Error::RepairRequired);
        }
        let journal: Self = strict_json(bytes).map_err(|_| Error::RepairRequired)?;
        journal.validate()?;
        Ok(journal)
    }
    fn validate(&self) -> Result<(), Error> {
        let hex = |s: &str, n| s.len() == n && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if self.schema != 1
            || !hex(&self.transaction, 32)
            || !hex(&self.package_sha256, 64)
            || self.old_build == 0
            || self.new_build < self.old_build
        {
            return Err(Error::RepairRequired);
        }
        Ok(())
    }
}
/// An implementation must independently revalidate package/backup/ACL/signatures,
/// authenticate SYSTEM, and establish exclusive maintenance before returning.
/// No methods take paths, service names, command lines, tokens, or network URLs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    RebootRequired,
    Busy,
}
pub trait Platform {
    fn verify_and_lock(&mut self, journal: &Journal) -> Result<(), Error>;
    /// Atomic protected replacement plus FlushFileBuffers, including parent metadata.
    fn persist(&mut self, journal: &Journal) -> Result<(), Error>;
    fn arm_recovery(&mut self) -> Result<(), Error>;
    /// Require fresh authenticated backend ACKs and copied-old-worker handover.
    fn confirm_drain_and_handover(&mut self) -> Result<(), Error>;
    fn stop_owned_processes(&mut self) -> Result<(), Error>;
    fn replace_components_and_metadata(&mut self) -> Result<InstallOutcome, Error>;
    fn reboot_observed(&mut self) -> Result<bool, Error>;
    fn start_service_under_maintenance(&mut self) -> Result<(), Error>;
    fn health_check_original_sessions(&mut self) -> Result<(), Error>;
    /// Durable receipt and successful-build high watermark, after health checks.
    fn commit_receipt(&mut self) -> Result<(), Error>;
    fn restore_previous_components_and_metadata(&mut self) -> Result<InstallOutcome, Error>;
    fn resume_before_replacement(&mut self) -> Result<(), Error>;
    fn health_check_previous(&mut self) -> Result<(), Error>;
    fn quarantine_candidate(&mut self) -> Result<(), Error>;
    fn restore_failure_policy_and_cleanup(&mut self) -> Result<(), Error>;
}

pub fn execute(platform: &mut impl Platform, journal: &mut Journal) -> Result<(), Error> {
    journal.validate()?;
    platform.verify_and_lock(journal)?;
    if journal.stage == Stage::AwaitingReboot {
        if !platform.reboot_observed()? {
            return Ok(());
        }
        advance(platform, journal, Stage::Starting)?;
        platform.start_service_under_maintenance()?;
        advance(platform, journal, Stage::HealthCheck)?;
        if platform.health_check_original_sessions().is_err() {
            return recover_locked(platform, journal);
        }
        platform.commit_receipt()?;
        advance(platform, journal, Stage::Committed)?;
        return platform.restore_failure_policy_and_cleanup();
    }
    if journal.stage != Stage::Prepared {
        return recover_locked(platform, journal);
    }
    platform.persist(journal)?;
    if let Err(error) = platform
        .arm_recovery()
        .and_then(|()| platform.confirm_drain_and_handover())
    {
        return finish_failure(platform, journal, error);
    }
    let result = (|| {
        advance(platform, journal, Stage::Stopping)?;
        if let Err(error) = platform.stop_owned_processes() {
            platform.resume_before_replacement()?;
            return finish_failure(platform, journal, error);
        }
        advance(platform, journal, Stage::Replacing)?;
        match platform.replace_components_and_metadata()? {
            InstallOutcome::RebootRequired => {
                advance(platform, journal, Stage::AwaitingReboot)?;
                return Ok(());
            }
            InstallOutcome::Busy => {
                // 1618 made no package change. Restore the previous live session
                // and report one failed attempt, without a background MSI retry.
                platform.resume_before_replacement()?;
                return finish_failure(platform, journal, Error::Busy);
            }
            InstallOutcome::Installed => {}
        }
        advance(platform, journal, Stage::Starting)?;
        platform.start_service_under_maintenance()?;
        advance(platform, journal, Stage::HealthCheck)?;
        platform.health_check_original_sessions()?;
        // Commit marker is last. Recovery treats a crash before it as uncommitted,
        // even if the candidate receipt was already written.
        platform.commit_receipt()?;
        advance(platform, journal, Stage::Committed)
    })();
    if let Err(error) = result {
        if journal.stage == Stage::RolledBack {
            return Err(error);
        }
        journal.failure = Some(error);
        platform.persist(journal)?;
        return recover_locked(platform, journal);
    }
    if journal.stage == Stage::AwaitingReboot {
        return Ok(());
    }
    platform.restore_failure_policy_and_cleanup()
}
pub fn recover(platform: &mut impl Platform, journal: &mut Journal) -> Result<(), Error> {
    journal.validate()?;
    platform.verify_and_lock(journal)?;
    recover_locked(platform, journal)
}
fn advance(platform: &mut impl Platform, journal: &mut Journal, stage: Stage) -> Result<(), Error> {
    let mut next = journal.clone();
    next.stage = stage;
    platform.persist(&next)?;
    *journal = next;
    Ok(())
}
fn finish_failure(platform: &mut impl Platform, journal: &mut Journal, error: Error) -> Result<(), Error> {
    journal.failure = Some(error);
    advance(platform, journal, Stage::RolledBack)?;
    platform.restore_failure_policy_and_cleanup()?;
    Err(error)
}
fn recover_locked(platform: &mut impl Platform, journal: &mut Journal) -> Result<(), Error> {
    if matches!(journal.stage, Stage::Committed | Stage::RolledBack) {
        return platform.restore_failure_policy_and_cleanup();
    }
    if journal.stage == Stage::RepairRequired {
        return Err(Error::RepairRequired);
    }
    if journal.stage == Stage::Prepared {
        // No product effect occurred. Do not shut down a user who never ACKed.
        advance(platform, journal, Stage::RolledBack)?;
        return platform.restore_failure_policy_and_cleanup();
    }
    if journal.stage == Stage::RollbackAwaitingReboot && !platform.reboot_observed()? {
        return Ok(());
    }
    // Recovering is resumable after power loss; rollback is idempotent and targets
    // only this transaction's recorded previous successful build. It cannot loop
    // back into candidate installation.
    let result = (|| {
        if journal.stage != Stage::HealthCheck || !journal.rollback_attempted {
            let mut next = journal.clone();
            next.rollback_attempted = true;
            next.stage = Stage::Recovering;
            platform.persist(&next)?;
            *journal = next;
            platform.stop_owned_processes()?;
            match platform.restore_previous_components_and_metadata()? {
                InstallOutcome::Busy => {
                    // Preserve an actual failed recovery for explicit repair;
                    // never retry an installation in the background.
                    return Err(Error::Busy);
                }
                InstallOutcome::RebootRequired => {
                    advance(platform, journal, Stage::RollbackAwaitingReboot)?;
                    return Ok(());
                }
                InstallOutcome::Installed => {}
            }
            // Recovering instructs backends to drain and exit. Persist the
            // health phase before relaunch so the old session can report health
            // instead of shutting itself down again. Resume here after a crash.
            advance(platform, journal, Stage::HealthCheck)?;
        }
        platform.health_check_previous()?;
        platform.quarantine_candidate()?;
        advance(platform, journal, Stage::RolledBack)?;
        platform.restore_failure_policy_and_cleanup()
    })();
    if result.is_err() {
        journal.failure = Some(Error::RepairRequired);
        let _ = advance(platform, journal, Stage::RepairRequired);
        return Err(Error::RepairRequired);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        events: Vec<&'static str>,
        fail: Option<&'static str>,
        saved: Option<Journal>,
        install_outcome: Option<InstallOutcome>,
        rollback_outcome: Option<InstallOutcome>,
        rebooted: bool,
        require_old_health_phase: bool,
    }
    impl Fake {
        fn action(&mut self, name: &'static str) -> Result<(), Error> {
            self.events.push(name);
            if self.fail == Some(name) {
                self.fail = None;
                Err(Error::Transaction)
            } else {
                Ok(())
            }
        }
    }
    impl Platform for Fake {
        fn verify_and_lock(&mut self, _: &Journal) -> Result<(), Error> {
            self.action("verify")
        }
        fn persist(&mut self, j: &Journal) -> Result<(), Error> {
            self.action("flush")?;
            self.saved = Some(j.clone());
            Ok(())
        }
        fn arm_recovery(&mut self) -> Result<(), Error> {
            self.action("arm")
        }
        fn confirm_drain_and_handover(&mut self) -> Result<(), Error> {
            self.action("drain")
        }
        fn stop_owned_processes(&mut self) -> Result<(), Error> {
            self.action("stop")
        }
        fn replace_components_and_metadata(&mut self) -> Result<InstallOutcome, Error> {
            assert_eq!(self.saved.as_ref().unwrap().stage, Stage::Replacing);
            self.action("replace")?;
            Ok(self.install_outcome.unwrap_or(InstallOutcome::Installed))
        }
        fn reboot_observed(&mut self) -> Result<bool, Error> {
            Ok(self.rebooted)
        }
        fn start_service_under_maintenance(&mut self) -> Result<(), Error> {
            self.action("start")
        }
        fn health_check_original_sessions(&mut self) -> Result<(), Error> {
            self.action("health")
        }
        fn commit_receipt(&mut self) -> Result<(), Error> {
            assert_eq!(self.saved.as_ref().unwrap().stage, Stage::HealthCheck);
            self.action("commit")
        }
        fn restore_previous_components_and_metadata(&mut self) -> Result<InstallOutcome, Error> {
            self.action("restore")?;
            Ok(self.rollback_outcome.unwrap_or(InstallOutcome::Installed))
        }
        fn resume_before_replacement(&mut self) -> Result<(), Error> {
            self.action("resume")
        }
        fn health_check_previous(&mut self) -> Result<(), Error> {
            if self.require_old_health_phase && self.saved.as_ref().unwrap().stage != Stage::HealthCheck {
                // During drain the service asks the backend to exit, not prove health.
                return Err(Error::Transaction);
            }
            self.action("old_health")
        }
        fn quarantine_candidate(&mut self) -> Result<(), Error> {
            self.action("quarantine")
        }
        fn restore_failure_policy_and_cleanup(&mut self) -> Result<(), Error> {
            self.action("cleanup")
        }
    }
    fn journal() -> Journal {
        Journal::new("01".repeat(16), 1, 2, "ab".repeat(32)).unwrap()
    }
    #[test]
    fn equal_build_repair_is_a_valid_journal_but_downgrade_is_not() {
        assert!(Journal::new("a".repeat(32), 30, 30, "b".repeat(64)).is_ok());
        assert!(Journal::new("a".repeat(32), 30, 29, "b".repeat(64)).is_err());
    }
    #[test]
    fn commit_only_after_health_and_all_boundaries_are_flushed() {
        let mut p = Fake::default();
        let mut j = journal();
        execute(&mut p, &mut j).unwrap();
        assert_eq!(j.stage, Stage::Committed);
        assert_eq!(
            p.events,
            [
                "verify", "flush", "arm", "drain", "flush", "stop", "flush", "replace", "flush", "start", "flush",
                "health", "commit", "flush", "cleanup"
            ]
        );
    }
    #[test]
    fn busy_never_stops_product() {
        let mut p = Fake {
            fail: Some("drain"),
            ..Default::default()
        };
        assert!(execute(&mut p, &mut journal()).is_err());
        assert!(!p.events.contains(&"stop"));
    }
    #[test]
    fn every_failure_recovers_exact_old_set() {
        for point in ["replace", "start", "health", "commit"] {
            let mut p = Fake {
                fail: Some(point),
                ..Default::default()
            };
            let mut j = journal();
            execute(&mut p, &mut j).unwrap();
            assert_eq!(j.stage, Stage::RolledBack);
            assert_eq!(p.events.iter().filter(|x| **x == "restore").count(), 1);
            assert!(p.events.contains(&"quarantine"));
        }
    }
    #[test]
    fn pre_replacement_msi_failure_restarts_old_session_without_repair() {
        let mut p = Fake {
            fail: Some("replace"),
            require_old_health_phase: true,
            ..Default::default()
        };
        let mut j = journal();
        execute(&mut p, &mut j).unwrap();
        assert_eq!(j.stage, Stage::RolledBack);
        assert_eq!(j.failure, Some(Error::Transaction));
        assert!(p.events.contains(&"old_health"));
        assert!(p.events.contains(&"cleanup"));
        assert_eq!(p.events.iter().filter(|event| **event == "replace").count(), 1);
    }
    #[test]
    fn interrupted_old_health_resumes_without_draining_the_restarted_session() {
        let mut j = journal();
        j.stage = Stage::HealthCheck;
        j.rollback_attempted = true;
        j.failure = Some(Error::Transaction);
        let mut p = Fake {
            require_old_health_phase: true,
            saved: Some(j.clone()),
            ..Default::default()
        };
        recover(&mut p, &mut j).unwrap();
        assert_eq!(j.stage, Stage::RolledBack);
        assert!(!p.events.contains(&"stop"));
        assert!(!p.events.contains(&"restore"));
        assert!(p.events.contains(&"old_health"));
        assert!(p.events.contains(&"cleanup"));
    }
    #[test]
    fn power_loss_recovery_is_idempotent_for_every_pending_stage() {
        for stage in [
            Stage::Prepared,
            Stage::Stopping,
            Stage::Replacing,
            Stage::Starting,
            Stage::HealthCheck,
            Stage::Recovering,
        ] {
            let mut j = journal();
            j.stage = stage;
            let mut p = Fake::default();
            recover(&mut p, &mut j).unwrap();
            let count = p.events.len();
            recover(&mut p, &mut j).unwrap();
            assert_eq!(&p.events[count..], ["verify", "cleanup"]);
            assert_eq!(j.stage, Stage::RolledBack);
        }
    }
    #[test]
    fn broken_backup_fails_closed_and_cannot_retry_candidate() {
        let mut p = Fake {
            fail: Some("restore"),
            ..Default::default()
        };
        let mut j = journal();
        j.stage = Stage::Replacing;
        assert_eq!(recover(&mut p, &mut j), Err(Error::RepairRequired));
        assert_eq!(j.stage, Stage::RepairRequired);
        assert_eq!(execute(&mut p, &mut j), Err(Error::RepairRequired));
        assert!(!p.events.contains(&"start"));
    }
    #[test]
    fn installer_busy_restores_previous_session_and_fails_without_a_candidate_retry() {
        let mut p = Fake {
            install_outcome: Some(InstallOutcome::Busy),
            ..Default::default()
        };
        let mut j = journal();
        assert_eq!(execute(&mut p, &mut j), Err(Error::Busy));
        assert_eq!(j.stage, Stage::RolledBack);
        assert_eq!(j.failure, Some(Error::Busy));
        assert!(!p.events.contains(&"health"));
        assert!(!p.events.contains(&"quarantine"));
        assert!(p.events.contains(&"resume"));
        assert!(p.events.contains(&"cleanup"));
        assert_eq!(p.events.iter().filter(|event| **event == "replace").count(), 1);
        p.events.clear();
        execute(&mut p, &mut j).unwrap();
        assert_eq!(p.events, ["verify", "cleanup"]);
        assert_eq!(j.stage, Stage::RolledBack);
    }
    #[test]
    fn reboot_required_keeps_pending_and_cannot_commit_before_a_new_boot() {
        let mut p = Fake {
            install_outcome: Some(InstallOutcome::RebootRequired),
            ..Default::default()
        };
        let mut j = journal();
        execute(&mut p, &mut j).unwrap();
        assert_eq!(j.stage, Stage::AwaitingReboot);
        assert!(!p.events.contains(&"commit"));
        p.events.clear();
        execute(&mut p, &mut j).unwrap();
        assert_eq!(p.events, ["verify"]);
        p.rebooted = true;
        execute(&mut p, &mut j).unwrap();
        assert_eq!(j.stage, Stage::Committed);
        assert!(!p.events.contains(&"replace"));
    }
    #[test]
    fn rollback_busy_requires_repair_and_reboot_remains_explicit() {
        for outcome in [InstallOutcome::Busy, InstallOutcome::RebootRequired] {
            let mut p = Fake {
                rollback_outcome: Some(outcome),
                ..Default::default()
            };
            let mut j = journal();
            j.stage = Stage::Replacing;
            let result = recover(&mut p, &mut j);
            assert!(!p.events.contains(&"cleanup"));
            assert!(!p.events.contains(&"quarantine"));
            if outcome == InstallOutcome::RebootRequired {
                result.unwrap();
                assert_eq!(j.stage, Stage::RollbackAwaitingReboot);
                p.events.clear();
                recover(&mut p, &mut j).unwrap();
                assert_eq!(p.events, ["verify"]);
            } else {
                assert_eq!(result, Err(Error::RepairRequired));
                assert_eq!(j.stage, Stage::RepairRequired);
                p.events.clear();
                assert_eq!(recover(&mut p, &mut j), Err(Error::RepairRequired));
                assert_eq!(p.events, ["verify"]);
                continue;
            }
            p.rebooted = true;
            p.rollback_outcome = None;
            recover(&mut p, &mut j).unwrap();
            assert_eq!(j.stage, Stage::RolledBack);
            assert!(p.events.contains(&"old_health"));
        }
    }
}
