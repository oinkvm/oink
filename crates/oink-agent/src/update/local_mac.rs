//! Authenticated local DMG installation, independent of Sparkle feed policy.
use super::{Error, Node, close_mac_clients, local::Staged};
use std::{
    ffi::CString,
    fs,
    io::Read,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
unsafe extern "C" {
    fn oink_local_verify_app(path: *const std::ffi::c_char) -> u32;
}
fn io_step<T>(step: &'static str, result: std::io::Result<T>, error: Error) -> Result<T, Error> {
    result.map_err(|source| {
        tracing::info!(step, error=?error, native_code=?source.raw_os_error(), detail=%source, "local upgrade step failed");
        error
    })
}
fn run(step: &'static str, binary: &str, args: &[&std::ffi::OsStr]) -> Result<Vec<u8>, Error> {
    let output = io_step(
        step,
        Command::new(binary)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output(),
        Error::Storage,
    )?;
    tracing::info!(step, binary, success=output.status.success(), exit_code=?output.status.code(), "local upgrade command finished");
    if !output.status.success() {
        return Err(Error::CorruptPackage);
    }
    Ok(output.stdout)
}
pub fn verify_app(path: &Path) -> Result<(), Error> {
    let path = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| Error::Storage)?;
    // SAFETY: immutable staged bundle; pointer remains live through native checks.
    let result = match unsafe { oink_local_verify_app(path.as_ptr()) } {
        0 => Ok(()),
        2 => Err(Error::NotNotarized),
        _ => Err(Error::Signature),
    };
    tracing::info!(step = "authenticate_bundle", ?result, "local upgrade");
    result
}
fn arm64_executable(path: &Path) -> Result<bool, Error> {
    // /usr/bin/lipo is a developer-tools shim and can show an Xcode-install
    // dialog on an end-user Mac. Read only bounded Mach-O CPU records here;
    // Security.framework subsequently authenticates every complete slice.
    let mut file = fs::File::open(path).map_err(|_| Error::CorruptPackage)?;
    let mut header = [0u8; 8];
    file.read_exact(&mut header).map_err(|_| Error::CorruptPackage)?;
    let arm = |cpu: u32, subtype: u32| cpu == 0x0100_000c && (subtype & 0x00ff_ffff) <= 1;
    match header[..4] {
        [0xcf, 0xfa, 0xed, 0xfe] => {
            let mut subtype = [0u8; 4];
            file.read_exact(&mut subtype).map_err(|_| Error::CorruptPackage)?;
            Ok(arm(
                u32::from_le_bytes(header[4..8].try_into().unwrap()),
                u32::from_le_bytes(subtype),
            ))
        }
        [0xca, 0xfe, 0xba, 0xbe] | [0xca, 0xfe, 0xba, 0xbf] | [0xbe, 0xba, 0xfe, 0xca] | [0xbf, 0xba, 0xfe, 0xca] => {
            let little = header[0] == 0xbe || header[0] == 0xbf;
            let number = |bytes: [u8; 4]| {
                if little {
                    u32::from_le_bytes(bytes)
                } else {
                    u32::from_be_bytes(bytes)
                }
            };
            let count = number(header[4..8].try_into().unwrap());
            if count == 0 || count > 16 {
                return Err(Error::CorruptPackage);
            }
            let length = if header[3] == 0xbf || header[0] == 0xbf { 32 } else { 20 };
            for _ in 0..count {
                let mut record = [0u8; 32];
                file.read_exact(&mut record[..length])
                    .map_err(|_| Error::CorruptPackage)?;
                if arm(
                    number(record[..4].try_into().unwrap()),
                    number(record[4..8].try_into().unwrap()),
                ) {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        [0xfe, 0xed, 0xfa, 0xcf] | [0xce, 0xfa, 0xed, 0xfe] | [0xfe, 0xed, 0xfa, 0xce] => Ok(false),
        _ => Err(Error::CorruptPackage),
    }
}
fn bundle() -> Result<PathBuf, Error> {
    let exe = std::env::current_exe().map_err(|_| Error::Storage)?;
    exe.parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Path::to_owned)
        .ok_or(Error::NotWritable)
}
fn plist(path: &Path, field: &str) -> Result<String, Error> {
    let key = format!(":{field}");
    let bytes = run(
        "read_bundle_metadata",
        "/usr/libexec/PlistBuddy",
        &["-c".as_ref(), format!("Print {key}").as_ref(), path.as_os_str()],
    )?;
    Ok(String::from_utf8(bytes)
        .map_err(|_| Error::CorruptPackage)?
        .trim()
        .to_owned())
}
fn tree(path: &Path, root: &Path, total: &mut u64) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path).map_err(|_| Error::CorruptPackage)?;
    if metadata.file_type().is_symlink() {
        if !fs::canonicalize(path)
            .map_err(|_| Error::CorruptPackage)?
            .starts_with(root)
        {
            return Err(Error::CorruptPackage);
        }
    } else if metadata.is_dir() {
        for child in fs::read_dir(path).map_err(|_| Error::CorruptPackage)? {
            tree(&child.map_err(|_| Error::CorruptPackage)?.path(), root, total)?;
        }
    } else if metadata.is_file() {
        *total = total.checked_add(metadata.len()).ok_or(Error::Limit)?;
        if *total > oink_update::feed::MAX_UNPACKED {
            return Err(Error::Limit);
        }
    } else {
        return Err(Error::CorruptPackage);
    }
    Ok(())
}
struct Mount(PathBuf);
impl Drop for Mount {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/hdiutil")
            .arg("detach")
            .arg("-force")
            .arg(&self.0)
            .output();
    }
}
pub(super) fn stage(path: &Path) -> Result<Staged, Error> {
    tracing::info!(step = "stage_started", "local upgrade");
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("dmg")) {
        return Err(Error::WrongProduct);
    }
    let destination = bundle()?;
    // A sibling staging directory proves replacement access before any drain.
    // No privilege escalation or authorization dialog is attempted on macOS.
    let directory = tempfile::Builder::new()
        .prefix(".oink-local-")
        .suffix(".noindex")
        .tempdir_in(destination.parent().ok_or(Error::NotWritable)?)
        .map_err(|_| Error::NotWritable)?;
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).map_err(|_| Error::Storage)?;
    let image = directory.path().join("input.dmg");
    let length = super::local::copy_input(path, &image)?;
    let mount = directory.path().join("mount");
    fs::create_dir(&mount).map_err(|_| Error::Storage)?;
    run(
        "mount_image",
        "/usr/bin/hdiutil",
        &[
            "attach".as_ref(),
            "-readonly".as_ref(),
            "-nobrowse".as_ref(),
            "-noautoopen".as_ref(),
            "-stdinpass".as_ref(),
            "-mountpoint".as_ref(),
            mount.as_os_str(),
            image.as_os_str(),
        ],
    )?;
    let mounted = Mount(mount.clone());
    let source = mount.join("Oink.app");
    if fs::symlink_metadata(&source)
        .map_err(|_| Error::WrongProduct)?
        .file_type()
        .is_symlink()
    {
        return Err(Error::WrongProduct);
    }
    let canonical_source = fs::canonicalize(&source).map_err(|_| Error::CorruptPackage)?;
    tree(&source, &canonical_source, &mut 0)?;
    let app = directory.path().join("Oink.app");
    run(
        "copy_bundle",
        "/usr/bin/ditto",
        &["--noqtn".as_ref(), source.as_os_str(), app.as_os_str()],
    )?;
    drop(mounted);
    fs::remove_file(image).map_err(|_| Error::Storage)?;
    let info = app.join("Contents/Info.plist");
    if plist(&info, "CFBundleIdentifier")? != oink_update::PRODUCT {
        return Err(Error::WrongProduct);
    }
    let executable = plist(&info, "CFBundleExecutable")?;
    if executable != "oink-backend" {
        return Err(Error::WrongProduct);
    }
    if !arm64_executable(&app.join("Contents/MacOS/oink-backend"))? {
        return Err(Error::WrongArchitecture);
    }
    verify_app(&app)?;
    let version = plist(&info, "CFBundleGetInfoString").or_else(|_| plist(&info, "CFBundleShortVersionString"))?;
    // The app signature seals its report manifest, including source identity.
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(app.join("Contents/Resources/report-manifest.json")).map_err(|_| Error::CorruptPackage)?,
    )
    .map_err(|_| Error::CorruptPackage)?;
    let source_tree_sha256 = report["source_tree_sha256"].as_str().unwrap_or("").to_owned();
    let package = oink_update::local::Package {
        product: oink_update::PRODUCT.into(),
        version,
        build: plist(&info, "CFBundleVersion")?
            .parse()
            .map_err(|_| Error::CorruptPackage)?,
        architecture: "arm64".into(),
        source_tree_sha256,
        minimum_windows_build: 0,
        length,
        sha256: String::new(),
        notes: read_notes(&app)?,
    };
    tracing::info!(step = "stage_finished", build = package.build, "local upgrade");
    Ok(Staged {
        path: app,
        _directory: directory,
        package,
    })
}
fn read_notes(app: &Path) -> Result<Option<oink_update::feed::ReleaseNotes>, Error> {
    let path = app.join("Contents/Resources/oink-update-notes.json");
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Error::CorruptPackage),
    };
    let mut bytes = Vec::new();
    file.take(oink_update::local::MAX_EMBEDDED_NOTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::CorruptPackage)?;
    oink_update::local::embedded_notes(&bytes).map(Some)
}
pub(super) fn install(node: &Node, staged: Staged) -> Result<(), Error> {
    tracing::info!(
        step = "install_reauthenticate",
        build = staged.package.build,
        "local upgrade"
    );
    verify_app(&staged.path)?;
    // Cancel a possible scheduled/cached Sparkle cycle without changing its
    // feed policy. Wait for its ready-on-quit continuation to be disarmed before
    // starting our own maintenance/shutdown; the normal update path is untouched.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (done, wait) = std::sync::mpsc::sync_channel(1);
        crate::backend::on_main(move || {
            super::native::cancel();
            let _ = done.send(super::native::quit_allowed());
        });
        if wait
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| Error::Busy)?
        {
            break;
        }
        if Instant::now() >= deadline {
            return Err(Error::Busy);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    tracing::info!(step = "prepare_maintenance", "local upgrade");
    let mut token = node.prepare_for_update().map_err(|_| Error::Busy)?;
    let destination = bundle()?;
    let mac = destination.join("Contents/MacOS");
    tracing::info!(step = "close_clients", "local upgrade");
    token.provider = Some(close_mac_clients(&token, &mac)?);
    token.validate().map_err(|_| Error::Busy)?;
    // sockaddr_un has a short path limit even when the app's filesystem path
    // is valid. Keep the readiness socket in a private short /tmp directory.
    let ready_directory = io_step(
        "readiness_directory",
        tempfile::Builder::new().prefix("oink-ready-").tempdir_in("/tmp"),
        Error::Storage,
    )?;
    let socket = ready_directory.path().join("ready.sock");
    let listener = io_step(
        "readiness_listener",
        std::os::unix::net::UnixListener::bind(&socket),
        Error::Storage,
    )?;
    io_step(
        "readiness_listener_nonblocking",
        listener.set_nonblocking(true),
        Error::Storage,
    )?;
    let mut child = io_step(
        "spawn_helper",
        Command::new(mac.join("oink-agent"))
            .arg("complete-local-update")
            .arg(&staged.path)
            .arg(&destination)
            .arg(std::process::id().to_string())
            .arg(&socket)
            .stdin(std::process::Stdio::null())
            .spawn(),
        Error::Transaction,
    )?;
    tracing::info!(step = "helper_spawned", helper_pid = child.id(), "local upgrade");
    let deadline = Instant::now() + token.remaining();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                read_ready(stream, deadline)?;
                tracing::info!(step = "helper_ready", "local upgrade");
                break;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return io_step("accept_readiness", Err(error), Error::Transaction),
        }
        if Instant::now() >= deadline || io_step("poll_helper", child.try_wait(), Error::Transaction)?.is_some() {
            tracing::info!(
                step = "helper_ready",
                "local upgrade helper exited or readiness timed out"
            );
            return Err(Error::Transaction);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let current = node.query_update_status().current_build;
    if staged.package.build > current {
        io_step(
            "write_relaunch_intent",
            super::presentation::begin(node.data_dir(), current, staged.package.build),
            Error::Storage,
        )?;
    }
    let _ = staged._directory.keep();
    tracing::info!(step = "backend_exit_requested", "local upgrade");
    token.shutdown_with_ui(true).map_err(|_| Error::Busy)
}
fn read_ready(mut stream: UnixStream, deadline: Instant) -> Result<(), Error> {
    // macOS 26.6.2 returns EINVAL for SO_RCVTIMEO after the helper has
    // written its byte and closed, even though that byte is still readable.
    // Poll the descriptor against the preparation deadline instead of setting
    // a socket timeout. Explicit nonblocking mode also avoids relying on accept
    // inheriting the listener's flags; authentication and shutdown are unchanged.
    io_step("readiness_nonblocking", stream.set_nonblocking(true), Error::Storage)?;
    let mut ready = [0u8; 1];
    while Instant::now() < deadline {
        match stream.read(&mut ready) {
            Ok(1) if ready == [1] => return Ok(()),
            Ok(_) => {
                tracing::info!(
                    step = "helper_ready",
                    "local upgrade helper sent invalid or empty readiness"
                );
                return Err(Error::Transaction);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return io_step("helper_ready", Err(error), Error::Transaction),
        }
        std::thread::sleep(Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())));
    }
    tracing::info!(step = "helper_ready", "local upgrade helper readiness timed out");
    Err(Error::Transaction)
}
/// Internal CLI continuation. Uses no administrator rights or shell commands.
/// Atomic exchange retains the old bundle until replacement and launch succeed.
pub fn complete(stage: &Path, destination: &Path, parent: u32, socket: &Path) -> Result<(), Error> {
    tracing::info!(step = "helper_started", parent_pid = parent, "local upgrade");
    let result = (|| {
        if fs::canonicalize(destination).map_err(|_| Error::Storage)?
            != fs::canonicalize(bundle()?).map_err(|_| Error::Storage)?
        {
            return Err(Error::Policy);
        }
        complete_with_launcher(stage, destination, parent, socket, |path| {
            let status = io_step(
                "reopen_replacement",
                replacement_reopen_command(path, &oink_foundation::paths::data_dir()).status(),
                Error::Transaction,
            )?;
            tracing::info!(step="reopen_replacement", success=status.success(), exit_code=?status.code(), "local upgrade");
            if status.success() {
                Ok(())
            } else {
                Err(Error::Transaction)
            }
        })
    })();
    tracing::info!(step = "helper_finished", ?result, "local upgrade");
    result
}
fn replacement_reopen_command(path: &Path, data_dir: &Path) -> Command {
    crate::backend::launch::mac_reopen_command(path, data_dir)
}

