#pragma once
#include <windows.h>
#include <cstdlib>
#include <cwchar>
#include <string>

// Turns the volatile microphone restart marker into Windows Installer's own
// restart prompt at the end of an interactive full uninstall.
namespace microphone_restart {
// The execute sequence sees UILevel 2 for /qn (also the updater's
// "/qn /norestart") and 3 once the client shows any UI: msiexec /x as started
// from Settings or Control Panel runs with basic UI, the same level /qb has.
inline constexpr int interactive_ui_level = 3;

// Prompt only when a microphone restart is owed and the run will ask the user.
// Without UI Windows Installer would restart automatically, and REBOOTPROMPT=S
// (/passive) restarts without asking, so both keep REBOOT=ReallySuppress.
inline bool prompt(const std::wstring& ui_level, const std::wstring& reboot_prompt, bool marker) {
    return marker && _wtoi(ui_level.c_str()) >= interactive_ui_level && reboot_prompt.empty();
}

// Written by the microphone cleanup (Microphone.ps1 RecordOperation) only
// when its driver removal needs a restart, or kept from an earlier operation
// whose restart is still owed. Windows discards it at boot.
inline LONG marker(bool& present) {
    HKEY key = nullptr;
    const LONG result = RegOpenKeyExW(HKEY_LOCAL_MACHINE, L"SOFTWARE\\Oink\\MicrophoneRestartRequired", 0,
                                      KEY_QUERY_VALUE | KEY_WOW64_64KEY, &key);
    present = result == ERROR_SUCCESS;
    if (present) RegCloseKey(key);
    return result == ERROR_FILE_NOT_FOUND ? ERROR_SUCCESS : result;
}
}
