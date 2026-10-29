//! Clipboard counters are only unique inside their window station and login.
//! Read our own token/session metadata; no RDS service, clipboard GetData or
//! retained clipboard handle is needed, and no credentials enter the journal.
use std::{io, mem::size_of};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Security::{
        Authentication::Identity::{LsaFreeReturnBuffer, LsaGetLogonSessionData, SECURITY_LOGON_SESSION_DATA},
        GetTokenInformation, TOKEN_QUERY, TOKEN_STATISTICS, TokenStatistics,
    },
    System::{
        DataExchange::GetClipboardSequenceNumber,
        StationsAndDesktops::{GetProcessWindowStation, GetUserObjectInformationW, UOI_NAME},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

pub(super) fn current() -> io::Result<[u8; 32]> {
    // SAFETY: all buffers below are initialized and sized for the requested
    // structure. Only this process's token is queried. Owned token/LSA buffers
    // are released on every path; the borrowed station is never closed.
    unsafe {
        let before = GetClipboardSequenceNumber();
        if before == 0 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let mut stats = TOKEN_STATISTICS::default();
        let mut returned = 0;
        let result = GetTokenInformation(
            token,
            TokenStatistics,
            Some((&mut stats as *mut TOKEN_STATISTICS).cast()),
            size_of::<TOKEN_STATISTICS>() as u32,
            &mut returned,
        );
        let _ = CloseHandle(token);
        result?;
        if returned as usize != size_of::<TOKEN_STATISTICS>() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut session: *mut SECURITY_LOGON_SESSION_DATA = std::ptr::null_mut();
        let status = LsaGetLogonSessionData(&stats.AuthenticationId, &mut session);
        if status.is_err() || session.is_null() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        // These fields precede the optional later additions to the structure.
        let valid = (*session).Size as usize >= std::mem::offset_of!(SECURITY_LOGON_SESSION_DATA, LogonServer);
        let details = valid.then(|| ((*session).LogonTime, (*session).Session));
        let _ = LsaFreeReturnBuffer(session.cast());
        let (logon, session_id) = details
            .filter(|(logon, _)| *logon > 0)
            .ok_or(io::ErrorKind::InvalidData)?;
        let station = GetProcessWindowStation()?;
        let mut bytes = 0;
        let _ = GetUserObjectInformationW(HANDLE(station.0), UOI_NAME, None, 0, Some(&mut bytes));
        if !(2..=65536).contains(&bytes) || !bytes.is_multiple_of(2) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut name = vec![0u16; bytes as usize / 2];
        GetUserObjectInformationW(
            HANDLE(station.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            bytes,
            Some(&mut returned),
        )?;
        if returned > bytes || name.last() != Some(&0) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut hash = blake3::Hasher::new();
        hash.update(b"across/windows-current-clipboard/1");
        hash.update(&logon.to_le_bytes());
        hash.update(&stats.AuthenticationId.LowPart.to_le_bytes());
        hash.update(&stats.AuthenticationId.HighPart.to_le_bytes());
        hash.update(&session_id.to_le_bytes());
        for unit in name {
            hash.update(&unit.to_le_bytes());
        }
        hash.update(&before.to_le_bytes());
        if GetClipboardSequenceNumber() != before {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(*hash.finalize().as_bytes())
    }
}
