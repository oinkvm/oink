//! Mobile diagnostics: the bounded PC proxy store for segments that phones
//! send live (the phone-side collector is `oink_mobile_core::diagnostics`).
//! The store is separate from the normal recorder and is the only side that
//! persists mobile diagnostic segments.  The v2 Diagnostics* adapter maps these values to the
//! existing Message variants; this module does not define another wire enum.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const MOBILE_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MOBILE_SEGMENT_LIMIT: usize = 256 * 1024;
pub(crate) const MOBILE_PROXY_HOLDER_LIMIT: u64 = 256 * 1024 * 1024;
/// Roll size of one per-phone append log. A phone flushing every 10 s writes
/// roughly 0.5 KiB per batch, so a log covers hours; eviction is per log.
pub(crate) const MOBILE_LOG_FILE_LIMIT: u64 = 1024 * 1024;
/// Logs not appended to for 7 days are deleted even under the size caps.
pub(crate) const MOBILE_RETENTION_US: i64 = 7 * 24 * 60 * 60 * 1_000_000;

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}

fn validate_token(value: &str, name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(valid_token(value), "invalid mobile diagnostic {name}");
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MobileAggregateKind {
    Input,
    AudioPacket,
    TransferProgress,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum MobileDiagnosticEvent {
    State {
        name: String,
        value: String,
    },
    Error {
        stage: String,
        reason: String,
    },
    Link {
        peer: String,
        phase: String,
        rtt_ms: Option<u32>,
        loss_ppm: Option<u32>,
    },
    Audio {
        endpoint: String,
        phase: String,
        owner: Option<String>,
        reason: Option<String>,
    },
    Transfer {
        operation: String,
        stage: String,
        bytes: u64,
    },
    Privilege {
        capability: String,
        phase: String,
        reason: Option<String>,
    },
    Aggregate {
        aggregate: MobileAggregateKind,
        key: String,
        count: u64,
        bytes: u64,
        min_value: Option<u64>,
        max_value: Option<u64>,
        last_value: Option<u64>,
    },
    Sample {
        event: oink_mobile_core::DiagnosticEvent,
    },
    /// A redacted Rust log record of the phone (call site and audited
    /// scalars only, see `oink_mobile_core::DiagnosticLog`).
    Log {
        log: oink_mobile_core::DiagnosticLog,
    },
    CollectorOverflow {
        dropped_segments: u64,
    },
    DroppedEvents {
        count: u64,
    },
}

impl MobileDiagnosticEvent {
    fn validate(&self) -> anyhow::Result<()> {
        let key = |value: &str, name: &str| validate_token(value, name);
        match self {
            Self::State { name, value } => {
                key(name, "state name")?;
                key(value, "state value")?;
            }
            Self::Error { stage, reason } => {
                key(stage, "error stage")?;
                key(reason, "error reason")?;
            }
            Self::Link {
                peer,
                phase,
                rtt_ms,
                loss_ppm,
            } => {
                key(peer, "link peer")?;
                key(phase, "link phase")?;
                anyhow::ensure!(rtt_ms.is_none_or(|value| value <= 600_000), "invalid mobile link rtt");
                anyhow::ensure!(
                    loss_ppm.is_none_or(|value| value <= 1_000_000),
                    "invalid mobile link loss"
                );
            }
            Self::Audio {
                endpoint,
                phase,
                owner,
                reason,
            } => {
                key(endpoint, "audio endpoint")?;
                key(phase, "audio phase")?;
                if let Some(value) = owner {
                    key(value, "audio owner")?;
                }
                if let Some(value) = reason {
                    key(value, "audio reason")?;
                }
            }
            Self::Transfer { operation, stage, .. } => {
                key(operation, "transfer operation")?;
                key(stage, "transfer stage")?;
            }
            Self::Privilege {
                capability,
                phase,
                reason,
            } => {
                key(capability, "privilege capability")?;
                key(phase, "privilege phase")?;
                if let Some(value) = reason {
                    key(value, "privilege reason")?;
                }
            }
            Self::Aggregate { key: aggregate_key, .. } => key(aggregate_key, "aggregate key")?,
            Self::Sample { event } => event.validate().map_err(anyhow::Error::msg)?,
            Self::Log { log } => log.validate().map_err(anyhow::Error::msg)?,
            Self::CollectorOverflow { .. } | Self::DroppedEvents { .. } => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct MobileDiagnosticSegment {
    pub source_device: String,
    pub proxy_device: Option<String>,
    pub session_id: String,
    pub batch_id: String,
    pub sequence: u64,
    pub offset: u64,
    pub first_at_us: i64,
    pub last_at_us: i64,
    pub events: Vec<MobileDiagnosticEvent>,
}

impl MobileDiagnosticSegment {
    pub(crate) fn encoded_len(&self) -> anyhow::Result<usize> {
        Ok(serde_json::to_vec(self)?.len())
    }

    pub(crate) fn validate(&self) -> anyhow::Result<usize> {
        validate_token(&self.source_device, "source device")?;
        if let Some(proxy) = &self.proxy_device {
            validate_token(proxy, "proxy device")?;
        }
        validate_token(&self.session_id, "session")?;
        validate_token(&self.batch_id, "batch")?;
        anyhow::ensure!(self.sequence > 0, "invalid mobile diagnostic sequence");
        anyhow::ensure!(
            self.first_at_us <= self.last_at_us,
            "invalid mobile diagnostic time range"
        );
        anyhow::ensure!(!self.events.is_empty(), "empty mobile diagnostic segment");
        for event in &self.events {
            event.validate()?;
        }
        let bytes = self.encoded_len()?;
        anyhow::ensure!(bytes <= MOBILE_SEGMENT_LIMIT, "mobile diagnostic segment too large");
        Ok(bytes)
    }

    fn key(&self) -> MobileSegmentKey {
        MobileSegmentKey {
            source_device: self.source_device.clone(),
            session_id: self.session_id.clone(),
            batch_id: self.batch_id.clone(),
            sequence: self.sequence,
            offset: self.offset,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Ord, PartialOrd)]
pub(crate) struct MobileSegmentKey {
    pub source_device: String,
    pub session_id: String,
    pub batch_id: String,
    pub sequence: u64,
    pub offset: u64,
}

/// A segment together with the PC that held it. Local records keep this
/// recorder's holder name; segments received during an export keep the remote
/// proxy so cross-holder de-duplication can report provenance.
#[derive(Clone, Debug)]
struct StoredMobileSegment {
    holder_device: String,
    segment: MobileDiagnosticSegment,
}

/// Each source directory holds append-only NDJSON logs plus this counter file.
/// A log is appended while it is `<seq>.open`; once it reaches `file` bytes (or
/// a later process finds it) it is renamed to `<seq>_<first>_<last>.mlog`, so
/// the index learns every sealed log's event range from its name alone.
const EVICTED_FILE: &str = "evicted";
const OPEN_SUFFIX: &str = ".open";
const SEALED_SUFFIX: &str = ".mlog";

/// Capacity policy of one PC's proxy store (see `docs/design/diagnostics.md`,
/// "手机诊断代存"). Sizes are on-disk bytes of the logs.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// Per phone on this holder; the export merge applies the same cap again
    /// across all holders of that phone.
    source: u64,
    /// All phones together on this holder.
    holder: u64,
    /// Roll size of one append log; eviction removes whole logs.
    file: u64,
    /// Logs not appended to for this long are deleted.
    retention_us: i64,
}

impl Limits {
    const DEFAULT: Self = Self {
        source: MOBILE_MAX_BYTES,
        holder: MOBILE_PROXY_HOLDER_LIMIT,
        file: MOBILE_LOG_FILE_LIMIT,
        retention_us: MOBILE_RETENTION_US,
    };
}

#[derive(Clone, Debug, PartialEq)]
struct LogFile {
    seq: u64,
    first_at_us: i64,
    last_at_us: i64,
    bytes: u64,
    /// PC wall clock of the last append, the basis of the retention period.
    modified_us: i64,
    open: bool,
}

impl LogFile {
    fn name(&self) -> String {
        if self.open {
            format!("{:016}{OPEN_SUFFIX}", self.seq)
        } else {
            format!(
                "{:016}_{}_{}{SEALED_SUFFIX}",
                self.seq, self.first_at_us, self.last_at_us
            )
        }
    }

    fn sealed(name: &str) -> Option<(u64, i64, i64)> {
        let mut parts = name.strip_suffix(SEALED_SUFFIX)?.split('_');
        let seq: u64 = parts.next()?.parse().ok()?;
        let first: i64 = parts.next()?.parse().ok()?;
        let last: i64 = parts.next()?.parse().ok()?;
        (parts.next().is_none() && first <= last).then_some((seq, first, last))
    }

    fn overlaps(&self, start: i64, end: i64) -> bool {
        self.bytes > 0 && self.last_at_us >= start && self.first_at_us <= end
    }
}

#[derive(Clone, Debug, Default)]
struct SourceLog {
    /// Ascending `seq`; only the last one can be open.
    files: Vec<LogFile>,
    /// Keys in the open log. The phone resends a batch whose upload was not
    /// acknowledged, and that resend arrives while the same log is open.
    /// Older duplicates are removed by the export merge.
    recent: BTreeSet<MobileSegmentKey>,
    evicted_bytes: u64,
    /// Newest event time of any evicted log; exports starting at or before it
    /// miss data and are partial.
    evicted_through_us: Option<i64>,
    /// A record or log could not be read back in this process.
    unreadable: bool,
}

impl SourceLog {
    fn bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes).sum()
    }

    fn evicted_in(&self, start: i64) -> bool {
        self.evicted_through_us.is_some_and(|through| through >= start)
    }

    fn evict(&mut self, file: &LogFile) {
        self.evicted_bytes = self.evicted_bytes.saturating_add(file.bytes);
        self.evicted_through_us = Some(
            self.evicted_through_us
                .map_or(file.last_at_us, |through| through.max(file.last_at_us)),
        );
    }
}

#[derive(Clone, Debug)]
enum Index {
    /// Nothing read yet. Recorder startup never touches the proxy directory;
    /// the first upload, membership check or export loads the index.
    Unloaded,
    Ready(BTreeMap<String, SourceLog>),
    /// The directory failed. Only the proxy store is off (uploads fail,
    /// exports say partial); the recorder and sharing keep running.
    Unavailable,
}

/// The PC side of mobile diagnostics. Memory holds an index of log files, not
/// segments; exports read the logs from disk one source at a time.
#[derive(Clone, Debug)]
pub(crate) struct MobileProxyStore {
    root: PathBuf,
    holder_device: String,
    writable: bool,
    limits: Limits,
    index: Index,
    /// Segments other holders sent during an export, keyed per holder.
    remote: BTreeMap<(MobileSegmentKey, String), MobileDiagnosticSegment>,
    /// Hard links of an export snapshot; removed with the last clone.
    pin: Option<Arc<tempfile::TempDir>>,
}

fn now_us() -> i64 {
    system_us(SystemTime::now())
}

fn system_us(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(i64::MAX))
}

fn not_found(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

fn remove_any(path: &Path) -> io::Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match result {
        Err(error) if not_found(&error) => Ok(()),
        other => other,
    }
}

struct Records {
    segments: Vec<MobileDiagnosticSegment>,
    bad: bool,
}

fn parse_records(bytes: &[u8], source: Option<&str>) -> Records {
    let mut records = Records {
        segments: Vec::new(),
        bad: false,
    };
    // Every record ends with a newline, so a torn final write is an
    // unterminated line that fails to parse and is reported, not trusted.
    let complete = bytes.len() - bytes.iter().rev().take_while(|byte| **byte != b'\n').count();
    records.bad = complete != bytes.len();
    for line in bytes[..complete]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        match serde_json::from_slice::<MobileDiagnosticSegment>(line) {
            Ok(segment)
                if segment.validate().is_ok() && source.is_none_or(|source| segment.source_device == source) =>
            {
                records.segments.push(segment)
            }
            _ => records.bad = true,
        }
    }
    records
}

fn parse_evicted(text: &str) -> Option<(u64, Option<i64>)> {
    let (bytes, through) = text.trim().split_once(' ')?;
    let through = if through == "-" {
        None
    } else {
        Some(through.parse().ok()?)
    };
    Some((bytes.parse().ok()?, through))
}

/// Builds the index from names and metadata. Only a log left open by an
/// earlier process is read (at most one roll size per source) to learn its
/// event range. A writable scan seals such logs and removes anything that is
/// not part of this layout (temporary files, earlier layouts).
fn scan(root: &Path, writable: bool) -> io::Result<BTreeMap<String, SourceLog>> {
    let mut sources = BTreeMap::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if not_found(&error) => return Ok(sources),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() && valid_token(&name) {
            sources.insert(name, scan_source(&entry.path(), writable)?);
        } else if writable {
            remove_any(&entry.path())?;
        }
    }
    Ok(sources)
}

