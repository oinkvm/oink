//! System File Provider catalogs and an authenticated, read-only loopback content bridge.
//! The separate signed manager resolves URLs without marking the Oink process dataless-I/O exempt.
use crate::FileProviderReason;
use crate::model::{ContentSize, DirectoryCursor, FileNode, NodeId, NodeKind};
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::content::namespace::{Flat, Names, Namespace};
use oink_foundation::content::ClipId;
use serde::{Deserialize, Serialize};

use crate::{ClipboardError, FileContents, FileEntry};

mod announced;
pub use announced::{
    Activation, PreparationKey, PreparedDrag, activate_prepared, prepare_announced, prepare_announced_with_cancel,
    revoke_prepared, try_prepare_enter_placeholder,
};

mod preparation_cancel;
pub use preparation_cancel::PreparationCancellation;

mod launch;
mod observer;
mod placeholders;
mod spare;
mod storage_watch;
use crate::provider_preparation::PreparationTiming;
pub use observer::Observer;
pub use placeholders::{configure_placeholder_pool, placeholder_pool_status, placeholder_status};

const MANAGER_TIMEOUT: Duration = Duration::from_secs(12);
struct Batch {
    urls: Mutex<Vec<PathBuf>>,
    token: String,
    contents: Mutex<Arc<dyn Namespace>>,
    state: Mutex<CatalogState>,
    catalog: PathBuf,
    readers: Mutex<usize>,
}
// Watch only this reader. Closing a Finder stream cannot cancel the independent
// whole-drop transfer or readers retained by another native consumer.
struct ReadWatch {
    batch: Arc<Batch>,
    done: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    socket: TcpStream,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl ReadWatch {
    fn new(socket: &TcpStream, batch: Arc<Batch>) -> io::Result<Self> {
        let control = socket.try_clone()?;
        let mut monitor = socket.try_clone()?;
        let done = Arc::new(AtomicBool::new(false));
        let finished = done.clone();
        let connected = Arc::new(AtomicBool::new(true));
        let live = connected.clone();
        let watched = batch.clone();
        *batch.readers.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        let worker = std::thread::Builder::new()
            .name("oink-provider-cancel".into())
            .spawn(move || {
                loop {
                    if finished.load(Ordering::Acquire) {
                        return;
                    }
                    match monitor.read(&mut [0u8; 1]) {
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                            ) =>
                        {
                            continue;
                        }
                        _ => break,
                    }
                }
                release_reader(&watched, &live);
            });
        match worker {
            Ok(worker) => Ok(Self {
                batch,
                done,
                connected,
                socket: control,
                worker: Some(worker),
            }),
            Err(e) => {
                *batch.readers.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
                Err(e)
            }
        }
    }
}
fn release_reader(batch: &Batch, connected: &AtomicBool) {
    let mut active = batch.readers.lock().unwrap_or_else(|e| e.into_inner());
    if connected.swap(false, Ordering::AcqRel) {
        *active -= 1;
    }
}
impl Drop for ReadWatch {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        let _ = self.socket.shutdown(Shutdown::Both);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        release_reader(&self.batch, &self.connected);
    }
}
struct Bridge {
    port: u16,
    batches: Mutex<HashMap<String, Arc<Batch>>>,
}
/// A target-local publication. Completed bytes have bounded cache retention;
/// this owner pin protects consumption-based eviction, while only active reads
/// can defer the absolute expiry. Cache clear retires inactive registrations.
#[derive(Clone)]
pub struct Registration {
    key: String,
    bridge: Weak<Bridge>,
    batch: Arc<Batch>,
    entries: Vec<FileEntry>,
    _content_pin: Option<crate::store::Batch>,
    admission: Option<Arc<announced::Admission>>,
}
impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("id", &self.key)
            .field("entries", &self.entries.len())
            .finish()
    }
}
impl Registration {
    pub fn entries(&self) -> &[FileEntry] {
        &self.entries
    }
    pub fn top_level_urls(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .filter(|e| super::files::is_top_level(e))
            .filter_map(|e| e.local_path.clone())
            .collect()
    }
    /// Cancel an unused drag or failed pasteboard write. An already open stream keeps its batch alive.
    pub fn cancel(&self) {
        if let Some(admission) = &self.admission {
            admission.revoke();
        }
        if let Some(bridge) = self.bridge.upgrade() {
            bridge
                .batches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.key);
        }
        // Registration owns this locally generated catalog path; retiring it
        // does not require looking up or reinitializing the process container.
        retire_catalog(&self.batch, &|| {
            if let Some(root) = self.batch.catalog.parent() {
                remove_catalog_in(root, &self.batch.catalog);
            }
        });
        signal_changes();
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    id: String,
    parent: String,
    name: String,
    directory: bool,
    size: Option<u64>,
    modified: Option<i64>,
    node: Option<u64>,
    version: u64,
    children: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hidden: Option<bool>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    #[serde(deserialize_with = "crate::store::schema_one")]
    schema: u8,
    token: String,
    port: u16,
    records: Vec<Record>,
    batch: Option<crate::store::BatchId>,
    urls: Vec<PathBuf>,
    #[serde(default)]
    received_at: Option<u64>,
}
struct CatalogState {
    catalog: Catalog,
    names: Names,
    aliases: HashMap<NodeId, String>,
    retired: bool,
}
impl CatalogState {
    fn persist(&self, path: &std::path::Path) -> io::Result<()> {
        // An already-open directory reader may finish after cancellation. Its
        // result stays usable by that reader but must not resurrect a WS deletion.
        if self.retired {
            Ok(())
        } else {
            save_catalog(path, &self.catalog)
        }
    }
}
fn retire_catalog(batch: &Batch, remove: &dyn Fn()) {
    let mut state = batch.state.lock().unwrap();
    state.retired = true;
    remove();
}
#[derive(Deserialize)]
#[serde(tag = "operation", deny_unknown_fields)]
enum Request {
    #[serde(rename = "read")]
    Read { token: String, batch: String, node: u64 },
    #[serde(rename = "list")]
    List {
        token: String,
        batch: String,
        node: u64,
        cursor: Option<DirectoryCursor>,
    },
}
#[derive(Serialize)]
struct Listing {
    records: Vec<Record>,
    next: Option<DirectoryCursor>,
    version: u64,
}
static DIAGNOSTIC: Mutex<FileProviderReason> = Mutex::new(FileProviderReason::DomainEnabled);
static SPARE: Mutex<spare::Pool> = Mutex::new(spare::Pool::new());
static DOMAIN_USABLE: AtomicBool = AtomicBool::new(false);
static STORAGE: Mutex<()> = Mutex::new(());
static BRIDGE: OnceLock<Result<Arc<Bridge>, String>> = OnceLock::new();
static CONTAINER: OnceLock<PathBuf> = OnceLock::new();
static DATA: OnceLock<PathBuf> = OnceLock::new();
static REQUESTED: AtomicBool = AtomicBool::new(false);
static DEMAND_CHANGED: tokio::sync::Notify = tokio::sync::Notify::const_new();
static STARTUP: launch::LaunchPreparation = launch::LaunchPreparation::new();

/// Close the provider gate before clipboard construction; native cleanup may
/// then run alongside IPC and window startup without adopting the stale domain.
pub fn begin_launch_preparation() {
    STARTUP.begin();
}
pub fn finish_launch_preparation(success: bool) {
    if !success {
        block_after_startup_failure();
        return;
    }
    STARTUP.finish(true);
    DEMAND_CHANGED.notify_one();
}
/// A failed preparation stays blocked until a new process retries recovery.
pub fn block_after_startup_failure() {
    *DIAGNOSTIC.lock().unwrap_or_else(|e| e.into_inner()) = FileProviderReason::RegistrationFailed;
    STARTUP.finish(false);
    DEMAND_CHANGED.notify_one();
}
pub fn launch_preparation_pending() -> bool {
    STARTUP.result().is_none()
}
fn check_launch_preparation() -> Result<(), ClipboardError> {
    match STARTUP.result() {
        Some(true) => Ok(()),
        None => Err(ClipboardError::FileProvider(FileProviderReason::Checking)),
        Some(false) => Err(ClipboardError::FileProvider(FileProviderReason::RegistrationFailed)),
    }
}

pub fn demand_changed() -> &'static tokio::sync::Notify {
    &DEMAND_CHANGED
}

/// Recover the authenticated bridge before any new paste/drag is published.
/// This only queries the existing container; it never enables a system domain.
pub(crate) fn initialize(data: PathBuf) {
    if DATA.set(data).is_err() {
        return;
    }
    std::thread::spawn(|| {
        if !STARTUP.wait() {
            return;
        }
        if let Ok(root) = container(Instant::now() + MANAGER_TIMEOUT) {
            let _storage = STORAGE.lock().unwrap_or_else(|e| e.into_inner());
            if bridge(&root).is_ok() {
                signal_changes();
            }
        }
        placeholders::recover();
        start_spare();
    });
}

/// Whether this process has attempted to publish remote files (warm-up does not count).
pub fn request_files() {
    if !REQUESTED.swap(true, Ordering::AcqRel) {
        DEMAND_CHANGED.notify_one();
    }
}
pub fn was_requested() -> bool {
    REQUESTED.load(Ordering::Acquire)
}

/// Actual system domain flags. Inspecting them never registers a domain or opens a window.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainStatus {
    pub id: String,
    pub hidden: bool,
    pub user_enabled: bool,
    pub disconnected: bool,
}

pub fn inspect() -> Result<Option<DomainStatus>, ClipboardError> {
    let json = manager(&["inspect"], Instant::now() + Duration::from_secs(3))?;
    let domains: Vec<DomainStatus> = serde_json::from_str(&json).map_err(error)?;
    Ok(domains.into_iter().find(|d| d.id == "oink-files"))
}

