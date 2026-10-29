//! Replacement processes must outlive the terminal or UI that requested them.
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// Actual process lifetime, also exposed by backend status and debug output.
#[derive(Debug, serde::Serialize)]
pub struct Lifetime {
    pub status: &'static str,
    pub in_job: Option<bool>,
    pub job_limit_flags: Option<u32>,
    pub query_error: Option<String>,
    /// Backend token UIAccess, non-private; None off Windows or if querying failed.
    pub uiaccess: Option<bool>,
}

#[cfg(windows)]
pub fn start_backend(data_dir: &Path) -> std::io::Result<u32> {
    oink_winsvc::backend::start(data_dir)
}

pub const TIED_TO_JOB: &str = "backend lifetime tied to the launching job";

/// Query the current process, rather than trusting an inherited environment marker.
pub fn lifetime() -> Lifetime {
    #[cfg(windows)]
    {
        match current_job_limits() {
            Ok(limits) => Lifetime {
                status: if limits.is_some() { TIED_TO_JOB } else { "independent" },
                in_job: Some(limits.is_some()),
                job_limit_flags: limits,
                query_error: None,
                uiaccess: oink_winsvc::backend::uiaccess().ok(),
            },
            Err(error) => Lifetime {
                status: "backend job lifetime unknown",
                in_job: None,
                job_limit_flags: None,
                query_error: Some(error.to_string()),
                uiaccess: oink_winsvc::backend::uiaccess().ok(),
            },
        }
    }
    #[cfg(not(windows))]
    Lifetime {
        status: "independent",
        in_job: None,
        job_limit_flags: None,
        query_error: None,
        uiaccess: None,
    }
}

#[cfg(windows)]
fn current_job_limits() -> std::io::Result<Option<u32>> {
    use windows::Win32::System::JobObjects::{
        IsProcessInJob, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        QueryInformationJobObject,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    use windows::core::BOOL;
    let mut in_job = BOOL::default();
    // SAFETY: the process pseudo-handle and output buffers are valid; sizes match.
    unsafe {
        IsProcessInJob(GetCurrentProcess(), None, &mut in_job)?;
        if !in_job.as_bool() {
            return Ok(None);
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        QueryInformationJobObject(
            None,
            JobObjectExtendedLimitInformation,
            &mut limits as *mut _ as _,
            std::mem::size_of_val(&limits) as u32,
            None,
        )?;
        Ok(Some(limits.BasicLimitInformation.LimitFlags.0))
    }
}

/// Null stdio and an independent console/process group. On Windows, escape jobs
/// when permitted; restrictive jobs retain the child instead of preventing startup.
pub fn spawn_detached(command: &mut Command) -> std::io::Result<Child> {
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    let request_breakaway = {
        use windows::Win32::System::JobObjects::{JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK};
        match current_job_limits() {
            // Silent breakaway happens automatically without an explicit flag.
            Ok(Some(flags)) if flags & JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK.0 != 0 => {
                tracing::info!(job_limit_flags = flags, "launching job permits silent breakaway");
                false
            }
            Ok(Some(flags)) if flags & JOB_OBJECT_LIMIT_BREAKAWAY_OK.0 != 0 => {
                tracing::info!(job_limit_flags = flags, "launching job permits explicit breakaway");
                true
            }
            Ok(Some(flags)) => {
                tracing::warn!(job_limit_flags = flags, "{TIED_TO_JOB}");
                false
            }
            Ok(None) => {
                tracing::info!("launching process is outside a job; breakaway unnecessary");
                false
            }
            Err(error) => {
                tracing::warn!(%error, "could not query launching job; starting without breakaway");
                false
            }
        }
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows::Win32::System::Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
        };
        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        command.creation_flags(
            (if request_breakaway {
                flags | CREATE_BREAKAWAY_FROM_JOB
            } else {
                flags
            })
            .0,
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only the async-signal-safe setsid syscall runs between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let result = command.spawn();
    #[cfg(windows)]
    let result = {
        use std::os::windows::process::CommandExt;
        use windows::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        match result {
            // A restrictive outer job can still deny an inner job's breakaway.
            Err(error) if request_breakaway && error.raw_os_error() == Some(5) => {
                tracing::warn!(%error, "breakaway denied; {TIED_TO_JOB}");
                command
                    .creation_flags((DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP).0)
                    .spawn()
            }
            result => result,
        }
    };
    let child = result?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::IsProcessInJob;
        use windows::core::BOOL;
        let mut in_job = BOOL::default();
        // SAFETY: Child owns this live process handle and the output is valid.
        match unsafe { IsProcessInJob(HANDLE(child.as_raw_handle()), None, &mut in_job) } {
            Ok(()) if in_job.as_bool() => tracing::warn!(pid = child.id(), "{TIED_TO_JOB}"),
            Err(error) => tracing::warn!(pid = child.id(), %error, "could not query child job lifetime"),
            _ => {}
        }
    }
    Ok(child)
}

/// Relaunch the app entry. macOS delegates to LaunchServices instead of executing
/// Contents/MacOS directly; -n lets the replacement wait for the old pair to exit.
pub fn reopen_entry(entry: &Path, data_dir: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let macos = entry.parent().filter(|p| p.file_name().is_some_and(|n| n == "MacOS"));
        let contents = macos
            .and_then(Path::parent)
            .filter(|p| p.file_name().is_some_and(|n| n == "Contents"));
        let bundle = contents
            .and_then(Path::parent)
            .filter(|p| p.extension().is_some_and(|e| e == "app"));
        let bundle = bundle.ok_or_else(|| std::io::Error::other("重新打开需要 macOS 应用包"))?;
        let status = mac_reopen_command(bundle, data_dir).status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!("LaunchServices 启动失败：{status}")));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = entry;
        oink_winsvc::backend::reopen(data_dir)?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        spawn_detached(
            Command::new(entry)
                .arg(super::REOPEN_FLAG)
                .env("OINK_HOME", data_dir)
                .env_remove(oink_autostart::LOGIN_LAUNCH_ENV),
        )?;
        Ok(())
    }
}

/// LaunchServices does not inherit the caller's environment. Every explicit
/// reopen, including a local package replacement, must preserve its data root
/// and clear the login-item marker so the main window appears.
#[cfg(target_os = "macos")]
pub(crate) fn mac_reopen_command(bundle: &Path, data_dir: &Path) -> Command {
    let mut home = std::ffi::OsString::from("OINK_HOME=");
    home.push(data_dir);
    let mut command = Command::new("/usr/bin/open");
    command
        .arg("-n")
        .arg(bundle)
        .args([
            "--stdin",
            "/dev/null",
            "--stdout",
            "/dev/null",
            "--stderr",
            "/dev/null",
            "--env",
        ])
        .arg(home)
        .arg("--env")
        .arg(format!("{}=0", oink_autostart::LOGIN_LAUNCH_ENV))
        .args(["--args", super::REOPEN_FLAG])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}
