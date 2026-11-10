// MSI stops its owned service; deferred validation waits for normal user shutdown.
// Immediate execute-sequence preparation requests the existing normal-user Quit path.
// Product identifiers are generated from packaging/identity.json by build.py.
#include <windows.h>
#include <msiquery.h>
#include <aclapi.h>
#include <sddl.h>
#include <shlobj.h>
#include <tlhelp32.h>
#include <userenv.h>
#include "GracefulStop.h"
#include <string>
#include <vector>
#include <cstddef>
#include "identity.h"
#include "FinishInstall.h"
#include "Relaunch.h"

static UINT fail(MSIHANDLE install, int code) {
    MSIHANDLE record = MsiCreateRecord(0);
    MsiRecordSetStringW(record, 0, graceful_stop::failure(code));
    MsiProcessMessage(install, INSTALLMESSAGE_ERROR, record);
    MsiCloseHandle(record);
    return ERROR_INSTALL_FAILURE;
}

static std::wstring root() {
    PWSTR path = nullptr;
    if (FAILED(SHGetKnownFolderPath(FOLDERID_ProgramFilesX64, 0, nullptr, &path))) return {};
    std::wstring value(path);
    CoTaskMemFree(path);
    return value + L"\\" ID_INSTALL_DIR;
}

static bool trusted(PSID sid) {
    for (const auto text : {L"S-1-5-18", L"S-1-5-32-544",
         L"S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"}) {
        PSID known = nullptr;
        if (!ConvertStringSidToSidW(text, &known)) return false;
        bool same = EqualSid(sid, known) != FALSE;
        LocalFree(known);
        if (same) return true;
    }
    return false;
}

static bool protected_handle(HANDLE file) {
    BY_HANDLE_FILE_INFORMATION info{};
    bool ok = GetFileInformationByHandle(file, &info) && !(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)
        && ((info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) || info.nNumberOfLinks == 1);
    PSECURITY_DESCRIPTOR sd = nullptr;
    PSID owner = nullptr;
    PACL acl = nullptr;
    if (ok) ok = GetSecurityInfo(file, SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
        &owner, nullptr, &acl, nullptr, &sd) == ERROR_SUCCESS && owner && trusted(owner) && acl;
    if (ok) {
        for (DWORD index = 0; index < acl->AceCount; ++index) {
            void* raw = nullptr;
            if (!GetAce(acl, index, &raw)) { ok = false; break; }
            auto ace = static_cast<ACCESS_ALLOWED_ACE*>(raw);
            if (ace->Header.AceFlags & INHERIT_ONLY_ACE) continue;
            PSID sid = nullptr;
            if (ace->Header.AceType == ACCESS_ALLOWED_ACE_TYPE || ace->Header.AceType == ACCESS_ALLOWED_CALLBACK_ACE_TYPE)
                sid = &ace->SidStart;
            else if (ace->Header.AceType == ACCESS_ALLOWED_OBJECT_ACE_TYPE || ace->Header.AceType == ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE) {
                auto object = static_cast<ACCESS_ALLOWED_OBJECT_ACE*>(raw);
                auto offset = offsetof(ACCESS_ALLOWED_OBJECT_ACE, ObjectType);
                if (object->Flags & ACE_OBJECT_TYPE_PRESENT) offset += sizeof(GUID);
                if (object->Flags & ACE_INHERITED_OBJECT_TYPE_PRESENT) offset += sizeof(GUID);
                sid = reinterpret_cast<BYTE*>(raw) + offset;
            }
            // Deny/audit ACEs do not grant writes and are not an admission veto.
            if (!sid) continue;
            constexpr DWORD writes = FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA |
                FILE_WRITE_ATTRIBUTES | FILE_DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_WRITE | GENERIC_ALL;
            if ((ace->Mask & writes) && !trusted(sid)) { ok = false; break; }
        }
    }
    if (sd) LocalFree(sd);
    return ok;
}

