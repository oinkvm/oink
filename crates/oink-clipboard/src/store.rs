//! Owned content batches. Cleanup and acquisition share cache's lifetime lock.
//! Live native promise writers pin their batch. Startup revokes orphaned exports
//! because their previous receiver can no longer publish a delivery.
use crate::cache::{self, CacheCategory, CacheSize, SourceLease};
use crate::model::{REPRESENTATION_BYTES, SNAPSHOT_BYTES};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
    sync::{Arc, Mutex},
};

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

const RECORD: &str = ".oink-content-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoreKind {
    SourceSnapshot,
    PromiseExport,
}
impl StoreKind {
    pub const ALL: [Self; 2] = [Self::SourceSnapshot, Self::PromiseExport];
    pub fn directory(self) -> &'static str {
        match self {
            Self::SourceSnapshot => "source-snapshots",
            Self::PromiseExport => "promise-exports",
        }
    }
    pub fn category(self) -> CacheCategory {
        match self {
            Self::SourceSnapshot => CacheCategory::SourceSnapshots,
            Self::PromiseExport => CacheCategory::PromiseExports,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum State {
    Receiving,
    Complete,
    Abandoned,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    kind: StoreKind,
    state: State,
    external_writers: bool,
    snapshot_bytes: u64,
    completed_at: Option<u64>,
}

/// Local execution records accept only the schema implemented by this build.
#[cfg(target_os = "macos")]
pub(crate) fn schema_one<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<u8, D::Error> {
    let schema = u8::deserialize(decoder)?;
    if schema != 1 {
        return Err(serde::de::Error::custom("unsupported local schema"));
    }
    Ok(schema)
}

/// Numeric batch identifiers are allocated by the owning operation. No user
/// paths can enter the managed namespace through this API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchId(pub [u8; 16]);
impl BatchId {
    pub fn random() -> io::Result<Self> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(|_| io::Error::other("content identity unavailable"))?;
        Ok(Self(bytes))
    }
    pub(crate) fn name(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[derive(Clone)]
pub struct Batch(Arc<BatchInner>);
struct BatchInner {
    id: BatchId,
    lease: Option<SourceLease>,
    record: Mutex<Record>,
    snapshots: Mutex<()>,
}
impl Drop for BatchInner {
    fn drop(&mut self) {
        let record = self.record.get_mut().unwrap_or_else(|e| e.into_inner());
        let disposable = disposable(record);
        let kind = record.kind;
        let path = self.lease.as_ref().unwrap().path().to_owned();
        // Release our own pin before queueing. A native reader owns another
        // Batch clone, so last-drop cannot occur while that reader is active.
        drop(self.lease.take());
        if disposable {
            cleanup().send(Cleanup::Remove(path, kind)).ok();
        }
    }
}
fn disposable(record: &Record) -> bool {
    !record.external_writers
}
enum Cleanup {
    Remove(std::path::PathBuf, StoreKind),
    #[cfg(all(test, debug_assertions))]
    Barrier(std::sync::mpsc::Sender<()>),
}
fn cleanup() -> &'static std::sync::mpsc::Sender<Cleanup> {
    static WORKER: std::sync::OnceLock<std::sync::mpsc::Sender<Cleanup>> = std::sync::OnceLock::new();
    WORKER.get_or_init(|| {
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("oink-content-cleanup".into())
            .spawn(move || {
                while let Ok(job) = receive.recv() {
                    match job {
                        Cleanup::Remove(path, kind) => {
                            let sources = cache::source_guard();
                            if !sources.keys().any(|p| p.starts_with(&path) || path.starts_with(p))
                                && load(&path, kind).is_ok_and(|r| disposable(&r))
                            {
                                // Refuse links anywhere in an owned tree. A busy
                                // Windows handle leaves its ownership marker for a
                                // later clear/startup instead of deleting around it.
                                let _ = cache::tree_bytes(&path).and_then(|_| remove_owned(&path));
                            }
                            drop(sources);
                            if let Some(root) = path.parent() {
                                cache::changed(root);
                            }
                        }
                        #[cfg(all(test, debug_assertions))]
                        Cleanup::Barrier(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })
            .expect("content cleanup worker");
        send
    })
}

/// Run off the input/native event thread before recovering publications. A
/// previous process has no native receiver left to accept an export. Its private
/// incoming/ready paths are disposable even if a source app still holds a writer
/// handle: no surviving consumer can receive that generation. Current receivers
/// hold lifetime pins and are excluded below. Nothing in a user directory is
/// traversed or removed. Unpinned source snapshots are disposable as well.
pub fn recover(data: &Path) {
    // Receiver cut-over runs under the provider admission gate. This recovery
    // owns source batches only and must not bypass that transaction.
    let sources = cache::source_guard();
    for kind in [StoreKind::SourceSnapshot, StoreKind::PromiseExport] {
        let root = data.join(kind.directory());
        let Ok(entries) = directory(&root).and_then(|_| fs::read_dir(&root)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if sources.keys().any(|p| p.starts_with(&path) || path.starts_with(p)) {
                continue;
            }
            if let Ok(record) = load(&path, kind)
                && (kind == StoreKind::PromiseExport || disposable(&record))
            {
                let _ = cache::tree_bytes(&path).and_then(|_| remove_owned(&path));
            }
        }
    }
}

fn remove_owned(path: &Path) -> io::Result<()> {
    let _mutation = cache::Mutation(path.parent().unwrap());
    for child in fs::read_dir(path)? {
        let child = child?;
        if child.file_name() == RECORD {
            continue;
        }
        if child.file_type()?.is_dir() {
            fs::remove_dir_all(child.path())?;
        } else {
            fs::remove_file(child.path())?;
        }
    }
    fs::remove_file(path.join(RECORD))?;
    fs::remove_dir(path)
}
impl std::fmt::Debug for Batch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContentBatch")
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid owned content batch")
}
fn create_directory(path: &Path) -> io::Result<()> {
    #[allow(unused_mut)] // Windows has no Unix permission bits.
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}
fn create_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn directory(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if cache::is_link(&meta) || !meta.is_dir() {
        return Err(invalid());
    }
    Ok(())
}
fn load(path: &Path, kind: StoreKind) -> io::Result<Record> {
    directory(path)?;
    let file = path.join(RECORD);
    let meta = fs::symlink_metadata(&file)?;
    if cache::is_link(&meta) || !meta.is_file() || meta.len() > 4096 {
        return Err(invalid());
    }
    let record: Record = serde_json::from_slice(&fs::read(file)?).map_err(|_| invalid())?;
    if record.version != 1 || record.kind != kind || (record.state == State::Complete && record.completed_at.is_none())
    {
        return Err(invalid());
    }
    Ok(record)
}

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()
    }
    // Windows replacement below flushes the retained file handle after rename;
    // an ordinary File::open cannot open a directory there.
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}
pub(crate) fn replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        // Cache accounting may still be reading the previous ownership record.
        // MoveFileExW rejects replacement of that open destination even with
        // FILE_SHARE_DELETE. Rust's rename supports POSIX replacement semantics
        // (available at our MSRV), so old readers keep their previous record.
        // Retain a writable handle across the rename and flush its data/metadata
        // before any durable receipt can be published.
        let file = fs::OpenOptions::new().write(true).open(from)?;
        fs::rename(from, to)?;
        file.sync_all()
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
}
fn save(path: &Path, record: &Record) -> io::Result<()> {
    let _mutation = cache::Mutation(path.parent().unwrap());
    let temporary = path.join(".oink-content-next");
    let mut file = create_file(&temporary)?;
    let result = (|| {
        file.write_all(&serde_json::to_vec(record).map_err(io::Error::other)?)?;
        file.sync_all()?;
        drop(file);
        replace(&temporary, &path.join(RECORD))?;
        sync_directory(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

impl Batch {
    /// Caller holds this batch's pin throughout writing. Names are generated
    /// from numeric node IDs, never from a peer-provided relative path.
    #[cfg(windows)]
    pub(crate) fn commit_file(&self, node: crate::model::NodeId) -> io::Result<()> {
        let _mutation = cache::Mutation(self.path().parent().unwrap());
        let record = self.0.record.lock().unwrap();
        if record.state != State::Receiving {
            return Err(invalid());
        }
        replace(
            &self.path().join(format!("f-{}.part", node.0)),
            &self.path().join(format!("f-{}", node.0)),
        )?;
        sync_directory(self.path())?;
        cache::changed(self.path().parent().unwrap());
        Ok(())
    }
    /// Create and pin before publishing any path to a reader or external writer.
    pub fn create(data: &Path, kind: StoreKind, id: BatchId) -> io::Result<Self> {
        directory(data)?;
        let root = data.join(kind.directory());
        match create_directory(&root) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        directory(&root)?;
        let path = root.join(id.name());
        let record = Record {
            version: 1,
            kind,
            state: State::Receiving,
            external_writers: false,
            snapshot_bytes: 0,
            completed_at: None,
        };
        let lease = SourceLease::create_owner(|| {
            create_directory(&path)?;
            if let Err(error) = save(&path, &record) {
                // This call just created the empty directory; no external writer
                // or consumer can have received it before create returns.
                let _ = fs::remove_dir(&path);
                return Err(error);
            }
            sync_directory(&root)?;
            Ok(path)
        })?;
        cache::changed(&root);
        Ok(Self(Arc::new(BatchInner {
            id,
            lease: Some(lease),
            record: Mutex::new(record),
            snapshots: Mutex::new(()),
        })))
    }
    // Consumption markers do not acquire a new payload capability. A reader
    // admitted before expiry may finish and mark EOF while holding its lease.
    pub fn path(&self) -> &Path {
        self.0.lease.as_ref().unwrap().path()
    }
    pub fn id(&self) -> BatchId {
        self.0.id
    }
    fn update(&self, change: impl FnOnce(&mut Record) -> io::Result<()>) -> io::Result<()> {
        let mut record = self.0.record.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = record.clone();
        change(&mut next)?;
        save(self.path(), &next)?;
        *record = next;
        cache::changed(self.path().parent().unwrap());
        Ok(())
    }
    /// Mark immediately before handing a destination to receivePromisedFiles.
    /// Live receipts clear this after every read-ready callback has returned;
    /// startup separately revokes exports whose receiver no longer exists.
    pub fn external_writers_started(&self) -> io::Result<()> {
        self.update(|r| {
            if r.kind != StoreKind::PromiseExport || r.state != State::Receiving {
                return Err(invalid());
            }
            r.external_writers = true;
            Ok(())
        })
    }
    pub fn external_writers_finished(&self) -> io::Result<()> {
        self.update(|r| {
            r.external_writers = false;
            Ok(())
        })
    }
    pub fn abandon(&self) -> io::Result<()> {
        self.update(|r| {
            r.state = State::Abandoned;
            Ok(())
        })
    }
    /// The caller has persisted its verified inventory/receipt in the batch before
    /// this commit. No source ACK can be sent before this succeeds.
    pub fn complete(&self) -> io::Result<()> {
        self.update(|r| {
            if r.state != State::Receiving || r.external_writers {
                return Err(invalid());
            }
            r.state = State::Complete;
            r.completed_at = Some(unix_now());
            Ok(())
        })
    }
    /// Snapshot one NON-FILE representation through EOF. The byte after an exact
    /// limit distinguishes valid complete data from oversized data; nothing is
    /// published on failure. File content uses the transfer's separate budget.
    pub fn snapshot(&self, representation: u32, reader: &mut dyn Read) -> io::Result<u64> {
        self.snapshot_with_limits(representation, reader, REPRESENTATION_BYTES, SNAPSHOT_BYTES)
    }
    fn snapshot_with_limits(
        &self,
        representation: u32,
        reader: &mut dyn Read,
        per_rep: u64,
        total: u64,
    ) -> io::Result<u64> {
        // Serialize writers for the aggregate budget, but never hold the batch
        // metadata lock while reading a native/network source. A reader of an
        // already stored representation needs that lock to acquire its lease.
        let _snapshot = self.0.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        let remaining = {
            let record = self.0.record.lock().unwrap_or_else(|e| e.into_inner());
            if record.state != State::Receiving {
                return Err(invalid());
            }
            total
                .checked_sub(record.snapshot_bytes)
                .ok_or_else(invalid)?
                .min(per_rep)
        };
        let part = self.path().join(format!("r-{representation}.part"));
        let destination = self.path().join(format!("r-{representation}"));
        if destination.try_exists()? {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        let _mutation = cache::Mutation(self.path().parent().unwrap());
        let mut file = create_file(&part)?;
        let result = (|| {
            let mut buffer = vec![0; 1024 * 1024];
            let mut bytes = 0_u64;
            loop {
                let length = (remaining - bytes).saturating_add(1).min(buffer.len() as u64) as usize;
                let n = reader.read(&mut buffer[..length])?;
                if n == 0 {
                    break;
                }
                if n as u64 > remaining - bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::FileTooLarge,
                        "representation snapshot budget exceeded",
                    ));
                }
                file.write_all(&buffer[..n])?;
                bytes += n as u64;
            }
            file.sync_all()?;
            drop(file);
            let mut record = self.0.record.lock().unwrap_or_else(|e| e.into_inner());
            // Abandon/complete can now run while the source is blocked. Such a
            // transition must retire this staged payload before publication.
            if record.state != State::Receiving {
                return Err(invalid());
            }
            replace(&part, &destination)?;
            let mut next = record.clone();
            next.snapshot_bytes += bytes;
            save(self.path(), &next)?;
            *record = next;
            cache::changed(self.path().parent().unwrap());
            Ok(bytes)
        })();
        if result.is_err() {
            let _ = fs::remove_file(part);
            let _ = fs::remove_file(destination);
        }
        result
    }
}

/// Returns allocated bytes, retained bytes, actual removal and an incomplete flag.
/// Unknown/malformed ownership and active external writers are never deleted.
pub fn usage(data: &Path, kind: StoreKind, clear: bool) -> (CacheSize, u64, bool) {
    let root = data.join(kind.directory());
    let mut size = CacheSize {
        category: kind.category(),
        bytes: 0,
        kept_bytes: 0,
    };
    let mut removed = 0;
    let mut incomplete = false;
    let sources = cache::source_guard();
    let entries = match directory(&root).and_then(|()| fs::read_dir(&root)) {
        Ok(entries) => entries,
        Err(e) => return (size, 0, e.kind() != io::ErrorKind::NotFound),
    };
    for entry in entries {
        let Ok(entry) = entry else {
            incomplete = true;
            continue;
        };
        let path = entry.path();
        let before = match cache::tree_bytes(&path) {
            Ok(n) => n,
            Err(_) => {
                incomplete = true;
                continue;
            }
        };
        let record = load(&path, kind);
        let protected = sources.keys().any(|p| p.starts_with(&path) || path.starts_with(p));
        let safe = record.as_ref().is_ok_and(|r| !r.external_writers) && !protected;
        if clear && safe {
            // No new pin can be acquired while this lifetime lock is held.
            // Keep our marker until the payload deletion succeeds (Windows can
            // refuse a file still held by a native consumer).
            let deletion = remove_owned(&path);
            incomplete |= deletion.is_err();
        }
        let after = match cache::tree_bytes(&path) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(_) => {
                incomplete = true;
                before
            }
        };
        removed += before.saturating_sub(after);
        size.bytes += after;
        if !safe || clear {
            size.kept_bytes += after;
        }
        incomplete |= record.is_err();
    }
    (size, removed, incomplete)
}

#[cfg(all(test, debug_assertions))]
mod tests;
