//! Bounded, privacy-first diagnostic flight recorder. All disk/compression work is off input threads.
mod buttons;
pub(crate) mod capture;
mod clock;
pub(crate) mod collection;
pub(crate) mod contacts;
mod crash_report;
pub(crate) mod cursor_clip;
pub(crate) mod cursor_reconciliation;
pub(crate) mod dnd;
mod gesture;
pub(crate) mod handoff;
pub(crate) mod input_trace;
pub(crate) mod keyboard;
pub(crate) mod layout;
#[cfg(all(test, debug_assertions))]
mod layout_tests;
mod privacy;
mod protocol_violations;
#[cfg(all(test, debug_assertions))]
mod size_only_tests;
mod store;
#[cfg(all(test, debug_assertions))]
mod tests;
mod trace;
mod volume;
mod wire_values;
pub use privacy::{Anonymizer, Event, Origin, SafeInput, StateKind};
pub use store::{BundleInfo, ClockSample, DiscardCounts, Reason, RecorderStatus, Snapshot};
pub(crate) use trace::{DiagnosticsLayer, bundle_only_target};

use crossbeam_queue::ArrayQueue;
use oink_foundation::{DeviceId, GroupId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDragPath {
    Provider,
    DegradedPromise,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAvailability {
    pub available: bool,
    pub reason: oink_clipboard::FileProviderReason,
}
impl From<oink_clipboard::FileProviderReason> for ProviderAvailability {
    fn from(reason: oink_clipboard::FileProviderReason) -> Self {
        use oink_clipboard::FileProviderReason as Reason;
        Self {
            available: matches!(reason, Reason::DomainEnabled | Reason::UrlsReady),
            reason,
        }
    }
}

impl ProviderAvailability {
    // Stable numeric wire code lives in the pre-existing snapshot scalar map.
    fn code(self) -> f64 {
        use oink_clipboard::FileProviderReason as R;
        match self.reason {
            R::Checking => 0.,
            R::DomainEnabled => 1.,
            R::UrlsReady => 2.,
            R::DomainDisabled => 3.,
            R::ExtensionNotRegistered => 4.,
            R::DomainDisconnected => 5.,
            R::RegistrationFailed => 6.,
            R::BoundExpired => 7.,
            R::Cancelled => 8.,
        }
    }
    fn from_code(code: f64) -> Option<Self> {
        use oink_clipboard::FileProviderReason as R;
        let reason = match code {
            0. => R::Checking,
            1. => R::DomainEnabled,
            2. => R::UrlsReady,
            3. => R::DomainDisabled,
            4. => R::ExtensionNotRegistered,
            5. => R::DomainDisconnected,
            6. => R::RegistrationFailed,
            7. => R::BoundExpired,
            8. => R::Cancelled,
            _ => return None,
        };
        Some(reason.into())
    }
}

pub const SCHEMA: u32 = 1;
// Automatic packages retain their existing save range; recording has no age limit.
pub const AUTOMATIC_RANGE_US: i64 = 30 * 60 * 1_000_000;
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_SEGMENT_RAW: usize = 512 * 1024;
const QUEUE: usize = 8192;
thread_local! { static CAPTURE_STAMP: std::cell::Cell<Option<clock::Stamp>> = const { std::cell::Cell::new(None) }; }
pub(crate) fn utc_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as i64)
}
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Row {
    pub mono_us: u64,
    pub clock_id: i64,
    pub utc_us: i64,
    pub event: Event,
}
struct Inner {
    me: DeviceId,
    group: Mutex<Option<GroupId>>,
    presses: ArrayQueue<keyboard::Presses>,
    gestures: ArrayQueue<oink_input::gesture_trace::Tracker>,
    press_sequence: AtomicU64,
    press_generation: AtomicU64,
    queue: arc_swap::ArcSwap<ArrayQueue<Row>>,
    enabled: AtomicBool,
    configured_enabled: AtomicBool,
    suspended: AtomicBool,
    stopped: AtomicBool,
    dropped: AtomicU64,
    errors: AtomicU64,
    clock: Arc<clock::Clock>,
    instance: String,
    previous_instance: Option<String>,
    store: Mutex<store::Store>,
    worker_thread: std::sync::OnceLock<std::thread::Thread>,
    worker_notified: AtomicBool,
    collection: std::sync::OnceLock<collection::Context>,
    recovery_wake: tokio::sync::Notify,
    recovering: AtomicBool,
    status_changed: tokio::sync::watch::Sender<(u64, u64)>,
    recovery_failed: Mutex<Option<String>>,
    recovery_completion: (Mutex<()>, Condvar),
    recovery_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    save_generation: AtomicU64,
    save: Mutex<SaveState>,
    #[cfg(all(test, debug_assertions))]
    export_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(all(test, debug_assertions))]
    automatic_wait_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    anon: arc_swap::ArcSwap<Anonymizer>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(thread) = self.worker_thread.get() {
            thread.unpark();
        }
    }
}
#[derive(Default)]
struct SaveState {
    // One completed result per cause (four entries), so an intervening emergency
    // cannot make callers waiting for a manual save export the same cutoff again.
    completed: Vec<CompletedSave>,
}
struct CompletedSave {
    reason: Reason,
    last_minutes: Option<u32>,
    generation: u64,
    result: Result<BundleInfo, String>,
}

#[cfg(all(test, debug_assertions))]
type RecoveryHook = Arc<dyn Fn(&AtomicBool) + Send + Sync>;

pub(crate) struct StartupRecoveryGuard(pub(crate) Option<Recorder>);
impl Drop for StartupRecoveryGuard {
    fn drop(&mut self) {
        // Cancellation / startup errors are orderly; an unwinding backend panic
        // must retain the running instance just like a panic after host startup.
        if !std::thread::panicking()
            && let Some(recorder) = self.0.take()
        {
            recorder.shutdown();
        }
    }
}

