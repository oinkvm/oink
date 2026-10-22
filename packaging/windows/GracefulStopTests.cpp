#include "GracefulStop.h"
#include <cassert>
#include <iostream>
int main() {
    using namespace graceful_stop;
    unsigned checks = 0;
    // Basic/quiet Settings uninstall gets the same normal Quit request as full UI.
    for (const auto ui : {L"2", L"3", L"4", L"5"})
        for (unsigned modes = 0; modes != 16; ++modes) {
            assert(permitted(ui, modes & 1, modes & 2, modes & 4, modes & 8) == (modes == 0));
            ++checks;
        }
    for (auto initial : {State::clear, State::busy, State::unavailable})
        for (bool request : {false, true})
            for (bool responds : {false, true}) {
                unsigned scans = 0, stops = 0, waits = 0;
                const auto result = prepare(request, [&] { ++scans; return initial; },
                    [&] { ++stops; return responds; }, [&] { ++waits; return true; });
                assert(result == initial);
                assert(stops == (initial != State::clear && request ? 1u : 0u));
                assert(waits == (stops && responds ? 60u : 0u));
                assert(scans == waits + 1 + (stops && !responds ? 1u : 0u));
                ++checks;
            }
    for (const auto operation : {"install", "upgrade", "repair", "uninstall"}) {
        unsigned scans = 0, stops = 0, waits = 0;
        assert(prepare(true, [&] { return ++scans == 4 ? State::clear : State::busy; },
            [&] { ++stops; return true; }, [&] { ++waits; return true; }) == State::clear);
        assert(stops == 1 && waits == 3);
        assert(prepare(false, [] { return State::clear; }, [] { assert(false); return false; }, [] { assert(false); return true; }) == State::clear);
        std::cout << operation << ": graceful success and retry after manual quit passed\n";
        // StopServices has notified ALL installed sessions; the deferred CA
        // waits for real exit rather than failing on the first observation.
        scans = 0; waits = 0;
        assert(observe([&] { return ++scans == 4 ? State::clear : State::busy; },
            [&] { ++waits; return true; }) == State::clear);
        assert(waits == 3);
        waits = 0;
        assert(observe([] { return State::busy; }, [&] { ++waits; return true; }) == State::busy);
        assert(waits == 60); // Bounded failure, never terminate a process.
        ++checks;
    }
    assert(prepare(true, [] { return State::busy; }, [] { return true; }, [] { return false; }) == State::canceled);
    for (int code = 1; code != 13; ++code) {
        assert(std::wstring(failure(code)).find(L"[1]") == std::wstring::npos);
        assert(std::wstring(failure(code)).size() > 30); ++checks;
    }
    std::cout << "graceful stop decision checks=" << checks << '\n';
}
