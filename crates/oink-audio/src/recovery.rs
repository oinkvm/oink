//! Durable write-ahead ownership for source endpoint muting on macOS/Windows.
//!
//! Only the mute bit is ever restored. Volume is an ownership check, never a
//! value to write back. An external volume event permanently revokes a lease,
//! even if the user later returns all values to their previous state.
//!
//! The caller must use one fixed per-user directory across process instances.
//! The file lock is held until `Journal` is dropped, including OS restoration.
//! Callback delivery and storage are not an atomic transaction: crash recovery
//! cannot prove that no user action occurred while the process was absent.

use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

const JOURNAL_NAME: &str = "audio-mute-recovery.json";
const LOCK_NAME: &str = "audio-mute-recovery.lock";
const FORMAT_VERSION: u32 = 1;

/// Read-only endpoint snapshot. Volume is the normalized master scalar (0..=1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointState {
    pub endpoint: String,
    pub muted: bool,
    pub volume: f32,
}
impl EndpointState {
    fn validate(&self) -> Result<(), String> {
        if self.endpoint.is_empty() || self.endpoint.len() > 4096 || self.endpoint.contains('\0') {
            return Err("invalid audio endpoint ID in recovery journal".into());
        }
        if !self.volume.is_finite() || !(0.0..=1.0).contains(&self.volume) {
            return Err("invalid endpoint volume in recovery journal".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryOutcome {
    NothingToRecover,
    Restored,
    AlreadyOriginal,
    UserOverride,
    EndpointUnavailable,
    /// The OS no longer has this endpoint or its persisted mute state.
    EndpointRemoved,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub outcome: RecoveryOutcome,
    pub endpoint: Option<String>,
    /// True on a restart recovery that used current values as a best-effort
    /// ownership check. No API exposes events that happened while we were down.
    pub crash_intent_uncertain: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    original: EndpointState,
    expected: EndpointState,
    owned: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    entry: Option<Entry>,
    #[serde(default)]
    deferred: Vec<Entry>,
}
impl Default for Record {
    fn default() -> Self {
        Self {
            version: FORMAT_VERSION,
            entry: None,
            deferred: Vec::new(),
        }
    }
}
impl Record {
    fn validate(&self) -> Result<(), String> {
        if self.version != FORMAT_VERSION {
            return Err("unsupported audio recovery journal version".into());
        }
        let mut endpoints = std::collections::BTreeSet::new();
        for entry in self.entry.iter().chain(&self.deferred) {
            if !endpoints.insert(&entry.original.endpoint) {
                return Err("duplicate endpoint in audio recovery journal".into());
            }
            entry.original.validate()?;
            entry.expected.validate()?;
            if entry.expected.endpoint != entry.original.endpoint
                || !entry.expected.muted
                || entry.expected.volume.to_bits() != entry.original.volume.to_bits()
            {
                return Err("inconsistent audio recovery journal; no endpoint was changed".into());
            }
        }
        Ok(())
    }
}

/// Minimal backend for deterministic recovery tests and platform integration.
/// `set_mute` must distinguish its own notifications where the OS permits it
/// (WASAPI event context; CoreAudio serialized value/readback observation).
/// It must not change volume, default devices, or any other endpoint.
pub trait EndpointBackend {
    /// `None` means the exact endpoint is unavailable; `permanently_removed`
    /// distinguishes deletion from a device that may return.
    fn read(&self, endpoint: &str) -> Result<Option<EndpointState>, String>;
    /// True only when the endpoint and its persisted controls were deleted,
    /// never for disabled, unplugged or not-present devices.
    fn permanently_removed(&self, _endpoint: &str) -> Result<bool, String> {
        Ok(false)
    }
    fn set_mute(&self, endpoint: &str, muted: bool) -> Result<(), String>;
    /// An event-driven latch, not a comparison of current endpoint values.
    fn ownership_lost(&self) -> bool;
    /// True if an external event changed the mute bit, including toggling back.
    /// The default is conservative for backends without this finer observation.
    fn mute_ownership_lost(&self) -> bool {
        self.ownership_lost()
    }
    /// Release only our applied mute after a level-only external change. Must
    /// recheck mute ownership immediately before the tagged OS write.
    fn release_owned_mute(&self, endpoint: &str) -> Result<(), String> {
        self.set_mute(endpoint, false)
    }
}

/// One exclusive process owns the recovery record until this object is dropped.
/// This type deliberately does not restore in Drop: only the platform lease can
/// safely stop IO, consult notification ownership, and perform a tagged write.
pub struct Journal {
    _lock: JournalLock,
    path: PathBuf,
    record: Record,
}

struct JournalLock(File);

impl Drop for JournalLock {
    fn drop(&mut self) {
        // Closing only our descriptor can leave the lock owned by a forked
        // child's inherited reference until exec/exit. CLOEXEC does not cover
        // that interval. End this acquired lock's ownership explicitly, also
        // when opening/validating the record returns an error.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}
impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("path", &self.path)
            .field("record", &self.record)
            .finish()
    }
}
impl Journal {
    /// Nonblocking cross-process lock. Corrupt or unreadable records fail closed.
    pub fn open(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("create audio recovery directory: {e}"))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(LOCK_NAME))
            .map_err(|e| format!("open audio recovery lock: {e}"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .map_err(|e| format!("audio recovery is already owned by another process or lock unavailable: {e}"))?;
        let lock = JournalLock(lock);
        let path = dir.join(JOURNAL_NAME);
        let record: Record = match std::fs::read(&path) {
            Ok(bytes) => {
                if bytes.len() > 32 * 1024 {
                    return Err("audio recovery journal is too large; no endpoint was changed".into());
                }
                serde_json::from_slice(&bytes)
                    .map_err(|e| format!("audio recovery journal is corrupt; no endpoint was changed: {e}"))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Record::default(),
            Err(e) => return Err(format!("read audio recovery journal: {e}")),
        };
        record.validate()?;
        Ok(Self {
            _lock: lock,
            path,
            record,
        })
    }

    pub fn endpoint(&self) -> Option<&str> {
        self.record.entry.as_ref().map(|entry| entry.original.endpoint.as_str())
    }

    pub fn has_active_pending_restore(&self) -> bool {
        self.record.entry.as_ref().is_some_and(|entry| entry.owned)
    }

    pub fn has_pending_restore(&self) -> bool {
        self.record
            .entry
            .iter()
            .chain(&self.record.deferred)
            .any(|entry| entry.owned)
    }

    /// Visit saved endpoints under the same process lock. Platform recovery
    /// binds an observer to each exact ID; a replacement default is never used.
    pub fn endpoints(&self) -> Vec<String> {
        self.record
            .entry
            .iter()
            .chain(&self.record.deferred)
            .map(|entry| entry.original.endpoint.clone())
            .collect()
    }

    pub fn select_endpoint(&mut self, endpoint: &str) -> Result<(), String> {
        if self.endpoint() == Some(endpoint) {
            return Ok(());
        }
        let index = self
            .record
            .deferred
            .iter()
            .position(|entry| entry.original.endpoint == endpoint)
            .ok_or("saved audio endpoint not found")?;
        let mut record = self.record.clone();
        let selected = record.deferred.remove(index);
        if let Some(old) = record.entry.replace(selected)
            && old.owned
        {
            record.deferred.push(old);
        }
        self.persist(&record)?;
        self.record = record;
        Ok(())
    }

    /// Called only after a read proved the current endpoint unavailable. Keep
    /// that lease durable without preventing a lease on another output. Storage
    /// errors still fail closed, and the same endpoint cannot be leased twice.
    pub fn defer_unavailable(&mut self) -> Result<(), String> {
        let mut record = self.record.clone();
        if let Some(entry) = record.entry.take() {
            record.deferred.push(entry);
        }
        self.persist(&record)?;
        self.record = record;
        Ok(())
    }

    pub fn has_pending_restore_for(&self, endpoint: &str) -> bool {
        self.record
            .entry
            .iter()
            .chain(&self.record.deferred)
            .any(|entry| entry.owned && entry.original.endpoint == endpoint)
    }

    /// Persist the original and intended state before any call to SetMute.
    /// An unresolved older lease cannot be overwritten by a new capture session.
    pub fn prepare(&mut self, original: EndpointState) -> Result<(), String> {
        original.validate()?;
        if self.record.entry.as_ref().is_some_and(|entry| entry.owned)
            || self
                .record
                .deferred
                .iter()
                .any(|entry| entry.owned && entry.original.endpoint == original.endpoint)
        {
            return Err("an earlier audio mute restoration is pending".into());
        }
        let mut expected = original.clone();
        expected.muted = true;
        let record = Record {
            version: FORMAT_VERSION,
            deferred: self.record.deferred.clone(),
            entry: Some(Entry {
                original,
                expected,
                owned: true,
            }),
        };
        self.persist(&record)?;
        self.record = record;
        Ok(())
    }

    /// Latch user ownership in memory before writing. A storage error must never
    /// cause this running process to restore a state over the user's choice.
    pub fn revoke(&mut self) -> Result<(), String> {
        if let Some(entry) = &mut self.record.entry {
            entry.owned = false;
            self.persist(&self.record)?;
        }
        Ok(())
    }

    /// Restore only when our expected state is still present and no external
    /// event was seen. Exact volume comparison is conservative: a tiny mismatch
    /// leaves user state untouched. A missing endpoint retains the journal.
    pub fn recover<B: EndpointBackend>(&mut self, backend: &B, after_crash: bool) -> Result<RecoveryReport, String> {
        if self.record.entry.is_none()
            && let Some(entry) = self.record.deferred.first()
        {
            let endpoint = entry.original.endpoint.clone();
            self.select_endpoint(&endpoint)?;
        }
        let Some(entry) = self.record.entry.clone() else {
            return Ok(RecoveryReport {
                outcome: RecoveryOutcome::NothingToRecover,
                endpoint: None,
                crash_intent_uncertain: false,
            });
        };
        let endpoint = Some(entry.original.endpoint.clone());
        let report = |outcome, uncertain| RecoveryReport {
            outcome,
            endpoint: endpoint.clone(),
            crash_intent_uncertain: uncertain,
        };
        if !entry.owned || backend.ownership_lost() {
            self.revoke()?;
            return Ok(report(RecoveryOutcome::UserOverride, false));
        }
        let Some(current) = backend.read(&entry.original.endpoint)? else {
            if backend.permanently_removed(&entry.original.endpoint)? {
                self.clear()?;
                return Ok(report(RecoveryOutcome::EndpointRemoved, false));
            }
            return Ok(report(RecoveryOutcome::EndpointUnavailable, after_crash));
        };
        if current == entry.original {
            self.clear()?;
            return Ok(report(RecoveryOutcome::AlreadyOriginal, false));
        }
        if current != entry.expected || backend.ownership_lost() {
            self.revoke()?;
            return Ok(report(RecoveryOutcome::UserOverride, false));
        }
        // Verify storage is still writable before attempting any OS mutation.
        // The durable original remains present until the restore succeeds.
        self.persist(&self.record)?;
        if backend.ownership_lost() {
            self.revoke()?;
            return Ok(report(RecoveryOutcome::UserOverride, false));
        }
        if let Err(error) = backend.set_mute(&entry.original.endpoint, entry.original.muted) {
            // A driver can disappear between the readback and the tagged write.
            // Classify that race by the exact endpoint, never by error text.
            if backend.read(&entry.original.endpoint)?.is_none() {
                if backend.permanently_removed(&entry.original.endpoint)? {
                    self.clear()?;
                    return Ok(report(RecoveryOutcome::EndpointRemoved, false));
                }
                return Ok(report(RecoveryOutcome::EndpointUnavailable, after_crash));
            }
            return Err(error);
        }
        // An external event arriving during SetMute wins ownership from now on.
        if backend.ownership_lost() {
            self.revoke()?;
            return Ok(report(RecoveryOutcome::UserOverride, false));
        }
        let Some(actual) = backend.read(&entry.original.endpoint)? else {
            if backend.permanently_removed(&entry.original.endpoint)? {
                self.clear()?;
                return Ok(report(RecoveryOutcome::EndpointRemoved, false));
            }
            return Ok(report(RecoveryOutcome::EndpointUnavailable, after_crash));
        };
        if actual != entry.original {
            self.revoke()?;
            return Ok(report(RecoveryOutcome::UserOverride, false));
        }
        self.clear()?;
        Ok(report(RecoveryOutcome::Restored, after_crash))
    }

    /// Retain an unavailable endpoint independently of the next source lease.
    /// IO/ownership/storage errors remain errors; only proven unavailability
    /// defers restoration, and proven deletion resolves it entirely.
    pub fn recover_or_defer<B: EndpointBackend>(
        &mut self,
        backend: &B,
        after_crash: bool,
    ) -> Result<RecoveryReport, String> {
        let report = self.recover(backend, after_crash)?;
        if report.outcome == RecoveryOutcome::EndpointUnavailable {
            self.defer_unavailable()?;
            tracing::warn!(
                ?report,
                "old audio endpoint unavailable; mute restoration deferred until reconnect"
            );
        }
        Ok(report)
    }

    /// On a live level-only override, undo our mute without restoring volume.
    /// The full-state lease remains durably revoked, even if this write fails.
    pub fn release_after_volume_change<B: EndpointBackend>(&mut self, backend: &B) -> Result<(), String> {
        let Some(entry) = self.record.entry.clone() else {
            return Ok(());
        };
        self.revoke()?;
        if entry.original.muted || backend.mute_ownership_lost() {
            return Ok(());
        }
        let Some(current) = backend.read(&entry.original.endpoint)? else {
            return Ok(());
        };
        if current.muted && !backend.mute_ownership_lost() {
            backend.release_owned_mute(&entry.original.endpoint)?;
        }
        Ok(())
    }

    fn clear(&mut self) -> Result<(), String> {
        let record = Record {
            entry: None,
            ..self.record.clone()
        };
        self.persist(&record)?;
        self.record = record;
        Ok(())
    }

    fn persist(&self, record: &Record) -> Result<(), String> {
        record.validate()?;
        atomic_json(&self.path, record).map_err(|e| format!("persist audio recovery journal: {e}"))
    }
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    if bytes.len() > 32 * 1024 {
        return Err("audio recovery journal is too large".into());
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let from: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: both buffers are valid NUL-terminated paths for the duration
        // of this synchronous atomic replacement. The process lock is held.
        unsafe {
            MoveFileExW(
                windows::core::PCWSTR(from.as_ptr()),
                windows::core::PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    File::open(path.parent().ok_or("audio journal has no parent")?)
        .and_then(|d| d.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct Fake {
        state: Mutex<Option<EndpointState>>,
        vanish_on_restore: AtomicBool,
        removed: AtomicBool,
        removal_error: AtomicBool,
        changed: AtomicBool,
        mute_changed: AtomicBool,
        writes: AtomicUsize,
        journal: PathBuf,
    }
    impl EndpointBackend for Fake {
        fn permanently_removed(&self, _: &str) -> Result<bool, String> {
            if self.removal_error.load(Ordering::SeqCst) {
                return Err("endpoint lookup failed".into());
            }
            Ok(self.removed.load(Ordering::SeqCst))
        }
        fn read(&self, endpoint: &str) -> Result<Option<EndpointState>, String> {
            Ok(self
                .state
                .lock()
                .unwrap()
                .as_ref()
                .filter(|s| s.endpoint == endpoint)
                .cloned())
        }
        fn set_mute(&self, endpoint: &str, muted: bool) -> Result<(), String> {
            if !muted && self.vanish_on_restore.load(Ordering::SeqCst) {
                self.state.lock().unwrap().take();
                return Err("endpoint vanished during mute write".into());
            }
            let disk: Record = serde_json::from_slice(&std::fs::read(&self.journal).unwrap()).unwrap();
            let record = disk.entry.unwrap();
            assert!(record.owned, "OS mutation requires durable write-ahead ownership");
            assert_eq!(record.original.endpoint, endpoint);
            self.writes.fetch_add(1, Ordering::SeqCst);
            self.state.lock().unwrap().as_mut().unwrap().muted = muted;
            Ok(())
        }
        fn ownership_lost(&self) -> bool {
            self.changed.load(Ordering::SeqCst)
        }
        fn mute_ownership_lost(&self) -> bool {
            self.mute_changed.load(Ordering::SeqCst)
        }
        fn release_owned_mute(&self, _: &str) -> Result<(), String> {
            assert!(!self.mute_ownership_lost());
            let disk: Record = serde_json::from_slice(&std::fs::read(&self.journal).unwrap()).unwrap();
            assert!(!disk.entry.unwrap().owned);
            self.writes.fetch_add(1, Ordering::SeqCst);
            self.state.lock().unwrap().as_mut().unwrap().muted = false;
            Ok(())
        }
    }
    fn setup() -> (tempfile::TempDir, Journal, Fake, EndpointState) {
        let dir = tempfile::tempdir().unwrap();
        let before = EndpointState {
            endpoint: "headphones-1".into(),
            muted: false,
            volume: 0.23,
        };
        let fake = Fake {
            state: Mutex::new(Some(before.clone())),
            vanish_on_restore: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            removal_error: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            mute_changed: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
            journal: dir.path().join(JOURNAL_NAME),
        };
        let journal = Journal::open(dir.path()).unwrap();
        (dir, journal, fake, before)
    }
    fn apply(journal: &mut Journal, backend: &Fake, before: EndpointState) {
        journal.prepare(before).unwrap();
        backend.set_mute("headphones-1", true).unwrap();
    }

    #[test]
    fn normal_stop_restores_only_mute() {
        let (_dir, mut journal, backend, before) = setup();
        apply(&mut journal, &backend, before.clone());
        assert_eq!(
            journal.recover(&backend, false).unwrap().outcome,
            RecoveryOutcome::Restored
        );
        assert_eq!(backend.read("headphones-1").unwrap(), Some(before));
        assert!(!journal.has_pending_restore());
        assert_eq!(backend.writes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn default_switch_restores_only_the_old_endpoint_and_preserves_new_remembered_controls() {
        struct Pair {
            states: Mutex<std::collections::BTreeMap<String, EndpointState>>,
            writes: Mutex<Vec<(String, bool)>>,
        }
        impl EndpointBackend for Pair {
            fn read(&self, endpoint: &str) -> Result<Option<EndpointState>, String> {
                Ok(self.states.lock().unwrap().get(endpoint).cloned())
            }
            fn set_mute(&self, endpoint: &str, muted: bool) -> Result<(), String> {
                self.states
                    .lock()
                    .unwrap()
                    .get_mut(endpoint)
                    .ok_or("endpoint unavailable")?
                    .muted = muted;
                self.writes.lock().unwrap().push((endpoint.into(), muted));
                Ok(())
            }
            fn ownership_lost(&self) -> bool {
                false
            }
        }
        for old_missing in [false, true] {
            for (volume, muted) in [(0.12, false), (0.87, false), (0.63, true)] {
                let dir = tempfile::tempdir().unwrap();
                let mut journal = Journal::open(dir.path()).unwrap();
                let old = EndpointState {
                    endpoint: "old-endpoint".into(),
                    volume: 0.4,
                    muted: false,
                };
                let new = EndpointState {
                    endpoint: "new-endpoint".into(),
                    volume,
                    muted,
                };
                let backend = Pair {
                    states: Mutex::new(
                        [(old.endpoint.clone(), old.clone()), (new.endpoint.clone(), new.clone())].into(),
                    ),
                    writes: Mutex::new(vec![]),
                };
                journal.prepare(old.clone()).unwrap();
                backend.set_mute(&old.endpoint, true).unwrap();
                let leased = backend.states.lock().unwrap().get(&old.endpoint).unwrap().clone();
                if old_missing {
                    backend.states.lock().unwrap().remove(&old.endpoint);
                }
                let outcome = journal.recover(&backend, false).unwrap().outcome;
                if old_missing {
                    assert_eq!(outcome, RecoveryOutcome::EndpointUnavailable);
                    assert!(journal.has_pending_restore());
                    backend.states.lock().unwrap().insert(old.endpoint.clone(), leased);
                    assert_eq!(
                        journal.recover(&backend, false).unwrap().outcome,
                        RecoveryOutcome::Restored
                    );
                } else {
                    assert_eq!(outcome, RecoveryOutcome::Restored);
                }
                assert_eq!(backend.read(&old.endpoint).unwrap(), Some(old.clone()));
                assert_eq!(backend.read(&new.endpoint).unwrap(), Some(new));
                assert_eq!(
                    *backend.writes.lock().unwrap(),
                    vec![(old.endpoint.clone(), true), (old.endpoint, false)]
                );
            }
        }
    }

    #[test]
    fn level_only_override_releases_our_mute_and_keeps_the_new_volume() {
        let (_dir, mut journal, backend, before) = setup();
        apply(&mut journal, &backend, before);
        backend.changed.store(true, Ordering::SeqCst);
        backend.state.lock().unwrap().as_mut().unwrap().volume = 0.4;
        journal.release_after_volume_change(&backend).unwrap();
        let current = backend.read("headphones-1").unwrap().unwrap();
        assert_eq!(current.volume, 0.4);
        assert!(!current.muted);
        assert_eq!(backend.writes.load(Ordering::SeqCst), 2);
        assert!(!journal.has_pending_restore());
        journal.recover(&backend, false).unwrap();
        assert_eq!(backend.writes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_mute_toggle_back_or_original_mute_is_never_overwritten_on_level_override() {
        for originally_muted in [false, true] {
            let (_dir, mut journal, backend, mut before) = setup();
            before.muted = originally_muted;
            apply(&mut journal, &backend, before);
            backend.changed.store(true, Ordering::SeqCst);
            backend.mute_changed.store(!originally_muted, Ordering::SeqCst);
            backend.state.lock().unwrap().as_mut().unwrap().volume = 0.4;
            journal.release_after_volume_change(&backend).unwrap();
            assert!(backend.read("headphones-1").unwrap().unwrap().muted);
            assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn originally_muted_is_not_unmuted() {
        let (_dir, mut journal, backend, mut before) = setup();
        before.muted = true;
        *backend.state.lock().unwrap() = Some(before.clone());
        apply(&mut journal, &backend, before);
        assert_eq!(
            journal.recover(&backend, false).unwrap().outcome,
            RecoveryOutcome::AlreadyOriginal
        );
        assert!(backend.state.lock().unwrap().as_ref().unwrap().muted);
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn user_changes_then_returns_to_same_values_still_wins_after_restart() {
        let (dir, mut journal, backend, before) = setup();
        apply(&mut journal, &backend, before);
        backend.changed.store(true, Ordering::SeqCst); // event, not snapshot comparison
        journal.revoke().unwrap();
        drop(journal);
        backend.changed.store(false, Ordering::SeqCst); // new process has no event history
        let mut journal = Journal::open(dir.path()).unwrap();
        assert_eq!(
            journal.recover(&backend, true).unwrap().outcome,
            RecoveryOutcome::UserOverride
        );
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_changed_volume_is_never_written_back() {
        let (_dir, mut journal, backend, before) = setup();
        apply(&mut journal, &backend, before);
        backend.state.lock().unwrap().as_mut().unwrap().volume = 0.4;
        assert_eq!(
            journal.recover(&backend, true).unwrap().outcome,
            RecoveryOutcome::UserOverride
        );
        assert_eq!(backend.state.lock().unwrap().as_ref().unwrap().volume, 0.4);
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn crash_after_set_mute_recovers_with_explicit_uncertainty() {
        let (dir, mut journal, backend, before) = setup();
        apply(&mut journal, &backend, before.clone());
        drop(journal); // simulate abrupt exit: no platform lease Drop
        let mut journal = Journal::open(dir.path()).unwrap();
        let report = journal.recover(&backend, true).unwrap();
        assert_eq!(report.outcome, RecoveryOutcome::Restored);
        assert!(report.crash_intent_uncertain);
        assert_eq!(backend.read("headphones-1").unwrap(), Some(before));
    }

    #[test]
    fn crash_before_set_mute_does_not_change_endpoint() {
        let (dir, mut journal, backend, before) = setup();
        journal.prepare(before).unwrap();
        drop(journal);
        let mut journal = Journal::open(dir.path()).unwrap();
        assert_eq!(
            journal.recover(&backend, true).unwrap().outcome,
            RecoveryOutcome::AlreadyOriginal
        );
        assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn missing_endpoint_keeps_original_until_device_returns() {
        let (dir, mut journal, backend, before) = setup();
        apply(&mut journal, &backend, before.clone());
        let absent = backend.state.lock().unwrap().take();
        assert_eq!(
            journal.recover(&backend, true).unwrap().outcome,
            RecoveryOutcome::EndpointUnavailable
        );
        assert!(journal.prepare(before.clone()).is_err());
        drop(journal);
        *backend.state.lock().unwrap() = absent;
        let mut journal = Journal::open(dir.path()).unwrap();
        assert_eq!(
            journal.recover(&backend, true).unwrap().outcome,
            RecoveryOutcome::Restored
        );
        assert_eq!(backend.read("headphones-1").unwrap(), Some(before));
    }

    #[test]
    fn permanently_deleted_endpoint_resolves_on_stop_and_restart_without_os_write() {
        for after_crash in [false, true] {
            let (dir, mut journal, backend, before) = setup();
            apply(&mut journal, &backend, before);
            backend.state.lock().unwrap().take();
            backend.removed.store(true, Ordering::SeqCst);
            if after_crash {
                drop(journal);
                journal = Journal::open(dir.path()).unwrap();
            }
            let report = journal.recover(&backend, after_crash).unwrap();
            assert_eq!(report.outcome, RecoveryOutcome::EndpointRemoved);
            assert!(!report.crash_intent_uncertain);
            assert!(!journal.has_pending_restore());
            assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
            drop(journal);
            assert!(!Journal::open(dir.path()).unwrap().has_pending_restore());
        }
    }

    #[test]
    fn unplugged_lease_does_not_block_replacement_and_recovers_after_replug() {
        let (dir, mut journal, backend, before) = setup();
        apply(&mut journal, &backend, before.clone());
        let old_muted = backend.state.lock().unwrap().take().unwrap();
        assert_eq!(
            journal.recover_or_defer(&backend, false).unwrap().outcome,
            RecoveryOutcome::EndpointUnavailable
        );
        assert!(
            journal.prepare(before.clone()).is_err(),
            "same endpoint cannot overwrite pending restoration"
        );
        let replacement = EndpointState {
            endpoint: "new-default".into(),
            muted: false,
            volume: 0.6,
        };
        *backend.state.lock().unwrap() = Some(replacement.clone());
        journal.prepare(replacement.clone()).unwrap();
        backend.set_mute("new-default", true).unwrap();
        drop(journal); // both independent records survive a crash
        let mut journal = Journal::open(dir.path()).unwrap();
        assert_eq!(journal.endpoints().len(), 2);
        assert_eq!(
            journal.recover(&backend, true).unwrap().outcome,
            RecoveryOutcome::Restored
        );
        assert_eq!(*backend.state.lock().unwrap(), Some(replacement));
        assert!(journal.has_pending_restore());
        assert_eq!(
            journal.recover(&backend, true).unwrap().outcome,
            RecoveryOutcome::EndpointUnavailable
        );
        assert_eq!(
            backend.writes.load(Ordering::SeqCst),
            3,
            "no write to replacement for old lease"
        );
        *backend.state.lock().unwrap() = Some(old_muted);
        assert_eq!(
            journal.recover(&backend, true).unwrap().outcome,
            RecoveryOutcome::Restored
        );
        assert_eq!(*backend.state.lock().unwrap(), Some(before));
        assert!(!journal.has_pending_restore());
    }

    #[test]
    fn removal_between_read_and_restore_write_is_classified_without_faulting_source() {
        for permanently_removed in [false, true] {
            let (_dir, mut journal, backend, before) = setup();
            apply(&mut journal, &backend, before);
            backend.vanish_on_restore.store(true, Ordering::SeqCst);
            backend.removed.store(permanently_removed, Ordering::SeqCst);
            let report = journal.recover_or_defer(&backend, false).unwrap();
            assert_eq!(
                report.outcome,
                if permanently_removed {
                    RecoveryOutcome::EndpointRemoved
                } else {
                    RecoveryOutcome::EndpointUnavailable
                }
            );
            assert_eq!(journal.has_pending_restore(), !permanently_removed);
            assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn deletion_requires_proof_and_durable_resolution() {
        let (dir, mut journal, backend, before) = setup();
        apply(&mut journal, &backend, before);
        backend.state.lock().unwrap().take();
        backend.removal_error.store(true, Ordering::SeqCst);
        assert!(journal.recover(&backend, false).is_err());
        assert!(journal.has_pending_restore());
        backend.removal_error.store(false, Ordering::SeqCst);
        backend.removed.store(true, Ordering::SeqCst);
        std::fs::create_dir(dir.path().join("audio-mute-recovery.tmp")).unwrap();
        assert!(journal.recover(&backend, false).is_err());
        assert!(journal.has_pending_restore());
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn corrupt_and_unwritable_journals_fail_before_os_mutation() {
        let (dir, mut journal, backend, before) = setup();
        std::fs::create_dir(dir.path().join("audio-mute-recovery.tmp")).unwrap();
        assert!(journal.prepare(before).is_err());
        assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
        drop(journal);
        std::fs::write(dir.path().join(JOURNAL_NAME), "broken").unwrap();
        assert!(Journal::open(dir.path()).is_err());
        assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn failed_revoke_persistence_still_relinquishes_live_ownership() {
        let (dir, mut journal, backend, before) = setup();
        apply(&mut journal, &backend, before);
        std::fs::create_dir(dir.path().join("audio-mute-recovery.tmp")).unwrap();
        assert!(journal.revoke().is_err());
        assert!(!journal.has_pending_restore());
        assert!(journal.recover(&backend, false).is_err());
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exclusive_lock_blocks_second_owner_until_first_closes() {
        let (dir, journal, _backend, _before) = setup();
        assert!(Journal::open(dir.path()).is_err());
        drop(journal);
        assert!(Journal::open(dir.path()).is_ok());
    }

    #[test]
    fn dropped_journal_releases_lock_while_inherited_descriptor_is_alive() {
        let (dir, journal, _backend, _before) = setup();
        // dup shares the open file description just like a descriptor inherited
        // by a forked child before exec. Keep that reference alive across Drop.
        let inherited = journal._lock.0.try_clone().unwrap();
        assert!(Journal::open(dir.path()).is_err(), "a live journal owns the lock");
        drop(journal);
        let replacement =
            Journal::open(dir.path()).expect("an inherited descriptor must not prolong journal ownership");
        drop(inherited);
        assert!(
            Journal::open(dir.path()).is_err(),
            "the replacement still owns the lock"
        );
        drop(replacement);
        assert!(Journal::open(dir.path()).is_ok());
    }
}
