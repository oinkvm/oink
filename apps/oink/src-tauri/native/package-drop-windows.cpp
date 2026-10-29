#include "package-drop-windows.h"
#include <ole2.h>
#include <shellapi.h>
#include <commctrl.h>
#include <atomic>
#include <map>
#include <string>
#include <vector>

namespace {
// Wry 0.57 enumerates WebView2 children once during creation. On Windows 11
// 26200 the visible browser/render HWNDs belong to another process and arrive
// later. OLE cannot reach the host's target across that process boundary.
// A rescan only on Show still races Chromium's asynchronous target replacement.
// Audit the live subtree instead, including after reparenting/renderer recovery.
// No input, clipboard, security policy or window visibility is changed here.
// See tauri-apps/wry#1639. "OleDropTargetInterface" is the OLE-owned registration
// property observed on Windows 10/11; it is read only to detect lost/replaced
// registrations and to avoid revoking a target that is no longer ours.
constexpr wchar_t registration_property[] = L"OleDropTargetInterface";
constexpr UINT_PTR subclass_id = 0x4f696e6b44526f70;

struct Target final : IDropTarget {
    std::atomic<ULONG> references{1};
    PackageHover hover;
    PackageReceived received;
    bool valid = false;
    Target(PackageHover h, PackageReceived r) : hover(h), received(r) {}
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid, void** result) override {
        if (!result) return E_POINTER;
        *result = nullptr;
        if (iid != IID_IUnknown && iid != IID_IDropTarget) return E_NOINTERFACE;
        *result = static_cast<IDropTarget*>(this); AddRef(); return S_OK;
    }
    ULONG STDMETHODCALLTYPE AddRef() override { return ++references; }
    ULONG STDMETHODCALLTYPE Release() override {
        const auto n = --references; if (!n) delete this; return n;
    }
    static std::wstring path(IDataObject* object) {
        if (!object) return {};
        FORMATETC format{CF_HDROP, nullptr, DVASPECT_CONTENT, -1, TYMED_HGLOBAL};
        STGMEDIUM medium{};
        if (FAILED(object->GetData(&format, &medium))) return {};
        std::wstring result;
        const auto drop = reinterpret_cast<HDROP>(medium.hGlobal);
        if (medium.tymed == TYMED_HGLOBAL && drop && DragQueryFileW(drop, MAXUINT, nullptr, 0) == 1) {
            const auto length = DragQueryFileW(drop, 0, nullptr, 0);
            if (length && length < 32768) {
                std::vector<wchar_t> value(length + 1);
                if (DragQueryFileW(drop, 0, value.data(), length + 1) == length &&
                    package_drop::installer_name({value.data(), length})) result.assign(value.data(), length);
            }
        }
        ReleaseStgMedium(&medium);
        return result;
    }
    HRESULT STDMETHODCALLTYPE DragEnter(IDataObject* object, DWORD, POINTL, DWORD* effect) override {
        if (!effect) return E_POINTER;
        valid = (*effect & DROPEFFECT_COPY) && !path(object).empty();
        hover(valid); *effect = valid ? DROPEFFECT_COPY : DROPEFFECT_NONE; return S_OK;
    }
    HRESULT STDMETHODCALLTYPE DragOver(DWORD, POINTL, DWORD* effect) override {
        if (!effect) return E_POINTER;
        *effect = valid ? DROPEFFECT_COPY : DROPEFFECT_NONE; return S_OK;
    }
    HRESULT STDMETHODCALLTYPE DragLeave() override { valid = false; hover(false); return S_OK; }
    HRESULT STDMETHODCALLTYPE Drop(IDataObject* object, DWORD, POINTL, DWORD* effect) override {
        if (!effect) return E_POINTER;
        const auto value = valid ? path(object) : std::wstring{};
        valid = false; hover(false); *effect = value.empty() ? DROPEFFECT_NONE : DROPEFFECT_COPY;
        if (!value.empty()) received(value.c_str());
        return S_OK;
    }
};