static bool protected_path(const std::wstring& path) {
    HANDLE file = CreateFileW(path.c_str(), READ_CONTROL | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr, OPEN_EXISTING,
        FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    if (file == INVALID_HANDLE_VALUE) {
        DWORD error = GetLastError();
        return error == ERROR_FILE_NOT_FOUND || error == ERROR_PATH_NOT_FOUND;
    }
    bool ok = protected_handle(file);
    CloseHandle(file);
    return ok;
}

#include "Maintenance.h"
#include "Quiescence.h"
#include "MaintenanceCleanup.h"
#include "UserDataCleanup.h"

static std::wstring property(MSIHANDLE install, const wchar_t* name) {
    wchar_t value[32768]; DWORD size = static_cast<DWORD>(std::size(value));
    return MsiGetPropertyW(install, name, value, &size) == ERROR_SUCCESS ? std::wstring(value, size) : std::wstring();
}

extern "C" __declspec(dllexport) UINT __stdcall FinishInstall(MSIHANDLE install) {
    MsiSetPropertyW(install, L"OinkFinishFailed", L"");
    if (!finish_install::allowed(property(install, L"UILevel"), !property(install, L"Installed").empty(),
        !property(install, L"REMOVE").empty(), MsiGetMode(install, MSIRUNMODE_SCHEDULED) != FALSE,
        MsiGetMode(install, MSIRUNMODE_COMMIT) != FALSE, MsiGetMode(install, MSIRUNMODE_ROLLBACK) != FALSE)) return ERROR_SUCCESS;
    const bool launch = property(install, L"OinkLaunch") == L"1";
    const bool login = property(install, L"OINK_START_AT_LOGIN") == L"1";
    if (!launch && !login) return ERROR_SUCCESS;
    const auto directory = root();
    std::wstring arguments = L"--installer-finish";
    if (launch) arguments += L" --launch";
    if (login) arguments += L" --start-at-login";
    bool foreground_granted = false;
    const bool completed = finish_install::launch(directory + L"\\" ID_UI, arguments, directory, &foreground_granted);
    const DWORD error = GetLastError();
    const auto outcome = L"FinishInstall: launch=" + std::to_wstring(launch) + L", login=" + std::to_wstring(login) +
        L", foreground_granted=" + std::to_wstring(foreground_granted) + L", helper_completed=" +
        std::to_wstring(completed) + L", error=" + std::to_wstring(error) + L".";
    // DoAction-launched DLL actions cannot use MsiProcessMessage. Property
    // changes remain in verbose MSI logs; the Finish control spawns the owned
    // warning dialog after refreshing the result property.
    MsiSetPropertyW(install, L"OinkFinishStatus", outcome.c_str());
    if (completed) {
        MsiSetPropertyW(install, L"OinkFinishToken", L"user account, UIAccess=0, interactive session matched client");
        return ERROR_SUCCESS;
    }
    MsiSetPropertyW(install, L"OinkFinishFailed", L"1");
    // The transaction has committed. A launch error must not claim install failed.
    return ERROR_SUCCESS;
}

static std::string database_product(MSIHANDLE database) {
    MSIHANDLE view = 0, row = 0;
    std::string result;
    if (MsiDatabaseOpenViewW(database, L"SELECT `Value` FROM `Property` WHERE `Property`='ProductCode'", &view) == ERROR_SUCCESS &&
        MsiViewExecute(view, 0) == ERROR_SUCCESS && MsiViewFetch(view, &row) == ERROR_SUCCESS) {
        wchar_t value[64]; DWORD size = static_cast<DWORD>(std::size(value));
        if (MsiRecordGetStringW(row, 1, value, &size) == ERROR_SUCCESS) result = admission::guid(maintenance::narrow(value));
    }
    if (row) MsiCloseHandle(row);
    if (view) MsiCloseHandle(view);
    return result;
}

static bool bind_package(MSIHANDLE install) {
    // Read ProductCode from the active database, not the overridable session
    // property. OriginalDatabase is only a source locator; reopen it, compare the
    // database identity and hash its exact bytes before authorizing any effects.
    MSIHANDLE active = MsiGetActiveDatabase(install), source_database = 0;
    const auto expected = admission::guid(maintenance::narrow(ID_PRODUCT_CODE));
    const auto source = property(install, L"OriginalDatabase");
    bool ok = active && !source.empty() && source.find(L'|') == std::wstring::npos && database_product(active) == expected &&
        MsiOpenDatabaseW(source.c_str(), MSIDBOPEN_READONLY, &source_database) == ERROR_SUCCESS && database_product(source_database) == expected;
    if (active) MsiCloseHandle(active);
    if (source_database) MsiCloseHandle(source_database);
    if (!ok) return false;
    const auto hash = maintenance::sha256(source);
    if (!admission::hex(hash, 64)) return false;
    std::string remove, remove_hash;
    for (DWORD index = 0;; ++index) {
        wchar_t product[39];
        UINT result = MsiEnumRelatedProductsW(ID_UPGRADE_CODE, 0, index, product);
        if (result == ERROR_NO_MORE_ITEMS) break;
        if (result != ERROR_SUCCESS) return false;
        auto code = admission::guid(maintenance::narrow(product));
        if (code == expected) continue;
        if (!remove.empty()) return false; // Multiple installed product families need repair first.
        wchar_t cached[32768]; DWORD size = static_cast<DWORD>(std::size(cached));
        if (MsiGetProductInfoExW(product, nullptr, MSIINSTALLCONTEXT_MACHINE, INSTALLPROPERTY_LOCALPACKAGE,
            cached, &size) != ERROR_SUCCESS || !protected_path(cached)) return false;
        remove = code; remove_hash = maintenance::sha256(cached);
        if (!admission::hex(remove_hash, 64)) return false;
    }
    // Private deferred data is always overwritten here. It carries derived
    // metadata, never a caller-provided "allow update"/"ignore pending" flag.
    // Use the effective invoking identity: impersonation token when present,
    // otherwise the process token (ERROR_NO_TOKEN only). Never use MSI public
    // properties as authority or borrow SYSTEM while impersonating a user.
    HANDLE caller = nullptr;
    auto thread = admission::CallerToken::unreadable;
    if (OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, TRUE, &caller)) {
        thread = maintenance::system_token(caller) ? admission::CallerToken::system : admission::CallerToken::other;
        CloseHandle(caller);
    } else if (GetLastError() == ERROR_NO_TOKEN) {
        thread = admission::CallerToken::absent;
    }
    const bool system_caller = admission::effective_system(thread, [] {
        return maintenance::system_process(GetCurrentProcess());
    });
    const auto data = source + L"|" + maintenance::wide(hash + "|" + remove + "|" + remove_hash) + (system_caller ? L"|1" : L"|0");
    return MsiSetPropertyW(install, L"CheckProtectedState", data.c_str()) == ERROR_SUCCESS;
}

