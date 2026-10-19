use super::*;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::{RemoteDesktop::*, Services::*};
pub(super) static STOP: AtomicBool = AtomicBool::new(false);
const NAME: &str = "OinkInput";
// SAFETY: SCM invokes this callback with the documented ABI; no pointers are dereferenced.
unsafe extern "system" fn control(
    code: u32,
    _kind: u32,
    _data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    if code == SERVICE_CONTROL_STOP || code == SERVICE_CONTROL_SHUTDOWN {
        STOP.store(true, Ordering::Release);
    }
    0
}
fn status(handle: SERVICE_STATUS_HANDLE, state: u32, error: u32) {
    let s = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: error,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_STOP_PENDING { 5000 } else { 0 },
    };
    // SAFETY: handle registered with SCM, status structure initialized.
    unsafe {
        SetServiceStatus(handle, &s);
    }
}
// SAFETY: SCM supplies the expected ABI; arguments unused.
unsafe extern "system" fn main(_argc: u32, _argv: *mut *mut u16) {
    // SAFETY: static service name and callback; no context needed.
    let handle = unsafe { RegisterServiceCtrlHandlerExW(wide(NAME).as_ptr(), Some(control), ptr::null_mut()) };
    if handle.is_null() {
        return;
    }
    status(handle, SERVICE_START_PENDING, 0);
    if !matches!(session(), Ok(0)) {
        status(handle, SERVICE_STOPPED, ERROR_ACCESS_DENIED);
        return;
    }
    // Reverse authentication requires SCM's stable RUNNING PID. Publish that state
    // before the launch worker can expose its pipe; clients already wait for the
    // pipe at logon. Otherwise an early connection races SERVICE_START_PENDING,
    // where QueryServiceStatusEx does not guarantee a valid PID.
    status(handle, SERVICE_RUNNING, 0);
    let launcher = thread::spawn(super::backend::serve);
    let updater = thread::spawn(super::update::serve);
    let mut child: Option<Child> = None;
    while !STOP.load(Ordering::Acquire) {
        // SAFETY: querying active physical session, no state mutation.
        let id = unsafe { WTSGetActiveConsoleSessionId() };
        let replace = child
            .as_ref()
            .is_some_and(|c| c.id != id || !c.alive() || security::user_sid(id).ok().as_ref() != Some(&c.user_sid));
        if replace {
            child.take();
        }
        if super::update::input_allowed()
            && child.is_none()
            && id != 0
            && id != u32::MAX
            && security::user_sid(id).is_ok()
        {
            match spawn(id) {
                Ok(c) => child = Some(c),
                Err(e) => eprintln!("Oink input helper start failed: {e}"),
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    status(handle, SERVICE_STOP_PENDING, 0);
    drop(child);
    let _ = launcher.join();
    let _ = updater.join();
    status(handle, SERVICE_STOPPED, 0);
}
pub(super) fn run() -> io::Result<()> {
    let mut name = wide(NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: table and service name live until dispatcher returns; null terminator included.
    check(unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) })
}
struct Child {
    id: u32,
    user_sid: Vec<u8>,
    process: Handle,
    stop: Handle,
}
impl Child {
    fn alive(&self) -> bool {
        // SAFETY: owned process handle, nonblocking query.
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        // SAFETY: owned event/process; give helper time to release keys before final bounded cleanup.
        unsafe {
            SetEvent(self.stop.0);
            if WaitForSingleObject(self.process.0, 2500) == WAIT_TIMEOUT {
                TerminateProcess(self.process.0, 1);
                WaitForSingleObject(self.process.0, 500);
            }
        }
    }
}
pub(super) fn privilege(t: HANDLE, name: &str) -> io::Result<()> {
    let mut luid = LUID::default();
    // SAFETY: local privilege lookup and token adjustment; token opened with ADJUST_PRIVILEGES.
    unsafe {
        check(LookupPrivilegeValueW(ptr::null(), wide(name).as_ptr(), &mut luid))?;
        let p = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        SetLastError(0);
        check(AdjustTokenPrivileges(t, 0, &p, 0, ptr::null_mut(), ptr::null_mut()))?;
        if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
            return Err(denied());
        }
    }
    Ok(())
}
fn spawn(id: u32) -> io::Result<Child> {
    let path = security::install_dir()?.join("oink-service.exe");
    security::protected_file(&path)?;
    let mut raw = ptr::null_mut();
    // SAFETY: broker's own SYSTEM token, requesting rights needed for duplicated session token only.
    check(unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
            &mut raw,
        )
    })?;
    let own = Handle::new(raw)?;
    for name in [
        "SeTcbPrivilege",
        "SeAssignPrimaryTokenPrivilege",
        "SeIncreaseQuotaPrivilege",
    ] {
        privilege(own.0, name)?;
    }
    let mut raw = ptr::null_mut();
    // SAFETY: new SYSTEM primary token; no client-supplied token or executable is used.
    check(unsafe {
        DuplicateTokenEx(
            own.0,
            TOKEN_ALL_ACCESS,
            ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut raw,
        )
    })?;
    let primary = Handle::new(raw)?;
    // SAFETY: writing a u32 session ID into our duplicated token.
    check(unsafe {
        SetTokenInformation(
            primary.0,
            TokenSessionId,
            ptr::addr_of!(id).cast(),
            size_of::<u32>() as u32,
        )
    })?;
    // Client may query the helper token and process for reverse authentication, but cannot
    // duplicate/adjust its token, write its memory, suspend it or create threads in it.
    let process_sd = security::identity_security(id, PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE)?;
    // The helper grants TOKEN_QUERY on its actual process token before exposing the pipe.
    // Windows does not preserve the source primary token object's DACL when assigning
    // the newly-created process token (verified on Windows with a medium-integrity client).
    let event_sd = security::system_security()?;
    let event_sa = event_sd.attrs();
    // SAFETY: named SYSTEM-only manual reset event, never inherit handles.
    let stop =
        Handle::new(unsafe { CreateEventW(&event_sa, 1, 0, wide(&format!(r"Global\Oink.Input.Stop.{id}")).as_ptr()) })?;
    // SAFETY: immediately after CreateEvent; fail closed on pre-existing name rather than trust it.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(denied());
    }
    let app = wide(path.to_str().ok_or_else(denied)?);
    // SAFETY: read-only own process ID.
    let pid = unsafe { GetCurrentProcessId() };
    let mut cmd = wide(&format!("\"{}\" --helper {pid}", path.display()));
    let mut desktop = wide(r"winsta0\default");
    let si = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpDesktop: desktop.as_mut_ptr(),
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    let sa = process_sd.attrs();
    // SAFETY: explicit protected executable path, fixed internal arguments, no inherited handles,
    // no user profile/environment or client-controlled cwd. Uses SYSTEM broker environment.
    check(unsafe {
        CreateProcessAsUserW(
            primary.0,
            app.as_ptr(),
            cmd.as_mut_ptr(),
            &sa,
            ptr::null(),
            0,
            CREATE_NO_WINDOW,
            ptr::null(),
            wide(security::install_dir()?.to_str().ok_or_else(denied)?).as_ptr(),
            &si,
            &mut pi,
        )
    })?;
    let process = Handle::new(pi.hProcess)?;
    drop(Handle::new(pi.hThread)?);
    Ok(Child {
        id,
        process,
        stop,
        user_sid: security::user_sid(id)?,
    })
}
