//! Per-user Cloud Files cache. Raw cfapi registration deliberately avoids WinRT
//! shell registration: it adds no Explorer navigation node. Unsigned standard
//! user providers, ordinary app reads and unregister were exercised on Windows
//! 10 22H2 and Windows 11. Session 0 readers instead fail with cloud access denied;
//! the SYSTEM input broker must never host this provider.
use crate::{FileContents, FileEntry, FileMetadata, cloud_files};
use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    io::{self, Read},
    mem::{offset_of, size_of},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::*,
    Storage::{CloudFilters::*, FileSystem::*},
    System::LibraryLoader::*,
};

mod received;
pub(crate) use received::{open_received, received_usage};
type Pin = unsafe extern "system" fn(HANDLE, i32, i32, *mut windows_sys::Win32::System::IO::OVERLAPPED) -> i32;
type Info = unsafe extern "system" fn(HANDLE, i32, *mut c_void, u32, *mut u32) -> i32;
type Register = unsafe extern "system" fn(*const u16, *const CF_SYNC_REGISTRATION, *const CF_SYNC_POLICIES, i32) -> i32;
type Connect = unsafe extern "system" fn(
    *const u16,
    *const CF_CALLBACK_REGISTRATION,
    *const c_void,
    i32,
    *mut CF_CONNECTION_KEY,
) -> i32;
type Disconnect = unsafe extern "system" fn(CF_CONNECTION_KEY) -> i32;
type Unregister = unsafe extern "system" fn(*const u16) -> i32;
type Create = unsafe extern "system" fn(*const u16, *mut CF_PLACEHOLDER_CREATE_INFO, u32, i32, *mut u32) -> i32;
type Execute = unsafe extern "system" fn(*const CF_OPERATION_INFO, *mut CF_OPERATION_PARAMETERS) -> i32;
type Progress = unsafe extern "system" fn(CF_CONNECTION_KEY, i64, i64, i64) -> i32;
type Platform = unsafe extern "system" fn(*mut CF_PLATFORM_INFO) -> i32;
struct Api {
    register: Register,
    connect: Connect,
    disconnect: Disconnect,
    #[allow(dead_code)]
    unregister: Unregister,
    pin: Pin,
    info: Info,
    create: Create,
    execute: Execute,
    progress: Progress,
}
fn wide(p: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    p.as_ref().encode_wide().chain(Some(0)).collect()
}
fn hr(code: i32) -> io::Result<()> {
    if code < 0 {
        Err(io::Error::from_raw_os_error(code & 0xffff))
    } else {
        Ok(())
    }
}
fn unavailable() -> io::Error {
    crate::content::unavailable(crate::model::Unavailable::Cancelled)
}
impl Api {
    fn load() -> io::Result<Arc<Self>> {
        static API: OnceLock<Result<Arc<Api>, i32>> = OnceLock::new();
        // SAFETY: System32 exports below use the exact cfapi ABI; all buffers
        // outlive their calls and the DLL remains loaded for the process.
        API.get_or_init(|| unsafe {
            // Resolve only from System32. A missing DLL/driver must be a reported
            // capability branch, not a loader failure that prevents backend start.
            // The module is retained for the process lifetime, including callbacks.
            let module = LoadLibraryExW(
                wide("cldapi.dll").as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            );
            if module.is_null() {
                return Err(GetLastError() as i32);
            }
            macro_rules! function {
                ($name:literal, $ty:ty) => {{
                    let pointer = GetProcAddress(module, concat!($name, "\0").as_ptr());
                    let Some(pointer) = pointer else {
                        return Err(ERROR_PROC_NOT_FOUND as i32);
                    };
                    std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(pointer)
                }};
            }
            let platform = function!("CfGetPlatformInfo", Platform);
            let mut info = CF_PLATFORM_INFO::default();
            let code = platform(&mut info);
            if code < 0 {
                return Err(code & 0xffff);
            }
            Ok(Arc::new(Api {
                pin: function!("CfSetPinState", Pin),
                info: function!("CfGetPlaceholderInfo", Info),
                register: function!("CfRegisterSyncRoot", Register),
                connect: function!("CfConnectSyncRoot", Connect),
                disconnect: function!("CfDisconnectSyncRoot", Disconnect),
                unregister: function!("CfUnregisterSyncRoot", Unregister),
                create: function!("CfCreatePlaceholders", Create),
                execute: function!("CfExecute", Execute),
                progress: function!("CfReportProviderProgress", Progress),
            }))
        })
        .clone()
        .map_err(io::Error::from_raw_os_error)
    }
}
static CURRENT: Mutex<Weak<Service>> = Mutex::new(Weak::new());

#[derive(Debug)]
pub(super) struct Owner(Arc<Service>);
impl std::ops::Deref for Owner {
    type Target = Service;
    fn deref(&self) -> &Service {
        &self.0
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        // Serialize the last owner against another backend joining the root.
        let mut current = CURRENT.lock().unwrap();
        if self.0.owners.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.stop();
            if current.ptr_eq(&Arc::downgrade(&self.0)) {
                *current = Weak::new();
            }
        }
    }
}

pub(super) fn initialize(cache: PathBuf) -> Option<Owner> {
    let root = cache.join("CloudFiles");
    let mut current = CURRENT.lock().unwrap();
    if let Some(service) = current.upgrade().filter(|s| !s.stopping.load(Ordering::Acquire)) {
        if service.root == root {
            service.owners.fetch_add(1, Ordering::AcqRel);
            return Some(Owner(service));
        }
        tracing::error!(reason = "different_cache", "cloud file provider unavailable");
        return None;
    }
    match Service::new(root) {
        Ok(service) => {
            *current = Arc::downgrade(&service);
            tracing::info!("cloud file provider ready");
            Some(Owner(service))
        }
        Err(error) => {
            // This is a platform capability branch (missing/disabled Cloud Files,
            // non-NTFS volume or failed root registration), not a retry after a
            // delivery failure. Keep virtual formats and report the native code.
            tracing::warn!(
                reason = "platform_unavailable",
                code = error.raw_os_error(),
                "cloud file provider unavailable; offering virtual files only"
            );
            None
        }
    }
}

pub(super) struct Offer {
    created: Instant,
    service: Arc<Service>,
    entries: Vec<FileEntry>,
    contents: Arc<dyn FileContents>,
    prepared: Mutex<Option<Arc<Delivery>>>,
    pub virtual_files: bool,
}
impl Offer {
    pub fn new(
        entries: Vec<FileEntry>,
        contents: Arc<dyn FileContents>,
        inventory: Option<&crate::model::Inventory>,
    ) -> Option<Self> {
        CURRENT.lock().unwrap().upgrade().map(|service| Self {
            created: Instant::now(),
            service,
            entries,
            contents,
            prepared: Mutex::new(None),
            virtual_files: cloud_files::formats(
                inventory.map_or(oink_foundation::clipboard::ClipPlatform::Unknown, |i| i.platform),
                inventory.is_some_and(|i| i.items.iter().any(|item| item.file.is_some() && item.virtual_files)),
                true,
            )
            .virtual_files,
        })
    }
    pub fn hdrop(&self) -> io::Result<Vec<u8>> {
        if self.service.stopping.load(Ordering::Acquire)
            || self.created.elapsed().as_secs() >= cloud_files::RETENTION_SECS
        {
            return Err(unavailable());
        }
        let mut prepared = self.prepared.lock().unwrap();
        if let Some(delivery) = prepared.as_ref() {
            if delivery.expired() {
                return Err(unavailable());
            }
            return Ok(super::files::build_hdrop(&delivery.roots));
        }
        // QueryGetData/EnumFormatEtc never come here. Explorer can ask GetData
        // during hover, so known sizes use the frozen inventory without selecting
        // file contents. Unknown lengths require real-demand metadata; failure is
        // returned, not cached, allowing the same object to retry after Drop.
        let mut entries = self
            .contents
            .descriptor_entries()?
            .unwrap_or_else(|| self.entries.clone());
        for (index, entry) in entries.iter_mut().enumerate().filter(|(_, e)| !e.is_dir) {
            let known = self.contents.descriptor_size_known(index);
            // Empty placeholders never issue FETCH_DATA, so resolve their
            // selected metadata now rather than silently publishing stale zero.
            if !known || entry.size == 0 {
                let metadata = match self.contents.metadata(index)? {
                    Some(metadata) => metadata,
                    None if known => continue, // a flat provider's immutable known length
                    None => return Err(io::Error::new(io::ErrorKind::InvalidData, "cloud file size unknown")),
                };
                if known && metadata.size != entry.size {
                    return Err(crate::content::unavailable(crate::model::Unavailable::Superseded));
                }
                entry.size = metadata.size;
                entry.modified_ms = metadata.modified_ms;
            }
            if entry.size == 0 {
                // Zero-byte placeholders never fetch. Verify the remote EOF now
                // so a stale/incorrect zero Stat cannot become a successful empty
                // file while the shared delivery later reports a stream mismatch.
                let mut reader = self.contents.open(index)?;
                if reader.read(&mut [0u8; 1])? != 0 {
                    return Err(crate::content::unavailable(crate::model::Unavailable::Superseded));
                }
            }
        }
        let delivery = self.service.publish(entries, self.contents.clone(), self.created)?;
        if delivery.expired() {
            return Err(unavailable());
        }
        let bytes = super::files::build_hdrop(&delivery.roots);
        *prepared = Some(delivery);
        Ok(bytes)
    }
}

