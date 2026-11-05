//! Ordinary files in the whole-offer automatic-fetch budget use a local cache.
//! Completed batches expire after 24h only when no clipboard or reader uses them.
use crate::content::{
    self, ReaderGuard,
    namespace::{Namespace, NativeFiles, StoredFile},
};
use crate::model::{ContentSize, FileNode, NodeId, Unavailable};
use crate::{NativeSpec, received};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

const RETENTION_MS: u64 = 24 * 60 * 60 * 1000;
const RECORD: &str = ".oink-cache.json";
static ROOT: OnceLock<PathBuf> = OnceLock::new();
mod cleanup;
pub(crate) use cleanup::cache_usage;
use cleanup::{ClipboardIdentity, Record, maintain};

static CLEANUP: OnceLock<std::sync::mpsc::SyncSender<()>> = OnceLock::new();

/// Coalesce cache mutations and clipboard replacement without a polling clock.
/// The worker separately sleeps until the earliest complete batch's 24h expiry.
pub(crate) fn changed(path: &Path) {
    if ROOT.get().is_some_and(|root| path.starts_with(root)) {
        clipboard_changed();
    }
}
pub(super) fn clipboard_changed() {
    if let Some(sender) = CLEANUP.get() {
        let _ = sender.try_send(());
    }
}
pub(super) fn initialize(cache: &Path) {
    let root = cache.join("ClipboardFiles");
    if ROOT.set(root.clone()).is_ok() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let _ = CLEANUP.set(sender);
        if let Err(error) = std::thread::Builder::new()
            .name("oink-small-file-cleanup".into())
            .spawn(move || {
                loop {
                    let next = match maintain(&root, false, received::now()) {
                        Ok(scan) => scan.next_expiry,
                        Err(error) => {
                            tracing::warn!(reason = ?content::reason(&error), "small file cleanup unavailable");
                            None
                        }
                    };
                    match next {
                        Some(expiry) => {
                            let _ =
                                receiver.recv_timeout(Duration::from_millis(expiry.saturating_sub(received::now())));
                        }
                        None => {
                            if receiver.recv().is_err() {
                                break;
                            }
                        }
                    }
                }
            })
        {
            tracing::warn!(%error, "small file cleanup worker unavailable");
        }
    }
}

/// Metadata only. A directory's root size is not its recursive byte count;
/// inspecting it would add remote enumeration at Offer time. Keep such offers
/// wholly on File Provider, also for links, unknown sizes and mixed selections.
fn eligible(inventory: &crate::model::Inventory) -> bool {
    let roots = &inventory.roots;
    let mut ids = std::collections::HashSet::new();
    !roots.is_empty()
        && roots.iter().all(|node| ids.insert(node.id))
        && inventory.automatic_pull(oink_foundation::clipboard::ClipPlatform::Mac)
}

enum State {
    Idle,
    Running,
    Ready(HashMap<NodeId, PathBuf>),
    Failed(Unavailable),
}

pub(super) struct SmallFiles {
    owner: Weak<SmallFiles>,
    root: PathBuf,
    files: Arc<dyn Namespace>,
    roots: Vec<FileNode>,
    guard: ReaderGuard,
    state: Mutex<State>,
    changed: Condvar,
    cancelled: AtomicBool,
    publication: Mutex<Option<io::Result<ClipboardIdentity>>>,
    await_publication: AtomicBool,
}
impl SmallFiles {
    pub(super) fn for_spec(spec: &NativeSpec, current: ReaderGuard) -> io::Result<Option<Arc<Self>>> {
        if !eligible(&spec.inventory) {
            return Ok(None);
        }
        current()?;
        let Some(files) = spec.file_contents.as_ref().and_then(|files| files.namespace()) else {
            return Ok(None);
        };
        if files.roots() != spec.inventory.roots {
            return Err(content::unavailable(Unavailable::Superseded));
        }
        let root = ROOT
            .get()
            .cloned()
            .ok_or_else(|| content::unavailable(Unavailable::ReadFailed))?;
        // Publication's guard compares against the pre-write changeCount and
        // expires with our own write. Namespace reads carry the receive policy;
        // native owner retirement cancels an uncompleted transfer separately.
        let policy = Self::new(root, files.clone(), spec.inventory.roots.clone(), current);
        policy.await_publication.store(true, Ordering::Release);
        let native: Arc<dyn NativeFiles> = policy.clone();
        files.bind_native(Arc::downgrade(&native))?;
        policy.start();
        Ok(Some(policy))
    }
    fn new(root: PathBuf, files: Arc<dyn Namespace>, roots: Vec<FileNode>, guard: ReaderGuard) -> Arc<Self> {
        Arc::new_cyclic(|owner| Self {
            owner: owner.clone(),
            root,
            files,
            roots,
            guard,
            state: Mutex::new(State::Idle),
            changed: Condvar::new(),
            cancelled: AtomicBool::new(false),
            publication: Mutex::new(None),
            await_publication: AtomicBool::new(false),
        })
    }

