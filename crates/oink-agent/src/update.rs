//! Backend update owner; UI has only typed local commands and status events.
use crate::{
    api::{ApiError, ApiEvent, ApiResult},
    node::{Node, lock},
};
use oink_foundation::maintenance;
use oink_update::{Error, barrier::Blocker, policy::*};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

const INPUT_PREPARATION_BUDGET: Duration = Duration::from_secs(6);
const AUDIO_PREPARATION_BUDGET: Duration = Duration::from_secs(3);
// One absolute Mac preparation deadline: 3s input reply + 3s fence + 3s
// current audio cleanup + two 8s provider commands. Each step uses its remainder;
// the approval itself is retained until Sparkle finishes or dismisses it.
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
    #[cfg(not(windows))]
    pub ready: bool,
    #[cfg(not(windows))]
    installing: bool,
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
        let status = UpdateStatus {
            current_build: option_env!("OINK_RELEASE_BUILD")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            version: oink_update::RELEASE_VERSION.into(),
            channel: policy.preferences.channel,
            checks_enabled: false,
            phase: Phase::Disabled,
            candidate: None,
            checked_at: None,
            next_check: None,
            deferred_until: policy.deferred_until,
            blockers: vec![],
            progress: None,
            error: error.or(Some(Error::NotConfigured)),
            scope: if cfg!(windows) { Scope::Machine } else { Scope::User },
            can_change_policy: !cfg!(windows),
        };
        Self {
            #[cfg(not(windows))]
            policy,
            status,
            #[cfg(not(windows))]
            ready: false,
            #[cfg(not(windows))]
            installing: false,
        }
    }
    #[cfg(not(windows))]
    fn begin_check(&mut self) -> Result<bool, Error> {
        if self.status.error == Some(Error::NotConfigured) {
            return Err(Error::NotConfigured);
        }
        if matches!(
            self.status.phase,
            Phase::Checking | Phase::Available | Phase::Downloading | Phase::Verifying | Phase::Ready
        ) {
            return Ok(false);
        }
        self.status.phase = Phase::Checking;
        self.status.error = None;
        Ok(true)
    }
    #[cfg(not(windows))]
    fn begin_install(&mut self, expected_build: u64) -> Result<(), Error> {
        if self.installing {
            return Err(Error::Transaction);
        }
        if !self.ready
            || self.status.phase != Phase::Ready
            || self.status.candidate.as_ref().map(|c| c.build) != Some(expected_build)
        {
            return Err(Error::Policy);
        }
        self.installing = true;
        self.status.phase = Phase::Quiescing;
        self.status.error = None;
        self.status.blockers.clear();
        Ok(())
    }
    #[cfg(all(not(windows), any(feature = "sparkle", test)))]
    fn dismissed(&mut self) {
        self.ready = false;
        if self.installing {
            self.installing = false;
            if self.status.phase != Phase::Committed {
                self.status.phase = Phase::Failed;
                self.status.error = Some(Error::Transaction);
            }
        } else if self.status.phase != Phase::Failed {
            self.status.phase = Phase::Idle;
        }
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
    ApiError::Unavailable {
        message: error.to_string(),
    }
}
#[cfg(not(windows))]
fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
impl Node {
    pub fn query_update_status(&self) -> UpdateStatus {
        #[cfg(windows)]
        if let Ok(status) = windows::status() {
            return status;
        }
        lock(&self.inner.update).status.clone()
    }
    pub fn execute_update_command(&self, command: Command) -> ApiResult<UpdateStatus> {
        #[cfg(windows)]
        {
            windows::command(command)
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
                Command::Check => {
                    if state.begin_check().map_err(api_error)? {
                        crate::backend::on_main(native::check);
                    }
                }
                Command::Download => {
                    if state.status.candidate.is_none() {
                        return Err(api_error(Error::Policy));
                    }
                    crate::backend::on_main(native::download);
                }
                Command::Install { expected_build } => {
                    state.begin_install(expected_build).map_err(api_error)?;
                    #[cfg(target_os = "macos")]
                    native::install_clicked();
                }
                Command::SetPreferences { preferences } => {
                    let previous = state.policy.clone();
                    state.policy.preferences = preferences.clone();
                    if let Err(error) = state.save(self.data_dir()) {
                        state.policy = previous;
                        return Err(api_error(error));
                    }
                    state.status.channel = preferences.channel;
                    state.status.checks_enabled = preferences.checks_enabled;
                    state.status.candidate = None;
                    state.ready = false;
                    crate::backend::on_main(move || native::preferences(preferences));
                }
                Command::Defer { choice } => {
                    let previous = state.policy.clone();
                    state.policy.defer(choice, now());
                    if let Err(error) = state.save(self.data_dir()) {
                        state.policy = previous;
                        return Err(api_error(error));
                    }
                    state.status.deferred_until = state.policy.deferred_until;
                    state.ready = false;
                    crate::backend::on_main(native::cancel);
                }
                Command::Skip { expected_build } => {
                    if state.status.candidate.as_ref().map(|c| c.build) != Some(expected_build) {
                        return Err(api_error(Error::Policy));
                    }
                    let previous = state.policy.clone();
                    state.policy.skipped_build = Some(expected_build);
                    if let Err(error) = state.save(self.data_dir()) {
                        state.policy = previous;
                        return Err(api_error(error));
                    }
                    state.ready = false;
                    crate::backend::on_main(native::cancel);
                }
                Command::Cancel => {
                    if state.installing {
                        return Err(api_error(Error::Transaction));
                    }
                    state.ready = false;
                    crate::backend::on_main(native::cancel);
                }
            }
            let status = state.status.clone();
            drop(state);
            self.emit(ApiEvent::UpdateStatusChanged(status.clone()));
            if status.phase == Phase::Quiescing {
                let node = self.clone();
                std::thread::spawn(move || install_candidate(node));
            }
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
// The live Prepared value is the sole approval. It retains the input fence,
// closed admission and provider suspension until replacement or an abort.
#[cfg(not(windows))]
static APPROVED: std::sync::Mutex<Option<Prepared>> = std::sync::Mutex::new(None);
#[cfg(target_os = "macos")]
mod mac_provider;
#[cfg(not(windows))]
static TERMINATION_WAITING: AtomicBool = AtomicBool::new(false);
#[cfg(not(windows))]
pub(crate) fn installation_termination() -> bool {
    APPROVED.lock().unwrap().is_some()
}
#[cfg(not(windows))]
pub(crate) fn termination_waiting() -> bool {
    TERMINATION_WAITING.swap(false, Ordering::AcqRel)
}
#[cfg(not(windows))]
pub(crate) fn request_native_termination() {
    TERMINATION_WAITING.store(true, Ordering::Release);
    if let Ok(node) = crate::backend::node() {
        node.request_stop();
    }
}
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
    let weak = std::sync::Arc::downgrade(&node.inner);
    let preferences = lock(&node.inner.update).policy.preferences.clone();
    crate::backend::on_main(move || native::start(preferences));
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let Some(_inner) = weak.upgrade() else {
                break;
            };
            if QUIT_REQUESTED.load(Ordering::Acquire) {
                crate::backend::on_main(try_quit);
                continue;
            }
        }
    });
}

