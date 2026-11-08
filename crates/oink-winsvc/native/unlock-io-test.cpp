// Isolated Windows kernel-I/O tests. Never start the service, touch its vault,
// register a provider, attach a desktop, or submit authentication credentials.
#include "unlock.cpp"
#include <cassert>
#include <thread>

static HANDLE testPipe() {
  GUID nonce{};
  assert(SUCCEEDED(CoCreateGuid(&nonce)));
  wchar_t text[40]{};
  StringFromGUID2(nonce, text, 40);
  std::wstring name = L"\\\\.\\pipe\\Oink.Unlock.Unit." + std::wstring(text);
  return CreateNamedPipeW(
      name.c_str(),
      PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
      PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS, 1,
      4096, 4096, 0, nullptr);
}
int main() {
  stopEvent = CreateEventW(nullptr, TRUE, FALSE, nullptr);
  sessionChanged.store(CreateEventW(nullptr, TRUE, FALSE, nullptr));
  assert(stopEvent && sessionChanged.load());
  for (HANDLE signal : {stopEvent, sessionChanged.load()}) {
    ResetEvent(stopEvent);
    ResetEvent(sessionChanged.load());
    Handle pipe(testPipe()), done(CreateEventW(nullptr, TRUE, FALSE, nullptr));
    assert(pipe.h != INVALID_HANDLE_VALUE && done.h);
    bool connected = true;
    std::thread waiter([&] {
      connected = connect(pipe.h);
      SetEvent(done.h);
    });
    // An empty listener parks until an event. This is a real kernel wait, not
    // a simulation of the system service or an unlock test.
    assert(WaitForSingleObject(done.h, 150) == WAIT_TIMEOUT);
    SetEvent(signal);
    assert(WaitForSingleObject(done.h, 1000) == WAIT_OBJECT_0);
    waiter.join();
    assert(!connected);
    DisconnectNamedPipe(
        pipe.h); // cancellation has completed before buffers drop
  }
  CloseHandle(stopEvent);
  CloseHandle(sessionChanged.load());
  stopEvent = nullptr;
  sessionChanged.store(nullptr);
}