    /// The automatic-fetch and manual receive paths share one worker/result.
    fn start(self: &Arc<Self>) {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, State::Idle) {
            *state = State::Running;
            let this = self.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("oink-small-files".into())
                .spawn(move || {
                    let deadline = Instant::now() + Duration::from_millis(crate::model::NO_PROGRESS_MS);
                    let result = this.materialize(deadline);
                    let mut state = this.state.lock().unwrap();
                    if !matches!(*state, State::Running) {
                        return; // timed-out/retired callback: Drop removes only our partial tree.
                    }
                    let result = result.and_then(|prepared| {
                        this.check(deadline)?;
                        prepared.commit()
                    });
                    *state = match result {
                        Ok(urls) => State::Ready(urls),
                        Err(error) => State::Failed(content::reason(&error)),
                    };
                    this.changed.notify_all();
                })
            {
                *state = State::Failed(content::reason(&error));
            }
        }
    }

    pub(super) fn published(&self, name: Option<&str>, count: u64) {
        let identity = super::clipboard_key::current(name).and_then(|key| {
            if super::pasteboard::change_count(name) != count {
                return Err(io::ErrorKind::Interrupted.into());
            }
            Ok(ClipboardIdentity {
                name: name.map(str::to_owned),
                key,
            })
        });
        *self.publication.lock().unwrap() = Some(identity);
        self.changed.notify_all();
    }

    /// A paste waits for already-started prefetch within its own 5s budget.
    pub(super) fn resolve(self: &Arc<Self>, node: NodeId, deadline: Instant) -> io::Result<String> {
        self.resolve_while(node, deadline, &|| true)
    }
    fn resolve_while(
        self: &Arc<Self>,
        node: NodeId,
        deadline: Instant,
        current: &dyn Fn() -> bool,
    ) -> io::Result<String> {
        if !self.roots.iter().any(|root| root.id == node) {
            return Err(content::unavailable(Unavailable::NoSuchItem));
        }
        self.start();
        let mut state = self.state.lock().unwrap();
        loop {
            if !current() {
                drop(state);
                self.retire();
                return Err(content::unavailable(Unavailable::Cancelled));
            }
            match &*state {
                State::Ready(urls) => {
                    return urls
                        .get(&node)
                        .map(|path| super::pasteboard::file_url_string(path))
                        .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem));
                }
                State::Failed(reason) => return Err(content::unavailable(*reason)),
                _ => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                *state = State::Failed(Unavailable::NoProgress);
                self.cancelled.store(true, Ordering::Release);
                self.changed.notify_all();
                drop(state);
                self.files.cancel_native_delivery();
                return Err(content::unavailable(Unavailable::NoProgress));
            }
            state = self
                .changed
                .wait_timeout(state, left.min(Duration::from_millis(10)))
                .unwrap()
                .0;
        }
    }

    fn check(&self, deadline: Instant) -> io::Result<()> {
        if Instant::now() >= deadline {
            return Err(content::unavailable(Unavailable::NoProgress));
        }
        if self.cancelled.load(Ordering::Acquire) {
            return Err(content::unavailable(Unavailable::Cancelled));
        }
        (self.guard)()
    }

    pub(super) fn retire(&self) {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, State::Ready(_) | State::Failed(_)) {
            return;
        }
        *state = State::Failed(Unavailable::Cancelled);
        self.cancelled.store(true, Ordering::Release);
        self.changed.notify_all();
        drop(state);
        self.files.cancel_native_delivery();
    }

    fn materialize(&self, deadline: Instant) -> io::Result<Prepared> {
        self.check(deadline)?;
        self.files.start_native_delivery();
        self.files
            .prepare_while(&|| !self.cancelled.load(Ordering::Acquire) && (self.guard)().is_ok())?;
        self.check(deadline)?;
        if self.files.roots() != self.roots {
            return Err(content::unavailable(Unavailable::Superseded));
        }
        private_dir(&self.root)?;
        // Serializes admission and restart cleanup across processes. Wait only
        // on this worker; AppKit waits on the bounded condition variable above.
        let lock = loop {
            if let Some(lock) = lock_root(&self.root)? {
                break lock;
            }
            self.check(deadline)?;
            std::thread::sleep(Duration::from_millis(5));
        };
        let (partial, destination) = loop {
            let id = crate::store::BatchId::random()?.name();
            let partial = self.root.join(format!("{id}.partial"));
            let destination = self.root.join(format!("{id}.ready"));
            if destination.try_exists()? {
                continue;
            }
            match fs::DirBuilder::new().mode(0o700).create(&partial) {
                Ok(()) => break (partial, destination),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        let batch_lock = lock_root(&partial)?.ok_or_else(|| content::unavailable(Unavailable::ReadFailed))?;
        let mut prepared = Prepared {
            partial,
            destination,
            names: Vec::new(),
            _lock: batch_lock,
        };
        // A stalled source must not hold the whole cache's admission lock.
        // Maintenance skips this batch's locked partial and reserves its full
        // small-file limit until its bounded callback's worker exits.
        drop(lock);
        let mut names = content::namespace::Names::new(crate::sync::files::TargetOs::current());
        private_dir(&prepared.partial.join("files"))?;
        let mut received_bytes = 0u64;
        for advertised in &self.roots {
            self.check(deadline)?;
            let node = self.files.select(advertised.id)?;
            if node != *advertised {
                return Err(content::unavailable(Unavailable::Superseded));
            }
            let ContentSize::Known(size) = node.size else {
                return Err(content::unavailable(Unavailable::InvalidData));
            };
            let base = names.entry(&node)?.path;
            let mut name = base.clone();
            let mut suffix = 1;
            let mut writer = loop {
                let result = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(prepared.partial.join("files").join(&name));
                match result {
                    Ok(writer) => break writer,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        // APFS/HFS+ can equate canonically equivalent Unicode
                        // names which the shared metadata table keeps distinct.
                        // create_new is the final collision authority on disk.
                        suffix += 1;
                        name = crate::sync::files::names::with_suffix(
                            &base,
                            suffix,
                            crate::sync::files::TargetOs::current(),
                        );
                        self.check(deadline)?;
                    }
                    Err(error) => return Err(error),
                }
            };
            let mut reader = content::terminal_reader(self.files.open(node.id, node.version, 0)?);
            let mut copied = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                self.check(deadline)?;
                let count = reader.read(&mut buffer)?;
                self.check(deadline)?;
                if count == 0 {
                    break;
                }
                copied = copied
                    .checked_add(count as u64)
                    .ok_or_else(|| content::unavailable(Unavailable::LimitExceeded))?;
                if copied > size {
                    return Err(content::unavailable(Unavailable::Superseded));
                }
                writer.write_all(&buffer[..count])?;
            }
            if copied != size {
                return Err(content::unavailable(Unavailable::ReadFailed));
            }
            received_bytes += copied;
            if let Some(ms) = node.modified_ms {
                let time = if ms >= 0 {
                    UNIX_EPOCH.checked_add(Duration::from_millis(ms as u64))
                } else {
                    UNIX_EPOCH.checked_sub(Duration::from_millis(ms.unsigned_abs()))
                };
                writer.set_modified(time.ok_or_else(|| content::unavailable(Unavailable::InvalidData))?)?;
            }
            received::apply_exec_bits(&writer, node.exec_bits)?;
            writer.sync_all()?;
            prepared.names.push((node.id, name));
        }
        let mut publication = self.publication.lock().unwrap();
        while self.await_publication.load(Ordering::Acquire) && publication.is_none() {
            self.check(deadline)?;
            publication = self
                .changed
                .wait_timeout(publication, Duration::from_millis(10))
                .unwrap()
                .0;
        }
        let clipboard = match publication.as_ref() {
            Some(Ok(identity)) => Some(identity.clone()),
            Some(Err(error)) => return Err(content::unavailable(content::reason(error))),
            None => None,
        };
        drop(publication);
        let record = Record {
            created_ms: received::now(),
            clipboard,
            bytes: received_bytes,
        };
        let mut writer = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(prepared.partial.join(RECORD))?;
        writer.write_all(&serde_json::to_vec(&record)?)?;
        writer.sync_all()?;
        self.check(deadline)?;
        Ok(prepared)
    }
}

