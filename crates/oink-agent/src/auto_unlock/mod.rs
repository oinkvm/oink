//! Local credentials never participate in owner options, group config or wire DTOs.
use oink_foundation::DeviceId;
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};
use zeroize::Zeroize;
pub mod sync;
pub mod typing;

#[cfg(target_os = "macos")]
mod macos;

/// Only the authenticated local API serializes this value. Debug is deliberately
/// redacted because Requests are also carried by diagnostic/error wrappers.
#[derive(Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Password(String);
impl Password {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}
impl Drop for Password {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
impl<'de> Deserialize<'de> for Password {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Never let serde echo a rejected value into an IPC error or crash report.
        String::deserialize(d)
            .map(Self)
            .map_err(|_| serde::de::Error::custom("invalid password value"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    #[serde(rename = "password_incorrect")]
    IncorrectPassword,
    WindowsHelloOnly,
    LoginUiNotReady,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub enabled: bool,
    pub failure: Option<FailureKind>,
    pub windows_hello_only: bool,
    /// Latest skipped sync unlock request; secondary text, never an alert.
    #[serde(default)]
    pub sync_last: Option<sync::SyncLast>,
}
static OPERATION: Mutex<()> = Mutex::new(());
const FAILURE_FILE: &str = "auto-unlock-failure.json";

fn error(message: &str) -> crate::api::ApiError {
    crate::api::ApiError::Unavailable {
        message: message.into(),
    }
}
pub(crate) fn status(dir: &Path) -> crate::api::ApiResult<Status> {
    let _guard = OPERATION.lock().map_err(|_| error("自动解锁不可用"))?;
    let enabled = platform_status().map_err(error)?;
    let failure = platform_failure().or_else(|| {
        if platform_clears_failure() {
            let _ = clear_failure(dir);
            None
        } else {
            read_failure(dir)
        }
    });
    let windows_hello_only = platform_windows_hello_only().map_err(error)?;
    Ok(Status {
        enabled,
        failure,
        windows_hello_only,
        sync_last: enabled.then(|| sync::read_last(dir)).flatten(),
    })
}
pub(crate) fn save(dir: &Path, password: Password) -> crate::api::ApiResult<()> {
    let _guard = OPERATION.lock().map_err(|_| error("自动解锁不可用"))?;
    if password.expose().is_empty() || password.expose().len() > 4096 || password.expose().contains('\0') {
        return Err(crate::api::ApiError::InvalidArgument {
            message: "请输入有效的登录密码".into(),
        });
    }
    platform_save(password.expose()).map_err(error)?;
    sync::clear_last(dir);
    clear_failure(dir).map_err(error)
}
pub(crate) fn disable(dir: &Path) -> crate::api::ApiResult<()> {
    let _guard = OPERATION.lock().map_err(|_| error("自动解锁不可用"))?;
    platform_disable().map_err(error)?;
    sync::clear_last(dir);
    clear_failure(dir).map_err(error)
}
fn clear_failure(dir: &Path) -> Result<(), &'static str> {
    match std::fs::remove_file(dir.join(FAILURE_FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("无法清除上次自动解锁状态"),
    }
}
fn persist_failure(dir: &Path, failure: FailureKind) -> Result<(), &'static str> {
    oink_foundation::storage::atomic_write(
        &dir.join(FAILURE_FILE),
        &serde_json::to_vec(&failure).map_err(|_| "无法保存上次自动解锁状态")?,
    )
    .map_err(|_| "无法保存上次自动解锁状态")
}
fn read_failure(dir: &Path) -> Option<FailureKind> {
    std::fs::read(dir.join(FAILURE_FILE))
        .ok()
        .and_then(|v| serde_json::from_slice(&v).ok())
}
#[cfg(windows)]
fn save_error(error: &std::io::Error) -> &'static str {
    if error.raw_os_error() == Some(1326) {
        "密码不正确"
    } else {
        "无法保存登录密码"
    }
}
pub(crate) fn source_unlocked(profile: crate::node::Profile, locked: Option<bool>) -> bool {
    profile == crate::node::Profile::User && locked == Some(false)
}

pub(crate) fn remote_input(source: DeviceId, age_ms: u32) {
    #[cfg(windows)]
    {
        let _ = oink_winsvc::auto_unlock_remote_input(source_tag(source), age_ms);
    }
    #[cfg(not(windows))]
    let _ = (source, age_ms);
}
pub(crate) fn attempt(node: &crate::node::Node, epoch: u64, source: DeviceId, initial_input_age_ms: Option<u32>) {
    if node.profile() != crate::node::Profile::User {
        return;
    }
    record_flow_diagnostic(8, epoch, 0);
    let dir = node.data_dir();
    let backend = node.input_backend();
    // No waiting/replaying: a simultaneous save, disable or attempt consumes this
    // entry without scheduling another authentication attempt.
    let Ok(_guard) = OPERATION.try_lock() else {
        record_flow_diagnostic(9, epoch, 1);
        return;
    };
    if platform_status() != Ok(true) || backend.session_locked() != Some(true) {
        record_flow_diagnostic(9, epoch, 2);
        return;
    }
    if let Err(failure) = platform_attempt(node, epoch, source, initial_input_age_ms) {
        record_flow_diagnostic(9, epoch, 3);
        // Only static reason text is persisted. No OS error, credential, input
        // event or serialization buffer is passed to diagnostics or telemetry.
        let _ = persist_failure(dir, failure);
    } else {
        record_flow_diagnostic(9, epoch, 0);
        // Windows keeps the durable service result authoritative until its
        // asynchronous provider outcome is reported. The macOS attempt is
        // synchronous, so a successful return clears its local notice here.
        #[cfg(not(windows))]
        let _ = clear_failure(dir);
    }
}
#[cfg(windows)]
fn platform_failure() -> Option<FailureKind> {
    use oink_winsvc::{AutoUnlockFailure, AutoUnlockResult};
    let status = oink_winsvc::auto_unlock_status().ok()?;
    match status.last_result {
        AutoUnlockResult::Failed | AutoUnlockResult::Expired => match status.failure_kind {
            AutoUnlockFailure::IncorrectPassword => Some(FailureKind::IncorrectPassword),
            AutoUnlockFailure::WindowsHelloOnly => Some(FailureKind::WindowsHelloOnly),
            // Older 105 service files may contain the removed curtain timeout.
            // Do not surface that obsolete reason in the Windows settings page.
            AutoUnlockFailure::LoginUiNotReady => None,
            AutoUnlockFailure::Unknown | AutoUnlockFailure::None => Some(FailureKind::Unknown),
        },
        _ => None,
    }
}
#[cfg(not(windows))]
fn platform_failure() -> Option<FailureKind> {
    None
}
#[cfg(windows)]
fn platform_clears_failure() -> bool {
    use oink_winsvc::AutoUnlockResult;
    matches!(
        oink_winsvc::auto_unlock_status().ok().map(|s| s.last_result),
        Some(AutoUnlockResult::Idle | AutoUnlockResult::Succeeded)
    )
}
#[cfg(not(windows))]
fn platform_clears_failure() -> bool {
    false
}
#[cfg(target_os = "macos")]
fn platform_status() -> Result<bool, &'static str> {
    macos::status()
}
#[cfg(target_os = "macos")]
fn platform_save(password: &str) -> Result<(), &'static str> {
    macos::save(password)
}
#[cfg(target_os = "macos")]
fn platform_disable() -> Result<(), &'static str> {
    macos::disable()
}
#[cfg(target_os = "macos")]
fn platform_attempt(
    node: &crate::node::Node,
    epoch: u64,
    _source: DeviceId,
    _initial_input_age_ms: Option<u32>,
) -> Result<(), FailureKind> {
    macos::attempt(node.input_backend().clone(), |job| {
        let input = node.input_thread().map_err(|_| "输入线程不可用")?;
        input.send(crate::threads::input::InputCommand::LocalUnlock {
            admission: crate::threads::input::LocalUnlockAdmission::Focus(epoch),
            job,
        });
        Ok(())
    })
    .map_err(macos_failure)
}
#[cfg(target_os = "macos")]
fn macos_failure(reason: &str) -> FailureKind {
    match reason {
        macos::PASSWORD_INCORRECT => FailureKind::IncorrectPassword,
        macos::DISPLAY_ASLEEP | macos::NOT_READY | "自动输入未能及时完成，已取消" | macos::LOCK_CHANGED => {
            FailureKind::LoginUiNotReady
        }
        _ => FailureKind::Unknown,
    }
}
#[cfg(windows)]
fn platform_status() -> Result<bool, &'static str> {
    let status = oink_winsvc::auto_unlock_status().map_err(|_| "无法读取自动解锁状态")?;
    record_provider_readiness_diagnostic(&status);
    Ok(status.enabled)
}
#[cfg(windows)]
fn platform_save(password: &str) -> Result<(), &'static str> {
    oink_winsvc::auto_unlock_save(password).map_err(|error| save_error(&error))
}
#[cfg(windows)]
fn platform_disable() -> Result<(), &'static str> {
    oink_winsvc::auto_unlock_disable().map_err(|_| "无法关闭自动解锁")
}
#[cfg(windows)]
fn platform_attempt(
    _node: &crate::node::Node,
    _epoch: u64,
    source: DeviceId,
    initial_input_age_ms: Option<u32>,
) -> Result<(), FailureKind> {
    let source = source_tag(source);
    if let Some(age_ms) = initial_input_age_ms {
        oink_winsvc::auto_unlock_remote_input(source, age_ms).map_err(|_| FailureKind::Unknown)?;
    }
    match oink_winsvc::auto_unlock_attempt(source) {
        Ok(()) => Ok(()),
        Err(_) => Err(platform_failure().unwrap_or(FailureKind::Unknown)),
    }
}
#[cfg(not(any(windows, target_os = "macos")))]
fn platform_status() -> Result<bool, &'static str> {
    Ok(false)
}
#[cfg(windows)]
fn platform_windows_hello_only() -> Result<bool, &'static str> {
    oink_winsvc::auto_unlock_status()
        .map(|s| s.windows_hello_only)
        .map_err(|_| "无法读取 Windows Hello 登录设置")
}
#[cfg(not(windows))]
fn platform_windows_hello_only() -> Result<bool, &'static str> {
    Ok(false)
}
#[cfg(not(any(windows, target_os = "macos")))]
fn platform_save(_: &str) -> Result<(), &'static str> {
    Err("此系统不支持自动解锁")
}
#[cfg(not(any(windows, target_os = "macos")))]
fn platform_disable() -> Result<(), &'static str> {
    Ok(())
}
#[cfg(not(any(windows, target_os = "macos")))]
fn platform_attempt(_: &crate::node::Node, _: u64, _: DeviceId, _: Option<u32>) -> Result<(), FailureKind> {
    Err(FailureKind::Unknown)
}

