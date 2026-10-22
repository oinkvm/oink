#pragma once
#include "CloudFilesCleanup.h"
#include "CleanupPrivileges.h"
#include <shlobj.h>

namespace user_cleanup {
inline HRESULT WINAPI no_sync_root(LPCWSTR) { return HRESULT_FROM_WIN32(ERROR_NOT_A_CLOUD_SYNC_ROOT); }
inline bool tree(const std::wstring& path, const cloud_cleanup::Report& report = {}) {
    // All reads are metadata-only handles. Cloud Files must be unregistered by
    // profile() first. The existing pinned walker never follows reparse points
    // or reads placeholder contents, so deletion cannot trigger a download.
    return cloud_cleanup::root(path, no_sync_root, false, report);
}
inline bool profile(const std::wstring& home, const wchar_t* directory,
                    cloud_cleanup::Unregister unregister = cloud_cleanup::unregister_root,
                    const cloud_cleanup::Report& report = {}) {
    const auto local = home + L"\\AppData\\Local\\" + directory;
    if (!cloud_cleanup::root(local + L"\\Cache\\CloudFiles", unregister, true, report)) return false;
    bool ok = true;
    for (const auto& path : {local, home + L"\\AppData\\Roaming\\" + directory})
        ok = tree(path, report) && ok;
    return ok;
}
inline bool profiles(const wchar_t* directory, const cloud_cleanup::Report& report = {}) {
    // This is a per-machine uninstall: clean fixed default stores for local,
    // domain and cloud user profiles, including signed-out users. Do not load
    // their registry hives, follow redirected roots or touch developer overrides.
    Privileges privileges;
    if (!privileges.active()) return cloud_cleanup::failed(report, directory, L"backup/restore privileges", privileges.error());
    bool ok = cloud_cleanup::profiles([&](const std::wstring& home) { return profile(home, directory, cloud_cleanup::unregister_root, report); }, report);
    PWSTR common = nullptr;
    const auto folder = SHGetKnownFolderPath(FOLDERID_ProgramData, 0, nullptr, &common);
    if (FAILED(folder)) return cloud_cleanup::failed(report, directory, L"ProgramData HRESULT", static_cast<DWORD>(folder));
    ok = tree(std::wstring(common) + L"\\" + directory, report) && ok;
    CoTaskMemFree(common);
    // OinkInput normally stores machine state only under its protected install
    // maintenance directory (cleaned separately). Remove its default SYSTEM
    // user store too if present; no Windows/service root itself is removed.
    wchar_t windows[32768];
    const auto length = GetWindowsDirectoryW(windows, static_cast<UINT>(std::size(windows)));
    if (!length || length >= std::size(windows)) return cloud_cleanup::failed(report, directory, L"GetWindowsDirectory", GetLastError());
    ok = profile(std::wstring(windows) + L"\\System32\\config\\systemprofile", directory, cloud_cleanup::unregister_root, report) && ok;
    HKEY software = nullptr;
    const auto opened = RegOpenKeyExW(HKEY_LOCAL_MACHINE, L"Software", 0, DELETE | KEY_READ | KEY_WRITE | KEY_WOW64_64KEY, &software);
    if (opened != ERROR_SUCCESS) return cloud_cleanup::failed(report, L"HKLM/Software", L"RegOpenKeyEx", opened);
    const auto result = RegDeleteTreeW(software, directory);
    RegCloseKey(software);
    if (result != ERROR_SUCCESS && result != ERROR_FILE_NOT_FOUND) ok = cloud_cleanup::failed(report, directory, L"RegDeleteTree", result);
    return ok;
}
}
