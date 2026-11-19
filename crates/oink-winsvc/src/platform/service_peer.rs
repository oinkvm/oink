//! Reverse authentication of the session-0 launch broker without opening its process.
use super::*;
use windows_sys::Win32::System::{Pipes::*, Services::*};

#[derive(Debug)]
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

fn open_service(service_name: &str) -> io::Result<Service> {
    // SAFETY: local SCM only, read-only access, owned returned handles.
    let manager = Service::new(unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) })
        .map_err(|e| context("OpenSCManagerW(launch)", e))?;
    // SAFETY: fixed service name and live SCM handle; no start/stop/change rights.
    Service::new(unsafe {
        OpenServiceW(
            manager.0,
            wide(service_name).as_ptr(),
            SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS,
        )
    })
}

fn installed_pid_for(service_name: &str, update: bool) -> io::Result<u32> {
    let service = open_service(service_name).map_err(|e| context("OpenServiceW(OinkInput query)", e))?;
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

pub(super) fn installed_pid() -> io::Result<u32> {
    installed_pid_for("OinkInput", false)
}

/// Read-only SCM observation, independent of launch-server authentication.
pub(super) fn startup_status() -> io::Result<(u32, u32, u32, bool)> {
    let service = open_service("OinkInput")?;
    let mut status = SERVICE_STATUS::default();
    let mut storage = [0usize; 8192 / size_of::<usize>()];
    let config = storage.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    let mut needed = 0;
    // SAFETY: query-only handle and aligned buffers owned through both calls.
    unsafe {
        check(QueryServiceStatus(service.0, &mut status))?;
        check(QueryServiceConfigW(
            service.0,
            config,
            size_of_val(&storage) as u32,
            &mut needed,
        ))?;
        let start = (*config).dwStartType;
        // Delayed automatic services also report SERVICE_AUTO_START. On Win10
        // 22H2, SCM kept the dependent STOPPED/1077 while its dependency was
        // START_PENDING. The boot-wide signal alone did not identify that wait.
        let pending = status.dwCurrentState == SERVICE_STOPPED
            && start == SERVICE_AUTO_START
            && status.dwWin32ExitCode == ERROR_SERVICE_NEVER_STARTED
            && dependency_starting((*config).lpDependencies)
                .map_err(|error| context("query OinkInput startup dependencies", error))?;
        Ok((status.dwCurrentState, start, status.dwWin32ExitCode, pending))
    }
}

/// Caller owns the successful QueryServiceConfigW buffer containing this MULTI_SZ.
unsafe fn dependency_starting(mut name: *const u16) -> io::Result<bool> {
    // SAFETY: non-null SCM buffers contain a double-NUL-terminated dependency list.
    if name.is_null() || unsafe { *name } == 0 {
        return Ok(false);
    }
    // SAFETY: local SCM, read-only database access.
    let manager = Service::new(unsafe {
        OpenSCManagerW(
            ptr::null(),
            ptr::null(),
            SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE,
        )
    })?;
    // SAFETY: name advances only to the next entry in the SCM MULTI_SZ.
    while unsafe { *name } != 0 {
        // SAFETY: the loop checked this nonempty entry. SCM prefixes group names with '+'.
        if unsafe { *name } == u16::from(b'+') {
            // SAFETY: dropping the prefix retains the group name and its terminator.
            if group_starting(&manager, unsafe { name.add(1) })? {
                return Ok(true);
            }
        } else {
            // SAFETY: NUL-terminated dependency name inside the live SCM buffer.
            match Service::new(unsafe { OpenServiceW(manager.0, name, SERVICE_QUERY_STATUS) }) {
                Ok(service) => {
                    let mut status = SERVICE_STATUS::default();
                    // SAFETY: owned query-only handle and writable output.
                    check(unsafe { QueryServiceStatus(service.0, &mut status) })?;
                    if status.dwCurrentState == SERVICE_START_PENDING {
                        return Ok(true);
                    }
                }
                Err(e) if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) => {}
                Err(e) => return Err(e),
            }
        }
        // SAFETY: each name and the list have separate NUL terminators.
        unsafe {
            while *name != 0 {
                name = name.add(1);
            }
            name = name.add(1);
        }
    }
    Ok(false)
}

