//! Backend update owner; UI has only typed local commands and status events.
use crate::{
    api::{ApiError, ApiEvent, ApiResult},
    node::{Node, lock},
};
mod local;
#[cfg(target_os = "macos")]
pub mod local_mac;
#[cfg(not(windows))]
mod online;
pub mod presentation;
use oink_foundation::maintenance;
pub use oink_update::policy::Phase;
use oink_update::{Error, barrier::Blocker, policy::*};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

const INPUT_PREPARATION_BUDGET: Duration = Duration::from_secs(6);
const AUDIO_PREPARATION_BUDGET: Duration = Duration::from_secs(3);
// One absolute Mac preparation deadline: 3s input reply + 3s fence + 3s
// current audio cleanup + two 8s provider commands. Each step uses its remainder
// until the replacement helper acknowledges readiness.
#[cfg(target_os = "macos")]
const PREPARATION_BUDGET: Duration = INPUT_PREPARATION_BUDGET
    .saturating_add(AUDIO_PREPARATION_BUDGET)
    .saturating_add(mac_provider::PREPARATION_BUDGET);
#[cfg(not(target_os = "macos"))]
const PREPARATION_BUDGET: Duration = Duration::from_secs(25);

#[derive(Debug)]
pub(crate) struct Controller {
    #[cfg(not(windows))]
    pub policy: Policy,
    pub status: UpdateStatus,
    local: Option<local::Staged>,
    local_mode: bool,
    #[cfg(not(windows))]
    installing: bool,
    #[cfg(not(windows))]
    restored_result: bool,
    #[cfg(not(windows))]
    online: online::Online,
    #[cfg(windows)]
    wake: Option<std::sync::mpsc::SyncSender<()>>,
}
impl Controller {
    pub(crate) fn load(dir: &Path) -> Self {
        let loaded = match std::fs::read(dir.join("updates.json")) {
            Ok(bytes) if bytes.len() <= 65536 => oink_update::feed::strict_json(&bytes),
            Ok(_) => Err(Error::Metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Policy::default()),
            Err(_) => Err(Error::Storage),
        };
        let error = loaded.as_ref().err().copied();
        let mut policy = loaded.unwrap_or_default();
        // An unreadable off/deferral setting must not silently turn checks back on.
        if error.is_some() {
            policy.preferences.checks_enabled = false;
        }
        let mut status = UpdateStatus {
            current_build: option_env!("OINK_RELEASE_BUILD")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            version: oink_update::RELEASE_VERSION.into(),
            checks_enabled: false,
            channel: policy.preferences.channel(),
            update_channel: policy.preferences.update_channel,
            install_after_download: policy.preferences.install_after_download,
            phase: Phase::Disabled,
            candidate: None,
            checked_at: None,
            next_check: None,
            deferred_until: policy.deferred_until,
            blockers: vec![],
            progress: None,
            error: error.or(Some(Error::NotConfigured)),
            failed_build: None,
            critical_prompted_build: policy.critical_prompted_build,
            scope: if cfg!(windows) { Scope::Machine } else { Scope::User },
            // The Windows service also accepts the user's preferences unelevated.
            can_change_policy: true,
        };
        let restored = presentation::outcome(dir, status.current_build);
        if let Some(result) = restored {
            status.phase = match result {
                presentation::Outcome::Committed => Phase::Committed,
                presentation::Outcome::Failed => Phase::Failed,
            };
            status.error = (result == presentation::Outcome::Failed).then_some(Error::Transaction);
        }
        Self {
            #[cfg(windows)]
            wake: None,
            #[cfg(not(windows))]
            restored_result: restored.is_some(),
            #[cfg(not(windows))]
            policy,
            status,
            local: None,
            local_mode: false,
            #[cfg(not(windows))]
            installing: false,
            #[cfg(not(windows))]
            online: online::Online::default(),
        }
    }
    /// Starts a check unless one is running or a download owns the candidate;
    /// returns the generation the result must still match.
    #[cfg(not(windows))]
    fn begin_check(&mut self) -> Result<Option<u64>, Error> {
        if self.status.error == Some(Error::NotConfigured) {
            return Err(Error::NotConfigured);
        }
        if self.local_mode
            || matches!(
                self.status.phase,
                Phase::Checking
                    | Phase::Downloading
                    | Phase::Verifying
                    | Phase::Ready
                    | Phase::Quiescing
                    | Phase::Installing
            )
        {
            return Ok(None);
        }
        self.online.generation += 1;
        self.status.phase = Phase::Checking;
        self.status.error = None;
        Ok(Some(self.online.generation))
    }
    #[cfg(not(windows))]
    fn save(&self, dir: &Path) -> Result<(), Error> {
        std::fs::create_dir_all(dir).map_err(|_| Error::Storage)?;
        oink_foundation::storage::atomic_write(
            &dir.join("updates.json"),
            &serde_json::to_vec_pretty(&self.policy).map_err(|_| Error::Storage)?,
        )
        .map_err(|_| Error::Storage)
    }
}
fn api_error(error: Error) -> ApiError {
    tracing::warn!(?error, reason = %error, "update command rejected");
    ApiError::UpdateFailed { update_error: error }
}
#[cfg(not(windows))]
fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
impl Node {
    pub fn query_update_status(&self) -> UpdateStatus {
        if lock(&self.inner.update).local_mode {
            return lock(&self.inner.update).status.clone();
        }
        #[cfg(windows)]
        if let Ok(status) = windows::status() {
            return status;
        }
        lock(&self.inner.update).status.clone()
    }
    pub fn execute_update_command(&self, command: Command) -> ApiResult<UpdateStatus> {
        match &command {
            Command::Install { expected_build } => tracing::info!(
                command_type = "install",
                expected_build,
                step = "backend_update_received",
                "update IPC"
            ),
            Command::Cancel => tracing::info!(command_type = "cancel", step = "backend_update_received", "update IPC"),
            Command::Check => tracing::info!(command_type = "check", step = "backend_update_received", "update IPC"),
            _ => {}
        }
        if let Command::OpenLocal { path } = &command {
            return local::open(self, path);
        }
        if lock(&self.inner.update).local_mode {
            match &command {
                Command::Install { expected_build } => return local::install(self, *expected_build),
                Command::Cancel | Command::Check => {
                    let mut state = lock(&self.inner.update);
                    state.local = None;
                    state.local_mode = false;
                    state.status.candidate = None;
                    state.status.error = None;
                    state.status.progress = None;
                    #[cfg(not(windows))]
                    {
                        state.status.phase = Phase::Idle;
                    }
                    #[cfg(not(windows))]
                    if command == Command::Cancel {
                        let status = state.status.clone();
                        drop(state);
                        self.emit(ApiEvent::UpdateStatusChanged(status.clone()));
                        // OpenLocal already superseded the feed candidate. A
                        // local cancellation must also work in manual-only builds.
                        return Ok(status);
                    }
                }
                _ => return Err(api_error(Error::Busy)),
            }
        }
        #[cfg(windows)]
        {
            let installing = matches!(&command, Command::Install { .. });
            if let Command::Install { expected_build } = &command {
                presentation::begin(
                    self.data_dir(),
                    lock(&self.inner.update).status.current_build,
                    *expected_build,
                )
                .map_err(|_| api_error(Error::Storage))?;
                tracing::info!(target: "oink_update_timing", phase="install_clicked", "Windows update boundary");
            }
            let result = windows::command(self, command);
            if installing && result.is_err() {
                presentation::clear(self.data_dir());
            }
            result
        }
        #[cfg(not(windows))]
        {
            if !oink_update::trust::mac_enabled() {
                return Err(api_error(Error::NotConfigured));
            }
            let mut state = lock(&self.inner.update);
            if state.installing {
                return Err(api_error(Error::Transaction));
            }
            match command {
                Command::OpenLocal { .. } => unreachable!("handled before platform dispatch"),
                Command::Check => {
                    if let Some(generation) = state.begin_check().map_err(api_error)? {
                        online::spawn_check(self, generation, oink_update::catalog::Check::Manual);
                    }
                }
                Command::Download => {
                    if let Some(download) = state.begin_download().map_err(api_error)? {
                        online::spawn_download(self, download);
                    }
                }
                Command::Install { expected_build } => {
                    // A downloaded feed package is staged exactly like a dropped DMG.
                    if state.status.candidate.as_ref().is_none_or(|c| c.local_package) {
                        return Err(api_error(Error::Policy));
                    }
                    drop(state);
                    return local::install(self, expected_build);
                }
                Command::SetPreferences { preferences } => {
                    let previous = state.policy.clone();
                    state.policy.preferences = preferences.clone();
                    if let Err(error) = state.save(self.data_dir()) {
                        state.policy = previous;
                        return Err(api_error(error));
                    }
                    state.status.checks_enabled = preferences.checks_enabled;
                    state.status.channel = preferences.channel();
                    state.status.update_channel = preferences.update_channel;
                    state.status.install_after_download = preferences.install_after_download;
                    // Another channel's candidate never carries over; its check
                    // only offers builds above the installed one, never a downgrade.
                    if previous.preferences.channel() != preferences.channel() {
                        state.supersede();
                        if let Ok(Some(generation)) = state.begin_check() {
                            online::spawn_check(self, generation, oink_update::catalog::Check::Automatic);
                        }
                    } else if preferences.checks_enabled && !previous.preferences.checks_enabled {
                        state.schedule_now();
                    }
                    state.publish_schedule();
                }
                Command::Defer { choice } => {
                    let previous = state.policy.clone();
                    state.policy.defer(choice, now());
                    if let Err(error) = state.save(self.data_dir()) {
                        state.policy = previous;
                        return Err(api_error(error));
                    }
                    state.status.deferred_until = state.policy.deferred_until;
                }
                Command::Skip { expected_build } => {
                    // Only a normal update can be skipped.
                    if state
                        .status
                        .candidate
                        .as_ref()
                        .is_none_or(|c| c.build != expected_build || c.kind != oink_update::catalog::Kind::Normal)
                    {
                        return Err(api_error(Error::Policy));
                    }
                    let previous = state.policy.clone();
                    state.policy.skipped_build = Some(expected_build);
                    if let Err(error) = state.save(self.data_dir()) {
                        state.policy = previous;
                        return Err(api_error(error));
                    }
                    state.supersede();
                }
                Command::Prompted { expected_build } => {
                    if state.status.candidate.as_ref().map(|c| c.build) != Some(expected_build) {
                        return Err(api_error(Error::Policy));
                    }
                    let previous = state.policy.clone();
                    state.policy.critical_prompted_build = Some(expected_build);
                    if let Err(error) = state.save(self.data_dir()) {
                        state.policy = previous;
                        return Err(api_error(error));
                    }
                    state.status.critical_prompted_build = Some(expected_build);
                }
                Command::Cancel => {
                    state.cancel_download();
                }
            }
            let status = state.status.clone();
            drop(state);
            self.emit(ApiEvent::UpdateStatusChanged(status.clone()));
            Ok(status)
        }
    }
    /// Explicit Install interrupts activity; only bounded cleanup/availability
    /// errors can refuse replacement. Admission closes before input is released.
    pub fn prepare_for_update(&self) -> Result<Prepared, Vec<Blocker>> {
        let gate = maintenance::process();
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
        gate.close_for_update(generation)
            .map_err(|_| vec![Blocker::OwnerMissing])?;
        let token = Prepared {
            node: self.clone(),
            gate: gate.clone(),
            generation,
            expires: Instant::now() + PREPARATION_BUDGET,
            consumed: false,
            #[cfg(target_os = "macos")]
            provider: None,
        };
        let input = self.input_thread().map_err(|_| vec![Blocker::OwnerMissing])?;
        let failures = input.injection_failures();
        let (reply, wait) = std::sync::mpsc::sync_channel(1);
        input.send(crate::threads::input::InputCommand::PrepareUpdate { generation, reply });
        let fence = wait
            .recv_timeout(if cfg!(target_os = "macos") {
                token.remaining()
            } else {
                INPUT_PREPARATION_BUDGET / 2
            })
            .map_err(|_| vec![Blocker::OwnerMissing])??;
        let deadline = if cfg!(target_os = "macos") {
            token.expires
        } else {
            Instant::now() + INPUT_PREPARATION_BUDGET / 2
        };
        while fence.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if fence.load(Ordering::Acquire) != 1 || input.injection_failures() != failures {
            return Err(vec![Blocker::HeldInput]);
        }
        if let Some(audio) = self.inner.audio.get() {
            let deadline = if cfg!(target_os = "macos") {
                token.expires
            } else {
                Instant::now() + AUDIO_PREPARATION_BUDGET
            };
            audio.prepare_update(deadline).map_err(|error| {
                tracing::warn!(%error, "update audio cleanup failed");
                vec![Blocker::IntegrationUnavailable]
            })?;
        }
        // Drain the accepted slider snapshot before authorizing replacement,
        // including a backend with no active audio owner. Its retained lease
        // permits this flush after admission closed; failure blocks the update.
        lock(&self.inner.config).flush_deferred().map_err(|error| {
            tracing::warn!(%error, "update settings flush failed");
            vec![Blocker::IntegrationUnavailable]
        })?;
        token.validate()?;
        Ok(token)
    }
}
#[derive(Debug)]
pub struct Prepared {
    node: Node,
    gate: maintenance::Gate,
    generation: u64,
    expires: Instant,
    consumed: bool,
    #[cfg(target_os = "macos")]
    provider: Option<mac_provider::Suspension>,
}
impl Prepared {
    fn remaining(&self) -> Duration {
        self.expires.saturating_duration_since(Instant::now())
    }
    pub fn validate(&self) -> Result<(), Vec<Blocker>> {
        if Instant::now() >= self.expires || self.gate.generation() != Some(self.generation) {
            return Err(vec![Blocker::SessionChanged]);
        }
        Ok(())
    }
    #[cfg(windows)]
    fn hold_for_health(&mut self) {
        self.expires = Instant::now() + Duration::from_secs(600);
        if let Ok(input) = self.node.input_thread() {
            input.send(crate::threads::input::InputCommand::CommitUpdate {
                generation: self.generation,
            });
        }
    }
    pub fn shutdown(self) -> Result<(), Vec<Blocker>> {
        self.shutdown_with_ui(false)
    }

