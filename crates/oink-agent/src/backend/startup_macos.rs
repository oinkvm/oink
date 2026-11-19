//! Pre-window startup diagnostics shared by the outer application and its UI helper.
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    os::unix::net::UnixDatagram,
    path::Path,
    ptr::NonNull,
    time::{Duration, Instant},
};

use crate::ipc::{BuildIdentity, Client, OtherBuild};

unsafe extern "C" {
    fn oink_startup_alert(
        screen: c_int,
        zh: bool,
        exit_code: c_int,
        detail: *const c_char,
        version: *const c_char,
        logs: *const c_char,
    ) -> c_int;
    fn oink_launch_background(
        bundle: *const c_char,
        home: *const c_char,
        reply: *const c_char,
        failure: *mut *mut c_char,
    ) -> *mut c_void;
    fn oink_launch_exited(process: *mut c_void) -> bool;
    fn oink_release_launch(process: *mut c_void);
}

fn cstring(value: &str) -> CString {
    CString::new(value.replace('\0', "�")).unwrap()
}

fn bundle(exe: &Path) -> Option<&Path> {
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|x| x == "app") && !p.ends_with("Contents/Helpers/Oink.app"))
}

#[derive(Debug, thiserror::Error)]
#[error("LaunchServices could not start the embedded UI helper: {0}")]
pub(super) struct HelperLaunchError(pub String);

fn screen(bundle: Option<&Path>, home: Option<&Path>, mismatch: bool) -> i32 {
    if let Some(path) = bundle
        && (!(path.starts_with("/Applications") || home.is_some_and(|h| path.starts_with(h.join("Applications"))))
            || path.components().any(|c| c.as_os_str() == "AppTranslocation"))
    {
        return 8;
    }
    if mismatch {
        return 9;
    }
    if bundle.is_some_and(|path| {
        [
            "Contents/MacOS/oink-backend",
            "Contents/Helpers/Oink.app/Contents/MacOS/oink",
            "Contents/Helpers/Oink.app/Contents/Info.plist",
            "Contents/Info.plist",
            "Contents/MacOS/oink-file-provider",
            "Contents/PlugIns/OinkFileProvider.appex/Contents/MacOS/OinkFileProvider",
            "Contents/Helpers/OinkPaste.app/Contents/MacOS/OinkPaste",
            "Contents/PlugIns/OinkFinderSync.appex/Contents/MacOS/OinkFinderSync",
        ]
        .iter()
        .any(|file| matches!(path.join(file).try_exists(), Ok(false)))
    }) {
        return 10;
    }
    11
}

#[derive(Debug, thiserror::Error)]
#[error("oink-backend exited with code {0}")]
struct BackendExit(u8);

fn alert(kind: i32, detail: &str, exit: Option<&BackendExit>) -> bool {
    let build = BuildIdentity::current();
    let version = cstring(&format!("{} ({})", build.version, build.build));
    let detail = cstring(detail);
    let logs = cstring(&oink_foundation::paths::log_dir().to_string_lossy());
    use crate::config::{AgentConfig, UiLanguage};
    let system = objc2_foundation::NSLocale::preferredLanguages()
        .firstObject()
        .map_or_else(String::new, |s| s.to_string());
    let lang = AgentConfig::load().map_or(UiLanguage::Auto, |c| c.settings.ui_language);
    let zh = lang.resolve(&system) == UiLanguage::SimplifiedChinese;
    // SAFETY: strings remain live through the synchronous main-thread modal.
    unsafe {
        oink_startup_alert(
            kind,
            zh,
            exit.map_or(-1, |e| i32::from(e.0)),
            detail.as_ptr(),
            version.as_ptr(),
            logs.as_ptr(),
        ) != 0
    }
}

pub(super) fn diagnose(error: &anyhow::Error) -> bool {
    let exe = std::env::current_exe().ok();
    let kind = screen(exe.as_deref().and_then(bundle), std::env::home_dir().as_deref(), false);
    let kind = if kind == 11 && error.is::<HelperLaunchError>() {
        10
    } else {
        kind
    };
    tracing::error!(%error, kind, "macOS startup failed");
    alert(kind, &format!("{error:#}"), error.downcast_ref())
}

struct Launched {
    app: NonNull<c_void>,
    reply: UnixDatagram,
    _directory: tempfile::TempDir,
}
impl Drop for Launched {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the retained NSRunningApplication.
        unsafe { oink_release_launch(self.app.as_ptr()) }
    }
}
fn launch(dir: &Path) -> anyhow::Result<Launched> {
    let exe = std::env::current_exe()?;
    let path = bundle(&exe).ok_or_else(|| anyhow::anyhow!("outer application bundle unavailable"))?;
    std::fs::metadata(path.join("Contents/MacOS/oink-backend"))?;
    let path = cstring(&path.to_string_lossy());
    let home = cstring(&dir.to_string_lossy());
    // LaunchServices owns the process. On macOS 26 even a same-user
    // EVFILT_PROC/NOTE_EXITSTATUS watch did not yield its exit status. Have
    // this launch send one byte before exiting; process death without a byte
    // still reports termination. A private short path fits sockaddr_un.
    let directory = tempfile::tempdir_in("/tmp")?;
    let reply_path = directory.path().join("status");
    let reply = UnixDatagram::bind(&reply_path)?;
    reply.set_nonblocking(true)?;
    let reply_path = cstring(&reply_path.to_string_lossy());
    let mut failure = std::ptr::null_mut();
    // SAFETY: synchronous native call returns one retained object or a strdup error.
    let process = unsafe { oink_launch_background(path.as_ptr(), home.as_ptr(), reply_path.as_ptr(), &mut failure) };
    if let Some(process) = NonNull::new(process) {
        return Ok(Launched {
            app: process,
            reply,
            _directory: directory,
        });
    }
    anyhow::ensure!(!failure.is_null(), "LaunchServices could not allocate error detail");
    // SAFETY: failure is a non-null strdup result, owned by this call.
    let error = unsafe {
        let text = CStr::from_ptr(failure).to_string_lossy().into_owned();
        libc::free(failure.cast());
        text
    };
    anyhow::bail!(error)
}

