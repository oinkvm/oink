//! Delete by handle without following name-surrogate reparse points or changing
//! attributes on a possibly hard-linked source. Pin every traversed directory.
use std::{
    fs::{self, File, OpenOptions},
    io,
    mem::size_of,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
};
use windows_sys::Win32::Storage::FileSystem::*;

fn open(path: &Path, delete: bool) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | if delete { DELETE } else { 0 })
        // Deny rename/replacement while resolving children beneath this handle.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}
fn info(file: &File) -> io::Result<FILE_ATTRIBUTE_TAG_INFO> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: the open file and the fixed-size output buffer live through the call.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}
fn traversable(info: &FILE_ATTRIBUTE_TAG_INFO) -> bool {
    info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
        && (info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 || info.ReparseTag & !0xf000 == 0x9000001a)
}
pub(super) fn pin_directory(path: &Path) -> io::Result<File> {
    let guard = open(path, false)?;
    if !traversable(&info(&guard)?) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(guard)
}
pub(super) fn clear_directory(path: &Path) -> io::Result<()> {
    let _guard = pin_directory(path)?;
    children(path)
}
fn children(path: &Path) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        remove(&entry?.path())?;
    }
    Ok(())
}
pub(super) fn remove(path: &Path) -> io::Result<()> {
    let file = match open(path, true) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let metadata = info(&file)?;
    if traversable(&metadata) {
        children(path)?;
    }
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: deletion affects this OPEN_REPARSE_POINT handle, never a link target.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // A private NTFS junction needs no symlink privilege or sync-root registration.
    fn junction(link: &Path, target: &Path) -> io::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{Foundation::GENERIC_WRITE, System::IO::DeviceIoControl};
        fs::create_dir(link)?;
        let target = target.canonicalize()?;
        let raw = target.as_os_str().encode_wide().collect::<Vec<_>>();
        // canonicalize produces an extended-length \\?\ path; mount points use \??\.
        assert_eq!(&raw[..4], &[92, 92, 63, 92]);
        let mut substitute = vec![92u16, 63, 63, 92];
        substitute.extend_from_slice(&raw[4..]);
        let print = &raw[4..];
        let sub_bytes = (substitute.len() * 2) as u16;
        let print_bytes = (print.len() * 2) as u16;
        let mut buffer = 0xa0000003u32.to_le_bytes().to_vec(); // IO_REPARSE_TAG_MOUNT_POINT
        buffer.extend((8u16 + sub_bytes + print_bytes + 4).to_le_bytes());
        buffer.extend(0u16.to_le_bytes()); // reserved
        for field in [0u16, sub_bytes, sub_bytes + 2, print_bytes] {
            buffer.extend(field.to_le_bytes());
        }
        for ch in substitute
            .into_iter()
            .chain([0])
            .chain(print.iter().copied())
            .chain([0])
        {
            buffer.extend(ch.to_le_bytes());
        }
        let file = OpenOptions::new()
            .access_mode(GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(link)?;
        let mut returned = 0;
        // SAFETY: the handle and encoded MOUNT_POINT_REPARSE_BUFFER remain live;
        // the request creates a link only in this test's private temporary tree.
        if unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                0x900a4,
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[test]
    fn junctions_are_unlinked_and_redirected_roots_are_rejected() {
        let owned = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("source"), b"original").unwrap();
        junction(&owned.path().join("link"), source.path()).unwrap();
        clear_directory(owned.path()).unwrap();
        assert_eq!(fs::read(source.path().join("source")).unwrap(), b"original");
        junction(&owned.path().join("redirected"), source.path()).unwrap();
        assert!(clear_directory(&owned.path().join("redirected")).is_err());
        assert!(super::super::prepare(&owned.path().join("redirected"), || panic!()).is_err());
        assert!(!source.path().join(super::super::MARKER).exists());
        assert_eq!(fs::read(source.path().join("source")).unwrap(), b"original");
    }

    #[test]
    fn readonly_hardlink_is_removed_without_changing_the_source() {
        let owned = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let original = source.path().join("original");
        fs::write(&original, b"source original").unwrap();
        fs::hard_link(&original, owned.path().join("received")).unwrap();
        let mut permissions = fs::metadata(&original).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&original, permissions).unwrap();
        clear_directory(owned.path()).unwrap();
        assert!(fs::metadata(&original).unwrap().permissions().readonly());
        assert_eq!(fs::read(&original).unwrap(), b"source original");
        assert_eq!(fs::read_dir(owned.path()).unwrap().count(), 0);
        remove(&original).unwrap(); // private fixture only
    }
}