pub(super) struct Source(pub Arc<SmallFiles>);
impl super::clipboard_files::FileUrlSource for Source {
    fn prepare(
        &self,
        _request: &super::clipboard_files::FileUrlRequest,
        current: &dyn Fn() -> bool,
    ) -> Result<super::clipboard_files::FileUrlPublication, crate::ClipboardError> {
        let deadline = Instant::now() + Duration::from_millis(crate::model::NO_PROGRESS_MS);
        let urls = objc2::rc::autoreleasepool(|_| {
            self.0
                .roots
                .iter()
                .map(|root| {
                    self.0
                        .resolve_while(root.id, deadline, current)
                        .map(|url| (root.id, url))
                })
                .collect::<io::Result<HashMap<_, _>>>()
        })
        .map_err(|error| crate::ClipboardError::ContentUnavailable(content::reason(&error)))?;
        Ok(super::clipboard_files::FileUrlPublication {
            urls,
            lease: Box::new(self.0.clone()),
        })
    }
    fn published(&self, name: Option<&str>, count: u64) {
        self.0.published(name, count);
    }
    fn cancel(&self) {
        self.0.retire();
    }
    fn cancel_on_read_timeout(&self) -> bool {
        true
    }
    fn channel(&self) -> crate::copy_diagnostics::Channel {
        crate::copy_diagnostics::Channel::Cache
    }
}
impl super::clipboard_files::FileUrlLease for Arc<SmallFiles> {
    fn cancel(&self) {
        self.retire();
    }
}

