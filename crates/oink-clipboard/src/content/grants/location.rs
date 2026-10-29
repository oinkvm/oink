use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Location {
    path: PathBuf,
    original: PathBuf,
    pub fingerprint: local::Fingerprint,
    #[serde(default)]
    symlink: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub followed: Vec<local::batch::Followed>,
    #[cfg(target_os = "macos")]
    native: mac::Identity,
    #[cfg(windows)]
    volume: PathBuf,
}
impl Location {
    pub fn immutable(&self) -> io::Result<PathBuf> {
        if self.inspect(&self.path)? != self.fingerprint {
            return Err(unavailable(Unavailable::Superseded));
        }
        Ok(self.path.clone())
    }
    pub fn capture(path: &Path) -> io::Result<Self> {
        let fingerprint = local::inspect_leaf(path)?;
        let symlink = fs::symlink_metadata(path)?.file_type().is_symlink();
        let path = if symlink {
            path.parent()
                .ok_or_else(|| unavailable(Unavailable::InvalidData))?
                .canonicalize()?
                .join(path.file_name().unwrap())
        } else {
            path.canonicalize()?
        };
        #[cfg(target_os = "macos")]
        let native = mac::Identity::capture(if symlink { path.parent().unwrap() } else { &path })?;
        #[cfg(windows)]
        let volume = windows::volume(&path)?;
        #[cfg(windows)]
        {
            // Establish that this volume accepts this exact file-ID mechanism
            // before issuing a durable token. Unsupported volumes fail here.
            let resolved = windows::resolve(&volume, fingerprint.identity[1])?;
            if local::inspect(&resolved)?.identity != fingerprint.identity {
                return Err(unavailable(Unavailable::Superseded));
            }
        }
        let result = Self {
            original: path.clone(),
            path,
            fingerprint,
            symlink,
            followed: Vec::new(),
            #[cfg(target_os = "macos")]
            native,
            #[cfg(windows)]
            volume,
        };
        if result.inspect(&result.path)? != result.fingerprint {
            return Err(unavailable(Unavailable::Superseded));
        }
        Ok(result)
    }
    pub fn capture_followed(path: &Path, followed: Vec<local::batch::Followed>) -> io::Result<Self> {
        for guard in &followed {
            guard.verify()?;
        }
        let mut location = Self::capture(path)?;
        location.followed = followed;
        Ok(location)
    }
    fn matches(&self, path: &Path, current: &local::Fingerprint) -> io::Result<bool> {
        if current.identity[1] != self.fingerprint.identity[1]
            || current.created != self.fingerprint.created
            || current.directory != self.fingerprint.directory
        {
            return Ok(false);
        }
        #[cfg(target_os = "macos")]
        return Ok(mac::volume_uuid(path)? == self.native.volume_uuid);
        #[cfg(windows)]
        return Ok(current.identity[0] == self.fingerprint.identity[0] && windows::volume(path)? == self.volume);
        #[cfg(not(any(target_os = "macos", windows)))]
        Ok(current.identity == self.fingerprint.identity)
    }
    fn inspect(&self, path: &Path) -> io::Result<local::Fingerprint> {
        if self.symlink {
            local::inspect_leaf(path)
        } else {
            local::inspect(path)
        }
    }
    pub fn resolve(&self) -> io::Result<PathBuf> {
        if !self.followed.is_empty() {
            for guard in &self.followed {
                guard.verify()?;
            }
            let current = self
                .inspect(&self.path)
                .map_err(|_| unavailable(Unavailable::Superseded))?;
            if current.identity != self.fingerprint.identity || current.created != self.fingerprint.created {
                return Err(unavailable(Unavailable::Superseded));
            }
            return Ok(self.path.clone());
        }
        // Links have no bookmark/target authority. Their parent is resolved by
        // the directory grant; never recover a link by resolving its target.
        if self.symlink {
            return (self.inspect(&self.path)? == self.fingerprint)
                .then(|| self.path.clone())
                .ok_or_else(|| unavailable(Unavailable::Superseded));
        }
        // The path is only a fast hint. Prefer the original object whenever it
        // still exists; atomic-save replacements are allowed only after Gone.
        if let Ok(current) = local::inspect(&self.path)
            && self.matches(&self.path, &current)?
        {
            return Ok(self.path.clone());
        }
        match self.resolve_identity() {
            Ok(path) => Ok(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let current = local::inspect(&self.original)?;
                if current.directory != self.fingerprint.directory {
                    return Err(unavailable(Unavailable::Unsupported));
                }
                #[cfg(target_os = "macos")]
                if mac::volume_uuid(&self.original)? != self.native.volume_uuid {
                    return Err(unavailable(Unavailable::Permission));
                }
                #[cfg(windows)]
                if windows::volume(&self.original)? != self.volume {
                    return Err(unavailable(Unavailable::Permission));
                }
                Ok(self.original.clone())
            }
            Err(error) => Err(error),
        }
    }
    fn resolve_identity(&self) -> io::Result<PathBuf> {
        #[cfg(target_os = "macos")]
        let path = self.native.resolve(self)?;
        #[cfg(windows)]
        let path = windows::resolve(&self.volume, self.fingerprint.identity[1])?;
        #[cfg(not(any(target_os = "macos", windows)))]
        let path = self.path.clone();
        let current = local::inspect(&path)?;
        if !self.matches(&path, &current)? {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        path.canonicalize()
    }
    pub fn refresh(&self, path: &Path) -> io::Result<Self> {
        let mut location = Self::capture(path)?;
        location.original = self.original.clone();
        location.followed = self.followed.clone();
        Ok(location)
    }
    pub fn current(&self, path: &Path, fingerprint: &local::Fingerprint) -> bool {
        self.path == path && self.fingerprint == *fingerprint
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use objc2_foundation::{
        NSData, NSString, NSURL, NSURLBookmarkCreationOptions as Creation,
        NSURLBookmarkResolutionOptions as Resolution, NSURLVolumeUUIDStringKey,
    };
    use std::{
        ffi::{CStr, CString, OsStr},
        os::unix::ffi::OsStrExt,
    };
    #[derive(Clone, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Identity {
        bookmark: Vec<u8>,
        volume_bookmark: Vec<u8>,
        pub volume_uuid: String,
    }
    fn url(path: &Path) -> io::Result<objc2::rc::Retained<NSURL>> {
        let text = path.to_str().ok_or_else(|| unavailable(Unavailable::InvalidData))?;
        Ok(NSURL::fileURLWithPath(&NSString::from_str(text)))
    }
    fn bookmark(path: &Path) -> io::Result<Vec<u8>> {
        // No implicit security scope: persisted links cannot grant additional
        // filesystem/TCC access or cause later resolution prompts.
        url(path)?
            .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
                Creation::WithoutImplicitSecurityScope,
                None,
                None,
            )
            .map(|data| data.to_vec())
            .map_err(|_| unavailable(Unavailable::Permission))
    }
    pub(super) fn volume_uuid(path: &Path) -> io::Result<String> {
        let mut value = None;
        // SAFETY: this resource key returns an NSString, checked by downcast.
        unsafe { url(path)?.getResourceValue_forKey_error(&mut value, NSURLVolumeUUIDStringKey) }
            .map_err(|_| unavailable(Unavailable::ReadFailed))?;
        value
            .and_then(|value| value.downcast::<NSString>().ok())
            .map(|value| value.to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| unavailable(Unavailable::Unsupported))
    }
    fn filesystem(path: &Path) -> io::Result<libc::statfs> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| unavailable(Unavailable::InvalidData))?;
        let mut result = std::mem::MaybeUninit::uninit();
        // SAFETY: NUL-terminated path and writable statfs output live for call.
        if unsafe { libc::statfs(path.as_ptr(), result.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful statfs initialized the entire output record.
        Ok(unsafe { result.assume_init() })
    }
    fn resolve_bookmark(bytes: &[u8]) -> io::Result<PathBuf> {
        // SAFETY: optional stale pointer is null; no UI, mounts or implicit access.
        let url = unsafe {
            NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
                &NSData::with_bytes(bytes),
                Resolution::WithoutUI | Resolution::WithoutMounting | Resolution::WithoutImplicitStartAccessing,
                None,
                std::ptr::null_mut(),
            )
        }
        .map_err(|_| unavailable(Unavailable::ReadFailed))?;
        // SAFETY: NSURL owns this NUL-terminated representation until the copy.
        let bytes = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) }.to_bytes();
        Ok(PathBuf::from(OsStr::from_bytes(bytes)))
    }
    impl Identity {
        pub(super) fn capture(path: &Path) -> io::Result<Self> {
            let filesystem = filesystem(path)?;
            // SAFETY: successful statfs returns a NUL-terminated mount name.
            let mount = unsafe { CStr::from_ptr(filesystem.f_mntonname.as_ptr()) }.to_bytes();
            let mount = Path::new(OsStr::from_bytes(mount));
            Ok(Self {
                bookmark: bookmark(path)?,
                volume_bookmark: bookmark(mount)?,
                volume_uuid: volume_uuid(path)?,
            })
        }
        pub(super) fn resolve(&self, location: &Location) -> io::Result<PathBuf> {
            if let Ok(path) = resolve_bookmark(&self.bookmark)
                && let Ok(current) = local::inspect(&path)
                && location.matches(&path, &current)?
            {
                return Ok(path);
            }
            // macOS 26.6.2: bookmarks can prefer a replacement at the old path;
            // fileReferenceURL-created bookmarks behave the same. Resolve the
            // saved volume bookmark, verify its stable UUID, then ask the kernel
            // for the original inode. No directory/disk scan or private option.
            // Neither a path fallback nor this lookup bypasses identity/scope.
            let mount = resolve_bookmark(&self.volume_bookmark)?;
            if volume_uuid(&mount)? != self.volume_uuid {
                return Err(unavailable(Unavailable::ReadFailed));
            }
            let mut filesystem = filesystem(&mount)?;
            unsafe extern "C" {
                fn fsgetpath(
                    buffer: *mut libc::c_char,
                    length: libc::size_t,
                    fsid: *mut libc::fsid_t,
                    object: u64,
                ) -> libc::ssize_t;
            }
            let mut buffer = vec![0; libc::PATH_MAX as usize];
            // SAFETY: valid bounded output and the mounted volume's current fsid.
            let length = unsafe {
                fsgetpath(
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &mut filesystem.f_fsid,
                    location.fingerprint.identity[1],
                )
            };
            if length < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fsgetpath success returns a NUL-terminated filesystem path.
            let bytes = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_bytes();
            Ok(PathBuf::from(OsStr::from_bytes(bytes)))
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use ::windows::{
        Win32::{Foundation::HANDLE, Storage::FileSystem::*},
        core::PCWSTR,
    };
    use std::os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    };
    pub(super) fn volume(path: &Path) -> io::Result<PathBuf> {
        let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut mount = vec![0; 32768];
        let mut volume = [0; 128];
        // SAFETY: NUL-terminated input and writable output buffers survive calls.
        unsafe {
            GetVolumePathNameW(PCWSTR(path.as_ptr()), &mut mount).map_err(io::Error::other)?;
            GetVolumeNameForVolumeMountPointW(PCWSTR(mount.as_ptr()), &mut volume).map_err(io::Error::other)?;
        }
        let end = volume
            .iter()
            .position(|c| *c == 0)
            .ok_or_else(|| unavailable(Unavailable::InvalidData))?;
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&volume[..end])))
    }
    pub(super) fn resolve(volume: &Path, id: u64) -> io::Result<PathBuf> {
        // A directory handle on the stored volume is sufficient as the hint;
        // no raw-volume handle, privilege changes, or disk scan is necessary.
        let hint = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(volume)
            .map_err(|_| unavailable(Unavailable::ReadFailed))?;
        let file = match open_id(&hint, id) {
            Ok(file) => file,
            Err(error) => {
                // Windows 11/NTFS returns ERROR_INVALID_PARAMETER for a deleted
                // file ID. Capture already proved this ID/volume mechanism; a
                // second lookup of the live volume-root ID distinguishes Gone
                // from an unsupported API/volume or invalid handle. Do not turn
                // generic errors into permission to read a path replacement.
                let reason = match error.code().0 as u32 {
                    0x80070002 | 0x80070003 => Unavailable::NoSuchItem,
                    0x80070057 if open_id(&hint, local::file_fingerprint(&hint)?.identity[1]).is_ok() => {
                        Unavailable::NoSuchItem
                    }
                    0x80070005 => Unavailable::Permission,
                    _ => Unavailable::ReadFailed,
                };
                return Err(unavailable(reason));
            }
        };
        let mut path = vec![0; 32768];
        // SAFETY: live owned handle and writable bounded UTF-16 buffer.
        let count =
            unsafe { GetFinalPathNameByHandleW(HANDLE(file.as_raw_handle()), &mut path, VOLUME_NAME_DOS) } as usize;
        if count == 0 {
            return Err(io::Error::last_os_error());
        }
        if count >= path.len() {
            return Err(unavailable(Unavailable::LimitExceeded));
        }
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&path[..count])))
    }
    fn open_id(hint: &File, id: u64) -> ::windows::core::Result<File> {
        let descriptor = FILE_ID_DESCRIPTOR {
            dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: id as i64 },
        };
        // SAFETY: borrowed live hint and initialized descriptor remain valid.
        let handle = unsafe {
            OpenFileById(
                HANDLE(hint.as_raw_handle()),
                &descriptor,
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        }?;
        // SAFETY: successful OpenFileById transfers a unique owned handle.
        Ok(unsafe { File::from_raw_handle(handle.0) })
    }
}
