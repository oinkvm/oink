#pragma once
#include <windows.h>
#include <initializer_list>
#include <string>

namespace cleanup {
enum class Result { removed, preserved, failed };

inline bool absent(const std::wstring& path) {
    if (GetFileAttributesW(path.c_str()) != INVALID_FILE_ATTRIBUTES) return false;
    return GetLastError() == ERROR_FILE_NOT_FOUND || GetLastError() == ERROR_PATH_NOT_FOUND;
}

// Failed-health recovery uses a full /x before reinstalling the verified prior
// MSI. Admission has already authenticated its SYSTEM worker and lock. Keep
// user registrations while the protected journal exists, even if that worker
// dies and recovery must resume. Access failures also preserve user state.
inline bool preserve_user_state(const std::wstring& directory) {
    return !absent(directory + L"\\pending.json");
}

template<class Validate>
bool remove_contents(const std::wstring& path, const std::wstring& skip, Validate validate) {
    WIN32_FIND_DATAW entry{};
    HANDLE search = FindFirstFileW((path + L"\\*").c_str(), &entry);
    if (search == INVALID_HANDLE_VALUE) return GetLastError() == ERROR_FILE_NOT_FOUND;
    bool ok = true;
    do {
        if (wcscmp(entry.cFileName, L".") == 0 || wcscmp(entry.cFileName, L"..") == 0 ||
            (!skip.empty() && _wcsicmp(entry.cFileName, skip.c_str()) == 0)) continue;
        const auto child = path + L"\\" + entry.cFileName;
        if ((entry.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) || !validate(child)) { ok = false; break; }
        if (entry.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY)
            ok = remove_contents(child, L"", validate) && RemoveDirectoryW(child.c_str());
        else ok = DeleteFileW(child.c_str()) != FALSE;
        if (!ok) break;
    } while (FindNextFileW(search, &entry));
    if (ok && GetLastError() != ERROR_NO_MORE_FILES) ok = false;
    FindClose(search);
    return ok;
}

// Called only by the full-uninstall commit action after MSI succeeds.
// Acquire that same lock exclusively before deleting product maintenance state;
// a worker's original handle or a pending transaction must survive untouched.
template<class Validate, class ValidateLock>
Result after_uninstall(const std::wstring& root, const std::wstring& directory,
                       const std::wstring& lock_name, Validate validate, ValidateLock validate_lock) {
    if (absent(root)) return Result::removed;
    if (!validate(root)) return Result::failed;
    if (!absent(directory)) {
        if (!validate(directory)) return Result::failed;
        HANDLE lock = CreateFileW((directory + L"\\" + lock_name).c_str(), GENERIC_READ | GENERIC_WRITE | DELETE,
            0, nullptr, OPEN_ALWAYS, FILE_FLAG_OPEN_REPARSE_POINT, nullptr);
        if (lock == INVALID_HANDLE_VALUE)
            return GetLastError() == ERROR_SHARING_VIOLATION ? Result::preserved : Result::failed;
        if (!validate_lock(lock)) { CloseHandle(lock); return Result::failed; }
        for (const auto name : {L"pending.json", L"msi-authorization.json"}) {
            if (!absent(directory + L"\\" + name)) {
                CloseHandle(lock);
                return Result::preserved;
            }
        }
        bool ok = remove_contents(directory, lock_name, validate);
        FILE_DISPOSITION_INFO disposition{TRUE};
        if (ok) ok = SetFileInformationByHandle(lock, FileDispositionInfo, &disposition, sizeof(disposition)) != FALSE;
        CloseHandle(lock);
        if (!ok || !RemoveDirectoryW(directory.c_str())) return Result::failed;
    }
    // Only the known maintenance subtree is recursive. Never recurse through the
    // install root, profile directories or another product's files.
    return RemoveDirectoryW(root.c_str()) ? Result::removed : Result::failed;
}
}
