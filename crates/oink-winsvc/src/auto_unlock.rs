//! Local-only credential management. Passwords are never part of network state.
use std::io;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoUnlockResult {
    Idle,
    Pending,
    Submitted,
    Succeeded,
    Failed,
    Expired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoUnlockFailure {
    None,
    IncorrectPassword,
    WindowsHelloOnly,
    LoginUiNotReady,
    Unknown,
}
/// Latest sync unlock operation, as the service last saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoUnlockSync {
    None,
    Pending,
    /// LogonUI acquired the credential; its outcome is not reported yet.
    Submitted,
    Succeeded,
    Failed,
    /// LogonUI did not serialize within the sync window (curtain still up).
    Expired,
}
/// A sync unlock operation is abandoned when LogonUI has not taken its
/// credential this long after the trigger (user decision Q3).
pub const AUTO_UNLOCK_SYNC_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoUnlockStatus {
    pub enabled: bool,
    pub pending: bool,
    pub last_result: AutoUnlockResult,
    pub failure_kind: AutoUnlockFailure,
    pub windows_hello_only: bool,
    /// Monotonic service diagnostics for the latest provider readiness edge.
    pub provider_ready_at_ms: u64,
    pub remote_input_at_ms: u64,
    pub remote_input_source: u64,
    pub readiness_decision: u32,
    /// Service GetTickCount64 of the latest provider success, and of this reply.
    pub succeeded_at_ms: u64,
    pub now_ms: u64,
    pub sync_id: u64,
    pub sync_outcome: AutoUnlockSync,
    /// Whether the provider polled the sync operation before it expired.
    pub sync_prepared: bool,
}
#[cfg(windows)]
unsafe extern "C" {
    fn oink_unlock_client(
        op: u32,
        source: u64,
        remote_input_age_ms: u32,
        password: *const u16,
        status: *mut u32,
    ) -> u32;
}
fn call(op: u32, source: u64, remote_input_age_ms: u32, password: Option<&str>) -> io::Result<AutoUnlockStatus> {
    call_with_id(op, source, remote_input_age_ms, password).map(|(status, _)| status)
}
fn call_with_id(
    op: u32,
    source: u64,
    remote_input_age_ms: u32,
    password: Option<&str>,
) -> io::Result<(AutoUnlockStatus, u64)> {
    #[cfg(not(windows))]
    {
        let _ = (op, source, remote_input_age_ms, password);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "auto unlock requires Windows",
        ))
    }
    #[cfg(windows)]
    {
        let mut secret = [0u16; 512];
        if let Some(password) = password {
            if password.contains('\0') || password.encode_utf16().count() > 511 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid password length or encoding",
                ));
            }
            for (to, from) in secret.iter_mut().zip(password.encode_utf16()) {
                *to = from;
            }
        }
        let mut status = [0u32; 20];
        // SAFETY: fixed native ABI, input/output buffers live through the call.
        let error =
            unsafe { oink_unlock_client(op, source, remote_input_age_ms, secret.as_ptr(), status.as_mut_ptr()) };
        for value in &mut secret {
            // SAFETY: value uniquely borrows a live, aligned u16 in the stack buffer.
            unsafe { std::ptr::write_volatile(value, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        let last_result = match status[1] {
            0 => AutoUnlockResult::Idle,
            1 => AutoUnlockResult::Pending,
            2 => AutoUnlockResult::Submitted,
            3 => AutoUnlockResult::Succeeded,
            4 => AutoUnlockResult::Failed,
            5 => AutoUnlockResult::Expired,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid unlock status")),
        };
        let failure_kind = match status[2] {
            0 => AutoUnlockFailure::None,
            1 => AutoUnlockFailure::IncorrectPassword,
            2 => AutoUnlockFailure::WindowsHelloOnly,
            3 => AutoUnlockFailure::LoginUiNotReady,
            4 => AutoUnlockFailure::Unknown,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid unlock failure kind",
                ));
            }
        };
        let pair = |low: usize| u64::from(status[low]) | (u64::from(status[low + 1]) << 32);
        let sync_outcome = match status[11] & 0xff {
            0 => AutoUnlockSync::None,
            1 => AutoUnlockSync::Pending,
            2 => AutoUnlockSync::Submitted,
            3 => AutoUnlockSync::Succeeded,
            4 => AutoUnlockSync::Failed,
            5 => AutoUnlockSync::Expired,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid sync status")),
        };
        let status_value = AutoUnlockStatus {
            enabled: status[0] != 0,
            pending: matches!(last_result, AutoUnlockResult::Pending | AutoUnlockResult::Submitted),
            last_result,
            failure_kind,
            windows_hello_only: status[3] != 0,
            provider_ready_at_ms: u64::from(status[4]) | (u64::from(status[5]) << 32),
            remote_input_at_ms: u64::from(status[6]) | (u64::from(status[7]) << 32),
            remote_input_source: u64::from(status[8]) | (u64::from(status[9]) << 32),
            readiness_decision: status[10],
            succeeded_at_ms: pair(12),
            now_ms: pair(14),
            sync_id: pair(16),
            sync_outcome,
            sync_prepared: status[11] & 0x100 != 0,
        };
        Ok((status_value, pair(18)))
    }
}
pub fn auto_unlock_status() -> io::Result<AutoUnlockStatus> {
    call(1, 0, 0, None)
}
/// Store after validating the password. Replaces the local failure latch.
pub fn auto_unlock_save(password: &str) -> io::Result<()> {
    call(2, 0, 0, Some(password)).map(|_| ())
}
pub fn auto_unlock_disable() -> io::Result<()> {
    call(3, 0, 0, None).map(|_| ())
}
/// Request one submission, never retry after an uncertain result.
pub fn auto_unlock_remote_input(source: u64, age_ms: u32) -> io::Result<()> {
    call(8, source, age_ms, None).map(|_| ())
}
/// Request one submission after binding it to the source that answered the
/// unlock query. The provider performs the final two-second readiness check.
pub fn auto_unlock_attempt(source: u64) -> io::Result<()> {
    call(4, source, 0, None).map(|_| ())
}
/// Sync unlock: request one submission for a source that answered the unlock
/// query on this session. Returns the operation id; it expires unconsumed after
/// [`AUTO_UNLOCK_SYNC_WINDOW`] and is never retried.
pub fn auto_unlock_sync_attempt(source: u64) -> io::Result<u64> {
    call_with_id(9, source, 0, None).map(|(_, id)| id)
}
#[cfg(all(test, windows))]
mod tests {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_ALL_ACCESS, REG_DWORD, REG_OPTION_VOLATILE, REG_SZ,
        REG_VALUE_TYPE, RRF_RT_REG_DWORD, RRF_SUBKEY_WOW6464KEY, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW,
        RegGetValueW, RegSetValueExW,
    };
    unsafe extern "C" {
        fn oink_unlock_hello_only_under(root: HKEY) -> bool;
    }
    /// The "only allow Windows Hello sign-in for Microsoft accounts" toggle,
    /// spelled independently of the native literal.
    const KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\PasswordLess\Device";
    const VALUE: &str = "DevicePasswordLessBuildVersion";
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }
    fn create(root: HKEY, path: &str) -> HKEY {
        let mut key = null_mut();
        // SAFETY: path is NUL-terminated and key receives one owned handle.
        let error = unsafe {
            RegCreateKeyExW(
                root,
                wide(path).as_ptr(),
                0,
                null(),
                REG_OPTION_VOLATILE,
                KEY_ALL_ACCESS,
                null(),
                &mut key,
                null_mut(),
            )
        };
        assert_eq!(error, ERROR_SUCCESS, "create {path}");
        key
    }
    fn set(key: HKEY, kind: REG_VALUE_TYPE, data: &[u8]) {
        // SAFETY: key is open for writing and data outlives the call.
        let error = unsafe { RegSetValueExW(key, wide(VALUE).as_ptr(), 0, kind, data.as_ptr(), data.len() as u32) };
        assert_eq!(error, ERROR_SUCCESS);
    }
    fn under(root: HKEY) -> bool {
        // SAFETY: root is an open registry handle; the native side only reads.
        unsafe { oink_unlock_hello_only_under(root) }
    }
    #[test]
    fn hello_only_reads_the_settings_toggle_location_and_value() {
        // A volatile scratch root under HKCU stands in for HKLM: same relative
        // path, so a malformed native literal finds nothing.
        let scratch = format!(r"Software\Oink\UnitTests\HelloOnly-{}", std::process::id());
        let root = create(HKEY_CURRENT_USER, &scratch);
        assert!(!under(root), "missing key means off");
        let device = create(root, KEY);
        assert!(!under(root), "missing value means off");
        set(device, REG_DWORD, &0u32.to_le_bytes());
        assert!(!under(root), "0 is the toggle off");
        set(device, REG_DWORD, &2u32.to_le_bytes());
        assert!(under(root), "2 is the toggle on");
        let text: Vec<u8> = wide("2").iter().flat_map(|c| c.to_le_bytes()).collect();
        set(device, REG_SZ, &text);
        assert!(!under(root), "only a DWORD counts");
        // SAFETY: both handles were opened above and are closed once; the
        // scratch tree is deleted by its own path.
        unsafe {
            RegCloseKey(device);
            RegCloseKey(root);
            assert_eq!(
                RegDeleteTreeW(HKEY_CURRENT_USER, wide(&scratch).as_ptr()),
                ERROR_SUCCESS
            );
        }
    }
    #[test]
    fn hello_only_matches_this_machine() {
        // Read-only cross-check of the real HKLM value on the test machine.
        let mut value = 0u32;
        let mut size = 4u32;
        // SAFETY: value and size are live outputs sized for one DWORD.
        let error = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                wide(KEY).as_ptr(),
                wide(VALUE).as_ptr(),
                RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY,
                null_mut(),
                (&raw mut value).cast(),
                &mut size,
            )
        };
        assert_eq!(under(HKEY_LOCAL_MACHINE), error == ERROR_SUCCESS && value != 0);
    }
}
