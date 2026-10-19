#pragma once
#include <bcrypt.h>
#include <sstream>
#include <iomanip>
#include "Authorization.h"

namespace maintenance {
inline std::string narrow(const std::wstring& value) {
    std::string result;
    for (wchar_t character : value) { if (character > 127) return {}; result.push_back(static_cast<char>(character)); }
    return result;
}
inline std::wstring wide(const std::string& value) { return std::wstring(value.begin(), value.end()); }
inline std::wstring directory() { return root() + L"\\" ID_MAINTENANCE; }
inline std::wstring lock_path() { return directory() + L"\\" ID_MAINTENANCE_LOCK; }
inline std::wstring lease_path() { return directory() + L"\\installer-lease.json"; }
inline std::string hex64(uint64_t value) { std::ostringstream out; out << std::hex << std::setfill('0') << std::setw(16) << value; return out.str(); }
inline uint64_t created(HANDLE process) {
    FILETIME start{}, exit{}, kernel{}, user{};
    if (!GetProcessTimes(process, &start, &exit, &kernel, &user)) return 0;
    return (static_cast<uint64_t>(start.dwHighDateTime) << 32) | start.dwLowDateTime;
}

inline std::wstring image(HANDLE process) {
    wchar_t path[32768]; DWORD size = static_cast<DWORD>(std::size(path));
    return QueryFullProcessImageNameW(process, 0, path, &size) ? std::wstring(path, size) : std::wstring();
}

inline bool system_token(HANDLE token) {
    DWORD bytes = 0; GetTokenInformation(token, TokenUser, nullptr, 0, &bytes);
    std::vector<BYTE> data(bytes);
    bool ok = GetTokenInformation(token, TokenUser, data.data(), bytes, &bytes) != FALSE;
    PSID system = nullptr;
    if (ok) ok = ConvertStringSidToSidW(L"S-1-5-18", &system) &&
        EqualSid(reinterpret_cast<TOKEN_USER*>(data.data())->User.Sid, system);
    if (system) LocalFree(system);
    return ok;
}

inline bool system_process(HANDLE process) {
    HANDLE token = nullptr;
    if (!OpenProcessToken(process, TOKEN_QUERY, &token)) return false;
    bool ok = system_token(token);
    CloseHandle(token);
    return ok;
}

inline std::wstring system_image(const wchar_t* name) {
    wchar_t path[MAX_PATH]; UINT size = GetSystemDirectoryW(path, MAX_PATH);
    return size && size < MAX_PATH ? std::wstring(path) + L"\\" + name : std::wstring();
}

inline bool lock_handle(HANDLE handle) {
    if (GetFileType(handle) != FILE_TYPE_DISK) return false;
    wchar_t path[32768];
    DWORD size = GetFinalPathNameByHandleW(handle, path, static_cast<DWORD>(std::size(path)), FILE_NAME_NORMALIZED);
    if (!size || size >= std::size(path)) return false;
    std::wstring value(path, size);
    if (value.rfind(L"\\\\?\\", 0) == 0) value.erase(0, 4);
    BY_HANDLE_FILE_INFORMATION info{};
    return _wcsicmp(value.c_str(), lock_path().c_str()) == 0 && GetFileInformationByHandle(handle, &info) &&
        !(info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY)) && info.nNumberOfLinks == 1;
}

inline std::string sha256(const std::wstring& path) {
    HANDLE file = CreateFileW(path.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING,
        FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN, nullptr);
    if (file == INVALID_HANDLE_VALUE) return {};
    BY_HANDLE_FILE_INFORMATION info{};
    if (!GetFileInformationByHandle(file, &info) || (info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY))) {
        CloseHandle(file); return {};
    }
    BCRYPT_ALG_HANDLE algorithm = nullptr; BCRYPT_HASH_HANDLE hash = nullptr;
    DWORD length = 0, received = 0;
    bool ok = BCryptOpenAlgorithmProvider(&algorithm, BCRYPT_SHA256_ALGORITHM, nullptr, 0) == 0 &&
        BCryptGetProperty(algorithm, BCRYPT_OBJECT_LENGTH, reinterpret_cast<PUCHAR>(&length), sizeof(length), &received, 0) == 0;
    std::vector<BYTE> object(length);
    if (ok) ok = BCryptCreateHash(algorithm, &hash, object.data(), length, nullptr, 0, 0) == 0;
    BYTE buffer[65536], digest[32];
    while (ok) {
        DWORD count = 0;
        if (!ReadFile(file, buffer, sizeof(buffer), &count, nullptr)) { ok = false; break; }
        if (!count) break;
        ok = BCryptHashData(hash, buffer, count, 0) == 0;
    }
    if (ok) ok = BCryptFinishHash(hash, digest, sizeof(digest), 0) == 0;
    if (hash) BCryptDestroyHash(hash);
    if (algorithm) BCryptCloseAlgorithmProvider(algorithm, 0);
    CloseHandle(file);
    if (!ok) return {};
    std::ostringstream result;
    for (BYTE byte : digest) result << std::hex << std::setfill('0') << std::setw(2) << static_cast<unsigned>(byte);
    return result.str();
}

