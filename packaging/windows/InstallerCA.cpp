// Deferred preflight never stops processes or accepts arbitrary paths.
// Immediate client preparation requests the existing normal-user Quit path.
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
#include "identity.h"
#include "FinishInstall.h"

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
            // Object/callback allow ACEs require evaluation we do not implement;
            // fail closed rather than silently overlooking a write grant.
            if (ace->Header.AceType != ACCESS_ALLOWED_ACE_TYPE) {
                if (ace->Header.AceType != ACCESS_DENIED_ACE_TYPE) { ok = false; break; }
                continue;
            }
            constexpr DWORD writes = FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA |
                FILE_WRITE_ATTRIBUTES | FILE_DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_WRITE | GENERIC_ALL;
            if ((ace->Mask & writes) && !trusted(&ace->SidStart)) { ok = false; break; }
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
#include "CloudFilesCleanup.h"

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
    const bool login = property(install, L"OinkLogin") == L"1";
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
        MsiSetPropertyW(install, L"OinkFinishToken", L"elevated=0, UIAccess=0, interactive session matched client");
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
    // Immediate actions impersonate the invoking client. Derive this from the
    // thread token, never CLIENTPROCESSID or another public MSI property. Only
    // a SYSTEM caller can claim the protected worker's admission record; a
    // manual elevated administrator must not borrow an in-flight authorization.
    HANDLE caller = nullptr;
    bool system_caller = OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, TRUE, &caller) && maintenance::system_token(caller);
    if (caller) CloseHandle(caller);
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

static bool running_service(const std::wstring& directory, quiescence::Process& identity, HANDLE& process) {
    SC_HANDLE scm = OpenSCManagerW(nullptr, nullptr, SC_MANAGER_CONNECT);
    if (!scm) return false;
    SC_HANDLE service = OpenServiceW(scm, ID_SERVICE, SERVICE_QUERY_STATUS);
    bool ok = false;
    if (service) {
        SERVICE_STATUS_PROCESS status{}; DWORD needed = 0;
        ok = QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, reinterpret_cast<BYTE*>(&status),
            sizeof(status), &needed) != FALSE;
        if (ok && status.dwCurrentState != SERVICE_STOPPED) {
            // SCM only guarantees the PID in stable states. OinkInput does not
            // pause; fail closed during a concurrent start/stop transition.
            ok = status.dwCurrentState == SERVICE_RUNNING && status.dwProcessId != 0;
            if (ok) {
                process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, status.dwProcessId);
                DWORD session = 0;
                ok = process && WaitForSingleObject(process, 0) == WAIT_TIMEOUT &&
                    ProcessIdToSessionId(status.dwProcessId, &session) && session == 0 && maintenance::system_process(process) &&
                    _wcsicmp(maintenance::image(process).c_str(), (directory + L"\\" ID_SERVICE_EXE).c_str()) == 0;
                if (ok) {
                    identity = {status.dwProcessId, 0, session, maintenance::created(process), true, true};
                    ok = identity.created != 0;
                }
            }
        }
        CloseServiceHandle(service);
    } else ok = GetLastError() == ERROR_SERVICE_DOES_NOT_EXIST;
    CloseServiceHandle(scm);
    return ok;
}

