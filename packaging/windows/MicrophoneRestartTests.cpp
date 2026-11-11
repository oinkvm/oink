// Pure prompt decision only: no registry writes, MSI session or restart.
#include "MicrophoneRestart.h"
#include <cassert>
#include <iostream>

int main() {
    using microphone_restart::prompt;
    unsigned cases = 0;
    for (const auto ui : {L"", L"2", L"3", L"4", L"5"})
        for (const auto mode : {L"", L"S", L"Suppress"})
            for (bool marker : {false, true}) {
                assert(prompt(ui, mode, marker) == (marker && _wtoi(ui) >= 3 && std::wstring(mode).empty()));
                ++cases;
            }
    // The two runs that matter most: Settings uninstall prompts, the updater's silent removal never does.
    assert(prompt(L"3", L"", true));
    assert(!prompt(L"2", L"", true));
    assert(!prompt(L"3", L"", false));
    bool present = true;
    // Read-only query of the real machine: it must either succeed or report the key missing.
    const LONG read = microphone_restart::marker(present);
    assert(read == ERROR_SUCCESS);
    (void)read;
    std::cout << cases << " microphone restart prompt cases passed (UILevel, REBOOTPROMPT, marker)\n";
    return 0;
}
