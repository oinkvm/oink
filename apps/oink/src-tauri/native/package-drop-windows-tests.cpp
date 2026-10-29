// Run only in the owned Windows test VM: this fixture creates native windows.
#include "package-drop-windows.cpp"
#include <cassert>
#include <functional>
#include <iostream>
#include <shlobj.h>
#include <cstdlib>

namespace {
unsigned hovered = 0, dropped = 0, failures = 0;
std::wstring last_path;
void hover(bool active) { if (active) ++hovered; }
void received(const wchar_t* value) { ++dropped; last_path = value; }
void failure(long) { ++failures; }
void pump_until(const std::function<bool()>& ready) {
    const auto end = GetTickCount64() + 4000;
    while (!ready()) {
        assert(GetTickCount64() < end);
        MSG message{};
        while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&message); DispatchMessageW(&message);
        }
        Sleep(5);
    }
}
struct Data final : IDataObject {
    std::atomic<ULONG> refs{1};
    std::vector<std::wstring> names;
    explicit Data(std::vector<std::wstring> value) : names(std::move(value)) {}
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid, void** out) override {
        if (!out) return E_POINTER; *out = nullptr;
        if (iid != IID_IUnknown && iid != IID_IDataObject) return E_NOINTERFACE;
        *out = static_cast<IDataObject*>(this); AddRef(); return S_OK;
    }
    ULONG STDMETHODCALLTYPE AddRef() override { return ++refs; }
    ULONG STDMETHODCALLTYPE Release() override { auto n = --refs; if (!n) delete this; return n; }
    HRESULT STDMETHODCALLTYPE GetData(FORMATETC* format, STGMEDIUM* out) override {
        if (format->cfFormat != CF_HDROP) return DV_E_FORMATETC;
        size_t count = 1;
        for (const auto& name : names) count += name.size() + 1;
        auto memory = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, sizeof(DROPFILES) + count * sizeof(wchar_t));
        auto data = static_cast<DROPFILES*>(GlobalLock(memory)); data->pFiles = sizeof(DROPFILES); data->fWide = TRUE;
        auto text = reinterpret_cast<wchar_t*>(data + 1);
        for (const auto& name : names) { memcpy(text, name.c_str(), (name.size() + 1) * sizeof(wchar_t)); text += name.size() + 1; }
        GlobalUnlock(memory); out->tymed = TYMED_HGLOBAL; out->hGlobal = memory; out->pUnkForRelease = nullptr; return S_OK;
    }
    HRESULT STDMETHODCALLTYPE GetDataHere(FORMATETC*, STGMEDIUM*) override { return E_NOTIMPL; }
    HRESULT STDMETHODCALLTYPE QueryGetData(FORMATETC* f) override { return f->cfFormat == CF_HDROP ? S_OK : DV_E_FORMATETC; }
    HRESULT STDMETHODCALLTYPE GetCanonicalFormatEtc(FORMATETC*, FORMATETC*) override { return E_NOTIMPL; }
    HRESULT STDMETHODCALLTYPE SetData(FORMATETC*, STGMEDIUM*, BOOL) override { return E_NOTIMPL; }
    HRESULT STDMETHODCALLTYPE EnumFormatEtc(DWORD, IEnumFORMATETC**) override { return E_NOTIMPL; }
    HRESULT STDMETHODCALLTYPE DAdvise(FORMATETC*, DWORD, IAdviseSink*, DWORD*) override { return OLE_E_ADVISENOTSUPPORTED; }
    HRESULT STDMETHODCALLTYPE DUnadvise(DWORD) override { return OLE_E_ADVISENOTSUPPORTED; }
    HRESULT STDMETHODCALLTYPE EnumDAdvise(IEnumSTATDATA**) override { return OLE_E_ADVISENOTSUPPORTED; }
};
DWORD exercise(Receiver* receiver, HWND destination, std::vector<std::wstring> paths) {
    auto data = new Data(std::move(paths));
    auto target = receiver->targets.at(destination).target;
    DWORD effect = DROPEFFECT_COPY; POINTL point{};
    target->DragEnter(data, 0, point, &effect);
    if (effect) target->Drop(data, 0, point, &effect); else target->DragLeave();
    data->Release(); return effect;
}
HWND window(HWND parent, const wchar_t* name) {
    WNDCLASSW cls{}; cls.hInstance = GetModuleHandleW(nullptr); cls.lpfnWndProc = DefWindowProcW;
    cls.lpszClassName = name; cls.hbrBackground = reinterpret_cast<HBRUSH>(COLOR_WINDOW + 1);
    RegisterClassW(&cls);
    auto result = CreateWindowW(name, name, parent ? WS_CHILD | WS_VISIBLE : WS_OVERLAPPEDWINDOW | WS_VISIBLE,
        parent ? 0 : 100, parent ? 0 : 100, 500, 350, parent, nullptr, cls.hInstance, nullptr);
    assert(result); return result;
}
PROCESS_INFORMATION child(HWND root) {
    wchar_t exe[MAX_PATH]{}; GetModuleFileNameW(nullptr, exe, MAX_PATH);
    auto command = std::wstring(L"\"") + exe + L"\" --child " + std::to_wstring(reinterpret_cast<UINT_PTR>(root));
    STARTUPINFOW startup{sizeof(startup)}; PROCESS_INFORMATION process{};
    assert(CreateProcessW(exe, command.data(), nullptr, nullptr, FALSE, CREATE_NO_WINDOW, nullptr, nullptr, &startup, &process));
    CloseHandle(process.hThread); return process;
}
}

