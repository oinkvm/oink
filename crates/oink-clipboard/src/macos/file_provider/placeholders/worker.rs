use super::*;
use std::collections::{HashSet, VecDeque};

static POOL: Mutex<Pool> = Mutex::new(Pool::new());
static ENABLED: AtomicBool = AtomicBool::new(false);
static SETTING_EPOCH: AtomicU64 = AtomicU64::new(0);
static FOREGROUND: AtomicUsize = AtomicUsize::new(0);
static LAST_BLOCKER: Mutex<(u64, u64)> = Mutex::new((0, 1));

/// Per-machine opt-in and scheduler hint. Returns without waiting for the OS.
/// Disabling revokes pending preparations immediately; their persistent ledgers
/// remain until the system acknowledges namespace deletion. Active deliveries
/// already handed to readers keep their independent normal lifetimes.
pub fn configure_placeholder_pool(enabled: bool, background_allowed: bool) {
    let retired = {
        let mut pool = POOL.lock().unwrap();
        if pool.enabled != enabled {
            SETTING_EPOCH.fetch_add(1, Ordering::AcqRel);
            ENABLED.store(enabled, Ordering::Release);
        }
        pool.configure(enabled, background_allowed)
    };
    retire(retired);
    retry_retirements();
    start();
}
/// Snapshot of available stock and background work; does not consume a parent.
pub fn placeholder_pool_status() -> PlaceholderPoolStatus {
    let mut status = POOL.lock().unwrap().status(0);
    let jobs = RETIREMENTS.lock().unwrap();
    status.cleanup_pending = (jobs.queued.len() + jobs.failed.difference(&jobs.queued).count()) as u32;
    status
}
pub fn placeholder_status() -> PlaceholderTiming {
    let mut timing = PlaceholderTiming::default();
    POOL.lock().unwrap().observe(&mut timing);
    let jobs = RETIREMENTS.lock().unwrap();
    timing.cleanup_pending = (jobs.queued.len() + jobs.failed.difference(&jobs.queued).count()) as u32;
    timing
}