fn safe_stage(stage: &Path, destination: &Path) -> Result<(), Error> {
    let directory = stage.parent().ok_or(Error::Policy)?;
    let name = directory.file_name().and_then(|s| s.to_str()).ok_or(Error::Policy)?;
    if stage.file_name().is_none_or(|s| s != "Oink.app")
        || !name.starts_with(".oink-local-")
        || !name.ends_with(".noindex")
    {
        return Err(Error::Policy);
    }
    let metadata = fs::symlink_metadata(directory).map_err(|_| Error::Storage)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(Error::Policy);
    }
    if fs::canonicalize(directory.parent().ok_or(Error::Policy)?).map_err(|_| Error::Storage)?
        != fs::canonicalize(destination.parent().ok_or(Error::Policy)?).map_err(|_| Error::Storage)?
    {
        return Err(Error::Policy);
    }
    Ok(())
}
fn complete_with_launcher(
    stage: &Path,
    destination: &Path,
    parent: u32,
    socket: &Path,
    mut launch: impl FnMut(&Path) -> Result<(), Error>,
) -> Result<(), Error> {
    // The continuation may remove only its private sibling staging directory.
    // Internal CLI arguments must never turn cleanup into arbitrary tree removal.
    safe_stage(stage, destination)?;
    verify_app(stage)?;
    use std::io::Write;
    let mut ready = io_step("helper_connect", UnixStream::connect(socket), Error::Transaction)?;
    io_step("helper_send_ready", ready.write_all(&[1]), Error::Transaction)?;
    drop(ready);
    tracing::info!(step = "helper_ready_sent", "local upgrade");
    let deadline = Instant::now() + Duration::from_secs(40);
    // SAFETY: signal 0 observes process existence; it never sends input or kills.
    while unsafe { libc::kill(parent as i32, 0) } == 0 {
        if Instant::now() >= deadline {
            tracing::info!(step = "wait_backend_exit", "local upgrade parent exit timed out");
            return Err(Error::Busy);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    tracing::info!(step = "backend_exited", "local upgrade");
    // Authentication has completed; removing quarantine avoids a Gatekeeper UI
    // on relaunch. No policy setting is disabled, including for future downloads.
    let authenticated = run(
        "remove_verified_quarantine",
        "/usr/bin/xattr",
        &["-dr".as_ref(), "com.apple.quarantine".as_ref(), stage.as_os_str()],
    )
    .map_err(|_| Error::NotWritable)
    .and_then(|_| verify_app(stage));
    if let Err(error) = authenticated {
        tracing::info!(step = "relaunch_original", ?error, "local upgrade");
        let _ = launch(destination);
        return Err(error);
    }
    if let Err(error) = exchange(stage, destination) {
        tracing::info!(step = "exchange_failed", ?error, "local upgrade");
        let _ = launch(destination);
        return Err(error);
    }
    tracing::info!(step = "bundle_exchanged", "local upgrade");
    if let Err(error) = launch(destination) {
        tracing::info!(step = "relaunch_failed_rolling_back", ?error, "local upgrade");
        exchange(stage, destination)?;
        let _ = launch(destination);
        return Err(error);
    }
    tracing::info!(step = "replacement_reopen_accepted", "local upgrade");
    if let Some(directory) = stage.parent() {
        let _ = fs::remove_dir_all(directory);
    }
    Ok(())
}
fn exchange(a: &Path, b: &Path) -> Result<(), Error> {
    let a = CString::new(a.as_os_str().as_encoded_bytes()).map_err(|_| Error::Storage)?;
    let b = CString::new(b.as_os_str().as_encoded_bytes()).map_err(|_| Error::Storage)?;
    // SAFETY: same-volume validated sibling paths; RENAME_SWAP is atomic and
    // never leaves the installed bundle absent. No user-data directory is touched.
    if unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) } != 0 {
        return io_step(
            "atomic_exchange",
            Err(std::io::Error::last_os_error()),
            Error::NotWritable,
        );
    }
    Ok(())
}
/// Attach only to the main product webview. Callback strings are borrowed for
/// the call; the receiver must copy them before returning.
/// # Safety
/// `view` must be a live NSView on the AppKit main thread.
pub unsafe fn attach_drop_view(
    view: *mut std::ffi::c_void,
    callback: extern "C" fn(*const std::ffi::c_char),
    hover: extern "C" fn(u8),
) {
    unsafe extern "C" {
        fn oink_local_attach_drop(
            view: *mut std::ffi::c_void,
            callback: extern "C" fn(*const std::ffi::c_char),
            hover: extern "C" fn(u8),
        );
    }
    // SAFETY: caller guarantees main-thread live NSView; the callback is static.
    unsafe {
        oink_local_attach_drop(view, callback, hover);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readiness_survives_helper_closing_immediately_after_its_byte() {
        use std::io::Write;
        let (receiver, mut helper) = UnixStream::pair().unwrap();
        helper.write_all(&[1]).unwrap();
        drop(helper);
        assert_eq!(read_ready(receiver, Instant::now() + Duration::from_secs(1)), Ok(()));
    }
    #[test]
    fn readiness_waits_for_a_delayed_byte_and_rejects_empty_or_invalid_messages() {
        use std::io::Write;
        let (receiver, mut helper) = UnixStream::pair().unwrap();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            helper.write_all(&[1]).unwrap();
        });
        assert_eq!(read_ready(receiver, Instant::now() + Duration::from_secs(1)), Ok(()));
        sender.join().unwrap();
        for message in [&[][..], &[0][..]] {
            let (receiver, mut helper) = UnixStream::pair().unwrap();
            helper.write_all(message).unwrap();
            drop(helper);
            assert_eq!(
                read_ready(receiver, Instant::now() + Duration::from_secs(1)),
                Err(Error::Transaction)
            );
        }
    }
    #[test]
    fn readiness_obeys_the_preparation_deadline_with_a_silent_live_helper() {
        let (receiver, _helper) = UnixStream::pair().unwrap();
        let started = Instant::now();
        assert_eq!(
            read_ready(receiver, started + Duration::from_millis(20)),
            Err(Error::Transaction)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn replacement_reopen_keeps_private_state_and_clears_login_launch() {
        let command = replacement_reopen_command(Path::new("/isolated/Oink.app"), Path::new("/private/update-state"));
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--env", "OINK_HOME=/private/update-state"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--env", &format!("{}=0", oink_autostart::LOGIN_LAUNCH_ENV)])
        );
        assert!(args.ends_with(&["--args".into(), crate::backend::REOPEN_FLAG.into()]));
    }
    #[test]
    fn preflight_reads_bundle_notes_and_accepts_older_bundles_without_them() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_notes(root.path()), Ok(None));
        let resources = root.path().join("Contents/Resources");
        fs::create_dir_all(&resources).unwrap();
        let path = resources.join("oink-update-notes.json");
        fs::write(&path, br#"{"schema":1,"notes":{"en":"Fixed drag"}}"#).unwrap();
        assert_eq!(
            read_notes(root.path()).unwrap().unwrap().for_language("ko"),
            "Fixed drag"
        );
        fs::write(&path, b"damaged").unwrap();
        assert_eq!(read_notes(root.path()), Err(Error::CorruptPackage));
    }
    #[test]
    fn architecture_check_needs_no_developer_tool_or_process() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("image");
        for (cpu, expected) in [(0x0100_000cu32, true), (0x0100_0007u32, false)] {
            let mut bytes = vec![0xcf, 0xfa, 0xed, 0xfe];
            bytes.extend(cpu.to_le_bytes());
            bytes.extend(0u32.to_le_bytes());
            fs::write(&path, bytes).unwrap();
            assert_eq!(arm64_executable(&path), Ok(expected));
        }
        let mut fat = vec![0xca, 0xfe, 0xba, 0xbe];
        fat.extend(1u32.to_be_bytes());
        fat.extend(0x0100_000cu32.to_be_bytes());
        fat.extend([0u8; 16]);
        fs::write(&path, fat).unwrap();
        assert_eq!(arm64_executable(&path), Ok(true));
        fs::write(&path, b"damaged executable").unwrap();
        assert_eq!(arm64_executable(&path), Err(Error::CorruptPackage));
    }
    #[test]
    fn bundle_tree_accepts_internal_links_and_rejects_external_links() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(root.path().join("payload"), "sealed").unwrap();
        std::os::unix::fs::symlink("payload", root.path().join("internal")).unwrap();
        let canonical = fs::canonicalize(root.path()).unwrap();
        assert_eq!(tree(root.path(), &canonical, &mut 0), Ok(()));
        std::os::unix::fs::symlink(outside.path(), root.path().join("external")).unwrap();
        assert_eq!(tree(root.path(), &canonical, &mut 0), Err(Error::CorruptPackage));
    }
    #[test]
    fn cleanup_rejects_ordinary_directories_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("Oink.app");
        assert_eq!(
            safe_stage(&root.path().join("Oink.app"), &destination),
            Err(Error::Policy)
        );
        let candidate = root.path().join(".oink-local-test.noindex");
        std::os::unix::fs::symlink(root.path(), &candidate).unwrap();
        assert_eq!(
            safe_stage(&candidate.join("Oink.app"), &destination),
            Err(Error::Policy)
        );
    }
    #[test]
    #[ignore = "requires genuine baseline/candidate bundles; isolated swap with a validation-only launcher, no desktop UI"]
    fn external_isolated_handoff_and_bundle_replacement() {
        let baseline = PathBuf::from(std::env::var("OINK_LOCAL_UPDATE_TEST_BASELINE").expect("explicit baseline"));
        let candidate = PathBuf::from(std::env::var("OINK_LOCAL_UPDATE_TEST_APP").expect("explicit candidate"));
        let evidence = PathBuf::from(std::env::var("OINK_LOCAL_UPDATE_TEST_EVIDENCE").expect("external evidence"));
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        for app in [&baseline, &candidate] {
            verify_app(app).unwrap();
        }
        let before = plist(&baseline.join("Contents/Info.plist"), "CFBundleVersion").unwrap();
        let expected = plist(&candidate.join("Contents/Info.plist"), "CFBundleVersion").unwrap();
        let root = tempfile::Builder::new()
            .suffix(".noindex")
            .tempdir_in(&evidence)
            .unwrap();
        let directory = root.path().join(".oink-local-probe.noindex");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let stage = directory.join("Oink.app");
        let destination = root.path().join("Oink.app");
        for (source, path) in [(&candidate, &stage), (&baseline, &destination)] {
            assert!(
                Command::new("/usr/bin/ditto")
                    .arg("--noqtn")
                    .arg(source)
                    .arg(path)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let data = root.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("pairing-marker"), "preserved").unwrap();
        super::super::presentation::begin(&data, before.parse().unwrap(), expected.parse().unwrap()).unwrap();
        let ready_directory = tempfile::Builder::new()
            .prefix("oink-ready-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = ready_directory.path().join("ready.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let mut parent = Command::new("/bin/sleep").arg("10").spawn().unwrap();
        let pid = parent.id();
        let expected_inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&stage).unwrap());
        let helper = std::thread::spawn(move || {
            complete_with_launcher(&stage, &destination, pid, &socket, |app| {
                // This probe never opens LaunchServices or starts a UI/backend.
                // Validate the real replacement at the production launch boundary.
                verify_app(app)?;
                assert_eq!(plist(&app.join("Contents/Info.plist"), "CFBundleVersion")?, expected);
                assert_eq!(
                    std::os::unix::fs::MetadataExt::ino(&fs::metadata(app).unwrap()),
                    expected_inode
                );
                assert_eq!(fs::read_to_string(data.join("pairing-marker")).unwrap(), "preserved");
                assert_eq!(
                    super::super::presentation::outcome(&data, expected.parse().unwrap()),
                    Some(super::super::presentation::Outcome::Committed)
                );
                fs::write(
                    evidence.join("isolated-replace-result.json"),
                    serde_json::to_vec_pretty(&serde_json::json!({
                        "old_build":before,"new_build":expected,"marker_preserved":true,
                        "authenticated_atomic_exchange":true,"launch_boundary_validated":true,"desktop_ui_started":false
                    }))
                    .unwrap(),
                )
                .unwrap();
                Ok(())
            })
        });
        let (ready, _) = listener.accept().unwrap();
        // Exercise the actual production receiver after the helper closed its
        // write side, rather than the old smoke probe's immediate blocking read.
        std::thread::sleep(Duration::from_millis(30));
        let result = read_ready(ready, Instant::now() + Duration::from_secs(1));
        parent.kill().unwrap();
        parent.wait().unwrap();
        assert_eq!(result, Ok(()));
        assert_eq!(helper.join().unwrap(), Ok(()));
    }
    #[test]
    #[ignore = "requires explicit genuine reference app; replaces isolated copies and runs desktop-disabled launch smoke only"]
    fn external_isolated_replace_and_backend_relaunch() {
        let reference = PathBuf::from(std::env::var("OINK_LOCAL_UPDATE_TEST_APP").expect("explicit reference app"));
        let evidence = PathBuf::from(std::env::var("OINK_LOCAL_UPDATE_TEST_EVIDENCE").expect("external evidence"));
        verify_app(&reference).unwrap();
        let root = tempfile::Builder::new()
            .prefix("replace-")
            .suffix(".noindex")
            .tempdir_in(&evidence)
            .unwrap();
        let directory = root.path().join(".oink-local-probe.noindex");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let stage = directory.join("Oink.app");
        let destination = root.path().join("Oink.app");
        for path in [&stage, &destination] {
            assert!(
                Command::new("/usr/bin/ditto")
                    .arg(&reference)
                    .arg(path)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let expected_inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&stage).unwrap());
        let data = root.path().join("private-state");
        fs::create_dir(&data).unwrap();
        let preserved = root.path().join("preserved-user-data");
        fs::create_dir(&preserved).unwrap();
        fs::write(preserved.join("pairing-marker"), "preserved").unwrap();
        let ready_directory = tempfile::Builder::new()
            .prefix("oink-ready-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = ready_directory.path().join("ready.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let mut parent = Command::new("/bin/sleep").arg("0.5").spawn().unwrap();
        let pid = parent.id();
        let helper = std::thread::spawn(move || {
            complete_with_launcher(&stage, &destination, pid, &socket, |app| {
                assert_eq!(
                    std::os::unix::fs::MetadataExt::ino(&fs::metadata(app).unwrap()),
                    expected_inode
                );
                let log = fs::File::create(evidence.join("isolated-mac-relaunch.log")).unwrap();
                let mut backend = Command::new(app.join("Contents/MacOS/oink-backend"))
                    .arg("--launch-smoke")
                    .env("OINK_HOME", &data)
                    .stdout(log.try_clone().unwrap())
                    .stderr(log)
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !data.join("launch-smoke-ready").exists() && Instant::now() < deadline {
                    assert!(backend.try_wait().unwrap().is_none());
                    std::thread::sleep(Duration::from_millis(50));
                }
                let ready = data.join("launch-smoke-ready").exists();
                // SAFETY: terminate only the exact disposable child created above;
                // --launch-smoke uses private state and disables desktop input/output.
                unsafe {
                    libc::kill(backend.id() as i32, libc::SIGTERM);
                }
                let stop = Instant::now() + Duration::from_secs(10);
                let code = loop {
                    if let Some(code) = backend.try_wait().unwrap() {
                        break code;
                    }
                    if Instant::now() >= stop {
                        backend.kill().unwrap();
                        let _ = backend.wait();
                        panic!("isolated smoke did not shut down");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                };
                assert_eq!(
                    fs::read_to_string(preserved.join("pairing-marker")).unwrap(),
                    "preserved"
                );
                if ready && code.success() {
                    Ok(())
                } else {
                    Err(Error::Transaction)
                }
            })
        });
        let (mut ready, _) = listener.accept().unwrap();
        let mut byte = [0u8; 1];
        ready.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [1]);
        parent.wait().unwrap();
        assert_eq!(helper.join().unwrap(), Ok(()));
    }
    #[test]
    fn atomic_exchange_keeps_both_versions_and_never_touches_data() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("new");
        let b = dir.path().join("old");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        fs::write(a.join("build"), "new").unwrap();
        fs::write(b.join("build"), "old").unwrap();
        fs::write(dir.path().join("data"), "pairing").unwrap();
        exchange(&a, &b).unwrap();
        assert_eq!(fs::read_to_string(b.join("build")).unwrap(), "new");
        assert_eq!(fs::read_to_string(a.join("build")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dir.path().join("data")).unwrap(), "pairing");
    }
}