#[derive(Clone)]
pub struct Recorder {
    inner: Arc<Inner>,
}
impl Recorder {
    pub fn start(dir: PathBuf, me: DeviceId, group: Option<GroupId>, enabled: bool) -> anyhow::Result<Self> {
        Self::start_with_clock(dir, me, group, enabled, clock::Clock::system())
    }
    pub(crate) fn start_suspended(
        dir: PathBuf,
        me: DeviceId,
        group: Option<GroupId>,
        enabled: bool,
        suspended: bool,
        collection: Option<collection::Context>,
    ) -> anyhow::Result<Self> {
        Self::start_inner(
            dir,
            me,
            group,
            enabled,
            suspended,
            clock::Clock::system(),
            collection,
            #[cfg(all(test, debug_assertions))]
            None,
            #[cfg(all(test, debug_assertions))]
            true,
        )
    }
    fn start_with_clock(
        dir: PathBuf,
        me: DeviceId,
        group: Option<GroupId>,
        enabled: bool,
        clock: Arc<clock::Clock>,
    ) -> anyhow::Result<Self> {
        Self::start_inner(
            dir,
            me,
            group,
            enabled,
            false,
            clock,
            None,
            #[cfg(all(test, debug_assertions))]
            None,
            #[cfg(all(test, debug_assertions))]
            true,
        )
    }
    #[cfg_attr(test, allow(clippy::too_many_arguments))]
    fn start_inner(
        dir: PathBuf,
        me: DeviceId,
        group: Option<GroupId>,
        enabled: bool,
        suspended: bool,
        clock: Arc<clock::Clock>,
        collection: Option<collection::Context>,
        #[cfg(all(test, debug_assertions))] recovery_hook: Option<RecoveryHook>,
        #[cfg(all(test, debug_assertions))] register_process_trace: bool,
    ) -> anyhow::Result<Self> {
        let anon = Anonymizer::for_membership(me, group);
        let mut store = store::Store::open_with_clock(dir, anon.device(me), enabled, clock.clone())?;
        let recovery_source = store.clone();
        // Every live segment needs its producer's identity, even if this peer
        // never invokes the local export API. Keep the recovery source unchanged
        // so a crash window still reports the build stored in its old segments.
        store.snapshot = collection
            .as_ref()
            .and_then(|context| context.environment(&anon))
            .unwrap_or_else(Snapshot::current_build);
        let this = Self {
            inner: Arc::new(Inner {
                me,
                group: Mutex::new(group),
                presses: {
                    let q = ArrayQueue::new(1);
                    let _ = q.push(keyboard::Presses::default());
                    q
                },
                gestures: {
                    let q = ArrayQueue::new(1);
                    let _ = q.push(oink_input::gesture_trace::Tracker::default());
                    q
                },
                press_sequence: AtomicU64::new(0),
                press_generation: AtomicU64::new(0),
                queue: arc_swap::ArcSwap::from_pointee(ArrayQueue::new(QUEUE)),
                enabled: AtomicBool::new(enabled),
                configured_enabled: AtomicBool::new(enabled),
                suspended: AtomicBool::new(suspended),
                stopped: AtomicBool::new(false),
                dropped: AtomicU64::new(0),
                errors: AtomicU64::new(0),
                clock,
                instance: store.instance.clone(),
                previous_instance: store.previous_instance.clone(),
                store: Mutex::new(store),
                worker_thread: Default::default(),
                worker_notified: AtomicBool::new(false),
                collection: collection.map(std::sync::OnceLock::from).unwrap_or_default(),
                recovery_wake: tokio::sync::Notify::new(),
                recovering: AtomicBool::new(true),
                status_changed: tokio::sync::watch::channel((0, 0)).0,
                recovery_failed: Mutex::new(None),
                recovery_completion: (Mutex::new(()), Condvar::new()),
                recovery_thread: Mutex::new(None),
                save_generation: AtomicU64::new(0),
                save: Mutex::new(SaveState::default()),
                #[cfg(all(test, debug_assertions))]
                export_hook: Mutex::new(None),
                #[cfg(all(test, debug_assertions))]
                automatic_wait_hook: Mutex::new(None),
                anon: arc_swap::ArcSwap::from_pointee(anon),
            }),
        };
        let weak = Arc::downgrade(&this.inner);
        let worker = std::thread::Builder::new()
            .name("oink-diagnostics".into())
            .spawn(move || worker(weak))?;
        let _ = this.inner.worker_thread.set(worker.thread().clone());
        let recovery = this.clone();
        let handle = std::thread::Builder::new()
            .name("oink-diagnostics-recovery".into())
            .spawn(move || {
                #[cfg(all(test, debug_assertions))]
                if let Some(hook) = recovery_hook {
                    hook(&recovery_source.cancelled);
                }
                recovery.recover(recovery_source)
            })?;
        *lock(&this.inner.recovery_thread) = Some(handle);
        #[cfg(not(all(test, debug_assertions)))]
        trace::register(&this);
        #[cfg(all(test, debug_assertions))]
        if register_process_trace {
            trace::register(&this);
        }
        this.keyboard_snapshot();
        Ok(this)
    }
    fn recover(&self, source: store::Store) {
        let result = (|| -> anyhow::Result<()> {
            let snapshot = source.recover(
                |segment, meta, path| {
                    source.check_cancelled()?;
                    let mut live = lock(&self.inner.store);
                    source.check_cancelled()?;
                    live.adopt_recovered(segment, meta, path)
                },
                |class| lock(&self.inner.store).record_discard(class),
            )?;
            if self.inner.enabled.load(Ordering::Acquire) && snapshot.previous_instance.is_some() {
                if let Some(context) = self.inner.collection.get() {
                    context
                        .runtime
                        .block_on(context.wait_recovery_ready(&snapshot, &self.inner.recovery_wake))?;
                }
                self.save_once(Reason::RecoveredCrash, || {
                    self.export_snapshot(snapshot, Reason::RecoveredCrash, 0, 0)
                })?;
            }
            // Once the crash archive is committed, a stop during staging cleanup
            // must not report the already-saved crash again on the next launch.
            lock(&self.inner.store).previous_instance = None;
            source.finish_recovery()?;
            Ok(())
        })();
        if let Err(error) = result
            && !source.cancelled.load(Ordering::Acquire)
        {
            self.inner.errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("diagnostic recovery failed");
            *lock(&self.inner.recovery_failed) = Some(format!("{error:#}"));
        }
        let _completion = lock(&self.inner.recovery_completion.0);
        self.inner.recovering.store(false, Ordering::Release);
        self.notify_status();
        self.inner.recovery_completion.1.notify_all();
    }
    pub(crate) fn ensure_recovered(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.inner.recovering.load(Ordering::Acquire),
            "diagnostics recovering; retry saving after recovery completes (diagnostics status)"
        );
        if let Some(error) = &*lock(&self.inner.recovery_failed) {
            anyhow::bail!("diagnostics recovery failed: {error}");
        }
        Ok(())
    }
    fn stop_recovery(&self) {
        lock(&self.inner.store).cancelled.store(true, Ordering::Release);
        self.inner.recovery_wake.notify_one();
        if let Some(handle) = lock(&self.inner.recovery_thread).take() {
            let _ = handle.join();
        }
    }
    #[cfg(all(test, debug_assertions))]
    pub(crate) fn wait_recovery(&self) {
        if let Some(handle) = lock(&self.inner.recovery_thread).take() {
            handle.join().unwrap();
        }
        self.ensure_recovered().unwrap();
    }
    // Automatic saves run on separate blocking tasks and cannot ask the caller
    // to retry after startup. Wait before taking the single-flight save lock:
    // recovery itself uses it for the crash archive.
    fn wait_automatic_recovery(&self) -> anyhow::Result<()> {
        let cancelled = lock(&self.inner.store).cancelled.clone();
        let mut completion = lock(&self.inner.recovery_completion.0);
        while self.inner.recovering.load(Ordering::Acquire) {
            #[cfg(all(test, debug_assertions))]
            if let Some(hook) = lock(&self.inner.automatic_wait_hook).clone() {
                hook();
            }
            completion = self
                .inner
                .recovery_completion
                .1
                .wait(completion)
                .unwrap_or_else(|e| e.into_inner());
        }
        anyhow::ensure!(
            !cancelled.load(Ordering::Acquire),
            "diagnostic automatic save cancelled by shutdown, disable or rekey"
        );
        self.ensure_recovered()
    }
    /// No I/O, waiting, formatting, mutex or compression on the input path.
    pub fn record(&self, event: Event) {
        if matches!(
            event,
            Event::Input {
                event: SafeInput::ContactsOmitted,
                ..
            }
        ) {
            return;
        }
        let queue = self.inner.queue.load();
        // Layout lifecycle changes remain observable while sharing/input sampling
        // is paused. Explicit diagnostics off and shutdown still forbid records.
        let layout_enabled = matches!(&event, Event::Topology(_))
            && self.inner.enabled.load(Ordering::Acquire)
            && !self.inner.stopped.load(Ordering::Acquire);
        if !self.enabled() && !layout_enabled {
            return;
        }
        let stamp = CAPTURE_STAMP.get().unwrap_or_else(|| self.inner.clock.now());
        if queue
            .push(Row {
                mono_us: stamp.mono_us,
                clock_id: stamp.clock_id,
                utc_us: stamp.utc_us,
                event,
            })
            .is_err()
        {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
        }
        if !self.inner.worker_notified.swap(true, Ordering::AcqRel)
            && let Some(thread) = self.inner.worker_thread.get()
        {
            thread.unpark();
        }
    }
    pub(super) fn with_capture_stamp(&self, stamp: clock::Stamp, action: impl FnOnce()) {
        struct Reset(Option<clock::Stamp>);
        impl Drop for Reset {
            fn drop(&mut self) {
                CAPTURE_STAMP.set(self.0);
            }
        }
        let _reset = Reset(CAPTURE_STAMP.replace(Some(stamp)));
        action();
    }
    pub fn enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Relaxed)
            && !self.inner.suspended.load(Ordering::Acquire)
            && !self.inner.stopped.load(Ordering::Relaxed)
    }
    pub fn anonymizer(&self) -> arc_swap::Guard<Arc<Anonymizer>> {
        self.inner.anon.load()
    }
    pub(crate) fn rekey(&self, me: DeviceId, group: Option<GroupId>) -> anyhow::Result<()> {
        self.inner.press_generation.fetch_add(1, Ordering::Relaxed);
        oink_input::native_lifecycle::reset_gestures();
        let was_enabled = self.inner.enabled.swap(false, Ordering::SeqCst);
        self.stop_recovery();
        let anon = Anonymizer::for_membership(me, group);
        let environment = self
            .inner
            .collection
            .get()
            .and_then(|context| context.environment(&anon))
            .unwrap_or_else(Snapshot::current_build);
        let mut s = lock(&self.inner.store);
        s.cancelled = Arc::new(AtomicBool::new(false));
        *lock(&self.inner.recovery_failed) = None;
        s.previous_instance = None;
        self.inner.queue.store(Arc::new(ArrayQueue::new(QUEUE)));
        s.set_enabled(false)?;
        s.device = anon.device(me);
        s.snapshot = environment;
        s.write_domain()?;
        self.inner.anon.store(Arc::new(anon));
        s.set_enabled(was_enabled)?;
        *lock(&self.inner.group) = group;
        self.inner.enabled.store(was_enabled, Ordering::SeqCst);
        self.keyboard_snapshot();
        Ok(())
    }
    /// Pause recording without deleting the pre-pause flight recorder window.
    pub fn set_suspended(&self, suspended: bool) {
        if self.inner.suspended.swap(suspended, Ordering::AcqRel) == suspended {
            return;
        }
        self.inner.press_generation.fetch_add(1, Ordering::Relaxed);
        oink_input::native_lifecycle::reset_gestures();
        self.notify_status();
        if !suspended {
            self.keyboard_snapshot();
            return;
        }
        let mut store = lock(&self.inner.store); // settle a drain already in progress
        // Detach the pending queue, persisting captured rows without generating a
        // sampling tick. The bounded flush does not take the export/save lock.
        // Recovery continues: pause preserves the incident, disable erases it.
        let pending = self.inner.queue.swap(Arc::new(ArrayQueue::new(QUEUE)));
        let rows = (0..QUEUE).filter_map(|_| pending.pop()).collect::<Vec<_>>();
        if !rows.is_empty()
            && let Err(error) = store.append(rows, self.inner.dropped.load(Ordering::Relaxed))
        {
            self.inner.errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(%error, "diagnostic suspension flush failed");
        }
    }
    pub fn set_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        let was_configured = self.inner.configured_enabled.swap(enabled, Ordering::AcqRel);
        let was_enabled = self.inner.enabled.load(Ordering::Acquire);
        if enabled && was_enabled {
            return Ok(());
        }
        self.inner.press_generation.fetch_add(1, Ordering::Relaxed);
        self.inner.enabled.store(false, Ordering::SeqCst);
        self.stop_recovery();
        let mut s = lock(&self.inner.store);
        s.cancelled = Arc::new(AtomicBool::new(false));
        *lock(&self.inner.recovery_failed) = None;
        s.previous_instance = None;
        self.inner.queue.store(Arc::new(ArrayQueue::new(QUEUE)));
        let applied = s.set_enabled(enabled);
        if applied.is_ok() {
            self.inner.enabled.store(enabled, Ordering::SeqCst);
        }
        drop(s);
        if applied.is_err() || was_configured != enabled || was_enabled != enabled {
            self.notify_status();
        }
        if applied.is_ok() {
            self.keyboard_snapshot();
        }
        applied
    }
    /// Current availability survives event retention and is exported with every package.
    #[cfg(any(target_os = "macos", all(test, debug_assertions)))]
    pub(crate) fn file_provider_changed(&self, reason: oink_clipboard::FileProviderReason) {
        {
            let mut store = lock(&self.inner.store);
            store.snapshot.file_provider = Some(reason.into());
            store
                .snapshot
                .routing_numbers
                .insert("file_provider_reason".into(), ProviderAvailability::from(reason).code());
        }
        // The availability owner deduplicates changes. Snapshot sampling may
        // already have observed this reason; it must not suppress its event.
        self.record(Event::FileProviderAvailability(reason.into()));
    }
    pub(crate) fn refresh_environment(&self) {
        let anon = self.anonymizer();
        if self.inner.enabled.load(Ordering::Acquire)
            && let Some(context) = self.inner.collection.get()
            && let Some(snapshot) = context.environment(&anon)
        {
            let mut store = lock(&self.inner.store);
            // A concurrent group change already installed its own complete
            // snapshot. Never put the old domain's identifiers into that store.
            if self.inner.enabled.load(Ordering::Acquire)
                && !self.inner.stopped.load(Ordering::Acquire)
                && Arc::ptr_eq(&anon, &self.anonymizer())
            {
                Self::replace_snapshot(&mut store, snapshot);
            }
        }
    }
    pub fn snapshot(&self, snapshot: Snapshot) {
        if self.enabled() {
            Self::replace_snapshot(&mut lock(&self.inner.store), snapshot);
        }
    }
    fn replace_snapshot(store: &mut store::Store, mut snapshot: Snapshot) {
        if snapshot.file_provider.is_none() {
            snapshot.file_provider = store.snapshot.file_provider;
        }
        if snapshot.group_state.is_none() {
            snapshot.group_state = store.snapshot.group_state.take();
        }
        if snapshot.layout.is_none() {
            snapshot.layout = store.snapshot.layout.take();
        }
        if let Some(provider) = snapshot.file_provider {
            snapshot
                .routing_numbers
                .insert("file_provider_reason".into(), provider.code());
        }
        store.snapshot = snapshot;
    }
    /// Called on the ordinary diagnostics task; configuration owns redaction.
    pub fn configuration(&self, config: &crate::config_module::ConfigModule) {
        if self.inner.enabled.load(Ordering::Acquire) {
            let anon = self.anonymizer();
            let excerpt = config.diagnostic_excerpt().to_string();
            let layout = config.diagnostic_layout(&anon);
            let mut store = lock(&self.inner.store);
            if Arc::ptr_eq(&anon, &self.anonymizer()) {
                store.snapshot.group_state = Some(excerpt);
                store.snapshot.layout = layout;
            }
        }
    }
    /// Serialize all save causes. Concurrent requests of the same cause join the
    /// completed result (including errors); distinct causes run in order. This
    /// mutex never participates in recording, collection or Store access.
    pub(crate) fn save_once(
        &self,
        reason: Reason,
        save: impl FnOnce() -> anyhow::Result<BundleInfo>,
    ) -> anyhow::Result<BundleInfo> {
        let generation = self.inner.save_generation.load(Ordering::Acquire);
        self.finish_save(
            generation,
            reason,
            if reason == Reason::Manual { Some(30) } else { None },
            save,
        )
    }
    fn finish_save(
        &self,
        generation: u64,
        reason: Reason,
        last_minutes: Option<u32>,
        save: impl FnOnce() -> anyhow::Result<BundleInfo>,
    ) -> anyhow::Result<BundleInfo> {
        let mut state = lock(&self.inner.save);
        if let Some(saved) = state
            .completed
            .iter()
            .find(|s| s.reason == reason && s.last_minutes == last_minutes && s.generation > generation)
        {
            return saved.result.clone().map_err(anyhow::Error::msg);
        }
        let result = save();
        let completed = CompletedSave {
            reason,
            last_minutes,
            generation: self.inner.save_generation.load(Ordering::Relaxed).wrapping_add(1),
            result: result.as_ref().cloned().map_err(|e| format!("{e:#}")),
        };
        if let Some(old) = state.completed.iter_mut().find(|s| s.reason == reason) {
            *old = completed;
        } else {
            state.completed.push(completed);
        }
        self.inner.save_generation.fetch_add(1, Ordering::Release);
        result
    }
    pub(crate) fn export_cutoff(&self, reason: Reason, complete: Option<bool>) -> anyhow::Result<BundleInfo> {
        self.export_cutoff_range(reason, complete, Some(30))
    }
    fn export_cutoff_range(
        &self,
        reason: Reason,
        complete: Option<bool>,
        last_minutes: Option<u32>,
    ) -> anyhow::Result<BundleInfo> {
        self.ensure_recovered()?;
        if let Some(context) = self.inner.collection.get() {
            context.sample(self);
        }
        let mut snapshot = {
            let mut s = lock(&self.inner.store);
            anyhow::ensure!(self.inner.enabled.load(Ordering::Relaxed), "diagnostics disabled");
            anyhow::ensure!(!self.inner.stopped.load(Ordering::Acquire), "diagnostics stopped");
            if self.enabled() || !self.inner.queue.load().is_empty() {
                self.drain(&mut s)?;
            }
            s.export_snapshot_for_save(reason, last_minutes)?
        };
        #[cfg(all(test, debug_assertions))]
        if let Some(hook) = lock(&self.inner.export_hook).clone() {
            hook();
        }
        snapshot.prepare_range(reason, last_minutes)?;
        if self.inner.collection.get().is_some() {
            self.export_snapshot(
                snapshot,
                reason,
                self.inner.dropped.load(Ordering::Relaxed),
                self.inner.errors.load(Ordering::Relaxed),
            )
        } else {
            snapshot.export_sync(
                reason,
                self.inner.dropped.load(Ordering::Relaxed),
                self.inner.errors.load(Ordering::Relaxed),
                complete,
            )
        }
    }
    fn export_snapshot(
        &self,
        mut snapshot: store::Store,
        reason: Reason,
        dropped: u64,
        errors: u64,
    ) -> anyhow::Result<BundleInfo> {
        if let Some(context) = self.inner.collection.get() {
            context.export(snapshot, reason, dropped, errors)
        } else {
            if snapshot.requested_range.is_none() {
                snapshot.prepare_range(reason, None)?;
            }
            snapshot.export(reason, dropped, errors)
        }
    }
    pub fn save_range(&self, last_minutes: Option<u32>) -> anyhow::Result<BundleInfo> {
        anyhow::ensure!(last_minutes != Some(0), "minutes must be positive");
        let generation = self.inner.save_generation.load(Ordering::Acquire);
        self.finish_save(generation, Reason::Manual, last_minutes, || {
            self.export_cutoff_range(Reason::Manual, None, last_minutes)
        })
    }
    pub fn save(&self, reason: Reason) -> anyhow::Result<BundleInfo> {
        if matches!(reason, Reason::Emergency | Reason::PeerRestart) {
            self.wait_automatic_recovery()?;
        } else {
            self.ensure_recovered()?;
        }
        self.save_once(reason, || self.export_cutoff(reason, None))
    }
    pub(crate) fn status_changes(&self) -> tokio::sync::watch::Receiver<(u64, u64)> {
        self.inner.status_changed.subscribe()
    }
    pub(crate) fn publish_statistics(&self, previous: &mut Instant, now: Instant) {
        if statistics_due(previous, now) {
            self.inner.status_changed.send_modify(|revision| revision.1 += 1);
        }
    }
    fn notify_status(&self) {
        self.inner.status_changed.send_modify(|revision| revision.0 += 1);
    }
    #[cfg(all(test, debug_assertions))]
    pub(crate) fn saved_bundles(&self) -> tokio::sync::watch::Receiver<u64> {
        lock(&self.inner.store).saved.subscribe()
    }
    pub fn folder(&self) -> anyhow::Result<String> {
        lock(&self.inner.store).folder()
    }
    #[cfg(all(test, debug_assertions))]
    pub fn list(&self) -> anyhow::Result<Vec<BundleInfo>> {
        lock(&self.inner.store).list()
    }
    pub fn status(&self) -> RecorderStatus {
        let mut s = lock(&self.inner.store).status();
        s.configured_enabled = self.inner.configured_enabled.load(Ordering::Acquire);
        s.suspended = self.inner.suspended.load(Ordering::Acquire);
        s.enabled =
            self.inner.enabled.load(Ordering::Acquire) && !s.suspended && !self.inner.stopped.load(Ordering::Acquire);
        s.recovering = self.inner.recovering.load(Ordering::Acquire);
        s.dropped = self.inner.dropped.load(Ordering::Relaxed);
        s.errors = self.inner.errors.load(Ordering::Relaxed);
        s.queued = self.inner.queue.load().len();
        s
    }
    pub fn shutdown(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.inner.worker_thread.get() {
            thread.unpark();
        }
        let incomplete = self.inner.recovering.load(Ordering::Acquire) || lock(&self.inner.recovery_failed).is_some();
        self.stop_recovery();
        let mut s = lock(&self.inner.store);
        // The worker is joined and old export snapshots still have cancelled
        // tokens. A fresh token permits the final bounded queue flush only.
        s.cancelled = Arc::new(AtomicBool::new(false));
        if self.inner.enabled.load(Ordering::Relaxed) {
            if !self.inner.suspended.load(Ordering::Acquire) || !self.inner.queue.load().is_empty() {
                let _ = self.drain(&mut s);
            }
            if incomplete && s.previous_instance.is_some() {
                // Preserve an unfinished earlier crash across a normal stop during
                // recovery. Original recover-* directories remain durable too.
                let _ = s.preserve_recovery_marker();
            } else {
                let _ = s.clean_shutdown();
            }
        }
    }
    fn drain(&self, s: &mut store::Store) -> anyhow::Result<()> {
        // Fixed budget: producers cannot keep the diagnostic worker/API here forever.
        let queue = self.inner.queue.load();
        let rows = (0..QUEUE).filter_map(|_| queue.pop()).collect::<Vec<_>>();
        let mut rows = rows;
        if rows.is_empty() {
            let stamp = self.inner.clock.now();
            rows.push(Row {
                mono_us: stamp.mono_us,
                clock_id: stamp.clock_id,
                utc_us: stamp.utc_us,
                event: Event::State {
                    kind: StateKind::RecorderTick,
                    active: true,
                    count: self.inner.dropped.load(Ordering::Relaxed),
                    value: self.inner.errors.load(Ordering::Relaxed) as f64,
                },
            });
        }
        s.append(rows, self.inner.dropped.load(Ordering::Relaxed))
    }
}
fn worker(weak: Weak<Inner>) {
    let mut last_status = Instant::now();
    loop {
        std::thread::park();
        let Some(inner) = weak.upgrade() else { return };
        if inner.stopped.load(Ordering::Acquire) {
            return;
        }
        // The first queued event schedules one bounded flush. No idle timer/tick.
        if inner.queue.load().is_empty() {
            inner.worker_notified.store(false, Ordering::Release);
            if !inner.queue.load().is_empty() {
                std::thread::current().unpark();
            }
            continue;
        }
        std::thread::park_timeout(Duration::from_secs(1));
        inner.worker_notified.store(false, Ordering::Release);
        if inner.stopped.load(Ordering::Acquire) {
            return;
        }
        let r = Recorder { inner };
        if r.inner.enabled.load(Ordering::Acquire) {
            let mut s = lock(&r.inner.store);
            if r.inner.enabled.load(Ordering::Acquire)
                && !r.inner.queue.load().is_empty()
                && r.drain(&mut s).and_then(|_| s.prune()).is_err()
            {
                r.inner.errors.fetch_add(1, Ordering::Relaxed);
            }
            // Recorder bytes/counters are aggregated here, never by a UI poll or
            // a global state timer. Control transitions notify independently.
            r.publish_statistics(&mut last_status, Instant::now());
        }
    }
}

