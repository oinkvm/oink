use super::*;
unsafe extern "C" {
    fn oink_unlock_start(auth: extern "C" fn(HANDLE, u32) -> HANDLE) -> u32;
    fn oink_unlock_stop();
    fn oink_unlock_wake();
    fn oink_unlock_session(kind: u32, session: u32) -> bool;
    fn oink_unlock_register(dll: *const u16) -> u32;
    fn oink_unlock_unregister(step: *mut u32, object: *mut u16, capacity: u32) -> u32;
}
extern "C" fn authenticate(pipe: HANDLE, session: u32) -> HANDLE {
    // Native worker calls only after reading a complete request. Retain the
    // authenticated creation-object handle through this check; native processing
    // remains bound to this connected pipe and rechecks the current console.
    match security::peer(pipe, session, false) {
        Ok(handle) => {
            let raw = handle.0;
            std::mem::forget(handle);
            raw
        }
        Err(_) => std::ptr::null_mut(),
    }
}
fn result(code: u32) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}
fn register() -> io::Result<()> {
    let path = security::install_dir()?.join("OinkCredentialProvider.dll");
    security::protected_file(&path)?;
    let path = wide(path.to_str().ok_or_else(denied)?);
    // SAFETY: native registration uses only the fixed private CLSID.
    result(unsafe { oink_unlock_register(path.as_ptr()) })
}
/// Uninstall (`--unregister-credential-provider`, run by the MSI as SYSTEM):
/// erase the auto unlock vault and remove the provider registration. An
/// already-absent vault or registration counts as removed. A failure names the
/// step and object; the MSI action copies this text into the installer log.
pub(super) fn unregister() -> io::Result<()> {
    let mut step = 0;
    let mut object = [0u16; 1024];
    // SAFETY: valid output pointers; the native side writes at most `capacity`
    // UTF-16 units including the terminator.
    let code = unsafe { oink_unlock_unregister(&mut step, object.as_mut_ptr(), object.len() as u32) };
    unregister_result(code, step, &object)
}
fn unregister_result(code: u32, step: u32, object: &[u16]) -> io::Result<()> {
    if code == 0 {
        return Ok(());
    }
    let end = object.iter().position(|&c| c == 0).unwrap_or(object.len());
    let object = String::from_utf16_lossy(&object[..end]);
    let error = io::Error::from_raw_os_error(code as i32);
    let what = unregister_step(step);
    let message = if object.is_empty() {
        format!("unregister credential provider: {what}: {error}")
    } else {
        format!("unregister credential provider: {what} {object}: {error}")
    };
    Err(io::Error::new(error.kind(), message))
}
/// Step numbers from `UnregisterStep` in native/unlock.cpp.
fn unregister_step(step: u32) -> &'static str {
    match step {
        1 => "not running as LocalSystem",
        2 => "locate ProgramData",
        3 => "open auto unlock vault",
        4 => "refusing non-SYSTEM-only auto unlock vault",
        5 => "list auto unlock vault",
        6 => "refusing directory or link in auto unlock vault",
        7 => "delete auto unlock vault file",
        8 => "remove auto unlock vault directory",
        9 => "delete credential provider key HKLM",
        10 => "delete credential provider class key HKLM",
        _ => "unknown step",
    }
}
pub(super) fn start() -> io::Result<()> {
    register()?;
    // SAFETY: function pointer remains live throughout the native worker.
    result(unsafe { oink_unlock_start(authenticate) })
}
pub(super) fn stop() {
    // SAFETY: no Rust-owned memory crosses the call; the native side owns its worker.
    unsafe { oink_unlock_stop() };
}
pub(super) fn session_event(kind: u32, session: u32) {
    // SAFETY: scalar notification values cross the ABI; the native side owns its state.
    let own = unsafe { oink_unlock_session(kind, session) };
    if own {
        tracing::info!(session, "auto unlock own event suppressed");
    }
}

static EVENTS: std::sync::LazyLock<crossbeam_queue::ArrayQueue<(u32, u32)>> =
    std::sync::LazyLock::new(|| crossbeam_queue::ArrayQueue::new(64));
