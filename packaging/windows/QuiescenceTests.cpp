#include "Quiescence.h"
#include <cassert>
#include <iostream>

int main() {
    using namespace quiescence;
    unsigned checks = 0;
    // All three maintenance operations use the same two unconditional CA gates.
    for (const auto operation : {"upgrade", "uninstall", "repair"}) {
        const Process service{100, 4, 0, 1000, true, true};
        const Process helper{200, 100, 1, 2000, true, true};
        auto check = [&](bool expected, Process process, const Process* broker = nullptr,
                         Phase phase = Phase::before_stop) {
            assert(service_owned(process, broker ? *broker : service, phase) == expected);
            ++checks;
        };
        check(true, service);
        check(true, helper);
        auto changed = helper; changed.session = 2; check(true, changed);
        changed = helper; changed.system = false; check(false, changed); // User impersonator.
        changed = helper; changed.service_image = false; check(false, changed); // UI/CLI/collector or another path.
        changed = helper; changed.parent = 99; check(false, changed);
        changed = helper; changed.session = 0; check(false, changed);
        changed = helper; changed.created = 999; check(false, changed); // Reused parent PID.
        changed = helper; changed.created = 0; check(false, changed); // Query failure.
        changed = service; changed.created = 1001; check(false, changed); // Reused service PID.
        changed = service; changed.system = false; check(false, helper, &changed);
        changed = service; changed.service_image = false; check(false, helper, &changed);
        changed = service; changed.session = 1; check(false, helper, &changed);
        changed = service; changed.created = 0; check(false, helper, &changed);
        const Process absent{};
        check(false, helper, &absent); // No SCM service: orphan helper must block.
        check(false, service, nullptr, Phase::after_stop);
        check(false, helper, nullptr, Phase::after_stop); // StopServices must reap helper.
        check(false, helper, &absent, Phase::after_stop);
        std::cout << operation << "_quiescence_checks=18\n";
    }
    std::cout << "quiescence_checks=" << checks << '\n';
}