// The only projection of a device's runtime environment. Every caller uses the
// producing node and its current anonymization domain, never the collecting peer.
// Keep the explicit scalar allow-list here; do not serialize Node/Settings wholesale.
fn runtime_environment(node: &crate::node::Node, anon: &Anonymizer) -> Snapshot {
    let settings = node.settings();
    let os_version = system_version();
    let mut snapshot = Snapshot {
        #[cfg(target_os = "macos")]
        file_provider: lock(&node.inner.file_provider).snapshot().reason.map(Into::into),
        #[cfg(not(target_os = "macos"))]
        file_provider: None,
        backend_uiaccess: crate::backend::launch::lifetime().uiaccess,
        group_state: node.link_config().ok().map(|g| g.diagnostic_excerpt().to_string()),
        version: env!("OINK_RELEASE_VERSION").into(),
        source_tree_sha256: env!("OINK_DIAGNOSTIC_SOURCE_TREE_SHA256").into(),
        os: std::env::consts::OS.into(),
        os_version,
        layout: node
            .link_config()
            .ok()
            .and_then(|config| config.diagnostic_layout(anon)),
        permissions: Some(node.input_backend().permissions()),
        settings: [
            ("sharing_enabled".into(), !node.fully_paused()),
            (
                "focus_locked".into(),
                node.input_status().is_ok_and(|s| s.locked_device.is_some()),
            ),
            ("diagnostics_enabled".into(), settings.diagnostics_enabled),
            ("forward_gestures".into(), settings.routing.forward_gestures),
            ("dnd".into(), settings.routing.dnd),
            ("emergency_combo".into(), settings.routing.emergency_combo),
            ("wake_displays_on_input".into(), settings.routing.wake_displays_on_input),
            (
                "return_pointer_on_display_sleep".into(),
                settings.routing.return_pointer_on_display_sleep,
            ),
            ("cursor_trail".into(), settings.routing.cursor_trail),
        ]
        .into(),
        routing_numbers: [
            ("heartbeat_interval_ms", settings.routing.heartbeat_interval_ms as f64),
            ("heartbeat_timeout_ms", settings.routing.heartbeat_timeout_ms as f64),
            ("line_height_ratio", settings.routing.line_height_ratio),
            (
                "fallback_reference_per_native_unit",
                settings.routing.fallback_reference_per_native_unit,
            ),
            ("max_forward_hops", settings.routing.max_forward_hops as f64),
            ("remote_scroll_speed", settings.routing.remote_scroll_speed),
            ("emergency_hold_ms", settings.routing.emergency_hold_ms as f64),
            ("dnd_push_fraction", settings.routing.dnd_push_fraction),
            (
                "edge_resistance_corner_fraction",
                settings.routing.edge_resistance_corner_fraction,
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .chain(match settings.routing.damping.platform_profile {
            oink_routing::damping::DampingProfile::EnergyDepth {
                energy,
                acceleration,
                max_weight,
                max_depth,
            } => vec![
                ("damping_energy".into(), energy),
                ("damping_acceleration".into(), acceleration),
                ("damping_max_weight".into(), max_weight),
                ("damping_max_depth".into(), max_depth),
            ],
            oink_routing::damping::DampingProfile::DepthOnly { depth } => vec![("damping_depth".into(), depth)],
        })
        .collect(),
        button_handoff: Some(settings.routing.button_handoff),
        reference_screen: settings
            .routing
            .fixed_reference_display
            .as_ref()
            .map(|s| anon.screen(node.inner.me, s)),
        injection_failures: node.inner.input.get().map_or(0, |i| i.injection_failures()),
        session_locked: node.input_backend().session_locked(),
        crash_report: {
            static REPORT: std::sync::OnceLock<crash_report::Report> = std::sync::OnceLock::new();
            REPORT.get_or_init(crash_report::latest).clone()
        },
    };
    // The typed field is in memory only; first/rekey segments must carry the
    // same persisted reason as snapshots refreshed just before collection.
    if let Some(provider) = snapshot.file_provider {
        snapshot
            .routing_numbers
            .insert("file_provider_reason".into(), provider.code());
    }
    snapshot
}
impl Snapshot {
    fn current_build() -> Self {
        Self {
            version: env!("OINK_RELEASE_VERSION").into(),
            source_tree_sha256: env!("OINK_DIAGNOSTIC_SOURCE_TREE_SHA256").into(),
            os: std::env::consts::OS.into(),
            os_version: system_version(),
            ..Self::default()
        }
    }
}

fn system_version() -> String {
    #[cfg(windows)]
    return oink_crash::windows::system_version()
        .map(|(major, minor, build)| format!("{major}.{minor}.{build}"))
        .unwrap_or_default();
    #[cfg(not(windows))]
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    #[cfg(not(windows))]
    VERSION
        .get_or_init(|| {
            #[cfg(target_os = "macos")]
            let out = std::process::Command::new("sw_vers")
                .arg("-productVersion")
                .output()
                .ok();
            #[cfg(not(any(target_os = "macos", windows)))]
            let out = std::process::Command::new("uname").arg("-r").output().ok();
            out.map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .chars()
                    .filter(|c| c.is_ascii_digit() || *c == '.')
                    .take(64)
                    .collect()
            })
            .unwrap_or_default()
        })
        .clone()
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("enabled", &self.enabled())
            .finish_non_exhaustive()
    }
}

