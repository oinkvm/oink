//! WTS observations are authority; session notifications only wake observers.
use super::*;
use windows_sys::Win32::System::RemoteDesktop::*;

fn number<T: Copy>(id: u32, class: WTS_INFO_CLASS) -> io::Result<T> {
    let mut raw = ptr::null_mut();
    let mut bytes = 0;
    // SAFETY: WTS allocates a buffer, checked before reading and freed once.
    unsafe {
        check(WTSQuerySessionInformationW(
            ptr::null_mut(),
            id,
            class,
            &mut raw,
            &mut bytes,
        ))?;
        let value = if !raw.is_null() && bytes as usize >= size_of::<T>() {
            Ok(raw.cast::<T>().read_unaligned())
        } else {
            Err(denied())
        };
        WTSFreeMemory(raw.cast());
        value
    }
}
fn observe(id: u32) -> crate::session_policy::Session {
    // The process session ID survives tscon; the transport and console attachment
    // change. A startup-only SM_REMOTESESSION check cannot track that.
    // Re-query WTS on every observation, including disconnected sessions. Unknown
    // observations fail closed. Measured on Windows 10 Pro 22H2 (19045): the
    // same process/session changes from Active/protocol 2 to Active/protocol 0
    // and becomes the physical console after tscon; RDP reconnect reverses it.
    // Disconnect without tscon reported Disconnected/protocol 0 while another
    // session owned the console, so protocol 0 alone is not console authority.
    // Other editions/versions have not been measured.
    crate::session_policy::Session {
        id,
        // SAFETY: read-only physical console query.
        physical_console: unsafe { WTSGetActiveConsoleSessionId() },
        active: number::<i32>(id, WTSConnectState).is_ok_and(|state| state == WTSActive),
        protocol: number::<u16>(id, WTSClientProtocolType).ok(),
    }
}
/// WTSActive alone is insufficient during physical-console handover. Windows
/// returns u32::MAX while attaching/detaching the console. A fresh observation
/// can resolve that transition without asking the user to retry.
#[allow(non_upper_case_globals)] // WTS enum constants use Win32 spelling.
pub(super) fn startup_session(id: u32) -> (&'static str, String) {
    let state = number::<i32>(id, WTSConnectState);
    let protocol = number::<u16>(id, WTSClientProtocolType);
    // SAFETY: read-only query, takes no handles.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    let detail = format!("session={id} WTSConnectState={state:?} WTSClientProtocolType={protocol:?} console={console}");
    let reason = match (state, protocol) {
        (Ok(WTSActive), Ok(p))
            if (crate::session_policy::Session {
                id,
                physical_console: console,
                active: true,
                protocol: Some(p),
            })
            .launch_allowed() =>
        {
            "eligible"
        }
        (Ok(WTSActive), Ok(0)) if console == u32::MAX => "transition",
        (Ok(WTSInit | WTSConnectQuery), _) => "transition",
        (Ok(WTSDisconnected), _) => "session",
        (Ok(_), Ok(_)) => "session",
        _ => "unavailable",
    };
    (reason, detail)
}
pub(super) fn console(id: u32) -> bool {
    observe(id).console()
}
pub fn remote_desktop() -> bool {
    session().map_or(true, |id| !console(id))
}
pub(super) fn launch_allowed(id: u32) -> bool {
    observe(id).launch_allowed()
}
pub(super) fn active() -> io::Result<Vec<u32>> {
    let mut raw = ptr::null_mut();
    let mut count = 0;
    // SAFETY: WTS-owned array, freed after copying session IDs.
    unsafe {
        check(WTSEnumerateSessionsW(ptr::null_mut(), 0, 1, &mut raw, &mut count))?;
        let ids = std::slice::from_raw_parts(raw, count as usize)
            .iter()
            .map(|s| s.SessionId)
            .filter(|id| launch_allowed(*id))
            .collect();
        WTSFreeMemory(raw.cast());
        Ok(ids)
    }
}

pub fn current_launch_allowed() -> bool {
    session().is_ok_and(launch_allowed)
}
