#include "ipc.hpp"

#include <atomic>
#include <credentialprovider.h>
#include <memory>
#include <ntsecapi.h>
#include <objbase.h>
#include <process.h>
#include <shlwapi.h>
#pragma comment(lib, "ole32.lib")
#pragma comment(lib, "secur32.lib")
#pragma comment(lib, "uuid.lib")
#pragma comment(lib, "shlwapi.lib")
std::atomic<long> objects{0};
HRESULT dup(const wchar_t *s, PWSTR *p) { return SHStrDupW(s, p); }
class Credential : public ICredentialProviderCredential2 {
  LONG refs = 1;
  CREDENTIAL_PROVIDER_USAGE_SCENARIO usage;
  std::shared_ptr<std::atomic<uint64_t>> pending;
  uint64_t submitted = 0;

public:
  Credential(CREDENTIAL_PROVIDER_USAGE_SCENARIO u,
             std::shared_ptr<std::atomic<uint64_t>> p)
      : usage(u), pending(p) {
    ++objects;
  }
  ~Credential() { --objects; }
  HRESULT STDMETHODCALLTYPE QueryInterface(REFIID i, void **p) override {
    *p = nullptr;
    if (i == IID_IUnknown || i == __uuidof(ICredentialProviderCredential) ||
        i == __uuidof(ICredentialProviderCredential2))
      *p = static_cast<ICredentialProviderCredential2 *>(this);
    else
      return E_NOINTERFACE;
    AddRef();
    return S_OK;
  }
  ULONG STDMETHODCALLTYPE AddRef() override {
    return InterlockedIncrement(&refs);
  }
  ULONG STDMETHODCALLTYPE Release() override {
    LONG r = InterlockedDecrement(&refs);
    if (!r)
      delete this;
    return r;
  }
  HRESULT STDMETHODCALLTYPE
  Advise(ICredentialProviderCredentialEvents *) override {
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE UnAdvise() override { return S_OK; }
  HRESULT STDMETHODCALLTYPE SetSelected(BOOL *a) override {
    *a = FALSE;
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE SetDeselected() override { return S_OK; }
  HRESULT STDMETHODCALLTYPE
  GetFieldState(DWORD id, CREDENTIAL_PROVIDER_FIELD_STATE *s,
                CREDENTIAL_PROVIDER_FIELD_INTERACTIVE_STATE *i) override {
    if (id > 1)
      return E_INVALIDARG;
    *s = CPFS_DISPLAY_IN_BOTH;
    *i = CPFIS_NONE;
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE GetStringValue(DWORD id, PWSTR *p) override {
    return dup(id == 0 ? L"Oink" : L"Unlock", p);
  }
  HRESULT STDMETHODCALLTYPE GetBitmapValue(DWORD, HBITMAP *) override {
    return E_NOTIMPL;
  }
  HRESULT STDMETHODCALLTYPE GetCheckboxValue(DWORD, BOOL *, PWSTR *) override {
    return E_NOTIMPL;
  }
  HRESULT STDMETHODCALLTYPE GetSubmitButtonValue(DWORD id,
                                                 DWORD *adjacent) override {
    if (id != 1)
      return E_INVALIDARG;
    *adjacent = 0;
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE GetComboBoxValueCount(DWORD, DWORD *,
                                                  DWORD *) override {
    return E_NOTIMPL;
  }
  HRESULT STDMETHODCALLTYPE GetComboBoxValueAt(DWORD, DWORD, PWSTR *) override {
    return E_NOTIMPL;
  }
  HRESULT STDMETHODCALLTYPE SetStringValue(DWORD, PCWSTR) override {
    return E_NOTIMPL;
  }
  HRESULT STDMETHODCALLTYPE SetCheckboxValue(DWORD, BOOL) override {
    return E_NOTIMPL;
  }
  HRESULT STDMETHODCALLTYPE SetComboBoxSelectedValue(DWORD, DWORD) override {
    return E_NOTIMPL;
  }
  HRESULT STDMETHODCALLTYPE CommandLinkClicked(DWORD) override {
    return E_NOTIMPL;
  }
  HRESULT STDMETHODCALLTYPE GetUserSid(PWSTR *s) override {
    Request q{};
    q.op = Poll;
    Reply r{};
    if (!rpc(q, r) || r.error)
      return E_FAIL;
    return dup(r.target.sid, s);
  }
  HRESULT STDMETHODCALLTYPE GetSerialization(
      CREDENTIAL_PROVIDER_GET_SERIALIZATION_RESPONSE *response,
      CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION *cs, PWSTR *status,
      CREDENTIAL_PROVIDER_STATUS_ICON *icon) override {
    *response = CPGSR_NO_CREDENTIAL_NOT_FINISHED;
    *status = nullptr;
    *icon = CPSI_NONE;
    ZeroMemory(cs, sizeof(*cs));
    uint64_t op = pending->exchange(0);
    if (!op)
      return S_OK;
    Request request{};
    request.op = Acquire;
    request.id = op;
    Reply reply{};
    bool obtained = rpc(request, reply) && reply.error == 0;
    SecureZeroMemory(&request, sizeof(request));
    if (!obtained) {
      SecureZeroMemory(&reply, sizeof(reply));
      return S_OK;
    }
    Config cfg = reply.target;
    wchar_t password[512]{};
    struct Wiper {
      wchar_t *p;
      ~Wiper() { SecureZeroMemory(p, 512 * sizeof(wchar_t)); }
    } wiper{password};
    memcpy(password, reply.password, sizeof(password));
    SecureZeroMemory(&reply, sizeof(reply));
    HANDLE lsa = nullptr;
    NTSTATUS st = LsaConnectUntrusted(&lsa);
    if (st < 0)
      return HRESULT_FROM_NT(st);
    char negotiate[] = "Negotiate";
    LSA_STRING name{9, 10, negotiate};
    ULONG package = 0;
    st = LsaLookupAuthenticationPackage(lsa, &name, &package);
    LsaDeregisterLogonProcess(lsa);
    if (st < 0)
      return HRESULT_FROM_NT(st);
    size_t dl = wcslen(cfg.domain) * 2, ul = wcslen(cfg.user) * 2,
           pl = wcslen(password) * 2;
    DWORD size = static_cast<DWORD>(sizeof(KERB_INTERACTIVE_UNLOCK_LOGON) + dl +
                                    ul + pl);
    auto k = static_cast<KERB_INTERACTIVE_UNLOCK_LOGON *>(CoTaskMemAlloc(size));
    if (!k)
      return E_OUTOFMEMORY;
    ZeroMemory(k, size);
    k->Logon.MessageType = usage == CPUS_UNLOCK_WORKSTATION
                               ? KerbWorkstationUnlockLogon
                               : KerbInteractiveLogon;
    BYTE *cursor = reinterpret_cast<BYTE *>(k) + sizeof(*k);
    auto pack = [&](UNICODE_STRING &dest, const wchar_t *s, size_t bytes) {
      dest.Length = dest.MaximumLength = static_cast<USHORT>(bytes);
      memcpy(cursor, s, bytes);
      dest.Buffer =
          reinterpret_cast<PWSTR>(cursor - reinterpret_cast<BYTE *>(k));
      cursor += bytes;
    };
    pack(k->Logon.LogonDomainName, cfg.domain, dl);
    pack(k->Logon.UserName, cfg.user, ul);
    pack(k->Logon.Password, password, pl);
    SecureZeroMemory(password, sizeof(password));
    cs->ulAuthenticationPackage = package;
    cs->clsidCredentialProvider = CLSID_Oink;
    cs->cbSerialization = size;
    cs->rgbSerialization = reinterpret_cast<BYTE *>(k);
    *response = CPGSR_RETURN_CREDENTIAL_FINISHED;
    submitted = op;
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE
  ReportResult(NTSTATUS status, NTSTATUS sub, PWSTR *text,
               CREDENTIAL_PROVIDER_STATUS_ICON *icon) override {
    Request request{};
    request.op = Outcome;
    request.id = submitted;
    request.status = static_cast<ULONG>(status != 0 ? status : sub);
    Reply reply{};
    rpc(request, reply);
    SecureZeroMemory(&request, sizeof(request));
    SecureZeroMemory(&reply, sizeof(reply));
    *icon = CPSI_ERROR;
    if (status == 0 && sub == 0) {
      *text = nullptr;
      *icon = CPSI_NONE;
      return S_OK;
    }
    return dup(L"Sign-in failed. Update the saved password in Oink before "
               L"trying again.",
               text);
  }
};
class Provider : public ICredentialProvider,
                 public ICredentialProviderSetUserArray {
  LONG refs = 1;
  CREDENTIAL_PROVIDER_USAGE_SCENARIO usage = CPUS_INVALID;
  Credential *cred = nullptr;
  std::shared_ptr<std::atomic<uint64_t>> pending =
      std::make_shared<std::atomic<uint64_t>>(0);
  IGlobalInterfaceTable *git = nullptr;
  DWORD cookie = 0;
  UINT_PTR context = 0;
  HANDLE stop = nullptr, thread = nullptr;
  // The service is the sole operation authority, surviving tile reconstruction.
  void recover() {
    // This provider does not automate initial sign-in. WTS user metadata is a
    // cheap fail-closed preflight, so startup LogonUI never waits on a service
    // pipe that is intentionally absent without a logged-on user. The service
    // still independently requires WTSQueryUserToken; CPUS_LOGON alone cannot
    // distinguish a lock screen from startup (measured on Win10 19045).
    LPWSTR user = nullptr;
    DWORD bytes = 0;
    const DWORD id = session();
    const bool signed_in = id == WTSGetActiveConsoleSessionId() &&
        WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, id, WTSUserName,
                                    &user, &bytes) &&
        bytes >= 2 * sizeof(wchar_t) && user && user[0];
    if (user) WTSFreeMemory(user);
    if (!signed_in) {
      pending->store(0);
      return;
    }
    Request q{};
    q.op = Poll;
    Reply r{};
    pending->store(rpc(q, r) && !r.error ? r.id : 0);
  }
  static unsigned __stdcall worker(void *p) {
    auto self = static_cast<Provider *>(p);
    HRESULT initialized = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    IGlobalInterfaceTable *git = nullptr;
    CoCreateInstance(CLSID_StdGlobalInterfaceTable, nullptr,
                     CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&git));
    uint64_t seen = 0;
    while (WaitForSingleObject(self->stop, 250) == WAIT_TIMEOUT) {
      self->recover();
      uint64_t op = self->pending->load();
      if (!op || op == seen)
        continue;
      seen = op;
      ICredentialProviderEvents *events = nullptr;
      if (git && SUCCEEDED(git->GetInterfaceFromGlobal(
                     self->cookie, IID_PPV_ARGS(&events)))) {
        events->CredentialsChanged(self->context);
        events->Release();
      }
    }
    if (git)
      git->Release();
    if (SUCCEEDED(initialized))
      CoUninitialize();
    return 0;
  }

public:
  Provider() {
    ++objects;
    recover();
  }
  ~Provider() {
    UnAdvise();
    if (cred)
      cred->Release();
    --objects;
  }
  HRESULT STDMETHODCALLTYPE QueryInterface(REFIID i, void **p) override {
    *p = nullptr;
    if (i == IID_IUnknown || i == __uuidof(ICredentialProvider))
      *p = static_cast<ICredentialProvider *>(this);
    else if (i == __uuidof(ICredentialProviderSetUserArray))
      *p = static_cast<ICredentialProviderSetUserArray *>(this);
    else
      return E_NOINTERFACE;
    AddRef();
    return S_OK;
  }
  ULONG STDMETHODCALLTYPE AddRef() override {
    return InterlockedIncrement(&refs);
  }
  ULONG STDMETHODCALLTYPE Release() override {
    LONG r = InterlockedDecrement(&refs);
    if (!r)
      delete this;
    return r;
  }
  HRESULT STDMETHODCALLTYPE
  SetUsageScenario(CREDENTIAL_PROVIDER_USAGE_SCENARIO u, DWORD) override {
    if (u != CPUS_LOGON && u != CPUS_UNLOCK_WORKSTATION)
      return E_NOTIMPL;
    usage = u;
    recover();
    if (cred)
      cred->Release();
    cred = new Credential(u, pending);
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE SetSerialization(
      const CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION *) override {
    return E_NOTIMPL;
  }
  HRESULT STDMETHODCALLTYPE Advise(ICredentialProviderEvents *events,
                                   UINT_PTR ctx) override {
    UnAdvise();
    recover();
    context = ctx;
    HRESULT hr = CoCreateInstance(CLSID_StdGlobalInterfaceTable, nullptr,
                                  CLSCTX_INPROC_SERVER, IID_PPV_ARGS(&git));
    if (FAILED(hr))
      return hr;
    hr = git->RegisterInterfaceInGlobal(
        events, __uuidof(ICredentialProviderEvents), &cookie);
    if (FAILED(hr))
      return hr;
    stop = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    thread = reinterpret_cast<HANDLE>(
        _beginthreadex(nullptr, 0, worker, this, 0, nullptr));
    return thread ? S_OK : E_FAIL;
  }
  HRESULT STDMETHODCALLTYPE UnAdvise() override {
    if (stop)
      SetEvent(stop);
    if (thread) {
      DWORD index = 0;
      CoWaitForMultipleHandles(COWAIT_DISPATCH_CALLS |
                                   COWAIT_DISPATCH_WINDOW_MESSAGES,
                               INFINITE, 1, &thread, &index);
      CloseHandle(thread);
      thread = nullptr;
    }
    if (stop) {
      CloseHandle(stop);
      stop = nullptr;
    }
    if (git) {
      if (cookie)
        git->RevokeInterfaceFromGlobal(cookie);
      git->Release();
      git = nullptr;
      cookie = 0;
    }
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE GetFieldDescriptorCount(DWORD *n) override {
    *n = 2;
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE GetFieldDescriptorAt(
      DWORD n, CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR **out) override {
    if (n > 1)
      return E_INVALIDARG;
    auto d = static_cast<CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR *>(
        CoTaskMemAlloc(sizeof(**out)));
    if (!d)
      return E_OUTOFMEMORY;
    ZeroMemory(d, sizeof(*d));
    d->dwFieldID = n;
    d->cpft = n ? CPFT_SUBMIT_BUTTON : CPFT_LARGE_TEXT;
    dup(n ? L"Submit" : L"Oink", &d->pszLabel);
    *out = d;
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE GetCredentialCount(DWORD *count, DWORD *def,
                                               BOOL *autologon) override {
    recover();
    const bool ready = cred && pending->load() != 0;
    *count = ready ? 1 : 0;
    *autologon = ready;
    *def = *autologon ? 0 : CREDENTIAL_PROVIDER_NO_DEFAULT;
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE
  GetCredentialAt(DWORD n, ICredentialProviderCredential **p) override {
    if (n || !cred)
      return E_INVALIDARG;
    return cred->QueryInterface(IID_PPV_ARGS(p));
  }
  HRESULT STDMETHODCALLTYPE
  SetUserArray(ICredentialProviderUserArray *) override {
    return S_OK;
  }
};
class Factory : public IClassFactory {
  LONG refs = 1;

public:
  Factory() { ++objects; }
  ~Factory() { --objects; }
  HRESULT STDMETHODCALLTYPE QueryInterface(REFIID i, void **p) override {
    *p = nullptr;
    if (i != IID_IUnknown && i != IID_IClassFactory)
      return E_NOINTERFACE;
    *p = this;
    AddRef();
    return S_OK;
  }
  ULONG STDMETHODCALLTYPE AddRef() override {
    return InterlockedIncrement(&refs);
  }
  ULONG STDMETHODCALLTYPE Release() override {
    LONG r = InterlockedDecrement(&refs);
    if (!r)
      delete this;
    return r;
  }
  HRESULT STDMETHODCALLTYPE CreateInstance(IUnknown *outer, REFIID i,
                                           void **p) override {
    if (outer)
      return CLASS_E_NOAGGREGATION;
    auto v = new Provider;
    HRESULT hr = v->QueryInterface(i, p);
    v->Release();
    return hr;
  }
  HRESULT STDMETHODCALLTYPE LockServer(BOOL b) override {
    objects += b ? 1 : -1;
    return S_OK;
  }
};
extern "C" HRESULT __stdcall DllGetClassObject(REFCLSID c, REFIID i, void **p) {
  if (!system() || session() != WTSGetActiveConsoleSessionId() ||
      !image(GetCurrentProcess(), windowsPath(L"\\System32\\LogonUI.exe")))
    return E_ACCESSDENIED;
  if (c != CLSID_Oink)
    return CLASS_E_CLASSNOTAVAILABLE;
  auto f = new Factory;
  HRESULT hr = f->QueryInterface(i, p);
  f->Release();
  return hr;
}
extern "C" HRESULT __stdcall DllCanUnloadNow() {
  return objects.load() ? S_FALSE : S_OK;
}
BOOL WINAPI DllMain(HINSTANCE, DWORD, LPVOID) { return TRUE; }
