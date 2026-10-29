use serde::{Deserialize, Serialize};
use std::path::Path;

const MARKER: &str = "file-provider-launch.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum LaunchAction {
    Suspend,
    Register,
    Reset,
    Recover,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchState {
    schema: u32,
    version: String,
    build: String,
    pending: Option<LaunchAction>,
    reset_token: Option<String>,
}

fn plan_launch(data: &Path, version: &str, build: &str) -> anyhow::Result<Option<LaunchState>> {
    let marker = data.join(MARKER);
    let previous: Option<LaunchState> = match std::fs::read(&marker) {
        Ok(bytes) => match serde_json::from_slice::<LaunchState>(&bytes) {
            Ok(state) if state.schema == 1 => Some(state),
            Ok(_) => {
                tracing::warn!("discarding unknown File Provider launch state schema");
                None
            }
            Err(error) => {
                tracing::warn!(%error, "discarding unreadable File Provider launch state");
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut fresh = false;
    reset_if_fresh(data, || {
        fresh = true;
        Ok(())
    })?;
    let action = if fresh
        || previous
            .as_ref()
            .is_some_and(|s| s.pending == Some(LaunchAction::Reset))
    {
        LaunchAction::Reset
    } else if previous
        .as_ref()
        .is_some_and(|s| s.version == version && s.build == build && s.pending.is_none())
    {
        return Ok(None);
    } else {
        LaunchAction::Recover
    };
    // A fresh-data reset is retried with the same identity even if the manager
    // completed but the host died before committing its launch marker. Otherwise
    // a retry could wipe new local files created after the native reset finished.
    let reset_token = if action == LaunchAction::Reset {
        match previous
            .as_ref()
            .filter(|s| s.pending == Some(LaunchAction::Reset))
            .and_then(|s| s.reset_token.clone())
        {
            Some(token) => Some(token),
            None => {
                let mut bytes = [0u8; 16];
                getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("reset identity: {e}"))?;
                Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
            }
        }
    } else {
        None
    };
    let state = LaunchState {
        schema: 1,
        version: version.into(),
        build: build.into(),
        pending: Some(action),
        reset_token,
    };
    // Persist before OS work: a failed fresh reset must remain a reset even if
    // the rest of the app creates its identity while File Provider is unavailable.
    oink_foundation::storage::atomic_write(&marker, &serde_json::to_vec(&state)?)?;
    Ok(Some(state))
}

fn finish_launch(
    data: &Path,
    mut state: LaunchState,
    mut run: impl FnMut(LaunchAction) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // An installer may already have reconnected the replaced bundle. Quiesce
    // that domain before changing LS/PlugInKit registrations: macOS 26 can
    // otherwise open both extension generations against the same database.
    for action in [
        LaunchAction::Suspend,
        LaunchAction::Register,
        state.pending.expect("planned launch action"),
    ] {
        tracing::info!(?action, "File Provider launch preparation");
        let started = std::time::Instant::now();
        let result = run(action);
        tracing::info!(
            ?action,
            elapsed_ms = started.elapsed().as_millis() as u64,
            success = result.is_ok(),
            "File Provider launch stage completed"
        );
        result?;
    }
    // Recover verifies namespace readiness without waiting for retained content
    // transfers. Failed preparation must leave this intent durable.
    state.pending = None;
    oink_foundation::storage::atomic_write(&data.join(MARKER), &serde_json::to_vec(&state)?)?;
    Ok(())
}

#[cfg(test)]
fn prepare_launch(
    data: &Path,
    version: &str,
    build: &str,
    run: impl FnMut(LaunchAction) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if let Some(state) = plan_launch(data, version, build)? {
        finish_launch(data, state, run)?;
    }
    Ok(())
}

static FAILURE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
pub(super) fn failure() -> Option<String> {
    FAILURE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(target_os = "macos")]
fn set_failure(failure: Option<String>) {
    let mut previous = FAILURE.lock().unwrap_or_else(|e| e.into_inner());
    if *previous != failure {
        *previous = failure;
        oink_clipboard::file_provider::demand_changed().notify_one();
    }
}

// Only Recover may reach this path, after Suspend and Register succeeded.
// Rechecking must neither disconnect a live domain nor replay a fresh-data reset.
fn recheck_recovery(
    result: anyhow::Result<()>,
    mut check: impl FnMut() -> anyhow::Result<()>,
    mut wait: impl FnMut() -> anyhow::Result<()>,
    mut pending: impl FnMut(&anyhow::Error),
) -> anyhow::Result<()> {
    let Err(error) = result else { return Ok(()) };
    pending(&error);
    loop {
        match check() {
            Ok(()) => return Ok(()),
            Err(error) => pending(&error),
        }
        wait()?;
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct Worker {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancelled.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) fn reset_if_fresh(data: &Path, mut reset: impl FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
    // Directory existence is insufficient: logs and the instance lock precede
    // identity creation. Only a missing identity denotes a fresh installation.
    match std::fs::symlink_metadata(data.join("device.key")) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => reset()?,
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn prepare(data: &Path) -> anyhow::Result<Option<Worker>> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME unavailable"))?;
    let installed = Path::new("/Applications/Oink.app/Contents/MacOS");
    // Isolated instances and bare development executables never manipulate the
    // installed provider's namespace. The registered manager is required by macOS.
    if data != oink_foundation::paths::StoragePaths::macos(&std::path::PathBuf::from(home)).data
        || std::env::current_exe()?.parent() != Some(installed)
    {
        return Ok(None);
    }
    // Choose and persist Reset before identity generation. The expensive native
    // work runs independently of IPC/window startup, while clipboard's gate
    // prevents container adoption, warm-up, publication and domain queries.
    let Some(state) = plan_launch(data, env!("OINK_RELEASE_VERSION"), env!("OINK_RELEASE_BUILD"))? else {
        return Ok(None);
    };
    oink_clipboard::file_provider::begin_launch_preparation();
    let data = data.to_owned();
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = cancelled.clone();
    let thread = std::thread::Builder::new()
        .name("oink-provider-launch".into())
        .spawn(move || {
            crate::telemetry::fatal_boundary(move || {
                let result = prepare_native(&data, state, &|| !stopping.load(std::sync::atomic::Ordering::Acquire));
                let failure = result.err().map(|error| {
                    tracing::error!(%error, "File Provider launch preparation failed; retry on next launch");
                    format!("File Provider launch recovery failed: {error}. Quit and reopen Oink to retry.")
                });
                let success = failure.is_none();
                set_failure(failure);
                oink_clipboard::file_provider::finish_launch_preparation(success);
                tracing::info!(success, "File Provider launch preparation completed");
            })
        })?;
    Ok(Some(Worker {
        cancelled,
        thread: Some(thread),
    }))
}

#[cfg(target_os = "macos")]
fn prepare_native(data: &Path, state: LaunchState, current: &dyn Fn() -> bool) -> anyhow::Result<()> {
    let installed = Path::new("/Applications/Oink.app/Contents/MacOS");
    let app = installed.parent().unwrap().parent().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(85);
    let reset_token = state.reset_token.clone();
    finish_launch(data, state, |action| {
        match action {
            LaunchAction::Suspend => {
                let (status, _) = run(
                    &installed.join("oink-file-provider"),
                    &["prepare-update".as_ref()],
                    std::time::Instant::now() + std::time::Duration::from_secs(15),
                    current,
                )?;
                // A fresh, not-yet-registered extension has no live domain to
                // suspend. Other failures must not authorize registration.
                anyhow::ensure!(
                    status.success() || status.code() == Some(6),
                    "File Provider suspension failed ({status})"
                );
                Ok(())
            }
            LaunchAction::Register => register_provider(
                app,
                std::time::Instant::now() + std::time::Duration::from_secs(20),
                current,
            ),
            LaunchAction::Reset | LaunchAction::Recover => {
                let command = if action == LaunchAction::Reset {
                    "reset-store"
                } else {
                    "recover-launch"
                };
                let end = deadline;
                let mut arguments = vec![std::ffi::OsStr::new(command)];
                if action == LaunchAction::Reset {
                    arguments.push(std::ffi::OsStr::new(
                        reset_token
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("missing reset identity"))?,
                    ));
                }
                let result = (|| {
                    loop {
                        let output = run(&installed.join("oink-file-provider"), &arguments, end, current)?;
                        if output.0.success() {
                            return Ok(());
                        }
                        // Registration may still be settling after a bundle move.
                        anyhow::ensure!(
                            output.0.code() == Some(6),
                            "File Provider {command} failed ({})",
                            output.0
                        );
                        pause(end, current)?;
                    }
                })();
                if action == LaunchAction::Reset {
                    return result;
                }
                let mut interval = std::time::Duration::from_secs(5);
                recheck_recovery(
                    result,
                    || {
                        let (status, _) = run(
                            &installed.join("oink-file-provider"),
                            &["check-launch".as_ref()],
                            std::time::Instant::now() + std::time::Duration::from_secs(15),
                            current,
                        )?;
                        anyhow::ensure!(status.success(), "File Provider readiness check failed ({status})");
                        tracing::info!("File Provider launch readiness recovered");
                        Ok(())
                    },
                    || {
                        let until = std::time::Instant::now() + interval;
                        loop {
                            anyhow::ensure!(current(), "File Provider readiness check cancelled");
                            let remaining = until.saturating_duration_since(std::time::Instant::now());
                            if remaining.is_zero() {
                                break;
                            }
                            std::thread::sleep(remaining.min(std::time::Duration::from_millis(100)));
                        }
                        interval = (interval * 2).min(std::time::Duration::from_secs(30));
                        Ok(())
                    },
                    |error| {
                        set_failure(Some(format!(
                            "File Provider launch recovery delayed: {error}. Checking domain readiness automatically."
                        )));
                    },
                )
            }
        }
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn run(
    program: &Path,
    args: &[&std::ffi::OsStr],
    deadline: std::time::Instant,
    current: &dyn Fn() -> bool,
) -> anyhow::Result<(std::process::ExitStatus, String)> {
    use std::io::Read;
    anyhow::ensure!(
        current() && std::time::Instant::now() < deadline,
        "File Provider launch operation timed out"
    );
    let mut output = tempfile::tempfile()?;
    let mut errors = tempfile::tempfile()?;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(errors.try_clone()?)
        .spawn()?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if current() && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                if let Err(error) = result {
                    return Err(error.into());
                }
                anyhow::bail!("File Provider launch operation timed out");
            }
        }
    };
    std::io::Seek::rewind(&mut output)?;
    let mut bytes = String::new();
    output.take(1024 * 1024).read_to_string(&mut bytes)?;
    if !status.success() {
        std::io::Seek::rewind(&mut errors)?;
        let mut detail = Vec::new();
        errors.take(4096).read_to_end(&mut detail)?;
        tracing::warn!(tool = ?program.file_name(), %status, stderr = %String::from_utf8_lossy(&detail).trim(),
            "File Provider launch tool failed");
    }
    Ok((status, bytes))
}

#[cfg(target_os = "macos")]
fn pause(deadline: std::time::Instant, current: &dyn Fn() -> bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        current() && std::time::Instant::now() < deadline,
        "File Provider registration timed out"
    );
    std::thread::sleep(
        std::time::Duration::from_millis(250).min(deadline.saturating_duration_since(std::time::Instant::now())),
    );
    Ok(())
}

fn registration_paths(output: &str) -> std::collections::BTreeSet<std::path::PathBuf> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim_start_matches([' ', '+', '-', '=', '!', '?']);
            if !line.starts_with("com.hachiwii.oink.fileprovider(") {
                return None;
            }
            let path = line.split('\t').next_back()?.trim();
            (path.starts_with('/') && path.ends_with(".appex")).then(|| path.into())
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn register_provider(app: &Path, deadline: std::time::Instant, current: &dyn Fn() -> bool) -> anyhow::Result<()> {
    let own = app.join("Contents/PlugIns/OinkFileProvider.appex");
    anyhow::ensure!(own.is_dir(), "Oink File Provider extension is missing");
    let checked = |program: &Path, args: &[&std::ffi::OsStr]| -> anyhow::Result<String> {
        let (status, output) = run(program, args, deadline, current)?;
        // A query may find no provider yet while automatic registration settles.
        anyhow::ensure!(
            status.success() || (args.first() == Some(&std::ffi::OsStr::new("-m")) && status.code() == Some(1)),
            "File Provider registration command failed ({status})"
        );
        Ok(output)
    };
    register_provider_with(app, checked, || pause(deadline, current))
}

fn register_provider_with(
    app: &Path,
    mut checked: impl FnMut(&Path, &[&std::ffi::OsStr]) -> anyhow::Result<String>,
    mut wait: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let plugin = Path::new("/usr/bin/pluginkit");
    let own = app.join("Contents/PlugIns/OinkFileProvider.appex");
    // macOS 26 kept a killed extension's working-set stream throttled after a
    // drag/Replace update; resolving new URLs or signalling did not recover it.
    // LaunchServices and PlugInKit also settle asynchronously after a move, and
    // a mounted copy of this same extension can win provider selection. Register
    // the running installed bundle, remove ONLY other registrations of Oink's
    // exact extension ID, then verify selection before reconnecting its domain. Do
    // not elect "use": that would override the user's disabled extension. The
    // disconnect/reconnect is separate and preserves domain identity/content.
    // The triggering throttle was observed on macOS 26; macOS 14/26 installed
    // manual-update acceptance is required for this launch workaround.
    let selected_args = [
        "-m".as_ref(),
        "-v".as_ref(),
        "-i".as_ref(),
        "com.hachiwii.oink.fileprovider".as_ref(),
    ];
    let own_selected =
        registration_paths(&checked(plugin, &selected_args)?) == std::collections::BTreeSet::from([own.clone()]);
    let all = checked(
        plugin,
        &[
            "-m".as_ref(),
            "-A".as_ref(),
            "-D".as_ref(),
            "-v".as_ref(),
            "-i".as_ref(),
            "com.hachiwii.oink.fileprovider".as_ref(),
        ],
    )?;
    let paths = registration_paths(&all);
    // Bundle replacement/installer registration may already have selected us.
    // Re-registering that exact selection invalidates the live extension again;
    // on macOS 26 overlapping database generations can remain permanently busy.
    if own_selected && paths == std::collections::BTreeSet::from([own.clone()]) {
        return Ok(());
    }
    for path in paths {
        if path != own {
            checked(plugin, &["-r".as_ref(), path.as_os_str()])?;
        }
    }
    if !own_selected {
        checked(
            Path::new(
                "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister",
            ),
            &["-f".as_ref(), app.as_os_str()],
        )?;
        checked(plugin, &["-a".as_ref(), own.as_os_str()])?;
    }
    loop {
        // Query while asynchronous registration settles; repeatedly adding the
        // same extension restarts the very transition we are waiting to finish.
        let selected = checked(plugin, &selected_args)?;
        if registration_paths(&selected) == std::collections::BTreeSet::from([own.clone()]) {
            return Ok(());
        }
        wait()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertain_recover_rechecks_readiness_without_repeating_registration_or_needing_content() {
        use std::cell::{Cell, RefCell};
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device.key"), [0; 32]).unwrap();
        let state = plan_launch(dir.path(), "0.3.0", "59").unwrap().unwrap();
        let bridge_started = Cell::new(false);
        let retained_reads = Cell::new(109);
        let checks = Cell::new(0);
        let waits = Cell::new(0);
        let errors = RefCell::new(vec![]);
        let mut actions = vec![];
        finish_launch(dir.path(), state, |action| {
            actions.push(action);
            if action != LaunchAction::Recover {
                return Ok(());
            }
            recheck_recovery(
                Err(anyhow::anyhow!("helper timed out after reconnect")),
                || {
                    let marker: LaunchState = serde_json::from_slice(&std::fs::read(dir.path().join(MARKER))?)?;
                    assert_eq!(marker.pending, Some(LaunchAction::Recover));
                    assert!(!bridge_started.get());
                    assert_eq!(retained_reads.get(), 109);
                    checks.set(checks.get() + 1);
                    // The native domain first reports disconnected, then has
                    // flags but no root URL, then becomes genuinely usable.
                    match checks.get() {
                        1 => anyhow::bail!("disconnected"),
                        2 => anyhow::bail!("root URL unavailable"),
                        _ => Ok(()),
                    }
                },
                || {
                    waits.set(waits.get() + 1);
                    Ok(())
                },
                |error| errors.borrow_mut().push(error.to_string()),
            )
        })
        .unwrap();
        assert_eq!(
            actions,
            [LaunchAction::Suspend, LaunchAction::Register, LaunchAction::Recover]
        );
        assert_eq!(waits.get(), 2);
        assert_eq!(checks.get(), 3);
        assert_eq!(errors.borrow().len(), 3);
        assert!(plan_launch(dir.path(), "0.3.0", "59").unwrap().is_none());
        // Completion releases the bridge despite outstanding work, so that
        // work can now consume it. No daemon reset or process relaunch occurred.
        bridge_started.set(true);
        retained_reads.set(retained_reads.get() - 1);
        assert!(bridge_started.get() && retained_reads.get() == 108);
    }

    #[test]
    fn cancelled_readiness_recheck_keeps_recover_marker_and_does_not_claim_success() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device.key"), [0; 32]).unwrap();
        let state = plan_launch(dir.path(), "0.3.0", "59").unwrap().unwrap();
        let result = finish_launch(dir.path(), state, |action| {
            if action != LaunchAction::Recover {
                return Ok(());
            }
            recheck_recovery(
                Err(anyhow::anyhow!("timeout")),
                || anyhow::bail!("still unavailable"),
                || anyhow::bail!("host stopped"),
                |_| {},
            )
        });
        assert_eq!(result.unwrap_err().to_string(), "host stopped");
        assert_eq!(
            plan_launch(dir.path(), "0.3.0", "59").unwrap().unwrap().pending,
            Some(LaunchAction::Recover)
        );
        recheck_recovery(
            Ok(()),
            || panic!("successful recovery rechecked"),
            || panic!("successful recovery delayed"),
            |_| panic!("successful recovery reported an error"),
        )
        .unwrap();
    }

    #[test]
    fn already_selected_installed_provider_is_not_registered_again() {
        let app = Path::new("/Applications/Oink.app");
        let own = app.join("Contents/PlugIns/OinkFileProvider.appex");
        let mut mutations = vec![];
        register_provider_with(
            app,
            |program, args| {
                if args.first() == Some(&std::ffi::OsStr::new("-m")) {
                    return Ok(format!(
                        " com.hachiwii.oink.fileprovider(0.2.0)\tUUID\tDATE\t{}\n",
                        own.display()
                    ));
                }
                mutations.push((
                    program.to_owned(),
                    args.iter().map(|s| s.to_os_string()).collect::<Vec<_>>(),
                ));
                Ok(String::new())
            },
            || panic!("selected registration is already current"),
        )
        .unwrap();
        assert!(
            mutations.is_empty(),
            "re-registering a live selected extension can invalidate its database: {mutations:?}"
        );
    }

    #[test]
    fn host_exit_cancels_and_reaps_an_inflight_launch_tool() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let stopping = cancelled.clone();
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let child_ready = ready.clone();
        let thread = std::thread::spawn(move || {
            let error = run(
                Path::new("/bin/sh"),
                &[
                    "-c".as_ref(),
                    "printf ready > \"$1\"; exec /bin/sleep 10".as_ref(),
                    "oink-provider-test".as_ref(),
                    child_ready.as_os_str(),
                ],
                std::time::Instant::now() + std::time::Duration::from_secs(10),
                &|| !stopping.load(Ordering::Acquire),
            )
            .unwrap_err();
            assert!(error.to_string().contains("timed out"));
        });
        let worker = Worker {
            cancelled,
            thread: Some(thread),
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !ready.exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let start = std::time::Instant::now();
        drop(worker);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn hung_launch_tool_is_killed_at_its_deadline() {
        let error = run(
            Path::new("/bin/sleep"),
            &["10".as_ref()],
            std::time::Instant::now() + std::time::Duration::from_millis(50),
            &|| true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn registration_prunes_only_oink_duplicates_then_waits_for_own_selection() {
        let app = Path::new("/Applications/Oink.app");
        let own = app.join("Contents/PlugIns/OinkFileProvider.appex");
        let duplicate = "/Volumes/Oink Update/Oink.app/Contents/PlugIns/OinkFileProvider.appex";
        let row = |path: &str| format!("-    com.hachiwii.oink.fileprovider(0.1.0)\tUUID\tDATE\t{path}\n");
        let all = row(own.to_str().unwrap())
            + &row(duplicate)
            + "     com.example.other(1.0)\tUUID\tDATE\t/Applications/Other.appex\n (3 plug-ins)\n";
        let mut calls = vec![];
        let mut selected_queries = 0;
        let mut waits = 0;
        register_provider_with(
            app,
            |program, args| {
                let args: Vec<_> = args.iter().map(|s| s.to_string_lossy().into_owned()).collect();
                calls.push((program.to_path_buf(), args.clone()));
                if args.contains(&"-A".into()) {
                    return Ok(all.clone());
                }
                if args.first().map(String::as_str) == Some("-m") {
                    selected_queries += 1;
                    return Ok(if selected_queries <= 2 {
                        row(duplicate)
                    } else {
                        row(own.to_str().unwrap())
                    });
                }
                Ok(String::new())
            },
            || {
                waits += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(waits, 1);
        let removals: Vec<_> = calls
            .iter()
            .filter(|(_, args)| args.first().map(String::as_str) == Some("-r"))
            .collect();
        assert_eq!(removals.len(), 1);
        assert_eq!(removals[0].1, ["-r", duplicate]);
        assert!(calls.iter().all(|(_, args)| !args.contains(&"-e".into())));
        assert_eq!(
            calls
                .iter()
                .filter(|(_, args)| args.first().map(String::as_str) == Some("-a"))
                .count(),
            1
        );
    }

    #[test]
    fn registration_wait_failure_is_reported_without_accessing_a_domain() {
        let result = register_provider_with(
            Path::new("/Applications/Oink.app"),
            |_, _| Ok(String::new()),
            || anyhow::bail!("registration timed out"),
        );
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    fn launch(
        data: &Path,
        build: &str,
        calls: &mut Vec<LaunchAction>,
        fail: Option<LaunchAction>,
    ) -> anyhow::Result<()> {
        prepare_launch(data, "0.1.0-beta.1", build, |action| {
            calls.push(action);
            if Some(action) == fail {
                anyhow::bail!("injected failure");
            }
            Ok(())
        })
    }

    #[test]
    fn new_build_recovers_once_and_same_build_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device.key"), [0; 32]).unwrap();
        let mut calls = vec![];
        launch(dir.path(), "18", &mut calls, None).unwrap();
        assert_eq!(
            calls,
            [LaunchAction::Suspend, LaunchAction::Register, LaunchAction::Recover]
        );
        calls.clear();
        launch(dir.path(), "18", &mut calls, None).unwrap();
        assert!(calls.is_empty());
        launch(dir.path(), "19", &mut calls, None).unwrap();
        assert_eq!(
            calls,
            [LaunchAction::Suspend, LaunchAction::Register, LaunchAction::Recover]
        );
        calls.clear();
        prepare_launch(dir.path(), "0.1.0", "19", |action| {
            calls.push(action);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            calls,
            [LaunchAction::Suspend, LaunchAction::Register, LaunchAction::Recover]
        );
    }

    #[test]
    fn suspension_precedes_registration_and_failures_stay_retryable() {
        for failure in [LaunchAction::Suspend, LaunchAction::Register, LaunchAction::Recover] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("device.key"), [0; 32]).unwrap();
            let mut calls = vec![];
            assert!(launch(dir.path(), "19", &mut calls, Some(failure)).is_err());
            assert_eq!(calls.last(), Some(&failure));
            let pending: LaunchState =
                serde_json::from_slice(&std::fs::read(dir.path().join(MARKER)).unwrap()).unwrap();
            assert_eq!(
                pending.pending,
                Some(LaunchAction::Recover),
                "failed namespace recovery must remain retryable in the same build"
            );
            assert_eq!(
                calls.len(),
                match failure {
                    LaunchAction::Suspend => 1,
                    LaunchAction::Register => 2,
                    _ => 3,
                }
            );
            calls.clear();
            launch(dir.path(), "19", &mut calls, None).unwrap();
            assert_eq!(
                calls,
                [LaunchAction::Suspend, LaunchAction::Register, LaunchAction::Recover]
            );
            calls.clear();
            launch(dir.path(), "19", &mut calls, None).unwrap();
            assert!(calls.is_empty());
        }
    }

    #[test]
    fn deferred_fresh_reset_keeps_its_decision_after_identity_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let state = plan_launch(dir.path(), "0.1.0-beta.1", "19").unwrap().unwrap();
        assert_eq!(state.pending, Some(LaunchAction::Reset));
        // The main startup can now generate identity and publish IPC while the
        // native worker is delayed; it must not turn Reset into Recover.
        std::fs::write(dir.path().join("device.key"), [0; 32]).unwrap();
        let mut calls = vec![];
        finish_launch(dir.path(), state, |action| {
            calls.push(action);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            calls,
            [LaunchAction::Suspend, LaunchAction::Register, LaunchAction::Reset]
        );
        assert!(plan_launch(dir.path(), "0.1.0-beta.1", "19").unwrap().is_none());
    }

    #[test]
    fn fresh_reset_identity_survives_host_loss_after_native_completion() {
        let dir = tempfile::tempdir().unwrap();
        let first = plan_launch(dir.path(), "0.1.0-beta.1", "19").unwrap().unwrap();
        let token = first.reset_token.unwrap();
        assert_eq!(token.len(), 32);
        std::fs::write(dir.path().join("device.key"), [0; 32]).unwrap();
        // The native manager may have completed. Until finish_launch commits,
        // another host must send the same token, not request another wipe.
        let retry = plan_launch(dir.path(), "0.1.0-beta.1", "19").unwrap().unwrap();
        assert_eq!(retry.reset_token.as_deref(), Some(token.as_str()));
        finish_launch(dir.path(), retry, |_| Ok(())).unwrap();
        std::fs::remove_file(dir.path().join("device.key")).unwrap();
        let next = plan_launch(dir.path(), "0.1.0-beta.1", "19").unwrap().unwrap();
        assert_ne!(next.reset_token.as_deref(), Some(token.as_str()));
    }

    #[test]
    fn fresh_reset_registers_first_and_survives_identity_creation_after_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut calls = vec![];
        assert!(launch(dir.path(), "19", &mut calls, Some(LaunchAction::Reset)).is_err());
        assert_eq!(
            calls,
            [LaunchAction::Suspend, LaunchAction::Register, LaunchAction::Reset]
        );
        // The rest of the app can start; the pending reset must survive it.
        std::fs::write(dir.path().join("device.key"), [0; 32]).unwrap();
        calls.clear();
        launch(dir.path(), "19", &mut calls, None).unwrap();
        assert_eq!(
            calls,
            [LaunchAction::Suspend, LaunchAction::Register, LaunchAction::Reset]
        );
    }

    #[test]
    fn fresh_identity_resets_before_creation_and_existing_identity_preserves_pool() {
        let dir = tempfile::tempdir().unwrap();
        // Locks, logs and settings can already exist before identity startup.
        std::fs::write(dir.path().join("backend.lock"), b"").unwrap();
        let mut calls = 0;
        reset_if_fresh(dir.path(), || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 1);
        std::fs::write(dir.path().join("device.key"), [0; 32]).unwrap();
        reset_if_fresh(dir.path(), || panic!("normal launch reset its domain")).unwrap();
        // An invalid identity belongs to recovery, not a fresh installation.
        std::fs::write(dir.path().join("device.key"), b"invalid").unwrap();
        reset_if_fresh(dir.path(), || panic!("invalid identity reset its domain")).unwrap();
    }

    #[test]
    fn failed_reset_stops_startup_and_leaves_fresh_state_retryable() {
        let dir = tempfile::tempdir().unwrap();
        assert!(reset_if_fresh(dir.path(), || anyhow::bail!("remove failed")).is_err());
        assert!(!dir.path().join("device.key").exists());
        reset_if_fresh(dir.path(), || Ok(())).unwrap();
    }
}