#[cfg(not(windows))]
fn install_failure(node: &Node, error: Error, blockers: Vec<Blocker>) {
    let mut state = lock(&node.inner.update);
    state.installing = false;
    state.status.phase = Phase::Failed;
    state.status.error = Some(error);
    state.status.blockers = blockers;
    let status = state.status.clone();
    drop(state);
    node.emit(ApiEvent::UpdateStatusChanged(status));
}
#[cfg(not(windows))]
fn install_candidate(node: Node) {
    let mut token = match node.prepare_for_update() {
        Ok(token) => token,
        Err(blockers) => {
            install_failure(&node, Error::Busy, blockers);
            return;
        }
    };
    #[cfg(target_os = "macos")]
    let parent = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_owned))
    {
        Some(parent) => parent,
        None => {
            install_failure(&node, Error::Storage, vec![Blocker::IntegrationUnavailable]);
            return;
        }
    };
    #[cfg(target_os = "macos")]
    let provider = match close_mac_clients(&token, &parent) {
        Ok(provider) => provider,
        Err(error) => {
            install_failure(&node, error, vec![Blocker::IntegrationUnavailable]);
            return;
        }
    };
    #[cfg(target_os = "macos")]
    {
        token.provider = Some(provider);
        native::quiesce_done();
    }
    crate::backend::on_main(move || {
        let node = token.node.clone();
        if token.validate().is_ok() {
            *APPROVED.lock().unwrap() = Some(token);
            if native::install() {
                if let Some(token) = APPROVED.lock().unwrap().as_ref()
                    && let Ok(input) = token.node.input_thread()
                {
                    input.send(crate::threads::input::InputCommand::CommitUpdate {
                        generation: token.generation,
                    });
                }
                return;
            }
            installation_failed();
        }
        install_failure(&node, Error::Busy, vec![Blocker::IntegrationUnavailable]);
    });
}

#[cfg(all(target_os = "macos", feature = "sparkle"))]
mod native;
#[cfg(not(all(target_os = "macos", feature = "sparkle")))]
mod native {
    #[cfg(not(windows))]
    use super::Preferences;
    #[cfg(not(windows))]
    pub fn start(_: Preferences) {}
    #[cfg(not(windows))]
    pub fn check() {}
    #[cfg(not(windows))]
    pub fn download() {}
    pub fn cancel() {}
    #[cfg(not(windows))]
    pub fn install() -> bool {
        false
    }
    #[cfg(target_os = "macos")]
    pub fn install_clicked() {}
    #[cfg(target_os = "macos")]
    pub fn quiesce_done() {}
    #[cfg(not(windows))]
    pub fn preferences(_: Preferences) {}
    pub fn quit_allowed() -> bool {
        true
    }
}
pub(crate) fn cancel_before_quit() -> bool {
    if native::quit_allowed() {
        true
    } else {
        native::cancel();
        false
    }
}

