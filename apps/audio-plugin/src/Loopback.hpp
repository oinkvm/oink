// SPDX-License-Identifier: MIT
#pragma once
#include <array>
#include <atomic>
#include <cstdint>
#include <cstring>
#include <limits>

// One HAL mixed writer, arbitrary simultaneous recording clients. Absolute sample
// positions make reads non-destructive; each client receives the same audio.
// No allocation, locks, waiting or logging on either realtime path.
class Loopback {
public:
    static constexpr uint64_t Capacity = 16384;
    static constexpr uint64_t Delay = 1024; // 21.33 ms at 48 kHz, explicit HAL latency.
    void write(uint64_t frame, float value) {
        auto& slot = slots_[frame % Capacity];
        slot.frame.store(Invalid);
        uint32_t bits;
        std::memcpy(&bits, &value, sizeof(bits));
        slot.bits.store(bits);
        slot.frame.store(frame);
    }
    float read(uint64_t frame) const {
        const auto& slot = slots_[frame % Capacity];
        if (slot.frame.load() != frame) return 0;
        const auto bits = slot.bits.load();
        if (slot.frame.load() != frame) return 0;
        float value;
        std::memcpy(&value, &bits, sizeof(value));
        return value;
    }
    // Called at output start/stop, while that device's IO is stopped.
    void clear() {
        for (auto& slot : slots_) slot.frame.store(Invalid);
    }
private:
    static constexpr uint64_t Invalid = std::numeric_limits<uint64_t>::max();
    struct Slot {
        std::atomic<uint64_t> frame{Invalid};
        std::atomic<uint32_t> bits{0};
    };
    static_assert(std::atomic<uint64_t>::is_always_lock_free);
    static_assert(std::atomic<uint32_t>::is_always_lock_free);
    std::array<Slot, Capacity> slots_{};
};
