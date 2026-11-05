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
        let bytes = lookup_sid_bytes(&local_sid);
        crate::console_policy::wsi_sid(sid, bytes, local_user)
    }
}

// SAFETY: storage contains a valid SID returned by a successful Windows lookup.
unsafe fn lookup_sid_bytes(storage: &[u32; 17]) -> &[u8] {
    // LookupAccountNameW only promises a required cbSid on insufficient space.
    // Windows 11 25H2 leaves our successful 68-byte capacity unchanged for the
    // 28-byte WsiAccount SID. Comparing the padded buffer misclassifies it as a
    // real user, revoking the original source and spawning an ordinary helper.
    // Read the SID's own length; exact SID/domain/type authorization is unchanged.
    // SAFETY: caller guarantees a valid SID in this aligned maximum-size buffer.
    unsafe {
        let length = GetLengthSid(storage.as_ptr().cast_mut().cast()) as usize;
        std::slice::from_raw_parts(storage.as_ptr().cast(), length)
    }
}

#[cfg(test)]
mod sid_tests {
    use super::*;

    #[test]
    fn lookup_buffer_capacity_is_not_the_local_account_sid_length() {
        let mut storage = [0u32; 17];
        let mut expected = vec![1, 5, 0, 0, 0, 0, 0, 5];
        for n in [21u32, 11, 22, 33, 1022] {
            expected.extend(n.to_le_bytes());
        }
        // SAFETY: the complete valid SID fits in the aligned initialized storage.
        unsafe { ptr::copy_nonoverlapping(expected.as_ptr(), storage.as_mut_ptr().cast(), expected.len()) };
        // SAFETY: constructed a complete valid local-account SID above.
        let actual = unsafe { lookup_sid_bytes(&storage) };
        assert_eq!(actual, expected);
        assert!(crate::console_policy::wsi_sid(&expected, actual, true));
    }

    #[test]
    fn successful_native_lookup_uses_the_sid_not_the_supplied_capacity() {
        let mut storage = [0u32; 17];
        let mut size = std::mem::size_of_val(&storage) as u32;
        let mut domain = [0u16; 256];
        let mut domain_size = domain.len() as u32;
        let mut kind = SidTypeUnknown;
        // A fixed OS account avoids depending on a PIN or WsiAccount existing.
        // SAFETY: initialized aligned buffers and output lengths; query only.
        check(unsafe {
            LookupAccountNameW(
                ptr::null(),
                wide("NT AUTHORITY\\SYSTEM").as_ptr(),
                storage.as_mut_ptr().cast(),
                &mut size,
                domain.as_mut_ptr(),
                &mut domain_size,
                &mut kind,
            )
        })
        .unwrap();
        // SAFETY: successful OS lookup returned a valid SID.
        let actual = unsafe { lookup_sid_bytes(&storage) };
        assert_eq!(actual, [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]);
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
    super::broker::changed();
    Ok(())
}

pub(super) fn processes() -> io::Result<Vec<Handle>> {
    let registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    // Duplicate under the registry lock; never return a borrowed handle that a
    // concurrent handoff or session cleanup could close before registration.
    registry
        .children
        .iter()
        .map(|child| child.process.duplicate())
        .collect()
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
