#pragma once
#include <windows.h>
#include <string>
#include <string_view>
#include <vector>

namespace embedded_powershell {
struct Handle {
    HANDLE value = nullptr;
    Handle() = default;
    explicit Handle(HANDLE handle) : value(handle) {}
    Handle(const Handle&) = delete;
    Handle& operator=(const Handle&) = delete;
    void close() { if (value && value != INVALID_HANDLE_VALUE) CloseHandle(value); value = nullptr; }
    ~Handle() { close(); }
};
struct Attributes {
    LPPROC_THREAD_ATTRIBUTE_LIST value = nullptr;
    ~Attributes() { if (value) DeleteProcThreadAttributeList(value); }
};

// The embedded cleanup exceeds CreateProcess's 32,767-character limit when
// base64 UTF-16 is put on its command line. Send exact UTF-8 through stdin; the
// fixed loader parses the complete script only after EOF. Inherit only the pipe
// and NUL handles, never unrelated handles belonging to the MSI host.
inline DWORD run(const std::wstring& executable, std::string_view script, const wchar_t* directory) {
    SECURITY_ATTRIBUTES security{sizeof(SECURITY_ATTRIBUTES), nullptr, TRUE};
    Handle input, writer;
    if (!CreatePipe(&input.value, &writer.value, &security, 0) ||
        !SetHandleInformation(writer.value, HANDLE_FLAG_INHERIT, 0)) return GetLastError();
    Handle null(CreateFileW(L"NUL", GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, &security,
                            OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr));
    if (null.value == INVALID_HANDLE_VALUE) return GetLastError();
    SIZE_T size = 0;
    InitializeProcThreadAttributeList(nullptr, 1, 0, &size);
    if (!size) return GetLastError();
    std::vector<BYTE> storage(size);
    Attributes attributes;
    auto list = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
    if (!InitializeProcThreadAttributeList(list, 1, 0, &size)) return GetLastError();
    attributes.value = list;
    HANDLE inherited[]{input.value, null.value};
    if (!UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                                   inherited, sizeof(inherited), nullptr, nullptr)) return GetLastError();
    STARTUPINFOEXW startup{};
    startup.StartupInfo.cb = sizeof(startup);
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = input.value;
    startup.StartupInfo.hStdOutput = startup.StartupInfo.hStdError = null.value;
    startup.lpAttributeList = list;
    std::wstring command = L"\"" + executable + L"\" -NoLogo -NoProfile -NonInteractive -Command "
        L"\"[Console]::InputEncoding=[Text.Encoding]::UTF8; & ([scriptblock]::Create([Console]::In.ReadToEnd()))\"";
    PROCESS_INFORMATION process{};
    if (!CreateProcessW(executable.c_str(), command.data(), nullptr, nullptr, TRUE,
                        CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT, nullptr, directory,
                        &startup.StartupInfo, &process)) return GetLastError();
    Handle child(process.hProcess), thread(process.hThread);
    input.close();
    while (!script.empty()) {
        DWORD written = 0;
        DWORD count = static_cast<DWORD>(script.size() > 4096 ? 4096 : script.size());
        if (!WriteFile(writer.value, script.data(), count, &written, nullptr) || !written) {
            DWORD error = GetLastError();
            TerminateProcess(child.value, 1);
            WaitForSingleObject(child.value, INFINITE);
            return error ? error : ERROR_WRITE_FAULT;
        }
        script.remove_prefix(written);
    }
    writer.close();
    if (WaitForSingleObject(child.value, INFINITE) != WAIT_OBJECT_0) return GetLastError();
    DWORD code = 1;
    if (!GetExitCodeProcess(child.value, &code)) return GetLastError();
    return code;
}
}
