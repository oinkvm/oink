//! Bind changeCount to its boot, login security session and named pasteboard.
//! SessionGetInfo is read-only; changeCount never asks an owner to render data.
use std::io;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SessionGetInfo(session: u32, session_id: *mut u32, attributes: *mut u32) -> i32;
}

pub(super) fn current(name: Option<&str>) -> io::Result<[u8; 32]> {
    let before = super::pasteboard::change_count(name);
    let mut session = 0u32;
    let mut attributes = 0u32;
    // kern.boottime changes when the wall clock is corrected. The boot UUID
    // is stable (XNU bsd/kern/kern_sysctl.c); an unavailable/denied UUID defers
    // identity confirmation instead of guessing from a potentially reused count.
    let mut boot = [0u8; 37];
    let mut size = boot.len();
    // SAFETY: output buffers have the declared ABI sizes. callerSecuritySession
    // (-1) asks only for this process's security session; sysctl reads metadata.
    let uid = unsafe {
        if SessionGetInfo(u32::MAX, &mut session, &mut attributes) != 0 || session == 0 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        if libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            boot.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        libc::getuid()
    };
    if size != boot.len()
        || boot[36] != 0
        || !boot[..36].iter().enumerate().all(|(i, byte)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut hash = blake3::Hasher::new();
    hash.update(b"across/macos-current-clipboard/1");
    hash.update(&boot[..36]);
    hash.update(&session.to_le_bytes());
    hash.update(&uid.to_le_bytes());
    // A named board and the system board cannot alias, even if names match.
    hash.update(&[u8::from(name.is_some())]);
    hash.update(name.unwrap_or_default().as_bytes());
    hash.update(&before.to_le_bytes());
    if super::pasteboard::change_count(name) != before {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    Ok(*hash.finalize().as_bytes())
}
