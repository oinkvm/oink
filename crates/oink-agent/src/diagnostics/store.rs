use super::*;
use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, Write};
#[path = "wire_store.rs"]
pub(super) mod wire;
const MAX_CRASH_PACKAGES: usize = 10;
const EXPORT_BUFFER: usize = 256 * 1024;
use std::path::Path;
use zip::write::SimpleFileOptions;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Manual,
    Emergency,
    RecoveredCrash,
    PeerRestart,
}
impl Reason {
    fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Emergency => "emergency",
            Self::RecoveredCrash => "recovered_crash",
            Self::PeerRestart => "peer_restart",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BundleInfo {
    pub path: String,
    pub at_ms: i64,
    pub reason: Reason,
    pub bytes: u64,
    pub included_device_count: Option<usize>,
    pub missing_device_count: Option<usize>,
    pub cutoff_complete: Option<bool>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RecorderStatus {
    /// Effective recording state (configuration AND not suspended/stopped).
    pub enabled: bool,
    /// User preference, including changes made during full pause.
    pub configured_enabled: bool,
    /// Full pause prevents recording regardless of the user preference.
    pub suspended: bool,
    pub recovering: bool,
    pub bytes: u64,
    pub limit_bytes: u64,
    pub queued: usize,
    pub dropped: u64,
    pub errors: u64,
    pub segments: usize,
    pub oldest_utc_us: Option<i64>,
    pub newest_utc_us: Option<i64>,
    /// Aggregate count of rejected local diagnostic segments.
    pub corrupt_segments: u64,
    pub discarded_segments: DiscardCounts,
    pub evicted_segments: u64,
}
/// Recovery outcomes only; no segment names, payloads or free-form errors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscardCounts {
    pub read: u64,
    pub decompress: u64,
    pub decode: u64,
    pub validation: u64,
    pub format: u64,
    pub size_limit: u64,
}
#[derive(Clone, Copy)]
pub(crate) enum DiscardClass {
    Read,
    Decompress,
    Decode,
    Validation,
    Format,
    SizeLimit,
}
impl DiscardCounts {
    fn add(&mut self, class: DiscardClass) {
        *match class {
            DiscardClass::Read => &mut self.read,
            DiscardClass::Decompress => &mut self.decompress,
            DiscardClass::Decode => &mut self.decode,
            DiscardClass::Validation => &mut self.validation,
            DiscardClass::Format => &mut self.format,
            DiscardClass::SizeLimit => &mut self.size_limit,
        } += 1;
    }
    fn report(self) {
        let Self {
            read,
            decompress,
            decode,
            validation,
            format,
            size_limit,
        } = self;
        if read + decompress + decode + validation + format + size_limit > 0 {
            tracing::warn!(
                read,
                decompress,
                decode,
                validation,
                format,
                size_limit,
                "discarded unreadable diagnostic segments"
            );
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    // Provider availability is projected from its typed scalar observation.
    #[serde(skip)]
    pub file_provider: Option<super::ProviderAvailability>,
    /// The configuration module's key-free excerpt; never read the group state file.
    pub group_state: Option<String>,
    pub version: String,
    pub source_tree_sha256: String,
    pub os: String,
    pub os_version: String,
    pub layout: Option<super::layout::CompleteLayout>,
    pub permissions: Option<oink_foundation::input::InputPermissions>,
    /// Only explicitly selected non-identifying scalar settings.
    pub settings: BTreeMap<String, bool>,
    pub routing_numbers: BTreeMap<String, f64>,
    pub button_handoff: Option<oink_routing::ButtonHandoff>,
    pub reference_screen: Option<String>,
    #[serde(default)]
    pub injection_failures: u64,
    pub session_locked: Option<bool>,
    /// Non-identifying Windows backend token capability; unknown/off Windows is None.
    pub backend_uiaccess: Option<bool>,
    pub crash_report: super::crash_report::Report,
}
impl Snapshot {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        if let Some(excerpt) = &self.group_state {
            anyhow::ensure!(
                serde_json::from_str::<serde_json::Value>(excerpt)?.is_object(),
                "invalid configuration excerpt"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum LayoutCaptureKind {
    Current,
    Retained,
    Unavailable,
}
#[derive(Serialize)]
struct LayoutCapture<'a> {
    kind: LayoutCaptureKind,
    instance: Option<&'a str>,
    version: &'a str,
    source_tree_sha256: &'a str,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClockSample {
    pub device: String,
    pub instance: String,
    pub at_us: i64,
    pub source_clock: i64,
    pub local_clock: i64,
    pub local_instance: String,
    pub first_source_mono_us: u64,
    pub last_source_mono_us: u64,
    pub offset_us: i64,
    pub uncertainty_us: u64,
}
impl ClockSample {
    pub fn estimate(device: String, instance: String, t1: i64, t2: i64, t3: i64, t4: i64) -> Option<Self> {
        let rtt = t4.checked_sub(t1)?.checked_sub(t3.checked_sub(t2)?)?;
        if !(0..=10_000_000).contains(&rtt) || t3 < t2 {
            return None;
        }
        Some(Self {
            device,
            instance,
            at_us: t4,
            source_clock: 0,
            local_clock: 0,
            local_instance: String::new(),
            first_source_mono_us: 0,
            last_source_mono_us: 0,
            offset_us: ((t2 as i128 - t1 as i128 + t3 as i128 - t4 as i128) / 2) as i64,
            uncertainty_us: rtt as u64 / 2,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Segment {
    pub schema: u32,
    pub device: String,
    pub instance: String,
    pub seq: u64,
    pub dropped: u64,
    pub snapshot: Snapshot,
    pub clock_samples: Vec<ClockSample>,
    pub rows: Vec<Row>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Meta {
    pub device: String,
    pub instance: String,
    pub seq: u64,
    pub clock_id: i64,
    pub first_mono_us: u64,
    pub last_mono_us: u64,
    pub first_us: i64,
    pub last_us: i64,
    pub bytes: u64,
    pub received_us: i64,
    #[serde(default)]
    pub network: bool,
}
impl Meta {
    fn name(&self) -> String {
        format!(
            "{}-{}-{}.{}",
            self.device,
            self.instance,
            self.seq,
            if self.network { "cbor" } else { "dseg" }
        )
    }
}
#[derive(Clone)]
pub(crate) struct Store {
    dir: PathBuf,
    clock: Arc<clock::Clock>,
    export_source: Option<Arc<tempfile::TempDir>>,
    export_clock: Option<clock::Stamp>,
    pub cancelled: Arc<AtomicBool>,
    recover_enabled: bool,
    #[cfg(all(test, debug_assertions))]
    pub segment_reads: Arc<AtomicU64>,
    pub device: String,
    pub instance: String,
    pub previous_instance: Option<String>,
    next: u64,
    pub snapshot: Snapshot,
    pub(super) network_received: u64,
    pub(super) network_expanded: u64,
    pub(super) network_environments: BTreeMap<String, (String, oink_protocol::v1::dto::Snapshot)>,
    pub(super) network_ends: BTreeMap<String, oink_protocol::v1::dto::DiagnosticsEnd>,
    pub gathered_environments: BTreeMap<String, (String, Snapshot)>,
    pub segments: VecDeque<Meta>,
    pub clocks: Vec<ClockSample>,
    pub bytes: u64,
    pub limit: u64,
    pub evicted: u64,
    pub corrupt: u64,
    discarded: DiscardCounts,
    #[cfg(all(test, debug_assertions))]
    pub saved: tokio::sync::watch::Sender<u64>,
    pub missing_members: BTreeMap<String, String>,
    pub requested_range: Option<(i64, i64)>,
    pub evicted_by_device: BTreeMap<String, u64>,
}
pub(super) fn token_ok(s: &str) -> bool {
    s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    replace(path, bytes, true)
}
// Export staging is disposable. Only the completed zip and flight recorder need
// durability; thousands of staging fsyncs compete with the live recorder's disk.
fn replace(path: &Path, bytes: &[u8], durable: bool) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    if durable {
        f.sync_all()?;
    }
    drop(f);
    fs::rename(tmp, path)
}
// Bytes 0..8 are the magic, byte 8 is schema 1, byte 9 is JSON + raw DEFLATE
// codec 1, and bytes 10..12 are reserved zeroes. The expanded JSON contains one
// named-field Segment with its producer version. No source revision selects a
// decoder. Network collection translates this internal format to profile CBOR.
pub(super) const HEADER: &[u8; 12] = b"OINKDIAG\x01\x01\x00\x00";
pub(crate) fn encode(segment: &Segment) -> anyhow::Result<Vec<u8>> {
    encode_raw(&serde_json::to_vec(segment)?)
}
pub(super) fn encode_raw(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(raw.len() <= MAX_SEGMENT_RAW, "diagnostic segment too large");
    let mut bytes = HEADER.to_vec();
    let mut z = DeflateEncoder::new(&mut bytes, Compression::fast());
    z.write_all(raw)?;
    z.finish()?;
    anyhow::ensure!(
        bytes.len() <= MAX_SEGMENT_RAW,
        "compressed diagnostic segment too large"
    );
    Ok(bytes)
}
pub(crate) fn decode(bytes: &[u8]) -> anyhow::Result<Segment> {
    decode_classified(bytes).map_err(|(_, error)| error)
}
pub(super) fn decode_classified(bytes: &[u8]) -> Result<Segment, (DiscardClass, anyhow::Error)> {
    if !bytes.starts_with(HEADER) {
        return Err((
            DiscardClass::Format,
            anyhow::anyhow!("unsupported local diagnostic format"),
        ));
    }
    if bytes.len() > MAX_SEGMENT_RAW {
        return Err((
            DiscardClass::SizeLimit,
            anyhow::anyhow!("compressed diagnostic segment too large"),
        ));
    }
    let raw = (|| -> anyhow::Result<_> {
        let mut raw = Vec::new();
        let mut decoder = DeflateDecoder::new(&bytes[HEADER.len()..]);
        (&mut decoder).take(MAX_SEGMENT_RAW as u64 + 1).read_to_end(&mut raw)?;
        if raw.len() <= MAX_SEGMENT_RAW {
            anyhow::ensure!(
                decoder.total_in() as usize == bytes.len() - HEADER.len(),
                "trailing compressed data"
            );
        }
        Ok(raw)
    })()
    .map_err(|error| (DiscardClass::Decompress, error))?;
    if raw.len() > MAX_SEGMENT_RAW {
        return Err((DiscardClass::SizeLimit, anyhow::anyhow!("diagnostic expansion limit")));
    }
    let s: Segment = serde_json::from_slice(&raw).map_err(|error| (DiscardClass::Decode, error.into()))?;
    (|| -> anyhow::Result<()> {
        anyhow::ensure!(
            s.schema == SCHEMA
                && token_ok(&s.device)
                && token_ok(&s.instance)
                && !s.rows.is_empty()
                && s.rows.len() <= 1024
                && s.rows.iter().all(|r| r.clock_id == s.rows[0].clock_id),
            "invalid diagnostic schema"
        );
        s.snapshot.validate()?;
        Ok(())
    })()
    .map_err(|error| (DiscardClass::Validation, error))?;
    Ok(s)
}

impl Store {
    #[cfg(all(test, debug_assertions))]
    pub fn open(dir: PathBuf, device: String, enabled: bool) -> anyhow::Result<Self> {
        Self::open_with_clock(dir, device, enabled, clock::Clock::system())
    }
    pub(super) fn open_with_clock(
        dir: PathBuf,
        device: String,
        enabled: bool,
        clock: Arc<clock::Clock>,
    ) -> anyhow::Result<Self> {
        private_dir(&dir)?;
        private_dir(&dir.join("buffer"))?;
        let instance = blake3::hash(format!("{}-{}-{:?}", utc_us(), std::process::id(), Instant::now()).as_bytes())
            .to_hex()[..24]
            .to_owned();
        let mut s = Self {
            dir,
            clock,
            export_source: None,
            export_clock: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            recover_enabled: enabled,
            #[cfg(all(test, debug_assertions))]
            segment_reads: Arc::new(AtomicU64::new(0)),
            device,
            instance,
            previous_instance: None,
            next: 0,
            snapshot: Snapshot::default(),
            gathered_environments: BTreeMap::new(),
            network_received: 0,
            network_expanded: 0,
            network_environments: BTreeMap::new(),
            network_ends: BTreeMap::new(),
            segments: VecDeque::new(),
            clocks: Vec::new(),
            bytes: 0,
            limit: MAX_BYTES,
            evicted: 0,
            corrupt: 0,
            discarded: DiscardCounts::default(),
            #[cfg(all(test, debug_assertions))]
            saved: tokio::sync::watch::channel(0).0,
            missing_members: BTreeMap::new(),
            requested_range: None,
            evicted_by_device: BTreeMap::new(),
        };
        let domain = fs::read_to_string(s.dir.join("domain")).ok();
        if domain.as_deref() == Some(s.device.as_str()) {
            s.previous_instance = fs::read_to_string(s.dir.join("running")).ok().filter(|v| token_ok(v));
        }
        // O(1) startup handoff: do not enumerate, read or decompress any segment
        // here. The old directory remains durable if recovery is interrupted.
        // New recordings use a fresh directory and can be flushed immediately.
        let old_domain = domain.as_deref().filter(|v| token_ok(v)).unwrap_or("discard");
        fs::rename(
            s.dir.join("buffer"),
            s.dir.join(format!("recover-{old_domain}-{}", s.instance)),
        )?;
        private_dir(&s.dir.join("buffer"))?;
        s.write_domain()?;
        s.set_enabled(enabled)?;
        Ok(s)
    }
    pub fn check_cancelled(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.cancelled.load(Ordering::Acquire),
            "diagnostics operation cancelled"
        );
        Ok(())
    }
    fn read_segment(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        self.check_cancelled()?;
        #[cfg(all(test, debug_assertions))]
        self.segment_reads.fetch_add(1, Ordering::Relaxed);
        anyhow::ensure!(fs::metadata(path)?.len() <= MAX_SEGMENT_RAW as u64, "oversize segment");
        Ok(fs::read(path)?)
    }
    fn restore_clocks(&mut self, segment: &Segment) {
        if segment.device != self.device {
            return;
        }
        for sample in segment.clock_samples.iter().take(64) {
            if let Some(old) = self.clocks.iter_mut().find(|c| {
                c.device == sample.device
                    && c.instance == sample.instance
                    && c.source_clock == sample.source_clock
                    && c.local_clock == sample.local_clock
                    && c.local_instance == sample.local_instance
            }) {
                let first = old.first_source_mono_us.min(sample.first_source_mono_us);
                let last = old.last_source_mono_us.max(sample.last_source_mono_us);
                if sample.uncertainty_us < old.uncertainty_us {
                    *old = sample.clone();
                }
                old.first_source_mono_us = first;
                old.last_source_mono_us = last;
            } else {
                self.clocks.push(sample.clone());
            }
            self.clocks.sort_by_key(|c| c.uncertainty_us);
            self.clocks.truncate(64);
        }
    }
    pub fn adopt_recovered(&mut self, segment: &Segment, meta: &Meta, path: &Path) -> anyhow::Result<()> {
        self.check_cancelled()?;
        if self
            .segments
            .iter()
            .any(|m| m.device == meta.device && m.instance == meta.instance && m.seq == meta.seq)
        {
            return Ok(());
        }
        let target = self.dir.join("buffer").join(meta.name());
        // Same immutable segment may have been recovered already; never
        // overwrite it or count it twice. All writers use this Store mutex.
        fs::hard_link(path, target)?;
        self.restore_clocks(segment);
        self.bytes += meta.bytes;
        self.segments.push_back(meta.clone());
        self.prune()
    }
    pub(super) fn record_discard(&mut self, class: DiscardClass) {
        self.corrupt += 1;
        self.discarded.add(class);
    }
    fn load_recovery_segment(&self, path: &Path) -> Result<(Segment, Meta), (DiscardClass, anyhow::Error)> {
        let bytes = self.read_segment(path).map_err(|error| (DiscardClass::Read, error))?;
        let segment = decode_classified(&bytes)?;
        let mut meta = Self::meta(&segment, bytes.len() as u64);
        meta.received_us = (|| -> std::io::Result<_> {
            Ok(fs::metadata(path)?
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_micros() as i64))
        })()
        .map_err(|error| (DiscardClass::Read, error.into()))?;
        if path.file_name().and_then(|name| name.to_str()) != Some(meta.name().as_str()) {
            return Err((DiscardClass::Validation, anyhow::anyhow!("invalid segment name")));
        }
        Ok((segment, meta))
    }
    /// Background validation, one bounded segment at a time, never under the
    /// live Store mutex. Private hard-link pins keep the pre-crash window intact
    /// while the live recorder prunes its independently indexed buffer.
    pub fn recover(
        &self,
        mut adopt: impl FnMut(&Segment, &Meta, &Path) -> anyhow::Result<()>,
        mut corrupt: impl FnMut(DiscardClass),
    ) -> anyhow::Result<Self> {
        let mut snapshot = self.clone();
        // Anchor the crash window before waiting for peers to reconnect.
        snapshot.export_clock = Some(self.clock.now());
        snapshot.segments.clear();
        snapshot.clocks.clear();
        snapshot.bytes = 0;
        snapshot.corrupt = 0;
        snapshot.discarded = DiscardCounts::default();
        for entry in fs::read_dir(&self.dir)? {
            self.check_cancelled()?;
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if (name.starts_with("export-") || name.starts_with("discard-")) && entry.file_type()?.is_dir() {
                self.remove_private_dir(&entry.path())?;
            } else if name.ends_with(".partial") && entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
            }
        }
        snapshot.export_source = Some(Arc::new(
            tempfile::Builder::new().prefix("export-").tempdir_in(&self.dir)?,
        ));
        for entry in fs::read_dir(&self.dir)? {
            self.check_cancelled()?;
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("recover-") || !entry.file_type()?.is_dir() {
                continue;
            }
            if !self.recover_enabled || !name.starts_with(&format!("recover-{}-", self.device)) {
                self.remove_private_dir(&entry.path())?;
                continue;
            }
            for file in fs::read_dir(entry.path())? {
                self.check_cancelled()?;
                let file = file?;
                if file.path().extension().is_none_or(|ext| ext != "dseg") {
                    continue;
                }
                let loaded = self.load_recovery_segment(&file.path());
                self.check_cancelled()?;
                match loaded {
                    Ok((segment, meta)) => {
                        // Unreleased continuous-replication copies are simply discarded.
                        if segment.device != self.device {
                            fs::remove_file(file.path())?;
                            continue;
                        }
                        if snapshot
                            .segments
                            .iter()
                            .any(|m| m.device == meta.device && m.instance == meta.instance && m.seq == meta.seq)
                        {
                            continue;
                        }
                        fs::hard_link(
                            file.path(),
                            snapshot.export_source.as_ref().unwrap().path().join(meta.name()),
                        )?;
                        adopt(&segment, &meta, &file.path())?;
                        snapshot.restore_clocks(&segment);
                        snapshot.bytes += meta.bytes;
                        snapshot.segments.push_back(meta);
                    }
                    Err((class, _)) => {
                        snapshot.record_discard(class);
                        corrupt(class);
                        fs::remove_file(file.path())?;
                    }
                }
            }
        }
        snapshot.discarded.report();
        snapshot.segments.make_contiguous().sort_by_key(|m| m.last_us);
        // Interrupted starts can leave several durable staging directories. Their
        // duplicated segments are deduplicated above; unique pre-start records
        // still obey the retention cap in the crash snapshot.
        while snapshot.bytes > snapshot.limit {
            self.check_cancelled()?;
            let meta = snapshot.segments.pop_front().unwrap();
            snapshot.bytes -= meta.bytes;
            snapshot.evicted += 1;
            fs::remove_file(snapshot.export_source.as_ref().unwrap().path().join(meta.name()))?;
        }
        Ok(snapshot)
    }
    fn remove_private_dir(&self, path: &Path) -> anyhow::Result<()> {
        for entry in fs::read_dir(path)? {
            self.check_cancelled()?;
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                self.remove_private_dir(&entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        self.check_cancelled()?;
        fs::remove_dir(path)?;
        Ok(())
    }
    pub fn finish_recovery(&self) -> anyhow::Result<()> {
        for entry in fs::read_dir(&self.dir)? {
            self.check_cancelled()?;
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("recover-") && entry.file_type()?.is_dir() {
                self.remove_private_dir(&entry.path())?;
            }
        }
        Ok(())
    }
    #[cfg(all(test, debug_assertions))]
    pub fn recover_for_test(&mut self) -> anyhow::Result<()> {
        let source = self.clone();
        let mut discarded = Vec::new();
        let mut snapshot = source.recover(
            |segment, meta, path| self.adopt_recovered(segment, meta, path),
            |class| discarded.push(class),
        )?;
        for class in discarded {
            self.record_discard(class);
        }
        if self.recover_enabled && self.previous_instance.is_some() {
            snapshot.prepare_range(Reason::RecoveredCrash, None)?;
            snapshot.export(Reason::RecoveredCrash, 0, 0)?;
        }
        source.finish_recovery()?;
        Ok(())
    }
    pub fn write_domain(&self) -> anyhow::Result<()> {
        fs::write(self.dir.join("domain"), self.device.as_bytes())?;
        Ok(())
    }
    pub fn set_enabled(&mut self, enabled: bool) -> anyhow::Result<()> {
        if enabled {
            // Windows FlushFileBuffers requires a writable handle; reopening with
            // File::open makes recorder startup fail with ERROR_ACCESS_DENIED.
            // Keep the write handle through the flush on every platform.
            let mut marker = File::create(self.dir.join("running"))?;
            marker.write_all(self.instance.as_bytes())?;
            marker.sync_all()?;
        } else {
            self.abandon_recovery()?;
            self.clean_shutdown()?;
            for m in self.segments.drain(..) {
                let _ = fs::remove_file(self.dir.join("buffer").join(m.name()));
            }
            self.bytes = 0;
            self.clocks.clear();
        }
        Ok(())
    }
    pub fn preserve_recovery_marker(&self) -> anyhow::Result<()> {
        if let Some(previous) = &self.previous_instance {
            atomic(&self.dir.join("running"), previous.as_bytes())?;
        }
        Ok(())
    }
    pub fn abandon_recovery(&self) -> anyhow::Result<()> {
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("recover-") && entry.file_type()?.is_dir() {
                fs::rename(entry.path(), self.dir.join(name.replacen("recover-", "discard-", 1)))?;
            }
        }
        Ok(())
    }
    pub fn clean_shutdown(&self) -> anyhow::Result<()> {
        match fs::remove_file(self.dir.join("running")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn append(&mut self, mut rows: Vec<Row>, dropped: u64) -> anyhow::Result<()> {
        rows.sort_by_key(|r| r.mono_us);
        let mut pending_dropped = dropped;
        // Bounded chunks also bound decompression and export memory.
        for epoch in rows.chunk_by(|a, b| a.clock_id == b.clock_id) {
            let mut offset = 0;
            while offset < epoch.len() {
                let mut length = (epoch.len() - offset).min(1024);
                loop {
                    let s = Segment {
                        schema: SCHEMA,
                        device: self.device.clone(),
                        instance: self.instance.clone(),
                        seq: self.next + 1,
                        dropped: pending_dropped,
                        snapshot: self.snapshot.clone(),
                        clock_samples: self.clocks.clone(),
                        rows: epoch[offset..offset + length].to_vec(),
                    };
                    let raw = serde_json::to_vec(&s)?;
                    if raw.len() > MAX_SEGMENT_RAW && length > 1 {
                        length = length.div_ceil(2);
                        continue;
                    }
                    let bytes = encode_raw(&raw)?;
                    self.next += 1;
                    self.insert(&s, &bytes)?;
                    pending_dropped = 0;
                    offset += length;
                    break;
                }
            }
        }
        Ok(())
    }
    fn meta(s: &Segment, bytes: u64) -> Meta {
        Meta {
            device: s.device.clone(),
            instance: s.instance.clone(),
            seq: s.seq,
            clock_id: s.rows[0].clock_id,
            first_mono_us: s.rows.iter().map(|r| r.mono_us).min().unwrap_or(0),
            last_mono_us: s.rows.iter().map(|r| r.mono_us).max().unwrap_or(0),
            first_us: s.rows.iter().map(|r| r.utc_us).min().unwrap_or(0),
            last_us: s.rows.iter().map(|r| r.utc_us).max().unwrap_or(0),
            bytes,
            received_us: utc_us(),
            network: false,
        }
    }
    pub fn insert(&mut self, s: &Segment, bytes: &[u8]) -> anyhow::Result<()> {
        if self
            .segments
            .iter()
            .any(|m| m.device == s.device && m.instance == s.instance && m.seq == s.seq)
        {
            return Ok(());
        }
        #[cfg(all(test, debug_assertions))]
        if s.device != self.device {
            if self.export_source.is_none() {
                *self = self.export_snapshot()?;
            }
            return self.add_gathered(s, bytes);
        }
        anyhow::ensure!(s.device == self.device, "local recorder accepts only own events");
        let mut meta = Self::meta(s, bytes.len() as u64);
        meta.received_us = self.clock.now().utc_us;
        atomic(
            &self
                .export_source
                .as_ref()
                .map_or_else(|| self.dir.join("buffer"), |dir| dir.path().into())
                .join(meta.name()),
            bytes,
        )?;
        self.bytes += meta.bytes;
        self.segments.push_back(meta);
        self.prune()
    }
    pub fn prune(&mut self) -> anyhow::Result<()> {
        // Live Store contains only locally recorded segments.
        loop {
            self.check_cancelled()?;
            let victim = (self.bytes > self.limit)
                .then(|| {
                    self.segments
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, m)| (m.last_us, m.seq))
                        .map(|(i, _)| i)
                })
                .flatten();
            let Some(i) = victim else { break };
            let Some(m) = self.segments.remove(i) else { break };
            self.bytes = self.bytes.saturating_sub(m.bytes);
            self.evicted += 1;
            fs::remove_file(
                self.export_source
                    .as_ref()
                    .map_or_else(|| self.dir.join("buffer"), |dir| dir.path().into())
                    .join(m.name()),
            )?;
        }
        Ok(())
    }
    pub fn read(&self, m: &Meta) -> anyhow::Result<Vec<u8>> {
        self.check_cancelled()?;
        Ok(fs::read(
            self.export_source
                .as_ref()
                .map_or_else(|| self.dir.join("buffer"), |dir| dir.path().to_path_buf())
                .join(m.name()),
        )?)
    }
    pub fn status(&self) -> RecorderStatus {
        let stamp = self.clock.now();
        let normalized = |m: &Meta, time: i64| time.saturating_sub(self.offset(m, stamp.clock_id).unwrap_or(0));
        RecorderStatus {
            bytes: self.bytes,
            limit_bytes: self.limit,
            segments: self.segments.len(),
            oldest_utc_us: self.segments.iter().map(|m| normalized(m, m.first_us)).min(),
            newest_utc_us: self.segments.iter().map(|m| normalized(m, m.last_us)).max(),
            corrupt_segments: self.corrupt,
            discarded_segments: self.discarded,
            evicted_segments: self.evicted,
            ..Default::default()
        }
    }
    pub fn folder(&self) -> anyhow::Result<String> {
        private_dir(&self.dir)?;
        Ok(self.dir.to_string_lossy().into_owned())
    }
    #[cfg(all(test, debug_assertions))]
    pub fn list(&self) -> anyhow::Result<Vec<BundleInfo>> {
        let mut list = Vec::new();
        for e in fs::read_dir(&self.dir)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(name) = name.strip_suffix(".zip") else {
                continue;
            };
            let Some((time, reason)) = name.split_once('-') else {
                continue;
            };
            let Ok(at_ms) = time.parse() else { continue };
            let reason = match reason.split('-').next().unwrap_or("") {
                "manual" => Reason::Manual,
                "emergency" => Reason::Emergency,
                "recovered_crash" => Reason::RecoveredCrash,
                "peer_restart" => Reason::PeerRestart,
                _ => continue,
            };
            let summary = tests::bundle_summary(&e.path());
            list.push(BundleInfo {
                path: e.path().to_string_lossy().into_owned(),
                at_ms,
                reason,
                bytes: e.metadata()?.len(),
                included_device_count: summary.as_ref().map(|v| v.0),
                missing_device_count: summary.as_ref().map(|v| v.1),
                cutoff_complete: summary.and_then(|v| v.2),
            });
        }
        list.sort_by_key(|b| std::cmp::Reverse(b.at_ms));
        Ok(list)
    }
    /// Pin immutable segments under the store lock, then export this detached copy.
    /// Hard links do not copy 64 MiB or hold thousands of file descriptors. Pruning,
    /// disabling and recording can continue without invalidating the saved cutoff.
    pub fn export_snapshot(&self) -> anyhow::Result<Self> {
        let dir = tempfile::Builder::new().prefix("export-").tempdir_in(&self.dir)?;
        for meta in &self.segments {
            self.check_cancelled()?;
            fs::hard_link(self.dir.join("buffer").join(meta.name()), dir.path().join(meta.name()))?;
        }
        let mut snapshot = self.clone();
        snapshot.export_source = Some(Arc::new(dir));
        snapshot.export_clock = Some(self.clock.now());
        Ok(snapshot)
    }
    pub(super) fn prepare_range(&mut self, reason: Reason, last_minutes: Option<u32>) -> anyhow::Result<()> {
        let end = self.export_clock().utc_us;
        let start = if reason != Reason::Manual {
            end.saturating_sub(AUTOMATIC_RANGE_US)
        } else {
            last_minutes.map_or(i64::MIN, |m| end.saturating_sub(i64::from(m) * 60 * 1_000_000))
        };
        self.requested_range = Some((start, end));
        self.evicted_by_device.insert(self.device.clone(), self.evicted);
        self.restrict_range(start, end)
    }
    pub(super) fn restrict_range(&mut self, start: i64, end: i64) -> anyhow::Result<()> {
        let source = self
            .export_source
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing collection staging"))?;
        let mut kept = VecDeque::new();
        let current_epoch = self.export_clock().clock_id;
        for meta in &self.segments {
            self.check_cancelled()?;
            // An immutable, locally validated segment entirely within the
            // cutoff already has exactly the bytes we need. Keep its hard link.
            // All-history snapshots retain previous clock epochs even after a
            // wall-clock backstep; those immutable rows preceded this cutoff.
            let current_source_epoch = if meta.device == self.device {
                meta.clock_id == current_epoch
            } else {
                self.clocks.iter().any(|c| {
                    c.device == meta.device
                        && c.instance == meta.instance
                        && c.source_clock == meta.clock_id
                        && c.local_clock == current_epoch
                        && c.local_instance == self.instance
                })
            };
            let historical_epoch = start == i64::MIN && !current_source_epoch;
            let Some(offset) = self.offset(meta, current_epoch) else {
                // The source already selected this range. Unknown historical
                // clocks stay explicitly uncalibrated, never assume zero skew.
                kept.push_back(meta.clone());
                continue;
            };
            let start = if start == i64::MIN {
                i64::MIN
            } else {
                start.saturating_add(offset)
            };
            let end = end.saturating_add(offset);
            if historical_epoch || (meta.first_us >= start && meta.last_us <= end) {
                kept.push_back(meta.clone());
                continue;
            }
            if meta.last_us < start || meta.first_us > end {
                fs::remove_file(source.path().join(meta.name()))?;
                continue;
            }
            if meta.network {
                if let Some(meta) = self.restrict_network(meta, start, end)? {
                    kept.push_back(meta);
                }
                continue;
            }
            let mut segment = decode(&self.read(meta)?)?;
            segment.rows.retain(|row| row.utc_us >= start && row.utc_us <= end);
            if segment.rows.is_empty() {
                fs::remove_file(source.path().join(meta.name()))?;
                continue;
            }
            let bytes = encode(&segment)?;
            replace(&source.path().join(meta.name()), &bytes, false)?;
            kept.push_back(Self::meta(&segment, bytes.len() as u64));
        }
        self.segments = kept;
        self.bytes = self.segments.iter().map(|m| m.bytes).sum();
        Ok(())
    }
    /// Reuse immutable compressed bytes; filter and re-encode only a boundary segment.
    #[cfg(all(test, debug_assertions))]
    pub(super) fn collection_segment(&self, meta: &Meta, start: i64, end: i64) -> anyhow::Result<Option<Vec<u8>>> {
        self.check_cancelled()?;
        if meta.last_us < start || meta.first_us > end {
            return Ok(None);
        }
        let bytes = self.read(meta)?;
        if meta.first_us >= start && meta.last_us <= end {
            return Ok(Some(bytes));
        }
        let mut segment = decode(&bytes)?;
        segment.rows.retain(|row| row.utc_us >= start && row.utc_us <= end);
        if segment.rows.is_empty() {
            return Ok(None);
        }
        encode(&segment).map(Some)
    }
    #[cfg(all(test, debug_assertions))]
    pub(super) fn add_gathered(&mut self, segment: &Segment, bytes: &[u8]) -> anyhow::Result<()> {
        self.check_cancelled()?;
        anyhow::ensure!(segment.device != self.device, "unexpected local origin");
        anyhow::ensure!(
            !self
                .segments
                .iter()
                .any(|m| m.device == segment.device && m.instance == segment.instance && m.seq == segment.seq),
            "duplicate segment"
        );
        let meta = Self::meta(segment, bytes.len() as u64);
        let directory = self
            .export_source
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing collection staging"))?;
        replace(&directory.path().join(meta.name()), bytes, false)?;
        self.bytes += meta.bytes;
        self.segments.push_back(meta);
        Ok(())
    }
    pub(super) fn current_stamp(&self) -> clock::Stamp {
        self.clock.now()
    }
    pub(super) fn export_clock(&self) -> clock::Stamp {
        self.export_clock.unwrap_or_else(|| self.clock.now())
    }
    // A selected historical range may belong to a previous peer instance/build.
    // Preserve its environment identity, but layout is an explicit export-time
    // observation with its own producer metadata, independent of that history.
    fn layout_view<'a>(
        &'a self,
        device: &str,
        historical: &'a Snapshot,
    ) -> (&'a Snapshot, Option<&'a str>, LayoutCaptureKind) {
        if device == self.device && self.snapshot.layout.is_some() {
            (&self.snapshot, Some(&self.instance), LayoutCaptureKind::Current)
        } else if let Some((instance, live)) = self.gathered_environments.get(device)
            && live.layout.is_some()
        {
            (
                live,
                token_ok(instance).then_some(instance.as_str()),
                LayoutCaptureKind::Current,
            )
        } else {
            (
                historical,
                None,
                if historical.layout.is_some() {
                    LayoutCaptureKind::Retained
                } else {
                    LayoutCaptureKind::Unavailable
                },
            )
        }
    }
    pub fn export(&self, reason: Reason, dropped: u64, errors: u64) -> anyhow::Result<BundleInfo> {
        self.export_sync(reason, dropped, errors, None)
    }
    pub fn export_sync(
        &self,
        reason: Reason,
        dropped: u64,
        errors: u64,
        cutoff_complete: Option<bool>,
    ) -> anyhow::Result<BundleInfo> {
        let mut trace = collection::CollectionTrace::new("export");
        self.check_cancelled()?;
        let export_clock = self.export_clock.unwrap_or_else(|| self.clock.now());
        let at_ms = export_clock.utc_us / 1000;
        let pending = tempfile::Builder::new()
            .prefix(&format!("{at_ms}-{}-{}-", reason.label(), self.next))
            .suffix(".partial")
            .tempfile_in(&self.dir)?;
        let (file, tmp) = pending.into_parts();
        let path = tmp.with_extension("zip");
        let mut zip = zip::ZipWriter::new(BufWriter::with_capacity(EXPORT_BUFFER, file));
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(1))
            .large_file(true)
            .unix_permissions(0o600);
        let mut sources: BTreeMap<String, Vec<Meta>> = BTreeMap::new();
        sources.entry(self.device.clone()).or_default();
        let mut missing_members = self.missing_members.clone();
        // Startup recovery precedes link attachment. Retained anonymous membership
        // still identifies devices we could not ask; it never reads the state file.
        if cutoff_complete.is_none()
            && let Some(excerpt) = self.snapshot.group_state.as_ref()
            && let Ok(excerpt) = serde_json::from_str::<serde_json::Value>(excerpt)
            && let Some(members) = excerpt["members"]["members"].as_array()
        {
            for member in members {
                if let Some(device) = member["device"].as_str()
                    && device != self.device
                    && !self.segments.iter().any(|m| m.device == device)
                {
                    missing_members
                        .entry(device.into())
                        .or_insert_with(|| "link_not_ready".into());
                }
            }
        }
        for device in missing_members.keys() {
            sources.entry(device.clone()).or_default();
        }
        for m in &self.segments {
            sources.entry(m.device.clone()).or_default().push(m.clone());
        }
        let included_device_count = sources.values().filter(|metas| !metas.is_empty()).count();
        let missing_device_count = sources.len() - included_device_count;
        let aliases: BTreeMap<String, String> = sources
            .keys()
            .enumerate()
            .map(|(i, k)| (k.clone(), format!("device-{}", letter(i))))
            .collect();
        let mut spool = TimelineSpool::new(self, export_clock.clock_id)?;
        trace.stage("serialized");
        let mut snapshots = BTreeMap::new();
        let mut versions = BTreeMap::new();
        let mut screens = BTreeMap::new();
        for device in sources.keys() {
            let snapshot = if device == &self.device && !self.snapshot.version.is_empty() {
                self.snapshot.clone()
            } else {
                spool
                    .snapshots
                    .get(device)
                    .map(|(instance, historical)| {
                        self.gathered_environments
                            .get(device)
                            .filter(|(live_instance, live)| {
                                live_instance == instance && live.source_tree_sha256 == historical.source_tree_sha256
                            })
                            .map_or_else(|| historical.clone(), |(_, live)| live.clone())
                    })
                    .unwrap_or_default()
            };
            snapshots.insert(device.clone(), snapshot.clone());
            versions.insert(
                device.clone(),
                serde_json::json!({"version": snapshot.version, "source_tree_sha256": snapshot.source_tree_sha256}),
            );
            let (layout_source, _, _) = self.layout_view(device, &snapshot);
            let mut own = layout_source
                .layout
                .iter()
                .chain(snapshot.layout.iter())
                .flat_map(|layout| {
                    layout
                        .saved
                        .iter()
                        .filter(|s| &s.device == device)
                        .map(|s| s.screen.clone())
                        .chain(
                            layout
                                .devices
                                .iter()
                                .filter(|d| &d.device == device)
                                .flat_map(|d| d.displays.iter().map(|s| s.screen.clone())),
                        )
                })
                .collect::<Vec<_>>();
            own.sort();
            own.dedup();
            for (i, screen) in own.into_iter().enumerate() {
                screens.insert(
                    screen,
                    format!("{}{}", aliases[device].trim_start_matches("device-"), i + 1),
                );
            }
        }
        for (device, (_, remote)) in self.network_environments.iter().chain(spool.wire_snapshots.iter()) {
            versions.insert(
                device.clone(),
                serde_json::json!({"version":remote.version()?.0,"source_tree_sha256":remote.source_tree_sha256()?.0}),
            );
        }
        let manifest = serde_json::json!({"schema":SCHEMA,"export_clock":{"device":self.device,"instance":self.instance,"clock_id":export_clock.clock_id,"utc_us":export_clock.utc_us},"cutoff_complete":cutoff_complete,"reason":reason,"at_ms":at_ms,"version":env!("OINK_RELEASE_VERSION"),"source_tree_sha256":env!("OINK_DIAGNOSTIC_SOURCE_TREE_SHA256"),"devices":aliases,"screens":screens,"versions":versions,"unrecognized_diagnostic_events":spool.unrecognized,"unrecognized_snapshot_fields":({let mut counts=spool.unknown_snapshot_fields.clone();for (_,snapshot) in self.network_environments.values(){for key in wire::unknown_snapshot_fields(snapshot){*counts.entry(key).or_insert(0u64)+=1;}}counts}),"normalized_peer_coverage":self.normalized_peer_coverage(export_clock.clock_id)?,"peer_ends":self.network_ends.iter().map(|(d,e)| (d.clone(),wire::json(&oink_protocol::v1::schema::WireValue::write(e)))).collect::<BTreeMap<_,_>>(),"coverage":self.segments,"dropped":dropped,"errors":errors,"evicted_segments":self.evicted,"corrupt_segments":self.corrupt,"missing_members":missing_members,"requested_range_us":self.requested_range.map(|(start,end)| (if start == i64::MIN {None} else {Some(start)},end)),"evicted_segments_by_device":({ let mut evicted = self.evicted_by_device.clone(); evicted.insert(self.device.clone(),self.evicted); evicted }),"missing":["Crash may lose the last flush interval and queued events.","Offline, unsupported, disabled, timed out or failed members contribute only completed segments; inspect missing_members and coverage.","Captured repeat follows router state; injected repeat provenance is unknown (null).","QUIC loss counters are unavailable on relay paths; initial session-lock state can be unknown.","Free-form log text and application identity are intentionally excluded; resolve callsite with source tree provenance.","Crash summaries contain typed exception classes and numeric timestamps; unavailable values are explicit."]});
        zip.start_file("manifest.json", options)?;
        {
            let mut entry = BufWriter::with_capacity(EXPORT_BUFFER, &mut zip);
            serde_json::to_writer_pretty(&mut entry, &manifest)?;
            entry.flush()?;
        }
        zip.start_file("crash-report.json", options)?;
        write_json(&mut zip, &super::crash_report::latest().export_view(at_ms))?;
        zip.start_file("clock-samples.json", options)?;
        write_json(&mut zip, &self.clocks)?;
        for device in sources.keys() {
            let alias = &aliases[device];
            zip.start_file(format!("devices/{alias}/timeline.ndjson"), options)?;
            spool.write_timeline(&mut zip, Some(device))?;
            let snapshot = &snapshots[device];
            if let Some(end) = self.network_ends.get(device) {
                use oink_protocol::v1::schema::WireDto;
                zip.start_file(format!("devices/{alias}/collection-end.cbor"), options)?;
                zip.write_all(&end.encode(oink_protocol::v1::limits::RELIABLE_FRAME)?)?;
            }
            let remote = spool
                .wire_snapshots
                .get(device)
                .or_else(|| self.network_environments.get(device));
            if let Some((instance, remote)) = remote {
                use oink_protocol::v1::schema::{WireDto, WireValue};
                if let Some((_, current)) = self.network_environments.get(device) {
                    zip.start_file(format!("devices/{alias}/current-environment.cbor"), options)?;
                    zip.write_all(&current.encode(MAX_SEGMENT_RAW)?)?;
                }
                zip.start_file(format!("devices/{alias}/environment.cbor"), options)?;
                zip.write_all(&remote.encode(MAX_SEGMENT_RAW)?)?;
                zip.start_file(format!("devices/{alias}/environment.json"), options)?;
                write_json(
                    &mut zip,
                    &serde_json::json!({"instance":instance,"retained":wire::json(&remote.write())}),
                )?;
                continue;
            }
            zip.start_file(format!("devices/{alias}/environment.json"), options)?;
            let mut environment = serde_json::to_value(snapshot)?;
            let (layout_source, layout_instance, capture_kind) = self.layout_view(device, snapshot);
            environment["layout"] = serde_json::to_value(&layout_source.layout)?;
            environment["layout_capture"] = serde_json::to_value(LayoutCapture {
                kind: capture_kind,
                instance: layout_instance,
                version: &layout_source.version,
                source_tree_sha256: &layout_source.source_tree_sha256,
            })?;
            environment["crash_report"] = snapshot.crash_report.export_view(at_ms);
            let provider = snapshot.file_provider.or_else(|| {
                snapshot
                    .routing_numbers
                    .get("file_provider_reason")
                    .and_then(|code| super::ProviderAvailability::from_code(*code))
            });
            environment["file_provider"] = serde_json::to_value(provider)?;
            write_json(&mut zip, &environment)?;
        }
        for meta in self.segments.iter().filter(|m| m.network) {
            zip.start_file(
                format!("devices/{}/segments/{}", aliases[&meta.device], meta.name()),
                options,
            )?;
            zip.write_all(&self.read(meta)?)?;
        }
        trace.stage("device_timelines");
        zip.start_file("merged.ndjson", options)?;
        spool.write_timeline(&mut zip, None)?;
        trace.stage("merged");
        self.check_cancelled()?;
        let mut file = zip.finish()?;
        file.flush()?;
        file.get_ref().sync_all()?;
        trace.stage("synced");
        drop(file);
        self.check_cancelled()?;
        fs::rename(&tmp, &path)?;
        let bundle = BundleInfo {
            path: path.to_string_lossy().into_owned(),
            at_ms,
            reason,
            bytes: fs::metadata(&path)?.len(),
            included_device_count: Some(included_device_count),
            missing_device_count: Some(missing_device_count),
            cutoff_complete,
        };
        self.retain_bundles(&bundle)?;
        #[cfg(all(test, debug_assertions))]
        self.saved
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        trace.finish(bundle.bytes, true);
        Ok(bundle)
    }
    pub(super) fn retain_bundles(&self, newest: &BundleInfo) -> anyhow::Result<()> {
        // Only automatic crash reports participate. Manual and emergency files
        // are never candidates, and retention never opens their zip/manifest.
        if !matches!(newest.reason, Reason::RecoveredCrash | Reason::PeerRestart) {
            return Ok(());
        }
        let mut crashes = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.to_string_lossy() == newest.path || !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some((time, reason)) = name
                .to_str()
                .and_then(|n| n.strip_suffix(".zip"))
                .and_then(|n| n.split_once('-'))
            else {
                continue;
            };
            if !matches!(reason.split('-').next(), Some("recovered_crash" | "peer_restart")) {
                continue;
            }
            let Ok(time) = time.parse::<i64>() else { continue };
            crashes.push((time, path));
        }
        crashes.sort_by_key(|(time, _)| std::cmp::Reverse(*time));
        for (_, path) in crashes.into_iter().skip(MAX_CRASH_PACKAGES - 1) {
            fs::remove_file(path)?;
        }
        Ok(())
    }
    // An old receiver epoch is valid only between its observed source timestamps.
    // Only the current receiver epoch can extend beyond its last sample. A segment
    // crossing an unobserved transition is uncalibrated in its entirety; this keeps
    // coverage bounds, retention and both exported timelines consistent without
    // decoding the complete ring on each status/prune operation.
    pub(super) fn offset(&self, meta: &Meta, local_clock: i64) -> Option<i64> {
        if meta.device == self.device {
            Some(0)
        } else {
            self.offset_sample(meta, local_clock).map(|c| c.offset_us)
        }
    }
    fn offset_sample(&self, meta: &Meta, local_clock: i64) -> Option<&ClockSample> {
        self.clocks
            .iter()
            .filter(|c| {
                c.device == meta.device
                    && c.instance == meta.instance
                    && c.source_clock == meta.clock_id
                    && meta.first_mono_us >= c.first_source_mono_us
                    && (meta.last_mono_us <= c.last_source_mono_us
                        || (c.local_clock == local_clock && c.local_instance == self.instance))
            })
            .min_by_key(|c| c.uncertainty_us)
    }
}
// Buffer before the compressor too: serde emits many tiny writes per object.
fn write_json(out: &mut impl Write, value: &impl Serialize) -> anyhow::Result<()> {
    let mut out = BufWriter::with_capacity(EXPORT_BUFFER, out);
    serde_json::to_writer(&mut out, value)?;
    out.flush()?;
    Ok(())
}
#[derive(Serialize)]
struct ExportRow<'a> {
    schema: u32,
    device: &'a str,
    instance: &'a str,
    segment: u64,
    index: usize,
    mono_us: u64,
    clock_id: i64,
    original_utc_us: i64,
    unified_utc_us: i64,
    clock_calibrated: bool,
    clock_offset_us: Option<i64>,
    clock_uncertainty_us: Option<u64>,
    earliest_unified_utc_us: Option<i64>,
    latest_unified_utc_us: Option<i64>,
    event: serde_json::Value,
}
struct Run {
    meta: Meta,
    start: u64,
    end: u64,
}
struct TimelineSpool {
    file: BufReader<File>,
    runs: Vec<Run>,
    snapshots: BTreeMap<String, (String, Snapshot)>,
    wire_snapshots: BTreeMap<String, (String, oink_protocol::v1::dto::Snapshot)>,
    unrecognized: BTreeMap<String, u64>,
    unknown_snapshot_fields: BTreeMap<u32, u64>,
    position: u64,
    cancelled: Arc<AtomicBool>,
}
impl TimelineSpool {
    // Decode and serialize each segment once for both timelines. Binary headers
    // carry merge keys and line lengths, so merging never parses JSON or rereads
    // an output line. RAM is one segment, two I/O buffers and O(segments) cursors.
    fn new(store: &Store, local_clock: i64) -> anyhow::Result<Self> {
        let mut spool = BufWriter::with_capacity(EXPORT_BUFFER, tempfile::tempfile_in(&store.dir)?);
        let mut position = 0;
        let mut runs = Vec::new();
        let mut line = Vec::new();
        let mut snapshots = BTreeMap::new();
        let mut wire_snapshots = BTreeMap::new();
        let mut unrecognized = BTreeMap::new();
        let mut unknown_snapshot_fields = BTreeMap::new();
        for meta in &store.segments {
            store.check_cancelled()?;
            let rows: Vec<(usize, u64, i64, i64, serde_json::Value)> = if meta.network {
                let segment = wire::segment(&store.read(meta)?)?;
                let mut current = Vec::new();
                use oink_protocol::v1::schema::WireValue;
                for (index, row) in segment.rows()?.0.into_iter().enumerate() {
                    let event = row.event()?;
                    let kind = event.kind()?.0;
                    let recognized = oink_protocol::v1::dto::diagnostic_event_schema(&kind).is_some();
                    if !recognized {
                        *unrecognized.entry(kind.clone()).or_insert(0u64) += 1;
                    }
                    current.push((
                        index,
                        row.mono_us()?,
                        row.clock_id()?,
                        row.utc_us()?,
                        serde_json::json!({"kind":kind,"recognized":recognized,"retained":wire::json(&event.write())}),
                    ));
                }
                let snapshot = segment.snapshot()?;
                for key in wire::unknown_snapshot_fields(&snapshot) {
                    *unknown_snapshot_fields.entry(key).or_insert(0u64) += 1;
                }
                wire_snapshots.insert(meta.device.clone(), (segment.instance()?.0, snapshot));
                current
            } else {
                let segment = decode(&store.read(meta)?)?;
                snapshots.insert(
                    meta.device.clone(),
                    (segment.instance.clone(), segment.snapshot.clone()),
                );
                segment
                    .rows
                    .iter()
                    .enumerate()
                    .map(|(i, r)| Ok((i, r.mono_us, r.clock_id, r.utc_us, serde_json::to_value(&r.event)?)))
                    .collect::<anyhow::Result<_>>()?
            };
            let offset = store.offset(meta, local_clock);
            let uncertainty = if meta.device == store.device {
                Some(0)
            } else {
                store.offset_sample(meta, local_clock).map(|c| c.uncertainty_us)
            };
            let mut rows = rows;
            rows.sort_by_key(|(index, _, _, utc, _)| (utc.saturating_sub(offset.unwrap_or(0)), *index));
            let start = position;
            for (index, mono_us, clock_id, utc_us, event) in rows {
                store.check_cancelled()?;
                let time = utc_us.saturating_sub(offset.unwrap_or(0));
                line.clear();
                serde_json::to_writer(
                    &mut line,
                    &ExportRow {
                        schema: SCHEMA,
                        device: &meta.device,
                        instance: &meta.instance,
                        segment: meta.seq,
                        index,
                        mono_us,
                        clock_id,
                        original_utc_us: utc_us,
                        unified_utc_us: time,
                        clock_calibrated: offset.is_some(),
                        clock_offset_us: offset,
                        clock_uncertainty_us: uncertainty,
                        earliest_unified_utc_us: uncertainty.map(|u| time.saturating_sub(u as i64)),
                        latest_unified_utc_us: uncertainty.map(|u| time.saturating_add(u as i64)),
                        event,
                    },
                )?;
                line.push(b'\n');
                spool.write_all(&time.to_le_bytes())?;
                spool.write_all(&(index as u32).to_le_bytes())?;
                spool.write_all(&(line.len() as u32).to_le_bytes())?;
                spool.write_all(&line)?;
                position += 16 + line.len() as u64;
            }
            runs.push(Run {
                meta: meta.clone(),
                start,
                end: position,
            });
        }
        spool.flush()?;
        let file = spool.into_inner().map_err(|e| e.into_error())?;
        let mut file = file;
        file.rewind()?;
        Ok(Self {
            // Overlapping runs switch this one reader frequently. Large read-ahead
            // would reread hundreds of KiB for each short row after every seek.
            file: BufReader::with_capacity(8 * 1024, file),
            runs,
            snapshots,
            wire_snapshots,
            unrecognized,
            unknown_snapshot_fields,
            position: 0,
            cancelled: store.cancelled.clone(),
        })
    }
    fn header(file: &mut BufReader<File>, cursor: &mut u64, position: u64) -> anyhow::Result<(i64, u32, u32)> {
        // seek_relative preserves the buffer for contiguous rows in a sorted run.
        file.seek_relative(position as i64 - *cursor as i64)?;
        *cursor = position + 16;
        let mut header = [0; 16];
        file.read_exact(&mut header)?;
        Ok((
            i64::from_le_bytes(header[..8].try_into()?),
            u32::from_le_bytes(header[8..12].try_into()?),
            u32::from_le_bytes(header[12..].try_into()?),
        ))
    }
    fn write_timeline(&mut self, out: &mut impl Write, device: Option<&str>) -> anyhow::Result<()> {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let mut out = BufWriter::with_capacity(EXPORT_BUFFER, out);
        let mut heap = BinaryHeap::new();
        for (id, run) in self
            .runs
            .iter()
            .enumerate()
            .filter(|(_, r)| device.is_none_or(|d| r.meta.device == d))
        {
            let (time, index, len) = Self::header(&mut self.file, &mut self.position, run.start)?;
            heap.push(Reverse((
                time,
                run.meta.device.as_str(),
                run.meta.instance.as_str(),
                run.meta.seq,
                index,
                id,
                run.start + 16,
                len,
            )));
        }
        let mut line = Vec::new();
        while let Some(Reverse((_, _, _, _, _, id, position, len))) = heap.pop() {
            anyhow::ensure!(
                !self.cancelled.load(Ordering::Acquire),
                "diagnostics operation cancelled"
            );
            self.file.seek_relative(position as i64 - self.position as i64)?;
            self.position = position + len as u64;
            line.resize(len as usize, 0);
            self.file.read_exact(&mut line)?;
            out.write_all(&line)?;
            let next = position + len as u64;
            let run = &self.runs[id];
            if next < run.end {
                let (time, index, len) = Self::header(&mut self.file, &mut self.position, next)?;
                heap.push(Reverse((
                    time,
                    run.meta.device.as_str(),
                    run.meta.instance.as_str(),
                    run.meta.seq,
                    index,
                    id,
                    next + 16,
                    len,
                )));
            }
        }
        out.flush()?;
        Ok(())
    }
}

fn letter(mut index: usize) -> String {
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + (index % 26) as u8) as char);
        if index < 26 {
            return s;
        }
        index = index / 26 - 1;
    }
}

#[cfg(all(test, debug_assertions))]
mod discard_tests {
    use super::*;

    #[test]
    fn recovery_discard_read_failure_is_classified_before_decode() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
        assert!(matches!(
            store.load_recovery_segment(&dir.path().join("missing.dseg")),
            Err((DiscardClass::Read, _))
        ));
    }
}
