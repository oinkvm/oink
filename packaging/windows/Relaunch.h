#pragma once
#include <string>
#include <vector>

// Standalone MSI relaunch (issue #36). The immediate PrepareUserSessions action
// records each (session, user) whose installed Oink is running before it asks
// Oink to quit: afterwards neither the caller's normal Quit nor StopServices
// leaves a process for a deferred action to observe. The commit-only
// RelaunchUserSessions action publishes that list for the new OinkInput
// service, which reopens the ordinary UI with the session's own logon token.
namespace relaunch {
struct Session {
    unsigned long id;
    std::wstring sid;
    bool operator==(const Session& other) const { return id == other.id && sid == other.sid; }
};

// Only the textual form ConvertSidToStringSid produces: "S-1-" and dash-separated
// decimal fields. It is copied into JSON without escaping.
inline bool valid_sid(const std::wstring& sid) {
    if (sid.size() < 5 || sid.size() > 184 || sid.compare(0, 4, L"S-1-") != 0 || sid.back() == L'-') return false;
    for (size_t index = 2; index != sid.size(); ++index) {
        const wchar_t c = sid[index];
        if (c == L'-' ? sid[index - 1] == L'-' : (c < L'0' || c > L'9')) return false;
    }
    return true;
}

// A session can host processes of another account (runas). Keep every distinct
// pair; the service opens only the pair whose SID is the session's logon user.
inline void add(std::vector<Session>& sessions, unsigned long id, const std::wstring& sid) {
    const Session session{id, sid};
    if (!id || !valid_sid(sid)) return;
    for (const auto& known : sessions) if (known == session) return;
    sessions.push_back(session);
}

inline std::wstring encode(const std::vector<Session>& sessions) {
    std::wstring data;
    for (const auto& session : sessions) {
        if (!data.empty()) data += L';';
        data += std::to_wstring(session.id) + L':' + session.sid;
    }
    return data;
}

// CustomActionData written by encode(). Any malformed entry rejects the whole
// list rather than relaunching a partial or guessed set.
inline bool decode(const std::wstring& data, std::vector<Session>& sessions) {
    sessions.clear();
    if (data.empty()) return true;
    size_t offset = 0;
    for (;;) {
        const size_t end = data.find(L';', offset);
        const auto item = data.substr(offset, end == std::wstring::npos ? end : end - offset);
        const size_t colon = item.find(L':');
        if (colon == 0 || colon == std::wstring::npos || colon > 10) return false;
        unsigned long long id = 0;
        for (size_t index = 0; index != colon; ++index) {
            if (item[index] < L'0' || item[index] > L'9') return false;
            id = id * 10 + (item[index] - L'0');
        }
        const auto sid = item.substr(colon + 1);
        if (!id || id > 0xffffffffull || !valid_sid(sid)) return false;
        add(sessions, static_cast<unsigned long>(id), sid);
        if (end == std::wstring::npos) return true;
        offset = end + 1;
    }
}

// Service contract: crates/oink-winsvc/src/platform/update/relaunch.rs.
inline std::string json(const std::vector<Session>& sessions) {
    std::string text = "{\"schema\":1,\"sessions\":[";
    for (size_t index = 0; index != sessions.size(); ++index) {
        if (index) text += ',';
        text += "{\"id\":" + std::to_string(sessions[index].id) + ",\"sid\":\"";
        for (wchar_t c : sessions[index].sid) text += static_cast<char>(c);
        text += "\"}";
    }
    return text + "]}";
}
}