int main(int argc, char** argv) {
    std::cout << std::unitbuf;
    _set_error_mode(_OUT_TO_STDERR);
    _set_abort_behavior(0, _WRITE_ABORT_MSG | _CALL_REPORTFAULT);
    if (argc == 3 && std::string(argv[1]) == "--child") {
        auto root = reinterpret_cast<HWND>(std::stoull(argv[2]));
        auto outer = window(root, L"OinkForeignBrowser"); window(outer, L"OinkForeignRender");
        MSG message{};
        while (IsWindow(root) && IsWindow(outer)) {
            while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) { TranslateMessage(&message); DispatchMessageW(&message); }
            Sleep(5);
        }
        return 0;
    }
    using namespace package_drop;
    assert(installer_name(L"C:\\Downloads\\Oink.MSI"));
    assert(!installer_name(L"C:\\Downloads\\Oink.msi.txt"));
    assert(!installer_name(L"C:\\Downloads\\Oink.dmg"));
    assert(!installer_name(L""));
    std::cout << "installer filename policy passed\n";
    if (argc != 2 || std::string(argv[1]) != "--owned-vm") return 0;
    assert(SUCCEEDED(OleInitialize(nullptr)));
    auto root = window(nullptr, L"OinkDropRegression");
    SetForegroundWindow(root);
    auto old_target = new Target(hover, received);
    assert(SUCCEEDED(RegisterDragDrop(root, old_target))); old_target->Release();
    // Wry's initial enumeration has finished. The visible render window then
    // arrives in a different process; a registered host ancestor is insufficient.
    auto first = child(root); HWND browser = nullptr, render = nullptr;
    pump_until([&] { browser = FindWindowExW(root, nullptr, L"OinkForeignBrowser", nullptr);
        render = browser ? FindWindowExW(browser, nullptr, L"OinkForeignRender", nullptr) : nullptr; return render != nullptr; });
    assert(!GetPropW(render, registration_property));
    std::cout << "red: one-shot host registration leaves the late foreign render HWND unregistered\n";
    assert(oink_package_drop_attach(root, hover, received, failure) == ERROR_SUCCESS);
    std::cout << "native receiver attached\n";
    assert(GetPropW(render, registration_property));
    DWORD_PTR state = 0; assert(GetWindowSubclass(root, lifecycle, subclass_id, &state));
    auto receiver = reinterpret_cast<Receiver*>(state);
    assert(exercise(receiver, render, {L"C:\\candidate.msi"}) == DROPEFFECT_COPY && dropped == 1 && hovered);
    assert(last_path == L"C:\\candidate.msi");
    assert(exercise(receiver, render, {L"C:\\other.dmg"}) == DROPEFFECT_NONE && dropped == 1);
    assert(exercise(receiver, render, {L"C:\\one.msi", L"C:\\two.msi"}) == DROPEFFECT_NONE && dropped == 1);
    // Chromium may revoke a target without changing the HWND. The live audit
    // detects the lost OLE registration rather than trusting an old HWND set.
    RevokeDragDrop(render); assert(!GetPropW(render, registration_property));
    pump_until([&] { return GetPropW(render, registration_property) != nullptr; });
    assert(exercise(receiver, render, {L"C:\\same.msi"}) == DROPEFFECT_COPY && dropped == 2);
    SendMessageW(browser, WM_CLOSE, 0, 0);
    assert(WaitForSingleObject(first.hProcess, 4000) == WAIT_OBJECT_0); CloseHandle(first.hProcess);
    auto second = child(root); render = nullptr;
    pump_until([&] { browser = FindWindowExW(root, nullptr, L"OinkForeignBrowser", nullptr);
        render = browser ? FindWindowExW(browser, nullptr, L"OinkForeignRender", nullptr) : nullptr;
        return render && GetPropW(render, registration_property); });
    assert(exercise(receiver, render, {L"C:\\recovered.msi"}) == DROPEFFECT_COPY && dropped == 3);
    assert(!failures);
    DestroyWindow(root);
    assert(WaitForSingleObject(second.hProcess, 4000) == WAIT_OBJECT_0); CloseHandle(second.hProcess);
    OleUninitialize();
    std::cout << "green: native COM MSI filtering, revoke repair, late/recreated cross-process renderer, teardown passed\n";
}
