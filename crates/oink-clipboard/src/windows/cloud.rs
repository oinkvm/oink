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

mod moved;
pub(crate) mod native;
mod population;
mod protection;
mod table;
mod withdrawal;
use table::DeliveredFile;
mod completion;
mod received;
pub(crate) use received::{open_received, received_usage};
type Hydrate = unsafe extern "system" fn(HANDLE, i64, i64, i32, *mut windows_sys::Win32::System::IO::OVERLAPPED) -> i32;
type Pin = unsafe extern "system" fn(HANDLE, i32, i32, *mut windows_sys::Win32::System::IO::OVERLAPPED) -> i32;
type Ranges = unsafe extern "system" fn(HANDLE, i32, i64, i64, *mut c_void, u32, *mut u32) -> i32;
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
type Update = unsafe extern "system" fn(
    HANDLE,
    *const CF_FS_METADATA,
    *const c_void,
    u32,
    *const CF_FILE_RANGE,
    u32,
    i32,
    *mut i64,
    *mut windows_sys::Win32::System::IO::OVERLAPPED,
) -> i32;
type Execute = unsafe extern "system" fn(*const CF_OPERATION_INFO, *mut CF_OPERATION_PARAMETERS) -> i32;
type Progress = unsafe extern "system" fn(CF_CONNECTION_KEY, i64, i64, i64) -> i32;
type Platform = unsafe extern "system" fn(*mut CF_PLATFORM_INFO) -> i32;
type OpenProtected = unsafe extern "system" fn(*const u16, i32, *mut HANDLE) -> i32;
type ReferenceProtected = unsafe extern "system" fn(HANDLE) -> bool;
type ProtectedHandle = unsafe extern "system" fn(HANDLE) -> HANDLE;
type CloseProtected = unsafe extern "system" fn(HANDLE);
struct Api {
    open_protected: OpenProtected,
    reference_protected: ReferenceProtected,
    win32_protected: ProtectedHandle,
    release_protected: CloseProtected,
    close_protected: CloseProtected,
    hydrate: Hydrate,
    register: Register,
    connect: Connect,
    disconnect: Disconnect,
    #[allow(dead_code)]
    unregister: Unregister,
    pin: Pin,
    info: Info,
    ranges: Ranges,
    create: Create,
    update: Update,
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

/// Numeric errors also go to the ordinary text log: the diagnostic ring can
/// evict a failed Explorer hydration before a user exports it. Codes at API
/// boundaries retain their original HRESULT; IO boundaries carry Win32 codes.
#[derive(Clone, Copy, Default)]
struct HydrationTrace {
    // None keeps unrelated retirement probes quiet; Some(0) is proactive population.
    request: Option<u64>,
    index: usize,
}
impl HydrationTrace {
    fn failed(&self, stage: u8, code: u32, error: &io::Error) {
        let Some(request) = self.request else { return };
        tracing::warn!(target: "oink_file_diagnostics", file_stage = stage,
            file_request = request, file_index = self.index, file_code = code,
            file_reason = crate::content::reason(error) as u8,
            "cloud file hydration step failed");
    }
    fn io<T>(&self, stage: u8, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            self.failed(stage, error.raw_os_error().unwrap_or(0) as u32, error);
        }
        result
    }
    fn hr(&self, stage: u8, code: i32) -> io::Result<()> {
        let result = hr(code);
        if let Err(error) = &result {
            self.failed(stage, code as u32, error);
        }
        result
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
            // Safe FULL-policy size changes require this platform opt-in.
            // Older Cloud Files integrations cannot implement our link contract.
            if info.IntegrationNumber < 0x500 {
                return Err(ERROR_NOT_SUPPORTED as i32);
            }
            Ok(Arc::new(Api {
                open_protected: function!("CfOpenFileWithOplock", OpenProtected),
                reference_protected: function!("CfReferenceProtectedHandle", ReferenceProtected),
                win32_protected: function!("CfGetWin32HandleFromProtectedHandle", ProtectedHandle),
                release_protected: function!("CfReleaseProtectedHandle", CloseProtected),
                close_protected: function!("CfCloseHandle", CloseProtected),
                hydrate: function!("CfHydratePlaceholder", Hydrate),
                pin: function!("CfSetPinState", Pin),
                info: function!("CfGetPlaceholderInfo", Info),
                ranges: function!("CfGetPlaceholderRangeInfo", Ranges),
                register: function!("CfRegisterSyncRoot", Register),
                connect: function!("CfConnectSyncRoot", Connect),
                disconnect: function!("CfDisconnectSyncRoot", Disconnect),
                unregister: function!("CfUnregisterSyncRoot", Unregister),
                create: function!("CfCreatePlaceholders", Create),
                update: function!("CfUpdatePlaceholder", Update),
                execute: function!("CfExecute", Execute),
                progress: function!("CfReportProviderProgress", Progress),
            }))
        })
        .clone()
        .map_err(io::Error::from_raw_os_error)
    }
}
static CURRENT: Mutex<Weak<Service>> = Mutex::new(Weak::new());

pub(crate) fn upgrade_directory(path: &Path) -> io::Result<bool> {
    received::valid_root(path)
}

fn prepare_upgrade(root: &Path) -> io::Result<()> {
    let cache = root.parent().ok_or(io::ErrorKind::InvalidInput)?;
    crate::upgrade::prepare(cache, || {
        match std::fs::symlink_metadata(root) {
            Ok(_) => crate::upgrade::clear_directory(root)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        crate::upgrade::remove(&root.with_extension("received"))
    })
}

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
            // non-NTFS volume or failed root registration). Remote file
            // publication reports unavailable before exposing any representation.
            tracing::warn!(
                reason = "platform_unavailable",
                code = error.raw_os_error(),
                "cloud file provider unavailable"
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
    cancelled: AtomicBool,
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
            cancelled: AtomicBool::new(false),
            virtual_files: cloud_files::formats(
                inventory.map_or(oink_foundation::clipboard::ClipPlatform::Unknown, |i| i.platform),
                inventory.is_some_and(|i| i.items.iter().any(|item| item.file.is_some() && item.virtual_files)),
                true,
            )
            .virtual_files,
        })
    }
    pub fn hdrop(&self) -> io::Result<Vec<u8>> {
        if let Some(namespace) = self.contents.namespace() {
            namespace.prepare()?;
        }
        let provisional = self
            .contents
            .namespace()
            .and_then(|namespace| namespace.source_link())
            .is_some_and(|source| source.provisional);

        let persistent = self
            .contents
            .namespace()
            .and_then(|files| files.source_link())
            .is_some();
        if self.service.stopping.load(Ordering::Acquire)
            || (!persistent && self.created.elapsed().as_secs() >= cloud_files::RETENTION_SECS)
        {
            return Err(unavailable());
        }
        let mut prepared = self.prepared.lock().unwrap();
        if self.cancelled.load(Ordering::Acquire) {
            return Err(unavailable());
        }
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
        let lazy = persistent && !self.virtual_files;
        let contents: Arc<dyn FileContents> = if lazy {
            let namespace = self.contents.namespace().ok_or(io::ErrorKind::InvalidData)?;
            let nodes = namespace.roots();
            let mut names = crate::content::namespace::Names::new(crate::sync::files::TargetOs::Windows);
            let entries = nodes
                .iter()
                .map(|node| names.entry(node))
                .collect::<io::Result<Vec<_>>>()?;
            crate::content::namespace::Files::restore_table(
                namespace,
                crate::sync::files::TargetOs::Windows,
                nodes,
                entries,
            )?
        } else {
            self.contents.clone()
        };
        let mut entries = contents.descriptor_entries()?.unwrap_or_else(|| self.entries.clone());
        for (index, entry) in entries.iter_mut().enumerate().filter(|(_, e)| !e.is_dir) {
            let known = contents.descriptor_size_known(index);
            // Empty placeholders never issue FETCH_DATA, so resolve their
            // selected metadata now rather than silently publishing stale zero.
            if !known || entry.size == 0 {
                let metadata = match contents.placeholder_metadata(index)? {
                    Some((metadata, _)) => metadata,
                    None if known => continue, // a flat provider's immutable known length
                    None => {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "cloud file size unknown"));
                    }
                };
                entry.size = metadata.size;
                entry.modified_ms = metadata.modified_ms;
            }
            if entry.size == 0 && !provisional {
                // Zero-byte placeholders never fetch. Verify the remote EOF now
                // so a stale/incorrect zero Stat cannot become a successful empty
                // file while the shared delivery later reports a stream mismatch.
                let version = contents.placeholder_metadata(index)?.map_or(0, |(_, version)| version);
                let mut reader = contents.open_placeholder(index, version)?;
                if reader.read(&mut [0u8; 1])? != 0 {
                    return Err(crate::content::unavailable(crate::model::Unavailable::Superseded));
                }
            }
        }
        let delivery = self.service.publish_inner(entries, contents, self.created, lazy)?;
        if delivery.expired() {
            self.service.cancel_received(delivery.id);
            return Err(unavailable());
        }
        let bytes = super::files::build_hdrop(&delivery.roots);
        *prepared = Some(delivery);
        Ok(bytes)
    }
    pub(super) fn cancel(&self) {
        let delivery = {
            let prepared = self.prepared.lock().unwrap();
            self.cancelled.store(true, Ordering::Release);
            prepared.clone()
        };
        if let Some(delivery) = delivery {
            for file in delivery.files() {
                file.deleted.store(true, Ordering::Release);
            }
            self.service.cancel_received(delivery.id);
        }
    }
}

