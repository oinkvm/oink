#pragma once
// Bounded flat JSON for the versioned MSI admission record. No permissive JSON
// parser, duplicate fields, escapes, extension fields or caller-selected paths.
#include <cstdint>
#include <map>
#include <string>

namespace admission {
struct Value { std::string text; bool number = false; };
using Object = std::map<std::string, Value>;

inline bool parse(const std::string& input, Object& result) {
    result.clear();
    if (input.size() > 8192) return false;
    size_t at = 0;
    auto space = [&] { while (at < input.size() && (input[at] == ' ' || input[at] == '\n' || input[at] == '\r' || input[at] == '\t')) ++at; };
    auto quoted = [&](std::string& value) {
        if (at == input.size() || input[at++] != '"') return false;
        value.clear();
        while (at < input.size() && input[at] != '"') {
            auto c = static_cast<unsigned char>(input[at++]);
            if (c < 32 || c > 126 || c == '\\') return false;
            value.push_back(static_cast<char>(c));
        }
        return at < input.size() && input[at++] == '"';
    };
    space();
    if (at == input.size() || input[at++] != '{') return false;
    space();
    if (at < input.size() && input[at] == '}') return false;
    while (at < input.size()) {
        std::string key; Value value;
        if (!quoted(key)) return false;
        space(); if (at == input.size() || input[at++] != ':') return false; space();
        if (at < input.size() && input[at] == '"') {
            if (!quoted(value.text)) return false;
        } else {
            value.number = true;
            while (at < input.size() && input[at] >= '0' && input[at] <= '9') value.text.push_back(input[at++]);
            if (value.text.empty() || (value.text.size() > 1 && value.text[0] == '0')) return false;
        }
        if (!result.emplace(key, value).second || result.size() > 16) return false;
        space();
        if (at == input.size()) return false;
        if (input[at++] == '}') { space(); return at == input.size(); }
        if (input[at - 1] != ',') return false;
        space();
    }
    return false;
}

inline std::string get(const Object& object, const std::string& key) {
    auto entry = object.find(key);
    return entry != object.end() && !entry->second.number ? entry->second.text : std::string();
}

inline bool unsigned_value(const Object& object, const std::string& key, uint64_t& result) {
    auto entry = object.find(key);
    if (entry == object.end() || !entry->second.number) return false;
    result = 0;
    for (char c : entry->second.text) {
        uint64_t digit = static_cast<uint64_t>(c - '0');
        if (result > (UINT64_MAX - digit) / 10) return false;
        result = result * 10 + digit;
    }
    return true;
}

inline bool hex(const std::string& text, size_t length) {
    if (text.size() != length) return false;
    for (char c : text) if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) return false;
    return true;
}

inline bool hex64(const std::string& text, uint64_t& result) {
    if (!hex(text, 16)) return false;
    result = 0;
    for (char c : text) result = result * 16 + static_cast<uint64_t>(c <= '9' ? c - '0' : c - 'a' + 10);
    return true;
}

inline std::string guid(std::string text) {
    if (text.size() == 38 && text.front() == '{' && text.back() == '}') text = text.substr(1, 36);
    if (text.size() != 36) return {};
    for (size_t index = 0; index < text.size(); ++index) {
        char& c = text[index];
        if (index == 8 || index == 13 || index == 18 || index == 23) { if (c != '-') return {}; }
        else {
            if (c >= 'A' && c <= 'F') c = static_cast<char>(c + ('a' - 'A'));
            if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) return {};
        }
    }
    return text;
}

inline bool worker_shape(const Object& object) {
    if (object.size() != 12) return false;
    for (const auto key : {"schema", "transaction", "worker_pid", "worker_created_filetime", "worker_sha256",
         "lock_handle", "operation", "product_code", "package_sha256", "remove_product_code",
         "remove_package_sha256", "pending_sha256"}) if (!object.count(key)) return false;
    uint64_t schema = 0, pid = 0, handle = 0, created = 0;
    if (!unsigned_value(object, "schema", schema) || schema != 1 ||
        !unsigned_value(object, "worker_pid", pid) || pid == 0 || pid > UINT32_MAX ||
        !hex64(get(object, "worker_created_filetime"), created) || created == 0 ||
        !hex64(get(object, "lock_handle"), handle) || handle == 0 ||
        !hex(get(object, "transaction"), 32) || !hex(get(object, "worker_sha256"), 64) ||
        !hex(get(object, "pending_sha256"), 64) || !hex(get(object, "package_sha256"), 64) ||
        guid(get(object, "product_code")).empty()) return false;
    auto operation = get(object, "operation");
    if (operation != "install" && operation != "rollback") return false;
    auto remove = get(object, "remove_product_code"), hash = get(object, "remove_package_sha256");
    return (remove.empty() && hash.empty()) || (!guid(remove).empty() && hex(hash, 64));
}
}
