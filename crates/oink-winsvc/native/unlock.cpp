#include "ipc.hpp"
#include "unlock-policy.hpp"
#include <algorithm>
#include <atomic>
#include <dpapi.h>
#include <memory>
#include <mutex>
#include <shlobj.h>
#pragma comment(lib, "crypt32.lib")
#pragma comment(lib, "shell32.lib")
#pragma comment(lib, "ole32.lib")
#pragma comment(lib, "user32.lib")
using Authenticate = HANDLE (*)(HANDLE, DWORD);
static std::mutex gate;
static UnlockOperation operation;
static std::atomic<bool> stopping{false};
// Unnamed events live until process exit: SCM callbacks may signal during stop.
static std::atomic<HANDLE> sessionChanged{nullptr};
static HANDLE stopEvent = nullptr;
static uint64_t sequence = 0;
static Config operationTarget{};
static HANDLE worker = nullptr;
static Authenticate backendAuth = nullptr;
static std::wstring root;
// The agent reports only after the source has answered UnlockQuery as
// unlocked. The service stamps receipt with GetTickCount64, so the readiness
// decision compares one monotonic clock and never trusts wall-clock input.
static uint64_t recentRemoteSource = 0, recentRemoteAt = 0;
// Latest sync unlock operation, reported to the agent once its window ends.
// Process-lifetime only: a sync operation never persists Pending, so a service
// restart simply forgets an unconsumed one (no secret was handed out).
enum SyncOutcome : DWORD {
  SyncNone = 0,
  SyncPending = 1,
  SyncSubmitted = 2,
  SyncSucceeded = 3,
  SyncFailed = 4,
  SyncExpired = 5
};
static uint64_t syncId = 0;
static SyncOutcome syncOutcome = SyncNone;
static bool syncPrepared = false;
static constexpr wchar_t OnlySystem[] = L"O:SYG:SYD:P(A;OICI;FA;;;SY)";
struct Descriptor {
  PSECURITY_DESCRIPTOR p = nullptr;
  explicit Descriptor(const wchar_t *s) {
    ConvertStringSecurityDescriptorToSecurityDescriptorW(s, 1, &p, nullptr);
  }
  ~Descriptor() {
    if (p)
      LocalFree(p);
  }
  SECURITY_ATTRIBUTES attrs() {
    return {sizeof(SECURITY_ATTRIBUTES), p, FALSE};
  }
};
static bool locked(DWORD id) {
  LPWSTR p = nullptr;
  DWORD bytes = 0;
  bool ok = false;
  if (WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, id,
                                  WTSSessionInfoEx, &p, &bytes)) {
    auto i = reinterpret_cast<WTSINFOEXW *>(p);
    ok = bytes >= sizeof(*i) && i->Level == 1 &&
         i->Data.WTSInfoExLevel1.SessionId == id &&
         i->Data.WTSInfoExLevel1.SessionFlags == WTS_SESSIONSTATE_LOCK;
    WTSFreeMemory(p);
  }
  return ok;
}
static bool privateHandle(HANDLE h) {
  PSID owner = nullptr;
  PACL acl = nullptr;
  PSECURITY_DESCRIPTOR sd = nullptr;
  if (GetSecurityInfo(h, SE_FILE_OBJECT,
                      OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                      &owner, nullptr, &acl, nullptr, &sd))
    return false;
  bool ok =
      owner && IsWellKnownSid(owner, WinLocalSystemSid) && acl && acl->AceCount;
  for (DWORD i = 0; ok && i < acl->AceCount; i++) {
    void *p = nullptr;
    ok = GetAce(acl, i, &p) != FALSE;
    if (ok) {
      auto a = static_cast<ACCESS_ALLOWED_ACE *>(p);
      ok = a->Header.AceType == ACCESS_ALLOWED_ACE_TYPE &&
           IsWellKnownSid(&a->SidStart, WinLocalSystemSid);
    }
  }
  FILE_ATTRIBUTE_TAG_INFO info{};
  ok = ok &&
       GetFileInformationByHandleEx(h, FileAttributeTagInfo, &info,
                                    sizeof(info)) &&
       !(info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT);
  if (sd)
    LocalFree(sd);
  return ok;
}
static bool target(DWORD id, Config &c) {
  Handle token;
  // This product unlocks an already signed-in console session only. Never use
  // the pre-login identity snapshot or a stored SID when no user token exists.
  // CPUS_LOGON is still accepted for locked sessions (Win10 LogonUI uses it).
  if (!WTSQueryUserToken(id, &token.h))
    return false;
  BYTE data[512];
  DWORD n = 0;
  if (!GetTokenInformation(token.h, TokenUser, data, sizeof(data), &n))
    return false;
  auto sid = reinterpret_cast<TOKEN_USER *>(data)->User.Sid;
  LPWSTR text = nullptr;
  if (!ConvertSidToStringSidW(sid, &text))
    return false;
  bool ok = wcslen(text) < 192;
  if (ok)
    wcscpy_s(c.sid, text);
  LocalFree(text);
  DWORD user = 128, domain = 128;
  SID_NAME_USE use;
  return ok &&
         LookupAccountSidW(nullptr, sid, c.user, &user, c.domain, &domain,
                           &use) &&
         use == SidTypeUser;
}
static std::wstring file(const Config &c, const wchar_t *suffix) {
  return root + L"\\" + c.sid + suffix;
}
static bool absent(const std::wstring &p) {
  DWORD a = GetFileAttributesW(p.c_str());
  return a == INVALID_FILE_ATTRIBUTES && GetLastError() == ERROR_FILE_NOT_FOUND;
}
static bool remove(const std::wstring &p) {
  return DeleteFileW(p.c_str()) || GetLastError() == ERROR_FILE_NOT_FOUND;
}
static bool readPrivateDword(const std::wstring &path, DWORD &value) {
  Handle h(CreateFileW(path.c_str(), GENERIC_READ | READ_CONTROL,
                       FILE_SHARE_READ, nullptr, OPEN_EXISTING,
                       FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
  DWORD n = 0;
  return h.h != INVALID_HANDLE_VALUE && privateHandle(h.h) &&
         GetFileSize(h.h, nullptr) == sizeof(value) &&
         ReadFile(h.h, &value, sizeof(value), &n, nullptr) &&
         n == sizeof(value);
}
static bool writePrivate(const std::wstring &path, const BYTE *data,
                         DWORD size);
static bool writePrivate(const std::wstring &path, const BYTE *data,
                         DWORD size) {
  Descriptor sd(OnlySystem);
  if (!sd.p)
    return false;
  auto sa = sd.attrs();
  auto temporary = path + L".new";
  // CREATE_NEW rejects pre-existing links/files, and the parent is pinned and
  // SYSTEM-only.
  if (!remove(temporary))
    return false;
  Handle h(CreateFileW(temporary.c_str(),
                       GENERIC_WRITE | READ_CONTROL | FILE_READ_ATTRIBUTES, 0,
                       &sa, CREATE_NEW, FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
  DWORD n = 0;
  bool ok = h.h != INVALID_HANDLE_VALUE && privateHandle(h.h) &&
            WriteFile(h.h, data, size, &n, nullptr) && n == size &&
            FlushFileBuffers(h.h);
  if (h.h != INVALID_HANDLE_VALUE) {
    CloseHandle(h.h);
    h.h = nullptr;
  }
  if (ok)
    ok = MoveFileExW(temporary.c_str(), path.c_str(),
                     MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) !=
         FALSE;
  if (!ok)
    remove(temporary);
  return ok;
}
static bool eraseAccount(const Config &c) {
  bool ok = true;
  // A crash before MoveFileEx may leave the encrypted staging file. Disable
  // removes that copy too, and attempts every deletion even after one error.
  for (const wchar_t *suffix : {L".dpapi", L".dpapi.new", L".blocked",
                                L".blocked.new", L".result", L".result.new",
                                L".failure", L".failure.new"})
    ok = remove(file(c, suffix)) && ok;
  return ok;
}
static FailureKind savedFailure(const Config &c) {
  DWORD value = FailureUnknown;
  if (absent(file(c, L".failure")))
    return FailureNone;
  if (!readPrivateDword(file(c, L".failure"), value) || value > FailureUnknown)
    return FailureUnknown;
  return static_cast<FailureKind>(value);
}
static bool persistFailure(const Config &c, FailureKind kind) {
  if (kind == FailureNone)
    return remove(file(c, L".failure"));
  DWORD value = kind;
  return writePrivate(file(c, L".failure"), reinterpret_cast<BYTE *>(&value),
                      sizeof(value));
}
// Settings > Accounts > Sign-in options > "For improved security, only allow
// Windows Hello sign-in for Microsoft accounts on this device" (Windows 10 2004
// "Require Windows Hello sign-in for Microsoft accounts"). Microsoft does not
// document where the toggle is stored; the Settings app keeps it in this HKLM
// DWORD (64-bit view): 2 when on, 0 when off (a test machine reads 0 with the
// toggle off). Read-only here: Oink never changes it. Raw string: in a plain
// literal "\M" etc. are unknown escapes (C4129) and the path loses its
// separators.
static constexpr wchar_t PasswordLessDeviceKey[] =
    LR"(SOFTWARE\Microsoft\Windows NT\CurrentVersion\PasswordLess\Device)";
static constexpr wchar_t PasswordLessDeviceValue[] =
    L"DevicePasswordLessBuildVersion";
// Reads the toggle below hive (HKEY_LOCAL_MACHINE in production). A missing
// key or value, or a value that is not a DWORD, means the toggle was never on.
extern "C" bool oink_unlock_hello_only_under(HKEY hive) {
  HKEY key = nullptr;
  if (RegOpenKeyExW(hive, PasswordLessDeviceKey, 0, KEY_READ | KEY_WOW64_64KEY,
                    &key) != ERROR_SUCCESS)
    return false;
  DWORD type = 0, value = 0, size = sizeof(value);
  const bool enabled =
      RegQueryValueExW(key, PasswordLessDeviceValue, nullptr, &type,
                       reinterpret_cast<BYTE *>(&value),
                       &size) == ERROR_SUCCESS &&
      type == REG_DWORD && size == sizeof(value) && value != 0;
  RegCloseKey(key);
  return enabled;
}
static bool windowsHelloOnly() {
  return oink_unlock_hello_only_under(HKEY_LOCAL_MACHINE);
}
static FailureKind outcomeFailure(DWORD status) {
  // ReportResult supplies the NTSTATUS returned by the password package.
  if (status == 0xC000006A || status == 0xC000006D)
    return FailureIncorrectPassword;
  return FailureUnknown;
}
struct Secret {
  Config target;
  wchar_t password[512];
  ~Secret() { SecureZeroMemory(this, sizeof(*this)); }
};
static bool save(const Config &c, const wchar_t *password) {
  Secret s{};
  s.target = c;
  wcscpy_s(s.password, password);
  DATA_BLOB in{sizeof(s), reinterpret_cast<BYTE *>(&s)}, out{};
  // Deliberately user-scope DPAPI under the service's SYSTEM identity. No
  // impersonation.
  if (!system() || !CryptProtectData(&in, L"Oink auto unlock", nullptr, nullptr,
                                     nullptr, CRYPTPROTECT_UI_FORBIDDEN, &out))
    return false;
  bool ok = writePrivate(file(c, L".dpapi"), out.pbData, out.cbData);
  SecureZeroMemory(out.pbData, out.cbData);
  LocalFree(out.pbData);
  return ok;
}
static DWORD verifyPassword(const Config &c, const wchar_t *password) {
  HANDLE token = nullptr;
  if (LogonUserW(c.user, c.domain, password, LOGON32_LOGON_NETWORK,
                 LOGON32_PROVIDER_DEFAULT, &token)) {
    CloseHandle(token);
    return ERROR_SUCCESS;
  }
  return GetLastError();
}
static bool acquire(const Config &c, Reply &reply) {
  Handle h(CreateFileW(file(c, L".dpapi").c_str(), GENERIC_READ | READ_CONTROL,
                       FILE_SHARE_READ, nullptr, OPEN_EXISTING,
                       FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
  if (h.h == INVALID_HANDLE_VALUE || !privateHandle(h.h))
    return false;
  DWORD size = GetFileSize(h.h, nullptr), n = 0;
  if (!size || size > 8192)
    return false;
  std::vector<BYTE> blob(size);
  if (!ReadFile(h.h, blob.data(), size, &n, nullptr) || n != size)
    return false;
  DATA_BLOB in{size, blob.data()}, plain{};
  if (!CryptUnprotectData(&in, nullptr, nullptr, nullptr, nullptr,
                          CRYPTPROTECT_UI_FORBIDDEN, &plain))
    return false;
  auto s = reinterpret_cast<Secret *>(plain.pbData);
  bool ok = plain.cbData == sizeof(Secret) &&
            !memcmp(&s->target, &c, sizeof(c)) && wmemchr(s->password, 0, 512);
  if (ok) {
    reply.target = c;
    memcpy(reply.password, s->password, sizeof(reply.password));
  }
  SecureZeroMemory(plain.pbData, plain.cbData);
  LocalFree(plain.pbData);
  return ok;
}
static bool block(const Config &c) {
  BYTE b = 1;
  return writePrivate(file(c, L".blocked"), &b, 1);
}
static HANDLE cpPeer(HANDLE pipe, DWORD id) {
  ULONG pid = 0;
  if (id == 0 || id != WTSGetActiveConsoleSessionId() ||
      !GetNamedPipeClientProcessId(pipe, &pid))
    return nullptr;
  Handle p(
      OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, pid)),
      t;
  DWORD actual = 0;
  if (!p.h || !ProcessIdToSessionId(pid, &actual) || actual != id ||
      !image(p.h, windowsPath(L"\\System32\\LogonUI.exe")) ||
      !OpenProcessToken(p.h, TOKEN_QUERY, &t.h) || !systemToken(t.h))
    return nullptr;
  if (!ImpersonateNamedPipeClient(pipe))
    return nullptr;
  Handle imp;
  bool ok = OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, TRUE, &imp.h) &&
            systemToken(imp.h);
  if (!RevertToSelf())
    TerminateProcess(GetCurrentProcess(), ERROR_ACCESS_DENIED);
  if (!ok || WaitForSingleObject(p.h, 0) != WAIT_TIMEOUT)
    return nullptr;
  HANDLE retained = p.h;
  p.h = nullptr;
  return retained;
}
static bool persistResult(const Config &c, UnlockOperation::State state) {
  DWORD value = state;
  return writePrivate(file(c, L".result"), reinterpret_cast<BYTE *>(&value),
                      sizeof(value));
}
static UnlockOperation::State savedResult(const Config &c) {
  auto path = file(c, L".result");
  if (absent(path))
    return UnlockOperation::Idle;
  Handle h(CreateFileW(path.c_str(), GENERIC_READ | READ_CONTROL,
                       FILE_SHARE_READ, nullptr, OPEN_EXISTING,
                       FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
  DWORD value = UnlockOperation::Failed, n = 0;
  if (h.h == INVALID_HANDLE_VALUE || !privateHandle(h.h) ||
      !ReadFile(h.h, &value, sizeof(value), &n, nullptr) ||
      n != sizeof(value) || value > UnlockOperation::Expired)
    return UnlockOperation::Failed;
  return static_cast<UnlockOperation::State>(value);
}
static void handle(Request &q, Reply &r, HANDLE pipe, DWORD id) {
  std::lock_guard<std::mutex> lock(gate);
  if (stopping)
    return;
  auto before = operation.state;
  operation.expire(GetTickCount64());
  if (before != operation.state && operation.sync_abandoned()) {
    // LogonUI never serialized within the sync window: the LockApp curtain
    // was still up (Q3). A skip, not a failure: no latch, no saved result.
    if (syncId == operation.id) {
      syncOutcome = SyncExpired;
      syncPrepared = operation.prepared;
    }
    operation.reset();
  } else if (before != operation.state) {
    persistFailure(operationTarget, FailureUnknown);
    remove(file(operationTarget, L".blocked"));
    persistResult(operationTarget, operation.state);
    if (operation.sync && syncId == operation.id)
      syncOutcome = SyncFailed;
  }
  Handle cpProcess(cpPeer(pipe, id));
  bool cp = cpProcess.h != nullptr;
  Handle peer(!cp && backendAuth ? backendAuth(pipe, id) : nullptr);
  bool backend = peer.h != nullptr;
  if (q.magic != 0x554b4e4f || q.reserved ||
      id != WTSGetActiveConsoleSessionId() || (!cp && !backend))
    return;
  Config c{};
  const bool signed_in = target(id, c);
  if (!signed_in)
    return;
  auto saved = savedResult(c);
  if ((saved == UnlockOperation::Pending ||
       saved == UnlockOperation::Submitted) &&
      (!operation.id || memcmp(&c, &operationTarget, sizeof(c)))) {
    saved = UnlockOperation::recover_saved(saved);
    persistFailure(c, FailureUnknown);
    remove(file(c, L".blocked"));
    persistResult(c, saved);
  }
  if (q.op == Status && backend)
    r.error = 0;
  else if (q.op == Store && backend && wmemchr(q.password, 0, 512)) {
    // LOGON32_LOGON_NETWORK validates the existing local account without
    // creating an interactive session. Do this before revoking or writing the
    // existing vault so a rejected replacement leaves it untouched.
    r.error = verifyPassword(c, q.password);
    if (r.error == ERROR_SUCCESS) {
      // Revoke before the first write, including partial/failed saves: an old
      // provider must never acquire the replacement password under its old op.
      operation.reset();
      if (save(c, q.password) && persistResult(c, UnlockOperation::Idle) &&
          remove(file(c, L".blocked")) && persistFailure(c, FailureNone)) {
        r.error = 0;
      }
    }
  } else if (q.op == Disable && backend) {
    operation.reset();
    if (eraseAccount(c))
      r.error = 0;
  } else if (q.op == Trigger && backend) {
    const bool retryable = saved == UnlockOperation::Idle ||
                           saved == UnlockOperation::Succeeded ||
                           saved == UnlockOperation::Failed ||
                           saved == UnlockOperation::Expired;
    if (retryable && !absent(file(c, L".dpapi")) && locked(id)) {
      // A failure latch is only a notice. It must not turn a later pointer
      // entry into a permanent disable, including after an older service left
      // the latch behind.
      if (saved == UnlockOperation::Failed || saved == UnlockOperation::Expired)
        remove(file(c, L".blocked"));
      if (windowsHelloOnly()) {
        operation.reset();
        operation.state = UnlockOperation::Failed;
        persistFailure(c, FailureWindowsHelloOnly);
        persistResult(c, operation.state);
        remove(file(c, L".blocked"));
        r.error = 0;
      } else if (operation.begin(++sequence, id, GetTickCount64(), signed_in,
                                 q.id)) {
        operationTarget = c;
        // The provider's first Poll is the LogonUI readiness edge. There is
        // no curtain helper or timeout here: the user dismisses LockApp,
        // then LogonUI decides whether this pending operation is eligible.
        if (persistResult(c, UnlockOperation::Pending))
          r.error = 0;
        else {
          operation.state = UnlockOperation::Failed;
          persistFailure(c, FailureUnknown);
          remove(file(c, L".blocked"));
          persistResult(c, operation.state);
        }
      }
    }
  } else if (q.op == SyncTrigger && backend && q.id) {
    // Same preconditions as Trigger. Hello-only devices are rejected without
    // a latch; the agent already skipped them. No Pending result is persisted:
    // an unconsumed sync operation is forgotten, never recovered as a failure.
    const bool retryable = saved == UnlockOperation::Idle ||
                           saved == UnlockOperation::Succeeded ||
                           saved == UnlockOperation::Failed ||
                           saved == UnlockOperation::Expired;
    if (retryable && !absent(file(c, L".dpapi")) && locked(id) &&
        !windowsHelloOnly() &&
        operation.begin(++sequence, id, GetTickCount64(), signed_in, q.id,
                        true)) {
      if (saved == UnlockOperation::Failed || saved == UnlockOperation::Expired)
        remove(file(c, L".blocked"));
      operationTarget = c;
      syncId = operation.id;
      syncOutcome = SyncPending;
      syncPrepared = false;
      r.id = operation.id;
      r.error = 0;
    }
  } else if (q.op == RemoteInput && backend && q.id) {
    const uint64_t now = GetTickCount64();
    const uint64_t age = std::min<uint64_t>(q.status, 60000);
    recentRemoteSource = q.id;
    recentRemoteAt = now >= age ? now - age : 0;
    r.error = 0;
  } else if (q.op == Poll && cp) {
    r.target = c;
    r.error = 0;
    const uint64_t now = GetTickCount64();
    if (locked(id) && !memcmp(&c, &operationTarget, sizeof(c)) &&
        operation.ready(operation.id, id, now, recentRemoteSource,
                        recentRemoteAt) && operation.pending(id, now))
      r.id = operation.id;
  } else if (q.op == Acquire && cp && locked(id) &&
             !memcmp(&c, &operationTarget, sizeof(c))) {
    // Atomic consume, then durable latch before decrypt/pipe write. A failed
    // send, crashed LogonUI or stopped service can never grant another
    // submission.
    if (operation.consume(q.id, id, GetTickCount64())) {
      if (block(c) && persistResult(c, UnlockOperation::Submitted) &&
          acquire(c, r))
        r.error = 0;
      else {
        operation.state = UnlockOperation::Failed;
        persistFailure(c, FailureUnknown);
        remove(file(c, L".blocked"));
        persistResult(c, operation.state);
      }
      if (operation.sync && syncId == operation.id) {
        syncPrepared = true;
        syncOutcome = operation.state == UnlockOperation::Submitted
                          ? SyncSubmitted
                          : SyncFailed;
      }
    }
  } else if (q.op == Outcome && cp && q.id == operation.id &&
             id == operation.session &&
             operation.state == UnlockOperation::Submitted) {
    operation.result(q.id, id, q.status == 0, GetTickCount64());
    if (q.status == 0) {
      if (!persistFailure(operationTarget, FailureNone))
        operation.state = UnlockOperation::Failed;
    } else {
      persistFailure(operationTarget, outcomeFailure(q.status));
      remove(file(operationTarget, L".blocked"));
    }
    if (!persistResult(operationTarget, operation.state))
      operation.state = UnlockOperation::Failed;
    if (operation.state == UnlockOperation::Succeeded &&
        !remove(file(operationTarget, L".blocked")))
      operation.state = UnlockOperation::Failed;
    if (operation.sync && syncId == operation.id)
      syncOutcome = operation.state == UnlockOperation::Succeeded
                        ? SyncSucceeded
                        : SyncFailed;
    r.error = 0;
  }
  r.present = !absent(file(c, L".dpapi"));
  r.state = savedResult(c);
  r.reserved = savedFailure(c);
  if (operation.id && !memcmp(&c, &operationTarget, sizeof(c)))
    r.state = operation.state;
  else if (r.state == UnlockOperation::Pending ||
           r.state == UnlockOperation::Submitted)
    r.state = UnlockOperation::recover_saved(
        static_cast<UnlockOperation::State>(r.state));
  if (!absent(file(c, L".blocked")) && r.state != UnlockOperation::Submitted &&
      r.state != UnlockOperation::Expired)
    r.state = UnlockOperation::Failed;
  if (operation.id && !memcmp(&c, &operationTarget, sizeof(c))) {
    r.ready_at = operation.ready_at;
    r.remote_input_at = operation.remote_input_at;
    r.remote_input_source = operation.remote_input_source;
    r.decision = operation.prepared ? (operation.eligible ? 1 : 2) : 0;
    r.succeeded_at = operation.succeeded_at;
  }
  r.now = GetTickCount64();
  r.sync = syncOutcome | (syncPrepared ? 0x100u : 0u);
  r.sync_id = syncId;
}
// Own each OVERLAPPED until completion, including cancellation. No sleeping
// polling loop and no FlushFileBuffers waiting on an untrusted client.
static bool transfer(HANDLE pipe, bool read, void *buffer, DWORD size,
                     DWORD timeout) {
  Handle event(CreateEventW(nullptr, TRUE, FALSE, nullptr));
  if (!event.h)
    return false;
  OVERLAPPED ov{};
  ov.hEvent = event.h;
  DWORD bytes = 0;
  BOOL started = read ? ReadFile(pipe, buffer, size, nullptr, &ov)
                      : WriteFile(pipe, buffer, size, nullptr, &ov);
  if (!started && GetLastError() != ERROR_IO_PENDING)
    return false;
  if (!started) {
    HANDLE waits[] = {stopEvent, event.h};
    if (WaitForMultipleObjects(2, waits, FALSE, timeout) != WAIT_OBJECT_0 + 1) {
      CancelIoEx(pipe, &ov);
      GetOverlappedResult(pipe, &ov, &bytes, TRUE);
      return false;
    }
  }
  return GetOverlappedResult(pipe, &ov, &bytes, FALSE) && bytes == size;
}
static bool connect(HANDLE pipe) {
  Handle event(CreateEventW(nullptr, TRUE, FALSE, nullptr));
  if (!event.h)
    return false;
  OVERLAPPED ov{};
  ov.hEvent = event.h;
  if (ConnectNamedPipe(pipe, &ov))
    return true;
  DWORD error = GetLastError();
  if (error == ERROR_PIPE_CONNECTED)
    return true;
  if (error != ERROR_IO_PENDING)
    return false;
  HANDLE waits[] = {stopEvent, sessionChanged.load(), event.h};
  DWORD bytes = 0;
  DWORD result = WaitForMultipleObjects(3, waits, FALSE, INFINITE);
  if (result != WAIT_OBJECT_0 + 2) {
    CancelIoEx(pipe, &ov);
    GetOverlappedResult(pipe, &ov, &bytes, TRUE);
    return false;
  }
  return GetOverlappedResult(pipe, &ov, &bytes, FALSE) != FALSE;
}
static DWORD WINAPI serve(void *) {
  Handle directory(CreateFileW(
      root.c_str(), READ_CONTROL | FILE_READ_ATTRIBUTES,
      FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
      FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
  if (directory.h == INVALID_HANDLE_VALUE || !privateHandle(directory.h))
    return ERROR_ACCESS_DENIED;
  while (!stopping) {
    // Reset before observation so a concurrent WTS transition remains signaled.
    ResetEvent(sessionChanged.load());
    DWORD id = WTSGetActiveConsoleSessionId();
    Config c{};
    if (!id || id == MAXDWORD || !target(id, c)) {
      HANDLE waits[] = {stopEvent, sessionChanged.load()};
      WaitForMultipleObjects(2, waits, FALSE, INFINITE);
      continue;
    }
    auto text =
        std::wstring(L"O:SYG:SYD:P(A;;GA;;;SY)(A;;0x12019b;;;") + c.sid + L")";
    Descriptor sd(text.c_str());
    if (!sd.p)
      return ERROR_ACCESS_DENIED;
    auto sa = sd.attrs();
    Handle pipe(CreateNamedPipeW(PipeName,
                                 PIPE_ACCESS_DUPLEX |
                                     FILE_FLAG_FIRST_PIPE_INSTANCE |
                                     FILE_FLAG_OVERLAPPED,
                                 PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE |
                                     PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                                 1, 4096, 4096, 3000, &sa));
    if (pipe.h == INVALID_HANDLE_VALUE)
      return GetLastError();
    if (connect(pipe.h) && !stopping) {
      Request q{};
      Reply r{};
      ULONG actual = MAXDWORD;
      if (transfer(pipe.h, true, &q, sizeof(q), 3000) &&
          GetNamedPipeClientSessionId(pipe.h, &actual) && actual == id)
        handle(q, r, pipe.h, actual);
      bool written = transfer(pipe.h, false, &r, sizeof(r), 3000);
      SecureZeroMemory(&q, sizeof(q));
      SecureZeroMemory(&r, sizeof(r));
      // Wait for the one-request client to drain/close; broken pipe completes
      // this read. A stalled/malicious client is disconnected after three
      // seconds.
      BYTE unexpected = 0;
      if (written)
        transfer(pipe.h, true, &unexpected, 1, 3000);
    }
    DisconnectNamedPipe(pipe.h);
  }
  return 0;
}
extern "C" void oink_unlock_wake() {
  if (HANDLE event = sessionChanged.load())
    SetEvent(event);
}
extern "C" DWORD oink_unlock_start(Authenticate auth) {
  if (!system() || session() != 0)
    return ERROR_ACCESS_DENIED;
  PWSTR programData = nullptr;
  if (FAILED(
          SHGetKnownFolderPath(FOLDERID_ProgramData, 0, nullptr, &programData)))
    return ERROR_PATH_NOT_FOUND;
  root = std::wstring(programData) + L"\\OinkAutoUnlock";
  CoTaskMemFree(programData);
  Descriptor sd(OnlySystem);
  if (!sd.p)
    return ERROR_ACCESS_DENIED;
  auto sa = sd.attrs();
  if (!CreateDirectoryW(root.c_str(), &sa) &&
      GetLastError() != ERROR_ALREADY_EXISTS)
    return GetLastError();
  Handle dir(CreateFileW(
      root.c_str(), READ_CONTROL | FILE_READ_ATTRIBUTES,
      FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
      FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
  if (dir.h == INVALID_HANDLE_VALUE || !privateHandle(dir.h))
    return ERROR_ACCESS_DENIED;
  GUID nonce{};
  if (FAILED(CoCreateGuid(&nonce)))
    return ERROR_GEN_FAILURE;
  memcpy(&sequence, &nonce, sizeof(sequence));
  stopEvent = CreateEventW(nullptr, TRUE, FALSE, nullptr);
  sessionChanged.store(CreateEventW(nullptr, TRUE, FALSE, nullptr));
  if (!stopEvent || !sessionChanged.load())
    return ERROR_NOT_ENOUGH_MEMORY;
  operation.reset();
  recentRemoteSource = 0;
  recentRemoteAt = 0;
  syncId = 0;
  syncOutcome = SyncNone;
  syncPrepared = false;
  backendAuth = auth;
  stopping = false;
  worker = CreateThread(nullptr, 0, serve, nullptr, 0, nullptr);
  return worker ? 0 : GetLastError();
}
extern "C" void oink_unlock_stop() {
  stopping = true;
  if (stopEvent)
    SetEvent(stopEvent);
  if (worker) {
    WaitForSingleObject(worker, INFINITE);
    CloseHandle(worker);
    worker = nullptr;
  }
}
extern "C" bool oink_unlock_session(DWORD kind, DWORD id) {
  std::lock_guard<std::mutex> lock(gate);
  // WTS is correlation, not proof of human intent. Both own and unknown events
  // are suppressed; no network publication originates in this service.
  if (kind == WTS_SESSION_UNLOCK || kind == WTS_SESSION_LOGON) {
    bool own = operation.own_unlock(id, GetTickCount64());
    if (id == operation.session && operation.state == UnlockOperation::Pending)
      operation.reset();
    return own;
  }
  if (kind == WTS_SESSION_LOGOFF || kind == WTS_CONSOLE_DISCONNECT) {
    if (id == operation.session)
      operation.reset();
  }
  return false;
}
extern "C" DWORD oink_unlock_client(DWORD op, uint64_t source,
                                    DWORD remote_input_age_ms,
                                    const wchar_t *password, DWORD *status) {
  Request q{};
  q.op = op;
  q.id = source;
  q.status = remote_input_age_ms;
  if (op == Store)
    memcpy(q.password, password, sizeof(q.password));
  Reply r{};
  bool ok = rpc(q, r);
  DWORD error = ok ? r.error : ERROR_SERVICE_NOT_ACTIVE;
  status[0] = r.present;
  status[1] = r.state;
  status[2] = r.reserved;
  status[3] = windowsHelloOnly() ? 1 : 0;
  status[4] = static_cast<DWORD>(r.ready_at);
  status[5] = static_cast<DWORD>(r.ready_at >> 32);
  status[6] = static_cast<DWORD>(r.remote_input_at);
  status[7] = static_cast<DWORD>(r.remote_input_at >> 32);
  status[8] = static_cast<DWORD>(r.remote_input_source);
  status[9] = static_cast<DWORD>(r.remote_input_source >> 32);
  status[10] = r.decision;
  status[11] = r.sync;
  status[12] = static_cast<DWORD>(r.succeeded_at);
  status[13] = static_cast<DWORD>(r.succeeded_at >> 32);
  status[14] = static_cast<DWORD>(r.now);
  status[15] = static_cast<DWORD>(r.now >> 32);
  status[16] = static_cast<DWORD>(r.sync_id);
  status[17] = static_cast<DWORD>(r.sync_id >> 32);
  status[18] = static_cast<DWORD>(r.id);
  status[19] = static_cast<DWORD>(r.id >> 32);
  SecureZeroMemory(&q, sizeof(q));
  SecureZeroMemory(&r, sizeof(r));
  return error;
}
static LSTATUS setValue(const std::wstring &key, const wchar_t *name,
                        const std::wstring &value) {
  HKEY h = nullptr;
  LSTATUS e =
      RegCreateKeyExW(HKEY_LOCAL_MACHINE, key.c_str(), 0, nullptr, 0,
                      KEY_WRITE | KEY_WOW64_64KEY, nullptr, &h, nullptr);
  if (!e) {
    e = RegSetValueExW(h, name, 0, REG_SZ,
                       reinterpret_cast<const BYTE *>(value.c_str()),
                       static_cast<DWORD>((value.size() + 1) * 2));
    RegCloseKey(h);
  }
  return e;
}
static std::wstring providerKey() {
  return std::wstring(L"SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Authen"
                      L"tication\\Credential Providers\\") +
         ProviderId;
}
static std::wstring classKey() {
  return std::wstring(L"SOFTWARE\\Classes\\CLSID\\") + ProviderId;
}
// Uninstall steps, reported with the failing object so the installer log says
// which one failed (oink_winsvc maps the numbers to text; keep them in sync).
enum UnregisterStep : DWORD {
  UnregisterIdentity = 1,
  UnregisterProgramData = 2,
  UnregisterVaultOpen = 3,
  UnregisterVaultUnsafe = 4,
  UnregisterVaultList = 5,
  UnregisterVaultEntry = 6,
  UnregisterVaultFile = 7,
  UnregisterVaultDirectory = 8,
  UnregisterProviderKey = 9,
  UnregisterClassKey = 10,
};
struct UnregisterFailure {
  DWORD *step;
  wchar_t *object;
  DWORD capacity;
  DWORD fail(UnregisterStep s, const std::wstring &what, DWORD error) const {
    *step = s;
    if (capacity)
      wcsncpy_s(object, capacity, what.c_str(), _TRUNCATE);
    return error ? error : ERROR_GEN_FAILURE;
  }
};
static DWORD eraseVault(const std::wstring &directory,
                        const UnregisterFailure &out) {
  DWORD attributes = GetFileAttributesW(directory.c_str());
  if (attributes == INVALID_FILE_ATTRIBUTES) {
    DWORD error = GetLastError();
    if (error == ERROR_FILE_NOT_FOUND || error == ERROR_PATH_NOT_FOUND)
      return 0; // Never enabled, or already erased.
    return out.fail(UnregisterVaultOpen, directory, error);
  }
  Handle d(CreateFileW(
      directory.c_str(), READ_CONTROL | FILE_READ_ATTRIBUTES,
      FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
      FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
  if (d.h == INVALID_HANDLE_VALUE)
    return out.fail(UnregisterVaultOpen, directory, GetLastError());
  // Same pinned-directory rule as the service: never delete through a link
  // or from a directory someone other than SYSTEM could have populated.
  if (!privateHandle(d.h))
    return out.fail(UnregisterVaultUnsafe, directory, ERROR_ACCESS_DENIED);
  WIN32_FIND_DATAW data{};
  HANDLE search = FindFirstFileW((directory + L"\\*").c_str(), &data);
  if (search == INVALID_HANDLE_VALUE)
    return out.fail(UnregisterVaultList, directory, GetLastError());
  DWORD error = 0;
  do {
    if (!wcscmp(data.cFileName, L".") || !wcscmp(data.cFileName, L".."))
      continue;
    auto entry = directory + L"\\" + data.cFileName;
    if (data.dwFileAttributes &
        (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)) {
      error = out.fail(UnregisterVaultEntry, entry, ERROR_ACCESS_DENIED);
      break;
    }
    if (!remove(entry)) {
      error = out.fail(UnregisterVaultFile, entry, GetLastError());
      break;
    }
  } while (FindNextFileW(search, &data));
  if (!error && GetLastError() != ERROR_NO_MORE_FILES)
    error = out.fail(UnregisterVaultList, directory, GetLastError());
  FindClose(search);
  if (error)
    return error;
  CloseHandle(d.h);
  d.h = nullptr;
  if (!RemoveDirectoryW(directory.c_str()))
    return out.fail(UnregisterVaultDirectory, directory, GetLastError());
  return 0;
}
static DWORD deleteTree(HKEY hive, const std::wstring &key, UnregisterStep step,
                        const UnregisterFailure &out) {
  LSTATUS e = RegDeleteTreeW(hive, key.c_str());
  if (e == ERROR_SUCCESS || e == ERROR_FILE_NOT_FOUND)
    return 0; // Already unregistered (for example removed by hand) is done.
  return out.fail(step, key, static_cast<DWORD>(e));
}
// Removal below a caller-chosen vault directory and registry root, so tests
// can exercise it on a scratch tree. Returns 0, or the Win32 error of the
// first failing step with *step and object naming it.
extern "C" DWORD oink_unlock_unregister_under(HKEY hive, const wchar_t *vault,
                                              DWORD *step, wchar_t *object,
                                              DWORD capacity) {
  const UnregisterFailure out{step, object, capacity};
  *step = 0;
  if (capacity)
    object[0] = 0;
  if (DWORD e = eraseVault(vault, out))
    return e;
  if (DWORD e = deleteTree(hive, providerKey(), UnregisterProviderKey, out))
    return e;
  return deleteTree(hive, classKey(), UnregisterClassKey, out);
}
// Uninstall: erase the vault, then remove this provider's registration.
// No session-0 requirement here, unlike registration: Windows Installer runs
// the deferred, non-impersonating uninstall action as LocalSystem but in the
// session of the msiexec client (session 1 for an uninstall started from the
// desktop; session 0 only for one started from a service or ssh). Requiring
// session 0 failed every desktop uninstall (issue #46). The caller has
// already verified the protected installed image.
extern "C" DWORD oink_unlock_unregister(DWORD *step, wchar_t *object,
                                        DWORD capacity) {
  const UnregisterFailure out{step, object, capacity};
  *step = 0;
  if (capacity)
    object[0] = 0;
  if (!system())
    return out.fail(UnregisterIdentity, L"", ERROR_ACCESS_DENIED);
  PWSTR programData = nullptr;
  HRESULT hr =
      SHGetKnownFolderPath(FOLDERID_ProgramData, 0, nullptr, &programData);
  if (FAILED(hr))
    return out.fail(UnregisterProgramData, L"", HRESULT_CODE(hr));
  auto vault = std::wstring(programData) + L"\\OinkAutoUnlock";
  CoTaskMemFree(programData);
  return oink_unlock_unregister_under(HKEY_LOCAL_MACHINE, vault.c_str(), step,
                                      object, capacity);
}
extern "C" DWORD oink_unlock_register(const wchar_t *dll) {
  if (!system() || session() != 0)
    return ERROR_ACCESS_DENIED;
  auto cp = providerKey();
  auto cls = classKey();
  DWORD e = setValue(cls + L"\\InprocServer32", nullptr, dll);
  if (!e)
    e = setValue(cls + L"\\InprocServer32", L"ThreadingModel", L"Apartment");
  if (!e)
    e = setValue(cp, nullptr, L"Oink auto unlock");
  return e;
}
