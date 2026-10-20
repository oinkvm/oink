#include "FinishInstall.h"
#include <cassert>
#include <sddl.h>
#include <iostream>

int main() {
    using namespace finish_install;
    for (const auto ui : {L"2", L"3", L"4", L"5"}) {
        for (unsigned mask = 0; mask != 32; ++mask) {
            assert(allowed(ui, mask & 1, mask & 2, mask & 4, mask & 8, mask & 16) ==
                   (std::wstring(ui) == L"5" && mask == 0));
        }
    }
    for (const auto type : {TokenElevationTypeLimited, TokenElevationTypeDefault, TokenElevationTypeFull})
        for (bool elevated : {false, true}) for (DWORD uiaccess : {0UL, 1UL})
            for (DWORD session : {0UL, 1UL, 8UL}) for (bool service : {false, true}) {
                // type/elevated intentionally have no bearing on authorization.
                (void)type; (void)elevated;
                assert(normal_flags(uiaccess, session, service) == (!uiaccess && session && !service));
            }
    assert(!normal_flags(0, 1, false, 2));
    assert(normal_flags(0, 2, false, 2));
    for (const auto text : {L"S-1-5-18", L"S-1-5-19", L"S-1-5-20", L"S-1-5-80-1-2-3-4-5", L"S-1-5-82-1-2-3-4-5"}) {
        PSID sid = nullptr;
        assert(ConvertStringSidToSidW(text, &sid));
        assert(service_account(sid));
        LocalFree(sid);
    }
    PSID user = nullptr;
    assert(ConvertStringSidToSidW(L"S-1-5-21-1-2-3-1001", &user));
    assert(!service_account(user));
    LocalFree(user);
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
    // A server/SYSTEM caller cannot create any child, even a harmless
    // nonexistent one. Native interactive child-token checking is also enforced
    // in launch() before ResumeThread, never inferred from MSI properties.
    if (!normal_user(GetCurrentProcess())) assert(!launch(L"missing.exe", L"", L"."));
    std::cout << "finish UI/token/focus/helper policy: matrix and native token checks passed\n";
}
