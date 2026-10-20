#pragma once
#include <string>
#include <initializer_list>

namespace quiescence {
inline bool product_path(const std::wstring& directory, const std::wstring& image,
                         std::initializer_list<const wchar_t*> roles) {
    for (const auto role : roles)
        if (_wcsicmp(image.c_str(), (directory + L"\\" + role).c_str()) == 0) return true;
    return false;
}
}
