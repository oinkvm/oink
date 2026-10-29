//! Application streams and active downloads use the same Cloud Files item.
use super::*;
use crate::content::{
    ReaderGuard,
    linked::Descriptor,
    namespace::{NativeFiles, StoredFile},
};
use crate::model::NodeId;
use std::{
    fs::File,
    io::{Seek, SeekFrom},
    os::windows::{
        fs::MetadataExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
};
use windows_sys::Win32::System::{
    IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
    Threading::{CreateEventW, WaitForSingleObject},
};

pub(super) type DownloadResult = Result<Vec<StoredFile>, (crate::model::Unavailable, Option<i32>)>;

/// Use the same consumer handle/lease as OLE streams, allowing the provider's
/// hydration writes while a user-session package receiver waits for content.
pub(crate) fn open_path(path: &Path) -> Option<io::Result<crate::FileReader>> {
    let service = CURRENT.lock().unwrap().upgrade()?;
    open_path_in(&service, path)
}
pub(super) fn open_path_in(service: &Arc<Service>, path: &Path) -> Option<io::Result<crate::FileReader>> {
    let deliveries: Vec<_> = service.deliveries.lock().unwrap().values().cloned().collect();
    for delivery in deliveries {
        let index = delivery.native_paths.lock().unwrap().iter().position(|url| url == path);
        if let Some(index) = index {
            let node = {
                let journal = service.received.lock().unwrap();
                journal
                    .sources
                    .get(&delivery.id)
                    .and_then(|source| source.nodes.get(index))
                    .map(|node| node.id)
            };
            return Some((|| {
                let node = node.ok_or(io::ErrorKind::NotFound)?;
                // A Drop reader upgrades provisional hover credentials before
                // starting I/O; a restored catalog already has durable authority.
                let namespace = delivery.contents().namespace();
                namespace
                    .and_then(|namespace| namespace.native_open(node, 0))
                    .unwrap_or_else(|| delivery.open(node, 0))
            })());
        }
    }
    None
}

pub(super) struct ReaderPin(Arc<AtomicUsize>);
impl ReaderPin {
    pub(super) fn new(delivery: &Delivery) -> io::Result<Self> {
        let service = delivery.service.upgrade().ok_or(io::ErrorKind::NotConnected)?;
        let journal = service.received.lock().unwrap();
        Self::admit(delivery, &journal)
    }
    fn admit(delivery: &Delivery, journal: &received::Journal) -> io::Result<Self> {
        if journal.cancelled.contains(&delivery.id) || journal.cleanup_inflight.iter().any(|(id, _)| *id == delivery.id)
        {
            return Err(io::ErrorKind::Interrupted.into());
        }
        delivery.readers.fetch_add(1, Ordering::AcqRel);
        Ok(Self(delivery.readers.clone()))
    }
}
impl Drop for ReaderPin {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct NativeReader {
    file: File,
    _pin: ReaderPin,
}
impl io::Read for NativeReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.file, bytes)
    }
}
impl Delivery {
    fn native_entry(&self, node: NodeId) -> io::Result<(Arc<Service>, usize, PathBuf)> {
        let service = self.service.upgrade().ok_or(io::ErrorKind::NotConnected)?;
        let index = {
            let journal = service.received.lock().unwrap();
            let source = journal.sources.get(&self.id).ok_or(io::ErrorKind::NotFound)?;
            source
                .nodes
                .iter()
                .position(|entry| entry.id == node)
                .ok_or(io::ErrorKind::NotFound)?
        };
        if self.file(index).entry.is_dir || self.file(index).deleted.load(Ordering::Acquire) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let path = self.file_path(index);
        Ok((service, index, path))
    }
    pub(super) fn checked_file(
        &self,
        service: &Service,
        index: usize,
        path: &Path,
        asynchronous: bool,
    ) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(if asynchronous {
                FILE_FLAG_OVERLAPPED | FILE_FLAG_OPEN_REPARSE_POINT
            } else {
                0
            })
            .open(path)?;
        let (owner, slot, _) = service.handle_identity(file.as_raw_handle(), 0)?;
        if owner != self.id || slot != index {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(file)
    }
    pub(super) fn hydrate_native(
        &self,
        service: &Service,
        index: usize,
        file: &File,
        check: &(dyn Fn() -> io::Result<()> + Send + Sync),
    ) -> io::Result<()> {
        check()?;
        let trace = HydrationTrace {
            request: Some(0),
            index,
        };
        // SAFETY: unnamed auto-owned event; no pointers retained by CreateEventW.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateEventW returned a newly owned handle.
        let event = unsafe { OwnedHandle::from_raw_handle(event) };
        let mut operation = OVERLAPPED {
            hEvent: event.as_raw_handle(),
            ..Default::default()
        };
        let handle = file.as_raw_handle();
        // SAFETY: handle is an owned overlapped file, operation/event stay live
        // until completion (including the cancellation/drain path below).
        let code = unsafe { (service.api.hydrate)(handle, 0, -1, CF_HYDRATE_FLAG_NONE, &mut operation) };
        if code >= 0 {
            return check();
        }
        if (code & 0xffff) != ERROR_IO_PENDING as i32 {
            return trace.hr(78, code);
        }
        let mut progress = self.file(index).hydrated.load(Ordering::Acquire);
        let mut changed = Instant::now();
        loop {
            let next = self.file(index).hydrated.load(Ordering::Acquire);
            if next != progress {
                progress = next;
                changed = Instant::now();
            }
            let failure = check().err().or_else(|| {
                (changed.elapsed().as_millis() >= crate::model::NO_PROGRESS_MS as u128)
                    .then(|| crate::content::unavailable(crate::model::Unavailable::NoProgress))
            });
            if let Some(error) = failure {
                // SAFETY: cancel and drain before releasing OVERLAPPED/event.
                unsafe {
                    CancelIoEx(handle, &operation);
                    let mut transferred = 0;
                    GetOverlappedResult(handle, &operation, &mut transferred, 1);
                }
                return Err(error);
            }
            // SAFETY: event remains owned for this bounded wait.
            let status = unsafe { WaitForSingleObject(event.as_raw_handle(), 25) };
            if status == WAIT_TIMEOUT {
                continue;
            }
            let mut transferred = 0;
            // SAFETY: even an unexpected wait failure drains the outstanding IO.
            let result = unsafe { GetOverlappedResult(handle, &operation, &mut transferred, 1) };
            if result == 0 {
                return trace.io(79, Err(io::Error::last_os_error()));
            }
            check()?;
            return Ok(());
        }
    }
}
impl NativeFiles for Delivery {
    fn lease(&self) -> io::Result<crate::content::namespace::NativeLease> {
        let service = self.service.upgrade().ok_or(io::ErrorKind::NotConnected)?;
        let journal = service.received.lock().unwrap();
        if journal.cancelled.contains(&self.id)
            || journal
                .deliveries
                .get(&self.id)
                .is_none_or(|nodes| nodes.iter().any(|node| node.deleted || node.withdrawn))
        {
            return Err(io::ErrorKind::Interrupted.into());
        }
        Ok(Box::new(ReaderPin::admit(self, &journal)?))
    }
    fn persist_link(&self, descriptor: &Descriptor) -> io::Result<()> {
        let _population = self.population.lock().unwrap();
        descriptor.validate()?;
        let service = self.service.upgrade().ok_or(io::ErrorKind::NotConnected)?;
        let mut journal = service.received.lock().unwrap();
        if journal.cancelled.contains(&self.id) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let previous = journal.sources.get(&self.id).ok_or(io::ErrorKind::NotFound)?.clone();
        if previous
            .descriptor
            .as_ref()
            .is_some_and(|source| source.token != descriptor.token)
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let mut next = previous.clone();
        next.descriptor = next.settlement.remaining_link(descriptor)?;
        let contents = self.restored_contents(&next)?;
        journal.sources.insert(self.id, next);
        if let Err(error) = journal.save(&service.root) {
            journal.sources.insert(self.id, previous);
            return Err(error);
        }
        drop(journal);
        if !descriptor.provisional {
            self.replace_contents(contents)?;
        }
        self.complete_received()?;
        Ok(())
    }
    fn list(
        &self,
        node: NodeId,
        cursor: Option<crate::model::DirectoryCursor>,
    ) -> io::Result<crate::model::DirectoryPage> {
        let _pin = ReaderPin::new(self)?;
        let service = self.service.upgrade().ok_or(io::ErrorKind::NotConnected)?;
        self.populate_directory(node, &|| Ok(()))?;
        let journal = service.received.lock().unwrap();
        let source = journal.sources.get(&self.id).ok_or(io::ErrorKind::NotFound)?;
        if journal.cancelled.contains(&self.id) || source.descriptor.as_ref().is_some_and(|source| source.provisional) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let index = source
            .nodes
            .iter()
            .position(|entry| entry.id == node)
            .ok_or(io::ErrorKind::NotFound)?;
        if self.file(index).deleted.load(Ordering::Acquire)
            || source.nodes[index].kind != crate::model::NodeKind::Directory
        {
            return Err(io::ErrorKind::NotFound.into());
        }
        let children = source
            .nodes
            .iter()
            .enumerate()
            .filter(|(slot, entry)| entry.parent == Some(node) && !self.file(*slot).deleted.load(Ordering::Acquire))
            .map(|(_, entry)| entry.clone())
            .collect::<Vec<_>>();
        crate::content::namespace::page(node, source.nodes[index].version, &children, cursor)
    }

