//! Local shell integration; clipboard selection/conversion lives in oink-clipboard.
use crate::config::{Settings, UiLanguage};
use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
};

pub fn language(value: UiLanguage) -> &'static str {
    #[cfg(target_os = "macos")]
    let system = objc2_foundation::NSLocale::preferredLanguages()
        .firstObject()
        .map(|s| s.to_string())
        .unwrap_or_default();
    #[cfg(windows)]
    fn preferred_language() -> String {
        use windows::{
            Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME},
            core::PWSTR,
        };
        let (mut count, mut length) = (0, 0);
        // SAFETY: sizing query writes only count/length and does not access a buffer.
        unsafe {
            if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, None, &mut length).is_err() {
                return String::new();
            }
        }
        let mut buffer = vec![0u16; length as usize];
        // SAFETY: UTF-16 output buffer has the capacity reported by the sizing call.
        unsafe {
            if GetUserPreferredUILanguages(
                MUI_LANGUAGE_NAME,
                &mut count,
                Some(PWSTR(buffer.as_mut_ptr())),
                &mut length,
            )
            .is_err()
            {
                return String::new();
            }
        }
        String::from_utf16_lossy(buffer.split(|ch| *ch == 0).next().unwrap_or(&[]))
    }
    #[cfg(windows)]
    let system = preferred_language();
    match value.resolve(&system) {
        UiLanguage::SimplifiedChinese => "zh-Hans",
        UiLanguage::Japanese => "ja",
        UiLanguage::Korean => "ko",
        _ => "en",
    }
}
pub fn title(language: &str) -> &'static str {
    match language {
        "zh-Hans" => "粘贴为文件",
        "ja" => "ファイルとして貼り付け",
        "ko" => "파일로 붙여넣기",
        _ => "Paste as File",
    }
}
fn checked(command: &mut Command) -> io::Result<std::process::Output> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.output()?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(io::Error::other(String::from_utf8_lossy(&output.stderr).into_owned()))
    }
}
/// Only a packaged backend registers shell hooks. Developer/isolated CLI runs
/// may invoke save directly and never replace the installed app's registration.
pub(crate) fn apply(settings: &Settings) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    #[cfg(target_os = "macos")]
    {
        let Some(mac) = exe.parent() else { return Ok(()) };
        let Some(contents) = mac.parent() else { return Ok(()) };
        let extension = contents.join("PlugIns/OinkFinderSync.appex");
        if !extension.is_dir() {
            return Ok(());
        }
        // A direct bundle launch may precede LaunchServices discovery: the
        // independent probes found that pluginkit -a alone then reports no
        // matches (macOS 26.6.2 / 14.7.8). Register only this containing app
        // before electing its extension; this never changes another app's vote.
        let app = contents.parent().ok_or(io::ErrorKind::InvalidInput)?;
        checked(Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister").arg("-f").arg(app))?;
        let output = checked(Command::new(mac.join("oink-file-provider")).arg("container"))?;
        // `container` returns AppGroup/FileProvider, whereas Finder Sync reads
        // preferences from the App Group itself. Never publish settings as a
        // catalog: old extensions enumerate every JSON in FileProvider.
        let catalog_root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        let root = catalog_root.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let value = serde_json::json!({"enabled":settings.paste_as_file,"language":language(settings.ui_language)});
        oink_foundation::storage::atomic_write(&root.join("paste-as-file.json"), &serde_json::to_vec(&value)?)?;
        checked(Command::new("/usr/bin/pluginkit").arg("-a").arg(extension))?;
        // Finder Sync is an explicit PlugInKit election. Registration alone is
        // insufficient; elect this exact extension without opening System Settings.
        // The independent signed probe checks actual host startup, not only '+'.
        checked(Command::new("/usr/bin/pluginkit").args([
            "-e",
            if settings.paste_as_file { "use" } else { "ignore" },
            "-i",
            "com.hachiwii.oink.findersync",
        ]))?;
    }
    #[cfg(windows)]
    {
        let agent = exe.with_file_name("oink.exe");
        // MSI supplies the helper beside the backend; avoid registering a build
        // tree as the user's production verb when it is run with isolated data.
        if !agent.exists() || !exe.with_file_name("installed.json").is_file() {
            return Ok(());
        }
        register_windows(
            &agent,
            "OinkPasteAsFile",
            settings.paste_as_file,
            language(settings.ui_language),
        )?;
    }
    Ok(())
}

#[cfg(any(windows, test))]
// Background menus have zero selected items. MultiSelectModel=Single suppresses
// the verb before IExplorerCommandState runs (Windows 11 26200); only item verbs
// should use that filter. Directory and ordinary-file selections stay singular.
const WINDOWS_VERBS: &[(&str, &str, bool)] = &[
    ("Directory\\Background", "%V", false),
    ("Directory", "%1", true),
    ("*", "%1", true),
];

