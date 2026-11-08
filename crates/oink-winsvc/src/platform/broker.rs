use super::*;
use crate::supervision::{Schedule, SessionEvent, SessionState};
use std::sync::OnceLock;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread,
};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::{RemoteDesktop::*, Services::*};
use windows_sys::Win32::{
    System::Registry::*,
    UI::WindowsAndMessaging::{WTS_CONSOLE_CONNECT, WTS_CONSOLE_DISCONNECT, WTS_SESSION_LOGOFF, WTS_SESSION_LOGON},
};
pub(super) static STOP: AtomicBool = AtomicBool::new(false);
static EVENTS: OnceLock<super::wake::Hub> = OnceLock::new();
pub(super) fn events() -> Option<&'static super::wake::Hub> {
    EVENTS.get()
}
pub(super) fn changed() {
    if let Some(events) = events() {
        events.broker.signal();
    }
}

#[cfg(test)]
mod event_tests {
    use super::*;
    #[test]
    fn session_callback_records_the_transition_and_wakes_the_waiter() {
        let events = EVENTS.get_or_init(|| super::super::wake::Hub::new().unwrap());
        let mut notification = WTSSESSION_NOTIFICATION {
            cbSize: std::mem::size_of::<WTSSESSION_NOTIFICATION>() as u32,
            dwSessionId: 0xf000_0001,
        };
        // SAFETY: private synthetic callback data, no SCM registration, no
        // installed service or desktop operation. Handler only queues an event.
        unsafe {
            control(
                SERVICE_CONTROL_SESSIONCHANGE,
                1,
                ptr::from_mut(&mut notification).cast(),
                ptr::null_mut(),
            );
        }
        assert!(matches!(
            events.sessions.pop(),
            Some(crate::supervision::SessionEvent::ConsoleConnect(0xf000_0001))
        ));
    }
}
const NAME: &str = "OinkInput";
// SAFETY: SCM invokes this callback with the documented ABI; no pointers are dereferenced.
unsafe extern "system" fn control(
    code: u32,
    kind: u32,
    data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    if code == SERVICE_CONTROL_STOP || code == SERVICE_CONTROL_SHUTDOWN {
        STOP.store(true, Ordering::Release);
        if let Some(events) = events() {
            events.stop();
        }
    } else if code == SERVICE_CONTROL_SESSIONCHANGE && !data.is_null() {
        // SAFETY: SCM supplies WTSSESSION_NOTIFICATION for this exact control.
        // Copy it before returning; the callback only queues and signals, never
        // queries WTS, takes a policy lock, or waits for another thread.
        let notification = unsafe { &*data.cast::<WTSSESSION_NOTIFICATION>() };
        if notification.cbSize >= std::mem::size_of::<WTSSESSION_NOTIFICATION>() as u32 {
            let id = notification.dwSessionId;
            super::auto_unlock::queue_event(kind, id);
            let event = match kind {
                WTS_CONSOLE_CONNECT => SessionEvent::ConsoleConnect(id),
                WTS_CONSOLE_DISCONNECT => SessionEvent::ConsoleDisconnect(id),
                WTS_SESSION_LOGON => SessionEvent::Logon(id),
                WTS_SESSION_LOGOFF => SessionEvent::Logoff(id),
                _ => SessionEvent::Changed,
            };
            if let Some(events) = events() {
                events.session(event);
            }
        }
    }
    0
}
fn status(handle: SERVICE_STATUS_HANDLE, state: u32, error: u32) {
    let s = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE
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
    let initialized = super::wake::Hub::new().and_then(|hub| {
        EVENTS
            .set(hub)
            .map_err(|_| io::Error::other("service wake sources already initialized"))
    });
    if let Err(error) = initialized {
        eprintln!("Oink event sources unavailable: {error}");
        status(handle, SERVICE_STOPPED, ERROR_GEN_FAILURE);
        return;
    }
    // Reverse authentication requires SCM's stable RUNNING PID. Publish that state
    // before the launch worker can expose its pipe; clients already wait for the
    // pipe at logon. Otherwise an early connection races SERVICE_START_PENDING,
    // where QueryServiceStatusEx does not guarantee a valid PID.
    status(handle, SERVICE_RUNNING, 0);
    if let Err(error) = super::auto_unlock::start() {
        eprintln!("Oink auto unlock unavailable: {error}");
    }
    let launcher = thread::spawn(super::backend::serve);
    let updater = thread::spawn(super::update::serve);
    let result = supervise(events().unwrap());
    if let Err(error) = &result {
        eprintln!("Oink supervision event source failed: {error}");
    }
    STOP.store(true, Ordering::Release);
    events().unwrap().stop();
    status(handle, SERVICE_STOP_PENDING, 0);
    if let Err(error) = super::prelogin::stop() {
        eprintln!("Oink pre-login stop failed: {error}");
    }
    super::auto_unlock::stop();
    let _ = launcher.join();
    let _ = updater.join();
    status(
        handle,
        SERVICE_STOPPED,
        if result.is_ok() { 0 } else { ERROR_GEN_FAILURE },
    );
}

