//! System File Provider catalogs and an authenticated, read-only loopback content bridge.
//! The separate signed manager resolves URLs without marking the Oink process dataless-I/O exempt.
use crate::FileProviderReason;
use crate::model::{ContentSize, DirectoryCursor, FileNode, NodeId, NodeKind};
use std::collections::{HashMap, HashSet};
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
mod moved;
pub(crate) mod native;
pub use announced::{
    Activation, PreparationKey, PreparedDrag, activate_prepared, prepare_announced, prepare_announced_with_cancel,
    revoke_prepared, try_prepare_enter_placeholder,
};

mod preparation_cancel;
pub use preparation_cancel::PreparationCancellation;

mod diagnostics;
mod launch;
mod observer;
mod owner_watch;
mod placeholders;
mod spare;
mod storage_watch;
use crate::provider_preparation::PreparationTiming;
pub use observer::Observer;
pub use placeholders::{configure_placeholder_pool, placeholder_pool_status, placeholder_status};

const MANAGER_TIMEOUT: Duration = Duration::from_secs(12);
struct Batch {
    owner: Weak<Batch>,
    urls: Mutex<Vec<PathBuf>>,
    token: String,
    contents: Mutex<Arc<dyn Namespace>>,
    state: Mutex<CatalogState>,
    catalog: PathBuf,
    readers: Arc<Mutex<usize>>,
}
impl Batch {
    /// Shared with the signed maintenance manager. Keep the inode for the whole
    /// consumer lifetime; no private catalog cleanup unlinks this lock file.
    fn consumer_lock(&self) -> io::Result<std::fs::File> {
        use std::os::fd::AsRawFd;
        if self.catalog.as_os_str().is_empty() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.catalog.with_extension("consumers"))?;
        // SAFETY: the file descriptor remains owned by the returned File. A
        // nonblocking shared flock conflicts with the manager's exclusive flock.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(file)
    }
}
// Watch only this reader. Closing a Finder stream cannot cancel the independent
// whole-drop transfer or readers retained by another native consumer.
struct ReadWatch {
    batch: Arc<Batch>,
    done: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    socket: TcpStream,
    worker: Option<std::thread::JoinHandle<()>>,
    _consumer: std::fs::File,
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
        let state = batch.state.lock().unwrap();
        if state.retired {
            return Err(io::ErrorKind::NotFound.into());
        }
        let consumer = batch.consumer_lock()?;
        *batch.readers.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        drop(state);
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
                _consumer: consumer,
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
/// A target-local publication backed by the OS received store. Persistent links
/// survive clipboard replacement; native consumers hold their own reader leases.
/// Cancellation retires only this publication and its receiver-side namespace.
#[derive(Clone)]
pub struct Registration {
    diagnostic_publication: (Instant, bool),
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
    pub(crate) fn diagnostic_publication(&self) -> (Instant, bool) {
        self.diagnostic_publication
    }
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
                cancel_catalog_in(root, &self.batch.catalog);
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symlink_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exec_bits: Option<u16>,
    size: Option<u64>,
    modified: Option<i64>,
    node: Option<u64>,
    version: u64,
    children: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hidden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    keep_local: Option<bool>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Catalog {
    #[serde(deserialize_with = "crate::store::schema_one")]
    schema: u8,
    token: String,
    port: u16,
    records: Vec<Record>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<crate::content::linked::Descriptor>,
    #[serde(
        default,
        skip_serializing_if = "crate::content::linked::completed::FinalLocal::is_empty"
    )]
    settlement: crate::content::linked::completed::FinalLocal,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    arrived: Vec<NodeId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    failed: Vec<NodeId>,
    urls: Vec<PathBuf>,
    #[serde(default)]
    received_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    move_state: Option<moved::State>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    consumed: Vec<NodeId>,
}
struct CatalogState {
    catalog: Catalog,
    names: Names,
    aliases: HashMap<NodeId, String>,
    retired: bool,
}
impl Catalog {
    fn persistent(&self) -> bool {
        self.source.is_some() || !self.settlement.is_empty()
    }
    fn file_policies(&mut self) {
        let mut pinned: HashSet<String> = self
            .records
            .iter()
            .filter(|record| record.node.is_some_and(|node| self.settlement.contains(NodeId(node))))
            .map(|record| record.id.clone())
            .collect();
        for _ in 0..256 {
            let before = pinned.len();
            for record in &self.records {
                if pinned.contains(&record.parent) {
                    pinned.insert(record.id.clone());
                }
            }
            if before == pinned.len() {
                break;
            }
        }
        for record in &mut self.records {
            record.keep_local = pinned.contains(&record.id).then_some(true);
        }
    }
}
impl CatalogState {
    fn source_available(&self, node: NodeId) -> io::Result<()> {
        // Failed one-shot roots can retain independent local edits. Their
        // records remain readable locally but must never reconnect to the
        // sender or accept a late EOF/listing. Native deletion is owned by the extension.
        let missing = || crate::content::unavailable(crate::model::Unavailable::NoSuchItem);
        if self.retired {
            return Err(missing());
        }
        if self.catalog.failed.is_empty() {
            return Ok(());
        }
        let mut record = self
            .catalog
            .records
            .iter()
            .find(|record| record.node == Some(node.0))
            .ok_or_else(missing)?;
        for _ in 0..256 {
            if record
                .node
                .is_some_and(|node| self.catalog.failed.contains(&NodeId(node)))
            {
                return Err(missing());
            }
            match self.catalog.records.iter().find(|parent| parent.id == record.parent) {
                Some(parent) => record = parent,
                None => return Ok(()),
            }
        }
        Err(io::ErrorKind::InvalidData.into())
    }
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
    #[serde(rename = "received-complete")]
    Complete {
        token: String,
        batch: String,
        node: u64,
        version: u64,
    },
    #[serde(rename = "received-failed")]
    Failed {
        token: String,
        batch: String,
        node: u64,
        version: u64,
    },
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
struct PublicationDiagnostic {
    reason: FileProviderReason,
    detail: Option<String>,
}
static DIAGNOSTIC: Mutex<PublicationDiagnostic> = Mutex::new(PublicationDiagnostic {
    reason: FileProviderReason::DomainEnabled,
    detail: None,
});
static SPARE: Mutex<spare::Pool> = Mutex::new(spare::Pool::new());
static DOMAIN_USABLE: AtomicBool = AtomicBool::new(false);
static STORAGE: Mutex<()> = Mutex::new(());
static BRIDGE: OnceLock<Result<Arc<Bridge>, diagnostics::Failure>> = OnceLock::new();
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
    // Publication attempted while preparation was pending may have recorded
    // Checking/an older failure. Verified native readiness supersedes it.
    {
        let mut diagnostic = DIAGNOSTIC.lock().unwrap_or_else(|e| e.into_inner());
        STARTUP.finish(true);
        diagnostic.reason = FileProviderReason::DomainEnabled;
        diagnostic.detail = None;
    }
    DEMAND_CHANGED.notify_one();
}
/// A failed preparation stays blocked until a new process retries recovery.
pub fn block_after_startup_failure() {
    {
        let mut diagnostic = DIAGNOSTIC.lock().unwrap_or_else(|e| e.into_inner());
        STARTUP.finish(false);
        diagnostic.reason = FileProviderReason::RegistrationFailed;
        diagnostic.detail = Some("launch_preparation_failed; see launch stage error".into());
    }
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
    // container() needs DATA. Construction precedes initialize_store(), so a
    // once-only warm-up there consumed its attempt with Checking and never ran
    // the manager's domain initialization. Start only after the store is set.
    warm_up();
    std::thread::spawn(|| {
        if !STARTUP.wait() {
            return;
        }
        if let Ok(root) = container(Instant::now() + MANAGER_TIMEOUT) {
            let ready = {
                let _storage = STORAGE.lock().unwrap_or_else(|e| e.into_inner());
                bridge(&root).is_ok()
            };
            if ready {
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
    let domains: Vec<DomainStatus> = serde_json::from_str(&json).map_err(|e| error_at("decode_helper_response", e))?;
    Ok(domains.into_iter().find(|d| d.id == "oink-files"))
}

/// Open only Oink's hidden root in Finder. The system Enable button is left to the user.
pub fn open_enable() -> Result<(), ClipboardError> {
    manager(&["open-enable"], Instant::now() + MANAGER_TIMEOUT).map(|_| ())
}
/// Most recent registration/initialization result, suitable for diagnostics (never contains tokens).
pub fn diagnostic() -> String {
    if launch_preparation_pending() {
        return FileProviderReason::Checking.detail();
    }
    let diagnostic = DIAGNOSTIC.lock().unwrap_or_else(|e| e.into_inner());
    diagnostic.detail.clone().unwrap_or_else(|| diagnostic.reason.detail())
}
/// A real domain transition invalidates the previous publication result.
pub fn domain_changed() {
    if STARTUP.result() == Some(true) {
        let mut diagnostic = DIAGNOSTIC.lock().unwrap_or_else(|e| e.into_inner());
        diagnostic.reason = FileProviderReason::DomainEnabled;
        diagnostic.detail = None;
    }
}
pub fn publication_reason() -> FileProviderReason {
    if launch_preparation_pending() {
        return FileProviderReason::Checking;
    }
    DIAGNOSTIC.lock().unwrap_or_else(|e| e.into_inner()).reason
}
pub fn failure_reason(error: &ClipboardError) -> FileProviderReason {
    match error {
        ClipboardError::FileProvider(reason) => *reason,
        _ => FileProviderReason::RegistrationFailed,
    }
}
fn record_result<T>(result: &Result<T, ClipboardError>) {
    // A failed source grant/metadata request says nothing about the installed
    // extension. Previously this was latched as RegistrationFailed, so a single
    // cancelled/stale source disabled every subsequent drag despite an enabled
    // domain. Fail this item with its typed cause and keep domain availability.
    if let Err(ClipboardError::ContentUnavailable(reason)) = result {
        tracing::warn!(
            operation = "source_prepare",
            ?reason,
            "File Provider source unavailable"
        );
        return;
    }
    let reason = match result {
        Ok(_) => FileProviderReason::UrlsReady,
        Err(e) => failure_reason(e),
    };
    // Cancellation describes the gesture, not the extension's availability.
    if reason != FileProviderReason::Cancelled {
        let mut diagnostic = DIAGNOSTIC.lock().unwrap_or_else(|e| e.into_inner());
        // A publication can return its pending-gate error after readiness was
        // verified. Do not let that late result reinstate a stale Checking state.
        if reason == FileProviderReason::Checking && !launch_preparation_pending() {
            return;
        }
        diagnostic.reason = reason;
        // All Platform errors reaching this private publication boundary are
        // constructed by error_at from code-only diagnostics. Source errors keep
        // their typed ContentUnavailable above; never use their Display text.
        diagnostic.detail = result.as_ref().err().map(ToString::to_string);
        if let Some(cause) = &diagnostic.detail {
            tracing::warn!(operation = "publish", %reason, %cause, "File Provider publication failed");
        }
        DEMAND_CHANGED.notify_one();
    }
}
#[track_caller]
fn error(e: impl Into<diagnostics::Failure>) -> ClipboardError {
    error_at("local_operation", e)
}
#[track_caller]
fn error_at(operation: &'static str, e: impl Into<diagnostics::Failure>) -> ClipboardError {
    let diagnostics::Failure(cause) = e.into();
    tracing::warn!(operation, %cause, error_site = std::panic::Location::caller().line(),
        "File Provider operation failed");
    ClipboardError::Platform(format!("File Provider {operation}: {cause}"))
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
    let result = run_manager(command, deadline, current);
    if result.is_err() {
        // Only the command verb is logged, never batch IDs or other arguments.
        tracing::warn!(
            operation = args.first().copied().unwrap_or("none"),
            "File Provider helper operation failed"
        );
    }
    result
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
        // Observe a completed rejection before the clock check. Otherwise an
        // error already returned by the helper could be relabelled as timeout.
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error(e));
            }
        }
        if let Err(error) = check_publication(deadline, current) {
            let _ = child.kill();
            let _ = child.wait();
            break Err(error);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = out
        .join()
        .map_err(|_| error("manager output thread failed"))?
        .map_err(error)?;
    let stderr = err
        .join()
        .map_err(|_| error("manager error thread failed"))?
        .map_err(error)?;
    // Helper output can contain user paths and bridge capabilities. Never log it.
    let status = status?;
    if !status.success() {
        let reason = match status.code() {
            Some(2) => FileProviderReason::BoundExpired,
            Some(3) => FileProviderReason::DomainDisabled,
            Some(4) => FileProviderReason::ExtensionNotRegistered,
            Some(5) => FileProviderReason::DomainDisconnected,
            Some(32) => FileProviderReason::ItemMissing,
            Some(33) => FileProviderReason::ItemUnsupportedType,
            Some(34) => FileProviderReason::ItemKindMismatch,
            Some(35) => FileProviderReason::ItemTargetMismatch,
            Some(36) => FileProviderReason::ItemIdentityMismatch,
            Some(37) => FileProviderReason::ItemMetadataMismatch,
            Some(38) => FileProviderReason::ItemAccessDenied,
            Some(39) => FileProviderReason::ItemMetadataUnreadable,
            _ => FileProviderReason::RegistrationFailed,
        };
        tracing::warn!(exit_code = status.code(), %reason, cause = %diagnostics::helper_error(&stderr),
            "File Provider helper rejected operation");
        if reason == FileProviderReason::RegistrationFailed {
            return Err(error_at(
                "helper",
                diagnostics::Failure(format!("{status}; {}", diagnostics::helper_error(&stderr))),
            ));
        }
        return Err(ClipboardError::FileProvider(reason));
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
    let data = DATA
        .get()
        .ok_or(ClipboardError::FileProvider(FileProviderReason::Checking))?;
    crate::upgrade::prepare(data, || Ok(())).map_err(|e| error_at("prepare_local_store", e))?;
    let root = PathBuf::from(manager_while(&["container"], deadline, current)?);
    watch_storage(&root);
    let _ = CONTAINER.set(root.clone());
    Ok(root)
}
/// Register/inspect the hidden domain off the input and UI threads. Never handles an authorization UI.
fn warm_up() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::spawn(|| {
            if !STARTUP.wait() {
                return;
            }
            let result = warm_up_domain(
                || container(Instant::now() + MANAGER_TIMEOUT),
                |_| manager(&["initialize"], Instant::now() + MANAGER_TIMEOUT).map(|_| ()),
                || DEMAND_CHANGED.notify_one(),
            );
            // Domain enablement is not evidence that a new item's URL is ready.
            if let Err(e) = result {
                tracing::info!(%e, "File Provider initialization unavailable");
            }
        });
    });
}
fn warm_up_domain<T, E>(
    container: impl FnOnce() -> Result<T, E>,
    initialize: impl FnOnce(&T) -> Result<(), E>,
    changed: impl FnOnce(),
) -> Result<(), E> {
    let result = container().and_then(|root| initialize(&root));
    // An earlier read may have observed an absent domain. Registration completes
    // in the separate manager, so explicitly refresh the read-only observer even
    // if no NSFileProviderDomainDidChange notification arrives in this process.
    changed();
    result
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
            let data = DATA
                .get()
                .ok_or_else(|| diagnostics::Failure::from("content store not configured"))?;
            let listener =
                Arc::new(TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(diagnostics::Failure::from)?);
            let bridge = Arc::new(Bridge {
                port: listener.local_addr().map_err(diagnostics::Failure::from)?.port(),
                batches: Mutex::new(HashMap::new()),
            });
            *bridge.batches.lock().unwrap() =
                recover_catalogs(data, root, bridge.port).map_err(diagnostics::Failure::from)?;
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
                    .map_err(diagnostics::Failure::from)?;
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
    let _mutation = crate::cache::Mutation(root);
    let mut batches = HashMap::new();
    for key in placeholders::ledger_keys(root) {
        cancel_catalog_in(root, &root.join(format!("{key}.json")));
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
                Err(_) => {
                    remove_catalog_in(root, &path);
                }
            }
        }
    }
    Ok(batches)
}
fn save_catalog(path: &std::path::Path, catalog: &Catalog) -> io::Result<()> {
    catalog.settlement.validate(catalog.source.as_ref())?;
    let bytes = serde_json::to_vec(catalog)?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    // Accounting may retain an unchanged catalog. Do not let that read path
    // rewrite it and schedule itself again through explicit owner invalidation.
    if std::fs::read(path).is_ok_and(|previous| previous == bytes) {
        // Preserve the commit barrier if an earlier rename succeeded but its
        // directory flush failed. The file was flushed before that rename.
        return crate::store::sync_directory(path.parent().unwrap());
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
    // A failed durability step can follow the committed rename. Failed writes
    // that never replaced the catalog must not retrigger a failed usage scan.
    if result.is_ok() || std::fs::read(path).is_ok_and(|current| current == bytes) {
        crate::cache::changed(path.parent().unwrap());
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
        symlink_target: node.kind.symlink_target().map(str::to_owned),
        exec_bits: node.exec_bits,
        size: match node.size {
            ContentSize::Known(n) => Some(n),
            ContentSize::Unknown => None,
        },
        modified: node.modified_ms,
        node: Some(node.id.0),
        version: node.version,
        children: None,
        hidden: None,
        keep_local: None,
    })
}
/// A native URL can be read long after its metadata was enumerated. Persist
/// the selected version before sending its first byte so subsequent item/working
/// set queries agree with fetchContents' returned Item. A retired catalog must
/// never be recreated by a reader admitted before its deletion.
fn refresh_file_record(batch: &Batch, entry: &FileNode) -> io::Result<bool> {
    let mut state = batch.state.lock().unwrap();
    state.source_available(entry.id)?;
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
    if old.size == size
        && old.modified == entry.modified_ms
        && old.version == entry.version
        && old.exec_bits == entry.exec_bits
    {
        return Ok(false);
    }
    let current = &mut state.catalog.records[index];
    current.size = size;
    current.modified = entry.modified_ms;
    current.version = entry.version;
    current.exec_bits = entry.exec_bits;
    if let Err(error) = state.persist(&batch.catalog) {
        state.catalog.records[index] = old;
        return Err(error);
    }
    Ok(true)
}