enum Owner {
    Ready,
    Restart,
    Cancel,
}

fn probe(dir: &Path, mut process: Option<&Launched>) -> anyhow::Result<Owner> {
    // Use the existing IPC Client hello budget for the whole publication handshake.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = Client::connect(dir).and_then(|client| {
            let other = match client.hello_with_timeout(remaining) {
                Ok(info) if info.build == BuildIdentity::current() => return Ok(Owner::Ready),
                Ok(info) => info.build,
                Err(error) => error.downcast::<OtherBuild>()?.0,
            };
            // Keep the authenticated endpoint across the modal. A replacement
            // backend cannot receive a stop addressed to the old owner's token.
            confirm_other(dir, client, other)
        });
        let error = match result {
            Ok(owner) => return Ok(owner),
            Err(error) => error,
        };
        let pending = error.downcast_ref::<std::io::Error>().is_some_and(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        });
        if !pending || Instant::now() >= deadline {
            return Err(error);
        }
        if let Some(p) = process {
            // SAFETY: Launched retains this application until after the probe.
            let exited = unsafe { oink_launch_exited(p.app.as_ptr()) };
            let mut code = [0];
            if p.reply.recv(&mut code).ok() == Some(1) {
                if code[0] == 0 && crate::instance::is_running(dir) == Some(true) {
                    process = None;
                    continue;
                }
                return Err(BackendExit(code[0]).into());
            }
            if exited {
                anyhow::bail!("oink-backend terminated before IPC hello: {error:#}");
            }
        } else if crate::instance::is_running(dir) != Some(true) {
            anyhow::bail!("oink-backend exited before IPC hello: {error:#}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn confirm_other(dir: &Path, client: Client, build: BuildIdentity) -> anyhow::Result<Owner> {
    let exe = std::env::current_exe()?;
    let kind = screen(bundle(&exe), std::env::home_dir().as_deref(), true);
    if !alert(kind, &format!("{} ({})", build.version, build.build), None) {
        return Ok(Owner::Cancel);
    }
    client.stop()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match (crate::instance::is_running(dir), crate::instance::is_ui_running(dir)) {
            (Some(false), Some(false)) => return Ok(Owner::Restart),
            (None, _) | (_, None) => anyhow::bail!("previous Oink ownership unavailable"),
            _ if Instant::now() >= deadline => anyhow::bail!("previous Oink has not finished quitting"),
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Called by the UI on its main thread before Tauri creates any windows.
pub fn connect_ui(dir: &Path) -> bool {
    let mut native_recovery = false;
    loop {
        let result = (|| {
            let process = match crate::instance::acquire(dir) {
                Ok(lock) => {
                    drop(lock);
                    native_recovery = true;
                    Some(launch(dir)?)
                }
                Err(error) if error.is::<crate::instance::AlreadyRunning>() => None,
                Err(error) => return Err(error),
            };
            probe(dir, process.as_ref())
        })();
        match result {
            Ok(Owner::Ready) if native_recovery => {
                // Tao requires its NSApplication subclass to be the very first
                // sharedApplication in the process. A pre-Tauri alert or a
                // LaunchServices recovery has already initialized AppKit.
                // Preserve this LaunchServices PID and start with fresh globals.
                let error = restart_process();
                if !diagnose(&error.into()) {
                    return false;
                }
            }
            Ok(Owner::Ready) => return true,
            Ok(Owner::Cancel) => return false,
            Ok(Owner::Restart) => native_recovery = true,
            Err(error) if diagnose(&error) => native_recovery = true,
            Err(_) => return false,
        }
    }
}

pub(super) fn restart_process() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe).args(std::env::args_os().skip(1)).exec(),
        Err(error) => error,
    }
}

pub(super) fn existing_owner(dir: &Path) -> anyhow::Result<()> {
    match probe(dir, None)? {
        Owner::Ready => {
            use crate::api::AgentApi;
            crate::remote::RemoteApi::new(dir).show_window()?;
            Ok(())
        }
        Owner::Cancel => Ok(()),
        Owner::Restart => Err(restart_process().into()),
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn classifies_outer_application_and_worker_failure() {
        let home = tempfile::tempdir().unwrap();
        let app = home.path().join("Applications/Test.app");
        let nested = app.join("Contents/Helpers/Oink.app/Contents/MacOS/oink");
        assert_eq!(bundle(&nested), Some(app.as_path()));
        let classify = |path, mismatch| screen(Some(path), Some(home.path()), mismatch);
        assert_eq!(classify(Path::new("/Volumes/Test/Test.app"), false), 8);
        assert_eq!(
            classify(Path::new("/Applications/AppTranslocation/test/Test.app"), false),
            8
        );
        assert_eq!(classify(&app, true), 9);
        assert_eq!(classify(&app, false), 10);
        assert_eq!(screen(None, None, false), 11);
    }
}
