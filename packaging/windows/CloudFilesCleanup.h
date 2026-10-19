#pragma once
#include <windows.h>
#include <sddl.h>
#include <string>
#include <vector>

namespace cloud_cleanup {
struct Handles {
    std::vector<HANDLE> values;
    ~Handles() { for (auto handle : values) CloseHandle(handle); }
};
inline bool missing(DWORD error) { return error == ERROR_FILE_NOT_FOUND || error == ERROR_PATH_NOT_FOUND; }
inline bool cloud_tag(DWORD tag) { return (tag & ~0x0000f000u) == 0x9000001au; }
inline bool info(HANDLE handle, FILE_ATTRIBUTE_TAG_INFO& attributes) {
    return GetFileInformationByHandleEx(handle, FileAttributeTagInfo, &attributes, sizeof(attributes)) != FALSE;
}
inline bool erase(HANDLE handle) {
    FILE_DISPOSITION_INFO disposition{TRUE};
    return SetFileInformationByHandle(handle, FileDispositionInfo, &disposition, sizeof(disposition)) != FALSE;
}
inline bool children(const std::wstring& path) {
    WIN32_FIND_DATAW entry{};
    HANDLE search = FindFirstFileW((path + L"\\*").c_str(), &entry);
    if (search == INVALID_HANDLE_VALUE) return GetLastError() == ERROR_FILE_NOT_FOUND;
    bool ok = true;
    do {
        if (wcscmp(entry.cFileName, L".") == 0 || wcscmp(entry.cFileName, L"..") == 0) continue;
        const auto child = path + L"\\" + entry.cFileName;
        // OPEN_REPARSE_POINT never opens the target of a junction/symlink. Deny
        // delete sharing while using a directory's path, so it cannot be renamed
        // or substituted between enumeration and opening its children.
        HANDLE handle = CreateFileW(child.c_str(), DELETE | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr);
        if (handle == INVALID_HANDLE_VALUE) { if (!missing(GetLastError())) ok = false; continue; }
        FILE_ATTRIBUTE_TAG_INFO attributes{};
        bool removed = info(handle, attributes);
        if (removed && (attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) &&
            !(attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)) removed = children(child);
        // Reparse entries are deleted themselves; recursion never follows them.
        if (removed) removed = erase(handle);
        CloseHandle(handle);
        ok = removed && ok;
    } while (FindNextFileW(search, &entry));
    const DWORD error = GetLastError();
    FindClose(search);
    return ok && error == ERROR_NO_MORE_FILES;
}
using Unregister = HRESULT(WINAPI*)(LPCWSTR);
inline HRESULT WINAPI unregister_root(LPCWSTR path) {
    HMODULE library = LoadLibraryExW(L"cldapi.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!library) return HRESULT_FROM_WIN32(GetLastError());
    auto unregister = reinterpret_cast<Unregister>(GetProcAddress(library, "CfUnregisterSyncRoot"));
    HRESULT result = unregister ? unregister(path) : HRESULT_FROM_WIN32(ERROR_PROC_NOT_FOUND);
    FreeLibrary(library);
    return result;
}
inline bool root(const std::wstring& path, Unregister unregister = unregister_root) {
    if (path.size() < 4 || path[1] != L':' || path[2] != L'\\') return false;
    const DWORD present = GetFileAttributesW(path.c_str());
    if (present == INVALID_FILE_ATTRIBUTES) return missing(GetLastError());
    Handles pins;
    // ProfileList is machine-owned, but all descendants are user writable.
    // Pin every ancestor without delete sharing, rejecting all ancestor reparse
    // points. A lexical prefix check alone would allow SYSTEM traversal through
    // a user-controlled junction. The final root may carry only a cloud tag.
    for (size_t end = path.find(L'\\', 3);; end = path.find(L'\\', end + 1)) {
        const bool last = end == std::wstring::npos;
        const auto part = last ? path : path.substr(0, end);
        HANDLE handle = CreateFileW(part.c_str(), FILE_READ_ATTRIBUTES | (last ? DELETE : 0),
            FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr);
        if (handle == INVALID_HANDLE_VALUE) return missing(GetLastError());
        pins.values.push_back(handle);
        FILE_ATTRIBUTE_TAG_INFO attributes{};
        if (!info(handle, attributes) || !(attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY) ||
            ((attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) && (!last || !cloud_tag(attributes.ReparseTag)))) return false;
        if (last) break;
    }
    // The connected backend must already be stopped by MSI's quiescence gate.
    // This call is valid as SYSTEM for a per-user raw sync root (probed on
    // Windows 10 22H2 and Windows 11). It deletes unhydrated placeholders and
    // reverts hydrated ones; the handle-based pass removes the leftovers.
    HRESULT result = unregister(path.c_str());
    if (FAILED(result) && result != HRESULT_FROM_WIN32(ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT) &&
        result != HRESULT_FROM_WIN32(ERROR_NOT_A_CLOUD_SYNC_ROOT) && result != HRESULT_FROM_WIN32(ERROR_NOT_A_CLOUD_FILE)) return false;
    return children(path) && erase(pins.values.back());
}
inline bool profiles(const wchar_t* data_directory) {
    HKEY profiles = nullptr;
    if (RegOpenKeyExW(HKEY_LOCAL_MACHINE, L"SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\ProfileList", 0,
        KEY_READ | KEY_WOW64_64KEY, &profiles) != ERROR_SUCCESS) return false;
    bool ok = true;
    for (DWORD index = 0;; ++index) {
        wchar_t sid[256]; DWORD sid_size = static_cast<DWORD>(std::size(sid));
        LONG result = RegEnumKeyExW(profiles, index, sid, &sid_size, nullptr, nullptr, nullptr, nullptr);
        if (result == ERROR_NO_MORE_ITEMS) break;
        if (result != ERROR_SUCCESS) { ok = false; continue; }
        PSID parsed = nullptr;
        if (!ConvertStringSidToSidW(sid, &parsed)) continue; // e.g. backup keys
        const bool service = IsWellKnownSid(parsed, WinLocalSystemSid) ||
            IsWellKnownSid(parsed, WinLocalServiceSid) || IsWellKnownSid(parsed, WinNetworkServiceSid);
        LocalFree(parsed);
        if (service) continue; // local, domain and cloud-directory users are included
        wchar_t image[32768]; DWORD bytes = sizeof(image), type = 0;
        HKEY profile = nullptr;
        if (RegOpenKeyExW(profiles, sid, 0, KEY_QUERY_VALUE, &profile) != ERROR_SUCCESS) { ok = false; continue; }
        result = RegQueryValueExW(profile, L"ProfileImagePath", nullptr, &type, reinterpret_cast<BYTE*>(image), &bytes);
        RegCloseKey(profile);
        if (result != ERROR_SUCCESS || (type != REG_SZ && type != REG_EXPAND_SZ) || bytes < sizeof(wchar_t) || bytes % sizeof(wchar_t) != 0 ||
            image[bytes / sizeof(wchar_t) - 1] != 0) { ok = false; continue; }
        wchar_t expanded[32768]; DWORD length = ExpandEnvironmentStringsW(image, expanded, static_cast<DWORD>(std::size(expanded)));
        if (!length || length > std::size(expanded)) { ok = false; continue; }
        // Default per-user local storage only. OINK_HOME developer overrides are
        // deliberately outside MSI ownership; identity/settings are never removed.
        const auto path = std::wstring(expanded) + L"\\AppData\\Local\\" + data_directory + L"\\Cache\\CloudFiles";
        ok = root(path) && ok;
    }
    RegCloseKey(profiles);
    return ok;
}
}
