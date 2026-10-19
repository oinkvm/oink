//! No libtest harness: the child checks and normalizes its priority in main,
//! before any application threads exist. No desktop, IPC or installed app access.
#[cfg(windows)]
fn main() {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    use windows_sys::Win32::System::Threading::{
        BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetPriorityClass, NORMAL_PRIORITY_CLASS,
    };

    fn priority() -> u32 {
        // SAFETY: read-only query of the current process pseudo-handle.
        let value = unsafe { GetPriorityClass(GetCurrentProcess()) };
        assert_ne!(value, 0, "GetPriorityClass: {}", std::io::Error::last_os_error());
        value
    }

    match std::env::args().nth(1).as_deref() {
        Some("below-normal-child") => {
            assert_eq!(
                priority(),
                BELOW_NORMAL_PRIORITY_CLASS,
                "fixture must start below Normal"
            );
            oink_foundation::process::initialize_priority().unwrap();
            assert_eq!(priority(), NORMAL_PRIORITY_CLASS, "entry point must normalize itself");
            // Model the UI -> host / autostart -> UI chain. No explicit priority
            // flag: this checks the Windows inheritance behavior after startup.
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("grandchild")
                .status()
                .unwrap();
            assert!(status.success(), "grandchild inherited the wrong priority");
        }
        Some("grandchild") => assert_eq!(priority(), NORMAL_PRIORITY_CLASS),
        // Cargo may forward libtest flags (e.g. --skip for an unrelated known
        // failure). This harness has one mandatory check; only its private child
        // modes select a different path.
        _ => {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("below-normal-child")
                .creation_flags(BELOW_NORMAL_PRIORITY_CLASS)
                .status()
                .unwrap();
            assert!(status.success(), "BelowNormal startup regression failed");
            println!("PASS: BelowNormal child normalizes before threads and spawns a Normal grandchild");
        }
    }
}

#[cfg(not(windows))]
fn main() {
    oink_foundation::process::initialize_priority().unwrap();
    println!("SKIP: process priority regression requires Windows");
}
