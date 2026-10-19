use super::*;
use std::{
    ffi::c_void,
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Security::Authorization::*,
    Storage::FileSystem::*,
    System::{Com::CoTaskMemFree, Pipes::*, RemoteDesktop::*},
    UI::Shell::*,
};

pub(super) fn install_dir() -> io::Result<PathBuf> {
    let mut raw = ptr::null_mut();
    // SAFETY: known folder identifier is static; returned allocation belongs to COM allocator.
    unsafe {
        if SHGetKnownFolderPath(&FOLDERID_ProgramFiles, 0, ptr::null_mut(), &mut raw) < 0 {
            return Err(denied());
        }
        let mut n = 0;
        while *raw.add(n) != 0 {
            n += 1;
        }
        let path = PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(raw, n)));
        CoTaskMemFree(raw.cast());
        Ok(path.join("Oink"))
    }
}
pub(super) fn sid_bytes(token: HANDLE) -> io::Result<Vec<u8>> {
    // Use word-aligned storage for TOKEN_USER and its inline SID.
    let mut buf = [0usize; 128];
    let mut size = 0;
    // SAFETY: aligned fixed buffer is larger than TOKEN_USER plus maximum SID.
    unsafe {
        check(GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            size_of_val(&buf) as u32,
            &mut size,
        ))?;
        let sid = (*(buf.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        Ok(std::slice::from_raw_parts(sid.cast::<u8>(), GetLengthSid(sid) as usize).to_vec())
    }
}
fn system_sid(sid: *mut c_void) -> bool {
    // SAFETY: caller supplies a valid SID obtained from a Windows token or ACL.
    unsafe { IsWellKnownSid(sid, WinLocalSystemSid) != 0 }
}
pub(super) fn privileged_sid(sid: *mut c_void) -> bool {
    // SAFETY: SID comes from a validated Windows security descriptor.
    unsafe {
        if system_sid(sid) || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0 {
            return true;
        }
        let mut installer = ptr::null_mut();
        if ConvertStringSidToSidW(
            wide("S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464").as_ptr(),
            &mut installer,
        ) == 0
        {
            return false;
        }
        let equal = EqualSid(sid, installer) != 0;
        LocalFree(installer);
        equal
    }
}
fn acl_safe(path: &Path) -> io::Result<()> {
    let name = wide(path.to_str().ok_or_else(denied)?);
    let (mut owner, mut acl, mut sd) = (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
    // SAFETY: Windows initializes descriptor and interior owner/DACL pointers; descriptor freed below.
    unsafe {
        let rc = GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(context(
                "GetNamedSecurityInfoW",
                io::Error::from_raw_os_error(rc as i32),
            ));
        }
        let result = (|| {
            // Program Files may be owned by TrustedInstaller; require privileged owner on the
            // application directory/files as well as Program Files itself.
            if owner.is_null() || !privileged_sid(owner) || acl.is_null() {
                return Err(denied());
            }
            for i in 0..u32::from((*acl).AceCount) {
                let mut ace = ptr::null_mut();
                check(GetAce(acl, i, &mut ace))?;
                let header = &*ace.cast::<ACE_HEADER>();
                if header.AceFlags & INHERIT_ONLY_ACE as u8 != 0 {
                    continue;
                }
                if header.AceType == 0 {
                    let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                    let sid = ptr::addr_of!(allowed.SidStart).cast_mut().cast();
                    const WRITE: u32 = GENERIC_ALL
                        | GENERIC_WRITE
                        | WRITE_DAC
                        | WRITE_OWNER
                        | DELETE
                        | FILE_WRITE_DATA
                        | FILE_APPEND_DATA
                        | FILE_WRITE_EA
                        | FILE_WRITE_ATTRIBUTES
                        | FILE_DELETE_CHILD;
                    if allowed.Mask & WRITE != 0 && !privileged_sid(sid) {
                        return Err(denied());
                    }
                } else if header.AceType != 1 {
                    return Err(denied());
                }
            }
            Ok(())
        })();
        LocalFree(sd);
        result
    }
}
pub(super) fn protected_file(path: &Path) -> io::Result<()> {
    let dir = install_dir()?;
    if path.parent() != Some(dir.as_path()) {
        return Err(denied());
    }
    let mut prefix = PathBuf::new();
    for c in path.components() {
        prefix.push(c);
        if prefix.as_os_str().len() < 4 {
            continue;
        }
        let name = wide(prefix.to_str().ok_or_else(denied)?);
        // SAFETY: null terminated absolute path, read-only metadata query.
        let attrs = unsafe { GetFileAttributesW(name.as_ptr()) };
        if attrs == INVALID_FILE_ATTRIBUTES || attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(denied());
        }
    }
    acl_safe(dir.parent().ok_or_else(denied)?)?;
    acl_safe(&dir)?;
    acl_safe(path)
}
pub(super) fn image(process: HANDLE) -> io::Result<PathBuf> {
    let mut buf = [0u16; 32768];
    let mut len = buf.len() as u32;
    // SAFETY: caller retains process handle; output buffer sized by len.
    check(unsafe { QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len) })
        .map_err(|e| context("QueryFullProcessImageNameW(peer)", e))?;
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}
fn client_installation_at(path: &Path, dir: &Path, verify: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<bool> {
    // Location selects the installation mode; a damaged protected installation must
    // return an error, never masquerade as the ordinary per-user installation.
    let matches = ["oink.exe", "oink-agent.exe"].iter().any(|name| {
        path.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&dir.join(name).as_os_str().to_string_lossy())
    });
    if !matches {
        return Ok(false);
    }
    verify(path)?;
    Ok(true)
}
pub(super) fn client_installation() -> io::Result<bool> {
    // SAFETY: query this process's kernel image path, not an environment-provided path.
    let path = image(unsafe { GetCurrentProcess() })?;
    client_installation_at(&path, &install_dir()?, protected_file)
}

