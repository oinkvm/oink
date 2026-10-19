#include "FinishInstall.h"
#include <cassert>
#include <iostream>

int main() {
    using namespace finish_install;
    for (const auto ui : {L"2", L"3", L"4", L"5"}) {
        for (unsigned mask = 0; mask != 32; ++mask) {
            assert(allowed(ui, mask & 1, mask & 2, mask & 4, mask & 8, mask & 16) ==
                   (std::wstring(ui) == L"5" && mask == 0));
        }
    }
    assert(normal_flags(false, 0, 1));
    assert(!normal_flags(true, 0, 1));
    assert(!normal_flags(false, 1, 1));
    assert(!normal_flags(false, 0, 0));
    assert(!normal_flags(false, 0, 1, 2));
    assert(normal_flags(false, 0, 2, 2));
    for (bool normal : {false, true}) for (bool launch : {false, true})
        for (bool granted : {false, true}) for (bool resumed : {false, true}) {
            unsigned grants = 0, resumes = 0;
            assert(resume_child(normal, launch, [&] { ++grants; return granted; },
                [&] { ++resumes; return resumed; }) == (normal && resumed));
            assert(grants == unsigned(normal && launch));
            assert(resumes == unsigned(normal));
        }
    assert(helper_completed(WAIT_OBJECT_0, true, ERROR_SUCCESS));
    assert(!helper_completed(WAIT_OBJECT_0, true, ERROR_ACCESS_DENIED));
    assert(!helper_completed(WAIT_OBJECT_0, false, ERROR_SUCCESS));
    assert(!helper_completed(WAIT_TIMEOUT, true, ERROR_SUCCESS));
    assert(!helper_completed(WAIT_FAILED, true, ERROR_SUCCESS));
    // A server/SYSTEM/elevated caller cannot create any child, even a harmless
    // nonexistent one. Native interactive child-token checking is also enforced
    // in launch() before ResumeThread, never inferred from MSI properties.
    if (!normal_user(GetCurrentProcess())) assert(!launch(L"missing.exe", L"", L"."));
    std::cout << "finish UI/token/focus/helper policy: 155 cases passed\n";
}
