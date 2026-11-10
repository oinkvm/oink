//! The local user token requests a graceful, planned restart. No elevation/force.
#[cfg(windows)]
fn arguments() -> (u32, u32) {
    // EWX_REBOOT; APPLICATION | INSTALLATION | PLANNED.
    (0x2, 0x0004_0000 | 0x2 | 0x8000_0000)
}

#[cfg(windows)]
pub(crate) fn restart() -> Result<(), String> {
    use windows::{
        Win32::{
            Foundation::*,
            Security::*,
            System::{Shutdown::*, Threading::*},
        },
        core::PCWSTR,
    };
    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: the guard owns this open token handle and closes it once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    // Serialize privilege enable/restore on this process token.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let run = || -> windows::core::Result<()> {
        let mut handle = HANDLE::default();
        // SAFETY: valid process handle and writable outputs; no impersonation.
        unsafe {
            OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut handle)?;
            let token = Token(handle);
            let mut user = [0u8; 256];
            let mut needed = 0;
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(user.as_mut_ptr().cast()),
                user.len() as u32,
                &mut needed,
            )?;
            let token_user = std::ptr::read_unaligned(user.as_ptr().cast::<TOKEN_USER>());
            if [WinLocalSystemSid, WinLocalServiceSid, WinNetworkServiceSid]
                .iter()
                .any(|kind| IsWellKnownSid(token_user.User.Sid, *kind).as_bool())
            {
                return Err(windows::core::Error::from_hresult(ERROR_ACCESS_DENIED.to_hresult()));
            }
            let mut luid = LUID::default();
            LookupPrivilegeValueW(PCWSTR::null(), SE_SHUTDOWN_NAME, &mut luid)?;
            let requested = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let mut previous = TOKEN_PRIVILEGES::default();
            let mut length = 0;
            AdjustTokenPrivileges(
                token.0,
                false,
                Some(&requested),
                std::mem::size_of_val(&previous) as u32,
                Some(&mut previous),
                Some(&mut length),
            )?;
            let assigned = GetLastError();
            if assigned == ERROR_NOT_ALL_ASSIGNED {
                return Err(windows::core::Error::from_hresult(assigned.to_hresult()));
            }
            let (flags, reason) = arguments();
            let result = ExitWindowsEx(EXIT_WINDOWS_FLAGS(flags), SHUTDOWN_REASON(reason));
            // Restore the original privilege even when Windows rejects the request.
            let restored = AdjustTokenPrivileges(token.0, false, Some(&previous), 0, None, None);
            result.and(restored)
        }
    };
    run().map_err(|error| error.to_string())
}
