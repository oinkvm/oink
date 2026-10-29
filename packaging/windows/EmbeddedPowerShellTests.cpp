// Harmless script transport only: no installer body, UAC or device operations.
#include "EmbeddedPowerShell.h"
#include <iostream>
int main() {
    wchar_t system[MAX_PATH]{};
    if (!GetSystemDirectoryW(system, MAX_PATH)) return 1;
    std::wstring executable = std::wstring(system) + L"\\WindowsPowerShell\\v1.0\\powershell.exe";
    if (embedded_powershell::run(executable, "exit 0", system) != 0) return 2;
    auto large = "#" + std::string(80'000, 'x') + "\nexit 7";
    if (embedded_powershell::run(executable, large, system) != 7) return 3;
    if (embedded_powershell::run(executable, "if ([int][char]'\xc3\xa9' -ne 233) { exit 9 }; exit 0", system) != 0) return 4;
    if (embedded_powershell::run(executable + L".missing", "exit 0", system) == 0) return 5;
    std::cout << "4 embedded PowerShell transport cases passed (80KB, UTF-8, exit status, launch failure)\n";
    return 0;
}
