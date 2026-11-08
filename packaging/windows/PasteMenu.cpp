// State handler for a classic Explorer verb (Windows 11: Show more options).
#include <windows.h>
#include <shobjidl.h>
#include <initializer_list>
#include <new>
static const CLSID kClass = {0x87d83265,0x289b,0x446d,{0xa5,0xf9,0xe4,0xb9,0xda,0x83,0xa8,0x75}};
static LONG objects = 0;
static bool available() {
    // Type queries only: constructing a menu must not render a delayed owner
    // or hydrate Oink's remote-file placeholders.
    for (UINT f : {CF_HDROP,CF_UNICODETEXT,CF_TEXT,CF_OEMTEXT,CF_TIFF,CF_DIB,CF_DIBV5,CF_BITMAP})
        if (IsClipboardFormatAvailable(f)) return true;
    for (const wchar_t* name : {L"PNG",L"JPEG",L"JFIF",L"GIF",L"WebP",L"HEIC",L"HEIF",L"AVIF",L"BMP",
            L"image/png",L"image/jpeg",L"image/gif",L"image/webp",L"image/heic",L"image/heif",L"image/avif",L"image/bmp",
            L"HTML Format",L"Rich Text Format",L"FileGroupDescriptorW",L"FileGroupDescriptor"})
        if (IsClipboardFormatAvailable(RegisterClipboardFormatW(name))) return true;
    return false;
}
class State final : public IExplorerCommandState {
    LONG refs = 1;
public:
    State() { InterlockedIncrement(&objects); }
    ~State() { InterlockedDecrement(&objects); }
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid, void** out) override {
        if (!out) return E_POINTER;
        *out = nullptr;
        if (iid == IID_IUnknown || iid == IID_IExplorerCommandState) { *out = static_cast<IExplorerCommandState*>(this); AddRef(); return S_OK; }
        return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef() override { return InterlockedIncrement(&refs); }
    ULONG STDMETHODCALLTYPE Release() override { ULONG n=InterlockedDecrement(&refs); if (!n) delete this; return n; }
    HRESULT STDMETHODCALLTYPE GetState(IShellItemArray*, BOOL, EXPCMDSTATE* state) override {
        if (!state) return E_POINTER;
        *state=available()?ECS_ENABLED:ECS_DISABLED; return S_OK;
    }
};
class Factory final : public IClassFactory {
    LONG refs=1;
public:
    Factory() { InterlockedIncrement(&objects); }
    ~Factory() { InterlockedDecrement(&objects); }
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid, void** out) override {
        if (!out) return E_POINTER;
        *out=nullptr;
        if (iid==IID_IUnknown || iid==IID_IClassFactory) { *out=static_cast<IClassFactory*>(this); AddRef(); return S_OK; }
        return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef() override { return InterlockedIncrement(&refs); }
    ULONG STDMETHODCALLTYPE Release() override { ULONG n=InterlockedDecrement(&refs); if (!n) delete this; return n; }
    HRESULT STDMETHODCALLTYPE CreateInstance(IUnknown* outer, REFIID iid, void** out) override {
        if (outer) return CLASS_E_NOAGGREGATION;
        State* obj=new(std::nothrow) State(); if (!obj) return E_OUTOFMEMORY;
        HRESULT hr=obj->QueryInterface(iid,out); obj->Release(); return hr;
    }
    HRESULT STDMETHODCALLTYPE LockServer(BOOL lock) override { if (lock) InterlockedIncrement(&objects); else InterlockedDecrement(&objects); return S_OK; }
};
STDAPI DllGetClassObject(REFCLSID clsid, REFIID iid, void** out) {
    if (clsid!=kClass) return CLASS_E_CLASSNOTAVAILABLE;
    Factory* obj=new(std::nothrow) Factory(); if (!obj) return E_OUTOFMEMORY;
    HRESULT hr=obj->QueryInterface(iid,out); obj->Release(); return hr;
}
STDAPI DllCanUnloadNow() { return objects==0?S_OK:S_FALSE; }