extern "C" __declspec(dllexport) UINT __stdcall CheckInstallPath(MSIHANDLE install) {
    std::wstring expected = root();
    wchar_t actual[32768];
    DWORD size = static_cast<DWORD>(std::size(actual));
    if (expected.empty() || MsiGetPropertyW(install, L"INSTALLFOLDER", actual, &size) != ERROR_SUCCESS)
        return fail(install, 1);
    std::wstring value(actual);
    while (!value.empty() && value.back() == L'\\') value.pop_back();
    return _wcsicmp(value.c_str(), expected.c_str()) == 0 && bind_package(install) ? ERROR_SUCCESS : fail(install, 2);
}

static bool clients_quiescent(const std::wstring& directory) {
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snapshot == INVALID_HANDLE_VALUE) {
        return false;
    }
    PROCESSENTRY32W entry{sizeof(entry)};
    bool busy = false;
    BOOL more = Process32FirstW(snapshot, &entry);
    while (more) {
        if (_wcsicmp(entry.szExeFile, ID_UI) != 0 && _wcsicmp(entry.szExeFile, ID_AGENT) != 0 && _wcsicmp(entry.szExeFile, ID_CRASH_COLLECTOR) != 0 &&
            _wcsicmp(entry.szExeFile, ID_SERVICE_EXE) != 0) {
            more = Process32NextW(snapshot, &entry);
            continue;
        }
        HANDLE process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, entry.th32ProcessID);
        if (!process) {
            if (GetLastError() != ERROR_INVALID_PARAMETER) { busy = true; break; }
        } else {
            if (WaitForSingleObject(process, 0) != WAIT_OBJECT_0) {
                const auto image = maintenance::image(process);
                if (image.empty()) busy = true;
                else busy = quiescence::product_path(directory, image, {ID_UI, ID_AGENT, ID_SERVICE_EXE, ID_CRASH_COLLECTOR});
            }
            CloseHandle(process);
        }
        if (busy) break;
        more = Process32NextW(snapshot, &entry);
    }
    if (!more && GetLastError() != ERROR_NO_MORE_FILES) busy = true;
    CloseHandle(snapshot);
    return !busy;
}

