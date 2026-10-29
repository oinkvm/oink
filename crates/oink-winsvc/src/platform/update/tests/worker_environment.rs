use super::*;
use std::{collections::BTreeMap, ffi::OsString, os::windows::ffi::OsStringExt};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};

fn entries(block: &[u16]) -> BTreeMap<OsString, OsString> {
    block
        .split(|c| *c == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let separator = entry.iter().position(|c| *c == u16::from(b'=')).unwrap();
            (
                OsString::from_wide(&entry[..separator]),
                OsString::from_wide(&entry[separator + 1..]),
            )
        })
        .collect()
}

#[test]
fn worker_environment_rebuilds_concrete_user_known_folders() {
    const CHILD: &str = "OINK_TEST_UPDATE_ENVIRONMENT_CHILD";
    if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new("user")) {
        use windows_sys::Win32::{System::Com::*, UI::Shell::*};
        let mut raw = ptr::null_mut();
        // SAFETY: initialize COM on this isolated test thread; query only.
        assert!(unsafe { CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32) } >= 0);
        // SAFETY: fixed known-folder ID, current user and writable path output.
        let result = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, 0, ptr::null_mut(), &mut raw) };
        let path = if raw.is_null() {
            None
        } else {
            // SAFETY: shell owns a NUL-terminated UTF-16 allocation until freed.
            unsafe {
                let mut length = 0;
                while *raw.add(length) != 0 {
                    length += 1;
                }
                let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(raw, length)));
                CoTaskMemFree(raw.cast());
                Some(path)
            }
        };
        // SAFETY: balances this thread's successful initialization.
        unsafe { CoUninitialize() };
        assert!(result >= 0, "SHGetKnownFolderPath: 0x{result:08x}");
        let path = path.unwrap();
        assert!(path.is_absolute());
        assert!(
            !path.to_string_lossy().contains('%'),
            "unexpanded known folder: {path:?}"
        );
        return;
    }
    if std::env::var_os(CHILD).is_some() {
        // The update worker rebuilds an already logged-on user's environment
        // with bInherit=FALSE. Query only: no desktop or installed app is started.
        let mut primary = ptr::null_mut();
        // SAFETY: own process token, read-only query/duplicate access.
        check(unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                windows_sys::Win32::Security::TOKEN_QUERY | windows_sys::Win32::Security::TOKEN_DUPLICATE,
                &mut primary,
            )
        })
        .unwrap();
        let primary = Handle::new(primary).unwrap();
        let mut environment = ptr::null_mut();
        // SAFETY: owned token and writable allocation output.
        check(unsafe { CreateEnvironmentBlock(&mut environment, primary.0, 0) }).unwrap();
        // SAFETY: userenv owns a double-NUL-terminated UTF-16 allocation.
        let values = unsafe {
            let raw = environment.cast::<u16>();
            let mut length = 0;
            while !(*raw.add(length) == 0 && *raw.add(length + 1) == 0) {
                length += 1;
            }
            let values = entries(std::slice::from_raw_parts(raw, length + 2));
            DestroyEnvironmentBlock(environment);
            values
        };
        for key in ["SystemDrive", "ProgramData", "ALLUSERSPROFILE", "USERPROFILE"] {
            let value = values
                .get(std::ffi::OsStr::new(key))
                .unwrap_or_else(|| panic!("missing {key}"));
            assert!(!value.is_empty(), "empty {key}");
            assert!(!value.to_string_lossy().contains('%'), "unexpanded {key}: {value:?}");
        }
        // Use a new process so the shell cannot reuse a known-folder cache from
        // before the rebuilt environment was applied. Never mutate test globals.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::update::worker::environment_tests::worker_environment_rebuilds_concrete_user_known_folders",
                "--nocapture",
            ])
            .env_clear()
            .envs(values)
            .env(CHILD, "user")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "platform::update::worker::environment_tests::worker_environment_rebuilds_concrete_user_known_folders",
            "--nocapture",
        ])
        .env_clear()
        .envs(entries(&environment().unwrap()))
        .env(CHILD, "1")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