fn reader_lease(path: &Path) -> io::Result<File> {
    let batch = path.parent().and_then(Path::parent).ok_or(io::ErrorKind::InvalidData)?;
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(batch.join(".lock"))?;
    // SAFETY: the owned File retains the shared consumer lock until drop.
    if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(lease)
}

impl NativeFiles for SmallFiles {
    fn lease(&self) -> io::Result<content::namespace::NativeLease> {
        let state = self.state.lock().unwrap();
        let State::Ready(paths) = &*state else {
            return Err(io::ErrorKind::WouldBlock.into());
        };
        let path = paths.values().next().ok_or(io::ErrorKind::InvalidData)?;
        Ok(Box::new(reader_lease(path)?))
    }

    fn persist_link(&self, descriptor: &content::linked::Descriptor) -> io::Result<()> {
        descriptor.validate()?;
        if descriptor.provisional || descriptor.roots != self.roots {
            return Err(content::unavailable(Unavailable::Permission));
        }
        // This receiver has no resumable placeholder or remote-linked final
        // item. Authority lives for this bounded transfer only; restart removes
        // partials and complete copies never reconnect to the original source.
        Ok(())
    }
    fn open(&self, node: NodeId, offset: u64) -> io::Result<crate::FileReader> {
        use std::io::{Seek, SeekFrom};
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| content::unavailable(Unavailable::Cancelled))?;
        owner.resolve(node, Instant::now() + content::native_read::NATIVE_READ_TIMEOUT)?;
        let state = self.state.lock().unwrap();
        let State::Ready(paths) = &*state else {
            return Err(content::unavailable(Unavailable::ReadFailed));
        };
        let path = paths
            .get(&node)
            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
        let lease = reader_lease(path)?;
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        struct Reader {
            file: File,
            _lease: File,
        }
        impl Read for Reader {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.file.read(bytes)
            }
        }
        Ok(Box::new(Reader { file, _lease: lease }))
    }
    fn metadata(&self, node: NodeId) -> io::Result<crate::FileMetadata> {
        let node = self
            .roots
            .iter()
            .find(|root| root.id == node)
            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
        let ContentSize::Known(size) = node.size else {
            return Err(content::unavailable(Unavailable::InvalidData));
        };
        Ok(crate::FileMetadata {
            size,
            modified_ms: node.modified_ms,
            exec_bits: node.exec_bits,
        })
    }
    fn download(
        &self,
        check: &ReaderGuard,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> io::Result<Vec<StoredFile>> {
        check()?;
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| content::unavailable(Unavailable::Cancelled))?;
        owner.resolve(
            self.roots[0].id,
            Instant::now() + Duration::from_millis(crate::model::NO_PROGRESS_MS),
        )?;
        check()?;
        let files: Vec<_> = self
            .roots
            .iter()
            .map(|node| StoredFile {
                node: node.id,
                version: node.version,
                size: self.metadata(node.id).unwrap().size,
            })
            .collect();
        progress(files.iter().map(|file| file.size).sum(), files.len() as u64);
        Ok(files)
    }
}

struct Prepared {
    partial: PathBuf,
    destination: PathBuf,
    names: Vec<(NodeId, String)>,
    _lock: File,
}
impl Prepared {
    fn commit(self) -> io::Result<HashMap<NodeId, PathBuf>> {
        fs::rename(&self.partial, &self.destination)?;
        crate::cache::changed(self.destination.parent().unwrap());
        Ok(self
            .names
            .iter()
            .map(|(id, name)| (*id, self.destination.join("files").join(name)))
            .collect())
    }
}
impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.partial);
    }
}

fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(content::unavailable(Unavailable::Permission));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}
fn lock_root(root: &Path) -> io::Result<Option<File>> {
    lock_at(root, true)
}
fn lock_at(root: &Path, create: bool) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root.join(".lock"))?;
    // SAFETY: the owned descriptor remains alive until the File is dropped.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(error)
    }
}
fn remove_directory(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        // A completed worker can rename a locked partial between enumeration
        // and inspection. Its complete received path must remain untouched.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
#[cfg(all(test, debug_assertions))]
mod tests;
