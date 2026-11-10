#include "Relaunch.h"
#include <cassert>
#include <iostream>

int main() {
    using namespace relaunch;
    const std::wstring user = L"S-1-5-21-1004336348-1177238915-682003330-1001";
    const std::wstring other = L"S-1-5-21-1004336348-1177238915-682003330-1002";
    for (const auto sid : {L"S-1-5-18", L"S-1-5-21-1-2-3-1001", user.c_str()}) assert(valid_sid(sid));
    for (const auto sid : {L"", L"S-1-", L"S-1-5-", L"S-1--5", L"S-2-5-21", L"s-1-5-21", L"S-1-5-21-1\"", L"S-1-5-21-1;2",
                           L"S-1-5-21-1:2", L"S-1-5-21-\x0661"})
        assert(!valid_sid(sid));

    std::vector<Session> sessions;
    add(sessions, 1, user);
    add(sessions, 1, user);   // UI and backend of the same user collapse.
    add(sessions, 1, other);  // A runas process in the same session stays distinct.
    add(sessions, 2, user);
    add(sessions, 0, user);   // Session 0 never hosts an interactive UI.
    add(sessions, 3, L"S-1-5-21-\"");
    assert((sessions == std::vector<Session>{{1, user}, {1, other}, {2, user}}));

    const auto data = encode(sessions);
    assert(data == L"1:" + user + L";1:" + other + L";2:" + user);
    std::vector<Session> decoded{{9, user}};
    assert(decode(data, decoded) && decoded == sessions);
    assert(decode(L"", decoded) && decoded.empty());
    for (const auto bad : {L";", L"1:", L":S-1-5-18", L"0:S-1-5-18", L"4294967296:S-1-5-18", L"12345678901:S-1-5-18",
                           L"1:S-1-5-18;", L"1:S-1-5-18;;2:S-1-5-18", L"x:S-1-5-18", L"1 :S-1-5-18", L"1:S-1-5-18:2"})
        assert(!decode(bad, decoded));
    assert(decode(L"4294967295:S-1-5-18", decoded) && decoded.size() == 1 && decoded[0].id == 4294967295ul);

    assert(json({}) == "{\"schema\":1,\"sessions\":[]}");
    assert(json({{1, L"S-1-5-21-1-2-3-1001"}, {2, L"S-1-5-18"}}) ==
           "{\"schema\":1,\"sessions\":[{\"id\":1,\"sid\":\"S-1-5-21-1-2-3-1001\"},{\"id\":2,\"sid\":\"S-1-5-18\"}]}");
    std::cout << "relaunch session record, CustomActionData and service JSON checks passed\n";
}