/// Covers Announced publication before Enter as well as activation. The agent's
/// native-drag flag alone cannot establish that the provider is idle.
pub(crate) struct Foreground;
impl Foreground {
    pub(crate) fn enter() -> Self {
        FOREGROUND.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for Foreground {
    fn drop(&mut self) {
        if FOREGROUND.fetch_sub(1, Ordering::AcqRel) == 1 {
            start();
        }
    }
}
pub(crate) fn activation_current(epoch: u64) -> bool {
    ENABLED.load(Ordering::Acquire) && SETTING_EPOCH.load(Ordering::Acquire) == epoch
}
pub(crate) fn take(roots: &[FileNode], timing: &mut PlaceholderTiming) -> Option<Taken> {
    let files = roots.iter().filter(|n| n.kind == NodeKind::File).count() as u32;
    let folders = roots.len() as u32 - files;
    let known = roots
        .iter()
        .all(|n| n.kind == NodeKind::Directory || matches!(n.size, ContentSize::Known(_)));
    let mut pool = POOL.lock().unwrap();
    pool.checkout(files, folders, known, timing).map(|ready| Taken {
        key: ready.key.clone(),
        ready: Some(ready),
        setting_epoch: SETTING_EPOCH.load(Ordering::Acquire),
        timing: timing.clone(),
    })
}
pub(super) fn release_taken(key: &str) {
    POOL.lock().unwrap().release_checkout(key);
    start();
}
pub(crate) fn protected() -> Vec<String> {
    POOL.lock().unwrap().protected().map(str::to_owned).collect()
}
fn can_prepare(generation: u64) -> bool {
    POOL.lock().unwrap().current(generation)
}
fn blocked(reason: u64, generation: u64) {
    let changed = {
        let mut last = LAST_BLOCKER.lock().unwrap();
        let changed = *last != (generation, reason);
        *last = (generation, reason);
        changed
    };
    if changed {
        record_refill(generation, 3, 0, reason, None);
    }
}
fn record_refill(generation: u64, event: u64, elapsed_us: u64, blocker: u64, failure: Option<FileProviderReason>) {
    let ready = POOL.lock().unwrap().ready.len();
    super::super::record_placeholder_refill(generation, event, elapsed_us, blocker, ready, failure);
}
pub(crate) fn start() {
    let blocker = if !ENABLED.load(Ordering::Acquire) {
        1
    } else if !DOMAIN_USABLE.load(Ordering::Acquire) || !POOL.lock().unwrap().background_allowed {
        2
    } else if FOREGROUND.load(Ordering::Acquire) != 0 {
        3
    } else if !matches!(BRIDGE.get(), Some(Ok(_))) {
        5
    } else {
        0
    };
    if blocker != 0 {
        blocked(blocker, 0);
        return;
    }
    let generation = {
        let mut pool = POOL.lock().unwrap();
        if pool.worker {
            return;
        }
        pool.start()
    };
    let Some(generation) = generation else {
        blocked(4, 0);
        return;
    };
    blocked(0, 0);
    // Independent hidden namespaces can progress while focus is remote or a
    // native drag lives. Only catalog critical sections yield to foreground work.
    std::thread::spawn(move || {
        while prepare(generation) {}
        let retry = POOL.lock().unwrap().failure.is_none();
        if retry {
            start();
        }
    });
}

fn priority_storage<'a>(
    storage: &'a Mutex<()>,
    foreground: &AtomicUsize,
    current: &dyn Fn() -> bool,
    waiting: &dyn Fn(u64),
) -> Result<std::sync::MutexGuard<'a, ()>, ClipboardError> {
    loop {
        if !current() {
            return Err(ClipboardError::FileProvider(FileProviderReason::Cancelled));
        }
        if foreground.load(Ordering::Acquire) == 0 {
            let guard = match storage.try_lock() {
                Ok(guard) => Some(guard),
                Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) => None,
            };
            if let Some(guard) = guard {
                if foreground.load(Ordering::Acquire) == 0 && current() {
                    return Ok(guard);
                }
                drop(guard);
            }
        }
        waiting(if foreground.load(Ordering::Acquire) != 0 { 3 } else { 6 });
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn background_storage(generation: u64, stock: u64) -> Result<std::sync::MutexGuard<'static, ()>, ClipboardError> {
    let guard = priority_storage(&STORAGE, &FOREGROUND, &|| can_prepare(generation), &|reason| {
        blocked(reason, stock)
    })?;
    blocked(0, stock);
    Ok(guard)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ledger {
    #[serde(deserialize_with = "crate::store::schema_one")]
    schema: u8,
    parent: Option<PathBuf>,
    urls: Vec<PathBuf>,
}
fn save_ledger(root: &std::path::Path, key: &str, ledger: &Ledger) -> io::Result<()> {
    let path = root.join(format!("{key}.placeholder"));
    let tmp = root.join(format!("{key}.placeholder-tmp"));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(ledger)?)?;
        file.sync_all()?;
        drop(file);
        crate::store::replace(&tmp, &path)?;
        crate::store::sync_directory(root)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}