fn scan_source(dir: &Path, writable: bool) -> io::Result<SourceLog> {
    let mut log = SourceLog::default();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == EVICTED_FILE {
            match fs::read_to_string(&path).ok().and_then(|text| parse_evicted(&text)) {
                Some((bytes, through)) => {
                    log.evicted_bytes = bytes;
                    log.evicted_through_us = through;
                }
                None => log.unreadable = true,
            }
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            // A read-only scan races the live store sealing or pruning.
            Err(error) if not_found(&error) => {
                log.unreadable = true;
                continue;
            }
            Err(error) => return Err(error),
        };
        let modified_us = metadata.modified().map_or(0, system_us);
        if let Some((seq, first_at_us, last_at_us)) = LogFile::sealed(&name) {
            log.files.push(LogFile {
                seq,
                first_at_us,
                last_at_us,
                bytes: metadata.len(),
                modified_us,
                open: false,
            });
            continue;
        }
        if let Some(seq) = name.strip_suffix(OPEN_SUFFIX).and_then(|seq| seq.parse::<u64>().ok()) {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if not_found(&error) => {
                    log.unreadable = true;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let records = parse_records(&bytes, None);
            log.unreadable |= records.bad;
            let range = records
                .segments
                .iter()
                .map(|segment| (segment.first_at_us, segment.last_at_us))
                .reduce(|(first, last), (a, b)| (first.min(a), last.max(b)));
            let Some((first_at_us, last_at_us)) = range else {
                if writable {
                    remove_any(&path)?;
                }
                continue;
            };
            let mut file = LogFile {
                seq,
                first_at_us,
                last_at_us,
                bytes: bytes.len() as u64,
                modified_us,
                open: true,
            };
            if writable {
                // Appends after a restart always start a new log.
                file.open = false;
                fs::rename(&path, dir.join(file.name()))?;
            }
            log.files.push(file);
            continue;
        }
        if writable {
            remove_any(&path)?;
        }
    }
    log.files.sort_by_key(|file| file.seq);
    Ok(log)
}