/// A promise can enumerate through NativeFiles without a simultaneous system
/// enumerator. Publish those children before returning their IDs: received-read
/// and received-stat resolve only catalogued native items. Use the same commit
/// as the extension's List request; background whole-tree download is not an
/// ordering barrier for an individual promise callback.
fn persist_listing(
    batch: &Batch,
    node: NodeId,
    cursor: Option<crate::model::DirectoryCursor>,
    page: &crate::model::DirectoryPage,
) -> io::Result<Vec<Record>> {
    let key = batch
        .catalog
        .file_stem()
        .and_then(|key| key.to_str())
        .ok_or(io::ErrorKind::InvalidData)?;
    let mut state = batch.state.lock().unwrap();
    state.source_available(node)?;
    let mut records = Vec::new();
    for child in &page.entries {
        let CatalogState { names, aliases, .. } = &mut *state;
        let record = node_record_with_aliases(key, child, names, aliases)?;
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
    state.catalog.file_policies();
    state.persist(&batch.catalog)?;
    Ok(records)
}

impl Batch {
    fn source_available(&self, node: NodeId) -> io::Result<()> {
        self.state.lock().unwrap().source_available(node)
    }
}

fn serve(mut stream: TcpStream, bridge: &Bridge) -> io::Result<()> {
    let admission = (|| {
        let update_activity = oink_foundation::maintenance::process()
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
            Request::Read { token, batch, node }
            | Request::List { token, batch, node, .. }
            | Request::Complete { token, batch, node, .. }
            | Request::Failed { token, batch, node, .. } => (token, batch, NodeId(*node)),
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
        let batch_id = batch_id.clone();
        Ok((update_activity, request, batch, node, batch_id))
    })();
    let (_update_activity, request, batch, node, batch_id) = admission.inspect_err(|error| {
        reject_request(&mut stream, error);
    })?;
    match request {
        Request::Complete { version, .. } => {
            batch.received_complete(node, version)?;
            return stream.write_all(&[0]);
        }
        Request::Failed { version, .. } => {
            if batch
                .state
                .lock()
                .unwrap()
                .catalog
                .records
                .iter()
                .any(|record| record.node == Some(node.0) && record.version == version)
            {
                batch.received_failed(node)?;
            }
            return stream.write_all(&[0]);
        }
        _ => {}
    }
    let reader = (|| {
        batch.source_available(node)?;
        ReadWatch::new(&stream, batch.clone())
    })();
    let _watch = reader.inspect_err(|error| {
        reject_request(&mut stream, error);
    })?;
    let contents = batch.contents.lock().unwrap().clone();
    let content_read = matches!(request, Request::Read { .. });
    let result: io::Result<()> = (|| match request {
        Request::Complete { .. } | Request::Failed { .. } => unreachable!(),
        Request::List { cursor, .. } => {
            let page = contents.list(node, cursor)?;
            let records = persist_listing(&batch, node, cursor, &page)?;
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
                &serde_json::json!({ "schema": 1, "size": size, "modified": entry.modified_ms, "version": entry.version, "exec_bits": entry.exec_bits }),
            )?;
            stream.write_all(&(u32::MAX - 1).to_be_bytes())?;
            stream.write_all(&(header.len() as u32).to_be_bytes())?;
            stream.write_all(&header)?;
            let mut buffer = vec![0; 256 * 1024];
            loop {
                if !_watch.connected.load(Ordering::Acquire) {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                batch.source_available(node)?;
                let count = reader.read(&mut buffer)?;
                batch.source_available(node)?;
                stream.write_all(&(count as u32).to_be_bytes())?;
                if count == 0 {
                    return Ok(());
                }
                stream.write_all(&buffer[..count])?;
            }
        }
    })();
    if let Err(ref error) = result {
        let _ = batch.received_failed(node);
        let reason = crate::content::reason(error);
        let linked = batch.state.lock().unwrap().catalog.persistent();
        let unavailable = !linked
            && (matches!(
                reason,
                crate::model::Unavailable::NoSuchItem | crate::model::Unavailable::Cancelled
            ) || (content_read && reason == crate::model::Unavailable::Superseded));
        if unavailable {
            tracing::info!(
                ?reason,
                "File Provider publication withdrawn after permanent content failure"
            );
            retire_publication(bridge, &batch_id, &batch);
        }
        let code = match reason {
            _ if unavailable => 2,
            _ if linked
                && matches!(
                    error.kind(),
                    io::ErrorKind::NotConnected
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::TimedOut
                ) =>
            {
                1
            }
            crate::model::Unavailable::ReadFailed if linked => 5,
            crate::model::Unavailable::NoProgress => 1,
            crate::model::Unavailable::NoSuchItem | crate::model::Unavailable::ReadFailed => 2,
            crate::model::Unavailable::Superseded => 3,
            crate::model::Unavailable::Cancelled => 4,
            crate::model::Unavailable::Permission => 6,
            _ => 5,
        };
        tracing::debug!(?reason, code, "File Provider content request failed");
        let _ = stream
            .write_all(&u32::MAX.to_be_bytes())
            .and_then(|_| stream.write_all(&[code]));
    }
    result
}

// Rejections before streaming still need the bridge's typed failure frame.
// An unframed EOF means a broken loopback connection to the extension (-1004),
// which hides a withdrawn item or denied capability as a network outage.
fn reject_request(stream: &mut TcpStream, error: &io::Error) {
    let reason = crate::content::reason(error);
    let code = match reason {
        crate::model::Unavailable::NoSuchItem => 2,
        crate::model::Unavailable::Superseded => 3,
        crate::model::Unavailable::Cancelled => 4,
        crate::model::Unavailable::Permission => 6,
        crate::model::Unavailable::NoProgress => 1,
        _ => 5,
    };
    tracing::debug!(?reason, code, "File Provider request rejected before streaming");
    let _ = stream
        .write_all(&u32::MAX.to_be_bytes())
        .and_then(|_| stream.write_all(&[code]));
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
            source: None,
            settlement: Default::default(),
            arrived: Vec::new(),
            failed: Vec::new(),
            urls: Vec::new(),
            received_at: None,
            move_state: None,
            consumed: Vec::new(),
        },
        Err(e) => return Err(e),
    };
    for index in (0..catalog.records.len()).rev() {
        let record = &catalog.records[index];
        let overlay: serde_json::Value = match std::fs::read(root.join(format!("{}.received", record.id))) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => serde_json::Value::Null,
            Err(e) => return Err(e),
        };
        let entry = &overlay[&record.id];
        let parent = entry["record"]["parent"].as_str().unwrap_or(&record.parent);
        if entry["deleted"] != true && parent == "NSFileProviderRootContainerItemIdentifier" {
            let id = record.id.clone();
            if record.modified.is_none() {
                // A source container has no source-file mtime. Use its real
                // catalog creation time once, keeping its identity/path intact.
                catalog.records[index].modified = Some(
                    std::fs::metadata(&path)?
                        .created()?
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_err(io::Error::other)?
                        .as_millis() as i64,
                );
                save_catalog(&path, &catalog)?;
            }
            return Ok(id);
        }
    }
    let id = if catalog.records.is_empty() {
        base
    } else {
        format!("{base}g{}", random()?)
    };
    catalog.records.push(Record {
        exec_bits: None,
        symlink_target: None,
        id: id.clone(),
        parent: "NSFileProviderRootContainerItemIdentifier".into(),
        name,
        directory: true,
        size: None,
        modified: Some(crate::received::now() as i64),
        node: None,
        version: 1,
        children: None,
        hidden: None,
        keep_local: None,
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
    let private_parent = roots.is_empty() && received_at.is_none();
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
        exec_bits: None,
        symlink_target: None,
        id: format!("{id}-root"),
        parent: source_parent,
        name: delivery_name,
        directory: true,
        size: None,
        modified: previous_root.map_or(received_at.map(|at| at as i64), |record| record.modified),
        node: None,
        version: 1,
        children: Some(roots.len() as u64),
        hidden: None,
        keep_local: None,
    }];
    let mut entries = Vec::new();
    for node in roots {
        records.push(node_record(id, &node, &mut names)?);
        entries.push(names.entry(&node)?);
    }
    let source = contents.source_link();
    let batch = Arc::new_cyclic(|owner| Batch {
        owner: owner.clone(),
        urls: Mutex::default(),
        token: token.clone(),
        contents: Mutex::new(contents),
        state: Mutex::new(CatalogState {
            catalog: Catalog {
                schema: 1,
                token,
                port,
                records,
                source,
                settlement: Default::default(),
                arrived: Vec::new(),
                failed: Vec::new(),
                urls: Vec::new(),
                received_at,
                move_state: None,
                consumed: Vec::new(),
            },
            names,
            aliases: HashMap::new(),
            retired: false,
        }),
        catalog,
        readers: Arc::new(Mutex::new(0)),
    });
    // Empty stock is internal, just like typed stock. Never show its random
    // publication key as a received folder while it awaits a real namespace.
    batch.state.lock().unwrap().catalog.file_policies();
    if private_parent {
        placeholders::hide_parent(id, &mut batch.state.lock().unwrap().catalog);
    }
    Ok((batch, entries))
}

