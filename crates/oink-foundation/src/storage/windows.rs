use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::{
    ffi::OsStrExt,
    io::{AsRawHandle, FromRawHandle},
};
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FileRenameInfoEx, SetFileInformationByHandle,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::WindowsProgramming::{
    FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
};

struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: this pointer was allocated by a LocalAlloc-based security API.
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn user_sid() -> io::Result<String> {
    // SAFETY: token and SID buffers remain alive for all calls; owned handles and
    // LocalAlloc buffers are released on every return path.
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut size = 0;
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut size);
            // usize storage provides TOKEN_USER alignment; the SID remains in the
            // same buffer until ConvertSidToStringSidW has copied it.
            let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
            if GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), size, &mut size) == 0 {
                return Err(io::Error::last_os_error());
            }
            let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
            let mut string = null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut string) == 0 {
                return Err(io::Error::last_os_error());
            }
            let _allocation = LocalAllocation(string.cast());
            let mut len = 0;
            while *string.add(len) != 0 {
                len += 1;
            }
            Ok(String::from_utf16_lossy(std::slice::from_raw_parts(string, len)))
        })();
        CloseHandle(token);
        result
    }
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut result: Vec<u16> = path.as_os_str().encode_wide().collect();
    if result.contains(&0) {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    result.push(0);
    Ok(result)
}

pub(super) fn create_private_new(path: &Path) -> io::Result<File> {
    let path = wide(path)?;
    let sid = user_sid()?;
    // Protected DACL, one full-access ACE for the current user, no inherited
    // grants. Passing it to CreateFileW avoids any wide-then-tighten interval.
    let descriptor: Vec<u16> = format!("O:{sid}D:P(A;;FA;;;{sid})\0").encode_utf16().collect();
    // SAFETY: UTF-16 strings are terminated; the descriptor and attributes outlive
    // CreateFileW, and the returned handle transfers exactly once to File.
    unsafe {
        let mut security = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(descriptor.as_ptr(), 1, &mut security, null_mut()) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let _allocation = LocalAllocation(security);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security,
            bInheritHandle: 0,
        };
        let handle = CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE | DELETE,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(File::from_raw_handle(handle))
    }
}

pub(super) fn replace(file: &File, destination: &Path) -> io::Result<()> {
    // Keep the original private writer through publication. Closing and reopening the
    // staging path with DELETE lets a scanner or reader acquire a non-delete-
    // sharing handle in between, making an otherwise valid write fail. Obtain
    // DELETE at creation and rename on that same handle. Read sharing keeps the
    // published path readable before the writer closes, while DELETE ownership
    // requires any admitted reader to permit rename. Verified on Windows 11.
    let destination = wide(&std::path::absolute(destination)?)?;
    let bytes = (destination.len() - 1) * size_of::<u16>();
    let length = std::mem::offset_of!(FILE_RENAME_INFO, FileName) + destination.len() * size_of::<u16>();
    let mut buffer = vec![0usize; length.div_ceil(size_of::<usize>())];
    // MoveFileEx replacement fails with ERROR_ACCESS_DENIED while an ordinary
    // reader holds the destination, even with FILE_SHARE_DELETE. Windows 10's
    // POSIX rename retains that reader's old snapshot and atomically installs
    // the already-flushed private file, including its owner-only DACL.
    // SAFETY: usize storage aligns FILE_RENAME_INFO and includes the complete
    // variable-length UTF-16 tail. The file handle and buffer outlive the call.
    let renamed = unsafe {
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        (*info).Anonymous.Flags = FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*info).RootDirectory = null_mut();
        (*info).FileNameLength = bytes as u32;
        std::ptr::copy_nonoverlapping(destination.as_ptr(), (*info).FileName.as_mut_ptr(), destination.len());
        SetFileInformationByHandle(file.as_raw_handle(), FileRenameInfoEx, info.cast(), length as u32)
    };
    if renamed == 0 {
        return Err(io::Error::last_os_error());
    }
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
        OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    };

    #[test]
    fn original_private_writer_can_publish_without_reopening() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join("settings.tmp");
        let destination = directory.path().join("settings.json");
        let mut writer = create_private_new(&temporary).unwrap();
        writer.write_all(b"private settings").unwrap();
        writer.sync_all().unwrap();
        let published = replace(&writer, &destination);
        let observed = std::fs::read(&destination);
        drop(writer);
        assert!(
            published.is_ok(),
            "publication must use the original private handle: {published:?}"
        );
        assert_eq!(observed.unwrap(), b"private settings");
    }

    #[test]
    fn replacement_keeps_exact_utf16_names_at_each_alignment() {
        let directory = tempfile::tempdir().unwrap();
        for suffix in ["", "x", "xx", "xxx"] {
            let path = directory.path().join(format!("配置{suffix}.json"));
            super::super::atomic_write(&path, b"first").unwrap();
            super::super::atomic_write(&path, b"second").unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"second");
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 4);
    }

    fn assert_owner_only(path: &Path) {
        // SAFETY: security pointers belong to the returned descriptor allocation,
        // retained until all ACL and SID inspections are complete.
        unsafe {
            let path = wide(path).unwrap();
            let mut owner = null_mut();
            let mut acl = null_mut();
            let mut descriptor = null_mut();
            assert_eq!(
                GetNamedSecurityInfoW(
                    path.as_ptr(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut owner,
                    null_mut(),
                    &mut acl,
                    null_mut(),
                    &mut descriptor
                ),
                0
            );
            let _allocation = LocalAllocation(descriptor);
            let mut control = 0;
            let mut revision = 0;
            assert_ne!(GetSecurityDescriptorControl(descriptor, &mut control, &mut revision), 0);
            assert_ne!(control & SE_DACL_PROTECTED, 0);
            assert!(!acl.is_null());
            assert_eq!((*acl).AceCount, 1);
            let mut ace = null_mut();
            assert_ne!(GetAce(acl, 0, &mut ace), 0);
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(ace.Header.AceType, 0);
            assert_eq!(ace.Header.AceFlags, 0);
            assert_ne!(EqualSid(owner, (&ace.SidStart as *const u32).cast_mut().cast()), 0);
            let mut owner_text = null_mut();
            assert_ne!(ConvertSidToStringSidW(owner, &mut owner_text), 0);
            let _owner_allocation = LocalAllocation(owner_text.cast());
            let mut len = 0;
            while *owner_text.add(len) != 0 {
                len += 1;
            }
            assert_eq!(
                String::from_utf16_lossy(std::slice::from_raw_parts(owner_text, len)),
                user_sid().unwrap()
            );
        }
    }

    #[test]
    fn key_and_open_temporary_have_protected_owner_only_dacl() {
        let dir = tempfile::tempdir().unwrap();
        let temporary = dir.path().join("device.key.tmp");
        let file = create_private_new(&temporary).unwrap();
        // Inspect the actual open staging file, before writing or renaming.
        assert_owner_only(&temporary);
        drop(file);
        let key = dir.path().join("device.key");
        super::super::atomic_write(&key, &[42; 32]).unwrap();
        assert_owner_only(&key);
        super::super::atomic_write(&key, &[43; 32]).unwrap();
        assert_owner_only(&key);
    }
}