fn prepare(generation: u64) -> bool {
    let started = Instant::now();
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let stock_generation = NEXT.fetch_add(1, Ordering::Relaxed);
    record_refill(stock_generation, 0, 0, 0, None);
    let mut created = None;
    let result = (|| {
        if !can_prepare(generation) {
            return Err(ClipboardError::FileProvider(FileProviderReason::Cancelled));
        }
        let root = CONTAINER.get().ok_or_else(|| error("container unavailable"))?;
        let port = BRIDGE
            .get()
            .and_then(|r| r.as_ref().ok())
            .ok_or_else(|| error("bridge unavailable"))?
            .port;
        let key = random().map_err(error)?;
        let path = root.join(format!("{key}.json"));
        let mut ledger = Ledger {
            schema: 1,
            parent: None,
            urls: Vec::new(),
        };
        let (batch, _) = prepare_batch(
            Flat::new(&[], Arc::new(EmptyContents)).map_err(error)?,
            &key,
            random().map_err(error)?,
            port,
            path.clone(),
        )
        .map_err(error)?;
        {
            let _storage = background_storage(generation, stock_generation)?;
            if !can_prepare(generation) {
                return Err(ClipboardError::FileProvider(FileProviderReason::Cancelled));
            }
            if !POOL.lock().unwrap().preparing(generation, key.clone()) {
                return Err(ClipboardError::FileProvider(FileProviderReason::Cancelled));
            }
            created = Some(key.clone());
            // Persist ownership before exposing any item to the working set.
            save_ledger(root, &key, &ledger).map_err(error)?;
            let mut state = batch.state.lock().unwrap();
            hide_parent(&key, &mut state.catalog);
            save_catalog(&path, &state.catalog).map_err(error)?;
        }
        let json = manager_while(
            &["prepare-placeholder-parent", &key],
            Instant::now() + MANAGER_TIMEOUT,
            &|| can_prepare(generation),
        )?;
        ledger.parent = Some(serde_json::from_str(&json).map_err(error)?);
        {
            let _storage = background_storage(generation, stock_generation)?;
            if !can_prepare(generation) {
                return Err(ClipboardError::FileProvider(FileProviderReason::Cancelled));
            }
            save_ledger(root, &key, &ledger).map_err(error)?;
            let mut state = batch.state.lock().unwrap();
            fill_catalog(&key, &mut state.catalog);
            save_catalog(&path, &state.catalog).map_err(error)?;
        }
        let json = manager_while(
            &["prepare-placeholders", &key],
            Instant::now() + MANAGER_TIMEOUT,
            &|| can_prepare(generation),
        )?;
        ledger.urls = serde_json::from_str(&json).map_err(error)?;
        if ledger.urls.len() != (FILES + FOLDERS) as usize {
            return Err(error("incomplete prepared namespace"));
        }
        {
            let _storage = background_storage(generation, stock_generation)?;
            if !can_prepare(generation) {
                return Err(ClipboardError::FileProvider(FileProviderReason::Cancelled));
            }
            save_ledger(root, &key, &ledger).map_err(error)?;
        }
        Ok(Ready {
            generation: stock_generation,
            key,
            preparation_us: started.elapsed().as_micros() as u64,
        })
    })();
    let mut pool = POOL.lock().unwrap();
    let accepted = pool.current(generation);
    let result = if accepted {
        result
    } else {
        Err(ClipboardError::FileProvider(FileProviderReason::Cancelled))
    };
    let failed = result.is_err();
    let failure = result.as_ref().err().map(failure_reason);
    let again = pool.finish(generation, result.map_err(|e| failure_reason(&e)));
    drop(pool);
    record_refill(
        stock_generation,
        if failed { 2 } else { 1 },
        started.elapsed().as_micros() as u64,
        0,
        failure,
    );
    if failed {
        retire(created.into_iter().collect());
    }
    again
}

