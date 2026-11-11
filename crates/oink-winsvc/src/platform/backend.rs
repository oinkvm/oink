//! SYSTEM-only launch broker. The backend remains the interactive user, on Default.
pub use super::backend_stop::{StopWatch, watch as watch_service_stop};
use super::*;
use crate::backend_launch::{BackendLaunch, MAX_FRAME};
use std::{path::Path, sync::atomic::Ordering, time::Duration};
use windows_sys::Win32::{
    Storage::FileSystem::*,
    System::{Pipes::*, RemoteDesktop::*},
};

fn name(id: u32) -> Vec<u16> {
    wide(&format!(r"\\.\pipe\Oink.Backend.v1.{id}"))
}

pub fn uiaccess() -> io::Result<bool> {
    // SAFETY: own process pseudo-handle, query-only token.
    number(token(unsafe { GetCurrentProcess() })?.0, TokenUIAccess).map(|n| n != 0)
}
/// Validate the UI role without rejecting the user's administrator token.
pub fn require_interactive_ui() -> io::Result<()> {
    // SAFETY: query-only current-process pseudo handle.
    let own = token(unsafe { GetCurrentProcess() })?;
    let allowed = crate::token_policy::interactive_ui(
        number(own.0, TokenUIAccess)?,
        number(own.0, TokenSessionId)?,
        crate::token_policy::service_account(&security::sid_bytes(own.0)?),
    );
    if allowed {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "UI requires an interactive user token without UIAccess (not SYSTEM or a service account)",
        ))
    }
}

pub(super) fn token_uiaccess(token: HANDLE) -> io::Result<u32> {
    number(token, TokenUIAccess)
}

