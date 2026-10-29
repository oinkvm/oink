use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::{
    ffi::OsStrExt,
    io::{AsRawHandle, FromRawHandle},
};
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_RENAME_INFO,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileRenameInfoEx, OPEN_ALWAYS, OPEN_EXISTING,
    SetFileInformationByHandle,
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
    open_private(
        path,
        GENERIC_WRITE | DELETE,
        FILE_SHARE_READ | FILE_SHARE_DELETE,
        CREATE_NEW,
    )
}

pub(super) fn open_private_lock(path: &Path) -> io::Result<File> {
    open_private(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, OPEN_ALWAYS)
}

fn open_private(path: &Path, access: u32, sharing: u32, disposition: u32) -> io::Result<File> {
    let path = wide(path)?;
    let sid = user_sid()?;
    // Protected DACL, one full-access ACE for the current user, no inherited
    // grants. A Medium minimum admits the same user's Medium and High processes
    // regardless of which created the file. No-read/write-up blocks Low callers
    // from reading IPC secrets or modifying shared state. This is not an elevation
    // grant: the exact user SID DACL still excludes other ordinary users.
    // Passing it to CreateFileW avoids any wide-then-tighten interval.
    let descriptor: Vec<u16> = format!("O:{sid}D:P(A;;FA;;;{sid})S:(ML;;NWNR;;;ME)\0")
        .encode_utf16()
        .collect();
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
            access,
            sharing,
            &attributes,
            disposition,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(File::from_raw_handle(handle))
    }
}

pub(super) fn replace(file: &File, destination: &Path, log_retries: bool) -> io::Result<()> {
    // Keep the original private writer through publication. Closing and reopening the
    // staging path with DELETE lets a scanner or reader acquire a non-delete-
    // sharing handle in between, making an otherwise valid write fail. Obtain
    // DELETE at creation and rename on that same handle. Read sharing keeps the
    // published path readable before the writer closes, while DELETE ownership
    // requires any admitted reader to permit rename. Verified on Windows 11.
    let destination_path = std::path::absolute(destination)?;
    let destination_name = wide(&destination_path)?;
    let bytes = (destination_name.len() - 1) * size_of::<u16>();
    let length = std::mem::offset_of!(FILE_RENAME_INFO, FileName) + destination_name.len() * size_of::<u16>();
    let mut buffer = vec![0usize; length.div_ceil(size_of::<usize>())];
    // MoveFileEx replacement fails with ERROR_ACCESS_DENIED while an ordinary
    // reader holds the destination, even with FILE_SHARE_DELETE. Windows 10's
    // POSIX rename retains that reader's old snapshot and atomically installs
    // the already-flushed private file, including its owner-only DACL.
    // SAFETY: usize storage aligns FILE_RENAME_INFO and includes the complete
    // variable-length UTF-16 tail. The file handle and buffer outlive the call.
    unsafe {
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        (*info).Anonymous.Flags = FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*info).RootDirectory = null_mut();
        (*info).FileNameLength = bytes as u32;
        std::ptr::copy_nonoverlapping(
            destination_name.as_ptr(),
            (*info).FileName.as_mut_ptr(),
            destination_name.len(),
        );
    }
    // Windows 11 Defender (MsMpEng) can retain the previous published FILE_OBJECT
    // after our writer closes: zero-loss ETW observed FSCTL_SET_PURGE_FAILURE_MODE
    // on it and replace-over-target STATUS_ACCESS_DENIED until final release.
    // Keep this original DELETE-capable staging handle and retry ONLY that native
    // rename with both STATUS_ACCESS_DENIED and ERROR_ACCESS_DENIED. Observed
    // holds were ~1.3–1.4 s; a 5 s total budget leaves scheduling headroom without
    // waiting indefinitely. 10 ms doubling to 250 ms handles short scans promptly
    // without busy polling. Creation, writes and flushes are never retried.
    // MoveFileExW failed 8/8 live cases; ReplaceFileW needed the staging handle
    // closed and exposed 18,247 missing-target reads in 2,000 replacements, as
    // well as copying the old target ACL. Neither is a fallback. This preserves
    // atomic reader snapshots, private DACLs and the single-file layout; Unix is
    // unchanged. Rationale, ACL-check limits and evidence: see
    // docs/design/windows-atomic-publication.md. Verified on Windows 11.
    let started = std::time::Instant::now();
    let outcome = super::windows_retry::publish_with_deadline(
        super::windows_retry::native_deadline(),
        || {
            // Windows 11 can leave a zero-handle destination unreplaceable after
            // Defender's purge-failure-mode control, even after the scanned
            // FILE_OBJECT closes. Keep a DELETE handle to the current target
            // across the POSIX rename so Windows retains its old snapshot.
            // Opening then closing did not help; holding it did in four live
            // cases.
            // Reacquire for each native attempt because another writer may
            // have replaced the target. All sharing is allowed, the original
            // staging writer stays open, and deadlines/retry counts are unchanged.
            // The guard is best-effort: absent or inaccessible targets still
            // receive the native rename's authoritative permission/share checks.
            let _destination = retain_destination(&destination_name);
            // SAFETY: the aligned rename buffer and original file remain alive
            // and unchanged across all synchronous calls.
            let renamed = unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    FileRenameInfoEx,
                    buffer.as_ptr().cast(),
                    length as u32,
                )
            };
            if renamed != 0 {
                return Ok(());
            }
            // SAFETY: read both thread-local error values immediately after the
            // failed native publication, before any other filesystem operation.
            let (code, status) = unsafe { (GetLastError(), RtlGetLastNtStatus()) };
            Err(super::windows_retry::RenameError {
                error: io::Error::from_raw_os_error(code as i32),
                status,
            })
        },
        || retry_allowed(&destination_path),
        || started.elapsed(),
        std::thread::sleep,
    );
    if log_retries && outcome.attempts > 1 {
        tracing::warn!(
            role = publication_role(destination),
            attempts = outcome.attempts,
            elapsed_ms = outcome.elapsed.as_millis() as u64,
            renamed = outcome.result.is_ok(),
            "Windows atomic publication retried"
        );
    }
    outcome.result?;
    file.sync_all()
}

