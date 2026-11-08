//! Local credentials never participate in owner options, group config or wire DTOs.
use oink_foundation::DeviceId;
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};
use zeroize::Zeroize;
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
    clear_failure(dir).map_err(error)
}
pub(crate) fn disable(dir: &Path) -> crate::api::ApiResult<()> {
    let _guard = OPERATION.lock().map_err(|_| error("自动解锁不可用"))?;
    platform_disable().map_err(error)?;
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
        input.send(crate::threads::input::InputCommand::LocalUnlock { epoch, job });
        Ok(())
    })
    .map_err(|reason| match reason {
        "密码不正确" => FailureKind::IncorrectPassword,
        "无法唤醒 macOS 锁屏显示器"
        | "macOS 锁屏密码框未在规定时间内就绪"
        | "自动输入未能及时完成，已取消"
        | "锁屏状态或登录会话已变化，自动解锁已停止" => FailureKind::LoginUiNotReady,
        _ => FailureKind::Unknown,
    })
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