fn remove_log_file(dir: &Path, log: &mut SourceLog, index: usize) -> io::Result<()> {
    let file = log.files.remove(index);
    match fs::remove_file(dir.join(file.name())) {
        Err(error) if !not_found(&error) => return Err(error),
        _ => {}
    }
    if file.open {
        log.recent.clear();
    }
    log.evict(&file);
    Ok(())
}

/// Applies retention, then the per-source cap, then the holder cap, and
/// persists the eviction counters it changed.
fn prune(root: &Path, limits: Limits, sources: &mut BTreeMap<String, SourceLog>, now: i64) -> io::Result<()> {
    let cutoff = now.saturating_sub(limits.retention_us);
    let mut changed = BTreeSet::new();
    for (source, log) in sources.iter_mut() {
        let dir = root.join(source);
        while let Some(index) = log.files.iter().position(|file| file.modified_us < cutoff) {
            remove_log_file(&dir, log, index)?;
            changed.insert(source.clone());
        }
        while log.bytes() > limits.source && !log.files.is_empty() {
            remove_log_file(&dir, log, 0)?;
            changed.insert(source.clone());
        }
    }
    while sources.values().map(SourceLog::bytes).sum::<u64>() > limits.holder {
        let Some((source, index)) = sources
            .iter()
            .flat_map(|(source, log)| {
                log.files
                    .iter()
                    .enumerate()
                    .map(move |(index, file)| ((file.modified_us, file.seq), source, index))
            })
            .min()
            .map(|(_, source, index)| (source.clone(), index))
        else {
            break;
        };
        let log = sources.get_mut(&source).expect("pruned source is indexed");
        remove_log_file(&root.join(&source), log, index)?;
        changed.insert(source);
    }
    for source in changed {
        let log = &sources[&source];
        let through = log
            .evicted_through_us
            .map_or_else(|| "-".to_owned(), |through| through.to_string());
        super::store::replace(
            &root.join(&source).join(EVICTED_FILE),
            format!("{} {through}", log.evicted_bytes).as_bytes(),
            false,
        )?;
    }
    Ok(())
}

