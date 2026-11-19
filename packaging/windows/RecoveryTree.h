#pragma once
#include <windows.h>
#include <string>
namespace recovery {
inline bool literal_entry(const std::wstring& name) {
    // NTFS permits names created through the extended namespace which Win32
    // trims or interprets as another path. Never turn an enumerated name into
    // a parent traversal, alternate stream or alias during privileged repair.
    return !name.empty() && name != L"." && name != L".." && name.back() != L'.' && name.back() != L' ' &&
        name.find_first_of(L"\\/:") == std::wstring::npos;
}
inline bool opened_as(HANDLE handle, const std::wstring& requested) {
    wchar_t path[32768]{};
    const DWORD length = GetFinalPathNameByHandleW(handle, path, 32768, FILE_NAME_NORMALIZED);
    if (!length || length >= 32768) return false;
    std::wstring actual(path, length);
    if (actual.rfind(L"\\\\?\\", 0) == 0) actual.erase(0, 4);
    return _wcsicmp(actual.c_str(), requested.c_str()) == 0;
}
struct Boundary {
    Boundary() = default;
    Boundary(const Boundary&) = delete;
    Boundary& operator=(const Boundary&) = delete;
    HANDLE handle = INVALID_HANDLE_VALUE;
    ~Boundary() { if (handle != INVALID_HANDLE_VALUE) CloseHandle(handle); }
    bool pin(const std::wstring& path, bool deny_write = false) {
        // Windows metadata-only handles do not enforce delete sharing. The
        // native directory-rename regression fails without LIST_DIRECTORY;
        // request real directory data access to keep every ancestor pinned.
        // Product-tree pins also exclude data writers: a previously opened
        // writable directory could otherwise acquire a reparse point after
        // validation. Refuse that handle before any ACL change or traversal.
        handle = CreateFileW(path.c_str(), FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES, FILE_SHARE_READ | (deny_write ? 0 : FILE_SHARE_WRITE),
            nullptr, OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS, nullptr);
        BY_HANDLE_FILE_INFORMATION info{};
        return handle != INVALID_HANDLE_VALUE && GetFileInformationByHandle(handle, &info) &&
            (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) && !(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) &&
            opened_as(handle, path);
    }
};

// Keep every directory on the recursion stack pinned against rename while
// enumerating children. An ACL reset cannot revoke already-open user handles;
// validating then closing a directory before recursion would follow a junction
// substituted through such a handle. Exclude data writers as well as deletion,
// and re-open each child with OPEN_REPARSE_POINT.
template<class Validate>
bool discard_contents(const std::wstring& path, const std::wstring& skip, Validate validate) {
    Boundary directory;
    if (!directory.pin(path, true)) return false;
    WIN32_FIND_DATAW entry{};
    HANDLE search = FindFirstFileW((path + L"\\*").c_str(), &entry);
    if (search == INVALID_HANDLE_VALUE) return GetLastError() == ERROR_FILE_NOT_FOUND;
    bool ok = true;
    do {
        if (wcscmp(entry.cFileName, L".") == 0 || wcscmp(entry.cFileName, L"..") == 0 || entry.cFileName == skip) continue;
        if (!literal_entry(entry.cFileName)) { ok = false; break; }
        const auto child = path + L"\\" + entry.cFileName;
        if ((entry.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) || !validate(child)) { ok = false; break; }
        ok = (entry.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) ?
            discard_contents(child, L"", validate) && RemoveDirectoryW(child.c_str()) : DeleteFileW(child.c_str()) != FALSE;
        if (!ok) break;
    } while (FindNextFileW(search, &entry));
    if (ok && GetLastError() != ERROR_NO_MORE_FILES) ok = false;
    FindClose(search); return ok;
}

}