/// The failure latch that the settings row shows. Windows keeps it in the
/// service; macOS in the agent data directory.
fn latched_failure(dir: &Path) -> Option<FailureKind> {
    platform_failure().or_else(|| if cfg!(windows) { None } else { read_failure(dir) })
}

/// Initiator side of sync unlock, run on an ordinary thread for each user
/// unlock edge (confirmed lock, then confirmed unlock) seen by the observer.
/// Pause is enforced by the input thread, which owns I0 and never sends while
/// this device is paused.
pub(crate) fn local_unlock_edge(node: &crate::node::Node) {
    let epoch = sync::next_edge_epoch();
    if node.profile() != crate::node::Profile::User || !platform_console_session() {
        sync::record_diagnostic(1, epoch, 3);
        return;
    }
    if !node.settings().sync_unlock_others {
        sync::record_diagnostic(1, epoch, 2);
        return;
    }
    // Oink's own unlock (pointer entry or sync) never fans out: no cascade.
    if platform_own_unlock() {
        sync::record_diagnostic(1, epoch, 1);
        return;
    }
    sync::record_diagnostic(1, epoch, 0);
    if let Ok(input) = node.input_thread() {
        input.send(crate::threads::input::InputCommand::SyncUnlockBroadcast { epoch });
    }
}

