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
        let mut status = [0u32; 11];
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
        Ok(AutoUnlockStatus {
            enabled: status[0] != 0,
            pending: matches!(last_result, AutoUnlockResult::Pending | AutoUnlockResult::Submitted),
            last_result,
            failure_kind,
            windows_hello_only: status[3] != 0,
            provider_ready_at_ms: u64::from(status[4]) | (u64::from(status[5]) << 32),
            remote_input_at_ms: u64::from(status[6]) | (u64::from(status[7]) << 32),
            remote_input_source: u64::from(status[8]) | (u64::from(status[9]) << 32),
            readiness_decision: status[10],
        })
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
