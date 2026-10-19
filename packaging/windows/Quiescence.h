#pragma once
#include <cstdint>

namespace quiescence {
enum class Phase { before_stop, after_stop };

struct Process {
    uint32_t pid = 0;
    uint32_t parent = 0;
    uint32_t session = 0;
    uint64_t created = 0;
    bool service_image = false; // Exact protected install path, not a basename.
    bool system = false;
};

// service is authenticated against SCM's live OinkInput PID, with its process
// handle held for the scan. Creation ordering rejects a reused parent PID.
inline bool service_owned(const Process& process, const Process& service, Phase phase) {
    if (phase != Phase::before_stop || !service.pid || !service.created || service.session != 0 ||
        !service.service_image || !service.system || !process.pid || !process.created ||
        !process.service_image || !process.system) return false;
    if (process.pid == service.pid)
        return process.session == 0 && process.created == service.created;
    // The broker creates its helper with its own SYSTEM token in a user session
    // and joins that child before reporting SERVICE_STOPPED. User clients never
    // qualify, even when named oink-service.exe or launched by the broker.
    return process.session != 0 && process.parent == service.pid && process.created >= service.created;
}
}
