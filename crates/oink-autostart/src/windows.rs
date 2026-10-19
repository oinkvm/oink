//! Windows：当前用户的 Run 注册表项（`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`）里名为 `Oink` 的值。
//!
//! - 登录时由资源管理器以用户的普通权限启动托盘入口；入口请求 `OinkInput` 创建用户 + UIAccess 后台。
//!   Run 值仍指向受保护安装的 `oink.exe --autostart`，服务不自行启动/监督后台。
//! - The MSI uninstall owner removes matching Oink Run entries from loaded user hives.
//! - 判断逻辑在 [`crate::run_entry`]（纯函数，带测试）；这里只读写注册表。
//!
//! Registry mutation, OS revocation and real login launch require installed-device acceptance.

use std::path::PathBuf;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_ROUTINE_FLAGS, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

use crate::{LaunchAtLoginProblem, LaunchAtLoginStatus, run_entry};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
/// 用户在系统里对各启动项的开关（只读，见 [`run_entry::approved_disabled`]）。
const APPROVED_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run");
const VALUE_NAME: PCWSTR = w!("Oink");
/// 界面程序的文件名：只有它能登记为登录项（服务进程是同一个程序加 `--service`；命令行工具不是）。
const UI_EXE: &str = "oink.exe";

fn describe(operation: &str, e: WIN32_ERROR) -> String {
    tracing::warn!(operation, native_code = e.0, "login item operation failed");
    "login_item_system_unavailable".into()
}

/// 当前程序的路径；不是界面程序时返回给用户看的原因。
fn ui_exe() -> Result<PathBuf, LaunchAtLoginProblem> {
    let exe = std::env::current_exe().map_err(|_| LaunchAtLoginProblem::SystemUnavailable)?;
    let is_ui = exe
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(UI_EXE));
    if is_ui {
        Ok(exe)
    } else {
        Err(LaunchAtLoginProblem::MainAppRequired)
    }
}

/// 读 HKCU 下的一个值；值不存在（或键不存在）返回 `None`。
fn read(key: PCWSTR, flags: REG_ROUTINE_FLAGS) -> Result<Option<Vec<u8>>, WIN32_ERROR> {
    // 先问大小再读；两次调用之间值变长时系统返回 ERROR_MORE_DATA，重来一次。
    for _ in 0..3 {
        let mut size = 0u32;
        // SAFETY: 键名与值名是以 0 结尾的常量宽字符串；只请求所需的字节数。
        let e = unsafe { RegGetValueW(HKEY_CURRENT_USER, key, VALUE_NAME, flags, None, None, Some(&mut size)) };
        if e == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if e.is_err() {
            return Err(e);
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` 有 `size` 字节可写，`size` 传入的是它的长度，返回时是实际写入的字节数。
        let e = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key,
                VALUE_NAME,
                flags,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if e == ERROR_MORE_DATA {
            continue;
        }
        if e == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if e.is_err() {
            return Err(e);
        }
        buf.truncate(size as usize);
        return Ok(Some(buf));
    }
    Err(ERROR_MORE_DATA)
}

fn run_command() -> Result<Option<String>, WIN32_ERROR> {
    Ok(read(RUN_KEY, RRF_RT_REG_SZ)?.map(|bytes| {
        let wide: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .take_while(|&c| c != 0)
            .collect();
        String::from_utf16_lossy(&wide)
    }))
}

pub(crate) fn status() -> LaunchAtLoginStatus {
    let exe = match ui_exe() {
        Ok(exe) => exe,
        Err(reason) => return LaunchAtLoginStatus::unavailable(reason),
    };
    let command = match run_command() {
        Ok(c) => c,
        Err(e) => {
            let _ = describe("read", e);
            return LaunchAtLoginStatus::unavailable(LaunchAtLoginProblem::SystemUnavailable);
        }
    };
    let approved = match read(APPROVED_KEY, RRF_RT_REG_BINARY) {
        Ok(value) => value,
        Err(error) => {
            let _ = describe("read_approval", error);
            return LaunchAtLoginStatus::unavailable(LaunchAtLoginProblem::SystemUnavailable);
        }
    };
    run_entry::evaluate(command.as_deref(), approved.as_deref(), &exe)
}

pub(crate) fn set_enabled(enabled: bool) -> Result<LaunchAtLoginStatus, String> {
    let exe = ui_exe().map_err(|problem| problem.code().to_owned())?;
    if enabled {
        let command = HSTRING::from(run_entry::command_for(&exe));
        // REG_SZ 的长度含结尾的 0，按字节计。
        let bytes = (command.len() + 1) * 2;
        // SAFETY: `command` 是以 0 结尾的宽字符串，`bytes` 是它含结尾 0 的字节数；键名与值名是常量。
        let e = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE_NAME,
                REG_SZ.0,
                Some(command.as_ptr().cast()),
                bytes as u32,
            )
        };
        if e.is_err() {
            return Err(describe("register", e));
        }
    } else {
        // SAFETY: 键名与值名是以 0 结尾的常量宽字符串。
        let e = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME) };
        // 本来就没有：已经是想要的状态。
        if e.is_err() && e != ERROR_FILE_NOT_FOUND {
            return Err(describe("unregister", e));
        }
    }
    Ok(status())
}

pub(crate) fn open_system_settings() -> Result<(), String> {
    // SAFETY: 参数都是以 0 结尾的常量宽字符串；没有父窗口。
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("ms-settings:startupapps"),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute 的约定：返回值大于 32 表示成功。
    if r.0 as usize > 32 {
        Ok(())
    } else {
        tracing::warn!(native_code = r.0 as usize, "could not open login item settings");
        Err("login_item_settings_unavailable".into())
    }
}