inline bool read_record(const std::wstring& path, admission::Object& object) {
    if (!protected_path(path)) return false;
    HANDLE file = CreateFileW(path.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT, nullptr);
    if (file == INVALID_HANDLE_VALUE) return false;
    LARGE_INTEGER size{}; bool ok = GetFileSizeEx(file, &size) && size.QuadPart > 0 && size.QuadPart <= 8192;
    std::string text(ok ? static_cast<size_t>(size.QuadPart) : 0, '\0');
    DWORD count = 0;
    if (ok) ok = ReadFile(file, text.data(), static_cast<DWORD>(text.size()), &count, nullptr) && count == text.size();
    CloseHandle(file);
    return ok && admission::parse(text, object);
}

inline bool package_matches(const admission::Object& record, const std::string& code, const std::string& hash) {
    return (admission::guid(admission::get(record, "product_code")) == code && admission::get(record, "package_sha256") == hash) ||
        (admission::guid(admission::get(record, "remove_product_code")) == code && admission::get(record, "remove_package_sha256") == hash);
}

inline bool duplicate_lock(HANDLE process, const admission::Object& record) {
    uint64_t raw = 0;
    if (!admission::hex64(admission::get(record, "lock_handle"), raw) || !raw) return false;
    HANDLE duplicate = nullptr;
    if (!DuplicateHandle(process, reinterpret_cast<HANDLE>(static_cast<uintptr_t>(raw)), GetCurrentProcess(),
        &duplicate, 0, FALSE, DUPLICATE_SAME_ACCESS)) return false;
    bool valid = lock_handle(duplicate);
    CloseHandle(duplicate);
    return valid;
}

inline HANDLE worker_authorized(const std::string& code, const std::string& hash, const std::string& remove, const std::string& remove_hash) {
    admission::Object record;
    if (!read_record(directory() + L"\\msi-authorization.json", record) || !admission::worker_shape(record) ||
        !package_matches(record, code, hash) || admission::guid(admission::get(record, "remove_product_code")) != remove ||
        admission::get(record, "remove_package_sha256") != remove_hash) return nullptr;
    const auto pending = directory() + L"\\pending.json";
    if (!protected_path(pending) || sha256(pending) != admission::get(record, "pending_sha256")) return nullptr;
    uint64_t pid = 0, start = 0;
    if (!admission::unsigned_value(record, "worker_pid", pid) ||
        !admission::hex64(admission::get(record, "worker_created_filetime"), start)) return nullptr;
    HANDLE worker = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_DUP_HANDLE | SYNCHRONIZE, FALSE, static_cast<DWORD>(pid));
    if (!worker) return nullptr;
    const auto executable = directory() + L"\\" ID_COPIED_WORKER;
    bool ok = WaitForSingleObject(worker, 0) == WAIT_TIMEOUT && system_process(worker) && created(worker) == start &&
        _wcsicmp(image(worker).c_str(), executable.c_str()) == 0 && protected_path(executable) &&
        sha256(executable) == admission::get(record, "worker_sha256") && duplicate_lock(worker, record);
    HANDLE duplicate = nullptr; uint64_t raw = 0;
    if (ok) ok = admission::hex64(admission::get(record, "lock_handle"), raw) &&
        DuplicateHandle(worker, reinterpret_cast<HANDLE>(static_cast<uintptr_t>(raw)), GetCurrentProcess(), &duplicate, 0, FALSE, DUPLICATE_SAME_ACCESS);
    CloseHandle(worker);
    return ok ? duplicate : nullptr;
}

