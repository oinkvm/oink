//! In-process last-chance numeric fallback. SYSTEM service/input helpers never
//! depend on this crate or install this hook. Raw memory is never captured here.
use std::sync::atomic::{AtomicPtr, Ordering};
use windows_sys::Win32::System::Diagnostics::Debug::{EXCEPTION_POINTERS, SetUnhandledExceptionFilter};
/// Read the real OS version without starting a console command or relying on
/// the application manifest's compatibility version virtualization.
pub fn system_version() -> Option<(u32, u32, u32)> {
    use windows_sys::{Wdk::System::SystemServices::RtlGetVersion, Win32::System::SystemInformation::OSVERSIONINFOW};
    // SAFETY: OSVERSIONINFOW is a plain integer/array structure.
    let mut version: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    version.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    // SAFETY: initialized structure with the required size and live output storage.
    (unsafe { RtlGetVersion(&mut version) } >= 0).then_some((
        version.dwMajorVersion,
        version.dwMinorVersion,
        version.dwBuildNumber,
    ))
}
pub fn os_class() -> oink_telemetry::OsVersionClass {
    match system_version() {
        Some((10, _, build)) if build >= 22000 => oink_telemetry::OsVersionClass::Windows11,
        Some((10, _, _)) => oink_telemetry::OsVersionClass::Windows10,
        _ => oink_telemetry::OsVersionClass::Unknown,
    }
}
static PREVIOUS: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
pub(crate) fn install() {
    // SAFETY: a process-lifetime extern callback with the required ABI; preserves
    // the previous filter's behavior and never dereferences an absent record.
    let previous = unsafe { SetUnhandledExceptionFilter(Some(filter)) };
    PREVIOUS.store(
        previous.map_or(std::ptr::null_mut(), |f| f as *mut _),
        Ordering::Release,
    );
}
unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
    if !info.is_null() {
        // SAFETY: Windows supplies valid exception records during this callback.
        let record = unsafe { (*info).ExceptionRecord };
        if !record.is_null() {
            // SAFETY: the non-null exception record is platform-owned and live.
            crate::capsule::native_exception(unsafe { (*record).ExceptionCode } as u32);
        }
    }
    crate::collector::capture(info);
    let old = PREVIOUS.load(Ordering::Acquire);
    if !old.is_null() {
        // SAFETY: only a function returned by SetUnhandledExceptionFilter is stored.
        let previous: unsafe extern "system" fn(*const EXCEPTION_POINTERS) -> i32 = unsafe { std::mem::transmute(old) };
        // SAFETY: forwarding the original platform-owned callback argument.
        return unsafe { previous(info) };
    }
    0 // EXCEPTION_CONTINUE_SEARCH: preserve OS crash handling.
}

#[cfg(test)]
mod tests {
    use oink_telemetry::*;
    #[test]
    fn version_is_queried_natively_without_a_console_process() {
        let (major, _, build) = super::system_version().expect("native Windows version");
        assert!(major >= 10 && build > 0);
        assert_ne!(super::os_class(), OsVersionClass::Unknown);
    }
    #[test]
    fn unhandled_exception_child_writes_numeric_capsule() {
        const KEY: &str = "OINK_NATIVE_CRASH_UNIT_CHILD";
        if let Some(path) = std::env::var_os(KEY) {
            let c = crate::capsule::Capture::install();
            c.arm(
                std::path::Path::new(&path),
                1,
                Component::Agent,
                BuildContext {
                    app_version: AppVersion {
                        major: 1,
                        minor: 0,
                        patch: 0,
                        build: 1,
                    },
                    os_version_class: OsVersionClass::Windows11,
                },
            )
            .unwrap();
            // SAFETY: isolated test child, no installed application or desktop state.
            unsafe {
                windows_sys::Win32::System::Diagnostics::Debug::SetErrorMode(
                    windows_sys::Win32::System::Diagnostics::Debug::SEM_NOGPFAULTERRORBOX,
                );
                windows_sys::Win32::System::Diagnostics::Debug::RaiseException(0xe000_beef, 0, 0, std::ptr::null());
            }
            std::process::exit(4);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capsule");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows::tests::unhandled_exception_child_writes_numeric_capsule",
                "--nocapture",
            ])
            .env(KEY, &path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let manifest = oink_telemetry::capture::Manifest {
            version: AppVersion {
                major: 1,
                minor: 0,
                patch: 0,
                build: 1,
            },
            images: vec![],
        };
        let (crash, _) = crate::capsule::recover(
            &path,
            ReportConsent {
                choice: Choice::Granted,
                policy_version: 3,
                generation: 1,
            },
            &manifest,
            OsVersionClass::Unknown,
            now(),
        )
        .unwrap();
        assert_eq!(crash.exception_code, Some(0xe000_beef));
        assert_eq!(crash.build.os_version_class, OsVersionClass::Windows11);
    }
}