/// Target side of sync unlock: one transaction per admitted request. Local
/// prechecks that need no waiting come first, so a device that is unlocked,
/// paused or not set up is never woken. Every skip is silent; only the reasons
/// listed in `sync::SyncSkip` are recorded.
pub(crate) fn sync_attempt(node: &crate::node::Node, source: DeviceId, epoch: u64) {
    use sync::{SourceAnswer, SyncSkip};
    sync::record_diagnostic(3, epoch, 0);
    if node.profile() != crate::node::Profile::User {
        sync::record_diagnostic(4, epoch, 1);
        return;
    }
    // No waiting/replaying: a concurrent save, disable or attempt wins.
    let Ok(guard) = OPERATION.try_lock() else {
        sync::record_diagnostic(4, epoch, 2);
        return;
    };
    let dir = node.data_dir();
    let backend = node.input_backend().clone();
    let precheck = if !node.input_status().is_ok_and(|s| !s.paused) {
        3
    } else if platform_status() != Ok(true) {
        4
    } else if platform_windows_hello_only() != Ok(false) {
        5
    } else if latched_failure(dir) == Some(FailureKind::IncorrectPassword) {
        // User decision Q5: after a wrong password, unattended sync attempts
        // stop until the password is changed (Save clears the latch).
        6
    } else if backend.session_locked() != Some(true) || !platform_sync_target() {
        7
    } else {
        0
    };
    sync::record_diagnostic(4, epoch, precheck);
    if precheck != 0 {
        return;
    }
    let Ok(input) = node.input_thread() else {
        return;
    };
    // Challenge the source and wake the display in parallel.
    let (reply, answer) = std::sync::mpsc::sync_channel(1);
    let asked = Instant::now();
    input.send(crate::threads::input::InputCommand::SyncUnlockQuery { source, reply });
    let woke = platform_wake(backend.as_ref());
    sync::record_diagnostic(5, epoch, u64::from(woke.is_err()));
    let answer = match answer.recv_timeout(sync::SOURCE_REPLY.saturating_sub(asked.elapsed())) {
        Ok(answer) => answer,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => SourceAnswer::Locked,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => SourceAnswer::Cancelled,
    };
    sync::record_diagnostic(6, epoch, answer.diagnostic_code());
    let skip = match (woke, answer) {
        (_, SourceAnswer::Cancelled) => return,
        (Err(()), _) => SyncSkip::DisplayWake,
        (Ok(_), SourceAnswer::Locked) => SyncSkip::SourceLocked,
        (Ok(_), SourceAnswer::Disconnected) => SyncSkip::SourceDisconnected,
        (Ok(woke), SourceAnswer::Unlocked) => {
            platform_sync_attempt(node, source, epoch, woke, guard);
            return;
        }
    };
    sync::persist_skip(dir, skip);
}

