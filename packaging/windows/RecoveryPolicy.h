#pragma once
#include "Authorization.h"
namespace recovery {
inline bool terminal_journal(const admission::Object& journal) {
    uint64_t schema = 0, old_build = 0, new_build = 0;
    for (const auto key : {"schema", "transaction", "oldBuild", "newBuild", "packageSha256", "stage", "rollbackAttempted", "failure"})
        if (!journal.count(key)) return false;
    return journal.size() == 8 && admission::unsigned_value(journal, "schema", schema) && schema == 1 &&
        admission::unsigned_value(journal, "oldBuild", old_build) && old_build &&
        admission::unsigned_value(journal, "newBuild", new_build) && new_build >= old_build &&
        admission::hex(admission::get(journal, "transaction"), 32) && admission::hex(admission::get(journal, "packageSha256"), 64) &&
        journal.at("rollbackAttempted").literal &&
        (admission::get(journal, "rollbackAttempted") == "true" || admission::get(journal, "rollbackAttempted") == "false") &&
        !journal.at("failure").number && (!journal.at("failure").literal || admission::get(journal, "failure") == "null") &&
        (admission::get(journal, "stage") == "committed" || admission::get(journal, "stage") == "rolled_back");
}
enum class Disposition { live, discard, refuse };
constexpr Disposition disposition(bool lock_held, bool untrusted, bool uninstall, bool worker_alive, bool terminal) {
    if (lock_held) return Disposition::live;
    return untrusted || uninstall || !worker_alive || terminal ? Disposition::discard : Disposition::refuse;
}
}