struct Delivery {
    service: Weak<Service>,
    id: u64,
    path: PathBuf,
    native_paths: Mutex<Vec<PathBuf>>,
    roots: Vec<PathBuf>,
    files: Mutex<Vec<Arc<DeliveredFile>>>,
    population: Mutex<()>,
    population_requests: Mutex<HashMap<usize, Vec<Weak<population::Pending>>>>,
    download: OnceLock<native::DownloadResult>,
    downloading: AtomicBool,
    contents: Mutex<Arc<dyn FileContents>>,
    created: Instant,
    readers: Arc<AtomicUsize>,
}
impl Delivery {
    fn contents(&self) -> Arc<dyn FileContents> {
        self.contents.lock().unwrap().clone()
    }
    fn file_path(&self, index: usize) -> PathBuf {
        self.native_paths.lock().unwrap()[index].clone()
    }
    fn expired(&self) -> bool {
        if self
            .service
            .upgrade()
            .is_some_and(|service| service.received.lock().unwrap().sources.contains_key(&self.id))
        {
            return false;
        }
        if self
            .contents()
            .namespace()
            .and_then(|files| files.source_link())
            .is_some()
        {
            return false;
        }
        self.created.elapsed().as_secs() >= cloud_files::RETENTION_SECS
    }
    fn active_reads(&self) -> usize {
        self.readers.load(Ordering::Acquire).saturating_add(
            self.files()
                .iter()
                .filter(|file| !file.entry.is_dir)
                .map(|file| file.opens.load(Ordering::Acquire).max(0) as usize)
                .sum::<usize>(),
        )
    }
    fn consumed(&self) -> bool {
        let files = self.files();
        files.iter().any(|file| !file.entry.is_dir)
            && files
                .iter()
                .filter(|file| !file.entry.is_dir)
                .all(|file| file.deleted.load(Ordering::Acquire))
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
    _pin: native::ReaderPin,
    service: Arc<Service>,
    delivery: Arc<Delivery>,
    index: usize,
    id: u64,
    request: Arc<Request>,
    population: Option<Arc<population::Pending>>,
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
        // Clear old receiver data before parsing its obsolete journal or
        // exposing callbacks. The root itself and its registration survive.
        prepare_upgrade(&root)?;
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
        // PARTIAL population publishes roots first; FETCH_PLACEHOLDERS discovers
        // direct children. No WinRT shell integration, streaming or auto-dehydration.
        let policies = CF_SYNC_POLICIES {
            StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
            Hydration: CF_HYDRATION_POLICY {
                Primary: CF_HYDRATION_POLICY_FULL,
                Modifier: CF_HYDRATION_POLICY_MODIFIER_ALLOW_FULL_RESTART_HYDRATION,
            },
            InSync: CF_INSYNC_POLICY_TRACK_FILE_LAST_WRITE_TIME
                | CF_INSYNC_POLICY_TRACK_FILE_CREATION_TIME
                | CF_INSYNC_POLICY_PRESERVE_INSYNC_FOR_SYNC_ENGINE,
            Population: CF_POPULATION_POLICY {
                Primary: CF_POPULATION_POLICY_PARTIAL,
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
                // The provider creates device/batch containers directly. With
                // PARTIAL population, leaving root enumeration enabled sends a
                // FETCH_PLACEHOLDERS callback with the root's empty identity;
                // our delivery callback rejects it. External Shell path parsing
                // (including zipfldr's post-archive SHFileOperation DELETE) then
                // fails before any delete callback. Disable demand only on the
                // root, including existing registrations; received directories
                // retain their lazy population policy.
                // https://learn.microsoft.com/windows/win32/api/cfapi/nf-cfapi-cfregistersyncroot
                CF_REGISTER_FLAG_DISABLE_ON_DEMAND_POPULATION_ON_ROOT
                    | if existed {
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
                Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
                Callback: Some(population::fetch),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_CANCEL_FETCH_PLACEHOLDERS,
                Callback: Some(cancel),
            },
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
    #[cfg(test)]
    fn publish(
        &self,
        entries: Vec<FileEntry>,
        contents: Arc<dyn FileContents>,
        created: Instant,
    ) -> io::Result<Arc<Delivery>> {
        self.publish_inner(entries, contents, created, false)
    }
    fn publish_inner(
        &self,
        entries: Vec<FileEntry>,
        contents: Arc<dyn FileContents>,
        created: Instant,
        lazy: bool,
    ) -> io::Result<Arc<Delivery>> {
        if entries.is_empty() || entries.len() > crate::model::MAX_DISCOVERED_NODES {
            return Err(io::Error::from_raw_os_error(ERROR_INVALID_DATA as i32));
        }
        if let Some(namespace) = contents.namespace() {
            namespace.prepare()?;
        }
        let persistent = match contents.namespace().and_then(|files| files.source_link()) {
            Some(descriptor) => {
                descriptor.validate()?;
                let nodes = contents.descriptor_nodes()?.ok_or(io::ErrorKind::InvalidData)?;
                if nodes.len() != entries.len() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                Some((descriptor, nodes))
            }
            None => None,
        };
        let provisional = persistent.as_ref().is_some_and(|(source, _)| source.provisional);
        let mut names = HashSet::new();
        let mut root_names = Vec::new();
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
                root_names.push(name);
            }
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
        let path = match crate::received::allocate_batch_with(&source, &crate::received::local_stamp(), |path| {
            self.create_container(path, id)
        }) {
            Ok(path) => path,
            Err(error) => {
                let _ = std::fs::remove_dir(&source);
                return Err(error);
            }
        };
        let roots = root_names.into_iter().map(|name| path.join(name)).collect();

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
                        | if entry.is_dir && !lazy {
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
        crate::cache::changed(&self.root);
        if let Err(error) = result {
            let _mutation = crate::cache::Mutation(&self.root);
            if std::fs::remove_dir_all(&path).is_err() {
                self.abandoned.lock().unwrap().push(path);
            }
            let _ = std::fs::remove_dir(&source);
            return Err(error);
        }
        let delivery = Arc::new(Delivery {
            service: self.context.as_ref().clone(),
            population: Mutex::new(()),
            population_requests: Mutex::new(HashMap::new()),
            download: OnceLock::new(),
            downloading: AtomicBool::new(false),
            id,
            native_paths: Mutex::new(
                entries
                    .iter()
                    .map(|entry| path.join(super::files::descriptor_path(&entry.path)))
                    .collect(),
            ),
            path,
            roots,
            files: Mutex::new(
                entries
                    .into_iter()
                    .map(|entry| Arc::new(DeliveredFile::new(entry, false)))
                    .collect(),
            ),
            contents: Mutex::new(contents),
            created,
            readers: Arc::new(AtomicUsize::new(0)),
        });
        {
            let mut journal = self.received.lock().unwrap();
            if let Some((descriptor, nodes)) = persistent {
                journal.sources.insert(
                    id,
                    received::Source {
                        descriptor: Some(descriptor),
                        settlement: Default::default(),
                        failed: HashSet::new(),
                        pending: if lazy {
                            nodes
                                .iter()
                                .filter(|node| node.kind == crate::model::NodeKind::Directory)
                                .map(|node| node.id)
                                .collect()
                        } else {
                            HashSet::new()
                        },
                        listed: HashSet::new(),
                        nodes,
                        base: delivery.path.strip_prefix(&self.root).map_err(io::Error::other)?.into(),
                    },
                );
            }
            journal.deliveries.insert(
                id,
                delivery
                    .entries()
                    .iter()
                    .enumerate()
                    .map(|(i, e)| received::Node {
                        path: delivery.file_path(i).strip_prefix(&self.root).unwrap().into(),
                        life: crate::received::Item::new(crate::received::now()),
                        directory: e.is_dir,
                        deleted: false,
                        withdrawn: false,
                        local_final: false,
                        renamed_locally: false,
                        handed_off: e.size == 0 && !provisional,
                        length: e.size,
                    })
                    .collect(),
            );
            if journal.pending_gaps.remove(&id) {
                for node in journal.deliveries.get_mut(&id).unwrap() {
                    node.life.gap(crate::received::now());
                }
            }
            let saved = journal.save(&self.root);
            drop(journal);
            if let Err(error) = saved {
                self.cancel_received(id);
                return Err(error);
            }
        }
        self.deliveries.lock().unwrap().insert(id, delivery.clone());
        if let Err(error) = native::bind(&delivery) {
            self.cancel_received(id);
            return Err(error);
        }
        for (index, entry) in delivery
            .entries()
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.is_dir && e.size == 0 && !provisional)
        {
            self.native_complete(id, index, entry.size);
        }
        tracing::info!(target: "oink_file_diagnostics", file_stage = 1_u8,
            count = delivery.entries().len(), "cloud file placeholders published");
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
        self.abandoned.lock().unwrap().retain(|path| {
            let _mutation = crate::cache::Mutation(&self.root);
            match std::fs::remove_dir_all(path) {
                Ok(()) => false,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(_) => true,
            }
        });
        let mut protected: std::collections::HashSet<_> = self
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
        protected.extend(self.received.lock().unwrap().sources.keys().copied());
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
    trace: HydrationTrace,
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
            file_request = trace.request.unwrap_or(0), file_index = trace.index,
            file_code = code as u32, file_status = status as u32, bytes = length, "cloud file native transfer failed");
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
            HydrationTrace::default(),
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
        !d.expired()
            && d.get_file(index).is_some_and(|file| !file.entry.is_dir)
            && !d.file(index).deleted.load(Ordering::Acquire)
    }) else {
        reject();
        return;
    };
    let Ok(pin) = native::ReaderPin::new(&delivery) else {
        reject();
        return;
    };
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
        _pin: pin,
        service: service.clone(),
        delivery,
        index,
        id: request_id,
        request,
        population: None,
    };
    // Unbounded queue, bounded data workers: native callback threads remain free
    // to deliver cancellation while an application waits for network content.
    if service.jobs.send(work).is_err() {
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
            .filter(|d| d.get_file(index).is_some_and(|file| !file.entry.is_dir))
        else {
            return;
        };
        // Signed balance tolerates thread-pool delivery of a close before its
        // paired open. Only positive balances defer expiry; closing does not
        // imply that a by-reference consumer has released the path.
        if open {
            delivery.file(index).opens.fetch_add(1, Ordering::AcqRel);
        } else {
            delivery.file(index).opens.fetch_sub(1, Ordering::AcqRel);
            if deleted {
                delivery.file(index).deleted.store(true, Ordering::Release);
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
        // placeholder or independent received-file consumer may need the same bytes.
    }
}
impl Work {
    fn trace(&self) -> HydrationTrace {
        HydrationTrace {
            request: Some(self.id),
            index: self.index,
        }
    }
    fn run(self) {
        let mutation_root = self.service.root.clone();
        let _mutation = crate::cache::Mutation(&mutation_root);
        if self.population.is_some() {
            self.populate();
            return;
        }
        let key = *self.service.connection.lock().unwrap();
        if let Some(key) = key {
            let op = operation(key, self.request.transfer, self.request.key);
            let file = self.delivery.file(self.index);
            let mut selected = file.selected.lock().unwrap();
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
                    self.trace(),
                );
                let _ = self.delivery.fail_received(self.index);
                if crate::content::reason(&error) == crate::model::Unavailable::NoSuchItem {
                    self.delivery.source_withdrawn(&self.service, self.index);
                }
                tracing::warn!(target: "oink_file_diagnostics", file_stage = 11_u8,
                    file_request = self.id, file_index = self.index,
                    file_code = status as u32, file_reason = crate::content::reason(&error) as u8,
                    bytes = self.request.progress.load(Ordering::Relaxed),
                    "cloud file hydration failed");
            }
        }
        self.service.requests.lock().unwrap().remove(&self.id);
    }
    fn update_placeholder(&self, op: &CF_OPERATION_INFO, metadata: FileMetadata) -> io::Result<()> {
        // Metadata changes arrive while a user read is pending. In Windows 11
        // probes CfUpdatePlaceholder blocked when resizing from FETCH_DATA.
        // RESTART_HYDRATION is the documented FULL-policy metadata update for
        // this situation. Registration requires ALLOW_FULL_RESTART_HYDRATION
        // so AV memory-mapped scans cannot deadlock a size change. It invalidates
        // partial old bytes, changes size/mtime,
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
        self.trace().hr(68, code)
    }

    fn hydrate(&self, op: &CF_OPERATION_INFO, selected: &mut (FileMetadata, Option<u64>)) -> io::Result<()> {
        if self.request.cancelled.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        if self.delivery.file(self.index).deleted.load(Ordering::Acquire) {
            return Err(crate::content::unavailable(crate::model::Unavailable::NoSuchItem));
        }
        let file = self.delivery.file(self.index);
        if file.withdrawn.load(Ordering::Acquire) || file.local_final.load(Ordering::Acquire) {
            return Err(crate::content::unavailable(crate::model::Unavailable::Superseded));
        }
        tracing::info!(target: "oink_file_diagnostics",
            file_stage = 5_u8, file_request = self.id, file_index = self.index,
            "cloud file selecting source metadata");
        let current = self
            .trace()
            .io(60, self.delivery.contents().placeholder_metadata(self.index))?
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
        // Publishing size/timestamps does not select a source version. Binding
        // the first version with unchanged metadata must serve the pending IO,
        // rather than invalidate it with RESTART_HYDRATION. A known version,
        // including zero for flat providers, still detects later replacements.
        let changed = selected.0 != metadata || selected.1.is_some_and(|old| old != version);
        // Even an explicit fetch of an empty stream must pass exact-version
        // EOF before updating native metadata. An EOF version error must not
        // expose an empty file as successful hydration.
        if metadata.size == 0 {
            let mut reader = self
                .trace()
                .io(61, self.delivery.contents().open_placeholder(self.index, version))?;
            if self.trace().io(63, reader.read(&mut [0u8; 1]))? != 0 {
                return Err(io::Error::from_raw_os_error(ERROR_INVALID_DATA as i32));
            }
            if self.request.cancelled.load(Ordering::Acquire)
                || self.delivery.file(self.index).deleted.load(Ordering::Acquire)
            {
                return Err(unavailable());
            }
            let _mutation = self.mutation(selected.0.size)?;
            if changed {
                self.update_placeholder(op, metadata)?;
            }
            *selected = (metadata, Some(version));
            self.service.native_complete(self.delivery.id, self.index, 0);
            return Ok(());
        }
        if changed {
            let _mutation = self.mutation(selected.0.size)?;
            self.update_placeholder(op, metadata)?;
            *selected = (metadata, Some(version));
            return Ok(()); // cfapi reissues IO against the updated placeholder
        }
        *selected = (metadata, Some(version));
        tracing::info!(target: "oink_file_diagnostics", file_stage = 9_u8,
            file_request = self.id, file_index = self.index, size = metadata.size,
            "cloud file source stream opening");
        let mut reader = self
            .trace()
            .io(61, self.delivery.contents().open_placeholder(self.index, version))?;
        let mut buffer = vec![0u8; 64 * 1024];
        let mut offset = 0u64;
        while offset < metadata.size {
            if self.request.cancelled.load(Ordering::Acquire)
                || self.delivery.file(self.index).deleted.load(Ordering::Acquire)
            {
                return Err(unavailable());
            }
            let count = buffer.len().min((metadata.size - offset) as usize);
            self.trace().io(62, reader.read_exact(&mut buffer[..count]))?;
            // Verify EOF before making the final range visible. This invokes the
            // existing remote stream's selected-version/integrity completion check.
            if offset + count as u64 == metadata.size && self.trace().io(63, reader.read(&mut [0u8; 1]))? != 0 {
                return Err(io::Error::from_raw_os_error(ERROR_INVALID_DATA as i32));
            }
            if self.request.cancelled.load(Ordering::Acquire)
                || self.delivery.file(self.index).deleted.load(Ordering::Acquire)
            {
                return Err(unavailable());
            }
            if offset == 0 {
                tracing::info!(target: "oink_file_diagnostics", file_stage = 10_u8,
                    file_request = self.id, file_index = self.index, bytes = count,
                    "cloud file first source bytes verified");
            }
            let mutation = self.mutation(metadata.size)?;
            if offset == 0 {
                tracing::info!(target: "oink_file_diagnostics", file_stage = 11_u8,
                    file_request = self.id, file_index = self.index,
                    "cloud file mutation protection acquired");
            }
            self.trace().io(
                69,
                transfer(
                    &self.service.api,
                    op,
                    offset as i64,
                    count as i64,
                    buffer.as_ptr().cast(),
                    STATUS_SUCCESS,
                    self.trace(),
                ),
            )?;
            drop(mutation);
            offset += count as u64;
            self.request.progress.store(offset, Ordering::Relaxed);
            self.delivery
                .file(self.index)
                .hydrated
                .fetch_add(count as u64, Ordering::Release);
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
    mod shell_ops;
    mod zip_drop;
    use super::*;
    include!("cloud/handle_leak_tests.rs");

    fn one_shot_delivery(service: &Arc<Service>) -> Arc<Delivery> {
        use crate::content::{
            Provider,
            grants::{FulfilledRoot, LinkToken},
            linked::{Connector, Descriptor},
            namespace::{Files, Names},
        };
        use crate::model::{ContentSize, FileNode, NodeId, NodeKind};
        let node = FileNode {
            id: NodeId(1),
            parent: None,
            name: "body".into(),
            kind: NodeKind::File,
            size: ContentSize::Known(3),
            version: 1,
            modified_ms: None,
            exec_bits: None,
        };
        let descriptor = Descriptor {
            offer: crate::content::tests::fixture().inventory().id,
            token: LinkToken([8; 32]),
            provisional: false,
            roots: vec![node.clone()],
            fulfilled: vec![FulfilledRoot {
                node: node.id,
                id: [2; 16],
            }],
            native: Default::default(),
        };
        let mut names = Names::new(crate::sync::files::TargetOs::Windows);
        let entries = vec![names.entry(&node).unwrap()];
        let namespace = Connector::default().restore(descriptor).unwrap();
        let files =
            Files::restore_table(namespace, crate::sync::files::TargetOs::Windows, vec![node], entries).unwrap();
        service
            .publish(files.descriptor_entries().unwrap().unwrap(), files, Instant::now())
            .unwrap()
    }
    #[test]
    fn build72_pending_receipt_recovery_keeps_complete_bytes_and_cleans_missing_bytes() {
        let _serial = SERIAL.lock().unwrap();
        for damage in ["none", "missing-handoff", "short-handoff"] {
            let missing = damage != "none";
            let temp = tempfile::tempdir().unwrap();
            let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
            let delivery = one_shot_delivery(&service);
            let id = delivery.id;
            let path = delivery.file_path(0);
            std::fs::write(&path, if missing { &b"a"[..] } else { &b"abc"[..] }).unwrap();
            let previous = service.received.lock().unwrap();
            let mut fixture: serde_json::Value =
                serde_json::from_str(include_str!("../../tests/fixtures/build72-pending-windows.json")).unwrap();
            fixture["deliveries"]["1"][0]["path"] = serde_json::json!(previous.deliveries[&delivery.id][0].path);
            fixture["sources"]["1"]["base"] = serde_json::json!(previous.sources[&delivery.id].base);
            if damage == "missing-handoff" {
                fixture["deliveries"]["1"][0]["handed_off"] = false.into();
            }
            if damage == "short-handoff" {
                fixture["deliveries"]["1"][0]["length"] = 1.into();
            }
            for table in ["deliveries", "sources"] {
                let table = fixture[table].as_object_mut().unwrap();
                let old = table.remove("1").unwrap();
                table.insert(id.to_string(), old);
            }
            drop(previous);
            std::fs::write(
                service.root.with_extension("received"),
                serde_json::to_vec(&fixture).unwrap(),
            )
            .unwrap();
            *service.received.lock().unwrap() = received::Journal::load(&service.root).unwrap();
            service.deliveries.lock().unwrap().clear();
            drop(delivery);
            service.recover_received().unwrap();
            let live = service.deliveries.lock().unwrap().get(&id).cloned();
            assert!(live.is_some(), "journal recovery did not bind the native delivery");
            service.maintain();
            let (source, nodes, saved) = {
                let journal = service.received.lock().unwrap();
                (
                    journal.sources[&id].clone(),
                    journal.deliveries[&id].clone(),
                    serde_json::to_string(&*journal).unwrap(),
                )
            };
            if missing {
                assert!(source.failed.contains(&crate::model::NodeId(1)));
                assert!(!path.exists(), "failed partial must be cleaned");
            } else {
                assert!(
                    source.descriptor.is_none(),
                    "old complete receive must become FinalLocal"
                );
                assert!(source.settlement.contains(crate::model::NodeId(1)));
                assert!(!nodes[0].withdrawn);
                assert_eq!(std::fs::read(&path).unwrap(), b"abc");
                assert!(!saved.contains("digest"));
                let again = received::Journal::load(&service.root).unwrap();
                assert!(again.sources[&id].descriptor.is_none());
            }
        }
    }

    #[test]
    fn one_shot_arrival_finishes_without_residency_pin_acceptance_or_source() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        unsafe extern "system" fn denied_pin(
            _: HANDLE,
            _: i32,
            _: i32,
            _: *mut windows_sys::Win32::System::IO::OVERLAPPED,
        ) -> i32 {
            0x80070005u32 as i32
        }
        let (service, _) = fake_with_pin(temp.path().join("CloudFiles"), owned_info, denied_pin);
        let delivery = one_shot_delivery(&service);
        // This fake domain has no receiver payload or connected sender: only
        // the successful transfer callback can complete its F1 lifecycle.
        service.native_complete(delivery.id, 0, 3);
        assert!(delivery.contents().namespace().unwrap().source_link().is_none());
        assert!(
            service.received.lock().unwrap().sources[&delivery.id]
                .settlement
                .contains(crate::model::NodeId(1))
        );
        let journal = received::Journal::load(&service.root).unwrap();
        assert!(journal.sources[&delivery.id].descriptor.is_none());
        assert!(
            !String::from_utf8(serde_json::to_vec(&journal).unwrap())
                .unwrap()
                .contains("pending_receipts")
        );
    }
    #[test]
    fn one_shot_restart_and_live_failure_mark_whole_receive_failed() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let delivery = one_shot_delivery(&service);
        let journal = received::Journal::load(&service.root).unwrap();
        assert!(journal.sources[&delivery.id].failed.contains(&crate::model::NodeId(1)));
        assert!(journal.deliveries[&delivery.id][0].withdrawn);
        delivery.fail_received(0).unwrap();
        assert!(
            service.received.lock().unwrap().sources[&delivery.id]
                .failed
                .contains(&crate::model::NodeId(1))
        );
        assert!(delivery.file(0).withdrawn.load(Ordering::Acquire));
    }
    #[test]
    fn upgrade_reuses_cloudfiles_root_and_removes_old_items_and_journal_once() {
        let cache = tempfile::tempdir().unwrap();
        let root = cache.path().join("CloudFiles");
        std::fs::create_dir_all(root.join("old")).unwrap();
        std::fs::write(root.join("old/received"), b"old received content").unwrap();
        std::fs::write(root.with_extension("received"), b"unreadable old journal").unwrap();
        // Denying delete sharing proves the transaction keeps this directory;
        // no Cloud Files registration API is used by this private fixture.
        use std::os::windows::fs::OpenOptionsExt;
        let _root = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&root)
            .unwrap();
        prepare_upgrade(&root).unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        assert!(!root.with_extension("received").exists());
        assert!(!cache.path().join("CloudLinkedFiles").exists());
        std::fs::write(root.join("new-link"), b"new").unwrap();
        prepare_upgrade(&root).unwrap();
        assert_eq!(std::fs::read(root.join("new-link")).unwrap(), b"new");
    }

    #[test]
    fn upgrade_waits_for_a_busy_old_item_and_resumes_without_reading_old_journal() {
        use std::os::windows::fs::OpenOptionsExt;
        let cache = tempfile::tempdir().unwrap();
        let root = cache.path().join("CloudFiles");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("busy"), b"old").unwrap();
        std::fs::write(root.with_extension("received"), b"not a journal").unwrap();
        let busy = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(root.join("busy"))
            .unwrap();
        assert!(prepare_upgrade(&root).is_err());
        assert!(root.with_extension("received").exists());
        drop(busy);
        prepare_upgrade(&root).unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        assert!(!root.with_extension("received").exists());
    }
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::atomic::AtomicI32;
    static SERIAL: Mutex<()> = Mutex::new(());
    static FAIL_AFTER: AtomicI32 = AtomicI32::new(-1);
    static PROGRESS_SERVICE: Mutex<Weak<Service>> = Mutex::new(Weak::new());
    static PROGRESS_UNLOCKED: AtomicBool = AtomicBool::new(false);
    static UPDATES: Mutex<Vec<(u64, i64)>> = Mutex::new(Vec::new());
    static TRANSFERS: Mutex<Vec<(i64, Vec<u8>, NTSTATUS)>> = Mutex::new(Vec::new());
    static KEYS: Mutex<Vec<(i64, PathBuf)>> = Mutex::new(Vec::new());
    static POPULATIONS: Mutex<Vec<NTSTATUS>> = Mutex::new(Vec::new());
    static IDENTITIES: Mutex<Vec<(PathBuf, [u8; 16])>> = Mutex::new(Vec::new());
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
        entries: *mut CF_PLACEHOLDER_CREATE_INFO,
        length: u32,
        flags: i32,
        processed: *mut u32,
    ) -> i32 {
        // SAFETY: caller retains the array and writable count through this call.
        unsafe {
            *processed = 0;
            for index in 0..length {
                let mut count = 0;
                let code = create_one(parent, entries.add(index as usize), 1, flags, &mut count);
                *processed += count;
                if code < 0 {
                    return code;
                }
            }
        }
        0
    }
    unsafe extern "system" fn create_one(
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
                std::fs::create_dir(&path).unwrap();
            } else {
                std::fs::File::create(&path)
                    .unwrap()
                    .set_len(info.FsMetadata.FileSize as u64)
                    .unwrap();
            }
            IDENTITIES.lock().unwrap().push((
                path.canonicalize().unwrap(),
                std::slice::from_raw_parts(info.FileIdentity.cast::<u8>(), 16)
                    .try_into()
                    .unwrap(),
            ));
            info.Result = 0;
            *count = 1;
        }
        0
    }
    unsafe extern "system" fn execute(op: *const CF_OPERATION_INFO, params: *mut CF_OPERATION_PARAMETERS) -> i32 {
        // SAFETY: production supplies the union arm selected by Type; all input
        // metadata and buffers remain live for these synchronous fake calls.
        unsafe {
            if (*op).Type == CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS {
                let params = &mut (*params).Anonymous.TransferPlaceholders;
                assert_eq!(params.PlaceholderArray.is_null(), params.PlaceholderCount == 0);
                POPULATIONS.lock().unwrap().push(params.CompletionStatus);
                if params.CompletionStatus != STATUS_SUCCESS {
                    return 0;
                }
                let path = KEYS
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(key, _)| *key == (*op).TransferKey)
                    .unwrap()
                    .1
                    .clone();
                params.EntriesProcessed = 0;
                for index in 0..params.PlaceholderCount as usize {
                    let info = params.PlaceholderArray.add(index);
                    if (*info).FsMetadata.BasicInfo.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                        assert_eq!(
                            (*info).Flags & CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION,
                            0
                        );
                    }
                    let mut count = 0;
                    let code = create(wide(&path).as_ptr(), info, 1, 0, &mut count);
                    params.EntriesProcessed += count;
                    if code < 0 {
                        return code;
                    }
                }
                return 0;
            }
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
    unsafe extern "system" fn fake_info(
        handle: HANDLE,
        kind: i32,
        buffer: *mut c_void,
        size: u32,
        written: *mut u32,
    ) -> i32 {
        // SAFETY: forwarding bounded fixture buffers; no native Cloud Files call.
        unsafe {
            let code = owned_info(handle, kind, buffer, size, written);
            if code >= 0 {
                (*buffer.cast::<CF_PLACEHOLDER_STANDARD_INFO>()).OnDiskDataSize = 0;
            }
            code
        }
    }
    fn fake(root: PathBuf) -> (Arc<Service>, mpsc::Receiver<Work>) {
        fake_with_info(root, fake_info)
    }
    unsafe extern "system" fn fake_update(
        _: HANDLE,
        _: *const CF_FS_METADATA,
        _: *const c_void,
        _: u32,
        _: *const CF_FILE_RANGE,
        _: u32,
        _: i32,
        _: *mut i64,
        _: *mut windows_sys::Win32::System::IO::OVERLAPPED,
    ) -> i32 {
        0
    }
    fn fake_with_info(root: PathBuf, info: Info) -> (Arc<Service>, mpsc::Receiver<Work>) {
        fake_with_pin(root, info, fake_pin)
    }
    fn fake_with_pin(root: PathBuf, info: Info, pin: Pin) -> (Arc<Service>, mpsc::Receiver<Work>) {
        unsafe extern "system" fn hydrate(
            _: HANDLE,
            _: i64,
            _: i64,
            _: i32,
            _: *mut windows_sys::Win32::System::IO::OVERLAPPED,
        ) -> i32 {
            0
        }
        fake_with_hydrate(root, info, hydrate, pin)
    }
    fn fake_with_hydrate(
        root: PathBuf,
        info: Info,
        hydrate: Hydrate,
        pin: Pin,
    ) -> (Arc<Service>, mpsc::Receiver<Work>) {
        FAIL_AFTER.store(-1, Ordering::Relaxed);
        TRANSFERS.lock().unwrap().clear();
        UPDATES.lock().unwrap().clear();
        POPULATIONS.lock().unwrap().clear();
        std::fs::create_dir(&root).unwrap();
        unsafe extern "system" fn ranges(
            _: HANDLE,
            _: i32,
            start: i64,
            length: i64,
            buffer: *mut c_void,
            _: u32,
            written: *mut u32,
        ) -> i32 {
            // SAFETY: fixture-owned output covers one complete resident range.
            unsafe {
                *buffer.cast::<CF_FILE_RANGE>() = CF_FILE_RANGE {
                    StartingOffset: start,
                    Length: length,
                };
                *written = size_of::<CF_FILE_RANGE>() as u32;
            }
            0
        }
        // Private filesystem fake: models ownership/share exclusion, not CF
        // kernel oplock/mapping behavior (which remains native acceptance).
        unsafe extern "system" fn open_protected(path: *const u16, flags: i32, out: *mut HANDLE) -> i32 {
            // SAFETY: paths/output belong to the synchronous private fixture.
            unsafe {
                *out = CreateFileW(
                    path,
                    GENERIC_READ
                        | if flags & CF_OPEN_FILE_FLAG_WRITE_ACCESS != 0 {
                            GENERIC_WRITE
                        } else {
                            0
                        }
                        | if flags & CF_OPEN_FILE_FLAG_DELETE_ACCESS != 0 {
                            DELETE
                        } else {
                            0
                        },
                    if flags & CF_OPEN_FILE_FLAG_EXCLUSIVE != 0 {
                        0
                    } else {
                        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
                    },
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                    std::ptr::null_mut(),
                );
                if *out == INVALID_HANDLE_VALUE {
                    (0x80070000u32 | GetLastError()) as i32
                } else {
                    0
                }
            }
        }
        unsafe extern "system" fn reference_protected(_: HANDLE) -> bool {
            true
        }
        unsafe extern "system" fn win32_protected(handle: HANDLE) -> HANDLE {
            handle
        }
        unsafe extern "system" fn release_protected(_: HANDLE) {}
        unsafe extern "system" fn close_protected(handle: HANDLE) {
            // SAFETY: balance the fixture's CreateFileW.
            unsafe {
                CloseHandle(handle);
            }
        }
        let api = Arc::new(Api {
            open_protected,
            reference_protected,
            win32_protected,
            release_protected,
            close_protected,
            ranges,
            hydrate,
            pin,
            info,
            register,
            connect,
            disconnect,
            unregister,
            create,
            update: fake_update,
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
    unsafe extern "system" fn owned_info(handle: HANDLE, _: i32, buffer: *mut c_void, _: u32, _: *mut u32) -> i32 {
        let mut path = [0u16; 32768];
        // SAFETY: all buffers and the metadata handle belong to this fixture.
        unsafe {
            let count = GetFinalPathNameByHandleW(handle, path.as_mut_ptr(), path.len() as u32, 0);
            if count == 0 || count as usize >= path.len() {
                return 0x80070178u32 as i32;
            }
            let path = PathBuf::from(String::from_utf16_lossy(&path[..count as usize]));
            let Some(identity) = IDENTITIES
                .lock()
                .unwrap()
                .iter()
                .find(|(p, _)| p == &path)
                .map(|(_, i)| *i)
            else {
                return 0x80070178u32 as i32;
            };
            let info = &mut *buffer.cast::<CF_PLACEHOLDER_STANDARD_INFO>();
            info.FileIdentityLength = 16;
            info.InSyncState = CF_IN_SYNC_STATE_IN_SYNC;
            info.OnDiskDataSize = std::fs::metadata(&path).map_or(0, |m| m.len() as i64);
            std::ptr::copy_nonoverlapping(identity.as_ptr(), info.FileIdentity.as_mut_ptr(), 16);
        }
        0
    }
    #[test]
    fn failed_async_drop_removes_even_hydrated_payload_and_empty_parents() {
        drop_cleanup_fixture(1, false, false, 3);
    }

    #[test]
    fn received_package_path_uses_native_reader_and_allows_provider_writes() {
        use crate::content::{
            Provider,
            linked::{Connector, Descriptor},
            namespace::Files,
        };
        use crate::model::{ContentSize, FileNode, NodeId, NodeKind};
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudLinkedFiles"), owned_info);
        let descriptor = Descriptor {
            native: Default::default(),
            provisional: false,
            fulfilled: vec![],
            offer: crate::content::tests::fixture().inventory().id,
            token: crate::content::grants::LinkToken([9; 32]),
            roots: vec![FileNode {
                exec_bits: None,
                id: NodeId(7),
                parent: None,
                name: "Oink.msi".into(),
                kind: NodeKind::File,
                size: ContentSize::Known(3),
                version: 1,
                modified_ms: None,
            }],
        };
        let contents = Files::new(
            Connector::default().restore(descriptor).unwrap(),
            crate::sync::files::TargetOs::Windows,
        )
        .unwrap();
        let delivery = service
            .publish(
                contents.descriptor_entries().unwrap().unwrap(),
                contents,
                Instant::now(),
            )
            .unwrap();
        let path = delivery.file_path(0);
        std::fs::write(&path, b"a").unwrap();
        let mut reader = native::open_path_in(&service, &path).unwrap().unwrap();
        assert_eq!(delivery.readers.load(Ordering::Acquire), 1);
        // The old upgrade spool denied FILE_SHARE_WRITE. The provider must be
        // able to fill its received copy while this consumer handle is open.
        std::fs::write(&path, b"abc").expect("reader cannot exclude hydration writes");
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"abc");
        drop(reader);
        assert_eq!(delivery.readers.load(Ordering::Acquire), 0);
        assert!(native::open_path_in(&service, &temp.path().join("other.msi")).is_none());
        std::fs::remove_file(&path).unwrap();
        assert!(native::open_path_in(&service, &path).unwrap().is_err());
    }

    #[test]
    fn received_package_reader_requires_explicit_hydration_before_reading() {
        use crate::content::namespace::NativeFiles;
        use crate::model::NodeId;
        static HYDRATIONS: AtomicUsize = AtomicUsize::new(0);
        static FAILURE: AtomicI32 = AtomicI32::new(0);
        unsafe extern "system" fn hydrate(
            handle: HANDLE,
            start: i64,
            length: i64,
            _: i32,
            operation: *mut windows_sys::Win32::System::IO::OVERLAPPED,
        ) -> i32 {
            assert_eq!((start, length), (0, -1));
            assert!(!operation.is_null());
            HYDRATIONS.fetch_add(1, Ordering::SeqCst);
            let failure = FAILURE.load(Ordering::SeqCst);
            if failure < 0 {
                return failure;
            }
            let mut path = [0u16; 32768];
            // SAFETY: the reader owns handle; path has the advertised capacity.
            let count = unsafe { GetFinalPathNameByHandleW(handle, path.as_mut_ptr(), path.len() as u32, 0) };
            assert!(count > 0 && (count as usize) < path.len());
            std::fs::write(PathBuf::from(String::from_utf16_lossy(&path[..count as usize])), b"abc").unwrap();
            0
        }
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_hydrate(temp.path().join("CloudFiles"), owned_info, hydrate, fake_pin);
        let delivery = service
            .publish(vec![entry("dummy.msi", 3, false)], Bytes::new(3), Instant::now())
            .unwrap();
        reader_catalog(&service, &delivery);
        HYDRATIONS.store(0, Ordering::SeqCst);
        FAILURE.store(0, Ordering::SeqCst);
        let mut reader = delivery.open(NodeId(1), 1).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(
            bytes, b"bc",
            "native content must be hydrated before reading at the requested offset"
        );
        assert_eq!(HYDRATIONS.load(Ordering::SeqCst), 1);
        drop(reader);
        FAILURE.store(0x80070185_u32 as i32, Ordering::SeqCst);
        let error = match native::open_path_in(&service, &delivery.file_path(0)).unwrap() {
            Ok(_) => panic!("a failed explicit hydration must not fall back to raw reads"),
            Err(error) => error,
        };
        assert_eq!(error.raw_os_error(), Some(389));
        assert_eq!(delivery.readers.load(Ordering::Acquire), 0);
        FAILURE.store(0, Ordering::SeqCst);
    }
    fn reader_catalog(service: &Service, delivery: &Delivery) {
        use crate::content::{Provider, linked::Descriptor};
        use crate::model::{ContentSize, FileNode, NodeId, NodeKind};
        let mut paths = HashMap::new();
        let nodes = delivery
            .entries()
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                let id = NodeId(index as u64 + 1);
                let parent = entry.path.rsplit_once('/').map(|(parent, _)| paths[parent]);
                let name = entry.path.rsplit('/').next().unwrap().to_owned();
                paths.insert(entry.path, id);
                FileNode {
                    exec_bits: None,
                    id,
                    parent,
                    name,
                    kind: if entry.is_dir {
                        NodeKind::Directory
                    } else {
                        NodeKind::File
                    },
                    size: ContentSize::Known(entry.size),
                    version: 1,
                    modified_ms: None,
                }
            })
            .collect::<Vec<_>>();
        // Synthetic authority with the same durable shape as a received package;
        // this fixture's FileContents supplies bytes without a network peer.
        let descriptor = Descriptor {
            native: Default::default(),
            provisional: false,
            fulfilled: vec![],
            offer: crate::content::tests::fixture().inventory().id,
            token: crate::content::grants::LinkToken([9; 32]),
            roots: nodes.iter().filter(|node| node.parent.is_none()).cloned().collect(),
        };
        let mut journal = service.received.lock().unwrap();
        journal.sources.insert(
            delivery.id,
            received::Source {
                descriptor: Some(descriptor),
                settlement: Default::default(),
                failed: HashSet::new(),
                pending: HashSet::new(),
                listed: HashSet::new(),
                nodes,
                base: delivery.path.strip_prefix(&service.root).unwrap().into(),
            },
        );
        journal.save(&service.root).unwrap();
    }
    #[test]
    fn persistent_received_journal_rebuilds_unhydrated_delivery_without_network() {
        use crate::content::{
            Provider,
            linked::{Connector, Descriptor},
            namespace::Files,
        };
        use crate::model::{ContentSize, FileNode, NodeId, NodeKind};
        unsafe extern "system" fn incomplete_info(
            handle: HANDLE,
            kind: i32,
            buffer: *mut c_void,
            size: u32,
            written: *mut u32,
        ) -> i32 {
            // SAFETY: forwarding the same fixture-owned native output buffer.
            let code = unsafe { owned_info(handle, kind, buffer, size, written) };
            if code >= 0 {
                // SAFETY: the successful fixture call initialized this header.
                unsafe {
                    (*buffer.cast::<CF_PLACEHOLDER_STANDARD_INFO>()).OnDiskDataSize = 0;
                }
            }
            code
        }
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), incomplete_info);
        let descriptor = Descriptor {
            native: Default::default(),
            provisional: false,
            fulfilled: vec![],
            offer: crate::content::tests::fixture().inventory().id,
            token: crate::content::grants::LinkToken([9; 32]),
            roots: vec![FileNode {
                exec_bits: None,
                id: NodeId(7),
                parent: None,
                name: "linked.bin".into(),
                kind: NodeKind::File,
                size: ContentSize::Known(3),
                version: 1,
                modified_ms: None,
            }],
        };
        let namespace = Connector::default().restore(descriptor).unwrap();
        let contents = Files::new(namespace, crate::sync::files::TargetOs::Windows).unwrap();
        let entries = contents.descriptor_entries().unwrap().unwrap();
        let delivery = service.publish(entries, contents, Instant::now()).unwrap();
        let id = delivery.id;
        let path = delivery.file_path(0);
        drop(delivery);
        service.deliveries.lock().unwrap().clear();
        *service.received.lock().unwrap() = received::Journal::load(&service.root).unwrap();
        service.recover_received().unwrap();
        assert!(path.exists(), "recovery must retain a linked dataless native item");
        let restored = service
            .deliveries
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .expect("restored fetch owner");
        assert_eq!(restored.file_path(0), path);
        assert!(restored.contents().namespace().unwrap().source_link().is_some());
        assert_eq!(
            restored.contents().placeholder_metadata(0).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        assert!(!restored.expired());
        // Recovery follows the journal's native path, not an old display name.
        // This simulates persisted rename metadata; native rename admission is
        // covered separately and still requires desktop acceptance.
        let renamed = service.root.join("relocated.bin");
        let old_identity_path = path.canonicalize().unwrap();
        std::fs::rename(&path, &renamed).unwrap();
        for (identity_path, _) in IDENTITIES.lock().unwrap().iter_mut() {
            if *identity_path == old_identity_path {
                *identity_path = renamed.canonicalize().unwrap();
            }
        }
        {
            let mut journal = service.received.lock().unwrap();
            journal.deliveries.get_mut(&id).unwrap()[0].path = PathBuf::from("relocated.bin");
            journal.save(&service.root).unwrap();
        }
        service.deliveries.lock().unwrap().clear();
        *service.received.lock().unwrap() = received::Journal::load(&service.root).unwrap();
        service.recover_received().unwrap();
        let relocated = service.deliveries.lock().unwrap().get(&id).cloned().unwrap();
        assert_eq!(relocated.file_path(0), renamed);
        assert_eq!(relocated.roots, vec![renamed.clone()]);
        service
            .received
            .lock()
            .unwrap()
            .sources
            .get_mut(&id)
            .unwrap()
            .descriptor
            .as_mut()
            .unwrap()
            .provisional = true;
        service.recover_received().unwrap();
        assert!(
            !renamed.exists(),
            "restart removes the exact provisional native identity"
        );
    }
    #[test]
    fn cancelled_native_drag_removes_payload_even_after_preview() {
        drop_cleanup_fixture(0, false, false, 3);
        drop_cleanup_fixture(0, false, false, 0);
    }
    #[test]
    fn successful_drag_preserves_received_payload() {
        drop_cleanup_fixture(2, false, false, 3);
    }
    #[test]
    fn reusable_clipboard_abort_does_not_delete_earlier_received_payload() {
        drop_cleanup_fixture(3, false, false, 3);
    }
    #[test]
    fn cancellation_retries_after_locked_file_and_journal_reload() {
        drop_cleanup_fixture(0, true, false, 3);
    }
    #[test]
    fn cancellation_does_not_recursively_remove_untracked_user_files() {
        drop_cleanup_fixture(0, false, true, 3);
    }
    fn drop_cleanup_fixture(outcome: u8, blocked: bool, foreign: bool, size: u64) {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let contents = Bytes::new(size as usize);
        let entries = vec![entry("folder", 0, true), entry("folder/file", size, false)];
        let offer = Offer {
            created: Instant::now(),
            service: service.clone(),
            entries: entries.clone(),
            contents: contents.clone(),
            prepared: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            virtual_files: true,
        };
        offer.hdrop().unwrap();
        let delivery = offer.prepared.lock().unwrap().as_ref().unwrap().clone();
        let batch = delivery.path.clone();
        let source = batch.parent().unwrap().to_path_buf();
        // A preview already completed hydration; this is still provisional.
        service.native_complete(delivery.id, 1, size);
        let lock = blocked.then(|| {
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(delivery.file_path(1))
                .unwrap()
        });
        if foreign {
            std::fs::write(batch.join("user-file"), b"keep me").unwrap();
        }
        let worker = super::super::worker::Worker::spawn().unwrap();
        worker
            .run(move |_| {
                use windows::Win32::{Foundation::E_ABORT, UI::Shell::IDataObjectAsyncCapability};
                use windows::core::Interface;
                let f = super::super::formats();
                let (data, shared) = super::super::vfile::build(
                    super::super::vfile::Spec {
                        cloud: Some(offer),
                        descriptors: super::super::files::descriptors(&entries),
                        fixed: vec![],
                        contents,
                        cf_descriptor: f.file_descriptor,
                        cf_contents: f.file_contents,
                    },
                    true,
                );
                if outcome != 3 {
                    shared.host_drag_images();
                }
                if outcome == 1 || outcome == 3 {
                    // SAFETY: local COM object, no desktop or clipboard is accessed.
                    unsafe {
                        data.cast::<IDataObjectAsyncCapability>()
                            .unwrap()
                            .EndOperation(E_ABORT, None, 0)
                            .unwrap();
                    }
                } else {
                    shared.drag_ended(outcome == 2, false);
                }
                if outcome < 2 {
                    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
                    let format = FORMATETC {
                        cfFormat: 15,
                        dwAspect: DVASPECT_CONTENT.0,
                        lindex: -1,
                        tymed: TYMED_HGLOBAL.0 as u32,
                        ..Default::default()
                    };
                    // SAFETY: the local data object and format remain live.
                    let result = unsafe { data.QueryGetData(&format) };
                    assert!(result.is_err(), "cancelled drag republished paths");
                }
            })
            .unwrap();
        if outcome >= 2 {
            service.maintain();
            assert!(delivery.file_path(1).exists());
            return;
        }
        if blocked {
            assert!(delivery.file_path(1).exists(), "incompatible open handle was ignored");
            let journal = received::Journal::load(&service.root).unwrap();
            assert!(journal.cancelled.contains(&delivery.id), "cancellation was not durable");
            *service.received.lock().unwrap() = journal;
            service.recover_received().unwrap();
            drop(lock);
        }
        service.maintain();
        assert!(!delivery.file_path(1).exists(), "cancelled payload survived");
        if foreign {
            assert_eq!(std::fs::read(batch.join("user-file")).unwrap(), b"keep me");
            return;
        }
        assert!(!batch.exists(), "failed/cancelled drop retained materialized files");
        assert!(
            source.is_dir(),
            "plain grouping directories have no identity proof for deletion"
        );
        assert_eq!(std::fs::read_dir(&source).unwrap().count(), 0);
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
                exec_bits: None,
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
                    exec_bits: None,
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
        assert!(delivery.file(0).withdrawn.load(Ordering::Acquire));
        assert!(
            delivery.path.join("file").exists(),
            "a native identity query failure preserves the path"
        );
        assert!(!delivery.file(0).deleted.load(Ordering::Acquire));
        request(&service, &delivery, &receiver, 0, false);
        assert!(delivery.file(0).withdrawn.load(Ordering::Acquire));
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
    fn received_cleanup_rechecks_unread_consumers_after_native_identity_query() {
        use crate::content::namespace::{NativeFiles, NativeLease};
        static TARGET: Mutex<Weak<Delivery>> = Mutex::new(Weak::new());
        static LEASE: Mutex<Option<NativeLease>> = Mutex::new(None);
        static QUERIES: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "system" fn racing_info(
            handle: HANDLE,
            kind: i32,
            buffer: *mut c_void,
            size: u32,
            written: *mut u32,
        ) -> i32 {
            // SAFETY: all arguments belong to the fake native metadata call.
            let result = unsafe { owned_info(handle, kind, buffer, size, written) };
            // The first two queries discover/pin the item. The third is the
            // exclusive deletion handle's identity check after the initial
            // busy snapshot. Admit a new unread stream in precisely that gap.
            if result >= 0 && QUERIES.fetch_add(1, Ordering::SeqCst) == 2 {
                let delivery = TARGET.lock().unwrap().upgrade().unwrap();
                *LEASE.lock().unwrap() = Some(delivery.lease().unwrap());
            }
            result
        }
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), racing_info);
        let delivery = service
            .publish(vec![entry("file", 3, false)], Bytes::new(3), Instant::now())
            .unwrap();
        let path = delivery.file_path(0);
        std::fs::write(&path, b"old").unwrap();
        {
            let mut journal = service.received.lock().unwrap();
            let node = &mut journal.deliveries.get_mut(&delivery.id).unwrap()[0];
            node.life = crate::received::Item::new(1);
            node.life.materialized = true;
            node.life.shell = true;
        }
        *TARGET.lock().unwrap() = Arc::downgrade(&delivery);
        QUERIES.store(0, Ordering::SeqCst);
        service.received_sweep();
        assert!(LEASE.lock().unwrap().is_some(), "fixture must enter the race window");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"old",
            "cleanup removed an unread consumer's copy"
        );
        drop(LEASE.lock().unwrap().take());
        service.received_sweep();
        assert!(!path.exists(), "cleanup retries after the final consumer drains");
        assert!(service.received.lock().unwrap().cleanup_inflight.is_empty());
    }

    #[test]
    fn late_source_body_cannot_overwrite_local_write_truncate_or_replacement() {
        static EDIT: AtomicUsize = AtomicUsize::new(0);
        struct Source {
            path: Mutex<PathBuf>,
            mode: usize,
        }
        impl FileContents for Source {
            fn metadata(&self, _: usize) -> io::Result<Option<FileMetadata>> {
                Ok(Some(FileMetadata {
                    exec_bits: None,
                    size: 3,
                    modified_ms: Some(1000),
                }))
            }
            fn open(&self, _: usize) -> io::Result<crate::FileReader> {
                struct Body {
                    path: PathBuf,
                    mode: usize,
                    bytes: io::Cursor<Vec<u8>>,
                    edited: bool,
                }
                impl Read for Body {
                    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                        if !self.edited {
                            self.edited = true;
                            match self.mode {
                                1 => std::fs::write(&self.path, b"user edit")?,
                                2 => std::fs::write(&self.path, b"")?,
                                _ => {
                                    std::fs::rename(&self.path, self.path.with_extension("moved"))?;
                                    std::fs::write(&self.path, b"replacement")?;
                                }
                            }
                            EDIT.store(self.mode, Ordering::Release);
                        }
                        self.bytes.read(output)
                    }
                }
                Ok(Box::new(Body {
                    path: self.path.lock().unwrap().clone(),
                    mode: self.mode,
                    bytes: io::Cursor::new(b"abc".to_vec()),
                    edited: false,
                }))
            }
        }
        unsafe extern "system" fn edited_info(
            handle: HANDLE,
            kind: i32,
            buffer: *mut c_void,
            size: u32,
            written: *mut u32,
        ) -> i32 {
            // SAFETY: forwarding private fixture buffers; successful call initialized the header.
            unsafe {
                let code = owned_info(handle, kind, buffer, size, written);
                if code >= 0 {
                    let info = &mut *buffer.cast::<CF_PLACEHOLDER_STANDARD_INFO>();
                    match EDIT.load(Ordering::Acquire) {
                        1 => {
                            info.ModifiedDataSize = 9;
                            info.InSyncState = CF_IN_SYNC_STATE_NOT_IN_SYNC;
                        }
                        2 => {
                            info.ModifiedDataSize = 0;
                            info.InSyncState = CF_IN_SYNC_STATE_NOT_IN_SYNC;
                        }
                        3 => {
                            std::ptr::copy_nonoverlapping(
                                999u64.to_le_bytes().as_ptr(),
                                info.FileIdentity.as_mut_ptr(),
                                8,
                            );
                        }
                        _ => {}
                    }
                }
                code
            }
        }
        let _serial = SERIAL.lock().unwrap();
        for mode in 1..=3 {
            EDIT.store(0, Ordering::Release);
            let temp = tempfile::tempdir().unwrap();
            let (service, receiver) = fake_with_info(temp.path().join("CloudFiles"), edited_info);
            let source = Arc::new(Source {
                path: Mutex::new(PathBuf::new()),
                mode,
            });
            let delivery = service
                .publish(vec![entry("file", 3, false)], source.clone(), Instant::now())
                .unwrap();
            let path = delivery.file_path(0);
            *source.path.lock().unwrap() = path.clone();
            request(&service, &delivery, &receiver, 0, false);
            assert!(
                TRANSFERS
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(_, _, status)| *status != STATUS_SUCCESS),
                "late source bytes reached a locally changed native file"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                match mode {
                    1 => b"user edit".as_slice(),
                    2 => b"",
                    _ => b"replacement",
                }
            );
        }
    }

    #[test]
    fn active_download_handle_allows_the_provider_mutation_guard() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let delivery = service
            .publish(vec![entry("file", 3, false)], Bytes::new(3), Instant::now())
            .unwrap();
        let path = delivery.file_path(0);
        // The active-download handle stays open until CfHydratePlaceholder
        // completes, including the provider's FETCH_DATA mutation window.
        let waiting = delivery.checked_file(&service, 0, &path, true).unwrap();
        let mutation = protection::Mutation::open(service.api.clone(), &path);
        assert!(
            mutation.is_ok(),
            "our active download must not count as a conflicting writer"
        );
        drop(mutation);
        drop(waiting);
    }

    #[test]
    fn recovery_never_discards_a_partly_resident_local_edit_as_an_abandoned_placeholder() {
        unsafe extern "system" fn dirty_info(
            handle: HANDLE,
            kind: i32,
            buffer: *mut c_void,
            size: u32,
            written: *mut u32,
        ) -> i32 {
            // SAFETY: forwards bounded private fixture storage; success initializes the header.
            unsafe {
                let result = owned_info(handle, kind, buffer, size, written);
                if result >= 0 {
                    let info = &mut *buffer.cast::<CF_PLACEHOLDER_STANDARD_INFO>();
                    info.InSyncState = CF_IN_SYNC_STATE_NOT_IN_SYNC;
                    info.ModifiedDataSize = 1;
                    info.OnDiskDataSize = 1;
                }
                result
            }
        }
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), dirty_info);
        let delivery = service
            .publish(vec![entry("partial", 3, false)], Bytes::new(3), Instant::now())
            .unwrap();
        let id = delivery.id;
        let path = delivery.file_path(0);
        std::fs::write(&path, b"own").unwrap();
        drop(delivery);
        service.deliveries.lock().unwrap().clear();
        service.received_sweep();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"own",
            "orphan recovery is not permission to discard edits"
        );
        assert!(!service.received.lock().unwrap().deliveries[&id][0].deleted);
    }

    #[test]
    fn empty_selected_root_survives_other_roots_last_file_moving_out() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let delivery = service
            .publish(
                vec![
                    entry("left", 0, true),
                    entry("right", 0, true),
                    entry("right/data.bin", 3, false),
                ],
                Bytes::new(3),
                Instant::now(),
            )
            .unwrap();
        reader_catalog(&service, &delivery);
        let left = delivery.file_path(0);
        let right = delivery.file_path(1);
        let child = delivery.file_path(2);
        assert_eq!(delivery.roots, [left.clone(), right.clone()]);
        std::fs::write(&child, b"abc").unwrap();
        // Explorer may finish one root before consuming another root's empty
        // folder. Moving the last payload must not retire that other selection.
        let target = temp.path().join("received");
        std::fs::create_dir(&target).unwrap();
        std::fs::rename(&child, target.join("data.bin")).unwrap();
        service.received_sweep();
        assert!(
            left.is_dir(),
            "an independently selected empty root is received content"
        );
        assert!(!service.received.lock().unwrap().deliveries[&delivery.id][0].deleted);
        assert!(!right.exists(), "the consumed nonempty root can retire normally");
        assert_eq!(std::fs::read(target.join("data.bin")).unwrap(), b"abc");
    }

    #[test]
    fn deleted_close_releases_consumer_before_pruning_empty_received_tree() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let delivery = service
            .publish(
                vec![entry("tree", 0, true), entry("tree/file", 3, false)],
                Bytes::new(3),
                Instant::now(),
            )
            .unwrap();
        let root = delivery.file_path(0);
        let child = delivery.file_path(1);
        std::fs::write(&child, b"abc").unwrap();
        service.received_observe(delivery.id, 1, crate::received::Reader::Shell, true, false);
        let target = temp.path().join("received-file");
        std::fs::rename(&child, &target).unwrap();
        service.received_observe(delivery.id, 1, crate::received::Reader::Shell, false, true);
        assert_eq!(service.received.lock().unwrap().opens[&(delivery.id, 1)], 0);
        service.received_sweep();
        assert!(!root.exists(), "a deleted close must not retain a phantom consumer");
        assert_eq!(std::fs::read(target).unwrap(), b"abc");
    }

    #[test]
    fn moved_child_without_close_does_not_pin_empty_parent_after_directory_reader_closes() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let delivery = service
            .publish(
                vec![entry("tree", 0, true), entry("tree/file", 3, false)],
                Bytes::new(3),
                Instant::now(),
            )
            .unwrap();
        let root = delivery.file_path(0);
        let child = delivery.file_path(1);
        std::fs::write(&child, b"abc").unwrap();
        service.received_observe(delivery.id, 0, crate::received::Reader::Shell, true, false);
        service.received_observe(delivery.id, 1, crate::received::Reader::Shell, true, false);
        let target = temp.path().join("moved-file");
        std::fs::rename(&child, &target).unwrap();
        service
            .received
            .lock()
            .unwrap()
            .moves
            .insert(delivery.id, moved::State::Observed);
        service.received_sweep();
        assert!(
            root.is_dir(),
            "a live directory consumer still protects the empty parent"
        );
        service.received_observe(delivery.id, 0, crate::received::Reader::Shell, false, false);
        service.received_sweep();
        assert!(
            !root.exists(),
            "a moved-out child does not protect its old empty parent"
        );
        assert_eq!(std::fs::read(target).unwrap(), b"abc");
        service.received_sweep();
        assert!(matches!(
            service.received.lock().unwrap().moves[&delivery.id],
            moved::State::Removed
        ));
    }

    #[test]
    fn prune_never_removes_a_foreign_empty_directory_at_an_old_received_path() {
        unsafe extern "system" fn replaced_info(
            handle: HANDLE,
            kind: i32,
            buffer: *mut c_void,
            size: u32,
            written: *mut u32,
        ) -> i32 {
            // SAFETY: forwarding private output buffers; override only the former leaf identity.
            unsafe {
                let result = owned_info(handle, kind, buffer, size, written);
                if result >= 0 {
                    let info = &mut *buffer.cast::<CF_PLACEHOLDER_STANDARD_INFO>();
                    if std::slice::from_raw_parts(info.FileIdentity.as_ptr().add(8), 8) == 0u64.to_le_bytes() {
                        std::ptr::copy_nonoverlapping(999u64.to_le_bytes().as_ptr(), info.FileIdentity.as_mut_ptr(), 8);
                    }
                }
                result
            }
        }
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), replaced_info);
        let delivery = service
            .publish(vec![entry("empty", 0, true)], Bytes::new(0), Instant::now())
            .unwrap();
        let path = delivery.file_path(0);
        std::fs::rename(&path, temp.path().join("moved-target")).unwrap();
        std::fs::create_dir(&path).unwrap();
        service.received.lock().unwrap().cancelled.insert(delivery.id);
        service.received_sweep();
        assert!(
            path.is_dir(),
            "prune deleted a foreign directory at an old publication path"
        );
        assert!(temp.path().join("moved-target").is_dir());
    }

    #[test]
    fn reported_move_deletes_an_unread_received_copy() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let delivery = service
            .publish(vec![entry("unread", 3, false)], Bytes::new(3), Instant::now())
            .unwrap();
        let path = delivery.file_path(0);
        std::fs::write(&path, b"abc").unwrap();
        let offer = Offer {
            created: Instant::now(),
            service: service.clone(),
            entries: vec![entry("unread", 3, false)],
            contents: Bytes::new(3),
            prepared: Mutex::new(Some(delivery.clone())),
            cancelled: AtomicBool::new(false),
            virtual_files: true,
        };
        offer.move_completed().unwrap();
        service.cleanup_moved();
        assert!(
            !path.exists(),
            "a successful MOVE report owns deletion even without a read"
        );
    }

    #[test]
    fn optimized_move_converges_identity_without_touching_the_target() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let entries = vec![entry("file", 3, false)];
        let contents = Bytes::new(3);
        let delivery = service
            .publish(entries.clone(), contents.clone(), Instant::now())
            .unwrap();
        let path = delivery.file_path(0);
        std::fs::write(&path, b"abc").unwrap();
        let target = temp.path().join("moved-target");
        std::fs::rename(&path, &target).unwrap();
        let offer = Offer {
            created: Instant::now(),
            service: service.clone(),
            entries,
            contents,
            prepared: Mutex::new(Some(delivery.clone())),
            cancelled: AtomicBool::new(false),
            virtual_files: true,
        };
        offer.optimized_move_completed().unwrap();
        service.cleanup_moved();
        assert_eq!(std::fs::read(target).unwrap(), b"abc");
        assert!(!path.exists());
        assert!(service.received.lock().unwrap().moves[&delivery.id] == moved::State::Removed);
    }

    #[test]
    fn native_move_feedback_matches_shell_source_rules() {
        use crate::content::namespace::NativeFiles;
        use windows::Win32::System::{
            Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, FORMATETC},
            DataExchange::RegisterClipboardFormatW,
            Ole::ReleaseStgMedium,
        };
        use windows::Win32::UI::Shell::IDataObjectAsyncCapability;
        use windows::core::Interface;
        let _serial = SERIAL.lock().unwrap();
        // SAFETY: private apartment, data objects and files; no live clipboard or sync root.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap() };
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                // SAFETY: balances initialization on this thread.
                unsafe { CoUninitialize() };
            }
        }
        let _apartment = Apartment;
        for (name, performed, pasted, logical, drag, asynchronous, delete, observed) in [
            ("drag unoptimized", Some(2), None, None, Some(true), None, true, true),
            ("drag MOVE alone", None, None, None, Some(true), None, false, true),
            (
                "drag disagreeing effects",
                Some(2),
                None,
                None,
                Some(false),
                None,
                false,
                true,
            ),
            ("drag optimized", Some(0), None, Some(2), Some(false), None, false, true),
            ("drag copy", Some(1), None, None, Some(false), None, false, false),
            ("paste unoptimized", Some(2), Some(2), None, None, None, true, true),
            ("paste optimized", None, Some(2), None, None, None, false, true),
            ("paste copy", None, None, None, None, None, false, false),
            (
                "async unoptimized",
                Some(2),
                None,
                None,
                Some(true),
                Some(2),
                true,
                true,
            ),
            ("async MOVE alone", None, None, None, Some(true), Some(2), false, true),
            (
                "async paste unoptimized",
                Some(2),
                Some(2),
                None,
                None,
                Some(2),
                true,
                true,
            ),
            ("async paste optimized", None, Some(2), None, None, Some(0), false, true),
            (
                "async optimized",
                Some(0),
                None,
                Some(2),
                Some(false),
                Some(0),
                false,
                true,
            ),
            ("async copy", Some(1), None, None, Some(false), Some(1), false, false),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
            let contents = Bytes::new(3);
            let entries = vec![entry("file", 3, false)];
            let delivery = service
                .publish(entries.clone(), contents.clone(), Instant::now())
                .unwrap();
            let path = delivery.file_path(0);
            std::fs::write(&path, b"abc").unwrap();
            // Neither an unread lease nor missing hydration/consumption proves
            // anything about a target's documented unoptimized MOVE report.
            let _consumer = delivery.lease().unwrap();
            let offer = Offer {
                created: Instant::now(),
                service: service.clone(),
                entries: entries.clone(),
                contents: contents.clone(),
                prepared: Mutex::new(Some(delivery.clone())),
                cancelled: AtomicBool::new(false),
                virtual_files: true,
            };
            let formats = super::super::formats();
            let (data, shared) = super::super::vfile::build(
                super::super::vfile::Spec {
                    cloud: Some(offer),
                    descriptors: super::super::files::descriptors(&entries),
                    fixed: vec![],
                    contents,
                    cf_descriptor: formats.file_descriptor,
                    cf_contents: formats.file_contents,
                },
                true,
            );
            let capability: IDataObjectAsyncCapability = data.cast().unwrap();
            if asynchronous.is_some() {
                // SAFETY: matching operation on this private data object.
                unsafe { capability.StartOperation(None).unwrap() };
            }
            for (format, effect) in [
                (windows::core::w!("Performed DropEffect"), performed),
                (windows::core::w!("Logical Performed DropEffect"), logical),
                (windows::core::w!("Paste Succeeded"), pasted),
            ] {
                let Some(effect) = effect else { continue };
                // SAFETY: owned medium and scalar format on this apartment.
                unsafe {
                    let format = FORMATETC {
                        cfFormat: RegisterClipboardFormatW(format) as u16,
                        dwAspect: 1,
                        lindex: -1,
                        tymed: 1,
                        ..Default::default()
                    };
                    use windows::Win32::System::Com::{STGMEDIUM, STGMEDIUM_0};
                    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
                    let handle = GlobalAlloc(GMEM_MOVEABLE, 4).unwrap();
                    let buffer = GlobalLock(handle).cast::<u8>();
                    assert!(!buffer.is_null());
                    std::ptr::copy_nonoverlapping(u32::to_le_bytes(effect).as_ptr(), buffer, 4);
                    let _ = GlobalUnlock(handle);
                    let mut medium = STGMEDIUM {
                        tymed: 1,
                        u: STGMEDIUM_0 { hGlobal: handle },
                        pUnkForRelease: std::mem::ManuallyDrop::new(None),
                    };
                    data.SetData(&format, &medium, false).unwrap();
                    ReleaseStgMedium(&mut medium);
                }
            }
            if let Some(moved) = drag {
                shared.drag_ended(true, moved);
            }
            if let Some(effect) = asynchronous {
                assert!(path.exists(), "{name}: SetData/DoDragDrop must wait for EndOperation");
                // SAFETY: completes the private operation after all SetData feedback.
                unsafe {
                    capability
                        .EndOperation(windows::core::HRESULT(0), None, effect)
                        .unwrap()
                };
            }
            service.cleanup_moved();
            assert_eq!(!path.exists(), delete, "{name}: physical deletion");
            let journal = service.received.lock().unwrap();
            assert_eq!(
                journal.moves.contains_key(&delivery.id),
                observed,
                "{name}: identity convergence"
            );
        }
    }

    #[test]
    fn cancelled_received_copy_waits_for_unread_consumer_release() {
        use crate::content::namespace::NativeFiles;
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let delivery = service
            .publish(vec![entry("file", 3, false)], Bytes::new(3), Instant::now())
            .unwrap();
        let path = delivery.file_path(0);
        std::fs::write(&path, b"old").unwrap();
        let lease = delivery.lease().unwrap();
        service.cancel_received(delivery.id);
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert!(delivery.lease().is_err(), "cancellation rejects new consumers");
        drop(lease);
        service.received_sweep();
        assert!(!path.exists());
    }

    #[test]
    fn withdrawal_identity_check_holds_the_handle_until_deletion() {
        static REPLACE: Mutex<Option<(PathBuf, PathBuf)>> = Mutex::new(None);
        static REPLACED: AtomicBool = AtomicBool::new(false);
        unsafe extern "system" fn racing_info(
            handle: HANDLE,
            kind: i32,
            buffer: *mut c_void,
            size: u32,
            written: *mut u32,
        ) -> i32 {
            // SAFETY: forwards the fixture-owned handle and output allocation.
            let result = unsafe { owned_info(handle, kind, buffer, size, written) };
            if result >= 0
                && let Some((path, saved)) = REPLACE.lock().unwrap().take()
            {
                // Model a local atomic save precisely between identity lookup
                // and deletion, without Cloud Files registration or desktop IO.
                if std::fs::rename(&path, saved).is_ok() {
                    std::fs::write(&path, b"user replacement").unwrap();
                    REPLACED.store(true, Ordering::SeqCst);
                }
            }
            result
        }
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, _) = fake_with_info(temp.path().join("CloudFiles"), racing_info);
        let delivery = service
            .publish(vec![entry("file", 3, false)], Bytes::new(3), Instant::now())
            .unwrap();
        let path = delivery.file_path(0);
        std::fs::write(&path, b"old").unwrap();
        *REPLACE.lock().unwrap() = Some((path.clone(), temp.path().join("old")));
        REPLACED.store(false, Ordering::SeqCst);
        delivery.file(0).deleted.store(true, Ordering::Release);
        delivery.remove_withdrawn(&service);
        assert!(
            !REPLACED.load(Ordering::SeqCst),
            "identity lookup must hold an exclusive native handle through deletion"
        );
        assert!(!path.exists());
    }
    #[test]
    fn confirmed_source_gone_preserves_locally_modified_native_bytes() {
        static MODIFIED: AtomicI64 = AtomicI64::new(0);
        unsafe extern "system" fn edited_info(
            handle: HANDLE,
            kind: i32,
            buffer: *mut c_void,
            size: u32,
            written: *mut u32,
        ) -> i32 {
            // SAFETY: the fixture owns the handle and native output allocation.
            let result = unsafe { owned_info(handle, kind, buffer, size, written) };
            if result >= 0 {
                // In-sync is cleared even by a truncate to zero, where there are
                // no modified ranges left to count. File length/time are no proof.
                // SAFETY: successful fixture query initialized the aligned header.
                let info = unsafe { &mut *buffer.cast::<CF_PLACEHOLDER_STANDARD_INFO>() };
                info.ModifiedDataSize = MODIFIED.load(Ordering::Relaxed);
                info.InSyncState = CF_IN_SYNC_STATE_NOT_IN_SYNC;
            }
            result
        }
        let _serial = SERIAL.lock().unwrap();
        for bytes in [b"user changed bytes".as_slice(), b""] {
            let temp = tempfile::tempdir().unwrap();
            let (service, receiver) = fake_with_info(temp.path().join("CloudFiles"), edited_info);
            let source = Arc::new(Versioned {
                size: 18,
                missing: true,
                fail_eof: false,
                selected: AtomicU64::new(0),
            });
            let delivery = service
                .publish(vec![entry("file", 18, false)], source, Instant::now())
                .unwrap();
            let path = delivery.file_path(0);
            std::fs::write(&path, bytes).unwrap();
            MODIFIED.store(bytes.len() as i64, Ordering::Relaxed);
            request(&service, &delivery, &receiver, 0, false);
            delivery.remove_withdrawn(&service);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "Gone must preserve local contents"
            );
            assert!(!delivery.file(0).deleted.load(Ordering::Acquire));
            assert!(!service.received.lock().unwrap().deliveries[&delivery.id][0].deleted);
            let journal = received::Journal::load(&service.root).unwrap();
            let saved = &journal.deliveries[&delivery.id][0];
            assert!(saved.local_final);
            assert!(!saved.withdrawn);
            assert_eq!(saved.length, bytes.len() as u64);
            request(&service, &delivery, &receiver, 0, false);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "a later fetch cannot revive the source"
            );
        }
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
    fn lazy_real_directory_publication_does_not_enumerate_children() {
        use crate::content::{
            Provider,
            linked::Descriptor,
            namespace::{Files, Namespace, NativeFiles},
        };
        use crate::model::{ContentSize, DirectoryCursor, DirectoryPage, FileNode, NodeId, NodeKind};
        struct Tree {
            nodes: Vec<FileNode>,
            lists: Arc<AtomicUsize>,
        }
        impl Namespace for Tree {
            fn roots(&self) -> Vec<FileNode> {
                vec![self.nodes[0].clone()]
            }
            fn source_link(&self) -> Option<Descriptor> {
                Some(Descriptor {
                    offer: crate::content::tests::fixture().inventory().id,
                    token: crate::content::grants::LinkToken([91; 32]),
                    roots: self.roots(),
                    native: Default::default(),
                    provisional: false,
                    fulfilled: vec![],
                })
            }
            fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
                self.lists.fetch_add(1, Ordering::Relaxed);
                crate::content::namespace::page(
                    node,
                    1,
                    &self
                        .nodes
                        .iter()
                        .filter(|entry| entry.parent == Some(node))
                        .cloned()
                        .collect::<Vec<_>>(),
                    cursor,
                )
            }
            fn select(&self, node: NodeId) -> io::Result<FileNode> {
                self.nodes
                    .iter()
                    .find(|entry| entry.id == node)
                    .cloned()
                    .ok_or(io::ErrorKind::NotFound.into())
            }
            fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<crate::FileReader> {
                panic!("publishing paths cannot read source bytes")
            }
        }
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, receiver) = fake_with_info(temp.path().join("CloudFiles"), owned_info);
        let lists = Arc::new(AtomicUsize::new(0));
        let mut nodes = vec![
            FileNode {
                exec_bits: None,
                id: NodeId(1),
                parent: None,
                name: "folder".into(),
                kind: NodeKind::Directory,
                size: ContentSize::Known(0),
                version: 1,
                modified_ms: None,
            },
            FileNode {
                exec_bits: None,
                id: NodeId(2),
                parent: Some(NodeId(1)),
                name: "child".into(),
                kind: NodeKind::File,
                size: ContentSize::Known(3),
                version: 1,
                modified_ms: None,
            },
            FileNode {
                exec_bits: None,
                id: NodeId(3),
                parent: Some(NodeId(1)),
                name: "nested".into(),
                kind: NodeKind::Directory,
                size: ContentSize::Known(0),
                version: 1,
                modified_ms: None,
            },
            FileNode {
                exec_bits: None,
                id: NodeId(4),
                parent: Some(NodeId(3)),
                name: "leaf".into(),
                kind: NodeKind::File,
                size: ContentSize::Known(8),
                version: 1,
                modified_ms: None,
            },
        ];
        let extra = crate::model::DIRECTORY_PAGE_ENTRIES + 1;
        nodes.extend((0..extra).map(|offset| FileNode {
            exec_bits: None,
            id: NodeId(10 + offset as u64),
            parent: Some(NodeId(1)),
            name: format!("paged-{offset}"),
            kind: NodeKind::File,
            size: ContentSize::Known(1),
            version: 1,
            modified_ms: None,
        }));
        let tree = Arc::new(Tree {
            nodes,
            lists: lists.clone(),
        });
        let contents = Files::new(tree.clone(), crate::sync::files::TargetOs::Windows).unwrap();
        let offer = Offer {
            service: service.clone(),
            entries: contents.roots().to_vec(),
            contents,
            created: Instant::now(),
            prepared: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            virtual_files: false,
        };
        offer.hdrop().unwrap();
        assert_eq!(
            lists.load(Ordering::Relaxed),
            0,
            "HDROP must publish only roots; directory enumeration belongs to FETCH_PLACEHOLDERS"
        );
        let delivery = offer.prepared.lock().unwrap().clone().unwrap();
        assert_eq!(delivery.entries().len(), 1);
        assert!(!delivery.file_path(0).join("child").exists());
        let root_path = delivery.file_path(0);
        let mut identity = [0u8; 16];
        identity[..8].copy_from_slice(&delivery.id.to_le_bytes());
        let mut info = CF_CALLBACK_INFO {
            CallbackContext: (&*service.context as *const Weak<Service>).cast_mut().cast(),
            FileIdentity: identity.as_ptr().cast(),
            FileIdentityLength: 16,
            TransferKey: 55,
            RequestKey: 56,
            ..Default::default()
        };
        KEYS.lock().unwrap().push((55, root_path.clone()));
        // Native cancellation only signals the worker; no source request escapes.
        // SAFETY: the fixture owns the callback record, context and identity.
        unsafe {
            population::fetch(&info, std::ptr::null());
            cancel(&info, std::ptr::null());
        }
        receiver.recv().unwrap().run();
        assert_eq!(lists.load(Ordering::Relaxed), 0);
        assert_eq!(*POPULATIONS.lock().unwrap(), [STATUS_CLOUD_FILE_REQUEST_CANCELED]);
        // A failed journal transaction cannot expose children or append live slots.
        let journal_path = service.root.with_extension("received");
        let backup_path = service.root.with_extension("saved");
        std::fs::rename(&journal_path, &backup_path).unwrap();
        std::fs::create_dir(&journal_path).unwrap();
        info.RequestKey = 57;
        // SAFETY: the callback data remains owned by this fixture.
        unsafe {
            population::fetch(&info, std::ptr::null());
        }
        receiver.recv().unwrap().run();
        assert_eq!(lists.load(Ordering::Relaxed), 2);
        assert_eq!(delivery.entries().len(), 1);
        assert!(!root_path.join("child").exists());
        assert!(service.received.lock().unwrap().sources[&delivery.id].listed.is_empty());
        std::fs::remove_dir(&journal_path).unwrap();
        std::fs::rename(&backup_path, &journal_path).unwrap();
        info.RequestKey = 58;
        FAIL_AFTER.store(1, Ordering::Relaxed);
        // SAFETY: the same fixture-owned callback data remains live.
        unsafe {
            population::fetch(&info, std::ptr::null());
        }
        receiver.recv().unwrap().run();
        assert_eq!(lists.load(Ordering::Relaxed), 4);
        assert!(root_path.join("child").exists());
        assert!(!root_path.join("nested").exists());
        assert_eq!(
            delivery.entries().len(),
            3 + extra,
            "all native slots must be durable before any child is exposed"
        );
        let journal = received::Journal::load(&service.root).unwrap();
        assert!(journal.sources[&delivery.id].listed.contains(&NodeId(1)));
        let original_child = service.disk_identity(&root_path.join("child")).unwrap();
        // Rebuild from disk after a partially successful native transfer. Recovery
        // must neither pull directories nor lose the uncreated child's slot.
        service.deliveries.lock().unwrap().clear();
        *service.received.lock().unwrap() = journal;
        service.recover_received().unwrap();
        assert_eq!(lists.load(Ordering::Relaxed), 4);
        let recovered = service.deliveries.lock().unwrap()[&delivery.id].clone();
        FAIL_AFTER.store(-1, Ordering::Relaxed);
        // A target-local replacement must never be overwritten on retry.
        let foreign = root_path.join("nested");
        std::fs::write(&foreign, b"local edit").unwrap();
        assert_eq!(
            recovered.populate_directory(NodeId(1), &|| Ok(())).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists,
        );
        assert_eq!(std::fs::read(&foreign).unwrap(), b"local edit");
        assert!(
            service.received.lock().unwrap().sources[&delivery.id]
                .pending
                .contains(&NodeId(1))
        );
        std::fs::remove_file(&foreign).unwrap();
        recovered.populate_directory(NodeId(1), &|| Ok(())).unwrap();
        assert_eq!(lists.load(Ordering::Relaxed), 4, "retry uses its durable discovery");
        assert_eq!(service.disk_identity(&root_path.join("child")).unwrap(), original_child);
        assert!(root_path.join("nested").is_dir());
        assert!(!root_path.join("nested/leaf").exists());
        let mut cursor = None;
        let mut listed = Vec::new();
        loop {
            let page = NativeFiles::list(&*recovered, NodeId(1), cursor).unwrap();
            listed.extend(page.entries);
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(listed.len(), 2 + extra);
        assert_eq!(lists.load(Ordering::Relaxed), 4);
        assert!(
            root_path.join("paged-0").exists(),
            "missing durable slots must not become deleted during recovery"
        );
        // Reattach the fixture source to exercise new demand after recovery.
        let source = service.received.lock().unwrap().sources[&delivery.id].clone();
        *recovered.contents.lock().unwrap() = Files::restore_table(
            tree,
            crate::sync::files::TargetOs::Windows,
            source.nodes,
            recovered.entries(),
        )
        .unwrap();
        let nested = NativeFiles::list(&*recovered, NodeId(3), None).unwrap();
        assert_eq!(nested.entries.len(), 1);
        assert!(root_path.join("nested/leaf").is_file());
        assert_eq!(lists.load(Ordering::Relaxed), 5);
        assert_eq!(recovered.entries().len(), 4 + extra);
        assert_eq!(
            service.disk_identity(&root_path.join("nested/leaf")).unwrap().1,
            3 + extra
        );
        assert!(
            service.received.lock().unwrap().sources[&delivery.id]
                .pending
                .is_empty()
        );
        assert!(service.requests.lock().unwrap().is_empty());
        assert_eq!(recovered.readers.load(Ordering::Relaxed), 0);
        KEYS.lock().unwrap().clear();
    }
    #[test]
    fn updated_placeholder_shrink_restarts_and_keeps_native_identity() {
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
            let identity = delivery.id;
            let path = delivery.file_path(0);
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
            assert_eq!(delivery.id, identity);
            assert_eq!(delivery.file_path(0), path);
            assert_eq!(delivery.file(0).selected.lock().unwrap().0.size, size);
        }
    }
    #[test]
    fn updated_placeholder_zero_inventory_uses_demand_selected_length() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, receiver) = fake(temp.path().join("CloudFiles"));
        let source = Arc::new(Versioned {
            size: 12,
            missing: false,
            fail_eof: false,
            selected: AtomicU64::new(0),
        });
        let offer = Offer {
            service,
            contents: source.clone(),
            entries: vec![entry("empty", 0, false)],
            created: Instant::now(),
            prepared: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            virtual_files: false,
        };
        offer
            .hdrop()
            .expect("a file that grew from zero must publish its selected size");
        let delivery = offer.prepared.lock().unwrap().clone().unwrap();
        assert_eq!(delivery.entries()[0].size, 12);
        assert_eq!(
            source.selected.load(Ordering::Relaxed),
            0,
            "metadata selection does not read a nonempty body"
        );
        request(&offer.service, &delivery, &receiver, 0, false);
        assert_eq!(source.selected.load(Ordering::Relaxed), 7);
        assert_eq!(
            TRANSFERS
                .lock()
                .unwrap()
                .iter()
                .map(|(_, bytes, _)| bytes.len())
                .sum::<usize>(),
            12
        );
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
                    exec_bits: None,
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
        delivery.file(0).selected.lock().unwrap().1 = Some(0);
        request(&service, &delivery, &receiver, 0, false);
        assert_eq!(UPDATES.lock().unwrap().len(), 1);
        assert_eq!(source.selected.load(Ordering::Relaxed), 7);
        assert_eq!(delivery.file(0).selected.lock().unwrap().1, Some(7));
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
            FileSize: delivery.file(index).selected.lock().unwrap().0.size as i64,
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
            if UPDATES.lock().unwrap().len() != before {
                let size = delivery.file(index).selected.lock().unwrap().0.size;
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(delivery.file_path(index))
                    .unwrap()
                    .set_len(size)
                    .unwrap();
            }
            if UPDATES.lock().unwrap().len() != before && delivery.file(index).selected.lock().unwrap().0.size != 0 {
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
    fn owner_publication_hydration_observation_and_cancellation_invalidate() {
        let _serial = SERIAL.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let (service, receiver) = fake(temp.path().join("CloudFiles"));
        let delivery = crate::cache::assert_mutation(&service.root, || {
            service.publish(vec![entry("file", 3, false)], Bytes::new(3), Instant::now())
        })
        .unwrap();
        crate::cache::assert_mutation(&service.root, || request(&service, &delivery, &receiver, 0, false));
        crate::cache::assert_mutation(&service.root, || {
            service.received_observe(delivery.id, 0, crate::received::Reader::App, true, false)
        });
        crate::cache::assert_mutation(&service.root, || {
            service.received_observe(delivery.id, 0, crate::received::Reader::App, false, false)
        });
        crate::cache::assert_mutation(&service.root, || service.cancel_received(delivery.id));
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
    fn creation_failure_leaves_no_empty_source_folder_and_expiry_releases_transfer_owner() {
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
        assert!(sources.is_empty(), "failed preparation left an empty source container");
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
            cancelled: AtomicBool::new(false),
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
            cancelled: AtomicBool::new(false),
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
            cancelled: AtomicBool::new(false),
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
            cancelled: AtomicBool::new(false),
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
            cancelled: AtomicBool::new(false),
            virtual_files: false,
        };
        let worker = super::super::worker::Worker::spawn().unwrap();
        worker
            .run(move |_| {
                use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL, TYMED_ISTREAM};
                let f = super::super::formats();
                use crate::content::Provider;
                use crate::model::*;
                let mut inventory = crate::content::tests::fixture().inventory();
                inventory.platform = oink_foundation::clipboard::ClipPlatform::Windows;
                inventory.roots = vec![FileNode {
                    exec_bits: None,
                    id: NodeId(1),
                    parent: None,
                    name: "one".into(),
                    kind: NodeKind::File,
                    size: ContentSize::Known(1),
                    version: 1,
                    modified_ms: None,
                }];
                inventory.items[0].file = Some(NodeId(1));
                inventory.items[0].virtual_files = false;
                inventory.items[0].representations = vec![Representation {
                    id: RepresentationId(1),
                    format: NativeFormat::WindowsPredefined(15),
                    standard: None,
                    medium: Medium::HGlobal,
                    aspect: 1,
                    index: -1,
                    data: RepresentationData::File(NodeId(1)),
                }];
                let native = super::super::content::target::build(crate::NativeSpec {
                    contents: Arc::new(crate::content::tests::Portable(inventory.clone())),
                    inventory,
                    files: entries.clone(),
                    file_contents: None,
                    sensitivity: crate::Sensitivity::Normal,
                })
                .unwrap();
                let (data, shared) = super::super::vfile::build_inner(
                    super::super::vfile::Spec {
                        cloud: Some(offer),
                        descriptors: super::super::files::descriptors(&entries),
                        fixed: vec![(f.preferred_drop_effect, 1u32.to_le_bytes().to_vec())],
                        contents,
                        cf_descriptor: f.file_descriptor,
                        cf_contents: f.file_contents,
                    },
                    true,
                    Some(native),
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
                    for name in ["Shell IDList Array", "FileNameW", "FileName"] {
                        let cf =
                            super::super::content::format_id(&NativeFormat::WindowsRegistered(name.into())).unwrap();
                        let f = format(cf as u32, TYMED_HGLOBAL.0 as u32);
                        assert!(data.QueryGetData(&f).is_ok());
                        let mut medium = data.GetData(&f).unwrap();
                        windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);
                    }
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
                    if missing || fail_eof {
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
                assert_eq!(metadata.len(), size);
                assert_eq!(
                    metadata
                        .modified()
                        .unwrap()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis(),
                    2000
                );
                assert_eq!(source.selected.load(Ordering::Relaxed), 7);
            }
        }
        drop(service);
        assert!(root.exists(), "backend exit preserves received files");
        cleanup_native_test_root(&root);
    }
    struct PublicationGate {
        delivery: u64,
        entered: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }
    static PUBLICATION_GATE: Mutex<Option<PublicationGate>> = Mutex::new(None);
    pub(super) fn before_population_create(delivery: u64, index: usize) -> io::Result<()> {
        let gate = PUBLICATION_GATE
            .lock()
            .unwrap()
            .as_ref()
            .filter(|gate| gate.delivery == delivery && index == 0)
            .map(|gate| (gate.entered.clone(), gate.release.clone()));
        let Some((entered, release)) = gate else {
            return Ok(());
        };
        entered.send(()).map_err(io::Error::other)?;
        let (ready, _) = release
            .1
            .wait_timeout_while(release.0.lock().unwrap(), Duration::from_secs(10), |ready| !*ready)
            .unwrap();
        if *ready {
            Ok(())
        } else {
            Err(io::ErrorKind::TimedOut.into())
        }
    }
    #[test]
    #[ignore = "private native Cloud Files tree; run in an interactive Windows session"]
    fn native_lazy_tree_population() {
        use crate::content::{
            Provider,
            linked::Descriptor,
            namespace::{Files, Namespace, NativeFiles},
        };
        use crate::model::{ContentSize, DirectoryCursor, DirectoryPage, FileNode, NodeId, NodeKind};
        let _ = tracing_subscriber::fmt().with_test_writer().with_ansi(false).try_init();
        struct Gate {
            entered: mpsc::Sender<()>,
            release: Arc<(Mutex<bool>, std::sync::Condvar)>,
        }
        struct Tree {
            nodes: Vec<FileNode>,
            gate: Option<Gate>,
        }
        impl Namespace for Tree {
            fn roots(&self) -> Vec<FileNode> {
                vec![self.nodes[0].clone()]
            }
            fn source_link(&self) -> Option<Descriptor> {
                Some(Descriptor {
                    offer: crate::content::tests::fixture().inventory().id,
                    token: crate::content::grants::LinkToken([91; 32]),
                    roots: self.roots(),
                    native: Default::default(),
                    provisional: false,
                    fulfilled: vec![],
                })
            }
            fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
                eprintln!("TREE_LIST {}", node.0);
                if node == NodeId(1)
                    && let Some(gate) = &self.gate
                {
                    gate.entered.send(()).unwrap();
                    let (ready, _) = gate
                        .release
                        .1
                        .wait_timeout_while(gate.release.0.lock().unwrap(), Duration::from_secs(10), |ready| !*ready)
                        .unwrap();
                    if !*ready {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                }
                crate::content::namespace::page(
                    node,
                    1,
                    &self
                        .nodes
                        .iter()
                        .filter(|n| n.parent == Some(node))
                        .cloned()
                        .collect::<Vec<_>>(),
                    cursor,
                )
            }
            fn select(&self, node: NodeId) -> io::Result<FileNode> {
                self.nodes
                    .iter()
                    .find(|n| n.id == node)
                    .cloned()
                    .ok_or(io::ErrorKind::NotFound.into())
            }
            fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
                assert_eq!(version, 1);
                let size = match self.select(node)?.size {
                    ContentSize::Known(n) => n,
                    _ => unreachable!(),
                };
                Ok(Box::new(io::Cursor::new(vec![b't'; (size - offset) as usize])))
            }
        }
        struct Cleanup(Arc<Service>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                *PUBLICATION_GATE.lock().unwrap() = None;
                self.0.stop();
                cleanup_native_test_root(&self.0.root);
            }
        }
        let mode = std::env::var("OINK_TREE_MODE").unwrap_or_else(|_| "download".into());
        let temp = tempfile::tempdir().unwrap();
        eprintln!("PRIVATE_TREE_ROOT {} mode={mode}", temp.path().display());
        let service = Service::new(temp.path().join("CloudFiles")).unwrap();
        let _cleanup = Cleanup(service.clone());
        let publishing = mode.ends_with("-publish");
        let concurrent = mode.ends_with("-download") || publishing;
        let (entered, started) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let tree = Arc::new(Tree {
            nodes: [
                (1, None, "tree", true, 0),
                (2, Some(1), "nested", true, 0),
                (3, Some(1), "zero.bin", false, 0),
                (4, Some(2), "child.txt", false, 24),
                (5, Some(2), "empty", true, 0),
            ]
            .into_iter()
            .map(|(id, parent, name, dir, size)| FileNode {
                exec_bits: None,
                id: NodeId(id),
                parent: parent.map(NodeId),
                name: name.into(),
                kind: if dir { NodeKind::Directory } else { NodeKind::File },
                size: ContentSize::Known(size),
                version: 1,
                modified_ms: None,
            })
            .collect(),
            gate: (concurrent && !publishing).then(|| Gate {
                entered: entered.clone(),
                release: release.clone(),
            }),
        });
        let contents = Files::new(tree, crate::sync::files::TargetOs::Windows).unwrap();
        let offer = Offer {
            service: service.clone(),
            entries: contents.roots().to_vec(),
            contents,
            created: Instant::now(),
            prepared: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            virtual_files: false,
        };
        offer.hdrop().unwrap();
        let delivery = offer.prepared.lock().unwrap().clone().unwrap();
        assert_eq!(delivery.entries().len(), 1);
        if publishing {
            *PUBLICATION_GATE.lock().unwrap() = Some(PublicationGate {
                delivery: delivery.id,
                entered,
                release: release.clone(),
            });
        }
        if mode == "download" {
            let check: crate::content::ReaderGuard = Arc::new(|| Ok(()));
            let files = delivery.download(&check, &|_, _| {}).unwrap();
            assert_eq!(files.len(), 2);
            assert_eq!(files.iter().map(|f| f.size).sum::<u64>(), 24);
        } else {
            let target = temp.path().join("destination");
            std::fs::create_dir(&target).unwrap();
            let download = concurrent.then(|| {
                let delivery = delivery.clone();
                std::thread::spawn(move || {
                    let check: crate::content::ReaderGuard = Arc::new(|| Ok(()));
                    delivery.download(&check, &|_, _| {})
                })
            });
            if concurrent {
                started.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            let marker = temp.path().join("consumer-open");
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "windows::cloud::tests::native_lazy_tree_consumer",
                    "--ignored",
                    "--nocapture",
                ])
                .env("OINK_TREE_SOURCE", delivery.file_path(0))
                .env("OINK_TREE_TARGET", &target)
                .env("OINK_TREE_MODE", &mode)
                .env("OINK_TREE_MARKER", &marker)
                .spawn()
                .unwrap();
            if concurrent {
                let deadline = Instant::now() + Duration::from_secs(5);
                let expected = if publishing {
                    marker.with_extension("attempt")
                } else {
                    marker.clone()
                };
                while !expected.exists() && Instant::now() < deadline {
                    assert!(child.try_wait().unwrap().is_none());
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(
                    expected.exists(),
                    "consumer must reach the selected population ordering"
                );
                if publishing {
                    assert!(!marker.exists(), "the protected directory must delay the consumer open");
                }
                *release.0.lock().unwrap() = true;
                release.1.notify_all();
            }
            let deadline = Instant::now() + Duration::from_secs(15);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    eprintln!("TREE_CONSUMER_TIMEOUT");
                    child.kill().unwrap();
                    break child.wait().unwrap();
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let result = download.map(|thread| thread.join().unwrap());
            assert!(status.success());
            if let Some(result) = result {
                let files = result.unwrap();
                assert_eq!(files.len(), 2);
                assert_eq!(files.iter().map(|f| f.size).sum::<u64>(), 24);
            }
            if mode != "enumerate" {
                assert_tree(&target.join("tree"));
                assert_eq!(delivery.file_path(0).exists(), mode.starts_with("copy"));
            }
        }
        assert!(
            service.received.lock().unwrap().sources[&delivery.id]
                .pending
                .is_empty()
        );
        if !mode.starts_with("move") && mode != "enumerate" {
            assert_tree(&delivery.file_path(0));
        }
        fn assert_tree(path: &Path) {
            assert_eq!(std::fs::read(path.join("nested/child.txt")).unwrap(), vec![b't'; 24]);
            assert_eq!(std::fs::read(path.join("zero.bin")).unwrap(), Vec::<u8>::new());
            assert_eq!(std::fs::read_dir(path.join("nested/empty")).unwrap().count(), 0);
            assert_eq!(std::fs::read_dir(path).unwrap().count(), 2);
            assert_eq!(std::fs::read_dir(path.join("nested")).unwrap().count(), 2);
        }
    }
    #[test]
    #[ignore = "child of native_lazy_tree_population"]
    fn native_lazy_tree_consumer() {
        let source = std::env::var("OINK_TREE_SOURCE").unwrap().replace('/', "\\");
        eprintln!("TREE_CONSUMER_SOURCE {source}");
        let target = std::env::var("OINK_TREE_TARGET").unwrap().replace('/', "\\");
        if std::env::var("OINK_TREE_MODE").unwrap() == "enumerate" {
            fn visit(path: &Path) {
                eprintln!("TREE_ENUM {}", path.display());
                for entry in std::fs::read_dir(path).unwrap() {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_dir() {
                        visit(&entry.path());
                    }
                }
            }
            visit(Path::new(&source));
            return;
        }
        let mode = std::env::var("OINK_TREE_MODE").unwrap();
        use std::os::windows::fs::OpenOptionsExt;
        let _held = (mode.ends_with("-download") || mode.ends_with("-publish")).then(|| {
            let marker = PathBuf::from(std::env::var_os("OINK_TREE_MARKER").unwrap());
            std::fs::write(marker.with_extension("attempt"), b"attempt").unwrap();
            let file = std::fs::OpenOptions::new()
                .access_mode(if mode.starts_with("move") {
                    DELETE | FILE_READ_ATTRIBUTES
                } else {
                    FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES
                })
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&source)
                .unwrap();
            std::fs::write(std::env::var_os("OINK_TREE_MARKER").unwrap(), b"open").unwrap();
            file
        });
        let destination = Path::new(&target).join("tree");
        if mode.starts_with("move") {
            // SAFETY: both NUL-terminated paths remain live for this native move.
            let moved = unsafe { MoveFileExW(wide(&source).as_ptr(), wide(&destination).as_ptr(), 0) };
            assert_ne!(moved, 0, "external tree MOVE: {}", io::Error::last_os_error());
        } else {
            fn copy_tree(source: &Path, target: &Path) {
                std::fs::create_dir(target).unwrap();
                for entry in std::fs::read_dir(source).unwrap() {
                    let entry = entry.unwrap();
                    let target = target.join(entry.file_name());
                    if entry.file_type().unwrap().is_dir() {
                        copy_tree(&entry.path(), &target);
                    } else {
                        std::fs::copy(entry.path(), target).unwrap();
                    }
                }
            }
            copy_tree(Path::new(&source), &destination);
        }
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
    #[ignore = "requires native Cloud Files on NTFS; uses only a private non-GUI sync root"]
    #[cfg(debug_assertions)]
    fn native_received_package_reader_waits_for_own_and_concurrent_hydration() {
        use std::sync::Condvar;
        let _ = tracing_subscriber::fmt().with_test_writer().with_ansi(false).try_init();
        struct Source {
            size: usize,
            fail: bool,
            entered: mpsc::Sender<()>,
            release: Arc<(Mutex<bool>, Condvar)>,
        }
        impl FileContents for Source {
            fn open(&self, _: usize) -> io::Result<crate::FileReader> {
                let _ = self.entered.send(());
                let (ready, wake) = &*self.release;
                let (ready, _) = wake
                    .wait_timeout_while(ready.lock().unwrap(), Duration::from_secs(10), |ready| !*ready)
                    .unwrap();
                if !*ready {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                if self.fail {
                    return Err(crate::content::unavailable(crate::model::Unavailable::ReadFailed));
                }
                Ok(Box::new(io::Cursor::new(vec![b'v'; self.size])))
            }
        }
        struct Cleanup(Arc<Service>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.stop();
                cleanup_native_test_root(&self.0.root);
            }
        }
        let temp = tempfile::tempdir().unwrap();
        eprintln!("PRIVATE_PACKAGE_ROOT {}", temp.path().display());
        let service = Service::new(temp.path().join("CloudFiles")).unwrap();
        let _cleanup = Cleanup(service.clone());
        for (com, concurrent, fail, size) in [
            (false, false, false, 131073),
            (false, true, false, 31_187_403),
            (false, false, true, 65537),
            (true, true, false, 39592),
            (true, true, false, 237820),
            (true, false, true, 65537),
        ] {
            let (entered, started) = mpsc::channel();
            let release = Arc::new((Mutex::new(false), Condvar::new()));
            let contents = Arc::new(Source {
                size,
                fail,
                entered,
                release: release.clone(),
            });
            let delivery = service
                .publish(vec![entry("dummy.msi", size as u64, false)], contents, Instant::now())
                .unwrap();
            // This private catalog gives the production path resolver its node
            // identity; bytes are synthetic and never reach package inspection.
            reader_catalog(&service, &delivery);
            let path = delivery.file_path(0);
            let error = std::fs::File::open(&path).unwrap().read(&mut [0; 1]).unwrap_err();
            eprintln!(
                "SELF_IMPLICIT_READ native_code={:?} concurrent={concurrent} terminal_failure={fail}",
                error.raw_os_error()
            );
            assert_eq!(error.raw_os_error(), Some(ERROR_CLOUD_FILE_ACCESS_DENIED as i32));
            assert!(
                started.try_recv().is_err(),
                "blocked implicit read must not enter our provider"
            );
            let background = concurrent.then(|| {
                let provider = service.clone();
                let native = delivery.clone();
                let target = path.clone();
                // Match the drop's existing active-download hydration, which
                // can start before the package/COM consumer opens its reader.
                std::thread::spawn(move || {
                    let _pin = native::ReaderPin::new(&native)?;
                    let file = native.checked_file(&provider, 0, &target, true)?;
                    native.hydrate_native(&provider, 0, &file, &|| Ok(()))
                })
            });
            if concurrent {
                started.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            let (send, receive) = mpsc::channel();
            let provider = service.clone();
            let worker = std::thread::spawn(move || {
                let result = (|| {
                    let bytes = if com {
                        struct NativeContents(Arc<Service>, PathBuf);
                        impl FileContents for NativeContents {
                            fn open(&self, _: usize) -> io::Result<crate::FileReader> {
                                native::open_path_in(&self.0, &self.1).unwrap()
                            }
                        }
                        crate::windows::vfile::tests::read_native_contents(
                            Arc::new(NativeContents(provider, path)),
                            size as u64,
                        )?
                    } else {
                        let mut reader = native::open_path_in(&provider, &path).unwrap()?;
                        let mut bytes = Vec::new();
                        reader.read_to_end(&mut bytes)?;
                        bytes
                    };
                    Ok::<_, io::Error>((bytes.len(), bytes.iter().all(|byte| *byte == b'v')))
                })();
                let _ = send.send(result);
            });
            let early = receive.recv_timeout(Duration::from_millis(250));
            // Always release the callback and join readers before asserting, so
            // the negative pre-fix run also unregisters its private sync root.
            *release.0.lock().unwrap() = true;
            release.1.notify_all();
            let pending = matches!(early, Err(mpsc::RecvTimeoutError::Timeout));
            let result = early.unwrap_or_else(|_| {
                receive.recv_timeout(Duration::from_secs(15)).unwrap_or_else(|_| {
                    eprintln!(
                        "PACKAGE_WAIT_TIMEOUT hydrated={} requests={}",
                        delivery.file(0).hydrated.load(Ordering::Acquire),
                        service.requests.lock().unwrap().len()
                    );
                    service.stop();
                    receive.recv_timeout(Duration::from_secs(5)).unwrap()
                })
            });
            worker.join().unwrap();
            if let Some(background) = background {
                background.join().unwrap().unwrap();
            }
            eprintln!(
                "EXPLICIT_PACKAGE_READ com={com} pending={pending} concurrent={concurrent} terminal_failure={fail} result={result:?}"
            );
            assert!(pending, "package reader must wait for explicit hydration");
            if fail {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap(), (size, true));
            }
        }
    }
    #[test]
    #[ignore = "child process for private non-GUI move fixture"]
    fn native_move_worker() {
        let Some(source) = std::env::var_os("OINK_PRIVATE_MOVE_SOURCE") else {
            return;
        };
        let target = std::env::var_os("OINK_PRIVATE_MOVE_TARGET").unwrap();
        let source = PathBuf::from(source);
        let target = PathBuf::from(target);
        assert!(source.starts_with(std::env::temp_dir()) && target.starts_with(std::env::temp_dir()));
        // SAFETY: private absolute source/destination paths on the same volume.
        let result = if unsafe { MoveFileExW(wide(&source).as_ptr(), wide(&target).as_ptr(), 0) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        eprintln!("PRIVATE_MOVE_CHILD result={result:?}");
        if std::env::var("OINK_PRIVATE_MOVE_FAIL").as_deref() == Ok("true") {
            assert_eq!(
                result.unwrap_err().raw_os_error(),
                Some(ERROR_CLOUD_FILE_UNSUCCESSFUL as i32)
            );
        } else {
            result.unwrap();
        }
    }
    #[test]
    #[ignore = "requires native Cloud Files on NTFS; uses only a private non-GUI sync root"]
    fn native_move_during_hydration() {
        let _ = tracing_subscriber::fmt().with_test_writer().with_ansi(false).try_init();
        struct Cleanup(Arc<Service>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.stop();
                cleanup_native_test_root(&self.0.root);
            }
        }
        struct Gated {
            entries: Vec<FileEntry>,
            fail: bool,
            entered: mpsc::Sender<()>,
            release: Arc<(Mutex<bool>, std::sync::Condvar)>,
        }
        impl FileContents for Gated {
            fn placeholder_metadata(&self, index: usize) -> io::Result<Option<(FileMetadata, u64)>> {
                Ok(Some((
                    FileMetadata {
                        exec_bits: None,
                        size: self.entries[index].size,
                        modified_ms: Some(1000),
                    },
                    1,
                )))
            }
            fn open(&self, index: usize) -> io::Result<crate::FileReader> {
                let _ = self.entered.send(());
                let (ready, _) = self
                    .release
                    .1
                    .wait_timeout_while(self.release.0.lock().unwrap(), Duration::from_secs(10), |ready| !*ready)
                    .unwrap();
                if !*ready {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                if self.fail {
                    return Err(crate::content::unavailable(crate::model::Unavailable::ReadFailed));
                }
                Ok(Box::new(io::Cursor::new(vec![b'm'; self.entries[index].size as usize])))
            }
        }
        // Session-0 implicit reads are denied by Cloud Filter. Explicitly start
        // provider hydration for every file before the independent mover, matching an active
        // drop download without requiring a desktop or changing provider flags.
        for (folder, fail) in [(false, false), (true, false), (false, true), (true, true)] {
            let temp = tempfile::tempdir().unwrap();
            eprintln!("PRIVATE_MOVE_ROOT {}", temp.path().display());
            let service = Service::new(temp.path().join("CloudFiles")).unwrap();
            let cleanup = Cleanup(service.clone());
            let (entered, started) = mpsc::channel();
            let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
            let entries = if folder {
                vec![
                    entry("folder", 0, true),
                    entry("folder/first.bin", 179_200, false),
                    entry("folder/sub", 0, true),
                    entry("folder/sub/second.bin", 6148, false),
                ]
            } else {
                vec![entry("move.bin", 179_200, false)]
            };
            let delivery = service
                .publish(
                    entries.clone(),
                    Arc::new(Gated {
                        entries,
                        fail,
                        entered,
                        release: release.clone(),
                    }),
                    Instant::now(),
                )
                .unwrap();
            reader_catalog(&service, &delivery);
            let path = delivery.file_path(0);
            let target = temp.path().join("moved.bin");
            let native = delivery.clone();
            let mut hydration = vec![std::thread::spawn(move || {
                let check: crate::content::ReaderGuard = Arc::new(|| Ok(()));
                crate::content::namespace::NativeFiles::download(&*native, &check, &|_, _| {}).map(|files| {
                    assert_eq!(files.len(), if folder { 2 } else { 1 });
                    assert!(files.iter().all(|file| file.version == 1));
                    assert_eq!(
                        files.iter().map(|file| file.size).sum::<u64>(),
                        if folder { 185_348 } else { 179_200 }
                    );
                })
            })];
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            if folder {
                // Session 0 cannot implicitly hydrate the other descendant.
                // Start that request explicitly while the real whole-delivery
                // downloader is blocked on the first file, then let MOVE join.
                let provider = service.clone();
                let native = delivery.clone();
                hydration.push(std::thread::spawn(move || {
                    let file = native.checked_file(&provider, 3, &native.file_path(3), true)?;
                    native.hydrate_native(&provider, 3, &file, &|| Ok(()))
                }));
                started.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            let source = path.clone();
            let destination = target.clone();
            let (send, receive) = mpsc::channel();
            let mover = std::thread::spawn(move || {
                let result = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "windows::cloud::tests::native_move_worker",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("OINK_PRIVATE_MOVE_SOURCE", &path)
                    .env("OINK_PRIVATE_MOVE_TARGET", &destination)
                    .env("OINK_PRIVATE_MOVE_FAIL", fail.to_string())
                    .status()
                    .and_then(|status| {
                        if status.success() {
                            Ok(())
                        } else {
                            Err(io::Error::other("move child failed"))
                        }
                    });
                let _ = send.send(result);
            });
            let early = receive.recv_timeout(Duration::from_millis(250));
            let pending = matches!(early, Err(mpsc::RecvTimeoutError::Timeout));
            *release.0.lock().unwrap() = true;
            release.1.notify_all();
            let moved = early.unwrap_or_else(|_| receive.recv_timeout(Duration::from_secs(15)).unwrap());
            mover.join().unwrap();
            let hydrated = hydration
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>();
            eprintln!(
                "PRIVATE_MOVE folder={folder} fail={fail} pending={pending} outcome={moved:?} hydration={hydrated:?}"
            );
            let source_exists = source.exists();
            drop(cleanup);
            moved.unwrap();
            assert!(pending, "move must wait for gated source bytes");
            for result in hydrated {
                assert_eq!(result.is_err(), fail);
            }
            if fail {
                assert!(
                    !target.exists(),
                    "failed hydration must not expose an incomplete destination"
                );
                assert!(source_exists, "failed move must preserve the received identity");
            } else {
                assert!(!source_exists);
                let first = if folder {
                    target.join("first.bin")
                } else {
                    target.clone()
                };
                assert_eq!(std::fs::read(first).unwrap(), vec![b'm'; 179_200]);
                if folder {
                    assert_eq!(std::fs::read(target.join("sub/second.bin")).unwrap(), vec![b'm'; 6148]);
                }
            }
        }
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
                .any(|d| d.files().iter().any(|file| file.opens.load(Ordering::Acquire) > 0));
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
