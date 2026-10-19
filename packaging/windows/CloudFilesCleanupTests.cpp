#include <windows.h>
#include <cassert>
#include <filesystem>
#include <cstdio>
#include "CloudFilesCleanup.h"

static unsigned unregister_calls = 0;
static HRESULT WINAPI ordinary_directory(LPCWSTR) {
    ++unregister_calls;
    return HRESULT_FROM_WIN32(ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT);
}
static HRESULT WINAPI denied(LPCWSTR) { return E_ACCESSDENIED; }

int wmain() {
    wchar_t temp[32768]; assert(GetTempPathW(32768, temp));
    // The machine's TEMP can use short names or traverse a junction. Place the
    // fixture at its final DOS path; production must still reject junctions.
    HANDLE directory = CreateFileW(temp, FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    assert(directory != INVALID_HANDLE_VALUE);
    wchar_t resolved[32768];
    const DWORD length = GetFinalPathNameByHandleW(directory, resolved, 32768, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS);
    CloseHandle(directory);
    assert(length > 7 && length < 32768 && wcsncmp(resolved, L"\\\\?\\", 4) == 0 && resolved[5] == L':');
    const auto base = std::wstring(resolved + 4) + L"\\oink-cloud-cleanup-" + std::to_wstring(GetCurrentProcessId());
    const auto path = base + L"\\CloudFiles";
    std::filesystem::create_directories(path + L"\\nested");
    // Diagnose host Cloud Filter availability without making this real-file
    // unit test depend on that optional OS service. No root is registered here.
    printf("Plain-directory Cloud Filter probe HRESULT=0x%08lx\n",
        static_cast<unsigned long>(cloud_cleanup::unregister_root(path.c_str())));
    HANDLE file = CreateFileW((path + L"\\nested\\data").c_str(), GENERIC_WRITE, 0, nullptr, CREATE_NEW, 0, nullptr);
    assert(file != INVALID_HANDLE_VALUE); CloseHandle(file);
    // A live non-delete-sharing handle must leave the root intact for retry.
    file = CreateFileW((path + L"\\nested\\data").c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING, 0, nullptr);
    assert(file != INVALID_HANDLE_VALUE);
    // These are real NTFS handle/containment tests, not Cloud Filter driver
    // integration tests. A plain fixture is not a registered sync root. Server
    // builders need not provide a working Cloud Files provider/filter service.
    assert(!cloud_cleanup::root(path, ordinary_directory));
    assert(std::filesystem::exists(path));
    CloseHandle(file);
    assert(!cloud_cleanup::root(path, denied));
    assert(std::filesystem::exists(path + L"\\nested\\data"));
    assert(cloud_cleanup::root(path, ordinary_directory));
    assert(!std::filesystem::exists(path));
    assert(cloud_cleanup::root(path, ordinary_directory));
    // Ancestor reparse points cannot redirect privileged cleanup. Creating an
    // NTFS junction needs no privilege and does not modify any system settings.
    const auto target = base + L"\\outside";
    std::filesystem::create_directories(target + L"\\CloudFiles");
    const auto link = base + L"\\link";
    auto command = L"cmd.exe /c mklink /J \"" + link + L"\" \"" + target + L"\"";
    STARTUPINFOW startup{sizeof(startup)}; PROCESS_INFORMATION process{};
    assert(CreateProcessW(nullptr, command.data(), nullptr, nullptr, FALSE, CREATE_NO_WINDOW, nullptr, nullptr, &startup, &process));
    assert(WaitForSingleObject(process.hProcess, 10000) == WAIT_OBJECT_0);
    DWORD exit = 1; assert(GetExitCodeProcess(process.hProcess, &exit) && exit == 0); CloseHandle(process.hThread); CloseHandle(process.hProcess);
    const unsigned before = unregister_calls;
    assert(!cloud_cleanup::root(link + L"\\CloudFiles", ordinary_directory));
    assert(unregister_calls == before);
    assert(std::filesystem::exists(target + L"\\CloudFiles"));
    std::filesystem::remove_all(base);
    return 0;
}