fn supervise(events: &super::wake::Hub) -> io::Result<()> {
    let mut helpers = crate::prelogin_route::Route::<Child>::new();
    let mut schedule = Schedule::default();
    let mut processes = Vec::new();
    let mut registry = Vec::new();
    let mut configuration = None;
    let mut maintenance = super::update::maintenance_notifications()?;
    // Register before the first observation so an installer change cannot land
    // between the initial read and the first subscription.
    registry.push(super::wake::Registry::new(
        HKEY_LOCAL_MACHINE,
        r"SOFTWARE\Oink\Prelogin",
    )?);
    // SAFETY: read-only physical console query, used only as a startup hint.
    if unsafe { WTSGetActiveConsoleSessionId() } == u32::MAX
        && let Some(candidate) = initial_console_candidate()?
    {
        schedule.bootstrap(candidate, std::time::Instant::now());
    }
    while !STOP.load(Ordering::Acquire) {
        let now = std::time::Instant::now();
        super::auto_unlock::drain_events();
        while let Some(event) = events.sessions.pop() {
            schedule.session(event, now);
        }
        if schedule.bootstrapping() {
            schedule.bootstrap_candidate(initial_console_candidate()?);
        }
        watch_processes(&helpers, &mut processes, events)?;
        // SAFETY: querying active physical session, no state mutation.
        let id = unsafe { WTSGetActiveConsoleSessionId() };
        // Network ownership and input-target ownership are independent. The
        // original pre-login process survives console changes; only this helper
        // moves. Holding the old helper until its stop/cleanup result prevents
        // two sessions accepting input concurrently.
        let watch = super::prelogin::tick();
        let result = (|| {
            let route = super::prelogin::route_identity()?;
            let account = console::account(id);
            let route = console::select(id, &account, route);
            // User sources keep their session-bound pipe on their own console;
            // only the temporary-session exception uses the routed endpoint.
            let route = route.filter(|s| s.user_sid.is_none() || s.identity.session != id);
            let desired = super::update::input_allowed().then_some(crate::prelogin_route::Target {
                session: id,
                source: route,
                user_sid: match account {
                    crate::console_policy::Account::User(sid) => Some(sid),
                    _ => None,
                },
            });
            helpers.update(desired, spawn)
        })();
        if let Err(error) = &result {
            eprintln!("Oink input helper transition failed: {error}");
        }
        let complete = std::time::Instant::now();
        schedule.helper_result(complete, result.is_err());
        let console = (id != 0 && id != u32::MAX).then_some(id);
        if schedule.reconcile(complete, console, session_state) {
            events.session_ready();
            // Publication may have completed after this iteration's snapshot.
            // Re-observe once before sleeping, rather than losing that edge.
            events.broker.signal();
        }
        let next = (watch.running, watch.owner);
        if configuration.as_ref() != Some(&next) {
            let mut replacement = Vec::new();
            if !next.0 {
                replacement.push(super::wake::Registry::new(
                    HKEY_LOCAL_MACHINE,
                    r"SOFTWARE\Oink\Prelogin",
                )?);
            }
            if let Some(owner) = &next.1 {
                for key in [
                    r"Software\Microsoft\Windows\CurrentVersion\Run",
                    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run",
                ] {
                    replacement.push(super::wake::Registry::new(HKEY_USERS, &format!(r"{owner}\{key}"))?);
                }
            }
            registry = replacement;
            configuration = Some(next);
            // Read once after arming a newly relevant hive/key. Events arriving
            // during setup remain signaled, and no periodic timer is required.
            events.broker.signal();
        }
        watch_processes(&helpers, &mut processes, events)?;
        let mut handles = vec![events.stop.raw(), events.broker.raw(), maintenance.raw()];
        handles.extend(registry.iter().map(super::wake::Registry::raw));
        match super::wake::wait(&handles, schedule.deadline(complete, watch.deadline))? {
            super::wake::Woke::Signal(0) => break,
            super::wake::Woke::Signal(2) => maintenance.rearm()?,
            super::wake::Woke::Signal(index) if index >= 3 => registry[index - 3].rearm()?,
            _ => {}
        }
    }
    Ok(())
}

