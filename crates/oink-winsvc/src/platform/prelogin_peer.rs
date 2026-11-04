//! Authentication for the broker-selected backend input route. The ordinary
//! user pipe keeps its original session/SID/impersonation checks unchanged.
use super::*;
use crate::prelogin_route::{Identity, Source};
use windows_sys::Win32::{Storage::FileSystem::SYNCHRONIZE, System::Pipes::*};

pub(super) fn pin(source: &Source) -> io::Result<Handle> {
    let expected = source.identity;
    // SAFETY: query/synchronize handles pin both identities against PID reuse.
    let process =
        Handle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, expected.pid) })?;
    // SAFETY: query/synchronize only; pins the service creation identity.
    let parent =
        Handle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, expected.parent) })?;
    let path = security::image(process.0)?;
    security::protected_file(&path)?;
    let process_token = token(process.0)?;
    let sid = security::sid_bytes(process_token.0)?;
    let mut session = 0;
    // SAFETY: pinned process, valid session output and SID bytes.
    let (system, alive) = unsafe {
        check(ProcessIdToSessionId(expected.pid, &mut session))?;
        (
            IsWellKnownSid(sid.as_ptr().cast_mut().cast(), WinLocalSystemSid) != 0,
            WaitForSingleObject(process.0, 0) == WAIT_TIMEOUT && WaitForSingleObject(parent.0, 0) == WAIT_TIMEOUT,
        )
    };
    let actual = Identity {
        pid: expected.pid,
        session,
        created: prelogin::created(process.0)?,
        parent: prelogin::parent_pid(expected.pid)?,
        parent_created: prelogin::created(parent.0)?,
    };
    let protected = path
        .as_os_str()
        .eq_ignore_ascii_case(security::install_dir()?.join("oink.exe").as_os_str());
    let authorized = if let Some(owner) = &source.user_sid {
        let origin = console::user_token(expected.session)?;
        crate::console_policy::user_source(
            expected,
            actual,
            owner,
            &sid,
            backend::token_uiaccess(process_token.0)? == 1,
            protected,
            alive,
        ) && security::sid_bytes(origin.0)? == *owner
            && source.logon == Some(console::logon(process_token.0)?)
            && source.logon == Some(console::logon(origin.0)?)
            && source.owner_sid == *owner
    } else {
        source.logon.is_none() && expected.accepts(actual, system, protected, alive)
    };
    if !authorized || expected.parent != service_peer::installed_pid()? {
        return Err(denied());
    }
    Ok(process)
}

/// Called after reading the first frame. The pinned expected handle remains
/// alive in the helper; each accepted connection also holds its own handle.
pub(super) fn peer(pipe: HANDLE, source: &Source) -> io::Result<Handle> {
    let expected = source.identity;
    let (mut pid, mut session) = (0, 0);
    // SAFETY: connected local pipe, kernel-supplied client identity.
    unsafe {
        check(GetNamedPipeClientProcessId(pipe, &mut pid))?;
        check(GetNamedPipeClientSessionId(pipe, &mut session))?;
    }
    if pid != expected.pid || session != expected.session {
        return Err(denied());
    }
    let process = pin(source)?;
    // Bind the pipe's effective token as well as its process token. A different
    // logon of the same SID cannot borrow a registered user's connection.
    // SAFETY: identification impersonation only, always reverted before work.
    unsafe {
        check(ImpersonateNamedPipeClient(pipe))?;
        let mut raw = ptr::null_mut();
        let identity = check(OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut raw)).and_then(|()| {
            let t = Handle::new(raw)?;
            Ok((security::sid_bytes(t.0)?, console::logon(t.0)?))
        });
        if RevertToSelf() == 0 {
            std::process::abort();
        }
        let (sid, logon) = identity?;
        let valid = match &source.user_sid {
            Some(owner) => &sid == owner && source.logon == Some(logon),
            None => IsWellKnownSid(sid.as_ptr().cast_mut().cast(), WinLocalSystemSid) != 0,
        };
        if !valid {
            return Err(denied());
        }
    }
    Ok(process)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(identity: Identity) -> Source {
        Source {
            identity,
            user_sid: None,
            logon: None,
            owner_sid: vec![],
        }
    }
    #[test]
    fn kernel_pipe_identity_cannot_be_relabelled_as_the_broker_child() {
        let (server, _client) = pipe_io::tests::pair();
        let own_session = session().unwrap();
        let expected = Identity {
            pid: std::process::id().wrapping_add(1),
            session: own_session,
            created: 20,
            parent: 7,
            parent_created: 10,
        };
        assert!(peer(server.0, &source(expected)).is_err());
        let wrong_session = Identity {
            pid: std::process::id(),
            session: own_session.wrapping_add(1),
            ..expected
        };
        assert!(peer(server.0, &source(wrong_session)).is_err());
        // Even a matching kernel PID/session from a normal test process cannot
        // enter: it is neither the service-pinned protected image nor SYSTEM.
        let ordinary = Identity {
            pid: std::process::id(),
            session: own_session,
            ..expected
        };
        assert!(peer(server.0, &source(ordinary)).is_err());
    }
}
