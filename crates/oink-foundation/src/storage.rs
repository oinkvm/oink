//! Atomic local persistence. Private files are inaccessible to other ordinary
//! users from the first instant of creation, including the staging file.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(windows)]
mod windows;

/// Create a new owner-only file. Existing paths are never opened or truncated.
pub fn create_private_new(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        windows::create_private_new(path)
    }
    #[cfg(not(windows))]
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    }
}

/// Flush a private sibling, replace the destination atomically, and flush the
/// containing directory on Unix. Failed writes remove only their own staging file.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let (temporary, mut file) = loop {
        let mut name = name.to_os_string();
        name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let temporary = parent.join(name);
        match create_private_new(&temporary) {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(windows)]
        windows::replace(&file, path)?;
        #[cfg(not(windows))]
        {
            drop(file);
            std::fs::rename(&temporary, path)?;
        }
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    #[cfg(windows)]
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_preserves_an_open_readers_snapshot() {
        use std::io::Read;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.cbor");
        atomic_write(&path, b"before").unwrap();
        let mut reader = File::open(&path).unwrap();
        atomic_write(&path, b"after").unwrap();
        let mut previous = Vec::new();
        reader.read_to_end(&mut previous).unwrap();
        assert_eq!(previous, b"before");
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
    #[test]
    fn replace_and_failed_replace_leave_no_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        assert!(atomic_write(&folder, b"fail").is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        assert!(create_private_new(&path).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