    fn shutdown_with_ui(mut self, quit_ui: bool) -> Result<(), Vec<Blocker>> {
        self.validate()?;
        // Deliver UI Quit before waking host shutdown: the backend main loop
        // may otherwise exit the process before this worker can send it.
        if quit_ui
            && crate::instance::is_ui_running(self.node.data_dir()) == Some(true)
            && let Err(error) = crate::ipc::UiControl::request_stop(self.node.data_dir(), Duration::from_secs(1))
        {
            tracing::warn!(%error, "could not deliver explicit product quit to UI");
        }
        #[cfg(target_os = "macos")]
        if let Some(provider) = self.provider.as_mut() {
            provider.commit();
        }
        self.node.request_stop();
        self.consumed = true;
        Ok(())
    }
}
impl Drop for Prepared {
    fn drop(&mut self) {
        if !self.consumed {
            // Restore the provider before admitting new work after an abort.
            #[cfg(target_os = "macos")]
            drop(self.provider.take());
            self.gate.reopen(self.generation);
            if let Ok(input) = self.node.input_thread() {
                input.send(crate::threads::input::InputCommand::AbortUpdate {
                    generation: self.generation,
                });
            }
        }
    }
}
#[cfg(target_os = "macos")]
mod mac_provider;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::startup_gate;
#[cfg(windows)]
pub(crate) fn attach(node: &Node) {
    windows::attach(node);
}
#[cfg(not(windows))]
pub(crate) fn attach(node: &Node) {
    online::attach(node);
}

