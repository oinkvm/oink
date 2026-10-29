//! One-time receiver-store cut-over. No journal contents or source paths are
//! interpreted. Publication must stay closed until this transaction succeeds.
use std::{fs, io, path::Path};

#[cfg(windows)]
mod windows;

const MARKER: &str = ".received-links-reuse-v1";
const PENDING: &[u8] = b"received-links-reuse-v1:pending\n";
const COMPLETE: &[u8] = b"received-links-reuse-v1:complete\n";

pub(crate) fn prepare(cache: &Path, clear_native: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    prepare_with(cache, clear_native, |_| Ok(()))
}

fn prepare_with(
    cache: &Path,
    clear_native: impl FnOnce() -> io::Result<()>,
    mut checkpoint: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<()> {
    fs::create_dir_all(cache)?;
    directory(cache)?;
    #[cfg(windows)]
    let _cache = windows::pin_directory(cache)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let guard = options.open(cache.join(".received-links-reuse-v1.lock"))?;
    if !guard.metadata()?.is_file() || guard.metadata()?.file_type().is_symlink() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    guard.lock()?;
    let marker = cache.join(MARKER);
    match fs::symlink_metadata(&marker) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => match fs::read(&marker)?.as_slice() {
            COMPLETE => return Ok(()),
            PENDING => {}
            _ => return Err(io::ErrorKind::InvalidData.into()),
        },
        Ok(_) => return Err(io::ErrorKind::PermissionDenied.into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            oink_foundation::storage::atomic_write(&marker, PENDING)?;
        }
        Err(e) => return Err(e),
    }
    checkpoint("pending")?;
    // This is the only obsolete application backing store. In particular,
    // source-snapshots, source grants, and promise-exports are not wipe targets.
    remove(&cache.join("completed-deliveries"))?;
    checkpoint("completed-deliveries")?;
    clear_native()?;
    checkpoint("native")?;
    // Windows completes its native-directory wipe in this callback. Mac has a
    // separate App Group/system-reset gate before returning a publication path.
    // A crash before this commit retries; neither gate admits new publications.
    oink_foundation::storage::atomic_write(&marker, COMPLETE)
}

pub(crate) fn directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    #[cfg(windows)]
    if !crate::windows::cloud::upgrade_directory(path)? {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

/// Keep the registered sync-root inode. Never recurse into a directory link.
#[cfg(any(windows, test))]
pub(crate) fn clear_directory(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    return windows::clear_directory(path);
    #[cfg(not(windows))]
    {
        directory(path)?;
        for entry in fs::read_dir(path)? {
            remove(&entry?.path())?;
        }
        Ok(())
    }
}

pub(crate) fn remove(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    return windows::remove(path);
    #[cfg(not(windows))]
    {
        let metadata = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            // std uses fd-relative traversal and never follows directory symlinks.
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_wipes_obsolete_backing_once_without_touching_source_stores() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            "completed-deliveries/old",
            "source-snapshots/keep",
            "promise-exports/keep",
            "unrelated/keep",
        ] {
            let p = temp.path().join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, b"original").unwrap();
        }
        prepare(temp.path(), || Ok(())).unwrap();
        assert!(!temp.path().join("completed-deliveries").exists());
        for name in ["source-snapshots/keep", "promise-exports/keep", "unrelated/keep"] {
            assert_eq!(fs::read(temp.path().join(name)).unwrap(), b"original");
        }
        fs::create_dir(temp.path().join("completed-deliveries")).unwrap();
        fs::write(temp.path().join("completed-deliveries/new"), b"new").unwrap();
        prepare(temp.path(), || panic!("a completed wipe must never run again")).unwrap();
        assert!(temp.path().join("completed-deliveries/new").exists());
    }

    #[test]
    fn interrupted_wipe_resumes_before_publication_and_never_commits_a_failure() {
        for failure in ["pending", "completed-deliveries", "native"] {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir(temp.path().join("completed-deliveries")).unwrap();
            fs::write(temp.path().join("completed-deliveries/old"), b"old").unwrap();
            fs::create_dir(temp.path().join("native")).unwrap();
            fs::write(temp.path().join("native/old"), b"old").unwrap();
            let clear = || clear_directory(&temp.path().join("native"));
            assert!(
                prepare_with(temp.path(), clear, |at| if at == failure {
                    Err(io::Error::other("simulated process loss"))
                } else {
                    Ok(())
                })
                .is_err()
            );
            assert_eq!(fs::read(temp.path().join(MARKER)).unwrap(), PENDING);
            prepare(temp.path(), clear).unwrap();
            assert!(!temp.path().join("completed-deliveries").exists());
            assert_eq!(fs::read_dir(temp.path().join("native")).unwrap().count(), 0);
            assert_eq!(fs::read(temp.path().join(MARKER)).unwrap(), COMPLETE);
        }
    }

    #[cfg(unix)]
    #[test]
    fn wipe_unlinks_leaf_links_but_refuses_a_redirected_store_root() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("source"), b"source").unwrap();
        fs::create_dir(temp.path().join("completed-deliveries")).unwrap();
        symlink(outside.path(), temp.path().join("completed-deliveries/link")).unwrap();
        prepare(temp.path(), || Ok(())).unwrap();
        assert_eq!(fs::read(outside.path().join("source")).unwrap(), b"source");
        symlink(outside.path(), temp.path().join("redirected")).unwrap();
        assert!(prepare(&temp.path().join("redirected"), || panic!()).is_err());
        assert!(!outside.path().join(MARKER).exists());
    }
}
