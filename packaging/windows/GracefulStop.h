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
    case 4: return L"Oink's installation root or a parent has unsafe permissions or redirected files. See the installer log for the rejected path.";
    case 5: return L"Oink's maintenance storage could not be safely restored. A redirected file, hard link or inaccessible path was found. See the installer log.";
    case 6: return L"An Oink update transaction is still running. Let it finish, then run Setup again. If it remains unfinished, restart Windows and run the latest Oink installer.";
    case 8: return L"Oink is running. Quit Oink in every signed-in user session, then run Setup again. Setup could not complete the normal shutdown; no process was force-closed.";
    case 9: case 10: return L"Oink Setup could not verify the OinkInput Windows service. Check the service configuration and try again.";
    case 11: case 12: return L"Oink Setup could not finish its protected maintenance transaction. Close other installers and try again.";
    default: return L"Oink Setup could not verify the installation. See the installer log for details.";
    }
}
}
