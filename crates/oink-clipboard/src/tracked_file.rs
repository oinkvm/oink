//! Local-only identity references for phone uploads. Never send these paths on
//! the wire. Ordinary PC clipboard files keep their path-bound authority.
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("tracked file is gone")]
    Gone,
    #[error("tracked file was edited")]
    Superseded,
    #[error(transparent)]
    Io(#[from] io::Error),
}
impl ResolveError {
    pub(crate) fn into_io(self) -> io::Error {
        match self {
            Self::Gone => crate::content::unavailable(crate::model::Unavailable::NoSuchItem),
            Self::Superseded => crate::content::unavailable(crate::model::Unavailable::Superseded),
            Self::Io(error) => error,
        }
    }
}
fn classify(error: io::Error) -> ResolveError {
    if error.kind() == io::ErrorKind::NotFound {
        ResolveError::Gone
    } else {
        ResolveError::Io(error)
    }
}

/// A serializable holder reference: path hint, volume/file identity and immutable
/// size/mtime version. Capture only phone-sent files; persisting it is the owner's
/// responsibility. Resolving never searches directories or follows replacements.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackedFile {
    path: PathBuf,
    identity: [u64; 2],
    size: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
    directory: bool,
    /// A phone proxy reference follows atomic handoffs recorded by its namespace.
    record_path: Option<PathBuf>,
    #[cfg(windows)]
    volume: PathBuf,
}
impl TrackedFile {
    pub fn capture(path: &Path) -> io::Result<Self> {
        let meta = std::fs::symlink_metadata(path)?;
        if crate::cache::is_link(&meta) || !(meta.is_file() || meta.is_dir()) {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let file = platform::open_path(path)?;
        Self::from_file(&file)
    }
    fn from_file(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        if !(meta.is_file() || meta.is_dir()) {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let path = platform::path(file)?;
        if is_trash(&path) {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(Self {
            path,
            identity: platform::identity(file)?,
            size: meta.len(),
            modified: meta.modified()?,
            created: meta.created().ok(),
            directory: meta.is_dir(),
            record_path: None,
            #[cfg(windows)]
            volume: platform::volume(file)?,
        })
    }
    pub fn resolve(&self) -> Result<PathBuf, ResolveError> {
        let (path, _) = self.open()?;
        Ok(path)
    }
    /// Opens and validates the identity and version on the same handle. Serving
    /// code should use this instead of opening the returned path a second time.
    pub fn open(&self) -> Result<(PathBuf, File), ResolveError> {
        if let Some(record) = &self.record_path {
            let bytes = std::fs::read(record).map_err(classify)?;
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|e| ResolveError::Io(io::Error::other(e)))?;
            let file: Self =
                serde_json::from_value(value["file"].clone()).map_err(|e| ResolveError::Io(io::Error::other(e)))?;
            if file.record_path.is_some() {
                return Err(ResolveError::Io(io::ErrorKind::InvalidData.into()));
            }
            return file.open();
        }
        let file = platform::open_id(self).map_err(classify)?;
        let path = platform::path(&file).map_err(classify)?;
        let meta = file.metadata().map_err(classify)?;
        if is_trash(&path) || platform::identity(&file).map_err(classify)? != self.identity {
            return Err(ResolveError::Gone);
        }
        // Creation time also guards against reused file IDs after deletion.
        if meta.created().ok() != self.created {
            return Err(ResolveError::Gone);
        }
        if meta.is_dir() != self.directory
            || meta.len() != self.size
            || meta.modified().map_err(classify)? != self.modified
        {
            return Err(ResolveError::Superseded);
        }
        Ok((path, file))
    }
    pub(crate) fn with_record(mut self, record: PathBuf) -> Self {
        self.record_path = Some(record);
        self
    }
    #[cfg(windows)]
    pub(crate) fn capture_native(hint: &Path, volume: u32, index: i64) -> io::Result<Self> {
        let hint = platform::open_path(hint)?;
        let file = platform::open_file_id(
            &hint,
            index as u64,
            windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES,
        )?;
        let reference = Self::from_file(&file)?;
        if !reference.matches_native(volume, index) {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(reference)
    }
    #[cfg(windows)]
    pub(crate) fn matches_native(&self, volume: u32, file: i64) -> bool {
        self.identity == [u64::from(volume), file as u64]
    }
}
fn is_trash(path: &Path) -> bool {
    path.components().any(|part| {
        let name = part.as_os_str().to_string_lossy();
        name.eq_ignore_ascii_case("$Recycle.Bin") || name == ".Trash" || name == ".Trashes"
    })
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    };
    pub fn identity(file: &File) -> io::Result<[u64; 2]> {
        let m = file.metadata()?;
        Ok([m.dev(), m.ino()])
    }
    pub fn open_path(path: &Path) -> io::Result<File> {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    }
    pub fn path(file: &File) -> io::Result<PathBuf> {
        let mut bytes = [0u8; libc::PATH_MAX as usize];
        // SAFETY: live fd and a PATH_MAX output buffer required by F_GETPATH.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = bytes.iter().position(|b| *b == 0).ok_or(io::ErrorKind::InvalidData)?;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..n])))
    }
    unsafe extern "C" {
        fn fsgetpath(buffer: *mut libc::c_char, size: usize, fsid: *mut libc::fsid_t, inode: u64) -> isize;
    }
    pub fn open_id(reference: &TrackedFile) -> io::Result<File> {
        let mut fsid = [reference.identity[0] as i32, 0i32];
        let mut bytes = [0u8; libc::PATH_MAX as usize];
        // openbyid_np requires Apple's private open-by-id entitlement on current
        // macOS (EPERM for ordinary apps). Public fsgetpath performs the inode
        // lookup with caller access checks; open(O_NOFOLLOW) + fstat + F_GETPATH
        // below revalidate the handle, so a rename/replacement race cannot grant
        // a different object. Tested on macOS 26.6.2; no scan or private entitlement.
        // SAFETY: fsid has the fsid_t layout and output is PATH_MAX bytes.
        if unsafe {
            fsgetpath(
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                fsid.as_mut_ptr().cast(),
                reference.identity[1],
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let n = bytes.iter().position(|b| *b == 0).ok_or(io::ErrorKind::InvalidData)?;
        open_path(Path::new(std::ffi::OsStr::from_bytes(&bytes[..n])))
    }
}
#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::{
        ffi::OsStringExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    };
    use windows_sys::Win32::{Foundation::INVALID_HANDLE_VALUE, Storage::FileSystem::*};
    pub fn open_path(path: &Path) -> io::Result<File> {
        std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }
    pub fn identity(file: &File) -> io::Result<[u64; 2]> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: live borrowed handle and writable information buffer.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::ErrorKind::Unsupported.into());
        }
        Ok([
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ])
    }
    pub fn path(file: &File) -> io::Result<PathBuf> {
        final_path(file, VOLUME_NAME_DOS)
    }
    pub fn volume(file: &File) -> io::Result<PathBuf> {
        Ok(final_path(file, VOLUME_NAME_GUID)?.components().take(2).collect())
    }
    fn final_path(file: &File, volume: u32) -> io::Result<PathBuf> {
        let mut bytes = vec![0u16; 32768];
        // SAFETY: live borrowed handle and UTF-16 output buffer.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                FILE_NAME_NORMALIZED | volume,
            )
        } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n >= bytes.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&bytes[..n])))
    }
    pub fn open_id(reference: &TrackedFile) -> io::Result<File> {
        // An existing file/directory on the same volume is a documented volume
        // hint; use the volume root, since the recorded leaf may have moved.
        let hint = open_path(&reference.volume)?;
        match open_file_id(&hint, reference.identity[1], FILE_GENERIC_READ) {
            Ok(file) => Ok(file),
            Err(error) if error.raw_os_error() == Some(87) => {
                // NTFS reports ERROR_INVALID_PARAMETER for an unlinked file ID.
                // A successful lookup of the live volume root proves that the
                // mechanism is supported; only then is this missing ID Gone.
                // No path fallback can accidentally serve a name replacement.
                let root_id = identity(&hint)?[1];
                if open_file_id(&hint, root_id, FILE_READ_ATTRIBUTES).is_ok() {
                    Err(io::ErrorKind::NotFound.into())
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }
    pub(super) fn open_file_id(hint: &File, id: u64, access: u32) -> io::Result<File> {
        let descriptor = FILE_ID_DESCRIPTOR {
            dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: id as i64 },
        };
        // SAFETY: hint handle, descriptor and null security attrs are valid.
        let handle = unsafe {
            OpenFileById(
                hint.as_raw_handle(),
                &descriptor,
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: newly returned owned native handle.
        Ok(unsafe { File::from_raw_handle(handle) })
    }
}
#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;
    pub fn open_path(_: &Path) -> io::Result<File> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub fn identity(_: &File) -> io::Result<[u64; 2]> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub fn path(_: &File) -> io::Result<PathBuf> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub fn open_id(_: &TrackedFile) -> io::Result<File> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(all(test, debug_assertions, any(windows, target_os = "macos")))]
mod tests {
    use super::*;
    #[test]
    fn rename_restart_replace_edit_delete() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"hello").unwrap();
        let reference = TrackedFile::capture(&a).unwrap();
        let reference: TrackedFile = serde_json::from_slice(&serde_json::to_vec(&reference).unwrap()).unwrap();
        std::fs::rename(&a, &b).unwrap();
        std::fs::write(&a, b"replacement").unwrap();
        assert_eq!(std::fs::read(reference.resolve().unwrap()).unwrap(), b"hello");
        std::fs::write(&b, b"edited contents").unwrap();
        assert!(matches!(reference.resolve(), Err(ResolveError::Superseded)));
        std::fs::remove_file(&b).unwrap();
        assert!(matches!(reference.resolve(), Err(ResolveError::Gone)));
    }
    #[test]
    fn folder_and_trash() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("folder");
        std::fs::create_dir(&a).unwrap();
        std::fs::write(a.join("child"), b"bytes").unwrap();
        let reference = TrackedFile::capture(&a).unwrap();
        let b = dir.path().join("renamed");
        std::fs::rename(&a, &b).unwrap();
        assert!(reference.resolve().unwrap().join("child").is_file());
        let trash = dir.path().join("$Recycle.Bin");
        std::fs::create_dir(&trash).unwrap();
        std::fs::rename(&b, trash.join("folder")).unwrap();
        assert!(matches!(reference.resolve(), Err(ResolveError::Gone)));
    }
}