/// Index-only summary used before streaming an export.
#[derive(Clone, Debug, Default)]
pub(crate) struct MobileOverview {
    pub sources: Vec<String>,
    /// On-disk bytes of the logs overlapping the range, at most
    /// `MOBILE_MAX_BYTES` per source. An upper bound: records outside the
    /// range that share a log are counted too.
    pub store_bytes: u64,
    pub partial: bool,
}

impl MobileProxyStore {
    pub(crate) fn new(root: PathBuf, holder_device: String) -> Self {
        Self::with_limits(root, holder_device, Limits::DEFAULT)
    }

    fn with_limits(root: PathBuf, holder_device: String, limits: Limits) -> Self {
        let index = if valid_token(&holder_device) {
            Index::Unloaded
        } else {
            Index::Unavailable
        };
        Self {
            root,
            holder_device,
            writable: true,
            limits,
            index,
            remote: BTreeMap::new(),
            pin: None,
        }
    }

    pub(crate) fn set_holder(&mut self, device: String) {
        self.holder_device = device;
    }

    pub(crate) fn unavailable(&self) -> bool {
        matches!(self.index, Index::Unavailable)
    }

    /// Runs a mutation on the loaded index. Any I/O failure turns the proxy
    /// store off for this process instead of guessing what is on disk.
    fn with_index<T>(
        &mut self,
        now: i64,
        work: impl FnOnce(&Path, Limits, &mut BTreeMap<String, SourceLog>) -> io::Result<T>,
    ) -> anyhow::Result<T> {
        anyhow::ensure!(self.writable, "mobile proxy snapshot is read only");
        let first = matches!(self.index, Index::Unloaded);
        if first {
            self.index = match scan(&self.root, true) {
                Ok(sources) => Index::Ready(sources),
                Err(error) => {
                    tracing::warn!(%error, "mobile diagnostic proxy store unavailable");
                    Index::Unavailable
                }
            };
        }
        let Index::Ready(sources) = &mut self.index else {
            anyhow::bail!("mobile diagnostic proxy store unavailable");
        };
        let result = (|| {
            if first {
                prune(&self.root, self.limits, sources, now)?;
            }
            work(&self.root, self.limits, sources)
        })();
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                tracing::warn!(%error, "mobile diagnostic proxy store unavailable");
                self.index = Index::Unavailable;
                Err(error.into())
            }
        }
    }

    pub(crate) fn accept(&mut self, segment: MobileDiagnosticSegment, proxy: &str) -> anyhow::Result<bool> {
        self.accept_at(segment, proxy, now_us())
    }

    fn accept_at(&mut self, segment: MobileDiagnosticSegment, proxy: &str, now: i64) -> anyhow::Result<bool> {
        segment.validate()?;
        anyhow::ensure!(segment.proxy_device.as_deref() == Some(proxy), "mobile proxy mismatch");
        let mut line = serde_json::to_vec(&segment)?;
        line.push(b'\n');
        self.with_index(now, move |root, limits, sources| {
            let source = segment.source_device.clone();
            let key = segment.key();
            let log = sources.entry(source.clone()).or_default();
            if log.recent.contains(&key) {
                return Ok(false);
            }
            let dir = root.join(&source);
            fs::create_dir_all(&dir)?;
            let append = log
                .files
                .last()
                .is_some_and(|file| file.open && file.bytes + line.len() as u64 <= limits.file);
            if !append {
                if let Some(file) = log.files.last_mut().filter(|file| file.open) {
                    let sealed = LogFile {
                        open: false,
                        ..file.clone()
                    };
                    fs::rename(dir.join(file.name()), dir.join(sealed.name()))?;
                    *file = sealed;
                }
                let seq = log.files.last().map_or(1, |file| file.seq + 1);
                log.files.push(LogFile {
                    seq,
                    first_at_us: segment.first_at_us,
                    last_at_us: segment.last_at_us,
                    bytes: 0,
                    modified_us: now,
                    open: true,
                });
                log.recent.clear();
            }
            let file = log.files.last_mut().expect("open mobile log");
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(file.name()))?
                .write_all(&line)?;
            file.bytes += line.len() as u64;
            file.first_at_us = file.first_at_us.min(segment.first_at_us);
            file.last_at_us = file.last_at_us.max(segment.last_at_us);
            file.modified_us = now;
            log.recent.insert(key);
            prune(root, limits, sources, now)?;
            Ok(true)
        })
    }

    /// Deletes the logs of phones that are no longer group members and applies
    /// retention. Every roster device counts as a member, so a phone whose
    /// facts are not known yet keeps its data.
    pub(crate) fn retain_sources(&mut self, members: &BTreeSet<String>) -> anyhow::Result<()> {
        self.retain_sources_at(members, now_us())
    }

    fn retain_sources_at(&mut self, members: &BTreeSet<String>, now: i64) -> anyhow::Result<()> {
        self.with_index(now, |root, limits, sources| {
            let removed = sources
                .keys()
                .filter(|source| !members.contains(*source))
                .cloned()
                .collect::<Vec<_>>();
            for source in removed {
                remove_any(&root.join(&source))?;
                sources.remove(&source);
            }
            prune(root, limits, sources, now)
        })
    }

    pub(crate) fn clear(&mut self) -> anyhow::Result<()> {
        self.remote.clear();
        match fs::remove_dir_all(&self.root) {
            Err(error) if !not_found(&error) => {
                self.index = Index::Unavailable;
                Err(error.into())
            }
            _ => {
                self.index = Index::Ready(BTreeMap::new());
                Ok(())
            }
        }
    }

    /// Read-only copy for one export. Logs are hard-linked like the desktop
    /// ring, so live appends and pruning cannot change what the export reads;
    /// the open log is read only up to the length recorded here. A failure
    /// gives an unavailable copy (the export says partial), never an error.
    pub(crate) fn pin(&self, parent: &Path, range: Option<(i64, i64)>) -> Self {
        self.pin_at(parent, range, now_us())
    }

    fn pin_at(&self, parent: &Path, range: Option<(i64, i64)>, now: i64) -> Self {
        let (start, end) = range.unwrap_or((i64::MIN, i64::MAX));
        let mut pinned = Self {
            root: PathBuf::new(),
            holder_device: self.holder_device.clone(),
            writable: false,
            limits: self.limits,
            index: Index::Unavailable,
            remote: self
                .remote
                .iter()
                .filter(|(_, segment)| segment.last_at_us >= start && segment.first_at_us <= end)
                .map(|(key, segment)| (key.clone(), segment.clone()))
                .collect(),
            pin: None,
        };
        let result = (|| -> io::Result<_> {
            let mut sources = match &self.index {
                Index::Ready(sources) => sources.clone(),
                Index::Unloaded => scan(&self.root, false)?,
                Index::Unavailable => return Err(io::Error::other("mobile diagnostic proxy store unavailable")),
            };
            let dir = tempfile::Builder::new().prefix("export-mobile-").tempdir_in(parent)?;
            let cutoff = now.saturating_sub(self.limits.retention_us);
            for (source, log) in &mut sources {
                log.recent.clear();
                let files = std::mem::take(&mut log.files);
                for file in files {
                    if file.modified_us < cutoff {
                        // Expired but not yet pruned by a live mutation.
                        log.evict(&file);
                        continue;
                    }
                    if !file.overlaps(start, end) {
                        continue;
                    }
                    let target = dir.path().join(source);
                    fs::create_dir_all(&target)?;
                    match fs::hard_link(self.root.join(source).join(file.name()), target.join(file.name())) {
                        Ok(()) => log.files.push(file),
                        Err(error) if not_found(&error) => log.unreadable = true,
                        Err(error) => return Err(error),
                    }
                }
            }
            Ok((sources, dir))
        })();
        match result {
            Ok((sources, dir)) => {
                pinned.root = dir.path().into();
                pinned.index = Index::Ready(sources);
                pinned.pin = Some(Arc::new(dir));
            }
            Err(error) => tracing::warn!(%error, "mobile diagnostic proxy snapshot unavailable"),
        }
        pinned
    }

    fn sources_view(&self) -> Option<Cow<'_, BTreeMap<String, SourceLog>>> {
        match &self.index {
            Index::Ready(sources) => Some(Cow::Borrowed(sources)),
            Index::Unloaded => scan(&self.root, false).ok().map(Cow::Owned),
            Index::Unavailable => None,
        }
    }

    fn read_local(&self, source: &str, log: &SourceLog, start: i64, end: i64) -> (Vec<MobileDiagnosticSegment>, bool) {
        let mut segments = Vec::new();
        let mut unreadable = log.unreadable;
        for file in log.files.iter().filter(|file| file.overlaps(start, end)) {
            let read = fs::File::open(self.root.join(source).join(file.name())).and_then(|opened| {
                let mut bytes = Vec::new();
                opened.take(file.bytes).read_to_end(&mut bytes)?;
                Ok(bytes)
            });
            match read {
                Ok(bytes) => {
                    let records = parse_records(&bytes, Some(source));
                    unreadable |= records.bad;
                    segments.extend(
                        records
                            .segments
                            .into_iter()
                            .filter(|segment| segment.last_at_us >= start && segment.first_at_us <= end),
                    );
                }
                Err(_) => unreadable = true,
            }
        }
        (segments, unreadable)
    }

    fn remote_sources(&self, start: i64, end: i64) -> BTreeSet<String> {
        self.remote
            .iter()
            .filter(|(_, segment)| segment.last_at_us >= start && segment.first_at_us <= end)
            .map(|((key, _), _)| key.source_device.clone())
            .collect()
    }

    pub(crate) fn overview(&self, start: i64, end: i64) -> MobileOverview {
        let mut sources = self.remote_sources(start, end);
        let mut overview = MobileOverview::default();
        match self.sources_view() {
            Some(view) => {
                for (source, log) in view.iter() {
                    let bytes = log
                        .files
                        .iter()
                        .filter(|file| file.overlaps(start, end))
                        .map(|file| file.bytes)
                        .sum::<u64>();
                    let partial = log.evicted_in(start) || log.unreadable;
                    if bytes > 0 || partial {
                        sources.insert(source.clone());
                    }
                    overview.store_bytes = overview.store_bytes.saturating_add(bytes.min(MOBILE_MAX_BYTES));
                    overview.partial |= partial;
                }
            }
            None => overview.partial = true,
        }
        overview.sources = sources.into_iter().collect();
        overview
    }

    pub(crate) fn has_source_in_range(&self, source: &str, start: i64, end: i64) -> bool {
        if self.remote_sources(start, end).contains(source) {
            return true;
        }
        self.sources_view()
            .and_then(|view| {
                view.get(source)
                    .map(|log| !self.read_local(source, log, start, end).0.is_empty())
            })
            .unwrap_or(false)
    }

    /// Adds a segment received during a diagnostic export without persisting
    /// it as if this PC were its owner. The holder in the wrapper remains the
    /// remote proxy, which preserves provenance for cross-holder de-duplication.
    pub(crate) fn accept_remote(
        &mut self,
        segment: MobileDiagnosticSegment,
        holder_device: String,
    ) -> anyhow::Result<bool> {
        segment.validate()?;
        validate_token(&holder_device, "mobile diagnostic holder")?;
        anyhow::ensure!(
            segment.proxy_device.as_deref() == Some(holder_device.as_str()),
            "mobile remote holder mismatch"
        );
        let key = (segment.key(), holder_device);
        if self.remote.contains_key(&key) {
            return Ok(false);
        }
        self.remote.insert(key, segment);
        Ok(true)
    }

    /// Reads one phone's data in the range from the local logs and the
    /// segments other holders sent, de-duplicates and keeps the newest
    /// `MOBILE_MAX_BYTES`. Memory is bounded by that one source.
    pub(crate) fn export_source(&self, source: &str, start: i64, end: i64) -> MobileSourceExport {
        let view = self.sources_view();
        let mut partial = view.is_none();
        let mut evicted_bytes = 0;
        let mut candidates = Vec::new();
        if let Some(log) = view.as_ref().and_then(|view| view.get(source)) {
            let (segments, unreadable) = self.read_local(source, log, start, end);
            partial |= unreadable;
            if log.evicted_in(start) {
                partial = true;
                evicted_bytes = log.evicted_bytes;
            }
            candidates.extend(segments.into_iter().map(|segment| StoredMobileSegment {
                holder_device: self.holder_device.clone(),
                segment,
            }));
        }
        candidates.extend(
            self.remote
                .iter()
                .filter(|((key, _), segment)| {
                    key.source_device == source && segment.last_at_us >= start && segment.first_at_us <= end
                })
                .map(|((_, holder_device), segment)| StoredMobileSegment {
                    holder_device: holder_device.clone(),
                    segment: segment.clone(),
                }),
        );
        let mut export = merge_source(candidates);
        export.partial |= partial;
        export.evicted_bytes = export.evicted_bytes.saturating_add(evicted_bytes);
        export
    }
}