struct Delivery {
    id: u64,
    path: PathBuf,
    roots: Vec<PathBuf>,
    entries: Vec<FileEntry>,
    contents: Arc<dyn FileContents>,
    created: Instant,
    batch: Option<crate::store::Batch>,
    readers: AtomicUsize,
    opens: Vec<AtomicI64>,
    deleted: Vec<AtomicBool>,
    // Serialize selections and transfers for the same file. OLE entries stay
    // immutable; Cloud Files metadata can advance to a new selected version.
    selected: Vec<Mutex<(FileMetadata, Option<u64>)>>,
}
impl Delivery {
    fn file_path(&self, index: usize) -> PathBuf {
        self.path.join(super::files::descriptor_path(&self.entries[index].path))
    }
    fn remove_withdrawn(&self, service: &Service) {
        // An application's handle may deny deletion until its failed read closes.
        // Keep the tombstone and retry cleanup without allowing another fetch.
        for (index, deleted) in self.deleted.iter().enumerate() {
            let path = self.file_path(index);
            if deleted.load(Ordering::Acquire) && service.native_identity_matches(&path, self.id, index) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    fn expired(&self) -> bool {
        self.created.elapsed().as_secs() >= cloud_files::RETENTION_SECS
            || self
                .batch
                .as_ref()
                .is_some_and(|batch| batch.remaining_retention().is_some_and(|left| left.is_zero()))
    }
    fn active_reads(&self) -> usize {
        self.readers.load(Ordering::Acquire).saturating_add(
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.is_dir)
                .map(|(i, _)| self.opens[i].load(Ordering::Acquire).max(0) as usize)
                .sum::<usize>(),
        )
    }
    fn consumed(&self) -> bool {
        // HDROP transfers paths by reference without a release notification.
        // Browsers can close a preview read then reopen for upload. Hydration,
        // EOF and close therefore cannot consume the publication. Only confirmed
        // native deletion retires it early; other paths keep the bounded deadline.
        self.entries.iter().any(|entry| !entry.is_dir)
            && self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.is_dir)
                .all(|(i, _)| self.deleted[i].load(Ordering::Acquire))
    }
}
struct Request {
    transfer: i64,
    key: i64,
    start: i64,
    end: i64,
    total: i64,
    progress: AtomicU64,
    cancelled: AtomicBool,
}
struct Work {
    service: Arc<Service>,
    delivery: Arc<Delivery>,
    index: usize,
    id: u64,
    request: Arc<Request>,
}
pub(super) struct Service {
    received: Mutex<received::Journal>,
    reader_classes: HashMap<String, crate::received::Reader>,
    api: Arc<Api>,
    root: PathBuf,
    owners: AtomicUsize,
    stopping: AtomicBool,
    connection: Mutex<Option<CF_CONNECTION_KEY>>,
    context: Box<Weak<Service>>,
    deliveries: Mutex<HashMap<u64, Arc<Delivery>>>,
    abandoned: Mutex<Vec<PathBuf>>,
    requests: Mutex<HashMap<u64, Arc<Request>>>,
    jobs: mpsc::Sender<Work>,
    next: AtomicU64,
}
impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CloudFiles")
    }
}
impl Service {
    fn new(root: PathBuf) -> io::Result<Arc<Self>> {
        let api = Api::load()?;
        std::fs::create_dir_all(root.parent().unwrap())?;
        // Cloud Files is NTFS-only, including when OINK_HOME relocates the cache.
        let mut volume = vec![0u16; 32768];
        let mut filesystem = [0u16; 64];
        // SAFETY: both output buffers have the supplied UTF-16 capacities.
        unsafe {
            if GetVolumePathNameW(
                wide(root.parent().unwrap()).as_ptr(),
                volume.as_mut_ptr(),
                volume.len() as u32,
            ) == 0
                || GetVolumeInformationW(
                    volume.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    filesystem.as_mut_ptr(),
                    filesystem.len() as u32,
                ) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        if String::from_utf16_lossy(&filesystem[..filesystem.iter().position(|u| *u == 0).unwrap_or(0)]) != "NTFS" {
            return Err(io::Error::from_raw_os_error(ERROR_NOT_SUPPORTED as i32));
        }
        let existed = root.exists();
        if existed && !received::valid_root(&root)? {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        std::fs::create_dir_all(&root)?;
        let journal = received::Journal::load(&root)?;
        let provider = wide("Oink");
        let version = wide("1");
        let registration = CF_SYNC_REGISTRATION {
            StructSize: size_of::<CF_SYNC_REGISTRATION>() as u32,
            ProviderName: provider.as_ptr(),
            ProviderVersion: version.as_ptr(),
            ..Default::default()
        };
        // FULL permits dehydrated placeholders and always completes the whole
        // file, including ranges beyond the platform's truncated >1GiB request.
        // ALWAYS_FULL population means we create the namespace before publishing
        // HDROP. No WinRT shell integration, streaming or auto-dehydration.
        let policies = CF_SYNC_POLICIES {
            StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
            Hydration: CF_HYDRATION_POLICY {
                Primary: CF_HYDRATION_POLICY_FULL,
                Modifier: 0,
            },
            Population: CF_POPULATION_POLICY {
                Primary: CF_POPULATION_POLICY_ALWAYS_FULL,
                Modifier: 0,
            },
            ..Default::default()
        };
        // SAFETY: registration, policies and their UTF-16 strings remain live.
        hr(unsafe {
            (api.register)(
                wide(&root).as_ptr(),
                &registration,
                &policies,
                if existed {
                    CF_REGISTER_FLAG_UPDATE
                } else {
                    CF_REGISTER_FLAG_NONE
                },
            )
        })?;
        let (jobs, receiver) = mpsc::channel::<Work>();
        let service = Arc::new_cyclic(|weak| Self {
            received: Mutex::new(journal),
            reader_classes: received::reader_classes(),
            api,
            root,
            owners: AtomicUsize::new(1),
            stopping: AtomicBool::new(false),
            connection: Mutex::new(None),
            context: Box::new(weak.clone()),
            deliveries: Mutex::new(HashMap::new()),
            abandoned: Mutex::new(Vec::new()),
            requests: Mutex::new(HashMap::new()),
            jobs,
            next: AtomicU64::new(1),
        });
        let callbacks = [
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_FETCH_DATA,
                Callback: Some(fetch),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
                Callback: Some(cancel),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NOTIFY_FILE_OPEN_COMPLETION,
                Callback: Some(opened),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION,
                Callback: Some(closed),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NOTIFY_RENAME,
                Callback: Some(received::renaming),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NOTIFY_RENAME_COMPLETION,
                Callback: Some(received::renamed),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
                Callback: Some(deleting),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NOTIFY_DELETE_COMPLETION,
                Callback: Some(deleted),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NONE,
                Callback: None,
            },
        ];
        let mut key = CF_CONNECTION_KEY::default();
        // SAFETY: callback table is terminated; the boxed weak context remains
        // stable until disconnect drains all native callbacks.
        hr(unsafe {
            (service.api.connect)(
                wide(&service.root).as_ptr(),
                callbacks.as_ptr(),
                (&*service.context as *const Weak<Service>).cast(),
                CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION
                    | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH
                    | CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO,
                &mut key,
            )
        })?;
        *service.connection.lock().unwrap() = Some(key);
        service.recover_received()?;
        let receiver = Arc::new(Mutex::new(receiver));
        for _ in 0..std::thread::available_parallelism().map_or(4, |n| n.get()) {
            let receiver = receiver.clone();
            std::thread::Builder::new()
                .name("oink-cloud-fetch".into())
                .spawn(move || {
                    loop {
                        let work = receiver.lock().unwrap().recv();
                        let Ok(work) = work else { break };
                        work.run();
                    }
                })?;
        }
        let weak = Arc::downgrade(&service);
        std::thread::Builder::new()
            .name("oink-cloud-retention".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(10));
                    let Some(service) = weak.upgrade() else { break };
                    service.maintain();
                }
            })?;
        Ok(service)
    }
    fn publish(
        &self,
        entries: Vec<FileEntry>,
        contents: Arc<dyn FileContents>,
        created: Instant,
    ) -> io::Result<Arc<Delivery>> {
        if entries.is_empty() || entries.len() > crate::model::MAX_DISCOVERED_NODES {
            return Err(io::Error::from_raw_os_error(ERROR_INVALID_DATA as i32));
        }
        let batch = contents.namespace().and_then(|namespace| namespace.local_batch());
        if batch
            .as_ref()
            .is_some_and(|batch| batch.remaining_retention().is_some_and(|left| left.is_zero()))
        {
            return Err(unavailable());
        }
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
        let id = u64::from_le_bytes(random);
        let origin = contents
            .namespace()
            .and_then(|n| n.origin())
            .unwrap_or(oink_foundation::DeviceId([0; 32]));
        let name = crate::received::source_folder(
            self.root.parent().unwrap(),
            origin,
            &crate::received::source_name(origin),
        )?;
        let source = self.root.join(name);
        let source_id = u64::from_le_bytes(origin.0[..8].try_into().unwrap());
        if !source.exists() {
            match self.create_container(&source, source_id) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        if !cfg!(test) && !self.owned_container(&source, source_id)? {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let path = crate::received::allocate_batch_with(&source, &crate::received::local_stamp(), |path| {
            self.create_container(path, id)
        })?;
        let mut names = HashSet::new();
        let mut roots = Vec::new();
        for entry in &entries {
            let name = super::files::descriptor_path(&entry.path);
            if name.is_empty()
                || name.encode_utf16().count() > 32000
                || entry.size > i64::MAX as u64
                || !names.insert(name.to_lowercase())
            {
                return Err(io::Error::from_raw_os_error(ERROR_INVALID_DATA as i32));
            }
            if !entry.path.contains('/') {
                roots.push(path.join(name));
            }
        }

        let result = (|| {
            // Parent directories precede their children even for flat providers
            // whose descriptor table is not in traversal order.
            let mut order: Vec<_> = entries.iter().enumerate().collect();
            order.sort_by_key(|(_, e)| e.path.matches('/').count());
            for (index, entry) in order {
                let relative = PathBuf::from(super::files::descriptor_path(&entry.path));
                let parent = path.join(relative.parent().unwrap_or(Path::new("")));
                if !parent.is_dir() {
                    return Err(io::Error::from_raw_os_error(ERROR_PATH_NOT_FOUND as i32));
                }
                let name = wide(relative.file_name().unwrap());
                let mut identity = [0u8; 16];
                identity[..8].copy_from_slice(&id.to_le_bytes());
                identity[8..].copy_from_slice(&(index as u64).to_le_bytes());
                let time = super::files::unix_ms_to_filetime(entry.modified_ms.unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64
                })) as i64;
                let mut info = CF_PLACEHOLDER_CREATE_INFO {
                    RelativeFileName: name.as_ptr(),
                    FsMetadata: CF_FS_METADATA {
                        BasicInfo: FILE_BASIC_INFO {
                            CreationTime: time,
                            LastAccessTime: time,
                            LastWriteTime: time,
                            ChangeTime: time,
                            FileAttributes: if entry.is_dir {
                                FILE_ATTRIBUTE_DIRECTORY
                            } else {
                                FILE_ATTRIBUTE_NORMAL
                            },
                        },
                        FileSize: entry.size as i64,
                    },
                    FileIdentity: identity.as_ptr().cast(),
                    FileIdentityLength: 16,
                    Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC
                        | if entry.is_dir {
                            CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION
                        } else {
                            0
                        },
                    ..Default::default()
                };
                let mut processed = 0;
                // SAFETY: one initialized entry, its name and identity remain
                // live through the synchronous create operation.
                hr(unsafe {
                    (self.api.create)(
                        wide(&parent).as_ptr(),
                        &mut info,
                        1,
                        CF_CREATE_FLAG_STOP_ON_ERROR,
                        &mut processed,
                    )
                })?;
                hr(info.Result)?;
                if processed != 1 {
                    return Err(io::Error::from_raw_os_error(ERROR_INVALID_DATA as i32));
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            if std::fs::remove_dir_all(&path).is_err() {
                self.abandoned.lock().unwrap().push(path);
            }
            return Err(error);
        }
        let opens = entries.iter().map(|_| AtomicI64::new(0)).collect();
        let deleted = entries.iter().map(|_| AtomicBool::new(false)).collect();
        let selected = entries
            .iter()
            .map(|entry| {
                Mutex::new((
                    FileMetadata {
                        size: entry.size,
                        modified_ms: entry.modified_ms,
                    },
                    None,
                ))
            })
            .collect();
        let delivery = Arc::new(Delivery {
            id,
            selected,
            opens,
            deleted,
            path,
            roots,
            entries,
            batch,
            contents,
            created,
            readers: AtomicUsize::new(0),
        });
        {
            let mut journal = self.received.lock().unwrap();
            journal.deliveries.insert(
                id,
                delivery
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| received::Node {
                        path: delivery.file_path(i).strip_prefix(&self.root).unwrap().into(),
                        life: crate::received::Item::new(crate::received::now()),
                        directory: e.is_dir,
                        deleted: false,
                        handed_off: e.size == 0,
                        length: e.size,
                    })
                    .collect(),
            );
            if journal.pending_gaps.remove(&id) {
                for node in journal.deliveries.get_mut(&id).unwrap() {
                    node.life.gap(crate::received::now());
                }
            }
            journal.save(&self.root)?;
        }
        for (index, entry) in delivery
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.is_dir && e.size == 0)
        {
            self.native_complete(id, index, entry.size);
        }
        self.deliveries.lock().unwrap().insert(id, delivery.clone());
        tracing::info!(target: "oink_file_diagnostics", file_stage = 1_u8,
            count = delivery.entries.len(), "cloud file placeholders published");
        Ok(delivery)
    }
    fn maintain(&self) {
        let key = *self.connection.lock().unwrap();
        let requests: Vec<_> = self
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|(id, request)| (*id, request.clone()))
            .collect();
        if let Some(key) = key {
            // Every valid progress report resets Cloud Files' fixed 60-second
            // fetch timeout. Snapshot first: native calls can race callbacks and
            // disconnect; neither lifecycle nor request locks may span them.
            for (id, request) in requests {
                if !request.cancelled.load(Ordering::Acquire) {
                    // SAFETY: these scalar keys came from our connection. If it
                    // disconnected meanwhile, cfapi returns an invalid-key error.
                    let code = unsafe {
                        (self.api.progress)(
                            key,
                            request.transfer,
                            request.total,
                            request.progress.load(Ordering::Relaxed) as i64,
                        )
                    };
                    tracing::info!(target: "oink_file_diagnostics",
                        file_stage = 4_u8, file_request = id, file_code = code as u32,
                        bytes = request.progress.load(Ordering::Relaxed), size = request.total,
                        "cloud file progress reported");
                }
            }
        }
        self.received_sweep();
        self.sweep(Instant::now());
    }
    fn sweep(&self, now: Instant) {
        self.abandoned
            .lock()
            .unwrap()
            .retain(|path| match std::fs::remove_dir_all(path) {
                Ok(()) => false,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(_) => true,
            });
        let protected: std::collections::HashSet<_> = self
            .received
            .lock()
            .unwrap()
            .deliveries
            .iter()
            .filter(|(_, nodes)| {
                nodes
                    .iter()
                    .any(|n| !n.directory && !n.deleted && (n.handed_off || n.life.materialized))
            })
            .map(|(id, _)| *id)
            .collect();
        self.deliveries.lock().unwrap().retain(|_, delivery| {
            delivery.remove_withdrawn(self);
            // User OS copies survive transfer expiry. Native deletion and the
            // received policy operate per file before empty tree retirement.
            if protected.contains(&delivery.id) {
                return !delivery.expired() || delivery.active_reads() > 0;
            }
            let remove = cloud_files::retire(
                if delivery.expired() {
                    cloud_files::RETENTION_SECS
                } else {
                    now.saturating_duration_since(delivery.created).as_secs()
                },
                delivery.consumed(),
                Arc::strong_count(delivery).saturating_sub(1),
                delivery.active_reads(),
            );
            if !remove {
                return true;
            }
            // Retire only the transfer owner. The independent journal checks
            // each OS identity, removes stale placeholders and prunes empty
            // directories; never recursively delete a user-visible batch.
            false
        });
    }
}
impl Service {
    fn stop(&self) {
        if self.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        for request in self.requests.lock().unwrap().values() {
            request.cancelled.store(true, Ordering::Release);
        }
        let key = self.connection.lock().unwrap().take();
        if let Some(key) = key {
            // SAFETY: this is our connection; no mutex is held while native
            // disconnect drains callbacks that may need to inspect the service.
            let code = unsafe { (self.api.disconnect)(key) };
            if code < 0 {
                tracing::warn!(code = code as u32, "cloud file disconnect failed");
            }
        }
        // Registration and fully hydrated pinned files outlive this backend.
        let mut journal = self.received.lock().unwrap();
        for node in journal.deliveries.values_mut().flatten() {
            node.life.gap(crate::received::now());
        }
        let _ = journal.save(&self.root);
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.stop();
    }
}

