#![cfg(debug_assertions)]
//! Native, private-pipe tests: no installed backend, desktop, or input is started.
use super::*;
use std::io::{BufRead, Read, Write};
use std::process::{Command, Stdio};
use windows_sys::Win32::{Security::Authorization::*, Storage::FileSystem::*};

fn test_name(pid: u32) -> Vec<u16> {
    wide(&format!(r"\\.\pipe\Oink.Unit.ServerIdentity.{pid}"))
}

fn open(name: &[u16]) -> io::Result<Handle> {
    // SAFETY: local terminated name, exact client rights, identification only.
    Handle::new(unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_READ_DATA | FILE_WRITE_DATA | READ_CONTROL,
            0,
            ptr::null(),
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            ptr::null_mut(),
        )
    })
}

#[test]
fn unqueryable_server_child() {
    if std::env::var_os("OINK_TEST_UNQUERYABLE_SERVER").is_none() {
        return;
    }
    let mut sd = ptr::null_mut();
    // This disposable child denies QUERY_LIMITED_INFORMATION even to its creator.
    // Its pipe still grants exact client rights. No product object is modified.
    // SAFETY: parsed descriptor lives through both kernel object operations.
    unsafe {
        check(ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide("D:P(D;;0x1000;;;WD)(A;;GA;;;WD)").as_ptr(),
            1,
            &mut sd,
            ptr::null_mut(),
        ))
        .unwrap();
        check(SetKernelObjectSecurity(
            GetCurrentProcess(),
            DACL_SECURITY_INFORMATION,
            sd,
        ))
        .unwrap();
        LocalFree(sd);
        check(ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019b;;;WD)").as_ptr(),
            1,
            &mut sd,
            ptr::null_mut(),
        ))
        .unwrap();
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    // SAFETY: unique private pipe, owned handle, descriptor copied at creation.
    let server = Handle::new(unsafe {
        CreateNamedPipeW(
            test_name(std::process::id()).as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            64,
            64,
            0,
            &sa,
        )
    })
    .unwrap();
    // SAFETY: kernel has copied the descriptor.
    unsafe { LocalFree(sd) };
    println!("IDENTITY_READY");
    std::io::stdout().flush().unwrap();
    let _ = std::io::stdin().read(&mut [0u8; 1]);
    drop(server);
}

#[test]
fn pipe_identity_does_not_require_openprocess() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "platform::service_peer::tests::unqueryable_server_child",
            "--nocapture",
        ])
        .env("OINK_TEST_UNQUERYABLE_SERVER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(output.read_line(&mut line).unwrap(), 0, "child did not create its pipe");
        if line.contains("IDENTITY_READY") {
            break;
        }
    }
    // SSH test runners may have SeDebugPrivilege enabled. Exercise the ordinary
    // effective caller, not an administrator bypass of the deliberately denied ACL.
    let revert = medium_caller().unwrap();
    let pipe = open(&test_name(child.id())).unwrap();
    // SAFETY: read-only process open; the child explicitly denies this right.
    let query = Handle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, child.id()) });
    assert_eq!(query.unwrap_err().raw_os_error(), Some(5));
    assert_eq!(pipe_identity(pipe.0).unwrap(), (child.id(), session().unwrap()));
    assert!(verify_pipe(pipe.0, child.id().wrapping_add(1)).is_err());
    if session().unwrap() == 0 {
        verify_pipe(pipe.0, child.id()).unwrap();
    }
    // Also exercise GetSecurityInfo on a connected client handle. Acceptance of
    // this fixture's owner depends on whether the test runner is elevated.
    let acl = security::service_pipe_security(pipe.0);
    if let Err(error) = acl {
        assert!(error.to_string().contains("launch pipe owner/DACL"), "{error}");
    }
    drop(pipe);
    drop(revert);
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

struct Revert;
impl Drop for Revert {
    fn drop(&mut self) {
        // SAFETY: this test thread owns the impersonation scope.
        if unsafe { RevertToSelf() } == 0 {
            std::process::abort();
        }
    }
}

fn medium_caller() -> io::Result<Revert> {
    caller_at(WinMediumLabelSid)
}

fn caller_at(integrity: WELL_KNOWN_SID_TYPE) -> io::Result<Revert> {
    let mut source = ptr::null_mut();
    let mut restricted = ptr::null_mut();
    let mut admin = [0u32; 17];
    let mut medium = [0u32; 17];
    let mut length = size_of_val(&admin) as u32;
    // SAFETY: private test token only. Strip privileges and make Administrators
    // deny-only, then lower integrity. This changes no user/system settings.
    unsafe {
        check(OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT,
            &mut source,
        ))?;
        let source = Handle::new(source)?;
        check(CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            ptr::null_mut(),
            admin.as_mut_ptr().cast(),
            &mut length,
        ))?;
        let disabled = SID_AND_ATTRIBUTES {
            Sid: admin.as_mut_ptr().cast(),
            Attributes: 0,
        };
        check(CreateRestrictedToken(
            source.0,
            DISABLE_MAX_PRIVILEGE,
            1,
            &disabled,
            0,
            ptr::null(),
            0,
            ptr::null(),
            &mut restricted,
        ))?;
        let restricted = Handle::new(restricted)?;
        length = size_of_val(&medium) as u32;
        check(CreateWellKnownSid(
            integrity,
            ptr::null_mut(),
            medium.as_mut_ptr().cast(),
            &mut length,
        ))?;
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: medium.as_mut_ptr().cast(),
                Attributes: 0x20, // SE_GROUP_INTEGRITY
            },
        };
        check(SetTokenInformation(
            restricted.0,
            TokenIntegrityLevel,
            ptr::addr_of!(label).cast(),
            size_of_val(&label) as u32 + length,
        ))?;
        check(ImpersonateLoggedOnUser(restricted.0))?;
    }
    Ok(Revert)
}

