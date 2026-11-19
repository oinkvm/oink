//! SYSTEM-owned update service and protected short-lived transaction worker.
mod cabinet;
mod local;
mod msi;
pub use local::{elevate as elevate_local, inspect as inspect_local, spool as spool_local};
mod pipe;
pub(in crate::platform) use pipe::elevated;
pub(super) mod protected;
mod relaunch;
mod service;
mod signature;
mod task;
mod worker;
use oink_update::{
    Error,
    catalog::{self, OsVersion},
    feed::{Channel, Release, Severity, Watermark},
    policy::*,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const ARCH: &str = "x64";
/// v2 replay watermark of one channel's catalog. The v1 "stable" entry stays
/// for older services and is not read here.
fn catalog_watermark(channel: Channel) -> String {
    format!("v2-{}", channel.as_str())
}
fn os_version(windows_build: u32) -> OsVersion {
    OsVersion([10, 0, windows_build])
}
/// Transactions (plan, rollback cache, MSI inspection) record the v1 release
/// shape; a verified v2 catalog release maps onto it field for field.
fn transaction_release(release: &catalog::Release) -> Result<Release, Error> {
    let OsVersion([10, 0, minimum_windows_build]) = OsVersion::parse(&release.minimum_os)? else {
        return Err(Error::Metadata);
    };
    Ok(Release {
        build: release.build,
        version: release.version.clone(),
        tag: release.tag.clone(),
        source_tree_sha256: release.source_tree_sha256.clone(),
        architecture: ARCH.into(),
        minimum_windows_build,
        protocol_baseline: release.protocol_baseline,
        minimum_updater_schema: release.minimum_updater_schema,
        channel: release.channel,
        // How an update is offered comes from the catalog rules, not the release.
        severity: Severity::Normal,
        notes: release.notes.clone(),
        package: release.package.clone(),
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MachinePolicy {
    policy: Policy,
    successful_build: u64,
    watermarks: BTreeMap<String, Watermark>,
    quarantine: BTreeSet<u64>,
}
impl MachinePolicy {
    fn record_success(&mut self, build: u64) {
        self.successful_build = self.successful_build.max(build);
        // Selection already rejects these builds by the successful high-water
        // mark. Keep only failures that could still otherwise be offered.
        self.quarantine.retain(|failed| *failed > self.successful_build);
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    id: u32,
    pid: u32,
    created: u64,
    sid: String,
    build: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthProof {
    session: Session,
    observed_unix: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    schema: u32,
    #[serde(default)]
    local: bool,
    #[serde(default = "yes")]
    rollback_package: bool,
    transaction: String,
    /// Catalog the candidate was selected from; local plans have none.
    #[serde(default)]
    channel: Channel,
    candidate: Release,
    previous: Release,
    candidate_inventory: msi::Inventory,
    previous_inventory: msi::Inventory,
    sessions: Vec<Session>,
    reboot_marker_armed: bool,
    lock_handle: String,
    worker_pid: u32,
    worker_created_filetime: String,
    worker_sha256: String,
}
fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
fn valid_transaction(tx: &str) -> bool {
    tx.len() == 32 && tx.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn transaction_dir(tx: &str) -> Result<std::path::PathBuf, Error> {
    if !valid_transaction(tx) {
        return Err(Error::Transaction);
    }
    Ok(protected::maintenance()
        .map_err(|_| Error::Storage)?
        .join("transactions")
        .join(tx))
}
fn policy() -> Result<MachinePolicy, Error> {
    let path = protected::maintenance()
        .map_err(|_| Error::Storage)?
        .join("policy.json");
    if !path.exists() {
        return Ok(MachinePolicy::default());
    }
    oink_update::feed::strict_json(&protected::read(&path, 65536)?)
}
fn save_policy(policy: &MachinePolicy) -> Result<(), Error> {
    protected::write(
        &protected::maintenance()
            .map_err(|_| Error::Storage)?
            .join("policy.json"),
        &serde_json::to_vec(policy).map_err(|_| Error::Metadata)?,
    )
}
fn installed() -> Result<msi::Receipt, Error> {
    let path = protected::root().map_err(|_| Error::Storage)?.join("installed.json");
    oink_update::feed::strict_json(&protected::read(&path, 65536)?)
}
// Cached observations are for admission/status only. Privileged transaction
// effects always re-read protected records under the maintenance lock. Directory
// rename events invalidate this snapshot, including writes by an older worker.
static OBSERVATION: std::sync::Mutex<Option<Result<Option<oink_update::transaction::Journal>, Error>>> =
    std::sync::Mutex::new(None);
fn invalidate_observation() {
    *OBSERVATION.lock().unwrap() = None;
}
fn journal() -> Result<Option<oink_update::transaction::Journal>, Error> {
    let mut cached = OBSERVATION.lock().unwrap();
    cached
        .get_or_insert_with(|| {
            let path = protected::maintenance()
                .map_err(|_| Error::Storage)?
                .join("pending.json");
            if !path.exists() {
                return Ok(None);
            }
            oink_update::transaction::Journal::parse(&protected::read(&path, 65536)?).map(Some)
        })
        .clone()
}
pub(super) fn pending() -> bool {
    journal().map_or(true, |journal| journal.is_some())
}
pub(super) struct MaintenanceNotifications(super::wake::Directory);
impl MaintenanceNotifications {
    pub(super) fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0.raw()
    }
    pub(super) fn rearm(&mut self) -> std::io::Result<()> {
        self.0.rearm()?;
        invalidate_observation();
        Ok(())
    }
}
pub(super) fn maintenance_notifications() -> std::io::Result<MaintenanceNotifications> {
    let notifications = MaintenanceNotifications(super::wake::Directory::new(&protected::maintenance()?)?);
    invalidate_observation();
    Ok(notifications)
}
pub fn request(request: oink_update::ipc::Request) -> Result<oink_update::ipc::Reply, Error> {
    pipe::request(request)
}
pub(super) fn serve() {
    service::serve();
}
pub fn run_worker() -> Result<(), Error> {
    worker::run()
}

fn committed_build(journal: &oink_update::transaction::Journal, installed_build: u64) -> bool {
    journal.stage == oink_update::transaction::Stage::Committed && installed_build == journal.new_build
}

pub(super) fn completed() -> bool {
    let check = || -> Result<bool, Error> {
        let Some(journal) = journal()? else {
            return Ok(false);
        };
        let installed = installed()?.build;
        Ok(committed_build(&journal, installed)
            || journal.stage == oink_update::transaction::Stage::RolledBack && installed == journal.old_build)
    };
    check().unwrap_or(false)
}

pub(super) fn launch_error() -> Option<&'static str> {
    match journal() {
        Err(_) => Some("oink_update_storage_unsafe"),
        Ok(None) => None,
        Ok(Some(journal)) => match installed() {
            Err(_) => Some("oink_update_storage_unsafe"),
            Ok(receipt)
                if committed_build(&journal, receipt.build)
                    || journal.stage == oink_update::transaction::Stage::RolledBack
                        && receipt.build == journal.old_build =>
            {
                None
            }
            Ok(_) => Some("oink_update_in_progress"),
        },
    }
}

pub(super) fn input_allowed() -> bool {
    if !pending() || completed() {
        return true;
    }
    let check = || -> Result<bool, Error> {
        let journal = journal()?.ok_or(Error::Transaction)?;
        Ok(transaction_dir(&journal.transaction)?
            .join("health-authorized.json")
            .exists())
    };
    check().unwrap_or(false)
}

fn yes() -> bool {
    true
}

#[cfg(test)]
#[path = "tests/catalog.rs"]
mod test_catalog;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_cleanup_requires_commit_and_the_exact_installed_candidate() {
        use oink_update::transaction::{Journal, Stage};
        let mut journal = Journal::new("a".repeat(32), 1, 2, "b".repeat(64)).unwrap();
        for stage in [
            Stage::Prepared,
            Stage::Replacing,
            Stage::HealthCheck,
            Stage::RepairRequired,
            Stage::RolledBack,
            Stage::Committed,
        ] {
            journal.stage = stage;
            assert_eq!(committed_build(&journal, 2), stage == Stage::Committed);
            assert!(!committed_build(&journal, 1));
            assert!(!committed_build(&journal, 3));
        }
    }
    #[test]
    fn catalog_releases_map_onto_the_transaction_release() {
        let catalog = test_catalog::verified(&test_catalog::payload(), Channel::Beta).unwrap();
        let release = &catalog.catalog().releases[1];
        let mapped = transaction_release(release).unwrap();
        assert_eq!(
            (mapped.build, mapped.minimum_windows_build, mapped.architecture.as_str()),
            (release.build, 17763, "x64")
        );
        assert_eq!(
            (&mapped.package, &mapped.notes, mapped.channel),
            (&release.package, &release.notes, release.channel)
        );
        for minimum in ["11.0.22000", "10.1.1"] {
            let mut other = release.clone();
            other.minimum_os = minimum.into();
            assert_eq!(transaction_release(&other), Err(Error::Metadata), "{minimum}");
        }
    }
    #[test]
    fn legacy_machine_policy_preserves_checks_and_stable_watermark() {
        for channel in ["stable", "beta"] {
            for checks_enabled in [false, true] {
                let mut stored = serde_json::to_value(MachinePolicy::default()).unwrap();
                stored["policy"]["preferences"] =
                    serde_json::json!({"checks_enabled": checks_enabled, "channel": channel});
                stored["watermarks"]["stable"] =
                    serde_json::json!({"sequence": 7, "payloadSha256": "stored", "lastVerifiedUnix": 123});
                stored["watermarks"]["beta"] =
                    serde_json::json!({"sequence": 99, "payloadSha256": "ignored", "lastVerifiedUnix": 456});
                let policy: MachinePolicy =
                    oink_update::feed::strict_json(&serde_json::to_vec(&stored).unwrap()).unwrap();
                assert_eq!(policy.policy.preferences.checks_enabled, checks_enabled);
                assert_eq!(policy.watermarks["stable"].sequence, 7);
                let saved = serde_json::to_value(policy).unwrap();
                assert_eq!(
                    saved["policy"]["preferences"],
                    serde_json::json!({"checks_enabled": checks_enabled, "update_channel": null, "install_after_download": true})
                );
            }
        }
    }
    #[test]
    fn successful_update_discards_only_obsolete_quarantine_history() {
        let mut policy = MachinePolicy {
            successful_build: 31,
            quarantine: BTreeSet::from([30, 32, 34]),
            ..Default::default()
        };
        policy.record_success(33);
        assert_eq!(policy.successful_build, 33);
        assert_eq!(policy.quarantine, BTreeSet::from([34]));
        policy.record_success(31); // The successful high-water mark never falls.
        assert_eq!(policy.successful_build, 33);
        assert_eq!(policy.quarantine, BTreeSet::from([34]));
    }
}