pub(super) fn verify_self() -> io::Result<()> {
    // SAFETY: pseudo process handle valid for this process lifetime.
    let me = unsafe { GetCurrentProcess() };
    let path = image(me)?;
    if path.file_name().and_then(|n| n.to_str()) != Some("oink-service.exe") {
        return Err(denied());
    }
    protected_file(&path)?;
    let sid = sid_bytes(token(me)?.0)?;
    if !system_sid(sid.as_ptr().cast_mut().cast()) {
        return Err(denied());
    }
    Ok(())
}
pub(super) fn user_sid(id: u32) -> io::Result<Vec<u8>> {
    let mut raw = ptr::null_mut();
    // SAFETY: SYSTEM broker/helper queries only the active session, owning the returned token.
    check(unsafe { WTSQueryUserToken(id, &mut raw) })?;
    sid_bytes(Handle::new(raw)?.0)
}
pub(super) fn pipe_security(id: u32) -> io::Result<Descriptor> {
    let sid = user_sid(id)?;
    let mut string = ptr::null_mut();
    // SAFETY: valid SID and LocalAlloc output string, freed after copying.
    let sid_text = unsafe {
        check(ConvertSidToStringSidW(sid.as_ptr().cast_mut().cast(), &mut string))?;
        let mut n = 0;
        while *string.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(string, n));
        LocalFree(string.cast());
        s
    };
    // Exact FILE_GENERIC_READ + FILE_GENERIC_WRITE minus FILE_CREATE_PIPE_INSTANCE (0x4).
    // Never grant GENERIC_WRITE on a pipe: it includes creating a competing instance.
    let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;0x12019b;;;{sid_text})"));
    let mut sd = ptr::null_mut();
    // SAFETY: parser allocates the descriptor; Descriptor owns its lifetime.
    check(unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut sd, ptr::null_mut()) })?;
    Ok(Descriptor(sd))
}