fn watch_processes(
    helpers: &crate::prelogin_route::Route<Child>,
    watched: &mut Vec<super::wake::Process>,
    events: &super::wake::Hub,
) -> io::Result<()> {
    let mut handles = super::prelogin::processes()?;
    handles.extend(super::console::processes()?);
    handles.extend(super::prelogin::backend_processes(None, true)?);
    if let Some(child) = helpers.child() {
        handles.push(child.process.duplicate()?);
    }
    let mut next = Vec::new();
    for process in handles {
        let identity = super::wake::Process::identity(&process)?;
        if next.iter().any(|old: &super::wake::Process| old.matches(identity)) {
            continue;
        }
        if let Some(index) = watched.iter().position(|old| old.matches(identity)) {
            next.push(watched.swap_remove(index));
        } else {
            next.push(super::wake::Process::new(process, events.broker.clone())?);
        }
    }
    if !watched.is_empty() {
        // Cancellation must not swallow an exit that happened between the
        // observation and removal of a now-unreferenced creation handle.
        events.broker.signal();
    }
    *watched = next;
    Ok(())
}

fn session_state(id: u32) -> SessionState {
    let mut raw = ptr::null_mut();
    let mut size = 0;
    // SAFETY: WTS owns the bounded protocol field; copy then release it. This
    // is only a transition-completion hint, never an authorized console target.
    let read = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            id,
            WTSClientProtocolType,
            &mut raw,
            &mut size,
        )
    };
    let local = if read != 0 && !raw.is_null() && size >= 2 {
        // SAFETY: WTS succeeded and supplied at least one protocol u16.
        Some(unsafe { *raw } == 0)
    } else {
        None
    };
    // SAFETY: read the immediately preceding WTS failure.
    let gone = read == 0 && unsafe { GetLastError() } == ERROR_CTX_WINSTATION_NOT_FOUND;
    if !raw.is_null() {
        // SAFETY: allocation returned by WTS, including unsuccessful queries.
        unsafe {
            WTSFreeMemory(raw.cast());
        }
    }
    SessionState {
        local,
        user: super::security::user_sid(id).is_ok(),
        gone,
    }
}