inline HANDLE guardian_process(const admission::Object& record) {
    uint64_t schema = 0, pid = 0, start = 0;
    if (record.size() != 9 || !admission::unsigned_value(record, "schema", schema) || schema != 1 ||
        !admission::unsigned_value(record, "guardian_pid", pid) || !pid || pid > UINT32_MAX ||
        !admission::hex64(admission::get(record, "guardian_created_filetime"), start) || !start ||
        !admission::hex(admission::get(record, "nonce"), 32)) return nullptr;
    HANDLE process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_DUP_HANDLE | SYNCHRONIZE, FALSE, static_cast<DWORD>(pid));
    if (!process) return nullptr;
    if (WaitForSingleObject(process, 0) != WAIT_TIMEOUT || !system_process(process) || created(process) != start ||
        _wcsicmp(image(process).c_str(), system_image(L"rundll32.exe").c_str()) != 0 || !duplicate_lock(process, record)) {
        CloseHandle(process); return nullptr;
    }
    return process;
}

inline bool nested_authorized(const std::string& code, const std::string& hash) {
    admission::Object record;
    // Only RemoveExistingProducts' old-product uninstall may reuse the lease.
    // The active product itself must never borrow an existing transaction.
    if (!read_record(lease_path(), record) || admission::guid(admission::get(record, "remove_product_code")) != code ||
        admission::get(record, "remove_package_sha256") != hash) return false;
    HANDLE guardian = guardian_process(record);
    if (!guardian) return false;
    CloseHandle(guardian);
    return true;
}

inline std::wstring event_name(const std::string& nonce, const wchar_t* suffix) {
    return ID_INSTALLER_EVENT_PREFIX + wide(nonce) + suffix;
}

inline bool write_lease(const std::string& text) {
    if (!protected_path(lease_path())) return false;
    HANDLE file = CreateFileW(lease_path().c_str(), GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS,
        FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH, nullptr);
    if (file == INVALID_HANDLE_VALUE) return false;
    DWORD count = 0;
    bool ok = WriteFile(file, text.data(), static_cast<DWORD>(text.size()), &count, nullptr) && count == text.size() && FlushFileBuffers(file);
    CloseHandle(file);
    return ok;
}

// Called by Windows' signed rundll32 as SYSTEM, with only explicitly inherited
// lock/events/engine-process handles. Its lifetime is independent of CA unloading.
inline void guard(const char* command) {
    if (!system_process(GetCurrentProcess())) return;
    std::istringstream args(command ? command : "");
    std::string lock_text, ready_text, release_text, engine_text, nonce, code, hash, remove, remove_hash, trailing;
    if (!(args >> lock_text >> ready_text >> release_text >> engine_text >> nonce >> code >> hash >> remove >> remove_hash) || args >> trailing) return;
    uint64_t lock_raw = 0, ready_raw = 0, release_raw = 0, engine_raw = 0;
    if (!admission::hex64(lock_text, lock_raw) || !admission::hex64(ready_text, ready_raw) ||
        !admission::hex64(release_text, release_raw) || !admission::hex64(engine_text, engine_raw) ||
        !admission::hex(nonce, 32) || admission::guid(code).empty() || !admission::hex(hash, 64)) return;
    HANDLE lock = reinterpret_cast<HANDLE>(static_cast<uintptr_t>(lock_raw));
    HANDLE ready = reinterpret_cast<HANDLE>(static_cast<uintptr_t>(ready_raw));
    HANDLE release = reinterpret_cast<HANDLE>(static_cast<uintptr_t>(release_raw));
    HANDLE engine = reinterpret_cast<HANDLE>(static_cast<uintptr_t>(engine_raw));
    if (!lock_handle(lock) || !system_process(engine) || _wcsicmp(image(engine).c_str(), system_image(L"msiexec.exe").c_str()) != 0) return;
    if (remove == "-") remove.clear();
    if (remove_hash == "-") remove_hash.clear();
    std::ostringstream record;
    record << "{\"schema\":1,\"guardian_pid\":" << GetCurrentProcessId()
           << ",\"guardian_created_filetime\":\"" << hex64(created(GetCurrentProcess()))
           << "\",\"lock_handle\":\"" << lock_text << "\",\"nonce\":\"" << nonce
           << "\",\"product_code\":\"" << code << "\",\"package_sha256\":\"" << hash
           << "\",\"remove_product_code\":\"" << remove << "\",\"remove_package_sha256\":\"" << remove_hash << "\"}";
    // Nine fields, counted explicitly by guardian_process.
    if (!write_lease(record.str())) { SetEvent(ready); return; }
    SetEvent(ready);
    HANDLE waits[] = {release, engine};
    WaitForMultipleObjects(2, waits, FALSE, INFINITE);
    DeleteFileW(lease_path().c_str());
    CloseHandle(lock);
}