/// Open only Oink's hidden root in Finder. The system Enable button is left to the user.
pub fn open_enable() -> Result<(), ClipboardError> {
    manager(&["open-enable"], Instant::now() + MANAGER_TIMEOUT).map(|_| ())
}
/// Most recent registration/initialization result, suitable for diagnostics (never contains tokens).
pub fn diagnostic() -> String {
    publication_reason().detail()
}
/// A real domain transition invalidates the previous publication result.
pub fn domain_changed() {
    if STARTUP.result() == Some(true) {
        *DIAGNOSTIC.lock().unwrap_or_else(|e| e.into_inner()) = FileProviderReason::DomainEnabled;
    }
}
pub fn publication_reason() -> FileProviderReason {
    if launch_preparation_pending() {
        return FileProviderReason::Checking;
    }
    *DIAGNOSTIC.lock().unwrap_or_else(|e| e.into_inner())
}
pub fn failure_reason(error: &ClipboardError) -> FileProviderReason {
    match error {
        ClipboardError::FileProvider(reason) => *reason,
        _ => FileProviderReason::RegistrationFailed,
    }
}
fn record_result<T>(result: &Result<T, ClipboardError>) {
    let reason = match result {
        Ok(_) => FileProviderReason::UrlsReady,
        Err(e) => failure_reason(e),
    };
    // Cancellation describes the gesture, not the extension's availability.
    if reason != FileProviderReason::Cancelled {
        *DIAGNOSTIC.lock().unwrap_or_else(|e| e.into_inner()) = reason;
        DEMAND_CHANGED.notify_one();
    }
}
fn error(e: impl std::fmt::Display) -> ClipboardError {
    ClipboardError::Platform(format!("Oink 文件扩展：{e}"))
}
fn manager(args: &[&str], deadline: Instant) -> Result<String, ClipboardError> {
    let _update_activity = oink_foundation::maintenance::process()
        .enter(oink_foundation::maintenance::ActivityKind::FileProvider)
        .ok_or_else(|| error("update maintenance"))?;

    manager_while(args, deadline, &|| true)
}
fn check_publication(deadline: Instant, current: &dyn Fn() -> bool) -> Result<(), ClipboardError> {
    if !current() {
        return Err(ClipboardError::FileProvider(FileProviderReason::Cancelled));
    }
    if Instant::now() >= deadline {
        return Err(ClipboardError::FileProvider(FileProviderReason::BoundExpired));
    }
    Ok(())
}
fn manager_while(args: &[&str], deadline: Instant, current: &dyn Fn() -> bool) -> Result<String, ClipboardError> {
    check_launch_preparation()?;
    check_publication(deadline, current)?;
    let exe = std::env::current_exe().map_err(error)?;
    let path = exe
        .parent()
        .ok_or_else(|| error("missing executable directory"))?
        .join("oink-file-provider");
    if !path.is_file() {
        return Err(ClipboardError::FileProvider(FileProviderReason::ExtensionNotRegistered));
    }
    let mut command = Command::new(path);
    command.args(args);
    run_manager(command, deadline, current)
}
fn run_manager(mut command: Command, deadline: Instant, current: &dyn Fn() -> bool) -> Result<String, ClipboardError> {
    check_publication(deadline, current)?;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(error)?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || {
        let mut b = Vec::new();
        stdout.take(16 * 1024 * 1024).read_to_end(&mut b).map(|_| b)
    });
    let err = std::thread::spawn(move || {
        let mut b = Vec::new();
        stderr.take(64 * 1024).read_to_end(&mut b).map(|_| b)
    });
    let status = loop {
        if let Err(error) = check_publication(deadline, current) {
            let _ = child.kill();
            let _ = child.wait();
            break Err(error);
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error(e));
            }
        }
    };
    let stdout = out
        .join()
        .map_err(|_| error("manager output thread failed"))?
        .map_err(error)?;
    let _stderr = err
        .join()
        .map_err(|_| error("manager error thread failed"))?
        .map_err(error)?;
    // Helper output can contain user paths and bridge capabilities. Never log it.
    let status = status?;
    if !status.success() {
        return Err(ClipboardError::FileProvider(match status.code() {
            Some(2) => FileProviderReason::BoundExpired,
            Some(3) => FileProviderReason::DomainDisabled,
            Some(4) => FileProviderReason::ExtensionNotRegistered,
            Some(5) => FileProviderReason::DomainDisconnected,
            _ => FileProviderReason::RegistrationFailed,
        }));
    }
    String::from_utf8(stdout).map(|s| s.trim().to_owned()).map_err(error)
}
fn container(deadline: Instant) -> Result<PathBuf, ClipboardError> {
    container_while(deadline, &|| true)
}
fn container_while(deadline: Instant, current: &dyn Fn() -> bool) -> Result<PathBuf, ClipboardError> {
    check_launch_preparation()?;
    if let Some(root) = CONTAINER.get() {
        return Ok(root.clone());
    }
    let root = PathBuf::from(manager_while(&["container"], deadline, current)?);
    let _ = CONTAINER.set(root.clone());
    watch_storage(&root);
    Ok(root)
}
/// Register/inspect the hidden domain off the input and UI threads. Never handles an authorization UI.
pub(crate) fn warm_up() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::spawn(|| {
            if !STARTUP.wait() {
                return;
            }
            let result = container(Instant::now() + MANAGER_TIMEOUT)
                .and_then(|_| manager(&["initialize"], Instant::now() + MANAGER_TIMEOUT));
            // Domain enablement is not evidence that a new item's URL is ready.
            if let Err(e) = result {
                tracing::info!(%e, "File Provider initialization unavailable");
            }
        });
    });
}
fn signal_changes() {
    std::thread::spawn(|| {
        let _ = manager(&["signal"], Instant::now() + MANAGER_TIMEOUT);
    });
}
// Pool counts are sampled immediately before consuming this drag's spare.
fn record_spare_status(status: spare::Status, epoch: Option<u64>) {
    if let Some(epoch) = epoch {
        tracing::debug!(
            dnd_phase = 49_u64,
            epoch,
            size = status.capacity as u64,
            count = status.ready as u64,
            active = status.worker,
            known = status.preparing,
            "File Provider spare pool before registration"
        );
    } else {
        tracing::debug!(
            dnd_phase = 49_u64,
            size = status.capacity as u64,
            count = status.ready as u64,
            active = status.worker,
            known = status.preparing,
            "File Provider spare pool before announcement preparation"
        );
    }
}
fn record_placeholder_path(timing: &PreparationTiming, trace: &PublicationTrace) {
    let p = &timing.placeholder;
    tracing::debug!(
        dnd_phase = 50_u64,
        preparation_generation = trace.generation,
        placeholder_generation = trace.stock_generation,
        provider_preparation = trace.kind(),
        epoch = trace.epoch(),
        provider_path = if p.used {
            2_u64
        } else if timing.parent_visible_us == Some(0) {
            0
        } else {
            1
        },
        placeholder_enabled = p.enabled,
        placeholder_used = p.used,
        placeholder_ready = u64::from(p.ready_parents),
        placeholder_preparing = p.preparing,
        placeholder_prepared_files = u64::from(p.prepared_files),
        placeholder_prepared_folders = u64::from(p.prepared_folders),
        placeholder_used_files = u64::from(p.used_files),
        placeholder_used_folders = u64::from(p.used_folders),
        placeholder_prepare_us = p.preparation_us,
        placeholder_final_us = p.final_urls_us,
        placeholder_fallback = p.fallback as u64,
        "File Provider namespace path"
    );
}
fn record_placeholder_refill(
    generation: u64,
    event: u64,
    elapsed_us: u64,
    blocked: u64,
    ready: usize,
    failure: Option<FileProviderReason>,
) {
    tracing::debug!(
        dnd_phase = 52_u64,
        placeholder_generation = generation,
        placeholder_refill = event,
        placeholder_blocked = blocked,
        elapsed_us,
        count = ready as u64,
        size = placeholders::PARENTS as u64,
        success = failure.is_none(),
        reason = failure.map(|r| r as u64),
        "File Provider placeholder refill"
    );
}
fn record_placeholder_cleanup(pending: usize, failure: Option<FileProviderReason>) {
    tracing::debug!(
        dnd_phase = 51_u64,
        count = pending as u64,
        success = failure.is_none(),
        reason = failure.map(|r| r as u64),
        "File Provider placeholder cleanup"
    );
}
pub fn spare_status() -> PreparationTiming {
    let mut timing = PreparationTiming::default();
    SPARE.lock().unwrap().observe(&mut timing);
    timing.placeholder = placeholder_status();
    timing
}
pub fn domain_usable() {
    DOMAIN_USABLE.store(true, Ordering::Release);
    start_spare();
    placeholders::retry_retirements();
    placeholders::start();
}
fn start_spare() {
    if !DOMAIN_USABLE.load(Ordering::Acquire) || !matches!(BRIDGE.get(), Some(Ok(_))) {
        return;
    }
    if SPARE.lock().unwrap().start() {
        std::thread::spawn(|| while prepare_spare() {});
    }
}
fn monotonic_ns() -> u64 {
    // Shared across helper/extension/process, unlike per-process Instant origins.
    unsafe extern "C" {
        fn clock_gettime_nsec_np(clock: libc::clockid_t) -> u64;
    }
    // SAFETY: Darwin accepts this public clock ID; the call takes no pointers.
    unsafe { clock_gettime_nsec_np(libc::CLOCK_UPTIME_RAW) }
}
struct EmptyContents;
impl FileContents for EmptyContents {
    fn open(&self, _: usize) -> io::Result<crate::FileReader> {
        Err(io::ErrorKind::NotFound.into())
    }
}
fn prepare_spare() -> bool {
    let started = Instant::now();
    let mut created = None;
    let mut result = (|| {
        let root = CONTAINER.get().ok_or_else(|| error("container unavailable"))?;
        let bridge = bridge(root)?;
        let key = random().map_err(error)?;
        let token = random().map_err(error)?;
        let path = root.join(format!("{key}.json"));
        {
            let _storage = STORAGE.lock().unwrap_or_else(|e| e.into_inner());
            SPARE.lock().unwrap().preparing(key.clone());
            created = Some(path.clone());
            let empty = Flat::new(&[], Arc::new(EmptyContents)).map_err(error)?;
            let (batch, _) = prepare_batch(empty, &key, token.clone(), bridge.port, path.clone()).map_err(error)?;
            save_catalog(&path, &batch.state.lock().unwrap().catalog).map_err(error)?;
        }
        // The helper waits for a coordinated empty-directory read after the
        // public download-request ACK. No foreground drag waits for this worker.
        manager(&["prepare-spare", &key], Instant::now() + MANAGER_TIMEOUT)?;
        Ok(spare::Spare {
            key,
            token,
            preparation_us: started.elapsed().as_micros() as u64,
        })
    })();
    let _storage = STORAGE.lock().unwrap_or_else(|e| e.into_inner());
    if result.is_err()
        && let Some(path) = created
    {
        remove_catalog(&path);
        signal_changes();
    }
    let elapsed_us = started.elapsed().as_micros() as u64;
    if let Ok(spare) = &mut result {
        spare.preparation_us = elapsed_us;
    }
    let again = SPARE
        .lock()
        .unwrap()
        .finish(result.map_err(|e| failure_reason(&e)), elapsed_us);
    if !again {
        placeholders::start();
    }
    again
}
fn random() -> io::Result<String> {
    let mut bytes = [0; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn bridge(root: &std::path::Path) -> Result<Arc<Bridge>, ClipboardError> {
    BRIDGE
        .get_or_init(|| {
            let data = DATA.get().ok_or_else(|| "content store not configured".to_owned())?;
            let listener = Arc::new(TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?);
            let bridge = Arc::new(Bridge {
                port: listener.local_addr().map_err(|e| e.to_string())?.port(),
                batches: Mutex::new(HashMap::new()),
            });
            *bridge.batches.lock().unwrap() = recover_catalogs(data, root, bridge.port).map_err(|e| e.to_string())?;
            // Bounded worker pool: untrusted clients cannot spawn unbounded threads or ask for paths.
            for _ in 0..4 {
                let listener = listener.clone();
                let weak = Arc::downgrade(&bridge);
                std::thread::Builder::new()
                    .name("oink-file-provider".into())
                    .spawn(move || {
                        for stream in listener.incoming().flatten() {
                            let Some(bridge) = weak.upgrade() else { break };
                            if let Err(e) = serve(stream, &bridge) {
                                tracing::debug!(%e, "File Provider content request ended");
                            }
                        }
                    })
                    .map_err(|e| e.to_string())?;
            }
            Ok(bridge)
        })
        .clone()
        .map_err(error)
}
// Startup never reuses a previous process's empty spare. Removing its catalog
// causes the ordinary working-set delta to delete the parent after bridge recovery.
fn recover_catalogs(
    data: &std::path::Path,
    root: &std::path::Path,
    port: u16,
) -> io::Result<HashMap<String, Arc<Batch>>> {
    let mut batches = HashMap::new();
    for key in placeholders::ledger_keys(root) {
        remove_catalog_in(root, &root.join(format!("{key}.json")));
    }
    for entry in std::fs::read_dir(root)?.flatten() {
        let path = entry.path();
        let Some(key) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        else {
            continue;
        };
        if path
            .extension()
            .is_some_and(|e| e == "enumerated" || e == "tmp" || e == "placeholder-tmp")
        {
            let _ = std::fs::remove_file(&path);
        } else if path.extension().is_some_and(|e| e == "json") {
            let _ = std::fs::remove_file(path.with_extension("tmp"));
            match restore_catalog(data, &path, key, port) {
                Ok(batch) => {
                    batches.insert(key.into(), batch);
                }
                Err(_) => remove_catalog_in(root, &path),
            }
        }
    }
    Ok(batches)
}
fn save_catalog(path: &std::path::Path, catalog: &Catalog) -> io::Result<()> {
    let bytes = serde_json::to_vec(catalog)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    let part = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&part)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        crate::store::replace(&part, path)?;
        crate::store::sync_directory(
            path.parent()
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
        )
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(part);
    }
    result
}
fn node_record(batch: &str, node: &FileNode, names: &mut Names) -> io::Result<Record> {
    node_record_with_aliases(batch, node, names, &HashMap::new())
}
fn node_record_with_aliases(
    batch: &str,
    node: &FileNode,
    names: &mut Names,
    aliases: &HashMap<NodeId, String>,
) -> io::Result<Record> {
    let entry = names.entry(node)?;
    Ok(Record {
        id: aliases
            .get(&node.id)
            .cloned()
            .unwrap_or_else(|| format!("{batch}-{}", node.id.0)),
        parent: node.parent.map_or_else(
            || format!("{batch}-root"),
            |id| aliases.get(&id).cloned().unwrap_or_else(|| format!("{batch}-{}", id.0)),
        ),
        name: entry.path.rsplit('/').next().unwrap().into(),
        directory: node.kind == NodeKind::Directory,
        size: match node.size {
            ContentSize::Known(n) => Some(n),
            ContentSize::Unknown => None,
        },
        modified: node.modified_ms,
        node: Some(node.id.0),
        version: node.version,
        children: None,
        hidden: None,
    })
}
/// A native URL can be read long after its metadata was enumerated. Persist
/// the selected version before sending its first byte so subsequent item/working
/// set queries agree with fetchContents' returned Item. A retired catalog must
/// never be recreated by a reader admitted before its deletion.
fn refresh_file_record(batch: &Batch, entry: &FileNode) -> io::Result<bool> {
    let mut state = batch.state.lock().unwrap();
    if state.retired {
        return Err(crate::content::unavailable(crate::model::Unavailable::NoSuchItem));
    }
    let index = state
        .catalog
        .records
        .iter()
        .position(|r| r.node == Some(entry.id.0) && !r.directory)
        .ok_or_else(|| crate::content::unavailable(crate::model::Unavailable::NoSuchItem))?;
    let old = state.catalog.records[index].clone();
    let size = match entry.size {
        ContentSize::Known(size) => Some(size),
        ContentSize::Unknown => None,
    };
    if old.size == size && old.modified == entry.modified_ms && old.version == entry.version {
        return Ok(false);
    }
    let current = &mut state.catalog.records[index];
    current.size = size;
    current.modified = entry.modified_ms;
    current.version = entry.version;
    if let Err(error) = state.persist(&batch.catalog) {
        state.catalog.records[index] = old;
        return Err(error);
    }
    Ok(true)
}

