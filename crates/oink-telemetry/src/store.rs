use crate::{
    sampling::{Candidate, Reservoir, random_below},
    *,
};
use serde::{Deserialize, Serialize};
#[cfg(not(windows))]
use std::io::Write;
use std::{
    fs::{File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

const DAY: u64 = 86400;
const CHECKPOINT_MAX: u64 = 1536 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    report: AdmittedReport,
    generation: u64,
    collected_at: u64,
    attempts: u8,
    next: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CrashAdmission {
    at: u64,
    build: AppVersion,
    component: Component,
    class: ExceptionClass,
    frame: Option<Frame>,
    callsite: Option<u32>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Budget {
    day: u64,
    sealed: u8,
    perf_roots: u8,
    perf_requests: u8,
    crash_requests: u8,
    perf_bytes: u32,
    crash_bytes: u32,
    last_perf: u64,
    last_crash: u64,
    pause_perf: u64,
    pause_crash: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema: u16,
    consent: ReportConsent,
    budget: Budget,
    admissions: Vec<CrashAdmission>,
    queue: Vec<Pending>,
    reservoirs: [Reservoir; 8],
    // Additive checkpoint field: preserve consent and spent budgets from the
    // existing four families when clipboard measurements are first enabled.
    #[serde(default)]
    clipboard_reservoirs: [Reservoir; 2],
    audio_minute: Option<Candidate>,
    audio_seen: u64,
}
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    _lock: File,
    state: State,
    last_checkpoint: Vec<u8>,
    disabled: bool,
}
impl Drop for Store {
    fn drop(&mut self) {
        // Release this writer's lease explicitly. On Unix, a concurrent fork
        // inherits the locked open file description until exec, even with
        // CLOEXEC; closing only our descriptor can leave the next owner Busy.
        let _ = fs2::FileExt::unlock(&self._lock);
    }
}
#[derive(Clone, Debug)]
pub struct Submission {
    pub report: AdmittedReport,
    pub generation: u64,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Accepted,
    Retry,
    InvalidProject,
    TooLarge,
    Limited { errors_until: u64, transactions_until: u64 },
    Drop,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueStatus {
    pub performance: u16,
    pub crashes: u16,
    pub disabled: bool,
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(windows)]
    {
        // NamedTempFile.persist uses MoveFileExW and shares the same scanner
        // exposure. Use the original-handle publication and its bounded policy;
        // never retry checkpoint writes, flushes or the whole store transaction.
        oink_foundation::storage::atomic_write(path, bytes).map_err(|_| Error::Io)
    }
    #[cfg(not(windows))]
    {
        let parent = path.parent().ok_or(Error::Io)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|_| Error::Io)?;
        tmp.write_all(bytes).map_err(|_| Error::Io)?;
        tmp.as_file().sync_all().map_err(|_| Error::Io)?;
        tmp.persist(path).map_err(|_| Error::Io)?;
        #[cfg(unix)]
        File::open(parent).and_then(|f| f.sync_all()).map_err(|_| Error::Io)?;
        Ok(())
    }
}
impl Store {
    pub fn open(dir: &Path, consent: ReportConsent, now: u64) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|_| Error::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|_| Error::Io)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("writer.lock"))
            .map_err(|_| Error::Io)?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(|_| Error::Busy)?;
        let path = dir.join("checkpoint.json");
        let state = match File::open(path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(CHECKPOINT_MAX + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| Error::Io)?;
                if bytes.len() as u64 > CHECKPOINT_MAX {
                    return Err(Error::Invalid);
                }
                // Corrupt budgets cannot be treated as a fresh allowance.
                let s: State = serde_json::from_slice(&bytes).map_err(|_| Error::Invalid)?;
                if s.schema != 3
                    || s.queue.len() > 266
                    || s.admissions.len() > 5
                    || s.reservoirs
                        .iter()
                        .chain(&s.clipboard_reservoirs)
                        .any(|r| r.keys.len() > 16 || r.candidates.len() > 2)
                {
                    return Err(Error::Invalid);
                }
                s
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State {
                schema: 3,
                consent,
                budget: Budget {
                    day: now / DAY,
                    ..Budget::default()
                },
                admissions: Vec::new(),
                queue: Vec::new(),
                reservoirs: std::array::from_fn(|_| Reservoir::default()),
                clipboard_reservoirs: std::array::from_fn(|_| Reservoir::default()),
                audio_minute: None,
                audio_seen: 0,
            },
            Err(_) => return Err(Error::Io),
        };
        let mut store = Self {
            dir: dir.to_path_buf(),
            _lock: lock,
            state,
            last_checkpoint: Vec::new(),
            disabled: false,
        };
        if !consent.granted() || store.state.consent != consent {
            store.clear();
        }
        // A durable revocation wins over an older settings write (e.g. disk
        // failure between the two). Never resurrect that old grant on restart.
        let consent = if store.state.consent.generation > consent.generation
            || (store.state.consent.generation == consent.generation
                && !store.state.consent.granted()
                && consent.granted())
        {
            ReportConsent {
                choice: Choice::Denied,
                generation: store.state.consent.generation,
                ..consent
            }
        } else {
            consent
        };
        store.state.consent = consent;
        store.expire(now);
        store.persist()?;
        Ok(store)
    }
    pub fn checkpoint(&mut self) -> Result<()> {
        self.persist()
    }
    fn persist(&mut self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.state).map_err(|_| Error::Invalid)?;
        if bytes.len() as u64 > CHECKPOINT_MAX {
            self.disabled = true;
            return Err(Error::Limit);
        }
        // Rejected samples and retries can leave state unchanged. Keep exact
        // bytes only after a successful durable write; changed admissions,
        // budgets and consent still use the same synchronous persistence path.
        if bytes == self.last_checkpoint {
            return Ok(());
        }
        if let Err(e) = atomic_write(&self.dir.join("checkpoint.json"), &bytes) {
            self.disabled = true;
            return Err(e);
        }
        self.last_checkpoint = bytes;
        Ok(())
    }
    fn clear(&mut self) {
        self.state.queue.clear();
        self.state.reservoirs = std::array::from_fn(|_| Reservoir::default());
        self.state.clipboard_reservoirs = std::array::from_fn(|_| Reservoir::default());
        self.state.audio_minute = None;
        self.state.audio_seen = 0;
        // Raw/capsule staging belongs exclusively to this report writer. OS crash
        // reports are never deleted. Budgets and the writer lock survive opt-out.
        for name in ["capsules", "raw"] {
            let path = self.dir.join(name);
            if path.exists() {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
    pub fn consent(&self) -> ReportConsent {
        self.state.consent
    }
    pub fn set_consent(&mut self, consent: ReportConsent) -> Result<()> {
        self.clear();
        self.state.consent = consent;
        self.persist()
    }
    pub fn status(&self) -> QueueStatus {
        QueueStatus {
            performance: self.state.queue.iter().filter(|q| !q.report.report.crash()).count() as u16,
            crashes: self.state.queue.iter().filter(|q| q.report.report.crash()).count() as u16,
            disabled: self.disabled,
        }
    }
    fn check(&self, generation: u64) -> Result<()> {
        if self.disabled || !self.state.consent.granted() {
            return Err(Error::Disabled);
        }
        if self.state.consent.generation != generation {
            return Err(Error::Stale);
        }
        Ok(())
    }
    fn expire(&mut self, now: u64) {
        let generation = self.state.consent.generation;
        self.state.queue.retain(|q| {
            q.generation == generation
                && q.report.validate().is_ok()
                && now >= q.collected_at
                && now - q.collected_at < if q.report.report.crash() { 7 * DAY } else { DAY }
        });
        self.state.admissions.retain(|a| now.saturating_sub(a.at) < 31 * DAY);
        // Validate category byte limits when loading a locally modified checkpoint.
        let mut counts = [0usize; 2];
        let mut bytes = [0usize; 2];
        self.state.queue.retain(|q| {
            let i = usize::from(q.report.report.crash());
            let Ok(wire) = wire::encode(&q.report) else {
                return false;
            };
            counts[i] += 1;
            bytes[i] += wire.len();
            counts[i] <= [256, 10][i] && bytes[i] <= [1024 * 1024, 256 * 1024][i]
        });
    }
    fn enqueue(&mut self, report: Report, at: u64, now: u64) -> Result<()> {
        let crash = report.crash();
        let index = usize::from(crash);
        let admitted = AdmittedReport::new(report, at)?;
        let size = wire::encode(&admitted)?.len();
        let mut count = 0;
        let mut bytes = size;
        for q in &self.state.queue {
            if q.report.report.crash() == crash {
                count += 1;
                bytes += wire::encode(&q.report)?.len();
            }
        }
        if count >= [256, 10][index] || bytes > [1024 * 1024, 256 * 1024][index] {
            return Err(Error::Limit);
        }
        self.state.queue.push(Pending {
            report: admitted,
            generation: self.state.consent.generation,
            collected_at: at,
            attempts: 0,
            next: now,
        });
        Ok(())
    }
    pub fn observe(&mut self, generation: u64, report: Report, collected_at: u64, now: u64) -> Result<()> {
        self.check(generation)?;
        report.validate()?;
        let is_crash = report.crash();
        if collected_at > now || now.saturating_sub(collected_at) >= if report.crash() { 7 * DAY } else { DAY } {
            return Err(Error::Invalid);
        }
        self.advance(now)?;
        if now
            < if report.crash() {
                self.state.budget.pause_crash
            } else {
                self.state.budget.pause_perf
            }
        {
            return Err(Error::Limit);
        }
        if let Report::SafeCrash(c) = &report {
            let fingerprint = CrashAdmission {
                at: now,
                build: c.build.app_version,
                component: c.component,
                class: c.exception_class,
                frame: c.frames().next().copied(),
                callsite: c.callsite,
            };
            if self.state.admissions.len() >= 5
                || self.state.admissions.iter().filter(|a| a.at / DAY == now / DAY).count() >= 2
                || self.state.admissions.iter().any(|a| {
                    now.saturating_sub(a.at) < DAY
                        && a.build == fingerprint.build
                        && a.component == fingerprint.component
                        && a.class == fingerprint.class
                        && a.frame == fingerprint.frame
                        && a.callsite == fingerprint.callsite
                })
            {
                return Err(Error::Limit);
            }
            self.enqueue(report, collected_at, now)?;
            self.state.admissions.push(fingerprint);
        } else {
            let context = report.context().ok_or(Error::Invalid)?;
            let distribution = matches!(report, Report::PerformanceDistribution(_));
            if !distribution && context.metric == Metric::AudioPushToPresentationEstimate {
                self.state.audio_seen = self.state.audio_seen.saturating_add(1);
                if random_below(self.state.audio_seen)? == 0 {
                    self.state.audio_minute = Some(Candidate {
                        report,
                        at: collected_at,
                    });
                }
            } else {
                let gated = if distribution
                    || matches!(context.metric.family(), Family::Drag | Family::Link | Family::Clipboard)
                {
                    random_below(10)? == 0
                } else {
                    true
                };
                // Input interval producers have already used their random-phase 1/100 gate.
                if gated {
                    self.offer(report, collected_at)?;
                }
            }
        }
        // Numeric performance arrivals are checkpointed once per worker batch,
        // not once per packet. Crash admission remains immediately durable.
        if is_crash { self.persist() } else { Ok(()) }
    }
    fn offer(&mut self, report: Report, at: u64) -> Result<()> {
        let family = report.context().ok_or(Error::Invalid)?.metric.family();
        let reservoirs = if family == Family::Clipboard {
            &mut self.state.clipboard_reservoirs[..]
        } else {
            &mut self.state.reservoirs[family as usize * 2..family as usize * 2 + 2]
        };
        let dist = matches!(report, Report::PerformanceDistribution(_));
        // The 16-key cap is shared by both reservoirs in a family.
        let context = report.context().unwrap();
        let keys: std::collections::HashSet<_> = reservoirs.iter().flat_map(|r| r.keys.iter().copied()).collect();
        if !keys.contains(&context) && keys.len() >= 16 {
            return Err(Error::Limit);
        }
        reservoirs[usize::from(dist)].offer(report, at, if dist { 1 } else { 2 })
    }
    pub fn advance(&mut self, now: u64) -> Result<()> {
        self.expire(now);
        if self.state.audio_minute.as_ref().is_some_and(|c| c.at / 60 < now / 60) {
            let c = self.state.audio_minute.take().unwrap();
            self.state.audio_seen = 0;
            self.offer(c.report, c.at)?;
        }
        if now / DAY > self.state.budget.day {
            // Seal once atomically with queue and counters. Clock rollback never
            // restores budget, and shutdown/restart never seals a second quota.
            let mut candidates: Vec<_> = self
                .state
                .reservoirs
                .iter_mut()
                .chain(&mut self.state.clipboard_reservoirs)
                .flat_map(|r| std::mem::take(r).candidates)
                .collect();
            for end in (1..candidates.len()).rev() {
                let selected = random_below((end + 1) as u64)? as usize;
                candidates.swap(end, selected);
            }
            let last_perf = self.state.budget.last_perf;
            let last_crash = self.state.budget.last_crash;
            let pause_perf = self.state.budget.pause_perf;
            let pause_crash = self.state.budget.pause_crash;
            self.state.budget = Budget {
                day: now / DAY,
                last_perf,
                last_crash,
                pause_perf,
                pause_crash,
                ..Budget::default()
            };
            for c in candidates {
                if now.saturating_sub(c.at) < DAY
                    && self.state.budget.sealed < 12
                    && self.enqueue(c.report, c.at, now).is_ok()
                {
                    self.state.budget.sealed += 1;
                }
            }
            self.persist()?;
        }
        Ok(())
    }
    pub fn prepare(&mut self, generation: u64, now: u64) -> Result<Option<Submission>> {
        self.check(generation)?;
        self.advance(now)?;
        let b = &self.state.budget;
        let eligible = self.state.queue.iter().position(|q| {
            if q.next > now {
                return false;
            }
            if q.report.report.crash() {
                q.attempts < 3
                    && b.crash_requests < 10
                    && now >= b.last_crash.saturating_add(600)
                    && now >= b.pause_crash
            } else {
                b.perf_roots < 12
                    && b.perf_requests < 32
                    && now >= b.last_perf.saturating_add(900)
                    && now >= b.pause_perf
            }
        });
        let Some(i) = eligible else { return Ok(None) };
        let q = &mut self.state.queue[i];
        let bytes = wire::encode(&q.report)?;
        let size = bytes.len() as u32;
        let b = &mut self.state.budget;
        if q.report.report.crash() {
            if b.crash_bytes.saturating_add(size) > 256 * 1024 {
                return Ok(None);
            }
            b.crash_requests += 1;
            b.crash_bytes += size;
            b.last_crash = now;
        } else {
            if b.perf_bytes.saturating_add(size) > 256 * 1024 {
                return Ok(None);
            }
            b.perf_roots += 1;
            b.perf_requests += 1;
            b.perf_bytes += size;
            b.last_perf = now;
        }
        q.attempts = q.attempts.saturating_add(1);
        q.next = now.saturating_add((60u64.saturating_mul(1 << q.attempts.min(9))).min(21600) + random_below(60)?);
        let result = Submission {
            report: q.report.clone(),
            generation,
            bytes,
        };
        // Charge every attempt durably BEFORE submission; crash/restart is not a refund.
        self.persist()?;
        Ok(Some(result))
    }
    pub fn complete(&mut self, submission: &Submission, result: Delivery) -> Result<()> {
        self.check(submission.generation)?;
        match result {
            Delivery::Retry => {
                self.state.queue.retain(|q| {
                    !(q.report.event_id == submission.report.event_id && q.report.report.crash() && q.attempts >= 3)
                });
            }
            Delivery::Limited {
                errors_until,
                transactions_until,
            } => {
                self.state.budget.pause_crash = self.state.budget.pause_crash.max(errors_until);
                self.state.budget.pause_perf = self.state.budget.pause_perf.max(transactions_until);
                self.state.queue.retain(|q| {
                    if q.report.report.crash() {
                        errors_until == 0
                    } else {
                        transactions_until == 0
                    }
                });
                if transactions_until > 0 {
                    self.state.reservoirs = std::array::from_fn(|_| Reservoir::default());
                    self.state.clipboard_reservoirs = std::array::from_fn(|_| Reservoir::default());
                    self.state.audio_minute = None;
                }
            }
            other => {
                self.state
                    .queue
                    .retain(|q| q.report.event_id != submission.report.event_id);
                if other == Delivery::InvalidProject {
                    self.disabled = true;
                }
            }
        }
        self.persist()
    }
    pub fn has_work(&self) -> bool {
        !self.state.queue.is_empty()
            || self
                .state
                .reservoirs
                .iter()
                .chain(&self.state.clipboard_reservoirs)
                .any(|r| !r.candidates.is_empty())
            || self.state.audio_minute.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn dropped_writer_releases_its_lease_while_an_inherited_descriptor_remains_open() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), consent(), DAY).unwrap();
        // A fork in another thread shares this open file description until exec,
        // even with CLOEXEC. A duplicate models that lifetime without forking a
        // multithreaded test process or depending on how quickly exec is scheduled.
        let inherited = store._lock.try_clone().unwrap();
        drop(store);
        let reopened = Store::open(directory.path(), consent(), DAY);
        drop(inherited);
        assert!(
            reopened.is_ok(),
            "the retired writer still owns its lease: {reopened:?}"
        );
    }

    #[test]
    fn unchanged_checkpoint_does_not_replace_file_but_crashes_remain_durable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint.json");
        let mut store = Store::open(directory.path(), consent(), DAY).unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(DAY);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        store.checkpoint().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), old);
        store.observe(1, crash(7), DAY, DAY).unwrap();
        assert_ne!(std::fs::metadata(&path).unwrap().modified().unwrap(), old);
        drop(store);
        let restored = Store::open(directory.path(), consent(), DAY).unwrap();
        assert_eq!(restored.state.admissions.len(), 1);
        assert_eq!(restored.state.queue.len(), 1);
    }

    #[test]
    #[ignore = "isolated resource benchmark; run with --ignored --nocapture"]
    fn resource_checkpoint_benchmark() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path(), consent(), DAY).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..1000 {
            store.checkpoint().unwrap();
        }
        println!(
            "unchanged_checkpoints=1000 elapsed_us={}",
            started.elapsed().as_micros()
        );
    }

    #[test]
    fn checkpoint_replacement_preserves_an_open_readers_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint.json");
        atomic_write(&path, b"before").unwrap();
        let mut reader = File::open(&path).unwrap();
        atomic_write(&path, b"after").unwrap();
        let mut previous = Vec::new();
        reader.read_to_end(&mut previous).unwrap();
        assert_eq!(previous, b"before");
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    fn consent() -> ReportConsent {
        ReportConsent {
            choice: Choice::Granted,
            policy_version: 3,
            generation: 1,
        }
    }
    fn build() -> BuildContext {
        BuildContext {
            app_version: AppVersion {
                major: 1,
                minor: 0,
                patch: 0,
                build: 1,
            },
            os_version_class: OsVersionClass::Unknown,
        }
    }
    fn crash(code: u32) -> Report {
        let mut c = SafeCrash::empty(build(), Component::Agent, CrashKind::RustPanic, ExceptionClass::Panic);
        c.callsite = Some(code);
        Report::SafeCrash(c)
    }
    #[test]
    fn opt_out_cleans_only_private_reports_and_preserves_other_storage_classes() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "Diagnostics/buffer/segment.dseg",
            "Diagnostics/1-manual.zip",
            "Diagnostics/2-emergency.zip",
            "oink.2026-01-01.log",
        ] {
            let path = root.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"keep").unwrap();
        }
        let reports = root.path().join("reports");
        let mut store = Store::open(&reports, consent(), DAY).unwrap();
        store.observe(1, crash(9), DAY, DAY).unwrap();
        for name in ["capsules/agent.bin", "raw/0.dmp", "raw/0.lease", "raw/0.safe"] {
            let path = reports.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"delete").unwrap();
        }
        let denied = ReportConsent {
            choice: Choice::Denied,
            generation: 2,
            ..consent()
        };
        store.set_consent(denied).unwrap();
        assert!(!store.has_work());
        assert!(!reports.join("capsules").exists());
        assert!(!reports.join("raw").exists());
        assert!(reports.join("checkpoint.json").exists());
        assert!(reports.join("writer.lock").exists());
        for name in [
            "Diagnostics/buffer/segment.dseg",
            "Diagnostics/1-manual.zip",
            "Diagnostics/2-emergency.zip",
            "oink.2026-01-01.log",
        ] {
            assert_eq!(std::fs::read(root.path().join(name)).unwrap(), b"keep");
        }
        drop(store);
        let restored = Store::open(&reports, denied, DAY).unwrap();
        assert_eq!(restored.consent(), denied);
        assert!(!restored.has_work());
    }
    fn interval() -> Report {
        Report::MeasuredInterval(MeasuredInterval {
            context: MeasurementContext {
                build: build(),
                metric: Metric::DragQueueWait,
                platform_pair: None,
                network_path: None,
                transport: None,
                codec: None,
            },
            elapsed_us: 1500,
        })
    }
    #[test]
    fn off_and_stale_never_create_reports_or_submissions() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path(), ReportConsent::default(), DAY).unwrap();
        assert_eq!(s.observe(0, crash(1), DAY, DAY), Err(Error::Disabled));
        assert!(s.prepare(0, DAY).is_err());
        assert!(!s.has_work());
        s.set_consent(consent()).unwrap();
        assert_eq!(s.observe(0, crash(1), DAY, DAY), Err(Error::Stale));
        assert!(!s.has_work());
        s.observe(1, crash(1), DAY, DAY).unwrap();
        s.set_consent(ReportConsent {
            choice: Choice::Denied,
            generation: 2,
            ..consent()
        })
        .unwrap();
        assert!(!s.has_work());
        assert!(s.prepare(1, DAY).is_err());
        let bytes = std::fs::read(dir.path().join("checkpoint.json")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("event_id"));
    }
    #[test]
    fn crash_budget_and_dedup_survive_restart_and_toggles() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path(), consent(), DAY).unwrap();
        s.observe(1, crash(1), DAY, DAY).unwrap();
        assert_eq!(s.observe(1, crash(1), DAY, DAY), Err(Error::Limit));
        s.observe(1, crash(2), DAY, DAY).unwrap();
        assert_eq!(s.observe(1, crash(3), DAY, DAY), Err(Error::Limit));
        s.set_consent(ReportConsent {
            choice: Choice::Denied,
            generation: 2,
            ..consent()
        })
        .unwrap();
        let next = ReportConsent {
            generation: 3,
            ..consent()
        };
        s.set_consent(next).unwrap();
        drop(s);
        let mut s = Store::open(dir.path(), next, DAY).unwrap();
        assert_eq!(s.observe(3, crash(3), DAY, DAY), Err(Error::Limit));
        for day in [2, 3, 4] {
            s.observe(3, crash(day as u32), day * DAY, day * DAY).unwrap();
        }
        assert_eq!(s.observe(3, crash(5), 5 * DAY, 5 * DAY), Err(Error::Limit));
        assert!(s.observe(3, crash(6), 33 * DAY, 33 * DAY).is_ok());
    }
    #[test]
    fn queue_bounds_ttl_single_writer_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path(), consent(), DAY).unwrap();
        assert_eq!(Store::open(dir.path(), consent(), DAY).unwrap_err(), Error::Busy);
        for _ in 0..256 {
            s.enqueue(interval(), DAY, DAY).unwrap();
        }
        assert_eq!(s.enqueue(interval(), DAY, DAY), Err(Error::Limit));
        for _ in 0..10 {
            s.enqueue(crash(1), DAY, DAY).unwrap();
        }
        assert_eq!(s.enqueue(crash(1), DAY, DAY), Err(Error::Limit));
        s.expire(2 * DAY);
        assert_eq!(s.status().performance, 0);
        assert_eq!(s.status().crashes, 10);
        s.expire(8 * DAY);
        assert_eq!(s.status().crashes, 0);
        drop(s);
        std::fs::write(dir.path().join("checkpoint.json"), b"corrupt").unwrap();
        assert_eq!(Store::open(dir.path(), consent(), DAY).unwrap_err(), Error::Invalid);
    }
    #[test]
    fn retries_keep_ids_charge_before_network_and_stop_at_three() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path(), consent(), DAY).unwrap();
        s.observe(1, crash(1), DAY, DAY).unwrap();
        let a = s.prepare(1, DAY).unwrap().unwrap();
        assert_eq!(s.state.budget.crash_requests, 1);
        s.complete(&a, Delivery::Retry).unwrap();
        drop(s);
        let mut s = Store::open(dir.path(), consent(), DAY + 600).unwrap();
        let b = s.prepare(1, DAY + 600).unwrap().unwrap();
        assert_eq!(a.report, b.report);
        assert_eq!(a.bytes, b.bytes);
        s.complete(&b, Delivery::Retry).unwrap();
        let c = s.prepare(1, DAY + 1200).unwrap().unwrap();
        s.complete(&c, Delivery::Retry).unwrap();
        assert_eq!(s.status().crashes, 0);
        assert_eq!(s.state.budget.crash_requests, 3);
    }
    #[test]
    fn performance_retry_roots_share_twelve_attempt_cap() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path(), consent(), DAY).unwrap();
        for _ in 0..20 {
            s.enqueue(interval(), DAY, DAY).unwrap();
        }
        for i in 0..12 {
            let a = s.prepare(1, DAY + i * 900).unwrap().unwrap();
            s.complete(&a, Delivery::Accepted).unwrap();
        }
        assert!(s.prepare(1, DAY + 12000).unwrap().is_none());
        assert_eq!(s.state.budget.perf_roots, 12);
    }
    #[test]
    fn category_limits_clear_pending_and_pause_without_cross_category_conversion() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path(), consent(), DAY).unwrap();
        s.enqueue(interval(), DAY, DAY).unwrap();
        s.enqueue(crash(1), DAY, DAY).unwrap();
        let a = s.prepare(1, DAY).unwrap().unwrap();
        s.complete(
            &a,
            Delivery::Limited {
                errors_until: DAY + 3600,
                transactions_until: 0,
            },
        )
        .unwrap();
        assert_eq!(s.status().crashes, 0);
        assert_eq!(s.status().performance, 1);
    }
    #[test]
    fn clipboard_family_preserves_spent_budget_and_shares_the_global_seal_cap() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path(), consent(), DAY).unwrap();
        s.state.budget.perf_roots = 12;
        s.persist().unwrap();
        drop(s);
        let path = dir.path().join("checkpoint.json");
        let mut checkpoint: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        checkpoint.as_object_mut().unwrap().remove("clipboard_reservoirs");
        std::fs::write(&path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        let mut s = Store::open(dir.path(), consent(), DAY + 1).unwrap();
        assert_eq!(s.state.budget.perf_roots, 12);
        assert_eq!(s.consent(), consent());
        for metric in [
            Metric::DragQueueWait,
            Metric::LinkSetup {
                result: SetupResult::Ready,
            },
            Metric::InputReceiveToNativeComplete,
            Metric::AudioPushToPresentationEstimate,
            Metric::ClipboardPublish,
        ] {
            for _ in 0..3 {
                let Report::MeasuredInterval(mut sample) = interval() else {
                    unreachable!()
                };
                sample.context.metric = metric;
                s.offer(Report::MeasuredInterval(sample), DAY + 2).unwrap();
            }
        }
        assert_eq!(s.state.clipboard_reservoirs[0].candidates.len(), 2);
        for metric in [
            Metric::DragPrepareStage {
                stage: PrepareStage::ParentVisible,
            },
            Metric::NetworkRtt,
            Metric::InputQueueWaitWindowMax,
            Metric::AudioBuffered {
                stage: AudioStage::Total,
            },
        ] {
            let Report::MeasuredInterval(sample) = interval() else {
                unreachable!()
            };
            let d = Report::PerformanceDistribution(PerformanceDistribution {
                context: MeasurementContext {
                    metric,
                    ..sample.context
                },
                histogram: HistogramSchema::Duration14,
                bins: sampling::singleton(10),
                sample_count: 1,
                missing: 0,
                saturated: false,
                window_us: Some(10),
                seal_us: 1,
            });
            d.validate().unwrap();
            s.offer(d, DAY + 2).unwrap();
        }
        assert_eq!(
            s.state
                .reservoirs
                .iter()
                .chain(&s.state.clipboard_reservoirs)
                .map(|r| r.candidates.len())
                .sum::<usize>(),
            14
        );
        s.advance(2 * DAY).unwrap();
        assert_eq!(s.state.budget.sealed, 12);
        assert_eq!(s.status().performance, 12);
        s.advance(2 * DAY + 1).unwrap();
        assert_eq!(s.status().performance, 12);
        s.set_consent(consent().change(1, Choice::Denied, POLICY_VERSION).unwrap())
            .unwrap();
        assert!(!s.has_work());
        assert_eq!(s.state.budget.sealed, 12);
    }

    #[test]
    fn no_startup_midnight_or_shutdown_ping_and_reservoirs_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path(), consent(), DAY).unwrap();
        s.advance(2 * DAY).unwrap();
        assert!(!s.has_work());
        for _ in 0..10000 {
            s.offer(interval(), 2 * DAY + 1).unwrap();
        }
        assert_eq!(s.state.reservoirs[0].candidates.len(), 2);
        assert_eq!(s.state.reservoirs[0].seen, 10000);
        s.persist().unwrap();
        drop(s);
        let mut s = Store::open(dir.path(), consent(), 2 * DAY + 2).unwrap();
        assert_eq!(s.status().performance, 0);
        s.advance(3 * DAY).unwrap();
        assert_eq!(s.status().performance, 2);
        s.advance(3 * DAY).unwrap();
        assert_eq!(s.status().performance, 2);
        let ids: Vec<_> = s.state.queue.iter().map(|q| q.report.event_id).collect();
        assert_ne!(ids[0], ids[1]);
    }
}
