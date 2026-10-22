#pragma once
#include <windows.h>
#include <initializer_list>

namespace user_cleanup {
// Foundation's atomic files deliberately grant only their user owner access.
// SYSTEM uninstall therefore needs backup/restore access, even to open metadata
// and delete handles. This per-machine uninstall includes signed-out users'
// default stores and service data. Impersonating only the uninstalling user
// cannot cover those owner-only files; an immediate user action would also
// delete data before commit. Use SYSTEM's existing privileges in a duplicated
// thread token, never widen an ACL, take ownership or leave the MSI host changed.
class Privileges {
    HANDLE previous_ = nullptr;
    HANDLE token_ = nullptr;
    bool active_ = false;
    DWORD error_ = ERROR_SUCCESS;
public:
    Privileges() {
        HANDLE source = nullptr;
        if (OpenThreadToken(GetCurrentThread(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE, TRUE, &previous_)) source = previous_;
        else {
            error_ = GetLastError();
            if (error_ != ERROR_NO_TOKEN || !OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &source)) {
                error_ = GetLastError(); return;
            }
        }
        const bool duplicated = DuplicateTokenEx(source, TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES | TOKEN_IMPERSONATE,
            nullptr, SecurityImpersonation, TokenImpersonation, &token_) != FALSE;
        error_ = GetLastError();
        if (source != previous_) CloseHandle(source);
        if (!duplicated) return;
        for (const auto name : {L"SeBackupPrivilege", L"SeRestorePrivilege"}) {
            TOKEN_PRIVILEGES privileges{}; privileges.PrivilegeCount = 1;
            if (!LookupPrivilegeValueW(nullptr, name, &privileges.Privileges[0].Luid)) { error_ = GetLastError(); return; }
            privileges.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
            if (!AdjustTokenPrivileges(token_, FALSE, &privileges, 0, nullptr, nullptr)) { error_ = GetLastError(); return; }
            error_ = GetLastError();
            if (error_ != ERROR_SUCCESS) return;
        }
        active_ = SetThreadToken(nullptr, token_) != FALSE;
        error_ = active_ ? ERROR_SUCCESS : GetLastError();
    }
    Privileges(const Privileges&) = delete;
    Privileges& operator=(const Privileges&) = delete;
    ~Privileges() {
        if (active_) SetThreadToken(nullptr, previous_);
        if (token_) CloseHandle(token_);
        if (previous_) CloseHandle(previous_);
    }
    bool active() const { return active_; }
    DWORD error() const { return error_; }
};
}