// The client-side scan is advisory: inspect only actual UI/backend/CLI/collector
// images. The elevated post-StopServices scan also covers the SYSTEM broker
// and helper; a normal user cannot query their token DACLs.
static graceful_stop::State user_sessions(const std::wstring& directory) {
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snapshot == INVALID_HANDLE_VALUE) return graceful_stop::State::unavailable;
    PROCESSENTRY32W entry{sizeof(entry)};
    auto result = graceful_stop::State::clear;
    BOOL more = Process32FirstW(snapshot, &entry);
    while (more) {
        if (_wcsicmp(entry.szExeFile, ID_UI) == 0 || _wcsicmp(entry.szExeFile, ID_AGENT) == 0 ||
            _wcsicmp(entry.szExeFile, ID_CRASH_COLLECTOR) == 0) {
            HANDLE process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, entry.th32ProcessID);
            if (!process) {
                if (GetLastError() != ERROR_INVALID_PARAMETER) result = graceful_stop::State::unavailable;
            } else {
                if (WaitForSingleObject(process, 0) != WAIT_OBJECT_0) {
                    const auto image = maintenance::image(process);
                    if (image.empty()) result = graceful_stop::State::unavailable;
                    else if (_wcsnicmp(image.c_str(), (directory + L"\\").c_str(), directory.size() + 1) == 0)
                        result = graceful_stop::State::busy;
                }
                CloseHandle(process);
            }
            if (result != graceful_stop::State::clear) break;
        }
        more = Process32NextW(snapshot, &entry);
    }
    if (!more && GetLastError() != ERROR_NO_MORE_FILES) result = graceful_stop::State::unavailable;
    CloseHandle(snapshot);
    return result;
}


static bool request_user_stop(MSIHANDLE install, const std::wstring& directory) {
    // Use only the actual caller's token. Never borrow Explorer/another session,
    // stop a service, send window-close/input messages, or terminate any process.
    // Execute-sequence immediate actions may run in the server while impersonating
    // the original caller; CreateProcessAsUser preserves that exact user token.
    HANDLE caller = nullptr;
    if (!OpenThreadToken(GetCurrentThread(), TOKEN_QUERY | TOKEN_DUPLICATE, TRUE, &caller)) {
        if (GetLastError() != ERROR_NO_TOKEN ||
            !OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &caller)) return false;
    }
    DWORD session = 0;
    bool ok = finish_install::normal_token(caller, session);
    HANDLE primary = nullptr;
    if (ok) ok = DuplicateTokenEx(caller, TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY, nullptr,
                                  SecurityImpersonation, TokenPrimary, &primary) != FALSE;
    CloseHandle(caller);
    const auto executable = directory + L"\\" ID_AGENT;
    ok = ok && protected_path(executable);
    void* environment = nullptr;
    if (ok) ok = CreateEnvironmentBlock(&environment, primary, FALSE) != FALSE;
    STARTUPINFOW startup{}; startup.cb = sizeof(startup);
    PROCESS_INFORMATION child{};
    std::wstring command = L"\"" + executable + L"\" stop";
    if (ok) {
        const DWORD flags = CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;
        if (finish_install::normal_user(GetCurrentProcess(), session))
            ok = CreateProcessW(executable.c_str(), command.data(), nullptr, nullptr, FALSE, flags,
                                environment, directory.c_str(), &startup, &child) != FALSE;
        else
            ok = CreateProcessAsUserW(primary, executable.c_str(), command.data(), nullptr, nullptr, FALSE,
                                      flags, environment, directory.c_str(), &startup, &child) != FALSE;
    }
    if (environment) DestroyEnvironmentBlock(environment);
    if (primary) CloseHandle(primary);
    if (child.hProcess) {
        ok = finish_install::normal_user(child.hProcess, session);
        CloseHandle(child.hThread);
        CloseHandle(child.hProcess);
    }
    MSIHANDLE message = MsiCreateRecord(0);
    MsiRecordSetStringW(message, 0, ok ?
        L"PrepareUserSessions: graceful installed CLI stop requested; child user token verified, UIAccess=0, caller session matched." :
        L"PrepareUserSessions: graceful stop unavailable; requires the original signed-in caller and protected installed CLI. StopServices will notify installed backends.");
    MsiProcessMessage(install, INSTALLMESSAGE_INFO, message);
    MsiCloseHandle(message);
    return ok;
}