fn operation(connection: CF_CONNECTION_KEY, transfer: i64, request: i64) -> CF_OPERATION_INFO {
    CF_OPERATION_INFO {
        StructSize: size_of::<CF_OPERATION_INFO>() as u32,
        Type: CF_OPERATION_TYPE_TRANSFER_DATA,
        ConnectionKey: connection,
        TransferKey: transfer,
        RequestKey: request,
        ..Default::default()
    }
}
fn transfer(
    api: &Api,
    op: &CF_OPERATION_INFO,
    offset: i64,
    length: i64,
    bytes: *const c_void,
    status: NTSTATUS,
) -> io::Result<()> {
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: (offset_of!(CF_OPERATION_PARAMETERS, Anonymous) + size_of::<CF_OPERATION_PARAMETERS_0_0>()) as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferData: CF_OPERATION_PARAMETERS_0_0 {
                CompletionStatus: status,
                Buffer: bytes,
                Offset: offset,
                Length: length,
                ..Default::default()
            },
        },
    };
    // SAFETY: op/params are initialized; bytes remain valid until CfExecute returns.
    let code = unsafe { (api.execute)(op, &mut params) };
    if code < 0 {
        tracing::warn!(target: "oink_file_diagnostics", file_stage = 12_u8,
            file_code = code as u32, bytes = length, "cloud file native transfer failed");
    }
    hr(code)
}
unsafe extern "system" fn fetch(info: *const CF_CALLBACK_INFO, params: *const CF_CALLBACK_PARAMETERS) {
    // cfapi owns these pointers only for this callback. Copy identities and
    // scalar operation keys; never retain process/path/correlation pointers.
    // SAFETY: cfapi supplies valid FETCH_DATA callback arguments for this call.
    let (info, params) = unsafe { (&*info, (*params).Anonymous.FetchData) };
    // SAFETY: this is our boxed Weak, retained until CfDisconnectSyncRoot returns.
    let service = unsafe { &*info.CallbackContext.cast::<Weak<Service>>() }.upgrade();
    let Some(service) = service else { return };
    let reject = || {
        let _ = transfer(
            &service.api,
            &operation(info.ConnectionKey, info.TransferKey, info.RequestKey),
            params.RequiredFileOffset,
            params.RequiredLength,
            std::ptr::null(),
            STATUS_CLOUD_FILE_REQUEST_CANCELED,
        );
    };
    if service.stopping.load(Ordering::Acquire) || info.FileIdentityLength != 16 || info.FileIdentity.is_null() {
        reject();
        return;
    }
    // SAFETY: the callback length was checked above and cfapi owns these 16 bytes.
    let bytes = unsafe { std::slice::from_raw_parts(info.FileIdentity.cast::<u8>(), 16) };
    let id = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    let index = u64::from_le_bytes(bytes[8..].try_into().unwrap()) as usize;
    let delivery = service.deliveries.lock().unwrap().get(&id).cloned();
    let Some(delivery) = delivery.filter(|d| {
        !d.expired() && d.entries.get(index).is_some_and(|e| !e.is_dir) && !d.deleted[index].load(Ordering::Acquire)
    }) else {
        reject();
        return;
    };
    delivery.readers.fetch_add(1, Ordering::AcqRel);
    let request = Arc::new(Request {
        transfer: info.TransferKey,
        key: info.RequestKey,
        start: params.RequiredFileOffset,
        end: params.RequiredFileOffset.saturating_add(params.RequiredLength),
        total: info.FileSize,
        progress: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
    });
    let request_id = service.next.fetch_add(1, Ordering::Relaxed);
    service.requests.lock().unwrap().insert(request_id, request.clone());
    tracing::info!(target: "oink_file_diagnostics", file_stage = 2_u8,
        file_request = request_id, file_index = index, size = info.FileSize,
        bytes = params.RequiredLength, "cloud file fetch received");
    let work = Work {
        service: service.clone(),
        delivery,
        index,
        id: request_id,
        request,
    };
    // Unbounded queue, bounded data workers: native callback threads remain free
    // to deliver cancellation while an application waits for network content.
    if let Err(error) = service.jobs.send(work) {
        error.0.delivery.readers.fetch_sub(1, Ordering::AcqRel);
        service.requests.lock().unwrap().remove(&request_id);
        reject();
    }
}
unsafe extern "system" fn opened(info: *const CF_CALLBACK_INFO, _: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: cfapi owns this live completion record for the callback duration.
    unsafe { observed_handle(&*info, true, false) };
}
unsafe extern "system" fn closed(info: *const CF_CALLBACK_INFO, params: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: cfapi owns this live completion record for the callback duration.
    unsafe {
        observed_handle(
            &*info,
            false,
            (*params).Anonymous.CloseCompletion.Flags & CF_CALLBACK_CLOSE_COMPLETION_FLAG_DELETED != 0,
        )
    };
}
unsafe fn observed_handle(info: &CF_CALLBACK_INFO, open: bool, deleted: bool) {
    // REQUIRE_PROCESS_INFO identifies provider bookkeeping opens. Do not count
    // our own metadata/cleanup handles as consumers or log process identities.
    // SAFETY: process info and context belong to this registered callback.
    unsafe {
        if !info.ProcessInfo.is_null()
            && (*info.ProcessInfo).ProcessId == windows::Win32::System::Threading::GetCurrentProcessId()
        {
            return;
        }
        if info.FileIdentityLength != 16 || info.FileIdentity.is_null() {
            return;
        }
        let Some(service) = (&*info.CallbackContext.cast::<Weak<Service>>()).upgrade() else {
            return;
        };
        let bytes = std::slice::from_raw_parts(info.FileIdentity.cast::<u8>(), 16);
        let id = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let index = u64::from_le_bytes(bytes[8..].try_into().unwrap()) as usize;
        // Stable source/batch containers are not content readers. Their delete
        // and rename completions are handled separately by path.
        if index == usize::MAX {
            return;
        }
        let reader = if info.ProcessInfo.is_null() || (*info.ProcessInfo).ImagePath.is_null() {
            crate::received::Reader::Unknown
        } else {
            let pointer = (*info.ProcessInfo).ImagePath;
            let mut length = 0;
            while length < 32768 && *pointer.add(length) != 0 {
                length += 1;
            }
            let image = String::from_utf16_lossy(std::slice::from_raw_parts(pointer, length)).to_lowercase();
            service
                .reader_classes
                .get(&image)
                .copied()
                .unwrap_or(if image == "unknown" {
                    crate::received::Reader::Unknown
                } else {
                    crate::received::Reader::App
                })
        };
        service.received_observe(id, index, reader, open, deleted);
        let deliveries = service.deliveries.lock().unwrap();
        let Some(delivery) = deliveries
            .get(&id)
            .filter(|d| d.entries.get(index).is_some_and(|entry| !entry.is_dir))
        else {
            return;
        };
        // Signed balance tolerates thread-pool delivery of a close before its
        // paired open. Only positive balances defer expiry; closing does not
        // imply that a by-reference consumer has released the path.
        if open {
            delivery.opens[index].fetch_add(1, Ordering::AcqRel);
        } else {
            delivery.opens[index].fetch_sub(1, Ordering::AcqRel);
            if deleted {
                delivery.deleted[index].store(true, Ordering::Release);
            }
        }
    }
}
unsafe extern "system" fn deleting(info: *const CF_CALLBACK_INFO, _: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: callback pointers and context are owned by cfapi for this call.
    unsafe {
        let i = &*info;
        let Some(service) = (&*i.CallbackContext.cast::<Weak<Service>>()).upgrade() else {
            return;
        };
        let mut op = operation(i.ConnectionKey, i.TransferKey, i.RequestKey);
        op.Type = CF_OPERATION_TYPE_ACK_DELETE;
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: (offset_of!(CF_OPERATION_PARAMETERS, Anonymous) + size_of::<CF_OPERATION_PARAMETERS_0_7>())
                as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                AckDelete: CF_OPERATION_PARAMETERS_0_7 {
                    Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
                    CompletionStatus: STATUS_SUCCESS,
                },
            },
        };
        let _ = (service.api.execute)(&op, &mut params);
    }
}
unsafe extern "system" fn deleted(info: *const CF_CALLBACK_INFO, _: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: native completion is for the identity provided during publication.
    unsafe {
        received::deleted_path(info);
        observed_handle(&*info, false, true);
    }
}
unsafe extern "system" fn cancel(info: *const CF_CALLBACK_INFO, _: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: cfapi supplies valid cancellation arguments during this callback.
    let info = unsafe { &*info };
    // SAFETY: this is our stable boxed Weak, drained before context destruction.
    let Some(service) = (unsafe { &*info.CallbackContext.cast::<Weak<Service>>() }).upgrade() else {
        return;
    };
    for (id, request) in service
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, r)| r.transfer == info.TransferKey && (info.RequestKey == 0 || r.key == info.RequestKey))
    {
        request.cancelled.store(true, Ordering::Release);
        tracing::info!(target: "oink_file_diagnostics", file_stage = 3_u8,
            file_request = *id, "cloud file fetch canceled by platform");
        // CANCEL_FETCH_DATA is a notification that the platform has already
        // canceled this IO, not an operation requiring acknowledgement. Calling
        // CfExecute from it reenters the native cancellation path. Only signal
        // the worker here; no native API may run under the request-table lock.
        // Do not cancel the shared whole-delivery download: another OLE stream,
        // placeholder or completed-delivery owner may still need the same bytes.
    }
}
impl Work {
    fn run(self) {
        let key = *self.service.connection.lock().unwrap();
        if let Some(key) = key {
            let op = operation(key, self.request.transfer, self.request.key);
            let mut selected = self.delivery.selected[self.index].lock().unwrap();
            if let Err(error) = self.hydrate(&op, &mut selected) {
                let status = if self.request.cancelled.load(Ordering::Acquire) {
                    STATUS_CLOUD_FILE_REQUEST_CANCELED
                } else {
                    match crate::content::reason(&error) {
                        crate::model::Unavailable::NoProgress => STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE,
                        crate::model::Unavailable::Cancelled => STATUS_CLOUD_FILE_REQUEST_CANCELED,
                        crate::model::Unavailable::Superseded => STATUS_CLOUD_FILE_NOT_IN_SYNC,
                        _ => STATUS_CLOUD_FILE_UNSUCCESSFUL,
                    }
                };
                let _ = transfer(
                    &self.service.api,
                    &op,
                    self.request.start,
                    self.request.end.saturating_sub(self.request.start),
                    std::ptr::null(),
                    status,
                );
                if crate::content::reason(&error) == crate::model::Unavailable::NoSuchItem {
                    self.delivery.deleted[self.index].store(true, Ordering::Release);
                    self.delivery.remove_withdrawn(&self.service);
                }
                tracing::warn!(target: "oink_file_diagnostics", file_stage = 11_u8,
                    file_request = self.id, file_index = self.index,
                    file_code = status as u32, file_reason = crate::content::reason(&error) as u8,
                    bytes = self.request.progress.load(Ordering::Relaxed),
                    "cloud file hydration failed");
            }
        }
        self.delivery.readers.fetch_sub(1, Ordering::AcqRel);
        self.service.requests.lock().unwrap().remove(&self.id);
    }
    fn update_placeholder(&self, op: &CF_OPERATION_INFO, metadata: FileMetadata) -> io::Result<()> {
        // Metadata changes arrive while a user read is pending. In Windows 11
        // probes CfUpdatePlaceholder blocked when resizing from FETCH_DATA.
        // RESTART_HYDRATION is the documented FULL-policy metadata update for
        // this situation: it invalidates partial old bytes, changes size/mtime,
        // and reissues pending reads. Never transfer under the old request after
        // restart; the next callback opens the exact newly selected version.
        let time = metadata
            .modified_ms
            .map_or(0, |ms| super::files::unix_ms_to_filetime(ms) as i64);
        let metadata = CF_FS_METADATA {
            BasicInfo: FILE_BASIC_INFO {
                LastWriteTime: time,
                ChangeTime: time,
                ..Default::default()
            },
            FileSize: metadata.size as i64,
        };
        let mut op = *op;
        op.Type = CF_OPERATION_TYPE_RESTART_HYDRATION;
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: (offset_of!(CF_OPERATION_PARAMETERS, Anonymous) + size_of::<CF_OPERATION_PARAMETERS_0_3>())
                as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                RestartHydration: CF_OPERATION_PARAMETERS_0_3 {
                    Flags: CF_OPERATION_RESTART_HYDRATION_FLAG_NONE,
                    FsMetadata: &metadata,
                    ..Default::default()
                },
            },
        };
        tracing::info!(target: "oink_file_diagnostics", file_stage = 7_u8,
            file_request = self.id, file_index = self.index, size = metadata.FileSize,
            "cloud file hydration restart entered");
        // SAFETY: the operation keys, metadata and union arm remain live for
        // this synchronous CfExecute; null identity retains delivery/index.
        let code = unsafe { (self.service.api.execute)(&op, &mut params) };
        tracing::info!(target: "oink_file_diagnostics", file_stage = 8_u8,
            file_request = self.id, file_index = self.index, file_code = code as u32,
            "cloud file hydration restart returned");
        hr(code)
    }

    fn hydrate(&self, op: &CF_OPERATION_INFO, selected: &mut (FileMetadata, Option<u64>)) -> io::Result<()> {
        if self.request.cancelled.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        if self.delivery.deleted[self.index].load(Ordering::Acquire) {
            return Err(crate::content::unavailable(crate::model::Unavailable::NoSuchItem));
        }
        tracing::info!(target: "oink_file_diagnostics",
            file_stage = 5_u8, file_request = self.id, file_index = self.index,
            "cloud file selecting source metadata");
        let current = self
            .delivery
            .contents
            .placeholder_metadata(self.index)?
            .unwrap_or((selected.0, selected.1.unwrap_or(0)));
        let (metadata, version) = current;
        tracing::info!(target: "oink_file_diagnostics", file_stage = 6_u8,
            file_request = self.id, file_index = self.index, size = metadata.size,
            "cloud file source metadata selected");
        if metadata.size > i64::MAX as u64 {
            return Err(crate::content::unavailable(crate::model::Unavailable::InvalidData));
        }
        if self.request.cancelled.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        if metadata.size < selected.0.size {
            // CfUpdatePlaceholder stalled during active hydration, and
            // RESTART_HYDRATION stalled on shrink in Windows 11 probes. Fail
            // explicitly rather than leaving the native read waiting forever
            // or completing it with padded/mixed bytes. A fresh publication
            // is currently required for the smaller selected version.
            tracing::warn!("cloud placeholder cannot shrink during an active read");
            return Err(crate::content::unavailable(crate::model::Unavailable::Superseded));
        }
        // Publishing size/timestamps does not select a source version. Binding
        // the first version with unchanged metadata must serve the pending IO,
        // rather than invalidate it with RESTART_HYDRATION. A known version,
        // including zero for flat providers, still detects later replacements.
        let changed = selected.0 != metadata || selected.1.is_some_and(|old| old != version);
        // Even an explicit fetch of an empty stream must pass exact-version
        // EOF before updating native metadata. An EOF version error must not
        // expose an empty file as successful hydration.
        if metadata.size == 0 {
            let mut reader = self.delivery.contents.open_placeholder(self.index, version)?;
            if reader.read(&mut [0u8; 1])? != 0 {
                return Err(io::Error::from_raw_os_error(ERROR_INVALID_DATA as i32));
            }
            if self.request.cancelled.load(Ordering::Acquire) {
                return Err(unavailable());
            }
            if changed {
                self.update_placeholder(op, metadata)?;
            }
            *selected = (metadata, Some(version));
            return Ok(());
        }
        if changed {
            self.update_placeholder(op, metadata)?;
            *selected = (metadata, Some(version));
            return Ok(()); // cfapi reissues IO against the updated placeholder
        }
        *selected = (metadata, Some(version));
        tracing::info!(target: "oink_file_diagnostics", file_stage = 9_u8,
            file_request = self.id, file_index = self.index, size = metadata.size,
            "cloud file source stream opening");
        let mut reader = self.delivery.contents.open_placeholder(self.index, version)?;
        let mut buffer = vec![0u8; 64 * 1024];
        let mut offset = 0u64;
        while offset < metadata.size {
            if self.request.cancelled.load(Ordering::Acquire) {
                return Err(unavailable());
            }
            let count = buffer.len().min((metadata.size - offset) as usize);
            reader.read_exact(&mut buffer[..count])?;
            // Verify EOF before making the final range visible. This invokes the
            // existing remote stream's selected-version/integrity completion check.
            if offset + count as u64 == metadata.size && reader.read(&mut [0u8; 1])? != 0 {
                return Err(io::Error::from_raw_os_error(ERROR_INVALID_DATA as i32));
            }
            if self.request.cancelled.load(Ordering::Acquire) {
                return Err(unavailable());
            }
            transfer(
                &self.service.api,
                op,
                offset as i64,
                count as i64,
                buffer.as_ptr().cast(),
                STATUS_SUCCESS,
            )?;
            offset += count as u64;
            self.request.progress.store(offset, Ordering::Relaxed);
        }
        self.service
            .native_complete(self.delivery.id, self.index, metadata.size);
        tracing::info!(target: "oink_file_diagnostics", file_stage = 10_u8,
            file_request = self.id, file_index = self.index, bytes = offset,
            "cloud file hydration completed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI32;
    static SERIAL: Mutex<()> = Mutex::new(());
    static FAIL_AFTER: AtomicI32 = AtomicI32::new(-1);
    static PROGRESS_SERVICE: Mutex<Weak<Service>> = Mutex::new(Weak::new());
    static PROGRESS_UNLOCKED: AtomicBool = AtomicBool::new(false);
    static UPDATES: Mutex<Vec<(u64, i64)>> = Mutex::new(Vec::new());
    static TRANSFERS: Mutex<Vec<(i64, Vec<u8>, NTSTATUS)>> = Mutex::new(Vec::new());
    unsafe fn string(p: *const u16) -> String {
        let mut length = 0;
        // SAFETY: fake API callers supply a live, terminated UTF-16 string.
        unsafe {
            while *p.add(length) != 0 {
                length += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p, length))
        }
    }
    unsafe extern "system" fn create(
        parent: *const u16,
        info: *mut CF_PLACEHOLDER_CREATE_INFO,
        _: u32,
        _: i32,
        count: *mut u32,
    ) -> i32 {
        if FAIL_AFTER.load(Ordering::Relaxed) == 0 {
            return 0x80070070u32 as i32;
        }
        FAIL_AFTER.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: the production publisher passes one live entry and output count.
        unsafe {
            let info = &mut *info;
            let path = PathBuf::from(string(parent)).join(string(info.RelativeFileName));
            if path.exists() {
                info.Result = 0x800700b7u32 as i32;
                *count = 0;
                return info.Result;
            }
            if info.FsMetadata.BasicInfo.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                std::fs::create_dir(path).unwrap();
            } else {
                std::fs::File::create(path)
                    .unwrap()
                    .set_len(info.FsMetadata.FileSize as u64)
                    .unwrap();
            }
            info.Result = 0;
            *count = 1;
        }
        0
    }
    unsafe extern "system" fn execute(op: *const CF_OPERATION_INFO, params: *mut CF_OPERATION_PARAMETERS) -> i32 {
        // SAFETY: production supplies the union arm selected by Type; all input
        // metadata and buffers remain live for these synchronous fake calls.
        unsafe {
            if (*op).Type == CF_OPERATION_TYPE_RESTART_HYDRATION {
                let metadata = &*(*params).Anonymous.RestartHydration.FsMetadata;
                UPDATES
                    .lock()
                    .unwrap()
                    .push((metadata.FileSize as u64, metadata.BasicInfo.LastWriteTime));
                return 0;
            }
            let p = (*params).Anonymous.TransferData;
            let bytes = if p.Buffer.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(p.Buffer.cast::<u8>(), p.Length as usize).to_vec()
            };
            TRANSFERS.lock().unwrap().push((p.Offset, bytes, p.CompletionStatus));
        }
        0
    }
    unsafe extern "system" fn register(
        _: *const u16,
        _: *const CF_SYNC_REGISTRATION,
        _: *const CF_SYNC_POLICIES,
        _: i32,
    ) -> i32 {
        0
    }
    unsafe extern "system" fn connect(
        _: *const u16,
        _: *const CF_CALLBACK_REGISTRATION,
        _: *const c_void,
        _: i32,
        _: *mut CF_CONNECTION_KEY,
    ) -> i32 {
        0
    }
    unsafe extern "system" fn disconnect(_: CF_CONNECTION_KEY) -> i32 {
        0
    }
    unsafe extern "system" fn unregister(_: *const u16) -> i32 {
        0
    }
    unsafe extern "system" fn progress(_: CF_CONNECTION_KEY, _: i64, _: i64, _: i64) -> i32 {
        if let Some(service) = PROGRESS_SERVICE.lock().unwrap().upgrade() {
            PROGRESS_UNLOCKED.store(
                service.connection.try_lock().is_ok() && service.requests.try_lock().is_ok(),
                Ordering::Relaxed,
            );
        }
        0
    }
    unsafe extern "system" fn fake_pin(
        _: HANDLE,
        _: i32,
        _: i32,
        _: *mut windows_sys::Win32::System::IO::OVERLAPPED,
    ) -> i32 {
        0
    }
    unsafe extern "system" fn fake_info(_: HANDLE, _: i32, _: *mut c_void, _: u32, _: *mut u32) -> i32 {
        0x80070178u32 as i32
    }
    fn fake(root: PathBuf) -> (Arc<Service>, mpsc::Receiver<Work>) {
        FAIL_AFTER.store(-1, Ordering::Relaxed);
        TRANSFERS.lock().unwrap().clear();
        UPDATES.lock().unwrap().clear();
        std::fs::create_dir(&root).unwrap();
        let api = Arc::new(Api {
            pin: fake_pin,
            info: fake_info,
            register,
            connect,
            disconnect,
            unregister,
            create,
            execute,
            progress,
        });
        let (jobs, receiver) = mpsc::channel();
        (
            Arc::new_cyclic(|weak| Service {
                received: Mutex::default(),
                reader_classes: HashMap::new(),
                api,
                root,
                owners: AtomicUsize::new(1),
                stopping: AtomicBool::new(false),
                connection: Mutex::new(Some(CF_CONNECTION_KEY::default())),
                context: Box::new(weak.clone()),
                deliveries: Mutex::new(HashMap::new()),
                abandoned: Mutex::new(Vec::new()),
                requests: Mutex::new(HashMap::new()),
                jobs,
                next: AtomicU64::new(1),
            }),
            receiver,
        )
    }
    struct Bytes {
        bytes: Vec<u8>,
        opens: AtomicUsize,
        size: AtomicU64,
    }
    impl Bytes {
        fn new(size: usize) -> Arc<Self> {
            Arc::new(Self {
                bytes: (0..size).map(|i| b'a' + (i % 26) as u8).collect(),
                opens: AtomicUsize::new(0),
                size: AtomicU64::new(size as u64),
            })
        }
    }
    impl FileContents for Bytes {
        fn open(&self, _: usize) -> io::Result<crate::FileReader> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(io::Cursor::new(self.bytes.clone())))
        }
        fn metadata(&self, _: usize) -> io::Result<Option<crate::FileMetadata>> {
            Ok(Some(crate::FileMetadata {
                size: self.size.load(Ordering::Relaxed),
                modified_ms: Some(1000),
            }))
        }
    }
    struct Versioned {
        size: u64,
        missing: bool,
        fail_eof: bool,
        selected: AtomicU64,
    }
    impl FileContents for Versioned {
        fn open(&self, _: usize) -> io::Result<crate::FileReader> {
            Err(crate::content::unavailable(crate::model::Unavailable::Superseded))
        }
        fn placeholder_metadata(&self, _: usize) -> io::Result<Option<(crate::FileMetadata, u64)>> {
            if self.missing {
                return Err(crate::content::unavailable(crate::model::Unavailable::NoSuchItem));
            }
            Ok(Some((
                crate::FileMetadata {
                    size: self.size,
                    modified_ms: Some(2000),
                },
                7,
            )))
        }
        fn open_placeholder(&self, _: usize, version: u64) -> io::Result<crate::FileReader> {
            assert_eq!(version, 7);
            self.selected.store(version, Ordering::Relaxed);
            struct Reader {
                left: u64,
                fail: bool,
            }
            impl Read for Reader {
                fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                    if self.left == 0 && self.fail {
                        return Err(crate::content::unavailable(crate::model::Unavailable::Superseded));
                    }
                    let count = self.left.min(buffer.len() as u64) as usize;
                    buffer[..count].fill(b'v');
                    self.left -= count as u64;
                    Ok(count)
                }
            }
            Ok(Box::new(Reader {
                left: self.size,
                fail: self.fail_eof,
            }))
        }
    }
    #[test]
    fn placeholder_reads_select_latest_version_instead_of_immutable_ole_descriptor() {
        let _serial = SERIAL.lock().unwrap();
        for size in [131073, 10] {
            let temp = tempfile::tempdir().unwrap();
            let (service, receiver) = fake(temp.path().join("CloudFiles"));
            let source = Arc::new(Versioned {
                size,
                missing: false,
                fail_eof: false,
                selected: AtomicU64::new(0),
            });
            let delivery = service
                .publish(vec![entry("file", 10, false)], source.clone(), Instant::now())
                .unwrap();
            request(&service, &delivery, &receiver, 0, false);
            assert_eq!(source.selected.load(Ordering::Relaxed), 7);
            assert_eq!(
                *UPDATES.lock().unwrap(),
                [(size, super::super::files::unix_ms_to_filetime(2000) as i64)]
            );
            let transfers = TRANSFERS.lock().unwrap();
            assert!(transfers.iter().all(|(_, _, status)| *status == STATUS_SUCCESS));
            assert_eq!(
                transfers.iter().map(|(_, bytes, _)| bytes.len()).sum::<usize>(),
                size as usize
            );
        }
    }
    #[test]
    fn missing_placeholder_is_withdrawn_and_cannot_be_fetched_again() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, receiver) = fake(temp.path().join("CloudFiles"));
        let source = Arc::new(Versioned {
            size: 10,
            missing: true,
            fail_eof: false,
            selected: AtomicU64::new(0),
        });
        let delivery = service
            .publish(vec![entry("file", 10, false)], source.clone(), Instant::now())
            .unwrap();
        request(&service, &delivery, &receiver, 0, false);
        assert!(delivery.deleted[0].load(Ordering::Acquire));
        assert!(
            delivery.path.join("file").exists(),
            "a native identity query failure preserves the path"
        );
        assert!(delivery.deleted[0].load(Ordering::Acquire));
        assert!(
            !request(&service, &delivery, &receiver, 0, false),
            "withdrawn identities must not queue a new source read"
        );
        assert_eq!(source.selected.load(Ordering::Relaxed), 0);
        assert!(
            TRANSFERS
                .lock()
                .unwrap()
                .iter()
                .all(|(_, _, status)| *status != STATUS_SUCCESS)
        );
    }
    #[test]
    fn version_changed_at_eof_never_completes_last_hydration_range() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, receiver) = fake(temp.path().join("CloudFiles"));
        let source = Arc::new(Versioned {
            size: 65537,
            missing: false,
            fail_eof: true,
            selected: AtomicU64::new(0),
        });
        let delivery = service
            .publish(vec![entry("file", 65537, false)], source.clone(), Instant::now())
            .unwrap();
        request(&service, &delivery, &receiver, 0, false);
        assert_eq!(source.selected.load(Ordering::Relaxed), 7);
        let transfers = TRANSFERS.lock().unwrap();
        assert_eq!(
            transfers
                .iter()
                .filter(|(_, _, status)| *status == 0)
                .map(|(_, bytes, _)| bytes.len())
                .sum::<usize>(),
            65536
        );
        assert_eq!(transfers.last().unwrap().2, STATUS_CLOUD_FILE_NOT_IN_SYNC);
    }
    #[test]
    fn shrinking_selected_version_fails_without_entering_blocking_native_resize() {
        let _serial = SERIAL.lock().unwrap();
        for size in [3, 0] {
            let temp = tempfile::tempdir().unwrap();
            let (service, receiver) = fake(temp.path().join("CloudFiles"));
            let source = Arc::new(Versioned {
                size,
                missing: false,
                fail_eof: false,
                selected: AtomicU64::new(0),
            });
            let delivery = service
                .publish(vec![entry("file", 10, false)], source.clone(), Instant::now())
                .unwrap();
            request(&service, &delivery, &receiver, 0, false);
            assert_eq!(source.selected.load(Ordering::Relaxed), 0);
            assert!(UPDATES.lock().unwrap().is_empty());
            assert!(
                TRANSFERS
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(_, _, status)| *status == STATUS_CLOUD_FILE_NOT_IN_SYNC)
            );
        }
    }
    #[test]
    fn empty_version_error_never_completes_native_metadata_update() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, receiver) = fake(temp.path().join("CloudFiles"));
        let source = Arc::new(Versioned {
            size: 0,
            missing: false,
            fail_eof: true,
            selected: AtomicU64::new(0),
        });
        let delivery = service
            .publish(vec![entry("file", 0, false)], source, Instant::now())
            .unwrap();
        request(&service, &delivery, &receiver, 0, false);
        assert!(
            UPDATES.lock().unwrap().is_empty(),
            "truncation would complete a failed read as an empty file"
        );
        assert_eq!(
            TRANSFERS.lock().unwrap().last().unwrap().2,
            STATUS_CLOUD_FILE_NOT_IN_SYNC
        );
    }
    fn pending_request() -> Arc<Request> {
        Arc::new(Request {
            transfer: 7,
            key: 0,
            start: 0,
            end: 4096,
            total: 4096,
            progress: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
        })
    }
    #[test]
    fn cancel_callback_is_a_notification_without_native_api_reentry() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake(temp.path().join("CloudFiles"));
        let request = pending_request();
        service.requests.lock().unwrap().insert(1, request.clone());
        let info = CF_CALLBACK_INFO {
            CallbackContext: (&*service.context as *const Weak<Service>).cast_mut().cast(),
            TransferKey: 7,
            ..Default::default()
        };
        // SAFETY: the fake owns callback records and context for this call.
        unsafe {
            cancel(&info, &CF_CALLBACK_PARAMETERS::default());
        }
        assert!(request.cancelled.load(Ordering::Acquire));
        assert!(
            TRANSFERS.lock().unwrap().is_empty(),
            "CANCEL_FETCH_DATA must not reenter CfExecute while the native cancellation is being delivered"
        );
    }
    #[test]
    fn progress_report_does_not_hold_lifecycle_or_request_locks() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake(temp.path().join("CloudFiles"));
        service.requests.lock().unwrap().insert(1, pending_request());
        *PROGRESS_SERVICE.lock().unwrap() = Arc::downgrade(&service);
        PROGRESS_UNLOCKED.store(false, Ordering::Relaxed);
        service.maintain();
        assert!(
            PROGRESS_UNLOCKED.load(Ordering::Relaxed),
            "native progress must not prevent fetch workers, cancellation or disconnect from acquiring their locks"
        );
        *PROGRESS_SERVICE.lock().unwrap() = Weak::new();
    }
    struct SameMetadata(Arc<Versioned>);
    impl FileContents for SameMetadata {
        fn open(&self, index: usize) -> io::Result<crate::FileReader> {
            self.0.open(index)
        }
        fn placeholder_metadata(&self, _: usize) -> io::Result<Option<(FileMetadata, u64)>> {
            Ok(Some((
                FileMetadata {
                    size: self.0.size,
                    modified_ms: Some(1000),
                },
                7,
            )))
        }
        fn open_placeholder(&self, index: usize, version: u64) -> io::Result<crate::FileReader> {
            self.0.open_placeholder(index, version)
        }
    }
    #[test]
    fn first_selected_version_with_matching_metadata_does_not_restart_pending_io() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, receiver) = fake(temp.path().join("CloudFiles"));
        let source = Arc::new(Versioned {
            size: 18613,
            missing: false,
            fail_eof: false,
            selected: AtomicU64::new(0),
        });
        let delivery = service
            .publish(
                vec![entry("file", 18613, false)],
                Arc::new(SameMetadata(source.clone())),
                Instant::now(),
            )
            .unwrap();
        request(&service, &delivery, &receiver, 0, false);
        assert!(
            UPDATES.lock().unwrap().is_empty(),
            "binding an initially unknown source version must not invalidate unchanged placeholder metadata"
        );
        assert_eq!(source.selected.load(Ordering::Relaxed), 7);
        assert_eq!(
            TRANSFERS
                .lock()
                .unwrap()
                .iter()
                .map(|(_, bytes, _)| bytes.len())
                .sum::<usize>(),
            18613
        );
    }
    #[test]
    fn a_previously_selected_zero_version_still_detects_a_replacement() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, receiver) = fake(temp.path().join("CloudFiles"));
        let source = Arc::new(Versioned {
            size: 10,
            missing: false,
            fail_eof: false,
            selected: AtomicU64::new(0),
        });
        let delivery = service
            .publish(
                vec![entry("file", 10, false)],
                Arc::new(SameMetadata(source.clone())),
                Instant::now(),
            )
            .unwrap();
        delivery.selected[0].lock().unwrap().1 = Some(0);
        request(&service, &delivery, &receiver, 0, false);
        assert_eq!(UPDATES.lock().unwrap().len(), 1);
        assert_eq!(source.selected.load(Ordering::Relaxed), 7);
        assert_eq!(delivery.selected[0].lock().unwrap().1, Some(7));
    }

    fn entry(path: &str, size: u64, dir: bool) -> FileEntry {
        FileEntry {
            path: path.into(),
            is_dir: dir,
            size,
            modified_ms: Some(1000),
            local_path: None,
        }
    }
    fn request(
        service: &Arc<Service>,
        delivery: &Arc<Delivery>,
        receiver: &mpsc::Receiver<Work>,
        index: usize,
        cancel_request: bool,
    ) -> bool {
        let id = *service.deliveries.lock().unwrap().keys().next().unwrap();
        let mut identity = [0u8; 16];
        identity[..8].copy_from_slice(&id.to_le_bytes());
        identity[8..].copy_from_slice(&(index as u64).to_le_bytes());
        let info = CF_CALLBACK_INFO {
            CallbackContext: (&*service.context as *const Weak<Service>).cast_mut().cast(),
            FileIdentity: identity.as_ptr().cast(),
            FileIdentityLength: 16,
            FileSize: delivery.selected[index].lock().unwrap().0.size as i64,
            TransferKey: 1,
            ..Default::default()
        };
        let params = CF_CALLBACK_PARAMETERS {
            Anonymous: CF_CALLBACK_PARAMETERS_0 {
                FetchData: CF_CALLBACK_PARAMETERS_0_1 {
                    RequiredLength: info.FileSize,
                    ..Default::default()
                },
            },
            ..Default::default()
        };
        // SAFETY: these live records implement the same callback ABI as cfapi.
        unsafe {
            fetch(&info, &params);
            if cancel_request {
                cancel(&info, &params);
            }
        }
        if let Ok(work) = receiver.try_recv() {
            let before = UPDATES.lock().unwrap().len();
            work.run();
            if UPDATES.lock().unwrap().len() != before && delivery.selected[index].lock().unwrap().0.size != 0 {
                assert!(request(service, delivery, receiver, index, false));
            }
            true
        } else {
            false
        }
    }
    fn notify(service: &Arc<Service>, index: usize, open: bool, deleted: bool) {
        let id = *service.deliveries.lock().unwrap().keys().next().unwrap();
        let mut identity = [0u8; 16];
        identity[..8].copy_from_slice(&id.to_le_bytes());
        identity[8..].copy_from_slice(&(index as u64).to_le_bytes());
        let mut process = CF_PROCESS_INFO {
            ProcessId: u32::MAX,
            ..Default::default()
        };
        let info = CF_CALLBACK_INFO {
            CallbackContext: (&*service.context as *const Weak<Service>).cast_mut().cast(),
            ProcessInfo: &mut process,
            FileIdentity: identity.as_ptr().cast(),
            FileIdentityLength: 16,
            ..Default::default()
        };
        // SAFETY: the fake owns these records through the synchronous callback.
        unsafe {
            observed_handle(&info, open, deleted);
        }
    }
    #[test]
    fn materialized_received_policy_survives_expiry_stop_and_reload_without_backing() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake(temp.path().join("CloudFiles"));
        let data = Bytes::new(3);
        let delivery = service
            .publish(vec![entry("media.bin", 3, false)], data, Instant::now())
            .unwrap();
        {
            let mut journal = service.received.lock().unwrap();
            let node = &mut journal.deliveries.get_mut(&delivery.id).unwrap()[0];
            node.life.materialized = true;
            node.handed_off = true;
            node.life.observe(crate::received::Reader::App, crate::received::now());
        }
        service.sweep(Instant::now() + Duration::from_secs(cloud_files::RETENTION_SECS + 1));
        assert!(delivery.file_path(0).exists());
        service.stop();
        assert!(delivery.file_path(0).exists());
        let recovered = received::Journal::load(&service.root).unwrap();
        let node = &recovered.deliveries[&delivery.id][0];
        assert!(node.life.materialized && node.life.observation_gap);
        assert!(!node.life.shell_only());
    }
    #[test]
    fn unknown_reader_disqualifies_the_whole_batch_and_system_does_not_renew_usage() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake(temp.path().join("CloudFiles"));
        let data = Bytes::new(1);
        let delivery = service
            .publish(vec![entry("a", 1, false), entry("b", 1, false)], data, Instant::now())
            .unwrap();
        service.received_observe(delivery.id, 0, crate::received::Reader::Unknown, true, false);
        assert!(
            service.received.lock().unwrap().deliveries[&delivery.id]
                .iter()
                .all(|n| n.life.observation_gap)
        );
    }
    #[test]
    fn unpaired_close_and_prepublication_reads_create_batch_observation_gaps() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake(temp.path().join("CloudFiles"));
        service.received_observe(123, 0, crate::received::Reader::App, true, false);
        assert!(service.received.lock().unwrap().pending_gaps.contains(&123));
        let delivery = service
            .publish(
                vec![entry("one", 1, false), entry("two", 1, false)],
                Bytes::new(1),
                Instant::now(),
            )
            .unwrap();
        service.received_observe(delivery.id, 0, crate::received::Reader::Shell, false, false);
        assert!(
            service.received.lock().unwrap().deliveries[&delivery.id]
                .iter()
                .all(|n| n.life.observation_gap)
        );
    }
    #[test]
    fn placeholders_are_lazy_hydration_uses_whole_verified_stream_and_consumed_cleanup() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, receiver) = fake(temp.path().join("CloudFiles"));
        let bytes = Bytes::new(131073);
        let delivery = service
            .publish(
                vec![entry("folder/child", 131073, false), entry("folder", 0, true)],
                bytes.clone(),
                Instant::now(),
            )
            .unwrap();
        assert_eq!(bytes.opens.load(Ordering::Relaxed), 0);
        assert_eq!(
            std::fs::metadata(delivery.path.join("folder/child")).unwrap().len(),
            131073
        );
        notify(&service, 0, true, false);
        request(&service, &delivery, &receiver, 0, false);
        assert!(!delivery.consumed());
        service.sweep(delivery.created + Duration::from_secs(cloud_files::RETENTION_SECS));
        assert!(delivery.path.exists(), "an app's open data handle defers expiry");
        notify(&service, 0, false, false);
        let transfers = TRANSFERS.lock().unwrap();
        assert_eq!(
            transfers.iter().flat_map(|(_, b, _)| b.clone()).collect::<Vec<_>>(),
            bytes.bytes
        );
        assert_eq!(
            transfers.iter().map(|(at, _, _)| *at).collect::<Vec<_>>(),
            [0, 65536, 131072]
        );
        assert!(transfers.iter().all(|(_, _, status)| *status == 0));
        drop(transfers);
        assert_eq!(delivery.readers.load(Ordering::Relaxed), 0);
        service.maintain();
        assert!(delivery.path.exists()); // native owner pins early eviction
        let path = delivery.path.clone();
        drop(delivery);
        service.maintain();
        assert!(path.exists(), "a closed read may be reopened by a browser blob");
        notify(&service, 0, true, false);
        notify(&service, 0, false, true);
        service.maintain();
        assert!(
            service.deliveries.lock().unwrap().is_empty(),
            "deleted items release their transfer owner"
        );
        assert!(
            path.exists(),
            "the fake callback does not physically delete native files"
        );
    }
    #[test]
    fn cancelled_fetch_returns_cloud_error_without_opening_source() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, receiver) = fake(temp.path().join("CloudFiles"));
        let bytes = Bytes::new(10);
        let delivery = service
            .publish(vec![entry("file", 10, false)], bytes.clone(), Instant::now())
            .unwrap();
        request(&service, &delivery, &receiver, 0, true);
        assert_eq!(bytes.opens.load(Ordering::Relaxed), 0);
        assert!(
            TRANSFERS
                .lock()
                .unwrap()
                .iter()
                .all(|(_, _, status)| *status == STATUS_CLOUD_FILE_REQUEST_CANCELED)
        );
    }
    #[test]
    fn creation_failure_keeps_only_source_folder_and_expiry_releases_transfer_owner() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake(temp.path().join("CloudFiles"));
        let bytes = Bytes::new(1);
        FAIL_AFTER.store(1, Ordering::Relaxed);
        assert!(
            service
                .publish(
                    vec![entry("one", 1, false), entry("two", 1, false)],
                    bytes.clone(),
                    Instant::now()
                )
                .is_err()
        );
        let sources: Vec<_> = std::fs::read_dir(&service.root).unwrap().collect();
        assert_eq!(sources.len(), 1);
        assert_eq!(
            std::fs::read_dir(sources[0].as_ref().unwrap().path()).unwrap().count(),
            0
        );
        FAIL_AFTER.store(-1, Ordering::Relaxed);
        let delivery = service
            .publish(vec![entry("one", 1, false)], bytes, Instant::now())
            .unwrap();
        delivery.readers.store(1, Ordering::Release);
        service.sweep(delivery.created + Duration::from_secs(cloud_files::RETENTION_SECS));
        assert!(delivery.path.exists());
        delivery.readers.store(0, Ordering::Release);
        service.sweep(delivery.created + Duration::from_secs(cloud_files::RETENTION_SECS));
        assert!(service.deliveries.lock().unwrap().is_empty());
        assert!(
            delivery.path.exists(),
            "without native identity evidence, never recursively delete received files"
        );
    }
    #[test]
    fn empty_placeholders_verify_eof_before_publication() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake(temp.path().join("CloudFiles"));
        let dishonest = Bytes::new(1);
        dishonest.size.store(0, Ordering::Relaxed);
        let offer = Offer {
            created: Instant::now(),
            service: service.clone(),
            entries: vec![entry("empty", 0, false)],
            contents: dishonest.clone(),
            prepared: Mutex::new(None),
            virtual_files: true,
        };
        assert!(offer.hdrop().is_err());
        assert!(service.deliveries.lock().unwrap().is_empty());
        let empty = Bytes::new(0);
        let offer = Offer {
            created: Instant::now(),
            service: service.clone(),
            entries: vec![entry("empty", 0, false)],
            contents: empty.clone(),
            prepared: Mutex::new(None),
            virtual_files: true,
        };
        assert!(offer.hdrop().is_ok());
        assert_eq!(empty.opens.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn late_hdrop_request_does_not_restart_publication_retention() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake(temp.path().join("CloudFiles"));
        let offer = Offer {
            created: Instant::now() - Duration::from_secs(cloud_files::RETENTION_SECS),
            service: service.clone(),
            entries: vec![entry("one", 1, false)],
            contents: Bytes::new(1),
            prepared: Mutex::new(None),
            virtual_files: true,
        };
        assert!(offer.hdrop().is_err());
        assert_eq!(service.deliveries.lock().unwrap().len(), 0);
    }
    #[test]
    fn backend_shutdown_disconnects_even_when_a_native_object_remains() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, receiver) = fake(temp.path().join("CloudFiles"));
        let owner = Owner(service.clone());
        let contents = Bytes::new(1);
        let offer = Offer {
            created: Instant::now(),
            service: service.clone(),
            entries: vec![entry("one", 1, false)],
            contents: contents.clone(),
            prepared: Mutex::new(None),
            virtual_files: false,
        };
        offer.hdrop().unwrap();
        let root = service.root.clone();
        drop(owner);
        assert!(service.connection.lock().unwrap().is_none());
        assert!(root.exists(), "backend exit retains the registered received root");
        assert!(offer.hdrop().is_err());
        assert_eq!(contents.opens.load(Ordering::Relaxed), 0);
        assert!(receiver.try_recv().is_err());
    }
    #[test]
    fn hdrop_only_object_omits_virtual_formats_and_shutdown_disables_paths() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake(temp.path().join("CloudFiles"));
        let contents = Bytes::new(1);
        let entries = vec![entry("one", 1, false)];
        let offer = Offer {
            created: Instant::now(),
            service,
            entries: entries.clone(),
            contents: contents.clone(),
            prepared: Mutex::new(None),
            virtual_files: false,
        };
        let worker = super::super::worker::Worker::spawn().unwrap();
        worker
            .run(move |_| {
                use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL, TYMED_ISTREAM};
                let f = super::super::formats();
                let (data, shared) = super::super::vfile::build(
                    super::super::vfile::Spec {
                        cloud: Some(offer),
                        descriptors: super::super::files::descriptors(&entries),
                        fixed: vec![(f.preferred_drop_effect, 1u32.to_le_bytes().to_vec())],
                        contents,
                        cf_descriptor: f.file_descriptor,
                        cf_contents: f.file_contents,
                    },
                    true,
                );
                let format = |cf: u32, medium: u32| FORMATETC {
                    cfFormat: cf as u16,
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: -1,
                    tymed: medium,
                    ..Default::default()
                };
                // SAFETY: live local COM object and initialized format structures.
                unsafe {
                    assert!(data.QueryGetData(&format(15, TYMED_HGLOBAL.0 as u32)).is_ok());
                    assert!(
                        data.QueryGetData(&format(f.file_descriptor, TYMED_HGLOBAL.0 as u32))
                            .is_err()
                    );
                    assert!(
                        data.QueryGetData(&format(f.file_contents, TYMED_ISTREAM.0 as u32))
                            .is_err()
                    );
                }
                // Flush uses this same shared transition and must not leave HDROP
                // paths to a provider that is about to disconnect.
                super::super::vfile::disable_files(&shared);
                // SAFETY: same local object and live format structure.
                unsafe {
                    assert!(data.QueryGetData(&format(15, TYMED_HGLOBAL.0 as u32)).is_err());
                }
            })
            .unwrap();
    }
    #[test]
    #[ignore = "requires a standard interactive Windows desktop session"]
    fn native_abandoned_root() {
        let cache = PathBuf::from(std::env::var_os("OINK_CLOUD_NATIVE_CACHE").unwrap());
        let _service = initialize(cache).unwrap();
        let offer = Offer::new(vec![entry("abandoned.bin", 10, false)], Bytes::new(10), None).unwrap();
        offer.hdrop().unwrap();
        std::process::exit(0); // intentionally skip destructors to exercise crash recovery
    }
    #[test]
    #[ignore = "requires a standard interactive Windows desktop session"]
    fn native_reader() {
        let path = std::env::var_os("OINK_CLOUD_NATIVE_READ").unwrap();
        if let Ok(expected) = std::env::var("OINK_CLOUD_NATIVE_VERSION_SIZE") {
            let result = if let Some(destination) = std::env::var_os("OINK_CLOUD_NATIVE_COPY") {
                let destination = PathBuf::from(destination);
                use windows::Win32::{System::Com::*, UI::Shell::*};
                use windows::core::PCWSTR;
                // SAFETY: this isolated child owns its STA and all shell objects;
                // the copy's source/destination strings stay live through calls.
                unsafe {
                    CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
                    let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER).unwrap();
                    op.SetOperationFlags(FOF_SILENT | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_NOCONFIRMMKDIR)
                        .unwrap();
                    let source: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide(&path).as_ptr()), None).unwrap();
                    let folder: IShellItem =
                        SHCreateItemFromParsingName(PCWSTR(wide(destination.parent().unwrap()).as_ptr()), None)
                            .unwrap();
                    op.CopyItem(
                        &source,
                        &folder,
                        PCWSTR(wide(destination.file_name().unwrap()).as_ptr()),
                        None,
                    )
                    .unwrap();
                    op.PerformOperations().unwrap();
                    assert!(!op.GetAnyOperationsAborted().unwrap().as_bool());
                }
                std::fs::read(destination)
            } else {
                std::fs::read(path)
            };
            if expected == "fail" {
                assert!(result.is_err(), "a failed version must not complete hydration");
            } else {
                let bytes = result.unwrap();
                assert_eq!(bytes.len(), expected.parse::<usize>().unwrap());
                assert!(bytes.iter().all(|byte| *byte == b'v'));
            }
            return;
        }
        let mut reader = std::fs::File::open(path).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 131073);
        assert!(bytes.iter().enumerate().all(|(i, b)| *b == b'a' + (i % 26) as u8));
        let marker = PathBuf::from(std::env::var_os("OINK_CLOUD_NATIVE_HOLD").unwrap());
        std::fs::write(&marker, b"ready").unwrap();
        for _ in 0..250 {
            if marker.with_extension("done").exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("parent did not release held reader");
    }
    #[test]
    #[ignore = "requires a standard interactive Windows desktop session"]
    fn native_placeholder_updates_selected_version_before_read() {
        let temp = tempfile::tempdir().unwrap();
        let service = initialize(temp.path().to_owned()).unwrap();
        let root = service.root.clone();
        for (size, missing, fail_eof) in [
            (3, false, false),
            (131073, false, false),
            (10, false, false),
            (0, false, false),
            (10, true, false),
            (65537, false, true),
        ] {
            let source = Arc::new(Versioned {
                size,
                missing,
                fail_eof,
                selected: AtomicU64::new(0),
            });
            let offer = Offer::new(vec![entry("version.bin", 10, false)], source.clone(), None).unwrap();
            let paths = super::super::files::parse_hdrop(&offer.hdrop().unwrap(), |_| unreachable!());
            assert_eq!(std::fs::metadata(&paths[0]).unwrap().len(), 10);
            eprintln!("native selected version: size={size} missing={missing} fail_eof={fail_eof}");
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "windows::cloud::tests::native_reader",
                    "--ignored",
                    "--nocapture",
                ])
                .env("OINK_CLOUD_NATIVE_READ", &paths[0])
                .env(
                    "OINK_CLOUD_NATIVE_VERSION_SIZE",
                    if missing || fail_eof || size < 10 {
                        "fail".into()
                    } else {
                        size.to_string()
                    },
                )
                .status()
                .unwrap();
            assert!(status.success());
            if missing {
                for _ in 0..50 {
                    service.maintain();
                    if !paths[0].exists() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                assert!(!paths[0].exists(), "NoSuchItem must withdraw the native path");
            } else {
                let metadata = std::fs::metadata(&paths[0]).unwrap();
                assert_eq!(metadata.len(), if size < 10 { 10 } else { size });
                assert_eq!(
                    metadata
                        .modified()
                        .unwrap()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis(),
                    if size < 10 { 1000 } else { 2000 }
                );
                assert_eq!(source.selected.load(Ordering::Relaxed), if size < 10 { 0 } else { 7 });
            }
        }
        drop(service);
        assert!(root.exists(), "backend exit preserves received files");
        cleanup_native_test_root(&root);
    }
    // Test domains now require explicit cleanup: production owner shutdown
    // deliberately preserves registration and received content.
    fn cleanup_native_test_root(root: &Path) {
        assert!(root.is_absolute() && root.starts_with(std::env::temp_dir()));
        assert_eq!(root.file_name().unwrap(), "CloudFiles");
        let api = Api::load().unwrap();
        // SAFETY: only this test's dedicated temporary sync root is unregistered.
        hr(unsafe { (api.unregister)(wide(root).as_ptr()) }).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(!root.exists());
    }
    #[test]
    #[ignore = "requires a standard interactive Windows desktop session"]
    fn native_explorer_copy_first_selection_preserves_matching_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let service = initialize(temp.path().to_owned()).unwrap();
        let source = Arc::new(Versioned {
            size: 18613,
            missing: false,
            fail_eof: false,
            selected: AtomicU64::new(0),
        });
        let offer = Offer::new(
            vec![entry("mark.png", 18613, false)],
            Arc::new(SameMetadata(source.clone())),
            None,
        )
        .unwrap();
        let paths = super::super::files::parse_hdrop(&offer.hdrop().unwrap(), |_| unreachable!());
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows::cloud::tests::native_reader",
                "--ignored",
                "--nocapture",
            ])
            .env("OINK_CLOUD_NATIVE_READ", &paths[0])
            .env("OINK_CLOUD_NATIVE_VERSION_SIZE", "18613")
            .env("OINK_CLOUD_NATIVE_COPY", temp.path().join("copied.png"))
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(source.selected.load(Ordering::Relaxed), 7);
        assert_eq!(paths[0].file_name().unwrap(), "mark.png");
        drop(offer);
        let root = service.root.clone();
        drop(service);
        assert!(root.exists());
        cleanup_native_test_root(&root);
    }
    #[test]
    #[ignore = "requires a standard interactive Windows desktop session"]
    fn native_placeholder_hydrates_and_survives_owner_restart() {
        let temp = tempfile::tempdir().unwrap();
        let abandoned = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows::cloud::tests::native_abandoned_root",
                "--ignored",
                "--nocapture",
            ])
            .env("OINK_CLOUD_NATIVE_CACHE", temp.path())
            .status()
            .unwrap();
        assert!(abandoned.success());
        assert!(temp.path().join("CloudFiles").exists());
        let service = initialize(temp.path().to_owned()).unwrap();
        assert!(
            service
                .received
                .lock()
                .unwrap()
                .deliveries
                .values()
                .flatten()
                .all(|n| n.directory || n.deleted),
            "restart removes only the abandoned never-hydrated files"
        );
        let contents = Bytes::new(131073);
        let root = service.root.clone();
        let offer = Offer::new(vec![entry("file.bin", 131073, false)], contents.clone(), None).unwrap();
        let paths = super::super::files::parse_hdrop(&offer.hdrop().unwrap(), |_| unreachable!());
        assert_eq!(contents.opens.load(Ordering::Relaxed), 0);
        let marker = temp.path().join("reader-ready");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows::cloud::tests::native_reader",
                "--ignored",
                "--nocapture",
            ])
            .env("OINK_CLOUD_NATIVE_READ", &paths[0])
            .env("OINK_CLOUD_NATIVE_HOLD", &marker)
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(marker.exists());
        assert_eq!(contents.opens.load(Ordering::Relaxed), 1);
        drop(offer);
        let mut consumer_open = false;
        for _ in 0..50 {
            consumer_open = service
                .deliveries
                .lock()
                .unwrap()
                .values()
                .any(|d| d.opens.iter().any(|opens| opens.load(Ordering::Acquire) > 0));
            if consumer_open && service.requests.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(consumer_open, "the native file-open notification must be observed");
        assert!(
            service.requests.lock().unwrap().is_empty(),
            "hydration must finish before checking the consumer lease"
        );
        service.sweep(Instant::now() + Duration::from_secs(cloud_files::RETENTION_SECS));
        assert!(paths[0].exists(), "native consumer handle must protect hydrated paths");
        std::fs::write(marker.with_extension("done"), b"close").unwrap();
        assert!(child.wait().unwrap().success());
        service.maintain();
        assert!(
            paths[0].exists(),
            "closing a read must not break later by-reference upload"
        );
        let again = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows::cloud::tests::native_reader",
                "--ignored",
                "--nocapture",
            ])
            .env("OINK_CLOUD_NATIVE_READ", &paths[0])
            .env("OINK_CLOUD_NATIVE_HOLD", &marker)
            .status()
            .unwrap();
        assert!(again.success());
        assert_eq!(contents.opens.load(Ordering::Relaxed), 1, "reopen uses hydrated bytes");
        service.sweep(Instant::now() + Duration::from_secs(cloud_files::RETENTION_SECS));
        assert!(
            paths[0].exists(),
            "application-read materialized files survive cache expiry"
        );
        drop(service);
        assert!(root.exists(), "normal shutdown preserves the native root");
        let restarted = initialize(temp.path().to_owned()).unwrap();
        assert_eq!(std::fs::read(&paths[0]).unwrap().len(), contents.bytes.len());
        drop(restarted);
        cleanup_native_test_root(&root);
    }
}