fn initial_console_candidate() -> io::Result<Option<u32>> {
    let (mut raw, mut count) = (ptr::null_mut(), 0);
    // SAFETY: one startup-only snapshot supplies a transition hint if the
    // service started after the connect notification but before publication.
    check(unsafe { WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut raw, &mut count) })?;
    if raw.is_null() {
        return if count == 0 {
            Ok(None)
        } else {
            Err(io::Error::other("missing WTS session array"))
        };
    }
    // SAFETY: successful WTS enumeration, nonnull array with count entries.
    let candidate = unsafe { std::slice::from_raw_parts(raw, count as usize) }
        .iter()
        .filter(|session| session.SessionId != 0 && (session.State == WTSActive || session.State == WTSConnected))
        .find(|session| session_state(session.SessionId).local == Some(true))
        .map(|session| session.SessionId);
    // SAFETY: allocation returned by WTS above, no borrowed string escapes.
    unsafe {
        WTSFreeMemory(raw.cast());
    }
    Ok(candidate)
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
    target: crate::prelogin_route::Target,
    stopped: bool,
    process: Handle,
    stop: Handle,
}
impl crate::prelogin_route::Helper for Child {
    fn target(&self) -> &crate::prelogin_route::Target {
        &self.target
    }
    fn alive(&self) -> bool {
        // SAFETY: owned process handle, nonblocking query.
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT }
    }
    fn stop(&mut self) -> io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        // The old process must exit before replacement. Release remains best
        // effort; a nonzero exit or a surviving disconnected session is not a
        // barrier (the same handoff rule as the network backend).
        // SAFETY: owned event/process; waits and forced termination are bounded.
        unsafe {
            if WaitForSingleObject(self.process.0, 0) != WAIT_OBJECT_0 {
                check(SetEvent(self.stop.0))?;
                match WaitForSingleObject(self.process.0, 2500) {
                    WAIT_OBJECT_0 => {}
                    WAIT_TIMEOUT => {
                        check(TerminateProcess(self.process.0, 1))?;
                        let exited = WaitForSingleObject(self.process.0, 500) == WAIT_OBJECT_0;
                        eprintln!(
                            "Oink helper stop timed out: session={} process_exit_confirmed={exited}",
                            self.target.session
                        );
                        if !exited {
                            return Err(io::Error::new(
                                io::ErrorKind::WouldBlock,
                                "helper_process_still_running",
                            ));
                        }
                    }
                    _ => return Err(io::Error::last_os_error()),
                }
            }
            let mut exit = 1;
            if GetExitCodeProcess(self.process.0, &mut exit) == 0 {
                eprintln!("Oink helper exit code unavailable: {}", io::Error::last_os_error());
            } else if exit != 0 {
                eprintln!(
                    "Oink helper exited during handoff: session={} code={exit:#010x}",
                    self.target.session
                );
            }
        }
        self.stopped = true;
        Ok(())
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        let _ = crate::prelogin_route::Helper::stop(self);
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
/// A primary SYSTEM token assigned to session `id`: the input helper and the
/// pre-login backend run as SYSTEM inside the console session.
pub(super) fn session_system_token(id: u32) -> io::Result<Handle> {
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
    Ok(primary)
}
fn spawn(target: crate::prelogin_route::Target) -> io::Result<Child> {
    let id = target.session;
    let route = target.source.as_ref();
    let path = security::install_dir()?.join("oink-service.exe");
    security::protected_file(&path)?;
    let primary = session_system_token(id)?;
    // Client may query the helper token and process for reverse authentication, but cannot
    // duplicate/adjust its token, write its memory, suspend it or create threads in it.
    let process_sd = if let Some(source) = route {
        match &source.user_sid {
            Some(sid) => security::sid_security(sid, PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE)?,
            None => security::system_security()?,
        }
    } else {
        security::identity_security(id, PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE)?
    };
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
    let mut cmd = wide(&match route {
        Some(source) => {
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(source)?);
            format!("\"{}\" --routed-helper {encoded}", path.display())
        }
        None => format!("\"{}\" --helper {pid}", path.display()),
    });
    // Before sign-in, Winlogon is the established desktop (as for the source
    // process). This helper creates no windows/hooks; its native owner then
    // follows OpenInputDesktop. The user helper keeps its existing Default entry.
    let mut desktop = wide(if route.is_some() {
        r"winsta0\Winlogon"
    } else {
        r"winsta0\default"
    });
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
        target,
        process,
        stop,
        stopped: false,
    })
}

#[cfg(test)]
mod exit_tests {
    use super::*;
    use crate::prelogin_route::{Helper as _, Target};
    use std::os::windows::io::IntoRawHandle;

    #[test]
    fn helper_exit_allows_replacement_even_when_release_failed_and_session_still_exists() {
        for exit in [0, 1, 7] {
            let process = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", &format!("exit {exit}")])
                .spawn()
                .unwrap();
            let process = Handle::new(process.into_raw_handle()).unwrap();
            // SAFETY: private child and unnamed event; no installed service or desktop action.
            let stop = Handle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) }).unwrap();
            // SAFETY: owned private process, bounded wait for actual exit.
            assert_eq!(unsafe { WaitForSingleObject(process.0, 5000) }, WAIT_OBJECT_0);
            let mut child = Child {
                target: Target {
                    session: session().unwrap(),
                    source: None,
                    user_sid: None,
                },
                stopped: false,
                process,
                stop,
            };
            child.stop().unwrap();
            assert!(!child.alive());
            child.stop().unwrap();
        }
    }
}
