use super::super::{Handle, check, denied, security, token, wide};
use oink_update::{Error, feed::hex};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::*, Security::Authorization::*, Security::*, Storage::FileSystem::*, System::Threading::*,
};

pub(super) fn system() -> io::Result<()> {
    // SAFETY: the pseudo handle refers to this process, and token() owns its token handle.
    let sid = security::sid_bytes(token(unsafe { GetCurrentProcess() })?.0)?;
    // SAFETY: SID bytes came from a validated Windows token.
    if unsafe { IsWellKnownSid(sid.as_ptr().cast_mut().cast(), WinLocalSystemSid) } == 0 {
        return Err(denied());
    }
    Ok(())
}
pub(in crate::platform) fn root() -> io::Result<PathBuf> {
    // Use the OS known folder, never ProgramFiles from a caller's environment.
    let parent = security::install_dir()?.parent().ok_or_else(denied)?.to_owned();
    Ok(parent.join("Oink"))
}
pub(super) fn maintenance() -> io::Result<PathBuf> {
    Ok(root()?.join("maintenance"))
}
fn acl(handle: HANDLE) -> io::Result<()> {
    let (mut owner, mut dacl, mut descriptor) = (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
    // SAFETY: opened handle has READ_CONTROL; Windows allocates descriptor and returns interior pointers.
    unsafe {
        let code = GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        );
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        let result = acl_policy(owner, dacl);
        LocalFree(descriptor);
        result
    }
}
// Pointers originate only from Windows-parsed security descriptors and remain
// live through this call. The policy can be exercised without creating a tree.
fn acl_policy(owner: windows_sys::Win32::Security::PSID, dacl: *mut ACL) -> io::Result<()> {
    // SAFETY: caller retains the OS-validated descriptor containing both pointers.
    unsafe {
        if owner.is_null() || dacl.is_null() || !security::privileged_sid(owner) {
            return Err(denied());
        }
        for index in 0..u32::from((*dacl).AceCount) {
            let mut entry = ptr::null_mut();
            check(GetAce(dacl, index, &mut entry))?;
            let header = &*entry.cast::<ACE_HEADER>();
            if header.AceFlags & INHERIT_ONLY_ACE as u8 != 0 {
                continue;
            }
            let sid = match header.AceType {
                0 | 9 => ptr::addr_of!((*entry.cast::<ACCESS_ALLOWED_ACE>()).SidStart)
                    .cast_mut()
                    .cast(),
                5 | 11 => {
                    let flags = ptr::addr_of!((*entry.cast::<ACCESS_ALLOWED_OBJECT_ACE>()).Flags).read();
                    let mut offset = std::mem::offset_of!(ACCESS_ALLOWED_OBJECT_ACE, ObjectType);
                    if flags & ACE_OBJECT_TYPE_PRESENT != 0 {
                        offset += size_of::<windows_sys::core::GUID>();
                    }
                    if flags & ACE_INHERITED_OBJECT_TYPE_PRESENT != 0 {
                        offset += size_of::<windows_sys::core::GUID>();
                    }
                    entry.cast::<u8>().add(offset).cast()
                }
                _ => continue, // Deny/audit ACEs do not grant writes.
            };
            {
                let allowed = &*entry.cast::<ACCESS_ALLOWED_ACE>();
                let write = GENERIC_ALL
                    | GENERIC_WRITE
                    | DELETE
                    | WRITE_DAC
                    | WRITE_OWNER
                    | FILE_WRITE_DATA
                    | FILE_APPEND_DATA
                    | FILE_WRITE_EA
                    | FILE_WRITE_ATTRIBUTES
                    | FILE_DELETE_CHILD;
                if allowed.Mask & write != 0 && !security::privileged_sid(sid) {
                    return Err(denied());
                }
            }
        }
        Ok(())
    }
}
fn inspect(file: &File, directory: bool) -> io::Result<()> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: owned file handle and correctly sized writable metadata buffer.
    check(unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) })?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        || (!directory && info.nNumberOfLinks != 1)
    {
        return Err(denied());
    }
    acl(file.as_raw_handle())
}
fn open_one(path: &Path, directory: bool) -> io::Result<File> {
    let file = open_raw(path, directory)?;
    inspect(&file, directory)?;
    Ok(file)
}
fn open_raw(path: &Path, directory: bool) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .access_mode(FILE_READ_DATA | FILE_READ_ATTRIBUTES | READ_CONTROL)
        .share_mode(FILE_SHARE_READ | if directory { FILE_SHARE_WRITE } else { 0 })
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | if directory { FILE_FLAG_BACKUP_SEMANTICS } else { 0 })
        .open(path)
}
/// Only fixed paths beneath the OS installation root are accepted.
pub(in crate::platform) fn open(path: &Path) -> io::Result<File> {
    let _parents = pin_parents(path, true)?;
    open_one(path, false)
}
/// Retains the protected maintenance directory and file until msiexec exits. FILE_SHARE_READ
/// permits installation reads while denying content writes, deletion and rename.
pub(super) struct PinnedFile {
    pub file: File,
    _parents: Vec<File>,
}
pub(super) fn pin(path: &Path) -> io::Result<PinnedFile> {
    let parents = pin_parents(path, true)?;
    Ok(PinnedFile {
        file: open_one(path, false)?,
        _parents: parents,
    })
}
pub(super) fn bytes(file: &mut File, max: u64) -> Result<Vec<u8>, Error> {
    if file.metadata().map_err(|_| Error::Storage)?.len() > max {
        return Err(Error::Limit);
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes).map_err(|_| Error::Storage)?;
    if bytes.len() as u64 > max {
        return Err(Error::Limit);
    }
    Ok(bytes)
}
fn pin_parents(path: &Path, product: bool) -> io::Result<Vec<File>> {
    if (product && !path.starts_with(root()?))
        || path.components().any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(denied());
    }
    let maintenance = maintenance()?;
    if path.starts_with(&maintenance) && path != maintenance {
        // Administrators/SYSTEM are trusted. Check the maintenance boundary,
        // not every ancestor of Program Files or every transaction directory.
        return Ok(vec![open_one(&maintenance, true)?]);
    }
    Ok(Vec::new())
}
pub(super) fn directory(path: &Path) -> io::Result<()> {
    let _parents = pin_parents(path, true)?;
    if path.exists() {
        return open_one(path, true).map(|_| ());
    }
    let parent = path.parent().ok_or_else(denied)?;
    let _parent = open_one(parent, true)?;
    let descriptor = security::staging_security()?;
    let attrs = descriptor.attrs();
    // SAFETY: trusted fixed child path, explicit protected SYSTEM-only DACL.
    check(unsafe { CreateDirectoryW(wide(path.to_str().ok_or_else(denied)?).as_ptr(), &attrs) })?;
    open_one(path, true).map(|_| ())
}
pub(super) fn read(path: &Path, max: u64) -> Result<Vec<u8>, Error> {
    let mut pinned = pin(path).map_err(|_| Error::Storage)?;
    bytes(&mut pinned.file, max)
}
pub(super) fn hash(path: &Path) -> Result<String, Error> {
    let mut file = open(path).map_err(|_| Error::Storage)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).map_err(|_| Error::Storage)?;
    Ok(hex(&hasher.finalize()))
}
pub(super) fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let _parents = pin_parents(path, true).map_err(|_| Error::Storage)?;
    let parent = path.parent().ok_or(Error::Storage)?;
    let _parent = open_one(parent, true).map_err(|_| Error::Storage)?;
    let temporary = path.with_extension("new");
    if temporary.exists() {
        // Interrupted write from this protected writer. Validate before removal;
        // sharing denial prevents deleting another writer's in-progress file.
        let stale = open(&temporary).map_err(|_| Error::Storage)?;
        drop(stale);
        std::fs::remove_file(&temporary).map_err(|_| Error::Storage)?;
    }
    // create_new rejects stale files and reparse/hardlink substitutions. The
    // directory's protected DACL applies before any content is written.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH)
        .open(&temporary)
        .map_err(|_| Error::Storage)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| Error::Storage)?;
    drop(file);
    let _verified = open(&temporary).map_err(|_| Error::Storage)?;
    drop(_verified);
    // SAFETY: both fixed children of the pinned protected directory; MOVEFILE_WRITE_THROUGH flushes the move.
    let ok = unsafe {
        MoveFileExW(
            wide(temporary.to_str().ok_or(Error::Storage)?).as_ptr(),
            wide(path.to_str().ok_or(Error::Storage)?).as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(Error::Storage);
    }
    Ok(())
}
pub(super) fn lock() -> Result<Handle, Error> {
    let directory = maintenance().map_err(|_| Error::Storage)?;
    let _parents = pin_parents(&directory, true).map_err(|_| Error::Storage)?;
    let _parent = open_one(&directory, true).map_err(|_| Error::Storage)?;
    let path = directory.join("maintenance.lock");
    let descriptor = security::system_security().map_err(|_| Error::Storage)?;
    let mut attrs = descriptor.attrs();
    attrs.bInheritHandle = 1;
    // SAFETY: fixed protected lock, exclusive sharing, inherited only into the old verified worker.
    let handle = Handle::new(unsafe {
        CreateFileW(
            wide(path.to_str().ok_or(Error::Storage)?).as_ptr(),
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
            0,
            &attrs,
            OPEN_ALWAYS,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
            ptr::null_mut(),
        )
    })
    .map_err(|_| Error::Busy)?;
    acl(handle.0).map_err(|_| Error::Storage)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: owned handle, writable information struct.
    check(unsafe { GetFileInformationByHandle(handle.0, &mut info) }).map_err(|_| Error::Storage)?;
    if info.nNumberOfLinks != 1
        || info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0
    {
        return Err(Error::Storage);
    }
    Ok(handle)
}
pub(super) fn created(process: HANDLE) -> Result<u64, Error> {
    let (mut creation, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: pinned live process handle and writable FILETIMEs.
    check(unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) })
        .map_err(|_| Error::Storage)?;
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}
/// Task Scheduler's actual DACL, obtained with IRegisteredTask::GetSecurityDescriptor.
pub(super) fn system_only_sddl(sddl: &str) -> io::Result<()> {
    let mut descriptor = ptr::null_mut();
    // SAFETY: parser allocates a descriptor from the OS-returned SDDL.
    check(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(sddl).as_ptr(), 1, &mut descriptor, ptr::null_mut())
    })?;
    // SAFETY: parsed descriptor and interior ACL remain live until LocalFree.
    unsafe {
        let result = (|| {
            let (mut acl, mut present, mut defaulted) = (ptr::null_mut(), 0, 0);
            check(GetSecurityDescriptorDacl(
                descriptor,
                &mut present,
                &mut acl,
                &mut defaulted,
            ))?;
            if present == 0 || acl.is_null() || (*acl).AceCount == 0 {
                return Err(denied());
            }
            for index in 0..u32::from((*acl).AceCount) {
                let mut entry = ptr::null_mut();
                check(GetAce(acl, index, &mut entry))?;
                let header = &*entry.cast::<ACE_HEADER>();
                if header.AceType == 0 {
                    let ace = &*entry.cast::<ACCESS_ALLOWED_ACE>();
                    if IsWellKnownSid(ptr::addr_of!(ace.SidStart).cast_mut().cast(), WinLocalSystemSid) == 0 {
                        return Err(denied());
                    }
                } else if header.AceType != 1 {
                    return Err(denied());
                }
            }
            Ok(())
        })();
        LocalFree(descriptor);
        result
    }
}
/// Windows Installer's protected cache is the only additional read-only root.
/// Paths come exclusively from MsiGetProductInfo(LocalPackage), never IPC.
pub(super) fn cached_msi(path: &Path, installer: &Path) -> io::Result<File> {
    if !path
        .parent()
        .is_some_and(|p| p.as_os_str().eq_ignore_ascii_case(installer.as_os_str()))
        || !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("msi"))
    {
        return Err(denied());
    }
    let _parents = pin_parents(path, false)?;
    open_one(path, false)
}

