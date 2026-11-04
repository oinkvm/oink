//! Broker-owned console routing. No client can register a PID or select a target.
use super::*;
use crate::{
    console_policy::{Account, ConsoleOwner},
    prelogin_route::{Identity, Source},
};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::System::{
    RemoteDesktop::*,
    SystemInformation::{ComputerNamePhysicalNetBIOS, GetComputerNameExW},
};

pub(super) fn logon(token: HANDLE) -> io::Result<u64> {
    let mut info = TOKEN_STATISTICS::default();
    let mut size = 0;
    // SAFETY: fixed token query and correctly sized/aligned output.
    check(unsafe {
        GetTokenInformation(
            token,
            TokenStatistics,
            ptr::addr_of_mut!(info).cast(),
            size_of::<TOKEN_STATISTICS>() as u32,
            &mut size,
        )
    })?;
    Ok((info.AuthenticationId.HighPart as u32 as u64) << 32 | u64::from(info.AuthenticationId.LowPart))
}
pub(super) fn user_token(id: u32) -> io::Result<Handle> {
    let mut raw = ptr::null_mut();
    // SAFETY: service/helper queries WTS; never accepts a client token.
    check(unsafe { WTSQueryUserToken(id, &mut raw) })?;
    Handle::new(raw)
}

/// Enumerate all sessions, including disconnected and tokenless ones. Token
/// absence is not session destruction, and a failed query is not proof either.
pub(super) fn session_exists(id: u32) -> io::Result<bool> {
    let (mut raw, mut count) = (ptr::null_mut(), 0);
    // SAFETY: WTS owns the returned array until WTSFreeMemory. No client IDs or
    // user-only filter can hide a disconnected session with outstanding holds.
    unsafe {
        check(WTSEnumerateSessionsW(
            WTS_CURRENT_SERVER_HANDLE,
            0,
            1,
            &mut raw,
            &mut count,
        ))?;
        if raw.is_null() {
            return Err(io::Error::other("empty WTS session enumeration"));
        }
        let exists = std::slice::from_raw_parts(raw, count as usize)
            .iter()
            .any(|s| s.SessionId == id);
        WTSFreeMemory(raw.cast());
        Ok(exists)
    }
}

fn no_user(id: u32, error: &io::Error) -> bool {
    if error.raw_os_error() != Some(ERROR_NO_TOKEN as i32) {
        return false;
    }
    let mut raw = ptr::null_mut();
    let mut bytes = 0;
    // ERROR_NO_TOKEN alone also occurs during transitions. Require a successful
    // empty WTS username observation; access/query failures are never a login screen.
    // SAFETY: WTS allocates the output; inspect bounded UTF-16 then free it.
    unsafe {
        if WTSQuerySessionInformationW(ptr::null_mut(), id, WTSUserName, &mut raw, &mut bytes) == 0 {
            return false;
        }
        let empty = !raw.is_null() && bytes >= 2 && *raw == 0;
        WTSFreeMemory(raw.cast());
        empty
    }
}

fn is_wsi(sid: &[u8]) -> bool {
    // An unqualified LookupAccountName can fall through to a trusted domain.
    // Qualify with this machine and also verify the returned domain and user type.
    // Never assume a RID: Windows 11 evidence uses a different RID from examples.
    // Admin/SYSTEM creation/rename of local accounts is an accepted trust boundary;
    // no independently verified Wsi-specific system flag is available.
    // SAFETY: fixed, aligned output buffers; all returned lengths checked before use.
    unsafe {
        let mut computer = [0u16; 256];
        let mut computer_len = computer.len() as u32;
        if GetComputerNameExW(ComputerNamePhysicalNetBIOS, computer.as_mut_ptr(), &mut computer_len) == 0 {
            return false;
        }
        let Ok(computer) = String::from_utf16(&computer[..computer_len as usize]) else {
            return false;
        };
        let name = wide(&format!("{computer}\\WsiAccount"));
        let mut local_sid = [0u32; 17];
        let mut sid_len = std::mem::size_of_val(&local_sid) as u32;
        let mut domain = [0u16; 256];
        let mut domain_len = domain.len() as u32;
        let mut kind = SidTypeUnknown;
        if LookupAccountNameW(
            ptr::null(),
            name.as_ptr(),
            local_sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        ) == 0
            || sid_len as usize > std::mem::size_of_val(&local_sid)
            || domain_len as usize > domain.len()
        {
            return false;
        }
        let domain = &domain[..domain.iter().position(|c| *c == 0).unwrap_or(domain.len())];
        let local_user =
            kind == SidTypeUser && String::from_utf16(domain).is_ok_and(|d| d.eq_ignore_ascii_case(&computer));
        let bytes = std::slice::from_raw_parts(local_sid.as_ptr().cast::<u8>(), sid_len as usize);
        crate::console_policy::wsi_sid(sid, bytes, local_user)
    }
}

