// Native filesystem tests use only unique temporary directories, never an
// installed product. Production's privileged ACL predicate is supplied by CA.
#include "MaintenanceCleanup.h"
#include <cassert>
#include <iostream>

static bool regular(HANDLE handle) {
    BY_HANDLE_FILE_INFORMATION info{};
    return GetFileInformationByHandle(handle, &info) && !(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) &&
        ((info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) || info.nNumberOfLinks == 1);
}

static bool regular_path(const std::wstring& path) {
    HANDLE handle = CreateFileW(path.c_str(), FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        nullptr, OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    if (handle == INVALID_HANDLE_VALUE) return cleanup::absent(path);
    bool ok = regular(handle);
    CloseHandle(handle);
    return ok;
}

static void put(const std::wstring& path) {
    HANDLE file = CreateFileW(path.c_str(), GENERIC_WRITE, 0, nullptr, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, nullptr);
    assert(file != INVALID_HANDLE_VALUE);
    DWORD written = 0;
    assert(WriteFile(file, "retained", 8, &written, nullptr) && written == 8);
    CloseHandle(file);
}

static bool retained(const std::wstring& path) {
    HANDLE file = CreateFileW(path.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING, 0, nullptr);
    if (file == INVALID_HANDLE_VALUE) return false;
    char bytes[8]; DWORD count = 0;
    bool ok = ReadFile(file, bytes, sizeof(bytes), &count, nullptr) && count == sizeof(bytes) &&
        std::string(bytes, count) == "retained";
    CloseHandle(file);
    return ok;
}

int main() {
    wchar_t temporary[MAX_PATH];
    assert(GetTempPathW(MAX_PATH, temporary));
    const auto base = std::wstring(temporary) + L"oink-uninstall-unit-" + std::to_wstring(GetCurrentProcessId()) +
        L"-" + std::to_wstring(GetTickCount64());
    assert(CreateDirectoryW(base.c_str(), nullptr));
    put(base + L"\\user-data");
    unsigned checks = 0;
    for (const auto marker : {L"", L"pending.json", L"msi-authorization.json", L"worker-lock", L"hardlink"}) {
        const auto root = base + L"\\product";
        const auto directory = root + L"\\maintenance";
        const auto lock_path = directory + L"\\maintenance.lock";
        assert(CreateDirectoryW(root.c_str(), nullptr));
        assert(CreateDirectoryW(directory.c_str(), nullptr));
        put(lock_path);
        put(directory + L"\\policy.json");
        assert(CreateDirectoryW((directory + L"\\transactions").c_str(), nullptr));
        put(directory + L"\\transactions\\cached-msi");
        HANDLE worker = INVALID_HANDLE_VALUE;
        const std::wstring name(marker);
        if (name == L"worker-lock") {
            worker = CreateFileW(lock_path.c_str(), GENERIC_READ | GENERIC_WRITE, 0, nullptr, OPEN_EXISTING, 0, nullptr);
            assert(worker != INVALID_HANDLE_VALUE);
        } else if (name == L"hardlink") {
            assert(DeleteFileW(lock_path.c_str()));
            assert(CreateHardLinkW(lock_path.c_str(), (base + L"\\user-data").c_str(), nullptr));
        } else if (!name.empty()) put(directory + L"\\" + name);
        // A rollback's journal/authorization preserves data/registrations; an
        // ordinary standalone uninstall removes them.
        assert(cleanup::preserve_user_state(directory) == (name == L"pending.json" || name == L"msi-authorization.json"));
        const auto result = cleanup::after_uninstall(root, directory, L"maintenance.lock", regular_path, regular);
        if (name.empty()) {
            assert(result == cleanup::Result::removed && cleanup::absent(root));
        } else {
            assert(result == (name == L"hardlink" ? cleanup::Result::failed : cleanup::Result::preserved));
            assert(retained(directory + L"\\policy.json") && retained(directory + L"\\transactions\\cached-msi"));
            if (worker != INVALID_HANDLE_VALUE) CloseHandle(worker);
            else if (name == L"hardlink") { assert(DeleteFileW(lock_path.c_str())); put(lock_path); }
            else assert(DeleteFileW((directory + L"\\" + name).c_str()));
            assert(!cleanup::preserve_user_state(directory));
            assert(cleanup::after_uninstall(root, directory, L"maintenance.lock", regular_path, regular) == cleanup::Result::removed);
        }
        assert(retained(base + L"\\user-data"));
        ++checks;
    }
    assert(cleanup::after_uninstall(base + L"\\product", base + L"\\product\\maintenance", L"maintenance.lock",
        regular_path, regular) == cleanup::Result::removed);
    ++checks;
    assert(DeleteFileW((base + L"\\user-data").c_str()) && RemoveDirectoryW(base.c_str()));
    std::cout << "maintenance_cleanup_native_cases=" << checks << '\n';
}