fn serve(mut stream: TcpStream, bridge: &Bridge) -> io::Result<()> {
    let _update_activity = oink_foundation::maintenance::process()
        .enter(oink_foundation::maintenance::ActivityKind::FileProvider)
        .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))?;

    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    io::BufReader::new((&stream).take(4096)).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err(io::Error::other("incomplete request"));
    }
    let request: Request = serde_json::from_str(&line).map_err(|_| io::Error::other("invalid request"))?;
    let (token, batch_id, node) = match &request {
        Request::Read { token, batch, node } | Request::List { token, batch, node, .. } => {
            (token, batch, NodeId(*node))
        }
    };
    let batch = bridge
        .batches
        .lock()
        .unwrap()
        .get(batch_id)
        .cloned()
        .filter(|b| &b.token == token)
        .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "unknown capability"))?;
    if !batch
        .state
        .lock()
        .unwrap()
        .catalog
        .records
        .iter()
        .any(|record| record.node == Some(node.0))
    {
        return Err(io::ErrorKind::NotFound.into());
    }
    let _watch = ReadWatch::new(&stream, batch.clone())?;
    let contents = batch.contents.lock().unwrap().clone();
    let content_read = matches!(request, Request::Read { .. });
    let result: io::Result<()> = (|| match request {
        Request::List { cursor, .. } => {
            let page = contents.list(node, cursor)?;
            let mut state = batch.state.lock().unwrap();
            let mut records = Vec::new();
            for child in &page.entries {
                let CatalogState { names, aliases, .. } = &mut *state;
                let record = node_record_with_aliases(batch_id, child, names, aliases)?;
                if !state.catalog.records.iter().any(|old| old.id == record.id) {
                    state.catalog.records.push(record.clone());
                }
                records.push(record);
            }
            if let Some(parent) = state
                .catalog
                .records
                .iter_mut()
                .find(|record| record.node == Some(node.0))
            {
                parent.version = page.version;
                if page.next.is_none() {
                    parent.children = Some(u64::from(cursor.map_or(0, |c| c.offset)) + page.entries.len() as u64);
                }
            }
            state.persist(&batch.catalog)?;
            drop(state);
            let bytes = serde_json::to_vec(&Listing {
                records,
                next: page.next,
                version: page.version,
            })?;
            stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
            stream.write_all(&bytes)
        }
        Request::Read { .. } => {
            let entry = contents.select(node)?;
            let ContentSize::Known(size) = entry.size else {
                return Err(io::ErrorKind::InvalidData.into());
            };
            let mut reader = contents.open(node, entry.version, 0)?;
            if refresh_file_record(&batch, &entry)? && CONTAINER.get().is_some() {
                signal_changes();
            }
            let header = serde_json::to_vec(
                &serde_json::json!({ "schema": 1, "size": size, "modified": entry.modified_ms, "version": entry.version }),
            )?;
            stream.write_all(&(u32::MAX - 1).to_be_bytes())?;
            stream.write_all(&(header.len() as u32).to_be_bytes())?;
            stream.write_all(&header)?;
            let mut buffer = vec![0; 256 * 1024];
            loop {
                if !_watch.connected.load(Ordering::Acquire) {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                let count = reader.read(&mut buffer)?;
                stream.write_all(&(count as u32).to_be_bytes())?;
                if count == 0 {
                    return Ok(());
                }
                stream.write_all(&buffer[..count])?;
            }
        }
    })();
    if let Err(ref error) = result {
        let reason = crate::content::reason(error);
        let unavailable = matches!(
            reason,
            crate::model::Unavailable::NoSuchItem | crate::model::Unavailable::Cancelled
        ) || (content_read && reason == crate::model::Unavailable::Superseded)
            // Prepared catalogs deliberately refuse speculative system reads.
            // An activated delivery losing its content grant is permanent.
            || (reason == crate::model::Unavailable::Permission
                && batch.state.lock().unwrap().catalog.batch.is_some());
        if unavailable {
            tracing::info!(
                ?reason,
                "File Provider publication withdrawn after permanent content failure"
            );
            retire_publication(bridge, batch_id, &batch);
        }
        let code = match reason {
            _ if unavailable => 2,
            crate::model::Unavailable::NoProgress => 1,
            crate::model::Unavailable::NoSuchItem | crate::model::Unavailable::ReadFailed => 2,
            crate::model::Unavailable::Superseded => 3,
            crate::model::Unavailable::Cancelled => 4,
            _ => 5,
        };
        let _ = stream
            .write_all(&u32::MAX.to_be_bytes())
            .and_then(|_| stream.write_all(&[code]));
    }
    result
}

fn retire_publication(bridge: &Bridge, key: &str, batch: &Arc<Batch>) {
    // macOS 26 retries versionNoLongerAvailable against the unchanged catalog
    // until the filesystem read fails with ETIMEDOUT. A permanently unavailable
    // source has no replacement version: withdraw its working-set identity and
    // return noSuchItem. Already admitted streams retain their own batch/read pins.
    bridge.batches.lock().unwrap().remove(key);
    retire_catalog(batch, &|| {
        if let Some(root) = batch.catalog.parent() {
            remove_catalog_in(root, &batch.catalog);
        }
    });
    if CONTAINER.get().is_some() {
        signal_changes();
    }
}

/// Called before the payload lifetime lock. Metadata must leave the working set
/// when the corresponding delivery expires, even if a native URL owner survives.
pub(crate) fn retire_unavailable(data: &std::path::Path, now: u64) {
    let Some(Ok(bridge)) = BRIDGE.get() else { return };
    let _ = received_usage(true);
    retire_unavailable_in(bridge, data, now);
}

fn retire_unavailable_in(bridge: &Bridge, data: &std::path::Path, now: u64) {
    let unavailable: Vec<_> = bridge
        .batches
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(key, batch)| {
            let id = batch.state.lock().unwrap().catalog.batch?;
            let all_deleted = (|| -> Option<bool> {
                let overlay: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(batch.catalog.with_extension("received")).ok()?).ok()?;
                let state = batch.state.lock().unwrap();
                let leaves: Vec<_> = state.catalog.records.iter().filter(|r| !r.directory).collect();
                Some(if leaves.is_empty() {
                    !state.catalog.records.is_empty()
                        && state.catalog.records.iter().all(|r| overlay[&r.id]["deleted"] == true)
                } else {
                    leaves.iter().all(|r| overlay[&r.id]["deleted"] == true)
                })
            })()
            .unwrap_or(false);
            (all_deleted || !crate::store::publication_available(data, id, now)).then(|| (key.clone(), batch.clone()))
        })
        .collect();
    for (key, batch) in unavailable {
        retire_publication(bridge, &key, &batch);
    }
}
/// A trashed device folder keeps its identity and children. A later delivery
/// creates a fresh active folder, rather than silently restoring the user's trash.
fn received_source_catalog(
    root: &std::path::Path,
    origin: oink_foundation::DeviceId,
    name: String,
) -> io::Result<String> {
    let base = format!("source{}", origin.to_hex());
    let path = root.join(format!("{base}.json"));
    let mut catalog: Catalog = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Catalog {
            schema: 1,
            token: String::new(),
            port: 0,
            records: Vec::new(),
            batch: None,
            urls: Vec::new(),
            received_at: None,
        },
        Err(e) => return Err(e),
    };
    for record in catalog.records.iter().rev() {
        let overlay: serde_json::Value = match std::fs::read(root.join(format!("{}.received", record.id))) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => serde_json::Value::Null,
            Err(e) => return Err(e),
        };
        let entry = &overlay[&record.id];
        let parent = entry["record"]["parent"].as_str().unwrap_or(&record.parent);
        if entry["deleted"] != true && parent == "NSFileProviderRootContainerItemIdentifier" {
            return Ok(record.id.clone());
        }
    }
    let id = if catalog.records.is_empty() {
        base
    } else {
        format!("{base}g{}", random()?)
    };
    catalog.records.push(Record {
        id: id.clone(),
        parent: "NSFileProviderRootContainerItemIdentifier".into(),
        name,
        directory: true,
        size: None,
        modified: None,
        node: None,
        version: 1,
        children: None,
        hidden: None,
    });
    save_catalog(&path, &catalog)?;
    Ok(id)
}
fn prepare_batch(
    contents: Arc<dyn Namespace>,
    id: &str,
    token: String,
    port: u16,
    catalog: PathBuf,
) -> io::Result<(Arc<Batch>, Vec<FileEntry>)> {
    prepare_batch_with_root(contents, id, token, port, catalog, None)
}
fn prepare_batch_with_root(
    contents: Arc<dyn Namespace>,
    id: &str,
    token: String,
    port: u16,
    catalog: PathBuf,
    previous_root: Option<&Record>,
) -> io::Result<(Arc<Batch>, Vec<FileEntry>)> {
    let roots = contents.roots();
    let received_at = contents.origin().map(|_| crate::received::now());
    let mut source_parent = "NSFileProviderRootContainerItemIdentifier".to_owned();
    let mut delivery_name = id.to_owned();
    if let Some(previous) = previous_root {
        source_parent = previous.parent.clone();
        delivery_name = previous.name.clone();
    } else if let Some(origin) = contents.origin() {
        let root = catalog.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let name = crate::received::source_folder(root, origin, &crate::received::source_name(origin))?;
        source_parent = received_source_catalog(root, origin, name)?;
        delivery_name = crate::received::local_stamp();
        let existing: Vec<Catalog> = std::fs::read_dir(root)?
            .flatten()
            .filter_map(|e| {
                std::fs::read(e.path())
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
            })
            .collect();
        let base = delivery_name.clone();
        let mut suffix = 2;
        while existing.iter().any(|c| {
            c.records
                .iter()
                .any(|r| r.parent == source_parent && r.name == delivery_name && r.id != format!("{id}-root"))
        }) {
            delivery_name = format!("{base}-{suffix}");
            suffix += 1;
        }
    }
    let mut names = Names::new(crate::sync::files::TargetOs::MacOs);
    let mut records = vec![Record {
        id: format!("{id}-root"),
        parent: source_parent,
        name: delivery_name,
        directory: true,
        size: None,
        modified: None,
        node: None,
        version: 1,
        children: Some(roots.len() as u64),
        hidden: None,
    }];
    let mut entries = Vec::new();
    for node in roots {
        records.push(node_record(id, &node, &mut names)?);
        entries.push(names.entry(&node)?);
    }
    let local_batch = contents.local_batch().map(|batch| batch.id());
    Ok((
        Arc::new(Batch {
            urls: Mutex::default(),
            token: token.clone(),
            contents: Mutex::new(contents),
            state: Mutex::new(CatalogState {
                catalog: Catalog {
                    schema: 1,
                    token,
                    port,
                    records,
                    batch: local_batch,
                    urls: Vec::new(),
                    received_at,
                },
                names,
                aliases: HashMap::new(),
                retired: false,
            }),
            catalog,
            readers: Mutex::new(0),
        }),
        entries,
    ))
}