fn restore_catalog(data: &std::path::Path, path: &std::path::Path, key: &str, port: u16) -> io::Result<Arc<Batch>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if crate::cache::is_link(&metadata) || !metadata.is_file() || metadata.len() > 64 * 1024 * 1024 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let legacy = crate::content::linked::completed::legacy_pending_roots(&value["source"]);
    let mut previous: Catalog = serde_json::from_value(value)?;
    native::recover_legacy_received(&mut previous, key, path, &legacy)?;
    let unfinished = previous
        .source
        .as_ref()
        .is_some_and(|source| !source.fulfilled.is_empty());
    native::discard_incomplete(&mut previous, key, path)?;
    if unfinished && !previous.persistent() {
        cancel_catalog_in(path.parent().ok_or(io::ErrorKind::InvalidData)?, path);
        return Err(io::ErrorKind::NotFound.into());
    }
    if previous.source.as_ref().is_some_and(|source| source.provisional) {
        cancel_catalog_in(path.parent().ok_or(io::ErrorKind::InvalidInput)?, path);
        return Err(io::ErrorKind::Interrupted.into());
    }
    let (contents, nodes): (Arc<dyn Namespace>, Vec<FileNode>) = if previous.persistent() {
        let contents = previous
            .settlement
            .restore(previous.source.clone(), crate::content::linked::native_connector())?;
        (contents, linked_catalog_nodes(key, &previous)?)
    } else {
        let _ = data;
        return Err(io::ErrorKind::NotFound.into());
    };
    let token = random()?;
    let (batch, _) = prepare_batch_with_root(contents, key, token, port, path.to_owned(), previous.records.first())?;
    batch.state.lock().unwrap().catalog.received_at = previous.received_at;
    {
        let mut state = batch.state.lock().unwrap();
        state.catalog.settlement = previous.settlement.clone();
        state.catalog.move_state = previous.move_state;
        state.catalog.consumed = previous.consumed.clone();
        state.retired = previous.move_state == Some(moved::State::Removed);
        state.catalog.arrived = previous.arrived.clone();
        state.catalog.failed = previous.failed.clone();
        placeholders::restore_bindings(key, &previous, &mut state)?;
        if let (Some(old), Some(new)) = (previous.records.first(), state.catalog.records.first_mut()) {
            new.parent = old.parent.clone();
            new.name = old.name.clone();
        }
        for node in nodes.iter().filter(|n| n.parent.is_some()) {
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
                    && old.symlink_target == new.symlink_target
                    && old.exec_bits == new.exec_bits
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
        if previous.persistent() {
            // Retain partial directory views and the metadata most recently
            // observed by native fetches. Recovery does not enumerate a source.
            state.catalog.records = previous.records.clone();
        }
        state.catalog.urls = previous.urls.clone();
        state.catalog.file_policies();
        save_catalog(path, &state.catalog)?;
    }
    *batch.urls.lock().unwrap() = previous.urls;
    native::recover(&batch)?;
    Ok(batch)
}