fn merge_source(mut values: Vec<StoredMobileSegment>) -> MobileSourceExport {
    values.sort_by(|a, b| {
        (
            &a.holder_device,
            &a.segment.session_id,
            a.segment.sequence,
            a.segment.offset,
        )
            .cmp(&(
                &b.holder_device,
                &b.segment.session_id,
                b.segment.sequence,
                b.segment.offset,
            ))
    });
    let mut unique = BTreeMap::<MobileSegmentKey, StoredMobileSegment>::new();
    let mut duplicate_holders = BTreeSet::new();
    for value in values {
        match unique.entry(value.segment.key()) {
            std::collections::btree_map::Entry::Occupied(previous) => {
                if previous.get().holder_device != value.holder_device {
                    duplicate_holders.insert(previous.get().holder_device.clone());
                    duplicate_holders.insert(value.holder_device);
                }
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(value);
            }
        }
    }
    let mut selected = unique.into_values().collect::<Vec<_>>();
    selected.sort_by_key(|value| {
        std::cmp::Reverse((value.segment.last_at_us, value.segment.sequence, value.segment.offset))
    });
    let mut bytes = 0u64;
    let mut evicted_bytes = 0u64;
    let mut segments = Vec::new();
    for value in selected {
        let size = serde_json::to_vec(&value.segment)
            .map(|bytes| bytes.len() as u64)
            .unwrap_or(u64::MAX);
        if bytes.saturating_add(size) <= MOBILE_MAX_BYTES {
            bytes = bytes.saturating_add(size);
            segments.push(MobileExportSegment {
                holder_device: value.holder_device,
                segment: value.segment,
                bytes: size,
            });
        } else {
            evicted_bytes = evicted_bytes.saturating_add(size);
        }
    }
    segments.sort_by(|a, b| {
        (
            a.segment.first_at_us,
            a.segment.last_at_us,
            a.segment.sequence,
            &a.holder_device,
        )
            .cmp(&(
                b.segment.first_at_us,
                b.segment.last_at_us,
                b.segment.sequence,
                &b.holder_device,
            ))
    });
    let reported_loss = segments.iter().any(|value| {
        value.segment.events.iter().any(|event| {
            matches!(
                event,
                MobileDiagnosticEvent::CollectorOverflow { .. } | MobileDiagnosticEvent::DroppedEvents { .. }
            )
        })
    });
    MobileSourceExport {
        segments,
        bytes,
        evicted_bytes,
        duplicate_holders: duplicate_holders.into_iter().collect(),
        partial: reported_loss || evicted_bytes > 0,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MobileExportSegment {
    pub holder_device: String,
    pub segment: MobileDiagnosticSegment,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct MobileSourceExport {
    pub segments: Vec<MobileExportSegment>,
    pub bytes: u64,
    pub evicted_bytes: u64,
    pub duplicate_holders: Vec<String>,
    pub partial: bool,
}

/// Group alias of a 64-hex device id; other values are already aliases.
pub(crate) fn alias_device(anonymizer: &super::Anonymizer, value: &str) -> String {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return value.to_owned();
    }
    let mut key = [0u8; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).expect("checked ASCII hex");
    }
    anonymizer.device(oink_foundation::DeviceId(key))
}

impl MobileSourceExport {
    pub(crate) fn anonymize(&mut self, anonymizer: &super::Anonymizer) {
        for item in &mut self.segments {
            item.holder_device = alias_device(anonymizer, &item.holder_device);
            item.segment.source_device = alias_device(anonymizer, &item.segment.source_device);
            item.segment.proxy_device = item
                .segment
                .proxy_device
                .as_deref()
                .map(|proxy| alias_device(anonymizer, proxy));
        }
        for holder in &mut self.duplicate_holders {
            *holder = alias_device(anonymizer, holder);
        }
    }

    /// Per-source entry of the manifest's `mobile_diagnostics.sources`.
    pub(crate) fn manifest(&self) -> serde_json::Value {
        serde_json::json!({
            "bytes": self.bytes,
            "segments": self.segments.len(),
            "partial": self.partial,
            "evicted_bytes": self.evicted_bytes,
            "duplicate_holders": self.duplicate_holders,
        })
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    const DAY_US: i64 = 24 * 60 * 60 * 1_000_000;

    fn event(name: &str) -> MobileDiagnosticEvent {
        MobileDiagnosticEvent::State {
            name: name.into(),
            value: "running".into(),
        }
    }
    fn segment(source: &str, session: &str, batch: &str, sequence: u64, at: i64) -> MobileDiagnosticSegment {
        MobileDiagnosticSegment {
            source_device: source.into(),
            proxy_device: None,
            session_id: session.into(),
            batch_id: batch.into(),
            sequence,
            offset: 0,
            first_at_us: at,
            last_at_us: at,
            events: vec![event("link")],
        }
    }
    fn proxied(source: &str, sequence: u64, at: i64, proxy: &str) -> MobileDiagnosticSegment {
        let mut item = segment(source, "session", "batch", sequence, at);
        item.proxy_device = Some(proxy.into());
        item
    }
    fn files(root: &Path) -> usize {
        fs::read_dir(root)
            .map(|sources| {
                sources
                    .flatten()
                    .filter(|entry| entry.path().is_dir())
                    .flat_map(|source| fs::read_dir(source.path()).unwrap().flatten())
                    .filter(|entry| entry.file_name() != EVICTED_FILE)
                    .count()
            })
            .unwrap_or(0)
    }
    fn small(root: PathBuf, holder: &str, source: u64, total: u64, file: u64) -> MobileProxyStore {
        MobileProxyStore::with_limits(
            root,
            holder.into(),
            Limits {
                source,
                holder: total,
                file,
                retention_us: 7 * DAY_US,
            },
        )
    }

    #[test]
    fn proxy_deduplicates_across_holders_and_orders_by_time() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let mut a = MobileProxyStore::new(first.path().join("mobile-proxy"), "pc-a".into());
        let old = proxied("mobile", 1, 1, "pc-a");
        let mut duplicate = old.clone();
        duplicate.proxy_device = Some("pc-b".into());
        let newest = proxied("mobile", 2, 2, "pc-b");
        assert!(a.accept(old, "pc-a").unwrap());
        let mut export = a.pin(second.path(), None);
        assert!(export.accept_remote(duplicate, "pc-b".into()).unwrap());
        assert!(export.accept_remote(newest, "pc-b".into()).unwrap());
        let source = export.export_source("mobile", i64::MIN, i64::MAX);
        assert_eq!(source.segments.len(), 2);
        assert_eq!(source.segments[0].segment.sequence, 1);
        assert_eq!(source.segments[1].segment.sequence, 2);
        assert_eq!(source.duplicate_holders, vec!["pc-a", "pc-b"]);
        assert!(!source.partial);
        assert_eq!(source.manifest()["segments"], 2);
        assert!(!export.overview(i64::MIN, i64::MAX).partial);
    }

    #[test]
    fn phone_log_records_are_stored_and_exported_and_unaudited_fields_are_refused() {
        use oink_mobile_core::{DiagnosticLog, LogLevel, LogValue};
        let temp = tempfile::tempdir().unwrap();
        let mut store = MobileProxyStore::new(temp.path().join("mobile-proxy"), "pc".into());
        let log = DiagnosticLog {
            timestamp_ms: 5,
            level: LogLevel::Warn,
            module: "oink_network::link::core".into(),
            line: 902,
            fields: vec![
                ("kind".into(), LogValue::Label("link_lost".into())),
                ("link_id".into(), LogValue::U64(4)),
            ],
        };
        let mut item = proxied("mobile", 1, 5_000, "pc");
        item.events = vec![MobileDiagnosticEvent::Log { log: log.clone() }];
        assert!(store.accept(item.clone(), "pc").unwrap());
        let export = store.pin(temp.path(), None).export_source("mobile", i64::MIN, i64::MAX);
        assert_eq!(export.segments.len(), 1);
        assert_eq!(export.segments[0].segment.events, item.events);

        let mut leaked = item;
        leaked.sequence = 2;
        leaked.events = vec![MobileDiagnosticEvent::Log {
            log: DiagnosticLog {
                fields: vec![("text".into(), LogValue::Label("hello".into()))],
                ..log
            },
        }];
        assert!(leaked.validate().is_err());
        assert!(store.accept(leaked, "pc").is_err());
    }

    #[test]
    fn source_total_is_capped_by_file_bytes_and_marks_eviction() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mobile-proxy");
        let mut store = small(root.clone(), "pc", 200_000, 1_000_000, 100_000);
        for sequence in 1..=3 {
            let mut item = proxied("mobile", sequence, sequence as i64, "pc");
            item.events = (0..1_000)
                .map(|_| MobileDiagnosticEvent::State {
                    name: "padding".into(),
                    value: "x".repeat(128),
                })
                .collect();
            assert!(store.accept_at(item, "pc", 0).unwrap());
        }
        let on_disk = fs::read_dir(root.join("mobile"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name() != EVICTED_FILE)
            .map(|entry| entry.metadata().unwrap().len())
            .sum::<u64>();
        assert!(on_disk <= 200_000, "{on_disk}");
        let pinned = store.pin_at(temp.path(), None, 0);
        let export = pinned.export_source("mobile", i64::MIN, i64::MAX);
        assert!(export.partial);
        assert!(export.evicted_bytes > 0);
        assert_eq!(export.segments.last().unwrap().segment.sequence, 3);
        // Eviction is about old data: a range after it is complete.
        assert!(!pinned.export_source("mobile", 3, 3).partial);
    }

    #[test]
    fn many_uploads_share_rolled_logs_instead_of_one_file_each() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mobile-proxy");
        let mut store = small(root.clone(), "pc", 1 << 20, 1 << 22, 16 * 1024);
        for sequence in 1..=500 {
            assert!(
                store
                    .accept_at(proxied("mobile", sequence, sequence as i64, "pc"), "pc", 0)
                    .unwrap()
            );
        }
        let count = files(&root);
        assert!(count < 20, "{count} files for 500 uploads");
        let export = store
            .pin_at(temp.path(), None, 0)
            .export_source("mobile", i64::MIN, i64::MAX);
        assert_eq!(export.segments.len(), 500);
        assert!(!export.partial);
        // A resend of an unacknowledged batch is not stored twice.
        assert!(!store.accept_at(proxied("mobile", 500, 500, "pc"), "pc", 0).unwrap());
    }

    #[test]
    fn logs_expire_after_the_retention_period() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mobile-proxy");
        let mut store = small(root.clone(), "pc", 1 << 20, 1 << 22, 300);
        assert!(store.accept_at(proxied("mobile", 1, 10, "pc"), "pc", 0).unwrap());
        assert!(
            store
                .accept_at(proxied("mobile", 2, 20, "pc"), "pc", 6 * DAY_US)
                .unwrap()
        );
        // Not yet pruned on disk: an export already leaves the expired log out.
        let pinned = store.pin_at(temp.path(), None, 7 * DAY_US + 1);
        let export = pinned.export_source("mobile", i64::MIN, i64::MAX);
        assert_eq!(export.segments.len(), 1);
        assert!(export.partial);
        assert!(
            store
                .accept_at(proxied("mobile", 3, 30, "pc"), "pc", 7 * DAY_US + 1)
                .unwrap()
        );
        let export = store
            .pin_at(temp.path(), None, 7 * DAY_US + 1)
            .export_source("mobile", i64::MIN, i64::MAX);
        assert_eq!(
            export
                .segments
                .iter()
                .map(|item| item.segment.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(export.partial);
        assert!(
            !store
                .pin_at(temp.path(), None, 7 * DAY_US + 1)
                .export_source("mobile", 20, 30)
                .partial
        );
    }

    #[test]
    fn removed_members_lose_their_logs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mobile-proxy");
        let mut store = MobileProxyStore::new(root.clone(), "pc".into());
        assert!(store.accept(proxied("kept", 1, 1, "pc"), "pc").unwrap());
        assert!(store.accept(proxied("gone", 1, 1, "pc"), "pc").unwrap());
        store.retain_sources(&BTreeSet::from(["kept".to_owned()])).unwrap();
        assert!(root.join("kept").exists());
        assert!(!root.join("gone").exists());
        assert!(!store.has_source_in_range("gone", i64::MIN, i64::MAX));
        assert!(store.has_source_in_range("kept", i64::MIN, i64::MAX));
    }

    #[test]
    fn opening_reads_nothing_and_a_broken_directory_only_disables_the_proxy() {
        let temp = tempfile::tempdir().unwrap();
        // The proxy root is a file: every directory operation fails.
        let root = temp.path().join("mobile-proxy");
        fs::write(&root, b"not a directory").unwrap();
        let mut store = MobileProxyStore::new(root.clone(), "pc".into());
        assert!(!store.unavailable(), "construction must not touch the disk");
        assert!(store.accept(proxied("mobile", 1, 1, "pc"), "pc").is_err());
        assert!(store.unavailable());
        assert!(store.overview(i64::MIN, i64::MAX).partial);
        let pinned = store.pin(temp.path(), None);
        assert!(pinned.unavailable());
        assert!(pinned.export_source("mobile", i64::MIN, i64::MAX).partial);
    }

    #[test]
    fn export_snapshot_is_stable_while_the_live_store_appends_and_prunes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mobile-proxy");
        let mut live = small(root.clone(), "pc", 600, 1 << 20, 300);
        assert!(live.accept_at(proxied("mobile", 1, 10, "pc"), "pc", 0).unwrap());
        let pinned = live.pin_at(temp.path(), None, 0);
        for sequence in 2..=6 {
            assert!(
                live.accept_at(proxied("mobile", sequence, sequence as i64 * 10, "pc"), "pc", 0)
                    .unwrap()
            );
        }
        let export = pinned.export_source("mobile", i64::MIN, i64::MAX);
        assert_eq!(
            export
                .segments
                .iter()
                .map(|item| item.segment.sequence)
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert!(!export.partial);
    }

    #[test]
    fn logs_survive_reopen_and_a_torn_tail_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mobile-proxy");
        let mut live = MobileProxyStore::new(root.clone(), "pc".into());
        assert!(live.accept(proxied("mobile", 1, 10, "pc"), "pc").unwrap());
        assert!(live.accept(proxied("mobile", 2, 20, "pc"), "pc").unwrap());
        let open = fs::read_dir(root.join("mobile"))
            .unwrap()
            .flatten()
            .find(|entry| entry.file_name().to_string_lossy().ends_with(OPEN_SUFFIX))
            .unwrap()
            .path();
        fs::OpenOptions::new()
            .append(true)
            .open(&open)
            .unwrap()
            .write_all(b"{\"source_dev")
            .unwrap();
        drop(live);

        let mut reopened = MobileProxyStore::new(root.clone(), "pc".into());
        let export = reopened
            .pin(temp.path(), None)
            .export_source("mobile", i64::MIN, i64::MAX);
        assert_eq!(export.segments.len(), 2);
        assert!(export.partial, "a torn record is reported");
        assert!(reopened.accept(proxied("mobile", 3, 30, "pc"), "pc").unwrap());
        assert!(!open.exists(), "the earlier open log is sealed");
        let export = reopened
            .pin(temp.path(), None)
            .export_source("mobile", i64::MIN, i64::MAX);
        assert_eq!(
            export
                .segments
                .iter()
                .map(|item| item.segment.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
}
