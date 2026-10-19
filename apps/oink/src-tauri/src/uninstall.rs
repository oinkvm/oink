//! The UI remains available for preparation errors. The installed native manager
//! owns final cleanup after every other process in the bundle has exited.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Only the running preparation may defer the UI's backend-loss exit.
pub(crate) fn preparing<T>(active: &AtomicBool, run: impl FnOnce() -> T) -> T {
    struct Reset<'a>(&'a AtomicBool);
    impl Drop for Reset<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    active.store(true, Ordering::Release);
    let _reset = Reset(active);
    run()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Step {
    InstalledApp,
    LeaveGroup,
    StopBackend,
    RemoveDomains,
    LoginItem,
    Finalizer,
}

fn prepare(mut run: impl FnMut(Step) -> Result<(), Step>) -> Result<(), Step> {
    for step in [
        Step::InstalledApp,
        Step::LeaveGroup,
        Step::StopBackend,
        Step::RemoveDomains,
        Step::LoginItem,
        Step::Finalizer,
    ] {
        run(step)?;
    }
    Ok(())
}

fn installed_bundle(exe: &Path) -> Option<PathBuf> {
    // The UI runs from the nested helper Contents/Helpers/Oink.app, which has the
    // same file name as the outer bundle; the installed app is the outermost one.
    let bundle = exe
        .ancestors()
        .filter(|path| path.file_name().is_some_and(|name| name == "Oink.app"))
        .last()?;
    (bundle == Path::new("/Applications/Oink.app")).then(|| bundle.into())
}

#[cfg(target_os = "macos")]
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Messages {
    title: String,
    failure: String,
    done: String,
    processes: String,
    privacy: String,
    data: String,
    trash: String,
}