#[cfg(any(windows, test))]
fn paste_menu_icon_value(dll: &Path, dark: bool) -> String {
    // The state DLL contains both VI-approved marks. Explorer does not
    // re-evaluate an Icon registry value when the theme changes. The caller
    // chooses the current system menu appearance and sends the normal
    // association-change notification below after re-registering the verb.
    let resource = if dark { 102 } else { 101 };
    format!(r#""{}",-{}"#, dll.display(), resource)
}

#[cfg(any(windows, test))]
fn shell_destination(target: &Path) -> io::Result<PathBuf> {
    let metadata = target.metadata()?;
    if metadata.is_dir() {
        Ok(target.to_path_buf())
    } else if metadata.is_file() {
        target
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| io::ErrorKind::InvalidInput.into())
    } else {
        Err(io::ErrorKind::InvalidInput.into())
    }
}

#[cfg(windows)]
pub fn register_windows(exe: &Path, verb: &str, enabled: bool, language: &str) -> io::Result<()> {
    register_windows_with_theme(exe, verb, enabled, language, crate::appearance::taskbar_dark())
}

#[cfg(windows)]
fn register_windows_with_theme(exe: &Path, verb: &str, enabled: bool, language: &str, dark: bool) -> io::Result<()> {
    const CLSID: &str = "{87D83265-289B-446D-A5F9-E4B9DA83A875}";
    let class_key = format!("HKCU\\Software\\Classes\\CLSID\\{CLSID}");
    let dll = exe.with_file_name("OinkPasteMenu.dll");
    if enabled {
        if !dll.is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "OinkPasteMenu.dll missing"));
        }
        checked(Command::new("reg.exe").args([
            "add",
            &format!("{class_key}\\InprocServer32"),
            "/ve",
            "/t",
            "REG_SZ",
            "/d",
            &dll.to_string_lossy(),
            "/f",
        ]))?;
        checked(Command::new("reg.exe").args([
            "add",
            &format!("{class_key}\\InprocServer32"),
            "/v",
            "ThreadingModel",
            "/t",
            "REG_SZ",
            "/d",
            "Apartment",
            "/f",
        ]))?;
    }
    for &(class, argument, single_selection) in WINDOWS_VERBS {
        let key = format!("HKCU\\Software\\Classes\\{class}\\shell\\{verb}");
        // Recreate our own verb so an existing installation loses obsolete
        // values, including the background's former MultiSelectModel=Single.
        delete_key(&key)?;
        if !enabled {
            continue;
        }
        checked(Command::new("reg.exe").args(["add", &key, "/ve", "/t", "REG_SZ", "/d", title(language), "/f"]))?;
        if single_selection {
            checked(Command::new("reg.exe").args([
                "add",
                &key,
                "/v",
                "MultiSelectModel",
                "/t",
                "REG_SZ",
                "/d",
                "Single",
                "/f",
            ]))?;
        }
        checked(Command::new("reg.exe").args([
            "add",
            &key,
            "/v",
            "Icon",
            "/t",
            "REG_SZ",
            "/d",
            &paste_menu_icon_value(&dll, dark),
            "/f",
        ]))?;
        checked(Command::new("reg.exe").args([
            "add",
            &key,
            "/v",
            "CommandStateHandler",
            "/t",
            "REG_SZ",
            "/d",
            CLSID,
            "/f",
        ]))?;
        // A folder can end in '\\'; append '\\.' so it cannot escape the closing
        // argv quote. Ordinary-file paths must not acquire that directory suffix.
        let suffix = if class == "*" { "" } else { "\\." };
        let command = format!("\"{}\" --paste-as-file \"{argument}{suffix}\"", exe.display());
        checked(Command::new("reg.exe").args([
            "add",
            &format!("{key}\\command"),
            "/ve",
            "/t",
            "REG_SZ",
            "/d",
            &command,
            "/f",
        ]))?;
    }
    if !enabled {
        delete_key(&class_key)?;
    }
    // Invalidate Explorer's association cache after the verb/state registration
    // changes. The user never needs to restart Explorer to apply this switch.
    use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
    // SAFETY: association-change notification has no item pointers.
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
    }
    Ok(())
}

/// Re-register only the shell icon after Windows changes its system-menu
/// appearance. Explorer keeps a classic verb's `Icon` value static, so the
/// normal preference apply path is insufficient for a light/dark transition.
#[cfg(windows)]
pub fn refresh_windows_icon(dark: bool) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let agent = exe.with_file_name("oink.exe");
    if !agent.exists() || !exe.with_file_name("installed.json").is_file() {
        return Ok(());
    }
    let settings = crate::config::AgentConfig::load()
        .map_err(|error| io::Error::other(error.to_string()))?
        .settings;
    register_windows_with_theme(
        &agent,
        "OinkPasteAsFile",
        settings.paste_as_file,
        language(settings.ui_language),
        dark,
    )
}