#[cfg(not(windows))]
fn install_failure(node: &Node, error: Error, blockers: Vec<Blocker>) {
    tracing::info!(step="install_failed", error=?error, ?blockers, "upgrade aborted before backend exit");
    let mut state = lock(&node.inner.update);
    state.installing = false;
    state.status.phase = Phase::Failed;
    state.status.error = Some(error);
    state.status.blockers = blockers;
    let status = state.status.clone();
    drop(state);
    node.emit(ApiEvent::UpdateStatusChanged(status));
    if presentation::outcome(node.data_dir(), lock(&node.inner.update).status.current_build).is_some() {
        std::thread::spawn(|| {
            if let Err(error) = crate::backend::show_ui() {
                tracing::warn!(%error, "could not show failed update");
            }
        });
    }
}

static QUITTING: AtomicBool = AtomicBool::new(false);
pub(crate) fn request_quit() {
    if QUITTING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(|| {
        let Ok(node) = crate::backend::node() else {
            // No backend runs: a restart after a network-layer failure is
            // waiting out its back-off. Quit instead of starting it again.
            crate::host::cancel_restart();
            QUITTING.store(false, Ordering::Release);
            return;
        };
        match node.prepare_for_update() {
            Ok(token) => {
                #[cfg(windows)]
                let saved = windows::user_quit();
                #[cfg(not(windows))]
                let saved = {
                    let mut state = lock(&node.inner.update);
                    state.policy.next_quit = false;
                    state.save(node.data_dir()).is_ok()
                };
                if saved {
                    let _ = token.shutdown_with_ui(true);
                }
            }
            Err(blockers) => {
                let mut state = lock(&node.inner.update);
                state.status.blockers = blockers;
                let status = state.status.clone();
                drop(state);
                node.emit(ApiEvent::UpdateStatusChanged(status));
            }
        }
        QUITTING.store(false, Ordering::Release);
    });
}
#[cfg(not(windows))]
pub(crate) fn prepare_reopen(node: &Node) -> ApiResult<Prepared> {
    node.prepare_for_update().map_err(|_| api_error(Error::Busy))
}