    fn open(&self, node: NodeId, offset: u64) -> io::Result<crate::FileReader> {
        let pin = ReaderPin::new(self)?;
        let (service, index, path) = self.native_entry(node)?;
        // This process connects with BLOCK_SELF_IMPLICIT_HYDRATION. A plain
        // ReadFile is denied while any bytes are dataless, even if another
        // consumer is already hydrating them. Explicitly join/start CF hydration
        // before exposing a synchronous reader; sharing flags alone cannot do
        // this. Keep the read-only waiter and lease through completion, then
        // recheck the reopened identity. The provider's self-hydration block
        // and data-write exclusion remain intact. Exercised on Windows 11 with
        // a delayed native provider and another explicit hydration in flight.
        let waiting = self.checked_file(&service, index, &path, true)?;
        let check = || {
            if service.stopping.load(Ordering::Acquire)
                || self.file(index).deleted.load(Ordering::Acquire)
                || service.received.lock().unwrap().cancelled.contains(&self.id)
            {
                Err(unavailable())
            } else {
                Ok(())
            }
        };
        self.hydrate_native(&service, index, &waiting, &check)?;
        let mut file = self.checked_file(&service, index, &path, false)?;
        drop(waiting);
        file.seek(SeekFrom::Start(offset))?;
        Ok(Box::new(NativeReader { file, _pin: pin }))
    }
    fn metadata(&self, node: NodeId) -> io::Result<FileMetadata> {
        let _pin = ReaderPin::new(self)?;
        let (service, index, path) = self.native_entry(node)?;
        let file = self.checked_file(&service, index, &path, false)?;
        let metadata = file.metadata()?;
        Ok(FileMetadata {
            exec_bits: None,
            size: metadata.len(),
            modified_ms: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| time.as_millis() as i64),
        })
    }
    fn download(
        &self,
        check: &ReaderGuard,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> io::Result<Vec<StoredFile>> {
        let _pin = ReaderPin::new(self)?;
        check()?;
        // Whole-drop delivery has one terminal result (as in Remote). A MOVE
        // waiting for that active download joins its result, rather than racing
        // another flush/journal transaction against the original owner.
        let result = self.download.get_or_init(|| {
            self.downloading.store(true, Ordering::Release);
            let result = self
                .download_body(check, progress)
                .map_err(|error| (crate::content::reason(&error), error.raw_os_error()));
            self.downloading.store(false, Ordering::Release);
            result
        });
        check()?;
        match result {
            Ok(files) => Ok(files.clone()),
            Err((_, Some(code))) => Err(io::Error::from_raw_os_error(*code)),
            Err((reason, None)) => Err(crate::content::unavailable(*reason)),
        }
    }
}
impl Delivery {
    fn download_body(
        &self,
        check: &ReaderGuard,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> io::Result<Vec<StoredFile>> {
        let service = self.service.upgrade().ok_or(io::ErrorKind::NotConnected)?;
        loop {
            check()?;
            let pending = {
                let journal = service.received.lock().unwrap();
                let source = journal.sources.get(&self.id).ok_or(io::ErrorKind::NotFound)?;
                source.pending.iter().next().copied()
            };
            let Some(node) = pending else { break };
            self.populate_directory(node, &**check)?;
        }
        let nodes = service
            .received
            .lock()
            .unwrap()
            .sources
            .get(&self.id)
            .ok_or(io::ErrorKind::NotFound)?
            .nodes
            .clone();
        let mut files = Vec::new();
        let mut total = 0u64;
        for node in nodes.iter().filter(|node| node.kind == crate::model::NodeKind::File) {
            check()?;
            let (_, index, path) = self.native_entry(node.id)?;
            let file = self.checked_file(&service, index, &path, true)?;
            self.hydrate_native(&service, index, &file, &**check)?;
            // The OS can complete hydration before the fetch worker releases
            // its final mutation guard. Join that worker before requesting
            // write access for FlushFileBuffers, and retain its version.
            let state = self.file(index);
            let selected = state.selected.lock().unwrap();
            let version = selected.1.unwrap_or(node.version);
            // A simultaneous MOVE may already have renamed this object. Reopen
            // the hydrated handle, never the old path; keep DELETE sharing so
            // the target's move handle can coexist with completion.
            // SAFETY: the original live handle was identity checked before hydration.
            let handle = unsafe {
                ReOpenFile(
                    file.as_raw_handle(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_DELETE,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: ReOpenFile returned a newly owned handle to the same file.
            let complete = unsafe { File::from_raw_handle(handle) };
            drop(file);
            let file = complete;
            let _resident = protection::Protected::for_file(service.api.clone(), &file)?;
            let size = file.metadata()?.len();
            let moved = match service.handle_identity(file.as_raw_handle(), size) {
                Ok((owner, slot, full)) => {
                    if owner != self.id
                        || slot != index
                        || !full
                        || !service.handle_fully_resident(file.as_raw_handle(), size)?
                    {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    false
                }
                Err(error) if error.raw_os_error() == Some(ERROR_NOT_A_CLOUD_FILE as i32) => {
                    // A successful target-local MOVE converts the hydrated
                    // object to an ordinary file. The original checked handle
                    // was retained through hydration and ReOpenFile, so this is
                    // not a replacement resolved through the old pathname.
                    let root = service.root.canonicalize()?;
                    let current = protection::Protected::file_path(&file)?;
                    if received::relative_path(&root, &current).is_some()
                        || file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                        || size != selected.0.size
                    {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    true
                }
                Err(error) => return Err(error),
            };
            file.sync_all()?;
            if version == 0 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            {
                let mut journal = service.received.lock().unwrap();
                let item = journal
                    .deliveries
                    .get_mut(&self.id)
                    .and_then(|nodes| nodes.get_mut(index))
                    .ok_or(io::ErrorKind::NotFound)?;
                if item.deleted {
                    return Err(io::ErrorKind::NotFound.into());
                }
                item.handed_off = true;
                item.life.materialized = true;
                item.length = size;
                item.deleted |= moved;
                journal.save(&service.root)?;
            }
            drop(selected);
            total = total.checked_add(size).ok_or(io::ErrorKind::InvalidData)?;
            files.push(StoredFile {
                node: node.id,
                version,
                size,
            });
            progress(total, files.len() as u64);
        }
        check()?;
        Ok(files)
    }
}
pub(super) fn bind(delivery: &Arc<Delivery>) -> io::Result<()> {
    if let Some(namespace) = delivery.contents().namespace() {
        let native: Arc<dyn NativeFiles> = delivery.clone();
        namespace.bind_native(Arc::downgrade(&native))?;
    }
    Ok(())
}