impl Recorder {
    pub(crate) fn peer_lifecycle(&self, peer: DeviceId, phase: &'static str, instance: Option<&str>) {
        let mut labels = vec![
            ("peer".into(), self.anonymizer().device(peer)),
            ("phase".into(), phase.into()),
        ];
        if let Some(instance) = instance.filter(|s| s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit())) {
            labels.push(("instance".into(), instance.into()));
        }
        self.record(Event::Log {
            module: module_path!().into(),
            line: line!(),
            level: 2,
            fields: Vec::new(),
            labels,
        });
    }
    pub(crate) fn remote_message(&self, mut from: DeviceId, msg: &oink_routing::Packet) {
        use oink_routing::input::{InputMsg, ScrollAmount};
        let (oink_routing::Packet::Input(input) | oink_routing::Packet::TimedInput { input, .. }) = msg else {
            return;
        };
        let mut msg = input;
        if !self.enabled() {
            return;
        }
        let mut depth = 0;
        while let InputMsg::Forwarded { origin, msg: inner, .. } = msg {
            if depth >= 8 {
                return;
            }
            depth += 1;
            from = origin.device_id;
            msg = inner;
        }
        let mut event = match msg {
            InputMsg::TouchpadContacts(_) | InputMsg::VolumeKeyOrigin { .. } => return,
            InputMsg::Motion { seq, dx, dy } => SafeInput::RemoteMotion {
                seq: *seq,
                dx: *dx,
                dy: *dy,
            },
            InputMsg::Key { code, pressed } | InputMsg::CapsKey { code, pressed } => SafeInput::Key {
                class: privacy::KeyClass::from_code(*code),
                pressed: *pressed,
                repeat: Some(false),
                diagnostic: privacy::KeyDiagnostic::default(),
            },
            InputMsg::Volume {
                code, pressed, repeat, ..
            } => SafeInput::Key {
                class: privacy::KeyClass::from_code(*code),
                pressed: *pressed,
                repeat: Some(*repeat),
                diagnostic: privacy::KeyDiagnostic::default(),
            },
            InputMsg::KeyRepeat { code, .. } => SafeInput::Key {
                class: privacy::KeyClass::from_code(*code),
                pressed: true,
                repeat: Some(true),
                diagnostic: privacy::KeyDiagnostic::default(),
            },
            InputMsg::Button { button, pressed } => SafeInput::Button {
                button: *button,
                pressed: *pressed,
            },
            InputMsg::Scroll {
                dx,
                dy,
                unit: ScrollAmount::Reference,
                phase,
                ..
            } => SafeInput::ScrollDistance {
                dx: *dx,
                dy: *dy,
                phase: *phase,
            },
            InputMsg::Scroll {
                dx,
                dy,
                unit: ScrollAmount::Lines,
                phase,
                ..
            } => SafeInput::Scroll {
                dx: *dx,
                dy: *dy,
                phase: *phase,
                unit: oink_foundation::input::ScrollUnit::Line,
            },
            InputMsg::Gesture { gesture, .. } => SafeInput::Gesture(*gesture),
            InputMsg::RelativeMotion {
                epoch,
                generation,
                seq,
                x,
                y,
            } => SafeInput::RelativeTotals {
                epoch: *epoch,
                generation: *generation,
                seq: *seq,
                x: *x,
                y: *y,
            },
            InputMsg::HeldSync { keys, buttons } => {
                self.record(Event::State {
                    kind: StateKind::HeldSync,
                    active: !keys.is_empty() || !buttons.is_empty(),
                    count: keys.len() as u64,
                    value: buttons.len() as f64,
                });
                return;
            }
            InputMsg::Forwarded { .. } => return,
        };
        let code = match msg {
            InputMsg::Key { code, .. } | InputMsg::Volume { code, .. } | InputMsg::KeyRepeat { code, .. } => {
                Some(*code)
            }
            _ => None,
        };
        let _ = self.annotate_key(&mut event, code, Origin::Remote, Some(from));
        self.record(Event::Input {
            origin: Origin::Remote,
            peer: Some(self.anonymizer().device(from)),
            original_us: None,
            cursor: None,
            event,
            success: None,
            modifiers: None,
            disposition: None,
        });
    }
}
pub(crate) fn api_event(recorder: &Recorder, event: &crate::api::ApiEvent) {
    use crate::api::{ApiEvent, ClipKind, ClipOutcome, FileTransferPhase};
    if !recorder.enabled() {
        return;
    }
    match event {
        ApiEvent::ClipActivity(c) => recorder.record(Event::Clipboard {
            kind_mask: c.kinds.iter().fold(0, |a, k| {
                a | match k {
                    ClipKind::Text => 1,
                    ClipKind::Html => 2,
                    ClipKind::Rtf => 4,
                    ClipKind::Image => 8,
                    ClipKind::Files => 16,
                    ClipKind::Custom => 32,
                }
            }),
            bytes: c.bytes,
            outcome: match c.outcome {
                ClipOutcome::Synced => 0,
                ClipOutcome::Skipped { .. } => 1,
                ClipOutcome::Failed { .. } => 2,
            },
        }),
        ApiEvent::FileTransfer(Some(f)) => recorder.record(Event::Files {
            files: f.files as u64,
            files_done: f.files_done as u64,
            bytes: f.total,
            bytes_done: f.done,
            phase: match f.phase {
                FileTransferPhase::Pending => 0,
                FileTransferPhase::Downloading => 1,
                FileTransferPhase::Ready => 2,
                FileTransferPhase::Failed => 3,
                FileTransferPhase::Cancelled => 4,
            },
        }),
        ApiEvent::Permissions(p) => recorder.record(Event::State {
            kind: StateKind::Permission,
            active: true,
            count: p.len() as u64,
            value: 0.0,
        }),
        _ => {}
    }
}