#[cfg(target_os = "macos")]
fn close_mac_clients(token: &Prepared, parent: &Path) -> Result<mac_provider::Suspension, Error> {
    token.validate().map_err(|_| Error::Busy)?;
    // Unix mapped executables do not lock their containing app directory; the
    // replacement helper swaps the bundle atomically. Other users, CLI commands
    // and manager invocations are not replacement blockers. Close our UI by its
    // existing event contract.
    let provider = mac_provider::Suspension::prepare(parent.join("oink-file-provider"), token.expires, &|| {
        token.validate().is_ok()
    })?;
    token.node.emit(ApiEvent::ExitForUpdate);
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_update_preserves_its_reason_across_the_ui_ipc() {
        #[derive(Clone)]
        struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Log {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let log = Log(Default::default());
        let output = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || output.clone())
            .finish();
        let error = tracing::subscriber::with_default(subscriber, || api_error(Error::Policy));
        let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains("update command rejected") && text.contains("Policy"),
            "missing typed refusal: {text}"
        );
        let wire = serde_json::to_value(&error).unwrap();
        assert_eq!(wire["code"], "update_failed");
        assert_eq!(wire["update_error"], "policy");
        assert_eq!(wire["problem"]["code"], "state_conflict");
        assert_eq!(serde_json::from_value::<ApiError>(wire).unwrap(), error);
    }

    // Uses the debug-only fake-platform Node entry point.
    #[cfg(all(target_os = "macos", debug_assertions))]
    #[test]
    fn local_ready_cancel_is_independent_of_feed_configuration() {
        use crate::api::{AgentApi, Request};
        use crate::ipc::Server;
        use crate::remote::RemoteApi;
        use std::sync::Arc;
        let directory = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let node = runtime
            .block_on(Node::start_with_fake_platform(crate::node::NodeOptions {
                net: oink_test_support::network_config(directory.path().into(), "local-update", 0),
                input: Arc::new(oink_test_support::input::FakeInput::new()),
                displays: vec![],
                display_backend: None,
                clipboard: None,
                mode_switch_in_callback: true,
                capture_reports_key_repeat: true,
                cursor_trail: false,
            }))
            .unwrap();
        {
            let mut state = lock(&node.inner.update);
            state.local_mode = true;
            state.status.phase = Phase::Ready;
            state.status.error = None;
            state.status.candidate = Some(Candidate {
                local_package: true,
                build: 65,
                version: "0.3.0-inhouse.65".into(),
                size: 1,
                published_at: None,
                severity: oink_update::feed::Severity::Normal,
                notes: Some(oink_update::feed::ReleaseNotes::from_json(br#"{"en":"Local fixes"}"#).unwrap()),
                kind: oink_update::catalog::Kind::Normal,
                notice: None,
            });
        }
        let server = runtime.block_on(Server::start(node.clone())).unwrap();
        let remote = RemoteApi::new(directory.path());
        let ready = remote.update_status().unwrap();
        assert_eq!(ready.phase, Phase::Ready);
        assert_eq!(ready.candidate.unwrap().build, 65);
        // A stale confirmation travels through the same Request -> RemoteApi
        // -> server dispatch path as the webview, without authorizing an install.
        let request: Request = serde_json::from_value(serde_json::json!({
            "method":"update", "params":{"command":{"type":"install", "expected_build":66}}
        }))
        .unwrap();
        let refusal = crate::api::dispatch(&remote, request).unwrap_err();
        assert_eq!(serde_json::to_value(refusal).unwrap()["update_error"], "policy");
        let request: Request = serde_json::from_value(serde_json::json!({
            "method":"update", "params":{"command":{"type":"cancel"}}
        }))
        .unwrap();
        let result = crate::api::dispatch(&remote, request);
        runtime.block_on(server.close());
        runtime.block_on(node.shutdown());
        assert!(
            result.is_ok(),
            "a local cancellation must not enter feed policy: {result:?}"
        );
        assert_eq!(node.query_update_status().phase, Phase::Idle);
    }

    #[cfg(not(windows))]
    #[test]
    fn manual_check_clears_old_failure_and_joins_a_running_check_or_download() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = Controller::load(dir.path());
        controller.status.phase = Phase::Failed;
        controller.status.error = Some(Error::Signature);
        let first = controller.begin_check().unwrap().unwrap();
        assert_eq!(controller.status.phase, Phase::Checking);
        assert_eq!(controller.status.error, None);
        assert_eq!(controller.begin_check(), Ok(None));
        for phase in [Phase::Downloading, Phase::Verifying, Phase::Ready] {
            controller.status.phase = phase;
            assert_eq!(controller.begin_check(), Ok(None));
            assert_eq!(controller.status.phase, phase);
        }
        // An offered candidate is refreshed: a newer build may have appeared.
        for phase in [Phase::Idle, Phase::Available] {
            controller.status.phase = phase;
            assert!(controller.begin_check().unwrap().unwrap() > first);
        }
        controller.status.phase = Phase::Failed;
        controller.local_mode = true;
        assert_eq!(controller.begin_check(), Ok(None), "a dropped package owns the dialog");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unrelated_bundle_cli_does_not_block_atomic_replacement() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("Oink.app/Contents/MacOS");
        std::fs::create_dir_all(&parent).unwrap();
        let parent = parent.canonicalize().unwrap();
        let image = parent.join("oink-agent");
        let source = directory.path().join("child.c");
        std::fs::write(
            &source,
            "#include <unistd.h>\nint main(void) { write(1, \"ready\\n\", 6); sleep(10); }\n",
        )
        .unwrap();
        assert!(
            std::process::Command::new("clang")
                .arg(&source)
                .arg("-o")
                .arg(&image)
                .status()
                .unwrap()
                .success()
        );
        let mut child = Child(
            std::process::Command::new(image)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut ready = [0; 6];
        std::io::Read::read_exact(child.0.stdout.as_mut().unwrap(), &mut ready).unwrap();
        assert_eq!(&ready, b"ready\n");
        let started = Instant::now() + Duration::from_secs(1);
        loop {
            let mut path = [0u8; 4096];
            // SAFETY: query only our live child into bounded storage.
            if unsafe { libc::proc_pidpath(child.0.id() as i32, path.as_mut_ptr().cast(), path.len() as u32) } > 0 {
                break;
            }
            assert!(Instant::now() < started, "test child did not start");
            std::thread::sleep(Duration::from_millis(1));
        }
        let manager = parent.join("oink-file-provider");
        std::fs::write(&manager, "#!/bin/sh\necho '[]'\n").unwrap();
        std::fs::set_permissions(&manager, std::fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let node = runtime
            .block_on(Node::start(crate::node::NodeOptions {
                net: oink_test_support::network_config(directory.path().join("state"), "bundle-cli", 0),
                input: Arc::new(oink_test_support::input::FakeInput::new()),
                displays: vec![],
                display_backend: None,
                clipboard: None,
                mode_switch_in_callback: true,
                capture_reports_key_repeat: true,
                cursor_trail: false,
            }))
            .unwrap();
        let gate = maintenance::Gate::default();
        gate.close_for_update(0).unwrap();
        let token = Prepared {
            node: node.clone(),
            gate,
            generation: 0,
            expires: Instant::now() + PREPARATION_BUDGET,
            consumed: false,
            provider: None,
        };
        let result = close_mac_clients(&token, &parent);
        assert!(
            result.is_ok(),
            "an open CLI image is not a Unix bundle replacement blocker"
        );
        drop(result);
        drop(token);
        runtime.block_on(node.shutdown());
    }

    #[test]
    fn product_quit_reaches_ui_before_backend_shutdown_can_exit_the_process() {
        use std::io::{BufRead, Write};
        use std::sync::Arc;
        let directory = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let node = runtime
            .block_on(Node::start(crate::node::NodeOptions {
                net: oink_test_support::network_config(directory.path().into(), "quit-order", 0),
                input: Arc::new(oink_test_support::input::FakeInput::new()),
                displays: vec![],
                display_backend: None,
                clipboard: None,
                mode_switch_in_callback: true,
                capture_reports_key_repeat: true,
                cursor_trail: false,
            }))
            .unwrap();
        // Let the fake sensor publish its initial unknown session before the
        // test supplies an unlocked session; otherwise those commands can race.
        // This ordering is required in both debug and release unit tests.
        runtime.block_on(node.hold_platform_sensor_clock());
        node.input_thread()
            .unwrap()
            .send(crate::threads::input::InputCommand::SessionLocked(Some(false)));
        while node.input_status().unwrap().update_locked {
            std::thread::yield_now();
        }
        let _ui = crate::instance::acquire_ui(directory.path()).unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        std::fs::write(
            directory.path().join("ui-ipc.json"),
            serde_json::json!({
                "port":listener.local_addr().unwrap().port(), "token":"private-quit-order"
            })
            .to_string(),
        )
        .unwrap();
        let stopping = node.stop_receiver();
        let ui = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let envelope: serde_json::Value = serde_json::from_str(&line).unwrap();
            let before_shutdown = !*stopping.borrow();
            writeln!(stream, "{{\"ok\":null}}").unwrap();
            assert_eq!(envelope["stop"], true);
            assert!(before_shutdown, "backend could exit before delivering UI Quit");
        });
        let gate = maintenance::Gate::default();
        gate.close_for_update(0).unwrap();
        let token = Prepared {
            node: node.clone(),
            gate,
            generation: 0,
            expires: Instant::now() + PREPARATION_BUDGET,
            consumed: false,
            #[cfg(target_os = "macos")]
            provider: None,
        };
        token.shutdown_with_ui(true).unwrap();
        ui.join().unwrap();
        assert!(*node.stop_receiver().borrow());
        runtime.block_on(node.shutdown());
    }

    #[cfg(not(windows))]
    #[test]
    fn legacy_preferences_files_preserve_checks_and_drop_channel_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updates.json");
        for channel in ["stable", "beta"] {
            for checks_enabled in [false, true] {
                let mut policy = serde_json::to_value(Policy::default()).unwrap();
                policy["preferences"] = serde_json::json!({"checks_enabled": checks_enabled, "channel": channel});
                std::fs::write(&path, serde_json::to_vec(&policy).unwrap()).unwrap();
                let controller = Controller::load(dir.path());
                assert_eq!(controller.status.error, Some(Error::NotConfigured));
                assert_eq!(controller.policy.preferences.checks_enabled, checks_enabled);
                controller.save(dir.path()).unwrap();
                let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                assert_eq!(
                    saved["preferences"],
                    serde_json::json!({"checks_enabled": checks_enabled, "update_channel": null, "install_after_download": true})
                );
            }
        }
    }

    #[test]
    fn stored_mode_is_invalid_and_never_rewritten_as_a_migration() {
        let dir = tempfile::tempdir().unwrap();
        let mut policy = serde_json::to_value(Policy::default()).unwrap();
        policy["preferences"]["mode"] = "automatic".into();
        let bytes = serde_json::to_vec(&policy).unwrap();
        std::fs::write(dir.path().join("updates.json"), &bytes).unwrap();
        let controller = Controller::load(dir.path());
        assert_eq!(controller.status.error, Some(Error::Metadata));
        assert_eq!(std::fs::read(dir.path().join("updates.json")).unwrap(), bytes);
        assert!(!controller.status.checks_enabled);
    }

    #[test]
    fn unconfigured_status_is_disabled_even_with_enabled_checks() {
        let directory = tempfile::tempdir().unwrap();
        let mut policy = Policy::default();
        policy.preferences.checks_enabled = true;
        std::fs::write(
            directory.path().join("updates.json"),
            serde_json::to_vec(&policy).unwrap(),
        )
        .unwrap();
        let controller = Controller::load(directory.path());
        assert_eq!(controller.status.phase, Phase::Disabled);
        assert_eq!(controller.status.error, Some(Error::NotConfigured));
        assert!(!controller.status.checks_enabled);
        assert!(controller.status.candidate.is_none());
        assert!(controller.status.next_check.is_none());
    }
}