#[cfg(target_os = "macos")]
fn platform_sync_attempt(
    node: &crate::node::Node,
    source: DeviceId,
    epoch: u64,
    woke: Instant,
    _guard: std::sync::MutexGuard<'_, ()>,
) {
    use sync::SyncSkip;
    let dir = node.data_dir();
    let result = macos::sync_attempt(node.input_backend().clone(), woke, |job| {
        let input = node.input_thread().map_err(|_| "输入线程不可用")?;
        input.send(crate::threads::input::InputCommand::LocalUnlock {
            admission: crate::threads::input::LocalUnlockAdmission::Sync(source),
            job,
        });
        Ok(())
    });
    match result {
        Ok(()) => {
            sync::record_diagnostic(7, epoch, 0);
            sync::record_diagnostic(8, epoch, 0);
            sync::clear_last(dir);
            let _ = clear_failure(dir);
        }
        Err(macos::DISPLAY_ASLEEP) => {
            sync::record_diagnostic(7, epoch, 2);
            sync::persist_skip(dir, SyncSkip::DisplayWake);
        }
        Err(macos::NOT_READY) => {
            sync::record_diagnostic(7, epoch, 1);
            sync::persist_skip(dir, SyncSkip::PasswordField);
        }
        Err(macos::LOCAL_TYPING) => {
            sync::record_diagnostic(8, epoch, 2);
            sync::persist_skip(dir, SyncSkip::LocalTyping);
        }
        // The user unlocked meanwhile, or pause/source loss revoked the
        // transaction before its first character: nothing to report.
        Err(reason) if macos::stopped_by_state_change(reason) => sync::record_diagnostic(8, epoch, 3),
        Err(reason) => {
            sync::record_diagnostic(8, epoch, 1);
            let _ = persist_failure(dir, macos_failure(reason));
        }
    }
}
#[cfg(windows)]
fn platform_sync_attempt(
    node: &crate::node::Node,
    source: DeviceId,
    epoch: u64,
    _woke: Instant,
    guard: std::sync::MutexGuard<'_, ()>,
) {
    use oink_winsvc::AutoUnlockSync;
    let dir = node.data_dir();
    let Ok(operation) = oink_winsvc::auto_unlock_sync_attempt(source_tag(source)) else {
        // Another operation is pending, the session is not the signed-in
        // console user's, or the service is unavailable: a silent skip.
        sync::record_diagnostic(8, epoch, 4);
        return;
    };
    drop(guard);
    // One check after the service's sync window, not a poll: the service
    // expires the operation on the first request after its 3 s window.
    std::thread::sleep(oink_winsvc::AUTO_UNLOCK_SYNC_WINDOW + Duration::from_millis(300));
    let Ok(status) = oink_winsvc::auto_unlock_status() else {
        return;
    };
    record_provider_readiness_diagnostic(&status);
    if status.sync_id != operation {
        return;
    }
    match status.sync_outcome {
        AutoUnlockSync::Expired => {
            // LogonUI did not serialize: the LockApp curtain was still up
            // (prepared=false additionally means the provider never polled).
            sync::record_diagnostic(7, epoch, if status.sync_prepared { 1 } else { 2 });
            sync::persist_skip(dir, sync::SyncSkip::LockScreen);
        }
        AutoUnlockSync::Submitted | AutoUnlockSync::Succeeded => {
            sync::record_diagnostic(7, epoch, 0);
            sync::record_diagnostic(8, epoch, 0);
            sync::clear_last(dir);
        }
        // The service latched the failure (for example a wrong password).
        AutoUnlockSync::Failed => sync::record_diagnostic(8, epoch, 1),
        // The user unlocked locally, which resets the pending operation.
        AutoUnlockSync::None | AutoUnlockSync::Pending => sync::record_diagnostic(8, epoch, 3),
    }
}
#[cfg(not(any(windows, target_os = "macos")))]
fn platform_sync_attempt(_: &crate::node::Node, _: DeviceId, _: u64, _: Instant, _: std::sync::MutexGuard<'_, ()>) {}

#[cfg(target_os = "macos")]
fn platform_wake(backend: &dyn oink_input::InputBackend) -> Result<Instant, ()> {
    // IOPMAssertionDeclareUserActivity lights a sleeping display (measured on
    // the 14.7 lock screen, including from display sleep).
    backend.declare_user_activity().map(|_| Instant::now()).map_err(|_| ())
}
#[cfg(windows)]
fn platform_wake(backend: &dyn oink_input::InputBackend) -> Result<Instant, ()> {
    let mut monitor = backend.display_power_observation().ok();
    // The subscription reports the current console display state on creation.
    let mut asleep = monitor.as_mut().and_then(|m| m.poll());
    backend.declare_user_activity().map_err(|_| ())?;
    let mut wait_awake = |limit: Duration, asleep: &mut Option<bool>| {
        let end = Instant::now() + limit;
        while *asleep == Some(true) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
            if let Some(state) = monitor.as_mut().and_then(|m| m.poll()) {
                *asleep = Some(state);
            }
        }
    };
    wait_awake(Duration::from_millis(500), &mut asleep);
    if asleep == Some(true) {
        // SetThreadExecutionState(ES_DISPLAY_REQUIRED) only resets the display
        // idle timer; Microsoft does not document it turning an off display
        // back on, and here it observably did not. User decision Q4 approves
        // exactly this fallback: one Oink-tagged zero-displacement mouse move
        // (no click, no key, the pointer does not move) solely to wake the
        // display. It is never used to dismiss the LockApp curtain.
        backend.reset_user_idle().map_err(|_| ())?;
        wait_awake(Duration::from_secs(1), &mut asleep);
    }
    if asleep == Some(true) {
        Err(())
    } else {
        Ok(Instant::now())
    }
}
#[cfg(not(any(windows, target_os = "macos")))]
fn platform_wake(_: &dyn oink_input::InputBackend) -> Result<Instant, ()> {
    Err(())
}