#[cfg(target_os = "macos")]
pub(crate) fn start(remote: &oink_agent::remote::RemoteApi, messages: Messages) -> Result<(), Step> {
    use oink_agent::api::AgentApi;
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    let exe = std::env::current_exe().map_err(|_| Step::InstalledApp)?;
    let bundle = installed_bundle(&exe).ok_or(Step::InstalledApp)?;
    let manager = bundle.join("Contents/MacOS/oink-file-provider");
    prepare(|step| {
        let result: Result<(), String> = (|| {
            match step {
                Step::InstalledApp => {
                    if !manager.is_file()
                        || remote.data_dir()
                            != oink_foundation::paths::StoragePaths::macos(&PathBuf::from(
                                std::env::var_os("HOME").ok_or("home unavailable")?,
                            ))
                            .data
                    {
                        return Err("installed bundle and default data directory required".into());
                    }
                }
                Step::LeaveGroup => {
                    if oink_agent::instance::is_running(remote.data_dir()) == Some(true)
                        && remote.group().map_err(|e| e.to_string())?.members.len() > 1
                    {
                        remote.leave_group().map_err(|e| e.to_string())?;
                        // The existing leave path queues the signed departure on C0.
                        // Keep the transport alive briefly before draining the host.
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
                Step::StopBackend => {
                    if !oink_agent::host::stop_backend(remote.data_dir(), Duration::from_secs(15))
                        .map_err(|e| e.to_string())?
                    {
                        return Err("backend did not stop".into());
                    }
                    // A manager started by warm-up can outlive its backend.
                    // Drain these writers before remove-domain, keeping this UI.
                    let result = Command::new(&manager)
                        .args(["stop-uninstall-workers", &std::process::id().to_string()])
                        .output()
                        .map_err(|e| e.to_string())?;
                    if !result.status.success() {
                        return Err("bundle workers did not stop".into());
                    }
                }
                Step::RemoveDomains | Step::LoginItem => {
                    let command = if step == Step::RemoveDomains {
                        "remove-domain"
                    } else {
                        "unregister-login-item"
                    };
                    let result = Command::new(&manager)
                        .arg(command)
                        .output()
                        .map_err(|e| e.to_string())?;
                    if !result.status.success() {
                        return Err(format!("manager exited {}", result.status));
                    }
                }
                Step::Finalizer => {
                    use std::os::unix::process::CommandExt;
                    let mut command = Command::new(&manager);
                    command
                        .arg("finish-uninstall")
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null());
                    // SAFETY: setsid is async-signal-safe and detaches the finalizer
                    // from the UI's lifetime without invoking a shell.
                    unsafe {
                        command.pre_exec(|| {
                            if libc_setsid() == -1 {
                                Err(std::io::Error::last_os_error())
                            } else {
                                Ok(())
                            }
                        });
                    }
                    let mut child = command.spawn().map_err(|e| e.to_string())?;
                    let payload = serde_json::json!({"uiPID": std::process::id(), "messages": messages});
                    child
                        .stdin
                        .take()
                        .ok_or("missing input")?
                        .write_all(&serde_json::to_vec(&payload).map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?;
                    let mut line = String::new();
                    std::io::BufReader::new(child.stdout.take().ok_or("missing output")?)
                        .read_line(&mut line)
                        .map_err(|e| e.to_string())?;
                    if line.trim() != "ready" {
                        let _ = child.wait();
                        return Err("finalizer did not acknowledge".into());
                    }
                    // It now waits for this process to exit before touching user stores.
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
            }
            Ok(())
        })();
        result.map_err(|error| {
            tracing::warn!(?step, %error, "uninstall preparation failed");
            step
        })
    })
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    #[link_name = "setsid"]
    fn libc_setsid() -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_uninstall_restores_backend_stop_handling_at_every_step() {
        for failed in [
            Step::InstalledApp,
            Step::LeaveGroup,
            Step::StopBackend,
            Step::RemoveDomains,
            Step::LoginItem,
            Step::Finalizer,
        ] {
            let active = AtomicBool::new(false);
            let result = preparing(&active, || {
                prepare(|step| {
                    assert!(active.load(Ordering::Acquire));
                    if step == failed { Err(step) } else { Ok(()) }
                })
            });
            assert_eq!(result, Err(failed));
            assert!(
                !active.load(Ordering::Acquire),
                "{failed:?} left product quit suppressed"
            );
        }
    }

    #[test]
    fn unwinding_preparation_cannot_leave_quit_suppressed() {
        let active = AtomicBool::new(false);
        let result = std::panic::catch_unwind(|| preparing(&active, || panic!("preparation failed")));
        assert!(result.is_err());
        assert!(!active.load(Ordering::Acquire));
    }
    #[test]
    fn stops_writers_before_removal_and_hands_off_only_after_success() {
        let mut calls = Vec::new();
        prepare(|step| {
            calls.push(step);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            calls,
            [
                Step::InstalledApp,
                Step::LeaveGroup,
                Step::StopBackend,
                Step::RemoveDomains,
                Step::LoginItem,
                Step::Finalizer
            ]
        );
        for failure in calls.clone() {
            let mut seen = Vec::new();
            assert_eq!(
                prepare(|step| {
                    seen.push(step);
                    if step == failure { Err(step) } else { Ok(()) }
                }),
                Err(failure)
            );
            assert_eq!(seen.last(), Some(&failure));
            assert_eq!(seen.len(), calls.iter().position(|step| *step == failure).unwrap() + 1);
        }
    }
    #[test]
    fn only_the_running_installed_outer_bundle_is_eligible() {
        for path in [
            "/Applications/Oink.app/Contents/Helpers/Oink.app/Contents/MacOS/oink",
            "/Applications/Oink.app/Contents/MacOS/oink-backend",
        ] {
            assert_eq!(
                installed_bundle(Path::new(path)),
                Some(PathBuf::from("/Applications/Oink.app")),
                "{path}"
            );
        }
        for path in [
            "/tmp/Oink.app/Contents/MacOS/oink",
            "/Applications/Across.app/Contents/MacOS/oink",
            "/tmp/target/debug/oink",
        ] {
            assert!(installed_bundle(Path::new(path)).is_none());
        }
    }
}
