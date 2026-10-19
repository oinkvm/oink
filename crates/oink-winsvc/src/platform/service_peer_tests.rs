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
            WinMediumLabelSid,
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