fn linked_catalog_nodes(key: &str, catalog: &Catalog) -> io::Result<Vec<FileNode>> {
    use std::collections::HashSet;
    if catalog.records.len() > crate::model::MAX_DISCOVERED_NODES + 1 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let root = format!("{key}-root");
    let mut ids = HashSet::new();
    let mut nodes = HashSet::new();
    let mut parents = HashMap::new();
    let mut result = Vec::new();
    for record in &catalog.records {
        if !ids.insert(&record.id) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let Some(id) = record.node.map(NodeId) else {
            if record.id != root || !record.directory {
                return Err(io::ErrorKind::InvalidData.into());
            }
            continue;
        };
        if !nodes.insert(id) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let parent = if record.parent == root {
            None
        } else {
            Some(*parents.get(&record.parent).ok_or(io::ErrorKind::InvalidData)?)
        };
        let node = FileNode {
            exec_bits: record.exec_bits,
            id,
            parent,
            name: record.name.clone(),
            kind: if let Some(target) = &record.symlink_target {
                NodeKind::Symlink(target.clone())
            } else if record.directory {
                NodeKind::Directory
            } else {
                NodeKind::File
            },
            size: record.size.map_or(ContentSize::Unknown, ContentSize::Known),
            version: record.version,
            modified_ms: record.modified,
        };
        if !node.valid() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if record.directory {
            parents.insert(record.id.clone(), id);
        }
        result.push(node);
    }
    Ok(result)
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
    if timing.placeholder.used
        && result.as_ref().err().is_some_and(|error| {
            !matches!(error, ClipboardError::ContentUnavailable(_)) && !failure_reason(error).item_rejected()
        })
    {
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
    let started = Instant::now();
    let mut waited = false;
    loop {
        if let Err(error) = check_publication(deadline, current) {
            tracing::info!(
                dnd_phase = 56_u64,
                elapsed_us = started.elapsed().as_micros() as u64,
                reason = failure_reason(&error) as u64,
                "File Provider catalog lock wait ended"
            );
            return Err(error);
        }
        let guard = match STORAGE.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        if let Some(guard) = guard {
            if waited {
                tracing::info!(
                    dnd_phase = 56_u64,
                    elapsed_us = started.elapsed().as_micros() as u64,
                    reason = 0_u64,
                    "File Provider catalog lock acquired"
                );
            }
            return Ok(guard);
        }
        waited = true;
        std::thread::sleep(Duration::from_millis(5));
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
    let contents: Arc<dyn Namespace> = match contents.namespace() {
        Some(namespace) => namespace,
        None => Flat::new(entries, contents).map_err(|e| error_at("source_namespace", e))?,
    };
    contents.prepare().map_err(|error| {
        let reason = crate::content::reason(&error);
        tracing::warn!(operation = "source_prepare", ?reason, cause = %diagnostics::Failure::from(error).0,
            "File Provider source preparation failed");
        ClipboardError::ContentUnavailable(reason)
    })?;
    let _storage = storage_until(deadline, current)?;
    let root = container_while(deadline, current)?;
    let bridge = bridge(&root)?;
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
    let helper_started = Instant::now();
    tracing::info!(
        dnd_phase = 58_u64,
        preparation_generation = trace.generation,
        provider_path = if typed { 2_u64 } else { 1_u64 },
        active = true,
        "File Provider publication helper started"
    );
    let managed = manager_while(&[command, &batch_id], deadline, &publication_current);
    tracing::info!(
        dnd_phase = 58_u64,
        preparation_generation = trace.generation,
        provider_path = if typed { 2_u64 } else { 1_u64 },
        active = false,
        elapsed_us = helper_started.elapsed().as_micros() as u64,
        success = managed.is_ok(),
        reason = managed.as_ref().err().map(failure_reason).map(|r| r as u64),
        "File Provider publication helper finished"
    );
    let result = managed.and_then(|json| {
        check_publication(deadline, &publication_current)?;
        let paths: Vec<PathBuf> = serde_json::from_str(&json).map_err(|e| error_at("decode_helper_response", e))?;
        let _storage = storage_until(deadline, &publication_current)?;
        if paths.len() != entries.len() {
            return Err(error("incomplete File Provider URL list"));
        }
        *batch.urls.lock().unwrap_or_else(|e| e.into_inner()) = paths.clone();
        {
            let mut state = batch.state.lock().unwrap();
            state.catalog.urls = paths.clone();
            save_catalog(&catalog, &state.catalog).map_err(|e| error_at("save_catalog", e))?;
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
            diagnostic_publication: (Instant::now(), typed),
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
    if result.is_err() {
        bridge
            .batches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&batch_id);
        retire_catalog(&batch, &|| cancel_catalog_in(&root, &catalog));
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
        save_catalog(catalog, &batch.state.lock().unwrap().catalog).map_err(|e| error_at("save_catalog", e))?;
        let contents = batch.contents.lock().unwrap().clone();
        native::bind(&batch, &contents).map_err(|e| error_at("bind_native", e))?;
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
fn cancel_catalog_in(root: &std::path::Path, path: &std::path::Path) {
    assert_eq!(path.parent(), Some(root), "catalog cancellation escaped Oink storage");
    if std::fs::symlink_metadata(root).is_ok_and(|m| !crate::cache::is_link(&m)) {
        // Explicit failure withdraws provisional paths, including hydrated
        // previews. Retention, in contrast, preserves delivered user payload.
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("enumerated"));
        crate::cache::changed(root);
    }
}
fn received_entry_protected(value: &serde_json::Value) -> bool {
    // A warmed private parent is materialized, but has received no user data.
    // Match the extension's overlay admission rule, including stale snapshots
    // encountered while a typed parent is being renamed or retired.
    value["record"]["hidden"] != true
        && !(value["record"]["directory"] == true
            && value["record"]["node"].is_null()
            && value["modified_locally"] != true)
        && value["deleted"] != true
        && (value["handed_off"] == true || value["materialized"] == true || value["fetching"].as_u64().unwrap_or(0) > 0)
}
fn received_catalog_protected(path: &std::path::Path) -> bool {
    if path.extension().is_some_and(|e| e == "json") {
        match std::fs::read(path).map(|b| serde_json::from_slice::<Catalog>(&b)) {
            Ok(Ok(catalog)) if catalog.persistent() => return true,
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
            .is_none_or(|entries| entries.values().any(received_entry_protected)),
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
        if std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Catalog>(&bytes).ok())
            .is_some_and(|catalog| catalog.persistent())
        {
            // Metadata and source credentials are the received item itself.
            // Cache maintenance cannot sever a never-hydrated user's path.
            // Explicit cancellation uses cancel_catalog_in instead.
            return;
        }
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
                    .filter(|(_, v)| received_entry_protected(v))
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
                    // Local imports exist only in the extension overlay. Keep
                    // their catalog ancestors so the overlay remains discoverable.
                    for (id, value) in overlay.as_object().ok_or(io::ErrorKind::InvalidData)? {
                        if keep.contains(id) && value["modified_locally"] == true {
                            keep.insert(
                                value["record"]["parent"]
                                    .as_str()
                                    .ok_or(io::ErrorKind::InvalidData)?
                                    .into(),
                            );
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

fn cache_storage(storage: &Mutex<()>, clear: bool) -> Option<std::sync::MutexGuard<'_, ()>> {
    // Usage is an eventually consistent read. Holding STORAGE across the native
    // cache helper (up to 12s) serialized later drag publications behind the
    // previous drag's accounting. Only destructive clear needs that exclusion.
    clear.then(|| storage.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Called only by the native owner, on request or storage notification; never registers a domain.
pub fn cache_usage(clear: bool) -> (Vec<crate::cache::CacheSize>, u64, bool, Vec<crate::store::Batch>) {
    use crate::cache::{CacheCategory, CacheSize};
    let _storage = cache_storage(&STORAGE, clear);
    let root = match container(Instant::now() + MANAGER_TIMEOUT) {
        Ok(root) => root,
        Err(_) => return (Vec::new(), 0, true, Vec::new()),
    };
    let root = &root;
    let _mutation = clear.then(|| crate::cache::Mutation(root));
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
    let mut catalog_incomplete =
        STORAGE_WATCH.get().is_none_or(|r| r.is_err()) || OWNER_WATCH.get().is_none_or(|r| r.is_err());
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
                && let Some(pin) = batch.contents.lock().unwrap().cache_batch()
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

static OWNER_WATCH: OnceLock<Result<owner_watch::OwnerWatch, String>> = OnceLock::new();
static STORAGE_WATCH: OnceLock<Result<Mutex<storage_watch::StorageWatch>, String>> = OnceLock::new();
fn watch_storage(root: &std::path::Path) {
    OWNER_WATCH.get_or_init(|| owner_watch::OwnerWatch::new(root));
    STORAGE_WATCH.get_or_init(|| storage_watch::StorageWatch::new(root).map(Mutex::new));
}

pub(crate) fn cache_root() -> Option<PathBuf> {
    CONTAINER.get().cloned()
}
fn batch_needed(batch: &Arc<Batch>, sources: &[PathBuf]) -> bool {
    batch.state.lock().unwrap().catalog.persistent()
        || Arc::strong_count(batch) > 1
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
    #[test]
    fn read_only_cache_helper_does_not_block_foreground_publication() {
        let storage = Arc::new(Mutex::new(()));
        let cache = storage.clone();
        let (entered, ready) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _scan = cache_storage(&cache, false);
            entered.send(()).unwrap();
            released.recv().unwrap();
        });
        ready.recv().unwrap();
        let foreground_can_publish = storage.try_lock().is_ok();
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            foreground_can_publish,
            "a read-only File Provider helper serialized the next drag's catalog publication"
        );
        assert!(cache_storage(&storage, true).is_some(), "clear still needs exclusion");
    }

    #[test]
    fn advertised_grant_publishes_app_links_and_execution_bits_to_provider_catalog() {
        use crate::content::{grants::SourceGrants, namespace::Local};
        use std::{
            fs,
            os::unix::fs::{PermissionsExt, symlink},
        };
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("Fixture.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::create_dir_all(app.join("Contents/Resources/nested")).unwrap();
        fs::write(app.join("Contents/MacOS/fixture"), b"executable").unwrap();
        fs::set_permissions(app.join("Contents/MacOS/fixture"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(app.join("Contents/Resources/nested/content.txt"), b"resource").unwrap();
        symlink("nested", app.join("Contents/Resources/current")).unwrap();
        symlink("nested/content.txt", app.join("Contents/Resources/linked.txt")).unwrap();
        let source = Local::new(vec![(NodeId(1), app)]).unwrap();
        let peer = oink_foundation::DeviceId([2; 32]);
        let grants = SourceGrants::load(&temp.path().join("grants"), [(peer, [1; 32])]).unwrap();
        grants.set_peer_safe_symlinks(
            peer,
            crate::negotiation::CAPABILITIES.contains(&"oink.files.safe-symlinks/1"),
        );
        grants.set_peer_exec_bits(
            peer,
            crate::negotiation::CAPABILITIES.contains(&"oink.files.exec-bits/1"),
        );
        let token = grants.issue(peer, source.as_ref(), &[NodeId(1)]).unwrap();
        let received = grants.open(peer, token).unwrap();
        let key = "e".repeat(64);
        let catalog = temp.path().join(format!("{key}.json"));
        let (batch, _) = prepare_batch(received.clone(), &key, "token".into(), 1, catalog.clone()).unwrap();
        let mut directories = vec![NodeId(1)];
        while let Some(id) = directories.pop() {
            let page = received.list(id, None).unwrap();
            persist_listing(&batch, id, None, &page).unwrap();
            directories.extend(
                page.entries
                    .iter()
                    .filter(|node| node.kind == NodeKind::Directory)
                    .map(|node| node.id),
            );
        }
        let catalog: Catalog = serde_json::from_slice(&fs::read(catalog).unwrap()).unwrap();
        let item = |name: &str| catalog.records.iter().find(|record| record.name == name).unwrap();
        assert_eq!(item("current").symlink_target.as_deref(), Some("nested"));
        assert_eq!(item("linked.txt").symlink_target.as_deref(), Some("nested/content.txt"));
        assert_eq!(item("fixture").exec_bits, Some(0o111));
        assert!(
            catalog
                .records
                .iter()
                .filter(|record| record.directory)
                .all(|record| record.exec_bits.is_none())
        );
    }
    #[test]
    fn warm_up_refreshes_initial_absent_domain_after_initialization_without_user_action() {
        use std::cell::{Cell, RefCell};
        // Fake getDomains initially runs before the manager's registration. The
        // completion wake must re-read actual flags even without an OS notice.
        let domain = Cell::new(false);
        let observed = Cell::new(false);
        let calls = RefCell::new(Vec::new());
        super::warm_up_domain(
            || {
                calls.borrow_mut().push("container with initialized DATA");
                Ok::<_, &str>("root")
            },
            |root| {
                assert_eq!(*root, "root");
                calls.borrow_mut().push("initialize domain");
                domain.set(true);
                Ok(())
            },
            || {
                calls.borrow_mut().push("refresh read-only domain flags");
                observed.set(domain.get());
            },
        )
        .unwrap();
        assert!(observed.get());
        assert_eq!(
            *calls.borrow(),
            [
                "container with initialized DATA",
                "initialize domain",
                "refresh read-only domain flags"
            ]
        );
        // Failure is also observed; it never becomes a fabricated enabled value.
        assert!(
            super::warm_up_domain(
                || Err::<(), _>("container unavailable"),
                |_| -> Result<(), &str> { panic!("failed container initialized domain") },
                || observed.set(false),
            )
            .is_err()
        );
        assert!(!observed.get());
    }

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn source_preparation_failure_does_not_disable_a_healthy_provider() {
        const CHILD: &str = "OINK_TEST_PROVIDER_SOURCE_FAILURE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            assert!(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "macos::file_provider::tests::source_preparation_failure_does_not_disable_a_healthy_provider"
                    ])
                    .env(CHILD, "1")
                    .status()
                    .unwrap()
                    .success()
            );
            return;
        }
        struct UnavailableSource(crate::model::Unavailable);
        impl Namespace for UnavailableSource {
            fn prepare(&self) -> io::Result<()> {
                Err(crate::content::unavailable(self.0))
            }
            fn roots(&self) -> Vec<FileNode> {
                linked_fixture().roots()
            }
            fn list(&self, _: NodeId, _: Option<DirectoryCursor>) -> io::Result<crate::model::DirectoryPage> {
                panic!("failed source was enumerated")
            }
            fn select(&self, _: NodeId) -> io::Result<FileNode> {
                panic!("failed source was selected")
            }
            fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<crate::FileReader> {
                panic!("failed source was opened")
            }
        }
        finish_launch_preparation(true);
        let clipboard = super::super::MacClipboard {
            name: Some(format!("com.hachiwii.oink.test.source-failure.{}", std::process::id())),
            watcher: Mutex::new(None),
            registration: Arc::new(Mutex::new(None)),
            local: Arc::new(Mutex::new(None)),
        };
        for cause in [
            crate::model::Unavailable::Cancelled,
            crate::model::Unavailable::Superseded,
            crate::model::Unavailable::Permission,
            crate::model::Unavailable::NoSuchItem,
        ] {
            let source = crate::content::namespace::Files::new(
                Arc::new(UnavailableSource(cause)),
                crate::sync::files::TargetOs::MacOs,
            )
            .unwrap();
            let error = publish(source.roots(), source.clone(), Duration::from_secs(1)).unwrap_err();
            assert!(
                matches!(error, ClipboardError::ContentUnavailable(reason) if reason == cause),
                "lost source failure: {error}"
            );
            assert_eq!(
                publication_reason(),
                FileProviderReason::DomainEnabled,
                "one source failure disabled all later file representations"
            );
            // Exercise the native adapter too: its old fallback erased the
            // typed failure again and reported that the extension needed enablement.
            let provider: Arc<dyn crate::content::Provider> = Arc::new(LinkedProvider(source.namespace().unwrap()));
            let spec = crate::NativeSpec {
                inventory: provider.inventory(),
                contents: provider,
                files: source.roots().to_vec(),
                file_contents: Some(source),
                sensitivity: crate::Sensitivity::Normal,
            };
            let error = crate::ClipboardBackend::write_native_guarded(
                &clipboard,
                spec,
                Arc::new(|| panic!("failed source reached clipboard publication")),
            )
            .unwrap_err();
            assert!(matches!(error, ClipboardError::ContentUnavailable(reason) if reason == cause));
        }
    }

    #[test]
    fn verified_launch_readiness_clears_a_pending_publication_error() {
        const CHILD: &str = "OINK_TEST_PROVIDER_READY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Exercise production globals in a private process without
            // initializing clipboard, registering a domain or touching IPC.
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "macos::file_provider::tests::verified_launch_readiness_clears_a_pending_publication_error",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        begin_launch_preparation();
        let pending: Result<(), _> = Err(ClipboardError::FileProvider(FileProviderReason::Checking));
        record_result(&pending);
        assert!(check_launch_preparation().is_err());
        finish_launch_preparation(true);
        assert!(check_launch_preparation().is_ok());
        assert_eq!(publication_reason(), FileProviderReason::DomainEnabled);
        assert!(!launch_preparation_pending());
        record_result(&pending);
        assert_eq!(publication_reason(), FileProviderReason::DomainEnabled);
        finish_launch_preparation(false);
        assert!(check_launch_preparation().is_err());
        record_result(&pending);
        assert_eq!(publication_reason(), FileProviderReason::RegistrationFailed);
    }

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
    fn empty_synthetic_receipts_do_not_survive_retirement_or_cancel() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("delivery.json");
        let (batch, _) = prepare_batch(
            Flat::new(&[], Arc::new(EmptyContents)).unwrap(),
            "delivery",
            "secret".into(),
            1,
            path.clone(),
        )
        .unwrap();
        let mut catalog = batch.state.lock().unwrap().catalog.clone();
        catalog.received_at = Some(1);
        catalog.records[0].hidden = None;
        catalog.records[0].name = "2026-10-23 01.00.00".into();
        save_catalog(&path, &catalog).unwrap();
        std::fs::write(
            path.with_extension("received"),
            serde_json::to_vec(&serde_json::json!({
                "delivery-root": {"record": catalog.records[0], "received_at": 1,
                    "handed_off": true, "materialized": true, "deleted": false}
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            !received_catalog_protected(&path),
            "statting a synthetic directory is not receipt"
        );
        remove_catalog_in(root.path(), &path);
        assert!(!path.exists(), "expired/failed empty parent survived");

        // Hover can hydrate even a zero-byte file. Explicit native cancellation
        // must retire the whole provisional publication, despite that receipt.
        let file = Record {
            exec_bits: None,
            symlink_target: None,
            id: "delivery-1".into(),
            parent: "delivery-root".into(),
            name: "zero".into(),
            directory: false,
            size: Some(0),
            modified: None,
            node: Some(1),
            version: 1,
            children: None,
            hidden: None,
            keep_local: None,
        };
        catalog.records.push(file.clone());
        save_catalog(&path, &catalog).unwrap();
        std::fs::write(path.with_extension("received"), serde_json::to_vec(&serde_json::json!({
            "delivery-1": {"record": file, "received_at": 1, "handed_off": true, "materialized": true, "deleted": false}
        })).unwrap()).unwrap();
        Registration {
            diagnostic_publication: (Instant::now(), false),
            key: "delivery".into(),
            bridge: Weak::new(),
            batch: batch.clone(),
            entries: vec![],
            _content_pin: None,
            admission: None,
        }
        .cancel();
        assert!(!path.exists(), "cancel retained a preview-hydrated publication");
        batch.state.lock().unwrap().persist(&path).unwrap();
        assert!(!path.exists(), "late callback resurrected cancelled publication");
    }

    #[test]
    fn private_preparation_overlay_cannot_pin_a_cancelled_typed_publication() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("typed.json");
        let (batch, _) = prepare_batch(
            Flat::new(&[], Arc::new(EmptyContents)).unwrap(),
            "typed",
            "secret".into(),
            1,
            path.clone(),
        )
        .unwrap();
        let mut state = batch.state.lock().unwrap();
        state.catalog.received_at = Some(1);
        save_catalog(&path, &state.catalog).unwrap();
        let mut private = state.catalog.records[0].clone();
        private.hidden = Some(true);
        drop(state);
        std::fs::write(path.with_extension("received"), serde_json::to_vec(&serde_json::json!({
            "typed-root": {"record": private, "received_at": 1, "handed_off": false, "materialized": true, "deleted": false}
        })).unwrap()).unwrap();
        assert!(!received_catalog_protected(&path));
        remove_catalog_in(root.path(), &path);
        assert!(!path.exists(), "an unused typed parent is not a received item");
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

    pub(super) fn linked_fixture() -> Arc<dyn Namespace> {
        use crate::content::Provider;
        crate::content::linked::Connector::default()
            .restore(crate::content::linked::Descriptor {
                native: Default::default(),
                provisional: false,
                fulfilled: vec![],
                offer: crate::content::tests::fixture().inventory().id,
                token: crate::content::grants::LinkToken([9; 32]),
                roots: vec![FileNode {
                    exec_bits: None,
                    id: NodeId(1),
                    parent: None,
                    name: "linked.txt".into(),
                    kind: NodeKind::File,
                    size: ContentSize::Known(6),
                    version: 7,
                    modified_ms: None,
                }],
            })
            .unwrap()
    }
    struct LinkedProvider(Arc<dyn Namespace>);
    impl crate::content::Provider for LinkedProvider {
        fn inventory(&self) -> crate::model::Inventory {
            let mut inventory = crate::content::tests::fixture().inventory();
            inventory.roots = self.0.roots();
            inventory
        }
        fn open(&self, _: crate::model::RepresentationId) -> io::Result<crate::FileReader> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn translator(&self) -> Arc<dyn crate::content::Translator> {
            crate::content::tests::fixture().translator()
        }
        fn namespace(&self) -> Option<Arc<dyn Namespace>> {
            Some(self.0.clone())
        }
    }

    #[test]
    fn filtered_received_namespaces_group_by_device_keep_ids_and_publish_real_dates() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let local = linked_fixture();
        let origin = local.origin().unwrap();
        let display = crate::received::source_folder(root.path(), origin, "Peer").unwrap();
        let provider = crate::policy::SharingPolicy::new(true).provider(Arc::new(LinkedProvider(local)));
        let namespace = provider.namespace().unwrap();
        let key = "a".repeat(64);
        let path = root.path().join(format!("{key}.json"));
        let (first, _) = prepare_batch(namespace.clone(), &key, "a".into(), 1, path.clone()).unwrap();
        let first_catalog = first.state.lock().unwrap().catalog.clone();
        let parent = &first_catalog.records[0];
        assert_eq!(parent.parent, format!("source{}", origin.to_hex()));
        assert_eq!(parent.id, format!("{key}-root"));
        assert_ne!(parent.name, key);
        assert_eq!(parent.modified, first_catalog.received_at.map(|at| at as i64));
        assert!(parent.modified.unwrap() > 1_700_000_000_000);
        assert_eq!(parent.hidden, None);
        let source_path = root.path().join(format!("{}.json", parent.parent));
        let source: Catalog = serde_json::from_slice(&std::fs::read(&source_path).unwrap()).unwrap();
        assert_eq!(source.records.len(), 1);
        assert_eq!(source.records[0].name, display);
        assert!(source.records[0].modified.unwrap() > 1_700_000_000_000);
        assert_eq!(source.records[0].parent, "NSFileProviderRootContainerItemIdentifier");
        crate::cache::assert_mutation(path.parent().unwrap(), || save_catalog(&path, &first_catalog)).unwrap();
        crate::cache::assert_no_mutation(path.parent().unwrap(), || save_catalog(&path, &first_catalog)).unwrap();
        // Device rename and another delivery reuse the one source folder.
        assert_eq!(
            crate::received::source_folder(root.path(), origin, "Renamed Peer").unwrap(),
            display
        );
        let (next, _) =
            prepare_batch(namespace, &"b".repeat(64), "b".into(), 2, root.path().join("next.json")).unwrap();
        assert_eq!(next.state.lock().unwrap().catalog.records[0].parent, parent.parent);
        let source_again: Catalog = serde_json::from_slice(&std::fs::read(source_path).unwrap()).unwrap();
        assert_eq!(source_again.records.len(), 1);
        assert_eq!(source_again.records[0].modified, source.records[0].modified);
        let restored = restore_catalog(data.path(), &path, &key, 3).unwrap();
        let restored = restored.state.lock().unwrap();
        assert_eq!(restored.catalog.records[0].id, parent.id);
        assert_eq!(restored.catalog.records[0].parent, parent.parent);
        assert_eq!(restored.catalog.records[0].name, parent.name);
        assert_eq!(restored.catalog.records[0].modified, parent.modified);
        assert_eq!(restored.catalog.records[1].id, first_catalog.records[1].id);
    }

    #[test]
    fn persistent_source_catalog_restores_unhydrated_paths_without_network_or_completed_cache() {
        use crate::content::Provider;
        use crate::content::linked::{Connector, Descriptor};
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let key = "d".repeat(64);
        let path = root.path().join(format!("{key}.json"));
        let descriptor = Descriptor {
            native: Default::default(),
            provisional: false,
            fulfilled: vec![],
            offer: crate::content::tests::fixture().inventory().id,
            token: crate::content::grants::LinkToken([9; 32]),
            roots: vec![FileNode {
                exec_bits: None,
                id: NodeId(1),
                parent: None,
                name: "folder".into(),
                kind: NodeKind::Directory,
                size: ContentSize::Unknown,
                version: 0,
                modified_ms: None,
            }],
        };
        let files = Connector::default().restore(descriptor).unwrap();
        let (batch, _) = prepare_batch(files, &key, "old-local-token".into(), 1, path.clone()).unwrap();
        {
            let mut state = batch.state.lock().unwrap();
            let child = FileNode {
                exec_bits: None,
                id: NodeId(2),
                parent: Some(NodeId(1)),
                name: "known.txt".into(),
                kind: NodeKind::File,
                size: ContentSize::Known(3),
                version: 1,
                modified_ms: None,
            };
            let record = node_record(&key, &child, &mut state.names).unwrap();
            state.catalog.records.push(record);
            state.persist(&path).unwrap();
        }
        let old: Vec<_> = batch
            .state
            .lock()
            .unwrap()
            .catalog
            .records
            .iter()
            .map(|r| (r.id.clone(), r.name.clone(), r.node))
            .collect();
        drop(batch);
        let restored = restore_catalog(data.path(), &path, &key, 2).unwrap();
        let current: Vec<_> = restored
            .state
            .lock()
            .unwrap()
            .catalog
            .records
            .iter()
            .map(|r| (r.id.clone(), r.name.clone(), r.node))
            .collect();
        assert_eq!(current, old);
        assert_eq!(restored.state.lock().unwrap().catalog.port, 2);
        let files = restored.contents.lock().unwrap();
        assert!(files.source_link().is_some());
        assert_eq!(files.select(NodeId(2)).unwrap_err().kind(), io::ErrorKind::NotConnected);
        drop(files);
        assert_eq!(std::fs::read_dir(data.path()).unwrap().count(), 0);
        remove_catalog_in(root.path(), &path);
        assert!(path.exists(), "cache clear cannot sever an unhydrated link");
        assert!(batch_needed(&restored, &[]));
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let token = restored.token.clone();
        let bridge = Bridge {
            port: address.port(),
            batches: Mutex::new(HashMap::from([(key.clone(), restored)])),
        };
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            assert_eq!(serve(stream, &bridge).unwrap_err().kind(), io::ErrorKind::NotConnected);
            assert_eq!(bridge.batches.lock().unwrap().len(), 1);
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        writeln!(
            client,
            "{}",
            serde_json::json!({"operation":"read","token":token,"batch":key,"node":2})
        )
        .unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).unwrap();
        worker.join().unwrap();
        assert_eq!(
            bytes,
            [255, 255, 255, 255, 1],
            "offline is unavailable, never confirmed gone"
        );
        let mut catalog: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        catalog.source.as_mut().unwrap().provisional = true;
        save_catalog(&path, &catalog).unwrap();
        assert!(restore_catalog(data.path(), &path, &key, 3).is_err());
        assert!(!path.exists(), "restart removes provisional native metadata");
    }

    #[test]
    fn source_date_repair_preserves_an_existing_folder_identity_and_name() {
        let root = tempfile::tempdir().unwrap();
        let origin = oink_foundation::DeviceId([8; 32]);
        let id = received_source_catalog(root.path(), origin, "Peer 080808080808".into()).unwrap();
        let path = root.path().join(format!("{id}.json"));
        let mut catalog: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        catalog.records[0].modified = None;
        save_catalog(&path, &catalog).unwrap();
        let created = std::fs::metadata(&path)
            .unwrap()
            .created()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert_eq!(
            received_source_catalog(root.path(), origin, "New name".into()).unwrap(),
            id
        );
        let repaired: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(repaired.records[0].modified, Some(created));
        assert_eq!(repaired.records[0].name, catalog.records[0].name);
        assert_eq!(repaired.records[0].parent, catalog.records[0].parent);
        assert_eq!(
            received_source_catalog(root.path(), origin, "Other name".into()).unwrap(),
            id
        );
        let repeated: Catalog = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(repeated.records[0].modified, Some(created));
    }

    #[test]
    fn empty_spare_parents_are_private_and_filling_preserves_their_identity() {
        let root = tempfile::tempdir().unwrap();
        let key = "d".repeat(64);
        let path = root.path().join(format!("{key}.json"));
        let (spare, _) = prepare_batch(
            Flat::new(&[], Arc::new(EmptyContents)).unwrap(),
            &key,
            "a".into(),
            1,
            path.clone(),
        )
        .unwrap();
        let spare = spare.state.lock().unwrap().catalog.clone();
        assert_eq!(spare.records[0].hidden, Some(true));
        assert_eq!(spare.records[0].name, format!(".Oink-prepared-{key}.noindex"));
        let entries = [FileEntry {
            path: "fixture".into(),
            is_dir: false,
            size: 0,
            modified_ms: None,
            local_path: None,
        }];
        let (filled, _) = prepare_batch(
            Flat::new(&entries, Arc::new(EmptyContents)).unwrap(),
            &key,
            "a".into(),
            1,
            path,
        )
        .unwrap();
        let filled = filled.state.lock().unwrap();
        assert_eq!(spare.records[0].id, filled.catalog.records[0].id);
        assert_eq!(filled.catalog.records[0].hidden, None);
    }

    #[test]
    fn a_new_delivery_does_not_restore_a_trashed_source_folder_or_move_its_old_batches() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let local = linked_fixture();
        let first_path = root.path().join("first.json");
        let (first, _) = prepare_batch(local.clone(), "first", "a".into(), 1, first_path.clone()).unwrap();
        let first_catalog = first.state.lock().unwrap().catalog.clone();
        save_catalog(&first_path, &first_catalog).unwrap();
        let old_parent = first_catalog.records[0].parent.clone();
        let source_path = root.path().join(format!("{old_parent}.json"));
        let mut source: Catalog = serde_json::from_slice(&std::fs::read(&source_path).unwrap()).unwrap();
        source.records[0].parent = "NSFileProviderTrashContainerItemIdentifier".into();
        std::fs::write(root.path().join(format!("{old_parent}.received")),serde_json::to_vec(&serde_json::json!({&old_parent:{"record":source.records[0],"received_at":1,"handed_off":false,"materialized":false,"deleted":false}})).unwrap()).unwrap();
        let (next, _) = prepare_batch(local, "next", "b".into(), 2, root.path().join("next.json")).unwrap();
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
    fn local_imported_empty_directory_is_received_data_not_a_synthetic_parent() {
        let local = serde_json::json!({
            "record": {"id":"batch-local-123", "parent":"batch-root", "directory":true,"node":null},
            "modified_locally":true,"handed_off":true,"materialized":true,"deleted":false
        });
        assert!(received_entry_protected(&local));
        let mut synthetic = local.clone();
        synthetic["modified_locally"] = false.into();
        assert!(!received_entry_protected(&synthetic));
        let mut deleted = local.clone();
        deleted["deleted"] = true.into();
        assert!(!received_entry_protected(&deleted));
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("batch.json");
        let catalog = Catalog {
            schema: 1,
            token: String::new(),
            port: 0,
            records: vec![Record {
                exec_bits: None,
                symlink_target: None,
                id: "batch-root".into(),
                parent: "root".into(),
                name: "received".into(),
                directory: true,
                size: None,
                modified: None,
                node: None,
                version: 0,
                children: None,
                hidden: None,
                keep_local: None,
            }],
            source: None,
            settlement: Default::default(),
            arrived: Vec::new(),
            failed: Vec::new(),
            urls: Vec::new(),
            received_at: Some(1),
            move_state: None,
            consumed: Vec::new(),
        };
        save_catalog(&path, &catalog).unwrap();
        std::fs::write(
            path.with_extension("received"),
            serde_json::to_vec(&serde_json::json!({"batch-local-123":local})).unwrap(),
        )
        .unwrap();
        remove_catalog_in(root.path(), &path);
        let restored: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            restored.records.len(),
            1,
            "overlay-only import must retain its catalog ancestor"
        );
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
    fn recovery_retires_a_pending_placeholder_before_restoring_linked_backing() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let local = linked_fixture();
        let key = "a".repeat(64);
        let path = root.path().join(format!("{key}.json"));
        let (batch, _) = prepare_batch(local, &key, "old".into(), 1, path.clone()).unwrap();
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
    }

    #[test]
    fn manager_failure_reasons_identify_enablement_actions() {
        for (code, reason, enable) in [
            (1, FileProviderReason::RegistrationFailed, false),
            (2, FileProviderReason::BoundExpired, false),
            (3, FileProviderReason::DomainDisabled, true),
            (4, FileProviderReason::ExtensionNotRegistered, true),
            (5, FileProviderReason::DomainDisconnected, false),
        ] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", &format!("exit {code}")]);
            let error = run_manager(command, Instant::now() + MANAGER_TIMEOUT, &|| true).unwrap_err();
            assert_eq!(failure_reason(&error), reason);
            assert_eq!(reason.needs_enable(), enable);
        }
        assert!(!FileProviderReason::Cancelled.needs_enable());
    }

    #[test]
    fn helper_failure_retains_native_codes_without_private_stderr() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", r#"printf '%s\n' 'crash /private/secret-name token=secret' 'OINK_FP_ERROR {"errors":[{"domain":"NSCocoaErrorDomain","code":4097},{"domain":"NSPOSIXErrorDomain","code":13}]}' >&2; exit 1"#]);
        let error = run_manager(command, Instant::now() + MANAGER_TIMEOUT, &|| true).unwrap_err();
        assert_eq!(failure_reason(&error), FileProviderReason::RegistrationFailed);
        let detail = error.to_string();
        assert!(detail.contains("NSCocoaErrorDomain code=4097 <- NSPOSIXErrorDomain code=13"));
        assert!(!detail.contains("private") && !detail.contains("secret"));
    }

    #[test]
    fn definitive_item_rejections_reach_drag_errors_without_waiting_for_the_bound() {
        for (code, reason) in [
            (32, FileProviderReason::ItemMissing),
            (33, FileProviderReason::ItemUnsupportedType),
            (34, FileProviderReason::ItemKindMismatch),
            (35, FileProviderReason::ItemTargetMismatch),
            (36, FileProviderReason::ItemIdentityMismatch),
            (37, FileProviderReason::ItemMetadataMismatch),
            (38, FileProviderReason::ItemAccessDenied),
            (39, FileProviderReason::ItemMetadataUnreadable),
        ] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", &format!("exit {code}")]);
            let started = Instant::now();
            let error = run_manager(command, started + Duration::from_secs(30), &|| true).unwrap_err();
            assert_eq!(failure_reason(&error), reason);
            assert!(
                error.to_string().contains(&reason.to_string()),
                "drag start reports the concrete typed reason"
            );
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "rejection must not wait for the publication deadline"
            );
            assert!(!reason.needs_enable());
            assert!(
                reason.item_rejected(),
                "a typed rejection must not retry ordinary publication"
            );
        }
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
    fn restart_refuses_a_catalog_without_a_persistent_source() {
        let data = tempfile::tempdir().unwrap();
        let catalogs = tempfile::tempdir().unwrap();
        let path = catalogs.path().join("batch.json");
        save_catalog(
            &path,
            &Catalog {
                schema: 1,
                token: "old".into(),
                port: 1,
                records: vec![],
                source: None,
                settlement: Default::default(),
                arrived: Vec::new(),
                failed: Vec::new(),
                urls: vec![],
                received_at: None,
                move_state: None,
                consumed: Vec::new(),
            },
        )
        .unwrap();
        assert!(restore_catalog(data.path(), &path, "batch", 2).is_err());
    }

    fn test_batch(contents: Arc<dyn FileContents>, entries: &[FileEntry], catalog: PathBuf) -> Arc<Batch> {
        prepare_batch(
            Flat::new(entries, contents).unwrap(),
            "batch",
            "secret".into(),
            0,
            catalog,
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
        let batch = test_batch(Arc::new(Busy(active.clone())), &[], PathBuf::new());
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
                exec_bits: None,
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
        let batch = test_batch(
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
            catalog_root.path().join("batch.json"),
        );
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
    fn read_batch(batch: Arc<Batch>, token: &str, node: u64) -> Vec<u8> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
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
        writeln!(
            client,
            "{}",
            serde_json::json!({"operation":"read","token":token,"batch":"batch","node":node})
        )
        .unwrap();
        let mut result = Vec::new();
        client.read_to_end(&mut result).unwrap();
        worker.join().unwrap();
        result
    }

    #[test]
    fn appkit_move_end_keeps_the_file_readable_for_finders_later_copy() {
        let root = tempfile::tempdir().unwrap();
        let opens = Arc::new(AtomicUsize::new(0));
        let batch = test_batch(
            Arc::new(Bytes {
                opens: opens.clone(),
                fail: false,
            }),
            &[FileEntry {
                path: "file.txt".into(),
                is_dir: false,
                size: 1,
                modified_ms: None,
                local_path: None,
            }],
            root.path().join("batch.json"),
        );
        let registration = Registration {
            diagnostic_publication: (Instant::now(), false),
            key: "batch".into(),
            bridge: Weak::new(),
            batch: batch.clone(),
            entries: Vec::new(),
            _content_pin: None,
            admission: None,
        };
        // Finder accepts MOVE before it asks File Provider for any content.
        // Exercise the same callback boundary, with no reader/consumption yet.
        registration.move_completed().unwrap();
        let bytes = read_batch(batch.clone(), "secret", 1);
        assert!(
            bytes.starts_with(&(u32::MAX - 1).to_be_bytes()),
            "Finder's post-drag fetch lost its source: {bytes:?}"
        );
        assert!(bytes.ends_with(&[0; 4]));
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert!(batch.state.lock().unwrap().catalog.move_state.is_none());
        assert!(!batch.state.lock().unwrap().retired);
    }

    #[test]
    fn rejected_provider_reads_send_their_typed_error_instead_of_eof() {
        for (case, token, node, code) in [(0, "secret", 1, 2), (1, "secret", 99, 2), (2, "wrong", 1, 6)] {
            let root = tempfile::tempdir().unwrap();
            let opens = Arc::new(AtomicUsize::new(0));
            let batch = test_batch(
                Arc::new(Bytes {
                    opens: opens.clone(),
                    fail: false,
                }),
                &[FileEntry {
                    path: "file.txt".into(),
                    is_dir: false,
                    size: 1,
                    modified_ms: None,
                    local_path: None,
                }],
                root.path().join("batch.json"),
            );
            if case == 0 {
                batch.state.lock().unwrap().retired = true;
            }
            let bytes = read_batch(batch, token, node);
            assert_eq!(bytes, [255, 255, 255, 255, code], "rejection {case} was not framed");
            assert_eq!(opens.load(Ordering::SeqCst), 0);
        }
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
    fn content_bridge_requires_capability_and_valid_node() {
        for (request, code) in [
            (
                "{\"operation\":\"read\",\"token\":\"wrong\",\"batch\":\"batch\",\"node\":1}\n",
                6,
            ),
            (
                "{\"operation\":\"read\",\"token\":\"secret\",\"batch\":\"batch\",\"node\":99}\n",
                2,
            ),
            (
                "{\"operation\":\"read\",\"token\":\"secret\",\"batch\":\"../../path\",\"node\":1}\n",
                6,
            ),
        ] {
            let (bytes, opens) = exchange(request, false);
            assert_eq!(bytes, [255, 255, 255, 255, code]);
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
        let root = tempfile::tempdir().unwrap();
        let batch = test_batch(
            Arc::new(Cancellable(cancels.clone())),
            &[],
            root.path().join("batch.json"),
        );
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