static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
pub(crate) fn request_quit() {
    QUIT_REQUESTED.store(true, Ordering::Release);
    crate::backend::on_main(try_quit);
}
fn try_quit() {
    if !QUIT_REQUESTED.load(Ordering::Acquire) || !cancel_before_quit() {
        return;
    }
    if !QUIT_REQUESTED.swap(false, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(|| {
        if let Ok(node) = crate::backend::node() {
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
        }
    });
}
/// Reopen is initiated off the AppKit thread; cancellation must be acknowledged
/// before another process is launched or the current host is shut down.
#[cfg(not(windows))]
pub(crate) fn prepare_reopen(node: &Node) -> ApiResult<Prepared> {
    let (reply, wait) = std::sync::mpsc::sync_channel(1);
    crate::backend::on_main(move || {
        let _ = reply.send(cancel_before_quit());
    });
    if wait.recv_timeout(Duration::from_secs(3)) != Ok(true) {
        return Err(api_error(Error::Busy));
    }
    node.prepare_for_update().map_err(|_| api_error(Error::Busy))
}

#[cfg(not(windows))]
fn installation_failed() {
    if let Some(token) = APPROVED.lock().unwrap().take() {
        // Drop may run the bounded provider reconnect. Keep it off AppKit;
        // Prepared reopens admission and aborts the input fence afterwards.
        std::thread::spawn(move || drop(token));
    }
}

#[cfg(target_os = "macos")]
fn close_mac_clients(token: &Prepared, parent: &Path) -> Result<mac_provider::Suspension, Error> {
    token.validate().map_err(|_| Error::Busy)?;
    // Unix mapped executables do not lock their containing app directory.
    // Sparkle 2.9.6 SUPlainInstaller swaps/renames the bundle and waits for its
    // target host itself. Other users, CLI commands and manager invocations are
    // not replacement blockers. Close our UI by its existing event contract.
    let provider = mac_provider::Suspension::prepare(parent.join("oink-file-provider"), token.expires, &|| {
        token.validate().is_ok()
    })?;
    token.node.emit(ApiEvent::ExitForUpdate);
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn manual_check_clears_old_failure_and_joins_the_existing_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = Controller::load(dir.path());
        controller.status.phase = Phase::Failed;
        controller.status.error = Some(Error::Signature);
        assert_eq!(controller.begin_check(), Ok(true));
        assert_eq!(controller.status.phase, Phase::Checking);
        assert_eq!(controller.status.error, None);
        assert_eq!(controller.begin_check(), Ok(false));
        for phase in [Phase::Available, Phase::Downloading, Phase::Verifying, Phase::Ready] {
            controller.status.phase = phase;
            assert_eq!(controller.begin_check(), Ok(false));
            assert_eq!(controller.status.phase, phase);
        }
        controller.status.phase = Phase::Idle;
        assert_eq!(controller.begin_check(), Ok(true));
    }

    #[cfg(not(windows))]
    #[test]
    fn silent_native_dismissal_after_approval_ends_the_install_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = Controller::load(dir.path());
        controller.installing = true;
        controller.ready = true;
        controller.status.phase = Phase::Installing;
        controller.dismissed();
        assert!(
            !controller.installing,
            "dismissal must release the attempt even without an error callback"
        );
        assert!(!controller.ready);
        assert_eq!(controller.status.phase, Phase::Failed);
        assert_eq!(controller.status.error, Some(Error::Transaction));
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
        let ready = Instant::now() + Duration::from_secs(1);
        while node.input_status().unwrap().update_locked {
            assert!(
                Instant::now() < ready,
                "fake input did not publish its unlocked session"
            );
            std::thread::sleep(Duration::from_millis(1));
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
    fn explicit_ready_install_starts_once_without_activity_or_idle_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = Controller::load(dir.path());
        controller.ready = true;
        controller.status.phase = Phase::Ready;
        controller.status.error = None;
        controller.status.candidate = Some(Candidate {
            build: 31,
            version: "0.1.1".into(),
            size: 1,
            published_at: None,
            severity: oink_update::feed::Severity::Normal,
            notes: serde_json::from_value(serde_json::json!({"en":"Fixes","zh-Hans":"修复"})).unwrap(),
        });
        controller.status.blockers = vec![Blocker::Audio, Blocker::RemoteInput, Blocker::Locked];
        assert_eq!(controller.begin_install(32), Err(Error::Policy));
        controller.begin_install(31).unwrap();
        assert_eq!(controller.status.phase, Phase::Quiescing);
        assert!(controller.status.blockers.is_empty());
        assert_eq!(controller.begin_install(31), Err(Error::Transaction));
        assert!(serde_json::to_value(&controller.status).unwrap().get("mode").is_none());
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
    fn beta_status_is_disabled_even_with_enabled_checks() {
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