#[derive(Default)]
struct Retirements {
    worker: bool,
    pending: VecDeque<String>,
    queued: HashSet<String>,
    failed: HashSet<String>,
}
static RETIREMENTS: std::sync::LazyLock<Mutex<Retirements>> = std::sync::LazyLock::new(Mutex::default);
pub(crate) fn retire(keys: Vec<String>) {
    if keys.is_empty() {
        return;
    }
    let mut jobs = RETIREMENTS.lock().unwrap();
    for key in keys {
        if jobs.queued.insert(key.clone()) {
            jobs.pending.push_back(key);
        }
    }
    if jobs.worker {
        return;
    }
    jobs.worker = true;
    drop(jobs);
    std::thread::spawn(|| {
        loop {
            let key = {
                let mut jobs = RETIREMENTS.lock().unwrap();
                let Some(key) = jobs.pending.pop_front() else {
                    jobs.worker = false;
                    return;
                };
                key
            };
            let result = retire_one(&key);
            let mut jobs = RETIREMENTS.lock().unwrap();
            jobs.queued.remove(&key);
            if result.is_err() {
                jobs.failed.insert(key);
            } else {
                jobs.failed.remove(&key);
            }
            super::super::record_placeholder_cleanup(jobs.failed.len(), result.as_ref().err().map(failure_reason));
        }
    });
}
fn retire_one(key: &str) -> Result<(), ClipboardError> {
    let root = CONTAINER.get().ok_or_else(|| error("container unavailable"))?;
    let deadline = Instant::now() + MANAGER_TIMEOUT;
    // Disabling invalidates the writer immediately, but removal must follow its
    // helper being reaped; otherwise a late preparer could recreate the catalog.
    while POOL.lock().unwrap().preparing.as_deref() == Some(key) {
        check_publication(deadline, &|| true)?;
        std::thread::sleep(Duration::from_millis(5));
    }
    let ledger = root.join(format!("{key}.placeholder"));
    {
        let _storage = STORAGE.lock().unwrap_or_else(|e| e.into_inner());
        remove_catalog_in(root, &root.join(format!("{key}.json")));
        if !ledger.exists() {
            save_ledger(
                root,
                key,
                &Ledger {
                    schema: 1,
                    parent: None,
                    urls: Vec::new(),
                },
            )
            .map_err(error)?;
        }
    }
    manager(&["retire-placeholders", key], Instant::now() + MANAGER_TIMEOUT)?;
    let _storage = STORAGE.lock().unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::remove_file(root.join(format!("{key}.placeholder-tmp")));
    std::fs::remove_file(ledger).map_err(error)?;
    crate::store::sync_directory(root).map_err(error)
}
pub(crate) fn retry_retirements() {
    let keys = RETIREMENTS.lock().unwrap().failed.iter().cloned().collect();
    retire(keys);
}
pub(crate) fn recover() {
    if let Some(root) = CONTAINER.get() {
        retire(ledger_keys(root));
    }
}
pub(crate) fn ledger_keys(root: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let key = path.file_stem()?.to_str()?;
            (path.extension()? == "placeholder" && key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| key.to_owned())
        })
        .collect()
}
pub(crate) fn complete(root: &std::path::Path, key: &str) -> io::Result<()> {
    // Durably remove the pending ledger before exposing a final URL to native
    // consumers. Recovery must never retire an already handed-off delivery.
    std::fs::remove_file(root.join(format!("{key}.placeholder")))?;
    crate::store::sync_directory(root)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn background_waits_for_foreground_without_cancelling_its_parent() {
        let storage = Arc::new(Mutex::new(()));
        let foreground = Arc::new(AtomicUsize::new(1));
        let current = Arc::new(AtomicBool::new(true));
        let (waiting, wait) = std::sync::mpsc::channel();
        let (done, completed) = std::sync::mpsc::channel();
        let worker = {
            let storage = storage.clone();
            let foreground = foreground.clone();
            let current = current.clone();
            std::thread::spawn(move || {
                let _guard = priority_storage(&storage, &foreground, &|| current.load(Ordering::Acquire), &|reason| {
                    let _ = waiting.send(reason);
                })
                .unwrap();
                done.send(()).unwrap();
            })
        };
        assert_eq!(wait.recv_timeout(Duration::from_secs(2)).unwrap(), 3);
        assert!(
            storage.try_lock().is_ok(),
            "background must not hold STORAGE while yielding"
        );
        assert!(completed.try_recv().is_err());
        foreground.store(0, Ordering::Release);
        completed.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert!(current.load(Ordering::Acquire));
    }
    #[test]
    fn disabling_background_permission_interrupts_priority_wait() {
        let storage = Mutex::new(());
        let foreground = AtomicUsize::new(1);
        let current = AtomicBool::new(true);
        let result = priority_storage(&storage, &foreground, &|| current.load(Ordering::Acquire), &|reason| {
            assert_eq!(reason, 3);
            current.store(false, Ordering::Release);
        });
        assert!(matches!(
            result,
            Err(ClipboardError::FileProvider(FileProviderReason::Cancelled))
        ));
        assert!(storage.try_lock().is_ok());
    }
}
