#pragma once
#include <windows.h>
#include <string_view>

namespace package_drop {
inline bool installer_name(std::wstring_view path) {
    return path.size() >= 4 &&
        CompareStringOrdinal(path.data() + path.size() - 4, 4, L".msi", 4, TRUE) == CSTR_EQUAL;
}
}

using PackageHover = void (*)(bool);
using PackageReceived = void (*)(const wchar_t*);
using PackageFailure = void (*)(long);
extern "C" DWORD oink_package_drop_attach(HWND root, PackageHover hover, PackageReceived received, PackageFailure failure);
