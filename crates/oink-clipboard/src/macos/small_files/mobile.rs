//! Persistent backing for mobile PNG and long-text clipboard publications.
//!
//! NSPasteboard providers may be asked for bytes after the mobile transfer
//! staging directory has been removed. Copying these representations into the
//! existing ClipboardFiles cache makes the native clipboard owner independent
//! of that staging lifetime. The existing cleaner removes old batches once no
//! clipboard identity or reader lock protects them.

use super::*;
use crate::content::{Provider, Translator};
use crate::model::{ContentSize, Inventory, RepresentationData, RepresentationId, StandardFormat};

const SHORT_TEXT_BYTES: u64 = 64 * 1024;

struct Cached {
    inventory: Inventory,
    translator: Arc<dyn Translator>,
    source: Arc<dyn Provider>,
    bodies: HashMap<RepresentationId, PathBuf>,
    // Keep publication protected even if persisting clipboard identity fails.
    _lease: File,
}
impl Provider for Cached {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn translator(&self) -> Arc<dyn Translator> {
        self.translator.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
        let Some(path) = self.bodies.get(&id) else {
            return self.source.open(id);
        };
        let lease = reader_lease(path)?;
        Ok(Box::new(Reader {
            file: File::open(path)?,
            _lease: lease,
        }))
    }
}

struct Reader {
    file: File,
    _lease: File,
}
impl Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.file.read(bytes)
    }
}

pub(in crate::macos) struct Publication {
    path: PathBuf,
    bytes: u64,
    _lock: File,
}
impl Publication {
    pub(in crate::macos) fn published(self, name: Option<&str>, count: u64) -> io::Result<()> {
        let key = super::super::clipboard_key::current(name)?;
        if super::super::pasteboard::change_count(name) != count {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let record = Record {
            created_ms: received::now(),
            clipboard: Some(ClipboardIdentity {
                name: name.map(str::to_owned),
                key,
            }),
            bytes: self.bytes,
        };
        // Write the record through a sibling and rename it atomically so a
        // crash cannot leave a truncated record that the shared cleaner cannot
        // age out.
        let pending = self.path.join(".oink-cache.next");
        let mut writer = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&pending)?;
        writer.write_all(&serde_json::to_vec(&record)?)?;
        writer.sync_all()?;
        fs::rename(pending, self.path.join(RECORD))
    }
}

pub(in crate::macos) fn prepare(spec: &mut NativeSpec, check: &ReaderGuard) -> io::Result<Option<Publication>> {
    let selected: Vec<_> = spec
        .inventory
        .items
        .iter()
        .flat_map(|item| &item.representations)
        .filter(|rep| {
            rep.standard == Some(StandardFormat::Png)
                || (rep.standard == Some(StandardFormat::Text)
                    && matches!(
                        rep.data,
                        RepresentationData::Reader {
                            size: ContentSize::Known(n)
                        } if n > SHORT_TEXT_BYTES
                    ))
        })
        .map(|rep| rep.id)
        .collect();
    if selected.is_empty() {
        return Ok(None);
    }
    check()?;
    let root = ROOT.get().ok_or(io::ErrorKind::NotConnected)?;
    private_dir(root)?;
    let deadline = Instant::now() + Duration::from_millis(crate::model::NO_PROGRESS_MS);
    let admission = loop {
        check()?;
        if let Some(lock) = lock_root(root)? {
            break lock;
        }
        if Instant::now() >= deadline {
            return Err(content::unavailable(crate::model::Unavailable::NoProgress));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let id = crate::store::BatchId::random()?.name();
    let partial = root.join(format!("{id}.partial"));
    fs::DirBuilder::new().mode(0o700).create(&partial)?;
    let prepared = Prepared {
        _lock: lock_root(&partial)?.ok_or(io::ErrorKind::WouldBlock)?,
        partial,
        destination: root.join(format!("{id}.ready")),
        names: vec![],
    };
    drop(admission);
    private_dir(&prepared.partial.join("files"))?;
    let mut bodies = HashMap::new();
    let mut bytes: u64 = 0;
    for representation in spec
        .inventory
        .items
        .iter()
        .flat_map(|item| &item.representations)
        .filter(|rep| selected.contains(&rep.id))
    {
        check()?;
        let mut input = content::terminal_reader(spec.contents.open(representation.id)?);
        let name = representation.id.0.to_string();
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(prepared.partial.join("files").join(&name))?;
        let mut buffer = [0; 64 * 1024];
        loop {
            check()?;
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            output.write_all(&buffer[..n])?;
            bytes = bytes
                .checked_add(n as u64)
                .ok_or_else(|| io::Error::from(io::ErrorKind::FileTooLarge))?;
        }
        output.sync_all()?;
        bodies.insert(representation.id, prepared.destination.join("files").join(name));
    }
    let record = Record {
        created_ms: received::now(),
        clipboard: None,
        bytes,
    };
    let mut metadata = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(prepared.partial.join(RECORD))?;
    metadata.write_all(&serde_json::to_vec(&record)?)?;
    metadata.sync_all()?;
    check()?;
    let path = prepared.destination.clone();
    // Convert the allocation lock to a consumer lock before AppKit can open
    // the provider. The cleaner cannot remove the batch during publication.
    let lock = prepared._lock.try_clone()?;
    prepared.commit()?;
    // SAFETY: `lock` owns a live file descriptor for the lock file, and the
    // nonblocking shared-lock flags are plain constants; `flock` reads no
    // caller-provided pointers and the descriptor stays open after this call.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    spec.contents = Arc::new(Cached {
        inventory: spec.inventory.clone(),
        translator: spec.contents.translator(),
        source: spec.contents.clone(),
        bodies,
        _lease: lock.try_clone()?,
    });
    Ok(Some(Publication {
        path,
        bytes,
        _lock: lock,
    }))
}