pub(super) fn queue_event(kind: u32, session: u32) {
    let _ = EVENTS.push((kind, session));
    // SAFETY: native event is process-lived; this callback only signals it.
    unsafe { oink_unlock_wake() };
}
pub(super) fn drain_events() {
    while let Some((kind, session)) = EVENTS.pop() {
        session_event(kind, session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_READ, REG_OPTION_VOLATILE, RegCloseKey, RegCreateKeyExW,
        RegDeleteTreeW, RegOpenKeyExW,
    };
    unsafe extern "C" {
        fn oink_unlock_unregister_under(
            hive: HKEY,
            vault: *const u16,
            step: *mut u32,
            object: *mut u16,
            capacity: u32,
        ) -> u32;
    }
    /// Registration paths, spelled independently of the native literals.
    const PROVIDER: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\Credential Providers\{361D74A8-F854-44FD-96B4-CAD7BFB51E09}";
    const CLASS: &str = r"SOFTWARE\Classes\CLSID\{361D74A8-F854-44FD-96B4-CAD7BFB51E09}";

    /// A volatile HKCU tree standing in for HKLM, deleted on drop.
    struct Scratch {
        path: String,
        root: HKEY,
    }
    impl Scratch {
        fn new(name: &str) -> Self {
            let path = format!(r"Software\Oink\UnitTests\Unregister-{name}-{}", std::process::id());
            let root = create(HKEY_CURRENT_USER, &path);
            Self { path, root }
        }
        fn exists(&self, path: &str) -> bool {
            let mut key = null_mut();
            // SAFETY: NUL-terminated path; key receives one owned handle, closed below.
            let error = unsafe { RegOpenKeyExW(self.root, wide(path).as_ptr(), 0, KEY_READ, &mut key) };
            if error == ERROR_SUCCESS {
                // SAFETY: opened above.
                unsafe { RegCloseKey(key) };
            }
            error == ERROR_SUCCESS
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            // SAFETY: root was opened by new; the tree is deleted by its own path.
            unsafe {
                RegCloseKey(self.root);
                RegDeleteTreeW(HKEY_CURRENT_USER, wide(&self.path).as_ptr());
            }
        }
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
    fn register_under(scratch: &Scratch) {
        for path in [PROVIDER, &format!(r"{CLASS}\InprocServer32")] {
            // SAFETY: handle returned by create is owned here.
            unsafe { RegCloseKey(create(scratch.root, path)) };
        }
    }
    fn unregister_under(scratch: &Scratch, vault: &std::path::Path) -> io::Result<()> {
        let mut step = 0;
        let mut object = [0u16; 1024];
        let vault = wide(vault.to_str().unwrap());
        // SAFETY: open scratch root, NUL-terminated vault path, live outputs.
        let code =
            unsafe { oink_unlock_unregister_under(scratch.root, vault.as_ptr(), &mut step, object.as_mut_ptr(), 1024) };
        unregister_result(code, step, &object)
    }
    fn vault_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("oink-unregister-{name}-{}", std::process::id()))
    }

    #[test]
    fn already_clean_state_is_removed() {
        // Never enabled, or provider keys deleted by hand (issue #46).
        let scratch = Scratch::new("clean");
        unregister_under(&scratch, &vault_path("clean").join("OinkAutoUnlock")).unwrap();
        // A missing parent (no ProgramData entry at all) is absent too.
        unregister_under(
            &scratch,
            &vault_path("clean-parent").join("missing").join("OinkAutoUnlock"),
        )
        .unwrap();
    }

    #[test]
    fn registration_is_removed() {
        let scratch = Scratch::new("registered");
        register_under(&scratch);
        unregister_under(&scratch, &vault_path("registered")).unwrap();
        assert!(!scratch.exists(PROVIDER));
        assert!(!scratch.exists(CLASS));
        // Running again after a completed removal still succeeds.
        unregister_under(&scratch, &vault_path("registered")).unwrap();
    }

    #[test]
    fn unsafe_vault_names_the_step_and_keeps_everything() {
        // A vault this (non-SYSTEM) test user created is not SYSTEM-only: the
        // removal refuses it, says so, and touches neither the files nor the
        // registration.
        let scratch = Scratch::new("unsafe");
        register_under(&scratch);
        let vault = vault_path("unsafe");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(vault.join("S-1-5-21-1.dpapi"), b"x").unwrap();
        let error = unregister_under(&scratch, &vault).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("refusing non-SYSTEM-only auto unlock vault"), "{text}");
        assert!(text.contains(vault.to_str().unwrap()), "{text}");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{text}");
        assert!(vault.join("S-1-5-21-1.dpapi").exists());
        assert!(scratch.exists(PROVIDER) && scratch.exists(CLASS));
        std::fs::remove_dir_all(&vault).unwrap();
    }

    #[test]
    fn unregister_requires_local_system_and_says_so() {
        // Test processes are not LocalSystem: the real entry point stops at
        // the identity step before touching HKLM or ProgramData.
        let text = unregister().unwrap_err().to_string();
        assert!(text.contains("not running as LocalSystem"), "{text}");
    }

    #[test]
    fn failures_name_step_object_and_os_error() {
        let object = wide(r"C:\ProgramData\OinkAutoUnlock");
        let error = unregister_result(32, 8, &object).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                r"unregister credential provider: remove auto unlock vault directory C:\ProgramData\OinkAutoUnlock: {}",
                io::Error::from_raw_os_error(32)
            )
        );
        let error = unregister_result(5, 1, &[0]).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("unregister credential provider: not running as LocalSystem: ")
        );
        assert!(unregister_result(0, 0, &[0]).is_ok());
    }
}