fn restore_catalog(data: &std::path::Path, path: &std::path::Path, key: &str, port: u16) -> io::Result<Arc<Batch>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if crate::cache::is_link(&metadata) || !metadata.is_file() || metadata.len() > 64 * 1024 * 1024 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let previous: Catalog = serde_json::from_slice(&std::fs::read(path)?)?;
    let id = previous.batch.ok_or(io::ErrorKind::InvalidData)?;
    let local = crate::content::Completed::restore(data, id)?;
    let token = random()?;
    let (batch, _) = prepare_batch_with_root(
        Arc::new(local.clone()),
        key,
        token,
        port,
        path.to_owned(),
        previous.records.first(),
    )?;
    batch.state.lock().unwrap().catalog.received_at = previous.received_at;
    {
        let mut state = batch.state.lock().unwrap();
        placeholders::restore_bindings(key, &previous, &mut state)?;
        if let (Some(old), Some(new)) = (previous.records.first(), state.catalog.records.first_mut()) {
            new.parent = old.parent.clone();
            new.name = old.name.clone();
        }
        for node in local.nodes().iter().filter(|n| n.parent.is_some()) {
            let CatalogState { names, aliases, .. } = &mut *state;
            let record = node_record_with_aliases(key, node, names, aliases)?;
            state.catalog.records.push(record);
        }
        // Stable IDs and sanitized collision names must survive process death.
        // Size/version hints can legitimately have changed on first file read.
        if previous.records.iter().any(|old| {
            !state.catalog.records.iter().any(|new| {
                old.id == new.id
                    && old.parent == new.parent
                    && old.name == new.name
                    && old.directory == new.directory
                    && old.node == new.node
            })
        }) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut counts = HashMap::<String, u64>::new();
        for record in &state.catalog.records {
            *counts.entry(record.parent.clone()).or_default() += 1;
        }
        for record in &mut state.catalog.records {
            if record.directory {
                record.version = 1;
                record.children = Some(counts.get(&record.id).copied().unwrap_or(0));
            }
        }
        state.catalog.urls = previous.urls.clone();
        save_catalog(path, &state.catalog)?;
    }
    *batch.urls.lock().unwrap() = previous.urls;
    Ok(batch)
}
/// Register a drag snapshot with a strict total wait budget. Only system-issued URLs are returned.
pub fn register(
    id: ClipId,
    entries: &[FileEntry],
    contents: Arc<dyn FileContents>,
    timeout: Duration,
) -> Result<Registration, ClipboardError> {
    tracing::debug!(?id, "registering File Provider drag snapshot");
    publish(entries, contents, timeout)
}
/// Wait for actual system URLs while the exact target host still holds Left.
/// macOS 26 needs two namespace materialization steps (~1.0–1.4 s in acceptance),
/// even with a warm extension. The former 750 ms UI budget killed publication
/// between those steps. There is no OS readiness latency guarantee: retain only
/// the manager's existing 12 s operation watchdog, plus owner cancellation. A
/// failed publication must not silently change file URLs into file promises.
pub fn register_drag(
    entries: &[FileEntry],
    contents: Arc<dyn FileContents>,
    current: &dyn Fn() -> bool,
) -> Result<Registration, ClipboardError> {
    publish_while(entries, contents, MANAGER_TIMEOUT, current)
}
/// Target drag owner opts into the one-shot spare and typed phase observations.
pub fn register_drag_measured(
    id: ClipId,
    epoch: u64,
    timing: &mut PreparationTiming,
    started: Instant,
    entries: &[FileEntry],
    contents: Arc<dyn FileContents>,
    current: &dyn Fn() -> bool,
) -> Result<Registration, ClipboardError> {
    let start = monotonic_ns().saturating_sub(started.elapsed().as_nanos() as u64);
    let result = publish_inner(
        entries,
        contents,
        started + MANAGER_TIMEOUT,
        current,
        &mut PublicationTrace::new(Some(PublicationOwner::Drag(id, epoch))),
        None,
        timing,
        start,
    );
    timing.registration_us = started.elapsed().as_micros() as u64;
    record_result(&result);
    result
}
pub(crate) fn publish(
    entries: &[FileEntry],
    contents: Arc<dyn FileContents>,
    timeout: Duration,
) -> Result<Registration, ClipboardError> {
    publish_while(entries, contents, timeout, &|| true)
}
fn publish_while(
    entries: &[FileEntry],
    contents: Arc<dyn FileContents>,
    timeout: Duration,
    current: &dyn Fn() -> bool,
) -> Result<Registration, ClipboardError> {
    let result = publish_inner(
        entries,
        contents,
        Instant::now() + timeout,
        current,
        &mut PublicationTrace::new(None),
        None,
        &mut PreparationTiming::default(),
        monotonic_ns(),
    );
    record_result(&result);
    result
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublicationOwner {
    Drag(ClipId, u64),
    Announced(PreparationKey),
    Entered(PreparationKey, u64),
}
impl PublicationOwner {
    fn permits_placeholder(self) -> bool {
        !matches!(self, Self::Announced(_))
    }
}
#[derive(Clone, Copy, Debug)]
struct PublicationTrace {
    owner: Option<PublicationOwner>,
    generation: u64,
    stock_generation: u64,
}
impl PublicationTrace {
    fn new(owner: Option<PublicationOwner>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            owner,
            generation: NEXT.fetch_add(1, Ordering::Relaxed),
            stock_generation: 0,
        }
    }
    fn kind(&self) -> u64 {
        match self.owner {
            Some(PublicationOwner::Announced(_)) => 0,
            Some(PublicationOwner::Entered(_, _)) => 1,
            _ => 2,
        }
    }
    fn epoch(&self) -> Option<u64> {
        match self.owner {
            Some(PublicationOwner::Drag(_, epoch) | PublicationOwner::Entered(_, epoch)) => Some(epoch),
            _ => None,
        }
    }
    fn activated(&self, epoch: u64) {
        tracing::debug!(
            dnd_phase = 53_u64,
            preparation_generation = self.generation,
            placeholder_generation = self.stock_generation,
            provider_preparation = self.kind(),
            epoch,
            "File Provider prepared namespace activated"
        );
    }
}
#[allow(clippy::too_many_arguments)]
fn publish_inner(
    entries: &[FileEntry],
    contents: Arc<dyn FileContents>,
    deadline: Instant,
    current: &dyn Fn() -> bool,
    trace: &mut PublicationTrace,
    selected: Option<placeholders::Taken>,
    timing: &mut PreparationTiming,
    start: u64,
) -> Result<Registration, ClipboardError> {
    let _foreground = placeholders::Foreground::enter();
    let mut result = publish_attempt(
        entries,
        contents.clone(),
        deadline,
        current,
        trace,
        selected,
        timing,
        start,
        true,
    );
    if timing.placeholder.used && result.is_err() {
        use crate::provider_preparation::PlaceholderFallback;
        timing.placeholder.used = false;
        timing.placeholder.fallback = if placeholder_status().enabled {
            PlaceholderFallback::PublicationFailed
        } else {
            PlaceholderFallback::DisabledDuringPublication
        };
        timing.placeholder.failure = result.as_ref().err().map(failure_reason);
        if check_publication(deadline, current).is_ok() {
            // A failed placeholder rename never replaces the ordinary spare
            // policy. Its consumed identity is retired, and this single fallback
            // keeps the original total deadline and source/owner cancellation.
            timing.parent_visible_us = None;
            timing.children_enumerated_us = None;
            timing.first_url_us = None;
            result = publish_attempt(entries, contents, deadline, current, trace, None, timing, start, false);
        }
    }
    if trace.owner.is_some() {
        record_placeholder_path(timing, trace);
    }
    result
}

