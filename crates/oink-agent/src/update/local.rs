use super::*;
use std::path::PathBuf;
#[derive(Debug)]
pub(super) struct Staged {
    pub package: oink_update::local::Package,
    pub path: PathBuf,
    pub _directory: tempfile::TempDir,
}
pub(super) fn open(node: &Node, path: &str) -> ApiResult<UpdateStatus> {
    {
        let mut state = lock(&node.inner.update);
        if matches!(
            state.status.phase,
            Phase::Quiescing | Phase::Installing | Phase::Verifying | Phase::Downloading
        ) {
            return Err(api_error(Error::Busy));
        }
        state.local_mode = true;
        state.local = None;
        #[cfg(not(windows))]
        {
            state.ready = false;
        }
        state.status.phase = Phase::Verifying;
        state.status.candidate = None;
        state.status.error = None;
        let status = state.status.clone();
        drop(state);
        node.emit(ApiEvent::UpdateStatusChanged(status));
        tracing::info!(phase = "local_verification_started", "local package verification");
    }
    #[cfg(target_os = "macos")]
    crate::backend::on_main(super::native::cancel);
    #[cfg(target_os = "macos")]
    let staged = super::local_mac::stage(Path::new(path));
    #[cfg(windows)]
    let staged = (|| {
        // Cloud Files callbacks belong to this user session. SYSTEM must only
        // see a fully copied ordinary file, never a received placeholder path.
        let directory = tempfile::Builder::new()
            .prefix("oink-local-")
            .tempdir()
            .map_err(|_| Error::Storage)?;
        let copy = directory.path().join("package.msi");
        copy_input(Path::new(path), &copy)?;
        let package = oink_winsvc::update::inspect_local(&copy)?;
        Ok::<_, Error>(Staged {
            package,
            path: copy,
            _directory: directory,
        })
    })();
    #[cfg(not(any(target_os = "macos", windows)))]
    let staged: Result<Staged, Error> = {
        let _ = path;
        Err(Error::OsUnsupported)
    };
    let mut state = lock(&node.inner.update);
    let result = staged.and_then(|staged| {
        #[cfg(target_os = "macos")]
        staged
            .package
            .validate_codesigned_dmg(&state.status.version, state.status.current_build, "arm64")?;
        #[cfg(not(target_os = "macos"))]
        staged.package.validate(
            &state.status.version,
            state.status.current_build,
            if cfg!(windows) { "x64" } else { "arm64" },
        )?;
        state.status.candidate = Some(staged.package.candidate());
        state.local = Some(staged);
        #[cfg(not(windows))]
        {
            state.ready = true;
        }
        Ok(())
    });
    state.status.error = result.err();
    state.status.phase = if state.status.error.is_some() {
        Phase::Failed
    } else {
        Phase::Ready
    };
    let status = state.status.clone();
    drop(state);
    tracing::info!(phase="local_verification_finished", result=?status.phase, error=?status.error, "local package verification");
    node.emit(ApiEvent::UpdateStatusChanged(status.clone()));
    Ok(status)
}
pub(super) fn install(node: &Node, expected: u64) -> ApiResult<UpdateStatus> {
    tracing::info!(step = "install_requested", expected_build = expected, "local upgrade");
    let mut state = lock(&node.inner.update);
    let staged = state.local.as_ref().ok_or_else(|| api_error(Error::Policy))?;
    if staged.package.build != expected || state.status.phase != Phase::Ready {
        return Err(api_error(Error::Policy));
    }
    #[cfg(windows)]
    {
        let path = staged.path.clone();
        let sha256 = staged.package.sha256.clone();
        // Leave the backend, input helper and networking fully live while UAC is
        // displayed. SYSTEM admission requires the elevated helper's kernel token.
        state.status.phase = Phase::Verifying;
        let node = node.clone();
        std::thread::spawn(move || {
            let result = oink_winsvc::update::elevate_local(&path, &sha256);
            let mut state = lock(&node.inner.update);
            state.local = None;
            state.local_mode = result.is_err();
            if let Err(error) = result {
                state.status.phase = Phase::Failed;
                state.status.error = Some(error);
            }
            let status = state.status.clone();
            drop(state);
            node.emit(ApiEvent::UpdateStatusChanged(status));
        });
    }
    #[cfg(target_os = "macos")]
    {
        let staged = state.local.take().unwrap();
        state.ready = false;
        state.status.phase = Phase::Quiescing;
        state.installing = true;
        let node = node.clone();
        std::thread::spawn(move || {
            if let Err(error) = super::local_mac::install(&node, staged) {
                super::install_failure(&node, error, vec![]);
                // The UI may already have closed during provider preparation;
                // equal-build repairs have no different-build outcome marker.
                if let Err(error) = crate::backend::show_ui() {
                    tracing::warn!(%error,"could not show local update failure");
                }
            }
        });
    }
    Ok(state.status.clone())
}

