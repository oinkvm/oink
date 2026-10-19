#include "GracefulStop.h"
#include <cassert>
#include <iostream>
int main() {
    using namespace graceful_stop;
    unsigned checks = 0;
    for (const auto ui : {L"2", L"3", L"4", L"5"})
        for (const auto opt : {L"", L"0", L"1", L"yes"})
            for (unsigned modes = 0; modes != 16; ++modes) {
                assert(permitted(ui, opt, modes & 1, modes & 2, modes & 4, modes & 8) ==
                    (modes == 0 && (std::wstring(ui) == L"5" || std::wstring(opt) == L"1")));
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
        ++checks;
    }
    assert(prepare(true, [] { return State::busy; }, [] { return true; }, [] { return false; }) == State::canceled);
    for (int code = 1; code != 13; ++code) {
        assert(std::wstring(failure(code)).find(L"[1]") == std::wstring::npos);
        assert(std::wstring(failure(code)).size() > 30); ++checks;
    }
    std::cout << "graceful stop decision checks=" << checks << '\n';
}