#[cfg(windows)]
fn delete_key(key: &str) -> io::Result<()> {
    use windows::{
        Win32::{
            Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS},
            System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW},
        },
        core::PCWSTR,
    };
    let path: Vec<u16> = key
        .strip_prefix("HKCU\\")
        .ok_or(io::ErrorKind::InvalidInput)?
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // SAFETY: current-user root and a terminated fixed product/test subkey path.
    let result = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
    if [ERROR_SUCCESS, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND].contains(&result) {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result.0 as i32))
    }
}

pub fn save(destination: &Path, settings: &Settings) -> anyhow::Result<Vec<PathBuf>> {
    if !settings.paste_as_file {
        return Ok(vec![]);
    }
    #[cfg(windows)]
    {
        use windows::Win32::{
            Foundation::{ERROR_SUCCESS, GetLastError, SetLastError},
            System::DataExchange::CountClipboardFormats,
        };
        // EmptyClipboard clears native formats before a new OLE object is
        // published. An empty click is a no-op even in that window; querying
        // metadata must not render a delayed owner or turn it into an error.
        // SAFETY: metadata-only Win32 query; preserve errors distinct from zero formats.
        unsafe {
            SetLastError(ERROR_SUCCESS);
            if CountClipboardFormats() == 0 && GetLastError() == ERROR_SUCCESS {
                return Ok(vec![]);
            }
        }
    }
    let backend = oink_clipboard::platform_backend()?;
    // This is a consumer of the current local clipboard (including Oink-written
    // delayed formats and file placeholders), never a new transport or offer.
    let data = oink_foundation::paths::data_dir();
    let captured = backend.capture(&data)?;
    let provider = captured
        .content
        .ok_or_else(|| io::Error::other("native clipboard provider unavailable"))?;
    if oink_clipboard::paste_file::select(&provider.inventory()).is_none() {
        return Ok(vec![]);
    }
    let now = time::OffsetDateTime::now_local()?;
    let timestamp = now.format(time::macros::format_description!(
        "[year]-[month]-[day] [hour].[minute].[second]"
    ))?;
    Ok(oink_clipboard::paste_file::save(
        provider.as_ref(),
        destination,
        language(settings.ui_language),
        &timestamp,
        cfg!(windows),
    )?)
}

/// GUI-subsystem entry used by Explorer, before starting Tauri or the backend.
#[cfg(windows)]
pub fn dispatch_shell_args() -> Option<std::process::ExitCode> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_none_or(|s| s != "--paste-as-file") {
        return None;
    }
    let result = (|| -> anyhow::Result<()> {
        anyhow::ensure!(args.len() == 2, "expected one destination folder");
        let config = crate::config::AgentConfig::load()?;
        save(&shell_destination(Path::new(&args[1]))?, &config.settings)?;
        Ok(())
    })();
    Some(match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            use windows::{
                Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
                core::PCWSTR,
            };
            let message: Vec<u16> = format!("{error:#}").encode_utf16().chain(Some(0)).collect();
            let title: Vec<u16> = "Oink".encode_utf16().chain(Some(0)).collect();
            // SAFETY: both UTF-16 strings are terminated and live for the modal call.
            unsafe {
                MessageBoxW(
                    None,
                    PCWSTR(message.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_OK | MB_ICONERROR,
                );
            }
            std::process::ExitCode::FAILURE
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_verb_accepts_zero_selected_items() {
        let background = WINDOWS_VERBS
            .iter()
            .find(|(class, _, _)| *class == "Directory\\Background")
            .unwrap();
        assert!(
            !background.2,
            "Single suppresses the background menu's zero-item selection"
        );
    }

    #[test]
    fn file_verb_is_single_selection() {
        assert!(
            WINDOWS_VERBS
                .iter()
                .any(|&(class, argument, single)| class == "*" && argument == "%1" && single)
        );
    }

    #[test]
    fn shell_file_and_folder_destinations() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("selected.txt");
        std::fs::write(&file, b"selected").unwrap();
        assert_eq!(shell_destination(root.path()).unwrap(), root.path());
        assert_eq!(shell_destination(&file).unwrap(), root.path());
        assert!(shell_destination(&root.path().join("missing")).is_err());
    }

    #[test]
    fn shell_icon_uses_the_embedded_light_or_dark_mark() {
        let dll = Path::new(r"C:\Program Files\Oink\OinkPasteMenu.dll");
        assert_eq!(
            paste_menu_icon_value(dll, false),
            r#""C:\Program Files\Oink\OinkPasteMenu.dll",-101"#
        );
        assert_eq!(
            paste_menu_icon_value(dll, true),
            r#""C:\Program Files\Oink\OinkPasteMenu.dll",-102"#
        );
    }
}
