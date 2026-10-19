#pragma once
#include <string>

namespace graceful_stop {
enum class State { clear, busy, unavailable, canceled };
inline bool permitted(const std::wstring& ui, const std::wstring& opt_in, bool nested,
                      bool scheduled, bool commit, bool rollback) {
    return !nested && !scheduled && !commit && !rollback && (ui == L"5" || opt_in == L"1");
}
// One graceful request, then a bounded observation period. A failed request,
// another user's session, or an unreadable process never authorizes replacement.
// Retry only observes again; it does not spawn repeated stop clients.
template<class Scan, class Stop, class Wait>
State prepare(bool request, Scan scan, Stop stop, Wait wait) {
    auto state = scan();
    if (state == State::clear || !request) return state;
    if (!stop()) return scan();
    for (unsigned tick = 0; tick != 60; ++tick) {
        if (!wait()) return State::canceled;
        state = scan();
        if (state == State::clear) return state;
    }
    return state;
}
inline const wchar_t* failure(int code) {
    switch (code) {
    case 1: case 3: return L"Oink Setup could not read the installation directory. Run this installer from a local file and try again.";
    case 2: return L"Oink Setup could not verify this package or its installation path. Use the original Oink installer and try again.";
    case 4: case 5: return L"Oink's installation has unsafe permissions or redirected files. Repair the protected installation before continuing.";
    case 6: return L"An Oink installation or update is already in progress, or the package could not be verified. Let it finish, then run Setup again.";
    case 8: return L"Oink is running. Quit Oink in every signed-in user session, then run Setup again. Quiet installs may request a graceful stop with OINK_GRACEFUL_STOP=1.";
    case 9: case 10: return L"Oink Setup could not verify the OinkInput Windows service. Check the service configuration and try again.";
    case 11: case 12: return L"Oink Setup could not finish its protected maintenance transaction. Close other installers and try again.";
    default: return L"Oink Setup could not verify the installation. See the installer log for details.";
    }
}
}
