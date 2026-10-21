//! SYSTEM-owned update service and protected short-lived transaction worker.
mod cabinet;
mod msi;
mod pipe;
pub(super) mod protected;
mod service;
mod signature;
mod task;
mod worker;
use oink_update::{
    Error,
    feed::{Release, Watermark},
    policy::*,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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
    transaction: String,
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
pub(super) fn pending() -> bool {
    protected::maintenance()
        .map(|p| p.join("pending.json").exists())
        .unwrap_or(true)
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

pub(super) fn input_allowed() -> bool {
    if !pending() {
        return true;
    }
    let check = || -> Result<bool, Error> {
        let journal = oink_update::transaction::Journal::parse(&protected::read(
            &protected::maintenance()
                .map_err(|_| Error::Storage)?
                .join("pending.json"),
            65536,
        )?)?;
        Ok(transaction_dir(&journal.transaction)?
            .join("health-authorized.json")
            .exists())
    };
    check().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
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
                    serde_json::json!({"checks_enabled": checks_enabled})
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