static bool clients_quiescent(const std::wstring& directory, quiescence::Phase phase) {
    quiescence::Process service{};
    HANDLE broker = nullptr;
    if (phase == quiescence::Phase::before_stop && !running_service(directory, service, broker)) {
        if (broker) CloseHandle(broker);
        return false;
    }
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snapshot == INVALID_HANDLE_VALUE) {
        if (broker) CloseHandle(broker);
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
                else if (_wcsnicmp(image.c_str(), (directory + L"\\").c_str(), directory.size() + 1) == 0) {
                    DWORD session = 0;
                    quiescence::Process candidate{entry.th32ProcessID, entry.th32ParentProcessID};
                    candidate.service_image = _wcsicmp(image.c_str(), (directory + L"\\" ID_SERVICE_EXE).c_str()) == 0;
                    if (candidate.service_image && ProcessIdToSessionId(candidate.pid, &session)) {
                        candidate.session = session;
                        candidate.created = maintenance::created(process);
                        candidate.system = maintenance::system_process(process);
                    }
                    busy = !quiescence::service_owned(candidate, service, phase);
                }
            }
            CloseHandle(process);
        }
        if (busy) break;
        more = Process32NextW(snapshot, &entry);
    }
    if (!more && GetLastError() != ERROR_NO_MORE_FILES) busy = true;
    CloseHandle(snapshot);
    if (broker) CloseHandle(broker);
    return !busy;
}

// The client-side scan is advisory: inspect only actual UI/backend/CLI/collector
// images. SYSTEM broker/helper authentication remains in the two unchanged,
// elevated service-aware gates; a normal user cannot query their token DACLs.
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

static bool normal_token(HANDLE token, DWORD& session) {
    TOKEN_ELEVATION elevation{}; DWORD access = 0, size = 0;
    return GetTokenInformation(token, TokenElevation, &elevation, sizeof(elevation), &size) &&
        GetTokenInformation(token, TokenSessionId, &session, sizeof(session), &size) &&
        GetTokenInformation(token, TokenUIAccess, &access, sizeof(access), &size) &&
        finish_install::normal_flags(elevation.TokenIsElevated != 0, access, session);
}

static bool request_user_stop(MSIHANDLE install, const std::wstring& directory) {
    // Use only the actual caller's token. Never borrow Explorer/another session,
    // stop a service, send window-close/input messages, or terminate any process.
    // Quiet opt-in immediate actions may run in the server while impersonating
    // the original caller; CreateProcessAsUser preserves that exact user token.
    HANDLE caller = nullptr;
    if (!OpenThreadToken(GetCurrentThread(), TOKEN_QUERY | TOKEN_DUPLICATE, TRUE, &caller)) {
        if (GetLastError() != ERROR_NO_TOKEN ||
            !OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &caller)) return false;
    }
    DWORD session = 0;
    bool ok = normal_token(caller, session);
    HANDLE primary = nullptr;
    if (ok) ok = DuplicateTokenEx(caller, TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY, nullptr,
                                  SecurityImpersonation, TokenPrimary, &primary) != FALSE;
    CloseHandle(caller);
    const auto executable = directory + L"\\" ID_AGENT;
    for (auto ancestor = directory; ok && !ancestor.empty();) {
        ok = protected_path(ancestor);
        const auto slash = ancestor.find_last_of(L'\\');
        if (slash == std::wstring::npos || slash < 3) break;
        ancestor.resize(slash);
    }
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
        L"PrepareUserSessions: graceful installed CLI stop requested; child token verified non-elevated, UIAccess=0, caller session matched." :
        L"PrepareUserSessions: graceful stop unavailable; requires the original non-elevated signed-in caller and protected installed CLI. Quit Oink manually.");
    MsiProcessMessage(install, INSTALLMESSAGE_INFO, message);
    MsiCloseHandle(message);
    return ok;
}