inline HANDLE installer_engine() {
    SC_HANDLE manager = OpenSCManagerW(nullptr, nullptr, SC_MANAGER_CONNECT);
    if (!manager) return nullptr;
    SC_HANDLE service = OpenServiceW(manager, L"msiserver", SERVICE_QUERY_STATUS);
    SERVICE_STATUS_PROCESS status{}; DWORD bytes = 0;
    bool ok = service && QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, reinterpret_cast<BYTE*>(&status), sizeof(status), &bytes);
    if (service) CloseServiceHandle(service);
    CloseServiceHandle(manager);
    return ok && status.dwProcessId ? OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, TRUE, status.dwProcessId) : nullptr;
}

inline bool start_guard(HANDLE lock, const std::string& code, const std::string& hash,
                        const std::string& remove, const std::string& remove_hash) {
    BYTE random[16];
    if (BCryptGenRandom(nullptr, random, sizeof(random), BCRYPT_USE_SYSTEM_PREFERRED_RNG) != 0) return false;
    std::ostringstream token;
    for (BYTE byte : random) token << std::hex << std::setfill('0') << std::setw(2) << static_cast<unsigned>(byte);
    const auto nonce = token.str();
    PSECURITY_DESCRIPTOR sd = nullptr;
    if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(L"O:SYG:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)", SDDL_REVISION_1, &sd, nullptr)) return false;
    SECURITY_ATTRIBUTES attributes{sizeof(attributes), sd, TRUE};
    HANDLE ready = CreateEventW(&attributes, TRUE, FALSE, event_name(nonce, L".ready").c_str());
    HANDLE release = CreateEventW(&attributes, TRUE, FALSE, event_name(nonce, L".release").c_str());
    LocalFree(sd);
    HANDLE engine = installer_engine();
    HANDLE inherited[] = {lock, ready, release, engine};
    bool ok = ready && release && engine && SetHandleInformation(lock, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
    SIZE_T bytes = 0;
    InitializeProcThreadAttributeList(nullptr, 1, 0, &bytes);
    std::vector<BYTE> storage(bytes);
    auto list = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
    bool initialized = InitializeProcThreadAttributeList(list, 1, 0, &bytes) != FALSE;
    ok = ok && initialized && UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        inherited, sizeof(inherited), nullptr, nullptr);
    HMODULE module = nullptr; wchar_t path[32768];
    ok = ok && GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        reinterpret_cast<LPCWSTR>(&start_guard), &module) && GetModuleFileNameW(module, path, static_cast<DWORD>(std::size(path))) && protected_path(path);
    PROCESS_INFORMATION process{};
    if (ok) {
        auto command = L"\"" + system_image(L"rundll32.exe") + L"\" \"" + path + L"\",MaintenanceGuard " +
            wide(hex64(reinterpret_cast<uintptr_t>(lock)) + " " + hex64(reinterpret_cast<uintptr_t>(ready)) + " " +
                 hex64(reinterpret_cast<uintptr_t>(release)) + " " + hex64(reinterpret_cast<uintptr_t>(engine)) + " " +
                 nonce + " " + code + " " + hash + " " + (remove.empty() ? "-" : remove) + " " + (remove_hash.empty() ? "-" : remove_hash));
        STARTUPINFOEXW startup{}; startup.StartupInfo.cb = sizeof(startup); startup.lpAttributeList = list;
        wchar_t windows[MAX_PATH];
        ok = GetWindowsDirectoryW(windows, MAX_PATH) != 0;
        std::wstring environment = std::wstring(L"SystemRoot=") + windows + L'\0' + L"PATH=" + system_image(L"") + L'\0' + L'\0';
        if (ok) ok = CreateProcessW(system_image(L"rundll32.exe").c_str(), command.data(), nullptr, nullptr, TRUE,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT, environment.data(), directory().c_str(), &startup.StartupInfo, &process) != FALSE;
        if (ok) {
            HANDLE waits[] = {ready, process.hProcess};
            ok = WaitForMultipleObjects(2, waits, FALSE, 15000) == WAIT_OBJECT_0;
            admission::Object record;
            uint64_t pid = 0;
            ok = ok && WaitForSingleObject(process.hProcess, 0) == WAIT_TIMEOUT && read_record(lease_path(), record) &&
                admission::unsigned_value(record, "guardian_pid", pid) && pid == process.dwProcessId &&
                admission::get(record, "nonce") == nonce && admission::get(record, "guardian_created_filetime") == hex64(created(process.hProcess)) &&
                package_matches(record, code, hash) && duplicate_lock(process.hProcess, record);
            if (!ok) { SetEvent(release); WaitForSingleObject(process.hProcess, 15000); }
        }
    }
    if (process.hThread) CloseHandle(process.hThread);
    if (process.hProcess) CloseHandle(process.hProcess);
    if (initialized) DeleteProcThreadAttributeList(list);
    if (ready) CloseHandle(ready);
    if (release) CloseHandle(release);
    if (engine) CloseHandle(engine);
    SetHandleInformation(lock, HANDLE_FLAG_INHERIT, 0);
    return ok;
}