impl Recorder {
    /// Called by D0's owner, independently of sharing pause or input streams.
    pub(crate) fn record_native(&self, stamp: Option<(u64, i64, i64)>, event: Event) {
        if let Some((mono_us, utc_us, clock_id)) = stamp {
            self.with_capture_stamp(
                clock::Stamp {
                    mono_us,
                    utc_us,
                    clock_id,
                },
                || self.record(event),
            );
        } else {
            self.record(event);
        }
    }
    pub(crate) fn install_native_clock(&self) {
        let clock = self.inner.clock.clone();
        oink_input::native_lifecycle::set_clock(Arc::new(move || {
            let stamp = clock.now();
            (stamp.mono_us, stamp.utc_us, stamp.clock_id)
        }));
    }
    pub(crate) fn sample_links(&self, links: &oink_network::link::LinkTable) {
        for link in &links.links {
            let sampled_at = Instant::now();
            let stamp = self.inner.clock.now();
            let capture_span = sampled_at.elapsed();
            let peer = self.anonymizer().device(link.peer);
            self.with_capture_stamp(stamp, || {
                self.record(Event::Link {
                    peer: peer.clone(),
                    online: link.state == oink_network::link::LinkState::Ready,
                    rtt_us: link.rtt.map(|r| r.as_micros() as u64),
                    sent_packets: Some(link.sent_packets),
                    lost_packets: Some(link.lost_packets),
                });
                if let (Some(rtt), Some(completed_at)) = (link.rtt, link.rtt_measured_at) {
                    // The 1 Hz row can observe a Pong completed hundreds of ms
                    // earlier. Keep its age so ETW/socket arrival and heartbeat
                    // processing can be compared without treating the row as a
                    // receive timestamp. The clock-read bracket bounds sampling
                    // uncertainty; no per-packet tracing runs on the network thread.
                    self.record(Event::Log {
                        module: "oink_heartbeat".into(),
                        line: 0,
                        level: 2,
                        fields: vec![
                            ("rtt_us".into(), rtt.as_secs_f64() * 1e6),
                            (
                                "sample_age_us".into(),
                                sampled_at.saturating_duration_since(completed_at).as_secs_f64() * 1e6,
                            ),
                            ("capture_span_us".into(), capture_span.as_secs_f64() * 1e6),
                        ],
                        labels: vec![("peer".into(), peer)],
                    });
                }
            });
        }
    }
    /// Ordinary diagnostics sampling reads the network owner's published snapshot.
    pub(crate) fn sample_scheduler(&self, status: &oink_network::link::SchedulerStatus) {
        self.sample_scheduler_role("Network", status);
    }
    pub(crate) fn sample_scheduler_role(&self, role: &str, status: &oink_network::link::SchedulerStatus) {
        use oink_foundation::realtime::Tier;
        let (present, native, tier) = status.applied.as_ref().map_or((0., 0., 0.), |applied| {
            (
                1.,
                applied.native as f64,
                match applied.tier {
                    Tier::Elevated => 1.,
                    Tier::Realtime => 2.,
                },
            )
        });
        self.record(Event::Log {
            module: "oink_scheduler".into(),
            line: 0,
            level: 2,
            fields: vec![
                ("readback_present".into(), present),
                ("native_priority".into(), native),
                ("tier".into(), tier),
                ("demotions".into(), status.demoted as f64),
                ("late_rounds".into(), status.late_rounds as f64),
                ("late_max_us".into(), status.late_max.as_secs_f64() * 1e6),
                ("over_budget".into(), status.over_budget as f64),
                ("over_budget_max_us".into(), status.over_budget_max.as_secs_f64() * 1e6),
            ],
            labels: vec![("source".into(), role.into())],
        });
    }
}

/// Measurement aggregation belongs to the recorder's flush source.
fn statistics_due(previous: &mut Instant, now: Instant) -> bool {
    if now.saturating_duration_since(*previous) < Duration::from_secs(3) {
        return false;
    }
    *previous = now;
    true
}