struct Registration {
    Target* target;
    HANDLE property;
};
struct Receiver {
    HWND root;
    PackageHover hover;
    PackageReceived received;
    PackageFailure failure;
    std::map<HWND, Registration> targets;
    bool scanning = false;
    bool closing = false;
    HRESULT last_error = S_OK;
    bool belongs(HWND window) const { return IsWindow(window) && GetAncestor(window, GA_ROOT) == root; }
    static void forget(HWND window, const Registration& registration) {
        if (GetPropW(window, registration_property) == registration.property) RevokeDragDrop(window);
        registration.target->Release();
    }
    void scan() {
        // RegisterDragDrop/RevokeDragDrop may pump messages. A nested timer or
        // Show callback must not mutate the map while this registration is live.
        if (scanning) return;
        scanning = true;
        std::vector<HWND> windows{root};
        EnumChildWindows(root, [](HWND window, LPARAM data) -> BOOL {
            reinterpret_cast<std::vector<HWND>*>(data)->push_back(window); return TRUE;
        }, reinterpret_cast<LPARAM>(&windows));
        for (auto it = targets.begin(); it != targets.end();) {
            if (!belongs(it->first)) { forget(it->first, it->second); it = targets.erase(it); }
            else ++it;
        }
        for (const auto window : windows) {
            if (closing) break;
            if (!belongs(window)) continue;
            auto found = targets.find(window);
            if (found != targets.end()) {
                if (GetPropW(window, registration_property) == found->second.property) continue;
                forget(window, found->second); targets.erase(found);
            }
            auto target = new Target(hover, received);
            RevokeDragDrop(window);
            const auto result = RegisterDragDrop(window, target);
            if (SUCCEEDED(result)) {
                const auto property = GetPropW(window, registration_property);
                if (property) targets.emplace(window, Registration{target, property});
                else { RevokeDragDrop(window); target->Release(); }
            } else {
                target->Release();
                if (result != last_error) { last_error = result; failure(result); }
            }
        }
        scanning = false;
        if (closing) delete this;
    }
    ~Receiver() {
        for (const auto& entry : targets) forget(entry.first, entry.second);
        hover(false);
        OleUninitialize();
    }
};

LRESULT CALLBACK lifecycle(HWND window, UINT message, WPARAM wparam, LPARAM lparam, UINT_PTR, DWORD_PTR data) {
    auto receiver = reinterpret_cast<Receiver*>(data);
    if (message == WM_TIMER && wparam == reinterpret_cast<UINT_PTR>(receiver)) {
        if (IsWindowVisible(window)) receiver->scan();
        return 0;
    }
    if (message == WM_SHOWWINDOW && wparam) receiver->scan();
    if (message == WM_NCDESTROY) {
        KillTimer(window, reinterpret_cast<UINT_PTR>(receiver));
        RemoveWindowSubclass(window, lifecycle, subclass_id);
        receiver->closing = true;
        if (!receiver->scanning) delete receiver;
    }
    return DefSubclassProc(window, message, wparam, lparam);
}
}

extern "C" DWORD oink_package_drop_attach(HWND root, PackageHover hover, PackageReceived received, PackageFailure failure) {
    if (!IsWindow(root) || !hover || !received || !failure) return ERROR_INVALID_PARAMETER;
    DWORD_PTR existing = 0;
    if (GetWindowSubclass(root, lifecycle, subclass_id, &existing)) return ERROR_SUCCESS;
    const auto ole = OleInitialize(nullptr);
    if (FAILED(ole)) return static_cast<DWORD>(ole);
    auto receiver = new Receiver{root, hover, received, failure};
    if (!SetWindowSubclass(root, lifecycle, subclass_id, reinterpret_cast<DWORD_PTR>(receiver))) {
        const auto error = GetLastError(); delete receiver; return error ? error : ERROR_GEN_FAILURE;
    }
    // This is a continuing registration audit, not a delayed startup retry.
    // It also repairs Chromium revoking/replacing a target without creating an HWND.
    if (!SetTimer(root, reinterpret_cast<UINT_PTR>(receiver), 250, nullptr)) {
        const auto error = GetLastError(); RemoveWindowSubclass(root, lifecycle, subclass_id);
        delete receiver; return error ? error : ERROR_GEN_FAILURE;
    }
    receiver->scan();
    return ERROR_SUCCESS;
}