static void info(MSIHANDLE install, const std::wstring& text) {
    MSIHANDLE record = MsiCreateRecord(0);
    MsiRecordSetStringW(record, 0, text.c_str());
    MsiProcessMessage(install, INSTALLMESSAGE_INFO, record);
    MsiCloseHandle(record);
}

// Every live installed UI/backend image (both are ID_UI) with a readable token,
// by its token session and user. The caller always reads its own processes;
// SYSTEM and elevated callers read every session. The SYSTEM pre-login backend
// is not a user session and is never recorded.
static std::vector<relaunch::Session> running_sessions(const std::wstring& directory) {
    std::vector<relaunch::Session> sessions;
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snapshot == INVALID_HANDLE_VALUE) return sessions;
    PROCESSENTRY32W entry{sizeof(entry)};
    for (BOOL more = Process32FirstW(snapshot, &entry); more; more = Process32NextW(snapshot, &entry)) {
        if (_wcsicmp(entry.szExeFile, ID_UI) != 0) continue;
        HANDLE process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, entry.th32ProcessID);
        if (!process) continue;
        HANDLE token = nullptr;
        if (WaitForSingleObject(process, 0) == WAIT_TIMEOUT &&
            _wcsicmp(maintenance::image(process).c_str(), (directory + L"\\" ID_UI).c_str()) == 0 &&
            OpenProcessToken(process, TOKEN_QUERY, &token)) {
            alignas(TOKEN_USER) BYTE user[sizeof(TOKEN_USER) + SECURITY_MAX_SID_SIZE]{};
            DWORD session = 0, size = 0;
            PWSTR text = nullptr;
            if (GetTokenInformation(token, TokenUser, user, sizeof(user), &size) &&
                GetTokenInformation(token, TokenSessionId, &session, sizeof(session), &size) &&
                !finish_install::service_account(reinterpret_cast<TOKEN_USER*>(user)->User.Sid) &&
                ConvertSidToStringSidW(reinterpret_cast<TOKEN_USER*>(user)->User.Sid, &text)) {
                relaunch::add(sessions, session, text);
                LocalFree(text);
            }
            CloseHandle(token);
        }
        CloseHandle(process);
    }
    CloseHandle(snapshot);
    return sessions;
}

static UINT user_session_gate(MSIHANDLE install) {
    const auto directory = root();
    if (directory.empty()) return fail(install, 3);
    const bool allowed = graceful_stop::permitted(property(install, L"UILevel"),
        !property(install, L"UPGRADINGPRODUCTCODE").empty(), MsiGetMode(install, MSIRUNMODE_SCHEDULED) != FALSE,
        MsiGetMode(install, MSIRUNMODE_COMMIT) != FALSE, MsiGetMode(install, MSIRUNMODE_ROLLBACK) != FALSE);
    if (allowed) {
        // Record before the stop request; RelaunchUserSessions consumes this
        // as CustomActionData only if the transaction commits (issue #36).
        const auto data = relaunch::encode(running_sessions(directory));
        MsiSetPropertyW(install, L"RelaunchUserSessions", data.c_str());
        info(install, L"PrepareUserSessions: running Oink sessions before Quit: " + (data.empty() ? std::wstring(L"none") : data) + L".");
    }
    const auto state = graceful_stop::prepare(allowed, [&] { return user_sessions(directory); },
        [&] { return request_user_stop(install, directory); }, [&] {
            MSIHANDLE progress = MsiCreateRecord(0);
            MsiRecordSetStringW(progress, 0, L"Waiting for Oink to quit safely");
            const int response = MsiProcessMessage(install, INSTALLMESSAGE_ACTIONDATA, progress);
            MsiCloseHandle(progress);
            if (response == IDCANCEL) return false;
            Sleep(250);
            return true;
        });
    if (state == graceful_stop::State::canceled) return ERROR_INSTALL_USEREXIT;
    const bool busy = state != graceful_stop::State::clear;
    // This pre-InstallValidate user-context request is not replacement authority.
    // Settings/basic UI and SYSTEM installs may not have a usable user token;
    // MSI StopServices triggers each backend's existing SCM stop subscription.
    // CheckQuiescence below observes the resulting normal cleanup in all sessions.
    if (busy) {
        MSIHANDLE message = MsiCreateRecord(0);
        MsiRecordSetStringW(message, 0, L"Oink session remains; waiting for orderly shutdown after StopServices.");
        MsiProcessMessage(install, INSTALLMESSAGE_INFO, message);
        MsiCloseHandle(message);
    }
    return ERROR_SUCCESS;
}
extern "C" __declspec(dllexport) UINT __stdcall PrepareUserSessions(MSIHANDLE install) {
    return user_session_gate(install);
}