fn retain_destination(path: &[u16]) -> Option<File> {
    // Open the directory entry itself, including a reparse point, rather than
    // following a link to a different object. No contents or attributes change.
    // SAFETY: the caller supplies a terminated UTF-16 path. A successful handle
    // transfers to File and is closed after the synchronous rename attempt.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        None
    } else {
        // SAFETY: CreateFileW returned a newly owned, valid handle.
        Some(unsafe { File::from_raw_handle(handle) })
    }
}

#[link(name = "ntdll")]
unsafe extern "system" {
    // Win32 also maps other statuses (such as delete-pending) to AccessDenied.
    // Read the thread's native status to exclude them; issue no new NT operation.
    fn RtlGetLastNtStatus() -> i32;
}

fn retry_allowed(destination: &Path) -> bool {
    // A readonly target or directory cannot be replaced by this file rename.
    // Do not treat a failed DELETE-open as proof of ACL denial: pending scanner
    // objects can return the same AccessDenied. A reliable ACL decision also
    // needs the parent's DELETE_CHILD grant, effective token and integrity rules;
    // a path DACL check alone is insufficient. Unknown ACL denials therefore
    // consume at most this retry budget. Our private app-data DACL normally
    // grants the current user full access; real ACL denial is unexpected.
    match std::fs::metadata(destination) {
        Ok(metadata) => !metadata.is_dir() && !metadata.permissions().readonly(),
        Err(_) => true,
    }
}

fn publication_role(destination: &Path) -> &'static str {
    // Only allowlisted role names enter the log, never caller-supplied paths or
    // filenames (crash/report names can contain per-instance identifiers).
    match destination.file_name().and_then(|name| name.to_str()) {
        Some("settings.json") => "settings",
        Some("device.key") => "identity",
        Some("pairing-recovery.cbor") => "pairing_recovery",
        Some("group-state.json") => "group_state",
        Some("addresses.json") => "addresses",
        Some("metadata-cache.cbor") => "facts_cache",
        Some("clock.json") => "clipboard_clock",
        Some("checkpoint.json") => "telemetry_checkpoint",
        Some("ipc.json") => "ipc_endpoint",
        _ => "local_state",
    }
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
    fn readonly_and_directory_targets_are_known_permanent_denials() {
        let directory = tempfile::tempdir().unwrap();
        assert!(!retry_allowed(directory.path()));
        let target = directory.path().join("settings.json");
        super::super::atomic_write(&target, b"before").unwrap();
        let permissions = std::fs::metadata(&target).unwrap().permissions();
        let mut readonly = permissions.clone();
        readonly.set_readonly(true);
        std::fs::set_permissions(&target, readonly).unwrap();
        assert!(!retry_allowed(&target));
        let result = super::super::atomic_write(&target, b"after");
        std::fs::set_permissions(&target, permissions).unwrap();
        assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
        assert_eq!(std::fs::read(&target).unwrap(), b"before");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn original_private_writer_can_publish_without_reopening() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join("settings.tmp");
        let destination = directory.path().join("settings.json");
        let mut writer = create_private_new(&temporary).unwrap();
        writer.write_all(b"private settings").unwrap();
        writer.sync_all().unwrap();
        let published = replace(&writer, &destination, true);
        let observed = std::fs::read(&destination);
        drop(writer);
        assert!(
            published.is_ok(),
            "publication must use the original private handle: {published:?}"
        );
        assert_eq!(observed.unwrap(), b"private settings");
    }

    #[test]
    fn replacement_respects_a_reader_that_denies_delete_sharing() {
        use std::os::windows::fs::OpenOptionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        super::super::atomic_write(&path, b"before").unwrap();
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .unwrap();
        let error = super::super::atomic_write(&path, b"after").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32));
        assert_eq!(std::fs::read(&path).unwrap(), b"before");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        drop(reader);
        super::super::atomic_write(&path, b"after").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
    }

    #[test]
    fn replacements_remain_visible_and_complete_to_concurrent_readers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        let before = [b'a'; 4096];
        let after = [b'b'; 4096];
        super::super::atomic_write(&path, &before).unwrap();
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                start.wait();
                for _ in 0..1024 {
                    let observed = std::fs::read(&path).unwrap();
                    assert!(observed == before || observed == after);
                }
            });
            start.wait();
            for sequence in 0..64 {
                super::super::atomic_write(&path, if sequence % 2 == 0 { &after } else { &before }).unwrap();
            }
        });
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
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