fn storage_until(
    deadline: Instant,
    current: &dyn Fn() -> bool,
) -> Result<std::sync::MutexGuard<'static, ()>, ClipboardError> {
    loop {
        check_publication(deadline, current)?;
        match STORAGE.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_attempt(
    entries: &[FileEntry],
    contents: Arc<dyn FileContents>,
    deadline: Instant,
    current: &dyn Fn() -> bool,
    trace: &mut PublicationTrace,
    selected: Option<placeholders::Taken>,
    timing: &mut PreparationTiming,
    start: u64,
    try_placeholders: bool,
) -> Result<Registration, ClipboardError> {
    let owner = trace.owner;
    if entries.is_empty() {
        return Err(error("empty file snapshot"));
    }
    if !REQUESTED.swap(true, Ordering::AcqRel) {
        DEMAND_CHANGED.notify_one();
    }
    let _storage = storage_until(deadline, current)?;
    let root = container_while(deadline, current)?;
    let bridge = bridge(&root)?;
    let contents: Arc<dyn Namespace> = match contents.namespace() {
        Some(namespace) => namespace,
        None => Flat::new(entries, contents).map_err(error)?,
    };
    check_publication(deadline, current)?;
    let mut placeholder = selected.or_else(|| {
        if try_placeholders && owner.is_some_and(PublicationOwner::permits_placeholder) {
            placeholders::take(&contents.roots(), &mut timing.placeholder)
        } else {
            None
        }
    });
    if let Some(taken) = &placeholder {
        timing.placeholder = taken.timing.clone();
        trace.stock_generation = taken.generation();
    } else if try_placeholders && owner.is_some_and(|owner| !owner.permits_placeholder()) {
        timing.placeholder = placeholder_status();
        if matches!(owner, Some(PublicationOwner::Announced(_))) && timing.placeholder.enabled {
            timing.placeholder.fallback = crate::provider_preparation::PlaceholderFallback::DeferredToEnter;
        }
    }
    let taken = owner.and_then(|owner| {
        let mut pool = SPARE.lock().unwrap();
        record_spare_status(
            pool.snapshot(),
            match owner {
                PublicationOwner::Drag(_, epoch) | PublicationOwner::Entered(_, epoch) => Some(epoch),
                PublicationOwner::Announced(_) => None,
            },
        );
        if placeholder.is_some() {
            pool.observe(timing);
            None
        } else {
            pool.take(owner, timing)
        }
    });
    if owner.is_some() {
        start_spare();
    }
    let placeholder_epoch = placeholder.as_ref().map(|taken| taken.setting_epoch);
    let typed = placeholder.is_some();
    let (batch_id, token) = if let Some(taken) = placeholder.as_mut() {
        let prepared = taken.ready.take().expect("taken namespace owns its parent");
        timing.parent_visible_us = Some(0);
        // A request started against a generic catalog must never authenticate to
        // the real node later bound to that provider slot, even with a guessed ID.
        let token = match random() {
            Ok(token) => token,
            Err(e) => {
                placeholders::retire(vec![prepared.key]);
                return Err(error(e));
            }
        };
        (prepared.key, token)
    } else if let Some(taken) = taken {
        debug_assert_eq!(Some(taken.owner), owner);
        timing.parent_visible_us = Some(0);
        (taken.spare.key, taken.spare.token)
    } else {
        (random().map_err(error)?, random().map_err(error)?)
    };
    let publication_current = || current() && placeholder_epoch.is_none_or(placeholders::activation_current);
    let catalog = root.join(format!("{batch_id}.json"));
    // A taken identity is always retired on failure, including cancellation before
    // the filled catalog was published. Old URLs can never name another drag.
    let stage = if typed {
        |contents, key, token, port, catalog, check, retire| {
            stage_publication_with_placeholders(contents, key, token, port, catalog, check, retire, true)
        }
    } else {
        stage_publication
    };
    let (batch, entries) = stage(
        contents,
        &batch_id,
        token,
        bridge.port,
        &catalog,
        &|| check_publication(deadline, &publication_current),
        &|| {
            remove_catalog(&catalog);
            if typed {
                placeholders::retire(vec![batch_id.clone()]);
            } else {
                signal_changes();
            }
        },
    )?;
    let content_pin = batch.contents.lock().unwrap().local_batch();
    if !typed {
        bridge
            .batches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(batch_id.clone(), batch.clone());
    }
    // While a typed rename is in flight there is no content bridge capability.
    // Only final namespace readiness admits readers; announced admission still
    // separately requires activate_prepared and its content grant.
    // Both kinds are protected while OS callbacks run: ordinary bridge batches
    // have the local strong pin; checked-out typed stock stays in pool.protected.
    // Neither an unrelated Announced helper nor a refill owns STORAGE here.
    drop(_storage);
    let command = if typed { "publish-placeholders" } else { "publish" };
    let result = manager_while(&[command, &batch_id], deadline, &publication_current).and_then(|json| {
        check_publication(deadline, &publication_current)?;
        let paths: Vec<PathBuf> = serde_json::from_str(&json).map_err(error)?;
        let _storage = storage_until(deadline, &publication_current)?;
        if paths.len() != entries.len() {
            return Err(error("incomplete File Provider URL list"));
        }
        *batch.urls.lock().unwrap_or_else(|e| e.into_inner()) = paths.clone();
        {
            let mut state = batch.state.lock().unwrap();
            state.catalog.urls = paths.clone();
            save_catalog(&catalog, &state.catalog).map_err(error)?;
        }
        if typed {
            placeholders::complete(&root, &batch_id).map_err(error)?;
            check_publication(deadline, &publication_current)?;
            bridge
                .batches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(batch_id.clone(), batch.clone());
            timing.placeholder.final_urls_us = Some(monotonic_ns().saturating_sub(start) / 1_000);
        }
        let entries = entries
            .iter()
            .cloned()
            .zip(paths)
            .map(|(mut e, p)| {
                e.local_path = Some(p);
                e
            })
            .collect();
        Ok(Registration {
            key: batch_id.clone(),
            bridge: Arc::downgrade(&bridge),
            batch: batch.clone(),
            entries,
            _content_pin: content_pin,
            admission: None,
        })
    });
    timing.children_enumerated_us = std::fs::read_to_string(catalog.with_extension("enumerated"))
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|time| time.saturating_sub(start) / 1_000);
    // Announced publications retain their admission wrapper even after Stored.
    // Their deferred content owner is installed only by activate_prepared.
    if result.is_ok()
        && !matches!(
            owner,
            Some(PublicationOwner::Announced(_) | PublicationOwner::Entered(_, _))
        )
    {
        let weak = Arc::downgrade(&batch);
        batch.contents.lock().unwrap().on_stored(Box::new(move |local| {
            let _storage = STORAGE.lock().unwrap_or_else(|e| e.into_inner());
            let Some(batch) = weak.upgrade() else { return };
            // Existing native registrations/streams keep their independent pins.
            // Only the dormant bridge catalog sheds its remote transfer owner.
            *batch.contents.lock().unwrap() = Arc::new(local);
            if let Some(root) = CONTAINER.get() {
                crate::cache::changed(root);
            }
        }));
    }
    if result.is_err() {
        bridge
            .batches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&batch_id);
        retire_catalog(&batch, &|| remove_catalog(&catalog));
        if typed {
            placeholders::retire(vec![batch_id.clone()]);
        } else {
            signal_changes();
        }
    }
    crate::cache::changed(&root);
    result
}

fn stage_publication(
    contents: Arc<dyn Namespace>,
    key: &str,
    token: String,
    port: u16,
    catalog: &std::path::Path,
    check: &dyn Fn() -> Result<(), ClipboardError>,
    retire: &dyn Fn(),
) -> Result<(Arc<Batch>, Vec<FileEntry>), ClipboardError> {
    stage_publication_with_placeholders(contents, key, token, port, catalog, check, retire, false)
}
#[allow(clippy::too_many_arguments)]
fn stage_publication_with_placeholders(
    contents: Arc<dyn Namespace>,
    key: &str,
    token: String,
    port: u16,
    catalog: &std::path::Path,
    check: &dyn Fn() -> Result<(), ClipboardError>,
    retire: &dyn Fn(),
    typed: bool,
) -> Result<(Arc<Batch>, Vec<FileEntry>), ClipboardError> {
    let result = (|| {
        check()?;
        let (batch, entries) = prepare_batch(contents, key, token, port, catalog.to_owned()).map_err(error)?;
        if typed {
            placeholders::bind(key, &mut batch.state.lock().unwrap()).map_err(error)?;
            // The generic preparation reported children too; it is not this
            // foreground update's enumeration boundary.
            let _ = std::fs::remove_file(catalog.with_extension("enumerated"));
        }
        check()?;
        save_catalog(catalog, &batch.state.lock().unwrap().catalog).map_err(error)?;
        Ok((batch, entries))
    })();
    if result.is_err() {
        retire();
    }
    result
}

fn remove_catalog(path: &std::path::Path) {
    let Some(root) = CONTAINER.get() else { return };
    remove_catalog_in(root, path);
}
fn received_catalog_protected(path: &std::path::Path) -> bool {
    if path.extension().is_some_and(|e| e == "json") {
        match std::fs::read(path).map(|b| serde_json::from_slice::<Catalog>(&b)) {
            Ok(Ok(catalog)) if catalog.received_at.is_none() => return false, // legacy layouts expire
            Ok(Err(_)) => return true, // uncertain metadata must not delete an adopted item
            _ => {}
        }
    }
    let sidecar = path.with_extension("received");
    match std::fs::read(sidecar) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .is_none_or(|entries| {
                entries.values().any(|v| {
                    v["deleted"] != true
                        && (v["handed_off"] == true
                            || v["materialized"] == true
                            || v["fetching"].as_u64().unwrap_or(0) > 0)
                })
            }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Catalog>(&b).ok())
            .is_some_and(|c| c.received_at.is_some() && c.records.iter().any(|r| !r.directory && r.size == Some(0))),
        Err(_) => true,
    }
}
pub fn received_usage(maintain: bool) -> io::Result<crate::received::Usage> {
    let value = manager(
        &[
            if maintain {
                "received-maintain"
            } else {
                "received-usage"
            },
            &crate::received::setting().days().to_string(),
        ],
        Instant::now() + MANAGER_TIMEOUT,
    )
    .map_err(io::Error::other)?;
    let mut usage: crate::received::Usage = serde_json::from_str(&value)?;
    usage.cleanup = crate::received::setting();
    Ok(usage)
}
pub fn open_received() -> io::Result<()> {
    manager(&["open-received"], Instant::now() + MANAGER_TIMEOUT)
        .map(|_| ())
        .map_err(io::Error::other)
}
fn remove_catalog_in(root: &std::path::Path, path: &std::path::Path) {
    assert_eq!(path.parent(), Some(root), "catalog removal escaped Oink storage");
    if std::fs::symlink_metadata(root).is_ok_and(|m| !crate::cache::is_link(&m)) {
        if received_catalog_protected(path) {
            // Only retained children and their ancestor chain survive detachment;
            // never-read siblings lose their expired backing and disappear.
            let retained = (|| -> io::Result<()> {
                let mut catalog: Catalog = serde_json::from_slice(&std::fs::read(path)?)?;
                let overlay: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(path.with_extension("received"))?)?;
                let mut keep: std::collections::HashSet<String> = overlay
                    .as_object()
                    .ok_or(io::ErrorKind::InvalidData)?
                    .iter()
                    .filter(|(_, v)| {
                        v["deleted"] != true
                            && (v["handed_off"] == true
                                || v["materialized"] == true
                                || v["fetching"].as_u64().unwrap_or(0) > 0)
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                let mut previous = 0;
                while previous != keep.len() {
                    previous = keep.len();
                    for record in &catalog.records {
                        if keep.contains(&record.id) {
                            keep.insert(record.parent.clone());
                        }
                    }
                }
                catalog.records.retain(|r| keep.contains(&r.id));
                save_catalog(path, &catalog)
            })();
            if retained.is_err() {
                tracing::warn!(
                    reason = "received_catalog_detach",
                    "received files conservatively retained"
                );
            }
            return;
        }
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("enumerated"));
        crate::cache::changed(root);
    }
}

/// Translate live native URL references to owner leases before retention scans.
/// This uses only the existing bridge state; it never invokes a system helper.
pub(crate) fn retention_pins() -> Vec<crate::store::Batch> {
    let sources = crate::cache::source_paths();
    let Some(Ok(bridge)) = BRIDGE.get() else {
        return Vec::new();
    };
    bridge
        .batches
        .lock()
        .unwrap()
        .values()
        .filter_map(|batch| {
            batch_needed(batch, &sources)
                .then(|| batch.contents.lock().unwrap().local_batch())
                .flatten()
        })
        .collect()
}