pub(super) fn service_pipe_security(pipe: HANDLE) -> io::Result<()> {
    let (mut owner, mut acl, mut sd) = (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
    // SAFETY: connected pipe opened with READ_CONTROL; Windows allocates sd and
    // returns interior owner/DACL pointers. Inspect this handle, never a path.
    unsafe {
        let rc = GetSecurityInfo(
            pipe,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(context(
                "GetSecurityInfo(launch pipe)",
                io::Error::from_raw_os_error(rc as i32),
            ));
        }
        let result = service_pipe_acl(owner, acl);
        LocalFree(sd);
        result
    }
}

// SAFETY: owner and ACL are valid interior pointers of a Windows security descriptor.
unsafe fn service_pipe_acl(owner: *mut c_void, acl: *mut ACL) -> io::Result<()> {
    // SYSTEM-created objects may default to Administrators as owner. Neither SID
    // can be assigned as owner by an ordinary caller.
    if owner.is_null() || !privileged_sid(owner) || acl.is_null() {
        return Err(context("launch pipe owner/DACL", denied()));
    }
    // SAFETY: Windows validated this descriptor; GetAce bounds-checks each entry.
    unsafe {
        for index in 0..u32::from((*acl).AceCount) {
            let mut ace = ptr::null_mut();
            check(GetAce(acl, index, &mut ace))?;
            let header = &*ace.cast::<ACE_HEADER>();
            if header.AceFlags & INHERIT_ONLY_ACE as u8 != 0 {
                continue;
            }
            if header.AceType == 0 {
                let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                let sid = ptr::addr_of!(allowed.SidStart).cast_mut().cast();
                // GENERIC_WRITE includes FILE_CREATE_PIPE_INSTANCE, unlike the
                // service's exact client rights. Do not accept competing servers.
                if allowed.Mask & (GENERIC_ALL | GENERIC_WRITE | FILE_CREATE_PIPE_INSTANCE | WRITE_DAC | WRITE_OWNER)
                    != 0
                    && !privileged_sid(sid)
                {
                    return Err(context("launch pipe permits replacement", denied()));
                }
            } else if header.AceType != 1 {
                return Err(denied());
            }
        }
    }
    Ok(())
}
pub(super) struct Descriptor(*mut c_void);
impl Descriptor {
    pub(super) fn raw(&self) -> *mut c_void {
        self.0
    }
    pub(super) fn attrs(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}
impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: LocalAlloc allocation returned by SDDL parser.
        unsafe {
            LocalFree(self.0);
        }
    }
}
pub(super) fn peer(pipe: HANDLE, id: u32, server: bool) -> io::Result<Handle> {
    peer_at(pipe, id, id, server)
}
pub(super) fn peer_at(pipe: HANDLE, process_session: u32, id: u32, server: bool) -> io::Result<Handle> {
    let (mut pid, mut sid) = (0, 0);
    // SAFETY: connected pipe and valid output variables; PID and session come from kernel.
    unsafe {
        if server {
            check(GetNamedPipeServerProcessId(pipe, &mut pid))
                .map_err(|e| context("GetNamedPipeServerProcessId", e))?;
            check(GetNamedPipeServerSessionId(pipe, &mut sid))
                .map_err(|e| context("GetNamedPipeServerSessionId", e))?;
        } else {
            check(GetNamedPipeClientProcessId(pipe, &mut pid))
                .map_err(|e| context("GetNamedPipeClientProcessId", e))?;
            check(GetNamedPipeClientSessionId(pipe, &mut sid))
                .map_err(|e| context("GetNamedPipeClientSessionId", e))?;
        }
    }
    if sid != process_session {
        return Err(denied());
    }
    // SAFETY: query-only handle pins identity across PID reuse.
    let process = Handle::new(unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | if process_session == 0 { 0 } else { SYNCHRONIZE },
            0,
            pid,
        )
    })
    .map_err(|e| context("OpenProcess(peer query/synchronize)", e))?;
    let path = image(process.0)?;
    protected_file(&path).map_err(|e| context("protected peer image/ACL", e))?;
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(denied)?;
    let t = token(process.0)?;
    let actual = sid_bytes(t.0)?;
    if server {
        if name != "oink-service.exe" || !system_sid(actual.as_ptr().cast_mut().cast()) {
            return Err(denied());
        }
    } else {
        if !matches!(name, "oink.exe" | "oink-agent.exe") || actual != user_sid(id)? {
            return Err(denied());
        }
        // Bind the pipe impersonation identity to the process identity too, detecting a
        // token-switching or impersonating caller. Revert before any privileged operation.
        // SAFETY: connected message was read before impersonation; no code runs as client except token query.
        unsafe {
            check(ImpersonateNamedPipeClient(pipe))?;
            let mut raw = ptr::null_mut();
            let opened = check(OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut raw));
            let identity = opened.and_then(|()| sid_bytes(Handle::new(raw)?.0));
            if RevertToSelf() == 0 {
                std::process::abort();
            }
            if identity? != actual {
                return Err(denied());
            }
        }
    }
    Ok(process)
}