extern "C" __declspec(dllexport) UINT __stdcall CheckQuiescence(MSIHANDLE install) {
    // StopServices closes the launch endpoint and joins outstanding helpers.
    // Wait for every product role here; MSI rollback restores service on failure.
    const auto directory = root();
    if (directory.empty()) return fail(install, 3);
    const auto state = graceful_stop::observe([&] {
        return clients_quiescent(directory) ? graceful_stop::State::clear : graceful_stop::State::busy;
    }, [] { Sleep(250); return true; });
    return state == graceful_stop::State::clear ? ERROR_SUCCESS : fail(install, 8);
}

// Commit-only, as SYSTEM, after the new OinkInput started: Windows Installer runs
// it only once the whole transaction (including nested old-product removal)
// succeeded, so a rolled-back install never reopens anything. Publishing is by
// rename into the protected maintenance directory, whose name-change
// notification already wakes the service's update engine; the service, not this
// short-lived installer process, creates the UI (WTSQueryUserToken of that
// session, never the installer's possibly elevated or SYSTEM token).
extern "C" __declspec(dllexport) UINT __stdcall RelaunchUserSessions(MSIHANDLE install) {
    std::vector<relaunch::Session> sessions;
    if (!relaunch::decode(property(install, L"CustomActionData"), sessions)) {
        info(install, L"RelaunchUserSessions: malformed session record; Oink is not reopened.");
        return ERROR_SUCCESS;
    }
    if (sessions.empty()) return ERROR_SUCCESS;
    const auto directory = maintenance::directory();
    // The in-app updater's worker drives its own msiexec under a pending
    // journal and reopens its drained sessions after health; it owns that relaunch.
    if (!maintenance::absent(directory + L"\\pending.json")) {
        info(install, L"RelaunchUserSessions: an update transaction owns the relaunch.");
        return ERROR_SUCCESS;
    }
    const auto temporary = directory + L"\\relaunch.new", target = directory + L"\\relaunch.json";
    const auto text = relaunch::json(sessions);
    bool ok = protected_path(directory);
    HANDLE file = ok ? CreateFileW(temporary.c_str(), GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH, nullptr) : INVALID_HANDLE_VALUE;
    DWORD written = 0;
    ok = file != INVALID_HANDLE_VALUE && WriteFile(file, text.data(), static_cast<DWORD>(text.size()), &written, nullptr) &&
        written == text.size() && FlushFileBuffers(file);
    if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
    ok = ok && MoveFileExW(temporary.c_str(), target.c_str(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH);
    const DWORD error = ok ? ERROR_SUCCESS : GetLastError();
    info(install, L"RelaunchUserSessions: " + relaunch::encode(sessions) + (ok ? L" handed to OinkInput." :
        L" not published; error=" + std::to_wstring(error) + L". Open Oink from the Start menu."));
    return ERROR_SUCCESS;
}

extern "C" __declspec(dllexport) UINT __stdcall CheckProtectedState(MSIHANDLE install) {
    const auto directory = root();
    if (directory.empty()) return fail(install, 3);
    if (!protected_path(maintenance::directory())) return fail(install, 5);
    const auto data = property(install, L"CustomActionData");
    std::vector<std::wstring> parts; size_t offset = 0;
    for (;;) {
        auto next = data.find(L'|', offset);
        parts.push_back(data.substr(offset, next == std::wstring::npos ? next : next - offset));
        if (next == std::wstring::npos) break;
        offset = next + 1;
    }
    if (parts.size() != 5 || (parts[4] != L"0" && parts[4] != L"1") || maintenance::sha256(parts[0]) != maintenance::narrow(parts[1]) ||
        !maintenance::admit(admission::guid(maintenance::narrow(ID_PRODUCT_CODE)), maintenance::narrow(parts[1]),
                            maintenance::narrow(parts[2]), maintenance::narrow(parts[3]), parts[4] == L"1")) return fail(install, 6);
    SC_HANDLE scm = OpenSCManagerW(nullptr, nullptr, SC_MANAGER_CONNECT);
    if (!scm) return fail(install, 9);
    SC_HANDLE service = OpenServiceW(scm, ID_SERVICE, SERVICE_QUERY_CONFIG);
    bool valid = true;
    if (service) {
        DWORD needed = 0;
        QueryServiceConfigW(service, nullptr, 0, &needed);
        std::vector<BYTE> bytes(needed);
        auto config = reinterpret_cast<QUERY_SERVICE_CONFIGW*>(bytes.data());
        const auto expected = L"\"" + directory + L"\\" ID_SERVICE_EXE L"\"";
        valid = QueryServiceConfigW(service, config, needed, &needed) && config->dwServiceType == SERVICE_WIN32_OWN_PROCESS &&
            _wcsicmp(config->lpServiceStartName, L"LocalSystem") == 0 && _wcsicmp(config->lpBinaryPathName, expected.c_str()) == 0;
        CloseServiceHandle(service);
    } else valid = GetLastError() == ERROR_SERVICE_DOES_NOT_EXIST;
    CloseServiceHandle(scm);
    return valid ? ERROR_SUCCESS : fail(install, 10);
}

// Post-removal cleanup is best effort. Commit custom actions can still trigger
// rollback; never emit INSTALLMESSAGE_ERROR or return failure after removal.
static void cleanup_log(MSIHANDLE install, const std::wstring& path, const wchar_t* operation, DWORD error) {
    MSIHANDLE record = MsiCreateRecord(3);
    if (!record) return;
    MsiRecordSetStringW(record, 0, L"Oink cleanup retained [1]; operation=[2]; error=[3]. Product removal will complete.");
    MsiRecordSetStringW(record, 1, path.c_str());
    MsiRecordSetStringW(record, 2, operation);
    const auto code = std::to_wstring(error);
    MsiRecordSetStringW(record, 3, code.c_str());
    MsiProcessMessage(install, INSTALLMESSAGE_INFO, record);
    MsiCloseHandle(record);
}

extern "C" __declspec(dllexport) UINT __stdcall CleanupMaintenance(MSIHANDLE install) {
    const auto directory = root();
    if (directory.empty()) { cleanup_log(install, ID_INSTALL_DIR, L"ProgramFiles", ERROR_PATH_NOT_FOUND); return ERROR_SUCCESS; }
    const auto result = cleanup::after_uninstall(directory, directory + L"\\" ID_MAINTENANCE,
        ID_MAINTENANCE_LOCK, protected_path, [](HANDLE lock) { return protected_handle(lock) && maintenance::lock_handle(lock); });
    if (result == cleanup::Result::failed) cleanup_log(install, directory, L"maintenance cleanup", GetLastError());
    return ERROR_SUCCESS;
}

extern "C" __declspec(dllexport) UINT __stdcall RemoveLoginEntry(MSIHANDLE) {
    if (cleanup::preserve_user_state(maintenance::directory())) return ERROR_SUCCESS;
    // Only loaded interactive hives. Do not mount profiles, change StartupApproved,
    // touch user data, or delete an unrelated command with the same value name.
    const auto expected = L"\"" + root() + L"\\" ID_UI L"\" --autostart";
    for (DWORD index = 0;; ++index) {
        wchar_t sid[256]; DWORD size = static_cast<DWORD>(std::size(sid));
        LONG result = RegEnumKeyExW(HKEY_USERS, index, sid, &size, nullptr, nullptr, nullptr, nullptr);
        if (result == ERROR_NO_MORE_ITEMS) break;
        if (result != ERROR_SUCCESS) continue;
        auto key = std::wstring(sid) + L"\\Software\\Microsoft\\Windows\\CurrentVersion\\Run";
        HKEY run = nullptr;
        if (RegOpenKeyExW(HKEY_USERS, key.c_str(), 0, KEY_QUERY_VALUE | KEY_SET_VALUE, &run) != ERROR_SUCCESS) continue;
        wchar_t command[32768]; DWORD bytes = sizeof(command), type = 0;
        if (RegQueryValueExW(run, ID_RUN_VALUE, nullptr, &type, reinterpret_cast<BYTE*>(command), &bytes) == ERROR_SUCCESS &&
            type == REG_SZ && bytes >= sizeof(wchar_t) && command[bytes / sizeof(wchar_t) - 1] == 0 &&
            _wcsicmp(command, expected.c_str()) == 0) RegDeleteValueW(run, ID_RUN_VALUE);
        RegCloseKey(run);
    }
    return ERROR_SUCCESS;
}

// Deferred, as SYSTEM, only when launch at login was chosen on the welcome page.
// CustomActionData is the installing user's SID. Same marker and descriptor as
// OinkInput's own writer (crates/oink-winsvc/src/platform/prelogin.rs): Enabled
// and Owner under HKLM\SOFTWARE\Oink\Prelogin, readable by users, writable by
// SYSTEM and Administrators only. The owner's backend exports its snapshot later.
extern "C" __declspec(dllexport) UINT __stdcall EnablePrelogin(MSIHANDLE install) {
    const auto owner = property(install, L"CustomActionData");
    PSID sid = nullptr;
    if (owner.empty() || !ConvertStringSidToSidW(owner.c_str(), &sid)) {
        cleanup_log(install, L"HKLM\\SOFTWARE\\Oink\\Prelogin", L"owner SID", GetLastError());
        return ERROR_SUCCESS;
    }
    LocalFree(sid);
    PSECURITY_DESCRIPTOR descriptor = nullptr;
    if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(
            L"O:SYG:SYD:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)(A;CI;KR;;;BU)", SDDL_REVISION_1, &descriptor, nullptr)) {
        cleanup_log(install, L"HKLM\\SOFTWARE\\Oink\\Prelogin", L"descriptor", GetLastError());
        return ERROR_SUCCESS;
    }
    SECURITY_ATTRIBUTES attributes{sizeof(attributes), descriptor, FALSE};
    HKEY key = nullptr;
    LONG result = RegCreateKeyExW(HKEY_LOCAL_MACHINE, L"SOFTWARE\\Oink\\Prelogin", 0, nullptr, 0,
        KEY_SET_VALUE | KEY_WOW64_64KEY, &attributes, &key, nullptr);
    LocalFree(descriptor);
    if (result == ERROR_SUCCESS) {
        const DWORD enabled = 1;
        result = RegSetValueExW(key, L"Enabled", 0, REG_DWORD, reinterpret_cast<const BYTE*>(&enabled), sizeof(enabled));
        if (result == ERROR_SUCCESS)
            result = RegSetValueExW(key, L"Owner", 0, REG_SZ, reinterpret_cast<const BYTE*>(owner.c_str()),
                static_cast<DWORD>((owner.size() + 1) * sizeof(wchar_t)));
        RegCloseKey(key);
    }
    if (result != ERROR_SUCCESS) cleanup_log(install, L"HKLM\\SOFTWARE\\Oink\\Prelogin", L"marker", static_cast<DWORD>(result));
    return ERROR_SUCCESS;
}

extern "C" __declspec(dllexport) UINT __stdcall CleanupUserData(MSIHANDLE install) {
    if (cleanup::preserve_user_state(maintenance::directory())) return ERROR_SUCCESS;
    user_cleanup::profiles(ID_DATA_DIRECTORY, [install](const std::wstring& path, const wchar_t* operation, DWORD error) {
        cleanup_log(install, path, operation, error);
    });
    return ERROR_SUCCESS;
}

// The archive retained by explicit installation survives MSI file removal. The
// embedded script rechecks ownership, exact device identity, hash and trust.
#include "microphone-cleanup.h"
#include "EmbeddedPowerShell.h"
extern "C" __declspec(dllexport) UINT __stdcall CleanupMicrophone(MSIHANDLE install) {
    wchar_t system[MAX_PATH]{};
    if (!GetSystemDirectoryW(system, MAX_PATH)) return ERROR_SUCCESS;
    std::wstring executable = std::wstring(system) + L"\\WindowsPowerShell\\v1.0\\powershell.exe";
    DWORD code = embedded_powershell::run(executable, MICROPHONE_CLEANUP, system);
    if (code) cleanup_log(install, L"VB-CABLE", L"managed microphone cleanup", code);
    return ERROR_SUCCESS;
}
