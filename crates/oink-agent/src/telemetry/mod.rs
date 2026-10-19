//! Per-user owner. Producers get only fixed numeric ingress; the authenticated
//! agent API owns consent. No peer/group protocol or diagnostic records enter here.
#[cfg(target_os = "macos")]
mod inbox;
pub mod projections;
use crossbeam_queue::ArrayQueue;
use oink_telemetry::{
    capture::Manifest,
    store::Store,
    transport::{Endpoint, Https},
    *,
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Unavailable,
    NumericCapsule,
    AttributedOsReport,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportStatus {
    pub consent: ReportConsent,
    pub configured: bool,
    pub region: Option<Region>,
    pub queued_performance: u16,
    pub queued_crashes: u16,
    pub delivery_disabled: bool,
    pub rust_crash: Coverage,
    pub native_crash: Coverage,
    pub file_provider: Coverage,
}
pub use oink_crash::capsule::fatal_boundary;
pub use oink_telemetry::realtime::Stamp;
#[derive(Debug)]
struct Observation {
    generation: u64,
    at: u64,
    report: Report,
}
#[derive(Debug)]
pub struct Ingress {
    generation: AtomicU64,
    queue: ArrayQueue<Observation>,
    wake: tokio::sync::Notify,
    pub(crate) build: BuildContext,
    epoch: Instant,
    enabled_us: AtomicU64,
    busy_until_us: AtomicU64,
    realtime: oink_telemetry::realtime::Ring,
}
impl Ingress {
    pub fn generation(&self) -> Option<u64> {
        let v = self.generation.load(Ordering::Acquire);
        (v != 0).then_some(v)
    }
    /// Called only by an existing ordinary statistics owner with observed work.
    pub fn mark_busy(&self) {
        self.busy_until_us
            .store(self.epoch.elapsed().as_micros() as u64 + 2_000_000, Ordering::Release);
    }
    fn busy(&self) -> bool {
        (self.epoch.elapsed().as_micros() as u64) < self.busy_until_us.load(Ordering::Acquire)
    }
    pub fn window_eligible(&self, window_us: u64) -> bool {
        self.generation().is_some()
            && window_us > 0
            && (self.epoch.elapsed().as_micros() as u64).saturating_sub(self.enabled_us.load(Ordering::Acquire))
                >= window_us
    }
    /// Ordinary owners may read the clock here; callbacks use stamp_at instead.
    pub fn stamp(&self) -> Option<Stamp> {
        self.realtime.stamp(Instant::now())
    }
    pub fn stamp_at(&self, at: Instant) -> Option<Stamp> {
        self.realtime.stamp(at)
    }
    pub fn input_stamp_at(&self, at: Instant, lossy: bool) -> Option<Stamp> {
        self.realtime.input_stamp(at, lossy)
    }
    pub fn interval(&self, stamp: Stamp, metric: Metric, end: Instant) {
        self.realtime.record(stamp, metric, end);
    }
    /// Called by existing ordinary statistics owners, never a callback. No extra
    /// telemetry poll timer or wakeup is added to the realtime path.
    pub fn drain_realtime(&self) {
        let current = Instant::now();
        let wall = now();
        while let Some(n) = self.realtime.pop() {
            if self.generation() != Some(n.stamp.generation) {
                continue;
            }
            let Some(age) = current.checked_duration_since(n.stamp.at) else {
                continue;
            };
            let mut context = self.context(n.metric);
            context.transport = n.stamp.transport;
            self.submit(
                n.stamp.generation,
                wall.saturating_sub(age.as_secs()),
                Report::MeasuredInterval(MeasuredInterval {
                    context,
                    elapsed_us: n.elapsed_us,
                }),
            );
        }
    }
    pub fn context(&self, metric: Metric) -> MeasurementContext {
        MeasurementContext {
            build: self.build,
            metric,
            platform_pair: None,
            network_path: None,
            transport: None,
            codec: None,
        }
    }
    pub fn submit(&self, generation: u64, at: u64, report: Report) {
        if self.generation() != Some(generation) {
            return;
        }
        if self.queue.push(Observation { generation, at, report }).is_ok() {
            self.wake.notify_one();
        }
    }
}
// A single process-lifetime ingress. OnceLock::get is one acquire load and
// never allocates or waits. The retained Arc prevents callback reclamation races.
static PRODUCER: std::sync::OnceLock<Arc<Ingress>> = std::sync::OnceLock::new();
pub fn producer() -> Option<&'static Ingress> {
    PRODUCER.get().map(Arc::as_ref)
}

