//! Local, user-initiated HAL installation. This module never participates in
//! group networking or calls the privileged path during discovery/startup.
use serde::{Deserialize, Serialize};
mod cache;
mod progress;
#[cfg(any(windows, test))]
mod windows_presence;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicrophonePluginState {
    #[default]
    Unsupported,
    NotInstalled,
    /// A Windows vendor driver exists without a matching Oink ownership record.
    Unmanaged,
    Installed,
    VersionMismatch,
    NotLoaded,
    Error,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicrophonePluginOperation {
    #[default]
    Idle,
    Installing,
    Uninstalling,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicrophonePluginErrorKind {
    Cancelled,
    AuthorizationDenied,
    VerificationFailed,
    QueryFailed,
    Other,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicrophonePluginStep {
    Verify,
    Extract,
    InstallDriver,
    UninstallDriver,
    CopyPlugin,
    RemovePlugin,
    RestartAudio,
    Finish,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MicrophonePluginStatus {
    pub state: MicrophonePluginState,
    #[serde(default)]
    pub managed: bool,
    /// A background inspection is in flight; the previous snapshot remains usable.
    #[serde(default)]
    pub refreshing: bool,
    pub step: Option<MicrophonePluginStep>,
    #[serde(default)]
    pub restart_required: bool,
    /// Last successful operation: Windows reboot lifetime, macOS process lifetime.
    #[serde(default)]
    pub last_operation: Option<Action>,
    pub step_started_at_ms: Option<u64>,
    pub operation: MicrophonePluginOperation,
    pub bundled_version: Option<String>,
    pub installed_version: Option<String>,
    pub bundled_git_sha: Option<String>,
    pub installed_git_sha: Option<String>,
    pub input_loaded: bool,
    pub feed_loaded: bool,
    pub input_git_sha: Option<String>,
    pub feed_git_sha: Option<String>,
    pub bundled_plugin_version: Option<String>,
    pub installed_plugin_version: Option<String>,
    pub input_plugin_version: Option<String>,
    pub feed_plugin_version: Option<String>,
    pub error_kind: Option<MicrophonePluginErrorKind>,
    /// Raw system/signature/authorization error or the latest install output.
    pub detail: Option<String>,
    pub last_operation_succeeded: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Install,
    Uninstall,
}

pub fn start() {
    cache::start();
}

/// Native audio inventory notifications coalesce into the single worker.
pub(crate) fn devices_changed() {
    cache::changed();
}

pub fn status() -> MicrophonePluginStatus {
    cache::status()
}

/// Synchronous local-only preparation for the explicit macOS app uninstall.
/// Uses the Settings authorization/removal path; no receipt is needed for ownership.
#[cfg(target_os = "macos")]
pub fn remove_for_app_uninstall(app: &std::path::Path) -> Result<(), (MicrophonePluginErrorKind, String)> {
    macos::remove_for_app_uninstall(app).map_err(|error| (error.kind, error.detail))
}

/// One inspection publishes installer status and the actual runtime endpoint
/// together. The endpoint is process-local, never part of the public IPC schema.
#[derive(Clone, Default)]
struct Inspection {
    status: MicrophonePluginStatus,
    #[cfg(any(windows, test))]
    render_id: Option<String>,
}
impl From<MicrophonePluginStatus> for Inspection {
    fn from(status: MicrophonePluginStatus) -> Self {
        Self {
            status,
            #[cfg(any(windows, test))]
            render_id: None,
        }
    }
}
#[cfg(windows)]
pub(crate) fn virtual_output() -> Option<String> {
    cache::virtual_output()
}

/// Internal Windows CLI continuation; never routed through IPC or discovery.
#[cfg(windows)]
pub fn complete_windows_installation(action: &str, token: &str) -> Result<(), String> {
    windows::complete(action, token)
}

fn inspect_status() -> Inspection {
    #[cfg(target_os = "macos")]
    return macos::status().into();
    #[cfg(windows)]
    return windows::status();
    #[cfg(not(any(target_os = "macos", windows)))]
    MicrophonePluginStatus::default().into()
}
pub fn request(action: Action) -> Result<MicrophonePluginStatus, String> {
    request_prepared(action, || Ok(None))
}
pub(crate) fn request_with_audio(
    action: Action,
    audio: &crate::audio::LinkHandle,
) -> Result<MicrophonePluginStatus, String> {
    request_prepared(action, || audio.prepare_driver_change().map(Some))
}
fn request_prepared(
    action: Action,
    prepare: impl FnOnce() -> Result<Option<crate::audio::DriverChangeGuard>, String>,
) -> Result<MicrophonePluginStatus, String> {
    start();
    cache::begin(action)?;
    let prepared = prepare().and_then(|guard| request_platform(action, guard));
    match prepared {
        Ok(_) => Ok(status()),
        Err(error) => {
            cache::completed();
            Err(error)
        }
    }
}
fn request_platform(
    action: Action,
    guard: Option<crate::audio::DriverChangeGuard>,
) -> Result<MicrophonePluginStatus, String> {
    #[cfg(target_os = "macos")]
    return macos::request(action, guard);
    #[cfg(windows)]
    return windows::request(action, guard);
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (action, guard);
        Err("virtual microphone plugin installation is only supported on macOS".into())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::{
        io::Read,
        path::{Path, PathBuf},
        process::{Command, Output, Stdio},
        sync::{
            Mutex,
            atomic::{AtomicU8, Ordering},
        },
        time::{Duration, Instant},
    };

    const DESTINATION: &str = "/Library/Audio/Plug-Ins/HAL/OinkMicrophone.driver";
    // Only installation migration recognizes this old identity; audio routing uses Oink UIDs only.
    const LEGACY_DESTINATION: &str = "/Library/Audio/Plug-Ins/HAL/AcrossMicrophone.driver";
    const LEGACY_PLUGIN_REQUIREMENT: &str = "=identifier \"com.across.microphone.driver\" and anchor apple generic and certificate leaf[subject.OU] = \"893V37X2CW\"";
    const REQUIREMENT: &str =
        "=identifier \"com.hachiwii.oink\" and anchor apple generic and certificate leaf[subject.OU] = \"893V37X2CW\"";
    const PLUGIN_REQUIREMENT: &str = "=identifier \"com.hachiwii.oink.microphone.driver\" and anchor apple generic and certificate leaf[subject.OU] = \"893V37X2CW\"";
    const BOOTSTRAP: &str = include_str!("../../../apps/audio-plugin/privileged-bootstrap.sh");
    const AUTHORIZATION_SCRIPT: &str = "on run argv\n do shell script (item 1 of argv) with administrator privileges with prompt \"Oink needs administrator authorization to install or remove its virtual microphone. System audio and calls will briefly stop.\"\nend run";
    static OPERATION: AtomicU8 = AtomicU8::new(0);
    #[derive(Default)]
    struct Outcome {
        result: Option<(bool, String, Option<MicrophonePluginErrorKind>)>,
        last_operation: Option<Action>,
    }
    impl Outcome {
        fn complete(&mut self, action: Action, success: bool, detail: String, kind: Option<MicrophonePluginErrorKind>) {
            if success {
                self.last_operation = Some(action);
            }
            self.result = Some((success, detail, kind));
        }
    }
    static RESULT: Mutex<Outcome> = Mutex::new(Outcome {
        result: None,
        last_operation: None,
    });

    fn output(command: &mut Command) -> Result<String, String> {
        let Output { status, stdout, stderr } = command.output().map_err(|e| e.to_string())?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        if status.success() { Ok(text) } else { Err(text) }
    }
    fn verify(path: &Path, requirement: &str, deep: bool) -> Result<(), String> {
        let mut command = Command::new("/usr/bin/codesign");
        command.args(["--verify", "--strict", "-R", requirement]);
        if deep {
            command.arg("--deep");
        }
        output(command.arg(path)).map(|_| ())
    }
    fn plist(path: &Path, key: &str) -> Result<String, String> {
        output(
            Command::new("/usr/bin/plutil")
                .args(["-extract", key, "raw", "-o", "-"])
                .arg(path),
        )
        .map(|s| s.trim().into())
    }
    fn application() -> Result<PathBuf, String> {
        let executable = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let macos = executable.parent().ok_or("missing executable parent")?;
        let contents = macos.parent().ok_or("missing Contents directory")?;
        let app = contents.parent().ok_or("missing application bundle")?;
        if macos.file_name().and_then(|s| s.to_str()) != Some("MacOS")
            || contents.file_name().and_then(|s| s.to_str()) != Some("Contents")
            || app.extension().and_then(|s| s.to_str()) != Some("app")
        {
            return Err("Use a signed, packaged Oink.app to manage the virtual microphone".into());
        }
        Ok(app.to_owned())
    }
    fn payload(app: &Path) -> PathBuf {
        app.join("Contents/Resources/AudioPlugin/OinkMicrophone.driver")
    }
    fn preflight(app: &Path) -> Result<String, String> {
        verify(app, REQUIREMENT, true)?;
        let plugin = payload(app);
        verify(&plugin, PLUGIN_REQUIREMENT, false)?;
        let script = app.join("Contents/Resources/AudioPlugin/install.sh");
        if !script.is_file()
            || std::fs::symlink_metadata(&script)
                .map_err(|e| e.to_string())?
                .file_type()
                .is_symlink()
        {
            return Err("Bundled installer is missing or is a symbolic link".into());
        }
        let app_sha = plist(&app.join("Contents/Resources/oink-build.json"), "source_tree_sha256")?;
        if app_sha != plist(&plugin.join("Contents/Info.plist"), "OinkGitSHA")? {
            return Err("Bundled microphone revision differs from Oink".into());
        }
        Ok(app_sha)
    }
    fn operation() -> MicrophonePluginOperation {
        match OPERATION.load(Ordering::Acquire) {
            1 => MicrophonePluginOperation::Installing,
            2 => MicrophonePluginOperation::Uninstalling,
            _ => MicrophonePluginOperation::Idle,
        }
    }
    #[derive(Debug)]
    pub(super) struct InspectionError {
        pub(super) kind: MicrophonePluginErrorKind,
        pub(super) detail: String,
    }
    impl From<String> for InspectionError {
        fn from(detail: String) -> Self {
            Self {
                kind: MicrophonePluginErrorKind::VerificationFailed,
                detail,
            }
        }
    }
    impl InspectionError {
        fn query(detail: String) -> Self {
            Self {
                kind: MicrophonePluginErrorKind::QueryFailed,
                detail,
            }
        }
        fn operation(detail: String) -> Self {
            Self {
                kind: classify_error(&detail),
                detail,
            }
        }
    }
    fn apply_inspection_error(state: &mut MicrophonePluginStatus, error: InspectionError) {
        state.state = MicrophonePluginState::Error;
        state.error_kind = Some(error.kind);
        state.detail = Some(error.detail);
    }
    pub(super) fn status() -> MicrophonePluginStatus {
        let mut result = MicrophonePluginStatus {
            operation: operation(),
            ..Default::default()
        };
        {
            let outcome = RESULT.lock().unwrap_or_else(|e| e.into_inner());
            result.last_operation = outcome.last_operation;
            if let Some((success, detail, kind)) = outcome.result.as_ref() {
                result.last_operation_succeeded = Some(*success);
                result.detail = Some(detail.clone());
                result.error_kind = *kind;
            }
        }
        let app = match application() {
            Ok(app) => app,
            Err(error) => {
                result.detail = Some(error);
                return result;
            }
        };
        if let Err(error) = inspect(&app, &mut result) {
            apply_inspection_error(&mut result, error);
        }
        result
    }
    fn inspect(app: &Path, state: &mut MicrophonePluginStatus) -> Result<(), InspectionError> {
        inspect_devices(state)?;
        inspect_files(app, state)
    }
    fn inspect_files(app: &Path, state: &mut MicrophonePluginStatus) -> Result<(), InspectionError> {
        let bundled = payload(app).join("Contents/Info.plist");
        state.bundled_version = Some(plist(&bundled, "CFBundleShortVersionString")?);
        state.bundled_git_sha = Some(plist(&bundled, "OinkGitSHA")?);
        state.bundled_plugin_version = Some(plist(&bundled, "OinkPluginVersion")?);
        inspect_installation(state, Path::new(DESTINATION), Path::new(LEGACY_DESTINATION), verify)
    }
    fn inspect_devices(state: &mut MicrophonePluginStatus) -> Result<(), InspectionError> {
        let devices = oink_audio::microphone::macos::plugin_status().map_err(InspectionError::query)?;
        state.input_loaded = devices.input_device.is_some();
        state.feed_loaded = devices.feed_device.is_some();
        state.input_git_sha = devices.input_git_sha;
        state.feed_git_sha = devices.feed_git_sha;
        state.input_plugin_version = devices.input_plugin_version;
        state.feed_plugin_version = devices.feed_plugin_version;
        Ok(())
    }
    fn inspect_installation(
        state: &mut MicrophonePluginStatus,
        destination: &Path,
        legacy: &Path,
        verify_bundle: impl Fn(&Path, &str, bool) -> Result<(), String>,
    ) -> Result<(), InspectionError> {
        let legacy_present = legacy.exists();
        let (installed_bundle, old_identity) = if destination.exists() {
            (destination, false)
        } else if legacy_present {
            (legacy, true)
        } else {
            state.state = MicrophonePluginState::NotInstalled;
            return Ok(());
        };
        verify_bundle(
            installed_bundle,
            if old_identity {
                LEGACY_PLUGIN_REQUIREMENT
            } else {
                PLUGIN_REQUIREMENT
            },
            false,
        )?;
        let installed = installed_bundle.join("Contents/Info.plist");
        state.installed_version = Some(plist(&installed, "CFBundleShortVersionString")?);
        state.installed_git_sha = Some(plist(
            &installed,
            if old_identity { "AcrossGitSHA" } else { "OinkGitSHA" },
        )?);
        state.installed_plugin_version = plist(
            &installed,
            if old_identity {
                "AcrossPluginVersion"
            } else {
                "OinkPluginVersion"
            },
        )
        .ok();
        // Even a matching new driver needs reinstall if the old bundle remains:
        // otherwise HAL can expose two devices with the same display name.
        state.state = if legacy_present {
            MicrophonePluginState::VersionMismatch
        } else {
            classify(state.input_loaded && state.feed_loaded, same_driver(state))
        };
        Ok(())
    }
    fn same_driver(state: &MicrophonePluginStatus) -> bool {
        state.bundled_plugin_version.is_some()
            && state.bundled_plugin_version == state.installed_plugin_version
            && state.bundled_plugin_version == state.input_plugin_version
            && state.bundled_plugin_version == state.feed_plugin_version
    }
    fn classify(loaded: bool, matches: bool) -> MicrophonePluginState {
        if !loaded {
            MicrophonePluginState::NotLoaded
        } else if !matches {
            MicrophonePluginState::VersionMismatch
        } else {
            MicrophonePluginState::Installed
        }
    }
    fn quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
    fn privileged_command(app: &Path, action: Action, expected_sha: &str, token: &str) -> Result<String, String> {
        let path = app.to_str().ok_or("application path is not UTF-8")?;
        let action = match action {
            Action::Install => "install",
            Action::Uninstall => "uninstall",
        };
        // No caller-controlled command, path, version, flags or shell fragments.
        Ok(format!(
            "/usr/bin/env -i /bin/bash --noprofile --norc -c {} -- {} {action} {} {}",
            quote(BOOTSTRAP),
            quote(path),
            quote(expected_sha),
            quote(token)
        ))
    }
    fn classify_error(error: &str) -> MicrophonePluginErrorKind {
        if error.contains("(-128)") || error.contains("(-60006)") {
            MicrophonePluginErrorKind::Cancelled
        } else if error.contains("(-60005)") {
            // These are explicit Authorization Services denial codes; never infer
            // an enterprise policy from a generic permission or shell error.
            MicrophonePluginErrorKind::AuthorizationDenied
        } else if error.contains("verification failed")
            || error.contains("code object is not signed")
            || error.contains("invalid signature")
        {
            MicrophonePluginErrorKind::VerificationFailed
        } else {
            MicrophonePluginErrorKind::Other
        }
    }
    fn read_progress(token: &str) {
        let Ok(file) = std::fs::File::open("/private/var/run/oink-microphone-progress/status.json") else {
            return;
        };
        let mut bytes = Vec::new();
        if file.take(8193).read_to_end(&mut bytes).is_ok()
            && let Some(receipt) = progress::parse(&bytes, token)
        {
            cache::progress(receipt.step);
        }
    }
    fn perform(app: &Path, action: Action, expected_sha: &str, token: &str) -> Result<String, InspectionError> {
        let command = privileged_command(app, action, expected_sha, token).map_err(InspectionError::operation)?;
        let authorization_started = Instant::now();
        // SecurityAgent alone handles authorization. Progress is a display-only
        // root-owned receipt because AppleScript buffers do-shell-script stdout.
        let mut child = Command::new("/usr/bin/osascript")
            .args(["-e", AUTHORIZATION_SCRIPT, &command])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| InspectionError::operation(e.to_string()))?;
        // Drain output while waiting, so diagnostic output cannot block the child.
        let stdout = drain(child.stdout.take().unwrap());
        let stderr = drain(child.stderr.take().unwrap());
        while child
            .try_wait()
            .map_err(|e| InspectionError::operation(e.to_string()))?
            .is_none()
        {
            read_progress(token);
            std::thread::sleep(Duration::from_millis(100));
        }
        read_progress(token);
        let exit = child.wait().map_err(|e| InspectionError::operation(e.to_string()))?;
        let transcript = format!(
            "{}{}",
            stdout.join().unwrap_or_default(),
            stderr.join().unwrap_or_default()
        );
        tracing::info!(
            elapsed_ms = authorization_started.elapsed().as_millis(),
            "microphone authorization and privileged work finished"
        );
        tracing::info!(%transcript, "microphone privileged phase timings");
        if !exit.success() {
            return Err(InspectionError::operation(transcript));
        }
        cache::progress(MicrophonePluginStep::Finish);
        let started = Instant::now();
        // Verify the installed bundle once. Only the changing HAL device identity
        // is polled; codesign/plutil are no longer relaunched every 200 ms.
        let mut state = MicrophonePluginStatus::default();
        inspect_files(app, &mut state)?;
        let files_absent = state.state == MicrophonePluginState::NotInstalled;
        let legacy_present = Path::new(LEGACY_DESTINATION).exists();
        wait_for_devices(
            action,
            files_absent,
            legacy_present,
            &mut state,
            &transcript,
            inspect_devices,
            || std::thread::sleep(Duration::from_millis(100)),
        )?;
        tracing::info!(elapsed_ms = started.elapsed().as_millis(), "microphone devices ready");
        Ok(transcript)
    }
    fn wait_for_devices(
        action: Action,
        files_absent: bool,
        legacy_present: bool,
        state: &mut MicrophonePluginStatus,
        transcript: &str,
        mut inspect: impl FnMut(&mut MicrophonePluginStatus) -> Result<(), InspectionError>,
        mut pause: impl FnMut(),
    ) -> Result<(), InspectionError> {
        let started = Instant::now();
        loop {
            let observed = inspect(state);
            let ready = match action {
                Action::Install => {
                    !files_absent && !legacy_present && state.input_loaded && state.feed_loaded && same_driver(state)
                }
                Action::Uninstall => files_absent && !state.input_loaded && !state.feed_loaded,
            };
            if observed.is_ok() && ready {
                return Ok(());
            }
            if started.elapsed() >= Duration::from_secs(5) {
                return Err(post_install_error(state, observed.err(), transcript, ""));
            }
            pause();
        }
    }
    fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut retained = Vec::new();
            let mut buffer = [0u8; 4096];
            while let Ok(length) = pipe.read(&mut buffer) {
                if length == 0 {
                    break;
                }
                retained.extend_from_slice(&buffer[..length]);
                if retained.len() > 65536 {
                    retained.drain(..retained.len() - 65536);
                }
            }
            String::from_utf8_lossy(&retained).into()
        })
    }
    fn post_install_error(
        state: &MicrophonePluginStatus,
        observed: Option<InspectionError>,
        transcript: &str,
        diagnostics: &str,
    ) -> InspectionError {
        let (kind, detail) = observed.map_or((MicrophonePluginErrorKind::Other, String::new()), |error| {
            (error.kind, error.detail)
        });
        InspectionError {
            kind,
            detail: format!(
                "{transcript}\nHAL post-install check incomplete: {:?}; {detail}\n{diagnostics}",
                state.state
            ),
        }
    }
    // Fixed names plus the signed identifier/Apple anchor/team establish ownership,
    // independently of runtime data, receipts, version metadata or loaded HAL UIDs.
    fn owned_installation_present(
        destination: &Path,
        legacy: &Path,
        verify_bundle: impl Fn(&Path, &str, bool) -> Result<(), String>,
    ) -> Result<bool, InspectionError> {
        let mut present = false;
        for (path, requirement) in [(destination, PLUGIN_REQUIREMENT), (legacy, LEGACY_PLUGIN_REQUIREMENT)] {
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(InspectionError::query(error.to_string())),
                Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "Refusing non-directory or symlink microphone destination: {}",
                        path.display()
                    )
                    .into());
                }
                Ok(_) => {}
            }
            verify_bundle(path, requirement, false)?;
            present = true;
        }
        Ok(present)
    }
    pub(super) fn remove_for_app_uninstall(app: &Path) -> Result<(), InspectionError> {
        OPERATION
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| InspectionError::operation("A microphone plugin operation is already pending".into()))?;
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                OPERATION.store(0, Ordering::Release);
                cache::completed();
            }
        }
        let _reset = Reset;
        if !owned_installation_present(Path::new(DESTINATION), Path::new(LEGACY_DESTINATION), verify)? {
            return Ok(());
        }
        let expected_sha = preflight(app)?;
        let token = progress::token().map_err(InspectionError::operation)?;
        perform(app, Action::Uninstall, &expected_sha, &token).map(|_| ())
    }
    pub(super) fn request(
        action: Action,
        guard: Option<crate::audio::DriverChangeGuard>,
    ) -> Result<MicrophonePluginStatus, String> {
        let code = match action {
            Action::Install => 1,
            Action::Uninstall => 2,
        };
        OPERATION
            .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "A microphone plugin operation is already pending")?;
        RESULT.lock().unwrap_or_else(|e| e.into_inner()).result = None;
        if let Err(error) = std::thread::Builder::new()
            .name("microphone-plugin-install".into())
            .spawn(move || {
                let _audio_guard = guard;
                cache::progress(MicrophonePluginStep::Verify);
                let result = (|| {
                    let started = Instant::now();
                    let app = application().map_err(InspectionError::from)?;
                    let expected_sha = preflight(&app).map_err(InspectionError::from)?;
                    tracing::info!(
                        elapsed_ms = started.elapsed().as_millis(),
                        "microphone preflight finished"
                    );
                    let token = progress::token().map_err(InspectionError::operation)?;
                    perform(&app, action, &expected_sha, &token)
                })();
                let (success, detail, kind) = match result {
                    Ok(_) => (true, "Virtual microphone operation completed".into(), None),
                    Err(error) => (false, error.detail, Some(error.kind)),
                };
                tracing::info!(?action, success, ?kind, %detail, "microphone plugin operation completed");
                RESULT
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .complete(action, success, detail, kind);
                OPERATION.store(0, Ordering::Release);
                cache::completed();
            })
        {
            RESULT.lock().unwrap_or_else(|e| e.into_inner()).complete(
                action,
                false,
                error.to_string(),
                Some(MicrophonePluginErrorKind::Other),
            );
            OPERATION.store(0, Ordering::Release);
            return Err(error.to_string());
        }
        Ok(super::status())
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn uninstall_ownership_needs_only_fixed_names_and_verified_identity() {
            let temp = tempfile::tempdir().unwrap();
            let current = temp.path().join("OinkMicrophone.driver");
            let legacy = temp.path().join("AcrossMicrophone.driver");
            // Other vendors do not trigger authorization or require inspection.
            std::fs::create_dir(temp.path().join("OtherVendor.driver")).unwrap();
            assert!(
                !owned_installation_present(&current, &legacy, |_, _, _| panic!(
                    "absent bundles must not be verified"
                ))
                .unwrap()
            );
            for path in [&legacy, &current] {
                // No receipt, app data, or version plist is needed (old installs).
                std::fs::create_dir(path).unwrap();
                assert!(
                    owned_installation_present(&current, &legacy, |path, requirement, deep| {
                        assert!(!deep);
                        assert_eq!(
                            requirement,
                            if path == legacy {
                                LEGACY_PLUGIN_REQUIREMENT
                            } else {
                                PLUGIN_REQUIREMENT
                            }
                        );
                        Ok(())
                    })
                    .unwrap()
                );
            }
            let error = owned_installation_present(&current, &legacy, |_, _, _| Err("wrong team or identifier".into()))
                .unwrap_err();
            assert_eq!(error.kind, MicrophonePluginErrorKind::VerificationFailed);
            assert_eq!(error.detail, "wrong team or identifier");
            std::fs::remove_dir(&legacy).unwrap();
            std::os::unix::fs::symlink(temp.path().join("missing"), &legacy).unwrap();
            assert!(owned_installation_present(&current, &legacy, |_, _, _| Ok(())).is_err());
            assert!(current.exists());
        }

        #[test]
        fn last_successful_operation_is_process_local_and_survives_failed_attempts() {
            let mut outcome = Outcome::default();
            assert_eq!(outcome.last_operation, None);
            outcome.complete(Action::Install, true, String::new(), None);
            assert_eq!(outcome.last_operation, Some(Action::Install));
            outcome.result = None; // Starting the next attempt preserves the last success.
            assert_eq!(outcome.last_operation, Some(Action::Install));
            for kind in [MicrophonePluginErrorKind::Cancelled, MicrophonePluginErrorKind::Other] {
                outcome.complete(Action::Uninstall, false, String::new(), Some(kind));
                assert_eq!(outcome.last_operation, Some(Action::Install));
            }
            outcome.complete(Action::Uninstall, true, String::new(), None);
            assert_eq!(outcome.last_operation, Some(Action::Uninstall));
            assert_eq!(Outcome::default().last_operation, None); // New app run.
        }
        #[test]
        fn readiness_retries_coreaudio_restart_without_repeating_file_verification() {
            let mut state = MicrophonePluginStatus {
                bundled_plugin_version: Some("fixture".into()),
                installed_plugin_version: Some("fixture".into()),
                ..Default::default()
            };
            let mut calls = 0;
            let mut pauses = 0;
            wait_for_devices(
                Action::Install,
                false,
                false,
                &mut state,
                "",
                |state| {
                    calls += 1;
                    if calls == 1 {
                        return Err(InspectionError::query("audio service restarting".into()));
                    }
                    state.input_loaded = true;
                    state.feed_loaded = true;
                    state.input_plugin_version = Some("fixture".into());
                    state.feed_plugin_version = Some("fixture".into());
                    Ok(())
                },
                || {
                    pauses += 1;
                },
            )
            .unwrap();
            assert_eq!((calls, pauses), (2, 1));
            wait_for_devices(
                Action::Uninstall,
                true,
                false,
                &mut state,
                "",
                |state| {
                    state.input_loaded = false;
                    state.feed_loaded = false;
                    Ok(())
                },
                || panic!("ready devices must not incur a fixed wait"),
            )
            .unwrap();
        }
        #[test]
        #[ignore = "read-only native timing; no device mutation or capture"]
        fn native_device_query_timing() {
            for _ in 0..5 {
                let started = Instant::now();
                let mut status = MicrophonePluginStatus::default();
                let result = inspect_devices(&mut status);
                eprintln!(
                    "native_device_query_ms={} result={:?}",
                    started.elapsed().as_secs_f64() * 1000.,
                    result
                );
                assert!(result.is_ok());
            }
        }
        #[test]
        fn installed_identity_migrates_legacy_and_rejects_signature_failures() {
            let temp = tempfile::tempdir().unwrap();
            let current = temp.path().join("OinkMicrophone.driver");
            let legacy = temp.path().join("AcrossMicrophone.driver");
            let mut state = MicrophonePluginStatus {
                bundled_plugin_version: Some("same-driver".into()),
                input_plugin_version: Some("same-driver".into()),
                feed_plugin_version: Some("same-driver".into()),
                input_loaded: false,
                feed_loaded: false,
                ..Default::default()
            };
            let verify_fixture = |path: &Path, requirement: &str, deep: bool| {
                assert!(!deep);
                assert_eq!(
                    requirement,
                    if path == legacy {
                        LEGACY_PLUGIN_REQUIREMENT
                    } else {
                        PLUGIN_REQUIREMENT
                    }
                );
                Ok(())
            };
            inspect_installation(&mut state, &current, &legacy, verify_fixture).unwrap();
            assert_eq!(state.state, MicrophonePluginState::NotInstalled);
            for (path, prefix) in [(&legacy, "Across"), (&current, "Oink")] {
                state.input_loaded = path == &current;
                state.feed_loaded = path == &current;
                std::fs::create_dir_all(path.join("Contents")).unwrap();
                std::fs::write(path.join("Contents/Info.plist"), format!(
                    "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleShortVersionString</key><string>0.1.0</string><key>{prefix}GitSHA</key><string>source</string><key>{prefix}PluginVersion</key><string>same-driver</string></dict></plist>"
                )).unwrap();
                inspect_installation(&mut state, &current, &legacy, verify_fixture).unwrap();
                assert_eq!(state.state, MicrophonePluginState::VersionMismatch);
                assert_eq!(state.installed_git_sha.as_deref(), Some("source"));
            }
            // Both names require cleanup even when all new driver hashes match.
            std::fs::remove_dir_all(&legacy).unwrap();
            inspect_installation(&mut state, &current, &legacy, verify_fixture).unwrap();
            assert_eq!(state.state, MicrophonePluginState::Installed);
            state.feed_loaded = false;
            inspect_installation(&mut state, &current, &legacy, verify_fixture).unwrap();
            assert_eq!(state.state, MicrophonePluginState::NotLoaded);
            let error = inspect_installation(&mut state, &current, &legacy, |_, _, _| Err("invalid signature".into()))
                .unwrap_err();
            assert_eq!(error.kind, MicrophonePluginErrorKind::VerificationFailed);
        }

        #[test]
        fn query_signature_and_post_install_failures_keep_distinct_categories() {
            for (error, expected) in [
                (
                    InspectionError::query("HAL driver identity query failed: 2003332927".into()),
                    MicrophonePluginErrorKind::QueryFailed,
                ),
                (
                    InspectionError::from("invalid signature".to_owned()),
                    MicrophonePluginErrorKind::VerificationFailed,
                ),
                (
                    InspectionError::operation("execution error (-128)".into()),
                    MicrophonePluginErrorKind::Cancelled,
                ),
            ] {
                let mut state = MicrophonePluginStatus::default();
                let detail = error.detail.clone();
                // The installation timeout used to relabel every failure as a
                // verification failure by inspecting its wrapper's wording.
                let wrapped = post_install_error(&state, Some(error), "installer completed", "");
                apply_inspection_error(&mut state, wrapped);
                assert_eq!(state.state, MicrophonePluginState::Error);
                assert_eq!(state.error_kind, Some(expected));
                assert!(state.detail.as_ref().unwrap().contains(&detail));
            }
            let waiting = post_install_error(&MicrophonePluginStatus::default(), None, "", "");
            assert_eq!(waiting.kind, MicrophonePluginErrorKind::Other);
            assert_eq!(
                serde_json::to_string(&MicrophonePluginErrorKind::QueryFailed).unwrap(),
                "\"query_failed\""
            );
        }
        #[test]
        fn driver_identity_ignores_unrelated_application_revisions() {
            let mut state = MicrophonePluginStatus {
                bundled_git_sha: Some("new-app".into()),
                installed_git_sha: Some("old-app".into()),
                input_git_sha: Some("old-app".into()),
                feed_git_sha: Some("old-app".into()),
                bundled_plugin_version: Some("driver-content".into()),
                installed_plugin_version: Some("driver-content".into()),
                input_plugin_version: Some("driver-content".into()),
                feed_plugin_version: Some("driver-content".into()),
                ..Default::default()
            };
            assert!(same_driver(&state));
            state.input_plugin_version = Some("old-content".into());
            assert!(!same_driver(&state));
            state.input_plugin_version = None;
            assert!(!same_driver(&state));
            assert!(!same_driver(&MicrophonePluginStatus::default()));
        }
        #[test]
        fn errors_have_stable_conservative_categories() {
            assert_eq!(
                classify_error("User canceled. (-128)"),
                MicrophonePluginErrorKind::Cancelled
            );
            assert_eq!(
                classify_error("Authorization (-60006)"),
                MicrophonePluginErrorKind::Cancelled
            );
            assert_eq!(
                classify_error("Authorization (-60005)"),
                MicrophonePluginErrorKind::AuthorizationDenied
            );
            assert_eq!(
                classify_error("HAL post-install verification failed"),
                MicrophonePluginErrorKind::VerificationFailed
            );
            assert_eq!(
                classify_error("Operation not permitted"),
                MicrophonePluginErrorKind::Other
            );
        }
        #[test]
        fn codesign_inline_requirement_is_not_a_filename() {
            verify(Path::new("/usr/bin/true"), "=anchor apple", false).unwrap();
            assert!(REQUIREMENT.starts_with("=identifier"));
            assert!(PLUGIN_REQUIREMENT.starts_with("=identifier"));
        }

        #[test]
        fn unsigned_bundle_fails_before_any_installer_execution() {
            let temp = tempfile::tempdir().unwrap();
            let app = temp.path().join("Oink.app");
            let resources = app.join("Contents/Resources/AudioPlugin");
            std::fs::create_dir_all(&resources).unwrap();
            std::fs::write(resources.join("install.sh"), "exit 99").unwrap();
            assert!(preflight(&app).is_err());
        }

        #[test]
        fn shell_path_is_data_even_with_metacharacters() {
            let path = "/tmp/Across ' $(touch NEVER) `x` ;\n.app";
            // Execute only printf, never the installer or authorization dialog.
            let printed = output(Command::new("/bin/sh").args(["-c", &format!("printf %s {}", quote(path))])).unwrap();
            assert_eq!(printed, path);
        }
        #[test]
        fn unloaded_is_not_success_even_at_matching_version() {
            assert_eq!(classify(false, true), MicrophonePluginState::NotLoaded);
            assert_eq!(classify(true, false), MicrophonePluginState::VersionMismatch);
            assert_eq!(classify(true, true), MicrophonePluginState::Installed);
        }
    }
}

#[cfg(windows)]
mod windows;

#[cfg(windows)]
mod windows_query;
