// Prompt decision and the marker hand-off on a scratch per-user registry root:
// no HKLM writes, MSI session or restart.
#include "MicrophoneRestart.h"
#include <cassert>
#include <iostream>

int main() {
    using namespace microphone_restart;
    unsigned cases = 0;
    for (const auto ui : {L"", L"2", L"3", L"4", L"5"})
        for (const auto mode : {L"", L"S", L"Suppress"})
            for (bool owed : {false, true}) {
                assert(prompt(ui, mode, owed) == (owed && _wtoi(ui) >= 3 && std::wstring(mode).empty()));
                ++cases;
            }
    // The two runs that matter most: Settings uninstall prompts, the updater's silent removal never does.
    assert(prompt(L"3", L"", true));
    assert(!prompt(L"2", L"", true));
    assert(!prompt(L"3", L"", false));

    const std::wstring base = L"Software\\OinkRestartHandoffTest-" + std::to_wstring(GetCurrentProcessId());
    const std::wstring product = base + L"\\Oink", marker = product + L"\\MicrophoneRestartRequired";
    const std::wstring signal = base + L"\\OinkUninstallRestartRequired";
    HKEY key = nullptr;
    assert(RegCreateKeyExW(HKEY_CURRENT_USER, product.c_str(), 0, nullptr, 0, KEY_WRITE, nullptr, &key, nullptr) == ERROR_SUCCESS);
    RegCloseKey(key);
    bool owed = true, present = true;
    // No restart owed: nothing is carried and the check finds nothing.
    assert(carry(HKEY_CURRENT_USER, marker.c_str(), signal.c_str(), owed) == ERROR_SUCCESS && !owed);
    assert(take(HKEY_CURRENT_USER, signal.c_str(), present) == ERROR_SUCCESS && !present);
    // Owed restart survives the product tree's deletion (CleanupUserData) through the signal.
    assert(RegCreateKeyExW(HKEY_CURRENT_USER, marker.c_str(), 0, nullptr, REG_OPTION_VOLATILE, KEY_WRITE, nullptr, &key, nullptr) == ERROR_SUCCESS);
    RegCloseKey(key);
    assert(carry(HKEY_CURRENT_USER, marker.c_str(), signal.c_str(), owed) == ERROR_SUCCESS && owed);
    assert(RegDeleteTreeW(HKEY_CURRENT_USER, product.c_str()) == ERROR_SUCCESS);
    assert(take(HKEY_CURRENT_USER, signal.c_str(), present) == ERROR_SUCCESS && present);
    // The check consumes the signal.
    assert(exists(HKEY_CURRENT_USER, signal.c_str(), present) == ERROR_SUCCESS && !present);
    // A non-volatile child under the carried signal is refused, so it is volatile.
    assert(RegCreateKeyExW(HKEY_CURRENT_USER, marker.c_str(), 0, nullptr, REG_OPTION_VOLATILE, KEY_WRITE, nullptr, &key, nullptr) == ERROR_SUCCESS);
    RegCloseKey(key);
    assert(carry(HKEY_CURRENT_USER, marker.c_str(), signal.c_str(), owed) == ERROR_SUCCESS && owed);
    HKEY child = nullptr;
    assert(RegCreateKeyExW(HKEY_CURRENT_USER, (signal + L"\\child").c_str(), 0, nullptr, 0, KEY_WRITE, nullptr, &child, nullptr) == ERROR_CHILD_MUST_BE_VOLATILE);
    assert(RegDeleteTreeW(HKEY_CURRENT_USER, base.c_str()) == ERROR_SUCCESS);
    cases += 4;
    std::cout << cases << " microphone restart prompt and hand-off cases passed (UILevel, REBOOTPROMPT, marker, signal)\n";
    return 0;
}