#[cfg(target_os = "macos")]
fn platform_sync_target() -> bool {
    // Locked, on console and login done for this backend's own user session.
    macos::console_locked()
}
#[cfg(not(target_os = "macos"))]
fn platform_sync_target() -> bool {
    // Windows: the service binds every operation to the signed-in console
    // user's token and rejects any other session itself.
    cfg!(windows)
}

#[cfg(windows)]
fn platform_console_session() -> bool {
    use windows::Win32::System::{
        RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId},
        Threading::GetCurrentProcessId,
    };
    let mut own = 0;
    // SAFETY: read-only session queries with a valid output slot.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut own).is_ok() && WTSGetActiveConsoleSessionId() == own }
}
#[cfg(not(windows))]
fn platform_console_session() -> bool {
    // macOS lock readings are already unknown for an off-console session.
    cfg!(target_os = "macos")
}

#[cfg(windows)]
fn platform_own_unlock() -> bool {
    // The service stamps provider success (ReportResult) 12–24 ms before WTS
    // UNLOCK; the observer sees the edge later still.
    oink_winsvc::auto_unlock_status().is_ok_and(|s| {
        s.last_result == oink_winsvc::AutoUnlockResult::Succeeded
            && s.succeeded_at_ms != 0
            && s.now_ms.saturating_sub(s.succeeded_at_ms) <= sync::OWN_UNLOCK.as_millis() as u64
    })
}
#[cfg(not(windows))]
fn platform_own_unlock() -> bool {
    sync::own_unlock_recent(Instant::now())
}