/// Remove only this updater's obsolete protected trees, after acquiring the
/// maintenance lock and establishing that no transaction is pending.
pub(super) fn prune(current_build: u64) -> Result<(), Error> {
    let maintenance = maintenance().map_err(|_| Error::Storage)?;
    for (name, transactions) in [("transactions", true), ("cache", false)] {
        let root = maintenance.join(name);
        if !root.exists() {
            continue;
        }
        let _root = open_one(&root, true).map_err(|_| Error::Storage)?;
        for child in std::fs::read_dir(&root).map_err(|_| Error::Storage)? {
            let child = child.map_err(|_| Error::Storage)?;
            let Some(name) = child.file_name().to_str().map(str::to_owned) else {
                return Err(Error::Storage);
            };
            let obsolete = if transactions {
                super::valid_transaction(&name)
            } else {
                name.parse::<u64>()
                    .is_ok_and(|build| build != current_build && build.to_string() == name)
            };
            if obsolete {
                remove_tree(&child.path())?;
            }
        }
    }
    Ok(())
}
fn remove_tree(path: &Path) -> Result<(), Error> {
    let directory = open_one(path, true).map_err(|_| Error::Storage)?;
    for child in std::fs::read_dir(path).map_err(|_| Error::Storage)? {
        let child = child.map_err(|_| Error::Storage)?;
        if child.file_type().map_err(|_| Error::Storage)?.is_dir() {
            remove_tree(&child.path())?;
        } else {
            let file = open(&child.path()).map_err(|_| Error::Storage)?;
            drop(file);
            std::fs::remove_file(child.path()).map_err(|_| Error::Storage)?;
        }
    }
    drop(directory);
    std::fs::remove_dir(path).map_err(|_| Error::Storage)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staging_acl_rejects_user_ownership_and_any_unprivileged_write_right() {
        let inspect = |sddl: &str| -> io::Result<()> {
            let mut descriptor = ptr::null_mut();
            // SAFETY: in-memory descriptor parser, with no filesystem/security setting changes.
            check(unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide(sddl).as_ptr(),
                    1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            })?;
            let result = (|| {
                let (mut owner, mut dacl, mut present, mut defaulted) = (ptr::null_mut(), ptr::null_mut(), 0, 0);
                // SAFETY: descriptor remains allocated through the complete policy check.
                unsafe {
                    check(GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted))?;
                    check(GetSecurityDescriptorDacl(
                        descriptor,
                        &mut present,
                        &mut dacl,
                        &mut defaulted,
                    ))?;
                }
                if present == 0 {
                    return Err(denied());
                }
                acl_policy(owner, dacl)
            })();
            // SAFETY: free exactly the descriptor allocated by the parser.
            unsafe {
                LocalFree(descriptor);
            }
            result
        };
        assert!(inspect("O:SYG:SYD:P(A;OICI;FA;;;SY)").is_ok());
        assert!(inspect("O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)").is_ok());
        assert!(inspect("O:SYG:SYD:P(D;;FA;;;BU)(A;;FA;;;SY)").is_ok());
        assert!(inspect("O:SYG:SYD:P(A;;FA;;;SY)(OA;;GR;;;AU)").is_ok());
        assert!(inspect("O:SYG:SYD:P(A;;FA;;;SY)(OA;;GR;00000000-0000-0000-0000-000000000001;;AU)").is_ok());
        for descriptor in [
            "O:BUG:BUD:P(A;;FA;;;SY)",
            "O:SYG:SYD:P(A;;FA;;;SY)(A;;GW;;;BU)",
            "O:SYG:SYD:P(A;;FA;;;SY)(A;;WD;;;WD)",
            "O:SYG:SYD:P(A;;FA;;;SY)(OA;;GW;;;AU)",
            "O:SYG:SYD:P(A;;FA;;;SY)(OA;;GW;00000000-0000-0000-0000-000000000001;;AU)",
            "O:SYG:SYD:NO_ACCESS_CONTROL",
        ] {
            assert!(inspect(descriptor).is_err(), "{descriptor}");
        }
    }
    #[test]
    fn native_task_dacl_requires_system_only() {
        assert!(system_only_sddl("O:SYG:SYD:P(A;;GA;;;SY)").is_ok());
        for descriptor in [
            "O:SYG:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)",
            "O:SYG:SYD:P(A;;GA;;;WD)",
            "O:SYG:SYD:P",
        ] {
            assert!(system_only_sddl(descriptor).is_err());
        }
    }
    #[test]
    fn staging_handle_denies_writes_rename_and_delete_but_allows_installer_reads() {
        let directory = std::env::temp_dir().join(format!("oink-pin-unit-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("package.msi");
        std::fs::write(&path, b"isolated bytes, never executed").unwrap();
        // Same open flags as production, bypassing only its installation-root
        // and privileged-owner policy for this isolated ordinary-user fixture.
        let pinned = PinnedFile {
            file: open_raw(&path, false).unwrap(),
            _parents: vec![open_raw(&directory, true).unwrap()],
        };
        assert!(std::fs::read(&path).is_ok());
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        assert!(std::fs::rename(&path, directory.join("replaced.msi")).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(&directory, directory.with_extension("moved")).is_err());
        drop(pinned);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn actual_hardlink_and_untrusted_root_are_rejected_without_product_access() {
        let directory = std::env::temp_dir().join(format!("oink-update-unit-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let file = directory.join("payload");
        let link = directory.join("alias");
        std::fs::write(&file, b"isolated test").unwrap();
        std::fs::hard_link(&file, &link).unwrap();
        let handle = File::open(&file).unwrap();
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: isolated test file handle, writable metadata storage.
        let queried = unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) };
        assert_ne!(queried, 0);
        assert_eq!(info.nNumberOfLinks, 2);
        assert!(inspect(&handle, false).is_err());
        assert!(open(&file).is_err());
        drop(handle);
        std::fs::remove_file(link).unwrap();
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