#[test]
fn user_ipc_accepts_medium_and_high_but_denies_low_and_other_users() {
    // The non-desktop SSH runner may be High. Lower only disposable tokens on
    // this test thread; never change UAC, the desktop, installed apps or accounts.
    // SAFETY: query-only current-process pseudo handle.
    let own = token(unsafe { GetCurrentProcess() }).unwrap();
    let sid = security::sid_bytes(own.0).unwrap();
    let (mut elevated, mut kind, mut len) = (0u32, 0u32, 0);
    // SAFETY: query-only token, correctly sized DWORD output fields.
    unsafe {
        check(GetTokenInformation(
            own.0,
            TokenElevation,
            ptr::addr_of_mut!(elevated).cast(),
            4,
            &mut len,
        ))
        .unwrap();
        check(GetTokenInformation(
            own.0,
            TokenElevationType,
            ptr::addr_of_mut!(kind).cast(),
            4,
            &mut len,
        ))
        .unwrap();
    }
    eprintln!(
        "private IPC fixture creator: elevation_type={kind}, elevated={elevated}, session={}",
        session().unwrap()
    );
    let mut text = ptr::null_mut();
    // SAFETY: token supplies a valid SID, converted allocation is freed below.
    let sid_text = unsafe {
        check(ConvertSidToStringSidW(sid.as_ptr().cast_mut().cast(), &mut text)).unwrap();
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let value = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        value
    };
    let directory = std::env::temp_dir().join(format!("oink-integrity-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let endpoint = directory.join("ipc.json");
    let lock = directory.join("ui.lock");
    oink_foundation::storage::atomic_write(&endpoint, b"private endpoint").unwrap();
    drop(oink_foundation::storage::open_private_lock(&lock).unwrap());
    for (index, user, allowed) in [(0, sid_text.as_str(), true), (1, "S-1-5-21-1-2-3-1001", false)] {
        let mut sd = ptr::null_mut();
        let descriptor = wide(&security::session_pipe_sddl(user));
        // SAFETY: private test pipe uses the production descriptor seam.
        unsafe {
            check(ConvertStringSecurityDescriptorToSecurityDescriptorW(
                descriptor.as_ptr(),
                1,
                &mut sd,
                ptr::null_mut(),
            ))
            .unwrap();
        }
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let name = wide(&format!(r"\\.\pipe\Oink.Unit.Integrity.{}.{index}", std::process::id()));
        // SAFETY: unique private local pipe; no installed service is contacted.
        let server = Handle::new(unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64,
                64,
                0,
                &attrs,
            )
        })
        .unwrap();
        // SAFETY: CreateNamedPipe has copied this LocalAlloc descriptor.
        unsafe {
            LocalFree(sd);
        }
        if allowed {
            // The creator's own token (High on the SSH validation host) connects.
            let mut listening = super::super::pipe_io::Operation::new(&server).unwrap();
            listening.connect().unwrap();
            drop(open(&name).unwrap());
            listening.cancel();
            drop(listening);
            // SAFETY: private pipe with no pending I/O, client already closed.
            unsafe {
                check(DisconnectNamedPipe(server.0)).unwrap();
            }
        }
        let mut listening = super::super::pipe_io::Operation::new(&server).unwrap();
        listening.connect().unwrap();
        let medium = medium_caller().unwrap();
        let connected = open(&name);
        assert_eq!(
            connected.is_ok(),
            allowed,
            "same user must be required at Medium: {connected:?}"
        );
        listening.cancel();
        drop(listening);
        if allowed {
            drop(connected);
            // SAFETY: client closed and the connect operation has drained.
            unsafe {
                check(DisconnectNamedPipe(server.0)).unwrap();
            }
            let file = oink_foundation::storage::open_private_lock(&lock).unwrap();
            fs2_lock(&file);
            assert_eq!(std::fs::read(&endpoint).unwrap(), b"private endpoint");
            oink_foundation::storage::atomic_write(&endpoint, b"medium endpoint").unwrap();
        }
        drop(medium);
        if allowed {
            assert_eq!(std::fs::read(&endpoint).unwrap(), b"medium endpoint");
            fs2_lock(&oink_foundation::storage::open_private_lock(&lock).unwrap());
        }
        let mut listening = super::super::pipe_io::Operation::new(&server).unwrap();
        listening.connect().unwrap();
        let low = caller_at(WinLowLabelSid).unwrap();
        assert_eq!(open(&name).unwrap_err().raw_os_error(), Some(5));
        // SAFETY: local private pipe, read-only request must also exclude Low.
        let read_only = Handle::new(unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_READ_DATA,
                0,
                ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        });
        assert_eq!(read_only.unwrap_err().raw_os_error(), Some(5));
        assert!(std::fs::read(&endpoint).is_err(), "Low must not read the IPC secret");
        assert!(oink_foundation::storage::open_private_lock(&lock).is_err());
        drop(low);
        listening.cancel();
        drop(listening);
        drop(server);
    }
    // Test files only; no product data or installed objects are touched.
    std::fs::remove_dir_all(directory).unwrap();
}

fn fs2_lock(file: &std::fs::File) {
    // LockFileEx requires read access only. No dependency on fs2 in this crate.
    use std::os::windows::io::AsRawHandle;
    let mut operation = windows_sys::Win32::System::IO::OVERLAPPED::default();
    // SAFETY: private owned file and valid OVERLAPPED output, no async I/O.
    unsafe {
        check(LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut operation,
        ))
        .unwrap();
        check(UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut operation)).unwrap();
    }
}