/// One challenge per accepted pointer entry; a response is bound to the actual
/// original pointer source, not a previous holder's claim about that source.
#[derive(Default)]
pub(crate) struct EntryGate {
    locked: bool,
    armed: bool,
    last_epoch: Option<u64>,
    pending: Option<(DeviceId, u64, Instant)>,
    authorized: Option<(DeviceId, u64, Instant)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryRejection {
    Locked,
    Armed,
    Stale,
    Source,
    NotAttached,
}

impl EntryRejection {
    /// Stable numeric diagnostic code. These values are local evidence only;
    /// they never cross the input or credential paths.
    pub(crate) const fn diagnostic_code(self) -> u64 {
        match self {
            Self::Locked => 1,
            Self::Armed => 2,
            Self::Stale => 3,
            Self::Source => 4,
            Self::NotAttached => 5,
        }
    }
}

fn session_lock_code(state: Option<bool>) -> i64 {
    match state {
        Some(true) => 1,
        Some(false) => 0,
        None => -1,
    }
}

/// Numeric-only diagnostics for the lock gate. The diagnostics layer admits
/// these reviewed fields under its existing privacy boundary; no input or
/// credential data is passed here.
pub(crate) fn record_session_lock_diagnostic_timed(
    observation: oink_input::SessionLockObservation,
    source: u64,
    duration: Option<Duration>,
) {
    tracing::info!(
        target: "oink_auto_unlock_diagnostics",
        auto_unlock_event = 1u64,
        auto_unlock_session_locked = session_lock_code(observation.state),
        auto_unlock_observation_source = source,
        auto_unlock_wts_error_code = observation.error_code.unwrap_or(0),
        auto_unlock_wts_query_failed = observation.error_code.is_some(),
        auto_unlock_observation_us = duration.map_or(0, |d| d.as_micros().min(u128::from(u64::MAX)) as u64),
        "automatic unlock session lock observation"
    );
}

pub(crate) fn record_entry_gate_diagnostic(source_status: u64, rejection: Option<EntryRejection>) {
    tracing::info!(
        target: "oink_auto_unlock_diagnostics",
        auto_unlock_event = 2u64,
        auto_unlock_source_status = source_status,
        auto_unlock_entry_rejection = rejection.map_or(0, EntryRejection::diagnostic_code),
        "automatic unlock entry gate decision"
    );
}

/// Recorder-only stage markers for correlating the cross-device unlock path.
/// Values are process-local numeric state; no source identity, credential or
/// serialized authentication data is retained.
pub(crate) fn record_flow_diagnostic(stage: u64, epoch: u64, outcome: u64) {
    tracing::info!(
        target: "oink_auto_unlock_diagnostics",
        auto_unlock_event = 3u64,
        auto_unlock_flow_stage = stage,
        auto_unlock_flow_epoch = epoch,
        auto_unlock_flow_outcome = outcome,
        "automatic unlock flow stage"
    );
}

#[cfg(windows)]
fn record_provider_readiness_diagnostic(status: &oink_winsvc::AutoUnlockStatus) {
    tracing::info!(
        target: "oink_auto_unlock_diagnostics",
        auto_unlock_event = 4u64,
        auto_unlock_provider_ready_ms = status.provider_ready_at_ms,
        auto_unlock_remote_input_ms = status.remote_input_at_ms,
        auto_unlock_remote_input_source = status.remote_input_source,
        auto_unlock_readiness_decision = status.readiness_decision,
        "automatic unlock provider readiness decision"
    );
}

impl EntryGate {
    pub(crate) fn observe(&mut self, locked: Option<bool>, holder: DeviceId, local: DeviceId) {
        if locked != Some(true) {
            self.locked = false;
            self.armed = false;
            self.pending = None;
            self.authorized = None;
        } else {
            if !self.locked {
                self.armed = holder != local;
            }
            self.locked = true;
            if holder != local {
                self.armed = true;
                self.pending = None;
                self.authorized = None;
            }
        }
    }
    pub(crate) fn cancel(&mut self) {
        self.pending = None;
        self.authorized = None;
    }
    pub(crate) fn enter_with_reason(
        &mut self,
        source: DeviceId,
        epoch: u64,
        now: Instant,
    ) -> Result<(), EntryRejection> {
        if !self.locked {
            return Err(EntryRejection::Locked);
        }
        if !self.armed {
            return Err(EntryRejection::Armed);
        }
        if self.last_epoch.is_some_and(|e| epoch <= e) {
            return Err(EntryRejection::Stale);
        }
        self.armed = false;
        self.last_epoch = Some(epoch);
        self.pending = Some((source, epoch, now));
        Ok(())
    }
    pub(crate) fn confirm(&mut self, source: DeviceId, epoch: u64, unlocked: bool, now: Instant) -> Option<DeviceId> {
        let (expected, entry, at) = self.pending?;
        if source != expected || epoch != entry {
            return None;
        }
        self.pending = None;
        if self.locked && unlocked && now.saturating_duration_since(at) <= Duration::from_secs(5) {
            self.authorized = Some((source, epoch, now));
            Some(source)
        } else {
            None
        }
    }
    pub(crate) fn authorized(&self, source: DeviceId, now: Instant) -> bool {
        self.authorized.is_some_and(|(expected, _, at)| {
            expected == source && now.saturating_duration_since(at) <= Duration::from_secs(5)
        })
    }
}

/// A numeric, process-local source tag binds native readiness evidence to the
/// authenticated peer without putting the 32-byte device identity in logs or
/// the Windows service ledger.
#[cfg(windows)]
pub(crate) fn source_tag(source: DeviceId) -> u64 {
    source.0.iter().fold(0xcbf29ce484222325, |hash, byte| {
        hash.wrapping_mul(0x100000001b3).wrapping_add(u64::from(*byte))
    })
}
#[cfg(test)]
mod tests;