inline bool ensure_directory(const std::wstring& path, const wchar_t* acl) {
    if (!protected_path(path)) return false;
    PSECURITY_DESCRIPTOR sd = nullptr;
    if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(acl, SDDL_REVISION_1, &sd, nullptr)) return false;
    SECURITY_ATTRIBUTES attributes{sizeof(attributes), sd, FALSE};
    bool ok = CreateDirectoryW(path.c_str(), &attributes) || GetLastError() == ERROR_ALREADY_EXISTS;
    LocalFree(sd);
    return ok && protected_path(path);
}

inline bool admit(const std::string& code, const std::string& hash, const std::string& remove, const std::string& remove_hash, bool system_caller) {
    if (!system_process(GetCurrentProcess()) || !ensure_directory(root(), L"O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)") ||
        !ensure_directory(directory(), L"O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")) return false;
    PSECURITY_DESCRIPTOR sd = nullptr;
    if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(L"O:SYG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)", SDDL_REVISION_1, &sd, nullptr)) return false;
    SECURITY_ATTRIBUTES attributes{sizeof(attributes), sd, FALSE};
    HANDLE lock = CreateFileW(lock_path().c_str(), GENERIC_READ | GENERIC_WRITE, 0, &attributes,
        OPEN_ALWAYS, FILE_FLAG_OPEN_REPARSE_POINT, nullptr);
    DWORD error = GetLastError();
    LocalFree(sd);
    if (lock == INVALID_HANDLE_VALUE) {
        if (error != ERROR_SHARING_VIOLATION) return false;
        if (nested_authorized(code, hash)) return true;
        if (!system_caller) return false;
        HANDLE worker_lock = worker_authorized(code, hash, remove, remove_hash);
        if (!worker_lock) return false;
        // Hold a duplicate through the MSI transaction even if the worker crashes.
        // The worker's original handle stays held across its later health check.
        bool ok = start_guard(worker_lock, code, hash, remove, remove_hash);
        CloseHandle(worker_lock);
        return ok;
    }
    // Owning the lock and finding pending recovery means the worker died; only
    // trusted recovery can resolve it. Never treat a stale journal as approval.
    DWORD pending = GetFileAttributesW((directory() + L"\\pending.json").c_str());
    DWORD pending_error = GetLastError();
    bool ok = lock_handle(lock) && pending == INVALID_FILE_ATTRIBUTES && pending_error == ERROR_FILE_NOT_FOUND;
    if (ok) ok = start_guard(lock, code, hash, remove, remove_hash);
    CloseHandle(lock); // Guardian has inherited the same open file description.
    return ok;
}

inline bool release() {
    admission::Object record;
    if (!read_record(lease_path(), record)) return true;
    if (admission::guid(admission::get(record, "product_code")) != admission::guid(narrow(ID_PRODUCT_CODE))) return true;
    HANDLE process = guardian_process(record);
    if (!process) return false;
    HANDLE event = OpenEventW(EVENT_MODIFY_STATE, FALSE, event_name(admission::get(record, "nonce"), L".release").c_str());
    bool ok = event && SetEvent(event) && WaitForSingleObject(process, 15000) == WAIT_OBJECT_0;
    if (event) CloseHandle(event);
    CloseHandle(process);
    return ok;
}
}
