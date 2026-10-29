//! Explicit, local installation from the immutable vendor archive in the MSI.
use super::*;
use std::{
    ffi::OsString,
    io::{Read, Write},
    os::windows::ffi::OsStringExt,
};
use std::{
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

const SCRIPT: &str = include_str!("../../../../packaging/windows/Microphone.ps1");
const BOOTSTRAP: &str = include_str!("../../../../packaging/windows/MicrophoneBootstrap.ps1");
static OPERATION: AtomicU8 = AtomicU8::new(0);
static RESULT: Mutex<Option<(bool, String)>> = Mutex::new(None);
fn progress_path() -> Result<PathBuf, String> {
    use ::windows::Win32::{System::Com::CoTaskMemFree, UI::Shell::*};
    // Windows 11's post-update launch environment can contain the literal
    // "%SystemDrive%\\ProgramData". Match the PowerShell writer's known folder,
    // rather than treating that environment value as a relative path.
    // SAFETY: the fixed known folder returns an allocated, terminated UTF-16 path.
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT, None) }
        .map_err(|error| format!("Cannot locate microphone progress directory: {error}"))?;
    // SAFETY: the returned allocation remains alive until CoTaskMemFree below.
    let directory = unsafe { OsString::from_wide(raw.as_wide()) };
    // SAFETY: release exactly the allocation returned by the shell API.
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    Ok(PathBuf::from(directory).join("OinkMicrophoneProgress/status.json"))
}
fn progress(path: &Path, token: &str) -> Option<progress::Receipt> {
    // This is display-only input: bounded size, matching random token and enum
    // step. It can never authorize installation or confer driver ownership.
    let mut file = std::fs::File::open(path).ok()?.take(8193);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    if bytes.len() > 8192 {
        return None;
    }
    progress::parse(&bytes, token).filter(|p| {
        matches!(
            p.step,
            MicrophonePluginStep::Verify
                | MicrophonePluginStep::Extract
                | MicrophonePluginStep::InstallDriver
                | MicrophonePluginStep::UninstallDriver
                | MicrophonePluginStep::Finish
        )
    })
}

