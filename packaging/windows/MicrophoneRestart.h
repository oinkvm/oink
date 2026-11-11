#pragma once
#include <windows.h>
#include <cstdlib>
#include <cwchar>
#include <string>

// Turns the volatile microphone restart marker into Windows Installer's own
// restart prompt at the end of an interactive full uninstall.
namespace microphone_restart {
// Written by Microphone.ps1 (RecordOperation) only while a driver change owes
// a restart; Windows discards it at boot.
inline constexpr wchar_t marker_key[] = L"SOFTWARE\\Oink\\MicrophoneRestartRequired";
// The later CleanupUserData commit action deletes the whole HKLM\Software\Oink
// tree, marker included, before any immediate action can run again. The
// microphone cleanup therefore carries an owed restart into this separate
// volatile key, which the post-commit check reads and removes. If that check
// never runs (for example a failed commit) it still disappears at boot.
inline constexpr wchar_t uninstall_signal_key[] = L"SOFTWARE\\OinkUninstallRestartRequired";

// The execute sequence sees UILevel 2 for /qn (also the updater's
// "/qn /norestart") and 3 once the client shows any UI: msiexec /x as started
// from Settings or Control Panel runs with basic UI, the same level /qb has.
inline constexpr int interactive_ui_level = 3;

// Prompt only when a microphone restart is owed and the run will ask the user.
// Without UI Windows Installer would restart automatically, and REBOOTPROMPT=S
// (/passive) restarts without asking, so both keep REBOOT=ReallySuppress.
inline bool prompt(const std::wstring& ui_level, const std::wstring& reboot_prompt, bool owed) {
    return owed && _wtoi(ui_level.c_str()) >= interactive_ui_level && reboot_prompt.empty();
}

inline LONG exists(HKEY root, const wchar_t* path, bool& present) {
    HKEY key = nullptr;
    const LONG result = RegOpenKeyExW(root, path, 0, KEY_QUERY_VALUE | KEY_WOW64_64KEY, &key);
    present = result == ERROR_SUCCESS;
    if (present) RegCloseKey(key);
    return result == ERROR_FILE_NOT_FOUND ? ERROR_SUCCESS : result;
}

// Commit-time, before user data cleanup: keep an owed restart for the check.
inline LONG carry(HKEY root, const wchar_t* marker, const wchar_t* signal, bool& owed) {
    const LONG read = exists(root, marker, owed);
    if (read != ERROR_SUCCESS || !owed) return read;
    HKEY key = nullptr;
    const LONG created = RegCreateKeyExW(root, signal, 0, nullptr, REG_OPTION_VOLATILE,
                                         KEY_WRITE | KEY_WOW64_64KEY, nullptr, &key, nullptr);
    if (created == ERROR_SUCCESS) RegCloseKey(key);
    return created;
}

// Post-commit: read the carried signal and remove it. Removal is best effort;
// the key is volatile and only ever means "a restart is still owed".
inline LONG take(HKEY root, const wchar_t* signal, bool& owed) {
    const LONG read = exists(root, signal, owed);
    if (owed) RegDeleteKeyExW(root, signal, KEY_WOW64_64KEY, 0);
    return read;
}
}
