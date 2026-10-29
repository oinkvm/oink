#include "UserDataCleanup.h"
#include <cassert>
#include <aclapi.h>
#include <filesystem>
#include <fstream>
#include <iostream>

static unsigned calls = 0;
static HRESULT WINAPI unregister_ok(LPCWSTR path) {
    ++calls;
    // Unregister must precede deleting any data, including identity/settings.
    assert(std::filesystem::exists(std::filesystem::path(path).parent_path().parent_path() / L"device.key"));
    return HRESULT_FROM_WIN32(ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT);
}
static HRESULT WINAPI unregister_denied(LPCWSTR) { ++calls; return E_ACCESSDENIED; }
// Both generations can remain registered after the linked-file cutover.
static std::vector<std::wstring> root_calls;
static std::wstring denied_root;
static HRESULT WINAPI unregister_tracked(LPCWSTR path) {
    root_calls.emplace_back(path);
    const auto local = std::filesystem::path(path).parent_path().parent_path();
    assert(std::filesystem::exists(local / L"device.key"));
    assert(std::filesystem::exists(local.parent_path().parent_path() / L"Roaming\\Oink\\settings.json"));
    return denied_root == path ? E_ACCESSDENIED : S_OK;
}
static void both_roots(const std::wstring& base) {
    const auto home = base + L"\\root-user";
    const auto local = home + L"\\AppData\\Local\\Oink";
    const auto roaming = home + L"\\AppData\\Roaming\\Oink";
    const std::vector<std::wstring> roots = {
        local + L"\\Cache\\CloudLinkedFiles", local + L"\\Cache\\CloudFiles"};
    auto create = [&](const std::vector<std::wstring>& paths) {
        for (const auto& path : paths) {
            std::filesystem::create_directories(path);
            std::ofstream{std::filesystem::path(path) / L"resident.txt"} << "private fixture";
        }
        std::filesystem::create_directories(roaming);
        std::ofstream{std::filesystem::path(local) / L"device.key"} << "private identity";
        std::ofstream{std::filesystem::path(roaming) / L"settings.json"} << "{}";
        root_calls.clear(); denied_root.clear();
    };
    for (const auto& present : {std::vector<std::wstring>{roots[0]}, std::vector<std::wstring>{roots[1]}, roots}) {
        create(present);
        assert(user_cleanup::profile(home, L"Oink", unregister_tracked));
        assert(root_calls == present); // New, old, or both, in that order.
        assert(!std::filesystem::exists(local) && !std::filesystem::exists(roaming));
    }
    // One failed unregister must not suppress the other root's attempt or erase
    // identity/settings. Every failed root reports its exact path and HRESULT.
    for (const auto& failed : roots) {
        create(roots); denied_root = failed;
        std::vector<std::wstring> failures;
        assert(!user_cleanup::profile(home, L"Oink", unregister_tracked,
            [&](const std::wstring& path, const wchar_t* operation, DWORD error) {
                assert(std::wstring(operation) == L"CfUnregisterSyncRoot HRESULT");
                assert(error == static_cast<DWORD>(E_ACCESSDENIED)); failures.push_back(path);
            }));
        assert(root_calls == roots && failures == std::vector<std::wstring>{failed});
        assert(std::filesystem::exists(failed + L"\\resident.txt"));
        assert(std::filesystem::exists(local + L"\\device.key"));
        assert(std::filesystem::exists(roaming + L"\\settings.json"));
        denied_root.clear(); root_calls.clear();
        assert(user_cleanup::profile(home, L"Oink", unregister_tracked));
        assert(root_calls == std::vector<std::wstring>{failed});
    }
    root_calls.clear();
    assert(user_cleanup::profile(home, L"Oink", unregister_tracked));
    assert(root_calls.empty()); // An already removed profile is idempotent.
}
// Match files written by foundation's protected, owner-only atomic writer.
// The MSI runs as SYSTEM rather than the owner. Disable backup/restore in an
// isolated thread token so an SSH build token cannot accidentally hide this.
static void private_acl(const std::wstring& base) {
    {
        user_cleanup::Privileges available;
        if (!available.active() && available.error() == ERROR_NOT_ALL_ASSIGNED) {
            // Ordinary users may build packages. The SYSTEM-only qualification
            // additionally runs under the installer identity on Windows.
            std::cout << "owner-only cleanup qualification requires backup/restore privileges: SKIP\n";
            return;
        }
        assert(available.active());
    }
    const auto home = base + L"\\private-user";
    const auto local = home + L"\\AppData\\Local\\Oink";
    std::filesystem::create_directories(local);
    const auto file = local + L"\\settings.json";
    std::ofstream{std::filesystem::path(file)} << "{}";
    PSECURITY_DESCRIPTOR sd = nullptr;
    assert(ConvertStringSecurityDescriptorToSecurityDescriptorW(L"D:P(A;;FA;;;S-1-0-0)", SDDL_REVISION_1, &sd, nullptr));
    BOOL present = FALSE, defaulted = FALSE; PACL acl = nullptr;
    assert(GetSecurityDescriptorDacl(sd, &present, &acl, &defaulted) && present);
    assert(SetNamedSecurityInfoW(const_cast<wchar_t*>(file.c_str()), SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, nullptr, nullptr, acl, nullptr) == ERROR_SUCCESS);
    LocalFree(sd);
    HANDLE process = nullptr, token = nullptr;
    assert(OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &process));
    assert(DuplicateTokenEx(process, TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_ADJUST_PRIVILEGES,
        nullptr, SecurityImpersonation, TokenImpersonation, &token));
    CloseHandle(process);
    for (const auto name : {L"SeBackupPrivilege", L"SeRestorePrivilege"}) {
        TOKEN_PRIVILEGES privileges{}; privileges.PrivilegeCount = 1;
        assert(LookupPrivilegeValueW(nullptr, name, &privileges.Privileges[0].Luid));
        assert(AdjustTokenPrivileges(token, FALSE, &privileges, 0, nullptr, nullptr));
    }
    assert(SetThreadToken(nullptr, token));
    HANDLE denied = CreateFileW(file.c_str(), DELETE | FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE,
        nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr);
    const auto error = GetLastError();
    if (denied != INVALID_HANDLE_VALUE) CloseHandle(denied);
    assert(denied == INVALID_HANDLE_VALUE && error == ERROR_ACCESS_DENIED);
    TOKEN_STATISTICS before{}; DWORD needed = 0;
    assert(GetTokenInformation(token, TokenStatistics, &before, sizeof(before), &needed));
    bool removed = false;
    {
        user_cleanup::Privileges privileges;
        assert(privileges.active());
        removed = user_cleanup::profile(home, L"Oink", unregister_denied);
    }
    // The guard restores the exact caller token, including disabled privileges.
    HANDLE restored = nullptr; TOKEN_STATISTICS after{};
    assert(OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, TRUE, &restored));
    assert(GetTokenInformation(restored, TokenStatistics, &after, sizeof(after), &needed));
    CloseHandle(restored);
    assert(before.TokenId.LowPart == after.TokenId.LowPart && before.TokenId.HighPart == after.TokenId.HighPart);
    assert(RevertToSelf()); CloseHandle(token);
    // Restore only this private fixture on a RED run, never real user ACLs.
    if (!removed) {
        assert(SetNamedSecurityInfoW(const_cast<wchar_t*>(file.c_str()), SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION, nullptr, nullptr, nullptr, nullptr) == ERROR_SUCCESS);
        std::filesystem::remove_all(home);
    }
    assert(removed);
    assert(!std::filesystem::exists(local));
    HANDLE source = nullptr, restricted = nullptr;
    assert(OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &source));
    LUID_AND_ATTRIBUTES remove{};
    assert(LookupPrivilegeValueW(nullptr, L"SeRestorePrivilege", &remove.Luid));
    assert(CreateRestrictedToken(source, 0, 0, nullptr, 1, &remove, 0, nullptr, &restricted));
    CloseHandle(source);
    HANDLE impersonation = nullptr;
    assert(DuplicateTokenEx(restricted, TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_DUPLICATE,
        nullptr, SecurityImpersonation, TokenImpersonation, &impersonation));
    CloseHandle(restricted);
    assert(SetThreadToken(nullptr, impersonation));
    {
        user_cleanup::Privileges privileges;
        assert(!privileges.active() && privileges.error() == ERROR_NOT_ALL_ASSIGNED);
    }
    assert(RevertToSelf()); CloseHandle(impersonation);
}
int wmain() {
    wchar_t temp[32768]; assert(GetTempPathW(32768, temp));
    HANDLE handle = CreateFileW(temp, FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    assert(handle != INVALID_HANDLE_VALUE);
    wchar_t final[32768]; const auto length = GetFinalPathNameByHandleW(handle, final, 32768, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS);
    CloseHandle(handle);
    assert(length > 7 && length < 32768 && wcsncmp(final, L"\\\\?\\", 4) == 0);
    const auto base = std::wstring(final + 4) + L"\\oink-data-cleanup-" + std::to_wstring(GetCurrentProcessId());
    both_roots(base);
    const auto home = base + L"\\user";
    const auto local = home + L"\\AppData\\Local\\Oink";
    const auto roaming = home + L"\\AppData\\Roaming\\Oink";
    for (const auto part : {L"Cache\\CloudFiles", L"WebView2", L"Logs", L"layout-profiles"})
        std::filesystem::create_directories(local + L"\\" + part);
    std::filesystem::create_directories(roaming);
    std::ofstream(std::filesystem::path(local) / L"device.key") << "private fixture identity";
    std::ofstream(std::filesystem::path(local) / L"Cache\\CloudFiles\\received.txt") << "resident fixture";
    std::ofstream(std::filesystem::path(roaming) / L"settings.json") << "{}";
    assert(SetFileAttributesW((local + L"\\Cache\\CloudFiles\\received.txt").c_str(), FILE_ATTRIBUTE_READONLY));
    std::filesystem::create_directories(home + L"\\AppData\\Local\\OtherProduct");
    const auto outside = home + L"\\AppData\\Local\\OtherProduct\\untouched.txt";
    std::ofstream{std::filesystem::path(outside)} << "unrelated fixture";
    assert(CreateHardLinkW((local + L"\\WebView2\\hardlink").c_str(), outside.c_str(), nullptr));
    assert(SetFileAttributesW(outside.c_str(), FILE_ATTRIBUTE_READONLY));
    std::wstring failed_path, operation; DWORD reported = 0;
    assert(!user_cleanup::profile(home, L"Oink", unregister_denied,
        [&](const std::wstring& path, const wchar_t* op, DWORD error) { failed_path = path; operation = op; reported = error; }));
    assert(failed_path == local + L"\\Cache\\CloudFiles" && operation == L"CfUnregisterSyncRoot HRESULT" && reported == static_cast<DWORD>(E_ACCESSDENIED));
    assert(std::filesystem::exists(local + L"\\device.key"));
    assert(user_cleanup::profile(home, L"Oink", unregister_ok));
    assert(!std::filesystem::exists(local) && !std::filesystem::exists(roaming));
    assert(std::filesystem::exists(home + L"\\AppData\\Local\\OtherProduct"));
    assert(user_cleanup::profile(home, L"Oink", unregister_ok));
    assert((GetFileAttributesW(outside.c_str()) & FILE_ATTRIBUTE_READONLY) != 0);
    assert(SetFileAttributesW(outside.c_str(), FILE_ATTRIBUTE_NORMAL));
    assert(calls == 2); // Absent roots do not call the driver.
    private_acl(base);
    const auto locked_home = base + L"\\locked-user";
    const auto locked_store = locked_home + L"\\AppData\\Local\\Oink";
    std::filesystem::create_directories(locked_store);
    const auto locked_path = locked_store + L"\\settings.json";
    std::ofstream{std::filesystem::path(locked_path)} << "{}";
    HANDLE locked = CreateFileW(locked_path.c_str(), GENERIC_READ, FILE_SHARE_READ,
        nullptr, OPEN_EXISTING, 0, nullptr);
    assert(locked != INVALID_HANDLE_VALUE);
    failed_path.clear(); reported = 0;
    {
        user_cleanup::Privileges privileges;
        assert(!user_cleanup::profile(locked_home, L"Oink", unregister_denied,
            [&](const std::wstring& path, const wchar_t*, DWORD error) { failed_path = path; reported = error; }));
    }
    assert(failed_path == locked_path && reported == ERROR_SHARING_VIOLATION);
    CloseHandle(locked);
    assert(user_cleanup::profile(locked_home, L"Oink", unregister_denied));
    std::filesystem::remove_all(base);
    std::cout << "user data: unregister-before-delete, failure preservation, complete cleanup, unrelated data preserved, absent idempotent PASS\n";
}