fn powershell() -> Result<PathBuf, String> {
    let mut buffer = [0u16; 32768];
    // SAFETY: writable UTF-16 buffer; the returned length excludes the terminator.
    let length =
        unsafe { ::windows::Win32::System::SystemInformation::GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err("Cannot locate Windows system directory".into());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..length])).join("WindowsPowerShell/v1.0/powershell.exe"))
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
        String::from_utf8_lossy(&retained).trim().into()
    })
}
fn bootstrap(mode: &str, action: &str, token: &str) -> Result<String, String> {
    if !matches!(mode, "elevate" | "execute")
        || !matches!(action, "install" | "uninstall")
        || token.len() != 32
        || !token.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("Invalid microphone installer request".into());
    }
    // Only enum/hex literals enter command text. The installer body goes through
    // stdin from this binary, never through a caller-selected file or encoding.
    Ok(format!(
        "$Mode='{mode}';$Action='{action}';$operationToken='{token}';\n{BOOTSTRAP}"
    ))
}
fn run(mode: &str, action: &str, token: &str) -> Result<String, String> {
    let command = bootstrap(mode, action, token)?;
    let path = progress_path()?;
    let mut child = Command::new(powershell()?)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ])
        .arg(command)
        .stdin(Stdio::piped())
        .creation_flags(0x08000000)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    // The small embedded payload cannot depend on filesystem state. Close stdin
    // before waiting so the bootstrap can finish its bounded read.
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(SCRIPT.as_bytes())
        .map_err(|e| e.to_string())?;
    let stdout = drain(child.stdout.take().expect("piped stdout"));
    let stderr = drain(child.stderr.take().expect("piped stderr"));
    let status = wait_for_installer(&mut child, &path, token, cache::progress)?;
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    let receipt = progress(&path, token);
    if mode == "elevate" {
        for change in receipt
            .as_ref()
            .and_then(|receipt| receipt.audio_defaults.as_ref())
            .into_iter()
            .flatten()
        {
            tracing::info!(action, flow=%change.flow, role=%change.role, before=?change.before,
                after=?change.after, outcome=%change.outcome, detail=%change.detail,
                "microphone vendor default audio change");
        }
    }
    if status.success() {
        Ok(stdout)
    } else if status.code() == Some(1223) {
        Err("Microphone authorization cancelled".into())
    } else {
        Err(receipt
            .filter(|p| !p.detail.is_empty())
            .map(|p| p.detail)
            .unwrap_or_else(|| stderr))
    }
}
fn wait_for_installer(
    child: &mut Child,
    path: &Path,
    token: &str,
    mut observe: impl FnMut(MicrophonePluginStep),
) -> Result<ExitStatus, String> {
    loop {
        if let Some(receipt) = progress(path, token) {
            observe(receipt.step);
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
/// Called only by the hidden installed CLI continuation after UAC. The bootstrap
/// checks the elevated token before creating anything in private ProgramData.
pub(super) fn complete(action: &str, token: &str) -> Result<(), String> {
    run("execute", action, token).map(|_| ())
}
pub(super) fn status() -> Inspection {
    let mut status = MicrophonePluginStatus {
        operation: match OPERATION.load(Ordering::Acquire) {
            1 => MicrophonePluginOperation::Installing,
            2 => MicrophonePluginOperation::Uninstalling,
            _ => MicrophonePluginOperation::Idle,
        },
        bundled_version: Some("VB-CABLE Pack45".into()),
        ..Default::default()
    };
    if let Some((success, detail)) = RESULT.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        status.last_operation_succeeded = Some(*success);
        status.detail = Some(detail.clone());
        if !success {
            status.error_kind = Some(if detail == "Microphone authorization cancelled" {
                MicrophonePluginErrorKind::Cancelled
            } else {
                MicrophonePluginErrorKind::Other
            });
        }
    }
    let mut inspection = Inspection::from(status);
    let observed = super::windows_query::query()
        .and_then(|inventory| oink_audio::microphone::virtual_output().map(|output| (inventory, output)));
    match observed {
        Ok(((installed, managed, restart_required, last_operation), output)) => {
            inspection.status.restart_required = restart_required;
            inspection.status.last_operation = last_operation;
            super::windows_presence::apply(&mut inspection, installed, managed, output);
        }
        Err(error) => {
            inspection.status.state = MicrophonePluginState::Error;
            inspection.status.error_kind = Some(MicrophonePluginErrorKind::QueryFailed);
            inspection.status.detail = Some(error);
        }
    }
    inspection
}

pub(super) fn request(
    action: Action,
    guard: Option<crate::audio::DriverChangeGuard>,
) -> Result<MicrophonePluginStatus, String> {
    let token = progress::token()?;
    let code = if action == Action::Install { 1 } else { 2 };
    OPERATION
        .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| "A microphone operation is already pending")?;
    *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let spawn = std::thread::Builder::new()
        .name("microphone-install".into())
        .spawn(move || {
            let _audio_guard = guard;
            let action = if action == Action::Install {
                "install"
            } else {
                "uninstall"
            };
            let result = run("elevate", action, &token);
            cache::progress(MicrophonePluginStep::Finish);
            if let Err(detail) = &result {
                tracing::warn!(action, %detail, "microphone installation operation failed");
            }
            *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(match result {
                Ok(_) => (
                    true,
                    "Virtual microphone operation completed; Windows may require a restart".into(),
                ),
                Err(e) => (false, e),
            });
            OPERATION.store(0, Ordering::Release);
            cache::completed();
        });
    if let Err(error) = spawn {
        OPERATION.store(0, Ordering::Release);
        return Err(error.to_string());
    }
    Ok(MicrophonePluginStatus {
        operation: if code == 1 {
            MicrophonePluginOperation::Installing
        } else {
            MicrophonePluginOperation::Uninstalling
        },
        ..Default::default()
    })
}

#[cfg(test)]
#[path = "tests/progress.rs"]
mod progress_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vendor_operations_preserve_only_available_changed_default_roles() {
        let dir = tempfile::tempdir().unwrap();
        let installer = dir.path().join("Microphone.ps1");
        let harness = dir.path().join("audio-defaults-test.ps1");
        let receipt = dir.path().join("status.json");
        std::fs::write(&installer, SCRIPT).unwrap();
        std::fs::write(&harness, include_str!("tests/audio-defaults.ps1")).unwrap();
        let output = Command::new(powershell().unwrap())
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(harness)
            .arg(installer)
            .arg(&receipt)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let changes = progress(&receipt, &"b".repeat(32)).unwrap().audio_defaults.unwrap();
        assert_eq!(changes.len(), 6);
        assert!(changes.iter().all(|change| change.outcome == "restored"));
    }
    #[test]
    fn volatile_restart_marker_survives_only_application_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let installer = dir.path().join("Microphone.ps1");
        let harness = dir.path().join("marker-test.ps1");
        std::fs::write(&installer, SCRIPT).unwrap();
        std::fs::write(&harness, include_str!("tests/volatile-marker.ps1")).unwrap();
        let output = Command::new(powershell().unwrap())
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(harness)
            .arg(installer)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(SCRIPT.matches("            MarkRestartRequired").count(), 2);
        assert_eq!(SCRIPT.matches("if ($process.ExitCode -notin @(0,3010))").count(), 2);
    }
    #[test]
    fn owned_package_cleanup_preserves_receipt_until_complete() {
        let dir = tempfile::tempdir().unwrap();
        let installer = dir.path().join("Microphone.ps1");
        let harness = dir.path().join("package-test.ps1");
        std::fs::write(&installer, SCRIPT).unwrap();
        std::fs::write(&harness, include_str!("tests/owned-package.ps1")).unwrap();
        let output = Command::new(powershell().unwrap())
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(harness)
            .arg(installer)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    #[test]
    fn embedded_installer_is_verified_pinned_and_removed_without_system_changes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bootstrap.ps1");
        let harness = dir.path().join("staging-test.ps1");
        std::fs::write(&source, BOOTSTRAP).unwrap();
        std::fs::write(&harness, include_str!("tests/embedded-staging.ps1")).unwrap();
        let output = Command::new(powershell().unwrap())
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(harness)
            .arg(source)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    #[test]
    fn bootstrap_rejects_untrusted_command_text_before_process_launch() {
        let token = "0".repeat(32);
        assert!(bootstrap("elevate", "install", &token).is_ok());
        assert!(bootstrap("execute", "uninstall", &token).is_ok());
        for (mode, action, token) in [
            ("other", "install", token.as_str()),
            ("execute", "query", token.as_str()),
            ("execute", "install';exit", token.as_str()),
            ("execute", "install", ""),
            ("execute", "install", "0000000000000000000000000000000'"),
        ] {
            assert!(bootstrap(mode, action, token).is_err());
        }
    }
    #[test]
    fn elevated_script_keeps_uac_and_hidden_verified_file_launch() {
        assert!(BOOTSTRAP.contains("-Verb RunAs -WindowStyle Hidden"));
        assert!(BOOTSTRAP.contains("'-WindowStyle','Hidden'"));
        assert!(BOOTSTRAP.contains("'-File', ('\"' + $path + '\"')"));
        assert!(!BOOTSTRAP.contains("EncodedCommand"));
        assert!(!SCRIPT.contains("EncodedCommand"));
        assert!(!SCRIPT.contains("Get-CimInstance"));
        assert!(BOOTSTRAP.contains("Protected $agent"));
        assert!(BOOTSTRAP.contains("Protected $directory"));
        assert!(BOOTSTRAP.contains("Protected $path"));
        assert!(SCRIPT.contains("Protected $progressDirectory"));
        assert!(SCRIPT.contains("Protected $progressFile"));
    }
}
