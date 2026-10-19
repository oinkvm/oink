//! Windows 登录项的判断逻辑（纯函数，所有平台都编译测试；读写注册表在 `windows.rs`）。

use std::path::Path;

use crate::{AUTOSTART_ARG, LaunchAtLoginProblem, LaunchAtLoginState, LaunchAtLoginStatus};

/// 写进 Run 项的命令行：带引号的程序路径 + [`AUTOSTART_ARG`]。
pub(crate) fn command_for(exe: &Path) -> String {
    format!("\"{}\" {AUTOSTART_ARG}", exe.display())
}

/// 命令行里的程序路径：带引号时取引号内，否则取到第一个空白。
pub(crate) fn command_exe(command: &str) -> &str {
    let command = command.trim_start();
    match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => command.split_whitespace().next().unwrap_or(""),
    }
}

/// Windows 路径比较：不区分大小写，`/` 与 `\` 等价。
fn same_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace('/', "\\").to_lowercase();
    norm(a) == norm(b)
}

/// 用户有没有在「设置 → 应用 → 启动」或任务管理器的「启动」页里把这一项关掉。
///
/// 系统把这个开关存在 `HKCU\…\Explorer\StartupApproved\Run` 的同名二进制值里，格式没有文档：
/// 第一个字节是偶数（常见 `02`、`06`）表示启用，奇数（`03`、`07`）表示被关闭，后面是关闭时间。
/// 这里**只读**它，为的是在界面上如实显示「登录时不会启动」；Oink 从不写这个值（那是用户在系统里的选择）。
/// 没有这个值、或值为空时按启用处理（系统的默认）。
/// 格式来自公开资料与 Windows 10 / 11 任务管理器的行为，本项目尚未在真机上核对。
pub(crate) fn approved_disabled(data: Option<&[u8]>) -> bool {
    data.and_then(|d| d.first()).is_some_and(|b| b & 1 == 1)
}

/// 由注册表里读到的内容得出状态。`command`：Run 项的命令行；`approved`：StartupApproved 里的同名值。
pub(crate) fn evaluate(command: Option<&str>, approved: Option<&[u8]>, exe: &Path) -> LaunchAtLoginStatus {
    let Some(command) = command.filter(|c| !c.trim().is_empty()) else {
        return LaunchAtLoginStatus::new(LaunchAtLoginState::Disabled, None);
    };
    let target = command_exe(command);
    if !same_path(target, &exe.to_string_lossy()) {
        return LaunchAtLoginStatus::new(
            LaunchAtLoginState::Disabled,
            Some(LaunchAtLoginProblem::DifferentInstallation),
        );
    }
    let trimmed = command.trim_start();
    let args = if let Some(quoted) = trimmed.strip_prefix('"') {
        quoted.split_once('"').map_or("", |(_, args)| args.trim())
    } else {
        trimmed
            .split_once(char::is_whitespace)
            .map_or("", |(_, args)| args.trim())
    };
    if args != AUTOSTART_ARG {
        return LaunchAtLoginStatus::new(
            LaunchAtLoginState::Disabled,
            Some(LaunchAtLoginProblem::InvalidRunEntry),
        );
    }
    if approved_disabled(approved) {
        return LaunchAtLoginStatus::new(
            LaunchAtLoginState::RequiresApproval,
            Some(LaunchAtLoginProblem::ApprovalRequired),
        );
    }
    LaunchAtLoginStatus::new(LaunchAtLoginState::Enabled, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Program Files\Oink\oink.exe";

    #[test]
    fn command_round_trip() {
        let cmd = command_for(Path::new(EXE));
        assert_eq!(cmd, r#""C:\Program Files\Oink\oink.exe" --autostart"#);
        assert_eq!(command_exe(&cmd), EXE);
        assert_eq!(command_exe(r"C:\Oink\oink.exe --autostart"), r"C:\Oink\oink.exe");
        assert_eq!(command_exe("  \"C:\\a b\\x.exe\""), r"C:\a b\x.exe");
        assert_eq!(command_exe(""), "");
    }

    #[test]
    fn startup_approved_flag() {
        assert!(!approved_disabled(None));
        assert!(!approved_disabled(Some(&[])));
        assert!(!approved_disabled(Some(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])));
        assert!(!approved_disabled(Some(&[6, 0, 0, 0])));
        assert!(approved_disabled(Some(&[3, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8])));
        assert!(approved_disabled(Some(&[7, 0, 0, 0])));
    }

    #[test]
    fn states() {
        let exe = Path::new(EXE);
        let ours = command_for(exe);
        assert_eq!(evaluate(None, None, exe).state, LaunchAtLoginState::Disabled);
        assert_eq!(evaluate(Some("  "), None, exe).state, LaunchAtLoginState::Disabled);
        assert_eq!(
            evaluate(Some(&ours), None, exe),
            LaunchAtLoginStatus::new(LaunchAtLoginState::Enabled, None)
        );
        // 路径大小写、斜杠方向不同仍是同一个程序。
        let other_case = r#""c:/program files/oink/OINK.EXE" --autostart"#;
        assert_eq!(
            evaluate(Some(other_case), Some(&[2, 0, 0, 0]), exe).state,
            LaunchAtLoginState::Enabled
        );
        for arguments in ["", "--service", "--autostart --service"] {
            let command = format!("\"{EXE}\" {arguments}");
            assert_eq!(
                evaluate(Some(&command), None, exe).problem,
                Some(LaunchAtLoginProblem::InvalidRunEntry)
            );
        }
        // 用户在系统里关掉了：已登记但不会启动。
        let off = evaluate(Some(&ours), Some(&[3, 0, 0, 0]), exe);
        assert_eq!(off.state, LaunchAtLoginState::RequiresApproval);
        assert_eq!(off.problem, Some(LaunchAtLoginProblem::ApprovalRequired));
        // 指向另一处安装：对当前这一份来说是未启用，并说明指向哪里。
        let elsewhere = r#""C:\Users\someone\AppData\Local\Programs\Oink\oink.exe" --autostart"#;
        let s = evaluate(Some(elsewhere), None, exe);
        assert_eq!(s.state, LaunchAtLoginState::Disabled);
        assert_eq!(s.problem, Some(LaunchAtLoginProblem::DifferentInstallation));
    }
}
