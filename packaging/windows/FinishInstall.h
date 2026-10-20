#pragma once
#include <windows.h>
#include <string>
#include <cstring>

namespace finish_install {
inline bool allowed(const std::wstring& ui, bool installed, bool removing, bool scheduled, bool commit, bool rollback) {
    return ui == L"5" && !installed && !removing && !scheduled && !commit && !rollback;
}

inline bool normal_flags(DWORD uiaccess, DWORD session, bool service, DWORD expected_session = MAXDWORD) {
    return !uiaccess && !service && session && (expected_session == MAXDWORD || expected_session == session);
}
inline bool service_account(PSID sid) {
    const SID_IDENTIFIER_AUTHORITY nt = SECURITY_NT_AUTHORITY;
    if (memcmp(GetSidIdentifierAuthority(sid), &nt, sizeof(nt)) != 0 || !*GetSidSubAuthorityCount(sid)) return false;
    const DWORD rid = *GetSidSubAuthority(sid, 0);
    return rid == SECURITY_SERVICE_ID_BASE_RID || rid == 82 ||
        (*GetSidSubAuthorityCount(sid) == 1 && (rid == SECURITY_LOCAL_SYSTEM_RID ||
            rid == SECURITY_LOCAL_SERVICE_RID || rid == SECURITY_NETWORK_SERVICE_RID));
}
inline bool normal_token(HANDLE token, DWORD& session, DWORD expected_session = MAXDWORD) {
    DWORD uiaccess = 0, size = 0;
    alignas(TOKEN_USER) BYTE user[sizeof(TOKEN_USER) + SECURITY_MAX_SID_SIZE]{};
    // Elevation is supported both for explicit administrator launches and for
    // the user's sole token with UAC disabled. Never host WebView2 as a service;
    // UIAccess is reserved for the input backend, not the WebView shell.
    return GetTokenInformation(token, TokenUser, user, sizeof(user), &size) &&
        GetTokenInformation(token, TokenSessionId, &session, sizeof(session), &size) &&
        GetTokenInformation(token, TokenUIAccess, &uiaccess, sizeof(uiaccess), &size) &&
        normal_flags(uiaccess, session, service_account(reinterpret_cast<TOKEN_USER*>(user)->User.Sid), expected_session);
}
inline bool normal_user(HANDLE process, DWORD expected_session = MAXDWORD) {
    HANDLE token = nullptr;
    if (!OpenProcessToken(process, TOKEN_QUERY, &token)) return false;
    DWORD session = 0;
    const bool ok = normal_token(token, session, expected_session);
    CloseHandle(token);
    return ok;
}

// Foreground permission is an optional focus hint, not permission to run the
// application. Windows may deny it even for an explicit launch (for example,
// an MSI DLL custom-action host did not receive the Finish button's input).
// Keep the normal-token check mandatory; a denied hint must not kill its child.
template<class Grant, class Resume>
inline bool resume_child(bool normal, bool launch, Grant grant, Resume resume) {
    if (!normal) return false;
    if (launch) grant();
    return resume();
}

inline bool helper_completed(DWORD wait, bool queried, DWORD exit) {
    return wait == WAIT_OBJECT_0 && queried && exit == ERROR_SUCCESS;
}

// The client UI keeps its original token across UAC; never create this process
// in a deferred/server CA. Check the real suspended child's token before it can
// run login registration or any app code. An installer explicitly started as
// administrator is allowed; the caller's elevation is an explicit user choice.
inline bool launch(const std::wstring& exe, const std::wstring& arguments, const std::wstring& directory,
                   bool* foreground_granted = nullptr) {
    DWORD session = 0;
    if (foreground_granted) *foreground_granted = false;
    if (!ProcessIdToSessionId(GetCurrentProcessId(), &session) || !normal_user(GetCurrentProcess())) {
        SetLastError(ERROR_ACCESS_DENIED);
        return false;
    }
    std::wstring command = L"\"" + exe + L"\" " + arguments;
    STARTUPINFOW startup{}; startup.cb = sizeof(startup);
    PROCESS_INFORMATION child{};
    if (!CreateProcessW(exe.c_str(), command.data(), nullptr, nullptr, FALSE, CREATE_SUSPENDED,
                        nullptr, directory.c_str(), &startup, &child)) return false;
    bool ok = resume_child(normal_user(child.hProcess, session), arguments.find(L"--launch") != std::wstring::npos,
        [&] {
            const bool granted = AllowSetForegroundWindow(child.dwProcessId) != FALSE;
            if (foreground_granted) *foreground_granted = granted;
        }, [&] { return ResumeThread(child.hThread) != MAXDWORD; });
    DWORD error = ok ? ERROR_SUCCESS : ERROR_ACCESS_DENIED;
    // Only a child which has never run may be discarded after a token/resume
    // failure. A denied foreground grant is never a termination condition.
    if (!ok) TerminateProcess(child.hProcess, ERROR_ACCESS_DENIED);
    if (ok) {
        // This is the short-lived --installer-finish helper, not the UI's
        // lifetime. Observe its login/launch result so post-spawn failures are
        // reported instead of silently treating process creation as success.
        const DWORD wait = WaitForSingleObject(child.hProcess, 10000);
        DWORD exit = ERROR_GEN_FAILURE;
        const bool queried = wait == WAIT_OBJECT_0 && GetExitCodeProcess(child.hProcess, &exit);
        ok = helper_completed(wait, queried, exit);
        if (!ok) error = wait == WAIT_TIMEOUT ? ERROR_TIMEOUT : ERROR_GEN_FAILURE;
    }
    CloseHandle(child.hThread);
    CloseHandle(child.hProcess);
    SetLastError(error);
    return ok;
}
}