pub(super) fn account(id: u32) -> Account {
    if id == 0 || id == u32::MAX {
        return Account::Unknown;
    }
    match user_token(id) {
        Ok(t) => match security::sid_bytes(t.0) {
            Ok(sid) if is_wsi(&sid) => Account::Wsi(sid),
            Ok(sid) if !crate::token_policy::service_account(&sid) => Account::User(sid),
            _ => Account::Unknown,
        },
        Err(e) if no_user(id, &e) => Account::NoUser,
        Err(_) => Account::Unknown,
    }
}

/// Execution-time check uses the already classified target's exact SID, not
/// another account-name lookup. A changed/unknown target requires a new helper.
pub(super) fn matches_account(id: u32, expected: &Account) -> bool {
    // SAFETY: read-only physical console observation.
    if unsafe { WTSGetActiveConsoleSessionId() } != id {
        return false;
    }
    match (expected, user_token(id)) {
        (Account::NoUser, Err(e)) => no_user(id, &e),
        (Account::Wsi(sid) | Account::User(sid), Ok(token)) => security::sid_bytes(token.0).is_ok_and(|s| &s == sid),
        _ => false,
    }
}

struct Registered {
    source: Source,
    process: Handle,
}
// SAFETY: pinned process handles are used only for identity queries and waits.
unsafe impl Send for Registered {}
#[derive(Default)]
struct Registry {
    children: Vec<Registered>,
    owner: ConsoleOwner,
}
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

/// Only called with the suspended --service process returned by CreateProcessAsUser.
pub(super) fn register(process: &Handle, id: u32, sid: &[u8]) -> io::Result<()> {
    let t = token(process.0)?;
    // SAFETY: retained child and service pseudo handle identify both generations.
    let identity = unsafe {
        Identity {
            pid: GetProcessId(process.0),
            session: id,
            created: prelogin::created(process.0)?,
            parent: GetCurrentProcessId(),
            parent_created: prelogin::created(GetCurrentProcess())?,
        }
    };
    let source = Source {
        identity,
        user_sid: Some(sid.to_vec()),
        owner_sid: sid.to_vec(),
        logon: Some(logon(t.0)?),
    };
    let _verified = prelogin_peer::pin(&source)?;
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    registry.children.retain(|c| {
        // SAFETY: retained process handle, nonblocking liveness check.
        unsafe { WaitForSingleObject(c.process.0, 0) == WAIT_TIMEOUT }
    });
    if registry.children.iter().any(|c| c.source.identity.session == id) {
        return Err(io::Error::other("a registered backend is still alive in this session"));
    }
    let mut retained = ptr::null_mut();
    // SAFETY: duplicate the broker's newly created child handle before resume.
    check(unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            process.0,
            GetCurrentProcess(),
            &mut retained,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    })?;
    registry.children.push(Registered {
        source,
        process: Handle::new(retained)?,
    });
    // The launch path verified this was the active console, never an RDP source.
    registry.owner.0 = Some(id);
    Ok(())
}

pub(super) fn select(id: u32, account: &Account, prelogin: Option<Source>) -> Option<Source> {
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    registry.owner.observe(id, account);
    if let Some(source) = prelogin {
        if id == source.identity.session && matches!(account, Account::NoUser) {
            registry.owner.0 = Some(id);
        }
        return (registry.owner.0 == Some(source.identity.session)
            && crate::console_policy::allows(source.identity.session, id, account, &source.owner_sid))
        .then_some(source);
    }
    registry.children.retain(|c| {
        // SAFETY: owned handle pins PID and creation lifetime until exit.
        unsafe { WaitForSingleObject(c.process.0, 0) == WAIT_TIMEOUT }
    });
    registry
        .children
        .iter()
        .find(|c| {
            registry.owner.0 == Some(c.source.identity.session)
                && crate::console_policy::allows(c.source.identity.session, id, account, &c.source.owner_sid)
        })
        .map(|c| c.source.clone())
}

/// A source cannot survive logout/reuse of its original session as an authority.
pub(super) fn source_current(source: &Source) -> bool {
    match (&source.user_sid, source.logon) {
        (Some(sid), Some(logon_id)) => user_token(source.identity.session).is_ok_and(|t| {
            security::sid_bytes(t.0).is_ok_and(|actual| actual == *sid) && logon(t.0).ok() == Some(logon_id)
        }),
        (None, None) => true,
        _ => false,
    }
}