#[derive(Debug)]
struct State {
    consent: ReportConsent,
    store: Option<Store>,
}
#[derive(Debug)]
pub struct Owner {
    state: Arc<Mutex<State>>,
    pub ingress: Arc<Ingress>,
    endpoint: Option<Endpoint>,
    manifest: Option<Manifest>,
    dir: PathBuf,
    cancel: tokio::sync::watch::Sender<u64>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    lifecycle: Mutex<()>,
    capture: Mutex<Option<Arc<oink_crash::capsule::Capture>>>,
    #[cfg(target_os = "macos")]
    inbox: Arc<Mutex<Option<inbox::Inbox>>>,
    #[cfg(windows)]
    collector: Mutex<Option<oink_crash::collector::Collector>>,
}
fn packaged_manifest() -> (AppVersion, Option<Manifest>) {
    let version = env!("OINK_RELEASE_VERSION")
        .split(['-', '+'])
        .next()
        .unwrap()
        .split('.')
        .map(str::parse::<u16>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("valid compiled version");
    let expected = AppVersion {
        major: version[0],
        minor: version[1],
        patch: version[2],
        build: env!("OINK_RELEASE_BUILD").parse().expect("valid compiled build"),
    };
    let manifest = std::env::current_exe()
        .ok()
        .and_then(|executable| Manifest::load(&executable, expected, env!("OINK_DIAGNOSTIC_SOURCE_TREE_SHA256")));
    (expected, manifest)
}

mod ui;
pub use ui::UiCapture;

impl Owner {
    pub fn new(data_dir: &Path, consent: ReportConsent) -> Self {
        let (expected, manifest) = packaged_manifest();
        let endpoint = manifest.as_ref().and_then(|_| Endpoint::compiled());
        #[cfg(windows)]
        let endpoint = endpoint.filter(|_| oink_crash::collector::allowed_process(std::process::id()));
        let build = BuildContext {
            app_version: expected,
            os_version_class: projections::os_class(),
        };
        let ingress = Arc::new(Ingress {
            generation: AtomicU64::new(0),
            queue: ArrayQueue::new(512),
            wake: tokio::sync::Notify::new(),
            build,
            epoch: Instant::now(),
            enabled_us: AtomicU64::new(0),
            busy_until_us: AtomicU64::new(0),
            realtime: oink_telemetry::realtime::Ring::new(
                512,
                Instant::now(),
                oink_telemetry::sampling::random_below(100).unwrap_or(0) as u8,
            ),
        });
        let dir = data_dir.join("reports");
        let store = if endpoint.is_some() || dir.exists() {
            Store::open(&dir, consent, now()).ok()
        } else {
            None
        };
        let consent = store.as_ref().map(Store::consent).unwrap_or(consent);
        let (cancel, _) = tokio::sync::watch::channel(consent.generation);
        let owner = Self {
            state: Arc::new(Mutex::new(State { consent, store })),
            ingress,
            endpoint,
            manifest,
            dir,
            cancel,
            task: Mutex::new(None),
            lifecycle: Mutex::new(()),
            capture: Mutex::new(None),
            #[cfg(target_os = "macos")]
            inbox: Arc::new(Mutex::new(None)),
            #[cfg(windows)]
            collector: Mutex::new(None),
        };
        if owner.endpoint.is_some() {
            let _ = PRODUCER.set(owner.ingress.clone());
            oink_clipboard::dnd::set_native_timing_enabled(|| producer().is_some_and(|p| p.generation().is_some()));
            oink_clipboard::dnd::set_preparation_observer(projections::drag_preparation);
            oink_audio::playback::set_presentation_observer(
                || producer().is_some_and(|p| p.generation().is_some()),
                projections::audio_presentation,
            );
            oink_network::link::timing::set_observer(projections::link_interval);
        }
        owner.start();
        owner
    }
    pub fn submit_crash(&self, generation: u64, mut crash: SafeCrash, collected_at: u64) -> Result<()> {
        if !matches!(crash.component, Component::UiRust | Component::UiWebview) {
            return Err(Error::Invalid);
        }
        let manifest = self.manifest.as_ref().ok_or(Error::Disabled)?;
        manifest.project(&mut crash)?;
        let mut state = self.state.lock().map_err(|_| Error::Io)?;
        if !state.consent.granted() || state.consent.generation != generation {
            return Err(Error::Stale);
        }
        state.store.as_mut().ok_or(Error::Disabled)?.observe(
            generation,
            Report::SafeCrash(crash),
            collected_at,
            now(),
        )?;
        self.ingress.wake.notify_one();
        Ok(())
    }
    pub fn status(&self) -> ReportStatus {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let queue = state.store.as_ref().map(Store::status);
        let configured = self.endpoint.is_some();
        ReportStatus {
            consent: state.consent,
            configured,
            region: self.endpoint.as_ref().map(|e| e.region),
            queued_performance: queue.map_or(0, |q| q.performance),
            queued_crashes: queue.map_or(0, |q| q.crashes),
            delivery_disabled: queue.is_none_or(|q| q.disabled),
            rust_crash: if configured {
                Coverage::NumericCapsule
            } else {
                Coverage::Unavailable
            },
            native_crash: if !configured {
                Coverage::Unavailable
            } else if cfg!(target_os = "macos") {
                Coverage::AttributedOsReport
            } else {
                Coverage::NumericCapsule
            },
            file_provider: Coverage::Unavailable,
        }
    }
    pub fn set(
        &self,
        expected: u64,
        choice: Choice,
        policy: u16,
        persist: impl FnOnce(ReportConsent) -> Result<()>,
    ) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().map_err(|_| Error::Io)?;
        let mut state = self.state.lock().map_err(|_| Error::Io)?;
        let next = state.consent.change(expected, choice, policy)?;
        // Close ingress and cancel before any disk work; opt-out never waits for HTTP.
        self.ingress.generation.store(0, Ordering::Release);
        self.ingress.realtime.disarm();
        self.ingress.realtime.clear();
        self.cancel.send_replace(next.generation);
        if let Some(task) = self.task.lock().map_err(|_| Error::Io)?.take() {
            task.abort();
        }
        if let Some(c) = self.capture.lock().map_err(|_| Error::Io)?.take() {
            c.disarm();
        }
        #[cfg(target_os = "macos")]
        {
            self.inbox.lock().map_err(|_| Error::Io)?.take();
        }
        #[cfg(windows)]
        {
            self.collector.lock().map_err(|_| Error::Io)?.take();
        }
        while self.ingress.queue.pop().is_some() {}
        state.consent = ReportConsent {
            choice: Choice::Denied,
            ..next
        };
        if let Some(store) = &mut state.store {
            store.set_consent(state_consent_off(next))?;
        }
        persist(next)?;
        if let Some(store) = &mut state.store {
            store.set_consent(next)?;
        }
        state.consent = next;
        drop(state);
        self.start();
        #[cfg(target_os = "macos")]
        self.refresh_file_provider();
        Ok(())
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn refresh_file_provider(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.consent.granted() {
            return;
        }
        let consent = state.consent;
        let (Some(root), Some(manifest), Some(store)) = (
            oink_clipboard::file_provider::report_inbox_root(),
            self.manifest.as_ref(),
            state.store.as_mut(),
        ) else {
            return;
        };
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        if inbox.is_none() {
            *inbox = inbox::Inbox::open(&root, consent, manifest, self.ingress.clone(), store).ok();
        }
    }
    fn start(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let consent = state.consent;
        if !consent.granted() || state.store.is_none() {
            return;
        }
        let (Some(endpoint), Some(manifest)) = (self.endpoint.clone(), self.manifest.as_ref()) else {
            return;
        };
        let Ok(https) = Https::new(endpoint) else { return };
        self.recover_and_arm(&mut state, consent, manifest);
        self.ingress
            .enabled_us
            .store(self.ingress.epoch.elapsed().as_micros() as u64, Ordering::Release);
        self.ingress.realtime.arm(consent.generation, Instant::now());
        self.ingress.generation.store(consent.generation, Ordering::Release);
        drop(state);
        let shared = self.state.clone();
        let ingress = self.ingress.clone();
        let mut cancel = self.cancel.subscribe();
        #[cfg(target_os = "macos")]
        let inbox = self.inbox.clone();
        #[cfg(target_os = "macos")]
        let manifest = manifest.clone();
        let task = tokio::spawn(async move {
            loop {
                if ingress.generation() != Some(consent.generation) {
                    break;
                }
                let (submission, pending) = {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(store) = &mut state.store else { break };
                    #[cfg(target_os = "macos")]
                    if let Some(inbox) = inbox.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                        inbox.recover(consent, &manifest, store);
                    }
                    while let Some(observation) = ingress.queue.pop() {
                        let _ = store.observe(observation.generation, observation.report, observation.at, now());
                    }
                    let _ = store.checkpoint();
                    let next = if ingress.busy() {
                        None
                    } else {
                        store.prepare(consent.generation, now()).ok().flatten()
                    };
                    (next, store.has_work())
                };
                if let Some(submission) = submission {
                    if ingress.generation() != Some(submission.generation) {
                        continue;
                    }
                    let result =
                        tokio::select! {biased; _=cancel.changed()=>break, result=https.send(&submission)=>result};
                    if let Some(store) = &mut shared.lock().unwrap_or_else(|e| e.into_inner()).store {
                        let _ = store.complete(&submission, result);
                    }
                }
                tokio::select! {biased; _=cancel.changed()=>break, _=ingress.wake.notified()=>{}, _=tokio::time::sleep(Duration::from_secs(60)),if pending=>{}}
            }
        });
        *self.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
    }
    fn recover_and_arm(&self, state: &mut State, consent: ReportConsent, manifest: &Manifest) {
        let capsule = self.dir.join("capsules/agent.bin");
        #[cfg(not(windows))]
        let mut recovered = None;
        #[cfg(windows)]
        let mut recovered = oink_crash::staging::recover(&self.dir.join("raw"), consent, manifest)
            .into_iter()
            .next();
        #[cfg(target_os = "macos")]
        {
            let lifetime = self.dir.join("capsules/lifetime.json");
            if let Ok(bytes) = std::fs::read(&lifetime)
                && bytes.len() <= 4096
                && let Ok(lifetime) = serde_json::from_slice::<oink_crash::macos::Lifetime>(&bytes)
                && let Some(home) = std::env::var_os("HOME")
            {
                recovered = oink_crash::macos::recover(
                    &PathBuf::from(home).join("Library/Logs/DiagnosticReports"),
                    lifetime,
                    consent,
                    manifest,
                    now(),
                );
            }
        }
        if recovered.is_none() {
            recovered =
                oink_crash::capsule::recover(&capsule, consent, manifest, self.ingress.build.os_version_class, now());
        }
        if let Some((crash, at)) = recovered
            && let Some(store) = &mut state.store
        {
            let _ = store.observe(consent.generation, Report::SafeCrash(crash), at, now());
        }
        #[cfg(windows)]
        if let Ok(executable) = std::env::current_exe()
            && let Some(parent) = executable.parent()
        {
            *self.collector.lock().unwrap_or_else(|e| e.into_inner()) = oink_crash::collector::Collector::start(
                &parent.join("oink-crash-collector.exe"),
                &self.dir.join("raw"),
                consent,
                self.ingress.build,
                manifest,
                Component::Agent,
            )
            .ok();
        }
        let capture = oink_crash::capsule::Capture::install();
        if capture
            .arm(&capsule, consent.generation, Component::Agent, self.ingress.build)
            .is_ok()
        {
            *self.capture.lock().unwrap_or_else(|e| e.into_inner()) = Some(capture);
        }
        #[cfg(target_os = "macos")]
        if let Ok(executable) = std::env::current_exe()
            && let Some(id) = oink_crash::macos::executable_uuid(&executable)
            && let Some(image) = manifest
                .images
                .iter()
                .find(|i| i.slot == ImageSlot::Agent && i.debug_id == id)
            && let Some(launched_at) = oink_crash::macos::process_launch(std::process::id())
        {
            let lifetime = oink_crash::macos::Lifetime {
                schema: 1,
                pid: std::process::id(),
                launched_at,
                armed_at: now(),
                generation: consent.generation,
                component: Component::Agent,
                image: ImageSlot::Agent,
                debug_id: image.debug_id,
                build: self.ingress.build,
            };
            if let Ok(bytes) = serde_json::to_vec(&lifetime) {
                let _ = oink_telemetry::store::atomic_write(&self.dir.join("capsules/lifetime.json"), &bytes);
            }
        }
    }
    pub fn shutdown(&self) {
        let _lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        self.ingress.generation.store(0, Ordering::Release);
        self.ingress.realtime.disarm();
        self.ingress.realtime.clear();
        if let Some(task) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            task.abort();
        }
        if !std::thread::panicking() {
            if let Some(c) = self.capture.lock().unwrap_or_else(|e| e.into_inner()).take() {
                c.disarm();
            }
            let _ = std::fs::remove_file(self.dir.join("capsules/lifetime.json"));
        }
        #[cfg(target_os = "macos")]
        {
            self.inbox.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
        #[cfg(windows)]
        {
            self.collector.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
    }
}
fn state_consent_off(next: ReportConsent) -> ReportConsent {
    ReportConsent {
        choice: Choice::Denied,
        ..next
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn consent_is_default_off_stale_updates_fail_and_disk_failure_stays_off() {
        // These builds deliberately have no vendor configuration.
        if Endpoint::compiled().is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let owner = Owner::new(dir.path(), ReportConsent::default());
        assert!(!owner.status().configured);
        assert_eq!(owner.status().consent.choice, Choice::Unasked);
        assert!(owner.ingress.generation().is_none());
        owner.set(0, Choice::Granted, POLICY_VERSION, |_| Ok(())).unwrap();
        assert_eq!(owner.status().consent.generation, 1);
        assert!(owner.ingress.generation().is_none());
        assert!(owner.task.lock().unwrap().is_none());
        assert_eq!(
            owner.set(0, Choice::Denied, POLICY_VERSION, |_| panic!("stale callback")),
            Err(Error::Stale)
        );
        assert_eq!(
            owner.set(1, Choice::Granted, POLICY_VERSION, |_| Err(Error::Io)),
            Err(Error::Io)
        );
        assert_eq!(owner.status().consent.choice, Choice::Denied);
        assert!(owner.ingress.generation().is_none());
        assert!(!dir.path().join("reports").exists());
    }
    #[test]
    fn local_settings_never_infer_a_grant_from_missing_corrupt_or_old_policy() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::AgentConfig::load_from(dir.path()).unwrap();
        config.save().unwrap();
        let baseline: serde_json::Value =
            serde_json::from_slice(&std::fs::read(config.settings_path()).unwrap()).unwrap();
        for reports in [
            serde_json::json!(null),
            serde_json::json!({"choice":"granted"}),
            serde_json::json!({"choice":"granted","policy_version":2,"generation":1}),
            serde_json::json!({"choice":"granted","policy_version":3,"generation":0}),
            serde_json::json!({"choice":"granted","policy_version":3,"generation":"corrupt"}),
            serde_json::json!({"choice":"granted","policy_version":3,"generation":1,"extra":"unknown"}),
        ] {
            std::fs::write(
                dir.path().join(crate::config::SETTINGS_FILE),
                serde_json::to_vec(&{
                    let mut value = baseline.clone();
                    value["local"]["reports"] = reports;
                    value
                })
                .unwrap(),
            )
            .unwrap();
            assert!(
                !crate::config::AgentConfig::load_from(dir.path())
                    .unwrap()
                    .settings
                    .reports
                    .granted()
            );
        }
    }

    #[test]
    fn concurrent_consent_updates_admit_only_one_expected_generation() {
        if Endpoint::compiled().is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let owner = Arc::new(Owner::new(dir.path(), ReportConsent::default()));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let owner = owner.clone();
                std::thread::spawn(move || owner.set(0, Choice::Denied, POLICY_VERSION, |_| Ok(())))
            })
            .collect();
        let successful = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(Result::is_ok)
            .count();
        assert_eq!(successful, 1);
        assert_eq!(owner.status().consent.generation, 1);
    }
}
