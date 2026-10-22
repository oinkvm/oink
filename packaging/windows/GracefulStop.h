#pragma once
#include <string>

namespace graceful_stop {
enum class State { clear, busy, unavailable, canceled };
inline bool permitted(const std::wstring& ui, bool nested,
                      bool scheduled, bool commit, bool rollback) {
    return !nested && !scheduled && !commit && !rollback && (ui == L"2" || ui == L"3" || ui == L"4" || ui == L"5");
}
// One request through normal product Quit. The authoritative deferred scan runs
// after MSI StopServices has notified every installed backend's SCM subscriber.
// Observe real process exit for at most 15 seconds; never force a busy process.
template<class Scan, class Wait>
State observe(Scan scan, Wait wait) {
    auto state = scan();
    for (unsigned tick = 0; state != State::clear && tick != 60; ++tick) {
        if (!wait()) return State::canceled;
        state = scan();
    }
    return state;
}
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
    case 8: return L"Oink is running. Quit Oink in every signed-in user session, then run Setup again. Setup could not complete the normal shutdown; no process was force-closed.";
    case 9: case 10: return L"Oink Setup could not verify the OinkInput Windows service. Check the service configuration and try again.";
    case 11: case 12: return L"Oink Setup could not finish its protected maintenance transaction. Close other installers and try again.";
    default: return L"Oink Setup could not verify the installation. See the installer log for details.";
    }
}
}
