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

inline bool ensure_directory(const std::wstring& path, const wchar_t* acl) {
    if (!protected_path(path)) return false;
    PSECURITY_DESCRIPTOR sd = nullptr;
    if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(acl, SDDL_REVISION_1, &sd, nullptr)) return false;
    SECURITY_ATTRIBUTES attributes{sizeof(attributes), sd, FALSE};
    bool ok = CreateDirectoryW(path.c_str(), &attributes) || GetLastError() == ERROR_ALREADY_EXISTS;
    LocalFree(sd);
    return ok && protected_path(path);
}

inline bool absent(const std::wstring& path) {
    if (GetFileAttributesW(path.c_str()) != INVALID_FILE_ATTRIBUTES) return false;
    return GetLastError() == ERROR_FILE_NOT_FOUND;
}

// Windows Installer serializes MSI execution. The worker retains the exclusive
// maintenance lock through MSI, health and recovery; no guardian or lock copies
// are needed to defend against trusted administrators or SYSTEM.
inline bool admit(const std::string& code, const std::string& hash, const std::string& remove, const std::string& remove_hash, bool system_caller) {
    if (!ensure_directory(root(), L"O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)") ||
        !ensure_directory(directory(), L"O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")) return false;
    HANDLE lock = CreateFileW(lock_path().c_str(), GENERIC_READ | GENERIC_WRITE, 0, nullptr,
        OPEN_ALWAYS, FILE_FLAG_OPEN_REPARSE_POINT, nullptr);
    if (lock == INVALID_HANDLE_VALUE) {
        if (GetLastError() != ERROR_SHARING_VIOLATION || !system_caller) return false;
        admission::Object record;
        return read_record(directory() + L"\\msi-authorization.json", record) &&
            admission::matches(record, code, hash, remove, remove_hash);
    }
    // A standalone manual MSI has no worker transaction. A stale journal or
    // authorization must be resolved by recovery before a new install starts.
    bool ok = lock_handle(lock) && protected_handle(lock) &&
        absent(directory() + L"\\pending.json") && absent(directory() + L"\\msi-authorization.json");
    CloseHandle(lock);
    return ok;
}
}
