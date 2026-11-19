#include "RecoveryPolicy.h"
#include "RecoveryTree.h"
#include <fstream>
#include <filesystem>
#include <winioctl.h>
#include <vector>
#include <cstring>
#include "Authorization.h"
#include <cassert>
#include <iostream>
static void junction(const std::filesystem::path& link, const std::filesystem::path& target) {
    std::filesystem::create_directory(link);
    HANDLE handle = CreateFileW(link.c_str(), GENERIC_WRITE, 0, nullptr, OPEN_EXISTING,
        FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    assert(handle != INVALID_HANDLE_VALUE);
    struct MountPoint { DWORD tag; WORD length, reserved, sub_offset, sub_length, print_offset, print_length; wchar_t path[2048]; } data{};
    const auto native = std::wstring(L"\\??\\") + target.wstring();
    const auto print = target.wstring();
    data.tag = IO_REPARSE_TAG_MOUNT_POINT;
    data.sub_length = static_cast<WORD>(native.size() * sizeof(wchar_t));
    data.print_offset = static_cast<WORD>(data.sub_length + sizeof(wchar_t));
    data.print_length = static_cast<WORD>(print.size() * sizeof(wchar_t));
    const size_t bytes = data.print_offset + data.print_length + sizeof(wchar_t);
    assert(bytes <= sizeof(data.path));
    memcpy(data.path, native.c_str(), data.sub_length);
    memcpy(reinterpret_cast<BYTE*>(data.path) + data.print_offset, print.c_str(), data.print_length);
    data.length = static_cast<WORD>(8 + bytes);
    DWORD returned = 0;
    assert(DeviceIoControl(handle, FSCTL_SET_REPARSE_POINT, &data, 8 + data.length, nullptr, 0, &returned, nullptr));
    CloseHandle(handle);
}
int main() {
    using namespace recovery;
    unsigned checks = 0;
    for (const auto name : {L".", L"..", L".. ", L"cache.", L"cache ", L"a:b", L"a\\b", L"a/b"}) {
        assert(!literal_entry(name)); ++checks;
    }
    assert(literal_entry(L"plan.json") && literal_entry(L"maintenance")); checks += 2;
    admission::Object journal;
    const auto text = std::string("{\"schema\":1,\"transaction\":\"") + std::string(32, 'a') +
        "\",\"oldBuild\":1,\"newBuild\":2,\"packageSha256\":\"" + std::string(64, 'b') +
        "\",\"stage\":\"committed\",\"rollbackAttempted\":false,\"failure\":null}";
    assert(admission::parse(text, journal, true) && terminal_journal(journal));
    auto changed = journal; changed["stage"].text = "rolled_back"; assert(terminal_journal(changed));
    changed = journal; changed["stage"].text = "prepared"; assert(!terminal_journal(changed));
    changed = journal; changed["schema"].text = "2"; assert(!terminal_journal(changed));
    changed = journal; changed["newBuild"].text = "0"; assert(!terminal_journal(changed));
    changed = journal; changed["extension"] = {}; assert(!terminal_journal(changed));
    changed = journal; changed.erase("packageSha256"); assert(!terminal_journal(changed));
    changed = journal; changed["oldBuild"].text = "0"; assert(!terminal_journal(changed));
    changed = journal; changed["transaction"].text = "invalid"; assert(!terminal_journal(changed));
    changed = journal; changed["rollbackAttempted"].literal = false; assert(!terminal_journal(changed));
    checks += 10;
    for (bool untrusted : {false, true}) for (bool uninstall : {false, true})
    for (bool alive : {false, true}) for (bool terminal : {false, true}) {
        assert(disposition(true, untrusted, uninstall, alive, terminal) == Disposition::live);
        const bool safe = untrusted || uninstall || !alive || terminal;
        assert(disposition(false, untrusted, uninstall, alive, terminal) ==
            (safe ? Disposition::discard : Disposition::refuse));
        checks += 2;
    }
    const auto base = std::filesystem::temp_directory_path() / ("oink-recovery-" + std::to_string(GetCurrentProcessId()));
    { Boundary volume; assert(volume.pin(base.root_path().wstring())); ++checks; }
    std::filesystem::create_directory(base);
    std::filesystem::create_directories(base / "transactions" / "nested");
    std::ofstream(base / "transactions" / "nested" / "record") << "discard";
    std::ofstream(base / "maintenance.lock") << "retain";
    std::ofstream(base / "outside") << "untouched";
    const auto alias = base.wstring() + L"\\.. ";
    const auto literal_alias = std::wstring(L"\\\\?\\") + alias;
    assert(CreateDirectoryW(literal_alias.c_str(), nullptr));
    { Boundary ambiguous;
      if (ambiguous.pin(alias)) {
          wchar_t actual[32768]{};
          GetFinalPathNameByHandleW(ambiguous.handle, actual, 32768, FILE_NAME_NORMALIZED);
          std::wcerr << L"unsafe normalized alias accepted: " << actual << L'\n'; return 1;
      }
    }
    assert(RemoveDirectoryW(literal_alias.c_str()));
    auto validate = [](const std::wstring&) { return true; };
    HANDLE writer = CreateFileW(base.c_str(), GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    assert(writer != INVALID_HANDLE_VALUE);
    { Boundary contested; assert(!contested.pin(base.wstring(), true) && GetLastError() == ERROR_SHARING_VIOLATION); }
    CloseHandle(writer);
    Boundary pinned;
    assert(pinned.pin(base.wstring(), true));
    writer = CreateFileW(base.c_str(), GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    assert(writer == INVALID_HANDLE_VALUE && GetLastError() == ERROR_SHARING_VIOLATION);
    assert(!MoveFileW(base.c_str(), (base.wstring() + L"-moved").c_str()));
    // The production walk must inspect children through the injected validator.
    unsigned visited = 0;
    assert(!discard_contents((base / "transactions").wstring(), L"", [&](const std::wstring&) { ++visited; return false; }));
    assert(visited == 1 && std::filesystem::exists(base / "transactions" / "nested" / "record"));
    assert(discard_contents((base / "transactions").wstring(), L"", validate));
    assert(std::filesystem::is_empty(base / "transactions"));
    assert(std::filesystem::exists(base / "outside"));
    // Directory handles deliberately deny DELETE while the walk is active.
    CloseHandle(pinned.handle); pinned.handle = INVALID_HANDLE_VALUE;
    assert(discard_contents(base.wstring(), L"maintenance.lock", validate));
    assert(std::filesystem::exists(base / "maintenance.lock"));
    const auto outside = base.wstring() + L"-outside";
    std::filesystem::create_directory(outside);
    std::ofstream(std::filesystem::path(outside) / "keep") << "never follow";
    junction(base / "redirect", outside);
    { Boundary redirect; assert(!redirect.pin((base / "redirect").wstring())); }
    assert(!discard_contents(base.wstring(), L"maintenance.lock", validate));
    assert(std::filesystem::exists(std::filesystem::path(outside) / "keep"));
    assert(RemoveDirectoryW((base / "redirect").c_str()));
    std::filesystem::remove_all(base);
    std::filesystem::remove_all(outside);
    checks += 16;
    std::cout << "recovery admission/filesystem checks=" << checks << '\n';
}