fn number(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<u32> {
    let (mut value, mut len) = (0u32, 0);
    // SAFETY: all callers request DWORD token fields into a correctly sized buffer.
    check(unsafe { GetTokenInformation(token, class, ptr::addr_of_mut!(value).cast(), 4, &mut len) })?;
    Ok(value)
}

/// A request starts exactly the installed backend. No direct-process fallback.
pub fn start(data_dir: &Path) -> io::Result<u32> {
    start_tracked(data_dir).map(|process| process.id())
}
/// The broker duplicates the original creation handle into this UI. Opening a
/// PID here would race early exit/PID reuse and UIAccess integrity restrictions.
pub fn start_tracked(data_dir: &Path) -> io::Result<StartedProcess> {
    let exe = std::env::current_exe()?;
    request(data_dir, exe.parent().ok_or_else(denied)?, false)
}
pub fn reopen(data_dir: &Path) -> io::Result<u32> {
    request(data_dir, &std::env::current_dir()?, true).map(|process| process.id())
}
pub fn start_in(data_dir: &Path, directory: &Path) -> io::Result<u32> {
    request(data_dir, directory, false).map(|process| process.id())
}
/// Owns only SYNCHRONIZE and PROCESS_QUERY_LIMITED_INFORMATION, never input,
/// mutation, token or duplication rights to the UIAccess process.
#[derive(Debug)]
pub struct StartedProcess {
    process: Handle,
    pid: u32,
}
impl StartedProcess {
    pub fn id(&self) -> u32 {
        self.pid
    }
    pub fn exit_code(&self) -> io::Result<Option<u32>> {
        // Check the signaled state first: 259 is also a legal process exit code.
        // SAFETY: this object owns the query/synchronize handle until Drop.
        match unsafe { WaitForSingleObject(self.process.0, 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: owned process handle and valid DWORD output.
                check(unsafe { GetExitCodeProcess(self.process.0, &mut code) })?;
                Ok(Some(code))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
}
fn request(data_dir: &Path, directory: &Path, reopen: bool) -> io::Result<StartedProcess> {
    if !security::client_installation()? {
        return Err(denied());
    }
    let mut vars: Vec<_> = std::env::vars_os()
        .filter(|(k, _)| !k.to_string_lossy().eq_ignore_ascii_case("OINK_HOME"))
        .collect();
    vars.push(("OINK_HOME".into(), data_dir.as_os_str().into()));
    vars.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
    use std::os::windows::ffi::OsStrExt;
    let directory: Vec<u16> = directory.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut environment = Vec::new();
    for (key, value) in vars {
        environment.extend(key.encode_wide());
        environment.push(u16::from(b'='));
        environment.extend(value.encode_wide());
        environment.push(0);
    }
    environment.push(0);
    crate::launch_recipe::validate_environment(&environment)?;
    let message = if reopen {
        BackendLaunch::Reopen { environment, directory }
    } else {
        BackendLaunch::Start { environment, directory }
    };
    let reply = crate::launch_recipe::wait_for_prelogin_exit(
        || exchange(message.clone()),
        |process_handle| {
            // SAFETY: only a verified service reply supplies this synchronize-only handle.
            let previous = Handle::new(process_handle as usize as HANDLE)?;
            tracing::info!("pre-login process still running; waiting for confirmed exit");
            // A signaled pinned process authorizes another Start request.
            // SAFETY: owned synchronize-only old-process handle from the broker.
            match unsafe { WaitForSingleObject(previous.0, INFINITE) } {
                WAIT_OBJECT_0 => Ok(()),
                _ => Err(io::Error::last_os_error()),
            }
        },
    )?;
    match reply {
        BackendLaunch::Started { pid, process_handle } => {
            // SAFETY: the verified SYSTEM broker duplicated this owned handle
            // into the requesting process before replying on this exact pipe.
            let process = Handle::new(process_handle as usize as HANDLE)?;
            tracing::info!(pid, reopen, "OinkInput backend entry created");
            Ok(StartedProcess { process, pid })
        }
        BackendLaunch::Failed { error } => Err(io::Error::other(error)),
        _ => Err(denied()),
    }
}
/// One request/reply on this session's launch pipe, after verifying the
/// installed OinkInput service as its server. Never replayed.
pub(super) fn exchange(message: BackendLaunch) -> io::Result<BackendLaunch> {
    if !security::client_installation()? {
        return Err(denied());
    }
    let id = session()?;
    // At logon the interactive token can appear before SCM has exposed the launch
    // pipe. Bound only this startup/pipe-busy wait; never replay a sent launch request.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let pipe = loop {
        // SAFETY: fixed local name; identification only, no inherited handles.
        let result = Handle::new(unsafe {
            CreateFileW(
                name(id).as_ptr(),
                FILE_READ_DATA | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | READ_CONTROL,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        });
        match result {
            Ok(pipe) => break pipe,
            Err(error) if matches!(error.raw_os_error(), Some(2 | 231)) && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(context("OinkInput UIAccess backend launch unavailable", error)),
        }
    };
    let mode = PIPE_READMODE_MESSAGE | PIPE_WAIT;
    // SAFETY: owned connected pipe and valid mode.
    check(unsafe { SetNamedPipeHandleState(pipe.0, &mode, ptr::null(), ptr::null()) })?;
    super::service_peer::verify(pipe.0)?;
    tracing::info!(session = id, "OinkInput launch server verified");
    send(&pipe, message)?;
    receive(&pipe)
}
fn send(pipe: &Handle, message: BackendLaunch) -> io::Result<()> {
    let frame = message.encode()?;
    if frame.len() > MAX_FRAME {
        return Err(io::Error::other("backend launch frame too large"));
    }
    let mut op = pipe_io::Operation::new(pipe)?;
    // SAFETY: frame precedes op and stays live through completion/cancellation.
    unsafe {
        op.write(&frame)?;
    }
    if op.wait(5000)? as usize != frame.len() {
        return Err(io::Error::other("short launch write"));
    }
    Ok(())
}
fn receive(pipe: &Handle) -> io::Result<BackendLaunch> {
    let mut buffer = vec![0u8; MAX_FRAME];
    let mut op = pipe_io::Operation::new(pipe)?;
    // SAFETY: buffer precedes op; cancellation drains before buffer destruction.
    unsafe {
        op.read(&mut buffer)?;
    }
    let count = op.wait(5000)? as usize;
    BackendLaunch::decode(&buffer[..count])
}

pub(super) fn serve() {
    let Some(events) = broker::events() else { return };
    // A persistent failure retries every 250 ms; log each distinct error once.
    let mut reported: Option<String> = None;
    while !broker::STOP.load(Ordering::Acquire) {
        // SAFETY: read-only active console session query.
        let id = unsafe { WTSGetActiveConsoleSessionId() };
        let retry = if id != 0 && id != u32::MAX && security::user_sid(id).is_ok() {
            match serve_session(id) {
                Ok(()) => {
                    reported = None;
                    continue;
                }
                Err(error) => {
                    let text = error.to_string();
                    if reported.as_deref() != Some(text.as_str()) {
                        tracing::warn!(session = id, error = %text, "UIAccess launch broker failed");
                        reported = Some(text);
                    }
                    // Preserve bind-failure recovery, but never wake periodically
                    // while a session simply has no interactive user.
                    Some(std::time::Instant::now() + Duration::from_millis(250))
                }
            }
        } else {
            None
        };
        if super::wake::wait(&[events.stop.raw(), events.launch.raw()], retry).is_err() {
            break;
        }
    }
}
fn serve_session(id: u32) -> io::Result<()> {
    let events = broker::events().ok_or_else(denied)?;
    let user = security::user_sid(id)?;
    let descriptor = security::pipe_security(id)?;
    let attrs = descriptor.attrs();
    // SAFETY: fixed per-session local pipe, exact session-user ACL, owned handle.
    let pipe = Handle::new(unsafe {
        CreateNamedPipeW(
            name(id).as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            MAX_FRAME as u32,
            MAX_FRAME as u32,
            0,
            &attrs,
        )
    })?;
    while !broker::STOP.load(Ordering::Acquire) {
        let mut op = pipe_io::Operation::new(&pipe)?;
        op.connect()?;
        loop {
            // SAFETY: event owned by op, non-mutating session query.
            let ready = super::wake::wait(&[op.event(), events.stop.raw(), events.launch.raw()], None)?;
            // SAFETY: read-only active console session query.
            let active = unsafe { WTSGetActiveConsoleSessionId() };
            if broker::STOP.load(Ordering::Acquire)
                || active != id
                || security::user_sid(id).ok().as_ref() != Some(&user)
            {
                return Ok(());
            }
            if matches!(ready, super::wake::Woke::Signal(0)) {
                break;
            }
        }
        op.cancel();
        drop(op);
        let result = (|| {
            let request = receive(&pipe)?;
            let _client = security::launch_peer(pipe.0, id).map_err(|e| context("authenticate launch client", e))?;
            // A session may have switched while authentication was running.
            // SAFETY: no mutation; launch must still belong to the active interactive user.
            if unsafe { WTSGetActiveConsoleSessionId() } != id {
                return Err(denied());
            }
            if super::update::pending() {
                return Err(denied());
            }
            let client_token = token(_client.0)?;
            if !security::interactive_token(client_token.0)? {
                return Err(denied());
            }
            if matches!(
                request,
                BackendLaunch::PreloginStatus
                    | BackendLaunch::PreloginClaim
                    | BackendLaunch::PreloginRelease
                    | BackendLaunch::PreloginSnapshot { .. }
            ) {
                return super::prelogin::handle(&request, id, &_client);
            }
            let user = security::sid_bytes(client_token.0)?;
            crate::launch_recipe::dispatch(request, |environment, directory, target| {
                let create = || {
                    let process = spawn(id, &user, environment, directory, target)?;
                    let handle = duplicate_for_client(&process, &_client)?;
                    Ok(BackendLaunch::Started {
                        pid: process.pid,
                        process_handle: handle,
                    })
                };
                if target == crate::launch_recipe::Target::Backend {
                    return super::prelogin::handoff(&_client, create);
                }
                create()
            })
        })();
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => {
                tracing::warn!(%error, "UIAccess backend start failed");
                BackendLaunch::Failed {
                    error: format!("OinkInput 无法启动 UIAccess 后台：{error}")
                        .chars()
                        .take(256)
                        .collect(),
                }
            }
        };
        let _ = send(&pipe, reply);
        // Wait for the client to close after consuming the reply. Disconnecting immediately
        // discards unread pipe data on Windows; a bounded read avoids FlushFileBuffers hanging SCM.
        let mut ack = [0u8; 1];
        let mut op = pipe_io::Operation::new(&pipe)?;
        // SAFETY: ack remains live until the pending read is completed/cancelled.
        if unsafe { op.read(&mut ack) }.is_ok() {
            let _ = op.wait(1000);
        }
        drop(op);
        // SAFETY: no operation remains outstanding, next loop reuses the server handle.
        unsafe {
            DisconnectNamedPipe(pipe.0);
        }
    }
    Ok(())
}
fn spawn(
    id: u32,
    user: &[u8],
    environment: &[u16],
    directory: &[u16],
    target: crate::launch_recipe::Target,
) -> io::Result<StartedProcess> {
    spawn_at(id, user, environment, directory, target, false)
}
fn spawn_at(
    id: u32,
    user: &[u8],
    environment: &[u16],
    directory: &[u16],
    target: crate::launch_recipe::Target,
    update: bool,
) -> io::Result<StartedProcess> {
    crate::launch_recipe::validate_environment(environment)?;
    let path = if update {
        super::update::protected::root()?.join("oink.exe")
    } else {
        security::install_dir()?.join("oink.exe")
    };
    if update {
        let _image = super::update::protected::open(&path)?;
    } else {
        security::protected_file(&path).map_err(|e| context("protected backend image", e))?;
    }
    let mut raw = ptr::null_mut();
    // SAFETY: broker's own token, only enable privileges already assigned to SYSTEM.
    check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES, &mut raw) })
        .map_err(|e| context("OpenProcessToken(launch privileges)", e))?;
    let own = Handle::new(raw)?;
    for privilege in [
        "SeTcbPrivilege",
        "SeAssignPrimaryTokenPrivilege",
        "SeIncreaseQuotaPrivilege",
        "SeImpersonatePrivilege",
    ] {
        broker::privilege(own.0, privilege).map_err(|e| context(privilege, e))?;
    }
    // Windows manifest activation requires signing. The existing SYSTEM broker instead sets
    // UIAccess on a duplicate of the interactive user's primary token (unsigned run1 recipe).
    // Always use WTS's logon token, even when the requesting UI is elevated.
    // A split-token administrator therefore keeps a non-elevated UIAccess backend
    // compatible with ordinary applications; with UAC off the sole user token is
    // naturally High. Never borrow SYSTEM or the caller's elevated token.
    // UIPI still prevents elevated windows receiving drag/drop and some messages
    // from non-elevated apps; cross-device drag involving those apps can fail when
    // Oink runs elevated. Elevation improves injection into elevated windows;
    // UIAccess on the normal backend provides that capability without elevation.
    let primary = crate::launch_recipe::uiaccess_token(
        || {
            let mut raw = ptr::null_mut();
            // SAFETY: SYSTEM with SeTcb queries the authenticated active session.
            check(unsafe { WTSQueryUserToken(id, &mut raw) }).map_err(|e| context("WTSQueryUserToken(backend)", e))?;
            Handle::new(raw)
        },
        |source| {
            let mut raw = ptr::null_mut();
            // SAFETY: owned source token; newly owned primary token output.
            check(unsafe {
                DuplicateTokenEx(
                    source.0,
                    TOKEN_ALL_ACCESS,
                    ptr::null(),
                    SecurityImpersonation,
                    TokenPrimary,
                    &mut raw,
                )
            })
            .map_err(|e| context("DuplicateTokenEx(backend)", e))?;
            Handle::new(raw)
        },
        |primary| {
            let enabled = u32::from(target.uiaccess());
            // SAFETY: DWORD TokenUIAccess on our duplicate only; caller has SeTcb.
            check(unsafe { SetTokenInformation(primary.0, TokenUIAccess, ptr::addr_of!(enabled).cast(), 4) })
                .map_err(|e| context("SetTokenInformation(TokenUIAccess)", e))
        },
        |source, primary| {
            if number(primary.0, TokenUIAccess)? != u32::from(target.uiaccess())
                || number(primary.0, TokenSessionId)? != id
                || !security::interactive_token(source.0)?
                || number(primary.0, TokenElevation)? != number(source.0, TokenElevation)?
                || security::sid_bytes(primary.0)? != security::sid_bytes(source.0)?
                || security::sid_bytes(source.0)? != user
                || integrity(primary.0)? != integrity(source.0)?
            {
                return Err(context("UIAccess token identity readback", denied()));
            }
            Ok(())
        },
    )?;
    let app = wide(path.to_str().ok_or_else(denied)?);
    let argument = target.argument();
    let mut command = wide(&format!("\"{}\" {argument}", path.display()));
    let mut desktop = wide(r"winsta0\default");
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpDesktop: desktop.as_mut_ptr(),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    let register_route = !update && target == crate::launch_recipe::Target::Backend;
    // CreateProcessAsUser otherwise opens the cwd/image as the calling SYSTEM
    // service, not hToken's user. CLI/reopen may have a user-only directory or
    // per-logon drive mapping. Use the documented target-user file-access context
    // for this call only; all privileged token construction is already complete.
    // No fallible return between impersonation and RevertToSelf. Capture the create
    // error first, and never send a pipe reply while impersonating the user.
    // Product UIAccess creation still requires post-install validation on Windows.
    // SAFETY: verified, owned user primary token with QUERY/DUPLICATE rights.
    check(unsafe { ImpersonateLoggedOnUser(primary.0) })
        .map_err(|e| context("ImpersonateLoggedOnUser(backend cwd)", e))?;
    // SAFETY: fixed protected image/args, verified user token, caller's UTF-16 environment/cwd,
    // no inherited handles/console. The creator is SCM's service, not the client's
    // terminal/UI job. Do not unconditionally request CREATE_BREAKAWAY_FROM_JOB:
    // Windows denies it in a job without breakaway rights. The backend logs its
    // actual job membership at entry. The service does not supervise the backend.
    let created = check(unsafe {
        CreateProcessAsUserW(
            primary.0,
            app.as_ptr(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_UNICODE_ENVIRONMENT
                | DETACHED_PROCESS
                | CREATE_NEW_PROCESS_GROUP
                | NORMAL_PRIORITY_CLASS
                | if register_route { CREATE_SUSPENDED } else { 0 },
            environment.as_ptr().cast(),
            directory.as_ptr(),
            &startup,
            &mut process,
        )
    });
    // SAFETY: this launch thread owns the impersonation scope. Continuing with
    // the wrong identity would affect the next client; match peer authentication.
    if unsafe { RevertToSelf() } == 0 {
        std::process::abort();
    }
    created.map_err(|e| context("CreateProcessAsUserW(backend entry)", e))?;
    let thread = Handle::new(process.hThread)?;
    let child = Handle::new(process.hProcess)?;
    if register_route {
        // Pin the exact service-created --service process before any user code
        // executes. UIAccess/path/parent scans alone cannot mint a route grant.
        crate::launch_recipe::registered_backend(
            || console::register(&child, id, user),
            || {
                // A console transition during creation invalidates this launch.
                // SAFETY: read-only query and owned suspended primary thread.
                if unsafe { WTSGetActiveConsoleSessionId() } != id {
                    return Err(denied());
                }
                // SAFETY: owned primary thread of our still-suspended child.
                if unsafe { ResumeThread(thread.0) } == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            },
            || {
                // SAFETY: our suspended child has not run; termination failure
                // is returned explicitly rather than claiming cleanup succeeded.
                unsafe {
                    check(TerminateProcess(child.0, 1))?;
                    if WaitForSingleObject(child.0, 2500) != WAIT_OBJECT_0 {
                        return Err(io::Error::other("suspended backend termination unconfirmed"));
                    }
                }
                Ok(())
            },
        )?;
    }
    Ok(StartedProcess {
        process: child,
        pid: process.dwProcessId,
    })
}
fn duplicate_for_client(process: &StartedProcess, client: &Handle) -> io::Result<u64> {
    let mut duplicate = ptr::null_mut();
    // SAFETY: both original handles are owned and pinned. SYSTEM duplicates
    // only wait/query rights into the authenticated pipe client. The UI needs
    // no OpenProcess access to a higher-integrity process and gains no UIAccess authority.
    check(unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            process.process.0,
            client.0,
            &mut duplicate,
            SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            0,
        )
    })
    .map_err(|e| context("duplicate backend wait handle", e))?;
    Ok(duplicate as usize as u64)
}
fn integrity(token: HANDLE) -> io::Result<u32> {
    let mut storage = [0usize; 64];
    let mut len = 0;
    // SAFETY: aligned ample storage holds TOKEN_MANDATORY_LABEL and its SID; query-only.
    unsafe {
        check(GetTokenInformation(
            token,
            TokenIntegrityLevel,
            storage.as_mut_ptr().cast(),
            size_of_val(&storage) as u32,
            &mut len,
        ))?;
        let sid = (*storage.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid;
        Ok(*GetSidSubAuthority(sid, u32::from(*GetSidSubAuthorityCount(sid)) - 1))
    }
}

/// Worker-only recovery launch with the same user SID/session and fixed product entry.
pub(super) fn spawn_update(id: u32, user: &[u8]) -> io::Result<u32> {
    spawn_update_target(id, user, crate::launch_recipe::Target::Backend)
}
pub(super) fn spawn_update_ui(id: u32, user: &[u8]) -> io::Result<u32> {
    spawn_update_target(id, user, crate::launch_recipe::Target::UpdateUi)
}
fn spawn_update_target(id: u32, user: &[u8], target: crate::launch_recipe::Target) -> io::Result<u32> {
    let mut raw = ptr::null_mut();
    // SAFETY: SYSTEM queries the authenticated original interactive session token.
    check(unsafe { WTSQueryUserToken(id, &mut raw) })?;
    let primary = Handle::new(raw)?;
    let mut environment = ptr::null_mut();
    // SAFETY: userenv builds this user's system/profile environment. Caller data
    // is never forwarded to the SYSTEM worker or used to select executable/cwd.
    check(unsafe { windows_sys::Win32::System::Environment::CreateEnvironmentBlock(&mut environment, primary.0, 0) })?;
    let block = unsafe {
        // SAFETY: CreateEnvironmentBlock returns a double-NUL UTF-16 allocation.
        let raw = environment.cast::<u16>();
        let mut n = 0;
        while n < 32767 && !(*raw.add(n) == 0 && *raw.add(n + 1) == 0) {
            n += 1;
        }
        let block = std::slice::from_raw_parts(raw, n + 2).to_vec();
        windows_sys::Win32::System::Environment::DestroyEnvironmentBlock(environment);
        block
    };
    let directory = wide(super::update::protected::root()?.to_str().ok_or_else(denied)?);
    spawn_at(id, user, &block, &directory, target, true).map(|process| process.id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::windows::io::AsRawHandle;

    fn track(child: &std::process::Child) -> StartedProcess {
        // SAFETY: current-process pseudo handle and live owned Child handle.
        let client = Handle::new(unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, GetCurrentProcessId()) }).unwrap();
        let mut raw = ptr::null_mut();
        // SAFETY: retain the original creation handle, including after exit.
        check(unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                child.as_raw_handle(),
                GetCurrentProcess(),
                &mut raw,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        })
        .unwrap();
        let original = StartedProcess {
            process: Handle::new(raw).unwrap(),
            pid: child.id(),
        };
        let duplicate = duplicate_for_client(&original, &client).unwrap();
        StartedProcess {
            process: Handle::new(duplicate as usize as HANDLE).unwrap(),
            pid: child.id(),
        }
    }

    #[test]
    fn original_creation_handle_reports_exit_before_first_poll_including_still_active_code() {
        for code in [1, 259, 3221225477_u32] {
            let mut child = std::process::Command::new("cmd.exe")
                .args(["/c", &format!("exit {code}")])
                .spawn()
                .unwrap();
            child.wait().unwrap();
            let tracked = track(&child);
            drop(child); // No PID lookup and no dependency on the creator staying alive.
            assert_eq!(tracked.exit_code().unwrap(), Some(code));
        }
    }

    #[test]
    #[ignore = "slow: held-process wait-handle fixture"]
    fn held_process_fixture() {
        if std::env::var_os("OINK_HELD_PROCESS_FIXTURE").is_some() {
            let mut input = std::io::stdin().lock();
            let mut release = [0; 1];
            std::io::Read::read_exact(&mut input, &mut release).unwrap();
        }
    }

    #[test]
    fn wait_handle_detects_a_later_exit_without_granting_process_mutation() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "platform::backend::tests::held_process_fixture", "--ignored"])
            .env("OINK_HELD_PROCESS_FIXTURE", "1")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let tracked = track(&child);
        assert_eq!(tracked.exit_code().unwrap(), None);
        // SAFETY: query/wait handle intentionally lacks PROCESS_TERMINATE.
        assert_eq!(unsafe { TerminateProcess(tracked.process.0, 42) }, 0);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(5));
        child.stdin.take().unwrap().write_all(&[1]).unwrap();
        child.wait().unwrap();
        assert!(tracked.exit_code().unwrap().is_some());
    }
}