/// The received store owns hydration and grant acquisition. Ordinary files use
/// an ordinary user-session read; SYSTEM only ever receives the staged copy.
pub(super) fn copy_input(path: &Path, destination: &Path) -> Result<u64, Error> {
    if !path.is_absolute() {
        return Err(Error::WrongProduct);
    }
    #[cfg(windows)]
    {
        let name = path.to_str().ok_or(Error::PackageUnavailable)?;
        if !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("msi"))
            || name.starts_with(r"\\")
            || name.contains(['"', '\0'])
            || name[2..].contains(':')
        {
            return Err(Error::WrongProduct);
        }
    }
    copy_input_with(path, destination, oink_clipboard::received::open_file)
}
fn copy_input_with(
    path: &Path,
    destination: &Path,
    provider: impl FnOnce(&Path) -> Option<std::io::Result<oink_clipboard::FileReader>>,
) -> Result<u64, Error> {
    use std::io::{Read, Write};
    // A recognized URL never falls back to File::open after a provider error.
    let mut input = provider(path)
        .unwrap_or_else(|| std::fs::File::open(path).map(|file| Box::new(file) as oink_clipboard::FileReader))
        .map_err(|error| input_error("open_package", error))?;
    let mut output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|_| Error::Storage)?;
    let mut count = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let size = input
            .read(&mut buffer)
            .map_err(|error| input_error("read_package", error))?;
        if size == 0 {
            break;
        }
        count += size as u64;
        if count > oink_update::feed::MAX_PACKAGE {
            return Err(Error::Limit);
        }
        output.write_all(&buffer[..size]).map_err(|_| Error::Storage)?;
    }
    if count == 0 {
        return Err(Error::Limit);
    }
    output.sync_all().map_err(|_| Error::Storage)?;
    Ok(count)
}
fn input_error(step: &'static str, error: std::io::Error) -> Error {
    tracing::info!(step, native_code=?error.raw_os_error(), detail=%error, "local package read unavailable");
    Error::PackageUnavailable
}

#[cfg(test)]
mod input_tests {
    use super::*;
    use std::{io, sync::mpsc};

    struct FakeReader {
        chunks: mpsc::Receiver<io::Result<Vec<u8>>>,
        requested: mpsc::Sender<()>,
    }
    impl io::Read for FakeReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.requested.send(()).unwrap();
            let bytes = self.chunks.recv().unwrap()?;
            assert!(bytes.len() <= output.len());
            output[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
    }
    #[test]
    fn received_package_waits_for_provider_content_and_successful_eof() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("package.dmg");
        // A raw open would copy these incomplete bytes and finish prematurely.
        std::fs::write(&source, b"placeholder").unwrap();
        let destination = dir.path().join("staged.dmg");
        let output = destination.clone();
        let (chunks, input) = mpsc::channel();
        let (requested, reads) = mpsc::channel();
        let (done, result) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            done.send(copy_input_with(&source, &output, |_| {
                Some(Ok(Box::new(FakeReader {
                    chunks: input,
                    requested,
                })))
            }))
            .unwrap();
        });
        reads.recv().unwrap();
        assert_eq!(result.try_recv(), Err(mpsc::TryRecvError::Empty));
        chunks.send(Ok(b"first".to_vec())).unwrap();
        reads.recv().unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"first");
        assert_eq!(result.try_recv(), Err(mpsc::TryRecvError::Empty));
        chunks.send(Ok(b"-complete-package".to_vec())).unwrap();
        reads.recv().unwrap();
        assert_eq!(result.try_recv(), Err(mpsc::TryRecvError::Empty));
        chunks.send(Ok(Vec::new())).unwrap();
        assert_eq!(result.recv().unwrap(), Ok(b"first-complete-package".len() as u64));
        worker.join().unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), b"first-complete-package");
    }
    #[test]
    fn provider_open_failure_never_reads_raw_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("package.msi");
        std::fs::write(&source, b"raw-placeholder").unwrap();
        let destination = dir.path().join("staged.msi");
        assert_eq!(
            copy_input_with(&source, &destination, |_| Some(Err(
                io::ErrorKind::PermissionDenied.into()
            ))),
            Err(Error::PackageUnavailable)
        );
        assert!(!destination.exists());
    }
    #[test]
    fn provider_terminal_failure_rejects_even_a_complete_looking_package() {
        struct Failing(bool);
        impl io::Read for Failing {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.0 {
                    return Err(io::ErrorKind::NotConnected.into());
                }
                self.0 = true;
                output[..7].copy_from_slice(b"package");
                Ok(7)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            copy_input_with(&dir.path().join("package.msi"), &dir.path().join("staged.msi"), |_| {
                Some(Ok(Box::new(Failing(false))))
            }),
            Err(Error::PackageUnavailable)
        );
    }
    #[test]
    fn ordinary_file_and_destination_storage_error_remain_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("package.dmg");
        let destination = dir.path().join("staged.dmg");
        std::fs::write(&source, b"ordinary-local-package").unwrap();
        assert_eq!(
            copy_input_with(&source, &destination, |_| None),
            Ok(b"ordinary-local-package".len() as u64)
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"ordinary-local-package");
        assert_eq!(copy_input_with(&source, &destination, |_| None), Err(Error::Storage));
    }
}