/// Called only by the native owner, on request or storage notification; never registers a domain.
pub fn cache_usage(clear: bool) -> (Vec<crate::cache::CacheSize>, u64, bool, Vec<crate::store::Batch>) {
    use crate::cache::{CacheCategory, CacheSize};
    let _storage = STORAGE.lock().unwrap_or_else(|e| e.into_inner());
    let root = match container(Instant::now() + MANAGER_TIMEOUT) {
        Ok(root) => root,
        Err(_) => return (Vec::new(), 0, true, Vec::new()),
    };
    let root = &root;
    let catalog_before = catalog_bytes(root).unwrap_or(0);
    let sources = crate::cache::source_paths();
    let mut protected = Vec::<String>::new();
    let mut retained_catalogs = std::collections::HashSet::new();
    for key in SPARE.lock().unwrap().protected() {
        retained_catalogs.insert(key.to_owned());
        protected.push(format!("{key}-root"));
    }
    for key in placeholders::protected() {
        retained_catalogs.insert(key.clone());
        protected.push(format!("{key}-root"));
        for (kind, count) in [("file", 5), ("folder", 5)] {
            for index in 0..count {
                protected.push(format!("{key}-slot-{kind}-{index}"));
            }
        }
    }
    if let Some(Ok(bridge)) = BRIDGE.get() {
        let batches = bridge.batches.lock().unwrap_or_else(|e| e.into_inner());
        for (key, batch) in batches.iter() {
            if batch_needed(batch, &sources) {
                retained_catalogs.insert(key.clone());
                protected.extend(
                    batch
                        .state
                        .lock()
                        .unwrap()
                        .catalog
                        .records
                        .iter()
                        .filter(|record| !record.directory)
                        .map(|record| record.id.clone()),
                );
            }
        }
    }
    let result = (|| -> Result<serde_json::Value, ClipboardError> {
        let mut child = Command::new(
            std::env::current_exe()
                .map_err(error)?
                .parent()
                .ok_or_else(|| error("missing executable directory"))?
                .join("oink-file-provider"),
        )
        .arg(if clear { "cache-clear" } else { "cache" })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(error)?;
        let input = serde_json::to_vec(&serde_json::json!({"items": protected, "paths": sources})).map_err(error)?;
        child
            .stdin
            .take()
            .ok_or_else(|| error("cache manager stdin"))?
            .write_all(&input)
            .map_err(error)?;
        let output = child.wait_with_output().map_err(error)?;
        if !output.status.success() {
            return Err(error("cache manager unavailable"));
        }
        serde_json::from_slice(&output.stdout).map_err(error)
    })();
    let mut may_clear_catalogs = false;
    if let Ok(value) = &result {
        may_clear_catalogs =
            clear && !value["busy"].as_bool().unwrap_or(true) && !value["incomplete"].as_bool().unwrap_or(true);
        if let Some(keys) = value["protected_batches"].as_array() {
            retained_catalogs.extend(keys.iter().filter_map(|k| k.as_str().map(str::to_owned)));
        }
    }
    if may_clear_catalogs && let Some(Ok(bridge)) = BRIDGE.get() {
        bridge
            .batches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, batch| {
                if retained_catalogs.contains(key) || batch_needed(batch, &sources) {
                    retained_catalogs.insert(key.clone());
                    true
                } else {
                    remove_catalog(&batch.catalog);
                    false
                }
            });
    }
    let mut catalog_bytes = 0;
    let mut catalog_incomplete = STORAGE_WATCH.get().is_none_or(|r| r.is_err());
    match std::fs::read_dir(root) {
        Ok(entries) => {
            for entry in entries {
                let Ok(entry) = entry else {
                    catalog_incomplete = true;
                    continue;
                };
                if entry.file_name() == "temporary" {
                    continue;
                }
                let path = entry.path();
                if received_cache_excluded(&path) {
                    continue;
                }
                let n = match crate::cache::tree_bytes(&path) {
                    Ok(n) => n,
                    Err(_) => {
                        catalog_incomplete = true;
                        continue;
                    }
                };
                let stale = path.extension().is_some_and(|e| e == "json")
                    && path.file_stem().and_then(|n| n.to_str()).is_some_and(|n| {
                        n.len() == 64 && n.bytes().all(|c| c.is_ascii_hexdigit()) && !retained_catalogs.contains(n)
                    });
                if may_clear_catalogs && (path.extension().is_some_and(|e| e == "tmp") || stale) {
                    remove_catalog(&path);
                    if !path.exists() {
                        continue;
                    }
                }
                catalog_bytes += n;
            }
        }
        Err(_) => catalog_incomplete = true,
    }
    if may_clear_catalogs {
        signal_changes();
    }
    // Keep system-protected/demanded cached deliveries pinned through the
    // caller's subsequent managed-store scan, under the same lifetime lock.
    let mut pins = Vec::new();
    if let Some(Ok(bridge)) = BRIDGE.get() {
        for (key, batch) in bridge.batches.lock().unwrap().iter() {
            if (retained_catalogs.contains(key) || (clear && !may_clear_catalogs))
                && let Some(pin) = batch.contents.lock().unwrap().local_batch()
            {
                pins.push(pin);
            }
        }
    }
    let mut categories = vec![CacheSize {
        category: CacheCategory::ProviderCatalog,
        bytes: catalog_bytes,
        kept_bytes: catalog_bytes,
    }];
    match result {
        Ok(v) => {
            let bytes = v["bytes"].as_u64().unwrap_or(0);
            let kept = v["kept"].as_u64().unwrap_or(bytes);
            categories.push(CacheSize {
                category: CacheCategory::ProviderMaterialized,
                bytes,
                kept_bytes: kept,
            });
            let temporary = v["temporary"].as_u64().unwrap_or(0);
            categories.push(CacheSize {
                category: CacheCategory::ProviderTemporary,
                bytes: temporary,
                kept_bytes: v["temporaryKept"].as_u64().unwrap_or(temporary),
            });
            (
                categories,
                catalog_before.saturating_sub(catalog_bytes) + v["removed"].as_u64().unwrap_or(0),
                catalog_incomplete || v["incomplete"].as_bool().unwrap_or(false),
                pins,
            )
        }
        Err(_) => (categories, catalog_before.saturating_sub(catalog_bytes), true, pins),
    }
}

pub fn is_cache_root(root: &std::path::Path) -> bool {
    CONTAINER.get().is_some_and(|r| r == root)
}

fn received_cache_excluded(path: &std::path::Path) -> bool {
    path.extension()
        .is_some_and(|e| e == "received" || e == "sources" || e == "lock")
        || path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("source") && s.len() == 70)
        || received_catalog_protected(path)
}
fn catalog_bytes(root: &std::path::Path) -> io::Result<u64> {
    std::fs::read_dir(root)?.try_fold(0u64, |sum, e| {
        let e = e?;
        if e.file_name() == "temporary" || received_cache_excluded(&e.path()) {
            Ok(sum)
        } else {
            Ok(sum.saturating_add(crate::cache::tree_bytes(&e.path())?))
        }
    })
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(root) = CONTAINER.get() {
            crate::cache::changed(root);
        }
    }
}

static STORAGE_WATCH: OnceLock<Result<Mutex<storage_watch::StorageWatch>, String>> = OnceLock::new();
fn watch_storage(root: &std::path::Path) {
    STORAGE_WATCH.get_or_init(|| storage_watch::StorageWatch::new(root).map(Mutex::new));
}

