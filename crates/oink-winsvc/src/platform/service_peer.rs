//! Reverse authentication of the session-0 launch broker without opening its process.
use super::*;
use windows_sys::Win32::System::{Pipes::*, Services::*};

struct Service(SC_HANDLE);
impl Service {
    fn new(raw: SC_HANDLE) -> io::Result<Self> {
        if raw.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(raw))
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        // SAFETY: owned SCM handle, which must not be passed to CloseHandle.
        unsafe { CloseServiceHandle(self.0) };
    }
}

pub(super) fn verify(pipe: HANDLE) -> io::Result<()> {
    // Windows 11 denies medium-IL OpenProcess(QUERY_LIMITED_INFORMATION) on the
    // session-0 SYSTEM service. The helper's explicit process/token ACL grants do
    // not apply to its parent. Query the local SCM instead: its service object
    // grants ordinary callers QUERY_CONFIG/QUERY_STATUS, not process access.
    // Kernel pipe PID + session bind this connection to the running, own-process
    // OinkInput service; SCM supplies its SYSTEM account and protected image.
    // A squatter cannot choose its PID/session or change the SCM registration.
    // Pipe owner/DACL alone identifies a trust boundary, not this service. Adding
    // process/token query ACEs would work but needlessly changes SYSTEM objects.
    // No client data is sent until this check succeeds; no verification fallback.
    // A privileged pipe owner/DACL also prevents a stale SCM PID from authenticating
    // an ordinary squatter after service exit/PID reuse (including session-0 SSH).
    security::service_pipe_security(pipe)?;
    let pid = installed_pid()?;
    verify_pipe(pipe, pid)
}

fn verify_pipe(pipe: HANDLE, service_pid: u32) -> io::Result<()> {
    let (pid, session) = pipe_identity(pipe)?;
    if pid == 0 || pid != service_pid || session != 0 {
        return Err(context("OinkInput pipe/SCM identity mismatch", denied()));
    }
    Ok(())
}

fn pipe_identity(pipe: HANDLE) -> io::Result<(u32, u32)> {
    let (mut pid, mut session) = (0, 0);
    // SAFETY: connected pipe and writable DWORD outputs; no process handle needed.
    unsafe {
        check(GetNamedPipeServerProcessId(pipe, &mut pid))
            .map_err(|e| context("GetNamedPipeServerProcessId(launch)", e))?;
        check(GetNamedPipeServerSessionId(pipe, &mut session))
            .map_err(|e| context("GetNamedPipeServerSessionId(launch)", e))?;
    }
    Ok((pid, session))
}

fn installed_pid_for(service_name: &str, update: bool) -> io::Result<u32> {
    // SAFETY: local SCM only, read-only access, owned returned handles.
    let manager = Service::new(unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) })
        .map_err(|e| context("OpenSCManagerW(launch)", e))?;
    // SAFETY: fixed service name and live SCM handle; no start/stop/change rights.
    let service = Service::new(unsafe {
        OpenServiceW(
            manager.0,
            wide(service_name).as_ptr(),
            SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS,
        )
    })
    .map_err(|e| context("OpenServiceW(OinkInput query)", e))?;
    // QueryServiceConfigW documents an 8 KiB maximum. Word storage aligns the
    // returned structure and all of its embedded string pointers.
    let mut storage = [0usize; 8192 / size_of::<usize>()];
    let mut needed = 0;
    let config = storage.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    // SAFETY: aligned buffer and its true size; strings live until storage drops.
    unsafe {
        check(QueryServiceConfigW(
            service.0,
            config,
            size_of_val(&storage) as u32,
            &mut needed,
        ))
        .map_err(|e| context("QueryServiceConfigW(OinkInput)", e))?;
        let path = if update {
            super::update::protected::root()?.join("oink-service.exe")
        } else {
            security::install_dir()?.join("oink-service.exe")
        };
        if update {
            let _image = super::update::protected::open(&path)?;
        } else {
            security::protected_file(&path)?;
        }
        let binary = wide(&format!("\"{}\"", path.display()));
        if (*config).dwServiceType != SERVICE_WIN32_OWN_PROCESS
            || !equal_string((*config).lpServiceStartName, &wide("LocalSystem"))
            || !equal_string((*config).lpBinaryPathName, &binary)
        {
            return Err(context("OinkInput installed configuration", denied()));
        }
    }
    let mut status = SERVICE_STATUS_PROCESS::default();
    // SAFETY: exact output buffer size and owned query-only service handle.
    check(unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            ptr::addr_of_mut!(status).cast(),
            size_of_val(&status) as u32,
            &mut needed,
        )
    })
    .map_err(|e| context("QueryServiceStatusEx(OinkInput)", e))?;
    // SCM only guarantees a valid PID in stable running/paused states. This
    // broker must be RUNNING; a start/stop race fails closed and is not replayed.
    if status.dwServiceType != SERVICE_WIN32_OWN_PROCESS
        || status.dwCurrentState != SERVICE_RUNNING
        || status.dwProcessId == 0
    {
        return Err(context("OinkInput is not running", denied()));
    }
    Ok(status.dwProcessId)
}

// SAFETY: actual is a terminated string inside a successful SCM query buffer.
unsafe fn equal_string(actual: *const u16, expected: &[u16]) -> bool {
    if actual.is_null() {
        return false;
    }
    for (index, expected) in expected.iter().enumerate() {
        // SAFETY: stop at the first mismatch, including an earlier terminator.
        if unsafe { *actual.add(index) } != *expected {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[path = "service_peer_tests.rs"]
mod tests;

pub(super) fn verify_update(pipe: HANDLE) -> io::Result<()> {
    security::service_pipe_security(pipe)?;
    verify_pipe(pipe, installed_pid_for("OinkInput", true)?)
}

fn installed_pid() -> io::Result<u32> {
    installed_pid_for("OinkInput", false)
}