pub(super) fn system_security() -> io::Result<Descriptor> {
    descriptor("D:P(A;;GA;;;SY)")
}
fn descriptor(sddl: &str) -> io::Result<Descriptor> {
    let mut sd = ptr::null_mut();
    // SAFETY: parser owns allocation; Descriptor frees it.
    check(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(sddl).as_ptr(), 1, &mut sd, ptr::null_mut())
    })?;
    Ok(Descriptor(sd))
}
pub(super) fn identity_security(id: u32, rights: u32) -> io::Result<Descriptor> {
    let sid = user_sid(id)?;
    let mut raw = ptr::null_mut();
    // SAFETY: SID from token, converted string freed after use.
    let text = unsafe {
        check(ConvertSidToStringSidW(sid.as_ptr().cast_mut().cast(), &mut raw))?;
        let mut n = 0;
        while *raw.add(n) != 0 {
            n += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(raw, n));
        LocalFree(raw.cast());
        text
    };
    descriptor(&format!("D:P(A;;GA;;;SY)(A;;0x{rights:x};;;{text})"))
}

/// Grant the session user read-only identity access to this helper's *actual* process token.
/// CreateProcessAsUser does not preserve the duplicated source token object's DACL.
/// Called as SYSTEM, before creating the public pipe; never grants TOKEN_DUPLICATE or ADJUST.
pub(super) fn grant_own_token_query(id: u32) -> io::Result<()> {
    let sd = identity_security(id, TOKEN_QUERY)?;
    let mut raw = ptr::null_mut();
    // SAFETY: own SYSTEM token only; WRITE_DAC is needed solely to set its object DACL.
    check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | WRITE_DAC, &mut raw) })
        .map_err(|e| context("OpenProcessToken(helper identity ACL)", e))?;
    let own = Handle::new(raw)?;
    // SAFETY: valid SDDL descriptor and owned token handle; only DACL is changed.
    unsafe {
        let mut dacl = ptr::null_mut();
        let (mut present, mut defaulted) = (0, 0);
        check(GetSecurityDescriptorDacl(
            sd.raw(),
            &mut present,
            &mut dacl,
            &mut defaulted,
        ))?;
        if present == 0 || dacl.is_null() {
            return Err(denied());
        }
        let rc = SetSecurityInfo(
            own.0,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null_mut(),
        );
        if rc != 0 {
            return Err(context(
                "SetSecurityInfo(helper token query ACL)",
                io::Error::from_raw_os_error(rc as i32),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod installation_tests {
    use super::*;

    #[test]
    fn launch_pipe_requires_privileged_owner_and_no_unprivileged_server_rights() {
        for (sddl, accepted) in [
            ("O:SYD:P(A;;GA;;;SY)(A;;0x12019b;;;IU)", true),
            ("O:BAD:P(A;;GA;;;SY)(A;;0x12019b;;;IU)", true),
            ("O:IUD:P(A;;GA;;;SY)(A;;0x12019b;;;IU)", false),
            ("O:SYD:NO_ACCESS_CONTROL", false),
            ("O:SYD:P(A;;GA;;;SY)(A;;GW;;;IU)", false),
            ("O:SYD:P(A;;GA;;;SY)(A;;0x4;;;IU)", false),
            ("O:SYD:P(A;;GA;;;SY)(A;;WD;;;IU)", false),
            ("O:SYD:P(A;;GA;;;SY)(A;;WO;;;IU)", false),
        ] {
            let sd = descriptor(sddl).unwrap();
            let (mut owner, mut acl) = (ptr::null_mut(), ptr::null_mut());
            let (mut present, mut defaulted) = (0, 0);
            // SAFETY: parsed descriptor owns all returned interior pointers.
            unsafe {
                check(GetSecurityDescriptorOwner(sd.raw(), &mut owner, &mut defaulted)).unwrap();
                check(GetSecurityDescriptorDacl(
                    sd.raw(),
                    &mut present,
                    &mut acl,
                    &mut defaulted,
                ))
                .unwrap();
                assert_eq!(service_pipe_acl(owner, acl).is_ok(), accepted, "{sddl}");
            }
        }
    }

    #[test]
    fn ordinary_installations_skip_protected_verification() {
        let dir = Path::new(r"C:\Program Files\Oink");
        for path in [r"C:\Users\tester\Oink\oink.exe", r"C:\build\oink-agent.exe"] {
            assert!(
                !client_installation_at(Path::new(path), dir, |_| panic!("ordinary image must stay local")).unwrap()
            );
        }
    }

    #[test]
    fn protected_location_requires_verification_even_with_different_case() {
        let dir = Path::new(r"C:\Program Files\Oink");
        for path in [
            r"C:\Program Files\Oink\oink.exe",
            r"c:\program files\oink\OINK-AGENT.EXE",
        ] {
            assert!(client_installation_at(Path::new(path), dir, |_| Ok(())).unwrap());
            assert_eq!(
                client_installation_at(Path::new(path), dir, |_| Err(denied()))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }
}