static UINT user_session_gate(MSIHANDLE install, bool request) {
    const auto directory = root();
    if (directory.empty()) return fail(install, 3);
    const bool allowed = request && graceful_stop::permitted(property(install, L"UILevel"), property(install, L"OINK_GRACEFUL_STOP"),
        !property(install, L"UPGRADINGPRODUCTCODE").empty(), MsiGetMode(install, MSIRUNMODE_SCHEDULED) != FALSE,
        MsiGetMode(install, MSIRUNMODE_COMMIT) != FALSE, MsiGetMode(install, MSIRUNMODE_ROLLBACK) != FALSE);
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
    if (MsiSetPropertyW(install, L"OinkSessionsRunning", busy ? L"1" : L"") != ERROR_SUCCESS) return ERROR_INSTALL_FAILURE;
    if (busy && property(install, L"UILevel") != L"5") {
        MSIHANDLE message = MsiCreateRecord(0);
        MsiRecordSetStringW(message, 0, graceful_stop::failure(8));
        MsiProcessMessage(install, INSTALLMESSAGE_INFO, message); // No quiet/passive dialog.
        MsiCloseHandle(message);
        return ERROR_INSTALL_FAILURE;
    }
    return ERROR_SUCCESS;
}
extern "C" __declspec(dllexport) UINT __stdcall PrepareUserSessions(MSIHANDLE install) {
    return user_session_gate(install, true);
}
extern "C" __declspec(dllexport) UINT __stdcall RefreshUserSessions(MSIHANDLE install) {
    return user_session_gate(install, false);
}

extern "C" __declspec(dllexport) UINT __stdcall CheckQuiescence(MSIHANDLE install) {
    // Repeat after StopServices closes the launch endpoint and joins outstanding
    // launch requests, as the existing installer script does. The first scan
    // alone has a launch race. MSI rollback restores the old service on failure.
    const auto directory = root();
    return !directory.empty() && clients_quiescent(directory, quiescence::Phase::after_stop) ? ERROR_SUCCESS : fail(install, 8);
}

extern "C" __declspec(dllexport) UINT __stdcall CheckProtectedState(MSIHANDLE install) {
    const auto directory = root();
    if (directory.empty()) return fail(install, 3);
    for (auto ancestor = directory; !ancestor.empty();) {
        if (!protected_path(ancestor)) return fail(install, 4);
        const auto slash = ancestor.find_last_of(L'\\');
        if (slash == std::wstring::npos || slash < 3) break;
        ancestor.resize(slash);
    }
    for (const auto name : {ID_UI, ID_AGENT, ID_SERVICE_EXE, ID_UPDATE_WORKER, ID_CRASH_COLLECTOR, ID_MAINTENANCE, ID_RECEIPT})
        if (!protected_path(directory + L"\\" + name)) return fail(install, 5);
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
    if (!clients_quiescent(directory, quiescence::Phase::before_stop)) return fail(install, 8);
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

extern "C" __declspec(dllexport) void CALLBACK MaintenanceGuard(HWND, HINSTANCE, LPSTR command, int) {
    maintenance::guard(command);
}

extern "C" __declspec(dllexport) UINT __stdcall ReleaseMaintenance(MSIHANDLE install) {
    return maintenance::release() ? ERROR_SUCCESS : fail(install, 11);
}

extern "C" __declspec(dllexport) UINT __stdcall CleanupMaintenance(MSIHANDLE install) {
    const auto directory = root();
    if (directory.empty()) return fail(install, 12);
    const auto result = cleanup::after_uninstall(directory, directory + L"\\" ID_MAINTENANCE,
        ID_MAINTENANCE_LOCK, protected_path, [](HANDLE lock) { return protected_handle(lock) && maintenance::lock_handle(lock); });
    return result != cleanup::Result::failed ? ERROR_SUCCESS : fail(install, 12);
}

extern "C" __declspec(dllexport) UINT __stdcall RemoveLoginEntry(MSIHANDLE) {
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

extern "C" __declspec(dllexport) UINT __stdcall CleanupCloudFiles(MSIHANDLE install) {
    if (cloud_cleanup::profiles(ID_DATA_DIRECTORY)) return ERROR_SUCCESS;
    MSIHANDLE record = MsiCreateRecord(0);
    MsiRecordSetStringW(record, 0, L"Cloud file cache cleanup failed. Close applications using shared files and retry uninstall.");
    MsiProcessMessage(install, INSTALLMESSAGE_ERROR, record);
    MsiCloseHandle(record);
    return ERROR_INSTALL_FAILURE;
}
