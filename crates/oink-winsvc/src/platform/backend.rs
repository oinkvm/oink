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
fn number(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<u32> {
    let (mut value, mut len) = (0u32, 0);
    // SAFETY: all callers request DWORD token fields into a correctly sized buffer.
    check(unsafe { GetTokenInformation(token, class, ptr::addr_of_mut!(value).cast(), 4, &mut len) })?;
    Ok(value)
}

/// A request starts exactly the installed backend. No direct-process fallback.
pub fn start(data_dir: &Path) -> io::Result<u32> {
    let exe = std::env::current_exe()?;
    start_in(data_dir, exe.parent().ok_or_else(denied)?)
}
pub fn reopen(data_dir: &Path) -> io::Result<u32> {
    request(data_dir, &std::env::current_dir()?, true)
}
pub fn start_in(data_dir: &Path, directory: &Path) -> io::Result<u32> {
    request(data_dir, directory, false)
}
fn request(data_dir: &Path, directory: &Path, reopen: bool) -> io::Result<u32> {
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
    tracing::info!(session = id, reopen, "OinkInput launch server verified");
    send(
        &pipe,
        if reopen {
            BackendLaunch::Reopen { environment, directory }
        } else {
            BackendLaunch::Start { environment, directory }
        },
    )?;
    match receive(&pipe)? {
        BackendLaunch::Started { pid } => {
            tracing::info!(pid, reopen, "OinkInput backend entry created");
            Ok(pid)
        }
        BackendLaunch::Failed { error } => Err(io::Error::other(error)),
        _ => Err(denied()),
    }
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
    while !broker::STOP.load(Ordering::Acquire) {
        // SAFETY: read-only active console session query.
        let id = unsafe { WTSGetActiveConsoleSessionId() };
        if id != 0
            && id != u32::MAX
            && security::user_sid(id).is_ok()
            && let Err(error) = serve_session(id)
        {
            eprintln!("Oink UIAccess launch broker: {error}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
fn serve_session(id: u32) -> io::Result<()> {
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
            let ready = unsafe { WaitForSingleObject(op.event(), 250) };
            // SAFETY: read-only active console session query.
            let active = unsafe { WTSGetActiveConsoleSessionId() };
            if broker::STOP.load(Ordering::Acquire)
                || active != id
                || security::user_sid(id).ok().as_ref() != Some(&user)
            {
                return Ok(());
            }
            if ready == WAIT_OBJECT_0 {
                break;
            }
            if ready != WAIT_TIMEOUT {
                return Err(io::Error::last_os_error());
            }
        }
        op.cancel();
        drop(op);
        let result = (|| {
            let request = receive(&pipe)?;
            let _client = security::peer(pipe.0, id, false).map_err(|e| context("authenticate launch client", e))?;
            // A session may have switched while authentication was running.
            // SAFETY: no mutation; launch must still belong to the active interactive user.
            if unsafe { WTSGetActiveConsoleSessionId() } != id {
                return Err(denied());
            }
            if super::update::pending() {
                return Err(denied());
            }
            let user = security::sid_bytes(token(_client.0)?.0)?;
            crate::launch_recipe::dispatch(request, |environment, directory, target| {
                spawn(id, &user, environment, directory, target)
            })
        })();
        let reply = match result {
            Ok(pid) => BackendLaunch::Started { pid },
            Err(error) => {
                eprintln!("Oink UIAccess backend start failed: {error}");
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
) -> io::Result<u32> {
    spawn_at(id, user, environment, directory, target, false)
}
fn spawn_at(
    id: u32,
    user: &[u8],
    environment: &[u16],
    directory: &[u16],
    target: crate::launch_recipe::Target,
    update: bool,
) -> io::Result<u32> {
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
    // Never use the SYSTEM or linked elevated token: user, IL and session remain unchanged.
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
            CREATE_UNICODE_ENVIRONMENT | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | NORMAL_PRIORITY_CLASS,
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
    drop(Handle::new(process.hThread)?);
    drop(Handle::new(process.hProcess)?);
    Ok(process.dwProcessId)
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
    spawn_at(
        id,
        user,
        &block,
        &directory,
        crate::launch_recipe::Target::Backend,
        true,
    )
}
