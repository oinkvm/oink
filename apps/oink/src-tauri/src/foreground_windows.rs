//! Foreground permission stays with the actual user-input recipient.
use oink_agent::api::{Hotkey, HotkeyAction};
use std::{
    cell::{Cell, RefCell},
    path::Path,
};
use tauri::AppHandle;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey},
            WindowsAndMessaging::{
                AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, FindWindowW, GetWindowThreadProcessId,
                PostMessageW, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_HOTKEY, WNDCLASSW,
            },
        },
    },
    core::HSTRING,
};

const OPEN: u32 = oink_agent::backend::foreground::OPEN_MESSAGE;
const HOTKEY: i32 = 71;
thread_local! {
    static PENDING: Cell<bool> = const { Cell::new(false) };
    static OWNER: RefCell<Option<(HWND, Option<AppHandle>)>> = const { RefCell::new(None) };
}
fn title(path: &Path) -> HSTRING {
    HSTRING::from(oink_agent::backend::foreground::window_title(path))
}

/// Publish a real kernel-owned UI PID even when no WebView/tray exists yet.
/// A hidden top-level window is discoverable with FindWindow; a message-only
/// window is not. It is never shown or used as an activation surrogate.
pub fn publish() -> windows::core::Result<()> {
    // SAFETY: this runs once on the UI entry thread; the class/window live for
    // this process lifetime and contain no borrowed Rust pointer.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = HSTRING::from(oink_agent::backend::foreground::WINDOW_CLASS);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: windows::core::PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &class,
            &title(&oink_foundation::paths::data_dir()),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        OWNER.with(|owner| *owner.borrow_mut() = Some((hwnd, None)));
    }
    Ok(())
}

pub fn install(app: &AppHandle) {
    OWNER.with(|owner| {
        if let Some((_, handle)) = owner.borrow_mut().as_mut() {
            *handle = Some(app.clone());
        }
    });
    if PENDING.with(|pending| pending.replace(false)) {
        crate::app::show_main(app);
    }
}

pub fn handoff(path: &Path) -> windows::core::Result<()> {
    // A manually launched second process holds launch-time foreground rights.
    // Grant to the UI's actual HWND owner, never the input backend or ASFW_ANY,
    // BEFORE posting: an IPC/posted message cannot itself transfer those rights.
    // This also works before any WebView exists. Documented Windows 10/11 API;
    // interactive launch/remote-input verification is pending.
    // SAFETY: FindWindow returns a desktop HWND; no borrowed pointer is retained.
    unsafe {
        let hwnd = FindWindowW(
            &HSTRING::from(oink_agent::backend::foreground::WINDOW_CLASS),
            &title(path),
        )?;
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return Err(windows::core::Error::from_thread());
        }
        crate::shell_policy::handoff(
            || AllowSetForegroundWindow(pid),
            || PostMessageW(Some(hwnd), OPEN, WPARAM(0), LPARAM(0)),
        )
    }
}

pub fn configure(app: &AppHandle, hotkeys: &[Hotkey]) {
    let chord = hotkeys
        .iter()
        .find(|h| h.action == HotkeyAction::OpenWindow && h.enabled)
        .and_then(|h| crate::windows_hotkey::chord(&h.keys));
    crate::app::on_main(app, move |_| {
        OWNER.with(|owner| {
            if let Some((hwnd, _)) = owner.borrow().as_ref() {
                // Registration and delivery belong to the UI event-loop thread.
                // The LL hook supplies no activation right; only real WM_HOTKEY
                // delivery does. Never post a synthetic WM_HOTKEY ourselves.
                // SAFETY: the live HWND belongs to this UI event-loop thread.
                unsafe {
                    let _ = UnregisterHotKey(Some(*hwnd), HOTKEY);
                    if let Some((mods, key)) = chord
                        && let Err(error) =
                            RegisterHotKey(Some(*hwnd), HOTKEY, HOT_KEY_MODIFIERS(mods) | MOD_NOREPEAT, key)
                    {
                        tracing::warn!(%error, "could not register open-window hotkey");
                    }
                }
            }
        });
    });
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if message == OPEN || (message == WM_HOTKEY && wp.0 == HOTKEY as usize) {
        let app = OWNER.with(|owner| owner.borrow().as_ref().and_then(|(_, app)| app.clone()));
        if let Some(app) = app {
            crate::app::show_main(&app);
        } else {
            // Tao may pump native messages before setup attaches its AppHandle.
            // Retain an early granted open instead of dropping a startup click.
            PENDING.with(|pending| pending.set(true));
        }
        return LRESULT(0);
    }
    // SAFETY: forward the original callback arguments to the system procedure.
    unsafe { DefWindowProcW(hwnd, message, wp, lp) }
}

/// Keep the installer Finish helper's grant alive across its argument-free UI
/// launch. Suspend before granting so a fast second instance cannot notify the
/// running UI first. No synthetic input and no elevated-token launch path.
pub fn launch_user_ui(exe: &Path) -> windows::core::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
                CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW, TerminateProcess,
            },
        },
        core::PWSTR,
    };
    let path = HSTRING::from(exe.as_os_str());
    let directory = HSTRING::from(exe.parent().unwrap_or(Path::new(".")).as_os_str());
    let mut command: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain(Some(0)).collect();
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    // Explicit Finish launch must not inherit a login marker from an updater
    // originally started at login. Build the child's environment without
    // mutating this process's environment while library threads may exist.
    let mut environment: Vec<_> = std::env::vars_os()
        .filter(|(name, _)| {
            !name
                .to_string_lossy()
                .eq_ignore_ascii_case(oink_autostart::LOGIN_LAUNCH_ENV)
        })
        .collect();
    environment.sort_by_cached_key(|(name, _)| name.to_string_lossy().to_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (name, value) in environment {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.extend([0, 0]);
    // Preserve detached-launch job handling: break away only when the current
    // job permits it; silent breakaway needs no explicit creation flag.
    use windows::Win32::System::JobObjects::{JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK};
    let limits = oink_agent::backend::launch::lifetime().job_limit_flags.unwrap_or(0);
    let mut flags = CREATE_SUSPENDED | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT;
    if limits & JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK.0 == 0 && limits & JOB_OBJECT_LIMIT_BREAKAWAY_OK.0 != 0 {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: command is writable, NUL-terminated storage; all other strings and
    // output structures and the Unicode environment live through CreateProcessW.
    // Handles close exactly once.
    unsafe {
        CreateProcessW(
            &path,
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            flags,
            Some(block.as_ptr().cast()),
            &directory,
            &startup,
            &mut process,
        )?;
        let result = crate::installer_finish::resume_user_ui(
            || {
                let grant = AllowSetForegroundWindow(process.dwProcessId);
                if let Err(error) = &grant {
                    tracing::info!(%error, "installer UI foreground grant denied; continuing normal-user launch");
                }
                grant
            },
            || {
                if ResumeThread(process.hThread) == u32::MAX {
                    Err(windows::core::Error::from_thread())
                } else {
                    Ok(())
                }
            },
        );
        if result.is_err() {
            let _ = TerminateProcess(process.hProcess, 1);
        }
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
        result
    }
}
