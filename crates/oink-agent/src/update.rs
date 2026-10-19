//! Backend update owner; UI has only typed local commands and status events.
use crate::{
    api::{ApiError, ApiEvent, ApiResult},
    node::{Node, lock},
};
use oink_foundation::maintenance::{self, ActivityKind};
use oink_update::{Error, barrier::Blocker, policy::*};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(crate) struct Controller {
    #[cfg(not(windows))]
    pub policy: Policy,
    pub status: UpdateStatus,
    #[cfg(not(windows))]
    pub ready: bool,
    #[cfg(not(windows))]
    install_requested: bool,
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
            mode: policy.preferences.mode,
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
            install_requested: false,
            #[cfg(not(windows))]
            installing: false,
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
                    if state.status.error == Some(Error::NotConfigured) {
                        return Err(api_error(Error::NotConfigured));
                    }
                    state.status.phase = Phase::Checking;
                    crate::backend::on_main(native::check);
                }
                Command::Download => {
                    if state.status.candidate.is_none() {
                        return Err(api_error(Error::Policy));
                    }
                    crate::backend::on_main(native::download);
                }
                Command::Install { expected_build } => {
                    if state.status.candidate.as_ref().map(|c| c.build) != Some(expected_build) {
                        return Err(api_error(Error::Policy));
                    }
                    state.install_requested = true;
                    if !state.ready {
                        crate::backend::on_main(native::download);
                    }
                }
                Command::SetPreferences { preferences } => {
                    let previous = state.policy.clone();
                    state.policy.preferences = preferences.clone();
                    if let Err(error) = state.save(self.data_dir()) {
                        state.policy = previous;
                        return Err(api_error(error));
                    }
                    state.install_requested = false;
                    state.status.channel = preferences.channel;
                    state.status.mode = preferences.mode;
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
                    state.install_requested = false;
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
                    state.install_requested = false;
                    state.ready = false;
                    crate::backend::on_main(native::cancel);
                }
                Command::Cancel => {
                    if state.installing {
                        return Err(api_error(Error::Transaction));
                    }
                    state.install_requested = false;
                    state.ready = false;
                    crate::backend::on_main(native::cancel);
                }
            }
            let status = state.status.clone();
            drop(state);
            self.emit(ApiEvent::UpdateStatusChanged(status.clone()));
            Ok(status)
        }
    }
    /// All activity owners use the same admission gate. A failed prepare restores
    /// admission and input routing; no active transfer or audio is forcibly ended.
    pub fn prepare_for_update(&self, automatic: bool) -> Result<Prepared, Vec<Blocker>> {
        let gate = maintenance::process();
        if automatic && gate.idle_millis() < 60_000 {
            return Err(vec![Blocker::IdleInterval]);
        }
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
        let input_generation = gate.input_generation();
        let busy = gate.close(generation).map_err(|_| vec![Blocker::OwnerMissing])?;
        if !busy.is_empty() {
            gate.reopen(generation);
            return Err(busy.into_iter().map(activity_blocker).collect());
        }
        let token = Prepared {
            node: self.clone(),
            generation,
            input_generation,
            expires: Instant::now() + Duration::from_secs(25),
            consumed: false,
        };
        let input = self.input_thread().map_err(|_| vec![Blocker::OwnerMissing])?;
        let failures = input.injection_failures();
        let (reply, wait) = std::sync::mpsc::sync_channel(1);
        input.send(crate::threads::input::InputCommand::PrepareUpdate { generation, reply });
        let fence = wait
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| vec![Blocker::OwnerMissing])??;
        let deadline = Instant::now() + Duration::from_secs(3);
        while fence.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if fence.load(Ordering::Acquire) != 1 || input.injection_failures() != failures {
            return Err(vec![Blocker::HeldInput]);
        }
        token.validate()?;
        Ok(token)
    }
}
fn activity_blocker(kind: ActivityKind) -> Blocker {
    match kind {
        ActivityKind::Drag => Blocker::Drag,
        ActivityKind::Promise => Blocker::Promise,
        ActivityKind::FileProvider => Blocker::FileProvider,
        ActivityKind::FileTransfer => Blocker::ClipboardBulk,
        ActivityKind::Audio => Blocker::Audio,
        ActivityKind::Configuration => Blocker::Configuration,
    }
}
#[derive(Debug)]
pub struct Prepared {
    node: Node,
    generation: u64,
    input_generation: u64,
    expires: Instant,
    consumed: bool,
}
impl Prepared {
    pub fn validate(&self) -> Result<(), Vec<Blocker>> {
        if Instant::now() >= self.expires || maintenance::process().input_generation() != self.input_generation {
            return Err(vec![Blocker::SessionChanged]);
        }
        let input = self.node.input_status().map_err(|_| vec![Blocker::OwnerMissing])?;
        if input.update_locked || input.update_suspended {
            return Err(vec![Blocker::Locked]);
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
    #[cfg(windows)]
    fn refresh_health_input(&mut self) -> Result<(), Vec<Blocker>> {
        let input = self.node.input_status().map_err(|_| vec![Blocker::OwnerMissing])?;
        if input.update_held_input || input.update_locked || input.update_suspended {
            return Err(vec![Blocker::HeldInput]);
        }
        self.input_generation = maintenance::process().input_generation();
        Ok(())
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
            maintenance::process().reopen(self.generation);
            if let Ok(input) = self.node.input_thread() {
                input.send(crate::threads::input::InputCommand::AbortUpdate {
                    generation: self.generation,
                });
            }
        }
    }
}
#[cfg(not(windows))]
static COMMITTED_GENERATION: AtomicU64 = AtomicU64::new(0);
#[cfg(not(windows))]
static INSTALL_AUTHORIZED: AtomicBool = AtomicBool::new(false);
#[cfg(not(windows))]
static TERMINATION_WAITING: AtomicBool = AtomicBool::new(false);
#[cfg(not(windows))]
pub(crate) fn installation_termination() -> bool {
    INSTALL_AUTHORIZED.load(Ordering::Acquire)
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
            let Some(inner) = weak.upgrade() else {
                break;
            };
            let node = Node { inner };
            if QUIT_REQUESTED.load(Ordering::Acquire) {
                crate::backend::on_main(try_quit);
                continue;
            }
            let (automatic, expected_build) = {
                let state = lock(&node.inner.update);
                if !state.ready || state.installing {
                    continue;
                }
                if !state.install_requested
                    && !state
                        .status
                        .candidate
                        .as_ref()
                        .is_some_and(|c| state.policy.may_download_automatically(c.build, now()))
                {
                    continue;
                }
                (
                    !state.install_requested,
                    state.status.candidate.as_ref().map(|c| c.build),
                )
            };
            match node.prepare_for_update(automatic) {
                Ok(mut token) => {
                    {
                        let mut state = lock(&node.inner.update);
                        if !state.ready
                            || state.status.candidate.as_ref().map(|c| c.build) != expected_build
                            || (automatic
                                && !state
                                    .status
                                    .candidate
                                    .as_ref()
                                    .is_some_and(|c| state.policy.may_download_automatically(c.build, now())))
                            || (!automatic && !state.install_requested)
                        {
                            continue;
                        }
                        state.installing = true;
                        state.status.phase = Phase::Installing;
                        state.status.blockers.clear();
                    }
                    #[cfg(target_os = "macos")]
                    if close_mac_clients(&token).is_err() {
                        let mut state = lock(&node.inner.update);
                        state.installing = false;
                        state.status.phase = Phase::WaitingForIdle;
                        state.status.blockers = vec![Blocker::OwnerMissing];
                        drop(state);
                        crate::backend::on_main(native::cancel);
                        continue;
                    }
                    crate::backend::on_main(move || {
                        if token.validate().is_ok() {
                            COMMITTED_GENERATION.store(token.generation, Ordering::Release);
                            INSTALL_AUTHORIZED.store(true, Ordering::Release);
                            if native::install() && INSTALL_AUTHORIZED.load(Ordering::Acquire) {
                                // Sparkle issues applicationShouldTerminate; hold the
                                // lease through host shutdown and its completion reply.
                                token.consumed = true;
                                if let Ok(input) = token.node.input_thread() {
                                    input.send(crate::threads::input::InputCommand::CommitUpdate {
                                        generation: token.generation,
                                    });
                                }
                                return;
                            }
                            INSTALL_AUTHORIZED.store(false, Ordering::Release);
                            COMMITTED_GENERATION.store(0, Ordering::Release);
                        }
                        lock(&token.node.inner.update).installing = false;
                    });
                }
                Err(blockers) => {
                    let mut state = lock(&node.inner.update);
                    state.status.phase = Phase::WaitingForIdle;
                    state.status.blockers = blockers;
                    let status = state.status.clone();
                    drop(state);
                    node.emit(ApiEvent::UpdateStatusChanged(status));
                }
            }
        }
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
            match node.prepare_for_update(false) {
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
    node.prepare_for_update(false).map_err(|_| api_error(Error::Busy))
}

#[cfg(all(target_os = "macos", feature = "sparkle"))]
fn installation_failed() {
    INSTALL_AUTHORIZED.store(false, Ordering::Release);
    let generation = COMMITTED_GENERATION.swap(0, Ordering::AcqRel);
    if generation == 0 {
        return;
    }
    std::thread::spawn(move || {
        maintenance::process().reopen(generation);
        if let Ok(node) = crate::backend::node() {
            if let Ok(input) = node.input_thread() {
                input.send(crate::threads::input::InputCommand::AbortUpdate { generation });
            }
        }
    });
}

#[cfg(target_os = "macos")]
fn close_mac_clients(token: &Prepared) -> Result<(), Error> {
    token.validate().map_err(|_| Error::Busy)?;
    token.node.emit(ApiEvent::ExitForUpdate);
    let parent = std::env::current_exe()
        .map_err(|_| Error::Storage)?
        .parent()
        .ok_or(Error::Storage)?
        .to_owned();
    let contents = parent.parent().ok_or(Error::Storage)?;
    let images = [
        contents.join("Helpers/Oink.app/Contents/MacOS/oink"),
        parent.join("oink-agent"),
        parent.join("oink-file-provider"),
    ];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        token.validate().map_err(|_| Error::Busy)?;
        if !mac_clients_running(&images)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Busy);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
#[cfg(target_os = "macos")]
fn mac_clients_running(images: &[std::path::PathBuf]) -> Result<bool, Error> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: a null buffer requests a count only. No process is stopped/signalled.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 || count > 65536 {
        return Err(Error::Busy);
    }
    let mut pids = vec![0i32; count as usize + 1024];
    // SAFETY: storage covers exactly the supplied byte size; libproc copies PIDs.
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as i32) };
    if count < 0 || count as usize >= pids.len() {
        return Err(Error::Busy);
    }
    for pid in pids.into_iter().take(count as usize) {
        if pid == std::process::id() as i32 {
            continue;
        }
        let mut path = [0u8; 4096];
        // SAFETY: bounded kernel-reported image path query; no caller-selected PID action.
        let length = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        if length <= 0 {
            continue;
        } // Exited/unrelated inaccessible process.
        let end = path.iter().position(|b| *b == 0).unwrap_or(path.len());
        if images.iter().any(|image| image.as_os_str().as_bytes() == &path[..end]) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        #[cfg(debug_assertions)]
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
        let token = Prepared {
            node: node.clone(),
            generation: 0,
            input_generation: maintenance::process().input_generation(),
            expires: Instant::now() + Duration::from_secs(25),
            consumed: false,
        };
        token.shutdown_with_ui(true).unwrap();
        ui.join().unwrap();
        assert!(*node.stop_receiver().borrow());
        runtime.block_on(node.shutdown());
    }

    #[test]
    fn beta_status_is_disabled_even_with_automatic_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let mut policy = Policy::default();
        policy.preferences.mode = Mode::Automatic;
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