pub(crate) fn cache_root() -> Option<PathBuf> {
    CONTAINER.get().cloned()
}
fn batch_needed(batch: &Arc<Batch>, sources: &[PathBuf]) -> bool {
    Arc::strong_count(batch) > 1
        || batch.contents.lock().unwrap().in_use()
        || batch
            .urls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|url| sources.iter().any(|s| url.starts_with(s) || s.starts_with(url)))
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn numeric_stock_generation_links_refill_publication_and_activation() {
        #[derive(Clone)]
        struct Output(Arc<Mutex<Vec<u8>>>);
        impl Write for Output {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = bytes.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || Output(output.clone()))
            .finish();
        let trace = PublicationTrace {
            owner: Some(PublicationOwner::Drag(
                ClipId {
                    origin: oink_foundation::DeviceId([3; 32]),
                    seq: 99,
                },
                41,
            )),
            generation: 7,
            stock_generation: 23,
        };
        tracing::subscriber::with_default(subscriber, || {
            record_placeholder_refill(23, 0, 0, 0, 0, None);
            record_placeholder_refill(23, 1, 2100, 0, 1, None);
            record_placeholder_path(&PreparationTiming::default(), &trace);
            trace.activated(41);
        });
        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert_eq!(output.lines().count(), 4);
        assert!(output.lines().all(|line| line.contains("placeholder_generation=23")));
        assert_eq!(output.matches("preparation_generation=7").count(), 2);
        assert_eq!(output.matches("epoch=41").count(), 2);
        for phase in [50, 52, 53] {
            assert!(output.contains(&format!("dnd_phase={phase}")));
        }
        assert!(!output.contains("origin") && !output.contains("seq=99"));
    }

    #[test]
    fn spare_and_new_parent_publish_identical_catalogs_and_cancel_retires_taken_identity() {
        let root = tempfile::tempdir().unwrap();
        let key = "a".repeat(64);
        let path = root.path().join(format!("{key}.json"));
        let content = || Flat::new(&[], Arc::new(EmptyContents)).unwrap();
        let (empty, _) = prepare_batch(content(), &key, "secret".into(), 1, path.clone()).unwrap();
        save_catalog(&path, &empty.state.lock().unwrap().catalog).unwrap();
        let signals = AtomicUsize::new(0);
        let retire = || {
            remove_catalog_in(root.path(), &path);
            signals.fetch_add(1, Ordering::SeqCst);
        };
        let cancelled = stage_publication(
            content(),
            &key,
            "secret".into(),
            1,
            &path,
            &|| Err(ClipboardError::FileProvider(FileProviderReason::Cancelled)),
            &retire,
        );
        assert!(cancelled.is_err());
        assert!(!path.exists());
        assert_eq!(signals.load(Ordering::SeqCst), 1);
        let entries = [FileEntry {
            path: "fixture".into(),
            is_dir: false,
            size: 0,
            modified_ms: None,
            local_path: None,
        }];
        let content = || Flat::new(&entries, Arc::new(EmptyContents)).unwrap();
        let (fresh, fresh_entries) =
            stage_publication(content(), &key, "secret".into(), 1, &path, &|| Ok(()), &retire).unwrap();
        let fresh_json = std::fs::read(&path).unwrap();
        save_catalog(&path, &empty.state.lock().unwrap().catalog).unwrap();
        let (filled, filled_entries) =
            stage_publication(content(), &key, "secret".into(), 1, &path, &|| Ok(()), &retire).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), fresh_json);
        assert_eq!(fresh_entries, filled_entries);
        assert_eq!(
            fresh.state.lock().unwrap().catalog.records[1].id,
            filled.state.lock().unwrap().catalog.records[1].id
        );
        // The equivalence fixture uses the same key deliberately; real allocation is
        // one-shot (Pool test). A later allocation never shares these URL item IDs.
        let next_key = "b".repeat(64);
        let (next, _) = prepare_batch(content(), &next_key, "new".into(), 1, root.path().join("next.json")).unwrap();
        assert!(
            !next
                .state
                .lock()
                .unwrap()
                .catalog
                .records
                .iter()
                .any(|r| r.id.starts_with(&key))
        );
    }

    #[test]
    fn late_directory_reader_cannot_resurrect_a_retired_working_set_catalog() {
        let root = tempfile::tempdir().unwrap();
        let key = "a".repeat(64);
        let path = root.path().join(format!("{key}.json"));
        let (batch, _) = prepare_batch(
            Flat::new(&[], Arc::new(EmptyContents)).unwrap(),
            &key,
            "secret".into(),
            1,
            path.clone(),
        )
        .unwrap();
        batch.state.lock().unwrap().persist(&path).unwrap();
        assert!(path.exists());
        let retained_reader = batch.clone();
        retire_catalog(&batch, &|| remove_catalog_in(root.path(), &path));
        assert!(!path.exists());
        retained_reader.state.lock().unwrap().persist(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn a_new_delivery_does_not_restore_a_trashed_source_folder_or_move_its_old_batches() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let owned = crate::content::Completed::test_single_file(data.path());
        let local = crate::content::Completed::restore(data.path(), owned.id()).unwrap();
        let first_path = root.path().join("first.json");
        let (first, _) = prepare_batch(Arc::new(local.clone()), "first", "a".into(), 1, first_path.clone()).unwrap();
        let first_catalog = first.state.lock().unwrap().catalog.clone();
        save_catalog(&first_path, &first_catalog).unwrap();
        let old_parent = first_catalog.records[0].parent.clone();
        let source_path = root.path().join(format!("{old_parent}.json"));
        let mut source: Catalog = serde_json::from_slice(&std::fs::read(&source_path).unwrap()).unwrap();
        source.records[0].parent = "NSFileProviderTrashContainerItemIdentifier".into();
        std::fs::write(root.path().join(format!("{old_parent}.received")),serde_json::to_vec(&serde_json::json!({&old_parent:{"record":source.records[0],"received_at":1,"handed_off":false,"materialized":false,"deleted":false}})).unwrap()).unwrap();
        let (next, _) = prepare_batch(Arc::new(local), "next", "b".into(), 2, root.path().join("next.json")).unwrap();
        let new_parent = next.state.lock().unwrap().catalog.records[0].parent.clone();
        assert_ne!(old_parent, new_parent);
        let restored = restore_catalog(data.path(), &first_path, "first", 3).unwrap();
        assert_eq!(restored.state.lock().unwrap().catalog.records[0].parent, old_parent);
        let source: Catalog = serde_json::from_slice(&std::fs::read(source_path).unwrap()).unwrap();
        assert_eq!(source.records.len(), 2);
        assert_eq!(source.records[0].name, source.records[1].name);
    }
    #[test]
    fn materialized_catalog_survives_restart_without_backing_and_trims_stale_siblings() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let key = "a".repeat(64);
        let path = root.path().join(format!("{key}.json"));
        let entries: Vec<_> = ["kept", "stale"]
            .into_iter()
            .map(|name| FileEntry {
                path: name.into(),
                is_dir: false,
                size: 1,
                modified_ms: None,
                local_path: None,
            })
            .collect();
        let (batch, _) = prepare_batch(
            Flat::new(&entries, Arc::new(EmptyContents)).unwrap(),
            &key,
            "token".into(),
            1,
            path.clone(),
        )
        .unwrap();
        let mut catalog = batch.state.lock().unwrap().catalog.clone();
        catalog.received_at = Some(crate::received::now());
        save_catalog(&path, &catalog).unwrap();
        let kept = catalog.records.iter().find(|r| r.name == "kept").unwrap();
        std::fs::write(
            path.with_extension("received"),
            serde_json::to_vec(&serde_json::json!({&kept.id:{"materialized":true,"deleted":false}})).unwrap(),
        )
        .unwrap();
        assert!(recover_catalogs(data.path(), root.path(), 2).unwrap().is_empty());
        let restored: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(restored.records.len(), 2);
        assert!(restored.records.iter().any(|r| r.id == kept.id));
        assert!(!restored.records.iter().any(|r| r.name == "stale"));
        std::fs::write(path.with_extension("received"), b"corrupt").unwrap();
        remove_catalog_in(root.path(), &path);
        assert!(path.exists(), "uncertain metadata is retained");
        std::fs::write(
            path.with_extension("received"),
            serde_json::to_vec(&serde_json::json!({&kept.id:{"materialized":true,"deleted":true}})).unwrap(),
        )
        .unwrap();
        remove_catalog_in(root.path(), &path);
        assert!(!path.exists());
    }

    #[test]
    fn startup_discards_leftover_spares_and_markers_without_touching_delivered_content() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        // Includes both ready catalogs and a sixth, partly prepared catalog
        // left by an earlier process; none is a reusable source capability.
        for digit in ['a', 'b', 'c', 'd', 'e', 'f'] {
            let key = digit.to_string().repeat(64);
            let path = root.path().join(format!("{key}.json"));
            let (empty, _) = prepare_batch(
                Flat::new(&[], Arc::new(EmptyContents)).unwrap(),
                &key,
                "old".into(),
                1,
                path.clone(),
            )
            .unwrap();
            save_catalog(&path, &empty.state.lock().unwrap().catalog).unwrap();
            std::fs::write(path.with_extension("enumerated"), "12").unwrap();
            std::fs::write(path.with_extension("tmp"), "interrupted write").unwrap();
            paths.push(path);
        }
        let user_content = root.path().join("unrelated-user-file");
        std::fs::write(&user_content, "untouched").unwrap();
        let recovered = recover_catalogs(data.path(), root.path(), 2).unwrap();
        assert!(recovered.is_empty());
        for path in paths {
            assert!(!path.exists());
            assert!(!path.with_extension("enumerated").exists());
            assert!(!path.with_extension("tmp").exists());
        }
        assert_eq!(std::fs::read_to_string(user_content).unwrap(), "untouched");
    }

    #[test]
    fn recovery_retires_a_pending_placeholder_before_restoring_completed_backing() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let owned = crate::content::Completed::test_single_file(data.path());
        let local = crate::content::Completed::restore(data.path(), owned.id()).unwrap();
        let key = "a".repeat(64);
        let path = root.path().join(format!("{key}.json"));
        let (batch, _) = prepare_batch(Arc::new(local), &key, "old".into(), 1, path.clone()).unwrap();
        placeholders::bind(&key, &mut batch.state.lock().unwrap()).unwrap();
        save_catalog(&path, &batch.state.lock().unwrap().catalog).unwrap();
        let ledger = path.with_extension("placeholder");
        std::fs::write(&ledger, br#"{"parent":null,"urls":[]}"#).unwrap();
        std::fs::write(path.with_extension("placeholder-tmp"), "interrupted").unwrap();
        drop(batch);
        // A pending ledger means no final URL was handed off, even if the
        // independent payload had already completed before the process died.
        assert!(recover_catalogs(data.path(), root.path(), 2).unwrap().is_empty());
        assert!(!path.exists() && !path.with_extension("placeholder-tmp").exists());
        assert!(ledger.exists()); // retain until the OS acknowledges deletion
        assert!(crate::content::Completed::restore(data.path(), owned.id()).is_ok());
    }

    #[test]
    fn manager_failure_reasons_select_only_known_unavailable_degradation() {
        for (code, reason, degraded) in [
            (1, FileProviderReason::RegistrationFailed, true),
            (2, FileProviderReason::BoundExpired, false),
            (3, FileProviderReason::DomainDisabled, true),
            (4, FileProviderReason::ExtensionNotRegistered, true),
            (5, FileProviderReason::DomainDisconnected, true),
        ] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", &format!("exit {code}")]);
            let error = run_manager(command, Instant::now() + MANAGER_TIMEOUT, &|| true).unwrap_err();
            assert_eq!(failure_reason(&error), reason);
            assert_eq!(reason.degraded_promise(), degraded);
        }
        assert!(!FileProviderReason::Cancelled.degraded_promise());
    }

    #[test]
    fn publication_waits_for_urls_beyond_the_old_ui_budget() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 1.1; printf '[\"ready\"]'"]);
        let started = Instant::now();
        let urls = run_manager(command, started + MANAGER_TIMEOUT, &|| true).unwrap();
        assert_eq!(urls, "[\"ready\"]");
        assert!(started.elapsed() > Duration::from_millis(750));
    }

    #[test]
    fn cancelled_preparation_reaps_helper_without_publishing_a_late_url() {
        let mut command = Command::new("/bin/sleep");
        command.arg("10");
        let polls = AtomicUsize::new(0);
        let started = Instant::now();
        let result = run_manager(command, started + MANAGER_TIMEOUT, &|| {
            polls.fetch_add(1, Ordering::Relaxed) < 3
        });
        assert!(matches!(
            result,
            Err(ClipboardError::FileProvider(FileProviderReason::Cancelled))
        ));
        assert!(started.elapsed() < Duration::from_secs(2)); // hang watchdog, not readiness policy
    }

    #[test]
    fn hung_helper_fails_at_the_operation_bound() {
        let mut command = Command::new("/bin/sleep");
        command.arg("10");
        let started = Instant::now();
        let result = run_manager(command, started + Duration::from_millis(50), &|| true);
        assert!(matches!(
            result,
            Err(ClipboardError::FileProvider(FileProviderReason::BoundExpired))
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn restart_rotates_bridge_capability_and_keeps_completed_urls_readable_without_pinning_idle_cache() {
        let data = tempfile::tempdir().unwrap();
        let catalogs = tempfile::tempdir().unwrap();
        let owned = crate::content::Completed::test_single_file(data.path());
        let local = crate::content::Completed::restore(data.path(), owned.id()).unwrap();
        let path = catalogs.path().join("batch.json");
        let (original, _) =
            prepare_batch(Arc::new(local), "batch", "old-capability".into(), 1234, path.clone()).unwrap();
        let urls = vec![catalogs.path().join("stable-provider-url")];
        {
            let mut state = original.state.lock().unwrap();
            state.catalog.urls = urls.clone();
            save_catalog(&path, &state.catalog).unwrap();
        }
        drop((original, owned));
        let restored = restore_catalog(data.path(), &path, "batch", 4321).unwrap();
        let current: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_ne!(current.token, "old-capability");
        assert_eq!(current.port, 4321);
        assert_eq!(current.urls, urls);
        assert_eq!(current.records[1].id, "batch-1");
        assert_eq!(*restored.urls.lock().unwrap(), urls);
        assert!(batch_needed(&restored, &urls));
        assert!(!batch_needed(&restored, &[]));
        let contents = restored.contents.lock().unwrap().clone();
        let mut reader = contents.open(NodeId(1), 7, 0).unwrap();
        assert_eq!(
            crate::store::usage(data.path(), crate::store::StoreKind::CompletedDelivery, true).1,
            0
        );
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"cached");
        drop(reader);
        assert!(crate::store::usage(data.path(), crate::store::StoreKind::CompletedDelivery, true).1 > 0);
        assert!(contents.open(NodeId(1), 7, 0).is_err());
        assert!(restore_catalog(data.path(), &path, "batch", 5678).is_err());
    }

    #[test]
    fn restart_refuses_an_unfinished_delivery_catalog() {
        let data = tempfile::tempdir().unwrap();
        let catalogs = tempfile::tempdir().unwrap();
        let owned = crate::store::Batch::create(
            data.path(),
            crate::store::StoreKind::CompletedDelivery,
            crate::store::BatchId([11; 16]),
        )
        .unwrap();
        let path = catalogs.path().join("batch.json");
        save_catalog(
            &path,
            &Catalog {
                schema: 1,
                token: "old".into(),
                port: 1,
                records: vec![],
                batch: Some(owned.id()),
                urls: vec![],
                received_at: None,
            },
        )
        .unwrap();
        assert!(restore_catalog(data.path(), &path, "batch", 2).is_err());
    }

    fn test_batch(contents: Arc<dyn FileContents>, entries: &[FileEntry]) -> Arc<Batch> {
        prepare_batch(
            Flat::new(entries, contents).unwrap(),
            "batch",
            "secret".into(),
            0,
            PathBuf::new(),
        )
        .unwrap()
        .0
    }
    #[test]
    fn cache_keeps_background_provider_transfer_without_native_readers() {
        struct Busy(Arc<AtomicBool>);
        impl FileContents for Busy {
            fn open(&self, _: usize) -> io::Result<Box<dyn Read + Send>> {
                Err(io::Error::other("unused"))
            }
            fn cache_in_use(&self) -> bool {
                self.0.load(Ordering::Acquire)
            }
        }
        let active = Arc::new(AtomicBool::new(true));
        let batch = test_batch(Arc::new(Busy(active.clone())), &[]);
        assert_eq!(Arc::strong_count(&batch), 1);
        assert!(batch_needed(&batch, &[]));
        active.store(false, Ordering::Release);
        assert!(!batch_needed(&batch, &[]));
    }

    struct Bytes {
        opens: Arc<AtomicUsize>,
        fail: bool,
    }
    impl FileContents for Bytes {
        fn metadata(&self, index: usize) -> io::Result<Option<crate::FileMetadata>> {
            assert_eq!(index, 0);
            Ok(Some(crate::FileMetadata {
                size: b"current version is larger than the manifest".len() as u64,
                modified_ms: Some(1790782863000),
            }))
        }
        fn open(&self, index: usize) -> io::Result<crate::FileReader> {
            assert_eq!(index, 0);
            self.opens.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(crate::content::unavailable(crate::model::Unavailable::Superseded));
            }
            Ok(Box::new(io::Cursor::new(
                b"current version is larger than the manifest".to_vec(),
            )))
        }
    }
    fn exchange(request: &str, fail: bool) -> (Vec<u8>, usize) {
        let catalog_root = tempfile::tempdir().unwrap();
        let opens = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let mut batch = test_batch(
            Arc::new(Bytes {
                opens: opens.clone(),
                fail,
            }),
            &[FileEntry {
                path: "中文.txt".into(),
                is_dir: false,
                size: 1,
                modified_ms: None,
                local_path: None,
            }],
        );
        Arc::get_mut(&mut batch).unwrap().catalog = catalog_root.path().join("batch.json");
        let bridge = Bridge {
            port: address.port(),
            batches: Mutex::new(HashMap::from([("batch".into(), batch)])),
        };
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = serve(stream, &bridge);
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let mut result = Vec::new();
        client.read_to_end(&mut result).unwrap();
        worker.join().unwrap();
        (result, opens.load(Ordering::SeqCst))
    }
    #[test]
    fn superseded_content_retires_the_visible_catalog_instead_of_retrying_its_version() {
        let root = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (batch, _) = prepare_batch(
            Flat::new(
                &[FileEntry {
                    path: "recording.mp4".into(),
                    is_dir: false,
                    size: 1,
                    modified_ms: None,
                    local_path: None,
                }],
                Arc::new(Bytes {
                    opens: Arc::default(),
                    fail: true,
                }),
            )
            .unwrap(),
            "batch",
            "secret".into(),
            address.port(),
            root.path().join("batch.json"),
        )
        .unwrap();
        save_catalog(&batch.catalog, &batch.state.lock().unwrap().catalog).unwrap();
        let bridge = Bridge {
            port: address.port(),
            batches: Mutex::new(HashMap::from([("batch".into(), batch.clone())])),
        };
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            assert!(serve(stream, &bridge).is_err());
            assert!(
                bridge.batches.lock().unwrap().is_empty(),
                "failed publication remains fetchable"
            );
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        client
            .write_all(b"{\"operation\":\"read\",\"token\":\"secret\",\"batch\":\"batch\",\"node\":1}\n")
            .unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).unwrap();
        assert_eq!(
            bytes,
            [255, 255, 255, 255, 2],
            "unavailable publication must report noSuchItem"
        );
        worker.join().unwrap();
        assert!(!batch.catalog.exists(), "unreadable file is still enumerated");
        assert!(batch.state.lock().unwrap().retired);
    }

    #[test]
    fn real_clipboard_read_persists_current_metadata_before_sending_content() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("file");
        std::fs::write(&source, b"old").unwrap();
        let files = crate::content::namespace::Local::clipboard(vec![(NodeId(1), source.clone())]).unwrap();
        let old_version = files.roots()[0].version;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (batch, _) = prepare_batch(
            files,
            "batch",
            "secret".into(),
            address.port(),
            root.path().join("batch.json"),
        )
        .unwrap();
        save_catalog(&batch.catalog, &batch.state.lock().unwrap().catalog).unwrap();
        std::fs::write(&source, b"latest content").unwrap();
        let bridge = Bridge {
            port: address.port(),
            batches: Mutex::new(HashMap::from([("batch".into(), batch.clone())])),
        };
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve(stream, &bridge).unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        client
            .write_all(b"{\"operation\":\"read\",\"token\":\"secret\",\"batch\":\"batch\",\"node\":1}\n")
            .unwrap();
        let mut header = [0; 4];
        client.read_exact(&mut header).unwrap();
        assert_eq!(u32::from_be_bytes(header), u32::MAX - 1);
        client.read_exact(&mut header).unwrap();
        let mut metadata = vec![0; u32::from_be_bytes(header) as usize];
        client.read_exact(&mut metadata).unwrap();
        let metadata: serde_json::Value = serde_json::from_slice(&metadata).unwrap();
        let catalog: Catalog = serde_json::from_slice(&std::fs::read(&batch.catalog).unwrap()).unwrap();
        let record = catalog.records.iter().find(|r| r.node == Some(1)).unwrap();
        assert_eq!(record.size, Some(14));
        assert_eq!(record.version, metadata["version"].as_u64().unwrap());
        assert_ne!(record.version, old_version);
        assert_eq!(record.modified, metadata["modified"].as_i64());
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).unwrap();
        worker.join().unwrap();
        assert_eq!(&rest[4..18], b"latest content");
        assert_eq!(&rest[18..], &[0, 0, 0, 0]);
    }

    #[test]
    fn moved_or_deleted_real_file_withdraws_the_published_item() {
        for deleted in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("file");
            std::fs::write(&source, b"offered").unwrap();
            let files = crate::content::namespace::Local::clipboard(vec![(NodeId(1), source.clone())]).unwrap();
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let (batch, _) = prepare_batch(
                files,
                "batch",
                "secret".into(),
                address.port(),
                root.path().join("batch.json"),
            )
            .unwrap();
            save_catalog(&batch.catalog, &batch.state.lock().unwrap().catalog).unwrap();
            if deleted {
                std::fs::remove_file(&source).unwrap();
            } else {
                std::fs::rename(&source, root.path().join("moved")).unwrap();
            }
            let bridge = Bridge {
                port: address.port(),
                batches: Mutex::new(HashMap::from([("batch".into(), batch.clone())])),
            };
            let worker = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                assert_eq!(
                    crate::content::reason(&serve(stream, &bridge).unwrap_err()),
                    crate::model::Unavailable::NoSuchItem
                );
                assert!(bridge.batches.lock().unwrap().is_empty());
            });
            let mut client = TcpStream::connect(address).unwrap();
            client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            client
                .write_all(b"{\"operation\":\"read\",\"token\":\"secret\",\"batch\":\"batch\",\"node\":1}\n")
                .unwrap();
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).unwrap();
            worker.join().unwrap();
            assert_eq!(bytes, [255, 255, 255, 255, 2]);
            assert!(!batch.catalog.exists());
            assert!(batch.state.lock().unwrap().retired);
        }
    }

    #[test]
    fn retention_withdraws_expired_missing_and_abandoned_catalogs_but_keeps_receiving() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let receiving = crate::store::Batch::create(
            data.path(),
            crate::store::StoreKind::CompletedDelivery,
            crate::store::BatchId([90; 16]),
        )
        .unwrap();
        let owned = crate::content::Completed::test_single_file(data.path());
        let local = crate::content::Completed::restore(data.path(), owned.id()).unwrap();
        let (batch, _) = prepare_batch(
            Arc::new(local),
            "batch",
            "secret".into(),
            0,
            root.path().join("batch.json"),
        )
        .unwrap();
        let bridge = Bridge {
            port: 0,
            batches: Mutex::default(),
        };
        let publish = |id| {
            let mut state = batch.state.lock().unwrap();
            state.retired = false;
            state.catalog.batch = Some(id);
            save_catalog(&batch.catalog, &state.catalog).unwrap();
            bridge.batches.lock().unwrap().insert("batch".into(), batch.clone());
        };
        publish(owned.id());
        retire_unavailable_in(&bridge, data.path(), 0);
        assert!(batch.catalog.exists());
        // A URL owner and an admitted reader survive TTL. Retiring metadata
        // stops new opens but must not interrupt that reader's verified bytes.
        let selected = batch.contents.lock().unwrap().select(NodeId(1)).unwrap();
        let mut reader = batch
            .contents
            .lock()
            .unwrap()
            .open(NodeId(1), selected.version, 0)
            .unwrap();
        retire_unavailable_in(&bridge, data.path(), u64::MAX);
        assert!(!batch.catalog.exists());
        assert!(bridge.batches.lock().unwrap().is_empty());
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert!(!bytes.is_empty());
        publish(receiving.id());
        retire_unavailable_in(&bridge, data.path(), u64::MAX);
        assert!(batch.catalog.exists(), "Receiving has no completed-delivery TTL yet");
        receiving.abandon().unwrap();
        retire_unavailable_in(&bridge, data.path(), 0);
        assert!(!batch.catalog.exists());
        publish(crate::store::BatchId([92; 16]));
        retire_unavailable_in(&bridge, data.path(), 0);
        assert!(!batch.catalog.exists(), "consumed/removed payload still presented");
    }

    #[test]
    fn retention_provider_rerequest_reports_unavailable_without_a_partial_header() {
        let data = tempfile::tempdir().unwrap();
        let catalogs = tempfile::tempdir().unwrap();
        let owned = crate::content::Completed::test_single_file(data.path());
        let local = crate::content::Completed::restore(data.path(), owned.id()).unwrap();
        let path = owned.path().to_owned();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (batch, _) = prepare_batch(
            Arc::new(local),
            "batch",
            "secret".into(),
            address.port(),
            catalogs.path().join("batch.json"),
        )
        .unwrap();
        crate::store::sweep_at(data.path(), u64::MAX);
        assert!(!path.exists());
        let bridge = Bridge {
            port: address.port(),
            batches: Mutex::new(HashMap::from([("batch".into(), batch)])),
        };
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            assert_eq!(serve(stream, &bridge).unwrap_err().kind(), io::ErrorKind::NotFound);
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        client
            .write_all(b"{\"operation\":\"read\",\"token\":\"secret\",\"batch\":\"batch\",\"node\":1}\n")
            .unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).unwrap();
        worker.join().unwrap();
        assert_eq!(bytes, [255, 255, 255, 255, 2]);
    }

    #[test]
    fn content_bridge_requires_capability_and_valid_node() {
        for request in [
            "{\"operation\":\"read\",\"token\":\"wrong\",\"batch\":\"batch\",\"node\":1}\n",
            "{\"operation\":\"read\",\"token\":\"secret\",\"batch\":\"batch\",\"node\":99}\n",
            "{\"operation\":\"read\",\"token\":\"secret\",\"batch\":\"../../path\",\"node\":1}\n",
        ] {
            let (bytes, opens) = exchange(request, false);
            assert!(bytes.is_empty());
            assert_eq!(opens, 0);
        }
    }
    #[test]
    fn content_bridge_frames_current_bytes_without_old_manifest_truncation() {
        let (bytes, opens) = exchange(
            "{\"operation\":\"read\",\"token\":\"secret\",\"batch\":\"batch\",\"node\":1}\n",
            false,
        );
        assert_eq!(&bytes[..4], &(u32::MAX - 1).to_be_bytes());
        let header_len = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let metadata: serde_json::Value = serde_json::from_slice(&bytes[8..8 + header_len]).unwrap();
        assert_eq!(metadata["size"], 43);
        assert_eq!(metadata["modified"], 1790782863000i64);
        let bytes = &bytes[8 + header_len..];
        let count = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(&bytes[4..4 + count], b"current version is larger than the manifest");
        assert_eq!(&bytes[4 + count..], &0u32.to_be_bytes());
        assert_eq!(opens, 1);
    }
    #[test]
    fn content_bridge_withdraws_a_superseded_source_instead_of_successful_eof() {
        let (bytes, opens) = exchange(
            "{\"operation\":\"read\",\"token\":\"secret\",\"batch\":\"batch\",\"node\":1}\n",
            true,
        );
        assert_eq!(&bytes[..4], &u32::MAX.to_be_bytes());
        assert_eq!(bytes[4], 2);
        assert_eq!(opens, 1);
    }
    #[test]
    fn disconnect_releases_only_its_reader_and_never_cancels_the_batch() {
        struct Cancellable(Arc<AtomicUsize>);
        impl FileContents for Cancellable {
            fn open(&self, _: usize) -> io::Result<crate::FileReader> {
                unreachable!()
            }
            fn cancel(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let cancels = Arc::new(AtomicUsize::new(0));
        let batch = test_batch(Arc::new(Cancellable(cancels.clone())), &[]);
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client_a = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server_a, _) = listener.accept().unwrap();
        let client_b = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server_b, _) = listener.accept().unwrap();
        let watch_a = ReadWatch::new(&server_a, batch.clone()).unwrap();
        let watch_b = ReadWatch::new(&server_b, batch.clone()).unwrap();
        drop(client_a);
        drop(watch_a); // join causally observes the disconnected reader
        assert_eq!(*batch.readers.lock().unwrap(), 1);
        assert_eq!(cancels.load(Ordering::SeqCst), 0);
        drop(client_b);
        drop(watch_b);
        assert_eq!(*batch.readers.lock().unwrap(), 0);
        assert_eq!(cancels.load(Ordering::SeqCst), 0);
    }
}

/// Existing authenticated App Group container, if the product has resolved it.
/// Reporting never enables a domain or invokes the manager to obtain a path.
pub fn report_inbox_root() -> Option<PathBuf> {
    CONTAINER.get().cloned()
}
