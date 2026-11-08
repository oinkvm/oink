#pragma once
#ifndef UNICODE
#define UNICODE
#define _UNICODE
#endif
#define WIN32_LEAN_AND_MEAN
#include <aclapi.h>
#include <cstdint>
#include <sddl.h>
#include <string>
#include <vector>
#include <windows.h>
#include <wtsapi32.h>
#pragma comment(lib, "advapi32.lib")
#pragma comment(lib, "wtsapi32.lib")
inline constexpr wchar_t PipeName[] = L"\\\\.\\pipe\\Oink.AutoUnlock.v1";
inline constexpr wchar_t ProviderId[] =
    L"{361D74A8-F854-44FD-96B4-CAD7BFB51E09}";
inline const CLSID CLSID_Oink = {
    0x361d74a8,
    0xf854,
    0x44fd,
    {0x96, 0xb4, 0xca, 0xd7, 0xbf, 0xb5, 0x1e, 0x09}};
enum Op : DWORD {
  Status = 1,
  Store = 2,
  Disable = 3,
  Trigger = 4,
  Poll = 5,
  Acquire = 6,
  Outcome = 7,
  // Input thread notifications are authenticated by the local Oink backend.
  RemoteInput = 8
};
// Stable, display-free failure categories carried in Reply::reserved. The
// value is local to this machine and never crosses the device-group protocol.
enum FailureKind : DWORD {
  FailureNone = 0,
  FailureIncorrectPassword = 1,
  FailureWindowsHelloOnly = 2,
  FailureLoginUiNotReady = 3,
  FailureUnknown = 4,
};
struct Config {
  wchar_t user[128]{}, domain[128]{}, sid[192]{};
};
struct Request {
  DWORD magic = 0x554b4e4f, op = 0;
  uint64_t id = 0;
  DWORD status = 0, reserved = 0;
  wchar_t password[512]{};
};
struct Reply {
  DWORD error = ERROR_ACCESS_DENIED, present = 0, state = 0,
        reserved = FailureNone;
  uint64_t id = 0;
  Config target{};
  wchar_t password[512]{};
  // Monotonic provider-readiness evidence. These values are numeric only and
  // are exposed to the local diagnostics snapshot, never to the wire protocol.
  uint64_t ready_at = 0, remote_input_at = 0, remote_input_source = 0;
  DWORD decision = 0;
};
static_assert(sizeof(Request) == 1048 && sizeof(Reply) == 1976);
struct Handle {
  HANDLE h = nullptr;
  explicit Handle(HANDLE v = nullptr) : h(v) {}
  ~Handle() {
    if (h && h != INVALID_HANDLE_VALUE)
      CloseHandle(h);
  }
  Handle(const Handle &) = delete;
};
inline DWORD session() {
  DWORD s = MAXDWORD;
  ProcessIdToSessionId(GetCurrentProcessId(), &s);
  return s;
}
inline bool systemToken(HANDLE token) {
  BYTE b[512];
  DWORD n = 0;
  return GetTokenInformation(token, TokenUser, b, sizeof(b), &n) &&
         IsWellKnownSid(reinterpret_cast<TOKEN_USER *>(b)->User.Sid,
                        WinLocalSystemSid);
}
inline bool system() {
  Handle t;
  return OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &t.h) &&
         systemToken(t.h);
}
inline bool image(HANDLE process, const std::wstring &expected) {
  wchar_t n[32768];
  DWORD size = 32768;
  return QueryFullProcessImageNameW(process, 0, n, &size) &&
         !_wcsicmp(n, expected.c_str());
}
inline std::wstring windowsPath(const wchar_t *suffix) {
  wchar_t dir[MAX_PATH];
  UINT n = GetWindowsDirectoryW(dir, MAX_PATH);
  return n && n < MAX_PATH ? std::wstring(dir) + suffix : L"";
}
// Bind every connection to SCM before sending even the first password byte.
inline bool serverVerified(HANDLE pipe) {
  ULONG pid = 0, sid = MAXDWORD;
  if (!GetNamedPipeServerProcessId(pipe, &pid) ||
      !GetNamedPipeServerSessionId(pipe, &sid) || sid != 0)
    return false;
  SC_HANDLE scm = OpenSCManagerW(nullptr, nullptr, SC_MANAGER_CONNECT),
            svc = scm ? OpenServiceW(scm, L"OinkInput", SERVICE_QUERY_STATUS)
                      : nullptr;
  SERVICE_STATUS_PROCESS state{};
  DWORD size = 0;
  bool ok = svc &&
            QueryServiceStatusEx(svc, SC_STATUS_PROCESS_INFO,
                                 reinterpret_cast<BYTE *>(&state),
                                 sizeof(state), &size) &&
            state.dwCurrentState == SERVICE_RUNNING && state.dwProcessId == pid;
  if (svc)
    CloseServiceHandle(svc);
  if (scm)
    CloseServiceHandle(scm);
  PSID owner = nullptr;
  PACL acl = nullptr;
  PSECURITY_DESCRIPTOR sd = nullptr;
  if (GetSecurityInfo(pipe, SE_KERNEL_OBJECT,
                      OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                      &owner, nullptr, &acl, nullptr, &sd))
    return false;
  ok = ok && owner && IsWellKnownSid(owner, WinLocalSystemSid) && acl &&
       acl->AceCount;
  for (DWORD i = 0; ok && i < acl->AceCount; i++) {
    void *p = nullptr;
    ok = GetAce(acl, i, &p) != FALSE;
    if (!ok)
      break;
    auto a = static_cast<ACCESS_ALLOWED_ACE *>(p);
    ok = a->Header.AceType == ACCESS_ALLOWED_ACE_TYPE &&
         (IsWellKnownSid(&a->SidStart, WinLocalSystemSid) ||
          !(a->Mask & (GENERIC_ALL | GENERIC_WRITE | FILE_CREATE_PIPE_INSTANCE |
                       WRITE_DAC | WRITE_OWNER | DELETE)));
  }
  if (sd)
    LocalFree(sd);
  return ok;
}
// NOWAIT read and write are bounded; only establishment may retry. Never
// resend.
inline bool rpc(Request &q, Reply &r) {
  Handle h;
  auto end = GetTickCount64() + 3000;
  do {
    h.h = CreateFileW(PipeName, 0x12019b, 0, nullptr, OPEN_EXISTING,
                      SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION, nullptr);
    if (h.h != INVALID_HANDLE_VALUE)
      break;
    Sleep(10);
  } while (GetTickCount64() < end);
  if (h.h == INVALID_HANDLE_VALUE || !serverVerified(h.h))
    return false;
  DWORD mode = PIPE_READMODE_MESSAGE | PIPE_NOWAIT, n = 0;
  if (!SetNamedPipeHandleState(h.h, &mode, nullptr, nullptr))
    return false;
  if (!WriteFile(h.h, &q, sizeof(q), &n, nullptr) || n != sizeof(q))
    return false;
  do {
    if (ReadFile(h.h, &r, sizeof(r), &n, nullptr))
      return n == sizeof(r);
    if (GetLastError() != ERROR_NO_DATA)
      return false;
    Sleep(10);
  } while (GetTickCount64() < end);
  return false;
}