fn group_starting(manager: &Service, group: *const u16) -> io::Result<bool> {
    // EnumServicesStatusEx documents a maximum page size of 256 KiB.
    let mut page = vec![0usize; 256 * 1024 / size_of::<usize>()];
    let mut resume = 0;
    loop {
        let (mut bytes, mut count) = (0, 0);
        // SAFETY: aligned output buffer, terminated group name, documented resume cursor.
        let more = match check(unsafe {
            EnumServicesStatusExW(
                manager.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32 | SERVICE_DRIVER,
                SERVICE_ACTIVE,
                page.as_mut_ptr().cast(),
                size_of_val(page.as_slice()) as u32,
                &mut bytes,
                &mut count,
                &mut resume,
                group,
            )
        }) {
            Ok(()) => false,
            Err(e) if e.raw_os_error() == Some(ERROR_MORE_DATA as i32) => true,
            Err(e) => return Err(e),
        };
        // SAFETY: SCM supplied count records within the aligned page.
        let entries =
            unsafe { std::slice::from_raw_parts(page.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(), count as usize) };
        if entries
            .iter()
            .any(|entry| entry.ServiceStatusProcess.dwCurrentState == SERVICE_START_PENDING)
        {
            return Ok(true);
        }
        if !more {
            return Ok(false);
        }
    }
}

/// Dropping registrations drains callbacks before releasing their context and SCM handle.
#[derive(Debug)]
pub struct StartupWatch<F> {
    registrations: Vec<isize>,
    service: Service,
    wake: Box<F>,
}
impl<F> Drop for StartupWatch<F> {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            // SAFETY: each successful subscription is drained exactly once.
            unsafe {
                UnsubscribeServiceChangeNotifications(registration);
            }
        }
    }
}
unsafe extern "system" fn changed<F: Fn()>(_: u32, context: *const std::ffi::c_void) {
    // SAFETY: stable boxed callback retained until unsubscription has drained.
    unsafe {
        (&*context.cast::<F>())();
    }
}

pub fn watch_startup<F: Fn() + Send + Sync + 'static>(missing: bool, wake: F) -> io::Result<StartupWatch<F>> {
    // SAFETY: local SCM, read-only database access.
    let manager = Service::new(unsafe {
        OpenSCManagerW(
            ptr::null(),
            ptr::null(),
            SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE,
        )
    })?;
    let (handle, events): (_, &[_]) = if missing {
        (manager, &[SC_EVENT_DATABASE_CHANGE])
    } else {
        // Win10 accepts QUERY_STATUS alone for status events but rejects the
        // property subscription with ERROR_ACCESS_DENIED without QUERY_CONFIG.
        // SAFETY: live SCM handle, fixed service name and read-only query rights.
        let service = Service::new(unsafe {
            OpenServiceW(
                manager.0,
                wide("OinkInput").as_ptr(),
                SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG,
            )
        })?;
        (service, &[SC_EVENT_STATUS_CHANGE, SC_EVENT_PROPERTY_CHANGE])
    };
    let mut watch = StartupWatch {
        registrations: Vec::new(),
        service: handle,
        wake: Box::new(wake),
    };
    for &event in events {
        let mut registration = 0;
        // SAFETY: handle and boxed callback remain live until the registration is drained.
        let rc = unsafe {
            SubscribeServiceChangeNotifications(
                watch.service.0,
                event,
                Some(changed::<F>),
                ptr::from_ref(watch.wake.as_ref()).cast(),
                &mut registration,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        watch.registrations.push(registration);
    }
    Ok(watch)
}
